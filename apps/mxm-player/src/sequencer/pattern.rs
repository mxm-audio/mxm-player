//! What the sequencer plays: a run of steps, each holding any set of notes.
//!
//! # Why a mask, and why `Copy`
//!
//! A step is a 128-bit mask — one bit per MIDI note — so the whole pattern is **a few hundred bytes
//! per step**, and its length is whatever somebody made it. That matters
//! because
//! the pattern is published to the audio thread by value: no allocation, and no `Arc` to drop in a
//! realtime callback. It is the discipline [`CcMask`](crate::control_map::CcMask) already set.
//!
//! Storage is polyphonic even though [`random`](super::random) generates one note per step. The
//! editing gesture is "toggle note keys on and off at that step" — plural — and refusing chords in
//! the data model would be a limit that buys nothing.

use serde::{Deserialize, Serialize};

/// How many steps a bar holds by default — sixteen sixteenths, one 4/4 bar.
///
/// A bar's own count is [`Pattern::steps_per_bar`]; twelve is a 3/4 bar.
pub const STEPS: usize = 16;

/// The most steps a bar may hold.
///
/// **The owner's number, not a derived one**, and it is the same as the default: a bar is at most
/// sixteen sixteenths, so 4/4 is the longest bar and everything shorter is an odd meter. Finer than
/// a sixteenth is a *step rate* — a different control, and deliberately not this one.
///
/// **The bar count has no maximum** and must not acquire one. Three were invented here before this
/// number was asked for, and the difference is that this one was specified.
pub const MAX_STEPS_PER_BAR: usize = 16;

// **There is deliberately no maximum here.** Three were tried and all three were the same mistake:
// sixteen steps because the command ring copied the pattern into every slot, then sixteen *bars* and
// thirty-two steps a bar for no reason at all — the second of those justified in a comment by what a
// mouse can reach, which is an interface fact sizing a data structure. `apps/mxm-player/AGENTS.md`
// already forbids that shape of reasoning twice over.
//
// A sequence is as long as somebody makes it. The storage is heap and the audio thread reads it
// through a pointer it never clones, so length costs memory and nothing else.

/// How many 16ths make up one beat.
pub const STEPS_PER_BEAT: f64 = 4.0;

/// One step: a bit per MIDI note.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Step(u128);

impl Step {
    pub const EMPTY: Step = Step(0);

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, note: u8) -> bool {
        note < 128 && self.0 & (1u128 << note) != 0
    }

    pub fn with(mut self, note: u8) -> Self {
        if note < 128 {
            self.0 |= 1u128 << note;
        }
        self
    }

    pub fn without(mut self, note: u8) -> Self {
        if note < 128 {
            self.0 &= !(1u128 << note);
        }
        self
    }

    /// Toggles `note`, which is the editing gesture the interface offers.
    pub fn toggled(self, note: u8) -> Self {
        if self.contains(note) {
            self.without(note)
        } else {
            self.with(note)
        }
    }

    pub fn count(self) -> u32 {
        self.0.count_ones()
    }

    /// The notes it holds, lowest first.
    ///
    /// Allocates, so it is for the GUI and for saving — **never** call it on the audio thread;
    /// use [`Step::for_each`] there.
    pub fn notes(self) -> Vec<u8> {
        (0u8..128).filter(|n| self.contains(*n)).collect()
    }

    /// Visits each note without allocating. This is the audio-thread path.
    pub fn for_each(self, mut f: impl FnMut(u8)) {
        let mut bits = self.0;
        while bits != 0 {
            let note = bits.trailing_zeros() as u8;
            f(note);
            bits &= bits - 1;
        }
    }
}

