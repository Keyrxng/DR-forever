// SPDX-License-Identifier: GPL-3.0-or-later

//! Char/join payload shapes and the parser round-trips the reference defines.

use drf_proto::{
    assign_payload, delete_ack_payload, delete_payload, group_connected_0930_payload,
    instance_count_payload, join_batch, parse_assign_payload, parse_ready_payload,
    parse_transfer_payload, ready_payload, ready_payload_with_explored, rosters_0301_payload,
    seed_payload, select_ack_payload, select_payload, transfer_payload,
    user_changed_group_0935_payload, JoinError, ASSIGN_TOWN_TRAILER, TRANSFER_REASON_DEFAULT,
};

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[test]
fn select_and_delete_bodies_are_the_documented_words() {
    assert_eq!(hex(&select_payload(831)), "3f030000");
    assert_eq!(hex(&delete_payload(831)), "3f030000");
    assert!(select_ack_payload(true).is_empty());
    assert_eq!(select_ack_payload(false), vec![0]);
    assert_eq!(hex(&delete_ack_payload(831, true)), "3f03000001");
    assert_eq!(hex(&delete_ack_payload(831, false)), "3f03000000");
}

#[test]
fn the_roster_burst_and_connected_bodies_are_fixed_shapes() {
    assert_eq!(hex(&rosters_0301_payload()), "01010000000000000000");
    assert_eq!(hex(&group_connected_0930_payload(831)), "3f0300000000");
}

#[test]
fn the_changed_group_body_carries_the_callers_session_word() {
    let head = [0xA9, 0x02, 0x00, 0x80];
    let body = user_changed_group_0935_payload(head, 831, "Keyrxng", 25);
    assert_eq!(&body[..4], &head);
    assert_eq!(
        hex(&body),
        "a90200803f030000000000000000000000000000013f0300004b657972786e67001900000001"
    );
    assert_eq!(&body[25..33], b"Keyrxng\0");
    let stripped = user_changed_group_0935_payload(head, 831, "Key\0rxng", 25);
    assert_eq!(&stripped[25..29], b"Key\0");
}

#[test]
fn assign_round_trips_with_and_without_types() {
    let town = assign_payload("town", 0, &[], None).unwrap();
    assert_eq!(hex(&town), "746f776e00000000000000000000");
    assert_eq!(&town[town.len() - 4..], &ASSIGN_TOWN_TRAILER);

    let parsed = parse_assign_payload(&town).unwrap();
    assert_eq!(parsed.zone, b"town");
    assert_eq!(parsed.seed, 0);
    assert!(parsed.gc_types.is_empty());
    assert_eq!(parsed.trailer, ASSIGN_TOWN_TRAILER);

    let with_types = assign_payload(
        "algate",
        0xDEAD_BEEF,
        &["world.town.npc.Bank"],
        Some(&[1, 2, 3, 4]),
    )
    .unwrap();
    let parsed = parse_assign_payload(&with_types).unwrap();
    assert_eq!(parsed.zone, b"algate");
    assert_eq!(parsed.seed, 0xDEAD_BEEF);
    assert_eq!(parsed.gc_types, vec![b"world.town.npc.Bank".to_vec()]);
    assert_eq!(parsed.trailer, vec![1, 2, 3, 4]);

    assert_eq!(
        parse_assign_payload(b"\x00\x00").unwrap_err(),
        JoinError::AssignEmptyZone
    );
    assert_eq!(
        parse_assign_payload(b"town").unwrap_err(),
        JoinError::AssignMissingZoneNul
    );
    assert_eq!(
        parse_assign_payload(b"town\x00").unwrap_err(),
        JoinError::AssignTruncatedAtCount
    );
}

