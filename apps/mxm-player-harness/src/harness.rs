//! A harness that owns an [`AudioWorker`] directly, with no engine and no audio device.
//!
//! The callback then runs **on the test thread**, which is what makes the interesting properties
//! assertable: whether it allocates, exactly which events reach the plugin, and what the press
//! accounting looks like afterwards.

use clack_extensions::tail::PluginTail;
use clack_host::prelude::*;
use mxm_player::clock::Clock;
use mxm_player::engine::PluginOutput;
use mxm_player::engine::fx::{FxChain, FxStage, StageConfig};
use mxm_player::engine::meters::Meters;
use mxm_player::engine::processor::{AudioWorker, Command, WorkerConfig};
use mxm_player::engine::stream::ProcessorOwner;
use mxm_player::envelope::{Envelope, negotiate, negotiate_effect};
use mxm_player::events::input::{
    PRODUCER_QUEUE_CAPACITY, PanicEpoch, Payload, SourceId, TimedEvent,
};
use mxm_player::host::{
    HostAudioProcessor, HostMainThread, HostShared, MxmHost, PlayerHostState, host_info,
};
use mxm_player::midi::{OutPanic, OutgoingEvent};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub const SAMPLE_RATE: f64 = 48_000.0;
pub const CHANNELS: usize = 2;

/// Where the mxm-mono-01 bundle lives, or `None` if it has not been built.
pub fn mxm_mono_01() -> Option<PathBuf> {
    artifact(
        "target/bundled/mxm-mono-01.clap",
        "cargo xtask bundle mxm-mono-01 --release",
    )
}

/// Where the mxm-mono-08 bundle lives, or `None` if it has not been built.
pub fn mxm_mono_08() -> Option<PathBuf> {
    artifact(
        "target/bundled/mxm-mono-08.clap",
        "cargo xtask bundle mxm-mono-08 --release",
    )
}

/// Where the mxm-mono-pr1 bundle lives, or `None` if it has not been built.
pub fn mxm_mono_pr1() -> Option<PathBuf> {
    artifact(
        "target/bundled/mxm-mono-pr1.clap",
        "cargo xtask bundle mxm-mono-pr1 --release",
    )
}

/// Where the fixture bundle lives, or `None` if it has not been staged.
pub fn fixtures() -> Option<PathBuf> {
    artifact(
        "target/fixtures/mxm-fixtures.clap",
        "cargo xtask fixtures --release",
    )
}

fn artifact(relative: &str, hint: &str) -> Option<PathBuf> {
    let path = crate::workspace_root().join(relative);

    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipping: {} is missing — run `{hint}`", path.display());
        None
    }
}

/// One effect in a driven chain: its switch, the queue its processor comes back on, and the
/// instance and entry kept alive for as long as the stage may run.
pub struct HarnessFx {
    pub bypassed: Arc<AtomicBool>,
    /// What the effect reports into, and what its editor asks through: this is where a
    /// `request_flush` lands.
    pub shared: Arc<PlayerHostState>,
    pub processor_return: rtrb::Consumer<StoppedPluginAudioProcessor<MxmHost>>,
    pub instance: PluginInstance<MxmHost>,
    pub entry: PluginEntry,
}

/// Everything a driven worker needs, kept alive together.
pub struct Harness {
    /// Declared first so it outlives nothing that matters; the worker is dropped first.
    pub worker: AudioWorker,
    /// The chain the worker runs, in order - empty unless built with [`Harness::with_fx`].
    pub fx: Vec<HarnessFx>,
    pub commands: rtrb::Producer<Command>,
    /// Sequencer states the worker has finished with. Drained by the harness, never by the worker.
    pub retired: rtrb::Consumer<std::sync::Arc<mxm_player::sequencer::SequencerState>>,
    pub inputs: Vec<rtrb::Producer<TimedEvent>>,
    pub midi_out: rtrb::Consumer<OutgoingEvent>,
    pub plugin_output: rtrb::Consumer<PluginOutput>,
    pub processor_return: rtrb::Consumer<StoppedPluginAudioProcessor<MxmHost>>,
    pub processor_returned: Arc<AtomicBool>,
    pub stream_exited: Arc<AtomicBool>,
    pub leaked: Arc<AtomicBool>,
    pub input_epoch: Arc<PanicEpoch>,
    pub out_panic: Arc<OutPanic>,
    pub thru_enabled: Arc<AtomicBool>,
    pub meters: Arc<Meters>,
    pub shared: Arc<PlayerHostState>,
    pub envelope: Envelope,
    /// The harness's own timeline, so a test can stamp events deterministically.
    pub clock: std::sync::Arc<Clock>,
    /// Kept alive: the instance must outlive the processor, and the entry the instance.
    pub instance: PluginInstance<MxmHost>,
    pub entry: PluginEntry,
    buffer: Vec<f32>,
}

