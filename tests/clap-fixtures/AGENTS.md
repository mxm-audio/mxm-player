# AGENTS.md — tests/clap-fixtures

Parent: [`../../AGENTS.md`](../../AGENTS.md)

# Purpose

CLAP plugins that exist only to be refused, to misbehave, or to emit things a well-behaved plugin
never would. They are what lets the player's compatibility envelope and fault handling be tested
without third-party plugins on the machine.

The main crate also holds **a well-behaved foreign reference plugin**, for when a player behaviour
cannot otherwise be proved independently: `dk.mxm.fixture.effect` is what the player's effect
hosting is proved against, precisely because it shares no code with the collection.

**Test-only. Never shipped.** `publish = false`, staged well away from `target/bundled/`.

# Ownership

Owns the clack fixture factory in `src/{lib.rs, fixture.rs, spec.rs}` and `Cargo.toml`, plus the
nested `nice-plug-output/` package used only to exercise nice-plug's own output queue.

Consumed by [`apps/mxm-player`](../../apps/mxm-player/AGENTS.md); built and staged by
[`xtask`](../../xtask/AGENTS.md).

# Local Contracts

## One clack cdylib, many ordinary fixture IDs

One Cargo package with one `cdylib` cannot produce several libraries, so the main clack factory
exposes every ordinary fixture ID. `nice-plug-output/` is the one deliberate second cdylib: the
behavior under test is nice-plug's wrapper itself, which a clack fixture necessarily bypasses. IDs
are namespaced `dk.mxm.fixture.<name>`.

## The differences live in `spec.rs`, not in code

Every clack fixture is the **same plugin type** driven by a different `FixtureSpec`. Adding an
ordinary host case is a **row in `spec.rs`**, not a new crate and not a new plugin impl. If a case
cannot be expressed as a spec row, extend `FixtureSpec` rather than special-casing `fixture.rs`.
The nested nice-plug package exists only because wrapper behavior cannot be expressed through clack.

## Quarantine

The player's tests load these **by path**, never through its scanner, so a deliberately hostile
fixture cannot end up in an ordinary scan. Staging therefore goes to `target/fixtures/`, well away
from `target/bundled/`. Do not stage a fixture anywhere the scanner looks, and do not add one to
`bundler.toml`.

`dk.mxm.fixture.hang` **never returns from `process()`**, by design. It cannot run in the main test
process; its test spawns a subprocess. Any similarly terminal fixture needs the same treatment.

## The reference effect is the one fixture that transforms audio

`Behaviour::Effect` backs two rows — `dk.mxm.fixture.effect` (stereo in, stereo out) and
`dk.mxm.fixture.effect-mono` (mono in, mono out), so the player's channel adapter is tested in
both directions. Both declare an in-place pair on their ports, which is what makes a host
(clap-validator included) share a buffer at all. The player loads them by path like every fixture.

Its contract is the arithmetic, held in `fixture.rs` and exported as constants: per channel,
`out = Amount * in + 0.5 * delayed`, where `delayed` is what the line held exactly 480 frames ago
and the line takes `in + 0.5 * delayed` in its place — every echo half the last. The tail is
480 × 12 frames; it returns `Tail` until the input has been silent that long, then snaps the
lines to digital silence and returns `Sleep`. `reset` restores exactly the `activate` state.

## Effect-shaped host callback coverage

`dk.mxm.fixture.effect-main-thread` uses `Behaviour::MainThreadCallback` with a stereo effect
port layout. It reports callback count through Amount and requests its second callback from the
first, so the player's engine can prove effect requests are serviced once per main-thread turn,
without opening an editor. It is staged only with the other fixtures, never in product bundles.

## Extension features are minimal and attributed

`clack-extensions` features are enabled only as some fixture needs them, with a comment naming which
one — `log` for `log-spam`, `tail` for `tail-shift` and `effect`. Keep that attribution when adding
a feature.

## MSRV

1.87, declared on `[package]` — no GUI dependencies. It belongs on the package, not under `[lib]`.

# Work Guidance

Add a fixture when a player behaviour cannot otherwise be provoked honestly — a refusal path, an
overflow counter, a terminal failure — or cannot otherwise be proved independently, which is what
the reference effect is for. Not to cover a case the real plugin already exercises.

A fixture that reads its input must not go through the zero-fill the others share: `process`
skips it for `Behaviour::Effect` because a zero-fill destroys the input when the host processes in
place. Read each input sample into a local before writing its output, for the same reason.

The nice-plug output fixture stays minimal and emits beyond the wrapper's configured capacity only
through `ProcessContext::send_event`. A distinct note-on admitted before saturation is followed by
its note-off after saturation, so the host regression proves terminations displace ordinary output
instead of sticking the note. Build it in debug: the production allocation guard is compiled out in
release.

# Verification

```bash
cargo xtask fixtures --release          # -> target/fixtures/mxm-fixtures.clap
cargo build -p nice-plug-output-fixture # -> target/debug; allocation guard enabled
cargo test -p mxm-player                # the fixtures' only real consumer
cargo clippy -p clap-fixtures --all-targets
cargo fmt -p clap-fixtures              # -p kept `--all` off vendor/ in the monorepo; none here now
clap-validator validate -p dk.mxm.fixture.effect target/release/clap_fixtures.dll
clap-validator validate -p dk.mxm.fixture.effect-mono target/release/clap_fixtures.dll
```

These plugins have no tests of their own; they are verified by the player's matrix using them.
`plugin_robustness` loads the nice-plug fixture directly from `target/debug`, never a release build.
The two effect rows are the exception: they are conforming, so clap-validator must pass them.
The hostile fixtures are expected to fail it, and that says nothing.

# Child DOX Index

No child AGENTS.md files.
