//! The player's window.
//!
//! Layout is organised by task and signal flow: what to load, what it is doing, what its
//! parameters are, and — fixed along the bottom, always visible — how to play it.
//!
//! Styling comes from `mxm-ui`, the collection's shared foundation, reached through [`adapter`].

pub mod adapter;
mod fx_rail;
pub mod keyboard;

use crate::config::PlayerConfig;
use crate::control_map::{CcMask, ControlMap};
use crate::discovery::{Found, ScanCache, Sentinel, find_bundles_in, scan_bundle, search_paths};
use crate::engine::audio::Backend;
use crate::engine::{AudioConfig, Engine, EngineState, PluginOutput};
use crate::events::input::Payload;
use crate::notifier::Notifier;
use crate::params::{ParamSet, ParamSnapshot};
use crate::sequencer::{self, Pattern, SequencerState, Transport};
use crate::settings::Settings;
use crate::state::{
    AudioState, FoundState, KeyboardState, MidiState, ParamState, PlayerState, PluginState,
    RefusedInputState, SequencerView, TimingState,
};
use egui::{Color32, Key};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// The reference size the design system asks every MXM interface to be laid out for.
pub const REFERENCE_SIZE: [f32; 2] = [1200.0, 760.0];

/// How often `logic()` runs while an editor is open — and it is a **measured** floor, not a taste.
///
/// A plugin editor is a window on this same thread. If the player asks for frames at or below the
/// display's frame interval the event loop never idles, the editor's window messages are never
/// dispatched, and **it renders white**. Measured on this machine (60 Hz, Vulkan, no OpenGL in the
/// player at all): white at 16 and 20 ms, correct at 25 ms and above, identical at 250 ms.
///
/// 50 ms is 2.5× that knee. The margin is deliberate because the knee tracks the *display's* refresh
/// rate, so a different monitor moves it — this must not be tuned down to whatever happens to work
/// on one desk. Servicing 20×/second keeps engine polling, plugin output and MIDI-to-step entry
/// imperceptibly quick; the sequencer's own clock is on the audio thread and is unaffected.
const EDITOR_SERVICE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// The same, with no editor open — nothing shares the thread, so the playhead can be smooth.
const PLAYHEAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
/// How often a player with nothing playing and no editor shown still services the engine.
///
/// Not zero, and not "only when something happens": `service()` is where a MIDI press reaches the
/// selected step, and a press arrives on the MIDI backend's thread with nothing to wake the GUI on
/// its own account. Ten turns a second is imperceptible for entering a note and costs nothing —
/// these frames run `logic` and draw an unchanged window.
const IDLE_SERVICE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// How often a knob being dragged into a step may write the settings file.
///
/// **Every other thing that persists is discrete** — a note toggled, a preset loaded — so writing
/// eagerly costs one file write per act. A lock is written from a *drag*, which produces a new value
/// every frame, so the same eagerness would write the settings file sixty times a second for as long
/// as the knob is moving. This bounds that to twice a second, and `on_exit` flushes whatever the
/// last interval did not, so nothing is lost by waiting.
const LOCK_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// The environment variable that overrides the theme the player opens in, for one run.
///
/// `MXM_EDITOR_THEME`'s opposite number for the host: `MXM_PLAYER_THEME=dark|light|system`, read
/// once at startup. The **Theme** control in the app bar is where a person chooses, and that choice
/// is saved; this is for a script that must have a known theme without touching what the person
/// left behind — a screenshot pair, most of all. It therefore overrides the saved choice for that
/// run and is never written back.
///
/// Unset means the saved choice, and no saved choice means the desktop.
pub const THEME_ENV: &str = "MXM_PLAYER_THEME";

/// How a theme choice is written in the settings file. [`mxm_ui::theme::from_name`] reads it back,
/// so the host and the editors spell the three names the same way.
fn theme_name(preference: egui::ThemePreference) -> &'static str {
    match preference {
        egui::ThemePreference::Dark => "dark",
        egui::ThemePreference::Light => "light",
        egui::ThemePreference::System => "system",
    }
}

/// Overrides [`EDITOR_SERVICE_INTERVAL`] via `MXM_SERVICE_MS`.
///
/// Kept because the failure it guards against is **machine-dependent and silent**: a white editor on
/// somebody else's display is diagnosed in one run by raising this, which is otherwise a rebuild.
fn editor_service_interval() -> std::time::Duration {
    std::env::var("MXM_SERVICE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(EDITOR_SERVICE_INTERVAL)
}

/// Fixed size of the Play/Pause toggle.
///
/// Fixed because the two labels are different widths, so an unsized button resizes when pressed and
/// shifts everything to its right — the same defect as the step row, in a smaller costume.
const TRANSPORT_BUTTON_SIZE: egui::Vec2 = egui::vec2(92.0, 22.0);

/// Fixed size of the tempo field.
///
/// Wide enough for "300.0 BPM" plus the caret it grows when clicked. Same reason as
/// [`TRANSPORT_BUTTON_SIZE`]: a control that changes width when you touch it moves everything after
/// it.
const TEMPO_FIELD_SIZE: egui::Vec2 = egui::vec2(88.0, 22.0);

/// Height of the sequencer panel.
///
/// Fixed so that loading a sequence cannot move the interface: the panel used to grow a row for
/// every thing a load could not keep. The bar strip's two rows are part of the fixed height, and
/// the delete confirmation lives *inline* on the first of them — a transient row of its own was
/// reserved space that read as a gap between the steps and the keyboard.
const SEQUENCER_HEIGHT: f32 = 148.0;

/// How much height one parameter control needs, including its label and spacing.
///
/// Used to work out how many fit in a column. An estimate, deliberately: the exact height depends
/// on the widget and the theme, and being a little conservative costs one row rather than clipping.
const PARAM_ROW_HEIGHT: f32 = 46.0;

/// Below this, a column is too narrow to set a value precisely.
///
/// A slider you cannot aim is worse than a scrollbar, so this is the point where wrapping stops and
/// scrolling takes over.
const MIN_PARAM_COLUMN_WIDTH: f32 = 210.0;

/// Fixed size of a sequencer step button.
///
/// Fixed on purpose: labelling a step with its notes made the row reflow on every edit, which is
/// the defect this replaces. Wide enough for two digits, tall enough for the number and its
/// has-notes marker.
const STEP_BUTTON_SIZE: egui::Vec2 = egui::vec2(
    mxm_ui::space::MIN_TARGET + mxm_ui::space::SPACE_1,
    mxm_ui::space::MIN_TARGET + mxm_ui::space::SPACE_1,
);

/// What a second click on the selected step does.
///
/// Named rather than inlined because the panel's hint line has to describe it before it happens,
/// and the two have to agree. See [`PlayerApp::tie_action`].
///
/// **Every variant acts on the clicked step itself.** The gesture ties backwards, always, so there
/// is no variant that reaches forwards to a neighbour and none that needs a second index to say
/// which step it touched.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TieAction {
    /// Stop this step continuing the previous note.
    Untie(usize),
    /// Continue the previous note through this step — a **hold**, because the step is empty.
    Tie(usize),
    /// Tie this step, which holds its own note — making it a **slide**.
    ///
    /// The same flag `Tie` sets; a distinct variant so the hint line can say what will actually
    /// happen, which for a slide is not "hold the previous note".
    Slide(usize),
}

/// How wide a step's marker is, as a fraction of the button.
///
/// A drum machine's step button carries a small inset bar; this is that idea and not its
/// appearance. §2 forbids simulated depth, embossing and photorealistic switches, so the marker is
/// a flat filled rect — the *shape* carries the meaning and nothing pretends to be plastic.
///
/// An untied note sounds for **half** its step, because `sequencer::clock::GATE` is 0.5, so its bar
/// fills the left half and the empty right half is the gap before the next step. That is not
/// decoration: it is the reason a run of separate notes sounds detached and a tied one does not.
///
/// **It is left-aligned so that ties can join it.** A tie *reaches backwards* — it continues the
/// note before it — so a tied step's bar will span its whole width and meet the previous step's at
/// the boundary, and a run will read as one unbroken bar whose length is the note's length. Drawing
/// it from the right edge would show a step joining the one *after* it, which is the opposite
/// relationship.
///
/// Duration moved to the merged button when ties landed — see `apps/mxm-player/NOTES.md`'s
/// *A step is a rest, a note, a hold or a slide* — so the bar's one job now is saying the step
/// holds notes, centred over the number, half a cell wide.
const STEP_MARKER_WIDTH: f32 = 0.5;

/// Height of the on-screen keyboard, which is fixed along the bottom of the window.
const KEYBOARD_HEIGHT: f32 = 140.0;

/// Vertical room the **left settings panel** needs to be worth showing.
///
/// Unlike `SEQUENCER_HEIGHT` and `KEYBOARD_HEIGHT` this is not an exact panel size — the panel
/// scrolls, so it does not need its whole content. It is the height below which showing it is
/// indistinguishable from not showing it: enough for the audio device section and the start of the
/// next, so the panel reads as a panel rather than a clipped strip.
/// The one file the state dump writes.
///
/// Its name is unchanged: renaming the buttons is a labelling fix, and moving the file would make
/// somebody's existing dump disappear to no purpose.
const STATE_DUMP: &str = "preset.clapstate";

const SETTINGS_MIN_HEIGHT: f32 = 320.0;

/// A floor for the collapsed height, used before the status bar has been measured once.
///
/// What `status_height` holds before the status bar has been drawn once.
///
/// The bar is content-sized, so its height is **measured** — see [`PlayerApp::collapsed_height`].
/// An earlier version guessed 28 px, and collapsing then shrank the window until the transport was
/// clipped out of existence; `collapsing_never_takes_the_transport_or_the_keyboard_with_it` caught
/// it, and the guess was replaced by a measurement with this as a generous fallback.
///
/// **It is a first-frame fallback, not a floor.** Applying it as `measured.max(FLOOR)` inflated
/// every collapsed window by 17 px, because the bar actually measures 23 — which is half of the
/// dead space that produced this comment.
const STATUS_HEIGHT_FLOOR: f32 = 40.0;

/// The window's minimum while expanded, matching what `main.rs` requests at startup.
///
/// Collapsing lowers the minimum to the collapsed content height, because otherwise the window cannot
/// shrink past the expanded floor and collapsing yields empty space instead of room — which would
/// make the whole feature do nothing.
pub const MIN_SIZE: (f32, f32) = (REFERENCE_SIZE[0] * 0.75, REFERENCE_SIZE[1] * 0.75);

/// Keyboard colours.
///
/// `crates/ui` (mxm-kit's `mxm-ui`) has landed, so the "placeholder until then" these once were is
/// over — but they are deliberately **not** all tokens. A piano keyboard's white and black are the
/// instrument, not the theme: they stay the same in dark and light, because a keyboard that
/// inverted would stop being a keyboard.
///
/// The state colours below are a different case and **should** move to tokens — `HELD_KEY` is
/// `mod-performance`, `SELECTED_KEY` is close to `mod-lfo`, and `KEY_LABEL` and `KEY_BORDER` are
/// `text-secondary` and `border`. That is a real change with a real risk: the keyboard's
/// regression tests assert on what is drawn, so it is worth doing on its own rather than as a
/// footnote to something else.
const WHITE_KEY: Color32 = Color32::from_gray(250);
const BLACK_KEY: Color32 = Color32::from_gray(40);
const HELD_KEY: Color32 = Color32::from_rgb(0x4c, 0xaf, 0x50);
/// A note written into the step being edited.
///
/// Colour alone is not the signal — a selected key also carries a filled marker, because the design
/// system forbids hue-only encoding. The two states can overlap, so a key can be both held and
/// selected, and that must read as a third thing rather than as either one.
pub const SELECTED_KEY: Color32 = Color32::from_rgb(0x3f, 0x7d, 0xd6);
/// Sounding *and* written into the step being edited.
const HELD_AND_SELECTED_KEY: Color32 = Color32::from_rgb(0x2e, 0x9c, 0x9c);
/// The marker drawn on a key that belongs to the selected step, so the state does not depend on
/// hue. Sized to stay legible on a black key.
const SELECTED_MARK: Color32 = Color32::from_gray(250);
const KEY_LABEL: Color32 = Color32::from_gray(110);
/// White keys are nearly the colour of the panel behind them, so without an outline the keyboard
/// reads as one pale block. The border is what makes it a keyboard.
const KEY_BORDER: Color32 = Color32::from_gray(150);

pub struct PlayerApp {
    engine: Engine,
    backend: Box<dyn Backend>,
    /// `None` when running without a window: there is no egui context to wake.
    notifier: Option<Notifier>,
    /// Commands from the local CLI socket, answered in [`Self::service`]. See [`crate::cli`].
    cli: Option<std::sync::mpsc::Receiver<crate::cli::Request>>,

    settings: Settings,
    settings_path: PathBuf,
    /// The theme on show, which is not always the one in `settings`: [`THEME_ENV`] overrides the
    /// saved choice for its run, and that override must not be written back. Kept here so the
    /// frame that applies it reads one field rather than re-deciding that precedence every time.
    theme: egui::ThemePreference,

    found: Vec<Found>,
    cache: ScanCache,
    sentinel: Sentinel,
    /// Where discovery looks. `None` means the standard locations.
    search_paths: Option<Vec<PathBuf>>,
    /// Where the MIDI port lists come from.
    midi_ports: crate::config::MidiPorts,
    /// The bundle a previous run died while scanning. Quarantine is offered, never applied.
    suspected: Option<PathBuf>,

    params: ParamSet,
    /// Values being edited this frame, so a drag reads smoothly rather than fighting the plugin.
    editing: HashMap<u32, f64>,
    /// Parameters with an open host gesture.
    open_gestures: Vec<u32>,
    /// How many values each open plugin-editor gesture has delivered into the pending step edit.
    ///
    /// One value is a **click** — a toggle, a text entry — and a click landing back on the patch
    /// is the reset gesture (the lock clears; off, and no dot). Many values are a **drag**, and a
    /// drag ending on the patch still pins, which keeps the absolute-lock ruling intact. The two
    /// cannot be told apart at commit time without this count.
    open_gesture_edits: std::collections::HashMap<u32, usize>,
    /// Parameters whose base was **parked when the editor's current gesture on them opened** —
    /// recorded then, because the held branch parks every editor gesture at rest for the drag's
    /// duration and would erase the distinction. A parked base is the state in which the editor's
    /// own reset aims at the factory default rather than the patch, so the commit reads a single
    /// value landing on the default as the reset it is. See `restore_base_for_step_edit`.
    gesture_began_parked: std::collections::HashSet<u32>,
    /// Set when a gesture ends, so the next frame requeries the authoritative value.
    ///
    /// A parameter change reaches the plugin through the audio thread, so requerying in the same
    /// frame would read the value back *before* it was applied — which is exactly the stale read
    /// that made controls snap back to where they started.
    requery_params: bool,
    text_entry: HashMap<u32, Option<String>>,

    octave: i32,
    sustain: bool,
    pitch_bend: f32,
    mod_wheel: f32,
    /// Notes currently held, so the keyboard doubles as a monitor. Includes MIDI arrivals.
    held: Vec<u8>,
    /// Notes held on a connected MIDI keyboard, refreshed from the host state each turn.
    ///
    /// Separate from `held` because the two are cleaned up by different owners: `held` is
    /// released by the GUI that pressed it, this one by the MIDI callback or a port closing.
    midi_held: Vec<u8>,
    /// The note a mouse drag is currently sounding.
    dragging: Option<u8>,
    /// Whether the window had focus last frame, so focus *loss* can be detected.
    had_focus: bool,

    audio_devices: Vec<String>,
    midi_inputs: Vec<String>,
    midi_outputs: Vec<String>,
    status: Option<String>,
    log_lines: Vec<String>,
    /// The plugin's reported latency, refreshed when it says it changed.
    latency: Option<u32>,
    /// The screen size the previous frame saw, so an in-progress resize can be recognised.
    last_screen_size: Option<egui::Vec2>,
    /// The window size last persisted, so geometry is only written when it actually moves.
    persisted_size: Option<(f32, f32)>,
    /// The collapse state the window geometry was last matched to.
    ///
    /// `None` until the first frame, so startup does not send a resize the user did not ask for.
    /// Only a change here moves the window, which is what keeps this out of the "the interface
    /// moved on its own" class of defect.
    collapse_applied: Option<(bool, bool)>,
    /// The status bar's measured height, from the last frame that drew it.
    status_height: f32,
    /// Whether the automatic scan has run. See [`PlayerApp::logic`].
    startup_scan_done: bool,
    /// Set when a plugin is loaded, so its editor opens on the next frame. See
    /// [`PlayerApp::show_editor_now`].
    open_editor_pending: bool,
    /// The player's own window, for making it the owner of a plugin's editor window. `None` on a
    /// headless run.
    host_window: Option<crate::ownership::HostWindow>,
    /// The last refusal from trying to open an editor, shown until it is dismissed or superseded.
    editor_refusal: Option<crate::engine::editor::EditorRefusal>,

    /// One controller across the whole collection. See [`crate::control_map`].
    control_map: ControlMap,
    /// The claimed-CC mask last sent to the audio worker, so it is only sent when it changes.
    published_mask: Option<CcMask>,
    /// What the last control-map load or rescan could not do. Shown, never fatal.
    control_map_problems: Vec<String>,
    /// How many columns the parameter panel laid out last frame.
    ///
    /// Observable so a test can assert the panel *actually wrapped*. Reachability alone would pass
    /// against a single tall scroller and prove nothing about the defect this fixes.
    param_columns: usize,

    // --- the sequencer -------------------------------------------------------------------------
    /// The authoritative copy. The worker's is a published snapshot of this.
    sequencer: SequencerState,
    /// Bumped by every edit; the worker acknowledges it.
    sequencer_serial: u64,
    /// The serial last accepted into the command queue.
    ///
    /// Bounds the GUI to **one unacknowledged publication**: a tempo drag at frame rate would
    /// otherwise fill the 64-slot command queue with stale states and delay the `Stop` that
    /// `Engine::stop_now` polls for. Edits made while one is outstanding coalesce into the next.
    sequencer_published: u64,
    /// The step being edited, if any. While one is selected the keyboard writes into it.
    /// Which parameter group the panel is showing. Transient view state — never persisted, and
    /// never a filter on what is writable: a host lane, a preset and a step lock reach every
    /// parameter whatever tab is on screen.
    parameter_tab: String,
    selected_step: Option<usize>,
    /// Steps selected **besides** [`Self::selected_step`], for editing several at once.
    ///
    /// The anchor keeps every meaning it has always had — it is what previews, what parks, what
    /// the panel reads — and the companions follow silently: a note toggled, or a lock written,
    /// lands on all of them. Shift-click extends a range from the anchor; Ctrl-click toggles
    /// individual steps, so every fourth step can be selected and locked in one gesture.
    also_selected: std::collections::BTreeSet<usize>,
    /// Which bar the step row shows. Interface state: the chips are the only way between bars,
    /// and the row always shows exactly one.
    selected_bar: usize,
    /// `Loop [ Bar | Pattern | All ]`. **Transport state, not music** — no file keeps it, a
    /// render ignores it, and it does not survive a restart, exactly like play/stop.
    loop_scope: LoopScope,
    /// One copied bar: its cells' notes, ties and locks — the same three things a resize moves —
    /// plus the plugin the locks were recorded under and the bar shape it was copied at.
    bar_clipboard: Option<BarClipboard>,
    /// Whether Copy and Clear act on the shown bar or on its whole eight-bar pattern.
    tools_on_pattern: bool,
    /// Locks read from the settings file that no loaded plugin has claimed yet.
    ///
    /// **Parked rather than applied or discarded.** A parameter id means nothing without the plugin
    /// it belongs to, and at startup there is no plugin — the bundle loads afterwards. Applying them
    /// blind would move whichever parameters happened to share a number; discarding them would throw
    /// away somebody's work because of the order two things happen in. So they wait here, are
    /// written back out unchanged by `persist`, and are installed by `claim_parked_locks` the moment
    /// the plugin they were recorded for is the one that is loaded.
    parked_locks: Option<crate::sequencer::locks::LockData>,
    /// The selected step's locked values, formatted by the plugin.
    ///
    /// Cached because formatting goes **through the plugin** — `value_to_text` is a call into
    /// somebody else's code, and doing it for every visible parameter on every frame would put a
    /// plugin call in the paint loop. Rebuilt only when the selection or the sequence changes,
    /// which is the only time the answer can differ.
    step_text: HashMap<crate::sequencer::locks::LockKey, String>,
    /// What `step_text` was built for: the selected step and the sequence's serial. Comparing the
    /// pair is what makes the cache correct without a flag on every path that can change a lock —
    /// every one of them bumps the serial already.
    step_text_for: Option<(usize, u64)>,
    /// What the sequencer is currently adding to each parameter, as last reported by the worker.
    ///
    /// The player's half of the same base-and-offset separation the plugin keeps: `params` holds the
    /// values, this holds what the steps are laying over them. It is display state — the
    /// notification that fills it is droppable — and nothing is decided from it.
    modulation: HashMap<crate::sequencer::locks::LockKey, f64>,
    /// A step edit from the plugin's own editor, waiting for the drag to finish.
    ///
    /// **Why it waits.** During the drag the plugin has already moved its own parameter, so the base
    /// carries the value you are hearing. Writing the lock straight away would make the runtime lay
    /// `lock - patch` *on top of that*, and the instrument would sound at roughly twice the deviation
    /// until you let go. Deferring the write leaves the offset silenced, so a drag sounds exactly
    /// like the value under the knob — and the lock takes that value when the gesture closes.
    ///
    /// **Keyed by `(step, param)`**, so a value recorded against one step can never be committed
    /// into another: selection can change mid-drag, and a plain per-parameter entry would follow it.
    /// Cleared wherever the world it described stops existing — deselect, Clear, any load, a plugin
    /// change, and tracking invalidation.
    pending_step_edit: HashMap<(usize, u32), f64>,
    /// Parameters whose base is **parked at the selected step's value**.
    ///
    /// A knob edited in the plugin's own editor keeps that value for as long as the step stays
    /// selected — the knob sits at the locked position, with no arc, because there is no modulation:
    /// the value *is* the base. Published to the runtime so those parameters' previews stay zero.
    /// Leaving the step restores each base to the patch and empties this.
    parked_bases: std::collections::HashSet<u32>,
    /// A lock changed and the settings file does not know yet. See [`LOCK_SAVE_INTERVAL`].
    locks_unsaved: bool,
    last_lock_save: std::time::Instant,
    /// **The sequence-patch**: what every parameter is when no step overrides it.
    ///
    /// It describes the **instrument**, not the sequence — so Clear, a sequence load and a MIDI load
    /// leave it alone. Emptying it there threw away the one thing that could stand in for a lock
    /// arriving without a baseline. Only a plugin change clears it, because parameter numbers mean
    /// nothing beside a different instrument.
    ///
    /// **It is simply what the instrument is set to while no step is selected.** Select a step and
    /// you are editing that step; select nothing and you are editing the patch the whole sequence
    /// deviates from. It can be changed at any time, and choosing a preset changes it like anything
    /// else — no moment of capture to get right, and nothing to detect.
    ///
    /// [`PlayerApp::follow_sequence_patch`] is what keeps it true, on every frame with no step
    /// selected. A version that snapshotted at one chosen moment instead cannot survive a patch
    /// change it did not anticipate: the baselines would still describe the patch you had moved on
    /// from, and every unlocked step would restore values from a preset you were no longer using.
    sequence_patch: HashMap<crate::sequencer::locks::LockKey, f64>,
    /// Locked parameters a load could not place, because the effect they name is not in the chain.
    ///
    /// **Kept, never published.** A sequence is work: a pattern saved with an effect automated and
    /// loaded before that effect is added must not lose it silently. They are written back out on
    /// the next save and resolve when a matching effect appears.
    pending_fx_locks: Vec<crate::sequencer::locks::LockedParam>,
    /// Advanced on every press of Random, so pressing it twice gives two different patterns.
    random_seed: u64,
    /// Text for the system clipboard, waiting for a frame that holds an `egui::Context`.
    ///
    /// The copy funnels are reached from both a button and a key, and neither has a context to
    /// hand — so the text is parked here and flushed once per frame. See
    /// [`PlayerApp::offer_to_system_clipboard`] for why a copy must reach the system clipboard.
    pending_clipboard_text: Option<String>,

    /// Where sequences are saved, and what the next one will be called.
    sequence_dir: PathBuf,
    /// Where rendered audio and `.mid` files go. Through `PlayerConfig`, like every other path the
    /// player touches, so a test can never write into real user storage.
    exports_dir: PathBuf,
    sequence_name: String,
    sequence_problems: Vec<String>,
    /// Whether a rendered export is peak-normalised. On by default: right for making samples, and
    /// one click away from off for comparing patch levels.
    normalise_export: bool,
}

impl PlayerApp {
    /// Starts the CLI listener, publishing the port beside the settings file.
    ///
    /// Public so a test can attach one to a sandboxed player; the windowed run does it in `new`.
    pub fn start_cli(&mut self) {
        match crate::cli::start(&self.settings_path) {
            Ok(rx) => self.cli = Some(rx),
            Err(reason) => self.status = Some(reason),
        }
    }

    /// Answers everything the CLI has queued. Once per service pass, on the GUI thread, because
    /// every method a command touches belongs to it.
    fn answer_cli(&mut self) {
        loop {
            let request = match self.cli.as_ref().map(|rx| rx.try_recv()) {
                Some(Ok(request)) => request,
                _ => return,
            };
            let answer = self.run_cli_command(&request.line);
            let _ = request.reply.send(answer);
        }
    }

    /// One command in, one JSON answer out.
    ///
    /// **The vocabulary is the contract: everything a person can do gets a verb.** That is what
    /// makes every piece of player logic reachable by a machine — a bug report can be replayed
    /// against the live player, and a composition can be authored by something that is not holding
    /// the mouse. A feature that lands without its verb has broken this contract, and the DOX says
    /// so where features are added.
    pub fn run_cli_command(&mut self, line: &str) -> String {
        let mut words = line.split_whitespace();
        let verb = words.next().unwrap_or("");
        let rest: Vec<&str> = words.collect();

        if verb == "dump" && rest.is_empty() {
            return self.cli_dump();
        }
        match self.cli_execute(verb, &rest) {
            Ok(ok) => format!(
                "{{\"ok\": {}}}",
                serde_json::to_string(&ok).unwrap_or_default()
            ),
            Err(err) => {
                format!(
                    "{{\"error\": {}}}",
                    serde_json::to_string(&err).unwrap_or_default()
                )
            }
        }
    }

    /// The verb table. `Result` so every arm can use `?` on its argument parsing.
    fn cli_execute(&mut self, verb: &str, rest: &[&str]) -> Result<String, String> {
        match (verb, rest) {
            // Matches on the id or the name, case-insensitively, so `load mxm-mono-01` works. The
            // `found` list in the dump is where a machine learns what is loadable.
            ("load", [wanted]) => {
                let found = self
                    .found
                    .iter()
                    .find(|f| {
                        f.is_supported()
                            && (f.id.eq_ignore_ascii_case(wanted)
                                || f.name.eq_ignore_ascii_case(wanted)
                                || f.id.to_ascii_lowercase().ends_with(&format!(
                                    ".{}",
                                    wanted.to_ascii_lowercase()
                                )))
                    })
                    .cloned()
                    .ok_or_else(|| {
                        format!("no loadable plugin matches `{wanted}` - see `found` in the dump")
                    })?;
                self.load(found.bundle.clone(), found.id.clone());
                Ok(format!("loading {}", found.id))
            }
            // `select 5` picks one step; `select 5-8` a range; `select 1,5,9,13` a scattered
            // set. The first named step is the anchor - the one that previews.
            ("select", [steps]) => {
                if let Some((from, to)) = steps.split_once('-') {
                    let from = self.cli_step(from)?;
                    let to = self.cli_step(to)?;
                    self.select_step(from);
                    self.shift_select_step(to);
                    Ok(format!("selected steps {}-{}", from.min(to) + 1, from.max(to) + 1))
                } else if steps.contains(',') {
                    let mut parsed = Vec::new();
                    for part in steps.split(',') {
                        parsed.push(self.cli_step(part)?);
                    }
                    let anchor = *parsed.first().ok_or("select needs at least one step")?;
                    self.select_step(anchor);
                    for step in &parsed[1..] {
                        if *step != anchor {
                            self.ctrl_select_step(*step);
                        }
                    }
                    Ok(format!("selected {} steps", parsed.len()))
                } else {
                    let step = self.cli_step(steps)?;
                    self.select_step(step);
                    Ok(format!("selected step {}", step + 1))
                }
            }
            ("deselect", []) => {
                self.deselect_step();
                Ok("deselected".to_owned())
            }
            ("set", [param, value]) => self.cli_set(param, Some(value)),
            ("reset", [param]) => self.cli_set(param, None),
            ("toggle", [step, note]) => {
                let s = self.cli_step(step)?;
                let key = crate::sequencer::pattern::parse_note(note)
                    .or_else(|| note.parse::<u8>().ok())
                    .ok_or_else(|| format!("`{note}` is not a note name or MIDI key"))?;
                self.toggle_step_note(s, key);
                Ok(format!("toggled {note} on step {}", s + 1))
            }
            ("tie", [step]) => {
                let s = self.cli_step(step)?;
                self.toggle_step_tie(s);
                Ok(format!("toggled the tie on step {}", s + 1))
            }
            ("lock", [step, param, value]) => {
                let s = self.cli_step(step)?;
                let key = self.cli_lock_key(param)?;
                let value: f64 = value
                    .parse()
                    .map_err(|_| format!("`{value}` is not a number"))?;

                let value = self.valid_lock_value(key, value)?;
                if !key.is_source() {
                    // A readback can include the current offset. Once locked, keep the recorded
                    // baseline rather than adopting that modulated readback on every CLI edit.
                    let patch = self.sequencer.locks.patch(key)
                        .or_else(|| self.fx_patch(key))
                        .ok_or_else(|| format!("cannot read the baseline for {param}"))?;
                    self.write_step_lock(s, key, value, patch)?;
                    return Ok(format!("step {} locks {param} at {value}", s + 1));
                }
                let id = key.param_id;
                // The same funnel a selected-step edit uses, so the CLI cannot author a lock the
                // interface could not — and every guard (modulatable, capacity) applies.
                let before = self.selected_step;
                // The verb names exactly one step; a multi-step selection in the window must not
                // ride along on the swapped anchor.
                let companions = std::mem::take(&mut self.also_selected);
                self.selected_step = Some(s);
                let accepted = self.parameter_edited(id, value);
                self.selected_step = before;
                self.also_selected = companions;
                if accepted {
                    Ok(format!("step {} locks {param} at {value}", s + 1))
                } else {
                    Err(self.status.clone().unwrap_or_else(|| "refused".to_owned()))
                }
            }
            ("unlock", [step, param]) => {
                let s = self.cli_step(step)?;
                let id = self.cli_lock_key(param)?;
                // **The step itself.** Locks are per step — tied ones included — so the step named
                // is the step cleared, and there is no owner to resolve any more.
                self.sequencer.locks.clear(s, id);
                self.sequencer_changed();
                self.locks_unsaved = true;
                Ok(format!("step {} no longer locks {param}", s + 1))
            }
            ("tempo", [bpm]) => bpm
                .parse::<f64>()
                .ok()
                .map(|t| {
                    self.set_tempo(t);
                    format!("tempo {} BPM", self.sequencer.tempo)
                })
                .ok_or_else(|| format!("`{bpm}` is not a tempo")),
            ("bars", [count]) => count
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .map(|n| {
                    self.set_bars(n);
                    format!(
                        "{} bars of {} steps",
                        self.sequencer.pattern.bars(),
                        self.sequencer.pattern.steps_per_bar()
                    )
                })
                .ok_or_else(|| format!("`{count}` is not a number of bars")),
            ("steps", [count]) => count
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .map(|n| {
                    self.set_steps_per_bar(n);
                    format!(
                        "{} bars of {} steps",
                        self.sequencer.pattern.bars(),
                        self.sequencer.pattern.steps_per_bar()
                    )
                })
                .ok_or_else(|| format!("`{count}` is not a number of steps")),
            // Not bounded by the bars that exist: viewing is free, and bars materialise when
            // notes land in them.
            ("bar", [bar]) => bar
                .parse::<usize>()
                .ok()
                .filter(|bar| (1..=10_000).contains(bar))
                .map(|bar| {
                    self.select_bar(bar - 1);
                    format!("showing bar {bar}")
                })
                .ok_or_else(|| "bars are counted from 1".to_owned()),
            ("copysteps", []) => {
                self.copy_steps();
                Ok(self.status.clone().unwrap_or_default())
            }
            ("cutsteps", []) => {
                self.cut_steps();
                Ok(self.status.clone().unwrap_or_default())
            }
            ("copybar", []) => {
                self.copy_bar();
                Ok(self.status.clone().unwrap_or_default())
            }
            ("copypattern", []) => {
                self.copy_pattern();
                Ok(self.status.clone().unwrap_or_default())
            }
            ("paste", []) => {
                let before = self.sequencer_serial;
                self.paste_clipboard();
                let answer = self.status.clone().unwrap_or_default();
                if self.sequencer_serial == before {
                    return Err(answer);
                }
                Ok(answer)
            }
            ("clearbar", []) => {
                self.clear_bar();
                Ok(self.status.clone().unwrap_or_default())
            }
            ("clearpattern", []) => {
                self.clear_pattern_bars();
                Ok(self.status.clone().unwrap_or_default())
            }
            ("loop", [scope]) => match *scope {
                "bar" => {
                    self.set_loop_scope(LoopScope::Bar);
                    Ok(format!("looping bar {}", self.selected_bar + 1))
                }
                "pattern" => {
                    self.set_loop_scope(LoopScope::Pattern);
                    let first = self.selected_bar / PATTERN_BARS * PATTERN_BARS;
                    Ok(format!("looping bars {}-{}", first + 1, first + PATTERN_BARS))
                }
                "all" => {
                    self.set_loop_scope(LoopScope::All);
                    Ok("looping the whole sequence".to_owned())
                }
                _ => Err("loop takes `bar`, `pattern` or `all`".to_owned()),
            },
            ("clear", []) => {
                self.clear_pattern();
                Ok("cleared".to_owned())
            }
            ("random", []) => {
                self.randomise();
                Ok(self.status.clone().unwrap_or_default())
            }
            ("play", []) => {
                self.try_play_from_start()?;
                Ok("playing from step 1".to_owned())
            }
            ("stop", []) => {
                self.stop_sequencer();
                Ok("stopped".to_owned())
            }
            // What a hardware knob or a latched touch strip does, reachable by a machine. Added
            // the day a stuck mod wheel could be *diagnosed* over the socket but not poked: the
            // vibrato-at-zero question needed `cc 1 0` and the vocabulary had no such sentence.
            ("cc", [controller, value]) => {
                let controller: u8 = controller
                    .parse()
                    .ok()
                    .filter(|c| *c < 128)
                    .ok_or("a controller is 0..=127")?;
                let value: u8 = value
                    .parse()
                    .ok()
                    .filter(|v| *v < 128)
                    .ok_or("a CC value is 0..=127")?;
                self.send_control_change(controller, value);
                Ok(format!("CC {controller} = {value}"))
            }
            ("note", [key]) => {
                let k = crate::sequencer::pattern::parse_note(key)
                    .or_else(|| key.parse::<u8>().ok())
                    .ok_or_else(|| format!("`{key}` is not a note"))?;
                self.note_on(k, 100.0 / 127.0);
                Ok(format!("note {k} on - send `off {key}` to release"))
            }
            ("off", [key]) => {
                let k = crate::sequencer::pattern::parse_note(key)
                    .or_else(|| key.parse::<u8>().ok())
                    .ok_or_else(|| format!("`{key}` is not a note"))?;
                self.note_off(k);
                Ok(format!("note {k} off"))
            }
            // **The state round-trip check, as verbs.** It was a panel section ("State dump")
            // for years; it is a plugin-development diagnostic, not a user control, and the CLI
            // is where machine-facing checks live. One slot, fixed path, not a preset.
            ("dumpstate", []) => {
                let path = self.settings_path.with_file_name(STATE_DUMP);
                self.engine
                    .save_state(&path)
                    .map(|()| format!("state dumped to {}", path.display()))
            }
            ("loadstate", []) => {
                let path = self.settings_path.with_file_name(STATE_DUMP);
                self.engine
                    .load_state(&path, &mut self.params)
                    .map(|()| "state dump loaded".to_owned())
            }
            // **The chain's acts.** `fx add` takes an id or a name the way `load` does, from what
            // the scan found effect-capable; the rest address an effect by its position in the
            // chain, one-based, as the strips are numbered. Dump/load are the generic developer
            // seam for durable effect model state that is intentionally not a CLAP parameter.
            ("fx", ["add", wanted]) => {
                let found = self
                    .found
                    .iter()
                    .find(|f| {
                        f.is_effect()
                            && (f.id.eq_ignore_ascii_case(wanted)
                                || f.name.eq_ignore_ascii_case(wanted)
                                || f.id.to_ascii_lowercase().ends_with(&format!(
                                    ".{}",
                                    wanted.to_ascii_lowercase()
                                )))
                    })
                    .cloned()
                    .ok_or_else(|| {
                        format!("no effect matches `{wanted}` - see `found` in the dump")
                    })?;
                let index = self.add_fx(found.bundle.clone(), found.id.clone())?;
                Ok(format!("added {} as effect {}", found.id, index + 1))
            }
            ("fx", ["remove", n]) => {
                let index = self.cli_fx_index(n)?;
                self.remove_fx(index)?;
                Ok(format!("removed effect {}", index + 1))
            }
            ("fx", ["move", from, to]) => {
                let from = self.cli_fx_index(from)?;
                let to = self.cli_fx_index(to)?;
                self.move_fx(from, to)?;
                Ok(format!("moved effect {} to {}", from + 1, to + 1))
            }
            ("fx", ["on", n]) => {
                let index = self.cli_fx_index(n)?;
                self.set_fx_bypassed(index, false)?;
                Ok(format!("effect {} on", index + 1))
            }
            ("fx", ["off", n]) => {
                let index = self.cli_fx_index(n)?;
                self.set_fx_bypassed(index, true)?;
                Ok(format!("effect {} off", index + 1))
            }
            ("fx", ["editor", n]) => {
                let index = self.cli_fx_index(n)?;
                self.show_fx_editor(index)?;
                Ok(format!("effect {}'s editor opened", index + 1))
            }
            ("fx", ["dumpstate", n]) => {
                let index = self.cli_fx_index(n)?;
                let path = self
                    .settings_path
                    .with_file_name(format!("effect-{}.clapstate", index + 1));
                let bytes = self.engine.capture_fx_state(index)?;
                std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
                Ok(format!(
                    "effect {} state dumped to {}",
                    index + 1,
                    path.display()
                ))
            }
            ("fx", ["loadstate", n]) => {
                let index = self.cli_fx_index(n)?;
                let path = self
                    .settings_path
                    .with_file_name(format!("effect-{}.clapstate", index + 1));
                let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                self.engine.load_fx_state(index, &bytes)?;
                Ok(format!("effect {} state dump loaded", index + 1))
            }
            ("fx", _) => Err(
                "fx takes `add <plugin>`, `remove <n>`, `move <from> <to>`, `on <n>`, `off <n>`, \
                 `editor <n>`, `dumpstate <n>` or `loadstate <n>`"
                    .to_owned(),
            ),
            ("export", [name]) => self
                .export_audio(name)
                .map(|path| format!("exported {}", path.display())),
            ("save", [name]) => self
                .save_sequence(name)
                .map(|path| format!("saved {}", path.display())),
            ("savemid", [name]) => self
                .save_midi(name)
                .map(|path| format!("saved {}", path.display())),
            // The keyboard's octave, absolute — the same act the -/+ buttons and the octave keys
            // step through.
            ("octave", [n]) => n
                .parse::<i32>()
                .ok()
                .filter(|n| (keyboard::MIN_OCTAVE..=keyboard::MAX_OCTAVE).contains(n))
                .map(|n| {
                    self.shift_octave(n - self.octave);
                    format!("octave {}", self.octave)
                })
                .ok_or_else(|| {
                    format!(
                        "octaves are {}..={}",
                        keyboard::MIN_OCTAVE,
                        keyboard::MAX_OCTAVE
                    )
                }),
            // The sweep's finds: acts the panel offered that no verb reached.
            ("rescan", []) => {
                self.rescan();
                Ok(format!(
                    "rescanned: {} plugins found",
                    self.found.len()
                ))
            }
            ("panic", []) => {
                self.engine.push_gui_event(Payload::GlobalPanic);
                Ok("panic sent - all sound off".to_owned())
            }
            // **Peak normalisation is right for making samples and wrong for measuring one.**
            // It is on by default because an exported hit is usually headed for a sampler, but a
            // render used to compare two patches has to keep its own gain: normalised, a pure
            // level change cancels exactly and two renders come back byte-identical.
            ("normalise", [state]) => match *state {
                "on" => {
                    self.set_normalise_export(true);
                    Ok("exports are peak-normalised".to_owned())
                }
                "off" => {
                    self.set_normalise_export(false);
                    Ok("exports keep their own gain".to_owned())
                }
                other => Err(format!("normalise takes on or off, not `{other}`")),
            },
            ("sustain", [state]) => match *state {
                "on" => {
                    self.set_sustain(true);
                    Ok("sustain held".to_owned())
                }
                "off" => {
                    self.set_sustain(false);
                    Ok("sustain released".to_owned())
                }
                _ => Err("sustain takes `on` or `off`".to_owned()),
            },
            ("bend", [amount]) => amount
                .parse::<f64>()
                .ok()
                .filter(|a| (-1.0..=1.0).contains(a))
                .map(|a| {
                    self.engine.push_gui_event(Payload::PitchBend {
                        channel: 0,
                        value: a,
                    });
                    format!("bend {a}")
                })
                .ok_or_else(|| "bend takes -1.0..=1.0".to_owned()),
            ("loadseq", [name]) => {
                let path = self.sequence_dir().join(format!("{name}.seq.json"));
                self.load_sequence(&path)
                    .map(|()| format!("loaded {}", path.display()))
            }
            // The two collapsible panels, separately addressable — they collapse independently.
            ("panel", [which, state]) => {
                let open = match *state {
                    "open" => true,
                    "closed" => false,
                    _ => return Err("panel takes `open` or `closed`".to_owned()),
                };
                match *which {
                    "settings" => {
                        self.settings.settings_collapsed = !open;
                        self.persist();
                        Ok(format!(
                            "settings panel {}",
                            if open { "open" } else { "closed" }
                        ))
                    }
                    "parameters" => {
                        self.settings.parameters_collapsed = !open;
                        self.persist();
                        Ok(format!(
                            "parameters panel {}",
                            if open { "open" } else { "closed" }
                        ))
                    }
                    _ => Err("panel takes `settings` or `parameters`".to_owned()),
                }
            }
            // The app bar's Theme control. No context to set it on from here — the next frame
            // carries `self.theme` into one — so this is the one verb whose act lands a frame
            // later, which is still before anything can look.
            ("theme", [wanted]) => {
                let preference = match *wanted {
                    "light" => egui::ThemePreference::Light,
                    "dark" => egui::ThemePreference::Dark,
                    "system" => egui::ThemePreference::System,
                    _ => return Err("theme takes `light`, `dark` or `system`".to_owned()),
                };
                self.theme = preference;
                self.settings.theme = Some(theme_name(preference).to_owned());
                self.persist();
                Ok(format!("theme {wanted}"))
            }
            _ => Err(
                "commands: dump | load <plugin> | select <1-16> | deselect | set <param> <value> | \
                 reset <param> | toggle <step> <note> | tie <step> | \
                 lock <step> <param> <value> | unlock <step> <param> | tempo <bpm> | \
                 clear | random | play | stop | note <key> | off <key> | \
                 bar <n> | copysteps | cutsteps | copybar | copypattern | paste | \
                 clearbar | clearpattern | \
                 loop <bar|pattern|all> | dumpstate | loadstate | \
                 octave <n> | panel <settings|parameters> <open|closed> | \
                 theme <light|dark|system> | panic | rescan | \
                 sustain <on|off> | normalise <on|off> | bend <amount> | loadseq <name> | \
                 cc <controller> <value> | export <name> | save <name> | savemid <name> |                  fx add <plugin> | fx remove <n> | fx move <from> <to> | fx on <n> |                  fx off <n> | fx editor <n> | fx dumpstate <n> | fx loadstate <n>"
                    .to_owned(),
            ),
        }
    }

