// SPDX-License-Identifier: GPL-3.0-or-later

//! Quest wire parity against the reference implementation in
//! The pinned bytes below are fixed test data.
//!
//! Outputs:
//! `offer 073506e004040131aee80006`
//! `add 073506e001040131aee81c000000000200526561643a2030202f20313100014b696c6c3a2033202f2033000006`
//! `add_all_done 073506e001040131aee81c000000010200526561643a2030202f20313100014b696c6c3a2033202f2033000006`
//! `add_placeholder 073506e001040131aee81c000000010101526561643a2031202f2031000006`
//! `add_pending 073506e001040131aee81c000000000100526561643a2030202f2031000006`
//! `objective 0 526561643a2030202f203131`
//!
//! The activate ack is NOT reference-pinned: the reference's `build_echo_0735`
//! appends a fixed HP foot that does not match the avatar's synch word, so the
//! test below pins the avatar-HP shape (`activate_ack_body`) instead.

use drf_proto::quest::END_MARKER;
use drf_proto::{
    activate_ack_body, build_quest_add_record, build_quest_offer_ack, parse_checkpoint_travel,
    parse_interact_click, parse_quest_abandon, parse_quest_accept, parse_quest_checkpoint,
    parse_quest_query, quest_gc_type, quest_objective, CheckpointTravel, InteractClick, QuestQuery,
    QUEST_PENDING_OBJECTIVE_PLACEHOLDER, QUEST_SUB_ACCEPT, QUEST_SUB_CLOSE, QUEST_SUB_QUERY,
    QUEST_SUB_SET_OFFERED, QUEST_ZERO_OBJECTIVE_PLACEHOLDER,
};

const QID: [u8; 5] = [0x04, 0x01, 0x31, 0xAE, 0xE8];
const REF: u16 = 0xE006;
const CLICK: [u8; 10] = [0x07, 0x34, 0x1F, 0xE0, 0x01, 0x01, 0x06, 0x01, 0x12, 0x10];

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedAdd {
    quest_ref: u16,
    qid: [u8; 5],
    instance_ref: u32,
    all_done: bool,
    objectives: Vec<(u8, Vec<u8>)>,
}

fn walk_add_record(inner: &[u8], expect_ref: u16) -> Option<ParsedAdd> {
    if inner.len() < 20 || inner[0] != 0x07 || inner[1] != 0x35 || inner[4] != QUEST_SUB_ACCEPT {
        return None;
    }
    if u16::from_le_bytes([inner[2], inner[3]]) != expect_ref
        || inner[inner.len() - 2] != 0x00
        || inner[inner.len() - 1] != END_MARKER
    {
        return None;
    }
    let qid: [u8; 5] = inner.get(5..10)?.try_into().ok()?;
    if qid[0] != 0x04 {
        return None;
    }
    let instance_ref = u32::from_le_bytes(inner.get(10..14)?.try_into().ok()?);
    let all_done = inner[14] != 0;
    let count = inner[15];
    let mut pos = 16;
    let mut objectives = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if pos >= inner.len() - 2 {
            return None;
        }
        let flags = inner[pos];
        if flags > 1 {
            return None;
        }
        let rest = &inner[pos + 1..inner.len() - 2];
        let end = rest.iter().position(|&b| b == 0)?;
        objectives.push((flags, rest[..end].to_vec()));
        pos += 1 + end + 1;
    }
    if pos != inner.len() - 2 {
        return None;
    }
    Some(ParsedAdd {
        quest_ref: expect_ref,
        qid,
        instance_ref,
        all_done,
        objectives,
    })
}

fn walk_offer_ack(inner: &[u8], expect_ref: u16) -> Option<[u8; 5]> {
    if inner.len() != 12
        || inner[0] != 0x07
        || inner[1] != 0x35
        || u16::from_le_bytes([inner[2], inner[3]]) != expect_ref
        || inner[4] != QUEST_SUB_SET_OFFERED
        || inner[10] != 0x00
        || inner[11] != END_MARKER
    {
        return None;
    }
    let qid: [u8; 5] = inner.get(5..10)?.try_into().ok()?;
    if qid[0] != 0x04 {
        return None;
    }
    Some(qid)
}

#[test]
fn constants_match_the_reference() {
    assert_eq!(QUEST_SUB_ACCEPT, 0x01);
    assert_eq!(QUEST_SUB_CLOSE, 0x02);
    assert_eq!(QUEST_SUB_SET_OFFERED, 0x04);
    assert_eq!(QUEST_SUB_QUERY, 0x06);
    assert_eq!(END_MARKER, 0x06);
    assert_eq!(
        QUEST_ZERO_OBJECTIVE_PLACEHOLDER,
        (1, b"Read: 1 / 1".as_slice())
    );
    assert_eq!(
        QUEST_PENDING_OBJECTIVE_PLACEHOLDER,
        (0, b"Read: 0 / 1".as_slice())
    );
}

