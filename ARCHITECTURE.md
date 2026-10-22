# Architecture

`drf-server` is a single-process, `std`-only Rust server for the Dungeon
Runners client protocol. It is an independent interoperability implementation;
see [`PROVENANCE.md`](PROVENANCE.md).

## Why Rust

- Fixed-width integer and deterministic layout control for protocol work.
- No garbage collector in the 30 Hz tick path.
- One toolchain for the transport, content model, protocol, and server.
- The production crates have no third-party dependencies.

## Runtime model

- One thread per connection and one accept thread per listener.
- Shared state is held behind a small number of `Mutex`es.
- The simulation-facing crates (`drf-sim`, `drf-trace`) are synchronous and
  must not depend on wall-clock time, async runtimes, or shared mutable state.
- Network I/O is isolated at the server edge. A protocol or simulation change
  must not be ordered by an async executor.

## Zone ownership

- Each zone has one owner.
- `Zone::tick()` is the sole mutator for that zone's simulation state.
- Ordering is explicit: FIFO queues and stable entity iteration.
- Cross-zone state is passed by value or through the server's shared store, not
  by sharing mutable simulation state.

## Crate boundaries

| Crate | Responsibility |
|---|---|
| `drf-wire` | Segment framing, zlib, and transport primitives |
| `drf-content` | `game.pki`/`game.pkg` reader, zones, classes, loot, quests |
| `drf-proto` | Message bodies and parsers consumed by the client |
| `drf-sim` | Deterministic movement and simulation primitives |
| `drf-trace` | Canonical trace event vocabulary |
| `drf-server` | Listeners, sessions, world state, persistence, orchestration |

Dependencies point inward: `drf-server` may depend on the lower-level crates,
but those crates must not depend on the server binary.

## Design constraints

- Prefer plain structs, `Vec`, and `VecDeque` over trait frameworks or an ECS.
- Keep protocol builders byte-oriented and independently testable.
- Keep content derivations tied to `game.pkg`; do not hand-write content that
  the archive already states.
- New behavior should be covered by a pinned byte or state test.
- Avoid adding dependencies. The crates are intentionally buildable offline
  with a stock Rust toolchain.

## Persistence

Character state is stored in a local JSON file. The store is intentionally
small and readable; it is not a database and is not safe for concurrent
multi-process writers. Back up the data directory before upgrading.