    /// A 1-based step argument, as people count them.
    /// A step argument: absolute (`33`) or bar-relative (`3:1`), both one-based — two spellings
    /// of the same step, so a generated sequence and a person reading the screen use whichever
    /// fits. **Not bounded by the sequence's length**: writing into a bar beyond the end is how
    /// the sequence grows, and a machine authoring bar ninety starts with nothing else there.
    fn cli_step(&self, step: &str) -> Result<usize, String> {
        let spb = self.sequencer.pattern.steps_per_bar();
        let parsed = match step.split_once(':') {
            Some((bar, within)) => {
                let bar: usize = bar.parse().ok().filter(|b| *b >= 1).ok_or_else(|| {
                    format!("`{step}` is not a step; bar:step counts both from 1")
                })?;
                let within: usize = within
                    .parse()
                    .ok()
                    .filter(|s| (1..=spb).contains(s))
                    .ok_or_else(|| format!("`{step}` is not a step; a bar holds {spb}"))?;
                Some((bar - 1) * spb + (within - 1))
            }
            None => step
                .parse::<usize>()
                .ok()
                .filter(|s| *s >= 1)
                .map(|s| s - 1),
        };
        parsed
            .filter(|s| *s < 100_000)
            .ok_or_else(|| "a step is `33` or `bar:step` like `3:1`, counted from 1".to_owned())
    }

    /// A parameter by name (case-insensitive, spaces as underscores) or numeric id.
    /// An effect's position as the strips number it: one-based, and it must exist.
    fn cli_fx_index(&self, n: &str) -> Result<usize, String> {
        let len = self.engine.fx_len();
        let index: usize = n
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| format!("`{n}` is not an effect position; they start at 1"))?;
        if index > len {
            return Err(match len {
                0 => "the chain is empty".to_owned(),
                1 => "the chain has one effect".to_owned(),
                n => format!("the chain has {n} effects"),
            });
        }
        Ok(index - 1)
    }

    fn cli_param(&self, param: &str) -> Result<u32, String> {
        let wanted = param.replace('_', " ");
        self.params
            .params
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(&wanted))
            .map(|p| p.id)
            .or_else(|| param.parse::<u32>().ok())
            .ok_or_else(|| format!("no parameter named `{param}`"))
    }

    /// Resolves a lock target written as `fx<n>:<parameter>`, or the source's bare form.
    ///
    /// **`fx<n>` is the chain position a person can see**, one-based, exactly as every other `fx`
    /// verb takes — and it is turned into the effect's *id* here, once, so that everything stored
    /// afterwards survives the chain being reordered underneath it.
    fn cli_lock_key(&mut self, param: &str) -> Result<crate::sequencer::locks::LockKey, String> {
        let Some((prefix, name)) = param.split_once(':') else {
            return Ok(crate::sequencer::locks::LockKey::source(
                self.cli_param(param)?,
            ));
        };
        let Some(digits) = prefix.strip_prefix("fx") else {
            return Err(format!(
                "`{prefix}` is not a target; write `fx<n>:<parameter>` for an effect, or the                  parameter alone for the instrument"
            ));
        };
        let index = self.cli_fx_index(digits)?;
        let id = self
            .engine
            .fx_info()
            .get(index)
            .map(|info| info.id)
            .ok_or_else(|| format!("there is no effect {digits}"))?;
        let params = self
            .engine
            .read_fx_params(index)
            .ok_or_else(|| format!("effect {digits} has no parameters to read"))?;
        let wanted = name.replace('_', " ");
        let param_id = params
            .params
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(&wanted))
            .map(|p| p.id)
            .or_else(|| name.parse::<u32>().ok())
            .ok_or_else(|| format!("effect {digits} has no parameter named `{name}`"))?;
        Ok(crate::sequencer::locks::LockKey::fx(id, param_id))
    }

    /// Metadata belongs to the target, never to a same-numbered parameter in the source.
    fn lock_parameter(&mut self, key: crate::sequencer::locks::LockKey) -> Option<ParamSnapshot> {
        if key.is_source() {
            return self.params.get(key.param_id).cloned();
        }
        let index = self
            .engine
            .fx_info()
            .iter()
            .position(|info| info.id == key.fx)?;
        self.engine
            .read_fx_params(index)?
            .get(key.param_id)
            .cloned()
    }

    fn valid_lock_value(
        &mut self,
        key: crate::sequencer::locks::LockKey,
        value: f64,
    ) -> Result<f64, String> {
        let param = self
            .lock_parameter(key)
            .ok_or_else(|| format!("unknown parameter {key}"))?;
        if !sequenceable(&param) {
            return Err(format!("{} cannot be sequenced", param.name));
        }
        if !value.is_finite()
            || !param.min.is_finite()
            || !param.max.is_finite()
            || param.min > param.max
        {
            return Err(format!("{key}: value and parameter range must be finite"));
        }
        let value = value.clamp(param.min, param.max);
        let value = if param.is_stepped {
            value.round().clamp(param.min, param.max)
        } else {
            value
        };
        if !(value as f32).is_finite() {
            return Err(format!("{key}: value exceeds the lock representation"));
        }
        Ok(value)
    }

    /// The shared authoring funnel for the source's selected-step edit and effect CLI locks.
    fn write_step_lock(
        &mut self,
        step: usize,
        key: crate::sequencer::locks::LockKey,
        value: f64,
        patch: f32,
    ) -> Result<(), String> {
        // No-source authoring is validated when an instrument arrives. CLI authoring already
        // requires a real parameter; an absent effect is never an authorable target.
        let value = if key.is_source() && self.engine.plugin_id().is_none() {
            if !(value as f32).is_finite() {
                return Err("lock value must be finite".into());
            }
            value
        } else {
            self.valid_lock_value(key, value)?
        };
        self.sequencer
            .locks
            .set(step, key, value as f32, patch)
            .map_err(|e| e.to_string())?;
        self.grow_to_reach(step);
        self.sequencer_changed();
        self.locks_unsaved = true;
        Ok(())
    }

    /// What an effect currently has a parameter set to: the baseline an effect lock deviates from.
    fn fx_patch(&mut self, key: crate::sequencer::locks::LockKey) -> Option<f32> {
        let index = self
            .engine
            .fx_info()
            .iter()
            .position(|info| info.id == key.fx)?;
        let params = self.engine.read_fx_params(index)?;
        params
            .params
            .iter()
            .find(|p| p.id == key.param_id)
            .map(|p| p.value as f32)
    }

    /// The chain as the lock file names it: each effect's live id and its `CLAP_ID`, in order.
    ///
    /// **Read fresh every time rather than cached.** It is what turns a saved reference into a live
    /// id and back, and a stale copy would attach a pattern's automation to the wrong effect — the
    /// exact failure the target exists to prevent.
    fn chain_refs(&mut self) -> Vec<(crate::engine::fx::FxId, String)> {
        self.engine
            .fx_info()
            .into_iter()
            .map(|info| (info.id, info.plugin_id))
            .collect()
    }

    /// One serializer for settings, explicit saves and source-switch parking.
    fn capture_locks(&mut self) -> Result<crate::sequencer::locks::LockData, String> {
        let chain = self.chain_refs();
        let live = crate::sequencer::locks::LockData::capture_in_chain(
            &self.sequencer.locks,
            self.engine.plugin_id(),
            &chain,
            &self.pending_fx_locks,
        )?;
        if let Some(mut parked) = self.parked_locks.clone() {
            // Authoring Source under the new instrument replaces the parked source; one record
            // cannot truthfully tag two different instruments' parameter namespaces.
            if live.params.iter().any(|entry| entry.fx.is_none()) {
                return Ok(live);
            }
            parked.params.extend(live.params);
            Ok(parked)
        } else {
            Ok(live)
        }
    }

    fn resolve_pending_fx_locks(&mut self) {
        if self.pending_fx_locks.is_empty() {
            return;
        }
        let chain = self.chain_refs();
        // Include the live set so the combined lock budgets are checked, not two separate sets.
        let data = match crate::sequencer::locks::LockData::capture_in_chain(
            &self.sequencer.locks,
            self.engine.plugin_id(),
            &chain,
            &self.pending_fx_locks,
        ) {
            Ok(data) => data,
            Err(reason) => {
                self.status = Some(reason);
                return;
            }
        };
        let (locks, mut problems, pending) =
            data.to_locks_in_chain(self.engine.plugin_id(), &chain);
        self.sequencer.locks = locks;
        self.pending_fx_locks = pending;
        problems.extend(self.prune_unknown_locks());
        self.sync_baselines();
        self.sequencer_changed();
        self.sequence_problems.extend(problems);
    }

    /// Sets a parameter, or resets it when `value` is `None`.
    fn cli_set(&mut self, param: &str, value: Option<&str>) -> Result<String, String> {
        let id = self.cli_param(param)?;
        let Some(value) = value else {
            self.reset_parameter(id);
            return Ok(format!("reset {param}"));
        };
        let value: f64 = value
            .parse()
            .map_err(|_| format!("`{value}` is not a number"))?;
        self.set_parameter(id, value.clamp(0.0, 1.0));
        Ok(format!("set {param} to {value}"))
    }

    /// Everything the window shows, **plus what it deliberately hides**.
    ///
    /// The additions are the point. `state()`\'s parameter values are corrected — a locked parameter
    /// reads as the patch, by design — so the readbacks that would expose a base-vs-patch divergence
    /// never appear in it. `raw_readback` is the plugin\'s own answer, uncorrected;
    /// `sequence_patch`, `parked_bases` and `modulation` are the bookkeeping the corrections are
    /// computed *from*. A machine holding all four can see every bug class this feature has had.
    fn cli_dump(&mut self) -> String {
        #[derive(serde::Serialize)]
        struct Dump {
            state: crate::state::PlayerState,
            /// Keyed by the lock's **written form** — a bare id for the source, `fx<id>:<param>`
            /// for an effect. JSON has no struct keys, and a dump that failed to serialise is a
            /// dump that says nothing at all: this one did, silently, until the conformance test
            /// noticed `parked_bases` had gone missing along with everything else.
            sequence_patch: std::collections::BTreeMap<String, f64>,
            parked_bases: Vec<u32>,
            modulation: std::collections::BTreeMap<String, f64>,
            raw_readback: std::collections::BTreeMap<u32, f64>,
        }

        let mut raw = self.params.clone();
        self.engine.refresh_param_values(&mut raw);

        let dump = Dump {
            state: self.state(),
            sequence_patch: self
                .sequence_patch
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
            parked_bases: {
                let mut v: Vec<u32> = self.parked_bases.iter().copied().collect();
                v.sort_unstable();
                v
            },
            modulation: self
                .modulation
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
            raw_readback: raw.params.iter().map(|p| (p.id, p.value)).collect(),
        };
        serde_json::to_string(&dump)
            .unwrap_or_else(|e| format!("{{\"error\": \"dump failed: {e}\"}}"))
    }
}

/// A pattern is eight bars: the interface's fixed geography — chips, the stepper, the loop's
/// middle scope and pattern copy/paste all move in these units, however long the sequence is.
pub const PATTERN_BARS: usize = 8;

/// One declaration, two artefacts: the data-carrying [`Gesture`] the input handlers dispatch,
/// and the fieldless [`GestureKind`] inventory with its `ALL` slice — generated together so
/// there is no second list to forget. The conformance sweep iterates `GestureKind::ALL` and
/// requires a coverage row per kind; a gesture added here without one fails the sweep, and a
/// gesture added *without* being here is a review-visible violation of the funnel rule below.
macro_rules! gestures {
    ($($name:ident $({ $($field:ident : $ty:ty),* $(,)? })?),* $(,)?) => {
        /// Something a hand did, routed through [`PlayerApp::perform`] — **the** funnel.
        #[derive(Copy, Clone, Debug, PartialEq)]
        pub enum Gesture {
            $($name $({ $($field: $ty),* })?),*
        }

        /// The inventory of gesture kinds, machine-iterable.
        #[derive(Copy, Clone, Debug, PartialEq, Eq)]
        pub enum GestureKind {
            $($name),*
        }

        impl Gesture {
            pub fn kind(&self) -> GestureKind {
                match self {
                    $(Gesture::$name { .. } => GestureKind::$name),*
                }
            }
        }

        impl GestureKind {
            pub const ALL: &'static [GestureKind] = &[$(GestureKind::$name),*];
        }
    };
}

gestures!(
    ClickStep { step: usize },
    ShiftClickStep { step: usize },
    CtrlClickStep { step: usize },
    EscapeKey,
    SpaceKey,
    CopyShortcut,
    CutShortcut,
    PasteShortcut,
    ResetParam { param_id: u32 },
    PlayNote { key: u8 },
    ReleaseNote { key: u8 },
    OctaveShift { by: i32 },
);

/// What the transport repeats: one bar, one eight-bar pattern, or the whole sequence.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum LoopScope {
    Bar,
    Pattern,
    #[default]
    All,
}

/// One bar's content, held for paste: notes, ties and locks — one notion of "what a bar holds".
///
/// **The locks carry the plugin they were copied under**, and are dropped on paste when that no
/// longer matches — exactly as `LockData::to_locks` does for a loaded sequence, and for the same
/// reason: a parameter id means nothing outside the instrument that assigned it. The notes and
/// ties still paste; only the locks are instrument-bound.
struct BarClipboard {
    /// What was copied — whole bars, or a run of steps. Paste applies it at the matching level.
    content: ClipContent,
    plugin: Option<String>,
    /// The bar shape at copy time. A 16-step bar pasted into a 12-step one would have to be
    /// truncated or padded, and both silently reshape music — so paste refuses instead.
    steps_per_bar: usize,
}

/// One bar's cells, in the clipboard.
struct ClipBar {
    steps: Vec<crate::sequencer::Step>,
    tied: Vec<bool>,
    /// `(step offset within the bar, parameter, value, the patch recorded for it)`.
    ///
    /// **Keyed, not a bare id**: copying a bar carries an effect's automation with it, and a
    /// paste that had forgotten which plugin it belonged to would land it on the source.
    locks: Vec<(usize, crate::sequencer::locks::LockKey, f32, Option<f32>)>,
}

impl ClipBar {
    /// Whether this bar holds anything at all — what decides how far a paste grows the sequence.
    fn holds_content(&self) -> bool {
        self.steps.iter().any(|step| !step.is_empty())
            || self.tied.iter().any(|tied| *tied)
            || !self.locks.is_empty()
    }
}

/// Whole bars, or a dense run of steps — the two things Copy can take.
enum ClipContent {
    Bars(Vec<ClipBar>),
    Steps(Vec<ClipStep>),
}

/// One step's cells, in the clipboard. A scattered selection copies as a dense run, in order —
/// exactly as a text editor concatenates a multi-cursor copy.
struct ClipStep {
    step: crate::sequencer::Step,
    tied: bool,
    /// `(parameter, value, the patch recorded for it)`. Keyed; see [`ClipBar`].
    locks: Vec<(crate::sequencer::locks::LockKey, f32, Option<f32>)>,
}

impl ClipStep {
    fn holds_content(&self) -> bool {
        !self.step.is_empty() || self.tied || !self.locks.is_empty()
    }
}

/// Whether a step may set this parameter.
///
/// **A read-only parameter cannot be set at all**, and **bypass is the host's switch** rather than
/// part of the sound — a step that silently bypassed the instrument would be the most confusing lock
/// it is possible to write.
///
/// A free function so it can be tested against parameters that do not exist in this collection. No
/// MXM instrument declares either flag today, and the player hosts any CLAP: the guard is about the
/// plugins it will meet, not the ones it ships with, so a session test against mxm-mono-01 could only
/// ever skip.
fn sequenceable(param: &ParamSnapshot) -> bool {
    // **Modulatable, because that is how a step's deviation is sent.** The player hosts any CLAP,
    // and a plugin that does not advertise `IS_MODULATABLE` for a parameter has said it will not
    // accept `CLAP_EVENT_PARAM_MOD` for it — sending one anyway is out of spec, and what happens
    // next is that plugin's business rather than something to find out by trying.
    param.is_modulatable && !(param.is_read_only || param.is_bypass)
}

/// A top-bar toggle that **looks like a control before you touch it**.
///
/// Reported as hovering *Parameters* or *Settings* moving the interface. It does not: the row is
/// measurably stable — only the hovered control's own pixels change. What moved was the **frame**,
/// which `selectable_label` draws on hover and not at rest, so it materialises a few pixels outside
/// the text that was there a moment earlier. Hover was creating the affordance rather than
/// strengthening one, which is the thing design system §7.2 exists to forbid, and a border arriving
/// out of nowhere is indistinguishable from the row shifting.
///
/// `frame_when_inactive` gives it a resting border. Hover then changes only the fill.
fn toggle_button(ui: &egui::Ui, label: &str, selected: bool) -> egui::Button<'static> {
    let button = egui::Button::new(label.to_owned())
        .selected(selected)
        .frame_when_inactive(true);
    if selected {
        // SS 7.2: selection fill *and* an accent border, like the segmented control's selected
        // cell. egui's selected-button path swaps only the fill and the text colour, which left
        // a selected toggle one pale wash away from an idle one - invisible on the loop row,
        // found live twice.
        button.stroke(egui::Stroke::new(
            2.0,
            mxm_ui::theme::tokens(ui.ctx()).accent,
        ))
    } else {
        button
    }
}

impl PlayerApp {
    /// The windowed player, wired to the machine it is running on.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Before the first frame, so the first thing drawn is already styled. `ui` re-applies
        // every frame to follow a host theme change, but a `Ui` reads a clone of the style it was
        // built with, so that path is always one frame behind — which would show as an unstyled
        // flash on startup without this.
        mxm_ui::theme::apply(&cc.egui_ctx);
        mxm_ui::typography::apply(&cc.egui_ctx);

        let mut app = Self::with_config(PlayerConfig::production());

        // The environment wins over the saved choice for this run and is not written back: see
        // [`THEME_ENV`]. `from_name` is `mxm_ui`'s, so the host and the editors read the same
        // names — and its "anything else is Light" rule is tested there rather than here, where
        // reading the process environment would make a test of it order-dependent.
        if let Ok(name) = std::env::var(THEME_ENV) {
            app.theme = mxm_ui::theme::from_name(&name);
        }
        cc.egui_ctx.set_theme(app.theme);

        // The CLI answers only in a windowed run. Headless tests build through `with_config` and
        // call `start_cli` themselves on their own sandboxed directory — otherwise every parallel
        // test would fight over one port file.
        app.start_cli();
        app.notifier = Some(Notifier::spawn(
            cc.egui_ctx.clone(),
            app.engine.shared().clone(),
        ));
        app
    }

    /// The player wired to whatever [`PlayerConfig`] says — including no window, no device and a
    /// sandboxed settings directory.
    ///
    /// There is no notifier here: waking the GUI needs an egui context, and a headless run has
    /// none. Everything else is identical to the windowed path, which is the point — a test that
    /// exercised a different construction would prove nothing about the real one.
    pub fn with_config(config: PlayerConfig) -> Self {
        let PlayerConfig {
            backend,
            settings_path,
            sentinel_path,
            control_map_path,
            sequence_dir,
            exports_dir,
            search_paths,
            clock,
            midi_ports,
        } = config;

        let settings = Settings::load(&settings_path);

        let mut engine = Engine::with_clock(clock);
        engine.set_audio_config(AudioConfig {
            device_name: settings.audio_device.clone(),
            sample_rate: settings.sample_rate,
            buffer_size: settings.buffer_size,
        });
        engine
            .thru_enabled
            .store(settings.midi_thru, Ordering::Release);
        // The saved MIDI selection has to reach the engine here, next to the audio config and
        // thru. The panel draws its ticks from `settings`, and `midi_controls` only calls
        // `set_midi_inputs` when the selection *changes* — so without this a restored tick
        // described a port that was never opened, and the keyboard it names did nothing until
        // the user unticked and re-ticked it. Both are recorded, so both are restored.
        engine.set_midi_inputs(settings.midi_inputs.clone());
        engine.set_midi_output(settings.midi_output.clone());

        let sentinel = Sentinel::new(sentinel_path);
        let suspected = sentinel.suspected();

        let audio_devices = backend.devices();
        let midi_inputs = midi_ports.inputs();
        let midi_outputs = midi_ports.outputs();

        let octave = settings.octave.unwrap_or(3);
        let settings_window_size = settings.window_size;

        // Read before `settings` is moved into the struct below.
        let tempo = settings.tempo.unwrap_or(sequencer::DEFAULT_TEMPO);
        let random_seed = settings.random_seed.unwrap_or(0x5EED_1234_ABCD_0001);
        let saved_locks = settings.sequence_locks.clone();
        let saved_pattern = {
            // **The shape comes from the file too.** A settings file written before a sequence had
            // one loads as a bar of sixteen, which is the shape it had.
            let bars = settings.sequence_bars.unwrap_or(1);
            let steps_per_bar = settings
                .sequence_steps_per_bar
                .unwrap_or(sequencer::pattern::STEPS);
            let mut pattern = settings
                .sequence_steps
                .as_ref()
                .map(|data| data.to_pattern_shaped(bars, steps_per_bar).0)
                .unwrap_or_else(|| {
                    let mut empty = Pattern::empty();
                    empty.set_steps_per_bar(steps_per_bar);
                    empty.set_bars(bars);
                    empty
                });
            // Absent in a settings file written before ties existed, which loads as untied.
            if let Some(tied) = settings.sequence_tied.as_ref() {
                let length = pattern.len();
                for (step, tied) in tied.iter().take(length).enumerate() {
                    pattern.set_tied(step, *tied);
                }
            }
            pattern
        };

        Self {
            parameter_tab: String::new(),
            engine,
            backend,
            notifier: None,
            cli: None,
            search_paths,
            midi_ports,
            // Whatever the settings say, or the desktop when they say nothing. A headless run
            // never applies it — there is no context to apply it to — but the CLI can still set
            // it, and a test can read it back.
            theme: settings
                .theme
                .as_deref()
                .map(mxm_ui::theme::from_name)
                .unwrap_or(egui::ThemePreference::System),
            settings,
            settings_path,
            found: Vec::new(),
            cache: ScanCache::default(),
            sentinel,
            suspected,
            params: ParamSet::default(),
            editing: HashMap::new(),
            open_gestures: Vec::new(),
            open_gesture_edits: std::collections::HashMap::new(),
            gesture_began_parked: std::collections::HashSet::new(),
            requery_params: false,
            text_entry: HashMap::new(),
            octave,
            sustain: false,
            pitch_bend: 0.5,
            mod_wheel: 0.0,
            held: Vec::new(),
            midi_held: Vec::new(),
            dragging: None,
            had_focus: true,
            audio_devices,
            midi_inputs,
            midi_outputs,
            control_map: ControlMap::load(control_map_path),
            published_mask: None,
            control_map_problems: Vec::new(),
            param_columns: 1,
            sequencer: SequencerState {
                tempo,
                pattern: saved_pattern,
                ..SequencerState::default()
            },
            sequencer_serial: 1,
            sequencer_published: 0,
            selected_step: None,
            sequence_patch: HashMap::new(),
            pending_fx_locks: Vec::new(),
            modulation: HashMap::new(),
            pending_step_edit: HashMap::new(),
            also_selected: std::collections::BTreeSet::new(),
            selected_bar: 0,
            loop_scope: LoopScope::All,
            tools_on_pattern: false,
            bar_clipboard: None,
            parked_bases: std::collections::HashSet::new(),
            parked_locks: saved_locks,
            step_text: HashMap::new(),
            step_text_for: None,
            locks_unsaved: false,
            last_lock_save: std::time::Instant::now(),
            // Seeded from the settings so two launches do not open on the same "random" pattern.
            random_seed,
            pending_clipboard_text: None,
            sequence_dir: sequence_dir.clone(),
            exports_dir: exports_dir.clone(),
            sequence_name: "sequence".to_owned(),
            sequence_problems: Vec::new(),
            normalise_export: true,
            status: None,
            log_lines: Vec::new(),
            latency: None,
            last_screen_size: None,
            persisted_size: settings_window_size,
            collapse_applied: None,
            status_height: STATUS_HEIGHT_FLOOR,
            startup_scan_done: false,
            open_editor_pending: false,
            host_window: None,
            editor_refusal: None,
        }
    }

    /// Window geometry is part of the full P4 persistence, written only when it changes so the
    /// eager settings file is not rewritten every frame.
    /// The editor toggle, and the two collapse toggles, in the status bar.
    ///
    /// They live here because the status bar is the one strip that never collapses — a control for
    /// un-collapsing has to survive being collapsed.
    /// The plugin picker, in the status bar so it survives the settings panel being collapsed.
    ///
    /// **A menu of what can be loaded, not a list of what was found.** The scan reports every CLAP
    /// on the machine, and on a working audio machine most of them are effects the player refuses —
    /// five of six here, which buried the one instrument it could actually host.
    ///
    /// **Refusals do not disappear, they move.** `src/envelope.rs`'s contract is that everything
    /// outside the v1 envelope is refused *with the reason shown*, because that is what keeps
    /// third-party readiness honest without third-party plugins to test against; a refusal without a
    /// reason is a bug. So the unloadable ones sit below a separator, disabled, each carrying its own
    /// reason — out of the way of the choice, still answering "why can I not load this?".
    fn plugin_picker(&mut self, ui: &mut egui::Ui) {
        let duplicates = crate::discovery::duplicated_ids(&self.found);
        let loadable: Vec<_> = self.found.iter().filter(|f| f.is_supported()).collect();
        let refused: Vec<_> = self.found.iter().filter(|f| !f.is_supported()).collect();

        // The button says what is loaded, so the bar answers "which plugin is this?" at a glance.
        // The engine knows only the CLAP id, so the display name comes from the scan that offered
        // it — and falls back to the id if a rescan has since lost sight of the bundle.
        let loaded = self.engine.plugin_id().map(str::to_owned);
        let label = match &loaded {
            Some(id) => self
                .found
                .iter()
                .find(|f| &f.id == id)
                .map_or_else(|| id.clone(), |f| f.name.clone()),
            None if loadable.is_empty() => "No plugin".to_owned(),
            None => "Load plugin".to_owned(),
        };

        let mut to_load = None;
        let mut rescan = false;

        ui.menu_button(format!("{label} ⏷"), |ui| {
            if loadable.is_empty() {
                ui.weak("Nothing here can be loaded.");
            }
            for found in &loadable {
                // Two bundles with one id are two builds of the same plugin, and one is usually
                // stale. The location is the only thing that tells them apart, so it is shown for
                // exactly that case rather than for every row.
                let text = if duplicates.contains(&found.id) {
                    format!("{}  ·  {}", found.name, found.location())
                } else {
                    found.name.clone()
                };
                if ui.button(text).on_hover_text(found.hover()).clicked() {
                    to_load = Some((found.bundle.clone(), found.id.clone()));
                    ui.close();
                }
            }

            ui.separator();
            if ui.button("Rescan").clicked() {
                rescan = true;
                ui.close();
            }

            if !refused.is_empty() {
                ui.separator();
                ui.weak(format!("{} cannot be loaded", refused.len()));
                for found in &refused {
                    let row = ui.add_enabled(false, egui::Button::new(&found.name));
                    if let Some(reason) = found.refusal_reason() {
                        row.on_disabled_hover_text(format!("{}
{reason}", found.location()));
                    }
                }
            }
        })
        .response
        .on_hover_text(
            "Load a plugin. Here rather than in the settings panel so it is reachable with that              panel collapsed, which is how the player is used beside a plugin's own interface.",
        );

        if rescan {
            self.rescan();
        }
        if let Some((bundle, id)) = to_load {
            self.load(bundle, id);
        }
    }

    fn editor_and_collapse_controls(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Right-to-left, so this is drawn after the collapse toggles and lands to their left.
            // Ordering matters here: the two toggles are muscle memory at the far right and must
            // not move because a plugin with a longer name was loaded.

            // Collapse the settings panel and the parameter panel, independently. There is
            // deliberately no "collapse everything": a single sweep that took the keyboard with it
            // is what the player's brief has refused twice.
            let mut settings_collapsed = self.settings.settings_collapsed;
            if ui
                .add(toggle_button(ui, "Settings", !settings_collapsed))
                .on_hover_text(
                    "Show the plugin browser, audio device, MIDI and state panel. Collapsing it                      makes room for a plugin's own interface beside the player.",
                )
                .clicked()
            {
                settings_collapsed = !settings_collapsed;
                self.settings.settings_collapsed = settings_collapsed;
                self.persist();
            }

            let mut parameters_collapsed = self.settings.parameters_collapsed;
            if ui
                .add(toggle_button(ui, "Parameters", !parameters_collapsed))
                .on_hover_text(
                    "Show the parameter list. Collapsing it shrinks the player to the transport                      and keyboard, which never collapse.",
                )
                .clicked()
            {
                parameters_collapsed = !parameters_collapsed;
                self.settings.parameters_collapsed = parameters_collapsed;
                self.persist();
            }

            // Design system §10: Dark, Light and System, switching immediately and persisted as
            // interface state. It sits to the *left* of the two toggles — drawn after them in a
            // right-to-left layout — because those two are muscle memory at the far right and a
            // rarely-used control must not push them along.
            //
            // `mxm_ui`'s control, the one the eight editors draw, so the host and the instruments
            // offer the same thing in the same shape. The persisting variant beside it is theirs:
            // an editor writes the collection's shared file, and the player's choice belongs in
            // the player's own settings, next to everything else it remembers.
            if let Some(chosen) = mxm_ui::shell::theme_control(ui) {
                self.choose_theme(ui.ctx(), chosen);
            }

            ui.separator();
            self.editor_control(ui);

            ui.separator();
            self.plugin_picker(ui);
        });
    }

    /// Switches theme now and remembers it.
    ///
    /// The context is passed in because the CLI has none: `theme dark` over the socket sets the
    /// field, and [`Self::ui`] carries it into the context on the next frame.
    fn choose_theme(&mut self, ctx: &egui::Context, preference: egui::ThemePreference) {
        self.theme = preference;
        self.settings.theme = Some(theme_name(preference).to_owned());
        ctx.set_theme(preference);
        self.persist();
    }

    /// "Show editor" — opens the plugin's own interface in a floating window it owns.
    fn editor_control(&mut self, ui: &mut egui::Ui) {
        // **The source's, and only the source's.** This button sits beside the instrument's name
        // and is about the instrument; reading the engine's one editor state made it say *Hide
        // editor* because an effect's window was open, and then hide the effect's (the owner,
        // 2026-09-04). An effect's window is its own strip's business.
        let visible = self.engine.editor_state().visible;

        let label = if visible {
            "Hide editor"
        } else {
            "Show editor"
        };
        if ui
            .button(label)
            .on_hover_text(
                "The instrument's own interface, in its own window. The parameter list stays                  available either way, and an effect's editor is opened from its own strip.",
            )
            .clicked()
        {
            if visible {
                // Hidden, not destroyed: showing it again is instant, and the plugin keeps
                // whatever transient state its interface holds.
                self.engine.hide_editor();
            } else {
                let ctx = ui.ctx().clone();
                self.show_editor_now(&ctx);
            }

            // **Without this the player can be left showing a black window.** eframe repaints
            // reactively, and showing or hiding a foreign top-level window invalidates ours without
            // generating any input for egui to react to. So nothing asks for a frame, and Windows
            // shows the bare background.
            ui.ctx().request_repaint();
        }

        // Refusals and unowned windows are shown, never only logged. `src/envelope.rs` sets the
        // contract: a refusal without a reason is a bug. An editor that sinks behind the player
        // looks like one too, unless the user is told it is a known gap.
        if let Some(why) = &self.editor_refusal {
            ui.colored_label(adapter::tokens_for(ui).warning, "editor unavailable")
                .on_hover_text(why.message());
        } else if let Some(why) = self.engine.editor_state().owned.reason() {
            ui.colored_label(adapter::tokens_for(ui).warning, "not in front")
                .on_hover_text(why);
        }
    }

    /// Opens the plugin's editor, takes keyboard focus back, and repaints.
    ///
    /// Shared by the "Show editor" button and the automatic open on load, so the three things that
    /// have to happen together cannot drift apart.
    fn show_editor_now(&mut self, ctx: &egui::Context) {
        let state = self.engine.editor_state();
        if state.visible {
            return;
        }

        if state.open {
            self.engine.show_editor();
        } else {
            match self.engine.open_editor(self.host_window) {
                Ok(_) => self.editor_refusal = None,
                Err(why) => {
                    // A plugin with no interface of its own is the ordinary case, not a failure to
                    // report loudly: the parameter panel is how it is played. So the reason is
                    // recorded for the control's tooltip and nothing is announced.
                    self.editor_refusal = Some(why);
                    return;
                }
            }
        }

        // **Take keyboard focus back.** Showing the editor hands it focus, after which the
        // computer keyboard plays nothing — the keystrokes go to the plugin — and `handle_input`
        // releases every held note. The editor stays on screen regardless: the player owns that
        // window, so an owned window sits above its owner rather than behind it.
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.repaint_after_editor_change(ctx);
    }

    /// Repaints the player after an editor appeared or disappeared, and keeps doing so briefly.
    ///
    /// **Without this the player is left showing a black window.** eframe repaints *reactively*,
    /// and a foreign top-level window appearing or vanishing invalidates ours without producing any
    /// input for egui to react to. Nothing asks for a frame, so Windows shows the bare background.
    ///
    /// One repaint proved not to be enough: the editor's teardown is not finished when the request
    /// is made. So this asks for a frame now and a few more over the next moment, which is long
    /// enough to settle and short enough not to be a spin.
    ///
    /// This is **not** the graphics-context conflict that used to compound it. That was two OpenGL
    /// renderers in one process fighting over a global current-context, and it is gone: the player
    /// renders through wgpu on D3D12, Vulkan or Metal. What remains is the plain reactive-repaint
    /// problem above, which would exist with any backend.
    fn repaint_after_editor_change(&self, ctx: &egui::Context) {
        ctx.request_repaint();
        for delay in [60, 160, 320] {
            ctx.request_repaint_after(std::time::Duration::from_millis(delay));
        }
    }

    /// What the window shrinks to when everything collapsible is collapsed.
    ///
    /// **The sum of the panels that never collapse, and nothing else.** `SEQUENCER_HEIGHT` and
    /// `KEYBOARD_HEIGHT` are exact panel sizes and the status bar's height is measured, so the sum
    /// is exact — anything added to it becomes visible dead space between the status bar and the
    /// transport, because the empty `CentralPanel` drawn while collapsed absorbs the slack.
    ///
    /// **This was wrong by 41 px, and both halves of the error were defensive.** A `+ 24.0` margin
    /// guarded against "rounding clipping a row", and `status_height.max(STATUS_HEIGHT_FLOOR)`
    /// guarded against a bad status-bar guess. But the status bar measures **23 px** and the floor
    /// is 40, so the `max` inflated every calculation by 17 whether or not the measurement was
    /// good. 17 + 24 = 41, which is exactly the slack measured in the running player.
    ///
    /// The floor is still the value `status_height` starts at, so the first frame has something
    /// sane before anything has been measured. It is not a floor applied to a real measurement.
    /// `pub` for the regression test in `t8_editor_ui.rs`, which cannot see this any other way: the
    /// fix is a **window size**, the harness does not honour viewport commands, and every structural
    /// assertion passes against the defect because AccessKit reports clipped widgets as present.
    pub fn collapsed_height(&self) -> f32 {
        let never_collapses = self.status_height + SEQUENCER_HEIGHT + KEYBOARD_HEIGHT;

        // **The left settings panel is collapsible independently, so "collapsed" is not one state.**
        // Sizing the window as though everything were collapsed while that panel is still open
        // squeezes it into whatever is left between the status bar and the sequencer — a clipped
        // strip of the audio section that reads as dead space, which is exactly what collapsing was
        // supposed to remove.
        if self.settings.settings_collapsed {
            never_collapses
        } else {
            never_collapses + SETTINGS_MIN_HEIGHT
        }
    }

    /// Applies the window size the collapse state calls for, and only when it changes.
    ///
    /// Collapsing exists to make room for a plugin's editor beside the player, so it has to shrink
    /// the *window* — leaving the space empty would be no room at all. Two things make that safe
    /// rather than another instance of the interface moving on its own:
    ///
    /// - It runs only when [`Self::collapse_applied`] disagrees with the current state, which
    ///   changes only when the user operates a collapse control.
    /// - The **expanded** size is what `remember_geometry` persists. Collapsing does not overwrite
    ///   it, so expanding returns to the size the user chose rather than a default.
    fn apply_collapse_geometry(&mut self, ctx: &egui::Context) {
        // Both toggles, because both change the height the window needs. Keying on the parameter
        // panel alone meant collapsing *it* sized the window for a settings panel that was still
        // open, and toggling the settings panel afterwards resized nothing at all.
        let state = (
            self.settings.parameters_collapsed,
            self.settings.settings_collapsed,
        );
        if self.collapse_applied == Some(state) {
            return;
        }
        self.collapse_applied = Some(state);
        let collapsed = self.settings.parameters_collapsed;

        let Some((width, _)) = self
            .persisted_size
            .or(self.settings.window_size)
            .or(Some((REFERENCE_SIZE[0], REFERENCE_SIZE[1])))
        else {
            return;
        };

        let height = if collapsed {
            self.collapsed_height()
        } else {
            self.settings
                .window_size
                .map(|(_, h)| h)
                .unwrap_or(REFERENCE_SIZE[1])
        };

        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
            MIN_SIZE.0,
            if collapsed {
                self.collapsed_height()
            } else {
                MIN_SIZE.1
            },
        )));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(width, height)));
    }

    fn remember_geometry(&mut self, ctx: &egui::Context) {
        let (size, maximized, fullscreen) = ctx.input(|i| {
            let viewport = i.viewport();
            (
                viewport.inner_rect.map(|r| r.size()),
                viewport.maximized,
                viewport.fullscreen,
            )
        });
        let Some(size) = size else {
            return;
        };
        if !geometry_worth_remembering(self.settings.parameters_collapsed, maximized, fullscreen) {
            return;
        }
        let current = (size.x, size.y);
        let moved = self
            .persisted_size
            .is_none_or(|(w, h)| (w - current.0).abs() > 1.0 || (h - current.1).abs() > 1.0);
        if moved {
            self.persisted_size = Some(current);
            self.settings.window_size = Some(current);
            self.persist();
        }
    }
}

