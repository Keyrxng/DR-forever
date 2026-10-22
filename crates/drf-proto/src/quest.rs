// SPDX-License-Identifier: GPL-3.0-or-later

//! The tutorial quest wire loop: the `07 34` NPC click, its shared `07 35`
//! activate ack ([`activate_ack_body`]), the c2s query/accept steps, and the
//! s2c SetOfferedQuest/AddQuest records.
//!
//! Every field is little-endian. The parsers fail closed (`None`) on a
//! truncated or mistagged shape; the builders neither panic nor emit an
//! unreadable objective label.

use crate::mover::{CEM_COMPONENT_UPDATE, ENTITY_CHANNEL, ENTITY_REQUEST, SYNCH_FLAG_HP};

/// c2s empty accept / s2c AddQuest.
pub const QUEST_SUB_ACCEPT: u8 = 0x01;
/// c2s close dialog.
pub const QUEST_SUB_CLOSE: u8 = 0x02;
/// s2c SetOfferedQuest.
pub const QUEST_SUB_SET_OFFERED: u8 = 0x04;
/// c2s QueryOfferedQuest.
pub const QUEST_SUB_QUERY: u8 = 0x06;
/// s2c AvailableQuestUpdate (the NPC `!` markers).
pub const QUEST_SUB_AVAILABLE: u8 = 0x07;
/// c2s turn-in query (sub 0x04) / s2c SetOfferedQuest share this byte.
pub const QUEST_SUB_TURNIN_QUERY: u8 = 0x04;
/// c2s turn-in complete (sub 0x05).
pub const QUEST_SUB_TURNIN_COMPLETE: u8 = 0x05;
/// s2c UpdateQuest: restate one held quest's completion (repaints its marker).
pub const QUEST_SUB_OBJECTIVE_UPDATE: u8 = 0x03;
/// s2c FinalizeQuest.
pub const QUEST_SUB_FINALIZE: u8 = 0x08;
/// The one-byte stream terminator.
pub const END_MARKER: u8 = crate::cem::END_MARKER;

/// A pre-completed placeholder objective for a quest with no authored ones.
pub const QUEST_ZERO_OBJECTIVE_PLACEHOLDER: (u8, &[u8]) = (0x01, b"Read: 1 / 1");
/// A pending placeholder objective for a quest with no readiness evidence.
pub const QUEST_PENDING_OBJECTIVE_PLACEHOLDER: (u8, &[u8]) = (0x00, b"Read: 0 / 1");

/// The shared `(07,35)` activate-ack body for a c2s `(07,34)` `0x06`
/// BehaviourActionActivate click.
///
/// The content is the click request's own bytes from `<componentId>` onward
/// (`<cid:u16> 01 <responseId> 06 <session> <target:u16>`), then the avatar's
/// per-component synch trailer `02 <hp_wire:u32>`. This is the content of a
/// `(07,35)` stream AFTER the `[07 35]` envelope; the caller appends whatever
/// follows in the same stream -- the `06` terminator for a bare ack (NPC,
/// portal, quest giver), or the ground-entity remove plus the item add for a
/// loot pickup.
///
/// `hp_wire` is the AVATAR's x256 HP word -- the same word the join's create
/// and every other avatar-component synch trailer carry. It is never the
/// clicked entity's HP and never a fixed literal: the activate ack addresses
/// the avatar's own UnitBehavior component, and
/// `EntitySynchInfo::Validate`@0x5DD900 fatals the avatar on a mismatch (the
/// `Last EntityManager error: 1` world-entry failure).
pub fn activate_ack_body(c2s_inner: &[u8], hp_wire: u32) -> Option<Vec<u8>> {
    if c2s_inner.len() < 10 || c2s_inner[0] != ENTITY_CHANNEL || c2s_inner[1] != ENTITY_REQUEST {
        return None;
    }
    let mut out = Vec::with_capacity(c2s_inner.len() - 2 + 5);
    out.extend_from_slice(&c2s_inner[2..]);
    out.push(SYNCH_FLAG_HP);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    Some(out)
}

/// A c2s `07 34` Interact-06 click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractClick {
    pub component_id: u16,
    pub response_id: u8,
    pub target: u16,
}

/// Parse `07 34 | u16 component | 01 | responseId | 06 | session | u16 target`.
pub fn parse_interact_click(inner: &[u8]) -> Option<InteractClick> {
    if inner.len() != 10
        || inner[0] != ENTITY_CHANNEL
        || inner[1] != ENTITY_REQUEST
        || inner[4] != 0x01
        || inner[6] != 0x06
    {
        return None;
    }
    Some(InteractClick {
        component_id: u16::from_le_bytes([inner[2], inner[3]]),
        response_id: inner[5],
        target: u16::from_le_bytes([inner[8], inner[9]]),
    })
}

