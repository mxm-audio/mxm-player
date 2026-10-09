//! Loading a plugin's CLAP entry.
//!
//! **On Linux a plugin's library is never unmapped.** A plugin built with Rust registers per-thread
//! destructors through its own copy of std the first time a host thread calls into it. If the
//! library is unmapped (`dlclose`) while that thread lives on, glibc later calls a destructor whose
//! code is gone, and the thread crashes as it exits (`__nptl_deallocate_tsd` jumping to an unmapped
//! address). Found 2026-10-06 on Ubuntu 24.04's glibc 2.39, where a test binary that loaded and
//! dropped a plugin crashed at exit about one run in six; Arch's glibc 2.44 never did in twenty.
//!
//! So the library is opened with `RTLD_NODELETE`: dropping the entry still deinitialises the plugin
//! as CLAP requires, but its code stays mapped, as most hosts leave it. Windows and macOS load it as
//! clack-host always has. Every load goes through here, the test harness's included
//! (`mxm-player-harness`): one direct `PluginEntry::load` left elsewhere is the crash back.
//!
//! **A `.vst3` is a CLAP entry too.** mxm-kit's `mxm-vst3-host` offers a VST3 module as an
//! in-process CLAP entry, which clack-host loads like any other (`PluginEntry::load_from_clack`),
//! so everything past this function treats a VST3 plugin as the CLAP plugin it is shown as. It
//! keeps the same never-unmapped rule for the module's library on Linux.

use clack_host::entry::{PluginEntry, PluginEntryError};
use std::path::Path;

/// Loads and initialises the CLAP entry of the plugin bundle at `bundle`, or the CLAP entry a
/// VST3 bundle is offered as.
///
/// # Safety
///
/// As [`PluginEntry::load`]: loading a library runs its code.
pub unsafe fn load(bundle: &Path) -> Result<PluginEntry, PluginEntryError> {
    if mxm_vst3_host::is_bundle(bundle) {
        let path = std::ffi::CString::new(bundle.to_string_lossy().into_owned())?;
        return PluginEntry::load_from_clack::<mxm_vst3_host::Vst3Entry>(&path);
    }
    #[cfg(target_os = "linux")]
    {
        use clack_host::entry::LibraryEntry;
        use libloading::os::unix::Library;
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let path = CString::new(bundle.as_os_str().as_bytes())?;
        let flags = libc::RTLD_NOW | libc::RTLD_LOCAL | libc::RTLD_NODELETE;
        // SAFETY: the caller's, as for `PluginEntry::load`.
        let library = unsafe { Library::open(Some(bundle), flags) }
            .map_err(PluginEntryError::LibraryLoadingError)?;
        // SAFETY: `clap_entry` in a CLAP bundle is an entry descriptor, the format's whole contract.
        let entry = unsafe { LibraryEntry::load_from_library(library.into()) }?;
        // SAFETY: the entry was resolved from this bundle's library just above.
        unsafe { PluginEntry::load_from(entry, &path) }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: the caller's.
        unsafe { PluginEntry::load(bundle) }
    }
}