/// Whether the window's current size is one the user chose, and so worth restoring next launch.
///
/// **Never while collapsed.** The collapsed height is a consequence of the collapse, not a size
/// the user chose, and persisting it would restore a small window next launch and then expand its
/// contents into it.
///
/// **Never while maximised or fullscreen.** Those are states, not sizes: the window is as wide as
/// the screen because the OS made it so, and remembering that width as if it had been dragged to
/// brought it back on the next start as a 1922-point-wide window with its borders on — the owner's
/// report, 2026-09-04 — because the state is not restored, only the number. `None` from the
/// backend counts as not maximised; a backend that cannot say is the normal case in the harness.
pub(crate) fn geometry_worth_remembering(
    collapsed: bool,
    maximized: Option<bool>,
    fullscreen: Option<bool>,
) -> bool {
    !collapsed && maximized != Some(true) && fullscreen != Some(true)
}

impl PlayerApp {
    /// Everything the app does each turn that is not drawing and needs no window.
    ///
    /// The GUI calls this from `eframe::App::logic`; a headless session calls it directly. There
    /// is deliberately **one** implementation: two would drift until a session test passed on a
    /// path no user takes.
    pub fn service(&mut self) {
        self.answer_cli();
        self.engine.poll();
        self.receive_midi_presses();
        self.publish_claimed_ccs();
        self.publish_sequencer();
        self.apply_plugin_output();
        self.close_idle_control_gestures();
        self.drain_logs();

        // A plugin asking to be processed while nothing is running can only be honoured here:
        // whoever owns the app owns the backend.
        if self.engine.take_idle_process_request() {
            match self.engine.start(self.backend.as_ref()) {
                Ok(()) => {
                    self.published_mask = None;
                    self.sequencer_published = 0;
                }
                Err(reason) => self.status = Some(reason),
            }
        }

        // A stream that died out from under a running transport: the worker stepping the
        // sequencer is gone, so the transport stops — what a rescan does — and any control gesture
        // it was mid-way through is closed. Said in the status line, because a Play button reading
        // "Stop" over silence is the dishonesty this exists to remove.
        if self.engine.take_stream_exit_notice() {
            self.close_control_gestures();
            self.quiet_the_sequencer();
            self.status = Some(match self.engine.state() {
                EngineState::StreamExited(reason) => format!("audio device stopped: {reason}"),
                _ => "audio device stopped".to_owned(),
            });
        }

        // The engine keeps the schedule; the app owns the backend, so the attempt happens here.
        // A fresh worker knows nothing, exactly as after a load: the claimed-CC mask and the
        // sequencer state are published again on the next frame.
        if let Some(Ok(attempts)) = self.engine.try_reconnect(self.backend.as_ref()) {
            self.published_mask = None;
            self.sequencer_published = 0;
            self.status = Some(format!("audio device reconnected on attempt {attempts}"));
        }

        if self.engine.latency_changed() || self.latency.is_none() {
            self.latency = self.engine.latency_samples();
        }
    }

    /// Re-reads the available MIDI ports from wherever this app was configured to get them.
    pub fn refresh_midi_ports(&mut self) {
        self.midi_inputs = self.midi_ports.inputs();
        self.midi_outputs = self.midi_ports.outputs();
    }

    /// Requeries every parameter's value and formatted text from the plugin.
    ///
    /// The panel does this a frame after a gesture ends; a headless caller has no frames, so it
    /// asks directly. Without it a state dump reports the values from when the plugin was loaded,
    /// which reads as though nothing was ever changed.
    pub fn refresh_params(&mut self) {
        let mut params = std::mem::take(&mut self.params);
        self.engine.refresh_param_values(&mut params);
        self.params = params;
        // **What the panel sent wins over what the readback still says — text included.** An
        // edit reaches the plugin through the audio thread; until it lands, the readback is the
        // pre-edit value. `editing` has always given the *value* that precedence when the panel
        // draws, but a refresh overwrote the *text* with the stale reading — and with nothing to
        // trigger another requery, the panel froze on it. Found as a once-in-several-runs test
        // flake that turned out to be this, not the test.
        let editing: Vec<(u32, f64)> = self.editing.iter().map(|(k, v)| (*k, *v)).collect();
        for (param_id, value) in editing {
            Self::note_edit(&mut self.engine, &mut self.params, param_id, value);
        }
        // **After the editing override, because the patch correction outranks it.** Here, because
        // this is the one place a refresh happens: every other path routes through it, so
        // correcting the sequenced parameters anywhere else would be correcting some of them some
        // of the time.
        self.show_patch_for_sequenced();
    }

    /// Everything the window shows, as data. See [`crate::state`].
    pub fn state(&mut self) -> PlayerState {
        let meters = self.engine.meters.clone();
        let plugin = self.engine.plugin_id().map(str::to_owned).map(|id| {
            let envelope = self.engine.envelope().cloned();
            PluginState {
                id,
                bundle: self
                    .engine
                    .bundle()
                    .map(|b| b.display().to_string())
                    .unwrap_or_default(),
                channels: envelope
                    .as_ref()
                    .map(|e| e.audio.channel_count)
                    .unwrap_or(0),
                selection: envelope
                    .as_ref()
                    .map(|e| e.selection.clone())
                    .unwrap_or_default(),
                note_input: envelope
                    .as_ref()
                    .and_then(|e| e.note_input.map(|p| p.dialect.label().to_owned())),
                note_output: envelope
                    .as_ref()
                    .and_then(|e| e.note_output.map(|p| p.dialect.label().to_owned())),
                carries_voice_ids: envelope
                    .as_ref()
                    .map(|e| e.note_input_carries_voice_ids())
                    .unwrap_or(false),
                latency_samples: self.latency,
                params: self
                    .params
                    .params
                    .iter()
                    .map(|p| ParamState {
                        id: p.id,
                        name: p.name.clone(),
                        module: p.module.clone(),
                        value: p.value,
                        text: p.text.clone(),
                        min: p.min,
                        max: p.max,
                        default: p.default,
                        modulation: self
                            .modulation
                            .get(&Self::source_key(p.id))
                            .copied()
                            .unwrap_or(0.0),
                    })
                    .collect(),
            }
        });

        let editors = self.engine.open_editor_targets();
        let fx = self
            .engine
            .fx_info()
            .into_iter()
            .enumerate()
            .map(|(index, info)| crate::state::FxState {
                id: info.plugin_id,
                bundle: info.bundle.display().to_string(),
                bypassed: info.bypassed,
                input_channels: info.input_channels,
                output_channels: info.output_channels,
                selection: info.selection,
                editor_open: editors.contains(&crate::engine::editor::EditorTarget::Fx(index)),
            })
            .collect();

        PlayerState {
            engine: format!("{:?}", self.engine.state()),
            plugin,
            fx,
            audio: AudioState {
                device: self.engine.audio_config().device_name.clone(),
                sample_rate: self.engine.audio_config().sample_rate,
                buffer_size: self.engine.audio_config().buffer_size,
                reconnect: self
                    .engine
                    .reconnect()
                    .map(|reconnect| crate::state::ReconnectState {
                        attempts: reconnect.attempts,
                        last_failure: reconnect.last_failure.clone(),
                    }),
            },
            midi: MidiState {
                available_inputs: self.midi_inputs.clone(),
                available_outputs: self.midi_outputs.clone(),
                connected_inputs: self.engine.connected_midi_inputs(),
                refused_inputs: self
                    .engine
                    .refused_midi_inputs()
                    .iter()
                    .map(|r| RefusedInputState {
                        port: r.port.clone(),
                        reason: r.reason.clone(),
                    })
                    .collect(),
                output: self.engine.midi_output_port().map(str::to_owned),
                output_faulted: self.engine.midi_output_faulted(),
                thru: self.engine.thru_enabled.load(Ordering::Acquire),
            },
            sequencer: {
                let (transport, step) = self.playhead();
                SequencerView {
                    transport: transport.label().to_owned(),
                    step,
                    playing_bar: self.playing_bar(),
                    tempo: self.sequencer.tempo,
                    bars: self.sequencer.pattern.bars(),
                    steps_per_bar: self.sequencer.pattern.steps_per_bar(),
                    selected_bar: self.selected_bar,
                    loop_scope: match self.loop_scope {
                        LoopScope::Bar => "bar",
                        LoopScope::Pattern => "pattern",
                        LoopScope::All => "all",
                    }
                    .to_owned(),
                    bars_with_notes: (0..self.sequencer.pattern.bars())
                        .map(|bar| {
                            let spb = self.sequencer.pattern.steps_per_bar();
                            (0..spb).any(|offset| {
                                !self.sequencer.pattern.step(bar * spb + offset).is_empty()
                            })
                        })
                        .collect(),
                    steps: crate::sequencer::pattern::PatternData::from(&self.sequencer.pattern).0,
                    tied: (0..self.sequencer.pattern.len())
                        .map(|step| self.sequencer.pattern.tied(step))
                        .collect(),
                    selected_step: self.selected_step,
                    selected_steps: self.selected_steps(),
                    locks: self.lock_views(),
                }
            },
            keyboard: KeyboardState {
                octave: self.octave,
                sustain: self.sustain,
                held: self.sounding(),
            },
            found: self
                .found
                .iter()
                .map(|f| FoundState {
                    id: f.id.clone(),
                    name: f.name.clone(),
                    vendor: f.vendor.clone(),
                    location: f.location(),
                    supported: f.is_supported(),
                    refusal: f.refusal_reason(),
                    effect: f.is_effect(),
                    effect_refusal: f.effect_refusal_reason(),
                })
                .collect(),
            status: self.status.clone(),
            log: self.log_lines.clone(),
            timing: TimingState {
                plugin_load: meters.plugin_load(),
                callback_load: meters.callback_load(),
                missed_deadlines: meters.missed_deadlines(),
                callbacks: meters.callbacks(),
                xruns: meters.xruns(),
                realtime_priority: meters.realtime_priority().label().to_owned(),
            },
        }
    }

    /// The engine, for a session that needs to drive it directly.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// The audio backend this app was configured with.
    pub fn backend(&self) -> &dyn Backend {
        self.backend.as_ref()
    }

    fn persist(&mut self) {
        // The sequencer rides along: a test pattern you have to rebuild on every launch is a
        // small thing that is annoying every single time.
        self.settings.tempo = Some(self.sequencer.tempo);
        self.settings.sequence_steps = Some(crate::sequencer::pattern::PatternData::from(
            &self.sequencer.pattern,
        ));
        self.settings.sequence_tied = Some(
            (0..self.sequencer.pattern.len())
                .map(|step| self.sequencer.pattern.tied(step))
                .collect(),
        );
        // **The shape, or a restart would re-bar the music.** The step list alone cannot say whether
        // forty-eight steps were three bars of sixteen or four of twelve.
        self.settings.sequence_bars = Some(self.sequencer.pattern.bars());
        self.settings.sequence_steps_per_bar = Some(self.sequencer.pattern.steps_per_bar());
        // Parked locks are written back **exactly as they were read**. Capturing the live set
        // instead would replace an unloaded instrument's sequencing with the empty set that stands
        // in for it, so merely opening the player with a different plugin would delete it.
        match self.capture_locks() {
            Ok(data) => self.settings.sequence_locks = Some(data),
            Err(reason) => {
                self.status = Some(reason);
                return; // Do not overwrite a good settings file with an unidentifiable target.
            }
        }
        self.settings.random_seed = Some(self.random_seed);
        // The chain, in order, with each effect's on/off: a restart reopens the rig it closed.
        self.settings.fx_chain = self
            .engine
            .fx_info()
            .into_iter()
            .map(|info| crate::settings::FxSetting {
                bundle: info.bundle,
                plugin_id: info.plugin_id,
                bypassed: info.bypassed,
            })
            .collect();
        // Whatever brought us here, the file is about to hold the current locks.
        self.locks_unsaved = false;
        self.last_lock_save = std::time::Instant::now();

        // Eager, because the process can die without warning, and crash-safe because eager
        // writing is exactly what makes a torn file likely.
        if let Err(reason) = self.settings.save(&self.settings_path) {
            self.status = Some(reason);
        }
    }

    /// Scanning executes arbitrary code, so it runs only with audio stopped — which is also what
    /// makes the sentinel meaningful, since no other plugin is then executing concurrently.
    pub fn rescan(&mut self) {
        if let Err(reason) = self.engine.stop_now() {
            self.status = Some(reason);
            return;
        }

        // A rescan can change which parameters exist, so no edit can be continued across it — and
        // neither can a deviation, which is why this stops through `quiet_the_sequencer` rather than
        // setting the transport itself.
        self.close_control_gestures();
        self.quiet_the_sequencer();

        self.found.clear();
        // Instrument maps are re-gathered with the instruments: an uninstalled plugin should not
        // leave its mapping behind, and a reinstalled one should pick up its current map.
        self.control_map.clear_instrument_maps();
        self.control_map_problems.clear();

        let roots = self.search_paths.clone().unwrap_or_else(search_paths);
        for bundle in find_bundles_in(&roots, &self.settings.quarantined) {
            let found = scan_bundle(&bundle, &self.sentinel);
            self.cache
                .record(&bundle, found.iter().map(|f| f.id.clone()).collect());
            self.found.extend(found);
            self.load_instrument_map_beside(&bundle);
        }
        // Loadable first, because that is the number that decides whether there is anything to do;
        // the total matters only when it is larger, and then the difference is the interesting part.
        let loadable = self.found.iter().filter(|f| f.is_supported()).count();
        self.status = Some(if loadable == self.found.len() {
            format!("{loadable} plugins found")
        } else {
            format!("{loadable} of {} plugins can be loaded", self.found.len())
        });

        // **The engine comes back if an instrument was playing through it.** The stop at the top
        // is right — the plugin list changes underneath a scan — but leaving it stopped left the
        // player at engine Idle with a loaded instrument and a Play button that produced nothing,
        // found live over the CLI. A load restarts the engine; a rescan that keeps the loaded
        // plugin must do the same.
        if self.engine.plugin_id().is_some()
            && let Err(reason) = self.engine.start(self.backend.as_ref())
        {
            self.status = Some(reason);
        }
    }

    /// Loads the control map that ships beside a bundle.
    ///
    /// An instrument owns its own mapping, because nobody is obliged to install the whole
    /// collection — see [`crate::control_map`]. A problem with one map is recorded and the scan
    /// continues: one broken file must not disable the controller for every other instrument.
    fn load_instrument_map_beside(&mut self, bundle: &std::path::Path) {
        let Some(dir) = bundle.parent() else {
            return;
        };
        for problem in self.control_map.load_instrument_maps_in(dir) {
            if !self.control_map_problems.contains(&problem) {
                self.control_map_problems.push(problem);
            }
        }
    }

    /// What the control map could not load. Shown in the UI; never fatal.
    pub fn control_map_problems(&self) -> &[String] {
        &self.control_map_problems
    }

    /// How many columns the parameter panel used last frame.
    pub fn param_columns(&self) -> usize {
        self.param_columns
    }

    pub fn control_map(&self) -> &ControlMap {
        &self.control_map
    }

    /// Re-reads the user's control-map overlay, keeping the working one if the new one is bad.
    pub fn reload_control_map(&mut self) {
        self.close_control_gestures();
        self.control_map.reload();
        match self.control_map.last_error() {
            Some(error) => self.status = Some(error.to_owned()),
            None => {
                self.status = Some(format!(
                    "Control map reloaded: {} pages, {} claimed CCs",
                    self.control_map.page_count(),
                    self.control_map.claimed().count()
                ))
            }
        }
    }

    /// Loads a plugin and starts audio. Reachable without a frame, so a session can call it.
    pub fn load(&mut self, bundle: PathBuf, plugin_id: String) {
        // Whatever was being edited belonged to the previous plugin.
        self.close_control_gestures();

        // **The live locks are parked under the instrument they were made for, before it goes.**
        // Parameter ids are only meaningful beside a `CLAP_ID`, and after the engine is replaced
        // there is no way to recover which instrument these belonged to. Without this, a set made
        // for one plugin whose ids happen to exist in the next survives every check — the ids are
        // there and the flags are right — and is then persisted under the new instrument's name.
        // Parking makes the identity comparison in `claim_parked_locks` the thing that decides.
        // **Unconditionally**, whether or not anything is locked: these are keyed by the outgoing
        // instrument's parameter numbers, and a value left behind becomes a baseline for whatever
        // happens to share a number in the next one. `pending_step_edit` goes with them - a drag
        // that never finished belongs to an instrument that no longer exists.
        self.modulation.clear();
        // Parked bases are dropped, not restored: the patch record is about to go too, and the new
        // plugin's state will define every value. Restoring into the outgoing instrument would be a
        // write to something being torn down.
        self.parked_bases.clear();
        self.sequence_patch.clear();
        self.pending_step_edit.clear();
        if !self.sequencer.locks.is_empty() || !self.pending_fx_locks.is_empty() {
            match self.capture_locks() {
                Ok(data) => self.parked_locks = Some(data),
                Err(reason) => {
                    self.status = Some(reason);
                    return;
                }
            }
            self.sequencer.locks.clear_all();
            self.pending_fx_locks.clear();
        }
        // A map may sit beside a bundle that was never scanned - a session loads directly.
        self.load_instrument_map_beside(&bundle);

        match self.engine.load(&bundle, &plugin_id) {
            Ok(()) => {
                self.params = self.engine.read_params();
                self.editing.clear();
                self.open_gestures.clear();
                // Every parameter just took its loaded value, so no knob is where its parameter
                // is, and the mask must be published again to a worker that has never seen it.
                self.control_map.rearm_all();
                self.published_mask = None;
                // A fresh worker knows nothing. Stop rather than transfer position: the sound has
                // already been interrupted completely, and finding the sequencer back at step 1
                // after loading a different plugin is what a person expects.
                self.quiet_the_sequencer();
                self.sequencer_published = 0;
                if let Some(notifier) = self.notifier.as_mut() {
                    notifier.retarget_state(self.engine.shared().clone());
                }

                self.settings.last_bundle = Some(bundle);
                self.settings.last_plugin_id = Some(plugin_id);
                // Before `persist`, so the file is written with the locks now in force rather than
                // with the parked copy it is about to stop being.
                self.claim_parked_locks();

                // **Whatever is in force is now checked against the instrument that just loaded**,
                // claimed or not. A sequence loaded with no plugin was validated against an empty
                // parameter set — which validates nothing — so without this its locks would be
                // published to whatever loaded next, including parameters that instrument does not
                // have or will not accept modulation for.
                self.sync_baselines();
                let problems = self.prune_unknown_locks();
                if let Some(first) = problems.first() {
                    self.status = Some(match problems.len() {
                        1 => first.clone(),
                        n => format!("{first} (and {} more)", n - 1),
                    });
                }
                self.persist();

                if let Err(reason) = self.engine.start(self.backend.as_ref()) {
                    self.status = Some(reason);
                }

                // After the engine is running, so the pushed values have somewhere to go.
                self.settle_bases_to_patch();

                // Choosing a plugin is asking to use it, so its own interface opens with it rather
                // than waiting for a second click. Deferred to the next frame for two reasons:
                // there is no `Context` here to take focus back or force a repaint with, and
                // opening after the load has settled keeps the two operations from interleaving.
                //
                // Only when there is a window. A headless run — every session test, and the
                // offline renderer — must never create one, and the notifier is exactly the flag
                // that distinguishes them: `PlayerApp::new` sets it, `with_config` does not.
                self.open_editor_pending = self.notifier.is_some();
            }
            Err(reason) => self.status = Some(format!("could not load: {reason}")),
        }
    }

    // --- the effect chain ------------------------------------------------------------------------
    //
    // Four acts, and no parameters: an effect is added, removed, moved, or switched off and on
    // (the owner's ruling, 2026-09-04). Its parameters are its own editor's business. The first
    // three change the graph's topology, so they go through the engine's stop-and-return protocol
    // and restart audio here, exactly as `load` does for the source; the source, its locks, its
    // patch and its gestures are untouched by any of them.

    /// Adds an effect at the end of the chain and restarts audio. The chain's index is returned.
    pub fn add_fx(&mut self, bundle: PathBuf, plugin_id: String) -> Result<usize, String> {
        let index = self.engine.add_fx(&bundle, &plugin_id)?;
        self.resolve_pending_fx_locks();
        self.restart_after_chain_change();
        Ok(index)
    }

    pub fn remove_fx(&mut self, index: usize) -> Result<(), String> {
        // **The automation goes with the effect, and it goes first.**
        //
        // The owner's requirement, and it is a safety property rather than tidiness: a lock names
        // its effect by id, and an id is never reused — but a lock left behind would still be a
        // lock the runtime walks and emits an action for at every step boundary, for an effect that
        // is not there. The processor drops those, so nothing would be *delivered* wrongly; what
        // would happen is quieter and worse to debug — a pattern that carries invisible automation,
        // counts against both lock budgets, and reappears the moment an effect is added.
        //
        // **Before the chain changes**, so no state naming an absent effect is ever published.
        let id = self.engine.fx_info().get(index).map(|info| info.id);
        if let Some(id) = id {
            // Do not delete work if the processor cannot be returned. Once stopped, remove
            // automation before changing topology, with no intervening audio publication.
            self.engine.stop_now()?;
            self.sequence_patch.retain(|key, _| key.fx != id);
            self.modulation.retain(|key, _| key.fx != id);
            if self.sequencer.locks.clear_fx(id) {
                self.sequencer_changed();
                self.locks_unsaved = true;
            }
        }
        self.engine.remove_fx(index)?;
        self.restart_after_chain_change();
        Ok(())
    }

    /// Rearranges the chain: the effect at `from` ends up at `to`.
    ///
    /// **The automation does not move, and that is the point of ids.** A lock names the effect, not
    /// the position, so a reorder is invisible to it. A position-keyed design would swap two
    /// effects' automation here without touching a single lock.
    pub fn move_fx(&mut self, from: usize, to: usize) -> Result<(), String> {
        self.engine.move_fx(from, to)?;
        self.restart_after_chain_change();
        Ok(())
    }

    /// Off means not called at all. No restart: the flag is read on the audio thread per chunk.
    pub fn set_fx_bypassed(&mut self, index: usize, bypassed: bool) -> Result<(), String> {
        self.engine.set_fx_bypassed(index, bypassed)?;
        self.persist();
        Ok(())
    }

    /// Opens one effect's own interface. One editor at a time: this closes any other.
    pub fn show_fx_editor(&mut self, index: usize) -> Result<(), String> {
        // **A toggle, like the instrument's own button.** The strips can each have a window open
        // at once now, so the button that opened one has to be the button that puts it away.
        let target = crate::engine::editor::EditorTarget::Fx(index);
        if self.engine.editor_state_for(target).visible {
            // Hidden, not destroyed: showing it again is instant, and the plugin keeps whatever
            // transient state its interface holds.
            self.engine.hide_editor_for(target);
            return Ok(());
        }
        match self.engine.open_fx_editor(index, self.host_window) {
            Ok(_) => {
                self.editor_refusal = None;
                Ok(())
            }
            Err(why) => {
                self.editor_refusal = Some(why.clone());
                Err(why.message().to_owned())
            }
        }
    }

    /// What every topology change does after the engine has stopped and edited the chain.
    ///
    /// A fresh worker knows nothing, so the claimed-CC mask and the sequencer state are published
    /// again; the transport stops, as it does on a source load, because the sound was interrupted
    /// completely and a restart mid-bar would resume into silence that was never heard.
    fn restart_after_chain_change(&mut self) {
        self.quiet_the_sequencer();
        self.published_mask = None;
        self.sequencer_published = 0;
        self.persist();
        if self.engine.plugin_id().is_some()
            && let Err(reason) = self.engine.start(self.backend.as_ref())
        {
            self.status = Some(reason);
        }
    }

    /// Reloads the chain the settings remember, after the source has loaded at startup.
    ///
    /// An effect that cannot be found or loaded is reported and skipped rather than failing the
    /// rest: a bundle moved since the last run is the ordinary reason, and the others still load.
    pub fn restore_fx_chain(&mut self) {
        let remembered = std::mem::take(&mut self.settings.fx_chain);
        if remembered.is_empty() {
            return;
        }
        let mut problems = Vec::new();
        for setting in &remembered {
            match self.engine.add_fx(&setting.bundle, &setting.plugin_id) {
                Ok(index) => {
                    let _ = self.engine.set_fx_bypassed(index, setting.bypassed);
                }
                Err(reason) => problems.push(format!("{}: {reason}", setting.plugin_id)),
            }
        }
        if let Some(first) = problems.first() {
            self.status = Some(format!(
                "could not restore an effect — {first}{}",
                if problems.len() > 1 {
                    format!(" (and {} more)", problems.len() - 1)
                } else {
                    String::new()
                }
            ));
        }
        self.restart_after_chain_change();
    }

    /// The whole input path from the window: computer keys, focus loss, and the pointer.
    fn handle_input(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.focused);

        // egui clears its `keys_down` on `WindowFocused(false)` *because winit may never deliver
        // the key-up*. That clearing is egui's own state and emits nothing we would turn into a
        // note-off, so alt-tabbing with a key held would leave the note sounding. This is a
        // panic, not a pause — but a *targeted* one: we know exactly which presses are ours.
        if self.had_focus && !focused {
            self.engine.push_gui_event(Payload::CleanupSource);
            self.held.clear();
            self.dragging = None;
        }
        self.had_focus = focused;

        if !focused {
            return;
        }

        // **Nothing on the computer keyboard is an instrument while a text field has focus.**
        // Not the transport, and not the note keys — typing "bass" would otherwise play four
        // notes and typing a space would start the sequencer.
        let typing = ctx.memory(|m| m.focused().is_some());

        // Anything already sounding when focus moves into a field has to be released, or the note
        // stays on: its key-up will be swallowed by the guard below.
        if typing && !self.held.is_empty() {
            self.engine.push_gui_event(Payload::CleanupSource);
            self.held.clear();
            self.dragging = None;
        }