/// Steps and their ties.
///
/// # Why a `bool` and not a richer gate
///
/// A step is a **rest**, a **note**, a **hold** or a **slide** — four things it *is*, from two
/// facts it stores: whether it holds notes, and whether it ties backwards. A rest is an empty
/// untied step (`steps[i].is_empty()` already says so); a hold is an empty tie, continuing the
/// previous note; a slide is a tie carrying notes — the gate stays open, the pitch moves. Storing
/// any of those as its own value would be a second copy of a stored fact, free to disagree with
/// it. What cannot be derived is whether the step ties backwards, and that is this array.
///
/// It holds only because **pitch comes exclusively from a keyboard**: on-screen, computer or MIDI.
/// Nothing else writes a note, so nothing can produce a step that holds notes and is meant to be
/// silent. If *mute this step but keep its notes* is ever wanted it is a **second** boolean; folding
/// it back in here re-creates the contradiction this design removes.
///
/// The saving is not just the byte: with a richer gate, every path that wrote a note had to
/// set the gate to match or write a note that never sounded — nine of them. `tied` defaults to
/// `false` and every write path is correct without knowing it exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    steps: Vec<Step>,
    /// Whether each step **continues the note before it**. See [`Pattern::tied`].
    tied: Vec<bool>,
    /// How many bars the pattern plays.
    bars: usize,
    /// How many steps each bar holds.
    ///
    /// **A step stays a sixteenth; this says how many the bar holds.** Twelve is a 3/4 bar. The
    /// alternative reading — that this subdivides a bar of fixed duration, so 32 would mean 32nd
    /// notes — is a *step rate*, a separate control, and deliberately not this one.
    steps_per_bar: usize,
}

impl Default for Pattern {
    fn default() -> Self {
        Self::empty()
    }
}

impl Pattern {
    /// One bar of sixteen, empty.
    ///
    /// A function rather than a `const`, because the storage is heap now — which is what removed the
    /// last ceiling on how long a sequence may be.
    pub fn empty() -> Self {
        Self::sized(1, STEPS)
    }

    /// An empty pattern of the given shape.
    pub fn sized(bars: usize, steps_per_bar: usize) -> Self {
        let bars = bars.max(1);
        let steps_per_bar = steps_per_bar.clamp(1, MAX_STEPS_PER_BAR);
        Self {
            steps: vec![Step::EMPTY; bars * steps_per_bar],
            tied: vec![false; bars * steps_per_bar],
            bars,
            steps_per_bar,
        }
    }

    /// Grows or shrinks the backing storage to match the shape.
    ///
    /// **Cells outside the new size are dropped, and that is the resize rule** — a shortened
    /// sequence loses what fell off, with its notes and ties. Locks are not here and are the
    /// caller's to drop; a pattern cannot see them.
    fn resize(&mut self) {
        let len = self.bars * self.steps_per_bar;
        self.steps.resize(len, Step::EMPTY);
        self.tied.resize(len, false);
    }

    /// How many steps this pattern plays in total, in sixteenths. Never zero.
    pub fn len(&self) -> usize {
        self.bars * self.steps_per_bar
    }

    /// How many bars it plays.
    pub fn bars(&self) -> usize {
        self.bars
    }

    /// How many steps each bar holds.
    pub fn steps_per_bar(&self) -> usize {
        self.steps_per_bar
    }

    /// The absolute index of `step` within `bar`, whether or not the pattern is that long.
    pub fn index_of(&self, bar: usize, step: usize) -> usize {
        bar * self.steps_per_bar + step
    }

    /// Sets the bar count. **No maximum** — see the note at the top of this file.
    ///
    /// Shrinking drops the bars that fall outside, with their notes and ties. Locks live elsewhere
    /// and are the caller's to drop.
    pub fn set_bars(&mut self, bars: usize) {
        self.bars = bars.max(1);
        self.resize();
    }

    /// Sets how many steps a bar holds, clamped to `1..=MAX_STEPS_PER_BAR`.
    ///
    /// **A cell keeps its `(bar, step)` coordinate**, which is why this is not the same as changing
    /// the total length: growing a bar moves every later bar further along the step array. Doing
    /// that is the caller's job, because the locks would otherwise be left behind — a pattern
    /// cannot see them.
    pub fn set_steps_per_bar(&mut self, steps: usize) {
        self.steps_per_bar = steps.clamp(1, MAX_STEPS_PER_BAR);
        self.resize();
    }

    pub fn step(&self, index: usize) -> Step {
        self.steps[index % self.len()]
    }

    pub fn set_step(&mut self, index: usize, step: Step) {
        let index = index % self.len();
        self.steps[index] = step;
    }

