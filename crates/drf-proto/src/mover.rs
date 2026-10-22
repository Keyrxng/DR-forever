// SPDX-License-Identifier: GPL-3.0-or-later

//! The movement surface: the client-control grant, the `07 34` request and
//! mover stream, the local move-ack, and the `07 0D` WorldInterval watchdog.
//!
//! The local avatar is CLIENT-AUTHORITATIVE: the client steers it and reports
//! position in c2s `07 34 | 0x65` records. The server's job is (a) to answer
//! the client's one control request with the FollowClient grant, without which
//! the input handler runs but the avatar never moves, and (b) to echo the
//! client's own 13-byte records back verbatim inside the `07 0D` WorldInterval
//! watchdog. A SYNTHESIZED position would lag local prediction and snap the
//! avatar; the echo confirms, it does not correct.
//!
//! Every address here is a COMPONENT id (`avatar.base.UnitBehavior`), never an
//! entity id: `processComponentUpdate` resolves the u16 addressee against a
//! component-only map, and an entity id latches the sticky entity-manager
//! error for the session.

use crate::cem::END_MARKER;
use crate::create::INTERVAL_PATHMANAGER_BLOCK;

/// The `0x35` component-update wire tag.
pub const CEM_COMPONENT_UPDATE: u8 = 0x35;

/// The `07` entity-channel byte.
pub const ENTITY_CHANNEL: u8 = 0x07;

/// The `0x34` entity-request envelope sub-channel.
pub const ENTITY_REQUEST: u8 = 0x34;

/// FollowClient sub-op (`UnitBehavior::RequestClientControl`).
pub const FOLLOW_SUB: u8 = 0x64;

/// UnitMoverUpdate sub-op.
pub const MOVE_SUB: u8 = 0x65;

/// The synch-trailer flag that carries an HP word.
pub const SYNCH_FLAG_HP: u8 = 0x02;

/// The move MODE a placement record carries (1 | 2).
pub const MOVE_TYPE_PLACEMENT: u8 = 0x03;

/// A movement-shape violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoverError {
    /// Component id zero is the unassigned sentinel.
    ZeroComponentId,
    /// HP is outside the wire-representable range.
    HpOutOfRange(u32),
    /// A move-ack's `data` is not exactly 13 bytes times its count.
    MoveAckShape,
}

impl std::fmt::Display for MoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoverError::ZeroComponentId => write!(f, "component ID 0 is the unassigned sentinel"),
            MoverError::HpOutOfRange(hp) => write!(f, "hp {hp} out of wire range"),
            MoverError::MoveAckShape => write!(f, "move-ack needs exactly 13 bytes per record"),
        }
    }
}

impl std::error::Error for MoverError {}

/// The x256 wire HP word: `hp * 256`. The entity synch trailer validates it
/// against the entity's real HP, so it is never a literal.
pub fn hp_wire_u32(hp: u32) -> Result<u32, MoverError> {
    if hp > 0x00FF_FFFF {
        return Err(MoverError::HpOutOfRange(hp));
    }
    Ok(hp.wrapping_mul(256))
}

/// The `(07,35)` FollowClient grant payload (after the `[07 35]` envelope):
/// `[u16 component][0x64][on][0x02][u32 hp][0x06]`.
pub fn follow_client_payload(component_id: u16, on: bool, hp: u32) -> Result<Vec<u8>, MoverError> {
    follow_client_payload_wire(component_id, on, hp_wire_u32(hp)?)
}

/// [`follow_client_payload`] taking the already-scaled x256 HP **wire** word.
///
/// The avatar's observed level-1 word ([`crate::avatar_hp_wire`]) is already
/// x256 and is not always a multiple of 256 once damage/regen has moved the
/// client's `Unit+0x2F0`, so callers that hold a wire word use this directly
/// instead of scaling again.
pub fn follow_client_payload_wire(
    component_id: u16,
    on: bool,
    hp_wire: u32,
) -> Result<Vec<u8>, MoverError> {
    if component_id == 0 {
        return Err(MoverError::ZeroComponentId);
    }
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&component_id.to_le_bytes());
    out.push(FOLLOW_SUB);
    out.push(if on { 0x01 } else { 0x00 });
    out.push(SYNCH_FLAG_HP);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    out.push(END_MARKER);
    Ok(out)
}

/// A c2s `07 34` control request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlRequest {
    pub entity_id: u16,
    pub on: bool,
}

