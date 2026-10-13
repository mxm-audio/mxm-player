//! Offline rendering: load a bundle, drive it with a scripted event list, keep the samples.
//!
//! This is what proves the hosting API before any audio device or window exists, and it is what
//! the P0 fixture compares against a direct `mxm-mono-01-dsp` render.

use crate::envelope::{
    AudioLayout, Dialect, EffectEnvelope, Envelope, Refusal, negotiate, negotiate_effect,
    negotiate_note_port, read_static_layout,
};
use crate::host::{
    HostAudioProcessor, HostMainThread, HostShared, MxmHost, PlayerHostState, host_info,
};
use clack_extensions::audio_ports::PluginAudioPorts;
use clack_extensions::audio_ports_config::{AudioPortsConfigBuffer, PluginAudioPortsConfig};
use clack_extensions::note_ports::PluginNotePorts;
use clack_host::events::event_types::{
    MidiEvent, NoteOffEvent, NoteOnEvent, ParamModEvent, ParamValueEvent,
};
use clack_host::prelude::*;
use clack_host::utils::Cookie;
use std::fmt;
use std::path::{Path, PathBuf};

/// One event to deliver at an absolute frame position.
#[derive(Copy, Clone, Debug)]
pub struct ScheduledEvent {
    pub frame: u64,
    pub kind: EventKind,
}

/// The event kinds the offline driver can produce. Deliberately small: this is a rendering
/// harness, not the player's full event path.
#[derive(Copy, Clone, Debug)]
pub enum EventKind {
    /// CLAP velocity is normalised (`0.0..=1.0`), *not* a MIDI integer.
    NoteOn {
        channel: u16,
        key: u16,
        velocity: f64,
        note_id: u32,
    },
    NoteOff {
        channel: u16,
        key: u16,
        velocity: f64,
        note_id: u32,
    },
    /// A raw MIDI 1 message, for plugins negotiated on the MIDI dialect.
    Midi { data: [u8; 3] },
    /// A modulation offset, for the deviation a step applies.
    ///
    /// **An offset, not a value**, exactly as the live path sends it: laid over the parameter
    /// without disturbing it, so the restored patch stays the patch for the whole render. An export
    /// that set values instead would drift away from the patch it was given, and would stop sounding
    /// like the thing being listened to — which is the one promise export makes.
    ///
    /// **Not a gesture.** A lock is automation, not somebody holding a knob, so no begin/end
    /// brackets it — the same rule the live path follows.
    ParamMod { param_id: u32, value: f64 },
    /// A parameter value, replacing whatever the parameter was set to.
    ///
    /// **The export does not use this** — a step's deviation is modulation. It is kept because the
    /// offline driver is a scripted-event harness rather than the export's private helper, and
    /// because being able to send the same number both ways is what lets a test prove which one the
    /// plugin actually received. Nothing above this layer can tell them apart: `clap_params.get_value`
    /// reports the modulated value either way.
    ParamValue { param_id: u32, value: f64 },
}

/// Everything an offline render produced.
pub struct RenderResult {
    /// One `Vec<f32>` per output channel, `frames` long.
    pub channels: Vec<Vec<f32>>,
    pub envelope: Envelope,
    /// The process statuses the plugin returned, in order, one per block.
    pub statuses: Vec<ProcessStatus>,
    /// Events accepted by the attached output-event sink across the render.
    pub output_event_count: u64,
}

