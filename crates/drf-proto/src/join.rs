// SPDX-License-Identifier: GPL-3.0-or-later

//! Zone-join payloads: assign, ready, instance count, seed and the join batch.
//!
//! The bodies match the client readers: `(13,0)` assign is a zone ASCIIZ, a
//! one-byte GC-type count and the `0xFF`-tagged type list; `(13,1)` Ready and
//! `(09,30)` are the selected sqlid plus a zero word; `(13,5)` carries two
//! `u32`; `(7,12)` is one `u32` seed; and the join batch is the interval
//! container ([`crate::cem::JOIN_RECORD_HEADER`], then [`crate::create`]
//! records) ending on `0x46`.

use std::fmt;

use crate::cem::{
    CEM_CHANNEL, CEM_INIT_SUB_ENTITIES, CEM_INTERVAL_CONTAINER, CONNECTED_CASE, TYPE_TAG_CLASSNAME,
};
use crate::create::{interval_payload, INTERVAL_PATHMANAGER_BLOCK};

/// The default `(13,0)` assign trailer: the final `u32` after the GC-type
/// list (four zero bytes). The client reads it into `ZoneClient+0xDC`; no
/// reader is known for it and captures are zero.
pub const ASSIGN_TOWN_TRAILER: [u8; 4] = [0; 4];

/// The `(13,2)` reason a portal hop sends. Death/respawn sends `b"respawn"`.
pub const TRANSFER_REASON_DEFAULT: &[u8] = b"transfer";

/// A zone-join payload shape violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    /// The assign GC-type list cannot fit a one-byte count.
    TooManyAssignTypes(usize),
    /// The assign payload has no NUL after the zone name.
    AssignMissingZoneNul,
    /// The assign zone name is empty.
    AssignEmptyZone,
    /// The assign payload ends before its type count.
    AssignTruncatedAtCount,
    /// An assign GC type is missing its `0xFF` tag.
    AssignMissingTypeTag,
    /// An assign GC type is not NUL-terminated.
    AssignMissingTypeNul,
    /// A ready payload is not exactly six bytes.
    ReadyLength(usize),
    /// A ready payload does not end on the `u16` zero word.
    ReadyTrailingWord,
    /// A transfer reason is empty or cannot fit the `[reason NUL]` record.
    TransferReasonShape,
    /// A transfer payload is not NUL-terminated.
    TransferNotTerminated,
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JoinError::TooManyAssignTypes(count) => {
                write!(f, "assign GC type count {count} exceeds 255")
            }
            JoinError::AssignMissingZoneNul => write!(f, "assign payload missing zone NUL"),
            JoinError::AssignEmptyZone => write!(f, "assign payload empty zone"),
            JoinError::AssignTruncatedAtCount => write!(f, "assign payload truncated at count"),
            JoinError::AssignMissingTypeTag => write!(f, "assign GC type missing tag"),
            JoinError::AssignMissingTypeNul => write!(f, "assign GC type missing NUL"),
            JoinError::ReadyLength(len) => write!(f, "ready payload must be 6 bytes, got {len}"),
            JoinError::ReadyTrailingWord => {
                write!(f, "ready payload trailing word must be 0000")
            }
            JoinError::TransferReasonShape => {
                write!(
                    f,
                    "transfer reason must be 1..=63 bytes with no interior NUL"
                )
            }
            JoinError::TransferNotTerminated => {
                write!(f, "transfer payload must be NUL-terminated")
            }
        }
    }
}

impl std::error::Error for JoinError {}

/// A parsed `(13,0)` assign payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignPayload {
    pub zone: Vec<u8>,
    /// The zone seed the client feeds to `World::initialize` (`ZoneClient+0xC8`);
    /// it is the MT19937 seed for a `Generated` world's maze.
    pub seed: u32,
    pub gc_types: Vec<Vec<u8>>,
    pub trailer: Vec<u8>,
}

