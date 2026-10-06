//! Input events: what a producer puts on its queue, and how the audio thread merges them.

use std::sync::atomic::{AtomicU32, Ordering};

/// Hard maximum number of simultaneous input producers.
///
/// Declared up front and never exceeded, because the merged buffer is sized once at activation
/// for this maximum and never resized — connecting a device at runtime must not reallocate
/// callback-owned storage. Ports beyond this are refused with a visible reason.
///
/// One slot is the GUI (on-screen keyboard, computer keyboard, parameter panel); the rest are
/// MIDI input connections.
pub const MAX_INPUT_PRODUCERS: usize = 9;

/// Slot 0 is always the GUI.
pub const GUI_SOURCE: SourceId = SourceId(0);

/// How many events one producer may have outstanding.
pub const PRODUCER_QUEUE_CAPACITY: usize = 1024;

/// Reserved room in the merged buffer for the events that must never be refused: one choke per
/// outstanding GUI press, plus the gesture `end` events that close host-tracked gestures.
pub const EMERGENCY_RESERVE: usize = super::press::MAX_GUI_PRESSES + MAX_OPEN_GESTURES;

/// How many parameter gestures may be open at once. One per parameter control being dragged;
/// the reserve exists so a gesture `end` can always be delivered.
pub const MAX_OPEN_GESTURES: usize = 64;

/// The merged buffer's capacity: the sum of all producer capacities plus the emergency reserve.
///
/// Stated as a function so the verification suite can assert the relationship rather than a
/// magic number.
pub const fn merged_capacity() -> usize {
    MAX_INPUT_PRODUCERS * PRODUCER_QUEUE_CAPACITY + EMERGENCY_RESERVE
}

/// Which producer an event came from.
///
/// Sustain state and press accounting are kept per source, so two keyboards playing the same
/// pitch cannot release one another's notes.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct SourceId(pub u8);

impl SourceId {
    pub fn is_gui(self) -> bool {
        self == GUI_SOURCE
    }

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A monotonically increasing panic generation.
///
/// Raising it means "everything queued before now is suspect": on panic the audio thread
/// discards pre-epoch note events *before* issuing recovery, so a note-on still in flight cannot
/// land after the recovery that was meant to clear it.
#[derive(Debug, Default)]
pub struct PanicEpoch {
    current: AtomicU32,
}

impl PanicEpoch {
    pub fn new() -> Self {
        Self::default()
    }

    /// The epoch a producer stamps onto the events it is enqueueing right now.
    pub fn current(&self) -> u32 {
        self.current.load(Ordering::Acquire)
    }

    /// Raises the epoch, and returns the new value.
    pub fn raise(&self) -> u32 {
        self.current.fetch_add(1, Ordering::AcqRel) + 1
    }
}

/// What an event actually is.
///
/// Deliberately `Copy` and free of heap data: these travel through lock-free queues that must
/// never allocate.
/// Voice IDs are **not** carried here: they are assigned by the audio thread, which is the one
/// place that owns the press table, so a release can always be resolved to the exact press it
/// belongs to. Producers say what happened; the audio thread says which voice it was.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Payload {
    /// CLAP velocity is normalised, not a MIDI integer.
    NoteOn {
        channel: u8,
        key: u8,
        velocity: f64,
    },
    NoteOff {
        channel: u8,
        key: u8,
        velocity: f64,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    /// Normalised `0.0..=1.0`, with 0.5 centred, matching CLAP.
    PitchBend {
        channel: u8,
        value: f64,
    },
    ParamValue {
        param_id: u32,
        value: f64,
    },
    /// A modulation offset laid over a parameter, leaving its own value alone.
    ///
    /// What the player sends while a step is selected: you hear the step, and the patch underneath
    /// it is untouched — so leaving the step needs no restore, and the instrument's own editor can
    /// see that this knob is being moved by something other than the knob.
    ParamMod {
        param_id: u32,
        value: f64,
    },
    GestureBegin {
        param_id: u32,
    },
    GestureEnd {
        param_id: u32,
    },
    /// Host-side sustain. Routed through the event stream rather than read as an atomic so that
    /// pedal-up is ordered against the notes around it, and so a source that is not currently
    /// sending anything still gets its deferred releases flushed.
    SustainPedal(bool),
    /// Release everything **this source** is holding, targeted per press.
    ///
    /// Raised by window focus loss, pointer-capture loss during a drag, and MIDI input
    /// disconnect. Deliberately not the global path: we know exactly which presses are ours.
    CleanupSource,
    /// We have *lost track* of what is held — a note-off was dropped. Global, blunt recovery in
    /// whichever dialect the plugin negotiated.
    GlobalPanic,
}

impl Payload {
    /// Whether losing this event would leave something stuck or a gesture open.
    ///
    /// These are the events that get the emergency treatment: if one cannot be enqueued, the
    /// producer raises a panic rather than dropping it silently.
    pub fn must_not_be_lost(&self) -> bool {
        matches!(
            self,
            Payload::NoteOff { .. }
                | Payload::CleanupSource
                | Payload::GlobalPanic
                | Payload::SustainPedal(_)
                | Payload::GestureEnd { .. }
        )
    }

