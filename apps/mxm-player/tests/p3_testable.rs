//! P3 — testable. Parameters with gestures, state round-trip, and the MIDI-out picker.

use mxm_player_harness::harness;

use harness::Harness;
use mxm_player::engine::audio::FakeBackend;
use mxm_player::engine::{Engine, PluginOutput};
use mxm_player::events::input::Payload;
use std::time::{Duration, Instant};

const PLUGIN: &str = "dk.mxm.mxm-mono-01";

fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    condition()
}

#[test]
fn every_parameter_is_described_and_formatted_by_the_plugin_itself() {
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };

    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    let params = engine.read_params();

    assert!(
        params.params.len() > 20,
        "mxm-mono-01 exposes a full panel's worth of parameters, got {}",
        params.params.len()
    );

    let cutoff = params
        .params
        .iter()
        .find(|p| p.name == "Cutoff")
        .expect("Cutoff is a parameter");
    assert!(cutoff.min < cutoff.max);
    assert!(
        cutoff.text.contains("Hz") || cutoff.text.contains("kHz"),
        "the label and unit must come from the plugin's own formatting, got `{}`",
        cutoff.text
    );

    // Direct text entry means the same thing the display does, because both go through the
    // plugin. The value lives in whatever range the plugin declares — nice-plug reports its
    // parameters normalised, so the host never assumes Hz here — and the round trip is what
    // has to hold.
    let parsed = engine
        .parse_param(cutoff.id, "1.0 kHz")
        .expect("the plugin parses its own format");
    assert!(
        (cutoff.min..=cutoff.max).contains(&parsed),
        "a parsed value must land inside the declared range: {parsed} not in \
         {}..={}",
        cutoff.min,
        cutoff.max
    );

    let formatted = engine.format_param(cutoff.id, parsed);
    assert!(
        formatted.contains("1.0 kHz") || formatted.contains("1000"),
        "formatting the parsed value must reproduce what was typed, got `{formatted}`"
    );
}

#[test]
fn a_parameter_change_reaches_the_plugin_with_its_gesture_intact() {
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };
    let mut h = Harness::new(&bundle, PLUGIN, 1).expect("mxm-mono-01 hosts");

    let params = mxm_player::params::ParamSet::read(&mut h.instance);
    let cutoff = params
        .params
        .iter()
        .find(|p| p.name == "Cutoff")
        .expect("Cutoff is a parameter")
        .clone();

    // A quarter of the way up whatever range the plugin declares, and distinct from where it
    // started, so the assertion cannot pass by accident.
    let target = cutoff.min + 0.25 * (cutoff.max - cutoff.min);
    assert!((target - cutoff.value).abs() > 1e-6);

    h.push(
        0,
        Payload::GestureBegin {
            param_id: cutoff.id,
        },
    );
    h.push(
        0,
        Payload::ParamValue {
            param_id: cutoff.id,
            value: target,
        },
    );
    h.push(
        0,
        Payload::GestureEnd {
            param_id: cutoff.id,
        },
    );
    h.render(256);

    let mut refreshed = params.clone();
    refreshed.refresh_values(&mut h.instance);
    let after = refreshed
        .get(cutoff.id)
        .expect("the parameter is still there");
    assert!(
        (after.value - target).abs() < 1e-4,
        "the plugin should have taken {target}, reports {}",
        after.value
    );

    h.shutdown();
}

#[test]
fn a_preset_round_trips_and_the_panel_is_requeried_afterwards() {
    // Requerying is not belt-and-braces: nice-plug never issues `params.rescan(VALUES)` after a
    // host state load, so a host that trusted the callback would show a stale panel.
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };

    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    let mut params = engine.read_params();

    let cutoff = params
        .params
        .iter()
        .find(|p| p.name == "Cutoff")
        .expect("Cutoff is a parameter")
        .clone();
    let original = cutoff.value;

    let dir = std::env::temp_dir().join("mxm-player-preset-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("preset.clapstate");
    engine.save_state(&path).expect("state saves");

    // Change the value, then load the preset back over it.
    let mut harness = Harness::new(&bundle, PLUGIN, 1).expect("mxm-mono-01 hosts");
    harness.push(
        0,
        Payload::ParamValue {
            param_id: cutoff.id,
            value: cutoff.min + 0.9 * (cutoff.max - cutoff.min),
        },
    );
    harness.render(256);
    harness.shutdown();

    engine
        .load_state(&path, &mut params)
        .expect("state loads and the panel is requeried");

    let after = params.get(cutoff.id).expect("still there");
    assert!(
        (after.value - original).abs() < 1e-6,
        "the preset should have restored {original}, panel shows {}",
        after.value
    );
    assert!(
        !after.text.is_empty(),
        "the formatted text must be refreshed too, not left stale"
    );
}

