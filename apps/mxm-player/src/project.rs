//! The player's setup as a tutti project: the instrument, its effects and the sequence, in the open
//! audio project format newDAWn's songs are in (the owner, 2026-10-21: "audio plugins, effects
//! plugins, musical data").
//!
//! # What goes in, and how it comes back
//!
//! - **One track**: the instrument first and the effects after it, in chain order, each with its
//!   CLAP state as one of the project's state files and an effect that is off written disabled.
//!   Every plugin is one of the project's dependencies.
//! - **The sequence as notes**, at the format's 960 ticks a quarter: a step is a sixteenth, and the
//!   notes are the ones a `.mid` holds ([`smf::note_spans`]), so every program plays them. One part
//!   plays the pattern from the start, and the song's loop is the pattern, as the player plays it.
//! - **The player's own sequence beside the notes**, under its extension [`NAMESPACE`] on the
//!   pattern: the ties, the shape and the parameter locks, which notes cannot say. Opening uses it
//!   while the notes are still the ones it describes. Notes another program changed are read as
//!   notes, through the grid rules a `.mid` passes ([`smf::from_notes`]), and the locks saved with
//!   the old notes are left out, which the player says.
//!
//! # Saving over a project the player opened
//!
//! The player writes back only what it read whole. The track it opened is updated in place, by id,
//! and everything else in the document (other tracks, another program's extensions, keys this
//! version does not know) stays as it was. A project it could not read whole (a device it did not
//! load, more than one part or pattern on the track, notes off the grid, events other than notes, a
//! tempo or meter that changes, another `ppq`) is saved under a new name instead, and
//! [`Place::refused`] says why.

use crate::engine::fx::FxId;
use crate::sequencer::clock::{MAX_TEMPO, MIN_TEMPO};
use crate::sequencer::pattern::Pattern;
use crate::sequencer::sequence::Sequence;
use crate::sequencer::smf;
use std::path::{Path, PathBuf};
use tutti::{Document, Id};
use tutti_fs::DirStore;

/// A project on disk, as the format's crate holds it.
pub type Project = tutti::Project<DirStore>;

/// The player's extension: its own sequence, on the pattern.
pub const NAMESPACE: &str = "dk.mxm.player";

/// The format's ticks a quarter that the player writes.
pub const PPQ: u32 = 960;

/// The state files' kind: the CLAP state stream, as newDAWn writes it.
pub const STATE_KIND: &str = "clap";

/// One step, a sixteenth, in the format's ticks.
pub const TICKS_PER_STEP: i64 = PPQ as i64 / 4;

/// The format's ticks per tick of [`smf`], whose division the note spans are in.
const SCALE: i64 = (PPQ / smf::TICKS_PER_QUARTER as u32) as i64;

/// The velocity a new note is written with: the one the sequencer plays and a `.mid` holds. A
/// note the project had keeps its own.
pub const VELOCITY: f64 = smf::VELOCITY as f64 / 127.0;

/// The longest sequence a project opens into, in bars of sixteen steps. Far past anything sequenced
/// by step, and a bound, because a project another program wrote may be any length and the step
/// pattern holds every step.
pub const MAX_BARS: i64 = 1024;

/// The player, as the project's last writer.
pub fn generator() -> tutti::Generator {
    tutti::Generator::new(
        "MXM Player",
        env!("CARGO_PKG_VERSION"),
        "https://github.com/mxm-audio/mxm-player",
    )
}

/// A plugin as the format names it: a bare CLAP id gets its scheme; a VST3 id has its own.
pub fn format_plugin(id: &str) -> String {
    if tutti::validate::PLUGIN_SCHEMES
        .iter()
        .any(|scheme| id.starts_with(scheme))
    {
        id.to_owned()
    } else {
        format!("clap:{id}")
    }
}

/// The player's id for a plugin the format names, or `None` for a scheme it cannot host.
pub fn player_plugin(plugin: &str) -> Option<String> {
    if let Some(id) = plugin.strip_prefix("clap:") {
        Some(id.to_owned())
    } else if plugin.starts_with(mxm_vst3_host::ID_PREFIX) {
        Some(plugin.to_owned())
    } else {
        None
    }
}

/// A new id of the format's form: a kind, then a ULID, unique across projects and machines.
pub fn new_id(prefix: &str) -> Id {
    tutti_fs::ulid(prefix)
}

/// One plugin of the setup, as the player has it now.
#[derive(Clone, Debug, PartialEq)]
pub struct Plugin {
    /// The player's id for it (a CLAP id, or `vst3:` and a class id).
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// Its CLAP state, when it gave one.
    pub state: Option<Vec<u8>>,
    /// An effect that is off is not called; an instrument is always on.
    pub enabled: bool,
    /// The device it is in the project the player opened, kept on a save over it.
    pub device: Option<Id>,
}

