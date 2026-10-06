//! The CLI, end to end: a real socket, a real plugin, real answers.
//!
//! The contract under test is the one that makes the player machine-testable at all: **everything a
//! person can do has a verb, and `dump` shows what the window shows plus what it deliberately
//! hides** — the uncorrected readbacks and the host bookkeeping that base-vs-patch bugs live in.

use mxm_player_harness::app_harness;

use mxm_player::sequencer::Transport;
use std::path::{Path, PathBuf};

const PLUGIN: &str = "dk.mxm.mxm-mono-01";

/// A sandboxed player with the CLI attached, plus the settings path a client needs to find it.
fn player_with_cli(name: &str) -> Option<(mxm_player::ui::PlayerApp, PathBuf)> {
    player_with_cli_on(
        name,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    )
}

/// [`player_with_cli`] on a backend the test keeps a handle to, so it can take the device away.
fn player_with_cli_on(
    name: &str,
    backend: Box<dyn mxm_player::engine::audio::Backend>,
) -> Option<(mxm_player::ui::PlayerApp, PathBuf)> {
    let bundled = app_harness::bundled_dir()?;
    let dir = std::env::temp_dir().join(format!("mxm-player-cli-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");

    let config = mxm_player::config::PlayerConfig::sandboxed(&dir, backend)
        .with_search_paths(vec![bundled.clone()]);
    let settings_path = config.settings_path.clone();
    let mut app = mxm_player::ui::PlayerApp::with_config(config);
    app.load(bundled.join("mxm-mono-01.clap"), PLUGIN.to_owned());
    app.start_cli();
    Some((app, settings_path))
}

/// Sends one command over the real socket, servicing the app until it answers.
///
/// The listener thread blocks on the GUI thread's reply, so the app must be serviced concurrently
/// with the request — a second thread sends while this one services.
fn command(app: &mut mxm_player::ui::PlayerApp, settings: &Path, line: &str) -> String {
    let settings = settings.to_path_buf();
    let line = line.to_owned();
    let client = std::thread::spawn(move || mxm_player::cli::run_command(&settings, &line));
    let started = std::time::Instant::now();
    loop {
        app.service();
        if client.is_finished() || started.elapsed() > std::time::Duration::from_secs(5) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    client.join().expect("client thread").expect("an answer")
}

#[test]
fn the_cli_authors_a_sequence_and_the_dump_shows_it() {
    let Some((mut app, settings)) = player_with_cli("author") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // Author a sequence entirely over the socket: notes, a tie, tempo, and a lock.
    assert!(command(&mut app, &settings, "toggle 1 C3").contains("ok"));
    assert!(command(&mut app, &settings, "toggle 5 C3").contains("ok"));
    assert!(command(&mut app, &settings, "tie 6").contains("ok"));
    assert!(command(&mut app, &settings, "tempo 132").contains("132"));
    assert!(command(&mut app, &settings, "set cutoff 0.3").contains("ok"));
    let locked = command(&mut app, &settings, "lock 5 cutoff 0.7");
    assert!(locked.contains("ok"), "locking over the CLI: {locked}");

    // The dump reflects all of it, machine-readably.
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    assert_eq!(dump["state"]["sequencer"]["tempo"], 132.0);
    assert_eq!(dump["state"]["sequencer"]["steps"][0][0], "C3");
    assert_eq!(dump["state"]["sequencer"]["tied"][5], true);
    let lock = &dump["state"]["sequencer"]["locks"][0];
    assert_eq!(lock["name"], "Cutoff");
    assert!(
        (lock["steps"][4].as_f64().expect("a lock at step 5") - 0.7).abs() < 1e-3,
        "the lock the CLI wrote is in the dump: {lock}"
    );

    // **And the parts the window hides**: the patch record and the raw readbacks are present, keyed
    // by parameter id — this is what makes base-vs-patch divergence visible to a machine at all.
    assert!(
        dump["sequence_patch"]
            .as_object()
            .is_some_and(|m| !m.is_empty()),
        "the dump carries the sequence-patch: {}",
        dump["sequence_patch"]
    );
    assert!(
        dump["raw_readback"]
            .as_object()
            .is_some_and(|m| !m.is_empty()),
        "the dump carries uncorrected readbacks"
    );
}

#[test]
fn the_cli_drives_the_transport_and_the_locks_stay_absolute() {
    let Some((mut app, settings)) = player_with_cli("transport") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    assert!(command(&mut app, &settings, "toggle 1 C3").contains("ok"));
    assert!(command(&mut app, &settings, "play").contains("ok"));
    assert_eq!(app.sequencer_state().transport, Transport::Playing);
    assert!(command(&mut app, &settings, "stop").contains("ok"));
    assert_eq!(app.sequencer_state().transport, Transport::Stopped);

    // The lock verb goes through the same funnel a selected-step edit does, so the lock it writes
    // is absolute: move the patch afterwards and the lock has not moved.
    assert!(command(&mut app, &settings, "set cutoff 0.3").contains("ok"));
    assert!(command(&mut app, &settings, "lock 5 cutoff 0.8").contains("ok"));
    assert!(command(&mut app, &settings, "set cutoff 0.5").contains("ok"));
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    let lock = &dump["state"]["sequencer"]["locks"][0];
    assert!(
        (lock["steps"][4].as_f64().expect("still locked") - 0.8).abs() < 1e-3,
        "the patch moved and the lock did not: {lock}"
    );

    // Unlock is a verb too.
    assert!(command(&mut app, &settings, "unlock 5 cutoff").contains("ok"));
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    assert!(
        dump["state"]["sequencer"]["locks"]
            .as_array()
            .is_some_and(|l| l.is_empty()),
        "unlocked: {}",
        dump["state"]["sequencer"]["locks"]
    );
}

#[test]
fn the_cli_round_trips_an_effects_durable_state() {
    let Some((mut app, settings)) = player_with_cli("fx-state") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    assert!(command(&mut app, &settings, "rescan").contains("\"ok\""));
    let added = command(&mut app, &settings, "fx add mxm-fx-curve");
    if added.contains("\"error\"") {
        eprintln!("skipped: run `cargo xtask bundle mxm-fx-curve` first ({added})");
        return;
    }

    let dumped = command(&mut app, &settings, "fx dumpstate 1");
    assert!(dumped.contains("\"ok\""), "dumping effect state: {dumped}");
    let path = settings.with_file_name("effect-1.clapstate");
    assert!(
        std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 8),
        "the command writes a non-empty CLAP state at {}",
        path.display()
    );

    let loaded = command(&mut app, &settings, "fx loadstate 1");
    assert!(loaded.contains("\"ok\""), "loading effect state: {loaded}");
}

#[test]
fn the_cli_exports_a_sound() {
    let Some((mut app, settings)) = player_with_cli("export") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    assert!(command(&mut app, &settings, "toggle 1 C3").contains("ok"));
    let answer = command(&mut app, &settings, "export cli-proof");
    assert!(answer.contains("ok"), "export over the CLI: {answer}");

    // The answer names the file, and the file is a real WAV with audio in it.
    let path = answer
        .split("exported ")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .map(|s| s.replace("\\\\", "\\"))
        .expect("the answer names the file");
    let decoded = mxm_audio_file_decode::decode_file(
        path.trim(),
        &mxm_audio_file_decode::Limits::new(
            usize::MAX,
            mxm_audio_file_decode::AtLimit::Refuse,
            mxm_audio_file_decode::Keep::AllUpTo(8),
        ),
    )
    .expect("a readable WAV");
    assert!(decoded.frames() > 0, "and it holds samples");
}

#[test]
fn an_unknown_command_answers_with_the_vocabulary() {
    let Some((mut app, settings)) = player_with_cli("help") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let answer = command(&mut app, &settings, "frobnicate");
    assert!(
        answer.contains("commands:") && answer.contains("export"),
        "a wrong verb teaches the right ones: {answer}"
    );
}

/// The dump, parsed.
fn dump(app: &mut mxm_player::ui::PlayerApp, settings: &Path) -> serde_json::Value {
    serde_json::from_str(&command(app, settings, "dump")).expect("dump is JSON")
}

/// Services the app until the dump satisfies `done`, or the timeout passes.
fn service_until(
    app: &mut mxm_player::ui::PlayerApp,
    settings: &Path,
    timeout: std::time::Duration,
    mut done: impl FnMut(&serde_json::Value) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if done(&dump(app, settings)) {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        app.service();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn play_is_refused_while_the_audio_device_is_stopped_and_returns_when_it_reconnects() {
    // The day this was written the status bar read `Failed to get current padding: OS Error
    // -2004287484` after Windows took the device away, and `play` answered "playing from step 1"
    // over silence. Now the death is named, Play refuses with the reason, and the player comes
    // back on its own once the device is free — all visible over the socket.
    let backend = mxm_player::engine::audio::FakeBackend::new();
    let kill = std::sync::Arc::clone(&backend.kill);
    let refuse = std::sync::Arc::clone(&backend.refuse_build);
    let Some((mut app, settings)) = player_with_cli_on("dead-stream", Box::new(backend)) else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let five_seconds = std::time::Duration::from_secs(5);
    assert!(
        service_until(&mut app, &settings, five_seconds, |d| d["state"]["engine"]
            == "Running"),
        "the player starts on the fake device"
    );
    assert!(command(&mut app, &settings, "play").contains("ok"));
    assert!(
        service_until(
            &mut app,
            &settings,
            five_seconds,
            |d| d["state"]["sequencer"]["transport"] == "playing"
        ),
        "the worker reports the transport playing before the device goes"
    );

    // Windows takes the device away, and another application keeps it: nothing can be built.
    refuse.store(true, std::sync::atomic::Ordering::Release);
    kill.store(true, std::sync::atomic::Ordering::Release);
    assert!(
        service_until(&mut app, &settings, five_seconds, |d| d["state"]["engine"]
            .as_str()
            .is_some_and(|s| s.starts_with("StreamExited"))),
        "the death is reported"
    );
    let state = dump(&mut app, &settings);
    assert!(
        state["state"]["audio"]["reconnect"].is_object(),
        "the reconnect is visible in the dump: {}",
        state["state"]["audio"]
    );
    // Two things stop it: the GUI's own transport, and the playhead the dead worker last
    // published — which the dump reports, and which would otherwise still say "playing".
    assert_eq!(
        state["state"]["sequencer"]["transport"], "stopped",
        "a stream death stops the transport rather than leaving Play lit over silence"
    );

    // Play is refused, and says why.
    let answer = command(&mut app, &settings, "play");
    assert!(
        answer.contains("error") && answer.contains("audio device stopped"),
        "play must refuse with the reason: {answer}"
    );
    assert_eq!(
        dump(&mut app, &settings)["state"]["sequencer"]["transport"],
        "stopped",
        "a refused play changes nothing"
    );

    // The attempts are refused by the device, and the refusals are shown.
    assert!(
        service_until(&mut app, &settings, five_seconds, |d| {
            d["state"]["audio"]["reconnect"]["attempts"]
                .as_u64()
                .is_some_and(|n| n >= 1)
                && d["state"]["audio"]["reconnect"]["last_failure"]
                    .as_str()
                    .is_some_and(|f| f.contains(mxm_player::engine::audio::REFUSED_BUILD))
        }),
        "refused attempts are counted and carry the device's reason"
    );

    // The device is free again: the player comes back by itself, and says so.
    refuse.store(false, std::sync::atomic::Ordering::Release);
    kill.store(false, std::sync::atomic::Ordering::Release);
    assert!(
        service_until(
            &mut app,
            &settings,
            std::time::Duration::from_secs(15),
            |d| d["state"]["engine"] == "Running"
        ),
        "the player reconnects once the device is back"
    );
    let state = dump(&mut app, &settings);
    assert!(state["state"]["audio"]["reconnect"].is_null());
    assert!(
        state["state"]["status"]
            .as_str()
            .is_some_and(|s| s.contains("reconnected")),
        "the reconnect is announced: {}",
        state["state"]["status"]
    );
    assert!(
        command(&mut app, &settings, "play").contains("ok"),
        "play works again"
    );
    assert_eq!(
        dump(&mut app, &settings)["state"]["sequencer"]["transport"],
        "playing"
    );
}

#[test]
fn the_cli_loads_a_plugin_by_name() {
    // A genuinely empty player, not the helper's - it pre-loads the plugin, which would let a
    // do-nothing verb pass this test.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let dir = std::env::temp_dir().join("mxm-player-cli-load");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let config = mxm_player::config::PlayerConfig::sandboxed(
        &dir,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    )
    .with_search_paths(vec![bundled]);
    let settings = config.settings_path.clone();
    let mut app = mxm_player::ui::PlayerApp::with_config(config);
    app.start_cli();
    app.rescan();
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    assert!(dump["state"]["plugin"].is_null(), "nothing loaded yet");
    let answer = command(&mut app, &settings, "load mxm-mono-01");
    assert!(answer.contains("ok"), "loading over the CLI: {answer}");
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    assert_eq!(dump["state"]["plugin"]["id"], PLUGIN);

    let refused = command(&mut app, &settings, "load nonsense-9000");
    assert!(refused.contains("error"), "a miss says so: {refused}");
}

#[test]
fn the_cc_verb_reaches_the_control_map() {
    // What a hardware knob does, as a sentence: `cc 74 96` must move cutoff through the same
    // claimed-CC routing the M32's knob uses, visibly in the dump.
    let Some((mut app, settings)) = player_with_cli("cc") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    let before: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    for value in ["0", "60", "127"] {
        assert!(command(&mut app, &settings, &format!("cc 74 {value}")).contains("ok"));
        app.service();
    }
    let after: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");

    let cutoff = |dump: &serde_json::Value| {
        dump["state"]["plugin"]["params"]
            .as_array()
            .and_then(|params| params.iter().find(|p| p["name"] == "Cutoff"))
            .and_then(|p| p["value"].as_f64())
            .expect("cutoff is in the dump")
    };
    assert!(
        cutoff(&after) > cutoff(&before),
        "CC 74 at full must have raised cutoff: {} -> {}",
        cutoff(&before),
        cutoff(&after)
    );

    let refused = command(&mut app, &settings, "cc 200 1");
    assert!(
        refused.contains("error"),
        "out-of-range is refused: {refused}"
    );
}

#[test]
fn every_bar_act_has_a_verb_and_the_dump_shows_the_shape() {
    // Plan section 6 step 5, as a binding contract: select a bar, copy, paste, clear, both
    // dimensions, and the loop scope - each a verb, each visible in the dump. This is what makes
    // a long sequence reachable before any of it is drawable.
    let Some((mut app, settings)) = player_with_cli("bars") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    assert!(command(&mut app, &settings, "bars 4").contains("4 bars"));
    assert!(command(&mut app, &settings, "steps 12").contains("12 steps"));
    assert!(command(&mut app, &settings, "toggle 13 C3").contains("ok")); // bar 2's first step
    assert!(command(&mut app, &settings, "bar 2").contains("ok"));
    assert!(command(&mut app, &settings, "copybar").contains("copied"));
    assert!(command(&mut app, &settings, "bar 4").contains("ok"));
    assert!(command(&mut app, &settings, "paste").contains("pasted"));
    assert!(command(&mut app, &settings, "loop bar").contains("looping bar 4"));

    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    let sequencer = &dump["state"]["sequencer"];
    assert_eq!(sequencer["bars"], 4);
    assert_eq!(sequencer["steps_per_bar"], 12);
    assert_eq!(sequencer["selected_bar"], 3);
    assert_eq!(sequencer["loop_scope"], "bar");
    assert_eq!(
        sequencer["bars_with_notes"],
        serde_json::json!([false, true, false, true]),
        "the chips' fill is machine-visible"
    );
    assert_eq!(sequencer["steps"][36][0], "C3", "the paste landed in bar 4");

    assert!(command(&mut app, &settings, "clearbar").contains("cleared"));
    assert!(command(&mut app, &settings, "loop all").contains("whole sequence"));
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    assert_eq!(
        dump["state"]["sequencer"]["bars_with_notes"],
        serde_json::json!([false, true, false, false]),
        "the cleared bar emptied"
    );
    assert_eq!(dump["state"]["sequencer"]["loop_scope"], "all");
}

#[test]
fn a_step_is_addressable_both_ways_and_a_far_bar_grows_on_write() {
    // `33` and `3:1` are two spellings of one step, and writing into a bar beyond the end is how
    // a generated sequence comes to exist at all - `bars N` first is not required.
    let Some((mut app, settings)) = player_with_cli("addressing") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    assert!(command(&mut app, &settings, "toggle 3:1 C3").contains("ok"));
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    assert_eq!(
        dump["state"]["sequencer"]["bars"], 3,
        "the write grew the sequence"
    );
    assert_eq!(
        dump["state"]["sequencer"]["steps"][32][0], "C3",
        "3:1 is absolute step 33"
    );

    // The same step, absolutely - toggling it back off.
    assert!(command(&mut app, &settings, "toggle 33 C3").contains("ok"));
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    assert!(
        dump["state"]["sequencer"]["steps"][32]
            .as_array()
            .is_some_and(|notes| notes.is_empty()),
        "33 and 3:1 name the same step"
    );
}

#[test]
fn the_selection_speaks_ranges_and_lists_and_the_clipboard_verbs_use_them() {
    let Some((mut app, settings)) = player_with_cli("selection") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    assert!(command(&mut app, &settings, "toggle 1 C3").contains("ok"));
    assert!(command(&mut app, &settings, "toggle 2 E3").contains("ok"));

    // A scattered list: the first named step is the anchor.
    assert!(command(&mut app, &settings, "select 1,5,9").contains("3 steps"));
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    assert_eq!(
        dump["state"]["sequencer"]["selected_steps"],
        serde_json::json!([0, 4, 8]),
        "the selection is machine-visible"
    );

    // A range, copied and pasted elsewhere.
    assert!(command(&mut app, &settings, "select 1-2").contains("steps 1-2"));
    assert!(command(&mut app, &settings, "copysteps").contains("2 steps copied"));
    assert!(command(&mut app, &settings, "select 9").contains("ok"));
    assert!(command(&mut app, &settings, "paste").contains("2 steps pasted"));
    let dump: serde_json::Value =
        serde_json::from_str(&command(&mut app, &settings, "dump")).expect("dump is JSON");
    assert_eq!(dump["state"]["sequencer"]["steps"][8][0], "C3");
    assert_eq!(dump["state"]["sequencer"]["steps"][9][0], "E3");
}

#[test]
fn savemid_writes_a_midi_file_that_reads_back() {
    // Found missing during a composing session: `save_midi` existed and had no verb, so the
    // machine could author a track and not hand its MIDI to a DAW.
    let Some((mut app, settings)) = player_with_cli("savemid") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    assert!(command(&mut app, &settings, "toggle 1 C3").contains("ok"));
    assert!(command(&mut app, &settings, "toggle 5 E3").contains("ok"));
    let answer = command(&mut app, &settings, "savemid cli-proof");
    assert!(answer.contains("ok"), "savemid over the CLI: {answer}");

    let path = answer
        .split("saved ")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .map(|s| s.replace("\\\\", "\\"))
        .expect("the answer names the file");
    let bytes = std::fs::read(path.trim()).expect("a readable .mid");
    assert_eq!(&bytes[..4], b"MThd", "a real SMF header");
    assert!(bytes.len() > 30, "with music in it");
}
