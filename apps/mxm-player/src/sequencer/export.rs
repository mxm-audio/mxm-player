//! Rendering a sequence to an audio file.
//!
//! # What makes this correct rather than merely working
//!
//! - **It renders a snapshot, not a fresh plugin.** Loading a bundle by path and id gives the
//!   plugin's *default* patch; the sound being exported has to be transferred as CLAP state.
//! - **The pattern plays once**, then releases. The live clock wraps every sixteen steps, so
//!   inheriting live playback would retrigger through the tail and nothing would ever go quiet.
//! - **The file is exactly two bars.** One of pattern plus the mandatory extra bar *is* double the
//!   sequence length, so the length is fixed rather than decided — which is also what a DAW grid
//!   wants.
//! - **The extra bar classifies the result**, it does not shorten it: silent throughout means the
//!   sound ended inside the file, anything else means it was truncated and says so.
//!
//! Everything here runs off the GUI thread. The musical cap bounds *frames*, not wall-clock: a
//! third-party plugin may initialise slowly, render slower than realtime, or not return at all.

use super::pattern::{Pattern, STEPS};

/// The silence floor, as a linear amplitude.
///
/// **−90 dBFS, measured rather than chosen.** mxm-mono-01's default-patch tail ends 0.100 s after
/// release under a −60 dBFS floor, 0.239 s under −90 and 0.241 s under −120 — the curve is vertical
/// below −90, so the exact value stops mattering there, while −60 would cut 140 ms of real decay.
/// −90 also sits just above the −96 dBFS noise floor of 16-bit audio.
pub const SILENCE_FLOOR: f32 = 3.162_277_7e-5;

// **A render is exactly as long as the sequence, and there is no constant for it.**
//
// There was: `BARS = 2`, *"one of pattern plus one to hear the tail"* — a guess the code made on the
// person's behalf, back when a sequence was always one bar. A sequence has a length now, and wanting
// a tail is said by adding empty bars, which anyone can do. Nine bars render nine bars.

/// Why the render stopped.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The sound ended inside the file.
    Finished,
    /// Still sounding at the end. A drone or a long release; reported, never silent about it.
    Truncated,
}

impl Ending {
    pub fn label(self) -> &'static str {
        match self {
            Ending::Finished => "finished",
            Ending::Truncated => "truncated at the two-bar cap",
        }
    }
}

/// A finished render.
pub struct Render {
    /// Interleaved samples.
    pub samples: Vec<f32>,
    pub channels: usize,
    pub sample_rate: f64,
    pub tempo: f64,
    pub ending: Ending,
    /// Whether normalisation was applied — it is skipped for a silent render.
    pub normalised: bool,
}

impl Render {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    pub fn seconds(&self) -> f64 {
        self.frames() as f64 / self.sample_rate
    }
}

/// How many frames one bar occupies at this tempo.
///
/// **Four beats, which is a bar only while a bar is sixteen sixteenths.** What tempo actually fixes
/// is the length of a *step* — see [`frames_per_step`] — and a bar is however many steps it holds.
/// This stays for the callers that mean "four beats", and every step calculation goes through the
/// other one.
pub fn frames_per_bar(tempo: f64, sample_rate: f64) -> usize {
    // A bar is four beats; a beat is 60/tempo seconds.
    (sample_rate * 4.0 * 60.0 / tempo).round() as usize
}

/// How many frames one **step** occupies at this tempo — a sixteenth.
///
/// The quantity tempo really fixes. A pattern's duration is its length times this, so a twelve-step
/// bar is three quarters of a four-beat one rather than a differently-subdivided version of it.
pub fn frames_per_step(tempo: f64, sample_rate: f64) -> usize {
    frames_per_bar(tempo, sample_rate) / STEPS
}

/// The largest absolute sample.
pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |p, s| p.max(s.abs()))
}

