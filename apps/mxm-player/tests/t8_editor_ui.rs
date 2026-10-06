//! Layer 2 for the editor toggle and the collapse — the real `PlayerApp`, driven the way a person
//! drives it.
//!
//! `t7_editor.rs` tests the engine's hosting API directly. That is not the same thing, and writing
//! only that was a mistake: the button wiring, the collapse toggles and the panels they hide are UI,
//! and this harness exists precisely so they do not have to be checked by hand.
//!
//! What this layer still cannot see is the one defect that actually shipped: the player and a
//! plugin editor both render through OpenGL in one process, and `kittest` draws into no window at
//! all. That needs a real display — `docs/known-issues.md` in mxm-kit records it.

use mxm_player_harness::app_harness;

use app_harness::AppHarness;
use kittest::Queryable;

/// Every control that must survive collapsing, by its accessible label.
///
/// The user's instruction, and the reason there is no "collapse everything": the status readings,
/// the transport and the keyboard are the performance surface, and only the testing surface folds
/// away.
const NEVER_COLLAPSES: &[&str] = &[
    // The status bar's engine reading, the transport's play toggle, and the keyboard's panic:
    // one from each of the three strips that never fold away.
    //
    // The glyph is part of the label, and pinning it is deliberate -- changing it silently would
    // break every test that finds the button, and this is where that surfaces.
    "idle", "▶ Play", "Panic",
];

fn player() -> AppHarness {
    let mut app = AppHarness::new("editor-ui", Vec::new());
    app.run();
    app
}

#[test]
fn the_editor_toggle_is_offered_before_a_plugin_is_loaded() {
    // It is a host function, not a plugin feature: the control exists, and pressing it explains
    // why it cannot do anything yet. Hiding the control instead would leave the user hunting for
    // something that is not there.
    let app = player();
    app.harness.get_by_label("Show editor");
}

#[test]
fn asking_for_an_editor_with_no_plugin_says_why() {
    // `src/envelope.rs`'s contract: a refusal without a reason is a bug.
    let mut app = player();
    app.harness.get_by_label("Show editor").click();
    app.run();
    app.harness.get_by_label("editor unavailable");
}

#[test]
fn both_panels_collapse_independently() {
    let mut app = player();

    // Both start expanded, so both toggles read as selected.
    app.harness.get_by_label("Parameters").click();
    app.run();
    app.harness.get_by_label("Settings").click();
    app.run();

    // Still there to switch back on — the controls live in the status bar, which never collapses,
    // because a control for un-collapsing has to survive being collapsed.
    app.harness.get_by_label("Parameters");
    app.harness.get_by_label("Settings");
}

#[test]
fn collapsing_never_takes_the_transport_or_the_keyboard_with_it() {
    // The user's instruction, as an assertion. A future "tidy-up" that folded the keyboard away
    // would fail here rather than in their hands.
    let mut app = player();
    for label in NEVER_COLLAPSES {
        app.harness.get_by_label(label);
    }

    app.harness.get_by_label("Parameters").click();
    app.run();
    app.harness.get_by_label("Settings").click();
    app.run();

    for label in NEVER_COLLAPSES {
        app.harness.get_by_label(label);
    }
}

#[test]
fn a_still_open_settings_panel_is_given_room_when_the_parameters_collapse() {
    // Reported from a screenshot: a clipped strip of the audio section between the status bar and
    // the transport, reading as dead space. It was the settings panel, squeezed — the two panels
    // collapse **independently**, but the window height was sized as though everything collapsible
    // had collapsed, keyed on the parameter panel alone.
    //
    // **Asserted on the height, and the first attempt at this test was worthless.** Looking for the
    // panel's controls in the tree passes against the defect: AccessKit reports a clipped widget as
    // present, exactly as it reports a scrolled-away row as present. The harness does not honour
    // viewport commands either, so the window never actually resizes here. The height the player
    // *asks* for is the only observable that moves.
    let mut app = player();
    app.harness.get_by_label("Parameters").click();
    app.run();

    let with_panel = app.harness.state_mut().collapsed_height();

    app.harness.get_by_label("Settings").click();
    app.run();
    let without_panel = app.harness.state_mut().collapsed_height();

    assert!(
        with_panel > without_panel,
        "a still-open settings panel must add height to the collapsed window, or it is squeezed          into whatever is left between the status bar and the sequencer: {with_panel} vs          {without_panel}"
    );
    assert!(
        with_panel - without_panel >= 200.0,
        "the room given has to be enough for the panel to be worth showing, not a strip:          {with_panel} vs {without_panel}"
    );
}

/// A control that exists **only** inside the settings panel, used to tell "gone" from "narrower".
///
/// This was `Rescan` until the plugin browser became a menu in the status bar — which put `Rescan`
/// outside the panel and, worse, inside a menu that is not in the tree until it is opened. Picking a
/// sentinel that is merely *usually* in the panel is how these tests stop testing collapse and start
/// testing something else.
const PANEL_ONLY: &str = "Refresh ports";

#[test]
fn collapsing_the_settings_panel_removes_it() {
    // The panel is gone, not merely narrower.
    let mut app = player();
    app.harness.get_by_label(PANEL_ONLY);

    app.harness.get_by_label("Settings").click();
    app.run();

    assert!(
        app.harness.query_by_label(PANEL_ONLY).is_none(),
        "collapsing the settings panel should remove it, not just shrink it"
    );
}

#[test]
fn the_collapse_survives_being_switched_back_on() {
    // Collapsing resizes the window, so the way back has to work. A one-way door would be worse
    // than no door.
    let mut app = player();
    app.harness.get_by_label("Settings").click();
    app.run();
    assert!(app.harness.query_by_label(PANEL_ONLY).is_none());

    app.harness.get_by_label("Settings").click();
    app.run();
    app.harness.get_by_label(PANEL_ONLY);
}

#[test]
fn the_keyboard_is_still_painted_when_everything_collapsible_is_collapsed() {
    // The keyboard allocates one interaction region and paints its keys directly, so no structural
    // assertion can see them — `painted_rects` is the only oracle that can. This is the check that
    // "the keyboard never collapses" means keys on screen, not just a label in the tree.
    let mut app = player();
    let before = app.painted_rects().len();

    app.harness.get_by_label("Parameters").click();
    app.run();
    app.harness.get_by_label("Settings").click();
    app.run();

    let after = app.painted_rects().len();
    assert!(
        after > 30,
        "expected the keyboard's keys to still be painted; only {after} rects were drawn \
         (was {before} expanded)"
    );
}
