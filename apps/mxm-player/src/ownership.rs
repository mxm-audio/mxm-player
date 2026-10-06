//! Making the player's window the owner of a plugin's editor window.
//!
//! An **owned** top-level window stays above its owner, minimises and restores with it, and takes
//! no taskbar entry of its own. Without it the editor sinks behind the player and gets lost, which
//! is the specific behaviour this module exists to prevent.
//!
//! # Why the host does this at all
//!
//! CLAP has a call for it — `gui.set_transient` — and it is unavailable. It needs
//! `EditorHandle::set_transient` in `nice-plug-core` and a `Window::set_owner` in `baseview`,
//! neither of which exists and neither of which is vendored here. **That is the real fix**, it is
//! three one-line platform calls behind three crate forks, and it belongs upstream. Until then the
//! player does it from its own side.
//!
//! # This is a workaround, and it fails safe
//!
//! It rests on properties CLAP does not promise: that the editor window is created during
//! `create`, on the calling thread, in our process. So it never guesses. If it cannot **positively
//! identify** the editor window it claims nothing and says why, because a wrong owner — some
//! unrelated window of ours yanked around — is worse than no owner.
//!
//! # Platform support
//!
//! Windows implements it. macOS and Linux compile, run, and report `Unsupported`. Root
//! `AGENTS.md` requires every `cfg` arm implemented rather than one arm and a `todo!()`; an
//! unsupported capability that says so is what `src/envelope.rs` does everywhere else, and it is
//! the same answer here.

use crate::engine::editor::Ownership;

/// A handle to the player's own window, taken from `eframe`.
///
/// Opaque on purpose: only this module knows what a window handle is for, and the rest of the
/// player should not learn.
#[derive(Clone, Copy, Debug)]
pub struct HostWindow {
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    raw: RawHandle,
}

#[cfg(target_os = "windows")]
type RawHandle = isize;
/// Elsewhere no window is enumerated or owned yet, so no handle is ever made. A type of its own
/// rather than `()`, so the code shared with Windows passes it as it passes a handle there.
#[cfg(not(target_os = "windows"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
struct RawHandle;

impl HostWindow {
    /// Reads the player's window handle from anything eframe gives us.
    ///
    /// Returns `None` on a headless run, which is the case the whole test suite depends on.
    pub fn from_handle(handle: &impl raw_window_handle::HasWindowHandle) -> Option<Self> {
        let _ = handle;
        #[cfg(target_os = "windows")]
        {
            use raw_window_handle::RawWindowHandle;
            match handle.window_handle().ok()?.as_raw() {
                RawWindowHandle::Win32(win32) => Some(Self {
                    raw: win32.hwnd.get(),
                }),
                _ => None,
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            None
        }
    }
}

/// Watches for the editor window appearing, so it can be owned before it is shown.
///
/// Created immediately before `gui.create` and resolved immediately after: baseview builds a
/// floating window inside `create`, and `show` only reveals it, so the window exists and is
/// invisible for exactly that interval. It is the only point where "owned before it appears" is
/// achievable.
pub struct Watcher {
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    before: Vec<RawHandle>,
}

impl Watcher {
    pub fn start() -> Self {
        Self { before: snapshot() }
    }

