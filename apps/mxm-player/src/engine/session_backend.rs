//! An audio backend that renders only when asked, so a session repeats exactly.
//!
//! # Why the free-running fake backend is not enough
//!
//! [`FakeBackend`](super::audio::FakeBackend) runs its callback on an unpaced thread. That is fine
//! for "does the engine reach `Running`", but it means an action races an arbitrary number of
//! callbacks: the same script produces a different number of rendered blocks each run, and events
//! land in different buffers. A virtual clock does not fix that — it fixes *timestamps*, not *how
//! many callbacks happened*.
//!
//! # Advancement and servicing are separate
//!
//! The subtlety that makes this work at all. `Engine::stop_now` queues `Command::Stop` and then
//! polls synchronously until the processor comes back, and that command is consumed only inside a
//! callback. Both `Engine::load` and `PlayerApp::rescan` call it. So a backend that rendered
//! *only* on request would deadlock: the driver calls `load`, `load` blocks, and the only thing
//! that could unblock it is a callback the blocked driver was supposed to issue. It would hang for
//! the wedge timeout and then report the plugin as wedged — a false diagnosis manufactured by the
//! harness.
//!
//! So:
//!
//! - **Audio advancement is script-driven.** Blocks are rendered only when the session grants
//!   them. This is what makes the captured audio byte-identical.
//! - **Command servicing is autonomous.** The stream thread services commands whenever it is not
//!   rendering, via [`AudioWorker::service_commands`], which renders nothing.
//!
//! Determinism survives because servicing produces no samples and does not consume a granted
//! block.

use super::audio::{Backend, DeviceChoice, ErrorSink};
use super::meters::{Meters, PriorityStatus};
use super::processor::AudioWorker;
use super::stream::AudioStream;
use crate::clock::Clock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long [`SessionControl::advance_blocks`] waits for the stream to catch up.
const ADVANCE_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared control of a stepped stream: how much to render, and what came out.
#[derive(Debug)]
pub struct SessionControl {
    /// Blocks the session has granted but the stream has not yet rendered.
    credits: AtomicU64,
    /// Blocks rendered since the stream started.
    rendered: AtomicU64,
    captured: Mutex<Vec<f32>>,
    clock: Arc<Clock>,
    frames_per_block: usize,
    channels: AtomicU64,
    sample_rate: Mutex<f64>,
    running: AtomicBool,
}

impl SessionControl {
    fn new(clock: Arc<Clock>, frames_per_block: usize) -> Self {
        Self {
            credits: AtomicU64::new(0),
            rendered: AtomicU64::new(0),
            captured: Mutex::new(Vec::new()),
            clock,
            frames_per_block,
            channels: AtomicU64::new(2),
            sample_rate: Mutex::new(48_000.0),
            running: AtomicBool::new(false),
        }
    }

    /// Renders `blocks` blocks and waits for them, advancing the virtual clock as it goes.
    ///
    /// Returns an error rather than hanging if the stream never catches up — a session that
    /// silently stalls is worse than one that says it did.
    pub fn advance_blocks(&self, blocks: u64) -> Result<(), String> {
        if !self.running.load(Ordering::Acquire) {
            return Err("no stream is running; start the engine first".to_owned());
        }

        let target = self.rendered.load(Ordering::Acquire) + blocks;
        self.credits.fetch_add(blocks, Ordering::AcqRel);

        let deadline = Instant::now() + ADVANCE_TIMEOUT;
        while self.rendered.load(Ordering::Acquire) < target {
            if Instant::now() > deadline {
                return Err(format!(
                    "the stream rendered {} of {target} blocks before timing out",
                    self.rendered.load(Ordering::Acquire)
                ));
            }
            std::thread::yield_now();
        }
        Ok(())
    }

    /// Everything rendered so far, interleaved.
    pub fn captured(&self) -> Vec<f32> {
        self.captured.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// The largest absolute sample rendered so far.
    pub fn peak(&self) -> f32 {
        self.captured
            .lock()
            .map(|c| c.iter().fold(0.0f32, |p, s| p.max(s.abs())))
            .unwrap_or(0.0)
    }

    pub fn clear_capture(&self) {
        if let Ok(mut c) = self.captured.lock() {
            c.clear();
        }
    }

    pub fn blocks_rendered(&self) -> u64 {
        self.rendered.load(Ordering::Acquire)
    }

    /// Whether a stream is running and therefore draining the worker's commands.
    ///
    /// Read by [`Session::advance_blocks`](crate::session::Session::advance_blocks) so that
    /// waiting for a publication to travel cannot wait for a worker that is not there — with no
    /// stream the block request itself is what should report the problem, and it does.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn frames_per_block(&self) -> usize {
        self.frames_per_block
    }

