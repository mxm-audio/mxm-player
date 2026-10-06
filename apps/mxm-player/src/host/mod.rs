//! The host side of every CLAP callback the player registers.
//!
//! The rule that shapes this module: **nothing on a plugin or audio thread ever calls into
//! egui**. `request_repaint()` reaches `Context::write()`, which takes an `RwLock`, so plugin
//! callbacks only ever store atomics here. A dedicated notifier thread polls them and wakes the
//! GUI on their behalf (see `crate::notifier`).

pub mod log;

use clack_extensions::audio_ports::{AudioPortRescanFlags, HostAudioPorts, HostAudioPortsImpl};
use clack_extensions::audio_ports_config::{HostAudioPortsConfig, HostAudioPortsConfigImpl};
use clack_extensions::gui::{GuiSize, HostGui, HostGuiImpl};
use clack_extensions::latency::{HostLatency, HostLatencyImpl};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::note_ports::{
    HostNotePorts, HostNotePortsImpl, NoteDialects, NotePortRescanFlags,
};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamRescanFlags,
};
use clack_extensions::state::{HostState, HostStateImpl};
use clack_extensions::tail::{HostTail, HostTailImpl};
use clack_host::prelude::*;
use log::LogSink;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Identity the player presents to plugins.
pub fn host_info() -> HostInfo {
    HostInfo::new(
        "MXM Player",
        "mxm",
        "https://mxm.dk",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("the host info strings are static and contain no interior nul")
}

/// The set of host callback handlers.
pub struct MxmHost;

impl HostHandlers for MxmHost {
    type Shared<'a> = HostShared;
    type MainThread<'a> = HostMainThread<'a>;
    type AudioProcessor<'a> = HostAudioProcessor<'a>;

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &HostShared) {
        builder
            .register::<HostLog>()
            .register::<HostParams>()
            .register::<HostAudioPorts>()
            .register::<HostAudioPortsConfig>()
            .register::<HostNotePorts>()
            .register::<HostState>()
            .register::<HostLatency>()
            .register::<HostTail>()
            .register::<HostGui>();
    }
}

/// What the plugin has asked the host to do, and anything it has told the host has changed.
///
/// Every field here is written from an arbitrary plugin thread and read from the GUI thread, so
/// all of it is atomic. Nothing in here does work: the GUI turns these into engine commands.
#[derive(Default)]
pub struct HostRequests {
    /// `request_restart`: deactivate and reactivate. Becomes a GUI-issued engine command.
    pub restart: AtomicBool,
    /// `request_process`: start processing again. Becomes a GUI-issued engine command.
    pub process: AtomicBool,
    /// `request_callback`: run `on_main_thread`. **Serviced on the GUI thread**, not turned into
    /// an engine command — waking the GUI is not the same as servicing the request.
    pub callback: AtomicBool,
    /// `params.request_flush`: flush parameters when not processing.
    pub param_flush: AtomicBool,

    /// `gui.request_resize`: the plugin wants its window a different size, packed width-high.
    ///
    /// The plugin owns its floating window, so there is no pane to exceed and the request is
    /// simply honoured. Stored rather than acted on because **`clap.gui` calls must happen on the
    /// main thread** and this may arrive on any thread — clack permits it.
    pub gui_resize: AtomicU64,
    /// Whether [`HostRequests::gui_resize`] holds a request the GUI has not applied yet.
    pub gui_resize_pending: AtomicBool,
    /// `gui.request_show` / `gui.request_hide`.
    pub gui_show: AtomicBool,
    pub gui_hide: AtomicBool,
}

/// Everything a plugin may notify the host about, collected for the GUI to act on.
#[derive(Default)]
pub struct HostNotifications {
    /// `params.rescan` flags, accumulated.
    pub param_rescan: AtomicU32,
    /// Set by `params.clear`; the GUI drops host-side state for the parameter.
    pub param_cleared: AtomicBool,
    /// Which parameter `clear` most recently named.
    pub param_cleared_id: AtomicU32,
    /// `audio-ports` or `audio-ports-config` changed: schedule a restart or mark unsupported.
    pub audio_ports_changed: AtomicBool,
    /// Whether the audio-port change requires deactivation before it can be honoured.
    pub audio_ports_needs_deactivate: AtomicBool,
    /// `note-ports` changed: rebuild dialect and port negotiation.
    pub note_ports_changed: AtomicBool,
    /// `state.mark_dirty`: the session has unsaved state.
    pub state_dirty: AtomicBool,
    /// `latency.changed`: re-query reported latency.
    pub latency_changed: AtomicBool,
    /// `tail.changed`: any cached finite tail is now stale and must not be slept on.
    pub tail_changed: AtomicBool,

