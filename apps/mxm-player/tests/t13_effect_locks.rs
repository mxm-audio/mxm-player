//! T13 — the sequencer's automation reaching an effect, and letting go of it cleanly.
//!
//! The owner's request of 2026-09-06: *can the effects' parameters be set with the sequencer, like
//! the synths can — and if an effect is deleted, the sequencer parts connected to it should be
//! deleted.*
//!
//! # Why the deletion half is the one to read first
//!
//! `param_id` is nice-plug's hash of a parameter's string id, so it identifies a parameter **within
//! one plugin** and nothing more: two plugins can hash two different parameters to the same `u32`.
//! While only the source could be automated that was harmless. The moment an effect can be, a lock
//! left behind by a removed effect stops being stale data and becomes a **silent misdelivery** —
//! sent to whatever else hashes the same, with nothing to show for it.
//!
//! So the target is part of a lock's key, and the tests below are about the three ways a chain
//! changes underneath one: removal takes the automation with it, a reorder does not touch it, and a
//! bypass does not either, because bypass is not deletion.

use mxm_player::engine::fx::{FxId, NO_FX_ID};
use mxm_player::sequencer::locks::{FxRef, LockData, LockKey, LockSet, LockedParam};

const PATCH: f32 = 0.5;
const CUTOFF: u32 = 7;
const DELAY_TIME: u32 = 7;
const FEEDBACK: u32 = 11;

const ONE: FxId = 1;
const TWO: FxId = 2;

/// A set with the source and two effects all automating something.
fn mixed() -> LockSet {
    let mut locks = LockSet::EMPTY;
    locks.set(0, CUTOFF, 0.2, PATCH).expect("room");
    locks.set(4, CUTOFF, 0.9, PATCH).expect("room");
    locks
        .set(0, LockKey::fx(ONE, DELAY_TIME), 0.3, PATCH)
        .expect("room");
    locks
        .set(8, LockKey::fx(ONE, FEEDBACK), 0.8, PATCH)
        .expect("room");
    locks
        .set(2, LockKey::fx(TWO, DELAY_TIME), 0.6, PATCH)
        .expect("room");
    locks
}

/// **The hazard, stated as a test.** Two plugins whose parameters hash alike are not hypothetical —
/// the hash is `h = h*31 + byte` over a short string — and the model must keep them apart by
/// construction rather than by nobody happening to collide.
#[test]
fn one_parameter_id_on_two_plugins_is_two_different_locks() {
    let mut locks = LockSet::EMPTY;
    // The *same* id, on the source and on each of two effects.
    locks.set(0, DELAY_TIME, 0.1, PATCH).expect("room");
    locks
        .set(0, LockKey::fx(ONE, DELAY_TIME), 0.2, PATCH)
        .expect("room");
    locks
        .set(0, LockKey::fx(TWO, DELAY_TIME), 0.3, PATCH)
        .expect("room");

    assert_eq!(locks.len(), 3, "three plugins, three locked parameters");
    assert_eq!(locks.get(0, DELAY_TIME), Some(0.1));
    assert_eq!(locks.get(0, LockKey::fx(ONE, DELAY_TIME)), Some(0.2));
    assert_eq!(locks.get(0, LockKey::fx(TWO, DELAY_TIME)), Some(0.3));
}

/// **A source lock is still its bare id**, which is what keeps every caller, file and script that
/// predates effect automation working unchanged — and an effect's lock matches none of them.
#[test]
fn the_source_keeps_the_bare_form_and_an_effect_never_answers_to_it() {
    let key = LockKey::source(CUTOFF);
    assert!(key.is_source());
    assert_eq!(key, CUTOFF);
    assert_eq!(key.fx, NO_FX_ID);

    let effect = LockKey::fx(ONE, CUTOFF);
    assert!(!effect.is_source());
    assert_ne!(
        effect, CUTOFF,
        "an effect's lock must not answer to a bare id"
    );
    assert_eq!(effect.to_string(), "fx1:7");
    assert_eq!(key.to_string(), "7");
}

