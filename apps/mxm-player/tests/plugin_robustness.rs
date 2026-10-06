//! Regression checks for four `nice-plug` defects that MXM plugins inherit.
//!
//! All four are fixed by the patched `nice-plug` in the nice-plug fork (mxm-audio/nice-plug, see
//! its `PATCHES.md`; `vendor/nice-plug` in the monorepo), and all four are still present upstream
//! — so these tests are what stops a careless refresh of the fork from silently reintroducing them.
//!
//! The allocation and hostile-state cases run in a subprocess, because unpatched they end in
//! `handle_alloc_error` — an abort, which no in-process assertion can catch.

use mxm_player_harness::harness;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CHILD_MARKER: &str = "MXM_PLAYER_ROBUSTNESS_CHILD";

/// The dense-event case must load the debug plugin binary directly. The ordinary shared helper
/// points at the release bundle, where nice-plug deliberately compiles out `assert_process_allocs`.
/// Missing debug evidence is therefore a failure for this regression rather than a skipped test.
fn guarded_debug_mxm_mono_01() -> PathBuf {
    guarded_debug_plugin("mxm_mono_01", "mxm-mono-01")
}

fn guarded_debug_mxm_para_07() -> PathBuf {
    guarded_debug_plugin("mxm_para_07", "mxm-para-07")
}

fn guarded_debug_plugin(binary_name: &str, package_name: &str) -> PathBuf {
    let binary = workspace_root().join("target").join("debug").join(format!(
        "{}{binary_name}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    assert!(
        binary.is_file(),
        "{} is missing; run `cargo xtask bundle {package_name}` without `--release` so nice-plug's allocation guard is compiled in",
        binary.display()
    );
    binary
}

/// The output-side regression must use a nice-plug plugin: the clack fixture emitter bypasses the
/// wrapper whose `ProcessContext::send_event()` guard is under test.
fn guarded_debug_output_fixture() -> PathBuf {
    let binary = workspace_root().join("target").join("debug").join(format!(
        "{}nice_plug_output_fixture{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    assert!(
        binary.is_file(),
        "{} is missing; run `cargo build -p nice-plug-output-fixture` so nice-plug's allocation guard is compiled in",
        binary.display()
    );
    binary
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the player lives two levels below the workspace root")
        .to_path_buf()
}

/// How long a child gets before it is called a hang.
const CHILD_BUDGET: Duration = Duration::from_secs(30);

/// Runs one named case in a subprocess and reports whether it exited cleanly.
fn run_child(test_name: &str, case: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(exe)
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD_MARKER, case)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    let deadline = Instant::now() + CHILD_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("the child exited with {status}"))
                };
            }
            Ok(None) => {}
            Err(error) => return Err(error.to_string()),
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err(format!("the child did not finish within {CHILD_BUDGET:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn more_events_than_the_configured_wrapper_capacity_do_not_allocate_or_abort() {
    if std::env::var(CHILD_MARKER).as_deref() == Ok("dense-events") {
        dense_events_child();
        return;
    }
    let _guarded_debug_binary = guarded_debug_mxm_mono_01();

    run_child(
        "more_events_than_the_configured_wrapper_capacity_do_not_allocate_or_abort",
        "dense-events",
    )
    .expect(
        "a plugin must survive a dense buffer of events; a host is entitled to send as many as \
         fit in the block",
    );
}

/// Establishes an audible note in one callback, then sends 2,000 ordinary events plus its release
/// into the next callback. The wrapper was activated for 64 frames, so its queue and each raw-event
/// inspection window are 512: this hostile list is nearly four times either bound and cannot be
/// mistaken for the old 513th-push threshold. The note sounds before saturation, so subsequent
/// silence can only demonstrate an admitted termination, not eviction of the note-on that would
/// have established it. This loads the guarded debug binary directly as above.
fn dense_events_child() {
    const HOSTILE_EVENT_COUNT: usize = 2_000;

    let bundle = guarded_debug_mxm_mono_01();
    let mut h = harness::Harness::with_max_frames(&bundle, "dk.mxm.mxm-mono-01", 2, 64)
        .expect("mxm-mono-01 hosts");
    h.render(64);

    use mxm_player::events::input::Payload;
    assert!(h.push(
        0,
        Payload::NoteOn {
            channel: 0,
            key: 60,
            velocity: 1.0,
        },
    ));
    h.render(64);
    assert!(
        h.peak() > 1e-3,
        "the target note must already be admitted and audible before the hostile callback"
    );

    // There are 126 non-termination CC numbers on each of 16 channels, enough to keep all 2,000
    // events distinct so the host does not coalesce the flood before nice-plug sees it.
    for i in 0..HOSTILE_EVENT_COUNT {
        let ordinal = (i % 126) as u8;
        let controller = match ordinal {
            0..=119 => ordinal,
            120..=121 => ordinal + 1,
            _ => ordinal + 2,
        };
        // Leave one slot on the note's own source for its release. Source identity is part of the
        // player's press ledger, so releasing from the other producer would correctly match
        // nothing and make this regression useless in a different way.
        let source = usize::from(i >= 1023);
        assert!(h.push(
            source,
            Payload::ControlChange {
                channel: (i / 126) as u8,
                controller,
                value: (i % 128) as u8,
            },
        ));
    }
    assert!(h.push(
        0,
        Payload::NoteOff {
            channel: 0,
            key: 60,
            velocity: 0.0,
        },
    ));
    h.render(64);
    // The init patch has a real release tail. Let that finish, then distinguish a delivered
    // NoteOff from the indefinitely sounding note that a dropped release leaves behind.
    for _ in 0..1_600 {
        h.render(64);
    }
    assert_eq!(
        h.render(64)
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0f32, f32::max),
        0.0,
        "overflow handling must admit the release of the note heard before saturation"
    );

    h.shutdown();
    std::process::exit(0);
}

