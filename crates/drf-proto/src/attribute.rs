// SPDX-License-Identifier: GPL-3.0-or-later

//! The c2s attribute-allocation requests and their `(07,03)` confirmation.
//!
//! `Hero::spendAttributePoint` does not mutate the Hero: it builds a `DFCMessage`, writes the stat byte and
//! the point count, and calls `Entity::sendRequest`  with request
//! type `0x11`. `returnAttributePoint`  uses `0x12` and
//! `reSpecAttributePoints`  uses `0x13` with an empty body.
//!
//! `Entity::sendRequest` forwards to
//! `ClientEntityManager::sendRequest(Entity*,...)` , which writes
//! the entity update type `0x04`, the avatar's entity id (`Entity+0x80`) and
//! the request type, then appends the body. So the c2s inner is
//!
//! ```text
//! 07 04 | entityId:u16 | 11 | statType:u8 | numPoints:u8   (spend)
//! 07 04 | entityId:u16 | 12 | statType:u8 | numPoints:u8   (return)
//! 07 04 | entityId:u16 | 13                                (respec)
//! ```
//!
//! The confirmation the client waits for is a `processEntityUpdate`
//! (CEM tag `0x03`): it reads `u16 entityId`, `u8 updateType`,
//! calls `Hero::processUpdate`  and then consumes the mandatory
//! `EntitySynchInfo` trailer. `Hero::processUpdate` case `0x11` calls
//! `processUpdateSpendAttribPoint` , which reads `statType` then
//! `numPoints` and only then decrements `Hero+0x33c` and bumps the chosen
//! `Hero+0x334/0x336/0x338/0x33A` word. Hence the reply is
//!
//! ```text
//! 07 03 | entityId:u16 | 11 | statType:u8 | numPoints:u8 | 02 | hpWire:u32 | 06
//! ```
//!
//! `statType` is `0=Strength 1=Agility 2=Toughness 3=Power` (the order of the
//! `StrengthAdd`/`AgilityAdd`/`ToughnessAdd`/`PowerAdd` UI branches at
//! the client UI).

/// c2s entity-update type: `Entity::sendRequest` / `ClientEntityManager`.
pub const ENTITY_REQUEST_TYPE: u8 = 0x04;

/// `Hero::spendAttributePoint` request type.
pub const REQ_SPEND_ATTRIB: u8 = 0x11;

/// `Hero::returnAttributePoint` request type.
pub const REQ_RETURN_ATTRIB: u8 = 0x12;

/// `Hero::reSpecAttributePoints` request type (empty body).
pub const REQ_RESPEC: u8 = 0x13;

/// `statType` byte for Strength (`Hero+0x334`).
pub const STAT_STRENGTH: u8 = 0;
/// `statType` byte for Agility (`Hero+0x336`).
pub const STAT_AGILITY: u8 = 1;
/// `statType` byte for Toughness (`Hero+0x338`).
pub const STAT_TOUGHNESS: u8 = 2;
/// `statType` byte for Power (`Hero+0x33A`).
pub const STAT_POWER: u8 = 3;

/// Number of allocatable primary stats.
pub const STAT_COUNT: usize = 4;

/// The synch-trailer flag that carries the avatar HP word.
pub const SYNCH_FLAG_HP: u8 = 0x02;

/// A parsed c2s attribute request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeRequest {
    /// `0x11`: move `points` from the unspent pool into `stat`.
    Spend {
        entity_id: u16,
        stat: u8,
        points: u8,
    },
    /// `0x12`: refund `points` from `stat` into the unspent pool.
    Return {
        entity_id: u16,
        stat: u8,
        points: u8,
    },
    /// `0x13`: reset the allocation.
    Respec { entity_id: u16 },
}

impl AttributeRequest {
    /// The avatar entity id the request names.
    pub fn entity_id(self) -> u16 {
        match self {
            AttributeRequest::Spend { entity_id, .. }
            | AttributeRequest::Return { entity_id, .. }
            | AttributeRequest::Respec { entity_id } => entity_id,
        }
    }
}

/// Parse the payload after the `07 04` header.
///
/// Returns `None` for a truncated frame, an unassigned entity id, an unknown
/// request type, out-of-range `statType`, or a zero point count. Callers must
/// ignore `None` rather than guess.
pub fn parse_attribute_request(payload: &[u8]) -> Option<AttributeRequest> {
    if payload.len() < 3 {
        return None;
    }
    let entity_id = u16::from_le_bytes([payload[0], payload[1]]);
    if entity_id == 0 {
        return None;
    }
    match payload[2] {
        REQ_SPEND_ATTRIB | REQ_RETURN_ATTRIB => {
            if payload.len() < 5 {
                return None;
            }
            let stat = payload[3];
            let points = payload[4];
            if usize::from(stat) >= STAT_COUNT || points == 0 {
                return None;
            }
            if payload[2] == REQ_SPEND_ATTRIB {
                Some(AttributeRequest::Spend {
                    entity_id,
                    stat,
                    points,
                })
            } else {
                Some(AttributeRequest::Return {
                    entity_id,
                    stat,
                    points,
                })
            }
        }
        REQ_RESPEC => Some(AttributeRequest::Respec { entity_id }),
        _ => None,
    }
}

