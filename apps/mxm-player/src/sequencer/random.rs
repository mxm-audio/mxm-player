//! A random monophonic bar in C Dorian: Euclidean rhythms decide where its notes start, and each
//! step's weight in the bar decides which notes they are.
//!
//! **The owner's design, tuned by ear on 2026-09-17.** The generator it replaced rested about one
//! step in four and drew every pitch uniformly. Measured, 92% of its bars were as unstructured as
//! random notes of the same count (coin flips: 97%). That gives a filter or an envelope nothing
//! steady to be heard against, and a bar nobody would keep. What replaced it came out of a Monte
//! Carlo study and two listening studies:
//!
//! - **Rhythm: three Euclidean rhythms, mixed two steps at a time.** Each source draws its own
//!   density and rotation, and every two-step block of the bar is copied from one of them. One
//!   Euclidean rhythm on its own is too predictable. Picking a source *per step* is already close
//!   to random with two of them, and blocks keep each source's local evenness. A structure score
//!   preferred longer blocks and a shared density; the owner's ear chose this over it. The score
//!   measured evenness, and evenness is not what makes a line musical. Toussaint, *The Euclidean
//!   Algorithm Generates Traditional Musical Rhythms* (2005).
//! - **Pitch: strong steps lean on the stable tones, and the bar drifts open toward its end.**
//!   Steps are ranked by metric weight, scaled down by how late in the bar they fall. A step's
//!   rank sets how hard it leans toward the tones a minor key hears as stable: the
//!   Krumhansl–Kessler probe-tone profile (1982). The first step is mostly C, then E♭, then G; the
//!   last step is uniform. Leaning the weakest steps toward the *unstable* tones was auditioned and
//!   not chosen.
//!
//! **The constants are listening decisions, not derivations.** Change them by listening, and say so
//! here.
//!
//! **Ties and slides ride on top, and never move a note.** A tie only ever replaces a rest, and a
//! slide only turns a note that already starts into a legato one. So the rhythm that was tuned is
//! still where the pitches change, whatever articulation lands on it.
//!
//! Seeded and deterministic, so a test can assert on what it produces. A tiny xorshift rather than
//! a dependency: this needs unpredictability of the "not the same sixteen notes every time" kind,
//! not of the cryptographic kind.

use super::pattern::{MAX_STEPS_PER_BAR, Pattern, STEPS_PER_BEAT};

/// C Dorian: the natural minor with a raised sixth — C D E♭ F G A B♭.
///
/// Semitones from C. Dorian is the mode most associated with the instruments this collection is
/// modelled on, and it stays usable over a drone, which is what makes it a good test scale.
pub const C_DORIAN: [u8; 7] = [0, 2, 3, 5, 7, 9, 10];

/// How stable each tone of [`C_DORIAN`] sounds in a minor key, in the same order.
///
/// The Krumhansl–Kessler minor-key probe-tone ratings (1982) at those pitch classes. What the
/// weighting uses is the order and the spacing: C, E♭, G, then F and D nearly level, B♭, and the
/// raised sixth last — the tone that gives Dorian its colour is the least settled one in it.
const STABILITY: [f64; 7] = [6.33, 3.52, 5.38, 3.53, 4.75, 2.69, 3.34];

/// The lowest note the generator writes, and how many octaves it spans.
///
/// One octave from C3, the owner's choice in the pitch study. Narrow on purpose: a line that
/// wanders across the keyboard tells you less about a filter than one that stays where you can hear
/// it.
pub const ROOT: u8 = 48;
pub const OCTAVES: u8 = 1;

/// How many Euclidean rhythms a bar is mixed from.
const RHYTHM_SOURCES: usize = 3;

/// How many steps in a row are copied from one source.
const BLOCK_STEPS: usize = 2;

