//! Three distinct measurements, deliberately not conflated.
//!
//! * **Plugin load** — time inside `process()` as a percentage of the buffer's wall-clock budget.
//! * **Callback load** — the whole callback, including conversion and event merging. The gap
//!   between the two says whether the player or the plugin is at fault.
//! * **Missed deadlines** — inferred when a callback overruns. Shown separately from backend
//!   xruns, which WASAPI does not reliably report; where the backend reports nothing the UI says
//!   so rather than showing a reassuring zero.
//!
//! Everything accumulates into atomics on the audio thread; the GUI only reads.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Load figures, in parts per million of the buffer budget, so they fit an integer atomic.
const PPM: f64 = 1_000_000.0;

#[derive(Debug, Default)]
pub struct Meters {
    plugin_load_ppm: AtomicU32,
    /// Time inside the effect chain's `process()` calls, separate from the source's: one
    /// aggregate "plugin" figure would make a second slot impossible to diagnose.
    fx_load_ppm: AtomicU32,
    callback_load_ppm: AtomicU32,
    peak_callback_load_ppm: AtomicU32,
    missed_deadlines: AtomicU64,
    callbacks: AtomicU64,
    frames_processed: AtomicU64,
    /// Whether the audio backend reports xruns at all. When it does not, the UI says "not
    /// reported" rather than showing a reassuring zero.
    xruns_reported: AtomicBool,
    xruns: AtomicU64,
    /// Realtime priority: whether promotion was requested, and what came of it. Surfaced rather
    /// than silently ignored, and honest about the case where the backend reports nothing.
    priority: AtomicU32,
}

/// What became of the audio thread's realtime-priority promotion.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum PriorityStatus {
    /// No stream is running, or the backend was never asked.
    NotRequested,
    /// Promotion was requested and the backend reports no outcome either way.
    ///
    /// This is the honest state on WASAPI through CPAL: the boost is applied inside the
    /// backend's worker thread and nothing is returned to the host. The UI says "requested"
    /// rather than showing a reassuring "yes".
    Requested,
    /// Promotion was requested and confirmed.
    Confirmed,
    /// Promotion was requested and failed.
    Failed,
}

impl PriorityStatus {
    fn from_raw(raw: u32) -> Self {
        match raw {
            1 => PriorityStatus::Requested,
            2 => PriorityStatus::Confirmed,
            3 => PriorityStatus::Failed,
            _ => PriorityStatus::NotRequested,
        }
    }

    fn to_raw(self) -> u32 {
        match self {
            PriorityStatus::NotRequested => 0,
            PriorityStatus::Requested => 1,
            PriorityStatus::Confirmed => 2,
            PriorityStatus::Failed => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PriorityStatus::NotRequested => "not requested",
            PriorityStatus::Requested => "requested (backend reports no outcome)",
            PriorityStatus::Confirmed => "promoted",
            PriorityStatus::Failed => "promotion failed",
        }
    }
}

