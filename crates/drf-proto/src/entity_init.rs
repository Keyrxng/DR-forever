// SPDX-License-Identifier: GPL-3.0-or-later

//! The `0x02` processEntityInit class-state payloads for a zone's placed world
//! entities: the checkpoint (obelisk), the `misc.ZonePortal*` and the NPCs.
//!
//! A `0x02` record is `0x02 | u16 entityId | <class payload>` -- no GCType, no
//! count, no length, no terminator. The class payload is a FIXED, POSITIONAL
//! per-class struct whose width and field order are the client's own
//! `readInit` chains, each tail-calling its base:
//!
//! * `WorldEntity::readInit` @0x4D3E60: `u32 flags, u32 x, u32 y, u32 z,
//!   u32 aux, u8 fb`, then the gated tails `fb&0x01 -> u16`, `fb&0x02 -> u8`,
//!   `fb&0x04 -> u32`, `fb&0x08 -> u32`.
//! * `Unit::readInit` @0x50A580 (tail-calls `WorldEntity`): `u8 unitfb,
//!   u8 level(+0x314), u16(+0x316), u16(+0x318)`, then the gated tails.
//! * `StockUnit::readInit` @0x503BF0 (tail-calls `Unit`): ten UNCONDITIONAL
//!   reads, 25 bytes, in code order `+0x33E, +0x338, +0x33A, +0x33F, +0x33C,
//!   +0x334, +0x340, +0x320, +0x324, +0x328`.
//! * `ZonePortal::readInit` @0x5B0E9x (tail-calls `WorldEntity`): cstring
//!   spawnPoint, cstring zone, `u16 Width(+0xD0)`, `u16 Height(+0xD4)`,
//!   `u32 Color(+0xD8)`.
//! * `CheckpointEntity` overrides no `readInit`, so its payload is
//!   `WorldEntity`'s and nothing more (21 bytes at `fb = 0`).
//!
//! DERIVED-ONLY SCOPE. The pose comes from `game.pkg`'s `<zone>.world` and the
//! NPC level from the archetype's own file. Every field whose only source is a
//! capture ships its reader-neutral default here, named in each builder:
//!
//! * the `WorldEntity` `flags` word: only the binary-derived scene-attach bit
//!   ([`WORLD_ENTITY_FLAGS_ATTACH`]) is shipped; the capture values
//!   (`0x00028406` NPC / `0x00028407` checkpoint / `0x00000006` portal) are
//!   NOT copied, so every other bit is zero.
//! * `fb` / `anim_state`: `fb = 0` clears every gate (the reference captured
//!   `fb = 0x02` with `anim_state = 1` is not copied), so the payload is one
//!   byte shorter than the authored capture shape.
//! * `unitfb`, `u316`, `u318`: `0` (the reference also uses `0`).
//! * `StockUnit` `hero_respec` (reference `8`), `hero_power` (reference `3600`),
//!   `npc_scale` (reference `100`), `hero_toughness`/`hero_attribute`/
//!   `hero_strength`/`hero_dirty` (reference `0`): all `0`. These are capture
//!   constants with no `game.pkg` statement; the reference non-zero values are not
//!   copied.
//! * the `ZonePortal` `Width`/`Height`: the placement's own `Width`/`Height`
//!   when its world file states them (`town.world` states `70`/`60`); a
//!   placement that states neither takes the client archetype's constructor
//!   default [`PORTAL_DEFAULT_WIDTH`] (`0x28`), never `0`. The portal `Color`
//!   IS derived (`misc/ZonePortal_agg.gc` states it), and the
//!   `SpawnPoint`/`Zone` cstrings come from `<zone>.world`.
//!
//! The `StockUnit` tail's last three words repeat the entity's x/y/z, which
//! `StockUnit::updateState` @0x503B37 copies from `+0x90/+0x94/+0x98`; they
//! are the entity's own pose, so they are shipped, not zeroed.

use std::fmt;

use drf_content::{PlacedEntity, PlacedKind};

use crate::cem::TYPE_TAG_CLASSNAME;

/// The five `WorldEntity::readInit` u32 header words (flags, x, y, z, aux).
pub const WORLD_ENTITY_CLASS_STATE_HEADER_LEN: usize = 20;