/// Each source's density, as pulses per sixteen steps, drawn uniformly between the two.
///
/// Scaled to a shorter bar by [`pulse_range`]. The range keeps every source clear of both a
/// near-empty bar and a solid one.
const MIN_PULSES_IN_16: usize = 3;
const MAX_PULSES_IN_16: usize = 13;

/// How hard the strongest step leans on the stable tones. At 1.5 the first step is C about three
/// times in four.
const FOCUS: f64 = 1.5;

/// How far a step's position lowers its metric weight: `weight × (1 − ramp × position)`.
///
/// At 0 the ranking is the plain metric grid. At 1 the late steps sink below the early in-between
/// ones, and b5 rises above b9. That is what makes the bar settle at its start and drift open
/// toward its end.
const TENSION_RAMP: f64 = 1.0;

/// Roughly how often a rest after a sounding note becomes a tie instead, out of 16.
///
/// Ties are here because a feature nobody can hear without hand-building a pattern is one nobody
/// checks. A tie only replaces a rest, so it lengthens a note and never adds or moves one.
const TIE_IN_16: u32 = 3;

/// Roughly how often a note that starts while another still sounds **slides** into it instead, out
/// of 16.
///
/// The owner's decision: the generator writes slides so the feature is discoverable without
/// reading anything. Rarer than ties, because a slide is an event — the acid line's signature move,
/// not its texture — and only ever written where a note is already sounding, which is the same
/// invariant the editing funnels guard.
const SLIDE_IN_16: u32 = 2;

/// A small xorshift. Deterministic for a seed, which is what makes the output testable.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Zero is a fixed point of xorshift, so it must never be the state.
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value in `0..bound`.
    fn below(&mut self, bound: u32) -> u32 {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() >> 32) as u32 % bound
    }

    /// A value in `[0, 1)`, from the top 53 bits so every value is exact.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Generates **one bar** of a monophonic pattern in C Dorian, `steps` long.
///
/// A bar rather than a whole pattern, because the button that presses this fills the bar being
/// shown and leaves the rest of the sequence alone. `steps` is the caller's `steps_per_bar` —
/// generating sixteen and writing them into a bar of twelve would either overflow into the next
/// bar or silently re-bar the music.
pub fn sequence(seed: u64, steps: usize) -> Pattern {
    let mut rng = Rng::new(seed);
    let mut pattern = Pattern::sized(1, steps);
    let steps = pattern.len();

    let starts = note_starts(&mut rng, steps);
    let leans = leans(steps);

    // Whether a note is still sounding as this step begins. **Not the same as "the previous step
    // has notes"**: a tie leaves its own step empty while the note carries on, so asking about the
    // notes would let a tie follow a note but never another tie, and the generator could never
    // produce a run longer than two steps.
    let mut sounding = false;

    for (step, lean) in leans.into_iter().enumerate().take(steps) {
        if starts & (1 << step) == 0 {
            // **A tie only where it can be heard.** One on step 1 has nothing to continue, and one
            // after a rest continues a silence. Both are legal — a half-built pattern looks exactly
            // like that — and neither makes a sound, which would make ties look broken in the one
            // place they are demonstrated. An *empty* tie, so it lengthens the note rather than
            // starting a run of its own.
            if sounding && rng.below(16) < TIE_IN_16 {
                pattern.set_tied(step, true);
                continue;
            }
            sounding = false;
            continue;
        }

        let note = note(&mut rng, lean);
        // **A slide: the gate stays open, the pitch moves.** Drawn only while something sounds — a
        // slide from silence is the state the editing funnels refuse, for the same reason.
        if sounding && rng.below(16) < SLIDE_IN_16 {
            pattern.set_tied(step, true);
        }
        pattern.toggle(step, note);
        sounding = true;
    }

    pattern
}

/// Whether a note belongs to C Dorian, in any octave.
pub fn is_in_c_dorian(note: u8) -> bool {
    C_DORIAN.contains(&(note % 12))
}

