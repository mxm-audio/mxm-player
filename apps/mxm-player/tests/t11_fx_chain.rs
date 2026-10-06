//! T11 — the effect chain, proved on a reference effect that shares no code with the collection.
//!
//! `dk.mxm.fixture.effect` is arithmetic this file can repeat: `out = 0.5·in + 0.5·d`, where `d`
//! is what its line held 480 frames ago and the line takes `in + 0.5·d`. So the chain's output is
//! **computed here from the dry render** and compared to the bit — which proves the boundary,
//! what reaches the effect and what comes back, rather than only that "something changed".
//!
//! The exact comparison runs until the fixture's tail has passed; past that the fixture snaps its
//! line to zero and sleeps, and the reference is held to a tolerance instead. Every event is
//! stamped at time zero, so it lands at the first frame of the block it is drained in, in every
//! run alike.

use mxm_player_harness::harness;

use harness::{CHANNELS, Harness, fixtures, mxm_mono_01};
use mxm_player::events::input::Payload;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

const SOURCE: &str = "dk.mxm.mxm-mono-01";
const EFFECT: &str = "dk.mxm.fixture.effect";
const EFFECT_MONO: &str = "dk.mxm.fixture.effect-mono";
const GUI: usize = 0;
const BLOCK: usize = 256;
/// Enough blocks to hear the note, its release, the echo's tail and the silence after.
const BLOCKS: usize = 420;

/// The fixture's constants, restated so a change there fails here by name.
const ECHO_DELAY: usize = 480;
const ECHO_FEEDBACK: f32 = 0.5;
const AMOUNT: f32 = 0.5;
const EFFECT_TAIL: usize = 5760;
/// What the fixture's residual can be once it snaps: twelve half-lives of feedback.
const TAIL_TOLERANCE: f32 = 1e-3;

// ------------------------------------------------------------------------------------------------
// The reference
// ------------------------------------------------------------------------------------------------

/// The fixture's transfer function, one channel.
struct Echo {
    line: Vec<f32>,
    write: usize,
}

impl Echo {
    fn new() -> Self {
        Self {
            line: vec![0.0; ECHO_DELAY],
            write: 0,
        }
    }

    fn step(&mut self, input: f32) -> f32 {
        let d = self.line[self.write];
        self.line[self.write] = input + ECHO_FEEDBACK * d;
        self.write = (self.write + 1) % ECHO_DELAY;
        AMOUNT * input + ECHO_FEEDBACK * d
    }

    fn clear(&mut self) {
        self.line.fill(0.0);
        self.write = 0;
    }
}

/// Interleaved frames as channels.
fn channels_of(interleaved: &[f32]) -> Vec<Vec<f32>> {
    (0..CHANNELS)
        .map(|c| interleaved.chunks(CHANNELS).map(|frame| frame[c]).collect())
        .collect()
}

/// The last frame at which any channel is not exact zero.
fn last_sounding_frame(channels: &[Vec<f32>]) -> Option<usize> {
    let frames = channels.first().map(Vec::len).unwrap_or(0);
    (0..frames)
        .rev()
        .find(|&f| channels.iter().any(|c| c[f] != 0.0))
}

