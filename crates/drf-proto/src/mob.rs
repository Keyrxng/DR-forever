// SPDX-License-Identifier: GPL-3.0-or-later

//! Monster (`world.<dungeon>.mob.*`) create/init/component builders.
//!
//! A dungeon mob is a `WorldEntity -> Unit -> StockUnit` entity, exactly like a
//! placed NPC, so its `0x02` class state reuses the same three-layer framing
//! (`WorldEntity::readInit` + `Unit::readInit` + `StockUnit::readInit`). The
//! differences from the NPC builders, and the three `0x35` component updates
//! that follow the components, mirror the reference
//! `dr_pyserver.managers.monsters._serialize_monster_stream`:
//!
//! * the `WorldEntity` flags word is `0x06` (activatable + visible), the value
//!   the reference ships for a monster (not the NPC constructor word);
//! * the `StockUnit` tail is all zero (no hero power / npc scale);
//! * FOUR nested components follow: the creature's `Behavior` class (a
//!   `creatures.base.behavior.*` subclass), `skills`, `manipulators` and
//!   `modifiers`. `Manipulators` is REQUIRED — `MonsterBehavior2::BuildSkillLists`
//!   dereferences it with no null check;
//! * a `0x35` SpawnAction and a `0x35` MoverUpdate carry the pose and the
//!   client-derivable max HP;
//! * a `0x35` FollowClient (`0x64`) enrollment ([`mob_enroll_payload`]) hands the
//!   mob's movement + combat to the client's own brain.
//!
//! ENROLL-AT-SPAWN. `UnitBehavior::UpdateMovement` branches on `UnitBehavior+0xE5` — when it is `4`
//! (client-controlled) it calls `UpdateFollowClient` and drives the
//! unit ONLY from inbound `0x65` MoverUpdate records. The `0x64` FollowClient
//! sub-op flips the mob to that state: `UnitBehavior::processUpdate` case `0x64`
//! sets `+0x156` bit 0 from the wire flag and, once no
//! action is active, calls `FollowClient` , setting
//! `+0xE5 = 4`. That hands the entity to the native `MonsterBehavior2` brain,
//! which runs its aggro scan, chase and `AttackTarget2` swing and — as the
//! enrolled, client-simulated owner — applies the player damage locally and
//! emits `0x65` movers on the mob's own behavior component. When the `0x64`
//! arrives while our `0x04` SpawnAction is still active, `FollowClient` is
//! deferred until `UnitBehavior::onActionStopped`  tests `+0x156` bit 0, so the enroll lands the moment the Spawn
//! action finishes either way.
//!
//! The client self-rolls the mob's HP once it sims it, so no later server
//! component update may assert the stale spawn HP; the install-time enrollment
//! carries the spawn word while the client's local value still agrees with it,
//! and the session then leaves the mob alone. The installed
//! `EntitySynchInfo::Validate` bypass (`[EDI+0xE5] != 1`) keeps both the
//! client-simulated mob and the avatar it damages out of the zero-tolerance HP
//! compare. Reference `dr_pyserver.managers.monsters` (`ENROLL_MONSTERS_AT_SPAWN`,
//! `build_monster_enroll_stream`).
//!
//! DERIVED-ONLY SCOPE. The behavior / skills bodies are the reference's fixed
//! shapes (itself a port of the C# `CombatPackets`), not re-derived
//! field-by-field from `game.pkg`. The manipulator list, by contrast, IS
//! derived: [`mob_manipulators_class_state`] ships the creature's own weapon
//! and skill objects resolved by [`drf_content::creature_manipulators`] from
//! its authored `Manipulators` block, so a ranged brain has a bow to fire and
//! a caster brain has a bolt to cast. A creature authoring no emittable
//! manipulator falls back to the generic base melee weapon
//! (`creatures.base.weapons.melee`, id 10).