/// The steps a bar's notes start on, as a bit mask — never empty.
///
/// A bar with nothing in it is a press that did nothing. The mix lands there about once in twenty
/// thousand rolls, so it rolls again. The fallback, a single note on the first step, is there so
/// the loop is bounded and not because it is expected to run.
fn note_starts(rng: &mut Rng, steps: usize) -> u32 {
    for _ in 0..64 {
        let (starts, _) = mixed_rhythm(rng, steps);
        if starts != 0 {
            return starts;
        }
    }
    1
}

/// [`RHYTHM_SOURCES`] Euclidean rhythms, each with its own density and rotation, and a bar built by
/// copying every [`BLOCK_STEPS`]-step block from one of them, picked uniformly.
///
/// Returns the mixed bar and the sources, so a test can check every block against them.
fn mixed_rhythm(rng: &mut Rng, steps: usize) -> (u32, [u32; RHYTHM_SOURCES]) {
    let (fewest, most) = pulse_range(steps);
    let sources: [u32; RHYTHM_SOURCES] = std::array::from_fn(|_| {
        let pulses = fewest + rng.below((most - fewest + 1) as u32) as usize;
        let rotation = rng.below(steps as u32) as usize;
        rotate(euclidean(pulses, steps), rotation, steps)
    });

    let mut mixed = 0;
    for start in (0..steps).step_by(BLOCK_STEPS) {
        let end = (start + BLOCK_STEPS).min(steps);
        let block = ((1u32 << end) - 1) & !((1u32 << start) - 1);
        let source = sources[rng.below(RHYTHM_SOURCES as u32) as usize];
        mixed |= source & block;
    }
    (mixed, sources)
}

/// The density range scaled to a bar of `steps`: 3–13 pulses of 16, and never below one.
fn pulse_range(steps: usize) -> (usize, usize) {
    let fewest = (MIN_PULSES_IN_16 * steps).div_ceil(16).max(1);
    let most = (MAX_PULSES_IN_16 * steps / 16).max(fewest);
    (fewest, most)
}

/// `pulses` spread as evenly as `steps` allows, as a bit mask.
///
/// The modular form of Bjorklund's algorithm: step `i` sounds when `i × pulses mod steps < pulses`.
/// It yields a rotation of the same necklace, and the caller rotates at random anyway.
fn euclidean(pulses: usize, steps: usize) -> u32 {
    (0..steps)
        .filter(|i| (i * pulses) % steps < pulses)
        .fold(0, |mask, i| mask | (1 << i))
}

/// `mask` moved `by` steps later around a bar of `steps`.
fn rotate(mask: u32, by: usize, steps: usize) -> u32 {
    if by == 0 {
        return mask;
    }
    let full = (1u32 << steps) - 1;
    ((mask << by) | (mask >> (steps - by))) & full
}

/// A step's weight in the metre, before the tension ramp.
///
/// The downbeat 1; the half-bar 0.8, where the bar has an even number of beats; other beats 0.6;
/// eighths 0.4; sixteenths 0.2. For sixteen steps: b1, b9, b5 and b13, the other odd-numbered
/// steps, then the even-numbered ones. A twelve-step bar has no half-bar, so its three beats weigh
/// the same after the first.
fn metric_weight(step: usize, steps: usize) -> f64 {
    let beat = STEPS_PER_BEAT as usize;
    if step == 0 {
        1.0
    } else if step.is_multiple_of(beat) {
        if steps.is_multiple_of(2 * beat) && step == steps / 2 {
            0.8
        } else {
            0.6
        }
    } else if step.is_multiple_of(2) {
        0.4
    } else {
        0.2
    }
}

