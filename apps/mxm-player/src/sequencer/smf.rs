//! Standard MIDI Files: the interchange format.
//!
//! Written here rather than taken as a dependency. The subset the sequencer needs is small and
//! fixed — one bar, sixteenths, one tempo — and a crate that reads arbitrary MIDI would carry far
//! more than that costs to pin.
//!
//! # What this promises, and what it does not
//!
//! - **A file this module wrote reads back identically.** Tempo is canonicalised on save to the
//!   value SMF can hold, so "identical" is true rather than approximately true.
//! - **A foreign file returns its pitches and grid positions only.** Velocity, note length, channel
//!   and same-pitch collisions within a step have nowhere to live in a 128-bit-per-step mask, so
//!   they are read, used where they mean something, and **named in the report** rather than lost
//!   quietly.
//! - **Anything the model cannot hold is refused with a reason**, not quantised. Silently rewriting
//!   somebody's DAW export is not a load.
//!
//! # Conformance, checked against an independent implementation
//!
//! Testing this writer against this reader would only prove they share assumptions. Both directions
//! are therefore checked against `mxm.midifile`, a separate Python implementation:
//! `tests/midi-conformance/verify.py` parses what we write, and `foreign.mid` — written by that
//! library at a different division and velocity — is loaded by the test suite.

use super::clock::{MAX_TEMPO, MIN_TEMPO};
use super::pattern::{Pattern, STEPS};

/// Ticks per quarter note in files this module writes.
///
/// 96 divides by 4 without remainder, so a sixteenth is exactly 24 ticks and no grid position ever
/// needs rounding.
pub const TICKS_PER_QUARTER: u16 = 96;

/// Ticks in one sixteenth, at our division.
pub const TICKS_PER_STEP: u32 = TICKS_PER_QUARTER as u32 / 4;

/// The channel notes are written on, and canonicalised to on load.
pub const CHANNEL: u8 = 0;

/// Velocity written for every note, until accent exists.
pub const VELOCITY: u8 = 100;

/// How far off the sixteenth grid a note may sit and still be accepted, in ticks.
///
/// A tolerance rather than exactness because another program's rounding is not corruption. An
/// eighth of a step: wide enough for rounding, far too narrow to swallow a genuine off-grid note.
pub const GRID_TOLERANCE: u32 = TICKS_PER_STEP / 8;

/// The time signature a bar of `steps` sixteenths is in, as `(numerator, denominator)`.
///
/// **A bar is however many sixteenths it holds**, so the honest answer is always `steps/16` — but
/// `16/16` is not what anybody writes for a 4/4 bar, and a DAW shows what the file says. So it is
/// reduced as far as it goes exactly: sixteen steps is 4/4, twelve is 3/4, ten is 5/8, and an odd
/// count that reduces no further stays over 16.
///
/// **`None` for a bar MIDI cannot express.** The numerator is one byte, and how far that reaches
/// depends on how far the bar reduces: a multiple of four gets to 255/4, which is
/// [`MAX_METER_STEPS`] sixteenths; an even bar that is not gets to 255/8, half of it; an odd one to
/// 255/16, a quarter. That is a limit of the file format and not of the sequencer, which is why it
/// lives here and not on `Pattern`.
///
/// Returning `None` rather than truncating is the point: the first version cast a `u32` to `u8` and
/// turned a 1,024-step bar into `0/4` — a broken file that read back as no steps at all.
pub fn meter_for(steps_per_bar: usize) -> Option<(u8, u8)> {
    let steps = steps_per_bar.max(1) as u32;
    let (numerator, denominator) = if steps.is_multiple_of(4) {
        (steps / 4, 4u8)
    } else if steps.is_multiple_of(2) {
        (steps / 2, 8)
    } else {
        (steps, 16)
    };
    u8::try_from(numerator)
        .ok()
        .map(|numerator| (numerator, denominator))
}

/// The longest bar a MIDI time signature can express, in sixteenths: 255/4.
///
/// **Only for a bar that is a multiple of four.** See [`meter_for`] — a bar that reduces less far
/// runs out of numerator sooner, and `meter_for` is the authority rather than this number.
pub const MAX_METER_STEPS: usize = 255 * 4;

/// How many sixteenths a bar of `numerator/denominator` holds — the inverse of [`meter_for`].
///
/// Zero for a meter that cannot be expressed in sixteenths, which is what makes a 3/32 file a
/// refusal rather than a silently wrong import.
pub fn steps_for(numerator: u8, denominator: u8) -> usize {
    if denominator == 0 || !16u32.is_multiple_of(u32::from(denominator)) {
        return 0;
    }
    let per = 16 / u32::from(denominator);
    (u32::from(numerator) * per) as usize
}

