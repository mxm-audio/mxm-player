//! Press identity: who is holding what, with which voice ID, and what is owed to the plugin.
//!
//! Three things this exists to get right, each of which was a real bug in an earlier design:
//!
//! * **Every press gets a voice ID, physical MIDI included.** mxm-mono-01's `NoteId::matches()`
//!   compares IDs only when *both* sides have one, otherwise falling back to channel and note,
//!   and removal scans newest-first. A choke carrying an ID would therefore be able to kill an
//!   ID-less physical note of the same pitch.
//! * **A release belongs to a specific press.** With two same-pitch presses deferred under
//!   sustain and a third arriving afterwards, a *count* cannot say which ID the pedal release
//!   owes. Every press is tracked individually.
//! * **Presses are counted per press, not per pitch.** mxm-mono-01 keeps a stack: a held key struck
//!   twice owes the plugin two note-offs. Collapsing them leaves the note sounding.
//!
//! The table lives on the audio thread, which is the one place every source's events converge.
//! That is what lets a release always be resolved to the exact press it belongs to, and what
//! makes pedal-lift flush sources that are not currently sending anything.

use super::input::SourceId;

/// How many presses the GUI may hold at once. Bounds the emergency reserve in the merged buffer:
/// focus-loss cleanup must never be refused for lack of room.
pub const MAX_GUI_PRESSES: usize = 128;

/// How many presses may be outstanding across every source at once. Exhaustion is bounded and
/// counted, never allocated for.
pub const MAX_TRACKED_PRESSES: usize = 512;

/// One outstanding press.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Press {
    pub source: SourceId,
    pub channel: u8,
    pub key: u8,
    pub voice_id: i32,
    /// Set when a release arrived while sustain was down. The press is still owed a note-off.
    pub release_deferred: bool,
}

/// What a release should cause.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// Emit a note-off for this press now. The entry is retained until
    /// [`PressTable::retire`] confirms the event was actually accepted.
    Release(Press),
    /// Sustain is down: the release is owed, and will be emitted when the pedal lifts.
    Deferred(Press),
    /// Nothing matched. A note-off with no press is not an error — it happens whenever a
    /// panic has already cleared the note — but it must not invent one.
    Unmatched,
}

/// Hands out voice IDs from a preallocated range.
///
/// IDs are never reused while a press holding one is outstanding, so a stale event carrying an
/// old ID cannot be mistaken for a live voice.
#[derive(Debug)]
pub struct VoiceIdPool {
    next: i32,
    /// Counts the times a press could not be tracked because the table was full.
    exhausted: u64,
}

impl VoiceIdPool {
    pub fn new() -> Self {
        Self {
            next: 1,
            exhausted: 0,
        }
    }

    fn allocate(&mut self) -> i32 {
        let id = self.next;
        // CLAP voice IDs are `i32` with -1 meaning "unspecified", so wrap back to 1 rather than
        // ever producing a negative one.
        self.next = if self.next == i32::MAX {
            1
        } else {
            self.next + 1
        };
        id
    }

    pub fn exhausted(&self) -> u64 {
        self.exhausted
    }
}

impl Default for VoiceIdPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Every outstanding press, in press order.
///
/// Ordered oldest-first; matching scans newest-first, mirroring how mxm-mono-01's own note stack
/// removes entries.
pub struct PressTable {
    presses: Vec<Press>,
    pool: VoiceIdPool,
    sustain_held: bool,
}

impl PressTable {
    /// Allocates for [`MAX_TRACKED_PRESSES`]. Called at activation, on the GUI thread.
    pub fn new() -> Self {
        Self {
            presses: Vec::with_capacity(MAX_TRACKED_PRESSES),
            pool: VoiceIdPool::new(),
            sustain_held: false,
        }
    }

    pub fn sustain_held(&self) -> bool {
        self.sustain_held
    }

    pub fn len(&self) -> usize {
        self.presses.len()
    }

    pub fn is_empty(&self) -> bool {
        self.presses.is_empty()
    }