impl Harness {
    /// Loads a plugin, activates it, and wires a worker to drive it by hand.
    pub fn new(bundle: &Path, plugin_id: &str, producers: usize) -> Result<Self, String> {
        Self::build(bundle, plugin_id, producers, SAMPLE_RATE, 4096, &[])
    }

    /// The same, with an explicit `max_frames_count`, so a test can vary what the plugin was
    /// activated for rather than only what it is asked to render.
    pub fn with_max_frames(
        bundle: &Path,
        plugin_id: &str,
        producers: usize,
        max_frames: u32,
    ) -> Result<Self, String> {
        Self::build(bundle, plugin_id, producers, SAMPLE_RATE, max_frames, &[])
    }

    /// The same direct host path with both activation quantities explicit. This is for robustness
    /// tests: rates need not be integer or conventional, and one activation may be driven at every
    /// legal callback size up to its advertised maximum.
    pub fn with_configuration(
        bundle: &Path,
        plugin_id: &str,
        producers: usize,
        sample_rate: f64,
        max_frames: u32,
    ) -> Result<Self, String> {
        Self::build(bundle, plugin_id, producers, sample_rate, max_frames, &[])
    }

    /// The same, with an effect chain after the source: `fx` names each effect's bundle and id,
    /// in signal order. Every effect starts **on**; flip [`HarnessFx::bypassed`] to switch one
    /// off, exactly as the strip does.
    pub fn with_fx(
        bundle: &Path,
        plugin_id: &str,
        producers: usize,
        fx: &[(&Path, &str)],
    ) -> Result<Self, String> {
        Self::build(bundle, plugin_id, producers, SAMPLE_RATE, 4096, fx)
    }

