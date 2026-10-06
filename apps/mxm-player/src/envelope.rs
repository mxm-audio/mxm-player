//! The v1 CLAP compatibility envelope, and the refusals that enforce it.
//!
//! Deliberately narrow and, importantly, *enforceable*: a host must provide buffers for every
//! active declared port, so "ignore the aux ports" is not an option. Everything outside the
//! envelope is refused **with the reason shown**, which is what keeps third-party readiness
//! honest without third-party plugins to test against.

use crate::host::MxmHost;
use clack_extensions::audio_ports::{AudioPortFlags, AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::audio_ports_config::{AudioPortsConfigBuffer, PluginAudioPortsConfig};
use clack_extensions::note_ports::{
    NoteDialect, NoteDialects, NotePortInfoBuffer, PluginNotePorts,
};
use clack_host::prelude::*;
use std::fmt;

/// Which note dialect the player will speak on a given port.
///
/// Negotiated **independently per direction**: a usable CLAP input port implies nothing about the
/// output ports' dialect or indexing.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Dialect {
    /// CLAP note events. Preferred: it is the only dialect that can carry a voice ID.
    Clap,
    /// Raw MIDI 1 messages.
    Midi1,
}

impl Dialect {
    /// Whether source-targeted cleanup (a per-press choke carrying a voice ID) is possible.
    ///
    /// MIDI 1 has no concept of a voice ID, so on a MIDI-dialect port a note-off cannot be told
    /// apart from any other note-off of the same channel and pitch.
    pub fn carries_voice_ids(self) -> bool {
        matches!(self, Dialect::Clap)
    }

    pub fn label(self) -> &'static str {
        match self {
            Dialect::Clap => "CLAP notes",
            Dialect::Midi1 => "MIDI 1",
        }
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One negotiated note port.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct NotePortBinding {
    /// The port's index, which is what output events carry and what input events must target.
    pub index: u16,
    pub id: ClapId,
    /// The dialect ordinary notes are sent in: CLAP where offered, because it is the only one
    /// that can carry a voice ID.
    pub dialect: Dialect,
    /// Whether the port *also* accepts MIDI 1, which is a separate question from the dialect
    /// chosen above — and the one that decides which global recovery actually reaches it.
    pub accepts_midi: bool,
}

/// The audio layout the player will drive.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct AudioLayout {
    /// 1 or 2. Nothing else is inside the envelope.
    pub channel_count: u32,
    pub port_id: ClapId,
}

/// Everything the player negotiated with a plugin it accepted.
#[derive(Clone, Debug, PartialEq)]
pub struct Envelope {
    pub audio: AudioLayout,
    pub note_input: Option<NotePortBinding>,
    pub note_output: Option<NotePortBinding>,
    /// The `audio-ports-config` entry selected, if the plugin implements that extension.
    pub selected_config: Option<ClapId>,
    /// A human-readable note about how the layout was chosen, for the browser.
    pub selection: String,
}

impl Envelope {
    /// Whether the plugin can be sent voice IDs at all, which decides whether focus-loss cleanup
    /// can be targeted or has to fall back to the global path.
    pub fn note_input_carries_voice_ids(&self) -> bool {
        self.note_input
            .is_some_and(|p| p.dialect.carries_voice_ids())
    }

    /// Which global recovery the plugin will actually act on.
    ///
    /// Describing the emergency path as "CC 120" quietly assumed every plugin accepts MIDI, and
    /// a CLAP-only note port never receives it — the recovery would be ignored and the very
    /// notes the panic exists to clear would stay stuck. But the reverse matters just as much:
    /// nice-plug's wrapper does not handle wildcard choke channel and key values, so for a
    /// plugin that accepts MIDI — which includes every MXM plugin today — CC 120 is the path
    /// that actually works, even when ordinary notes go out as CLAP events.
    pub fn global_recovery(&self) -> GlobalRecovery {
        match self.note_input {
            Some(port) if port.accepts_midi => GlobalRecovery::AllSoundOff,
            Some(_) => GlobalRecovery::WildcardChoke,
            None => GlobalRecovery::AllSoundOff,
        }
    }
}

/// How the player clears everything when it has lost track of what is held.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum GlobalRecovery {
    /// CC 120 on every channel. What a plugin that accepts MIDI will actually act on.
    AllSoundOff,
    /// A wildcard `NoteChoke`, which CLAP defines for exactly this. The only option for a
    /// CLAP-only note port — and therefore the one P1 verifies against a CLAP-only fixture
    /// rather than trusting.
    WildcardChoke,
}

