//! Regression checks for four `nice-plug` defects that MXM plugins inherit.
//!
//! All four are fixed by the patched `nice-plug` in the nice-plug fork (mxm-audio/nice-plug, see
//! its `PATCHES.md`; `vendor/nice-plug` in the monorepo), and all four are still present upstream
//! — so these tests are what stops a careless refresh of the fork from silently reintroducing them.
//!
//! *nice-plug 0.4.2 (2026-10-06):* "still present upstream" described 0.3.0. 0.4.2 fixes the rescan
//! and the non-finite value itself, bounds the state length itself, and has no wrapper output
//! queue, so the output case below now proves 0.4's direct `try_send_event()` instead. The fork's
//! `PATCHES.md` has each patch's status. The tests stay: they prove that what replaced a patch
//! still behaves.
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
/// wrapper whose `ProcessContext::try_send_event()` is under test (`send_event()` and its MXM
/// guard until nice-plug 0.4.2).
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

/// Establishes an audible note in one callback, then sends 2,500 ordinary events plus its release
/// into the next callback. The wrapper was activated for 64 frames, so its queue and each raw-event
/// inspection window are 1,024: 512 for the frames plus one per parameter, raised to the floor of
/// nice-plug 0.4.2's `Plugin::INPUT_EVENT_CAPACITY`. This hostile list is more than twice that, so
/// the wrapper skips its middle, and it cannot be mistaken for the old 513th-push threshold. The
/// note sounds before saturation, so subsequent silence can only demonstrate an admitted
/// termination, not eviction of the note-on that would have established it. This loads the guarded
/// debug binary directly as above.
///
/// *Before nice-plug 0.4.2* the window was 512 and 2,000 events were nearly four times it. 2,000
/// no longer reach twice 1,024. The player's own input list cannot carry four times 1,024 in one
/// callback, and its overflow would invoke global recovery instead of delivering the list, so the
/// count is 2,500 and the epoch check below makes sure the player delivered every event.
fn dense_events_child() {
    const HOSTILE_EVENT_COUNT: usize = 2_500;
    // 126 non-termination CC numbers on each of 16 channels.
    const DISTINCT_CONTROLLERS: usize = 126 * 16;

    let bundle = guarded_debug_mxm_mono_01();
    let mut h = harness::Harness::with_max_frames(&bundle, "dk.mxm.mxm-mono-01", 3, 64)
        .expect("mxm-mono-01 hosts");
    h.render(64);

    use mxm_player::events::input::{PRODUCER_QUEUE_CAPACITY, Payload};
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

    // The host coalesces a controller moved twice at one sample offset, so each event must be a
    // distinct controller at its offset or the flood shrinks before nice-plug sees it. The CC
    // numbers give 2,016 distinct controllers, not enough for one offset, so the arrival is
    // stamped explicitly: the first round arrives before the previous callback began and lands on
    // frame 0, the rest after this callback began and land on the last frame.
    //
    // *Before nice-plug 0.4.2* all 2,000 fitted one round and were pushed at the clock's time.
    let late = 0;
    let newest = u64::MAX - 1;
    for i in 0..HOSTILE_EVENT_COUNT {
        let pair = i % DISTINCT_CONTROLLERS;
        let ordinal = (pair % 126) as u8;
        let controller = match ordinal {
            0..=119 => ordinal,
            120..=121 => ordinal + 1,
            _ => ordinal + 2,
        };
        // Fill each producer's queue in turn, but leave one slot on the note's own source for its
        // release. Source identity is part of the player's press ledger, so releasing from another
        // producer would correctly match nothing and make this regression useless in a different
        // way.
        let source = (i + 1) / PRODUCER_QUEUE_CAPACITY;
        let arrival = if i < DISTINCT_CONTROLLERS {
            late
        } else {
            newest
        };
        assert!(h.push_at(
            source,
            arrival,
            Payload::ControlChange {
                channel: (pair / 126) as u8,
                controller,
                value: (i % 128) as u8,
            },
        ));
    }
    // After every hostile event, on the last frame.
    assert!(h.push_at(
        0,
        u64::MAX,
        Payload::NoteOff {
            channel: 0,
            key: 60,
            velocity: 0.0,
        },
    ));
    let epoch = h.input_epoch.current();
    h.render(64);
    assert_eq!(
        h.input_epoch.current(),
        epoch,
        "the player must deliver the whole hostile list: its own global recovery would silence \
         the note and prove nothing about the wrapper"
    );
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
    // Nearly four times the wrapper's 1,024-event window, so its middle is skipped. 2,000 until
    // nice-plug 0.4.2, when the window was 512.
    const HOSTILE_EVENT_COUNT: usize = 4_000;
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
    // queue with ordinary input before the zero-velocity NoteOn arrives at the end of the bounded
    // suffix. That queue holds 1,024 events since nice-plug 0.4.2, `Plugin::INPUT_EVENT_CAPACITY`'s
    // floor (512 before). mxm-para-07 treats that event as NoteOff, so it must survive saturation
    // just like an explicit NoteOff. Enough later callbacks are rendered for the real release tail
    // to settle to the instrument's exact inert silence.
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
        "a nice-plug plugin must not allocate when it fills the host's output-event list, and a \
         termination the full list refused must reach the host in the next call",
    );
}

