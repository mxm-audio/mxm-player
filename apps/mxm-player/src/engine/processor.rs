//! The audio thread's work: merge, convert, chunk, process, measure.
//!
//! Nothing here allocates, locks, or does I/O. Every buffer is sized at activation, every queue
//! is lock-free, and `midir::send()` — which *is* I/O — happens on a worker thread reached
//! through a queue, never from this callback.

use crate::engine::fx::FxChain;
use crate::engine::meters::Meters;
use crate::engine::stream::ProcessorOwner;
use crate::envelope::{Dialect, Envelope, GlobalRecovery};
use crate::events::input::{GUI_SOURCE, MergedInput, Payload, SourceId, TimedEvent};
use crate::events::output::{FixedEventBuffer, OutputRoute, classify};
use crate::events::press::{Press, PressTable, ReleaseOutcome};
use clack_extensions::tail::{PluginTail, TailLength};
use clack_host::events::EventFlags;
use clack_host::events::event_types::TransportFlags;
use clack_host::events::event_types::{
    MidiEvent, NoteChokeEvent, NoteOffEvent, NoteOnEvent, ParamGestureBeginEvent,
    ParamGestureEndEvent, ParamModEvent, ParamValueEvent, TransportEvent,
};
use clack_host::prelude::*;
use clack_host::utils::Cookie;
use clack_host::utils::{BeatTime, SecondsTime};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Everything the worker's scratch can hold in one chunk.
///
/// The runtime's own maximum — which already covers its owed zeroes and its preview, since it emits
/// both itself — plus the one release the worker prepends when a transport change was handled where
/// nothing could be rendered.
pub const MAX_CHUNK_ACTIONS: usize = crate::sequencer::runtime::MAX_ACTIONS_PER_CHUNK + 1;

const _: () = assert!(
    MAX_CHUNK_ACTIONS > crate::sequencer::runtime::MAX_ACTIONS_PER_CHUNK,
    "the worker's scratch must cover everything the runtime produces, plus its own owed release"
);

/// The largest number of frames the player will ever ask a plugin to process in one go.
///
/// `BufferSize::Fixed` is only a *request*: CPAL may deliver any frame count, so an oversized
/// callback is split into chunks of at most this many frames, with no allocation.
pub const MAX_BLOCK_FRAMES: u32 = 4096;

/// How many consecutive exactly-silent buffers are required before the player will sleep a
/// plugin that returned `ContinueIfNotQuiet`.
///
/// Deliberately conservative, and **exact zero**: a nonzero threshold would truncate quiet
/// release tails, and mxm-mono-01's `flush` already guarantees exact zeros when it is genuinely
/// silent. Plugins that emit dither or noise will correctly never sleep.
pub const QUIET_BUFFERS_BEFORE_SLEEP: u32 = 8;

/// What the engine is doing with the plugin right now.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum RunState {
    /// Calling `process()` every callback.
    Running,
    /// The plugin asked to sleep, or went quiet. Output is silence until something wakes it.
    Sleeping,
    /// `process()` returned an error. The plugin is never called again in a tight loop.
    Failed,
    /// A `Stop` command was honoured: the processor has been handed back and this callback only
    /// outputs silence from now on.
    Stopped,
}

/// Commands from the GUI. The command queue's only producer is the GUI thread.
/// How hard the sequencer strikes a note.
///
/// Fixed, and not a control: per-step velocity is composition, and this exists to let you hear a
/// synth. A little under full so a velocity-sensitive patch is not pinned at its ceiling.
const SEQUENCER_VELOCITY: f64 = 100.0 / 127.0;

// Not `Eq`: the sequencer state carries a tempo, and `f64` has no total equality. Nothing
// compares commands for equality anyway.
//
// `large_enum_variant` is allowed deliberately. Clippy's remedy is to box the big variant, and a
// `Box` here would mean an allocation on the GUI thread and — far worse — a **deallocation on the
// audio thread** when the worker drops the old one. Carrying the 288-byte sequencer state inline
// costs the ring buffer 288 bytes per slot, which is 18 KB once, allocated at activation. That is
// the trade this codebase makes everywhere: fixed memory up front over any heap traffic in a
// realtime callback.
#[derive(Clone, PartialEq, Debug)]
pub enum Command {
    /// Stop processing and hand the processor back through the return queue.
    Stop,
    /// Reset the plugin's processing state.
    Reset,
    /// Which CCs the player has claimed, so they reach the GUI instead of the plugin.
    ///
    /// A 16-byte bitset by value: no allocation, no `Arc` to drop on the audio thread, and
    /// answering "is this claimed" is two instructions. Everything else about mapping lives on
    /// the GUI thread — see [`crate::control_map`].
    SetClaimedCcs(crate::control_map::CcMask),
    /// The sequencer's whole state, **by pointer**.
    ///
    /// Transport is part of the state rather than a stream of events, which is what stops Play,
    /// Pause and Stop being dropped or reordered. See [`crate::sequencer::SequencerState`].
    ///
    /// **An `Arc`, so the ring is not sized by the payload.** By value this variant made every one
    /// of the ring's 64 slots as large as a whole sequencer state, and the state cannot grow past a
    /// compile-time constant while that is true — which is the same as saying a sequence has a
    /// maximum length. Eight bytes a slot instead, and the payload is heap the **writer** allocated.
    ///
    /// **The audio thread never drops the last reference.** It reads what it needs and hands the
    /// `Arc` straight back through `retired`, to be released on the thread that made it. See
    /// [`AudioWorker::retire_sequencer`].
    SetSequencer(std::sync::Arc<crate::sequencer::SequencerState>),
}

/// The audio-thread half of the engine.
pub struct AudioWorker {
    /// The effects after the source, in order. **Declared before `owner`, and that is
    /// load-bearing**: fields drop in declaration order, so when a dead stream drops this worker
    /// every effect's processor is handed back before the source's — and the engine, which waits
    /// on the source's flag, finds the effects' already in their queues.
    fx: FxChain,
    owner: ProcessorOwner,
    commands: rtrb::Consumer<Command>,
    /// Where a consumed sequencer state goes to be freed, off this thread.
    retired: rtrb::Producer<std::sync::Arc<crate::sequencer::SequencerState>>,
    /// One consumer per producer slot. Slot 0 is the GUI; the rest are MIDI inputs.
    inputs: Vec<rtrb::Consumer<TimedEvent>>,
    /// Events bound for the MIDI-out worker, tagged with the out-panic epoch.
    midi_out: Option<rtrb::Producer<crate::midi::OutgoingEvent>>,
    /// Events bound for the GUI: plugin-driven parameter changes and gestures.
    to_gui: rtrb::Producer<crate::engine::PluginOutput>,

    merged: MergedInput,
    presses: PressTable,
    /// Preallocated working room for press lists, so answering "what is outstanding" never
    /// allocates on the audio thread.
    scratch: Vec<Press>,
    /// Locks that would not fit in the input arena. Counted rather than recovered from: see
    /// `emit_sequencer_action`.
    dropped_locks: u64,
    /// Presses captured for a legato joint, before the note-ons that would shadow them.
    ///
    /// Preallocated because it is filled on the audio thread. See `emit_legato_joint`.
    legato: Vec<Press>,
    input_events: FixedEventBuffer,
    output_events: FixedEventBuffer,

    /// One `Vec<f32>` per channel, `MAX_BLOCK_FRAMES` long, allocated at activation.
    channels: Vec<Vec<f32>>,
    ports: AudioPorts,
    silent_input: InputAudioBuffers<'static>,

