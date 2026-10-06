//! The sequencer as the audio thread sees it: a clock, a pattern, and what is currently sounding.
//!
//! Deliberately knows nothing about CLAP, the press table, or how a note is emitted. It answers
//! *what should happen and when*; the worker answers *how*. That split is what lets every timing
//! property be tested without an audio device or a plugin.

use super::clock::{Boundary, Clock, DEFAULT_TEMPO, MAX_BOUNDARIES_PER_CHUNK, Transport};
use super::pattern::{Pattern, Step};
use crate::events::input::{MAX_INPUT_PRODUCERS, SourceId};

/// The sequencer's press identity.
///
/// **A press source, but not an input producer.** It has no queue and pushes nothing — it emits
/// directly on the audio thread — so it sits past the producer slots on purpose.
/// `MAX_INPUT_PRODUCERS` counts *queues* and sizes the merged buffer; giving the sequencer a slot
/// there would size that buffer for a producer that never pushes.
pub const SEQUENCER_SOURCE: SourceId = SourceId(MAX_INPUT_PRODUCERS as u8);

/// The whole of the sequencer's state, as one replaceable value.
///
/// Published to the audio thread by value: 256 bytes of pattern plus a few scalars, `Copy`, no
/// allocation and no `Arc` to drop in a realtime callback.
///
/// **Transport is part of the state, not a stream of events.** That is what removes the ordering
/// problem rather than solving it: Play, Pause and Stop cannot be dropped, reordered or coalesced
/// into the wrong outcome, because they are not messages. `generation` is what makes pressing Play
/// twice restart rather than look like a no-op.
///
/// **Not `Copy`, and it must not become so again.** It owns a pattern whose length is whatever
/// somebody made it, and it reaches the audio thread as an `Arc` — which is what let the length stop
/// being a compile-time constant. A `Copy` version would be one that had a maximum again.
#[derive(Clone, Debug, PartialEq)]
pub struct SequencerState {
    pub pattern: Pattern,
    /// What each step **sets**, beyond which notes it plays.
    ///
    /// Beside the pattern rather than inside it: `Pattern` is notes and ties and knows nothing about
    /// any instrument, and locks are parameter ids belonging to whatever plugin is loaded. Keeping
    /// them apart is what lets a sequence be loaded into a different instrument and still play.
    pub locks: super::locks::LockSet,
    /// The step being edited, if any.
    ///
    /// **Published so that the runtime can preview it**, which is what makes the runtime the *only*
    /// thing that ever applies a modulation offset. An earlier design had the host apply the preview
    /// itself; the offsets were then owned by two parties, and every question about taking one off
    /// again — a stop, a Clear, a plugin change, a queue that would not take the event — had to be
    /// answered twice, in a place that could not order its answer against the other's.
    /// **`u32`, not `u8`** — the sequence has no maximum length, and an `Option<u8>` here was a
    /// silent 255-step cap on which steps could be previewed at all.
    pub editing: Option<u32>,
    /// Parameters whose **base already carries the step's value**, so their preview must be zero.
    ///
    /// A knob edited in the plugin's own editor keeps its value for as long as the step stays
    /// selected — that is what the person set, and a knob that springs back to the patch on release
    /// reads as the control refusing input, however well annotated. While the base is parked there,
    /// previewing the lock's offset on top would sound a whole deviation too high, so the preview
    /// for exactly these parameters is zero. The player restores the bases — and empties this — when
    /// the step is left.
    pub held: HeldParams,
    pub tempo: f64,
    /// Loop only a window of the sequence: `(first bar, how many bars)` — `Bar` scope is one bar,
    /// `Pattern` scope is eight. `None` is `All`. **Transport state, not music**: no file keeps
    /// it, and a render ignores it.
    pub bar: Option<(u32, u32)>,
    pub transport: Transport,
    /// Bumped by any command that must restart a run, even when nothing else changed.
    pub generation: u64,
    /// Publication serial, acknowledged by the worker.
    ///
    /// Separate from `generation` because they answer different questions: `generation` says
    /// "start again", `serial` says "I have this one" — and a tempo nudge bumps the second without
    /// restarting the run.
    pub serial: u64,
}

impl Default for SequencerState {
    fn default() -> Self {
        Self {
            pattern: Pattern::empty(),
            locks: crate::sequencer::locks::LockSet::EMPTY,
            editing: None,
            held: HeldParams::EMPTY,
            tempo: DEFAULT_TEMPO,
            bar: None,
            transport: Transport::Stopped,
            generation: 0,
            serial: 0,
        }
    }
}

/// What the worker must do at a sample offset in this chunk.
///
/// **No `Eq`.** `SetParam` carries an `f32`, and a value is compared for equality here only by tests
/// that are comparing stored numbers — which `PartialEq` gives them.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Action {
    /// Release these notes.
    Release { frame: u32, notes: Step },
    /// Sound these notes.
    Sound { frame: u32, notes: Step },
    /// Modulate a parameter, because this step deviates from the patch.
    ///
    /// **An offset, not a value** — `CLAP_EVENT_PARAM_MOD`. It is laid over the parameter without
    /// disturbing it, so the parameter's own value stays the patch for as long as the sequence runs
    /// and no amount of sequencing can erode it. `value` is the offset, and zero means *this step
    /// does not move it*, which still has to be sent — see [`crate::sequencer::locks`].
    ///
    /// **Not a gesture.** No begin, no end — it is automation, which is what it is, and it avoids
    /// taking on sixteen more chances per bar to leave a gesture open.
    SetParam {
        frame: u32,
        /// **Which plugin's parameter, and which one.** Not a bare id: the same `u32` can name a
        /// parameter on two different plugins, so an action without a target could be delivered to
        /// the wrong one. See [`super::locks::LockKey`].
        key: super::locks::LockKey,
        value: f32,
    },
}

impl Action {
    pub fn frame(self) -> u32 {
        match self {
            Action::Release { frame, .. }
            | Action::Sound { frame, .. }
            | Action::SetParam { frame, .. } => frame,
        }
    }

    /// The notes this action carries, if it carries any.
    ///
    /// **Fallible now.** It used to assume every action was about notes; `SetParam` is not, and
    /// making that a compile error at every call site was the point of changing the signature rather
    /// than returning an empty `Step`.
    pub fn notes(self) -> Option<Step> {
        match self {
            Action::Release { notes, .. } | Action::Sound { notes, .. } => Some(notes),
            Action::SetParam { .. } => None,
        }
    }
}

/// Every action one chunk of *stepping* can produce: each boundary sounding, releasing and setting
/// every locked parameter, plus the wrap.
const STEPPING_ACTIONS: usize =
    MAX_BOUNDARIES_PER_CHUNK * (2 + super::locks::MAX_LOCKED_PARAMS) + 1;

/// The most actions one chunk can produce, from **every** source.
///
/// `advance` prepends what it owes before it steps, so one chunk can carry all three at once: the
/// zeroes for a lock set just abandoned, a full preview of the step being edited, and then a whole
/// chunk of stepping. Replacing one fully locked pattern with a disjoint fully locked one does
/// exactly that.
///
/// **This is the size both the runtime's own vector and the worker's scratch are built from.** They
/// hold the same actions; sizing them separately is how one ends up smaller than what it is handed
/// and allocates inside the audio callback — which has now happened twice, once when locks began
/// emitting per parameter and once when the owed work moved into `advance` and only the emission
/// changed.
pub const MAX_ACTIONS_PER_CHUNK: usize =
    STEPPING_ACTIONS + super::locks::MAX_LOCKED_PARAMS + super::locks::MAX_LOCKED_PARAMS;

// **Checked at compile time, not by a test.** A test reports the drift; this refuses to build with
// it. Every additive source is named, so adding a fourth without widening the bound cannot compile.
const _: () = assert!(
    MAX_ACTIONS_PER_CHUNK
        >= STEPPING_ACTIONS + super::locks::MAX_LOCKED_PARAMS + super::locks::MAX_LOCKED_PARAMS,
    "the shared maximum must cover a chunk of stepping, its owed zeroes and a full preview at once"
);

/// Parameters whose base carries a step's value, published by value with the rest of the state.
///
/// A fixed set, `Copy`, bounded by what one pattern can lock — the same shape as every other
/// audio-thread collection here.
#[derive(Copy, Clone, Debug)]
pub struct HeldParams {
    ids: [super::locks::LockKey; super::locks::MAX_LOCKED_PARAMS],
    count: usize,
}

impl HeldParams {
    pub const EMPTY: Self = Self {
        ids: [super::locks::LockKey::source(0); super::locks::MAX_LOCKED_PARAMS],
        count: 0,
    };

    pub fn contains(&self, key: impl Into<super::locks::LockKey>) -> bool {
        self.ids[..self.count].contains(&key.into())
    }

    pub fn insert(&mut self, key: impl Into<super::locks::LockKey>) {
        let key = key.into();
        if self.contains(key) || self.count == self.ids.len() {
            return;
        }
        self.ids[self.count] = key;
        self.count += 1;
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = super::locks::LockKey> + '_ {
        self.ids[..self.count].iter().copied()
    }
}

/// Compared by contents, not arrival order, for the same reason [`super::locks::LockSet`] is.
impl PartialEq for HeldParams {
    fn eq(&self, other: &Self) -> bool {
        self.count == other.count && self.ids[..self.count].iter().all(|id| other.contains(*id))
    }
}

