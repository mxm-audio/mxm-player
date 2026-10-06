//! MIDI output: the worker, its table of notes it has actually sent, and the panic that clears
//! them.
//!
//! Note-offs bound for MIDI out get the same guarantee as note-offs bound for the plugin. A
//! counter alone is not enough: a dropped release leaves a note sounding on external hardware,
//! which is exactly the failure the input path goes to such lengths to avoid.
//!
//! Because this is not the audio thread it may block in `midir::send()`, which is precisely why
//! the recovery lives here and not in the callback.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// How many distinct (channel, note) pairs may be outstanding on one port.
pub const MAX_SENT_NOTES: usize = 512;

/// How many times a failing send is retried before the port is declared faulted.
pub const MAX_SEND_RETRIES: u32 = 8;

/// One event on its way out, tagged with the panic epoch current when it was queued.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OutgoingEvent {
    pub epoch: u32,
    pub data: [u8; 3],
}

/// The out-panic: an epoch, raised whenever a release could not be queued.
///
/// Ordering matters, and a table of sent notes is not enough on its own. A note-on may still be
/// sitting in the queue when its note-off is refused; releasing only what the table knows about
/// would send all-notes-off *first* and the queued note-on *after* it, creating precisely the
/// stuck note the panic was meant to clear.
#[derive(Debug, Default)]
pub struct OutPanic {
    epoch: AtomicU32,
    raised: AtomicBool,
    count: AtomicU64,
}

impl OutPanic {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Raises the panic and bumps the epoch. Never blocks, never spins.
    pub fn raise(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.raised.store(true, Ordering::Release);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Consumes the flag, leaving the epoch where it is.
    pub fn take(&self) -> bool {
        self.raised.swap(false, Ordering::AcqRel)
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

/// Where bytes actually go. Behind a trait so a test can fail a send on demand — `midir::send()`
/// really does fail on ordinary short messages on Windows, and that path has to be exercised.
pub trait MidiSink {
    fn send(&mut self, data: &[u8]) -> Result<(), String>;
    fn name(&self) -> String;
}

/// What one pump pass did.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SendOutcome {
    pub sent: u64,
    /// Note events discarded because they belonged to a superseded epoch.
    pub discarded: u64,
    /// Releases and recovery messages that failed to send and are being retried.
    pub retrying: u64,
    /// The port has failed too many times in a row and is reported rather than silently emptied.
    pub faulted: bool,
}

/// One (channel, note) the hardware believes is sounding, with how many presses are outstanding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct SentNote {
    channel: u8,
    key: u8,
    presses: u32,
}

/// Sends what the plugin and the keyboards produce, and keeps track of what the hardware
/// believes is sounding.
pub struct MidiOutWorker<S: MidiSink> {
    sink: S,
    queue: rtrb::Consumer<OutgoingEvent>,
    panic: Arc<OutPanic>,
    sent: Vec<SentNote>,
    /// Bitmask of the channels this port has ever been sent a note on.
    touched_channels: u16,
    /// Messages that failed to send and must be retried; only ever releases and recovery.
    retry: Vec<[u8; 3]>,
    consecutive_failures: u32,
    faulted: Arc<AtomicBool>,
    dropped: u64,
}

impl<S: MidiSink> MidiOutWorker<S> {
    pub fn new(
        sink: S,
        queue: rtrb::Consumer<OutgoingEvent>,
        panic: Arc<OutPanic>,
        faulted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            sink,
            queue,
            panic,
            sent: Vec::with_capacity(MAX_SENT_NOTES),
            touched_channels: 0,
            retry: Vec::with_capacity(MAX_SENT_NOTES),
            consecutive_failures: 0,
            faulted,
            dropped: 0,
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// How many notes the hardware currently believes are sounding.
    pub fn outstanding(&self) -> u32 {
        self.sent.iter().map(|n| n.presses).sum()
    }

    /// One pass: retries, then the panic if one is pending, then the queue.
    pub fn pump(&mut self) -> SendOutcome {
        let mut outcome = SendOutcome::default();

        self.flush_retries(&mut outcome);

        if self.panic.take() {
            self.recover(&mut outcome);
        }

        let epoch = self.panic.epoch();
        while let Ok(event) = self.queue.pop() {
            // Only note events are epoch-filtered: discarding a pre-panic pitch bend would help
            // nobody, and a stale controller value is harmless.
            if event.epoch < epoch && super::is_note_message(event.data) {
                outcome.discarded += 1;
                continue;
            }
            self.send_tracked(event.data, &mut outcome);
        }

        outcome.faulted = self.faulted.load(Ordering::Relaxed);
        outcome
    }

    /// Emits the note-offs the table says are outstanding, then all-notes-off on every channel
    /// this port has touched.
    ///
    /// The queue is drained of superseded note events **first**, so a stale note-on cannot be
    /// delivered after the recovery that was meant to clear it.
    fn recover(&mut self, outcome: &mut SendOutcome) {
        let epoch = self.panic.epoch();
        let mut carried: Vec<OutgoingEvent> = Vec::with_capacity(0);
        while let Ok(event) = self.queue.pop() {
            if event.epoch < epoch && super::is_note_message(event.data) {
                outcome.discarded += 1;
            } else {
                carried.push(event);
            }
        }

        // Releases the table says are outstanding, one per press.
        let outstanding: Vec<SentNote> = self.sent.clone();
        for note in outstanding {
            for _ in 0..note.presses {
                self.send_tracked([0x80 | (note.channel & 0x0f), note.key & 0x7f, 0], outcome);
            }
        }

        for channel in 0..16u8 {
            if self.touched_channels & (1 << channel) != 0 {
                // All notes off. Not tracked: it is not a note event in the table's sense.
                self.send_raw([0xb0 | channel, 123, 0], outcome);
            }
        }

        for event in carried {
            self.send_tracked(event.data, outcome);
        }
    }

    fn flush_retries(&mut self, outcome: &mut SendOutcome) {
        if self.retry.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.retry);
        for data in pending {
            self.send_tracked(data, outcome);
        }
    }

