# AGENTS.md — mxm-player

DOX rail for this repository. Project-wide instructions, durable workflow rules, and the
top-level Child DOX Index.

---

# DOX framework

- DOX is a highly performant AGENTS.md hierarchy installed here
- Agents must follow DOX instructions across any edits

## Core Contract

- AGENTS.md files are binding work contracts for their subtrees
- Work products, source materials, instructions, records, assets, and durable docs must stay
  understandable from the nearest applicable AGENTS.md plus every parent AGENTS.md above it

## Read Before Editing

1. Read the root AGENTS.md
2. Identify every file or folder you expect to touch
3. Walk from the repository root to each target path
4. Read every AGENTS.md found along each route
5. If a parent AGENTS.md lists a child AGENTS.md whose scope contains the path, read that child and
   continue from there
6. Use the nearest AGENTS.md as the local contract and parent docs for repo-wide rules
7. If docs conflict, the closer doc controls local work details, but no child doc may weaken DOX

Do not rely on memory. Re-read the applicable DOX chain in the current session before editing.

## Update After Editing

Every meaningful change requires a DOX pass before the task is done.

Update the closest owning AGENTS.md when a change affects:

- purpose, scope, ownership, or responsibilities
- durable structure, contracts, workflows, or operating rules
- required inputs, outputs, permissions, constraints, side effects, or artifacts
- user preferences about behavior, communication, process, organization, or quality
- AGENTS.md creation, deletion, move, rename, or index contents

Update parent docs when parent-level structure, ownership, workflow, or child index changes. Update
child docs when parent changes alter local rules. Correct stale or contradictory text immediately, and move its history to `NOTES.md` rather than deleting it; a discarded idea or replaced design is deleted outright, not archived: git keeps it (the owner, 2026-10-08).
Small edits that do not change behavior or contracts may leave docs unchanged, but the DOX pass
still must happen.

## Hierarchy

- Root AGENTS.md is the DOX rail
- Child AGENTS.md files own domain-specific instructions and their own Child DOX Index
- Each parent explains what its direct children cover and what stays owned by the parent
- The closer a doc is to the work, the more specific and practical it must be

## Child Doc Shape

- Create a child AGENTS.md when a folder becomes a durable boundary with its own purpose, rules,
  responsibilities, workflow, materials, or quality standards
- Work Guidance must reflect current project standards or user instructions; leave it empty if
  there are none yet
- Verification must reflect an existing check; leave it empty until one exists

Default section order: Purpose · Ownership · Local Contracts · Work Guidance · Verification ·
Child DOX Index

## Style

