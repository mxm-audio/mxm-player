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
/// The same effect offering mono in, stereo out first and stereo in, stereo out second, as the
/// MXM effects do.
const EFFECT_LAYOUTS: &str = "dk.mxm.fixture.effect-layouts";
/// The same effect, stereo, its output held back by the latency it reports.
const EFFECT_LATENCY: &str = "dk.mxm.fixture.effect-latency";
const GUI: usize = 0;
const BLOCK: usize = 256;
/// Enough blocks to hear the note, its release, the echo's tail and the silence after.
const BLOCKS: usize = 420;

/// The fixture's constants, restated so a change there fails here by name.
const ECHO_DELAY: usize = 480;
const ECHO_FEEDBACK: f32 = 0.5;
const AMOUNT: f32 = 0.5;
const EFFECT_TAIL: usize = 5760;
/// What `effect-latency` reports and holds its output back by.
const EFFECT_LATENCY_FRAMES: usize = 1000;
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

/// **The chain is told each chunk's first-sample position, as an export tells it.** CLAP's process
/// transport is the state as of the block's first sample. The live worker used to build it after
/// advancing the sequencer, so it reported each chunk's *end* — one chunk ahead of the offline
/// render for the same sample (`offline::tests::an_export_reports_each_block_at_its_first_sample`
/// is the export half). The probe effect writes the position it was given into its output.
#[test]
fn the_chain_is_told_each_chunks_first_sample_position() {
    use mxm_player::engine::processor::Command;
    use mxm_player::sequencer::Transport;
    use mxm_player::sequencer::runtime::SequencerState;

    let Some(source) = mxm_mono_01() else {
        return;
    };
    let Some(bundle) = fixtures() else {
        return;
    };
    let mut h = Harness::with_fx(
        &source,
        SOURCE,
        1,
        &[(bundle.as_path(), "dk.mxm.fixture.transport-probe")],
    )
    .expect("the chain builds");
    let _ = h
        .commands
        .push(Command::SetSequencer(std::sync::Arc::new(SequencerState {
            tempo: 120.0,
            transport: Transport::Playing,
            generation: 1,
            serial: 1,
            ..Default::default()
        })));

    // A held note, so the stage has input and runs: a stage with silence coming in is skipped,
    // and the output would be the source's silence whatever the transport said.
    h.push(0, payload(Act::NoteOn(60)).expect("a note"));

    let first = h.render(BLOCK)[0];
    let second = h.render(BLOCK)[0];
    let one_block = BLOCK as f64 / harness::SAMPLE_RATE * 120.0 / 60.0;
    assert_eq!(first, 0.0, "the first chunk of a sequence is at beat 0");
    assert!(
        (f64::from(second) - one_block).abs() < 1e-6,
        "the second chunk starts one chunk in: {second}, expected {one_block}"
    );
}

// ------------------------------------------------------------------------------------------------
// An effect's layout follows what feeds it, and an export takes its latency back out
// ------------------------------------------------------------------------------------------------

/// **An effect offering both widths takes the one that reaches it**, wherever it sits: a stereo
/// source gives it stereo in, a mono effect before it gives it mono in, and a reorder chooses
/// again. First-compatible took mono in every time, so every MXM effect (each lists mono in first)
/// summed a stereo chain to mono (the owner's drum loops, 2026-11-18).
#[test]
fn an_effect_offering_both_widths_takes_the_one_that_reaches_it() {
    let (Some(source), Some(bundle)) = (mxm_mono_01(), fixtures()) else {
        return;
    };
    let dir = scratch("layouts");
    let mut app = app_in(&dir);
    app.load(source, SOURCE.to_owned());
    app.add_fx(bundle.clone(), EFFECT_LAYOUTS.to_owned())
        .expect("the two-layout effect loads");
    let inputs = |app: &mut mxm_player::ui::PlayerApp| -> Vec<u32> {
        app.state().fx.iter().map(|f| f.input_channels).collect()
    };
    assert_eq!(inputs(&mut app), [2], "after a stereo source");

    app.add_fx(bundle, EFFECT_MONO.to_owned())
        .expect("the mono effect loads");
    app.move_fx(1, 0).expect("the mono effect goes first");
    assert_eq!(
        inputs(&mut app),
        [1, 1],
        "after a mono effect, the two-layout one takes mono in"
    );
    app.move_fx(0, 1).expect("and back");
    assert_eq!(inputs(&mut app), [2, 1], "after the stereo source again");
}

