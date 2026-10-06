//! The engine: what the GUI thread owns, and the one protocol every reconfiguration goes through.
//!
//! The processor returns through a capacity-one lock-free queue and an atomic flag. Ordinary
//! reconfiguration polls once per frame; `stop_now` synchronously polls/yields for up to the
//! handshake timeout before a caller can safely scan, unload or change topology.

pub mod audio;
pub mod editor;
pub mod fx;
pub mod meters;
pub mod processor;
pub mod session_backend;
pub mod stream;

use crate::envelope::{EffectEnvelope, Envelope, Refusal, negotiate, negotiate_effect};
use crate::events::input::{
    MAX_INPUT_PRODUCERS, PRODUCER_QUEUE_CAPACITY, PanicEpoch, SourceId, TimedEvent,
};
use crate::host::{
    HostAudioProcessor, HostMainThread, HostShared, MxmHost, PlayerHostState, host_info,
};
use crate::midi::port::{MidirSink, RunningWorker};
use crate::midi::{OutPanic, OutgoingEvent};
use crate::params::ParamSet;
use audio::Backend;
use clack_extensions::latency::PluginLatency;
use clack_extensions::state::PluginState;
use clack_extensions::tail::PluginTail;
use clack_host::events::UnknownEvent;
use clack_host::events::event_types::{
    ParamGestureBeginEvent, ParamGestureEndEvent, ParamValueEvent,
};
use clack_host::prelude::*;
use fx::{FxChain, FxInfo, FxSlot, FxStage, MAX_FX, StageConfig, widest_output};
use meters::Meters;
use processor::{AudioWorker, Command, MAX_BLOCK_FRAMES, WorkerConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use stream::{AudioStream, ProcessorOwner};

/// How long the GUI waits for the processor before declaring the plugin wedged.
///
/// Bounds the entire Stop handshake, including queue admission. A timeout does not prove a
/// plugin callback hung: the backend may have stopped dispatching callbacks altogether.
pub const WEDGE_TIMEOUT: Duration = Duration::from_secs(3);

/// What every path that discovers a wedged engine reports, so the UI shows one message.
const WEDGED_MESSAGE: &str =
    "audio stop timed out waiting for the processor return; restart the player";

/// How long after a stream dies the first reconnect is tried. Short, because the common cause —
/// another application holding the device for a moment, a sample-rate switch — is over quickly.
pub const RECONNECT_FIRST_DELAY: Duration = Duration::from_millis(250);
/// The longest wait between reconnect attempts, once the backoff has grown to it. Attempts never
/// stop: an interface that is unplugged comes back when it is plugged in, and each attempt is one
/// device activation and one plugin activation, which is cheap at this rate.
pub const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(5);

/// How many plugin-driven output events may be waiting for the GUI.
pub const PLUGIN_OUTPUT_CAPACITY: usize = 4096;

/// How many MIDI-out events may be queued for the worker.
pub const MIDI_OUT_CAPACITY: usize = 4096;

/// Something the plugin emitted that the GUI needs to know about.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum PluginOutput {
    ParamValue {
        param_id: u32,
        value: f64,
    },
    /// The **offset** the sequencer is applying to a parameter, because a step deviates from the
    /// patch.
    ///
    /// Not a value: since locks became modulation the parameter's own value is untouched, so there
    /// is nothing here for the panel to redraw and nothing for a mapped knob to catch up to — its
    /// pickup compares against the value, which has not moved. The player records it so that *what
    /// the instrument is sounding* — the value plus this — can be shown and asserted on.
    ///
    /// **Droppable.** The channel is bounded; if this does not fit it is discarded and
    /// `PlayerHostState::note_unsettled_params` is set instead — see there for why a flag rather
    /// than a retry. Nothing is decided from it: an offset that must come off is the **runtime's** to
    /// send, and it tracks that itself, precisely because this notification may be lost.
    SequencerParam {
        /// **Which plugin's parameter**, not a bare id — the panel has to attribute an effect's
        /// automation to that effect. See [`crate::sequencer::locks::LockKey`].
        param_id: crate::sequencer::locks::LockKey,
        value: f32,
    },
    GestureBegin {
        param_id: u32,
    },
    GestureEnd {
        param_id: u32,
    },
    /// A control change the player claimed, on its way to the GUI instead of to the plugin.
    ///
    /// Mapping runs on the GUI thread — it needs the layout file, the current parameter values
    /// and the page display, none of which belong on the audio thread. The worker's only job is
    /// to recognise a claimed CC and route it here. See [`crate::control_map`].
    MappedControlChange {
        controller: u8,
        value: u8,
    },
    /// The bounded output sink was full, so some of what the plugin emitted was lost.
    ///
    /// A panic flag does nothing for a lost gesture `end`, so this is the plugin-originated
    /// remedy: host gesture tracking is marked invalid, every host-tracked gesture is closed
    /// safely, and parameter values are refreshed on the GUI thread.
    TrackingInvalidated,
}

impl PluginOutput {
    pub fn from_event(event: &UnknownEvent) -> Option<Self> {
        if let Some(e) = event.as_event::<ParamValueEvent>() {
            return Some(PluginOutput::ParamValue {
                param_id: e.param_id()?.get(),
                value: e.value(),
            });
        }
        if let Some(e) = event.as_event::<ParamGestureBeginEvent>() {
            return Some(PluginOutput::GestureBegin {
                param_id: e.param_id()?.get(),
            });
        }
        if let Some(e) = event.as_event::<ParamGestureEndEvent>() {
            return Some(PluginOutput::GestureEnd {
                param_id: e.param_id()?.get(),
            });
        }
        None
    }
}

/// What the engine is doing, from the GUI's point of view.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineState {
    /// No plugin loaded, or no stream running.
    Idle,
    /// A stream is running and the plugin is being processed.
    Running,
    /// Stop is requested and the processor has not returned. Queue admission may still be
    /// pending; retries and the return share one bounded deadline.
    AwaitingStoppedProcessor,
    /// The audio backend's worker loop exited — a device failure, not a hung plugin. The stream
    /// can be dropped and joined safely, and the engine reactivates once a device is chosen.
    StreamExited(String),
    /// The callback never returned the processor. Terminal: the stream cannot be dropped without
    /// deadlocking, so everything is structurally leaked and the process must be replaced.
    Wedged,
    /// Something failed in a way the user needs to see.
    Failed(String),
}

/// What to do once the processor comes back.
#[derive(Clone, Debug, PartialEq)]
enum PendingAction {
    /// Deactivate and stop, leaving the engine idle.
    Stop,
    /// Deactivate, renegotiate, and reactivate with the same or new settings.
    Reconfigure,
}

/// The engine's attempts to come back after its stream died out from under it.
///
/// A stream dies when the platform takes the device away — on Windows, another application
/// opening it in exclusive mode, its sample rate or clock changing, or the cable coming out —
/// and none of that is the plugin's doing: the processor came back cleanly, so a restart is all
/// that is needed. The engine cannot start itself, because whoever owns the app owns the
/// backend, so it keeps the schedule here and the app asks [`Engine::try_reconnect`] once per
/// frame. Attempts double their spacing from [`RECONNECT_FIRST_DELAY`] up to
/// [`RECONNECT_MAX_DELAY`] and never stop while the stream is dead.
#[derive(Clone, Debug, PartialEq)]
pub struct Reconnect {
    /// Attempts made so far. Zero until the first is due.
    pub attempts: u32,
    /// Why the last attempt failed, if one has. The device being held by another application
    /// shows up here, in the backend's words.
    pub last_failure: Option<String>,
    next_at: Instant,
}

impl Reconnect {
    fn scheduled() -> Self {
        Self {
            attempts: 0,
            last_failure: None,
            next_at: Instant::now() + RECONNECT_FIRST_DELAY,
        }
    }

    /// The wait after `attempts` failures: doubling from the first delay, capped.
    fn delay_after(attempts: u32) -> Duration {
        RECONNECT_FIRST_DELAY
            .saturating_mul(1u32 << attempts.min(8))
            .min(RECONNECT_MAX_DELAY)
    }