/// A parsed `(13,1)` ready payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyPayload {
    pub sqlid: u32,
}

fn trim_asciiz(value: &str) -> &str {
    value.split('\u{0}').next().unwrap_or("")
}

/// The `(13,0)` assign payload: `[zone NUL][u32 seed][u8 count][GC types][u32 trailer]`.
///
/// The client's `processConnected` reads exactly this: the zone name into
/// `ZoneClient+0xC4`, a `u32` into `+0xC8` (the maze seed handed to
/// `World::initialize`), a `u8` GC-type count, that many `[0xFF][type NUL]`
/// strings, and a final `u32` into `+0xDC`. `seed` is [`maze::layout_seed`] for
/// a `Generated` world (zero otherwise), so the server and client build the
/// same maze; `trailer` defaults to [`ASSIGN_TOWN_TRAILER`].
pub fn assign_payload(
    zone: &str,
    seed: u32,
    gc_types: &[&str],
    trailer: Option<&[u8]>,
) -> Result<Vec<u8>, JoinError> {
    if gc_types.len() > 255 {
        return Err(JoinError::TooManyAssignTypes(gc_types.len()));
    }
    let mut out = trim_asciiz(zone).as_bytes().to_vec();
    out.push(0);
    out.extend_from_slice(&seed.to_le_bytes());
    out.push(gc_types.len() as u8);
    for gc_type in gc_types {
        out.push(TYPE_TAG_CLASSNAME);
        out.extend_from_slice(trim_asciiz(gc_type).as_bytes());
        out.push(0);
    }
    out.extend_from_slice(trailer.unwrap_or(&ASSIGN_TOWN_TRAILER));
    Ok(out)
}

/// Parse a `(13,0)` assign payload.
pub fn parse_assign_payload(pay: &[u8]) -> Result<AssignPayload, JoinError> {
    let znul = pay
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(JoinError::AssignMissingZoneNul)?;
    let zone = pay[..znul].to_vec();
    if zone.is_empty() {
        return Err(JoinError::AssignEmptyZone);
    }
    if pay.len() < znul + 5 {
        return Err(JoinError::AssignTruncatedAtCount);
    }
    let seed = u32::from_le_bytes([pay[znul + 1], pay[znul + 2], pay[znul + 3], pay[znul + 4]]);
    let count = pay[znul + 5] as usize;
    let mut cur = &pay[znul + 6..];
    let mut gc_types = Vec::with_capacity(count);
    for _ in 0..count {
        if cur.len() < 2 || cur[0] != TYPE_TAG_CLASSNAME {
            return Err(JoinError::AssignMissingTypeTag);
        }
        let tnul = cur[1..]
            .iter()
            .position(|&byte| byte == 0)
            .map(|pos| pos + 1)
            .ok_or(JoinError::AssignMissingTypeNul)?;
        gc_types.push(cur[1..tnul].to_vec());
        cur = &cur[tnul + 1..];
    }
    Ok(AssignPayload {
        zone,
        seed,
        gc_types,
        trailer: cur.to_vec(),
    })
}

/// The `(13,1)` ready payload: `[u32 sqlid][u16 0]`.
pub fn ready_payload(sqlid: u32) -> Vec<u8> {
    ready_payload_with_explored(sqlid, None)
}