/// Parse a c2s `07 34 | u16 entity | 0x64 | on` control request, or `None`.
pub fn parse_control_request(inner: &[u8]) -> Option<ControlRequest> {
    if inner.len() < 5 || inner[0] != ENTITY_CHANNEL || inner[1] != ENTITY_REQUEST {
        return None;
    }
    if inner[4] != FOLLOW_SUB {
        return None;
    }
    let entity_id = u16::from_le_bytes([inner[2], inner[3]]);
    if entity_id == 0 {
        return None;
    }
    let on = inner.get(5).copied().unwrap_or(1) != 0;
    Some(ControlRequest { entity_id, on })
}

/// A parsed c2s `07 34 | 0x65` mover stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoverStream {
    /// The component the records address. The avatar's `avatar.base.UnitBehavior`
    /// is the only owner-prediction stream; a client-controlled unit (one whose
    /// `UnitBehavior+0xE5` is `4`) can in principle address other components,
    /// so the caller must route by this id and never mistake a non-avatar
    /// record for the avatar's.
    pub component: u16,
    /// The session counter byte the client sent; the echo must repeat it.
    pub session: u8,
    /// The number of 13-byte elements.
    pub count: u8,
    /// The client's own 13-byte records, sliced verbatim.
    pub data: Vec<u8>,
}

/// Parse a c2s `07 34 | u16/u32 addr | 0x65 | u8 session | u8 count | n*13B`.
///
/// The addressee width is sub-op dependent, so both are tried; the first that
/// yields a self-consistent element run wins. A `0x65` component update always
/// addresses a **component** id, so the returned [`MoverStream::component`] is
/// the low u16 of the addressee.
pub fn parse_mover(inner: &[u8]) -> Option<MoverStream> {
    if inner.len() < 5 || inner[0] != ENTITY_CHANNEL || inner[1] != ENTITY_REQUEST {
        return None;
    }
    for addr_len in [2usize, 4] {
        let base = 2 + addr_len;
        if inner.len() < base + 2 {
            continue;
        }
        if inner[base] != MOVE_SUB {
            continue;
        }
        let session = inner[base + 1];
        let count = inner[base + 2] as usize;
        let need = base + 3 + 13 * count;
        if count == 0 || need > inner.len() {
            continue;
        }
        return Some(MoverStream {
            component: u16::from_le_bytes([inner[2], inner[3]]),
            session,
            count: count as u8,
            data: inner[base + 3..need].to_vec(),
        });
    }
    None
}

/// Bytes of one mover element.
pub const MOVE_RECORD_SIZE: usize = 13;

/// Decode the `(x, y)` of the LAST complete 13-byte mover record in `data`, in
/// x256 world units, or `None` when `data` carries no complete record.
///
/// The c2s record layout is `[type:u8][heading:i32][X:i32][Y:i32]`: the
/// reference `_handle_client_move` reads `move type`, then `heading`, then
/// `pos_x`, then `pos_y` (movement.py:964-970), so X is the record's bytes
/// `5..9` and Y its bytes `9..13`. The live dungeon join's first mover frames
/// decode to `X = -650, Y = 870` -- the generated `dungeon00_level01` start
/// pose -- under this order and to nonsense under any other (see
/// [`tests::last_mover_position_reads_the_live_dungeon_start_frame`]).
pub fn last_mover_position(data: &[u8]) -> Option<[i32; 2]> {
    let count = data.len() / MOVE_RECORD_SIZE;
    if count == 0 {
        return None;
    }
    let record = &data[(count - 1) * MOVE_RECORD_SIZE..count * MOVE_RECORD_SIZE];
    let x = i32::from_le_bytes(record[5..9].try_into().ok()?);
    let y = i32::from_le_bytes(record[9..13].try_into().ok()?);
    Some([x, y])
}

/// The wire's mover-count ceiling (a single `u8` count).
pub const MOVE_ACK_MAX_RECORDS: usize = 255;

/// The staged owner move-ack: the client's records coalesced since the last
/// WorldInterval, with the session byte they share.
///
/// Between two intervals (~133 ms) the client emits several c2s batches, and
/// the ack must carry ALL of them -- not just the newest. Overwriting instead
/// of appending leaves the client unconfirmed for most of its own movement and
/// it judders.
#[derive(Debug, Default, Clone)]
pub struct MoveAck {
    session: u8,
    data: Vec<u8>,
}

