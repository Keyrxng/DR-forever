// SPDX-License-Identifier: GPL-3.0-or-later

//! The join `0x08 player` create-init body and its nested managers.
//!
//! A `player` entity does NOT arrive as a character-graph node on the entity
//! wire. The client's `Player::readInit` (`0x5B3AB0`) reads the player's own
//! name as an ASCIIZ and then a fixed 27-byte tail; the body is
//! `[name NUL][tail]`. The graph node (`crate::graph`) is a different surface
//! -- it is the character-select/DFC-graph reply, and putting it here makes the
//! client read into the node and die at `GCClassRegistry::readType` with an
//! invalid type tag.
//!
//! The tail's `v1` word is entity-bound and never zero; its exact semantics are
//! unknown, so it is a caller-supplied value (the reference join uses `0x04A4`).
//! Every other field is fixed: a zero leading word, the `0x0180` invariant at
//! `+7`, the selected sqlid as a `u16`, twelve zero bytes, the minimum item
//! quality byte, and three zero bytes.
//!
//! The player's `QuestManager` and `DialogManager` are dereferenced every frame
//! with no NULL guard (`0x47C97D`/`0x47C995` -> `0x6258B2`), so a top-level
//! `0x32` is not enough: the two records must be NESTED inside this `0x08`
//! body, abutting, so the client links them as the player's children. Their
//! ids come from the world's component allocator, never a literal.

use crate::cem::TYPE_TAG_CLASSNAME;
use crate::quest::build_quest_add_record;

/// The `player` create-init classname (wire ASCIIZ, `REAL_PLAYER_CLASSNAME`).
pub const PLAYER_CLASSNAME: &[u8] = b"player";

/// The `0x08` create-init wire tag.
pub const CEM_CREATE_INIT: u8 = 0x08;

/// The `0x32` component-create wire tag.
pub const CEM_COMPONENT_CREATE: u8 = 0x32;

/// The player-init tail length in bytes.
pub const PLAYER_INIT_TAIL_LEN: usize = 27;

/// The tail's `+7` invariant word (`REAL_PLAYER_TAIL_V3`). Its low byte
/// (`0x80`) is fixed; its high byte is the permission byte at `+8`.
pub const PLAYER_TAIL_V3: u16 = 0x0180;

/// The tail's `+23` minimum item quality byte.
pub const PLAYER_MINIMUM_ITEM_QUALITY: u8 = 1;

/// The permission byte `Player+0xA0` for a normal account (`0x01`). The
/// client's `Avatar::IsAdmin` (`0x4EE6A0`) returns true only when that byte
/// is `0`, which is what gates `/admin` and `/adminui`.
pub const PLAYER_ROLE_PLAYER: u8 = 0x01;

/// The permission byte `Player+0xA0` that makes `Avatar::IsAdmin` true.
pub const PLAYER_ROLE_ADMIN: u8 = 0x00;

/// A body-shape violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerError {
    /// Entity id zero is the unassigned sentinel.
    ZeroEntityId,
    /// The player name is empty or longer than 20 bytes.
    BadName,
    /// A component id is zero, equals the owner, or repeats.
    BadComponentId,
}

impl std::fmt::Display for PlayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlayerError::ZeroEntityId => write!(f, "entity ID 0 is the unassigned sentinel"),
            PlayerError::BadName => write!(f, "player name must be 1..=20 bytes"),
            PlayerError::BadComponentId => {
                write!(
                    f,
                    "component id must be non-zero, distinct from the owner, and unique"
                )
            }
        }
    }
}

impl std::error::Error for PlayerError {}

/// The class payload a wire-`0x08` `player` createinit carries: `[name NUL]`
/// then the 27-byte tail. `v1` is the entity-bound word at tail `+4`; it is
/// never zero. The permission byte is [`PLAYER_ROLE_PLAYER`].
pub fn real_player_init_payload(name: &[u8], sqlid: u16, v1: u16) -> Result<Vec<u8>, PlayerError> {
    real_player_init_payload_with_role(name, sqlid, v1, PLAYER_ROLE_PLAYER)
}