    envelope: Envelope,
    tail: Option<PluginTail>,
    cached_tail: Option<TailLength>,
    tail_frames_remaining: Option<u64>,

    /// CCs the player claims. Empty until the GUI says otherwise, so nothing changes for a
    /// plugin loaded before a map is resolved.
    claimed_ccs: crate::control_map::CcMask,

    /// The audio-thread half of the sequencer.
    sequencer: crate::sequencer::Runtime,
    /// Scratch for the actions of one chunk, swapped out rather than cloned so nothing allocates.
    seq_actions: Vec<crate::sequencer::Action>,
    /// Notes the sequencer owes a release that no `process()` call has yet delivered.
    ///
    /// A Pause serviced by `service_commands` renders nothing, so its note-offs cannot reach the
    /// plugin there. Nothing may treat a release as delivered that no plugin has received.
    seq_owed_release: crate::sequencer::Step,

    state: RunState,
    quiet_buffers: u32,
    /// When the previous callback began, on the host-monotonic clock.
    ///
    /// Events are stamped with a shared arrival time inside the callback they arrived in; this
    /// is the other half of the bridge, mapping that common timeline onto sample offsets. It is
    /// deliberately the *previous* callback's start: an event that arrived during the last
    /// buffer period is placed proportionally inside the buffer being rendered now, which
    /// preserves the interval between two events even though both are one buffer late.
    previous_callback_nanos: Option<u64>,
    /// The current callback's start, so the mapping is computed once per callback.
    callback_nanos: u64,
    /// The nanoseconds one callback period covers, from the last two callback starts.
    callback_period_nanos: u64,
    steady_time: u64,
    sample_rate: f64,

    meters: Arc<Meters>,
    shared: Arc<crate::host::PlayerHostState>,
    /// Whether MIDI thru is on, so keyboard events are echoed outward after sustain handling.
    thru_enabled: Arc<AtomicBool>,
    /// Raised when a release could not be queued to the MIDI-out worker.
    out_panic: Arc<crate::midi::OutPanic>,
    /// The panic epoch stamped onto input events.
    input_epoch: Arc<crate::events::input::PanicEpoch>,
    /// The shared timeline. Read once per callback; see `clock.rs` for the realtime contract.
    clock: Arc<crate::clock::Clock>,
    /// How many output events the sink had refused when the GUI was last told.
    last_output_overflow: u64,
    /// The epoch this callback last acted on.
    ///
    /// A producer whose queue is full cannot enqueue anything — not even a panic — so it raises
    /// the epoch atomic instead. Watching that atomic here is what turns "the note-off was
    /// dropped" into recovery rather than a stuck note.
    last_seen_epoch: u32,
}

/// Everything the worker needs at construction, assembled on the GUI thread at activation.
pub struct WorkerConfig {
    pub owner: ProcessorOwner,
    /// The effect chain, already activated. Empty when there is none.
    pub fx: FxChain,
    pub commands: rtrb::Consumer<Command>,
    /// Where a consumed sequencer state goes to be freed, off this thread.
    pub retired: rtrb::Producer<std::sync::Arc<crate::sequencer::SequencerState>>,
    pub inputs: Vec<rtrb::Consumer<TimedEvent>>,
    pub midi_out: Option<rtrb::Producer<crate::midi::OutgoingEvent>>,
    pub to_gui: rtrb::Producer<crate::engine::PluginOutput>,
    pub envelope: Envelope,
    pub tail: Option<PluginTail>,
    pub sample_rate: f64,
    pub meters: Arc<Meters>,
    pub shared: Arc<crate::host::PlayerHostState>,
    pub thru_enabled: Arc<AtomicBool>,
    pub out_panic: Arc<crate::midi::OutPanic>,
    pub input_epoch: Arc<crate::events::input::PanicEpoch>,
    pub clock: Arc<crate::clock::Clock>,
}

impl AudioWorker {
    /// Allocates everything the callback will ever need. Called on the GUI thread.
    /// Hands a consumed sequencer state back to be freed off the audio thread.
    ///
    /// **The whole reason the state travels by pointer.** An `Arc` dropped here could take the last
    /// reference and call the allocator inside the callback, which is the one thing this engine does
    /// not do. Pushing it to the GUI moves that cost to a thread allowed to pay it.
    ///
    /// A failed push cannot happen — the queue is as deep as the command queue and the GUI drains it
    /// before pushing — but if it ever did, **keeping the state is the safe answer and dropping it
    /// is not**, so the error is deliberately ignored rather than unwrapped. `rtrb` returns the
    /// value inside the error, which is then dropped... on this thread. That residual risk is why
    /// the depth argument above has to hold, and `retiring_never_overflows` is what says it does.
    fn retire_sequencer(&mut self, state: std::sync::Arc<crate::sequencer::SequencerState>) {
        let _ = self.retired.push(state);
    }

    pub fn new(config: WorkerConfig) -> Self {
        let channel_count = config.envelope.audio.channel_count as usize;

        Self {
            fx: config.fx,
            owner: config.owner,
            commands: config.commands,
            retired: config.retired,
            inputs: config.inputs,
            midi_out: config.midi_out,
            to_gui: config.to_gui,

            merged: MergedInput::new(),
            presses: PressTable::new(),
            scratch: Vec::with_capacity(crate::events::press::MAX_TRACKED_PRESSES),
            dropped_locks: 0,
            legato: Vec::with_capacity(crate::events::press::MAX_TRACKED_PRESSES),
            input_events: FixedEventBuffer::new(),
            output_events: FixedEventBuffer::new(),

            channels: vec![vec![0.0; MAX_BLOCK_FRAMES as usize]; channel_count],
            ports: AudioPorts::with_capacity(channel_count, 1),
            silent_input: InputAudioBuffers::empty(),

            envelope: config.envelope,
            tail: config.tail,
            cached_tail: None,
            tail_frames_remaining: None,

            claimed_ccs: crate::control_map::CcMask::default(),

            sequencer: crate::sequencer::Runtime::new(config.sample_rate),
            seq_actions: Vec::with_capacity(MAX_CHUNK_ACTIONS),
            seq_owed_release: crate::sequencer::Step::EMPTY,

            state: RunState::Running,
            quiet_buffers: 0,
            previous_callback_nanos: None,
            callback_nanos: 0,
            callback_period_nanos: 0,
            steady_time: 0,
            sample_rate: config.sample_rate,

            meters: config.meters,
            shared: config.shared,
            thru_enabled: config.thru_enabled,
            out_panic: config.out_panic,
            last_output_overflow: 0,
            last_seen_epoch: config.input_epoch.current(),
            input_epoch: config.input_epoch,
            clock: config.clock,
        }
    }

    pub fn state(&self) -> RunState {
        self.state
    }

    /// The whole callback. `output` is interleaved, `channel_count` channels.
    pub fn callback(&mut self, output: &mut [f32], channel_count: usize) {
        let started = Instant::now();
        let frames = (output.len() / channel_count.max(1)) as u64;

        // The host-monotonic bridge, taken once per callback.
        let now = self.clock.now_nanos();
        self.callback_period_nanos = self
            .previous_callback_nanos
            .map(|previous| now.saturating_sub(previous))
            .unwrap_or(0);
        self.previous_callback_nanos = Some(self.callback_nanos);
        self.callback_nanos = now;

        self.handle_commands();

        let mut plugin_time = std::time::Duration::ZERO;
        let mut fx_time = std::time::Duration::ZERO;
        if self.state == RunState::Stopped || self.state == RunState::Failed {
            output.fill(0.0);
        } else {
            self.drain_inputs();
            (plugin_time, fx_time) = self.render(output, channel_count);
        }

        let budget = std::time::Duration::from_secs_f64(frames as f64 / self.sample_rate);
        self.meters
            .record_callback(plugin_time, fx_time, started.elapsed(), budget, frames);
    }

