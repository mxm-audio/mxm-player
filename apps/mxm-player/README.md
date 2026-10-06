# MXM Player

A CLAP host for testing and playing the MXM Synth Collection.

This machine has no DAW, so mxm-mono-01 could not be played at all — only rendered offline through its
own DSP. Every MXM plugin's plan ends with "load it in a host and play it", and this is what makes
that step possible. It is not only a test harness: by the owner's brief it should be good enough to play
live.

**Its own product.** Own version, MSRV, README and licence, released independently. It shares the
design system and `crates/ui` without being coupled to any plugin's release cycle.

## Why a host, not a standalone wrapper

nice-plug can compile a plugin into a standalone executable in an afternoon, but that produces one
binary per plugin, tests the *standalone* wrapper rather than the `.clap` we ship, and cannot open
anything else. A host loads bundles at runtime, works for every future MXM plugin without
rebuilding, and can load third-party CLAP plugins for comparison.

## Building and running

```bash
cargo run -p mxm-player --release

# The artifacts the player and its tests expect:
cargo xtask bundle mxm-mono-01 --release   # -> target/bundled/mxm-mono-01.clap
cargo xtask fixtures --release         # -> target/fixtures/mxm-fixtures.clap

cargo test -p mxm-player
cargo clippy -p mxm-player --all-targets
```

The tests skip with an explanatory message when an artifact is missing, because a missing build is
not a hosting bug.

## What it does

- **Plugin browser** over the standard CLAP locations, `CLAP_PATH`, and this repo's
  `target/bundled`. Plugins outside the v1 envelope are listed with the reason they were refused.
- **Three input paths**, all first-class: an on-screen keyboard fixed along the bottom of the
  window, the computer keyboard in a piano layout, and connected MIDI keyboards.
- **MIDI in and out**, including keyboard thru, so the output picker is actually verifiable —
  mxm-mono-01 emits no MIDI of its own.
- **A sixteen-step sequencer** whose steps are rests, notes or **ties**. A tie continues the note
  before it, so a note followed by *k* ties lasts *k* + 1 steps — and the tied steps are drawn as
  **one wide button**, because a tie joins them. The small bar on a button means that step holds
  notes, centred over its number. Click a step to select it, play any keyboard to set its pitch,
  click it again to change how long the note is, Escape to stop. A run is one note: a pitch played
  into a tied step lands on the step that started the run, and the keyboard marks that run's notes.
  **Play always starts from step 1** — which is what the spacebar always did — and the other
  half of that is Stop, since there is nothing to resume.
- **Steps set parameters, not just notes.** Select a step and turn any knob — in the player's panel,
  on a mapped hardware controller, or in the instrument's own editor — and that step sets it. Turning
  it is how you record it, and you hear what you are setting as you do. Double-click a knob to put it
  back to the patch, which is the same thing as that step setting nothing. A dot in a step's corner
  says it sets something; a dot beside a parameter's name says the selected step sets *that*, and
  while a step is selected the knobs show what the step does rather than what is currently sounding.

  Up to 32 parameters can be sequenced at once; the 33rd is refused with the limit named. Locks are
  saved with the sequence and restored on the next launch, and they remember which instrument they
  were recorded for: load a different one and they **wait** rather than moving its controls or being
  thrown away, and they come back when their own instrument does. A parameter that instrument no
  longer has is dropped and said so. **MIDI cannot carry any of this** — saving as `.mid` says so,
  and loading one clears them.
- **An effect chain after the instrument**, shown as small vertical strips to the right of the
  sequencer, one per effect in signal order: `+` adds an effect, and each strip switches it off
  (off means it is not called at all), opens its own editor, moves it earlier or later, and
  removes it. The instrument itself has no strip — it is named in the bar at the top, whose Show
  editor button opens its interface. The player shows **no
  effect parameters** — the effect's editor is where they live. The chain is saved and comes back
  on the next launch, and it is in every export.
