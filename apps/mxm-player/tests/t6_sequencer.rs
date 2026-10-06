//! The sequencer, measured where it matters.
//!
//! Timing properties live in unit tests beside the clock, where they can be exercised at hundreds
//! of tempos without a plugin. What is here is what only the whole player can answer: that the
//! notes reach a real synth, that the transport controls do what they say, that nothing is left
//! sounding, and that the interface a person actually touches drives all of it.

use mxm_player_harness::app_harness;

use app_harness::AppHarness;
use kittest::Queryable;
use mxm_player::sequencer::{self, Transport};
use mxm_player::session::Session;
use mxm_player::ui::SELECTED_KEY;
use std::path::PathBuf;

const PLUGIN: &str = "dk.mxm.mxm-mono-01";

fn session(name: &str) -> Option<Session> {
    let dir = app_harness::bundled_dir()?;
    let file = dir.join("mxm-mono-01.clap");
    let mut session = Session::scratch(name, vec![dir]);
    session.load(&file, PLUGIN);
    Some(session)
}

use mxm_measure::channels::left;

/// Peak magnitude of a capture.
///
/// **A shim over `mxm-measure`, and the `expect` is the point.** The shared ruler reports absence for
/// a **non-finite** buffer rather than the largest number in it, because `f32::max` would otherwise
/// let a render that is half NaN measure as perfectly healthy — and then pass every "is it quiet?"
/// assertion below. Panicking here is the loud failure that behaviour deserves.
fn peak(samples: &[f32]) -> f32 {
    mxm_measure::level::peak(samples).expect("the capture is finite")
}

/// Where the signal restarts, in frames.
///
/// Measured on a **windowed envelope**, not on raw samples: a sawtooth crosses zero once per
/// cycle, so a sample-by-sample detector reports an onset every ~180 frames at C3 and finds
/// hundreds of notes in a bar. The window has to be shorter than the gap being measured and
/// longer than a cycle of the lowest note.
const ENVELOPE_WINDOW: usize = 128;

/// Where the signal starts again after falling silent.
///
/// Reliable only when the notes are actually separated, which is what [`sharpen`] arranges. Trying
/// to infer re-attacks from a sustained sawtooth is a heuristic that reports either every cycle or
/// every sixth note depending on how it is tuned; making the signal easy to measure is the better
/// trade.
///
/// **All of this stays local, envelope included.** A windowed peak envelope looked like a shared
/// primitive until the second candidate consumer was examined: `mxm-shimmer`'s preset audit windows
/// a *stereo* buffer for energy, not a mono one for peaks, so the two are different quantities and
/// this is the only caller. Deciding what counts as a restart — this window, this floor, a gate that
/// reopens at a fifth of it — was never shareable anyway. `crates/mxm-measure/AGENTS.md`'s declined
/// register records both.
fn onsets(samples: &[f32], floor: f32) -> Vec<usize> {
    let envelope: Vec<f32> = samples.chunks(ENVELOPE_WINDOW).map(peak).collect();
    let mut found = Vec::new();
    let mut quiet = true;
    for (index, level) in envelope.iter().enumerate() {
        if quiet && *level > floor {
            found.push(index * ENVELOPE_WINDOW);
            quiet = false;
        } else if !quiet && *level < floor * 0.2 {
            quiet = true;
        }
    }
    found
}

/// Gives the patch a fast attack and a short release, so each step is a distinct blip.
///
/// Driven through the control map — CC 73 is Attack and CC 72 is Release — which is the same path
/// a hardware knob takes. The full sweep is there to engage pickup, which deliberately refuses to
/// jump a parameter until the control has caught up with it.
fn sharpen(session: &mut Session) {
    for cc in [73u8, 72] {
        for value in [0u8, 32, 64, 96, 127, 0] {
            session.app().send_control_change(cc, value);
            session.advance_blocks(1).expect("advance");
        }
    }
    session.clear_capture();
}

// --- through a real synth -----------------------------------------------------------------------

#[test]
fn the_sequencer_plays_a_pattern_through_the_plugin() {
    let Some(mut session) = session("seq-plays") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(120.0);
    for step in 0..16 {
        session.app().toggle_step_note(step, 60);
    }
    session.app().play_from_start();

    // A bar at 120 BPM is 96,000 frames; 512 per block.
    session.advance_blocks(200).expect("advance");

    assert!(
        peak(&session.captured()) > 0.01,
        "the sequencer should have made a sound"
    );
}

#[test]
fn steps_arrive_at_the_interval_the_tempo_says() {
    let Some(mut session) = session("seq-interval") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // 120 BPM: a 16th is 6,000 frames at 48 kHz.
    session.app().set_tempo(120.0);
    for step in 0..16 {
        session.app().toggle_step_note(step, 60);
    }
    sharpen(&mut session);
    session.app().play_from_start();
    session.advance_blocks(120).expect("advance");

    let mono = left(&session.captured());
    let onsets = onsets(&mono, 0.02);
    assert!(onsets.len() >= 4, "expected several notes, got {onsets:?}");

    // The gate is half a step, so onsets are one step apart, not half.
    for pair in onsets.windows(2) {
        let gap = pair[1] - pair[0];
        // Accurate to the envelope window either way, which is what the tolerance allows for.
        assert!(
            (6_000 - ENVELOPE_WINDOW * 2..=6_000 + ENVELOPE_WINDOW * 2).contains(&gap),
            "steps were {gap} frames apart; a 16th at 120 BPM is 6000"
        );
    }
}

#[test]
fn a_faster_tempo_puts_the_steps_closer_together() {
    let Some(mut session) = session("seq-faster") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(240.0);
    for step in 0..16 {
        session.app().toggle_step_note(step, 60);
    }
    sharpen(&mut session);
    session.app().play_from_start();
    session.advance_blocks(120).expect("advance");

    let onsets = onsets(&left(&session.captured()), 0.02);
    assert!(onsets.len() >= 6, "expected more notes when faster");
    for pair in onsets.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            (3_000 - ENVELOPE_WINDOW * 2..=3_000 + ENVELOPE_WINDOW * 2).contains(&gap),
            "at 240 BPM a 16th is 3000 frames, got {gap}"
        );
    }
}

#[test]
fn a_rest_does_not_hang_the_sequencer() {
    // The blocker the review found, end to end: a pattern that rests for fifteen steps is exactly
    // what sleeps the plugin, and a playhead tied to the plugin's run state would freeze there.
    let Some(mut session) = session("seq-rest") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(240.0);
    session.app().toggle_step_note(0, 60); // one note, fifteen rests
    session.app().play_from_start();

    // Two full bars at 240 BPM: 16 * 3000 * 2 frames.
    session
        .advance_blocks(2 * 16 * 3000 / 512)
        .expect("advance");

    let onsets = onsets(&left(&session.captured()), 0.02);
    assert!(
        onsets.len() >= 2,
        "step 1 should have come round again after fifteen rests; onsets: {onsets:?}"
    );
}

#[test]
fn stopping_silences_the_sequencer() {
    let Some(mut session) = session("seq-stop") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(120.0);
    for step in 0..16 {
        session.app().toggle_step_note(step, 60);
    }
    session.app().play_from_start();
    session.advance_blocks(40).expect("advance");
    assert!(peak(&session.captured()) > 0.01, "it should be sounding");

    session.app().stop_sequencer();
    // Long enough for mxm-mono-01's release to run out; the point is that nothing is *held*, not that
    // the tail is instant.
    session.advance_blocks(300).expect("settle the release");
    session.clear_capture();
    session.advance_blocks(60).expect("listen");

    assert!(
        peak(&session.captured()) < 0.001,
        "stop must leave nothing sounding, peak was {}",
        peak(&session.captured())
    );
}

#[test]
fn pausing_silences_the_sequencer_and_keeps_its_place() {
    let Some(mut session) = session("seq-pause") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(120.0);
    for step in 0..16 {
        session.app().toggle_step_note(step, 60);
    }
    session.app().play_from_start();
    session.advance_blocks(30).expect("into step 3ish");

    let before = session.state().sequencer.step;
    session.app().pause();
    session.advance_blocks(300).expect("settle the release");
    session.clear_capture();
    session.advance_blocks(60).expect("listen");

    assert!(
        peak(&session.captured()) < 0.001,
        "pause must leave nothing sounding"
    );
    assert_eq!(
        session.state().sequencer.step,
        before,
        "pause holds its place"
    );
}

#[test]
fn the_playhead_the_gui_reads_is_the_one_the_audio_thread_wrote() {
    let Some(mut session) = session("seq-playhead") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(120.0);
    for step in 0..16 {
        session.app().toggle_step_note(step, 60);
    }

    assert_eq!(session.state().sequencer.transport, "stopped");

    session.app().play_from_start();
    session.advance_blocks(4).expect("advance");
    assert_eq!(session.state().sequencer.transport, "playing");

    // Four steps in at 120 BPM: 4 * 6000 frames.
    session.advance_blocks(4 * 6000 / 512).expect("advance");
    let step = session.state().sequencer.step;
    assert!((3..=5).contains(&step), "expected step 4ish, got {step}");
}

#[test]
fn a_random_bar_sounds_and_stays_in_c_dorian() {
    let Some(mut session) = session("seq-random") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(240.0);
    session.app().randomise();

    for names in &session.state().sequencer.steps {
        for name in names {
            let note = sequencer::pattern::parse_note(name).expect("a real note");
            assert!(
                sequencer::random::is_in_c_dorian(note),
                "{name} is outside C Dorian"
            );
        }
    }

    session.app().play_from_start();
    session.advance_blocks(200).expect("advance");
    assert!(peak(&session.captured()) > 0.01, "it should have sounded");
}

#[test]
fn a_tempo_drag_never_delays_stopping_the_engine() {
    // The publication bound, end to end. Without it a frame-rate tempo drag fills the 64-slot
    // command queue with stale states and the `Stop` that `stop_now` polls for waits behind them.
    let Some(mut session) = session("seq-flood") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().play_from_start();
    for tick in 0..500 {
        session.app().set_tempo(60.0 + f64::from(tick % 200));
        session.app().service();
    }

    let started = std::time::Instant::now();
    session.app().rescan(); // goes through Engine::stop_now
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "stopping took {:?}, so the queue was crowded",
        started.elapsed()
    );
}

// --- saving -------------------------------------------------------------------------------------

#[test]
fn a_sequence_saves_and_loads_back() {
    let Some(mut session) = session("seq-save") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(137.0);
    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(7, 67);
    let before = session.state().sequencer.steps.clone();

    let path = session.app().save_sequence("test one").expect("save");

    session.app().clear_pattern();
    assert_ne!(session.state().sequencer.steps, before, "cleared");

    session.app().load_sequence(&path).expect("load");
    assert_eq!(session.state().sequencer.steps, before, "restored");
    assert_eq!(session.state().sequencer.tempo, 137.0);
}

#[test]
fn a_saved_sequence_plays_the_same_way_it_was_saved() {
    let Some(mut session) = session("seq-save-plays") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(240.0);
    for step in 0..16 {
        session.app().toggle_step_note(step, 60);
    }
    let path = session.app().save_sequence("playable").expect("save");

    session.app().clear_pattern();
    session.app().load_sequence(&path).expect("load");

    sharpen(&mut session);
    session.app().play_from_start();
    session.advance_blocks(120).expect("advance");

    let onsets = onsets(&left(&session.captured()), 0.02);
    assert!(
        onsets.len() >= 6,
        "a loaded sequence should play: {onsets:?}"
    );
}

#[test]
fn a_sequence_carries_no_patch_and_names_an_instrument_only_when_it_must() {
    // What lets the same test sequence be run through two different synths and compared.
    let Some(mut session) = session("seq-portable") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    session.app().toggle_step_note(0, 60);
    let path = session.app().save_sequence("portable").expect("save");

    let text = std::fs::read_to_string(&path).expect("read");
    assert!(!text.contains(PLUGIN), "a plugin id leaked in:\n{text}");
    assert!(text.contains("C3"), "notes should be readable:\n{text}");

    // **A sequence holds no patch** - no state blob, no starting value for anything. Locks are the
    // one thing it carries that is bound to an instrument, and they are part of the *line* rather
    // than the patch: an acid line is a note pattern and a filter moving under it. So the id is
    // written when there are locks to tag, and it is the only thing about the instrument that is.
    let param = a_param(&mut session);
    session.app().select_step(0);
    session.app().set_parameter(param, 0.6);
    session.app().deselect_step();
    let path = session.app().save_sequence("sequenced").expect("save");

    let text = std::fs::read_to_string(&path).expect("read");
    assert!(
        text.contains(PLUGIN),
        "locks must say which instrument they were recorded for:\n{text}"
    );
    // It carries **one patch value per sequenced parameter**, and has to: a deviation is
    // meaningless without the thing it deviates from, and the steps that do not set a parameter put
    // it back to exactly this. What it still does not carry is a patch — no state blob, and nothing
    // at all about the parameters the sequence never touches.
    assert!(
        !text.contains("\"state\""),
        "no state blob belongs in a sequence:\n{text}"
    );
    let sequenced = text.matches("\"patch\"").count();
    assert_eq!(
        sequenced, 1,
        "one patch value, for the one parameter a step sets:\n{text}"
    );
}

#[test]
fn a_malformed_sequence_file_is_refused_and_leaves_the_pattern_alone() {
    let Some(mut session) = session("seq-malformed") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().toggle_step_note(3, 64);
    let before = session.state().sequencer.steps.clone();

    let bad: PathBuf = session.dir().join("broken.seq.json");
    std::fs::write(&bad, "{ not a sequence").unwrap();

    assert!(session.app().load_sequence(&bad).is_err());
    assert_eq!(
        session.state().sequencer.steps,
        before,
        "a bad file must not disturb what is loaded"
    );
}

// --- the interface ------------------------------------------------------------------------------

#[test]
fn one_button_toggles_between_play_and_stop() {
    let mut harness = AppHarness::new("seq-toggle", Vec::new());
    harness.run();

    // One control, whose label reads as what pressing it does.
    harness.harness.get_by_label("\u{25b6} Play").click();
    harness.run();
    assert_eq!(
        harness.app().sequencer_state().transport,
        Transport::Playing
    );

    harness.harness.get_by_label("\u{23f9} Stop").click();
    harness.run();
    assert_eq!(
        harness.app().sequencer_state().transport,
        Transport::Stopped,
        "the other half of a Play that always rewinds is Stop, not Pause"
    );

    harness.harness.get_by_label("\u{25b6} Play").click();
    harness.run();
    assert_eq!(
        harness.app().sequencer_state().transport,
        Transport::Playing
    );
}

#[test]
fn play_always_rewinds_and_there_is_no_second_button_that_does_it() {
    // There used to be a *From start* button, and Play resumed where it left off -- while the
    // spacebar, the same gesture, rewound. Two controls whose difference was invisible until you
    // pressed one, and a keyboard shortcut that agreed with neither.
    let mut harness = AppHarness::new("seq-rewind", Vec::new());
    harness.run();

    assert!(
        harness
            .harness
            .query_by_label("\u{23ee} From start")
            .is_none(),
        "the second button is gone"
    );

    let before = harness.app().sequencer_state().generation;
    harness.harness.get_by_label("\u{25b6} Play").click();
    harness.run();
    let first = harness.app().sequencer_state().generation;
    assert!(first > before, "Play restarts the run");
    assert_eq!(
        harness.app().sequencer_state().transport,
        Transport::Playing
    );

    // Stop, then Play again: still from the top, which is the whole point of the change.
    harness.harness.get_by_label("\u{23f9} Stop").click();
    harness.run();
    assert_eq!(
        harness.app().sequencer_state().transport,
        Transport::Stopped
    );

    harness.harness.get_by_label("\u{25b6} Play").click();
    harness.run();
    assert!(
        harness.app().sequencer_state().generation > first,
        "and again after a pause, rather than resuming in place"
    );
}

#[test]
fn the_random_button_fills_the_shown_bar() {
    let mut harness = AppHarness::new("seq", Vec::new());
    harness.run();
    assert!(harness.app().pattern().is_empty());

    harness.harness.get_by_label("Random").click();
    harness.run();

    assert!(
        !harness.app().pattern().is_empty(),
        "Random should have written a bar"
    );
}

// **There is deliberately no audio test for a tie to the *same* pitch here, and that is a
// finding rather than an omission.**
//
// `emit_legato_joint` exists because `PressTable::take_exact` resolves a release with `rposition`
// -- the newest matching press -- so a tie carrying a pitch that is already sounding would release
// the press it had just made. The obvious test is "assert it does not leave a stuck note", and it
// passes with the fix removed, because nothing sticks: the runtime emits `Sound`, `Sound` +
// `Release` at the joint, then `Release` at the following step start. Two note-ons and two
// note-offs either way. Only *which* press each note-off names differs, and with equal pitches
// that is inaudible.
//
// So the guarantee is asserted where it is real and observable -- `a_release_resolves_to_the_newest
// _press_of_that_key` in `src/events/press.rs`, which is the hazard itself -- and not pretended to
// be acoustic here. An oracle that cannot fail is not an oracle.

#[test]
fn a_tied_run_sounds_without_a_gap_where_an_untied_pair_has_one() {
    // The audible point of a tie, as a comparison rather than an absolute: the same two steps
    // with and without the tie, measured the same way. An absolute threshold here would be a
    // number about mxm-mono-01's envelope; the difference between the two is about the tie.
    let Some(mut tied) = session("seq-tie-gap-tied") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let Some(mut untied) = session("seq-tie-gap-untied") else {
        return;
    };

    for (session, tie) in [(&mut tied, true), (&mut untied, false)] {
        session.app().set_tempo(120.0);
        session.app().toggle_step_note(0, 60);
        if tie {
            session.app().toggle_step_tie(1);
        }
        sharpen(session);
        session.app().play_from_start();
        session.advance_blocks(24).expect("advance"); // two steps
    }

    // The second half of step 1 is where an untied note has already stopped and a tied one has
    // not. 6,000..12,000 frames, mono.
    let window = |session: &mut Session| -> f32 {
        let mono = left(&session.captured());
        let half = mono[6_000.min(mono.len())..12_000.min(mono.len())].to_vec();
        peak(&half)
    };

    let with_tie = window(&mut tied);
    let without = window(&mut untied);
    assert!(
        without < 1e-4,
        "an untied note must be gone by the second half of its step, measured {without}"
    );
    assert!(
        with_tie > 0.01,
        "a tie must keep it sounding through that same window, measured {with_tie}"
    );
}

/// Sign changes in a window — a coarse pitch proxy. A saw crosses zero twice per cycle, so a
/// note an octave up roughly doubles the count; the assertion is the ratio between two
/// renditions, never an absolute figure about mxm-mono-01's waveform.
fn sign_changes(samples: &[f32]) -> usize {
    samples
        .windows(2)
        .filter(|pair| (pair[0] >= 0.0) != (pair[1] >= 0.0))
        .count()
}

#[test]
fn a_slide_is_audible_as_a_slide_on_the_rendered_audio() {
    // §9 of the plan: pitch at the front of the following note, measured on audio, not read back
    // from the pattern. Two renditions falsify the oracle against each other — a hold keeps the
    // old pitch through the same window a slide changes it in, so a measurement that could not
    // tell them apart would fail here rather than passing forever.
    let Some(mut slide) = session("seq-slide-audio") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let Some(mut hold) = session("seq-slide-audio-hold") else {
        return;
    };

    for (session, slides) in [(&mut slide, true), (&mut hold, false)] {
        session.app().set_tempo(120.0);
        session.app().toggle_step_note(0, 48); // C2
        if slides {
            session.app().toggle_step_note(1, 60); // C3, an octave up...
            session.app().toggle_step_tie(1); // ...under the same open gate
        } else {
            session.app().toggle_step_tie(1); // a hold: the C2 continues
        }
        sharpen(session);
        session.app().play_from_start();
        session.advance_blocks(24).expect("advance"); // two steps
    }

    // Inside the slide's own gate: step 2 runs 6,000..12,000 frames at 120 BPM and its note gates
    // to the halfway point, so 6,600..8,800 is sounding in both renditions.
    let pitch_window = |session: &mut Session| -> Vec<f32> {
        let mono = left(&session.captured());
        mono[6_600.min(mono.len())..8_800.min(mono.len())].to_vec()
    };
    let slid = pitch_window(&mut slide);
    let held = pitch_window(&mut hold);
    let slid_crossings = sign_changes(&slid);
    let held_crossings = sign_changes(&held);
    assert!(
        held_crossings > 0,
        "the hold must be sounding in the measured window, or this proves nothing"
    );
    assert!(
        slid_crossings as f64 > held_crossings as f64 * 1.5,
        "an octave slide must roughly double the zero-crossing rate: slide {slid_crossings}, \
         hold {held_crossings}"
    );

    // And the slide's gate is a note's gate: past its 50% close the slide is quiet while the hold
    // — whose gate stays open to the next step start — is not.
    let tail = |session: &mut Session| -> f32 {
        let mono = left(&session.captured());
        peak(&mono[10_000.min(mono.len())..11_800.min(mono.len())])
    };
    let slide_tail = tail(&mut slide);
    let hold_tail = tail(&mut hold);
    assert!(
        hold_tail > 0.01,
        "the hold must still sound past the slide's gate point, measured {hold_tail}"
    );
    assert!(
        slide_tail < hold_tail * 0.2,
        "a slide's own note closes at the ordinary gate: slide tail {slide_tail}, hold tail {hold_tail}"
    );
}

/// Every bar the step row paints to say "this step holds notes".
///
/// The marker is `SPACE_2` tall and never anything else, which is what tells it apart from the
/// step backgrounds and the run outlines in a painted row that has no widget per mark.
fn note_markers(harness: &AppHarness) -> Vec<app_harness::PaintedRect> {
    harness
        .painted_rects()
        .into_iter()
        .filter(|r| (r.rect.height() - mxm_ui::space::SPACE_2).abs() < 0.5)
        .collect()
}

#[test]
fn a_tie_that_holds_no_note_paints_no_marker() {
    // **Reported from a screenshot.** Clear the pattern, then tie the first step: a full-width bar
    // appeared, promising a long note, with nothing anywhere in the pattern to sound.
    //
    // It was structural, not a slip. The marker's width was the note's *duration*, so a tie drew
    // one whether or not there was a note to continue -- and the first step, which continues the
    // previous pass through a pattern that is empty, continues nothing at all. The bar now follows
    // the notes and only the notes; duration is the merged button's business.
    let mut harness = AppHarness::new("seq-empty-tie", Vec::new());
    harness.run();

    harness.harness.get_by_label("Step 1: empty").click();
    harness.run();
    harness.harness.get_by_label("Step 1: empty").click();
    harness.run();

    assert!(
        harness.app().pattern().tied(0),
        "the premise: the step really is tied"
    );
    assert!(
        harness.app().pattern().is_empty(),
        "the premise: and the pattern holds no notes at all"
    );
    assert!(
        note_markers(&harness).is_empty(),
        "a tie continuing nothing must not paint a marker: {:?}",
        note_markers(&harness).len()
    );
}

