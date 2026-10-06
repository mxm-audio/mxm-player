//! The harnesses MXM Player's tests drive it with — [`app_harness`], the real app with no window
//! and a sandboxed settings directory, and [`harness`], the audio worker run on the test thread —
//! shared with every plugin's host tests, which load that plugin's bundle through the real host.

use std::path::{Path, PathBuf};

pub mod app_harness;
pub mod harness;

/// The root of the repository under test: the nearest directory at or above the running test's
/// package that holds `Cargo.lock`. Bundles are staged under its `target/bundled` and the fixture
/// plugins under `target/fixtures`.
///
/// Read from `CARGO_MANIFEST_DIR` at run time — cargo sets it for every test it runs — rather than
/// at compile time, because this crate's own manifest is wherever the dependency was fetched to. So
/// the answer is right in this repository and in each product's own (as it was in the monorepo's
/// workspace).
pub fn workspace_root() -> PathBuf {
    let start = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("a working directory"));
    start
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
        .map(Path::to_path_buf)
        .unwrap_or(start)
}