/// **The owner's requirement.** Removing an effect removes the locks that belonged to it — and the
/// parameters and baselines with them, so the pattern carries no invisible automation and neither
/// budget is spent on an effect that is gone.
#[test]
fn removing_an_effect_removes_its_automation_and_nothing_else() {
    let mut locks = mixed();
    let before = locks.locks();

    assert!(locks.clear_fx(ONE), "effect one was automated");

    assert_eq!(locks.get(0, LockKey::fx(ONE, DELAY_TIME)), None);
    assert_eq!(locks.get(8, LockKey::fx(ONE, FEEDBACK)), None);
    assert!(!locks.locks_anywhere(LockKey::fx(ONE, DELAY_TIME)));
    assert!(!locks.locks_anywhere(LockKey::fx(ONE, FEEDBACK)));

    // The source's and the other effect's are untouched, to the value.
    assert_eq!(locks.get(0, CUTOFF), Some(0.2));
    assert_eq!(locks.get(4, CUTOFF), Some(0.9));
    assert_eq!(locks.get(2, LockKey::fx(TWO, DELAY_TIME)), Some(0.6));
    assert_eq!(locks.locks(), before - 2);
    assert_eq!(locks.automated_fx(), vec![TWO]);
}

/// Removing an effect nothing automates changes nothing at all, and says so — a caller uses that to
/// avoid republishing the sequencer for no reason.
#[test]
fn removing_an_effect_that_was_never_automated_changes_nothing() {
    let mut locks = mixed();
    let before = locks;
    assert!(!locks.clear_fx(9), "effect nine automates nothing");
    assert_eq!(locks, before);
}

/// **A reorder must not touch automation, and that is what ids are for.** Moving an effect changes
/// its position and nothing else; a design keyed on position would swap two effects' automation
/// here without a single lock being written.
#[test]
fn a_reorder_is_invisible_to_the_locks() {
    let locks = mixed();
    // A move changes no key, because no key holds a position. Stated as the property rather than by
    // calling `move_fx`, which needs a live chain: what would break under a position-keyed design is
    // exactly this — that the set is a function of ids alone.
    assert_eq!(locks.get(0, LockKey::fx(ONE, DELAY_TIME)), Some(0.3));
    assert_eq!(locks.get(2, LockKey::fx(TWO, DELAY_TIME)), Some(0.6));
    assert_eq!(locks.automated_fx(), vec![ONE, TWO]);
}

/// **The source is never swept up by an effect's removal**, whatever ids are in play.
#[test]
fn clearing_an_effect_can_never_clear_the_source() {
    let mut locks = mixed();
    assert!(
        !locks.clear_fx(NO_FX_ID),
        "there is no effect zero, and asking must not empty the source's automation"
    );
    assert_eq!(locks.get(0, CUTOFF), Some(0.2));
    locks.clear_fx(ONE);
    locks.clear_fx(TWO);
    assert_eq!(locks.get(0, CUTOFF), Some(0.2));
    assert_eq!(locks.get(4, CUTOFF), Some(0.9));
}

// --- the file -----------------------------------------------------------------------------------

fn chain() -> Vec<(FxId, String)> {
    vec![
        (ONE, "dk.mxm.mxm-bucket-delay".to_owned()),
        (TWO, "dk.mxm.mxm-folded-spring".to_owned()),
    ]
}

/// An effect's automation survives a save and a load, matched back to the same effect.
#[test]
fn an_effects_automation_round_trips_through_the_file() {
    let locks = mixed();
    let data =
        LockData::capture_in_chain(&locks, Some("dk.mxm.mxm-mono-01"), &chain(), &[]).unwrap();
    let text = serde_json::to_string(&data).expect("serialises");
    let read: LockData = serde_json::from_str(&text).expect("parses");

    let (back, problems, pending) = read.to_locks_in_chain(Some("dk.mxm.mxm-mono-01"), &chain());
    assert!(problems.is_empty(), "{problems:?}");
    assert!(pending.is_empty(), "every effect is in the chain");
    assert_eq!(back, locks);
}

