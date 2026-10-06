//! T2 — one regression test per defect found by a human looking at the screen.
//!
//! The keyboard and slider defects are covered by the oracle spikes in `t1_oracles.rs`, which are
//! written as regressions rather than throwaway probes. This file covers the fourth — duplicate
//! plugin rows — and the behaviours around the browser that went with it.

use mxm_player_harness::app_harness;

use app_harness::AppHarness;
use kittest::Queryable;
use mxm_player::sequencer::Transport;
use std::path::PathBuf;

/// Stages a second, distinct copy of the bundle so the browser sees the same plugin twice.
///
/// The real situation: an installed copy alongside the build output, one of them stale.
fn stage_second_copy(name: &str) -> Option<(PathBuf, PathBuf)> {
    let bundled = app_harness::bundled_dir()?;
    let source = bundled.join("mxm-mono-01.clap");

    let second = std::env::temp_dir().join(format!("mxm-player-second-{name}"));
    let _ = std::fs::remove_dir_all(&second);
    std::fs::create_dir_all(&second).ok()?;
    std::fs::copy(&source, second.join("mxm-mono-01.clap")).ok()?;

    Some((bundled, second))
}

#[test]
fn the_same_plugin_in_two_places_is_listed_twice_with_different_locations() {
    // The defect: two identical rows, no way to tell which copy would be loaded — so clicking the
    // wrong one silently tested a build that no longer existed.
    let Some((bundled, second)) = stage_second_copy("locations") else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut app = AppHarness::new("duplicates", vec![bundled, second]);
    app.harness.state_mut().rescan();
    app.run();

    let state = app.state();
    let rows: Vec<_> = state
        .found
        .iter()
        .filter(|f| f.id == "dk.mxm.mxm-mono-01")
        .collect();

    assert_eq!(
        rows.len(),
        2,
        "both copies must be listed, not deduplicated"
    );
    assert_ne!(
        rows[0].location, rows[1].location,
        "the locations are the only thing distinguishing them, so they must differ"
    );
    assert!(
        rows.iter().all(|r| !r.location.is_empty()),
        "a row with no location is exactly the defect"
    );
}

#[test]
fn a_symlinked_copy_is_collapsed_rather_than_listed_twice() {
    // The install instructions tell the user to symlink rather than copy, so the same *file*
    // reachable by two paths must not become two rows. Two real copies still do — that is the
    // distinction canonicalisation is there to make.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    // The same directory named twice is the portable stand-in for a symlink: no privileges
    // needed, and it exercises the same canonicalisation.
    let mut app = AppHarness::new("symlink", vec![bundled.clone(), bundled]);
    app.harness.state_mut().rescan();
    app.run();

    let state = app.state();
    let rows = state
        .found
        .iter()
        .filter(|f| f.id == "dk.mxm.mxm-mono-01")
        .count();
    assert_eq!(rows, 1, "one file reached by two paths is one plugin");
}

#[test]
fn the_browser_shows_a_refusal_reason_for_every_unsupported_plugin() {
    // The refusal path is what keeps third-party readiness honest, and it is only honest if the
    // reason reaches the browser.
    let fixtures = app_harness::workspace_root()
        .join("target")
        .join("fixtures");
    if !fixtures.join("mxm-fixtures.clap").exists() {
        eprintln!("skipping: run `cargo xtask fixtures --release`");
        return;
    }

    let mut app = AppHarness::new("refusals", vec![fixtures]);
    app.harness.state_mut().rescan();
    app.run();

    let state = app.state();
    let refused: Vec<_> = state.found.iter().filter(|f| !f.supported).collect();
    assert!(
        !refused.is_empty(),
        "the fixture bundle contains deliberately out-of-envelope plugins"
    );
    for row in &refused {
        let reason = row
            .refusal
            .as_ref()
            .unwrap_or_else(|| panic!("`{}` was refused with no reason shown", row.id));
        assert!(
            reason.len() > 10,
            "`{}` has a reason too terse to act on: {reason}",
            row.id
        );
    }
}