impl MoveAck {
    /// Append a mover stream's records, coalescing up to the wire ceiling.
    /// Records from a different session byte replace the stage.
    pub fn queue(&mut self, session: u8, count: u8, data: &[u8]) {
        let n = (count as usize).min(data.len() / MOVE_RECORD_SIZE);
        if n == 0 {
            return;
        }
        let normalized = &data[..n * MOVE_RECORD_SIZE];
        if self.data.is_empty() || self.session != session {
            self.session = session;
            self.data = normalized.to_vec();
        } else {
            self.data.extend_from_slice(normalized);
        }
        let ceiling = MOVE_ACK_MAX_RECORDS * MOVE_RECORD_SIZE;
        if self.data.len() > ceiling {
            self.data.drain(..self.data.len() - ceiling);
        }
    }

    /// Take the staged `(session, count, data)`, or `None` when empty.
    pub fn take(&mut self) -> Option<(u8, u8, Vec<u8>)> {
        if self.data.is_empty() {
            return None;
        }
        let count = (self.data.len() / MOVE_RECORD_SIZE) as u8;
        let data = std::mem::take(&mut self.data);
        Some((self.session, count, data))
    }
}

/// The `0x35` local move-ack record (no `07` envelope, no `0x06` END -- it
/// rides inside the WorldInterval, which supplies the terminator):
/// `[0x35][u16 component][0x65][session][count][data][0x02][u32 hp]`.
pub fn mover_ack(
    component_id: u16,
    session: u8,
    count: u8,
    data: &[u8],
    hp: u32,
) -> Result<Vec<u8>, MoverError> {
    mover_ack_wire(component_id, session, count, data, hp_wire_u32(hp)?)
}

/// [`mover_ack`] taking the already-scaled x256 HP **wire** word.
pub fn mover_ack_wire(
    component_id: u16,
    session: u8,
    count: u8,
    data: &[u8],
    hp_wire: u32,
) -> Result<Vec<u8>, MoverError> {
    if component_id == 0 {
        return Err(MoverError::ZeroComponentId);
    }
    if count == 0 || data.len() != 13 * count as usize {
        return Err(MoverError::MoveAckShape);
    }
    let mut out = Vec::with_capacity(11 + data.len());
    out.push(CEM_COMPONENT_UPDATE);
    out.extend_from_slice(&component_id.to_le_bytes());
    out.push(MOVE_SUB);
    out.push(session);
    out.push(count);
    out.extend_from_slice(data);
    out.push(SYNCH_FLAG_HP);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    Ok(out)
}

/// The owner's local move-ack, gated on whether the avatar's HP synch compare
/// is live in this zone.
///
/// `EntitySynchInfo::Validate` (`0x5DD900`) runs the zero-tolerance HP compare
/// for the avatar only while `(*(behavior+0x11C)+0x95) & 1` is CLEAR. That byte
/// is the **town / peaceful-zone** flag, not a per-action input-authority bit:
/// the same `[+0x95] & 1` gates the client's "Can't use in town." skill check
/// (`0x4A4AE7`), and its constructor default is `0x10` (combat). A dungeon
/// therefore has no window in which the compare is skipped, and the client
/// never reports its own HP upstream — so any avatar-HP trailer the server
/// sends to the owner in a combat zone asserts a value it cannot know and
/// fatals the entity manager (error 1) the moment a mob has damaged the avatar.
///
/// The `0x35` grammar makes the trailer mandatory (the reader at `0x5DD840`
/// always consumes `flags` and, when `flags & 2`, an HP word), so the only safe
/// combat-zone move-ack is no move-ack at all: this returns an empty payload
/// and the WorldInterval carries only the budget/PathManager watchdog. In town
/// (and the tutorial) the peaceful bit is set, the compare is skipped, and the
/// echo is safe — it is returned with the avatar's wire word exactly as
/// [`mover_ack_wire`] builds it.
///
/// This does not by itself solve a client-triggered avatar-HP ack in a combat
/// zone; see the session's `DRF_HP_DIAG` diagnostic and the `+0xE5` client
/// hook note.
///
/// `trust_client_sim` is the `--trust-client-sim` CLI gate: when the paired
/// client hook (a per-entity `EntitySynchInfo::Validate` detour that bypasses
/// the HP compare for units the client simulates, `[EDI+0xE5] != 1`) is
/// installed, the owner's trailer is never compared and the combat-zone
/// suppression is lifted -- the verbatim echo is returned in every zone.
/// Default `false` keeps today's safe behaviour: town/tutorial only.
pub fn owner_move_ack_wire(
    combat_zone: bool,
    trust_client_sim: bool,
    component_id: u16,
    session: u8,
    count: u8,
    data: &[u8],
    hp_wire: u32,
) -> Vec<u8> {
    if combat_zone && !trust_client_sim {
        return Vec::new();
    }
    mover_ack_wire(component_id, session, count, data, hp_wire).unwrap_or_default()
}

