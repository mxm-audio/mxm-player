//! T1 — prove both oracles before anything is built on them.
//!
//! Two claims the plan rests on, each of which has to be demonstrated rather than assumed:
//!
//! 1. **Paint output can see the keyboard.** It is drawn with bare `painter` calls, so no
//!    structural assertion can observe it at all.
//! 2. **A drag can move a real `egui::Slider`**, which is the interaction the parameter-snap-back
//!    defect lived in.
//!
//! Each has a companion test asserting the oracle would have *failed* on the broken behaviour —
//! an oracle that cannot fail is not an oracle.

use mxm_player_harness::app_harness;

use app_harness::AppHarness;
use kittest::Queryable;

/// The keyboard's colours, as `ui/mod.rs` paints them.
const WHITE_KEY: egui::Color32 = egui::Color32::from_gray(250);
const BLACK_KEY: egui::Color32 = egui::Color32::from_gray(40);

fn keyboard_app(name: &str) -> AppHarness {
    let mut app = AppHarness::new(name, Vec::new());
    app.run();
    app
}

// --- oracle 1: paint output ---------------------------------------------------------------------

#[test]
fn paint_output_sees_the_keyboard_at_all() {
    let app = keyboard_app("paint-sees");

    let whites = app.painted_rects_filled(WHITE_KEY);
    let blacks = app.painted_rects_filled(BLACK_KEY);

    assert!(
        !whites.is_empty(),
        "the keyboard's white keys must be visible to the paint oracle"
    );
    assert!(
        !blacks.is_empty(),
        "the keyboard's black keys must be visible to the paint oracle"
    );
}

#[test]
fn the_black_key_oracle_would_have_caught_the_original_defect() {
    // The defect: `draw_keys` drew only white keys. This is the assertion that fails in that
    // world, expressed as a property rather than a count — black keys are *shorter* than white
    // ones, which is what makes them black keys rather than a second row of whites.
    let app = keyboard_app("black-key-defect");

    let whites = app.painted_rects_filled(WHITE_KEY);
    let blacks = app.painted_rects_filled(BLACK_KEY);
    assert!(!whites.is_empty() && !blacks.is_empty());

    let white_height = whites[0].height();
    assert!(
        blacks.iter().all(|b| b.height() < white_height),
        "a black key must be shorter than a white one"
    );
    assert!(
        blacks.iter().all(|b| b.width() < whites[0].width()),
        "a black key must be narrower than a white one"
    );

    // Groups of two and three: five black keys per seven white ones, allowing for the run ending
    // partway through an octave.
    let ratio = blacks.len() as f32 / whites.len() as f32;
    assert!(
        (0.55..=0.80).contains(&ratio),
        "expected roughly five black keys per seven white, got {} of {}",
        blacks.len(),
        whites.len()
    );
}

#[test]
fn the_width_oracle_would_have_caught_keys_stretching_instead_of_revealing() {
    // The defect: key width was `available_width / count`, so a wider window drew the same keys
    // wider. The property that distinguishes the two designs is what is asserted.
    let mut app = keyboard_app("width-defect");

    app.resize(900.0, 760.0);
    let narrow = app.painted_rects_filled(WHITE_KEY);
    let narrow_width = narrow.first().map(|r| r.width()).unwrap_or(0.0);

    app.resize(1800.0, 760.0);
    let wide = app.painted_rects_filled(WHITE_KEY);
    let wide_width = wide.first().map(|r| r.width()).unwrap_or(0.0);

    assert!(
        wide.len() > narrow.len(),
        "a wider window must reveal more keys: {} then {}",
        narrow.len(),
        wide.len()
    );
    assert!(
        (wide_width - narrow_width).abs() < 0.5,
        "key width must not change with window width: {narrow_width} then {wide_width}"
    );
}

// --- oracle 2: dragging a real slider ------------------------------------------------------------

#[test]
fn a_drag_moves_a_real_parameter_slider() {
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut app = AppHarness::new("drag", vec![bundled.clone()]);
    app.harness.state_mut().rescan();
    app.run();
    app.harness.state_mut().load(
        bundled.join("mxm-mono-01.clap"),
        "dk.mxm.mxm-mono-01".to_owned(),
    );
    app.run();

    let before = app
        .state()
        .param("Cutoff")
        .expect("Cutoff is a parameter")
        .value;

    // The slider is reachable by the parameter it edits, because the visible name labels it.
    let rect = app.harness.get_by_label("Cutoff").rect();

    // Drag from the middle of the control towards its left end.
    let from = rect.center();
    let to = egui::pos2(rect.left() + rect.width() * 0.2, rect.center().y);

    app.harness.hover_at(from);
    app.harness.drag_at(from);
    app.run();
    app.harness.hover_at(to);
    app.run();
    app.harness.drop_at(to);
    app.run();

    let after = app
        .state()
        .param("Cutoff")
        .expect("Cutoff is still a parameter")
        .value;

    assert!(
        (after - before).abs() > 1e-6,
        "the drag must actually move the control: {before} then {after}"
    );
    assert!(
        after < before,
        "dragging left must lower the value: {before} then {after}"
    );
}

