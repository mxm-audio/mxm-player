# NOTES.md — apps/mxm-player

The detail behind this folder's AGENTS.md: history, measurements, rationale and worked examples.
AGENTS.md is the contract; this file is the reference it links to.

References kept from the monorepo: `plans/…`, `docs/known-issues.md` and
`scripts/capture_player.ps1` are files of the former `mxm-collection` monorepo, kept in the private
archive and not in this repository. `plugins/AGENTS.md` and `plugins/<plugin>/host-tests` are paths
inside each product's own repository. `docs/briefs/mxm-player.md` is in this repository's root
`docs/`.

## Ownership in full

**Its own product.** Own version, MSRV, `README.md` and `LICENSE`, released independently of any
plugin's cycle. It shares the design system and `crates/ui` without being coupled to a plugin
release.

Owns `src/`, `tests/`, `Cargo.toml`, `README.md`, `LICENSE`. Its tests drive it through
[`apps/mxm-player-harness`](../mxm-player-harness/AGENTS.md) (`app_harness`, `harness`), which every
plugin's host tests share. **A plugin's tests through the player belong to that plugin**, in
`plugins/<plugin>/host-tests` (`cargo test -p <plugin>-host-tests`), with their fixtures; this
player's own tests use mxm-mono-01 only as the reference instrument and the fixture plugins for
hostile cases. Its design brief is `docs/briefs/mxm-player.md`. Current work that
touches the player is owned by its plan or handover in `plans/` (`plans/AGENTS.md` in the private archive); durable
behavior stays here.

Does not own the fixtures it loads — see [`tests/clap-fixtures/AGENTS.md`](../../tests/clap-fixtures/AGENTS.md).

## Scope, the envelope and the effect chain

### Scope, deliberately

In v1 there is **no audio input from the device**, **no embedded plugin GUI**, and **one source**
followed by a **serial chain of effects**. That chain is
the only routing the player has: source → effects → device, in the order the strips show. No bus,
no send, no parallel path.

**"No embedded" is now precise rather than absolute.** The player shows a plugin's own interface in
a **floating window the plugin owns** — `src/engine/editor.rs`. Embedding, where the plugin's window
lives inside ours, stays unsupported: it needs a container window in three platform implementations
and it is impossible on Wayland, which has no cross-process embedding primitive at all.
These are not gaps to be filled opportunistically; each is a decision recorded in `README.md` with
its reasoning. Do not add a disabled control implying otherwise.

### The compatibility envelope is enforceable, and refusals carry reasons

`src/envelope.rs` defines the narrow v1 CLAP envelope. Everything outside it is refused **with the
reason shown**, which is what keeps third-party readiness honest without third-party plugins to test
against. A refusal without a reason is a bug.

### The effect chain: serial, reorderable, and off means uncalled

The player hosts effects after the source in **one serial chain**; the chain can be **reordered**;
the player shows **no
effect parameters** — an effect's controls are its own editor's business — only a way to rearrange,
a way to open each effect's editor, and an **on/off that bypasses the effect and stops its CPU
use**. *"Generally when an effect or a synth is doing nothing it should not use any CPU."*

- **An effect is a plugin with exactly one audio input, judged by its ports.** `src/envelope.rs`
  has a second negotiation, `negotiate_effect`, beside the source's: one main input and one main
  output, mono or stereo each, chosen through `audio-ports-config` first-compatible as the source's
  is, and refused with the reason otherwise. Discovery classifies every plugin for **both** slots
  (`Found.support` and `Found.effect`), on two instances, because `audio-ports-config::select` is a
  choice made on an instance. A plugin advertising both shapes qualifies for both; each picker
  shows its refusals with their reasons, as the plugin picker always has.
- **The chain is the engine's, and a topology change goes through stop-and-return.**
  `src/engine/fx.rs`: an `FxSlot` on the GUI thread (entry, instance, envelope, switch), an
  `FxStage` on the audio thread (its processor in a `ProcessorOwner`, its own buffers sized at
  activation, its tail bookkeeping), and the `FxChain` the worker runs after the source. Add,
  remove and move stop the worker, edit the slots and restart — exactly as `load` does for the
  source; the source, its locks and its patch are untouched. At most `MAX_FX` (8). Effects are
  activated after the source and deactivated before it, on every path, a failed start included.
- **Off is not called — but it is still reached.** The switch is one atomic the stage reads per
  chunk; off, the stage is skipped and its input passes untouched — to the bit, `t11_fx_chain.rs` says. No restart: it is a
  flag, not a topology change. **Back on, the stage is reset** (CLAP `reset`) before it runs
  again, so nothing it held from before the gap comes out.

  A switched-off effect still **takes its editor's changes and host automation, including final
  modulation zeroes**, through CLAP's
  `params.flush` (`FxStage::flush_params`), and `FxChain::wants_processing` counts bypassed
  effects so the graph stays awake long enough to reach one. nice-plug applies an editor's edit
  only when the plugin is processed or flushed; without the flush a switched-off effect's knobs
  followed the pointer and sprang back on release, because nothing ever applied the value (the
  owner, 2026-09-04). A flush is not a way back into the signal — the audio stays untouched.
  **With no stream there is no stage**, which is how a restored chain starts: no source loaded.
  `FxSlot::flush_params_while_inactive` then flushes on the main thread, as CLAP allows for an
  inactive plugin, from `Engine::service_host_requests` each frame; an active effect is left to
  its stage. Without it the effect's editor was dead until an instrument was loaded, keyboard
  and pointer alike (the owner, 2026-09-30).
  `engine::tests::an_effect_with_nothing_running_takes_its_editors_edits` holds it. A panic resets every stage the same
  way — an effect has no note port for All Sound Off to reach, and a tail outliving a panic would
  not be one.
- **Sleeping is per stage, and the tail follows the input.** A stage runs when its input carries a
  non-zero sample, when host events are pending, when the plugin asked to be processed, or while it is still awake from before;
  it sleeps on `Sleep`, on a finite tail counted down to zero, or on eight quiet buffers under
  `ContinueIfNotQuiet` — the source's rules. **The tail countdown restarts on every chunk that
  carries input.** Counted from the first `Tail` status it put a delay to sleep mid-note; the
  bit-exact comparison found it. The whole graph sleeps only when the source is asleep **and no
  stage is open**: with the source asleep and a tail still running, the worker feeds the chain
  zeros and keeps calling it until it stops, then calls nothing.
- **Channels adapt at every boundary, one way.** Stereo into mono is the sum at half, mono into
  stereo is duplicated — at each stage's input and once more at the device, which is opened for
  the widest output in the graph. A stage whose plugin errors bypasses itself and stays bypassed.