/// Each step's lean toward the stable tones: 1 at the strongest step, falling evenly by rank to 0
/// at the weakest.
///
/// Steps rank by metric weight × `(1 − TENSION_RAMP × position)`, where position runs from 0 on the
/// first step to 1 on the last. A tie goes to the earlier step. At sixteen steps the ranking opens
/// b1, b5, b9, b3 and closes b14, b15, b16.
fn leans(steps: usize) -> [f64; MAX_STEPS_PER_BAR] {
    let last = steps.saturating_sub(1).max(1) as f64;
    let weight =
        |step: usize| metric_weight(step, steps) * (1.0 - TENSION_RAMP * step as f64 / last);

    let mut ranked: [usize; MAX_STEPS_PER_BAR] = std::array::from_fn(|step| step);
    let ranked = &mut ranked[..steps];
    ranked.sort_by(|&a, &b| weight(b).total_cmp(&weight(a)).then(a.cmp(&b)));

    let mut leans = [0.0; MAX_STEPS_PER_BAR];
    for (rank, &step) in ranked.iter().enumerate() {
        leans[step] = 1.0 - rank as f64 / last;
    }
    leans
}

/// The chance of each tone of [`C_DORIAN`] on a step with this lean.
///
/// `exp(FOCUS × lean × (stability − mean stability))`, normalised: a softmax over the probe-tone
/// profile, sharpened by the lean. A lean of 0 is uniform.
fn tone_chances(lean: f64) -> [f64; 7] {
    let mean = STABILITY.iter().sum::<f64>() / STABILITY.len() as f64;
    let weights = STABILITY.map(|stability| (FOCUS * lean * (stability - mean)).exp());
    let total: f64 = weights.iter().sum();
    weights.map(|weight| weight / total)
}