/// Scales the render so its loudest sample sits at 0 dBFS.
///
/// **A silent render is left alone**: its peak is zero, and scaling it would be a division by zero
/// that turns digital black into full-scale noise.
///
/// Returns whether anything was scaled.
pub fn normalise(samples: &mut [f32]) -> bool {
    let peak = peak(samples);
    if peak <= 0.0 || !peak.is_finite() {
        return false;
    }
    let gain = 1.0 / peak;
    for sample in samples.iter_mut() {
        *sample *= gain;
    }
    true
}

/// Whether the last bar of a render stayed below the floor throughout.
///
/// **The whole bar, not its final sample.** A delay tap that sounds in the middle of the extra bar
/// and stops before the end has not finished — it has repeated, and the next repeat would land
/// outside the file. Checking only the boundary would call that finished and cut it.
pub fn extra_bar_is_silent(samples: &[f32], channels: usize, frames_per_bar: usize) -> bool {
    let frames = samples.len() / channels.max(1);
    let start = frames.saturating_sub(frames_per_bar);
    samples[start * channels.max(1)..]
        .iter()
        .all(|s| s.abs() < SILENCE_FLOOR)
}

/// The events one traversal of the pattern produces, as `(frame, key, on)`.
///
/// **One traversal.** The live clock wraps modulo sixteen; a render that inherited that would
/// retrigger through the tail bar and never go quiet.
///
/// **A second implementation of the gate rules, deliberately.** `Runtime` answers them by
/// advancing a clock and reacting to boundaries; this answers them by arithmetic over the pattern,
/// because there is no clock offline. They must agree, and
/// `an_export_gives_a_tied_run_the_same_length_the_runtime_does` is what says they do.
pub fn schedule(pattern: &Pattern, tempo: f64, sample_rate: f64) -> Vec<(usize, u8, bool)> {
    let step_frames = frames_per_step(tempo, sample_rate);
    // The same 50% gate the sequencer plays live.
    let gate = step_frames / 2;

    // (frame, rank, key, on): the rank orders same-frame events. An ordinary run ends *at* the
    // next note's start frame, and there the runtime releases before sounding — off (0) before
    // on (1). A run ending at a **slide** is the legato joint, and there the order inverts:
    // the plugin must see the new note arrive while the old one is still held, so a legato off
    // ranks *after* the ons (2). One sort key carrying both, because a plain off-before-on sort
    // handed a glide-on-legato instrument two disjoint notes at every slide — a retrigger in the
    // render where live playback slides.
    let mut events = Vec::new();
    for (index, step) in pattern.steps() {
        if step.is_empty() {
            continue;
        }
        let start = index * step_frames;

        // The 50% gate survives only when both of the runtime's suppression clauses are false.
        // `held_past_gate` is that rule, wrap included; `run_ends_at` is where the offline
        // difference lives, truncating at the bar so a wrapped run cannot sound through the tail
        // bar that classifies the render.
        let (end, ends_in_slide) = if pattern.held_past_gate(index) {
            let end_step = pattern.run_ends_at(index);
            let slide = end_step < pattern.len()
                && pattern.tied(end_step)
                && !pattern.step(end_step).is_empty();
            (end_step * step_frames, slide)
        } else {
            (start + gate, false)
        };

        let off_rank = if ends_in_slide { 2u8 } else { 0u8 };
        step.for_each(|key| {
            events.push((start, 1u8, key, true));
            events.push((end, off_rank, key, false));
        });
    }
    events.sort_by_key(|(frame, rank, key, _)| (*frame, *rank, *key));

    // **A same-pitch legato joint is merged into one unbroken note.** The schedule renders as
    // raw MIDI, which carries no voice identity — so a slide from C3 to C3 would send note-on
    // C3 then note-off C3 at one frame, and the off can only kill the note that just sounded.
    // Live playback disambiguates through the press table (`emit_legato_joint` releases the
    // *old* press); offline the audibly identical answer is to send nothing at the joint for
    // that key — the note simply continues, and the slide's own off releases it later. Per
    // key: a slide to a *different* pitch keeps its on and the old note's legato off, and in
    // a chord each key is judged alone.
    let joints: std::collections::HashSet<(usize, u8)> = {
        let ons: std::collections::HashSet<(usize, u8)> = events
            .iter()
            .filter(|(_, rank, _, on)| *on && *rank == 1)
            .map(|(frame, _, key, _)| (*frame, *key))
            .collect();
        events
            .iter()
            .filter(|(frame, rank, key, on)| !*on && *rank == 2 && ons.contains(&(*frame, *key)))
            .map(|(frame, _, key, _)| (*frame, *key))
            .collect()
    };
    events.retain(|(frame, rank, key, _)| {
        !(joints.contains(&(*frame, *key)) && (*rank == 1 || *rank == 2))
    });

    events
        .into_iter()
        .map(|(frame, _, key, on)| (frame, key, on))
        .collect()
}

