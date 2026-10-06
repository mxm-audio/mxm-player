//! What each fixture advertises, in one table.
//!
//! Every fixture is the same plugin type driven by a different [`FixtureSpec`], so adding a
//! case to the verification matrix is a row here rather than a new crate.

use clack_extensions::audio_ports::AudioPortFlags;
use clack_extensions::note_ports::{NoteDialect, NoteDialects};
use clack_plugin::plugin::features;
use std::ffi::CStr;

/// The behaviour a fixture exhibits once it is processing.
///
/// Port layout is described separately, by [`FixtureSpec::audio_outputs`] and friends, because
/// most out-of-envelope fixtures differ only in what they declare.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Behaviour {
    /// Renders silence and asks to sleep. The default for envelope fixtures, which are refused
    /// before they ever process.
    Silent,
    /// Emits parameter, gesture, note, and deliberately unrepresentable output events, and can be
    /// driven to flood the host's output sink.
    EventEmitter,
    /// Burns a calibrated number of floating-point operations per sample.
    CpuLoad,
    /// Never returns from `process()`. Only ever loaded in a subprocess.
    Hang,
    /// Logs from two threads at once through the host's `log` extension.
    LogSpam,
    /// Reports a finite tail, then switches to `TailLength::Infinite` and notifies the host.
    TailShift,
    /// Asks the host to run `on_main_thread`, counts how often it actually did, and asks again
    /// from inside that callback — which a host must service on a *later* turn, not recursively.
    MainThreadCallback,
    /// Counts the voices it is holding and reports the total as its parameter value, so a host
    /// can prove its recovery actually cleared them rather than assuming it did.
    VoiceCounter,
    /// A well-behaved audio effect with a measurable transform and a finite tail: the input
    /// scaled by the `Amount` parameter plus a single delayed echo of it, decaying. Exists so the
    /// player's effect hosting can be proved against a plugin that shares no code with the
    /// collection.
    Effect,
}

/// One audio port declaration.
#[derive(Copy, Clone, Debug)]
pub struct PortDecl {
    pub name: &'static str,
    pub channel_count: u32,
    pub is_main: bool,
}

/// Everything a fixture advertises and does.
#[derive(Copy, Clone, Debug)]
pub struct FixtureSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub audio_inputs: &'static [PortDecl],
    pub audio_outputs: &'static [PortDecl],
    /// `None` means the fixture does not implement `audio-ports-config` at all, so a host takes
    /// its static `audio-ports` layout.
    pub configs: Option<&'static [ConfigDecl]>,
    pub note_input: Option<NoteDialectDecl>,
    pub note_output: Option<NoteDialectDecl>,
    pub behaviour: Behaviour,
}

/// One `audio-ports-config` entry.
#[derive(Copy, Clone, Debug)]
pub struct ConfigDecl {
    pub id: u32,
    pub name: &'static str,
    pub input_port_count: u32,
    pub output_port_count: u32,
    pub main_output_channels: Option<u32>,
}

/// A note port's advertised dialects. Kept separate from `NoteDialects` so the table stays
/// `const`-constructible.
#[derive(Copy, Clone, Debug)]
pub enum NoteDialectDecl {
    /// CLAP notes only - the case that proves a MIDI-only recovery path would be ignored.
    ClapOnly,
    /// CLAP notes preferred, MIDI 1 also accepted. What every MXM plugin advertises.
    ClapAndMidi,
}

impl NoteDialectDecl {
    pub fn dialects(self) -> NoteDialects {
        match self {
            Self::ClapOnly => NoteDialect::Clap.into(),
            Self::ClapAndMidi => NoteDialects::CLAP | NoteDialects::MIDI,
        }
    }

    pub fn preferred(self) -> NoteDialect {
        NoteDialect::Clap
    }
}

