//! Per-step parameter values: what a step sets, beyond which notes it plays.
//!
//! # A lock is a deviation from the patch
//!
//! You start from a patch — one you made or loaded — and turning a knob with a step selected records
//! *that knob, at that step*. Everything untouched plays the patch. So the storage is sparse by
//! nature rather than by compromise: most patterns move two or three knobs, and this says so.
//!
//! **Which is why every entry carries the patch value it deviates from.** A parameter is a single
//! value on the plugin; setting it at step 5 leaves it there for steps 6, 7 and 8 as well, so a
//! design that emitted only the locked steps would not sequence at all — the first turn of the knob
//! would move the sound for the whole bar and every step would agree with every other. The parameter
//! has to be *put back* on the steps that do not set it, and only this set knows what "back" is.
//!
//! # A lock is sent as **modulation**, and that is what makes it a deviation
//!
//! The sequencer emits `CLAP_EVENT_PARAM_MOD`, not `PARAM_VALUE`. CLAP modulation is an offset laid
//! *over* a parameter without disturbing it — nice-plug applies it as
//! `(unmodulated + offset).clamp(0, 1)` and keeps the two readable separately — so the parameter's
//! own value stays the patch for as long as the sequence runs.
//!
//! Three things follow, and together they are the reason for the change:
//!
//! - **The patch cannot be eroded by the sequencer**, because nothing the sequencer sends is a value.
//!   Every defect in this feature so far has been some version of the step's value becoming the
//!   patch; that is now structurally impossible rather than prevented by care.
//! - **Leaving a step needs no restore.** An unlocked step sends offset zero, which *is* the patch.
//! - **The instrument can see what a step deviates by**, because it holds both numbers — which is
//!   what lets mxm-mono-01's own editor mark a sequenced knob with no protocol between us at all.
//!
//! So a locked parameter is sent on **every** step: its own offset where the step sets it, and zero
//! where it does not. `patch` is what the offsets are measured from — captured the first time a
//! parameter is locked and updated whenever the knob moves with no step selected, which is what
//! editing the patch *is*.
//!
//! # Why sparse is forced, and why the cap is bytes
//!
//! `SequencerState` is `Copy` and published to the audio thread **by value** — no allocation, no
//! `Arc` to drop in a realtime callback. A full snapshot per step would be 16 × *n* values where *n*
//! belongs to whatever plugin is loaded, and the player loads plugins it has never heard of. **There
//! is no *n* to size for.**
//!
//! So a pattern carries **the locks somebody actually set** — a `(step, parameter, value)` each —
//! and, separately, the parameters they belong to with the baseline each deviates from.
//!
//! **Two budgets, because two different things are being spent.** [`MAX_LOCKS`] bounds the total,
//! which is what the bytes cost. [`MAX_LOCKED_PARAMS`] bounds the distinct parameters, which is what
//! the *realtime* cost is: every locked parameter emits an action at every boundary — that is the
//! put-back rule above — so it is the number `MAX_ACTIONS_PER_CHUNK` has to accommodate. Sparse
//! storage does not shrink that one, and conflating the two is what the previous form did.
//!
//! **The previous form gave every locked parameter a value per step**, so cost was
//! `parameters × steps` and nearly all of it was empty: three knobs used 216 of 2,312 bytes, and
//! every bar added would have multiplied the waste. This costs more at sixteen steps and less from
//! about forty on, which is the trade — **cost follows what was automated, not what could be.**
//!
//! `MAX_LOCKED_PARAMS` is still 32. What changed is why: it was 32 because mxm-mono-01 has 27
//! parameters — a plugin fact sizing a host structure, the same error as the earlier cap of eight
//! *"because a controller has eight knobs"* — and it is now the number the realtime action budget
//! affords. **Nothing here is sized from a number belonging to an instrument.** Raise it only by
//! widening that budget.
//!
//! # NaN means "not locked"
//!
//! A normalised parameter value is finite by definition — `plugins/mxm-mono-01/src/preset.rs` in
//! mxm-mono-01 already refuses non-finite values on the way in — so NaN cannot collide with a real
//! one. It costs nothing, removes a mask that could disagree with the values beside it, and makes
//! clearing a lock a single store.

/// Older files have sixteen cells. New captures extend through the last authored lock;
/// trailing empty cells need not be stored because the sequence carries its own shape.
use super::pattern::STEPS;
use crate::engine::fx::{FxId, NO_FX_ID};

/// **Which plugin a locked parameter belongs to, and which parameter.**
///
/// # A parameter id alone is not an identity, and the reason is a hash
///
/// `param_id` is nice-plug's hash of a parameter's string id — `h = h*31 + byte`, pinned in
/// `control_map::schema::hash_param_id`. It identifies a parameter *within one plugin* and nothing
/// more: two plugins can hash two different parameters to the same `u32`, and nothing in this
/// player would notice.
///
/// While the sequencer could only automate the source that was harmless, because there was one
/// plugin a lock could possibly mean. The moment an effect can be automated it stops being
/// harmless: a lock left behind by a removed effect would not sit inert, it would be **delivered to
/// whatever else hashes the same**, silently. So the target is part of the key rather than a table
/// beside it — after the effect is gone, "which effect did this belong to" is not recoverable from
/// anything else.
///
/// # The source is the zero, deliberately
///
/// `fx` is [`NO_FX_ID`] for the source, which is what makes three things fall out at once: every
/// existing call site that passes a bare `u32` still means the source (`impl From<u32>`), every
/// pattern already saved reads correctly with no target recorded, and every `mxm-cli lock` script
/// keeps working.
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct LockKey {
    /// The effect this parameter belongs to, or [`NO_FX_ID`] for the source.
    ///
    /// **An id, never a position.** An effect's index changes when an earlier one is removed or the
    /// chain is reordered; its id does not. See [`FxId`].
    pub fx: FxId,
    pub param_id: u32,
}

impl LockKey {
    pub const fn source(param_id: u32) -> Self {
        Self {
            fx: NO_FX_ID,
            param_id,
        }
    }

    pub const fn fx(fx: FxId, param_id: u32) -> Self {
        Self { fx, param_id }
    }

    pub const fn is_source(self) -> bool {
        self.fx == NO_FX_ID
    }
}

/// The source's parameters are the bare form, so nothing that predates effect automation changes.
impl From<u32> for LockKey {
    fn from(param_id: u32) -> Self {
        Self::source(param_id)
    }
}

