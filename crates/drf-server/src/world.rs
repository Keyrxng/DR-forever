// SPDX-License-Identifier: GPL-3.0-or-later

//! Town world state: u16 entity allocation, the CEM tick block, the town zone
//! definition, and the town entity roster.
//!
//! This module owns the town roster. Generated dungeon population and
//! per-session zone state live in [`crate::session`]. Values below are derived
//! from `game.pkg` or are explicitly marked as session bootstrap values; no
//! game assets are included in this repository.
//!
//! Provenance of the values below:
//!   * `game.pkg`-derived: the zone token resolves the shipped [`town.zone`]
//!     `ZoneDef` whose `Name` is `Town` and whose `Label` is `Townston`
//!     (read from `game.pkg`; the client matches zone names case-
//!     insensitively, so the lowercase wire token `town` loads `Town`), the
//!     class names `world.checkpoints.TownCheckpointEntity`, `misc.Waypoint`
//!     and `world.town.npc.{OldMan1,VendorWeapon1,VendorPotion1}` (all
//!     placed by `town.world`).
//!   * session bootstrap constants: the assign GC-type list (empty) and trailer
//!     (eight zero bytes) — the live town `(13,0)` shape, not readable from
//!     `game.pkg`.
//!   * E0 (approximate, unverified) pose constants: the town spawn, the
//!     static poses and the NPC poses. They sit near the town spawn rather
//!     than at the placements `town.world` states; see each constant.

use std::collections::{HashMap, HashSet};

use drf_proto::JoinError;

/// The first entity id the allocator hands out (`ENTITY_ID_MIN`).
pub const ENTITY_ID_MIN: u16 = 0x1000;

/// The last allocatable entity id (`ENTITY_ID_MAX`).
pub const ENTITY_ID_MAX: u16 = 0xFFFF;

/// The first component id the allocator hands out.
///
/// Component ids are a separate space from entity ids: the client holds them
/// in its component map, and a re-used id from a previous join is client error
/// 9 (the sticky `CEM+0xAC0` latch). Component ids start here.
pub const COMPONENT_ID_MIN: u16 = 0xE000;

/// The CEM constructor's tick interval in milliseconds (`+0xA80`, `0x21`).
pub const TICK_INTERVAL_MS_DEFAULT: u32 = 33;

/// The CEM constructor's ticks-per-update budget (`+0xA84`, `0x0B`).
pub const TICK_BUDGET_DEFAULT: u32 = 11;

/// A server-assigned u16 entity id allocator.
///
/// Ids are drawn from `[ENTITY_ID_MIN, ENTITY_ID_MAX]`; zero is the
/// unassigned sentinel and is never handed out. A freed id becomes
/// allocatable again, so ids are only unique among the live set.
#[derive(Debug)]
pub struct EntityAllocator {
    next: u16,
    live: HashSet<u16>,
}

impl Default for EntityAllocator {
    fn default() -> EntityAllocator {
        EntityAllocator::new()
    }
}

impl EntityAllocator {
    /// A fresh allocator starting at [`ENTITY_ID_MIN`].
    pub fn new() -> EntityAllocator {
        EntityAllocator {
            next: ENTITY_ID_MIN,
            live: HashSet::new(),
        }
    }

    /// Allocate the next free id, or `None` when the id space is exhausted.
    pub fn alloc(&mut self) -> Option<u16> {
        let span = u32::from(ENTITY_ID_MAX) - u32::from(ENTITY_ID_MIN) + 1;
        for _ in 0..span {
            let candidate = self.next;
            self.next = if self.next == ENTITY_ID_MAX {
                ENTITY_ID_MIN
            } else {
                self.next + 1
            };
            if candidate != 0 && !self.live.contains(&candidate) {
                self.live.insert(candidate);
                return Some(candidate);
            }
        }
        None
    }

    /// Release `eid`. Freeing an unallocated id is a no-op.
    pub fn free(&mut self, eid: u16) {
        self.live.remove(&eid);
    }

    /// Is `eid` currently allocated?
    pub fn contains(&self, eid: u16) -> bool {
        self.live.contains(&eid)
    }
}

/// The CEM tick block mirror: update number, interval and ticks/update budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickState {
    update_no: u32,
    interval_ms: u32,
    budget: u32,
}

impl Default for TickState {
    fn default() -> TickState {
        TickState::new()
    }
}

impl TickState {
    /// A tick state at the client constructor defaults (33 ms, budget 11).
    pub fn new() -> TickState {
        TickState {
            update_no: 0,
            interval_ms: TICK_INTERVAL_MS_DEFAULT,
            budget: TICK_BUDGET_DEFAULT,
        }
    }

