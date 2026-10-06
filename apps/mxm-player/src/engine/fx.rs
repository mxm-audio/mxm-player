//! The effect chain: what the source feeds, in order, on the way to the device.
//!
//! **A serial chain, not a graph** — the owner's ruling (2026-09-04): effects sit in a fixed
//! order the user can rearrange, each with an on/off that bypasses it, and the player shows no
//! effect parameters at all: an effect is played from its own editor. No sends, no parallel
//! routing, no sidechains; the chain is one line from the source to the output.
//!
//! Two halves, on two threads:
//!
//! - [`FxSlot`] is what the GUI thread owns for each loaded effect: its entry, its instance and its
//!   negotiated [`EffectEnvelope`]. Adding, removing or reordering a slot is a topology change and
//!   goes through the engine's stop-and-return protocol, exactly as a source change does.
//! - [`FxStage`] is what the audio thread owns for the same effect while a stream runs: its
//!   processor, its own buffers, and its own sleep and tail bookkeeping. [`FxChain`] runs the
//!   stages in order over one chunk.
//!
//! # Bypass stops the CPU, not only the sound
//!
//! An effect that is off is **not audio-processed**. The bypass flag is one atomic the GUI writes and the
//! audio thread reads per chunk; a bypassed stage costs nothing beyond that read, and the signal
//! passes it untouched — the chain simply keeps pointing at the previous stage's output. When it is
//! switched back on its processor is `reset` first, because whatever its delay lines and filters
//! held is from before the gap and would come back as a burst of the past.
//!
//! # Its input is what wakes it
//!
//! A source is woken by notes; an effect has none. Each stage sleeps on its own status — `Sleep`,
//! a drained finite tail, or exact silence out for several buffers under `ContinueIfNotQuiet` — and
//! wakes the moment its input carries a sample that is not exact zero. So a stage with a tail keeps
//! running on silence after the source has slept, and the whole graph sleeps only when the source
//! has nothing to send and no stage is open.
//!
//! # Channels adapt at every boundary
//!
//! Mono into a stereo effect is duplicated; stereo into a mono effect is summed at half; the same
//! shape is copied. The device is opened for the widest output in the chain, and the final
//! interleave adapts once more. Every adaptation is a copy into the stage's own input buffers, so
//! no stage ever writes into another's memory.

use crate::engine::processor::MAX_BLOCK_FRAMES;
use crate::engine::stream::ProcessorOwner;
use crate::envelope::EffectEnvelope;
use crate::events::output::FixedEventBuffer;
use crate::host::{MxmHost, PlayerHostState};
use clack_extensions::params::PluginParams;
use clack_extensions::tail::{PluginTail, TailLength};
use clack_host::events::event_types::{ParamModEvent, TransportEvent};
use clack_host::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How many effects the chain holds at most.
///
/// A bound, so every buffer and queue is allocated at activation and nothing grows on the audio
/// thread. Eight is more than a person will chain behind one instrument to audition it; the
/// number is not a design constraint anybody should feel.
pub const MAX_FX: usize = 8;

/// One effect's identity, for anything that has to still mean the same effect after the chain
/// changes.
///
/// **A position is not an identity.** An effect is addressed by index nearly everywhere here, and
/// `Engine::remove_fx` already has to walk the open editors fixing up indices after a removal;
/// `move_fx` reorders them outright. Anything held *outside* the chain that named a position would
/// therefore be re-pointed at a different effect by an unrelated edit — which is exactly what the
/// sequencer's per-step automation must never do.
///
/// So: a counter, assigned once at `add_fx` and **never reused**, even after the effect it belonged
/// to is gone. Reuse would let a stale reference attach itself to a new effect, which is the same
/// defect wearing a different hat.
pub type FxId = u32;

/// The id no effect has, so a default can never collide with a real one.
pub const NO_FX_ID: FxId = 0;

/// How many consecutive exactly-silent output buffers put a `ContinueIfNotQuiet` stage to sleep.
/// The same figure the source uses, for the same reason: exact zero, never a threshold.
const QUIET_BUFFERS_BEFORE_SLEEP: u32 = 8;

