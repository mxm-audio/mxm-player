//! One plugin type, driven by a [`FixtureSpec`].
//!
//! Every fixture behaves identically except where its spec says otherwise, so the interesting
//! differences stay in `spec.rs` rather than being spread across ten near-identical plugins.

use crate::spec::{Behaviour, FixtureSpec};
use clack_extensions::audio_ports::{
    AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPorts, PluginAudioPortsImpl,
};
use clack_extensions::audio_ports_config::{
    AudioPortConfigWriter, AudioPortsConfiguration, MainPortInfo, PluginAudioPortsConfig,
    PluginAudioPortsConfigImpl,
};
use clack_extensions::log::{HostLog, LogSeverity};
use clack_extensions::note_ports::{
    NotePortInfo, NotePortInfoWriter, PluginNotePorts, PluginNotePortsImpl,
};
use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginAudioProcessorParams,
    PluginMainThreadParams, PluginParams,
};
use clack_extensions::tail::{HostTail, PluginTail, PluginTailImpl, TailLength};
use clack_plugin::events::event_types::{
    NoteChokeEvent, NoteExpressionEvent, NoteExpressionType, NoteOffEvent, NoteOnEvent,
    ParamGestureBeginEvent, ParamGestureEndEvent, ParamValueEvent,
};
use clack_plugin::prelude::*;
use clack_plugin::process::ConstantMask;
use clack_plugin::process::audio::ChannelPair;
use clack_plugin::utils::Cookie;
use std::ffi::CStr;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;

/// The parameter every fixture exposes. For `event-emitter` it also controls how many events are
/// emitted per buffer, which is how the host's output-sink overflow is exercised.
pub const PARAM_ID: u32 = 0;

/// The effect's echo is a single delay of exactly this many frames, at any sample rate.
pub const ECHO_DELAY_FRAMES: usize = 480;

/// Each echo is this fraction of the one before: the line stores `in + ECHO_FEEDBACK * delayed`.
pub const ECHO_FEEDBACK: f32 = 0.5;

/// What the effect reports through the `tail` extension, and how long its input must have been
/// silent before it asks to sleep: twelve echoes at 0.5 each is under -70 dB.
pub const EFFECT_TAIL_FRAMES: u32 = ECHO_DELAY_FRAMES as u32 * 12;

/// Shared, thread-safe fixture state.
pub struct FixtureShared {
    pub spec: &'static FixtureSpec,
    /// The single parameter's value, so main thread and audio thread agree without locking.
    value: AtomicU64,
    /// Set once the log-spam threads have been asked to stop.
    stop_logging: Arc<AtomicBool>,
    /// Whether `tail-shift` has flipped to an infinite tail yet.
    tail_infinite: Arc<AtomicBool>,
    /// Counts process() calls, so `tail-shift` can flip after a known number of buffers.
    buffers_processed: AtomicU32,
    /// How many times the host has actually run `on_main_thread`.
    main_thread_calls: AtomicU32,
    /// Whether the re-request made from inside `on_main_thread` has been issued yet.
    re_requested: AtomicBool,
    /// How many voices the `voice-counter` fixture believes it is holding.
    voices: AtomicU32,
}

impl FixtureShared {
    fn new(spec: &'static FixtureSpec) -> Self {
        Self {
            spec,
            value: AtomicU64::new(0.5f64.to_bits()),
            stop_logging: Arc::new(AtomicBool::new(false)),
            tail_infinite: Arc::new(AtomicBool::new(false)),
            buffers_processed: AtomicU32::new(0),
            main_thread_calls: AtomicU32::new(0),
            re_requested: AtomicBool::new(false),
            voices: AtomicU32::new(0),
        }
    }

    fn value(&self) -> f64 {
        f64::from_bits(self.value.load(Ordering::Relaxed))
    }

    fn set_value(&self, value: f64) {
        self.value.store(value.to_bits(), Ordering::Relaxed);
    }
}

impl<'a> PluginShared<'a> for FixtureShared {}

