// SPDX-License-Identifier: GPL-3.0-or-later

//! The c2s attack/use leg and its `(07,35)` acknowledgement.
//!
//! An attack is a `(07,34)` entity request whose inner is
//! `07 34 | <ent:u32> | 50 | <seq:u8> | <slot:u8> | <tgt:u16>` (11 bytes).
//! The client blocks until the server echoes the request body back behind a
//! `(07,35)` tag and appends the avatar's x256 HP word, exactly like the NPC
//! click echo. The reply is `07 35 <body[2:]> 02 <hp:u32> 06`.
//!
//! The subtype `0x51` is the client's UsePosition leg
//! (`07 34 <ent:u32> 51 <seq> 69 <X:i32> <Y:i32> <W:i32>`, 21 bytes). It names
//! no target entity, so [`parse_attack_use`] declines it; it must not be
//! mistaken for a targeted attack.

use crate::mover::{CEM_COMPONENT_UPDATE, ENTITY_CHANNEL, ENTITY_REQUEST};

/// The targeted attack/use subtype.
pub const ATTACK_SUB_USE: u8 = 0x50;
/// The position-only use subtype (no target entity).
pub const ATTACK_SUB_USE_POSITION: u8 = 0x51;
/// The self-cast (`Use`) subtype: buffs and AoE skills.
pub const ATTACK_SUB_SELF_CAST: u8 = 0x52;

/// The six-byte trailer of an attack ack: `02 <hp:u32 LE> 06`.
pub const ATTACK_ACK_FOOT_LEN: usize = 6;

/// A parsed c2s `07 34` targeted attack/use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttackUse {
    /// The entity the client attacked.
    pub target: u16,
    /// The action/skill slot the client used.
    pub slot: u8,
    /// The request subtype (currently always [`ATTACK_SUB_USE`]).
    pub subtype: u8,
}

/// Parse a c2s `07 34 | <ent:u32> | 50 | <seq> | <slot> | <tgt:u16>` attack.
///
/// Returns `None` for the position-only `0x51` leg (it has no target) and for
/// any other envelope shape.
pub fn parse_attack_use(inner: &[u8]) -> Option<AttackUse> {
    if inner.len() != 11
        || inner[0] != ENTITY_CHANNEL
        || inner[1] != ENTITY_REQUEST
        || inner[6] != ATTACK_SUB_USE
    {
        return None;
    }
    Some(AttackUse {
        target: u16::from_le_bytes([inner[9], inner[10]]),
        slot: inner[8],
        subtype: inner[6],
    })
}

/// A parsed c2s `07 34 <cid:u16> 01 <resp> 52 <session> <slot>` self-cast.
///
/// The client sends this for every `Use` skill (buffs and AoE). Without an
/// action ack the cast never resolves: no animation, no effect, and the client
/// retries. The effect itself is client-authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfCast {
    /// The acting unit's Behavior component id.
    pub component: u16,
    /// The action response id to echo.
    pub response: u8,
    /// The actor's rolling session byte.
    pub session: u8,
    /// The hotbar slot id the skill was cast from.
    pub slot: u8,
}

/// Parse a c2s `07 34 <cid:u16> 01 <resp> 52 <session> <slot>` self-cast.
///
/// Trailing `EntitySynchInfo` bytes are ignored; only the fixed nine-byte head
/// is required.
pub fn parse_self_cast(inner: &[u8]) -> Option<SelfCast> {
    if inner.len() < 9
        || inner[0] != ENTITY_CHANNEL
        || inner[1] != ENTITY_REQUEST
        || inner[4] != 0x01
        || inner[6] != ATTACK_SUB_SELF_CAST
    {
        return None;
    }
    Some(SelfCast {
        component: u16::from_le_bytes([inner[2], inner[3]]),
        response: inner[5],
        session: inner[7],
        slot: inner[8],
    })
}

/// A parsed c2s position cast (`0x51`): `07 34 <cid:u16> 01 <resp> 51
/// <session> <action_id> <x:u32> <y:u32> <z:u32>`, packed as the client's
/// `BehaviourActionUsePosition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionCast {
    pub component: u16,
    pub response: u8,
    pub session: u8,
    pub action_id: u8,
    pub x: u32,
    pub y: u32,
    pub z: u32,
}

/// Parse a c2s `07 34 <cid:u16> 01 <resp> 51 ...` position cast.
pub fn parse_position_cast(inner: &[u8]) -> Option<PositionCast> {
    if inner.len() < 21
        || inner[0] != ENTITY_CHANNEL
        || inner[1] != ENTITY_REQUEST
        || inner[4] != 0x01
        || inner[6] != ATTACK_SUB_USE_POSITION
    {
        return None;
    }
    let rest = &inner[10..21];
    let x = u32::from(inner[9])
        | (u32::from(rest[0]) << 8)
        | (u32::from(rest[1]) << 16)
        | (u32::from(rest[2]) << 24);
    let y = u32::from_le_bytes([rest[3], rest[4], rest[5], rest[6]]);
    let z = u32::from_le_bytes([rest[7], rest[8], rest[9], rest[10]]);
    Some(PositionCast {
        component: u16::from_le_bytes([inner[2], inner[3]]),
        response: inner[5],
        session: inner[7],
        action_id: inner[8],
        x,
        y,
        z,
    })
}