/// `WorldEntity` flags word bit 2: the scene-attach gate
/// `WorldEntity::readInit` @0x4D3E60 sets on `EntityObject+0xC8` and
/// `SimpleMapRenderer::render` @0x4D8280 tests. Derived from the binary; a
/// clear bit gates the entity off.
pub const WORLD_ENTITY_FLAGS_ATTACH: u32 = 0x04;

/// `WorldEntity::WorldEntity` @0x4D2FB0 `orl $0x6,0xA0`: bits 1+2.
pub const WORLD_ENTITY_FLAGS_BASE: u32 = 0x0000_0006;
/// `Unit::Unit` @0x508C20 `orl $0x8000,0xA0`.
pub const UNIT_FLAGS: u32 = 0x0000_8000;
/// `StockUnit::StockUnit` @0x502D60 clears bit 16 and sets bit 17.
pub const STOCK_UNIT_FLAGS: u32 = 0x0002_0000;

/// A StockUnit-rooted NPC's `WorldEntity` flags word: the three constructors'
/// defaults (`0x6 | 0x8000 | 0x20000`). Bit 0 (the collision bit,
/// `WorldEntity::setBlocking` @0x4D33E0) is clear, which is faithful: no
/// tutorial NPC's archetype states `Blocking`.
pub const NPC_WORLD_ENTITY_FLAGS: u32 = WORLD_ENTITY_FLAGS_BASE | UNIT_FLAGS | STOCK_UNIT_FLAGS;

/// A `CheckpointEntity`'s flags word: the WorldEntity default plus the
/// Blocking bit, because `base/CheckpointEntity.gc` states `Blocking = true`.
pub const CHECKPOINT_WORLD_ENTITY_FLAGS: u32 = WORLD_ENTITY_FLAGS_BASE | 0x1;

/// A `ZonePortal`'s flags word: the WorldEntity default (no `Blocking` stated).
pub const PORTAL_WORLD_ENTITY_FLAGS: u32 = WORLD_ENTITY_FLAGS_BASE;

/// `StockUnit::StockUnit` @0x502DC0 loads `0x0E10` into `hero_power` (+0x33A).
pub const NPC_HERO_POWER: u16 = 3600;
/// `StockUnit::StockUnit` @0x502DB9 loads `0x64` into `+0x33F` (`npc_scale`).
pub const NPC_SCALE: u8 = 100;

/// The `ZonePortal::ZonePortal` @0x5B0AB0 constructor defaults for `Width`
/// (`movl $0x28,0xD0`) and `Height` (`movl $0x28,0xD4`): what a portal
/// inherits when its placement states neither. `ZonePortal::readInit`
/// @0x5B0F70 reads both from the class-state, and
/// `ZonePortalObject::VisNode::render` @0x5B0280 sizes and colours the drawn
/// quad from `+0xD0`/`+0xD4`/`+0xD8` -- so a zero renders nothing and
/// `ZonePortalObject::getDefaultBoundingBox` @0x5B0680 gives it no collision.
pub const PORTAL_DEFAULT_WIDTH: u16 = 0x28;
/// A portal's constructor default `Height`; see [`PORTAL_DEFAULT_WIDTH`].
pub const PORTAL_DEFAULT_HEIGHT: u16 = 0x28;

/// `fb` gate: a `u16` animation id follows.
pub const WE_TAIL_ANIMATION_ID: u8 = 0x01;
/// `fb` gate: a `u8` animation state follows.
pub const WE_TAIL_ANIMATION_STATE: u8 = 0x02;
/// `fb` gate: a `u32` follows.
pub const WE_TAIL_AUX_C: u8 = 0x04;
/// `fb` gate: a `u32` follows.
pub const WE_TAIL_AUX_D: u8 = 0x08;

/// The width of the position-only `CheckpointEntity` class state at `fb = 0`.
pub const CHECKPOINT_CLASS_STATE_LEN: usize = WORLD_ENTITY_CLASS_STATE_HEADER_LEN + 1;

/// The width of the position-only `StockUnit` NPC class state at `fb = 0`.
pub const NPC_CLASS_STATE_LEN: usize = WORLD_ENTITY_CLASS_STATE_HEADER_LEN + 1 + 6 + 25;

/// A class-state framing violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityInitError {
    /// A `fb` gate bit is set but no value for its tail was supplied.
    GatedTailMissing(u8),
    /// A field the `readInit` chain needs could not be derived.
    MissingField(&'static str),
}