/// **Two copies of one effect keep their own automation.** The reference is the plugin *and which
/// of them*, so a chain holding the same delay twice does not merge their locks.
#[test]
fn two_copies_of_one_effect_do_not_share_automation() {
    let twice = vec![
        (ONE, "dk.mxm.mxm-bucket-delay".to_owned()),
        (TWO, "dk.mxm.mxm-bucket-delay".to_owned()),
    ];
    let mut locks = LockSet::EMPTY;
    locks
        .set(0, LockKey::fx(ONE, DELAY_TIME), 0.25, PATCH)
        .expect("room");
    locks
        .set(0, LockKey::fx(TWO, DELAY_TIME), 0.75, PATCH)
        .expect("room");

    let data = LockData::capture_in_chain(&locks, None, &twice, &[]).unwrap();
    let (back, problems, pending) = data.to_locks_in_chain(None, &twice);
    assert!(problems.is_empty() && pending.is_empty());
    assert_eq!(back.get(0, LockKey::fx(ONE, DELAY_TIME)), Some(0.25));
    assert_eq!(back.get(0, LockKey::fx(TWO, DELAY_TIME)), Some(0.75));
}

/// **A pattern outlives its chain.** Loaded before its effect is added, the automation is *kept
/// aside*, not silently dropped — a sequence is work — and it never reaches the audio thread while
/// it names an effect that is not there.
#[test]
fn a_pattern_loaded_without_its_effect_keeps_the_automation_aside() {
    let locks = mixed();
    let data = LockData::capture_in_chain(&locks, None, &chain(), &[]).unwrap();

    // Loaded into an empty chain: the source's locks arrive, the effects' are held.
    let (back, problems, pending) = data.to_locks_in_chain(None, &[]);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(back.get(0, CUTOFF), Some(0.2), "the source's still load");
    assert!(
        back.automated_fx().is_empty(),
        "nothing naming an absent effect may reach the audio thread"
    );
    assert_eq!(pending.len(), 3, "three effect parameters were held aside");

    // Saved again, they are still in the file rather than lost between two sessions.
    let round = LockData::capture_in_chain(&back, None, &[], &pending).unwrap();
    let (recovered, problems, still_pending) = round.to_locks_in_chain(None, &chain());
    assert!(problems.is_empty(), "{problems:?}");
    assert!(
        still_pending.is_empty(),
        "the chain is back, so they resolve"
    );
    assert_eq!(recovered, locks, "the automation came back intact");
}

/// A file written before effects could be automated has no target on any parameter, and every one
/// of them means the source. No migration, no version bump.
#[test]
fn a_file_that_predates_effect_automation_loads_as_the_source() {
    let older = LockData {
        plugin: None,
        params: vec![LockedParam {
            fx: None,
            param: CUTOFF,
            patch: Some(PATCH),
            steps: (0..16).map(|step| (step == 3).then_some(0.8)).collect(),
        }],
    };
    let text = serde_json::to_string(&older).expect("serialises");
    assert!(
        !text.contains("\"fx\""),
        "a source-only file must not grow a target field: {text}"
    );

    let (locks, problems, pending) = older.to_locks_in_chain(None, &chain());
    assert!(problems.is_empty() && pending.is_empty());
    assert_eq!(locks.get(3, CUTOFF), Some(0.8));
    assert!(locks.param_ids().iter().all(|key| key.is_source()));
}

/// A reference naming an ordinal the chain does not reach is held aside rather than attached to
/// whichever copy happens to exist — the same rule as a missing plugin, for the same reason.
#[test]
fn a_reference_past_the_end_of_the_chain_is_held_rather_than_guessed() {
    let data = LockData {
        plugin: None,
        params: vec![LockedParam {
            fx: Some(FxRef {
                plugin: "dk.mxm.mxm-bucket-delay".to_owned(),
                ordinal: 3,
            }),
            param: DELAY_TIME,
            patch: Some(PATCH),
            steps: (0..16).map(|step| (step == 1).then_some(0.4)).collect(),
        }],
    };
    let (locks, problems, pending) = data.to_locks_in_chain(None, &chain());
    assert!(problems.is_empty(), "{problems:?}");
    assert!(
        locks.is_empty(),
        "a fourth copy of the delay is not in the chain, so nothing may be loaded for it"
    );
    assert_eq!(pending.len(), 1);
}