- **Audio output** device, sample rate and buffer size, persisted by stable identity.
- **Three separate load figures**: time inside `process()`, the whole callback, and missed
  deadlines — the gap between the first two says whether the player or the plugin is at fault.
- **A generic parameter panel** with correct gesture bracketing, and state save/load, so "does
  this plugin's state round-trip" is a one-click check. It is a **state dump** — one slot, one file —
  and deliberately not a preset browser: presets belong to the plugin, whose own editor the player
  opens.

**A local CLI drives everything.** While the player runs, `mxm-cli` talks to it over a loopback
socket: `mxm-cli dump` returns the full state as JSON — including bookkeeping the window never shows
— and `load`, `select`, `set`, `lock`, `toggle`, `tie`, `tempo`, `play`, `export`, `fx` and the rest
cover everything the mouse can do. It exists so a machine (a test, or an AI assisting with composition)
can drive and observe the real player instead of a person relaying what the screen shows.

**Saving a sequence as `.mid` reports what MIDI cannot keep**, in the same place a load reports its
losses: a legato joint — a tie carrying its own notes — becomes two notes back to back and reads
back as an ordinary note, and a run carrying past the end of the bar is truncated there. Loading
accepts notes half a step or a whole number of steps long. A length in between, such as one and a
half steps, is **refused with the position named** rather than quantised, and so is a file where two
notes crossing one step would need it tied and untied at once.

## What it deliberately does not do

- **No audio input** until a plugin in this collection needs one. CPAL has no duplex stream, so
  input means a ring buffer, channel conversion, a latency policy, resampling and clock-drift
  compensation — and with no plugin that has inputs there would be nothing to route audio through.
  There is no input picker in the UI at all, rather than a disabled control implying it is nearly
  there.

  **Effects are hosted without it.** An effect is a plugin with an audio input *from the graph*,
  not from the device: the player runs a chain of them after the instrument (above), which needs
  no duplex stream. Device input stays as written, and costs what the paragraph above says.
- **Plugin GUIs open as floating windows**, not embedded panes. "Show editor" opens the plugin's
  own interface in a window the plugin owns; the parameter panel stays available either way, and is
  still the only way a plugin with no interface of its own can be played. Embedding — the plugin's
  window inside ours — stays unsupported: it needs a container window in three platform
  implementations, and Wayland has no cross-process embedding primitive at all.
- **The editor window is kept in front of the player on Windows.** Elsewhere it opens unowned and
  the player says so. The fix is upstream in `baseview`; see `src/ownership.rs`.
- **One source and one serial chain.** No bus, no send, no parallel path — no routing beyond
  source → effects → device, in the order the strips show.

## Fault isolation, stated plainly

**In-process hosting cannot be made crash-safe.** A native access violation in plugin code
terminates the player; a blocking plugin hangs whichever thread it blocks. Clack's safe wrappers
make the *host* side sound — they do not sandbox the plugin's native code.

The mitigations are honest but partial: device and port choices are persisted eagerly and
crash-safely, a sentinel points at the suspected bundle after a discovery crash, and a timeout
keeps the GUI alive in **one specific case** — a plugin that fails to return the audio processor
*from the audio callback*. A plugin is equally free to hang inside `instantiate`, `activate`,
state save or load, a parameter query, or `on_main_thread`, all of which run on the GUI thread, and
no in-process timeout can recover those.

## Interface

The player follows the collection design system in
[`docs/MXM_DESIGN_SYSTEM.md`](https://github.com/mxm-audio/mxm-kit/blob/main/docs/MXM_DESIGN_SYSTEM.md), with one
stated, temporary compromise: the shared interface foundation (`crates/ui`) is built at M4a, which
belongs to the collection's plan rather than this one. Until it lands the player uses plain
unstyled eframe widgets, confined to `src/ui/adapter.rs` so the retrofit is bounded to that one
module. This is not the MXM look and is not shipped as such.

## Licence

MIT. See `LICENSE`.