impl fmt::Display for EntityInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntityInitError::GatedTailMissing(bit) => {
                write!(f, "fb gate bit {bit:#04x} is set but no value was given")
            }
            EntityInitError::MissingField(name) => {
                write!(f, "the class-state field {name} could not be derived")
            }
        }
    }
}

impl std::error::Error for EntityInitError {}

/// The `WorldEntity::readInit` class state: 20 B header + `fb` + gated tails.
///
/// `fb = 0` is the reader-neutral form (no gated bytes at all). A gate bit set
/// without its value is an error rather than a short payload the client would
/// read past.
#[allow(clippy::too_many_arguments)]
pub fn build_world_entity_class_state(
    flags: u32,
    x: u32,
    y: u32,
    z: u32,
    aux: u32,
    fb: u8,
    animation_id: Option<u16>,
    animation_state: Option<u8>,
    aux_c: Option<u32>,
    aux_d: Option<u32>,
) -> Result<Vec<u8>, EntityInitError> {
    let mut out = Vec::with_capacity(WORLD_ENTITY_CLASS_STATE_HEADER_LEN + 1 + 10);
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&z.to_le_bytes());
    out.extend_from_slice(&aux.to_le_bytes());
    out.push(fb);
    if fb & WE_TAIL_ANIMATION_ID != 0 {
        let value = animation_id.ok_or(EntityInitError::GatedTailMissing(WE_TAIL_ANIMATION_ID))?;
        out.extend_from_slice(&value.to_le_bytes());
    }
    if fb & WE_TAIL_ANIMATION_STATE != 0 {
        let value =
            animation_state.ok_or(EntityInitError::GatedTailMissing(WE_TAIL_ANIMATION_STATE))?;
        out.push(value);
    }
    if fb & WE_TAIL_AUX_C != 0 {
        let value = aux_c.ok_or(EntityInitError::GatedTailMissing(WE_TAIL_AUX_C))?;
        out.extend_from_slice(&value.to_le_bytes());
    }
    if fb & WE_TAIL_AUX_D != 0 {
        let value = aux_d.ok_or(EntityInitError::GatedTailMissing(WE_TAIL_AUX_D))?;
        out.extend_from_slice(&value.to_le_bytes());
    }
    Ok(out)
}

/// The `Unit::readInit` tail: `u8 unit_flags, u8 level, u16, u16`.
pub fn build_unit_class_state(level: u8, unit_flags: u8, u316: u16, u318: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(6);
    out.push(unit_flags);
    out.push(level);
    out.extend_from_slice(&u316.to_le_bytes());
    out.extend_from_slice(&u318.to_le_bytes());
    out
}

/// The `StockUnit::readInit` tail: ten unconditional reads, 25 bytes, in the
/// code order `+0x33E, +0x338, +0x33A, +0x33F, +0x33C, +0x334, +0x340,
/// +0x320, +0x324, +0x328` (not offset order).
#[allow(clippy::too_many_arguments)]
pub fn build_stock_unit_class_state(
    hero_respec: u8,
    hero_toughness: u16,
    hero_power: u16,
    npc_scale: u8,
    hero_attribute: u16,
    hero_strength: u32,
    hero_dirty: u8,
    x: u32,
    y: u32,
    z: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(25);
    out.push(hero_respec);
    out.extend_from_slice(&hero_toughness.to_le_bytes());
    out.extend_from_slice(&hero_power.to_le_bytes());
    out.push(npc_scale);
    out.extend_from_slice(&hero_attribute.to_le_bytes());
    out.extend_from_slice(&hero_strength.to_le_bytes());
    out.push(hero_dirty);
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&z.to_le_bytes());
    out
}

/// The 52-byte position-only class state for a StockUnit-rooted NPC.
///
/// `fb = 0`, `unitfb = 0`, and every `StockUnit` capture constant zeroed (see
/// the module note); only the pose and the derived `level` ride.
pub fn npc_class_state(pose: [u32; 4], level: u8) -> Result<Vec<u8>, EntityInitError> {
    let [x, y, z, heading] = pose;
    let mut out = build_world_entity_class_state(
        NPC_WORLD_ENTITY_FLAGS,
        x,
        y,
        z,
        heading,
        0,
        None,
        None,
        None,
        None,
    )?;
    out.extend_from_slice(&build_unit_class_state(level, 0, 0, 0));
    out.extend_from_slice(&build_stock_unit_class_state(
        0,
        0,
        NPC_HERO_POWER,
        NPC_SCALE,
        0,
        0,
        0,
        x,
        y,
        z,
    ));
    Ok(out)
}

