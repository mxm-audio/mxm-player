//! P0 — the refusal paths, driven by deliberately out-of-envelope fixtures.
//!
//! This is what keeps third-party readiness honest without third-party plugins to test against:
//! each fixture is refused **with the right reason**, rather than crashing or being quietly
//! half-accepted.
//!
//! Requires `cargo xtask fixtures --release`.

use mxm_player::envelope::{Dialect, Refusal};
use mxm_player::offline::{RenderError, inspect, list_plugins};
use std::path::{Path, PathBuf};

fn fixtures() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the player lives two levels below the workspace root")
        .join("target/fixtures/mxm-fixtures.clap");

    if path.exists() {
        Some(path)
    } else {
        eprintln!(
            "skipping: {} is missing — run `cargo xtask fixtures --release`",
            path.display()
        );
        None
    }
}

fn refusal(bundle: &Path, id: &str) -> Refusal {
    match inspect(bundle, id) {
        Err(RenderError::Refused(r)) => r,
        Err(other) => panic!("`{id}` failed for the wrong reason: {other}"),
        Ok(envelope) => panic!("`{id}` should have been refused, but was accepted: {envelope:?}"),
    }
}

#[test]
fn out_of_envelope_plugins_are_refused_with_the_right_reason() {
    let Some(bundle) = fixtures() else { return };

    assert_eq!(
        refusal(&bundle, "dk.mxm.fixture.audio-input"),
        Refusal::HasAudioInputs(1)
    );
    assert_eq!(
        refusal(&bundle, "dk.mxm.fixture.two-outputs"),
        Refusal::WrongOutputPortCount(2)
    );
    assert_eq!(
        refusal(&bundle, "dk.mxm.fixture.surround"),
        Refusal::UnsupportedChannelCount(4)
    );
    assert_eq!(
        refusal(&bundle, "dk.mxm.fixture.non-main-output"),
        Refusal::OutputNotMain
    );
    assert_eq!(
        refusal(&bundle, "dk.mxm.fixture.no-config"),
        Refusal::NoCompatibleConfiguration
    );
}

#[test]
fn refusals_never_crash_the_host_and_are_repeatable() {
    let Some(bundle) = fixtures() else { return };

    // Loading and refusing the same hostile fixture repeatedly must be as safe as doing it once.
    for _ in 0..5 {
        assert_eq!(
            refusal(&bundle, "dk.mxm.fixture.two-outputs"),
            Refusal::WrongOutputPortCount(2)
        );
    }
}

#[test]
fn in_envelope_fixtures_are_accepted_and_negotiate_their_own_dialect() {
    let Some(bundle) = fixtures() else { return };

    let clap_and_midi = inspect(&bundle, "dk.mxm.fixture.event-emitter")
        .expect("the event emitter is inside the envelope");
    assert_eq!(clap_and_midi.audio.channel_count, 2);
    assert_eq!(
        clap_and_midi.note_input.map(|p| p.dialect),
        Some(Dialect::Clap),
        "CLAP is preferred when both dialects are offered"
    );
    assert!(
        clap_and_midi.note_output.is_some(),
        "the emitter declares a note output port, negotiated independently of its input"
    );

    let clap_only = inspect(&bundle, "dk.mxm.fixture.clap-only-notes")
        .expect("a CLAP-only note port is inside the envelope");
    assert_eq!(clap_only.note_input.map(|p| p.dialect), Some(Dialect::Clap));
    assert!(
        clap_only.note_output.is_none(),
        "declaring no note output must not be mistaken for declaring one"
    );
}

#[test]
fn every_fixture_in_the_matrix_is_present_in_the_bundle() {
    let Some(bundle) = fixtures() else { return };

    let ids = list_plugins(&bundle).expect("the fixture bundle exposes a plugin factory");
    for expected in [
        "dk.mxm.fixture.audio-input",
        "dk.mxm.fixture.two-outputs",
        "dk.mxm.fixture.surround",
        "dk.mxm.fixture.no-config",
        "dk.mxm.fixture.non-main-output",
        "dk.mxm.fixture.event-emitter",
        "dk.mxm.fixture.cpu-load",
        "dk.mxm.fixture.hang",
        "dk.mxm.fixture.log-spam",
        "dk.mxm.fixture.tail-shift",
        "dk.mxm.fixture.clap-only-notes",
    ] {
        assert!(
            ids.iter().any(|id| id == expected),
            "the verification matrix needs `{expected}`, which the bundle does not expose"
        );
    }
}