    /// Adds a note without removing one that is already there.
    ///
    /// Loading needs this: two note-ons for the same pitch in one step must not cancel each other,
    /// they must collapse — and the collapse is reported rather than silently toggling the note
    /// back off.
    pub fn toggle_on(&mut self, index: usize, note: u8) {
        let index = index % self.len();
        self.steps[index] = self.steps[index].with(note);
    }

    pub fn toggle(&mut self, index: usize, note: u8) {
        let index = index % self.len();
        self.steps[index] = self.steps[index].toggled(note);
    }

    /// Whether step `index` **continues the note before it**.
    ///
    /// **A tie reaches backwards.** A tie on step *n* does not mean "step *n* holds on"; it means
    /// the note from step *n* − 1 is still sounding through step *n*. Defining it forwards was the
    /// first attempt and could not work: an ordinary note closes its gate halfway through its own
    /// step, long before the next step begins, so there is nothing left for the next step to hold.
    pub fn tied(&self, index: usize) -> bool {
        self.tied[index % self.len()]
    }

    pub fn set_tied(&mut self, index: usize, tied: bool) {
        let index = index % self.len();
        self.tied[index] = tied;
    }

    /// Flips a step's tie, which is the editing gesture the interface offers.
    pub fn toggle_tied(&mut self, index: usize) {
        let index = index % self.len();
        self.tied[index] = !self.tied[index];
    }

    /// The step that **begins the run** `index` belongs to.
    ///
    /// An *empty* tied step is not a note of its own; it is the continuation of one, and walking
    /// back to the step that started it is how the interface answers "which note is this?". A tie
    /// **carrying notes** is a slide — the gate stays open, the pitch moves — and it begins a run
    /// of its own, exactly as [`Pattern::run_ends_at`] already reads it from the other direction.
    /// The two walked the same runs asymmetrically for as long as the interface could not author a
    /// slide; the symmetry is load-bearing now that it can.
    ///
    /// **Stops at the first step rather than wrapping.** Live, a tie there continues across the loop
    /// point; for editing there is nothing further back to walk to, and a search that wrapped would
    /// not terminate on a pattern that is tied all the way round.
    pub fn run_start(&self, index: usize) -> usize {
        self.run_start_from(index, 0)
    }

    /// [`Pattern::run_start`], stopped at `first` instead of at step zero.
    ///
    /// `Bar` scope needs the floor: the selected bar is the sequence, so a run is resolved within
    /// it and never continues in from a bar that is not playing.
    pub fn run_start_from(&self, index: usize, first: usize) -> usize {
        let mut step = index % self.len();
        // A note-carrying step is a head — a slide's own step included — so the walk continues
        // only through *empty* ties. The emptiness test is the same one `run_ends_at` calls
        // load-bearing.
        while step > first && self.tied(step) && self.step(step).is_empty() {
            step -= 1;
        }
        step
    }

    /// The note-carrying step a slide (or hold) at `step` would continue, if one is reachable.
    ///
    /// The authoring guard's question: a tied step's gate holds *something* open, and that
    /// something must exist — a slide hanging off a rest is the state the tie invariant forbids.
    /// Walks backwards through empty ties, **wrapping once, bounded** — the same walk
    /// [`Pattern::untie_orphans`] repairs by, because live playback holds a note across the loop
    /// point and a tie on step 1 legitimately continues the last step's note. A full cycle of
    /// empty ties reaching no note counts as none. (`run_start`, the *editing* walk, still does
    /// not wrap: it answers "which run does this cell belong to", a question about the bar as
    /// drawn, where there is nothing further back than the first step.)
    pub fn slide_head(&self, step: usize) -> Option<usize> {
        let len = self.len();
        let step = step % len;
        let mut probe = (step + len - 1) % len;
        let mut walked = 0;
        loop {
            if walked >= len {
                return None; // came back around: a cycle of ties, no head anywhere
            }
            if !self.step(probe).is_empty() {
                return Some(probe); // a note: the head this slide would continue
            }
            if !self.tied(probe) {
                return None; // an empty, untied step: a rest, which heads nothing
            }
            probe = (probe + len - 1) % len;
            walked += 1;
        }
    }