#[test]
fn a_step_with_notes_paints_one_marker_whether_or_not_it_is_tied() {
    // The other half: the marker follows the notes, so tying a step must neither add one nor take
    // one away. Without that the two channels are still entangled, just differently.
    let mut harness = AppHarness::new("seq-marker-follows-notes", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.run();
    assert_eq!(note_markers(&harness).len(), 1, "one note, one marker");

    harness.app().toggle_step_tie(1);
    harness.run();
    assert_eq!(
        note_markers(&harness).len(),
        1,
        "tying the step after it must not paint a second marker"
    );

    harness.app().toggle_step_note(0, 60);
    harness.run();
    assert!(
        note_markers(&harness).is_empty(),
        "and taking the note away must take the marker with it"
    );
    assert!(
        harness.app().pattern().tied(1),
        "and the tie stays exactly as authored — notes and ties are separate facts, so deleting          one never rewrites the other"
    );
}

#[test]
fn a_note_played_into_a_tied_step_lands_there_and_makes_a_slide() {
    // **The owner's Option A: the redirect is gone.** Tie step 2 to step 1, select step 2, play a
    // note — and the pitch lands on step 2, turning the hold into a slide: the gate stays open,
    // the pitch moves. To repitch the held note, select its head.
    let mut harness = AppHarness::new("seq-tied-entry", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.app().toggle_step_tie(1);
    harness.run();

    harness.app().select_step(1);
    harness.app().note_on(67, 0.8);
    harness.run();

    assert!(
        harness.app().pattern().step(1).contains(67),
        "the pitch lands on the step itself — a slide"
    );
    assert!(
        harness.app().pattern().step(0).contains(60),
        "the head keeps its own note"
    );
    assert!(harness.app().pattern().tied(1), "and the tie is untouched");
}

#[test]
fn a_note_lands_on_the_selected_step_however_many_ties_deep_it_is() {
    // A hold deep in a run still has a head to slide from — the walk passes every empty tie — so
    // the pitch lands there, and the steps between stay continuations.
    let mut harness = AppHarness::new("seq-tied-entry-deep", Vec::new());
    harness.run();

    harness.app().toggle_step_note(4, 60);
    for step in 5..=7 {
        harness.app().toggle_step_tie(step);
    }
    harness.run();

    harness.app().select_step(7);
    harness.app().note_on(72, 0.8);
    harness.run();

    assert!(
        harness.app().pattern().step(7).contains(72),
        "the pitch lands on the selected step — a slide three holds deep"
    );
    for step in 5..=6 {
        assert!(
            harness.app().pattern().step(step).is_empty(),
            "step {step} is a continuation, not a note"
        );
    }
    assert!(
        harness.app().pattern().step(4).contains(60),
        "the head keeps its note"
    );
}

#[test]
fn a_note_into_a_tie_with_no_head_lands_and_sounds_from_there() {
    // **Ties and notes are separate** — the owner's ruling. A pitch played into a tie hanging off
    // a rest used to be refused, on the reasoning that a slide needs something to slide from. It
    // lands now: the steps before it are silent, this note starts the sound, and the run carries
    // it on. What a pattern means is the runtime's to decide, not the editor's to forbid.
    let mut harness = AppHarness::new("seq-slide-no-head", Vec::new());
    harness.run();

    harness.app().force_tie_for_test(3); // step 4 tied; steps 1-3 are rests
    harness.app().select_step(3);
    harness.app().note_on(67, 0.8);
    harness.run();

    assert!(
        harness.app().pattern().step(3).contains(67),
        "the note lands where it was played"
    );
    assert!(
        harness.app().pattern().tied(3),
        "and the tie it was played into is untouched"
    );
}

#[test]
fn deleting_a_head_leaves_the_slide_that_continued_it_exactly_where_it_was() {
    // The head-protection guard is gone with the invariant it served: deleting a note a slide
    // hung off used to be refused, naming the step in the way. Nothing is refused now — the slide
    // keeps its own note and its own tie and simply has nothing in front of it to continue, which
    // the runtime plays by starting the sound there.
    let mut harness = AppHarness::new("seq-head-guard", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.app().toggle_step_tie(1);
    harness.app().select_step(1);
    harness.app().note_on(67, 0.8); // a slide on step 2
    harness.run();

    harness.app().toggle_step_note(0, 60); // delete the head's note
    harness.run();
    assert!(
        harness.app().pattern().step(0).is_empty(),
        "the head goes, unguarded"
    );
    assert!(
        harness.app().pattern().step(1).contains(67),
        "the slide keeps its note"
    );
    assert!(
        harness.app().pattern().tied(1),
        "and its tie: an edit never rewrites a step the gesture did not name"
    );
}

#[test]
fn a_midi_note_played_into_a_tied_step_lands_in_the_same_place() {
    // The three input paths must not diverge -- the README calls all of them first-class, and MIDI
    // is the one that reaches the pattern by a different route. All land on the step itself now.
    let mut harness = AppHarness::new("seq-tied-entry-midi", Vec::new());
    harness.run();

    harness.app().toggle_step_note(2, 60);
    harness.app().toggle_step_tie(3);
    harness.run();

    let shared = harness.app().engine_mut().shared().clone();
    harness.app().select_step(3);
    shared.set_midi_sounding(64, true);
    harness.run();

    assert!(
        harness.app().pattern().step(3).contains(64),
        "MIDI lands on the step itself, exactly as the on-screen and computer keyboards do"
    );
    assert!(
        harness.app().pattern().step(2).contains(60),
        "the head keeps its own note"
    );
}

#[test]
fn clear_empties_the_pattern() {
    let mut harness = AppHarness::new("seq", Vec::new());
    harness.run();
    harness.harness.get_by_label("Random").click();
    harness.run();
    assert!(!harness.app().pattern().is_empty());

    harness.harness.get_by_label("Clear").click();
    harness.run();
    assert!(harness.app().pattern().is_empty());
}

#[test]
fn clicking_the_empty_space_beside_the_steps_stops_editing() {
    // Escape already did this and is hard to remember; clicking away is what people try first.
    // `deselect_step` is the same call both reach, so this is a second door onto one decision.
    let mut harness = AppHarness::new("seq-click-away", Vec::new());
    harness.run();

    harness.harness.get_by_label("Step 1: empty").click();
    harness.run();
    assert_eq!(
        harness.app().selected_step(),
        Some(0),
        "a step is being edited"
    );

    // To the right of the last step button, in the same band: literally the area outside one of
    // them. Derived from the button's own rect rather than hard-coded, so re-laying out the row
    // cannot quietly turn this into a click on nothing in particular.
    let last = harness.harness.get_by_label("Step 16: empty").rect();
    harness.click_at(egui::pos2(last.right() + 40.0, last.center().y));

    assert_eq!(
        harness.app().selected_step(),
        None,
        "clicking the panel's empty space should stop editing the step"
    );
}

#[test]
fn a_control_in_the_sequencer_panel_is_not_swallowed_by_the_deselect_background() {
    // The deselect area covers the whole panel, so every control drawn on top of it has to keep
    // taking its own clicks. This is the failure the background risks: it is invisible, it spans
    // everything, and if the ordering ever inverts the symptom is controls that stop responding
    // rather than anything that looks like a bug in this feature.
    let mut harness = AppHarness::new("seq-panel-controls", Vec::new());
    harness.run();

    harness.harness.get_by_label("Step 3: empty").click();
    harness.run();
    assert_eq!(harness.app().selected_step(), Some(2));

    harness.harness.get_by_label("▶ Play").click();
    harness.run();

    assert_eq!(
        harness.app().sequencer_state().transport,
        Transport::Playing,
        "the transport button still takes its own click"
    );
    assert_eq!(
        harness.app().selected_step(),
        Some(2),
        "and pressing it does not stop editing the step"
    );
}

#[test]
fn a_knob_turned_on_a_tied_step_writes_that_step() {
    // **Locks are per step — the owner's ruling.** A knob turned while a hold is selected writes
    // the hold itself: the modulation walks under the held note, which is the point, and the dot
    // draws exactly where the value is heard.
    let mut harness = AppHarness::new("seq-tied-lock", Vec::new());
    harness.run();

    // Steps 1 to 4 are one run: the note is on step 1 and 2, 3 and 4 continue it.
    harness.app().toggle_step_note(0, 60);
    for step in [1, 2, 3] {
        harness.app().toggle_step_tie(step);
    }
    harness.app().select_step(3);
    harness.run();

    harness.app().set_parameter(42, 0.75);

    assert_eq!(
        harness.app().sequencer_state().locks.get(3, 42),
        Some(0.75),
        "the lock belongs to the selected step, tied or not"
    );
    assert_eq!(
        harness.app().sequencer_state().locks.get(0, 42),
        None,
        "and nothing was redirected to the head"
    );
}

#[test]
fn tying_a_step_keeps_its_notes_and_its_locks() {
    // Tying a note-carrying step makes it a **slide**: its notes and locks belong to a run it now
    // heads, so tying empties nothing. The old rule — a tied step is part of the note before it
    // and contributes nothing — defended the invariant this change removed.
    let mut harness = AppHarness::new("seq-tie-empties", Vec::new());
    harness.run();

    harness.app().toggle_step_note(4, 60); // a head for the slide to reach
    harness.app().toggle_step_note(5, 64);
    harness.app().select_step(5);
    harness.run();
    harness.app().set_parameter(42, 0.6);
    assert_eq!(harness.app().sequencer_state().locks.get(5, 42), Some(0.6));

    harness.app().toggle_step_tie(5);

    assert!(
        harness.app().pattern().step(5).contains(64),
        "tying keeps its notes — it is a slide now"
    );
    assert_eq!(
        harness.app().sequencer_state().locks.get(5, 42),
        Some(0.6),
        "and what it set"
    );
}

#[test]
fn tying_a_note_with_nothing_sounding_before_it_is_written_as_asked() {
    // The slide-needs-a-head guard is gone from the tie path too, and for the same reason: a tie
    // is a fact about a step, not a claim about the step before it. Authoring one over silence
    // is how a pattern gets built out of order — the ties first, the notes after.
    let mut harness = AppHarness::new("seq-tie-no-head", Vec::new());
    harness.run();

    harness.app().toggle_step_note(3, 64); // steps 1-3 are rests
    harness.app().toggle_step_tie(3);
    harness.run();

    assert!(
        harness.app().pattern().tied(3),
        "the tie is written where it was asked for"
    );
    assert!(
        harness.app().pattern().step(3).contains(64),
        "and the note it sits on is untouched"
    );

    // Give it something to continue, and nothing about the step has to be re-authored.
    harness.app().toggle_step_note(2, 60);
    harness.run();
    assert!(
        harness.app().pattern().tied(3) && harness.app().pattern().step(2).contains(60),
        "the note in front arrives later and the run simply becomes audible"
    );
}

#[test]
fn an_export_reads_each_steps_own_locks_exactly_as_playback_does() {
    // `export.rs`'s own contract: *"the pattern would sound one way live and another way rendered,
    // which is the one thing export must never do."* Locks are per step in both paths now — live
    // and rendered agree by reading the same cell, with nothing resolved through a run head.
    use mxm_player::sequencer::export;
    use mxm_player::sequencer::locks::LockSet;
    use mxm_player::sequencer::pattern::Pattern;

    let mut pattern = Pattern::empty();
    pattern.toggle(4, 60);
    for step in [5, 6, 7] {
        pattern.set_tied(step, true);
    }
    let mut locks = LockSet::EMPTY;
    locks.set(4, 42, 0.9, 0.2).expect("room");
    locks.set(6, 42, 0.4, 0.2).expect("room"); // a hold with a lock of its own

    let scheduled = export::schedule_locks(&pattern, &locks, 120.0, 48_000.0);

    // One offset per step, in step order, plus the zero at the bar's end.
    let per_step: Vec<f32> = scheduled
        .iter()
        .filter(|(_, id, _)| *id == 42)
        .map(|(_, _, offset)| *offset)
        .take(16)
        .collect();

    for (step, offset) in per_step.iter().enumerate() {
        let expected: f32 = match step {
            4 => 0.7,
            6 => 0.2,
            _ => 0.0,
        };
        assert!(
            (offset - expected).abs() < 1e-6,
            "rendered step {} must offset by {expected}, not {offset} — live playback does",
            step + 1
        );
    }
}

#[test]
fn lengthening_a_note_onto_a_locked_step_keeps_what_that_step_set() {
    // Locks are per step, so a step swallowed by a run keeps its lock — and the runtime reads it
    // there, moving the parameter under the held note. Nothing is stored that cannot be heard.
    let mut harness = AppHarness::new("seq-lengthen-locked", Vec::new());
    harness.run();

    harness.app().select_step(1);
    harness.run();
    harness.app().set_parameter(42, 0.6);
    assert_eq!(harness.app().sequencer_state().locks.get(1, 42), Some(0.6));

    // Step 1 holds a note; selecting step 2 twice ties it, lengthening that note over it.
    // `select_step` is exactly what the click handler calls, and the second call is where
    // `tie_action` runs — so this is the click path, without depending on a step's accessible label.
    harness.app().toggle_step_note(0, 60);
    harness.app().deselect_step();
    harness.app().select_step(1);
    harness.app().select_step(1);
    harness.run();

    assert!(
        harness.app().pattern().tied(1),
        "the second click should have lengthened the note over step 2"
    );
    assert_eq!(
        harness.app().sequencer_state().locks.get(1, 42),
        Some(0.6),
        "a step swallowed by a run keeps its lock, which is heard under the held note"
    );
}

#[test]
fn selecting_a_tied_step_previews_that_step() {
    // The runtime previews whatever step it is told, and with locks per step the published step is
    // the raw selection — the panel shows the same cell the preview sounds, so the two halves of
    // "what am I editing" cannot disagree.
    let mut harness = AppHarness::new("seq-preview-tied", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    for step in [1, 2, 3] {
        harness.app().toggle_step_tie(step);
    }
    harness.app().select_step(0);
    harness.run();
    harness.app().set_parameter(42, 0.8);

    harness.app().select_step(3);
    harness.run();

    assert_eq!(
        harness.app().published_editing_step(),
        Some(3),
        "the runtime must be told the selected step itself — its locks are its own"
    );
}

#[test]
fn a_lock_on_a_tied_step_loads_and_is_kept() {
    // The state an older build wrote as an accident is music now: a hold's lock moves the
    // parameter under the held note, so a file carrying one loads as written — the owner's
    // file-true-to-the-interface ruling, observed on the load path.
    let mut harness = AppHarness::new("seq-legacy-tied-lock", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.app().select_step(2);
    harness.run();
    harness.app().set_parameter(42, 0.5);
    assert_eq!(harness.app().sequencer_state().locks.get(2, 42), Some(0.5));

    harness.app().force_tie_for_test(2);
    assert_eq!(
        harness.app().sequencer_state().locks.get(2, 42),
        Some(0.5),
        "a tied step keeps its lock — it is heard there"
    );
}

#[test]
fn what_is_rendered_is_what_was_heard_including_across_the_loop_point() {
    // **The requirement that settled the design.** An export must not introduce a sound change the
    // live pattern did not have. Locks are per step in both paths, so each reads the same cell —
    // including at step 1 of a run that wraps the loop point, where both put the parameter back
    // to the patch because step 1 sets nothing.
    use mxm_player::sequencer::export;
    use mxm_player::sequencer::locks::LockSet;
    use mxm_player::sequencer::pattern::{Pattern, STEPS};

    // A run inside the bar, and a run that crosses the loop point: the two cases that can differ.
    let mut pattern = Pattern::empty();
    pattern.toggle(4, 60);
    for step in [5, 6, 7] {
        pattern.set_tied(step, true);
    }
    pattern.toggle(15, 67);
    pattern.set_tied(0, true);

    let mut locks = LockSet::EMPTY;
    locks.set(4, 42, 0.9, 0.2).expect("room");
    locks.set(15, 42, 0.8, 0.2).expect("room");

    let rendered: Vec<f32> = export::schedule_locks(&pattern, &locks, 120.0, 48_000.0)
        .iter()
        .filter(|(_, id, _)| *id == 42)
        .map(|(_, _, offset)| *offset)
        .take(STEPS)
        .collect();

    // What live playback emits — asserted as concrete numbers rather than by calling the same
    // function twice, which would agree with itself however wrong it was. Per step: only the
    // steps that lock something deviate.
    let expected: Vec<f32> = (0..STEPS)
        .map(|step| match step {
            4 => 0.7,
            15 => 0.6,
            _ => 0.0,
        })
        .collect();

    for (step, (got, want)) in rendered.iter().zip(expected.iter()).enumerate() {
        assert!(
            (got - want).abs() < 1e-6,
            "rendered step {} offsets by {got}, but playback offsets by {want}",
            step + 1
        );
    }
}

#[test]
fn the_cli_can_build_a_long_sequence_and_a_bar_that_is_not_four_four() {
    // **The use this exists for**: a sequence authored by a machine, longer than anyone would draw
    // by hand. Sixty-four bars is a whole track, and the CLI is the path with no mouse — which is
    // why a step count derived from what a mouse can reach was the wrong kind of number.
    let mut harness = AppHarness::new("cli-long-sequence", Vec::new());
    harness.run();

    let out = harness.app().run_cli_command("bars 64");
    assert!(out.contains("64 bars of 16 steps"), "{out}");
    assert_eq!(harness.app().pattern().len(), 64 * 16);

    // Twelve steps to a bar is 3/4 — a step stays a sixteenth and the bar holds fewer of them.
    let out = harness.app().run_cli_command("steps 12");
    assert!(out.contains("64 bars of 12 steps"), "{out}");
    assert_eq!(harness.app().pattern().len(), 64 * 12);

    // A note in the last bar is reachable, which is the whole point.
    let last = 64 * 12 - 1;
    harness.app().toggle_step_note(last, 60);
    assert!(harness.app().pattern().step(last).contains(60));

    let out = harness.app().run_cli_command("bars 0");
    assert!(out.contains("error"), "zero bars is not a sequence: {out}");
}

#[test]
fn shrinking_a_sequence_takes_the_locks_that_fall_outside_it() {
    // A lock past the end would be stored, saved, unreachable — and would come back the moment the
    // sequence grew again. The same defect the tied-step rule exists to prevent, one dimension over.
    let mut harness = AppHarness::new("cli-shrink-locks", Vec::new());
    harness.run();

    harness.app().set_bars(4);
    harness.app().select_step(60);
    harness.run();
    harness.app().set_parameter(42, 0.75);
    assert_eq!(
        harness.app().sequencer_state().locks.get(60, 42),
        Some(0.75),
        "the lock is in the fourth bar"
    );

    harness.app().set_bars(2);

    assert_eq!(
        harness.app().sequencer_state().locks.get(60, 42),
        None,
        "and goes with the bar it was in"
    );
    assert_eq!(
        harness.app().selected_step(),
        None,
        "and the selection cannot point past the end either"
    );
}

#[test]
fn a_long_sequence_renders_all_of_itself() {
    // **"If the length is 9 bars, we save 9 bars."** A render used to be two bars whatever the
    // sequence was — one of pattern plus one the code appended to hear the tail. That was a guess
    // made on somebody's behalf, and it became wrong the moment a sequence could be longer than the
    // guess.
    let Some(mut session) = session("seq-export-long") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    session.app().set_tempo(120.0);
    session.app().set_bars(9);
    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(8 * 16, 64);

    let path = session.app().export_audio("nine").expect("export");
    let decoded = decode(&path);
    let frames = decoded.frames();

    let steps = session.app().pattern().len();
    let expected = steps
        * mxm_player::sequencer::export::frames_per_step(120.0, f64::from(decoded.sample_rate));
    assert_eq!(steps, 9 * 16, "nine bars of sixteen");
    assert!(
        frames.abs_diff(expected) <= 1,
        "nine bars should render nine bars: expected {expected} frames, got {frames}"
    );

    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(name.contains("9bars"), "{name}");
}

#[test]
fn clicking_a_step_selects_it_and_clicking_again_ties_it() {
    // The second click used to deselect. It ties instead, because a click has no pitch to offer
    // and so can only ever say *how long* — and that gesture was the only one free.
    let mut harness = AppHarness::new("seq-select", Vec::new());
    harness.run();
    assert_eq!(harness.app().selected_step(), None);

    harness.harness.get_by_label("Step 1: empty").click();
    harness.run();
    assert_eq!(harness.app().selected_step(), Some(0));
    assert!(
        !harness.app().pattern().tied(0),
        "the first click only ever selects, so a tie can never be set on the way to a note"
    );

    harness.harness.get_by_label("Step 1: empty").click();
    harness.run();
    assert!(
        harness.app().pattern().tied(0),
        "clicking the selected step should tie it"
    );
    assert_eq!(
        harness.app().selected_step(),
        Some(0),
        "and it stays selected, so the keyboard still writes here"
    );

    harness
        .harness
        .get_by_label("Step 1: tied, holding the previous note")
        .click();
    harness.run();
    assert!(!harness.app().pattern().tied(0), "a third click unties it");
}

#[test]
fn clicking_a_note_with_a_rest_in_front_of_it_ties_it_all_the_same() {
    // **A superseded report, kept deliberately.** This was once a defect: select step 13, play a
    // note, click it again, and it tied step 13 to the empty step 12 — "a legato joint hanging off
    // a rest". Two mechanisms were built to prevent it, first a click that reached forwards
    // instead, then a guard that refused the tie.
    //
    // The owner's ruling retires both: a tie is a fact about its own step, and a run with no note
    // in front of it is silence, not damage. The runtime already played it that way. So the state
    // the report named is now legal, and this test is what says the reversal was deliberate rather
    // than a regression that slipped back in.
    let mut harness = AppHarness::new("seq-lengthen", Vec::new());
    harness.run();

    harness.app().select_step(12);
    harness.app().note_on(60, 0.8);
    harness.app().note_off(60);
    harness.run();
    assert!(harness.app().pattern().step(12).contains(60), "the premise");

    harness.app().select_step(12);
    harness.run();

    assert!(
        harness.app().pattern().tied(12),
        "the click ties the step it named, whatever sits in front of it"
    );
    assert!(
        !harness.app().pattern().tied(13),
        "and nothing happens to a step the click never named"
    );
    assert!(
        harness.app().pattern().step(12).contains(60),
        "and it is still the same note, in the same place"
    );
}

#[test]
fn a_note_ties_to_the_note_in_front_of_it_wherever_it_sits() {
    // The whole of the new rule: the click sets the flag on the step you clicked, and the answer
    // does not depend on where in the row that step is. The last step is the one the old rule
    // could not tie at all -- its click had nowhere forwards to go -- so it is the one tested.
    let mut harness = AppHarness::new("seq-tie-anywhere", Vec::new());
    harness.run();

    let last = harness.app().pattern().len() - 1;
    harness.app().toggle_step_note(last - 1, 60);
    harness.app().toggle_step_note(last, 67);
    harness.run();

    harness.app().select_step(last);
    harness.app().select_step(last);
    harness.run();

    assert!(
        harness.app().pattern().tied(last),
        "the last step ties to the note in front of it like any other"
    );
    assert!(
        harness.app().pattern().step(last).contains(67),
        "and keeps its own note, which is what makes it a slide"
    );
}

#[test]
fn lengthening_a_note_is_clicking_the_step_it_swallows() {
    // A note is made longer from the step that changes -- the empty one after it -- not from the
    // note itself. One click selects that step, the second ties it, and a third takes it back.
    let mut harness = AppHarness::new("seq-shorten", Vec::new());
    harness.run();

    harness.app().toggle_step_note(4, 60);
    harness.run();

    harness.app().select_step(5);
    harness.app().select_step(5); // lengthen: step 6 becomes a hold
    harness.run();
    assert!(harness.app().pattern().tied(5));

    harness.app().select_step(5); // and back
    harness.run();
    assert!(
        !harness.app().pattern().tied(5),
        "the gesture has to be reversible from the step you are looking at"
    );
}

#[test]
fn lengthening_over_another_note_makes_it_a_slide() {
    // The old refusal row, and now the gesture's second way to author a slide: tying a
    // note-carrying next step keeps the gate open into it — tie onto a pitch, where Option A is
    // pitch into a tie. Clicking again takes the slide back off.
    let mut harness = AppHarness::new("seq-blocked", Vec::new());
    harness.run();

    harness.app().toggle_step_note(4, 60);
    harness.app().toggle_step_note(5, 67);
    harness.run();

    harness.app().select_step(5);
    harness.app().select_step(5);
    harness.run();

    assert!(
        !harness.app().pattern().tied(4),
        "the note in front is untouched — the click names one step"
    );
    assert!(
        harness.app().pattern().tied(5),
        "the clicked step is tied — a slide"
    );
    assert!(
        harness.app().pattern().step(5).contains(67),
        "and it keeps its own note, which is what makes it one"
    );

    harness.app().select_step(5); // untie: the slide detaches
    harness.run();
    assert!(
        !harness.app().pattern().tied(5),
        "the gesture is reversible from the step you are looking at"
    );
    assert!(
        harness.app().pattern().step(5).contains(67),
        "untying a slide leaves an ordinary note"
    );
}

#[test]
fn the_hint_line_describes_the_click_that_will_actually_happen() {
    // The gesture means different things depending on what the step holds, so a hint composed
    // separately from the action would be a second implementation of that rule -- free to describe
    // the wrong one. Both read from `tie_action`; this is what says so.
    let mut harness = AppHarness::new("seq-hint", Vec::new());
    harness.run();

    // An empty step: the click ties it to what comes before.
    harness.app().select_step(3);
    harness.run();
    harness.harness.get_by_label_contains("click again to tie");

    // A step holding a note ties into a slide, and the hint says so rather than "tie" — the
    // word is what tells you the pitch will move under a held gate.
    harness.app().toggle_step_note(7, 60);
    harness.app().deselect_step();
    harness.app().select_step(7);
    harness.run();
    harness
        .harness
        .get_by_label_contains("click again to slide");

    // A tied step offers the way back out — capitalised, because after "a slide." it starts a
    // sentence. That the hint says what the step *is* as well as what the click does is the point.
    harness.app().select_step(7); // the promised click: step 8 becomes a slide
    harness.run();
    harness
        .harness
        .get_by_label_contains("a slide. Click again to untie");
}

#[test]
fn a_first_click_on_a_different_step_never_ties_it() {
    // The property that makes the gesture safe: moving the selection is always just a selection,
    // however many times you have clicked elsewhere.
    let mut harness = AppHarness::new("seq-move", Vec::new());
    harness.run();

    harness.harness.get_by_label("Step 1: empty").click();
    harness.run();
    harness.harness.get_by_label("Step 1: empty").click();
    harness.run();
    assert!(harness.app().pattern().tied(0), "step 1 is now tied");

    harness.harness.get_by_label("Step 3: empty").click();
    harness.run();
    assert_eq!(harness.app().selected_step(), Some(2));
    assert!(
        !harness.app().pattern().tied(2),
        "arriving at a new step must not tie it"
    );
    assert!(
        harness.app().pattern().tied(0),
        "and must not disturb step 1"
    );
}

#[test]
fn escape_stops_editing_a_step_without_touching_it() {
    // It has to exist: the second click ties rather than letting go, so without Escape there is
    // no way to put the keyboard back to only playing.
    let mut harness = AppHarness::new("seq-escape", Vec::new());
    harness.run();

    harness.harness.get_by_label("Step 2: empty").click();
    harness.run();
    assert_eq!(harness.app().selected_step(), Some(1));

    harness.harness.key_press(egui::Key::Escape);
    harness.run();
    assert_eq!(harness.app().selected_step(), None, "Escape lets go");
    assert!(
        !harness.app().pattern().tied(1),
        "and leaves the step exactly as it was"
    );
}

#[test]
fn the_step_row_does_not_change_width_when_a_note_is_added() {
    // The original complaint: steps were labelled with their notes, so the row reflowed on every
    // edit. This is the assertion that would have failed before.
    let mut harness = AppHarness::new("seq-width", Vec::new());
    harness.run();

    let before = harness.harness.get_by_label("Step 1: empty").rect().width();

    harness.app().select_step(0);
    harness.app().note_on(60, 0.8);
    harness.app().note_off(60);
    harness.run();

    let after = harness.harness.get_by_label("Step 1: C3").rect().width();
    assert!(
        (before - after).abs() < 0.5,
        "the step button resized from {before} to {after} when a note was added"
    );
}

#[test]
fn a_steps_accessible_name_says_which_notes_it_holds() {
    // The notes left the visible label so the row would stop moving. They must not leave the
    // accessibility tree — painted keys cannot carry them, since the keyboard allocates one
    // interaction region for every key.
    let mut harness = AppHarness::new("seq-a11y", Vec::new());
    harness.run();
    harness.app().select_step(2);
    harness.app().note_on(60, 0.8);
    harness.app().note_off(60);
    harness.app().note_on(67, 0.8);
    harness.app().note_off(67);
    harness.run();

    harness.harness.get_by_label("Step 3: C3, G3");
}

#[test]
fn selecting_a_step_marks_its_notes_on_the_keyboard() {
    // Paint output, because keys have no AccessKit node of their own.
    let mut harness = AppHarness::new("seq-marks", Vec::new());
    harness.run();
    harness.app().select_step(0);
    harness.app().note_on(60, 0.8);
    harness.app().note_off(60);
    harness.run();

    let marked = harness.painted_rects_filled(SELECTED_KEY);
    assert!(
        !marked.is_empty(),
        "the selected step's note should be marked on the keyboard"
    );
}

#[test]
fn selecting_a_tied_step_shows_that_steps_own_notes() {
    // The highlight and the write have to be the same step, and with the redirect gone that step
    // is the selection itself: a hold marks nothing — honestly, since a pitch played there lands
    // there as a slide — and a slide marks its own note, not its head's.
    let mut harness = AppHarness::new("seq-tied-keyboard", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.app().toggle_step_tie(1);
    harness.app().toggle_step_tie(2);
    harness.run();

    let on_the_run_start = {
        harness.app().select_step(0);
        harness.run();
        harness.painted_rects_filled(SELECTED_KEY).len()
    };
    assert!(on_the_run_start > 0, "the premise: step 1's note is marked");

    for tied in [1usize, 2] {
        harness.app().select_step(tied);
        harness.run();
        assert_eq!(
            harness.painted_rects_filled(SELECTED_KEY).len(),
            0,
            "a hold has no notes of its own, and the keyboard says so — a pitch played here \
             lands here (step {})",
            tied + 1
        );
    }

    // A slide shows its own note.
    harness.app().select_step(2);
    harness.app().note_on(67, 0.8);
    harness.app().note_off(67);
    harness.run();
    assert!(
        harness.app().pattern().step(2).contains(67),
        "the premise: step 3 is a slide now"
    );
    assert_eq!(
        harness.painted_rects_filled(SELECTED_KEY).len(),
        on_the_run_start,
        "a slide marks its own note on the keyboard"
    );
}

#[test]
fn the_keyboard_writes_into_the_selected_step() {
    let mut harness = AppHarness::new("seq", Vec::new());
    harness.run();

    harness.harness.get_by_label("Step 1: empty").click();
    harness.run();

    // The computer keyboard takes the same path as the on-screen keys.
    harness.app().note_on(60, 0.8);
    harness.app().note_off(60);
    harness.run();

    assert!(
        harness.app().pattern().step(0).contains(60),
        "the note should have been written into step 1"
    );
}

#[test]
fn the_keyboard_only_edits_while_a_step_is_selected() {
    // The modal risk, as a test: with nothing selected the keyboard must only play.
    let mut harness = AppHarness::new("seq", Vec::new());
    harness.run();
    assert_eq!(harness.app().selected_step(), None);

    harness.app().note_on(60, 0.8);
    harness.app().note_off(60);
    harness.run();

    assert!(
        harness.app().pattern().is_empty(),
        "playing a note with no step selected must not edit the pattern"
    );
}

#[test]
fn the_panel_says_which_mode_the_keyboard_is_in() {
    // If the mode is not visible, somebody presses a key expecting a note and silently edits.
    let mut harness = AppHarness::new("seq", Vec::new());
    harness.run();
    harness
        .harness
        .get_by_label_contains("Click a step to edit");

    harness.harness.get_by_label("Step 1: empty").click();
    harness.run();
    harness.harness.get_by_label_contains("Editing step 1");
}

#[test]
fn the_parameter_panel_wraps_into_columns_when_it_is_short() {
    // The reported defect: a short window hid the bottom of the list. Asserted by counting
    // columns, because reachability alone would pass against the single tall scroller this
    // replaces and would prove nothing.
    let Some(dir) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let mut harness = AppHarness::new("seq-wrap", vec![dir.clone()]);
    harness.run();
    harness
        .app()
        .load(dir.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    harness.run();

    // Wide enough that the minimum-column-width cap is not what is being measured.
    harness.resize(1600.0, 900.0);
    let tall = harness.app().param_columns();

    harness.resize(1600.0, 460.0);
    let short = harness.app().param_columns();

    assert!(
        short > tall,
        "a shorter window should wrap into more columns; got {tall} tall, {short} short"
    );
}

#[test]
fn the_sequencer_sits_above_the_keyboard() {
    // Where the ask put it. The keyboard keeps the bottom edge; the sequencer is directly above.
    let mut harness = AppHarness::new("seq", Vec::new());
    harness.run();

    let sequencer_bottom = harness.harness.get_by_label("Random").rect().bottom();
    let keys = harness.painted_rects();
    let lowest_key = keys
        .iter()
        .map(|r| r.rect.bottom())
        .fold(f32::MIN, |a, b| a.max(b));

    assert!(
        sequencer_bottom < lowest_key,
        "the sequencer ({sequencer_bottom}) should sit above the keyboard ({lowest_key})"
    );
}

// --- listening ----------------------------------------------------------------------------------

/// Renders a random C Dorian sequence to a WAV, for a person to listen to.
///
/// `#[ignore]`d: it writes a file and proves nothing an assertion could. Run it with
/// `cargo test -p mxm-player --test t6_sequencer -- --ignored --nocapture` when you want to hear
/// what the sequencer sounds like rather than read what it measured.
#[test]
#[ignore]
fn render_sequencer_demo() {
    let Some(mut session) = session("seq-demo") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(132.0);
    session.app().randomise();
    session.app().play_from_start();

    // Four bars at 132 BPM.
    let frames = (4 * 16) as f64 * 48_000.0 * 15.0 / 132.0;
    session
        .advance_blocks((frames / 512.0) as u64)
        .expect("advance");

    let (wav, json) = session.write_artifacts("sequencer-demo").expect("write");
    println!("wrote {}", wav.display());
    println!("wrote {}", json.display());
    for names in &session.state().sequencer.steps {
        print!("{:?} ", names);
    }
    println!();
}

/// Measures where mxm-mono-01's tail actually falls, so the export silence floor is a number with
/// evidence behind it rather than a guess. `--ignored --nocapture`.
#[test]
#[ignore]
fn measure_decay_floor() {
    let Some(mut session) = session("seq-floor") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // One note, long release, then listen to the tail.
    session.app().set_tempo(120.0);
    session.app().toggle_step_note(0, 48);
    session.app().play_from_start();
    session.advance_blocks(20).expect("sound it");
    session.app().pause(); // release the note, then listen to the tail
    session.clear_capture();
    session.advance_blocks(900).expect("tail");

    let mono = left(&session.captured());
    let sr = 48_000.0f64;
    println!(
        "tail frames: {}  ({:.2}s)",
        mono.len(),
        mono.len() as f64 / sr
    );

    for db in [-40.0f64, -60.0, -72.0, -80.0, -90.0, -100.0, -120.0] {
        let floor = 10f32.powf((db / 20.0) as f32);
        let last = mono.iter().rposition(|s| s.abs() > floor);
        match last {
            Some(index) => println!(
                "{:>7.0} dBFS  last exceeded at frame {:>8}  ({:.3}s after release)",
                db,
                index,
                index as f64 / sr
            ),
            None => println!("{:>7.0} dBFS  never exceeded", db),
        }
    }
    let peak_tail = mono.iter().fold(0.0f32, |p, s| p.max(s.abs()));
    println!(
        "tail peak: {peak_tail:.6} ({:.1} dBFS)",
        20.0 * peak_tail.log10()
    );
}

// --- export -------------------------------------------------------------------------------------

#[test]
fn a_midi_file_round_trips_through_the_player() {
    let Some(mut session) = session("seq-midi") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(137.0);
    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(0, 67);
    session.app().toggle_step_note(9, 51);
    let before = session.state().sequencer.steps.clone();

    let path = session.app().save_midi("round-trip").expect("save");
    assert_eq!(path.extension().unwrap(), "mid");

    session.app().clear_pattern();
    session.app().load_midi(&path).expect("load");

    assert_eq!(session.state().sequencer.steps, before, "pattern");
    // Saving canonicalises the tempo to what SMF can hold, so the round trip is exact.
    assert_eq!(
        session.state().sequencer.tempo,
        mxm_player::sequencer::smf::canonical_tempo(137.0)
    );
}

#[test]
fn a_midi_file_the_model_cannot_hold_is_refused_with_a_reason() {
    let Some(mut session) = session("seq-midi-refuse") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    session.app().toggle_step_note(3, 64);
    let before = session.state().sequencer.steps.clone();

    let bad = session.dir().join("broken.mid");
    std::fs::write(&bad, b"MThd not really a midi file").unwrap();

    let error = session.app().load_midi(&bad).expect_err("must refuse");
    assert!(!error.is_empty(), "the refusal must say why");
    assert_eq!(
        session.state().sequencer.steps,
        before,
        "a refused file must leave the sequence untouched"
    );
}

#[test]
fn exporting_audio_reproduces_the_current_patch_not_the_default() {
    // The finding that would have shipped a wrong file: loading a bundle by path and id gives the
    // plugin's *default* patch. Without transferring state, this test fails.
    let Some(mut session) = session("seq-export-patch") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(240.0);
    for step in 0..16 {
        session.app().toggle_step_note(step, 48);
    }

    let default_render = session.app().export_audio("default").expect("export");
    let default_bytes = std::fs::read(&default_render).expect("read");

    // Close the filter hard, through the control map — the same path a knob takes.
    for value in [127u8, 96, 64, 32, 0] {
        session.app().send_control_change(74, value);
        session.advance_blocks(1).expect("advance");
    }

    let closed_render = session.app().export_audio("closed").expect("export");
    let closed_bytes = std::fs::read(&closed_render).expect("read");

    assert_ne!(
        default_bytes, closed_bytes,
        "the export must carry the patch that was dialled in, not the plugin's defaults"
    );
}

#[test]
fn an_exported_wav_is_two_bars_at_its_stated_tempo() {
    let Some(mut session) = session("seq-export-length") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(120.0);
    session.app().toggle_step_note(0, 60);
    let path = session.app().export_audio("length").expect("export");

    let decoded = decode(&path);
    let frames = decoded.frames();

    // **Exactly the sequence, with no tail appended.** It used to be two bars — one of pattern plus
    // one the code added on the person's behalf — and a sequence has a length of its own now.
    let steps = session.app().pattern().len();
    let expected = steps
        * mxm_player::sequencer::export::frames_per_step(120.0, f64::from(decoded.sample_rate));
    assert!(
        frames.abs_diff(expected) <= 1,
        "a one-bar sequence should render one bar: expected {expected} frames, got {frames}"
    );
    assert_eq!(decoded.bits_per_sample, Some(32));
    assert!(
        decoded.float,
        "the export is float, so a hot patch cannot clip"
    );
}

#[test]
fn the_file_name_states_the_tempo_and_length() {
    let Some(mut session) = session("seq-export-name") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    session.app().set_tempo(137.0);
    session.app().set_bars(4);
    session.app().toggle_step_note(0, 60);
    let path = session.app().export_audio("acid").expect("export");

    // **The sequence's own length.** It used to say `2bars` for everything, because a render was
    // always one bar of pattern plus one of tail.
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(name.contains("137bpm"), "{name}");
    assert!(name.contains("4bars"), "{name}");
}

#[test]
fn an_exported_wav_peaks_at_full_scale_when_normalised() {
    let Some(mut session) = session("seq-export-level") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(240.0);
    session.app().toggle_step_note(0, 48);
    session.app().set_normalise_export(true);
    let path = session.app().export_audio("loud").expect("export");

    let peak = decode(&path)
        .interleaved
        .iter()
        .fold(0.0f32, |p, s| p.max(s.abs()));
    assert!((peak - 1.0).abs() < 1e-4, "peak was {peak}");
}

#[test]
fn turning_normalisation_off_keeps_the_rendered_level() {
    // What makes comparing two patches for loudness possible again.
    let Some(mut session) = session("seq-export-raw") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(240.0);
    session.app().toggle_step_note(0, 48);
    session.app().set_normalise_export(false);
    let path = session.app().export_audio("raw").expect("export");

    let peak = decode(&path)
        .interleaved
        .iter()
        .fold(0.0f32, |p, s| p.max(s.abs()));
    assert!(
        peak < 0.99,
        "an un-normalised export should carry its rendered level, peaked at {peak}"
    );
}

#[test]
fn an_exported_wav_stays_valid_with_the_acid_chunk_present() {
    // The property that matters if the chunk is wrong or unsupported: a reader that has never
    // heard of `acid` must still see an ordinary, correct WAV.
    let Some(mut session) = session("seq-export-acid") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    session.app().toggle_step_note(0, 60);
    let path = session.app().export_audio("acid-chunk").expect("export");

    // symphonia knows nothing about `acid`, so this is a reader that has never heard of it.
    assert!(
        decode(&path).frames() > 0,
        "a reader that ignores `acid` must still read it"
    );

    let bytes = std::fs::read(&path).expect("read");
    let chunks = mxm_audio_file::acid::chunks(&bytes).expect("walkable by declared lengths");
    assert!(
        chunks.iter().any(|(id, _)| id == b"acid"),
        "the chunk should be there: {chunks:?}"
    );
}

#[test]
fn saving_twice_never_overwrites_the_first_file() {
    let Some(mut session) = session("seq-export-collide") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    session.app().toggle_step_note(0, 60);

    let first = session.app().export_audio("same-name").expect("export");
    let second = session.app().export_audio("same-name").expect("export");

    assert_ne!(first, second, "the second save must not replace the first");
    assert!(first.exists() && second.exists());
}

#[test]
fn every_export_path_stays_inside_the_sandbox() {
    // `PlayerConfig` owns external paths so a test can never write into real user storage. An
    // export location that bypassed it would put files in somebody's actual exports folder.
    let Some(mut session) = session("seq-export-sandbox") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    session.app().toggle_step_note(0, 60);

    let sandbox = session.dir().to_path_buf();
    let wav = session.app().export_audio("sandboxed").expect("export");
    let mid = session.app().save_midi("sandboxed").expect("save");

    assert!(wav.starts_with(&sandbox), "{}", wav.display());
    assert!(mid.starts_with(&sandbox), "{}", mid.display());
}

/// Exports a random C Dorian loop as a `.wav` and a `.mid`, for a person to listen to and inspect.
///
/// `--ignored --nocapture`.
#[test]
#[ignore]
fn export_demo() {
    let Some(mut session) = session("seq-export-demo") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_tempo(126.0);
    session.app().randomise();

    let wav = session
        .app()
        .export_audio("mxm-mono-01-dorian")
        .expect("wav");
    let mid = session.app().save_midi("mxm-mono-01-dorian").expect("mid");
    println!("wav: {}", wav.display());
    println!("mid: {}", mid.display());
    println!("status: {:?}", session.state().status);
    for names in &session.state().sequencer.steps {
        print!("{names:?} ");
    }
    println!();
}

/// A `.mid` written by **mxm.midifile**, an independent implementation.
///
/// The other half of the conformance gate: our reader agreeing with our writer proves they share
/// assumptions, not that either is right. This file was produced by something that is not us, and
/// is deliberately awkward — division 480 rather than our 96, so the grid must be rescaled, and a
/// velocity we cannot keep, which must be reported rather than dropped silently.
///
/// Regenerate with `tests/midi-conformance/verify.py`.
const FOREIGN_MIDI: &[u8] = include_bytes!("midi-conformance/foreign.mid");

#[test]
fn a_midi_file_from_an_independent_implementation_loads() {
    let Some(mut session) = session("seq-foreign-midi") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    let path = session.dir().join("foreign.mid");
    std::fs::write(&path, FOREIGN_MIDI).unwrap();
    session
        .app()
        .load_midi(&path)
        .expect("a foreign file must load");

    let state = session.state();
    // A C minor triad on step 1 at division 480, rescaled onto our sixteenth grid.
    assert_eq!(state.sequencer.steps[0], vec!["C2", "D#2", "G2"]);
    // And a C3 eight steps later.
    assert_eq!(state.sequencer.steps[8], vec!["C3"]);
    assert!(
        (state.sequencer.tempo - 110.0).abs() < 0.01,
        "{}",
        state.sequencer.tempo
    );
}

#[test]
fn a_foreign_file_names_the_velocity_it_cannot_keep() {
    // Read, used where it means something, and lost — but never silently.
    let Some(mut session) = session("seq-foreign-lost") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    let path = session.dir().join("foreign.mid");
    std::fs::write(&path, FOREIGN_MIDI).unwrap();
    session.app().load_midi(&path).expect("loads");

    let problems = session.app().sequence_problems().to_vec();
    assert!(
        problems.iter().any(|p| p.contains("velocit")),
        "velocity loss must be named: {problems:?}"
    );
}

// --- geometry that must not move ------------------------------------------------------------------

#[test]
fn the_transport_button_does_not_change_size_when_it_toggles() {
    // "Play" and "Stop" are different widths, so an unsized button resizes when pressed and
    // shifts everything to its right.
    let mut harness = AppHarness::new("seq-btn-size", Vec::new());
    harness.run();

    let stopped = harness.harness.get_by_label("▶ Play").rect();

    harness.harness.get_by_label("▶ Play").click();
    harness.run();
    let playing = harness.harness.get_by_label("⏹ Stop").rect();

    assert!(
        (stopped.width() - playing.width()).abs() < 0.5,
        "the toggle went from {} to {} wide",
        stopped.width(),
        playing.width()
    );
    assert!(
        (stopped.min.x - playing.min.x).abs() < 0.5,
        "the toggle moved from x={} to x={}",
        stopped.min.x,
        playing.min.x
    );
}

#[test]
fn what_follows_the_transport_button_does_not_move_when_it_toggles() {
    // The point of the fixed size: the controls after it stay put.
    let mut harness = AppHarness::new("seq-btn-shift", Vec::new());
    harness.run();
    let before = harness.harness.get_by_label("Random").rect().min.x;

    harness.harness.get_by_label("▶ Play").click();
    harness.run();
    let after = harness.harness.get_by_label("Random").rect().min.x;

    assert!(
        (before - after).abs() < 0.5,
        "Random shifted from {before} to {after} when the transport was pressed"
    );
}

#[test]
fn the_sequencer_panel_keeps_its_height_when_a_load_has_something_to_report() {
    // The panel used to grow a row per reported problem, pushing the whole interface up.
    let Some(mut session) = session("seq-panel-height") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let _ = &mut session;

    let mut harness = AppHarness::new("seq-panel-h", Vec::new());
    harness.run();
    let quiet = harness.harness.get_by_label("Random").rect().min.y;

    // A foreign file that reports lost information.
    let path = harness.dir().join("foreign.mid");
    std::fs::write(&path, FOREIGN_MIDI).unwrap();
    harness.app().load_midi(&path).expect("loads");
    harness.run();

    assert!(
        !harness.app().sequence_problems().is_empty(),
        "this file should report something it could not keep"
    );

    let noisy = harness.harness.get_by_label("Random").rect().min.y;
    assert!(
        (quiet - noisy).abs() < 0.5,
        "the panel moved from y={quiet} to y={noisy} when a load reported something"
    );
}

#[test]
fn our_own_midi_file_reports_nothing_lost() {
    // It came back claiming it had lost note lengths, which was untrue — the lengths *are* our
    // gate — and which added a row that moved the interface.
    let Some(mut session) = session("seq-own-midi-clean") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(7, 63);
    let path = session.app().save_midi("clean").expect("save");
    session.app().load_midi(&path).expect("load");

    assert!(
        session.app().sequence_problems().is_empty(),
        "a file we wrote loses nothing: {:?}",
        session.app().sequence_problems()
    );
}

/// Renders the sequencer panel in both transport states and with a reporting load, so the geometry
/// can be looked at rather than only asserted. `--ignored --nocapture`.
#[test]
#[ignore]
fn snapshot_panel_states() {
    let mut harness = AppHarness::new("seq-snap", Vec::new());
    harness.run();

    let stopped = harness.harness.get_by_label("\u{25b6} Play").rect();
    let random_stopped = harness.harness.get_by_label("Random").rect().min.x;

    harness.harness.get_by_label("\u{25b6} Play").click();
    harness.run();
    let playing = harness.harness.get_by_label("\u{23f9} Stop").rect();
    let random_playing = harness.harness.get_by_label("Random").rect().min.x;

    println!(
        "Play  button: {:.1} wide at x={:.1}",
        stopped.width(),
        stopped.min.x
    );
    println!(
        "Stop  button: {:.1} wide at x={:.1}",
        playing.width(),
        playing.min.x
    );
    println!("Random x: {random_stopped:.1} stopped, {random_playing:.1} playing");

    let quiet = harness.harness.get_by_label("Random").rect().min.y;
    let path = harness.dir().join("foreign.mid");
    std::fs::write(&path, FOREIGN_MIDI).unwrap();
    harness.app().load_midi(&path).expect("loads");
    harness.run();
    let noisy = harness.harness.get_by_label("Random").rect().min.y;
    println!(
        "panel row y: {quiet:.1} clean, {noisy:.1} with {} report(s)",
        harness.app().sequence_problems().len()
    );
}

// --- the bar strip's playhead ---------------------------------------------------------------------

/// The bar chips are painted with bare `painter` calls, so **only paint output can see them** --
/// there is no AccessKit node carrying their fill. The chips are the 24x20 rects in the strip.
fn bar_chip_fills(harness: &AppHarness) -> Vec<egui::Color32> {
    harness
        .painted_rects()
        .into_iter()
        .filter(|r| (r.width() - 24.0).abs() < 0.5 && (r.height() - 20.0).abs() < 0.5)
        .map(|r| r.fill)
        .collect()
}

#[test]
fn the_playing_bar_is_lit_the_way_the_playing_step_is() {
    let Some(mut session) = session("bar-strip-playhead") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // Two bars of content, so there is a bar to move *to*.
    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(16, 67);

    assert_eq!(
        session.app().playing_bar(),
        None,
        "a stopped transport sounds no bar, however the playhead is parked"
    );

    session.app().play_from_start();
    // **Blocks first, and several.** The position is the audio thread's, so nothing is sounding
    // until it has run -- the same reason the step row lights nothing on the frame Play is
    // pressed. One block is not enough to assert on: the worker has to pick the transport command
    // up, and `advance_blocks` returns when the block is *counted*, which can precede the publish.
    // Eight blocks is 4096 frames, still inside step one at 120 BPM (6000 frames a step).
    session.advance_blocks(8).expect("advance");
    assert_eq!(session.app().playing_bar(), Some(0), "it starts in bar one");

    // Far enough in to be somewhere in bar two: 16 steps at 120 BPM is 16 * 6000 frames.
    session.advance_blocks(18 * 6000 / 512).expect("advance");
    let (_, step) = session.app().playhead();
    assert_eq!(
        session.app().playing_bar(),
        Some(1),
        "the chip should follow the playhead into bar two; it is at step {step}"
    );
    assert_eq!(
        session.state().sequencer.playing_bar,
        Some(1),
        "and the dump says the same, so the chip is observable over the socket"
    );

    // And blocks after the stop, for the same reason: the chip follows what the audio thread
    // published, not what the GUI last asked for.
    session.app().stop_sequencer();
    session.advance_blocks(8).expect("advance");
    assert_eq!(
        session.app().playing_bar(),
        None,
        "stopping puts every chip out"
    );
}

#[test]
fn a_stopped_transport_lights_no_bar() {
    // The playhead parks on a step when the transport stops, and a chip that stayed lit would
    // claim a bar is sounding when nothing is -- the same rule the step row's highlight follows.
    let mut harness = AppHarness::new("bar-strip-stopped", Vec::new());
    harness.run();
    harness.app().toggle_step_note(0, 60);
    harness.run();

    let fills = bar_chip_fills(&harness);
    assert_eq!(
        fills.len(),
        8,
        "eight chips, whatever the sequence's length"
    );

    // Both themes' token, so the test does not have to know which one the harness resolved.
    assert!(
        !fills.contains(&mxm_ui::DARK.success) && !fills.contains(&mxm_ui::LIGHT.success),
        "nothing is sounding, so no chip may carry the playing fill"
    );
}

#[test]
fn the_playhead_the_bar_strip_reads_is_not_capped_at_a_byte() {
    // The published step was narrowed to `u8`, a silent 255-step ceiling: past bar sixteen the
    // strip would name a bar sixteen bars too early, and the step row lit the wrong cell. The
    // packed word always had the room, so the byte bought nothing.
    let Some(mut session) = session("bar-strip-far") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // Bar twenty holds a note, which is step 304 -- past the byte.
    session.app().toggle_step_note(19 * 16, 60);
    assert_eq!(session.app().sequencer_state().pattern.bars(), 20);

    session.app().set_tempo(300.0);
    session.app().play_from_start();

    let mut reached = 0usize;
    for _ in 0..2000 {
        session.advance_blocks(1).expect("advance");
        let (_, step) = session.app().playhead();
        reached = reached.max(step);
    }
    assert!(
        reached > 255,
        "the playhead never got past a byte: highest step seen was {reached}"
    );
}

// --- Ctrl+C / Ctrl+X / Ctrl+V --------------------------------------------------------------------

/// Presses a clipboard shortcut **the way winit delivers one**.
///
/// This is the whole point of the helper, and the reason the fault it guards was invisible. Winit
/// never sends `Ctrl+C` as a key event: `egui-winit` recognises the chord itself and pushes
/// `Event::Copy` / `Event::Cut` / `Event::Paste`, returning before any `Event::Key` is emitted. A
/// test that synthesised `Key::C` with a ctrl modifier would therefore pass against an app that
/// does nothing at all when a person presses the keys.
fn clipboard_event(harness: &mut AppHarness, event: egui::Event) {
    harness.harness.input_mut().events.push(event);
    harness.run();
}

#[test]
fn ctrl_c_and_ctrl_v_copy_and_paste_a_bar() {
    let mut harness = AppHarness::new("seq-clipboard-bar", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.app().toggle_step_note(4, 67);
    harness.run();

    clipboard_event(&mut harness, egui::Event::Copy);
    harness.app().select_bar(1);
    clipboard_event(&mut harness, egui::Event::Paste("anything".to_owned()));

    let pattern = harness.app().pattern().clone();
    assert_eq!(pattern.bars(), 2, "the pasted bar materialised");
    assert!(pattern.step(16).contains(60), "bar two got bar one's notes");
    assert!(pattern.step(20).contains(67));
}

#[test]
fn ctrl_x_cuts_the_bar_it_copied() {
    let mut harness = AppHarness::new("seq-clipboard-cut", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.run();

    clipboard_event(&mut harness, egui::Event::Cut);
    assert!(
        harness.app().pattern().is_empty(),
        "Ctrl+X should have emptied the bar it copied"
    );

    clipboard_event(&mut harness, egui::Event::Paste("anything".to_owned()));
    assert!(
        harness.app().pattern().step(0).contains(60),
        "and what it cut should paste back"
    );
}

#[test]
fn ctrl_c_acts_on_the_selection_when_there_is_one() {
    let mut harness = AppHarness::new("seq-clipboard-steps", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.app().select_step(0);
    harness.run();

    clipboard_event(&mut harness, egui::Event::Copy);
    harness.app().select_step(8);
    clipboard_event(&mut harness, egui::Event::Paste("anything".to_owned()));

    assert!(
        harness.app().pattern().step(8).contains(60),
        "a copied step pastes at the anchor"
    );
}

#[test]
fn the_shortcuts_survive_clicking_a_step_first() {
    // The real gesture is "click the step, then press Ctrl+C", and the guard these sit behind is
    // `memory.focused().is_some()` -- *any* focused widget, not only a text field. If clicking a
    // step button focused it, every shortcut in the panel would go quiet the moment somebody
    // selected something, which is exactly when they are wanted.
    let mut harness = AppHarness::new("seq-clipboard-focus", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.run();
    harness.harness.get_by_label("Step 1: C3").click();
    harness.run();

    clipboard_event(&mut harness, egui::Event::Copy);
    harness.app().select_step(8);
    clipboard_event(&mut harness, egui::Event::Paste("anything".to_owned()));

    assert!(
        harness.app().pattern().step(8).contains(60),
        "a click before the chord must not silence it"
    );
}

#[test]
fn a_copy_reaches_the_system_clipboard_so_the_paste_chord_is_delivered_at_all() {
    // `egui-winit` only emits `Event::Paste` when the *system* clipboard has something in it: an
    // empty one and the chord is swallowed with no event of any kind. So a copy that left the
    // system clipboard alone would work once and then be unpasteable on a freshly booted machine
    // -- the shortcut dead again, for a reason nothing on screen could explain.
    let mut harness = AppHarness::new("seq-clipboard-system", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.run();

    // **One frame, not four.** The command is drained by the integration every pass, so it is
    // only in the output of the frame that produced it.
    harness.harness.input_mut().events.push(egui::Event::Copy);
    harness.harness.step();

    let copied = harness
        .harness
        .output()
        .platform_output
        .commands
        .iter()
        .find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        })
        .expect("Copy must put text on the system clipboard");
    assert!(
        copied.starts_with("C3 . . ."),
        "the text should read as the steps it copied, got {copied:?}"
    );
}

// --- the spacebar ---------------------------------------------------------------------------------

/// Presses and releases space, the way a keyboard would.
fn tap_space(harness: &mut AppHarness) {
    harness.harness.input_mut().events.push(egui::Event::Key {
        key: egui::Key::Space,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run();
}

#[test]
fn space_plays_from_the_beginning_and_stops() {
    let mut harness = AppHarness::new("seq-space", Vec::new());
    harness.run();
    assert_eq!(
        harness.app().sequencer_state().transport,
        Transport::Stopped
    );

    tap_space(&mut harness);
    assert_eq!(
        harness.app().sequencer_state().transport,
        Transport::Playing
    );

    tap_space(&mut harness);
    assert_eq!(
        harness.app().sequencer_state().transport,
        Transport::Stopped,
        "space is the same call the button makes, so it stops rather than pausing"
    );
}

#[test]
fn space_always_rewinds_rather_than_resuming() {
    // "Pause and play from the beginning", not "pause and resume" — so every start is from step 1.
    let mut harness = AppHarness::new("seq-space-rewind", Vec::new());
    harness.run();

    tap_space(&mut harness);
    let first = harness.app().sequencer_state().generation;

    tap_space(&mut harness); // pause
    tap_space(&mut harness); // play again
    let second = harness.app().sequencer_state().generation;

    assert!(
        second > first,
        "space must restart the run, not resume it ({first} -> {second})"
    );
}

#[test]
fn space_does_not_start_the_sequencer_while_a_name_is_being_typed() {
    // Otherwise naming a sequence "acid bass" would start and stop it mid-word.
    let mut harness = AppHarness::new("seq-space-typing", Vec::new());
    harness.run();

    // Focus the name field, then type a space. Clicking it is what a person would do.
    harness.harness.get_by_label("Name").click();
    harness.run();
    assert!(
        harness.harness.ctx.memory(|m| m.focused().is_some()),
        "the name field should have focus"
    );
    tap_space(&mut harness);

    assert_eq!(
        harness.app().sequencer_state().transport,
        Transport::Stopped,
        "space belongs to the text field while it has focus"
    );
}

// --- the computer keyboard is not an instrument while you are typing --------------------------------
//
// Only the release-on-focus test is here. Tests asserting "typing does not sound a note" were
// written and then removed: they passed **with and without** the guard, because `egui_kittest`
// already withholds key events from the note path once a text field has focus. A test that cannot
// fail is worse than no test — it reports coverage it does not have. The guard itself is a fix for
// behaviour observed in the running player, and is currently unverified by test.

/// A harness with mxm-mono-01 loaded, so the engine is running and notes actually sound.
///
/// Without a plugin `push_gui_event` has nowhere to push, so `held` stays empty and a test that
/// asserts "no notes sounded" passes for the wrong reason.
fn playing_harness(name: &str) -> Option<AppHarness> {
    let dir = app_harness::bundled_dir()?;
    let mut harness = AppHarness::new(name, vec![dir.clone()]);
    harness.run();
    harness
        .app()
        .load(dir.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    harness.run();
    Some(harness)
}

#[test]
fn a_note_held_when_a_field_takes_focus_is_released() {
    // Its key-up will be swallowed by the guard, so the note would otherwise stay on forever.
    let Some(mut harness) = playing_harness("seq-typing-stuck") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    harness.harness.input_mut().events.push(egui::Event::Key {
        key: egui::Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run();
    assert!(!harness.app().state().keyboard.held.is_empty(), "sounding");

    harness.harness.get_by_label("Name").click();
    harness.run();

    assert!(
        harness.app().state().keyboard.held.is_empty(),
        "taking focus must release what the keyboard was holding"
    );
}

#[test]
fn the_tempo_field_does_not_resize_when_it_is_clicked() {
    // A `DragValue` turns into a text field when clicked, and its width then follows the content.
    // Reported: clicking the tempo shifted everything to its right.
    let mut harness = AppHarness::new("seq-tempo-size", Vec::new());
    harness.run();

    let resting = harness.harness.get_by_value("120.0 BPM").rect();
    let neighbour_before = harness.harness.get_by_label("Random").rect().min.x;

    harness.harness.get_by_value("120.0 BPM").click();
    harness.run();

    let neighbour_after = harness.harness.get_by_label("Random").rect().min.x;
    assert!(
        (neighbour_before - neighbour_after).abs() < 0.5,
        "Random shifted from {neighbour_before} to {neighbour_after} when the tempo was clicked"
    );
    assert!(
        resting.width() > 0.0,
        "the tempo field should have a measurable width"
    );
}

// --- parameter locks ------------------------------------------------------------------------

/// Cutoff's parameter id, looked up by name so these do not hard-code a number a later build of
/// mxm-mono-01 could renumber.
///
/// **By name, and a continuous one.** The first parameter in the list is a footage switch: setting
/// it to 0.85 quantises to 1.0, and the test would then be measuring the switch's rounding rather
/// than the lock. Cutoff is also the parameter anybody actually sequences.
fn a_param(session: &mut Session) -> u32 {
    session
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|param| param.name == "Cutoff")
        .expect("mxm-mono-01 has a cutoff")
        .id
}

fn locked_at(session: &mut Session, param_id: u32, step: usize) -> Option<f32> {
    session
        .state()
        .sequencer
        .locks
        .iter()
        .find(|lock| lock.param_id == param_id)
        .and_then(|lock| lock.steps.get(step).copied().flatten())
}

#[test]
fn turning_a_knob_with_a_step_selected_writes_into_that_step() {
    let Some(mut session) = session("locks-author") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    // Nothing selected: moving a knob is just moving a knob.
    session.app().set_parameter(param, 0.3);
    assert_eq!(
        locked_at(&mut session, param, 0),
        None,
        "a knob turn with no step selected must not sequence anything"
    );

    session.app().select_step(2);
    session.app().set_parameter(param, 0.8);

    assert_eq!(
        locked_at(&mut session, param, 2),
        Some(0.8),
        "the selected step now sets it"
    );
    assert_eq!(
        locked_at(&mut session, param, 0),
        None,
        "and no other step does"
    );
}

#[test]
fn double_clicking_a_knob_returns_the_parameter_to_the_patch() {
    let Some(mut session) = session("locks-reset") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    // The patch value: what the parameter was set to before any step touched it.
    //
    // Advanced afterwards because a parameter edit travels **through the audio thread** - the value
    // the panel shows comes back from the plugin, and reading it in the same breath as writing it
    // reads the value from before the write. The same reason `settle_pending_edits` exists.
    session.app().set_parameter(param, 0.42);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(5);
    session.app().set_parameter(param, 0.9);
    session.advance_blocks(4).expect("advance");
    assert_eq!(locked_at(&mut session, param, 5), Some(0.9));

    session.app().reset_parameter(param);
    session.advance_blocks(4).expect("advance");

    assert_eq!(
        locked_at(&mut session, param, 5),
        None,
        "the step sets nothing again"
    );
    let live = session
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|p| p.id == param)
        .expect("the parameter")
        .value;
    assert!(
        (live - 0.42).abs() < 1e-6,
        "and the parameter is back at the patch value, not at its factory default: {live}"
    );
}

#[test]
fn a_saved_sequence_carries_what_its_steps_set() {
    let Some(mut session) = session("locks-roundtrip") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().toggle_step_note(0, 60);
    session.app().select_step(3);
    session.app().set_parameter(param, 0.65);
    session.app().deselect_step();

    let path = session.app().save_sequence("with locks").expect("save");
    session.app().clear_pattern();
    assert_eq!(locked_at(&mut session, param, 3), None, "cleared");

    session.app().load_sequence(&path).expect("load");
    assert_eq!(
        locked_at(&mut session, param, 3),
        Some(0.65),
        "a sequence that lost its filter movement lost half the line"
    );
}

#[test]
fn loading_a_sequence_without_locks_stops_the_ones_in_force() {
    let Some(mut session) = session("locks-replaced") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    let bare = session.app().save_sequence("no locks").expect("save");

    session.app().select_step(1);
    session.app().set_parameter(param, 0.7);
    session.app().deselect_step();
    assert_eq!(locked_at(&mut session, param, 1), Some(0.7));

    session.app().load_sequence(&bare).expect("load");
    assert_eq!(
        locked_at(&mut session, param, 1),
        None,
        "a sequence carrying none says its steps set nothing, not that it has no opinion"
    );
}

#[test]
fn clear_takes_the_locks_and_random_keeps_them() {
    let Some(mut session) = session("locks-clear-random") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().select_step(4);
    session.app().set_parameter(param, 0.55);
    session.app().deselect_step();

    // Random rolls new notes under the sweep you built. That is the whole use of the button.
    // Locks are per step, so this one stays whether the roll puts a note, a tie or a rest there.
    session.app().randomise();
    assert_eq!(
        locked_at(&mut session, param, 4),
        Some(0.55),
        "randomising the notes must not throw away the movement under them"
    );

    // Clear is a request to start again, and a pattern that looks empty must not still be moving
    // the filter every bar.
    session.app().clear_pattern();
    assert_eq!(locked_at(&mut session, param, 4), None);
}

#[test]
fn loading_a_midi_file_clears_the_locks_and_says_so() {
    let Some(mut session) = session("locks-midi") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().toggle_step_note(0, 60);
    let path = session.app().save_midi("plain").expect("save");

    session.app().select_step(6);
    session.app().set_parameter(param, 0.33);
    session.app().deselect_step();

    session.app().load_midi(&path).expect("load");

    assert_eq!(
        locked_at(&mut session, param, 6),
        None,
        "MIDI cannot carry locks, so keeping them would put a sweep nobody imported under imported notes"
    );
    assert!(
        session
            .app()
            .sequence_problems()
            .iter()
            .any(|problem| problem.contains("parameter locks")),
        "and the loss has to be named: {:?}",
        session.app().sequence_problems()
    );
}

#[test]
fn a_locked_step_moves_the_parameter_while_it_plays() {
    // The audio-thread path end to end: the lock reaches the plugin, and the panel is told, so
    // what is on screen is what the instrument has.
    let Some(mut session) = session("locks-audible") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.2);
    session.app().select_step(0);
    session.app().set_parameter(param, 0.85);
    session.app().deselect_step();
    session.app().toggle_step_note(0, 60);

    sharpen(&mut session);
    session.app().play_from_start();
    // **Inside step 1, not past it.** A step is 6,000 frames at 120 BPM, so four blocks of 512 lands
    // well within the first one. Running further would read the parameter after step 2 had put it
    // back to the patch — which is the feature working, not failing.
    session.advance_blocks(4).expect("advance");

    let live = sounding(&mut session, param);
    assert!(
        (live - 0.85).abs() < 1e-3,
        "step 1 sets it to 0.85, and that is what is sounding: {live}"
    );

    // And once the playhead is past it, the parameter is back at the patch — the other half of the
    // same fact, and the one whose absence made this feature do nothing.
    session.advance_blocks(24).expect("advance");
    let after = sounding(&mut session, param);
    assert!(
        (after - 0.2).abs() < 1e-3,
        "step 2 must put it back to the patch, not leave it where step 1 put it: {after}"
    );
}

#[test]
fn selecting_a_step_shows_what_that_step_sets() {
    // Selecting a step already makes the **keyboard** show that step's notes. The knobs follow the
    // same rule: what is on screen while a step is selected is what the step does, not what the
    // patch says. Without it you would be turning a knob away from a value you could not see.
    //
    // **Asserted by the patch's reading disappearing and coming back**, rather than by comparing two
    // live readings. Since locks became modulation, a sequenced parameter's own value *is* the patch
    // and stays there — so the two readings this test used to compare are now always equal, and
    // comparing them proved nothing.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let mut app = AppHarness::new("locks-panel", vec![bundled.clone()]);
    app.run();
    app.harness.state_mut().rescan();
    app.run();
    app.harness
        .state_mut()
        .load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    app.run();

    let param = cutoff_id(&mut app);

    // The patch, and how the plugin writes it. **Computed from the value, then polled onto the
    // screen** — the flake this suite carried for days was here: scraping the panel's text after
    // a fixed four frames captured the *pre-edit* value's text under a loaded machine, the first
    // assert then passed vacuously against the stale text, and every later assert chased a wrong
    // expectation. The expected text must come from what was set, never from what is showing.
    app.app().set_parameter(param, 0.30);
    let patch_text = app.app().engine_mut().format_param(param, 0.30);
    // Slept between frames, not merely looped: the fake backend's audio thread free-runs, and
    // with the whole suite in parallel a busy poll can finish before this app's audio thread is
    // ever scheduled - the readback these frames wait for happens there.
    let mut showing = false;
    for _ in 0..64 {
        app.run();
        if app.harness.query_by_label(&patch_text).is_some() {
            showing = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        showing,
        "with nothing selected the panel shows the patch: `{patch_text}`"
    );

    // Step 4 sets something else, and the panel is now about the step.
    app.app().select_step(3);
    app.app().set_parameter(param, 0.90);
    app.run();
    assert!(
        app.harness.query_by_label(&patch_text).is_none(),
        "a selected step's own value must replace the patch's reading, not sit beside it"
    );

    // **And what it shows is the step's own value, formatted by the plugin** — not merely something
    // other than the patch. Asserting only the absence would pass if the reading vanished entirely.
    let step_text = app.app().engine_mut().format_param(param, 0.90);
    assert!(
        app.harness.query_by_label(&step_text).is_some(),
        "the step's value must be on screen, formatted as the plugin writes it: `{step_text}`"
    );

    // Leave it, and the panel is about the instrument again. **Polled, not a fixed frame
    // count**: the restore is a push through the engine, applied by the audio thread and read
    // back afterwards, and under a loaded machine four frames were not always enough - this was
    // the suite's one flake, striking about once in six full runs before it was pinned.
    app.app().deselect_step();
    let mut came_back = false;
    for _ in 0..64 {
        app.run();
        if app.harness.query_by_label(&patch_text).is_some() {
            came_back = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        came_back,
        "and leaving the step brings the patch's reading back: `{patch_text}`"
    );

    // A step that sets nothing shows the patch too, even while selected.
    app.app().select_step(5);
    app.run();
    assert!(
        app.harness.query_by_label(&patch_text).is_some(),
        "step 6 sets nothing, so the patch is the only value there is"
    );
}

fn cutoff_id(app: &mut AppHarness) -> u32 {
    app.state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|p| p.name == "Cutoff")
        .expect("mxm-mono-01 has a cutoff")
        .id
}

/// The plugin's own formatting of the cutoff's live value — the string the panel prints beside it.

#[test]
fn a_sequenced_step_and_a_sequenced_knob_are_both_marked() {
    // **A mark nothing can see is a mark that can silently stop being drawn.** Both dots are painted
    // directly rather than being widgets, so this reads the paint list — the same oracle the
    // keyboard's keys use, and the reason `painted_circles` exists.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let mut app = AppHarness::new("locks-marks", vec![bundled.clone()]);
    app.run();
    app.harness.state_mut().rescan();
    app.run();
    app.harness
        .state_mut()
        .load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    app.run();

    let before = app.painted_circles().len();

    let param = cutoff_id(&mut app);
    app.app().select_step(3);
    app.app().set_parameter(param, 0.8);
    app.run();

    // Two more dots than before: one on step 4, one beside the cutoff's name.
    let with_marks = app.painted_circles().len();
    assert_eq!(
        with_marks,
        before + 2,
        "a sequenced step and its knob must each be marked: {before} then {with_marks}"
    );

    // Deselecting leaves the step marked - it still sets something - but takes the knob's mark,
    // which is about the *selected* step and has nothing to say when there isn't one.
    app.app().deselect_step();
    app.run();
    let deselected = app.painted_circles().len();
    assert_eq!(
        deselected,
        before + 1,
        "the step still sets it; the knob is about the selection: {with_marks} then {deselected}"
    );

    // And clearing the sequence takes both.
    app.app().clear_pattern();
    app.run();
    assert_eq!(
        app.painted_circles().len(),
        before,
        "Clear takes the locks, so it must take their marks"
    );
}

#[test]
fn an_export_carries_what_the_steps_set() {
    // `sequencer::export` renders through a **second plugin instance** with the live one's state
    // restored, precisely so an export sounds like what was being listened to. The restored state is
    // the patch; the locks are the deviations from it, and an export that dropped them would sound
    // like the patch — the same failure the state transfer exists to prevent, one level down.
    let Some(mut session) = session("seq-export-locks") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_tempo(120.0);
    for step in 0..4 {
        session.app().toggle_step_note(step, 60);
    }
    // A wide-open filter, so the difference a lock makes is audible rather than subtle.
    session.app().set_parameter(param, 0.95);
    session.advance_blocks(4).expect("advance");
    let open = std::fs::read(session.app().export_audio("open").expect("export")).expect("read");

    // Two of the four steps close the filter right down.
    for step in [1usize, 3] {
        session.app().select_step(step);
        session.app().set_parameter(param, 0.05);
    }
    session.app().deselect_step();

    // **Put the live value back where it was.** Writing a lock also sends it, so you hear what you
    // are setting - which means the captured patch has moved too, and an export compared without
    // this would differ because of the patch rather than because of the locks. Restoring it is what
    // makes the difference attributable: same patch, same notes, only the steps changed.
    session.app().set_parameter(param, 0.95);
    session.advance_blocks(4).expect("advance");
    let sequenced =
        std::fs::read(session.app().export_audio("sequenced").expect("export")).expect("read");

    assert_ne!(
        open, sequenced,
        "the export must carry what the steps set, not just the patch they deviate from"
    );
}

#[test]
fn a_locked_parameter_is_scheduled_before_the_note_it_shapes() {
    // The ordering the whole feature rests on, checked where an export can be checked: a note
    // sounded before its cutoff arrives is a note played on the previous step's sound.
    use mxm_player::sequencer::export;
    use mxm_player::sequencer::locks::LockSet;
    use mxm_player::sequencer::pattern::Pattern;

    let mut locks = LockSet::EMPTY;
    locks.set(2, 7, 0.25, 0.5).expect("room");
    locks.set(2, 3, 0.75, 0.5).expect("room");

    let mut pattern = Pattern::empty();
    pattern.toggle(2, 60);

    let scheduled = export::schedule_locks(&pattern, &locks, 120.0, 48_000.0);
    let notes = export::schedule(&pattern, 120.0, 48_000.0);

    let note_frame = notes
        .iter()
        .find(|(_, _, on)| *on)
        .map(|(frame, _, _)| *frame)
        .expect("the note");

    // **Every step carries every locked parameter**, because an offset left in force would hold the
    // parameter there for the rest of the bar - so an export that visited only the deviating steps
    // would sound different from live playback, which is the one thing it must never do.
    let moved: Vec<&(usize, mxm_player::sequencer::locks::LockKey, f32)> =
        scheduled.iter().filter(|(_, _, v)| *v != 0.0).collect();
    assert_eq!(moved.len(), 2, "only step 3 deviates: {moved:?}");
    for (frame, param_id, _) in &moved {
        assert_eq!(
            *frame, note_frame,
            "parameter {param_id} must deviate on the step it belongs to"
        );
    }

    // Ordered by parameter id within a step, so a render is a function of the sequence and not of
    // which knob was touched first.
    let ids: Vec<mxm_player::sequencer::locks::LockKey> =
        moved.iter().map(|(_, id, _)| *id).collect();
    assert_eq!(ids, vec![3, 7]);

    // **And a zero at the end of the bar.** An export is the pattern plus a mandatory tail bar; the
    // pattern does not wrap, so without this the last step's offset stays applied through the whole
    // tail and the release rings out under a deviation that live playback would have taken off.
    let bar = export::frames_per_bar(120.0, 48_000.0);
    for param in [3u32, 7] {
        assert!(
            scheduled
                .iter()
                .any(|(frame, id, value)| *frame == bar && *id == param && *value == 0.0),
            "parameter {param} must be zeroed where the pattern ends: {scheduled:?}"
        );
    }
}

#[test]
fn a_mapped_knob_writes_into_a_selected_step_just_as_the_panel_does() {
    // **The path that would otherwise ship half-working.** Sequencing that works from the on-screen
    // panel and does nothing from the hardware knob beside it is worse than sequencing that does not
    // work at all, because you would believe you had recorded something.
    let Some(mut session) = session("locks-cc") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().select_step(2);
    // CC 74 is the collection's filter cutoff, per docs/MXM_CONTROL_MAP.md. A sweep rather than one
    // message: a mapped knob has to **pick up** the parameter before it takes effect, so the first
    // value is absorbed by design and a single message would prove nothing either way.
    for value in [127u8, 96, 64, 32, 100] {
        session.app().send_control_change(74, value);
        session.advance_blocks(1).expect("advance");
    }
    session.app().deselect_step();

    let from_cc = locked_at(&mut session, param, 2).expect("the mapped knob wrote a lock");

    // The same value from the panel, on another step, must produce the same lock.
    session.app().select_step(4);
    session.app().set_parameter(param, f64::from(from_cc));
    session.app().deselect_step();

    assert_eq!(
        locked_at(&mut session, param, 4),
        Some(from_cc),
        "both authoring paths must produce the same thing"
    );
}

#[test]
fn what_the_steps_set_survives_a_restart() {
    // A sequencer that comes back minus its locks is worse than one that comes back empty, because
    // the notes look right.
    //
    // A second `PlayerApp` over the same sandboxed directory is what a restart *is* — there is no
    // shutdown hook to fake, and the settings file is the only thing that crosses.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let dir = std::env::temp_dir().join("mxm-player-locks-restart");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");

    let value = {
        let mut app = sandboxed(&dir, &bundled);
        let param = cutoff_of(&mut app);

        app.toggle_step_note(0, 60);
        app.select_step(7);
        app.set_parameter(param, 0.42);
        app.deselect_step();
        // The debounce holds the file back; ending the session is what flushes it.
        app.flush_settings();
        (param, 0.42f32)
    };

    let mut app = sandboxed(&dir, &bundled);
    let locked = app
        .sequencer_state()
        .locks
        .get(7, value.0)
        .expect("what step 8 sets must come back with the pattern");
    assert!(
        (locked - value.1).abs() < 1e-6,
        "and come back as the same value: {locked}"
    );
    assert_eq!(
        app.state().sequencer.steps[0],
        vec!["C3".to_owned()],
        "the notes come back too, as they always did"
    );
}

/// A player in `dir`, with mxm-mono-01 loaded — a launch, as far as the settings file is concerned.
fn sandboxed(dir: &std::path::Path, bundled: &std::path::Path) -> mxm_player::ui::PlayerApp {
    let config = mxm_player::config::PlayerConfig::sandboxed(
        dir,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    )
    .with_search_paths(vec![bundled.to_path_buf()]);
    let mut app = mxm_player::ui::PlayerApp::with_config(config);
    app.load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    app
}

fn cutoff_of(app: &mut mxm_player::ui::PlayerApp) -> u32 {
    app.state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|p| p.name == "Cutoff")
        .expect("mxm-mono-01 has a cutoff")
        .id
}

#[test]
fn locks_recorded_for_another_instrument_park_rather_than_playing_or_vanishing() {
    // Auditioning a second instrument is not a mistake, and it must not cost you your sequencing.
    // The two failures this rules out are opposite and both bad: applying them would move whichever
    // parameters happened to share a number, and dropping them would delete somebody's work because
    // of the order two things happen in.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let dir = std::env::temp_dir().join("mxm-player-locks-park");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");

    // A settings file whose locks belong to an instrument that is not the one about to load.
    let settings = mxm_player::settings::Settings {
        sequence_locks: Some(mxm_player::sequencer::locks::LockData {
            plugin: Some("com.someone.else.synth".to_owned()),
            params: vec![mxm_player::sequencer::locks::LockedParam {
                fx: None,
                param: 0,
                patch: Some(0.5),
                steps: (0..16).map(|step| (step == 5).then_some(0.9)).collect(),
            }],
        }),
        ..Default::default()
    };
    settings
        .save(&dir.join("settings.json"))
        .expect("the settings should save");

    let mut app = sandboxed(&dir, &bundled);
    assert!(
        app.sequencer_state().locks.param_ids().is_empty(),
        "a foreign instrument's parameter numbers must not move this one's controls"
    );

    // Written back out unchanged, so the next launch of their own plugin still finds them.
    app.flush_settings();
    let text = std::fs::read_to_string(dir.join("settings.json")).expect("read");
    assert!(
        text.contains("com.someone.else.synth"),
        "parked locks must survive being written back:\n{text}"
    );
}

#[test]
fn two_identical_sessions_dump_the_same_locks() {
    // A dump is a function of the sequence, not of which knob was touched first. Arrival order is
    // exactly the kind of thing that quietly breaks that, which is why `param_ids` sorts.
    let Some(mut first) = session("locks-determinism-a") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let Some(mut second) = session("locks-determinism-b") else {
        return;
    };

    let ids: Vec<u32> = first
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .filter(|p| ["Cutoff", "Resonance", "Glide"].contains(&p.name.as_str()))
        .map(|p| p.id)
        .collect();
    assert!(ids.len() >= 2, "need a few parameters to reorder: {ids:?}");

    // The same locks, written in opposite orders.
    for (index, id) in ids.iter().enumerate() {
        first.app().select_step(index);
        first.app().set_parameter(*id, 0.25 + index as f64 * 0.1);
    }
    for (index, id) in ids.iter().enumerate().rev() {
        second.app().select_step(index);
        second.app().set_parameter(*id, 0.25 + index as f64 * 0.1);
    }
    first.app().deselect_step();
    second.app().deselect_step();

    assert_eq!(
        serde_json::to_string(&first.state().sequencer.locks).expect("serialises"),
        serde_json::to_string(&second.state().sequencer.locks).expect("serialises"),
        "two sessions that did the same thing in different orders must dump the same"
    );
}

#[test]
fn the_reported_flow_sequences_rather_than_moving_the_whole_bar() {
    // **Reported as "it does not sequence at all".** Clear, Random, select a step, turn a knob — and
    // every step sounded the same, because a parameter is one value on the plugin and nothing ever
    // put it back. The knob moved the whole bar instead of one step of it.
    let Some(mut session) = session("locks-reported") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().clear_pattern();
    session.app().randomise();

    // The patch, as it stands with nothing selected.
    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");

    session.app().select_step(2);
    session.app().set_parameter(param, 0.90);
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");

    // Step 3 sets it; nothing else does.
    assert_eq!(locked_at(&mut session, param, 2), Some(0.9));
    assert_eq!(locked_at(&mut session, param, 1), None);

    // And the value the other fifteen put it back to is the patch — **not** the value just dialled
    // in. If the baseline had followed the edit, every step would agree with step 3 and the bar
    // would be flat again, which is exactly what was reported.
    let patch = session
        .app()
        .sequencer_state()
        .locks
        .patch(param)
        .expect("a sequenced parameter has a patch value");
    assert!(
        (patch - 0.30).abs() < 1e-6,
        "the steps that set nothing must restore the patch, not the lock: {patch}"
    );
}

#[test]
fn turning_a_knob_with_nothing_selected_moves_the_patch_the_steps_restore() {
    // Anything edited with no step selected edits the sequence-patch itself. Without this the
    // sequencer fights the knob: you turn it, and the next step puts it back where it was.
    let Some(mut session) = session("locks-patch-follows") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(2);
    session.app().set_parameter(param, 0.90);
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");

    // Now move the patch, with nothing selected.
    session.app().set_parameter(param, 0.55);
    session.advance_blocks(4).expect("advance");

    let patch = session
        .app()
        .sequencer_state()
        .locks
        .patch(param)
        .expect("a sequenced parameter has a patch value");
    assert!(
        (patch - 0.55).abs() < 1e-6,
        "the patch follows the knob: {patch}"
    );
    assert_eq!(
        locked_at(&mut session, param, 2),
        Some(0.9),
        "and what the step sets is untouched by it"
    );
}

#[test]
fn a_toggle_clicked_back_to_the_patch_clears_the_lock() {
    // **The reported flow.** Lock a toggle-shaped parameter on a step from the plugin's own
    // editor, then click it back to the patch value: off, and no dot. It failed only when the
    // click's begin/value/end straddled two service turns — the value then committed through the
    // drag path, which pins at the patch instead of clearing — so this drives exactly that split.
    let Some(mut session) = session("locks-click-to-patch") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30); // the patch
    session.advance_blocks(4).expect("advance");

    // Click "on" in the plugin's editor, split across turns: begin+value, then end.
    session.app().select_step(4);
    session.app().plugin_gesture_began(param);
    session.app().plugin_moved(&[(param, 0.80)]);
    session.advance_blocks(2).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(2).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.8),
        "the premise: the click on wrote the lock"
    );

    // Click back to the patch, same split. One value landing on the patch is a click, and a
    // click back to the patch is the reset: the step sets nothing.
    session.app().plugin_gesture_began(param);
    session.app().plugin_moved(&[(param, 0.30)]);
    session.advance_blocks(2).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(2).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        None,
        "clicking a control back to the patch clears the lock — off, and no dot"
    );

    // The same click arriving in one batch takes the instantaneous branch; both timings must
    // agree, or the gesture works by luck.
    session.app().plugin_moved(&[(param, 0.80)]);
    session.advance_blocks(2).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.8),
        "the premise again, through the one-batch path"
    );
    session.app().plugin_moved(&[(param, 0.30)]);
    session.advance_blocks(2).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        None,
        "the one-batch click clears too"
    );
}

#[test]
fn a_toggle_clicked_on_at_rest_stays_on_while_the_step_is_selected() {
    // **The other half of the reported toggle.** Clicking On wrote the lock and then put the
    // plugin's base straight back to the patch, so the editor's toggle sprang to Off in the same
    // instant — a control that reads as dead however correct the bookkeeping. At rest an
    // instantaneous edit now parks, exactly as a drag's release does: the base keeps the clicked
    // value (the toggle shows On), the preview for it is zero, and leaving the step returns the
    // patch.
    let Some(mut session) = session("locks-click-parks") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");

    session.app().select_step(4);
    session.app().plugin_moved(&[(param, 0.80)]); // an instantaneous editor click
    session.advance_blocks(2).expect("advance");

    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.8),
        "the click wrote the lock"
    );
    let dump = session.app().run_cli_command("dump");
    assert!(
        dump.contains(&format!("\"parked_bases\":[{param}]")),
        "the base is parked at the clicked value, so the control shows what was set"
    );

    // Clicking back to the patch is still the reset: off, no dot, nothing parked.
    session.app().plugin_moved(&[(param, 0.30)]);
    session.advance_blocks(2).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        None,
        "the click back to the patch clears"
    );
    let dump = session.app().run_cli_command("dump");
    assert!(
        dump.contains("\"parked_bases\":[]"),
        "nothing stays parked after the clear"
    );
}

#[test]
fn an_export_keeps_the_legato_joint_in_order() {
    // The renderer feeds a plugin exactly as live playback does, so at a slide the new note must
    // arrive while the old is still held — on before off at the joint frame. A plain
    // off-before-on sort handed a glide-on-legato instrument two disjoint notes at every slide:
    // a retrigger in the render where live playback slides, which is the one thing export must
    // never do. Ordinary boundaries keep off-before-on, so a repeated pitch stays unambiguous.
    use mxm_player::sequencer::export;
    use mxm_player::sequencer::pattern::Pattern;

    let mut pattern = Pattern::empty();
    pattern.toggle(0, 48);
    pattern.set_tied(1, true);
    pattern.toggle(1, 60); // a slide on step 2
    pattern.toggle(4, 50); // an ordinary run...
    pattern.set_tied(5, true);
    pattern.toggle(6, 52); // ...ending exactly where the next note starts

    let events = export::schedule(&pattern, 120.0, 48_000.0);
    let step = export::frames_per_step(120.0, 48_000.0);

    let joint: Vec<(u8, bool)> = events
        .iter()
        .filter(|(frame, _, _)| *frame == step)
        .map(|(_, key, on)| (*key, *on))
        .collect();
    assert_eq!(
        joint,
        vec![(60, true), (48, false)],
        "at the slide the new note sounds before the old releases"
    );

    let boundary: Vec<(u8, bool)> = events
        .iter()
        .filter(|(frame, _, _)| *frame == step * 6)
        .map(|(_, key, on)| (*key, *on))
        .collect();
    assert_eq!(
        boundary,
        vec![(50, false), (52, true)],
        "an ordinary run end still releases before the next note sounds"
    );
}

#[test]
fn an_export_merges_a_same_pitch_slide_into_one_unbroken_note() {
    // The schedule renders as raw MIDI, which carries no voice identity: a slide from C3 to C3
    // would send note-on then note-off for the same key at one frame, and the off can only kill
    // the note that just sounded — silence in the render where live playback holds. Audibly a
    // same-pitch slide is a continuation, so the render sends nothing at the joint and one off
    // at the slide's own end.
    use mxm_player::sequencer::export;
    use mxm_player::sequencer::pattern::Pattern;

    let mut pattern = Pattern::empty();
    pattern.toggle(0, 48);
    pattern.set_tied(1, true);
    pattern.toggle(1, 48); // a slide to the same pitch

    let events = export::schedule(&pattern, 120.0, 48_000.0);
    let step = export::frames_per_step(120.0, 48_000.0);

    assert_eq!(
        events,
        vec![(0, 48, true), (step + step / 2, 48, false)],
        "one note, from the head's start to the slide's own gate — no events at the joint"
    );
}

#[test]
fn a_slide_can_wrap_the_loop_point_like_a_hold_does() {
    // Live playback holds the last step's note across the repeat, so a pitch played into a tied
    // first step has a head to slide from — refusing it contradicted both the runtime and the
    // row's own hint, which tells you to tie the first step to carry across the loop.
    let mut harness = AppHarness::new("seq-slide-wrap", Vec::new());
    harness.run();

    harness.app().toggle_step_note(15, 60);
    harness.app().toggle_step_tie(0);
    harness.app().select_step(0);
    harness.app().note_on(64, 0.8);
    harness.run();

    assert!(
        harness.app().pattern().step(0).contains(64),
        "the pitch lands: step 1 slides from step 16 across the loop point"
    );
    assert!(harness.app().pattern().tied(0));

    // And deleting the note it continues leaves it alone: no edit reaches across the wrap to
    // rewrite a step the gesture never named.
    harness.app().toggle_step_note(15, 60);
    harness.run();
    assert!(
        harness.app().pattern().step(15).is_empty(),
        "the wrapped head deletes, unguarded"
    );
    assert!(
        harness.app().pattern().tied(0) && harness.app().pattern().step(0).contains(64),
        "and step 1 keeps both its tie and its note"
    );
}

#[test]
fn a_load_keeps_a_slide_that_reaches_no_note_exactly_as_written() {
    // The file is true to the interface, and the interface allows this state now — so the load
    // stores it as written rather than repairing it. There is nothing to report: a tie reaching
    // no note is a pattern with nothing to hold yet, and the runtime plays it by starting the
    // sound at the note.
    let Some(mut session) = session("seq-load-orphan-slide") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().toggle_step_note(3, 64); // steps 1-3 rest
    session.app().force_tie_for_test(3); // bypasses the guard: an orphan slide
    let path = session.app().save_sequence("orphan-slide").expect("save");

    session.app().clear_pattern();
    session.app().load_sequence(&path).expect("load");

    assert!(
        session.app().pattern().tied(3),
        "the tie comes back as authored"
    );
    assert!(
        session.app().pattern().step(3).contains(64),
        "and so does the note it sits on"
    );
    assert!(
        session
            .app()
            .sequence_problems()
            .iter()
            .all(|p| !p.contains("slide from")),
        "nothing to report: it was never damage: {:?}",
        session.app().sequence_problems()
    );
}

#[test]
fn a_half_built_pattern_of_empty_ties_round_trips_through_a_save() {
    // `is_blank`'s contract: ties sketched before their notes are work in progress, silent and
    // legal, and a save must not lose them. Nothing is repaired on load any more, for ties of
    // either kind, so an authored scaffold comes back exactly as written.
    let Some(mut session) = session("seq-half-built-roundtrip") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // The click gesture's own path: tie empty steps with nothing sounding before them.
    session.app().select_step(4);
    session.app().select_step(4); // second click ties step 5
    session.app().select_step(7);
    session.app().select_step(7); // and step 8
    let path = session.app().save_sequence("half-built").expect("save");

    session.app().clear_pattern();
    session.app().load_sequence(&path).expect("load");

    assert!(
        session.app().pattern().tied(4) && session.app().pattern().tied(7),
        "the sketched ties survive the round trip"
    );
}

#[test]
fn untying_a_hold_between_a_head_and_a_slide_leaves_the_slide_alone() {
    // Breaking the joint breaks only the joint. The slide behind it keeps its tie and its note
    // and now has nothing in front to continue, which is a legal pattern and a silent gap — not
    // a state to repair. One click, one step: that is the whole of what an edit may touch.
    let mut harness = AppHarness::new("seq-untie-strands-slide", Vec::new());
    harness.run();

    harness.app().toggle_step_note(0, 60);
    harness.app().toggle_step_tie(1); // a hold
    harness.app().toggle_step_tie(2); // another hold...
    harness.app().select_step(2);
    harness.app().note_on(67, 0.8); // ...which the pitch turns into a slide
    harness.run();
    assert!(
        harness.app().pattern().tied(2) && harness.app().pattern().step(2).contains(67),
        "the premise: step 3 is a slide"
    );

    harness.app().toggle_step_tie(1); // untie the hold between them
    harness.run();

    assert!(
        !harness.app().pattern().tied(1),
        "the hold clicked is untied"
    );
    assert!(
        harness.app().pattern().tied(2),
        "and the slide behind it keeps its tie: the click named one step"
    );
    assert!(
        harness.app().pattern().step(2).contains(67),
        "along with its note"
    );
}

#[test]
fn a_lock_made_live_survives_stop_and_the_editor_can_still_remove_it() {
    // **The reported repro, end to end.** Tie two notes, select the slide, click Slide On while
    // playing (lock written, base unparked — live rules), then Stop with the step still
    // selected. At rest the runtime previews the lock as modulation, so the editor's toggle
    // showed On over a base of Off — and clicking it Off wrote a base that was already Off:
    // **no event**, a click the player never saw, a lock that could never be removed. Stopping
    // now parks the selected step's locks onto the base, so the editor shows the truth and its
    // Off click emits a real 0.0.
    let Some(mut session) = session("locks-live-then-stop") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.0); // the patch: Off
    session.advance_blocks(4).expect("advance");
    session.app().toggle_step_note(0, 48);
    session.app().toggle_step_note(1, 60);
    session.app().toggle_step_tie(1); // the slide
    session.app().select_step(1);
    session.app().play_from_start();
    session.advance_blocks(4).expect("advance");

    // Slide On, clicked in the editor while playing.
    session.app().plugin_gesture_began(param);
    session.app().plugin_moved(&[(param, 1.0)]);
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(2).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(1, param),
        Some(1.0),
        "the premise: the lock was written mid-playback"
    );

    // Stop, step still selected: the lock parks onto the base.
    session.app().stop_sequencer();
    session.advance_blocks(2).expect("advance");
    let dump = session.app().run_cli_command("dump");
    assert!(
        dump.contains(&format!("\"parked_bases\":[{param}]")),
        "stopping parks the selected step's lock onto the base, so the editor shows On"
    );

    // The editor now shows On through the base, so the Off click emits a real 0.0 — and clears.
    session.app().plugin_gesture_began(param);
    session.app().plugin_moved(&[(param, 0.0)]);
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(2).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(1, param),
        None,
        "the Off click removes the lock — the control is alive again after Stop"
    );
}