    pub fn channels(&self) -> usize {
        self.channels.load(Ordering::Acquire) as usize
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate.lock().map(|r| *r).unwrap_or(48_000.0)
    }
}

/// A [`Backend`] whose streams render only on request.
pub struct SessionBackend {
    control: Arc<SessionControl>,
}

impl SessionBackend {
    pub fn new(clock: Arc<Clock>, frames_per_block: usize) -> Self {
        Self {
            control: Arc::new(SessionControl::new(clock, frames_per_block)),
        }
    }

    /// The handle a session drives the stream through.
    pub fn control(&self) -> Arc<SessionControl> {
        Arc::clone(&self.control)
    }
}

impl Backend for SessionBackend {
    fn devices(&self) -> Vec<String> {
        vec!["session".to_owned()]
    }

    fn sample_rates(&self, _device: Option<&str>) -> Vec<u32> {
        vec![44_100, 48_000, 96_000]
    }

    fn open(
        &self,
        config: &super::AudioConfig,
        _plugin_channels: u32,
    ) -> Result<DeviceChoice, String> {
        Ok(DeviceChoice {
            name: "session".to_owned(),
            sample_rate: config.sample_rate.map(f64::from).unwrap_or(48_000.0),
            channel_count: 2,
            requested_buffer_size: Some(self.control.frames_per_block as u32),
        })
    }

    fn build(
        &self,
        choice: &DeviceChoice,
        worker: AudioWorker,
        errors: ErrorSink,
        meters: Arc<Meters>,
    ) -> Result<Box<dyn AudioStream>, String> {
        meters.record_priority(PriorityStatus::NotRequested);
        self.control
            .channels
            .store(choice.channel_count as u64, Ordering::Release);
        if let Ok(mut rate) = self.control.sample_rate.lock() {
            *rate = choice.sample_rate;
        }

        Ok(Box::new(SteppedStream::new(
            choice.clone(),
            worker,
            errors,
            Arc::clone(&self.control),
        )))
    }
}

/// A stream that renders granted blocks and services commands the rest of the time.
struct SteppedStream {
    choice: DeviceChoice,
    control: Arc<SessionControl>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    worker: Option<AudioWorker>,
    _errors: ErrorSink,
}

impl SteppedStream {
    fn new(
        choice: DeviceChoice,
        worker: AudioWorker,
        errors: ErrorSink,
        control: Arc<SessionControl>,
    ) -> Self {
        Self {
            choice,
            control,
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
            worker: Some(worker),
            _errors: errors,
        }
    }
}

impl AudioStream for SteppedStream {
    fn play(&mut self) -> Result<(), String> {
        let Some(mut worker) = self.worker.take() else {
            return Ok(());
        };

        let control = Arc::clone(&self.control);
        let stop = Arc::clone(&self.stop);
        let frames = control.frames_per_block;
        let channels = self.choice.channel_count;
        let sample_rate = self.choice.sample_rate;

        control.running.store(true, Ordering::Release);

        self.handle = Some(std::thread::spawn(move || {
            let mut buffer = vec![0.0f32; frames * channels];

            while !stop.load(Ordering::Acquire) {
                // Spend a granted block, if there is one.
                let granted = control
                    .credits
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |c| {
                        c.checked_sub(1).or(Some(0)).filter(|_| c > 0)
                    })
                    .is_ok();

                if granted {
                    buffer.fill(0.0);
                    worker.callback(&mut buffer, channels);

                    if let Ok(mut captured) = control.captured.lock() {
                        captured.extend_from_slice(&buffer);
                    }
                    // The clock moves with the audio, so event timestamps are reproducible.
                    control.clock.advance_frames(frames as u64, sample_rate);
                    control.rendered.fetch_add(1, Ordering::AcqRel);
                } else {
                    // Not rendering: service commands so a synchronous `stop_now` inside `load`
                    // or `rescan` can complete. This produces no samples, so the block count the
                    // script asked for is unaffected.
                    worker.service_commands();
                    std::thread::yield_now();
                }
            }

            control.running.store(false, Ordering::Release);
            // Dropping the worker runs the owner's `Drop`, handing the processor back.
            drop(worker);
        }));

        Ok(())
    }

    fn device_name(&self) -> String {
        self.choice.name.clone()
    }

    fn sample_rate(&self) -> f64 {
        self.choice.sample_rate
    }

    fn channel_count(&self) -> usize {
        self.choice.channel_count
    }
}

impl Drop for SteppedStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
