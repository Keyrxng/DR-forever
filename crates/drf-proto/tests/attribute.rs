// SPDX-License-Identifier: GPL-3.0-or-later

//! Wire-level round trip for the c2s attribute-allocation request and its
//! `(07,03)` confirmation.

use drf_proto::{
    build_hero_stat_update, parse_attribute_request, AttributeRequest, REQ_SPEND_ATTRIB,
    STAT_TOUGHNESS,
};

#[test]
fn spend_request_round_trips_into_the_confirmation() {
    // `Hero::spendAttributePoint` -> ClientEntityManager:
    // `07 04 <entity:u16> 11 <stat> <points>`.
    let inner = [0x07, 0x04, 0x00, 0x10, REQ_SPEND_ATTRIB, STAT_TOUGHNESS, 2];
    let request = parse_attribute_request(&inner[2..]).expect("spend request");
    assert_eq!(
        request,
        AttributeRequest::Spend {
            entity_id: 0x1000,
            stat: STAT_TOUGHNESS,
            points: 2,
        }
    );

    let AttributeRequest::Spend {
        entity_id,
        stat,
        points,
    } = request
    else {
        panic!("expected a spend");
    };
    let payload =
        build_hero_stat_update(entity_id, REQ_SPEND_ATTRIB, stat, points, 50_539).expect("ack");

    // `07 03 <entity:u16> 11 <stat> <points> 02 <hp:u32> 06`.
    let mut expected = vec![0x07, 0x03, 0x00, 0x10, 0x11, 0x02, 0x02, 0x02];
    expected.extend_from_slice(&50_539u32.to_le_bytes());
    expected.push(0x06);
    let mut actual = vec![0x07, 0x03];
    actual.extend_from_slice(&payload);
    assert_eq!(actual, expected);
}

#[test]
fn over_spend_cannot_be_expressed_as_a_confirmation() {
    // The parser rejects a zero count; the builder rejects an unassigned
    // entity, so an invalid request has no ack shape at all.
    assert!(parse_attribute_request(&[0x00, 0x10, REQ_SPEND_ATTRIB, 0, 0]).is_none());
    assert!(build_hero_stat_update(0, REQ_SPEND_ATTRIB, 0, 1, 0).is_none());
}