#[test]
fn selecting_a_locked_step_at_rest_parks_its_locks() {
    // The other door into the same dead state: a lock that exists before the step is selected —
    // written in an earlier selection, or loaded from a file — previews as modulation on
    // selection, and the editor's controls go dead the same way. Selecting at rest parks too.
    let Some(mut session) = session("locks-select-parks") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(4);
    session.app().set_parameter(param, 0.80); // a lock, made from the panel
    session.advance_blocks(2).expect("advance");
    session.app().deselect_step();
    session.advance_blocks(2).expect("advance");

    session.app().select_step(4);
    session.advance_blocks(2).expect("advance");
    let dump = session.app().run_cli_command("dump");
    assert!(
        dump.contains(&format!("\"parked_bases\":[{param}]")),
        "selecting a locked step at rest parks its lock onto the base"
    );

    session.app().deselect_step();
    session.advance_blocks(2).expect("advance");
    let dump = session.app().run_cli_command("dump");
    assert!(
        dump.contains("\"parked_bases\":[]"),
        "and leaving the step unparks, back to the patch"
    );
}

#[test]
fn a_toggle_click_alternates_the_lock_while_the_transport_runs() {
    // **The reported repro, live.** While playing, the base cannot park — it springs to the
    // patch the moment the hand lets go — so a toggle whose lock is On always shows Off in the
    // plugin's editor, and every click emits On. Without the re-click rule the lock could be
    // written once and never removed while playing: the dot was stuck until Stop. A click that
    // writes exactly what the step already locks is the same toggle clicked again, and it
    // clears; the next click writes it back. On, off, on — the finger's meaning.
    let Some(mut session) = session("locks-live-toggle") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.0); // the patch: Off
    session.advance_blocks(4).expect("advance");
    session.app().toggle_step_note(0, 48);
    session.app().select_step(0);
    session.app().play_from_start();
    session.advance_blocks(4).expect("advance");

    let click = |session: &mut Session| {
        session.app().plugin_gesture_began(param);
        session.app().plugin_moved(&[(param, 1.0)]); // live, the toggle can only emit On
        session.app().plugin_gesture_ended(param);
        session.advance_blocks(2).expect("advance");
    };

    click(&mut session);
    assert_eq!(
        session.app().sequencer_state().locks.get(0, param),
        Some(1.0),
        "the first click writes the lock, mid-playback"
    );

    click(&mut session);
    assert_eq!(
        session.app().sequencer_state().locks.get(0, param),
        None,
        "the same click again clears it — a toggle's second click means off"
    );

    click(&mut session);
    assert_eq!(
        session.app().sequencer_state().locks.get(0, param),
        Some(1.0),
        "and the third writes it back: the click alternates"
    );
}

