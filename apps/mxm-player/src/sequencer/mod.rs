//! A sixteen-step sequencer, so you can hear a synth without playing it.
//!
//! Not a composition tool. Where a choice was between more musical control and fewer things to
//! think about before you hear a sound, the second won.
//!
//! **Parameter locks widened that, and here is where the line now sits.** A step sets parameters as
//! well as notes, because hearing a synth means hearing it move: a filter that opens across a bar is
//! the difference between auditioning an instrument and listening to a static chord. It is still not
//! a composition tool — there is one pattern, sixteen steps, no song, no automation curves and no
//! recording of a performance. What was added is *one more thing a step can say*, authored by the
//! gesture that already existed: select a step, and the keyboard writes its notes while the knobs
//! write its parameters.
//!
//! # Where it runs, and why that differs from the control map
//!
//! **On the audio thread.** A GUI-driven sequencer fires on frame boundaries: ~16.7 ms of jitter
//! at 60 fps, which is 13% of a 16th note at 120 BPM and 19% at 180. Audibly unsteady, and it
//! would defeat the one thing the feature is for.
//!
//! [`crate::control_map`] went the other way, deliberately: a knob turn is not a note. One frame of
//! latency on a filter sweep is inaudible; one frame of jitter on a note is a bad player.
//!
//! # The parts
//!
//! - [`pattern`] — sixteen steps of notes, `Copy` and 256 bytes, so it publishes by value.
//! - [`clock`] — one accumulator in sixteenths; step index, phase, gate and transport beats are all
//!   derived from it, so they cannot disagree.
//! - [`runtime`] — the audio-thread half: what to sound and release, and when.
//! - [`random`] — a monophonic bar in C Dorian: three Euclidean rhythms mixed two steps at a time,
//!   and each step's weight in the bar choosing its notes. Seeded so tests can assert on it.
//! - [`locks`] — what each step sets besides its notes: 32 parameters × 16 steps, `Copy` and
//!   heap-free, published to the audio thread with everything else.
//! - [`sequence`] — saving and loading. It carries **no patch**; locks ride along tagged with the
//!   instrument they were recorded for, because they are part of the line rather than the patch.
//! - [`smf`] — Standard MIDI Files: the interchange format a DAW understands.
//! - [`export`] — rendering a sequence to audio: one traversal, two bars, normalised.

pub mod clock;
pub mod export;
pub mod locks;
pub mod pattern;
pub mod random;
pub mod runtime;
pub mod sequence;
pub mod smf;

pub use clock::{Boundary, Clock, DEFAULT_TEMPO, MAX_TEMPO, MIN_TEMPO, Transport};
pub use pattern::{Pattern, Step};
pub use random::sequence as random_sequence;
pub use runtime::{Action, Runtime, SEQUENCER_SOURCE, SequencerState};
pub use sequence::Sequence;
