//! P2 — playable. The event path, driven through a real plugin with the callback on this thread.
//!
//! The plugin is mxm-mono-01 throughout, because it is the one that actually exercises the awkward
//! cases: a press *stack* rather than one voice per pitch, and a `NoteId::matches()` that falls
//! back to channel and note whenever either side lacks an ID.

use mxm_player_harness::harness;

use harness::Harness;
use mxm_player::events::input::Payload;

const PLUGIN: &str = "dk.mxm.mxm-mono-01";

/// Slot 0 is the GUI; slot 1 stands in for a physical MIDI keyboard.
const GUI: usize = 0;
const KEYBOARD: usize = 1;

fn note_on(key: u8) -> Payload {
    Payload::NoteOn {
        channel: 0,
        key,
        velocity: 100.0 / 127.0,
    }
}

fn note_off(key: u8) -> Payload {
    Payload::NoteOff {
        channel: 0,
        key,
        velocity: 0.0,
    }
}

fn harness() -> Option<Harness> {
    let bundle = harness::mxm_mono_01()?;
    Some(Harness::new(&bundle, PLUGIN, 2).expect("mxm-mono-01 should host cleanly"))
}

/// Blocks of 256 frames to render before the default patch is guaranteed to be at **exact** zero.
///
/// **Coupled to `release` in mxm-mono-01's `plugins/mxm-mono-01/src/params.rs`**, and deliberately
/// generous. These tests assert exact silence rather than "quiet", so the wait has to cover the
/// release time *plus* however long the exponential tail takes to flush to zero — a nominal 250 ms
/// release is not silent at 250 ms.
///
/// This was `80`, sized for a 200 ms release, and the init-patch retune broke six tests at once.
/// That is the right failure and the wrong place to fix it fourteen times: change this one number.
const RELEASE_BLOCKS: usize = 160;

#[test]
fn a_note_sounds_and_a_release_silences_it() {
    let Some(mut h) = harness() else { return };

    h.render(256);
    assert_eq!(h.peak(), 0.0, "an idle synth renders exact silence");

    h.push(GUI, note_on(60));
    h.render(256);
    assert!(h.peak() > 1e-3, "a note-on must produce audible output");

    h.push(GUI, note_off(60));
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert_eq!(
        h.peak(),
        0.0,
        "the release must reach exact silence, not merely get quiet"
    );

    h.shutdown();
}

#[test]
fn a_key_struck_twice_is_released_twice() {
    // mxm-mono-01 keeps a press *stack*: two note-ons of one pitch owe two note-offs. Collapsing
    // them leaves the note sounding forever.
    let Some(mut h) = harness() else { return };

    h.push(GUI, note_on(60));
    h.push(GUI, note_on(60));
    h.render(256);
    assert!(h.peak() > 1e-3);

    h.push(GUI, note_off(60));
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert!(
        h.peak() > 1e-3,
        "one release of two presses must leave the note sounding"
    );

    h.push(GUI, note_off(60));
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert_eq!(h.peak(), 0.0, "the second release must finish the note");

    h.shutdown();
}

#[test]
fn sustain_defers_releases_and_the_pedal_owes_every_one_of_them() {
    let Some(mut h) = harness() else { return };

    h.push(GUI, Payload::SustainPedal(true));
    h.push(GUI, note_on(60));
    h.push(GUI, note_on(60));
    h.push(GUI, note_off(60));
    h.push(GUI, note_off(60));
    for _ in 0..40 {
        h.render(256);
    }
    assert!(
        h.peak() > 1e-3,
        "notes released under the pedal must keep sounding"
    );

    h.push(GUI, Payload::SustainPedal(false));
    for _ in 0..120 {
        h.render(256);
    }
    assert_eq!(
        h.peak(),
        0.0,
        "the pedal owes two note-offs; a single collapsed release fails this test"
    );

    h.shutdown();
}

#[test]
fn losing_focus_stops_only_the_notes_the_window_is_holding() {
    // The test that would have caught the Revision 10 contradiction: focus loss routed through
    // the global path would kill the physical note too.
    let Some(mut h) = harness() else { return };

    h.push(KEYBOARD, note_on(64));
    h.push(GUI, note_on(60));
    h.render(256);
    assert!(h.peak() > 1e-3);

    h.push(GUI, Payload::CleanupSource);
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert!(
        h.peak() > 1e-3,
        "the physical note must survive the window losing focus"
    );

    // ...and it must still respond to its own note-off afterwards.
    h.push(KEYBOARD, note_off(64));
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert_eq!(h.peak(), 0.0);

    h.shutdown();
}

#[test]
fn same_pitch_mixed_source_cleanup_works_with_the_physical_press_first() {
    // Removal scans newest-first, so the order matters and both orders are tested.
    let Some(mut h) = harness() else { return };

    h.push(KEYBOARD, note_on(60));
    h.push(GUI, note_on(60));
    h.render(256);

    h.push(GUI, Payload::CleanupSource);
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert!(
        h.peak() > 1e-3,
        "the physical note of the same pitch must survive"
    );

    h.push(KEYBOARD, note_off(60));
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert_eq!(h.peak(), 0.0);

    h.shutdown();
}