/// Why a file could not be loaded.
///
/// Each names information the sixteen-step model **cannot hold**. Refusing is the honest answer.
// Not `Eq`: `TempoOutOfRange` carries the offending BPM, and `f64` has no total equality.
#[derive(Clone, Debug, PartialEq)]
pub enum Refused {
    NotAMidiFile,
    Truncated,
    UnsupportedFormat(u16),
    SeveralNoteTracks(usize),
    SeveralChannels(Vec<u8>),
    TempoChange,
    TimeSignature {
        numerator: u8,
        denominator: u8,
    },
    LongerThanOneBar {
        ticks: u32,
    },
    OffGrid {
        ticks: u32,
    },
    TempoOutOfRange(f64),
    UnpairedNote {
        key: u8,
    },
    /// A note whose length is neither the half-step gate nor a whole number of steps.
    UnrepresentableLength {
        key: u8,
        at: u32,
        ticks: u32,
    },
    /// Two notes crossing one step that would need it tied and untied at once.
    ConflictingGates {
        step: usize,
        keys: Vec<u8>,
    },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::NotAMidiFile => write!(f, "this is not a MIDI file"),
            Refused::Truncated => write!(f, "the file ends in the middle of an event"),
            Refused::UnsupportedFormat(n) => {
                write!(
                    f,
                    "MIDI file format {n} is not supported; use format 0 or 1"
                )
            }
            Refused::SeveralNoteTracks(n) => write!(
                f,
                "the file has {n} tracks containing notes; the sequencer holds one"
            ),
            Refused::SeveralChannels(channels) => write!(
                f,
                "the file uses MIDI channels {channels:?}; a step holds pitches only, so channel \
                 identity has nowhere to live"
            ),
            Refused::TempoChange => write!(f, "the file changes tempo partway through"),
            Refused::TimeSignature {
                numerator,
                denominator,
            } => write!(
                f,
                "the file is in {numerator}/{denominator}, which is not a whole number of \
                 sixteenth notes"
            ),
            Refused::LongerThanOneBar { ticks } => write!(
                f,
                "the file is {ticks} ticks long, which is more than the sequence it was read into"
            ),
            Refused::OffGrid { ticks } => write!(
                f,
                "a note starts at tick {ticks}, which is not on the sixteenth grid"
            ),
            Refused::TempoOutOfRange(bpm) => write!(
                f,
                "the file is {bpm:.1} BPM; the sequencer runs between {MIN_TEMPO} and {MAX_TEMPO}"
            ),
            Refused::UnrepresentableLength { key, at, ticks } => write!(
                f,
                "the note {key} at tick {at} lasts {ticks} ticks; a step holds either half a step \
                 ({}) or a whole number of steps, and {ticks} is neither",
                TICKS_PER_STEP / 2
            ),
            Refused::ConflictingGates { step, keys } => write!(
                f,
                "step {} is crossed by notes {keys:?} that would need it tied and untied at once",
                step + 1
            ),
            Refused::UnpairedNote { key } => {
                write!(f, "a note ({key}) is never released")
            }
        }
    }
}

/// What a load kept, and what it could not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Loaded {
    pub pattern: Pattern,
    pub tempo: f64,
    /// Information read and then dropped, named so nobody wonders where it went.
    pub lost: Vec<String>,
}

// --- writing ------------------------------------------------------------------------------------

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// SMF's variable-length quantity: seven bits per byte, high bit set on all but the last.
fn push_varint(out: &mut Vec<u8>, mut value: u32) {
    let mut buffer = [0u8; 4];
    let mut len = 0;
    buffer[len] = (value & 0x7f) as u8;
    len += 1;
    value >>= 7;
    while value > 0 {
        buffer[len] = ((value & 0x7f) as u8) | 0x80;
        len += 1;
        value >>= 7;
    }
    for byte in buffer[..len].iter().rev() {
        out.push(*byte);
    }
}

/// Microseconds per quarter note, which is how SMF stores tempo.
pub fn micros_per_quarter(bpm: f64) -> u32 {
    (60_000_000.0 / bpm).round() as u32
}

/// The tempo an SMF file will actually hold.
///
/// Saving canonicalises to this, so a file written here round-trips **exactly** rather than to
/// within a rounding error.
pub fn canonical_tempo(bpm: f64) -> f64 {
    60_000_000.0 / f64::from(micros_per_quarter(bpm))
}

/// Writes a type-0 file holding one bar of the pattern.
/// What writing a pattern to MIDI could not keep, in the same words a load reports its losses.
///
/// **Saving reports its losses too.** Two things a `.mid` cannot hold, and letting somebody find
/// either by ear afterwards is the failure this exists to prevent.
pub fn write_report(pattern: &Pattern) -> Vec<String> {
    let mut lost = Vec::new();

    // **The legato joint.** A tie carrying notes of its own is a note ending exactly where the next
    // begins, with no gap and no retrigger. In the file that is a note-off and a note-on at the
    // same tick, off before on — unambiguous in MIDI even at one pitch, and the only thing MIDI can
    // say here. What is gone is "these two must not retrigger", which is a property of the gate and
    // has no representation at all. **Reading it back gives an ordinary note, not a tie.**
    let joints = (0..pattern.len())
        .filter(|step| pattern.tied(*step) && !pattern.step(*step).is_empty())
        .count();
    if joints > 0 {
        lost.push(format!(
            "{joints} legato joint(s): a tie carrying its own notes is written as two notes \
             back to back, and reads back as an ordinary note"
        ));
    }

    // **A run that wraps the loop point.** A note on step 16 tied by a tie on step 1 carries across
    // the repeat — the tie reaches backwards, so it is step 1's gate that holds it. This file is
    // one bar, so the run is cut at the bar end. Refusing the whole file for a pattern that plays
    // correctly would be worse; saying nothing would be the silent rewrite.
    let wrapped: Vec<String> = (0..pattern.len())
        .filter(|step| {
            !pattern.step(*step).is_empty()
                && pattern.held_past_gate(*step)
                && pattern.run_ends_at(*step) == pattern.len()
                && pattern.tied(0)
        })
        .map(|step| (step + 1).to_string())
        .collect();
    if !wrapped.is_empty() {
        lost.push(format!(
            "a run from step {} carries past the end of the bar and was truncated there",
            wrapped.join(", ")
        ));
    }

    lost
}

pub fn write(pattern: &Pattern, tempo: f64) -> Vec<u8> {
    let mut track = Vec::new();

    // Tempo and time signature, both at tick 0.
    push_varint(&mut track, 0);
    track.extend_from_slice(&[0xff, 0x51, 0x03]);
    let micros = micros_per_quarter(tempo);
    track.extend_from_slice(&micros.to_be_bytes()[1..]);

    push_varint(&mut track, 0);
    // 4/4, 24 MIDI clocks per metronome click, 8 32nds per quarter.
    // **The meter the bar is actually in**, rather than an assumed 4/4. `denominator` is stored as
    // its log2, which is what the format asks for.
    //
    // **A bar too long for the format gets no time signature at all**, rather than a truncated one.
    // Absent means 4/4 by the specification, so the bar lines in a DAW will be wrong — but the notes
    // are right, and `write_report` says so out loud. Writing a wrong meter would corrupt both.
    if let Some((numerator, denominator)) = meter_for(pattern.steps_per_bar()) {
        let log2 = denominator.trailing_zeros() as u8;
        track.extend_from_slice(&[0xff, 0x58, 0x04, numerator, log2, 24, 8]);
    }

    // Notes, in tick order.
    //
    // **A tied run is one note, not several.** A note followed by *k* empty ties lasts *k* + 1
    // steps, which MIDI expresses natively as a single note whose off lands on a step boundary. An
    // untied note keeps the half-step gate the sequencer plays.
    let gate = TICKS_PER_STEP / 2;
    let mut events: Vec<(u32, u8, u8)> = Vec::new(); // (tick, status, key)
    for (index, step) in pattern.steps() {
        if step.is_empty() {
            continue;
        }
        let start = index as u32 * TICKS_PER_STEP;
        let end = if pattern.held_past_gate(index) {
            // `run_ends_at` stops at the bar, which is the truncation `report` names: a run that
            // wraps is a property of a repeating pattern, and this file holds exactly one bar.
            pattern.run_ends_at(index) as u32 * TICKS_PER_STEP
        } else {
            start + gate
        };
        step.for_each(|key| {
            events.push((start, 0x90 | CHANNEL, key));
            events.push((end, 0x80 | CHANNEL, key));
        });
    }
    finish(track, events, pattern.len())
}