use crate::cem::{END_MARKER, TYPE_TAG_CLASSNAME};
use crate::entity_init::{
    build_stock_unit_class_state, build_unit_class_state, build_world_entity_class_state,
    npc_component_record, npc_modifiers_class_state, EntityInitError,
};
use crate::mover::{follow_client_payload_wire, MoverError};
use drf_content::{CreatureManipulator, ManipulatorKind};

/// A monster's `WorldEntity` flags word: activatable (`0x02`) + visible/attach
/// (`0x04`). The reference `monsters.py` ships exactly this for a mob.
pub const MOB_WORLD_ENTITY_FLAGS: u32 = 0x0000_0006;

/// The `Behavior` classname a mob ships when its creature states none.
///
/// The reference `write_gc_type` lowercases every classname that is not an
/// entity/weapon name, so the canonical wire form is lowercase; the client's
/// `GCClassRegistry::getObjectDef` resolves it through a
/// `String::CaseInsensitiveLessThan` map either way.
pub const MOB_BEHAVIOR_CLASS: &str = "creatures.base.behavior.melee";

/// The `skills` component classname (reference `write_gc_type("skills")`).
pub const MOB_SKILLS_CLASS: &str = "skills";
/// The `manipulators` component classname.
pub const MOB_MANIPULATORS_CLASS: &str = "manipulators";
/// The `modifiers` component classname.
pub const MOB_MODIFIERS_CLASS: &str = "modifiers";

/// The width of a mob's position-only `0x02` class state (`fb = 0`).
pub const MOB_CLASS_STATE_LEN: usize =
    crate::entity_init::WORLD_ENTITY_CLASS_STATE_HEADER_LEN + 1 + 6 + 25;

/// A monster's `0x02` class state: `WorldEntity` (flags `0x06`, pose), the
/// `Unit` tail (level) and a zeroed `StockUnit` tail.
pub fn mob_class_state(pose: [u32; 4], level: u8) -> Result<Vec<u8>, EntityInitError> {
    mob_class_state_with_current_health(pose, level, None)
}

/// The three Q8 multipliers the client applies to a unit's own stats.
///
/// They ride the `Unit::readInit` `unit_flags` byte's bit `0x40` block:
/// health -> `Unit+0x2FC`, damage -> `Unit+0x300`, loot -> `Unit+0x304`, then
/// a one-byte group/loot cap (`Unit+0x2FB`). Health multiplies the max HP the
/// client derives from `UnitDesc`; damage multiplies its outgoing hit; loot is
/// only the target-tooltip value (real drops are server-authored).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnitScale {
    pub health: u16,
    pub damage: u16,
    pub loot: u16,
}

impl UnitScale {
    /// `0x100` in all three columns: no `0x40` block is emitted.
    pub const NORMAL: Self = Self {
        health: 0x100,
        damage: 0x100,
        loot: 0x100,
    };

    pub const fn is_normal(self) -> bool {
        self.health == 0x100 && self.damage == 0x100 && self.loot == 0x100
    }
}

/// A monster class state with an explicit x256 **current-HP** word.
///
/// `Unit::readInit` reads a `unit_flags` byte; bit `0x02` makes it consume a
/// following `u32` current-health word (`Unit+0x2F0`) instead of deriving it
/// from `UnitDesc::getMaxHealth(level)`. The **maximum** remains
/// `UnitDesc::getMaxHealth` (`Unit+0x14C`); use [`mob_class_state_scaled`] to
/// raise that maximum.
pub fn mob_class_state_with_current_health(
    pose: [u32; 4],
    level: u8,
    current_health: Option<u32>,
) -> Result<Vec<u8>, EntityInitError> {
    mob_class_state_scaled(pose, level, current_health, UnitScale::NORMAL)
}

