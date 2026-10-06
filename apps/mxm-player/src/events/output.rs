//! The plugin's output events: a bounded sink that fails rather than growing, and the routing
//! that decides where each kind goes.
//!
//! Clack's `EventBuffer::push()` can grow its backing `Vec`, so preallocating it does not make
//! the callback allocation-free if a plugin emits more events than expected. This sink has fixed
//! capacity and **fails instead of growing**. Overflow increments a visible counter: a host that
//! quietly loses plugin output is worse than one that says it did.

use clack_host::events::UnknownEvent;
use clack_host::events::event_types::{
    MidiEvent, MidiSysExEvent, NoteEndEvent, NoteOffEvent, NoteOnEvent, ParamGestureBeginEvent,
    ParamGestureEndEvent, ParamValueEvent,
};
use clack_host::events::io::{InputEventBuffer, OutputEventBuffer, TryPushError};

/// How many bytes of events one process call may produce. Sized generously: a plugin that
/// exceeds it is misbehaving, and the counter says so.
pub const OUTPUT_BUFFER_BYTES: usize = 64 * 1024;

/// How many events one process call may produce.
pub const OUTPUT_BUFFER_EVENTS: usize = 4096;

/// Where an output event has to go.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum OutputRoute {
    /// Parameter value and gesture events, so the generic panel tracks plugin-driven changes.
    Gui,
    /// Note and MIDI events that are representable in MIDI 1.
    MidiOut,
    /// Note expression, per-note pitch — anything not representable in MIDI 1. Counted as
    /// "unrepresented", not silently dropped.
    Unrepresentable,
    /// SysEx and other variable-sized events. Explicitly rejected in v1: a bounded sink cannot
    /// promise arbitrary sizes.
    Rejected,
}

/// Decides an event's destination from its type.
pub fn classify(event: &UnknownEvent) -> OutputRoute {
    if event.as_event::<ParamValueEvent>().is_some()
        || event.as_event::<ParamGestureBeginEvent>().is_some()
        || event.as_event::<ParamGestureEndEvent>().is_some()
    {
        return OutputRoute::Gui;
    }

    if event.as_event::<MidiSysExEvent>().is_some() {
        return OutputRoute::Rejected;
    }

    if event.as_event::<NoteOnEvent>().is_some()
        || event.as_event::<NoteOffEvent>().is_some()
        || event.as_event::<MidiEvent>().is_some()
    {
        return OutputRoute::MidiOut;
    }

    // `NoteEnd` is informational and has no MIDI 1 equivalent, but losing it costs nothing, so
    // it is routed to the GUI rather than counted as an unrepresented loss.
    if event.as_event::<NoteEndEvent>().is_some() {
        return OutputRoute::Gui;
    }

    // Note expression, param mod, MIDI 2, transport, and anything in a non-core space.
    if event.as_core_event().is_none() {
        return OutputRoute::Rejected;
    }
    OutputRoute::Unrepresentable
}

/// What the sink lost, and why. Shown in the UI.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct OutputCounters {
    /// Events that did not fit. The sink was full.
    pub overflowed: u64,
    /// Events with no MIDI 1 representation, so nothing could be sent onward.
    pub unrepresented: u64,
    /// SysEx and other variable-sized events, refused by policy.
    pub rejected: u64,
}

/// A fixed-capacity output-event sink.
///
/// Events are copied verbatim into a byte arena, so any CLAP event type round-trips, but the
/// arena never grows. Alignment is handled by storing `u64` words: `clap_event_header` needs
/// 4-byte alignment, and 8 is safely more.
pub struct FixedEventBuffer {
    storage: Vec<u64>,
    /// Word offset of each stored event, in push order.
    offsets: Vec<u32>,
    used_words: usize,
    counters: OutputCounters,
}

impl FixedEventBuffer {
    /// Allocates the arena. Called at activation, on the GUI thread.
    pub fn new() -> Self {
        Self {
            storage: vec![0; OUTPUT_BUFFER_BYTES / size_of::<u64>()],
            offsets: Vec::with_capacity(OUTPUT_BUFFER_EVENTS),
            used_words: 0,
            counters: OutputCounters::default(),
        }
    }

    /// An empty stand-in that owns no storage, for the same swap-out reason as
    /// [`MergedInput::placeholder`](crate::events::input::MergedInput::placeholder).
    pub fn placeholder() -> Self {
        Self {
            storage: Vec::new(),
            offsets: Vec::new(),
            used_words: 0,
            counters: OutputCounters::default(),
        }
    }

    /// Folds a placeholder's counters back into the real buffer after a swap.
    pub fn absorb_counters(&mut self, other: &FixedEventBuffer) {
        self.counters.overflowed += other.counters.overflowed;
        self.counters.unrepresented += other.counters.unrepresented;
        self.counters.rejected += other.counters.rejected;
    }

