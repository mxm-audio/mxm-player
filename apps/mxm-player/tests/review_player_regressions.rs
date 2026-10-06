//! Player review regressions: real app persistence, real plugin delivery and Stop admission.
//! Requires mono-01 and bucket-delay bundles plus the quarantined CLAP fixtures.
//! MXM_PLAYER_TEST_BUNDLES optionally selects isolated product bundles (never fixtures).
use harness::Harness;
use mxm_player::control_map::schema::hash_param_id;
use mxm_player::engine::processor::Command;
use mxm_player::params::ParamSet;
use mxm_player::sequencer::locks::{LockKey, LockSet};
use mxm_player::sequencer::{Sequence, SequencerState};
use mxm_player::session::Session;
use mxm_player::settings::Settings;
use mxm_player_harness::harness;
use std::path::PathBuf;
use std::sync::{Arc, atomic::Ordering};

const SOURCE: &str = "dk.mxm.mxm-mono-01";
const FX: &str = "dk.mxm.mxm-bucket-delay";
fn bundle(name: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let p = if name == "mxm-fixtures" {
        root.join("target/fixtures/mxm-fixtures.clap")
    } else {
        std::env::var_os("MXM_PLAYER_TEST_BUNDLES")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target/bundled"))
            .join(format!("{name}.clap"))
    };
    assert!(p.exists(), "required artifact missing: {}", p.display());
    p
}
fn session(name: &str) -> Session {
    let mut s = Session::scratch(&format!("review-{name}-{}", std::process::id()), vec![]);
    s.load(bundle("mxm-mono-01"), SOURCE);
    assert_eq!(s.state().engine, "Running");
    s
}
fn add_fx(s: &mut Session) {
    s.app()
        .add_fx(bundle("mxm-bucket-delay"), FX.to_owned())
        .unwrap();
}
fn command(s: &mut Session, text: &str) {
    let reply = s.app().run_cli_command(text);
    assert!(reply.contains("\"ok\""), "{text}: {reply}");
}
fn saved_with_fx(s: &mut Session) -> PathBuf {
    add_fx(s);
    command(s, "lock 1 fx1:tap_1 0.2");
    s.app().save_sequence("effect").unwrap()
}

#[test]
fn settings_keep_the_effect_target() {
    let mut s = session("settings-target");
    saved_with_fx(&mut s);
    // An ordinary eager save, without relying on the lock debounce.
    s.app().set_fx_bypassed(0, false).unwrap();
    let settings = Settings::load(&s.dir().join("settings.json"));
    let data = settings.sequence_locks.unwrap();
    let entry = data
        .params
        .iter()
        .find(|p| p.param == hash_param_id("tap1"))
        .unwrap();
    assert!(
        entry.fx.is_some(),
        "effect lock persisted as SOURCE: {entry:?}"
    );
}

#[test]
fn adding_the_missing_effect_resolves_pending_locks() {
    let mut s = session("pending-resolve");
    let path = saved_with_fx(&mut s);
    s.app().remove_fx(0).unwrap();
    s.app().load_sequence(&path).unwrap();
    assert!(s.app().sequencer_state().locks.is_empty());
    add_fx(&mut s);
    assert_eq!(
        s.app().sequencer_state().locks.locks(),
        1,
        "adding matching effect must resolve its pending lock"
    );
}

#[test]
fn clear_does_not_resave_unresolved_automation() {
    let mut s = session("pending-clear");
    let path = saved_with_fx(&mut s);
    s.app().remove_fx(0).unwrap();
    s.app().load_sequence(&path).unwrap();
    s.app().clear_pattern();
    let path = s.app().save_sequence("cleared").unwrap();
    let (seq, _) = Sequence::load(&path).unwrap();
    assert!(
        seq.locks.is_none_or(|data| data.params.is_empty()),
        "Clear must clear pending automation too"
    );
}

#[test]
fn lock_on_second_bar_survives_sequence_save() {
    let mut s = session("second-bar");
    add_fx(&mut s);
    s.app().set_bars(2);
    command(&mut s, "lock 17 fx1:tap_1 0.2");
    let path = s.app().save_sequence("two-bars").unwrap();
    s.app().load_sequence(&path).unwrap();
    assert_eq!(
        s.app().sequencer_state().locks.locks(),
        1,
        "lock past step sixteen was lost"
    );
}