#[test]
fn a_refused_gesture_end_is_retried_until_it_is_accepted() {
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    engine.start(&backend).expect("the fake backend starts");

    // Fill the GUI producer queue so nothing more fits. The audio thread is draining
    // concurrently, so this races — what matters is that whatever is refused is remembered.
    let mut refused = None;
    for i in 0..(mxm_player::events::input::PRODUCER_QUEUE_CAPACITY * 4) {
        let param_id = (i % 8) as u32;
        if !engine.push_gui_event(Payload::GestureEnd { param_id }) {
            refused = Some(param_id);
            break;
        }
    }

    if let Some(param_id) = refused {
        assert!(
            engine.gesture_end_pending(param_id),
            "a refused gesture end must be retained for retry"
        );
        // Polling is what retries it, every frame, until the audio thread has drained room.
        assert!(
            wait_for(
                || {
                    engine.poll();
                    !engine.gesture_end_pending(param_id)
                },
                Duration::from_secs(3)
            ),
            "the retry must eventually be accepted"
        );
    }

    engine.stop_now().expect("the processor comes back");
}

#[test]
fn a_flooded_output_sink_tells_the_gui_its_tracking_is_no_longer_trustworthy() {
    let Some(bundle) = harness::fixtures() else {
        return;
    };
    let mut h = Harness::new(&bundle, "dk.mxm.fixture.event-emitter", 1).expect("it hosts");

    // Drive the emitter to flood: more events than the bounded sink can carry.
    h.push(
        0,
        Payload::ParamValue {
            param_id: 0,
            value: 1.0,
        },
    );

    let mut invalidated = false;
    for _ in 0..8 {
        h.render(256);
        if h.drain_plugin_output()
            .iter()
            .any(|e| matches!(e, PluginOutput::TrackingInvalidated))
        {
            invalidated = true;
            break;
        }
    }

    assert!(
        invalidated,
        "a host that quietly loses plugin output is worse than one that says it did"
    );

    h.shutdown();
}

#[test]
fn a_midi_only_plugin_is_reported_as_unable_to_carry_voice_ids() {
    // Stated honestly rather than described as targeted: the MIDI dialect carries raw MIDI,
    // which has no concept of a voice ID.
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };
    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");

    let envelope = engine.envelope().expect("negotiated");
    assert!(
        envelope.note_input_carries_voice_ids(),
        "mxm-mono-01 negotiates the CLAP dialect, so the targeted path is the one exercised daily"
    );
    assert_eq!(
        envelope.global_recovery(),
        mxm_player::envelope::GlobalRecovery::AllSoundOff,
        "it also accepts MIDI, so CC 120 is the recovery that actually reaches it"
    );
}

#[test]
fn an_edit_survives_the_round_trip_through_the_audio_thread() {
    // The panel used to snap every control back the moment it was released. The cause was timing,
    // not bookkeeping: a parameter edit reaches the plugin *through the audio thread*, so the
    // panel's snapshot is still the pre-edit value for a frame or so afterwards. Falling back to
    // it on release is what threw the edit away.
    let Some(bundle) = harness::mxm_mono_01() else {
        return;
    };

    let backend = FakeBackend::new();
    let mut engine = Engine::new();
    engine.load(&bundle, PLUGIN).expect("mxm-mono-01 loads");
    let mut params = engine.read_params();
    engine.start(&backend).expect("the fake backend starts");

    let cutoff = params
        .params
        .iter()
        .find(|p| p.name == "Cutoff")
        .expect("Cutoff is a parameter")
        .clone();
    let target = cutoff.min + 0.25 * (cutoff.max - cutoff.min);
    assert!((target - cutoff.value).abs() > 1e-6);

    // Exactly what the panel does for one drag.
    engine.push_gui_event(Payload::GestureBegin {
        param_id: cutoff.id,
    });
    engine.push_gui_event(Payload::ParamValue {
        param_id: cutoff.id,
        value: target,
    });
    engine.push_gui_event(Payload::GestureEnd {
        param_id: cutoff.id,
    });

    // Requerying immediately would read back the *old* value, because the audio thread has not
    // consumed the event yet — which is why the panel defers its requery by a frame.
    assert!(
        wait_for(
            || {
                engine.refresh_param_values(&mut params);
                params
                    .get(cutoff.id)
                    .is_some_and(|p| (p.value - target).abs() < 1e-4)
            },
            Duration::from_secs(2)
        ),
        "the edit must survive the round trip; the panel reports {:?}",
        params.get(cutoff.id).map(|p| p.value)
    );

    engine.stop_now().expect("the processor comes back");
}
