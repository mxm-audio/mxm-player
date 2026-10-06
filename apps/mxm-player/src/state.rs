//! What the player is showing, as data.
//!
//! One structure carrying everything the window displays: engine state, the negotiated envelope,
//! meters, parameters, held notes, refusals, the log tail and MIDI topology.
//!
//! It has three consumers, and the third is the reason it exists at all:
//!
//! 1. UI tests assert against it instead of against pixels.
//! 2. Session tests record it alongside the audio they rendered.
//! 3. **An agent investigating a problem reads it** rather than taking a screenshot and squinting.
//!
//! # Determinism
//!
//! Everything here is reproducible **except the fields marked as timing**. `Meters` divides
//! wall-clock durations, so load figures and deadline counts can never repeat exactly. They are
//! carried as diagnostics and excluded from any byte-for-byte comparison — see
//! [`PlayerState::without_timing`].

use serde::{Deserialize, Serialize};

/// The whole visible state of the player.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    pub engine: String,
    /// Present once a plugin is loaded.
    pub plugin: Option<PluginState>,
    /// The effect chain after it, in order. Empty when there is none.
    #[serde(default)]
    pub fx: Vec<FxState>,
    pub audio: AudioState,
    pub midi: MidiState,
    pub keyboard: KeyboardState,
    pub sequencer: SequencerView,
    /// Plugins found by the last scan, in the order the browser lists them.
    pub found: Vec<FoundState>,
    /// The most recent status line, if any.
    pub status: Option<String>,
    /// The tail of the plugin's own log output.
    pub log: Vec<String>,
    /// Wall-clock derived, and therefore never reproducible. Excluded from comparisons.
    pub timing: TimingState,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginState {
    pub id: String,
    pub bundle: String,
    pub channels: u32,
    /// How the audio layout was chosen, as shown in the panel.
    pub selection: String,
    pub note_input: Option<String>,
    pub note_output: Option<String>,
    /// Whether the plugin can be sent voice IDs, which decides whether cleanup can be targeted.
    pub carries_voice_ids: bool,
    /// `None` when the plugin does not implement the latency extension — not the same as zero.
    pub latency_samples: Option<u32>,
    pub params: Vec<ParamState>,
}

/// One effect in the chain, as the strip shows it: what it is, whether it is on, and whether
/// its editor is the one open. No parameters — the player shows none for an effect.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FxState {
    pub id: String,
    pub bundle: String,
    pub bypassed: bool,
    pub input_channels: u32,
    pub output_channels: u32,
    /// How the audio layout was chosen.
    pub selection: String,
    /// Whether this effect's own editor is the open one.
    pub editor_open: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ParamState {
    pub id: u32,
    pub name: String,
    /// The plugin's own grouping for this parameter — CLAP's `param_info.module`, which nice-plug
    /// fills from `#[nested(group = …)]`. Empty for a plugin that declares none, which is most of
    /// them.
    ///
    /// **Carried into the serialisable state so a check can assert which group a parameter is in
    /// rather than where it landed on screen.** The panel's tabs are derived from it, and a test
    /// that reached for a control by pixel position is what four of them did before.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub module: String,
    pub value: f64,
    /// The plugin's own formatting, which is what the panel shows.
    pub text: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    /// What the **sequencer** is currently adding to it, if anything.
    ///
    /// Separate from `value` because they are separate things: `value` is what the parameter is set
    /// to and what a knob shows, and this is the offset a step is laying over it. Adding them is
    /// what the instrument is sounding right now; neither alone is.
    ///
    /// Zero for almost everything, so it is skipped when it is — a dump of twenty-seven parameters
    /// should not carry twenty-seven zeroes to say nothing happened.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub modulation: f64,
}

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

/// The sequencer, as data.
///
/// `transport` and `step` are read from the **audio thread's** playhead rather than recomputed
/// here, so a headless test asserts on the same number the sound came from.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SequencerView {
    pub transport: String,
    pub step: usize,
    /// The bar being sounded, or absent when nothing is -- what the bar strip lights a chip from.
    #[serde(default)]
    pub playing_bar: Option<usize>,
    pub tempo: f64,
    /// The sequence's shape: `bars` of `steps_per_bar` steps each.
    #[serde(default)]
    pub bars: usize,
    #[serde(default)]
    pub steps_per_bar: usize,
    /// Which bar the step row shows — the chips' outline.
    #[serde(default)]
    pub selected_bar: usize,
    /// `Loop [ Bar | Pattern | All ]`: what the transport repeats — "bar", "pattern" or "all".
    #[serde(default)]
    pub loop_scope: String,
    /// One flag per bar: whether it holds any notes — the chips' fill.
    #[serde(default)]
    pub bars_with_notes: Vec<bool>,
    /// Sixteen steps of note names, the same shape a saved sequence takes.
    pub steps: Vec<Vec<String>>,
    /// Which steps continue the note before them.
    ///
    /// Ties are visible, deterministic app state, so a session dump carries them: without this a
    /// scripted run could set one and have no way to read it back.
    #[serde(default)]
    pub tied: Vec<bool>,
    pub selected_step: Option<usize>,
    /// The whole selection, ascending — the anchor plus every shift/ctrl companion. Empty when
    /// nothing is selected.
    #[serde(default)]
    pub selected_steps: Vec<usize>,
    /// What each step **sets**, one entry per locked parameter, ordered by parameter id.
    ///
    /// Ordered so a dump is a function of the sequence rather than of which knob was touched first
    /// — two identical sessions must produce byte-identical dumps, and arrival order is exactly the
    /// kind of thing that would quietly break that.
    ///
    /// `null` where a step sets nothing, for the reason [`crate::sequencer::locks::LockData`] gives:
    /// the NaN that is right in memory is not a JSON number.
    #[serde(default)]
    pub locks: Vec<LockView>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LockView {
    pub param_id: u32,
    /// Which effect in the chain this parameter belongs to, absent for the source.
    ///
    /// **A dump that showed only the parameter id could not tell two plugins' automation apart**,
    /// which is the same ambiguity the lock model itself had to lose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx: Option<crate::engine::fx::FxId>,
    /// The parameter's name, when a plugin is loaded to ask. A dump with bare numbers in it is
    /// readable by a test and by nobody else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub steps: Vec<Option<f32>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioState {
    pub device: Option<String>,
    pub sample_rate: Option<u32>,
    pub buffer_size: Option<u32>,
    /// Present while the stream is dead and the engine is trying to come back.
    #[serde(default)]
    pub reconnect: Option<ReconnectState>,
}

