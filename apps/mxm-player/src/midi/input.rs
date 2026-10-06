//! MIDI input connections.
//!
//! Each connection owns its own SPSC queue, because `rtrb` is single-producer and a MIDI
//! callback runs on the backend's own thread. Events are stamped with the host-monotonic clock
//! **inside the callback**, before any queueing.
//!
//! Nothing here spins: spinning inside a `midir` callback stalls the MIDI backend thread. When a
//! note-off cannot be enqueued the epoch is raised instead, and the audio thread turns that into
//! recovery — the note-off may be dropped, but the panic supersedes it, which is a
//! stuck-note-free outcome by a different route.

use crate::clock;
use crate::events::input::{PanicEpoch, Payload, SourceId, TimedEvent};
use midir::{MidiInput, MidiInputConnection, MidiInputPort};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use wmidi::MidiMessage;

/// A port as offered in the picker. Persisted **by name**, not by enumeration index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortInfo {
    pub name: String,
}

/// Lists the MIDI input ports currently available.
pub fn list_ports() -> Result<Vec<PortInfo>, String> {
    let input = MidiInput::new("mxm-player-scan").map_err(|e| e.to_string())?;
    Ok(input
        .ports()
        .iter()
        .filter_map(|port| input.port_name(port).ok().map(|name| PortInfo { name }))
        .collect())
}

/// Counters a connection maintains, so the UI can tell "no sound" from "no MIDI".
#[derive(Debug, Default)]
pub struct InputCounters {
    pub received: AtomicU64,
    /// Droppable events lost because the queue was full.
    pub dropped: AtomicU64,
    /// Times a must-not-be-lost event could not be enqueued and raised the panic instead.
    pub panics: AtomicU64,
    /// Messages the player does not translate (SysEx, program change, and the rest).
    pub ignored: AtomicU64,
}

/// A live connection. Dropping it closes the port.
pub struct Connection {
    _connection: MidiInputConnection<ConnectionState>,
    pub port_name: String,
    pub source: SourceId,
    pub counters: Arc<InputCounters>,
}

/// What the callback needs. Moved into the connection.
struct ConnectionState {
    producer: rtrb::Producer<TimedEvent>,
    source: SourceId,
    epoch: Arc<PanicEpoch>,
    counters: Arc<InputCounters>,
    /// Read inside the MIDI backend's own callback, so it must never block — see `clock.rs`.
    clock: Arc<clock::Clock>,
    /// Where a note going down or coming up is published for the on-screen keyboard to show.
    host: Arc<crate::host::PlayerHostState>,
}

/// Opens a port and starts feeding `producer`.
pub fn connect(
    port_name: &str,
    source: SourceId,
    producer: rtrb::Producer<TimedEvent>,
    epoch: Arc<PanicEpoch>,
    clock: Arc<clock::Clock>,
    host: Arc<crate::host::PlayerHostState>,
) -> Result<Connection, String> {
    // The reason names the port even when no port can be opened at all: a Linux machine without the
    // ALSA sequencer (`/dev/snd/seq`) refuses the client itself, and the user still needs to know
    // which input it is about.
    let mut input = MidiInput::new("mxm-player").map_err(|e| {
        format!("MIDI input port `{port_name}` cannot be opened: this system offers no MIDI ({e})")
    })?;
    input.ignore(midir::Ignore::None);

    let port = find_port(&input, port_name)?;
    let counters = Arc::new(InputCounters::default());

    let state = ConnectionState {
        producer,
        source,
        epoch,
        counters: Arc::clone(&counters),
        clock,
        host,
    };

    let connection = input
        .connect(&port, "mxm-player-in", on_message, state)
        .map_err(|e| e.to_string())?;

    Ok(Connection {
        _connection: connection,
        port_name: port_name.to_owned(),
        source,
        counters,
    })
}

fn find_port(input: &MidiInput, name: &str) -> Result<MidiInputPort, String> {
    input
        .ports()
        .into_iter()
        .find(|port| input.port_name(port).is_ok_and(|n| n == name))
        .ok_or_else(|| format!("no MIDI input port named `{name}` is connected"))
}