#[test]
fn a_zero_velocity_note_on_after_input_saturation_still_terminates_the_note() {
    use mxm_player::offline::{EventKind, RenderConfig, ScheduledEvent, render};

    const BLOCK: u32 = 64;
    const HOSTILE_EVENT_COUNT: usize = 2_000;
    const RELEASE_FRAME: u64 = BLOCK as u64;
    const NOTE_ID: u32 = 7;
    const NOTE: u16 = 60;

    let bundle = guarded_debug_mxm_para_07();
    let mut events = Vec::with_capacity(HOSTILE_EVENT_COUNT + 2);
    events.push(ScheduledEvent {
        frame: 0,
        kind: EventKind::NoteOn {
            channel: 0,
            key: NOTE,
            velocity: 1.0,
            note_id: NOTE_ID,
        },
    });
    for i in 0..HOSTILE_EVENT_COUNT {
        events.push(ScheduledEvent {
            frame: RELEASE_FRAME,
            // CC 1 is ordinary input for mxm-para-07, not one of the termination CCs. Repeating
            // it is intentional: the offline driver's raw CLAP event list does not coalesce.
            kind: EventKind::Midi {
                data: [0xB0 | (i % 16) as u8, 1, (i % 128) as u8],
            },
        });
    }
    events.push(ScheduledEvent {
        frame: RELEASE_FRAME,
        kind: EventKind::NoteOn {
            channel: 0,
            key: NOTE,
            velocity: 0.0,
            note_id: NOTE_ID,
        },
    });

    // The target note is established in the first callback. The next callback fills the wrapper's
    // 512-event queue with ordinary input before the zero-velocity NoteOn arrives at the end of the
    // bounded suffix. mxm-para-07 treats that event as NoteOff, so it must survive saturation just
    // like an explicit NoteOff. Enough later callbacks are rendered for the real release tail to
    // settle to the instrument's exact inert silence.
    let result = render(
        &bundle,
        "dk.mxm.mxm-para-07",
        RenderConfig {
            state: None,
            sample_rate: 48_000.0,
            block_size: BLOCK,
            total_frames: u64::from(BLOCK) * 1_603,
        },
        &events,
    )
    .expect("the guarded mxm-para-07 binary renders the hostile input list");
    let channel = &result.channels[0];
    assert!(
        channel[..BLOCK as usize]
            .iter()
            .any(|sample| sample.abs() > 1e-3),
        "the target note must sound in the callback before saturation"
    );
    assert_eq!(
        channel[channel.len() - BLOCK as usize..]
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0f32, f32::max),
        0.0,
        "a zero-velocity NoteOn after saturation must release the earlier-callback note"
    );
}