#[test]
fn two_clicks_on_one_toggle_in_one_batch_stay_two_clicks() {
    // A fast on-then-off can land in a single drained turn. Interpreted as one gesture it
    // counts two values — a "drag" — and the second click, landing on the patch, pins a lock
    // instead of clearing. Single-parameter batches are interpreted strictly in queue order,
    // so each click keeps its own meaning.
    use mxm_player::engine::PluginOutput;

    let Some(mut session) = session("locks-double-click-batch") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(4);

    session.app().receive_plugin_outputs(vec![
        PluginOutput::GestureBegin { param_id: param },
        PluginOutput::ParamValue {
            param_id: param,
            value: 0.80,
        },
        PluginOutput::GestureEnd { param_id: param },
        PluginOutput::GestureBegin { param_id: param },
        PluginOutput::ParamValue {
            param_id: param,
            value: 0.30,
        },
        PluginOutput::GestureEnd { param_id: param },
    ]);
    session.advance_blocks(2).expect("advance");

    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        None,
        "click on, click off: the second click clears, whatever turn it lands in"
    );
}

#[test]
fn a_queued_editor_batch_is_interpreted_in_queue_order() {
    // The drain path itself, fed a synthetic batch: begin, values, end in ONE drained turn —
    // the shape a real editor drag takes when the player's service interval spans it. Acting on
    // the end before the values reached `plugin_moved` committed the previous turn's pending
    // edit while this turn's drag sat unprocessed, and the drag then fell through the
    // instantaneous branch: ending on the patch, it cleared a lock it should pin.
    use mxm_player::engine::PluginOutput;

    let Some(mut session) = session("locks-queued-batch") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(4);

    // A whole drag in one drained batch, settling on the patch value: it must pin.
    session.app().receive_plugin_outputs(vec![
        PluginOutput::GestureBegin { param_id: param },
        PluginOutput::ParamValue {
            param_id: param,
            value: 0.60,
        },
        PluginOutput::ParamValue {
            param_id: param,
            value: 0.30,
        },
        PluginOutput::GestureEnd { param_id: param },
    ]);
    session.advance_blocks(2).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.3),
        "a one-batch drag ending on the patch pins, in queue order"
    );

    // And a one-batch click back to the patch clears — the same path, one value.
    session.app().receive_plugin_outputs(vec![
        PluginOutput::GestureBegin { param_id: param },
        PluginOutput::ParamValue {
            param_id: param,
            value: 0.30,
        },
        PluginOutput::GestureEnd { param_id: param },
    ]);
    session.advance_blocks(2).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        None,
        "a one-batch click back to the patch clears"
    );
}

