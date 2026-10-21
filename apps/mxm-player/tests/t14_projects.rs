//! T14 — the player's setup as a tutti project (the owner, 2026-10-21: "audio plugins, effects
//! plugins, musical data").
//!
//! Proved end to end on real plugins over the real command socket: a setup authored in one player
//! is saved as a project, the project is valid to the format's own crate, a second player opens it
//! to the same instrument, effect chain, sound, sequence and locks, and saving it again goes over
//! the same project, the same devices kept.

use mxm_player_harness::{app_harness, harness};
use std::path::{Path, PathBuf};

const PLUGIN: &str = "dk.mxm.mxm-mono-01";
const EFFECT: &str = "dk.mxm.fixture.effect";

/// A sandboxed player that has scanned mxm-mono-01 and the fixture effect, with its socket open.
fn player(name: &str) -> Option<(mxm_player::ui::PlayerApp, PathBuf)> {
    let bundled = app_harness::bundled_dir()?;
    let fixtures = harness::fixtures()?.parent()?.to_path_buf();
    let dir = std::env::temp_dir().join(format!("mxm-player-t14-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let config = mxm_player::config::PlayerConfig::sandboxed(
        &dir,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    )
    .with_search_paths(vec![bundled, fixtures]);
    let settings = config.settings_path.clone();
    let mut app = mxm_player::ui::PlayerApp::with_config(config);
    app.rescan();
    app.start_cli();
    Some((app, settings))
}

/// One command over the socket, servicing the app until it answers (as `t9_cli` does).
fn command(app: &mut mxm_player::ui::PlayerApp, settings: &Path, line: &str) -> String {
    let settings = settings.to_path_buf();
    let line = line.to_owned();
    let client = std::thread::spawn(move || mxm_player::cli::run_command(&settings, &line));
    let started = std::time::Instant::now();
    loop {
        app.service();
        if client.is_finished() || started.elapsed() > std::time::Duration::from_secs(10) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    client.join().expect("client thread").expect("an answer")
}

fn ok(app: &mut mxm_player::ui::PlayerApp, settings: &Path, line: &str) -> String {
    let answer = command(app, settings, line);
    assert!(answer.contains("ok"), "`{line}`: {answer}");
    answer
}

fn dump(app: &mut mxm_player::ui::PlayerApp, settings: &Path) -> serde_json::Value {
    serde_json::from_str(&command(app, settings, "dump")).expect("the dump is JSON")
}

fn cutoff(dump: &serde_json::Value) -> f64 {
    dump["state"]["plugin"]["params"]
        .as_array()
        .expect("parameters")
        .iter()
        .find(|param| {
            param["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case("cutoff"))
        })
        .and_then(|param| param["value"].as_f64())
        .expect("a cutoff")
}

#[test]
fn a_setup_saved_as_a_project_opens_in_another_player_as_it_was() {
    let Some((mut first, settings)) = player("save") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` and `cargo xtask fixtures` first");
        return;
    };
    for line in [
        "load mxm-mono-01",
        &format!("fx add {EFFECT}"),
        "fx off 1",
        "toggle 1 C3",
        "toggle 5 E3",
        "tie 6",
        "tempo 132",
        "set cutoff 0.3",
        "lock 5 cutoff 0.7",
    ] {
        ok(&mut first, &settings, line);
    }
    let saved = ok(&mut first, &settings, "saveproject Line");
    let folder = first.exports_dir().join("Line");
    assert!(saved.contains("Line"), "{saved}");
    let before = dump(&mut first, &settings);

    // The format's own crate opens it as a valid project with the setup in it.
    let opened = tutti_fs::open(&folder).expect("a tutti project");
    assert!(opened.report.is_valid(), "{:?}", opened.report);
    let doc = opened.project.document();
    let track = &doc.tracks[0];
    assert_eq!(track.devices.len(), 2);
    assert_eq!(track.devices[0].plugin, format!("clap:{PLUGIN}"));
    assert_eq!(track.devices[0].role, "instrument");
    assert!(track.devices[0].state.is_some(), "the instrument's sound");
    assert_eq!(track.devices[1].plugin, format!("clap:{EFFECT}"));
    assert!(!track.devices[1].enabled, "the effect that is off");
    assert_eq!(doc.tempo[0].bpm, 132.0);
    let notes: Vec<(i64, i64, u8)> = track.patterns[0]
        .events
        .iter()
        .filter_map(|event| match event {
            tutti::Event::Note(note) => Some((note.at, note.length, note.pitch)),
            _ => None,
        })
        .collect();
    // C3 untied on step 1, half a step; E3 on step 5 tied over step 6, two steps.
    let step = mxm_player::project::TICKS_PER_STEP;
    let note = |name| mxm_player::sequencer::pattern::parse_note(name).expect("a note");
    assert!(notes.contains(&(0, step / 2, note("C3"))), "{notes:?}");
    assert!(
        notes.contains(&(4 * step, 2 * step, note("E3"))),
        "{notes:?}"
    );
    assert!(
        doc.dependencies
            .iter()
            .any(|d| d.id == format!("clap:{PLUGIN}"))
    );

    // A second player opens it as it was: the sound, the chain, the sequence and its locks.
    let Some((mut second, settings2)) = player("open") else {
        return;
    };
    ok(
        &mut second,
        &settings2,
        &format!("openproject {}", folder.display()),
    );
    let after = dump(&mut second, &settings2);
    assert_eq!(
        after["state"]["project"].as_str(),
        Some(folder.display().to_string().as_str()),
        "the dump names the open project"
    );
    assert_eq!(after["state"]["plugin"]["id"], PLUGIN);
    assert_eq!(after["state"]["fx"][0]["id"], EFFECT);
    assert_eq!(after["state"]["fx"][0]["bypassed"], true);
    for key in ["steps", "tied", "tempo", "bars", "steps_per_bar", "locks"] {
        assert_eq!(
            after["state"]["sequencer"][key], before["state"]["sequencer"][key],
            "the sequencer's {key}"
        );
    }
    assert!(
        (cutoff(&after) - cutoff(&before)).abs() < 1e-6,
        "the sound: cutoff {} became {}",
        cutoff(&before),
        cutoff(&after)
    );

    // Saving it again goes over the same project and the same devices.
    let devices: Vec<String> = track
        .devices
        .iter()
        .map(|d| d.id.as_str().to_owned())
        .collect();
    let again = ok(&mut second, &settings2, "saveproject Line");
    assert!(again.contains("Line"), "{again}");
    let reopened = tutti_fs::open(&folder).expect("still a project");
    let doc = reopened.project.document();
    assert_eq!(doc.tracks.len(), 1);
    let kept: Vec<String> = doc.tracks[0]
        .devices
        .iter()
        .map(|d| d.id.as_str().to_owned())
        .collect();
    assert_eq!(kept, devices);
    assert!(
        !second.exports_dir().join("Line-2").exists(),
        "not a second project"
    );
}

#[test]
fn a_project_with_a_plugin_not_installed_opens_what_it_can_and_never_saves_over_it() {
    let Some((mut first, settings)) = player("missing") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` and `cargo xtask fixtures` first");
        return;
    };
    ok(&mut first, &settings, "load mxm-mono-01");
    ok(&mut first, &settings, "toggle 1 C3");
    ok(&mut first, &settings, "saveproject Lost");
    let folder = first.exports_dir().join("Lost");

    // Another machine's effect, not installed here.
    let mut project = tutti_fs::open(&folder).expect("a project").project;
    let mut elsewhere = tutti::Device::new(
        tutti_fs::ulid("dev"),
        "audio_effect",
        "clap:org.example.elsewhere",
    );
    elsewhere.name = "Elsewhere".to_owned();
    project.document_mut().tracks[0].devices.push(elsewhere);
    project
        .save(&mxm_player::project::generator())
        .expect("saved");

    let opened = ok(
        &mut first,
        &settings,
        &format!("openproject {}", folder.display()),
    );
    assert!(opened.contains("couldn't honour"), "{opened}");
    assert!(
        first
            .sequence_problems()
            .iter()
            .any(|p| p.contains("Elsewhere") && p.contains("isn't installed")),
        "{:?}",
        first.sequence_problems()
    );
    let refused = command(&mut first, &settings, "saveproject Lost");
    assert!(refused.contains("another name"), "{refused}");
    let doc = tutti_fs::open(&folder).expect("a project").project;
    assert_eq!(
        doc.document().tracks[0].devices.len(),
        2,
        "the effect it couldn't load is still there"
    );
}
