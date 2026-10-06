# mxm-player

MXM Player, the CLAP host that plays, sequences and tests the MXM instruments and effects.

Part of the MXM collection: every instrument, effect and tool lives in its own repository under
[github.com/mxm-audio](https://github.com/mxm-audio), built on
[mxm-kit](https://github.com/mxm-audio/mxm-kit).

## Building

Rust 1.95 or newer. On Linux, install ALSA, JACK, X11, xkbcommon and a GL loader first.

```bash
cargo build --release
cargo test
```

## Licence

GPL-3.0-or-later — see [`LICENSE`](LICENSE) and [`NOTICE.md`](NOTICE.md). The MXM
name and logo are trademarks; [`TRADEMARKS.md`](TRADEMARKS.md) says how they may be used.

Contributions are welcome: see [`CONTRIBUTING.md`](CONTRIBUTING.md). How the repository is
organised, and the rules each part keeps, are in [`AGENTS.md`](AGENTS.md).