/// The `(07,0D)` WorldInterval payload (after the `[07 0D]` envelope):
/// `[u32 tick][u32 0x21][u32 0x03][8 PathManager][move_ack]
/// [interval_messages][0x06]`.
///
/// This is the movement watchdog. The client consumes one entity-channel
/// message per ~133 ms and triples its world clock on a backlog, so the
/// server's local move-ack and every sustained per-unit stream (gnome movers,
/// actions, fidgets) ride here rather than as standalone frames.
pub fn world_interval_payload(tick: u32, move_ack: &[u8], interval_messages: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + move_ack.len() + interval_messages.len() + 1);
    out.extend_from_slice(&tick.to_le_bytes());
    out.extend_from_slice(&0x21u32.to_le_bytes());
    out.extend_from_slice(&0x03u32.to_le_bytes());
    out.extend_from_slice(&INTERVAL_PATHMANAGER_BLOCK);
    out.extend_from_slice(move_ack);
    out.extend_from_slice(interval_messages);
    out.push(END_MARKER);
    out
}

/// The `(07,35)` spawn-placement payload (after the `[07 35]` envelope):
/// `[u16 component][0x65][session][1][b0][f1][x][y][0x02][u32 hp][0x06]`.
///
/// A placement, not locomotion: the client is authoritative for its own
/// avatar and a synthesized record would only fight its prediction. It is
/// still the correct mechanism for a remote unit.
#[allow(clippy::too_many_arguments)]
pub fn placement_payload(
    component_id: u16,
    session: u8,
    b0: u8,
    f1: i32,
    x: i32,
    y: i32,
    hp: u32,
) -> Result<Vec<u8>, MoverError> {
    placement_payload_wire(component_id, session, b0, f1, x, y, hp_wire_u32(hp)?)
}

