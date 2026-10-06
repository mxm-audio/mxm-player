//! One controller, sixteen knobs, the same meaning on every instrument.
//!
//! Eight **fixed** knobs that never change meaning — cutoff, resonance, attack, decay, release,
//! glide, LFO rate, LFO depth — on the MMA Sound Controllers, so a GM2-aware controller works with
//! no configuration at all. Plus eight **paged** knobs that reach everything else, where a page
//! holds the same role in the same slot on every instrument whether or not that instrument has it.
//! mxm-mono-01 leaves the Osc 2 and Filter-envelope pages empty; a two-oscillator instrument fills
//! them, and nothing moves under the player's fingers.
//!
//! # This all runs on the GUI thread, deliberately
//!
//! Mapping needs three things that live here and nowhere else: the layout file, the current
//! parameter *values* (for pickup), and the page display. Putting it on the audio thread would
//! mean publishing a map to a realtime consumer and tracking parameter values a second time.
//!
//! So the audio worker learns only which CCs are claimed — a 128-bit mask, [`CcMask`] — and
//! forwards those to the GUI instead of to the plugin. Everything else happens here. A knob turn
//! is not a note; one frame of latency is not audible, and the simplicity is worth a great deal.
//!
//! # What the layout is, and is not
//!
//! [`schema`] holds the file format, and there are two of them because they have different owners:
//!
//! - The **collection standard** — roles, fixed knobs, the bank, the pages — is the same for every
//!   instrument and ships with the player.
//! - An **instrument map** says which of that instrument's parameters fill those roles, and ships
//!   **beside the `.clap` bundle**. Nobody is obliged to install the whole collection: someone who
//!   downloads only mxm-mono-01 must still get a working controller layout, and the player must not
//!   need to have heard of an instrument released after it.
//!
//! Both are **data**. Adding an instrument is adding a file, never a change here. That is the
//! requirement the design exists to serve, and `tests/t5_control_map.rs` asserts it directly.

pub mod curve;
pub mod schema;
pub mod takeover;

use crate::params::ParamSet;
use curve::Range;
use schema::{Curve, Instrument, InstrumentMap, Layout, LayoutError, SLOTS_PER_PAGE};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use takeover::Takeover;

/// How long a mapped control may be still before its gesture is closed.
///
/// A CC has no release, so a gesture has to be ended by something. Long enough that a slow,
/// deliberate knob turn is one gesture; short enough that the host is not left tracking an edit
/// nobody is making.
pub const GESTURE_IDLE_NANOS: u64 = 500_000_000;

/// Which CCs the player claims, so the audio worker can keep them from reaching the plugin.
///
/// A bitset rather than a map: it is 16 bytes, `Copy`, and answering "is this CC claimed" on the
/// audio thread must not touch the heap.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CcMask([u64; 2]);

impl CcMask {
    pub fn set(&mut self, cc: u8) {
        let cc = usize::from(cc & 0x7f);
        self.0[cc / 64] |= 1u64 << (cc % 64);
    }

    pub fn contains(&self, cc: u8) -> bool {
        let cc = usize::from(cc & 0x7f);
        self.0[cc / 64] & (1u64 << (cc % 64)) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0 == [0, 0]
    }

    pub fn count(&self) -> u32 {
        self.0[0].count_ones() + self.0[1].count_ones()
    }
}

/// What one claimed CC does.
#[derive(Clone, Debug, PartialEq)]
pub enum Binding {
    /// Drives whatever parameter fills `role` on the loaded instrument.
    Role {
        role: String,
        curve: Curve,
    },
    PageDown,
    PageUp,
}

/// The layout resolved against nothing in particular: CC → binding.
///
/// Independent of which plugin is loaded, because the *roles* are. Which parameter a role reaches
/// is answered later, per instrument, by [`ControlMap::param_for`].
#[derive(Clone, Debug, Default)]
struct Resolved {
    by_cc: Vec<(u8, Binding)>,
    mask: CcMask,
}

/// What happened to an incoming CC.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Not ours. It should reach the plugin untouched.
    Unclaimed,
    /// Ours, and it moved the bank to another page.
    PageChanged,
    /// Ours, and this edit should be sent.
    Edit(Edit),
    /// Ours, and deliberately nothing happened.
    Absorbed(Absorbed),
}

/// An edit to send to the plugin.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Edit {
    pub param_id: u32,
    pub value: f64,
    /// Whether a gesture has to be opened before this value.
    pub begin_gesture: bool,
}