    /// Whether a note starting on `index` is held **past its 50% gate**.
    ///
    /// Both of `Runtime`'s suppression clauses, and it **wraps** exactly as the runtime's lookahead
    /// does: a tie on step 1 holds step 16's note across the loop point, which is right for a
    /// pattern that repeats. Offline callers get the wrap here too and truncate at the bar in
    /// [`Pattern::run_ends_at`] instead — one suppression rule, one place the offline difference
    /// lives, rather than two rules to keep in agreement.
    ///
    /// **A slide gates like a note.** A tied step carrying notes starts a new note, so its own
    /// gate is the ordinary half step unless the step after it is tied too; only an *empty* tie —
    /// a hold — fills its whole step, because only a hold is a continuation. The `tied(index)`
    /// clause therefore asks whether the step is empty, and the file follows the interface's
    /// meaning here rather than keeping a legacy one — the owner's ruling.
    pub fn held_past_gate(&self, index: usize) -> bool {
        (self.tied(index) && self.step(index).is_empty()) || self.tied(index + 1)
    }

    /// Where a run starting at `from` ends, as a step index, **stopping at the bar**.
    ///
    /// A run ends at the first following step that either is not tied — which releases what is
    /// sounding at its own start — or is a tie carrying notes of its own, which starts a new run
    /// rather than extending this one. That word *empty* is load-bearing and is the whole of the
    /// difference between "a note plus k ties lasts k + 1 steps" and something shorter.
    ///
    /// **Does not wrap.** Live, the pattern repeats and a run can cross the loop point; a render
    /// and a one-bar MIDI file both play it once, so the run is truncated here. `Runtime` never
    /// calls this — it walks boundaries and wraps naturally.
    pub fn run_ends_at(&self, from: usize) -> usize {
        let mut step = from + 1;
        while step < self.len() {
            if !self.tied(step) || !self.step(step).is_empty() {
                return step;
            }
            step += 1;
        }
        self.len()
    }

    /// Whether the pattern would make **no sound at all**.
    ///
    /// Ties count: a pattern of nothing but ties has no note to continue, so it is as silent as a
    /// pattern of rests. Saving and the "is there anything here" checks both want that reading.
    pub fn is_empty(&self) -> bool {
        self.steps.iter().all(|s| s.is_empty())
    }

    /// Whether the pattern holds **no data**, ties included.
    ///
    /// Distinct from [`Pattern::is_empty`]: a half-built pattern of ties with no notes yet is
    /// silent but is not nothing, and discarding it on save would lose somebody's work in progress.
    pub fn is_blank(&self) -> bool {
        self.is_empty() && !self.tied.iter().any(|t| *t)
    }

    pub fn clear(&mut self) {
        *self = Self::empty();
    }

    pub fn steps(&self) -> impl Iterator<Item = (usize, Step)> + '_ {
        self.steps.iter().copied().enumerate()
    }
}

// --- naming -------------------------------------------------------------------------------------

const NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// A MIDI note as a name: 60 is `C3`.
///
/// Octave numbering matches the player's on-screen keyboard, so what a saved file says is what the
/// keyboard shows. There is no universal convention — C3 and C4 are both defended for 60 — so the
/// one that matters is agreeing with what is on screen.
pub fn note_name(note: u8) -> String {
    let octave = (note / 12) as i32 - 2;
    format!("{}{}", NAMES[(note % 12) as usize], octave)
}

/// Parses a name back to a MIDI note. Accepts flats as well as sharps.
pub fn parse_note(text: &str) -> Option<u8> {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }

    let letter = bytes[0].to_ascii_uppercase();
    let base = match letter {
        b'C' => 0i32,
        b'D' => 2,
        b'E' => 4,
        b'F' => 5,
        b'G' => 7,
        b'A' => 9,
        b'B' => 11,
        _ => return None,
    };

    let mut rest = &text[1..];
    let mut accidental = 0i32;
    while let Some(first) = rest.chars().next() {
        match first {
            '#' | '♯' => accidental += 1,
            'b' | '♭' => accidental -= 1,
            _ => break,
        }
        rest = &rest[first.len_utf8()..];
    }

    let octave: i32 = rest.parse().ok()?;
    let value = (octave + 2) * 12 + base + accidental;
    (0..128).contains(&value).then_some(value as u8)
}

