# drf-server

An independent, open-source server for **Dungeon Runners** (2007), written in
Rust with no external runtime or async framework.

> **Unofficial project.** This repository is not affiliated with, endorsed by,
> or supported by NCsoft, NetDevil, or any other rights holder. Dungeon Runners
> and related names are trademarks of their respective owners. See
> [`NOTICE.md`](NOTICE.md).

## Status

Early alpha. The current solo lane covers login, character management, town and
generated dungeon entry, movement, combat, loot, merchants, trainers, quests,
inventory/equipment, difficulty selection, and persistence.

This is an interoperability server. It is not a game client, does not include
the game, and does not distribute any game assets. You must supply your own
legally obtained client installation (`game.pki`, `game.pkg`, and the client
executable).

## Requirements

- Rust 1.88 or newer (edition 2021)
- A legally obtained Dungeon Runners client installation

No third-party Rust dependencies are used by the server crates.

## Build

```sh
cargo build --release
```

## Run

```sh
./target/release/drf-server \
  --dir ./data \
  --game-pki /path/to/game.pki \
  --game-pkg /path/to/game.pkg
```

The server listens on auth port `2110` and game port `3503` by default. Point
the client at the same host and ports.

Useful options:

```text
--auth-port N
--game-port N
--advertised-game-port N
--game-ip IPV4
--dir PATH
--game-pki PATH --game-pkg PATH
--admin
```

`--admin` enables the shipped administrative command surface. Do not expose an
admin-enabled server to untrusted clients.

## Test

Content tests use `game.pki`/`game.pkg`. Point the test suite at your
installation explicitly:

```sh
DRF_GAME_PKI=/path/to/game.pki \
DRF_GAME_PKG=/path/to/game.pkg \
cargo test --workspace
```

Tests that require the game archive skip when it is unavailable. The transport,
protocol, and persistence tests do not need game assets.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Workspace layout

| Crate | Purpose |
|---|---|
| `drf-wire` | Game-stream framing, zlib, and transport primitives |
| `drf-content` | `game.pki`/`game.pkg` reader, content model, zones, loot, quests |
| `drf-proto` | Client-compatible message bodies and parsers |
| `drf-sim` | Deterministic simulation primitives |
| `drf-trace` | Canonical trace vocabulary |
| `drf-server` | Listeners, sessions, world state, persistence, orchestration |

## Contributing

Read [`CONTRIBUTING.md`](CONTRIBUTING.md) before opening a pull request. In
particular, do not add game assets, client binaries, network captures,
decompiled output, or code that is incompatible with GPLv3.

## Security

Please do not report security issues in public issues. See
[`SECURITY.md`](SECURITY.md).

## License

The source code in this repository is licensed under the GNU General Public
License v3.0 or later. See [`LICENSE`](LICENSE). The license does not grant
rights to Dungeon Runners, its game data, its trademarks, or any third-party
material. Some portions are adapted from the GPLv3-licensed `dr_pyserver`
project; see [`NOTICE.md`](NOTICE.md) and [`PROVENANCE.md`](PROVENANCE.md).
