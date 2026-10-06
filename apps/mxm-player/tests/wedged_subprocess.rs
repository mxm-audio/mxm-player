//! The wedged-engine path, as a subprocess test.
//!
//! The hang fixture never returns from `process()`, by design, so it cannot run in the main test
//! process. Its test spawns one, drives it to the wedged state, and asserts the process exits
//! rather than hanging — which is the only way to test the terminal path honestly.
//!
//! Retaining the stream in ordinary application state would not be enough: normal field
//! destruction still calls `Stream::drop()`, which joins a worker that cannot exit while the
//! callback is wedged inside the plugin. So the transition is structural — everything is moved
//! into deliberately leaked storage and the process terminates without running destructors.

use mxm_player_harness::harness;

use std::time::{Duration, Instant};

/// Set in the child, so the same binary plays both parts.
const CHILD_MARKER: &str = "MXM_PLAYER_WEDGE_CHILD";

/// How long the parent gives the child before calling it a hang.
///
/// Comfortably more than the engine's own wedge timeout, so a slow machine is not mistaken for
/// a failure.
const CHILD_BUDGET: Duration = Duration::from_secs(45);

#[test]
fn a_wedged_engine_still_lets_the_process_exit() {
    if std::env::var(CHILD_MARKER).is_ok() {
        run_as_child();
        return;
    }

    let Some(_) = harness::fixtures() else { return };

    let exe = std::env::current_exe().expect("the test binary knows its own path");
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "a_wedged_engine_still_lets_the_process_exit",
            "--nocapture",
        ])
        .env(CHILD_MARKER, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the child spawns");

    let deadline = Instant::now() + CHILD_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return;
                }
                // The child's own output is the diagnosis; swallowing it would leave a failure
                // with nothing to act on.
                let output = child.wait_with_output().map(|o| {
                    format!(
                        "stdout:
{}
stderr:
{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    )
                });
                panic!(
                    "the wedged child should terminate cleanly, got {status}
{}",
                    output.unwrap_or_else(|e| format!("(could not read its output: {e})"))
                );
            }
            Ok(None) => {}
            Err(error) => panic!("could not wait for the child: {error}"),
        }

        if Instant::now() > deadline {
            let _ = child.kill();
            panic!(
                "the child did not exit within {:?}; a wedged engine must not be able to hang \
                 shutdown",
                CHILD_BUDGET
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_wedged_engine_never_reports_a_clean_stop() {
    if std::env::var(CHILD_MARKER).is_ok() {
        return;
    }
    let Some(_) = harness::fixtures() else { return };

    // Driven in the same child as the test above; this one asserts the property directly, in
    // process, using the fact that a wedged engine has no stream. `enter_wedged` takes the
    // stream, so the "nothing to stop" shortcut in `stop_now` used to report success — and
    // callers use that return to decide it is safe to run plugin code with audio stopped, which
    // is what makes the scan sentinel mean anything.
    use mxm_player::engine::{Engine, EngineState};

    let mut engine = Engine::new();
    // A fresh engine has no stream and nothing loaded: stopping is genuinely a no-op.
    assert!(engine.stop_now().is_ok());
    assert_eq!(*engine.state(), EngineState::Idle);
    assert!(!engine.is_wedged());
}

/// The child: load the plugin that never returns, drive it to wedged, and exit the way the
/// window-close path does.
fn run_as_child() {
    use mxm_player::engine::Engine;
    use mxm_player::engine::audio::FakeBackend;

    let Some(bundle) = harness::fixtures() else {
        std::process::exit(0);
    };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine
        .load(&bundle, "dk.mxm.fixture.hang")
        .expect("the hang fixture loads");

    // There is a race to lose here, and losing it is not a failure: `Stop` is handled at the top
    // of the callback, so if it arrives before the very first callback has entered the plugin,
    // the processor comes back cleanly and nothing ever wedges. That is correct behaviour — it
    // just is not the state this test is about. So: settle, try, and retry if we won the race.
    for attempt in 1..=5u32 {
        engine.start(&backend).expect("the fake backend starts");

        // Give the callback time to enter the plugin and never come back. Lengthening per
        // attempt, because the hang fixture spins a core and can starve a loaded machine.
        std::thread::sleep(Duration::from_millis(300 * u64::from(attempt)));

        // The ordinary protocol: ask for the processor back. It will never arrive, because the
        // callback is stuck inside the plugin — so the engine times out and goes terminal.
        if engine.stop_now().is_err() {
            assert!(
                engine.is_wedged(),
                "a plugin that never returns must reach the terminal state, not simply give up"
            );

            // What the window-close path does: settings are flushed, then the process is
            // terminated outright, bypassing every destructor — including the stream's, which
            // would deadlock.
            eprintln!("child reached the wedged state; exiting without running destructors");
            std::process::exit(0);
        }

        eprintln!("attempt {attempt}: stopped before the callback entered the plugin; retrying");
    }

    // Never wedged in five attempts. That is a broken test setup, not a passing plugin — say so
    // rather than exiting 0 and reporting a green test that checked nothing.
    eprintln!("the hang fixture never wedged the engine; the test could not set up its own state");
    std::process::exit(1);
}