/// The 21-byte `WorldEntity`-only class state for a non-Unit interactive
/// (`base.Door` / gates).
pub fn world_entity_interactive_class_state(
    pose: [u32; 4],
    blocking: bool,
) -> Result<Vec<u8>, EntityInitError> {
    let [x, y, z, heading] = pose;
    build_world_entity_class_state(
        WORLD_ENTITY_FLAGS_BASE | u32::from(blocking),
        x,
        y,
        z,
        heading,
        0,
        None,
        None,
        None,
        None,
    )
}

/// The `Door` class state: `WorldEntity` plus the two door bytes
/// (`Door::readInit` @0x5A6A10 tail-calls `WorldEntity` and reads two `u8`s).
pub fn door_class_state(pose: [u32; 4], blocking: bool) -> Result<Vec<u8>, EntityInitError> {
    let mut out = world_entity_interactive_class_state(pose, blocking)?;
    out.extend_from_slice(&[0, 0]);
    Ok(out)
}

/// The `WorldEntity + Unit` class state for a `base.NonCombatInteractive`
/// (chest, shrine, teleport, quest NCI). These archetypes carry a
/// `UnitBehavior`; their placement states no Level, so the Unit level byte is
/// the class's own zero rather than an invented value. `NCI::readInit`
/// @0x5A8E20 tail-calls `Unit::readInit`, then reads `u8`, `u8`, `u16`.
pub fn world_entity_unit_interactive_class_state(
    pose: [u32; 4],
    blocking: bool,
    track_activating_players: bool,
) -> Result<Vec<u8>, EntityInitError> {
    let [x, y, z, heading] = pose;
    let mut out = build_world_entity_class_state(
        WORLD_ENTITY_FLAGS_BASE | UNIT_FLAGS | u32::from(blocking),
        x,
        y,
        z,
        heading,
        0,
        None,
        None,
        None,
        None,
    )?;
    out.extend_from_slice(&build_unit_class_state(0, 0, 0, 0));
    out.extend_from_slice(&[0, 0, 0, 0]);
    if track_activating_players {
        // `NonCombatInteractive::readInit` loops on u32 activator ids until
        // `0x9327e8`, the zero-initialized BSS `CharSQLID::INVALID`. An empty
        // initial tracked list is `0`, not `-1`.
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    Ok(out)
}

/// The 21-byte `WorldEntity`-only class state for a `CheckpointEntity`.
pub fn checkpoint_class_state(pose: [u32; 4]) -> Result<Vec<u8>, EntityInitError> {
    let [x, y, z, heading] = pose;
    build_world_entity_class_state(
        CHECKPOINT_WORLD_ENTITY_FLAGS,
        x,
        y,
        z,
        heading,
        0,
        None,
        None,
        None,
        None,
    )
}

/// The `ZonePortal` class state: `WorldEntity`, the spawn/zone cstrings, then
/// `u16 Width`, `u16 Height`, `u32 Color`.
///
/// `spawn_point`/`zone` come from `<zone>.world` and `color` from the portal
/// archetype; `width`/`height` are the caller's derived values (the placement's
/// own, or [`PORTAL_DEFAULT_WIDTH`] when the placement states none).
pub fn zone_portal_class_state(
    pose: [u32; 4],
    spawn_point: &str,
    zone: &str,
    width: u16,
    height: u16,
    color: u32,
) -> Result<Vec<u8>, EntityInitError> {
    let [x, y, z, heading] = pose;
    let mut out = build_world_entity_class_state(
        PORTAL_WORLD_ENTITY_FLAGS,
        x,
        y,
        z,
        heading,
        0,
        None,
        None,
        None,
        None,
    )?;
    out.extend_from_slice(spawn_point.as_bytes());
    out.push(0);
    out.extend_from_slice(zone.as_bytes());
    out.push(0);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&color.to_le_bytes());
    Ok(out)
}