impl Meters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one callback. `budget` is the wall-clock time the delivered frames represent;
    /// `plugin_time` is the source's and `fx_time` the effect chain's, kept apart.
    pub fn record_callback(
        &self,
        plugin_time: std::time::Duration,
        fx_time: std::time::Duration,
        callback_time: std::time::Duration,
        budget: std::time::Duration,
        frames: u64,
    ) {
        let budget_secs = budget.as_secs_f64();
        if budget_secs <= 0.0 {
            return;
        }

        let plugin = (plugin_time.as_secs_f64() / budget_secs * PPM) as u32;
        let fx = (fx_time.as_secs_f64() / budget_secs * PPM) as u32;
        let callback = (callback_time.as_secs_f64() / budget_secs * PPM) as u32;

        self.plugin_load_ppm.store(plugin, Ordering::Relaxed);
        self.fx_load_ppm.store(fx, Ordering::Relaxed);
        self.callback_load_ppm.store(callback, Ordering::Relaxed);
        self.peak_callback_load_ppm
            .fetch_max(callback, Ordering::Relaxed);
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        self.frames_processed.fetch_add(frames, Ordering::Relaxed);

        // A callback that took longer than the audio it produced has missed its deadline. That
        // is inference, not a backend report, and it is labelled as such in the UI.
        if callback_time > budget {
            self.missed_deadlines.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn plugin_load(&self) -> f32 {
        self.plugin_load_ppm.load(Ordering::Relaxed) as f32 / PPM as f32
    }

    /// The effect chain's share of the budget, apart from the source's.
    pub fn fx_load(&self) -> f32 {
        self.fx_load_ppm.load(Ordering::Relaxed) as f32 / PPM as f32
    }

    pub fn callback_load(&self) -> f32 {
        self.callback_load_ppm.load(Ordering::Relaxed) as f32 / PPM as f32
    }

    pub fn peak_callback_load(&self) -> f32 {
        self.peak_callback_load_ppm.load(Ordering::Relaxed) as f32 / PPM as f32
    }

    pub fn missed_deadlines(&self) -> u64 {
        self.missed_deadlines.load(Ordering::Relaxed)
    }

    pub fn callbacks(&self) -> u64 {
        self.callbacks.load(Ordering::Relaxed)
    }

    pub fn frames_processed(&self) -> u64 {
        self.frames_processed.load(Ordering::Relaxed)
    }

    /// `None` means the backend does not report xruns, which is the honest answer on WASAPI.
    pub fn xruns(&self) -> Option<u64> {
        self.xruns_reported
            .load(Ordering::Relaxed)
            .then(|| self.xruns.load(Ordering::Relaxed))
    }

    pub fn record_xrun(&self) {
        self.xruns_reported.store(true, Ordering::Relaxed);
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    /// Records what became of the realtime-priority promotion.
    pub fn record_priority(&self, status: PriorityStatus) {
        self.priority.store(status.to_raw(), Ordering::Relaxed);
    }

    pub fn realtime_priority(&self) -> PriorityStatus {
        PriorityStatus::from_raw(self.priority.load(Ordering::Relaxed))
    }

    pub fn reset_peaks(&self) {
        self.peak_callback_load_ppm.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn plugin_and_callback_load_are_reported_separately() {
        let meters = Meters::new();
        meters.record_callback(
            Duration::from_micros(500),
            Duration::from_micros(100),
            Duration::from_micros(800),
            Duration::from_micros(1000),
            48,
        );

        assert!((meters.plugin_load() - 0.5).abs() < 1e-3);
        assert!(
            (meters.fx_load() - 0.1).abs() < 1e-3,
            "the chain's share is its own"
        );
        assert!((meters.callback_load() - 0.8).abs() < 1e-3);
        assert_eq!(
            meters.missed_deadlines(),
            0,
            "a callback inside its budget has not missed a deadline"
        );
    }

    #[test]
    fn an_overrun_is_counted_as_a_missed_deadline() {
        let meters = Meters::new();
        meters.record_callback(
            Duration::from_micros(1200),
            Duration::ZERO,
            Duration::from_micros(1500),
            Duration::from_micros(1000),
            48,
        );
        assert_eq!(meters.missed_deadlines(), 1);
    }

    #[test]
    fn unreported_xruns_read_as_unknown_not_as_zero() {
        let meters = Meters::new();
        assert_eq!(
            meters.xruns(),
            None,
            "a backend that reports nothing must not look like a backend reporting zero"
        );
        meters.record_xrun();
        assert_eq!(meters.xruns(), Some(1));
    }

    #[test]
    fn priority_promotion_is_reported_honestly_including_when_nothing_is_known() {
        let meters = Meters::new();
        assert_eq!(meters.realtime_priority(), PriorityStatus::NotRequested);

        meters.record_priority(PriorityStatus::Requested);
        assert_eq!(meters.realtime_priority(), PriorityStatus::Requested);
        assert!(
            meters.realtime_priority().label().contains("no outcome"),
            "an unknown outcome must not read as a confirmed promotion"
        );

        meters.record_priority(PriorityStatus::Failed);
        assert_eq!(meters.realtime_priority(), PriorityStatus::Failed);
    }
}
