// SPDX-License-Identifier: GPL-3.0-or-later

//! Bling Gnome (`creatures.summon.blinggnome.base.BlingGnome_Summon`) wire
//! builders.
//!
//! The shapes are the live-proven DRS-NET reference (`BlingGnomeRuntime` and
//! `BlingGnomeLockstepRuntime`): a StockUnit create/init whose owner bit is set,
//! three `0x32` components (Modifiers / Manipulators / the authored Behavior),
//! PlayAnimation `0x20`, MoverUpdate `0x65` and the ConvertItemsToGold `0xA1`
//! action. The sync trailer is always the GNOME's own HP, never the avatar's.

use crate::cem::END_MARKER;
use crate::entity_init::build_world_entity_class_state;
use crate::player::component_create_inner;

pub const GNOME_ENTITY_CLASS: &str = "creatures.summon.blinggnome.base.BlingGnome_Summon";
pub const GNOME_BEHAVIOR_SUFFIX: &str = ".Behavior";
pub const GNOME_MODIFIERS_CLASS: &str = "Modifiers";
pub const GNOME_MANIPULATORS_CLASS: &str = "Manipulators";

pub const GNOME_WORLD_ENTITY_FLAGS: u32 = 0x06;
pub const GNOME_UNIT_FLAGS: u8 = 0x16;

pub const ANIM_FIDGET_FIRST: u16 = 101;
pub const ANIM_FIDGET_LAST: u16 = 104;
pub const ANIM_PICKUP: u16 = 110;
pub const ANIM_ACTIVE_SKILL: u16 = 140;
pub const ANIM_DEATH: u16 = 150;
pub const ANIM_SPAWN: u16 = 180;

const ANIM_STATE_DIRECT: u8 = 0x00;
const ANIM_STATE_ACTIVE: u8 = 0x06;

const HENCHMAN_HEALTH_LEVELS: [i32; 7] = [1, 5, 10, 50, 75, 100, 110];
const HENCHMAN_HEALTH_VALUES: [f64; 7] =
    [245.0, 1550.0, 2810.0, 10989.0, 19646.0, 26009.0, 28020.0];

fn fixed_rounded(value: f64) -> i64 {
    (value * 256.0).round() as i64
}

fn curve_value(level: u8) -> f64 {
    let level = i32::from(level.max(1));
    if level <= HENCHMAN_HEALTH_LEVELS[0] {
        return HENCHMAN_HEALTH_VALUES[0];
    }
    for index in 1..HENCHMAN_HEALTH_LEVELS.len() {
        if level <= HENCHMAN_HEALTH_LEVELS[index] {
            let lo = HENCHMAN_HEALTH_LEVELS[index - 1];
            let hi = HENCHMAN_HEALTH_LEVELS[index];
            let t = f64::from(level - lo) / f64::from(hi - lo);
            return HENCHMAN_HEALTH_VALUES[index - 1]
                + (HENCHMAN_HEALTH_VALUES[index] - HENCHMAN_HEALTH_VALUES[index - 1]) * t;
        }
    }
    *HENCHMAN_HEALTH_VALUES.last().unwrap()
}

/// The gnome's max HP **wire** word: the henchman curve × the summon's authored
/// `MaxHealth` scale, rounded Fixed32 then shifted back, ×256.
pub fn gnome_hp_wire(level: u8, max_health_scale: f64) -> u32 {
    let curve = fixed_rounded(curve_value(level));
    let scale = fixed_rounded(max_health_scale.max(0.0));
    let hp = (curve * scale) >> 16;
    (hp.max(1) as u32) * 256
}

fn put_component(out: &mut Vec<u8>, owner: u16, cid: u16, class: &str, post: u8, body: &[u8]) {
    out.extend_from_slice(&component_create_inner(
        owner,
        cid,
        class.as_bytes(),
        post,
        body,
    ));
}

fn put_synch(out: &mut Vec<u8>, hp_wire: u32) {
    out.push(0x02);
    out.extend_from_slice(&hp_wire.to_le_bytes());
}

fn put_play_animation(
    out: &mut Vec<u8>,
    behavior_id: u16,
    hp_wire: u32,
    logical_id: u16,
    anim_state: u8,
    duration_ticks: u32,
) {
    let (state, arg) = if logical_id == ANIM_ACTIVE_SKILL {
        (ANIM_STATE_ACTIVE, 40)
    } else if (ANIM_FIDGET_FIRST..=ANIM_FIDGET_LAST).contains(&logical_id) {
        (ANIM_STATE_DIRECT, u32::from(logical_id))
    } else {
        (anim_state, u32::from(logical_id))
    };
    out.push(0x35);
    out.extend_from_slice(&behavior_id.to_le_bytes());
    out.extend_from_slice(&[0x04, 0x20, 0x00]);
    out.extend_from_slice(&u32::from(state).to_le_bytes());
    out.extend_from_slice(&arg.to_le_bytes());
    out.extend_from_slice(&duration_ticks.to_le_bytes());
    out.extend_from_slice(&0x3F80_0000u32.to_le_bytes());
    put_synch(out, hp_wire);
}