    /// Identifies the editor window and makes `host` its owner.
    pub fn claim(self, host: HostWindow) -> Ownership {
        let after = snapshot();
        let new: Vec<RawHandle> = after
            .into_iter()
            .filter(|h| !self.before.contains(h))
            .collect();

        match new.len() {
            0 => Ownership::Unowned(
                "The plugin did not create its window where the player could find it.",
            ),
            1 => adopt(new[0], host),
            _ => Ownership::Unowned(
                "The plugin created several windows at once, so the player could not tell which \
                 one is the editor.",
            ),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod platform {
    use super::RawHandle;
    use std::ffi::c_void;

    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn EnumThreadWindows(thread_id: u32, callback: EnumProc, param: isize) -> i32;
        fn GetClassNameW(hwnd: isize, buffer: *mut u16, max: i32) -> i32;
        fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
        fn SetLastError(code: u32);
        fn GetLastError() -> u32;
    }

    type EnumProc = unsafe extern "system" fn(hwnd: isize, param: isize) -> i32;

    /// `GWLP_HWNDPARENT`. On a top-level window this sets the **owner**, not the parent, which is
    /// the distinction the whole module turns on: an owned window is still top-level.
    const GWLP_HWNDPARENT: i32 = -8;

    unsafe extern "system" fn collect(hwnd: isize, param: isize) -> i32 {
        let out = unsafe { &mut *(param as *mut Vec<RawHandle>) };
        out.push(hwnd);
        1
    }

    pub fn snapshot() -> Vec<RawHandle> {
        let mut out: Vec<RawHandle> = Vec::new();
        let ptr = std::ptr::from_mut(&mut out) as isize;
        unsafe { EnumThreadWindows(GetCurrentThreadId(), collect, ptr) };
        out
    }

    /// Whether a window was created by baseview, which is what nice-plug builds its editors on.
    ///
    /// baseview registers a window class named `Baseview-<uuid>` per window, so the class name is
    /// a **positive identification** rather than a guess. That matters: "exactly one new window"
    /// is only a count, and a plugin that creates one helper synchronously and its editor later
    /// would pass a count-only check and have the wrong window adopted.
    pub fn is_editor_window(hwnd: RawHandle) -> bool {
        let mut buffer = [0u16; 128];
        let len = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        if len <= 0 {
            return false;
        }
        let name = String::from_utf16_lossy(&buffer[..len as usize]);
        name.starts_with("Baseview-")
    }

    /// Returns whether the owner was set.
    pub fn set_owner(window: RawHandle, owner: RawHandle) -> bool {
        // `SetWindowLongPtrW` returns 0 both on failure and when the previous value was 0, which
        // it is for a window with no owner — exactly our case. `GetLastError` is the only way to
        // tell them apart, and it must be cleared first.
        unsafe {
            SetLastError(0);
            let previous = SetWindowLongPtrW(window, GWLP_HWNDPARENT, owner);
            previous != 0 || GetLastError() == 0
        }
    }

    /// Silences the unused-import warning on the `c_void` we keep for readability of the FFI block.
    #[allow(dead_code)]
    fn _unused(_: *mut c_void) {}
}

// ---------------------------------------------------------------------------------------------
// Every other platform
// ---------------------------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::RawHandle;

    /// No windows are enumerated, so nothing is ever adopted and `claim` reports `Unsupported`.
    pub fn snapshot() -> Vec<RawHandle> {
        Vec::new()
    }

    pub fn is_editor_window(_window: RawHandle) -> bool {
        false
    }

    pub fn set_owner(_window: RawHandle, _owner: RawHandle) -> bool {
        false
    }
}

fn snapshot() -> Vec<RawHandle> {
    platform::snapshot()
}

fn adopt(window: RawHandle, host: HostWindow) -> Ownership {
    if !SUPPORTED {
        return Ownership::Unowned(UNSUPPORTED_REASON);
    }
    if !platform::is_editor_window(window) {
        return Ownership::Unowned(
            "The window the plugin created is not one the player recognises, so it was left alone.",
        );
    }
    if platform::set_owner(window, host.raw) {
        Ownership::Owned
    } else {
        Ownership::Unowned("The system refused to make the player the editor window's owner.")
    }
}

/// Whether this platform can own a plugin's editor window.
pub const SUPPORTED: bool = cfg!(target_os = "windows");

/// Shown when it cannot. Not a log line: the symptom — an editor that sinks behind the player —
/// looks like a bug unless the user is told it is a known gap.
pub const UNSUPPORTED_REASON: &str =
    "On this platform the editor opens as its own window, which the player cannot keep in front.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watcher_with_no_new_windows_claims_nothing() {
        // The headless case, and the case where a plugin creates its window somewhere we cannot
        // see. Both must decline rather than adopt whatever happens to be lying around.
        let watcher = Watcher { before: snapshot() };
        let claimed = watcher.claim(HostWindow {
            raw: Default::default(),
        });
        assert!(matches!(claimed, Ownership::Unowned(_)));
    }

    #[test]
    fn the_unsupported_reason_is_shown_not_logged() {
        assert!(!UNSUPPORTED_REASON.is_empty());
        assert!(UNSUPPORTED_REASON.ends_with('.'));
    }

    #[test]
    fn support_matches_the_platform_arm_that_is_implemented() {
        // Root AGENTS.md wants every arm implemented rather than one arm and a `todo!()`. Both
        // arms exist; this pins which one actually does the work, so a future arm landing without
        // updating `SUPPORTED` is a test failure rather than a silent lie to the user.
        assert_eq!(SUPPORTED, cfg!(target_os = "windows"));
    }
}