impl Drop for FixtureShared {
    fn drop(&mut self) {
        self.stop_logging.store(true, Ordering::Relaxed);
    }
}

/// Main-thread fixture state.
pub struct FixtureMainThread<'a> {
    shared: &'a FixtureShared,
    host: HostMainThreadHandle<'a>,
}

impl<'a> PluginMainThread<'a, FixtureShared> for FixtureMainThread<'a> {
    fn on_main_thread(&mut self) {
        if self.shared.spec.behaviour != Behaviour::MainThreadCallback {
            return;
        }
        self.shared
            .main_thread_calls
            .fetch_add(1, Ordering::Relaxed);

        // Ask again from *inside* the callback, exactly once. A correct host services that on a
        // later turn rather than recursing.
        if !self.shared.re_requested.swap(true, Ordering::Relaxed) {
            self.host.shared().request_callback();
        }
    }
}

/// The fixture plugin. All fixture IDs instantiate this same type.
pub struct Fixture;

impl Plugin for Fixture {
    type AudioProcessor<'a> = FixtureAudioProcessor<'a>;
    type Shared<'a> = FixtureShared;
    type MainThread<'a> = FixtureMainThread<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, shared: Option<&FixtureShared>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginNotePorts>()
            .register::<PluginParams>()
            .register::<PluginTail>();

        // Only the fixture that exists to advertise incompatible configurations implements
        // `audio-ports-config`; the rest are taken as their static `audio-ports` layout.
        if shared.is_some_and(|s| s.spec.configs.is_some()) {
            builder.register::<PluginAudioPortsConfig>();
        }
    }
}

/// Builds the shared state for a given fixture ID.
pub fn new_shared(spec: &'static FixtureSpec) -> FixtureShared {
    FixtureShared::new(spec)
}

/// Builds the main-thread state.
pub fn new_main_thread<'a>(
    host: HostMainThreadHandle<'a>,
    shared: &'a FixtureShared,
) -> FixtureMainThread<'a> {
    if shared.spec.behaviour == Behaviour::MainThreadCallback {
        host.shared().request_callback();
    }
    FixtureMainThread { shared, host }
}

// --- audio ports -----------------------------------------------------------------------------

impl PluginAudioPortsImpl for FixtureMainThread<'_> {
    fn count(&mut self, is_input: bool) -> u32 {
        let ports = if is_input {
            self.shared.spec.audio_inputs
        } else {
            self.shared.spec.audio_outputs
        };
        ports.len() as u32
    }

    fn get(&mut self, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        let ports = if is_input {
            self.shared.spec.audio_inputs
        } else {
            self.shared.spec.audio_outputs
        };
        let Some(port) = ports.get(index as usize) else {
            return;
        };
        let Some(id) = ClapId::from_raw(index) else {
            return;
        };

        // The effect processes in place correctly and says so: each of its ports pairs with the
        // port of the same index on the other side, when that one has the same channel count.
        // Without the declaration a host - clap-validator included - never shares a buffer, and
        // the in-place path would go unexercised. No other fixture declares a pair.
        let in_place_pair = if self.shared.spec.behaviour == Behaviour::Effect {
            let other = if is_input {
                self.shared.spec.audio_outputs
            } else {
                self.shared.spec.audio_inputs
            };
            other
                .get(index as usize)
                .filter(|o| o.channel_count == port.channel_count)
                .map(|_| id)
        } else {
            None
        };

        writer.set(&AudioPortInfo {
            id,
            name: port.name.as_bytes(),
            channel_count: port.channel_count,
            flags: port.flags(),
            port_type: AudioPortType::from_channel_count(port.channel_count),
            in_place_pair,
        });
    }
}

// --- audio ports config ----------------------------------------------------------------------

