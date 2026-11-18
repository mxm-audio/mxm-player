//! MXM Player — a CLAP host for testing and playing the MXM Synth Collection.
//!
//! A host, not a standalone wrapper: it loads bundles at runtime, so it works for every future
//! MXM plugin without rebuilding, and tests the `.clap` we actually ship rather than a
//! standalone wrapper around it.
//!
//! The crate is a library as well as a binary so the verification suite can drive the engine,
//! the envelope and the event paths directly, without a window or an audio device.

/// What `cargo xtask` staged for this crate's tests (the fixtures), below this run's build folder:
/// `CARGO_TARGET_DIR` where it is set, as Linux's cargo wrapper sets it below `MXM_BUILD_ROOT`, or
/// else the workspace's `target/`; `mxm_player_harness::build_dir` for the other tests, which this
/// crate's own can't depend on. Read from the checkout's `target/` on Linux, they loaded Windows'
/// DLLs there (2026-11-18).
#[cfg(test)]
pub(crate) fn staged_for_tests(relative: &str) -> std::path::PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
        })
        .join(relative)
}

pub mod cli;
pub mod clock;
pub mod config;
pub mod control_map;
pub mod discovery;
pub mod engine;
pub mod entry;
pub mod envelope;
pub mod events;
pub mod host;
pub mod midi;
pub mod notifier;
pub mod offline;
pub mod ownership;
pub mod params;
pub mod project;
pub mod sequencer;
pub mod session;
pub mod settings;
pub mod state;
pub mod ui;