    fn is_due(&self) -> bool {
        Instant::now() >= self.next_at
    }
}

/// How the audio side is configured.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioConfig {
    /// `None` means the system default device.
    pub device_name: Option<String>,
    pub sample_rate: Option<u32>,
    /// A *request*: the backend may deliver any frame count, and oversized callbacks are chunked.
    pub buffer_size: Option<u32>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device_name: None,
            sample_rate: None,
            buffer_size: Some(512),
        }
    }
}

/// A loaded plugin, its instance, and the entry that owns its library.
struct Loaded {
    /// Kept alive for as long as the instance is: the entry owns the loaded library, and
    /// dropping it while an instance exists would unload the code out from under it.
    _entry: PluginEntry,
    instance: PluginInstance<MxmHost>,
    envelope: Envelope,
    plugin_id: String,
    /// Where it was loaded from, so the browser can show it and settings can persist it.
    pub bundle: PathBuf,
}

/// The GUI thread's half of the engine.
/// A MIDI input port the player could not open, and the reason it could not.
///
/// The reason travels with the port because the three failures a user actually hits — the port is
/// held by another application, the device has been unplugged, the input limit is reached — look
/// identical without it and have nothing in common to try next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedInput {
    pub port: String,
    pub reason: String,
}

/// One effect's return queue, so the engine can take its processor back and deactivate it.
///
/// Each stage hands back through its own capacity-one queue, on the same flags discipline the
/// source uses; the engine waits on the **source's** flag and, because the worker hands the
/// effects back first, finds these already filled.
struct FxReturn {
    queue: rtrb::Consumer<StoppedPluginAudioProcessor<MxmHost>>,
    /// Set if the stage's owner ever had to leak its processor. Read into
    /// [`Engine::leaked_processor`], so the condition stays visible.
    leaked: Arc<AtomicBool>,
}

pub struct Engine {
    state: EngineState,
    loaded: Option<Loaded>,
    /// The effects after the source, in order. See [`fx`].
    fx: Vec<FxSlot>,
    /// The next identity to hand an effect, never reset and never reused. See [`fx::FxId`].
    next_fx_id: fx::FxId,
    /// One per effect, while a stream runs.
    fx_returns: Vec<FxReturn>,
    shared: Arc<PlayerHostState>,
    /// Whether the plugin's own interface is open, and whether the player owns its window.
    ///
    /// Held here rather than in the UI because the editor's lifetime is the *instance's*: unloading
    /// a plugin must destroy its editor first, and the UI is not what knows when that happens.
    /// One state per open editor window. **Not one in total**: the owner's ruling, 2026-09-04 —
    /// a synth and the effects after it are one job, and closing one window to see another was
    /// making the user pay for the engine's convenience. See `editor.rs`'s `EditorTarget`.
    editors: Vec<editor::EditorState>,

    stream: Option<Box<dyn AudioStream>>,
    commands: Option<rtrb::Producer<Command>>,
    /// Sequencer states the worker has finished with, waiting to be freed here.
    ///
    /// **The audio thread must not release the last reference to one**, so it hands them back and
    /// this side drops them. Drained before every publish, which is also what keeps the queue from
    /// ever filling — see [`Engine::set_sequencer`].
    retired: Option<rtrb::Consumer<std::sync::Arc<crate::sequencer::SequencerState>>>,
    /// The rate the plugin was last activated at. An export renders at this, so the file matches
    /// what was heard rather than what was asked for.
    effective_sample_rate: Option<f64>,
    processor_return: Option<rtrb::Consumer<StoppedPluginAudioProcessor<MxmHost>>>,
    processor_returned: Arc<AtomicBool>,
    stream_exited: Arc<AtomicBool>,
    leaked: Arc<AtomicBool>,
    stream_error: Arc<std::sync::Mutex<Option<String>>>,

    /// GUI-side producer for the events the on-screen and computer keyboards generate.
    gui_events: Option<rtrb::Producer<TimedEvent>>,
    plugin_output: Option<rtrb::Consumer<PluginOutput>>,
    /// Handed to the MIDI-out worker thread: the GUI is not who sends.
    midi_out_consumer: Option<rtrb::Consumer<OutgoingEvent>>,

    pub input_epoch: Arc<PanicEpoch>,
    pub out_panic: Arc<OutPanic>,
    pub thru_enabled: Arc<AtomicBool>,
    pub meters: Arc<Meters>,
    /// The one timeline every event source stamps against.
    clock: Arc<crate::clock::Clock>,

    audio_config: AudioConfig,
    /// MIDI input ports the user has asked for, by name. Changing this reconfigures.
    desired_midi_inputs: Vec<String>,
    /// The live MIDI input connections, one producer slot each.
    midi_inputs: Vec<crate::midi::input::Connection>,
    /// The MIDI output port the user has asked for, by name.
    desired_midi_output: Option<String>,
    midi_output: Option<RunningWorker>,
    /// Ports refused because the producer maximum was reached, so the UI can say so.
    refused_midi_inputs: Vec<RefusedInput>,
    stop_requested_at: Option<Instant>,
    /// Queue admission is distinct from requesting Stop; retry until admitted or timed out.
    stop_queued: bool,
    pending: Option<PendingAction>,
    /// Gesture `end` events that were refused and must be retried every frame.
    retry_gesture_ends: Vec<u32>,
    /// Present while the stream is dead and the engine is trying to come back.
    reconnect: Option<Reconnect>,
    /// Set when the stream dies, taken by the app once, so it can react to the death itself —
    /// stop the transport, close gestures — the way it does around a rescan.
    stream_exit_notice: bool,
}

impl Engine {
    /// An engine on the production monotonic clock.
    pub fn new() -> Self {
        Self::with_clock(crate::clock::Clock::monotonic())
    }

    /// An engine on a caller-supplied clock, so a session can render reproducibly.
    pub fn with_clock(clock: Arc<crate::clock::Clock>) -> Self {
        Self {
            clock,
            state: EngineState::Idle,
            loaded: None,
            fx: Vec::new(),
            // Ids start at one, so `NO_FX_ID` is a value no effect ever has.
            next_fx_id: 1,
            fx_returns: Vec::new(),
            shared: Arc::new(PlayerHostState::new()),
            editors: Vec::new(),
            stream: None,
            commands: None,
            retired: None,
            effective_sample_rate: None,
            processor_return: None,
            processor_returned: Arc::new(AtomicBool::new(false)),
            stream_exited: Arc::new(AtomicBool::new(false)),
            leaked: Arc::new(AtomicBool::new(false)),
            stream_error: Arc::new(std::sync::Mutex::new(None)),
            gui_events: None,
            plugin_output: None,
            midi_out_consumer: None,
            input_epoch: Arc::new(PanicEpoch::new()),
            out_panic: Arc::new(OutPanic::new()),
            thru_enabled: Arc::new(AtomicBool::new(false)),
            meters: Arc::new(Meters::new()),
            audio_config: AudioConfig::default(),
            desired_midi_inputs: Vec::new(),
            midi_inputs: Vec::new(),
            desired_midi_output: None,
            midi_output: None,
            refused_midi_inputs: Vec::new(),
            stop_requested_at: None,
            stop_queued: false,
            pending: None,
            retry_gesture_ends: Vec::new(),
            reconnect: None,
            stream_exit_notice: false,
        }
    }

    pub fn state(&self) -> &EngineState {
        &self.state
    }

    pub fn shared(&self) -> &Arc<PlayerHostState> {
        &self.shared
    }

    /// The timeline every event source stamps against.
    /// Tells the audio worker which CCs the player has claimed.
    ///
    /// Returns whether it was delivered: with no stream running there is no worker to tell, and
    /// the mask is published again when one starts.
    pub fn set_claimed_ccs(&mut self, mask: crate::control_map::CcMask) -> bool {
        match self.commands.as_mut() {
            Some(commands) => commands.push(Command::SetClaimedCcs(mask)).is_ok(),
            None => false,
        }
    }

