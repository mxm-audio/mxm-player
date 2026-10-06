//! Saving and loading a sequence.
//!
//! # A sequence holds notes, tempo, and what its steps *change*
//!
//! It holds **no patch**: no state blob, no starting value for anything. The point of a *collection*
//! is running the same test sequence through mxm-mono-01 and through the next instrument and comparing
//! what you hear, and binding a sequence to a patch would destroy exactly that comparison.
//!
//! Parameter locks are the one exception, and they are not really one. A lock is not part of the
//! patch — it is part of the *line*, the same way a tie is: an acid line is a note pattern and a
//! filter moving under it, and a file that kept the notes and dropped the movement would have kept
//! the half that is easy to hear was missing. So locks are written here, **tagged with the plugin
//! they were recorded for**, and [`super::locks::LockData::to_locks`] drops them with a message when
//! the sequence is loaded under a different instrument. The comparison the paragraph above is about
//! is therefore intact: the notes play through anything, and the parameter numbers — which mean
//! nothing outside the instrument that assigned them — never move a control they were not written
//! for.
//!
//! Reproducing a **particular sample** still takes three things, and they stay separate: the
//! sequence, the plugin, and that plugin's state — which the player saves and loads through the CLAP
//! `state` extension. Composing them is the caller's job, not this file format's.
//!
//! # One file, three uses
//!
//! It falls out of a sequence being small, declarative data rather than being designed for:
//!
//! - **recall** — a sequence you liked, on the next launch;
//! - **a test fixture** — checked into the repo, loaded by a session, rendered byte-identically;
//! - **a sample source** — a fixed input, so two renders differ only by what you changed.
//!
//! Notes are written as names (`["C3", "G3"]`), not as a bitmask, because the file is meant to be
//! opened, diffed in a pull request and hand-edited.

use super::clock::{DEFAULT_TEMPO, MAX_TEMPO, MIN_TEMPO};
use super::pattern::{Pattern, PatternData, STEPS};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The schema version this build writes.
///
/// **4** since a sequence carries its own shape; **3** added parameter locks; **2** added ties.
/// Older versions are *migrated*, not merely tolerated — see [`Sequence::parse`], which names one
/// function per supported version rather than trying to fill unknown fields from defaults.
pub const SCHEMA_VERSION: u32 = 4;

/// The oldest version this build can read, by migrating it forward.
pub const OLDEST_READABLE: u32 = 1;

/// The extension a saved sequence takes.
pub const EXTENSION: &str = "seq.json";

/// A saved sequence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    pub schema_version: u32,
    #[serde(default)]
    pub name: String,
    pub tempo: f64,
    pub steps: PatternData,
    /// Which steps **continue the note before them**, one boolean per step.
    ///
    /// Booleans rather than names: names were the right call for notes, which have sixty-one of
    /// them and are hand-edited constantly, but a tie has two states and JSON already spells them.
    ///
    /// `#[serde(default)]` is what lets a version 1 file parse at all; `migrate_from_v1` is what
    /// gives it the right length afterwards.
    #[serde(default)]
    pub tied: Vec<bool>,
    /// What each step **sets**, beyond the notes it plays.
    ///
    /// `Option`, and absent from the file when there is nothing to say, so a sequence with no locks
    /// is byte-identical to the version 2 file it would have been — which matters because these are
    /// diffed in pull requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locks: Option<super::locks::LockData>,
    /// How many bars it plays.
    ///
    /// **Both dimensions are written, because the step list alone is ambiguous.** Forty-eight steps
    /// is three bars of sixteen or four of twelve, and a file that did not say would be read back as
    /// different music from the one that was saved.
    ///
    /// `#[serde(default)]` is what lets a version 3 file parse; `migrate_from_v3` is what gives it
    /// the shape it had — one bar of sixteen.
    #[serde(default)]
    pub bars: usize,
    /// How many steps each of those bars holds.
    #[serde(default)]
    pub steps_per_bar: usize,
}