    /// `gui.closed`: the editor is gone — the user closed the window, or the connection was lost.
    ///
    /// **This flag is never dropped or coalesced away.** When `gui_closed_was_destroyed` is set,
    /// CLAP requires the host to call `destroy` as an acknowledgement, and nice-plug refuses a
    /// later `create` while its editor handle is still set — so losing this means the editor never
    /// reopens for the life of the instance, silently.
    pub gui_closed: AtomicBool,
    /// Whether the plugin destroyed the window itself, which is what makes `destroy` mandatory.
    pub gui_closed_was_destroyed: AtomicBool,
}

/// Thread-safe host state.
///
/// Held behind an `Arc` so the GUI thread and the notifier thread can watch it without going
/// through the plugin instance, which is `!Send` and lives on the GUI thread.
pub struct PlayerHostState {
    pub requests: HostRequests,
    pub notifications: HostNotifications,
    pub logs: LogSink,
    /// Bumped by every callback above. The notifier thread watches this one value rather than
    /// polling a dozen flags, and it is the only thing that turns into a `request_repaint`.
    wake: AtomicU64,
    /// Where the sequencer is, packed as `transport << 32 | step`.
    ///
    /// **Latest value, not history**, so an atomic rather than a queue: the playhead is state, and
    /// a full queue must never be able to stall the step highlight. The exact position lives on the
    /// audio thread; recomputing it on GUI frames would duplicate the clock and disagree with it
    /// exactly where it matters — around pause, tempo changes, and commands not yet accepted.
    playhead: AtomicU64,
    /// Which MIDI notes an external keyboard is holding down, as a 128-bit set in two halves.
    ///
    /// Written from the **MIDI callback**, which is I/O-free and wait-free by contract, so this is
    /// a pair of atomics and one `fetch_or`/`fetch_and` per note rather than anything shared and
    /// locked. Two halves rather than one value because a note is a single bit in exactly one of
    /// them, so no update ever needs to span both.
    midi_sounding: [AtomicU64; 2],
    /// Every external note that has gone **down** since the GUI last looked, same layout.
    ///
    /// A latch rather than a reading of `midi_sounding`, because that one is state and this is an
    /// event: a step is entered on the press, and a key struck and released between two GUI turns
    /// would leave no trace in the live set at all. Drained with a `swap`, so nothing is counted
    /// twice and nothing needs a queue.
    midi_pressed: [AtomicU64; 2],
    /// The publication serial the audio worker has consumed.
    ///
    /// Bounds the GUI to **one unacknowledged sequencer publication**: a tempo drag at frame rate
    /// would otherwise fill the 64-slot command queue with stale states and delay the `Stop` that
    /// `Engine::stop_now` polls for.
    sequencer_ack: AtomicU64,
    /// Set when a sequencer parameter notification could not be queued, or when the transport came
    /// to rest.
    ///
    /// **A flag rather than a message, and that is the whole design.** The notification channel is
    /// bounded and droppable; a flag cannot be lost, and coalescing many failures into one requery
    /// is exactly what is wanted. One atomic store on the audio thread — wait-free and
    /// allocation-free, the discipline `set_midi_sounding` and the playhead already follow.
    unsettled_params: AtomicBool,
}

/// The handler clack hands to plugin threads: a thin `Arc` handle onto [`PlayerHostState`].
pub struct HostShared {
    state: Arc<PlayerHostState>,
}

impl HostShared {
    pub fn new(state: Arc<PlayerHostState>) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &Arc<PlayerHostState> {
        &self.state
    }
}

impl std::ops::Deref for HostShared {
    type Target = PlayerHostState;