#[test]
fn ready_round_trips_and_rejects_drift() {
    let body = ready_payload(831);
    assert_eq!(hex(&body), "3f0300000000");
    assert_eq!(parse_ready_payload(&body).unwrap().sqlid, 831);

    assert_eq!(
        parse_ready_payload(&[0, 0]).unwrap_err(),
        JoinError::ReadyLength(2)
    );
    assert_eq!(
        parse_ready_payload(&[1, 0, 0, 0, 1, 0]).unwrap_err(),
        JoinError::ReadyTrailingWord
    );

    // The memory-hungry form is the same grammar with a u16 count and that
    // many words, which is what `MiniMapExplored::ReadExploredBits` reads.
    let explored = [0x0000_0001u32, 0x8000_0000, 0xFFFF_FFFF];
    let body = ready_payload_with_explored(831, Some(&explored));
    assert_eq!(body.len(), 6 + 4 * explored.len());
    assert_eq!(&body[4..6], &3u16.to_le_bytes());
    for (index, word) in explored.iter().enumerate() {
        let at = 6 + index * 4;
        assert_eq!(&body[at..at + 4], &word.to_le_bytes());
    }
}

#[test]
fn count_and_seed_are_the_documented_words() {
    assert_eq!(hex(&instance_count_payload(0, 0)), "0000000000000000");
    assert_eq!(hex(&instance_count_payload(3, 2)), "0300000002000000");
    assert_eq!(hex(&seed_payload(0x1234_5678)), "78563412");
}

#[test]
fn transfer_is_one_nul_terminated_reason() {
    let body = transfer_payload(TRANSFER_REASON_DEFAULT).unwrap();
    assert_eq!(hex(&body), "7472616e7366657200");
    assert_eq!(parse_transfer_payload(&body).unwrap(), b"transfer");

    // The reason is NUL-trimmed, and the client ignores any trailing bytes.
    assert_eq!(
        hex(&transfer_payload(b"transfer\0JUNK").unwrap()),
        "7472616e7366657200"
    );
    assert_eq!(
        parse_transfer_payload(b"transfer\x00JUNK").unwrap(),
        b"transfer"
    );

    assert_eq!(
        transfer_payload(b"").unwrap_err(),
        JoinError::TransferReasonShape
    );
    assert_eq!(
        parse_transfer_payload(b"transfer").unwrap_err(),
        JoinError::TransferNotTerminated
    );
    assert_eq!(
        parse_transfer_payload(b"\x00").unwrap_err(),
        JoinError::TransferReasonShape
    );
}

#[test]
fn a_dungeon_assign_is_the_zone_plus_the_empty_town_shape() {
    // The generated destination has no GC types and the eight-zero trailer,
    // so the assign names the zone and nothing else -- the exact bytes the
    // reference's `build_assign_payload(b"dungeon00_level01", (), ZONE_TRAILER)`
    // produces.
    let body = assign_payload("dungeon00_level01", 0, &[], None).unwrap();
    assert_eq!(
        hex(&body),
        "64756e67656f6e30305f6c6576656c303100000000000000000000"
    );
    let parsed = parse_assign_payload(&body).unwrap();
    assert_eq!(parsed.zone, b"dungeon00_level01");
    assert!(parsed.gc_types.is_empty());
    assert_eq!(parsed.trailer, ASSIGN_TOWN_TRAILER);
}

#[test]
fn the_join_batch_frames_budgets_header_creates_and_terminator() {
    let first = drf_proto::create_body(0x1234, "Player").unwrap();
    let second = drf_proto::create_body(0x1235, "Avatar").unwrap();
    let inner = join_batch(
        (10_544_915, 33, 3),
        &[(0x01, &first), (0x08, &second)],
        0x46,
    );
    assert_eq!(
        hex(&inner),
        "070d13e7a00021000000030000000100000064001400013412ff506c6179657200083512ff417661746172001e46"
    );
    assert_eq!(&inner[..2], &[0x07, 0x0D]);
    assert_eq!(&inner[14..22], &drf_proto::INTERVAL_PATHMANAGER_BLOCK);
    // A CONNECTED terminator is preceded by the `0x1E` sub-entities marker.
    assert_eq!(&inner[inner.len() - 2..], &[0x1E, 0x46]);
}