/// A c2s `07 34` QuestManager QueryOfferedQuest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuestQuery {
    pub component_id: u16,
    pub npc_entity_id: u32,
    pub qid: [u8; 5],
}

/// Parse `07 34 | u16 questManager | 06 | u32 npcEntityId | 04 + u32 hash`.
pub fn parse_quest_query(inner: &[u8], quest_ref: u16) -> Option<QuestQuery> {
    if quest_ref == 0 || inner.len() != 14 {
        return None;
    }
    if inner[0] != ENTITY_CHANNEL
        || inner[1] != ENTITY_REQUEST
        || u16::from_le_bytes([inner[2], inner[3]]) != quest_ref
        || inner[4] != QUEST_SUB_QUERY
    {
        return None;
    }
    let qid = [inner[9], inner[10], inner[11], inner[12], inner[13]];
    if qid[0] != 0x04 {
        return None;
    }
    Some(QuestQuery {
        component_id: quest_ref,
        npc_entity_id: u32::from_le_bytes([inner[5], inner[6], inner[7], inner[8]]),
        qid,
    })
}

/// Parse a five-byte `07 34 | u16 questManager | 01|02` accept/close step.
pub fn parse_quest_accept(inner: &[u8], quest_ref: u16) -> Option<u8> {
    if quest_ref == 0 || inner.len() != 5 {
        return None;
    }
    if inner[0] != ENTITY_CHANNEL
        || inner[1] != ENTITY_REQUEST
        || u16::from_le_bytes([inner[2], inner[3]]) != quest_ref
    {
        return None;
    }
    match inner[4] {
        QUEST_SUB_ACCEPT | QUEST_SUB_CLOSE => Some(inner[4]),
        _ => None,
    }
}

/// Parse a nine-byte `07 34 | u16 questManager | 03 | u32 instance`: the
/// client's `QuestManager::abandon` request.
///
/// The payload is the quest's instance ref (the client's `Quest + 0x68`),
/// not its qid. The server answers with RemoveQuest and a fresh available
/// directory so the journal and the giver marker both drop it.
pub fn parse_quest_abandon(inner: &[u8], quest_ref: u16) -> Option<u32> {
    if quest_ref == 0 || inner.len() != 9 {
        return None;
    }
    if inner[0] != ENTITY_CHANNEL
        || inner[1] != ENTITY_REQUEST
        || u16::from_le_bytes([inner[2], inner[3]]) != quest_ref
        || inner[4] != QUEST_SUB_OBJECTIVE_UPDATE
    {
        return None;
    }
    Some(u32::from_le_bytes(inner[5..9].try_into().ok()?))
}

/// A selected obelisk checkpoint: either its FQN (`0xFF` classname type) or
/// the GC class hash the client actually serializes (`0x04` + u32).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointTravel {
    Class(String),
    Hash(u32),
}

fn checkpoint_body(mut rest: &[u8]) -> Option<CheckpointTravel> {
    match *rest.first()? {
        0x04 => {
            let hash = u32::from_le_bytes(rest.get(1..5)?.try_into().ok()?);
            Some(CheckpointTravel::Hash(hash))
        }
        0xFF => {
            rest = &rest[1..];
            let nul = rest.iter().position(|&byte| byte == 0)?;
            std::str::from_utf8(&rest[..nul])
                .ok()
                .filter(|class| class.starts_with("world.checkpoints."))
                .map(|class| CheckpointTravel::Class(class.to_string()))
        }
        _ => None,
    }
}

/// Parse a c2s obelisk checkpoint travel (sub `0x07`).
///
/// `QuestManager::goToCheckpoint` sends the selected checkpoint's GC type:
/// `[0x04][u32 dfc_hash]` for the shipped class objects (the client's
/// `GCClassRegistry::writeType`), or a `[0xFF][classname NUL]` classname
/// type. Both are accepted; the class itself is the destination.
pub fn parse_quest_checkpoint(inner: &[u8], quest_ref: u16) -> Option<CheckpointTravel> {
    if inner.len() < 7 || quest_ref == 0 {
        return None;
    }
    if inner[0] != ENTITY_CHANNEL
        || inner[1] != ENTITY_REQUEST
        || u16::from_le_bytes([inner[2], inner[3]]) != quest_ref
        || inner[4] != 0x07
    {
        return None;
    }
    checkpoint_body(&inner[5..])
}

/// Parse an obelisk travel by its GC type alone, regardless of which
/// component cid the client addressed.
pub fn parse_checkpoint_travel(inner: &[u8]) -> Option<CheckpointTravel> {
    if inner.len() < 7
        || inner[0] != ENTITY_CHANNEL
        || inner[1] != ENTITY_REQUEST
        || inner[4] != 0x07
    {
        return None;
    }
    checkpoint_body(&inner[5..])
}