/// What the player saves.
#[derive(Clone, Debug)]
pub struct Setup {
    pub title: String,
    pub instrument: Option<Plugin>,
    pub effects: Vec<Plugin>,
    /// The player's own sequence, locks included: the extension.
    pub sequence: Sequence,
    /// The pattern the notes are written from, and the tempo.
    pub pattern: Pattern,
    pub tempo: f64,
}

/// The track, pattern and part the player owns in a project, and whether saving over it is safe.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Place {
    pub track: Option<Id>,
    pub pattern: Option<Id>,
    pub part: Option<Id>,
    /// Why saving over this project would lose something, when it would.
    pub refused: Option<String>,
}

/// A project the player opened or saved, which a save under its name goes over.
pub struct OpenProject {
    pub folder: PathBuf,
    pub project: Project,
    pub place: Place,
    /// The instrument's device, with the plugin it was.
    pub instrument: Option<(String, Id)>,
    /// Each effect's device, by the effect's id in the chain.
    pub effects: Vec<(FxId, Id)>,
}

impl OpenProject {
    /// A project just made, owning nothing yet.
    pub fn new(folder: PathBuf, project: Project) -> Self {
        Self {
            folder,
            project,
            place: Place::default(),
            instrument: None,
            effects: Vec::new(),
        }
    }
}

/// The setup written into the project's document, in place: the track the player owns updated by
/// id, or a new one. The states are put in the folder; the document is written by the caller's
/// save. Gives each plugin's device, the instrument's first, in the setup's order.
pub fn write(open: &mut OpenProject, setup: &Setup) -> Result<Vec<Id>, String> {
    if let Some(reason) = &open.place.refused {
        return Err(format!(
            "saving over {} would lose what the player couldn't read ({reason}); save it under \
             another name",
            open.folder.display()
        ));
    }
    // The states first: they are files of the project, and a device names one by its hash.
    let mut states: Vec<Option<tutti::StateRef>> = Vec::new();
    for plugin in setup.instrument.iter().chain(&setup.effects) {
        states.push(match &plugin.state {
            Some(bytes) => Some(
                open.project
                    .put_state(STATE_KIND, bytes)
                    .map_err(|e| format!("{}'s state could not be kept: {e}", plugin.name))?,
            ),
            None => None,
        });
    }
    let mut states = states.into_iter();

    let doc = open.project.document_mut();
    let track_id = match &open.place.track {
        Some(id) => id.clone(),
        None => {
            let id = new_id("trk");
            let name = setup
                .instrument
                .as_ref()
                .map_or_else(|| setup.title.clone(), |plugin| plugin.name.clone());
            doc.tracks.push(tutti::Track::new(id.clone(), &name));
            id
        }
    };
    let track = doc.track_mut(&track_id).ok_or_else(|| {
        format!(
            "the track {} is no longer in the project",
            track_id.as_str()
        )
    })?;

    // The chain: the instrument, then the effects, each the device it was where it was one.
    let mut old = std::mem::take(&mut track.devices);
    let mut devices = Vec::new();
    let mut ids = Vec::new();
    for (plugin, role) in setup
        .instrument
        .iter()
        .map(|plugin| (plugin, "instrument"))
        .chain(setup.effects.iter().map(|plugin| (plugin, "audio_effect")))
    {
        let id = plugin.device.clone().unwrap_or_else(|| new_id("dev"));
        let mut device = match old.iter().position(|device| device.id == id) {
            Some(at) => old.remove(at),
            None => tutti::Device::new(id.clone(), role, ""),
        };
        let plugin_id = format_plugin(&plugin.id);
        if device.plugin != plugin_id {
            device.aliases.clear();
        }
        device.role = role.to_owned();
        device.name = plugin.name.clone();
        device.plugin = plugin_id;
        device.state = states.next().flatten();
        // The snapshot described the state that was there; the new one has none.
        device.params.clear();
        device.overrides.clear();
        device.enabled = plugin.enabled;
        ids.push(device.id.clone());
        devices.push(device);
    }
    track.devices = devices;

    // The notes, each the event it was where one sat at the same tick and pitch.
    let length = setup.pattern.len() as i64 * TICKS_PER_STEP;
    let pattern_id = open.place.pattern.clone().unwrap_or_else(|| new_id("pat"));
    let mut pattern = match track.patterns.iter().position(|p| p.id == pattern_id) {
        Some(at) => track.patterns.remove(at),
        None => tutti::Pattern::new(pattern_id.clone(), length),
    };
    let old_length = open.place.pattern.as_ref().map(|_| pattern.length);
    let mut old_notes: Vec<tutti::Note> = pattern
        .events
        .iter()
        .filter_map(|event| match event {
            tutti::Event::Note(note) if note.audio.is_none() => Some(note.clone()),
            _ => None,
        })
        .collect();
    pattern.length = length;
    pattern.name = setup.sequence.name.clone();
    pattern.events = notes(&setup.pattern)
        .into_iter()
        .map(|(at, note_length, pitch)| {
            // The note that sat at this tick and pitch stays that note: its id, its velocity and
            // release, and whatever another program keeps on it. A new one plays at the
            // sequencer's level.
            let mut note = match old_notes
                .iter()
                .position(|old| old.at == at && old.pitch == pitch)
            {
                Some(found) => old_notes.remove(found),
                None => tutti::Note::new(new_id("evt"), at, note_length, pitch, VELOCITY),
            };
            note.length = note_length;
            tutti::Event::Note(note)
        })
        .collect();
    let sequence = serde_json::to_value(&setup.sequence).map_err(|e| e.to_string())?;
    pattern.extensions.insert(
        NAMESPACE.to_owned(),
        serde_json::json!({ "sequence": sequence }),
    );
    track.patterns.push(pattern);

    let part_id = open.place.part.clone().unwrap_or_else(|| new_id("prt"));
    let mut part = match track.parts.iter().position(|p| p.id == part_id) {
        Some(at) => track.parts.remove(at),
        None => tutti::Part::new(part_id.clone(), pattern_id.clone(), 0, length),
    };
    part.pattern = pattern_id.clone();
    part.at = 0;
    part.length = length;
    part.offset = 0;
    part.r#loop = None;
    track.parts.push(part);

    write_time(doc, setup, length, old_length);
    set_dependencies(doc, setup);
    if doc.metadata.title.is_none() {
        doc.metadata.title = Some(setup.title.clone());
    }

    open.place.track = Some(track_id);
    open.place.pattern = Some(pattern_id);
    open.place.part = Some(part_id);
    Ok(ids)
}