/// Compares `actual` to `expected` exactly up to `exact_until`, and within the tail tolerance
/// after it. Names the first frame that disagrees.
fn assert_matches(actual: &[Vec<f32>], expected: &[Vec<f32>], exact_until: usize, what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}: channel count");
    for (channel, (a, e)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(a.len(), e.len(), "{what}: length of channel {channel}");
        for (frame, (x, y)) in a.iter().zip(e).enumerate() {
            if frame <= exact_until {
                assert!(
                    x == y,
                    "{what}: channel {channel} frame {frame}: rendered {x}, reference {y}"
                );
            } else {
                assert!(
                    (x - y).abs() <= TAIL_TOLERANCE,
                    "{what}: channel {channel} frame {frame} (past the tail): rendered {x}, \
                     reference {y}"
                );
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// The script
// ------------------------------------------------------------------------------------------------

#[derive(Copy, Clone)]
enum Act {
    NoteOn(u8),
    NoteOff(u8),
    Panic,
    /// Switch the effect at this index off (`true`) or on.
    Bypass(usize, bool),
}

/// The note every test plays: on at block 2, off at block 40.
const NOTE: &[(usize, Act)] = &[(2, Act::NoteOn(60)), (40, Act::NoteOff(60))];

fn payload(act: Act) -> Option<Payload> {
    match act {
        Act::NoteOn(key) => Some(Payload::NoteOn {
            channel: 0,
            key,
            velocity: 100.0 / 127.0,
        }),
        Act::NoteOff(key) => Some(Payload::NoteOff {
            channel: 0,
            key,
            velocity: 0.0,
        }),
        Act::Panic => Some(Payload::GlobalPanic),
        Act::Bypass(..) => None,
    }
}

/// Runs the script through the source and `fx`, and returns the interleaved output.
///
/// `None` when the artifacts are not built; the caller skips with the hint already printed.
fn run(fx: &[&str], script: &[(usize, Act)]) -> Option<Vec<f32>> {
    let source = mxm_mono_01()?;
    let bundle = fixtures()?;
    let chain: Vec<(&Path, &str)> = fx.iter().map(|id| (bundle.as_path(), *id)).collect();
    let mut h = Harness::with_fx(&source, SOURCE, 1, &chain).expect("the chain builds");

    let mut out = Vec::with_capacity(BLOCKS * BLOCK * CHANNELS);
    for block in 0..BLOCKS {
        for (at, act) in script {
            if *at != block {
                continue;
            }
            match act {
                Act::Bypass(index, off) => h.fx[*index].bypassed.store(*off, Ordering::Relaxed),
                other => {
                    let payload = payload(*other).expect("an event");
                    // Stamped at time zero: it lands at frame 0 of this block, every run alike.
                    assert!(h.push_at(GUI, 0, payload), "the queue accepts the event");
                }
            }
        }
        out.extend_from_slice(h.render(BLOCK));
    }
    h.shutdown();
    Some(out)
}

/// The dry render and the frame its sound ends at, asserting the script actually sounded and
/// left room for the tail and the silence after it.
fn dry(script: &[(usize, Act)]) -> Option<(Vec<Vec<f32>>, usize)> {
    let dry = channels_of(&run(&[], script)?);
    let last = last_sounding_frame(&dry).expect("the source sounded");
    assert!(
        last + EFFECT_TAIL + 4 * BLOCK < BLOCKS * BLOCK,
        "the script must leave room past the tail: last sounding frame {last}"
    );
    Some((dry, last))
}

// ------------------------------------------------------------------------------------------------
// The engine's chain
// ------------------------------------------------------------------------------------------------

#[test]
fn an_effect_transforms_the_source_to_the_bit_and_its_tail_outlives_it() {
    let Some((dry, last)) = dry(NOTE) else {
        return;
    };
    let wet = channels_of(&run(&[EFFECT], NOTE).expect("built above"));

    let expected: Vec<Vec<f32>> = dry
        .iter()
        .map(|channel| {
            let mut echo = Echo::new();
            channel.iter().map(|&x| echo.step(x)).collect()
        })
        .collect();
    assert_matches(&wet, &expected, last + EFFECT_TAIL, "one stereo effect");

    // The tail: the effect keeps sounding after the source has stopped, which is the case the
    // graph's sleep must not cut short.
    let tail_frames = last + 1..last + EFFECT_TAIL;
    assert!(
        wet.iter()
            .any(|c| c[tail_frames.clone()].iter().any(|&s| s != 0.0)),
        "the echo must outlive the source"
    );
    // And then the whole graph sleeps: exact zeros, not a decaying residual forever.
    let end = BLOCKS * BLOCK;
    assert!(
        wet.iter()
            .all(|c| c[end - 2 * BLOCK..].iter().all(|&s| s == 0.0)),
        "with the source and the tail both finished, the output is exact silence"
    );
}

#[test]
fn off_is_the_dry_signal_to_the_bit() {
    let Some((dry, _)) = dry(NOTE) else {
        return;
    };
    let mut script = vec![(0, Act::Bypass(0, true))];
    script.extend_from_slice(NOTE);
    let bypassed = channels_of(&run(&[EFFECT], &script).expect("built above"));
    assert!(
        bypassed == dry,
        "an effect that is off must not touch the signal, sample for sample"
    );
}

#[test]
fn an_effect_switched_back_on_starts_clean() {
    // Off while its line still holds the note, back on once the source is silent: nothing may
    // come out, because whatever it held is from before the gap.
    let Some((dry, last)) = dry(NOTE) else {
        return;
    };
    let resume = 300;
    assert!(
        last < resume * BLOCK,
        "the source must be silent by block {resume}; it sounded until frame {last}"
    );
    let mut script = NOTE.to_vec();
    script.push((20, Act::Bypass(0, true)));
    script.push((resume, Act::Bypass(0, false)));
    let wet = channels_of(&run(&[EFFECT], &script).expect("built above"));

    assert!(
        wet.iter()
            .all(|c| c[resume * BLOCK..].iter().all(|&s| s == 0.0)),
        "an effect coming back on must not play what it held before it was switched off"
    );
    // The test bites: the line held something when the switch went off.
    assert!(
        dry.iter()
            .any(|c| c[..20 * BLOCK].iter().any(|&s| s != 0.0)),
        "the source was sounding when the effect was switched off"
    );
}

#[test]
fn a_panic_clears_the_effect_as_it_clears_the_source() {
    let mut script = NOTE.to_vec();
    let panic_at = 30;
    script.push((panic_at, Act::Panic));
    let Some((dry, last)) = dry(&script) else {
        return;
    };
    let wet = channels_of(&run(&[EFFECT], &script).expect("built above"));

    // The reference forgets everything at the panic frame, as the stage's reset does.
    let expected: Vec<Vec<f32>> = dry
        .iter()
        .map(|channel| {
            let mut echo = Echo::new();
            channel
                .iter()
                .enumerate()
                .map(|(frame, &x)| {
                    if frame == panic_at * BLOCK {
                        echo.clear();
                    }
                    echo.step(x)
                })
                .collect()
        })
        .collect();
    assert_matches(&wet, &expected, last + EFFECT_TAIL, "a panic mid-note");
}

#[test]
fn a_mono_effect_hears_the_sum_at_half_and_is_heard_on_both_sides() {
    let Some((dry, last)) = dry(NOTE) else {
        return;
    };
    let wet = channels_of(&run(&[EFFECT_MONO], NOTE).expect("built above"));

    let mut echo = Echo::new();
    let mono: Vec<f32> = (0..dry[0].len())
        .map(|f| echo.step(0.5 * (dry[0][f] + dry[1][f])))
        .collect();
    let expected = vec![mono.clone(), mono];
    assert_matches(
        &wet,
        &expected,
        last + EFFECT_TAIL,
        "one mono effect in a stereo chain",
    );
}

#[test]
fn two_effects_run_in_order() {
    let Some((dry, last)) = dry(NOTE) else {
        return;
    };
    let wet = channels_of(&run(&[EFFECT, EFFECT], NOTE).expect("built above"));

    let expected: Vec<Vec<f32>> = dry
        .iter()
        .map(|channel| {
            let (mut first, mut second) = (Echo::new(), Echo::new());
            channel
                .iter()
                .map(|&x| second.step(first.step(x)))
                .collect()
        })
        .collect();
    assert_matches(&wet, &expected, last + EFFECT_TAIL, "two effects in series");
}

/// **A sleeping graph wakes for an effect that asks to be processed**, which is what makes an
/// effect's own editor work at all.
///
/// nice-plug queues a parameter change from an editor and applies it only when that queue is
/// written, inside `process`; it asks the host to run the plugin by calling `request_flush`. A
/// graph that slept through the request left the plugin never taking the edit, so the knob —
/// redrawn each frame from a value that never moved — jittered under the pointer instead of
/// turning. Reported by the owner against `mxm-chorus-06`, 2026-09-04.
///
/// The flag being **taken** is the proof: only `FxStage::process` consumes it, so a cleared flag
/// means the stage ran.
#[test]
fn an_effect_asking_to_be_processed_wakes_a_sleeping_graph() {
    let Some(source) = mxm_mono_01() else {
        return;
    };
    let Some(bundle) = fixtures() else {
        return;
    };
    let mut h = Harness::with_fx(&source, SOURCE, 1, &[(bundle.as_path(), EFFECT)])
        .expect("the chain builds");

    // Nothing played: the source falls asleep and the chain has nothing to do.
    for _ in 0..64 {
        h.render(BLOCK);
    }
    assert!(
        !h.fx[0].shared.requests.param_flush.load(Ordering::Acquire),
        "nothing has asked for a flush yet"
    );

    // The effect's editor moves a knob: nice-plug queues the value and asks the host to run it.
    h.fx[0]
        .shared
        .requests
        .param_flush
        .store(true, Ordering::Release);
    h.render(BLOCK);

    assert!(
        !h.fx[0].shared.requests.param_flush.load(Ordering::Acquire),
        "the graph slept through the request, so the effect never took its own editor's edit"
    );

    // And it goes back to sleep afterwards: a request wakes the graph for as long as it takes to
    // satisfy it, not for the rest of the session.
    for _ in 0..64 {
        h.render(BLOCK);
    }
    h.fx[0]
        .shared
        .requests
        .param_flush
        .store(true, Ordering::Release);
    h.render(BLOCK);
    assert!(
        !h.fx[0].shared.requests.param_flush.load(Ordering::Acquire),
        "a second request must wake it just as the first did"
    );
    h.shutdown();
}

/// **A switched-off effect still takes its own editor's parameter changes.**
///
/// Off means its audio is not run. It does not mean the plugin stops existing: its editor is
/// openable and its knobs turn, and nice-plug applies what they ask for only when the plugin is
/// processed **or flushed**. With neither, the knobs moved under the pointer and sprang back on
/// release, because nothing ever applied the value — the owner, against a bypassed
/// `mxm-chorus-06`, 2026-09-04.
///
/// The flag being taken is the proof: only the bypassed branch's flush consumes it, since a
/// bypassed stage returns before the processing path that would otherwise take it.
#[test]
fn a_switched_off_effect_still_takes_its_editors_edits() {
    let Some(source) = mxm_mono_01() else {
        return;
    };
    let Some(bundle) = fixtures() else {
        return;
    };
    let mut h = Harness::with_fx(&source, SOURCE, 1, &[(bundle.as_path(), EFFECT)])
        .expect("the chain builds");

    // Switched off, and the graph left with nothing to do.
    h.fx[0].bypassed.store(true, Ordering::Relaxed);
    for _ in 0..64 {
        h.render(BLOCK);
    }

    // Its editor moves a knob.
    h.fx[0]
        .shared
        .requests
        .param_flush
        .store(true, Ordering::Release);
    h.render(BLOCK);

    assert!(
        !h.fx[0].shared.requests.param_flush.load(Ordering::Acquire),
        "a switched-off effect was never reached, so its editor's edit was never applied"
    );

    // And it is still off: flushing is not a way back into the signal.
    assert!(h.fx[0].bypassed.load(Ordering::Relaxed));
    h.shutdown();
}

// ------------------------------------------------------------------------------------------------
// The application's chain
// ------------------------------------------------------------------------------------------------

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mxm-player-fx-chain-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn app_in(dir: &Path) -> mxm_player::ui::PlayerApp {
    let config = mxm_player::config::PlayerConfig::sandboxed(
        dir,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    );
    mxm_player::ui::PlayerApp::with_config(config)
}

#[test]
fn reordering_keeps_each_effect_its_switch() {
    let (Some(source), Some(bundle)) = (mxm_mono_01(), fixtures()) else {
        return;
    };
    let dir = scratch("reorder");
    let mut app = app_in(&dir);
    app.load(source, SOURCE.to_owned());
    app.add_fx(bundle.clone(), EFFECT.to_owned())
        .expect("the stereo effect loads");
    app.add_fx(bundle, EFFECT_MONO.to_owned())
        .expect("the mono effect loads");
    app.set_fx_bypassed(0, true).expect("effect 1 switches off");

    app.move_fx(0, 1).expect("effect 1 moves to 2");
    let state = app.state();
    let ids: Vec<&str> = state.fx.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, [EFFECT_MONO, EFFECT], "the order after the move");
    assert!(
        !state.fx[0].bypassed && state.fx[1].bypassed,
        "the switch belongs to the effect, not to the slot: {:?}",
        state.fx
    );
    assert_eq!(
        (state.fx[0].input_channels, state.fx[1].input_channels),
        (1, 2),
        "each strip reports its own layout"
    );

    app.remove_fx(1).expect("effect 2 leaves");
    let state = app.state();
    assert_eq!(state.fx.len(), 1);
    assert_eq!(state.fx[0].id, EFFECT_MONO);
}

#[test]
fn the_chain_comes_back_after_a_restart_with_its_switches() {
    let (Some(source), Some(bundle)) = (mxm_mono_01(), fixtures()) else {
        return;
    };
    let dir = scratch("restart");
    {
        let mut app = app_in(&dir);
        app.load(source, SOURCE.to_owned());
        app.add_fx(bundle.clone(), EFFECT.to_owned())
            .expect("the stereo effect loads");
        app.add_fx(bundle, EFFECT_MONO.to_owned())
            .expect("the mono effect loads");
        app.set_fx_bypassed(1, true).expect("effect 2 switches off");
    }

    // A new player over the same settings, before any source is loaded: the chain needs none.
    let mut app = app_in(&dir);
    assert!(
        app.state().fx.is_empty(),
        "nothing until the chain is restored"
    );
    app.restore_fx_chain();
    let state = app.state();
    let ids: Vec<&str> = state.fx.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, [EFFECT, EFFECT_MONO]);
    assert_eq!(
        state.fx.iter().map(|f| f.bypassed).collect::<Vec<_>>(),
        [false, true],
        "the switches come back as they were left"
    );
}

/// Reads an exported WAV back as channels.
fn wav_channels(path: &Path) -> Vec<Vec<f32>> {
    let decoded = mxm_audio_file_decode::decode_file(
        path,
        &mxm_audio_file_decode::Limits::new(
            usize::MAX,
            mxm_audio_file_decode::AtLimit::Refuse,
            mxm_audio_file_decode::Keep::AllUpTo(8),
        ),
    )
    .expect("a readable WAV");
    let channels = decoded.channels;
    let samples = decoded.interleaved;
    (0..channels)
        .map(|c| samples.chunks(channels).map(|frame| frame[c]).collect())
        .collect()
}

#[test]
fn an_export_goes_through_the_chain_and_an_effect_that_is_off_stays_out_of_it() {
    let (Some(source), Some(bundle)) = (mxm_mono_01(), fixtures()) else {
        return;
    };
    let dir = scratch("export");
    let mut app = app_in(&dir);
    app.load(source, SOURCE.to_owned());
    app.toggle_step_note(0, 60);
    // Each file is compared to the others sample for sample, so none of them may be levelled.
    app.set_normalise_export(false);

    let alone = wav_channels(&app.export_audio("alone").expect("the source exports"));
    app.add_fx(bundle, EFFECT.to_owned())
        .expect("the stereo effect loads");
    let through = wav_channels(&app.export_audio("through").expect("the chain exports"));
    app.set_fx_bypassed(0, true)
        .expect("the effect switches off");
    let off = wav_channels(&app.export_audio("off").expect("the bypassed chain exports"));

    assert!(
        off == alone,
        "an effect that is off must leave the export exactly as the source alone"
    );
    let last = last_sounding_frame(&alone).expect("the export sounded");
    let expected: Vec<Vec<f32>> = alone
        .iter()
        .map(|channel| {
            let mut echo = Echo::new();
            channel.iter().map(|&x| echo.step(x)).collect()
        })
        .collect();
    assert_matches(
        &through,
        &expected,
        (last + EFFECT_TAIL).min(through[0].len()),
        "the export through one effect",
    );
}

#[test]
fn the_same_session_with_a_chain_renders_identical_audio() {
    let (Some(source), Some(bundle)) = (mxm_mono_01(), fixtures()) else {
        return;
    };
    let roots = vec![source.parent().expect("a directory").to_path_buf()];
    let render = |name: &str, with_chain: bool| -> Vec<f32> {
        let mut session = mxm_player::session::Session::scratch(name, roots.clone());
        session.load(&source, SOURCE);
        if with_chain {
            session
                .app()
                .add_fx(bundle.clone(), EFFECT.to_owned())
                .expect("the effect loads into the session");
        }
        session.advance_blocks(2).expect("the session advances");
        session.app().note_on(60, 100.0 / 127.0);
        session.advance_blocks(8).expect("the session advances");
        session.app().note_off(60);
        session.advance_blocks(24).expect("the session advances");
        session.captured()
    };

    let first = render("fx-determinism-a", true);
    let second = render("fx-determinism-b", true);
    let alone = render("fx-determinism-dry", false);
    assert!(!first.is_empty(), "the session rendered nothing");
    assert!(
        first == second,
        "the same session through the same chain must render byte-identical audio; first \
         divergence at sample {:?}",
        first.iter().zip(&second).position(|(a, b)| a != b)
    );
    assert!(
        first != alone,
        "the chain is in the session's path: with it the audio differs from without"
    );
}
