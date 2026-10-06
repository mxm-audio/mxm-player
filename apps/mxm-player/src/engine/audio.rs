//! The audio backend, behind a trait.
//!
//! The trait is not abstraction for its own sake: CPAL's output callback returns `()`,
//! `process_output` is private, and its errors arise inside backend operations, so no fixture
//! and no player code can inject a terminal failure through any public API. Driving the
//! `StreamExited` path honestly needs a seam we own, and this is it.

use crate::engine::AudioConfig;
use crate::engine::meters::Meters;
use crate::engine::processor::AudioWorker;
use crate::engine::stream::AudioStream;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

/// A device the backend has resolved and agreed a format with.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceChoice {
    pub name: String,
    pub sample_rate: f64,
    pub channel_count: usize,
    /// What was actually asked for. `BufferSize::Fixed` is only a request.
    pub requested_buffer_size: Option<u32>,
}

/// Where terminal stream errors are recorded for display. The error callback does no work
/// beyond storing the cause.
pub type ErrorSink = Arc<Mutex<Option<String>>>;

/// A backend error in words a person can act on, with the backend's own text kept after it.
///
/// CPAL classifies what the platform said — WASAPI's `AUDCLNT_E_DEVICE_INVALIDATED` becomes
/// `DeviceNotAvailable`, `AUDCLNT_E_DEVICE_IN_USE` becomes `DeviceBusy` — but its message is the
/// raw HRESULT, and on Windows that reads as `OS Error -2004287484 (FormatMessageW() returned
/// error 317)`. That is what the status bar showed the day another application took the device
/// away. The classification is what the user needs; the raw text stays in parentheses because it
/// is what a bug report needs.
pub fn describe(error: &cpal::Error) -> String {
    use cpal::ErrorKind;
    let meaning = match error.kind() {
        ErrorKind::DeviceNotAvailable => Some(
            "Windows disconnected the audio device: another application took exclusive control, \
             its sample rate or clock changed, or it was unplugged",
        ),
        ErrorKind::DeviceBusy => {
            Some("the audio device is held in exclusive mode by another application")
        }
        ErrorKind::StreamInvalidated => {
            Some("the audio device was reconfigured and the stream must be rebuilt")
        }
        ErrorKind::HostUnavailable => Some("the audio service is not running"),
        ErrorKind::PermissionDenied => Some("the audio device refused access"),
        _ => None,
    };
    match meaning {
        Some(meaning) => format!("{meaning} ({error})"),
        None => error.to_string(),
    }
}

pub trait Backend {
    /// Output devices, by name. The default device is listed first.
    fn devices(&self) -> Vec<String>;
    /// Sample rates the named device advertises for output.
    fn sample_rates(&self, device: Option<&str>) -> Vec<u32>;
    /// Resolves a device and agrees a format that can carry `plugin_channels`.
    fn open(&self, config: &AudioConfig, plugin_channels: u32) -> Result<DeviceChoice, String>;
    /// Builds a running stream driven by `worker`.
    fn build(
        &self,
        choice: &DeviceChoice,
        worker: AudioWorker,
        errors: ErrorSink,
        meters: Arc<Meters>,
    ) -> Result<Box<dyn AudioStream>, String>;
}

/// The real backend: CPAL, in shared mode. No exclusive-mode control is offered, because that is
/// not what CPAL's WASAPI backend does.
pub struct CpalBackend {
    host: cpal::Host,
}

