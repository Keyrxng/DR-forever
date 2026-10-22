// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-server`: the runnable solo server (std-only, one thread per connection).
//!
//! The layers are `store` (character persistence), `world` (town entities and
//! the CEM tick block), `server` (listeners, the auth handshake, and shared
//! state), and `session` (one game connection's state and handlers). Wire
//! builders live in `drf-proto`; segment framing lives in `drf-wire`.
//!
//! The server currently covers the solo play loop: login, character
//! management, town and generated dungeons, movement, combat, loot, merchants,
//! trainers, quests, inventory, and persistence.

pub mod opcodes;
pub mod replay;
pub mod server;
pub mod session;
pub mod store;
pub mod world;

pub use server::{parse_server_list_payload, server_list_payload, Server, Shared};
pub use store::Store;
pub use world::World;
