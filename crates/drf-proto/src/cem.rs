// SPDX-License-Identifier: GPL-3.0-or-later

//! CEM batch framing: the `[07][tag][payload]...[end]` record stream.
//!
//! `ClientEntityManager::processMessage` reads one-byte tags from a single
//! stream and dispatches each record until the END (`0x06`) or CONNECTED
//! (`0x46`) terminator. This module owns the stream grammar only; record
//! bodies live in [`crate::create`].

use std::fmt;

/// The CEM entity-stream channel byte.
pub const CEM_CHANNEL: u8 = 0x07;

/// The silent clean-end terminator.
pub const END_MARKER: u8 = 0x06;

/// The CONNECTED terminator, as a wire tag.
pub const CONNECTED_MARKER: u8 = 0x46;

/// The CONNECTED terminator, named as the client's wire case.
pub const CONNECTED_CASE: u8 = 0x46;

/// The join batch terminator (CONNECTED parity).
pub const JOIN_END_MARKER: u8 = 0x46;

/// `processEntityCreate`.
pub const CEM_CREATE: u8 = 0x01;

/// `processEntityInit`.
pub const CEM_ENTITY_INIT: u8 = 0x02;

/// `processEntityUpdate` @0x5DAE30: `[u16 entity][u8 update]`, the tag the
/// join batch's player-avatar binding already rides. Dispatch reads the
/// entity id, then the update id, then calls the entity's `processUpdate`.
pub const CEM_ENTITY_UPDATE: u8 = 0x03;

/// Entity removal.
pub const CEM_REMOVE: u8 = 0x05;

/// Silent return / clean end.
pub const CEM_STREAM_END: u8 = 0x06;

/// `processEntityCreateInit`.
pub const CEM_CREATE_INIT: u8 = 0x08;

/// The interval container message type.
pub const CEM_INTERVAL_CONTAINER: u8 = 0x0D;

/// The bare sub-entities marker emitted before CONNECTED.
pub const CEM_INIT_SUB_ENTITIES: u8 = 0x1E;

/// No type; clears the result.
pub const TYPE_TAG_NONE: u8 = 0x00;

/// One-byte numeric TypeID.
pub const TYPE_TAG_U8: u8 = 0x01;

/// Two-byte numeric TypeID.
pub const TYPE_TAG_U16: u8 = 0x02;

/// Invalid type tag.
pub const TYPE_TAG_INVALID: u8 = 0x03;

/// Four-byte numeric TypeID.
pub const TYPE_TAG_U32: u8 = 0x04;

/// NUL-terminated classname type tag.
pub const TYPE_TAG_CLASSNAME: u8 = 0xFF;

/// Non-unit entity flag word.
pub const FLAG_NONUNIT: u8 = 0;

/// Unit entity flag word.
pub const FLAG_UNIT: u8 = 2;

/// The join-record header carried after the interval budgets.
pub const JOIN_RECORD_HEADER: [u8; 8] = [0xA7, 0x2D, 0x01, 0x00, 0x64, 0x00, 0x14, 0x00];

/// A CEM framing failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CemError {
    /// A batch must carry at least one record.
    EmptyBatch,
    /// The first payload repeats a byte the frame header already carries.
    DoubleTag(u8),
}

impl fmt::Display for CemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CemError::EmptyBatch => write!(f, "empty CEM batch"),
            CemError::DoubleTag(byte) => write!(
                f,
                "CEM payload opens with {byte:#04x}, which repeats the frame header"
            ),
        }
    }
}

impl std::error::Error for CemError {}

/// Frame `(tag, payload)` records as a CEM inner stream.
///
/// The result is `[07][tag0][payload0][tag1][payload1]...[06]`. The first
/// payload must not repeat the channel or its own tag, because the client
/// reads the frame header's message type as the first record's tag.
pub fn frame_inner(parts: &[(u8, &[u8])]) -> Result<Vec<u8>, CemError> {
    frame_inner_with_end(parts, Some(END_MARKER))
}

/// Frame records with an explicit terminator; `None` suppresses it.
pub fn frame_inner_with_end(parts: &[(u8, &[u8])], end: Option<u8>) -> Result<Vec<u8>, CemError> {
    let Some(((tag0, pay0), rest)) = parts.split_first() else {
        return Err(CemError::EmptyBatch);
    };
    if let Some(&first) = pay0.first() {
        if first == CEM_CHANNEL || first == *tag0 {
            return Err(CemError::DoubleTag(first));
        }
    }

    let mut out = Vec::new();
    out.push(CEM_CHANNEL);
    out.push(*tag0);
    out.extend_from_slice(pay0);
    for (tag, payload) in rest {
        out.push(*tag);
        out.extend_from_slice(payload);
    }
    if let Some(end) = end {
        out.push(end);
    }
    Ok(out)
}
