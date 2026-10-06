//! T3 — deterministic sessions through the real application path.
//!
//! Two acceptance criteria, both from the plan:
//!
//! 1. The same session run twice produces **byte-identical audio**.
//! 2. A session that **loads a plugin while streaming completes promptly** — the case a naive
//!    stepped backend deadlocks on, and which must be proven without waiting out the wedge
//!    timeout.

use mxm_player_harness::app_harness;

use mxm_player::session::Session;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const PLUGIN: &str = "dk.mxm.mxm-mono-01";

fn bundle() -> Option<(PathBuf, PathBuf)> {
    let dir = app_harness::bundled_dir()?;
    let file = dir.join("mxm-mono-01.clap");
    Some((dir, file))
}

/// A short scripted performance: a note, a release, and enough blocks to hear the release finish.
fn play_a_note(session: &mut Session) -> Result<(), String> {
    session.advance_blocks(2)?;
    session.app().note_on(60, 100.0 / 127.0);
    session.advance_blocks(8)?;
    session.app().note_off(60);
    session.advance_blocks(24)?;
    Ok(())
}

#[test]
fn the_same_session_twice_renders_identical_audio() {
    let Some((dir, file)) = bundle() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let render = |name: &str| -> Vec<f32> {
        let mut session = Session::scratch(name, vec![dir.clone()]);
        session.load(&file, PLUGIN);
        play_a_note(&mut session).expect("the session advances");
        session.captured()
    };

    let first = render("determinism-a");
    let second = render("determinism-b");

    assert!(!first.is_empty(), "the session rendered nothing");
    assert_eq!(
        first.len(),
        second.len(),
        "the same script must render the same number of samples"
    );
    assert!(
        first == second,
        "the same session must render byte-identical audio; first divergence at sample {:?}",
        first.iter().zip(second.iter()).position(|(a, b)| a != b)
    );

    // ...and it is a real performance, not silence that trivially matches itself.
    assert!(
        first.iter().any(|s| s.abs() > 1e-4),
        "the render should contain audible signal"
    );
}

#[test]
fn loading_a_plugin_while_streaming_completes_promptly() {
    // The deadlock the architecture exists to avoid. `Engine::load` calls `stop_now`, which polls
    // synchronously until a *callback* consumes `Command::Stop`. If the stepped backend only
    // rendered on request, the driver would block waiting for a callback only it could issue —
    // hanging for the wedge timeout and then blaming the plugin.
    let Some((dir, file)) = bundle() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut session = Session::scratch("load-while-streaming", vec![dir]);
    session.load(&file, PLUGIN);
    session.advance_blocks(4).expect("the session advances");
    assert_eq!(session.state().engine, "Running");

    // Load again with the stream already running: this is the re-entrant path.
    let started = Instant::now();
    session.load(&file, PLUGIN);
    let elapsed = started.elapsed();

    // Well under `WEDGE_TIMEOUT` (3s). Passing by waiting out the timeout would be a false pass,
    // so the budget is what makes this assertion mean anything.
    assert!(
        elapsed < Duration::from_millis(1500),
        "loading while streaming took {elapsed:?}; command servicing has regressed"
    );
    assert_ne!(
        session.state().engine,
        "Wedged",
        "a reload must never be mistaken for a hung plugin"
    );

    session.advance_blocks(4).expect("it still renders after");
    assert_eq!(session.state().engine, "Running");
}

#[test]
fn rescanning_while_streaming_completes_promptly() {
    // The other caller of `stop_now`, for the same reason.
    let Some((dir, file)) = bundle() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut session = Session::scratch("rescan-while-streaming", vec![dir]);
    session.load(&file, PLUGIN);
    session.advance_blocks(4).expect("the session advances");

    let started = Instant::now();
    session.app().rescan();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(1500),
        "rescanning while streaming took {elapsed:?}"
    );
    assert!(session.state().lists_plugin(PLUGIN));
}

#[test]
fn audio_advances_only_when_the_session_asks() {
    // What makes the audio reproducible: no free-running thread deciding how much was rendered.
    let Some((dir, file)) = bundle() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut session = Session::scratch("advance", vec![dir]);
    session.load(&file, PLUGIN);

    assert_eq!(
        session.blocks_rendered(),
        0,
        "nothing granted, nothing rendered"
    );

    // However long the process is left alone, an unadvanced session renders nothing more.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(session.blocks_rendered(), 0);

    session.advance_blocks(5).expect("the session advances");
    assert_eq!(session.blocks_rendered(), 5);

    let samples = session.captured().len();
    let expected = 5 * mxm_player::session::FRAMES_PER_BLOCK * 2;
    assert_eq!(samples, expected, "five stereo blocks of audio");
}

#[test]
fn the_virtual_clock_moves_with_the_audio_and_not_with_the_wall() {
    let Some((dir, file)) = bundle() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut session = Session::scratch("clock", vec![dir]);
    session.load(&file, PLUGIN);

    let before = session.clock().now_nanos();
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        session.clock().now_nanos(),
        before,
        "wall-clock time must not move a virtual clock"
    );

    session.advance_blocks(10).expect("the session advances");
    let after = session.clock().now_nanos();

    // Ten blocks of 512 frames at 48 kHz.
    let expected = (10.0 * 512.0 / 48_000.0 * 1e9) as u64;
    let drift = after.abs_diff(before + expected);
    assert!(
        drift < 1_000_000,
        "the clock should have advanced by the audio rendered: expected {expected}, got {}",
        after - before
    );
}

#[test]
fn a_session_writes_the_audio_and_the_state_it_produced() {
    // The two artifacts an investigation wants: what came out, and what the player thought was
    // going on when it did.
    let Some((dir, file)) = bundle() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut session = Session::scratch("artifacts", vec![dir]);
    session.load(&file, PLUGIN);
    play_a_note(&mut session).expect("the session advances");

    let (wav, json) = session.write_artifacts("note").expect("artifacts written");
    assert!(wav.exists() && json.exists());

    let dump = std::fs::read_to_string(&json).expect("readable");
    assert!(dump.contains(PLUGIN), "the state dump names the plugin");
    assert!(dump.contains("\"engine\""));
}

#[test]
fn timing_figures_are_excluded_from_the_determinism_contract() {
    // Meters divide wall-clock durations, so they can never repeat. Comparing whole states would
    // make the determinism claim noise; comparing without timing keeps it a signal.
    let Some((dir, file)) = bundle() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let run = |name: &str| {
        let mut session = Session::scratch(name, vec![dir.clone()]);
        session.load(&file, PLUGIN);
        play_a_note(&mut session).expect("the session advances");
        session.state()
    };

    let a = run("timing-a");
    let b = run("timing-b");

    assert_eq!(
        a.without_timing(),
        b.without_timing(),
        "everything but the timing figures must repeat exactly"
    );
}

#[test]
fn a_rescan_does_not_leave_a_loaded_plugin_silent() {
    // Found live: `rescan` stops the engine while the plugin list changes underneath it, and
    // nothing restarted it - the player sat at engine Idle, device none, and play produced
    // nothing until the plugin was loaded again. A rescan with an instrument loaded must hand
    // the engine back.
    let Some((dir, file)) = bundle() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };
    let mut session = Session::scratch("rescan-restarts", vec![dir]);
    session.load(&file, PLUGIN);
    session.advance_blocks(4).expect("the session advances");

    assert_eq!(session.state().engine, "Running", "the premise: audio runs");
    session.app().rescan();
    for _ in 0..8 {
        session.advance_blocks(1).ok();
    }
    assert_eq!(
        session.state().engine,
        "Running",
        "a rescan must not leave the loaded instrument silent"
    );
}