#[test]
fn a_preset_burst_with_a_step_selected_is_still_a_patch_change() {
    // The batch's *size* is the preset heuristic, so the drain must interpret a turn's values
    // together: a preset load arrives as begin/value/end per parameter, and split at each
    // gesture boundary it reads as sequencing — a whole instrument's worth of locks written
    // into whichever step happened to be selected, which is the *"I can only ever use one
    // sound with a sequence"* fault the heuristic exists to prevent.
    use mxm_player::engine::PluginOutput;

    let Some(mut session) = session("locks-preset-burst") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let params: Vec<u32> = session
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .map(|param| param.id)
        .take(3)
        .collect();
    assert!(params.len() >= 3, "the premise needs several parameters");

    session.app().select_step(4);
    let mut burst = Vec::new();
    for (index, id) in params.iter().enumerate() {
        burst.push(PluginOutput::GestureBegin { param_id: *id });
        burst.push(PluginOutput::ParamValue {
            param_id: *id,
            value: 0.2 + index as f64 * 0.1,
        });
        burst.push(PluginOutput::GestureEnd { param_id: *id });
    }
    session.app().receive_plugin_outputs(burst);
    session.advance_blocks(2).expect("advance");

    for id in &params {
        assert_eq!(
            session.app().sequencer_state().locks.get(4, *id),
            None,
            "a preset burst writes the patch, never locks into the selected step"
        );
    }
}