#[test]
fn loading_a_plugin_from_the_browser_starts_audio() {
    // The end-to-end path a person actually takes: rescan, click the row, hear something. Every
    // existing test asserted this as engine calls; none went through the button.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut app = AppHarness::new("load-click", vec![bundled]);
    app.harness.state_mut().rescan();
    app.run();

    assert_eq!(app.state().engine, "Idle");

    // Through the picker, which is how a person loads a plugin now: the browser list moved into a
    // menu in the status bar so it survives the settings panel being collapsed. A menu's contents
    // do not exist until it is open, hence two clicks and two frames.
    app.harness.get_by_label("Load plugin ⏷").click();
    app.run();
    app.harness.get_by_label("mxm-mono-01").click();
    app.run();

    let state = app.state();
    assert_eq!(
        state.engine, "Running",
        "clicking the plugin must load it and start audio"
    );
    assert_eq!(
        state.plugin.as_ref().map(|p| p.id.as_str()),
        Some("dk.mxm.mxm-mono-01")
    );
}

#[test]
fn playing_a_note_shows_it_held_on_the_keyboard() {
    // The keyboard doubles as a monitor, which is only true if held notes reach it.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut app = AppHarness::new("held", vec![bundled.clone()]);
    app.harness.state_mut().rescan();
    app.run();
    app.harness.state_mut().load(
        bundled.join("mxm-mono-01.clap"),
        "dk.mxm.mxm-mono-01".to_owned(),
    );
    app.run();

    app.harness.state_mut().note_on(60, 100.0 / 127.0);
    app.run();
    assert_eq!(app.state().keyboard.held, vec![60]);

    app.harness.state_mut().note_off(60);
    app.run();
    assert!(
        app.state().keyboard.held.is_empty(),
        "a released note must stop being shown as held"
    );
}

#[test]
fn a_button_has_a_border_before_it_is_hovered() {
    // Reported from a screenshot: controls read as plain text and only looked like buttons once
    // the pointer was over them. The player was using egui's defaults, which leave an inactive
    // widget with no visible stroke.
    let mut app = AppHarness::new("border", Vec::new());
    app.run();

    let style = app.harness.ctx.style_of(app.harness.ctx.theme());
    let inactive = style.visuals.widgets.inactive.bg_stroke;

    assert!(
        inactive.width > 0.0,
        "an untouched control must have a border, not only a hovered one"
    );
    assert_ne!(
        inactive.color,
        egui::Color32::TRANSPARENT,
        "the border must be visible"
    );

    // And it must differ from the surface it sits on, or a one-pixel border is invisible anyway.
    assert_ne!(
        inactive.color, style.visuals.widgets.inactive.bg_fill,
        "border and fill must not be the same colour"
    );
}

#[test]
fn hovering_strengthens_a_border_rather_than_creating_one() {
    let mut app = AppHarness::new("border-hover", Vec::new());
    app.run();

    let style = app.harness.ctx.style_of(app.harness.ctx.theme());
    let inactive = style.visuals.widgets.inactive.bg_stroke;
    let hovered = style.visuals.widgets.hovered.bg_stroke;

    assert!(inactive.width > 0.0 && hovered.width > 0.0);
    assert_ne!(
        inactive.color, hovered.color,
        "hover should still be visible as a change"
    );
}

#[test]
fn the_browser_edge_never_reaches_into_the_keyboard() {
    // The keyboard owns the whole bottom edge. The browser column's content has a floor — a
    // 280px list, a 120px log and the device sections — and in a short window that floor is
    // taller than the panel's rect, so the panel grew past it and drew its edge straight down
    // through the keys. Reported from a screenshot after dragging the window shorter.
    //
    // A drag-resize is a sequence of heights, one frame each, so this asserts a *single* frame
    // per height. Settling for several frames afterwards would hide it.
    let mut app = AppHarness::new("browser-edge", Vec::new());
    app.run();

    for height in [1000.0f32, 880.0, 760.0, 640.0, 580.0, 520.0] {
        app.harness.set_size(egui::vec2(1200.0, height));
        app.harness.run_steps(1);

        let keyboard_top = height - KEYBOARD_HEIGHT;
        let Some(edge) = tallest_vertical_line(&app) else {
            continue;
        };
        assert!(
            edge.bottom() <= keyboard_top,
            "at {height}px tall the browser edge reaches {:.0}, {:.0}px into a keyboard that              starts at {keyboard_top}",
            edge.bottom(),
            edge.bottom() - keyboard_top
        );
    }
}