/// The tempo and the meter, one point each, and the loop: the pattern in a new project, following
/// the pattern where it was the pattern, and another program's own left as it is.
fn write_time(doc: &mut Document, setup: &Setup, length: i64, old_length: Option<i64>) {
    doc.ppq = PPQ;
    match doc.tempo.as_mut_slice() {
        [point] if point.at == 0 => point.bpm = setup.tempo,
        _ => doc.tempo = vec![tutti::TempoPoint::new(0, setup.tempo)],
    }
    // Every bar the sequencer holds is a whole number of sixteenths a byte of numerator reaches.
    let (numerator, denominator) = smf::meter_for(setup.pattern.steps_per_bar()).unwrap_or((4, 4));
    match doc.meter.as_mut_slice() {
        [point] if point.at == 0 => {
            point.numerator = u32::from(numerator);
            point.denominator = u32::from(denominator);
        }
        _ => {
            doc.meter = vec![tutti::MeterPoint::new(
                0,
                u32::from(numerator),
                u32::from(denominator),
            )]
        }
    }
    match &mut doc.r#loop {
        None => {
            doc.r#loop = Some(tutti::Loop {
                start: 0,
                end: length,
                enabled: true,
                unknown: Default::default(),
            })
        }
        Some(range) if range.start == 0 && Some(range.end) == old_length => range.end = length,
        Some(_) => {}
    }
}

/// What the player knows of its plugins, for their dependency entries: the name, the vendor and the
/// format. An entry the document has already is left as it is; the format's crate keeps the list
/// in step with the devices when the project is saved (one no device uses goes).
fn set_dependencies(doc: &mut Document, setup: &Setup) {
    for plugin in setup.instrument.iter().chain(&setup.effects) {
        let id = format_plugin(&plugin.id);
        if doc.dependencies.iter().any(|entry| entry.id == id) {
            continue;
        }
        let format = if plugin.id.starts_with(mxm_vst3_host::ID_PREFIX) {
            "vst3"
        } else {
            "clap"
        };
        let mut entry = tutti::Dependency::plugin(&id, &plugin.name);
        entry.vendor = (!plugin.vendor.is_empty()).then(|| plugin.vendor.clone());
        entry.formats = vec![format.to_owned()];
        doc.dependencies.push(entry);
    }
}

/// The notes a pattern plays, as `(at, length, pitch)` in the format's ticks.
pub fn notes(pattern: &Pattern) -> Vec<(i64, i64, u8)> {
    smf::note_spans(pattern)
        .into_iter()
        .map(|(start, end, key)| {
            let at = i64::from(start) * SCALE;
            (at, i64::from(end) * SCALE - at, key)
        })
        .collect()
}

// --- reading ---------------------------------------------------------------------------------

/// A device of the project, as the player will load it.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub id: Id,
    /// The player's id for its plugin; `None` for a scheme the player cannot host.
    pub plugin: Option<String>,
    /// The plugin as the project names it.
    pub named: String,
    pub name: String,
    pub state: Option<Vec<u8>>,
    pub enabled: bool,
}