impl PluginAudioPortsConfigImpl for FixtureMainThread<'_> {
    fn count(&mut self) -> u32 {
        self.shared
            .spec
            .configs
            .map(|c| c.len() as u32)
            .unwrap_or(0)
    }

    fn get(&mut self, index: u32, writer: &mut AudioPortConfigWriter) {
        let Some(configs) = self.shared.spec.configs else {
            return;
        };
        let Some(config) = configs.get(index as usize) else {
            return;
        };
        let Some(id) = ClapId::from_raw(config.id) else {
            return;
        };

        writer.write(&AudioPortsConfiguration {
            id,
            name: config.name.as_bytes(),
            input_port_count: config.input_port_count,
            output_port_count: config.output_port_count,
            main_input: None,
            main_output: config.main_output_channels.map(|channels| MainPortInfo {
                channel_count: channels,
                port_type: AudioPortType::from_channel_count(channels),
            }),
        });
    }

    fn select(&mut self, _config_id: ClapId) -> Result<(), PluginError> {
        // Every advertised configuration is deliberately outside the player's envelope, so a
        // correct host never gets here. Accepting the call keeps the failure the host's decision.
        Ok(())
    }
}

// --- note ports ------------------------------------------------------------------------------

impl PluginNotePortsImpl for FixtureMainThread<'_> {
    fn count(&mut self, is_input: bool) -> u32 {
        let decl = if is_input {
            self.shared.spec.note_input
        } else {
            self.shared.spec.note_output
        };
        u32::from(decl.is_some())
    }

    fn get(&mut self, index: u32, is_input: bool, writer: &mut NotePortInfoWriter) {
        if index != 0 {
            return;
        }
        let decl = if is_input {
            self.shared.spec.note_input
        } else {
            self.shared.spec.note_output
        };
        let Some(decl) = decl else { return };

        writer.set(&NotePortInfo {
            id: ClapId::new(0),
            name: if is_input { b"Notes in" } else { b"Notes out" },
            supported_dialects: decl.dialects(),
            preferred_dialect: Some(decl.preferred()),
        });
    }
}

// --- params ----------------------------------------------------------------------------------

impl PluginMainThreadParams for FixtureMainThread<'_> {
    fn count(&mut self) -> u32 {
        1
    }

    fn get_info(&mut self, param_index: u32, info: &mut ParamInfoWriter) {
        if param_index != 0 {
            return;
        }
        info.set(&ParamInfo {
            id: ClapId::new(PARAM_ID),
            flags: ParamInfoFlags::IS_AUTOMATABLE,
            cookie: Cookie::empty(),
            name: b"Amount",
            module: b"",
            min_value: 0.0,
            max_value: 1.0,
            default_value: 0.5,
        });
    }

    fn get_value(&mut self, param_id: ClapId) -> Option<f64> {
        if param_id != ClapId::new(PARAM_ID) {
            return None;
        }
        // The main-thread fixture reports its callback count here, so a host can observe it
        // through the params extension alone.
        if self.shared.spec.behaviour == Behaviour::MainThreadCallback {
            return Some(f64::from(
                self.shared.main_thread_calls.load(Ordering::Relaxed),
            ));
        }
        // The voice counter reports what it is still holding, the same way.
        if self.shared.spec.behaviour == Behaviour::VoiceCounter {
            return Some(f64::from(self.shared.voices.load(Ordering::Relaxed)));
        }
        Some(self.shared.value())
    }

    fn value_to_text(
        &mut self,
        param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> core::fmt::Result {
        if param_id != ClapId::new(PARAM_ID) {
            return Err(core::fmt::Error);
        }
        write!(writer, "{value:.3}")
    }

    fn text_to_value(&mut self, param_id: ClapId, text: &CStr) -> Option<f64> {
        if param_id != ClapId::new(PARAM_ID) {
            return None;
        }
        text.to_str().ok()?.trim().parse().ok()
    }

    fn flush(&mut self, input_parameter_changes: &InputEvents, _out: &mut OutputEvents) {
        apply_param_events(self.shared, input_parameter_changes);
    }
}

