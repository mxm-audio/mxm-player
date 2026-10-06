//! A plugin's editor embedded the way a DAW embeds it on macOS, resized from the host's side.
//!
//! MXM Player opens editors floating, so it never takes the path Bitwig, Reaper or Live take on
//! macOS: the host owns the window, hands the plugin an `NSView` through CLAP's Cocoa API and calls
//! `set_size` when the user resizes it. CLAP measures those sizes in **points** on Cocoa. After
//! `set_size(w, h)` the plugin must report `get_size == (w, h)`, and its view must be `w`×`h`
//! points. nice-plug 0.3.0 used pixels there: on a Retina screen the view came out at half the
//! size and `get_size` reported double, so editors scaled instead of reflowing (the owner saw it
//! in Bitwig, 2026-10-06). This is the check that would have caught it, and the macOS counterpart of
//! the collection's Windows `editor_resize`.
//!
//! ```text
//! cargo run -p mxm-player-harness --example embedded_resize -- <bundle.clap> <plugin id>
//! ```
//!
//! Run it in the logged-in user's session (a Terminal on the Mac): it opens a real window for a
//! couple of seconds. It prints one line per step and exits non-zero on any mismatch.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("embedded_resize checks CLAP's Cocoa embedding, so it runs on macOS only");
}

#[cfg(target_os = "macos")]
fn main() {
    std::process::exit(mac::run());
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::PathBuf;

    use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiSize, PluginGui, Window};
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSView, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoop, NSSize};

    /// The sizes the host resizes to, in points: larger, much larger, then smaller again.
    const SIZES: [(u32, u32); 3] = [(900, 640), (1300, 900), (700, 500)];

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().collect();
        let [_, bundle, id] = args.as_slice() else {
            eprintln!("usage: embedded_resize <bundle.clap> <plugin id>");
            return 2;
        };
        let bundle = PathBuf::from(bundle);

        let mtm = MainThreadMarker::new().expect("runs on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        app.finishLaunching();

        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Resizable;
        let frame = NSRect::new(NSPoint::new(120.0, 120.0), NSSize::new(640.0, 480.0));
        // SAFETY: a fresh window on the main thread, kept alive until the end of this function.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: the window is owned by `window` (a `Retained`), not by AppKit's close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.makeKeyAndOrderFront(None);
        let content = window
            .contentView()
            .expect("a titled window has a content view");
        println!("backing scale factor {}", window.backingScaleFactor());

        // SAFETY: the bundle path is the caller's; loading runs its entry point once.
        let entry = match unsafe { mxm_player::entry::load(&bundle) } {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("cannot load {}: {e}", bundle.display());
                return 2;
            }
        };
        let (_state, mut instance) = match mxm_player_harness::harness::instantiate(&entry, id) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("cannot instantiate {id}: {e}");
                return 2;
            }
        };
        let Some(gui) = instance.plugin_shared_handle().get_extension::<PluginGui>() else {
            eprintln!("{id} has no GUI extension");
            return 2;
        };
        let mut handle = instance.plugin_handle();
        let config = GuiConfiguration {
            api_type: GuiApiType::COCOA,
            is_floating: false,
        };
        if !gui.is_api_supported(&mut handle, config) || gui.create(&mut handle, config).is_err() {
            eprintln!("{id} refuses an embedded Cocoa editor");
            return 1;
        }

        // A DAW sizes its container to what the plugin asks for, then embeds.
        let mut failures = 0;
        let initial = gui.get_size(&mut handle);
        if let Some(size) = initial {
            window.setContentSize(NSSize::new(f64::from(size.width), f64::from(size.height)));
        }
        let parent = Window::from_cocoa_nsview(Retained::as_ptr(&content) as *mut _);
        // SAFETY: `content` outlives the editor: `destroy` runs before this function returns.
        if unsafe { gui.set_parent(&mut handle, parent) }.is_err() {
            eprintln!("{id} refused the parent view");
            gui.destroy(&mut handle);
            return 1;
        }
        let _ = gui.show(&mut handle);
        pump(0.5);
        failures += report(
            "open",
            initial,
            initial.is_some(),
            &gui,
            &mut handle,
            &content,
        );

        for (width, height) in SIZES {
            let asked = GuiSize { width, height };
            let size = gui.adjust_size(&mut handle, asked).unwrap_or(asked);
            window.setContentSize(NSSize::new(f64::from(size.width), f64::from(size.height)));
            let accepted = gui.set_size(&mut handle, size).is_ok();
            pump(0.4);
            let step = format!("resize to {width}x{height}");
            failures += report(&step, Some(size), accepted, &gui, &mut handle, &content);
        }

        gui.destroy(&mut handle);
        // The plugin goes before the window it was embedded in.
        drop(instance);
        window.close();
        println!("{}", if failures == 0 { "PASS" } else { "FAIL" });
        failures
    }

    /// One line: what the host set, what the plugin reports, and the plugin view's real frame.
    fn report(
        step: &str,
        expected: Option<GuiSize>,
        accepted: bool,
        gui: &PluginGui,
        handle: &mut clack_host::plugin::PluginMainThreadHandle<'_>,
        content: &NSView,
    ) -> i32 {
        let reported = gui.get_size(handle);
        let view = plugin_view_size(content);
        let ok = accepted
            && expected.is_some()
            && reported == expected
            && expected.map(|s| (f64::from(s.width), f64::from(s.height))) == view;
        println!(
            "{} {step}: host {:?}, accepted {accepted}, plugin reports {:?}, plugin view {:?} points",
            if ok { "ok  " } else { "FAIL" },
            expected.map(|s| (s.width, s.height)),
            reported.map(|s| (s.width, s.height)),
            view,
        );
        i32::from(!ok)
    }

    /// The frame of the plugin's view: the one subview it added to the host's content view.
    fn plugin_view_size(content: &NSView) -> Option<(f64, f64)> {
        let view = content.subviews().firstObject()?;
        let frame = view.frame();
        Some((frame.size.width, frame.size.height))
    }

    /// Lets AppKit and the editor's own timers run, as a host's event loop would.
    fn pump(seconds: f64) {
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(seconds));
    }
}