/// Why an offline render could not run.
#[derive(Debug)]
pub enum RenderError {
    /// The plugin does not implement the `state` extension, so its patch cannot be moved to a
    /// second instance. Exporting a default patch instead would be worse than refusing.
    NoState,
    /// The plugin refused the state it was handed.
    StateRefused(String),
    Load(String),
    NoPluginFactory,
    PluginNotFound(String),
    Refused(Refusal),
    Instance(PluginInstanceError),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::NoState => write!(
                f,
                "this plugin does not support saving its state, so its current sound cannot be                  rendered separately"
            ),
            RenderError::StateRefused(why) => {
                write!(f, "the plugin would not accept its own saved state: {why}")
            }
            RenderError::Load(e) => write!(f, "could not load the bundle: {e}"),
            RenderError::NoPluginFactory => f.write_str("the bundle exposes no plugin factory"),
            RenderError::PluginNotFound(id) => write!(f, "the bundle contains no plugin `{id}`"),
            RenderError::Refused(r) => write!(f, "outside the v1 envelope: it {r}"),
            RenderError::Instance(e) => write!(f, "the plugin instance failed: {e}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<PluginInstanceError> for RenderError {
    fn from(e: PluginInstanceError) -> Self {
        RenderError::Instance(e)
    }
}

/// How an offline render is configured.
// Not `Copy`: it carries the state bytes to restore, which are owned.
#[derive(Clone, Debug)]
pub struct RenderConfig {
    pub sample_rate: f64,
    /// Both `min_frames_count` and `max_frames_count`: a fixed block size makes the render
    /// reproducible, which is the point of the P0 comparison.
    pub block_size: u32,
    pub total_frames: u64,
    /// CLAP state to restore before activation, so the render is of a **particular patch** rather
    /// than the plugin's defaults.
    ///
    /// Without this an export sounds nothing like what the user was listening to: a bundle path
    /// and a plugin id give you a freshly instantiated plugin, and nothing else.
    pub state: Option<Vec<u8>>,
    /// The tempo of a transport that starts playing at beat 0 with the render, or `None` for no
    /// transport at all.
    ///
    /// **Export supplies one and nothing else does.** A tempo-synced LFO or delay follows the host's
    /// tempo, so an export rendered without it runs at its free rate while live playback runs synced
    /// (`crate::events::transport`). Every other caller — host tests, golden renders — renders as it
    /// always has, with no transport.
    pub tempo: Option<f64>,
}

/// One effect a render goes through after the source: where it comes from and the patch to
/// restore into it.
///
/// An effect that is **off** is not listed — the caller leaves it out, as the live chain leaves
/// it uncalled. `state: None` renders the effect's default patch, which for an effect without the
/// state extension is the only patch it has.
#[derive(Clone, Debug)]
pub struct EffectSpec {
    pub bundle: PathBuf,
    pub plugin_id: String,
    pub state: Option<Vec<u8>>,
    /// This effect's own automation, already resolved to *its* parameter ids.
    ///
    /// **The split happens where the chain is known**, so nothing here has to understand targets:
    /// an effect is rendered with its events exactly as the source is with its own. Without this a
    /// rendered file would silently lack every effect's automation while the live path had it,
    /// which is the one difference nobody would hear until they had shipped the render.
    pub events: Vec<ScheduledEvent>,
}

/// A fresh, deactivated instance of `plugin_id` from `entry`, and the host state it reports into.
///
/// **The one place the player makes a plugin instance**: the live engine, the offline render,
/// discovery and `mxm-player-harness` all come here, so the descriptor lookup and the reason a
/// plugin cannot be created are said one way. A caller that has no use for the host state drops it.
pub fn instantiate_with_state(
    entry: &PluginEntry,
    plugin_id: &str,
) -> Result<(PluginInstance<MxmHost>, std::sync::Arc<PlayerHostState>), RenderError> {
    let factory = entry
        .get_plugin_factory()
        .ok_or(RenderError::NoPluginFactory)?;

    let descriptor = factory
        .plugin_descriptors()
        .find(|d| d.id().map(|id| id.to_bytes()) == Some(plugin_id.as_bytes()))
        .ok_or_else(|| RenderError::PluginNotFound(plugin_id.to_owned()))?;
    let id = descriptor
        .id()
        .ok_or_else(|| RenderError::PluginNotFound(plugin_id.to_owned()))?
        .to_owned();

    let state = std::sync::Arc::new(PlayerHostState::new());
    let for_plugin = std::sync::Arc::clone(&state);
    let instance = PluginInstance::<MxmHost>::new(
        move |_| HostShared::new(for_plugin),
        |shared| HostMainThread::new(shared),
        entry,
        &id,
        &host_info(),
    )?;
    Ok((instance, state))
}

/// [`instantiate_with_state`] for a caller with no use for the host state.
fn instantiate(
    entry: &PluginEntry,
    plugin_id: &str,
) -> Result<PluginInstance<MxmHost>, RenderError> {
    instantiate_with_state(entry, plugin_id).map(|(instance, _)| instance)
}

/// Restores a patch into a deactivated instance.
///
/// Before negotiation and activation, because a patch can change what the plugin reports about
/// itself — and because `state.load` is a main-thread call.
fn restore(instance: &mut PluginInstance<MxmHost>, bytes: &[u8]) -> Result<(), RenderError> {
    let ext: clack_extensions::state::PluginState = instance
        .plugin_shared_handle()
        .get_extension()
        .ok_or(RenderError::NoState)?;
    let mut handle = instance.plugin_handle();
    let mut cursor = std::io::Cursor::new(bytes);
    ext.load(&mut handle, &mut cursor)
        .map_err(|e| RenderError::StateRefused(e.to_string()))
}

/// Loads a bundle, instantiates one plugin from it, and renders `total_frames` frames.
///
/// The plugin is activated, `start_processing()` is called before the first block and
/// `stop_processing()` before deactivation, exactly as a real host must.
pub fn render(
    bundle: &Path,
    plugin_id: &str,
    config: RenderConfig,
    events: &[ScheduledEvent],
) -> Result<RenderResult, RenderError> {
    render_inner(bundle, plugin_id, config, events, None, None)
}

/// Renders one advertised source configuration by index, optionally feeding its mono auxiliary
/// input. This is the bundle-level host proof for layouts the player's production source slot
/// deliberately does not select.
pub fn render_configuration(
    bundle: &Path,
    plugin_id: &str,
    config: RenderConfig,
    events: &[ScheduledEvent],
    configuration_index: u32,
    auxiliary_input: Option<&[f32]>,
) -> Result<RenderResult, RenderError> {
    render_inner(
        bundle,
        plugin_id,
        config,
        events,
        Some(configuration_index),
        auxiliary_input,
    )
}

fn render_inner(
    bundle: &Path,
    plugin_id: &str,
    config: RenderConfig,
    events: &[ScheduledEvent],
    configuration_index: Option<u32>,
    auxiliary_input: Option<&[f32]>,
) -> Result<RenderResult, RenderError> {
    // SAFETY: loading a CLAP bundle runs arbitrary code from the file. That is inherent to
    // hosting; the caller decides which bundles are trustworthy. Fixtures are loaded by path,
    // never through the scanner.
    let entry =
        unsafe { crate::entry::load(bundle) }.map_err(|e| RenderError::Load(e.to_string()))?;
    let mut instance = instantiate(&entry, plugin_id)?;

    if let Some(bytes) = config.state.as_deref() {
        restore(&mut instance, bytes)?;
    }

    let (envelope, auxiliary_ports) = match configuration_index {
        Some(index) => select_source_configuration(&mut instance, index)?,
        None => (negotiate(&mut instance).map_err(RenderError::Refused)?, 0),
    };
    if auxiliary_ports == 0 && auxiliary_input.is_some() {
        return Err(RenderError::Load(
            "audio was supplied to a configuration with no auxiliary input".to_owned(),
        ));
    }
    if auxiliary_ports > 0
        && auxiliary_input.is_some_and(|input| input.len() < config.total_frames as usize)
    {
        return Err(RenderError::Load(
            "the auxiliary input is shorter than the requested render".to_owned(),
        ));
    }
    let channel_count = envelope.audio.channel_count as usize;

    let audio_config = PluginAudioConfiguration {
        sample_rate: config.sample_rate,
        min_frames_count: config.block_size,
        max_frames_count: config.block_size,
    };
    let processor = instance.activate(|shared, _| HostAudioProcessor::new(shared), audio_config)?;

    let mut output_channels: Vec<Vec<f32>> =
        vec![Vec::with_capacity(config.total_frames as usize); channel_count];
    let mut statuses = Vec::new();

    {
        let mut processor = processor
            .start_processing()
            .map_err(|e| RenderError::Instance(e.into()))?;

        let mut block_buffers: Vec<Vec<f32>> =
            vec![vec![0.0; config.block_size as usize]; channel_count];
        let mut output_ports = AudioPorts::with_capacity(channel_count, 1);
        let mut input_ports = AudioPorts::with_capacity(1, auxiliary_ports);
        let mut input_block = vec![0.0f32; config.block_size as usize];
        let mut input_events = EventBuffer::new();
        let mut output_events = EventBuffer::new();
        let mut output_event_count = 0u64;

        let mut frame = 0u64;
        while frame < config.total_frames {
            let block_frames = config.block_size.min((config.total_frames - frame) as u32);

            input_events.clear();
            for event in events
                .iter()
                .filter(|e| e.frame >= frame && e.frame < frame + u64::from(block_frames))
            {
                let time = (event.frame - frame) as u32;
                push_event(&mut input_events, time, event.kind, &envelope);
            }
            output_events.clear();

            for buffer in &mut block_buffers {
                buffer[..block_frames as usize].fill(0.0);
            }

            let input_audio = if auxiliary_ports == 1 {
                if let Some(source) = auxiliary_input {
                    let from = frame as usize;
                    input_block[..block_frames as usize]
                        .copy_from_slice(&source[from..from + block_frames as usize]);
                } else {
                    input_block[..block_frames as usize].fill(0.0);
                }
                input_ports.with_input_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only([InputChannel::from_buffer(
                        &mut input_block[..block_frames as usize],
                        false,
                    )]),
                }])
            } else {
                InputAudioBuffers::empty()
            };
            let mut audio_outputs = output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(
                    block_buffers
                        .iter_mut()
                        .map(|b| &mut b[..block_frames as usize]),
                ),
            }]);

            output_events.clear();
            let transport = config.tempo.map(|tempo| {
                crate::events::transport::playing_from_start(frame, config.sample_rate, tempo)
            });
            let status = processor.process(
                &input_audio,
                &mut audio_outputs,
                &InputEvents::from_buffer(&input_events),
                &mut OutputEvents::from_buffer(&mut output_events),
                Some(frame),
                transport.as_ref(),
            )?;
            statuses.push(status);
            output_event_count += u64::from(output_events.len());

            for (channel, buffer) in output_channels.iter_mut().zip(block_buffers.iter()) {
                channel.extend_from_slice(&buffer[..block_frames as usize]);
            }

            frame += u64::from(block_frames);
        }

        let processor = processor.stop_processing();
        instance.deactivate(processor);

        Ok(RenderResult {
            channels: output_channels,
            envelope,
            statuses,
            output_event_count,
        })
    }
}