/// What the player negotiated with a plugin it accepted **as an effect**: one main audio input,
/// one main audio output, mono or stereo each way, and no note ports required.
///
/// A separate shape from [`Envelope`] rather than an optional input on it, because the two roles
/// are refused for opposite reasons — an instrument *with* an input is refused as a source, a
/// plugin *without* one as an effect — and a single struct would have to carry every refusal
/// twice. The effect's note ports are deliberately not negotiated: the player sends effects no
/// notes, and an effect that also happens to accept them is still an effect.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectEnvelope {
    pub input: AudioLayout,
    pub output: AudioLayout,
    /// The `audio-ports-config` entry selected, if the plugin implements that extension.
    pub selected_config: Option<ClapId>,
    /// How the layout was chosen, for the picker.
    pub selection: String,
}

/// Why a plugin was refused. Every variant is shown to the user verbatim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The source slot hosts instruments: there is nothing to route audio through.
    HasAudioInputs(u32),
    /// The effect slot needs something to process.
    NoAudioInput,
    /// The player provides exactly one input port's buffers to an effect.
    WrongInputPortCount(u32),
    /// CLAP requires the host to identify a main port; the player feeds that one.
    InputNotMain,
    /// Mono or stereo in, only.
    UnsupportedInputChannelCount(u32),
    /// The plugin advertises configurations, none of which is one mono/stereo main input and one
    /// mono/stereo main output.
    NoCompatibleEffectConfiguration,
    /// The player provides exactly one output port's buffers.
    WrongOutputPortCount(u32),
    /// CLAP requires the host to identify a main port; the player drives that one.
    OutputNotMain,
    /// Mono or stereo only.
    UnsupportedChannelCount(u32),
    /// The plugin advertises configurations, none of which satisfy the above.
    NoCompatibleConfiguration,
    /// No `audio-ports` extension at all: nothing to drive.
    NoAudioPorts,
    /// The plugin's `audio-ports` extension refused to describe its own port.
    PortInfoUnavailable(u32),
    /// The plugin declined the configuration the player selected.
    ConfigurationRefused(ClapId),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::HasAudioInputs(n) => write!(
                f,
                "declares {n} audio input port(s); the source slot hosts instruments only"
            ),
            Refusal::NoAudioInput => {
                f.write_str("declares no audio input; it is an instrument, not an effect")
            }
            Refusal::WrongInputPortCount(n) => write!(
                f,
                "declares {n} audio input ports; the player feeds an effect exactly one"
            ),
            Refusal::InputNotMain => {
                f.write_str("its only audio input port is not flagged as the main port")
            }
            Refusal::UnsupportedInputChannelCount(n) => write!(
                f,
                "its main input has {n} channels; the player supports mono or stereo"
            ),
            Refusal::NoCompatibleEffectConfiguration => f.write_str(
                "advertises no audio port configuration with one mono or stereo main input and one \
                 mono or stereo main output",
            ),
            Refusal::WrongOutputPortCount(n) => write!(
                f,
                "declares {n} audio output ports; the player drives exactly one"
            ),
            Refusal::OutputNotMain => {
                f.write_str("its only audio output port is not flagged as the main port")
            }
            Refusal::UnsupportedChannelCount(n) => write!(
                f,
                "its main output has {n} channels; the player supports mono or stereo"
            ),
            Refusal::NoCompatibleConfiguration => f.write_str(
                "advertises no audio port configuration that is mono or stereo out, with no input",
            ),
            Refusal::NoAudioPorts => {
                f.write_str("does not implement the audio-ports extension, so it has no output")
            }
            Refusal::PortInfoUnavailable(index) => {
                write!(f, "would not describe its audio port {index}")
            }
            Refusal::ConfigurationRefused(id) => write!(
                f,
                "refused the audio port configuration {} the player selected",
                id.get()
            ),
        }
    }
}

/// A port layout as the plugin currently reports it.
pub(crate) struct StaticLayout {
    pub(crate) input_count: u32,
    pub(crate) output_count: u32,
    pub(crate) main_output: Option<(ClapId, u32, bool)>,
    /// The single input port, when there is exactly one — what the effect role checks. The boolean
    /// says whether it is main, so an exact-configuration host probe can distinguish an auxiliary.
    pub(crate) main_input: Option<(ClapId, u32, bool)>,
}