/// [`real_player_init_payload`] with an explicit permission byte. Tail `+8`
/// is the standalone u8 the client reads into `Player+0xA0`; `0` is the
/// `Avatar::IsAdmin` state.
pub fn real_player_init_payload_with_role(
    name: &[u8],
    sqlid: u16,
    v1: u16,
    role: u8,
) -> Result<Vec<u8>, PlayerError> {
    if name.is_empty() || name.len() > 20 {
        return Err(PlayerError::BadName);
    }
    let mut out = Vec::with_capacity(name.len() + 1 + PLAYER_INIT_TAIL_LEN);
    out.extend_from_slice(name);
    out.push(0);
    out.extend_from_slice(&0u32.to_le_bytes()); // tail[0:4] zero
    out.extend_from_slice(&v1.to_le_bytes()); // tail[4:6] entity-bound word
    out.push(0); // tail[6] zero
    out.push((PLAYER_TAIL_V3 & 0xFF) as u8); // tail[7] fixed 0x80
    out.push(role); // tail[8] permission byte (Player+0xA0)
    out.extend_from_slice(&sqlid.to_le_bytes()); // tail[9:11]
    out.extend_from_slice(&[0u8; 12]); // tail[11:23] zero
    out.push(PLAYER_MINIMUM_ITEM_QUALITY); // tail[23]
    out.extend_from_slice(&[0u8; 3]); // tail[24:27] zero
    Ok(out)
}

/// A nested `0x32` component-create record (no channel prefix, no END): the
/// shape a record takes when embedded inside a larger CEM stream.
pub fn component_create_inner(
    owner: u16,
    component_id: u16,
    classname: &[u8],
    post_type_byte: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(CEM_COMPONENT_CREATE);
    out.extend_from_slice(&owner.to_le_bytes());
    out.extend_from_slice(&component_id.to_le_bytes());
    out.push(TYPE_TAG_CLASSNAME);
    out.extend_from_slice(classname);
    out.push(0);
    out.push(post_type_byte);
    out.extend_from_slice(payload);
    out
}

/// The offer directory block: `[giverCount:u8]` then per giver
/// `[class cstring][count:u8][0x04 u32 hash]...`. The same bytes the
/// `0x07` AvailableQuestUpdate body carries, serialised once
/// (`messages.quest_directory_body` @8230). `None`/empty is the empty block.
pub fn quest_directory_body(givers: &[(String, Vec<u32>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(givers.len() as u8);
    for (class, quests) in givers {
        out.extend_from_slice(class.as_bytes());
        out.push(0);
        out.push(quests.len() as u8);
        for qid in quests {
            out.push(0x04);
            out.extend_from_slice(&qid.to_le_bytes());
        }
    }
    out
}

/// The clean `QuestManager` record: the minimum that constructs.
///
/// The payload is `nextInstanceId=1`, an unsaved town portal (`Hello` /
/// `HelloAgain`), no alternate zone, empty zone-portal strings, the offer
/// directory block, and an empty journal + empty checkpoint block. The
/// interiors are grounded on `QuestManager::readInit` (`0x5C1C40`); no capture
/// is a source. `directory` is [`quest_directory_body`]'s bytes (empty for a
/// session with no corpus).
pub fn derived_quest_manager_record(owner: u16, component_id: u16, directory: &[u8]) -> Vec<u8> {
    derived_quest_manager_record_with_journal(owner, component_id, directory, 1, &[])
}

/// One instantiated quest the join installs in the shipped QuestManager
/// journal.
///
/// The record's bytes are the AddQuest record's own field order with its
/// `07 35 <qm> 01` head and `00 06` tail removed
/// (`messages.quest_journal_record` @19407), so the bytes an accept ships and
/// the bytes a re-login ships cannot drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    /// The five-byte GC quest id (`0x04` + the u32 hash).
    pub qid: [u8; 5],
    /// The store's own instance id, minted once at accept.
    pub instance_ref: u32,
    /// The `allComplete` byte the store recorded.
    pub all_done: bool,
    /// `(flags, label)` objectives, with the store's progress applied.
    pub objectives: Vec<(u8, Vec<u8>)>,
}

/// The `QuestManager` record with an explicit next-instance counter and a
/// replayed instantiated-quest journal.
///
/// The journal records are written between the instantiated-quest `u16` count
/// and the checkpoint block, which is the only place the client's
/// `QuestManager::readInit` reads them. `next_instance` is the monotone
/// allocator word the server will hand out next, so the wire word and the
/// session's own counter agree by construction.
pub fn derived_quest_manager_record_with_journal(
    owner: u16,
    component_id: u16,
    directory: &[u8],
    next_instance: u32,
    journal: &[JournalRecord],
) -> Vec<u8> {
    derived_quest_manager_record_with_journal_and_checkpoints(
        owner,
        component_id,
        directory,
        next_instance,
        journal,
        &[],
    )
}

/// [`derived_quest_manager_record_with_journal`] with the checkpoint list
/// the obelisk dialog reads: `u16 count` followed by each checkpoint's
/// `[0xFF][classname NUL]` GC type (`QuestManager::readInit`'s second loop).
pub fn derived_quest_manager_record_with_journal_and_checkpoints(
    owner: u16,
    component_id: u16,
    directory: &[u8],
    next_instance: u32,
    journal: &[JournalRecord],
    checkpoints: &[&str],
) -> Vec<u8> {
    derived_quest_manager_record_with_journal_checkpoints_and_recent_portal(
        owner,
        component_id,
        directory,
        next_instance,
        journal,
        QuestManagerTail::checkpoints(checkpoints),
    )
}

/// The optional QuestManager tail rows: the obelisk's checkpoint list and
/// its Recent Zone Portal entry. Grouping them keeps the wire builders below
/// the argument-count limit while making the pair explicit.
#[derive(Debug, Clone, Copy)]
pub struct QuestManagerTail<'a> {
    pub checkpoints: &'a [&'a str],
    pub recent_portal: Option<(&'a str, &'a str)>,
}