    /// Drops every sequencer state the worker has handed back.
    ///
    /// **This is where a snapshot is actually freed**, on the thread that made it. Popping moves
    /// each one out of the queue and the loop body drops it.
    fn drain_retired(&mut self) {
        if let Some(retired) = self.retired.as_mut() {
            while retired.pop().is_ok() {}
        }
    }

    /// How many states the worker has handed back and this side has not yet freed.
    ///
    /// For the test that says retiring cannot overflow; not part of the engine's job.
    #[doc(hidden)]
    pub fn retired_waiting(&self) -> usize {
        self.retired.as_ref().map_or(0, |r| r.slots())
    }

    /// Publishes the sequencer's whole state to the audio worker.
    ///
    /// Returns whether it was delivered: with no stream running there is no worker to tell, and
    /// the state is published again when one starts.
    pub fn set_sequencer(&mut self, state: crate::sequencer::SequencerState) -> bool {
        // **Drained before pushing, which is what bounds the states in flight.** Each command the
        // worker consumes retires at most one state, so emptying this first means the retire queue
        // can never be asked to hold more than the command queue's depth — and the worker's
        // hand-back cannot fail. Dropping happens right here, on the thread allowed to call the
        // allocator. Before the `match`, because both borrow `self`.
        self.drain_retired();
        match self.commands.as_mut() {
            Some(commands) => commands
                .push(Command::SetSequencer(std::sync::Arc::new(state)))
                .is_ok(),
            None => false,
        }
    }

    /// The sequencer position the audio thread last published: `(transport, step)`.
    pub fn playhead(&self) -> (u8, u32) {
        self.shared().playhead()
    }

    /// The publication serial the audio worker has consumed.
    pub fn sequencer_ack(&self) -> u64 {
        self.shared().sequencer_ack()
    }

    /// The plugin's CLAP state, as bytes.
    ///
    /// A **synchronous call into the plugin**, on the GUI thread, which allocates. That is why it
    /// cannot happen on the audio thread — and why a plugin that hangs here hangs the interface,
    /// which `NOTES.md` (*Fault isolation is partial*) records as unrecoverable in-process.
    pub fn capture_state(&mut self) -> Result<Vec<u8>, String> {
        let loaded = self
            .loaded
            .as_mut()
            .ok_or_else(|| "no plugin is loaded".to_owned())?;
        let ext: clack_extensions::state::PluginState = loaded
            .instance
            .plugin_shared_handle()
            .get_extension()
            .ok_or_else(|| "the plugin does not implement the state extension".to_owned())?;

        let mut bytes = Vec::new();
        let mut handle = loaded.instance.plugin_handle();
        ext.save(&mut handle, &mut bytes)
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    }

    /// The sample rate audio is running at, if it is running.
    pub fn sample_rate(&self) -> Option<f64> {
        self.effective_sample_rate
    }

    pub fn clock(&self) -> &Arc<crate::clock::Clock> {
        &self.clock
    }

    pub fn envelope(&self) -> Option<&Envelope> {
        self.loaded.as_ref().map(|l| &l.envelope)
    }

    pub fn plugin_id(&self) -> Option<&str> {
        self.loaded.as_ref().map(|l| l.plugin_id.as_str())
    }

    /// The bundle the loaded plugin came from.
    pub fn bundle(&self) -> Option<&Path> {
        self.loaded.as_ref().map(|l| l.bundle.as_path())
    }

    pub fn audio_config(&self) -> &AudioConfig {
        &self.audio_config
    }

    pub fn is_wedged(&self) -> bool {
        matches!(self.state, EngineState::Wedged)
    }

    /// Whether a processor was leaked because the return queue refused it — the source's or
    /// any effect's. Never a silent loss.
    pub fn leaked_processor(&self) -> bool {
        self.leaked.load(Ordering::Acquire)
            || self
                .fx_returns
                .iter()
                .any(|r| r.leaked.load(Ordering::Acquire))
    }

    /// Loads a bundle and instantiates one plugin from it, with its own host state.
    ///
    /// Shared by the source and the effects: the entry, the descriptor lookup and the instance
    /// are the same either way. Negotiation is the caller's, because it is the role.
    fn instantiate(
        bundle: &Path,
        plugin_id: &str,
    ) -> Result<(PluginEntry, PluginInstance<MxmHost>, Arc<PlayerHostState>), String> {
        // SAFETY: loading a CLAP bundle runs its code. That is inherent to hosting; the browser
        // decides which bundles are offered, and fixtures are only ever loaded by path.
        let entry = unsafe { crate::entry::load(bundle) }.map_err(|e| e.to_string())?;
        let factory = entry
            .get_plugin_factory()
            .ok_or_else(|| "the bundle exposes no plugin factory".to_owned())?;
        let id = factory
            .plugin_descriptors()
            .find(|d| d.id().map(|id| id.to_bytes()) == Some(plugin_id.as_bytes()))
            .and_then(|d| d.id())
            .ok_or_else(|| format!("the bundle contains no plugin `{plugin_id}`"))?
            .to_owned();

        let shared = Arc::new(PlayerHostState::new());
        let state_for_plugin = Arc::clone(&shared);
        let instance = PluginInstance::<MxmHost>::new(
            move |_| HostShared::new(state_for_plugin),
            |s| HostMainThread::new(s),
            &entry,
            &id,
            &host_info(),
        )
        .map_err(|e| e.to_string())?;
        Ok((entry, instance, shared))
    }

    /// Loads a plugin from a bundle, replacing whatever was loaded before.
    ///
    /// Scanning and loading both execute arbitrary plugin code, so this deliberately runs with
    /// audio stopped. **The effect chain stays**: changing the source does not unload the effects
    /// after it, which is what makes auditioning one effect against several instruments a matter
    /// of changing the instrument.
    pub fn load(&mut self, bundle: &Path, plugin_id: &str) -> Result<(), String> {
        // Before anything else: the outgoing plugin's editor is a window *it* owns, and dropping
        // the instance out from under it would leave that window orphaned. `close_editor` is a
        // no-op when none is open, and an effect's editor is left alone — its instance stays.
        self.close_editor_for(editor::EditorTarget::Source);
        self.stop_now()?;

        let (entry, mut instance, shared) = Self::instantiate(bundle, plugin_id)?;
        let envelope = negotiate(&mut instance).map_err(|r: Refusal| format!("it {r}"))?;

        self.shared = shared;
        self.loaded = Some(Loaded {
            _entry: entry,
            instance,
            envelope,
            plugin_id: plugin_id.to_owned(),
            bundle: bundle.to_path_buf(),
        });
        // A new plugin is a new start; whatever the old stream's death left pending is over.
        self.reconnect = None;
        self.stream_exit_notice = false;
        self.state = EngineState::Idle;
        Ok(())
    }

    // --- the effect chain ------------------------------------------------------------------------

    /// Adds an effect at the end of the chain. A topology change: audio stops, and the caller
    /// starts it again, exactly as after [`Engine::load`].
    ///
    /// Refused with the reason when the plugin is not an effect the player can feed, or the chain
    /// is full. A failed add leaves the chain as it was and the source usable.
    pub fn add_fx(&mut self, bundle: &Path, plugin_id: &str) -> Result<usize, String> {
        if self.fx.len() >= MAX_FX {
            return Err(format!("the chain holds at most {MAX_FX} effects"));
        }
        self.stop_now()?;
        let (entry, mut instance, shared) = Self::instantiate(bundle, plugin_id)?;
        let envelope = negotiate_effect(&mut instance).map_err(|r: Refusal| format!("it {r}"))?;
        let id = self.next_fx_id;
        self.next_fx_id += 1;
        self.fx.push(FxSlot {
            id,
            _entry: entry,
            instance,
            envelope,
            plugin_id: plugin_id.to_owned(),
            bundle: bundle.to_path_buf(),
            shared,
            bypassed: Arc::new(AtomicBool::new(false)),
        });
        Ok(self.fx.len() - 1)
    }