    /// Sends a message and updates the table — **only on a successful send**.
    ///
    /// Clearing the table optimistically would forget notes that are still sounding on hardware,
    /// which is the same class of silent wrongness as dropping a note-off.
    fn send_tracked(&mut self, data: [u8; 3], outcome: &mut SendOutcome) {
        match self.sink.send(&data) {
            Ok(()) => {
                self.consecutive_failures = 0;
                outcome.sent += 1;

                if super::is_press(data) {
                    self.touched_channels |= 1 << (data[0] & 0x0f);
                    self.record_press(data[0] & 0x0f, data[1]);
                } else if super::is_release(data) {
                    self.record_release(data[0] & 0x0f, data[1]);
                }
            }
            Err(_) => {
                self.consecutive_failures += 1;
                if super::is_release(data) {
                    // A failed release keeps its entry, and is retried.
                    if self.retry.len() < MAX_SENT_NOTES {
                        self.retry.push(data);
                        outcome.retrying += 1;
                    } else {
                        self.dropped += 1;
                    }
                } else {
                    self.dropped += 1;
                }

                if self.consecutive_failures >= MAX_SEND_RETRIES {
                    // A dead MIDI output the user can see beats a stuck note they cannot explain.
                    self.faulted.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    fn send_raw(&mut self, data: [u8; 3], outcome: &mut SendOutcome) {
        match self.sink.send(&data) {
            Ok(()) => {
                self.consecutive_failures = 0;
                outcome.sent += 1;
            }
            Err(_) => {
                self.consecutive_failures += 1;
                if self.retry.len() < MAX_SENT_NOTES {
                    self.retry.push(data);
                    outcome.retrying += 1;
                }
                if self.consecutive_failures >= MAX_SEND_RETRIES {
                    self.faulted.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    fn record_press(&mut self, channel: u8, key: u8) {
        if let Some(note) = self
            .sent
            .iter_mut()
            .find(|n| n.channel == channel && n.key == key)
        {
            note.presses += 1;
        } else if self.sent.len() < MAX_SENT_NOTES {
            self.sent.push(SentNote {
                channel,
                key,
                presses: 1,
            });
        } else {
            self.dropped += 1;
        }
    }

    fn record_release(&mut self, channel: u8, key: u8) {
        if let Some(index) = self
            .sent
            .iter()
            .position(|n| n.channel == channel && n.key == key)
        {
            self.sent[index].presses = self.sent[index].presses.saturating_sub(1);
            if self.sent[index].presses == 0 {
                self.sent.remove(index);
            }
        }
    }

    /// Releases everything outstanding, for a clean port switch or shutdown.
    ///
    /// Changing the MIDI output port releases everything held on the old port before closing it.
    pub fn release_all(&mut self) -> SendOutcome {
        let mut outcome = SendOutcome::default();
        self.recover(&mut outcome);
        outcome
    }

    pub fn port_name(&self) -> String {
        self.sink.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decides which messages a test sink should refuse.
    type FailurePredicate = Box<dyn Fn(&[u8]) -> bool + Send>;

    /// A sink that records what it was sent and can be told to fail.
    struct TestSink {
        sent: Vec<[u8; 3]>,
        fail_matching: Option<FailurePredicate>,
    }

    impl TestSink {
        fn new() -> Self {
            Self {
                sent: Vec::new(),
                fail_matching: None,
            }
        }
    }

    impl MidiSink for TestSink {
        fn send(&mut self, data: &[u8]) -> Result<(), String> {
            if let Some(predicate) = &self.fail_matching
                && predicate(data)
            {
                return Err("injected failure".to_owned());
            }
            let mut bytes = [0u8; 3];
            bytes[..data.len().min(3)].copy_from_slice(&data[..data.len().min(3)]);
            self.sent.push(bytes);
            Ok(())
        }

        fn name(&self) -> String {
            "test sink".to_owned()
        }
    }

    fn worker(
        capacity: usize,
    ) -> (
        MidiOutWorker<TestSink>,
        rtrb::Producer<OutgoingEvent>,
        Arc<OutPanic>,
        Arc<AtomicBool>,
    ) {
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        let panic = Arc::new(OutPanic::new());
        let faulted = Arc::new(AtomicBool::new(false));
        let worker = MidiOutWorker::new(
            TestSink::new(),
            consumer,
            Arc::clone(&panic),
            Arc::clone(&faulted),
        );
        (worker, producer, panic, faulted)
    }

    #[test]
    fn the_table_tracks_what_the_hardware_believes_is_sounding() {
        let (mut worker, mut queue, _panic, _faulted) = worker(16);
        queue
            .push(OutgoingEvent {
                epoch: 0,
                data: [0x90, 60, 100],
            })
            .unwrap();
        queue
            .push(OutgoingEvent {
                epoch: 0,
                data: [0x90, 60, 100],
            })
            .unwrap();
        worker.pump();
        assert_eq!(
            worker.outstanding(),
            2,
            "a key struck twice owes two releases on the wire too"
        );

        queue
            .push(OutgoingEvent {
                epoch: 0,
                data: [0x80, 60, 0],
            })
            .unwrap();
        worker.pump();
        assert_eq!(worker.outstanding(), 1);
    }

    #[test]
    fn a_panic_releases_the_table_and_then_says_all_notes_off() {
        let (mut worker, mut queue, panic, _faulted) = worker(16);
        queue
            .push(OutgoingEvent {
                epoch: 0,
                data: [0x92, 60, 100],
            })
            .unwrap();
        worker.pump();
        worker.sink.sent.clear();

        panic.raise();
        worker.pump();

        assert!(
            worker.sink.sent.contains(&[0x82, 60, 0]),
            "the outstanding note must be released explicitly"
        );
        assert!(
            worker.sink.sent.contains(&[0xb2, 123, 0]),
            "all-notes-off must follow, on every channel touched"
        );
        assert_eq!(worker.outstanding(), 0);
    }

    #[test]
    fn a_stale_note_on_is_discarded_rather_than_delivered_after_the_recovery() {
        let (mut worker, mut queue, panic, _faulted) = worker(16);

        // A note-on queued before the panic, still in flight when it fires.
        queue
            .push(OutgoingEvent {
                epoch: 0,
                data: [0x90, 64, 100],
            })
            .unwrap();
        panic.raise();
        // ...and one queued after it, which must still play.
        let epoch = panic.epoch();
        queue
            .push(OutgoingEvent {
                epoch,
                data: [0x90, 67, 100],
            })
            .unwrap();

        let outcome = worker.pump();

        assert_eq!(
            outcome.discarded, 1,
            "the pre-panic note-on must be dropped"
        );
        assert!(
            !worker.sink.sent.contains(&[0x90, 64, 100]),
            "delivering it after all-notes-off would create the stuck note the panic exists to clear"
        );
        assert!(
            worker.sink.sent.contains(&[0x90, 67, 100]),
            "playing on through a panic must behave sanely"
        );
        assert_eq!(worker.outstanding(), 1);
    }

    #[test]
    fn a_failed_release_is_retained_and_retried_not_forgotten() {
        let (mut worker, mut queue, _panic, faulted) = worker(16);
        queue
            .push(OutgoingEvent {
                epoch: 0,
                data: [0x90, 60, 100],
            })
            .unwrap();
        worker.pump();
        assert_eq!(worker.outstanding(), 1);

        // Fail every note-off from here on.
        worker.sink.fail_matching = Some(Box::new(|data| data[0] & 0xf0 == 0x80));
        queue
            .push(OutgoingEvent {
                epoch: 0,
                data: [0x80, 60, 0],
            })
            .unwrap();
        let outcome = worker.pump();

        assert_eq!(outcome.retrying, 1);
        assert_eq!(
            worker.outstanding(),
            1,
            "the entry must survive: the hardware still believes the note is sounding"
        );

        // Enough failures and the port is reported faulted rather than silently emptied.
        for _ in 0..MAX_SEND_RETRIES {
            worker.pump();
        }
        assert!(faulted.load(Ordering::Relaxed));

        // Once the port recovers, the retried release actually lands.
        worker.sink.fail_matching = None;
        worker.pump();
        assert_eq!(worker.outstanding(), 0);
    }

    #[test]
    fn switching_ports_releases_everything_held_on_the_old_one() {
        let (mut worker, mut queue, _panic, _faulted) = worker(16);
        for key in [60u8, 64, 67] {
            queue
                .push(OutgoingEvent {
                    epoch: 0,
                    data: [0x90, key, 100],
                })
                .unwrap();
        }
        worker.pump();
        worker.sink.sent.clear();

        worker.release_all();

        for key in [60u8, 64, 67] {
            assert!(worker.sink.sent.contains(&[0x80, key, 0]));
        }
        assert_eq!(worker.outstanding(), 0);
    }
}