/// One loaded effect, on the GUI thread.
pub struct FxSlot {
    /// **Stable for this effect's whole life in the chain**, and never reused. See [`FxId`].
    pub id: FxId,
    /// Kept alive for as long as the instance is: the entry owns the loaded library.
    pub(crate) _entry: PluginEntry,
    pub(crate) instance: PluginInstance<MxmHost>,
    pub envelope: EffectEnvelope,
    pub plugin_id: String,
    pub bundle: PathBuf,
    /// This effect's host state: its requests and notifications, separate from the source's.
    pub(crate) shared: Arc<PlayerHostState>,
    /// Whether it is bypassed. Shared with the audio thread, which reads it per chunk.
    pub(crate) bypassed: Arc<AtomicBool>,
}

impl FxSlot {
    pub fn bypassed(&self) -> bool {
        self.bypassed.load(Ordering::Relaxed)
    }

    /// Bypass is the one control the GUI changes without a topology change: one atomic store.
    pub fn set_bypassed(&self, bypassed: bool) {
        self.bypassed.store(bypassed, Ordering::Relaxed);
    }

    /// Hands the plugin its own editor's queued parameter changes **while nothing is running**.
    ///
    /// [`FxStage::flush_params`] needs a stage, and a stage exists only while a stream runs. With
    /// no source loaded — the state a restored chain starts in — there is none, so an effect's
    /// editor opened onto controls that did nothing: nice-plug applies an editor's edit only when
    /// the plugin is processed or flushed, and neither happened (the owner, 2026-09-30, with the
    /// keyboard cursor on `mxm-fx-convolution`'s Mix). CLAP makes `params.flush` a main-thread
    /// call on an inactive plugin for exactly this case.
    ///
    /// **An active effect is left alone**: it returns no inactive handle, and its stage takes the
    /// request on the audio thread. What comes back is the plugin's own echo, dropped as the stage
    /// drops it.
    pub(crate) fn flush_params_while_inactive(&mut self) {
        let params: Option<PluginParams> = self.instance.plugin_shared_handle().get_extension();
        let Some(mut handle) = self.instance.inactive_plugin_handle() else {
            return;
        };
        if !self
            .shared
            .requests
            .param_flush
            .swap(false, Ordering::AcqRel)
        {
            return;
        }
        let Some(params) = params else {
            return;
        };
        let mut echo = EventBuffer::new();
        params.flush(
            &mut handle,
            &InputEvents::empty(),
            &mut OutputEvents::from_buffer(&mut echo),
        );
    }
}

/// What the interface and the state dump show for one effect.
#[derive(Clone, Debug, PartialEq)]
pub struct FxInfo {
    /// The chain identity anything outside the chain refers to it by. Not its position.
    pub id: FxId,
    pub plugin_id: String,
    pub bundle: PathBuf,
    pub bypassed: bool,
    pub input_channels: u32,
    pub output_channels: u32,
    pub selection: String,
}

/// One effect's audio-thread half.
pub struct FxStage {
    /// Which effect this is, whatever position it currently occupies. What the sequencer's
    /// automation is routed by.
    id: FxId,
    owner: ProcessorOwner,
    input_channels: usize,
    output_channels: usize,
    bypassed: Arc<AtomicBool>,
    /// Whether the previous chunk found the stage bypassed, so re-enabling can reset it.
    was_bypassed: bool,
    shared: Arc<PlayerHostState>,
    tail: Option<PluginTail>,
    /// The params extension, if the plugin has one. Used only while bypassed, where it is the one
    /// way the plugin's own editor reaches it.
    params: Option<PluginParams>,
    cached_tail: Option<TailLength>,
    tail_frames_remaining: Option<u64>,
    /// Whether the plugin is being called. False once it has slept; its input wakes it.
    active: bool,
    quiet_buffers: u32,
    steady_time: u64,
    /// The stage's own input, adapted from whatever precedes it; `input_channels` of them.
    input: Vec<Vec<f32>>,
    /// Its output; `output_channels` of them. What the next stage, or the device, reads.
    output: Vec<Vec<f32>>,
    input_ports: AudioPorts,
    output_ports: AudioPorts,
    /// **What the player sends this effect for the coming chunk.**
    ///
    /// It used to be named `no_events` and was always empty, with the comment *"the player sends
    /// effects no events; its parameters move through its own editor, inside the plugin"*. That was
    /// true until the sequencer could automate an effect's parameters, which needs a channel from
    /// the host to the effect that its own editor is not.
    ///
    /// Fixed capacity, allocated at activation, cleared and refilled per chunk: the same shape the
    /// source's list has, and nothing here allocates.
    incoming: FixedEventBuffer,
    /// What the effect emits — parameter changes from its editor, mostly — read and discarded.
    /// The player shows no effect parameters, so there is nothing for these to update.
    emitted: FixedEventBuffer,
}

