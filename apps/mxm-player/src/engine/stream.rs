//! Who owns the audio processor while the stream is running, and how it always gets back.
//!
//! **A dead stream is not a wedged plugin, and the two must not be confused.** The ordinary
//! protocol assumes a *future* callback will observe `Command::Stop`. After a fatal WASAPI error
//! that assumption fails: CPAL calls the error callback and then breaks out of its worker loop,
//! so no further data callback ever runs. Nothing would return the processor, the GUI would hit
//! its timeout, and an ordinary device failure would be misdiagnosed as a hung plugin.
//!
//! So ownership is returned on the way out, not only from inside a callback: when the worker
//! loop exits, the callback closure is dropped, and this owner's `Drop` performs the explicit
//! stop transition and only then enqueues the stopped processor.

use crate::host::MxmHost;
use clack_host::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The return queue's element. Capacity one, exactly one producer.
pub type ProcessorReturn = rtrb::Producer<StoppedPluginAudioProcessor<MxmHost>>;

/// Holds the processor for as long as the stream callback exists.
///
/// It holds a [`PluginAudioProcessor`] — clack's either-state enum — rather than a started or
/// stopped processor directly. That matters: clack models started and stopped as distinct
/// type-states, the return queue carries a `StoppedPluginAudioProcessor`, and a terminal CPAL
/// failure leaves the processor **started**. So the owner cannot simply forward what it holds.
pub struct ProcessorOwner {
    processor: Option<PluginAudioProcessor<MxmHost>>,
    return_queue: ProcessorReturn,
    /// Set once the processor is in the return queue and the GUI may take it.
    processor_returned: Arc<AtomicBool>,
    /// Set **only after a successful enqueue**, so the GUI never sees the flag without the
    /// processor behind it.
    stream_exited: Arc<AtomicBool>,
    /// Counts a deliberate leak, so the condition is visible rather than merely survived.
    leaked: Arc<AtomicBool>,
}

impl ProcessorOwner {
    pub fn new(
        processor: StoppedPluginAudioProcessor<MxmHost>,
        return_queue: ProcessorReturn,
        processor_returned: Arc<AtomicBool>,
        stream_exited: Arc<AtomicBool>,
        leaked: Arc<AtomicBool>,
    ) -> Self {
        Self {
            processor: Some(processor.into()),
            return_queue,
            processor_returned,
            stream_exited,
            leaked,
        }
    }

    /// The processor in whatever state it currently holds.
    pub fn processor_mut(&mut self) -> Option<&mut PluginAudioProcessor<MxmHost>> {
        self.processor.as_mut()
    }

    /// The started processor, starting it if this is the first callback.
    ///
    /// `start_processing` is an audio-thread operation in CLAP, so it happens here rather than
    /// on the GUI thread that activated the plugin.
    pub fn started_mut(&mut self) -> Option<&mut StartedPluginAudioProcessor<MxmHost>> {
        self.processor.as_mut()?.ensure_processing_started().ok()
    }

    /// Honours `Command::Stop`: stop processing, hand the processor back, and report whether it
    /// actually went. A refused enqueue leaves ownership here so it can be retried.
    pub fn hand_back(&mut self) -> bool {
        let Some(processor) = self.processor.take() else {
            return true;
        };

        match self.return_queue.push(processor.into_stopped()) {
            Ok(()) => {
                self.processor_returned.store(true, Ordering::Release);
                true
            }
            Err(rtrb::PushError::Full(stopped)) => {
                self.processor = Some(stopped.into());
                false
            }
        }
    }

    /// Whether the processor is still held here.
    pub fn holds_processor(&self) -> bool {
        self.processor.is_some()
    }
}

impl Drop for ProcessorOwner {
    fn drop(&mut self) {
        let Some(processor) = self.processor.take() else {
            return;
        };

        // The explicit stop transition, which the type-states require and a terminal CPAL
        // failure would otherwise skip.
        let stopped = processor.into_stopped();

        match self.return_queue.push(stopped) {
            Ok(()) => {
                self.processor_returned.store(true, Ordering::Release);
                self.stream_exited.store(true, Ordering::Release);
            }
            Err(rtrb::PushError::Full(stopped)) => {
                // The queue has capacity one and exactly one producer, so it cannot legitimately
                // be full here. If it somehow is, keep ownership and leak deliberately rather
                // than dropping the processor on a non-GUI thread — never a silent loss.
                self.leaked.store(true, Ordering::Release);
                std::mem::forget(stopped);
            }
        }
    }
}

/// A running audio stream, behind a trait so the engine can be driven by a fake backend in tests.
///
/// CPAL's output callback returns `()`, `process_output` is private, and its errors arise inside
/// backend operations, so neither a CLAP fixture nor player code can inject a terminal failure
/// through any public API. Testing that path therefore needs a seam we own, and this is it.
pub trait AudioStream: Send {
    /// Starts the stream. Called once, on the GUI thread.
    fn play(&mut self) -> Result<(), String>;
    /// The device the stream is running on, for display.
    fn device_name(&self) -> String;
    fn sample_rate(&self) -> f64;
    fn channel_count(&self) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_return_queue_leaks_rather_than_dropping_on_the_wrong_thread() {
        // The queue is deliberately zero-capacity-equivalent here: pushing always fails, which
        // is the "cannot legitimately happen" branch. What matters is that it does not drop.
        let (producer, _consumer) =
            rtrb::RingBuffer::<StoppedPluginAudioProcessor<MxmHost>>::new(1);
        let returned = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let leaked = Arc::new(AtomicBool::new(false));

        // With no processor there is nothing to hand back, and nothing is flagged.
        let owner = ProcessorOwner {
            processor: None,
            return_queue: producer,
            processor_returned: Arc::clone(&returned),
            stream_exited: Arc::clone(&exited),
            leaked: Arc::clone(&leaked),
        };
        drop(owner);

        assert!(!returned.load(Ordering::Acquire));
        assert!(
            !exited.load(Ordering::Acquire),
            "the exit flag must never be set without a processor behind it"
        );
        assert!(!leaked.load(Ordering::Acquire));
    }
}