/// Entity create (`0x01`) + owner-bearing StockUnit init (`0x02`).
pub fn build_entity_snapshot(
    entity_id: u16,
    owner_ref: u16,
    level: u8,
    hp_wire: u32,
    pos: [i32; 3],
    heading: i32,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x07);
    out.push(0x01);
    out.extend_from_slice(&entity_id.to_le_bytes());
    out.push(0xFF);
    out.extend_from_slice(GNOME_ENTITY_CLASS.as_bytes());
    out.push(0);

    out.push(0x02);
    out.extend_from_slice(&entity_id.to_le_bytes());
    out.extend_from_slice(&GNOME_WORLD_ENTITY_FLAGS.to_le_bytes());
    for value in [pos[0], pos[1], pos[2], heading] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.push(0x00);
    let unit_flags = GNOME_UNIT_FLAGS | u8::from(owner_ref != 0);
    out.push(unit_flags);
    out.push(level);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    if owner_ref != 0 {
        out.extend_from_slice(&owner_ref.to_le_bytes());
    }
    out.extend_from_slice(&hp_wire.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0);
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(END_MARKER);
    out
}

/// The three `0x32` components plus the spawn animation, in one stream.
pub fn build_component_packet(
    entity_id: u16,
    modifiers_id: u16,
    manipulators_id: u16,
    behavior_id: u16,
    behavior_class: &str,
    anchor: [i32; 2],
    hp_wire: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x07);

    let mut modifiers = Vec::new();
    modifiers.extend_from_slice(&0u32.to_le_bytes());
    modifiers.extend_from_slice(&0u32.to_le_bytes());
    modifiers.push(0);
    put_component(
        &mut out,
        entity_id,
        modifiers_id,
        GNOME_MODIFIERS_CLASS,
        1,
        &modifiers,
    );

    put_component(
        &mut out,
        entity_id,
        manipulators_id,
        GNOME_MANIPULATORS_CLASS,
        1,
        &[0],
    );

    let mut behavior = Vec::new();
    behavior.extend_from_slice(&[0, 0, 0, 1, 0x42]);
    behavior.extend_from_slice(&anchor[0].to_le_bytes());
    behavior.extend_from_slice(&anchor[1].to_le_bytes());
    behavior.extend_from_slice(&[1, 0, 0, 0, 0x09, 0, 0]);
    behavior.extend_from_slice(&anchor[0].to_le_bytes());
    behavior.extend_from_slice(&anchor[1].to_le_bytes());
    put_component(
        &mut out,
        entity_id,
        behavior_id,
        behavior_class,
        1,
        &behavior,
    );

    put_play_animation(
        &mut out,
        behavior_id,
        hp_wire,
        ANIM_SPAWN,
        ANIM_STATE_DIRECT,
        24,
    );
    out.push(END_MARKER);
    out
}

/// A one-record walk / two-record terminal MoverUpdate `0x65`.
pub fn build_mover_update(
    behavior_id: u16,
    hp_wire: u32,
    old: [i32; 3],
    old_heading: i32,
    pos: [i32; 3],
    heading: i32,
    terminal: bool,
) -> Vec<u8> {
    let heading_changed = heading != old_heading;
    let moved = pos[0] != old[0] || pos[1] != old[1];
    let mut flags = u8::from(terminal);
    if heading_changed {
        flags |= 0x02;
    }
    if !terminal && moved {
        flags |= 0x06;
    }
    let mut out = Vec::new();
    out.push(0x35);
    out.extend_from_slice(&behavior_id.to_le_bytes());
    out.extend_from_slice(&[0x65, 0x00]);
    if terminal {
        out.push(0x02);
        out.push(0x06);
        out.extend_from_slice(&heading.to_le_bytes());
        out.extend_from_slice(&(pos[0] + 256).to_le_bytes());
        out.extend_from_slice(&pos[1].to_le_bytes());
        out.push((flags | 0x03) & 0x07);
        out.extend_from_slice(&heading.to_le_bytes());
        out.extend_from_slice(&pos[0].to_le_bytes());
        out.extend_from_slice(&pos[1].to_le_bytes());
    } else {
        out.push(0x01);
        out.push(flags & 0x07);
        out.extend_from_slice(&heading.to_le_bytes());
        out.extend_from_slice(&pos[0].to_le_bytes());
        out.extend_from_slice(&pos[1].to_le_bytes());
    }
    put_synch(&mut out, hp_wire);
    out
}

/// PlayAnimation `0x20` as its own stream.
pub fn build_play_animation(
    behavior_id: u16,
    hp_wire: u32,
    logical_id: u16,
    anim_state: u8,
    duration_ticks: u32,
) -> Vec<u8> {
    let mut out = vec![0x07];
    put_play_animation(
        &mut out,
        behavior_id,
        hp_wire,
        logical_id,
        anim_state,
        duration_ticks,
    );
    out.push(END_MARKER);
    out
}