    /// Whether this is a note event, and therefore subject to epoch filtering. Controller
    /// messages are not filtered: discarding a pre-panic pitch bend would help nobody.
    pub fn is_note_event(&self) -> bool {
        matches!(self, Payload::NoteOn { .. } | Payload::NoteOff { .. })
    }
}

/// One event on its way to the plugin.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TimedEvent {
    /// Host-monotonic arrival time in nanoseconds, stamped **inside the callback the event
    /// arrived in**, before any queueing. That gives one timeline across every source.
    pub arrival_nanos: u64,
    /// The panic epoch current when this event was enqueued.
    pub epoch: u32,
    pub source: SourceId,
    pub payload: Payload,
    /// The order this event was merged in, assigned by [`MergedInput::push`].
    ///
    /// It exists so the merge can be ordered with an **unstable** sort and still be stable in
    /// effect. `sort_by_key` is Rust's *stable* sort, which allocates a scratch buffer once the
    /// slice is more than a handful of elements long — an allocation on the audio thread, and one
    /// that only appears under a dense buffer of events.
    sequence: u32,
}

impl TimedEvent {
    pub fn new(arrival_nanos: u64, epoch: u32, source: SourceId, payload: Payload) -> Self {
        Self {
            arrival_nanos,
            epoch,
            source,
            payload,
            sequence: 0,
        }
    }
}

/// The audio thread's merged input buffer.
///
/// Fixed capacity, allocated once at activation and never resized. Overflow is defined here as
/// well as per producer queue: the reserve means a full merge cannot discard the events that must
/// never be lost.
pub struct MergedInput {
    events: Vec<TimedEvent>,
    capacity: usize,
    /// Events dropped because the buffer was full. Droppable kinds only — the others raise a
    /// panic instead.
    dropped: u64,
}

