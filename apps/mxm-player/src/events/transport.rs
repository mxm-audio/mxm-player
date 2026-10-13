//! The CLAP transport a `process()` call runs under, built in one place.
//!
//! **The live worker and the offline render both build it here**, so an export hears the tempo and
//! position live playback does. They used to be two paths: the live one built a transport per chunk
//! and the offline one passed none, and a tempo-synced LFO or delay exported at its free rate while it
//! played synced — the one difference export exists to rule out.

use clack_host::events::EventFlags;
use clack_host::events::event_types::{TransportEvent, TransportFlags};
use clack_host::prelude::*;
use clack_host::utils::{BeatTime, SecondsTime};

/// The transport as of a chunk's first sample.
///
/// CLAP defines the process transport as the state *as of sample 0*, and the tempo only ever changes
/// between chunks, so every field here is true for the whole chunk. **4/4**, so a bar is four beats:
/// the bar fields have to agree with the beat position rather than being plausible on their own.
pub fn at(beats: f64, seconds: f64, tempo: f64, playing: bool) -> TransportEvent {
    let mut flags = TransportFlags::HAS_TEMPO
        | TransportFlags::HAS_BEATS_TIMELINE
        | TransportFlags::HAS_SECONDS_TIMELINE
        | TransportFlags::HAS_TIME_SIGNATURE;
    if playing {
        flags |= TransportFlags::IS_PLAYING;
    }

    let bar_number = (beats / 4.0).floor();

    TransportEvent {
        header: EventHeader::new_core(0, EventFlags::empty()),
        flags,
        song_pos_beats: BeatTime::from_float(beats),
        song_pos_seconds: SecondsTime::from_float(seconds),
        tempo,
        tempo_inc: 0.0,
        loop_start_beats: BeatTime::from_int(0),
        loop_end_beats: BeatTime::from_int(0),
        loop_start_seconds: SecondsTime::from_int(0),
        loop_end_seconds: SecondsTime::from_int(0),
        bar_start: BeatTime::from_float(bar_number * 4.0),
        bar_number: bar_number as i32,
        time_signature_numerator: 4,
        time_signature_denominator: 4,
    }
}

/// The transport `frame` samples into a render that starts playing at beat 0 at a fixed `tempo` —
/// what an export is.
pub fn playing_from_start(frame: u64, sample_rate: f64, tempo: f64) -> TransportEvent {
    let seconds = frame as f64 / sample_rate;
    at(seconds * tempo / 60.0, seconds, tempo, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_render_from_the_start_is_at_the_beat_its_frames_reach() {
        // Two seconds at 120 bpm is four beats: one bar in, bar 1 starting on beat 4.
        let event = playing_from_start(96_000, 48_000.0, 120.0);
        assert_eq!(event.tempo, 120.0);
        assert!(event.flags.contains(TransportFlags::IS_PLAYING));
        assert_eq!(event.song_pos_beats, BeatTime::from_float(4.0));
        assert_eq!(event.song_pos_seconds, SecondsTime::from_float(2.0));
        assert_eq!(event.bar_number, 1);
        assert_eq!(event.bar_start, BeatTime::from_float(4.0));
    }

    #[test]
    fn a_stopped_transport_still_carries_its_tempo() {
        let event = at(0.0, 0.0, 90.0, false);
        assert!(!event.flags.contains(TransportFlags::IS_PLAYING));
        assert!(event.flags.contains(TransportFlags::HAS_TEMPO));
        assert_eq!(event.tempo, 90.0);
    }
}