/// Appends the note events to a prepared track and wraps the whole thing in a format 0 file.
///
/// Split out of [`write`] so a test can build a file from an explicit `(tick, status, key)` list.
/// Rewriting a delta time in finished bytes is not a substitute: deltas are **relative**, so
/// changing one silently moves every event after it — which is how a test meaning "this note is
/// longer" ends up meaning "and the next one is off the grid".
fn finish(mut track: Vec<u8>, mut events: Vec<(u32, u8, u8)>, total_steps: usize) -> Vec<u8> {
    // Note-offs before note-ons at the same tick, so a repeated pitch is unambiguous.
    events.sort_by_key(|(tick, status, key)| (*tick, *status & 0xf0, *key));

    let mut previous = 0u32;
    for (tick, status, key) in events {
        push_varint(&mut track, tick - previous);
        previous = tick;
        track.push(status);
        track.push(key);
        track.push(if status & 0xf0 == 0x90 { VELOCITY } else { 0 });
    }

    // End of track at the **sequence's** end, so the file is as long as the music even when the
    // last steps rest — which is what makes it loop in a DAW. It used to be one bar, back when a
    // sequence was always one bar.
    let end = TICKS_PER_STEP * total_steps as u32;
    push_varint(&mut track, end.saturating_sub(previous));
    track.extend_from_slice(&[0xff, 0x2f, 0x00]);

    let mut out = Vec::with_capacity(track.len() + 22);
    out.extend_from_slice(b"MThd");
    push_u32(&mut out, 6);
    push_u16(&mut out, 0); // format 0
    push_u16(&mut out, 1); // one track
    push_u16(&mut out, TICKS_PER_QUARTER);
    out.extend_from_slice(b"MTrk");
    push_u32(&mut out, track.len() as u32);
    out.extend_from_slice(&track);
    out
}

// --- reading ------------------------------------------------------------------------------------

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn u8(&mut self) -> Result<u8, Refused> {
        let byte = *self.bytes.get(self.at).ok_or(Refused::Truncated)?;
        self.at += 1;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16, Refused> {
        Ok(u16::from(self.u8()?) << 8 | u16::from(self.u8()?))
    }

    fn u32(&mut self) -> Result<u32, Refused> {
        Ok(u32::from(self.u16()?) << 16 | u32::from(self.u16()?))
    }

    fn tag(&mut self, tag: &[u8; 4]) -> Result<(), Refused> {
        if self.remaining() < 4 || &self.bytes[self.at..self.at + 4] != tag {
            return Err(Refused::NotAMidiFile);
        }
        self.at += 4;
        Ok(())
    }

    fn varint(&mut self) -> Result<u32, Refused> {
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.u8()?;
            value = (value << 7) | u32::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Refused::Truncated)
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], Refused> {
        if self.remaining() < count {
            return Err(Refused::Truncated);
        }
        let slice = &self.bytes[self.at..self.at + count];
        self.at += count;
        Ok(slice)
    }
}

/// One note event, as read.
struct NoteEvent {
    tick: u32,
    channel: u8,
    key: u8,
    velocity: u8,
    on: bool,
}