#[test]
fn cli_effect_lock_grows_the_sequence() {
    let mut s = session("grow");
    add_fx(&mut s);
    command(&mut s, "lock 33 fx1:tap_1 0.2");
    assert!(
        s.app().sequencer_state().pattern.len() >= 33,
        "accepted lock is outside the playable pattern"
    );
}

#[test]
fn cli_effect_lock_uses_the_declared_parameter_range() {
    let mut s = session("range");
    add_fx(&mut s);
    let params = s.app().engine_mut().read_fx_params(0).unwrap();
    let line = params.params.iter().find(|p| p.name == "Line").unwrap();
    assert!(
        line.max >= 3.0 && line.is_modulatable,
        "probe needs a modulatable enum: {line:?}"
    );
    let id = line.id;
    command(&mut s, "lock 1 fx1:Line 3");
    let fx = s.app().engine_mut().fx_info()[0].id;
    assert_eq!(
        s.app().sequencer_state().locks.get(0, LockKey::fx(fx, id)),
        Some(3.0)
    );
}

#[test]
fn cli_refuses_nonmodulatable_effect_parameters() {
    let mut s = session("modulatable");
    let fixture = bundle("mxm-fixtures");
    s.app()
        .add_fx(fixture, "dk.mxm.fixture.effect".to_owned())
        .unwrap();
    let params = s.app().engine_mut().read_fx_params(0).unwrap();
    assert!(
        !params.params[0].is_modulatable,
        "fixture must not advertise modulation"
    );
    let reply = s.app().run_cli_command("lock 1 fx1:Amount 0.2");
    assert!(
        reply.contains("\"error\""),
        "nonmodulatable parameter accepted: {reply}"
    );
}

#[test]
fn startup_restores_targets_and_later_bar_source_locks() {
    let mut s = session("startup");
    add_fx(&mut s);
    command(&mut s, "lock 33 fx1:tap_1 0.2");
    command(&mut s, "lock 34 cutoff 0.4");
    s.app().set_fx_bypassed(0, false).unwrap();
    let dir = s.dir().to_path_buf();
    drop(s);
    let mut restored = Session::new(dir, vec![]);
    restored.app().restore_fx_chain();
    restored.load(bundle("mxm-mono-01"), SOURCE);
    let fx = restored.app().engine_mut().fx_info()[0].id;
    let locks = restored.app().sequencer_state().locks;
    assert_eq!(
        locks.get(32, LockKey::fx(fx, hash_param_id("tap1"))),
        Some(0.2)
    );
    assert_eq!(locks.get(33, hash_param_id("cutoff")), Some(0.4));
}

#[test]
fn source_switch_parks_only_source_automation_and_restores_it() {
    let mut s = session("switch");
    saved_with_fx(&mut s);
    command(&mut s, "lock 17 cutoff 0.4");
    s.load(bundle("mxm-fixtures"), "dk.mxm.fixture.main-thread");
    assert_eq!(
        s.app().sequencer_state().locks.locks(),
        1,
        "effect survives another source"
    );
    s.load(bundle("mxm-mono-01"), SOURCE);
    let locks = s.app().sequencer_state().locks;
    assert_eq!(locks.locks(), 2);
    assert_eq!(locks.get(16, hash_param_id("cutoff")), Some(0.4));
    assert_eq!(
        locks.get(0, LockKey::fx(1, hash_param_id("tap1"))),
        Some(0.2)
    );
}

#[test]
fn unresolved_automation_survives_settings_restart_and_then_resolves() {
    let mut s = session("pending-startup");
    let path = saved_with_fx(&mut s);
    s.app().remove_fx(0).unwrap();
    s.app().load_sequence(&path).unwrap();
    let dir = s.dir().to_path_buf();
    drop(s);
    let mut restored = Session::new(dir, vec![]);
    restored.load(bundle("mxm-mono-01"), SOURCE);
    assert!(restored.app().sequencer_state().locks.is_empty());
    add_fx(&mut restored);
    assert_eq!(restored.app().sequencer_state().locks.locks(), 1);
}