/// The `0x02` class-state payload for one placed world entity, by its kind.
///
/// A portal's `Width`/`Height` are its placement's own when `game.pkg` states
/// them; a placement that states neither takes the client archetype's
/// constructor default ([`PORTAL_DEFAULT_WIDTH`]). They are never zero: the
/// renderer draws the portal's quad at exactly `Width` x `Height`.
pub fn placed_entity_class_state(entity: &PlacedEntity) -> Result<Vec<u8>, EntityInitError> {
    match entity.kind {
        PlacedKind::Checkpoint => checkpoint_class_state(entity.pose),
        PlacedKind::Portal => zone_portal_class_state(
            entity.pose,
            entity.spawn_point.as_deref().unwrap_or(""),
            entity.zone.as_deref().unwrap_or(""),
            entity.width.unwrap_or(PORTAL_DEFAULT_WIDTH),
            entity.height.unwrap_or(PORTAL_DEFAULT_HEIGHT),
            entity.color.unwrap_or(0),
        ),
        PlacedKind::Npc => {
            let level = entity.level.ok_or(EntityInitError::MissingField("level"))?;
            npc_class_state(entity.pose, level)
        }
        PlacedKind::WorldEntity => {
            world_entity_interactive_class_state(entity.pose, entity.blocking)
        }
        PlacedKind::Door => door_class_state(entity.pose, entity.blocking),
        PlacedKind::WorldEntityUnit => world_entity_unit_interactive_class_state(
            entity.pose,
            entity.blocking,
            entity.track_activating_players,
        ),
    }
}

// -- the three NPC components ---------------------------------------------
//
// A placed NPC ships three nested `0x32` components next to its `0x02`. They
// are load-bearing: `MonsterBehavior2::BuildSkillLists` @0x51D6D0 dereferences
// the `Manipulators` child at 0x51D6FF with no null check, and a missing
// `Behavior` makes `Unit::getBehavior` @0x50ADF0 answer NULL and the click
// path fault at 0x52295A. Their class payloads are `game.pkg` facts; only the
// Behavior's state machine had capture-measured values, and that form is
// replaced by the state-less substitute below.

/// The `npc.Base.Manipulators` component classname.
pub const NPC_MANIPULATORS_CLASS: &str = "npc.Base.Manipulators";
/// The `npc.Base.Modifiers` component classname.
pub const NPC_MODIFIERS_CLASS: &str = "npc.Base.Modifiers";
/// The `npc.Base.Behavior` component classname.
pub const NPC_BEHAVIOR_CLASS: &str = "npc.Base.Behavior";

/// `UnitBehavior::readInit`'s own flag byte count (three).
const NPC_UB_FLAG_BYTES: usize = 3;

/// The `npc.Base.Manipulators` class payload, 54 bytes including the post
/// byte. `Manipulators::readInit` @0x4FD050 reads a `u8 count` and then, per
/// manipulator, `readType` (0xFF + FQN) and `Item::readData` @0x581710 +
/// `MeleeWeapon::readInit` @0x5923C0. `game.pkg`'s `npc/Base.gc` states
/// exactly one (`PrimaryWeapon extends creatures.base.weapons.melee`,
/// `ID = 10`), so `count = 1`, `fqn` is its instance name and `id` its ID.
/// The eleven-byte tail after the ID is that class's `readInit` body.
pub fn npc_manipulators_class_state(fqn: &str, id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(19 + fqn.len());
    out.push(0x01); // post byte
    out.push(0x01); // one manipulator (game.pkg)
    out.push(TYPE_TAG_CLASSNAME);
    out.extend_from_slice(fqn.as_bytes());
    out.push(0);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&[
        0xFF, 0xFF, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]);
    out
}

/// The `npc.Base.Modifiers` class payload, 10 bytes including the post byte.
///
/// `Modifiers::readInit` @0x502080 reads two u32s and a modifier count. The
/// `game.pkg` `npc/Base.gc` block is empty and the ctor @0x501110 zeroes the
/// words, so all three are neutral zero.
pub fn npc_modifiers_class_state() -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    out.push(0x01); // post byte
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0); // zero modifiers (the pkg block is empty)
    out
}

