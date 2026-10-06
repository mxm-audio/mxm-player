//! The plugin-GUI hosting contract, headlessly.
//!
//! What these prove is the part that is invisible by clicking: that mxm-mono-01 **advertises a floating
//! editor at all**, and that the player's refusals carry reasons. Opening a real window is not done
//! here — that needs a display and would flash a window through every test run — so the interactive
//! half is `cargo run -p mxm-player` and the M4b sign-off records it.
//!
//! Requires `cargo xtask bundle mxm-mono-01 --release`; skips with a message rather than failing if the
//! bundle is missing, because a missing build is not a hosting bug.

use mxm_player_harness::app_harness;

use mxm_player::engine::Engine;
use mxm_player::engine::editor::{EditorRefusal, EditorState, EditorTarget, Ownership};

fn engine_with_mxm_mono_01() -> Option<Engine> {
    let bundled = app_harness::bundled_dir()?;
    let mut engine = Engine::new();
    engine
        .load(&bundled.join("mxm-mono-01.clap"), "dk.mxm.mxm-mono-01")
        .ok()?;
    Some(engine)
}

#[test]
fn mxm_mono_01_advertises_a_floating_editor() {
    // **This is the regression test for the vendored nice-plug patch.** Upstream refuses every
    // floating configuration — `if is_floating { return false }` — and with that refusal in place
    // the player can never show mxm-mono-01's interface, on any platform. A refresh of `vendor/nice-plug`
    // that dropped the patch would fail here rather than in a user's hands.
    let Some(mut engine) = engine_with_mxm_mono_01() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    assert!(
        engine.editor_available().is_ok(),
        "mxm-mono-01 should expose clap.gui"
    );
    assert!(
        engine.editor_floating_supported(),
        "mxm-mono-01 should support a floating editor; check the MXM PATCH in \
         vendor/nice-plug/src/wrapper/clap/wrapper.rs"
    );
}

#[test]
fn mxm_fx_curve_advertises_its_production_editor_through_the_effect_path() {
    let Some(bundled) = app_harness::any_bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-fx-curve --release`");
        return;
    };
    let path = bundled.join("mxm-fx-curve.clap");
    if !path.exists() {
        eprintln!("skipping: run `cargo xtask bundle mxm-fx-curve --release`");
        return;
    }
    let mut engine = Engine::new();
    engine
        .add_fx(&path, "dk.mxm.mxm-fx-curve")
        .expect("load curve effect");
    assert!(
        engine.fx_editor_floating_supported(0),
        "mxm-fx-curve should expose its production editor through the floating effect path"
    );
}

/// Opens the curve editor through the exact floating effect path used by the player. Kept ignored
/// because it creates a native window; run it when the editor's construction or layout changes.
#[test]
#[ignore = "creates a real window; needs a display"]
fn mxm_fx_curve_editor_opens_closes_and_reopens() {
    let Some(bundled) = app_harness::any_bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-fx-curve --release`");
        return;
    };
    let path = bundled.join("mxm-fx-curve.clap");
    if !path.exists() {
        eprintln!("skipping: run `cargo xtask bundle mxm-fx-curve --release`");
        return;
    }
    let mut engine = Engine::new();
    engine
        .add_fx(&path, "dk.mxm.mxm-fx-curve")
        .expect("load curve effect");
    for attempt in 1..=2 {
        let state = engine
            .open_fx_editor(0, None)
            .unwrap_or_else(|why| panic!("attempt {attempt}: {}", why.message()));
        assert!(state.open, "attempt {attempt}: curve editor did not open");
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            engine.service_editor();
        }
        engine.close_editor_for(EditorTarget::Fx(0));
        assert!(
            !engine.editor_state_for(EditorTarget::Fx(0)).open,
            "attempt {attempt}: curve editor did not close"
        );
    }
}

#[test]
fn a_plugin_with_no_gui_is_refused_with_a_reason() {
    // `src/envelope.rs` sets the contract for the whole player: everything outside the supported
    // envelope is refused **with the reason shown**, and a refusal without a reason is a bug. The
    // editor is one more capability under that rule, not an exception to it.
    let engine = Engine::new();
    let refusal = engine.editor_available().unwrap_err();
    assert_eq!(refusal, EditorRefusal::NoPlugin);
    assert!(!refusal.message().is_empty());
}

#[test]
fn closing_an_editor_that_is_not_open_is_harmless() {
    // `close_editor` is the only place `destroy` is called, and it runs on the unload path, the
    // user-closed path and the toggle path. Being a no-op when nothing is open is what keeps
    // "exactly one destroy per successful create" true without any caller having to check.
    let mut engine = Engine::new();
    engine.close_editor();
    engine.close_editor();
    assert_eq!(engine.editor_state(), EditorState::default());
}