/// Where rendered files go by default.
pub fn default_dir() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("mxm-player")
        .join("exports")
}

/// The file name a render takes: the de-facto sample-library convention.
///
/// Tempo and bar count in the name, so a person reading a file browser knows what it is without
/// opening it. This is what the design relies on for tempo — the `acid` chunk is a bonus whose
/// interoperability is unverified.
pub fn file_name(stem: &str, tempo: f64, bars: u32) -> String {
    let safe: String = stem
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
    format!("{safe}_{}bpm_{bars}bars", tempo.round() as u32)
}

/// Where each step's modulation lands, in frames.
///
/// **Every locked parameter on every step**, as an offset from the patch — including the zeroes on
/// the steps that set nothing. That is what the live runtime emits, and an export that visited only
/// the steps that deviate would leave the previous step's offset in force for the rest of the bar:
/// the pattern would sound one way live and another way rendered, which is the one thing export must
/// never do.
///
/// The pattern plays once in an export, so unlike the live runtime there is no wrap. Ordered by step
/// and then by parameter id, so a render is a function of the sequence and not of which knob was
/// touched first.
#[must_use]
pub fn schedule_locks(
    pattern: &Pattern,
    locks: &LockSet,
    tempo: f64,
    sample_rate: f64,
) -> Vec<(usize, crate::sequencer::locks::LockKey, f32)> {
    let step_frames = frames_per_step(tempo, sample_rate);

    let mut events = Vec::new();
    for step in 0..pattern.len() {
        let mut at_step = Vec::new();
        // **A step's locks are its own** — per step, tied ones included, exactly as the live
        // runtime reads them. Nothing resolves a step's locks to any other step any more, so
        // live and rendered agree by reading the same cell rather than by two resolvers kept in
        // step.
        locks.for_each_at(step, |param_id, offset| at_step.push((param_id, offset)));
        at_step.sort_unstable_by_key(|(id, _)| *id);
        events.extend(
            at_step
                .into_iter()
                .map(|(param_id, offset)| (step * step_frames, param_id, offset)),
        );
    }

    // **And zero at the end of the sequence.** The pattern does not wrap in a render, so without
    // this the last step's offset would stay applied through whatever follows and a release would
    // ring out under a deviation live playback takes off when it stops. The two would then differ in
    // exactly the part nobody listens to closely enough to catch.
    // `param_ids` is sorted, so these arrive in the same order as every other step's — a render is
    // a function of the sequence, not of which knob was touched first, and that has to hold for the
    // last events as much as the first.
    let end = pattern.len() * step_frames;
    for param_id in locks.param_ids() {
        events.push((end, param_id, 0.0));
    }
    events
}

// --- rendering ----------------------------------------------------------------------------------

use crate::offline::{EffectSpec, EventKind, RenderConfig, ScheduledEvent};
use crate::sequencer::locks::LockSet;
use std::path::Path;