// --- serialisation ------------------------------------------------------------------------------

/// A pattern as it appears in a saved sequence: sixteen lists of note names.
///
/// **Names, not a bitmask.** A saved sequence is meant to be opened, diffed in a pull request and
/// hand-edited; `["C3", "G3"]` supports that and `4398046511104` does not.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PatternData(pub Vec<Vec<String>>);

impl From<&Pattern> for PatternData {
    fn from(pattern: &Pattern) -> Self {
        // **What the pattern plays, not what it can hold.** Writing the whole capacity array would
        // put five hundred empty steps in every file, and reading them back through a setter that
        // wraps at the pattern's length would fold them over the music.
        PatternData(
            pattern.steps[..pattern.len()]
                .iter()
                .map(|step| step.notes().into_iter().map(note_name).collect())
                .collect(),
        )
    }
}

impl PatternData {
    /// Rebuilds a pattern, reporting every name it could not read.
    ///
    /// Unreadable names are collected rather than failing the whole file: one typo in step 12
    /// should not cost you the other fifteen steps, and the reason still has to be visible.
    /// Rebuilds a pattern of the given shape.
    ///
    /// **The shape is passed in rather than guessed**, because a 48-step list is ambiguous between
    /// three bars of sixteen and four of twelve, and a file that did not say would be read back as
    /// different music. `Sequence` carries both numbers for exactly this reason.
    pub fn to_pattern_shaped(&self, bars: usize, steps_per_bar: usize) -> (Pattern, Vec<String>) {
        let mut pattern = Pattern::empty();
        pattern.set_steps_per_bar(steps_per_bar);
        pattern.set_bars(bars);
        let mut problems = Vec::new();

        for (index, names) in self.0.iter().take(pattern.len()).enumerate() {
            let mut step = Step::EMPTY;
            for name in names {
                match parse_note(name) {
                    Some(note) => step = step.with(note),
                    None => problems.push(format!("step {}: `{name}` is not a note", index + 1)),
                }
            }
            pattern.set_step(index, step);
        }

        if self.0.len() > pattern.len() {
            problems.push(format!(
                "the pattern has {} steps; only the first {} were used",
                self.0.len(),
                pattern.len()
            ));
        }

        (pattern, problems)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_tie_that_reaches_no_note_is_kept_exactly_as_written() {
        // **Ties and notes are independent** — the owner's ruling. A tie no longer has to reach a
        // note to be storable: the runtime sounds nothing until a note arrives, so a tie over a
        // rest is a pattern with nothing to hold yet, not damage to repair. Every structural edit
        // used to run an invariant across the whole pattern for this; nothing does now, and this
        // is what says the ties survive it.
        let mut pattern = super::Pattern::sized(3, 4);
        pattern.toggle(0, 60); // a head in bar one
        for step in 1..12 {
            pattern.set_tied(step, true); // one long run
        }
        // Bar two cleared by hand, exactly as clear_bar does it: bar three now continues a rest.
        for step in 4..8 {
            pattern.set_step(step, super::Step::EMPTY);
            pattern.set_tied(step, false);
        }

        assert!(
            (8..12).all(|step| pattern.tied(step)),
            "bar three keeps its ties, and simply has nothing to hold until a note lands in it"
        );
        assert!(
            (1..4).all(|step| pattern.tied(step)),
            "and the run whose head survives is untouched"
        );
    }

    #[test]
    fn a_pattern_of_nothing_but_ties_is_silent_without_being_rewritten() {
        // It reads as silence — `is_blank` already says so — and stays as authored. The old
        // invariant untied the lot; sketching ties before their notes is now ordinary work.
        let mut pattern = super::Pattern::sized(1, 4);
        for step in 0..4 {
            pattern.set_tied(step, true);
        }
        assert!(pattern.is_empty(), "no note anywhere, so no sound");
        assert!(
            (0..4).all(|step| pattern.tied(step)),
            "and the sketch survives, ready for a note to be dropped into it"
        );
    }

    use super::*;

    #[test]
    fn a_pattern_costs_only_what_it_holds() {
        // The reason the audio thread can be handed one without an allocation or an Arc. The
        // assertion is about `Copy` and no heap, not about any particular figure — but the figure
        // is pinned so that a growth has to be noticed and argued for rather than absorbed.
        //
        // **A pattern is no longer a fixed array and no longer `Copy`**, which is what removed the
        // last ceiling on how long a sequence can be. Three ceilings were tried and every one was a
        // number somebody invented; the struct itself is now two vectors and two counts, and the
        // music costs what the music costs.
        //
        // The audio thread reads it through an `Arc` it never clones — that is the property that
        // makes this affordable, and `Runtime::apply` returning the outgoing pointer is what keeps
        // it true.
        let mut pattern = Pattern::empty();
        let one_bar = pattern.len();
        pattern.set_bars(64);
        assert_eq!(
            pattern.len(),
            one_bar * 64,
            "sixty-four bars is sixty-four bars"
        );
        pattern.set_bars(512);
        assert_eq!(
            pattern.len(),
            one_bar * 512,
            "and there is no ceiling to bump into"
        );

        pattern.toggle(one_bar * 500, 60);
        assert!(
            pattern.step(one_bar * 500).contains(60),
            "a note in the five-hundredth bar is in the five-hundredth bar"
        );
    }

    #[test]
    fn a_pattern_plays_its_own_length_rather_than_a_constant() {
        // Twelve steps is a 3/4 bar. The wrap has to follow the pattern's length, not `STEPS`, or a
        // shorter bar would read cells it does not have — the wrong bar, silently, rather than a
        // wrong note.
        let mut pattern = Pattern::empty();
        assert_eq!(
            pattern.len(),
            STEPS,
            "a pattern starts at the default length"
        );

        pattern.set_steps_per_bar(12);
        pattern.toggle(0, 60);
        assert_eq!(pattern.len(), 12);
        assert_eq!(
            pattern.step(12),
            pattern.step(0),
            "step 13 of a twelve-step bar is step 1 again"
        );

        // Clamped rather than panicking: a spinner drives this, and zero steps is not a pattern.
        pattern.set_steps_per_bar(0);
        assert_eq!(pattern.len(), 1);
        pattern.set_steps_per_bar(128);
        assert_eq!(
            pattern.len(),
            MAX_STEPS_PER_BAR,
            "a bar is at most sixteen sixteenths — the owner's number"
        );
    }

    #[test]
    fn shortening_a_pattern_drops_what_falls_outside_it() {
        // **The resize rule, and it changed when the storage did.** A fixed array kept the cells
        // past the end, so shortening and lengthening again restored them — an artefact of the
        // array rather than a decision, and one that contradicted what saving already did. Heap
        // storage drops them, which is the rule the plan states: what falls outside goes, with its
        // notes and its ties.
        //
        // Locks are not here and are the caller's to drop; a pattern cannot see them.
        let mut pattern = Pattern::empty();
        pattern.toggle(15, 64);
        pattern.set_steps_per_bar(8);
        assert_eq!(pattern.len(), 8);
        pattern.set_steps_per_bar(16);
        assert!(
            pattern.step(15).is_empty(),
            "a step that fell outside the sequence does not come back when it grows again"
        );
    }

    #[test]
    fn a_tie_is_not_a_second_copy_of_whether_a_step_has_notes() {
        // The whole reason the gate is a bool: all four combinations are meaningful, and none of
        // them can contradict the notes, because the notes are not stored twice.
        let mut pattern = Pattern::empty();
        assert!(pattern.step(0).is_empty() && !pattern.tied(0), "rest");

        pattern.toggle(0, 60);
        assert!(!pattern.step(0).is_empty() && !pattern.tied(0), "note");

        pattern.set_tied(1, true);
        assert!(
            pattern.step(1).is_empty() && pattern.tied(1),
            "an empty tie continues step 1"
        );

        pattern.toggle(1, 67);
        assert!(
            !pattern.step(1).is_empty() && pattern.tied(1),
            "a tie carrying notes"
        );
    }

    #[test]
    fn a_run_reports_the_step_that_started_it() {
        let mut pattern = Pattern::empty();
        pattern.toggle(4, 60);
        pattern.set_tied(5, true);
        pattern.set_tied(6, true);

        assert_eq!(pattern.run_start(4), 4, "a note begins its own run");
        assert_eq!(pattern.run_start(5), 4);
        assert_eq!(pattern.run_start(6), 4, "however many ties deep");
        assert_eq!(
            pattern.run_start(7),
            7,
            "the next untied step starts a new one"
        );
    }

    #[test]
    fn a_slide_is_the_head_of_its_own_run_from_both_directions() {
        // The §4 defect from the plan: `run_ends_at` stopped at a note-carrying tie and
        // `run_start` walked straight through it, so the two disagreed about where a run begins —
        // on exactly the sequences that could not be authored yet. Symmetric now, and this holds
        // the symmetry.
        let mut pattern = Pattern::empty();
        pattern.toggle(4, 60); // a head
        pattern.set_tied(5, true); // a hold
        pattern.set_tied(6, true);
        pattern.toggle(6, 67); // a slide: tied, carrying a note
        pattern.set_tied(7, true); // a hold continuing the slide

        assert_eq!(pattern.run_start(6), 6, "a slide begins its own run");
        assert_eq!(
            pattern.run_start(7),
            6,
            "a hold after a slide belongs to the slide"
        );
        assert_eq!(
            pattern.run_start(5),
            4,
            "a hold before it still belongs to the head"
        );
        assert_eq!(
            pattern.run_ends_at(4),
            6,
            "and the head's run ends where the slide's begins"
        );
    }

    #[test]
    fn a_slide_gates_like_a_note_and_a_hold_fills_its_step() {
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true); // hold: fills its step
        pattern.set_tied(2, true);
        pattern.toggle(2, 67); // slide, nothing after: half-step gate
        pattern.toggle(4, 62);
        pattern.set_tied(5, true);
        pattern.toggle(5, 64); // slide held onward by a tie
        pattern.set_tied(6, true);

        assert!(
            pattern.held_past_gate(0),
            "a note followed by a tie is held"
        );
        assert!(pattern.held_past_gate(1), "a hold fills its whole step");
        assert!(
            !pattern.held_past_gate(2),
            "a slide starts a new note and closes at the ordinary gate"
        );
        assert!(
            pattern.held_past_gate(5),
            "unless the step after the slide is tied too"
        );
    }