/// The musical data, as the player can hold it.
#[derive(Clone, Debug)]
pub enum Music {
    /// The player's own sequence, the notes still the ones it describes: exact, locks included.
    Sequence(Sequence),
    /// Notes another program wrote or changed, through the step grid.
    Notes(smf::Loaded),
    /// Nothing on the track plays.
    Nothing,
}

/// What opening a project found.
#[derive(Debug)]
pub struct Read {
    pub place: Place,
    pub instrument: Option<Device>,
    pub effects: Vec<Device>,
    /// The music, or why it does not fit the step sequencer.
    pub music: Result<Music, String>,
    pub tempo: f64,
    /// Everything the player could not honour, for the user.
    pub problems: Vec<String>,
}

/// Reads the first track with an instrument: its chain, its music and the time it plays in.
pub fn read(project: &Project) -> Result<Read, String> {
    let doc = project.document();
    let tracks = doc.all_tracks();
    let track = tracks
        .iter()
        .find(|track| {
            track
                .devices
                .iter()
                .any(|d| d.effective_role() == tutti::Role::Instrument)
        })
        .copied()
        .ok_or("the project has no track with an instrument for the player to play")?;
    let mut problems = Vec::new();
    // What saving over the project would lose: the first reason is the one given.
    let mut refused: Vec<String> = Vec::new();
    let others = tracks.len() - 1;
    if others > 0 {
        problems.push(format!(
            "the player plays the track \"{}\"; the project's other {others} track(s) are kept, \
             not played",
            track.name
        ));
    }

    let mut instrument = None;
    let mut effects = Vec::new();
    for device in &track.devices {
        let role = device.effective_role();
        let wanted = match role {
            tutti::Role::Instrument => instrument.is_none(),
            tutti::Role::AudioEffect => true,
            _ => false,
        };
        if !wanted {
            problems.push(format!(
                "{} ({}) is a device the player doesn't host, kept in the project",
                display_name(device),
                device.role
            ));
            refused.push(format!("{} isn't hosted", display_name(device)));
            continue;
        }
        let state = match &device.state {
            Some(state) if state.kind == STATE_KIND => match project.read(&state.path()) {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    problems.push(format!(
                        "{}'s state could not be read ({error}): it plays its default",
                        display_name(device)
                    ));
                    refused.push(format!("{}'s state is missing", display_name(device)));
                    None
                }
            },
            Some(state) => {
                problems.push(format!(
                    "{}'s state is a .{} file, which the player can't give it: it plays its default",
                    display_name(device),
                    state.kind
                ));
                refused.push(format!(
                    "{}'s state is a .{}",
                    display_name(device),
                    state.kind
                ));
                None
            }
            None => {
                if !device.params.is_empty() {
                    problems.push(format!(
                        "{} has parameter values but no state, which the player can't apply: it \
                         plays its default",
                        display_name(device)
                    ));
                    refused.push(format!("{}'s parameter values", display_name(device)));
                }
                None
            }
        };
        if !device.overrides.is_empty() {
            problems.push(format!(
                "{} has edits another program made without its plugin, which the player can't \
                 apply",
                display_name(device)
            ));
            refused.push(format!("{}'s edits", display_name(device)));
        }
        let read = Device {
            id: device.id.clone(),
            plugin: player_plugin(&device.plugin),
            named: device.plugin.clone(),
            name: display_name(device),
            state,
            enabled: device.enabled,
        };
        if read.plugin.is_none() {
            problems.push(format!(
                "{} is a {} plugin, which the player can't host",
                read.name,
                device.plugin.split(':').next().unwrap_or("")
            ));
            refused.push(format!("{} isn't hosted", read.name));
        }
        if role == tutti::Role::Instrument {
            instrument = Some(read);
        } else {
            effects.push(read);
        }
    }

    // The time.
    if doc.ppq != PPQ {
        refused.push(format!("its ppq is {}", doc.ppq));
    }
    let mut tempo = doc.tempo.first().map_or(120.0, |point| point.bpm);
    if doc.tempo.len() > 1 {
        problems.push("the tempo changes; the player plays the first".to_owned());
        refused.push("its tempo changes".to_owned());
    }
    if !tempo.is_finite() || !(MIN_TEMPO..=MAX_TEMPO).contains(&tempo) {
        problems.push(format!(
            "the tempo is {tempo:.1} BPM; the player runs between {MIN_TEMPO} and {MAX_TEMPO}"
        ));
        refused.push("its tempo is out of the player's range".to_owned());
        tempo = if tempo.is_finite() {
            tempo.clamp(MIN_TEMPO, MAX_TEMPO)
        } else {
            crate::sequencer::DEFAULT_TEMPO
        };
    }
    if doc.meter.len() > 1 {
        problems.push("the meter changes; the player plays the first".to_owned());
        refused.push("its meter changes".to_owned());
    }

    // The part and its pattern.
    let mut place = Place {
        track: Some(track.id.clone()),
        ..Place::default()
    };
    if track.parts.len() > 1 || track.patterns.len() > 1 {
        problems.push(format!(
            "the track has {} part(s) and {} pattern(s); the player plays the first part",
            track.parts.len(),
            track.patterns.len()
        ));
        refused.push("its track has more than one part or pattern".to_owned());
    }
    let part = track.parts.iter().min_by_key(|part| part.at);
    let pattern = part.and_then(|part| track.patterns.iter().find(|p| p.id == part.pattern));
    let music = match (part, pattern) {
        (Some(part), Some(pattern)) => {
            place.part = Some(part.id.clone());
            place.pattern = Some(pattern.id.clone());
            let whole = part.at == 0
                && part.offset == 0
                && part.length == pattern.length
                && !part.mute
                && part
                    .r#loop
                    .as_ref()
                    .is_none_or(|range| range.start == 0 && range.end == pattern.length);
            if !whole {
                problems.push(
                    "the part plays the pattern from elsewhere than its start, or not whole; the \
                     player plays the pattern"
                        .to_owned(),
                );
                refused.push("its part isn't the whole pattern".to_owned());
            }
            music_of(doc, pattern, tempo, &mut problems, &mut refused)
        }
        _ => {
            if !track.patterns.is_empty() {
                refused.push("its track has a pattern no part plays".to_owned());
            }
            Ok(Music::Nothing)
        }
    };
    if music.is_err() {
        refused.push("its notes don't fit the step sequencer".to_owned());
    }
    place.refused = refused.into_iter().next();
    Ok(Read {
        place,
        instrument,
        effects,
        music,
        tempo,
        problems,
    })
}

