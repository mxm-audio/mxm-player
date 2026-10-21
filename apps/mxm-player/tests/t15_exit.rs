//! T15 — closing the player destroys every plugin and releases its library while the process
//! still runs.
//!
//! The engine once dropped the instance before the stream that held its audio processor, and
//! clack-host leaks an instance whose processor is still out: no plugin was destroyed, nor its
//! library unloaded, before the process exited. Windows then ran each library's shutdown after
//! ending every other thread, and Pigments, whose shutdown waits for a thread of its own, hung the
//! exit past even a kill (2026-10-21). A plugin's library still mapped after the app is gone is
//! that leak, whichever plugin it is, so mxm-mono-01 proves it here.

#[cfg(windows)]
#[test]
fn closing_the_player_unloads_its_plugins_library() {
    use mxm_player_harness::app_harness;
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    }
    let mapped = |path: &std::path::Path| {
        let wide: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: a NUL-terminated wide path; the call only looks the module up.
        !unsafe { GetModuleHandleW(wide.as_ptr()) }.is_null()
    };

    let Some(bundled) = app_harness::bundled_dir() else {
        eprintln!("skipped: run `cargo xtask bundle mxm-mono-01` first");
        return;
    };
    let bundle = bundled.join("mxm-mono-01.clap");
    let dir = std::env::temp_dir().join("mxm-player-t15-exit");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let config = mxm_player::config::PlayerConfig::sandboxed(
        &dir,
        Box::new(mxm_player::engine::audio::FakeBackend::new()),
    )
    .with_search_paths(vec![bundled.clone()]);
    let mut app = mxm_player::ui::PlayerApp::with_config(config);
    app.load(bundle.clone(), "dk.mxm.mxm-mono-01".to_owned());
    assert!(app.state().plugin.is_some(), "loaded");
    assert!(
        mapped(&bundle),
        "its library is in the process while it plays"
    );

    drop(app);
    assert!(
        !mapped(&bundle),
        "the library is still mapped after the player is gone: its instance was leaked"
    );
}
