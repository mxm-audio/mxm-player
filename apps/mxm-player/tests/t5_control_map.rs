//! One controller across the whole collection.
//!
//! The first test is the requirement itself: **adding an instrument must be adding a file.** If it
//! ever needs a code change here, the design has failed and the rest of this file is decoration.
//!
//! Everything after it is measured through the real path — a control change pushed from a source,
//! merged on the audio thread, claimed by the mask, routed back, mapped, and applied to the
//! plugin. Mapping a CC directly in a test would prove nothing about the route a knob takes.

use mxm_player_harness::app_harness;

use mxm_player::control_map::curve::{self, Range};
use mxm_player::control_map::schema::{self, Curve, InstrumentMap, Layout};
use mxm_player::control_map::{Absorbed, ControlMap, Outcome};
use mxm_player::params::{ParamSet, ParamSnapshot};
use mxm_player::session::Session;
use std::path::PathBuf;

const PLUGIN: &str = "dk.mxm.mxm-mono-01";

/// CC numbers from the shipped standard, restated so a silent change to it fails here.
const CC_CUTOFF: u8 = 74;
const CC_ATTACK: u8 = 73;
const CC_SLOT_1: u8 = 102;
const CC_PAGE_UP: u8 = 111;

fn session(name: &str) -> Option<Session> {
    let dir = app_harness::bundled_dir()?;
    let file = dir.join("mxm-mono-01.clap");
    let mut session = Session::scratch(name, vec![dir]);
    session.load(&file, PLUGIN);
    Some(session)
}

fn param(session: &mut Session, string_id: &str) -> mxm_player::state::ParamState {
    let id = schema::hash_param_id(string_id);
    session
        .state()
        .plugin
        .expect("a plugin should be loaded")
        .params
        .iter()
        .find(|p| p.id == id)
        .cloned()
        .unwrap_or_else(|| panic!("mxm-mono-01 has no parameter `{string_id}`"))
}

/// Turns a knob, then gives the player a frame to see it come back from the audio thread.
fn turn(session: &mut Session, cc: u8, value: u8) {
    session.app().send_control_change(cc, value);
    session.advance_blocks(2).expect("advance");
}

// --- the requirement ----------------------------------------------------------------------------

#[test]
fn a_new_instrument_gets_the_collection_layout_by_adding_a_file_and_nothing_else() {
    // The whole point of the design. This test writes a map for an instrument that does not
    // exist, for parameters this crate has never heard of, and asserts the standard applies to
    // it. Nothing below names mxm-mono-01, and nothing in `src/` needs to change for it to pass.
    let map = InstrumentMap::parse(
        r#"{
            "schema_version": 1,
            "instruments": [{
                "clap_id": "com.example.two-osc",
                "name": "Something With Two Oscillators",
                "params": {
                    "osc1.tune":         "osc-a-tune",
                    "osc2.tune":         "osc-b-tune",
                    "filter.cutoff":     "vcf-freq",
                    "amp_env.attack":    "vca-a",
                    "filter_env.attack": "vcf-a"
                }
            }]
        }"#,
    )
    .expect("a well-formed instrument map");

    let dir = std::env::temp_dir().join("mxm-control-map-new-instrument");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("two-osc.control-map.json");
    std::fs::write(&path, serde_json::to_string(&map).unwrap()).unwrap();

    let mut control_map = ControlMap::shipped();
    control_map.load_instrument_map(&path).expect("loads");

    assert!(control_map.knows_instrument("com.example.two-osc"));

    // The roles mxm-mono-01 leaves empty are filled on this one, and they are the same roles in the
    // same slots. That is what makes one controller work across instruments.
    for role in ["osc2.tune", "filter_env.attack"] {
        assert!(
            control_map.param_for("com.example.two-osc", role).is_some(),
            "role `{role}` should be filled on a two-oscillator instrument"
        );
    }

    // And it resolves to the parameter the file named, not to something guessed from a name.
    assert_eq!(
        control_map.param_for("com.example.two-osc", "filter.cutoff"),
        Some(schema::hash_param_id("vcf-freq"))
    );
}