impl Sequence {
    pub fn new(name: impl Into<String>, pattern: &Pattern, tempo: f64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            name: name.into(),
            tempo: tempo.clamp(MIN_TEMPO, MAX_TEMPO),
            steps: PatternData::from(pattern),
            tied: (0..pattern.len()).map(|step| pattern.tied(step)).collect(),
            locks: None,
            bars: pattern.bars(),
            steps_per_bar: pattern.steps_per_bar(),
        }
    }

    /// The same sequence, carrying what its steps set.
    ///
    /// Separate from `new` so that every existing caller keeps saying what it meant — a fixture that
    /// is about notes stays about notes — and so that a sequence with no locks writes no `locks` key
    /// at all.
    #[must_use]
    pub fn with_locks(self, locks: &super::locks::LockSet, plugin: Option<&str>) -> Self {
        self.with_locks_in_chain(locks, plugin, &[], &[])
            .expect("source-only sequence received an effect lock")
    }

    /// The same, naming each effect so its automation can be found again in a later session.
    ///
    /// `pending` are entries a load could not place because their effect was not in the chain: they
    /// are carried straight through, so saving a pattern whose effect is not currently loaded does
    /// not quietly discard its automation.
    pub fn with_locks_in_chain(
        mut self,
        locks: &super::locks::LockSet,
        plugin: Option<&str>,
        chain: super::locks::ChainRefs<'_>,
        pending: &[super::locks::LockedParam],
    ) -> Result<Self, String> {
        if locks.param_ids().is_empty() && pending.is_empty() {
            return Ok(self);
        }
        self.locks = Some(super::locks::LockData::capture_in_chain(
            locks, plugin, chain, pending,
        )?);
        Ok(self)
    }

    /// Brings a version 1 file up to date.
    ///
    /// **The whole migration is "every step is untied"**, because a version 1 file has no ties: the
    /// concept did not exist when it was written. There is nothing to derive and nothing that could
    /// disagree with the notes — which is the second time collapsing the gate to a boolean removed
    /// a rule rather than adding one.
    fn migrate_from_v1(&mut self) {
        self.tied = vec![false; STEPS];
        // Chained, not jumped: a version 1 file is a version 2 file with no ties, and everything
        // version 2 needs doing to it needs doing to this as well. Setting the version straight to
        // the newest here is how a later migration silently stops running for the oldest files.
        self.migrate_from_v2();
    }

    /// Brings a version 2 file up to date.
    ///
    /// **The whole migration is "no step sets anything"**: locks did not exist when it was written,
    /// and `#[serde(default)]` has already produced exactly that. The function exists anyway, so
    /// that the version policy is one named function per version rather than a gap that happens to
    /// work.
    fn migrate_from_v2(&mut self) {
        // Chained, for the reason `migrate_from_v1` gives: jumping straight to the newest is how a
        // later migration silently stops running for the older files.
        self.migrate_from_v3();
    }

    /// Brings a version 3 file up to date.
    ///
    /// **The whole migration is "one bar of sixteen"**, because that is the only shape a version 3
    /// file could have had — the concept of a shape did not exist when it was written, and
    /// `#[serde(default)]` leaves both numbers zero.
    fn migrate_from_v3(&mut self) {
        self.bars = 1;
        self.steps_per_bar = super::pattern::STEPS;
        self.schema_version = SCHEMA_VERSION;
    }

    /// Parses a sequence, reporting anything it had to skip.
    ///
    /// **Explicit migration, not "accept anything older".** Each readable version has a named
    /// function and its own test; a version this build has never heard of is refused with the
    /// number named, because guessing at an unknown schema is worse than saying no. An unreadable
    /// *note* is a different matter and is only reported: one typo in step 12 should not cost the
    /// other fifteen steps.
    pub fn parse(text: &str) -> Result<(Self, Vec<String>), String> {
        let mut sequence: Sequence =
            serde_json::from_str(text).map_err(|e| format!("this is not a sequence file: {e}"))?;

        match sequence.schema_version {
            v if v == SCHEMA_VERSION => {}
            3 => sequence.migrate_from_v3(),
            2 => sequence.migrate_from_v2(),
            1 => sequence.migrate_from_v1(),
            v if v > SCHEMA_VERSION => {
                return Err(format!(
                    "the sequence is schema version {v}, but this build understands \
                     {SCHEMA_VERSION}"
                ));
            }
            v => {
                return Err(format!(
                    "schema version {v} is not one this build can read; it writes {SCHEMA_VERSION} \
                     and migrates from {OLDEST_READABLE}"
                ));
            }
        }

        let (_, mut problems) = sequence.shaped_pattern();
        problems.extend(sequence.tie_problems());
        Ok((sequence, problems))
    }

    /// The pattern this file describes, at the shape it records.
    ///
    /// **Clamped, not trusted.** The two numbers come from a file somebody may have edited, and a
    /// zero or a nonsense pair would otherwise produce a pattern of no steps. `Pattern`'s own
    /// setters do the clamping, so there is one rule for what a shape may be.
    fn shaped_pattern(&self) -> (Pattern, Vec<String>) {
        let bars = if self.bars == 0 { 1 } else { self.bars };
        let steps_per_bar = if self.steps_per_bar == 0 {
            STEPS
        } else {
            self.steps_per_bar
        };
        self.steps.to_pattern_shaped(bars, steps_per_bar)
    }

    /// What was wrong with the `tied` array, repaired rather than refused.
    ///
    /// The same treatment an over-long step list already gets. There is no unrecognised-value case
    /// to handle: JSON has exactly two booleans, which is the point of storing one.
    fn tie_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let length = self.shaped_pattern().0.len();
        if self.tied.len() < length && !self.tied.is_empty() {
            problems.push(format!(
                "the tie list has {} entries; the remaining steps were left untied",
                self.tied.len()
            ));
        }
        if self.tied.len() > length {
            problems.push(format!(
                "the tie list has {} entries; only the first {length} were used",
                self.tied.len()
            ));
        }
        problems
    }

    /// The pattern and tempo it describes.
    pub fn unpack(&self) -> (Pattern, f64, Vec<String>) {
        let (mut pattern, mut problems) = self.shaped_pattern();
        let length = pattern.len();
        for (step, tied) in self.tied.iter().take(length).enumerate() {
            pattern.set_tied(step, *tied);
        }
        problems.extend(self.tie_problems());
        let tempo = if self.tempo.is_finite() {
            self.tempo.clamp(MIN_TEMPO, MAX_TEMPO)
        } else {
            DEFAULT_TEMPO
        };
        (pattern, tempo, problems)
    }

    pub fn load(path: &Path) -> Result<(Self, Vec<String>), String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("{} could not be read: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Writes the sequence so that a crash mid-write cannot destroy the previous copy.
    ///
    /// The same temp-file-then-rename the settings use: a saved sequence is something people keep.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;

        let temp = path.with_extension("tmp");
        std::fs::write(&temp, text.as_bytes()).map_err(|e| e.to_string())?;
        std::fs::rename(&temp, path).map_err(|e| e.to_string())
    }
}