/// **Since nice-plug 0.4.2 the wrapper has no output queue**, so the "configured wrapper capacity"
/// of this test's name no longer exists. The name is kept because mxm-kit's `docs/known-issues.md`
/// cites it. `try_send_event()` pushes straight into the host's output list, which here is the
/// player's fixed sink (`events::output::FixedEventBuffer`). That sink is the only bound: when it
/// refuses an event, the wrapper returns `SendEventError::HostBufferFull` and hands the event back.
///
/// The fixture sends its distinct note-on into the empty list, then ordinary note-ons until the
/// sink refuses one, then the distinct note's note-off. A CLAP output list is append-only, so
/// nothing can make room for the note-off in that call. The wrapper reports the refusal, and the
/// fixture sends the note-off first in its next call. So a termination after saturation still
/// reaches the host, one call later and only because the refusal was reported. All of it runs under
/// `assert_process_allocs`.
///
/// *Before nice-plug 0.4.2:* the fixture sent 1000 note-ons through `send_event()` after
/// activation for 64 frames, then the note-off for a distinct note admitted near the end of the
/// bounded prefix. The wrapper's configured bound was 513: 64 frames × 8 events plus one exposed
/// parameter. Its guarded path dropped ordinary overflow but displaced one ordinary event for the
/// termination, so exactly 513 events arrived in the same call, the note-off among them. The
/// unguarded `push_back` reallocated past its original 512-event storage under
/// `assert_process_allocs`.
fn dense_output_events_child() {
    use mxm_player::engine::PluginOutput;
    use mxm_player::midi::{is_press, is_release};

    let bundle = guarded_debug_output_fixture();
    let mut h =
        harness::Harness::with_max_frames(&bundle, "dk.mxm.fixture.nice-plug-output-flood", 1, 64)
            .expect("the nice-plug output fixture hosts");

    h.render(64);
    let saturated = h.drain_midi_out();
    assert!(
        h.drain_plugin_output()
            .contains(&PluginOutput::TrackingInvalidated),
        "the player's own output sink must be what refused the flood; nothing between the plugin \
         and the host may bound it first"
    );
    assert!(
        saturated
            .first()
            .is_some_and(|event| event.data[1] == 61 && is_press(event.data)),
        "the distinct note-on, sent into the empty list, must reach the host first"
    );
    assert!(
        !saturated
            .iter()
            .any(|event| event.data[1] == 61 && is_release(event.data)),
        "a full output list cannot take the termination in the same call"
    );

    h.render(64);
    let next = h.drain_midi_out();
    assert!(
        next.first()
            .is_some_and(|event| event.data[1] == 61 && is_release(event.data)),
        "the termination the full list refused must come back to the plugin with HostBufferFull \
         and reach the host first in its next call"
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