    /// Maps an event's arrival time onto a sample offset inside the buffer being rendered.
    ///
    /// Everything that arrived before the previous callback began is already late and lands on
    /// frame 0. Everything else is placed proportionally, which is what keeps the interval
    /// between two events intact.
    fn offset_for(&self, arrival_nanos: u64, frames: u32) -> u32 {
        let Some(previous) = self.previous_callback_nanos else {
            return 0;
        };
        if self.callback_period_nanos == 0 || frames == 0 || arrival_nanos <= previous {
            return 0;
        }

        let elapsed = arrival_nanos - previous;
        let fraction = (elapsed as f64 / self.callback_period_nanos as f64).clamp(0.0, 1.0);
        ((fraction * f64::from(frames)) as u32).min(frames - 1)
    }

    /// Handles pending commands **without rendering anything**.
    ///
    /// This exists for the deterministic session backend, where audio only advances when the
    /// script asks for it. `Engine::stop_now` polls synchronously until a *callback* consumes
    /// `Command::Stop`, and `load` and `rescan` both call it — so without a way to service
    /// commands independently of rendering, a stepped backend deadlocks and then reports the
    /// plugin as wedged.
    ///
    /// It deliberately does **not** drain the input queues: `render` is what converts merged
    /// events, so draining here without rendering would silently discard them.
    pub fn service_commands(&mut self) {
        self.handle_commands();
    }

    fn handle_commands(&mut self) {
        while let Ok(command) = self.commands.pop() {
            match command {
                Command::Stop => {
                    // Every processor is handed back — the effects first, so that when the engine
                    // sees the source's flag theirs are already queued — and this callback outputs
                    // silence from now on.
                    if self.fx.hand_back() && self.owner.hand_back() {
                        self.state = RunState::Stopped;
                    }
                }
                Command::SetClaimedCcs(mask) => self.claimed_ccs = mask,
                Command::SetSequencer(state) => {
                    // Read before retiring: once the state is handed back it is gone from here, and
                    // retiring last is what keeps the audio thread from ever holding the last
                    // reference for longer than one command.
                    let serial = state.serial;
                    let now_playing = state.transport.is_playing();
                    // Whatever a transport change orphans is *owed*, not released: this may be
                    // running under `service_commands`, which renders nothing.
                    let was_playing = self.sequencer.transport().is_playing();
                    // **The runtime keeps the new state and hands back the old one.** It reads
                    // through the pointer rather than copying, which is what lets a pattern be any
                    // length — a copy of a heap-backed pattern would allocate right here.
                    let (owed, previous) = self.sequencer.apply(state);
                    owed.for_each(|note| self.seq_owed_release = self.seq_owed_release.with(note));

                    // **What it stopped modulating is owed a zero.** The runtime decides this, not
                    // the host: only it can order a zero against the steps it is still emitting, and
                    // a host sending one would have to publish the state that stops the stepping
                    // first — two different queues, so an old step could overtake the zero and leave
                    // the parameter stuck.
                    self.shared.acknowledge_sequencer(serial);
                    // **Handed back, never dropped here.** Releasing the last reference would free
                    // on the audio thread. The queue is as deep as the command queue and the GUI
                    // drains it before pushing, so a failure is not reachable — and if it somehow
                    // were, holding the state costs a snapshot's memory until the next one and
                    // freeing it here would cost a deallocation inside the callback.
                    self.retire_sequencer(previous);

                    // **Coming to rest is when somebody stops to look at what they made**, and the
                    // last lock before the stop has no next step behind it to republish what a
                    // dropped notification lost. So the resting state is always settled, whatever
                    // happened during the run: one flag, one requery, and the panel shows what the
                    // instrument actually has.
                    if was_playing && !now_playing {
                        self.shared.note_unsettled_params();
                    }
                }
                Command::Reset => {
                    if let Some(processor) = self.owner.processor_mut() {
                        processor.reset();
                    }
                    // The effects too: a reset that left a reverb ringing would not be one.
                    self.fx.reset_all();
                    self.presses.clear();
                    self.state = RunState::Running;
                    self.quiet_buffers = 0;
                }
            }
        }
    }

    /// Drains every producer queue and merges by arrival time.
    fn drain_inputs(&mut self) {
        self.merged.clear();

        let mut refused = false;
        for (slot, queue) in self.inputs.iter_mut().enumerate() {
            let _ = slot;
            while let Ok(event) = queue.pop() {
                if !self.merged.push(event) {
                    // A full merge is itself a lost-track condition for anything that must not
                    // be lost. Droppable kinds are counted by `MergedInput`.
                    if event.payload.must_not_be_lost() {
                        refused = true;
                    }
                }
            }
        }

        if refused {
            self.input_epoch.raise();
        }

        // Honour a panic raised by *any* producer, including one whose queue was too full to
        // enqueue the panic itself. The pre-epoch note events are discarded before recovery is
        // issued, so a note-on still in flight cannot land after the cleanup meant to clear it.
        let epoch = self.input_epoch.current();
        if epoch != self.last_seen_epoch {
            self.last_seen_epoch = epoch;
            self.merged.discard_pre_epoch_notes(epoch, None);
            self.merged
                .push(TimedEvent::new(0, epoch, GUI_SOURCE, Payload::GlobalPanic));
        }

        self.merged.sort_by_arrival();
    }