#[test]
fn a_drag_whose_frames_arrive_in_one_batch_still_pins_at_the_patch() {
    // A drag's values can queue up and arrive as one service turn's batch, collapsed to the
    // final value — which must not make it count as a click. Counting the batch as one value
    // let a quick drag ending on the patch clear a lock it should pin.
    let Some(mut session) = session("locks-batched-drag") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");

    session.app().select_step(4);
    session.app().plugin_gesture_began(param);
    // The whole drag in one batch, settling on the patch value.
    session
        .app()
        .plugin_moved(&[(param, 0.60), (param, 0.45), (param, 0.30)]);
    session.advance_blocks(1).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(2).expect("advance");

    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.3),
        "a batched drag ending on the patch is still a drag: it pins"
    );
}

#[test]
fn a_drag_ending_on_the_patch_value_still_pins() {
    // The other half of the pair of rulings: a click clears, a **drag** pins. A drag is many
    // values; its ending on the patch is a hand releasing there, and the absolute-lock rule —
    // a step pinned to today's patch stays pinned when the patch moves — survives for it.
    let Some(mut session) = session("locks-drag-to-patch") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");

    session.app().select_step(4);
    session.app().plugin_gesture_began(param);
    session.app().plugin_moved(&[(param, 0.60)]);
    session.advance_blocks(1).expect("advance");
    session.app().plugin_moved(&[(param, 0.45)]);
    session.advance_blocks(1).expect("advance");
    session.app().plugin_moved(&[(param, 0.30)]); // the hand settles on the patch value
    session.advance_blocks(1).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(2).expect("advance");

    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.3),
        "a drag ending on the patch value is a deliberate pin, not a click to clear"
    );
}

#[test]
fn a_lock_at_the_patch_value_is_still_a_lock() {
    // **"The lock must be an absolute value"** — the user's words, and the semantics an earlier rule
    // broke: it cleared a lock whenever an edit landed on the patch value, reasoning the two sound
    // the same. They do, today; a step pinned to today's patch value must stay pinned when the patch
    // moves tomorrow. The collapse made the step follow every later patch edit, which is exactly a
    // relative lock. Watched happen on screen before this test existed: a drag ending on the patch
    // value silently deleted the lock.
    let Some(mut session) = session("locks-absolute-at-patch") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");

    // Lock the step at exactly the patch value.
    session.app().select_step(4);
    session.app().set_parameter(param, 0.30);
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.3),
        "a lock at the patch value is a real lock, not a no-op to collapse"
    );
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");

    // Move the patch. The step must stay where it was pinned.
    session.app().set_parameter(param, 0.70);
    session.advance_blocks(4).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.3),
        "the patch moved and the lock did not - absolute, as stated"
    );

    // Clearing stays a deliberate act: the panel's double-click reset.
    session.app().select_step(4);
    session.app().reset_parameter(param);
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        None,
        "the reset gesture still clears"
    );
}

/// **What the instrument is sounding**: the parameter's value plus what the step is adding.
///
/// The two are separate on purpose — the value is what a knob is set to, the modulation is what the
/// sequencer is laying over it — so a test about what you *hear* has to add them, and a test about
/// what a control *shows* must not.
fn sounding(session: &mut Session, param: u32) -> f64 {
    let state = session.state();
    let p = state
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|p| p.id == param)
        .cloned()
        .expect("the parameter");
    p.value + p.modulation
}

/// What the panel shows for a parameter: its own value, with nothing added.
fn live(session: &mut Session, param: u32) -> f64 {
    session
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|p| p.id == param)
        .expect("the parameter")
        .value
}

/// The values a parameter takes over `bars` bars, sampled several times a step.
///
/// Sampled rather than read once, because what is wrong with an animated parameter is not where it
/// ends up but that it **moves**: a single reading lands wherever the playhead happens to be and
/// says nothing about the fifteen other steps.
fn watch(session: &mut Session, param: u32, bars: usize) -> Vec<f64> {
    let mut seen = Vec::new();
    for _ in 0..(bars * 16 * 3) {
        session.advance_blocks(4).expect("advance");
        seen.push(sounding(session, param));
    }
    seen
}

#[test]
fn a_value_set_during_playback_is_what_the_unlocked_steps_play() {
    // **Reported as "I cannot set a value on a running sequence".** The first fix stood the
    // sequencer down while a knob was held. Modulation makes that unnecessary and, kept, actively
    // wrong: the hand moves the parameter's *value* and the sequencer moves an *offset* laid over
    // it, so they are different layers and cannot fight. Turning a knob during playback sets the
    // patch, and every step that does not override it plays exactly that.
    let Some(mut session) = session("locks-live-edit") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.20);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(0);
    session.app().set_parameter(param, 0.90);
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");

    sharpen(&mut session);
    session.app().play_from_start();

    // Move the patch while it runs — with nothing selected, which is what editing the patch is.
    session.app().begin_parameter_gesture(param);
    session.app().set_parameter(param, 0.55);
    let seen = watch(&mut session, param, 2);
    session.app().end_parameter_gesture(param);

    // Most steps play what was just dialled in. **The edit stuck**, which is the whole report.
    assert!(
        seen.iter().filter(|v| (*v - 0.55).abs() < 1e-3).count() > seen.len() / 2,
        "the value set by hand is what the unlocked steps play: {seen:?}"
    );

    // And step 1 still does its own thing, because a lock is an absolute value and it is measured
    // from the patch that is now in force.
    assert!(
        seen.iter().any(|v| (*v - 0.90).abs() < 1e-3),
        "the step that sets it still sets it: {seen:?}"
    );
}

#[test]
fn leaving_a_step_does_not_make_its_value_the_patch() {
    // **The hole that made the collapse come back.** Editing a step sends the value to the plugin so
    // you hear it — which leaves the instrument sitting at that step's value. The patch follows the
    // instrument whenever no step is selected, so the moment you deselected, the step's value became
    // the patch: every step then agreed with the one you had just edited, and the sequence was flat
    // again for the second time.
    //
    // Driven through frames rather than through a `Session`, because it is `follow_sequence_patch`
    // running on the frame loop that does the damage.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let mut app = AppHarness::new("locks-deselect", vec![bundled.clone()]);
    app.run();
    app.harness.state_mut().rescan();
    app.run();
    app.harness
        .state_mut()
        .load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    app.run();

    let param = cutoff_id(&mut app);

    // The patch. Several frames, so that the requery a completed edit asks for definitely lands:
    // it reads the plugin, which has not consumed the edit yet, so `params` reverts to the value
    // from before it. What the patch must follow is what the **control is showing**, not that.
    app.app().set_parameter(param, 0.20);
    for _ in 0..4 {
        app.run();
    }

    // One step deviates from it.
    app.app().select_step(3);
    app.app().set_parameter(param, 0.90);
    app.run();

    // And now leave the step, as anybody would.
    app.app().deselect_step();
    app.run();
    app.run();

    let patch = app
        .app()
        .sequencer_state()
        .locks
        .patch(param)
        .expect("a sequenced parameter has a patch value");
    assert!(
        (patch - 0.20).abs() < 1e-3,
        "leaving a step must not turn its value into the patch: {patch}"
    );
    assert_eq!(
        app.app().sequencer_state().locks.get(3, param),
        Some(0.9),
        "and the step keeps what it was given"
    );
}

#[test]
fn choosing_a_patch_replaces_the_sequence_patch_rather_than_filling_a_step() {
    // **Reported as "I can only ever use one sound with a sequence".** A preset load reaches the
    // player as a value per parameter, exactly as a knob turn does — so with a step selected the
    // whole instrument was written into that one step as locks, and every step then sounded the
    // same. One parameter is a hand on a knob; a whole patch arriving together is a patch.
    let Some(mut session) = session("locks-patch-change") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);
    let others: Vec<u32> = session
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .map(|p| p.id)
        .filter(|id| *id != param)
        .take(4)
        .collect();

    // A sequence: a patch, and one step that deviates from it.
    session.app().set_parameter(param, 0.20);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(3);
    session.app().set_parameter(param, 0.90);
    session.advance_blocks(4).expect("advance");

    // Now choose a different patch — while the step is still selected, which is the case that broke.
    let mut patch: Vec<(u32, f64)> = vec![(param, 0.45)];
    patch.extend(others.iter().map(|id| (*id, 0.6)));
    session.app().plugin_moved(&patch);

    // The step keeps what it was given, and gains nothing.
    assert_eq!(
        locked_at(&mut session, param, 3),
        Some(0.9),
        "the step it was already sequencing is not disturbed"
    );
    for id in &others {
        assert_eq!(
            locked_at(&mut session, *id, 3),
            None,
            "a patch must not be written into a step as {} locks",
            others.len()
        );
    }

    // And what the unlocked steps restore is the new patch.
    let restored = session
        .app()
        .sequencer_state()
        .locks
        .patch(param)
        .expect("a sequenced parameter has a patch value");
    assert!(
        (restored - 0.45).abs() < 1e-6,
        "choosing a patch is choosing what the sequence deviates from: {restored}"
    );
}

#[test]
fn one_knob_in_the_plugins_editor_still_sequences() {
    // The other half of the same rule, and the thing that must not be broken by fixing the first: a
    // single parameter arriving from the plugin is somebody turning a knob, and with a step selected
    // that is how you sequence from the instrument's own interface.
    let Some(mut session) = session("locks-one-knob") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.20);
    session.advance_blocks(4).expect("advance");

    session.app().select_step(5);
    session.app().plugin_moved(&[(param, 0.75)]);

    assert_eq!(
        locked_at(&mut session, param, 5),
        Some(0.75),
        "one knob at a time is a hand, and a selected step takes it"
    );
}

#[test]
fn a_patch_change_is_not_undone_by_what_the_panel_last_sent() {
    // `editing` holds what the panel last sent and wins when the panel draws — and the sequence-patch
    // follows what the controls show. A stale entry therefore hid the value that had just arrived
    // and put the old one back on the next frame, so a patch change never registered at all.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let mut app = AppHarness::new("locks-patch-sticks", vec![bundled.clone()]);
    app.run();
    app.harness.state_mut().rescan();
    app.run();
    app.harness
        .state_mut()
        .load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    app.run();

    let param = cutoff_id(&mut app);

    // A sequence, authored from the panel — which is what fills `editing`.
    app.app().set_parameter(param, 0.20);
    for _ in 0..4 {
        app.run();
    }
    app.app().select_step(2);
    app.app().set_parameter(param, 0.90);
    app.run();
    app.app().deselect_step();
    app.run();

    // A patch arrives from the plugin.
    let patch: Vec<(u32, f64)> = vec![(param, 0.45), (param + 1, 0.6), (param + 2, 0.6)];
    app.app().plugin_moved(&patch);
    for _ in 0..4 {
        app.run();
    }

    let restored = app
        .app()
        .sequencer_state()
        .locks
        .patch(param)
        .expect("a sequenced parameter has a patch value");
    assert!(
        (restored - 0.45).abs() < 1e-3,
        "the patch the plugin reported must stick, not be replaced by what the panel last sent: \
         {restored}"
    );
}

#[test]
fn the_panel_reads_the_parameter_not_what_the_sequence_adds_to_it() {
    // **The contamination that came with modulation.** nice-plug reports the *modulated* value to
    // the host, so a requery during playback reads the patch plus whatever offset the playhead is
    // applying. The panel must show the parameter's own value: it is the thing a knob is set to, and
    // the sequence rides over it. Without this every sequenced control jitters through the bar while
    // nothing is moving it — and worse, anything that treats the reading as the patch drifts by one
    // step's deviation per requery.
    let Some(mut session) = session("locks-readback") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.20);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(0);
    session.app().set_parameter(param, 0.90);
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");

    sharpen(&mut session);
    session.app().play_from_start();

    // Land inside the step that deviates, so the plugin really is reporting a modulated value, then
    // requery exactly as a gesture end or a settle would.
    session.advance_blocks(4).expect("advance");
    session.app().refresh_params();

    let shown = live(&mut session, param);
    assert!(
        (shown - 0.20).abs() < 1e-3,
        "the panel shows the parameter, not the parameter plus the step's offset: {shown}"
    );
}

#[test]
fn editing_a_step_modulates_rather_than_moving_the_patch() {
    // **This is what makes the instrument's own editor mark the knob**, and it is the case you are
    // actually looking at: stopped, a step selected, turning knobs. The step's value is laid over
    // the parameter as an offset, so the parameter itself — the patch — never moves, and the editor
    // can see that something other than the knob is moving it.
    let Some(mut session) = session("locks-step-mod") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");

    session.app().select_step(2);
    session.app().set_parameter(param, 0.55);
    session.advance_blocks(4).expect("advance");

    // **A second edit, after the first has been published.** Selecting a step and editing it in the
    // same breath is coalesced into one publish, which changes the selection - so keying the preview
    // on the selection alone passes that and leaves every later edit inaudible. This is the case
    // that catches it.
    session.app().set_parameter(param, 0.80);
    session.advance_blocks(4).expect("advance");

    // You hear the step.
    let heard = sounding(&mut session, param);
    assert!(
        (heard - 0.80).abs() < 1e-3,
        "editing a step must be audible, or you are sequencing blind: {heard}"
    );

    // **And the patch underneath it has not moved.** Sending a value here instead would put the
    // step's value into the parameter, and every other step would then agree with it.
    let base = live(&mut session, param);
    assert!(
        (base - 0.30).abs() < 1e-3,
        "the parameter itself is still the patch: {base}"
    );

    // Leaving the step takes the offset away, and nothing has to be written back to do it.
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");
    let after = sounding(&mut session, param);
    assert!(
        (after - 0.30).abs() < 1e-3,
        "and leaving it returns to the patch: {after}"
    );
}

#[test]
fn a_rendered_lock_reaches_the_plugin_as_modulation_and_not_as_a_value() {
    // **The honest oracle for the transport change**, and the reason it has to be an audio one.
    //
    // Every host-side assertion in this file is blind to it: `clap_params.get_value` reports the
    // *modulated* value, so a host reading back cannot tell an offset from a value, and the player's
    // own dump is corrected from its own record. Rendering is the one place the difference is
    // audible — an offset is measured from the patch, a value replaces it, so the same number sent
    // the two ways produces two different sounds unless the patch happens to be zero.
    //
    // Driven through `offline::render` rather than the player, because that is the layer where the
    // event kind is chosen and nothing above it can observe the choice.
    use mxm_player::offline::{EventKind, RenderConfig, ScheduledEvent};

    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let bundle = bundled.join("mxm-mono-01.clap");

    // **The real parameter id**, looked up through a session: mxm-mono-01's ids are hashes, not small
    // integers, and an id nothing matches makes both renders identical and the test vacuous.
    let Some(mut session) = session("oracle-id") else {
        return;
    };
    let cutoff = a_param(&mut session);
    drop(session);
    let render = |kind: EventKind| {
        let events = vec![
            ScheduledEvent { frame: 0, kind },
            ScheduledEvent {
                frame: 0,
                kind: EventKind::Midi {
                    data: [0x90, 60, 100],
                },
            },
        ];
        mxm_player::offline::render(
            &bundle,
            PLUGIN,
            RenderConfig {
                sample_rate: 48_000.0,
                block_size: 512,
                total_frames: 24_000,
                state: None,
            },
            &events,
        )
        .expect("render")
    };

    let as_offset = render(EventKind::ParamMod {
        param_id: cutoff,
        value: -0.6,
    });
    let as_value = render(EventKind::ParamValue {
        param_id: cutoff,
        value: -0.6,
    });

    let rms = |r: &mxm_player::offline::RenderResult| {
        let ch = &r.channels[0];
        (ch.iter()
            .map(|s| f64::from(*s) * f64::from(*s))
            .sum::<f64>()
            / ch.len() as f64)
            .sqrt()
    };

    // An offset of -0.6 from a wide-open patch closes the filter a long way; the same number as a
    // *value* is clamped into range and lands somewhere else entirely. If the two render alike, the
    // event kind is not reaching the plugin as the kind it was sent as.
    let offset_rms = rms(&as_offset);
    let value_rms = rms(&as_value);
    assert!(
        (offset_rms - value_rms).abs() > 1e-4,
        "an offset and a value must not sound the same: {offset_rms} vs {value_rms}"
    );
}

/// The RMS of a held note, which is how the **instrument's own state** is observed.
///
/// `show_patch_for_sequenced` makes the player's dump report the patch for a sequenced parameter
/// whatever the plugin actually has, so a corrupted base value is invisible from the host — nothing
/// in `state()` can see it and neither can `clap_params.get_value`, which reports the modulated
/// value. The sound is the only witness left.
fn held_note_rms(session: &mut Session, note: u8) -> f64 {
    session.clear_capture();
    session.app().note_on(note, 100.0 / 127.0);
    session.advance_blocks(20).expect("advance");
    session.app().note_off(note);
    session.advance_blocks(4).expect("advance");

    let mono = left(&session.captured());
    (mono
        .iter()
        .map(|s| f64::from(*s) * f64::from(*s))
        .sum::<f64>()
        / mono.len().max(1) as f64)
        .sqrt()
}

#[test]
fn every_authoring_path_leaves_the_patch_where_it_was() {
    // **The blocker a review found: only the panel followed the new model.** A mapped controller
    // sent a value before recording the lock, and an edit from the plugin's own editor had already
    // moved the parameter by the time the player saw it — so on both paths, editing a step rewrote
    // the patch out from under every other step. The panel's own path was fine, which is exactly why
    // it went unnoticed.
    let Some(mut session) = session("locks-paths-patch") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);
    let patch_sound = held_note_rms(&mut session, 60);

    // The plugin's own editor: **it changes the parameter itself and tells the player afterwards.**
    // Both halves are needed to reproduce the defect — the notification alone leaves the player's
    // model wrong but the instrument untouched, and it is the instrument that has to be put back.
    session.app().select_step(1);
    session
        .app()
        .engine_mut()
        .push_gui_event(mxm_player::events::input::Payload::ParamValue {
            param_id: param,
            value: 0.85,
        });
    session.advance_blocks(2).expect("advance");
    session.app().plugin_moved(&[(param, 0.85)]);
    session.advance_blocks(4).expect("advance");
    assert_eq!(
        locked_at(&mut session, param, 1),
        Some(0.85),
        "the step takes the value"
    );
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");

    let after_editor = held_note_rms(&mut session, 60);
    assert!(
        (after_editor - patch_sound).abs() < patch_sound * 0.05,
        "editing a step from the plugin's own editor must leave the patch alone: \
         {patch_sound} then {after_editor}"
    );

    // A mapped controller, on another step.
    session.app().select_step(3);
    for value in [127u8, 96, 64, 32, 100] {
        session.app().send_control_change(74, value);
        session.advance_blocks(1).expect("advance");
    }
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");

    let after_cc = held_note_rms(&mut session, 60);
    assert!(
        (after_cc - patch_sound).abs() < patch_sound * 0.05,
        "a hardware knob editing a step must not move the patch either: \
         {patch_sound} then {after_cc}"
    );
}

#[test]
fn clearing_a_pattern_takes_the_modulation_off_the_instrument() {
    // **Once an id leaves the lock set the sequencer can no longer emit a zero for it.** Anything
    // not taken off here stays laid over the plugin for ever — a filter half shut by a pattern that
    // does not exist any more, with nothing on screen to explain it.
    let Some(mut session) = session("locks-clear-mod") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);
    let patch_sound = held_note_rms(&mut session, 60);

    session.app().select_step(1);
    session.app().set_parameter(param, 0.85);
    session.advance_blocks(4).expect("advance");
    let with_step = held_note_rms(&mut session, 60);
    assert!(
        (with_step - patch_sound).abs() > patch_sound * 0.05,
        "the step must be audible, or this test cannot tell the two apart"
    );

    session.app().clear_pattern();
    session.advance_blocks(4).expect("advance");

    let after = held_note_rms(&mut session, 60);
    assert!(
        (after - patch_sound).abs() < patch_sound * 0.05,
        "clearing the pattern must take its deviation off the instrument: \
         {patch_sound} then {after}"
    );
}

#[test]
fn stopping_takes_the_last_steps_deviation_off() {
    // The last step to run leaves its offset in force and there is no next step to replace it. A
    // stopped sequencer is deviating from nothing, and the instrument should be sitting at the patch
    // rather than wherever the playhead happened to stop.
    let Some(mut session) = session("locks-stop-mod") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);
    let patch_sound = held_note_rms(&mut session, 60);

    session.app().select_step(0);
    session.app().set_parameter(param, 0.85);
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");

    session.app().play_from_start();
    session.advance_blocks(4).expect("advance");
    session.app().stop_sequencer();
    session.advance_blocks(8).expect("advance");

    let after = held_note_rms(&mut session, 60);
    assert!(
        (after - patch_sound).abs() < patch_sound * 0.05,
        "a stopped sequencer deviates from nothing: {patch_sound} then {after}"
    );
}

#[test]
fn a_running_sequence_audibly_deviates_on_the_locked_step_and_not_elsewhere() {
    // **The positive oracle for the live transport**, which the offline comparison does not give:
    // that one exercises `offline::push_event`, and this exercises `AudioWorker::emit_sequencer_action`.
    //
    // It has to be audio. `state()` reports what the player believes, `clap_params.get_value` reports
    // the modulated value either way, and the modulation cache is filled by a droppable notification.
    // The sound is the only thing that knows whether the runtime emitted anything at all.
    let Some(mut session) = session("locks-live-audio") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    // A note on every step, so each one is audible, and a wide-open patch.
    for step in 0..16 {
        session.app().toggle_step_note(step, 60);
    }
    session.app().set_parameter(param, 0.95);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    // A bar with no locks at all.
    session.app().set_tempo(120.0);
    session.clear_capture();
    session.app().play_from_start();
    session.advance_blocks(190).expect("advance");
    let plain = left(&session.captured());
    session.app().stop_sequencer();
    session.advance_blocks(8).expect("advance");

    // One step closes the filter hard.
    session.app().select_step(4);
    session.app().set_parameter(param, 0.05);
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");

    session.clear_capture();
    session.app().play_from_start();
    session.advance_blocks(190).expect("advance");
    let sequenced = left(&session.captured());
    session.app().stop_sequencer();

    // A step is 6,000 frames at 120 BPM. Step 5 is frames 24,000..30,000; step 1 is 0..6,000.
    let window = |samples: &[f32], from: usize, to: usize| {
        let slice = &samples[from.min(samples.len())..to.min(samples.len())];
        (slice
            .iter()
            .map(|s| f64::from(*s) * f64::from(*s))
            .sum::<f64>()
            / slice.len().max(1) as f64)
            .sqrt()
    };

    let locked_plain = window(&plain, 24_000, 30_000);
    let locked_seq = window(&sequenced, 24_000, 30_000);
    assert!(
        locked_seq < locked_plain * 0.9,
        "the locked step must audibly deviate: {locked_plain} then {locked_seq}"
    );

    let other_plain = window(&plain, 0, 6_000);
    let other_seq = window(&sequenced, 0, 6_000);
    assert!(
        (other_seq - other_plain).abs() < other_plain * 0.1,
        "and every other step must sound as it did, or the deviation is not confined to its step: \
         {other_plain} then {other_seq}"
    );
}