impl MergedInput {
    /// Allocates for [`merged_capacity`]. Called at activation, on the GUI thread.
    pub fn new() -> Self {
        let capacity = merged_capacity();
        Self {
            events: Vec::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    /// An empty stand-in that owns no storage.
    ///
    /// Used to swap the real buffer out of the worker for the duration of a borrow, so the
    /// conversion loop can read the merged events while mutating everything else — without the
    /// clone that would otherwise allocate on the audio thread.
    pub fn placeholder() -> Self {
        Self {
            events: Vec::new(),
            capacity: 0,
            dropped: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    pub fn as_slice(&self) -> &[TimedEvent] {
        &self.events
    }

    /// Pushes without allocating. Returns false if the buffer is full, in which case the caller
    /// must raise a panic rather than pretend the event was delivered.
    pub fn push(&mut self, mut event: TimedEvent) -> bool {
        if self.events.len() == self.capacity {
            self.dropped += 1;
            return false;
        }
        // Stamped here rather than by the producer: merge order is what the tie-break needs, and
        // only the merge knows it.
        event.sequence = self.events.len() as u32;
        self.events.push(event);
        true
    }

    /// Orders the merged events by arrival time.
    ///
    /// Events that arrived within the same nanosecond keep their merge order, which is what stops
    /// a note-off overtaking its own note-on. That ordering comes from the explicit `sequence`
    /// tie-break rather than from a stable sort: Rust's stable sort allocates scratch space for
    /// anything but a very short slice, and this runs on the audio thread. The unstable sort
    /// allocates nothing, and with a unique tie-break it produces exactly the same order.
    pub fn sort_by_arrival(&mut self) {
        self.events
            .sort_unstable_by_key(|e| (e.arrival_nanos, e.sequence));
    }

    /// Discards every **note** event from before `epoch`.
    ///
    /// This is the ordering half of the panic: `all_sound_off()` clears the voice stack as it
    /// stands at that moment, so a pre-panic note-on arriving afterwards would create a fresh,
    /// permanent voice — the stuck note the panic was raised to prevent. Post-epoch events are
    /// preserved so playing continues normally.
    pub fn discard_pre_epoch_notes(&mut self, epoch: u32, source: Option<SourceId>) -> usize {
        let before = self.events.len();
        self.events.retain(|e| {
            let in_scope = source.is_none_or(|s| e.source == s);
            !(in_scope && e.epoch < epoch && e.payload.is_note_event())
        });
        before - self.events.len()
    }
}

impl Default for MergedInput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(epoch: u32, source: u8, key: u8) -> TimedEvent {
        TimedEvent::new(
            u64::from(key),
            epoch,
            SourceId(source),
            Payload::NoteOn {
                channel: 0,
                key,
                velocity: 1.0,
            },
        )
    }

    #[test]
    fn the_merge_reserves_room_for_what_must_never_be_refused() {
        let merged = MergedInput::new();
        assert!(
            merged.capacity() >= MAX_INPUT_PRODUCERS * PRODUCER_QUEUE_CAPACITY + EMERGENCY_RESERVE,
            "the merged buffer must hold every producer's queue plus the emergency reserve"
        );
    }

    #[test]
    fn a_full_merge_reports_the_refusal_rather_than_dropping_silently() {
        let mut merged = MergedInput::new();
        for i in 0..merged.capacity() {
            assert!(merged.push(note_on(0, 0, (i % 128) as u8)));
        }
        assert!(!merged.push(note_on(0, 0, 60)));
        assert_eq!(merged.dropped(), 1);
    }

    #[test]
    fn a_panic_discards_stale_notes_but_keeps_what_came_after() {
        let mut merged = MergedInput::new();
        merged.push(note_on(0, 0, 60)); // pre-panic: must not reach the plugin
        merged.push(note_on(1, 0, 62)); // post-panic: playing continues
        merged.push(TimedEvent::new(
            5,
            0,
            SourceId(0),
            Payload::ControlChange {
                channel: 0,
                controller: 1,
                value: 64,
            },
        ));

        let discarded = merged.discard_pre_epoch_notes(1, None);
        assert_eq!(discarded, 1);
        assert_eq!(merged.len(), 2, "controllers are not epoch-filtered");
        assert!(
            merged
                .as_slice()
                .iter()
                .all(|e| !matches!(e.payload, Payload::NoteOn { key: 60, .. }))
        );
    }

    #[test]
    fn a_gui_panic_leaves_physical_midi_events_entirely_alone() {
        let mut merged = MergedInput::new();
        merged.push(note_on(0, 0, 60)); // GUI, pre-panic
        merged.push(note_on(0, 1, 60)); // physical MIDI, same pitch, pre-panic

        merged.discard_pre_epoch_notes(1, Some(GUI_SOURCE));

        assert_eq!(merged.len(), 1);
        assert_eq!(merged.as_slice()[0].source, SourceId(1));
    }

    #[test]
    fn arrival_order_is_stable_so_a_release_cannot_overtake_its_press() {
        let mut merged = MergedInput::new();
        merged.push(TimedEvent::new(
            10,
            0,
            SourceId(0),
            Payload::NoteOn {
                channel: 0,
                key: 60,
                velocity: 1.0,
            },
        ));
        merged.push(TimedEvent::new(
            10,
            0,
            SourceId(0),
            Payload::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0.0,
            },
        ));
        merged.sort_by_arrival();

        assert!(matches!(
            merged.as_slice()[0].payload,
            Payload::NoteOn { .. }
        ));
    }

    #[test]
    fn the_events_that_must_not_be_lost_are_exactly_the_ones_named() {
        assert!(
            Payload::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0.0
            }
            .must_not_be_lost()
        );
        assert!(Payload::GlobalPanic.must_not_be_lost());
        assert!(Payload::CleanupSource.must_not_be_lost());
        assert!(Payload::GestureEnd { param_id: 0 }.must_not_be_lost());

        assert!(
            !Payload::NoteOn {
                channel: 0,
                key: 60,
                velocity: 1.0
            }
            .must_not_be_lost(),
            "note-ons are droppable: losing one is a missed note, not a stuck one"
        );
        assert!(
            !Payload::ControlChange {
                channel: 0,
                controller: 1,
                value: 0
            }
            .must_not_be_lost()
        );
    }
}