impl FixtureSpec {
    /// The CLAP category features the descriptor declares. Only the conforming effect declares
    /// any - CLAP requires a category of a well-behaved plugin, and the rest are not that - and
    /// its width feature follows the main output.
    pub fn features(&self) -> &'static [&'static CStr] {
        if self.behaviour != Behaviour::Effect {
            return &[];
        }
        let main_out = self.audio_outputs.iter().find(|port| port.is_main);
        if main_out.map(|port| port.channel_count) == Some(1) {
            &[features::AUDIO_EFFECT, features::MONO]
        } else {
            &[features::AUDIO_EFFECT, features::STEREO]
        }
    }
}

impl PortDecl {
    pub fn flags(&self) -> AudioPortFlags {
        if self.is_main {
            AudioPortFlags::IS_MAIN
        } else {
            AudioPortFlags::empty()
        }
    }
}

const STEREO_MAIN_OUT: &[PortDecl] = &[PortDecl {
    name: "Main out",
    channel_count: 2,
    is_main: true,
}];

const STEREO_MAIN_IN: &[PortDecl] = &[PortDecl {
    name: "Main in",
    channel_count: 2,
    is_main: true,
}];

const MONO_MAIN_OUT: &[PortDecl] = &[PortDecl {
    name: "Main out",
    channel_count: 1,
    is_main: true,
}];

const MONO_MAIN_IN: &[PortDecl] = &[PortDecl {
    name: "Main in",
    channel_count: 1,
    is_main: true,
}];

const NO_PORTS: &[PortDecl] = &[];

