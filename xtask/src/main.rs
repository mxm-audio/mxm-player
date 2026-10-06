//! This repository's `cargo xtask`: the shared tooling in `mxm-xtask`, plus the one command only
//! this repository needs.
//!
//! `cargo xtask bundle` is `nice_plug_xtask`'s own, followed by staging each instrument's control
//! map beside its bundle (`mxm_xtask::control_maps`).
//!
//! `cargo xtask fixtures` builds and stages the test-only CLAP plugins in `tests/clap-fixtures`.
//! They are not plugins we ship, so they do not go through `bundle`.

mod fixtures;

use std::path::Path;

fn main() -> nice_plug_xtask::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();

    if raw.first().map(String::as_str) == Some("fixtures") {
        return fixtures::stage(&raw[1..]);
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits directly in the workspace root");
    mxm_xtask::main(root)
}