/// Selects one exact advertised configuration and validates the source shape used by this
/// verification harness: no main input, optionally one mono auxiliary, one mono/stereo main output
/// and zero advertised note outputs. Unlike [`negotiate`], this intentionally does not skip
/// input-bearing configurations.
fn select_source_configuration(
    instance: &mut PluginInstance<MxmHost>,
    index: u32,
) -> Result<(Envelope, usize), RenderError> {
    let shared = instance.plugin_shared_handle();
    let configs: PluginAudioPortsConfig = shared.get_extension().ok_or_else(|| {
        RenderError::Load("the plugin exposes no audio configurations".to_owned())
    })?;
    let audio_ports: PluginAudioPorts = shared
        .get_extension()
        .ok_or_else(|| RenderError::Load("the plugin exposes no audio ports".to_owned()))?;
    let note_ports: Option<PluginNotePorts> = shared.get_extension();

    let mut config_buffer = AudioPortsConfigBuffer::new();
    let mut handle = instance.plugin_handle();
    let advertised = configs
        .get(&mut handle, index, &mut config_buffer)
        .ok_or_else(|| {
            RenderError::Load(format!("audio configuration {index} is not advertised"))
        })?;
    let id = advertised.id;
    let name = String::from_utf8_lossy(advertised.name).into_owned();
    configs
        .select(&mut handle, id)
        .map_err(|_| RenderError::Refused(Refusal::ConfigurationRefused(id)))?;

    let layout = read_static_layout(instance, &audio_ports).map_err(RenderError::Refused)?;
    let (port_id, channels, is_main) = layout
        .main_output
        .ok_or_else(|| RenderError::Load("the selected configuration has no output".to_owned()))?;
    if layout.output_count != 1 || !is_main || !matches!(channels, 1 | 2) {
        return Err(RenderError::Load(
            "the selected configuration is not one mono/stereo main output".to_owned(),
        ));
    }
    let auxiliary_ports = match (layout.input_count, layout.main_input) {
        (0, None) => 0,
        (1, Some((_id, 1, false))) => 1,
        _ => {
            return Err(RenderError::Load(
                "the selected configuration is not zero inputs or one mono auxiliary".to_owned(),
            ));
        }
    };
    if let Some(ext) = &note_ports {
        let mut handle = instance.plugin_handle();
        if ext.count(&mut handle, false) != 0 {
            return Err(RenderError::Load(
                "the selected source advertises a note-output port".to_owned(),
            ));
        }
    }
    let note_input = note_ports
        .as_ref()
        .and_then(|ext| negotiate_note_port(instance, ext, true));
    Ok((
        Envelope {
            audio: AudioLayout {
                channel_count: channels,
                port_id,
            },
            note_input,
            note_output: None,
            selected_config: Some(id),
            selection: format!("configuration {index} \"{name}\""),
        },
        auxiliary_ports,
    ))
}