fn display_name(device: &tutti::Device) -> String {
    if device.name.is_empty() {
        device.plugin.clone()
    } else {
        device.name.clone()
    }
}

/// The pattern's music: the player's own sequence while its notes are unchanged, else the notes.
fn music_of(
    doc: &Document,
    pattern: &tutti::Pattern,
    tempo: f64,
    problems: &mut Vec<String>,
    refused: &mut Vec<String>,
) -> Result<Music, String> {
    let mut notes: Vec<(i64, i64, u8, f64)> = Vec::new();
    let mut others = 0usize;
    for event in &pattern.events {
        match event {
            tutti::Event::Note(note) if note.audio.is_none() => {
                if note.expressions.is_some() {
                    problems.push("note expressions aren't played by the player".to_owned());
                    refused.push("its notes have expressions".to_owned());
                }
                notes.push((note.at, note.length, note.pitch, note.velocity));
            }
            _ => others += 1,
        }
    }
    if others > 0 {
        problems.push(format!(
            "{others} event(s) that aren't notes (audio, chords, parameter changes) aren't played \
             by the player"
        ));
        refused.push("its pattern has events that aren't notes".to_owned());
    }
    notes.sort_by_key(|&(at, length, pitch, _)| (at, pitch, length));

    let meter = doc.meter.first().map(|p| (p.numerator, p.denominator));
    if let Some(sequence) = pattern
        .extensions
        .get(NAMESPACE)
        .and_then(|ours| ours.get("sequence"))
    {
        match Sequence::parse(&sequence.to_string()) {
            Ok((sequence, parse_problems)) => {
                let (shaped, _, _) = sequence.unpack();
                let mut written = self::notes(&shaped);
                written.sort_by_key(|&(at, length, pitch)| (at, pitch, length));
                let unchanged = doc.ppq == PPQ
                    && pattern.length == shaped.len() as i64 * TICKS_PER_STEP
                    && meter
                        == smf::meter_for(shaped.steps_per_bar())
                            .map(|(n, d)| (u32::from(n), u32::from(d)))
                    && notes.len() == written.len()
                    && notes
                        .iter()
                        .zip(&written)
                        .all(|(read, wrote)| (read.0, read.1, read.2) == *wrote);
                if unchanged {
                    problems.extend(parse_problems);
                    return Ok(Music::Sequence(sequence));
                }
                problems.push(
                    "the notes were changed in another program since the player saved them: they \
                     are read as notes, and the parameter locks saved with them are left out"
                        .to_owned(),
                );
            }
            Err(reason) => problems.push(format!(
                "the player's own sequence in the project can't be read ({reason}): the notes are \
                 read instead"
            )),
        }
    }
    if notes.is_empty() {
        return Ok(Music::Nothing);
    }
    // Bounded before anything is sized from it.
    let longest = MAX_BARS * 4 * i64::from(doc.ppq);
    let end = notes
        .iter()
        .map(|&(at, length, _, _)| at.saturating_add(length))
        .max()
        .unwrap_or(0)
        .max(pattern.length);
    if end > longest {
        return Err(format!(
            "the pattern is longer than the {MAX_BARS} bars the player opens"
        ));
    }

    // Through the step grid, as a `.mid`'s notes go: offs before ons at one tick.
    let mut events = Vec::with_capacity(notes.len() * 2);
    for &(at, length, pitch, velocity) in &notes {
        let (Ok(on), Ok(off)) = (u32::try_from(at), u32::try_from(at + length)) else {
            return Err(format!(
                "the note {pitch} at tick {at} is outside what the sequencer holds"
            ));
        };
        let velocity = (velocity * 127.0).round().clamp(1.0, 127.0) as u8;
        events.push(smf::NoteEvent {
            tick: on,
            channel: smf::CHANNEL,
            key: pitch,
            velocity,
            on: true,
        });
        events.push(smf::NoteEvent {
            tick: off,
            channel: smf::CHANNEL,
            key: pitch,
            velocity: 0,
            on: false,
        });
    }
    events.sort_by_key(|event| (event.tick, event.on, event.key));
    let time_signature = match meter {
        Some((numerator, denominator)) => {
            match (u8::try_from(numerator), u8::try_from(denominator)) {
                (Ok(n), Ok(d)) => Some((n, d)),
                _ => return Err(format!("the meter {numerator}/{denominator} is too large")),
            }
        }
        None => None,
    };
    let ppq = doc.ppq;
    let mut loaded = smf::from_notes(events, ppq, time_signature, tempo, Vec::new())
        .map_err(|refused| format!("the notes don't fit the step sequencer: {refused}"))?;
    // As long as the pattern, rests at its end included: a sequence is whole bars.
    let bar = (loaded.pattern.steps_per_bar() as i64 * i64::from(ppq) / 4).max(1);
    let bars = usize::try_from((pattern.length + bar - 1) / bar).unwrap_or(1);
    if bars > loaded.pattern.bars() {
        loaded.pattern.set_bars(bars);
    }
    problems.extend(loaded.lost.iter().cloned());
    // **Read whole only if writing back gives the same notes**: the grid rounds a start near a step
    // onto it, the player's division shortens a length it can't hold, and two notes of one pitch in a
    // step become one. Velocities and releases are kept by the write, so they don't count.
    let mut read: Vec<(i64, i64, u8)> = notes.iter().map(|&(at, l, p, _)| (at, l, p)).collect();
    read.sort_by_key(|&(at, length, pitch)| (at, pitch, length));
    let mut again = self::notes(&loaded.pattern);
    again.sort_by_key(|&(at, length, pitch)| (at, pitch, length));
    if read != again {
        problems.push(
            "some notes were moved, shortened or merged onto the step grid; saving over the \
             project would write them so"
                .to_owned(),
        );
        refused.push("its notes aren't all on the step grid".to_owned());
    }
    Ok(Music::Notes(loaded))
}