#[test]
fn an_instrument_map_ships_beside_its_bundle_not_inside_the_player() {
    // Nobody is obliged to install the whole collection, so the player must not be the place an
    // instrument's mapping lives.
    let standard = schema::shipped();
    let text = serde_json::to_string(&standard).unwrap();
    assert!(
        !text.contains("dk.mxm.mxm-mono-01"),
        "the collection standard must not name any particular instrument"
    );
}

#[test]
fn one_broken_instrument_map_does_not_disable_the_controller_for_the_others() {
    let dir = std::env::temp_dir().join("mxm-control-map-broken");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    std::fs::write(dir.join("broken.control-map.json"), "{ not json").unwrap();
    std::fs::write(
        dir.join("good.control-map.json"),
        r#"{"schema_version":1,"instruments":[
            {"clap_id":"com.example.good","name":"Good","params":{"filter.cutoff":"c"}}]}"#,
    )
    .unwrap();

    let mut control_map = ControlMap::shipped();
    let problems = control_map.load_instrument_maps_in(&dir);

    assert_eq!(problems.len(), 1, "the broken one should be reported");
    assert!(
        control_map.knows_instrument("com.example.good"),
        "the good one should still have loaded"
    );
}

#[test]
fn a_map_naming_a_role_that_does_not_exist_is_refused_with_the_typo_named() {
    let dir = std::env::temp_dir().join("mxm-control-map-typo");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("typo.control-map.json"),
        r#"{"schema_version":1,"instruments":[
            {"clap_id":"com.example.typo","name":"Typo","params":{"filter.cutof":"c"}}]}"#,
    )
    .unwrap();

    let mut control_map = ControlMap::shipped();
    let problems = control_map.load_instrument_maps_in(&dir);
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].contains("filter.cutof"),
        "the reason should name the typo: {}",
        problems[0]
    );
}

// --- the standard -------------------------------------------------------------------------------

#[test]
fn the_reserved_ccs_can_never_be_claimed() {
    // 120 and 123 are what the panic machinery uses; 1 is a performance gesture mxm-mono-01 consumes
    // without writing a parameter. If any of these were claimed the worker would stop forwarding
    // them, and a stuck note would have no way out.
    let claimed = ControlMap::shipped().claimed();
    for cc in schema::RESERVED_CCS {
        assert!(!claimed.contains(cc), "CC {cc} must never be claimed");
    }
}

#[test]
fn the_fixed_knobs_are_the_mma_sound_controllers() {
    // Not invented here: a GM2-aware controller already sends these, so the eight fixed knobs work
    // with no configuration at all.
    let claimed = ControlMap::shipped().claimed();
    for cc in [74u8, 71, 73, 75, 72, 5, 76, 77] {
        assert!(claimed.contains(cc), "CC {cc} should be a fixed knob");
    }
}

#[test]
fn the_m32_factory_template_drives_the_same_eight_knobs() {
    // NI's Komplete Kontrol M32 and A-series ship a MIDI template whose first knob page sends
    // CC 14-21 - not the GM2 sound controllers - so out of the box its eight knobs did nothing.
    // Those CCs are aliases for the same eight roles, in the same order, and the proof is
    // end-to-end: the alias must move the parameter exactly as the GM2 knob does.
    let Some(mut session) = session("cc-m32-alias") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    for value in [0u8, 40, 96] {
        turn(&mut session, CC_CUTOFF, value);
    }
    let via_gm2 = param(&mut session, "cutoff").value;

    // Reset away from where the GM2 knob left it, then arrive by the alias.
    for value in [0u8, 20] {
        turn(&mut session, 14, value);
    }
    for value in [40u8, 96] {
        turn(&mut session, 14, value);
    }
    let via_alias = param(&mut session, "cutoff").value;

    assert!(
        (via_alias - via_gm2).abs() < 1e-6,
        "CC 14 must land cutoff exactly where CC 74 does: {via_gm2} vs {via_alias}"
    );

    let claimed = ControlMap::shipped().claimed();
    for cc in 14u8..=21 {
        assert!(claimed.contains(cc), "CC {cc} should be a fixed-knob alias");
    }
}

#[test]
fn every_page_holds_eight_slots_so_clap_never_splits_and_renames_one() {
    // nice-plug auto-splits a >8-slot page into "{name} {n}" and numbers pages positionally, so a
    // ninth slot would silently rename a page that the map keys on.
    for page in &schema::shipped().pages {
        assert_eq!(page.slots.len(), 8, "page `{}`", page.title());
    }
}