/// Runs a rendered source through `effects`, in order, at the block size the source was
/// rendered at.
///
/// The channel count adapts at each boundary exactly as the live chain adapts it — stereo into a
/// mono effect sums at half, mono into a stereo effect duplicates — so an export sounds like the
/// room did. The render is **not lengthened**: an effect's tail past the end is the sequence's
/// business, as `sequencer::export` says, and an empty bar at the end is how one is asked for.
/// `tempo` is [`RenderConfig::tempo`]'s, for the effects: an export passes the same one to both.
pub fn through_effects(
    mut result: RenderResult,
    effects: &[EffectSpec],
    sample_rate: f64,
    block_size: u32,
    tempo: Option<f64>,
) -> Result<RenderResult, RenderError> {
    for effect in effects {
        result.channels = run_effect(effect, &result.channels, sample_rate, block_size, tempo)?;
    }
    Ok(result)
}

/// One effect over the whole of `source`, block by block, as [`render`] drives the source.
fn run_effect(
    effect: &EffectSpec,
    source: &[Vec<f32>],
    sample_rate: f64,
    block_size: u32,
    tempo: Option<f64>,
) -> Result<Vec<Vec<f32>>, RenderError> {
    // SAFETY: as in `render` — loading a bundle runs its code, and the caller chose the bundle.
    let entry = unsafe { crate::entry::load(&effect.bundle) }
        .map_err(|e| RenderError::Load(e.to_string()))?;
    let mut instance = instantiate(&entry, &effect.plugin_id)?;
    if let Some(bytes) = effect.state.as_deref() {
        restore(&mut instance, bytes)?;
    }

    let envelope = negotiate_effect(&mut instance).map_err(RenderError::Refused)?;
    let input_channels = envelope.input.channel_count as usize;
    let output_channels = envelope.output.channel_count as usize;
    let frames = source.first().map(Vec::len).unwrap_or(0);

    let processor = instance.activate(
        |shared, _| HostAudioProcessor::new(shared),
        PluginAudioConfiguration {
            sample_rate,
            min_frames_count: block_size,
            max_frames_count: block_size,
        },
    )?;

    let mut output: Vec<Vec<f32>> = vec![Vec::with_capacity(frames); output_channels];
    {
        let mut processor = processor
            .start_processing()
            .map_err(|e| RenderError::Instance(e.into()))?;

        let block = block_size as usize;
        let mut input_block: Vec<Vec<f32>> = vec![vec![0.0; block]; input_channels];
        let mut output_block: Vec<Vec<f32>> = vec![vec![0.0; block]; output_channels];
        let mut input_ports = AudioPorts::with_capacity(input_channels, 1);
        let mut output_ports = AudioPorts::with_capacity(output_channels, 1);
        let mut input_events = EventBuffer::new();
        let mut output_events = EventBuffer::new();
        // Sorted once: the loop walks it with a cursor rather than scanning the whole list per
        // block, exactly as the source's render does.
        let mut scheduled = effect.events.clone();
        scheduled.sort_by_key(|event| event.frame);
        let mut next_event = 0usize;

        let mut frame = 0usize;
        while frame < frames {
            let n = block.min(frames - frame);
            adapt_channels(source, &mut input_block, frame, n);
            for buffer in &mut output_block {
                buffer[..n].fill(0.0);
            }
            output_events.clear();
            input_events.clear();
            while next_event < scheduled.len() && scheduled[next_event].frame < (frame + n) as u64 {
                let event = &scheduled[next_event];
                let at = event.frame.saturating_sub(frame as u64) as u32;
                push_effect_event(&mut input_events, at, event.kind);
                next_event += 1;
            }

            let inputs = input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    input_block
                        .iter_mut()
                        .map(|b| InputChannel::from_buffer(&mut b[..n], false)),
                ),
            }]);
            let mut outputs = output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(
                    output_block.iter_mut().map(|b| &mut b[..n]),
                ),
            }]);
            processor.process(
                &inputs,
                &mut outputs,
                &InputEvents::from_buffer(&input_events),
                &mut OutputEvents::from_buffer(&mut output_events),
                Some(frame as u64),
                tempo
                    .map(|tempo| {
                        crate::events::transport::playing_from_start(
                            frame as u64,
                            sample_rate,
                            tempo,
                        )
                    })
                    .as_ref(),
            )?;

            for (channel, buffer) in output.iter_mut().zip(output_block.iter()) {
                channel.extend_from_slice(&buffer[..n]);
            }
            frame += n;
        }

        let processor = processor.stop_processing();
        instance.deactivate(processor);
    }
    Ok(output)
}