impl CpalBackend {
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }

    fn resolve(&self, name: Option<&str>) -> Result<cpal::Device, String> {
        match name {
            None => self
                .host
                .default_output_device()
                .ok_or_else(|| "no default audio output device".to_owned()),
            Some(name) => self
                .host
                .output_devices()
                .map_err(|e| describe(&e))?
                .find(|d| d.to_string() == name)
                .ok_or_else(|| format!("no audio output device named `{name}`")),
        }
    }
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for CpalBackend {
    fn devices(&self) -> Vec<String> {
        let default = self.host.default_output_device().map(|d| d.to_string());

        let mut names: Vec<String> = self
            .host
            .output_devices()
            .map(|devices| devices.map(|d| d.to_string()).collect())
            .unwrap_or_default();

        if let Some(default) = default {
            names.retain(|n| *n != default);
            names.insert(0, default);
        }
        names
    }

    fn sample_rates(&self, device: Option<&str>) -> Vec<u32> {
        let Ok(device) = self.resolve(device) else {
            return Vec::new();
        };
        let Ok(configs) = device.supported_output_configs() else {
            return Vec::new();
        };

        let mut rates: Vec<u32> = configs
            .flat_map(|c| {
                [
                    c.min_sample_rate(),
                    c.max_sample_rate(),
                    44_100,
                    48_000,
                    88_200,
                    96_000,
                ]
                .into_iter()
                .filter(move |rate| *rate >= c.min_sample_rate() && *rate <= c.max_sample_rate())
            })
            .collect();
        rates.sort_unstable();
        rates.dedup();
        rates
    }

    fn open(&self, config: &AudioConfig, plugin_channels: u32) -> Result<DeviceChoice, String> {
        let device = self.resolve(config.device_name.as_deref())?;
        let name = device.to_string();
        let default = device.default_output_config().map_err(|e| describe(&e))?;

        // 32-bit float only, which is the whole envelope: no format conversion is involved.
        if default.sample_format() != cpal::SampleFormat::F32 {
            return Err(format!(
                "`{name}` offers {} samples; the player is 32-bit float only",
                default.sample_format()
            ));
        }

        let sample_rate = config
            .sample_rate
            .map(f64::from)
            .unwrap_or_else(|| f64::from(default.sample_rate()));

        // A mono plugin driving a stereo device is fine — the render duplicates. More device
        // channels than that would leave channels unaccounted for, so the device's own count is
        // what the callback interleaves to.
        let channel_count = usize::from(default.channels()).max(plugin_channels as usize);

        Ok(DeviceChoice {
            name,
            sample_rate,
            channel_count,
            requested_buffer_size: config.buffer_size,
        })
    }

    fn build(
        &self,
        choice: &DeviceChoice,
        mut worker: AudioWorker,
        errors: ErrorSink,
        meters: Arc<Meters>,
    ) -> Result<Box<dyn AudioStream>, String> {
        let device = self.resolve(Some(&choice.name))?;
        let default = device.default_output_config().map_err(|e| describe(&e))?;

        let config = cpal::StreamConfig {
            channels: default.channels(),
            sample_rate: choice.sample_rate as u32,
            buffer_size: match choice.requested_buffer_size {
                // Only ever a *request*: the backend may deliver any frame count, and the
                // callback chunks anything oversized without allocating.
                Some(frames) => cpal::BufferSize::Fixed(frames),
                None => cpal::BufferSize::Default,
            },
        };

        let channel_count = usize::from(config.channels);
        let error_sink = Arc::clone(&errors);

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    worker.callback(data, channel_count);
                },
                move |error| {
                    // Records the cause for display; does no work beyond storing it. The
                    // translation is a `match` and a `format!`, on a thread that is exiting.
                    if let Ok(mut slot) = error_sink.lock() {
                        *slot = Some(describe(&error));
                    }
                },
                None,
            )
            .map_err(|e| describe(&e))?;

        // CPAL's `realtime` feature is what promotes the worker thread; the backend reports no
        // outcome, and the meter says so rather than claiming success.
        meters.record_priority(crate::engine::meters::PriorityStatus::Requested);

        Ok(Box::new(CpalStream {
            stream,
            name: choice.name.clone(),
            sample_rate: choice.sample_rate,
            channel_count,
        }))
    }
}

struct CpalStream {
    stream: cpal::Stream,
    name: String,
    sample_rate: f64,
    channel_count: usize,
}

// SAFETY: `cpal::Stream` is not `Send` on every platform because some backends tie it to the
// thread that created it. The player only ever creates, plays and drops a stream on the GUI
// thread; the box is `Send` solely so the engine can hold it behind a `dyn AudioStream`.
unsafe impl Send for CpalStream {}

impl AudioStream for CpalStream {
    fn play(&mut self) -> Result<(), String> {
        self.stream.play().map_err(|e| describe(&e))
    }

