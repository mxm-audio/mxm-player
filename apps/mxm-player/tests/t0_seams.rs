//! T0 — the seams hold, and the app behaves the same through them.
//!
//! The acceptance criterion for T0: a test can construct the real `PlayerApp` and drive it without
//! touching the machine it runs on.

use mxm_player_harness::app_harness;

use app_harness::AppHarness;

#[test]
fn the_app_builds_and_runs_with_no_window_and_no_device() {
    let mut app = AppHarness::new("builds", Vec::new());
    app.run();

    let state = app.state();
    assert_eq!(state.engine, "Idle", "nothing is loaded yet");
    assert!(state.plugin.is_none());
    assert!(
        state.found.is_empty(),
        "an empty search path must find nothing"
    );
}

#[test]
fn a_sandboxed_run_never_touches_the_real_settings() {
    // The reason this seam exists. Before it, an app-level test rewrote the settings file of
    // whoever was using the player on the same machine.
    let real = mxm_player::settings::Settings::default_path();
    let before = std::fs::read(&real).ok();

    let mut app = AppHarness::new("settings", Vec::new());
    app.run();
    // Something that persists: changing the octave writes settings immediately.
    app.harness.state_mut().shift_octave(1);
    app.run();

    let after = std::fs::read(&real).ok();
    assert_eq!(
        before, after,
        "a sandboxed run must leave the real settings file exactly as it found it"
    );

    // ...and it wrote its own copy instead.
    assert!(
        app.dir().join("settings.json").exists(),
        "the sandboxed settings file should have been written"
    );
}

#[test]
fn discovery_searches_only_what_it_was_given() {
    // Without this, a test would discover whatever plugins happen to be installed on the machine
    // running it, and pass or fail accordingly.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut app = AppHarness::new("discovery", vec![bundled]);
    app.harness.state_mut().rescan();
    app.run();

    let state = app.state();
    assert!(
        state.lists_plugin("dk.mxm.mxm-mono-01"),
        "the staged bundle should be found, got {:?}",
        state.found.iter().map(|f| &f.id).collect::<Vec<_>>()
    );
    // **Every plugin found must have come from the directory we named**, which is the property
    // this test exists for. It used to assert `found.len() == 1`, which was the same claim only
    // while the collection had one plugin — a collection fact encoded in a player test, and it
    // broke the moment a second instrument was bundled. The player still needs to know nothing
    // about either of them.
    let bundled = app_harness::bundled_dir().expect("checked above");
    let named = bundled.to_string_lossy().to_string();
    let named = named.trim_end_matches(std::path::is_separator);
    for found in &state.found {
        assert!(
            found.location.contains(named),
            "{:?} came from {:?}, outside the directory we named",
            found.id,
            found.location
        );
    }
    assert!(
        !state.found.is_empty(),
        "the staged bundles should have been found"
    );
}

#[test]
fn servicing_runs_without_a_frame() {
    // Seam 4: the lifecycle the GUI runs each turn is callable headlessly, and it is the *same*
    // implementation — two would drift until a session test passed on a path no user takes.
    let mut app = AppHarness::new("servicing", Vec::new());
    app.run();

    for _ in 0..10 {
        app.harness.state_mut().service();
    }

    assert_eq!(
        app.state().engine,
        "Idle",
        "servicing an idle app is a no-op"
    );
}

#[test]
fn the_state_dump_describes_a_loaded_plugin() {
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut app = AppHarness::new("state-dump", vec![bundled.clone()]);
    app.harness.state_mut().rescan();
    app.run();

    app.harness.state_mut().load(
        bundled.join("mxm-mono-01.clap"),
        "dk.mxm.mxm-mono-01".to_owned(),
    );
    app.run();

    let state = app.state();
    let plugin = state.plugin.as_ref().expect("a plugin is loaded");
    assert_eq!(plugin.id, "dk.mxm.mxm-mono-01");
    assert_eq!(plugin.channels, 2, "mxm-mono-01 negotiates stereo");
    assert!(
        plugin.carries_voice_ids,
        "it advertises the CLAP note dialect"
    );
    assert!(
        plugin.params.len() > 20,
        "the panel's parameters are in the dump, got {}",
        plugin.params.len()
    );
    assert!(
        state.param("Cutoff").is_some(),
        "named parameters are addressable"
    );

    // And it is readable as JSON, which is what an investigation actually looks at.
    assert!(state.to_json().contains("dk.mxm.mxm-mono-01"));
}