    pub fn exhausted(&self) -> u64 {
        self.pool.exhausted()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Press> {
        self.presses.iter()
    }

    /// How many presses a given source is holding. What the focus-loss cleanup has to cover.
    pub fn count_for(&self, source: SourceId) -> usize {
        self.presses.iter().filter(|p| p.source == source).count()
    }

    /// Registers a new press and assigns it a voice ID.
    ///
    /// Returns `None` when the table is full, which is counted rather than allocated for. The
    /// caller must then *not* send the note-on: an untracked press is a note we could never
    /// release.
    pub fn press(&mut self, source: SourceId, channel: u8, key: u8) -> Option<Press> {
        if self.presses.len() == MAX_TRACKED_PRESSES {
            self.pool.exhausted += 1;
            return None;
        }

        let press = Press {
            source,
            channel,
            key,
            voice_id: self.pool.allocate(),
            release_deferred: false,
        };
        self.presses.push(press);
        Some(press)
    }

    /// Resolves a release to exactly one press: the **newest** matching one that is not already
    /// awaiting a deferred release.
    ///
    /// Newest-first is not arbitrary: mxm-mono-01's `NoteStack::remove` scans newest-first too, so
    /// matching the same way keeps the host's idea of which voice is ending aligned with the
    /// plugin's.
    pub fn release(&mut self, source: SourceId, channel: u8, key: u8) -> ReleaseOutcome {
        let Some(index) = self.presses.iter().rposition(|p| {
            p.source == source && p.channel == channel && p.key == key && !p.release_deferred
        }) else {
            return ReleaseOutcome::Unmatched;
        };

        if self.sustain_held {
            self.presses[index].release_deferred = true;
            return ReleaseOutcome::Deferred(self.presses[index]);
        }

        ReleaseOutcome::Release(self.presses[index])
    }

    /// Resolves a release that **ignores the sustain pedal**.
    ///
    /// For the sequencer, whose gate is a fixed fraction of a step. Routing it through
    /// [`PressTable::release`] would let a held pedal defer it, turning that gate into a drone —
    /// and a test utility's timing must not change because a pedal is down.
    ///
    /// Returns the press without removing it: like `release`, the accounting survives until the
    /// note-off has actually been accepted, so a refused event can be retried.
    pub fn take_exact(&mut self, source: SourceId, channel: u8, key: u8) -> Option<Press> {
        let index = self.presses.iter().rposition(|p| {
            p.source == source && p.channel == channel && p.key == key && !p.release_deferred
        })?;
        Some(self.presses[index])
    }

    /// Removes a press once its note-off or choke has actually been accepted.
    ///
    /// Never optimistic: an event that could not be enqueued leaves the press in place so it can
    /// be retried, rather than losing the accounting needed to retry it.
    pub fn retire(&mut self, voice_id: i32) -> bool {
        if let Some(index) = self.presses.iter().position(|p| p.voice_id == voice_id) {
            self.presses.remove(index);
            true
        } else {
            false
        }
    }

    /// Sets the pedal, collecting into `out` the presses owed a release when it lifts.
    ///
    /// Each deferred press yields exactly one note-off, which is what stops a key struck twice
    /// under the pedal from being collapsed into a single release. `out` is a caller-owned
    /// buffer so the audio thread never allocates to answer this.
    pub fn set_sustain(&mut self, held: bool, out: &mut Vec<Press>) {
        out.clear();
        self.sustain_held = held;
        if held {
            return;
        }
        out.extend(self.presses.iter().filter(|p| p.release_deferred).copied());
    }

    /// Everything one source is holding, newest first — the order the chokes are emitted in.
    pub fn outstanding_for(&self, source: SourceId, out: &mut Vec<Press>) {
        out.clear();
        out.extend(
            self.presses
                .iter()
                .rev()
                .filter(|p| p.source == source)
                .copied(),
        );
    }

    /// Everything every source is holding, newest first.
    pub fn outstanding(&self, out: &mut Vec<Press>) {
        out.clear();
        out.extend(self.presses.iter().rev().copied());
    }

    /// Drops all accounting. Only ever called once a *global* recovery has been accepted — the
    /// blunt path, for when we have lost track of what is held.
    pub fn clear(&mut self) {
        self.presses.clear();
        self.sustain_held = false;
    }

    /// Drops one source's accounting, leaving every other source untouched.
    pub fn clear_source(&mut self, source: SourceId) {
        self.presses.retain(|p| p.source != source);
    }
}

impl Default for PressTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUI: SourceId = SourceId(0);
    const KEYBOARD: SourceId = SourceId(1);

    #[test]
    fn every_press_gets_its_own_voice_id() {
        let mut table = PressTable::new();
        let a = table.press(GUI, 0, 60).unwrap();
        let b = table.press(KEYBOARD, 0, 60).unwrap();
        assert_ne!(
            a.voice_id, b.voice_id,
            "two presses of the same pitch from different sources must be distinguishable"
        );
    }

    #[test]
    fn a_release_resolves_to_the_newest_press_of_that_key() {
        // **The hazard `emit_sequencer_action` has to work around for a tie.**
        //
        // `take_exact` matches with `rposition`, which is correct for a keyboard -- a key struck
        // twice releases the newer press first, matching how a plugin's note stack pops. It is
        // exactly wrong for a legato joint: a tie carrying a pitch that is already sounding emits
        // the new note-on *first* (so the plugin sees it arrive while the old one is held), and a
        // fresh lookup for the note-off then names the press that was just made rather than the
        // one the tie is meant to end.
        //
        // So `emit_legato_joint` resolves the release **before** the note-ons, against the table
        // as it stands here. This test is that state: with two presses of one key outstanding, a
        // lookup finds the newer.
        let mut table = PressTable::new();
        let outgoing = table.press(GUI, 0, 60).unwrap();
        let incoming = table.press(GUI, 0, 60).unwrap();

        let found = table
            .take_exact(GUI, 0, 60)
            .expect("a press is outstanding");
        assert_eq!(
            found.voice_id, incoming.voice_id,
            "a lookup after the tie's note-on names the new press, not the one to end"
        );
        assert_ne!(found.voice_id, outgoing.voice_id);

        // And the reason capturing early is safe: `take_exact` reports without removing, so
        // nothing has been treated as delivered by having looked.
        assert_eq!(table.len(), 2, "looking must not retire anything");
    }

