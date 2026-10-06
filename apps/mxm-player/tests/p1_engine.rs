//! P1 — the output-only engine, driven end to end.
//!
//! Everything here runs against the fake backend rather than a real device, for two reasons:
//! CI has no audio hardware, and CPAL gives no public way to provoke a terminal stream failure.
//! The real backend is exercised by hand; that is recorded as a manual step in the plan rather
//! than pretended to be automated.

use mxm_player::engine::audio::FakeBackend;
use mxm_player::engine::{AudioConfig, Engine, EngineState};
use mxm_player::events::input::Payload;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PLUGIN: &str = "dk.mxm.mxm-mono-01";

fn bundle() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the player lives two levels below the workspace root")
        .join("target/bundled/mxm-mono-01.clap");

    if path.exists() {
        Some(path)
    } else {
        eprintln!(
            "skipping: {} is missing — run `cargo xtask bundle mxm-mono-01 --release`",
            path.display()
        );
        None
    }
}

/// Polls the engine the way the GUI does, until `done` or the deadline. Never blocks.
fn pump(engine: &mut Engine, timeout: Duration, mut done: impl FnMut(&Engine) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        engine.poll();
        if done(engine) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    engine.poll();
    done(engine)
}

fn wait_for(condition: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    condition()
}

#[test]
fn a_note_plays_through_the_running_engine() {
    let Some(bundle) = bundle() else { return };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    engine.start(&backend).expect("the fake backend starts");
    assert_eq!(*engine.state(), EngineState::Running);

    assert!(
        wait_for(|| backend.frames_rendered() > 0, Duration::from_secs(2)),
        "the callback should be running"
    );
    backend.clear_capture();

    assert!(engine.push_gui_event(Payload::NoteOn {
        channel: 0,
        key: 60,
        velocity: 100.0 / 127.0,
    }));

    assert!(
        wait_for(|| backend.peak() > 1e-3, Duration::from_secs(2)),
        "a hard-coded note should produce audible output, peak was {}",
        backend.peak()
    );

    engine.push_gui_event(Payload::NoteOff {
        channel: 0,
        key: 60,
        velocity: 0.0,
    });
    engine.stop_now().expect("the processor should come back");
}

#[test]
fn quiet_detection_sleeps_an_idle_plugin_and_a_note_wakes_it() {
    // The direct test of the `CONTINUE_IF_NOT_QUIET` path: nice-plug maps `ProcessStatus::Normal`
    // to it, and mxm-mono-01 returns `Normal` whenever it is idle — so a host that always kept
    // processing would never sleep our own synth.
    let Some(bundle) = bundle() else { return };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    engine.start(&backend).expect("the fake backend starts");

    let meters = engine.meters.clone();
    assert!(
        wait_for(|| meters.callbacks() > 32, Duration::from_secs(2)),
        "the callback should be running"
    );

    // Idle and exactly silent: the plugin should have been put to sleep by now, and the output
    // is silence either way.
    assert_eq!(backend.peak(), 0.0, "an idle synth renders exact silence");

    backend.clear_capture();
    engine.push_gui_event(Payload::NoteOn {
        channel: 0,
        key: 64,
        velocity: 1.0,
    });

    assert!(
        wait_for(|| backend.peak() > 1e-3, Duration::from_secs(2)),
        "a new note must wake a sleeping plugin"
    );

    engine.stop_now().expect("the processor should come back");
}

#[test]
fn repeated_device_and_plugin_changes_never_deadlock() {
    let Some(bundle) = bundle() else { return };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();

    for round in 0..6 {
        engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
        engine.set_audio_config(AudioConfig {
            device_name: None,
            sample_rate: Some(if round % 2 == 0 { 44_100 } else { 48_000 }),
            buffer_size: Some(if round % 3 == 0 { 128 } else { 512 }),
        });
        engine.start(&backend).expect("the fake backend starts");

        assert!(
            wait_for(|| engine.meters.callbacks() > 0, Duration::from_secs(2)),
            "round {round}: the callback should run"
        );

        engine
            .stop_now()
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        assert!(
            !engine.is_wedged(),
            "round {round}: a clean stop must not look like a hang"
        );
        assert!(
            !engine.leaked_processor(),
            "round {round}: nothing should have been leaked"
        );
    }
}