/// Everything a stage needs at construction, assembled on the GUI thread at activation.
pub struct StageConfig {
    pub id: FxId,
    pub owner: ProcessorOwner,
    pub envelope: EffectEnvelope,
    pub bypassed: Arc<AtomicBool>,
    pub shared: Arc<PlayerHostState>,
    pub tail: Option<PluginTail>,
    /// The plugin's `params` extension, for the flush a **bypassed** effect needs — see
    /// [`FxStage::flush_params`].
    pub params: Option<PluginParams>,
}

impl FxStage {
    /// Queues a modulation offset for this effect, for the coming chunk.
    ///
    /// Returns whether it fitted. A full buffer drops it and says so, exactly as the source's does:
    /// on this thread there is nowhere to report to, and dropping one offset costs a few
    /// milliseconds of the wrong value where panicking would cost the performance.
    pub fn push_param_mod(&mut self, frame: u32, id: ClapId, value: f32) -> bool {
        self.incoming.push_event(&ParamModEvent::new(
            frame,
            id,
            Pckn::match_all(),
            f64::from(value),
            clack_host::utils::Cookie::empty(),
        ))
    }

    /// Allocates the stage's buffers. GUI thread, at activation.
    pub fn new(config: StageConfig) -> Self {
        let input_channels = config.envelope.input.channel_count as usize;
        let output_channels = config.envelope.output.channel_count as usize;
        Self {
            id: config.id,
            owner: config.owner,
            input_channels,
            output_channels,
            bypassed: config.bypassed,
            was_bypassed: false,
            shared: config.shared,
            tail: config.tail,
            params: config.params,
            cached_tail: None,
            tail_frames_remaining: None,
            active: false,
            quiet_buffers: 0,
            steady_time: 0,
            input: vec![vec![0.0; MAX_BLOCK_FRAMES as usize]; input_channels],
            output: vec![vec![0.0; MAX_BLOCK_FRAMES as usize]; output_channels],
            input_ports: AudioPorts::with_capacity(input_channels, 1),
            output_ports: AudioPorts::with_capacity(output_channels, 1),
            incoming: FixedEventBuffer::new(),
            emitted: FixedEventBuffer::new(),
        }
    }

    pub fn output_channels(&self) -> usize {
        self.output_channels
    }

    /// Whether this stage would run on the next chunk even with silence coming in: it is on and
    /// has not slept. What decides whether the graph as a whole may sleep.
    pub fn is_open(&self) -> bool {
        !self.bypassed.load(Ordering::Relaxed) && self.active
    }

    /// Whether this effect has asked to be processed — see [`FxChain::wants_processing`].
    ///
    /// **Reads without consuming.** The flags are consumed in [`Self::process`], which is the
    /// place that satisfies them; taking them here would answer the question and then throw the
    /// answer away.
    ///
    /// **A bypassed effect counts.** It is not run, but it still has to be *reached*: its editor
    /// is openable and its knobs turn, and the only thing that applies what they ask for is the
    /// CLAP flush [`Self::flush_params`] performs — inside `process`, which a sleeping graph does
    /// not call. The first version of this excluded them, reasoning that an effect that is not
    /// called has nothing to flush into. That was wrong, and the owner found it: with the chorus
    /// switched off, its knobs moved under the pointer for a second and sprang back on release,
    /// because nothing ever applied the value (2026-09-04).
    fn wants_processing(&self) -> bool {
        !self.incoming.is_empty()
            || self.shared.requests.process.load(Ordering::Acquire)
            || self.shared.requests.param_flush.load(Ordering::Acquire)
    }