impl<'a> QuestManagerTail<'a> {
    pub fn checkpoints(checkpoints: &'a [&'a str]) -> Self {
        Self {
            checkpoints,
            recent_portal: None,
        }
    }
}

/// [`derived_quest_manager_record_with_journal_and_checkpoints`] with the
/// obelisk dialog's **Recent Zone Portal** row.
///
/// `QuestManager::readInit` reads three final strings before the offer
/// directory. The first is `AutoPortalZone` (the row is drawn only when it is
/// non-empty), the second is `AutoPortalSpawn`, and the third is an unnamed
/// legacy string. The row's "go" is the empty c2s `0x0C` request, so the
/// server owns the destination; the client only needs these two names for the
/// label and the gate.
pub fn derived_quest_manager_record_with_journal_checkpoints_and_recent_portal(
    owner: u16,
    component_id: u16,
    directory: &[u8],
    next_instance: u32,
    journal: &[JournalRecord],
    tail: QuestManagerTail<'_>,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&next_instance.to_le_bytes()); // next quest instance id
    payload.push(0); // town portal not saved
    payload.extend_from_slice(b"Hello\0"); // town portal default name
    payload.extend_from_slice(b"HelloAgain\0"); // town portal target
    payload.extend_from_slice(&0u32.to_le_bytes()); // town portal zone id
    payload.push(0); // alternate zone not enabled
    payload.push(0); // alternate zone name (empty)
    payload.push(0); // alternate zone default name (empty)
    payload.extend_from_slice(&0u32.to_le_bytes()); // alternate zone id
                                                    // The recent zone portal: AutoPortalZone, AutoPortalSpawn, then the
                                                    // unnamed legacy string. An empty first string is the client's
                                                    // "No Recent Zone Portal" state.
    if let Some((zone, spawn)) = tail.recent_portal.filter(|(zone, _)| !zone.is_empty()) {
        payload.extend_from_slice(zone.as_bytes());
        payload.push(0);
        payload.extend_from_slice(spawn.as_bytes());
        payload.push(0);
    } else {
        payload.push(0);
        payload.push(0);
    }
    payload.push(0); // unnamed legacy string (empty)
    if directory.is_empty() {
        payload.push(0); // available-quest giver count
    } else {
        payload.extend_from_slice(directory); // available-quest directory block
    }
    payload.extend_from_slice(&(journal.len() as u16).to_le_bytes()); // instantiated-quest count
    for entry in journal {
        // The AddQuest record's body is its `07 35 <qm> 01` head (5 B) and
        // `00 06` tail (2 B) removed.
        let add = build_quest_add_record(
            &entry.qid,
            component_id,
            entry.instance_ref,
            &entry.objectives,
            Some(entry.all_done),
            None,
        );
        if add.len() > 7 {
            payload.extend_from_slice(&add[5..add.len() - 2]);
        }
    }
    payload.extend_from_slice(&(tail.checkpoints.len() as u16).to_le_bytes()); // checkpoint count
    for checkpoint in tail.checkpoints {
        payload.push(TYPE_TAG_CLASSNAME);
        payload.extend_from_slice(checkpoint.as_bytes());
        payload.push(0);
    }
    component_create_inner(owner, component_id, b"QuestManager", 1, &payload)
}