    fn build(
        bundle: &Path,
        plugin_id: &str,
        producers: usize,
        sample_rate: f64,
        max_frames: u32,
        fx: &[(&Path, &str)],
    ) -> Result<Self, String> {
        // SAFETY: loading a CLAP bundle runs its code. These are our own artifacts, loaded by
        // path from the build tree.
        let entry = unsafe { mxm_player::entry::load(bundle) }.map_err(|e| e.to_string())?;
        let (shared, mut instance) = instantiate(&entry, plugin_id)?;

        let envelope = negotiate(&mut instance).map_err(|r| r.to_string())?;
        let tail: Option<PluginTail> = instance.plugin_shared_handle().get_extension();

        let processor = instance
            .activate(
                |shared, _| HostAudioProcessor::new(shared),
                PluginAudioConfiguration {
                    sample_rate,
                    min_frames_count: 1,
                    max_frames_count: max_frames,
                },
            )
            .map_err(|e| e.to_string())?;

        let (command_producer, command_consumer) = rtrb::RingBuffer::new(64);
        let (return_producer, return_consumer) = rtrb::RingBuffer::new(1);
        let (output_producer, output_consumer) = rtrb::RingBuffer::new(4096);
        let (midi_out_producer, midi_out_consumer) = rtrb::RingBuffer::new(4096);

        let mut input_producers = Vec::new();
        let mut input_consumers = Vec::new();
        for _ in 0..producers {
            let (producer, consumer) = rtrb::RingBuffer::new(PRODUCER_QUEUE_CAPACITY);
            input_producers.push(producer);
            input_consumers.push(consumer);
        }

        let processor_returned = Arc::new(AtomicBool::new(false));
        let stream_exited = Arc::new(AtomicBool::new(false));
        let leaked = Arc::new(AtomicBool::new(false));
        let input_epoch = Arc::new(PanicEpoch::new());
        let out_panic = Arc::new(OutPanic::new());
        let thru_enabled = Arc::new(AtomicBool::new(false));
        let meters = Arc::new(Meters::new());
        // Monotonic by default: existing tests assert behaviour, not reproducibility.
        let clock = Clock::monotonic();

        let owner = ProcessorOwner::new(
            processor,
            return_producer,
            Arc::clone(&processor_returned),
            Arc::clone(&stream_exited),
            Arc::clone(&leaked),
        );

        // Where the worker hands back a consumed sequencer state, so it is never dropped on the
        // audio thread. As deep as the command queue, exactly as the engine sizes it.
        let (retire_producer, retire_consumer) = rtrb::RingBuffer::new(64);

        // The chain, built as the engine builds it: each effect activated for the same block
        // bound as the source, its processor owned by a stage, its return queue kept here.
        let mut stages = Vec::with_capacity(fx.len());
        let mut kept = Vec::with_capacity(fx.len());
        for (position, (fx_bundle, fx_id)) in fx.iter().enumerate() {
            // SAFETY: as above - our own artifacts, loaded by path.
            let fx_entry =
                unsafe { mxm_player::entry::load(fx_bundle) }.map_err(|e| e.to_string())?;
            let (fx_shared, mut fx_instance) = instantiate(&fx_entry, fx_id)?;
            let fx_envelope = negotiate_effect(&mut fx_instance).map_err(|r| r.to_string())?;
            let fx_tail: Option<PluginTail> = fx_instance.plugin_shared_handle().get_extension();
            let fx_params: Option<clack_extensions::params::PluginParams> =
                fx_instance.plugin_shared_handle().get_extension();
            let fx_processor = fx_instance
                .activate(
                    |shared, _| HostAudioProcessor::new(shared),
                    PluginAudioConfiguration {
                        sample_rate,
                        min_frames_count: 1,
                        max_frames_count: max_frames,
                    },
                )
                .map_err(|e| e.to_string())?;
            let (fx_return_producer, fx_return_consumer) = rtrb::RingBuffer::new(1);
            let bypassed = Arc::new(AtomicBool::new(false));
            let fx_shared_for_stage = Arc::clone(&fx_shared);
            let fx_owner = ProcessorOwner::new(
                fx_processor,
                fx_return_producer,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
            );
            stages.push(FxStage::new(StageConfig {
                // The harness builds its chain by hand, so it hands out ids by hand too: one each,
                // from one, exactly as the engine's counter does.
                id: (position + 1) as mxm_player::engine::fx::FxId,
                owner: fx_owner,
                envelope: fx_envelope,
                bypassed: Arc::clone(&bypassed),
                shared: fx_shared_for_stage,
                tail: fx_tail,
                params: fx_params,
            }));
            kept.push(HarnessFx {
                bypassed,
                shared: Arc::clone(&fx_shared),
                processor_return: fx_return_consumer,
                instance: fx_instance,
                entry: fx_entry,
            });
        }

        let worker = AudioWorker::new(WorkerConfig {
            owner,
            fx: FxChain::new(stages),
            commands: command_consumer,
            retired: retire_producer,
            inputs: input_consumers,
            midi_out: Some(midi_out_producer),
            to_gui: output_producer,
            envelope: envelope.clone(),
            tail,
            sample_rate,
            meters: Arc::clone(&meters),
            shared: Arc::clone(&shared),
            thru_enabled: Arc::clone(&thru_enabled),
            out_panic: Arc::clone(&out_panic),
            input_epoch: Arc::clone(&input_epoch),
            clock: Arc::clone(&clock),
        });

        Ok(Self {
            worker,
            fx: kept,
            commands: command_producer,
            retired: retire_consumer,
            inputs: input_producers,
            midi_out: midi_out_consumer,
            plugin_output: output_consumer,
            processor_return: return_consumer,
            processor_returned,
            stream_exited,
            leaked,
            input_epoch,
            out_panic,
            thru_enabled,
            meters,
            shared,
            envelope,
            clock,
            instance,
            entry,
            buffer: vec![0.0; max_frames as usize * CHANNELS],
        })
    }