/// The keyboard's height, mirrored from `ui::KEYBOARD_HEIGHT`, which is private.
const KEYBOARD_HEIGHT: f32 = 140.0;

/// The longest vertical line the frame painted, which is the browser's edge.
fn tallest_vertical_line(app: &AppHarness) -> Option<egui::Rect> {
    fn walk(shape: &egui::Shape, out: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::LineSegment { points, .. } => {
                out.push(egui::Rect::from_two_pos(points[0], points[1]));
            }
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let mut lines = Vec::new();
    for clipped in &app.harness.output().shapes {
        walk(&clipped.shape, &mut lines);
    }
    lines
        .into_iter()
        .filter(|r| r.width() <= 4.0 && r.height() > 40.0)
        .max_by(|a, b| a.height().partial_cmp(&b.height()).unwrap())
}

#[test]
fn a_saved_midi_selection_is_restored_into_the_engine_not_only_into_the_ticks() {
    // Reported as "the player does not react to my MIDI keyboard" with the port's box ticked.
    // The box is drawn from `settings`, but `midi_controls` only calls `set_midi_inputs` when
    // the selection *changes* — so on startup the tick was restored and the port was not. The
    // engine had nothing to open, and unticking and re-ticking was the only way to get sound.
    //
    // Proven by opening the port from outside the player while it ran and the box was ticked:
    // Windows MIDI inputs are exclusive, so a successful open meant the player never had it.
    let dir = std::env::temp_dir().join("mxm-player-ui-midi-restore");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let settings_path = dir.join("settings.json");

    let saved = mxm_player::settings::Settings {
        midi_inputs: vec!["a keyboard".to_owned(), "another keyboard".to_owned()],
        midi_output: Some("a synth".to_owned()),
        ..Default::default()
    };
    saved
        .save(&settings_path)
        .expect("the settings should save");

    let config = mxm_player::config::PlayerConfig::sandboxed(
        &dir,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    )
    .with_search_paths(Vec::new());
    let mut app = mxm_player::ui::PlayerApp::with_config(config);

    assert_eq!(
        app.engine_mut().desired_midi_inputs(),
        ["a keyboard", "another keyboard"],
        "a restored tick must describe a port the engine has actually been asked to open"
    );
    assert_eq!(
        app.engine_mut().desired_midi_output(),
        Some("a synth"),
        "the saved MIDI output is restored the same way"
    );
}

#[test]
fn a_note_played_on_a_midi_keyboard_lights_the_key_it_sounds() {
    // `draw_keys` promises "held notes are highlighted, including ones arriving over MIDI, so the
    // keyboard doubles as a monitor". It did not: `held` is only what the GUI itself originated,
    // and a MIDI note goes from the MIDI callback straight into the audio thread's queue without
    // ever passing through it. Reported as sound playing fine while no key changed colour.
    //
    // Driven through the host state rather than a real port, so it runs on a machine with no
    // MIDI hardware — that publication *is* the seam the callback writes to.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut app = AppHarness::new("midi-monitor", vec![bundled.clone()]);
    app.harness.state_mut().rescan();
    app.run();
    app.harness.state_mut().load(
        bundled.join("mxm-mono-01.clap"),
        "dk.mxm.mxm-mono-01".to_owned(),
    );
    app.run();

    assert!(app.state().keyboard.held.is_empty());

    let shared = app.harness.state_mut().engine_mut().shared().clone();
    shared.set_midi_sounding(64, true);
    app.run();
    assert_eq!(
        app.state().keyboard.held,
        vec![64],
        "a note held on a MIDI keyboard must show on the on-screen keyboard"
    );

    // And it must be released again, or the key stays lit with nothing left to clear it.
    shared.set_midi_sounding(64, false);
    app.run();
    assert!(
        app.state().keyboard.held.is_empty(),
        "the key must go out when the MIDI note is released"
    );

    // A port closing takes its notes with it.
    shared.set_midi_sounding(70, true);
    app.run();
    assert_eq!(app.state().keyboard.held, vec![70]);
    shared.clear_midi_sounding();
    app.run();
    assert!(
        app.state().keyboard.held.is_empty(),
        "a note held as its port goes away has no release coming and must not stay lit"
    );
}

#[test]
fn hovering_a_top_bar_toggle_moves_nothing_beside_it() {
    // **Reported**: hovering *Parameters* or *Settings* shifted everything to their left by a few
    // pixels. The row is laid out right to left, so a control that grows when the pointer arrives
    // pushes its neighbours leftward — the same defect as the tempo field, one bar up.
    //
    // Measured on a neighbour rather than on the control, because a test that watched the control
    // itself would pass while the row still jumped.
    let mut harness = AppHarness::new("hover-shift", Vec::new());
    harness.run();

    let before = harness.harness.get_by_label("Settings").rect();
    let editor_before = harness.harness.get_by_label("Show editor").rect().min.x;

    harness.harness.get_by_label("Parameters").hover();
    harness.run();

    let after = harness.harness.get_by_label("Settings").rect();
    let editor_after = harness.harness.get_by_label("Show editor").rect().min.x;

    assert!(
        (before.min.x - after.min.x).abs() < 0.5,
        "Settings moved from {} to {} when Parameters was hovered",
        before.min.x,
        after.min.x
    );
    assert!(
        (editor_before - editor_after).abs() < 0.5,
        "the editor button moved from {editor_before} to {editor_after} when Parameters was hovered"
    );
}

#[test]
fn an_idle_player_keeps_asking_for_frames_so_midi_can_still_reach_a_step() {
    // `a_midi_keyboard_enters_notes_into_a_selected_step` drives frames itself, so it passes
    // whether or not the app ever *asks* for one. The real player only runs `logic` when eframe
    // draws, and eframe draws reactively — so when the repaint request was narrowed to
    // playing-or-editor-visible, a stopped player with no editor open asked for no frames,
    // `service()` never ran, and a MIDI press never reached the selected step.
    //
    // It broke MIDI alone because the on-screen and computer keyboards write the step from inside
    // the input event that woke the frame. MIDI arrives on the backend's own thread and needs a
    // frame it did not cause.
    let mut app = AppHarness::new("idle-repaint", Vec::new());
    app.run();

    assert_eq!(
        app.app().sequencer_state().transport,
        Transport::Stopped,
        "the premise: nothing is playing"
    );
    assert!(
        !app.app().engine_mut().editor_state().visible,
        "the premise: no editor is shown"
    );
    assert!(
        app.harness.ctx.has_requested_repaint(),
        "an idle player must still ask for a frame, or it stops servicing the engine and a MIDI          keyboard cannot enter a note into a selected step"
    );
}

#[test]
fn a_midi_keyboard_enters_notes_into_a_selected_step() {
    // "Click a step to edit it with the keyboard" — and the README calls MIDI keyboards a
    // first-class input path. It was not one here: `note_on` writes into the selected step, and
    // a MIDI note never reaches `note_on`. It is stamped in the MIDI callback and queued for the
    // audio thread, so selecting a step and playing the hardware toggled nothing.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut app = AppHarness::new("midi-step-entry", vec![bundled.clone()]);
    app.harness.state_mut().rescan();
    app.run();
    app.harness.state_mut().load(
        bundled.join("mxm-mono-01.clap"),
        "dk.mxm.mxm-mono-01".to_owned(),
    );
    app.run();

    let shared = app.harness.state_mut().engine_mut().shared().clone();

    // Nothing selected: a press must not write into the pattern.
    shared.set_midi_sounding(60, true);
    shared.set_midi_sounding(60, false);
    app.run();
    assert!(
        app.app().pattern().is_empty(),
        "with no step selected a MIDI note must only play, never edit"
    );

    app.harness.state_mut().select_step(2);
    app.run();

    shared.set_midi_sounding(60, true);
    app.run();
    assert_eq!(
        app.state().sequencer.steps[2],
        vec!["C3"],
        "a MIDI note played while a step is selected must be entered into it"
    );

    // Toggling: the same note again takes it back out.
    shared.set_midi_sounding(60, false);
    app.run();
    shared.set_midi_sounding(60, true);
    app.run();
    assert!(
        app.state().sequencer.steps[2].is_empty(),
        "playing the same note again must toggle it back off"
    );

    // A key struck and released inside one turn still lands: the press is latched, not sampled.
    shared.set_midi_sounding(67, true);
    shared.set_midi_sounding(67, false);
    app.run();
    assert_eq!(
        app.state().sequencer.steps[2],
        vec!["G3"],
        "a note released before the GUI's next turn must still have been entered"
    );
}

#[test]
fn a_midi_port_that_cannot_be_opened_says_why_rather_than_blaming_the_limit() {
    // Every failure was reported as "the input limit was reached", including failures that had
    // nothing to do with the limit — a port held by another application, or a device unplugged
    // since the selection was saved. The player's own contract is that a refusal without a reason
    // is a bug; a refusal with an invented one is worse, because it sends the reader somewhere
    // there is nothing to find.
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let dir = std::env::temp_dir().join("mxm-player-ui-midi-refusal");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");

    // A port that certainly is not there, which is the "device went away" case.
    let missing = "no such MIDI keyboard";
    let saved = mxm_player::settings::Settings {
        midi_inputs: vec![missing.to_owned()],
        ..Default::default()
    };
    saved
        .save(&dir.join("settings.json"))
        .expect("the settings should save");

    let config = mxm_player::config::PlayerConfig::sandboxed(
        &dir,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    )
    .with_search_paths(vec![bundled.clone()]);
    let mut app = mxm_player::ui::PlayerApp::with_config(config);
    app.load(
        bundled.join("mxm-mono-01.clap"),
        "dk.mxm.mxm-mono-01".to_owned(),
    );

    let refused = app.engine_mut().refused_midi_inputs().to_vec();
    assert_eq!(
        refused.iter().map(|r| r.port.as_str()).collect::<Vec<_>>(),
        [missing],
        "a port that could not be opened must still be listed"
    );

    let reason = &refused[0].reason;
    assert!(
        reason.contains(missing),
        "the reason should name the port it is about: {reason}"
    );
    assert!(
        !reason.contains("limit"),
        "a missing port is not a limit problem, and must not be reported as one: {reason}"
    );
}

/// The settle path discards **only** the outputs a full requery supersedes.
///
/// It used to discard the whole drain — `for _ in self.engine.drain_plugin_output() {}` — and the
/// audio thread sets the settle flag whenever a `SequencerParam` push is refused, which is what
/// happens once the GUI queue fills from an output-sink overflow. The `TrackingInvalidated` that
/// same overflow queued went into the discard with everything else, so the recovery that would
/// have closed the stranded gestures never ran, nothing cleared the condition, and the branch
/// requeried every parameter every frame for the rest of the session.
#[test]
fn a_settling_requery_keeps_the_outputs_it_does_not_supersede() {
    use mxm_player::engine::PluginOutput;
    use mxm_player::sequencer::locks::LockKey;

    let survives = mxm_player::ui::PlayerApp::survives_a_requery;

    // Superseded by the requery: the reading it would overwrite is the authoritative one.
    assert!(
        !survives(&PluginOutput::ParamValue {
            param_id: 7,
            value: 0.5
        }),
        "a queued value must not outlive the requery that supersedes it"
    );
    assert!(
        !survives(&PluginOutput::SequencerParam {
            param_id: LockKey::source(7),
            value: 0.25
        }),
        "a sequencer offset is superseded the same way"
    );

    // Not superseded: each carries work a requery does not do.
    assert!(
        survives(&PluginOutput::TrackingInvalidated),
        "dropping this is what latched the condition it exists to clear"
    );
    assert!(
        survives(&PluginOutput::GestureEnd { param_id: 7 }),
        "a gesture that never closes is a knob the host thinks is still being held"
    );
    assert!(
        survives(&PluginOutput::GestureBegin { param_id: 7 }),
        "its begin travels with it"
    );
    assert!(
        survives(&PluginOutput::MappedControlChange {
            controller: 74,
            value: 100
        }),
        "a claimed control change still has to land"
    );
}