/// Negotiates the **effect** envelope with a deactivated plugin instance.
///
/// The same procedure as [`negotiate`] with the roles reversed: a configuration is compatible when
/// it has exactly one main input and one main output, each mono or stereo, and no auxiliary
/// inputs — the player has no sidechain to offer. Note ports are read but not required.
pub fn negotiate_effect(instance: &mut PluginInstance<MxmHost>) -> Result<EffectEnvelope, Refusal> {
    let shared = instance.plugin_shared_handle();
    let audio_ports: PluginAudioPorts = shared.get_extension().ok_or(Refusal::NoAudioPorts)?;
    let configs: Option<PluginAudioPortsConfig> = shared.get_extension();

    let (selected_config, selection) = match configs {
        Some(configs) => select_effect_configuration(instance, &configs)?,
        None => (
            None,
            "static audio-ports layout (no audio-ports-config)".to_owned(),
        ),
    };

    let layout = read_static_layout(instance, &audio_ports)?;
    let (input, output) = check_effect(layout)?;

    Ok(EffectEnvelope {
        input,
        output,
        selected_config,
        selection,
    })
}

/// The first advertised configuration that is one main mono/stereo input and one main
/// mono/stereo output. First-compatible, as for the source role.
fn select_effect_configuration(
    instance: &mut PluginInstance<MxmHost>,
    configs: &PluginAudioPortsConfig,
) -> Result<(Option<ClapId>, String), Refusal> {
    let mut handle = instance.plugin_handle();
    let count = configs.count(&mut handle);
    if count == 0 {
        return Ok((
            None,
            "static audio-ports layout (no configurations advertised)".to_owned(),
        ));
    }

    let mut buffer = AudioPortsConfigBuffer::new();
    let mut chosen = None;

    for index in 0..count {
        let mut handle = instance.plugin_handle();
        let Some(config) = configs.get(&mut handle, index, &mut buffer) else {
            continue;
        };

        let compatible = config.input_port_count == 1
            && config.output_port_count == 1
            && config
                .main_input
                .is_some_and(|main| matches!(main.channel_count, 1 | 2))
            && config
                .main_output
                .is_some_and(|main| matches!(main.channel_count, 1 | 2));

        if compatible {
            let name = String::from_utf8_lossy(config.name).into_owned();
            let ins = config.main_input.map(|m| m.channel_count).unwrap_or(0);
            let outs = config.main_output.map(|m| m.channel_count).unwrap_or(0);
            chosen = Some((config.id, index, name, ins, outs));
            break;
        }
    }

    let Some((id, index, name, ins, outs)) = chosen else {
        return Err(Refusal::NoCompatibleEffectConfiguration);
    };

    let mut handle = instance.plugin_handle();
    configs
        .select(&mut handle, id)
        .map_err(|_| Refusal::ConfigurationRefused(id))?;

    Ok((
        Some(id),
        format!(
            "configuration {index} \"{name}\" ({} in, {} out)",
            channel_word(ins),
            channel_word(outs)
        ),
    ))
}

fn check_effect(layout: StaticLayout) -> Result<(AudioLayout, AudioLayout), Refusal> {
    match layout.input_count {
        0 => return Err(Refusal::NoAudioInput),
        1 => {}
        n => return Err(Refusal::WrongInputPortCount(n)),
    }
    if layout.output_count != 1 {
        return Err(Refusal::WrongOutputPortCount(layout.output_count));
    }

    let (in_id, in_channels, in_main) = layout.main_input.ok_or(Refusal::PortInfoUnavailable(0))?;
    if !in_main {
        return Err(Refusal::InputNotMain);
    }
    if !matches!(in_channels, 1 | 2) {
        return Err(Refusal::UnsupportedInputChannelCount(in_channels));
    }

    let (out_id, out_channels, out_main) =
        layout.main_output.ok_or(Refusal::PortInfoUnavailable(0))?;
    if !out_main {
        return Err(Refusal::OutputNotMain);
    }
    if !matches!(out_channels, 1 | 2) {
        return Err(Refusal::UnsupportedChannelCount(out_channels));
    }

    Ok((
        AudioLayout {
            channel_count: in_channels,
            port_id: in_id,
        },
        AudioLayout {
            channel_count: out_channels,
            port_id: out_id,
        },
    ))
}

