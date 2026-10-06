//! Builds and stages the test-only CLAP fixture plugins.
//!
//! The player's tests load these **by path**, never through its scanner, so a deliberately
//! hostile fixture (one of them never returns from `process()`) cannot end up in an ordinary
//! scan. Staging therefore goes to `target/fixtures/`, well away from `target/bundled/`.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

const PACKAGE: &str = "clap-fixtures";
const BUNDLE_NAME: &str = "mxm-fixtures";

/// Builds `clap-fixtures` and stages it as `target/fixtures/mxm-fixtures.clap`.
///
/// Extra arguments are passed through to `cargo build`, so `--release` and `--target` work as
/// they do for `bundle`. Without `--release` a debug build is staged, which is what the
/// allocation-hook tests want.
pub fn stage(args: &[String]) -> Result<()> {
    let workspace_root = workspace_root()?;
    let release = args.iter().any(|a| a == "--release");

    let status = Command::new(cargo())
        .current_dir(&workspace_root)
        .arg("build")
        .args(["-p", PACKAGE])
        .args(args)
        .status()
        .context("Could not run `cargo build` for the fixtures")?;
    if !status.success() {
        bail!("Building `{PACKAGE}` failed");
    }

    let profile_dir = if release { "release" } else { "debug" };
    let built = workspace_root
        .join("target")
        .join(profile_dir)
        .join(library_file_name());
    if !built.exists() {
        bail!("Expected the fixture library at {}", built.display());
    }

    let staging_dir = workspace_root.join("target").join("fixtures");
    std::fs::create_dir_all(&staging_dir)
        .with_context(|| format!("Could not create {}", staging_dir.display()))?;

    let staged = stage_bundle(&built, &staging_dir)?;
    println!("Staged fixtures at {}", staged.display());
    Ok(())
}

/// A bare `.clap` file is the correct shape on Windows and Linux; macOS needs the bundle layout.
#[cfg(not(target_os = "macos"))]
fn stage_bundle(built: &Path, staging_dir: &Path) -> Result<PathBuf> {
    let target = staging_dir.join(format!("{BUNDLE_NAME}.clap"));
    std::fs::copy(built, &target)
        .with_context(|| format!("Could not copy the fixture library to {}", target.display()))?;
    Ok(target)
}

#[cfg(target_os = "macos")]
fn stage_bundle(built: &Path, staging_dir: &Path) -> Result<PathBuf> {
    let bundle = staging_dir.join(format!("{BUNDLE_NAME}.clap"));
    let macos_dir = bundle.join("Contents").join("MacOS");
    std::fs::create_dir_all(&macos_dir)
        .with_context(|| format!("Could not create {}", macos_dir.display()))?;

    let target = macos_dir.join(BUNDLE_NAME);
    std::fs::copy(built, &target)
        .with_context(|| format!("Could not copy the fixture library to {}", target.display()))?;

    let plist = bundle.join("Contents").join("Info.plist");
    std::fs::write(&plist, info_plist())
        .with_context(|| format!("Could not write {}", plist.display()))?;

    Ok(bundle)
}

#[cfg(target_os = "macos")]
fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{BUNDLE_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>dk.mxm.fixtures</string>
    <key>CFBundleName</key>
    <string>{BUNDLE_NAME}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
</dict>
</plist>
"#
    )
}

fn library_file_name() -> String {
    if cfg!(target_os = "windows") {
        "clap_fixtures.dll".to_owned()
    } else if cfg!(target_os = "macos") {
        "libclap_fixtures.dylib".to_owned()
    } else {
        "libclap_fixtures.so".to_owned()
    }
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// `cargo xtask` runs from wherever the user invoked it, so find the workspace root the same way
/// `nice_plug_xtask` does: walk up from this crate's manifest directory.
fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .context("Could not determine the workspace root from the xtask manifest directory")
}