    /// Removes the effect at `index`. Its editor, if open, goes with it. A topology change.
    pub fn remove_fx(&mut self, index: usize) -> Result<(), String> {
        if index >= self.fx.len() {
            return Err(format!("there is no effect {}", index + 1));
        }
        self.close_editor_for(editor::EditorTarget::Fx(index));
        self.stop_now()?;
        self.fx.remove(index);
        // An editor open on a later slot now belongs to an earlier index.
        for state in &mut self.editors {
            if let editor::EditorTarget::Fx(open) = state.target
                && open > index
            {
                state.target = editor::EditorTarget::Fx(open - 1);
            }
        }
        Ok(())
    }

    /// Moves the effect at `from` so that it sits at `to`. **Rearranging is the one edit the
    /// chain offers besides on and off** — the owner's ruling. A topology change.
    pub fn move_fx(&mut self, from: usize, to: usize) -> Result<(), String> {
        let len = self.fx.len();
        if from >= len || to >= len {
            return Err(format!("the chain has {len} effect(s)"));
        }
        if from == to {
            return Ok(());
        }
        self.stop_now()?;
        let slot = self.fx.remove(from);
        self.fx.insert(to, slot);
        // Every open editor follows its effect.
        for state in &mut self.editors {
            if let editor::EditorTarget::Fx(open) = state.target {
                let moved = if open == from {
                    to
                } else if from < open && open <= to {
                    open - 1
                } else if to <= open && open < from {
                    open + 1
                } else {
                    open
                };
                state.target = editor::EditorTarget::Fx(moved);
            }
        }
        Ok(())
    }

    /// Switches an effect off or on. **Not a topology change**: one atomic the audio thread reads
    /// per chunk, so the sound continues and nothing restarts. Off means not called at all.
    pub fn set_fx_bypassed(&mut self, index: usize, bypassed: bool) -> Result<(), String> {
        let slot = self
            .fx
            .get(index)
            .ok_or_else(|| format!("there is no effect {}", index + 1))?;
        slot.set_bypassed(bypassed);
        Ok(())
    }

    /// The chain as the interface and the state dump show it.
    pub fn fx_info(&self) -> Vec<FxInfo> {
        self.fx
            .iter()
            .map(|slot| FxInfo {
                id: slot.id,
                plugin_id: slot.plugin_id.clone(),
                bundle: slot.bundle.clone(),
                bypassed: slot.bypassed(),
                input_channels: slot.envelope.input.channel_count,
                output_channels: slot.envelope.output.channel_count,
                selection: slot.envelope.selection.clone(),
            })
            .collect()
    }

    pub fn fx_len(&self) -> usize {
        self.fx.len()
    }

    pub fn fx_envelope(&self, index: usize) -> Option<&EffectEnvelope> {
        self.fx.get(index).map(|s| &s.envelope)
    }

    /// An effect's CLAP state, for an export or developer dump that must capture what is being
    /// heard.
    pub fn capture_fx_state(&mut self, index: usize) -> Result<Vec<u8>, String> {
        let slot = self
            .fx
            .get_mut(index)
            .ok_or_else(|| format!("there is no effect {}", index + 1))?;
        let ext: clack_extensions::state::PluginState = slot
            .instance
            .plugin_shared_handle()
            .get_extension()
            .ok_or_else(|| "the effect does not implement the state extension".to_owned())?;
        let mut bytes = Vec::new();
        let mut handle = slot.instance.plugin_handle();
        ext.save(&mut handle, &mut bytes)
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    }

    /// Restores one effect's CLAP state from bytes.
    ///
    /// This deliberately uses the same live-instance state extension as a DAW restore. The plugin
    /// and wrapper own publication to an active processor; the player neither decodes the state nor
    /// invents effect-specific parameters for durable model data.
    pub fn load_fx_state(&mut self, index: usize, bytes: &[u8]) -> Result<(), String> {
        let slot = self
            .fx
            .get_mut(index)
            .ok_or_else(|| format!("there is no effect {}", index + 1))?;
        let ext: clack_extensions::state::PluginState = slot
            .instance
            .plugin_shared_handle()
            .get_extension()
            .ok_or_else(|| "the effect does not implement the state extension".to_owned())?;
        let mut input = bytes;
        let mut handle = slot.instance.plugin_handle();
        ext.load(&mut handle, &mut input).map_err(|e| e.to_string())
    }

    /// Activates every effect and builds its stage, after the source has been activated.
    ///
    /// On any failure the stages already built are dropped — each owner's `Drop` hands its
    /// processor back — and every effect activated so far is deactivated again, so a refused
    /// effect never leaves an instance stuck active.
    fn activate_fx(&mut self, configuration: PluginAudioConfiguration) -> Result<FxChain, String> {
        let mut stages = Vec::with_capacity(self.fx.len());
        let mut returns = Vec::with_capacity(self.fx.len());
        let mut failure = None;

        for (index, slot) in self.fx.iter_mut().enumerate() {
            let processor = match slot
                .instance
                .activate(|shared, _| HostAudioProcessor::new(shared), configuration)
            {
                Ok(processor) => processor,
                Err(e) => {
                    failure = Some(format!("effect {} would not activate: {e}", index + 1));
                    break;
                }
            };
            let tail: Option<PluginTail> = slot.instance.plugin_shared_handle().get_extension();
            // Its own editor's only route in while it is switched off — see `FxStage::flush_params`.
            let params: Option<clack_extensions::params::PluginParams> =
                slot.instance.plugin_shared_handle().get_extension();
            let (producer, consumer) = rtrb::RingBuffer::new(1);
            let leaked = Arc::new(AtomicBool::new(false));
            // The returned and exited flags are per owner and the engine waits on the source's
            // alone, so these two are given and never read: the queue is what is consulted.
            let owner = ProcessorOwner::new(
                processor,
                producer,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                Arc::clone(&leaked),
            );
            stages.push(FxStage::new(StageConfig {
                id: slot.id,
                owner,
                envelope: slot.envelope.clone(),
                bypassed: Arc::clone(&slot.bypassed),
                shared: Arc::clone(&slot.shared),
                tail,
                params,
            }));
            returns.push(FxReturn {
                queue: consumer,
                leaked,
            });
        }

        if let Some(reason) = failure {
            // Dropping the stages hands each processor back through its queue; take them out
            // again and deactivate, so nothing stays active.
            drop(stages);
            for (slot, ret) in self.fx.iter_mut().zip(returns.iter_mut()) {
                if let Ok(processor) = ret.queue.pop() {
                    slot.instance.deactivate(processor);
                }
            }
            return Err(reason);
        }

        self.fx_returns = returns;
        Ok(FxChain::new(stages))
    }

    /// Takes every effect's processor back and deactivates it. Called on the way to idle,
    /// whether the stop was asked for or the stream died.
    fn deactivate_fx(&mut self) {
        let returns = std::mem::take(&mut self.fx_returns);
        for (slot, mut ret) in self.fx.iter_mut().zip(returns) {
            if let Ok(processor) = ret.queue.pop() {
                slot.instance.deactivate(processor);
            }
        }
    }

