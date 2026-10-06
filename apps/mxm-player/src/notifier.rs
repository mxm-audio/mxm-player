//! The one thread allowed to wake the GUI on someone else's behalf.
//!
//! `request_repaint()` reaches `Context::write()`, which takes an `RwLock` — so a plugin thread
//! or the audio callback must never call it. They store atomics; this thread watches those
//! atomics and calls `request_repaint()` for them.
//!
//! That keeps idle behaviour good — no polling cadence burning frames — while guaranteeing the
//! GUI is woken promptly. Only the GUI thread and this thread ever touch egui.

use crate::host::PlayerHostState;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the notifier looks. Short enough to feel immediate, long enough to be free.
const POLL_INTERVAL: Duration = Duration::from_millis(8);

pub struct Notifier {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Kept so the notifier can be pointed at a new plugin without the caller holding a context.
    ctx: egui::Context,
}

impl Notifier {
    /// Starts watching `state`, repainting `ctx` whenever something happens.
    pub fn spawn(ctx: egui::Context, state: Arc<PlayerHostState>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);

        let ctx_for_self = ctx.clone();
        let handle = std::thread::spawn(move || {
            let mut last = state.wake_generation();
            while !stop_for_thread.load(Ordering::Acquire) {
                let current = state.wake_generation();
                if current != last {
                    last = current;
                    ctx.request_repaint();
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        });

        Self {
            stop,
            handle: Some(handle),
            ctx: ctx_for_self,
        }
    }

    /// Points the notifier at a different plugin's state, after a plugin change.
    pub fn retarget(&mut self, ctx: egui::Context, state: Arc<PlayerHostState>) {
        let replacement = Notifier::spawn(ctx, state);
        let old = std::mem::replace(self, replacement);
        drop(old);
    }

    /// The same, reusing the context this notifier already has.
    pub fn retarget_state(&mut self, state: Arc<PlayerHostState>) {
        let ctx = self.ctx.clone();
        self.retarget(ctx, state);
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