/// Build the `(07,03)` confirmation payload (everything after `07 03`).
///
/// `sub_type` is the request type being confirmed (`0x11` spend, `0x12`
/// return); `hp_wire` is the session's avatar x256 HP word. Returns `None` for
/// an unassigned entity/stat, a zero point count, or a sub-type that has no
/// stat body.
pub fn build_hero_stat_update(
    entity_id: u16,
    sub_type: u8,
    stat: u8,
    points: u8,
    hp_wire: u32,
) -> Option<Vec<u8>> {
    if entity_id == 0
        || !matches!(sub_type, REQ_SPEND_ATTRIB | REQ_RETURN_ATTRIB)
        || usize::from(stat) >= STAT_COUNT
        || points == 0
    {
        return None;
    }
    let mut out = Vec::with_capacity(11);
    out.extend_from_slice(&entity_id.to_le_bytes());
    out.push(sub_type);
    out.push(stat);
    out.push(points);
    out.push(SYNCH_FLAG_HP);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    out.push(crate::cem::END_MARKER);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_spend_request_from_the_client_wire() {
        // `07 04 <eid> 11 <stat> <points>`
        let payload = [0x00, 0x10, REQ_SPEND_ATTRIB, STAT_TOUGHNESS, 2];
        assert_eq!(
            parse_attribute_request(&payload),
            Some(AttributeRequest::Spend {
                entity_id: 0x1000,
                stat: STAT_TOUGHNESS,
                points: 2,
            })
        );
    }

    #[test]
    fn parses_return_and_respec() {
        assert_eq!(
            parse_attribute_request(&[0x01, 0x10, REQ_RETURN_ATTRIB, STAT_AGILITY, 1]),
            Some(AttributeRequest::Return {
                entity_id: 0x1001,
                stat: STAT_AGILITY,
                points: 1,
            })
        );
        assert_eq!(
            parse_attribute_request(&[0x01, 0x10, REQ_RESPEC]),
            Some(AttributeRequest::Respec { entity_id: 0x1001 })
        );
    }

    #[test]
    fn rejects_malformed_or_unknown_requests() {
        assert_eq!(parse_attribute_request(&[]), None);
        assert_eq!(
            parse_attribute_request(&[0x00, 0x10, REQ_SPEND_ATTRIB]),
            None
        );
        assert_eq!(
            parse_attribute_request(&[0x00, 0x10, REQ_SPEND_ATTRIB, STAT_POWER, 0]),
            None
        );
        assert_eq!(
            parse_attribute_request(&[0x00, 0x10, REQ_SPEND_ATTRIB, STAT_COUNT as u8, 1]),
            None
        );
        assert_eq!(parse_attribute_request(&[0x00, 0x00, REQ_RESPEC]), None);
        assert_eq!(parse_attribute_request(&[0x00, 0x10, 0x77]), None);
    }

    #[test]
    fn stat_update_is_the_verified_grammar() {
        let body = build_hero_stat_update(0x1014, REQ_SPEND_ATTRIB, STAT_STRENGTH, 3, 50_539)
            .expect("valid confirmation");
        let mut full = vec![0x07, 0x03];
        full.extend_from_slice(&body);
        assert_eq!(
            full,
            vec![
                0x07, 0x03, // channel 07, CEM_ENTITY_UPDATE
                0x14, 0x10, // eid 0x1014 LE
                0x11, // spend
                0x00, // Strength
                0x03, // points
                0x02, // HP-bearing synch trailer
                0x6b, 0xc5, 0x00, 0x00, // 50_539 LE
                0x06, // CEM end
            ]
        );
    }

    #[test]
    fn refuses_to_build_a_bodyless_or_zero_update() {
        assert!(build_hero_stat_update(0, REQ_SPEND_ATTRIB, STAT_STRENGTH, 1, 0).is_none());
        assert!(build_hero_stat_update(1, REQ_RESPEC, STAT_STRENGTH, 1, 0).is_none());
        assert!(build_hero_stat_update(1, REQ_SPEND_ATTRIB, STAT_COUNT as u8, 1, 0).is_none());
        assert!(build_hero_stat_update(1, REQ_SPEND_ATTRIB, STAT_STRENGTH, 0, 0).is_none());
    }
}