/// [`placement_payload`] taking the already-scaled x256 HP **wire** word.
#[allow(clippy::too_many_arguments)]
pub fn placement_payload_wire(
    component_id: u16,
    session: u8,
    b0: u8,
    f1: i32,
    x: i32,
    y: i32,
    hp_wire: u32,
) -> Result<Vec<u8>, MoverError> {
    if component_id == 0 {
        return Err(MoverError::ZeroComponentId);
    }
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&component_id.to_le_bytes());
    out.push(MOVE_SUB);
    out.push(session);
    out.push(0x01);
    out.push(b0);
    out.extend_from_slice(&f1.to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.push(SYNCH_FLAG_HP);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    out.push(END_MARKER);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_request_parses_and_rejects() {
        let inner = [0x07, 0x34, 0x35, 0x11, 0x64, 0x01];
        assert_eq!(
            parse_control_request(&inner),
            Some(ControlRequest {
                entity_id: 0x1135,
                on: true
            })
        );
        // A mover sub-op is not a control request.
        assert_eq!(
            parse_control_request(&[0x07, 0x34, 0x00, 0x10, 0x65, 0x00, 0x01, 0x03]),
            None
        );
        // Too short.
        assert_eq!(parse_control_request(&[0x07, 0x34, 0x00, 0x10]), None);
    }

    #[test]
    fn mover_stream_slices_the_client_records_verbatim() {
        // 07 34 <eid u16> 65 <session> <count=1> <13B>
        let mut inner = vec![0x07, 0x34, 0x00, 0x10, 0x65, 0x07, 0x01];
        let element = [0x03u8, 0, 0, 0x5a, 0, 0, 0x80, 0x01, 0, 0, 0, 0x40, 0xff];
        inner.extend_from_slice(&element);
        let mover = parse_mover(&inner).unwrap();
        assert_eq!(mover.session, 0x07);
        assert_eq!(mover.count, 1);
        assert_eq!(mover.data, element);
    }

    #[test]
    fn last_mover_position_reads_the_live_dungeon_start_frame() {
        // A real live c2s mover frame from a `dungeon00_level01` join
        // (avatar UnitBehavior cid 0xE0BF, sub 0x65, session 0, count 1):
        // record `00 00 18 00 00 00 76 fd ff 00 66 03 00`. Read as
        // `[type u8][heading i32][X i32][Y i32]` it is type 0, heading 24.0,
        // X = 0xFFFD7600 = -166400 (-650 x256), Y = 0x00036600 = 222720
        // (870 x256) -- exactly the generated dungeon start pose. Any other
        // field order yields a pose the zone never authorizes.
        let record = [
            0x00u8, 0x00, 0x18, 0x00, 0x00, 0x00, 0x76, 0xfd, 0xff, 0x00, 0x66, 0x03, 0x00,
        ];
        assert_eq!(record.len(), MOVE_RECORD_SIZE);
        assert_eq!(&record[1..5], &(24i32 * 256).to_le_bytes(), "heading");
        assert_eq!(last_mover_position(&record), Some([-650 * 256, 870 * 256]));

        // The LAST record wins when a frame batches several.
        let mut two = record.to_vec();
        two.extend_from_slice(&record);
        two[MOVE_RECORD_SIZE + 5..MOVE_RECORD_SIZE + 9]
            .copy_from_slice(&(7i32 * 256).to_le_bytes());
        two[MOVE_RECORD_SIZE + 9..MOVE_RECORD_SIZE + 13]
            .copy_from_slice(&(9i32 * 256).to_le_bytes());
        assert_eq!(last_mover_position(&two), Some([7 * 256, 9 * 256]));

        // No complete record -> no position.
        assert_eq!(last_mover_position(&[]), None);
        assert_eq!(last_mover_position(&record[..MOVE_RECORD_SIZE - 1]), None);
    }

    #[test]
    fn grant_payload_shape() {
        // hp 51200 -> wire 51200*256 = 0x00C80000. Payload after `[07 35]`.
        let payload = follow_client_payload(0xE005, true, 51200).unwrap();
        assert_eq!(payload.len(), 10);
        assert_eq!(&payload[0..2], &0xE005u16.to_le_bytes());
        assert_eq!(payload[2], FOLLOW_SUB);
        assert_eq!(payload[3], 0x01);
        assert_eq!(payload[4], SYNCH_FLAG_HP);
        assert_eq!(&payload[5..9], &0x00C8_0000u32.to_le_bytes());
        assert_eq!(payload[9], END_MARKER);
    }

    #[test]
    fn move_ack_echoes_the_records() {
        let data = [0x03u8; 13];
        let ack = mover_ack(0xE005, 0x07, 1, &data, 51200).unwrap();
        assert_eq!(ack[0], CEM_COMPONENT_UPDATE);
        assert_eq!(&ack[1..3], &0xE005u16.to_le_bytes());
        assert_eq!(ack[3], MOVE_SUB);
        assert_eq!(ack[4], 0x07);
        assert_eq!(ack[5], 1);
        assert_eq!(&ack[6..19], &data);
        assert_eq!(ack[19], SYNCH_FLAG_HP);
        assert_eq!(&ack[20..24], &0x00C8_0000u32.to_le_bytes());
    }

    #[test]
    fn wire_variants_do_not_rescale_the_hp_word() {
        // 50539 is the observed Warlock word; it is NOT a multiple of 256 and
        // must land on the wire verbatim, never multiplied again.
        let grant = follow_client_payload_wire(0xE005, true, 50_539).unwrap();
        assert_eq!(&grant[5..9], &50_539u32.to_le_bytes());
        let data = [0x03u8; 13];
        let ack = mover_ack_wire(0xE005, 0x07, 1, &data, 50_539).unwrap();
        assert_eq!(&ack[20..24], &50_539u32.to_le_bytes());
        let placement = placement_payload_wire(0xE005, 0x00, 0x03, 0, 1, 2, 50_539).unwrap();
        assert_eq!(
            &placement[placement.len() - 5..placement.len() - 1],
            &50_539u32.to_le_bytes()
        );
    }

    #[test]
    fn move_ack_coalesces_appending_not_overwriting() {
        let mut staged = MoveAck::default();
        let a = [0x01u8; 13];
        let b = [0x02u8; 13];
        staged.queue(0x07, 1, &a);
        staged.queue(0x07, 1, &b);
        let (session, count, data) = staged.take().unwrap();
        assert_eq!(session, 0x07);
        assert_eq!(count, 2, "both batches are carried, not just the newest");
        assert_eq!(&data[..13], &a);
        assert_eq!(&data[13..], &b);
        // A different session byte replaces the stage.
        staged.queue(0x08, 1, &a);
        let (session, count, _) = staged.take().unwrap();
        assert_eq!(session, 0x08);
        assert_eq!(count, 1);
        assert!(staged.take().is_none());
    }

    #[test]
    fn move_ack_keeps_the_most_recent_255_records() {
        let mut staged = MoveAck::default();
        for i in 0..300u32 {
            staged.queue(0, 1, &[i as u8; 13]);
        }
        let (_, count, data) = staged.take().unwrap();
        assert_eq!(count as usize, MOVE_ACK_MAX_RECORDS);
        assert_eq!(data.len(), MOVE_ACK_MAX_RECORDS * MOVE_RECORD_SIZE);
        // The tail is kept: the last record is the 299th.
        assert_eq!(data[data.len() - 13], (299u32 % 256) as u8);
    }

    #[test]
    fn owner_move_ack_is_dropped_in_a_combat_zone_and_kept_in_town() {
        let data = [0x03u8; 13];
        // A dungeon has no synch-compare bypass, so the owner move-ack is
        // suppressed entirely (the `0x35` trailer cannot be omitted).
        assert!(
            owner_move_ack_wire(true, false, 0xE005, 0x07, 1, &data, 50_539).is_empty(),
            "a combat-zone move-ack must not carry the avatar HP trailer"
        );
        // Town/tutorial sets the peaceful bit, so the echo is safe and keeps
        // the avatar's wire word verbatim.
        let ack = owner_move_ack_wire(false, false, 0xE005, 0x07, 1, &data, 50_539);
        assert_eq!(ack[0], CEM_COMPONENT_UPDATE);
        assert_eq!(&ack[1..3], &0xE005u16.to_le_bytes());
        assert_eq!(ack[19], SYNCH_FLAG_HP);
        assert_eq!(&ack[20..24], &50_539u32.to_le_bytes());
    }

    #[test]
    fn trusted_client_keeps_the_combat_zone_move_ack() {
        // With the `+0xE5` client-sim hook installed the owner's trailer is
        // never compared, so the combat-zone suppression is lifted and the
        // verbatim echo is sent -- byte-for-byte the same as in town.
        let data = [0x03u8; 13];
        let combat = owner_move_ack_wire(true, true, 0xE005, 0x07, 1, &data, 50_539);
        assert_eq!(combat[0], CEM_COMPONENT_UPDATE);
        assert_eq!(&combat[1..3], &0xE005u16.to_le_bytes());
        assert_eq!(combat[3], MOVE_SUB);
        assert_eq!(combat[4], 0x07);
        assert_eq!(combat[5], 1);
        assert_eq!(&combat[6..19], &data);
        assert_eq!(combat[19], SYNCH_FLAG_HP);
        assert_eq!(&combat[20..24], &50_539u32.to_le_bytes());
        assert_eq!(
            combat,
            owner_move_ack_wire(false, true, 0xE005, 0x07, 1, &data, 50_539),
            "the hook makes a combat-zone ack identical to a town ack"
        );
    }

    #[test]
    fn owner_move_ack_fails_closed_on_a_bad_shape() {
        // A malformed record is dropped rather than sent half-built, even in
        // town where the trailer would otherwise be legal.
        assert!(owner_move_ack_wire(false, false, 0xE005, 0x07, 2, &[0u8; 13], 50_539).is_empty());
        assert!(owner_move_ack_wire(false, false, 0, 0x07, 1, &[0u8; 13], 50_539).is_empty());
        // The trusted-client gate does not relax the shape check.
        assert!(owner_move_ack_wire(true, true, 0, 0x07, 1, &[0u8; 13], 50_539).is_empty());
    }

    #[test]
    fn world_interval_layout() {
        let ack = mover_ack(0xE005, 0, 1, &[0u8; 13], 51200).unwrap();
        let body = world_interval_payload(42, &ack, &[]);
        assert_eq!(&body[0..4], &42u32.to_le_bytes());
        assert_eq!(&body[4..8], &0x21u32.to_le_bytes());
        assert_eq!(&body[8..12], &0x03u32.to_le_bytes());
        assert_eq!(&body[12..20], &INTERVAL_PATHMANAGER_BLOCK);
        assert_eq!(&body[20..20 + ack.len()], ack.as_slice());
        assert_eq!(*body.last().unwrap(), END_MARKER);
    }
}