/// A monster class state with explicit current HP and the client's own
/// health/damage/loot multipliers.
///
/// `Unit::readInit` reads the optional current-HP word before the `0x40`
/// block, so callers spawning a non-Normal mob should pass the already-scaled
/// full HP word here. The `0x40` block then makes `Unit::computeAttributes`
/// raise the client-derived max HP by `scale.health / 0x100`.
pub fn mob_class_state_scaled(
    pose: [u32; 4],
    level: u8,
    current_health: Option<u32>,
    scale: UnitScale,
) -> Result<Vec<u8>, EntityInitError> {
    let [x, y, z, heading] = pose;
    let mut out = build_world_entity_class_state(
        MOB_WORLD_ENTITY_FLAGS,
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
    let mut unit_flags = 0u8;
    if current_health.is_some() {
        unit_flags |= 0x02;
    }
    if !scale.is_normal() {
        unit_flags |= 0x40;
    }
    out.extend_from_slice(&build_unit_class_state(level, unit_flags, 0, 0));
    if let Some(current_health) = current_health {
        out.extend_from_slice(&current_health.to_le_bytes());
    }
    if !scale.is_normal() {
        out.extend_from_slice(&scale.health.to_le_bytes());
        out.extend_from_slice(&scale.damage.to_le_bytes());
        out.extend_from_slice(&scale.loot.to_le_bytes());
        out.push(1); // `Unit+0x2FB` group/loot cap; 1 keeps the tooltip at x1.
    }
    out.extend_from_slice(&build_stock_unit_class_state(0, 0, 0, 0, 0, 0, 0, 0, 0, 0));
    Ok(out)
}

/// The `Behavior` component's class state (post byte + body).
///
/// The reference's fixed 48-byte body: `Behavior::readInit` (`0xFF 00 00 00`),
/// `DFCStateMachine::readInit` (`0x85 00`, five zero u32, a zero byte),
/// `UnitBehavior::readInit` (`0xFF 00 00`) and `StateMachine::ReadMessage`
/// (`0x0F` + three `0xFFFF` + `0x10` + two zero u32 + `0x0001`).
pub fn mob_behavior_class_state() -> Vec<u8> {
    let mut out = Vec::with_capacity(49);
    out.push(0x01); // post byte
    out.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00]); // Behavior::readInit
    out.extend_from_slice(&[0x85, 0x00]); // DFCStateMachine::readInit
    out.extend_from_slice(&[0u8; 20]); // five zero u32
    out.push(0x00);
    out.extend_from_slice(&[0xFF, 0x00, 0x00]); // UnitBehavior::readInit
    out.push(0x0F); // StateMachine::ReadMessage gate
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.push(0x10);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0x0001u16.to_le_bytes());
    out
}

/// The `skills` component's class state (post byte + 6 bytes).
pub fn mob_skills_class_state() -> Vec<u8> {
    vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00]
}

/// The `manipulators` component's class state: the creature's OWN weapon(s)
/// and skill(s), one block per entry, or the generic base melee weapon when
/// the creature authors none (see [`drf_content::creature_manipulators`]).
///
/// The bodies are the reference `monsters.py` OP5 layouts, client-verified
/// 2026-06-11:
///
/// * `ActiveSkill:  <gcType> <id:u32> 0x00 <flags:0x00>`
/// * `MeleeWeapon:  <gcType> <id:u32> 6×0x00 <u16:0> 0x00 <u16:0>`
/// * `RangedWeapon: <gcType> <id:u32> 6×0x00 <u16:0> <u16:0>`
///
/// The count byte precedes the list and the classname tag is
/// [`TYPE_TAG_CLASSNAME`] (`0xFF`), the same encoding the other components use.
/// The gc type preserves its authored case (`write_gc_type(..., preserve_case=True)`).
pub fn mob_manipulators_class_state(entries: &[CreatureManipulator]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x01); // post byte
    out.push(entries.len() as u8); // manipulator count
    for entry in entries {
        out.push(TYPE_TAG_CLASSNAME);
        out.extend_from_slice(entry.gc_type.as_bytes());
        out.push(0);
        out.extend_from_slice(&entry.id.to_le_bytes());
        match entry.kind {
            ManipulatorKind::Skill => {
                // readData (u32 id + 1 byte) + readState flags byte (0 = no
                // optional fields). The C#-derived 5-byte shape omitted it.
                out.push(0x00);
                out.push(0x00);
            }
            ManipulatorKind::Melee => {
                // Weapon::readData 6 bytes + readState: +0x86 u16, melee-only
                // +0x8d byte, then the target u16 (0 = none).
                out.extend_from_slice(&[0u8; 6]);
                out.extend_from_slice(&0u16.to_le_bytes());
                out.push(0x00);
                out.extend_from_slice(&0u16.to_le_bytes());
            }
            ManipulatorKind::Ranged => {
                // Same readData, no melee-only byte.
                out.extend_from_slice(&[0u8; 6]);
                out.extend_from_slice(&0u16.to_le_bytes());
                out.extend_from_slice(&0u16.to_le_bytes());
            }
        }
    }
    out
}