/// Why a claimed CC did nothing. Each of these is a normal state, not an error.
#[derive(Clone, Debug, PartialEq)]
pub enum Absorbed {
    /// The slot is a deliberate gap in the page.
    EmptySlot,
    /// This instrument has no parameter in that role — Osc 2 on a one-oscillator synth.
    RoleNotOnThisInstrument { role: String },
    /// The layout names a parameter the loaded plugin does not have.
    ParameterMissing { role: String, param_id: u32 },
    /// The knob has not yet reached the parameter, so moving it must not jump the value.
    PickupPending { role: String },
    /// Nothing is loaded.
    NoPlugin,
}

/// Where the active layout came from, for the UI to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Compiled in. What a fresh install uses.
    Shipped,
    /// The shipped layout with a user overlay applied.
    User(PathBuf),
}

/// A gesture this module opened and is responsible for closing.
#[derive(Copy, Clone, Debug)]
struct OpenGesture {
    param_id: u32,
    last_touched_nanos: u64,
}

/// The whole thing: layout, resolution, page state, pickup state, open gestures.
pub struct ControlMap {
    layout: Layout,
    /// Gathered from the files found beside the installed bundles, keyed by `CLAP_ID`.
    instruments: BTreeMap<String, Instrument>,
    /// Where each instrument's map was found, so the UI can say why a knob does nothing.
    instrument_sources: BTreeMap<String, PathBuf>,
    source: Source,
    resolved: Resolved,
    user_path: Option<PathBuf>,
    active_page: usize,
    takeover: Takeover,
    open: Vec<OpenGesture>,
    /// Why the last load or reload did not do what was asked. Shown in the UI; never fatal.
    last_error: Option<String>,
}

impl Default for ControlMap {
    fn default() -> Self {
        Self::shipped()
    }
}

impl ControlMap {
    /// The compiled-in layout, with no user file.
    pub fn shipped() -> Self {
        let layout = schema::shipped();
        let resolved = resolve(&layout);
        Self {
            layout,
            instruments: BTreeMap::new(),
            instrument_sources: BTreeMap::new(),
            source: Source::Shipped,
            resolved,
            user_path: None,
            active_page: 0,
            takeover: Takeover::default(),
            open: Vec::new(),
            last_error: None,
        }
    }