#[test]
fn a_terminal_backend_failure_is_recovered_from_not_misdiagnosed_as_a_hang() {
    // The distinction the whole stream-exit handoff exists for: after a fatal backend error no
    // further data callback runs, so nothing would honour `Command::Stop`. Without the handoff
    // the GUI would time out and blame the plugin.
    let Some(bundle) = bundle() else { return };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    engine.start(&backend).expect("the fake backend starts");

    assert!(
        wait_for(|| engine.meters.callbacks() > 0, Duration::from_secs(2)),
        "the callback should be running before the failure"
    );

    // The worker loop exits with no further data callback, exactly as CPAL's does.
    backend.kill();

    assert!(
        pump(&mut engine, Duration::from_secs(5), |e| matches!(
            e.state(),
            EngineState::StreamExited(_)
        )),
        "the engine should reach StreamExited, not Wedged; it is at {:?}",
        engine.state()
    );
    assert!(
        !engine.is_wedged(),
        "a dead stream must never be misdiagnosed as a wedged plugin"
    );
    assert!(!engine.leaked_processor());

    // ...and it reactivates without a process restart.
    let recovered = FakeBackend::new();
    engine
        .start(&recovered)
        .expect("the engine should reactivate once a device is chosen");
    assert_eq!(*engine.state(), EngineState::Running);
    engine.stop_now().expect("the processor should come back");
}