/// The engine's reconnect in progress, as the status bar shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReconnectState {
    pub attempts: u32,
    /// The backend's reason for the last refused attempt, if one was refused.
    pub last_failure: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MidiState {
    pub available_inputs: Vec<String>,
    pub available_outputs: Vec<String>,
    pub connected_inputs: Vec<String>,
    /// Ports that could not be connected, each with the reason shown beside it.
    pub refused_inputs: Vec<RefusedInputState>,
    pub output: Option<String>,
    pub output_faulted: bool,
    pub thru: bool,
}

/// A MIDI input the player could not open, as the panel shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RefusedInputState {
    pub port: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyboardState {
    pub octave: i32,
    pub sustain: bool,
    /// Notes sounding right now, from every source.
    pub held: Vec<u8>,
}

/// One entry in the plugin browser.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FoundState {
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// The directory it was found in — what distinguishes two copies of the same plugin.
    pub location: String,
    pub supported: bool,
    /// Why it was refused, in the words the browser shows.
    pub refusal: Option<String>,
    /// Whether it can sit in the effect chain — the other slot, judged by its ports.
    #[serde(default)]
    pub effect: bool,
    /// Why it cannot be an effect, in the words the effect picker shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_refusal: Option<String>,
}

/// Figures derived from wall-clock measurement. Never reproducible.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimingState {
    pub plugin_load: f32,
    pub callback_load: f32,
    pub missed_deadlines: u64,
    pub callbacks: u64,
    /// `None` when the backend does not report xruns, which is not the same as reporting zero.
    pub xruns: Option<u64>,
    pub realtime_priority: String,
}

impl PlayerState {
    /// The same state with every wall-clock-derived figure zeroed.
    ///
    /// This is what two runs of one session are compared on. Comparing the whole structure would
    /// fail on load percentages that can never repeat, which would make the determinism claim
    /// noise rather than a signal.
    pub fn without_timing(&self) -> Self {
        Self {
            timing: TimingState::default(),
            ..self.clone()
        }
    }

    /// Pretty JSON, for writing next to a rendered WAV or reading during an investigation.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// Whether a plugin with this ID is listed, whatever its support status.
    pub fn lists_plugin(&self, id: &str) -> bool {
        self.found.iter().any(|f| f.id == id)
    }

    /// The parameter with this name, if the loaded plugin has one.
    pub fn param(&self, name: &str) -> Option<&ParamState> {
        self.plugin.as_ref()?.params.iter().find(|p| p.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_timing(load: f32) -> PlayerState {
        PlayerState {
            engine: "running".to_owned(),
            timing: TimingState {
                plugin_load: load,
                callbacks: load as u64,
                ..TimingState::default()
            },
            ..PlayerState::default()
        }
    }

    #[test]
    fn timing_is_excluded_from_the_determinism_comparison() {
        // Two runs of the same session differ only in figures that cannot repeat.
        let a = state_with_timing(0.11);
        let b = state_with_timing(0.37);

        assert_ne!(a, b, "the raw states differ, as they always will");
        assert_eq!(
            a.without_timing(),
            b.without_timing(),
            "with timing excluded, the same session is the same state"
        );
    }

    #[test]
    fn a_real_difference_still_shows_through() {
        let mut a = state_with_timing(0.11);
        let b = state_with_timing(0.11);
        a.engine = "wedged".to_owned();

        assert_ne!(
            a.without_timing(),
            b.without_timing(),
            "excluding timing must not excuse a genuine difference"
        );
    }

    #[test]
    fn state_round_trips_through_json() {
        let state = PlayerState {
            engine: "running".to_owned(),
            status: Some("2 plugins found".to_owned()),
            ..PlayerState::default()
        };
        let parsed: PlayerState = serde_json::from_str(&state.to_json()).expect("valid json");
        assert_eq!(parsed, state);
    }
}