/// Negotiates the envelope with a **deactivated** plugin instance.
///
/// `audio-ports-config::select` requires a deactivated plugin, so this must run before
/// `activate()` and again after any layout change.
pub fn negotiate(instance: &mut PluginInstance<MxmHost>) -> Result<Envelope, Refusal> {
    let shared = instance.plugin_shared_handle();
    let audio_ports: PluginAudioPorts = shared.get_extension().ok_or(Refusal::NoAudioPorts)?;
    let configs: Option<PluginAudioPortsConfig> = shared.get_extension();
    let note_ports: Option<PluginNotePorts> = shared.get_extension();

    let (selected_config, selection) = match configs {
        Some(configs) => select_configuration(instance, &configs)?,
        None => (
            None,
            "static audio-ports layout (no audio-ports-config)".to_owned(),
        ),
    };

    let layout = read_static_layout(instance, &audio_ports)?;
    let audio = check(layout)?;

    let note_input = note_ports
        .as_ref()
        .and_then(|ext| negotiate_note_port(instance, ext, true));
    let note_output = note_ports
        .as_ref()
        .and_then(|ext| negotiate_note_port(instance, ext, false));

    Ok(Envelope {
        audio,
        note_input,
        note_output,
        selected_config,
        selection,
    })
}

/// Picks the **first advertised configuration** that satisfies the envelope.
///
/// CLAP has no "preferred" configuration, so first-compatible is the whole policy — and it is
/// asserted in the P0 fixture rather than assumed.
fn select_configuration(
    instance: &mut PluginInstance<MxmHost>,
    configs: &PluginAudioPortsConfig,
) -> Result<(Option<ClapId>, String), Refusal> {
    let mut handle = instance.plugin_handle();
    let count = configs.count(&mut handle);
    if count == 0 {
        return Ok((
            None,
            "static audio-ports layout (no configurations advertised)".to_owned(),
        ));
    }

    let mut buffer = AudioPortsConfigBuffer::new();
    let mut chosen = None;

    for index in 0..count {
        let mut handle = instance.plugin_handle();
        let Some(config) = configs.get(&mut handle, index, &mut buffer) else {
            continue;
        };

        let compatible = config.input_port_count == 0
            && config.output_port_count == 1
            && config
                .main_output
                .is_some_and(|main| matches!(main.channel_count, 1 | 2));

        if compatible {
            let name = String::from_utf8_lossy(config.name).into_owned();
            let channels = config.main_output.map(|m| m.channel_count).unwrap_or(0);
            chosen = Some((config.id, index, name, channels));
            break;
        }
    }

    let Some((id, index, name, channels)) = chosen else {
        return Err(Refusal::NoCompatibleConfiguration);
    };

    let mut handle = instance.plugin_handle();
    configs
        .select(&mut handle, id)
        .map_err(|_| Refusal::ConfigurationRefused(id))?;

    Ok((
        Some(id),
        format!(
            "configuration {index} \"{name}\" ({} out)",
            channel_word(channels)
        ),
    ))
}

fn channel_word(channels: u32) -> &'static str {
    match channels {
        1 => "mono",
        2 => "stereo",
        _ => "unsupported",
    }
}

pub(crate) fn read_static_layout(
    instance: &mut PluginInstance<MxmHost>,
    audio_ports: &PluginAudioPorts,
) -> Result<StaticLayout, Refusal> {
    let mut handle = instance.plugin_handle();
    let input_count = audio_ports.count(&mut handle, true);
    let output_count = audio_ports.count(&mut handle, false);

    let main_output = if output_count == 1 {
        let mut buffer = AudioPortInfoBuffer::new();
        let mut handle = instance.plugin_handle();
        let info = audio_ports
            .get(&mut handle, 0, false, &mut buffer)
            .ok_or(Refusal::PortInfoUnavailable(0))?;
        Some((
            info.id,
            info.channel_count,
            info.flags.contains(AudioPortFlags::IS_MAIN),
        ))
    } else {
        None
    };

    // Read the same way, and only when there is exactly one: the source role never looks at it,
    // and the effect role refuses any other count before it would.
    let main_input = if input_count == 1 {
        let mut buffer = AudioPortInfoBuffer::new();
        let mut handle = instance.plugin_handle();
        let info = audio_ports
            .get(&mut handle, 0, true, &mut buffer)
            .ok_or(Refusal::PortInfoUnavailable(0))?;
        Some((
            info.id,
            info.channel_count,
            info.flags.contains(AudioPortFlags::IS_MAIN),
        ))
    } else {
        None
    };

    Ok(StaticLayout {
        input_count,
        output_count,
        main_output,
        main_input,
    })
}

