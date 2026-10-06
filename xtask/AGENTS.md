# AGENTS.md — xtask

Parent: [`../AGENTS.md`](../AGENTS.md)

# Purpose

The repository's build tooling, reached through `cargo xtask` (aliased in `.cargo/config.toml` to
`run --package xtask --release --`).

Two commands:

- `bundle` — the shared tooling in [`crates/mxm-xtask`](https://github.com/mxm-audio/mxm-kit/blob/main/crates/mxm-xtask/AGENTS.md):
  `nice_plug_xtask`'s bundler, followed by staging each plugin's control map beside its bundle
- `fixtures` — the one command only this repository needs: builds and stages the test-only CLAP
  plugins in `tests/clap-fixtures`

The check that a player's words speak to the player now runs in each plugin's own tests, from
`mxm_plugin_test::hover_text` (see [`crates/mxm-plugin-test`](https://github.com/mxm-audio/mxm-kit/blob/main/crates/mxm-plugin-test/AGENTS.md)).

# Ownership

Owns `src/{main.rs, fixtures.rs}` and `Cargo.toml`. Bundling and control-map staging are
`crates/mxm-xtask`'s.

# Local Contracts

- **Stay thin.** `main.rs` passes the workspace root to `mxm_xtask::main` and handles `fixtures`
  itself. Add a command here only when neither upstream tool can do it, and say why in the module
  doc.
- **Shipped and test-only artifacts never share a directory.** `bundle` writes to
  `target/bundled/`; `fixtures` writes to `target/fixtures/`. That separation is what keeps a
  hostile fixture out of an ordinary plugin scan — see
  [`tests/clap-fixtures/AGENTS.md`](../tests/clap-fixtures/AGENTS.md). Do not merge them.
- Fixtures do not go through `bundle` and are not listed in `bundler.toml`.
- **`bundle` and `fixtures` build the outermost workspace on the path**, so a worktree nested inside
  the repository builds the parent checkout: see
  [`crates/mxm-xtask/AGENTS.md`](https://github.com/mxm-audio/mxm-kit/blob/main/crates/mxm-xtask/AGENTS.md). `fixtures` copies to
  `target/fixtures/mxm-fixtures.clap` of the same outermost checkout.
- `publish = false`. Dependencies pinned exactly.

# Work Guidance

A new plugin needs a row in the root `bundler.toml` mapping crate name to display bundle name; no
xtask change is required for it, and its `control-map.json` is picked up from the same row.

# Verification

```bash
cargo xtask bundle mxm-mono-01 --release    # -> target/bundled/mxm-mono-01.clap
                                        #    + mxm-mono-01.control-map.json
cargo xtask fixtures --release          # -> target/fixtures/mxm-fixtures.clap
cargo clippy -p xtask --all-targets
```

The check is that both artifacts appear at the paths above and that
`clap-validator validate "target/bundled/mxm-mono-01.clap"` can load the bundled one. There are no unit
tests here.

# Child DOX Index

No child AGENTS.md files.
