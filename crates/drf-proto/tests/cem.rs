// SPDX-License-Identifier: GPL-3.0-or-later

//! Framing and body round-trips for the CEM record stream.

use drf_proto::{
    acquire_payload, create_body, frame_inner, frame_inner_with_end, interval_payload,
    interval_stream, item_create_body, minimal_entity_stream, parse_acquire_payload,
    parse_create_body, parse_item_create_body, parse_remove_inner, remove_inner, CemError,
    CreateError, CEM_CHANNEL, CEM_CREATE, CEM_INIT_SUB_ENTITIES, CONNECTED_CASE, END_MARKER,
    INTERVAL_PATHMANAGER_BLOCK, ITEMOBJECT_CLASS, TOWN_BUDGET_TRIPLE,
};

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[test]
fn a_batch_has_the_channel_first_tag_order_then_end() {
    let first = create_body(0x1234, "world.town.npc.OldMan1").unwrap();
    let second = create_body(0x1235, "world.town.npc.VendorWeapon1").unwrap();
    let inner = frame_inner(&[(CEM_CREATE, &first), (CEM_CREATE, &second)]).unwrap();

    assert_eq!(inner[0], CEM_CHANNEL);
    assert_eq!(inner[1], CEM_CREATE);
    assert_eq!(&inner[2..2 + first.len()], &first[..]);
    assert_eq!(inner[2 + first.len()], CEM_CREATE);
    assert_eq!(
        &inner[3 + first.len()..3 + first.len() + second.len()],
        &second[..]
    );
    assert_eq!(inner.last(), Some(&END_MARKER));
    assert_eq!(&inner[..3], &[0x07, 0x01, 0x34]);
}

#[test]
fn the_end_marker_can_be_replaced_or_suppressed() {
    let body = create_body(0x0042, "Avatar").unwrap();
    let connected = frame_inner_with_end(&[(CEM_CREATE, &body)], Some(CONNECTED_CASE)).unwrap();
    assert_eq!(connected.last(), Some(&CONNECTED_CASE));

    let open = frame_inner_with_end(&[(CEM_CREATE, &body)], None).unwrap();
    assert_eq!(&open[..2], &[CEM_CHANNEL, CEM_CREATE]);
    assert_eq!(open.len(), 2 + body.len());
}

#[test]
fn a_duplicated_header_byte_is_refused() {
    let body = create_body(0x0001, "Avatar").unwrap();
    let error = frame_inner(&[(CEM_CREATE, &body)]).unwrap_err();
    assert_eq!(error, CemError::DoubleTag(CEM_CREATE));

    let channel_prefix = [CEM_CHANNEL, 0x00];
    assert_eq!(
        frame_inner(&[(CEM_CREATE, &channel_prefix)]).unwrap_err(),
        CemError::DoubleTag(CEM_CHANNEL)
    );
    assert_eq!(frame_inner(&[]).unwrap_err(), CemError::EmptyBatch);
}

#[test]
fn create_bodies_round_trip() {
    let body = create_body(0x1234, "world.town.npc.OldMan1\0ignored").unwrap();
    let parsed = parse_create_body(&body).unwrap();
    assert_eq!(parsed.entity_id, 0x1234);
    assert_eq!(parsed.classname, b"world.town.npc.OldMan1");
    assert_eq!(&body[..2], &0x1234u16.to_le_bytes());
    assert_eq!(body[2], 0xFF);

    assert_eq!(
        create_body(0, "Avatar").unwrap_err(),
        CreateError::ZeroEntityId
    );
    assert_eq!(
        parse_create_body(&[0x34, 0x12, 0xFF, 0x41, 0x00, 0x00]).unwrap_err(),
        CreateError::TrailingBytes
    );
}