    /// Hands the plugin its own editor's queued parameter changes **without processing audio**.
    ///
    /// CLAP's `params.flush` exists for exactly this: a plugin that is active but not processing
    /// still has to take parameter changes, and a framework that queues them until the next
    /// `process` would otherwise never apply them. nice-plug is such a framework — it says so in
    /// `raw_set_parameter_normalized` — so without this a switched-off effect's editor is a row of
    /// controls that do nothing at all.
    ///
    /// `flush_active`, the audio-thread half, because the processor is active and this runs on the
    /// audio thread. Host automation is sent in too, including final modulation zeroes.
    /// What comes back is the plugin's own echo, dropped with the rest of its output events.
    fn flush_params(&mut self) {
        let Some(params) = self.params else {
            return;
        };
        self.emitted.clear();
        let Some(processor) = self.owner.processor_mut() else {
            return;
        };
        let mut handle = processor.plugin_handle();
        params.flush_active(
            &mut handle,
            &InputEvents::from_buffer(&self.incoming),
            &mut OutputEvents::from_buffer(&mut self.emitted),
        );
    }

    /// Copies `source` into this stage's input, adapting the channel count, and reports whether
    /// any sample was not exact zero.
    fn adapt_input(&mut self, source: &[Vec<f32>], frames: usize) -> bool {
        let mut nonzero = false;
        match (source.len(), self.input_channels) {
            (0, _) => {
                for channel in &mut self.input {
                    channel[..frames].fill(0.0);
                }
            }
            (2, 1) => {
                let (left, right) = (&source[0][..frames], &source[1][..frames]);
                for (i, out) in self.input[0][..frames].iter_mut().enumerate() {
                    *out = 0.5 * (left[i] + right[i]);
                    nonzero |= *out != 0.0;
                }
            }
            (1, 2) => {
                let mono = &source[0][..frames];
                for channel in &mut self.input {
                    channel[..frames].copy_from_slice(mono);
                }
                nonzero = mono.iter().any(|s| *s != 0.0);
            }
            _ => {
                for (channel, from) in self.input.iter_mut().zip(source.iter()) {
                    channel[..frames].copy_from_slice(&from[..frames]);
                    nonzero |= from[..frames].iter().any(|s| *s != 0.0);
                }
            }
        }
        nonzero
    }