/// The audio-thread half of the sequencer.
pub struct Runtime {
    clock: Clock,
    /// The state being played, **held by pointer and never cloned**.
    ///
    /// Copying it out was what forced the pattern to be a fixed-size array: a `Vec` copied here
    /// would allocate inside the callback. Reading through the pointer is what lets a sequence be
    /// any length at all. The outgoing pointer leaves through [`Runtime::apply`]'s return, to be
    /// released on the thread that made it.
    state: std::sync::Arc<SequencerState>,
    transport: Transport,
    generation: u64,
    /// The notes the sequencer currently believes are sounding.
    sounding: Step,
    /// The step being previewed, if any. See [`SequencerState::editing`].
    editing: Option<u32>,
    /// The loop window the boundaries in flight were computed for: `(first step, length)`.
    ///
    /// A mirror of the clock's, **updated when the switch actually happens** rather than when it
    /// is queued — the tie lookahead must describe the window a boundary belongs to, not the one
    /// somebody just asked for.
    window: (usize, usize),
    /// The window the state last asked for, so a republish that changes nothing queues nothing.
    bar: Option<(u32, u32)>,
    /// Parameters whose preview is suppressed because their base carries the step's value. See
    /// [`SequencerState::held`].
    held: HeldParams,
    /// Whether the preview has changed and its offsets have not been emitted yet.
    preview_owed: bool,
    /// Preallocated scratch, so advancing never allocates on the audio thread.
    boundaries: Vec<Boundary>,
    actions: Vec<Action>,
    /// Parameters this runtime currently has a **non-zero offset** applied to.
    ///
    /// Only ever added to while emitting, which is what bounds the debt below: a parameter can be
    /// abandoned only if it was applied, and nothing becomes applied between two renders.
    applied: AbandonedOffsets,
    /// Parameters this runtime has been modulating and is about to stop.
    ///
    /// **The runtime owes these a zero**, and only it can order that against the steps it is still
    /// emitting — see [`Runtime::apply`]. Emitted at the next render, at frame 0.
    ///
    /// **Bounded by [`super::locks::MAX_LOCKED_PARAMS`], provably.** An entry is only added for a
    /// parameter in `applied`, and `applied` only grows while emitting — which happens during a
    /// render, which is also when the debt is paid. So between two renders the debt can name at most
    /// the parameters that were applied at the last one, and there can be no more of those than a
    /// pattern can lock.
    abandoned: AbandonedOffsets,
}

/// The parameters a runtime owes a zero offset.
///
/// A fixed set, because this is read and written on the audio thread. Its capacity matches
/// [`super::locks::MAX_LOCKED_PARAMS`], which is the most a pattern can modulate at once — so it
/// cannot overflow with anything that matters.
#[derive(Copy, Clone, Debug)]
struct AbandonedOffsets {
    ids: [super::locks::LockKey; super::locks::MAX_LOCKED_PARAMS],
    count: usize,
}

impl AbandonedOffsets {
    const EMPTY: Self = Self {
        ids: [super::locks::LockKey::source(0); super::locks::MAX_LOCKED_PARAMS],
        count: 0,
    };

    fn insert(&mut self, param_id: impl Into<super::locks::LockKey>) {
        let param_id = param_id.into();
        if self.ids[..self.count].contains(&param_id) || self.count == self.ids.len() {
            return;
        }
        self.ids[self.count] = param_id;
        self.count += 1;
    }

    fn contains(&self, param_id: impl Into<super::locks::LockKey>) -> bool {
        let param_id = param_id.into();
        self.ids[..self.count].contains(&param_id)
    }

    fn remove(&mut self, param_id: impl Into<super::locks::LockKey>) {
        let param_id = param_id.into();
        let Some(index) = self.ids[..self.count].iter().position(|id| *id == param_id) else {
            return;
        };
        self.count -= 1;
        self.ids[index] = self.ids[self.count];
    }

    fn drain(
        &mut self,
    ) -> (
        [super::locks::LockKey; super::locks::MAX_LOCKED_PARAMS],
        usize,
    ) {
        let taken = (self.ids, self.count);
        self.count = 0;
        taken
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

impl Runtime {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            clock: Clock::new(DEFAULT_TEMPO, sample_rate),
            state: std::sync::Arc::new(SequencerState::default()),
            transport: Transport::Stopped,
            generation: 0,
            sounding: Step::EMPTY,
            applied: AbandonedOffsets::EMPTY,
            editing: None,
            window: (0, crate::sequencer::pattern::STEPS),
            bar: None,
            held: HeldParams::EMPTY,
            preview_owed: false,
            abandoned: AbandonedOffsets::EMPTY,
            boundaries: Vec::with_capacity(MAX_BOUNDARIES_PER_CHUNK),
            // Two note actions per boundary, plus one `SetParam` per lockable parameter, plus room
            // for a teardown release. Sized from the same constant as the worker's vector: a `push`
            // past capacity here would allocate **inside the audio callback**, and sizing one of the
            // two from a stale number is exactly how that happens.
            actions: Vec::with_capacity(MAX_ACTIONS_PER_CHUNK),
        }
    }

    pub fn transport(&self) -> Transport {
        self.transport
    }

    pub fn step(&self) -> usize {
        self.clock.step()
    }

    pub fn tempo(&self) -> f64 {
        self.clock.tempo()
    }

    pub fn beats(&self) -> f64 {
        self.clock.beats()
    }

    pub fn seconds(&self) -> f64 {
        self.clock.seconds()
    }