/// Build the `(07,35)` position-cast ack in the client's unpacked form.
pub fn position_cast_ack(inner: &[u8], hp_wire: u32) -> Vec<u8> {
    let Some(cast) = parse_position_cast(inner) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(27);
    out.extend_from_slice(&[ENTITY_CHANNEL, CEM_COMPONENT_UPDATE]);
    out.extend_from_slice(&inner[2..4]);
    out.push(0x01);
    out.push(cast.response);
    out.push(ATTACK_SUB_USE_POSITION);
    out.push(cast.session);
    out.push(cast.action_id);
    out.extend_from_slice(&cast.x.to_le_bytes());
    out.extend_from_slice(&cast.y.to_le_bytes());
    out.extend_from_slice(&cast.z.to_le_bytes());
    out.push(0x02);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    out.push(crate::cem::END_MARKER);
    out
}

/// Build the `(07,35)` self-cast ack: the nine-byte action head behind the
/// reply tag, then `02 <hp_wire:u32 LE> 06`.
pub fn self_cast_ack(inner: &[u8], hp_wire: u32) -> Vec<u8> {
    if inner.len() < 9 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(9 + ATTACK_ACK_FOOT_LEN);
    out.extend_from_slice(&[ENTITY_CHANNEL, CEM_COMPONENT_UPDATE]);
    out.extend_from_slice(&inner[2..9]);
    out.push(0x02);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    out.push(crate::cem::END_MARKER);
    out
}

/// Build the `(07,35)` attack ack: the request body from byte 2 onward behind
/// the reply tag, then `02 <hp_wire:u32 LE> 06`.
///
/// `hp_wire` is the AVATAR's x256 HP word (the same value the join's synch
/// trailers carry), not the attacked mob's HP.
pub fn attack_ack(c2s_inner: &[u8], hp_wire: u32) -> Vec<u8> {
    if c2s_inner.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(c2s_inner.len() + ATTACK_ACK_FOOT_LEN);
    out.extend_from_slice(&[ENTITY_CHANNEL, CEM_COMPONENT_UPDATE]);
    out.extend_from_slice(&c2s_inner[2..]);
    out.push(0x02);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    out.push(crate::cem::END_MARKER);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBSERVED: [u8; 11] = [
        0x07, 0x34, 0x3f, 0xe2, 0x01, 0x01, 0x50, 0x01, 0x0a, 0x7f, 0x10,
    ];

    #[test]
    fn parses_the_observed_attack_use() {
        let attack = parse_attack_use(&OBSERVED).expect("observed attack");
        assert_eq!(attack.target, 0x107f);
        assert_eq!(attack.slot, 0x0a);
        assert_eq!(attack.subtype, ATTACK_SUB_USE);
    }

    #[test]
    fn declines_the_position_only_subtype() {
        let mut inner = OBSERVED;
        inner[6] = ATTACK_SUB_USE_POSITION;
        assert!(parse_attack_use(&inner).is_none());
    }

    #[test]
    fn ack_replaces_the_tag_and_appends_the_hp_foot() {
        let ack = attack_ack(&OBSERVED, 51200);
        let mut expected = vec![0x07, 0x35];
        expected.extend_from_slice(&OBSERVED[2..]);
        expected.extend_from_slice(&[0x02, 0x00, 0xc8, 0x00, 0x00, 0x06]);
        assert_eq!(ack, expected);
    }

    #[test]
    fn parses_and_acks_a_self_cast() {
        let inner = [0x07, 0x34, 0x7f, 0xe1, 0x01, 0x02, 0x52, 0x00, 0x65];
        let cast = parse_self_cast(&inner).expect("observed self-cast");
        assert_eq!(cast.component, 0xE17F);
        assert_eq!(cast.response, 0x02);
        assert_eq!(cast.session, 0x00);
        assert_eq!(cast.slot, 0x65);
        let ack = self_cast_ack(&inner, 51200);
        let mut expected = vec![0x07, 0x35];
        expected.extend_from_slice(&inner[2..9]);
        expected.extend_from_slice(&[0x02, 0x00, 0xc8, 0x00, 0x00, 0x06]);
        assert_eq!(ack, expected);
    }

    #[test]
    fn parses_and_acks_a_position_cast() {
        let inner = [
            0x07, 0x34, 0x7f, 0xe1, 0x01, 0x02, 0x51, 0x00, 0x65, 0x00, 0x10, 0x00, 0x00, 0x20,
            0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x00,
        ];
        let cast = parse_position_cast(&inner).expect("position cast");
        assert_eq!(cast.action_id, 0x65);
        assert_eq!((cast.x, cast.y, cast.z), (0x1000, 0x20, 0x30));
        let ack = position_cast_ack(&inner, 51200);
        let mut expected = vec![0x07, 0x35, 0x7f, 0xe1, 0x01, 0x02, 0x51, 0x00, 0x65];
        expected.extend_from_slice(&0x1000u32.to_le_bytes());
        expected.extend_from_slice(&0x20u32.to_le_bytes());
        expected.extend_from_slice(&0x30u32.to_le_bytes());
        expected.extend_from_slice(&[0x02, 0x00, 0xc8, 0x00, 0x00, 0x06]);
        assert_eq!(ack, expected);
    }

    #[test]
    fn remove_frame_is_the_cem_entity_remove() {
        assert_eq!(
            crate::remove_frame(0x107f),
            vec![0x07, 0x05, 0x7f, 0x10, 0x06]
        );
    }
}