/// The projects in `dir`: each folder holding one tutti document, by folder name, sorted.
pub fn list(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && tutti_fs::document_in(path).is_ok())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            Some((name, path))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// A folder for a new project called `stem` in `dir`: the name itself, or the first `stem-N` that
/// is free, never one that exists.
pub fn free_folder(dir: &Path, stem: &str) -> PathBuf {
    let first = dir.join(stem);
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem}-{n}")))
        .find(|candidate| !candidate.exists())
        .expect("a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(pattern: Pattern) -> Setup {
        Setup {
            title: "Line".to_owned(),
            instrument: Some(Plugin {
                id: "dk.mxm.mxm-mono-01".to_owned(),
                name: "mxm-mono-01".to_owned(),
                vendor: "MXM".to_owned(),
                state: Some(b"patch".to_vec()),
                enabled: true,
                device: None,
            }),
            effects: vec![Plugin {
                id: "vst3:0123456789ABCDEF0123456789ABCDEF".to_owned(),
                name: "Echo".to_owned(),
                vendor: String::new(),
                state: None,
                enabled: false,
                device: None,
            }],
            sequence: Sequence::new("Line", &pattern, 128.0),
            pattern,
            tempo: 128.0,
        }
    }

    fn a_line() -> Pattern {
        let mut pattern = Pattern::sized(2, 12);
        pattern.toggle(0, 48);
        pattern.toggle(0, 55);
        pattern.toggle(3, 51);
        pattern.toggle(13, 60);
        pattern.set_tied(14, true);
        pattern.set_tied(15, true);
        pattern
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mxm-player-project-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn saved(name: &str, setup: &Setup) -> PathBuf {
        let folder = scratch(name);
        let project = tutti_fs::create(&folder, "Line", &generator()).unwrap();
        let mut open = OpenProject::new(folder.clone(), project);
        write(&mut open, setup).unwrap();
        open.project.save(&generator()).unwrap();
        folder
    }

    #[test]
    fn plugins_take_the_formats_schemes_and_come_back() {
        assert_eq!(
            format_plugin("dk.mxm.mxm-mono-01"),
            "clap:dk.mxm.mxm-mono-01"
        );
        let vst3 = "vst3:0123456789ABCDEF0123456789ABCDEF";
        assert_eq!(format_plugin(vst3), vst3);
        assert_eq!(
            player_plugin("clap:dk.mxm.mxm-mono-01").unwrap(),
            "dk.mxm.mxm-mono-01"
        );
        assert_eq!(player_plugin(vst3).unwrap(), vst3);
        assert_eq!(player_plugin("au:aumu/Sinx/MXMa"), None);
    }

    #[test]
    fn ids_are_the_formats_and_never_repeat() {
        let a = new_id("trk");
        let b = new_id("trk");
        assert_ne!(a, b);
        assert!(Id::new(a.as_str()).is_ok());
        assert!(a.as_str().starts_with("trk_"));
    }

    #[test]
    fn the_notes_are_a_mids_at_the_formats_ticks() {
        // Step 0's chord, half a step each; step 13 tied through 15: three steps.
        let notes = notes(&a_line());
        assert!(notes.contains(&(0, TICKS_PER_STEP / 2, 48)));
        assert!(notes.contains(&(3 * TICKS_PER_STEP, TICKS_PER_STEP / 2, 51)));
        assert!(notes.contains(&(13 * TICKS_PER_STEP, 3 * TICKS_PER_STEP, 60)));
    }

    #[test]
    fn a_saved_setup_is_a_valid_project_that_reads_back_exactly() {
        let folder = saved("exact", &setup(a_line()));
        let opened = tutti_fs::open(&folder).unwrap();
        assert!(opened.report.is_valid(), "{:?}", opened.report);
        let doc = opened.project.document();
        assert_eq!(doc.meter[0].numerator, 3, "twelve sixteenths is 3/4");
        assert_eq!(doc.tempo[0].bpm, 128.0);
        assert_eq!(doc.dependencies.len(), 2);
        assert!(doc.extensions_used.iter().any(|used| used == NAMESPACE));
        let track = &doc.tracks[0];
        assert_eq!(track.devices[0].role, "instrument");
        assert_eq!(track.devices[1].role, "audio_effect");
        assert!(!track.devices[1].enabled, "an effect that is off");

        let read = read(&opened.project).unwrap();
        assert_eq!(read.place.refused, None, "{:?}", read.problems);
        assert!(read.problems.is_empty(), "{:?}", read.problems);
        assert_eq!(read.instrument.unwrap().state.unwrap(), b"patch");
        assert_eq!(read.effects.len(), 1);
        match read.music.unwrap() {
            Music::Sequence(sequence) => assert_eq!(sequence.unpack().0, a_line()),
            other => panic!("not the player's own sequence: {other:?}"),
        }
    }

    #[test]
    fn notes_another_program_changed_are_read_as_notes_without_the_locks() {
        let folder = saved("changed", &setup(a_line()));
        let mut opened = tutti_fs::open(&folder).unwrap().project;
        if let tutti::Event::Note(note) = &mut opened.document_mut().tracks[0].patterns[0].events[0]
        {
            note.pitch += 1;
        }
        let read = read(&opened).unwrap();
        assert!(
            read.problems
                .iter()
                .any(|p| p.contains("changed in another program")),
            "{:?}",
            read.problems
        );
        match read.music.unwrap() {
            Music::Notes(loaded) => {
                assert_eq!(loaded.pattern.steps_per_bar(), 12);
                assert_eq!(loaded.pattern.bars(), 2, "as long as the pattern");
                assert!(loaded.pattern.tied(15), "a tie past the first bar");
            }
            other => panic!("not read as notes: {other:?}"),
        }
        assert_eq!(
            read.place.refused, None,
            "notes on the grid write back whole"
        );
    }

    #[test]
    fn a_save_over_keeps_each_notes_own_data_and_another_programs_loop() {
        let folder = saved("kept", &setup(a_line()));
        let mut project = tutti_fs::open(&folder).unwrap().project;
        let doc = project.document_mut();
        doc.r#loop = Some(tutti::Loop {
            start: 960,
            end: 1920,
            enabled: false,
            unknown: Default::default(),
        });
        let pattern = &mut doc.tracks[0].patterns[0];
        pattern.extensions.clear();
        if let tutti::Event::Note(note) = &mut pattern.events[0] {
            note.velocity = 0.3;
            note.release = 0.9;
            note.extensions.insert(
                "org.example.daw".to_owned(),
                serde_json::json!({"accent": true}),
            );
        }
        let kept = pattern.events[0].clone();
        project.save(&generator()).unwrap();

        let opened = tutti_fs::open(&folder).unwrap().project;
        let read = read(&opened).unwrap();
        assert_eq!(read.place.refused, None, "{:?}", read.problems);
        let Ok(Music::Notes(loaded)) = read.music else {
            panic!("not read as notes");
        };
        let mut open = OpenProject::new(folder.clone(), opened);
        open.place = read.place.clone();
        let mut again = setup(loaded.pattern.clone());
        again.instrument.as_mut().unwrap().device = read.instrument.map(|d| d.id);
        write(&mut open, &again).unwrap();
        open.project.save(&generator()).unwrap();

        let doc = tutti_fs::open(&folder).unwrap().project.document().clone();
        assert!(
            doc.tracks[0].patterns[0].events.contains(&kept),
            "the note as it was"
        );
        assert_eq!(doc.r#loop.unwrap().start, 960, "their loop");
    }

    #[test]
    fn notes_the_grid_would_move_refuse_a_save_over() {
        let folder = saved("near-grid", &setup(a_line()));
        let mut project = tutti_fs::open(&folder).unwrap().project;
        let pattern = &mut project.document_mut().tracks[0].patterns[0];
        pattern.extensions.clear();
        if let tutti::Event::Note(note) = &mut pattern.events[0] {
            // Inside the grid's tolerance: read onto step 1, so written back moved.
            note.at += 5;
        }
        let read = read(&project).unwrap();
        assert!(read.music.is_ok());
        assert!(read.place.refused.is_some(), "{:?}", read.problems);
    }

    #[test]
    fn a_pattern_past_the_longest_the_player_opens_is_refused_before_anything_is_sized() {
        let folder = saved("long", &setup(a_line()));
        let mut project = tutti_fs::open(&folder).unwrap().project;
        let pattern = &mut project.document_mut().tracks[0].patterns[0];
        pattern.extensions.clear();
        pattern.length = 1_000_000_000_000;
        let read = read(&project).unwrap();
        assert!(read.music.is_err());
    }

    #[test]
    fn notes_off_the_grid_play_nothing_and_refuse_a_save_over() {
        let folder = saved("off-grid", &setup(a_line()));
        let mut opened = tutti_fs::open(&folder).unwrap().project;
        let pattern = &mut opened.document_mut().tracks[0].patterns[0];
        pattern.extensions.clear();
        if let tutti::Event::Note(note) = &mut pattern.events[0] {
            // A third of a step: far off the sixteenth grid.
            note.at += TICKS_PER_STEP / 3;
        }
        let read = read(&opened).unwrap();
        assert!(read.music.is_err());
        assert!(read.place.refused.is_some());
    }

    #[test]
    fn a_save_over_keeps_what_the_player_does_not_own() {
        let folder = saved("over", &setup(a_line()));
        let mut project = tutti_fs::open(&folder).unwrap().project;
        project
            .document_mut()
            .tracks
            .push(tutti::Track::new(new_id("trk"), "Theirs"));
        project.document_mut().extensions.insert(
            "org.example.daw".to_owned(),
            serde_json::json!({"kept": true}),
        );
        project.save(&generator()).unwrap();

        let opened = tutti_fs::open(&folder).unwrap().project;
        let read = read(&opened).unwrap();
        assert_eq!(read.place.refused, None, "{:?}", read.problems);
        let mut open = OpenProject::new(folder.clone(), opened);
        open.place = read.place.clone();
        let instrument = read.instrument.unwrap();
        let mut again = setup(Pattern::sized(1, 16));
        again.instrument.as_mut().unwrap().device = Some(instrument.id.clone());
        write(&mut open, &again).unwrap();
        open.project.save(&generator()).unwrap();

        let doc = tutti_fs::open(&folder).unwrap().project.document().clone();
        assert_eq!(doc.tracks.len(), 2);
        assert!(doc.tracks.iter().any(|track| track.name == "Theirs"));
        assert_eq!(
            doc.extensions["org.example.daw"],
            serde_json::json!({"kept": true})
        );
        let ours = doc
            .tracks
            .iter()
            .find(|track| track.name != "Theirs")
            .unwrap();
        assert_eq!(ours.devices[0].id, instrument.id, "the same device");
        assert!(
            ours.patterns[0].events.is_empty(),
            "the new, empty sequence"
        );
        assert_eq!(doc.meter[0].numerator, 4);
    }

    #[test]
    fn a_new_project_never_takes_a_folder_that_exists() {
        let dir = scratch("free");
        std::fs::create_dir_all(dir.join("Line")).unwrap();
        assert_eq!(free_folder(&dir, "Line"), dir.join("Line-2"));
        assert_eq!(free_folder(&dir, "Other"), dir.join("Other"));
    }
}