#[test]
fn a_sequence_written_before_baselines_still_deviates() {
    // **A file from before patch values were stored has no baseline**, and every offset would then
    // be measured from nothing — the locks would load and do nothing at all, silently, which is
    // worse than losing them because the interface still shows the dots.
    //
    // The player adopts the value the instrument is actually at, which is what an unlocked step
    // would have restored anyway.
    let Some(mut session) = session("locks-legacy-baseline") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    // A sequence file as an older build would have written it: locks, no `patch`.
    session.app().set_parameter(param, 0.40);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(2);
    session.app().set_parameter(param, 0.90);
    session.app().deselect_step();
    let path = session.app().save_sequence("legacy").expect("save");

    let text = std::fs::read_to_string(&path).expect("read");
    let stripped: String = text
        .lines()
        .filter(|line| !line.contains("\"patch\""))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, stripped).expect("write");

    session.app().clear_pattern();
    session.app().load_sequence(&path).expect("load");
    session.advance_blocks(4).expect("advance");

    let patch = session
        .app()
        .sequencer_state()
        .locks
        .patch(param)
        .expect("the player must supply a baseline the file did not carry");
    // **The instrument's own value, not the first value the file happened to lock.** Standing the
    // first lock in looks harmless and is not: every step would then deviate from a number nobody
    // chose, and the step that "set" it would deviate by nothing at all.
    assert!(
        (patch - 0.40).abs() < 1e-3,
        "the baseline must be what the instrument is set to, not the first locked value: {patch}"
    );
    assert_eq!(
        session.app().sequencer_state().locks.get(2, param),
        Some(0.9),
        "and what the step sets is unchanged"
    );
}

#[test]
fn locks_loaded_with_no_plugin_are_checked_against_the_one_that_arrives() {
    // **Loading a sequence with nothing loaded validates against an empty parameter set**, which
    // validates nothing — `prune_unknown_locks` returns early when there are no parameters to check
    // against. Without a second check when a plugin does arrive, those locks are published to it,
    // including for parameters it does not have or will not accept modulation for.
    //
    // Not the parked path: parked locks are checked when they are claimed. This is the one that goes
    // straight into the live set with no instrument to check it against.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let dir = std::env::temp_dir().join("mxm-player-locks-noplugin");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");

    let config = mxm_player::config::PlayerConfig::sandboxed(
        &dir,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    )
    .with_search_paths(vec![bundled.clone()]);
    let mut app = mxm_player::ui::PlayerApp::with_config(config);

    // A sequence naming a parameter no instrument has, loaded with nothing loaded.
    let sequence = mxm_player::sequencer::Sequence {
        schema_version: 3,
        // A version 3 file has no shape of its own: `migrate_from_v3` gives it one bar of sixteen,
        // and these are the zeroes `serde(default)` would have left.
        bars: 0,
        steps_per_bar: 0,
        name: "from nowhere".to_owned(),
        tempo: 120.0,
        steps: mxm_player::sequencer::pattern::PatternData(vec![vec!["C3".to_owned()]]),
        tied: vec![false; 16],
        locks: Some(mxm_player::sequencer::locks::LockData {
            plugin: None,
            params: vec![mxm_player::sequencer::locks::LockedParam {
                fx: None,
                param: 999_999,
                patch: Some(0.2),
                steps: (0..16).map(|step| (step == 3).then_some(0.9)).collect(),
            }],
        }),
    };
    let path = dir.join("nowhere.seq.json");
    sequence.save(&path).expect("save");
    app.load_sequence(&path).expect("load");
    assert!(
        !app.sequencer_state().locks.locks_anywhere(999_999),
        "with no instrument to check them against they wait rather than going live"
    );

    // Now an instrument arrives.
    app.load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    assert!(
        !app.sequencer_state().locks.locks_anywhere(999_999),
        "a lock for a parameter the instrument does not have must not survive loading it"
    );
}

#[test]
fn locks_do_not_survive_a_plugin_change_by_sharing_parameter_numbers() {
    // **The check that identity, not id, decides.** A set made for one instrument whose ids also
    // exist in the next passes every other test there is — the ids are present and the flags are
    // right — so without parking it under the `CLAP_ID` it was made for, it would go on playing
    // through a different synth and then be persisted under that synth's name.
    //
    // **What this can and cannot show.** The collection ships one instrument, so a genuine A-to-B
    // mismatch is not reachable from here: with only mxm-mono-01, parking and not parking produce the
    // same result, and disabling the parking leaves this test green. The comparison itself is
    // covered where it can be — `locks_recorded_for_another_plugin_are_dropped` in `locks.rs` and
    // `locks_recorded_for_another_instrument_park_rather_than_playing_or_vanishing` here. What this
    // pins is the half that *is* reachable: a reload of the same instrument brings its locks back
    // rather than losing them to the round trip through parking, and they carry its name.
    //
    // **This gap closes when MXM-303 lands**, and that is the moment to come back to it.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let dir = std::env::temp_dir().join("mxm-player-locks-identity");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");

    let config = mxm_player::config::PlayerConfig::sandboxed(
        &dir,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    )
    .with_search_paths(vec![bundled.clone()]);
    let mut app = mxm_player::ui::PlayerApp::with_config(config);
    app.load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());

    let param = app
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|p| p.name == "Cutoff")
        .expect("mxm-mono-01 has a cutoff")
        .id;

    app.set_parameter(param, 0.30);
    app.select_step(2);
    app.set_parameter(param, 0.80);
    app.deselect_step();
    assert_eq!(app.sequencer_state().locks.get(2, param), Some(0.8));

    // The same instrument again: the identity matches, so the locks come back.
    app.load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    assert_eq!(
        app.sequencer_state().locks.get(2, param),
        Some(0.8),
        "reloading the instrument they were made for must bring them back"
    );

    // And they carry that instrument's name, not merely whatever is loaded now.
    app.flush_settings();
    let text = std::fs::read_to_string(dir.join("settings.json")).expect("read");
    assert!(
        text.contains(PLUGIN),
        "the locks must record the instrument they were made for:\n{text}"
    );
}

#[test]
fn a_zero_that_will_not_fit_is_delivered_later() {
    // **The last zero for a parameter has nothing behind it.** No later step will replace it, so a
    // dropped one leaves the plugin modulated for ever — a filter half shut with nothing on screen
    // to explain it. The debt is kept separately from the display cache, which a queued report can
    // overwrite, and `service` pays it.
    let Some(mut session) = session("locks-owed-zero") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);
    let patch_sound = held_note_rms(&mut session, 60);

    session.app().select_step(1);
    session.app().set_parameter(param, 0.85);
    session.advance_blocks(2).expect("advance");

    // Fill the GUI queue so the zeroes that deselecting sends cannot all fit. `PRODUCER_QUEUE_CAPACITY`
    // is 1024; this is comfortably past it, with the engine given no chance to drain.
    for _ in 0..1400 {
        session.app().send_control_change(1, 64);
    }
    session.app().deselect_step();

    // Now let it drain and catch up.
    session.advance_blocks(40).expect("advance");

    let after = held_note_rms(&mut session, 60);
    assert!(
        (after - patch_sound).abs() < patch_sound * 0.05,
        "a zero that would not fit must still arrive: {patch_sound} then {after}"
    );
}

#[test]
fn a_knob_dragged_in_the_plugins_editor_does_not_jump_back_while_it_is_held() {
    // **Reported as "when I move the cutoff it jumps back to the original value"** — stopped, with a
    // step selected.
    //
    // The plugin's editor moves the parameter itself and tells the player afterwards. With a step
    // selected that is the patch moving, so the player puts it back — but putting it back on *every*
    // notification means putting it back on every frame of a drag, and the knob fights the hand.
    // A drag is bracketed by gestures, and that is the seam: while one is open the parameter is
    // somebody's to move, and the base is restored when they let go.
    //
    // **Driven through `AppHarness`, not `Session`.** `Session::state()` requeries first, and a
    // requery corrects a *locked* parameter's reading to the patch by design — so it would report
    // the patch whatever the player did, and the test would pass without the fix.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let mut app = AppHarness::new("locks-drag-back", vec![bundled.clone()]);
    app.run();
    app.harness.state_mut().rescan();
    app.run();
    app.harness
        .state_mut()
        .load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    app.run();

    let param = cutoff_id(&mut app);
    let shown = |app: &mut AppHarness| {
        app.state()
            .plugin
            .expect("a plugin is loaded")
            .params
            .iter()
            .find(|p| p.id == param)
            .expect("the parameter")
            .value
    };

    app.app().set_parameter(param, 0.30);
    for _ in 0..4 {
        app.run();
    }
    app.app().select_step(4);

    // A drag in the plugin's own editor: it opens a gesture, moves its parameter, and reports.
    app.app().plugin_gesture_began(param);
    for value in [0.50f64, 0.60, 0.70, 0.80] {
        app.app().plugin_moved(&[(param, value)]);
        let now = shown(&mut app);
        assert!(
            (now - value).abs() < 1e-3,
            "the knob must follow the hand while the gesture is open: asked {value}, shows {now}"
        );
        // **And the lock is not written yet.** The plugin's base already carries this value, so a
        // lock written now would have the runtime lay `lock - patch` on top of it — the drag would
        // sound at roughly twice the deviation until release.
        assert_eq!(
            app.app().sequencer_state().locks.get(4, param),
            None,
            "the step takes the value when the hand lets go, not per frame"
        );
    }

    // **Letting go keeps the value.** Three shipped versions returned the parameter to the patch
    // here — correct by the modulation model, and rejected three times as the knob refusing input.
    // The base holds the step's value for as long as the step stays selected; leaving the step is
    // the one moment it returns to the patch.
    app.app().plugin_gesture_ended(param);
    let base = shown(&mut app);
    assert!(
        (base - 0.80).abs() < 1e-3,
        "released, still on the step: the knob keeps what the hand set: {base}"
    );
    assert_eq!(
        app.app().sequencer_state().locks.get(4, param),
        Some(0.8),
        "with the step holding where the drag ended"
    );
}

#[test]
fn a_batched_drag_is_one_hand_not_a_patch_change() {
    // A drag's frames can queue and arrive as one batch. Counting *events* would put that batch in
    // the several-at-once branch — a patch change — and write nothing into the step; what matters is
    // that they are all one parameter, and only where it ended up.
    let Some(mut session) = session("locks-batched-drag") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(3);

    session
        .app()
        .plugin_moved(&[(param, 0.50), (param, 0.65), (param, 0.80)]);

    assert_eq!(
        session.app().sequencer_state().locks.get(3, param),
        Some(0.8),
        "one knob, many frames: the step takes where the drag ended"
    );
    let patch = session
        .app()
        .sequencer_state()
        .locks
        .patch(param)
        .expect("a sequenced parameter has a patch value");
    assert!(
        (patch - 0.30).abs() < 1e-3,
        "and the patch is untouched, not replaced by a misread preset load: {patch}"
    );
}

#[test]
fn dragging_over_an_existing_lock_sounds_like_the_value_under_the_knob() {
    // **The reported case, exactly**: step 5 selected, and it already sets the cutoff. The plugin's
    // base carries the dragged value, and the old lock's offset was still previewed on top — so the
    // drag sounded at roughly `dragged + (old_lock - patch)` rather than `dragged`. The old lock's
    // preview has to come off for the parameter being held, and the release re-creates the lock at
    // where the drag ended.
    let Some(mut session) = session("locks-drag-existing") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);
    let patch_sound = held_note_rms(&mut session, 60);

    // The step already sets it, audibly.
    session.app().select_step(4);
    session.app().set_parameter(param, 0.80);
    session.advance_blocks(4).expect("advance");
    let locked_sound = held_note_rms(&mut session, 60);
    assert!(
        (locked_sound - patch_sound).abs() > patch_sound * 0.05,
        "the existing lock previews, audibly: {patch_sound} vs {locked_sound}"
    );

    // What 0.55 sounds like with no lock in the picture at all - measured, not assumed, so the
    // assertion below is a comparison of sounds rather than of bookkeeping.
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");
    session.app().set_parameter(param, 0.55);
    session.advance_blocks(4).expect("advance");
    let target_sound = held_note_rms(&mut session, 60);
    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(4);
    session.advance_blocks(4).expect("advance");

    // Now a drag in the plugin's own editor moves the base to 0.55.
    session.app().plugin_gesture_began(param);
    session
        .app()
        .engine_mut()
        .push_gui_event(mxm_player::events::input::Payload::ParamValue {
            param_id: param,
            value: 0.55,
        });
    session.app().plugin_moved(&[(param, 0.55)]);
    session.advance_blocks(4).expect("advance");

    let heard = held_note_rms(&mut session, 60);
    assert!(
        (heard - target_sound).abs() < target_sound * 0.05,
        "a drag must sound like the value under the knob, not that plus the old deviation:          want {target_sound}, heard {heard}"
    );

    // Release: the step takes where the drag ended, and the base returns to the patch.
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(4).expect("advance");
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.55),
        "the step holds where the drag ended"
    );
    let after = held_note_rms(&mut session, 60);
    assert!(
        (after - target_sound).abs() < target_sound * 0.05,
        "and what is sounding is unchanged by the handover: want {target_sound}, heard {after}"
    );
}

// **Untestable for now, and said rather than faked**: a drag whose lock is *refused* must still
// return the base to the patch — `restore_base_for_step_edit` deliberately ignores what
// `parameter_edited` answered. The only refusals are an unsequenceable parameter, which mxm-mono-01
// does not declare, and a full lock set, which its 27 parameters cannot produce against a capacity
// of 32. Both gaps close with MXM-303, alongside the A-to-B identity test above.

#[test]
fn deselecting_mid_drag_keeps_the_existing_lock() {
    // **The case the first design lost.** It cleared the canonical lock at drag start and re-created
    // it on release - so a deselect mid-drag, which drops the pending edit, deleted a lock the
    // person never asked to remove. Suppression instead of deletion is what makes this hold: the
    // lock was never touched, only its preview silenced.
    let Some(mut session) = session("locks-deselect-mid-drag") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    session.app().select_step(4);
    session.app().set_parameter(param, 0.80);
    session.advance_blocks(4).expect("advance");

    // A drag begins in the plugin's editor, and the step is deselected before it ends.
    session.app().plugin_gesture_began(param);
    session.app().plugin_moved(&[(param, 0.55)]);
    session.advance_blocks(2).expect("advance");
    session.app().deselect_step();
    session.advance_blocks(2).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(4).expect("advance");

    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.8),
        "an abandoned drag must not take an existing lock with it"
    );
}

#[test]
fn the_users_flow_a_dragged_knob_keeps_its_value_until_the_step_is_left() {
    // **The reported flow, end to end, judged by the sound** — which is what the knob draws from.
    // Stopped, a step selected, drag the cutoff in the plugin's own editor, release. The value must
    // HOLD: what you set is what the step now is, and it stays put — audibly and visibly — until you
    // leave the step. Returning to the patch on release, however well annotated, is what three
    // attempts shipped and what the user rejected three times.
    let Some(mut session) = session("locks-user-flow") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);
    let patch_sound = held_note_rms(&mut session, 60);

    // Reference: what 0.55 sounds like, with nothing else in the picture.
    session.app().set_parameter(param, 0.55);
    session.advance_blocks(4).expect("advance");
    let target_sound = held_note_rms(&mut session, 60);
    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");

    // The flow: select step 5, drag in the plugin's editor, release.
    session.app().select_step(4);
    session.app().plugin_gesture_began(param);
    session
        .app()
        .engine_mut()
        .push_gui_event(mxm_player::events::input::Payload::ParamValue {
            param_id: param,
            value: 0.55,
        });
    session.app().plugin_moved(&[(param, 0.55)]);
    session.advance_blocks(4).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(8).expect("advance");

    // **After release, still on the step: it holds.**
    let after_release = held_note_rms(&mut session, 60);
    assert!(
        (after_release - target_sound).abs() < target_sound * 0.05,
        "released, still on the step: the value must hold, not return to the patch. \
         patch {patch_sound}, target {target_sound}, heard {after_release}"
    );
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.55),
        "and the step holds it"
    );

    // Leaving the step is the moment the instrument returns to the patch — chosen, not sprung.
    session.app().deselect_step();
    session.advance_blocks(8).expect("advance");
    let after_leave = held_note_rms(&mut session, 60);
    assert!(
        (after_leave - patch_sound).abs() < patch_sound * 0.05,
        "leaving the step returns to the patch: {patch_sound} vs {after_leave}"
    );

    // And coming back to the step brings the step's sound back.
    session.app().select_step(4);
    session.advance_blocks(8).expect("advance");
    let back = held_note_rms(&mut session, 60);
    assert!(
        (back - target_sound).abs() < target_sound * 0.05,
        "re-selecting the step sounds the step: {target_sound} vs {back}"
    );
}

#[test]
fn playing_with_a_parked_base_does_not_stack_the_deviation() {
    // A base parked at a step's value under a *running* sequence would have the stepping's offsets
    // land on top of it: the locked step would sound at lock + (lock - patch). Starting playback
    // unparks first.
    let Some(mut session) = session("locks-parked-play") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    for step in 0..16 {
        session.app().toggle_step_note(step, 60);
    }
    session.app().set_parameter(param, 0.60);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    // A reference bar: the step locked LOW the ordinary way, nothing parked. Low on purpose - a
    // stacked deviation from a low lock clamps toward zero and silences the step, so the two ways
    // of authoring sound unmistakably different if play fails to unpark. Locked high, both ways
    // clamp against the top of the range and the oracle cannot tell them apart.
    session.app().select_step(4);
    session.app().set_parameter(param, 0.20);
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");
    session.clear_capture();
    session.app().play_from_start();
    session.advance_blocks(190).expect("advance");
    let reference = left(&session.captured());
    session.app().stop_sequencer();
    session.advance_blocks(8).expect("advance");

    // The same lock re-authored through a plugin-editor drag, leaving the base parked at 0.9 - then
    // play WITHOUT deselecting first, which is exactly when stacking would happen.
    session.app().select_step(4);
    session.app().plugin_gesture_began(param);
    session
        .app()
        .engine_mut()
        .push_gui_event(mxm_player::events::input::Payload::ParamValue {
            param_id: param,
            value: 0.20,
        });
    session.app().plugin_moved(&[(param, 0.20)]);
    session.advance_blocks(4).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(4).expect("advance");

    session.clear_capture();
    session.app().play_from_start();
    session.advance_blocks(190).expect("advance");
    let parked = left(&session.captured());
    session.app().stop_sequencer();

    let window = |samples: &[f32], from: usize, to: usize| {
        let slice = &samples[from.min(samples.len())..to.min(samples.len())];
        (slice
            .iter()
            .map(|s| f64::from(*s) * f64::from(*s))
            .sum::<f64>()
            / slice.len().max(1) as f64)
            .sqrt()
    };
    // Step 5 is frames 24,000..30,000 at 120 BPM.
    let want = window(&reference, 24_000, 30_000);
    let got = window(&parked, 24_000, 30_000);
    assert!(
        (got - want).abs() < want * 0.05,
        "the locked step must sound the same however the lock was authored: {want} vs {got}"
    );
}

#[test]
fn a_lock_is_absolute_and_editing_a_step_never_touches_the_patch() {
    // **Reported as "editing a step edits the sequencer patch too, and then it keeps the locked as
    // a delta value."** The chain: with a step selected, the panel's write-back stamped the step's
    // value into the params snapshot, where it masqueraded as the base; after deselecting it showed
    // as the patch, and one later tweak from that shown value made it the real patch — at which
    // point `lock - patch` was about zero and the step followed every patch move like a delta.
    let Some(mut session) = session("locks-absolute") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.30);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    // Lock step 5 at 0.8 through the panel path — the one whose write-back was the door.
    session.app().select_step(4);
    session.app().set_parameter(param, 0.80);
    session.advance_blocks(4).expect("advance");
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");

    // The patch is still the patch.
    let patch = session
        .app()
        .sequencer_state()
        .locks
        .patch(param)
        .expect("a sequenced parameter has a patch value");
    assert!(
        (patch - 0.30).abs() < 1e-3,
        "editing a step must never move the patch: {patch}"
    );

    // Now genuinely move the patch — nothing selected, which is what editing the patch is.
    session.app().set_parameter(param, 0.50);
    session.advance_blocks(4).expect("advance");

    // What 0.8 sounds like, for the comparison.
    session.app().select_step(4);
    session.advance_blocks(8).expect("advance");
    let on_step = held_note_rms(&mut session, 60);
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");
    session.app().set_parameter(param, 0.80);
    session.advance_blocks(4).expect("advance");
    let target = held_note_rms(&mut session, 60);
    session.app().set_parameter(param, 0.50);
    session.advance_blocks(4).expect("advance");

    // **Absolute**: the step sounds 0.8 whatever the patch became.
    assert!(
        (on_step - target).abs() < target * 0.05,
        "a lock is an absolute value; moving the patch afterwards must not move the step: \
         locked sound {on_step}, 0.8 sounds like {target}"
    );
    assert_eq!(
        session.app().sequencer_state().locks.get(4, param),
        Some(0.8),
        "and the stored lock is still 0.8"
    );
}

#[test]
fn dragging_the_panels_slider_into_a_step_never_touches_the_patch() {
    // **The corrupting path is the panel's own draw loop**, which a `Session` never runs: its
    // write-back stamped every edit into the params snapshot, including step edits whose value was
    // never sent — so the step's value sat where the base belongs, showed as the patch after
    // deselecting, and the next frame's follow could absorb it. Only a real panel drag reaches it.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let mut app = AppHarness::new("locks-panel-drag", vec![bundled.clone()]);
    app.run();
    app.harness.state_mut().rescan();
    app.run();
    app.harness
        .state_mut()
        .load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    app.run();

    let param = cutoff_id(&mut app);
    app.app().set_parameter(param, 0.30);
    for _ in 0..4 {
        app.run();
    }
    app.app().select_step(4);
    app.run();

    // Drag the real Cutoff slider in the panel, far right.
    let rect = app.harness.get_by_label("Cutoff").rect();
    let from = rect.center();
    let to = egui::pos2(rect.right() - 2.0, rect.center().y);
    app.harness.hover_at(from);
    app.harness.drag_at(from);
    app.run();
    app.harness.hover_at(to);
    app.run();
    app.harness.drop_at(to);
    app.run();

    let locked = app
        .app()
        .sequencer_state()
        .locks
        .get(4, param)
        .expect("the drag wrote a lock into the selected step");
    assert!(locked > 0.5, "and it went where the drag went: {locked}");

    // **Immediately after deselecting, before any requery can correct it**: the snapshot must be
    // showing the patch, not the step's value. The write-back stamped the step value in here, and
    // although a requery corrected it a frame later, that frame is exactly when `deselect` reads it
    // and when a person sees the "patch" sitting at the step value.
    app.app().deselect_step();
    let shown = app
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|p| p.id == param)
        .expect("the parameter")
        .value;
    assert!(
        (shown - 0.30).abs() < 1e-3,
        "right after deselecting, the panel shows the patch, not the step: {shown}"
    );
    for _ in 0..6 {
        app.run();
    }

    let patch = app
        .app()
        .sequencer_state()
        .locks
        .patch(param)
        .expect("a sequenced parameter has a patch value");
    assert!(
        (patch - 0.30).abs() < 1e-3,
        "dragging the panel's slider into a step must never move the patch: {patch}"
    );
}

#[test]
fn editing_a_step_under_a_running_transport_leaves_the_other_steps_alone() {
    // **The live-tweaking flow**: the sequence is playing, a step is selected, and a knob in the
    // plugin's own editor is dragged. The lock must land in that step — and every *other* step must
    // go on sounding what it always did. The failing build parked the base at the dragged value and
    // suppressed the parameter's offsets, so the whole sequence sounded the selected step's cutoff:
    // "it still changes it for all the other steps too".
    let Some(mut session) = session("locks-live-edit") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    for step in 0..16 {
        session.app().toggle_step_note(step, 60);
    }
    session.app().set_parameter(param, 0.60);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    session.app().play_from_start();
    session.advance_blocks(8).expect("advance");

    // Select a step mid-playback and drag cutoff in the instrument's editor, exactly as a person
    // does: gesture opens, the plugin's own parameter moves, gesture closes. The step stays
    // selected and the transport keeps running - nobody deselects between tweaks of a live take.
    session.app().select_step(4);
    session.app().plugin_gesture_began(param);
    session
        .app()
        .engine_mut()
        .push_gui_event(mxm_player::events::input::Payload::ParamValue {
            param_id: param,
            value: 0.20,
        });
    session.app().plugin_moved(&[(param, 0.20)]);
    session.advance_blocks(4).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(2).expect("advance");

    // The edit itself must have landed in the step.
    assert_eq!(
        locked_at(&mut session, param, 4),
        Some(0.20),
        "the drag wrote the step's lock"
    );

    // Ride the playhead onto a step far from the edited one, then ask the plugin itself what it is
    // sounding. Fresh, uncorrected, modulated - the same reading the CLI's dump exposes, because
    // this divergence is invisible to every corrected view.
    let mut on_far_step = false;
    for _ in 0..300 {
        session.advance_blocks(1).expect("advance");
        let (_, step) = session.app().playhead();
        if (8..=14).contains(&step) {
            on_far_step = true;
            break;
        }
    }
    assert!(on_far_step, "the playhead reached the far half of the bar");
    session.advance_blocks(1).expect("advance");

    let dump: serde_json::Value =
        serde_json::from_str(&session.app().run_cli_command("dump")).expect("the dump is JSON");
    let sounding = dump["raw_readback"][param.to_string()]
        .as_f64()
        .expect("cutoff is in the readback");
    assert!(
        (sounding - 0.60).abs() < 0.05,
        "a step that sets nothing must sound the patch (0.60) while the edited step stays \
         selected, not the edited value: the plugin reads {sounding}"
    );
}