    fn deref(&self) -> &PlayerHostState {
        &self.state
    }
}

impl PlayerHostState {
    pub fn new() -> Self {
        Self {
            requests: HostRequests::default(),
            notifications: HostNotifications::default(),
            logs: LogSink::new(),
            wake: AtomicU64::new(0),
            playhead: AtomicU64::new(0),
            midi_sounding: [AtomicU64::new(0), AtomicU64::new(0)],
            midi_pressed: [AtomicU64::new(0), AtomicU64::new(0)],
            sequencer_ack: AtomicU64::new(0),
            unsettled_params: AtomicBool::new(false),
        }
    }

    /// Records that something happened the GUI should look at. Stores an atomic and nothing else.
    fn wake(&self) {
        self.wake.fetch_add(1, Ordering::Release);
    }

    /// The current wake generation, for the notifier thread to compare against.
    pub fn wake_generation(&self) -> u64 {
        self.wake.load(Ordering::Acquire)
    }

    /// Publishes the sequencer's position. Called from the audio thread; wait-free.
    ///
    /// Bumps the wake generation only when the step actually changes, so an idle window repaints
    /// as steps advance without being woken every callback.
    /// **`u32`, not `u8`.** The step was narrowed to a byte here, which was a silent 255-step
    /// ceiling on the *reported* position: past bar sixteen the step row lit the wrong cell and
    /// the bar strip would name the wrong bar. The packed word always had the room -- transport
    /// lives above bit 32 -- so the byte bought nothing. The same ceiling, and the same fix, as
    /// `published_editing_step`.
    pub fn set_playhead(&self, transport: u8, step: u32) {
        let packed = (u64::from(transport) << 32) | u64::from(step);
        if self.playhead.swap(packed, Ordering::Release) != packed {
            self.wake();
        }
    }

    /// `(transport, step)` as the audio thread last left them.
    pub fn playhead(&self) -> (u8, u32) {
        let packed = self.playhead.load(Ordering::Acquire);
        ((packed >> 32) as u8, (packed & 0xffff_ffff) as u32)
    }

    /// Records an external MIDI note going down or coming up. Called from the MIDI callback;
    /// wait-free, and wakes the GUI only when the set actually changes.
    ///
    /// The on-screen keyboard is a monitor as well as an input, and a note played on a connected
    /// keyboard has to light the key it sounds — the GUI's own `held` list only ever holds notes
    /// the GUI itself originated.
    pub fn set_midi_sounding(&self, note: u8, down: bool) {
        let (half, bit) = (usize::from(note >= 64), 1u64 << (note % 64));
        let Some(word) = self.midi_sounding.get(half) else {
            return;
        };
        let before = if down {
            // Latched separately: the press is what enters a note into a selected step, and it
            // must survive being released again before the GUI's next turn.
            if let Some(latch) = self.midi_pressed.get(half) {
                latch.fetch_or(bit, Ordering::Release);
            }
            word.fetch_or(bit, Ordering::Release)
        } else {
            word.fetch_and(!bit, Ordering::Release)
        };
        if (before & bit != 0) != down {
            self.wake();
        }
    }

    /// Every MIDI note currently held on a connected keyboard.
    /// Records that the panel may be showing a parameter the sequencer has since moved.
    ///
    /// Called from the audio thread when a notification will not fit, and unconditionally when the
    /// transport comes to rest — so the resting state is correct whatever happened during the run,
    /// which is the moment somebody stops to look at what they made.
    pub fn note_unsettled_params(&self) {
        self.unsettled_params.store(true, Ordering::Release);
        self.wake();
    }

    /// Takes the flag, if it is set. Draining is the point: one requery answers any number of drops.
    pub fn take_unsettled_params(&self) -> bool {
        self.unsettled_params.swap(false, Ordering::AcqRel)
    }

    pub fn midi_sounding(&self) -> Vec<u8> {
        let mut notes = Vec::new();
        for (half, word) in self.midi_sounding.iter().enumerate() {
            let mut bits = word.load(Ordering::Acquire);
            while bits != 0 {
                let bit = bits.trailing_zeros() as u8;
                notes.push(half as u8 * 64 + bit);
                bits &= bits - 1;
            }
        }
        notes
    }