    /// Runs the stage over one chunk, or skips it.
    ///
    /// Returns `None` when the signal passes untouched — bypassed, or asleep with silence coming
    /// in — and the time spent in the plugin otherwise, with the result in `self.output`.
    fn process(
        &mut self,
        source: &[Vec<f32>],
        frames: usize,
        transport: &TransportEvent,
    ) -> Option<Duration> {
        if self.bypassed.load(Ordering::Relaxed) {
            self.was_bypassed = true;
            // Off means the audio is not run. It does **not** mean the plugin stops existing: its
            // editor is open and its knobs turn, so what they queue is handed over here through
            // CLAP's own flush. See `flush_params`.
            let asked = self.shared.requests.process.swap(false, Ordering::AcqRel)
                | self
                    .shared
                    .requests
                    .param_flush
                    .swap(false, Ordering::AcqRel);
            if asked || !self.incoming.is_empty() {
                self.flush_params();
            }
            return None;
        }
        if self.was_bypassed {
            // Whatever it held is from before the gap. Cleared, and treated as freshly woken:
            // the input decides below whether it runs.
            self.was_bypassed = false;
            if let Some(processor) = self.owner.processor_mut() {
                processor.reset();
            }
            self.active = false;
            self.quiet_buffers = 0;
            self.tail_frames_remaining = None;
        }

        let nonzero = self.adapt_input(source, frames);
        // **Both taken, not read.** A latched flush would keep the effect awake for the rest of
        // the session — the collection's *doing nothing costs nothing* rule, lost to a `load`.
        // Processing this chunk is what satisfies the request, so this chunk owns it.
        let requested = self.shared.requests.process.swap(false, Ordering::AcqRel)
            | self
                .shared
                .requests
                .param_flush
                .swap(false, Ordering::AcqRel);
        if nonzero {
            // **The tail follows the input, so its countdown starts over on every chunk that
            // carries some.** Counting from the first `Tail` status put an effect that reports
            // `Tail` while it is still being fed — the fixture, and any delay — to sleep in the
            // middle of the note, found by the chain's bit-exact comparison.
            self.quiet_buffers = 0;
            self.tail_frames_remaining = None;
        }
        if nonzero || requested || !self.incoming.is_empty() {
            if !self.active {
                self.quiet_buffers = 0;
                self.tail_frames_remaining = None;
            }
            self.active = true;
        }
        if !self.active {
            return None;
        }

        for channel in &mut self.output {
            channel[..frames].fill(0.0);
        }
        self.emitted.clear();

        let started = Instant::now();
        let status = {
            let processor = self.owner.started_mut()?;
            let inputs = self.input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    self.input
                        .iter_mut()
                        .map(|c| InputChannel::from_buffer(&mut c[..frames], false)),
                ),
            }]);
            let mut outputs = self.output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(
                    self.output.iter_mut().map(|c| &mut c[..frames]),
                ),
            }]);
            processor.process(
                &inputs,
                &mut outputs,
                &InputEvents::from_buffer(&self.incoming),
                &mut OutputEvents::from_buffer(&mut self.emitted),
                Some(self.steady_time),
                Some(transport),
            )
        };
        let elapsed = started.elapsed();
        self.steady_time += frames as u64;

        match status {
            Ok(status) => self.apply_status(status, frames),
            Err(_) => {
                // Never called again in a tight loop; the signal passes it from here on.
                self.active = false;
                self.bypassed.store(true, Ordering::Relaxed);
                self.was_bypassed = true;
                for channel in &mut self.output {
                    channel[..frames].fill(0.0);
                }
                return None;
            }
        }
        Some(elapsed)
    }

    fn apply_status(&mut self, status: ProcessStatus, frames: usize) {
        match status {
            ProcessStatus::Continue => {
                self.quiet_buffers = 0;
                self.tail_frames_remaining = None;
            }
            ProcessStatus::ContinueIfNotQuiet => {
                let silent = self
                    .output
                    .iter()
                    .all(|c| c[..frames].iter().all(|s| *s == 0.0));
                if silent {
                    self.quiet_buffers += 1;
                    if self.quiet_buffers >= QUIET_BUFFERS_BEFORE_SLEEP {
                        self.active = false;
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
                self.active = false;
                self.quiet_buffers = 0;
                self.tail_frames_remaining = None;
            }
        }
    }

    /// The same rule the source follows: a finite tail counts down and sleeps at zero, and a
    /// `changed` notification throws the cached length away.
    fn update_tail(&mut self, frames: usize) {
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
                if remaining == 0 {
                    self.active = false;
                }
            }
            _ => self.tail_frames_remaining = None,
        }
    }

    /// A panic or a host reset: the processor's state goes, on the audio thread, as CLAP allows.
    fn reset(&mut self) {
        if let Some(processor) = self.owner.processor_mut() {
            processor.reset();
        }
        self.active = false;
        self.quiet_buffers = 0;
        self.tail_frames_remaining = None;
    }
}

/// The stages in order, run over one chunk.
pub struct FxChain {
    stages: Vec<FxStage>,
}