#[test]
fn more_output_events_than_the_configured_wrapper_capacity_do_not_allocate_or_abort() {
    if std::env::var(CHILD_MARKER).as_deref() == Ok("dense-output-events") {
        dense_output_events_child();
        return;
    }
    let _guarded_debug_binary = guarded_debug_output_fixture();

    run_child(
        "more_output_events_than_the_configured_wrapper_capacity_do_not_allocate_or_abort",
        "dense-output-events",
    )
    .expect(
        "a nice-plug plugin must not grow its output queue when it emits more events than the configured bound",
    );
}

/// The fixture emits 1000 note-ons through nice-plug after activation for 64 frames, followed by
/// the note-off for a distinct note admitted near the end of the bounded prefix. The wrapper's
/// configured bound is 513: 64 frames × 8 events plus one exposed parameter. The guarded path drops
/// ordinary overflow but displaces one ordinary event for the termination, while the unguarded
/// `push_back` reallocates past its original 512-event storage under `assert_process_allocs`.
fn dense_output_events_child() {
    let bundle = guarded_debug_output_fixture();
    let mut h =
        harness::Harness::with_max_frames(&bundle, "dk.mxm.fixture.nice-plug-output-flood", 1, 64)
            .expect("the nice-plug output fixture hosts");

    h.render(64);
    let events = h.drain_midi_out();
    assert_eq!(
        events.len(),
        513,
        "the wrapper must keep exactly its configured output-event capacity"
    );

    let admitted = events
        .iter()
        .position(|event| event.data[1] == 61 && mxm_player::midi::is_press(event.data))
        .expect("the distinct note-on inside the configured capacity must reach the host");
    let terminated = events
        .iter()
        .position(|event| event.data[1] == 61 && mxm_player::midi::is_release(event.data))
        .expect("the distinct note's post-saturation termination must reach the host");
    assert!(
        admitted < terminated,
        "the admitted note must reach the host before its termination"
    );

    h.shutdown();
    std::process::exit(0);
}

#[test]
fn malformed_state_is_rejected_rather_than_aborting_the_plugin() {
    if std::env::var(CHILD_MARKER).as_deref() == Ok("invalid-state") {
        invalid_state_child();
        return;
    }
    let Some(_) = harness::mxm_mono_01() else {
        return;
    };

    run_child(
        "malformed_state_is_rejected_rather_than_aborting_the_plugin",
        "invalid-state",
    )
    .expect(
        "loading corrupt state must fail cleanly; a plugin that aborts on a bad preset takes the \
         host down with it",
    );
}

/// Feeds the plugin a state blob whose leading length field is nonsense.
///
/// This is not a hypothetical: a truncated or corrupted project file produces exactly this, and
/// the plugin should refuse it rather than try to allocate an exabyte.
fn invalid_state_child() {
    use mxm_player::engine::Engine;

    let Some(bundle) = harness::mxm_mono_01() else {
        std::process::exit(0);
    };

    let dir = std::env::temp_dir().join("mxm-player-invalid-state");
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("corrupt.clapstate");

    // A plausible-looking blob with a hostile length prefix.
    let mut bytes = 0x0E3A_51B2_C4D6_7788u64.to_le_bytes().to_vec();
    bytes.extend_from_slice(b"{\"not\": \"a valid state\"}");
    std::fs::write(&path, &bytes).expect("the scratch file is writable");

    let mut engine = Engine::new();
    engine
        .load(&bundle, "dk.mxm.mxm-mono-01")
        .expect("mxm-mono-01 loads");
    let mut params = engine.read_params();

    // Failing is the correct outcome; aborting is not. Either return is acceptable here — the
    // test is that the process survives to print this line.
    let outcome = engine.load_state(&path, &mut params);
    eprintln!("load_state returned: {outcome:?}");

    std::process::exit(0);
}