    #[test]
    fn slide_head_names_the_note_a_tied_step_continues() {
        let mut pattern = Pattern::empty();
        pattern.toggle(2, 60);
        pattern.set_tied(3, true); // a hold: its head is step 2
        pattern.set_tied(4, true);

        assert_eq!(pattern.slide_head(3), Some(2));
        assert_eq!(
            pattern.slide_head(4),
            Some(2),
            "through any number of holds"
        );
        assert_eq!(
            pattern.slide_head(1),
            None,
            "nothing sounds before step 1's tie would"
        );
        assert_eq!(
            pattern.slide_head(0),
            None,
            "the wrap reaches only a rest here, which heads nothing"
        );
    }

    #[test]
    fn slide_head_wraps_the_loop_point_as_playback_does() {
        // Live, a tie on step 1 holds the last step's note across the repeat — so a slide there
        // is authorable, continuing that note. The walk wraps once, bounded: a full cycle of
        // empty ties is no head, which is also what keeps it terminating on an all-tied pattern.
        let mut pattern = Pattern::empty();
        pattern.toggle(15, 60);
        assert_eq!(
            pattern.slide_head(0),
            Some(15),
            "step 1 slides from the last step's note, across the loop point"
        );

        let mut all_tied = Pattern::empty();
        for step in 0..STEPS {
            all_tied.set_tied(step, true);
        }
        assert_eq!(
            all_tied.slide_head(3),
            None,
            "a cycle of nothing but ties reaches no head and terminates"
        );
    }