#[test]
fn a_layout_that_cannot_be_used_is_refused_rather_than_half_applied() {
    let mut layout = schema::shipped();
    layout.fixed[0].cc = layout.fixed[1].cc;
    assert!(Layout::parse(&serde_json::to_string(&layout).unwrap()).is_err());
}

// --- curves -------------------------------------------------------------------------------------

#[test]
fn the_cutoff_knob_sweeps_musically_rather_than_linearly() {
    // The premise of the whole curve system. CLAP carries no skew hint, so a host that maps
    // linearly puts half travel at 10 kHz and the first step at 176 Hz.
    let cutoff = Range::new(20.0, 20_000.0, false);
    let half = curve::to_value(Curve::Log, cutoff, 0.5);
    assert!(
        (400.0..1_000.0).contains(&half),
        "half travel landed at {half} Hz, which is not where a filter sweep's middle belongs"
    );
}

#[test]
fn the_shipped_layout_gives_frequencies_and_times_a_log_curve() {
    let layout = schema::shipped();
    for role in [
        "filter.cutoff",
        "amp_env.attack",
        "amp_env.release",
        "lfo1.rate",
        "voice.glide",
    ] {
        assert_eq!(layout.curve_for(role), Curve::Log, "role `{role}`");
    }
    // And leaves depths alone, where linear is what a player expects.
    assert_eq!(layout.curve_for("filter.resonance"), Curve::Linear);
}

// --- resolution against a loaded instrument -----------------------------------------------------

fn mxm_mono_01_map() -> ControlMap {
    let mut map = ControlMap::shipped();
    map.load_instrument_map(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mxm-mono-01.control-map.json"),
    )
    .expect("mxm-mono-01's own map should load");
    map
}

fn snapshot(id: u32, min: f64, max: f64, value: f64) -> ParamSet {
    ParamSet {
        params: vec![ParamSnapshot {
            id,
            name: "test".into(),
            module: String::new(),
            min,
            max,
            default: value,
            value,
            text: String::new(),
            is_stepped: false,
            is_hidden: false,
            is_read_only: false,
            is_modulatable: true,
            is_bypass: false,
        }],
    }
}

#[test]
fn a_role_this_instrument_does_not_have_is_inert_rather_than_reassigned() {
    // mxm-mono-01 has one oscillator and one envelope. The Osc 2 page is empty on it and must stay
    // empty: silently landing on some neighbouring parameter would be worse than doing nothing.
    let mut map = mxm_mono_01_map();
    assert!(map.param_for(PLUGIN, "osc2.tune").is_none());
    assert!(map.param_for(PLUGIN, "filter_env.attack").is_none());

    // Page 2 is Osc 2. Turning a slot knob there must do nothing at all.
    let params = ParamSet::default();
    assert_eq!(
        map.handle_cc(CC_PAGE_UP, 127, Some(PLUGIN), &params, 0),
        Outcome::PageChanged
    );
    let outcome = map.handle_cc(CC_SLOT_1, 100, Some(PLUGIN), &params, 0);
    assert!(
        matches!(
            outcome,
            Outcome::Absorbed(Absorbed::RoleNotOnThisInstrument { .. })
        ),
        "expected an inert slot, got {outcome:?}"
    );
}

#[test]
fn the_page_knobs_move_the_bank_and_wrap() {
    let mut map = mxm_mono_01_map();
    let params = ParamSet::default();
    let pages = map.page_count();

    assert_eq!(map.active_page(), 0);
    for _ in 0..pages {
        map.handle_cc(CC_PAGE_UP, 127, Some(PLUGIN), &params, 0);
    }
    assert_eq!(map.active_page(), 0, "a full turn should come back round");
}

#[test]
fn a_button_release_does_not_step_the_page_a_second_time() {
    // Controllers commonly send 127 on press and 0 on release; some send only 127. Acting on the
    // press covers both without double-stepping.
    let mut map = mxm_mono_01_map();
    let params = ParamSet::default();
    map.handle_cc(CC_PAGE_UP, 127, Some(PLUGIN), &params, 0);
    assert_eq!(map.active_page(), 1);
    map.handle_cc(CC_PAGE_UP, 0, Some(PLUGIN), &params, 0);
    assert_eq!(map.active_page(), 1, "the release must not step again");
}