#[test]
fn servicing_the_editor_with_none_open_does_nothing() {
    // Called every frame from `logic`, including the whole time no plugin is loaded.
    let mut engine = Engine::new();
    for _ in 0..8 {
        engine.service_editor();
    }
    assert!(!engine.editor_state().open);
}

#[test]
fn a_headless_player_never_claims_ownership() {
    // There is no window to own anything, so `HostWindow::from_handle` yields `None` and the
    // attempt must decline rather than adopt some unrelated window of ours.
    let Some(mut engine) = engine_with_mxm_mono_01() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };
    assert_eq!(engine.editor_state().owned, Ownership::NotOpen);
    engine.close_editor();
}

#[test]
fn unloading_a_plugin_closes_its_editor_first() {
    // The editor is a window the *plugin* owns. Dropping the instance out from under it would
    // orphan that window, so `load` closes it before anything else — including when the new load
    // fails.
    let Some(mut engine) = engine_with_mxm_mono_01() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    let bundled = app_harness::bundled_dir().unwrap();
    let _ = engine.load(&bundled.join("mxm-mono-01.clap"), "dk.mxm.mxm-mono-01");
    assert!(
        !engine.editor_state().open,
        "a reload must not leave the previous editor marked open"
    );
}

/// Opens mxm-mono-01's editor for real, then closes it.
///
/// `#[ignore]` because it creates an actual OS window: it would flash through every test run, and
/// it needs a display. Run it deliberately:
///
/// ```text
/// cargo test -p mxm-player --test t7_editor -- --ignored --nocapture
/// ```
///
/// What it proves that the headless tests cannot: `create` succeeds against the real plugin,
/// `show` succeeds, and `destroy` is accepted — and then that **a second open succeeds**, which is
/// the case a missed `destroy` acknowledgement breaks and which is invisible on the first attempt.
#[test]
#[ignore = "creates a real window; needs a display"]
fn the_editor_opens_and_reopens() {
    let Some(mut engine) = engine_with_mxm_mono_01() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };

    for attempt in 1..=2 {
        let state = engine
            .open_editor(None)
            .unwrap_or_else(|why| panic!("attempt {attempt}: {}", why.message()));
        assert!(state.open, "attempt {attempt}: editor did not report open");
        assert!(engine.editor_state().open);

        // No host window in a test, so ownership must decline rather than adopt something.
        assert!(
            matches!(state.owned, Ownership::Unowned(_)),
            "attempt {attempt}: claimed ownership with no host window"
        );

        engine.close_editor();
        assert!(!engine.editor_state().open);
    }
}

// --- mxm-poly-06 ---------------------------------------------------------------------------------

/// **Two windows at once**, which is the owner's ruling of 2026-09-04 replacing the day-old
/// one-at-a-time rule. Headless: no window is created, so what is proved here is the engine's
/// bookkeeping — that each target carries its own state, that closing one leaves the others, and
/// that the app bar's own reading stays the source's.
///
/// The window half is `the_editor_opens_and_reopens`'s job and is `#[ignore]`d for its reason.
#[test]
fn each_target_has_its_own_editor_state() {
    use mxm_player::engine::editor::EditorTarget;

    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-01 --release`");
        return;
    };
    let Some(mut engine) = engine_with_mxm_mono_01() else {
        return;
    };
    if engine
        .add_fx(&bundled.join("mxm-chorus-06.clap"), "dk.mxm.mxm-chorus-06")
        .is_err()
    {
        eprintln!("skipping: run `cargo xtask bundle mxm-chorus-06 --release`");
        return;
    }

    // Nothing open: every target reads closed, and the app bar reads the source.
    assert!(!engine.any_editor_open());
    assert!(engine.open_editor_targets().is_empty());
    assert!(!engine.editor_state().open);
    assert!(!engine.editor_state_for(EditorTarget::Fx(0)).open);

    // Closing one that is not open is harmless, and closing the source does not touch the chain.
    engine.close_editor_for(EditorTarget::Fx(0));
    engine.close_editor();
    assert!(!engine.any_editor_open());

    // Both plugins advertise a floating editor, which is what makes two windows possible at all.
    assert!(engine.editor_floating_supported());
    assert!(engine.fx_editor_floating_supported(0));
}

fn engine_with_mxm_poly_06() -> Option<Engine> {
    let bundled = app_harness::bundled_dir()?;
    let bundle = bundled.join("mxm-poly-06.clap");
    if !bundle.exists() {
        return None;
    }
    let mut engine = Engine::new();
    engine.load(&bundle, "dk.mxm.mxm-poly-06").ok()?;
    Some(engine)
}

/// The polysynth advertises a floating editor too — the same regression test for the vendored
/// patch as `mxm_mono_01_advertises_a_floating_editor`, because only a plugin with a test here
/// would catch a refresh dropping the floating-window support.
#[test]
fn mxm_poly_06_advertises_a_floating_editor() {
    let Some(mut engine) = engine_with_mxm_poly_06() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-poly-06 --release`");
        return;
    };
    assert!(
        engine.editor_available().is_ok(),
        "mxm-poly-06 should expose clap.gui"
    );
    assert!(
        engine.editor_floating_supported(),
        "mxm-poly-06 should support a floating editor; check the MXM PATCH in \
         vendor/nice-plug/src/wrapper/clap/wrapper.rs"
    );
}