    /// Converts merged events into CLAP events, applying host-side sustain on the way.
    ///
    /// Returns false if anything could not be converted, which is treated as a lost-track
    /// condition rather than a silent loss.
    fn convert_events(&mut self, frames: u32, include_queued: bool) -> bool {
        self.input_events.clear();
        // **And every effect's, in the same breath.** An effect's automation is pushed into its own
        // list from the same actions as the source's; a list that was filled and never emptied
        // would replay the last chunk's offsets for ever and then refuse every new one once full.
        self.fx.clear_incoming();
        let mut complete = true;

        // Sequencer actions and queued events go into **one stream ordered by sample offset**.
        // `FixedEventBuffer` exposes events in push order, so appending the sequencer's afterwards
        // would give the plugin a non-monotonic stream whenever a keypress or a panic fell between
        // two sequencer boundaries in the same chunk.
        //
        // Swapped out rather than cloned, so nothing allocates.
        let mut actions = std::mem::take(&mut self.seq_actions);
        let mut next_action = 0usize;

        // The merged list is only read here, and nothing in the loop touches it - so it is
        // swapped out for an empty stand-in rather than cloned, which would allocate.
        let merged = std::mem::replace(&mut self.merged, MergedInput::placeholder());

        for (index, &event) in merged.as_slice().iter().enumerate() {
            if !include_queued {
                break;
            }
            let time = self.offset_for(event.arrival_nanos, frames);

            // A legato joint is two actions at one frame, so `emit_from` reports how many it
            // consumed rather than always one. The second is at the same offset as the first, so
            // taking both here never emits anything ahead of its time.
            while next_action < actions.len() && actions[next_action].frame() <= time {
                next_action += self.emit_from(&actions, next_action, &mut complete);
            }

            // Coalescing, P4 rules: only a *genuinely stale* same-offset event is dropped, and
            // never across a gesture boundary. In P2, where everything landed on the buffer
            // boundary, keeping only the newest value per controller was valid; once events
            // carry distinct offsets the intermediate values become audible.
            if superseded_at_same_offset(&merged, index, time, frames, self) {
                continue;
            }

            match event.payload {
                Payload::NoteOn {
                    channel,
                    key,
                    velocity,
                } => {
                    let Some(press) = self.presses.press(event.source, channel, key) else {
                        // An untracked press is a note we could never release. Refuse to send it.
                        complete = false;
                        continue;
                    };
                    if !self.emit_note_on(press, velocity, time) {
                        self.presses.retire(press.voice_id);
                        complete = false;
                    } else {
                        self.echo_thru(press, Some(velocity), true);
                    }
                }

                Payload::NoteOff {
                    channel,
                    key,
                    velocity,
                } => match self.presses.release(event.source, channel, key) {
                    ReleaseOutcome::Release(press) => {
                        if self.emit_note_off(press, velocity, time) {
                            self.presses.retire(press.voice_id);
                            self.echo_thru(press, Some(velocity), false);
                        } else {
                            // Retained so it can be retried: never optimistic.
                            complete = false;
                        }
                    }
                    // Deferred under the pedal. The plugin hears nothing yet, and neither does
                    // anything downstream of thru — the echo happens after sustain handling.
                    ReleaseOutcome::Deferred(_) => {}
                    ReleaseOutcome::Unmatched => {}
                },

                Payload::SustainPedal(held) => {
                    let mut owed = std::mem::take(&mut self.scratch);
                    self.presses.set_sustain(held, &mut owed);
                    for press in owed.iter().copied() {
                        if self.emit_note_off(press, 0.0, time) {
                            self.presses.retire(press.voice_id);
                            self.echo_thru(press, Some(0.0), false);
                        } else {
                            complete = false;
                        }
                    }
                    self.scratch = owed;
                }

                Payload::CleanupSource => {
                    if !self.cleanup_source(event.source, time) {
                        complete = false;
                    }
                }

                Payload::GlobalPanic => {
                    if self.emit_global_recovery(time) {
                        self.presses.clear();
                        // Atomic with clearing the table: the sequencer's notes are gone whatever
                        // it believed, and it must not try to release presses that no longer
                        // exist. It picks up again at the next step boundary.
                        self.sequencer.forget_sounding();
                        self.seq_owed_release = crate::sequencer::Step::EMPTY;
                    } else {
                        complete = false;
                    }
                    // An effect has no note port for the recovery to reach; CLAP's `reset` is
                    // its panic. A tail that outlived All Sound Off would not be a panic.
                    self.fx.reset_all();
                    self.raise_out_panic();
                }

                Payload::ControlChange {
                    channel,
                    controller,
                    value,
                } => {
                    // A claimed CC is a parameter edit, not a controller message. It goes to the
                    // GUI, which owns the map, and deliberately **not** also to the plugin: two
                    // live paths to one value is how a knob ends up acting twice.
                    //
                    // The reserved CCs can never be claimed (`control_map::schema::RESERVED_CCS`),
                    // so All Sound Off and All Notes Off always reach the plugin and the panic
                    // machinery keeps working.
                    if self.claimed_ccs.contains(controller) {
                        let _ =
                            self.to_gui
                                .push(crate::engine::PluginOutput::MappedControlChange {
                                    controller,
                                    value,
                                });
                        continue;
                    }

                    if self
                        .envelope
                        .note_input
                        .is_some_and(|p| p.dialect == Dialect::Midi1)
                        || self.envelope.note_input.is_none()
                    {
                        complete &=
                            self.push_midi(time, [0xb0 | (channel & 0x0f), controller, value]);
                    } else {
                        // The CLAP dialect has no controller messages; mxm-mono-01 and every other
                        // MXM plugin also advertise MIDI, so this is only reached for a
                        // CLAP-only plugin, where the controller genuinely cannot be expressed.
                        complete &=
                            self.push_midi(time, [0xb0 | (channel & 0x0f), controller, value]);
                    }
                }

                Payload::PitchBend { channel, value } => {
                    let raw = (value.clamp(0.0, 1.0) * 16_383.0) as u16;
                    complete &= self.push_midi(
                        time,
                        [
                            0xe0 | (channel & 0x0f),
                            (raw & 0x7f) as u8,
                            ((raw >> 7) & 0x7f) as u8,
                        ],
                    );
                }

                Payload::ParamValue { param_id, value } => {
                    let Some(id) = ClapId::from_raw(param_id) else {
                        continue;
                    };
                    complete &= self.input_events.push_event(&ParamValueEvent::new(
                        time,
                        id,
                        Pckn::match_all(),
                        value,
                        Cookie::empty(),
                    ));
                }
                Payload::ParamMod { param_id, value } => {
                    if let Some(id) = ClapId::from_raw(param_id) {
                        complete &= self.input_events.push_event(&ParamModEvent::new(
                            time,
                            id,
                            Pckn::match_all(),
                            value,
                            Cookie::empty(),
                        ));
                    }
                }
                Payload::GestureBegin { param_id } => {
                    if let Some(id) = ClapId::from_raw(param_id) {
                        complete &= self
                            .input_events
                            .push_event(&ParamGestureBeginEvent::new(time, id));
                    }
                }
                Payload::GestureEnd { param_id } => {
                    if let Some(id) = ClapId::from_raw(param_id) {
                        complete &= self
                            .input_events
                            .push_event(&ParamGestureEndEvent::new(time, id));
                    }
                }
            }
        }

        self.merged = merged;

        while next_action < actions.len() {
            next_action += self.emit_from(&actions, next_action, &mut complete);
        }
        actions.clear();
        self.seq_actions = actions;

        complete
    }

    /// Emits the action at `index`, taking **two** when they form a legato joint.
    ///
    /// Returns how many were consumed, so a caller advancing an index stays in step with it.
    ///
    /// A tie carrying notes emits `Sound` then `Release` at the same frame — the runtime puts them
    /// in that order deliberately, and it is the only thing that does; an ordinary step start
    /// releases *before* it sounds. Seeing the pair here is therefore an exact test for the joint,
    /// and it must be handled as one unit, because the release has to be resolved against the
    /// press table **before** the note-ons add to it.
    fn emit_from(
        &mut self,
        actions: &[crate::sequencer::Action],
        index: usize,
        complete: &mut bool,
    ) -> usize {
        use crate::sequencer::Action;

        if let Action::Sound { frame, .. } = actions[index]
            && let Some(release @ Action::Release { frame: joint, .. }) =
                actions.get(index + 1).copied()
            && frame == joint
        {
            *complete &= self.emit_legato_joint(actions[index], release);
            return 2;
        }

        *complete &= self.emit_sequencer_action(actions[index]);
        1
    }