#[test]
fn parse_quest_abandon_reads_the_instance() {
    let frame = [0x07, 0x34, 0x4C, 0xE0, 0x03, 0x14, 0x00, 0x00, 0x00];
    assert_eq!(parse_quest_abandon(&frame, 0xE04C), Some(20));
    assert_eq!(parse_quest_abandon(&frame, 0xE04D), None);
    assert!(parse_quest_abandon(&frame[..8], 0xE04C).is_none());
}

#[test]
fn obelisk_checkpoint_travel_names_the_selected_checkpoint_class() {
    let mut frame = vec![0x07, 0x34, 0x4C, 0xE0, 0x07, 0xFF];
    frame.extend_from_slice(b"world.checkpoints.TutorialCheckpoint\0");
    assert_eq!(
        parse_quest_checkpoint(&frame, 0xE04C),
        Some(CheckpointTravel::Class(
            "world.checkpoints.TutorialCheckpoint".to_string()
        ))
    );
    assert_eq!(
        parse_checkpoint_travel(&frame),
        Some(CheckpointTravel::Class(
            "world.checkpoints.TutorialCheckpoint".to_string()
        ))
    );
    assert_eq!(parse_quest_checkpoint(&frame, 0xE04D), None);
    let mut other = frame.clone();
    let start = 6;
    other.truncate(start);
    other.extend_from_slice(b"avatar.classes.Fighter\0");
    assert_eq!(parse_checkpoint_travel(&other), None);
    // The shipped client serializes the selected class as its GC hash.
    let hash_frame = [0x07, 0x34, 0x4C, 0xE0, 0x07, 0x04, 0xE0, 0x4D, 0xBB, 0xBB];
    assert_eq!(
        parse_checkpoint_travel(&hash_frame),
        Some(CheckpointTravel::Hash(0xBBBB_4DE0))
    );
    assert_eq!(
        parse_quest_checkpoint(&hash_frame, 0xE04C),
        Some(CheckpointTravel::Hash(0xBBBB_4DE0))
    );
}

#[test]
fn activate_ack_body_echoes_the_click_and_carries_the_avatar_hp() {
    // The shared activate ack: the click's own bytes from the componentId
    // onward, then the avatar's synch trailer `02 <hp:u32 LE>`. With the
    // avatar's x256 HP word (51200 = 0xC800) the bytes are
    // `1fe0010106011210 02 00c80000`.
    let body = activate_ack_body(&CLICK, 51200).expect("activate ack");
    assert_eq!(hex(&body), "1fe00101060112100200c80000");
    assert_eq!(&body[..8], &CLICK[2..], "the click bytes, 1:1");
    assert_eq!(&body[8..], &[0x02, 0x00, 0xC8, 0x00, 0x00], "avatar synch");

    assert!(activate_ack_body(&CLICK[..9], 51200).is_none());
    let mut miss = CLICK;
    miss[1] = 0x35;
    assert!(activate_ack_body(&miss, 51200).is_none());
}

#[test]
fn interact_click_accepts_the_real_shape_and_rejects_truncation() {
    assert_eq!(
        parse_interact_click(&CLICK),
        Some(InteractClick {
            component_id: 0xE01F,
            response_id: 0x01,
            target: 0x1012,
        })
    );
    assert!(parse_interact_click(&CLICK[..9]).is_none());

    let mut bad = CLICK;
    bad[4] = 0x02;
    assert!(parse_interact_click(&bad).is_none());
    let mut bad = CLICK;
    bad[6] = 0x05;
    assert!(parse_interact_click(&bad).is_none());
    let mut bad = CLICK;
    bad[0] = 0x35;
    assert!(parse_interact_click(&bad).is_none());
}

#[test]
fn quest_query_accepts_the_real_shape_and_rejects_truncation() {
    let inner = [
        0x07, 0x34, 0x06, 0xE0, 0x06, 0x01, 0x10, 0x00, 0x00, 0x04, 0x01, 0x31, 0xAE, 0xE8,
    ];
    assert_eq!(
        parse_quest_query(&inner, REF),
        Some(QuestQuery {
            component_id: REF,
            npc_entity_id: 0x1001,
            qid: QID,
        })
    );
    assert!(parse_quest_query(&inner[..13], REF).is_none());

    let mut bad = inner;
    bad[2] = 0x07;
    assert!(parse_quest_query(&bad, REF).is_none());
    let mut bad = inner;
    bad[4] = 0x04;
    assert!(parse_quest_query(&bad, REF).is_none());
    let mut bad = inner;
    bad[9] = 0x05;
    assert!(parse_quest_query(&bad, REF).is_none());
    assert!(parse_quest_query(&inner, 0).is_none());
}