/// **A source lock compares equal to its bare parameter id**, and an effect's compares equal to
/// nothing.
///
/// The same argument as [`From<u32>`]: the source is the case that existed before targets did, so
/// it keeps the shorter spelling everywhere — in a file, in a `mxm-cli` verb, in a caller and in an
/// assertion. What this must never do is let an *effect's* key match a bare id, which is the
/// silent misdelivery the target exists to prevent; `is_source` is the guard.
impl PartialEq<u32> for LockKey {
    fn eq(&self, param_id: &u32) -> bool {
        self.is_source() && self.param_id == *param_id
    }
}

impl PartialEq<LockKey> for u32 {
    fn eq(&self, key: &LockKey) -> bool {
        key == self
    }
}

/// How a key is written in a message and in a `mxm-cli` verb: a bare id for the source, `fx<id>:`
/// prefixed for an effect. The source keeps the short spelling everywhere it already had it.
impl std::fmt::Display for LockKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_source() {
            write!(f, "{}", self.param_id)
        } else {
            write!(f, "fx{}:{}", self.fx, self.param_id)
        }
    }
}

/// How many distinct parameters one pattern may lock.
///
/// **A realtime budget, and the only thing it is derived from is this player.** Every locked
/// parameter emits one action at *every* boundary — that is the put-back rule — so this is the
/// number `MAX_ACTIONS_PER_CHUNK` has to accommodate, alongside the owed zeroes and the preview a
/// chunk can carry at the same time. Sparse storage does not shrink it: the actions are per
/// parameter, not per lock.
///
/// **It used to be 32 because MXM-mono-01 has 27 parameters**, which was a plugin fact sizing a host
/// structure — the same mistake as the earlier cap of eight *"because a controller has eight
/// knobs"*. The number is unchanged and its justification is not. Raise it only by widening the
/// action budget it is drawn from, never to suit a particular instrument.
pub const MAX_LOCKED_PARAMS: usize = 32;

/// How many locks one pattern may hold in total, across every step.
///
/// **The memory budget, and the reason it is separate.** Bytes are spent per *lock*; realtime
/// actions are spent per *parameter*. Conflating them is what made the previous form cost
/// `parameters × steps` — 2,312 bytes whether three knobs moved or five hundred, and multiplying
/// with every bar added.
///
/// **A flat budget, deliberately not `parameters × steps`.** It was that briefly, while a pattern
/// was sixteen steps and matching the dense form's capability exactly was free. It cannot stay
/// that: at `MAX_STEPS` the same formula is 16,384 locks — 196 KB — for a capability nobody wants,
/// since automating thirty-two parameters on every one of five hundred steps is not a thing anyone
/// does by hand or by machine.
///
/// 1,024 locks is 12 KB at **any** length, which is the property the sparse form exists for. It is
/// generous: thirty-two parameters across a whole thirty-two-step bar is 1,024 exactly, and a
/// sixty-four-bar line moving four knobs a bar is 256.
///
/// This is what makes [`Refused::NoRoom`] reachable — it was not while the two budgets coincided.
pub const MAX_LOCKS: usize = 1024;

// **`Refused::NoRoom` is reachable at any sequence long enough**, and there is no longer a capacity
// to compare against: a pattern has no maximum length. The relationship that mattered — the lock
// budget binding before the parameter budget — now holds for every sequence past
// `MAX_LOCKS / MAX_LOCKED_PARAMS` steps, which is thirty-two.

/// A parameter something locks, and what the steps that do not lock it put it back to.
#[derive(Copy, Clone, Debug)]
struct Param {
    /// Which plugin's parameter, and which one. See [`LockKey`].
    key: LockKey,
    /// What the steps that do **not** set this parameter put it back to. See the module docs.
    patch: f32,
}

impl Param {
    const fn empty() -> Self {
        Self {
            key: LockKey::source(0),
            patch: f32::NAN,
        }
    }

    /// Whether this parameter knows what its offsets are measured from.
    ///
    /// A file written before baselines were stored has none, and every offset would then be zero —
    /// the locks would survive the load and do nothing, which is worse than losing them, because
    /// nothing says so. The player adopts a baseline for these; see `adopt_missing_baselines`.
    fn has_patch(&self) -> bool {
        self.patch.is_finite()
    }
}

/// One step setting one parameter.
///
/// **Twelve bytes, and none of them scales with the sequence's length** — which is the whole point
/// of storing locks this way. The previous form gave every locked parameter a value per step, so
/// cost was `parameters × steps` and most of it was empty: three knobs used 216 of 2,312 bytes, and
/// a longer sequence multiplied the waste.
#[derive(Copy, Clone, Debug)]
struct Lock {
    step: u32,

    key: LockKey,
    value: f32,
}

impl Lock {
    const fn empty() -> Self {
        Self {
            step: 0,
            key: LockKey::source(0),
            value: f32::NAN,
        }
    }
}

/// Every locked parameter in one pattern.
///
/// `Copy` and heap-free, so it can be published to the audio thread beside the pattern.
#[derive(Copy, Clone, Debug)]
pub struct LockSet {
    /// The distinct parameters something locks, and their baselines.
    params: [Param; MAX_LOCKED_PARAMS],
    param_count: usize,
    /// Every `(step, parameter, value)` anybody has set. Order is arrival order.
    locks: [Lock; MAX_LOCKS],
    lock_count: usize,
}

impl Default for LockSet {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Why a lock could not be written.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Already locking [`MAX_LOCKED_PARAMS`] different parameters.
    Full,
    /// Already holding [`MAX_LOCKS`] locks in total.
    ///
    /// **Not reachable while a sequence is at most `STEPS` long** — the two budgets coincide there,
    /// so [`Refused::Full`] always arrives first. It becomes reachable when length does; see
    /// `the_lock_budget_cannot_bite_before_the_parameter_budget_at_this_capacity`.
    NoRoom,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Full => write!(
                f,
                "this pattern already locks {MAX_LOCKED_PARAMS} different parameters, which is all \
                 it can hold; clear one to lock another"
            ),
            Refused::NoRoom => write!(
                f,
                "this pattern already holds {MAX_LOCKS} locks, which is all it can hold; clear some \
                 to set another"
            ),
        }
    }
}

impl LockSet {
    pub const EMPTY: LockSet = LockSet {
        params: [Param::empty(); MAX_LOCKED_PARAMS],
        param_count: 0,
        locks: [Lock::empty(); MAX_LOCKS],
        lock_count: 0,
    };

