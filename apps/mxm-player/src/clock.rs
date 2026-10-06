//! One clock, shared by every event source.
//!
//! Every event is stamped with a host-monotonic arrival time **inside its own MIDI callback**,
//! before any queueing. That gives one timeline across all sources immediately, even though P2
//! still applies events at the buffer boundary — the timestamps are already comparable, so P4
//! can map them onto sample offsets without redefining anything.
//!
//! Deriving each connection's origin from its first message was considered and rejected:
//! `host_time_at_callback − raw_midi_timestamp` includes callback scheduling latency, so
//! different ports would acquire different offsets and the common timeline would be defeated.
//!
//! # The realtime contract
//!
//! This clock is read from three thread contexts: the **audio callback**
//! (`engine::processor::AudioWorker::callback`), the GUI thread, and **MIDI backend callbacks**.
//! Every read must therefore be **nonblocking, allocation-free, lock-free and I/O-free** —
//! blocking inside a MIDI callback stalls the backend thread, and allocating inside the audio
//! callback breaks the one rule this project treats as inviolable.
//!
//! That is why this is a closed enum rather than a trait object. A `dyn Fn() -> u64` would let a
//! caller install a clock that locks or allocates, and nothing would catch it until a dropout in
//! somebody's DAW. With two arms, both obviously wait-free, the contract holds by construction
//! instead of by convention.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A source of monotonic timestamps in nanoseconds.
#[derive(Debug)]
pub enum Clock {
    /// Real elapsed time since the player started. What production uses.
    Monotonic { epoch: Instant },
    /// Time that only moves when someone advances it, so a session renders identically twice.
    Virtual { nanos: AtomicU64 },
}

impl Clock {
    /// The production clock. Fixes its epoch now, so the first stamp is not also the first call.
    pub fn monotonic() -> Arc<Self> {
        Arc::new(Clock::Monotonic {
            epoch: Instant::now(),
        })
    }

    /// A clock that starts at zero and only moves when [`advance`](Self::advance) is called.
    pub fn virtual_clock() -> Arc<Self> {
        Arc::new(Clock::Virtual {
            nanos: AtomicU64::new(0),
        })
    }

    /// Nanoseconds since this clock's origin.
    ///
    /// Wait-free on both arms: `Instant::elapsed` is a vDSO/`QueryPerformanceCounter` read, and
    /// the virtual arm is a relaxed atomic load.
    #[inline]
    pub fn now_nanos(&self) -> u64 {
        match self {
            Clock::Monotonic { epoch } => epoch.elapsed().as_nanos() as u64,
            Clock::Virtual { nanos } => nanos.load(Ordering::Acquire),
        }
    }

    /// Moves a virtual clock forward. A no-op on the monotonic one, which moves by itself.
    pub fn advance(&self, by_nanos: u64) {
        if let Clock::Virtual { nanos } = self {
            nanos.fetch_add(by_nanos, Ordering::AcqRel);
        }
    }

    /// Moves a virtual clock forward by a number of frames at a sample rate.
    ///
    /// The unit a session actually thinks in: "render one more block".
    pub fn advance_frames(&self, frames: u64, sample_rate: f64) {
        if sample_rate > 0.0 {
            self.advance((frames as f64 / sample_rate * 1e9) as u64);
        }
    }

    /// Whether this clock moves on its own.
    pub fn is_virtual(&self) -> bool {
        matches!(self, Clock::Virtual { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_monotonic_clock_only_moves_forward() {
        let clock = Clock::monotonic();
        let a = clock.now_nanos();
        let b = clock.now_nanos();
        assert!(b >= a);
    }

    #[test]
    fn a_virtual_clock_moves_only_when_told_to() {
        let clock = Clock::virtual_clock();
        assert_eq!(clock.now_nanos(), 0);

        // However long a test takes, an unadvanced virtual clock has not moved.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(clock.now_nanos(), 0);

        clock.advance(1_000);
        assert_eq!(clock.now_nanos(), 1_000);
    }

    #[test]
    fn advancing_by_frames_matches_the_sample_rate() {
        let clock = Clock::virtual_clock();
        clock.advance_frames(48_000, 48_000.0);
        assert_eq!(clock.now_nanos(), 1_000_000_000, "one second of audio");

        clock.advance_frames(512, 48_000.0);
        let expected = 1_000_000_000 + (512.0 / 48_000.0 * 1e9) as u64;
        assert_eq!(clock.now_nanos(), expected);
    }

    #[test]
    fn advancing_a_monotonic_clock_is_a_no_op_rather_than_a_panic() {
        // Session code calls `advance` without caring which clock it holds.
        let clock = Clock::monotonic();
        clock.advance(1_000_000);
        assert!(!clock.is_virtual());
    }
}
