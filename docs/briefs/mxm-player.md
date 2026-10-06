# Design brief — MXM Player

The player is a host, not an instrument, so the §14 questions land slightly differently: there is
no source hardware to remove things from, and its "sound-design task" is somebody else's. It is
still an MXM interface, so it answers them.

**Status.** This brief describes the target. `crates/ui` landed at M4a and the player renders
through it: the interim unstyled-widget foundation is gone, and `apps/mxm-player/src/ui/adapter.rs`
now only translates between CLAP parameter snapshots and the shared controls. The §15 QA gate has
not been run in full, and this brief does not claim to have passed it.

## 1. Primary task

Two, and they pull in different directions:

- **Testing.** Does each control do what its label says, does it click on note-on, does state
  survive a round trip, is the plugin costing what it should.
- **Playing.** It should be pleasant to actually play, including live.

Where they conflict, playing wins for the bottom of the window and testing wins for everything
above it. The keyboard is always visible; the diagnostics are always available but never in the
way.

**Parameter locks moved the line, and it is worth saying where it now is.** The sequencer was *not a
composition tool*; a step that sets parameters as well as notes is a genuine widening of that. It
earns its place under **testing**, not under playing: hearing a synth means hearing it move, and a
filter that opens across a bar is the difference between auditioning an instrument and listening to
a static chord. What is still refused is everything past one bar - no song, no automation curves, no
recorded performance. The test is whether a feature helps you *hear what an instrument does*; a
feature that helps you write a piece with it belongs in a DAW, which is what the `.mid` export is
for.

## 2. The three to five things reached for most

1. **The keyboard** — on-screen, computer, or a connected MIDI keyboard.
2. **Which plugin is loaded.**
3. **A parameter**, dragged, typed into, or reset.
4. **Octave, sustain, pitch bend and mod wheel** — the performance controls beside the keys.
5. **The load figures**, glanced at rather than studied.

## 3. Signal flow that must be visible

The player's own, not the plugin's: **input → plugin → audio out**, with **MIDI out** branching
off it. Three things must be legible at a glance because they are what "no sound" actually means:

- whether MIDI is arriving (per-direction activity),
- whether the engine is running, sleeping, or has failed,
- whether audio is leaving.

The plugin's internal signal flow is the plugin editor's job, not the host's.

## 4. Play view

The bottom strip: keyboard, octave, sustain, bend, mod, panic. Fixed height, always present,
never scrolled away.

Above it, the parameter panel. It **was** the plugin's play surface; a plugin with an interface of
its own now shows it through **"Show editor"**, in a floating window the plugin owns. The parameter
panel stays — it is how a plugin with no interface is played at all, and how a hidden or unmapped
parameter is reached.

**The player collapses so both windows fit.** Two panels collapse, independently and remembered:
the parameter panel and the left settings panel. **Three never do** — the status bar, the sequencer
and transport, and the keyboard. That is this section's own ranking applied: playing wins for the
bottom of the window, testing wins for everything above it. There is no "collapse everything";
collapsing changes the layout only when the user asks, never as a consequence of opening an editor.

## 5. Advanced controls and disclosure

The left panel holds everything that is set once and then forgotten: the plugin browser, audio
device, sample rate, buffer size, MIDI ports, thru, state save/load, and the plugin's own log.
It is a panel rather than a modal because a device change during testing is common enough that
burying it would be worse than the space it costs.

It **collapses**, which is not burying: one labelled control in the status bar, which never
collapses itself. The space it costs is worth reclaiming when a plugin's own window is open beside
the player.

Refusal reasons sit inline under the plugin that was refused, not behind a tooltip: they are the
whole point of the browser being able to show unsupported plugins at all.

## 6. Top-level views

None. The player is one screen. Adding Synth/Mod/FX views would be borrowing an instrument's
anatomy for something that is not an instrument.

## 7. Identity accent

The player takes the **neutral** system accent rather than an instrument identity colour — it is
the frame around other people's instruments, and a strong accent of its own would compete with
whichever plugin is loaded. The one saturated colour it does use is the running/held green, which
carries state rather than identity, and the failure red.

Contrast is measured, not eyeballed, in both themes, when the design lands on `crates/ui`.

## 8. Live visualisations that materially help

- **Held-note highlighting on the keyboard**, including notes arriving over MIDI, so the keyboard
  doubles as a monitor. This is the one visualisation that repeatedly answers a real question:
  is the note stuck, or was it never sent?
- **Plugin load vs callback load, side by side.** One number would be useless; the *gap* is the
  diagnostic.
- **Per-direction MIDI activity.** The quickest way to tell "no sound" from "no MIDI".

Deliberately absent: a spectrum analyser and a waveform scope. Neither answers a question this
tool is for, and both would be motion for its own sake.

## 9. What is removed, and why

There is no source hardware. What is removed is *DAW*: no transport, no timeline, no chains, no
recording, no routing graph. §7 of the plan is the fence, and the UI does not imply anything
beyond it — which is why there is no audio-input picker at all rather than a disabled one.

## 10. Minimum size and 200% scale

Reference frame 1200×760, resizable 75–200%, like every other MXM interface.

- The keyboard strip keeps its height and loses white keys from the right, never its controls.
- The left panel collapses first; the parameter panel is the last thing to give up width and
  scrolls rather than truncating.
- The status bar wraps its readings rather than eliding them: a load figure that has silently
  disappeared is worse than one on a second line.
