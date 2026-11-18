//! The harnesses MXM Player's tests drive it with — [`app_harness`], the real app with no window
//! and a sandboxed settings directory, and [`harness`], the audio worker run on the test thread —
//! shared with every plugin's host tests, which load that plugin's bundle through the real host.
//! [`host_test`] is what those host tests do the same way: their bundle, a parameter by name, and
//! the golden digest.

use std::path::{Path, PathBuf};

pub mod app_harness;
pub mod harness;
pub mod host_test;

/// The root of the repository under test: the nearest directory at or above the running test's
/// package that holds `Cargo.lock`. Bundles are staged under its build folder's `bundled` and the
/// fixture plugins under its `fixtures` ([`build_dir`]).
///
/// Read from `CARGO_MANIFEST_DIR` at run time — cargo sets it for every test it runs — rather than
/// at compile time, because this crate's own manifest is wherever the dependency was fetched to. So
/// the answer is right in this repository and in each product's own (as it was in the monorepo's
/// workspace).
pub fn workspace_root() -> PathBuf {
    workspace_root_inner()
}

/// The build folder this run builds in, where the xtasks stage what the tests load (the plugins'
/// bundles, the fixtures): `CARGO_TARGET_DIR` where it is set, as Linux's cargo wrapper sets it below
/// `MXM_BUILD_ROOT`, or else the workspace's `target/`. Read from there, a test never loads another
/// system's builds from a checkout two systems share (Linux's loaded Windows' DLLs from `target/`,
/// 2026-11-18).
pub fn build_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

fn workspace_root_inner() -> PathBuf {
    let start = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("a working directory"));
    start
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
        .map(Path::to_path_buf)
        .unwrap_or(start)
}