#[test]
fn loading_state_tells_the_host_its_parameter_values_are_stale() {
    // The third defect: the state round-trip was always *correct*, but silent. Only the editor
    // was notified, so a host that trusted the callback showed stale values — which is why the
    // player requeries unconditionally after a load, and why `clap-validator`'s three
    // `state-reproducibility-*` tests failed.
    use mxm_player::engine::Engine;
    use std::sync::atomic::Ordering;

    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };

    let mut engine = Engine::new();
    engine
        .load(&bundle, "dk.mxm.mxm-mono-01")
        .expect("mxm-mono-01 loads");
    let mut params = engine.read_params();

    let dir = std::env::temp_dir().join("mxm-player-rescan-test");
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("preset.clapstate");
    engine.save_state(&path).expect("state saves");

    // Nothing has asked for a rescan yet.
    engine
        .shared()
        .notifications
        .param_rescan
        .store(0, Ordering::Release);

    engine.load_state(&path, &mut params).expect("state loads");

    // `load_state` requeries on its own — the player does not rely on this callback — but the
    // plugin must still send it, or every other host is left with a stale panel.
    assert_ne!(
        engine
            .shared()
            .notifications
            .param_rescan
            .load(Ordering::Acquire),
        0,
        "loading state must tell the host its cached parameter values are stale"
    );
}

#[test]
fn a_non_finite_parameter_value_from_the_host_is_dropped() {
    // **Defect 4, found by mxm-mono-00's code review (round 4).** A `CLAP_EVENT_PARAM_VALUE`
    // carrying NaN passed straight through the wrapper: `f32::clamp` returns NaN for NaN, so
    // every range bound below it was moot, the smoother took the value, and the DSP multiplied
    // it into the mix -- `NaN * 0` is `NaN`, so one poisoned parameter silenced the instrument
    // for good. The patched wrapper drops the event; the parameter keeps its value and the note
    // keeps sounding. Driven through the player's own host path, as the other three are.
    use mxm_player::events::input::Payload;
    use mxm_player::session::Session;

    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };
    let dir = bundle
        .parent()
        .expect("a bundle has a directory")
        .to_path_buf();
    let mut session = Session::scratch("robustness-nan-param", vec![dir]);
    session.load(&bundle, "dk.mxm.mxm-mono-01");

    let (cutoff, before) = {
        let state = session.state();
        let param = state
            .plugin
            .as_ref()
            .expect("mxm-mono-01 is loaded")
            .params
            .iter()
            .find(|p| p.name == "Cutoff")
            .expect("mxm-mono-01 has a cutoff");
        (param.id, param.value)
    };

    session.app().note_on(60, 1.0);
    session.advance_blocks(4).expect("advance");
    session.clear_capture();

    session
        .app()
        .engine_mut()
        .push_gui_event(Payload::ParamValue {
            param_id: cutoff,
            value: f64::NAN,
        });
    session.advance_blocks(8).expect("advance");

    let audio = session.captured();
    assert!(
        audio.iter().all(|s| s.is_finite()),
        "a NaN parameter value from the host poisoned the audio"
    );
    assert!(
        audio.iter().any(|s| s.abs() > 1e-3),
        "and the note went silent under it"
    );
    let after = session
        .state()
        .plugin
        .expect("still loaded")
        .params
        .into_iter()
        .find(|p| p.id == cutoff)
        .expect("cutoff")
        .value;
    assert!(
        after.is_finite() && (after - before).abs() < 1e-6,
        "the cutoff reads {after} where it was {before}: the non-finite value was applied"
    );
}