fn check(layout: StaticLayout) -> Result<AudioLayout, Refusal> {
    if layout.input_count > 0 {
        return Err(Refusal::HasAudioInputs(layout.input_count));
    }
    if layout.output_count != 1 {
        return Err(Refusal::WrongOutputPortCount(layout.output_count));
    }

    let (port_id, channel_count, is_main) =
        layout.main_output.ok_or(Refusal::PortInfoUnavailable(0))?;
    if !is_main {
        return Err(Refusal::OutputNotMain);
    }
    if !matches!(channel_count, 1 | 2) {
        return Err(Refusal::UnsupportedChannelCount(channel_count));
    }

    Ok(AudioLayout {
        channel_count,
        port_id,
    })
}

/// Negotiates one direction's note port, preferring CLAP and falling back to MIDI 1.
///
/// Returns `None` when the plugin declares no port in that direction, which is not a refusal:
/// an instrument with no note *output* is entirely ordinary.
pub(crate) fn negotiate_note_port(
    instance: &mut PluginInstance<MxmHost>,
    note_ports: &PluginNotePorts,
    is_input: bool,
) -> Option<NotePortBinding> {
    let mut handle = instance.plugin_handle();
    let count = note_ports.count(&mut handle, is_input);
    let mut buffer = NotePortInfoBuffer::new();

    for index in 0..count {
        let mut handle = instance.plugin_handle();
        let Some(info) = note_ports.get(&mut handle, index, is_input, &mut buffer) else {
            continue;
        };

        let dialect = pick_dialect(info.supported_dialects)?;
        let accepts_midi = info.supported_dialects.supports(NoteDialect::Midi);
        // The index is what output events carry, and it must be representable as the `u16` the
        // event header uses.
        let index = u16::try_from(index).ok()?;

        return Some(NotePortBinding {
            index,
            id: info.id,
            dialect,
            accepts_midi,
        });
    }

    None
}