/// The MIDI callback. Stamps, translates, enqueues — and never blocks.
fn on_message(_timestamp: u64, bytes: &[u8], state: &mut ConnectionState) {
    // Host-monotonic, taken here rather than derived from the backend's own timestamp, whose
    // epoch is not shared with anything else.
    let arrival = state.clock.now_nanos();
    state.counters.received.fetch_add(1, Ordering::Relaxed);

    let Ok(message) = MidiMessage::try_from(bytes) else {
        state.counters.ignored.fetch_add(1, Ordering::Relaxed);
        return;
    };

    let Some(payload) = translate(&message) else {
        state.counters.ignored.fetch_add(1, Ordering::Relaxed);
        return;
    };

    // The on-screen keyboard doubles as a monitor, so what a connected keyboard is holding has
    // to reach it. Published before the queue, because a full queue must not cost the key its
    // highlight — and both directions are wait-free.
    match payload {
        Payload::NoteOn { key, .. } => state.host.set_midi_sounding(key, true),
        Payload::NoteOff { key, .. } => state.host.set_midi_sounding(key, false),
        _ => {}
    }

    let event = TimedEvent::new(arrival, state.epoch.current(), state.source, payload);
    if state.producer.push(event).is_err() {
        if payload.must_not_be_lost() {
            // Raise the panic rather than spin. The audio thread discards pre-epoch notes and
            // issues recovery in the dialect the plugin negotiated.
            state.epoch.raise();
            state.counters.panics.fetch_add(1, Ordering::Relaxed);
        } else {
            state.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Translates a MIDI 1 message into the player's own vocabulary.
///
/// Returns `None` for anything the player does not carry, which the caller counts rather than
/// pretending to have handled.
pub fn translate(message: &MidiMessage<'_>) -> Option<Payload> {
    Some(match message {
        // Velocity zero is a note-off by convention.
        MidiMessage::NoteOn(channel, note, velocity) if u8::from(*velocity) == 0 => {
            Payload::NoteOff {
                channel: channel.index(),
                key: u8::from(*note),
                velocity: 0.0,
            }
        }
        MidiMessage::NoteOn(channel, note, velocity) => Payload::NoteOn {
            channel: channel.index(),
            key: u8::from(*note),
            // CLAP velocity is normalised, not a MIDI integer.
            velocity: f64::from(u8::from(*velocity)) / 127.0,
        },
        MidiMessage::NoteOff(channel, note, velocity) => Payload::NoteOff {
            channel: channel.index(),
            key: u8::from(*note),
            velocity: f64::from(u8::from(*velocity)) / 127.0,
        },
        MidiMessage::ControlChange(channel, function, value) => Payload::ControlChange {
            channel: channel.index(),
            controller: u8::from(*function),
            value: u8::from(*value),
        },
        MidiMessage::PitchBendChange(channel, bend) => Payload::PitchBend {
            channel: channel.index(),
            value: f64::from(u16::from(*bend)) / 16_383.0,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wmidi::{Channel, Note, U7};

    #[test]
    fn a_zero_velocity_note_on_becomes_a_note_off() {
        let message = MidiMessage::NoteOn(Channel::Ch1, Note::C3, U7::from_u8_lossy(0));
        assert!(matches!(translate(&message), Some(Payload::NoteOff { .. })));
    }

    #[test]
    fn velocity_is_normalised_for_clap_not_passed_through_as_an_integer() {
        let message = MidiMessage::NoteOn(Channel::Ch1, Note::C3, U7::from_u8_lossy(100));
        let Some(Payload::NoteOn { velocity, .. }) = translate(&message) else {
            panic!("expected a note-on")
        };
        assert!((velocity - 100.0 / 127.0).abs() < 1e-12);
    }

    #[test]
    fn untranslatable_messages_are_reported_rather_than_invented() {
        let message = MidiMessage::ProgramChange(Channel::Ch1, U7::from_u8_lossy(3));
        assert_eq!(translate(&message), None);
    }

    #[test]
    fn a_centred_pitch_bend_lands_on_the_clap_centre() {
        let message =
            MidiMessage::PitchBendChange(Channel::Ch1, wmidi::U14::try_from(8192u16).unwrap());
        let Some(Payload::PitchBend { value, .. }) = translate(&message) else {
            panic!("expected a pitch bend")
        };
        assert!((value - 0.5).abs() < 1e-3);
    }
}