/// The empty `DialogManager` record (zero-byte payload, post-type byte 1).
pub fn derived_dialog_manager_record(owner: u16, component_id: u16) -> Vec<u8> {
    component_create_inner(owner, component_id, b"DialogManager", 1, &[])
}

/// The nested `QuestManager` + `DialogManager` pair, abutting, in that order.
///
/// These ride inside the player's `0x08` body so the client links them as the
/// player's children; a top-level `0x32` never becomes a child and the
/// per-frame update crashes on the NULL.
pub fn derived_player_manager_components(
    owner: u16,
    quest_component_id: u16,
    dialog_component_id: u16,
    directory: &[u8],
) -> Result<Vec<u8>, PlayerError> {
    derived_player_manager_components_with_journal(
        owner,
        quest_component_id,
        dialog_component_id,
        directory,
        1,
        &[],
    )
}

/// The nested `QuestManager` + `DialogManager` pair with a replayed journal.
pub fn derived_player_manager_components_with_journal(
    owner: u16,
    quest_component_id: u16,
    dialog_component_id: u16,
    directory: &[u8],
    next_instance: u32,
    journal: &[JournalRecord],
) -> Result<Vec<u8>, PlayerError> {
    derived_player_manager_components_with_journal_and_checkpoints(
        owner,
        quest_component_id,
        dialog_component_id,
        directory,
        next_instance,
        journal,
        &[],
    )
}

/// [`derived_player_manager_components_with_journal`] plus the QuestManager
/// checkpoint list.
pub fn derived_player_manager_components_with_journal_and_checkpoints(
    owner: u16,
    quest_component_id: u16,
    dialog_component_id: u16,
    directory: &[u8],
    next_instance: u32,
    journal: &[JournalRecord],
    checkpoints: &[&str],
) -> Result<Vec<u8>, PlayerError> {
    derived_player_manager_components_with_journal_checkpoints_and_recent_portal(
        owner,
        quest_component_id,
        dialog_component_id,
        directory,
        next_instance,
        journal,
        QuestManagerTail::checkpoints(checkpoints),
    )
}

/// [`derived_player_manager_components_with_journal_and_checkpoints`] plus
/// the recent-zone-portal row of the obelisk dialog.
pub fn derived_player_manager_components_with_journal_checkpoints_and_recent_portal(
    owner: u16,
    quest_component_id: u16,
    dialog_component_id: u16,
    directory: &[u8],
    next_instance: u32,
    journal: &[JournalRecord],
    tail: QuestManagerTail<'_>,
) -> Result<Vec<u8>, PlayerError> {
    if owner == 0 {
        return Err(PlayerError::ZeroEntityId);
    }
    if quest_component_id == 0
        || dialog_component_id == 0
        || quest_component_id == dialog_component_id
        || quest_component_id == owner
        || dialog_component_id == owner
    {
        return Err(PlayerError::BadComponentId);
    }
    let mut out = derived_quest_manager_record_with_journal_checkpoints_and_recent_portal(
        owner,
        quest_component_id,
        directory,
        next_instance,
        journal,
        tail,
    );
    out.extend_from_slice(&derived_dialog_manager_record(owner, dialog_component_id));
    Ok(out)
}

/// The join `0x08 player` body (the payload after the record's `0x08` tag):
/// `[eid][0xFF]["player" NUL][name NUL][tail]` then the nested managers.
pub fn join_player_init_body(
    entity_id: u16,
    name: &[u8],
    sqlid: u16,
    v1: u16,
    components: &[u8],
) -> Result<Vec<u8>, PlayerError> {
    join_player_init_body_with_role(entity_id, name, sqlid, v1, PLAYER_ROLE_PLAYER, components)
}