#[test]
fn a_knob_away_from_the_parameter_does_not_jump_it() {
    let mut map = mxm_mono_01_map();
    let cutoff = schema::hash_param_id("cutoff");
    // The parameter is at the top of its range; the knob is at the bottom.
    let params = snapshot(cutoff, 20.0, 20_000.0, 20_000.0);

    let outcome = map.handle_cc(CC_CUTOFF, 0, Some(PLUGIN), &params, 0);
    assert!(
        matches!(outcome, Outcome::Absorbed(Absorbed::PickupPending { .. })),
        "expected pickup to hold the value, got {outcome:?}"
    );

    // Swept up to where the parameter is, it takes control.
    let outcome = map.handle_cc(CC_CUTOFF, 127, Some(PLUGIN), &params, 0);
    assert!(
        matches!(outcome, Outcome::Edit(_)),
        "expected the knob to take control, got {outcome:?}"
    );
}

#[test]
fn a_gesture_opens_once_and_closes_when_the_knob_goes_quiet() {
    let mut map = mxm_mono_01_map();
    let cutoff = schema::hash_param_id("cutoff");
    let params = snapshot(cutoff, 20.0, 20_000.0, 20.0);

    let Outcome::Edit(first) = map.handle_cc(CC_CUTOFF, 0, Some(PLUGIN), &params, 0) else {
        panic!("the knob starts where the parameter is, so it should take control at once");
    };
    assert!(first.begin_gesture, "the first move opens a gesture");

    let Outcome::Edit(second) = map.handle_cc(CC_CUTOFF, 1, Some(PLUGIN), &params, 1_000) else {
        panic!("expected a second edit");
    };
    assert!(!second.begin_gesture, "the same turn is one gesture");

    assert!(map.expired_gestures(1_000).is_empty(), "not idle yet");

    let later = 1_000 + mxm_player::control_map::GESTURE_IDLE_NANOS;
    assert_eq!(
        map.expired_gestures(later),
        vec![cutoff],
        "a CC has no release, so going quiet is what ends the edit"
    );
    assert!(!map.has_open_gestures());
}

#[test]
fn closing_everything_leaves_no_gesture_open() {
    // Plugin unload, rescan, engine stop and reload all take this path. An unclosed gesture is
    // something this codebase already treats as a serious failure.
    let mut map = mxm_mono_01_map();
    let cutoff = schema::hash_param_id("cutoff");
    let params = snapshot(cutoff, 20.0, 20_000.0, 20.0);
    map.handle_cc(CC_CUTOFF, 0, Some(PLUGIN), &params, 0);

    assert_eq!(map.close_all_gestures(), vec![cutoff]);
    assert!(!map.has_open_gestures());
}

#[test]
fn a_malformed_reload_keeps_the_map_that_was_working() {
    let dir = std::env::temp_dir().join("mxm-control-map-reload");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("control-map.json");

    // A valid overlay that moves page-up.
    std::fs::write(
        &path,
        r#"{"bank":{"slot_cc":[102,103,104,105,106,107,108,109],
                    "page_down_cc":110,"page_up_cc":115}}"#,
    )
    .unwrap();
    let mut map = ControlMap::load(&path);
    assert!(map.claimed().contains(115), "the overlay should apply");
    assert!(map.last_error().is_none());

    // Now break it mid-edit.
    std::fs::write(&path, "{ oops").unwrap();
    map.reload();

    assert!(
        map.claimed().contains(115),
        "a bad edit must not rearrange the controller; the last good map stays"
    );
    assert!(
        map.last_error().is_some(),
        "and the problem should be reported rather than swallowed"
    );
}

#[test]
fn a_missing_user_file_is_the_ordinary_case_and_not_an_error() {
    let path = std::env::temp_dir().join("mxm-control-map-absent/control-map.json");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    let map = ControlMap::load(&path);
    assert!(map.last_error().is_none());
    // The shipped standard's fourteen pages: the original eight, Chorus, LFO 2, Delay, Shimmer,
    // Classic verb and the Dynamics page appended for mxm-fx-curve. Pinned here as in `mxm-control-map`'s
    // own tests, so a new page is a deliberate act.
    assert_eq!(map.page_count(), 14);
}