/// Renders a pattern through a *separate* instance of the plugin, restoring `state`.
///
/// Off the GUI thread by construction: nothing here touches the app. The plugin instance lives
/// entirely for the duration of this call, which is also its CLAP main thread — so there is no
/// cross-thread lifecycle to service.
#[allow(clippy::too_many_arguments)]
pub fn render_offline(
    bundle: &Path,
    plugin_id: &str,
    state: Vec<u8>,
    pattern: Pattern,
    locks: LockSet,
    tempo: f64,
    sample_rate: f64,
    normalise_it: bool,
    effects: &[EffectSpec],
    // The chain's ids, in the same order as `effects`, so an effect's automation can be matched
    // to the effect it belongs to.
    fx_order: &[crate::engine::fx::FxId],
) -> Result<Render, String> {
    // The sequence's own length, in frames. `frames_per_step` rather than `frames_per_bar` because
    // a bar is however many steps it holds — twelve of them is a 3/4 bar and three quarters as long.
    let total = pattern.len() * frames_per_step(tempo, sample_rate);

    // **The locks first, and at the same frame as the notes they belong to.** The restored state is
    // the patch; a step's locks are the deviations from it, and a note sounded before its cutoff
    // arrives is a note played on the previous step's sound. `push_event` preserves the order it is
    // given within a frame, which is the same guarantee the live path relies on.
    // **Split by target where the chain is known.** The source's automation goes into its own
    // stream; an effect's goes onto that effect's `EffectSpec`, so a render carries every plugin's
    // automation exactly as live playback does. Nothing downstream has to understand targets.
    let mut effects: Vec<EffectSpec> = effects.to_vec();
    let scheduled = schedule_locks(&pattern, &locks, tempo, sample_rate);
    let mut events: Vec<ScheduledEvent> = Vec::new();
    for (frame, key, value) in scheduled {
        let event = ScheduledEvent {
            frame: frame as u64,
            kind: EventKind::ParamMod {
                param_id: key.param_id,
                value: f64::from(value),
            },
        };
        if key.is_source() {
            events.push(event);
        } else if let Some(index) = fx_order.iter().position(|id| *id == key.fx) {
            // A target the caller did not describe is dropped rather than sent to the source: an
            // export that quietly moved the wrong plugin's parameter is the defect this whole
            // change exists to make impossible.
            if let Some(spec) = effects.get_mut(index) {
                spec.events.push(event);
            }
        }
    }

    events.extend(
        schedule(&pattern, tempo, sample_rate)
            .into_iter()
            .map(|(frame, key, on)| ScheduledEvent {
                frame: frame as u64,
                kind: if on {
                    EventKind::Midi {
                        data: [0x90, key, super::smf::VELOCITY],
                    }
                } else {
                    EventKind::Midi {
                        data: [0x80, key, 0],
                    }
                },
            }),
    );
    // Stable, so the locks scheduled above stay ahead of the notes at the same frame.
    events.sort_by_key(|event| event.frame);

    const BLOCK: u32 = 512;
    let result = crate::offline::render(
        bundle,
        plugin_id,
        RenderConfig {
            sample_rate,
            block_size: BLOCK,
            total_frames: total as u64,
            state: Some(state),
        },
        &events,
    )
    .map_err(|e| e.to_string())?;
    // Then the chain, every effect that is on with its own patch: an export that skipped it
    // would sound like the instrument alone, which is not what was being listened to.
    let result = crate::offline::through_effects(result, &effects, sample_rate, BLOCK)
        .map_err(|e| e.to_string())?;

    let channels = result.channels.len().max(1);
    let frames = result.channels.first().map(Vec::len).unwrap_or(0);
    let mut samples = Vec::with_capacity(total * channels);
    for frame in 0..frames {
        for channel in &result.channels {
            samples.push(channel.get(frame).copied().unwrap_or(0.0));
        }
    }

    // **Over the sequence's last bar**, before normalisation moves the level: a delay tap that
    // sounds mid-bar has not finished, whatever the final sample says.
    //
    // This used to be a *mandatory extra* bar the render appended. It is the last bar of the
    // sequence now — a render is exactly as long as the sequence, and somebody who wants a tail
    // adds empty bars, which is also what makes this report `Finished` for them.
    let last_bar = pattern.steps_per_bar() * frames_per_step(tempo, sample_rate);
    let ending = if extra_bar_is_silent(&samples, channels, last_bar) {
        Ending::Finished
    } else {
        Ending::Truncated
    };

    let normalised = normalise_it && normalise(&mut samples);

    Ok(Render {
        samples,
        channels,
        sample_rate,
        tempo,
        ending,
        normalised,
    })
}