/// The offline chain chooses the same way, proved by the audio: a stereo signal whose right side
/// is silent stays silent on the right through an effect fed it in stereo, where an effect fed the
/// sum would echo the left side on both.
#[test]
fn the_offline_chain_feeds_an_effect_in_stereo_when_the_signal_is_stereo() {
    use mxm_player::offline::{
        EffectSpec, EventKind, RenderConfig, ScheduledEvent, render, through_effects,
    };
    let (Some(source), Some(bundle)) = (mxm_mono_01(), fixtures()) else {
        return;
    };
    let frames = (BLOCKS * BLOCK) as u64;
    let note = |frame, on| ScheduledEvent {
        frame,
        kind: if on {
            EventKind::NoteOn {
                channel: 0,
                key: 60,
                velocity: 0.8,
                note_id: 1,
            }
        } else {
            EventKind::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0.0,
                note_id: 1,
            }
        },
    };
    let config = RenderConfig {
        sample_rate: harness::SAMPLE_RATE,
        block_size: BLOCK as u32,
        total_frames: frames,
        state: None,
        tempo: None,
    };
    let mut dry = render(
        &source,
        SOURCE,
        config,
        &[note(2 * BLOCK as u64, true), note(40 * BLOCK as u64, false)],
    )
    .expect("the source renders");
    assert_eq!(dry.channels.len(), 2, "a stereo source");
    dry.channels[1].fill(0.0);
    let left = dry.channels[0].clone();
    assert!(left.iter().any(|x| *x != 0.0), "the source sounded");

    let wet = through_effects(
        dry,
        &[EffectSpec {
            bundle: bundle.clone(),
            plugin_id: EFFECT_LAYOUTS.to_owned(),
            state: None,
            events: Vec::new(),
        }],
        harness::SAMPLE_RATE,
        BLOCK as u32,
        None,
    )
    .expect("the chain renders");

    let last = left
        .iter()
        .rposition(|x| *x != 0.0)
        .expect("the source sounded");
    let mut echo = Echo::new();
    let expected = vec![
        left.iter().map(|&x| echo.step(x)).collect::<Vec<f32>>(),
        vec![0.0; left.len()],
    ];
    assert_matches(
        &wet.channels,
        &expected,
        last + EFFECT_TAIL,
        "a stereo signal through the two-layout effect",
    );
    assert!(
        wet.channels[1].iter().all(|x| *x == 0.0),
        "the silent side stays silent: the effect was not fed the sum"
    );
}

/// The latency fixture really holds its output back: live, with nothing to compensate it, it is
/// the plain effect exactly `EFFECT_LATENCY_FRAMES` later. Which is what makes the export test
/// below able to fail.
#[test]
fn the_latency_fixture_is_the_plain_effect_late_by_its_latency() {
    let Some((_, last)) = dry(NOTE) else {
        return;
    };
    let plain = channels_of(&run(&[EFFECT], NOTE).expect("built above"));
    let late = channels_of(&run(&[EFFECT_LATENCY], NOTE).expect("built above"));
    let expected: Vec<Vec<f32>> = plain
        .iter()
        .map(|p| {
            let mut shifted = vec![0.0; EFFECT_LATENCY_FRAMES];
            shifted.extend_from_slice(&p[..p.len() - EFFECT_LATENCY_FRAMES]);
            shifted
        })
        .collect();
    // Each snaps its residue to silence at its own block's end, so past the tail it is held to
    // the tolerance, as every chain comparison here is.
    assert_matches(
        &late,
        &expected,
        last + EFFECT_TAIL + EFFECT_LATENCY_FRAMES,
        "the latency fixture, live",
    );
}

/// **An export takes an effect's reported latency back out**: through the latency fixture it is
/// the export through the plain effect, to the bit until the tail has passed. Without it, oldify's
/// 41.6 ms put every export behind its grid and cut its last 41.6 ms (the owner's drum loops,
/// 2026-11-18).
#[test]
fn an_export_takes_an_effects_latency_back_out() {
    let (Some(source), Some(bundle)) = (mxm_mono_01(), fixtures()) else {
        return;
    };
    let dir = scratch("latency");
    let mut app = app_in(&dir);
    app.load(source, SOURCE.to_owned());
    app.toggle_step_note(0, 60);
    app.set_normalise_export(false);
    let alone = wav_channels(&app.export_audio("alone").expect("the source exports"));
    let last = last_sounding_frame(&alone).expect("the export sounded");

    app.add_fx(bundle.clone(), EFFECT.to_owned())
        .expect("the plain effect loads");
    let plain = wav_channels(&app.export_audio("plain").expect("the chain exports"));
    app.remove_fx(0).expect("the plain effect leaves");
    app.add_fx(bundle, EFFECT_LATENCY.to_owned())
        .expect("the latency effect loads");
    let late = wav_channels(&app.export_audio("late").expect("the chain exports"));

    assert_matches(
        &late,
        &plain,
        (last + EFFECT_TAIL).min(plain[0].len()),
        "an export through an effect with latency, against the same effect without it",
    );
}