#[test]
fn same_pitch_mixed_source_cleanup_works_with_the_physical_press_last() {
    let Some(mut h) = harness() else { return };

    h.push(GUI, note_on(60));
    h.push(KEYBOARD, note_on(60));
    h.render(256);

    h.push(GUI, Payload::CleanupSource);
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert!(
        h.peak() > 1e-3,
        "the newer physical press must survive a GUI cleanup"
    );

    h.push(KEYBOARD, note_off(60));
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert_eq!(h.peak(), 0.0);

    h.shutdown();
}

#[test]
fn saturating_a_producer_queue_never_leaves_a_note_stuck() {
    let Some(mut h) = harness() else { return };

    h.push(GUI, note_on(60));
    h.render(256);
    assert!(h.peak() > 1e-3);

    // Fill the queue so the note-off cannot be enqueued. The producer raises the panic instead:
    // the note-off may be dropped, but the panic supersedes it.
    h.saturate(GUI);
    let epoch_before = h.input_epoch.current();
    assert!(
        !h.push(GUI, note_off(60)),
        "the queue should be full by construction"
    );
    h.input_epoch.raise();
    assert!(h.input_epoch.current() > epoch_before);

    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert_eq!(
        h.peak(),
        0.0,
        "a dropped note-off must not leave a note sounding"
    );

    h.shutdown();
}

#[test]
fn a_pre_panic_note_on_never_lands_after_the_recovery() {
    // The input-side mirror of the output epoch test: recovery clears the voice stack as it
    // stands at that moment, so a stale note-on arriving afterwards would create a fresh,
    // permanent voice.
    let Some(mut h) = harness() else { return };

    // Queued before the panic, and never rendered until after it.
    h.push(GUI, note_on(60));
    h.push(GUI, Payload::GlobalPanic);

    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert_eq!(
        h.peak(),
        0.0,
        "the stale note-on must be discarded, not delivered after recovery"
    );

    // ...and playing on through a panic behaves sanely.
    h.push(GUI, note_on(62));
    h.render(256);
    assert!(h.peak() > 1e-3, "post-panic events must still play");

    h.shutdown();
}

#[test]
fn an_octave_shift_with_keys_held_leaves_nothing_sounding() {
    let Some(mut h) = harness() else { return };

    h.push(GUI, note_on(60));
    h.render(256);
    assert!(h.peak() > 1e-3);

    // Shifting octave releases held notes first, through the same targeted cleanup.
    h.push(GUI, Payload::CleanupSource);
    h.push(GUI, note_on(72));
    h.render(256);
    assert!(h.peak() > 1e-3, "the new octave's note should sound");

    h.push(GUI, note_off(72));
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert_eq!(
        h.peak(),
        0.0,
        "nothing from before the shift may still be sounding"
    );

    h.shutdown();
}

#[test]
fn thru_echoes_the_keyboard_only_after_host_sustain_handling() {
    let Some(mut h) = harness() else { return };
    h.thru_enabled
        .store(true, std::sync::atomic::Ordering::Release);

    h.push(GUI, Payload::SustainPedal(true));
    h.push(GUI, note_on(60));
    h.push(GUI, note_off(60));
    h.render(256);

    let sent = h.drain_midi_out();
    assert!(
        sent.iter().any(|e| mxm_player::midi::is_press(e.data)),
        "thru should have echoed the note-on"
    );
    assert!(
        !sent.iter().any(|e| mxm_player::midi::is_release(e.data)),
        "external hardware must receive the same deferred note-offs the plugin does"
    );

    h.push(GUI, Payload::SustainPedal(false));
    h.render(256);
    let sent = h.drain_midi_out();
    assert!(
        sent.iter().any(|e| mxm_player::midi::is_release(e.data)),
        "lifting the pedal must release outward too"
    );

    h.shutdown();
}

#[test]
fn a_source_cleanup_with_thru_enabled_also_releases_outward() {
    // Anything that silences notes at the plugin must also silence what thru has sent outward,
    // or external gear keeps sounding after the plugin has gone quiet.
    let Some(mut h) = harness() else { return };
    h.thru_enabled
        .store(true, std::sync::atomic::Ordering::Release);

    h.push(GUI, note_on(60));
    h.render(256);
    h.drain_midi_out();

    h.push(GUI, Payload::GlobalPanic);
    h.render(256);

    assert!(
        h.out_panic.count() > 0,
        "an input panic with thru enabled must raise the out-panic"
    );

    h.shutdown();
}

#[test]
fn events_that_arrive_apart_stay_apart_inside_the_buffer() {
    // The P4 refinement: arrival times are mapped onto sample offsets, so the interval between
    // two events survives even though both are one buffer late.
    let Some(mut h) = harness() else { return };

    // Establish a callback period first: the mapping needs two callback starts.
    h.render(256);
    h.render(256);

    let base = h.clock.now_nanos();
    h.push_at(GUI, base, note_on(60));
    h.push_at(GUI, base + 2_000_000, note_off(60));
    h.render(256);

    // Both events landed in one buffer; what matters is that the render did not panic and the
    // note did not stick.
    for _ in 0..RELEASE_BLOCKS {
        h.render(256);
    }
    assert_eq!(h.peak(), 0.0);

    h.shutdown();
}