    pub fn is_empty(&self) -> bool {
        self.param_count == 0
    }

    /// How many distinct parameters this pattern locks.
    pub fn len(&self) -> usize {
        self.param_count
    }

    /// How many locks it holds in total, across every step.
    pub fn locks(&self) -> usize {
        self.lock_count
    }

    fn find(&self, key: LockKey) -> Option<usize> {
        self.params[..self.param_count]
            .iter()
            .position(|p| p.key == key)
    }

    fn find_lock(&self, step: usize, key: LockKey) -> Option<usize> {
        self.locks[..self.lock_count]
            .iter()
            .position(|l| l.step as usize == step && l.key == key)
    }

    /// What `param_id` is set to at `step`, if that step sets it.
    pub fn get(&self, step: usize, key: impl Into<LockKey>) -> Option<f32> {
        self.find_lock(step, key.into())
            .map(|index| self.locks[index].value)
    }

    /// Records that `step` sets `param_id` to `value`.
    ///
    /// A non-finite `value` **clears** the lock rather than storing nonsense, because a value that
    /// came from a control is finite by definition and letting one in through the front door would
    /// make "set to nothing" and "not set" indistinguishable.
    ///
    /// **Two budgets, and they refuse separately.** [`MAX_LOCKED_PARAMS`] bounds the distinct
    /// parameters, because every one of them emits an action at every boundary and the realtime
    /// action budget is what that has to fit inside. [`MAX_LOCKS`] bounds the total, which is what
    /// the bytes actually cost. Neither is derived from any instrument's parameter count.
    pub fn set(
        &mut self,
        step: usize,
        key: impl Into<LockKey>,
        value: f32,
        patch: f32,
    ) -> Result<(), Refused> {
        let key = key.into();
        if !value.is_finite() {
            self.clear(step, key);
            return Ok(());
        }

        if let Some(index) = self.find_lock(step, key) {
            self.locks[index].value = value;
            return Ok(());
        }

        if self.find(key).is_none() {
            if self.param_count == MAX_LOCKED_PARAMS {
                return Err(Refused::Full);
            }
            // The baseline is taken **only when the parameter is first locked**. Every later edit is
            // a step's value, not a new patch; overwriting it here would make the patch chase the
            // last lock written, and the steps that restore it would restore the wrong thing.
            self.params[self.param_count] = Param { key, patch };
            self.param_count += 1;
        }

        if self.lock_count == MAX_LOCKS {
            // The parameter may have just been added for a lock that will not fit. Drop it again,
            // or the set would carry a parameter that locks nothing — which every step would then
            // pay an action for.
            self.release_unused_params();
            return Err(Refused::NoRoom);
        }
        self.locks[self.lock_count] = Lock {
            step: step as u32,
            key,
            value,
        };
        self.lock_count += 1;
        Ok(())
    }

    /// Moves what the unlocked steps put this parameter back to.
    ///
    /// This is what turning a knob with **no step selected** means: you are editing the patch, and
    /// the steps that do not override it must follow. Without this the sequencer would fight the
    /// knob — you would turn it and hear it snap back at the next step.
    ///
    /// Silent for a parameter nothing locks: there is no entry to hold the value, and none is
    /// needed, since an unlocked parameter is simply left where the plugin has it.
    pub fn set_patch(&mut self, key: impl Into<LockKey>, patch: f32) {
        if let Some(index) = self.find(key.into())
            && patch.is_finite()
        {
            self.params[index].patch = patch;
        }
    }

    /// What the unlocked steps put `param_id` back to.
    pub fn patch(&self, key: impl Into<LockKey>) -> Option<f32> {
        let patch = self.params[self.find(key.into())?].patch;
        patch.is_finite().then_some(patch)
    }

    /// Removes everything `step` sets.
    ///
    /// For the paths that delete a step's content outright — a bar clear, a resize dropping cells,
    /// an unlock. **Tying no longer calls this**: locks are per step, tied ones included, so a
    /// step keeps what it sets whatever its gate does.
    ///
    /// Returns whether anything was removed, so a caller can avoid republishing for nothing.
    pub fn clear_step(&mut self, step: usize) -> bool {
        let before = self.lock_count;
        self.retain_locks(|l| l.step as usize != step);
        let removed = self.lock_count != before;
        if removed {
            self.release_unused_params();
        }
        removed
    }

    /// Drops locks outside a resized pattern. The lock budget bounds the count, not the step
    /// addresses: one lock can sit arbitrarily far beyond the new end.
    pub fn truncate(&mut self, length: usize) -> usize {
        let before = self.lock_count;
        self.retain_locks(|lock| (lock.step as usize) < length);
        self.release_unused_params();
        before - self.lock_count
    }

    /// Removes one lock. A parameter with no locks left **releases its slot**, so clearing is what
    /// makes room rather than a separate operation somebody has to know about.
    pub fn clear(&mut self, step: usize, key: impl Into<LockKey>) {
        let Some(index) = self.find_lock(step, key.into()) else {
            return;
        };
        self.forget_lock(index);
        self.release_unused_params();
    }

    /// Removes every lock on one parameter.
    pub fn clear_param(&mut self, key: impl Into<LockKey>) {
        let key = key.into();
        self.retain_locks(|l| l.key != key);
        if let Some(index) = self.find(key) {
            self.forget_param(index);
        }
    }

    /// Removes every lock belonging to one effect, and the parameters and baselines with them.
    ///
    /// **This is what an effect's removal calls, and it runs before the chain changes**, so no lock
    /// naming an absent effect is ever published to the audio thread. Returns whether anything went,
    /// so a caller can avoid republishing for nothing.
    ///
    /// Nothing else may do this. A *reorder* must not — an id survives it, which is the whole point
    /// of ids — and a *bypass* must not, because bypass is not deletion.
    pub fn clear_fx(&mut self, fx: FxId) -> bool {
        if fx == NO_FX_ID {
            return false;
        }
        let before = self.lock_count;
        self.retain_locks(|l| l.key.fx != fx);
        let removed = self.lock_count != before;
        // Parameters go even where no lock did: an effect's baseline is as much its property as its
        // locks, and one left behind would be adopted by the next effect to reuse the id — which
        // `FxId` never does, but the set must not depend on that promise from outside.
        for index in (0..self.param_count).rev() {
            if self.params[index].key.fx == fx {
                self.forget_param(index);
            }
        }
        removed
    }