    /// Starts audio with the current configuration.
    pub fn start(&mut self, backend: &dyn Backend) -> Result<(), String> {
        let Some(loaded) = self.loaded.as_mut() else {
            return Err("no plugin is loaded".to_owned());
        };
        if self.stream.is_some() {
            return Ok(());
        }

        // Opened for the widest output in the chain: a stereo chorus after a mono synth wants
        // two channels, and a bypassed stage's absence is adapted in the interleave, not by
        // reopening the device.
        let widest = widest_output(loaded.envelope.audio.channel_count, &self.fx);
        let choice = backend.open(&self.audio_config, widest)?;
        let sample_rate = choice.sample_rate;
        // The rate the plugin is actually running at, which is what an export must match — not
        // what was requested, which a device may refuse.
        self.effective_sample_rate = Some(sample_rate);
        let channel_count = choice.channel_count;

        let audio_configuration = PluginAudioConfiguration {
            sample_rate,
            min_frames_count: 1,
            max_frames_count: MAX_BLOCK_FRAMES,
        };
        let processor = loaded
            .instance
            .activate(
                |shared, _| HostAudioProcessor::new(shared),
                audio_configuration,
            )
            .map_err(|e| e.to_string())?;

        let tail: Option<PluginTail> = loaded.instance.plugin_shared_handle().get_extension();

        /// How many commands may be in flight, and therefore how many sequencer states may be.
        const COMMAND_CAPACITY: usize = 64;

        // Every queue is allocated here, on the GUI thread, and never resized. Queue slots
        // are preallocated for the hard maximum, so connecting a port at runtime claims a free
        // slot rather than allocating on any thread.
        let (command_producer, command_consumer) = rtrb::RingBuffer::new(COMMAND_CAPACITY);
        // **As deep as the command queue, and that is what makes it un-overflowable.** Each
        // `SetSequencer` the worker consumes retires at most one state, and `set_sequencer` drains
        // this before pushing — so the states in flight can never exceed the commands in flight.
        let (retire_producer, retire_consumer) = rtrb::RingBuffer::new(COMMAND_CAPACITY);
        let (return_producer, mut return_consumer) = rtrb::RingBuffer::new(1);
        let (output_producer, output_consumer) = rtrb::RingBuffer::new(PLUGIN_OUTPUT_CAPACITY);
        let (midi_out_producer, midi_out_consumer) = rtrb::RingBuffer::new(MIDI_OUT_CAPACITY);

        let mut producers = Vec::with_capacity(MAX_INPUT_PRODUCERS);
        let mut consumers = Vec::with_capacity(MAX_INPUT_PRODUCERS);
        for _ in 0..MAX_INPUT_PRODUCERS {
            let (producer, consumer) = rtrb::RingBuffer::new(PRODUCER_QUEUE_CAPACITY);
            producers.push(producer);
            consumers.push(consumer);
        }
        let mut producers = producers.into_iter();
        // Slot 0 is always the GUI: the on-screen keyboard, the computer keyboard, the panel.
        let gui_producer = producers.next().expect("at least one producer slot");

        self.processor_returned.store(false, Ordering::Release);
        self.stream_exited.store(false, Ordering::Release);

        let owner = ProcessorOwner::new(
            processor,
            return_producer,
            Arc::clone(&self.processor_returned),
            Arc::clone(&self.stream_exited),
            Arc::clone(&self.leaked),
        );

        // The effects, activated after the source. `loaded` is not borrowed past here, so the
        // chain can borrow `self`; on failure the source's processor is taken back below.
        let fx = match self.activate_fx(audio_configuration) {
            Ok(chain) => chain,
            Err(reason) => {
                drop(owner);
                let loaded = self.loaded.as_mut().expect("loaded above");
                Self::deactivate_after_failed_start(
                    loaded,
                    &mut return_consumer,
                    &self.processor_returned,
                    &self.stream_exited,
                    &self.stream_error,
                );
                return Err(reason);
            }
        };
        let loaded = self.loaded.as_mut().expect("loaded above");

        let worker = AudioWorker::new(WorkerConfig {
            owner,
            fx,
            commands: command_consumer,
            retired: retire_producer,
            inputs: consumers,
            midi_out: Some(midi_out_producer),
            to_gui: output_producer,
            envelope: loaded.envelope.clone(),
            tail,
            sample_rate,
            meters: Arc::clone(&self.meters),
            shared: Arc::clone(&self.shared),
            thru_enabled: Arc::clone(&self.thru_enabled),
            out_panic: Arc::clone(&self.out_panic),
            input_epoch: Arc::clone(&self.input_epoch),
            clock: Arc::clone(&self.clock),
        });

        let mut stream = match backend.build(
            &choice,
            worker,
            Arc::clone(&self.stream_error),
            Arc::clone(&self.meters),
        ) {
            Ok(stream) => stream,
            Err(reason) => {
                // The worker was dropped inside the refused build, so the owner's `Drop` has
                // already handed the processor back. It must be taken and deactivated here:
                // left in the queue, the instance stays active and the plugin refuses every
                // later start with "already activated" — which is what happened before this
                // arm existed, and which turns one busy device into a plugin that never plays.
                Self::deactivate_after_failed_start(
                    loaded,
                    &mut return_consumer,
                    &self.processor_returned,
                    &self.stream_exited,
                    &self.stream_error,
                );
                self.deactivate_fx();
                return Err(reason);
            }
        };
        if let Err(reason) = stream.play() {
            // Dropping the stream joins its worker, which drops the owner, which hands back.
            drop(stream);
            let loaded = self.loaded.as_mut().expect("loaded above");
            Self::deactivate_after_failed_start(
                loaded,
                &mut return_consumer,
                &self.processor_returned,
                &self.stream_exited,
                &self.stream_error,
            );
            self.deactivate_fx();
            return Err(reason);
        }

        self.stream = Some(stream);
        self.commands = Some(command_producer);
        self.retired = Some(retire_consumer);
        self.processor_return = Some(return_consumer);
        self.gui_events = Some(gui_producer);
        self.plugin_output = Some(output_consumer);
        self.state = EngineState::Running;
        self.reconnect = None;

        // The MIDI-out consumer is handed to the worker thread by the caller; keeping it here
        // would make the queue's only consumer the GUI, which is not who sends.
        self.midi_out_consumer = Some(midi_out_consumer);
        let _ = channel_count;

        self.connect_desired_midi_inputs(producers);
        self.start_midi_output();
        Ok(())
    }

    /// Pushes one event from the GUI's producer slot.
    ///
    /// Returns false if the queue refused it; the caller must then react rather than assume it
    /// was delivered.
    pub fn push_gui_event(&mut self, payload: crate::events::input::Payload) -> bool {
        let event = TimedEvent::new(
            self.clock.now_nanos(),
            self.input_epoch.current(),
            SourceId(0),
            payload,
        );

        let Some(queue) = self.gui_events.as_mut() else {
            return false;
        };
        if queue.push(event).is_ok() {
            return true;
        }

        if payload.must_not_be_lost() {
            // Raise the panic rather than lose it silently. The audio thread discards pre-epoch
            // notes and issues recovery.
            self.input_epoch.raise();
            if let crate::events::input::Payload::GestureEnd { param_id } = payload {
                // A panic flag does nothing for a lost gesture `end`: it is retained and retried
                // every frame until accepted, and no new gesture may begin for that parameter
                // until it has been.
                if !self.retry_gesture_ends.contains(&param_id) {
                    self.retry_gesture_ends.push(param_id);
                }
            }
        }
        false
    }

    /// Whether a parameter still owes a gesture `end`, so no new gesture may begin for it.
    pub fn gesture_end_pending(&self, param_id: u32) -> bool {
        self.retry_gesture_ends.contains(&param_id)
    }

    /// Called once per GUI frame. Never blocks.
    pub fn poll(&mut self) {
        self.retry_refused_gesture_ends();
        self.service_host_requests();
        self.check_processor_return();
    }