/// Copies `n` frames of `source` from `from` into `input`, adapting the channel count as the
/// live chain does: stereo into mono is the sum at half, mono into stereo is duplicated, and
/// anything else is copied by index with missing channels silent.
fn adapt_channels(source: &[Vec<f32>], input: &mut [Vec<f32>], from: usize, n: usize) {
    match (source.len(), input.len()) {
        (2, 1) => {
            let (left, right) = (&source[0][from..from + n], &source[1][from..from + n]);
            for (i, out) in input[0][..n].iter_mut().enumerate() {
                *out = 0.5 * (left[i] + right[i]);
            }
        }
        (1, 2) => {
            let mono = &source[0][from..from + n];
            for channel in input.iter_mut() {
                channel[..n].copy_from_slice(mono);
            }
        }
        _ => {
            for (index, channel) in input.iter_mut().enumerate() {
                match source.get(index) {
                    Some(from_channel) => {
                        channel[..n].copy_from_slice(&from_channel[from..from + n])
                    }
                    None => channel[..n].fill(0.0),
                }
            }
        }
    }
}

/// One scheduled event for an **effect**: parameter automation and nothing else.
///
/// Separate from [`push_event`] because that one picks a note dialect from the plugin's note input
/// port, and an effect has none — the plugin conventions (mxm-kit's `docs/plugin-conventions.md`,
/// *A developer channel in every editor*; once `plugins/AGENTS.md`) say an effect carries no note
/// port at all. Anything that is not a parameter change is dropped rather than guessed at: a note
/// scheduled for an effect is a bug upstream, and inventing a port for it would hide that.
fn push_effect_event(buffer: &mut EventBuffer, time: u32, kind: EventKind) {
    match kind {
        EventKind::ParamValue { param_id, value } => {
            let Some(id) = ClapId::from_raw(param_id) else {
                return;
            };
            buffer.push(&ParamValueEvent::new(
                time,
                id,
                Pckn::match_all(),
                value,
                Cookie::empty(),
            ));
        }
        EventKind::ParamMod { param_id, value } => {
            let Some(id) = ClapId::from_raw(param_id) else {
                return;
            };
            buffer.push(&ParamModEvent::new(
                time,
                id,
                Pckn::match_all(),
                value,
                Cookie::empty(),
            ));
        }
        // Notes and raw MIDI are not an effect's business: it has no note port at all, so
        // anything of that shape scheduled for one is a bug upstream and is dropped rather than
        // invented a port for.
        EventKind::NoteOn { .. } | EventKind::NoteOff { .. } | EventKind::Midi { .. } => {}
    }
}