/// The state-less derived `npc.Base.Behavior` class payload, 38 bytes
/// including the post byte.
///
/// `MonsterBehavior2::readInit` @0x51B7C0 chains `UnitBehavior::readInit`
/// @0x51FE10 (`Behavior::readInit` @0x515970 then `UnitMover::ReadInit`
/// @0x535C20), `StateMachine::ReadMessage` @0x5F0C70, then its own tail. The
/// reference 76-byte interior embeds a four-entry state list whose u32 values are
/// capture-measured, so this ships the cleared substitute:
///
/// * `Behavior::readInit`: flag byte 0, both GcRef slots NULL (four zero u8).
/// * `UnitMover::ReadInit`: gate `0x00` (no gated byte, no u32 pair, no
///   `0x80` u32), the entity's heading twice, mode `1` (so the mode-2 branch
///   is skipped).
/// * `UnitBehavior::readInit`: three zero flag bytes, so bit 0 of `+0x156` is
///   clear and the rest of the chain is skipped.
/// * `StateMachine::ReadMessage`: gate `0x2E` -- bits `0x02/0x04/0x08/0x20`
///   read three zero u16 fields and a zero u32; bit `0x10` is CLEAR, so the
///   four-entry state list is omitted.
/// * `MonsterBehavior2` tail: gate `0x00`, then X and Y (no optional u16s).
pub fn npc_behavior_class_state(pose: [u32; 4]) -> Result<Vec<u8>, EntityInitError> {
    let [x, y, _z, heading] = pose;
    let mut out = Vec::with_capacity(38);
    out.push(0x01); // post byte
    out.extend_from_slice(&[0, 0, 0, 0]); // Behavior::readInit
    out.push(0x00); // UnitMover gate
    out.extend_from_slice(&heading.to_le_bytes());
    out.extend_from_slice(&heading.to_le_bytes());
    out.push(0x01); // UnitMover mode != 2
    out.extend_from_slice(&[0; NPC_UB_FLAG_BYTES]); // UnitBehavior flags
    out.push(0x2E); // StateMachine gate: bit 0x10 cleared
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0x00); // MonsterBehavior2 gate
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    Ok(out)
}

/// One nested `0x32` component record: `[0x32][owner][cid][0xFF][class][NUL]`
/// `[post][readInit]`, where `class_state[0]` is the post byte.
pub fn npc_component_record(
    owner: u16,
    component_id: u16,
    classname: &str,
    class_state: &[u8],
) -> Result<Vec<u8>, EntityInitError> {
    let Some((&post, body)) = class_state.split_first() else {
        return Err(EntityInitError::MissingField("component class state"));
    };
    Ok(crate::player::component_create_inner(
        owner,
        component_id,
        classname.as_bytes(),
        post,
        body,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_entity_gates_are_optional_and_fail_loud() {
        let zero = build_world_entity_class_state(0, 1, 2, 3, 4, 0, None, None, None, None)
            .expect("fb=0 has no tails");
        assert_eq!(zero.len(), 21);
        assert_eq!(&zero[20..], &[0]);
        let gated = build_world_entity_class_state(
            0,
            0,
            0,
            0,
            0,
            WE_TAIL_ANIMATION_STATE,
            None,
            Some(7),
            None,
            None,
        )
        .expect("anim state supplied");
        assert_eq!(&gated[20..], &[WE_TAIL_ANIMATION_STATE, 7]);
        assert_eq!(
            build_world_entity_class_state(
                0,
                0,
                0,
                0,
                0,
                WE_TAIL_ANIMATION_ID,
                None,
                None,
                None,
                None
            ),
            Err(EntityInitError::GatedTailMissing(WE_TAIL_ANIMATION_ID))
        );
    }

    #[test]
    fn class_state_widths_are_fixed() {
        assert_eq!(
            checkpoint_class_state([0; 4]).unwrap().len(),
            CHECKPOINT_CLASS_STATE_LEN
        );
        assert_eq!(
            npc_class_state([0; 4], 1).unwrap().len(),
            NPC_CLASS_STATE_LEN
        );
        assert_eq!(CHECKPOINT_CLASS_STATE_LEN, 21);
        assert_eq!(NPC_CLASS_STATE_LEN, 52);
    }

    #[test]
    fn npc_tail_repeats_the_pose_words() {
        let payload = npc_class_state([1, 2, 3, 4], 9).unwrap();
        // WorldEntity header x/y/z at 4/8/12.
        assert_eq!(&payload[4..8], &1u32.to_le_bytes());
        assert_eq!(&payload[8..12], &2u32.to_le_bytes());
        assert_eq!(&payload[12..16], &3u32.to_le_bytes());
        assert_eq!(&payload[16..20], &4u32.to_le_bytes());
        // Level at the Unit byte.
        assert_eq!(payload[22], 9);
        // StockUnit x/y/z at the tail's last three words.
        let tail = &payload[payload.len() - 12..];
        assert_eq!(&tail[0..4], &1u32.to_le_bytes());
        assert_eq!(&tail[4..8], &2u32.to_le_bytes());
        assert_eq!(&tail[8..12], &3u32.to_le_bytes());
    }
}
