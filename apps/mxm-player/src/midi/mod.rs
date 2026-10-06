//! MIDI in and out, and the conversion between CLAP events and MIDI 1 bytes.

pub mod input;
pub mod out;
pub mod port;

pub use out::{MidiOutWorker, MidiSink, OutPanic, OutgoingEvent, SendOutcome};

use clack_host::events::UnknownEvent;
use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent};

/// Converts one plugin output event into MIDI 1 bytes, or `None` if it has no representation.
///
/// `None` is not a silent drop: the caller counts it as "unrepresented", which is the whole
/// point of distinguishing it from a successful conversion.
pub fn to_midi1(event: &UnknownEvent) -> Option<[u8; 3]> {
    if let Some(midi) = event.as_event::<MidiEvent>() {
        return Some(midi.data());
    }

    if let Some(note) = event.as_event::<NoteOnEvent>() {
        let channel = note.pckn().raw_channel();
        let key = note.pckn().raw_key();
        // A wildcard channel or key cannot be expressed as a MIDI 1 message.
        if channel < 0 || key < 0 {
            return None;
        }
        return Some([
            0x90 | (channel as u8 & 0x0f),
            key as u8 & 0x7f,
            (note.velocity() * 127.0).round().clamp(1.0, 127.0) as u8,
        ]);
    }

    if let Some(note) = event.as_event::<NoteOffEvent>() {
        let channel = note.pckn().raw_channel();
        let key = note.pckn().raw_key();
        if channel < 0 || key < 0 {
            return None;
        }
        return Some([
            0x80 | (channel as u8 & 0x0f),
            key as u8 & 0x7f,
            (note.velocity() * 127.0).round().clamp(0.0, 127.0) as u8,
        ]);
    }

    None
}

/// Whether a MIDI 1 message releases a note. Note-on with velocity zero counts, by convention.
pub fn is_release(data: [u8; 3]) -> bool {
    let status = data[0] & 0xf0;
    status == 0x80 || (status == 0x90 && data[2] == 0)
}

/// Whether a MIDI 1 message starts a note.
pub fn is_press(data: [u8; 3]) -> bool {
    data[0] & 0xf0 == 0x90 && data[2] > 0
}

/// Whether a message is a note event at all, and therefore subject to epoch filtering.
pub fn is_note_message(data: [u8; 3]) -> bool {
    is_press(data) || is_release(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clack_host::prelude::*;

    #[test]
    fn wildcard_notes_have_no_midi_1_representation() {
        let wildcard = NoteOnEvent::new(0, Pckn::match_all(), 1.0);
        assert_eq!(
            to_midi1(wildcard.as_unknown()),
            None,
            "a wildcard note must be counted as unrepresented, not turned into channel 0"
        );
    }

    #[test]
    fn ordinary_notes_convert_faithfully() {
        let note = NoteOnEvent::new(0, Pckn::new(0u16, 3u16, 64u16, 9u32), 100.0 / 127.0);
        let data = to_midi1(note.as_unknown()).expect("an ordinary note converts");
        assert_eq!(data[0], 0x93);
        assert_eq!(data[1], 64);
        assert_eq!(data[2], 100);
        assert!(is_press(data));
        assert!(!is_release(data));
    }

    #[test]
    fn a_zero_velocity_note_on_counts_as_a_release() {
        assert!(is_release([0x90, 60, 0]));
        assert!(!is_press([0x90, 60, 0]));
    }
}
