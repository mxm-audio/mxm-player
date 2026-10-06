//! Settings, written crash-safely because §10 means the process can die without warning.
//!
//! In-process hosting cannot be made crash-safe, so the choices that would be annoying to lose —
//! device, ports, last plugin, quarantine list — are written **immediately on change** rather
//! than at exit. That eager policy is what demands the atomic replacement below: overwriting in
//! place could leave a truncated file, which is exactly the failure eager persistence exists to
//! survive.
//!
//! `std::fs::rename` is the mechanism, and it is sufficient on Windows. Renaming over an existing
//! file fails for `MoveFileW` and for Python's `os.rename`, but Rust's `std::fs::rename` passes
//! `MOVEFILE_REPLACE_EXISTING`; measured on this machine, replacement succeeds, and succeeds even
//! while another handle holds the target open for reading. The one real hazard is a *transient*
//! sharing violation from an indexer or antivirus, so the write is wrapped in a short bounded
//! retry rather than reaching for a different API.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How many times a refused replacement is retried before the old file is kept.
pub const REPLACE_ATTEMPTS: u32 = 5;

/// How long to wait between attempts. Tens of milliseconds, not seconds: this runs on the GUI
/// thread and an indexer's handle is released quickly.
pub const REPLACE_BACKOFF: Duration = Duration::from_millis(20);

/// Everything persisted eagerly, from P1.
///
/// Keyed by stable device and port identity, never enumeration index — an index means something
/// different the moment a device is plugged in.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `None` means the system default device.
    pub audio_device: Option<String>,
    pub sample_rate: Option<u32>,
    pub buffer_size: Option<u32>,
    /// MIDI input ports, by name.
    pub midi_inputs: Vec<String>,
    /// MIDI output port, by name.
    pub midi_output: Option<String>,
    pub midi_thru: bool,
    /// The bundle and plugin ID last loaded, so the player comes back where it was.
    pub last_bundle: Option<PathBuf>,
    pub last_plugin_id: Option<String>,
    /// The effect chain after it, in order, with each effect's on/off. A settings file written
    /// before the chain existed loads with none, by the same default rule as everything below.
    #[serde(default)]
    pub fx_chain: Vec<FxSetting>,
    /// Bundles the user chose to quarantine after a crash during discovery.
    pub quarantined: Vec<PathBuf>,

    // --- P4: the rest of the session -----------------------------------------------------
    /// The **expanded** window size, never the collapsed one.
    ///
    /// Collapsing shrinks the window to make room for a plugin's editor beside it. If that size
    /// were persisted, the next launch would restore a small window and expand its contents into
    /// it. So collapsing does not write this; expanding restores it.
    pub window_size: Option<(f32, f32)>,
    /// Whether the parameter panel is collapsed, persisted so a player closed collapsed reopens
    /// collapsed — at the size it will have once expanded again.
    #[serde(default)]
    pub parameters_collapsed: bool,
    /// Whether the left settings panel is collapsed. Independent of the above.
    #[serde(default)]
    pub settings_collapsed: bool,
    pub octave: Option<i32>,
    pub scroll_wheel_editing: bool,
    /// `light`, `dark` or `system` — design system §10, persisted as interface state and never as
    /// anything a plugin or a preset can reach.
    ///
    /// `None` is what a settings file written before the theme control says, and it means
    /// **follow the desktop**: the player had no choice of its own until this landed, and a
    /// missing choice must not silently become one.
    pub theme: Option<String>,

    // --- the sequencer -------------------------------------------------------------------------
    /// Tempo and the last pattern, so a test sequence survives a restart. Small things that are
    /// annoying every single time they are missing.
    pub tempo: Option<f64>,
    pub sequence_steps: Option<crate::sequencer::pattern::PatternData>,
    /// Which steps continue the note before them.
    ///
    /// Separate and optional, so a settings file written before ties existed loads with every step
    /// untied rather than failing — the same treatment `sequence_steps` itself gets.
    pub sequence_tied: Option<Vec<bool>>,
    /// What each step sets, beyond its notes.
    ///
    /// Optional for the reason `sequence_tied` is: a settings file written before locks existed
    /// loads with none rather than failing.
    pub sequence_locks: Option<crate::sequencer::locks::LockData>,
    /// The sequence's shape, so a restart reopens the music at the size it was left.
    ///
    /// **Both, because the step list alone is ambiguous** — forty-eight steps is three bars of
    /// sixteen or four of twelve. Optional for the reason `sequence_tied` is: a settings file
    /// written before a sequence had a shape loads as one bar of sixteen rather than failing.
    pub sequence_bars: Option<usize>,
    pub sequence_steps_per_bar: Option<usize>,
    /// Carried so two launches do not open on the same "random" pattern.
    pub random_seed: Option<u64>,
}

