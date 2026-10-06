//! The player's event vocabulary and the queues that carry it.
//!
//! Two rules shape everything here:
//!
//! * **`rtrb` is SPSC**, so every producer owns its own queue. The audio thread drains and merges
//!   them by arrival time into one fixed-capacity buffer.
//! * **Note-offs, chokes and all-sound-off must not be lost.** A lost note-off is a stuck note,
//!   so when one cannot be enqueued the producer raises a panic epoch instead, and the audio
//!   thread discards the pre-epoch note events before issuing recovery.

pub mod input;
pub mod output;
pub mod press;

pub use input::{
    EMERGENCY_RESERVE, MAX_INPUT_PRODUCERS, MergedInput, PRODUCER_QUEUE_CAPACITY, PanicEpoch,
    Payload, SourceId, TimedEvent, merged_capacity,
};
pub use output::{FixedEventBuffer, OutputCounters, OutputRoute, classify};
pub use press::{MAX_GUI_PRESSES, MAX_TRACKED_PRESSES, PressTable, VoiceIdPool};