/// [`join_player_init_body`] with an explicit permission byte (`Player+0xA0`).
/// [`PLAYER_ROLE_ADMIN`] makes `Avatar::IsAdmin` true so the client's
/// `/adminui` and `/admin` surfaces arm.
pub fn join_player_init_body_with_role(
    entity_id: u16,
    name: &[u8],
    sqlid: u16,
    v1: u16,
    role: u8,
    components: &[u8],
) -> Result<Vec<u8>, PlayerError> {
    if entity_id == 0 {
        return Err(PlayerError::ZeroEntityId);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&entity_id.to_le_bytes());
    out.push(TYPE_TAG_CLASSNAME);
    out.extend_from_slice(PLAYER_CLASSNAME);
    out.push(0);
    out.extend_from_slice(&real_player_init_payload_with_role(name, sqlid, v1, role)?);
    out.extend_from_slice(components);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_payload_is_name_plus_27_byte_tail() {
        let body = real_player_init_payload(b"Hero", 1, 0x04A4).unwrap();
        assert_eq!(body.len(), 5 + 27);
        assert_eq!(&body[..5], b"Hero\0");
        let tail = &body[5..];
        assert_eq!(&tail[0..4], &[0, 0, 0, 0]);
        assert_eq!(&tail[4..6], &0x04A4u16.to_le_bytes());
        assert_eq!(tail[6], 0);
        assert_eq!(&tail[7..9], &0x0180u16.to_le_bytes());
        assert_eq!(&tail[9..11], &1u16.to_le_bytes());
        assert_eq!(&tail[11..23], &[0u8; 12]);
        assert_eq!(tail[23], PLAYER_MINIMUM_ITEM_QUALITY);
        assert_eq!(&tail[24..27], &[0u8; 3]);
    }

    #[test]
    fn player_role_byte_is_tail_8_only() {
        let player = real_player_init_payload(b"Hero", 1, 0x04A4).unwrap();
        let admin =
            real_player_init_payload_with_role(b"Hero", 1, 0x04A4, PLAYER_ROLE_ADMIN).unwrap();
        assert_eq!(player[5 + 8], PLAYER_ROLE_PLAYER);
        assert_eq!(admin[5 + 8], PLAYER_ROLE_ADMIN);
        assert_eq!(player.len(), admin.len());
        for (index, (a, b)) in player.iter().zip(admin.iter()).enumerate() {
            if index == 5 + 8 {
                continue;
            }
            assert_eq!(a, b, "role must only move tail[8] (index {index})");
        }
    }

    #[test]
    fn player_name_bounds() {
        assert_eq!(
            real_player_init_payload(b"", 1, 1),
            Err(PlayerError::BadName)
        );
        assert!(real_player_init_payload(&[b'a'; 20], 1, 1).is_ok());
        assert_eq!(
            real_player_init_payload(&[b'a'; 21], 1, 1),
            Err(PlayerError::BadName)
        );
    }

    // The nested pair is 82 bytes for the empty state: a 61-byte QuestManager
    // record abutting a 21-byte DialogManager record.
    #[test]
    fn derived_manager_pair_is_82_bytes_and_abuts() {
        let pair = derived_player_manager_components(0x1000, 0xE000, 0xE001, &[]).unwrap();
        assert_eq!(pair.len(), 82);
        assert_eq!(pair[0], CEM_COMPONENT_CREATE);
        // QuestManager record = 61 bytes, so the DialogManager starts at 61.
        assert_eq!(pair[61], CEM_COMPONENT_CREATE);
        assert_eq!(&pair[62..64], &0x1000u16.to_le_bytes()); // owner
        assert_eq!(&pair[64..66], &0xE001u16.to_le_bytes()); // dialog id
        assert_eq!(pair[66], TYPE_TAG_CLASSNAME);
        assert_eq!(&pair[67..81], b"DialogManager\0");
        assert_eq!(pair[81], 1);
    }

    // The directory block is `[giverCount][class cstring][count][04 u32]...`.
    #[test]
    fn directory_body_encodes_givers_and_quests() {
        let body = quest_directory_body(&[(
            "world.tutorial.npc.HelperNoobosaur00".to_string(),
            vec![0xFBBE_D69D],
        )]);
        let mut expected = vec![0x01];
        expected.extend_from_slice(b"world.tutorial.npc.HelperNoobosaur00\0");
        expected.extend_from_slice(&[0x01, 0x04, 0x9D, 0xD6, 0xBE, 0xFB]);
        assert_eq!(body, expected);
        assert_eq!(quest_directory_body(&[]), vec![0x00]);
    }

    #[test]
    fn manager_ids_are_validated() {
        assert_eq!(
            derived_player_manager_components(0x1000, 0, 0xE001, &[]),
            Err(PlayerError::BadComponentId)
        );
        assert_eq!(
            derived_player_manager_components(0x1000, 0xE000, 0xE000, &[]),
            Err(PlayerError::BadComponentId)
        );
        assert_eq!(
            derived_player_manager_components(0x1000, 0x1000, 0xE001, &[]),
            Err(PlayerError::BadComponentId)
        );
    }

    // The journal installs AddQuest-body records between the instantiated
    // `u16` count and the checkpoint block, and moves the counter word.
    #[test]
    fn journal_records_are_installed_and_move_the_counter() {
        let directory = quest_directory_body(&[]);
        let empty = derived_quest_manager_record_with_journal(0x1000, 0xE000, &directory, 1, &[]);
        let journal = [JournalRecord {
            qid: [0x04, 0x9D, 0xD6, 0xBE, 0xFB],
            instance_ref: 5,
            all_done: true,
            objectives: vec![(0x01, b"Ticket: 1 / 1".to_vec())],
        }];
        let with =
            derived_quest_manager_record_with_journal(0x1000, 0xE000, &directory, 6, &journal);
        // The AddQuest body: qid 5 + instance 4 + allComplete 1 + count 1 +
        // (flags 1 + label 13 + NUL 1) = 26 bytes. The empty record already
        // carries its own count/checkpoint words, so only the table grows.
        assert_eq!(with.len(), empty.len() + 26);
        // The create's first word is the next-instance counter.
        let quest_create = &with[20..];
        assert_eq!(&quest_create[0..4], &6u32.to_le_bytes());
        // The record's qid and instance id are on the wire in order.
        assert!(
            with.windows(9)
                .any(|window| window == [0x04, 0x9D, 0xD6, 0xBE, 0xFB, 5, 0, 0, 0]),
            "qid + instance ref land in the journal table"
        );
        assert!(
            with.windows(b"Ticket: 1 / 1\0".len())
                .any(|window| window == b"Ticket: 1 / 1\0"),
            "the objective label is on the wire"
        );
    }

    #[test]
    fn join_body_frames_the_classname_and_components() {
        let comps = derived_player_manager_components(0x1000, 0xE000, 0xE001, &[]).unwrap();
        let rec = join_player_init_body(0x1000, b"Hero", 1, 0x04A4, &comps).unwrap();
        assert_eq!(&rec[0..2], &0x1000u16.to_le_bytes());
        assert_eq!(rec[2], TYPE_TAG_CLASSNAME);
        assert_eq!(&rec[3..10], b"player\0");
        assert_eq!(&rec[10..15], b"Hero\0");
        assert_eq!(rec.len(), 2 + 1 + 7 + 5 + 27 + 82);
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    #[test]
    fn checkpoint_block_carries_the_starting_classes() {
        let classes = [
            "world.checkpoints.TownCheckpoint",
            "world.checkpoints.TutorialCheckpoint",
        ];
        let empty = derived_quest_manager_record(0x1000, 0xE000, &[]);
        let with = derived_quest_manager_record_with_journal_and_checkpoints(
            0x1000,
            0xE000,
            &[],
            1,
            &[],
            &classes,
        );
        assert!(with.len() > empty.len());
        let mut needle = vec![0x02, 0x00];
        for class in classes {
            needle.push(TYPE_TAG_CLASSNAME);
            needle.extend_from_slice(class.as_bytes());
            needle.push(0);
        }
        assert!(
            with.windows(needle.len()).any(|window| window == needle),
            "the checkpoint count + GC types ride the QuestManager create"
        );
    }

    #[test]
    fn recent_zone_portal_block_carries_zone_and_spawn() {
        let rec = derived_quest_manager_record_with_journal_checkpoints_and_recent_portal(
            0x1000,
            0xE000,
            &[],
            1,
            &[],
            QuestManagerTail {
                checkpoints: &[],
                recent_portal: Some(("dungeon00_level02", "start1")),
            },
        );
        let needle = b"dungeon00_level02\0start1\0\0";
        assert!(
            rec.windows(needle.len()).any(|window| window == needle),
            "AutoPortalZone + AutoPortalSpawn + legacy string ride the create"
        );
    }

    #[test]
    fn absent_recent_zone_portal_stays_empty() {
        let rec = derived_quest_manager_record_with_journal_checkpoints_and_recent_portal(
            0x1000,
            0xE000,
            &[],
            1,
            &[],
            QuestManagerTail::checkpoints(&[]),
        );
        // The three empty strings sit immediately before the empty offer
        // directory count and the journal/checkpoint counts.
        let needle = [0u8; 4];
        assert!(rec.windows(needle.len()).any(|window| window == needle));
    }
}