    /// A tick state with explicit interval and budget.
    pub fn with_budget(interval_ms: u32, budget: u32) -> TickState {
        TickState {
            update_no: 0,
            interval_ms,
            budget,
        }
    }

    /// Advance the update number by one and return `(update_no, interval, budget)`.
    pub fn next_tick(&mut self) -> (u32, u32, u32) {
        self.update_no = self.update_no.wrapping_add(1);
        (self.update_no, self.interval_ms, self.budget)
    }

    /// The current `(update_no, interval, budget)`.
    pub fn snapshot(&self) -> (u32, u32, u32) {
        (self.update_no, self.interval_ms, self.budget)
    }
}

/// The zone token the `(13,0)` assign carries for the town.
///
/// `game.pkg`'s `town.zone` states `Name = Town`, and the client resolves
/// zone names case-insensitively, so the lowercase token `town` loads
/// `Town`'s world — the token the verified join sends.
pub const ZONE_TOWN: &str = "town";

/// The town `ZoneDef`'s own `Label` property (`Townston`), derived from
/// `town.zone`. Log-only; nothing on the wire depends on it.
pub const TOWN_ZONE_LABEL: &str = "Townston";

/// The town assign GC-type list: empty (session bootstrap constant, matching the live town shape).
pub const TOWN_ZONE_GC_TYPES: &[&str] = &[];

/// The assign trailer: the final `u32` after the GC-type list (four zero
/// bytes), read into `ZoneClient+0xDC`. The zone seed is a separate, preceding
/// `u32` (see [`assign_payload`]).
pub const TOWN_ZONE_TRAILER: [u8; 4] = [0; 4];

/// A `(13,0)` zone definition: the zone token plus its assign GC types and
/// trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneDef {
    pub zone: &'static str,
    pub gc_types: &'static [&'static str],
    pub trailer: &'static [u8],
}

/// The town zone definition.
pub const ZONE_DEF_TOWN: ZoneDef = ZoneDef {
    zone: ZONE_TOWN,
    gc_types: TOWN_ZONE_GC_TYPES,
    trailer: &TOWN_ZONE_TRAILER,
};

/// The assign GC-type list for `zone`.
///
/// The town is the only served zone, so every query answers the town list,
/// matching the default branch (`zone_gc_types`).
pub fn zone_gc_types(_zone: &str) -> &'static [&'static str] {
    TOWN_ZONE_GC_TYPES
}

/// The assign trailer for `zone`; the town trailer for every query.
pub fn zone_trailer(_zone: &str) -> &'static [u8] {
    &TOWN_ZONE_TRAILER
}

/// The zone definition for `zone`; the town definition for every query.
pub fn zone_def(_zone: &str) -> ZoneDef {
    ZONE_DEF_TOWN
}

/// The `(13,0)` assign payload for `zone`, built by
/// [`drf_proto::assign_payload`].
pub fn assign_payload(zone: &str) -> Result<Vec<u8>, JoinError> {
    let seed = if zone == ZONE_TOWN || zone == "tutorial" {
        0
    } else {
        drf_content::maze::layout_seed(zone)
    };
    assign_payload_with_seed(zone, seed)
}

/// The `(13,0)` assign with an explicit per-entry maze seed.
pub fn assign_payload_with_seed(zone: &str, seed: u32) -> Result<Vec<u8>, JoinError> {
    let def = zone_def(zone);
    // The seed the client stores at `ZoneClient+0xC8` and hands to
    // `World::initialize`. For a `Generated` world it is the maze RNG seed and
    // MUST equal the value the server used to compute the arrival pose, so the
    // server and client build the same maze. Non-generated zones ignore it.
    // Only a `Generated` world consumes the seed; the town and tutorial
    // assign the zero the original sent them.
    let seed = if zone == ZONE_TOWN || zone == "tutorial" {
        0
    } else {
        seed
    };
    drf_proto::assign_payload(zone, seed, def.gc_types, Some(def.trailer))
}

/// The two kinds of town static the flood places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticKind {
    Checkpoint,
    Waypoint,
}

/// A town static (`world.checkpoints.TownCheckpointEntity` or `misc.Waypoint`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TownStatic {
    pub entity_id: u16,
    pub classname: &'static str,
    pub kind: StaticKind,
    pub x: i32,
    pub y: i32,
}

/// A town NPC (`world.town.npc.*`). `base_w` is the shared facing word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TownNpc {
    pub entity_id: u16,
    pub classname: &'static str,
    pub base_x: i32,
    pub base_y: i32,
    pub base_w: i32,
}