/// Where saved sequences live by default.
pub fn default_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mxm-player")
        .join("sequences")
}

/// The sequences in `dir`, by name, sorted.
pub fn list(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(EXTENSION))
        })
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    n.trim_end_matches(EXTENSION)
                        .trim_end_matches('.')
                        .to_owned()
                })
                .unwrap_or_default();
            (name, path)
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The path a sequence called `name` takes in `dir`.
pub fn path_for(dir: &Path, name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let safe = if safe.trim_matches('-').is_empty() {
        "sequence".to_owned()
    } else {
        safe
    };
    dir.join(format!("{safe}.{EXTENSION}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_pattern() -> Pattern {
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.toggle(0, 67);
        pattern.toggle(4, 63);
        pattern.toggle(15, 48);
        pattern
    }

    #[test]
    fn a_sequence_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join("mxm-seq-roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let path = path_for(&dir, "test one");

        Sequence::new("test one", &a_pattern(), 137.0)
            .save(&path)
            .expect("save");

        let (loaded, problems) = Sequence::load(&path).expect("load");
        assert!(problems.is_empty());

        let (pattern, tempo, _) = loaded.unpack();
        assert_eq!(pattern, a_pattern());
        assert_eq!(tempo, 137.0);
        assert_eq!(loaded.name, "test one");
    }

    #[test]
    fn a_saved_sequence_is_readable_and_hand_editable() {
        let sequence = Sequence::new("readable", &a_pattern(), 120.0);
        let text = serde_json::to_string_pretty(&sequence).unwrap();
        assert!(
            text.contains("\"C3\""),
            "notes should read as names:\n{text}"
        );
        assert!(text.contains("\"G3\""));
        assert!(!text.contains("4398046511104"), "no bitmasks");
    }

    #[test]
    fn a_sequence_says_nothing_about_an_instrument() {
        // The property that lets the same sequence be run through two different synths and
        // compared. If a plugin id ever appears here, that comparison is gone.
        let text = serde_json::to_string(&Sequence::new("x", &a_pattern(), 120.0)).unwrap();
        for forbidden in ["clap_id", "plugin", "param", "state"] {
            assert!(
                !text.contains(forbidden),
                "`{forbidden}` leaked into a sequence:\n{text}"
            );
        }
    }

    #[test]
    fn a_version_one_file_migrates_to_every_step_untied() {
        // The whole of the migration. A version 1 file has no ties because the concept did not
        // exist, so there is nothing to derive -- and nothing that could disagree with the notes.
        let text = r#"{"schema_version":1,"name":"old","tempo":120,"steps":[["C3"],["G3"]]}"#;
        let (sequence, problems) = Sequence::parse(text).expect("a version 1 file must load");

        assert_eq!(sequence.schema_version, SCHEMA_VERSION, "migrated forward");
        assert!(problems.is_empty(), "{problems:?}");

        let (pattern, _, _) = sequence.unpack();
        assert!(pattern.step(0).contains(60), "its notes survive");
        assert!(
            (0..STEPS).all(|step| !pattern.tied(step)),
            "and nothing is tied"
        );
    }

    #[test]
    fn a_version_zero_file_is_refused_with_its_version_named() {
        // Not migrated from, not guessed at. Zero is the case a "anything older is fine" policy
        // would silently accept and misread.
        let text = r#"{"schema_version":0,"name":"x","tempo":120,"steps":[]}"#;
        let error = Sequence::parse(text).expect_err("version 0 must be refused");
        assert!(error.contains('0'), "the version must be named: {error}");
    }

    #[test]
    fn ties_round_trip_through_a_saved_sequence() {
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true);
        pattern.set_tied(2, true);

        let sequence = Sequence::new("tied", &pattern, 120.0);
        let text = serde_json::to_string(&sequence).expect("serialise");
        let (back, problems) = Sequence::parse(&text).expect("parse");
        assert!(problems.is_empty(), "{problems:?}");

        let (restored, _, _) = back.unpack();
        assert_eq!(restored, pattern, "a tie must survive the file");
    }

    #[test]
    fn a_short_tie_list_leaves_the_rest_untied_and_says_so() {
        // The same repair-and-report an over-long step list already gets: one malformed field
        // must not cost the notes.
        let text = r#"{"schema_version":2,"name":"x","tempo":120,
                       "steps":[["C3"]],"tied":[false,true]}"#;
        let (sequence, problems) = Sequence::parse(text).expect("it must still load");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("untied"), "{problems:?}");

        let (pattern, _, _) = sequence.unpack();
        assert!(pattern.tied(1), "what was given is honoured");
        assert!(!pattern.tied(9), "and the rest is untied");
    }

    #[test]
    fn an_over_long_tie_list_is_trimmed_and_says_so() {
        let text = format!(
            r#"{{"schema_version":2,"name":"x","tempo":120,"steps":[["C3"]],"tied":[{}]}}"#,
            vec!["false"; STEPS + 4].join(",")
        );
        let (_, problems) = Sequence::parse(&text).expect("it must still load");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("only the first"), "{problems:?}");
    }

    #[test]
    fn a_future_schema_version_is_refused_rather_than_guessed_at() {
        let text = r#"{"schema_version":99,"name":"x","tempo":120,"steps":[]}"#;
        let error = Sequence::parse(text).unwrap_err();
        assert!(error.contains("99"), "{error}");
    }

    #[test]
    fn something_that_is_not_a_sequence_is_refused_with_a_reason() {
        assert!(Sequence::parse("{ oops").is_err());
        assert!(Sequence::parse("[]").is_err());
    }

    #[test]
    fn one_unreadable_note_is_reported_without_losing_the_rest() {
        let text = r#"{"schema_version":1,"name":"x","tempo":120,
                       "steps":[["C3"],["H9"],["G3"]]}"#;
        let (sequence, problems) = Sequence::parse(text).expect("still a sequence");
        assert_eq!(problems.len(), 1);

        let (pattern, _, _) = sequence.unpack();
        assert!(pattern.step(0).contains(60));
        assert!(pattern.step(2).contains(67));
    }

    #[test]
    fn an_absurd_tempo_in_a_file_is_clamped_rather_than_trusted() {
        let text = r#"{"schema_version":1,"name":"x","tempo":100000,"steps":[]}"#;
        let (sequence, _) = Sequence::parse(text).unwrap();
        let (_, tempo, _) = sequence.unpack();
        assert_eq!(tempo, MAX_TEMPO);
    }

    #[test]
    fn a_name_with_awkward_characters_still_gets_a_usable_path() {
        let dir = Path::new("/tmp/x");
        let path = path_for(dir, "my/test: seq");
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains(':'), "{name}");
        assert!(name.ends_with(EXTENSION));
    }

    #[test]
    fn an_empty_name_does_not_produce_a_dotfile() {
        let path = path_for(Path::new("/tmp/x"), "///");
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("sequence"),
            "{path:?}"
        );
    }

    #[test]
    fn listing_finds_saved_sequences_by_name() {
        let dir = std::env::temp_dir().join("mxm-seq-list");
        let _ = std::fs::remove_dir_all(&dir);
        for name in ["beta", "alpha"] {
            Sequence::new(name, &a_pattern(), 120.0)
                .save(&path_for(&dir, name))
                .unwrap();
        }
        // Something that is not a sequence must not be offered as one.
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();

        let found = list(&dir);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].0, "alpha", "sorted by name");
        assert_eq!(found[1].0, "beta");
    }

    #[test]
    fn listing_a_directory_that_does_not_exist_is_empty_rather_than_an_error() {
        assert!(list(Path::new("/definitely/not/here")).is_empty());
    }
}