    /// A tie's joint: capture, sound, then release what was captured.
    ///
    /// **The hazard this exists for.** `PressTable::take_exact` resolves a release with
    /// `rposition` — the *newest* matching press. Sound C3 while C3 is already held, then ask to
    /// release C3, and it hands back the press that was just made: the old note never ends and the
    /// new one does. A tie to the same pitch is exactly that case, and so is a chord transition
    /// that shares a note.
    ///
    /// So the identities are resolved first, while the table still holds only the outgoing
    /// presses, and the note-offs go to *those* presses rather than to whatever a fresh lookup
    /// would find.
    ///
    /// **Capturing early is not retiring early.** `take_exact` deliberately returns a press
    /// without removing it, so a refused event can be retried and nothing is treated as delivered
    /// that no plugin received. Each press is retired only when its note-off is accepted, exactly
    /// as on the ordinary path.
    fn emit_legato_joint(
        &mut self,
        sound: crate::sequencer::Action,
        release: crate::sequencer::Action,
    ) -> bool {
        let source = crate::sequencer::SEQUENCER_SOURCE;
        let mut complete = true;

        // 1. Capture, before anything is emitted.
        let mut captured = std::mem::take(&mut self.legato);
        captured.clear();
        let presses = &mut self.presses;
        let Some(outgoing) = release.notes() else {
            // Only a `Release` reaches here; `emit_from` matched on it to get this far.
            return true;
        };
        outgoing.for_each(|key| {
            if let Some(press) = presses.take_exact(source, 0, key) {
                captured.push(press);
            }
        });

        // 2. The new notes, so the plugin sees them arrive while the old ones are still held.
        complete &= self.emit_sequencer_action(sound);

        // 3. The old notes, against the identities captured in step 1.
        let frame = release.frame();
        for press in captured.drain(..) {
            if self.emit_note_off(press, 0.0, frame) {
                self.presses.retire(press.voice_id);
                self.echo_thru(press, Some(0.0), false);
            } else {
                complete = false;
            }
        }

        self.legato = captured;
        complete
    }

    /// Sounds or releases one step's notes, through the press table.
    ///
    /// **Releases bypass host sustain.** `PressTable` carries one global `sustain_held` and defers
    /// matching releases while it is set; routing a sequencer gate through that would let the pedal
    /// hold it, turning a fixed 50% gate into exactly the drone Pause is meant to prevent. A test
    /// utility's gate must not change because a pedal is down.
    fn emit_sequencer_action(&mut self, action: crate::sequencer::Action) -> bool {
        use crate::sequencer::Action;
        let source = crate::sequencer::SEQUENCER_SOURCE;
        let mut complete = true;

        match action {
            Action::Sound { frame, notes } => notes.for_each(|key| {
                let Some(press) = self.presses.press(source, 0, key) else {
                    // An untracked press is a note we could never release. Refuse to send it.
                    complete = false;
                    return;
                };
                if self.emit_note_on(press, SEQUENCER_VELOCITY, frame) {
                    self.echo_thru(press, Some(SEQUENCER_VELOCITY), true);
                } else {
                    self.presses.retire(press.voice_id);
                    complete = false;
                }
            }),

            // **A lock is automation, not a gesture**, and a dropped one must not lower `complete`.
            // `process` answers `!complete` with global recovery: it raises the panic epoch, clears
            // the press table and emits an all-notes-off. That is right for a note that could not be
            // sent and catastrophic for a filter value — one cutoff that did not fit would silence
            // everything. So this counts and moves on. It is the single most dangerous line in the
            // feature, because getting it wrong makes a player that panics under load and looks
            // like a plugin fault.
            Action::SetParam { frame, key, value } => {
                // **No suppression while a knob is held, and that is the point of modulation.** The
                // hand moves the parameter's *value*; the sequencer moves an offset laid over it.
                // They are different layers and cannot fight, so a value set during playback simply
                // stays set and the sequence goes on riding above it.
                //
                // An earlier version stood the automation down while a gesture was open, which was
                // right when a lock was a `PARAM_VALUE`. Kept after the change to modulation, it
                // froze the offset instead: the parameter stuck at whichever step had last fired,
                // for as long as the knob was held.
                let Some(id) = ClapId::from_raw(key.param_id) else {
                    return true;
                };

                // **An effect's automation goes to that effect, resolved by identity.**
                //
                // A lock naming an effect that is no longer in the chain is dropped here, silently
                // and on purpose: this is the audio thread, there is nowhere to report to, and it is
                // the last line of defence behind the removal that should already have deleted it.
                // Dropping is what makes a stale lock harmless rather than a modulation delivered to
                // whichever plugin happens to hash the same parameter id.
                if !key.is_source() {
                    let sent = self
                        .fx
                        .stage_mut(key.fx)
                        .is_some_and(|stage| stage.push_param_mod(frame, id, value));
                    if !sent {
                        self.dropped_locks = self.dropped_locks.saturating_add(1);
                        if value == 0.0 {
                            self.sequencer.re_owe_zero(key);
                        }
                    }
                    // The panel is told either way: it shows what the sequencer asked for.
                    if self
                        .to_gui
                        .push(crate::engine::PluginOutput::SequencerParam {
                            param_id: key,
                            value,
                        })
                        .is_err()
                    {
                        self.shared.note_unsettled_params();
                    }
                    return true;
                }
                // **Modulation, not a value.** CLAP modulation is laid over a parameter without
                // disturbing it, so the parameter's own value stays the patch for as long as the
                // sequence runs. Every defect this feature has had was some version of a step's
                // value becoming the patch; sending an offset makes that structurally impossible
                // rather than something the host has to be careful about.
                let sent = self.input_events.push_event(&ParamModEvent::new(
                    frame,
                    id,
                    Pckn::match_all(),
                    f64::from(value),
                    Cookie::empty(),
                ));
                if !sent {
                    self.dropped_locks = self.dropped_locks.saturating_add(1);
                    // **A dropped zero is re-owed; a dropped deviation is not.** The next step
                    // replaces a deviation, so losing one costs a few milliseconds of the wrong
                    // filter. Nothing replaces a zero — it is the *end* of the modulation — so
                    // losing it leaves the parameter permanently offset.
                    if value == 0.0 {
                        // **Back to the runtime**, which owns the debt and can prove it bounded.
                        // A dropped deviation is replaced by the next step; a dropped zero is the
                        // *end* of the modulation and nothing replaces it.
                        self.sequencer.re_owe_zero(key);
                    }
                    return true;
                }

                // Tell the GUI. A drop here is not harmless on its own: usually the next step
                // republishes and the panel catches up, but the **last** notification before the
                // transport stops has no next step behind it, and a drop there leaves the panel and
                // the pickup state stale indefinitely. So a failure sets a flag the GUI answers with
                // a full requery — a flag cannot be lost, and coalescing is exactly what it is for.
                if self
                    .to_gui
                    .push(crate::engine::PluginOutput::SequencerParam {
                        param_id: key,
                        value,
                    })
                    .is_err()
                {
                    self.shared.note_unsettled_params();
                }
                return true;
            }

            Action::Release { frame, notes } => notes.for_each(|key| {
                let Some(press) = self.presses.take_exact(source, 0, key) else {
                    return;
                };
                if self.emit_note_off(press, 0.0, frame) {
                    self.presses.retire(press.voice_id);
                    self.echo_thru(press, Some(0.0), false);
                } else {
                    complete = false;
                }
            }),
        }
        complete
    }

    /// Targeted cleanup: one choke per outstanding press of this source, carrying its voice ID.
    ///
    /// Physical MIDI input is left entirely alone when the GUI is cleaned up — that hardware
    /// still has the key down and will send its own note-off.
    fn cleanup_source(&mut self, source: SourceId, time: u32) -> bool {
        let mut outstanding = std::mem::take(&mut self.scratch);
        self.presses.outstanding_for(source, &mut outstanding);
        if outstanding.is_empty() {
            self.scratch = outstanding;
            return true;
        }

        // On a MIDI-dialect port, voice IDs cannot be transmitted at all. Targeted cleanup is
        // then only safe when no other source is holding the same channel and pitch; otherwise
        // nothing in the message distinguishes them and the honest answer is the global path.
        let targeted = self.envelope.note_input_carries_voice_ids()
            || !self.overlaps_other_source(source, &outstanding);

        if !targeted {
            let accepted = self.emit_global_recovery(time);
            if accepted {
                self.presses.clear();
                self.raise_out_panic();
            }
            self.scratch = outstanding;
            return accepted;
        }

        let mut all_accepted = true;
        for press in outstanding.iter().copied() {
            if self.emit_choke(press, time) {
                // Accounting is zeroed only once the choke has been accepted, never
                // optimistically: a refused choke must keep what is needed to retry it.
                self.presses.retire(press.voice_id);
                self.echo_thru(press, Some(0.0), false);
            } else {
                all_accepted = false;
            }
        }
        self.scratch = outstanding;
        all_accepted
    }