/// The town `ZonePortal` entity. The destination grammar is deferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TownPortal {
    pub entity_id: u16,
    pub classname: &'static str,
}

/// A session's avatar entity record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvatarRecord {
    pub entity_id: u16,
    pub sqlid: u32,
    pub name: Vec<u8>,
    pub avatar_class: Vec<u8>,
}

/// A session's `player` entity record, stable across respawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerRecord {
    pub entity_id: u16,
    pub name: Vec<u8>,
}

/// A connection id. Opaque to this module.
pub type ConnId = u64;

/// The town checkpoint class (`town.world`): derived from `game.pkg`.
pub const TOWN_CHECKPOINT_CLASS: &str = "world.checkpoints.TownCheckpointEntity";

/// The town waypoint class (`town.world`): derived from `game.pkg`.
pub const TOWN_WAYPOINT_CLASS: &str = "misc.Waypoint";

/// The `ZonePortal` class the town create flood spells the portal as
/// (`GC_ZONEPORTAL`).
pub const PORTAL_CLASS: &str = "ZonePortal";

/// The old-man town NPC class: derived from `game.pkg`.
pub const NPC_CLASS_OLDMAN: &str = "world.town.npc.OldMan1";

/// The weapon vendor town NPC class: derived from `game.pkg`.
pub const NPC_CLASS_VENDOR_W: &str = "world.town.npc.VendorWeapon1";

/// The potion vendor town NPC class: derived from `game.pkg`.
pub const NPC_CLASS_VENDOR_P: &str = "world.town.npc.VendorPotion1";

/// E0: the town spawn the verified join uses, raw x256.
///
/// This is a session bootstrap value, not a `game.pkg`
/// placement: `town.world`'s own `misc.Waypoint Start` is at `(122880,
/// -48896)` on the wire.
pub const TOWN_SPAWN: (i32, i32) = (22970, 122378);

/// E0: the checkpoint pose, near [`TOWN_SPAWN`] (the pkg places
/// `world.checkpoints.TownCheckpointEntity` at `(113664, -43520)`).
pub const TOWN_CHECKPOINT_POS: (i32, i32) = (22970, 122378);

/// E0: the four town waypoint poses, near [`TOWN_SPAWN`].
pub const TOWN_WAYPOINT_POSITIONS: [(i32, i32); 4] = [
    (23970, 124378),
    (21970, 124378),
    (23970, 120378),
    (21970, 120378),
];

/// E0: the shared town NPC facing word.
pub const TOWN_NPC_W: i32 = -48891;

/// E0: the three seed town NPCs as `(class, x, y)`. The poses are
/// approximate town-adjacent coordinates, not the placements `town.world`
/// states.
pub const TOWN_NPC_DEFS: [(&str, i32, i32); 3] = [
    (NPC_CLASS_OLDMAN, 24970, 122378),
    (NPC_CLASS_VENDOR_W, 20970, 122878),
    (NPC_CLASS_VENDOR_P, 22970, 120378),
];

/// The town world: allocator, tick block and the town entity registry.
#[derive(Debug)]
pub struct World {
    pub entities: EntityAllocator,
    pub tick: TickState,
    town_statics: Vec<TownStatic>,
    town_npcs: Vec<TownNpc>,
    portals: Vec<TownPortal>,
    avatars: HashMap<ConnId, AvatarRecord>,
    players: HashMap<ConnId, PlayerRecord>,
    next_component: u16,
}

impl Default for World {
    fn default() -> World {
        World::new()
    }
}

impl World {
    /// Build the town world and allocate its statics, NPCs and portal.
    ///
    /// Allocation order matches the world constructor: NPCs, then the
    /// town portal, then the statics last so their ids sit at the end.
    pub fn new() -> World {
        let mut world = World {
            entities: EntityAllocator::new(),
            tick: TickState::new(),
            town_statics: Vec::new(),
            town_npcs: Vec::new(),
            portals: Vec::new(),
            avatars: HashMap::new(),
            players: HashMap::new(),
            next_component: COMPONENT_ID_MIN,
        };
        world.ensure_town_npcs();
        world.ensure_portals();
        world.ensure_town_statics();
        world
    }

    /// Allocate the next free entity id (`0x1000..`). Used for the zone's
    /// placed world entities as well as avatars and players.
    pub fn alloc_entity(&mut self) -> u16 {
        self.entities.alloc().expect("entity ID space exhausted")
    }

