# Contributing

Issues and pull requests are welcome.

- **Read the `AGENTS.md` chain first** — the root, then the one in each folder you touch. They are
  the working rules: what each part owns, what it must not do, and how it is checked.
- **All three platforms.** Windows, macOS and Linux; anything platform-specific is `cfg`-gated with
  every arm implemented.
- **Check before you send** — the fast tier, then the host tests if you changed what a player hears:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test
  ```

**Before your first pull request is merged you will be asked to sign the MXM Contributor Licence
Agreement** (CLA Assistant does this on the pull request). It lets mxm keep publishing your
contribution under the GPL, and under other licences if a project ever needs to; you keep the
copyright in what you wrote.