impl FxChain {
    pub fn new(stages: Vec<FxStage>) -> Self {
        Self { stages }
    }

    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// Empties every stage's incoming events, before a chunk fills them.
    ///
    /// Here rather than in each stage's own `process` because a **bypassed or sleeping stage never
    /// processes**, and one that kept last chunk's events would replay them whenever it woke — and
    /// because a buffer nobody empties is a buffer that fills, after which every later chunk's
    /// automation is silently refused.
    ///
    /// Called from `Processor::convert_events`, which is where the source's own list is cleared, so
    /// the two cannot drift apart.
    pub fn clear_incoming(&mut self) {
        for stage in &mut self.stages {
            stage.incoming.clear();
        }
    }

    /// The stage carrying `id`, if it is still in the chain.
    ///
    /// **Resolved by identity, never by position**, which is what makes a reference held outside
    /// the chain survive a removal or a reorder.
    pub fn stage_mut(&mut self, id: FxId) -> Option<&mut FxStage> {
        self.stages.iter_mut().find(|stage| stage.id == id)
    }

    /// Whether any stage would run on silence: the graph may not sleep while one would.
    pub fn any_open(&self) -> bool {
        self.stages.iter().any(FxStage::is_open)
    }

    /// Whether any effect has **asked** to be processed since it was last run.
    ///
    /// **The sleeping graph has to consult this, and for a while it did not.** A plugin whose
    /// editor moves a parameter queues the change and calls the host's `request_flush`; nice-plug
    /// only applies the value when that queue is written, which happens inside `process`. So an
    /// effect that is never processed never takes its own editor's edits — and the knob, redrawn
    /// each frame from a value that never moves, jitters under the pointer instead of turning.
    ///
    /// The source never had the fault because the worker consults *its* flags in the same breath
    /// as deciding to sleep. An effect's flags live behind its own host state, which nothing
    /// outside `FxStage::process` was reading — and `process` is exactly what a sleeping graph
    /// does not call. Reported by the owner against `mxm-chorus-06`, 2026-09-04.
    pub fn wants_processing(&self) -> bool {
        self.stages.iter().any(FxStage::wants_processing)
    }

    /// Runs every stage in order on `source`, the chunk the source produced.
    ///
    /// Returns which stage's output holds the result — `None` when every stage was skipped and
    /// the result is still `source` — and the time spent inside effects.
    pub fn run(
        &mut self,
        source: &[Vec<f32>],
        frames: usize,
        transport: &TransportEvent,
    ) -> (Option<usize>, Duration) {
        let mut current: Option<usize> = None;
        let mut elapsed = Duration::ZERO;
        for index in 0..self.stages.len() {
            let (earlier, rest) = self.stages.split_at_mut(index);
            let input: &[Vec<f32>] = match current {
                None => source,
                Some(j) => &earlier[j].output,
            };
            if let Some(time) = rest[0].process(input, frames, transport) {
                elapsed += time;
                current = Some(index);
            }
        }
        (current, elapsed)
    }

    /// The buffers a run left the result in.
    pub fn output(&self, stage: usize) -> &[Vec<f32>] {
        &self.stages[stage].output
    }

    /// Hands every stage's processor back through its own return queue, for the stop protocol.
    /// True only when all of them went.
    pub fn hand_back(&mut self) -> bool {
        let mut all = true;
        for stage in &mut self.stages {
            all &= stage.owner.hand_back();
        }
        all
    }

    pub fn reset_all(&mut self) {
        for stage in &mut self.stages {
            stage.reset();
        }
    }
}

/// Where the chain's output must go: the widest output any stage has, or the source's.
///
/// The device is opened for this before the stream starts, so a stereo chorus after a mono synth
/// gets two channels and a bypassed stage's absence is adapted in the interleave, not by
/// reopening the device.
pub fn widest_output(source_channels: u32, slots: &[FxSlot]) -> u32 {
    slots
        .iter()
        .map(|s| s.envelope.output.channel_count)
        .fold(source_channels, u32::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_is_opened_for_the_widest_output_in_the_chain() {
        // No slots: the source decides.
        assert_eq!(widest_output(1, &[]), 1);
        assert_eq!(widest_output(2, &[]), 2);
    }
}