    pub fn clear(&mut self) {
        self.offsets.clear();
        self.used_words = 0;
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    pub fn counters(&self) -> OutputCounters {
        self.counters
    }

    /// Records an event that was pushed but cannot be sent onward.
    pub fn count_unrepresented(&mut self) {
        self.counters.unrepresented += 1;
    }

    /// Records an event refused by policy.
    pub fn count_rejected(&mut self) {
        self.counters.rejected += 1;
    }

    /// Pushes a typed event, reporting whether it fitted.
    ///
    /// Used on the input side, where a refused push is not a plugin misbehaving but the host
    /// running out of reserve — and therefore something the caller must react to rather than
    /// ignore.
    pub fn push_event<E: clack_host::events::Event>(&mut self, event: &E) -> bool {
        self.try_push(event.as_unknown()).is_ok()
    }

    /// Iterates the events pushed since the last [`clear`](Self::clear), in push order.
    pub fn iter(&self) -> impl Iterator<Item = &UnknownEvent> + '_ {
        self.offsets.iter().map(move |&offset| {
            let header = self.storage[offset as usize..]
                .as_ptr()
                .cast::<clack_host::events::EventHeader>()
                .cast();
            // SAFETY: the arena holds a verbatim copy of a valid event, written by `try_push`
            // from a `&UnknownEvent` the plugin gave us, at a `u64`-aligned offset, and its
            // header's `size` field describes exactly how many bytes were copied.
            unsafe { UnknownEvent::from_raw(header) }
        })
    }
}

impl Default for FixedEventBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// The same fixed arena also serves as the audio thread's **input** list, so converting merged
/// events into CLAP events allocates nothing either.
impl InputEventBuffer for FixedEventBuffer {
    fn len(&self) -> u32 {
        self.offsets.len() as u32
    }

    fn get(&self, index: u32) -> Option<&UnknownEvent> {
        let offset = *self.offsets.get(index as usize)?;
        let header = self.storage[offset as usize..]
            .as_ptr()
            .cast::<clack_host::events::EventHeader>()
            .cast();
        // SAFETY: as in `iter` — a verbatim copy of a valid event at a `u64`-aligned offset.
        Some(unsafe { UnknownEvent::from_raw(header) })
    }
}

impl OutputEventBuffer for FixedEventBuffer {
    fn try_push(&mut self, event: &UnknownEvent) -> Result<(), TryPushError> {
        // SAFETY: `as_raw` returns a pointer to the event's own header, valid for this call.
        let size = unsafe { (*event.as_raw()).size } as usize;
        // A zero-sized or absurd event header is the plugin misbehaving; refuse rather than
        // trusting it enough to copy.
        if size < size_of::<clack_host::events::EventHeader>() || size > OUTPUT_BUFFER_BYTES {
            self.counters.rejected += 1;
            return Err(TryPushError::new());
        }

        let words = size.div_ceil(size_of::<u64>());
        if self.offsets.len() == OUTPUT_BUFFER_EVENTS
            || self.used_words + words > self.storage.len()
        {
            self.counters.overflowed += 1;
            return Err(TryPushError::new());
        }

        let offset = self.used_words;
        let source = (event as *const UnknownEvent).cast::<u8>();
        let destination = self.storage[offset..].as_mut_ptr().cast::<u8>();
        // SAFETY: `size` bytes starting at `event` are the event's own storage, which CLAP
        // guarantees; the destination has at least `words * 8 >= size` bytes, checked above, and
        // the regions cannot overlap because the arena is ours alone.
        unsafe { std::ptr::copy_nonoverlapping(source, destination, size) };

        self.used_words += words;
        self.offsets.push(offset as u32);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clack_host::prelude::*;

    fn param(value: f64) -> ParamValueEvent {
        ParamValueEvent::new(
            0,
            ClapId::new(0),
            Pckn::match_all(),
            value,
            clack_host::utils::Cookie::empty(),
        )
    }

    #[test]
    fn events_round_trip_through_the_arena() {
        let mut buffer = FixedEventBuffer::new();
        buffer.try_push(param(0.25).as_unknown()).unwrap();
        buffer
            .try_push(NoteOnEvent::new(7, Pckn::new(0u16, 0u16, 60u16, 3u32), 1.0).as_unknown())
            .unwrap();

        let events: Vec<_> = buffer.iter().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].as_event::<ParamValueEvent>().unwrap().value(),
            0.25
        );
        let note = events[1].as_event::<NoteOnEvent>().unwrap();
        assert_eq!(note.header().time(), 7);
        assert_eq!(note.pckn().raw_key(), 60);
    }

    #[test]
    fn the_sink_fails_instead_of_growing_and_counts_what_it_lost() {
        let mut buffer = FixedEventBuffer::new();
        let mut pushed = 0u64;
        while buffer.try_push(param(0.5).as_unknown()).is_ok() {
            pushed += 1;
            assert!(pushed < 1_000_000, "the sink must be bounded");
        }
        assert_eq!(buffer.counters().overflowed, 1);
        assert!(pushed > 0);

        // ...and it recovers cleanly for the next process call.
        buffer.clear();
        assert!(buffer.try_push(param(0.5).as_unknown()).is_ok());
    }

    #[test]
    fn routing_matches_what_each_destination_can_actually_carry() {
        let pckn = Pckn::new(0u16, 0u16, 60u16, 1u32);

        assert_eq!(classify(param(0.5).as_unknown()), OutputRoute::Gui);
        assert_eq!(
            classify(ParamGestureBeginEvent::new(0, ClapId::new(0)).as_unknown()),
            OutputRoute::Gui
        );
        assert_eq!(
            classify(NoteOnEvent::new(0, pckn, 1.0).as_unknown()),
            OutputRoute::MidiOut
        );
        assert_eq!(
            classify(MidiEvent::new(0, 0, [0x90, 60, 100]).as_unknown()),
            OutputRoute::MidiOut
        );
        assert_eq!(
            classify(
                clack_host::events::event_types::NoteExpressionEvent::new(
                    0,
                    pckn,
                    clack_host::events::event_types::NoteExpressionType::Volume,
                    0.5,
                )
                .as_unknown()
            ),
            OutputRoute::Unrepresentable
        );
    }
}
