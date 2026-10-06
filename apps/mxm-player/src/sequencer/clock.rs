//! One clock: the beat position. Everything else is derived from it.
//!
//! # Why one number rather than three
//!
//! The obvious design keeps a frame counter for step starts, a latched step duration for the gate,
//! and a beat position for the transport. Those are three quantities that must agree, with nothing
//! forcing them to — and they come apart the first time somebody drags the tempo. Double the tempo
//! halfway through a 500 ms step and a gate latched at 50% of the old duration falls **after** the
//! next step has already started.
//!
//! So there is one accumulator, measured in **sixteenths**, and:
//!
//! | Derived | From |
//! |---|---|
//! | step index | `floor(position) mod 16` |
//! | step phase | `fract(position)` |
//! | step start | position crossing a whole number |
//! | gate close | position crossing a half |
//! | transport beats | `position / 4` |
//!
//! Nothing is latched, so a tempo change has nothing to invalidate: it only alters how fast the
//! position advances. **A gate-close boundary cannot cross its next step start, because 0.5 < 1.0
//! by construction** rather than by arithmetic somebody has to keep correct. And the transport
//! cannot disagree with the sequencer, because it is not a second derivation of the same idea — it
//! is the same number.
//!
//! **A boundary and a note are two different things, and ties made the distinction matter.** The
//! sentence above is about the *boundary*, which still lands where it always did: `GateClose` is
//! emitted at every half-step and never past a `StepStart`. What can now outlast a step is the
//! **note**, because [`Runtime`](super::runtime::Runtime) declines to act on a boundary when the
//! step is tied. Nothing here changed to allow that — no per-step gate length, no latched duration,
//! which is exactly the design this module doc exists to defend. The clock still says *here is a
//! boundary*; deciding what a boundary means was never its job.
//!
//! # Why not `steady_time`
//!
//! `AudioWorker::steady_time` looks like a free playhead and is not one: `render` writes silence
//! and `continue`s **before** incrementing it, so it advances only while `RunState::Running`. A
//! rest produces quiet buffers, quiet buffers sleep the plugin, and a sequencer keyed on
//! `steady_time` would then never reach its next boundary — stuck for good, on its first rest.
//!
//! This clock advances for every output frame while playing, whatever the plugin is doing.

use super::pattern::{STEPS, STEPS_PER_BEAT};

/// The tempo range the transport accepts.
pub const MIN_TEMPO: f64 = 20.0;
pub const MAX_TEMPO: f64 = 300.0;
pub const DEFAULT_TEMPO: f64 = 120.0;

/// Where the gate closes within a step. Half, so every step retriggers the envelope — which is
/// what you want when judging a sound, and which sidesteps the legato question a full gate raises.
pub const GATE: f64 = 0.5;

/// What the transport is doing.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    #[default]
    Stopped,
    Playing,
    Paused,
}

impl Transport {
    pub fn is_playing(self) -> bool {
        self == Transport::Playing
    }

    pub fn label(self) -> &'static str {
        match self {
            Transport::Stopped => "stopped",
            Transport::Playing => "playing",
            Transport::Paused => "paused",
        }
    }
}

/// Something the sequencer must do at a known sample offset inside a chunk.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// The loop window just changed, at a bar's end, as queued. **Emitted immediately before the
    /// first `StepStart` of the new window**, and the runtime must release whatever is sounding on
    /// it: two isolated bars are two sequences, and the destination bar may open with a tie —
    /// whose `StepStart` deliberately releases nothing.
    WindowSwitch { frame: u32 },
    /// Release the previous step, then sound this one.
    StepStart { frame: u32, step: usize },
    /// Release the step that is sounding. Always earlier than the next `StepStart`.
    ///
    /// `step` is the step **whose** gate this is — the one that started at the preceding whole
    /// position. The clock has always known it; it began reporting it when ties arrived, because
    /// whether to act on this boundary depends on that step and the one after it. Reporting it is
    /// not a change to *when* the boundary lands.
    GateClose { frame: u32, step: usize },
}

impl Boundary {
    pub fn frame(self) -> u32 {
        match self {
            Boundary::StepStart { frame, .. }
            | Boundary::GateClose { frame, .. }
            | Boundary::WindowSwitch { frame } => frame,
        }
    }
}