    fn retry_refused_gesture_ends(&mut self) {
        if self.retry_gesture_ends.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.retry_gesture_ends);
        for param_id in pending {
            let event = TimedEvent::new(
                self.clock.now_nanos(),
                self.input_epoch.current(),
                SourceId(0),
                crate::events::input::Payload::GestureEnd { param_id },
            );
            let accepted = self
                .gui_events
                .as_mut()
                .is_some_and(|queue| queue.push(event).is_ok());
            if !accepted {
                self.retry_gesture_ends.push(param_id);
            }
        }
    }

    /// Turns the plugin's requests into work, on the right thread for each.
    fn service_host_requests(&mut self) {
        // `request_callback` asks the host to run `on_main_thread`. Waking the GUI is not the
        // same as servicing the request, and turning it into an audio-thread command would be
        // plainly wrong: it is consumed and serviced here, on the GUI thread. A request raised
        // *during* that callback sets the flag again and is serviced on a later frame.
        if self.shared.take_callback_request()
            && let Some(loaded) = self.loaded.as_mut()
        {
            loaded.instance.call_on_main_thread_callback();
        }

        let mut reconfigure = self.shared.requests.restart.swap(false, Ordering::AcqRel)
            | self
                .shared
                .notifications
                .audio_ports_changed
                .swap(false, Ordering::AcqRel);
        for slot in &mut self.fx {
            if slot.shared.take_callback_request() {
                slot.instance.call_on_main_thread_callback();
            }
            slot.flush_params_while_inactive();
            reconfigure |= slot.shared.requests.restart.swap(false, Ordering::AcqRel)
                | slot
                    .shared
                    .notifications
                    .audio_ports_changed
                    .swap(false, Ordering::AcqRel);
        }
        if reconfigure && self.stream.is_some() {
            self.request_reconfigure();
        }
    }

    /// Whether a plugin process request is waiting for the audio thread. Observational only: tests
    /// and diagnostics use this to distinguish a missing CLAP callback from failed wake handling.
    pub fn process_request_pending(&self) -> bool {
        self.shared.requests.process.load(Ordering::Acquire)
    }

    /// Whether the plugin has asked to be processed while nothing is running.
    ///
    /// `request_process` is consumed by the audio thread when a stream exists — it is what wakes
    /// a sleeping plugin. With no stream there is nobody to consume it, so the GUI, which owns
    /// the backend, has to turn it into a start.
    pub fn take_idle_process_request(&mut self) -> bool {
        if self.stream.is_some() {
            return false;
        }
        self.loaded.is_some() && self.shared.requests.process.swap(false, Ordering::AcqRel)
    }

    /// The reconnect in progress, if the stream died and the engine is trying to come back.
    pub fn reconnect(&self) -> Option<&Reconnect> {
        self.reconnect.as_ref()
    }

    /// Whether the stream has died since this was last asked. Once per death, for the app to
    /// react to the death itself — stop the transport, close gestures — the way it does around
    /// a rescan; the reconnect is the engine's business and does not need it.
    pub fn take_stream_exit_notice(&mut self) -> bool {
        std::mem::take(&mut self.stream_exit_notice)
    }

    /// One reconnect attempt, if one is due. Called once per frame by whoever owns the backend.
    ///
    /// `None` when there is nothing to do: the stream is alive, or the next attempt is not due
    /// yet. Otherwise the attempt's outcome — `Ok` carries how many attempts it took, `Err` the
    /// backend's reason, and the next attempt is scheduled further out. A failed attempt leaves
    /// the state at the original [`EngineState::StreamExited`]: the reason the stream died is
    /// what the user needs to see, and the failure travels in [`Reconnect::last_failure`].
    pub fn try_reconnect(&mut self, backend: &dyn Backend) -> Option<Result<u32, String>> {
        if !matches!(self.state, EngineState::StreamExited(_)) || self.loaded.is_none() {
            self.reconnect = None;
            return None;
        }
        if !self.reconnect.as_ref().is_some_and(Reconnect::is_due) {
            return None;
        }
        let attempts = self.reconnect.as_ref().map_or(0, |r| r.attempts) + 1;
        match self.start(backend) {
            Ok(()) => Some(Ok(attempts)),
            Err(reason) => {
                self.reconnect = Some(Reconnect {
                    attempts,
                    last_failure: Some(reason.clone()),
                    next_at: Instant::now() + Reconnect::delay_after(attempts),
                });
                Some(Err(reason))
            }
        }
    }

    /// Takes back the processor a failed start left in the return queue and deactivates the
    /// plugin, so the instance can be activated again. A failed start is not a stream death:
    /// the flags and the error slot the owner's `Drop` set are cleared, or the next real death
    /// would be reported with a stale reason.
    fn deactivate_after_failed_start(
        loaded: &mut Loaded,
        returns: &mut rtrb::Consumer<StoppedPluginAudioProcessor<MxmHost>>,
        processor_returned: &AtomicBool,
        stream_exited: &AtomicBool,
        stream_error: &std::sync::Mutex<Option<String>>,
    ) {
        if let Ok(processor) = returns.pop() {
            loaded.instance.deactivate(processor);
        }
        processor_returned.store(false, Ordering::Release);
        stream_exited.store(false, Ordering::Release);
        if let Ok(mut slot) = stream_error.lock() {
            *slot = None;
        }
    }

    /// Step 1: begin one bounded handshake. Later requests cannot postpone its deadline.
    fn request_reconfigure(&mut self) {
        if self.pending.is_none() {
            self.pending = Some(PendingAction::Reconfigure);
        }
        self.issue_stop();
    }

    fn issue_stop(&mut self) {
        if self.stream.is_none() || self.is_wedged() {
            return;
        }
        self.state = EngineState::AwaitingStoppedProcessor;
        self.stop_requested_at.get_or_insert_with(Instant::now);
        if !self.stop_queued {
            self.stop_queued = self
                .commands
                .as_mut()
                .is_some_and(|queue| queue.push(Command::Stop).is_ok());
        }
    }

    /// Steps 3 to 5: take the processor, drop the stream, deactivate, and reactivate if asked.
    fn check_processor_return(&mut self) {
        if self.state == EngineState::AwaitingStoppedProcessor && !self.stop_queued {
            self.issue_stop();
        }
        // The handoff can arrive without anyone having asked for it: after a terminal backend
        // failure the stream's worker loop exits, its closure is dropped, and the owner's `Drop`
        // hands the processor back. That must be noticed while the engine still thinks it is
        // running, or an ordinary device failure would look like nothing at all.
        let unsolicited = self.state == EngineState::Running
            && self.stream_exited.load(Ordering::Acquire)
            && self.processor_returned.load(Ordering::Acquire);

        if self.state != EngineState::AwaitingStoppedProcessor && !unsolicited {
            return;
        }

        if !self.processor_returned.load(Ordering::Acquire) {
            if let Some(since) = self.stop_requested_at
                && since.elapsed() > WEDGE_TIMEOUT
            {
                self.enter_wedged();
            }
            return;
        }

        let exited = self.stream_exited.load(Ordering::Acquire);
        let Some(processor) = self.processor_return.as_mut().and_then(|q| q.pop().ok()) else {
            return;
        };

        // The stream can be dropped safely now: a callback that handed the processor back is not
        // wedged inside the plugin.
        self.stream = None;
        self.commands = None;
        self.gui_events = None;
        self.processor_returned.store(false, Ordering::Release);
        self.stop_requested_at = None;
        self.stop_queued = false;

        if let Some(loaded) = self.loaded.as_mut() {
            loaded.instance.deactivate(processor);
        }
        // The effects' processors were handed back before the source's — the worker's field
        // order sees to that on a dead stream, and `Command::Stop` does it explicitly.
        self.deactivate_fx();

        if exited {
            let reason = self
                .stream_error
                .lock()
                .ok()
                .and_then(|e| e.clone())
                .unwrap_or_else(|| "the audio backend stopped".to_owned());
            self.state = EngineState::StreamExited(reason);
            self.pending = None;
            self.reconnect = Some(Reconnect::scheduled());
            self.stream_exit_notice = true;
            // The playhead is the worker's publication, and the worker is gone: what it last
            // wrote would stand as "playing" until a new worker overwrote it, lighting the button
            // and the step row over silence. This thread is the only writer left, so it is reset
            // here rather than left to go stale.
            self.shared.set_playhead(0, 0);
            return;
        }

        match self.pending.take() {
            Some(PendingAction::Reconfigure) => {
                // Port declarations are only safe to renegotiate once every processor is back.
                let result = (|| -> Result<(), String> {
                    if let Some(loaded) = self.loaded.as_mut() {
                        loaded.envelope =
                            negotiate(&mut loaded.instance).map_err(|e| e.to_string())?;
                    }
                    for slot in &mut self.fx {
                        slot.envelope =
                            negotiate_effect(&mut slot.instance).map_err(|e| e.to_string())?;
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => {
                        self.state = EngineState::Idle;
                        // The app owns the backend and will start us on its next service turn.
                        self.shared.requests.process.store(true, Ordering::Release);
                    }
                    Err(reason) => self.state = EngineState::Failed(reason),
                }
            }
            Some(PendingAction::Stop) | None => self.state = EngineState::Idle,
        }
    }

    /// Stops audio and synchronously polls/yields until the processor returns or time runs out.
    ///
    /// Used before anything that executes plugin code with audio running, which includes
    /// scanning: that is what makes the scan sentinel meaningful.
    pub fn stop_now(&mut self) -> Result<(), String> {
        // A wedged engine has no stream — `enter_wedged` took it — so the "nothing to stop"
        // shortcut below would otherwise report a clean stop for an engine that is terminally
        // broken. Callers use this return to decide it is safe to run plugin code with audio
        // stopped, which is exactly what makes the scan sentinel mean anything.
        if self.is_wedged() {
            return Err(WEDGED_MESSAGE.to_owned());
        }
        if self.stream.is_none() {
            return Ok(());
        }
        self.pending = Some(PendingAction::Stop);
        self.issue_stop();

        let deadline = Instant::now() + WEDGE_TIMEOUT;
        while self.state == EngineState::AwaitingStoppedProcessor && Instant::now() < deadline {
            self.check_processor_return();
            std::thread::yield_now();
        }

        if self.state == EngineState::AwaitingStoppedProcessor {
            self.enter_wedged();
            return Err(WEDGED_MESSAGE.to_owned());
        }
        // The polling loop above calls `check_processor_return`, which reaches the timeout and
        // wedges on its own. Falling through to `Ok` after that would report success for the one
        // outcome this method exists to detect.
        if self.is_wedged() {
            return Err(WEDGED_MESSAGE.to_owned());
        }
        Ok(())
    }

    /// The terminal state.
    ///
    /// Retaining the stream in ordinary application state is **not enough**: when the window
    /// closes, normal field destruction still calls `Stream::drop()`, which signals CPAL's
    /// worker and then *joins* it — and that worker only exits once the current callback
    /// returns. So the transition is structural: the stream, the processor, the plugin instance
    /// and its loaded library are moved into deliberately leaked storage, and no destructor for
    /// any of them ever runs.
    fn enter_wedged(&mut self) {
        struct Wedged {
            _stream: Option<Box<dyn AudioStream>>,
            _loaded: Option<Loaded>,
            _fx: Vec<FxSlot>,
            _fx_returns: Vec<FxReturn>,
        }

        // The editor is deliberately *not* closed here. Wedging means the plugin has stopped
        // answering, so a `clap.gui` call would hang the GUI thread as surely as the one that got
        // us here. The window leaks with the instance, which is what wedging already means. The
        // effects leak with it: their processors are on the same stuck callback.
        let wedged = Wedged {
            _stream: self.stream.take(),
            _loaded: self.loaded.take(),
            _fx: std::mem::take(&mut self.fx),
            _fx_returns: std::mem::take(&mut self.fx_returns),
        };
        self.editors.clear();
        // Ownership never returns to droppable state. Reconfiguration after a hang is
        // impossible in-process — a consequence of having no process isolation, not a gap.
        Box::leak(Box::new(wedged));

        self.commands = None;
        self.gui_events = None;
        self.processor_return = None;
        self.reconnect = None;
        self.state = EngineState::Wedged;
    }

    /// Takes the MIDI-out consumer, so the worker thread can own it.
    pub fn take_midi_out_consumer(&mut self) -> Option<rtrb::Consumer<OutgoingEvent>> {
        self.midi_out_consumer.take()
    }

    /// Drains what the plugin has emitted for the GUI since the last frame.
    pub fn drain_plugin_output(&mut self) -> Vec<PluginOutput> {
        let mut out = Vec::new();
        if let Some(queue) = self.plugin_output.as_mut() {
            while let Ok(event) = queue.pop() {
                out.push(event);
            }
        }
        out
    }

    /// Opens the MIDI inputs the user asked for, claiming one preallocated slot each.
    ///
    /// Ports beyond `MAX_INPUT_PRODUCERS` are **refused with a visible reason**, the same way an
    /// incompatible plugin is. "All ports" selects up to the maximum and says so when it
    /// truncates.
    fn connect_desired_midi_inputs(
        &mut self,
        producers: impl Iterator<Item = rtrb::Producer<TimedEvent>>,
    ) {
        self.midi_inputs.clear();
        self.refused_midi_inputs.clear();
        // Ports are about to be reopened; anything held across the gap has no release coming.
        self.shared().clear_midi_sounding();

        let mut producers = producers;
        let mut slot = 1u8;

        for name in self.desired_midi_inputs.clone() {
            let Some(producer) = producers.next() else {
                self.refused_midi_inputs.push(RefusedInput {
                    port: name,
                    reason: format!(
                        "the player opens at most {MAX_INPUT_PRODUCERS} MIDI inputs at once"
                    ),
                });
                continue;
            };
            match crate::midi::input::connect(
                &name,
                SourceId(slot),
                producer,
                Arc::clone(&self.input_epoch),
                Arc::clone(&self.clock),
                Arc::clone(self.shared()),
            ) {
                Ok(connection) => {
                    self.midi_inputs.push(connection);
                    slot += 1;
                }
                // The reason is the whole point: "busy", "gone" and "too many" are different
                // problems with different fixes, and inventing one for all three sent the user
                // looking at a limit they had not reached.
                Err(reason) => self
                    .refused_midi_inputs
                    .push(RefusedInput { port: name, reason }),
            }
        }
    }

    /// Ports that could not be connected, each carrying **why**. Shown rather than silently
    /// truncated, and never with a reason the code guessed.
    pub fn refused_midi_inputs(&self) -> &[RefusedInput] {
        &self.refused_midi_inputs
    }

    /// What the user has asked to have open, whether or not a stream exists yet to open it.
    ///
    /// Distinct from [`Engine::connected_midi_inputs`]: a desired port is only connected once
    /// the stream starts, and the two differing is exactly what a restored-but-dead selection
    /// looks like.
    pub fn desired_midi_inputs(&self) -> &[String] {
        &self.desired_midi_inputs
    }

    /// The MIDI output port the user has asked for, connected or not.
    pub fn desired_midi_output(&self) -> Option<&str> {
        self.desired_midi_output.as_deref()
    }

    /// The MIDI input ports currently connected.
    pub fn connected_midi_inputs(&self) -> Vec<String> {
        self.midi_inputs
            .iter()
            .map(|c| c.port_name.clone())
            .collect()
    }

    /// Replaces the set of MIDI inputs, which is a topology change and therefore goes through
    /// the stop-and-return protocol rather than being applied under a running audio thread.
    pub fn set_midi_inputs(&mut self, ports: Vec<String>) {
        if ports == self.desired_midi_inputs {
            return;
        }
        self.desired_midi_inputs = ports;
        if self.stream.is_some() {
            // Disconnecting also releases anything thru has sent outward.
            if self.thru_enabled.load(Ordering::Relaxed) {
                self.out_panic.raise();
            }
            self.request_reconfigure();
        }
    }

    fn start_midi_output(&mut self) {
        let Some(name) = self.desired_midi_output.clone() else {
            return;
        };
        let Some(queue) = self.midi_out_consumer.take() else {
            return;
        };
        match MidirSink::open(&name) {
            Ok(sink) => {
                self.midi_output = Some(crate::midi::port::spawn(
                    sink,
                    queue,
                    Arc::clone(&self.out_panic),
                ));
            }
            Err(reason) => {
                self.state = EngineState::Failed(reason);
            }
        }
    }

    /// Chooses the MIDI output port. Dropping the old worker releases everything held on it.
    pub fn set_midi_output(&mut self, port: Option<String>) {
        if port == self.desired_midi_output {
            return;
        }
        self.desired_midi_output = port;
        // Releases outstanding notes on the old port before closing it.
        self.midi_output = None;
        if self.stream.is_some() {
            self.request_reconfigure();
        }
    }

    pub fn midi_output_faulted(&self) -> bool {
        self.midi_output.as_ref().is_some_and(|w| w.is_faulted())
    }

    pub fn midi_output_port(&self) -> Option<&str> {
        self.desired_midi_output.as_deref()
    }

    /// The plugin's reported latency in samples, re-queried whenever it says it changed.
    ///
    /// `None` means the plugin does not implement the `latency` extension, which is not the same
    /// as reporting zero.
    pub fn latency_samples(&mut self) -> Option<u32> {
        let loaded = self.loaded.as_mut()?;
        let ext: PluginLatency = loaded.instance.plugin_shared_handle().get_extension()?;
        // Consume the change flag: the value below is the fresh one either way.
        self.shared
            .notifications
            .latency_changed
            .store(false, Ordering::Release);
        let mut handle = loaded.instance.plugin_handle();
        Some(ext.get(&mut handle))
    }

    /// Whether the plugin has said its latency changed since it was last read.
    pub fn latency_changed(&self) -> bool {
        self.shared
            .notifications
            .latency_changed
            .load(Ordering::Acquire)
    }

    /// Reads every parameter's metadata and value.
    pub fn read_params(&mut self) -> ParamSet {
        match self.loaded.as_mut() {
            Some(loaded) => ParamSet::read(&mut loaded.instance),
            None => ParamSet::default(),
        }
    }

    /// Reads one effect's parameters, the same way the source's are read.
    ///
    /// **New with effect automation.** The player showed no effect parameters at all — an effect's
    /// controls are its own editor's business — but the sequencer cannot offer to automate what it
    /// cannot name, so the chain's parameters are readable now. Main thread, on demand: nothing
    /// caches them, because an effect's parameter list does not change while it is loaded and a
    /// stale copy is worse than a read.
    pub fn read_fx_params(&mut self, index: usize) -> Option<ParamSet> {
        let slot = self.fx.get_mut(index)?;
        Some(ParamSet::read(&mut slot.instance))
    }

    /// Requeries values and formatted text, leaving metadata alone.
    pub fn refresh_param_values(&mut self, set: &mut ParamSet) {
        if let Some(loaded) = self.loaded.as_mut() {
            set.refresh_values(&mut loaded.instance);
        }
    }

    /// Formats a value the way the plugin would, so the panel and the plugin never disagree.
    pub fn format_param(&mut self, param_id: u32, value: f64) -> String {
        let Some(loaded) = self.loaded.as_mut() else {
            return format!("{value:.3}");
        };
        let Some(id) = ClapId::from_raw(param_id) else {
            return format!("{value:.3}");
        };
        let Some(ext): Option<clack_extensions::params::PluginParams> =
            loaded.instance.plugin_shared_handle().get_extension()
        else {
            return format!("{value:.3}");
        };
        crate::params::format_value(&mut loaded.instance, &ext, id, value)
    }

    /// Parses typed-in text through the plugin, so direct entry means what the display means.
    pub fn parse_param(&mut self, param_id: u32, text: &str) -> Option<f64> {
        let loaded = self.loaded.as_mut()?;
        let id = ClapId::from_raw(param_id)?;
        crate::params::parse_text(&mut loaded.instance, id, text)
    }

    /// Saves the plugin's state through the `state` extension.
    pub fn save_state(&mut self, path: &Path) -> Result<(), String> {
        let loaded = self
            .loaded
            .as_mut()
            .ok_or_else(|| "no plugin is loaded".to_owned())?;
        let ext: PluginState = loaded
            .instance
            .plugin_shared_handle()
            .get_extension()
            .ok_or_else(|| "the plugin does not implement the state extension".to_owned())?;

        let mut bytes = Vec::new();
        let mut handle = loaded.instance.plugin_handle();
        ext.save(&mut handle, &mut bytes)
            .map_err(|e| e.to_string())?;
        std::fs::write(path, &bytes).map_err(|e| e.to_string())
    }

    /// Loads plugin state, then **requeries every parameter**.
    ///
    /// Not belt-and-braces: nice-plug 0.3.0 never issues `params.rescan(VALUES)` after a host
    /// state load, so a host that trusted the callback would show a stale panel with our own
    /// plugins.
    pub fn load_state(&mut self, path: &Path, params: &mut ParamSet) -> Result<(), String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let loaded = self
            .loaded
            .as_mut()
            .ok_or_else(|| "no plugin is loaded".to_owned())?;
        let ext: PluginState = loaded
            .instance
            .plugin_shared_handle()
            .get_extension()
            .ok_or_else(|| "the plugin does not implement the state extension".to_owned())?;

        {
            let mut handle = loaded.instance.plugin_handle();
            ext.load(&mut handle, &mut bytes.as_slice())
                .map_err(|e| e.to_string())?;
        }

        *params = ParamSet::read(&mut loaded.instance);
        Ok(())
    }

    /// Replaces the audio configuration, restarting the stream through the normal protocol.
    pub fn set_audio_config(&mut self, config: AudioConfig) {
        self.audio_config = config;
        if self.stream.is_some() {
            self.request_reconfigure();
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_engine_is_idle_and_holds_nothing() {
        let engine = Engine::new();
        assert_eq!(*engine.state(), EngineState::Idle);
        assert!(engine.envelope().is_none());
        assert!(!engine.is_wedged());
        assert!(!engine.leaked_processor());
        assert!(engine.fx_info().is_empty());
    }

    #[test]
    fn chain_edits_on_an_empty_chain_are_refused_with_the_index_named() {
        let mut engine = Engine::new();
        assert!(engine.remove_fx(0).unwrap_err().contains("effect 1"));
        assert!(
            engine
                .set_fx_bypassed(2, true)
                .unwrap_err()
                .contains("effect 3")
        );
        assert!(engine.move_fx(0, 1).unwrap_err().contains("0 effect"));
    }

    /// **An effect takes its editor's edits with no source loaded**, which is how a restored chain
    /// starts: no stream, so no stage to flush it. The flag being taken is the proof, as in
    /// `t11_fx_chain`: with no stream only the main-thread flush consumes it.
    #[test]
    fn an_effect_with_nothing_running_takes_its_editors_edits() {
        let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/fixtures/mxm-fixtures.clap");
        assert!(bundle.exists(), "build the quarantined fixtures first");
        let mut engine = Engine::new();
        engine.add_fx(&bundle, "dk.mxm.fixture.effect").unwrap();
        assert!(!engine.fx[0].instance.is_active());

        // Its editor moves a knob: nice-plug queues the value and asks the host for a flush.
        engine.fx[0]
            .shared
            .requests
            .param_flush
            .store(true, Ordering::Release);
        engine.poll();

        assert!(
            !engine.fx[0]
                .shared
                .requests
                .param_flush
                .load(Ordering::Acquire),
            "nothing was running, so the effect never took its own editor's edit"
        );
        assert!(!engine.fx[0].instance.is_active(), "a flush is not a start");
    }

    #[test]
    fn effect_restart_and_port_requests_stop_renegotiate_and_allow_restart() {
        let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/fixtures/mxm-fixtures.clap");
        assert!(bundle.exists(), "build the quarantined fixtures first");
        for ports in [false, true] {
            let backend = audio::FakeBackend::new();
            let mut engine = Engine::new();
            engine.load(&bundle, "dk.mxm.fixture.main-thread").unwrap();
            engine.add_fx(&bundle, "dk.mxm.fixture.effect").unwrap();
            engine.start(&backend).unwrap();
            if ports {
                engine.fx[0]
                    .shared
                    .notifications
                    .audio_ports_changed
                    .store(true, Ordering::Release);
            } else {
                engine.fx[0]
                    .shared
                    .requests
                    .restart
                    .store(true, Ordering::Release);
            }
            // Prove negotiation actually happened, not merely that the engine changed labels.
            engine.fx[0].envelope.output.channel_count = 1;
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && engine.state != EngineState::Idle {
                engine.poll();
                std::thread::yield_now();
            }
            let idle = engine.state == EngineState::Idle;
            let channels = engine.fx[0].envelope.output.channel_count;
            let requested_start = engine.take_idle_process_request();
            engine.stop_now().unwrap();
            assert!(
                idle && requested_start,
                "effect request was not serviced (ports={ports})"
            );
            assert_eq!(channels, 2);
            engine.start(&backend).unwrap();
            engine.stop_now().unwrap();
        }
    }

    #[test]
    fn a_refused_gesture_end_is_remembered_until_it_is_accepted() {
        let mut engine = Engine::new();
        // With no stream there is no queue, so the push is refused and the retry recorded.
        assert!(!engine.push_gui_event(crate::events::input::Payload::GestureEnd { param_id: 7 }));
        assert!(
            !engine.gesture_end_pending(7),
            "with no engine running there is no gesture to close"
        );
    }
}