- Keep docs concise, current, and operational
- Document stable contracts, not diary entries
- **A value the code holds is named, not copied** (the owner, 2026-09-24: *"Why are you writing the
  opening pages size in such detail. Is that not already described in the code?"*). A size a test
  derives — an opening size, a minimum, a card floor — or a constant the code declares is stated in
  DOX as its rule, its constant and the test that holds it, never its number: a copied number goes
  stale the day the code moves. Two exceptions: the design system states its own tokens and
  rules, because it is the normative source the code implements; and a plan's revision history
  records what was measured when — a dated record, not the current value
- Put broad rules in parent docs and concrete details in child docs
- Prefer direct bullets with explicit names
- Do not duplicate rules across many files unless each scope needs a local version
- Keep AGENTS.md current: correct a stale note and move its history to `NOTES.md` instead of explaining it here;
  a discarded idea is deleted, never kept as history
- Trim obvious statements, repeated rules, misplaced detail, and warnings for risks that no longer
  exist

## Closeout

1. Re-check changed paths against the DOX chain
2. Update nearest owning docs and any affected parents or children
3. Refresh every affected Child DOX Index
4. Correct stale or contradictory text; move the reasoning still in force to `NOTES.md`, and delete a
   discarded idea outright
5. Run existing verification when relevant
6. Report any docs intentionally left unchanged and why

---

---

# Purpose

**mxm-player** is MXM Player, the CLAP host that plays, sequences and tests the MXM instruments and effects.

**No new features** (the owner, 2026-10-07: "All player work now goes into the daw"). Playing,
sequencing and hosting work belongs in newDAWn. This repository stays as the host the plugins'
`host-tests` and the collection checks run in: fix what those need, nothing more.

It is one of the MXM products, each in its own repository under
[github.com/mxm-audio](https://github.com/mxm-audio), built on the MIT-licensed
[mxm-kit](https://github.com/mxm-audio/mxm-kit) — the design system, keyboard navigation, presets,
modulation, the control map and the checks every plugin shares. Until 2026-10 all of it was one
repository (`mxm-collection`); references to `plans/` name its design history, which stays in
a private archive.

Reference-quality open source: clarity beats cleverness, and every nontrivial algorithm names the
technique or paper it comes from.

# Ownership

Root owns `Cargo.toml`, `Cargo.lock`, `LICENSE`, `NOTICE.md`, `TRADEMARKS.md`, `README.md`,
`CONTRIBUTING.md`, `.cargo/`, `.github/` and `xtask/`.
Each folder with an `AGENTS.md` owns its contents; the index is below.

**Dependencies follow each repository's `main`, and `Cargo.lock` pins the exact commit.** The kit comes from mxm-kit's
`main`, another product's crates from its repository's `main`, and nice-plug and
egui-baseview from their MXM forks (`[patch.crates-io]`, their `main` as well).

## Windows, Linux and macOS — all three, always

**An absolute requirement.** Everything here runs on all three; a change that works on one and
breaks another is a broken change. CI builds and tests on all three, when started by hand (the owner, 2026-10-06); before a push, check on Windows; Linux and macOS are checked later, together.

- **Anything platform-specific is `cfg`-gated with every arm implemented**, never one arm and a
  silent nothing elsewhere.
- **Linux needs system libraries** the other two carry in their SDKs — ALSA (and JACK) for audio,
  and X11, xkbcommon and a GL loader for the window.
- **A dependency that does not support all three cannot be taken**, whatever else it offers.

## MSRV is per crate

| Crate | MSRV | Why |
|---|---|---|
| `apps/mxm-player` | **1.95** | eframe + egui |

## Licensing

**GPL-3.0-or-later** (`LICENSE`). `NOTICE.md` lists the third-party code in its builds. The MXM
name and logo are not covered by the licence: see `TRADEMARKS.md`.

- **MPL-2.0 is accepted** for symphonia, through `mxm-audio-file-decode` only, used unmodified;
  never vendor, patch or modify an MPL crate.
- Check the licence before porting any algorithm, and record source and licence in a comment at
  the top of the file. Cite techniques even when the implementation is original.

## Research citations

A citation written `` `research:<path>` `` names a page in MXM's private research repository. It
is plain text in a code span, never a link, and nothing here depends on it at build or test
time. Facts, numbers, our own measurements and short quotations cross into this repository;
third-party files, images and verbatim text never do.

# Verification

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask fixtures --release              # the hosting fixtures
cargo build -p nice-plug-output-fixture      # the output allocation regression
cargo xtask fetch                            # plugins from other repositories its tests load
cargo test
```

CI runs the same on Windows, macOS and Linux, only when started by hand
(the owner, 2026-10-06). Before a push, run them on Windows; Linux and macOS are checked later,
together.

# Child DOX Index

| Doc | Scope |
|---|---|
| [`apps/mxm-player/AGENTS.md`](apps/mxm-player/AGENTS.md) | MXM Player host, realtime rules, sequencer, effects and verification |
| [`apps/mxm-player-harness/AGENTS.md`](apps/mxm-player-harness/AGENTS.md) | The harnesses the player's tests and every plugin's `host-tests/` drive it with — the windowless app, the worker on the test thread, and where a repository's bundles are found |
| [`tests/clap-fixtures/AGENTS.md`](tests/clap-fixtures/AGENTS.md) | Deliberately misbehaving CLAP plugins and how they stay quarantined — plus the one conforming reference effect the player's effect hosting is proved against |
| [`xtask/AGENTS.md`](xtask/AGENTS.md) | Build tooling: bundling and fixture staging |