/// Prefer CLAP; fall back to MIDI 1; refuse anything that offers neither.
///
/// MPE and MIDI 2 are deliberately not accepted: the player does not translate them, and
/// claiming a dialect it cannot speak would be worse than declining the port.
fn pick_dialect(supported: NoteDialects) -> Option<Dialect> {
    if supported.supports(NoteDialect::Clap) {
        Some(Dialect::Clap)
    } else if supported.supports(NoteDialect::Midi) {
        Some(Dialect::Midi1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect_preference_is_clap_then_midi_and_nothing_else() {
        assert_eq!(
            pick_dialect(NoteDialects::CLAP | NoteDialects::MIDI),
            Some(Dialect::Clap)
        );
        assert_eq!(pick_dialect(NoteDialects::MIDI), Some(Dialect::Midi1));
        assert_eq!(pick_dialect(NoteDialects::CLAP), Some(Dialect::Clap));
        assert_eq!(pick_dialect(NoteDialects::MIDI_MPE), None);
        assert_eq!(pick_dialect(NoteDialects::MIDI2), None);
        assert_eq!(pick_dialect(NoteDialects::empty()), None);
    }

    fn envelope_with(dialect: Dialect, accepts_midi: bool) -> Envelope {
        Envelope {
            audio: AudioLayout {
                channel_count: 2,
                port_id: ClapId::new(0),
            },
            note_input: Some(NotePortBinding {
                index: 0,
                id: ClapId::new(0),
                dialect,
                accepts_midi,
            }),
            note_output: None,
            selected_config: None,
            selection: String::new(),
        }
    }

    #[test]
    fn recovery_is_chosen_by_what_the_port_accepts_not_by_the_dialect_in_use() {
        // A CLAP-only port never receives CC 120, so recovery there has to be a wildcard choke.
        assert_eq!(
            envelope_with(Dialect::Clap, false).global_recovery(),
            GlobalRecovery::WildcardChoke
        );
        // A port that accepts MIDI gets CC 120 even though ordinary notes go out as CLAP
        // events: that is the path that actually works today.
        assert_eq!(
            envelope_with(Dialect::Clap, true).global_recovery(),
            GlobalRecovery::AllSoundOff
        );
        assert_eq!(
            envelope_with(Dialect::Midi1, true).global_recovery(),
            GlobalRecovery::AllSoundOff
        );
    }

    #[test]
    fn only_clap_notes_can_carry_a_voice_id() {
        assert!(Dialect::Clap.carries_voice_ids());
        assert!(!Dialect::Midi1.carries_voice_ids());
    }

    fn layout(input_count: u32, output_count: u32, channels: u32, is_main: bool) -> StaticLayout {
        StaticLayout {
            input_count,
            output_count,
            main_output: (output_count == 1).then(|| (ClapId::new(0), channels, is_main)),
            main_input: (input_count == 1).then(|| (ClapId::new(0), 2, true)),
        }
    }

    fn effect_layout(
        input_count: u32,
        in_channels: u32,
        in_main: bool,
        output_count: u32,
        out_channels: u32,
    ) -> StaticLayout {
        StaticLayout {
            input_count,
            output_count,
            main_output: (output_count == 1).then(|| (ClapId::new(1), out_channels, true)),
            main_input: (input_count == 1).then(|| (ClapId::new(0), in_channels, in_main)),
        }
    }

    /// The effect role refuses exactly what it says, and accepts every mono/stereo pairing.
    #[test]
    fn the_effect_envelope_refuses_exactly_what_it_says_it_does() {
        assert_eq!(
            check_effect(effect_layout(0, 2, true, 1, 2)).unwrap_err(),
            Refusal::NoAudioInput
        );
        assert_eq!(
            check_effect(effect_layout(2, 2, true, 1, 2)).unwrap_err(),
            Refusal::WrongInputPortCount(2)
        );
        assert_eq!(
            check_effect(effect_layout(1, 2, false, 1, 2)).unwrap_err(),
            Refusal::InputNotMain
        );
        assert_eq!(
            check_effect(effect_layout(1, 6, true, 1, 2)).unwrap_err(),
            Refusal::UnsupportedInputChannelCount(6)
        );
        assert_eq!(
            check_effect(effect_layout(1, 2, true, 2, 2)).unwrap_err(),
            Refusal::WrongOutputPortCount(2)
        );
        assert_eq!(
            check_effect(effect_layout(1, 2, true, 1, 4)).unwrap_err(),
            Refusal::UnsupportedChannelCount(4)
        );

        for (ins, outs) in [(1, 1), (1, 2), (2, 1), (2, 2)] {
            let (input, output) = check_effect(effect_layout(1, ins, true, 1, outs)).unwrap();
            assert_eq!((input.channel_count, output.channel_count), (ins, outs));
        }
    }

    /// The two roles are opposites on the one point that decides them: an input.
    #[test]
    fn an_instrument_is_no_effect_and_an_effect_is_no_instrument() {
        assert_eq!(
            check(layout(1, 1, 2, true)).unwrap_err(),
            Refusal::HasAudioInputs(1)
        );
        assert!(check_effect(effect_layout(1, 2, true, 1, 2)).is_ok());
        assert!(check(layout(0, 1, 2, true)).is_ok());
        assert_eq!(
            check_effect(effect_layout(0, 2, true, 1, 2)).unwrap_err(),
            Refusal::NoAudioInput
        );
    }

    #[test]
    fn the_envelope_refuses_exactly_what_it_says_it_does() {
        assert_eq!(
            check(layout(1, 1, 2, true)).unwrap_err(),
            Refusal::HasAudioInputs(1)
        );
        assert_eq!(
            check(layout(0, 2, 2, true)).unwrap_err(),
            Refusal::WrongOutputPortCount(2)
        );
        assert_eq!(
            check(layout(0, 1, 2, false)).unwrap_err(),
            Refusal::OutputNotMain
        );
        assert_eq!(
            check(layout(0, 1, 4, true)).unwrap_err(),
            Refusal::UnsupportedChannelCount(4)
        );

        assert_eq!(check(layout(0, 1, 2, true)).unwrap().channel_count, 2);
        assert_eq!(check(layout(0, 1, 1, true)).unwrap().channel_count, 1);
    }

    #[test]
    fn refusals_read_as_sentences() {
        // The browser shows these verbatim, so they have to make sense on their own.
        assert!(
            Refusal::HasAudioInputs(1)
                .to_string()
                .contains("instrument")
        );
        assert!(
            Refusal::UnsupportedChannelCount(6)
                .to_string()
                .contains("mono or stereo")
        );
    }
}