    fn device_name(&self) -> String {
        self.name.clone()
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn channel_count(&self) -> usize {
        self.channel_count
    }
}

/// What [`FakeBackend::build`] says when told to refuse.
pub const REFUSED_BUILD: &str = "the fake backend refused to build a stream";

/// A backend that runs the callback on a thread we control, for tests.
///
/// It can be told to exit its worker loop the way a terminal WASAPI failure does — with no
/// further data callback — which is exactly the case CPAL gives no way to provoke.
pub struct FakeBackend {
    pub sample_rate: f64,
    pub channel_count: usize,
    pub frames_per_callback: usize,
    /// Set to make the worker loop exit with no further data callback, exactly as CPAL's does
    /// after a fatal backend error.
    pub kill: Arc<std::sync::atomic::AtomicBool>,
    /// Set to make `build` fail, the way a device held in exclusive mode by another application
    /// makes CPAL's fail. The worker is dropped inside the refused build, exactly as CPAL drops
    /// its closures, so the processor comes back through the owner's `Drop`.
    pub refuse_build: Arc<std::sync::atomic::AtomicBool>,
    /// Everything the callback rendered, so a test can assert that a note actually sounded.
    pub captured: Arc<Mutex<Vec<f32>>>,
    /// How long the stream sleeps between callbacks. Zero runs as fast as it can, which is what
    /// a test wants.
    pub realtime: bool,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self {
            sample_rate: 48_000.0,
            channel_count: 2,
            frames_per_callback: 256,
            kill: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            refuse_build: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            captured: Arc::new(Mutex::new(Vec::new())),
            realtime: false,
        }
    }

    /// Simulates the terminal backend failure CPAL gives no way to provoke.
    pub fn kill(&self) {
        self.kill.store(true, Ordering::Release);
    }

    /// Lets the next stream live again after [`kill`](Self::kill).
    pub fn revive(&self) {
        self.kill.store(false, Ordering::Release);
    }

    /// Whether `build` refuses, the way it does while another application holds the device.
    pub fn refuse_builds(&self, refuse: bool) {
        self.refuse_build.store(refuse, Ordering::Release);
    }

    /// The largest absolute sample the callback has rendered so far.
    pub fn peak(&self) -> f32 {
        self.captured
            .lock()
            .map(|samples| samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs())))
            .unwrap_or(0.0)
    }

    pub fn frames_rendered(&self) -> usize {
        self.captured.lock().map(|s| s.len()).unwrap_or(0) / self.channel_count.max(1)
    }

    pub fn clear_capture(&self) {
        if let Ok(mut samples) = self.captured.lock() {
            samples.clear();
        }
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for FakeBackend {
    fn devices(&self) -> Vec<String> {
        vec!["fake output".to_owned()]
    }

    fn sample_rates(&self, _device: Option<&str>) -> Vec<u32> {
        vec![44_100, 48_000]
    }

    fn open(&self, config: &AudioConfig, _plugin_channels: u32) -> Result<DeviceChoice, String> {
        Ok(DeviceChoice {
            name: "fake output".to_owned(),
            sample_rate: config
                .sample_rate
                .map(f64::from)
                .unwrap_or(self.sample_rate),
            channel_count: self.channel_count,
            requested_buffer_size: config.buffer_size,
        })
    }

    fn build(
        &self,
        choice: &DeviceChoice,
        worker: AudioWorker,
        errors: ErrorSink,
        meters: Arc<Meters>,
    ) -> Result<Box<dyn AudioStream>, String> {
        if self.refuse_build.load(Ordering::Acquire) {
            // `worker` is dropped here, as CPAL drops its closures when `Initialize` fails.
            return Err(REFUSED_BUILD.to_owned());
        }
        meters.record_priority(crate::engine::meters::PriorityStatus::Requested);
        Ok(Box::new(FakeStream::new(
            choice.clone(),
            worker,
            errors,
            self.frames_per_callback,
            Arc::clone(&self.kill),
            Arc::clone(&self.captured),
            self.realtime,
        )))
    }
}

/// A stream whose worker loop the test drives, and can kill outright.
pub struct FakeStream {
    choice: DeviceChoice,
    frames: usize,
    /// Set to stop the worker loop. Dropping the stream also stops it.
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Set to make the worker loop exit *without* honouring any further command — the terminal
    /// backend failure CPAL cannot be asked for.
    kill: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _errors: ErrorSink,
    worker: Option<AudioWorker>,
    captured: Arc<Mutex<Vec<f32>>>,
    realtime: bool,
}

impl FakeStream {
    #[allow(clippy::too_many_arguments)]
    fn new(
        choice: DeviceChoice,
        worker: AudioWorker,
        errors: ErrorSink,
        frames: usize,
        kill: Arc<std::sync::atomic::AtomicBool>,
        captured: Arc<Mutex<Vec<f32>>>,
        realtime: bool,
    ) -> Self {
        Self {
            choice,
            frames,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kill,
            handle: None,
            _errors: errors,
            worker: Some(worker),
            captured,
            realtime,
        }
    }

    /// A handle a test can use to simulate a terminal backend failure.
    pub fn kill_switch(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.kill)
    }
}

