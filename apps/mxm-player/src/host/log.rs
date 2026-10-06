//! Transport for the plugin's own log output.
//!
//! CLAP's `log` extension is thread-safe on the host side, so a plugin may call it from several
//! of its threads at once. That rules out an `rtrb` SPSC queue: this needs a genuine
//! multi-producer transport. It is a bounded lock-free MPMC queue of fixed-size records — the
//! message is copied into a fixed buffer and truncated if oversized, nothing is formatted or
//! allocated at the call site, and drops are counted so the UI can say it lost messages rather
//! than quietly losing them.

use clack_extensions::log::LogSeverity;
use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicU64, Ordering};

/// How much of one log message is kept. Longer messages are truncated, and the record says so.
pub const MAX_MESSAGE_LEN: usize = 240;

/// How many records may be outstanding before new ones are dropped.
pub const LOG_CAPACITY: usize = 1024;

/// One log message, copied into fixed storage at the call site.
#[derive(Clone)]
pub struct LogRecord {
    pub severity: LogSeverity,
    /// Valid UTF-8 for `len` bytes: it is filled by copying from a `&str` on a char boundary.
    bytes: [u8; MAX_MESSAGE_LEN],
    len: usize,
    pub truncated: bool,
}

impl LogRecord {
    fn new(severity: LogSeverity, message: &str) -> Self {
        let mut bytes = [0u8; MAX_MESSAGE_LEN];

        // Truncate on a char boundary so the record is always valid UTF-8.
        let mut end = message.len().min(MAX_MESSAGE_LEN);
        while end > 0 && !message.is_char_boundary(end) {
            end -= 1;
        }
        bytes[..end].copy_from_slice(&message.as_bytes()[..end]);

        Self {
            severity,
            bytes,
            len: end,
            truncated: end < message.len(),
        }
    }

    pub fn message(&self) -> &str {
        // The buffer is only ever filled from a `&str` truncated at a char boundary.
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("<invalid utf-8>")
    }
}

/// The queue itself, plus its drop counter.
pub struct LogSink {
    queue: ArrayQueue<LogRecord>,
    dropped: AtomicU64,
}

impl LogSink {
    pub fn new() -> Self {
        Self {
            queue: ArrayQueue::new(LOG_CAPACITY),
            dropped: AtomicU64::new(0),
        }
    }

    /// Called from any plugin thread. Never blocks, never allocates.
    pub fn push(&self, severity: LogSeverity, message: &str) {
        if self.queue.push(LogRecord::new(severity, message)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Called from the GUI thread to drain what has accumulated.
    pub fn pop(&self) -> Option<LogRecord> {
        self.queue.pop()
    }

    /// How many messages were lost because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

impl Default for LogSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_messages_are_truncated_on_a_char_boundary() {
        let sink = LogSink::new();
        let message = "é".repeat(MAX_MESSAGE_LEN); // two bytes each
        sink.push(LogSeverity::Info, &message);

        let record = sink.pop().expect("a record");
        assert!(record.truncated);
        assert!(record.message().len() <= MAX_MESSAGE_LEN);
        assert!(record.message().chars().all(|c| c == 'é'));
    }

    #[test]
    fn overflow_is_counted_rather_than_silent() {
        let sink = LogSink::new();
        for _ in 0..LOG_CAPACITY + 10 {
            sink.push(LogSeverity::Info, "hello");
        }
        assert_eq!(sink.dropped(), 10);
    }

    #[test]
    fn concurrent_producers_do_not_lose_track_of_what_was_dropped() {
        use std::sync::Arc;

        let sink = Arc::new(LogSink::new());
        let per_thread = LOG_CAPACITY;
        let threads: Vec<_> = (0..2)
            .map(|t| {
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || {
                    for i in 0..per_thread {
                        sink.push(LogSeverity::Info, &format!("thread {t} message {i}"));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        let mut received = 0u64;
        while sink.pop().is_some() {
            received += 1;
        }
        assert_eq!(received + sink.dropped(), (per_thread * 2) as u64);
    }
}