/// The `(13,1)` ready payload, with the zone's minimap explored array when
/// the destination has a minimap.
///
/// The client's `ZoneClient::processReady` reads the sqlid word, then—only
/// when `ZoneDef+0x84 & 1` is set—`MiniMapExplored::ReadExploredBits`, whose
/// grammar is `[u16 count][count x u32]`. A count mismatch returns without
/// consuming the words, so the count must equal the minimap's own `+0x40`
/// cell count; [`None`] sends the 8-byte form used before this was known.
pub fn ready_payload_with_explored(sqlid: u32, explored: Option<&[u32]>) -> Vec<u8> {
    let mut out = sqlid.to_le_bytes().to_vec();
    match explored {
        Some(words) => {
            out.extend_from_slice(&(words.len().min(u16::MAX as usize) as u16).to_le_bytes());
            for word in words.iter().take(u16::MAX as usize) {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
        None => out.extend_from_slice(&0u16.to_le_bytes()),
    }
    out
}

/// Parse a `(13,1)` ready payload.
pub fn parse_ready_payload(pay: &[u8]) -> Result<ReadyPayload, JoinError> {
    if pay.len() != 6 {
        return Err(JoinError::ReadyLength(pay.len()));
    }
    let sqlid = u32::from_le_bytes([pay[0], pay[1], pay[2], pay[3]]);
    if pay[4..] != [0, 0] {
        return Err(JoinError::ReadyTrailingWord);
    }
    Ok(ReadyPayload { sqlid })
}

/// The `(13,5)` instance-count payload: `[u32 max][u32 current]`.
pub fn instance_count_payload(max_count: u32, current: u32) -> Vec<u8> {
    let mut out = max_count.to_le_bytes().to_vec();
    out.extend_from_slice(&current.to_le_bytes());
    out
}

/// The `(13,2)` transfer payload: one NUL-terminated reason string.
///
/// The client's `processDisconnected` reads exactly one `readString` and
/// ignores every byte after the first NUL, so the record is `reason NUL`. A
/// portal hop sends [`TRANSFER_REASON_DEFAULT`]; death/respawn sends
/// `b"respawn"`. The reason is NUL-trimmed and must then be 1..=63 bytes.
pub fn transfer_payload(reason: &[u8]) -> Result<Vec<u8>, JoinError> {
    let reason = reason.split(|&byte| byte == 0).next().unwrap_or(&[]);
    if reason.is_empty() || reason.len() > 63 {
        return Err(JoinError::TransferReasonShape);
    }
    let mut out = reason.to_vec();
    out.push(0);
    Ok(out)
}

/// Parse a `(13,2)` transfer payload, the inverse of [`transfer_payload`].
pub fn parse_transfer_payload(pay: &[u8]) -> Result<Vec<u8>, JoinError> {
    let Some(nul) = pay.iter().position(|&byte| byte == 0) else {
        return Err(JoinError::TransferNotTerminated);
    };
    if nul == 0 {
        return Err(JoinError::TransferReasonShape);
    }
    Ok(pay[..nul].to_vec())
}

/// The `(7,12)` seed payload: exactly one `u32`.
///
/// The town variant is a descriptor table, not this shape; it is refused as a
/// capture-derived transplant and is not built here.
pub fn seed_payload(seed: u32) -> Vec<u8> {
    seed.to_le_bytes().to_vec()
}

/// The join batch inner: budgets, [`INTERVAL_PATHMANAGER_BLOCK`], creates,
/// terminator.
///
/// The result is `[07][0D][12 budgets][8 PathManager][tag payload]...[end]`,
/// with the `0x1E` sub-entities marker before a CONNECTED terminator. The
/// budget triple is followed by the eight-byte PathManager block the client's
/// `IsKindOf<World,DFCNode>` gate demands; without it the client eats the first
/// entity record's type byte.
pub fn join_batch(budget_triple: (u32, u32, u32), creates: &[(u8, &[u8])], end: u8) -> Vec<u8> {
    let (tick, interval_ms, budget) = budget_triple;
    let mut out = Vec::new();
    out.extend_from_slice(&[CEM_CHANNEL, CEM_INTERVAL_CONTAINER]);
    out.extend_from_slice(&interval_payload(tick, interval_ms, budget));
    out.extend_from_slice(&INTERVAL_PATHMANAGER_BLOCK);
    for (tag, payload) in creates {
        out.push(*tag);
        out.extend_from_slice(payload);
    }
    if end == CONNECTED_CASE {
        out.push(CEM_INIT_SUB_ENTITIES);
    }
    out.push(end);
    out
}