    /// Forgets every external note, for a source cleanup or a port closing.
    ///
    /// Without this a note held as the port disconnects stays lit for ever, with nothing left
    /// that could ever release it.
    pub fn clear_midi_sounding(&self) {
        let mut changed = false;
        for word in &self.midi_sounding {
            changed |= word.swap(0, Ordering::Release) != 0;
        }
        for latch in &self.midi_pressed {
            latch.store(0, Ordering::Release);
        }
        if changed {
            self.wake();
        }
    }

    /// Takes every external note pressed since the last call, lowest first.
    ///
    /// Draining is the point: each press is delivered once, to whoever is editing.
    pub fn take_midi_pressed(&self) -> Vec<u8> {
        let mut notes = Vec::new();
        for (half, latch) in self.midi_pressed.iter().enumerate() {
            let mut bits = latch.swap(0, Ordering::AcqRel);
            while bits != 0 {
                let bit = bits.trailing_zeros() as u8;
                notes.push(half as u8 * 64 + bit);
                bits &= bits - 1;
            }
        }
        notes
    }

    pub fn acknowledge_sequencer(&self, serial: u64) {
        self.sequencer_ack.store(serial, Ordering::Release);
    }

    pub fn sequencer_ack(&self) -> u64 {
        self.sequencer_ack.load(Ordering::Acquire)
    }

    /// Consumes the callback request, if there is one.
    ///
    /// The GUI calls this once per frame and, if it returns true, runs
    /// `PluginInstance::call_on_main_thread_callback()`. A request raised *during* that callback
    /// sets the flag again and is serviced on a later frame rather than recursing.
    pub fn take_callback_request(&self) -> bool {
        self.requests.callback.swap(false, Ordering::AcqRel)
    }
}

impl Default for PlayerHostState {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> SharedHandler<'a> for HostShared {
    fn request_restart(&self) {
        self.requests.restart.store(true, Ordering::Release);
        self.wake();
    }

    fn request_process(&self) {
        self.requests.process.store(true, Ordering::Release);
        self.wake();
    }

    fn request_callback(&self) {
        self.requests.callback.store(true, Ordering::Release);
        self.wake();
    }
}

/// `clap.gui`, host side.
///
/// Every method here may be called from **any** thread — clack permits `request_resize`
/// asynchronously, and a plugin may call the others from wherever it likes. So none of them does
/// work: they record into atomics and wake the GUI, which applies them on the main thread where
/// `clap.gui` calls are legal. That is the same shape the rest of this file already uses for host
/// requests, and it is what keeps a plugin calling from its audio thread from allocating or
/// blocking in ours.
impl HostGuiImpl for HostShared {
    fn resize_hints_changed(&self) {
        // Nothing to do: the plugin owns its floating window, so its size constraints are between
        // it and the window manager. Recorded as deliberate rather than left as an empty method.
    }

    fn request_resize(&self, new_size: GuiSize) -> Result<(), HostError> {
        let packed = u64::from(new_size.width) << 32 | u64::from(new_size.height);
        self.requests.gui_resize.store(packed, Ordering::Relaxed);
        self.requests
            .gui_resize_pending
            .store(true, Ordering::Release);
        self.wake();
        Ok(())
    }

    fn request_show(&self) -> Result<(), HostError> {
        self.requests.gui_show.store(true, Ordering::Release);
        self.wake();
        Ok(())
    }

    fn request_hide(&self) -> Result<(), HostError> {
        self.requests.gui_hide.store(true, Ordering::Release);
        self.wake();
        Ok(())
    }

    fn closed(&self, was_destroyed: bool) {
        // Order matters: `was_destroyed` must be visible before `gui_closed`, because the GUI reads
        // `gui_closed` to decide whether to act at all.
        if was_destroyed {
            self.notifications
                .gui_closed_was_destroyed
                .store(true, Ordering::Relaxed);
        }
        self.notifications.gui_closed.store(true, Ordering::Release);
        self.wake();
    }
}

impl HostLogImpl for HostShared {
    fn log(&self, severity: LogSeverity, message: &str) {
        self.logs.push(severity, message);
        self.wake();
    }
}