    /// Pushes an event onto one producer's queue, as that producer would.
    pub fn push(&mut self, source: usize, payload: Payload) -> bool {
        let event = TimedEvent::new(
            self.clock.now_nanos(),
            self.input_epoch.current(),
            SourceId(source as u8),
            payload,
        );
        self.inputs[source].push(event).is_ok()
    }

    /// Pushes without going through the clock, so a test can control ordering exactly.
    pub fn push_at(&mut self, source: usize, arrival_nanos: u64, payload: Payload) -> bool {
        let event = TimedEvent::new(
            arrival_nanos,
            self.input_epoch.current(),
            SourceId(source as u8),
            payload,
        );
        self.inputs[source].push(event).is_ok()
    }

    /// Fills one producer's queue to the brim, so the next push is refused.
    pub fn saturate(&mut self, source: usize) -> usize {
        let mut pushed = 0;
        while self.push(
            source,
            Payload::ControlChange {
                channel: 0,
                controller: 20,
                value: 64,
            },
        ) {
            pushed += 1;
            assert!(pushed < 1_000_000, "the producer queue must be bounded");
        }
        pushed
    }

    /// Runs one callback of `frames` frames and returns the interleaved output.
    pub fn render(&mut self, frames: usize) -> &[f32] {
        self.buffer.resize(frames * CHANNELS, 0.0);
        self.buffer.fill(0.0);
        self.worker.callback(&mut self.buffer, CHANNELS);
        &self.buffer
    }

    /// The largest absolute sample of the last render.
    pub fn peak(&self) -> f32 {
        self.buffer.iter().fold(0.0f32, |p, s| p.max(s.abs()))
    }

    /// Everything the MIDI-out queue has received since the last drain.
    pub fn drain_midi_out(&mut self) -> Vec<OutgoingEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.midi_out.pop() {
            out.push(event);
        }
        out
    }

    pub fn drain_plugin_output(&mut self) -> Vec<PluginOutput> {
        let mut out = Vec::new();
        while let Ok(event) = self.plugin_output.pop() {
            out.push(event);
        }
        out
    }

    /// Hands the processors back and deactivates them, as the engine's protocol does - the
    /// chain's first, as the worker returns them, then the source's.
    pub fn shutdown(mut self) {
        let _ = self.commands.push(Command::Stop);
        self.render(64);
        for fx in &mut self.fx {
            if let Ok(processor) = fx.processor_return.pop() {
                fx.instance.deactivate(processor);
            }
        }
        if let Ok(processor) = self.processor_return.pop() {
            self.instance.deactivate(processor);
        }
    }
}

/// A fresh instance of `plugin_id` from `entry`, with the host state it reports into.
pub fn instantiate(
    entry: &PluginEntry,
    plugin_id: &str,
) -> Result<(Arc<PlayerHostState>, PluginInstance<MxmHost>), String> {
    let factory = entry
        .get_plugin_factory()
        .ok_or_else(|| "no plugin factory".to_owned())?;
    let id = factory
        .plugin_descriptors()
        .find(|d| d.id().map(|id| id.to_bytes()) == Some(plugin_id.as_bytes()))
        .and_then(|d| d.id())
        .ok_or_else(|| format!("no plugin `{plugin_id}`"))?
        .to_owned();

    let shared = Arc::new(PlayerHostState::new());
    let for_plugin = Arc::clone(&shared);
    let instance = PluginInstance::<MxmHost>::new(
        move |_| HostShared::new(for_plugin),
        |shared| HostMainThread::new(shared),
        entry,
        &id,
        &host_info(),
    )
    .map_err(|e| e.to_string())?;
    Ok((shared, instance))
}