/// The most boundaries one chunk can produce before the clock refuses to go further.
///
/// A guard, not a limit that is ever reached in use: at 300 BPM and 48 kHz a 512-frame chunk holds
/// about half a step. It exists so a nonsense sample rate cannot make the audio thread loop for a
/// long time.
pub const MAX_BOUNDARIES_PER_CHUNK: usize = 64;

/// Where a queued window enters when it is applied.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Land on the window's own first step — selecting a bar to loop.
    WindowStart,
    /// Land on the step after the bar being left — widening back to the whole sequence, which
    /// continues into the next bar rather than jumping to step one.
    Continue,
}

/// The sequencer's position, in sixteenths.
#[derive(Copy, Clone, Debug)]
pub struct Clock {
    /// Sixteenths since the run started. The single source of everything else.
    ///
    /// **Monotonic across window switches.** A queued bar jump rebases [`Clock::origin`], never
    /// this — the transport's beat timeline keeps counting through the jump, exactly as it does
    /// across the ordinary loop wrap.
    position: f64,
    tempo: f64,
    sample_rate: f64,
    /// The first step of the loop window. Zero except in `Bar` scope, where the selected bar is
    /// the sequence.
    window_start: usize,
    /// How many steps the loop is, which is what the position wraps against.
    ///
    /// **Held here rather than read from the pattern** because the clock is what wraps, and a clock
    /// that asked something else for its own loop point would be two owners of one fact. `Runtime`
    /// sets it from the pattern whenever state is applied.
    length: usize,
    /// The position at which the current window began; the step mapping counts from here. Rebased
    /// exactly when a queued window is applied, which is what makes the jump land on the new
    /// window's entry step while `position` runs on.
    origin: f64,
    /// A window change waiting for the current bar's end. An instant jump lands off the downbeat;
    /// every hardware sequencer queues instead, and so does this.
    pending: Option<(usize, usize, Entry)>,
    /// Steps per bar, which is what "the current bar's end" is measured in. In `Bar` scope the
    /// window is one bar, so its end and the wrap coincide; in `All` scope the edges are every
    /// multiple of this.
    bar_length: usize,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            position: 0.0,
            tempo: DEFAULT_TEMPO,
            sample_rate: 48_000.0,
            window_start: 0,
            length: STEPS,
            origin: 0.0,
            pending: None,
            bar_length: STEPS,
        }
    }
}

impl Clock {
    pub fn new(tempo: f64, sample_rate: f64) -> Self {
        Self {
            tempo: tempo.clamp(MIN_TEMPO, MAX_TEMPO),
            sample_rate: sample_rate.max(1.0),
            ..Self::default()
        }
    }

    pub fn tempo(&self) -> f64 {
        self.tempo
    }