    #[test]
    fn a_key_struck_twice_owes_two_releases() {
        let mut table = PressTable::new();
        let first = table.press(GUI, 0, 60).unwrap();
        let second = table.press(GUI, 0, 60).unwrap();
        assert_eq!(table.len(), 2);

        // Newest-first, matching how the plugin's own note stack pops.
        let ReleaseOutcome::Release(released) = table.release(GUI, 0, 60) else {
            panic!("the first release should resolve to a press")
        };
        assert_eq!(released.voice_id, second.voice_id);
        table.retire(released.voice_id);

        let ReleaseOutcome::Release(released) = table.release(GUI, 0, 60) else {
            panic!("the second release should resolve to the remaining press")
        };
        assert_eq!(released.voice_id, first.voice_id);
        table.retire(released.voice_id);

        assert!(table.is_empty());
        assert_eq!(table.release(GUI, 0, 60), ReleaseOutcome::Unmatched);
    }

    #[test]
    fn sustain_defers_every_press_individually_not_once_per_pitch() {
        let mut table = PressTable::new();
        let mut owed = Vec::new();
        table.set_sustain(true, &mut owed);
        table.press(GUI, 0, 60).unwrap();
        table.press(GUI, 0, 60).unwrap();

        assert!(matches!(
            table.release(GUI, 0, 60),
            ReleaseOutcome::Deferred(_)
        ));
        assert!(matches!(
            table.release(GUI, 0, 60),
            ReleaseOutcome::Deferred(_)
        ));

        table.set_sustain(false, &mut owed);
        assert_eq!(
            owed.len(),
            2,
            "a single collapsed release would leave the note sounding"
        );
    }

    #[test]
    fn a_deferred_release_ends_exactly_the_press_it_belongs_to() {
        // Three presses of one pitch; only the middle one is released under the pedal.
        let mut table = PressTable::new();
        let first = table.press(GUI, 0, 60).unwrap();
        let second = table.press(GUI, 0, 60).unwrap();
        let mut owed = Vec::new();
        table.set_sustain(true, &mut owed);

        let ReleaseOutcome::Deferred(deferred) = table.release(GUI, 0, 60) else {
            panic!("release under the pedal should defer")
        };
        assert_eq!(deferred.voice_id, second.voice_id);

        let third = table.press(GUI, 0, 60).unwrap();

        table.set_sustain(false, &mut owed);
        assert_eq!(owed.len(), 1);
        assert_eq!(
            owed[0].voice_id, second.voice_id,
            "the pedal owes the specific press that was released, not the newest or oldest"
        );

        // The other two are still held and still resolvable.
        assert!(table.retire(owed[0].voice_id));
        let ReleaseOutcome::Release(next) = table.release(GUI, 0, 60) else {
            panic!("the remaining presses must still be releasable")
        };
        assert_eq!(next.voice_id, third.voice_id);
        table.retire(next.voice_id);
        let ReleaseOutcome::Release(last) = table.release(GUI, 0, 60) else {
            panic!("the oldest press must still be releasable")
        };
        assert_eq!(last.voice_id, first.voice_id);
    }

    #[test]
    fn clearing_one_source_leaves_the_others_holding_their_notes() {
        let mut table = PressTable::new();
        let gui = table.press(GUI, 0, 60).unwrap();
        let physical = table.press(KEYBOARD, 0, 60).unwrap();

        let mut scratch = Vec::new();
        table.outstanding_for(GUI, &mut scratch);
        assert_eq!(scratch, vec![gui]);
        table.clear_source(GUI);

        assert_eq!(table.len(), 1);
        table.outstanding(&mut scratch);
        assert_eq!(scratch[0].voice_id, physical.voice_id);
        // ...and the physical note still responds to its own note-off afterwards.
        assert!(matches!(
            table.release(KEYBOARD, 0, 60),
            ReleaseOutcome::Release(_)
        ));
    }

    #[test]
    fn a_refused_release_keeps_its_accounting_so_it_can_be_retried() {
        let mut table = PressTable::new();
        let press = table.press(GUI, 0, 60).unwrap();

        let ReleaseOutcome::Release(released) = table.release(GUI, 0, 60) else {
            panic!("expected a release")
        };
        // Pretend the event could not be enqueued: nothing is retired.
        assert_eq!(table.len(), 1, "the press must survive a refused release");
        assert_eq!(released.voice_id, press.voice_id);

        // On retry it is still there to be released.
        assert!(table.retire(press.voice_id));
        assert!(table.is_empty());
    }

    #[test]
    fn press_tracking_is_bounded_and_counted_not_allocated_for() {
        let mut table = PressTable::new();
        for i in 0..MAX_TRACKED_PRESSES {
            assert!(table.press(GUI, 0, (i % 128) as u8).is_some());
        }
        assert!(table.press(GUI, 0, 60).is_none());
        assert_eq!(table.exhausted(), 1);
    }
}