        // **Escape stops editing a step.** It has to exist: clicking the selected step ties it
        // rather than deselecting, so without this there would be no way to put the keyboard back
        // to only playing except by clicking a different step and untying that one.
        //
        // Guarded by `typing` like everything else here — Escape belongs to the text field while
        // one has focus, and stealing it would break leaving a half-typed sequence name.
        if !typing && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            self.perform(Gesture::EscapeKey);
        }

        // Space is the transport, and it is **the same call** the button makes rather than a second
        // copy of the same decision. They used to differ: the button resumed in place while Space
        // rewound, which is the disagreement that removing *From start* settled.
        if !typing && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Space)) {
            self.perform(Gesture::SpaceKey);
        }

        // **Ctrl+C/X/V on the sequencer.** With steps selected they act on the selection; with
        // none they act on the shown bar or its pattern, following the same toggle the buttons
        // follow. Paste applies whatever was copied. Behind the `typing` guard with everything
        // else: while a text field has focus these belong to the text.
        //
        // **The chord never arrives as a key press, and that is what made these dead.**
        // `egui-winit` recognises Ctrl+C/X/V itself and pushes `Event::Copy`, `Event::Cut` or
        // `Event::Paste`, returning *before* it emits any `Event::Key` — so a
        // `consume_key(COMMAND, Key::C)` waits for something no keyboard produces. Reading the
        // events is not merely the workaround: it is the only spelling that is right, and it also
        // picks up the platform's other clipboard chords (Ctrl+Insert, Shift+Insert,
        // Shift+Delete) which egui maps onto the same three events.
        //
        // Removed from the queue rather than merely read, so nothing downstream sees a clipboard
        // event this already acted on.
        if !typing {
            let (mut copy, mut cut, mut paste) = (false, false, false);
            ctx.input_mut(|i| {
                i.events.retain(|event| match event {
                    egui::Event::Copy => {
                        copy = true;
                        false
                    }
                    egui::Event::Cut => {
                        cut = true;
                        false
                    }
                    // The text is the *system* clipboard's, which is never what a sequencer
                    // pastes: the bar clipboard holds notes, ties and locks, and no text form
                    // could carry a lock's parameter identity. The event is the chord arriving.
                    egui::Event::Paste(_) => {
                        paste = true;
                        false
                    }
                    _ => true,
                });
            });
            if copy {
                self.perform(Gesture::CopyShortcut);
            }
            if cut {
                self.perform(Gesture::CutShortcut);
            }
            if paste {
                self.perform(Gesture::PasteShortcut);
            }
        }

        if typing {
            return;
        }

        let events = ctx.input(|i| i.events.clone());
        for event in events {
            let egui::Event::Key {
                key,
                pressed,
                repeat,
                ..
            } = event
            else {
                continue;
            };

            match keyboard::translate(key, pressed, repeat, self.octave) {
                Some(keyboard::KeyAction::NoteOn(note)) => {
                    self.perform(Gesture::PlayNote { key: note });
                }
                Some(keyboard::KeyAction::NoteOff(note)) => {
                    self.perform(Gesture::ReleaseNote { key: note });
                }
                Some(keyboard::KeyAction::OctaveDown) => {
                    self.perform(Gesture::OctaveShift { by: -1 });
                }
                Some(keyboard::KeyAction::OctaveUp) => {
                    self.perform(Gesture::OctaveShift { by: 1 });
                }
                None => {}
            }
        }
    }

    /// Drops locks on parameters the loaded plugin does not have, reporting each one.
    ///
    /// **Matching the plugin id is not enough.** A synth that gains and loses parameters between
    /// versions keeps its `CLAP_ID`, so a sequence recorded against last month's build can name a
    /// parameter this one no longer has. Sending events for it would be automation aimed at nothing;
    /// keeping it would mean a lock that never fires and never explains itself. So it is dropped by
    /// name, and said out loud — which is what failing gracefully against a different synth means in
    /// practice, since a *different* plugin has already been refused wholesale by then.
    ///
    /// Silent when no plugin is loaded: there is nothing to check against, and an empty parameter
    /// set is not evidence that a parameter is gone.
    fn prune_unknown_locks(&mut self) -> Vec<String> {
        // **Anything about to be dropped can no longer be zeroed by the sequencer**, so it is zeroed
        // here — including a parameter the instrument *has* but will not accept modulation for,
        // which an earlier version left applied while removing the lock that would have cleared it.
        let mut problems = Vec::new();
        for param_id in self.sequencer.locks.param_ids() {
            if param_id.is_source() && self.engine.plugin_id().is_none() {
                continue;
            }
            let refusal = match self.lock_parameter(param_id) {
                None => Some(format!(
                    "parameter {} is not in this instrument; what the steps set for it was dropped",
                    param_id.param_id
                )),
                // **A known parameter can still be unsequenceable.** A step's deviation is sent as
                // modulation, so one that does not advertise it — or is read-only, or is bypass —
                // cannot carry a lock however it arrived. Dropping only the *unknown* ids would let
                // an imported file play events the plugin never agreed to take.
                Some(param) if !sequenceable(&param) => Some(format!(
                    "{} cannot be sequenced by this instrument; what the steps set for it was dropped",
                    param.name
                )),
                Some(_) => None,
            };
            if let Some(problem) = refusal {
                self.sequencer.locks.clear_param(param_id);
                self.sequence_patch.remove(&param_id);
                problems.push(problem);
            }
        }
        if !problems.is_empty() {
            self.sequencer_changed();
        }
        problems
    }

    /// Keeps the sequence-patch level with the instrument while no step is selected.
    ///
    /// **This is the whole rule.** With a step selected you are editing that step; with nothing
    /// selected you are editing the patch, whatever moved it — a knob here, a knob in the plugin's
    /// own editor, or a preset chosen from the plugin's browser. Following the instrument means none
    /// of those has to be recognised: a preset load is a great many parameters changing while
    /// nothing is selected, which is exactly what "this is the new patch" looks like from here.
    ///
    /// Everything already locked is re-based as it goes, because a lock is a deviation from *this*
    /// patch: leaving old baselines behind would have the unlocked steps restoring values from a
    /// patch nobody is using any more.
    ///
    /// Cheap enough to run every frame — one map write per parameter, and only while nothing is
    /// selected. The sequence is only republished when something actually moved, so a still
    /// instrument costs nothing downstream.
    fn follow_sequence_patch(&mut self) {
        if self.selected_step.is_some() {
            return;
        }
        let mut moved = false;
        for param in &self.params.params {
            // **A sequenced parameter's reading is contaminated, so it is skipped.** nice-plug
            // reports the *modulated* value to the host, so for anything the sequencer is
            // modulating `params` holds the patch plus whatever offset the playhead is applying
            // rather than the patch. Following that would walk the patch up by one offset on every
            // requery — the collapse this feature keeps finding new doors into.
            //
            // Safe because the player already knows the truth: for a locked parameter its own
            // `sequence_patch` **is** the base, and nothing but the player moves it.
            if self.sequencer.locks.locks_anywhere(param.id) {
                continue;
            }
            // **What the control is showing, not what the last requery read.** An edit reaches the
            // plugin through the audio thread and is read back afterwards, so `params` reverts to
            // the pre-edit value for a frame or two after every requery - and the patch would then
            // record a value nobody chose, most visibly the factory default a moment after somebody
            // had dialled something else. `editing` is the same resolution the panel draws with.
            let showing = self.editing.get(&param.id).copied().unwrap_or(param.value);
            let key = crate::sequencer::locks::LockKey::source(param.id);
            if self.sequence_patch.get(&key) == Some(&showing) {
                continue;
            }
            self.sequence_patch.insert(key, showing);
            if self.sequencer.locks.locks_anywhere(param.id) {
                self.sequencer.locks.set_patch(param.id, showing as f32);
                moved = true;
            }
        }
        if moved {
            self.sequencer_changed();
            self.locks_unsaved = true;
        }
    }

    /// Rebuilds [`Self::step_text`] if the selection or the sequence has moved on.
    fn refresh_step_text(&mut self) {
        let key = self
            .editing_step()
            .map(|step| (step, self.sequencer_serial));
        if self.step_text_for == key {
            return;
        }
        self.step_text_for = key;
        self.step_text.clear();
        let Some(step) = self.editing_step() else {
            return;
        };
        for (param_id, value) in self.sequencer.locks.at(step) {
            // The source's own formatter. An effect's parameters are formatted by that effect, and
            // until the picker can reach them this shows the raw value rather than a wrong unit.
            let text = if param_id.is_source() {
                self.engine
                    .format_param(param_id.param_id, f64::from(value))
            } else {
                format!("{value:.3}")
            };
            self.step_text.insert(param_id, text);
        }
    }

    /// What the step being edited sets this parameter to, if it sets it at all.
    ///
    /// `None` when no step is selected, which is the ordinary case: nothing is being edited, so the
    /// live value is the only value there is.
    ///
    /// [`Self::editing_step`], not the raw selection: a tied step is part of one note and reads the
    /// locks of the run it belongs to.
    fn step_value(&self, param_id: u32) -> Option<f64> {
        let step = self.editing_step()?;
        self.sequencer.locks.get(step, param_id).map(f64::from)
    }

    /// The source's key for a parameter id, which is what every control on the instrument panel
    /// authors against. An effect's automation is keyed by its own id; see the lock model.
    fn source_key(param_id: u32) -> crate::sequencer::locks::LockKey {
        crate::sequencer::locks::LockKey::source(param_id)
    }

    /// What each step sets, for a state dump. See [`crate::state::LockView`].
    fn lock_views(&self) -> Vec<crate::state::LockView> {
        self.sequencer
            .locks
            .param_ids()
            .into_iter()
            .map(|param_id| crate::state::LockView {
                param_id: param_id.param_id,
                fx: (!param_id.is_source()).then_some(param_id.fx),
                name: param_id
                    .is_source()
                    .then(|| self.params.get(param_id.param_id).map(|p| p.name.clone()))
                    .flatten(),
                steps: (0..self.sequencer.pattern.len())
                    .map(|step| self.sequencer.locks.get(step, param_id))
                    .collect(),
            })
            .collect()
    }

    /// Installs parked locks, if the plugin now loaded is the one they were recorded for.
    ///
    /// Called after every load, including a load of the *same* plugin, because that is exactly when
    /// a person who quit with locks in place expects to find them again.
    fn claim_parked_locks(&mut self) {
        let Some(parked) = self.parked_locks.clone() else {
            return;
        };
        let Some(loaded) = self.engine.plugin_id().map(str::to_owned) else {
            return;
        };
        // Only Source is bound to the recorded instrument. Effects retain their own identities
        // and must remain usable while another source is auditioned.
        let mut claim = parked.clone();
        if parked.plugin.as_deref().is_some_and(|id| id != loaded) {
            claim.params.retain(|entry| entry.fx.is_some());
            claim.plugin = Some(loaded.clone());
            let mut waiting = parked;
            waiting.params.retain(|entry| entry.fx.is_none());
            self.parked_locks = (!waiting.params.is_empty()).then_some(waiting);
        } else {
            self.parked_locks = None;
        }
        let chain = self.chain_refs();
        let (locks, mut problems, pending) = claim.to_locks_in_chain(Some(&loaded), &chain);
        self.sequencer.locks = locks;
        self.pending_fx_locks = pending;
        self.sync_baselines();
        self.sequencer_changed();
        problems.extend(self.prune_unknown_locks());
        // Locks on tied steps are kept: they are per step now, holds and slides included, so a
        // file that carries them is carrying music rather than a leftover.
        if let Some(first) = problems.first() {
            self.status = Some(match problems.len() {
                1 => first.clone(),
                n => format!("{first} (and {} more)", n - 1),
            });
        }
        // The file now means what memory means, so a later `persist` may capture rather than echo.
        self.locks_unsaved = true;
    }

    /// A parameter was moved. **If a step is selected, that step now sets it.**
    ///
    /// The gesture that already existed: selecting a step makes the keyboard modal and `note_on`
    /// writes into it. This is the same rule for knobs — *while a step is selected, the keyboard
    /// writes its note and the knobs write its parameters* — and Escape stops both.
    ///
    /// **Every path arrives here**: the player's own panel, a mapped hardware knob through
    /// `apply_mapped_cc`, and the plugin's own editor, whose knob turns reach the player as
    /// `PluginOutput::ParamValue`. Leaving any of them out would mean sequencing worked in some
    /// places and silently did nothing in others, which is the shape of defect
    /// `apps/mxm-player/NOTES.md`'s *"whatever the note keys do, a MIDI keyboard does too"* rule
    /// was written about.
    ///
    /// **The value is still sent to the plugin.** You hear what you are setting, exactly as playing
    /// a note into a selected step sounds it.
    fn parameter_edited(&mut self, param_id: u32, value: f64) -> bool {
        // **[`Self::editing_step`], the same answer the notes take** — the raw selection: locks
        // are per step, tied ones included, so the knob writes exactly the step the panel shows.
        let Some(step) = self.editing_step() else {
            // **No step selected: this is the patch being edited.** A parameter something else
            // sequences has to follow, or the sequencer would fight the knob — you would turn it and
            // hear it snap back at the next step, because the unlocked steps would go on restoring
            // the value it used to have.
            // Recorded whether or not anything is locked yet: this **is** the patch, and the first
            // lock written against this parameter later needs to know what it deviates from. An
            // earlier version only updated a key that already existed, which meant the map was empty
            // exactly when it was about to be needed and a fallback was quietly doing the work.
            self.sequence_patch
                .insert(Self::source_key(param_id), value);
            if self.sequencer.locks.locks_anywhere(param_id) {
                self.sequencer.locks.set_patch(param_id, value as f32);
                self.sequencer_changed();
                self.locks_unsaved = true;
            }
            return true;
        };

        // **Some parameters are not the sequencer's to move.** A read-only one cannot be set at all,
        // and bypass is the host's switch rather than part of the sound - a step that silently
        // bypassed the instrument would be the most confusing lock it is possible to write. Refused
        // *here*, at the funnel, rather than in each control: the mapped-CC path can name any id at
        // all, and a guard only the panel enforces is not a guard.
        if let Some(param) = self.params.get(param_id)
            && !sequenceable(param)
        {
            self.status = Some(format!("{} cannot be sequenced", param.name));
            // **Refusing has to stop the delivery too.** Returning quietly here left the caller to
            // send a `ParamMod` for a parameter the plugin never said it would accept, which is the
            // one thing this guard exists to prevent.
            return false;
        }

        self.grow_to_reach(step);

        // The patch's own value, remembered the first time this parameter is locked — so that
        // clearing a lock has somewhere to return to. Without it, "back to the patch" would mean
        // "back to whatever the sequencer last left here", which is not a patch value at all.
        //
        // **Taken from what the control was showing, not from `value`.** `value` is the value being
        // *written into the step*; recording that as the patch value would make clearing the lock
        // return the parameter to the lock, which is the one thing it must not do.
        //
        // `editing` before `params` for the same reason the panel draws them in that order: an edit
        // reaches the plugin through the audio thread, so `params` can still be reporting the value
        // from before the last few frames. This runs *before* `parameter_moved` updates `editing`,
        // which is what makes "what it was showing" mean the value from before this edit.
        let patch_before = self
            .sequence_patch
            .get(&Self::source_key(param_id))
            .copied()
            .unwrap_or_else(|| {
                self.editing
                    .get(&param_id)
                    .copied()
                    .or_else(|| self.params.get(param_id).map(|param| param.value))
                    .unwrap_or(value)
            });
        self.sequence_patch
            .entry(Self::source_key(param_id))
            .or_insert(patch_before);

        // **A lock at the patch value is a real lock — a *drag* never collapses.** An earlier rule
        // cleared the lock whenever an edit landed on the patch value; the owner rejected it,
        // because a lock is an absolute value: a step pinned to today's patch must stay pinned
        // when the patch moves tomorrow. What the same owner later ruled the other way is the
        // **click**: an instantaneous edit — a toggle, a click-jump, a text entry — landing back
        // on the patch means *this step sets nothing*, and the callers resolve that to the reset
        // path before this function runs (`clicked_to_patch` in the panel, the single-edit commit
        // in `restore_base_for_step_edit`, the instantaneous branch in `plugin_moved`). By the
        // time a patch-valued edit reaches here it is a drag's ending or a scripted set, and it
        // pins.
        match self.write_step_lock(step, Self::source_key(param_id), value, patch_before as f32) {
            Ok(()) => {
                // **The companions take the same lock.** A multi-step selection edits every step
                // it holds — the same absolute value on each, exactly as if the knob had been
                // turned on each in turn. Refusals past the anchor keep what landed and say so:
                // dropping the anchor's lock because a companion hit the budget would undo an
                // edit that was already heard.
                let companions: Vec<usize> = self
                    .also_selected
                    .iter()
                    .copied()
                    .filter(|companion| *companion != step)
                    .collect();
                let mut refused_at = None;
                for companion in companions {
                    self.grow_to_reach(companion);
                    if self
                        .write_step_lock(
                            companion,
                            Self::source_key(param_id),
                            value,
                            patch_before as f32,
                        )
                        .is_err()
                    {
                        refused_at = Some(companion);
                    }
                }
                self.status =
                    refused_at.map(|at| format!("the lock set filled at step {}", at + 1));
                self.sequencer_changed();
                // Marked, not written: this runs on every frame of a knob drag.
                self.locks_unsaved = true;
                true
            }
            Err(refused) => {
                self.status = Some(refused.to_string());
                false
            }
        }
    }

    /// A parameter reached a new value from the interface. Returns the value that took effect.
    ///
    /// **The one place a knob turn is interpreted**, so the panel, a scripted run and the CLI cannot
    /// drift apart: they are the same code, not three copies of it. Two things happen here and the
    /// order matters — the plugin is told, so you hear it, and the sequencer is told, so a selected
    /// step records it.
    ///
    /// `reset` is §7.1's double-click. With a step selected it means *this step sets nothing*, and
    /// the value that takes effect is the patch's own rather than the parameter's default — see
    /// [`Self::parameter_reset_if`].
    pub fn parameter_moved(&mut self, param_id: u32, value: f64, reset: bool) -> f64 {
        let value = self.parameter_reset_if(reset, param_id).unwrap_or(value);
        // **Before `editing` is updated.** The patch value it records is read from there, so writing
        // the new value first would record the edit as the thing to return to.
        // A refusal stops the delivery: see `parameter_edited`.
        if !reset && !self.parameter_edited(param_id, value) {
            return value;
        }

        self.deliver_edit(param_id, value, reset);
        value
    }

    /// Records an edit in the panel's own snapshot, so the control stays where it was put.
    ///
    /// **The snapshot is authoritative until the plugin answers.** An edit reaches the plugin
    /// through the audio thread and is read back afterwards, so a panel that redrew from the
    /// snapshot alone would show the value from before the edit — which is the snap-back defect
    /// `the_drag_oracle_would_have_caught_the_snap_back_defect` is named for.
    ///
    /// An associated function taking the two fields rather than `&mut self`, because the panel calls
    /// it with a `ParamSet` it has taken out of `self` for the duration of the draw. One copy of the
    /// rule, reachable from both.
    /// Puts the panel's reading of every sequenced parameter back to the patch.
    ///
    /// A requery reads the **modulated** value, which for a sequenced parameter is the patch plus
    /// whatever offset the playhead happens to be applying. That is not what the control should
    /// show: the knob shows the value, and the sequence rides over it. Without this the panel would
    /// jitter through every bar while nothing was actually moving those controls.
    fn show_patch_for_sequenced(&mut self) {
        for key in self.sequencer.locks.param_ids() {
            // **The instrument panel only.** An effect's controls are drawn by that effect's own
            // editor, which reads its own parameters; nothing here has a control to correct.
            if !key.is_source() {
                continue;
            }
            let param_id = key.param_id;
            // **A loaded lock set brings its own baseline, and nothing else will supply one.**
            // `follow_sequence_patch` skips locked parameters — that is what stops the modulated
            // read-back contaminating the patch — so a set that arrived from a file or from parking
            // would never get an entry at all, and this correction would silently do nothing.
            if !self.sequence_patch.contains_key(&key)
                && let Some(stored) = self.sequencer.locks.patch(key)
            {
                self.sequence_patch.insert(key, f64::from(stored));
            }
            let Some(patch) = self.sequence_patch.get(&key).copied() else {
                continue;
            };
            let text = self.engine.format_param(param_id, patch);
            if let Some(param) = self.params.get_mut(param_id) {
                param.value = patch;
                param.text = text;
            }
        }
    }

    fn note_edit(engine: &mut Engine, params: &mut ParamSet, param_id: u32, value: f64) {
        let text = engine.format_param(param_id, value);
        if let Some(param) = params.get_mut(param_id) {
            param.value = value;
            param.text = text;
        }
    }

    /// Writes anything the debounce is still holding back.
    ///
    /// Public because shutdown is not the only end of a session: a scripted run stops without an
    /// event loop, and so would a CLI. See [`LOCK_SAVE_INTERVAL`] for what is being held and why.
    pub fn flush_settings(&mut self) {
        if self.locks_unsaved {
            self.persist();
        }
    }

    /// Takes hold of a parameter, as pressing the mouse on its knob does.
    ///
    /// **Bracketing an edit is what the CLAP contract asks of a host**, so a host that does not do
    /// it records automation as unrelated jumps. It no longer has anything to do with the sequencer:
    /// a hand moves the parameter's value and a step moves an offset laid over it, so they are
    /// different layers and there is nothing to stand down.
    ///
    /// Public so a scripted run and a CLI can hold a control the way a hand does.
    pub fn begin_parameter_gesture(&mut self, param_id: u32) {
        if self.engine.gesture_end_pending(param_id) {
            return;
        }
        self.engine
            .push_gui_event(Payload::GestureBegin { param_id });
        self.open_gestures.push(param_id);
    }

    /// Lets go of a parameter, closing the gesture the host opened.
    pub fn end_parameter_gesture(&mut self, param_id: u32) {
        self.engine.push_gui_event(Payload::GestureEnd { param_id });
        self.open_gestures.retain(|open| *open != param_id);
        self.requery_params = true;
    }

    /// Sends an edit to the plugin — **only when it is the patch that changed.**
    ///
    /// With a step selected the edit belongs to that step, and the *runtime* applies it: the state
    /// it publishes carries which step is being edited, and the runtime lays the offset over the
    /// parameter itself. Nothing is sent from here, and that is the point — an offset applied from
    /// two places has to be taken off from two places, and only one of them can order its answer
    /// against the steps still being emitted.
    ///
    /// So the host's job with a step selected is simply to write the lock and republish. Which it
    /// has already done by the time this runs.
    fn deliver_edit(&mut self, param_id: u32, value: f64, reset: bool) {
        if self.selected_step.is_some() && !reset {
            return;
        }
        self.editing.insert(param_id, value);
        self.engine
            .push_gui_event(Payload::ParamValue { param_id, value });
        // **Only here.** The panel's snapshot records the parameter's *value*, and with a step
        // selected the value did not change — the deviation is an offset the runtime applies.
        Self::note_edit(&mut self.engine, &mut self.params, param_id, value);
    }

    /// Makes the lock set and the sequence-patch agree about every locked parameter.
    ///
    /// Two directions, and which one wins depends on who knows:
    ///
    /// - **A lock that arrived with a baseline is authoritative**, because that is what its steps
    ///   were measured against when they were written. The sequence-patch takes it.
    /// - **A lock with no baseline takes the instrument's current value.** A file written before
    ///   baselines were stored has none, and `Locked::offset` then yields zero for every step — the
    ///   locks would load and do nothing at all, silently. The current value is the honest stand-in:
    ///   it is what the instrument is set to, which is what an unlocked step restores anyway.
    ///
    /// **Never a wholesale clear.** The sequence-patch describes the *instrument*, which a sequence
    /// load does not change; emptying it threw away the one thing that could stand in for a missing
    /// baseline.
    fn sync_baselines(&mut self) {
        for key in self.sequencer.locks.param_ids() {
            match self.sequencer.locks.patch(key) {
                Some(stored) => {
                    self.sequence_patch.insert(key, f64::from(stored));
                }
                None => {
                    // Never borrow a same-numbered source parameter for an effect.
                    let Some(value) = self
                        .sequence_patch
                        .get(&key)
                        .copied()
                        .or_else(|| self.lock_parameter(key).map(|p| p.value))
                    else {
                        continue;
                    };
                    self.sequencer.locks.set_patch(key, value as f32);
                    self.sequence_patch.insert(key, value);
                }
            }
        }
    }

    /// Puts the instrument where the loaded lock record says the patch is.
    ///
    /// A load is the one moment base and patch can legitimately disagree: the plugin's state
    /// restores what the instrument was last *left at*, the lock record restores what its
    /// deviations are *measured from*, and nothing ties the two together. A base stranded by an
    /// old bug then survives every launch — watched live: a plugin restored at cutoff 0.946 under
    /// a record whose patch said 0.890, sounding a step's lock on every unlocked step while every
    /// corrected view showed the patch. The record wins: it is what the locks were authored
    /// against, and what an unlocked step promises to restore.
    fn settle_bases_to_patch(&mut self) {
        for key in self.sequencer.locks.param_ids() {
            // The instrument's own parameters. An effect's base is the effect's business.
            if !key.is_source() {
                continue;
            }
            let param_id = key.param_id;
            let Some(patch) = self.sequence_patch.get(&key).copied() else {
                continue;
            };
            // Unconditionally: there is nothing here that could be compared. The panel snapshot
            // is *corrected* - a locked parameter in it reads as the patch by design - so checking
            // it against the patch always answers "equal", exactly when the instrument disagrees.
            // A redundant set is harmless; a skipped one is this bug.
            self.engine.push_gui_event(Payload::ParamValue {
                param_id,
                value: patch,
            });
            Self::note_edit(&mut self.engine, &mut self.params, param_id, patch);
        }
    }

    /// Restores every parked base to the patch and empties the set.
    ///
    /// Called wherever the world the parked values belong to ends: leaving the step, Clear, a
    /// sequence or MIDI load, a plugin change, and the transport starting — a base parked at a
    /// step's value under a *running* sequence would have the stepping's offsets land on top of it
    /// and everything would sound a deviation high.
    /// Parks the selected step's locked parameters at their locked values — **at rest only**.
    ///
    /// The reason is a click the player cannot see. The plugin's editor displays base plus
    /// modulation, and its controls write the base relative to what they display: with a lock
    /// previewed as modulation the Slide toggle shows On over a base of Off, so clicking it Off
    /// writes a base that is already Off — **no event is emitted**, the control is dead, and a
    /// lock made under a running transport could never be removed from the editor after Stop.
    /// Parking puts the locked value on the base (the preview is held to zero through the same
    /// `held` set, so nothing doubles): the editor shows the truth, and its next click inverts
    /// something real. This is the same state a drag's release or a click already leaves —
    /// stopping and selecting just reach it too, so how a lock was *made* stops mattering.
    fn park_selected_step_locks(&mut self) {
        if self.sequencer.transport != Transport::Stopped {
            return;
        }
        let Some(step) = self.selected_step else {
            return;
        };
        let mut moved = false;
        for key in self.sequencer.locks.param_ids() {
            // **Parking is the instrument panel's own trick**: it puts the plugin's base where the
            // selected step's lock is so the knob shows what the step sets. An effect's controls
            // are drawn by its own editor, which the player does not park.
            if !key.is_source() {
                continue;
            }
            let param_id = key.param_id;
            let Some(locked) = self.sequencer.locks.get(step, key) else {
                continue;
            };
            if !self.parked_bases.insert(param_id) {
                continue;
            }
            let value = f64::from(locked);
            self.engine
                .push_gui_event(Payload::ParamValue { param_id, value });
            Self::note_edit(&mut self.engine, &mut self.params, param_id, value);
            moved = true;
        }
        if moved {
            self.sequencer_changed();
        }
    }

    fn unpark_bases(&mut self) {
        for param_id in std::mem::take(&mut self.parked_bases) {
            let Some(patch) = self
                .sequence_patch
                .get(&Self::source_key(param_id))
                .copied()
            else {
                continue;
            };
            self.engine.push_gui_event(Payload::ParamValue {
                param_id,
                value: patch,
            });
            Self::note_edit(&mut self.engine, &mut self.params, param_id, patch);
        }
    }

    /// Puts a parameter back to the patch after the plugin's own editor moved it into a step.
    ///
    /// **Not while the control is held.** The plugin's editor moves its parameter and tells the
    /// player afterwards, so with a step selected every frame of a drag arrives as "the patch just
    /// moved" — and restoring on each of them puts the knob back under the hand sixty times a
    /// second. That is the *"it jumps back to the original value"* fault.
    ///
    /// A drag is bracketed by gestures, which is the seam: while one is open the parameter is
    /// somebody's to move, and this runs when they let go. What the step sets was recorded on the
    /// way past, so nothing is lost by waiting.
    fn restore_base_for_step_edit(&mut self, param_id: u32) {
        if self.selected_step.is_none() || self.open_gestures.contains(&param_id) {
            return;
        }

        // The drag is over, so what it settled on is what the step sets. **At rest, the base stays
        // where the hand put it** — the knob sits at the locked position, no arc, for as long as
        // the step is selected; three attempts that returned it to the patch on release were each
        // rejected as the knob refusing input. If the *lock* is refused (unsequenceable, or the set
        // is full), the base does go back: a refusal must not become a patch edit nobody made.
        //
        // **Under a running transport the base goes back too.** Parking works by publishing the
        // parameter as held, which zeroes its offsets — a rest-time trick, invisible while nothing
        // steps. With the sequencer running it silenced every step's deviation and left the base at
        // the hand's value, so the whole sequence sounded the edited step's cutoff: *"it still
        // changes it for all the other steps too"*. The moment the hand lets go, a live take needs
        // the base at the patch and the offsets doing the work.
        if let Some(step) = self.editing_step()
            && let Some(value) = self.pending_step_edit.remove(&(step, param_id))
        {
            // **A click landing back on the patch is the reset, not a lock at the patch value.**
            // The owner's ruling: setting a control back to the sequencer patch means the step
            // sets nothing — off, and no dot. It reached this branch instead of the
            // instantaneous one only because the editor's begin/value/end straddled two service
            // turns, which is timing, not intent. A *drag* — many values — ending on the patch
            // still pins, so the absolute-lock ruling survives for the gesture it was made about.
            //
            // **And under a running transport, a click landing on the value the step already
            // locks clears it too.** Live, the base cannot park — it springs to the patch the
            // moment the hand lets go — so a toggle whose lock is On always *shows* Off and
            // every click emits On: the control cannot express Off at all, and the lock could
            // never be removed while playing. A click that writes exactly what the step already
            // sets is the same toggle clicked again, and a toggle's second click means off. At
            // rest the rule stands down: the parked base makes the control alternate honestly,
            // and a click on a control showing Off must not delete an unseen lock.
            let edits = self.open_gesture_edits.remove(&param_id).unwrap_or(1);
            let near =
                |target: Option<f64>| target.is_some_and(|target| (value - target).abs() < 1e-6);
            let patch = self
                .sequence_patch
                .get(&Self::source_key(param_id))
                .copied();
            // **A parked knob's reset arrives as the factory default, here as well as in the
            // instantaneous branch.** Parking blinds the editor's reset-target to everything but
            // the default — and the editor brackets its double-click in a gesture, so the batch
            // loop routes the value here, never to `plugin_moved`'s instantaneous branch. Reading
            // only the patch as a reset here wrote the default back *as the lock*: base parked
            // at the default, no modulation, no dot on the knob — and the step's dot stayed.
            // Reported from exactly that, on mxm-poly-06's Noise knob. Parked *at the gesture's
            // open*, because the held branch parks every editor gesture at rest, and reading
            // that would turn any click landing on the default into a reset.
            let was_parked = self.gesture_began_parked.remove(&param_id);
            let default = self.params.get(param_id).map(|p| p.default);
            let live = self.sequencer.transport != Transport::Stopped;
            let re_click = live
                && self
                    .sequencer
                    .locks
                    .get(step, param_id)
                    .is_some_and(|locked| (value - f64::from(locked)).abs() < 1e-6);
            if edits <= 1 && (near(patch) || (was_parked && near(default)) || re_click) {
                if self.parked_bases.remove(&param_id) {
                    self.sequencer_changed();
                }
                self.reset_parameter(param_id);
                return;
            }
            let accepted = self.parameter_edited(param_id, value);
            let live = self.sequencer.transport != Transport::Stopped;
            if (!accepted || live) && self.parked_bases.remove(&param_id) {
                if let Some(patch) = self
                    .sequence_patch
                    .get(&Self::source_key(param_id))
                    .copied()
                {
                    self.engine.push_gui_event(Payload::ParamValue {
                        param_id,
                        value: patch,
                    });
                    Self::note_edit(&mut self.engine, &mut self.params, param_id, patch);
                }
                self.sequencer_changed();
            }
        }

        // **A gesture-less edit reaching here still restores the base.** At rest, `plugin_moved`
        // parks an instantaneous edit instead of calling this — the toggle keeps showing what was
        // clicked — so this tail runs for the live-transport case, where the stepping's offsets
        // need the patch underneath them. Parked parameters are exactly the ones it must not
        // touch.
        if self.parked_bases.contains(&param_id) {
            return;
        }
        let Some(patch) = self
            .sequence_patch
            .get(&Self::source_key(param_id))
            .copied()
        else {
            return;
        };
        if self.params.get(param_id).is_some_and(|p| p.value == patch) {
            return;
        }
        self.engine.push_gui_event(Payload::ParamValue {
            param_id,
            value: patch,
        });
        Self::note_edit(&mut self.engine, &mut self.params, param_id, patch);
    }

    /// The plugin reported that a gesture opened on one of its own controls.
    ///
    /// Public so a scripted run can reproduce a drag in the instrument's editor, which is where the
    /// timing of the base restore matters and nothing else can produce one.
    pub fn plugin_gesture_began(&mut self, param_id: u32) {
        if !self.open_gestures.contains(&param_id) {
            self.open_gestures.push(param_id);
        }
        // A fresh gesture starts a fresh count; a stale one would call a click a drag.
        self.open_gesture_edits.remove(&param_id);
        // Whether the base is parked *now*, before this gesture's own values park it: the
        // editor's reset aims at the factory default exactly when it is, and the commit has to
        // know which target the editor was aiming at.
        if self.parked_bases.contains(&param_id) {
            self.gesture_began_parked.insert(param_id);
        } else {
            self.gesture_began_parked.remove(&param_id);
        }
    }

    /// The plugin reported that one of its own gestures closed. See [`Self::plugin_gesture_began`].
    pub fn plugin_gesture_ended(&mut self, param_id: u32) {
        self.open_gestures.retain(|open| *open != param_id);
        self.restore_base_for_step_edit(param_id);
        self.open_gesture_edits.remove(&param_id);
        self.gesture_began_parked.remove(&param_id);
    }

    /// Moves a parameter, as turning its knob would. See [`Self::parameter_moved`].
    ///
    /// **A complete edit**, so it does what releasing a knob does: records the value in the panel's
    /// own snapshot and asks for a requery. A drag does neither on every frame — re-reading
    /// mid-drag is what makes a control snap back — but a scripted set has no release to wait for.
    pub fn set_parameter(&mut self, param_id: u32, value: f64) {
        self.parameter_moved(param_id, value, false);
        self.requery_params = true;
    }

    /// Returns a parameter to the patch, as double-clicking its knob would.
    pub fn reset_parameter(&mut self, param_id: u32) {
        let value = self.params.get(param_id).map_or(0.0, |param| param.default);
        self.parameter_moved(param_id, value, true);
        self.requery_params = true;
    }

    /// Clears what the selected step sets for `param_id`, and returns the parameter to the patch.
    ///
    /// §7.1 already gives every continuous control a double-click reset. With a step selected, the
    /// value it resets to is the **patch's**, and returning a parameter to the patch is exactly what
    /// having no lock means — so the gesture is reinterpreted rather than a new one being added.
    fn parameter_reset_if(&mut self, reset: bool, param_id: u32) -> Option<f64> {
        if !reset {
            return None;
        }
        // [`Self::editing_step`], like every other lock path: clearing has to reach the step the
        // write went to, or a double-click on a tied step would silently clear nothing.
        let step = self.editing_step()?;
        self.sequencer.locks.clear(step, param_id);
        self.sequencer_changed();

        // The last lock on this parameter is gone, so the patch value is the live value again and
        // there is nothing left to remember.
        // **The entry is kept, not removed, even when this was the parameter's last lock.** This
        // only ever runs with a step selected, and while one is selected nothing else maintains
        // the map — `follow_sequence_patch` deliberately stands down. Removing the entry here
        // opened a window where the *next* editor edit re-derived "the patch" from the panel
        // snapshot, which at that instant already held the arriving value: the patch became the
        // edit, and a click back to the real patch no longer read as one. Once the step is left,
        // the follow refreshes the entry every frame, so keeping it costs nothing.
        self.persist();
        self.sequence_patch
            .get(&Self::source_key(param_id))
            .copied()
    }

    /// Sends a control change from the GUI source, exactly as a MIDI port would.
    ///
    /// The whole path: it goes through the merge on the audio thread, which decides whether the
    /// map claims it, and a claimed one comes back to be mapped here. A test that mapped the CC
    /// directly would prove nothing about the route a real knob takes.
    pub fn send_control_change(&mut self, controller: u8, value: u8) {
        self.engine.push_gui_event(Payload::ControlChange {
            channel: 0,
            controller,
            value,
        });
    }

    /// Takes what a connected keyboard has played since the last turn.
    ///
    /// A MIDI note never reaches [`PlayerApp::note_on`] — it is stamped in the MIDI callback and
    /// queued straight for the audio thread — so everything that path does for the on-screen and
    /// computer keyboards has to be done here too, or a MIDI keyboard is not the first-class
    /// input the player claims. Writing into a selected step is the one that was missing.
    ///
    /// Sounding the note is **not** repeated here: the audio thread already has it. This is the
    /// editing half only.
    fn receive_midi_presses(&mut self) {
        let shared = self.engine.shared().clone();
        if let Some(step) = self.selected_step {
            for note in shared.take_midi_pressed() {
                self.toggle_step_note(step, note);
            }
        } else {
            // Still drained, so a press made with no step selected cannot be applied later.
            let _ = shared.take_midi_pressed();
        }
        self.midi_held = shared.midi_sounding();
    }

    pub fn note_on(&mut self, note: u8, velocity: f64) {
        // While a step is selected the keyboard writes into it **and** sounds the note, so you
        // hear what you are entering. Both the on-screen keys and the computer keyboard take this
        // path, which is what makes typing a pattern in workable.
        for step in self.selected_steps() {
            self.toggle_step_note(step, note);
        }

        if self.held.contains(&note) {
            return;
        }
        if self.engine.push_gui_event(Payload::NoteOn {
            channel: 0,
            key: note,
            velocity,
        }) {
            self.held.push(note);
        }
    }

    pub fn note_off(&mut self, note: u8) {
        self.engine.push_gui_event(Payload::NoteOff {
            channel: 0,
            key: note,
            velocity: 0.0,
        });
        self.held.retain(|n| *n != note);
    }

    /// Shifting releases held notes first, so nothing sticks at the old pitch.
    pub fn shift_octave(&mut self, delta: i32) {
        let next = (self.octave + delta).clamp(keyboard::MIN_OCTAVE, keyboard::MAX_OCTAVE);
        if next == self.octave {
            return;
        }
        // The same targeted cleanup as focus loss: our presses, and nobody else's.
        self.engine.push_gui_event(Payload::CleanupSource);
        self.held.clear();
        self.octave = next;
        self.settings.octave = Some(next);
        self.persist();
    }

    pub fn set_sustain(&mut self, held: bool) {
        self.sustain = held;
        // Host-side: note-offs for held keys are deferred until the pedal lifts. CC 64 is
        // deliberately *not* forwarded, because mxm-mono-01 ignores sustain and a control that
        // silently does nothing is worse than no control.
        self.engine.push_gui_event(Payload::SustainPedal(held));
    }

    /// Tells the audio worker which CCs to route to the GUI instead of to the plugin.
    ///
    /// Only when it changes: the command queue is small, and a mask resent every frame would
    /// crowd out a `Stop` that something is waiting on.
    fn publish_claimed_ccs(&mut self) {
        let mask = self.control_map.claimed();
        if self.published_mask == Some(mask) {
            return;
        }
        if self.engine.set_claimed_ccs(mask) {
            self.published_mask = Some(mask);
        }
    }

    // --- the sequencer -----------------------------------------------------------------------

    /// Publishes the sequencer's state, at most one publication ahead of the worker.
    ///
    /// The bound is what stops a tempo drag at frame rate filling the 64-slot command queue with
    /// stale states and delaying the `Stop` that `Engine::stop_now` polls for. Edits made while a
    /// publication is outstanding coalesce into the next one, so the worker still converges on the
    /// latest state without the queue ever holding more than one.
    fn publish_sequencer(&mut self) {
        if self.sequencer_published >= self.sequencer_serial {
            return;
        }
        if self.sequencer_published > self.engine.sequencer_ack() {
            return; // one is still outstanding
        }

        // **Cloned, on this thread.** The state owns a pattern of whatever length somebody made,
        // so this is a real copy — paid here, where allocating is allowed, and never on the audio
        // thread, which reads the published `Arc` and never clones it.
        let mut state = self.sequencer.clone();
        state.serial = self.sequencer_serial;
        // **Which step is being edited travels with the state.** The runtime previews it, which is
        // what keeps one owner of an applied offset; publishing it here rather than storing it in
        // `self.sequencer` keeps the selection a piece of interface state that happens to be told to
        // the worker, rather than part of the sequence itself.
        //
        // **[`Self::editing_step`]** — the raw selection, now that locks are per step: the
        // runtime previews the selected step's own locks, which is also the cell the panel
        // shows. One notion of "the step being edited", or the screen and the sound disagree.
        state.editing = self.published_editing_step();
        // The loop window: one bar, the selected bar's eight-bar pattern, or everything. The
        // runtime queues a change for the current bar's end when it arrives mid-play.
        state.bar = match self.loop_scope {
            LoopScope::Bar => Some((self.selected_bar as u32, 1)),
            LoopScope::Pattern => Some((
                (self.selected_bar / PATTERN_BARS * PATTERN_BARS) as u32,
                PATTERN_BARS as u32,
            )),
            LoopScope::All => None,
        };
        // Parameters whose base is parked at the step's value get no preview — the value is already
        // on the base. Only meaningful while a step is selected.
        state.held = crate::sequencer::runtime::HeldParams::EMPTY;
        if self.selected_step.is_some() {
            for param_id in &self.parked_bases {
                state.held.insert(*param_id);
            }
        }
        if self.engine.set_sequencer(state) {
            self.sequencer_published = self.sequencer_serial;
        }
    }

    /// Whether an edit is still waiting for its turn to reach the worker.
    ///
    /// **What a headless session waits on before it renders.** [`publish_sequencer`] holds at one
    /// publication outstanding, so an edit made while the worker has not yet acknowledged the
    /// previous state does not travel on this frame. That is harmless in the window, where the
    /// next frame is sixteen milliseconds away and carries it; it is not harmless in a session,
    /// where the next thing that happens is a block rendered from whatever the worker last
    /// adopted. See [`Session::advance_blocks`](crate::session::Session::advance_blocks).
    ///
    /// [`publish_sequencer`]: Self::publish_sequencer
    pub fn sequencer_publish_pending(&self) -> bool {
        self.sequencer_published < self.sequencer_serial
    }

    /// Marks the sequencer state as changed, so it is republished.
    fn sequencer_changed(&mut self) {
        self.sequencer_serial += 1;
    }

    pub fn sequencer_state(&self) -> SequencerState {
        self.sequencer.clone()
    }

    /// `(transport, step)` as the audio thread last reported them.
    ///
    /// Read rather than recomputed: the exact position lives on the audio thread, and a second
    /// derivation on GUI frames would disagree exactly where it matters — around pause, tempo
    /// changes, and commands the worker has not accepted yet.
    pub fn playhead(&self) -> (Transport, usize) {
        let (transport, step) = self.engine.playhead();
        let transport = match transport {
            1 => Transport::Playing,
            2 => Transport::Paused,
            _ => Transport::Stopped,
        };
        (transport, step as usize)
    }

    /// The bar the sequencer is sounding, or `None` when nothing is.
    ///
    /// **A named method rather than an expression inside the bar strip**, so a test can assert on
    /// exactly what the chips are lit from rather than on a second copy of the same arithmetic.
    ///
    /// `Playing` only, exactly as the step row's highlight: stopping parks the playhead on a step,
    /// and a chip that stayed lit would claim a bar is sounding when nothing is.
    pub fn playing_bar(&self) -> Option<usize> {
        let (transport, step) = self.playhead();
        (transport == Transport::Playing).then(|| step / self.sequencer.pattern.steps_per_bar())
    }

    /// The single transport toggle: play when stopped or paused, pause while playing.
    ///
    /// **Never bumps the run generation**, which is the entire difference from
    /// The one transport gesture: **play from step 1, or stop.**
    ///
    /// There used to be a separate *From start* button, and Play resumed where it left off. Two
    /// controls, and the difference between them was invisible until you pressed one — while the
    /// spacebar, which is the same gesture, already rewound. So Play and Space disagreed, and the
    /// second button existed to offer what most presses of the first were meant to do.
    ///
    /// **And once Play always rewinds, the other half is Stop, not Pause.** A pause you cannot
    /// resume from is a stop with a misleading name and a playhead left in the wrong place. So this
    /// sets [`Transport::Stopped`], which rewinds the clock, and the button says so.
    ///
    /// `Transport::Paused` survives in the model and is still reachable from
    /// [`pause`](Self::pause), which the paths that must silence the sequencer *without a person
    /// asking* use — they want the position kept. `Runtime` implements resume too, since a
    /// same-generation transition to `Playing` is what that means and the export depends on the
    /// distinction. **This narrowed the controls, not the model**: a control removed is easy to
    /// restore, and a model collapsed is not.
    pub fn play_stop(&mut self) {
        if self.sequencer.transport == Transport::Playing {
            self.stop_sequencer();
        } else {
            self.play_from_start();
        }
    }

    /// Rewinds to step 1 and plays, from any state.
    ///
    /// Replaces Stop: hearing the sequence from the top was two clicks and is now one. The new
    /// generation is what makes the runtime restart rather than resume.
    ///
    /// No longer has a button of its own — [`play_pause`](Self::play_pause) is this, and the
    /// spacebar always was.
    pub fn play_from_start(&mut self) {
        if let Err(reason) = self.try_play_from_start() {
            self.status = Some(reason);
        }
    }

    /// [`play_from_start`](Self::play_from_start) with the refusal returned rather than shown,
    /// for the CLI's `play` verb.
    ///
    /// **Refused while the audio device is stopped or the plugin is wedged.** There is no worker
    /// to step the sequencer, so setting the transport to `Playing` would light the button over
    /// silence; the refusal carries why, the same way a refused MIDI port does. Not refused with
    /// no plugin loaded — see the transport label: the button follows the GUI's intent there, and
    /// a Play that never changed would read as broken.
    pub fn try_play_from_start(&mut self) -> Result<(), String> {
        if let Some(reason) = self.transport_refusal() {
            return Err(reason);
        }
        // A base parked at a step's value under a *running* sequence would have the stepping's
        // offsets land on top of it, and everything would sound a deviation high.
        self.unpark_bases();
        self.sequencer.generation += 1;
        self.sequencer.transport = Transport::Playing;
        self.sequencer_changed();
        Ok(())
    }

    /// Why the transport cannot play right now, if it cannot.
    fn transport_refusal(&self) -> Option<String> {
        match self.engine.state() {
            EngineState::StreamExited(reason) => Some(if self.engine.reconnect().is_some() {
                format!("audio device stopped, reconnecting: {reason}")
            } else {
                format!("audio device stopped: {reason}")
            }),
            EngineState::Wedged => Some(
                "audio stop timed out waiting for the processor return; restart the player"
                    .to_owned(),
            ),
            _ => None,
        }
    }

    /// Kept for the paths that must silence the sequencer without a person asking — plugin load,
    /// rescan, engine stop. There is no Stop button any more.
    pub fn pause(&mut self) {
        self.sequencer.transport = Transport::Paused;
        self.sequencer_changed();
    }

    pub fn stop_sequencer(&mut self) {
        self.sequencer.transport = Transport::Stopped;
        self.sequencer_changed();
        // Coming to rest with a step selected: its locks park onto the base, so the plugin's
        // editor shows them and its controls stay operable — see `park_selected_step_locks`.
        self.park_selected_step_locks();
    }

    /// Silences the sequencer, for the paths that change what is loaded underneath it — a rescan, a
    /// plugin change.
    ///
    /// **It does not take the modulation off, and must not try.** The runtime owns every offset —
    /// the steps it plays and the step being previewed alike — and zeroes them itself when the
    /// transport comes to rest or a parameter leaves the lock set. That is the only place the
    /// ordering can be right: a zero sent from here would travel a different queue from the state
    /// that stops the stepping, and an old step could overtake it.
    fn quiet_the_sequencer(&mut self) {
        self.sequencer.transport = Transport::Stopped;
        self.sequencer_changed();
    }

    pub fn set_tempo(&mut self, tempo: f64) {
        let tempo = tempo.clamp(sequencer::MIN_TEMPO, sequencer::MAX_TEMPO);
        if tempo != self.sequencer.tempo {
            self.sequencer.tempo = tempo;
            self.sequencer_changed();
        }
    }

    pub fn tempo(&self) -> f64 {
        self.sequencer.tempo
    }

    pub fn pattern(&self) -> Pattern {
        self.sequencer.pattern.clone()
    }

    /// **The gesture funnel.** Input handling may only act by dispatching a [`Gesture`] here —
    /// the same one-funnel rule `parameter_edited` carries, for the same reason: an act that
    /// bypasses the funnel is an act the conformance sweep cannot see. Each arm delegates to the
    /// method the CLI's verbs also reach, which is what keeps a gesture and its verb one
    /// behaviour rather than two that agree today.
    ///
    /// One exception is documented rather than hidden: the panel's double-click reset arrives as
    /// a widget `ControlOutcome` inside the parameter row's draw loop and keeps its in-place
    /// route; [`GestureKind::ResetParam`] still names the act, and its arm is the same
    /// `reset_parameter` the CLI's `reset` verb calls.
    pub fn perform(&mut self, gesture: Gesture) {
        match gesture {
            Gesture::ClickStep { step } => self.select_step(step),
            Gesture::ShiftClickStep { step } => self.shift_select_step(step),
            Gesture::CtrlClickStep { step } => self.ctrl_select_step(step),
            Gesture::EscapeKey => self.deselect_step(),
            Gesture::SpaceKey => self.play_stop(),
            Gesture::CopyShortcut => {
                if self.selected_step.is_some() {
                    self.copy_steps();
                } else if self.tools_on_pattern {
                    self.copy_pattern();
                } else {
                    self.copy_bar();
                }
            }
            Gesture::CutShortcut => {
                if self.selected_step.is_some() {
                    self.cut_steps();
                } else {
                    if self.tools_on_pattern {
                        self.copy_pattern();
                        self.clear_pattern_bars();
                    } else {
                        self.copy_bar();
                        self.clear_bar();
                    }
                    self.status = Some("cut".to_owned());
                }
            }
            Gesture::PasteShortcut => self.paste_clipboard(),
            Gesture::ResetParam { param_id } => self.reset_parameter(param_id),
            Gesture::PlayNote { key } => self.note_on(key, 100.0 / 127.0),
            Gesture::ReleaseNote { key } => self.note_off(key),
            Gesture::OctaveShift { by } => self.shift_octave(by),
        }
    }

    /// Grows the sequence to reach `step`, **because content is arriving there**.
    ///
    /// The owner's rule: navigation never adds bars — you can wander to pattern twelve, look
    /// around and leave — but the moment a note, a tie or a lock lands in a bar beyond the end,
    /// the sequence is that many bars and the counter says so. Every write funnel calls this
    /// first, not least because reads and writes past the pattern's length wrap silently.
    fn grow_to_reach(&mut self, step: usize) {
        let spb = self.sequencer.pattern.steps_per_bar();
        if step >= self.sequencer.pattern.len() {
            self.sequencer.pattern.set_bars(step / spb + 1);
        }
    }

    /// Toggles `note` on `step` — **the step itself**, tied or not.
    ///
    /// A pitch played into a tied step lands there, turning a hold into a **slide** — the gate
    /// stays open, the pitch moves. To repitch a held note, select its head.
    ///
    /// **Nothing here is guarded, and nothing else is touched.** Two guards used to live in this
    /// funnel — *a slide needs a head*, and *the head is protected while a slide continues it* —
    /// along with a repair that untied whatever a deletion had stranded. All three are gone with
    /// the tie invariant they served (the owner's ruling; see `apps/mxm-player/NOTES.md`, *Ties
    /// and notes are independent*): a tie reaching no note is a run with nothing to hold yet,
    /// which the runtime already plays as silence until a note arrives. So a note lands wherever
    /// it is played, a note leaves whenever it is deleted, and the ties around it are left as the
    /// author wrote them.
    ///
    /// Every input path arrives here — on-screen keys, computer keyboard, MIDI drain and the CLI's
    /// `toggle` — which is what keeps that one rule instead of four.
    pub fn toggle_step_note(&mut self, step: usize, note: u8) {
        self.grow_to_reach(step);
        self.sequencer.pattern.toggle(step, note);
        self.sequencer_changed();
        self.persist();
    }

    /// Flips whether a step **continues the note before it**.
    ///
    /// Tying an empty step makes a **hold**; tying a step that carries a pitch makes a **slide**,
    /// which keeps its notes and its locks — they belong to a run it now heads. **Unguarded**, and
    /// the same in both directions: a tie is a fact about its own step, so it can be written
    /// before the note it will continue, and the step's notes are never consulted or changed.
    pub fn toggle_step_tie(&mut self, step: usize) {
        self.grow_to_reach(step);
        let tying = !self.sequencer.pattern.tied(step);
        self.set_step_tie(step, tying);
        self.sequencer_changed();
        self.persist();
    }

    /// The step the runtime is told is being edited.
    ///
    /// **[`Self::editing_step`]**, which is the raw selection now that a pitch and a lock both
    /// land on the step itself — the runtime previews that step's own locks, per step like
    /// everything else. One notion of "the step being edited", or the screen and the sound
    /// disagree.
    ///
    /// A named method rather than an expression inline in `publish_sequencer`, so that a test can
    /// assert on exactly what is published rather than on a copy of the same reasoning.
    #[doc(hidden)]
    pub fn published_editing_step(&self) -> Option<u32> {
        // `u32`, because `u8` here was a silent 255-step cap on previewing a selected step —
        // exactly the kind of ceiling the no-maximum-length rule forbids.
        self.editing_step()
            .and_then(|step| u32::try_from(step).ok())
    }

    /// Ties a step directly, for tests that want the state without the selection dance.
    #[doc(hidden)]
    pub fn force_tie_for_test(&mut self, step: usize) {
        self.sequencer.pattern.set_tied(step, true);
    }

    /// Sets how many bars the sequence plays.
    ///
    /// **Shrinking deletes**, which is the whole of the resize rule: the bars that fall outside go,
    /// with their notes, their ties **and their locks**. The pattern drops the first two; the locks
    /// live in a different structure and are dropped here, because nothing else can see both.
    ///
    /// A lock left on a step the sequence no longer has would be stored, saved, unreachable — and
    /// would come back the moment somebody lengthened the sequence again. That is the shape of
    /// defect the tied-step rule already exists to prevent.
    pub fn set_bars(&mut self, bars: usize) {
        self.sequencer.pattern.set_bars(bars);
        self.drop_locks_past_the_end();
        self.selected_bar = self
            .selected_bar
            .min(self.sequencer.pattern.bars().saturating_sub(1));
        self.sequencer_changed();
        self.persist();
    }

    /// Sets how many steps each bar holds. A step stays a sixteenth, so twelve is a 3/4 bar.
    ///
    /// **Cells keep their `(bar, step)` coordinate**, so this is not the same as changing the total
    /// length — and what falls outside goes, exactly as [`Self::set_bars`] describes.
    pub fn set_steps_per_bar(&mut self, steps: usize) {
        self.sequencer.pattern.set_steps_per_bar(steps);
        self.drop_locks_past_the_end();
        // A clipboard copied at the old shape would have to be truncated or padded to paste, and
        // both silently reshape music. Discarding cannot lie.
        self.bar_clipboard = None;
        self.sequencer_changed();
        self.persist();
    }

    /// Shows a bar in the step row, deselecting a step that would no longer be visible.
    ///
    /// Also what `Loop [Bar]` follows: in bar scope the selected bar is the sequence, and the
    /// runtime queues the change for the current bar's end when it arrives mid-play.
    pub fn select_bar(&mut self, bar: usize) {
        // **Not clamped to the bars that exist.** Viewing is free; a bar materialises only when
        // content lands in it. The step row draws a virtual bar empty, and the first note into it
        // is what grows the sequence.
        if bar == self.selected_bar {
            return;
        }
        self.selected_bar = bar;
        // A selected step outside the shown bar would be edited blind — and the whole selection
        // lives in one bar, so leaving the bar drops all of it.
        if self
            .selected_step
            .is_some_and(|step| step / self.sequencer.pattern.steps_per_bar() != bar)
        {
            self.deselect_step();
        }
        self.also_selected
            .retain(|step| step / self.sequencer.pattern.steps_per_bar() == bar);
        self.sequencer_changed();
    }

    pub fn selected_bar(&self) -> usize {
        self.selected_bar
    }

    /// `Loop [ Bar | Pattern | All ]`. Transport state: queued to the current bar's end by the
    /// runtime when it changes mid-play, never persisted, ignored by a render.
    pub fn set_loop_scope(&mut self, scope: LoopScope) {
        if self.loop_scope == scope {
            return;
        }
        self.loop_scope = scope;
        self.sequencer_changed();
    }

    pub fn loop_scope(&self) -> LoopScope {
        self.loop_scope
    }

    /// Copies the selected bar: notes, ties, locks, and the plugin the locks belong to.
    pub fn copy_bar(&mut self) {
        self.copy_bars(self.selected_bar, 1);
        self.status = Some(format!("bar {} copied", self.selected_bar + 1));
    }

    /// Copies the selected bar's whole eight-bar pattern.
    pub fn copy_pattern(&mut self) {
        let first = self.selected_bar / PATTERN_BARS * PATTERN_BARS;
        self.copy_bars(first, PATTERN_BARS);
        self.status = Some(format!(
            "bars {}-{} copied",
            first + 1,
            first + PATTERN_BARS
        ));
    }

    /// Copies `count` bars from `first_bar` into the clipboard.
    ///
    /// A bar beyond the end copies as empty — viewing is free, and a pattern copied at the edge
    /// of the sequence is what it looks like: its real bars, then silence.
    fn copy_bars(&mut self, first_bar: usize, count: usize) {
        let pattern = &self.sequencer.pattern;
        let spb = pattern.steps_per_bar();
        let existing = pattern.bars();
        let mut bars = Vec::with_capacity(count);
        for bar in first_bar..first_bar + count {
            if bar >= existing {
                bars.push(ClipBar {
                    steps: vec![sequencer::Step::EMPTY; spb],
                    tied: vec![false; spb],
                    locks: Vec::new(),
                });
                continue;
            }
            let first = bar * spb;
            let mut locks = Vec::new();
            for param_id in self.sequencer.locks.param_ids() {
                let patch = self.sequencer.locks.patch(param_id);
                for offset in 0..spb {
                    if let Some(value) = self.sequencer.locks.get(first + offset, param_id) {
                        locks.push((offset, param_id, value, patch));
                    }
                }
            }
            bars.push(ClipBar {
                steps: (0..spb)
                    .map(|offset| pattern.step(first + offset))
                    .collect(),
                tied: (0..spb)
                    .map(|offset| pattern.tied(first + offset))
                    .collect(),
                locks,
            });
        }
        self.bar_clipboard = Some(BarClipboard {
            content: ClipContent::Bars(bars),
            plugin: self.engine.plugin_id().map(str::to_owned),
            steps_per_bar: spb,
        });
        self.offer_to_system_clipboard();
    }

    /// Writes what was just copied onto the **system** clipboard, as text.
    ///
    /// **This is what makes Ctrl+V arrive at all**, and it is not a courtesy. `egui-winit` turns
    /// the paste chord into an `Event::Paste` **only when the system clipboard reads back as
    /// non-empty text** — on an empty clipboard, or one holding a file or an image, the chord is
    /// swallowed and no event of any kind is emitted. A copy that left the system clipboard alone
    /// would therefore make Ctrl+V work or not work depending on what the person last copied in
    /// another application, with nothing on screen to explain it.
    ///
    /// **The text is an export, never the thing that pastes.** Paste applies `bar_clipboard`,
    /// which carries notes, ties *and* locks; a lock is a parameter identity and a value, and no
    /// step-name line could round-trip one. So this is deliberately a legible record — the same
    /// step tokens the CLI speaks — for pasting into a note, a message or a test.
    fn offer_to_system_clipboard(&mut self) {
        if let Some(text) = self.clipboard_text() {
            self.pending_clipboard_text = Some(text);
        }
    }

    /// The copied content as lines of step tokens: a note name, `.` for a rest, `~` for a hold,
    /// `~NOTE` for a slide.
    ///
    /// One line per bar, or a single line for a copied run of steps. A chord joins its notes with
    /// `+` — patterns hold them, because a `.mid` import can write one. A slide keeps its notes
    /// behind the `~`: a bare `~` for a tied step carrying a pitch would export a hold where the
    /// music has a pitch change, which is a different bassline.
    fn clipboard_text(&self) -> Option<String> {
        fn token(step: crate::sequencer::Step, tied: bool) -> String {
            let notes = || {
                step.notes()
                    .into_iter()
                    .map(crate::sequencer::pattern::note_name)
                    .collect::<Vec<_>>()
                    .join("+")
            };
            if tied && step.is_empty() {
                return "~".to_owned();
            }
            if tied {
                return format!("~{}", notes());
            }
            if step.is_empty() {
                return ".".to_owned();
            }
            notes()
        }

        let clipboard = self.bar_clipboard.as_ref()?;
        Some(match &clipboard.content {
            ClipContent::Bars(bars) => bars
                .iter()
                .map(|bar| {
                    bar.steps
                        .iter()
                        .zip(&bar.tied)
                        .map(|(step, tied)| token(*step, *tied))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            ClipContent::Steps(steps) => steps
                .iter()
                .map(|step| token(step.step, step.tied))
                .collect::<Vec<_>>()
                .join(" "),
        })
    }

    /// Copies the selected steps as a dense run: notes, ties and locks, in step order.
    pub fn copy_steps(&mut self) {
        let selection = self.selected_steps();
        if selection.is_empty() {
            self.status = Some("select steps to copy".to_owned());
            return;
        }
        let total = self.sequencer.pattern.len();
        let steps: Vec<ClipStep> = selection
            .iter()
            .map(|index| {
                if *index >= total {
                    return ClipStep {
                        step: sequencer::Step::EMPTY,
                        tied: false,
                        locks: Vec::new(),
                    };
                }
                let locks =
                    self.sequencer
                        .locks
                        .param_ids()
                        .into_iter()
                        .filter_map(|param_id| {
                            self.sequencer.locks.get(*index, param_id).map(|value| {
                                (param_id, value, self.sequencer.locks.patch(param_id))
                            })
                        })
                        .collect();
                ClipStep {
                    step: self.sequencer.pattern.step(*index),
                    tied: self.sequencer.pattern.tied(*index),
                    locks,
                }
            })
            .collect();
        let count = steps.len();
        self.bar_clipboard = Some(BarClipboard {
            content: ClipContent::Steps(steps),
            plugin: self.engine.plugin_id().map(str::to_owned),
            steps_per_bar: self.sequencer.pattern.steps_per_bar(),
        });
        self.offer_to_system_clipboard();
        self.status = Some(format!(
            "{count} step{} copied",
            if count == 1 { "" } else { "s" }
        ));
    }

    /// Ctrl+X: copies the selected steps, then empties them.
    pub fn cut_steps(&mut self) {
        let selection = self.selected_steps();
        if selection.is_empty() {
            self.status = Some("select steps to cut".to_owned());
            return;
        }
        self.copy_steps();
        let total = self.sequencer.pattern.len();
        let count = selection.len();
        for index in selection {
            if index >= total {
                continue;
            }
            self.sequencer
                .pattern
                .set_step(index, sequencer::Step::EMPTY);
            self.sequencer.pattern.set_tied(index, false);
            self.sequencer.locks.clear_step(index);
        }
        self.sequencer_changed();
        self.locks_unsaved = true;
        self.persist();
        self.status = Some(format!(
            "{count} step{} cut",
            if count == 1 { "" } else { "s" }
        ));
    }

    /// Pastes a copied run of steps over the steps starting at the anchor.
    ///
    /// **Paste replaces**, exactly as pasting over a selected word does: each target step's notes,
    /// tie and locks are overwritten by the clipboard's — a tied target stops being tied unless
    /// the pasted step ties. A run longer than the bar wraps on into the next, growing the
    /// sequence if the pasted content reaches beyond its end.
    fn paste_steps(&mut self) {
        let Some(anchor) = self.selected_step else {
            self.status = Some("select a step to paste at".to_owned());
            return;
        };
        let clipboard = self.bar_clipboard.take().expect("caller checked");
        let ClipContent::Steps(steps) = &clipboard.content else {
            unreachable!("caller matched Steps");
        };
        let loaded = self.engine.plugin_id().map(str::to_owned);
        let locks_travel = match (clipboard.plugin.as_deref(), loaded.as_deref()) {
            (Some(recorded), Some(now)) => recorded == now,
            _ => false,
        };
        if let Some(reach) = steps
            .iter()
            .enumerate()
            .filter(|(_, step)| step.holds_content())
            .map(|(index, _)| index)
            .next_back()
        {
            self.grow_to_reach(anchor + reach);
        }
        let mut dropped = false;
        let total = self.sequencer.pattern.len();
        for (index, incoming) in steps.iter().enumerate() {
            let target = anchor + index;
            if target >= total {
                break; // beyond the growth: the clipboard's empty tail writes nothing
            }
            self.sequencer.pattern.set_step(target, incoming.step);
            self.sequencer.pattern.set_tied(target, incoming.tied);
            self.sequencer.locks.clear_step(target);
            if locks_travel {
                for (param_id, value, patch) in &incoming.locks {
                    let patch = patch.unwrap_or(*value);
                    if self
                        .sequencer
                        .locks
                        .set(target, *param_id, *value, patch)
                        .is_err()
                    {
                        dropped = true;
                    }
                }
            } else {
                dropped |= !incoming.locks.is_empty();
            }
        }
        let count = steps.len();
        self.bar_clipboard = Some(clipboard);
        self.sequencer_changed();
        self.locks_unsaved = true;
        self.persist();
        self.status = Some(if dropped {
            format!("{count} steps pasted; locks belonging to another instrument were dropped")
        } else {
            format!("{count} step{} pasted", if count == 1 { "" } else { "s" })
        });
    }

    /// Replaces bars with the clipboard, from the selected bar (or its pattern's start, when a
    /// pattern was copied): notes and ties always, locks only when the plugin still matches — a
    /// parameter id means nothing outside the instrument that assigned it. Refuses a clipboard
    /// copied at a different bar shape rather than reshaping it.
    ///
    /// **Pasting is content**: the sequence grows to reach the last pasted bar that holds
    /// anything, and no further — empty tail bars in the clipboard add nothing.
    pub fn paste_clipboard(&mut self) {
        let spb = self.sequencer.pattern.steps_per_bar();
        let Some(clipboard) = self.bar_clipboard.as_ref() else {
            self.status = Some("nothing copied yet".to_owned());
            return;
        };
        if matches!(clipboard.content, ClipContent::Steps(_)) {
            self.paste_steps();
            return;
        }
        if clipboard.steps_per_bar != spb {
            self.status = Some(format!(
                "the clipboard holds {}-step bars and a bar is {spb} steps now; copy again",
                clipboard.steps_per_bar
            ));
            return;
        }
        let ClipContent::Bars(bars) = &clipboard.content else {
            unreachable!("steps handled above");
        };
        let count = bars.len();
        let first_bar = if count == 1 {
            self.selected_bar
        } else {
            self.selected_bar / PATTERN_BARS * PATTERN_BARS
        };
        let loaded = self.engine.plugin_id().map(str::to_owned);
        let locks_travel = match (clipboard.plugin.as_deref(), loaded.as_deref()) {
            (Some(recorded), Some(now)) => recorded == now,
            _ => false,
        };

        let clipboard = self.bar_clipboard.take().expect("checked above");
        let ClipContent::Bars(bars) = &clipboard.content else {
            unreachable!("steps handled above");
        };
        if let Some(reach) = bars
            .iter()
            .enumerate()
            .filter(|(_, bar)| bar.holds_content())
            .map(|(index, _)| index)
            .next_back()
        {
            self.grow_to_reach((first_bar + reach + 1) * spb - 1);
        }

        let mut dropped = false;
        let existing = self.sequencer.pattern.bars();
        for (index, bar) in bars.iter().enumerate() {
            let target = first_bar + index;
            if target >= existing {
                break; // a virtual bar being overwritten with nothing stays nothing
            }
            let first = target * spb;
            for offset in 0..spb {
                self.sequencer
                    .pattern
                    .set_step(first + offset, bar.steps[offset]);
                self.sequencer
                    .pattern
                    .set_tied(first + offset, bar.tied[offset]);
                self.sequencer.locks.clear_step(first + offset);
            }
            if locks_travel {
                for (offset, param_id, value, patch) in &bar.locks {
                    let patch = patch.unwrap_or(*value);
                    if self
                        .sequencer
                        .locks
                        .set(first + offset, *param_id, *value, patch)
                        .is_err()
                    {
                        dropped = true;
                    }
                }
            } else {
                dropped |= !bar.locks.is_empty();
            }
        }
        self.bar_clipboard = Some(clipboard);
        self.sequencer_changed();
        self.locks_unsaved = true;
        self.persist();
        let what = if count == 1 {
            format!("bar {}", first_bar + 1)
        } else {
            format!("bars {}-{}", first_bar + 1, first_bar + count)
        };
        self.status = Some(if dropped {
            format!("{what} pasted; locks belonging to another instrument were dropped")
        } else {
            format!("{what} pasted")
        });
    }

    /// Empties the selected bar: notes, ties and locks — the same three things a paste replaces.
    pub fn clear_bar(&mut self) {
        self.clear_bars(self.selected_bar, 1);
        self.status = Some(format!("bar {} cleared", self.selected_bar + 1));
    }

    /// Empties the selected bar's whole eight-bar pattern.
    pub fn clear_pattern_bars(&mut self) {
        let first = self.selected_bar / PATTERN_BARS * PATTERN_BARS;
        self.clear_bars(first, PATTERN_BARS);
        self.status = Some(format!(
            "bars {}-{} cleared",
            first + 1,
            first + PATTERN_BARS
        ));
    }

    fn clear_bars(&mut self, first_bar: usize, count: usize) {
        let spb = self.sequencer.pattern.steps_per_bar();
        let first = first_bar * spb;
        let end = (first_bar + count) * spb;
        self.edit_stored_locks(|steps| {
            let end = end.min(steps.len());
            if first < end {
                steps[first..end].fill(None);
            }
        });
        let existing = self.sequencer.pattern.bars();
        for bar in first_bar..(first_bar + count).min(existing) {
            let first = bar * spb;
            for offset in 0..spb {
                self.sequencer
                    .pattern
                    .set_step(first + offset, sequencer::Step::EMPTY);
                self.sequencer.pattern.set_tied(first + offset, false);
                self.sequencer.locks.clear_step(first + offset);
            }
        }
        self.sequencer_changed();
        self.locks_unsaved = true;
        self.persist();
    }

    /// Destructive edits apply to unresolved and parked cells as well as live ones.
    fn edit_stored_locks(&mut self, edit: impl Fn(&mut Vec<Option<f32>>)) {
        let update = |entries: &mut Vec<crate::sequencer::locks::LockedParam>| {
            for entry in entries.iter_mut() {
                edit(&mut entry.steps);
            }
            entries.retain(|entry| entry.steps.iter().any(Option::is_some));
        };
        update(&mut self.pending_fx_locks);
        if let Some(parked) = &mut self.parked_locks {
            update(&mut parked.params);
        }
    }

    /// Drops locks on steps the sequence no longer has.
    ///
    /// Returns how many live locks were cleared.
    fn drop_locks_past_the_end(&mut self) -> usize {
        let length = self.sequencer.pattern.len();
        let cleared = self.sequencer.locks.truncate(length);
        self.edit_stored_locks(|steps| steps.truncate(length));
        if self.selected_step.is_some_and(|step| step >= length) {
            self.deselect_step();
        }
        cleared
    }

    /// Sets whether `step` continues the note before it.
    ///
    /// **Every path that ties a step goes through here**, and tying no longer empties anything: a
    /// step that carries a pitch becomes a slide and keeps its notes, and locks are per step
    /// (tied ones included), so what a step sets survives whatever its gate does. The emptying
    /// rule this funnel used to enforce defended the old invariant — *a tied step is empty* — and
    /// went with it.
    ///
    /// **Untying repairs the slides it strands.** Breaking a joint mid-chain — untying a hold
    /// between a head and a slide — leaves the slide with nothing to slide from, a state every
    /// authoring path refuses; it becomes an ordinary note, visibly, notes and locks kept.
    /// Empty ties stranded the same way stay: a half-built pattern is legal, and untying has
    /// always been allowed to leave one.
    fn set_step_tie(&mut self, step: usize, tied: bool) {
        self.sequencer.pattern.set_tied(step, tied);
    }

    pub fn selected_step(&self) -> Option<usize> {
        self.selected_step
    }

    /// What a second click on `step` will do.
    ///
    /// **The click ties the step you clicked to the note in front of it** — one rule, the same at
    /// every position in the row, and the same direction the flag itself points. `tied[n]` has
    /// always meant "step *n* continues the note before it", so the gesture now sets exactly the
    /// boolean it names, on exactly the step under the pointer.
    ///
    /// **The step's own tie flag is checked first**, and that precedence is what keeps a slide
    /// unambiguous — a slide both holds notes and is tied, so without it two rows would claim the
    /// same step. A tied step's click means *break the joint here*: a hold becomes a rest, a slide
    /// becomes an ordinary note, notes and locks kept either way.
    ///
    /// **A step holding a note ties into a slide**, and needs nothing in front of it to do so —
    /// the delegation this replaced existed to stop a note on step 13 tying itself to the rest on
    /// step 12, and that state is legal now, silent until a note lands in front of it. Lengthening
    /// a note is clicking the step you want it to swallow, which is the step whose flag changes.
    fn tie_action(&self, step: usize) -> TieAction {
        let pattern = &self.sequencer.pattern;
        if pattern.tied(step) {
            return TieAction::Untie(step);
        }
        if pattern.step(step).is_empty() {
            return TieAction::Tie(step);
        }
        TieAction::Slide(step)
    }

    /// Selects a step; **clicking the one already selected ties it to the note in front of it**.
    ///
    /// While a step is selected, every keyboard — on-screen, computer and MIDI — writes into it
    /// rather than only playing. That is the whole of pitch entry, and a click has no pitch to
    /// offer, so a click can only ever say *how long*, never *what*.
    ///
    /// The second click used to deselect. It changes the length instead, because that gesture was
    /// the only one free and a tie needs no new affordance, no modifier and no extra row. **The
    /// first click on any step still only selects**, so reaching for a step to type a note into it
    /// can never change a length by accident. Deselecting is Escape, or clicking a different step.
    ///
    /// [`PlayerApp::tie_action`] decides what the second click does, and the panel's hint line
    /// reads from the same function — a gesture whose meaning depends on what a step holds is only
    /// usable if the text describing it is derived from the same answer. **What it never depends on
    /// is where in the row the step sits**: the click ties the step under it, first to last, and
    /// lengthening a note is clicking the step you want that note to swallow.
    /// Every step in the selection, the anchor first among equals: ascending, empty when nothing
    /// is selected.
    pub fn selected_steps(&self) -> Vec<usize> {
        let Some(anchor) = self.selected_step else {
            return Vec::new();
        };
        let mut steps: Vec<usize> = std::iter::once(anchor)
            .chain(self.also_selected.iter().copied())
            .collect();
        steps.sort_unstable();
        steps.dedup();
        steps
    }

    /// Shift-click: the selection becomes the range from the anchor to `step`, inclusive — text
    /// selection's gesture. Without an anchor it is a plain selection.
    pub fn shift_select_step(&mut self, step: usize) {
        let Some(anchor) = self.selected_step else {
            self.select_step(step);
            return;
        };
        let (low, high) = (anchor.min(step), anchor.max(step));
        self.also_selected = (low..=high).filter(|s| *s != anchor).collect();
        self.sequencer_changed();
    }

    /// Ctrl-click: toggles one step in or out of the selection, contiguous or not. Toggling the
    /// anchor off promotes the lowest companion; toggling the last step off deselects.
    pub fn ctrl_select_step(&mut self, step: usize) {
        let Some(anchor) = self.selected_step else {
            self.select_step(step);
            return;
        };
        if step == anchor {
            if let Some(next) = self.also_selected.pop_first() {
                self.selected_step = Some(next);
            } else {
                self.deselect_step();
                return;
            }
        } else if !self.also_selected.remove(&step) {
            self.also_selected.insert(step);
        }
        self.sequencer_changed();
    }

    pub fn select_step(&mut self, step: usize) {
        self.also_selected.clear();
        if self.selected_step != Some(step) {
            // Moving to a different step is leaving the old one, exactly as deselecting is: a drag
            // that never closed has no step to commit into any more, and a base parked at the old
            // step's value would sit under the new step's preview sounding a value that belongs to
            // neither the patch nor the step now open.
            self.pending_step_edit.clear();
            self.unpark_bases();
            self.selected_step = Some(step);
            // The runtime previews the selected step, so it has to be told.
            self.sequencer_changed();
            // At rest, the newly selected step's locks park onto the base, so the plugin's
            // editor shows them and its controls stay operable — see `park_selected_step_locks`.
            self.park_selected_step_locks();
            return;
        }

        match self.tie_action(step) {
            TieAction::Untie(at) => {
                self.set_step_tie(at, false);
            }
            TieAction::Tie(at) | TieAction::Slide(at) => {
                self.set_step_tie(at, true);
            }
        }
        self.sequencer_changed();
        self.persist();
    }

    /// Stops editing a step, without touching it.
    pub fn deselect_step(&mut self) {
        let was_editing = self.selected_step.is_some();
        self.selected_step = None;
        self.also_selected.clear();
        // A drag that never closed its gesture has nothing to commit into: the step it was for is
        // no longer being edited. Dropped rather than committed, because committing on deselect
        // would write a lock from a gesture the person has not finished.
        self.pending_step_edit.clear();
        // **Leaving the step is when the instrument returns to the patch** — the one jump, at a
        // moment the person chose. Every parked base goes back, and the runtime's preview for the
        // *other* locked parameters comes off with the republish below.
        self.unpark_bases();
        // **Nothing is sent from here.** The runtime is told which step is being edited and applies
        // the preview itself, so leaving one is just a republish with none — and the offsets come off
        // in the same place they went on.
        if was_editing {
            self.sequencer_changed();
        }
    }

    /// Empties the pattern **and everything its steps set**.
    ///
    /// Clearing the notes but keeping the locks would leave a pattern that looks empty and still
    /// moves the filter every bar, with nothing on screen to say why — a trap, and the reason this
    /// takes both.
    pub fn clear_pattern(&mut self) {
        self.sequencer.pattern.clear();
        self.pending_fx_locks.clear();
        self.parked_locks = None;
        self.pending_step_edit.clear();
        self.unpark_bases();
        // **The runtime takes the offsets off, not this.** It sees each parameter leave its lock
        // set when the new state arrives and owes it a zero — the only place the ordering can be
        // right, and the only way there is one owner of an applied offset rather than two that can
        // disagree. What is dropped here is just this side's record of them.
        self.sequencer.locks.clear_all();
        self.modulation.clear();
        self.sequencer_changed();
        self.persist();
    }

    /// Fills **the shown bar** with a monophonic sequence in C Dorian.
    ///
    /// **The bar, not the sequence.** Replacing the whole pattern made Random unusable past bar
    /// one: selecting bar two and pressing it threw the sequence away and wrote a single bar back
    /// — the reported fault. A bar is also the unit every other tool here works in, so Random
    /// composes with them: roll bar one, keep it, move on and roll bar two.
    ///
    /// **It grows the sequence to reach the bar**, because notes are landing in it. That is the
    /// same rule a note or a lock written into a virtual bar follows ([`Self::grow_to_reach`]) —
    /// bars materialise on content, never on navigation.
    ///
    /// **Locks are kept — all of them.** Unlike Clear, this is not a request to start again:
    /// rolling new notes under a filter sweep you built is the whole use, and it is what makes the
    /// button worth pressing twice. Locks are per step now, tied steps included, so there is no
    /// longer an exception for a step the roll ties — a lock there is heard there.
    pub fn randomise(&mut self) {
        let spb = self.sequencer.pattern.steps_per_bar();
        let first = self.selected_bar * spb;
        // Before any write: past the pattern's length `set_step` wraps silently, so a bar beyond
        // the end would otherwise be rolled into bar one — which is exactly the reported fault.
        self.grow_to_reach(first + spb - 1);

        self.random_seed = self
            .random_seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let bar = sequencer::random_sequence(self.random_seed, spb);
        for offset in 0..spb {
            // Notes and the tie together: a generated step carrying both is a slide, which the
            // generator writes only where a note already sounds — the same invariant the funnels
            // guard.
            self.sequencer
                .pattern
                .set_step(first + offset, bar.step(offset));
            self.set_step_tie(first + offset, bar.tied(offset));
        }
        self.status = Some(format!("bar {} randomised", self.selected_bar + 1));
        self.sequencer_changed();
        self.persist();
    }

    // --- saving ------------------------------------------------------------------------------

    pub fn sequence_dir(&self) -> &Path {
        &self.sequence_dir
    }

    pub fn sequence_problems(&self) -> &[String] {
        &self.sequence_problems
    }

    pub fn exports_dir(&self) -> &Path {
        &self.exports_dir
    }

    pub fn normalise_export(&self) -> bool {
        self.normalise_export
    }

    pub fn set_normalise_export(&mut self, on: bool) {
        self.normalise_export = on;
    }

    /// Renders the sequence through the loaded plugin and writes a `.wav`.
    ///
    /// **A snapshot, not a fresh plugin.** The plugin's CLAP state is captured here, on the GUI
    /// thread, and restored into a *separate* instance — otherwise the file would carry the
    /// plugin's default patch rather than the sound being listened to. Live playback is untouched.
    ///
    /// Capturing state is a synchronous call into the plugin, so a plugin that hangs there hangs
    /// the interface. That is inherited, not introduced: `NOTES.md` (*Fault isolation is partial*)
    /// records that no in-process timeout recovers it.
    pub fn export_audio(&mut self, name: &str) -> Result<PathBuf, String> {
        let Some(plugin_id) = self.engine.plugin_id().map(str::to_owned) else {
            return Err("load a plugin before exporting audio".to_owned());
        };
        let Some(bundle) = self.engine.bundle().map(Path::to_path_buf) else {
            return Err("load a plugin before exporting audio".to_owned());
        };

        // Every edit the panel has accepted must be in the file. Parameter changes reach the plugin
        // through the audio thread, so the snapshot has to wait for them.
        self.settle_pending_edits();

        let state = self
            .engine
            .capture_state()
            .map_err(|reason| format!("this plugin's sound cannot be exported: {reason}"))?;

        let tempo = self.sequencer.tempo;
        let sample_rate = self.engine.sample_rate().unwrap_or(48_000.0);
        let pattern = self.sequencer.pattern.clone();
        let normalise = self.normalise_export;

        // The chain travels too: every effect that is on, with its patch captured the way the
        // source's is. One that is off is left out, as the live path leaves it uncalled; one
        // without the state extension renders its default patch, the only patch it has.
        let mut effects = Vec::new();
        // **The ids in the same order as the specs**, so the export can put each effect's
        // automation on the effect it belongs to. A bypassed effect is left out of both, exactly as
        // the live path leaves it uncalled — and its automation goes with it rather than sliding
        // onto its neighbour.
        let mut fx_order = Vec::new();
        for (index, info) in self.engine.fx_info().into_iter().enumerate() {
            if info.bypassed {
                continue;
            }
            let state = self.engine.capture_fx_state(index).ok();
            fx_order.push(info.id);
            effects.push(crate::offline::EffectSpec {
                bundle: info.bundle,
                plugin_id: info.plugin_id,
                state,
                events: Vec::new(),
            });
        }

        // The locks travel with the pattern: an export that dropped them would sound like the
        // patch, which is exactly the failure the state transfer above exists to prevent.
        let render = sequencer::export::render_offline(
            &bundle,
            &plugin_id,
            state,
            pattern,
            self.sequencer.locks,
            tempo,
            sample_rate,
            normalise,
            &effects,
            &fx_order,
        )?;

        let stem =
            sequencer::export::file_name(name, render.tempo, self.sequencer.pattern.bars() as u32);
        let path = unique_path(&self.exports_dir, &stem, "wav");
        sequencer::export::write_wav(&render, &path)?;

        self.status = Some(format!(
            "Saved {} — {}, {:.2}s{}",
            path.display(),
            render.ending.label(),
            render.seconds(),
            if render.normalised {
                ", normalised"
            } else {
                ""
            }
        ));
        Ok(path)
    }

    /// Waits for queued parameter edits to reach the plugin.
    ///
    /// The panel can be showing a value the plugin has not consumed — edits travel through the
    /// audio thread. Without this, pressing Save straight after moving a slider exports the value
    /// from before the move.
    fn settle_pending_edits(&mut self) {
        for _ in 0..8 {
            self.service();
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        self.refresh_params();
    }

    /// Saves the pattern and tempo as a Standard MIDI File.
    ///
    /// **MIDI, not JSON.** A `.mid` goes into a DAW; the JSON format stays for repository fixtures,
    /// where being readable and diffable is the whole point. Tempo is canonicalised to the value
    /// SMF can hold, so the file round-trips exactly.
    pub fn save_midi(&mut self, name: &str) -> Result<PathBuf, String> {
        let tempo = sequencer::smf::canonical_tempo(self.sequencer.tempo);
        let bytes = sequencer::smf::write(&self.sequencer.pattern, tempo);
        let path = unique_path(&self.exports_dir, name, "mid");
        write_atomically(&path, &bytes)?;

        // **Reported on save, not only on load.** The read path has always named what it could not
        // keep; without this, a sequence saved as MIDI would lose its locks silently and the loss
        // would surface only much later, in a file that plays the right notes wrongly.
        if !self.sequencer.locks.param_ids().is_empty() {
            self.sequence_problems = vec![
                "a MIDI file cannot carry parameter locks; save as a sequence to keep them"
                    .to_owned(),
            ];
        }

        // The file holds what the file holds: keep the live tempo in step with it, or saving and
        // reloading would move the tempo by a rounding error.
        if tempo != self.sequencer.tempo {
            self.sequencer.tempo = tempo;
            self.sequencer_changed();
        }
        // **A save reports what it could not keep, exactly as a load does.** A legato joint and a
        // run that wraps the loop point are both things MIDI cannot hold; letting somebody find
        // either by ear afterwards is the silent rewrite this line prevents.
        self.sequence_problems = sequencer::smf::write_report(&self.sequencer.pattern);

        self.status = Some(format!("Saved {}", path.display()));
        Ok(path)
    }

    /// Saves a sequence as JSON. **For tests and fixtures**, not a user-facing control.
    pub fn save_sequence(&mut self, name: &str) -> Result<PathBuf, String> {
        let path = sequencer::sequence::path_for(&self.sequence_dir, name);
        let mut sequence =
            sequencer::Sequence::new(name, &self.sequencer.pattern, self.sequencer.tempo);
        let data = self.capture_locks()?;
        sequence.locks = (!data.params.is_empty()).then_some(data);
        sequence.save(&path)?;
        self.status = Some(format!("Saved {}", path.display()));
        Ok(path)
    }

    /// Loads a `.mid`, replacing the pattern and tempo.
    ///
    /// Refuses rather than reinterprets: anything the sixteen-step model cannot hold comes back as
    /// a reason, and **the current sequence is left untouched**. What it *can* hold but not keep —
    /// velocity, note length, channel, duplicate pitches — is named before the overwrite.
    pub fn load_midi(&mut self, path: &Path) -> Result<(), String> {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("{} could not be read: {e}", path.display()))?;
        let loaded = sequencer::smf::read(&bytes)
            .map_err(|reason| format!("{} was not loaded: {reason}", path.display()))?;

        // **MIDI cannot carry locks**, so loading one clears them and says so. Keeping them would
        // put a filter sweep you did not import under notes you did.
        let had_locks = !self.sequencer.locks.is_empty()
            || !self.pending_fx_locks.is_empty()
            || self.parked_locks.is_some();
        self.pending_fx_locks.clear();
        self.unpark_bases();
        self.sequencer.locks.clear_all();
        self.modulation.clear();
        self.pending_step_edit.clear();
        self.parked_locks = None;

        self.sequencer.pattern = loaded.pattern;
        self.sequencer.tempo = loaded.tempo;
        self.sequencer_changed();

        self.sequence_problems = loaded.lost.clone();
        if had_locks {
            self.sequence_problems.push(
                "a MIDI file cannot carry parameter locks; the ones in force were cleared"
                    .to_owned(),
            );
        }
        self.status = Some(match self.sequence_problems.len() {
            0 => format!("Loaded {}", path.display()),
            n => format!("Loaded {} — {n} thing(s) could not be kept", path.display()),
        });
        self.persist();
        Ok(())
    }

    /// Everything Load can offer: `.mid` from exports, and legacy `.seq.json` where it has always
    /// lived. Nothing a user saved has to be moved.
    pub fn loadable_sequences(&self) -> Vec<(String, PathBuf)> {
        let mut found: Vec<(String, PathBuf)> = list_with_extension(&self.exports_dir, "mid");
        found.extend(sequencer::sequence::list(&self.sequence_dir));
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    }

    /// Loads a sequence, replacing the pattern and tempo.
    ///
    /// A sequence carries no instrument, so this works whatever is loaded — which is the point:
    /// the same test sequence can be run through two synths and compared.
    pub fn load_sequence(&mut self, path: &Path) -> Result<(), String> {
        let (sequence, problems) = sequencer::Sequence::load(path)?;
        let (pattern, tempo, more) = sequence.unpack();
        self.pending_fx_locks.clear();

        // The file's locks replace what is in force — **including replacing them with nothing**.
        // A sequence that carries none is a sequence whose steps set nothing, not a sequence with
        // no opinion, and leaving the previous ones running under it would be automation from a
        // file nobody loaded.
        //
        // **With no instrument loaded they are parked rather than installed.** `to_locks` would
        // otherwise throw the recorded `CLAP_ID` away — there is nothing to compare it against yet —
        // and the set would then be published to whatever loaded next as though it had always
        // belonged to it. Parked, the comparison still happens, just later.
        if self.engine.plugin_id().is_none() {
            self.sequencer.locks.clear_all();
            self.modulation.clear();
            self.pending_step_edit.clear();
            self.parked_locks = sequence.locks.clone();
        }
        // **Through the chain**, so an effect's automation is matched back to the effect it was
        // written for, and anything naming an effect that is not loaded is *held* rather than
        // dropped. Loading through the chainless form discarded every effect lock the moment a
        // sequence was reloaded — silently, which is the worst way to lose somebody's work.
        let chain = self.chain_refs();
        let (locks, lock_problems) = match sequence.locks.as_ref() {
            Some(data) if self.engine.plugin_id().is_some() => {
                let (locks, problems, pending) =
                    data.to_locks_in_chain(self.engine.plugin_id(), &chain);
                self.pending_fx_locks = pending;
                (locks, problems)
            }
            _ => (crate::sequencer::locks::LockSet::EMPTY, Vec::new()),
        };
        self.pending_step_edit.clear();
        self.unpark_bases();
        if self.engine.plugin_id().is_some() {
            self.sequencer.locks = locks;
            self.sync_baselines();
            self.parked_locks = None;
        }
        let mut lock_problems = lock_problems;
        lock_problems.extend(self.prune_unknown_locks());

        self.sequencer.pattern = pattern;
        // Locks on tied steps load as written: per-step locks made them meaningful — a hold's
        // lock moves a parameter under the held note — so a file that carries them is music.
        //
        // **Ties load exactly as written, orphans included.** A tie reaching no note is not a
        // damaged file to repair, it is a pattern with nothing to hold yet: the runtime sounds
        // nothing until a note arrives, so there is no state here to report and no edit to make
        // on the way in. The file is true to the interface because the interface allows it too.
        self.sequencer.tempo = tempo;
        self.settle_bases_to_patch();
        self.sequencer_changed();

        self.sequence_problems = problems;
        self.sequence_problems.extend(more);
        self.sequence_problems.extend(lock_problems);
        self.sequence_name = sequence.name.clone();
        self.status = Some(match self.sequence_problems.len() {
            0 => format!("Loaded {}", sequence.name),
            n => format!("Loaded {} with {n} problem(s)", sequence.name),
        });
        self.persist();
        Ok(())
    }

    pub fn saved_sequences(&self) -> Vec<(String, PathBuf)> {
        sequencer::sequence::list(&self.sequence_dir)
    }

    /// Ends gestures whose controls have gone quiet.
    ///
    /// A CC has no release, so nothing else would ever close them, and an unclosed gesture leaves
    /// the host tracking an edit nobody is making.
    fn close_idle_control_gestures(&mut self) {
        let now = self.engine.clock().now_nanos();
        for param_id in self.control_map.expired_gestures(now) {
            self.end_mapped_gesture(param_id);
        }
    }

    /// Closes every gesture the control map opened, whatever its age.
    ///
    /// Called wherever an edit can no longer be continued: plugin unload, rescan, engine stop,
    /// control-map reload. `Payload::GestureEnd` is a must-not-be-lost event, and the merged
    /// buffer reserves room for exactly this.
    pub fn close_control_gestures(&mut self) {
        for param_id in self.control_map.close_all_gestures() {
            self.end_mapped_gesture(param_id);
        }
    }

    fn end_mapped_gesture(&mut self, param_id: u32) {
        self.engine.push_gui_event(Payload::GestureEnd { param_id });
        self.open_gestures.retain(|p| *p != param_id);
        // The plugin may have clamped or quantised what we sent, so the authoritative value is
        // read back a frame later — the same deferral the on-screen panel uses.
        self.requery_params = true;
    }

    /// Turns one claimed control change into an edit, a page change, or nothing.
    fn apply_mapped_cc(&mut self, controller: u8, value: u8) {
        let now = self.engine.clock().now_nanos();
        let clap_id = self.engine.plugin_id().map(str::to_owned);
        let outcome =
            self.control_map
                .handle_cc(controller, value, clap_id.as_deref(), &self.params, now);

        match outcome {
            crate::control_map::Outcome::Edit(edit) => {
                if edit.begin_gesture {
                    self.engine.push_gui_event(Payload::GestureBegin {
                        param_id: edit.param_id,
                    });
                    if !self.open_gestures.contains(&edit.param_id) {
                        self.open_gestures.push(edit.param_id);
                    }
                }
                // Through the funnel, so a mapped knob behaves exactly as the panel does. Sending
                // a `ParamValue` here is what let a hardware knob move the patch out from under a
                // step it was supposed to be editing.
                // `deliver_edit` records the value in the panel's snapshot **only when a value was
                // what it sent** — with a step selected the deviation goes out as an offset and the
                // parameter's own value has not moved. Recording it here regardless is what made a
                // mapped knob report a value the plugin never took.
                if self.parameter_edited(edit.param_id, edit.value) {
                    self.deliver_edit(edit.param_id, edit.value, false);
                }
                // Written into the snapshot immediately. The edit reaches the plugin through the
                // audio thread, so a panel drawn this frame would otherwise show the old value
                // and the control would appear to snap back.
                if let Some(param) = self.params.get_mut(edit.param_id) {
                    param.value = edit.value;
                }
                let text = self.engine.format_param(edit.param_id, edit.value);
                if let Some(param) = self.params.get_mut(edit.param_id) {
                    param.text = text;
                }
            }
            crate::control_map::Outcome::PageChanged => {
                self.status = Some(format!(
                    "Control page {} of {}: {}",
                    self.control_map.active_page() + 1,
                    self.control_map.page_count(),
                    self.control_map.active_page_title()
                ));
            }
            // Every absorbed case is a normal state, not an error, and deliberately silent: a
            // knob on a role this instrument does not have should do nothing at all, rather than
            // filling the status line while somebody is playing.
            crate::control_map::Outcome::Absorbed(_) => {}
            // The worker only forwards what the mask claims, so this means the mask and the map
            // disagree — which can happen for one frame after a reload.
            crate::control_map::Outcome::Unclaimed => {}
        }
    }

    fn apply_plugin_output(&mut self) {
        // Deferred by one frame on purpose: the audio thread drains the parameter event well
        // inside a frame, so by now the plugin holds the value we sent and can be asked what it
        // actually did with it — clamping, quantising, or accepting it as-is.
        if self.requery_params {
            self.requery_params = false;
            self.refresh_params();
        }

        // **The settle drains before it requeries.** A queued notification applied *after* a full
        // requery would overwrite the authoritative value with an older one — the same
        // read-back-before-the-edit-lands trap the panel already avoids by requerying next frame.
        let unsettled = self.engine.shared().take_unsettled_params();
        if unsettled {
            // **Only the value notifications are stale. The rest still have to happen.**
            //
            // Dropping the whole drain here left the flag latched: the audio thread sets it
            // whenever a `SequencerParam` push is refused, which is what happens once `to_gui`
            // fills from an output-sink overflow — and the `TrackingInvalidated` that overflow
            // also queued went into the same discard, so the recovery that would have closed the
            // stranded gestures never ran and the status never said so. The branch then requeried
            // every parameter, every frame, for the rest of the session.
            //
            // A value queued behind a full requery would overwrite the authoritative reading with
            // an older one, so those two variants are still dropped. Nothing else carries a value
            // for the requery to supersede, and discarding it loses real work: a gesture `end`
            // that never closes, and a claimed control change that never lands.
            let surviving: Vec<PluginOutput> = self
                .engine
                .drain_plugin_output()
                .into_iter()
                .filter(Self::survives_a_requery)
                .collect();
            self.receive_plugin_outputs(surviving);
            self.refresh_params();
            return;
        }

        // What the plugin reported this time round, decided on together — see `plugin_moved`.
        let outputs = self.engine.drain_plugin_output();
        self.receive_plugin_outputs(outputs);
    }

    /// Applies a drained batch of plugin outputs, **in queue order**.
    ///
    /// Split from the drain so a test can feed a synthetic batch: the interpretation of a
    /// begin/values/end sequence depends on the order this loop sees it in — the flush before
    /// each gesture arm below — and no headless test can make a real plugin emit one.
    #[doc(hidden)]
    /// Whether a plugin output still has to be acted on **after** a full parameter requery.
    ///
    /// A requery supersedes a value, so a value queued behind one would overwrite the
    /// authoritative reading with an older one. It supersedes nothing else: a gesture `end` still
    /// has to close its gesture, a claimed control change still has to land, and
    /// `TrackingInvalidated` still has to run the recovery for the overflow that stranded them.
    #[must_use]
    pub fn survives_a_requery(output: &PluginOutput) -> bool {
        !matches!(
            output,
            PluginOutput::ParamValue { .. } | PluginOutput::SequencerParam { .. }
        )
    }

    pub fn receive_plugin_outputs(&mut self, outputs: Vec<PluginOutput>) {
        // **The batch's shape is decided first, over the whole drain.** One parameter's values
        // are a hand — a click or a drag — and are interpreted strictly in queue order, so two
        // gestures on one knob in one drain stay two gestures (a second click back to the patch
        // clears rather than being merged into the first as a drag). Several parameters' values
        // are a patch change, and the heuristic needs to see them together — split at gesture
        // boundaries, a preset burst reads as sequencing and becomes locks. The count is the
        // only thing that can tell the shapes apart, and it needs the whole batch.
        let single_param = {
            let mut params = outputs.iter().filter_map(|output| match output {
                PluginOutput::ParamValue { param_id, .. } => Some(*param_id),
                _ => None,
            });
            let first = params.next();
            first.is_none() || params.all(|id| Some(id) == first)
        };

        let mut from_plugin: Vec<(u32, f64)> = Vec::new();
        // In the patch-change shape, gesture ends are deferred until the batch's values have
        // been interpreted; in the single-parameter shape they act in place, after the values
        // collected so far are flushed — queue order either way.
        let mut ended: Vec<u32> = Vec::new();

        for output in outputs {
            match output {
                // Collected rather than acted on one at a time: what a burst of these *means*
                // depends on how many of them there are. `plugin_moved` below does all of it.
                PluginOutput::ParamValue { param_id, value } => {
                    from_plugin.push((param_id, value));
                }
                // A value the **sequencer** set. The plugin never echoes what the host sent, so
                // without this the panel would go on drawing the old one and a mapped knob's pickup
                // would compare against a value that no longer exists.
                // **An offset the sequencer applied, not a value the parameter took.** Since locks
                // became modulation the parameter's own value is untouched, so this must never reach
                // `param.value` — doing so would put an offset where a value belongs and the patch
                // would drift by a step's deviation per requery. Nor does a mapped knob need to
                // catch up: pickup compares against the value, which has not moved.
                //
                // It is recorded separately, because *what the instrument is sounding* is the value
                // plus this, and something has to hold the second half.
                PluginOutput::SequencerParam { param_id, value } => {
                    // **A non-zero offset is recorded only for a parameter that is still sequenced.**
                    // These are queued, so one can arrive after its lock has been cleared, and
                    // putting it back would resurrect a deviation the plugin has already dropped.
                    //
                    // **A zero is honoured whatever the lock set says**: it is the cleanup message —
                    // the runtime zeroing an abandoned offset — and refusing it for an unlocked
                    // parameter left the cache claiming a deviation the plugin no longer applies.
                    // That is what made a drag over an existing lock read a step's worth too high.
                    if value == 0.0 {
                        self.modulation.remove(&param_id);
                    } else if self.sequencer.locks.locks_anywhere(param_id) {
                        self.modulation.insert(param_id, f64::from(value));
                    }
                }
                // Plugin-driven gestures are tracked so the panel does not fight the plugin
                // while it is automating itself.
                //
                // **Begins act now; ends act after the whole batch is interpreted.** Two shapes
                // pull opposite ways here. Acting on an end before the values it brackets have
                // been seen commits a pending edit from the *previous* turn while this turn's
                // drag still sits in `from_plugin` — the drag then falls through the
                // instantaneous branch and, ending on the patch, clears a lock it should pin.
                // But flushing the values at each boundary splits the batch, and the batch's
                // *size* is the preset heuristic: a preset load arrives as begin/value/end per
                // parameter, and fed one parameter at a time it reads as sequencing — a whole
                // patch written into the selected step as locks. So the values stay collected,
                // gestures stay open through `plugin_moved` (which is what routes a drag's
                // values into the pending edit), and the ends run afterwards, in order.
                PluginOutput::GestureBegin { param_id } => {
                    if single_param && !from_plugin.is_empty() {
                        let staged = std::mem::take(&mut from_plugin);
                        self.plugin_moved(&staged);
                    }
                    // The same door a split gesture comes through, so what a begin records
                    // cannot differ by timing.
                    self.plugin_gesture_began(param_id);
                }
                PluginOutput::GestureEnd { param_id } => {
                    if single_param {
                        if !from_plugin.is_empty() {
                            let staged = std::mem::take(&mut from_plugin);
                            self.plugin_moved(&staged);
                        }
                        self.plugin_gesture_ended(param_id);
                    } else {
                        ended.push(param_id);
                    }
                }
                PluginOutput::MappedControlChange { controller, value } => {
                    self.apply_mapped_cc(controller, value);
                }
                PluginOutput::TrackingInvalidated => {
                    // Close every gesture the host is tracking, then requery: what the sink lost
                    // may have included the `end` that would have closed them.
                    for param_id in std::mem::take(&mut self.open_gestures) {
                        self.engine.push_gui_event(Payload::GestureEnd { param_id });
                    }
                    self.editing.clear();
                    // The gesture ends this recovery closes may include the one a pending step edit
                    // was waiting for, and it will never close now.
                    self.pending_step_edit.clear();
                    // Through `refresh_params`, so recovery does not leave the panel reporting a
                    // sequenced parameter's *modulated* value as though it were the value.
                    self.refresh_params();
                    self.status = Some(
                        "The plugin emitted more events than the host could carry; \
                              parameter tracking was refreshed."
                            .to_owned(),
                    );
                }
            }
        }

        self.plugin_moved(&from_plugin);

        // The deferred gesture ends, in queue order, after the values they bracket have been
        // interpreted: a drag's pending edit commits with its full click-or-drag count, and a
        // preset burst was already read whole above.
        for param_id in ended {
            self.plugin_gesture_ended(param_id);
        }
    }

    /// Applies what the plugin reported, and decides what it *meant*.
    ///
    /// Public because it is the seam a test can reach: a preset load happens inside the plugin's own
    /// window, which nothing can drive from outside, so the rule is exercised here rather than taken
    /// on trust.
    ///
    /// **One parameter is a hand; several at once are a patch.** A person turns one control at a
    /// time, so a single parameter arriving from the plugin's editor is somebody sequencing — and
    /// with a step selected it is written into that step. A whole instrument's worth arriving
    /// together is a preset being chosen, which is a new sequence-patch, and writing *that* into the
    /// selected step would put an entire patch in one step and leave every step sounding the same.
    /// That is the "it changes the patch per step" fault: one sound per sequence, for ever.
    ///
    /// **This is a heuristic, and it is here because CLAP offers nothing better.** A plugin reports a
    /// preset load exactly as it reports a knob: `begin`, a value, `end`, per parameter. There is no
    /// flag saying "this was a patch change", so the host has to read intent from shape. The proper
    /// fix is for the instrument to say so — see `docs/briefs/mxm-player.md` — and until then the
    /// shape is the only evidence there is. Its one wrong answer is a preset that happens to change
    /// a single parameter, which is then recorded as a lock; the cost is one unwanted lock, visible
    /// as a dot, and undone by setting that parameter back to the patch.
    pub fn plugin_moved(&mut self, changes: &[(u32, f64)]) {
        // **The plugin is the authority now.** `editing` holds what the panel last sent and takes
        // precedence when the panel draws — and `follow_sequence_patch` reads the same thing. A
        // stale entry would therefore hide the value that just arrived and then quietly put the old
        // one back on the next frame, so the sequence-patch would never register a patch change at
        // all. That is exactly the "a sequence does not register when I change patch" fault.
        let clap_id = self.engine.plugin_id().map(str::to_owned);
        for (param_id, value) in changes {
            self.editing.remove(param_id);

            // The parameter moved for a reason that was not this controller, so any knob bound to it
            // must catch up again before it takes effect.
            self.control_map
                .parameter_moved_elsewhere(*param_id, clap_id.as_deref());

            // The panel's own snapshot, so what is on screen is what the instrument has. Formatted
            // from the value that arrived rather than read back, exactly as the other paths do.
            let text = self.engine.format_param(*param_id, *value);
            if let Some(param) = self.params.get_mut(*param_id) {
                param.value = *value;
                param.text = text;
            }
        }

        // **Distinct parameters, not event count.** A drag's frames can queue up and arrive as one
        // batch, and several values for one knob are still one hand - counting events would call
        // that a patch change and write nothing into the step.
        let mut distinct: Vec<u32> = changes.iter().map(|(id, _)| *id).collect();
        distinct.sort_unstable();
        distinct.dedup();
        // How many values the parameter really delivered, kept from before the collapse: the
        // click-or-drag count must see every frame of a drag, or a drag whose frames arrive in
        // one batch collapses to "one value" and reads as a click.
        let raw_edits = changes.len();
        let changes: &[(u32, f64)] = if distinct.len() == 1 && changes.len() > 1 {
            // One knob, many frames: only where it ended up matters.
            &changes[changes.len() - 1..]
        } else {
            changes
        };

        match changes {
            [] => {}
            [(param_id, value)] => {
                // **Held: remember the value, suppress the preview, and touch nothing canonical.**
                // Writing the lock now would double the deviation for as long as the drag lasts —
                // and so does an *existing* lock's preview under the dragged base, which is why the
                // runtime is told to emit zero for this one parameter while it is held. An earlier
                // fix cleared the lock itself and re-created it on release; a deselect or a save
                // mid-drag then lost or persisted a state nobody had chosen.
                if let Some(step) = self
                    .editing_step()
                    .filter(|_| self.open_gestures.contains(param_id))
                {
                    self.pending_step_edit.insert((step, *param_id), *value);
                    // Counted so the commit can tell a click from a drag: one value is a click,
                    // and a click landing back on the patch clears the lock rather than pinning.
                    // By `raw_edits`, not one: a drag's frames can arrive as one batch and were
                    // collapsed above, and counting the batch as a single value would let a drag
                    // ending on the patch clear a lock it should pin.
                    *self.open_gesture_edits.entry(*param_id).or_insert(0) += raw_edits;
                    if self.parked_bases.insert(*param_id) {
                        self.sequencer_changed();
                    }
                    return;
                }

                // **An instantaneous editor edit that lands on the patch is §7.1's reset: the
                // step sets nothing.** The editor cannot say "clear" — CLAP only carries values —
                // so its double-click arrives as begin/set/end in one batch, indistinguishable by
                // shape from a click. What distinguishes it is the *value*: no drag is here
                // (drags come through the gesture branch above, so the no-collapse rule is
                // untouched), and an instantaneous jump exactly onto the patch value while a step
                // is selected reads as "put this back" - which, with a step selected, the
                // player's own contract defines as clearing the lock, never pinning it.
                //
                // **The parked clause covers the editor's blind spot.** While a base is parked
                // the parameter reports no modulation, so the editor's reset aims at the factory
                // default instead of the base - the one honest signal it has left. A parked
                // parameter is one an editor drag just wrote, and an instantaneous default
                // arriving on it is a reset, not a person dialling the default as a lock.
                if let Some(step) = self.editing_step() {
                    let near = |target: Option<f64>| {
                        target.is_some_and(|target| (*value - target).abs() < 1e-6)
                    };
                    let patch = self
                        .sequence_patch
                        .get(&Self::source_key(*param_id))
                        .copied();
                    let default = self.params.get(*param_id).map(|p| p.default);
                    let parked = self.parked_bases.contains(param_id);
                    // Live re-click: a click writing exactly what the step already locks is the
                    // same toggle clicked again, and its second click means off — the control
                    // cannot express Off itself while the base springs to the patch. See the
                    // commit branch in `restore_base_for_step_edit` for the full reasoning; the
                    // rule stands down at rest, where parking makes the control alternate.
                    let re_click = self.sequencer.transport != Transport::Stopped
                        && self
                            .sequencer
                            .locks
                            .get(step, *param_id)
                            .is_some_and(|locked| (*value - f64::from(locked)).abs() < 1e-6);
                    if near(patch) || (parked && near(default)) || re_click {
                        self.pending_step_edit.remove(&(step, *param_id));
                        if self.parked_bases.remove(param_id) {
                            self.sequencer_changed();
                        }
                        // The panel's own reset: clears the step's lock and puts the parameter
                        // back to the patch - which also repairs the base when the value that
                        // arrived was the default rather than the patch.
                        self.reset_parameter(*param_id);
                        return;
                    }
                }
                let accepted = self.parameter_edited(*param_id, *value);
                // **The plugin has already moved its own parameter**, which is the patch. With a
                // step selected that has to be undone and re-expressed as modulation, or editing a
                // step from the instrument's own editor would quietly rewrite the patch — the one
                // path where that is hardest to notice, because the knob you are holding looks
                // right.
                if accepted {
                    // **At rest, an instantaneous edit parks, exactly as a drag's release does.**
                    // The base stays where the click put it for as long as the step is selected —
                    // so a toggle clicked On *shows* On instead of springing straight back to the
                    // patch, which read as the control being dead. Parking publishes the
                    // parameter as held, zeroing its preview, so base-plus-offset cannot sound a
                    // deviation high; `unpark_bases` returns the patch when the step is left.
                    // Under a running transport the base goes back at once, as it does for drags:
                    // the stepping's offsets need the patch underneath them.
                    if self.selected_step.is_some()
                        && self.sequencer.transport == Transport::Stopped
                    {
                        if self.parked_bases.insert(*param_id) {
                            self.sequencer_changed();
                        }
                    } else {
                        self.restore_base_for_step_edit(*param_id);
                    }
                }
            }
            many => {
                // A patch change. The sequence-patch takes the new values whether or not a step is
                // selected: choosing a patch is choosing what the whole sequence deviates from, and
                // it is not an edit to the step you happen to have open.
                for (param_id, value) in many {
                    self.sequence_patch
                        .insert(Self::source_key(*param_id), *value);
                    if self.sequencer.locks.locks_anywhere(*param_id) {
                        self.sequencer.locks.set_patch(*param_id, *value as f32);
                    }
                }
                self.sequencer_changed();
                self.locks_unsaved = true;
            }
        }
    }

    fn drain_logs(&mut self) {
        let shared = self.engine.shared().clone();
        while let Some(record) = shared.logs.pop() {
            let suffix = if record.truncated { " […]" } else { "" };
            self.log_lines.push(format!(
                "[{:?}] {}{suffix}",
                record.severity,
                record.message()
            ));
        }
        let dropped = shared.logs.dropped();
        if dropped > 0 && self.log_lines.last().is_none_or(|l| !l.contains("dropped")) {
            self.log_lines
                .push(format!("({dropped} log messages dropped)"));
        }
        // Bounded: a misbehaving plugin must not grow the window's memory without limit.
        let excess = self.log_lines.len().saturating_sub(500);
        self.log_lines.drain(..excess);
    }
}

impl eframe::App for PlayerApp {
    /// Shutdown. **This is the only hook that runs when the window's X is clicked.**
    ///
    /// `close_requested()` is not: eframe's native integration returns
    /// `EventResult::CloseRequested` straight out of the winit event and shuts down without drawing
    /// another frame, so a check for it inside `logic` is unreachable code for the main viewport.
    /// An earlier attempt at this fix lived there and did nothing.
    ///
    /// What has to happen here is destroying the plugin's editor. It is a window the **plugin**
    /// owns, in our process: closing the player without telling the plugin leaves that window
    /// behind with its graphics context being torn down underneath it and nothing repainting it —
    /// which is the black window that survives the player.
    ///
    /// `close_editor` is the same `hide` → `destroy` the toggle uses, so the exactly-one-`destroy`
    /// rule holds on this path too rather than being special-cased away.
    fn on_exit(&mut self) {
        // Before the wedge check, because a wedged plugin is a reason to lose the *audio*, not a
        // reason to lose the last half-second of sequencing.
        self.flush_settings();

        // A wedged plugin has stopped answering, so a `clap.gui` call would hang the thread that
        // is trying to exit. The window goes with the process instead.
        if self.engine.is_wedged() {
            return;
        }
        // Every window this player opened, not only the instrument's: an effect's editor left
        // behind is the same black window with its context torn down underneath it.
        self.engine.close_all_editors();
    }

    /// Everything that is not drawing. Also runs while the window is hidden, which is exactly
    /// when the engine still needs polling and the plugin still needs servicing.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // **Scan once, on the first frame — not during construction.**
        //
        // Scanning executes plugin code: `scan_bundle` loads each bundle to ask what is in it. Two
        // consequences shape the timing.
        //
        // Doing it in the constructor would mean a plugin that hangs or crashes on load does so
        // before there is a window, and the player would look like it simply failed to start. On
        // the first frame the window already exists, so the sentinel's record survives into a
        // visible next launch, where `suspected` explains what happened and offers to quarantine
        // it. That machinery already existed for the manual rescan; running the scan automatically
        // is what makes it matter at startup.
        //
        // It is also why this is deferred rather than eager: the window appears immediately and
        // the scan happens behind it, instead of the launch stalling on somebody else's code.
        if !self.startup_scan_done {
            self.startup_scan_done = true;
            self.rescan();
            // The chain comes back before any source does: an effect needs no source to be
            // instantiated, and the source's own load then starts audio once, with the chain in
            // it, rather than starting twice.
            self.restore_fx_chain();

            // `MXM_AUTO_EDITOR=<clap-id>` loads that plugin and opens its editor without anyone
            // clicking. It exists because the white-editor failure is **visual, machine-dependent
            // and only reproducible with a real editor window** — no headless test can see it — so
            // measuring the safe cadence needs a script that can launch, open and screenshot.
            if let Ok(want) = std::env::var("MXM_AUTO_EDITOR")
                && let Some(found) = self
                    .found
                    .iter()
                    .find(|f| f.is_supported() && f.id == want)
                    .cloned()
            {
                self.load(found.bundle.clone(), found.id.clone());
                self.open_editor_pending = true;
            }
        }

        if std::mem::take(&mut self.open_editor_pending) {
            self.show_editor_now(ctx);
        }

        self.service();
        // The plugin's GUI callbacks arrive on arbitrary threads and are recorded as atomics.
        // This is the main thread, which is the only place `clap.gui` calls are legal.
        if self.engine.service_editor() {
            self.repaint_after_editor_change(ctx);
        }

        // **Keep servicing while there is something to service, even unfocused.**
        //
        // `logic` runs only when eframe draws, and eframe draws reactively — so an unfocused
        // player with no pointer over it stops polling the engine, and the sequencer stalls and
        // MIDI presses stop reaching the selected step. Asking for a repaint is what keeps the
        // frames coming.
        //
        // This was *not* safe while the player rendered through OpenGL: both painters then fought
        // over one global current-context and the editor rendered white. The player renders through
        // wgpu now, so there is nothing to contend for — that is the whole point of the swap, and
        // this is the behaviour it buys back.
        //
        // **`visible`, not `open`.** Hiding an editor leaves `open` true (`EditorState` keeps them
        // apart deliberately), so keying on `open` would repaint for ever after a hide — the idle
        // burn this condition exists to avoid. Idle with nothing playing and nothing shown still
        // sleeps.
        // **A bounded cadence, not a continuous repaint — and the bound is two-sided.**
        //
        // `logic()` runs only when eframe draws, so an unfocused player would stop servicing the
        // engine: the sequencer stalls and MIDI presses stop reaching the selected step. That is
        // why a repaint is requested at all.
        //
        // But the plugin's editor is a window **on this same thread**, and it is starved from both
        // directions. Measured, on Vulkan, with no OpenGL anywhere in the player:
        //
        // - `request_repaint()` every frame leaves the event loop no idle time, the editor's
        //   messages are never dispatched, and **it renders white**. This is the symptom
        //   `docs/known-issues.md` (in mxm-kit) attributed to two OpenGL painters sharing a
        //   context; it reproduces with the player on Vulkan, so that diagnosis is not the whole
        //   story.
        // - Repainting only reactively is the opposite failure: the thread cycles rarely, the
        //   editor gets few chances, and its interface feels sluggish.
        //
        // So ask for a frame *after a delay* rather than immediately. The loop idles in between,
        // which is what lets the editor's window messages through, and the engine still gets
        // serviced many times a second.
        //
        // **A frame is always asked for, and only the interval changes.** An earlier version of
        // this made the request conditional on playing-or-editor-visible, which reintroduced the
        // very failure the paragraph above describes: a stopped player with no editor open asked
        // for no frames at all, so `service()` never ran and a MIDI keyboard could not enter a
        // note into a selected step. The on-screen and computer keyboards were unaffected and hid
        // it — they write the step from inside the input event that woke the frame, so they never
        // need a frame they did not cause. **MIDI is the only input that does**, which is why it
        // is the only one that broke, and why the fix belongs here rather than near the latch.
        // **Any** editor: an effect's window starves and renders white exactly as the source's
        // does, and the player cannot know which of them is on screen.
        // **A resize is an interaction, and it gets an interaction's frame rate.** Stopped, with
        // no editor open, the cadence below is `IDLE_SERVICE_INTERVAL` — ten frames a second,
        // which is plenty for servicing the engine and visibly stuttery when the window edge is
        // being dragged. On Windows a resize runs in a modal loop, so the timer can be the only
        // thing asking for frames while it does.
        //
        // The boost is deliberately *below* the editor branch. An open editor's interval still
        // wins over everything, for the reason the paragraph above gives: starving it renders it
        // white, and that is a worse failure than a coarse resize.
        let size = ctx
            .input(|input| input.raw.screen_rect)
            .map_or(egui::Vec2::ZERO, |rect| rect.size());
        let resizing = self.last_screen_size.is_some_and(|last| last != size);
        self.last_screen_size = Some(size);

        let editor_visible = self.engine.any_editor_open();
        ctx.request_repaint_after(if editor_visible {
            // Slowest, and it wins over the playhead: below this the editor starves and renders
            // white, and a smooth playhead is not worth a blank editor.
            editor_service_interval()
        } else if resizing || self.sequencer.transport == Transport::Playing {
            PLAYHEAD_INTERVAL
        } else {
            IDLE_SERVICE_INTERVAL
        });

        // **Every frame, not only while the parameter panel is drawn.** The patch can move for
        // reasons that have nothing to do with the panel — a knob in the plugin's own editor, a
        // preset chosen from the plugin's own browser — and a rule that only held while a
        // particular panel happened to be visible would be no rule at all.
        self.follow_sequence_patch();

        // The drag that has been writing locks has been quiet long enough to be worth a file write.
        if self.locks_unsaved && self.last_lock_save.elapsed() >= LOCK_SAVE_INTERVAL {
            self.persist();
        }

        self.handle_input(ctx);
        // After `handle_input`, so a copy made by the key chord in this same frame goes out in it.
        // A copy made by a *button* is drawn later and rides the next frame, which is soon enough:
        // nothing can read the system clipboard in between.
        if let Some(text) = self.pending_clipboard_text.take() {
            ctx.copy_text(text);
        }
        self.remember_geometry(ctx);

        // While wedged the stream cannot be dropped without deadlocking, so a normal shutdown
        // would hang. Window close is intercepted and the process is terminated outright, which
        // bypasses destructors entirely.
        // Kept for viewport-level closes; the **main** window's X does not reach here — see
        // `on_exit`. While wedged the stream cannot be dropped without deadlocking, so a normal
        // shutdown would hang, and the process is terminated outright instead.
        if ctx.input(|i| i.viewport().close_requested()) && self.engine.is_wedged() {
            let _ = self.settings.save(&self.settings_path);
            std::process::exit(0);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // Every frame, so a change of host theme is followed rather than sampled once at startup.
        //
        // This runs one frame behind, and deliberately: a `Ui` holds a clone of the style it was
        // built with, so writing the context's style here cannot affect the `Ui` already passed
        // in. `mxm_ui` resolves its named styles defensively for exactly this reason, and
        // `PlayerApp::style` applies them before the first frame so the lag is never visible.
        mxm_ui::theme::apply(ui.ctx());
        mxm_ui::typography::apply(ui.ctx());

        // The chosen theme, for the paths that have no context to set it on themselves: the CLI's
        // `theme` verb, and any test that sets the field directly. Compared rather than written
        // every frame, so nothing here is fighting `ThemePreference::System` for the desktop's.
        if ui.ctx().options(|options| options.theme_preference) != self.theme {
            ui.ctx().set_theme(self.theme);
        }

        // The player's own window handle, for making it the owner of a plugin's editor window.
        // Read every frame rather than cached: it is two field reads, and a cached handle would
        // outlive a window the host recreated.
        self.host_window = crate::ownership::HostWindow::from_handle(&*frame);

        // Three panels are never collapsible, at the user's instruction, and they are the
        // performance surface: the status readings, the sequencer and the keyboard. The two that
        // collapse are the testing surface — the parameter panel and the left settings panel —
        // which is the brief's own ranking: "playing wins for the bottom of the window and testing
        // wins for everything above it".
        self.top_bar(ui);
        // Before the side panel, so the keyboard runs the full width of the window rather than
        // being boxed into whatever the browser leaves. It is the thing you play; it gets the
        // whole bottom edge.
        self.keyboard_panel(ui);
        // After the keyboard, so it sits directly above it: the first bottom panel added keeps
        // the bottom edge.
        self.sequencer_panel(ui);
        if !self.settings.settings_collapsed {
            self.side_panel(ui);
        }
        if self.settings.parameters_collapsed {
            // Nothing in the middle. The panels above and below keep their own space, so the
            // window shrinks to them rather than leaving a hole.
            egui::CentralPanel::default().show(ui, |_ui| {});
        } else {
            self.parameter_panel(ui);
        }
        self.apply_collapse_geometry(ui.ctx());
    }
}

impl PlayerApp {
    fn top_bar(&mut self, root: &mut egui::Ui) {
        let response = egui::Panel::top("status").show(root, |ui| {
            ui.horizontal(|ui| {
                let (label, colour) = match self.engine.state() {
                    EngineState::Idle => ("idle", adapter::tokens_for(ui).text_secondary),
                    EngineState::Running => ("running", adapter::tokens_for(ui).success),
                    EngineState::AwaitingStoppedProcessor => {
                        ("stopping", adapter::tokens_for(ui).warning)
                    }
                    EngineState::StreamExited(_) => {
                        ("audio device stopped", adapter::tokens_for(ui).warning)
                    }
                    EngineState::Wedged => ("audio stop timed out", adapter::tokens_for(ui).danger),
                    EngineState::Failed(_) => ("failed", adapter::tokens_for(ui).danger),
                };
                ui.colored_label(colour, label);

                match self.engine.state() {
                    EngineState::StreamExited(reason) => {
                        // What happened, and what is being done about it — with the backend's
                        // reason for the last refusal, which is where "held by another
                        // application" shows up while the reconnect keeps trying.
                        let text = match self.engine.reconnect() {
                            Some(reconnect) => match (&reconnect.last_failure, reconnect.attempts) {
                                (Some(failure), attempts) => format!(
                                    "{reason}; reconnect attempt {attempts} failed: {failure}; retrying"
                                ),
                                (None, 0) => format!("{reason}; reconnecting"),
                                (None, attempts) => {
                                    format!("{reason}; reconnecting (attempt {attempts})")
                                }
                            },
                            None => reason.clone(),
                        };
                        ui.label(text);
                    }
                    EngineState::Failed(reason) => {
                        ui.label(reason.clone());
                    }
                    _ => {}
                }

                ui.separator();

                // **A reading appears when it has something to say.** Six of these stood in the
                // bar at all times, most of them reporting that nothing was wrong -- "late 0
                // xruns not reported priority requested (backend reports no outcome) latency 0
                // samples" -- which is a paragraph of diagnostics across a musician's transport.
                // The owner asked for it to go (2026-09-07). None of it is lost: every field is
                // still in the CLI's `dump`, which is where this project already decided
                // diagnostics live. What stays here is the one live number, and then only the
                // things that are actually a problem.
                let meters = self.engine.meters.clone();
                adapter::reading(
                    ui,
                    "load",
                    &format!("{:.0}%", meters.callback_load() * 100.0),
                    "The whole audio callback as a share of its wall-clock budget. `dump` \
                     separates the plugin's share from the player's",
                );
                if meters.missed_deadlines() > 0 {
                    adapter::reading(
                        ui,
                        "late",
                        &meters.missed_deadlines().to_string(),
                        "Callbacks that overran their budget. Inferred by the player, not \
                         reported by the backend",
                    );
                }
                // `None` is "the backend does not report them", which is not news; a count is.
                if let Some(count) = meters.xruns().filter(|count| *count > 0) {
                    adapter::reading(
                        ui,
                        "xruns",
                        &count.to_string(),
                        "Buffer underruns as reported by the audio backend",
                    );
                }
                // Promotion working is the expected case and says nothing worth the width.
                if meters.realtime_priority() != crate::engine::meters::PriorityStatus::Confirmed {
                    adapter::reading(
                        ui,
                        "priority",
                        meters.realtime_priority().label(),
                        "Whether the audio thread was promoted to realtime priority",
                    );
                }
                if let Some(samples) = self.latency.filter(|samples| *samples > 0) {
                    adapter::reading(
                        ui,
                        "latency",
                        &format!("{samples} samples"),
                        "Latency the plugin reports through the latency extension",
                    );
                }

                self.editor_and_collapse_controls(ui);

                if self.engine.is_wedged() {
                    ui.separator();
                    if ui.button("Restart player").clicked() {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe).spawn();
                        }
                        let _ = self.settings.save(&self.settings_path);
                        std::process::exit(0);
                    }
                }
            });
        });

        self.status_height = response.response.rect.height();
    }

    fn side_panel(&mut self, root: &mut egui::Ui) {
        egui::Panel::left("browser")
            .default_size(360.0)
            .show(root, |ui| {
                // The column's own content has a floor — a 280px list, a 120px log and the
                // device sections — that is taller than the panel is given in a short window.
                // Without this the panel grows past its rect instead of fitting it, and the
                // browser's edge is then drawn straight down through the keyboard, which owns
                // the bottom edge. Scrolling is the declared fallback, as in the parameter panel.
                egui::ScrollArea::vertical()
                    .id_salt("browser-column")
                    .show(ui, |ui| {
                        if let Some(suspect) = self.suspected.clone() {
                            ui.colored_label(
                        adapter::tokens_for(ui).warning,
                        format!(
                            "The previous run stopped while scanning {}. That is the suspected \
                             bundle, not a proven culprit — the marker also survives a power cut.",
                            suspect.display()
                        ),
                    );
                            ui.horizontal(|ui| {
                                if ui.button("Quarantine it").clicked() {
                                    self.settings.quarantined.push(suspect.clone());
                                    self.persist();
                                    self.sentinel.cleared();
                                    self.suspected = None;
                                }
                                if ui.button("Keep it").clicked() {
                                    self.sentinel.cleared();
                                    self.suspected = None;
                                }
                            });
                            ui.separator();
                        }

                        ui.separator();
                        self.audio_controls(ui);
                        ui.separator();
                        self.midi_controls(ui);
                    });
            });
    }

    fn audio_controls(&mut self, ui: &mut egui::Ui) {
        ui.heading("Audio out");

        let mut config = self.engine.audio_config().clone();
        let current = config
            .device_name
            .clone()
            .unwrap_or_else(|| "System default".to_owned());

        egui::ComboBox::from_label("Device")
            .selected_text(current)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(config.device_name.is_none(), "System default")
                    .clicked()
                {
                    config.device_name = None;
                }
                for device in &self.audio_devices {
                    let selected = config.device_name.as_deref() == Some(device.as_str());
                    if ui.selectable_label(selected, device).clicked() {
                        config.device_name = Some(device.clone());
                    }
                }
            });

        let rates = self.backend.sample_rates(config.device_name.as_deref());
        egui::ComboBox::from_label("Sample rate")
            .selected_text(match config.sample_rate {
                Some(rate) => format!("{rate} Hz"),
                None => "Device default".to_owned(),
            })
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(config.sample_rate.is_none(), "Device default")
                    .clicked()
                {
                    config.sample_rate = None;
                }
                for rate in rates {
                    if ui
                        .selectable_label(config.sample_rate == Some(rate), format!("{rate} Hz"))
                        .clicked()
                    {
                        config.sample_rate = Some(rate);
                    }
                }
            });

        egui::ComboBox::from_label("Buffer size")
            .selected_text(match config.buffer_size {
                Some(size) => format!("{size} frames"),
                None => "Device default".to_owned(),
            })
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(config.buffer_size.is_none(), "Device default")
                    .clicked()
                {
                    config.buffer_size = None;
                }
                for size in [64u32, 128, 256, 512, 1024, 2048] {
                    if ui
                        .selectable_label(
                            config.buffer_size == Some(size),
                            format!("{size} frames"),
                        )
                        .clicked()
                    {
                        config.buffer_size = Some(size);
                    }
                }
            });
        ui.small("Buffer size is a request; the device may deliver any frame count.");

        if config != *self.engine.audio_config() {
            self.settings.audio_device = config.device_name.clone();
            self.settings.sample_rate = config.sample_rate;
            self.settings.buffer_size = config.buffer_size;
            self.persist();
            self.engine.set_audio_config(config);
        }
    }

    fn midi_controls(&mut self, ui: &mut egui::Ui) {
        ui.heading("MIDI");
        if ui.button("Refresh ports").clicked() {
            self.refresh_midi_ports();
        }

        let mut selected = self.settings.midi_inputs.clone();
        ui.label("Inputs");
        for port in self.midi_inputs.clone() {
            let mut on = selected.contains(&port);
            if ui.checkbox(&mut on, &port).changed() {
                if on {
                    selected.push(port.clone());
                } else {
                    selected.retain(|p| *p != port);
                }
            }
        }
        if ui.button("All ports").clicked() {
            selected = self.midi_inputs.clone();
        }
        if selected != self.settings.midi_inputs {
            self.settings.midi_inputs = selected.clone();
            self.persist();
            self.engine.set_midi_inputs(selected);
        }

        for refused in self.engine.refused_midi_inputs() {
            ui.colored_label(
                adapter::tokens_for(ui).warning,
                format!("{}: refused — {}", refused.port, refused.reason),
            );
        }

        let mut output = self.settings.midi_output.clone();
        egui::ComboBox::from_label("Output")
            .selected_text(output.clone().unwrap_or_else(|| "None".to_owned()))
            .show_ui(ui, |ui| {
                if ui.selectable_label(output.is_none(), "None").clicked() {
                    output = None;
                }
                for port in &self.midi_outputs {
                    if ui
                        .selectable_label(output.as_deref() == Some(port.as_str()), port)
                        .clicked()
                    {
                        output = Some(port.clone());
                    }
                }
            });
        if output != self.settings.midi_output {
            self.settings.midi_output = output.clone();
            self.persist();
            self.engine.set_midi_output(output);
        }

        if self.engine.midi_output_faulted() {
            ui.colored_label(
                adapter::tokens_for(ui).danger,
                "The MIDI output port keeps refusing messages and has been marked faulted.",
            );
        }

        let mut thru = self.settings.midi_thru;
        if ui
            .checkbox(&mut thru, "Keyboard thru")
            .on_hover_text(
                "Echo the on-screen and computer keyboards to the MIDI output, after host \
                 sustain handling",
            )
            .changed()
        {
            self.settings.midi_thru = thru;
            self.engine.thru_enabled.store(thru, Ordering::Release);
            self.persist();
        }
    }

    /// What a hardware controller currently reaches on this instrument.
    ///
    /// Worth showing rather than leaving to be discovered by turning knobs: the paged half of the
    /// map is contextual by design, so "which page am I on and what is on it" is the one question
    /// the design creates and has to answer.
    fn control_surface(&mut self, ui: &mut egui::Ui, clap_id: &str) {
        ui.separator();

        if !self.control_map.knows_instrument(clap_id) {
            ui.label("No control map found for this instrument.");
            ui.small(
                "A control map ships beside the .clap bundle. Without one the fixed knobs and                  the paged bank have nothing to reach on this plugin.",
            );
            for problem in &self.control_map_problems {
                ui.colored_label(adapter::tokens_for(ui).danger, problem);
            }
            return;
        }

        ui.horizontal(|ui| {
            ui.label(format!(
                "Controller page {} of {}",
                self.control_map.active_page() + 1,
                self.control_map.page_count()
            ));
            ui.strong(self.control_map.active_page_title());
        });

        // Slot by slot, so an empty slot is visibly empty rather than a knob that does nothing
        // for reasons nobody can see.
        let slots = self.control_map.active_slots();
        let slot_ccs = self.control_map.layout().bank.slot_cc.clone();
        ui.horizontal_wrapped(|ui| {
            for (index, role) in slots.iter().enumerate() {
                let cc = slot_ccs.get(index).copied().unwrap_or(0);
                match role {
                    Some(role) => {
                        let filled = self.control_map.param_for(clap_id, role).is_some();
                        let text = format!("CC{cc} {role}");
                        if filled {
                            ui.small(text);
                        } else {
                            ui.weak(text);
                        }
                    }
                    None => {
                        ui.weak(format!("CC{cc} —"));
                    }
                }
            }
        });

        if let Some(error) = self.control_map.last_error() {
            ui.colored_label(adapter::tokens_for(ui).danger, error);
        }
    }

    /// What the empty module is called on its tab.
    ///
    /// The parameters a plugin declares no group for are the instrument itself — its oscillators,
    /// its filter, its envelopes — and they are what a player wants first, so this tab is built
    /// first and opens first.
    const MAIN_TAB: &'static str = "Main";

    /// The plugin's groups, in the order the parameters declare them, with the ungrouped ones
    /// first. Derived every frame from the snapshot rather than cached: a plugin can change its
    /// parameter set, and a stale tab strip would be worse than none.
    fn parameter_groups(drawable: &[&ParamSnapshot]) -> Vec<String> {
        let mut groups: Vec<String> = Vec::new();
        if drawable.iter().any(|p| p.module.is_empty()) {
            groups.push(String::new());
        }
        for param in drawable {
            if !param.module.is_empty() && !groups.iter().any(|group| group == &param.module) {
                groups.push(param.module.clone());
            }
        }
        groups
    }

    /// The group the panel is showing: the remembered one while it still exists, and otherwise the
    /// first — so unloading a plugin, or loading one whose groups differ, cannot leave the panel
    /// filtering on a tab that is not there.
    fn selected_group(groups: &[String], remembered: &str) -> String {
        groups
            .iter()
            .find(|group| group.as_str() == remembered)
            .or_else(|| groups.first())
            .cloned()
            .unwrap_or_default()
    }

    /// The tab strip. Labels come from the plugin, so it is the plugin that names its own groups.
    fn parameter_tabs(&mut self, ui: &mut egui::Ui, groups: &[String]) {
        let selected = Self::selected_group(groups, &self.parameter_tab);
        ui.horizontal_wrapped(|ui| {
            for group in groups {
                let label = if group.is_empty() {
                    Self::MAIN_TAB
                } else {
                    group.as_str()
                };
                // A step's locks live on parameters, not on views, so switching tabs never
                // disturbs them — but a lock on a parameter behind another tab is invisible, which
                // is a real hazard for a sequencer. The count says where they are.
                let locked = self.locked_parameters_in(group);
                let text = if locked > 0 {
                    format!("{label} ({locked})")
                } else {
                    label.to_owned()
                };
                if ui
                    .selectable_label(&selected == group, text)
                    .on_hover_text(if locked > 0 {
                        format!("{locked} parameter(s) locked by the selected step")
                    } else {
                        String::new()
                    })
                    .clicked()
                {
                    self.parameter_tab = group.clone();
                }
            }
        });
        ui.add_space(4.0);
    }

    /// How many of a group's parameters the selected step locks. Zero when no step is selected.
    fn locked_parameters_in(&self, group: &str) -> usize {
        if self.selected_step.is_none() {
            return 0;
        }
        self.params
            .params
            .iter()
            .filter(|p| !p.is_hidden && p.module == group)
            .filter(|p| self.step_value(p.id).is_some())
            .count()
    }

    fn parameter_panel(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default().show(root, |ui| {
            let Some(id) = self.engine.plugin_id().map(str::to_owned) else {
                ui.heading("No plugin loaded");
                // Points at the picker by its own label, so the empty state and the control agree.
                // It used to say "choose one from the list", which outlived the list.
                ui.label("Choose one from the plugin menu at the top of the window.");
                return;
            };

            ui.heading(&id);
            if let Some(envelope) = self.engine.envelope() {
                ui.small(format!(
                    "{} channels · {} · notes in: {} · notes out: {}",
                    envelope.audio.channel_count,
                    envelope.selection,
                    envelope
                        .note_input
                        .map(|p| p.dialect.label())
                        .unwrap_or("none"),
                    envelope
                        .note_output
                        .map(|p| p.dialect.label())
                        .unwrap_or("none"),
                ));
                if !envelope.note_input_carries_voice_ids() {
                    ui.colored_label(
                        adapter::tokens_for(ui).warning,
                        "This plugin speaks MIDI rather than CLAP notes, so it cannot be sent \
                         voice IDs. Cleanup after focus loss falls back to a global release when \
                         two sources hold the same pitch.",
                    );
                }
            }

            self.control_surface(ui, &id);

            ui.separator();
            let params = std::mem::take(&mut self.params);
            // Collected rather than applied in the loop, because the panel borrows `params` to
            // draw from while these describe edits to it.
            let mut edits: Vec<(u32, f64)> = Vec::new();

            // Wrap into columns by available height, then scroll only if the columns would get too
            // narrow to aim at. The invariant is **never clips**: a short window used to hide the
            // bottom of a single tall column with no way to reach it.
            let drawable: Vec<&ParamSnapshot> =
                params.params.iter().filter(|p| !p.is_hidden).collect();
            // **The plugin's own grouping is already on the wire.** CLAP carries a module path per
            // parameter and this panel already parsed it; it just never used it. A flat list was
            // right for twenty-seven parameters and stopped being right when one instrument's
            // modulation became 88 routing pairs, because 109 rows saturate the column cap at
            // every window height and the instrument's own controls end up wherever the chunking
            // puts them.
            let groups = Self::parameter_groups(&drawable);
            if groups.len() > 1 {
                // Only when there is something to choose between: a tab strip over one tab is
                // chrome that says nothing, and one tab is every effect and every instrument in
                // the collection except this one.
                self.parameter_tabs(ui, &groups);
            }
            let selected = Self::selected_group(&groups, &self.parameter_tab);
            let visible: Vec<&ParamSnapshot> = drawable
                .iter()
                .copied()
                .filter(|p| groups.len() <= 1 || p.module == selected)
                .collect();
            let available = ui.available_size_before_wrap();
            let rows = ((available.y / PARAM_ROW_HEIGHT).floor() as usize).max(1);
            let wanted = visible.len().div_ceil(rows).max(1);
            // How many columns actually fit at a usable width. Fewer than wanted means the rest
            // scrolls, which is the declared fallback rather than an accident.
            let affordable = ((available.x / MIN_PARAM_COLUMN_WIDTH).floor() as usize).max(1);
            let columns = wanted.min(affordable);
            let per_column = visible.len().div_ceil(columns).max(1);

            // Before anything is drawn, so the values and their formatting come from one moment.
            self.refresh_step_text();

            let mut outcomes: Vec<(u32, adapter::ControlOutcome, f64)> = Vec::new();
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.columns(columns, |uis| {
                    for (index, chunk) in visible.chunks(per_column).enumerate() {
                        let Some(column) = uis.get_mut(index) else {
                            continue;
                        };
                        for param in chunk {
                            // **A selected step's own value wins.** Selecting a step already makes
                            // the keyboard show that step's notes rather than what is sounding; the
                            // knobs follow the same rule, so what is on screen is what the step
                            // does. Without it you would be turning a knob away from a value you
                            // could not see, which is not sequencing but guessing.
                            let locked = self.step_value(param.id);
                            let mut value = locked.unwrap_or_else(|| {
                                *self.editing.get(&param.id).unwrap_or(&param.value)
                            });
                            // Cloned rather than borrowed: `text_entry` below wants `self`
                            // mutably, and at most a screenful of short strings is a cheaper
                            // price than threading the cache through the closure.
                            let step_text = locked
                                .and_then(|_| self.step_text.get(&Self::source_key(param.id)))
                                .cloned();
                            let entry = self.text_entry.entry(param.id).or_default();
                            let outcome = adapter::parameter(
                                column,
                                param,
                                &mut value,
                                entry,
                                self.settings.scroll_wheel_editing,
                                step_text.as_deref(),
                            );
                            outcomes.push((param.id, outcome, value));
                        }
                    }
                });
            });

            // Editing validates against the real target metadata. Return the drawing snapshot
            // before dispatching outcomes, rather than hiding every parameter from the funnel.
            self.params = params;
            for (param_id, outcome, value) in outcomes {
                if outcome.gesture_started && !self.engine.gesture_end_pending(param_id) {
                    self.engine
                        .push_gui_event(Payload::GestureBegin { param_id });
                    self.open_gestures.push(param_id);
                }
                if outcome.changed {
                    // **A click that lands back on the patch is the reset.** An instantaneous
                    // edit — gesture opened and closed in one frame: a click-jump, a keyboard or
                    // wheel nudge — arriving exactly at the patch value while a step is selected
                    // means *this step sets nothing*: the lock clears, off and no dot. A drag
                    // ending there still pins, which is the absolute-lock ruling kept.
                    let clicked_to_patch = outcome.gesture_started
                        && outcome.gesture_ended
                        && self.selected_step.is_some()
                        && self
                            .sequence_patch
                            .get(&Self::source_key(param_id))
                            .is_some_and(|patch| (value - *patch).abs() < 1e-6);
                    let reset = outcome.reset || clicked_to_patch;
                    let sent = self.parameter_moved(param_id, value, reset);
                    // **Only what was actually sent goes into the snapshot.** With a step selected,
                    // `deliver_edit` sends nothing — the edit is the step's, not the parameter's —
                    // and writing the step's value into `params` anyway put it where the base
                    // belongs. It then showed as the patch after deselecting, and one later tweak
                    // from that shown value made it the real patch: the lock collapsed to a delta of
                    // nothing, which is exactly *"it keeps the locked as a delta value"*.
                    if self.selected_step.is_none() || reset {
                        edits.push((param_id, sent));
                    }
                }
                if outcome.gesture_ended {
                    self.engine.push_gui_event(Payload::GestureEnd { param_id });
                    self.open_gestures.retain(|p| *p != param_id);
                    self.editing.remove(&param_id);
                    // The snapshot already carries the edited value, so releasing the control does
                    // not snap it back; this only asks the plugin to confirm it.
                    self.requery_params = true;
                }
            }

            self.param_columns = columns;

            // Written back so the control keeps the value it was just dragged to. Without this the
            // snapshot still held the value from when the plugin was loaded, and every control
            // jumped back to it the moment it was released.
            for (param_id, value) in edits {
                Self::note_edit(&mut self.engine, &mut self.params, param_id, value);
            }
        });
    }

    /// The sequencer, between the parameters and the keyboard.
    fn sequencer_panel(&mut self, root: &mut egui::Ui) {
        egui::Panel::bottom("sequencer")
            .exact_size(SEQUENCER_HEIGHT)
            .show(root, |ui| {
                // **Clicking the panel's empty space stops editing a step**, which is what Escape does
                // — made discoverable, because reaching for Escape is not what anyone tries first.
                //
                // **Registered before the panel's contents, so it sits underneath them.** egui gives a
                // click to the last widget registered under the pointer, so every button, field and
                // step button added below still takes its own; only bare background reaches this. That
                // ordering is the whole mechanism, so it is guarded rather than trusted:
                // `a_control_in_the_sequencer_panel_is_not_swallowed_by_the_deselect_background` and
                // `clicking_a_step_selects_it_and_clicking_again_ties_it` both fail if it inverts.
                //
                // Scoped to this panel rather than the window: while a step is selected the knobs write
                // *into* that step, so a click on the parameter panel must not deselect — that would
                // make editing a lock impossible.
                let background = ui.interact(
                    ui.max_rect(),
                    ui.id().with("deselect-background"),
                    egui::Sense::click(),
                );

                let (transport, playing_step) = self.playhead();

                // The rows in a group of their own, so their width can be read back: a panel's
                // own `min_rect` is forced to the panel's full width, and would say the rows
                // left nothing.
                let rows = ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    // The label follows the **GUI's** transport, not the playhead the audio thread
                    // publishes. Intent and position are different things: with no plugin loaded there
                    // is no worker to confirm anything, and a Play button that never changed would
                    // read as broken. The step highlight below still comes from the audio thread,
                    // which is where position actually lives.
                    let toggle = if self.sequencer.transport == Transport::Playing {
                        "⏹ Stop"
                    } else {
                        "▶ Play"
                    };
                    if ui
                        .add_sized(TRANSPORT_BUTTON_SIZE, egui::Button::new(toggle))
                        .on_hover_text("Play from step 1, or stop. Space does the same.")
                        .clicked()
                    {
                        self.play_stop();
                    }

                    ui.separator();

                    // Sized, because a `DragValue` becomes a text field when clicked and its width
                    // then follows the content — so clicking it shifted everything to its right.
                    let mut tempo = self.sequencer.tempo;
                    if ui
                        .add_sized(
                            TEMPO_FIELD_SIZE,
                            egui::DragValue::new(&mut tempo)
                                .range(sequencer::MIN_TEMPO..=sequencer::MAX_TEMPO)
                                .speed(0.5)
                                .suffix(" BPM"),
                        )
                        .changed()
                    {
                        self.set_tempo(tempo);
                    }

                    ui.separator();
                    if ui
                        .button("Random")
                        .on_hover_text(
                            "Fill the shown bar with a monophonic sequence in C Dorian, so you can \
                         hear the synth without playing anything. Parameter locks are kept, and \
                         the sequence grows to reach the bar.",
                        )
                        .clicked()
                    {
                        self.randomise();
                    }
                    if ui.button("Clear").clicked() {
                        self.clear_pattern();
                    }

                    ui.separator();
                    self.sequence_controls(ui);
                });

                self.bar_strip(ui);
                self.step_row(ui, playing_step, transport);

                // Exactly one line, always. The keyboard is modal while a step is selected and that
                // has to be obvious — but a panel that grows a row when a load has something to say
                // moves the whole interface, which is the defect this replaces.
                ui.horizontal(|ui| {
                    match self.selected_step {
                        Some(step) => {
                            // The second click ties rather than deselecting, so the line has to say
                            // both what that click will do *and* how to stop — a gesture that changed
                            // meaning is only discoverable if the hint that described it changes too.
                            // **Read from `tie_action`, not written beside it.** What the second click
                            // does depends on what the step holds, so a hint composed separately would
                            // be a second implementation of that rule, free to describe the wrong one.
                            let next_click = match self.tie_action(step) {
                                TieAction::Untie(_) => "click again to untie".to_owned(),
                                TieAction::Tie(_) => "click again to tie".to_owned(),
                                TieAction::Slide(_) => "click again to slide".to_owned(),
                            };

                            // What a tied step *is* now needs a word — a pitch lands here, so the
                            // old "notes go to step N" is gone with the redirect it described.
                            let pattern = &self.sequencer.pattern;
                            let text = if pattern.tied(step) && !pattern.step(step).is_empty() {
                                format!(
                                    "Editing step {} — a slide. {}, Esc to stop.",
                                    step + 1,
                                    next_click[..1].to_uppercase() + &next_click[1..]
                                )
                            } else if pattern.tied(step) {
                                format!(
                                    "Editing step {} — held; play a note to slide here. {}, Esc to stop.",
                                    step + 1,
                                    // Capitalised because it starts a sentence here and not there.
                                    next_click[..1].to_uppercase() + &next_click[1..]
                                )
                            } else {
                                format!("Editing step {} — {next_click}, Esc to stop.", step + 1)
                            };
                            // A token, not a literal: this was a hard-coded amber, which is the escape
                            // `crates/ui/AGENTS.md` (in mxm-kit) names — *a colour chosen in a
                            // panel is a token that has escaped*. Editing is a mode, and `warning`
                            // is the mode colour.
                            ui.colored_label(adapter::tokens_for(ui).warning, text);
                        }
                        None => {
                            ui.weak("Click a step to edit it with the keyboard.");
                        }
                    }

                    // What a load could not keep goes behind a marker rather than into rows. The
                    // detail is on hover; the geometry never changes.
                    if !self.sequence_problems.is_empty() {
                        ui.separator();
                        ui.colored_label(
                            adapter::tokens_for(ui).danger,
                            format!("⚠ {}", self.sequence_problems.len()),
                        )
                        .on_hover_text(self.sequence_problems.join(
                            "
",
                        ));
                    }
                });

                }).response.rect;

                // **The device rail lives in this panel's free right region** — the owner's
                // placement (2026-09-04): right of the sequencer's rows, so it is on screen
                // whether or not the parameter panel is collapsed, which is exactly when the
                // player sits beside an effect's editor. Drawn after the rows so their width is
                // measured rather than guessed; the rail takes only what they leave.
                let panel = ui.max_rect();
                let free = egui::Rect::from_min_max(
                    egui::pos2(rows.right() + fx_rail::RAIL_INSET, panel.top()),
                    egui::pos2(panel.right(), panel.bottom()),
                );
                self.fx_rail(ui, free);

                // Read after the contents so the guard above is the whole story: if anything in this
                // panel took the click, `background` never saw it. `deselect_step` is the same call
                // Escape makes — one decision, two ways to reach it — and it is a no-op with nothing
                // selected, so a stray click on the panel costs nothing.
                if background.clicked() {
                    self.deselect_step();
                }
            });
    }

    /// What a step holds, in words — the accessible name and the tooltip.
    ///
    /// This is where the note names went when the visible label became fixed-width. A screen
    /// reader gets one string per step rather than a run of separate labels, which is an
    /// improvement on what it had.
    /// What a step holds, as a sentence. The only place a screen reader or a test can learn it.
    ///
    /// The tie is named rather than implied, because the marker that shows it is a shape and a
    /// shape has no accessible form. "Tied" is said first: it is what the step *is*, and its notes
    /// are a further fact about it.
    fn step_description(index: usize, step: sequencer::Step, tied: bool) -> String {
        let number = index + 1;
        match (tied, step.is_empty()) {
            (false, true) => format!("Step {number}: empty"),
            (true, true) => format!("Step {number}: tied, holding the previous note"),
            (tied, false) => {
                let notes: Vec<String> = step
                    .notes()
                    .into_iter()
                    .map(sequencer::pattern::note_name)
                    .collect();
                let notes = notes.join(", ");
                if tied {
                    // The word is **slide**: the gate stayed open and the pitch moved. The joint
                    // marker that shows it is a shape, and a shape has no accessible form, so this
                    // is where a screen reader — or a test — learns the step slides.
                    format!("Step {number}: slide to {notes}")
                } else {
                    format!("Step {number}: {notes}")
                }
            }
        }
    }

    /// Sixteen step buttons, with the playing one highlighted from the audio thread.
    ///
    /// **A tied run is drawn as one wide button, not as a bar laid over several.** The first
    /// version drew a bar whose width was the note's duration, which said the right thing and said
    /// it in the wrong place: a mark spanning three buttons contradicts the buttons it spans. The
    /// tie joins the steps, so it is the *control* that joins — no divider, one outline, one shape.
    /// The bar goes back to its own job, which is saying that a step holds notes.
    ///
    /// That split also removes a defect the combined mark had by construction. A tie continuing
    /// nothing — the first step, or one after a rest — drew a full-width bar promising a long note
    /// that does not sound. Now the bar follows the notes and only the notes, so there is nothing
    /// left for it to overstate.
    /// §1 of the arbitrary-length plan: the bar pager, the chips, the shape, copy/paste/clear,
    /// and the loop scope. The step row below always shows exactly one bar; the chips are the
    /// only way between bars.
    /// The eight bar chips: which bars hold notes, which one is shown, and which one is sounding.
    fn bar_strip(&mut self, ui: &mut egui::Ui) {
        let tokens = adapter::tokens_for(ui);
        let bars = self.sequencer.pattern.bars();
        let spb = self.sequencer.pattern.steps_per_bar();
        let playing_bar = self.playing_bar();

        ui.horizontal(|ui| {
            ui.label("Bar");

            // **Always eight chips** — the current pattern's bars, whatever the sequence's
            // length. The player is a loop workbench, not a DAW: a person works in eight bars,
            // and a longer sequence (the CLI can generate one of any length) is reached by
            // stepping the pattern window, never by enumerating it.
            let pattern_index = self.selected_bar / PATTERN_BARS;
            if ui
                .add_enabled(pattern_index > 0, egui::Button::new("\u{2039}"))
                .on_hover_text("The previous eight bars")
                .clicked()
            {
                self.select_bar((pattern_index - 1) * PATTERN_BARS);
            }

            let first = pattern_index * PATTERN_BARS;
            for bar in first..first + PATTERN_BARS {
                // A bar beyond the end is viewable and empty; it becomes real when notes land in
                // it, so it draws exactly like an empty bar — the counter is what says how many
                // exist. Guarded reads: past the length they would wrap.
                let held = bar < bars && {
                    let start = bar * spb;
                    (0..spb).any(|offset| !self.sequencer.pattern.step(start + offset).is_empty())
                };
                let current = bar == self.selected_bar;
                let sounding = playing_bar == Some(bar);
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(24.0, 20.0), egui::Sense::click());
                let painter = ui.painter();
                // Filled = holds notes; the current bar outlined. Fill and outline, not hue
                // alone, per the design system.
                //
                // **Playing takes the fill, shown keeps the outline**, so the two never compete
                // for the same pixels and a bar can say both at once -- which is the common case,
                // since the shown bar is usually the one being auditioned. `success` is the same
                // token the step row lights the sounding step with: one colour means "this is
                // sounding now" everywhere in the panel, rather than each row inventing its own.
                painter.rect_filled(
                    rect,
                    mxm_ui::space::RADIUS,
                    if sounding {
                        tokens.success
                    } else if held {
                        tokens.surface_3
                    } else {
                        tokens.surface_2
                    },
                );
                painter.rect_stroke(
                    rect,
                    mxm_ui::space::RADIUS,
                    egui::Stroke::new(
                        if current {
                            mxm_ui::space::HAIRLINE * 2.0
                        } else {
                            mxm_ui::space::HAIRLINE
                        },
                        if current {
                            tokens.accent
                        } else if response.hovered() {
                            tokens.border_strong
                        } else {
                            tokens.border
                        },
                    ),
                    egui::StrokeKind::Inside,
                );
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    (bar + 1).to_string(),
                    egui::FontId::monospace(9.0),
                    if sounding {
                        // **`canvas`, the ink the step row uses on this same fill.** Everything
                        // drawn over `success` there -- the note bar, the lock dot and the step
                        // number -- switches to it, because ordinary text colours are chosen to
                        // sit on a surface token and go muddy on an accent. One fill, one ink.
                        tokens.canvas
                    } else if held {
                        tokens.text_primary
                    } else {
                        tokens.text_secondary
                    },
                );
                let described = format!(
                    "Bar {}{}{}{}",
                    bar + 1,
                    if held { ", holds notes" } else { ", empty" },
                    if current { ", shown" } else { "" },
                    if sounding { ", playing" } else { "" }
                );
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::SelectableLabel,
                        true,
                        current,
                        described.clone(),
                    )
                });
                if response.on_hover_text(described).clicked() {
                    self.select_bar(bar);
                }
            }

            if ui
                .add(egui::Button::new("\u{203a}"))
                .on_hover_text("The next eight bars. Bars are added when notes land in them.")
                .clicked()
            {
                self.select_bar((pattern_index + 1) * PATTERN_BARS);
            }
            ui.label(format!("{}-{}", first + 1, first + PATTERN_BARS));

            ui.separator();
            // **Click-steppers plus typed entry, no dragging.** A drag fired a resize per frame,
            // fought the confirmation, and applied whatever value the drag died on rather than
            // the one on screen.
            if ui
                .small_button("\u{2212}")
                .on_hover_text("One bar fewer - deletes the last bar")
                .clicked()
            {
                self.set_bars(bars.saturating_sub(1).max(1));
            }
            let mut total = bars;
            let bars_field = ui.add(
                egui::DragValue::new(&mut total)
                    .range(1..=9999)
                    .speed(0.0)
                    .update_while_editing(false),
            );
            let bars_field = bars_field.on_hover_text(
                "How many bars the sequence plays - the last bar with content. Type a smaller \
                 number to delete from the end.",
            );
            if ui.small_button("+").on_hover_text("One bar more").clicked() {
                self.set_bars(bars + 1);
            }
            if bars_field.changed() && total != bars {
                self.set_bars(total);
            }
            ui.label(if bars == 1 { "bar of" } else { "bars of" });

            if ui
                .small_button("\u{2212}")
                .on_hover_text("One step fewer per bar - cuts every bar's last step")
                .clicked()
            {
                self.set_steps_per_bar(spb.saturating_sub(1).max(1));
            }
            let mut steps = spb;
            let steps_field = ui.add(
                egui::DragValue::new(&mut steps)
                    .range(1..=crate::sequencer::pattern::MAX_STEPS_PER_BAR)
                    .speed(0.0)
                    .update_while_editing(false),
            );
            let steps_field = steps_field.on_hover_text(
                "How many steps each bar holds. A step stays a sixteenth: twelve is a 3/4 bar.",
            );
            if ui
                .small_button("+")
                .on_hover_text("One step more per bar")
                .clicked()
            {
                self.set_steps_per_bar((spb + 1).min(crate::sequencer::pattern::MAX_STEPS_PER_BAR));
            }
            if steps_field.changed() && steps != spb {
                self.set_steps_per_bar(steps);
            }
            ui.label("steps");
        });

        ui.horizontal(|ui| {
            // Visible buttons, never a hover-reveal or a hidden menu. What Copy and Clear act on
            // follows the Bar/Pattern toggle beside them; Paste applies whatever was copied.
            if ui
                .button("Copy")
                .on_hover_text("Copy the notes, ties and parameter locks")
                .clicked()
            {
                if self.tools_on_pattern {
                    self.copy_pattern();
                } else {
                    self.copy_bar();
                }
            }
            if ui
                .button("Paste")
                .on_hover_text(
                    "Replace with what was copied - a bar, or a whole pattern. Locks paste \
                     only into the instrument they were copied under.",
                )
                .clicked()
            {
                self.paste_clipboard();
            }
            if ui
                .button(if self.tools_on_pattern {
                    "Clear pattern"
                } else {
                    "Clear bar"
                })
                .on_hover_text("Empty the notes, ties and parameter locks")
                .clicked()
            {
                if self.tools_on_pattern {
                    self.clear_pattern_bars();
                } else {
                    self.clear_bar();
                }
            }
            // `toggle_button`, not `selectable_label`: the hover-only frame materialises pixels
            // that read as the row jumping - and a click whose press and release straddle that
            // shift is dropped, which made these buttons feel dead. The same fault, and fix, as
            // the top bar's toggles.
            for (pattern, label, hover) in [
                (false, "Bar", "Copy and Clear act on the shown bar."),
                (true, "Pattern", "Copy and Clear act on these eight bars."),
            ] {
                if ui
                    .add(toggle_button(ui, label, self.tools_on_pattern == pattern))
                    .on_hover_text(hover)
                    .clicked()
                {
                    self.tools_on_pattern = pattern;
                }
            }

            ui.separator();
            ui.label("Loop");
            // Compact toggles, not the shared segmented control: that widget divides the whole
            // width it is given into equal cells - right inside a card, absurd on a toolbar row,
            // where it drew half-screen buttons. The labels name what will actually repeat -
            // "Bar 9-16" when the second pattern is on screen - because "Bar"/"Pattern" next
            // to the Bar|Pattern tools toggle read as the same words meaning different things.
            let first = self.selected_bar / PATTERN_BARS * PATTERN_BARS;
            for (scope, label, hover) in [
                (
                    LoopScope::Bar,
                    "1 Bar".to_owned(),
                    "Repeat only the shown bar, round and round - the audition loop.",
                ),
                (
                    LoopScope::Pattern,
                    format!("Bar {}-{}", first + 1, first + PATTERN_BARS),
                    "Repeat these eight bars - the loop you would export.",
                ),
                (
                    LoopScope::All,
                    "All bars".to_owned(),
                    "Repeat the whole sequence. Switching lands at the current bar's end, \
                     never mid-bar.",
                ),
            ] {
                if ui
                    .add(toggle_button(ui, &label, self.loop_scope == scope))
                    .on_hover_text(hover)
                    .clicked()
                {
                    self.set_loop_scope(scope);
                }
            }
        });
    }

    fn step_row(&mut self, ui: &mut egui::Ui, playing_step: usize, transport: Transport) {
        // **The row always shows exactly one bar** — the selected one; the chips are the only way
        // between bars. `origin` is that bar's first absolute step, and everything the row says to
        // the rest of the player stays in absolute steps, so the CLI, the dump and a saved file
        // never renumber when the view pages.
        let steps = self.sequencer.pattern.steps_per_bar();
        let origin = self.selected_bar * steps;
        let total = self.sequencer.pattern.len();
        // A shown bar may lie beyond the sequence's end - viewing is free, and the bar becomes
        // real when notes land in it. Reads past the length wrap, so every read is guarded: a
        // virtual step is an empty, untied step with no locks.
        let tied_at = |index: usize| index < total && self.sequencer.pattern.tied(index);
        let step_at = |index: usize| {
            if index < total {
                self.sequencer.pattern.step(index)
            } else {
                sequencer::Step::EMPTY
            }
        };

        let mut clicked = None;
        let modifiers = ui.input(|i| i.modifiers);
        ui.horizontal(|ui| {
            let gap = ui.spacing().item_spacing.x;

            // **Allocated first, painted afterwards.** A run spans several cells and its bounds are
            // not known until they have all been laid out. Fixed geometry, as everything in this
            // row is: the size never depends on content, so a step gaining notes — or a tie — moves
            // nothing.
            let mut cells: Vec<(egui::Rect, egui::Response)> = Vec::with_capacity(steps);
            for _ in 0..steps {
                cells.push(ui.allocate_exact_size(STEP_BUTTON_SIZE, egui::Sense::click()));
            }

            let whole = cells[0].0.union(cells[steps - 1].0);
            if ui.is_rect_visible(whole) {
                let tokens = adapter::tokens_for(ui);
                let painter = ui.painter();
                let radius = mxm_ui::space::RADIUS;

                // A run is a step plus every tied step following it — **a slide included**: the
                // owner's ruling is that the tied note reads as one big step, so the button does
                // not split at a note-carrying tie. What says a slide is there is the standard
                // note bar on that step, which already follows the notes and only the notes.
                // `tied(0)` makes the first run a continuation too — of the previous pass through
                // the pattern, since the tie reaches backwards across the loop point exactly as
                // the runtime's lookahead does.
                let mut runs: Vec<(usize, usize)> = Vec::new();
                let mut first = 0usize;
                while first < steps {
                    let mut last = first;
                    while last + 1 < steps && tied_at(origin + last + 1) {
                        last += 1;
                    }
                    runs.push((first, last));
                    first = last + 1;
                }
                // A run reaching in from before this bar, or carrying on past it — the previous
                // bar, the next one, or the loop point; the row cannot show where it joins either
                // way, so those ends draw square.
                let enters = tied_at(origin);
                let leaves = tied_at((origin + steps) % total);

                for (first, last) in runs {
                    // Open ends are where the run continues off this bar's row.
                    let open_left = first == 0 && enters;
                    let open_right = last == steps - 1 && leaves;
                    let corners = |left: bool, right: bool| egui::CornerRadius {
                        nw: if left { 0 } else { radius },
                        sw: if left { 0 } else { radius },
                        ne: if right { 0 } else { radius },
                        se: if right { 0 } else { radius },
                    };

                    let run_rect = egui::Rect::from_min_max(
                        cells[first].0.min,
                        egui::pos2(cells[last].0.right(), cells[last].0.bottom()),
                    );
                    painter.rect_filled(run_rect, corners(open_left, open_right), tokens.surface_2);

                    // Playing and hover stay **per step**, drawn over the run's own fill. The run
                    // says how long the note is; these say which step you are looking at, and
                    // losing that to the merge would make a wide button unclickable in the dark.
                    for (offset, (cell, response)) in cells[first..=last].iter().enumerate() {
                        let index = first + offset;
                        let cell = *cell;
                        let is_playing =
                            transport == Transport::Playing && playing_step == origin + index;
                        let hovered = response.hovered();
                        if !is_playing && !hovered {
                            continue;
                        }
                        // Into the gaps on joined sides, so no sliver of the run's fill shows
                        // through between two cells of one button.
                        let left = if index > first {
                            cell.left() - gap / 2.0
                        } else {
                            cell.left()
                        };
                        let right = if index < last {
                            cell.right() + gap / 2.0
                        } else {
                            cell.right()
                        };
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(left, cell.top()),
                                egui::pos2(right, cell.bottom()),
                            ),
                            corners(index > first || open_left, index < last || open_right),
                            if is_playing {
                                tokens.success
                            } else {
                                tokens.surface_3
                            },
                        );
                    }

                    // §7.2: a resting control has a one-pixel border, and hover strengthens it
                    // rather than being what makes it look like a control. Drawn last, so the
                    // per-step fills above cannot paint over the outline that makes it one button.
                    let touched = (first..=last).any(|index| cells[index].1.hovered());
                    painter.rect_stroke(
                        run_rect,
                        corners(open_left, open_right),
                        egui::Stroke::new(
                            mxm_ui::space::HAIRLINE,
                            if touched {
                                tokens.border_strong
                            } else {
                                tokens.border
                            },
                        ),
                        egui::StrokeKind::Inside,
                    );
                }

                // Selection is a mode — the keyboard is writing into this step — so it outlines the
                // **step**, not the run. Inside a wide button that is exactly the point: it says
                // which of the joined steps a note would land on.
                if let Some(selected) = self
                    .selected_step
                    .filter(|step| (origin..origin + steps).contains(step))
                {
                    painter.rect_stroke(
                        cells[selected - origin].0,
                        radius,
                        egui::Stroke::new(mxm_ui::space::HAIRLINE * 2.0, tokens.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                // The companions outline thinner: selected, but not the one that previews.
                for companion in &self.also_selected {
                    if (origin..origin + steps).contains(companion) {
                        painter.rect_stroke(
                            cells[companion - origin].0,
                            radius,
                            egui::Stroke::new(mxm_ui::space::HAIRLINE, tokens.accent),
                            egui::StrokeKind::Inside,
                        );
                    }
                }

                for (offset, (cell, _)) in cells.iter().enumerate() {
                    let index = origin + offset;
                    let step = step_at(index);
                    let is_playing = transport == Transport::Playing && playing_step == index;

                    // **The bar says the step holds notes, and nothing else.** Duration is the
                    // button's business now. Left-aligned and half a cell wide because that is
                    // where and how long the note itself sounds — the run around it is what says
                    // whether anything holds it open.
                    if !step.is_empty() {
                        // **Centred over the number**, which is the only thing under it. It used to
                        // be left-aligned, back when its width was the note's duration and the left
                        // edge was where the note began. Duration is the button's extent now, so
                        // that alignment said nothing and only looked off-centre.
                        let inset = mxm_ui::space::SPACE_1;
                        let top = cell.top() + cell.height() * 0.28;
                        painter.rect_filled(
                            egui::Rect::from_center_size(
                                egui::pos2(cell.center().x, top + mxm_ui::space::SPACE_2 / 2.0),
                                egui::vec2(
                                    (cell.width() - inset * 2.0) * STEP_MARKER_WIDTH,
                                    mxm_ui::space::SPACE_2,
                                ),
                            ),
                            radius / 2,
                            if is_playing {
                                tokens.canvas
                            } else {
                                tokens.text_primary
                            },
                        );
                    }

                    // **A dot says the step sets something besides its notes.**
                    //
                    // A different *shape* in a different *place* from the note bar, not a different
                    // colour of it — §7.2. The bar is what the step plays and sits over the number;
                    // this is an annotation about the step, so it goes in the corner, where a mark
                    // on a form goes. A person who has never been told what it means can still see
                    // that these two steps are not like the others, which is the whole job.
                    // **On every step that locks something**, tied ones included: locks are per
                    // step, a hold's lock moves the parameter under the held note, and the dot
                    // draws exactly where the value is heard.
                    if index < total && self.sequencer.locks.step_has_locks(index) {
                        painter.circle_filled(
                            egui::pos2(
                                cell.right() - mxm_ui::space::SPACE_2,
                                cell.top() + mxm_ui::space::SPACE_2,
                            ),
                            mxm_ui::space::HAIRLINE * 2.0,
                            if is_playing {
                                tokens.canvas
                            } else {
                                tokens.text_primary
                            },
                        );
                    }

                    // The number is a label on the panel, not part of the switch: small, quiet, and
                    // below the marker. A player counts steps from it; it never states the state.
                    painter.text(
                        egui::pos2(cell.center().x, cell.bottom() - mxm_ui::space::SPACE_2),
                        egui::Align2::CENTER_BOTTOM,
                        (offset + 1).to_string(),
                        egui::FontId::monospace(10.0),
                        if is_playing {
                            tokens.canvas
                        } else {
                            tokens.text_secondary
                        },
                    );
                }
            }

            for (offset, (_, response)) in cells.into_iter().enumerate() {
                let index = origin + offset;
                // **Numbered within the bar** - the row always reads one to n; the chips say
                // which bar this is.
                let described = Self::step_description(offset, step_at(index), tied_at(index));
                let selected = self.selected_step == Some(index);
                let response = response.on_hover_text(described.clone());

                // A painted control has no accessible name of its own, and the notes never appear
                // in the visible label — so this is the only place a screen reader, or a UI test,
                // can learn what the step holds.
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::SelectableLabel,
                        true,
                        selected,
                        described.clone(),
                    )
                });

                if response.clicked() {
                    clicked = Some(index);
                }
            }
        });
        if let Some(index) = clicked {
            // Text selection's gestures: shift extends a range from the anchor, ctrl toggles the
            // one step, a plain click selects it alone (and a second plain click ties, as ever).
            if modifiers.shift {
                self.perform(Gesture::ShiftClickStep { step: index });
            } else if modifiers.command {
                self.perform(Gesture::CtrlClickStep { step: index });
            } else {
                self.perform(Gesture::ClickStep { step: index });
            }
        }
    }

    /// Saving and loading. Save offers audio or MIDI; Load takes a sequence, never audio.
    fn sequence_controls(&mut self, ui: &mut egui::Ui) {
        // Labelled, not merely hinted. A placeholder disappears the moment you type, so on its own
        // it leaves the field nameless to a screen reader — and ambiguous to a test.
        let label = ui.label("Name");
        ui.add(
            egui::TextEdit::singleline(&mut self.sequence_name)
                .desired_width(110.0)
                .hint_text("sequence"),
        )
        .labelled_by(label.id);

        ui.menu_button("Save", |ui| {
            // Normalising is right for making samples and wrong for comparing patch levels, so it
            // is a choice rather than a policy — defaulted to the common case.
            ui.checkbox(&mut self.normalise_export, "Normalise to 0 dBFS")
                .on_hover_text(
                    "On: every export peaks at 0 dBFS, which is what a sample library wants. \
                     Off: the file carries the level it was rendered at, so two exports can be \
                     compared for loudness.",
                );
            ui.separator();

            if ui.button("Audio (.wav)").clicked() {
                let name = self.sequence_name.clone();
                if let Err(reason) = self.export_audio(&name) {
                    self.status = Some(reason);
                }
                ui.close();
            }
            if ui.button("MIDI sequence (.mid)").clicked() {
                let name = self.sequence_name.clone();
                if let Err(reason) = self.save_midi(&name) {
                    self.status = Some(reason);
                }
                ui.close();
            }
        });

        let saved = self.loadable_sequences();
        ui.menu_button(format!("Load ({})", saved.len()), |ui| {
            if saved.is_empty() {
                ui.weak("Nothing to load yet.");
                return;
            }
            for (name, path) in saved {
                if ui.button(&name).clicked() {
                    let outcome = if path.extension().is_some_and(|e| e == "mid") {
                        self.load_midi(&path)
                    } else {
                        self.load_sequence(&path)
                    };
                    if let Err(reason) = outcome {
                        self.status = Some(reason);
                    }
                    ui.close();
                }
            }
        });

        if ui
            .button("Open folder")
            .on_hover_text("Where saved files go.")
            .clicked()
        {
            let path = self.exports_dir.clone();
            let _ = std::fs::create_dir_all(&path);
            self.status = Some(format!("Exports are in {}", path.display()));
            open_folder(&path);
        }
    }

    fn keyboard_panel(&mut self, root: &mut egui::Ui) {
        egui::Panel::bottom("keyboard")
            .exact_size(KEYBOARD_HEIGHT)
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("Octave {}", self.octave));
                    if ui.button("−").clicked() {
                        self.shift_octave(-1);
                    }
                    if ui.button("+").clicked() {
                        self.shift_octave(1);
                    }

                    let mut sustain = self.sustain;
                    if ui
                        .checkbox(&mut sustain, "Sustain")
                        .on_hover_text(
                            "Holds notes in the host. CC 64 is deliberately not forwarded: \
                             mxm-mono-01 ignores sustain, and a control that silently does nothing \
                             is worse than no control.",
                        )
                        .changed()
                    {
                        self.set_sustain(sustain);
                    }

                    let bend = ui.add(
                        egui::Slider::new(&mut self.pitch_bend, 0.0..=1.0)
                            .text("Bend")
                            .show_value(false),
                    );
                    if bend.changed() {
                        self.engine.push_gui_event(Payload::PitchBend {
                            channel: 0,
                            value: f64::from(self.pitch_bend),
                        });
                    }
                    if bend.drag_stopped() {
                        self.pitch_bend = 0.5;
                        self.engine.push_gui_event(Payload::PitchBend {
                            channel: 0,
                            value: 0.5,
                        });
                    }

                    if ui
                        .add(
                            egui::Slider::new(&mut self.mod_wheel, 0.0..=1.0)
                                .text("Mod")
                                .show_value(false),
                        )
                        .changed()
                    {
                        self.engine.push_gui_event(Payload::ControlChange {
                            channel: 0,
                            controller: 1,
                            value: (self.mod_wheel * 127.0) as u8,
                        });
                    }

                    if ui
                        .button("Panic")
                        .on_hover_text("Release everything, everywhere")
                        .clicked()
                    {
                        self.engine.push_gui_event(Payload::GlobalPanic);
                        self.held.clear();
                    }
                });

                self.draw_keys(ui);
            });
    }

    /// The step a played note would land in: **the selected one**, tied or not.
    ///
    /// **The keyboard's highlight and the note it writes have to be the same step.** With the
    /// redirect gone (the owner's Option A), both answer the raw selection: a pitch lands on the
    /// step itself — a hold becomes a slide — so a hold shows an empty keyboard honestly, and
    /// repitching a held note means selecting its head. The lock paths read the same answer,
    /// because locks are per step too.
    fn editing_step(&self) -> Option<usize> {
        self.selected_step
    }

    /// The notes of the step being edited, if one is selected.
    fn selected_step_notes(&self) -> sequencer::Step {
        self.editing_step()
            .map(|step| self.sequencer.pattern.step(step))
            .unwrap_or(sequencer::Step::EMPTY)
    }

    /// Whether `note` is being held by **any** input path.
    ///
    /// `held` is only what the GUI itself originated — the on-screen keys and the computer
    /// keyboard. A note played on a connected MIDI keyboard is published by the MIDI callback
    /// instead, and without this the key it sounds never lit.
    fn is_sounding(&self, note: u8) -> bool {
        self.held.contains(&note) || self.midi_held.contains(&note)
    }

    /// Everything sounding right now, from every source, lowest first.
    fn sounding(&self) -> Vec<u8> {
        let mut notes = self.held.clone();
        for note in &self.midi_held {
            if !notes.contains(note) {
                notes.push(*note);
            }
        }
        notes.sort_unstable();
        notes
    }

    /// Which of the three states a key is in, as a colour.
    ///
    /// Deliberately *not* the playing step's notes: a keyboard flickering sixteen times a bar is
    /// noise, and the step row already shows position.
    fn key_colour(&self, note: u8, selected: sequencer::Step, resting: Color32) -> Color32 {
        match (self.is_sounding(note), selected.contains(note)) {
            (true, true) => HELD_AND_SELECTED_KEY,
            (true, false) => HELD_KEY,
            (false, true) => SELECTED_KEY,
            (false, false) => resting,
        }
    }

    /// Click and drag; vertical position within the key sets velocity. Held notes are
    /// highlighted, including ones arriving over MIDI, so the keyboard doubles as a monitor.
    ///
    /// While a step is selected its notes are marked here too — this is where the note names went
    /// when the step row became fixed-width.
    fn draw_keys(&mut self, ui: &mut egui::Ui) {
        let selected_notes = self.selected_step_notes();
        let available = ui.available_size_before_wrap();
        // Fixed key width: a wider window reveals more of the keyboard rather than stretching the
        // same keys, which is what a longer keyboard means on any other instrument.
        let count = keyboard::white_key_count(available.x);
        let whites = keyboard::white_keys(self.octave, count);
        let blacks = keyboard::black_keys(self.octave, count);
        if whites.is_empty() {
            return;
        }

        let white_width = keyboard::WHITE_KEY_WIDTH;
        let white_height = available.y.max(24.0);
        let black_width = white_width * keyboard::BLACK_KEY_WIDTH;
        let black_height = white_height * keyboard::BLACK_KEY_HEIGHT;

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(white_width * whites.len() as f32, white_height),
            egui::Sense::click_and_drag(),
        );
        let painter = ui.painter_at(rect);

        let white_rect = |index: usize| {
            egui::Rect::from_min_size(
                rect.min + egui::vec2(white_width * index as f32, 0.0),
                egui::vec2(white_width - 1.0, white_height),
            )
        };
        let black_rect = |key: &keyboard::BlackKey| {
            egui::Rect::from_min_size(
                rect.min + egui::vec2(key.centre * white_width - black_width / 2.0, 0.0),
                egui::vec2(black_width, black_height),
            )
        };

        for (index, note) in whites.iter().enumerate() {
            let key = white_rect(index);
            painter.rect(
                key,
                2.0,
                self.key_colour(*note, selected_notes, WHITE_KEY),
                egui::Stroke::new(1.0, KEY_BORDER),
                egui::StrokeKind::Inside,
            );

            // The second channel, so "in this step" never depends on hue alone.
            if selected_notes.contains(*note) {
                painter.circle_filled(
                    egui::pos2(key.center().x, key.min.y + 8.0),
                    3.0,
                    SELECTED_MARK,
                );
            }

            // Only the Cs are named. A label on every key is noise; the Cs are what a player
            // counts from, and they make the current octave readable at a glance.
            if let Some(label) = keyboard::octave_label(*note) {
                painter.text(
                    egui::pos2(key.center().x, key.max.y - 4.0),
                    egui::Align2::CENTER_BOTTOM,
                    label,
                    egui::FontId::proportional(10.0),
                    KEY_LABEL,
                );
            }
        }

        // Drawn after the white keys, so they sit on top where a player expects them.
        for key in &blacks {
            let rect = black_rect(key);
            painter.rect_filled(
                rect,
                2.0,
                self.key_colour(key.note, selected_notes, BLACK_KEY),
            );
            if selected_notes.contains(key.note) {
                painter.circle_filled(
                    egui::pos2(rect.center().x, rect.min.y + 8.0),
                    3.0,
                    SELECTED_MARK,
                );
            }
        }

        // A step can hold notes the keyboard is not showing — it paints a width- and
        // octave-dependent run, while a step holds any note 0..=127. Without this the step would
        // show a filled marker and the keyboard nothing, which is worse than the labels this
        // replaced.
        if let (Some(lowest), Some(highest)) = (whites.first(), whites.last()) {
            let below = selected_notes.notes().into_iter().any(|n| n < *lowest);
            let above = selected_notes.notes().into_iter().any(|n| n > *highest);
            for (present, corner, arrow) in [
                (below, rect.left_top(), "◀"),
                (above, rect.right_top(), "▶"),
            ] {
                if present {
                    painter.text(
                        corner + egui::vec2(if arrow == "◀" { 8.0 } else { -8.0 }, 10.0),
                        egui::Align2::CENTER_CENTER,
                        arrow,
                        egui::FontId::proportional(13.0),
                        SELECTED_KEY,
                    );
                }
            }
        }

        let pointer = response.interact_pointer_pos();
        // Hit-tested before the white keys for the same reason they are drawn after them: a black
        // key overlaps the two whites it sits between, and the pointer is on whichever is on top.
        let hit = pointer.and_then(|pos| {
            blacks
                .iter()
                .find(|key| black_rect(key).contains(pos))
                .map(|key| (key.note, black_height))
                .or_else(|| {
                    let index = ((pos.x - rect.min.x) / white_width).floor();
                    usize::try_from(index as i64)
                        .ok()
                        .and_then(|i| whites.get(i))
                        .map(|note| (*note, white_height))
                })
        });

        if (response.drag_stopped() || response.clicked())
            && let Some(note) = self.dragging.take()
        {
            self.note_off(note);
        }

        // Losing pointer capture mid-drag runs the same targeted cleanup as focus loss.
        if !response.is_pointer_button_down_on()
            && self.dragging.is_some()
            && !response.dragged()
            && let Some(note) = self.dragging.take()
        {
            self.note_off(note);
        }

        if response.is_pointer_button_down_on()
            && let (Some((note, height)), Some(pos)) = (hit, pointer)
            && self.dragging != Some(note)
        {
            if let Some(previous) = self.dragging.take() {
                self.note_off(previous);
            }
            // Measured against the key actually struck, so the bottom of a black key is as loud
            // as the bottom of a white one.
            let fraction = ((pos.y - rect.min.y) / height).clamp(0.0, 1.0);
            self.note_on(note, keyboard::velocity_from_position(fraction));
            self.dragging = Some(note);
        }
    }
}