    fn overlaps_other_source(&self, source: SourceId, outstanding: &[Press]) -> bool {
        self.presses.iter().any(|other| {
            other.source != source
                && outstanding
                    .iter()
                    .any(|p| p.channel == other.channel && p.key == other.key)
        })
    }

    fn note_port(&self) -> u16 {
        self.envelope.note_input.map(|p| p.index).unwrap_or(0)
    }

    fn dialect(&self) -> Dialect {
        self.envelope
            .note_input
            .map(|p| p.dialect)
            .unwrap_or(Dialect::Clap)
    }

    fn emit_note_on(&mut self, press: Press, velocity: f64, time: u32) -> bool {
        match self.dialect() {
            Dialect::Clap => {
                let pckn = Pckn::new(
                    self.note_port(),
                    u16::from(press.channel),
                    u16::from(press.key),
                    press.voice_id as u32,
                );
                self.input_events
                    .push_event(&NoteOnEvent::new(time, pckn, velocity))
            }
            Dialect::Midi1 => self.push_midi(
                time,
                [
                    0x90 | (press.channel & 0x0f),
                    press.key & 0x7f,
                    ((velocity * 127.0).round().clamp(1.0, 127.0)) as u8,
                ],
            ),
        }
    }

    fn emit_note_off(&mut self, press: Press, velocity: f64, time: u32) -> bool {
        match self.dialect() {
            Dialect::Clap => {
                let pckn = Pckn::new(
                    self.note_port(),
                    u16::from(press.channel),
                    u16::from(press.key),
                    press.voice_id as u32,
                );
                self.input_events
                    .push_event(&NoteOffEvent::new(time, pckn, velocity))
            }
            Dialect::Midi1 => self.push_midi(
                time,
                [
                    0x80 | (press.channel & 0x0f),
                    press.key & 0x7f,
                    ((velocity * 127.0).round().clamp(0.0, 127.0)) as u8,
                ],
            ),
        }
    }

    fn emit_choke(&mut self, press: Press, time: u32) -> bool {
        match self.dialect() {
            Dialect::Clap => {
                let pckn = Pckn::new(
                    self.note_port(),
                    u16::from(press.channel),
                    u16::from(press.key),
                    press.voice_id as u32,
                );
                self.input_events
                    .push_event(&NoteChokeEvent::new(time, pckn))
            }
            // MIDI has no choke; the nearest honest equivalent is a note-off.
            Dialect::Midi1 => self.emit_note_off(press, 0.0, time),
        }
    }

    /// Global recovery, **in the dialect the plugin actually negotiated**.
    ///
    /// A CLAP-only note port never receives CC 120, so ending the emergency path there would be
    /// ignored and the very notes the panic exists to clear would stay stuck.
    fn emit_global_recovery(&mut self, time: u32) -> bool {
        match self.envelope.global_recovery() {
            GlobalRecovery::AllSoundOff => {
                let mut accepted = true;
                for channel in 0..16u8 {
                    accepted &= self.push_midi(time, [0xb0 | channel, 120, 0]);
                }
                accepted
            }
            GlobalRecovery::WildcardChoke => {
                let pckn = Pckn::match_all();
                self.input_events
                    .push_event(&NoteChokeEvent::new(time, pckn))
            }
        }
    }

    fn push_midi(&mut self, time: u32, data: [u8; 3]) -> bool {
        let port = self.note_port();
        self.input_events
            .push_event(&MidiEvent::new(time, port, data))
    }

    /// Echoes a keyboard event to MIDI out, **after** host sustain handling, so external
    /// hardware receives the same deferred note-offs the plugin does.
    fn echo_thru(&mut self, press: Press, velocity: Option<f64>, on: bool) {
        if !press.source.is_gui() || !self.thru_enabled.load(Ordering::Relaxed) {
            return;
        }
        let Some(queue) = self.midi_out.as_mut() else {
            return;
        };

        let velocity = velocity.unwrap_or(0.0);
        let data = if on {
            [
                0x90 | (press.channel & 0x0f),
                press.key & 0x7f,
                ((velocity * 127.0).round().clamp(1.0, 127.0)) as u8,
            ]
        } else {
            [0x80 | (press.channel & 0x0f), press.key & 0x7f, 0]
        };

        let event = crate::midi::OutgoingEvent {
            epoch: self.out_panic.epoch(),
            data,
        };
        if queue.push(event).is_err() && !on {
            // A dropped release leaves a note sounding on external hardware, which is exactly
            // what the out-panic exists to clear. It never spins.
            self.out_panic.raise();
        }
    }

    /// Anything that silences notes at the plugin must also silence what thru has sent outward.
    fn raise_out_panic(&mut self) {
        if self.thru_enabled.load(Ordering::Relaxed) {
            self.out_panic.raise();
        }
    }

    /// Runs the source and then the chain over the callback's frames, in chunks of at most
    /// [`MAX_BLOCK_FRAMES`]. Returns the time spent in the source and the time spent in effects.
    fn render(
        &mut self,
        output: &mut [f32],
        channel_count: usize,
    ) -> (std::time::Duration, std::time::Duration) {
        let total_frames = output.len() / channel_count.max(1);
        let mut plugin_time = std::time::Duration::ZERO;
        let mut fx_time = std::time::Duration::ZERO;
        let mut done = 0usize;
        // Events land on the first chunk's boundary: P2 applies everything at the buffer
        // boundary, and sample-accurate offsets arrive at P4.
        let mut events_pending = true;

        while done < total_frames {
            let frames = (total_frames - done).min(MAX_BLOCK_FRAMES as usize);

            // The sequencer advances for **every** chunk, before the sleep/wake decision below.
            // A clock that stopped while the plugin slept would never reach its next boundary —
            // and a rest is exactly what puts a plugin to sleep. Its notes then land in
            // `input_events`, which is what `woken` tests, so the sequencer wakes the plugin by
            // construction rather than through a second mechanism.
            self.advance_sequencer(frames as u32);

            if events_pending {
                if !self.convert_events(frames as u32, true) {
                    // Conversion could not complete: we have lost track, so recover globally.
                    let epoch = self.input_epoch.raise();
                    self.merged.discard_pre_epoch_notes(epoch, None);
                    self.input_events.clear();
                    self.emit_global_recovery(0);
                    self.presses.clear();
                    self.sequencer.forget_sounding();
                    self.seq_owed_release = crate::sequencer::Step::EMPTY;
                    self.raise_out_panic();
                }
                events_pending = false;
            } else {
                // Queued events all landed on the first chunk; the sequencer still has its own.
                let _ = self.convert_events(frames as u32, false);
            }

            // The flags are **taken**, not read: a latched flush would keep the source awake for
            // the rest of the session, and processing this chunk is what satisfies the request.
            let woken = !self.input_events.is_empty()
                | self.shared.requests.process.swap(false, Ordering::AcqRel)
                | self
                    .shared
                    .requests
                    .param_flush
                    .swap(false, Ordering::AcqRel);
            if woken && self.state == RunState::Sleeping {
                self.state = RunState::Running;
                self.quiet_buffers = 0;
            }

            let source_running = self.state == RunState::Running;

            // **The whole graph sleeps only when nothing in it would run**: the source asleep, no
            // effect open, and no effect asking to be run. That is the one path that touches no
            // plugin and no buffer.
            //
            // The third clause is the one that was missing: an effect asks to be processed when
            // its own editor moves a parameter, and a graph that slept through the request left
            // the plugin never taking the edit — see `FxChain::wants_processing`.
            if !source_running && !self.fx.any_open() && !self.fx.wants_processing() {
                for frame in 0..frames {
                    for channel in 0..channel_count {
                        output[(done + frame) * channel_count + channel] = 0.0;
                    }
                }
                done += frames;
                // The clock moved even though the plugin slept, so the GUI must still see it.
                self.publish_playhead();
                continue;
            }

            if source_running {
                plugin_time += self.process_chunk(frames);
                self.route_output_events();
                self.steady_time += frames as u64;
            } else {
                // The source sleeps and an effect is still draining: it gets silence, and the
                // source's own clock does not move — it is asleep, whatever the effect is doing.
                for channel in &mut self.channels {
                    channel[..frames].fill(0.0);
                }
            }

            // The chain, on whatever the source produced. Its transport is the same chunk's.
            let transport = self.transport_event();
            let (result, elapsed) = self.fx.run(&self.channels, frames, &transport);
            fx_time += elapsed;
            match result {
                None => self.interleave_from(&self.channels, output, done, frames, channel_count),
                Some(stage) => {
                    let buffers = self.fx.output(stage);
                    Self::interleave_buffers(buffers, output, done, frames, channel_count);
                }
            }

            done += frames;
            self.publish_playhead();
        }

        (plugin_time, fx_time)
    }