/// Opens mxm-poly-06's editor for real, then closes it, twice — the same proof as
/// `the_editor_opens_and_reopens`, for the same reason, `#[ignore]`d for the same reason.
///
/// ```text
/// cargo test -p mxm-player --test t7_editor -- --ignored --nocapture
/// ```
#[test]
#[ignore = "creates a real window; needs a display"]
fn mxm_poly_06s_editor_opens_and_reopens() {
    let Some(mut engine) = engine_with_mxm_poly_06() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-poly-06 --release`");
        return;
    };

    for attempt in 1..=2 {
        let state = engine
            .open_editor(None)
            .unwrap_or_else(|why| panic!("attempt {attempt}: {}", why.message()));
        assert!(state.open, "attempt {attempt}: editor did not report open");
        assert!(engine.editor_state().open);
        assert!(
            matches!(state.owned, Ownership::Unowned(_)),
            "attempt {attempt}: claimed ownership with no host window"
        );
        engine.close_editor();
        assert!(!engine.editor_state().open);
    }
}

// --- mxm-mono-00 ---------------------------------------------------------------------------------

fn engine_with_mxm_mono_00() -> Option<Engine> {
    let bundled = app_harness::bundled_dir()?;
    let bundle = bundled.join("mxm-mono-00.clap");
    if !bundle.exists() {
        return None;
    }
    let mut engine = Engine::new();
    engine.load(&bundle, "dk.mxm.mxm-mono-00").ok()?;
    Some(engine)
}

/// The semi-modular advertises a floating editor too — the same regression test for the vendored
/// patch as the siblings', because only a plugin with a test here would catch a refresh dropping
/// the floating-window support.
#[test]
fn mxm_mono_00_advertises_a_floating_editor() {
    let Some(mut engine) = engine_with_mxm_mono_00() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-00 --release`");
        return;
    };
    assert!(
        engine.editor_available().is_ok(),
        "mxm-mono-00 should expose clap.gui"
    );
    assert!(
        engine.editor_floating_supported(),
        "mxm-mono-00 should support a floating editor; check the MXM PATCH in \
         vendor/nice-plug/src/wrapper/clap/wrapper.rs"
    );
}

/// Opens mxm-mono-00's editor for real, then closes it, twice — the same proof as
/// `the_editor_opens_and_reopens`, for the same reason, `#[ignore]`d for the same reason.
///
/// ```text
/// cargo test -p mxm-player --test t7_editor -- --ignored --nocapture
/// ```
#[test]
#[ignore = "creates a real window; needs a display"]
fn mxm_mono_00s_editor_opens_and_reopens() {
    let Some(mut engine) = engine_with_mxm_mono_00() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-00 --release`");
        return;
    };

    for attempt in 1..=2 {
        let state = engine
            .open_editor(None)
            .unwrap_or_else(|why| panic!("attempt {attempt}: {}", why.message()));
        assert!(state.open, "attempt {attempt}: editor did not report open");
        assert!(engine.editor_state().open);
        assert!(
            matches!(state.owned, Ownership::Unowned(_)),
            "attempt {attempt}: claimed ownership with no host window"
        );
        engine.close_editor();
        assert!(!engine.editor_state().open);
    }
}

// --- mxm-mono-02 ---------------------------------------------------------------------------------

fn engine_with_mxm_mono_02() -> Option<Engine> {
    let bundled = app_harness::bundled_dir()?;
    let bundle = bundled.join("mxm-mono-02.clap");
    if !bundle.exists() {
        return None;
    }
    let mut engine = Engine::new();
    engine.load(&bundle, "dk.mxm.mxm-mono-02").ok()?;
    Some(engine)
}

/// The same regression test for the vendored floating-window patch, for the same reason as the
/// siblings': only a plugin with a test here would catch a refresh dropping the support.
#[test]
fn mxm_mono_02_advertises_a_floating_editor() {
    let Some(mut engine) = engine_with_mxm_mono_02() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-02 --release`");
        return;
    };
    assert!(
        engine.editor_available().is_ok(),
        "mxm-mono-02 should expose clap.gui"
    );
    assert!(
        engine.editor_floating_supported(),
        "mxm-mono-02 should support a floating editor; check the MXM PATCH in \
         vendor/nice-plug/src/wrapper/clap/wrapper.rs"
    );
}