    /// Every effect this pattern automates, in ascending order. For the interface and for saving.
    pub fn automated_fx(&self) -> Vec<FxId> {
        let mut ids: Vec<FxId> = self.params[..self.param_count]
            .iter()
            .map(|p| p.key.fx)
            .filter(|fx| *fx != NO_FX_ID)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub fn clear_all(&mut self) {
        *self = Self::EMPTY;
    }

    /// Keeps the locks `keep` accepts, contiguously below `lock_count`.
    fn retain_locks(&mut self, keep: impl Fn(&Lock) -> bool) {
        let mut out = 0;
        for index in 0..self.lock_count {
            if keep(&self.locks[index]) {
                self.locks[out] = self.locks[index];
                out += 1;
            }
        }
        for slot in out..self.lock_count {
            self.locks[slot] = Lock::empty();
        }
        self.lock_count = out;
    }

    fn forget_lock(&mut self, index: usize) {
        for i in index..self.lock_count - 1 {
            self.locks[i] = self.locks[i + 1];
        }
        self.lock_count -= 1;
        self.locks[self.lock_count] = Lock::empty();
    }

    /// Removes a parameter, keeping the occupied ones contiguous below `param_count`.
    fn forget_param(&mut self, index: usize) {
        for i in index..self.param_count - 1 {
            self.params[i] = self.params[i + 1];
        }
        self.param_count -= 1;
        self.params[self.param_count] = Param::empty();
    }

    /// Drops parameters that no longer lock any step.
    ///
    /// **In reverse, because `forget_param` shifts everything after it down**; forward, each removal
    /// would move the next candidate under the index just examined.
    fn release_unused_params(&mut self) {
        for index in (0..self.param_count).rev() {
            let key = self.params[index].key;
            if !self.locks[..self.lock_count].iter().any(|l| l.key == key) {
                self.forget_param(index);
            }
        }
    }

    /// The `index`th locked parameter's id, in arrival order.
    ///
    /// **The audio thread's way of walking the parameters**, because [`LockSet::param_ids`]
    /// allocates a `Vec` and there is no allocating on that thread. Taking an index rather than
    /// returning an iterator is deliberate: the caller needs `&mut self` for its own fields inside
    /// the loop, and a borrow held across the body would not allow that.
    ///
    /// Arrival order, not sorted — the audio thread does not care, and sorting is what the `Vec`
    /// was for.
    pub fn param_id_at(&self, index: usize) -> Option<LockKey> {
        (index < self.param_count).then(|| self.params[index].key)
    }

    /// Every locked parameter id, in a **stable order**.
    ///
    /// **Allocates: for the GUI and for saving, never the audio thread.** See
    /// [`LockSet::param_id_at`].
    ///
    /// Sorted rather than in arrival order: `state.rs` is compared byte for byte by
    /// `t3_session.rs`, and a dump whose order depended on which knob somebody touched first would
    /// differ between two sessions that had reached the same pattern.
    pub fn param_ids(&self) -> Vec<LockKey> {
        let mut ids: Vec<LockKey> = self.params[..self.param_count]
            .iter()
            .map(|p| p.key)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Every locked parameter that has no baseline to measure its offsets from.
    pub fn without_patch(&self) -> Vec<LockKey> {
        self.params[..self.param_count]
            .iter()
            .filter(|p| !p.has_patch())
            .map(|p| p.key)
            .collect()
    }

    /// Whether `param_id` is locked anywhere in the pattern.
    pub fn locks_anywhere(&self, key: impl Into<LockKey>) -> bool {
        self.find(key.into()).is_some()
    }

    /// Whether `step` sets anything at all.
    pub fn step_has_locks(&self, step: usize) -> bool {
        self.locks[..self.lock_count]
            .iter()
            .any(|l| l.step as usize == step)
    }

    /// Every `(param_id, value)` `step` sets, in the same stable order as [`LockSet::param_ids`].
    ///
    /// Allocates, so it is for the GUI and for saving. The audio thread uses
    /// [`LockSet::for_each_at`].
    pub fn at(&self, step: usize) -> Vec<(LockKey, f32)> {
        let mut out: Vec<(LockKey, f32)> = self.locks[..self.lock_count]
            .iter()
            .filter(|l| l.step as usize == step)
            .map(|l| (l.key, l.value))
            .collect();
        out.sort_unstable_by_key(|(key, _)| *key);
        out
    }

    /// Visits what `step` sets without allocating. **This is the audio-thread path.**
    ///
    /// **Every locked parameter, every step** — its offset from the patch, which is zero on the steps
    /// that set nothing. Visiting only the steps that set something is the shape of the defect that
    /// made this feature do nothing at all: an offset left in force means every step after it agrees
    /// with that lock, and a pattern where every step sounds the same is not a sequence.
    ///
    /// The value passed is a **modulation offset**, not a parameter value. See the module docs.
    ///
    /// **One pass over the locks, into a stack scratch**, rather than a search per parameter. The
    /// per-parameter form is `parameters × locks` and a chunk may carry
    /// `MAX_BOUNDARIES_PER_CHUNK` of these; this is `locks + parameters` and the scratch is
    /// [`MAX_LOCKED_PARAMS`] floats on the stack, so nothing allocates.
    pub fn for_each_at(&self, step: usize, mut f: impl FnMut(LockKey, f32)) {
        let mut value = [f32::NAN; MAX_LOCKED_PARAMS];
        for lock in &self.locks[..self.lock_count] {
            if lock.step as usize == step
                && let Some(slot) = self.find(lock.key)
            {
                value[slot] = lock.value;
            }
        }
        for (slot, param) in self.params[..self.param_count].iter().enumerate() {
            // A step whose value *equals* the patch yields zero, and that is right — the two are the
            // same sound, and the player collapses them when authoring for the same reason.
            let offset = if value[slot].is_finite() && param.patch.is_finite() {
                value[slot] - param.patch
            } else {
                0.0
            };
            f(param.key, offset);
        }
    }
}

/// Two sets are equal when they lock the same parameters to the same values.
///
/// Written by hand because the derived one would compare unused slots and arrival order, and would
/// call two identical patterns different because the knobs were touched in a different sequence.
/// `f32` equality is the right comparison here: these are stored values, never computed ones.
impl PartialEq for LockSet {
    fn eq(&self, other: &Self) -> bool {
        if self.param_count != other.param_count || self.lock_count != other.lock_count {
            return false;
        }
        let same_params = self.params[..self.param_count].iter().all(|mine| {
            other.find(mine.key).is_some_and(|index| {
                let theirs = &other.params[index];
                (mine.patch.is_nan() && theirs.patch.is_nan()) || mine.patch == theirs.patch
            })
        });
        same_params
            && self.locks[..self.lock_count].iter().all(|mine| {
                other
                    .find_lock(mine.step as usize, mine.key)
                    .is_some_and(|index| other.locks[index].value == mine.value)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in sequence-patch for tests about the locks themselves.
    ///
    /// Named rather than repeated, so a test that is about *what a step sets* does not read as
    /// though the number mattered. The tests that are about the patch value say so.
    pub(super) const PATCH: f32 = 0.5;

    #[test]
    fn the_two_budgets_refuse_separately_and_say_which() {
        // Bytes are spent per lock; realtime actions per parameter. A single cap conflating them is
        // what made the old form cost `parameters × steps`, so the two refusals are distinct and
        // each names itself — somebody who has hit one needs to know which.
        let mut locks = LockSet::EMPTY;
        for id in 0..MAX_LOCKED_PARAMS as u32 {
            locks
                .set(0, id, 0.5, PATCH)
                .expect("room for the first of each");
        }
        assert_eq!(
            locks.set(0, MAX_LOCKED_PARAMS as u32, 0.5, PATCH),
            Err(Refused::Full),
            "one more *parameter* hits the realtime budget"
        );
    }

    #[test]
    fn the_lock_budget_is_reachable_now_that_a_sequence_can_be_long() {
        // **`Refused::NoRoom` was unreachable while the two budgets coincided**, when `MAX_LOCKS`
        // was `MAX_LOCKED_PARAMS * STEPS` and filling one filled the other. Capacity is
        // `MAX_STEPS` now and the lock budget is a flat figure, so the total is what runs out
        // first — `MAX_LOCKED_PARAMS` parameters across `MAX_STEPS` steps would be far more locks
        // than the budget holds.
        //
        let mut locks = LockSet::EMPTY;
        let mut step = 0;
        let mut id = 0u32;
        // Fill by step, cycling a handful of parameters, so the parameter budget is never the
        // limit — only the total.
        while locks.locks() < MAX_LOCKS {
            locks.set(step, id, 0.5, PATCH).expect("room until full");
            id += 1;
            if id == 4 {
                id = 0;
                step += 1;
            }
        }
        assert_eq!(locks.locks(), MAX_LOCKS);
        assert!(locks.len() <= 4, "only a few parameters were used");

        assert_eq!(
            locks.set(step + 1, 0, 0.5, PATCH),
            Err(Refused::NoRoom),
            "the total is what refuses, and it says so"
        );
    }

    #[test]
    fn a_refused_lock_does_not_leave_a_parameter_behind() {
        // A parameter is added before its first lock is stored, so a refusal between the two would
        // leave one locking nothing — and every step would then pay a realtime action for it.
        let mut locks = LockSet::EMPTY;
        let mut step = 0;
        while locks.locks() < MAX_LOCKS {
            locks.set(step, 1, 0.5, PATCH).expect("room until full");
            step += 1;
        }
        let before = locks.len();
        assert_eq!(locks.set(0, 9_999, 0.5, PATCH), Err(Refused::NoRoom));
        assert_eq!(
            locks.len(),
            before,
            "the parameter added for a lock that did not fit is dropped again"
        );
        assert!(!locks.locks_anywhere(9_999));
    }

    #[test]
    fn a_lock_set_is_copy_and_small_enough_to_publish_by_value() {
        // The reason the audio thread can be handed one without an allocation or an `Arc`.
        //
        // **Two budgets, and the arithmetic says which is which.** A parameter costs an id and the
        // baseline its offsets are measured from; a lock costs a step, an id and a value. Neither
        // term contains the sequence's length — that is the whole point of the sparse form, and a
        // term reappearing here would mean it had been lost.
        // A key is **two** words, not one: which plugin, and which parameter. That is what the
        // fourth byte-group in each term is, and it is what four kilobytes buys — an automation
        // that can name an effect rather than hoping only one plugin is ever loaded.
        let params = MAX_LOCKED_PARAMS * (4 + 4 + 4);
        let locks = MAX_LOCKS * (4 + 4 + 4 + 4);
        assert_eq!(
            std::mem::size_of::<LockSet>(),
            params + locks + 2 * std::mem::size_of::<usize>()
        );
        fn assert_copy<T: Copy>() {}
        assert_copy::<LockSet>();
    }

    #[test]
    fn what_a_lock_set_costs_does_not_depend_on_how_long_the_sequence_is() {
        // The property the representation exists for, asserted rather than described. The dense form
        // spent `parameters × steps`, so every bar added multiplied it; this spends per lock.
        let mut locks = LockSet::EMPTY;
        let before = std::mem::size_of_val(&locks);
        for step in 0..STEPS {
            locks.set(step, 7, 0.5, PATCH).expect("room");
        }
        assert_eq!(
            std::mem::size_of_val(&locks),
            before,
            "storage is a fixed budget; filling it does not change what it costs"
        );
        assert_eq!(
            locks.locks(),
            STEPS,
            "and it holds one lock per step that set one"
        );
        assert_eq!(locks.len(), 1, "for a single parameter");
    }

    #[test]
    fn an_empty_set_locks_nothing_anywhere() {
        let locks = LockSet::EMPTY;
        assert!(locks.is_empty());
        assert_eq!(locks.get(0, 7), None);
        assert!(!locks.locks_anywhere(7));
        assert!(!locks.step_has_locks(0));
    }

    #[test]
    fn a_lock_is_a_value_at_one_step_and_nowhere_else() {
        // The whole point of the sparse form: locking step 3 must not pin the parameter for the
        // bar, which is the opposite of a lock.
        let mut locks = LockSet::EMPTY;
        locks.set(3, 42, 0.75, PATCH).expect("room");

        assert_eq!(locks.get(3, 42), Some(0.75));
        for step in 0..STEPS {
            if step != 3 {
                assert_eq!(locks.get(step, 42), None, "step {step}");
            }
        }
    }

    #[test]
    fn clearing_the_last_lock_on_a_parameter_frees_its_slot() {
        // Clearing is what makes room, rather than a separate operation somebody has to know about.
        let mut locks = LockSet::EMPTY;
        locks.set(0, 1, 0.1, PATCH).expect("room");
        locks.set(5, 1, 0.2, PATCH).expect("room");
        assert_eq!(locks.len(), 1);

        locks.clear(0, 1);
        assert_eq!(locks.len(), 1, "still locked at step 5");
        locks.clear(5, 1);
        assert_eq!(locks.len(), 0, "and now it is gone entirely");
        assert!(!locks.locks_anywhere(1));
    }

    #[test]
    fn a_non_finite_value_clears_rather_than_being_stored() {
        // NaN is how "not locked" is spelled. Letting one in through the front door would make the
        // two indistinguishable.
        let mut locks = LockSet::EMPTY;
        locks.set(2, 9, 0.5, PATCH).expect("room");
        locks.set(2, 9, f32::NAN, PATCH).expect("clears");
        assert_eq!(locks.get(2, 9), None);
        assert!(locks.is_empty(), "and the slot went with it");
    }

    #[test]
    fn the_thirty_third_parameter_is_refused_with_a_reason() {
        // The only limit anybody can reach, and it must say so rather than be ignored.
        let mut locks = LockSet::EMPTY;
        for id in 0..MAX_LOCKED_PARAMS as u32 {
            locks.set(0, id, 0.5, PATCH).expect("room");
        }
        assert_eq!(locks.len(), MAX_LOCKED_PARAMS);

        let refused = locks.set(0, 999, 0.5, PATCH).expect_err("no room");
        assert_eq!(refused, Refused::Full);
        assert!(
            refused.to_string().contains(&MAX_LOCKED_PARAMS.to_string()),
            "{refused}"
        );

        // And a parameter already locked still takes another step, because that needs no slot.
        locks
            .set(4, 0, 0.25, PATCH)
            .expect("an existing parameter always fits");
    }

    #[test]
    fn clearing_one_parameter_makes_room_for_another() {
        let mut locks = LockSet::EMPTY;
        for id in 0..MAX_LOCKED_PARAMS as u32 {
            locks.set(0, id, 0.5, PATCH).expect("room");
        }
        locks.clear_param(0);
        locks.set(0, 999, 0.5, PATCH).expect("the freed slot");
        assert_eq!(locks.len(), MAX_LOCKED_PARAMS);
        assert!(!locks.locks_anywhere(0));
    }

    #[test]
    fn forgetting_a_middle_entry_keeps_the_rest() {
        // The compaction is the fiddly part: an off-by-one here loses somebody's locks silently.
        let mut locks = LockSet::EMPTY;
        for id in 0..5u32 {
            locks.set(0, id, id as f32 / 10.0, PATCH).expect("room");
        }
        locks.clear_param(2);

        assert_eq!(locks.param_ids(), vec![0, 1, 3, 4]);
        for id in [0u32, 1, 3, 4] {
            assert_eq!(locks.get(0, id), Some(id as f32 / 10.0), "id {id}");
        }
    }

    #[test]
    fn the_order_of_a_dump_does_not_depend_on_which_knob_was_touched_first() {
        // `t3_session.rs` compares dumps byte for byte, so two sessions that reached the same
        // pattern by different routes must produce the same bytes.
        let mut one = LockSet::EMPTY;
        let mut other = LockSet::EMPTY;
        for id in [7u32, 3, 11] {
            one.set(0, id, 0.5, PATCH).expect("room");
        }
        for id in [11u32, 7, 3] {
            other.set(0, id, 0.5, PATCH).expect("room");
        }

        assert_eq!(one.param_ids(), other.param_ids());
        assert_eq!(one.at(0), other.at(0));
        assert_eq!(one, other, "and they are the same set");
    }

    #[test]
    fn two_sets_are_equal_by_what_they_lock_and_not_by_arrival_order() {
        let mut one = LockSet::EMPTY;
        one.set(0, 5, 0.5, PATCH).expect("room");
        one.set(1, 6, 0.25, PATCH).expect("room");

        let mut other = LockSet::EMPTY;
        other.set(1, 6, 0.25, PATCH).expect("room");
        other.set(0, 5, 0.5, PATCH).expect("room");

        assert_eq!(one, other);

        other.set(2, 6, 0.9, PATCH).expect("room");
        assert_ne!(one, other, "and a difference in values is a difference");
    }

    #[test]
    fn the_audio_path_yields_offsets_and_the_gui_path_yields_values() {
        // **They deliberately differ, and by exactly the patch.** `at` is what the interface shows
        // and what a file records — the value a step *sets*. `for_each_at` is what the audio thread
        // sends — how far that step moves the parameter from the patch. Asserting they are equal is
        // what an earlier version of this test did, and it was right until locks became modulation.
        let mut locks = LockSet::EMPTY;
        locks.set(1, 4, 0.2, PATCH).expect("room");
        locks.set(1, 9, 0.8, PATCH).expect("room");
        locks.set(2, 4, 0.3, PATCH).expect("room");

        let mut visited = Vec::new();
        locks.for_each_at(1, |id, v| visited.push((id, v)));
        visited.sort_unstable_by_key(|(id, _)| *id);

        // Compared with a tolerance, because the offset is a subtraction and adding the patch back
        // does not have to land on the same bits. That is a property of floating point, not of this.
        let restated: Vec<(LockKey, f32)> =
            visited.iter().map(|(id, off)| (*id, off + PATCH)).collect();
        let expected = locks.at(1);
        assert_eq!(restated.len(), expected.len());
        for ((id, got), (want_id, want)) in restated.iter().zip(expected.iter()) {
            assert_eq!(id, want_id);
            assert!(
                (got - want).abs() < 1e-6,
                "offset plus patch is the value the step sets: {got} vs {want}"
            );
        }
    }

    #[test]
    fn a_step_that_sets_nothing_is_still_visited_with_a_zero_offset() {
        // Silence would leave the previous step's offset in force, and the parameter would stay
        // where that step put it for the rest of the bar — the defect that made this feature do
        // nothing at all, in its modulation form.
        let mut locks = LockSet::EMPTY;
        locks.set(0, 4, 0.9, 0.2).expect("room");

        let mut visited = Vec::new();
        locks.for_each_at(1, |id, v| visited.push((id, v)));
        assert_eq!(
            visited,
            vec![(LockKey::source(4), 0.0)],
            "step 2 sets nothing, and must say so"
        );
    }

    #[test]
    fn a_step_holding_the_patch_value_offsets_by_nothing() {
        // The same sound as no lock at all, which is why the player collapses the two when
        // authoring. Here it falls out of the arithmetic rather than being a special case.
        let mut locks = LockSet::EMPTY;
        locks.set(0, 4, 0.5, 0.5).expect("room");

        let mut visited = Vec::new();
        locks.for_each_at(0, |id, v| visited.push((id, v)));
        assert_eq!(visited, vec![(LockKey::source(4), 0.0)]);
    }

    #[test]
    fn a_step_knows_whether_it_sets_anything() {
        // What the step button's second mark is drawn from.
        let mut locks = LockSet::EMPTY;
        locks.set(6, 2, 0.4, PATCH).expect("room");
        assert!(locks.step_has_locks(6));
        assert!(!locks.step_has_locks(7));
    }
}

/// A [`LockSet`] as it is written to the settings file.
///
/// **`Option<f32>` rather than the in-memory NaN**, because JSON has no NaN: a non-finite float is
/// written as `null` and then read back as a parse error, so the sentinel that is right in memory is
/// exactly wrong on disk. `null` says "this step does not set it" in a way a person reading the file
/// can see.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LockData {
    /// The plugin these were recorded against.
    ///
    /// Parameter ids mean nothing on their own — id 7 is a filter cutoff in one instrument and a
    /// glide time in another. Stored so locks recorded for one instrument are not silently applied
    /// to a different one, which would be automation nobody wrote.
    pub plugin: Option<String>,
    pub params: Vec<LockedParam>,
}

/// Which effect a locked parameter belongs to, in a form that still means the same effect in a
/// later session.
///
/// **Not an [`FxId`]**, which is a counter and means nothing once the player has restarted, and not
/// a position, which changes when an earlier effect is removed. What identifies an effect to a
/// person is *which plugin it is* and *which of them*, and that is what is written down.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FxRef {
    /// The effect's `CLAP_ID`.
    pub plugin: String,
    /// Which of the effects carrying that id, counting from zero — so two copies of one effect in
    /// a chain keep their own automation rather than sharing it.
    #[serde(default)]
    pub ordinal: usize,
}

/// Each effect in the chain, in order: its live id and its `CLAP_ID`.
///
/// What [`LockData::capture`] turns ids into references with, and what [`LockData::to_locks`] turns
/// references back into ids with.
pub type ChainRefs<'a> = &'a [(FxId, String)];

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LockedParam {
    pub param: u32,
    /// Which effect this parameter belongs to. **Absent means the source**, which is what every
    /// pattern saved before effects could be automated means — so those files load unchanged, with
    /// no version bump and no migration step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx: Option<FxRef>,
    /// What the steps that do not set this parameter put it back to.
    ///
    /// `Option`, and absent from files written before it existed. Such a file loads with **no**
    /// baseline, deliberately: the player then supplies the value the instrument is actually set to,
    /// which it knows and this file does not. An earlier version stood the first locked value in
    /// here, which looked harmless and stopped that from ever happening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<f32>,
    /// One entry per step; `null` where the step sets nothing.
    pub steps: Vec<Option<f32>>,
}

/// Turns an effect's live id into the reference a file keeps, using the chain it is in.
fn reference_for(fx: FxId, chain: ChainRefs<'_>) -> Option<FxRef> {
    let index = chain.iter().position(|(id, _)| *id == fx)?;
    let plugin = chain[index].1.clone();
    let ordinal = chain[..index]
        .iter()
        .filter(|(_, id)| *id == plugin)
        .count();
    Some(FxRef { plugin, ordinal })
}

/// Turns a file's reference back into a live id, if that effect is in the chain.
fn resolve(reference: &FxRef, chain: ChainRefs<'_>) -> Option<FxId> {
    chain
        .iter()
        .filter(|(_, id)| *id == reference.plugin)
        .nth(reference.ordinal)
        .map(|(id, _)| *id)
}

impl LockData {
    /// Captures a lock set for the settings file.
    ///
    /// `chain` is what the effect ids are written down as; `pending` are entries a previous load
    /// could not resolve, carried through untouched so that saving a pattern whose effect is not
    /// currently in the chain does not quietly discard its automation.
    pub fn capture_in_chain(
        locks: &LockSet,
        plugin: Option<&str>,
        chain: ChainRefs<'_>,
        pending: &[LockedParam],
    ) -> Result<Self, String> {
        let steps = locks.locks[..locks.lock_count]
            .iter()
            .map(|lock| lock.step as usize + 1)
            .max()
            .unwrap_or(STEPS)
            .max(STEPS);
        let mut params = Vec::new();
        for key in locks.param_ids() {
            // None means Source on disk. An unresolvable live effect must never become None.
            let fx = if key.is_source() {
                None
            } else {
                Some(reference_for(key.fx, chain).ok_or_else(|| {
                    format!("cannot save lock {key}: effect is absent from the chain")
                })?)
            };
            params.push(LockedParam {
                param: key.param_id,
                fx,
                patch: locks.patch(key),
                steps: (0..steps).map(|step| locks.get(step, key)).collect(),
            });
        }
        params.extend_from_slice(pending);
        Ok(Self {
            plugin: plugin.map(str::to_owned),
            params,
        })
    }

    /// Captures a source-only set. Effect callers must supply a chain to `capture_in_chain`.
    /// Panics on an effect key rather than writing it as source automation.
    #[must_use]
    pub fn capture(locks: &LockSet, plugin: Option<&str>) -> Self {
        Self::capture_in_chain(locks, plugin, &[], &[])
            .expect("source-only capture received an effect lock")
    }

    /// Rebuilds a lock set, reporting anything it could not take.
    ///
    /// Problems are collected rather than failing the whole file, exactly as `PatternData` does: a
    /// lock set that outgrew the cap should not cost you the ones that fit.
    #[must_use]
    pub fn to_locks(&self, plugin: Option<&str>) -> (LockSet, Vec<String>) {
        let (locks, problems, _) = self.to_locks_in_chain(plugin, &[]);
        (locks, problems)
    }

    /// Rebuilds a lock set against a chain, and hands back the entries it could not place.
    ///
    /// **An entry whose effect is not in the chain is kept, not dropped.** A sequence is work, and a
    /// pattern saved with an effect automated and loaded before that effect is added would otherwise
    /// lose it silently — the worst of the three outcomes. They come back as `pending`: held aside,
    /// never published to the audio thread, re-resolved when a matching effect appears, and written
    /// back out on the next save.
    #[must_use]
    pub fn to_locks_in_chain(
        &self,
        plugin: Option<&str>,
        chain: ChainRefs<'_>,
    ) -> (LockSet, Vec<String>, Vec<LockedParam>) {
        let mut locks = LockSet::EMPTY;
        let mut problems = Vec::new();
        let mut pending: Vec<LockedParam> = Vec::new();

        // Locks recorded against a different instrument are **dropped, not translated**. There is no
        // honest mapping from one plugin's parameter ids to another's, and applying them anyway
        // would move whichever parameters happened to share a number.
        let foreign_source = matches!((self.plugin.as_deref(), plugin),
            (Some(recorded), Some(loaded)) if recorded != loaded);
        if foreign_source && self.params.iter().any(|entry| entry.fx.is_none()) {
            problems.push(format!(
                "source parameter locks were recorded for `{}`, not `{}` - dropped",
                self.plugin.as_deref().unwrap(),
                plugin.unwrap()
            ));
        }

        for entry in &self.params {
            if foreign_source && entry.fx.is_none() {
                continue;
            }
            // An effect that is not in the chain: held aside rather than dropped or, worse,
            // loaded with no target and delivered to whatever hashes the same id.
            let target = match &entry.fx {
                None => LockKey::source(entry.param),
                Some(reference) => match resolve(reference, chain) {
                    Some(fx) => LockKey::fx(fx, entry.param),
                    None => {
                        pending.push(entry.clone());
                        continue;
                    }
                },
            };
            // **A file written before patch values existed gets none here**, deliberately. An
            // earlier version stood the first locked value in, which looks harmless and is not: it
            // makes `has_patch` true, so the player's own `adopt_missing_baselines` — which knows
            // what the instrument is actually set to — never runs, and every step then deviates from
            // a value nobody chose. Leaving it absent is what lets the player supply a real one.
            let patch = entry.patch.filter(|p| p.is_finite());

            for (step, value) in entry.steps.iter().enumerate() {
                let Some(value) = value else { continue };
                if !value.is_finite() {
                    problems.push(format!(
                        "parameter {}: step {} is not a number",
                        entry.param,
                        step + 1
                    ));
                    continue;
                }
                if locks
                    .set(step, target, *value, patch.unwrap_or(f32::NAN))
                    .is_err()
                {
                    problems.push(format!(
                        "parameter {}: no room for any more locked parameters",
                        entry.param
                    ));
                    break;
                }
            }
        }

        (locks, problems, pending)
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::tests::PATCH;
    use super::*;

    fn sample() -> LockSet {
        let mut locks = LockSet::EMPTY;
        locks.set(0, 7, 0.25, PATCH).expect("room");
        locks.set(3, 7, 0.75, PATCH).expect("room");
        locks.set(3, 12, 0.5, PATCH).expect("room");
        locks
    }

    #[test]
    fn a_lock_set_survives_the_round_trip() {
        let locks = sample();
        let data = LockData::capture(&locks, Some("com.mxm.101"));
        let text = serde_json::to_string(&data).expect("serialises");
        let read: LockData = serde_json::from_str(&text).expect("parses");
        let (back, problems) = read.to_locks(Some("com.mxm.101"));

        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(back, locks);
    }

    #[test]
    fn the_in_memory_nan_sentinel_cannot_be_written_to_disk() {
        // **Why `Option<f32>` and not the NaN that is right in memory.** This is not a mutation
        // anyone can make to the code under test - it is a property of JSON - so the test carries
        // its own evidence: the alternative, spelled out, and shown to lose data.
        //
        // A non-finite float is written as `null`, and `null` will not read back as an `f32`. So the
        // NaN form does not survive one round trip, and it fails on *read*, with a file already
        // written - the worst place to find out.
        let nan_form = serde_json::to_string(&vec![f32::NAN, 0.5]).expect("serialises");
        assert_eq!(nan_form, "[null,0.5]", "NaN is not a JSON number");
        assert!(
            serde_json::from_str::<Vec<f32>>(&nan_form).is_err(),
            "if this ever parses, the sentinel would be safe on disk and this file can simplify"
        );

        // The form actually used reads back, and says "sets nothing" where nothing is set.
        let text = serde_json::to_string(&LockData::capture(&sample(), None)).expect("serialises");
        assert!(text.contains("null"), "unlocked steps must be null: {text}");
        assert!(
            serde_json::from_str::<LockData>(&text).is_ok(),
            "what we write must be readable: {text}"
        );
    }

    #[test]
    fn locks_recorded_for_another_plugin_are_dropped() {
        let data = LockData::capture(&sample(), Some("com.mxm.101"));
        let (back, problems) = data.to_locks(Some("com.other.synth"));

        assert_eq!(
            back,
            LockSet::EMPTY,
            "parameter 7 means something else there"
        );
        assert_eq!(problems.len(), 1, "and it must say so: {problems:?}");
    }

    #[test]
    fn locks_from_a_file_written_before_the_plugin_was_recorded_are_kept() {
        // `plugin: None` is what an older settings file has. Refusing those would throw away work
        // for a rule that did not exist when it was written.
        let mut data = LockData::capture(&sample(), Some("com.mxm.101"));
        data.plugin = None;
        let (back, problems) = data.to_locks(Some("com.mxm.101"));

        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(back, sample());
    }

    #[test]
    fn a_file_with_too_many_locked_parameters_keeps_the_ones_that_fit() {
        let params = (0..MAX_LOCKED_PARAMS as u32 + 4)
            .map(|param| LockedParam {
                fx: None,
                param,
                patch: Some(PATCH),
                steps: (0..STEPS).map(|_| Some(0.5)).collect(),
            })
            .collect();
        let data = LockData {
            plugin: None,
            params,
        };
        let (back, problems) = data.to_locks(None);

        assert_eq!(back.param_ids().len(), MAX_LOCKED_PARAMS);
        assert_eq!(problems.len(), 4, "and each one says why: {problems:?}");
    }

    #[test]
    fn a_nonsense_value_costs_only_itself() {
        let data = LockData {
            plugin: None,
            params: vec![LockedParam {
                fx: None,
                param: 7,
                patch: Some(PATCH),
                steps: vec![Some(f32::INFINITY), Some(0.5)],
            }],
        };
        let (back, problems) = data.to_locks(None);

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(back.get(1, 7), Some(0.5), "the readable one survives");
    }
}