/// The full fixture set. Index order is the factory's plugin index order.
pub const FIXTURES: &[FixtureSpec] = &[
    FixtureSpec {
        id: "dk.mxm.fixture.audio-input",
        name: "MXM Fixture: audio input",
        description: "Declares an audio input port; the player must refuse it",
        audio_inputs: STEREO_MAIN_IN,
        audio_outputs: STEREO_MAIN_OUT,
        configs: None,
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: None,
        behaviour: Behaviour::Silent,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.two-outputs",
        name: "MXM Fixture: two outputs",
        description: "Declares two audio output ports; the player must refuse it",
        audio_inputs: NO_PORTS,
        audio_outputs: &[
            PortDecl {
                name: "Main out",
                channel_count: 2,
                is_main: true,
            },
            PortDecl {
                name: "Aux out",
                channel_count: 2,
                is_main: false,
            },
        ],
        configs: None,
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: None,
        behaviour: Behaviour::Silent,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.surround",
        name: "MXM Fixture: surround",
        description: "One output port with four channels; the player must refuse it",
        audio_inputs: NO_PORTS,
        audio_outputs: &[PortDecl {
            name: "Main out",
            channel_count: 4,
            is_main: true,
        }],
        configs: None,
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: None,
        behaviour: Behaviour::Silent,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.no-config",
        name: "MXM Fixture: no compatible configuration",
        description: "Advertises configurations, none of which satisfy the v1 envelope",
        audio_inputs: STEREO_MAIN_IN,
        audio_outputs: &[
            PortDecl {
                name: "Main out",
                channel_count: 6,
                is_main: true,
            },
            PortDecl {
                name: "Aux out",
                channel_count: 2,
                is_main: false,
            },
        ],
        configs: Some(&[
            ConfigDecl {
                id: 0,
                name: "6.0 with sidechain",
                input_port_count: 1,
                output_port_count: 2,
                main_output_channels: Some(6),
            },
            ConfigDecl {
                id: 1,
                name: "Quad",
                input_port_count: 1,
                output_port_count: 1,
                main_output_channels: Some(4),
            },
        ]),
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: None,
        behaviour: Behaviour::Silent,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.non-main-output",
        name: "MXM Fixture: non-main output",
        description: "Exactly one output port, flagged not-main; the player must refuse it",
        audio_inputs: NO_PORTS,
        audio_outputs: &[PortDecl {
            name: "Side out",
            channel_count: 2,
            is_main: false,
        }],
        configs: None,
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: None,
        behaviour: Behaviour::Silent,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.event-emitter",
        name: "MXM Fixture: event emitter",
        description: "Emits parameter, gesture, note and unrepresentable output events",
        audio_inputs: NO_PORTS,
        audio_outputs: STEREO_MAIN_OUT,
        configs: None,
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: Some(NoteDialectDecl::ClapAndMidi),
        behaviour: Behaviour::EventEmitter,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.cpu-load",
        name: "MXM Fixture: CPU load",
        description: "Calibrated busy loop, for validating the load meters",
        audio_inputs: NO_PORTS,
        audio_outputs: STEREO_MAIN_OUT,
        configs: None,
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: None,
        behaviour: Behaviour::CpuLoad,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.hang",
        name: "MXM Fixture: hang",
        description: "Never returns from process(); for the wedged-engine test only",
        audio_inputs: NO_PORTS,
        audio_outputs: STEREO_MAIN_OUT,
        configs: None,
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: None,
        behaviour: Behaviour::Hang,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.log-spam",
        name: "MXM Fixture: log spam",
        description: "Logs from two threads at once, exercising the host's log transport",
        audio_inputs: NO_PORTS,
        audio_outputs: STEREO_MAIN_OUT,
        configs: None,
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: None,
        behaviour: Behaviour::LogSpam,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.tail-shift",
        name: "MXM Fixture: tail shift",
        description: "Reports a finite tail, then switches to infinite and notifies the host",
        audio_inputs: NO_PORTS,
        audio_outputs: STEREO_MAIN_OUT,
        configs: None,
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: None,
        behaviour: Behaviour::TailShift,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.main-thread",
        name: "MXM Fixture: main-thread callback",
        description: "Requests on_main_thread and reports how many times the host ran it",
        audio_inputs: NO_PORTS,
        audio_outputs: STEREO_MAIN_OUT,
        configs: None,
        note_input: Some(NoteDialectDecl::ClapAndMidi),
        note_output: None,
        behaviour: Behaviour::MainThreadCallback,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.clap-only-notes",
        name: "MXM Fixture: CLAP-only notes",
        description: "A note input port that accepts CLAP notes and nothing else",
        audio_inputs: NO_PORTS,
        audio_outputs: STEREO_MAIN_OUT,
        configs: None,
        note_input: Some(NoteDialectDecl::ClapOnly),
        note_output: None,
        behaviour: Behaviour::VoiceCounter,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.effect",
        name: "MXM Fixture: effect",
        description: "A conforming audio effect: stereo in, stereo out, a gain and a decaying echo, finite tail",
        audio_inputs: STEREO_MAIN_IN,
        audio_outputs: STEREO_MAIN_OUT,
        configs: None,
        note_input: None,
        note_output: None,
        behaviour: Behaviour::Effect,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.effect-main-thread",
        name: "MXM Fixture: effect main-thread callback",
        description: "An effect-shaped plugin requesting two main-thread callbacks",
        audio_inputs: STEREO_MAIN_IN,
        audio_outputs: STEREO_MAIN_OUT,
        configs: None,
        note_input: None,
        note_output: None,
        behaviour: Behaviour::MainThreadCallback,
    },
    FixtureSpec {
        id: "dk.mxm.fixture.effect-mono",
        name: "MXM Fixture: effect (mono)",
        description: "The same conforming effect, mono in and mono out, so a host's channel adapter is tested in both directions",
        audio_inputs: MONO_MAIN_IN,
        audio_outputs: MONO_MAIN_OUT,
        configs: None,
        note_input: None,
        note_output: None,
        behaviour: Behaviour::Effect,
    },
];

/// Looks a fixture up by its CLAP ID.
pub fn find(id: &str) -> Option<&'static FixtureSpec> {
    FIXTURES.iter().find(|f| f.id == id)
}