/// The `modifiers` component's class state (identical bytes to the NPC form).
pub fn mob_modifiers_class_state() -> Vec<u8> {
    npc_modifiers_class_state()
}

/// One nested `0x32` component record (`[0x32][owner][cid][0xFF][class][NUL]`
/// `[post][readInit]`).
pub fn mob_component_record(
    owner: u16,
    component_id: u16,
    classname: &str,
    class_state: &[u8],
) -> Result<Vec<u8>, EntityInitError> {
    npc_component_record(owner, component_id, classname, class_state)
}

/// The `0x35` SpawnAction payload (after the `0x35` tag):
/// `[u16 behavior][0x04 0x04 0xFF][i32 x][i32 y][i32 z][u16 entity]`
/// `[0x02][u32 hp_wire]`.
pub fn mob_spawn_action_payload(
    behavior_id: u16,
    pose: [i32; 4],
    entity_id: u16,
    hp_wire: u32,
) -> Vec<u8> {
    let [x, y, z, _heading] = pose;
    let mut out = Vec::with_capacity(23);
    out.extend_from_slice(&behavior_id.to_le_bytes());
    out.extend_from_slice(&[0x04, 0x04, 0xFF]);
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&z.to_le_bytes());
    out.extend_from_slice(&entity_id.to_le_bytes());
    out.push(0x02);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    out
}

/// The `0x35` FollowClient enroll payload a mob's create stream carries, after
/// the `0x35` tag: `[u16 behavior][0x64][0x01][0x02][u32 hp_wire]`.
///
/// The full wire block is `0x35 <behaviorId:u16 LE> 0x64 0x01 0x02
/// <hpWire:u32 LE>` — the reference `monsters.py:663-674`
/// (`ENROLL_MONSTERS_AT_SPAWN`) and `build_monster_enroll_stream`, and exactly
/// [`follow_client_payload_wire`]'s `[u16 comp][0x64][on][0x02][u32 hp][0x06]`
/// grant with the stream-level `0x06` stripped. Inside a join create stream the
/// sole `0x06` is the batch terminator, so an embedded one would end the stream
/// early and truncate every record after it.
///
/// `0x64` is the `FollowClient` control-ON hand-off (`on == 0x01`, synch flag
/// `0x02`): it flips the unit to client-controlled so its native
/// `MonsterBehavior2` brain runs, and the trailer carries the mob's authored
/// max-HP **wire** word — the value the client itself derives at spawn, so the
/// zero-tolerance `EntitySynchInfo::Validate` compare holds.
pub fn mob_enroll_payload(behavior_id: u16, hp_wire: u32) -> Result<Vec<u8>, MoverError> {
    let record = follow_client_payload_wire(behavior_id, true, hp_wire)?;
    let body = record
        .strip_suffix(&[END_MARKER])
        .unwrap_or(record.as_slice());
    Ok(body.to_vec())
}