/// Opens mxm-mono-02's editor for real, then closes it, twice — the same proof as
/// `the_editor_opens_and_reopens`, for the same reason, `#[ignore]`d for the same reason.
///
/// ```text
/// cargo test -p mxm-player --test t7_editor -- --ignored --nocapture
/// ```
#[test]
#[ignore = "creates a real window; needs a display"]
fn mxm_mono_02s_editor_opens_and_reopens() {
    let Some(mut engine) = engine_with_mxm_mono_02() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-02 --release`");
        return;
    };

    for attempt in 1..=2 {
        let state = engine
            .open_editor(None)
            .unwrap_or_else(|why| panic!("attempt {attempt}: {}", why.message()));
        assert!(state.open, "attempt {attempt}: editor did not report open");
        assert!(engine.editor_state().open);
        assert!(
            matches!(state.owned, Ownership::Unowned(_)),
            "attempt {attempt}: claimed ownership with no host window"
        );
        engine.close_editor();
        assert!(!engine.editor_state().open);
    }
}

// --- mxm-mono-08 ---------------------------------------------------------------------------------

fn engine_with_mxm_mono_08() -> Option<Engine> {
    let bundled = app_harness::any_bundled_dir()?;
    let bundle = bundled.join("mxm-mono-08.clap");
    if !bundle.exists() {
        return None;
    }
    let mut engine = Engine::new();
    engine.load(&bundle, "dk.mxm.mxm-mono-08").ok()?;
    Some(engine)
}

#[test]
fn mxm_mono_08_advertises_a_floating_editor() {
    let Some(mut engine) = engine_with_mxm_mono_08() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-08 --release`");
        return;
    };
    assert!(
        engine.editor_available().is_ok(),
        "mxm-mono-08 should expose clap.gui"
    );
    assert!(
        engine.editor_floating_supported(),
        "mxm-mono-08 should support a floating editor; check the MXM PATCH in \
         vendor/nice-plug/src/wrapper/clap/wrapper.rs"
    );
}

/// Opens the production four-view editor, closes it, and proves that plugin and player state both
/// permit a second open. Kept ignored because it creates a real native window.
#[test]
#[ignore = "creates a real window; needs a display"]
fn mxm_mono_08s_editor_opens_and_reopens() {
    let Some(mut engine) = engine_with_mxm_mono_08() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-mono-08 --release`");
        return;
    };

    for attempt in 1..=2 {
        let state = engine
            .open_editor(None)
            .unwrap_or_else(|why| panic!("attempt {attempt}: {}", why.message()));
        assert!(state.open, "attempt {attempt}: editor did not report open");
        assert!(engine.editor_state().open);
        assert!(
            matches!(state.owned, Ownership::Unowned(_)),
            "attempt {attempt}: claimed ownership with no host window"
        );
        engine.close_editor();
        assert!(!engine.editor_state().open);
    }
}

// --- mxm-para-07 ---------------------------------------------------------------------------------

fn engine_with_mxm_para_07() -> Option<Engine> {
    let bundled = app_harness::any_bundled_dir()?;
    let bundle = bundled.join("mxm-para-07.clap");
    if !bundle.exists() {
        return None;
    }
    let mut engine = Engine::new();
    engine.load(&bundle, "dk.mxm.mxm-para-07").ok()?;
    Some(engine)
}

#[test]
fn mxm_para_07_advertises_a_floating_editor() {
    let Some(mut engine) = engine_with_mxm_para_07() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-para-07 --release`");
        return;
    };
    assert!(
        engine.editor_available().is_ok(),
        "mxm-para-07 should expose clap.gui"
    );
    assert!(
        engine.editor_floating_supported(),
        "mxm-para-07 should support a floating editor; check the MXM PATCH in \
         vendor/nice-plug/src/wrapper/clap/wrapper.rs"
    );
}

/// Deliberately interactive real-window coverage for create/destroy/recreate. Kept ignored in the
/// ordinary suite because it needs a display and flashes an OS window.
#[test]
#[ignore = "creates a real window; needs a display"]
fn mxm_para_07s_editor_opens_closes_and_reopens() {
    let Some(mut engine) = engine_with_mxm_para_07() else {
        eprintln!("skipping: run `cargo xtask bundle mxm-para-07 --release`");
        return;
    };

    for attempt in 1..=2 {
        let state = engine
            .open_editor(None)
            .unwrap_or_else(|why| panic!("attempt {attempt}: {}", why.message()));
        assert!(state.open, "attempt {attempt}: editor did not report open");
        assert!(engine.editor_state().open);
        assert!(
            matches!(state.owned, Ownership::Unowned(_)),
            "attempt {attempt}: claimed ownership with no host window"
        );
        engine.close_editor();
        assert!(!engine.editor_state().open);
    }
}