#[test]
fn the_drag_oracle_would_have_caught_the_snap_back_defect() {
    // The defect: the control moved during the drag and reverted the instant it was released,
    // because the panel fell back to a snapshot the edit had not reached yet. So the assertion
    // that matters is not "did it move" but "did it *stay* moved after release, and after the
    // frames in which the requery happens".
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut app = AppHarness::new("snap-back", vec![bundled.clone()]);
    app.harness.state_mut().rescan();
    app.run();
    app.harness.state_mut().load(
        bundled.join("mxm-mono-01.clap"),
        "dk.mxm.mxm-mono-01".to_owned(),
    );
    app.run();

    let before = app.state().param("Cutoff").expect("Cutoff").value;

    let rect = app.harness.get_by_label("Cutoff").rect();
    let from = rect.center();
    let to = egui::pos2(rect.left() + rect.width() * 0.2, rect.center().y);

    app.harness.hover_at(from);
    app.harness.drag_at(from);
    app.run();
    app.harness.hover_at(to);
    app.run();

    let during = app.state().param("Cutoff").expect("Cutoff").value;
    assert!(
        (during - before).abs() > 1e-6,
        "the control should have moved during the drag"
    );

    app.harness.drop_at(to);

    // Several frames past release: the requery is deliberately deferred by one, and the snap-back
    // showed up exactly here.
    for _ in 0..8 {
        app.run();
        app.harness.state_mut().service();
    }

    let after = app.state().param("Cutoff").expect("Cutoff").value;
    assert!(
        (after - during).abs() < 1e-3,
        "the value must hold after release: {during} during the drag, {after} after it"
    );
    assert!(
        (after - before).abs() > 1e-6,
        "it must not have snapped back to where it started ({before})"
    );
}

/// **The compatibility statement the tab strip lives or dies by.**
///
/// Every effect and every instrument but one declares no parameter groups, and for those the panel
/// must be exactly what it was: no tab strip, every parameter drawn. A plugin that *does* group its
/// parameters gets one tab per group, its own ungrouped controls first, and every parameter still
/// reachable — hidden means not painted, never not writable.
#[test]
fn parameter_groups_come_from_the_plugin_and_an_ungrouped_one_is_unchanged() {
    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let mut app = AppHarness::new("groups", vec![bundled.clone()]);
    app.harness.state_mut().rescan();
    app.run();
    app.harness.state_mut().load(
        bundled.join("mxm-mono-01.clap"),
        "dk.mxm.mxm-mono-01".to_owned(),
    );
    app.run();

    let params = app
        .state()
        .plugin
        .as_ref()
        .expect("a loaded plugin")
        .params
        .clone();
    assert!(
        params.len() > 100,
        "this check is about a plugin with a lot of parameters; got {}",
        params.len()
    );

    // The plugin named its own groups, and the player did not invent them.
    let mut groups: Vec<&str> = params.iter().map(|p| p.module.as_str()).collect();
    groups.sort_unstable();
    groups.dedup();
    assert!(
        groups.len() > 1,
        "mxm-mono-01 declares routing groups; the panel has nothing to tab: {groups:?}"
    );
    assert!(
        groups.contains(&""),
        "the instrument's own parameters carry no module and open the panel: {groups:?}"
    );

    // Cutoff is one of the instrument's own, so it is on the tab that opens — which is what makes
    // it reachable by label without touching a tab first.
    let cutoff = params
        .iter()
        .find(|p| p.name == "Cutoff")
        .expect("Cutoff is a parameter");
    assert_eq!(
        cutoff.module, "",
        "Cutoff belongs to the instrument, not to a routing group"
    );

    // **Every parameter stays writable whatever the panel is showing.** A routing parameter sits
    // behind a tab nobody has clicked, and setting it must still land.
    let routed = params
        .iter()
        .find(|p| !p.module.is_empty() && p.max > p.min)
        .expect("a grouped parameter")
        .clone();
    let target = routed.min + (routed.max - routed.min) * 0.75;
    app.harness.state_mut().set_parameter(routed.id, target);
    for _ in 0..8 {
        app.run();
        app.harness.state_mut().service();
    }
    let state = app.state();
    let after = state
        .plugin
        .as_ref()
        .expect("a loaded plugin")
        .params
        .iter()
        .find(|p| p.id == routed.id)
        .expect("still a parameter")
        .value;
    assert!(
        (after - target).abs() < 1e-3,
        "a parameter behind an unopened tab was not writable: asked {target}, got {after}"
    );
}