// --- through the real player --------------------------------------------------------------------

#[test]
fn a_nice_plug_parameter_reaches_the_host_normalised_so_the_plugin_keeps_its_own_curve() {
    // Worth pinning, because it decides what the curve system is *for*. nice-plug reports
    // `min_value = 0.0` and `max_value = step_count.unwrap_or(1)`
    // (`src/wrapper/clap/wrapper.rs:3760-3764` in the nice-plug fork, mxm-audio/nice-plug; it was
    // `vendor/nice-plug/…:3455-3459` in the monorepo), so a continuous parameter arrives
    // as 0..1 and the plugin applies its own skew inside. Mapping linearly across that therefore
    // *inherits* the plugin's curve, and a host-side log curve on top would double-apply it.
    //
    // The layout's log curves are not wasted: CLAP permits plain ranges, and a plugin reporting
    // 20..20000 Hz genuinely needs one. `curve::to_value` falls back to linear whenever the range
    // starts at zero, which is exactly the normalised case.
    let Some(mut session) = session("cc-normalised") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    let cutoff = param(&mut session, "cutoff");
    assert_eq!((cutoff.min, cutoff.max), (0.0, 1.0));
}

#[test]
fn a_fixed_knob_moves_the_parameter_it_names_on_a_real_plugin() {
    let Some(mut session) = session("cc-cutoff") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    let before = param(&mut session, "cutoff");
    // A full sweep, so the knob is guaranteed to cross wherever the parameter is sitting and
    // pickup engages. Where it starts is a property of the plugin's default, not of the map.
    for value in [0u8, 32, 64, 96, 127] {
        turn(&mut session, CC_CUTOFF, value);
    }
    let after = param(&mut session, "cutoff");

    assert_ne!(
        after.value, before.value,
        "CC {CC_CUTOFF} should have moved Cutoff"
    );
    // Normalised, per the test above. Full travel is full open.
    assert!(
        after.value > 0.99,
        "a knob at the top should open the filter fully, got {}",
        after.value
    );
    // And the plugin's own formatting confirms it landed where a player would expect.
    assert!(
        after.text.contains("kHz"),
        "fully open should format as kilohertz, got `{}`",
        after.text
    );
}

#[test]
fn a_claimed_cc_reaches_the_parameter_and_not_the_plugins_own_cc_handling() {
    // mxm-mono-01 ignores CC 73 itself, so the only way Attack can move is through the map.
    let Some(mut session) = session("cc-attack") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    let before = param(&mut session, "attack");
    for value in [0u8, 60, 127] {
        turn(&mut session, CC_ATTACK, value);
    }
    let after = param(&mut session, "attack");
    assert!(
        after.value > before.value,
        "Attack should have risen: {} -> {}",
        before.value,
        after.value
    );
}

#[test]
fn an_unclaimed_cc_still_reaches_the_plugin() {
    // The mod wheel is reserved precisely so it keeps working as a performance gesture. If the
    // routing swallowed it, this would be silent.
    let Some(mut session) = session("cc-modwheel") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // `vcolfo` until the machine's own modulation became routing; the LFO-to-pitch depth is the
    // (pitch, LFO) pair now, and the wheel is added into it at play time exactly as it was added
    // into the knob.
    let before = param(&mut session, "mod_pitch_lfo");
    turn(&mut session, 1, 127);
    let after = param(&mut session, "mod_pitch_lfo");

    assert_eq!(
        after.value, before.value,
        "the mod wheel modulates live and must not write the LFO-depth parameter"
    );
}

#[test]
fn turning_a_knob_leaves_no_gesture_open_once_it_goes_quiet() {
    let Some(mut session) = session("cc-gesture") else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };

    // A full sweep, so pickup engages wherever the parameter happens to start.
    for value in [0u8, 64, 127] {
        turn(&mut session, CC_CUTOFF, value);
    }
    assert!(
        session.app().control_map().has_open_gestures(),
        "an edit in progress should have an open gesture"
    );

    session.app().close_control_gestures();
    assert!(
        !session.app().control_map().has_open_gestures(),
        "every path that ends an edit must close the gesture"
    );
}