#[test]
fn replacement_files_do_not_keep_unresolved_locks() {
    for midi in [false, true] {
        let mut s = session(&format!("pending-replace-{midi}"));
        let empty = if midi {
            s.app().save_midi("empty").unwrap()
        } else {
            s.app().save_sequence("empty").unwrap()
        };
        let path = saved_with_fx(&mut s);
        s.app().remove_fx(0).unwrap();
        s.app().load_sequence(&path).unwrap();
        if midi {
            s.app().load_midi(&empty).unwrap();
        } else {
            s.app().load_sequence(&empty).unwrap();
        }
        add_fx(&mut s);
        assert!(s.app().sequencer_state().locks.is_empty());
    }
}

#[test]
fn effect_load_checks_capabilities_and_unknown_numeric_ids() {
    let mut s = session("invalid-load");
    let path = saved_with_fx(&mut s);
    s.app().remove_fx(0).unwrap();
    s.app()
        .add_fx(bundle("mxm-fixtures"), "dk.mxm.fixture.effect".into())
        .unwrap();
    let (mut seq, _) = Sequence::load(&path).unwrap();
    let entry = &mut seq.locks.as_mut().unwrap().params[0];
    entry.fx.as_mut().unwrap().plugin = "dk.mxm.fixture.effect".into();
    entry.param = s.app().engine_mut().read_fx_params(0).unwrap().params[0].id;
    seq.save(&path).unwrap();
    s.app().load_sequence(&path).unwrap();
    assert!(s.app().sequencer_state().locks.is_empty());
    for verb in [
        "lock 1 fx1:4294967294 0.2",
        "lock 1 4294967294 0.2",
        "lock 1 cutoff NaN",
    ] {
        assert!(
            s.app().run_cli_command(verb).contains("\"error\""),
            "{verb}"
        );
    }
}

#[test]
fn editing_a_live_effect_lock_does_not_adopt_its_modulated_readback_as_patch() {
    let mut s = session("baseline");
    add_fx(&mut s);
    command(&mut s, "lock 1 fx1:tap_1 0.2");
    let key = LockKey::fx(1, hash_param_id("tap1"));
    let patch = s.app().sequencer_state().locks.patch(key);
    command(&mut s, "select 1");
    s.advance_blocks(4).unwrap();
    command(&mut s, "lock 1 fx1:tap_1 0.3");
    assert_eq!(s.app().sequencer_state().locks.patch(key), patch);
}

#[test]
fn an_unidentifiable_effect_is_refused_not_written_as_source() {
    let mut locks = LockSet::EMPTY;
    locks.set(0, LockKey::fx(7, 9), 0.2, 0.5).unwrap();
    assert!(
        mxm_player::sequencer::locks::LockData::capture_in_chain(&locks, None, &[], &[]).is_err()
    );
}

#[test]
fn engine_services_effect_callbacks_once_per_main_thread_turn() {
    let mut engine = mxm_player::engine::Engine::new();
    engine
        .add_fx(&bundle("mxm-fixtures"), "dk.mxm.fixture.effect-main-thread")
        .unwrap();
    assert_eq!(engine.read_fx_params(0).unwrap().params[0].value, 0.0);
    engine.poll();
    assert_eq!(engine.read_fx_params(0).unwrap().params[0].value, 1.0);
    engine.poll();
    assert_eq!(engine.read_fx_params(0).unwrap().params[0].value, 2.0);
    engine.poll();
    assert_eq!(engine.read_fx_params(0).unwrap().params[0].value, 2.0);
}

#[test]
fn removing_and_reordering_real_effects_preserves_only_the_survivors_locks() {
    let mut s = session("chain-identities");
    add_fx(&mut s);
    add_fx(&mut s);
    command(&mut s, "lock 1 fx1:tap_1 0.2");
    command(&mut s, "lock 1 fx2:tap_1 0.7");
    s.app().move_fx(0, 1).unwrap();
    s.app().remove_fx(0).unwrap();
    add_fx(&mut s);
    let ids = s.app().engine_mut().fx_info();
    let locks = s.app().sequencer_state().locks;
    assert_eq!(locks.locks(), 1);
    assert_eq!(
        locks.get(0, LockKey::fx(ids[0].id, hash_param_id("tap1"))),
        Some(0.2)
    );
    assert_eq!(
        locks.get(0, LockKey::fx(ids[1].id, hash_param_id("tap1"))),
        None
    );
}