    /// Sets the tempo. Called at a chunk boundary, never inside one, so that a chunk has exactly
    /// one tempo — which is what makes the CLAP transport coherent as of sample 0.
    pub fn set_tempo(&mut self, tempo: f64) {
        self.tempo = tempo.clamp(MIN_TEMPO, MAX_TEMPO);
    }

    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate.max(1.0);
    }

    /// Sixteenths per frame at the current tempo.
    fn per_frame(&self) -> f64 {
        // A 16th is a quarter-beat: beats/second is tempo/60, so sixteenths/second is tempo/15.
        self.tempo / (15.0 * self.sample_rate)
    }

    pub fn position(&self) -> f64 {
        self.position
    }

    /// The step that is sounding, as an absolute index into the pattern.
    pub fn step(&self) -> usize {
        self.step_at(self.position)
    }

    /// The absolute step a position maps to: the window's start plus the wrapped offset since the
    /// window began. One expression, used for the playhead and for every boundary emitted.
    fn step_at(&self, position: f64) -> usize {
        self.window_start
            + ((position - self.origin).floor() as i64).rem_euclid(self.length as i64) as usize
    }

    /// Sets the loop length in steps, clamped to at least one. The window starts at zero — the
    /// whole sequence, which is `All` scope and every caller that predates scopes.
    ///
    /// **The position is left where it is.** A length change mid-run should not restart the bar, and
    /// `step()` wraps against the new length on the next read — so shortening while playing lands
    /// wherever that arithmetic puts it, which is the same thing hardware does.
    pub fn set_length(&mut self, length: usize) {
        self.window_start = 0;
        self.length = length.max(1);
        self.pending = None;
    }

    /// Sets the loop window immediately: `Bar` scope while at rest, or a restart.
    pub fn set_window(&mut self, start: usize, length: usize) {
        self.window_start = start;
        self.length = length.max(1);
        self.pending = None;
    }

    /// Queues a window change for **the current bar's end** — the next step-start whose in-window
    /// offset is a multiple of [`Clock::bar_length`], the wrap included. Applied inside `advance`,
    /// which emits [`Boundary::WindowSwitch`] at the frame it happens. A later queue replaces an
    /// earlier one: the person changed their mind before the bar ended.
    pub fn queue_window(&mut self, start: usize, length: usize, entry: Entry) {
        self.pending = Some((start, length.max(1), entry));
    }

    /// Tells the clock what a bar is, so "the current bar's end" means something in `All` scope.
    pub fn set_bar_length(&mut self, steps: usize) {
        self.bar_length = steps.max(1);
    }

    pub fn window(&self) -> (usize, usize) {
        (self.window_start, self.length)
    }

    pub fn length(&self) -> usize {
        self.length
    }

    /// How far through that step, `0.0..1.0`.
    pub fn phase(&self) -> f64 {
        self.position - self.position.floor()
    }

    /// The transport's beat position. Not a separate quantity — the same number, rescaled.
    pub fn beats(&self) -> f64 {
        self.position / STEPS_PER_BEAT
    }

    pub fn seconds(&self) -> f64 {
        self.beats() * 60.0 / self.tempo
    }

    pub fn reset(&mut self) {
        self.position = 0.0;
        self.origin = 0.0;
    }

    /// Moves back to the start of the step it is in.
    ///
    /// What Pause does. Keeping the exact position and *also* restarting the step on resume would
    /// leave the sequencer out of phase with the beat timeline it reports; snapping means resume
    /// has no phase to reconcile, and the small backward step happens while nothing is sounding.
    pub fn snap_to_step(&mut self) {
        self.position = self.position.floor();
    }

    /// Advances by `frames`, collecting every boundary crossed, in order.
    ///
    /// `out` is cleared first and never grows past [`MAX_BOUNDARIES_PER_CHUNK`], so this allocates
    /// nothing provided the caller reuses the buffer — which the audio thread does.
    pub fn advance(&mut self, frames: u32, out: &mut Vec<Boundary>) {
        out.clear();
        if frames == 0 {
            return;
        }

        let per_frame = self.per_frame();
        let start = self.position;
        let end = start + per_frame * f64::from(frames);

        // Boundaries sit on every half-sixteenth: whole numbers start a step, halves close a gate.
        // Walking them in one sequence is what keeps them ordered by construction.
        //
        // The span is half-open, `[start, end)`. A boundary landing exactly on `start` fires here;
        // one landing exactly on `end` waits for the next chunk, which begins at that position.
        // That is what makes each boundary fire exactly once across consecutive chunks — and it is
        // what sounds step 1 the instant a run begins, at position zero.
        let mut next = (start * 2.0).ceil() / 2.0;

        while next < end && out.len() < MAX_BOUNDARIES_PER_CHUNK {
            let frame = (((next - start) / per_frame).floor() as i64)
                .clamp(0, i64::from(frames - 1)) as u32;

            let is_step = (next - next.round()).abs() < f64::EPSILON * 8.0;

            // **A queued window applies at the current bar's end** — a step start whose in-window
            // offset is a bar multiple, the wrap included. Applied before the step is mapped, so
            // this same boundary is the new window's first step; the switch marker goes out first
            // so the runtime can release what the bar being left still holds.
            if is_step && let Some((w_start, w_len, entry)) = self.pending {
                let offset =
                    ((next - self.origin).floor() as i64).rem_euclid(self.length as i64) as usize;
                if offset.is_multiple_of(self.bar_length) {
                    let entry_step = match entry {
                        Entry::WindowStart => w_start,
                        // The step after the bar being left, wrapped into the new window. The new
                        // window is the whole sequence, so the old bar's end is inside it.
                        Entry::Continue => {
                            (self.window_start + offset + self.bar_length) % w_len.max(1)
                        }
                    };
                    self.window_start = w_start;
                    self.length = w_len;
                    // Rebase so `next` maps exactly onto the entry step; `position` runs on.
                    self.origin = next - (entry_step - w_start) as f64;
                    self.pending = None;
                    out.push(Boundary::WindowSwitch { frame });
                }
            }

            // `floor` for both: a step start sits at *n* and its gate close at *n* + 0.5, so the
            // whole part is the step either way and one expression covers them.
            let step = self.step_at(next);
            out.push(if is_step {
                Boundary::StepStart { frame, step }
            } else {
                Boundary::GateClose { frame, step }
            });

            next += 0.5;
        }

        self.position = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f64 = 48_000.0;

    fn boundaries(clock: &mut Clock, frames: u32) -> Vec<Boundary> {
        let mut out = Vec::new();
        clock.advance(frames, &mut out);
        out
    }

    #[test]
    fn a_sixteenth_at_120_bpm_is_an_eighth_of_a_second() {
        let clock = Clock::new(120.0, SR);
        // 120 BPM: a beat is 0.5 s, a 16th is 0.125 s, which is 6000 frames at 48 kHz.
        assert!((1.0 / clock.per_frame() - 6000.0).abs() < 1e-6);
    }

    #[test]
    fn steps_land_where_the_tempo_says_they_should() {
        let mut clock = Clock::new(120.0, SR);
        // One step exactly: the boundary at its far end belongs to the next block, not this one.
        let found = boundaries(&mut clock, 6000);
        let starts: Vec<_> = found
            .iter()
            .filter(|b| matches!(b, Boundary::StepStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1, "{found:?}");
        assert_eq!(found[0].frame(), 0, "the run starts on a step");
    }

    #[test]
    fn the_gate_closes_halfway_through_the_step() {
        let mut clock = Clock::new(120.0, SR);
        let found = boundaries(&mut clock, 6000);
        let gate = found
            .iter()
            .find(|b| matches!(b, Boundary::GateClose { .. }))
            .expect("a gate close");
        // Half of 6000 frames.
        assert!(
            (gate.frame() as i64 - 3000).abs() <= 1,
            "gate at {}",
            gate.frame()
        );
    }

    #[test]
    fn boundaries_come_back_in_order() {
        let mut clock = Clock::new(200.0, SR);
        let found = boundaries(&mut clock, 8192);
        assert!(found.len() > 2, "expected several: {found:?}");
        for pair in found.windows(2) {
            assert!(pair[0].frame() <= pair[1].frame(), "out of order: {pair:?}");
        }
    }

    #[test]
    fn every_step_gets_exactly_one_gate_before_the_next_one_starts() {
        // The property the one-clock design exists to guarantee: gates and steps strictly
        // alternate, so a note is always released before the next is sounded. Frame offsets are
        // per chunk and cannot be compared across chunks, so the check is on the *sequence*.
        let mut clock = Clock::new(137.0, SR);
        let mut sequence = Vec::new();
        for _ in 0..400 {
            sequence.extend(boundaries(&mut clock, 512));
        }
        assert!(
            sequence.len() > 20,
            "expected a long run: {}",
            sequence.len()
        );

        let mut expecting_step = true;
        for boundary in &sequence {
            match boundary {
                Boundary::StepStart { .. } => {
                    assert!(expecting_step, "two step starts with no gate between them");
                    expecting_step = false;
                }
                Boundary::GateClose { .. } => {
                    assert!(!expecting_step, "two gates with no step between them");
                    expecting_step = true;
                }
                Boundary::WindowSwitch { .. } => {}
            }
        }
    }

    #[test]
    fn boundaries_within_one_chunk_are_ordered_by_frame() {
        let mut clock = Clock::new(300.0, SR);
        let found = boundaries(&mut clock, 8192);
        for pair in found.windows(2) {
            assert!(pair[0].frame() <= pair[1].frame(), "out of order: {pair:?}");
        }
    }

    #[test]
    fn a_chunk_with_no_boundary_is_normal() {
        let mut clock = Clock::new(60.0, SR);
        // 60 BPM: a 16th is 12000 frames, so a 512-frame chunk usually crosses nothing.
        boundaries(&mut clock, 512); // consumes the boundary at zero
        let found = boundaries(&mut clock, 512);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_chunk_may_hold_several_boundaries() {
        let mut clock = Clock::new(300.0, SR);
        // 300 BPM: a 16th is 2400 frames, so 8192 frames spans several steps and their gates.
        let found = boundaries(&mut clock, 8192);
        assert!(found.len() >= 6, "expected several, got {found:?}");
    }

    #[test]
    fn the_step_index_wraps_at_sixteen() {
        let mut clock = Clock::new(120.0, SR);
        let mut seen = Vec::new();
        for _ in 0..17 {
            for boundary in boundaries(&mut clock, 6000) {
                if let Boundary::StepStart { step, .. } = boundary {
                    seen.push(step);
                }
            }
        }
        assert_eq!(seen[0], 0);
        assert_eq!(seen[15], 15);
        assert_eq!(seen[16], 0, "the seventeenth step is the first again");
    }

    #[test]
    fn timing_does_not_drift_at_a_tempo_whose_step_is_not_a_whole_number_of_frames() {
        // 137 BPM at 48 kHz is 5255.474... frames per step. An integer counter would drag.
        let mut clock = Clock::new(137.0, SR);
        let mut steps = 0;
        let blocks = 2_000;
        for _ in 0..blocks {
            for boundary in boundaries(&mut clock, 512) {
                if matches!(boundary, Boundary::StepStart { .. }) {
                    steps += 1;
                }
            }
        }
        let frames = f64::from(blocks) * 512.0;
        let expected = frames * clock.per_frame();
        assert!(
            (steps as f64 - expected).abs() < 1.5,
            "counted {steps} steps, expected about {expected}"
        );
    }

    #[test]
    fn a_tempo_change_cannot_strand_a_gate_after_its_next_step() {
        // The round-3 finding, as a test. Accelerate hard mid-step and check ordering holds.
        for (from, to) in [(60.0, 300.0), (300.0, 60.0)] {
            let mut clock = Clock::new(from, SR);
            boundaries(&mut clock, 100); // land partway into step 0
            clock.set_tempo(to);

            let mut phase_of_gate = None;
            for _ in 0..400 {
                for boundary in boundaries(&mut clock, 256) {
                    match boundary {
                        Boundary::GateClose { .. } => phase_of_gate = Some(true),
                        Boundary::StepStart { .. } => {
                            assert!(
                                phase_of_gate.is_some(),
                                "{from}->{to}: a step started before its gate ever closed"
                            );
                            phase_of_gate = None;
                        }
                        Boundary::WindowSwitch { .. } => {}
                    }
                }
            }
        }
    }

    #[test]
    fn beats_are_the_same_number_the_sequencer_runs_on() {
        let mut clock = Clock::new(120.0, SR);
        boundaries(&mut clock, 6000 * 4); // four sixteenths = one beat
        assert!((clock.beats() - 1.0).abs() < 1e-9, "{}", clock.beats());
        assert!((clock.seconds() - 0.5).abs() < 1e-9, "{}", clock.seconds());
    }

    #[test]
    fn pause_snaps_to_the_step_it_was_in() {
        let mut clock = Clock::new(120.0, SR);
        boundaries(&mut clock, 9000); // one and a half steps
        assert_eq!(clock.step(), 1);
        assert!(clock.phase() > 0.4);

        clock.snap_to_step();
        assert_eq!(clock.step(), 1, "it stays on the step it was in");
        assert!(clock.phase().abs() < 1e-9, "and starts it from the top");
    }

    #[test]
    fn the_tempo_range_is_clamped_rather_than_trusted() {
        let mut clock = Clock::new(1e9, SR);
        assert_eq!(clock.tempo(), MAX_TEMPO);
        clock.set_tempo(0.0);
        assert_eq!(clock.tempo(), MIN_TEMPO);
    }

    #[test]
    fn a_nonsense_sample_rate_cannot_hang_the_audio_thread() {
        // The guard, not a limit reached in use.
        let mut clock = Clock::new(MAX_TEMPO, 1.0);
        let found = boundaries(&mut clock, 4096);
        assert!(found.len() <= MAX_BOUNDARIES_PER_CHUNK);
    }
}