    #[test]
    fn walking_back_from_a_fully_tied_pattern_terminates() {
        // It stops at the first step rather than wrapping. Live a tie there continues across the
        // loop point, but a search that wrapped would not terminate on this pattern.
        let mut pattern = Pattern::empty();
        for step in 0..STEPS {
            pattern.set_tied(step, true);
        }
        assert_eq!(pattern.run_start(STEPS - 1), 0);
    }

    #[test]
    fn a_pattern_of_ties_alone_is_silent_but_is_not_nothing() {
        // `is_empty` decides "does this make a sound"; `is_blank` decides "is there anything to
        // save". A half-built pattern is the case that separates them, and treating it as nothing
        // would throw away work in progress.
        let mut pattern = Pattern::empty();
        assert!(pattern.is_empty() && pattern.is_blank());

        pattern.set_tied(3, true);
        assert!(pattern.is_empty(), "no notes, so nothing sounds");
        assert!(
            !pattern.is_blank(),
            "but a tie was entered and must survive a save"
        );
    }

    #[test]
    fn clearing_a_pattern_takes_the_ties_with_it() {
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true);
        pattern.clear();
        assert!(
            pattern.is_blank(),
            "Clear means an empty pattern, ties included"
        );
    }

    #[test]
    fn toggling_a_note_puts_it_in_and_takes_it_out() {
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        assert!(pattern.step(0).contains(60));
        pattern.toggle(0, 60);
        assert!(!pattern.step(0).contains(60));
    }

    #[test]
    fn a_step_holds_a_chord_even_though_random_writes_one_note() {
        let step = Step::EMPTY.with(60).with(63).with(67);
        assert_eq!(step.count(), 3);
        assert_eq!(step.notes(), vec![60, 63, 67]);
    }

    #[test]
    fn visiting_notes_without_allocating_gives_the_same_answer() {
        // `for_each` is the audio-thread path; `notes` is the GUI one. They must agree.
        let step = Step::EMPTY.with(36).with(60).with(127);
        let mut visited = Vec::new();
        step.for_each(|n| visited.push(n));
        assert_eq!(visited, step.notes());
    }

    #[test]
    fn middle_c_is_named_the_same_as_the_on_screen_keyboard_calls_it() {
        assert_eq!(note_name(60), "C3");
        assert_eq!(note_name(0), "C-2");
        assert_eq!(note_name(127), "G8");
    }

    #[test]
    fn every_note_round_trips_through_its_name() {
        for note in 0u8..128 {
            let name = note_name(note);
            assert_eq!(parse_note(&name), Some(note), "{name}");
        }
    }

    #[test]
    fn flats_are_accepted_because_people_write_e_flat() {
        // C Dorian is spelled with E flat and B flat, so a hand-edited file will contain them.
        assert_eq!(parse_note("Eb3"), parse_note("D#3"));
        assert_eq!(parse_note("Bb2"), parse_note("A#2"));
    }

    #[test]
    fn a_pattern_round_trips_through_its_saved_form() {
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.toggle(0, 67);
        pattern.toggle(15, 48);

        let data = PatternData::from(&pattern);
        let (back, problems) = data.to_pattern_shaped(1, STEPS);
        assert!(problems.is_empty());
        assert_eq!(back, pattern);
    }

    #[test]
    fn a_saved_pattern_reads_as_note_names() {
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        let data = PatternData::from(&pattern);
        assert_eq!(data.0[0], vec!["C3".to_owned()]);
    }

    #[test]
    fn one_bad_note_name_does_not_cost_the_other_fifteen_steps() {
        let data = PatternData(vec![
            vec!["C3".to_owned()],
            vec!["H9".to_owned()],
            vec!["G3".to_owned()],
        ]);
        let (pattern, problems) = data.to_pattern_shaped(1, STEPS);

        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("H9"), "{:?}", problems);
        assert!(pattern.step(0).contains(60), "step 1 should survive");
        assert!(pattern.step(2).contains(67), "step 3 should survive");
    }

    #[test]
    fn an_out_of_range_note_is_refused_rather_than_wrapping() {
        assert_eq!(parse_note("C9"), None);
        assert_eq!(parse_note("C-3"), None);
    }
}