/// The `04 + u32` little-endian GC type tag for a quest hash.
pub fn quest_gc_type(qid_u32: u32) -> [u8; 5] {
    let b = qid_u32.to_le_bytes();
    [0x04, b[0], b[1], b[2], b[3]]
}

/// The s2c `07 35 | u16 questManager | 04 | qid(5) | 00 | 06` SetOfferedQuest.
pub fn build_quest_offer_ack(qid: &[u8; 5], quest_ref: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&[ENTITY_CHANNEL, CEM_COMPONENT_UPDATE]);
    out.extend_from_slice(&quest_ref.to_le_bytes());
    out.push(QUEST_SUB_SET_OFFERED);
    out.extend_from_slice(qid);
    out.push(0x00);
    out.push(END_MARKER);
    out
}

/// The s2c `07 35 | u16 questManager | 07 | <directory> | 00 | 06`
/// AvailableQuestUpdate: the whole offer directory re-sent, so a quest that
/// just left the offered set loses its giver's marker. `directory` is
/// [`crate::quest_directory_body`]'s bytes.
pub fn build_quest_available_update(quest_ref: u16, directory: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(7 + directory.len());
    out.extend_from_slice(&[ENTITY_CHANNEL, CEM_COMPONENT_UPDATE]);
    out.extend_from_slice(&quest_ref.to_le_bytes());
    out.push(QUEST_SUB_AVAILABLE);
    out.extend_from_slice(directory);
    out.push(0x00);
    out.push(END_MARKER);
    out
}

/// Parse a c2s turn-in leg: `07 34 | u16 questManager | 04 | u32 instance`
/// (query, 9B) or `... 05 | u32 instance | 00` (complete, 10B). Returns
/// `(step, instance_ref)`.
pub fn parse_quest_turnin(inner: &[u8], quest_ref: u16) -> Option<(u8, u32)> {
    if inner.len() < 5 || inner[..2] != [ENTITY_CHANNEL, ENTITY_REQUEST] {
        return None;
    }
    if u16::from_le_bytes([inner[2], inner[3]]) != quest_ref {
        return None;
    }
    let instance = u32::from_le_bytes(inner.get(5..9)?.try_into().ok()?);
    match (inner.len(), inner[4]) {
        (9, QUEST_SUB_TURNIN_QUERY) => Some((QUEST_SUB_TURNIN_QUERY, instance)),
        (10, QUEST_SUB_TURNIN_COMPLETE) if inner[9] == 0 => {
            Some((QUEST_SUB_TURNIN_COMPLETE, instance))
        }
        _ => None,
    }
}

/// The s2c `07 35 | u16 questManager | 06 | u32 instance | 00 | 06`
/// ShowTurnInDialog, sent in answer to a turn-in query for a ready quest.
pub fn build_quest_ready_ack(quest_ref: u16, instance_ref: u32) -> Vec<u8> {
    quest_instance_record(quest_ref, QUEST_SUB_QUERY, instance_ref, None)
}

/// The s2c `07 35 | u16 questManager | 08 | u32 instance | 00 | 06`
/// FinalizeQuest, sent when the client confirms the turn-in.
pub fn build_quest_finalize(quest_ref: u16, instance_ref: u32) -> Vec<u8> {
    quest_instance_record(quest_ref, QUEST_SUB_FINALIZE, instance_ref, None)
}

/// The s2c `07 35 | u16 questManager | 02 | u32 instance | 00 | 06`
/// RemoveQuest, sent right behind FinalizeQuest.
pub fn build_quest_remove(quest_ref: u16, instance_ref: u32) -> Vec<u8> {
    quest_instance_record(quest_ref, QUEST_SUB_CLOSE, instance_ref, None)
}

/// The s2c `07 35 | u16 questManager | 03 | u32 instance | 00 kind | u8
/// complete | 00 | 06` UpdateQuest: the only frame that makes the client
/// re-read a held quest's completion and repaint its marker.
pub fn build_quest_objective_update(quest_ref: u16, instance_ref: u32, complete: bool) -> Vec<u8> {
    quest_instance_record(
        quest_ref,
        QUEST_SUB_OBJECTIVE_UPDATE,
        instance_ref,
        Some(u8::from(complete)),
    )
}