/// A note for a step with this lean: a tone drawn from [`tone_chances`], in an octave drawn
/// uniformly.
fn note(rng: &mut Rng, lean: f64) -> u8 {
    let draw = rng.unit();
    let chances = tone_chances(lean);
    let mut below = 0.0;
    let tone = chances
        .iter()
        .position(|chance| {
            below += chance;
            draw < below
        })
        .unwrap_or(C_DORIAN.len() - 1);
    let octave = rng.below(u32::from(OCTAVES)) as u8;
    ROOT + octave * 12 + C_DORIAN[tone]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequencer::pattern::STEPS;

    #[test]
    fn every_note_it_writes_is_in_c_dorian() {
        // A property of the whole output rather than a sample of it, checked across many seeds.
        for seed in 1..500u64 {
            let pattern = sequence(seed, STEPS);
            for (step, notes) in pattern.steps() {
                notes.for_each(|note| {
                    assert!(
                        is_in_c_dorian(note),
                        "seed {seed} step {step} wrote {note}, which is outside C Dorian"
                    );
                });
            }
        }
    }

    #[test]
    fn it_is_monophonic() {
        for seed in 1..500u64 {
            for (step, notes) in sequence(seed, STEPS).steps() {
                assert!(notes.count() <= 1, "seed {seed} step {step} wrote a chord");
            }
        }
    }

    #[test]
    fn it_leaves_rests_so_you_can_hear_a_release_tail() {
        let mut with_rests = 0;
        for seed in 1..200u64 {
            let pattern = sequence(seed, STEPS);
            let rests = (0..STEPS)
                .filter(|&step| pattern.step(step).is_empty() && !pattern.tied(step))
                .count();
            if rests > 0 {
                with_rests += 1;
            }
        }
        assert!(
            with_rests > 190,
            "only {with_rests} of 199 seeds rested at all"
        );
    }

    #[test]
    fn a_press_never_writes_an_empty_bar() {
        // The mix can pick an empty block from every source; about one roll in twenty thousand
        // does. A press that writes nothing looks like a button that is broken.
        for steps in 1..=MAX_STEPS_PER_BAR {
            for seed in 0..5_000u64 {
                assert!(
                    !sequence(seed, steps).is_empty(),
                    "seed {seed} wrote an empty bar of {steps}"
                );
            }
        }
    }

    #[test]
    fn a_sixteen_step_bar_holds_about_eight_notes_on_average() {
        // Each source averages eight pulses in sixteen, and neither ties nor slides add or remove a
        // note. A mean far from eight means the mix, or the articulation on top of it, is wrong.
        let seeds = 4_000u64;
        let notes: usize = (0..seeds)
            .map(|seed| {
                let pattern = sequence(seed, STEPS);
                (0..STEPS)
                    .filter(|&step| !pattern.step(step).is_empty())
                    .count()
            })
            .sum();
        let mean = notes as f64 / seeds as f64;
        assert!(
            (7.5..8.5).contains(&mean),
            "{mean} notes per bar on average"
        );
    }

    #[test]
    fn the_same_seed_always_gives_the_same_sequence() {
        // What lets a test assert on a generated pattern at all.
        assert_eq!(sequence(42, STEPS), sequence(42, STEPS));
    }

    #[test]
    fn different_seeds_give_different_sequences() {
        let first = sequence(1, STEPS);
        let differ = (2..60u64).filter(|s| sequence(*s, STEPS) != first).count();
        assert!(differ > 50, "only {differ} of 58 seeds differed");
    }

    #[test]
    fn it_stays_in_the_range_it_promises() {
        for seed in 1..500u64 {
            for (_, notes) in sequence(seed, STEPS).steps() {
                notes.for_each(|note| {
                    assert!(
                        (ROOT..ROOT + OCTAVES * 12).contains(&note),
                        "seed {seed} wrote {note}, outside the one-octave range"
                    );
                });
            }
        }
    }

    #[test]
    fn euclidean_rhythms_spread_their_pulses_as_evenly_as_the_bar_allows() {
        // Maximal evenness: every gap between consecutive pulses, around the bar, is one of two
        // neighbouring lengths.
        for steps in 1..=MAX_STEPS_PER_BAR {
            for pulses in 1..=steps {
                let mask = euclidean(pulses, steps);
                let at: Vec<usize> = (0..steps).filter(|i| mask & (1 << i) != 0).collect();
                assert_eq!(at.len(), pulses, "E({pulses},{steps}) has the wrong count");
                let gaps: Vec<usize> = (0..at.len())
                    .map(|k| (at[(k + 1) % at.len()] + steps - at[k] - 1) % steps + 1)
                    .collect();
                let (shortest, longest) = (gaps.iter().min(), gaps.iter().max());
                assert!(
                    longest.unwrap() - shortest.unwrap() <= 1,
                    "E({pulses},{steps}) is uneven: gaps {gaps:?}"
                );
            }
        }
    }

    #[test]
    fn a_rotation_keeps_every_pulse() {
        for steps in 1..=MAX_STEPS_PER_BAR {
            let mask = euclidean(steps.div_ceil(3), steps);
            for by in 0..steps {
                let rotated = rotate(mask, by, steps);
                assert_eq!(
                    rotated.count_ones(),
                    mask.count_ones(),
                    "{steps} steps by {by}"
                );
                assert_eq!(
                    rotated >> steps,
                    0,
                    "{steps} steps by {by} spilled past the bar"
                );
            }
        }
    }

    #[test]
    fn every_two_step_block_comes_from_one_of_the_three_rhythms() {
        for steps in 1..=MAX_STEPS_PER_BAR {
            let mut rng = Rng::new(0xB10C);
            for _ in 0..500 {
                let (mixed, sources) = mixed_rhythm(&mut rng, steps);
                for start in (0..steps).step_by(BLOCK_STEPS) {
                    let end = (start + BLOCK_STEPS).min(steps);
                    let block = ((1u32 << end) - 1) & !((1u32 << start) - 1);
                    assert!(
                        sources.iter().any(|source| source & block == mixed & block),
                        "block {start}..{end} of {steps} matches no source"
                    );
                }
            }
        }
    }

    #[test]
    fn each_source_holds_three_to_thirteen_pulses_in_sixteen() {
        assert_eq!(pulse_range(16), (3, 13));
        assert_eq!(pulse_range(12), (3, 9), "scaled, not clipped");
        assert_eq!(pulse_range(1), (1, 1), "a one-step bar still sounds");
    }

    #[test]
    fn the_steps_rank_as_the_owner_tuned_them() {
        // The ranking the pitch study was listened to with, tension ramp 1: the first half-bar
        // settles, and the last steps are the most open.
        let leans = leans(STEPS);
        let mut ranked: Vec<usize> = (0..STEPS).collect();
        ranked.sort_by(|&a, &b| leans[b].total_cmp(&leans[a]));
        let b: Vec<usize> = ranked.iter().map(|step| step + 1).collect();
        assert_eq!(b, [1, 5, 9, 3, 7, 2, 4, 6, 11, 13, 8, 10, 12, 14, 15, 16]);
        assert_eq!(leans[0], 1.0);
        assert_eq!(leans[STEPS - 1], 0.0);
    }

    #[test]
    fn the_first_step_leans_on_c_then_e_flat_then_g() {
        let chances = tone_chances(leans(STEPS)[0]);
        let by = |pc: u8| chances[C_DORIAN.iter().position(|&t| t == pc).unwrap()];
        let (c, d, e_flat, f, g, a, b_flat) = (by(0), by(2), by(3), by(5), by(7), by(9), by(10));
        assert!(c > e_flat && e_flat > g, "C {c}, E♭ {e_flat}, G {g}");
        assert!(
            g > f.max(d) && f.min(d) > b_flat && b_flat > a,
            "the unstable tones out of order"
        );
        assert!(
            (0.70..0.76).contains(&c),
            "C is {c} on the first step, tuned to about 0.73"
        );
    }

    #[test]
    fn the_last_step_is_uniform() {
        // The owner chose "flat" over leaning the weakest steps toward the unstable tones.
        for chance in tone_chances(leans(STEPS)[STEPS - 1]) {
            assert!((chance - 1.0 / 7.0).abs() < 1e-12, "{chance}");
        }
    }

    #[test]
    fn generated_bars_open_on_c_and_end_anywhere() {
        // The model end to end: the first step's notes are mostly C, the last step's are not.
        let (mut first, mut first_c, mut last, mut last_c) = (0, 0, 0, 0);
        for seed in 0..6_000u64 {
            let pattern = sequence(seed, STEPS);
            pattern.step(0).for_each(|note| {
                first += 1;
                first_c += usize::from(note % 12 == 0);
            });
            pattern.step(STEPS - 1).for_each(|note| {
                last += 1;
                last_c += usize::from(note % 12 == 0);
            });
        }
        let first_share = first_c as f64 / first as f64;
        let last_share = last_c as f64 / last as f64;
        assert!(
            (0.68..0.78).contains(&first_share),
            "step 1 is C {first_share}"
        );
        assert!(
            last_share < 0.2,
            "step 16 is C {last_share}, tuned to 1 in 7"
        );
    }

    #[test]
    fn it_writes_ties_often_enough_that_the_feature_is_audible() {
        // The reason ties are generated at all: a feature you have to hand-build a pattern to hear
        // is one nobody checks. A tie only replaces a rest that follows a note, so with the
        // Euclidean mix about three bars in five carry one — measured 60% — and two presses almost
        // always show one.
        let with_ties = (1..200u64)
            .filter(|seed| {
                let pattern = sequence(*seed, STEPS);
                (0..STEPS).any(|step| pattern.tied(step))
            })
            .count();
        assert!(
            with_ties > 90,
            "only {with_ties} of 199 seeds tied anything, which is too rare to notice"
        );
    }

    #[test]
    fn it_never_writes_a_tie_that_cannot_be_heard() {
        // A tie continues whatever is sounding. On step 1 nothing is; after a rest nothing is.
        // Both are legal -- a half-built pattern looks like that -- but silent, and generating one
        // would make ties look broken in the one place they are demonstrated.
        //
        // Walks back through any chain of ties to the note that started it, which is also what
        // proves a tie is only ever written where a *run* is live rather than only after a note.
        // The walk stops at a note-carrying step — a slide is a head for the steps behind it.
        for seed in 1..500u64 {
            let pattern = sequence(seed, STEPS);
            assert!(!pattern.tied(0), "seed {seed} tied the first step");

            for step in 1..STEPS {
                if !pattern.tied(step) {
                    continue;
                }
                let mut back = step;
                while back > 0 && pattern.tied(back) && pattern.step(back).is_empty() {
                    back -= 1;
                }
                assert!(
                    !pattern.step(back).is_empty(),
                    "seed {seed} step {step} belongs to a run starting at {back}, which is a rest"
                );
            }
        }
    }

    #[test]
    fn it_writes_runs_longer_than_two_steps() {
        // The reason `sounding` is tracked rather than the previous step's notes being consulted:
        // a tie leaves its own step empty, so "the step before has notes" lets a tie follow a note
        // and never another tie, capping every run at two steps. The k + 1 arithmetic is most
        // likely to be wrong for larger k, so the generator has to be able to reach it.
        let longest = (1..300u64)
            .map(|seed| {
                let pattern = sequence(seed, STEPS);
                let mut best = 0;
                let mut run = 0;
                for step in 0..STEPS {
                    if pattern.tied(step) {
                        run += 1;
                        best = best.max(run);
                    } else {
                        run = 0;
                    }
                }
                best
            })
            .max()
            .unwrap_or(0);
        assert!(
            longest >= 2,
            "the longest run of ties across 299 seeds was {longest}; ties never chain"
        );
    }

    #[test]
    fn it_writes_slides_often_enough_that_the_feature_is_discoverable() {
        // The owner's decision: the generator is how slides surface without reading anything.
        // Occasional, not the texture — and a slide needs a note to start right where another
        // still sounds, which the Euclidean mix gives about one bar in three (measured 34%). Three
        // presses show one. Across many rolls they must actually appear, or the invariant check
        // below passes vacuously.
        let with_slides = (1..200u64)
            .filter(|seed| {
                let pattern = sequence(*seed, STEPS);
                (0..STEPS).any(|step| pattern.tied(step) && !pattern.step(step).is_empty())
            })
            .count();
        assert!(
            with_slides > 40,
            "only {with_slides} of 199 seeds slid at all, which is too rare to discover"
        );
    }

    #[test]
    fn every_slide_it_writes_has_a_note_to_slide_from() {
        // **A property of the generator, not of the format.** The editor allows a slide from
        // silence — a tie is a fact about its own step — but a generated bar is meant to be
        // playable the moment it lands, and a slide nobody can hear is not that. What the editor
        // permits and what the generator chooses are two questions, and this is the second.
        for seed in 1..500u64 {
            let pattern = sequence(seed, STEPS);
            for step in 0..STEPS {
                if !pattern.tied(step) || pattern.step(step).is_empty() {
                    continue;
                }
                assert!(
                    pattern.slide_head(step).is_some(),
                    "seed {seed} step {step} slides from nothing"
                );
            }
        }
    }

    #[test]
    fn a_zero_seed_still_generates_something() {
        // Zero is a fixed point of xorshift; if it reached the state the generator would stall.
        assert!(!sequence(0, STEPS).is_empty());
    }

    #[test]
    fn it_generates_exactly_one_bar_of_the_length_it_is_asked_for() {
        // It is written into a bar of the caller's shape. Sixteen steps into a bar of twelve
        // would run over into the next bar or re-bar the music, and both are silent corruption.
        for steps in 1..=MAX_STEPS_PER_BAR {
            let pattern = sequence(7, steps);
            assert_eq!(pattern.bars(), 1, "{steps} steps produced more than a bar");
            assert_eq!(pattern.len(), steps);
        }
    }
}
