//! Layer 3: the whole application, driven headlessly and reproducibly.
//!
//! A **library driver**, not a shipped feature — there is no CLI flag, no script format and no
//! parser. Rust tests construct a [`Session`], step it, and assert; an agent investigating a
//! problem does the same in a throwaway test rather than taking a screenshot.
//!
//! It drives the *real* `PlayerApp` through the same servicing lifecycle the window uses
//! ([`PlayerApp::service`]), so what it proves is true of the path a user takes. Two
//! implementations would drift until a session test passed on a path nobody runs.
//!
//! # What is reproducible, and what is not
//!
//! - **Rendered audio is byte-identical** between runs of the same session. Audio advances only
//!   when [`Session::advance_blocks`] grants it, and the virtual clock moves with it.
//! - **Timing figures are not, and never can be.** `Meters` divides wall-clock durations. They are
//!   carried as diagnostics and excluded from comparison — see
//!   [`PlayerState::without_timing`](crate::state::PlayerState::without_timing).

use crate::clock::Clock;
use crate::config::PlayerConfig;
use crate::engine::session_backend::{SessionBackend, SessionControl};
use crate::state::PlayerState;
use crate::ui::PlayerApp;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many frames one advanced block renders.
pub const FRAMES_PER_BLOCK: usize = 512;

/// How long [`Session::advance_blocks`] waits for an edit to reach the worker before giving up.
///
/// Generous, because it is not a timing assertion — it is a guard against hanging if the worker
/// stops draining. The wait it bounds is normally over in one turn of the loop.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// The app, a clock, and a stream that renders on request.
pub struct Session {
    app: PlayerApp,
    control: Arc<SessionControl>,
    clock: Arc<Clock>,
    dir: PathBuf,
}

impl Session {
    /// A session confined to `dir`, searching only `search_paths` for plugins.
    pub fn new(dir: impl Into<PathBuf>, search_paths: Vec<PathBuf>) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(&dir);

        let clock = Clock::virtual_clock();
        let backend = SessionBackend::new(Arc::clone(&clock), FRAMES_PER_BLOCK);
        let control = backend.control();

        let config = PlayerConfig::sandboxed(&dir, Box::new(backend))
            .with_search_paths(search_paths)
            .with_clock(Arc::clone(&clock));

        Self {
            app: PlayerApp::with_config(config),
            control,
            clock,
            dir,
        }
    }

    /// A session in a uniquely named scratch directory.
    pub fn scratch(name: &str, search_paths: Vec<PathBuf>) -> Self {
        let dir = std::env::temp_dir().join(format!("mxm-player-session-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        Self::new(dir, search_paths)
    }

    pub fn app(&mut self) -> &mut PlayerApp {
        &mut self.app
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn clock(&self) -> &Arc<Clock> {
        &self.clock
    }

    /// Scans, then loads a plugin and starts audio.
    ///
    /// Both steps go through `stop_now`, which is exactly the re-entrancy the stepped backend
    /// services autonomously — see `engine::session_backend`.
    pub fn load(&mut self, bundle: impl Into<PathBuf>, plugin_id: &str) {
        self.app.load(bundle.into(), plugin_id.to_owned());
        self.app.service();
    }

    /// Renders `blocks` blocks, servicing the app between them the way a frame would.
    ///
    /// # Why it services until the edits have travelled, rather than once
    ///
    /// [`PlayerApp::publish_sequencer`] keeps at most one publication outstanding, so a frame that
    /// finds the worker still holding the previous state publishes nothing and leaves the edit for
    /// the next frame. In the window that is invisible: the next frame is sixteen milliseconds
    /// away and the state travels then. Here the next thing that happens is a *block*, rendered
    /// from whatever the worker last adopted — so a single `service` could grant a block that
    /// still had the old transport, the old loop window and the old pattern.
    ///
    /// **This is what made `loop_bar_keeps_the_playhead_inside_the_selected_bar` flaky.** It fails
    /// only when the audio thread has not been scheduled between the load's publication and the
    /// test's first block, which is why it took a fully loaded machine running the whole suite in
    /// parallel to show it: the playhead read step 0, the transport never having started.
    ///
    /// Waiting costs nothing when there is nothing to wait for — the common case leaves the loop
    /// on its first pass, exactly as the old single call did.
    pub fn advance_blocks(&mut self, blocks: u64) -> Result<(), String> {
        for _ in 0..blocks {
            self.settle()?;
            self.control.advance_blocks(1)?;
        }
        self.app.service();
        Ok(())
    }

    /// Services frames until every sequencer edit has been handed to the worker.
    ///
    /// A published command is adopted by the block that follows it — `Processor::callback` drains
    /// the queue before it renders — so reaching the queue is the whole of what has to happen
    /// before a block can be granted. Waiting for the worker's acknowledgement too would be
    /// waiting for something the render itself performs.
    fn settle(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        loop {
            self.app.service();
            // With no stream there is nobody to drain the queue, and the block request that
            // follows says so far better than a timeout here would.
            if !self.app.sequencer_publish_pending() || !self.control.is_running() {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(
                    "the sequencer state never reached the worker; it stopped draining commands"
                        .to_owned(),
                );
            }
            std::thread::yield_now();
        }
    }

    /// Everything rendered so far, interleaved.
    pub fn captured(&self) -> Vec<f32> {
        self.control.captured()
    }

    /// The largest absolute sample rendered so far.
    pub fn peak(&self) -> f32 {
        self.control.peak()
    }

    pub fn clear_capture(&self) {
        self.control.clear_capture();
    }

    pub fn blocks_rendered(&self) -> u64 {
        self.control.blocks_rendered()
    }

    /// Everything the player is showing, as data.
    ///
    /// Parameters are requeried first: a session has no frames, so nothing else would refresh
    /// them, and a dump reporting load-time values reads as though nothing had been changed.
    pub fn state(&mut self) -> PlayerState {
        self.app.refresh_params();
        self.app.state()
    }

    /// Writes the captured audio and the state dump side by side, for inspection.
    ///
    /// The two artifacts an investigation actually wants: what came out, and what the player
    /// thought was going on when it did.
    pub fn write_artifacts(&mut self, name: &str) -> Result<(PathBuf, PathBuf), String> {
        let wav = self.dir.join(format!("{name}.wav"));
        let json = self.dir.join(format!("{name}.json"));

        // **The state first.** The encoder refuses a non-finite sample, and a session is often run to
        // investigate exactly that plugin: the dump survives, and the error names the sample.
        let state = self.state();
        std::fs::write(&json, state.to_json()).map_err(|e| e.to_string())?;

        mxm_audio_file::write(
            &wav,
            &self.captured(),
            self.control.channels() as u16,
            self.control.sample_rate() as u32,
            mxm_audio_file::Target::WavFloat32,
        )
        .map_err(|e| format!("{e}; the state dump was written to {}", json.display()))?;

        Ok((wav, json))
    }
}