impl AudioStream for FakeStream {
    fn play(&mut self) -> Result<(), String> {
        let Some(mut worker) = self.worker.take() else {
            return Ok(());
        };

        let running = Arc::clone(&self.running);
        let kill = Arc::clone(&self.kill);
        let captured = Arc::clone(&self.captured);
        let frames = self.frames;
        let channel_count = self.choice.channel_count;
        let sample_rate = self.choice.sample_rate;
        let realtime = self.realtime;

        running.store(true, Ordering::Release);
        self.handle = Some(std::thread::spawn(move || {
            let mut buffer = vec![0.0f32; frames * channel_count];
            while running.load(Ordering::Acquire) {
                if kill.load(Ordering::Acquire) {
                    // The worker loop exits with no further data callback, exactly as CPAL's
                    // does after a fatal backend error. Dropping `worker` here is what runs the
                    // owner's `Drop` and hands the processor back.
                    break;
                }
                buffer.fill(0.0);
                worker.callback(&mut buffer, channel_count);

                if let Ok(mut samples) = captured.lock() {
                    // Bounded, so a long test cannot grow without limit.
                    if samples.len() < 1 << 22 {
                        samples.extend_from_slice(&buffer);
                    }
                }

                if realtime {
                    std::thread::sleep(std::time::Duration::from_secs_f64(
                        frames as f64 / sample_rate,
                    ));
                } else {
                    std::thread::yield_now();
                }
            }
            // Dropping the worker here runs the owner's `Drop`, which performs the stop
            // transition and hands the processor back.
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

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text CPAL produced on Windows the day the device was taken away — kept verbatim, so
    /// the test says what the status bar used to show.
    const RAW: &str = "Failed to get current padding: OS Error -2004287484 \
                       (FormatMessageW() returned error 317) (os error -2004287484)";

    #[test]
    fn a_device_taken_away_is_described_in_words_with_the_raw_text_kept() {
        let error = cpal::Error::with_message(cpal::ErrorKind::DeviceNotAvailable, RAW);
        let described = describe(&error);
        assert!(
            described.starts_with("Windows disconnected the audio device"),
            "the meaning leads: {described}"
        );
        assert!(
            described.contains("exclusive control"),
            "the likeliest cause is named: {described}"
        );
        assert!(
            described.ends_with(&format!("({RAW})")),
            "the raw text stays: {described}"
        );
    }

    #[test]
    fn a_busy_device_names_the_application_holding_it() {
        let error =
            cpal::Error::with_message(cpal::ErrorKind::DeviceBusy, "AUDCLNT_E_DEVICE_IN_USE");
        assert!(describe(&error).starts_with("the audio device is held in exclusive mode"));
    }

    #[test]
    fn an_unclassified_error_passes_through_unchanged() {
        // No meaning is invented for a kind CPAL could not classify: guessing would be a lie
        // dressed as help.
        let error = cpal::Error::with_message(cpal::ErrorKind::BackendError, "something odd");
        assert_eq!(describe(&error), "something odd");
    }
}