    pub fn sounding(&self) -> Step {
        self.sounding
    }

    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.clock.set_sample_rate(sample_rate);
    }

    /// Adopts a published state, returning notes that must be released as a result.
    ///
    /// Returns what to release rather than releasing anything itself: emission belongs to the
    /// worker, and **nothing may treat a release as delivered that no plugin has received.**
    #[must_use]
    /// Applies a new state, reporting the notes it orphans.
    ///
    /// **What it stops modulating is reported separately**, through
    /// [`Runtime::take_abandoned_offsets`]: a parameter that leaves the lock set, or every locked
    /// parameter when the transport comes to rest, is owed a zero — and only the runtime can order
    /// that correctly against the steps it is still emitting. The host cannot: it would have to send
    /// the zero before publishing the state that stops the stepping, and the two travel different
    /// queues, so an old step could overtake the zero and leave the parameter stuck.
    /// **Returns the state it replaced**, which the caller must send somewhere it can be freed.
    /// Dropping it here would call the allocator on the audio thread — see
    /// `AudioWorker::retire_sequencer`.
    pub fn apply(
        &mut self,
        state: std::sync::Arc<SequencerState>,
    ) -> (Step, std::sync::Arc<SequencerState>) {
        let restart = state.generation != self.generation;
        self.generation = state.generation;

        // Anything the outgoing set was modulating that the incoming one does not name.
        // **By index, because `param_ids` allocates** and this runs on the audio thread. Fetching
        // the id afresh each turn releases the borrow on the state before the body needs
        // `self.abandoned`.
        let mut index = 0;
        while let Some(param_id) = self.state.locks.param_id_at(index) {
            // **Only if it is actually applied.** Queueing a zero for a parameter that never
            // received a non-zero offset would be an event the plugin does not need, and — because
            // the debt is bounded — could crowd out one that is needed.
            if !state.locks.locks_anywhere(param_id) && self.applied.contains(param_id) {
                self.abandoned.insert(param_id);
            }
            index += 1;
        }
        let locks_moved = state.locks != self.state.locks;

        // **Swapped, never cloned.** The pattern is heap now, so a copy here would allocate inside
        // the callback — which is the whole reason a sequence used to need a maximum length.
        let previous = std::mem::replace(&mut self.state, state);
        // **The pattern's length is the clock's loop point**, and this is the only place the two
        // are joined. A clock wrapping at a different length from the pattern it plays would put
        // the playhead on a step the pattern does not have — which is not a wrong note, it is the
        // wrong *bar*, silently.
        //
        // With scopes, "the loop" is a window: the whole sequence in `All`, one bar in `Bar`. A
        // **scope or bar change while playing is queued for the current bar's end** — an instant
        // jump lands off the downbeat — and everything else (a resize, a restart, anything at
        // rest) applies immediately, exactly as `set_length` always did.
        self.clock
            .set_bar_length(self.state.pattern.steps_per_bar());
        let total = self.state.pattern.len();
        let desired = match self.state.bar {
            Some((first, count)) => {
                let spb = self.state.pattern.steps_per_bar();
                let start = (first as usize * spb).min(total.saturating_sub(1));
                (start, (count as usize * spb).min(total - start))
            }
            None => (0, total),
        };
        let scope_moved = self.state.bar != self.bar;
        self.bar = self.state.bar;
        if scope_moved
            && self.transport == Transport::Playing
            && self.state.transport == Transport::Playing
            && !restart
        {
            let entry = if self.state.bar.is_some() {
                super::clock::Entry::WindowStart
            } else {
                super::clock::Entry::Continue
            };
            self.clock.queue_window(desired.0, desired.1, entry);
            // `self.window` keeps describing the boundaries still in flight; it follows when the
            // switch is emitted.
        } else {
            self.clock.set_window(desired.0, desired.1);
            self.window = desired;
        }
        let state = &self.state;

        // **The preview, while nothing is playing.** Selecting a step puts the instrument at that
        // step so you can hear what you are editing; leaving it takes the offsets off again. Doing
        // it here rather than in the host is what keeps one owner: the same code that stops applying
        // an offset is the code that applied it, and it can order the two against each other.
        // **On a changed selection *or* changed locks.** Editing a step that is already selected
        // changes neither the selection nor the generation, so keying only on those left every edit
        // after the first inaudible — and the test missed it because selecting and editing in the
        // same breath are coalesced into one publish, which does change the selection.
        let editing = state.editing.filter(|_| !state.transport.is_playing());
        let changed = editing != self.editing || state.held != self.held || locks_moved || restart;
        self.editing = editing;
        self.held = state.held;
        // **Only while at rest**, because a running sequence is already emitting an offset per step
        // and a preview laid over that would fight it. And on *leaving* a step as much as entering
        // one: the zeroes that take the preview off are this same emission with no step selected,
        // which is why the flag cannot be conditional on there being one.
        if changed && !state.transport.is_playing() {
            self.preview_owed = true;
        }

        // Tempo only ever changes between chunks, so a chunk has exactly one tempo — which is what
        // keeps the CLAP transport coherent as of sample 0.
        self.clock.set_tempo(state.tempo);

        let was = self.transport;
        self.transport = state.transport;

        let released = match (was, state.transport) {
            // Pause snaps to the step it was in: resume then has no phase to reconcile, and the
            // small backward step happens while nothing is sounding.
            // **Coming to rest abandons every offset**: the last step to run left one applied and
            // there is no next step to replace it, so the instrument would sit permanently wherever
            // the playhead happened to stop.
            (_, Transport::Paused) => {
                if was != Transport::Paused {
                    self.abandon_all();
                }
                self.clock.snap_to_step();
                self.release_all()
            }
            (_, Transport::Stopped) => {
                // **On the transition, not on every publish.** A stopped sequencer receives a new
                // state every time a lock is written, and abandoning on each of them would take the
                // offset off again the instant it was applied — editing a step would be silent.
                if was != Transport::Stopped {
                    self.abandon_all();
                }
                self.clock.reset();
                self.release_all()
            }
            // Any transition *into* playing with a new run generation restarts — including from
            // paused. That is the whole difference between Play/Pause and Play-from-start: both
            // reach `Playing`, and only the second bumps the generation. Matching on
            // `Playing -> Playing` alone would make restart-from-paused silently resume instead.
            (_, Transport::Playing) if restart => {
                self.clock.reset();
                self.release_all()
            }
            _ => Step::EMPTY,
        };
        (released, previous)
    }

    /// [`Runtime::apply`] for tests, which do not have a worker to hand the old state back to.
    ///
    /// Dropping it here is fine: a test is not the audio thread. Production code must **not** use
    /// this — the whole point of `apply` returning the outgoing pointer is that somebody who is
    /// allowed to allocate releases it.
    #[cfg(test)]
    pub fn apply_for_test(&mut self, state: SequencerState) -> Step {
        self.apply(std::sync::Arc::new(state)).0
    }

    /// How many parameters are owed a zero right now. For the test that pins the bound.
    #[cfg(test)]
    pub fn owed_zero_count(&self) -> usize {
        self.abandoned.count
    }

    /// Marks every parameter this runtime is modulating as owed a zero.
    fn abandon_all(&mut self) {
        // By index: `param_ids` allocates, and this is the audio thread.
        let mut index = 0;
        while let Some(param_id) = self.state.locks.param_id_at(index) {
            if self.applied.contains(param_id) {
                self.abandoned.insert(param_id);
            }
            index += 1;
        }
    }

    /// Takes back a zero the input arena refused, so the next chunk sends it again.
    ///
    /// **`applied` is restored too**, or the bound would be wrong: the debt may only name parameters
    /// that are applied, and generating the zero had already marked this one clear.
    pub fn re_owe_zero(&mut self, param_id: impl Into<super::locks::LockKey>) {
        let param_id = param_id.into();
        self.applied.insert(param_id);
        self.abandoned.insert(param_id);
    }

    /// Records an offset this runtime is about to emit, so it knows what it has applied.
    fn note_applied(&mut self, param_id: super::locks::LockKey, value: f32) {
        if value == 0.0 {
            self.applied.remove(param_id);
        } else {
            self.applied.insert(param_id);
        }
    }

    /// Emits everything owed at the start of a chunk: the zeroes, then the preview.
    ///
    /// **Here rather than in the worker**, so that one place decides what has been applied and one
    /// place records it — which is what makes the debt provably bounded. The worker used to build
    /// these itself, and then the runtime could not know what was on the plugin.
    fn emit_owed(&mut self) {
        let (abandoned, count) = self.abandoned.drain();
        for param_id in &abandoned[..count] {
            self.applied.remove(*param_id);
            self.actions.push(Action::SetParam {
                frame: 0,
                key: *param_id,
                value: 0.0,
            });
        }

        if !self.preview_owed {
            return;
        }
        self.preview_owed = false;

        // Every locked parameter, so the answer is complete rather than a difference: the step's own
        // offset where it sets one, and zero where it does not.
        let step = self.editing.map(|step| step as usize);
        let locks = std::sync::Arc::clone(&self.state);
        let locks = &locks.locks;
        let mut index = 0;
        while let Some(param_id) = locks.param_id_at(index) {
            index += 1;
            // A held parameter's preview is zero: the step's value is already on the base, and the
            // lock's offset on top of it would sound a whole deviation too high.
            let value = match step
                .filter(|_| !self.held.contains(param_id))
                .and_then(|step| locks.get(step, param_id))
            {
                Some(value) => value - locks.patch(param_id).unwrap_or(value),
                None => 0.0,
            };
            self.note_applied(param_id, value);
            self.actions.push(Action::SetParam {
                frame: 0,
                key: param_id,
                value,
            });
        }
    }

    /// Forgets what it is holding, without emitting anything.
    ///
    /// For a global panic, where the press table has already been cleared and the notes are gone
    /// whatever the sequencer believed. It resumes at the next step boundary rather than trying to
    /// repair mid-step.
    pub fn forget_sounding(&mut self) {
        self.sounding = Step::EMPTY;
    }

    fn release_all(&mut self) -> Step {
        std::mem::replace(&mut self.sounding, Step::EMPTY)
    }

    /// Advances the clock by `frames` and returns what to do, in sample order.
    ///
    /// **Called for every chunk, before the sleep/wake decision, whatever the plugin's run state.**
    /// A clock that stopped while the plugin slept would never reach its next boundary, and a rest
    /// is exactly what puts a plugin to sleep.
    pub fn advance(&mut self, frames: u32) -> &[Action] {
        self.actions.clear();

        // **Before the transport check**, because the two things owed here — the zeroes that take an
        // abandoned offset off, and the preview of a selected step — both happen precisely when the
        // sequencer is *not* playing. This is also the only place `applied` grows, which is what
        // bounds the debt: nothing can be abandoned between two chunks that was not applied by the
        // end of the last one.
        self.emit_owed();

        if self.transport != Transport::Playing {
            return &self.actions;
        }

        let mut boundaries = std::mem::take(&mut self.boundaries);
        self.clock.advance(frames, &mut boundaries);

        for boundary in boundaries.iter().copied() {
            match boundary {
                // **The queued window just applied, and whatever the old bar still holds must be
                // released here** — the new window's first step follows at this same frame, and if
                // it opens with a tie its `StepStart` deliberately releases nothing. Two isolated
                // bars are two sequences; without this, a destination bar beginning with ties
                // would continue a note from the bar just left.
                Boundary::WindowSwitch { frame } => {
                    if !self.sounding.is_empty() {
                        let notes = std::mem::replace(&mut self.sounding, Step::EMPTY);
                        self.actions.push(Action::Release { frame, notes });
                    }
                    // From here on the boundaries describe the new window.
                    self.window = self.clock.window();
                }
                Boundary::GateClose { frame, step } => {
                    // **Two clauses, and both are needed.** `tied(step + 1)` is what stops an
                    // ordinary note ending halfway through its own step so that the tie after it
                    // has something left to continue; `tied(step)` — for an **empty** tie, a hold
                    // — is what makes a hold last a whole step rather than half of one. With only
                    // the first, a tied run comes out in halves.
                    //
                    // **A slide is not a hold.** A tie carrying notes started a new note at this
                    // step's own start, and that note gates like any other: half a step, unless
                    // the step after it is tied too. The emptiness test is `held_past_gate`'s —
                    // this is that rule with the window-aware lookahead, and the two must agree.
                    //
                    // The lookahead **wraps**, and that is right here: the pattern repeats, so a
                    // tie on step 1 holds step 16's note across the loop point. An export plays
                    // the pattern once and must not — `sequencer::export` forces a release at the
                    // bar end for exactly this reason.
                    // **The lookahead wraps within the window.** In `Bar` scope the selected bar
                    // is the sequence: a tie on its first step continues its last step's note, and
                    // asking about the *next bar's* first step would borrow a tie from outside
                    // what is playing or shown.
                    let (w_start, w_len) = self.window;
                    let after = if step + 1 >= w_start + w_len {
                        w_start
                    } else {
                        step + 1
                    };
                    let holds =
                        self.state.pattern.tied(step) && self.state.pattern.step(step).is_empty();
                    let suppressed = holds || self.state.pattern.tied(after);
                    if !suppressed && !self.sounding.is_empty() {
                        let notes = std::mem::replace(&mut self.sounding, Step::EMPTY);
                        self.actions.push(Action::Release { frame, notes });
                    }
                }
                Boundary::StepStart { frame, step } => {
                    // **Before the notes at this frame.** `FixedEventBuffer` exposes events in push
                    // order, so a note sounded before its cutoff arrives is a note played on the
                    // previous step's sound — which is the whole point of the feature, lost to one
                    // line's worth of ordering.
                    // **A step's locks are its own, whatever the step is.** Locks are per step —
                    // the owner's ruling — so a hold applies its locks over a note that is still
                    // sounding, which is what lets modulation walk under a held gate. A step with
                    // no lock for a parameter returns it to the patch (offset zero), exactly as
                    // every unlocked step always has; a head's lock does *not* carry through its
                    // run, and a hold that wants the offset keeps its own lock, which is what the
                    // panel shows and the whole of why the behaviour is predictable from it.
                    //
                    // This replaced a `run_start` resolution that made a tied run read its head's
                    // locks. Everything that reasoning had to defend — the export wrap, the
                    // bar-scope borrow — dissolves with it: nothing resolves a step's locks to any
                    // other step, live, rendered or in the file.
                    let mut applied = self.applied;
                    self.state.locks.for_each_at(step, |param_id, value| {
                        if value == 0.0 {
                            applied.remove(param_id);
                        } else {
                            applied.insert(param_id);
                        }
                        self.actions.push(Action::SetParam {
                            frame,
                            key: param_id,
                            value,
                        });
                    });
                    self.applied = applied;

                    let notes = self.state.pattern.step(step);

                    if !self.state.pattern.tied(step) {
                        // A step start releases anything still held first. With a 50% gate that is
                        // normally nothing, but a tempo change, a resume, or the end of a tied run
                        // can leave a note sounding — the last of those is how a run ends, at a
                        // step boundary rather than halfway through a step.
                        if !self.sounding.is_empty() {
                            let notes = std::mem::replace(&mut self.sounding, Step::EMPTY);
                            self.actions.push(Action::Release { frame, notes });
                        }
                        if !notes.is_empty() {
                            self.sounding = notes;
                            self.actions.push(Action::Sound { frame, notes });
                        }
                    } else if !notes.is_empty() {
                        // **A tie carrying notes starts a new run**, and it is legato: sound
                        // before release, both at this frame, so the plugin sees the new note
                        // arrive while the old one is still held. Release-then-sound gives it two
                        // disjoint notes at the same sample, which is a retrigger with extra steps.
                        //
                        // The worker is what makes the order survive contact with the press table
                        // — see `emit_sequencer_action`, which captures the identities to release
                        // before it emits anything.
                        let previous = std::mem::replace(&mut self.sounding, notes);
                        self.actions.push(Action::Sound { frame, notes });
                        if !previous.is_empty() {
                            self.actions.push(Action::Release {
                                frame,
                                notes: previous,
                            });
                        }
                    }
                    // An **empty** tie does nothing at all: the previous note simply continues,
                    // which is the whole of legato and the reason `k + 1` arithmetic works.
                }
            }
        }

        self.boundaries = boundaries;
        &self.actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- loop scope --------------------------------------------------------------------------

    /// Two bars of four steps: bar one plays note 60 on its first step, bar two note 72 on its.
    fn two_bars(first_tied: bool) -> Pattern {
        let mut pattern = Pattern::sized(2, 4);
        pattern.toggle(0, 60);
        if first_tied {
            // Bar one's note is held by ties to the bar's edge - so it is still sounding when the
            // switch lands - and bar two is nothing but ties: no untied step start ever releases,
            // so only the switch itself can. This is the exact shape the release rule exists for.
            for step in 1..8 {
                pattern.set_tied(step, true);
            }
        } else {
            pattern.toggle(4, 72);
        }
        pattern
    }

    fn state_with(pattern: Pattern, bar: Option<(u32, u32)>, generation: u64) -> SequencerState {
        SequencerState {
            locks: crate::sequencer::locks::LockSet::EMPTY,
            pattern,
            editing: None,
            held: HeldParams::EMPTY,
            tempo: 120.0,
            bar,
            transport: Transport::Playing,
            generation,
            serial: generation,
        }
    }

    fn sounds(actions: &[Action]) -> Vec<u8> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Sound { notes, .. } => Some(notes.notes()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn bar_scope_loops_the_selected_bar_and_nothing_else() {
        let mut runtime = Runtime::new(SR);
        runtime.apply_for_test(state_with(two_bars(false), Some((0, 1)), 1));

        // Four bars' worth of playing time; only bar one's note may ever sound.
        let mut heard = Vec::new();
        for _ in 0..380 {
            heard.extend(sounds(runtime.advance(512)));
        }
        assert!(heard.len() >= 3, "the bar looped: {heard:?}");
        assert!(
            heard.iter().all(|note| *note == 60),
            "bar scope must loop the selected bar alone: {heard:?}"
        );
    }

    #[test]
    fn a_bar_change_lands_at_the_bar_end_and_releases_what_sounds() {
        // The destination bar opens with a tie - the exact case the release rule exists for. A
        // tied step's StepStart deliberately releases nothing, so without the switch releasing,
        // bar one's note would sound on into a bar it does not belong to.
        let mut runtime = Runtime::new(SR);
        runtime.apply_for_test(state_with(two_bars(true), Some((0, 1)), 1));

        // Sound bar one's note, then ask for bar two mid-bar. Same generation: not a restart.
        let mut actions: Vec<Action> = runtime.advance(512).to_vec();
        runtime.apply_for_test(state_with(two_bars(true), Some((1, 1)), 1));

        // **Queued, not instant**: half a bar later the window has not moved. A bar of four steps
        // at 120 BPM is 24,000 frames; twenty chunks are well inside it.
        for _ in 0..20 {
            actions.extend(runtime.advance(512).iter().copied());
        }
        assert_eq!(
            runtime.clock.window().0,
            0,
            "a bar change waits for the bar's end, never jumps mid-bar"
        );

        // Play across the bar end and well into the next.
        let mut released_after_switch = false;
        let mut switched = false;
        for _ in 0..380 {
            for action in runtime.advance(512) {
                actions.push(*action);
                match action {
                    Action::Release { notes, .. } if switched && notes.contains(60) => {
                        released_after_switch = true;
                    }
                    _ => {}
                }
            }
            if !switched && runtime.clock.window().0 == 4 {
                switched = true;
                // Nothing may sound between the switch and here that belongs to bar one.
            }
        }
        assert!(switched, "the queued bar change was applied");
        // The note from bar one was released at or before the switch: after it, advancing a long
        // time in a bar of nothing but a tie and rests, note 60 must not still be sounding. The
        // runtime's own bookkeeping is the oracle.
        assert!(
            runtime.sounding.is_empty(),
            "nothing sounds in a bar of ties and rests: {:?}",
            runtime.sounding.notes()
        );
        let _ = released_after_switch;
        // And the release actually happened as an action, not merely as bookkeeping.
        assert!(
            actions.iter().any(
                |action| matches!(action, Action::Release { notes, .. } if notes.contains(60))
            ),
            "bar one's note was released"
        );
    }

    #[test]
    fn leaving_bar_scope_continues_into_the_next_bar() {
        let mut runtime = Runtime::new(SR);
        runtime.apply_for_test(state_with(two_bars(false), Some((0, 1)), 1));
        let mut heard = sounds(runtime.advance(512));
        // Back to the whole sequence, queued at the bar's end.
        runtime.apply_for_test(state_with(two_bars(false), None, 1));
        for _ in 0..380 {
            heard.extend(sounds(runtime.advance(512)));
        }
        // The sequence continues into bar two rather than restarting at bar one: note 72 arrives,
        // and the first note after the switch is 72, not another 60.
        let after_first = &heard[1..];
        assert!(
            after_first.contains(&72),
            "the sequence widened and bar two played: {heard:?}"
        );
        assert_eq!(
            after_first.first(),
            Some(&72),
            "leaving bar scope continues into the next bar, not back to bar one: {heard:?}"
        );
    }

    #[test]
    fn in_bar_scope_a_tie_on_the_bars_first_step_wraps_within_the_bar() {
        // The bar is the sequence: its first step's tie continues its *own* last note, exactly as
        // the sequence's step one continues step sixteen in All scope. The oracle is what is
        // *sounding just after the wrap*: with the tie honoured, the last step's note is still
        // held; with an unwindowed lookahead - which would consult the next bar's untied first
        // step - the gate releases it before the wrap.
        let mut pattern = Pattern::sized(2, 4);
        pattern.toggle(3, 65); // last step of bar one
        pattern.set_tied(0, true); // bar one's first step continues it
        let mut runtime = Runtime::new(SR);
        runtime.apply_for_test(state_with(pattern, Some((0, 1)), 1));

        let mut saw_last_step = false;
        let mut checked_wraps = 0;
        for _ in 0..760 {
            runtime.advance(512);
            let step = runtime.clock.step();
            if step == 3 {
                saw_last_step = true;
            }
            if saw_last_step && step == 0 {
                assert!(
                    runtime.sounding.contains(65),
                    "the tie must hold the bar's own last note across the bar's wrap"
                );
                saw_last_step = false;
                checked_wraps += 1;
            }
        }
        assert!(checked_wraps >= 2, "the bar looped: {checked_wraps}");
    }

    #[test]
    fn bar_scope_does_not_borrow_locks_from_a_bar_that_is_not_playing() {
        // A run headed in bar one, tied into bar two, with a lock on its head. In bar-two scope
        // the bar is the sequence: the tie is not continued and the head's lock is not borrowed -
        // the parameter is put back (offset zero), not held at a value from a bar nobody can see.
        let mut pattern = Pattern::sized(2, 4);
        pattern.toggle(3, 65); // bar one's last step heads a run
        pattern.set_tied(4, true); // that crosses into bar two
        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        locks.set(3, 7, 0.9, 0.5).expect("a lock on the head");

        let mut runtime = Runtime::new(SR);
        let mut state = state_with(pattern, Some((1, 1)), 1);
        state.locks = locks;
        runtime.apply_for_test(state);

        let mut offsets = Vec::new();
        for _ in 0..380 {
            for action in runtime.advance(512) {
                if let Action::SetParam {
                    key: crate::sequencer::locks::LockKey { fx: 0, param_id: 7 },
                    value,
                    ..
                } = action
                {
                    offsets.push(*value);
                }
            }
        }
        assert!(!offsets.is_empty(), "the locked parameter was addressed");
        assert!(
            offsets.iter().all(|offset| *offset == 0.0),
            "bar two must not sound bar one's lock: {offsets:?}"
        );
    }

    /// A stand-in sequence-patch for tests about the locks themselves.
    ///
    /// Named rather than repeated, so a test that is about *what a step sets* does not read as
    /// though the number mattered. The tests that are about the patch value say so.
    const PATCH: f32 = 0.5;

    const SR: f64 = 48_000.0;

    fn playing(pattern: Pattern, tempo: f64) -> Runtime {
        let mut runtime = Runtime::new(SR);
        let released = runtime.apply_for_test(SequencerState {
            locks: crate::sequencer::locks::LockSet::EMPTY,
            pattern: pattern.clone(),
            editing: None,
            held: HeldParams::EMPTY,
            tempo,
            bar: None,
            transport: Transport::Playing,
            generation: 1,
            serial: 1,
        });
        assert!(released.is_empty());
        runtime
    }

    fn one_note_on_every_step(note: u8) -> Pattern {
        let mut pattern = Pattern::empty();
        for step in 0..16 {
            pattern.toggle(step, note);
        }
        pattern
    }

    #[test]
    fn a_stopped_sequencer_does_nothing() {
        let mut runtime = Runtime::new(SR);
        assert!(runtime.advance(6000).is_empty());
    }

    #[test]
    fn the_first_step_sounds_the_instant_the_run_begins() {
        let mut runtime = playing(one_note_on_every_step(60), 120.0);
        let actions = runtime.advance(512);
        assert_eq!(
            actions,
            &[Action::Sound {
                frame: 0,
                notes: Step::EMPTY.with(60)
            }]
        );
    }

    #[test]
    fn a_note_is_released_before_the_next_is_sounded() {
        let mut runtime = playing(one_note_on_every_step(60), 120.0);
        let mut sequence = Vec::new();
        for _ in 0..40 {
            sequence.extend(runtime.advance(512).iter().copied());
        }

        let mut sounding = false;
        for action in &sequence {
            match action {
                Action::Sound { .. } => {
                    assert!(!sounding, "a note sounded while another was still held");
                    sounding = true;
                }
                Action::Release { .. } => {
                    assert!(sounding, "released something that was not sounding");
                    sounding = false;
                }
                // This pattern locks nothing, so one appearing here is the bug.
                Action::SetParam { .. } => panic!("no parameter is locked: {action:?}"),
            }
        }
        assert!(sequence.len() >= 4, "expected a run: {}", sequence.len());
    }

    #[test]
    fn an_empty_step_is_a_rest_and_sounds_nothing() {
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        // Steps 1..15 are empty.
        let mut runtime = playing(pattern.clone(), 120.0);

        // Just under one bar: 180 blocks of 512 is 92,160 frames, and a bar at 120 BPM is 96,000.
        // Deliberately not landing on a boundary, so the count cannot turn on a rounding hair.
        let mut sounds = 0;
        for _ in 0..180 {
            for action in runtime.advance(512) {
                if matches!(action, Action::Sound { .. }) {
                    sounds += 1;
                }
            }
        }
        assert_eq!(sounds, 1, "only step 1 holds a note");
    }

    #[test]
    fn a_rest_does_not_stop_the_clock() {
        // The blocker from round 1, as a property: a pattern that rests for fifteen steps must
        // still come back round to step 1.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        let mut runtime = playing(pattern.clone(), 120.0);

        // A bar and a half, so step 1 must come round exactly once more. Fifteen rests in
        // between is precisely what would sleep the plugin and freeze a `steady_time` playhead.
        let mut sounds = 0;
        for _ in 0..285 {
            for action in runtime.advance(512) {
                if matches!(action, Action::Sound { .. }) {
                    sounds += 1;
                }
            }
        }
        assert_eq!(
            sounds, 2,
            "step 1 should have come round despite fifteen rests"
        );
    }

    #[test]
    fn pause_reports_the_notes_it_is_holding_so_the_worker_can_release_them() {
        let mut runtime = playing(one_note_on_every_step(60), 120.0);
        runtime.advance(100); // sound step 1
        assert!(!runtime.sounding().is_empty());

        let released = runtime.apply_for_test(SequencerState {
            locks: crate::sequencer::locks::LockSet::EMPTY,
            pattern: one_note_on_every_step(60),
            editing: None,
            held: HeldParams::EMPTY,
            tempo: 120.0,
            bar: None,
            transport: Transport::Paused,
            generation: 1,
            serial: 2,
        });
        assert!(released.contains(60), "pause must hand back what it held");
        assert!(runtime.sounding().is_empty());
    }

    #[test]
    fn pause_keeps_the_step_and_stop_returns_to_the_first() {
        let mut runtime = playing(one_note_on_every_step(60), 120.0);
        runtime.advance(6000 * 3 + 100); // into step 4
        assert_eq!(runtime.step(), 3);

        let state = SequencerState {
            locks: crate::sequencer::locks::LockSet::EMPTY,
            pattern: one_note_on_every_step(60),
            editing: None,
            held: HeldParams::EMPTY,
            tempo: 120.0,
            bar: None,
            transport: Transport::Paused,
            generation: 1,
            serial: 2,
        };
        let _ = runtime.apply_for_test(state.clone());
        assert_eq!(runtime.step(), 3, "pause holds its place");

        let _ = runtime.apply_for_test(SequencerState {
            bar: None,
            transport: Transport::Stopped,
            ..state
        });
        assert_eq!(runtime.step(), 0, "stop goes back to the beginning");
    }

    #[test]
    fn resuming_from_pause_plays_the_whole_step_it_was_in() {
        let mut runtime = playing(one_note_on_every_step(60), 120.0);
        runtime.advance(6000 + 3500); // into step 2, past its gate

        let state = SequencerState {
            locks: crate::sequencer::locks::LockSet::EMPTY,
            pattern: one_note_on_every_step(60),
            editing: None,
            held: HeldParams::EMPTY,
            tempo: 120.0,
            bar: None,
            transport: Transport::Paused,
            generation: 1,
            serial: 2,
        };
        let _ = runtime.apply_for_test(state.clone());
        let _ = runtime.apply_for_test(SequencerState {
            bar: None,
            transport: Transport::Playing,
            ..state
        });

        // The first thing that happens on resume is step 2 sounding from its start.
        let actions: Vec<_> = runtime.advance(512).to_vec();
        assert!(
            matches!(actions.first(), Some(Action::Sound { frame: 0, .. })),
            "expected the step to restart: {actions:?}"
        );
        assert_eq!(runtime.step(), 1);
    }

    #[test]
    fn play_from_start_restarts_even_from_paused() {
        // The whole difference between the two transport buttons. Both reach `Playing`; only
        // Play-from-start bumps the generation, and matching on `Playing -> Playing` alone would
        // make this case silently resume instead.
        let pattern = one_note_on_every_step(60);
        let mut runtime = playing(pattern.clone(), 120.0);
        runtime.advance(6000 * 5);
        assert_eq!(runtime.step(), 5);

        let paused = SequencerState {
            locks: crate::sequencer::locks::LockSet::EMPTY,
            pattern: pattern.clone(),
            editing: None,
            held: HeldParams::EMPTY,
            tempo: 120.0,
            bar: None,
            transport: Transport::Paused,
            generation: 1,
            serial: 2,
        };
        let _ = runtime.apply_for_test(paused.clone());
        assert_eq!(runtime.step(), 5, "pause holds its place");

        let _ = runtime.apply_for_test(SequencerState {
            bar: None,
            transport: Transport::Playing,
            generation: 2,
            serial: 3,
            ..paused
        });
        assert_eq!(
            runtime.step(),
            0,
            "play-from-start rewinds, even from paused"
        );
    }

    #[test]
    fn play_pause_resumes_from_paused_without_rewinding() {
        // The other half: same generation, so it must not restart.
        let pattern = one_note_on_every_step(60);
        let mut runtime = playing(pattern.clone(), 120.0);
        runtime.advance(6000 * 5);

        let paused = SequencerState {
            locks: crate::sequencer::locks::LockSet::EMPTY,
            pattern: pattern.clone(),
            editing: None,
            held: HeldParams::EMPTY,
            tempo: 120.0,
            bar: None,
            transport: Transport::Paused,
            generation: 1,
            serial: 2,
        };
        let _ = runtime.apply_for_test(paused.clone());
        let _ = runtime.apply_for_test(SequencerState {
            bar: None,
            transport: Transport::Playing,
            serial: 3,
            ..paused
        });
        assert_eq!(runtime.step(), 5, "resume keeps its place");
    }

    #[test]
    fn pressing_play_again_restarts_rather_than_doing_nothing() {
        let pattern = one_note_on_every_step(60);
        let mut runtime = playing(pattern.clone(), 120.0);
        runtime.advance(6000 * 5);
        assert_eq!(runtime.step(), 5);

        // Same transport, new generation: that is what a second press of Play looks like.
        let _ = runtime.apply_for_test(SequencerState {
            locks: crate::sequencer::locks::LockSet::EMPTY,
            pattern: pattern.clone(),
            editing: None,
            held: HeldParams::EMPTY,
            tempo: 120.0,
            bar: None,
            transport: Transport::Playing,
            generation: 2,
            serial: 3,
        });
        assert_eq!(runtime.step(), 0, "a fresh run starts at the beginning");
    }

    #[test]
    fn a_panic_leaves_the_sequencer_holding_nothing() {
        let mut runtime = playing(one_note_on_every_step(60), 120.0);
        runtime.advance(100);
        assert!(!runtime.sounding().is_empty());

        runtime.forget_sounding();
        assert!(runtime.sounding().is_empty());

        // And it picks up again at the next boundary rather than trying to repair mid-step.
        let mut sounded = false;
        for _ in 0..40 {
            if runtime
                .advance(512)
                .iter()
                .any(|a| matches!(a, Action::Sound { .. }))
            {
                sounded = true;
                break;
            }
        }
        assert!(sounded, "it should resume at the next step");
    }

    #[test]
    fn actions_come_back_in_sample_order() {
        let mut runtime = playing(one_note_on_every_step(60), 300.0);
        for _ in 0..20 {
            let actions = runtime.advance(4096);
            for pair in actions.windows(2) {
                assert!(pair[0].frame() <= pair[1].frame(), "{pair:?}");
            }
        }
    }

    #[test]
    fn the_sequencer_is_a_press_source_but_not_a_producer_slot() {
        // The distinction matters: MAX_INPUT_PRODUCERS sizes the merged buffer for queues, and the
        // sequencer has none.
        assert_eq!(SEQUENCER_SOURCE.index(), MAX_INPUT_PRODUCERS);
        assert!(!SEQUENCER_SOURCE.is_gui());
    }

    #[test]
    fn only_a_parameter_that_was_applied_is_owed_a_zero() {
        // **The bound, and why the capacity alone is not it.** The debt is a fixed set, so it cannot
        // grow without limit whatever happens - but a set filled with parameters that were never
        // modulated would crowd out one that *was*, and that one would then stay modulated for ever.
        //
        // A zero is therefore owed only for a parameter actually applied, and nothing becomes
        // applied except while rendering. So between two renders the debt names at most what was
        // applied at the last one - here, exactly one thing.
        use crate::sequencer::locks::{LockSet, MAX_LOCKED_PARAMS};

        let mut runtime = Runtime::new(SR);

        // One parameter, previewed and therefore applied.
        let mut locks = LockSet::EMPTY;
        locks.set(0, 7, 0.9, 0.2).expect("room");
        let _ = runtime.apply_for_test(SequencerState {
            pattern: Pattern::empty(),
            locks,
            editing: Some(0),
            held: HeldParams::EMPTY,
            tempo: 120.0,
            bar: None,
            transport: Transport::Stopped,
            generation: 1,
            serial: 1,
        });
        assert!(
            !runtime.advance(512).is_empty(),
            "the preview must have been emitted, or nothing is applied and this proves nothing"
        );

        // Now a great many distinct lock sets, none of them ever rendered.
        for round in 1..8u32 {
            let mut locks = LockSet::EMPTY;
            for i in 0..MAX_LOCKED_PARAMS as u32 {
                locks.set(0, round * 1000 + i, 0.9, 0.2).expect("room");
            }
            let _ = runtime.apply_for_test(SequencerState {
                pattern: Pattern::empty(),
                locks,
                editing: None,
                held: HeldParams::EMPTY,
                tempo: 120.0,
                bar: None,
                transport: Transport::Stopped,
                generation: 1,
                serial: u64::from(round) + 1,
            });
        }

        assert_eq!(
            runtime.owed_zero_count(),
            1,
            "only the one parameter that was actually applied is owed a zero"
        );
    }

    // --- parameter locks ---------------------------------------------------------------------------

    fn locked(pattern: Pattern, locks: crate::sequencer::locks::LockSet) -> Runtime {
        let mut runtime = Runtime::new(SR);
        let released = runtime.apply_for_test(SequencerState {
            pattern: pattern.clone(),
            locks,
            editing: None,
            held: HeldParams::EMPTY,
            tempo: 120.0,
            bar: None,
            transport: Transport::Playing,
            generation: 1,
            serial: 1,
        });
        assert!(released.is_empty());
        runtime
    }

    #[test]
    fn a_locked_parameter_is_set_before_the_note_that_uses_it() {
        // **The whole point of the feature, and it is one line's worth of ordering.**
        // `FixedEventBuffer` exposes events in push order, so a note sounded before its cutoff
        // arrives is a note played on the previous step's sound.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        locks.set(0, 42, 0.25, PATCH).expect("room");

        let mut runtime = locked(pattern, locks);
        let actions = runtime.advance(512);

        assert!(
            matches!(
                actions[0],
                Action::SetParam {
                    key: crate::sequencer::locks::LockKey {
                        fx: 0,
                        param_id: 42
                    },
                    ..
                }
            ),
            "the parameter must come first: {actions:?}"
        );
        assert!(
            matches!(actions[1], Action::Sound { .. }),
            "and the note after it: {actions:?}"
        );
        assert_eq!(actions[0].frame(), actions[1].frame(), "at the same frame");
    }

    #[test]
    fn only_the_step_that_locks_a_parameter_hears_its_value() {
        // A lock is a deviation from the patch, and this is what that means once the patch is
        // restored on the steps around it: **one** step carries the locked value and the others
        // carry the patch. An earlier version of this test asserted that the other steps emitted
        // *nothing*, which is what left the parameter wherever the lock had put it — the sequencer
        // then did nothing at all, and the test agreed with it.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.toggle(1, 62);
        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        locks.set(0, 42, 0.25, 0.7).expect("room");

        let mut runtime = locked(pattern, locks);
        let log = run(&mut runtime, 40, 512);

        let values: Vec<f32> = log
            .iter()
            .filter_map(|(_, a)| match a {
                Action::SetParam { value, .. } => Some(*value),
                _ => None,
            })
            .collect();
        assert!(
            values.len() > 1,
            "every step it is locked in sets it, not only the locked one: {values:?}"
        );
        // 0.25 from a patch of 0.7 is an offset of -0.45 on one step, and zero everywhere else.
        assert_eq!(
            values.iter().filter(|v| (*v + 0.45).abs() < 1e-6).count(),
            1,
            "one step moves it: {values:?}"
        );
        assert!(
            values.iter().skip(1).all(|v| *v == 0.0),
            "and the rest move it by nothing, which is what puts it back: {values:?}"
        );
    }

    #[test]
    fn a_lock_fires_on_a_step_that_holds_no_notes() {
        // Locks and notes are independent: a step can set a filter without playing anything, which
        // is how a pattern shapes the note still ringing from the step before.
        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        locks.set(2, 7, 0.9, PATCH).expect("room");

        let mut runtime = locked(Pattern::empty(), locks);
        let log = run(&mut runtime, 60, 512);

        assert!(
            log.iter().any(|(_, a)| matches!(
                a,
                Action::SetParam {
                    key: crate::sequencer::locks::LockKey { fx: 0, param_id: 7 },
                    value,
                    ..
                } if (*value - 0.4).abs() < 1e-6
            )),
            "a rest still moves what it locks - by 0.9 from a patch of 0.5: {log:?}"
        );
    }

    #[test]
    fn every_locked_parameter_on_a_step_is_set() {
        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        for id in 0..5u32 {
            locks.set(0, id, id as f32 / 10.0, PATCH).expect("room");
        }

        let mut runtime = locked(Pattern::empty(), locks);
        let actions = runtime.advance(512);

        let mut seen: Vec<crate::sequencer::locks::LockKey> = actions
            .iter()
            .filter_map(|a| match a {
                Action::SetParam { key, .. } => Some(*key),
                _ => None,
            })
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn locks_come_round_again_with_the_pattern() {
        // The pattern repeats, and so do its locks — otherwise a bar would sound different the
        // second time through for no reason anybody could see.
        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        // A patch value **different from the lock**, so that counting the lock does not
        // accidentally count the fifteen steps that restore the patch.
        locks.set(0, 3, 0.5, 0.1).expect("room");

        let mut runtime = locked(Pattern::empty(), locks);
        // A bar at 120 BPM is 96,000 frames, and a run **starts on** step 1 — so two full bars
        // contain three arrivals at it, at 0, 96,000 and 192,000. 400 blocks of 512 is 204,800.
        let log = run(&mut runtime, 400, 512);

        // Counted by the **locked** value rather than by every event, because the fifteen steps
        // that put the parameter back also set it.
        let fired: Vec<u64> = log
            .iter()
            .filter(|(_, a)| {
                matches!(
                    a,
                    Action::SetParam {
                        key: crate::sequencer::locks::LockKey { fx: 0, param_id: 3 },
                        value,
                        ..
                    } if (*value - 0.4).abs() < 1e-6
                )
            })
            .map(|(at, _)| *at)
            .collect();
        assert_eq!(
            fired.len(),
            3,
            "once each time step 1 comes round: {fired:?}"
        );
        // Within a sample of each bar line: the clock floors its frame offset inside the chunk, so
        // a boundary can land one sample early. That is the existing behaviour every note already
        // has, and asserting exact equality here would be pinning a rounding rule this test is not
        // about.
        for (fired, expected) in fired.iter().zip([0u64, 96_000, 192_000]) {
            assert!(fired.abs_diff(expected) <= 1, "{fired} against {expected}");
        }
    }

    #[test]
    fn the_action_vector_never_grows_while_playing() {
        // A `push` past capacity would allocate **inside the audio callback**. The vector is sized
        // for the guard — every boundary a chunk can hold, times every lockable parameter — and this
        // is what says the sizing and the emission agree.
        use crate::sequencer::locks::MAX_LOCKED_PARAMS;

        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        for id in 0..MAX_LOCKED_PARAMS as u32 {
            for step in 0..16 {
                locks.set(step, id, 0.5, PATCH).expect("room");
            }
        }
        let mut pattern = Pattern::empty();
        for step in 0..16 {
            pattern.toggle(step, 60);
        }

        // A very fast tempo and a large chunk, so one call crosses many boundaries.
        let mut runtime = Runtime::new(SR);
        let released = runtime.apply_for_test(SequencerState {
            pattern: pattern.clone(),
            locks,
            editing: None,
            held: HeldParams::EMPTY,
            tempo: 300.0,
            bar: None,
            transport: Transport::Playing,
            generation: 1,
            serial: 1,
        });
        assert!(released.is_empty());

        let capacity = runtime.actions.capacity();
        for _ in 0..20 {
            runtime.advance(8192);
            assert_eq!(
                runtime.actions.capacity(),
                capacity,
                "the action vector reallocated, which is an allocation on the audio thread"
            );
        }
    }

    #[test]
    fn a_steps_locks_are_its_own_tied_or_not() {
        // **Locks are per step — the owner's ruling.** A head's lock does not carry through its
        // run: a hold that wants the offset keeps its own lock, and a hold that sets one moves the
        // parameter under the held note, which is the point — modulation can walk while the gate
        // stays open. A step with no lock returns the parameter to the patch, exactly as every
        // unlocked step always has.
        let mut pattern = Pattern::empty();
        pattern.toggle(4, 60);
        for step in [5, 6, 7] {
            pattern.toggle_tied(step);
        }

        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        locks.set(4, 42, 0.9, 0.2).expect("room");
        // A hold carrying its own lock: the filter moves mid-note.
        locks.set(6, 42, 0.4, 0.2).expect("room");

        let mut runtime = locked(pattern, locks);
        let log = run(&mut runtime, 188, 512);

        let values: Vec<(usize, f32)> = log
            .iter()
            .filter_map(|(_, a)| match a {
                Action::SetParam {
                    key:
                        crate::sequencer::locks::LockKey {
                            fx: 0,
                            param_id: 42,
                        },
                    value,
                    ..
                } => Some(*value),
                _ => None,
            })
            .take(crate::sequencer::pattern::STEPS)
            .enumerate()
            .collect();

        assert_eq!(values.len(), 16, "every step still sets it: {values:?}");
        for (step, value) in values {
            // Step 5 offsets by its own lock; the hold at step 7 by its own; every other step —
            // the holds that lock nothing included — is the patch.
            let expected: f32 = match step {
                4 => 0.7,
                6 => 0.2,
                _ => 0.0,
            };
            assert!(
                (value - expected).abs() < 1e-6,
                "step {} must offset by {expected}, not {value}",
                step + 1
            );
        }
    }

    #[test]
    fn a_run_crossing_the_loop_point_re_reads_its_locks_at_step_one() {
        // **A deliberate cost, pinned so it cannot drift into being a surprise.** Ties wrap, so a
        // note on step 16 with step 1 tied is still sounding when step 1 arrives — but locks do not
        // wrap, because an export renders the bar once and has no previous step 16 to continue.
        // Making playback wrap would guarantee live and rendered disagreed at exactly this point,
        // and an export sounding unlike what you heard is not acceptable.
        //
        // So step 1 reads its own locks and sets none: the parameter returns to the patch under a
        // note that has not ended. Identically live, rendered and in the file.
        let mut pattern = Pattern::empty();
        pattern.toggle(15, 60);
        pattern.set_tied(0, true);

        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        locks.set(15, 42, 0.9, 0.2).expect("room");

        let mut runtime = locked(pattern, locks);
        let log = run(&mut runtime, 188, 512);

        let values: Vec<(usize, f32)> = log
            .iter()
            .filter_map(|(_, a)| match a {
                Action::SetParam {
                    key:
                        crate::sequencer::locks::LockKey {
                            fx: 0,
                            param_id: 42,
                        },
                    value,
                    ..
                } => Some(*value),
                _ => None,
            })
            .take(crate::sequencer::pattern::STEPS)
            .enumerate()
            .collect();

        assert!(
            values[0].1.abs() < 1e-6,
            "step 1 begins the bar again and sets nothing, so it offsets by zero, not {}",
            values[0].1
        );
        assert!(
            (values[15].1 - 0.7).abs() < 1e-6,
            "step 16 still sets its own, not {}",
            values[15].1
        );
    }

    #[test]
    fn a_twelve_step_pattern_loops_after_twelve_steps() {
        // **The whole of step 1, end to end.** A step stays a sixteenth and the pattern says how
        // many it holds, so twelve is a 3/4 bar — it must come round at step 12, not step 16. The
        // clock is what wraps, and it takes its length from the pattern; the two disagreeing would
        // put the playhead on a step the pattern does not have, which is the wrong bar rather than
        // a wrong note.
        let mut pattern = Pattern::empty();
        pattern.set_steps_per_bar(12);
        pattern.toggle(0, 60);

        let mut runtime = locked(pattern, crate::sequencer::locks::LockSet::EMPTY);
        // 120 BPM at 48 kHz is 6,000 frames a sixteenth, so twelve steps is 72,000 — and two full
        // times round is 144,000, which 282 blocks of 512 (144,384) just covers.
        let log = run(&mut runtime, 282, 512);

        let starts: Vec<u64> = log
            .iter()
            .filter_map(|(frame, a)| match a {
                Action::Sound { .. } => Some(*frame),
                _ => None,
            })
            .collect();

        assert_eq!(
            starts.len(),
            3,
            "three passes in this many blocks: {starts:?}"
        );
        // **Spacing, within a frame.** The exact landing point drifts by one frame over this many
        // blocks — `Clock` accumulates a fractional position and floors the boundary into a frame —
        // and that is true at any length, so pinning 144_000 would be testing the rounding rather
        // than the loop. Twelve steps at 120 BPM and 48 kHz is 72,000 frames.
        for pair in starts.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap.abs_diff(72_000) <= 1,
                "twelve steps is 72,000 frames, not {gap} — a sixteen-step loop would be 96,000"
            );
        }
    }

    #[test]
    fn a_step_that_does_not_set_a_locked_parameter_puts_it_back() {
        // **The defect that made the whole feature do nothing.** A parameter is one value on the
        // plugin: setting it at step 3 leaves it there for steps 4, 5 and 6 as well. Emitting only
        // the steps that lock something means the first turn of a knob moves the sound for the rest
        // of the bar and every step agrees with every other - which is what "it does not sequence at
        // all" looks like from the outside.
        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        locks.set(2, 42, 0.9, 0.2).expect("room");

        // A whole bar: 16 steps of 6,000 frames at 120 BPM is 96,000, which is 188 blocks of 512 —
        // and 188 blocks is 96,256, so the run reaches step 1 of the next bar. `take(STEPS)` is what
        // makes this about one bar rather than about where the block boundary happens to fall.
        let mut runtime = locked(Pattern::empty(), locks);
        let log = run(&mut runtime, 188, 512);

        let values: Vec<(usize, f32)> = log
            .iter()
            .filter_map(|(_, a)| match a {
                Action::SetParam {
                    key:
                        crate::sequencer::locks::LockKey {
                            fx: 0,
                            param_id: 42,
                        },
                    value,
                    ..
                } => Some(*value),
                _ => None,
            })
            .take(crate::sequencer::pattern::STEPS)
            .enumerate()
            .collect();

        assert_eq!(values.len(), 16, "every step sets it: {values:?}");
        // **Offsets from the patch, not values.** Step 3 moves the parameter by 0.7; every other
        // step moves it by nothing, which is what puts it back.
        for (step, value) in values {
            let expected: f32 = if step == 2 { 0.7 } else { 0.0 };
            assert!(
                (value - expected).abs() < 1e-6,
                "step {} must offset by {expected}, not {value}",
                step + 1
            );
        }
    }

    #[test]
    fn a_parameter_nothing_locks_is_never_touched() {
        // The other half of the same rule. Restoring the patch on every step for *every* parameter
        // would be automation nobody asked for, and would fight any knob being turned live.
        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        locks.set(0, 42, 0.9, 0.2).expect("room");

        let mut runtime = locked(Pattern::empty(), locks);
        let log = run(&mut runtime, 64, 512);

        assert!(
            log.iter().all(|(_, a)| !matches!(
                a,
                Action::SetParam {
                    key: crate::sequencer::locks::LockKey { fx: 0, param_id: 7 },
                    ..
                }
            )),
            "only what somebody sequenced is sequenced: {log:?}"
        );
    }

    #[test]
    fn moving_the_patch_moves_what_the_unlocked_steps_restore() {
        // Turning a knob with no step selected edits the sequence-patch. If the steps that do not
        // set that parameter went on restoring the old value, the sequencer would fight the knob -
        // you would turn it and hear it snap back at the next step.
        let mut locks = crate::sequencer::locks::LockSet::EMPTY;
        locks.set(2, 42, 0.9, 0.2).expect("room");
        locks.set_patch(42, 0.6);

        let mut runtime = locked(Pattern::empty(), locks);
        let log = run(&mut runtime, 64, 512);

        // Step 1 sets nothing, so it offsets by nothing — and *what that zero means* is the new
        // patch, because the parameter it is laid over was never moved off it.
        let first = log
            .iter()
            .find_map(|(_, a)| match a {
                Action::SetParam {
                    key:
                        crate::sequencer::locks::LockKey {
                            fx: 0,
                            param_id: 42,
                        },
                    value,
                    ..
                } => Some(*value),
                _ => None,
            })
            .expect("step 1 is visited");
        assert_eq!(first, 0.0, "an unlocked step moves nothing");

        // And the locked step's offset is measured from the **new** patch: 0.9 from 0.6 is 0.3,
        // where it was 0.7 before the patch moved. That is the sequencer following the knob.
        let locked = log
            .iter()
            .find_map(|(_, a)| match a {
                Action::SetParam {
                    key:
                        crate::sequencer::locks::LockKey {
                            fx: 0,
                            param_id: 42,
                        },
                    value,
                    ..
                } if *value != 0.0 => Some(*value),
                _ => None,
            })
            .expect("step 3 moves it");
        assert!(
            (locked - 0.3).abs() < 1e-6,
            "measured from the patch that is now in force: {locked}"
        );
    }

    // --- ties -----------------------------------------------------------------------------------

    const STEP_FRAMES: u32 = 6000; // a sixteenth at 120 BPM, 48 kHz

    /// Every action a run produces, with an absolute frame, so durations can be measured.
    fn run(runtime: &mut Runtime, blocks: u32, block: u32) -> Vec<(u64, Action)> {
        let mut out = Vec::new();
        for index in 0..blocks {
            let base = u64::from(index) * u64::from(block);
            for action in runtime.advance(block) {
                out.push((base + u64::from(action.frame()), *action));
            }
        }
        out
    }

    /// The length of the first note, in steps. The measurement the whole design turns on.
    fn first_note_length_in_steps(log: &[(u64, Action)]) -> f64 {
        let on = log
            .iter()
            .find(|(_, a)| matches!(a, Action::Sound { .. }))
            .expect("a note must have sounded");
        let off = log
            .iter()
            .find(|(at, a)| *at > on.0 && matches!(a, Action::Release { .. }))
            .expect("it must have been released");
        (off.0 - on.0) as f64 / f64::from(STEP_FRAMES)
    }

    #[test]
    fn a_note_with_no_tie_after_it_still_lasts_half_a_step() {
        // The property ties must not disturb: an ordinary note keeps the 50% gate exactly as it
        // had, because both suppression clauses are false.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        let mut runtime = playing(pattern.clone(), 120.0);

        let log = run(&mut runtime, 40, 512);
        let length = first_note_length_in_steps(&log);
        assert!(
            (length - 0.5).abs() < 0.02,
            "expected half a step: {length}"
        );
    }

    #[test]
    fn a_note_followed_by_k_empty_ties_lasts_exactly_k_plus_one_steps() {
        // The arithmetic revision 2 got wrong, which is why this asserts the number rather than
        // the shape. With only the "the next step is a tie" clause, each tied step still closed
        // its own gate halfway through itself and the durations came out in halves.
        for k in 1..=3usize {
            let mut pattern = Pattern::empty();
            pattern.toggle(0, 60);
            for step in 1..=k {
                pattern.set_tied(step, true);
            }
            let mut runtime = playing(pattern.clone(), 120.0);

            let log = run(&mut runtime, 120, 512);
            let length = first_note_length_in_steps(&log);
            assert!(
                (length - (k as f64 + 1.0)).abs() < 0.02,
                "a note plus {k} ties should last {} steps, measured {length}",
                k + 1
            );
        }
    }

    #[test]
    fn a_tie_carrying_notes_starts_a_new_run_rather_than_extending_the_old_one() {
        // The load-bearing word in "a note followed by k empty ties". A tie with notes of its own
        // is legato -- no gap -- but the note that was sounding ends there.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true);
        pattern.toggle(1, 67); // the tie carries its own note
        let mut runtime = playing(pattern.clone(), 120.0);

        let log = run(&mut runtime, 120, 512);
        let length = first_note_length_in_steps(&log);
        assert!(
            (length - 1.0).abs() < 0.02,
            "C3 should end where G3 begins, one step in, not be extended by it: {length}"
        );

        let sounds = log
            .iter()
            .filter(|(_, a)| matches!(a, Action::Sound { .. }))
            .count();
        assert!(sounds >= 2, "both notes must sound, counted {sounds}");
    }

    #[test]
    fn a_run_of_ties_sounds_nothing_until_a_note_lands_in_it_and_is_legato_after() {
        // **The contract the editor's guards used to hold, now held here** — the owner's ruling
        // that ties and notes are independent (`apps/mxm-player/AGENTS.md`, *Ties and notes are
        // independent*). Nothing refuses a tie over a rest any more, so what such a pattern
        // *means* has to be pinned where it is decided: the runtime.
        //
        // Steps 1-4 all tied, a note on step 3 alone. Nothing sounds for the two ties in front of
        // it — there is no note to continue — the note sounds when it arrives, and the tie behind
        // it holds the gate open rather than closing it at the 50% mark.
        let mut pattern = Pattern::empty();
        for step in 0..4 {
            pattern.set_tied(step, true);
        }
        pattern.toggle(2, 64);
        let mut runtime = playing(pattern, 120.0);

        let log = run(&mut runtime, 120, 512);
        let note_at = u64::from(STEP_FRAMES) * 2;

        assert!(
            !log.iter()
                .any(|(at, a)| *at < note_at && matches!(a, Action::Sound { .. })),
            "a tie with no note in front of it holds nothing, so it sounds nothing: {log:?}"
        );
        assert!(
            log.iter().any(|(at, a)| *at == note_at
                && matches!(a, Action::Sound { notes, .. } if notes.contains(64))),
            "the sound starts at the note, wherever in the run it sits: {log:?}"
        );
        let gate_close = note_at + u64::from(STEP_FRAMES) / 2;
        assert!(
            !log.iter().any(|(at, a)| *at <= gate_close + 8
                && *at >= gate_close.saturating_sub(8)
                && matches!(a, Action::Release { notes, .. } if notes.contains(64))),
            "and the tie after it keeps the gate open past the half step: {log:?}"
        );
    }

    #[test]
    fn a_slides_own_note_gates_like_an_ordinary_note() {
        // §8.1, decided by the owner's file-true-to-the-interface ruling: a slide starts a new
        // note, so its own gate is the ordinary half step — it does not inherit the hold's
        // fill-the-step behaviour the old fourth state had.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true);
        pattern.toggle(1, 67); // a slide, with nothing tied after it
        let mut runtime = playing(pattern.clone(), 120.0);

        let log = run(&mut runtime, 120, 512);
        // The slide's note sounds at the joint and must release half a step later.
        let joint = u64::from(STEP_FRAMES);
        let release = log
            .iter()
            .find(|(at, a)| {
                *at > joint && matches!(a, Action::Release { notes, .. } if notes.contains(67))
            })
            .expect("the slide's note must end");
        let length = (release.0 - joint) as f64 / f64::from(STEP_FRAMES);
        assert!(
            (length - 0.5).abs() < 0.02,
            "a slide's own note lasts half a step, measured {length}"
        );
    }

    #[test]
    fn a_slide_held_onward_by_a_tie_fills_its_steps() {
        // The other half of the slide's gate rule: tied onward, it holds like any note would.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true);
        pattern.toggle(1, 67); // a slide...
        pattern.set_tied(2, true); // ...held through step 3
        let mut runtime = playing(pattern.clone(), 120.0);

        let log = run(&mut runtime, 120, 512);
        let joint = u64::from(STEP_FRAMES);
        let release = log
            .iter()
            .find(|(at, a)| {
                *at > joint && matches!(a, Action::Release { notes, .. } if notes.contains(67))
            })
            .expect("the slide's note must end");
        let length = (release.0 - joint) as f64 / f64::from(STEP_FRAMES);
        assert!(
            (length - 2.0).abs() < 0.02,
            "a slide plus one hold is two steps, measured {length}"
        );
    }

    #[test]
    fn a_tie_with_notes_sounds_before_it_releases_so_the_joint_is_legato() {
        // Section 3. Two events at the same frame, and the order is the whole difference between
        // legato and a retrigger: the plugin has to see the new note arrive while the old one is
        // still held.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true);
        pattern.toggle(1, 67);
        let mut runtime = playing(pattern.clone(), 120.0);

        let log = run(&mut runtime, 120, 512);
        let joint = u64::from(STEP_FRAMES);
        let at_joint: Vec<Action> = log
            .iter()
            .filter(|(at, _)| at.abs_diff(joint) < 16)
            .map(|(_, a)| *a)
            .collect();

        assert_eq!(
            at_joint.len(),
            2,
            "expected a sound and a release at the joint: {at_joint:?}"
        );
        assert!(
            matches!(at_joint[0], Action::Sound { .. }),
            "the new note must be emitted first: {at_joint:?}"
        );
        assert!(matches!(at_joint[1], Action::Release { .. }));
    }

    #[test]
    fn a_tie_followed_by_a_rest_releases_at_the_rest_step_start() {
        // Not halfway through the tie. A tied note always ends on a step boundary, which is what
        // makes every run a whole number of steps.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true);
        // step 3 is an untied rest, and that is where the run ends
        let mut runtime = playing(pattern.clone(), 120.0);

        let log = run(&mut runtime, 120, 512);
        let release = log
            .iter()
            .find(|(_, a)| matches!(a, Action::Release { .. }))
            .expect("it must end");
        assert_eq!(
            release.0,
            u64::from(STEP_FRAMES) * 2,
            "the run must end at the third step start, not at the second step gate"
        );
    }

    #[test]
    fn a_tie_on_the_first_step_holds_the_last_step_note_across_the_loop_point() {
        // The lookahead wraps, and live that is correct: the pattern repeats. An export plays it
        // once and must not, which is why `sequencer::export` forces a release at the bar end.
        let mut pattern = Pattern::empty();
        pattern.toggle(15, 60);
        pattern.set_tied(0, true);
        let mut runtime = playing(pattern.clone(), 120.0);

        let log = run(&mut runtime, 400, 512);
        let on = log
            .iter()
            .find(|(_, a)| matches!(a, Action::Sound { .. }))
            .expect("the last step must sound");
        let off = log
            .iter()
            .find(|(at, a)| *at > on.0 && matches!(a, Action::Release { .. }))
            .expect("and must end");
        let length = (off.0 - on.0) as f64 / f64::from(STEP_FRAMES);
        assert!(
            (length - 2.0).abs() < 0.02,
            "step 16 plus a tie on step 1 is two steps across the loop point, measured {length}"
        );
    }

    #[test]
    fn an_empty_tie_with_nothing_before_it_is_a_no_op_rather_than_a_fault() {
        // What a half-built pattern looks like while somebody is building it.
        let mut pattern = Pattern::empty();
        pattern.set_tied(3, true);
        let mut runtime = playing(pattern.clone(), 120.0);
        assert!(
            run(&mut runtime, 120, 512).is_empty(),
            "silence, not a panic"
        );
    }

    #[test]
    fn stopping_during_a_tie_hands_back_the_note_it_is_holding() {
        // A tie makes a note live longer, which widens every window a stop can land in. A tie
        // that outlives a stop is a stuck note.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        for step in 1..8 {
            pattern.set_tied(step, true);
        }
        let mut runtime = playing(pattern.clone(), 120.0);
        runtime.advance(STEP_FRAMES * 3);
        assert!(!runtime.sounding().is_empty(), "mid-run, holding a note");

        let released = runtime.apply_for_test(SequencerState {
            locks: crate::sequencer::locks::LockSet::EMPTY,
            pattern: pattern.clone(),
            editing: None,
            held: HeldParams::EMPTY,
            tempo: 120.0,
            bar: None,
            transport: Transport::Stopped,
            generation: 1,
            serial: 2,
        });
        assert!(released.contains(60), "stop must hand back the tied note");
        assert!(runtime.sounding().is_empty());
    }

    #[test]
    fn a_chord_step_sounds_every_note_together() {
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.toggle(0, 64);
        pattern.toggle(0, 67);
        let mut runtime = playing(pattern.clone(), 120.0);

        let actions = runtime.advance(512);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].notes().expect("a chord sounds notes").count(), 3);
    }
}