/// Converts one scheduled event into whichever dialect the note input port negotiated.
fn push_event(buffer: &mut EventBuffer, time: u32, kind: EventKind, envelope: &Envelope) {
    let port = envelope.note_input.map(|p| p.index).unwrap_or(0);
    let dialect = envelope
        .note_input
        .map(|p| p.dialect)
        .unwrap_or(Dialect::Clap);

    match kind {
        EventKind::NoteOn {
            channel,
            key,
            velocity,
            note_id,
        } => match dialect {
            Dialect::Clap => buffer.push(&NoteOnEvent::new(
                time,
                Pckn::new(port, channel, key, note_id),
                velocity,
            )),
            Dialect::Midi1 => buffer.push(&MidiEvent::new(
                time,
                port,
                crate::midi::note_on(channel as u8, key as u8, velocity),
            )),
        },
        EventKind::NoteOff {
            channel,
            key,
            velocity,
            note_id,
        } => match dialect {
            Dialect::Clap => buffer.push(&NoteOffEvent::new(
                time,
                Pckn::new(port, channel, key, note_id),
                velocity,
            )),
            Dialect::Midi1 => buffer.push(&MidiEvent::new(
                time,
                port,
                crate::midi::note_off(channel as u8, key as u8, velocity),
            )),
        },
        EventKind::Midi { data } => buffer.push(&MidiEvent::new(time, port, data)),
        EventKind::ParamValue { param_id, value } => {
            let Some(id) = ClapId::from_raw(param_id) else {
                return;
            };
            buffer.push(&ParamValueEvent::new(
                time,
                id,
                Pckn::match_all(),
                value,
                Cookie::empty(),
            ));
        }
        EventKind::ParamMod { param_id, value } => {
            let Some(id) = ClapId::from_raw(param_id) else {
                return;
            };
            buffer.push(&ParamModEvent::new(
                time,
                id,
                Pckn::match_all(),
                value,
                Cookie::empty(),
            ));
        }
    }
}