/// Whether a key is one the player consumes, so text fields keep their own typing.
pub fn is_playing_key(key: Key) -> bool {
    keyboard::key_to_note(key, 3).is_some()
        || keyboard::is_octave_down(key)
        || keyboard::is_octave_up(key)
}

// --- file helpers -------------------------------------------------------------------------------

/// A path that does not already exist, by adding a suffix.
///
/// **Never overwrites silently.** Saving twice under one name gives two files, and the export says
/// which one it actually wrote.
fn unique_path(dir: &Path, stem: &str, extension: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{extension}"));
    if !first.exists() {
        return first;
    }
    for n in 2..1000 {
        let candidate = dir.join(format!("{stem}-{n}.{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

/// Writes via a temporary file and a rename, so a crash mid-write cannot leave a half-written file
/// looking like a finished one.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, path).map_err(|e| e.to_string())
}

/// Files in `dir` with the given extension, by stem, sorted.
fn list_with_extension(dir: &Path, extension: &str) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == extension))
        .filter_map(|path| {
            let name = path.file_stem()?.to_str()?.to_owned();
            Some((name, path))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Opens a folder in the system file browser.
///
/// Best effort and deliberately silent on failure: the path is already in the status line, which is
/// the part that matters.
fn open_folder(path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

#[cfg(test)]
mod geometry_tests {
    use super::geometry_worth_remembering;

    /// A maximised or fullscreen window is a state, not a size: remembering its width brought the
    /// player back screen-wide with its borders on (the owner's report, 2026-09-04).
    #[test]
    fn a_maximised_or_fullscreen_or_collapsed_window_is_not_remembered() {
        assert!(
            geometry_worth_remembering(false, None, None),
            "the ordinary case"
        );
        assert!(geometry_worth_remembering(false, Some(false), Some(false)));
        assert!(
            !geometry_worth_remembering(false, Some(true), None),
            "maximised"
        );
        assert!(
            !geometry_worth_remembering(false, None, Some(true)),
            "fullscreen"
        );
        assert!(!geometry_worth_remembering(true, None, None), "collapsed");
    }
}

#[cfg(test)]
mod lock_guard_tests {
    use super::*;

    fn param(name: &str) -> ParamSnapshot {
        ParamSnapshot {
            id: 1,
            name: name.to_owned(),
            module: String::new(),
            min: 0.0,
            max: 1.0,
            default: 0.0,
            value: 0.0,
            text: String::new(),
            is_stepped: false,
            is_hidden: false,
            is_read_only: false,
            is_bypass: false,
            is_modulatable: true,
        }
    }

    #[test]
    fn an_ordinary_parameter_can_be_sequenced() {
        assert!(sequenceable(&param("Cutoff")));
    }

    #[test]
    fn a_read_only_parameter_cannot_be_sequenced() {
        // It cannot be set at all, so a lock on it is an event the plugin will ignore and a mark on
        // screen saying something is happening that is not.
        let mut param = param("Voices");
        param.is_read_only = true;
        assert!(!sequenceable(&param));
    }

    #[test]
    fn bypass_cannot_be_sequenced() {
        // The host's switch, not part of the sound.
        let mut param = param("Bypass");
        param.is_bypass = true;
        assert!(!sequenceable(&param));
    }

    #[test]
    fn a_parameter_that_does_not_accept_modulation_cannot_be_sequenced() {
        // A step's deviation is sent as `CLAP_EVENT_PARAM_MOD`. A plugin that has not advertised
        // modulation for a parameter has said it will not take one, and the player hosts plugins it
        // has never met.
        let mut param = param("Something else's knob");
        param.is_modulatable = false;
        assert!(!sequenceable(&param));
    }

    #[test]
    fn a_hidden_parameter_is_not_guarded_here() {
        // Hidden means "do not draw it", not "do not set it" - and something that is not drawn
        // cannot be turned, so there is no gesture to refuse. Pinned so that adding it to the guard
        // is a decision somebody makes on purpose rather than by pattern-matching the flag list.
        let mut param = param("Internal");
        param.is_hidden = true;
        assert!(sequenceable(&param));
    }
}