#[test]
fn clearing_or_shrinking_unresolved_bars_cannot_resurrect_their_locks() {
    for shrink in [false, true] {
        let mut s = session(&format!("pending-resize-{shrink}"));
        add_fx(&mut s);
        command(&mut s, "lock 1601 fx1:tap_1 0.2");
        let path = s.app().save_sequence("long").unwrap();
        s.app().remove_fx(0).unwrap();
        s.app().load_sequence(&path).unwrap();
        if shrink {
            s.app().set_bars(1);
        } else {
            command(&mut s, "bar 101");
            s.app().clear_bar();
        }
        add_fx(&mut s);
        assert!(s.app().sequencer_state().locks.is_empty());
    }
    let mut s = session("live-resize");
    add_fx(&mut s);
    command(&mut s, "lock 1601 fx1:tap_1 0.2");
    s.app().set_bars(1);
    assert!(s.app().sequencer_state().locks.is_empty());
}

fn chain() -> Harness {
    Harness::with_fx(
        &bundle("mxm-mono-01"),
        SOURCE,
        1,
        &[(&bundle("mxm-bucket-delay"), FX)],
    )
    .unwrap()
}
fn read_tap(h: &mut Harness) -> f64 {
    ParamSet::read(&mut h.fx[0].instance)
        .get(hash_param_id("tap1"))
        .unwrap()
        .value
}
fn preview(h: &mut Harness, patch: f32) {
    let mut locks = LockSet::EMPTY;
    locks
        .set(0, LockKey::fx(1, hash_param_id("tap1")), 0.2, patch)
        .unwrap();
    h.commands
        .push(Command::SetSequencer(Arc::new(SequencerState {
            locks,
            editing: Some(0),
            serial: 1,
            ..Default::default()
        })))
        .unwrap();
}
#[test]
fn a_sleeping_effect_receives_a_lock_preview() {
    let mut h = chain();
    for _ in 0..16 {
        h.render(512);
    }
    let patch = read_tap(&mut h) as f32;
    assert!((patch - 0.2).abs() > 0.1);
    preview(&mut h, patch);
    h.render(512);
    let got = read_tap(&mut h);
    assert!(
        (got - 0.2).abs() < 1e-5,
        "preview was silently discarded: {got}"
    );
}
#[test]
fn bypass_does_not_lose_the_last_modulation_zero() {
    let mut h = chain();
    let patch = read_tap(&mut h) as f32;
    preview(&mut h, patch);
    h.fx[0]
        .shared
        .requests
        .process
        .store(true, Ordering::Release);
    h.render(512);
    assert!(
        (read_tap(&mut h) - 0.2).abs() < 1e-5,
        "offset must first have reached plugin"
    );
    h.fx[0].bypassed.store(true, Ordering::Release);
    h.commands
        .push(Command::SetSequencer(Arc::new(SequencerState {
            serial: 2,
            ..Default::default()
        })))
        .unwrap();
    h.render(512);
    h.fx[0].bypassed.store(false, Ordering::Release);
    h.fx[0]
        .shared
        .requests
        .process
        .store(true, Ordering::Release);
    h.render(512);
    let got = read_tap(&mut h);
    assert!(
        (got - f64::from(patch)).abs() < 1e-5,
        "cleared lock survived bypass/reset: {got}, patch {patch}"
    );
}