/// The s2c UpdateQuest objective-list form: kind `0x01` makes the client
/// clear its cached objective strings and read the supplied `(flags, label)`
/// rows. It is the only frame that can change a displayed count such as
/// `Wolf Pelt: 1 / 12`; `kind 0x00` only updates the all-complete bit.
pub fn build_quest_objective_labels_update(
    quest_ref: u16,
    instance_ref: u32,
    objectives: &[(u8, Vec<u8>)],
) -> Vec<u8> {
    let kept: Vec<&(u8, Vec<u8>)> = objectives
        .iter()
        .filter(|(_, label)| !label.is_empty())
        .collect();
    let count = kept.len().min(0xFF) as u8;
    let mut out = Vec::with_capacity(11 + kept.iter().map(|(_, l)| l.len() + 2).sum::<usize>());
    out.extend_from_slice(&[ENTITY_CHANNEL, CEM_COMPONENT_UPDATE]);
    out.extend_from_slice(&quest_ref.to_le_bytes());
    out.push(QUEST_SUB_OBJECTIVE_UPDATE);
    out.extend_from_slice(&instance_ref.to_le_bytes());
    out.push(0x01); // UpdateQuest kind: readObjectives
    out.push(count);
    for (flags, label) in kept {
        out.push(*flags);
        out.extend_from_slice(label);
        out.push(0x00);
    }
    out.push(0x00);
    out.push(END_MARKER);
    out
}

/// `07 35 | u16 questManager | sub | u32 instance | [u8 extra] | 00 | 06`.
fn quest_instance_record(quest_ref: u16, sub: u8, instance_ref: u32, extra: Option<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&[ENTITY_CHANNEL, CEM_COMPONENT_UPDATE]);
    out.extend_from_slice(&quest_ref.to_le_bytes());
    out.push(sub);
    out.extend_from_slice(&instance_ref.to_le_bytes());
    if let Some(extra) = extra {
        out.push(0x00); // UpdateQuest kind: the only one that carries a payload
        out.push(extra);
    }
    out.push(0x00);
    out.push(END_MARKER);
    out
}

/// One `(flags, label)` objective. A NUL truncates the label; `current` and
/// `required` wrap the text as `"<label>: <cur> / <req>"`, with `required`
/// defaulting to and never falling below 1.
pub fn quest_objective(
    label: &str,
    current: Option<u32>,
    required: Option<u32>,
    complete: bool,
) -> (u8, Vec<u8>) {
    let base = label.split('\u{0}').next().unwrap_or("");
    let flags = if complete { 0x01 } else { 0x00 };
    if current.is_some() || required.is_some() {
        let cur = current.unwrap_or(0);
        let req = required.unwrap_or(1).max(1);
        (flags, format!("{base}: {cur} / {req}").into_bytes())
    } else {
        (flags, base.as_bytes().to_vec())
    }
}

fn truncate_nul(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(pos) => &bytes[..pos],
        None => bytes,
    }
}

/// The s2c AddQuest record: `07 35 | u16 questManager | 01 | qid(5) |
/// u32 instance | u8 allComplete | u8 count | [u8 flags][label cstr]... |
/// 00 | 06`.
///
/// An empty objective list becomes one placeholder: pending when
/// `ready_hint == Some(false)`, else pre-completed. An empty or all-NUL label
/// is dropped; if that leaves none, the placeholder is used instead.
pub fn build_quest_add_record(
    qid: &[u8; 5],
    quest_ref: u16,
    instance_ref: u32,
    objectives: &[(u8, Vec<u8>)],
    all_done: Option<bool>,
    ready_hint: Option<bool>,
) -> Vec<u8> {
    let mut kept: Vec<(u8, Vec<u8>)> = Vec::with_capacity(objectives.len().max(1));
    for (flags, label) in objectives {
        let text = truncate_nul(label);
        if !text.is_empty() {
            kept.push((*flags, text.to_vec()));
        }
    }
    kept.truncate(0xFF);
    if kept.is_empty() {
        let (flags, label) = if ready_hint == Some(false) {
            QUEST_PENDING_OBJECTIVE_PLACEHOLDER
        } else {
            QUEST_ZERO_OBJECTIVE_PLACEHOLDER
        };
        kept.push((flags, label.to_vec()));
    }

    let all_done = all_done.unwrap_or_else(|| kept.iter().all(|(flags, _)| flags & 0x01 != 0));
    let count = kept.len() as u8;

    let mut out = Vec::with_capacity(19 + kept.iter().map(|(_, l)| l.len() + 2).sum::<usize>());
    out.extend_from_slice(&[ENTITY_CHANNEL, CEM_COMPONENT_UPDATE]);
    out.extend_from_slice(&quest_ref.to_le_bytes());
    out.push(QUEST_SUB_ACCEPT);
    out.extend_from_slice(qid);
    out.extend_from_slice(&instance_ref.to_le_bytes());
    out.push(if all_done { 0x01 } else { 0x00 });
    out.push(count);
    for (flags, label) in &kept {
        out.push(*flags);
        out.extend_from_slice(label);
        out.push(0x00);
    }
    out.push(0x00);
    out.push(END_MARKER);
    out
}