#[test]
fn quest_accept_parses_both_steps_and_rejects_others() {
    assert_eq!(
        parse_quest_accept(&[0x07, 0x34, 0x06, 0xE0, 0x01], REF),
        Some(0x01)
    );
    assert_eq!(
        parse_quest_accept(&[0x07, 0x34, 0x06, 0xE0, 0x02], REF),
        Some(0x02)
    );
    assert!(parse_quest_accept(&[0x07, 0x34, 0x06, 0xE0, 0x03], REF).is_none());
    assert!(parse_quest_accept(&[0x07, 0x34, 0x06, 0xE0], REF).is_none());
    assert!(parse_quest_accept(&[0x07, 0x34, 0x07, 0xE0, 0x01], REF).is_none());
    assert!(parse_quest_accept(&[0x07, 0x34, 0x06, 0xE0, 0x01], 0).is_none());
}

#[test]
fn quest_gc_type_and_offer_ack_round_trip() {
    let hash = u32::from_le_bytes([0x01, 0x31, 0xAE, 0xE8]);
    assert_eq!(quest_gc_type(hash), QID);
    assert_eq!(hex(&quest_gc_type(hash)), "040131aee8");

    let offer = build_quest_offer_ack(&QID, REF);
    assert_eq!(hex(&offer), "073506e004040131aee80006");
    assert_eq!(walk_offer_ack(&offer, REF), Some(QID));
}

#[test]
fn objective_labels_match_the_reference() {
    assert_eq!(
        quest_objective("Read", None, None, false),
        (0, b"Read".to_vec())
    );
    let (flags, text) = quest_objective("Read", Some(0), Some(11), false);
    assert_eq!(flags, 0);
    assert_eq!(hex(&text), "526561643a2030202f203131");
    assert_eq!(text, b"Read: 0 / 11");
    assert_eq!(
        quest_objective("Read", Some(1), None, true),
        (1, b"Read: 1 / 1".to_vec())
    );
    assert_eq!(
        quest_objective("Read", None, Some(0), false),
        (0, b"Read: 0 / 1".to_vec())
    );
    assert_eq!(
        quest_objective("Read\u{0}ignored", None, None, false),
        (0, b"Read".to_vec())
    );
}

#[test]
fn add_record_round_trips_through_a_strict_inverse() {
    let objectives = vec![
        (0u8, b"Read: 0 / 11".to_vec()),
        (1u8, b"Kill: 3 / 3".to_vec()),
    ];
    let add = build_quest_add_record(&QID, REF, 0x1C, &objectives, None, None);
    assert_eq!(
        hex(&add),
        "073506e001040131aee81c000000000200526561643a2030202f20313100014b696c6c3a2033202f2033000006"
    );
    assert_eq!(
        walk_add_record(&add, REF),
        Some(ParsedAdd {
            quest_ref: REF,
            qid: QID,
            instance_ref: 0x1C,
            all_done: false,
            objectives,
        })
    );

    let forced = build_quest_add_record(&QID, REF, 0x1C, &[], Some(true), None);
    assert_eq!(
        hex(&forced),
        "073506e001040131aee81c000000010101526561643a2031202f2031000006"
    );
    assert_eq!(forced[14], 0x01);
    assert_eq!(forced[15], 0x01);
}

#[test]
fn empty_objectives_use_the_ready_or_pending_placeholder() {
    let pending = build_quest_add_record(&QID, REF, 0x1C, &[], None, Some(false));
    assert_eq!(
        hex(&pending),
        "073506e001040131aee81c000000000100526561643a2030202f2031000006"
    );
    let pending_walk = walk_add_record(&pending, REF).unwrap();
    assert_eq!(
        pending_walk.objectives,
        vec![(
            QUEST_PENDING_OBJECTIVE_PLACEHOLDER.0,
            QUEST_PENDING_OBJECTIVE_PLACEHOLDER.1.to_vec()
        )]
    );
    assert_eq!(pending[14], 0x00);

    let ready = build_quest_add_record(&QID, REF, 0x1C, &[], None, None);
    assert_eq!(
        hex(&ready),
        "073506e001040131aee81c000000010101526561643a2031202f2031000006"
    );
    let ready_walk = walk_add_record(&ready, REF).unwrap();
    assert_eq!(
        ready_walk.objectives,
        vec![(
            QUEST_ZERO_OBJECTIVE_PLACEHOLDER.0,
            QUEST_ZERO_OBJECTIVE_PLACEHOLDER.1.to_vec()
        )]
    );
    assert_eq!(ready[14], 0x01);
}
