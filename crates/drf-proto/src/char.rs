// SPDX-License-Identifier: GPL-3.0-or-later

//! Character-manager payloads: select/delete acks and the select roster burst.
//!
//! The channel coordinates are the client's own dispatch: `UserManagerClient`
//! `0x03` (`0x01` rosters), `GroupClient` `0x09` (`0x30` connected, `0x35`
//! user-changed-group) and `CharacterManagerClient` `0x04` (`0x04` delete,
//! `0x05` select). Each body is the shape its reader consumes; nothing here
//! came from a capture.

/// The `(03,01)` rosters-message burst: `01 01` then eight zero bytes.
///
/// Grounded on the client binary (`0x602929`, `0x6029B5`), not on a specimen.
pub const ROSTERS_0301: [u8; 10] = [0x01, 0x01, 0, 0, 0, 0, 0, 0, 0, 0];

fn asciiz(value: &str) -> Vec<u8> {
    value
        .split('\u{0}')
        .next()
        .unwrap_or("")
        .as_bytes()
        .to_vec()
}

/// The `(04,05)` select request body: the selected character's `u32` sqlid.
pub fn select_payload(sqlid: u32) -> Vec<u8> {
    sqlid.to_le_bytes().to_vec()
}

/// The `(04,05)` select ack: bare on success, a single zero byte on failure.
///
/// The live success shape carries no payload at all; failure did not appear
/// live, so the failure body is the minimal one-byte zero.
pub fn select_ack_payload(ok: bool) -> Vec<u8> {
    if ok {
        Vec::new()
    } else {
        vec![0]
    }
}

/// The `(03,01)` rosters message burst body.
pub fn rosters_0301_payload() -> Vec<u8> {
    ROSTERS_0301.to_vec()
}

/// The `(09,30)` `processConnected` body: `[u32 sqlid][u16 0]`.
pub fn group_connected_0930_payload(sqlid: u32) -> Vec<u8> {
    let mut out = sqlid.to_le_bytes().to_vec();
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// The `(09,35)` `processUserChangedGroup` body for `name` and `level`.
///
/// `roster_head` is the four-byte session word the live `0935` specimen opens
/// with (`d5 05 00 80`). It is a per-session allocation artifact, not content:
/// `game.pkg` cannot settle it and no name binds it, so the caller supplies it
/// rather than this builder embedding one session's value.
pub fn user_changed_group_0935_payload(
    roster_head: [u8; 4],
    sqlid: u32,
    name: &str,
    level: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&roster_head);
    out.extend_from_slice(&sqlid.to_le_bytes());
    out.extend_from_slice(&[0u8; 12]);
    out.push(0x01);
    out.extend_from_slice(&sqlid.to_le_bytes());
    out.extend_from_slice(&asciiz(name));
    out.push(0);
    out.extend_from_slice(&level.to_le_bytes());
    out.push(0x01);
    out
}

/// The `(04,04)` delete request body: the character's `u32` sqlid.
pub fn delete_payload(sqlid: u32) -> Vec<u8> {
    sqlid.to_le_bytes().to_vec()
}

/// The `(04,04)` delete ack: `[u32 sqlid][u8 ok]`.
pub fn delete_ack_payload(sqlid: u32, ok: bool) -> Vec<u8> {
    let mut out = sqlid.to_le_bytes().to_vec();
    out.push(u8::from(ok));
    out
}