#[test]
fn switching_steps_returns_a_parked_base_to_the_patch() {
    // At rest, a base parks at the dragged value while its step stays selected. Clicking a
    // *different* step is leaving that step just as surely as deselecting is - the parked value
    // belongs to the step being left, and carrying it into the next one has the knob (and the
    // preview) sounding step 5's cutoff while step 10 is open.
    let Some(mut session) = session("locks-park-switch") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(4, 60);
    session.app().set_parameter(param, 0.60);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    // The rest-time editor drag: park is expected while step 5 stays selected.
    session.app().select_step(4);
    session.app().plugin_gesture_began(param);
    session
        .app()
        .engine_mut()
        .push_gui_event(mxm_player::events::input::Payload::ParamValue {
            param_id: param,
            value: 0.20,
        });
    session.app().plugin_moved(&[(param, 0.20)]);
    session.advance_blocks(4).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(2).expect("advance");

    session.app().select_step(9);
    session.advance_blocks(4).expect("advance");

    let dump: serde_json::Value =
        serde_json::from_str(&session.app().run_cli_command("dump")).expect("the dump is JSON");
    let sounding = dump["raw_readback"][param.to_string()]
        .as_f64()
        .expect("cutoff is in the readback");
    assert!(
        (sounding - 0.60).abs() < 0.05,
        "with step 10 open, the instrument must be back at the patch (0.60), \
         not the value step 5 parked: the plugin reads {sounding}"
    );
    assert_eq!(
        locked_at(&mut session, param, 4),
        Some(0.20),
        "and step 5's lock survives the switch"
    );
}

#[test]
fn loading_a_sequence_settles_the_instrument_on_its_patch() {
    // The fossil: a base stranded away from the patch survives persistence - the divergence is
    // reloaded every launch, the corrected views all show the patch, and the instrument sounds
    // something else until the parameter happens to be touched. Watched live: a plugin restored at
    // cutoff 0.946 under a lock record whose patch said 0.890, sounding a step's lock on every
    // step. When locks and their baselines load, the instrument is put where the record says.
    let Some(mut session) = session("locks-settle-load") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    // A sequence whose lock record carries patch 0.60.
    session.app().toggle_step_note(0, 60);
    session.app().set_parameter(param, 0.60);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);
    session.app().select_step(4);
    session.app().set_parameter(param, 0.20);
    session.app().deselect_step();
    session.advance_blocks(4).expect("advance");
    let saved = session.app().save_sequence("settle-proof").expect("saved");

    // Strand the instrument somewhere else, with no locks to correct for.
    session.app().clear_pattern();
    session.app().set_parameter(param, 0.90);
    session.advance_blocks(4).expect("advance");

    session.app().load_sequence(&saved).expect("loaded");
    session.advance_blocks(4).expect("advance");

    let dump: serde_json::Value =
        serde_json::from_str(&session.app().run_cli_command("dump")).expect("the dump is JSON");
    let sounding = dump["raw_readback"][param.to_string()]
        .as_f64()
        .expect("cutoff is in the readback");
    assert!(
        (sounding - 0.60).abs() < 0.05,
        "the loaded record says the patch is 0.60, so that is what the instrument must sound - \
         not where it happened to be standing: the plugin reads {sounding}"
    );
}

// --- bars: the interface's model, driven as the interface drives it -----------------------------

#[test]
fn a_copied_bar_pastes_with_its_notes_ties_and_locks() {
    let Some(mut session) = session("bars-copy-paste") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_bars(3);
    session.app().set_parameter(param, 0.60);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    // Author bar two: a note, a tie extending it, and a lock on its head.
    session.app().toggle_step_note(16, 60);
    session.app().toggle_step_tie(17);
    session.app().select_step(16);
    session.app().set_parameter(param, 0.25);
    session.app().deselect_step();
    session.advance_blocks(2).expect("advance");

    session.app().select_bar(1);
    session.app().copy_bar();
    session.app().select_bar(2);
    session.app().paste_clipboard();

    let pattern = session.app().sequencer_state().pattern.clone();
    assert!(pattern.step(32).contains(60), "the note pasted");
    assert!(pattern.tied(33), "the tie pasted");
    assert_eq!(
        locked_at(&mut session, param, 32),
        Some(0.25),
        "the lock pasted, at the same offset in the destination bar"
    );
    // The source bar is untouched.
    assert!(pattern.step(16).contains(60), "the source keeps its note");
    assert_eq!(locked_at(&mut session, param, 16), Some(0.25));
}

#[test]
fn clearing_a_bar_leaves_the_bar_that_continued_it_untouched() {
    // A bar clear empties the bar it names and nothing else. The tie in the bar after it stays:
    // it continues nothing now, which is silence rather than damage, and the moment a note lands
    // in front of it the run is audible again — with the tie the author wrote still there.
    let Some(mut session) = session("bars-clear-orphan") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_bars(3);
    // A run headed at the end of bar two, tied into bar three.
    session.app().toggle_step_note(31, 60);
    session.app().toggle_step_tie(32);
    assert!(session.app().sequencer_state().pattern.tied(32));

    session.app().select_bar(1);
    session.app().clear_bar();

    let pattern = session.app().sequencer_state().pattern.clone();
    assert!(pattern.step(31).is_empty(), "the head went with its bar");
    assert!(
        pattern.tied(32),
        "and bar three keeps the tie it was authored with — the clear named one bar"
    );
}

#[test]
fn loop_bar_keeps_the_playhead_inside_the_selected_bar() {
    let Some(mut session) = session("bars-loop-scope") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_bars(2);
    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(16, 72);
    session.app().select_bar(1);
    session.app().set_loop_scope(mxm_player::ui::LoopScope::Bar);
    session.app().play_from_start();

    // Two bars' worth of time; the playhead must never leave bar two.
    //
    // **This test was flaky, and the flake was in the harness, not here.** Under a heavily loaded
    // full-suite run it read step 0 with the transport stopped: `PlayerApp::publish_sequencer`
    // keeps at most one publication outstanding, so when the audio thread had not been scheduled
    // since the load's publication, the single `service` before the first block published nothing
    // and the block rendered with the transport, the loop window and the pattern all still unset.
    // `Session::advance_blocks` now services until the edits have reached the worker's queue.
    //
    // The mechanism is not theoretical: instrumented, a single full-suite run had that wait engage
    // ten times, and each of those would have been a block rendered from a stale state.
    //
    // The message still names the block and the transport, which is what pointed at the transport
    // being stopped rather than at the bar being left.
    for block in 0..380 {
        session.advance_blocks(1).expect("advance");
        let (transport, step) = session.app().playhead();
        assert!(
            (16..32).contains(&step),
            "loop-bar playhead left the selected bar at block {block}: step {step}, transport {transport:?}"
        );
    }
}

#[test]
fn bars_materialise_on_content_and_never_on_viewing() {
    let Some(mut session) = session("bars-materialise") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    assert_eq!(session.app().sequencer_state().pattern.bars(), 1);

    // Wander to pattern twelve and look around: nothing grows.
    session.app().select_bar(89);
    assert_eq!(
        session.app().sequencer_state().pattern.bars(),
        1,
        "viewing is free"
    );

    // The first note into bar ninety is what makes the sequence ninety bars.
    session.app().toggle_step_note(89 * 16, 60);
    assert_eq!(
        session.app().sequencer_state().pattern.bars(),
        90,
        "content is what adds bars"
    );
    assert!(
        session
            .app()
            .sequencer_state()
            .pattern
            .step(89 * 16)
            .contains(60)
    );
}

#[test]
fn random_fills_the_shown_bar_and_grows_the_sequence_to_reach_it() {
    // **Reported as "it randomizes bar 1".** Random replaced the whole pattern, so selecting bar
    // two and pressing it threw the sequence away and wrote one bar back. It fills the bar being
    // shown, and a bar past the end materialises exactly as a note into it would.
    let Some(mut session) = session("bars-random") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().randomise();
    let bar_one: Vec<_> = (0..16)
        .map(|step| session.app().sequencer_state().pattern.step(step))
        .collect();
    assert!(
        bar_one.iter().any(|step| !step.is_empty()),
        "bar one should hold notes"
    );

    session.app().select_bar(1);
    session.app().randomise();

    let pattern = session.app().sequencer_state().pattern.clone();
    assert_eq!(
        pattern.bars(),
        2,
        "content in bar two makes the sequence two bars"
    );
    assert!(
        (0..16).all(|step| pattern.step(step) == bar_one[step]),
        "bar one must be untouched"
    );
    assert!(
        (16..32).any(|step| !pattern.step(step).is_empty()),
        "bar two is what was randomised"
    );
}

#[test]
fn random_fills_a_bar_of_the_shape_the_sequence_has() {
    // Sixteen steps written into a bar of twelve would run over into the next bar, or re-bar the
    // music by replacing the pattern -- the second is what the whole-pattern version did.
    let Some(mut session) = session("bars-random-shape") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().set_steps_per_bar(12);
    session.app().select_bar(2);
    session.app().randomise();

    let pattern = session.app().sequencer_state().pattern.clone();
    assert_eq!(pattern.steps_per_bar(), 12, "the shape must survive");
    assert_eq!(pattern.bars(), 3);
    assert!(
        (0..24).all(|step| pattern.step(step).is_empty()),
        "bars one and two were not asked for"
    );
    assert!(
        (24..36).any(|step| !pattern.step(step).is_empty()),
        "bar three is what was randomised"
    );
}

#[test]
fn a_copied_pattern_pastes_as_eight_bars_and_grows_to_its_content() {
    let Some(mut session) = session("bars-pattern-paste") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // Two bars of content in pattern one.
    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(16, 72);
    session.app().copy_pattern();

    // Paste at pattern two: the sequence grows to reach the pasted content - bar ten, because
    // the clipboard's content ends in its second bar - and no further.
    session.app().select_bar(8);
    session.app().paste_clipboard();

    let pattern = session.app().sequencer_state().pattern.clone();
    assert_eq!(
        pattern.bars(),
        10,
        "grown to the last pasted bar with content"
    );
    assert!(
        pattern.step(8 * 16).contains(60),
        "bar nine got bar one's note"
    );
    assert!(
        pattern.step(9 * 16).contains(72),
        "bar ten got bar two's note"
    );
}

#[test]
fn loop_pattern_keeps_the_playhead_inside_the_eight_bars() {
    let Some(mut session) = session("bars-loop-pattern") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // Ten bars of content, looping pattern one: the playhead never reaches bar nine.
    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(9 * 16, 72);
    session.app().select_bar(2);
    session
        .app()
        .set_loop_scope(mxm_player::ui::LoopScope::Pattern);
    session.app().play_from_start();

    let mut lowest = usize::MAX;
    let mut highest = 0;
    for _ in 0..380 {
        session.advance_blocks(1).expect("advance");
        let (_, step) = session.app().playhead();
        assert!(
            step < 8 * 16,
            "loop-pattern playhead left the eight bars: step {step}"
        );
        lowest = lowest.min(step);
        highest = highest.max(step);
    }
    // And it genuinely spans the pattern - not one bar mislabelled as eight: it started at the
    // pattern's first bar and crossed into the second within two bars of playing time.
    assert!(
        lowest < 16 && highest >= 16,
        "the window is the whole pattern: saw steps {lowest}..={highest}"
    );
}

// --- multi-step selection -----------------------------------------------------------------------

#[test]
fn a_shift_range_takes_the_note_on_every_selected_step() {
    let Some(mut session) = session("sel-shift-note") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().select_step(0);
    session.app().shift_select_step(3);
    session.app().note_on(60, 0.8);
    session.app().note_off(60);

    let pattern = session.app().sequencer_state().pattern.clone();
    for step in 0..4 {
        assert!(
            pattern.step(step).contains(60),
            "step {} took the note",
            step + 1
        );
    }
    assert!(pattern.step(4).is_empty(), "the range ended where it ended");
    assert_eq!(
        session.app().selected_step(),
        Some(0),
        "the anchor - the step that previews - is unchanged"
    );
}

#[test]
fn a_scattered_selection_locks_every_fourth_step_in_one_gesture() {
    let Some(mut session) = session("sel-ctrl-lock") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.60);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    session.app().select_step(0);
    session.app().ctrl_select_step(4);
    session.app().ctrl_select_step(8);
    session.app().ctrl_select_step(12);
    session.app().set_parameter(param, 0.25);

    for step in [0usize, 4, 8, 12] {
        assert_eq!(
            locked_at(&mut session, param, step),
            Some(0.25),
            "step {} took the lock",
            step + 1
        );
    }
    assert_eq!(
        locked_at(&mut session, param, 2),
        None,
        "an unselected step took nothing"
    );
}

#[test]
fn pasting_steps_replaces_what_is_under_them() {
    // The owner's rule: like pasting over a selected word. The target's notes, tie and lock all
    // go; what lands is exactly the clipboard's cells.
    let Some(mut session) = session("sel-paste-replace") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);
    session.app().set_parameter(param, 0.60);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    // The source: two steps, a note with a lock, then a tie extending it.
    session.app().toggle_step_note(0, 60);
    session.app().select_step(0);
    session.app().set_parameter(param, 0.25);
    session.app().deselect_step();
    session.app().toggle_step_tie(1);

    // The target: different note, its own lock, a lock on a parameter the clipboard does not
    // carry at all, and a tie hanging off it.
    let resonance = session
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|p| p.name == "Resonance")
        .expect("mxm-mono-01 has a resonance")
        .id;
    session.app().toggle_step_note(8, 72);
    session.app().select_step(8);
    session.app().set_parameter(param, 0.90);
    session.app().set_parameter(resonance, 0.70);
    session.app().deselect_step();
    session.app().toggle_step_tie(9);

    session.app().select_step(0);
    session.app().shift_select_step(1);
    session.app().copy_steps();
    session.app().select_step(8);
    session.app().paste_clipboard();

    let pattern = session.app().sequencer_state().pattern.clone();
    assert!(pattern.step(8).contains(60), "the pasted note replaced 72");
    assert!(!pattern.step(8).contains(72), "the old note is gone");
    assert_eq!(
        locked_at(&mut session, param, 8),
        Some(0.25),
        "the pasted lock replaced 0.90"
    );
    assert_eq!(
        locked_at(&mut session, resonance, 8),
        None,
        "a lock the clipboard does not carry is gone - paste replaces, never merges"
    );
    assert!(pattern.tied(9), "the pasted tie landed");
}

#[test]
fn pasting_past_the_bar_wraps_on_and_grows_the_sequence() {
    let Some(mut session) = session("sel-paste-wrap") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(3, 72);
    session.app().select_step(0);
    session.app().shift_select_step(3);
    session.app().copy_steps();

    // Paste four steps starting at step fifteen of a one-bar sequence.
    session.app().select_step(14);
    session.app().paste_clipboard();

    let pattern = session.app().sequencer_state().pattern.clone();
    assert_eq!(
        pattern.bars(),
        2,
        "the paste reached into bar two, so it exists"
    );
    assert!(
        pattern.step(14).contains(60),
        "the run starts at the anchor"
    );
    assert!(pattern.step(17).contains(72), "and wraps into the next bar");
}

#[test]
fn cutting_steps_copies_then_empties_them() {
    let Some(mut session) = session("sel-cut") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    session.app().toggle_step_note(0, 60);
    session.app().toggle_step_note(1, 62);
    session.app().select_step(0);
    session.app().shift_select_step(1);
    session.app().cut_steps();

    let pattern = session.app().sequencer_state().pattern.clone();
    assert!(
        pattern.step(0).is_empty() && pattern.step(1).is_empty(),
        "cut empties"
    );

    session.app().select_step(4);
    session.app().paste_clipboard();
    let pattern = session.app().sequencer_state().pattern.clone();
    assert!(
        pattern.step(4).contains(60) && pattern.step(5).contains(62),
        "and what was cut pastes back"
    );
}

// --- the editor's double-click clears the lock --------------------------------------------------

#[test]
fn an_instantaneous_editor_edit_on_the_patch_value_clears_the_lock() {
    // "I double click on everything but the parameter lock stays on." The editor cannot say
    // "clear" - CLAP carries only values - so its double-click arrives as an instantaneous jump
    // onto the patch value, and the player reads that as what the contract says a reset means
    // with a step selected: the step sets nothing.
    let Some(mut session) = session("reset-clears") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.60);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    session.app().select_step(2);
    session.app().set_parameter(param, 0.25);
    assert_eq!(
        locked_at(&mut session, param, 2),
        Some(0.25),
        "the premise: locked"
    );

    // The double-click, as the player receives it: gesture opened and closed within the batch,
    // one value, exactly the patch.
    session.app().plugin_moved(&[(param, 0.60)]);

    assert_eq!(
        locked_at(&mut session, param, 2),
        None,
        "an instantaneous edit landing on the patch clears the lock"
    );
}

#[test]
fn a_reset_on_a_parked_knob_clears_the_lock_and_repairs_the_base() {
    // The editor's blind spot: while a base is parked the parameter reports no modulation, so
    // its double-click aims at the factory default instead of the patch. Arriving on a parked
    // parameter, that default is a reset - and the base, sitting at the default the plugin just
    // took, is put back to the patch.
    let Some(mut session) = session("reset-parked") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.60);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    // An editor drag writes the lock and parks the base at it.
    session.app().select_step(2);
    session.app().plugin_gesture_began(param);
    session
        .app()
        .engine_mut()
        .push_gui_event(mxm_player::events::input::Payload::ParamValue {
            param_id: param,
            value: 0.25,
        });
    session.app().plugin_moved(&[(param, 0.25)]);
    session.advance_blocks(2).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(2).expect("advance");
    assert_eq!(
        locked_at(&mut session, param, 2),
        Some(0.25),
        "the premise: locked"
    );

    // The double-click on the parked knob: an instantaneous jump to the factory default.
    let default = session
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|p| p.id == param)
        .expect("cutoff")
        .default;
    session.app().plugin_moved(&[(param, default)]);
    session.advance_blocks(4).expect("advance");

    assert_eq!(
        locked_at(&mut session, param, 2),
        None,
        "the reset on a parked knob clears the lock"
    );
    let dump: serde_json::Value =
        serde_json::from_str(&session.app().run_cli_command("dump")).expect("dump is JSON");
    let raw = dump["raw_readback"][param.to_string()]
        .as_f64()
        .expect("cutoff in readback");
    assert!(
        (raw - 0.60).abs() < 0.05,
        "and the base is back at the patch, not the default: {raw}"
    );
}

#[test]
fn a_bracketed_reset_on_a_parked_knob_clears_the_lock_too() {
    // **Reported from mxm-poly-06's Noise knob**: a step locked it, the knob was double-clicked
    // in the instrument's editor, the knob's dot went — and the step's dot stayed. The editor
    // brackets its double-click in a gesture, begin/value/end, and the batch loop routes a value
    // arriving inside an open gesture to the pending edit, so the reset reached the gesture-close
    // commit — which read only the *patch* value as a reset. On a parked knob the editor's reset
    // aims at the factory default, so the default was written back as the lock: base parked at
    // the default, no modulation, no dot on the knob, and the step still locked. The one-batch
    // path (`a_reset_on_a_parked_knob_clears_the_lock_and_repairs_the_base`) already accepted the
    // parked default; both timings must agree, or the gesture works by luck.
    let Some(mut session) = session("reset-parked-bracketed") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    use mxm_player::engine::PluginOutput;
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.60); // the patch, away from the factory default
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);
    let default = session
        .state()
        .plugin
        .expect("a plugin is loaded")
        .params
        .iter()
        .find(|p| p.id == param)
        .expect("cutoff")
        .default;
    assert!(
        (default - 0.60).abs() > 0.05,
        "the premise: the patch is not the default ({default})"
    );

    // An editor drag writes the lock and parks the base at it.
    let lock_from_the_editor = |session: &mut Session| {
        session.app().plugin_gesture_began(param);
        session
            .app()
            .engine_mut()
            .push_gui_event(mxm_player::events::input::Payload::ParamValue {
                param_id: param,
                value: 0.25,
            });
        session.app().plugin_moved(&[(param, 0.25)]);
        session.advance_blocks(2).expect("advance");
        session.app().plugin_gesture_ended(param);
        session.advance_blocks(2).expect("advance");
    };
    session.app().select_step(2);
    lock_from_the_editor(&mut session);
    assert_eq!(
        locked_at(&mut session, param, 2),
        Some(0.25),
        "the premise: locked, and parked"
    );

    // The double-click exactly as the editor emits it: begin, the default, end, in one batch.
    session.app().receive_plugin_outputs(vec![
        PluginOutput::GestureBegin { param_id: param },
        PluginOutput::ParamValue {
            param_id: param,
            value: default,
        },
        PluginOutput::GestureEnd { param_id: param },
    ]);
    session.advance_blocks(4).expect("advance");
    assert_eq!(
        locked_at(&mut session, param, 2),
        None,
        "a bracketed reset on a parked knob clears the lock: the step's dot goes with the knob's"
    );
    let dump: serde_json::Value =
        serde_json::from_str(&session.app().run_cli_command("dump")).expect("dump is JSON");
    assert!(
        dump["parked_bases"]
            .as_array()
            .is_some_and(|parked| parked.is_empty()),
        "nothing stays parked after the reset: {}",
        dump["parked_bases"]
    );
    let raw = dump["raw_readback"][param.to_string()]
        .as_f64()
        .expect("cutoff in readback");
    assert!(
        (raw - 0.60).abs() < 0.05,
        "and the base is back at the patch, not the default: {raw}"
    );

    // The other timing a real editor produces: begin and value in one turn, end in the next.
    lock_from_the_editor(&mut session);
    assert_eq!(
        locked_at(&mut session, param, 2),
        Some(0.25),
        "re-locked and parked"
    );
    session.app().plugin_gesture_began(param);
    session.app().plugin_moved(&[(param, default)]);
    session.advance_blocks(2).expect("advance");
    session.app().plugin_gesture_ended(param);
    session.advance_blocks(4).expect("advance");
    assert_eq!(
        locked_at(&mut session, param, 2),
        None,
        "split across turns, the reset clears too"
    );
}

#[test]
fn a_gestured_drag_ending_on_the_patch_value_still_locks() {
    // The no-collapse rule is untouched: the reset reading applies only to instantaneous edits.
    // A drag - gesture open across batches - ending exactly on the patch value writes the lock,
    // absolute, as the owner ruled long ago.
    let Some(mut session) = session("reset-no-collapse") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let param = a_param(&mut session);

    session.app().set_parameter(param, 0.60);
    session.advance_blocks(4).expect("advance");
    sharpen(&mut session);

    session.app().select_step(2);
    session.app().plugin_gesture_began(param);
    session.app().plugin_moved(&[(param, 0.25)]);
    session.advance_blocks(2).expect("advance");
    session.app().plugin_moved(&[(param, 0.60)]); // the hand comes back to the patch value
    session.advance_blocks(2).expect("advance");
    session.app().plugin_gesture_ended(param);

    assert_eq!(
        locked_at(&mut session, param, 2),
        Some(0.60),
        "a drag ending on the patch value is a lock at that value, never a collapse"
    );
}

#[test]
fn clicking_a_loop_toggle_selects_it_visibly() {
    // "When I select either of these they don't show as selected." The toggle must both
    // change the scope and paint as selected on the next frame - the scope moving while the
    // highlight stays put is exactly the reported bug. The labels are the spoken-out ones
    // ("1 Bar", "Bar 1-8", "All bars"), which also pins that the loop row no longer collides
    // with the Bar|Pattern tools toggle's words.
    let mut app = AppHarness::new("loop-toggle-select", Vec::new());
    app.run();

    use egui_kittest::kittest::{NodeT, by};
    use mxm_player::ui::LoopScope;
    for (label, scope) in [
        ("1 Bar", LoopScope::Bar),
        ("Bar 1-8", LoopScope::Pattern),
        ("All bars", LoopScope::All),
    ] {
        let pos = app.harness.get(by().label(label)).rect().center();
        app.click_at(pos);
        app.run();
        assert_eq!(
            app.app().loop_scope(),
            scope,
            "{label}: the scope follows the click"
        );
        let node = app.harness.get(by().label(label));
        assert_eq!(
            format!("{:?}", node.accesskit_node().toggled()),
            "Some(True)",
            "{label}: the clicked toggle shows as selected"
        );
    }
}

/// Reads an exported WAV back through the collection's decoder.
fn decode(path: &std::path::Path) -> mxm_audio_file_decode::Decoded {
    mxm_audio_file_decode::decode_file(
        path,
        &mxm_audio_file_decode::Limits::new(
            usize::MAX,
            mxm_audio_file_decode::AtLimit::Refuse,
            mxm_audio_file_decode::Keep::AllUpTo(8),
        ),
    )
    .expect("a valid WAV")
}