/// Loads a bundle and negotiates the envelope for one plugin, without activating it.
///
/// This is the refusal path on its own: the browser needs to say *why* a plugin is unsupported
/// without ever starting audio.
pub fn inspect(bundle: &Path, plugin_id: &str) -> Result<Envelope, RenderError> {
    // SAFETY: as in `render` — loading a bundle runs its code; the caller chooses the bundle.
    let entry =
        unsafe { crate::entry::load(bundle) }.map_err(|e| RenderError::Load(e.to_string()))?;
    let mut instance = instantiate(&entry, plugin_id)?;

    negotiate(&mut instance).map_err(RenderError::Refused)
}

/// Lists the plugin IDs a bundle exposes.
pub fn list_plugins(bundle: &Path) -> Result<Vec<String>, RenderError> {
    // SAFETY: as in `render`.
    let entry =
        unsafe { crate::entry::load(bundle) }.map_err(|e| RenderError::Load(e.to_string()))?;
    let factory = entry
        .get_plugin_factory()
        .ok_or(RenderError::NoPluginFactory)?;
    Ok(factory
        .plugin_descriptors()
        .filter_map(|d| d.id().map(|id| id.to_string_lossy().into_owned()))
        .collect())
}

/// Describes every plugin in a bundle, including why any of them are unsupported.
///
/// This instantiates each plugin to negotiate the envelope, which runs plugin code — so the
/// caller must have stopped audio and armed the scan sentinel first.
pub fn describe(bundle: &Path) -> Vec<crate::discovery::Found> {
    // SAFETY: as in `render` — loading a bundle runs its code; the scanner decides which.
    let Ok(entry) = (unsafe { crate::entry::load(bundle) }) else {
        return Vec::new();
    };
    let Some(factory) = entry.get_plugin_factory() else {
        return Vec::new();
    };

    let descriptors: Vec<(String, String, String, String)> = factory
        .plugin_descriptors()
        .filter_map(|d| {
            Some((
                d.id()?.to_string_lossy().into_owned(),
                d.name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                d.vendor()
                    .map(|v| v.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                d.description()
                    .map(|v| v.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ))
        })
        .collect();

    descriptors
        .into_iter()
        .map(|(id, name, vendor, description)| {
            // Two instances, one per role: `audio-ports-config::select` is a choice made on an
            // instance, and the two roles may choose differently.
            // Only the verdicts are kept: loading negotiates again (`discovery::Verdict`).
            let support = describe_one(&entry, &id)
                .and_then(|mut i| negotiate(&mut i))
                .map(|_| ())
                .map_err(|refusal| refusal.to_string());
            let effect = describe_one(&entry, &id)
                .and_then(|mut i| negotiate_effect(&mut i))
                .map(|_| ())
                .map_err(|refusal| refusal.to_string());
            crate::discovery::Found {
                bundle: bundle.to_path_buf(),
                id,
                name,
                vendor,
                description,
                support,
                effect,
            }
        })
        .collect()
}

/// A fresh, deactivated instance to negotiate one role on.
///
/// **A failure to create it is said as that**, through [`instantiate`]'s own reason, and never as an
/// envelope refusal it is not.
fn describe_one(entry: &PluginEntry, plugin_id: &str) -> Result<PluginInstance<MxmHost>, Refusal> {
    instantiate(entry, plugin_id).map_err(|why| Refusal::NotInstantiable(why.to_string()))
}

/// Loads a bundle and negotiates the **effect** envelope for one plugin, without activating it.
pub fn inspect_effect(bundle: &Path, plugin_id: &str) -> Result<EffectEnvelope, RenderError> {
    // SAFETY: as in `render` — loading a bundle runs its code; the caller chooses the bundle.
    let entry =
        unsafe { crate::entry::load(bundle) }.map_err(|e| RenderError::Load(e.to_string()))?;
    let mut instance = instantiate(&entry, plugin_id)?;
    negotiate_effect(&mut instance).map_err(RenderError::Refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::NotePortBinding;

    fn midi_envelope() -> Envelope {
        Envelope {
            audio: AudioLayout {
                channel_count: 2,
                port_id: ClapId::new(0),
            },
            note_input: Some(NotePortBinding {
                index: 0,
                id: ClapId::new(0),
                dialect: Dialect::Midi1,
                accepts_midi: true,
            }),
            note_output: None,
            selected_config: None,
            selection: String::new(),
        }
    }

    fn midi_bytes(kind: EventKind) -> [u8; 3] {
        let mut buffer = EventBuffer::new();
        push_event(&mut buffer, 0, kind, &midi_envelope());
        let event = buffer.iter().next().expect("one event");
        event
            .as_event::<MidiEvent>()
            .expect("a MIDI 1 event")
            .data()
    }

    #[test]
    fn a_plugin_that_cannot_be_created_is_not_reported_as_having_no_audio_ports() {
        // A refusal without a reason is a bug, and a wrong reason is worse: describing a plugin
        // that could not even be instantiated as "no audio ports" sends whoever reads the picker
        // looking at the wrong extension.
        let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/fixtures/mxm-fixtures.clap");
        if !bundle.exists() {
            eprintln!("skipping: run `cargo xtask fixtures --release` to stage the fixtures");
            return;
        }
        let error = inspect_effect(&bundle, "dk.mxm.fixture.not-in-this-bundle")
            .expect_err("there is no such plugin");
        assert!(
            !matches!(error, RenderError::Refused(Refusal::NoAudioPorts)),
            "reported as {error}"
        );
        assert!(
            error
                .to_string()
                .contains("dk.mxm.fixture.not-in-this-bundle"),
            "the reason names the plugin it could not create: {error}"
        );

        // The picker's own path, which `inspect_effect` no longer goes through: `describe` makes
        // its instances with `describe_one`, and a creation failure there is that refusal exactly.
        // SAFETY: a test fixture bundle, built by this workspace.
        let entry = unsafe { crate::entry::load(&bundle) }.expect("the fixture bundle loads");
        match describe_one(&entry, "dk.mxm.fixture.not-in-this-bundle") {
            Err(Refusal::NotInstantiable(why)) => assert!(
                why.contains("dk.mxm.fixture.not-in-this-bundle"),
                "the reason names the plugin: {why}"
            ),
            Err(other) => panic!("reported as {other}"),
            Ok(_) => panic!("there is no such plugin"),
        }
    }

    #[test]
    fn an_export_reports_each_block_at_its_first_sample() {
        // CLAP's process transport is the state as of the block's first sample. An export's effects
        // run under one that starts at beat 0 and advances by each block; with no tempo there is no
        // transport at all. The probe writes the position it was given into its output, and the
        // live worker is held to the same in `t11_fx_chain`.
        let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/fixtures/mxm-fixtures.clap");
        if !bundle.exists() {
            eprintln!("skipping: run `cargo xtask fixtures --release` to stage the fixtures");
            return;
        }
        let probe = || EffectSpec {
            bundle: bundle.clone(),
            plugin_id: "dk.mxm.fixture.transport-probe".to_owned(),
            state: None,
            events: Vec::new(),
        };
        let silence = || RenderResult {
            channels: vec![vec![0.0; 1024]; 2],
            envelope: midi_envelope(),
            statuses: Vec::new(),
            output_event_count: 0,
        };

        let timed = through_effects(silence(), &[probe()], 48_000.0, 512, Some(120.0))
            .expect("the probe runs");
        let one_block = 512.0 / 48_000.0 * 120.0 / 60.0;
        assert_eq!(timed.channels[0][0], 0.0, "an export starts at beat 0");
        assert!(
            (f64::from(timed.channels[0][512]) - one_block).abs() < 1e-6,
            "the second block starts one block in: {}",
            timed.channels[0][512]
        );

        let untimed =
            through_effects(silence(), &[probe()], 48_000.0, 512, None).expect("the probe runs");
        assert_eq!(untimed.channels[0][0], -1.0, "no tempo, no transport");
    }

    #[test]
    fn a_quiet_note_on_is_still_a_note_on_as_the_live_path_sends_it() {
        // Velocity 0 on a note-on is a note-off by MIDI convention. The live path floors at 1
        // (`midi::to_midi1`, the processor's own note-on); a render that rounded to 0 would turn
        // the quietest step of an export into a release.
        for velocity in [0.0, 0.001, 0.003] {
            let data = midi_bytes(EventKind::NoteOn {
                channel: 0,
                key: 60,
                velocity,
                note_id: 0,
            });
            assert_eq!(data[0] & 0xf0, 0x90);
            assert_eq!(data[2], 1, "velocity {velocity} must stay a note-on");
        }
        let loud = midi_bytes(EventKind::NoteOn {
            channel: 0,
            key: 60,
            velocity: 2.0,
            note_id: 0,
        });
        assert_eq!(loud[2], 127);
    }
}