/// The `0x35` MoverUpdate payload (after the `0x35` tag):
/// `[u16 behavior][0x65 0x00 0x01 0x03][i32 heading][i32 x][i32 y]`
/// `[0x02][u32 hp_wire]`.
pub fn mob_mover_update_payload(behavior_id: u16, pose: [i32; 4], hp_wire: u32) -> Vec<u8> {
    let [x, y, _z, heading] = pose;
    let mut out = Vec::with_capacity(22);
    out.extend_from_slice(&behavior_id.to_le_bytes());
    out.extend_from_slice(&[0x65, 0x00, 0x01, 0x03]);
    out.extend_from_slice(&heading.to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.push(0x02);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mob_class_state_is_52_bytes_with_the_mob_flags_and_level() {
        let bytes = mob_class_state([1, 2, 3, 4], 2).unwrap();
        assert_eq!(bytes.len(), MOB_CLASS_STATE_LEN);
        assert_eq!(bytes.len(), 52);
        assert_eq!(&bytes[0..4], &MOB_WORLD_ENTITY_FLAGS.to_le_bytes());
        assert_eq!(&bytes[4..8], &1u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &2u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &3u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &4u32.to_le_bytes());
        assert_eq!(bytes[20], 0, "WorldEntity fb is clear");
        assert_eq!(bytes[21], 0, "unitfb is clear");
        assert_eq!(bytes[22], 2, "level rides the Unit byte");
    }

    #[test]
    fn scaled_mob_state_carries_full_hp_and_the_multiplier_block() {
        let hp = 43776u32; // 171 * 256
        let scale = UnitScale {
            health: 0x180,
            damage: 0x135,
            loot: 0x180,
        };
        let state = mob_class_state_scaled([1, 2, 3, 4], 2, Some(hp), scale).unwrap();
        let unit = 21;
        assert_eq!(state[unit], 0x42, "current HP + multipliers");
        assert_eq!(state[unit + 1], 2, "level");
        assert_eq!(&state[unit + 6..unit + 10], &hp.to_le_bytes());
        assert_eq!(&state[unit + 10..unit + 12], &0x180u16.to_le_bytes());
        assert_eq!(&state[unit + 12..unit + 14], &0x135u16.to_le_bytes());
        assert_eq!(&state[unit + 14..unit + 16], &0x180u16.to_le_bytes());
        assert_eq!(state[unit + 16], 1, "loot party cap");
        assert_eq!(state.len(), MOB_CLASS_STATE_LEN + 11);
    }

    #[test]
    fn component_states_have_the_reference_widths() {
        assert_eq!(mob_behavior_class_state().len(), 49);
        assert_eq!(mob_behavior_class_state()[0], 0x01);
        assert_eq!(
            mob_skills_class_state(),
            vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00]
        );
        assert_eq!(mob_modifiers_class_state().len(), 10);
    }

    #[test]
    fn manipulators_carry_the_base_melee_weapon_fallback() {
        let entries = [CreatureManipulator::default_melee()];
        let bytes = mob_manipulators_class_state(&entries);
        assert_eq!(bytes[0], 0x01, "post byte");
        assert_eq!(bytes[1], 0x01, "one manipulator");
        assert_eq!(bytes[2], 0xFF);
        let name = b"creatures.base.weapons.melee\0";
        assert_eq!(&bytes[3..3 + name.len()], name);
        let id_at = 3 + name.len();
        assert_eq!(&bytes[id_at..id_at + 4], &10u32.to_le_bytes());
        // 11-byte melee tail: 6×0, u16, melee byte, u16.
        assert_eq!(bytes.len(), id_at + 4 + 11);
    }

    #[test]
    fn manipulator_bodies_match_the_per_kind_layouts() {
        let skill = CreatureManipulator {
            gc_type: "skills.creature.CreatureRend".into(),
            id: 0,
            kind: ManipulatorKind::Skill,
        };
        let ranged = CreatureManipulator {
            gc_type: "creatures.base.weapons.ranged".into(),
            id: 10,
            kind: ManipulatorKind::Ranged,
        };
        let bytes = mob_manipulators_class_state(&[skill, ranged]);
        assert_eq!(bytes[0], 0x01, "post byte");
        assert_eq!(bytes[1], 0x02, "two manipulators");

        // Skill: `FF <name> 00 <id:u32> 00 00`.
        assert_eq!(bytes[2], 0xFF);
        let skill_name = b"skills.creature.CreatureRend\0";
        assert_eq!(&bytes[3..3 + skill_name.len()], skill_name);
        let skill_id = 3 + skill_name.len();
        assert_eq!(&bytes[skill_id..skill_id + 4], &0u32.to_le_bytes());
        assert_eq!(&bytes[skill_id + 4..skill_id + 6], &[0x00, 0x00]);

        // Ranged: `FF <name> 00 <id:u32>` then 6×0 + u16 + u16 (10 bytes).
        let ranged_at = skill_id + 6;
        assert_eq!(bytes[ranged_at], 0xFF);
        let ranged_name = b"creatures.base.weapons.ranged\0";
        assert_eq!(
            &bytes[ranged_at + 1..ranged_at + 1 + ranged_name.len()],
            ranged_name
        );
        let ranged_id = ranged_at + 1 + ranged_name.len();
        assert_eq!(&bytes[ranged_id..ranged_id + 4], &10u32.to_le_bytes());
        assert_eq!(&bytes[ranged_id + 4..], &[0u8; 10]);
        assert_eq!(bytes.len(), ranged_id + 4 + 10);
    }

    #[test]
    fn enroll_payload_is_the_follow_client_grant_without_the_stream_end() {
        // The exact reference block body: `64 01 02` then the x256 HP word.
        let enroll = mob_enroll_payload(0xE005, 29184).unwrap();
        assert_eq!(
            enroll,
            vec![
                0x05, 0xE0, // behavior component 0xE005
                0x64, // FollowClient sub-op
                0x01, // control ON
                0x02, // synch flag: the u32 HP word follows
                0x00, 0x72, 0x00, 0x00, // 29184 = 0x7200, LE
            ]
        );
        // No trailing `0x06`: the join batch owns the stream terminator.
        assert_ne!(*enroll.last().unwrap(), END_MARKER);
        // The avatar ComponentUpdate builder produces the identical `0x64`
        // grant bytes, so the two channels cannot drift.
        let grant = follow_client_payload_wire(0xE005, true, 29184).unwrap();
        assert_eq!(&grant[..grant.len() - 1], enroll.as_slice());
        assert_eq!(*grant.last().unwrap(), END_MARKER);
    }

    #[test]
    fn trailers_carry_the_pose_and_hp() {
        let pose = [0x100, -0x200, 0x300, 0x400];
        let action = mob_spawn_action_payload(0xE005, pose, 0x1010, 29184);
        assert_eq!(&action[0..2], &0xE005u16.to_le_bytes());
        assert_eq!(&action[2..5], &[0x04, 0x04, 0xFF]);
        assert_eq!(&action[5..9], &0x100i32.to_le_bytes());
        assert_eq!(&action[9..13], &(-0x200i32).to_le_bytes());
        assert_eq!(&action[13..17], &0x300i32.to_le_bytes());
        assert_eq!(&action[17..19], &0x1010u16.to_le_bytes());
        assert_eq!(action[19], 0x02);
        assert_eq!(&action[20..24], &29184u32.to_le_bytes());

        let mover = mob_mover_update_payload(0xE005, pose, 29184);
        assert_eq!(&mover[0..2], &0xE005u16.to_le_bytes());
        assert_eq!(&mover[2..6], &[0x65, 0x00, 0x01, 0x03]);
        assert_eq!(&mover[6..10], &0x400i32.to_le_bytes());
        assert_eq!(&mover[10..14], &0x100i32.to_le_bytes());
        assert_eq!(&mover[14..18], &(-0x200i32).to_le_bytes());
        assert_eq!(mover[18], 0x02);
        assert_eq!(&mover[19..23], &29184u32.to_le_bytes());
    }
}