    /// The shipped layout, with `path` applied over it if it exists and parses.
    ///
    /// A missing file is the ordinary case, not a failure. A malformed one leaves the shipped
    /// layout active and records why — refusing to start over a control map would be worse than
    /// starting with the defaults.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut map = Self::shipped();
        map.user_path = Some(path);
        map.reload();
        map
    }

    /// Re-reads the user file, keeping the working map if the new one is unusable.
    ///
    /// **Transactional on purpose.** Falling back to the defaults on a bad edit would rearrange
    /// somebody's controller in the middle of editing the file that controls it. The last layout
    /// that worked stays until a whole valid one replaces it.
    pub fn reload(&mut self) {
        let Some(path) = self.user_path.clone() else {
            return;
        };

        if !path.exists() {
            // Not an error: no user file means the shipped layout, which is a complete answer.
            self.adopt(schema::shipped(), Source::Shipped);
            self.last_error = None;
            return;
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                self.last_error = Some(format!("{} could not be read: {error}", path.display()));
                return;
            }
        };

        match apply_overlay(schema::shipped(), &text) {
            Ok(layout) => {
                self.adopt(layout, Source::User(path));
                self.last_error = None;
            }
            Err(error) => {
                // The previous layout stays exactly as it was.
                self.last_error = Some(format!("{} was not applied: {error}", path.display()));
            }
        }
    }

    fn adopt(&mut self, layout: Layout, source: Source) {
        self.resolved = resolve(&layout);
        self.active_page = self.active_page.min(layout.pages.len().saturating_sub(1));
        self.layout = layout;
        self.source = source;
        // The map underneath changed, so no knob can be assumed to be where its parameter is.
        self.takeover.rearm_all();
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Every CC the player claims. What the audio worker needs, and all it needs.
    pub fn claimed(&self) -> CcMask {
        self.resolved.mask
    }

    pub fn active_page(&self) -> usize {
        self.active_page
    }

    /// What the player shows for the current page.
    pub fn active_page_title(&self) -> String {
        self.layout
            .pages
            .get(self.active_page)
            .map(schema::Page::title)
            .unwrap_or_else(|| "—".to_owned())
    }

    pub fn page_count(&self) -> usize {
        self.layout.pages.len()
    }

    /// The roles on the current page, in slot order, for display.
    pub fn active_slots(&self) -> Vec<Option<String>> {
        self.layout
            .pages
            .get(self.active_page)
            .map(|page| page.slots.clone())
            .unwrap_or_else(|| vec![None; SLOTS_PER_PAGE])
    }

    /// Which CLAP parameter fills `role` on this instrument, if any.
    pub fn param_for(&self, clap_id: &str, role: &str) -> Option<u32> {
        self.instruments
            .get(clap_id)?
            .params
            .get(role)
            .map(schema::ParamRef::clap_id)
    }

    /// Whether a map was found for this instrument at all.
    ///
    /// The difference between "this synth has no oscillator 2" and "nobody installed a map for
    /// this synth" is one the player should be able to state, rather than presenting both as a
    /// silent knob.
    pub fn knows_instrument(&self, clap_id: &str) -> bool {
        self.instruments.contains_key(clap_id)
    }

    pub fn instrument_source(&self, clap_id: &str) -> Option<&Path> {
        self.instrument_sources.get(clap_id).map(PathBuf::as_path)
    }

    pub fn known_instruments(&self) -> impl Iterator<Item = &str> {
        self.instruments.keys().map(String::as_str)
    }

    /// Loads every instrument map beside the bundles in `dir`.
    ///
    /// Called for each directory the scan visits, so an instrument's map arrives with the
    /// instrument. Returns what went wrong, per file — one unreadable map must not stop the rest
    /// from loading, or installing a broken plugin would disable the controller for every other.
    pub fn load_instrument_maps_in(&mut self, dir: &Path) -> Vec<String> {
        let mut problems = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return problems;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !(name.ends_with(InstrumentMap::SUFFIX) || name == InstrumentMap::BARE) {
                continue;
            }
            if let Err(problem) = self.load_instrument_map(&path) {
                problems.push(problem);
            }
        }
        problems
    }

    /// Loads one instrument map file.
    pub fn load_instrument_map(&mut self, path: &Path) -> Result<(), String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("{} could not be read: {e}", path.display()))?;
        let map = InstrumentMap::parse(&text)
            .map_err(|e| format!("{} was not loaded: {e}", path.display()))?;

        for instrument in map.instruments {
            // Checked against the standard here, where the file that caused it can be named.
            self.layout
                .check_instrument(&instrument)
                .map_err(|e| format!("{} was not loaded: {e}", path.display()))?;
            self.instrument_sources
                .insert(instrument.clap_id.clone(), path.to_path_buf());
            self.instruments
                .insert(instrument.clap_id.clone(), instrument);
        }
        self.takeover.rearm_all();
        Ok(())
    }

    /// Forgets every instrument map, before a rescan repopulates them.
    pub fn clear_instrument_maps(&mut self) {
        self.instruments.clear();
        self.instrument_sources.clear();
    }

    /// Feeds one incoming control change through the map.
    ///
    /// `now_nanos` comes from the host clock and drives gesture idle timing; `params` is the
    /// current snapshot, which is what makes pickup possible.
    pub fn handle_cc(
        &mut self,
        cc: u8,
        value: u8,
        clap_id: Option<&str>,
        params: &ParamSet,
        now_nanos: u64,
    ) -> Outcome {
        let Some(binding) = self.binding_for(cc) else {
            return Outcome::Unclaimed;
        };

        match binding {
            // Buttons commonly send 127 on press and 0 on release, and some send only 127. Acting
            // on the press and ignoring the release covers both without double-stepping.
            Binding::PageDown => {
                if value > 0 {
                    self.change_page(-1);
                    Outcome::PageChanged
                } else {
                    Outcome::Absorbed(Absorbed::EmptySlot)
                }
            }
            Binding::PageUp => {
                if value > 0 {
                    self.change_page(1);
                    Outcome::PageChanged
                } else {
                    Outcome::Absorbed(Absorbed::EmptySlot)
                }
            }
            Binding::Role { role, curve } => {
                self.handle_role_cc(cc, value, &role, curve, clap_id, params, now_nanos)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_role_cc(
        &mut self,
        cc: u8,
        value: u8,
        role: &str,
        curve: Curve,
        clap_id: Option<&str>,
        params: &ParamSet,
        now_nanos: u64,
    ) -> Outcome {
        let Some(clap_id) = clap_id else {
            return Outcome::Absorbed(Absorbed::NoPlugin);
        };

        let Some(param_id) = self.param_for(clap_id, role) else {
            // The ordinary case for a role this instrument does not have. Inert, never an error,
            // and never quietly reassigned to a neighbouring parameter.
            return Outcome::Absorbed(Absorbed::RoleNotOnThisInstrument {
                role: role.to_owned(),
            });
        };

        let Some(snapshot) = params.get(param_id) else {
            return Outcome::Absorbed(Absorbed::ParameterMissing {
                role: role.to_owned(),
                param_id,
            });
        };

        let range = Range::new(snapshot.min, snapshot.max, snapshot.is_stepped);
        let knob = curve::position_of(value);
        let parameter = curve::to_position(curve, range, snapshot.value);

        if !self.takeover.accepts(cc, knob, parameter) {
            return Outcome::Absorbed(Absorbed::PickupPending {
                role: role.to_owned(),
            });
        }

        let begin_gesture = self.touch_gesture(param_id, now_nanos);
        Outcome::Edit(Edit {
            param_id,
            value: curve::to_value(curve, range, knob),
            begin_gesture,
        })
    }

    fn binding_for(&self, cc: u8) -> Option<Binding> {
        if let Some((_, binding)) = self.resolved.by_cc.iter().find(|(c, _)| *c == cc) {
            return Some(binding.clone());
        }
        // Bank slots resolve through the active page, so they cannot be in the flat table.
        let slot = self.layout.bank.slot_cc.iter().position(|c| *c == cc)?;
        let page = self.layout.pages.get(self.active_page)?;
        match page.slots.get(slot).and_then(Option::as_ref) {
            Some(role) => Some(Binding::Role {
                role: role.clone(),
                curve: self.layout.curve_for(role),
            }),
            // A claimed CC on an empty slot: still ours, still absorbed. Letting it fall through
            // to the plugin would make an empty slot behave differently from a filled one.
            None => Some(Binding::Role {
                role: String::new(),
                curve: Curve::default(),
            }),
        }
    }

    fn change_page(&mut self, delta: isize) {
        let count = self.layout.pages.len();
        if count == 0 {
            return;
        }
        let next = (self.active_page as isize + delta).rem_euclid(count as isize) as usize;
        if next != self.active_page {
            self.active_page = next;
            // Every bank knob now points at a different parameter, so none of them can be assumed
            // to be where their new parameter is.
            for cc in &self.layout.bank.slot_cc {
                self.takeover.rearm(*cc);
            }
        }
    }

    /// Marks a parameter as being edited now. Returns whether a gesture had to be opened.
    fn touch_gesture(&mut self, param_id: u32, now_nanos: u64) -> bool {
        if let Some(open) = self.open.iter_mut().find(|g| g.param_id == param_id) {
            open.last_touched_nanos = now_nanos;
            return false;
        }
        self.open.push(OpenGesture {
            param_id,
            last_touched_nanos: now_nanos,
        });
        true
    }

    /// Gestures that have gone quiet and must now be closed.
    ///
    /// Called every frame. A CC has no release, so this is what ends an edit.
    pub fn expired_gestures(&mut self, now_nanos: u64) -> Vec<u32> {
        let idle =
            |g: &OpenGesture| now_nanos.saturating_sub(g.last_touched_nanos) >= GESTURE_IDLE_NANOS;
        let expired: Vec<u32> = self
            .open
            .iter()
            .filter(|g| idle(g))
            .map(|g| g.param_id)
            .collect();
        self.open.retain(|g| !idle(g));
        expired
    }

    /// Closes every open gesture, whatever its age.
    ///
    /// The other half of the contract: plugin unload, engine stop, config reload, MIDI
    /// disconnect and shutdown all leave a gesture open otherwise, and an unclosed gesture is
    /// something this codebase already treats as a serious failure — see
    /// `Payload::must_not_be_lost`.
    pub fn close_all_gestures(&mut self) -> Vec<u32> {
        self.takeover.rearm_all();
        self.open.drain(..).map(|g| g.param_id).collect()
    }

    pub fn has_open_gestures(&self) -> bool {
        !self.open.is_empty()
    }

    /// Tells the map a parameter moved for a reason that was not this controller.
    ///
    /// The GUI panel, a state load, plugin-driven output, a preset. Every knob bound to it is
    /// re-armed, so the next turn picks up from the new value instead of snapping back to where
    /// the knob happens to be sitting.
    pub fn parameter_moved_elsewhere(&mut self, param_id: u32, clap_id: Option<&str>) {
        let Some(clap_id) = clap_id else {
            self.takeover.rearm_all();
            return;
        };
        for cc in self.ccs_reaching(clap_id, param_id) {
            self.takeover.rearm(cc);
        }
    }

    /// Every claimed CC that currently reaches `param_id`.
    fn ccs_reaching(&self, clap_id: &str, param_id: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for (cc, binding) in &self.resolved.by_cc {
            if let Binding::Role { role, .. } = binding
                && self.param_for(clap_id, role) == Some(param_id)
            {
                out.push(*cc);
            }
        }
        if let Some(page) = self.layout.pages.get(self.active_page) {
            for (slot, role) in page.slots.iter().enumerate() {
                if let Some(role) = role
                    && self.param_for(clap_id, role) == Some(param_id)
                    && let Some(cc) = self.layout.bank.slot_cc.get(slot)
                {
                    out.push(*cc);
                }
            }
        }
        out
    }

    /// Everything moved, so nothing can be assumed. Used on plugin load and state restore.
    pub fn rearm_all(&mut self) {
        self.takeover.rearm_all();
    }
}

/// Flattens a layout into the CC table the runtime uses.
///
/// Bank slots are deliberately absent: they depend on the active page, so they are resolved at
/// lookup time instead. They are still in the mask, because the worker must keep them from the
/// plugin whichever page is showing.
fn resolve(layout: &Layout) -> Resolved {
    let mut by_cc = Vec::new();
    let mut mask = CcMask::default();

    for knob in &layout.fixed {
        if schema::is_reserved(knob.cc) {
            continue;
        }
        by_cc.push((
            knob.cc,
            Binding::Role {
                role: knob.role.clone(),
                curve: layout.curve_for(&knob.role),
            },
        ));
        mask.set(knob.cc);
    }

    by_cc.push((layout.bank.page_down_cc, Binding::PageDown));
    mask.set(layout.bank.page_down_cc);
    by_cc.push((layout.bank.page_up_cc, Binding::PageUp));
    mask.set(layout.bank.page_up_cc);

    for cc in &layout.bank.slot_cc {
        mask.set(*cc);
    }

    Resolved { by_cc, mask }
}

/// A user file: any section it names replaces the shipped one, and the rest is left alone.
///
/// Overlaying rather than replacing wholesale is what makes "move page-up to CC 115" a two-line
/// file instead of a copy of the whole standard, which would then silently miss every later change
/// to it. Instruments are deliberately not here: they ship beside their bundles.
#[derive(Clone, Debug, Default, serde::Deserialize)]
struct Overlay {
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    roles: Option<std::collections::BTreeMap<String, schema::RoleSpec>>,
    #[serde(default)]
    fixed: Option<Vec<schema::FixedKnob>>,
    #[serde(default)]
    bank: Option<schema::Bank>,
    #[serde(default)]
    pages: Option<Vec<schema::Page>>,
}

fn apply_overlay(mut base: Layout, text: &str) -> Result<Layout, LayoutError> {
    let overlay: Overlay =
        serde_json::from_str(text).map_err(|e| LayoutError::Parse(e.to_string()))?;

    if let Some(version) = overlay.schema_version
        && version != schema::SCHEMA_VERSION
    {
        return Err(LayoutError::UnsupportedVersion {
            found: version,
            supported: schema::SCHEMA_VERSION,
        });
    }

    if let Some(roles) = overlay.roles {
        base.roles.extend(roles);
    }
    if let Some(fixed) = overlay.fixed {
        base.fixed = fixed;
    }
    if let Some(bank) = overlay.bank {
        base.bank = bank;
    }
    if let Some(pages) = overlay.pages {
        base.pages = pages;
    }

    // Validated as a whole: an overlay that is fine in isolation can still collide with the base.
    let text = serde_json::to_string(&base).map_err(|e| LayoutError::Parse(e.to_string()))?;
    Layout::parse(&text)
}

/// Where the user's control map lives.
pub fn default_user_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mxm-player")
        .join("control-map.json")
}

/// Writes a starting point for someone who wants to edit the map.
///
/// The shipped layout verbatim, so the first edit is a change to something that already works
/// rather than a guess at the schema.
pub fn write_starter(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, schema::SHIPPED).map_err(|e| e.to_string())
}