impl PluginAudioProcessorParams for FixtureAudioProcessor<'_> {
    fn flush(&mut self, input_parameter_changes: &InputEvents, _out: &mut OutputEvents) {
        apply_param_events(self.shared, input_parameter_changes);
    }
}

fn apply_param_events(shared: &FixtureShared, events: &InputEvents) {
    for event in events {
        if let Some(event) = event.as_event::<ParamValueEvent>() {
            shared.set_value(event.value());
        }
    }
}

// --- audio processor -------------------------------------------------------------------------

pub struct FixtureAudioProcessor<'a> {
    shared: &'a FixtureShared,
    host: HostAudioProcessorHandle<'a>,
    host_tail: Option<HostTail>,
    /// Rising sample counter, so the CPU-load fixture's work is not optimised away.
    accumulator: f64,
    /// The `log-spam` fixture's two concurrent logging threads, joined on drop.
    log_threads: Vec<JoinHandle<()>>,
    /// The effect's delay lines, one per output channel across every port, sized in `activate`
    /// so that `process` allocates nothing. Empty for every other behaviour.
    echo_lines: Vec<EchoLine>,
    /// The ring position every line writes at next.
    echo_write: usize,
    /// Frames since the last nonzero input sample, saturating. Starts past the tail: a fresh
    /// line has nothing to ring.
    frames_since_input: u32,
}

/// One channel's echo line, exactly as long as the delay: the slot about to be written is the
/// one written `ECHO_DELAY_FRAMES` frames ago.
#[derive(Clone, Copy)]
struct EchoLine {
    samples: [f32; ECHO_DELAY_FRAMES],
}

impl EchoLine {
    const SILENT: Self = Self {
        samples: [0.0; ECHO_DELAY_FRAMES],
    };

    /// Runs one sample: `out = amount * x + ECHO_FEEDBACK * d`, where `d` is what the line held
    /// `ECHO_DELAY_FRAMES` frames ago, and the line takes `x + ECHO_FEEDBACK * d` in its place -
    /// which is what makes each echo half the last.
    #[inline]
    fn step(&mut self, write: usize, amount: f32, x: f32) -> f32 {
        let d = self.samples[write];
        self.samples[write] = x + ECHO_FEEDBACK * d;
        amount * x + ECHO_FEEDBACK * d
    }
}