/// One effect as the settings remember it: where it came from, and whether it was off.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FxSetting {
    pub bundle: PathBuf,
    pub plugin_id: String,
    #[serde(default)]
    pub bypassed: bool,
}

impl Settings {
    /// Loads the settings, tolerating malformed data rather than refusing to start.
    ///
    /// A file that cannot be parsed is left on disk under a `.bad` name and defaults are used:
    /// throwing it away would destroy the evidence, and refusing to start over a settings file
    /// would be worse than starting fresh.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };

        match serde_json::from_str(&text) {
            Ok(settings) => settings,
            Err(_) => {
                let _ = std::fs::rename(path, path.with_extension("json.bad"));
                Self::default()
            }
        }
    }

    /// Writes the settings so that a crash mid-write cannot destroy the previous copy.
    ///
    /// Serialise to a sibling temporary file, flush and close it, then atomically replace.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        let temp = path.with_extension("json.tmp");
        {
            let mut file = std::fs::File::create(&temp).map_err(|e| e.to_string())?;
            file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            // Flushed and synced before the rename, so the replacement can only ever expose a
            // complete file.
            file.flush().map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }

        replace_with_retry(&temp, path)
    }

    /// The settings file's location.
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("mxm-player")
            .join("settings.json")
    }
}

/// Atomically replaces `target` with `temp`, retrying briefly on a transient sharing violation.
pub fn replace_with_retry(temp: &Path, target: &Path) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..REPLACE_ATTEMPTS {
        match std::fs::rename(temp, target) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last = error.to_string();
                if attempt + 1 < REPLACE_ATTEMPTS {
                    std::thread::sleep(REPLACE_BACKOFF);
                }
            }
        }
    }

    // Give up and keep the old file: a stale setting is better than none at all.
    let _ = std::fs::remove_file(temp);
    Err(format!(
        "could not replace {}: {last} (the previous settings were kept)",
        target.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mxm-player-tests-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn settings_round_trip() {
        let dir = temp_dir("round-trip");
        let path = dir.join("settings.json");

        let settings = Settings {
            audio_device: Some("Speakers".to_owned()),
            sample_rate: Some(48_000),
            midi_inputs: vec!["Keystation".to_owned()],
            midi_thru: true,
            ..Settings::default()
        };
        settings.save(&path).unwrap();

        assert_eq!(Settings::load(&path), settings);
    }

    #[test]
    fn replacement_succeeds_over_an_existing_file_and_leaves_no_temp_behind() {
        let dir = temp_dir("replace");
        let path = dir.join("settings.json");

        Settings::default().save(&path).unwrap();
        for rate in [44_100u32, 48_000, 96_000] {
            Settings {
                sample_rate: Some(rate),
                ..Settings::default()
            }
            .save(&path)
            .unwrap();
        }

        assert_eq!(Settings::load(&path).sample_rate, Some(96_000));
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temporary file must not survive a successful replacement"
        );
    }

    #[test]
    fn replacement_succeeds_while_the_target_is_open_for_reading() {
        // The specific Windows claim this design rests on, asserted rather than assumed.
        let dir = temp_dir("open-for-reading");
        let path = dir.join("settings.json");
        Settings::default().save(&path).unwrap();

        let handle = std::fs::File::open(&path).unwrap();
        Settings {
            sample_rate: Some(88_200),
            ..Settings::default()
        }
        .save(&path)
        .expect("std::fs::rename passes MOVEFILE_REPLACE_EXISTING");
        drop(handle);

        assert_eq!(Settings::load(&path).sample_rate, Some(88_200));
    }

    #[test]
    fn an_interrupted_write_leaves_the_previous_copy_intact() {
        let dir = temp_dir("interrupted");
        let path = dir.join("settings.json");
        Settings {
            sample_rate: Some(48_000),
            ..Settings::default()
        }
        .save(&path)
        .unwrap();

        // A truncated temporary file, as a crash mid-write would leave.
        std::fs::write(path.with_extension("json.tmp"), "{ \"sample_rate\": 9").unwrap();

        assert_eq!(
            Settings::load(&path).sample_rate,
            Some(48_000),
            "the live file must be untouched by a half-written temporary"
        );
    }

    #[test]
    fn malformed_settings_are_tolerated_and_kept_as_evidence() {
        let dir = temp_dir("malformed");
        let path = dir.join("settings.json");
        std::fs::write(&path, "this is not json").unwrap();

        assert_eq!(Settings::load(&path), Settings::default());
        assert!(
            path.with_extension("json.bad").exists(),
            "the unparseable file is kept rather than thrown away"
        );
    }
}
