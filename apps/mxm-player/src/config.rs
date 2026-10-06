//! How a `PlayerApp` is wired to the outside world.
//!
//! Everything the app touches that is *not* the app: the audio backend, where settings live, where
//! the scan sentinel lives, which directories are searched for plugins, the clock, and how MIDI
//! ports are enumerated.
//!
//! [`PlayerConfig::production`] builds exactly what the shipped binary uses, so the production path
//! stays the ordinary one rather than becoming a special case of a test configuration.
//!
//! This is not only for tests. Making the settings location overridable is what a portable or
//! no-install mode would need anyway, and it is what stops a test run from overwriting the
//! settings of whoever is using the player on the same machine.

use crate::clock::Clock;
use crate::engine::audio::{Backend, CpalBackend};
use crate::midi;
use std::path::PathBuf;
use std::sync::Arc;

/// Where the list of MIDI ports comes from.
#[derive(Clone, Debug)]
pub enum MidiPorts {
    /// Ask the system. What production does.
    Live,
    /// A fixed list, so a test does not depend on what is plugged into the machine.
    Fixed {
        inputs: Vec<String>,
        outputs: Vec<String>,
    },
}

impl MidiPorts {
    pub fn inputs(&self) -> Vec<String> {
        match self {
            MidiPorts::Live => midi::input::list_ports()
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.name)
                .collect(),
            MidiPorts::Fixed { inputs, .. } => inputs.clone(),
        }
    }

    pub fn outputs(&self) -> Vec<String> {
        match self {
            MidiPorts::Live => midi::port::list_ports().unwrap_or_default(),
            MidiPorts::Fixed { outputs, .. } => outputs.clone(),
        }
    }
}

/// Everything a `PlayerApp` needs from its environment.
pub struct PlayerConfig {
    pub backend: Box<dyn Backend>,
    pub settings_path: PathBuf,
    pub sentinel_path: PathBuf,
    /// The user's control-map overlay. The shipped collection standard is compiled in, so this
    /// file is optional and its absence is the ordinary case.
    pub control_map_path: PathBuf,
    /// Where saved sequences live. A sequence is instrument-independent, so it is a player
    /// concern rather than a plugin one.
    pub sequence_dir: PathBuf,
    /// Where rendered audio and `.mid` files go.
    ///
    /// Through the config like every other path, so a test can never write into the real one.
    pub exports_dir: PathBuf,
    /// `None` searches the standard CLAP locations plus `CLAP_PATH` and this repo's
    /// `target/bundled`. `Some` searches exactly what it names, which is what a test wants.
    pub search_paths: Option<Vec<PathBuf>>,
    pub clock: Arc<Clock>,
    pub midi_ports: MidiPorts,
}

impl PlayerConfig {
    /// What the shipped binary runs: a real device, the user's settings, the whole machine.
    pub fn production() -> Self {
        Self {
            backend: Box::new(CpalBackend::new()),
            settings_path: crate::settings::Settings::default_path(),
            sentinel_path: crate::discovery::Sentinel::default_path(),
            control_map_path: crate::control_map::default_user_path(),
            sequence_dir: crate::sequencer::sequence::default_dir(),
            exports_dir: crate::sequencer::export::default_dir(),
            search_paths: None,
            clock: Clock::monotonic(),
            midi_ports: MidiPorts::Live,
        }
    }

    /// A configuration that touches nothing outside `dir` and no hardware at all.
    ///
    /// The point of the whole seam: an app-level test that ran with the production configuration
    /// would rewrite the settings file of whoever is using the player on this machine.
    pub fn sandboxed(dir: impl Into<PathBuf>, backend: Box<dyn Backend>) -> Self {
        let dir = dir.into();
        Self {
            backend,
            settings_path: dir.join("settings.json"),
            sentinel_path: dir.join("scanning.sentinel"),
            control_map_path: dir.join("control-map.json"),
            sequence_dir: dir.join("sequences"),
            exports_dir: dir.join("exports"),
            // Empty rather than `None`: a test that means to find nothing must not discover
            // whatever happens to be installed on the machine running it.
            search_paths: Some(Vec::new()),
            clock: Clock::virtual_clock(),
            midi_ports: MidiPorts::Fixed {
                inputs: Vec::new(),
                outputs: Vec::new(),
            },
        }
    }

    /// Points discovery at exactly these directories.
    pub fn with_search_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.search_paths = Some(paths);
        self
    }

    /// Uses a caller-supplied clock instead of the default for this configuration.
    pub fn with_clock(mut self, clock: Arc<Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Supplies a fixed list of MIDI ports.
    pub fn with_midi_ports(mut self, inputs: Vec<String>, outputs: Vec<String>) -> Self {
        self.midi_ports = MidiPorts::Fixed { inputs, outputs };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::audio::FakeBackend;

    #[test]
    fn a_sandboxed_config_stays_inside_its_directory() {
        let dir = std::env::temp_dir().join("mxm-player-config-test");
        let config = PlayerConfig::sandboxed(&dir, Box::new(FakeBackend::new()));

        assert!(config.settings_path.starts_with(&dir));
        assert!(config.sentinel_path.starts_with(&dir));
        assert!(config.control_map_path.starts_with(&dir));
        assert!(config.sequence_dir.starts_with(&dir));
        assert!(config.exports_dir.starts_with(&dir));
        assert_ne!(
            config.settings_path,
            crate::settings::Settings::default_path(),
            "a sandboxed run must never write the real settings file"
        );
    }

    #[test]
    fn a_sandboxed_config_finds_no_plugins_by_accident() {
        let dir = std::env::temp_dir().join("mxm-player-config-test");
        let config = PlayerConfig::sandboxed(&dir, Box::new(FakeBackend::new()));

        // `None` would mean "the standard locations", which on a developer's machine is a real
        // set of installed plugins and makes a test depend on what is installed.
        assert_eq!(config.search_paths, Some(Vec::new()));
    }

    #[test]
    fn a_sandboxed_config_uses_a_clock_that_does_not_move_on_its_own() {
        let dir = std::env::temp_dir().join("mxm-player-config-test");
        let config = PlayerConfig::sandboxed(&dir, Box::new(FakeBackend::new()));
        assert!(config.clock.is_virtual());
    }

    #[test]
    fn fixed_midi_ports_do_not_ask_the_machine() {
        let ports = MidiPorts::Fixed {
            inputs: vec!["Fake keyboard".to_owned()],
            outputs: vec!["Fake synth".to_owned()],
        };
        assert_eq!(ports.inputs(), vec!["Fake keyboard".to_owned()]);
        assert_eq!(ports.outputs(), vec!["Fake synth".to_owned()]);
    }
}