impl Drop for FixtureAudioProcessor<'_> {
    fn drop(&mut self) {
        self.shared.stop_logging.store(true, Ordering::Relaxed);
        for handle in self.log_threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl<'a> PluginAudioProcessor<'a, FixtureShared, FixtureMainThread<'a>>
    for FixtureAudioProcessor<'a>
{
    fn activate(
        host: HostAudioProcessorHandle<'a>,
        main_thread: &mut FixtureMainThread<'a>,
        shared: &'a FixtureShared,
        _audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        let log_threads = if shared.spec.behaviour == Behaviour::LogSpam {
            shared.stop_logging.store(false, Ordering::Relaxed);
            start_log_threads(main_thread.host.shared(), shared.stop_logging.clone())
        } else {
            Vec::new()
        };

        // The effect's only allocation: one line per output channel, here rather than in
        // `process`.
        let echo_lines = if shared.spec.behaviour == Behaviour::Effect {
            let channels: usize = shared
                .spec
                .audio_outputs
                .iter()
                .map(|port| port.channel_count as usize)
                .sum();
            vec![EchoLine::SILENT; channels]
        } else {
            Vec::new()
        };

        Ok(Self {
            shared,
            host_tail: host.get_extension(),
            host,
            accumulator: 0.0,
            log_threads,
            echo_lines,
            echo_write: 0,
            frames_since_input: EFFECT_TAIL_FRAMES + 1,
        })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        apply_param_events(self.shared, events.input);

        let frames = audio.frames_count();
        // Every fixture but the effect renders silence. The effect reads its input first, which
        // a zero-fill would destroy when the host processes in place.
        if self.shared.spec.behaviour != Behaviour::Effect {
            for mut port in audio.output_ports() {
                let Some(channels) = port.channels()?.into_f32() else {
                    continue;
                };
                for channel in channels {
                    channel.fill(0.0);
                }
            }
        }

        match self.shared.spec.behaviour {
            Behaviour::Silent => Ok(ProcessStatus::Sleep),
            Behaviour::EventEmitter => {
                self.emit_events(events.output, frames);
                Ok(ProcessStatus::Continue)
            }
            Behaviour::CpuLoad => {
                self.burn(frames);
                Ok(ProcessStatus::Continue)
            }
            Behaviour::Hang => {
                // Deliberately never returns. Only ever loaded in a subprocess.
                loop {
                    std::hint::spin_loop();
                }
            }
            Behaviour::LogSpam => Ok(ProcessStatus::Continue),
            Behaviour::MainThreadCallback => Ok(ProcessStatus::Continue),
            Behaviour::VoiceCounter => {
                self.count_voices(events.input);
                Ok(ProcessStatus::Continue)
            }
            Behaviour::TailShift => {
                let n = self
                    .shared
                    .buffers_processed
                    .fetch_add(1, Ordering::Relaxed);
                // Flip to an infinite tail once, after a handful of buffers, and tell the host:
                // a host caching the earlier finite value would truncate the tail.
                if n == 8 && !self.shared.tail_infinite.swap(true, Ordering::Relaxed) {
                    if let Some(tail) = self.host_tail {
                        tail.changed(&mut self.host);
                    }
                }
                Ok(ProcessStatus::Tail)
            }
            Behaviour::Effect => self.process_effect(&mut audio),
        }
    }

    fn reset(&mut self) {
        // Only the effect holds processing state worth the name. A cleared line and a counter
        // past the tail are exactly what `activate` starts with, so a host cannot tell a reset
        // from a reactivation - which is what `clap_plugin::reset` promises.
        if self.shared.spec.behaviour == Behaviour::Effect {
            self.echo_lines.fill(EchoLine::SILENT);
            self.echo_write = 0;
            self.frames_since_input = EFFECT_TAIL_FRAMES + 1;
        }
    }
}

impl FixtureAudioProcessor<'_> {
    /// The conforming effect: `out[c][i] = amount * in[c][i] + 0.5 * line[c][i - 480]`, with
    /// the line taking `in + 0.5 * delayed` in its place, so every echo is half the last and the
    /// twelfth is under -70 dB. `amount` is the `Amount` parameter.
    ///
    /// Each input sample is read into a local before its output is written, so in-place
    /// processing - the host handing one buffer as both - is arithmetically identical to
    /// out-of-place. An output channel with no input of its own takes channel 0's, and runs
    /// before channel 0 does, while an in-place channel 0 still holds its input.
    ///
    /// The input's constant mask is deliberately not consulted: CLAP makes checking it optional,
    /// which obliges the host to fill a constant channel anyway.
    ///
    /// Returns `Tail` while the input is live or an echo could still be audible, and `Sleep` once
    /// the input has been silent for longer than the tail. The lines then hold under -70 dB of
    /// anything and are snapped to digital silence, so the sleep is exact and nothing is left to
    /// decay into subnormals.
    fn process_effect(&mut self, audio: &mut Audio) -> Result<ProcessStatus, PluginError> {
        let amount = self.shared.value() as f32;
        let frames = audio.frames_count() as usize;
        let was_sleeping = self.sleeping();
        let write_start = self.echo_write;
        // The index of the last nonzero input sample this block, across every channel.
        let mut last_input: Option<usize> = None;
        // Lines are laid out port after port, output channel after output channel.
        let mut next_line = 0;

        for mut port in audio.port_pairs() {
            let Some(mut channels) = port.channels()?.into_f32() else {
                continue;
            };
            let inputs = channels.input_channel_count();
            let outputs = channels.output_channel_count();
            let port_lines = next_line;
            next_line += outputs;

            // Channels without an input of their own first, then the paired ones.
            for c in (inputs..outputs).chain(0..inputs.min(outputs)) {
                let Some(line) = self.echo_lines.get_mut(port_lines + c) else {
                    break;
                };
                let Some(pair) = channels.channel_pair(c) else {
                    break;
                };

                let mut write = write_start;
                let mut run = |i: usize, x: f32| -> f32 {
                    if x != 0.0 {
                        last_input = Some(i);
                    }
                    let y = line.step(write, amount, x);
                    write = if write + 1 == ECHO_DELAY_FRAMES {
                        0
                    } else {
                        write + 1
                    };
                    y
                };

                match pair {
                    ChannelPair::InPlace(io) => {
                        for (i, sample) in io.iter_mut().enumerate() {
                            let x = *sample;
                            *sample = run(i, x);
                        }
                    }
                    ChannelPair::InputOutput(input, output) => {
                        for (i, (x, out)) in input.iter().zip(output.iter_mut()).enumerate() {
                            *out = run(i, *x);
                        }
                    }
                    ChannelPair::OutputOnly(output) => {
                        // Channel 0's input, when the port has one; the echo of nothing
                        // otherwise. With inputs present, `c >= inputs >= 1`, so channel 0 is
                        // never `output` itself and nothing aliases.
                        let source = if inputs > 0 {
                            channels.channel_pair(0)
                        } else {
                            None
                        };
                        let source: &[f32] = match &source {
                            Some(ChannelPair::InputOnly(input))
                            | Some(ChannelPair::InputOutput(input, _)) => input,
                            Some(ChannelPair::InPlace(io)) => io,
                            _ => &[],
                        };
                        for (i, out) in output.iter_mut().enumerate() {
                            let x = source.get(i).copied().unwrap_or(0.0);
                            *out = run(i, x);
                        }
                    }
                    // More inputs than outputs: nothing to write.
                    ChannelPair::InputOnly(_) => {}
                }
            }
        }

        self.echo_write = (write_start + frames) % ECHO_DELAY_FRAMES;
        self.frames_since_input = match last_input {
            Some(i) => (frames - 1 - i) as u32,
            None => self.frames_since_input.saturating_add(frames as u32),
        };

        let sleeping = self.sleeping();
        if sleeping {
            self.echo_lines.fill(EchoLine::SILENT);
        }

        // Asleep at both ends of the block with nothing fed in: every output sample was exactly
        // zero, and only then may the host be told so.
        let constant = if was_sleeping && last_input.is_none() {
            ConstantMask::FULLY_CONSTANT
        } else {
            ConstantMask::FULLY_DYNAMIC
        };
        for mut port in audio.output_ports() {
            port.set_constant_mask(constant);
        }

        Ok(if sleeping {
            ProcessStatus::Sleep
        } else {
            ProcessStatus::Tail
        })
    }

    /// Whether the effect's input has been silent for longer than its tail.
    fn sleeping(&self) -> bool {
        self.frames_since_input > EFFECT_TAIL_FRAMES
    }

    /// Tracks what this fixture is holding, so a host's recovery is provable rather than assumed.
    ///
    /// A wildcard choke — the only global recovery a CLAP-only note port can receive — clears
    /// everything, which is exactly the claim the player needs verified.
    fn count_voices(&mut self, events: &InputEvents) {
        for event in events {
            if event.as_event::<NoteOnEvent>().is_some() {
                self.shared.voices.fetch_add(1, Ordering::Relaxed);
            } else if event.as_event::<NoteOffEvent>().is_some() {
                let _ =
                    self.shared
                        .voices
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                            Some(v.saturating_sub(1))
                        });
            } else if let Some(choke) = event.as_event::<NoteChokeEvent>() {
                let pckn = choke.pckn();
                if pckn.raw_key() < 0 && pckn.raw_channel() < 0 {
                    // Wildcard: everything goes.
                    self.shared.voices.store(0, Ordering::Relaxed);
                } else {
                    let _ = self.shared.voices.fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |v| Some(v.saturating_sub(1)),
                    );
                }
            }
        }
    }

    /// Emits one of each interesting output-event kind, then as many further events as the
    /// `Amount` parameter asks for, which is how the host's bounded sink is driven to overflow.
    fn emit_events(&mut self, output: &mut OutputEvents, frames: u32) {
        let param = ClapId::new(PARAM_ID);
        let pckn = Pckn::new(0u16, 0u16, 60u16, 0u32);

        output.try_push(ParamGestureBeginEvent::new(0, param)).ok();
        output
            .try_push(ParamValueEvent::new(
                0,
                param,
                Pckn::match_all(),
                self.shared.value(),
                Cookie::empty(),
            ))
            .ok();
        output.try_push(ParamGestureEndEvent::new(0, param)).ok();

        output.try_push(NoteOnEvent::new(0, pckn, 1.0)).ok();
        output
            .try_push(NoteOffEvent::new(frames.saturating_sub(1), pckn, 0.0))
            .ok();

        // Deliberately not representable in MIDI 1: the host must count it, not drop it silently.
        output
            .try_push(NoteExpressionEvent::new(
                0,
                pckn,
                NoteExpressionType::Volume,
                0.5,
            ))
            .ok();

        // The flood, scaled by the parameter.
        let flood = (self.shared.value() * 4096.0) as u32;
        for i in 0..flood {
            let time = i % frames.max(1);
            if output.try_push(NoteOnEvent::new(time, pckn, 1.0)).is_err() {
                break;
            }
        }
    }

    /// A calibrated busy loop: `Amount` scales the number of multiply-adds per frame, so the
    /// player's CPU meters can be validated against a known quantity of work.
    fn burn(&mut self, frames: u32) {
        let per_frame = (self.shared.value() * 2000.0) as u32;
        let mut acc = self.accumulator;
        for _ in 0..frames {
            for i in 0..per_frame {
                acc = acc.mul_add(1.000_000_1, f64::from(i) * 1e-9);
            }
        }
        // Keep the result live so the optimiser cannot delete the loop.
        self.accumulator = if acc.is_finite() { acc } else { 0.0 };
    }
}