    /// Allocate the next component id (`0xE000..`). Component ids are never
    /// reused within a session, so a re-join does not re-issue an id the
    /// client already holds.
    pub fn alloc_component(&mut self) -> u16 {
        let id = self.next_component;
        self.next_component = if self.next_component == u16::MAX {
            COMPONENT_ID_MIN
        } else {
            self.next_component + 1
        };
        id
    }

    /// Allocate the three seed town NPCs once (idempotent).
    pub fn ensure_town_npcs(&mut self) -> &[TownNpc] {
        if self.town_npcs.is_empty() {
            let mut out = Vec::with_capacity(TOWN_NPC_DEFS.len());
            for &(classname, x, y) in TOWN_NPC_DEFS.iter() {
                let entity_id = self.alloc_entity();
                out.push(TownNpc {
                    entity_id,
                    classname,
                    base_x: x,
                    base_y: y,
                    base_w: TOWN_NPC_W,
                });
            }
            self.town_npcs = out;
        }
        &self.town_npcs
    }

    /// Allocate the town `ZonePortal` entity once (idempotent).
    pub fn ensure_portals(&mut self) -> &[TownPortal] {
        if self.portals.is_empty() {
            let entity_id = self.alloc_entity();
            self.portals.push(TownPortal {
                entity_id,
                classname: PORTAL_CLASS,
            });
        }
        &self.portals
    }

    /// Allocate the four town waypoints and one checkpoint once
    /// (idempotent).
    pub fn ensure_town_statics(&mut self) -> &[TownStatic] {
        if self.town_statics.is_empty() {
            let mut out = Vec::with_capacity(1 + TOWN_WAYPOINT_POSITIONS.len());
            let entity_id = self.alloc_entity();
            out.push(TownStatic {
                entity_id,
                classname: TOWN_CHECKPOINT_CLASS,
                kind: StaticKind::Checkpoint,
                x: TOWN_CHECKPOINT_POS.0,
                y: TOWN_CHECKPOINT_POS.1,
            });
            for &(x, y) in TOWN_WAYPOINT_POSITIONS.iter() {
                let entity_id = self.alloc_entity();
                out.push(TownStatic {
                    entity_id,
                    classname: TOWN_WAYPOINT_CLASS,
                    kind: StaticKind::Waypoint,
                    x,
                    y,
                });
            }
            self.town_statics = out;
        }
        &self.town_statics
    }

    /// Allocate a fresh avatar entity for the selecting character.
    ///
    /// `avatar_class` is the stored class FQN; an absent or empty value
    /// falls back to `Avatar`.
    pub fn spawn_avatar(
        &mut self,
        conn_id: ConnId,
        sqlid: u32,
        name: &[u8],
        avatar_class: Option<&[u8]>,
    ) -> u16 {
        let entity_id = self.alloc_entity();
        let class = avatar_class.map(asciiz_bytes).unwrap_or(&[]);
        let avatar_class = if class.is_empty() {
            b"Avatar".to_vec()
        } else {
            class.to_vec()
        };
        self.avatars.insert(
            conn_id,
            AvatarRecord {
                entity_id,
                sqlid,
                name: name.to_vec(),
                avatar_class,
            },
        );
        entity_id
    }

    /// Allocate the session's `player` entity.
    ///
    /// The record is the session's stable player identity; a respawn changes
    /// the avatar incarnation (a new [`spawn_avatar`](Self::spawn_avatar)
    /// entity) but not this binding, so the caller mints it once per session.
    pub fn spawn_player(&mut self, conn_id: ConnId, name: &[u8]) -> u16 {
        let entity_id = self.alloc_entity();
        self.players.insert(
            conn_id,
            PlayerRecord {
                entity_id,
                name: name.to_vec(),
            },
        );
        entity_id
    }

    /// The town statics, in allocation order (checkpoint first).
    pub fn town_statics(&self) -> &[TownStatic] {
        &self.town_statics
    }

    /// The town NPCs, in allocation order.
    pub fn town_npcs(&self) -> &[TownNpc] {
        &self.town_npcs
    }

    /// The town portal entities.
    pub fn portals(&self) -> &[TownPortal] {
        &self.portals
    }

    /// The avatar record for `conn_id`, if any.
    pub fn avatar(&self, conn_id: ConnId) -> Option<&AvatarRecord> {
        self.avatars.get(&conn_id)
    }

    /// The player record for `conn_id`, if any.
    pub fn player(&self, conn_id: ConnId) -> Option<&PlayerRecord> {
        self.players.get(&conn_id)
    }
}

/// The bytes up to the first NUL, or the whole slice when none.
fn asciiz_bytes(bytes: &[u8]) -> &[u8] {
    bytes.split(|&byte| byte == 0).next().unwrap_or(&[])
}
