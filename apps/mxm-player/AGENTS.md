# AGENTS.md — apps/mxm-player

Parent: [`../../AGENTS.md`](../../AGENTS.md)

# Purpose

A CLAP **host** for testing and playing the MXM Synth Collection. It loads bundles at runtime, so it works
for every future MXM plugin without rebuilding and tests the `.clap` we actually ship rather than a
standalone wrapper around it.

The reasoning, history and measurements behind each rule are in [NOTES.md](NOTES.md): read them first.

# Ownership

- **Its own product**: own version, MSRV and `README.md`, released independently of any plugin's cycle,
  sharing the design system and mxm-kit's `mxm-ui` (`crates/ui`) without being coupled to a plugin release.
- Owns `src/`, `tests/`, `Cargo.toml`, `README.md`; not the fixtures it loads ([`tests/clap-fixtures/AGENTS.md`](../../tests/clap-fixtures/AGENTS.md)).
- Tests drive it through [`apps/mxm-player-harness`](../mxm-player-harness/AGENTS.md). **A plugin's tests
  through the player belong to that plugin**, in its repository's `plugins/<plugin>/host-tests`; this
  player's own use mxm-mono-01 only as the reference instrument and the fixture plugins for hostile cases.
- Design brief: `docs/briefs/mxm-player.md`. Current work belongs to its plan or handover in `plans/` (private
  archive); durable behavior stays here. [NOTES.md § Ownership](NOTES.md#ownership-in-full)

# Local Contracts

## Scope and the effect chain

- **v1 scope is decided**: no audio input, no embedded plugin GUI (editors float in a window the plugin owns,
  `src/engine/editor.rs`), one source and one serial chain (source → effects → device; no bus, send or
  parallel path). Never add a disabled control implying otherwise. [NOTES.md § Scope](NOTES.md#scope-deliberately)
- **`src/envelope.rs` is the v1 CLAP envelope**: anything outside it is refused with the reason shown, in
  every picker; a refusal without a reason is a bug. [NOTES.md § Envelope](NOTES.md#the-compatibility-envelope-is-enforceable-and-refusals-carry-reasons)
- **The chain** (`src/engine/fx.rs`): an effect has one main input and one main output. Add, remove and move
  stop the worker, edit the slots and restart, the source untouched; at most `MAX_FX`; effects activate
  after the source and deactivate before it on every path. **Off is uncalled**: input passes to the bit;
  back on is CLAP `reset` first; edits still arrive through `params.flush` (`FxStage::flush_params`, or
  `FxSlot::flush_params_while_inactive` with no stream). A tail countdown restarts on every chunk carrying
  input; an erroring stage bypasses itself for good. Verbs `fx add|remove|move|on|off|editor`, one-based;
  `fx_chain` is restored before any source loads. [NOTES.md § Effect chain](NOTES.md#the-effect-chain-serial-reorderable-and-off-means-uncalled)
- **Effect locks keep identity**: `LockKey` is plugin + parameter; `FxId` is never reused; removal deletes
  an effect's locks before publishing; a missing live effect is a save error, never Source. Authoring goes
  through `write_step_lock`. [NOTES.md § Effect locks](NOTES.md#effect-automation-identities-survive-chain-edits)

## Threads, realtime and the engine

- **Never call egui from a plugin or audio thread** (store atomics; `src/notifier.rs` wakes the GUI). `rtrb`
  is SPSC: one queue per producer. Stamp each event in its own callback from `src/clock.rs`. `midir::send()`
  never on the audio thread. [NOTES.md § Threading](NOTES.md#threading)
- **The audio callback allocates nothing**: buffers sized at activation; the output sink fails rather than
  grows; `MergedInput::sort_by_arrival` stays `sort_unstable_by_key` with a `sequence` tie-break, never a
  stable sort; allocation tests include a dense buffer. [NOTES.md § Allocation](NOTES.md#the-audio-callback-allocates-nothing)
- **`Clock` is a closed enum**, every read wait-free. A test never runs `PlayerConfig::production()`: use
  `PlayerConfig::sandboxed`, with explicit search paths. [NOTES.md § Clock](NOTES.md#one-clock-and-it-is-read-on-the-audio-thread) · [§ Config](NOTES.md#the-app-is-configured-never-hard-wired)
- **`PlayerApp::service` is the one servicing implementation**, window and headless; never a second copy.
  The session backend services commands whenever not rendering and never drains input queues there.
  [NOTES.md § Servicing](NOTES.md#servicing-is-one-implementation-called-from-two-places) · [§ Advancement](NOTES.md#audio-advancement-and-command-servicing-are-separate)
- **A wedged engine never reports a clean stop**: `Engine::stop_now` checks wedged on entry and after
  polling; a Stop that did not fit is retried, never `Ok`. [NOTES.md § Wedged](NOTES.md#a-wedged-engine-never-reports-a-clean-stop)
- **A dead stream is neither wedged nor stopped**: errors go through `audio::describe`; `Engine::try_reconnect`
  runs each frame from `service` and never gives up; a reconnect republishes the CC mask and sequencer
  state; Play refuses while dead or wedged; a start failing after activation deactivates. [NOTES.md § Dead stream](NOTES.md#a-dead-stream-is-not-a-wedged-plugin)
- **Discovery runs plugin code**: write the sentinel before entering an uncached bundle, clear it on success.
  Settings are written eagerly, replaced atomically. **Fault isolation is partial**; say so plainly. [NOTES.md § Discovery](NOTES.md#discovery-executes-arbitrary-code) · [§ Settings](NOTES.md#settings-are-written-eagerly-and-replaced-atomically) · [§ Faults](NOTES.md#fault-isolation-is-partial-and-says-so)

## Parameters, keyboards and MIDI input

- **Edits are asynchronous**: write the value into the snapshot as well as sending it, and requery on the
  next frame, never the same one. [NOTES.md § Async edits](NOTES.md#parameter-edits-are-asynchronous-and-the-panel-must-not-forget-that)
- **Tabs come from CLAP's `module` path**: none or one group shows no strip; ungrouped is **Main** and opens
  first; tabs filter painting only, never persisted. **The keyboard is sized in keys**; black keys are drawn
  after the whites and hit-tested before them. [NOTES.md § Tabs](NOTES.md#the-parameter-panel-tabs-by-the-plugins-own-groups) · [§ Keyboard](NOTES.md#the-keyboard-is-sized-in-keys-not-in-fractions-of-the-window)
- **Every input path counts**: ask `PlayerApp::sounding`, not `held`; a closing port clears MIDI's set;
  `RefusedInput` carries the real reason. [NOTES.md § Monitors](NOTES.md#the-on-screen-keyboard-monitors-every-input-path-and-held-is-only-one-of-them) · [§ Refused](NOTES.md#a-refused-midi-input-carries-the-reason-it-was-refused)
- **Whatever `note_on` edits, `receive_midi_presses` edits too**, without re-sounding. Restore a setting into
  the engine, not only the panel (`with_config`). [NOTES.md § MIDI parity](NOTES.md#whatever-the-note-keys-do-a-midi-keyboard-does-too) · [§ Restoring](NOTES.md#restoring-a-setting-means-restoring-it-into-the-engine-not-only-into-the-panel)
- **Typing plays nothing**: no key acts while a text field has focus, and held notes are released when one
  takes it. Space is the transport. [NOTES.md § Typing](NOTES.md#the-computer-keyboard-is-not-an-instrument-while-you-are-typing)

## The interface

- **Status bar**: `load` always, other meters only when they have something to say, all in `dump`; the
  plugin picker lives there. Sibling `ScrollArea`s need distinct `id_salt`s; the browser column scrolls as a
  whole. [NOTES.md § Status](NOTES.md#a-status-reading-appears-when-it-has-something-to-say) · [§ Picker](NOTES.md#the-plugin-picker-is-a-menu-of-what-can-be-loaded-and-it-lives-in-the-status-bar) · [§ ScrollAreas](NOTES.md#sibling-scrollareas-need-distinct-ids) · [§ Browser](NOTES.md#the-browser-column-fits-the-panel-it-is-given)
- **Style comes from `mxm-ui`**, at startup and every frame; `src/ui/adapter.rs` is the only translation
  point, so never style outside it; every colour from `adapter::tokens_for`. Theme: `theme <light|dark|system>`,
  `settings.theme`; `MXM_PLAYER_THEME` overrides one run, never written back. [NOTES.md § Style](NOTES.md#the-player-has-a-style-and-it-comes-from-mxm-ui) · [§ Adapter](NOTES.md#styling-comes-from-mxm-ui-and-srcuiadapterrs-is-the-only-translation-point) · [§ Theme](NOTES.md#the-theme-is-the-players-own-choice-and-the-desktop-is-only-the-default)
- **Play always rewinds**; Stop rewinds; Space calls the same function. Transport-row controls are fixed
  size; assert row stability on a neighbour. [NOTES.md § Play](NOTES.md#play-always-rewinds-and-there-is-one-button-for-it) · [§ Row](NOTES.md#nothing-in-the-transport-row-changes-size-when-you-touch-it)
- **A tie joins step buttons**; `step_description` names tie and slide. AccessKit labels are interface:
  renaming one is a test-visible change. [NOTES.md § Step buttons](NOTES.md#a-tie-joins-the-buttons-the-bar-says-the-step-holds-notes) · [§ AccessKit](NOTES.md#accesskit-labels-are-interface)
- **Collapsing**: only the parameter and settings panels; the layout changes only when the user changes it;
  remember the expanded, non-maximised size only. [NOTES.md § Collapsing](NOTES.md#collapsing-and-the-one-rule-about-movement)

## The sequencer

- **A step stores notes + `tied`**: rest, note, hold or slide, all authorable. Ties and notes are independent:
  no guard, no repair, no edit rewrites a step it did not name; never reintroduce a repair walk.
  [NOTES.md § Step model](NOTES.md#a-step-is-a-rest-a-note-a-hold-or-a-slide--and-it-stores-one-boolean) · [§ Independent](NOTES.md#ties-and-notes-are-independent-and-no-edit-rewrites-a-step-you-did-not-name)
- **One suppression rule**, `Pattern::held_past_gate`, for runtime, render and MIDI writer; only
  `run_ends_at` truncates. A slide sounds, then releases, at one frame. [NOTES.md § Legato](NOTES.md#a-tie-with-notes-sounds-before-it-releases-and-the-press-table-fights-it)
- **Input lands on the step given** (`toggle_step_note`, every path). The second click ties (`tie_action`
  decides; the hint line reads it). [NOTES.md § Pitch](NOTES.md#a-pitch-lands-on-the-step-itself-and-run_start-is-symmetric-with-run_ends_at) · [§ Click](NOTES.md#clicking-the-selected-step-ties-it-to-the-note-in-front-of-it) · [§ MIDI loss](NOTES.md#what-midi-cannot-keep-said-on-save-as-well-as-on-load)
- **Bars**: eight chips, no maximum length; bars materialise on content (`grow_to_reach`), never on
  navigation; clipboard chords are `Event::Copy/Cut/Paste`, never key presses. [NOTES.md § Bars](NOTES.md#bars-steps--bar--pattern--sequence)
- **The clock runs on the audio thread** with its own playhead; nothing counts as delivered until a plugin
  got it; `MAX_ACTIONS_PER_CHUNK` sizes every action vector. [NOTES.md § Audio-thread clock](NOTES.md#the-sequencer-runs-on-the-audio-thread--and-the-control-map-does-not)

## Step locks

- **A lock is `CLAP_EVENT_PARAM_MOD`**, an offset, never a value, for `IS_MODULATABLE` parameters only. The
  host cannot tell offset from value: test the transport by its audio, the host's bookkeeping with
  `AppHarness`, not `Session::state()`. [NOTES.md § Locks](NOTES.md#a-step-sets-parameters-as-well-as-notes-and-one-funnel-decides-that)
- **The runtime owns every offset and its zero**; the host applies none. Locks are per step, ties included,
  no inheritance; a drag's lock waits in `pending_step_edit`; a base parked at rest waits for `unpark_bases`.
  [NOTES.md § Parking](NOTES.md#the-runtime-previews-gestures-parking-and-settling) · [§ Per step](NOTES.md#step-edits-zeroes-and-locks-per-step)
- **Every edit goes `parameter_edited` → `deliver_edit`**; `parameter_edited` runs before `editing` is updated: do not reorder. [NOTES.md § Reading an edit](NOTES.md#the-sequence-patch-and-how-an-edit-is-read)
- **State reaches the audio thread by `Arc`**, handed back, never dropped or cloned there; no `LockSet::param_ids` there. [NOTES.md § By pointer](NOTES.md#the-state-reaches-the-audio-thread-by-pointer)
- **No ceiling sized from an interface or a plugin fact**; `MAX_LOCKED_PARAMS` grows only with the action
  budget. Locks carry their plugin's CLAP id (equality, no rename shim). [NOTES.md § Budgets](NOTES.md#no-maximum-length-budgets-and-storage) · [§ Across plugins](NOTES.md#locks-across-plugins-files-and-paths)

## The CLI, export and files

- **The CLI ships** (never remove it from a release build), loopback only. Every user-visible act gets a
  verb, `dump` visibility and a `COVERAGE` row in the same change (`tests/t10_cli_conformance.rs`). Input
  acts only through `PlayerApp::perform`; `lock` takes the selected-step funnel; `dump` keeps what the
  window hides. [NOTES.md § CLI](NOTES.md#the-cli-everything-a-person-can-do-has-a-verb-and-that-is-a-contract)
- **Export renders a second instance with the live state**, after pending edits settle, through every
  effect that is on, with the locks on every step; a plugin without `state` is refused; `normalise <on|off>`;
  a render is exactly the sequence. [NOTES.md § Export](NOTES.md#exporting-audio-renders-a-snapshot-and-says-what-it-cannot-promise) · [§ The file](NOTES.md#the-exported-file-is-exactly-the-sequence)
- **`.mid` is interchange, `.seq.json` fixtures**: refuse with a reason, name what is lost; run
  `tests/midi-conformance/verify.py` when the writer changes. Files are written by `mxm-audio-file`; the
  player decodes nothing. [NOTES.md § MIDI](NOTES.md#midi-is-the-interchange-format-json-is-the-fixture-format)
- **Control mapping runs on the GUI thread**; the audio thread learns a `CcMask`; `RESERVED_CCS` are never
  claimed; gestures always close; reload is transactional. [NOTES.md § Control map](NOTES.md#control-mapping-runs-on-the-gui-thread-and-the-audio-thread-learns-only-a-mask)

## Rendering and plugin windows

- **wgpu on D3D12, Vulkan or Metal only**, never OpenGL: `glow` is still linked
  (`cargo tree -p mxm-player -i glow`) and only `main.rs`'s `Backends` restriction keeps it unused, so an
  adapter logged as `Gl` breaks this. [NOTES.md § wgpu](NOTES.md#the-player-renders-through-wgpu-and-that-is-not-a-preference)
- **A frame is always requested; only the interval changes** (`EDITOR_SERVICE_INTERVAL` while an editor is
  visible). Never lower `MXM_SERVICE_MS` for resize traffic. [NOTES.md § Unfocused](NOTES.md#servicing-continues-while-unfocused-and-that-is-what-the-renderer-swap-bought) · [§ Resize](NOTES.md#floating-resize-stays-inside-the-plugins-window)
- **Editors**: one `destroy` per `create`, only in `close_editor`; every `clap.gui` call on the main thread;
  ownership never gates an editor; as many editors as plugins. [NOTES.md § Plugin GUIs](NOTES.md#plugin-guis-floating-and-three-invariants)

# Work Guidance

- Read the module doc comment before changing a module. They carry the reasoning, including designs
  that were tried and rejected; that context is the point.
- Dependencies are pinned exactly, `clack-*` at `=0.1.1`. Keep `clack-extensions` features minimal
  and note which fixture needs each one.
- `egui_kittest` and `kittest` are **dev-only** — they never reach the shipped binary — but they
  must move in lockstep with `egui`/`eframe` on every upgrade.
- MSRV 1.95 (eframe + egui).

# Verification

**Per change, run what the change can reach** (the owner, 2026-09-17), nothing else. Each `tests/` file
links the whole app: name as few as the change reaches, filter inside by name and **check the reported
count**, since a filter matching nothing passes. [NOTES.md § Per change](NOTES.md#per-change-run-what-the-change-can-reach)

| You touched | Run |
|---|---|
| `src/sequencer/random.rs` | `cargo test -p mxm-player --lib sequencer::random`. Add `--test t6_sequencer random` when `randomise` or its button changed |
| `src/sequencer/` `pattern`, `clock`, `runtime`, `locks` | `--lib sequencer::<module>`, then `--test t6_sequencer` filtered by feature: `tie`, `slide`, `lock`, `loop`, `tempo`. Its rendered-audio tests are the sequencer's own timing and articulation proofs, not synth tests |
| `src/sequencer/` `sequence`, `smf`, `export`, `wav` | `--lib sequencer::<module>`, then `--test t6_sequencer` filtered by `sequence`, `midi` or `export` |
| The sequencer panel in `src/ui/` | `--test t6_sequencer`, filtered by the feature |
| `src/cli.rs` | `--test t9_cli` and `--test t10_cli_conformance` |
| `src/control_map/` or the collection standard/product maps | `--lib control_map` and `--test t5_control_map`; its inventory sweep loads every `bundler.toml` product map against the standard |
| The effect chain | `--test t11_fx_chain` and `--test t13_effect_locks`; for one effect, its own `cargo test -p <effect>-host-tests --test effect_chain` |
| `src/host/`, `src/engine/`, `envelope.rs`, `discovery.rs` | Layer 1 below: `p0_envelope`, `p1_engine`, `p2_playing`, `p3_testable`, `verification`, and mxm-mono-01's bit-exact hosting proof `cargo test -p mxm-mono-01-host-tests --test hosting` |
| Floating editor windows | `--test t7_editor`. `editor_resize` needs every product's bundle, so it lives in newdawn-workspace (the workspace's `collection-tests/`) |
| One synth, its plugin or its DSP crate | That crate's own tests, then `cargo test -p <synth>-host-tests` (its `behaviour` and `golden_audio`) if the change is audible. Never the sequencer files |

Lint with `cargo clippy -p mxm-player --lib` while iterating, `--all-targets` once a test file changed. A
comment-only or format-only edit gets `rustfmt --check` on the file and no test run.

**The gate, for merging into main or on request** ([NOTES.md § The gate](NOTES.md#the-gate-merging-into-main-or-on-request)):

```bash
cargo xtask fetch                        # the plugins test-bundles.txt pins, release and debug
cargo build -p nice-plug-output-fixture  # output allocation regression
cargo xtask fixtures --release           # -> target/fixtures/mxm-fixtures.clap
cargo test -p mxm-player && cargo clippy -p mxm-player --all-targets && cargo run -p mxm-player --release
```

- A missing artifact makes a test **skip with a message**; a skipped test is not a passing one, so check
  the output. `tests/plugin_robustness.rs` fails, never skips, without its `target/debug` builds.
- Layer 1 bypasses the app, 2 runs it headless, 3 renders deterministic sessions and each product's host
  tests: [NOTES.md § The test matrix](NOTES.md#the-test-matrix). Assertions are structural, state or paint
  output (only paint sees the keyboard); no image snapshots; prove every new oracle by falsification.
  [§ Styles](NOTES.md#the-three-assertion-styles-and-which-can-see-what) · [§ Oracles](NOTES.md#an-oracle-that-cannot-fail-is-not-an-oracle)
- Investigate with a throwaway `Session` test and its dump, not a screenshot; audition through
  `Harness::with_fx`; never reduce `Session::advance_blocks` to one `service`. Manual, never pretended
  automated: the real CPAL backend and a MIDI keyboard. [NOTES.md § Driving](NOTES.md#driving-it-yourself) · [§ Auditioning](NOTES.md#auditioning-a-plugin-here-with-no-window-and-no-sound-card)

# Child DOX Index

No child AGENTS.md files. `src/` and `tests/` are covered by this doc.