impl PluginTailImpl for FixtureAudioProcessor<'_> {
    fn get(&self) -> TailLength {
        if self.shared.spec.behaviour == Behaviour::Effect {
            return TailLength::Finite(EFFECT_TAIL_FRAMES);
        }
        if self.shared.spec.behaviour != Behaviour::TailShift {
            return TailLength::default();
        }
        if self.shared.tail_infinite.load(Ordering::Relaxed) {
            TailLength::Infinite
        } else {
            TailLength::Finite(4800)
        }
    }
}

/// Spawns the two threads that log concurrently, which is the whole point of the `log-spam`
/// fixture: a host transport that is only single-producer safe will lose messages or worse.
///
/// The handles are joined in [`FixtureAudioProcessor::drop`], which runs during `deactivate`,
/// while the plugin instance - and therefore the host handle - is still alive.
fn start_log_threads(host: HostSharedHandle<'_>, stop: Arc<AtomicBool>) -> Vec<JoinHandle<()>> {
    let Some(log) = host.get_extension::<HostLog>() else {
        return Vec::new();
    };

    // SAFETY: `HostSharedHandle` is `Send + Sync` and only exposes thread-safe host operations.
    // The borrow is extended to `'static` solely so the handle can cross a `thread::spawn`
    // boundary; both threads are joined in `Drop` before the instance is destroyed, so the
    // handle never outlives what it points at.
    let host: HostSharedHandle<'static> = unsafe { core::mem::transmute(host) };

    (0..2u32)
        .map(|thread| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let message = if thread == 0 {
                    c"log-spam thread 0: the quick brown fox jumps over the lazy dog"
                } else {
                    c"log-spam thread 1: sphinx of black quartz, judge my vow"
                };
                while !stop.load(Ordering::Relaxed) {
                    log.log(&host, LogSeverity::Info, message);
                }
            })
        })
        .collect()
}