#[test]
fn item_create_bodies_round_trip() {
    let body = item_create_body(
        0x2001,
        "items.pal.1HMacePAL.Normal001",
        "tester1",
        1,
        7,
        1,
        0x22,
        0x33,
        0,
    )
    .unwrap();
    assert_eq!(&body[..2], &0x2001u16.to_le_bytes());
    assert!(body
        .windows(ITEMOBJECT_CLASS.len() + 1)
        .any(|window| window == b"ItemObject\0"));

    let parsed = parse_item_create_body(&body).unwrap();
    assert_eq!(parsed.entity_id, 0x2001);
    assert_eq!(parsed.classname, ITEMOBJECT_CLASS.as_bytes());
    assert_eq!(parsed.flags, 0);
    assert_eq!(parsed.hp, None);
    assert_eq!(parsed.state, 1);
    assert_eq!(parsed.slot, 7);
    assert_eq!((parsed.w1, parsed.w2, parsed.timer), (1, 0x22, 0x33));
    assert_eq!(parsed.lock, 0);
    assert_eq!(parsed.owner, b"tester1");
    assert_eq!(parsed.itemdesc, b"items.pal.1HMacePAL.Normal001");

    let no_owner = item_create_body(
        0x2002,
        "items.pal.1HMacePAL.Normal001",
        "x",
        1,
        0,
        0,
        0,
        0,
        0,
    )
    .unwrap();
    assert!(parse_item_create_body(&no_owner).unwrap().owner.is_empty());
    assert_eq!(
        item_create_body(0x2002, "desc", "", 1, 0, 1, 0, 0, 0).unwrap_err(),
        CreateError::EmptyOwner
    );
}

#[test]
fn remove_and_acquire_round_trip() {
    let inner = remove_inner(0x1234);
    assert_eq!(hex(&inner), "0705341206");
    assert_eq!(parse_remove_inner(&inner).unwrap(), 0x1234);

    let payload = acquire_payload(0x2001, 0x1234, "items.pal.1HMacePAL.Normal001").unwrap();
    let parsed = parse_acquire_payload(&payload).unwrap();
    assert_eq!(parsed.drop_id, 0x2001);
    assert_eq!(parsed.avatar_id, 0x1234);
    assert_eq!(parsed.itemdesc, b"items.pal.1HMacePAL.Normal001");
}

#[test]
fn interval_bodies_carry_the_documented_layout() {
    let payload = interval_payload(10_544_915, 33, 3);
    assert_eq!(payload.len(), 12);
    assert_eq!(&payload[..4], &10_544_915u32.to_le_bytes());
    assert_eq!(&payload[4..8], &33u32.to_le_bytes());
    assert_eq!(&payload[8..], &3u32.to_le_bytes());

    let entities = [(0x1234, "world.town.npc.OldMan1")];
    let stream = interval_stream(
        &entities,
        TOWN_BUDGET_TRIPLE,
        Some(&INTERVAL_PATHMANAGER_BLOCK),
        CONNECTED_CASE,
        &[],
        &[],
        &[],
        &[],
    )
    .unwrap();
    assert_eq!(&stream[..12], &payload[..]);
    assert_eq!(&stream[12..20], &INTERVAL_PATHMANAGER_BLOCK[..]);
    assert_eq!(stream[20], CEM_CREATE);
    assert_eq!(
        &stream[stream.len() - 2..],
        &[CEM_INIT_SUB_ENTITIES, CONNECTED_CASE]
    );
}

#[test]
fn an_entity_stream_rejects_a_foreign_component_owner() {
    let entities = [(0x1234, "Avatar")];
    let components = [(0x9999, &b"\x32"[..])];
    assert_eq!(
        minimal_entity_stream(&entities, END_MARKER, &components, &[], &[], &[]).unwrap_err(),
        CreateError::ComponentOwnerNotInStream(0x9999)
    );
    assert_eq!(
        minimal_entity_stream(&entities, 0x07, &[], &[], &[], &[]).unwrap_err(),
        CreateError::BadTerminator(0x07)
    );
}