/// ConvertItemsToGold action `0xA1` id 5, radius/centre/ratio and HP trailer.
pub fn build_convert_action(
    behavior_id: u16,
    hp_wire: u32,
    pos: [i32; 2],
    radius: u16,
    conversion_ratio: f64,
) -> Vec<u8> {
    let mut out = vec![0x07];
    out.push(0x35);
    out.extend_from_slice(&behavior_id.to_le_bytes());
    out.extend_from_slice(&[0x04, 0xA1, 0x00]);
    out.extend_from_slice(&5u32.to_le_bytes());
    out.extend_from_slice(&radius.to_le_bytes());
    out.extend_from_slice(&pos[0].to_le_bytes());
    out.extend_from_slice(&pos[1].to_le_bytes());
    out.extend_from_slice(&(fixed_rounded(conversion_ratio) as i32).to_le_bytes());
    put_synch(&mut out, hp_wire);
    out.push(END_MARKER);
    out
}

/// `07 05 <eid> 06` entity removal.
pub fn build_entity_remove(entity_id: u16) -> Vec<u8> {
    vec![
        0x07,
        0x05,
        (entity_id & 0xFF) as u8,
        (entity_id >> 8) as u8,
        END_MARKER,
    ]
}

/// Used by the constructor to keep `build_world_entity_class_state` linked with
/// the same WorldEntity flags the snapshot writes.
pub fn gnome_world_entity_class_state(pos: [u32; 4]) -> Vec<u8> {
    build_world_entity_class_state(
        GNOME_WORLD_ENTITY_FLAGS,
        pos[0],
        pos[1],
        pos[2],
        pos[3],
        0,
        None,
        None,
        None,
        None,
    )
    .expect("fb=0 has no gated tails")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hp_curve_matches_the_reference_fixed_point() {
        assert_eq!(gnome_hp_wire(1, 0.3), 73 * 256);
        assert!(gnome_hp_wire(3, 0.3) > gnome_hp_wire(1, 0.3));
        assert!(gnome_hp_wire(3, 0.3) < gnome_hp_wire(5, 0.3));
        assert_eq!(gnome_hp_wire(200, 0.3), gnome_hp_wire(110, 0.3));
    }

    #[test]
    fn snapshot_is_create_then_owner_bearing_stock_unit_init() {
        let packet = build_entity_snapshot(0x6001, 0x1234, 10, 0x1000, [1, 2, 3], 4);
        assert_eq!(&packet[..5], &[0x07, 0x01, 0x01, 0x60, 0xFF]);
        assert!(packet
            .windows(GNOME_ENTITY_CLASS.len() + 1)
            .any(|w| { w == [GNOME_ENTITY_CLASS.as_bytes(), &[0]].concat().as_slice() }));
        let init = 5 + GNOME_ENTITY_CLASS.len() + 1;
        assert_eq!(packet[init], 0x02, "init follows the class NUL");
        let unit = init + 1 + 2 + 4 + 4 * 4 + 1;
        assert_eq!(packet[unit], 0x17, "owner bit set");
        assert_eq!(packet[unit + 1], 10, "level");
        assert_eq!(&packet[packet.len() - 1..], &[END_MARKER]);
    }

    #[test]
    fn component_packet_carries_modifiers_manipulators_behavior_and_spawn_anim() {
        let packet = build_component_packet(
            0x6001,
            0x6005,
            0x6006,
            0x6007,
            "creatures.summon.blinggnome.base.BlingGnome_Summon.Behavior",
            [100, 200],
            0x1000,
        );
        assert_eq!(&packet[..7], &[0x07, 0x32, 0x01, 0x60, 0x05, 0x60, 0xFF]);
        assert_eq!(packet.iter().filter(|byte| **byte == 0x32).count(), 3);
        assert!(packet.windows(8).any(|w| w == b"Behavior"));
        assert!(packet.windows(4).any(|w| w == [0x04, 0x20, 0x00, 0x00]));
        assert_eq!(packet[packet.len() - 1], END_MARKER);
    }

    #[test]
    fn convert_action_is_the_a1_id5_shape() {
        let packet = build_convert_action(0x6007, 0x1000, [300 * 256, 400 * 256], 200, 0.5);
        assert_eq!(&packet[..7], &[0x07, 0x35, 0x07, 0x60, 0x04, 0xA1, 0x00]);
        assert_eq!(&packet[7..11], &5u32.to_le_bytes());
        assert_eq!(&packet[11..13], &200u16.to_le_bytes());
        assert_eq!(&packet[13..17], &(300 * 256i32).to_le_bytes());
        assert_eq!(&packet[17..21], &(400 * 256i32).to_le_bytes());
        assert_eq!(&packet[21..25], &128i32.to_le_bytes());
        assert_eq!(&packet[25..30], &[0x02, 0, 0x10, 0, 0]);
        assert_eq!(packet[30], END_MARKER);
    }
}
