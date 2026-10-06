# AGENTS.md — apps/mxm-player-harness

Parent: [`../../AGENTS.md`](../../AGENTS.md)

# Purpose

The harnesses MXM Player's tests drive it with, as a library every plugin's host tests share:

- `app_harness` — the real `PlayerApp` under `egui_kittest`, with no window, a fake audio backend and
  a sandboxed settings directory, read structurally, by state or by paint output
- `harness` — an `AudioWorker` owned directly, with no engine and no device, so the callback runs on
  the test thread and allocation, event order and the effect chain (`Harness::with_fx`) are
  assertable
- `workspace_root` — the repository under test, so bundles are found in its `target/bundled` and
  the fixture plugins in its `target/fixtures`

They were the player's `tests/app_harness` and `tests/harness` modules until each plugin's tests
through the player moved to `plugins/<plugin>/host-tests` (`plans/plan-repo-split.md` in the
private archive, Phase 1), in each product's own repository. How to use each, and what each can and
cannot see, is in [`apps/mxm-player/NOTES.md`](../mxm-player/NOTES.md) *Verification detail*.

# Ownership

Owns `Cargo.toml` and `src/{lib.rs, app_harness.rs, harness.rs}`. The player's behaviour is the
player's; this crate only drives it.

# Local Contracts

- **No fixed path.** `workspace_root` reads `CARGO_MANIFEST_DIR` at run time — cargo sets it for
  every test — and walks up to the nearest `Cargo.lock`, so it is right in this repository and in
  each product's own (as it was in the monorepo's workspace). Never derive the root from this
  crate's own manifest.
- **A missing artifact skips, with the command that builds it**, rather than failing: a missing build
  is not a hosting bug.
- **The settings directory is always sandboxed.** With the production configuration an app-level test
  would rewrite the settings of whoever uses the player on the same machine.
- Not test-built itself (`test = false`); it is exercised by every package that drives it.

# Verification

```bash
cargo clippy -p mxm-player-harness --all-targets -- -D warnings
cargo check --tests -p mxm-player                       # its users still compile against it
cargo check --tests -p <plugin>-host-tests              # in each product's repository, at the new tag
```

# Child DOX Index

None.