    /// Advances the sequencer's clock and collects what this chunk must do.
    ///
    /// Any release owed from a transport change handled by `service_commands` — which renders
    /// nothing and so cannot deliver note-offs — is prepended here, at frame 0, now that a
    /// `process()` call is actually going to happen.
    fn advance_sequencer(&mut self, frames: u32) {
        let mut actions = std::mem::take(&mut self.seq_actions);
        actions.clear();

        let owed = std::mem::replace(&mut self.seq_owed_release, crate::sequencer::Step::EMPTY);
        if !owed.is_empty() {
            actions.push(crate::sequencer::Action::Release {
                frame: 0,
                notes: owed,
            });
        }

        // The runtime emits its own owed work — the zeroes for what it abandoned, and the preview
        // of a selected step — at the start of `advance`. It has to: it is the only thing that knows
        // what it has applied, and that knowledge is what bounds the debt.
        actions.extend_from_slice(self.sequencer.advance(frames));
        self.seq_actions = actions;
    }

    /// Publishes the sequencer's position for the GUI. Wait-free.
    fn publish_playhead(&self) {
        let transport = match self.sequencer.transport() {
            crate::sequencer::Transport::Stopped => 0u8,
            crate::sequencer::Transport::Playing => 1,
            crate::sequencer::Transport::Paused => 2,
        };
        self.shared
            .set_playhead(transport, self.sequencer.step() as u32);
    }

    /// The transport this chunk runs under.
    ///
    /// **Not derived from the sequencer's clock — it is the same number.** `song_pos_beats` is the
    /// sequencer's own accumulator, which is what makes disagreement impossible rather than merely
    /// unlikely. CLAP defines the process transport as the state *as of sample 0*, and the tempo
    /// only ever changes between chunks, so every field here is true for the whole chunk.
    fn transport_event(&self) -> TransportEvent {
        let beats = self.sequencer.beats();
        let mut flags = TransportFlags::HAS_TEMPO
            | TransportFlags::HAS_BEATS_TIMELINE
            | TransportFlags::HAS_SECONDS_TIMELINE
            | TransportFlags::HAS_TIME_SIGNATURE;
        if self.sequencer.transport().is_playing() {
            flags |= TransportFlags::IS_PLAYING;
        }

        // 4/4, so a bar is four beats. The bar fields have to agree with the beat position rather
        // than being plausible on their own.
        let bar_number = (beats / 4.0).floor();

        TransportEvent {
            header: EventHeader::new_core(0, EventFlags::empty()),
            flags,
            song_pos_beats: BeatTime::from_float(beats),
            song_pos_seconds: SecondsTime::from_float(self.sequencer.seconds()),
            tempo: self.sequencer.tempo(),
            tempo_inc: 0.0,
            loop_start_beats: BeatTime::from_int(0),
            loop_end_beats: BeatTime::from_int(0),
            loop_start_seconds: SecondsTime::from_int(0),
            loop_end_seconds: SecondsTime::from_int(0),
            bar_start: BeatTime::from_float(bar_number * 4.0),
            bar_number: bar_number as i32,
            time_signature_numerator: 4,
            time_signature_denominator: 4,
        }
    }