- **One editor at a time, source or effect.** `EditorTarget` names whose window is open; opening
  another closes it first, and the target follows its effect through a move or a removal.
  *Superseded by "As many editors at once as there are plugins" under [Plugin GUIs](#plugin-guis-floating-and-three-invariants).*
- **The rail** (`src/ui/fx_rail.rs`) is the whole interface: one compact vertical strip per
  **effect** in the **free region right of the sequencer's rows** — the owner's placement — in
  signal order: a coloured header carrying the effect's number, the name read bottom-to-top, and
  at the foot `On` / `Edit` and `<` / `>` / `×`, then a `+` strip whose menu is the effect picker.
  **The source has no strip** (the owner, 2026-09-04): its name is in the status bar's picker and
  its editor behind that bar's Show/Hide editor button, and a strip repeating both would be a
  second control for one act. The sequencer panel measures its rows and hands the
  rail what they leave — **the rows grouped in a `vertical` of their own and that group's rect
  read back**, because a panel's own `min_rect` is forced to the panel's full width and reports
  no room at all — so the rail never moves a row and is on screen with the parameter panel
  collapsed — which is how the player sits beside an editor, and where a rail in the collapsible
  band vanished. A long chain scrolls sideways. The name is painted, not laid out, because egui
  lays text out horizontally; the strips are laid out with `with_layout`, not `horizontal`,
  because a horizontal row starts one button tall and staggers tall children; the acts are
  collected while drawing and applied after it.
- **The verbs**: `fx add <plugin>`, `fx remove <n>`, `fx move <from> <to>`, `fx on <n>`,
  `fx off <n>` and `fx editor <n>`, one-based as the strips are numbered; the dump carries
  `state.fx`. Two developer diagnostics, `fx dumpstate <n>` and `fx loadstate <n>`, round-trip the
  named live effect's opaque CLAP state through `effect-<n>.clapstate` beside player settings. They
  are the generic machine seam for durable effect model data that is deliberately not represented
  as automatable parameters; the player never decodes or specialises that state. The chain persists
  as `fx_chain` in the settings and is restored at startup **before
  any source loads** — an effect needs none, and the source's own load then starts audio once,
  with the chain in it. An effect that cannot be restored is reported and skipped.
- **An export goes through the chain**: every effect that is on, with its own state captured as
  the source's is (`offline::through_effects`), at the source's block size. One that is off is
  left out, as the live path leaves it uncalled. The render is not lengthened for a tail — an
  empty bar at the end is how one is asked for, as before.
- **Proof is against a reference effect that shares no code with the collection.**
  `tests/clap-fixtures`'s `dk.mxm.fixture.effect` and its mono twin are arithmetic a test can
  repeat, so `tests/t11_fx_chain.rs` computes the chain's output from the dry render and compares
  to the bit — what reaches the effect and what comes back — for one effect, two in series, a mono
  effect in a stereo chain, off, back on after a gap, a panic, the export, and a session run twice.

### Effect automation identities survive chain edits

Effects receive their own parameter-event buffers and expose parameter snapshots on demand.
`sequencer::locks::LockKey` therefore identifies both the plugin and parameter; a nice-plug
`param_id` is only unique within one plugin.

- Every effect receives a stable, never-reused `engine::fx::FxId`; chain position is not identity.
- Removing an effect deletes its locks before publishing the changed chain. Reorder and bypass keep
  them.
- Source locks retain the compatible bare parameter form. An authored effect key is
  `fx<n>:<parameter>`, with the visible position resolved to an `FxId` when the command is handled.
- Locks loaded before their effect exists remain pending and resolve when a matching effect appears.
  Clear, replacement, scoped clear and shrink operations remove corresponding pending cells.
- A missing live effect reference is a save error, never silently reinterpreted as Source.
- Source switching parks Source locks only; effect identities survive it.
- Source and effect authoring share `write_step_lock`, including finite/range/stepped/capability
  validation and baseline handling. Final modulation zeroes remain owed until actually consumed.

`tests/t13_effect_locks.rs` proves model properties including an intentional hash collision;
`review_player_regressions.rs` crosses app/plugin boundaries. Tests of delivery must observe plugin
readback or audio, not merely successful insertion into a stage buffer.

## Threads, realtime and the engine

### Threading

The rules that shape this codebase. Breaking one produces a deadlock or a torn frame, not a
compile error.

- **Nothing on a plugin thread or the audio thread ever calls into egui.** `request_repaint()`
  reaches `Context::write()`, which takes an `RwLock`. Those threads store atomics; the dedicated
  notifier thread (`src/notifier.rs`) polls them and wakes the GUI on their behalf.
- **`rtrb` is SPSC**, so every producer owns its own queue. The audio thread drains and merges them
  by arrival time.
- **Every event is stamped inside its own callback**, before any queueing, from the one monotonic
  clock in `src/clock.rs`. That keeps one comparable timeline across all sources.
- **`midir::send()` is I/O** and never happens on the audio thread. It goes through a queue to a
  worker thread, which is also where panic recovery lives because it is allowed to block.
- **Ordinary reconfiguration polls once per frame.** The processor returns through a capacity-one
  lock-free queue and an atomic flag. `stop_now` is synchronous: it polls/yields on the caller's
  thread for up to three seconds, including queue admission, before scanning or changing topology.

### The audio callback allocates nothing

`src/engine/processor.rs` sizes every buffer at activation and uses only lock-free queues. Two
traps, both of which have already been walked into once:

- Clack's `EventBuffer::push()` *can* grow its backing `Vec`, so the output sink in
  `src/events/output.rs` has **fixed capacity and fails instead of growing**, incrementing a
  visible overflow counter.
- **Rust's stable sort allocates.** `sort_by_key` / `sort_by` reach for scratch space once a slice
  is more than a handful of elements, so a merge that is allocation-free under six events allocates
  under four hundred. `MergedInput::sort_by_arrival` therefore uses `sort_unstable_by_key` with an
  explicit `sequence` tie-break, which is stable in effect and allocates nothing. Do not "simplify"
  it back to a stable sort.

Because the failure only appears past a threshold, the allocation tests must include a **dense**
buffer, not only a sparse one — see `tests/verification.rs`. The wrapper-overflow regressions in
`tests/plugin_robustness.rs` directly load `target/debug` binaries, where nice-plug's
`assert_process_allocs` is compiled in: mxm-mono-01 first sounds its target in one callback, then
puts 2,000 ordinary events and that target's explicit release in the next; mxm-para-07 independently
ends the saturated callback with a zero-velocity NoteOn carrying the prior callback's note id, which
its event contract interprets as NoteOff. The dedicated nice-plug output fixture floods
`ProcessContext::send_event` and terminates its separately admitted note. Thus silence/output proves
termination rather than eviction of a same-queue note-on. The vendored
unit test separately proves a million reported inputs select only two capacity-sized windows. They
fail if either artifact is absent rather than silently accepting an ordinary unguarded release
build.

### One clock, and it is read on the audio thread

[`Clock`](src/clock.rs) is a **closed enum**, not a trait object, and deliberately so. It is read
inside `AudioWorker::callback`, on the GUI thread, and inside MIDI backend callbacks. Every read
must be **nonblocking, allocation-free, lock-free and I/O-free** — blocking in a MIDI callback
stalls the backend thread, allocating in the audio callback breaks the rule this project treats as
inviolable. A `dyn Fn() -> u64` would let a caller install a clock that violates that and nothing
would catch it until a dropout in somebody's DAW. Two arms, both obviously wait-free, make the
contract hold by construction.

`tests/verification.rs::the_audio_callback_never_allocates` is what keeps it honest.

### The app is configured, never hard-wired

`PlayerApp::with_config` takes a [`PlayerConfig`](src/config.rs) carrying the backend, settings
path, sentinel path, plugin search paths, clock and MIDI enumeration. `PlayerConfig::production()`
builds what the shipped binary uses, so **production stays the ordinary path** rather than becoming
a special case of a test configuration.

Two rules that are not negotiable:

- **A test must never run the production configuration.** It would rewrite the settings of whoever
  is using the player on the same machine. `PlayerConfig::sandboxed` exists for this.
- **A sandboxed config searches an explicit, possibly empty, path list** — never the standard
  locations. Otherwise a test discovers whatever happens to be installed on the machine running it
  and passes or fails accordingly.

### Servicing is one implementation, called from two places

`PlayerApp::service` is everything the app does each turn that is not drawing: engine poll, plugin
output, log drain, idle process requests, latency refresh. The window calls it from
`eframe::App::logic`; a headless session calls it directly.

**Do not grow a second copy for headless use.** Two would drift until a session test passed on a
path no user takes, which is worse than no test at all. `Engine::process_request_pending` is an
observational diagnostic only: unlike `take_idle_process_request`, it never consumes the callback,
so a real-bundle test can prove the callback arrived before the ordinary audio path services it.

### Audio advancement and command servicing are separate

In the deterministic session backend ([`src/engine/session_backend.rs`](src/engine/session_backend.rs)):

- **Advancement is script-driven** — blocks render only when the session grants them, which is what
  makes captured audio byte-identical.
- **Command servicing is autonomous** — the stream thread calls `AudioWorker::service_commands`
  whenever it is not rendering, which renders nothing.

`Engine::stop_now` polls until a callback consumes `Command::Stop`, and both `load` and `rescan` call
it. A backend that services commands only during requested renders deadlocks and falsely reports a
plugin wedge after `WEDGE_TIMEOUT`.

`service_commands` deliberately does **not** drain the input queues: `render` is what converts
merged events, so draining without rendering would discard them silently.

### A wedged engine never reports a clean stop

`enter_wedged` takes the stream, so any "nothing to stop" shortcut keyed on `stream.is_none()` will
report success for an engine that is terminally broken. `Engine::stop_now` checks the wedged state
explicitly, both on entry and after its polling loop — the loop wedges on its own via
`check_processor_return`, so falling through to `Ok` afterwards would hide the one outcome the
method exists to detect.

This matters beyond tidiness: callers use that return to decide it is safe to run plugin code with
audio stopped, which is what makes the scan sentinel mean anything.

**Queue admission is part of stopping.** `AwaitingStoppedProcessor` begins with the request,
not successful enqueue. A refused Stop is retried on every poll; `stop_queued` prevents duplicates,
and repeated requests cannot reset the deadline. Never return `Ok` merely because Stop did not fit.
A timeout reports an unreturned processor, not proof that a plugin callback hung.

**Effect host requests are serviced too**, even without an editor. `Engine::poll` invokes each
requested `on_main_thread` once per turn. Restart/audio-port requests stop the whole chain,
renegotiate only after deactivation, and ask the app to start again with fresh publications.
`dk.mxm.fixture.effect-main-thread` checks actual callback delivery; engine tests exercise the
restart and port-notification flags separately.

### A dead stream is not a wedged plugin

`src/engine/stream.rs` keeps these separate. The ordinary protocol assumes a *future* callback will
observe `Command::Stop`; after a fatal WASAPI error no further callback runs, so that assumption
fails and a different path must return the processor. Do not collapse the two cases.

**A dead stream is not a stopped one either, and the engine comes back from it by itself.** The
platform takes a device away — on Windows, another application opening it in exclusive mode, its
sample rate or clock changing, the cable coming out — and WASAPI answers the next
`GetCurrentPadding` with `AUDCLNT_E_DEVICE_INVALIDATED`. None of that is the plugin's doing, the
processor came back cleanly, and a restart is all it takes. Three things follow, each found the day
the status bar read `OS Error -2004287484 (FormatMessageW() returned error 317)` on a real
interface (`docs/known-issues.md`, *Windows takes the audio device away*):

- **The reason is described, not dumped.** `audio::describe` turns CPAL's classification —
  `DeviceNotAvailable`, `DeviceBusy`, `StreamInvalidated`, `HostUnavailable`, `PermissionDenied` —
  into a sentence a person can act on and keeps the backend's own text in parentheses, because a
  bug report needs it. Every CPAL error the backend reports goes through it. A kind CPAL could not
  classify passes through unchanged rather than being guessed at.
- **The engine schedules the reconnect and the app drives it.** Whoever owns the app owns the
  backend, so `Engine::try_reconnect` is asked once per frame from `service` — one implementation,
  called from two places — and starts the stream again when an attempt is due: after
  `RECONNECT_FIRST_DELAY`, then doubling up to `RECONNECT_MAX_DELAY`, and never giving up while the
  stream is dead, so an unplugged interface comes back when it is plugged in. A refused attempt
  leaves `StreamExited(<why it died>)` on show and carries the refusal in `Reconnect::last_failure`;
  the status bar and the dump's `audio.reconnect` show both. `service` reacts to the death itself
  exactly once, through `take_stream_exit_notice`: it stops the transport and closes control
  gestures as a rescan does — and the engine resets the published playhead at the death, because
  the worker that owned it is gone and its last word would stand as "playing" over silence, in the
  step row and in the dump, until a new worker overwrote it. A fresh worker knows nothing, so after
  a reconnect the CC mask and the sequencer state are published again, as after a load.
- **Play refuses while the stream is dead or the plugin is wedged**, with the reason —
  `try_play_from_start` returns it to the CLI, and the button and the spacebar go through
  `play_from_start`, which puts it in the status line. Before this, `play` answered "playing from
  step 1" over silence. It is deliberately *not* refused with no plugin loaded: the transport
  label's comment explains why that case must keep toggling.

**And a start that fails after activation must deactivate.** A device held in exclusive mode
refuses `Initialize`, so `build` fails *after* the plugin was activated; the worker is dropped
inside the refused build and the owner's `Drop` hands the processor back. `Engine::start` takes it
from the return queue and deactivates, or the instance stays active and clack refuses every later
start with "Plugin was already activated" — one busy device would have turned into a plugin that
never plays again, and the reconnect would have hit it on its second attempt.
`p1_engine.rs::a_start_refused_by_the_device_leaves_the_plugin_ready_to_start_again` went red on
exactly that message before the arm existed.

Guards: `p1_engine.rs::a_dead_stream_is_reconnected_by_the_engine_once_the_device_is_back` and
`t9_cli.rs::play_is_refused_while_the_audio_device_is_stopped_and_returns_when_it_reconnects`, both
driven by the fake backend's `kill` and `refuse_build` switches. The real backend cannot be driven
from a test; the known-issues entry records how it was exercised by hand.

### Discovery executes arbitrary code

`src/discovery.rs` caches on path, size and mtime — but a cache cannot quarantine something that
crashes the process mid-scan. Before entering an uncached bundle the scanner writes a **sentinel**
naming it and clears it on success. Preserve that ordering.

### Settings are written eagerly and replaced atomically

In-process hosting can die without warning, so device, ports, last plugin and the quarantine list
are written **immediately on change**. Eager writing is what demands atomic replacement: overwriting
in place could leave a truncated file, which is the exact failure eager persistence exists to avoid.

### Fault isolation is partial, and says so

In-process hosting **cannot** be made crash-safe. A native access violation in plugin code
terminates the player; a blocking plugin hangs whichever thread it blocks. The timeout recovers
**one** case — an uncompleted Stop/processor-return handshake, whether the backend stopped
calling or a plugin call did not return. A plugin may
equally hang in `instantiate`, `activate`, state save/load, a parameter query, or `on_main_thread`,
all on the GUI thread, and no in-process timeout recovers those. Keep this stated plainly; do not
let the docs drift toward implying safety.

## Parameters and the panel

### Parameter edits are asynchronous, and the panel must not forget that

An edit reaches the plugin **through the audio thread**, so for a frame or so afterwards the
panel's snapshot still holds the pre-edit value. Two rules follow, and breaking either one makes
every control snap back to where it started the moment it is released:

- On change, write the new value into the snapshot as well as sending it. The snapshot is what the
  control falls back to once the drag ends.
- Requery the plugin on the **next** frame, never the same one. Requerying immediately reads back
  the old value, which is the very bug this avoids.

`tests/p3_testable.rs::an_edit_survives_the_round_trip_through_the_audio_thread` is the guard.

### The parameter panel tabs by the plugin's own groups

The panel wraps controls by height and derives tabs from CLAP’s parameter `module` path. nice-plug
fills that path from `#[nested(group = …)]`; `ParamSnapshot::module` and serializable
`ParamState::module` preserve it so tests can assert groups without relying on screen position.

The rules, in the order they matter:

- **A plugin that declares no groups keeps exactly the panel it had.** One group means no tab strip
  at all — a strip over a single tab is chrome that says nothing. That is every effect and every
  other instrument, and it is the compatibility statement the change lives or dies by;
  `parameter_groups_come_from_the_plugin_and_an_ungrouped_one_is_unchanged` holds it.
- **The ungrouped parameters are the instrument, so they are the tab that opens.** They are what a
  player wants first, and they are what makes *Cutoff* reachable by its label without clicking
  anything. Their tab is called **Main**; every other tab is named by the plugin.
- **Tabs filter what is painted and nothing else.** A host automation lane, a preset and a
  sequencer step lock reach every parameter whatever tab is showing — the same rule as `is_hidden`,
  and the same rule the plugin editors follow. The selected tab is transient view state and is
  never persisted.
- **A step's locks live on parameters, not on views**, so switching tabs cannot disturb one. A lock
  behind an unopened tab would be invisible, which is a real hazard in a sequencer, so each tab
  carries a count of the selected step's locks inside it.
- Groups are derived from the snapshot every frame rather than cached: a plugin can change its
  parameter set, and a stale strip would be worse than none. A remembered tab that no longer exists
  falls back to the first.

Routing groups are target-local. `mxm-mono-01` has four routing groups with at most 22 parameters;
`mxm-mono-08` has fourteen — ten *Modulation* and four *Triggers* — with at most 32. Hiding absent routes is optional readability work, not a
usability prerequisite. `the_drag_oracle_would_have_caught_the_snap_back_defect` falsifies the edit
round trip by cutting the parameter write path.

## Keyboards and MIDI input

### The keyboard is sized in keys, not in fractions of the window

White keys have a **fixed** width. A wider window reveals more of the keyboard; it does not
stretch the same keys, which is what a longer keyboard means on any other instrument. Near the top
of MIDI's range the run slides *down* rather than ending early, so a wide window is never left with
a gap on the right.

Two details that look cosmetic and are not:

- **Black keys are drawn after the white keys and hit-tested before them.** They overlap the two
  whites they sit between, so the pointer belongs to whichever is on top.
- **White keys need an outline.** They are nearly the colour of the panel behind them; without the
  border the keyboard reads as one pale block.

Only the Cs are labelled — a label on every key is noise, and the Cs are what a player counts from.
`C3` is middle C at MIDI 60, matching the octave the shift keys report.

### The on-screen keyboard monitors every input path, and `held` is only one of them

`held` contains notes originated by the GUI; hardware MIDI goes directly from its callback to the
audio thread. External notes are therefore published separately by the MIDI callback into
`PlayerHostState::set_midi_sounding` — a
pair of atomics and one `fetch_or`/`fetch_and` per note, because that callback is wait-free and
I/O-free by the same contract as the clock. It wakes the GUI only when the set changes, exactly as
`set_playhead` does. `PlayerApp::sounding` is the one place that answers "what is sounding, from
every source"; ask it rather than `held`.

**A port closing clears the set.** A note held across a disconnect has no release coming, and
would otherwise stay lit for ever. `tests/t2_regressions.rs::a_note_played_on_a_midi_keyboard_lights_the_key_it_sounds`
covers both directions and the disconnect.

### A refused MIDI input carries the reason it was refused

`connect_desired_midi_inputs` can fail because a port is held elsewhere, disappeared after being
saved, or exceeds the input limit. `RefusedInput` carries the port and the actual reason together;
never replace backend failures with a generic limit message. `tests/t2_regressions.rs::a_midi_port_that_cannot_be_opened_says_why_rather_than_blaming_the_limit`
asserts both halves: the reason names the port, and it does not blame the limit.

### Whatever the note keys do, a MIDI keyboard does too

`note_on` is the GUI path: it sounds the note and, while a step is selected, writes into it. Hardware
MIDI is stamped in its callback and queued directly to audio, so every editing behavior added to
`note_on` needs an explicit MIDI counterpart.

`PlayerApp::receive_midi_presses` is the MIDI half, run once a turn from `service`. It does the
**editing** only: the audio thread already has the note, so sounding it again would double it.

Presses are latched, not sampled. `midi_sounding` is state — what is down *now* — and a key struck
and released between two GUI turns leaves nothing in it. `midi_pressed` is the event: set on every
note-on, drained with a `swap`, so each press is delivered exactly once however short it was. When
adding something to `note_on`, ask which of the two it needs.

### Restoring a setting means restoring it into the **engine**, not only into the panel

`with_config` turns saved settings into running engine state. Any persisted value acted on by the
engine must be applied there, not merely restored into the panel model; change-only UI handlers will
not send an already displayed value. `tests/t2_regressions.rs::a_saved_midi_selection_is_restored_
into_the_engine_not_only_into_the_ticks` guards the seam.

A note for diagnosing the next one: Windows MIDI inputs are exclusive, so opening the port from a
second process while the player claims it is a decisive test of whether the player really has it.

### The computer keyboard is not an instrument while you are typing

**Nothing on it plays while a text field has focus** — not the note keys, not the octave keys, not
the transport. `handle_input` runs in `logic()`, *before* any widget draws, so it sees every key
event whether or not a field is about to consume it. Without the guard, naming a sequence "bass"
played four notes.

Anything already sounding when focus moves into a field is released, because its key-up will be
swallowed by that same guard and the note would stay on.

**Space is the transport**: pause while playing, play from the beginning otherwise. It always
rewinds, so every start is from step 1.

`space_does_not_start_the_sequencer_while_a_name_is_being_typed` and
`a_note_held_when_a_field_takes_focus_is_released` are falsified guards. `egui_kittest` itself
withholds note-key events once text has focus, so the broader “typing does not sound a note” path
still requires a running-player check; do not claim that behavior from the harness.

## The interface

### A status reading appears when it has something to say

The bar always shows one `load` figure for the whole callback. It shows `late` and `xruns` only above
zero, `priority` only when promotion is not confirmed, and `latency` only when nonzero. Every meter
field remains available through CLI `dump` (`state::Meters`); diagnostics belong there rather than
occupying a healthy musician surface.

### The plugin picker is a menu of what can be loaded, and it lives in the status bar

**In the status bar, not the settings panel.** The settings panel collapses — that is what makes room
for a plugin's own interface beside the player — and a plugin chooser that disappears with it cannot
be used in the arrangement the player is meant for. The picker sits beside *Show editor* so loading a
plugin never requires expanding anything.

**It prioritizes what can be loaded.** Loadable plugins lead the menu. Entries outside the v1
envelope remain below a separator, disabled, with their refusal reason and location on hover; a
refusal without a visible reason is a bug.

**A loadable entry's hover says what the plugin is** (the owner, 2026-09-27): its descriptor's own
description, then its vendor — `Found::hover`, read from the CLAP descriptor at discovery
(`offline::describe`), in both the instrument picker and the effect rail's. A plugin that describes
nothing shows its vendor alone; the vendor alone told a player nothing about which to choose.

**Location is shown for same-ID duplicates, and only for them.** An installed copy alongside the
build output is routine, the two entries are otherwise identical, and one is usually stale — loading
the wrong one means testing a build that no longer exists. Showing the path on every row was noise;
showing it on the rows where it is the only distinguishing information is the point. **Quietly**,
either way: this is a routine situation, not a warning, and colouring it as one trains the user to
ignore the colour that does mean something.

Discovery canonicalises paths, which collapses the symlink case the install instructions produce
and *only* that. Two genuine copies stay two entries, because they really are two plugins.

### Sibling `ScrollArea`s need distinct IDs

Every `ScrollArea` sharing one `Ui` must carry its own `id_salt`; otherwise siblings share an offset
and undo each other’s scrolling. If a second scroll area is added beside `log_view`, restore a
painter-level scrolling regression: AccessKit rectangles remain in content coordinates and cannot
observe the offset.

### The browser column fits the panel it is given

Its content has a floor — a 280px list, a 120px log and the device sections — that can exceed a
short panel. The whole column scrolls so its edge never enters the keyboard’s region.
`tests/t2_regressions.rs::the_browser_edge_never_reaches_into_the_keyboard`
asserts **one frame per height**: a drag-resize is a sequence of heights, one frame each, and
settling for several frames afterwards hides the overflow.

### The player has a style, and it comes from `mxm-ui`

`mxm_ui::theme` and `mxm_ui::typography` are applied in `PlayerApp::new` — before the first frame,
so nothing is drawn unstyled — and again every frame in `ui`, so a host theme change is followed
rather than sampled once. The per-frame call is always one frame behind, because a `Ui` holds a
clone of the style it was built with; that is why `mxm-ui` resolves its named text styles
defensively and why the startup call exists.

egui’s defaults leave an inactive widget with no visible stroke, so the shared theme makes resting
controls legible before hover.

- **A resting control has a one-pixel border.** Hover *strengthens* it; hover is a change of degree,
  never the thing that makes a button look like a button.
- **Both themes are written out in full.** Neither is derived from the other by inverting or
  lightening, because a derived palette drifts out of contrast in one of them. `apply` sets both, so
  a host switching at runtime is followed.
- **Contrast is measured, not asserted.** `ui::theme`'s tests compute WCAG relative luminance and
  check body text at 4.5:1 and borders against their surfaces. The design system says these meet
  AA; the tests are what make that true rather than claimed.

**Only §5.1 is applied.** Modulation colours, typography and layout are not, and this file does not
claim the design system is implemented — it makes controls legible.

### Styling comes from `mxm-ui`, and `src/ui/adapter.rs` is the only translation point

The interim compromise ended at M4a. `adapter.rs` no longer holds unstyled widgets; it converts
between CLAP's `ParamSnapshot` in real units and `mxm-ui`'s normalised controls, and that is all it
should ever do. **Do not style outside the adapter** — a colour or a size chosen in a panel is a
token that has escaped `crates/ui`.

The retrofit found three defects that a single-consumer design would have shipped, which is the
argument for having done it before mxm-mono-01's editor was built on the same API:

- **Scroll-wheel editing applied on hover alone.** §7.1 permits the wheel only *"with focus or a
  modifier"*, so scrolling a long parameter list could silently automate whatever passed under the
  pointer. Now `mxm_ui::Wheel::FocusOrModifier`.
- **Parameter sliders had no accessible name.** A screen reader announced bare "slider", and a UI
  test could only find one by position. Now `labelled_by` the visible label.
- **Controls sat below §11's 32 px pointer minimum.** `PARAM_ROW_HEIGHT` already budgeted 46 px —
  a label line plus a 32 px control — so the panel's wrap arithmetic did not change.

### The theme is the player's own choice, and the desktop is only the default

Design system §10 asks every interface for Dark, Light and System. The player had none: egui's
default `ThemePreference::System` resolved against whatever the desktop was set to, and there was
no way — from the interface, from a script, from anywhere — to ask it for the other one.

- **`mxm_ui::shell::theme_control`**, in the app bar, to the left of the two collapse toggles.
  Those two are muscle memory at the far right; a rarely-used control must not push them along, so
  it is drawn after them in that right-to-left row. The eight editors draw the same widget in the
  same slot — the player is a member of the collection here, not a special case — but they call
  `editor_theme_control`, which writes the collection's shared file. The player's choice belongs in
  the player's own settings.
- **`theme <light|dark|system>`** is its verb, as the contract requires. It is the one verb whose
  act lands a frame late: `cli_execute` has no `egui::Context` to set a theme on, so it writes
  `PlayerApp::theme` and `ui()` carries that into the context on the next frame — one comparison,
  so nothing is fighting `System` for the desktop's answer every frame.
- **Saved** in `settings.theme` as `light`, `dark` or `system`. Absent means follow the desktop:
  a settings file written before this existed has made no choice, and a missing choice must not
  silently become one.
- **`MXM_PLAYER_THEME` overrides the saved choice for one run and is never written back.** It is
  `MXM_EDITOR_THEME`'s opposite number for the host, and it exists for the same reason: a
  screenshot pair needs a known theme without disturbing what the person using the machine chose.
  `scripts/capture_player.ps1` opens the player under it and then drives `theme dark` over the CLI,
  so both halves of a pair come from one launch and one scene. Before this landed, the only route
  to a dark host was editing the developer's Windows theme — a change to their machine to
  photograph ours.

The on-screen keyboard does not follow the theme, and that is deliberate: see
"The keyboard is sized in keys, not in fractions of the window" — white keys stay white, because a
keyboard that inverted would stop being a keyboard.

### Play always rewinds, and there is one button for it

`play_pause` calls `play_from_start` whenever transport is not playing. The visible opposite is
Stop, which sets `Transport::Stopped` and rewinds; the spacebar calls the same function rather than
duplicating transport semantics.

The model still supports in-place resume: a same-generation transition to `Playing` resumes, while
a bumped generation restarts. Export and automatic `PlayerApp::pause` paths need the distinction
even though the visible transport offers only rewind-and-play and Stop.
`play_pause_resumes_from_paused_without_rewinding` protects the model.

### Nothing in the transport row changes size when you touch it

Every transport-row control is explicitly sized through `TRANSPORT_BUTTON_SIZE`,
`TEMPO_FIELD_SIZE` or `STEP_BUTTON_SIZE`, and the panel has fixed height. egui otherwise sizes to
content and shifts neighboring controls when labels or edit modes change.

A border that appears only on hover reads as movement. `toggle_button` uses
`Button::frame_when_inactive`, so rest already has a frame and hover changes only fill. Selection
also paints a 2 px accent border from `theme::tokens`, giving the second treatment design-system
§7.2 requires.

Assert row stability through a neighboring control rather than the edited control itself. `the_tempo_field_does_not_resize_when_it_is_
clicked` checks where *Random* is before and after the click; without the fix it moves 30 px. A test
that measured the control itself would pass while the row still jumped.

### A tie joins the buttons; the bar says the step holds notes

Step buttons are painted as pointer-target squares with a hairline border and a small filled bar
when the step holds notes; dot labels do not communicate duration or state.

**Duration is the button's extent, not the bar's.** A run of a note plus its **empty** ties is
drawn as **one wide button** — no divider, one outline, one shape — because a tie joins the steps,
so it is the control that joins. `step_row` allocates all sixteen cells first and paints
afterwards, since a run's bounds are not known until they have all been laid out.

**A slide does not split the button — the owner's ruling, reversing the plan's §7.** The tied note
reads as **one big step**: the run merges every tied step whether or not it carries notes, so the
runtime's "a slide starts a new run" is a fact about gates and locks, not about the drawing. What
says a slide is there is the **standard note bar on that step** — the bar already follows the
notes and only the notes, so a second pitch inside the run marks itself with no new mark invented.
`step_description` names a slide (*"Step 4: slide to G3"*) because the painted bar has no accessible
form. The bar follows notes only; duration belongs to the merged button extent. Therefore an empty
tie paints no marker, guarded by `a_tie_that_holds_no_note_paints_no_marker`.

**The bar is centred over the number**, which is the only thing under it. It was left-aligned while
its width was the duration and its left edge was where the note began; once the button's extent took
that job over, the alignment said nothing and only looked off-centre.

Within a run:

- **Playing and hover stay per step**, painted over the run's own fill. The run says how long the
  note is; these say which step you are looking at, and losing that to the merge would make a wide
  button unclickable in the dark. They extend half the item spacing into the gaps, or a sliver of
  the run's fill shows through between two cells of one button.
- **Selection outlines the step, not the run** — inside a wide button that is the point: it says
  which of the joined steps a note would land on.
- **The outline is drawn last**, after the per-step fills, or they paint over the thing that makes
  it one button.

**Open ends are the loop point.** A run carrying across it has nowhere to join on the row, so it is
drawn square rather than rounded — continuing off the end rather than stopping there. `tied(0)` opens
both the first run's left edge and the last run's right edge, because it is one run either side of
the repeat.

Painting by hand means the accessibility tree is not free. `step_row` still calls `widget_info` with
`step_description`, which **names the tie and the slide**: a shape has no accessible form, and
`tests/t8_editor_ui.rs` and `t6_sequencer.rs` find steps by their label.

**Every colour comes from `adapter::tokens_for`.** Painted controls and status labels must not bypass
theme tokens. In particular, `tokens.warning` is chosen and contrast-tested for the light panel;
raw yellow is unreadable there.

The piano keyboard's palette is the one deliberate exception, and it carries its own reasoning: its
white and black are the instrument rather than the theme, and its state colours are a real
change with a real risk, since the keyboard's regression tests assert on what is drawn.

### AccessKit labels are interface

UI tests find controls by their accessible label, so **renaming a control is a test-visible
change**. That is a feature, not a tax: a control with no accessible name is announced to a screen
reader as just "slider". Parameter controls are tied to their visible name with `labelled_by`.

### Collapsing, and the one rule about movement

Two panels collapse: the **parameter panel** and the **left settings panel**, independently, each
remembered. Three never do: the **status bar**, the **sequencer and transport**, and the
**keyboard**. There is deliberately no "collapse everything" — a single sweep taking the keyboard
with it is what `docs/briefs/mxm-player.md` has refused twice.

Collapsing shrinks the *window*, because leaving the space empty would be no room at all. Two
things make that safe rather than another instance of the interface moving on its own:

- **The layout changes when the user changes it, and at no other time.** Opening an editor never
  collapses anything.
- **`remember_geometry` persists the expanded size only, and never a maximised or fullscreen
  one.** Collapsing does not write it, so expanding returns to the size the user chose. Maximized
  and fullscreen are states, not reusable dimensions; `geometry_worth_remembering` guards the
  distinction. egui reports
  no monitor size to clamp a remembered size against; the first window is clamped by the viewport
  builder, and the remembered one is trusted because of what it refuses to record. The collapse
  *state* persists separately.

`ui::MIN_SIZE` is the expanded floor and `collapsed_height()` the collapsed one. The minimum is
**mode-dependent on purpose**: without that the window cannot shrink past the expanded floor and
collapsing yields empty space instead of room.

The two panels collapse independently, so requested height depends on both. Add
`SETTINGS_MIN_HEIGHT` whenever settings remains open, and react to either toggle.
`t8_editor_ui.rs::a_still_open_settings_panel_is_given_room_when_the_parameters_collapse` asserts the
requested height; AccessKit presence cannot detect clipping, and the harness does not apply viewport
commands.

`collapsed_height()` is the plain sum of panels that never collapse. `STATUS_HEIGHT_FLOOR` is only a
first-frame fallback, not an addition to a measured status height; do not add speculative clipping
margins.

## The sequencer

### A step is a rest, a note, a hold or a slide — and it stores one boolean

`Pattern` carries `tied: [bool; 16]` beside the notes. **Not a three-valued gate**, and the reason
is a rule about representation: a step is a rest when it holds no notes, and `steps[i].is_empty()`
already says so. Storing `Rest` as well would be a second copy of that fact, free to disagree with
it. What cannot be derived is whether the step ties backwards.

| `notes` | `tied` | The step is |
|---|---|---|
| empty | `false` | **Rest** |
| present | `false` | **Note** — half a step, unchanged |
| empty | `true` | **Hold** — the previous note continues through it |
| present | `true` | **Slide** — the gate stays open, the pitch moves. Starts a run of its own |

**All four rows are authorable.** A slide is the TB-303's slide button — a new pitch arriving while
the old note is still held, which `Runtime` plays as sound-then-release at one frame — and there
are two gestures for writing one, undone from either end: **play a pitch into a tied step** and it
lands there, or **click a step that holds a note**, which ties that step. The 303's slide flag
reaches forwards from step *n*; the tie reaches backwards from *n + 1* — the same boundary
described from opposite sides, which is why no second flag exists. A second boolean would be a
second copy of a stored fact, free to disagree with it.

#### Ties and notes are independent, and no edit rewrites a step you did not name

**The owner's ruling, 2026-09-02.** A step carries two facts — what notes it holds, and whether it
ties backwards — and **nothing couples them**. Every combination is authorable, in any order, at
any position. Tie a run first and drop notes into it afterwards; delete a note and the ties around
it stay exactly where they were.

**No authoring guard and no repair.** A tie that reaches no note is a valid unfinished run and plays
silence until a note appears inside it. `Runtime` sounds only note-bearing steps and uses ties only
to suppress release (`runtime.rs:721-753`). Every edit is local: clearing, deleting, tying and
pasting touch only named steps; no orphan-repair pass may rewrite surrounding ties.

**The file is true to the interface.** A `.seq.json` state uses all four rows above; a hand-authored
note-bearing tie is a slide and its locks are heard. Loading performs no repair because the
interface can author every stored state. Golden audio and state-combination coverage protect
existing authorable sequences.

If *mute this step but keep its notes* is ever wanted it is a **second** boolean; folding it back
into `tied` re-creates the contradiction this design removes. The saving is not the byte: with
three values, every path that wrote a note had to set the gate to match or write a note that never
sounded — nine of them. `tied` defaults to `false` and every write path is correct without knowing
it exists.

**A tie reaches backwards.** A tie on step *n* means the note from step *n* − 1 remains sounding
through step *n*. An ordinary note otherwise closes halfway through its own step.

Two clauses in `Runtime`, and **both** are needed:

| At | Condition | What happens |
|---|---|---|
| `GateClose` in step *n* | (`tied(n)` **and** *n* is empty) **or** `tied(n + 1)` | suppress — nothing is released |
| `StepStart(n)` | `!tied(n)` | release, then sound this step's notes — **which is a rest when it has none**, and is why `Rest` and `Note` were never two behaviours |
| `StepStart(n)` | `tied(n)`, no notes | nothing at all — a hold: the previous note continues |
| `StepStart(n)` | `tied(n)`, with notes | **sound, then release** — a slide, see below |

`tied(n + 1)` stops an ordinary note at the previous gate-close boundary; the
`tied(n)`-and-empty clause makes a **hold** last a whole step. If either is omitted,
`a_note_followed_by_k_empty_ties_lasts_exactly_k_plus_one_steps` reports the shortened duration. The emptiness test is what makes **a slide gate like a
note**: it started a new note at its own step, so it closes at the ordinary half step unless the
step after it is tied too — `a_slides_own_note_gates_like_an_ordinary_note` and
`a_slide_held_onward_by_a_tie_fills_its_steps` pin both halves, and
`a_slide_is_audible_as_a_slide_on_the_rendered_audio` hears them.

**The lookahead wraps, and that is right.** The pattern repeats, so a tie on step 1 holds step 16's
note across the loop point. Offline it must not: `Pattern::run_ends_at` truncates at the bar, and it
is the **only** place that difference lives. `Pattern::held_past_gate` is the suppression rule, wrap
included, shared by the runtime, the render and the MIDI writer — one rule, not three to keep in
agreement.

#### A tie with notes sounds before it releases, and the press table fights it

Legato means the plugin sees the new note arrive **while the old one is still held**.
Release-then-sound gives it two disjoint notes at the same sample, which is a retrigger with extra
steps. So `Runtime` emits `Sound` then `Release` at the same frame — the only thing that produces
that order, since an ordinary step start releases first.

`emit_from` in `engine/processor.rs` matches that pair and hands it to `emit_legato_joint`, which
**resolves the release before emitting the note-ons**. Without that, `PressTable::take_exact`
matches with `rposition` — the newest press — and a tie to a pitch already sounding would release
the press it had just made. Capturing early is not retiring early: `take_exact` reports without
removing, so a refused event can still be retried and nothing is treated as delivered that no plugin
received.

**There is deliberately no audio test for the same-pitch case, and that is a finding.** The obvious
one — "assert no stuck note" — passes with the fix removed, because nothing sticks: the following
step's release collects the leftover, so both orders emit two note-ons and two note-offs. Only
*which* press each note-off names differs, and with equal pitches that is inaudible. The guarantee
is asserted where it is real, in `a_release_resolves_to_the_newest_press_of_that_key`. An oracle that
cannot fail is not an oracle.

#### A pitch lands on the step itself, and `run_start` is symmetric with `run_ends_at`

`PlayerApp::toggle_step_note` writes the step it is given. **Every input path arrives there** —
the on-screen keys, the computer keyboard, the MIDI drain and the CLI's `toggle` all call it — so
the guards are stated once instead of four times, and
`a_midi_note_played_into_a_tied_step_lands_in_the_same_place` is what stops the paths drifting
apart. The note bar on a slide step marks its second pitch; do not redirect input to the run head.

**The keyboard shows the selected step's own notes.** `editing_step` answers the same question for
the display that `toggle_step_note` answers for the write — both are the raw selection, so they
cannot disagree. A hold marks nothing, honestly: a pitch played there lands there, as a slide. **A
highlight that does not match where the note lands is worse than no highlight**, because it looks
like an answer.

`Pattern::run_start` **stops at a note-carrying tie**, symmetric with `run_ends_at` — a slide is
the head of its own run from both directions, and
`a_slide_is_the_head_of_its_own_run_from_both_directions` holds the symmetry. It **stops at the first step rather than wrapping**: live, a tie there
continues across the loop point; for editing there is nothing further back to walk to, and a
wrapping search would not terminate on a pattern tied all the way round —
`walking_back_from_a_fully_tied_pattern_terminates` holds that. (`slide_head`, the *playback*
question, wraps once and is bounded, because a tie on step 1 legitimately reaches a head at the
pattern's end; the two walks differ deliberately and each says why.)

#### Clicking the selected step ties it to the note in front of it

Click selects, as it always did. **The second click ties the step you clicked**, taking over the
gesture that used to deselect; Escape deselects instead. The first click on any step still only
selects, so reaching for a step to type a note into it can never change a tie by accident.

**One rule, the same at every position in the row** (the owner's ruling, 2026-09-02): the click sets
`tied[n]` on the step under the pointer, and `tied[n]` has always meant *step n continues the note
before it*. The gesture and the flag now point the same way, so there is no position — the last step
included — where the click means something else. `tie_action` is the single place that decides:

| The step | The click |
|---|---|
| is tied, empty (a hold) | unties it — the hold becomes a rest |
| is tied, holds notes (a slide) | unties it — an ordinary note, notes and locks kept |
| is empty | ties it — continues whatever is in front of it, a **hold** |
| holds a note | ties it — **which makes it a slide** |

There is no refusal row, and no position the click cannot reach. Whether anything is in front to
continue is the runtime's question, not the gesture's.

**The step's own tie flag is checked first**, and that precedence is what keeps a slide
unambiguous: a slide both holds notes and is tied, so without it two rows would claim the same
step. A tied step's click means *break the joint here*.

**Lengthening a note means clicking the step it swallows**, not the source note. The pointer’s step
always owns the changed flag, including the last step and a note with a rest before it.
`clicking_a_note_with_a_rest_in_front_of_it_ties_it_all_the_same` and
`a_note_ties_to_the_note_in_front_of_it_wherever_it_sits` guard that direction.

`TieAction::Slide` stays a distinct variant purely so the hint line can say *"click again to slide"*
rather than "tie" — the word is what tells you the pitch moves under a gate that stays open.

**The panel's hint line reads from `tie_action` too.** A gesture whose meaning depends on what a step
holds is only usable if the text describing it is derived from the same answer; composing the hint
separately would be a second implementation of the rule, free to describe the wrong one.
`the_hint_line_describes_the_click_that_will_actually_happen` is what holds them together.

**Clicking the sequencer panel's empty space deselects too**, because Escape is the discoverable
half of nothing — clicking away is what people reach for first. Both call `deselect_step`, so there
is one decision with two doors onto it, not two implementations to keep in agreement.

Two properties make it safe, and both are tested:

- **Scoped to the sequencer panel, not the window.** While a step is selected the knobs write *into*
  that step, so a click on the parameter panel must not deselect — that would make editing a lock
  impossible.
- **Registered before the panel's contents, so it sits underneath them.** egui gives a click to the
  last widget registered under the pointer, so every control drawn afterwards still takes its own.
  The risk this carries is worth naming: the region is invisible and spans the whole panel, so if
  that ordering ever inverts the symptom is *controls that stop responding*, not anything that looks
  like a bug in deselection. `a_control_in_the_sequencer_panel_is_not_swallowed_by_the_deselect_background`
  is the guard, and `clicking_a_step_selects_it_and_clicking_again_ties_it` covers the step row.

There is **no gate strip**. Joined buttons express duration; the inner bar says only that a step
holds notes.

#### What MIDI cannot keep, said on save as well as on load

A tied run is a note longer than one step, which MIDI expresses natively. Two things it cannot hold,
and `smf::write_report` names both rather than letting somebody find them by ear:

- **The legato joint.** A slide — a tie carrying notes — is a note-off and a note-on at one tick. What is lost is
  *and do not retrigger*, which is a property of the gate. It reads back as an ordinary note.
- **A run that wraps the loop point**, truncated at the bar, naming the step.

Reading accepts exactly two lengths: **half a step**, or a **whole number** of steps. One and a half
is expressible in MIDI and not here, so it is refused with the note and tick named — quantising
somebody's export would be rewriting it. **One step is the exception**: it is what this player itself
writes at a legato joint, so refusing it would reject our own file. It loads as an ordinary note and
the shortening is reported.

**One gate per step bounds what can be imported, and it is broader than chords.** Each run states
what it needs of each step it crosses; a disagreement is refused, naming the step and the notes. That
catches unequal chords *and* staggered overlaps — a note across steps 1–3 while a half-step note
starts on step 2 — which no rule about notes *starting together* would see.

### Bars: steps → bar → pattern → sequence

The player is a **loop workbench feeding a DAW**, not a DAW. A person works in eight bars or
fewer; a machine over the CLI can author a whole track. That split is the design: the interface
commits to a fixed geography — **always eight chips**, one `PATTERN_BARS` pattern — while the
sequence underneath has no maximum length and export renders all of it as a stem.

- **The step row shows exactly one bar**, numbered **one to *n*, always** — the chips say which
  bar it is. Internally everything stays in absolute steps; the CLI addresses them both ways,
  absolute (`33`) and bar-relative (`3:1`), two spellings of one step. The row at *n* ≠ 16 is
  moot: `MAX_STEPS_PER_BAR = 16` is the owner's stated ceiling.
- **The bar strip shows the playhead as well as the shape.** The sounding bar takes the `success`
  fill -- the same token the step row lights the sounding step with, so one colour means "this is
  sounding now" across the panel, and its number switches to `canvas` for the same reason the step
  row's does: ordinary text colours are picked to sit on a surface token and go muddy on an accent
  -- while the shown bar keeps its accent outline. Fill and outline
  never compete, so a chip says both at once, which is the common case when a bar is being
  auditioned. `playing_bar()` is the one decision both the chips and the dump read, and it is
  `Playing` only: stopping parks the playhead on a step, and a chip that stayed lit would claim a
  bar is sounding when nothing is. A playhead outside the shown eight lights nothing -- the window
  does not chase it, because navigation is the person's.
- **The published playhead and editing steps are `u32`.** Their packed words have room below the
  transport bits; do not introduce a narrower reporting ceiling than the unbounded sequence.
- **Bars materialise on content, never on navigation.** The ‹ › steppers move the eight-chip
  window freely — wander to pattern twelve, look, leave, and nothing grew. The first note, tie or
  lock into a bar beyond the end grows the sequence to reach it (`grow_to_reach`, called by every
  write funnel — not least because reads and writes past the pattern's length **wrap silently**,
  which is also why the step row guards every read on a virtual bar). The bar counter shows the
  truth; typing a smaller number is the one deliberate delete.
- **Lowering a dimension deletes immediately, without confirmation.** Typed entry is the deliberate
  act. The counters are **click-steppers plus typed entry, no dragging**: a
  drag fired a resize per frame and applied whatever value the drag died on.
- **There is no tie invariant.** There was one — *every tied step must reach a note walking
  backwards* — held by guards at the funnels and by `Pattern::untie_orphans` after every structural
  edit. It is retired (see *Ties and notes are independent*): a tie reaching nothing is silence, the
  runtime has always played it that way, and enforcing it meant edits that rewrote steps the author
  never named. **Do not reintroduce a repair walk.** If a pattern looks wrong, the question is what
  the runtime does with it, not what the editor should have refused.
- **The bar clipboard carries the plugin its locks were copied under** and drops them on paste when
  that no longer matches — the notes and ties still paste.
  It is discarded when steps-per-bar changes: truncating or padding a pasted bar would silently
  reshape music, and discarding cannot lie.
- **The selection is an anchor plus companions.** The anchor keeps every meaning a selected step
  has always had — it previews, it parks, the panel reads it — and the companions follow silently.
  Shift-click extends a range from the anchor; Ctrl-click toggles individual steps (every fourth
  step, in one gesture); both live within the shown bar and leaving it drops them. A note played
  or a lock written lands on **every** selected step at the same absolute value; a refusal past
  the anchor keeps what landed and says so, because undoing an edit that was already heard is
  worse. The CLI spells it `select 5-8` or `select 1,5,9` — the first named step is the anchor.
- **Ctrl+C/X/V act on the selection, or on the bar/pattern when there is none** — following the
  same `[Bar | Pattern]` toggle the buttons follow, behind the same `typing` guard as every other
  shortcut. A copied run of steps pastes **at the anchor and replaces what is under it** — notes,
  ties and locks, a tied target included, exactly as pasting over a selected word; a lock the
  clipboard does not carry is gone afterwards, never merged. A run longer than the bar wraps on
  into the next, growing the sequence if its content reaches beyond the end.
- **The clipboard chords are read as `Event::Copy` / `Cut` / `Paste`, never as key presses**, and
  getting this wrong is silent. `egui-winit` recognises Ctrl+C/X/V itself and pushes those events,
  returning *before* it emits any `Event::Key` — so `consume_key(COMMAND, Key::C)` waits for
  something no keyboard produces, and the shortcuts were dead in the app while nothing failed.
  Reading the events also picks up the platform's other clipboard chords (Ctrl+Insert,
  Shift+Insert, Shift+Delete) for free.
- **A copy also writes the copied steps onto the system clipboard, as text** — and that is what
  makes Ctrl+V arrive at all: `egui-winit` emits `Event::Paste` **only when the system clipboard
  reads back as non-empty text**, so with an empty one, or one holding a file or an image, the
  chord is swallowed entirely. Without this the shortcut would work or not work according to what
  was last copied in another application. The text is an export, never what pastes: paste applies
  the bar clipboard, which carries locks that no line of step names could round-trip.
- **Random fills the shown bar**, and grows the sequence to reach it like any other content. A
  bar is the unit every other tool works in, which is what lets Random compose with them: roll
  bar one, keep it, move on and roll bar two. Anything whole-pattern is unusable past bar one —
  selecting bar two and pressing it threw the sequence away and wrote a single bar back, the
  reported fault. The generator is asked for a bar of the sequence's own `steps_per_bar`; a fixed
  sixteen would overrun a bar of twelve or silently re-bar the music. *Clear* beside it stays
  whole-sequence; `Clear bar`/`Clear pattern` in the tools row are the scoped ones.
- **What Random writes is the owner's design, tuned by ear (2026-09-17)**, and
  `src/sequencer/random.rs` carries the study behind it. **Rhythm:** three Euclidean rhythms, each
  with 3–13 pulses in 16 and a random rotation, and every two-step block copied from one of them.
  **Pitch:** C Dorian over one octave from C3. Steps rank by metric weight × (1 − position in the
  bar), and a step's rank sets how hard it leans on the Krumhansl–Kessler stable tones. That gives C
  about three times in four on the first step, and every tone equally likely on the last. The
  constants are listening decisions: a test pins the step ranking and the first and last steps' odds,
  so a change to them is deliberate. **Ties and slides ride on top and never move a note** — a tie
  only replaces a rest after a sounding note, a slide only makes a starting note legato — so about
  three bars in five carry a tie and one in three a slide. A press never writes an empty bar.
- **Copy/Paste/Clear act at two levels** — the shown bar, or its whole eight-bar pattern,
  following the `[Bar | Pattern]` toggle beside them. One clipboard, holding whatever was copied;
  Paste applies it at the matching level and **pasting is content**: the sequence grows to the
  last pasted bar that holds anything, and no further.
- **`Loop [ 1 Bar | Bar a-b | All bars ]` is transport state, not music.** The middle label
  names the shown pattern's span (`Bar 9-16` on the second page). No file keeps it, a render
  ignores it, it does not survive a restart. In `Bar` and `Pattern` scope the window **is** the
  sequence: the clock loops it, ties wrap within it, and `run_start_from` floors at its first
  step so a run reaching in from an earlier bar is neither continued nor asked for its locks.
- **A scope or bar change mid-play lands at the current bar's end, never mid-bar** — the clock
  queues it (`queue_window`) and applies it at a bar-multiple boundary, emitting
  `Boundary::WindowSwitch` immediately before the new window's first step. **The switch releases
  whatever is sounding**: two isolated bars are two sequences, and a destination bar opening with a
  tie has a `StepStart` that deliberately releases nothing — without the explicit release it would
  continue a note from the bar just left. Leaving `Bar` scope continues into the *next* bar
  (`Entry::Continue`) rather than restarting the sequence.

### The sequencer runs on the audio thread — and the control map does not

The two sit either side of the same line, deliberately, and the reason is the whole contract:
**a knob turn is not a note.** One frame of latency on a filter sweep is inaudible; one frame of
jitter on a note is a bad player. GUI frames carry ~16.7 ms of jitter at 60 fps, which is 13% of a
16th note at 120 BPM and 19% at 180.

[`src/sequencer/`](src/sequencer/mod.rs) therefore keeps its clock on the audio thread, and three
rules hold it together:

- **It has a playhead of its own, and it is advanced before the sleep/wake decision.**
  `AudioWorker::steady_time` looks like a free playhead and is not one: `render` writes silence and
  `continue`s *before* incrementing it, so it advances only while `RunState::Running`. A rest
  produces quiet buffers, quiet buffers sleep the plugin, and a sequencer keyed on `steady_time`
  would hang on its first rest — permanently. Its notes land in `input_events`, which is what
  `woken` tests, so the sequencer wakes the plugin by construction rather than through a second
  mechanism.
- **One clock, not three.** `clock.rs` accumulates sixteenths; step index, phase, gate close and
  the transport's beat position are all *derived* from that one number. The obvious design keeps a
  frame counter, a latched gate duration and a beat position separately — three quantities that
  must agree with nothing forcing them to, which come apart the first time somebody drags the
  tempo. Here a gate-close *boundary* cannot cross its next step start because 0.5 < 1.0 by
  construction.

  **Ties did not change that, and the distinction is the reason.** A boundary still lands where it
  always did; what outlasts a step is the **note**, because `Runtime` declines to act on a boundary
  when the step is tied. The alternative — a per-step gate length in the clock — reintroduces the
  latched duration that comes apart the first time somebody drags the tempo. The clock says *here
  is a boundary*; what a boundary means was never its job.
- **Sequencer and queued events go into one sample-offset-ordered stream.** `FixedEventBuffer`
  exposes events in push order, so appending the sequencer's afterwards would hand the plugin a
  non-monotonic stream whenever a keypress or a panic fell between two boundaries in one chunk.

Two smaller rules that are easy to get wrong:

- **Sequencer gates ignore host sustain.** `PressTable` has one global `sustain_held` and defers
  matching releases; a pedal must not turn a fixed 50% gate into a drone. That is what
  `PressTable::take_exact` exists for.
- **Nothing may treat a release as delivered that no plugin received.** `service_commands` renders
  nothing, so a Pause handled there records what it owes and the next `process()` call delivers it.
  **The same rule covers a modulation offset**: a zero the input arena rejects is re-owed, because a
  dropped deviation is replaced by the next step and a dropped zero is the *end* of the modulation,
  which nothing replaces.
- **The action vectors are sized from one shared constant, checked at compile time.** The worker
  copies the runtime's actions into its own scratch and prepends its owed work; sizing the two
  separately is how one ends up smaller than what it is handed and **allocates inside the audio
  callback**. That happened once, when locks began emitting one action per parameter per boundary
  and only the runtime's vector was resized. `MAX_ACTIONS_PER_CHUNK` carries a
  `const _: () = assert!(..)` beside it, so the drift refuses to build rather than being reported.

## Step locks

### A step sets parameters as well as notes, and one funnel decides that

Select a step and move a knob: that step now sets it. The gesture is not new — selecting a step
already makes the keyboard modal, and `note_on` writes into it. This is the same rule for knobs, and
Escape stops both.

- **Nothing suppresses the sequencer while a knob is held, and nothing should.** A hand moves the
  parameter's *value*; a step moves an *offset* laid over it. They are different layers and cannot
  fight, so a value set during playback simply stays set. An earlier design stood the automation down
  while a gesture was open, which was right when a lock was a value; kept after the change it froze
  the offset instead, and a held parameter stuck at whichever step had last fired.
- **A lock is sent as `CLAP_EVENT_PARAM_MOD`, never as a value.** CLAP modulation is an offset laid
  over a parameter without disturbing it, so the parameter's own value stays the patch for as long as
  the sequence runs. Every defect this feature had was some version of a step's value becoming the
  patch; as modulation that is **structurally impossible** rather than prevented by care, and three
  pieces of machinery went with it:

  | Gone | Why it existed | Why it does not now |
  |---|---|---|
  | Held-parameter suppression | The sequencer's values fought a knob under the hand | A hand moves the *value*, a step moves the *offset*. Different layers cannot fight |
  | `restore_patch_values` | Leaving a step had to undo what showing it did | The runtime previews the step and takes the preview off itself |
  | `SequencerParam` writing `param.value` | The panel had to follow what the sequencer set | The value does not change, so there is nothing to follow |

  **What it costs**: `ext_params_get_value` reports the *modulated* value, so a read-back during
  playback is the patch plus whatever the playhead is applying. Two defences, both required —
  `follow_sequence_patch` skips a parameter that is locked, and `refresh_params` puts the panel's
  reading back through `show_patch_for_sequenced`. Without either, the patch walks up by one
  deviation per requery, which is the same collapse arriving through the read-back. A lock set
  arriving from a file or from parking gets no entry from the follow, so `show_patch_for_sequenced`
  adopts the baseline stored beside it.

  **And it costs the host's own oracles.** Because the panel's reading is corrected from the player's
  record, *nothing on the host side can distinguish an offset from a value* — every player-level
  assertion passes either way, and so does anything reading `state()`. The only witnesses are the
  instrument, which holds the two apart, and the sound. A test that means to check the transport must
  render audio or run inside the plugin; a test that reads `state()` is checking bookkeeping.
- **The runtime owns every offset the sequencer applied, and takes it off itself.** A parameter that
  leaves the lock set, and every locked parameter when the transport comes to rest, is owed a zero;
  the runtime records the debt and the worker emits it at frame 0 of the next process call, which is
  the same owed-work pattern note releases already use for the same reason.

  **Only the runtime can order this.** A zero sent from the host would travel the event queue while
  the state that stops the stepping travels the command queue, so an old step could overtake the zero
  and leave the parameter stuck. It is also what keeps there being **one** owner of an applied offset
  rather than two that can disagree: the player clears only the offsets *it* sent, which are the ones
  that show a step while it is being edited.

  Abandoning happens on the **transition** into rest, not on every publish. A stopped sequencer
  receives a new state every time a lock is written, and abandoning on each would take the offset off
  the instant it was applied - editing a step would be silent.
- **An offset must be taken off whenever its lock stops existing**, and the runtime does it: it sees
  the parameter leave its lock set when the new state arrives, and owes it a zero. Clear, a sequence
  or MIDI load, a plugin change and a pruned parameter all reach it that way. The host sends nothing.
- **Only a parameter that advertises `IS_MODULATABLE` can be sequenced.** The player hosts any CLAP,
  and a plugin that has not advertised modulation for a parameter has said it will not take a
  `PARAM_MOD` for it. Sending one anyway is out of spec, not merely unlikely to work.
- **`Payload::ParamMod` coalesces exactly as `ParamValue` does.** Editing a step is a drag, so it
  produces one per frame; without coalescing a drag fills the fixed input arena, which lowers
  `complete`, which invokes global recovery — an all-notes-off because somebody turned a knob.

#### The runtime previews; gestures, parking and settling

- **One thing applies offsets, and it is the runtime.** The published state carries which step is
  being edited, and the runtime previews it — so selecting a step lays that step over the instrument
  and leaving one takes it off, in the same place that applies and removes a *playing* step's
  offsets. The host applies none.

  **The debt is provably bounded, and that is the reason for `applied`.** A zero is owed only for a
  parameter the runtime actually applied, and nothing becomes applied except while rendering — which
  is also when the debt is paid. So between two renders the debt can name at most what was applied at
  the last one, which is at most one pattern's worth. A fixed set alone would not do it: it cannot
  overflow, but filled with parameters that were never modulated it would crowd out one that was, and
  that one would stay modulated for ever.

  This is the rule three review rounds kept circling. With the host previewing as well, an offset was
  owned by two parties, and every question about taking one off — a stop, a Clear, a plugin change, a
  queue that would not take the event — had to be answered twice, in a place that could not order its
  answer against the other's. One owner deleted the host's owed-zero machinery entirely.

  **Values stay the host's, and the host puts one back when the hand lets go.** When the plugin's own
  editor moves a parameter while a step is selected, that is the patch moving — but the editor reports
  *every frame of a drag*, so restoring on each one puts the knob back under the hand sixty times a
  second. That is the *"it jumps back to the original value"* fault. A drag is bracketed by gestures,
  and that is the seam: while one is open the parameter is somebody's to move, and the base is
  restored on the close. What the step sets was recorded on the way past, so nothing is lost by
  waiting.

  **A control edited in the editor keeps its value for as long as the step stays selected — at
  rest, and only at rest — and a click parks the same way a drag's release does.** The base is
  *parked* at the step's value — the knob sits at the locked position, no arc, because there is no
  modulation: the value is the base. Three shipped versions returned the base to the patch on
  release; each was correct by the modulation model and rejected by the person using it, because a
  knob that springs back reads as refusing input whatever annotation rides with it. The same
  rejection arrived for the instantaneous form — a Slide toggle clicked On sprang straight back to
  Off while the lock previewed on top, reading as a dead control — so an instantaneous editor edit
  at rest parks too (`a_toggle_clicked_on_at_rest_stays_on_while_the_step_is_selected`).

  **And parking is reached by arrival as well as by editing: stopping with a step selected, and
  selecting a step at rest, park that step's locks onto the base.** The reason is a click the
  player cannot see. The editor displays base plus modulation and writes the base relative to
  what it displays — with a lock previewed as modulation, a toggle shows On over a base of Off,
  so clicking it Off writes a base that is already Off and **no event is emitted at all**. A lock
  made under a running transport was therefore stuck after Stop: the dot could never be removed,
  reported from exactly that flow. Parked, the base carries the lock, the preview is held to
  zero, and the next click inverts something real.
  `a_lock_made_live_survives_stop_and_the_editor_can_still_remove_it` is the repro;
  `selecting_a_locked_step_at_rest_parks_its_locks` covers the other door in. `parked_bases` names those parameters, travels to the runtime as
  `SequencerState::held` so their previews stay zero, and `unpark_bases` restores them wherever
  the parked world ends: leaving the step — deselecting **or selecting a different one** — Clear,
  the loads, and **starting playback** — a parked base under a running sequence would have the
  stepping's offsets land on top of it. A plugin change drops them instead of restoring, because the
  write would land in an instrument being torn down.

  **Under a running transport, the base goes back the moment the gesture closes.** Parking works by
  held-zeroing the parameter's offsets, which is invisible at rest and catastrophic mid-take: with
  the sequencer stepping it silenced every step's deviation and left the base at the hand's value,
  so the whole sequence sounded the edited step's cutoff — *"it still changes it for all the other
  steps too"*. The drag itself still sounds the hand on every step (the editor moves the real
  parameter; that is the audition), but on release a live take needs the base at the patch and the
  offsets doing the work. `restore_base_for_step_edit` restores whenever the transport is not
  stopped; the knob showing the stepping's modulation afterwards is correct, not a spring-back.

  **A load settles the instrument on the record's patch, unconditionally.** The plugin's state
  restores what the instrument was *left at*; the lock record restores what its deviations are
  *measured from*; nothing else ties them together, so a base stranded by an old bug survived every
  launch — watched live: cutoff restored at 0.946 under a record saying 0.890, sounding a step's
  lock on every unlocked step while every corrected view showed the patch. `settle_bases_to_patch`
  pushes the record's patch for every locked parameter after a plugin or sequence load — with no
  comparison first, because the panel snapshot is corrected and reads as the patch precisely when
  the instrument disagrees.

  The arc still exists — it is what a *deselected* deviation looks like from the editor.

  **During the drag, everything canonical is left alone.** The pending value waits in
  `pending_step_edit`, keyed by `(step, parameter)`, and an existing lock's preview is *suppressed*
  — `SequencerState::held` names the parameter and the runtime emits zero for exactly it — rather
  than the lock being cleared and re-created. The cleared-and-re-created version lost the lock to a
  deselect mid-drag and could persist the gap to disk. Pending edits are dropped, never committed, on
  every invalidating path: deselect, Clear, both loads, a plugin change, tracking invalidation.

  **During the drag the lock is deferred, not written.** The plugin's base already carries the value
  under the hand, so a lock written per frame has the runtime lay `lock - patch` on top of it and the
  drag sounds at roughly twice the deviation. `pending_step_edit` holds it until the gesture closes,
  and the step takes where the drag ended.

  **The return of the knob is information, and it is drawn.** On release the knob goes back to the
  patch — the value is the patch, the deviation is modulation — and without an accompanying shape
  that reads as the control refusing input, which is exactly how it was reported. `ParamView` carries
  the modulation offset and the knob draws an arc from its position to where the modulation takes it:
  the marker is yours, the arc is the sequencer's.

  **A locked parameter's reading cannot observe this**, which is worth knowing before writing a test
  for it: a requery corrects it to the patch by design, and `Session::state()` requeries first — so a
  session test reports the patch whatever the player did. `AppHarness` reads the snapshot as it
  stands, and is the harness for anything about the host's own bookkeeping.

  The host puts it back — the deviation is expressed by the
  lock, not by leaving the parameter where the editor left it.

#### Step edits, zeroes, and locks per step

- **A step edit is modulation too, not just a playing step.** With a step selected the player sends
  the deviation as an offset: you hear the step, the patch beneath it is untouched, and the
  instrument's own editor can mark the knob because something other than the knob is moving it. An
  offset is cleared whenever its lock is — on reset, on returning to the patch, on deselect, on
  Clear, on a load, on a plugin change, on a rescan and on stop.

  **The panel's snapshot records a value only when a value was sent.** With a step selected the
  parameter's own value did not change, so recording the step's number there would make the panel's
  idea of the base *be* the step — and anything reading the base back, including the baseline a
  loaded lock set adopts, would then get the step instead of the patch.

  **A zero that would not fit is retried.** `Payload::ParamMod` is droppable and the last zero for a
  parameter has nothing behind it, so losing it leaves the plugin modulated for ever; the entry stays
  in the cache and `service` sweeps until it lands. For the same reason a queued
  `PluginOutput::SequencerParam` is ignored for a parameter that is no longer locked — it would
  otherwise resurrect an offset already dropped.

  **A lock set is re-checked against whatever plugin loads next.** One loaded with no instrument was
  validated against an empty parameter set, which validates nothing.
- **A lock is a deviation from the sequence-patch, and the unlocked steps put the parameter back.**
  This is the rule the first implementation left out, and leaving it out made the feature do nothing:
  a parameter is one value on the plugin, so setting it at step 3 leaves it there for steps 4, 5 and
  6 as well. Every step therefore sets every *locked* parameter — to its own value where it has one
  and to the patch value where it does not — and parameters nobody sequenced are never touched.
  **Since locks became modulation this is expressed as an offset**: the step's deviation where it has
  one, and zero where it does not. Zero is not silence — an offset left in force holds the parameter
  there for the rest of the bar, which is the same defect in its modulation form.
- **Locks are per step, ties included — the owner's ruling.** A step's locks are its own, whatever
  its gate does: a knob turned while a hold is selected writes the hold, the runtime applies each
  step's locks at its own `StepStart` whether or not a note event fires there, and a hold's lock
  therefore moves the parameter **under the held note** — modulation walking while the gate stays
  open, which is the point. There is **no inheritance**: a head's lock does not carry through its
  run, and a step with no lock for a parameter returns it to the patch, exactly as every unlocked
  step always has — a hold that wants the offset keeps its own lock, which is what the panel shows
  and why the behaviour is predictable from it. Tying no longer empties a step of anything, and
  the restore paths keep locks on tied steps: they are music now, not leftovers. The lock dot
  draws on every step that locks something, because that is exactly where the value is heard.
  `a_steps_locks_are_its_own_tied_or_not` pins the runtime half;
  `an_export_reads_each_steps_own_locks_exactly_as_playback_does` the render half.

  This deleted the `run_start` **lock** resolution wholesale — the head-owns-everything rule, the
  tying-empties rule, and the drop-locks-on-tied-steps restore sweep all defended the old
  invariant and went with it. Live and rendered agree by reading the same cell, with no resolver
  left for the two paths to keep in step; a run crossing the loop point re-reads its locks at
  step 1 for the same reason — step 1's locks are step 1's — identically live, rendered and in
  the file.

#### The sequence-patch, and how an edit is read

- **The sequence-patch is simply what the instrument is set to while no step is selected.** Select a
  step and you are editing that step; select nothing and you are editing the patch. It can change at
  any time. `follow_sequence_patch` keeps it true on every frame, which is why nothing has to
  *detect* a patch change: a preset chosen from the plugin's own browser is a great many parameters
  moving while nothing is selected, which is exactly what a new patch looks like from here. It runs
  in the frame loop rather than in the panel, because a rule that held only while one panel was
  visible would be no rule at all.
- **One parameter is a hand; several at once are a patch.** `plugin_moved` is the single handler for
  everything the plugin reports, and it reads intent from shape: a person turns one control at a
  time, so one parameter arriving from the plugin's editor is somebody sequencing, while a whole
  instrument's worth arriving together is a preset being chosen. Without this, choosing a patch with
  a step selected wrote the entire patch into that step as locks and every step then sounded the
  same — *"I can only ever use one sound with a sequence"*.

  **This is a heuristic, and it is here because CLAP offers nothing better.** A plugin reports a
  preset load exactly as it reports a knob: `begin`, a value, `end`, per parameter. There is no flag
  for "this was a patch change", so shape is the only evidence. Its one wrong answer is a preset that
  changes a single parameter, which is recorded as a lock — one unwanted dot, undone by setting that
  parameter back to the patch. The proper fix is for the instrument to say so; see the open question
  in `docs/briefs/mxm-player.md`.
- **A value from the plugin clears `editing` for that parameter.** The plugin is the authority when a
  value moved for a reason that was not this controller, and `editing` otherwise wins both when the
  panel draws and when the sequence-patch follows — so a stale entry hid the arriving value and put
  the old one back a frame later, and a patch change never registered at all.
- **Leaving a step puts the instrument back to the patch.** Editing a step sends the value to the
  plugin so you can hear it, which leaves the instrument sitting at that step's value — and the patch
  follows the instrument the moment no step is selected. That was once a real hazard: leaving a step
  turned that step's value into the patch, every other step agreed with it, and the sequence went
  flat. **Modulation removed the hazard rather than guarding it** — a step's deviation is an offset
  laid over the parameter, so leaving one takes the offset off and the value was never moved. The
  runtime does that, in the same place it applies the preview.
- **The follow reads what the control is showing, not `params`.** An edit reaches the plugin through
  the audio thread and is read back afterwards, so `params` reverts to the pre-edit value for a frame
  or two after every requery. Following that records a value nobody chose — most visibly the factory
  default, a moment after somebody dialled something else. `editing` first, then `params`, which is
  the resolution the panel draws with.
- **A drag ending on the patch value is a real lock; a click landing there is the reset.** Two
  owner rulings, and the line between them is the gesture's shape. A lock is an **absolute
  value** — a step pinned to today's patch value stays pinned when the patch moves tomorrow — so a
  *drag* (a gesture carrying many values) that settles on the patch pins there; collapsing it made
  the step follow every later patch edit, watched happening live before that was removed. But a
  **click** — a toggle, a click-jump, a text entry: a gesture carrying exactly one value — landing
  back on the patch means *this step sets nothing*: the lock clears, off and no dot, which is the
  second ruling, made from a Slide toggle whose Off left the dot behind. The count of values per
  open editor gesture (`open_gesture_edits`) is what tells them apart at commit, because the
  editor's begin/value/end can straddle two service turns and arrive shaped like a drag; the
  panel's same-frame gesture flags answer it for its own controls; the double-click reset remains
  the explicit form. **Under a running transport a click landing on the value the step already
  locks clears it too** — live, the base cannot park, so a toggle whose lock is On always shows
  Off in the plugin's editor and every click emits On: the control cannot express Off at all, and
  the lock was stuck until Stop. A click writing exactly what the step already sets is the same
  toggle clicked again, and its second click means off; the rule stands down at rest, where
  parking makes the control alternate honestly and a click on a control showing Off must not
  delete an unseen lock. `a_toggle_click_alternates_the_lock_while_the_transport_runs` is the
  reported repro, pinned. And **clearing a parameter's last lock keeps its `sequence_patch` entry**:
  the clear runs with a step selected, which is exactly when `follow_sequence_patch` stands down —
  removing the entry let the next editor edit re-derive "the patch" from a snapshot already
  holding the arriving value, after which a click back to the real patch no longer read as one.
  `a_toggle_clicked_back_to_the_patch_clears_the_lock` drives both batch timings;
  `a_drag_ending_on_the_patch_value_still_pins` holds the other ruling.
- **Every path arrives at `PlayerApp::parameter_edited` and then `deliver_edit`.** The player's own panel, a mapped hardware
  knob through `apply_mapped_cc`, and the plugin's own editor, whose knob turns reach the player as
  `PluginOutput::ParamValue`. Adding a fourth way to move a parameter without routing it here would
  mean sequencing worked in some places and silently did nothing in others — the same shape of defect
  as *whatever the note keys do, a MIDI keyboard does too*.
- **The value still reaches the plugin.** You hear what you are setting, exactly as playing a note
  into a selected step sounds it.
- **§7.1's double-click is reinterpreted, not duplicated.** With a step selected, "put this back"
  means the *patch's* value, and a step that sets nothing is precisely what that is. **The
  instrument's editor reaches the same clearing through its values**, because CLAP carries no
  "clear": an *instantaneous* editor edit (gesture opened and closed in one batch — a
  double-click's shape) landing exactly on the patch value while a step is selected is read as
  this reset and clears the lock; on a **parked** parameter the same reading accepts the factory
  default, since parking blinds the editor's own reset-target to everything else. Drags are
  untouched — they arrive through the gesture branch, so the no-collapse rule stands: a drag
  ending on the patch value is still a lock at that value. The
  instrument's editor obeys the same rule from its side of the wall: while a host modulates a
  parameter its unmodulated base **is** the patch, so `binding::reset_target` aims the editor's
  double-click there instead of at the factory default. Shipped wrong once — a Range lock sounding
  2' double-clicked to 8', the factory default.

  **Both readings hold at the gesture-close commit as well as in the instantaneous branch.** The
  editor brackets its double-click in a gesture, and the batch loop opens the gesture before it
  interprets the values, so a bracketed reset is routed to the pending edit and decided in
  `restore_base_for_step_edit` — which read only the *patch* as a reset. On a parked knob the
  default therefore came back **as the lock**: base parked at the default, no modulation, no dot
  on the knob, and the step's dot still there. Reported from exactly that — mxm-poly-06's Noise
  knob, *"there is no dot because I double clicked it, but the lock seems to stay"* — and the
  lock's value was the factory default, which is what named the cause. The parked state is
  recorded **when the gesture opens** (`gesture_began_parked`), because the held branch parks
  every editor gesture at rest for the drag's duration, and reading that would turn any click
  landing on the default into a reset. The batch loop's begin and end arms go through
  `plugin_gesture_began` / `plugin_gesture_ended` for the same reason: one door, so what a begin
  records cannot differ by timing. `a_bracketed_reset_on_a_parked_knob_clears_the_lock_too`
  drives the one-batch and the split timings; it failed at `Some(0.9457)` — the default — before.
- **The patch value is read from what the control was showing, before the edit is applied.** It is
  not the parameter's factory default and it is not the value being written; recording the latter
  would make clearing a lock return the parameter to the lock. `parameter_edited` runs before
  `editing` is updated for exactly this reason — do not reorder it.
- **A selected step's value wins in the panel**, value and formatted text together, through the one
  `step_text` argument. Two arguments — "is it marked" and "what does it read" — is how a slider ends
  up sitting at one number and printing another. The text is cached because formatting calls into the
  plugin, and rebuilt only when the selection or the sequence serial changes.

#### The state reaches the audio thread by pointer

**The sequencer's state reaches the audio thread by pointer, and the audio thread never releases the
last reference to one.** `Command::SetSequencer` carries an `Arc`, so a ring slot is 24 bytes rather
than a whole state — by value it was ~6.9 KB a slot across 64 slots, and **while that was true the
state could not grow past a compile-time constant, which is the same as saying a sequence has a
maximum length.**

The reclamation rule is the part to keep intact:

- The **writer allocates**, on the GUI thread. Growth happens there, which is what a fixed-capacity
  handoff — a triple buffer over pre-sized slabs — could not offer: a ceiling *is* a maximum length.
- The worker reads what it needs and **hands the `Arc` straight back** through the retire queue.
  Dropping it there could call the allocator inside the callback.
- The retire queue is **as deep as the command queue**, and `Engine::set_sequencer` drains it before
  pushing. Each consumed command retires at most one state, so the states in flight can never exceed
  the commands in flight and the hand-back cannot fail.
  `retiring_a_sequencer_state_cannot_overflow_its_queue` is that argument as a test, and
  `consuming_a_sequencer_state_never_allocates_or_frees_on_the_audio_thread` is the property it
  protects.

**Nothing on the audio thread may call `LockSet::param_ids`** — it returns a `Vec`. Use
`param_id_at`. Three call sites in `Runtime` did, and had since locks were added; the allocation
test above is what found them, because no earlier test published a sequencer state inside a measured
callback.

`LockSet` is `Copy` and heap-free, with **NaN meaning "this step sets
nothing"** — a value that came from a control is finite by definition, so the sentinel costs no
range. The audio thread visits them through `for_each_at`, never through the allocating `at`.

#### No maximum length: budgets and storage

**A sequence is `bars × steps_per_bar` steps, and there is no maximum.** Storage is heap and the
audio thread reads it through an `Arc` it never clones, so length costs memory and nothing else.

**Three ceilings were tried and all three were invented.** Sixteen steps, because the command ring
copied the pattern into all 64 slots — a real constraint, removed by the pointer handoff. Then
sixteen *bars* and thirty-two steps a bar, for nothing: the second justified in a comment by what a
mouse can reach, which is an **interface fact sizing a data structure** — the third time this
codebase has made that mistake, after a controller fact and a plugin fact, and the first two are
criticised a few paragraphs below. It also contradicted the requirement outright: an
AI-authored bassline for a whole track is ninety bars, and the CLI is the path with no mouse.

**A render is exactly the sequence, and there is no constant for its length.** There was —
`export::BARS = 2`, *"one of pattern plus one to hear the tail"* — a guess the code made while a
sequence was always one bar, and wrong the moment one could be longer than the guess. Nine bars
render nine bars; a tail is empty bars somebody added. The finished-or-truncated classification
measures the sequence's **last** bar rather than an appended one, so adding silence is also how you
make a render report `Finished`.

**Resizing drops what falls outside, including the locks.** `Pattern` drops the steps and their
ties; the locks live in a separate structure and `PlayerApp::set_bars` / `set_steps_per_bar` drop
those, because nothing else can see both. A lock past the end would be stored, saved, unreachable,
and would come back when the sequence grew again — the same defect the tied-step rule exists to
prevent, one dimension over. The selection is cleared for the same reason.

`Runtime` holds the state by pointer and **must never clone it** — a clone of a heap-backed pattern
allocates inside the callback, and that is the only thing standing between here and a maximum length
coming back. `apply` returns the pointer it replaced so somebody allowed to allocate can release it. A step stays a sixteenth — twelve to a bar is
3/4 — and the other reading, where the count subdivides a fixed bar so 32 means 32nd notes, is a
*step rate*: a separate control, deliberately not this one.

**Both dimensions are persisted, on both paths.** A step list alone is ambiguous — forty-eight steps
is three bars of sixteen or four of twelve — so a file that did not say would be read back as
different music. Schema 4 carries them in a `.seq.json`, and `Settings` carries them so a restart
reopens at the size it closed. `migrate_from_v3` gives an older file the only shape it could have
had: one bar of sixteen.

**Nothing here is sized from a number belonging to an instrument**, and there are **two budgets**
because two different things are spent. `MAX_LOCKS` bounds the total number of locks, which is what
the bytes cost. `MAX_LOCKED_PARAMS` bounds the distinct parameters, which is what the *realtime*
cost is — every locked parameter emits an action at every boundary, so it is the figure
`MAX_ACTIONS_PER_CHUNK` has to accommodate. Conflating them is what the dense form did.

`MAX_LOCKED_PARAMS` is still 32 and its justification is new: it was that number because
MXM-mono-01 has 27 parameters — a plugin fact sizing a host structure, the same error as the earlier
cap of eight *"because a controller has eight knobs"* — and it is now what the action budget
affords. Raise it only by widening that budget, never to suit an instrument.

**Storage is sparse: a `(step, parameter, value)` per lock**, plus the parameters and their
baselines. The dense form spent `parameters × steps` and was nearly all empty — three knobs used 216
of 2,312 bytes — and every bar added would have multiplied it. Sparse costs *more* at sixteen steps
and less from about forty on, which is the trade: cost follows what was automated rather than what
could be. `Refused::NoRoom` is consequently unreachable until a sequence can exceed `STEPS`, because
the two budgets coincide there; the branch is kept for when length arrives.

**On disk the sentinel inverts.** `LockData` uses `Option<f32>`, because JSON writes a non-finite
float as `null` and then refuses to read `null` back as an `f32` — the NaN form does not survive one
round trip, and it fails on *read*, with the file already written.
`the_in_memory_nan_sentinel_cannot_be_written_to_disk` carries that evidence rather than asserting it.

#### Locks across plugins, files and paths

**Locks are tagged with the plugin they were recorded for, and fail gracefully otherwise.** Parameter
id 7 is a filter cutoff in one instrument and a glide time in another, so:

| Situation | What happens |
|---|---|
| Recorded for a different `CLAP_ID` | **parked**, written back out unchanged, and claimed the moment their own plugin loads. Auditioning a second instrument is not a mistake |
| The plugin matches but a parameter is gone | dropped by name, and reported. A synth keeps its id across versions that add and remove parameters |
| No plugin recorded (an older file) | kept: refusing them would throw away work for a rule that did not exist when they were written |

**The id is compared for equality, and nothing maps an old name forward.** A lock set tagged with a
CLAP id no instrument ships under any more is simply foreign: parked, written back out, waiting for
a plugin that will never load. That is the owner's ruling on the one rename this collection has had
— sequences written before it are not worth a migration — and `plugins/AGENTS.md` fixes the id for
good, so there is no second rename for a shim to serve. Do not reintroduce one.

**Every path that touches a pattern has an answer.** A set that survives some of them and not others
is the defect this table prevents:

| Path | The locks |
|---|---|
| **Clear** | cleared too. A pattern that looks empty and still sweeps the filter every bar is a trap |
| **Random** | **kept — all of them.** Locks are per step, tied ones included, so a lock on a step the roll ties is heard there; the old exception defended the old invariant. Rolling new notes under a sweep you built is the whole use of the button. Random also rolls the occasional **slide** (the owner's decision), and only where a note already sounds — the generator honours the same invariant the funnels guard |
| **Load `.seq.json`** | replaced by the file's — including replaced by *nothing* |
| **Load `.mid`** | cleared, and reported. MIDI cannot carry them |
| **Save `.mid`** | written without them, and reported **on save**, which the read path's list does not cover |
| **Plugin change** | revalidated, parked, never silently dropped |
| **Restart** | persisted in `Settings`, on a 500 ms debounce with a flush in `on_exit` — a knob drag produces a new value every frame, and every other thing that persists is discrete |

**The GUI is told what the sequencer set**, through `PluginOutput::SequencerParam`: a plugin never
echoes a value the host sent. Since locks became modulation it reports an **offset** rather than a
value, so nothing about the panel's reading follows from it — it is recorded so that *what the
instrument is sounding* can be shown, and nothing is decided from it, because it is droppable. That notification is **droppable**, and a drop sets
`PlayerHostState::note_unsettled_params` instead — a flag cannot be lost, and coalescing many drops
into one requery is what it is for. The flag is also set unconditionally when the transport comes to
rest, because the last lock before a stop has no next step behind it to republish what a drop lost,
and coming to rest is when somebody stops to look at what they made.

## The CLI

### The CLI: everything a person can do has a verb, and that is a contract

**The CLI ships.** It is part of the product, not scaffolding behind a development flag: a person
can hand the player to an AI assistant, which loads an instrument, writes a pattern, moves
parameters, runs the transport, renders audio and reads back what the window shows. The rules below
exist to make that interface trustworthy, not only to make tests possible — which is why a verb is
owed for every user-visible act, why `dump` is honest about what the window hides, and why a
machine-written lock takes the same funnel a human one does. Do not remove it from a release build.

Loopback only, and that is the boundary: an assistant runs on the same machine as the player.
Shipping the CLI opens nothing to the network.

**And two things a person cannot do from the player, the CLI can**. `cc 119 <view>` and
`cc 118 <0|127>` reach a loaded plugin's *developer channel* (`plugins/AGENTS.md`), which switches
the plugin editor's view or its expander — when the player was started from a shell with
`MXM_DEV_CC` exported, because the plugin reads the environment it is instantiated in. Nothing in
the player knows about it; the CC goes through the merge like any unmapped controller.
`fx dumpstate <n>` and `fx loadstate <n>` expose an effect's opaque CLAP state at a fixed local file,
so an automated developer can inspect or modify persistent model data without the player inventing
hidden parameters or product-specific commands.

`src/cli.rs` listens on a loopback socket (ephemeral port, published in `cli.json` beside the
settings) and answers one line with one JSON line. Commands are queued to the **GUI thread** and run
in `service`, because everything they touch belongs to it. `mxm-cli` is a thin client; a test calls
`cli::run_command` directly, and `t9_cli.rs` drives the whole thing over a real socket.

Three rules, and the first is the one that will be broken by accident:

- **A feature that lands without its verb has broken the machine-testability contract — and
  `tests/t10_cli_conformance.rs` now enforces it, not review and memory.** The CLI exists because
  a whole class of bug — base-vs-patch divergence — was invisible to every automated eye and had
  to be found with screenshots and a settings file. Whatever a person can do with the mouse, a
  machine must be able to do and observe over the socket; when adding a user-visible act, add its
  verb, its `dump` visibility, **and its `COVERAGE` row** in the same change. The sweep walks the
  accessibility tree across interface states and fails, by the widget's own label, on anything
  with neither a row nor an argued `EXEMPT` entry; the same binary executes every row over a real
  socket, so the table cannot rot in either direction.
- **Gestures route through one funnel.** Input handling may only act by dispatching a `Gesture`
  through `PlayerApp::perform` — the same one-funnel rule `parameter_edited` carries. The
  `gestures!` macro generates the enum and its `GestureKind::ALL` inventory from a single token
  list, and the sweep requires a coverage row per kind; a gesture implemented outside the funnel
  is a review-visible violation of this rule, which is the strongest guarantee available.
- **`dump` shows what the window shows *plus what it deliberately hides*.** The window's parameter
  values are corrected (a locked parameter reads as the patch, by design), so the dump additionally
  carries `raw_readback` (the plugin's own uncorrected answers), `sequence_patch`, `parked_bases`
  and the `modulation` cache — the bookkeeping the corrections are computed from. Removing any of
  these blinds the machine to the bug class that motivated the whole surface.
- **The `lock` verb goes through the same funnel a selected-step edit does** — never a direct write
  into the `LockSet` — so every guard (modulatable, capacity, baselines) applies to a machine-written
  lock exactly as to a human one. This is also what makes the CLI honest for AI-assisted
  composition: it cannot author a state the interface could not.

The CLI starts only in a windowed run (`PlayerApp::new`); headless tests attach their own with
`start_cli` on a sandboxed directory, or every parallel test would fight over one port file.

## Export and files

### Exporting audio renders a snapshot, and says what it cannot promise

`sequencer::export` renders through a **second plugin instance** with the live one's CLAP state
restored into it. That is not a refinement: `offline::render` is given a bundle path and a plugin
id, which produce the plugin's **default patch**. An export without the state transfer sounds
nothing like what was being listened to.

- **State is captured on the GUI thread** (`Engine::capture_state`), because it allocates and calls
  into the plugin. Only the bytes cross to the renderer.
- **The chain goes with it.** Every effect that is on has its own state captured the same way
  (`Engine::capture_fx_state`) and is run over the rendered source in order, at the same block size
  (`offline::through_effects`); one that is off is left out, as the live path leaves it uncalled.
- **Pending edits are settled first.** A parameter change reaches the plugin *through the audio
  thread*, so the panel can be showing a value the plugin has not consumed; exporting immediately
  after a slider move would otherwise write the previous value.
- **The locks are scheduled with the pattern, ahead of the notes at the same frame.** The restored
  state is the patch; the locks are the deviations from it, so an export without them sounds like the
  patch — the same failure the state transfer prevents, one level down. `EventKind::ParamMod`
  exists for this and is **not** bracketed by a gesture: a lock is automation, not somebody holding a
  knob. It carries an **offset on every step, including the zeroes** — an export that visited only
  the deviating steps would sound different from live playback, which is the one thing it must not
  do. A test comparing two exports must restore the live value first, because writing a lock also
  sends it; without that the exports differ because the *patch* moved and the test proves nothing.
- **A plugin with no `state` extension is refused**, naming the reason. Envelope negotiation does
  not require the extension, so this is reachable — and exporting a default patch instead would be
  the failure above, silently.
- **Peak normalisation is on by default and `normalise off` turns it off.** It is right for making
  samples — an exported hit is usually headed for a sampler — and wrong for every use of export as
  a *measurement*, because it removes exactly the thing being measured. Under it a pure gain change
  cancels perfectly: `set Master 0.05` and `set Master 1.0` render byte-identical files, which is
  how it burned an afternoon of comparing two patches that were genuinely different. The switch was
  in the settings panel with no command behind it; `normalise <on|off>` is that command, and
  `t10_cli_conformance`'s rule is why it had to exist — a machine reaches the same state over the
  socket. Comparing patches through a normalised export is still possible with a gain-invariant
  measure: regress one render onto the other and read the residual, as
  `mxm-creative-sampler-dsp`'s `examples/measure_converter.rs` does.
- **A slide's legato joint is ordered in the schedule, and the same-pitch joint is merged.** The
  render sends raw MIDI, which carries no voice identity: at a slide to a *different* pitch the
  new note-on precedes the old note-off (a plain off-first sort was a retrigger where live
  playback slides), and at a slide to the *same* pitch neither event is sent — the note simply
  continues, released at the slide's own end — because on-then-off for one key at one frame can
  only kill the note that just sounded. Live playback resolves that joint through the press
  table; raw MIDI cannot, so the render carries the joint's audible meaning rather than its
  events. **What that costs, stated:** a third-party instrument that sounds a repeated legato
  note-on audibly would render a same-pitch slide as a hold. The two are indistinguishable on
  this collection's instruments, whose voices define the joint as no-retrigger — the same
  reasoning `smf::write_report` already records for the `.mid` file's version of this loss.

**What the deadline does not cover.** Capturing state is a synchronous call into the plugin, so a
plugin that hangs there hangs the GUI, exactly as the fault-isolation section below says. Export
**inherits** that and does not worsen it. Do not let this section drift toward implying otherwise.

**Every path goes through `PlayerConfig`.** `exports_dir` sits beside `settings_path` and
`sequence_dir` for the same reason: a test that wrote to the real one would put files in a person's
own exports folder.

### The exported file is exactly the sequence

**Nine bars render nine bars.** It used to be exactly two — one of pattern plus a mandatory extra to
hear the tail — which made sense while a sequence was always one bar and became wrong the moment one
could be longer than the guess. Wanting a tail is said by adding empty bars, which anybody can do,
so the code stopped deciding it.

- **The pattern plays once.** The live clock wraps at the sequence's end; a render that inherited
  that would retrigger through the tail and never go quiet.
- **The last bar classifies, it does not shorten.** Silent throughout at **−90 dBFS** means the
  sound ended inside the file; anything sounding means truncated, and it says so. Checking only the
  final sample would call a delay tap that stops mid-bar "finished".
- **−90 dBFS was measured, not chosen** — mxm-mono-01's default tail ends at 0.169 s under −60 dBFS and
  0.338 s under −90, and the curve is vertical below −90: −100 and −120 are both reached within
  0.001 s of it. `measure_decay_floor` is the (ignored) test that produced those numbers.

  **They move with the init patch and were re-measured when it changed** — the retune took the
  release from 200 to 250 ms, and the previous figures (0.100 s and 0.239 s) were the old patch's.
  The threshold itself did not need to change, because what justifies −90 is the *shape* of the
  curve rather than where it starts.
- **32-bit float, live sample rate, the plugin's channel count.** Float is what makes normalising to
  exactly 0 dBFS safe — fixed point would risk inter-sample overshoot.
- **Normalisation is a checkbox, on by default.** Right for making samples, wrong for comparing two
  patches' loudness, so it is a choice rather than a policy. A silent render is never scaled: that
  would be a division by zero turning digital black into full-scale noise.

### MIDI is the interchange format; JSON is the fixture format

They are not competing. `.mid` goes into a DAW — what a person asked for. `.seq.json` is readable,
diffable and hand-editable, which is why fixtures use it and why MIDI cannot replace it.

`sequencer::smf` is a **hand-written** reader and writer: the subset is small and fixed, and a crate
that read arbitrary MIDI would carry far more than that costs to pin.

- **Tempo is canonicalised on save** to the value SMF can hold, so a file we wrote round-trips
  *exactly* rather than to within a rounding error.
- **Anything the model cannot hold is refused with a reason** — several note tracks, several
  channels, a tempo change, a time signature that is not 4/4, content past one bar, off-grid onsets,
  a tempo outside 20–300. Quantising somebody's DAW export would be rewriting it, not loading it.
- **What it can read but not keep is named before the overwrite**: velocity, note length, channel,
  and same-pitch collisions within a step.

**MIDI conformance is established, by something that is not us.** Testing our writer against our
reader would only prove they share assumptions, so both directions are checked against
[`mxm.midifile`](https://pypi.org/project/mxm.midifile/), an independent Python implementation:

- `tests/midi-conformance/verify.py` parses a file the player wrote and asserts its header, tempo,
  time signature, grid alignment, channel and note pairing. Run it when the writer changes.
- `tests/midi-conformance/foreign.mid` was **written by that library** and is loaded by
  `a_midi_file_from_an_independent_implementation_loads`. It is deliberately awkward — division 480
  rather than our 96, so the grid is rescaled, and a velocity we cannot keep, which must be
  reported.

**The file is written by `mxm-audio-file`, and the player decodes nothing.** Export and the session
driver's capture both go through the collection's encoder (float WAV, atomic write), and the `acid`
append lives there as `mxm_audio_file::acid`, moved unchanged from what was `sequencer/wav.rs`. The
player takes the **encoder only**: `mxm-audio-file-decode`, which holds MPL-2.0 symphonia, is a
dev-dependency its tests read exports back through, so `cargo tree -e normal -p mxm-player` shows no
symphonia crate. **A non-finite sample refuses the file** rather than writing a plausible WAV over a
plugin defect: an export fails with the sample named, and `Session::write_artifacts` writes the state
dump **before** the audio, so a capture of a misbehaving plugin keeps its state and says where the
NaN is.

**The `acid` chunk is unverified, and deferred deliberately** — there is no DAW on the development
machine to test it against, and no independent implementation of that chunk the way `mxm.midifile`
serves for MIDI. So **nothing is promised about DAW auto-sync**: the **file name** carries tempo,
and the WAV stays valid whether or not the chunk is understood.

*To close it:* drop an exported `.wav` into a DAW that reads ACID metadata and confirm it
tempo-matches without being told the tempo. If it does not, the chunk is wrong and should be fixed
or dropped — **not** left in while the claim is made. Nothing else depends on the outcome.

## Control mapping

### Control mapping runs on the GUI thread, and the audio thread learns only a mask

[`src/control_map/`](src/control_map/mod.rs) turns a hardware knob into a parameter edit. All of it
runs on the GUI thread, because that is where the three things it needs already live: the layout
file, the current parameter *values* (which pickup compares against), and the page display.

The audio worker is told only which CCs are claimed — a 128-bit [`CcMask`], `Copy`, 16 bytes, no
allocation and no `Arc` to drop on the audio thread. A claimed CC is routed to the GUI as
`PluginOutput::MappedControlChange` **instead of** to the plugin, never as well: two live paths to
one value is how a knob ends up acting twice.

The round trip costs one frame. A knob turn is not a note, and the alternative — publishing a map
to a realtime consumer and tracking parameter values a second time — buys nothing for it.

Three rules that are not negotiable:

- **Reserved CCs can never be claimed.** 120 and 123 are what the panic machinery uses; 1 is a
  performance gesture a plugin consumes without writing a parameter. `RESERVED_CCS` enforces it and
  a layout claiming one is refused whole.
- **A gesture must always be closed.** A CC has no release, so an idle timeout ends one, and
  `close_control_gestures` covers plugin unload, rescan, engine stop and reload. `GestureEnd` is a
  `must_not_be_lost` payload with reserved room in the merged buffer — leaving one open is the
  failure this codebase already takes seriously.
- **Reload is transactional.** A malformed user file leaves the last working map active. Falling
  back to defaults would rearrange somebody's controller while they were editing the file that
  controls it.

The collection standard is [`docs/MXM_CONTROL_MAP.md`](https://github.com/mxm-audio/mxm-kit/blob/main/docs/MXM_CONTROL_MAP.md), which is
normative. Per-instrument maps ship beside their bundles, not here. The current fourteen-page
standard ends with the Dynamics page added for `mxm-fx-curve`; its one occupied role maps that
effect's sole automatable parameter, Mix.

## Rendering and plugin windows

### The player renders through `wgpu`, and that is not a preference

`Cargo.toml` selects `eframe`'s **`wgpu`** feature, and `main.rs` restricts `Backends` to D3D12,
Vulkan and Metal. Both halves matter:

- **A plugin editor renders through OpenGL in this same process and thread.** Two OpenGL painters
  share one global current-context and corrupt each other — the editor renders white, the player
  fills with garbage. `docs/known-issues.md` has the diagnosis to the line.
- **wgpu has its own GL backend**, so asking for wgpu is not by itself asking for something that is
  not OpenGL. Without the `Backends` restriction a fallback would restore the conflict with **no
  visible sign that anything had changed**, which is why `main.rs` also logs the adapter it actually
  selected. If that line ever says `Gl`, this contract has been broken.

  This is not hypothetical, and it is checkable in one command:

  ```bash
  cargo tree -p mxm-player -i glow      # glow <- wgpu-hal <- wgpu <- egui-wgpu <- eframe
  ```

  **`glow` is still linked into the player**, because `wgpu-hal` compiles its GL backend in
  regardless. Linking it creates no context and is harmless; what keeps it unused is the runtime
  restriction alone. Removing that line would not produce a build error — it would produce an
  intermittent rendering bug.
- **No usable adapter is fatal**, where the old backend would have limped on software OpenGL. That
  is an accepted cost of not sharing an API with plugin editors, and the failure says so in those
  terms rather than looking like a crash.

**Do not move a plugin to wgpu to solve a host problem.** The cost belongs in one binary, not in
every `.clap` the collection ships.

### Servicing continues while unfocused, and that is what the renderer swap bought

`logic()` runs only when eframe draws, and eframe draws reactively — so an unfocused player would
stop polling the engine, stalling the sequencer and dropping MIDI presses bound for the selected
step. `logic` therefore requests a repaint while **the transport is playing or an editor is
visible**.

**But the rate is a measured floor, not a taste, and it is the whole subtlety here.** A plugin
editor is a window *on this thread*. Ask for frames at or below the display's frame interval and the
event loop never idles, the editor's messages are never dispatched, and **it renders white** —
measured at 16 and 20 ms, correct from 25 ms up, on a 60 Hz display with the player on Vulkan and no
OpenGL anywhere in it. So:

- **50 ms while an editor is visible**, 2.5× that knee. The margin is deliberate: the knee tracks the
  *display's* refresh rate, so it moves between machines and must not be tuned to one desk.
  `MXM_SERVICE_MS` overrides it for diagnosis.
- **The ordinary frame rate when no editor is open**, since nothing then shares the thread.

**This was long attributed to the OpenGL contention and it is not that.** `docs/known-issues.md` has
both entries and the measurements that separate them. Repainting continuously does make the white
editor constant, which is why the two were conflated — but it does so on Vulkan too.

**`visible`, not `open`.** `EditorState` keeps them apart deliberately — hiding an editor does not
destroy it — so keying on `open` would repaint for ever after a hide. Idle with nothing playing and
nothing shown still sleeps.

**No headless test can see this.** It needs a real editor window and the evidence is what the window
looks like, so `MXM_AUTO_EDITOR=<clap-id>` exists to open one unattended for a screenshot script.
Capture the **editor window**, not the screen: the player overlaps it, and measuring the overlap
reports a healthy result for a completely blank editor.

**A frame is always asked for; only the interval changes.** Making the request itself conditional —
*playing or editor visible* — brought the failure straight back in a form that looked like a MIDI
bug: a stopped player with no editor asked for no frames, `service()` never ran, and a MIDI keyboard
could not enter a note into a selected step. **Only MIDI broke, and that is the diagnostic.** The
on-screen and computer keyboards write the step from inside the input event that woke the frame, so
they never need a frame they did not cause; MIDI arrives on the backend's own thread and does.
`an_idle_player_keeps_asking_for_frames_so_midi_can_still_reach_a_step` guards it by asserting
`has_requested_repaint`, because `a_midi_keyboard_enters_notes_into_a_selected_step` drives frames
itself and passes either way.

Idle is `IDLE_SERVICE_INTERVAL`, 100 ms. Ten turns a second is imperceptible for entering a note,
and the editor's floor does not apply when no editor is shown.

### Floating resize stays inside the plugin's window

`host.gui.request_resize` is for a parent's client area; the player's floating editors have none.
The vendored nice-plug callback therefore accepts a floating resize locally rather than waking the
player and having `service_editor` echo it back. Embedded DAW editors still negotiate with their
host. Do not compensate for this traffic by lowering `MXM_SERVICE_MS`: the unnecessary wake itself
bypasses that cadence. The host's generic queued-request handling is unchanged.

The collection's `editor_resize` (newdawn-workspace's `collection-tests/editor_resize.rs`, this repository's
until the split into one repository per product) is an ignored Windows desktop regression against **all twenty shipped
editors**, with no production settings or audio device: 24 native resizes per editor, repeated
after close/reopen. A manifest-completeness oracle exposed mxm-bucket-delay's omission from the
first nine-entry run. The last thirteen-entry Windows run (2026-09-11; mixed already-staged
profiles, with a fresh release sampler) reported `0/24` host wakes on both opens for every then-listed
bundle, including mxm-creative-sampler; it predates mxm-para-07, mxm-fx-convolution and mxm-classic-verb and is not
evidence for the current twenty-entry inventory, which also adds mxm-fx-curve, mxm-fx-delay, mxm-drum-machine and mxm-model-drums. Profile-specific all-debug and all-release staging
remain the formal gate. It checks accepted sizes and absence of host round-trips, not paint FPS or
manual drag smoothness. Bundle each profile before running it; missing or unloadable bundles fail.

### Plugin GUIs: floating, and three invariants

`src/engine/editor.rs` is the whole lifecycle. Each of these fails **silently** if broken, which is
why they are here and not only in the code:

- **Exactly one `destroy` per successful `create`, on every path** — including after
  `closed(was_destroyed = true)`, which *requires* it as an acknowledgement rather than forbidding
  it. nice-plug refuses a second `create` while its editor handle is still set, so a missed
  acknowledgement means the editor never opens again for the life of that instance. `close_editor`
  is the only place `destroy` is called; keep it that way.
- **The computer keyboard does not play while the editor has focus, and this is unfixed.**
  `handle_input` reads key events from the *player's* window, so once the editor takes focus the
  QWERTY note keys go to the plugin and sound nothing. MIDI is unaffected: a MIDI note is stamped in
  its own callback and queued straight for the audio thread, never passing through `handle_input`.
  `show_editor_now` takes focus back when the editor *opens*, which covers only that moment — click
  the editor to turn a knob and the keys are dead again. Anything held is released rather than stuck,
  via `CleanupSource` on focus loss. **Fixing it means a thread-scoped keyboard hook in the host**
  (not a global one), Windows-only in the first instance, with the other arms declared `Unsupported`
  as `src/ownership.rs` already does — deliberately not done, because it would also play notes into a
  plugin's own text fields, whose focus state the host cannot see.
- **Ownership never gates the editor.** If the player cannot make its window the owner, the editor
  still opens, and the fact is **shown** — an editor that sinks behind the player looks like a bug
  unless the user is told it is a known gap.
- **Every `clap.gui` call happens on the main thread.** The plugin's callbacks arrive on *any*
  thread — clack permits `request_resize` asynchronously — so `HostShared` records them in atomics
  and `Engine::service_editor` applies them from `logic`. Never call `clap.gui` from anywhere else.
- **As many editors at once as there are plugins** (the owner, 2026-09-04, replacing the
  one-at-a-time rule this player shipped with for a day). Tweaking a synth against the effect after
  it is one job, and closing one window to see the other made the user pay for the engine's
  convenience. Nothing in CLAP needed the restriction — each instance owns its window, its
  `create`/`destroy` pair and its ownership claim — what was single was the *bookkeeping*, so
  `Engine::editors` is a `Vec<EditorState>` keyed by `EditorTarget` and every entry is one window.
  `service_editor` drains **every** plugin's flags, open or not: a flag set just before a window
  closed would otherwise fire the next time it opened, hiding it the instant it appeared. The
  states follow their effect through a move and leave with it on a removal.
- **The app bar’s editor button belongs only to the source** and sits beside its name. Each effect
  strip owns its own editor toggle; no aggregate engine editor state may drive either.

`src/ownership.rs` is a **workaround** and says so. It rests on properties CLAP does not promise, so
it identifies the editor window positively — baseview's `Baseview-<uuid>` window class — and claims
nothing when it cannot. Windows implements it; macOS and Linux implement the arm as an explicit
`Unsupported`. The real fix is CLAP's `set_transient`, which needs three unvendored crates and
belongs upstream.

## Verification detail

### Per change: run what the change can reach

**The owner's rule (2026-09-17).** A change runs the unit tests of the module it touched and the
integration tests for that feature, and nothing else. A sequencer change does not re-test the synths,
and a synth change does not re-test the sequencer. Everything after this table is the gate for
merging into main, or for an explicit request: the twenty bundles, the editor sweeps, a bare
`cargo test -p mxm-player` and clippy with `--all-targets`.

**Name as few integration files as the change reaches.** Each file in `tests/` is its own binary
that links the whole app, so every file named costs a link, and that dominates the time. Filter
inside a file by name, and **check the reported count**: a filter that matches nothing passes with
zero tests.

**While iterating, lint with `cargo clippy -p mxm-player --lib`**, and add `--all-targets` once a
test file changed. **A comment-only or format-only edit gets `rustfmt --check` on the file and no
test run.**

### The gate: merging into main, or on request

**The behaviour tests' measurements come from `mxm-measure`**, a dev-dependency: peak, RMS, channel
de-interleave, note-to-hertz and a component's amplitude, which the six behaviour files each used to
carry their own copy of. One figure moved with that migration and it is a **correction** — every
local `magnitude_at` computed `|X|/N`, which is half a component's amplitude, while the shared probe
reports the amplitude. Every assertion here is a comparison, so none depended on the absolute value,
but a magnitude quoted from an older run of these tests is **6 dB low**.

```bash
# Plugins from other MXM repositories the tests load, at the tags test-bundles.txt pins:
# mxm-mono-01 and mxm-bucket-delay release; mxm-mono-01 and mxm-para-07 debug, for the
# explicit-release and zero-velocity input regressions.
cargo xtask fetch
cargo build -p nice-plug-output-fixture      # output allocation regression
```

The collection's native resize regression, `editor_resize`, needs every product's bundle in one
profile, so it lives in newdawn-workspace with the other collection tests (`collection-tests/`).

```bash
cargo xtask fixtures --release          # -> target/fixtures/mxm-fixtures.clap
cargo test -p mxm-player
cargo clippy -p mxm-player --all-targets
cargo run -p mxm-player --release
```

Tests **skip with an explanatory message** when an artifact is missing, because a missing build is
not a hosting bug. Once an artifact exists, a load failure is a test failure rather than a skip. A
skipped test is not a passing one — check the output.

### The test matrix

The matrix, and what each layer exists to prove:

**Three layers, each answering what the one below cannot.** Layer 1 bypasses the app entirely and
always did; layers 2 and 3 exist because it does.

| Layer | File | Proves |
|---|---|---|
| 1 | `plugins/mxm-mono-01/host-tests/tests/hosting.rs` | The hosting API offline, against a direct `mxm-mono-01-dsp` render, **bit-exact** as in-memory `f32` |
| 1 | `tests/p0_envelope.rs` | Each out-of-envelope fixture is refused with the *right reason* |
| 1 | `tests/p1_engine.rs` | The output-only engine end to end, against the fake backend |
| 1 | `tests/p2_playing.rs` | The event path through a real plugin, callback on the test thread |
| 1 | `tests/p3_testable.rs` | Parameters with gestures, state round-trip, MIDI-out picker |
| 1 | `tests/verification.rs` | Properties needing a misbehaving plugin, or a look at the audio thread |
| 1 | `tests/wedged_subprocess.rs` | The terminal wedged path — must be a subprocess, since the fixture never returns from `process()` |
| 1 | `tests/plugin_robustness.rs` | The pinned nice-plug robustness defects, fixed in [`vendor/nice-plug`](https://github.com/mxm-audio/nice-plug/blob/main/PATCHES.md) and still unfixed upstream, including guarded input/output queue floods; the input cases establish audibility before a 2,000-event hostile callback ends in either explicit release or mxm-para-07's zero-velocity NoteOn release. These stop a careless refresh from reintroducing them |
| 2 | `tests/t0_seams.rs` | The app builds and runs headlessly, in a sandbox, touching nothing outside it |
| 2 | `tests/t1_oracles.rs` | The two oracles the UI layer rests on, each proven by falsification |
| 2 | `tests/t2_regressions.rs` | One test per defect a human found by looking at the screen |
| 2 | `tests/t7_editor.rs` | Floating-GUI advertisement and independent editor state headlessly, including mxm-fx-curve through the effect path; ignored native-window cases open, service, close and reopen covered editors, including mxm-mono-08 and mxm-fx-curve |
| 2 | newdawn-workspace's `collection-tests/editor_resize.rs` | Every `bundler.toml` declaration appears exactly once across the editor and explicit-headless inventories; all twenty shipped floating editors, including mxm-fx-convolution, mxm-fx-delay, mxm-classic-verb, mxm-creative-sampler, mxm-drum-machine, mxm-para-07 and mxm-fx-curve, open, traverse 24 native Windows sizes without host resize round-trips, close and reopen. Run explicitly against complete debug and release editor inventories. The sampler's separate OS-file-drop gate is not implied by resizing |
| 3 | `tests/t3_session.rs` | Deterministic sessions: byte-identical audio, and `load` while streaming completing promptly |
| 3 | `plugins/mxm-mono-01/host-tests/tests/golden_audio.rs` | A fixed score still sounds the same **through the real app path** — mxm-mono-01 |
| 3 | `plugins/mxm-poly-06/host-tests/tests/golden_audio.rs` | The same for mxm-poly-06, shipped with the instrument rather than after it |
| 3 | `plugins/mxm-mono-08/host-tests/tests/golden_audio.rs` | The same for mxm-mono-08: sounding-touch ownership, sequencer routing and its built-in spring |
| 3 | `plugins/mxm-mono-pr1/host-tests/tests/golden_audio.rs` | The same for mxm-mono-pr1: additive oscillators, sync/PWM, modulation, articulation, performance ownership and release, with controlled listen-before-update regeneration. The ordered-edge sync repair's independently reproduced `8e1c04ab75ef28f5` is provisionally pinned and its focused discriminator passes. No human listening is claimed, and final reference listening remains a manual release gate |
| 3 | `plugins/mxm-para-07/host-tests/tests/golden_audio.rs` | The same for mxm-para-07: two-pitch assignment, middle-key gate life, collapse and shared release as one fixed score |
| 3 | `plugins/mxm-mono-02/host-tests/tests/golden_audio.rs` | The same for mxm-mono-02: both oscillators and the sub, the LFO-swept pulse width, the envelope on a resonant cutoff with key tracking, vibrato, the auto bend, portamento and the keyboard block's low-note priority. Pinned before its routing conversion and provisionally repinned after it, for the owner's listening pass to confirm |
| 3 | `plugins/mxm-mono-03/host-tests/tests/golden_audio.rs` | The same for mxm-mono-03: a resonant squelch, two accented notes whose sweep climbs, a tie with Slide on, and the tail. Pinned before its routing conversion and provisionally repinned after it at `555a7888da50f235`, for the owner's listening pass to confirm |
| 3 | `plugins/mxm-mono-01/host-tests/tests/behaviour.rs` | Each mxm-mono-01 control does what its label says, on rendered audio |
| 3 | `plugins/mxm-poly-06/host-tests/tests/behaviour.rs` | mxm-poly-06 on rendered audio: six pitches, a seventh steals, the chorus makes the stereo, the boost boosts, exact silence at rest and after the tail, and its routing through the real callback — the envelope route opening a closed filter over a chord, and the LFO routed to amplitude making a tremolo |
| 3 | `plugins/mxm-mono-00/host-tests/tests/behaviour.rs` | mxm-mono-00 on rendered audio: a note at its pitch, a host-written cutoff, a routing pair re-patched as a parameter with a second source **summed** onto the same input, a self-running patch sounding with no key down (the `KeepAlive` path), the reverb tail ending in exact silence |
| 3 | `plugins/mxm-mono-02/host-tests/tests/behaviour.rs` | mxm-mono-02 on rendered audio: a note at its pitch, a host-written cutoff, the lower key winning and the higher returning on its release (the keyboard block), HOLD keeping the voice sounding with no key down (the `KeepAlive` path), the envelope mode ending it in exact silence, the routing's headline gesture through the real callback — raising `Pulse width from LFO to width` sweeps the pulse — and a CLAP state naming the follower routes retired with the external input (2026-09-26) still loading everything else |
| 3 | `plugins/mxm-mono-03/host-tests/tests/behaviour.rs` | mxm-mono-03 on rendered audio: exact silence at rest, a note at its pitch, a host-written cutoff darkening it, the per-step accent button making a note louder, Slide gliding into the next note, a release ending in exact silence, and its routing through the real callback — the Env Mod route opening a closed filter, and the mod wheel routed into the resonance changing the sound only once it moves |
| 3 | `plugins/mxm-mono-08/host-tests/tests/behaviour.rs` | mxm-mono-08 through ordinary discovery: all 370 parameters and its data-shipped control map, keyed/latest-touch pitch, host edits and a same-instance CLAP-state round trip, sequencer modulation, exact idle/release silence, the finite spring tail, and an isolated-child debug-bundle proof that editor Once crosses the real CLAP wake boundary exactly once without runtime environment mutation |
| 3 | `plugins/mxm-mono-08/host-tests/tests/robustness.rs` | mxm-mono-08 through the direct host at hostile sample rates and callback sizes, including multiple event offsets and debug allocation assertions |
| 3 | `plugins/mxm-mono-pr1/host-tests/tests/behaviour.rs` | mxm-mono-pr1 through ordinary discovery plus direct configuration selection: its data-shipped map and 52 parameters, exact idle/release silence, keyed pitch, host cutoff edits, Normal and Retrigger ownership, Drone wake, both input-free layouts with noise and the LFO-clocked Repeat heard through them and bit-identical stereo duplication (the external inputs were retired 2026-09-26), a same-instance CLAP-state round trip, floating-GUI advertisement, and an ignored deliberately invoked native open/close/reopen proof |
| 3 | `plugins/mxm-mono-pr1/host-tests/tests/robustness.rs` | mxm-mono-pr1 through the direct host in debug and release: 1–8192-frame callbacks at 1 kHz–768 kHz, multiple event offsets, a dense same-callback burst ending in panic, finite/bounded output and debug process-allocation assertions |
| 3 | `plugins/mxm-creative-sampler/host-tests/tests/behaviour.rs` | mxm-creative-sampler through ordinary discovery and CLAP state with no linked product code: embedded audio sounds in a fresh host after its source WAV is deleted, carries no path, round-trips canonically, malformed embedded audio preserves the working sample and patch, the init patch and all three readers sound through the bundle, and Mono/glide/the filter envelope survive the wrapper. **The last is the only host-level proof of the sampler's routing system** — `Cutoff from Envelope amount` is a route pair now, not a fixed knob, so it exercises the per-sample amount path through a real CLAP boundary rather than a unit harness |
| 3 | `plugins/mxm-model-drums/host-tests/tests/behaviour.rs` | mxm-model-drums through ordinary discovery: stereo compatibility selected and its 650 parameters, two Kit notes each playing the Ringing kick finite and under full scale, a turned Tune, Decay and Body each changing the next hit, and a general control (`Slot 1` / `Control 1`) round-tripping through CLAP state |
| 3 | `plugins/mxm-drum-machine/host-tests/tests/behaviour.rs` | mxm-drum-machine through ordinary discovery: the generic first-compatible policy skips its full 17-port configuration and selects configuration 1, stereo compatibility; the fixed pre-D7 non-default state restores with Output=`L+R` and MIDI channel=`Kit`, retains old sound parameters and reproduces the current main render bit-for-bit (the fixture README records each deliberate re-capture) |
| 3 | `plugins/mxm-para-07/host-tests/tests/behaviour.rs` | mxm-para-07 discovered as an ordinary bundle: both exact audio configurations and no third advertised, a session naming the ids retired with the external input (2026-09-26) restoring bit-identically, mono/bit-identical dual-mono equality and an empty attached output-event sink; dual-pitch assignment and middle-key gate life, event ordering, host cutoff, trigger automation/restoration, HOLD/S&H activity, exact tail silence, current plus frozen-old-player map/id compatibility, and all fifty complete factory states rendering audibly with deterministic CLAP state bytes |
| 3 | `tests/t5_control_map.rs` | One controller across the collection — and above all that **adding an instrument is adding a file**, never a code change |
| 3 | `tests/t6_sequencer.rs` | The sequencer through a real synth: step intervals measured from the audio, rests that must not hang it, transport that must silence, the panel a person actually touches, and the export — including that it carries **the current patch** rather than the plugin's defaults |
| 1 (model only) | `tests/t13_effect_locks.rs` | `LockSet`/`LockData` identity, clearing and serializer/decoder properties; complemented by the real app/plugin regressions below |
| 1–3 | `tests/review_player_regressions.rs` | Regressions for settings/startup/source-switch identity, later bars, unresolved lifecycle, actual chain edits, CLI guards/baselines, sleeping/bypassed plugin readback, effect callbacks, Stop saturation and automation stress. Requires built mono-01/delay bundles and quarantined fixtures; `MXM_PLAYER_TEST_BUNDLES` can select isolated product bundles |
| 3 | `plugins/mxm-bucket-delay/host-tests/tests/effect_chain.rs` | mxm-bucket-delay through the real chain: the delay is reached and its repeats arrive a delay time later, **off returns the dry to the bit**, the graph reaches exact silence, and an effect switched back on after a gap starts from silence |
| 3 | `plugins/mxm-shimmer/host-tests/tests/effect_chain.rs` | mxm-shimmer through ordinary bundle discovery and the real effect chain: it is audible, outlives the source, leaks nothing before input, and player bypass restores the dry render to the bit |
| 3 | `plugins/mxm-classic-verb/host-tests/tests/effect_chain.rs` | mxm-classic-verb through the real effect chain: audible stereo processing, a tail beyond the source, no pre-input leakage and bit-exact Player bypass |
| 3 | `plugins/mxm-fx-delay/host-tests/tests/effect_chain.rs` | mxm-fx-delay through ordinary discovery and the real effect chain: effect classification, data-shipped map, finite audible tail, no pre-input leakage and bit-exact Player bypass |
| 3 | `plugins/mxm-fx-convolution/host-tests/tests/behaviour.rs` | mxm-fx-convolution without linked product code: ordinary effect discovery and data-shipped map, nine stable parameters/defaults, complete current and legacy-three-id path-free response state with neutral model/Feedback migration, successful real-state Size normalization with exact readback, the unchanged Revision 6 zero-Feedback digest, malformed-state atomicity, active high-rate refusal rollback with a bit-identical live tail, audible finite rendering, exact idle and bit-exact Player bypass |
| 3 | `plugins/mxm-fx-convolution/host-tests/tests/robustness.rs` | mxm-fx-convolution through a direct real-bundle host: both layouts and dry/wet routing, no output events, sample-offset legacy/wet-post automation, finite tails, hostile rates/spans and debug allocation safety; a project-authored one-tap response additionally proves certified sub-unity decay, automation into `KeepAlive` plus real `clap_host_tail.changed`, and exact stop through Feedback Off, Mix-zero park/wake and reset |
| 3 | `tests/t11_fx_chain.rs` | The effect chain on the reference effect fixture, its output computed from the dry render and compared **to the bit**: one effect, two in series, a mono effect in a stereo chain, off, back on clean after a gap, a panic, the export through it, and a session run twice |

### The three assertion styles, and which can see what

Structural assertions cannot see the painted keyboard.

| Style | Reads | Blind to |
|---|---|---|
| **Structural** | the AccessKit tree, `get_by_label` | anything drawn with bare `painter` calls |
| **State** | `PlayerApp::state()` → [`PlayerState`](src/state.rs) | what was actually drawn |
| **Paint output** | `AppHarness::painted_rects`, from `Harness::output().shapes` | text layout, perceived colour |

`draw_keys` allocates **one** interaction region and paints every key directly, so the keyboard has
no per-key AccessKit node. Only paint output can see whether keys were drawn at all.

**Image snapshots are deliberately not used.** They are brittle across GPU, driver, font and DPI,
and paint-output assertions cover the case that motivated them without a GPU.

### An oracle that cannot fail is not an oracle

Every oracle in `t1_oracles.rs` was proven by **falsification**: the fix was reverted, the test was
watched to go red, and the fix restored. Do the same for any new oracle. The falsifications used:

- delete the black-key draw loop → the black-key tests fail;
- restore `available.x / count` key widths → the width test fails;
- remove the parameter snapshot write-back → the snap-back test fails;
- remove `service_commands` from the session backend → both "completes promptly" tests fail, after
  hanging for exactly `WEDGE_TIMEOUT`;
- reduce `Session::settle` to a single `service` → nothing fails on an idle machine, and that is
  the point. It was measured instead: printing a line each time the wait engages, one full-suite
  run engaged it ten times, which is ten blocks the single call would have rendered stale.

### Driving it yourself

`apps/mxm-player-harness/src/app_harness.rs` hosts the real `PlayerApp` under `egui_kittest`;
[`src/session.rs`](src/session.rs) drives the whole app headlessly and reproducibly and can write
the rendered audio next to a JSON state dump (`Session::write_artifacts`). For an investigation,
write a throwaway test against `Session` and read the dump — do not take a screenshot.

`apps/mxm-player-harness/src/harness.rs` owns an `AudioWorker` directly with no engine and no device, which is what
makes allocation, exact event delivery and press accounting assertable. Its explicit-configuration
constructor varies sample rate and maximum callback size without adding any product-specific player
production path; bundle helpers and tests may name products.

The three-second wedge timer covers an outstanding Stop, not an ordinary-process watchdog. Capture
the stop initiator, callback phase and thread stacks before attributing a timeout to plugin code;
`plans/handover-2026-09-06-player-wedge.md` (`plans/handover-2026-09-06-player-wedge.md` in the private archive) owns
the unresolved incident.

### Auditioning a plugin here, with no window and no sound card

**A plugin does not need a person at a keyboard to be auditioned through the player**, and a plugin
author should not wait for one. `Harness::with_fx(source_bundle, source_id, producers, &[(bundle,
id)])` builds the real chain — source, then the effects in signal order — and `render(frames)`
returns interleaved audio from the same worker a device would drive. Flip `HarnessFx::bypassed`
exactly as the strip does. `Harness::new` is the same thing for an instrument with no chain.

That is the whole of what *auditioned through the player* means as a gate, and it is a test rather
than a session: `tests/t11_fx_chain.rs` and `plugins/mxm-bucket-delay/host-tests/tests/effect_chain.rs` are the two worked examples.
What it does **not** cover is what a person sees and hears — the editor by eye, and a real host —
which is why those stay separate gates.

For anything about the app's own bookkeeping rather than its audio, use `Session` (above) or
`apps/mxm-player-harness/src/app_harness.rs`; for the *live* player, the CLI socket in [`src/cli.rs`](src/cli.rs)
answers a running instance with the same state the window draws.

**`Session::advance_blocks` services until the edits have travelled, not once.** The app publishes
the sequencer at most one state ahead of the worker, so a frame that finds the previous one still
unacknowledged publishes nothing and leaves the edit to the next frame. A window has a next frame
sixteen milliseconds later; a session's next event is a rendered *block*, so a single `service`
could grant a block that still had the old transport, loop window and pattern. That is what made
`loop_bar_keeps_the_playhead_inside_the_selected_bar` fail under load — it read step 0, stopped.
Instrumented, the wait engages about ten times per full-suite run, so anything that edits and then
renders was exposed. Do not reduce it back to one `service` call.

`AppHarness::run` draws a **fixed** number of frames rather than `Harness::run`'s
run-until-settled: scroll areas animate, so "settled" never arrives and `run` panics on its step
cap. Fixed counts are also the more deterministic choice.

Two subprocess tests deliberately race against a hanging plugin. `Stop` is handled at the top of
the callback, so arriving before the first callback has entered the plugin is a *correct* clean
stop — just not the state under test. Those children **retry** rather than asserting on the first
attempt, and fail loudly if they never reach the state at all, rather than exiting 0 on a test that
checked nothing.

Manual steps, recorded in the plan rather than pretended to be automated: the real CPAL backend, and
playing it with a MIDI keyboard.