#[test]
fn a_dead_stream_is_reconnected_by_the_engine_once_the_device_is_back() {
    // What the player does after Windows takes the device away: the stream dies, the processor
    // comes back cleanly, and the engine keeps trying to start again — further apart each time —
    // until the device is there to be had. Driven the way `service` drives it, once per poll.
    let Some(bundle) = bundle() else { return };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    engine.start(&backend).expect("the fake backend starts");
    assert!(
        wait_for(|| engine.meters.callbacks() > 0, Duration::from_secs(2)),
        "the callback should be running before the failure"
    );
    assert!(
        engine.reconnect().is_none(),
        "nothing to reconnect while the stream is alive"
    );

    // The device goes away, and stays away: nothing can be built on it.
    backend.refuse_builds(true);
    backend.kill();
    assert!(
        pump(&mut engine, Duration::from_secs(5), |e| matches!(
            e.state(),
            EngineState::StreamExited(_)
        )),
        "the engine should reach StreamExited; it is at {:?}",
        engine.state()
    );
    let died_as = engine.state().clone();
    let reconnect = engine.reconnect().expect("a death schedules a reconnect");
    assert_eq!(reconnect.attempts, 0);
    assert_eq!(reconnect.last_failure, None);

    // Attempts are refused by the device, reported with its reason, and spaced further apart.
    let mut outcomes = Vec::new();
    let mut when = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while outcomes.len() < 2 && Instant::now() < deadline {
        engine.poll();
        if let Some(outcome) = engine.try_reconnect(&backend) {
            outcomes.push(outcome);
            when.push(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        outcomes.len(),
        2,
        "two refused attempts within five seconds"
    );
    for outcome in &outcomes {
        let refusal = outcome.as_ref().expect_err("the device refused");
        assert!(
            refusal.contains(mxm_player::engine::audio::REFUSED_BUILD),
            "the device's reason travels: {refusal}"
        );
    }
    assert!(
        when[1] - when[0] >= Duration::from_millis(400),
        "the second attempt waits at least twice the first delay; it waited {:?}",
        when[1] - when[0]
    );
    assert_eq!(
        *engine.state(),
        died_as,
        "a refused attempt leaves the original reason on show"
    );
    let reconnect = engine.reconnect().expect("still reconnecting");
    assert_eq!(reconnect.attempts, 2);
    assert!(
        reconnect
            .last_failure
            .as_deref()
            .is_some_and(|f| f.contains(mxm_player::engine::audio::REFUSED_BUILD)),
        "the last refusal is carried for display: {:?}",
        reconnect.last_failure
    );
    assert!(!engine.leaked_processor(), "refused attempts leak nothing");

    // The device comes back.
    backend.refuse_builds(false);
    backend.revive();
    let callbacks_before = engine.meters.callbacks();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut recovered = None;
    while recovered.is_none() && Instant::now() < deadline {
        engine.poll();
        if let Some(outcome) = engine.try_reconnect(&backend) {
            recovered = Some(outcome);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        recovered,
        Some(Ok(3)),
        "the third attempt succeeds once the device is back"
    );
    assert_eq!(*engine.state(), EngineState::Running);
    assert!(
        engine.reconnect().is_none(),
        "a running stream has nothing to reconnect"
    );
    assert!(
        wait_for(
            || engine.meters.callbacks() > callbacks_before,
            Duration::from_secs(2)
        ),
        "the callback runs again after the reconnect"
    );
    assert!(
        engine.try_reconnect(&backend).is_none(),
        "nothing to do while the stream is alive"
    );
    engine.stop_now().expect("the processor should come back");
}

#[test]
fn a_start_refused_by_the_device_leaves_the_plugin_ready_to_start_again() {
    // A device held in exclusive mode by another application refuses `Initialize`, so `build`
    // fails *after* the plugin was activated. The processor is handed back through the owner's
    // `Drop` inside the refused build — and unless the engine takes it and deactivates, the
    // instance stays active and every later start is refused by the plugin instead of the device.
    // With the reconnect in `service`, a refused build is routine, so this path must be clean.
    let Some(bundle) = bundle() else { return };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");

    backend.refuse_builds(true);
    let refused = engine
        .start(&backend)
        .expect_err("a refused build must fail the start");
    assert!(
        refused.contains(mxm_player::engine::audio::REFUSED_BUILD),
        "the device's reason comes through: {refused}"
    );
    assert_eq!(
        *engine.state(),
        EngineState::Idle,
        "a start that never produced a stream leaves the engine where it was"
    );
    assert!(!engine.leaked_processor());

    backend.refuse_builds(false);
    engine
        .start(&backend)
        .expect("the plugin must be reactivatable once the device is free");
    assert_eq!(*engine.state(), EngineState::Running);
    assert!(
        wait_for(|| engine.meters.callbacks() > 0, Duration::from_secs(2)),
        "the callback should run after the recovery"
    );
    engine.stop_now().expect("the processor should come back");
}

#[test]
fn the_meters_separate_plugin_time_from_callback_time() {
    let Some(bundle) = bundle() else { return };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    engine.start(&backend).expect("the fake backend starts");

    let meters = engine.meters.clone();
    assert!(
        wait_for(|| meters.callbacks() > 64, Duration::from_secs(2)),
        "the callback should be running"
    );

    assert!(
        meters.callback_load() >= meters.plugin_load(),
        "the callback includes the plugin, so it can never be the cheaper of the two"
    );
    assert_eq!(
        meters.realtime_priority(),
        mxm_player::engine::meters::PriorityStatus::Requested,
        "priority promotion is requested, and reported honestly as unconfirmed"
    );
    assert_eq!(
        meters.xruns(),
        None,
        "a backend that reports no xruns must not look like one reporting zero"
    );

    engine.stop_now().expect("the processor should come back");
}

#[test]
fn changing_the_midi_topology_while_playing_runs_the_stop_and_return_protocol() {
    // Connecting or disconnecting an input changes the set of producers, so it cannot be applied
    // under a running audio thread: queue slots are preallocated at activation and never resized.
    let Some(bundle) = bundle() else { return };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    engine.start(&backend).expect("the fake backend starts");
    assert_eq!(*engine.state(), EngineState::Running);

    assert!(
        wait_for(|| engine.meters.callbacks() > 0, Duration::from_secs(2)),
        "the callback should be running before the topology changes"
    );

    // A port that does not exist is still a topology change: what matters is the protocol.
    engine.set_midi_inputs(vec!["some keyboard".to_owned()]);
    assert_eq!(
        *engine.state(),
        EngineState::AwaitingStoppedProcessor,
        "the change must go through the stop-and-return protocol"
    );

    assert!(
        pump(&mut engine, Duration::from_secs(5), |e| *e.state()
            == EngineState::Idle),
        "the processor must come back; it is at {:?}",
        engine.state()
    );
    assert!(!engine.is_wedged());
    assert!(!engine.leaked_processor());

    // ...and it comes back up with the new topology, reporting what it could not connect.
    engine.start(&backend).expect("the engine restarts");
    assert_eq!(*engine.state(), EngineState::Running);
    let refused = engine.refused_midi_inputs();
    assert_eq!(
        refused.iter().map(|r| r.port.as_str()).collect::<Vec<_>>(),
        ["some keyboard"]
    );
    // The reason travels with the port: "busy", "gone" and "too many" need different answers,
    // so a refusal that named none of them sent the reader to the wrong one.
    assert!(
        !refused[0].reason.is_empty(),
        "a refused port must say why it was refused"
    );

    engine.stop_now().expect("the processor comes back");
}

#[test]
fn a_disconnected_input_with_thru_enabled_also_releases_what_went_outward() {
    let Some(bundle) = bundle() else { return };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine
        .thru_enabled
        .store(true, std::sync::atomic::Ordering::Release);
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    engine.set_midi_inputs(vec!["some keyboard".to_owned()]);
    engine.start(&backend).expect("the fake backend starts");

    let before = engine.out_panic.count();
    engine.set_midi_inputs(Vec::new());

    assert!(
        engine.out_panic.count() > before,
        "removing a producer with thru enabled must release outward too: external gear has no \
         idea the port went away"
    );

    let _ = pump(&mut engine, Duration::from_secs(5), |e| {
        *e.state() == EngineState::Idle
    });
    engine.stop_now().expect("the processor comes back");
}