#[test]
fn rapid_effect_locks_and_chain_restarts_complete_without_wedging() {
    let mut s = session("stress");
    add_fx(&mut s);
    command(&mut s, "toggle 1 C3");
    for round in 0..4 {
        command(&mut s, "play");
        for block in 0..1024 {
            if block % 8 == 0 {
                let step = 1 + (block / 8) % 16;
                for tap in 1..=6 {
                    let value = ((block + tap * 7 + round) % 101) as f32 / 100.0;
                    command(&mut s, &format!("lock {step} fx1:tap_{tap} {value}"));
                }
            }
            s.advance_blocks(1).unwrap();
        }
        assert_eq!(s.state().engine, "Running");
        s.app().remove_fx(0).unwrap();
        add_fx(&mut s);
    }
    assert!(s.peak() > 1e-3, "must have rendered real audio");
    s.app().engine_mut().stop_now().unwrap();
    eprintln!(
        "stress: 4096 blocks / 43.69 audio seconds, 3072 CLI tap-lock edits, four remove/add cycles; no wedge"
    );
}

// A backend which retains a healthy worker but never dispatches a callback. This models
// callback starvation, NOT a plugin which hangs. No device and no worker thread is involved.
struct NoCallbacks;
struct HeldStream {
    _worker: mxm_player::engine::processor::AudioWorker,
}
impl mxm_player::engine::stream::AudioStream for HeldStream {
    fn play(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn device_name(&self) -> String {
        "review/no callbacks".into()
    }
    fn sample_rate(&self) -> f64 {
        48_000.0
    }
    fn channel_count(&self) -> usize {
        2
    }
}
impl mxm_player::engine::audio::Backend for NoCallbacks {
    fn devices(&self) -> Vec<String> {
        vec!["review/no callbacks".into()]
    }
    fn sample_rates(&self, _: Option<&str>) -> Vec<u32> {
        vec![48_000]
    }
    fn open(
        &self,
        _: &mxm_player::engine::AudioConfig,
        _: u32,
    ) -> Result<mxm_player::engine::audio::DeviceChoice, String> {
        Ok(mxm_player::engine::audio::DeviceChoice {
            name: "review/no callbacks".into(),
            sample_rate: 48_000.0,
            channel_count: 2,
            requested_buffer_size: None,
        })
    }
    fn build(
        &self,
        _: &mxm_player::engine::audio::DeviceChoice,
        worker: mxm_player::engine::processor::AudioWorker,
        _: mxm_player::engine::audio::ErrorSink,
        _: Arc<mxm_player::engine::meters::Meters>,
    ) -> Result<Box<dyn mxm_player::engine::stream::AudioStream>, String> {
        Ok(Box::new(HeldStream { _worker: worker }))
    }
}
#[test]
fn wedged_does_not_prove_a_plugin_callback_was_entered() {
    use mxm_player::engine::{Engine, EngineState, WEDGE_TIMEOUT};
    let mut engine = Engine::new();
    engine.load(&bundle("mxm-mono-01"), SOURCE).unwrap();
    engine.start(&NoCallbacks).unwrap();
    std::thread::sleep(WEDGE_TIMEOUT + std::time::Duration::from_millis(50));
    engine.poll();
    assert_eq!(
        *engine.state(),
        EngineState::Running,
        "no processing watchdog exists"
    );
    assert_eq!(engine.meters.callbacks(), 0);
    assert!(engine.stop_now().is_err());
    assert!(engine.is_wedged());
    assert_eq!(
        engine.meters.callbacks(),
        0,
        "this backend NEVER called the plugin"
    );
    eprintln!(
        "healthy uncalled plugin: Running beyond 3s; Wedged only after stop request timed out"
    );
}
#[test]
fn a_full_command_queue_cannot_report_a_successful_stop() {
    use mxm_player::engine::{Engine, EngineState};
    let mut engine = Engine::new();
    engine.load(&bundle("mxm-mono-01"), SOURCE).unwrap();
    engine.start(&NoCallbacks).unwrap();
    for _ in 0..64 {
        assert!(engine.set_claimed_ccs(Default::default()));
    }
    assert!(!engine.set_claimed_ccs(Default::default()));
    let result = engine.stop_now();
    let still_running = *engine.state() == EngineState::Running;
    // On the fixed path ownership is already quarantined by the timeout. Keep this guard so
    // fix-removal falsification cannot destroy an active instance after a false clean stop.
    if still_running {
        std::mem::forget(engine);
    }
    assert!(
        !(result.is_ok() && still_running),
        "stop_now returned Ok while Stop was never queued and engine is still Running"
    );
}