/// Writes the render as a 32-bit float WAV with an `acid` chunk.
///
/// Float because a hot patch cannot clip on the way out and normalising to exactly 0 dBFS is then
/// safe — fixed point would risk inter-sample overshoot.
pub fn write_wav(render: &Render, path: &Path) -> Result<(), String> {
    // The collection's encoder writes the float WAV. A non-finite sample is refused rather than
    // written: a NaN in an export is a plugin defect the file must not hide.
    let (bytes, _) = mxm_audio_file::encode(
        &render.samples,
        render.channels as u16,
        render.sample_rate as u32,
        mxm_audio_file::Target::WavFloat32,
    )
    .map_err(|e| e.to_string())?;

    // **The sequence's beats, not a constant.** A step is a sixteenth, so four to a beat; a
    // twelve-step bar is three beats. Rounded, because the ACID chunk counts whole beats and a
    // sequence whose length is not a multiple of four does not have a whole number of them — the
    // rounding is in the metadata only and the audio is unaffected.
    // `frames`, not `samples.len()` — the samples are interleaved, so the raw length is frames
    // times channels and would make a stereo render twice as many beats as it is.
    let frames_per_beat = render.sample_rate * 60.0 / render.tempo;
    let beats = ((render.frames() as f64 / frames_per_beat).round() as u32).max(1);
    let bytes = mxm_audio_file::acid::with_acid_chunk(bytes, render.tempo, beats)?;

    // Atomic, with the parent created: a failure or a crash never leaves a half-written file
    // looking finished.
    mxm_audio_file::write_atomic(path, &bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_export_gives_a_tied_run_the_same_length_the_runtime_does() {
        // Two implementations of one rule: `Runtime` reacts to clock boundaries, `schedule` does
        // arithmetic over the pattern. This is the seam where they could drift apart silently.
        let bar = frames_per_bar(120.0, 48_000.0);
        let step = bar / STEPS;

        for ties in 1..=3usize {
            let mut pattern = Pattern::empty();
            pattern.toggle(0, 60);
            for index in 1..=ties {
                pattern.set_tied(index, true);
            }

            let events = schedule(&pattern, 120.0, 48_000.0);
            let on = events.iter().find(|(_, _, on)| *on).expect("a note on");
            let off = events.iter().find(|(_, _, on)| !*on).expect("a note off");
            assert_eq!(
                off.0 - on.0,
                step * (ties + 1),
                "a note plus {ties} ties is {} steps offline, exactly as it is live",
                ties + 1
            );
        }
    }

    #[test]
    fn an_untied_note_keeps_its_half_step_gate_in_an_export() {
        let bar = frames_per_bar(120.0, 48_000.0);
        let step = bar / STEPS;

        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        let events = schedule(&pattern, 120.0, 48_000.0);
        assert_eq!(events[1].0 - events[0].0, step / 2);
    }

    #[test]
    fn a_run_that_would_wrap_is_truncated_at_the_bar_rather_than_running_into_the_tail() {
        // Live, a tie on step 1 holds step 16's note across the loop point, and that is correct
        // for a repeating pattern. A render plays the pattern **once**: inheriting the wrap would
        // sound a note through the extra bar, and "silent throughout" is what classifies a render
        // as finished. A correct render would be reported as truncated.
        let bar = frames_per_bar(120.0, 48_000.0);
        let mut pattern = Pattern::empty();
        pattern.toggle(15, 60);
        pattern.set_tied(0, true);

        let events = schedule(&pattern, 120.0, 48_000.0);
        let off = events.iter().find(|(_, _, on)| !*on).expect("a note off");
        assert_eq!(
            off.0, bar,
            "the run must end at the bar, not carry into the tail"
        );
    }

    #[test]
    fn a_tie_carrying_notes_ends_the_previous_run_at_the_step_boundary() {
        // The load-bearing word in "a note followed by k *empty* ties".
        let bar = frames_per_bar(120.0, 48_000.0);
        let step = bar / STEPS;

        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true);
        pattern.toggle(1, 67);

        let events = schedule(&pattern, 120.0, 48_000.0);
        let off = events
            .iter()
            .find(|(_, key, on)| !*on && *key == 60)
            .expect("C3 must end");
        assert_eq!(
            off.0, step,
            "C3 ends where G3 begins, not extended by it and not cut at its own gate"
        );
    }

    #[test]
    fn a_bar_at_120_bpm_is_two_seconds() {
        assert_eq!(frames_per_bar(120.0, 48_000.0), 96_000);
        assert_eq!(frames_per_bar(240.0, 48_000.0), 48_000);
    }

    #[test]
    fn the_floor_is_ninety_decibels_down() {
        let db = 20.0 * f64::from(SILENCE_FLOOR).log10();
        assert!((db + 90.0).abs() < 0.01, "floor is {db} dBFS");
    }

    #[test]
    fn normalising_puts_the_peak_at_full_scale() {
        let mut samples = vec![0.1, -0.25, 0.05];
        assert!(normalise(&mut samples));
        assert!((peak(&samples) - 1.0).abs() < 1e-6);
        // And the shape is unchanged.
        assert!((samples[0] / samples[1] + 0.4).abs() < 1e-6);
    }

    #[test]
    fn a_silent_render_is_not_scaled() {
        // Otherwise digital black becomes full-scale noise, via a division by zero.
        let mut samples = vec![0.0f32; 64];
        assert!(!normalise(&mut samples));
        assert!(samples.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn the_pattern_is_scheduled_once_not_looped() {
        // The live clock wraps every sixteen steps. A render that inherited that would retrigger
        // through the tail bar and never go quiet, so every export would hit the cap.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        let events = schedule(&pattern, 120.0, 48_000.0);
        assert_eq!(events.len(), 2, "one note on, one note off: {events:?}");

        let bar = frames_per_bar(120.0, 48_000.0);
        assert!(
            events.iter().all(|(frame, _, _)| *frame < bar),
            "nothing may be scheduled past the first bar"
        );
    }

    #[test]
    fn every_step_gets_a_release_before_the_next_step_starts() {
        let mut pattern = Pattern::empty();
        for step in 0..STEPS {
            pattern.toggle(step, 60);
        }
        let events = schedule(&pattern, 120.0, 48_000.0);
        let step_frames = frames_per_bar(120.0, 48_000.0) / STEPS;

        for pair in events.chunks(2) {
            let (on_frame, _, on) = pair[0];
            let (off_frame, _, off) = pair[1];
            assert!(on && !off, "each step is an on then an off");
            assert!(
                off_frame - on_frame <= step_frames,
                "the gate must close within its step"
            );
        }
    }

    #[test]
    fn a_bar_that_sounds_anywhere_is_not_silent() {
        let bar = 1000;
        let mut samples = vec![0.0f32; bar * 3];
        // A tap in the middle of the last bar, silent by its end.
        samples[bar * 2 + 100] = 0.5;
        assert!(
            !extra_bar_is_silent(&samples, 1, bar),
            "a tap mid-bar means the sound has not finished"
        );
        // Checking only the final sample would have called this finished.
        assert_eq!(samples[samples.len() - 1], 0.0);
    }

    #[test]
    fn a_genuinely_quiet_bar_is_silent() {
        let bar = 1000;
        let mut samples = vec![0.0f32; bar * 3];
        samples[10] = 0.9; // loud, but in the first bar
        samples[bar * 2 + 50] = SILENCE_FLOOR / 2.0; // below the floor
        assert!(extra_bar_is_silent(&samples, 1, bar));
    }

    #[test]
    fn the_file_name_carries_tempo_and_length() {
        assert_eq!(file_name("acid", 120.0, 2), "acid_120bpm_2bars");
        assert_eq!(file_name("my patch!", 137.4, 2), "my-patch-_137bpm_2bars");
    }

    #[test]
    fn an_empty_name_does_not_produce_a_dotfile() {
        assert!(file_name("///", 120.0, 2).starts_with("sequence"));
    }
}
