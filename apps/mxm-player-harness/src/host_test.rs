//! What every plugin's host tests do the same way: find their own bundle, set a parameter by its
//! display name, and take a golden digest of what came out.
//!
//! Each was a few lines copied into every `plugins/<plugin>/host-tests/tests/*.rs` — the digest seven
//! times, the bundle lookup two dozen, the parameter edit thirteen — and a few lines copied that often
//! is how one host test came to measure differently from its siblings
//! (`plans/plan-architecture-audit.md` A4, B7). What a test does *around* these — settling, reading the
//! parameter's text back, its own skip message — stays in the test.

use mxm_player::events::input::Payload;
use mxm_player::session::Session;
use mxm_player::state::ParamState;
use std::path::PathBuf;

/// `plugin`'s release bundle in the workspace's `target/bundled`, as `(directory, bundle)`, or `None`
/// when it has not been built — the gate a host test skips on. Never another plugin's bundle: in a
/// repository of one plugin's own, the others are never built.
pub fn plugin_bundle(plugin: &str) -> Option<(PathBuf, PathBuf)> {
    let dir = crate::workspace_root().join("target").join("bundled");
    let file = dir.join(format!("{plugin}.clap"));
    file.exists().then_some((dir, file))
}

/// Sends the parameter named `name` to `fraction` of its plain range, as the panel would, and
/// returns what the parameter was before the edit. Nothing is advanced: settling is the test's call.
///
/// # Panics
///
/// If the loaded plugin has no parameter of that name — a renamed parameter is a failing test, not
/// a silently skipped edit.
pub fn push_fraction(session: &mut Session, name: &str, fraction: f64) -> ParamState {
    let param = session
        .state()
        .param(name)
        .unwrap_or_else(|| panic!("`{name}` is not a parameter"))
        .clone();
    session
        .app()
        .engine_mut()
        .push_gui_event(Payload::ParamValue {
            param_id: param.id,
            value: param.min + fraction * (param.max - param.min),
        });
    param
}

/// A stable digest of rendered samples on their exact bit patterns: FNV-1a over each sample's
/// little-endian bits, as 16 hex digits.
///
/// No dependency, and exact — a bit-for-bit comparison expressed compactly, not a perceptual one.
/// Every golden render in the collection is pinned by it, so it must never change: a changed
/// function would move every recorded digest at once.
pub fn digest(samples: &[f32]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for sample in samples {
        for byte in sample.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}