    fn process_chunk(&mut self, frames: usize) -> std::time::Duration {
        let transport = self.transport_event();
        self.output_events.clear();
        for channel in &mut self.channels {
            channel[..frames].fill(0.0);
        }

        let started = Instant::now();
        let status = {
            let Some(processor) = self.owner.started_mut() else {
                return std::time::Duration::ZERO;
            };

            let mut audio_outputs = self.ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(
                    self.channels.iter_mut().map(|c| &mut c[..frames]),
                ),
            }]);

            processor.process(
                &self.silent_input,
                &mut audio_outputs,
                &InputEvents::from_buffer(&self.input_events),
                &mut OutputEvents::from_buffer(&mut self.output_events),
                Some(self.steady_time),
                Some(&transport),
            )
        };
        let elapsed = started.elapsed();

        match status {
            Ok(status) => self.apply_status(status, frames),
            Err(_) => {
                // Move to a safe idle state and surface it. Never called again in a tight loop.
                self.state = RunState::Failed;
                for channel in &mut self.channels {
                    channel[..frames].fill(0.0);
                }
            }
        }

        elapsed
    }

    fn apply_status(&mut self, status: ProcessStatus, frames: usize) {
        match status {
            ProcessStatus::Continue => {
                self.quiet_buffers = 0;
                self.tail_frames_remaining = None;
            }
            ProcessStatus::ContinueIfNotQuiet => {
                // Exact zero, across several consecutive buffers. No nonzero threshold in v1: a
                // threshold would truncate quiet release tails.
                let silent = self
                    .channels
                    .iter()
                    .all(|c| c[..frames].iter().all(|s| *s == 0.0));
                if silent {
                    self.quiet_buffers += 1;
                    if self.quiet_buffers >= QUIET_BUFFERS_BEFORE_SLEEP {
                        self.state = RunState::Sleeping;
                    }
                } else {
                    self.quiet_buffers = 0;
                }
            }
            ProcessStatus::Tail => {
                self.quiet_buffers = 0;
                self.update_tail(frames);
            }
            ProcessStatus::Sleep => {
                self.state = RunState::Sleeping;
                self.quiet_buffers = 0;
                self.tail_frames_remaining = None;
            }
        }
    }

    /// `Tail` carries no duration: the length, if available at all, comes from the separate
    /// optional `tail` extension.
    fn update_tail(&mut self, frames: usize) {
        // A `changed` notification invalidates any cached finite value — sleeping on a stale one
        // would truncate the tail.
        if self
            .shared
            .notifications
            .tail_changed
            .swap(false, Ordering::AcqRel)
        {
            self.cached_tail = None;
            self.tail_frames_remaining = None;
        }

        if self.cached_tail.is_none()
            && let (Some(tail), Some(processor)) = (self.tail, self.owner.processor_mut())
        {
            self.cached_tail = Some(tail.get(&processor.plugin_handle()));
        }

        match self.cached_tail {
            Some(TailLength::Finite(samples)) => {
                let remaining = self
                    .tail_frames_remaining
                    .unwrap_or(u64::from(samples))
                    .saturating_sub(frames as u64);
                self.tail_frames_remaining = Some(remaining);
                if remaining == 0 && self.input_events.is_empty() {
                    self.state = RunState::Sleeping;
                }
            }
            // Infinite, or no `tail` extension at all: keep going until the plugin says `Sleep`.
            _ => self.tail_frames_remaining = None,
        }
    }

    fn interleave_from(
        &self,
        buffers: &[Vec<f32>],
        output: &mut [f32],
        offset: usize,
        frames: usize,
        channel_count: usize,
    ) {
        Self::interleave_buffers(buffers, output, offset, frames, channel_count);
    }

    /// Interleaves `buffers` into the device's frames, adapting the channel count once more:
    /// mono into a stereo device is duplicated, stereo into a mono device is summed at half.
    fn interleave_buffers(
        buffers: &[Vec<f32>],
        output: &mut [f32],
        offset: usize,
        frames: usize,
        channel_count: usize,
    ) {
        let Some(first) = buffers.first() else {
            output[offset * channel_count..(offset + frames) * channel_count].fill(0.0);
            return;
        };
        for frame in 0..frames {
            for channel in 0..channel_count {
                let source = match (buffers.len(), channel_count) {
                    // Stereo into a mono device: the sum at half, never one side.
                    (2, 1) => 0.5 * (buffers[0][frame] + buffers[1][frame]),
                    _ => buffers
                        .get(channel)
                        .map(|c| c[frame])
                        // A mono source driving a stereo device: duplicate rather than go silent.
                        .unwrap_or_else(|| first[frame]),
                };
                output[(offset + frame) * channel_count + channel] = source;
            }
        }
    }

    /// Routes what the plugin emitted: parameters to the GUI, representable notes and MIDI to
    /// the MIDI-out worker, everything else counted.
    fn route_output_events(&mut self) {
        // Swapped out rather than cloned, for the same reason as the merged input list.
        let mut emitted =
            std::mem::replace(&mut self.output_events, FixedEventBuffer::placeholder());

        // Anything the sink refused is output the host never saw, which may include a gesture
        // `end`. The GUI is told, once per occurrence, so it can close what it is tracking.
        let overflowed = emitted.counters().overflowed;
        if overflowed > self.last_output_overflow {
            self.last_output_overflow = overflowed;
            let _ = self
                .to_gui
                .push(crate::engine::PluginOutput::TrackingInvalidated);
        }
        let epoch = self.out_panic.epoch();
        let mut panic_needed = false;

        for event in emitted.iter() {
            match classify(event) {
                OutputRoute::Gui => {
                    if let Some(output) = crate::engine::PluginOutput::from_event(event) {
                        // A full GUI queue costs a stale panel reading, not a stuck note, so it
                        // is dropped rather than escalated.
                        let _ = self.to_gui.push(output);
                    }
                }
                OutputRoute::MidiOut => match crate::midi::to_midi1(event) {
                    Some(data) => {
                        if let Some(queue) = self.midi_out.as_mut()
                            && queue
                                .push(crate::midi::OutgoingEvent { epoch, data })
                                .is_err()
                            && crate::midi::is_release(data)
                        {
                            panic_needed = true;
                        }
                    }
                    None => self.output_events.count_unrepresented(),
                },
                OutputRoute::Unrepresentable => self.output_events.count_unrepresented(),
                OutputRoute::Rejected => self.output_events.count_rejected(),
            }
        }

        if panic_needed {
            self.out_panic.raise();
        }

        emitted.absorb_counters(&self.output_events);
        self.output_events = emitted;
    }
}

/// Whether a later event at the same sample offset makes this one stale.
///
/// Only continuous controllers coalesce: a note, a gesture boundary or a panic is never merged
/// away, and a value that lands on a different offset is a value the listener would hear.
fn superseded_at_same_offset(
    merged: &MergedInput,
    index: usize,
    time: u32,
    frames: u32,
    worker: &AudioWorker,
) -> bool {
    let event = merged.as_slice()[index];

    merged.as_slice()[index + 1..].iter().any(|later| {
        worker.offset_for(later.arrival_nanos, frames) == time
            && overtakes(event.payload, later.payload)
    })
}

/// Whether `later` makes `earlier` pointless: the same control, moved again.
///
/// Only continuous streams coalesce. A note, a gesture boundary or a panic is never merged — each
/// one *is* the event rather than a sample of a value on its way somewhere.
///
/// **Modulation coalesces exactly as a value does.** Editing a step is a drag, so it produces one
/// per frame; the earlier ones are values the parameter passed through on the way, and sending them
/// all means the plugin smooths toward each in turn for no audible gain. In combination with a busy
/// MIDI port and a dense bar of sequencer events it is also what fills the input arena, and a full
/// arena lowers `complete`, which invokes global recovery.
fn overtakes(earlier: Payload, later: Payload) -> bool {
    match (earlier, later) {
        (
            Payload::ControlChange {
                channel: a,
                controller: ca,
                ..
            },
            Payload::ControlChange {
                channel: b,
                controller: cb,
                ..
            },
        ) => a == b && ca == cb,
        (Payload::PitchBend { channel: a, .. }, Payload::PitchBend { channel: b, .. }) => a == b,
        (Payload::ParamValue { param_id: a, .. }, Payload::ParamValue { param_id: b, .. }) => {
            a == b
        }
        (Payload::ParamMod { param_id: a, .. }, Payload::ParamMod { param_id: b, .. }) => a == b,
        _ => false,
    }
}
#[cfg(test)]
mod coalescing_tests {
    use super::overtakes;
    use crate::events::input::Payload;

    fn modulation(param_id: u32, value: f64) -> Payload {
        Payload::ParamMod { param_id, value }
    }

    #[test]
    fn a_later_modulation_overtakes_an_earlier_one_on_the_same_parameter() {
        // **Editing a step is a drag**, so it produces one of these per frame and they pile into one
        // buffer. Only the last is worth sending; the rest are values the parameter passed through.
        assert!(overtakes(modulation(7, 0.1), modulation(7, 0.2)));
    }

    #[test]
    fn one_parameters_drag_does_not_swallow_anothers() {
        assert!(!overtakes(modulation(7, 0.1), modulation(9, 0.2)));
    }

    #[test]
    fn a_modulation_and_a_value_do_not_overtake_each_other() {
        // Different layers: one moves the parameter, the other is laid over it. Coalescing across
        // them would drop an edit rather than an intermediate.
        let value = Payload::ParamValue {
            param_id: 7,
            value: 0.1,
        };
        assert!(!overtakes(value, modulation(7, 0.2)));
        assert!(!overtakes(modulation(7, 0.2), value));
    }

    #[test]
    fn a_note_is_never_overtaken() {
        // The rule that keeps coalescing safe: a note *is* the event, not a sample of one.
        let note = Payload::NoteOn {
            channel: 0,
            key: 60,
            velocity: 1.0,
        };
        assert!(!overtakes(note, note));
    }
}