impl HostParamsImplShared for HostShared {
    fn request_flush(&self) {
        self.requests.param_flush.store(true, Ordering::Release);
        self.wake();
    }
}

/// Main-thread host state.
pub struct HostMainThread<'a> {
    pub shared: &'a HostShared,
    /// A handle to the plugin's own main-thread side, valid from `initialized` onward.
    pub plugin: Option<InitializedPluginHandle<'a>>,
}

impl<'a> HostMainThread<'a> {
    pub fn new(shared: &'a HostShared) -> Self {
        Self {
            shared,
            plugin: None,
        }
    }
}

impl<'a> MainThreadHandler<'a> for HostMainThread<'a> {
    fn initialized(&mut self, instance: InitializedPluginHandle<'a>) {
        self.plugin = Some(instance);
    }
}

impl HostParamsImplMainThread for HostMainThread<'_> {
    fn rescan(&mut self, flags: ParamRescanFlags) {
        self.shared
            .notifications
            .param_rescan
            .fetch_or(flags.bits(), Ordering::Release);
        self.shared.wake();
    }

    fn clear(&mut self, param_id: ClapId, _flags: ParamClearFlags) {
        self.shared
            .notifications
            .param_cleared_id
            .store(param_id.get(), Ordering::Release);
        self.shared
            .notifications
            .param_cleared
            .store(true, Ordering::Release);
        self.shared.wake();
    }
}

impl HostAudioPortsImpl for HostMainThread<'_> {
    fn is_rescan_flag_supported(&self, _flag: AudioPortRescanFlags) -> bool {
        // The player reactivates from scratch on any layout change, which covers every flag —
        // including the ones that require deactivation.
        true
    }

    fn rescan(&mut self, flags: AudioPortRescanFlags) {
        if flags.requires_deactivate() {
            self.shared
                .notifications
                .audio_ports_needs_deactivate
                .store(true, Ordering::Release);
        }
        self.shared
            .notifications
            .audio_ports_changed
            .store(true, Ordering::Release);
        self.shared.wake();
    }
}

impl HostAudioPortsConfigImpl for HostMainThread<'_> {
    fn rescan(&mut self) {
        // A configuration change always means renegotiating the envelope, so it is treated
        // exactly like a layout change that requires deactivation.
        self.shared
            .notifications
            .audio_ports_needs_deactivate
            .store(true, Ordering::Release);
        self.shared
            .notifications
            .audio_ports_changed
            .store(true, Ordering::Release);
        self.shared.wake();
    }
}

impl HostNotePortsImpl for HostMainThread<'_> {
    fn supported_dialects(&self) -> NoteDialects {
        // The player speaks CLAP notes and MIDI 1. It does not claim MPE or MIDI 2, because it
        // does not translate them.
        NoteDialects::CLAP | NoteDialects::MIDI
    }

    fn rescan(&mut self, _flags: NotePortRescanFlags) {
        self.shared
            .notifications
            .note_ports_changed
            .store(true, Ordering::Release);
        self.shared.wake();
    }
}

impl HostStateImpl for HostMainThread<'_> {
    fn mark_dirty(&mut self) {
        self.shared
            .notifications
            .state_dirty
            .store(true, Ordering::Release);
        self.shared.wake();
    }
}

impl HostLatencyImpl for HostMainThread<'_> {
    fn changed(&mut self) {
        self.shared
            .notifications
            .latency_changed
            .store(true, Ordering::Release);
        self.shared.wake();
    }
}

/// Audio-thread host state.
pub struct HostAudioProcessor<'a> {
    pub shared: &'a HostShared,
}

impl<'a> HostAudioProcessor<'a> {
    pub fn new(shared: &'a HostShared) -> Self {
        Self { shared }
    }
}

impl<'a> AudioProcessorHandler<'a> for HostAudioProcessor<'a> {}

impl HostTailImpl for HostAudioProcessor<'_> {
    fn changed(&mut self) {
        // Invalidating a cached finite tail is the whole point: sleeping on a stale value would
        // truncate the tail the plugin just told us got longer.
        self.shared
            .notifications
            .tail_changed
            .store(true, Ordering::Release);
        self.shared.wake();
    }
}