/// Reads a file into a pattern and a tempo.
///
/// **Bounded work**: every loop is driven by a length the header declares, and a malformed file
/// fails before anything is returned, so the caller's current sequence is never partially replaced.
pub fn read(bytes: &[u8]) -> Result<Loaded, Refused> {
    let mut reader = Reader::new(bytes);
    reader.tag(b"MThd")?;
    let header_len = reader.u32()?;
    if header_len < 6 {
        return Err(Refused::NotAMidiFile);
    }
    let format = reader.u16()?;
    let track_count = reader.u16()?;
    let division = reader.u16()?;
    // Skip any header bytes beyond the six we understand.
    let _ = reader.take(header_len as usize - 6)?;

    if format > 1 {
        return Err(Refused::UnsupportedFormat(format));
    }
    if division & 0x8000 != 0 {
        // SMPTE timing, not ticks per quarter.
        return Err(Refused::UnsupportedFormat(format));
    }
    let ticks_per_quarter = u32::from(division).max(1);

    let mut notes: Vec<NoteEvent> = Vec::new();
    let mut tempos: Vec<u32> = Vec::new();
    let mut time_signature: Option<(u8, u8)> = None;
    let mut note_tracks = 0usize;
    let mut ignored: Vec<&str> = Vec::new();

    for _ in 0..track_count {
        if reader.remaining() == 0 {
            break;
        }
        reader.tag(b"MTrk")?;
        let length = reader.u32()? as usize;
        let body = reader.take(length)?;
        let mut track = Reader::new(body);

        let mut tick = 0u32;
        let mut running_status = 0u8;
        let mut track_has_notes = false;

        while track.remaining() > 0 {
            tick += track.varint()?;
            let mut status = track.u8()?;
            if status & 0x80 == 0 {
                // Running status: the byte was data, not a status.
                track.at -= 1;
                status = running_status;
                if status == 0 {
                    return Err(Refused::NotAMidiFile);
                }
            } else if status < 0xf0 {
                running_status = status;
            }

            match status {
                0xff => {
                    let kind = track.u8()?;
                    let len = track.varint()? as usize;
                    let data = track.take(len)?;
                    match kind {
                        0x51 if len == 3 => tempos.push(
                            u32::from(data[0]) << 16 | u32::from(data[1]) << 8 | u32::from(data[2]),
                        ),
                        0x58 if len >= 2 => {
                            time_signature = Some((data[0], 1u8 << data[1].min(7)));
                        }
                        0x2f => break,
                        0x03 | 0x06 => ignored.push("track names and markers"),
                        _ => {}
                    }
                }
                0xf0 | 0xf7 => {
                    let len = track.varint()? as usize;
                    let _ = track.take(len)?;
                    ignored.push("SysEx");
                }
                _ => {
                    let kind = status & 0xf0;
                    let channel = status & 0x0f;
                    match kind {
                        0x80 | 0x90 => {
                            let key = track.u8()?;
                            let velocity = track.u8()?;
                            track_has_notes = true;
                            notes.push(NoteEvent {
                                tick,
                                channel,
                                key,
                                velocity,
                                // Velocity zero is a note-off by convention.
                                on: kind == 0x90 && velocity > 0,
                            });
                        }
                        0xa0 => {
                            let _ = track.take(2)?;
                            ignored.push("aftertouch");
                        }
                        0xb0 => {
                            let _ = track.take(2)?;
                            ignored.push("controller messages");
                        }
                        0xe0 => {
                            let _ = track.take(2)?;
                            ignored.push("pitch bend");
                        }
                        0xc0 => {
                            let _ = track.take(1)?;
                            ignored.push("program changes");
                        }
                        0xd0 => {
                            let _ = track.take(1)?;
                            ignored.push("channel pressure");
                        }
                        _ => return Err(Refused::NotAMidiFile),
                    }
                }
            }
        }

        if track_has_notes {
            note_tracks += 1;
        }
    }

    if note_tracks > 1 {
        return Err(Refused::SeveralNoteTracks(note_tracks));
    }
    if tempos.len() > 1 {
        return Err(Refused::TempoChange);
    }
    // **The meter says how long a bar is**, rather than having to be 4/4. What it cannot say is a
    // bar that is not a whole number of sixteenths, which is a refusal — importing 3/32 as something
    // near it would be inventing music.
    let steps_per_bar = match time_signature {
        Some((numerator, denominator)) => {
            let steps = steps_for(numerator, denominator);
            // Zero means the meter is not a whole number of sixteenths; too many means the bar is
            // longer than one this sequencer holds — 5/4 is twenty sixteenths and a bar is sixteen.
            // **Refused rather than re-barred**: silently turning somebody's 5/4 into bars of a
            // different length is rewriting their music, which is what every other refusal here
            // exists to avoid.
            if steps == 0 || steps > super::pattern::MAX_STEPS_PER_BAR {
                return Err(Refused::TimeSignature {
                    numerator,
                    denominator,
                });
            }
            steps
        }
        None => STEPS,
    };

    let tempo = match tempos.first() {
        Some(micros) => 60_000_000.0 / f64::from(*micros),
        None => super::clock::DEFAULT_TEMPO,
    };
    if !(MIN_TEMPO..=MAX_TEMPO).contains(&tempo) {
        return Err(Refused::TempoOutOfRange(tempo));
    }

    // Everything below is in *our* ticks, so a foreign division is rescaled before the grid check.
    let scale = |tick: u32| tick * TICKS_PER_QUARTER as u32 / ticks_per_quarter;

    let mut channels: Vec<u8> = notes.iter().map(|n| n.channel).collect();
    channels.sort_unstable();
    channels.dedup();
    if channels.len() > 1 {
        return Err(Refused::SeveralChannels(channels));
    }

    // **The pattern is sized from the file**, so a long one imports whole. The furthest event is
    // the end; a file that stops mid-bar rounds up, because a sequence is whole bars.
    let last_tick = notes.iter().map(|note| note.tick).max().unwrap_or(0);
    let total_steps = (last_tick / TICKS_PER_STEP + 1).max(1) as usize;
    let bars = total_steps.div_ceil(steps_per_bar).max(1);
    let mut pattern = Pattern::sized(bars, steps_per_bar);
    let bar = TICKS_PER_STEP * pattern.len() as u32;
    let mut lost: Vec<String> = Vec::new();
    let mut velocities: Vec<u8> = Vec::new();
    let mut open: Vec<(u8, u32)> = Vec::new();
    let mut starts: Vec<(u8, u32, usize)> = Vec::new();
    /// One sounding note, resolved to steps: where it starts and how many steps it lasts.
    ///
    /// `steps` of `None` is the ordinary half-step gate. `Some(m)` is a run of *m* whole steps,
    /// which is a note plus *m* − 1 ties.
    struct Run {
        key: u8,
        step: usize,
        steps: Option<usize>,
    }
    let mut runs: Vec<Run> = Vec::new();
    let mut duplicates = 0usize;
    let mut shortened = 0usize;

    for note in &notes {
        let tick = scale(note.tick);
        if !note.on {
            // Pair it with its start: the length is what says whether this was a plain note or a
            // tied run, so it decides the pattern rather than merely being reported as lost.
            if let Some(index) = starts.iter().rposition(|(key, _, _)| *key == note.key) {
                let (_, start, step) = starts.remove(index);
                let length = tick.saturating_sub(start);

                // **Exactly two lengths are representable, and "near enough" is not one of them.**
                // Half a step is an untied note; a whole number of steps is a run. A length of one
                // and a half steps is perfectly expressible in MIDI and has no form here, so it is
                // refused with the note and the position named. Quantising somebody's DAW export
                // would be rewriting it, not loading it.
                let steps = if length == TICKS_PER_STEP / 2 {
                    None
                } else if length >= TICKS_PER_STEP && length % TICKS_PER_STEP == 0 {
                    Some((length / TICKS_PER_STEP) as usize)
                } else {
                    return Err(Refused::UnrepresentableLength {
                        key: note.key,
                        at: start,
                        ticks: length,
                    });
                };

                // **Exactly one step is the one whole length the model cannot hold**, and it is
                // accepted rather than refused because it is what this player's own file looks
                // like at a legato joint. A run is a note plus *k* ties and lasts *k* + 1 steps, so
                // the shortest run is two; an untied note is half a step. One step falls between
                // them, and it arises the moment a tie carries notes of its own — the joint the
                // writer already reports it cannot keep.
                //
                // So it reads back as an ordinary note, exactly as `write_report` warns, and the
                // shortening is named here rather than left to be discovered by ear.
                if steps == Some(1) {
                    shortened += 1;
                }
                runs.push(Run {
                    key: note.key,
                    step,
                    steps,
                });
            }
            open.retain(|(key, _)| *key != note.key);
            continue;
        }
        if tick >= bar {
            return Err(Refused::LongerThanOneBar { ticks: tick });
        }
        let step = tick / TICKS_PER_STEP;
        let offset = tick % TICKS_PER_STEP;
        let off_grid = offset.min(TICKS_PER_STEP - offset);
        if off_grid > GRID_TOLERANCE {
            return Err(Refused::OffGrid { ticks: tick });
        }
        // Round to the nearer boundary, which is what the tolerance is for.
        let step = if offset * 2 > TICKS_PER_STEP {
            step + 1
        } else {
            step
        };
        if step as usize >= pattern.len() {
            return Err(Refused::LongerThanOneBar { ticks: tick });
        }

        if pattern.step(step as usize).contains(note.key) {
            duplicates += 1;
        }
        pattern.toggle_on(step as usize, note.key);
        velocities.push(note.velocity);
        open.push((note.key, tick));
        starts.push((note.key, tick, step as usize));
    }

    if let Some((key, _)) = open.first() {
        return Err(Refused::UnpairedNote { key: *key });
    }

    // **One gate per step is what bounds what can be imported.** Every note sounding across a given
    // step must agree about that step, so each run states what it needs and a disagreement is
    // refused rather than resolved. Not truncated to the shortest and not extended to the longest:
    // both would be a silent rewrite of somebody's file.
    //
    // This catches more than unequal chords. A note running steps 1 to 3 while a half-step note
    // starts on step 2 needs step 2 tied for the first and untied for the second, and no rule about
    // notes *starting together* would see it.
    let mut required: Vec<Option<bool>> = vec![None; STEPS];
    let mut blamed: Vec<Vec<u8>> = vec![Vec::new(); STEPS];
    for run in &runs {
        // Its own step starts the note, so it is never a continuation.
        let mut wants: Vec<(usize, bool)> = vec![(run.step, false)];
        match run.steps {
            // A plain note closes at its own gate, which only happens when the next step is not a
            // tie — so it requires that too. A one-step note is treated identically: it has no
            // representation of its own and is read as an ordinary note, counted in `shortened`.
            None | Some(1) => wants.push((run.step + 1, false)),
            Some(steps) => {
                for offset in 1..steps {
                    wants.push((run.step + offset, true));
                }
                wants.push((run.step + steps, false));
            }
        }

        for (step, tied) in wants {
            if step >= STEPS {
                continue;
            }
            blamed[step].push(run.key);
            match required[step] {
                Some(existing) if existing != tied => {
                    let mut keys = std::mem::take(&mut blamed[step]);
                    keys.sort_unstable();
                    keys.dedup();
                    return Err(Refused::ConflictingGates { step, keys });
                }
                _ => required[step] = Some(tied),
            }
        }
    }
    for (step, tied) in required.iter().enumerate() {
        pattern.set_tied(step, tied.unwrap_or(false));
    }

    // Everything the model read and cannot keep, named before it is lost.
    if velocities.iter().any(|v| *v != VELOCITY) {
        lost.push("note velocities (the sequencer plays every step at one level)".to_owned());
    }
    // **Note lengths are no longer lost wholesale.** They used to be reported as such whenever any
    // note differed from the gate, because every step played a fixed half-step gate and a longer
    // note had nowhere to go. A whole number of steps is now a tied run and anything fractional is
    // refused above, so the only survivor is the one-step note — the legato joint, which has no
    // representation and is read as an ordinary note.
    if shortened > 0 {
        lost.push(format!(
            "{shortened} note(s) one step long: a legato joint has no representation here, so \
             each was read as an ordinary note"
        ));
    }
    if let Some(channel) = channels.first()
        && *channel != CHANNEL
    {
        lost.push(format!(
            "MIDI channel {} (notes are canonicalised to channel {})",
            channel + 1,
            CHANNEL + 1
        ));
    }
    if duplicates > 0 {
        lost.push(format!(
            "{duplicates} repeated note(s) landing in a step that already held that pitch"
        ));
    }
    ignored.sort_unstable();
    ignored.dedup();
    for kind in ignored {
        lost.push(format!("{kind} (ignored)"));
    }

    Ok(Loaded {
        pattern,
        tempo,
        lost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_pattern() -> Pattern {
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.toggle(0, 67);
        pattern.toggle(4, 63);
        pattern.toggle(15, 48);
        pattern
    }

    #[test]
    fn a_file_we_wrote_reads_back_identically() {
        // The strong promise, and the one that matters for saving your own work.
        let tempo = canonical_tempo(137.0);
        let bytes = write(&a_pattern(), tempo);
        let loaded = read(&bytes).expect("our own file must load");
        assert_eq!(loaded.pattern, a_pattern());
        assert_eq!(loaded.tempo, tempo, "tempo must survive exactly");
    }

    #[test]
    fn tempo_is_canonicalised_so_the_round_trip_is_exact_not_approximate() {
        // SMF stores integer microseconds per quarter, so an arbitrary f64 BPM cannot survive.
        // Saving snaps to what the file can hold, which makes "identical" true.
        let canonical = canonical_tempo(137.0);
        assert_ne!(canonical, 137.0, "137 BPM is not representable");
        assert_eq!(
            canonical_tempo(canonical),
            canonical,
            "but it is a fixpoint"
        );
    }

    #[test]
    fn the_header_is_a_type_0_file_with_one_track() {
        let bytes = write(&a_pattern(), 120.0);
        assert_eq!(&bytes[0..4], b"MThd");
        assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), 6);
        assert_eq!(u16::from_be_bytes(bytes[8..10].try_into().unwrap()), 0);
        assert_eq!(u16::from_be_bytes(bytes[10..12].try_into().unwrap()), 1);
        assert_eq!(
            u16::from_be_bytes(bytes[12..14].try_into().unwrap()),
            TICKS_PER_QUARTER
        );
        assert_eq!(&bytes[14..18], b"MTrk");
    }

    #[test]
    fn an_empty_pattern_still_writes_a_full_bar() {
        // So the file loops in a DAW even when the last steps rest.
        let bytes = write(&Pattern::empty(), 120.0);
        let loaded = read(&bytes).expect("an empty pattern is a valid file");
        assert!(loaded.pattern.is_empty());
    }

    #[test]
    fn a_chord_survives() {
        let loaded = read(&write(&a_pattern(), 120.0)).unwrap();
        assert_eq!(loaded.pattern.step(0).count(), 2);
    }

    #[test]
    fn something_that_is_not_a_midi_file_is_refused() {
        assert_eq!(read(b"not midi at all"), Err(Refused::NotAMidiFile));
        assert_eq!(read(&[]), Err(Refused::NotAMidiFile));
    }

    #[test]
    fn a_truncated_file_is_refused_rather_than_half_read() {
        let bytes = write(&a_pattern(), 120.0);
        for cut in [20, 30, bytes.len() - 3] {
            assert!(read(&bytes[..cut]).is_err(), "cut at {cut} should refuse");
        }
    }

    #[test]
    fn a_tempo_outside_the_supported_range_is_refused_not_clamped() {
        // The runtime clamps, so loading 400 BPM as 300 would report a success that did not happen.
        let bytes = write(&a_pattern(), 120.0);
        let mut hacked = bytes.clone();
        // Rewrite the tempo meta payload to 400 BPM.
        let micros = micros_per_quarter(400.0).to_be_bytes();
        let at = hacked
            .windows(3)
            .position(|w| w == [0xff, 0x51, 0x03])
            .expect("a tempo event")
            + 3;
        hacked[at..at + 3].copy_from_slice(&micros[1..]);
        assert!(matches!(
            read(&hacked),
            Err(Refused::TempoOutOfRange(bpm)) if bpm > 300.0
        ));
    }

    #[test]
    fn a_meter_that_is_not_whole_sixteenths_is_refused_and_the_rest_are_not() {
        // **3/4 used to be refused and is not any more.** A bar is however many sixteenths it holds,
        // so the meter says the shape rather than having to be one particular shape. What cannot be
        // read is a bar that is not a whole number of sixteenths — importing 3/32 as something near
        // it would be inventing music.
        let bytes = write(&a_pattern(), 120.0);
        let at = bytes
            .windows(3)
            .position(|w| w == [0xff, 0x58, 0x04])
            .expect("a time signature")
            + 3;

        let mut three_four = bytes.clone();
        three_four[at] = 3; // 3/4, which is twelve sixteenths
        let loaded = read(&three_four).expect("3/4 is a whole number of sixteenths");
        assert_eq!(
            loaded.pattern.steps_per_bar(),
            12,
            "a 3/4 bar is twelve sixteenths"
        );

        let mut three_thirtyseconds = bytes.clone();
        three_thirtyseconds[at] = 3;
        three_thirtyseconds[at + 1] = 5; // denominator 2^5 = 32
        assert!(matches!(
            read(&three_thirtyseconds),
            Err(Refused::TimeSignature { .. })
        ));
    }

    #[test]
    fn the_meter_written_is_the_meter_the_bar_is_in() {
        // Sixteen sixteenths is 4/4 and not 16/16, because a DAW shows what the file says.
        assert_eq!(meter_for(16), Some((4, 4)));
        assert_eq!(meter_for(12), Some((3, 4)));
        assert_eq!(meter_for(10), Some((5, 8)));
        assert_eq!(meter_for(7), Some((7, 16)));

        // **Every bar, rather than a range somebody liked the look of** — and the property is a
        // conditional one, because how far the one-byte numerator reaches depends on how far the
        // bar reduces. Whatever can be named must read back as itself.
        for steps in 1..=(MAX_METER_STEPS + 64) {
            if let Some((numerator, denominator)) = meter_for(steps) {
                assert_eq!(steps_for(numerator, denominator), steps, "{steps} steps");
            }
        }

        // The three ceilings, which are the format's and not ours.
        assert!(
            meter_for(1020).is_some(),
            "255/4 is the furthest a bar reaches"
        );
        assert_eq!(meter_for(1024), None);
        assert!(
            meter_for(510).is_some(),
            "an even bar that is not a multiple of four: 255/8"
        );
        assert_eq!(meter_for(514), None);
        assert!(meter_for(255).is_some(), "an odd bar: 255/16");
        assert_eq!(meter_for(257), None);
    }

    #[test]
    fn a_meter_the_format_cannot_name_is_none_rather_than_truncated() {
        // **The guard is about the format, not about us.** A bar is capped at
        // `MAX_STEPS_PER_BAR` now, so no pattern can reach this — but `meter_for` answers a question
        // about MIDI, and the first version of it cast a `u32` to `u8` and turned a 1,024-step bar
        // into `0/4`: a file that reads back as no steps at all. Kept and tested directly, because
        // the cap is somebody's decision and this is arithmetic.
        assert_eq!(meter_for(MAX_METER_STEPS + 4), None);
        assert!(meter_for(MAX_METER_STEPS).is_some());
    }

    #[test]
    fn a_meter_longer_than_a_bar_is_refused_rather_than_re_barred() {
        // 5/4 is twenty sixteenths and a bar holds sixteen. Turning it into bars of a different
        // length would be rewriting somebody's music, which is what every refusal here avoids.
        let bytes = write(&a_pattern(), 120.0);
        let at = bytes
            .windows(3)
            .position(|w| w == [0xff, 0x58, 0x04])
            .expect("a time signature")
            + 3;

        let mut five_four = bytes.clone();
        five_four[at] = 5; // 5/4 = twenty sixteenths
        assert!(matches!(
            read(&five_four),
            Err(Refused::TimeSignature { .. })
        ));

        // Four is the longest bar there is, and it still reads.
        assert!(read(&bytes).is_ok());
    }

    #[test]
    fn a_long_sequence_survives_a_midi_round_trip() {
        // **Import refused anything past one bar** and export ended the track at one bar, both from
        // when a sequence was always one. A file is as long as the music now, and reading one back
        // sizes the pattern from what is in it.
        let mut pattern = Pattern::sized(8, 12);
        pattern.toggle(0, 60);
        pattern.toggle(50, 64);
        let last = pattern.len() - 1;
        pattern.toggle(last, 67);

        let bytes = write(&pattern, 120.0);
        let loaded = read(&bytes).expect("it reads back");

        assert_eq!(
            loaded.pattern.steps_per_bar(),
            12,
            "the meter carried the shape"
        );
        assert_eq!(
            loaded.pattern.len(),
            pattern.len(),
            "and the length carried too"
        );
        assert!(loaded.pattern.step(0).contains(60));
        assert!(loaded.pattern.step(50).contains(64));
        assert!(
            loaded.pattern.step(last).contains(67),
            "a note in the last bar is not lost"
        );
    }

    #[test]
    fn the_variable_length_encoding_matches_the_specification() {
        // The spec's own examples. Getting this wrong would produce a file that looks fine to our
        // reader and nothing else.
        for (value, expected) in [
            (0u32, vec![0x00u8]),
            (0x40, vec![0x40]),
            (0x7f, vec![0x7f]),
            (0x80, vec![0x81, 0x00]),
            (0x2000, vec![0xc0, 0x00]),
            (0x3fff, vec![0xff, 0x7f]),
            (0x100000, vec![0xc0, 0x80, 0x00]),
        ] {
            let mut out = Vec::new();
            push_varint(&mut out, value);
            assert_eq!(out, expected, "varint for {value:#x}");
        }
    }

    #[test]
    fn a_note_off_before_a_note_on_at_the_same_tick() {
        // Otherwise a pitch repeated on consecutive steps is ambiguous.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.toggle(1, 60);
        let loaded = read(&write(&pattern, 120.0)).expect("loads");
        assert!(loaded.pattern.step(0).contains(60));
        assert!(loaded.pattern.step(1).contains(60));
    }

    #[test]
    fn a_file_with_a_different_velocity_reports_the_velocity_and_nothing_else() {
        // Only what actually changed. Reporting note-length loss here too — which an earlier
        // version did unconditionally — was untrue, and it added a row that moved the interface.
        let mut bytes = write(&a_pattern(), 120.0);
        let at = bytes
            .windows(2)
            .position(|w| w[0] == 0x90 && w[1] == 60)
            .expect("a note-on");
        bytes[at + 2] = 42;

        let loaded = read(&bytes).expect("still loadable");
        assert!(
            loaded.lost.iter().any(|s| s.contains("velocit")),
            "velocity loss must be named: {:?}",
            loaded.lost
        );
        assert!(
            !loaded.lost.iter().any(|s| s.contains("length")),
            "the note lengths did not change: {:?}",
            loaded.lost
        );
    }

    /// A file holding exactly these notes, each `(step, key, length in ticks)`.
    ///
    /// Built from absolute ticks rather than by patching a written file: delta times are relative,
    /// so editing one moves everything after it.
    fn midi_with(notes: &[(usize, u8, u32)]) -> Vec<u8> {
        let mut track = Vec::new();
        push_varint(&mut track, 0);
        track.extend_from_slice(&[0xff, 0x51, 0x03]);
        track.extend_from_slice(&micros_per_quarter(120.0).to_be_bytes()[1..]);
        push_varint(&mut track, 0);
        track.extend_from_slice(&[0xff, 0x58, 0x04, 4, 2, 24, 8]);

        let mut events = Vec::new();
        for (step, key, length) in notes {
            let start = *step as u32 * TICKS_PER_STEP;
            events.push((start, 0x90 | CHANNEL, *key));
            events.push((start + length, 0x80 | CHANNEL, *key));
        }
        finish(track, events, STEPS)
    }

    #[test]
    fn a_note_lasting_two_steps_reads_back_as_a_note_and_a_tie() {
        // This used to be reported as a *loss* -- "note lengths (the sequencer uses a fixed
        // gate)" -- because every step played a fixed half-step gate and a longer note had
        // nowhere to go. It has somewhere to go now.
        let loaded = read(&midi_with(&[(0, 60, TICKS_PER_STEP * 2)])).expect("still loadable");
        assert!(
            loaded.pattern.tied(1),
            "a two-step note is a note plus one tie"
        );
        assert!(
            loaded.lost.is_empty(),
            "and nothing is lost: {:?}",
            loaded.lost
        );
    }

    #[test]
    fn a_note_lasting_exactly_one_step_is_read_as_an_ordinary_note_and_said_so() {
        // **The one whole length the model cannot hold.** A run is a note plus k ties and lasts
        // k + 1 steps, so the shortest run is two steps; an untied note is half a step. One step
        // falls between them, and it is exactly what this player writes at a legato joint.
        //
        // Accepted rather than refused -- refusing would reject a file the player itself produced
        // -- and the shortening is named, because the alternative is somebody noticing by ear.
        let loaded = read(&midi_with(&[(0, 60, TICKS_PER_STEP)])).expect("it must still load");

        assert!(!loaded.pattern.tied(1), "read as an ordinary note");
        assert!(loaded.pattern.step(0).contains(60), "the pitch survives");
        assert!(
            loaded.lost.iter().any(|s| s.contains("one step long")),
            "the shortening must be named: {:?}",
            loaded.lost
        );
    }

    /// Writes a tied pattern to the path in `MXM_SMF_OUT`, for `tests/midi-conformance/verify.py`.
    ///
    /// **Not a test — a facility**, which is why it is `#[ignore]`d and asserts nothing. The
    /// conformance script parses what we write with a separate implementation that shares no code
    /// with this module, and it needs a real file. Doing that by hand meant "save a tied pattern
    /// from the player first", which is a step easy to skip and impossible to repeat exactly.
    ///
    /// ```text
    /// MXM_SMF_OUT=tied.mid cargo test -p mxm-player --lib writes_a_tied_pattern -- --ignored
    /// python apps/mxm-player/tests/midi-conformance/verify.py tied.mid foreign.mid
    /// ```
    #[test]
    #[ignore = "writes a file for the conformance script; run it explicitly"]
    fn writes_a_tied_pattern_for_the_conformance_script() {
        let Ok(path) = std::env::var("MXM_SMF_OUT") else {
            eprintln!("set MXM_SMF_OUT to the path to write");
            return;
        };

        // Everything the writer can produce: a plain note, a run of three, a two-step run, and a
        // legato joint. The last is the case the script most needs to see, because it is the one
        // written as two notes at a single tick.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true);
        pattern.set_tied(2, true);
        pattern.toggle(4, 67);
        pattern.toggle(6, 62);
        pattern.set_tied(7, true);
        pattern.toggle(10, 69);
        pattern.toggle(11, 64);
        pattern.set_tied(11, true);

        std::fs::write(&path, write(&pattern, 120.0)).expect("write the file");
        eprintln!("wrote {path}");
        for problem in write_report(&pattern) {
            eprintln!("  reported: {problem}");
        }
    }

    #[test]
    fn a_tied_run_round_trips_through_midi_as_one_long_note() {
        for ties in 1..=3usize {
            let mut pattern = Pattern::empty();
            pattern.toggle(0, 60);
            for step in 1..=ties {
                pattern.set_tied(step, true);
            }

            let bytes = write(&pattern, 120.0);
            let loaded = read(&bytes).expect("our own file must load");
            assert_eq!(
                loaded.pattern, pattern,
                "a run of {ties} tie(s) must survive the file unchanged"
            );
            assert!(loaded.lost.is_empty(), "{:?}", loaded.lost);
        }
    }

    #[test]
    fn a_length_that_is_neither_half_a_step_nor_a_whole_number_is_refused_by_position() {
        // One and a half steps. Perfectly expressible in MIDI, and it has no form here -- so it is
        // refused with the note and the tick named, exactly as an off-grid onset already is.
        // Quantising it would be rewriting somebody's export, not loading it.
        let bytes = midi_with(&[(0, 60, TICKS_PER_STEP + TICKS_PER_STEP / 2)]);
        let refused = read(&bytes).expect_err("one and a half steps has no representation");
        let Refused::UnrepresentableLength { key, ticks, .. } = refused else {
            panic!("expected an unrepresentable length, got {refused}");
        };
        assert_eq!(key, 60);
        assert_eq!(ticks, TICKS_PER_STEP + TICKS_PER_STEP / 2);
        assert!(
            refused.to_string().contains(&ticks.to_string()),
            "the reason must name the length: {refused}"
        );
    }

    #[test]
    fn a_chord_whose_notes_end_at_different_times_is_refused() {
        // One gate per step: two notes crossing step 2 cannot have it tied and untied at once.
        let bytes = midi_with(&[(0, 60, TICKS_PER_STEP * 2), (0, 67, TICKS_PER_STEP / 2)]);
        let refused = read(&bytes).expect_err("the chord disagrees about step 2");
        let Refused::ConflictingGates { step, keys } = &refused else {
            panic!("expected conflicting gates, got {refused}");
        };
        assert_eq!(*step, 1, "step 2 is where they disagree");
        assert!(keys.contains(&60) && keys.contains(&67), "{keys:?}");
    }

    #[test]
    fn a_staggered_overlap_is_refused_even_though_the_notes_do_not_start_together() {
        // The case an "unequal chord" rule misses entirely: a note across steps 1 to 3 and a
        // half-step note starting on step 2. Step 2 must be tied for the first and untied for the
        // second, and nothing about their *onsets* would reveal it.
        let bytes = midi_with(&[(0, 60, TICKS_PER_STEP * 3), (1, 67, TICKS_PER_STEP / 2)]);
        let refused = read(&bytes).expect_err("step 2 is claimed twice");
        assert!(
            matches!(refused, Refused::ConflictingGates { step: 1, .. }),
            "expected a conflict at step 2, got {refused}"
        );
    }

    #[test]
    fn saving_a_legato_joint_reports_that_it_cannot_be_kept() {
        // A tie carrying its own notes is a note ending exactly where the next begins, with no
        // retrigger. MIDI can say "back to back"; it cannot say "and do not retrigger". Reading it
        // back therefore gives an ordinary note, and the *report* is the only place that is said.
        let mut pattern = Pattern::empty();
        pattern.toggle(0, 60);
        pattern.set_tied(1, true);
        pattern.toggle(1, 67);

        let report = write_report(&pattern);
        assert!(
            report.iter().any(|s| s.contains("legato")),
            "the loss must be named on save: {report:?}"
        );

        let loaded = read(&write(&pattern, 120.0)).expect("it still loads");
        assert!(
            !loaded.pattern.tied(1),
            "the joint reads back as an ordinary note, which is what the report warned about"
        );
    }

    #[test]
    fn saving_a_run_that_wraps_the_loop_point_reports_the_truncation() {
        // A note on step 16 tied by a tie on step 1 carries across the repeat. This file is one
        // bar, so the run is cut at the bar end. Refusing a pattern that plays correctly would be
        // worse; saying nothing would be the silent rewrite.
        let mut pattern = Pattern::empty();
        pattern.toggle(STEPS - 1, 60);
        pattern.set_tied(0, true);

        let report = write_report(&pattern);
        assert!(
            report.iter().any(|s| s.contains("truncated")),
            "the truncation must be named: {report:?}"
        );
        assert!(
            report.iter().any(|s| s.contains("16")),
            "and the step with it: {report:?}"
        );
    }

    #[test]
    fn our_own_file_reports_no_velocity_loss() {
        let loaded = read(&write(&a_pattern(), 120.0)).unwrap();
        assert!(
            !loaded.lost.iter().any(|s| s.contains("velocit")),
            "we wrote the canonical velocity: {:?}",
            loaded.lost
        );
    }
}
