// SPDX-License-Identifier: GPL-3.0-or-later

//! The server→client Action sub-op of a `0x35` ComponentUpdate.
//!
//! A `0x35` ComponentUpdate addressed to a unit's **Behavior** component with
//! `updateType = 0x01` is the action-dispatch channel. After the u16
//! `componentID` and the `0x01` update type, the body is:
//!
//! ```text
//! 0x35 | componentID:u16 | 0x01 | layer:u8 | classID:u8 | <readData payload> | <synch trailer>
//! ```
//!
//! [`action_sub_op`] builds the `[0x01][layer][classID][payload]` body;
//! [`action_frame`] wraps it into a standalone `0x35` frame with the mandatory
//! `EntitySynchInfo` trailer and the `0x06` CEM terminator, matching the worked
//! layouts in the action-channel protocol notes. The per-action
//! payload builders ([`kill_payload`], [`attack_target2_payload`], …) emit the
//! exact `readData` widths.
//!
//! # The layer byte is `Behavior+0x7D` — never invent it
//!
//! `Behavior::processUpdate` `case 0x01` compares the wire `layer` byte against
//! the target Behavior's generation counter at `Behavior+0x7D`. Equal →
//! `doActionLocal` → the action starts; unequal → `onDoActionFailed(reason 3)`
//! and the action is dropped. That counter is set from the trailing byte
//! of the behavior's create payload (`Behavior::readInit`) and advanced only by
//! **locally-driven** interrupts/clears/terminations/removals — and the client
//! advertises its current value in its own outbound `0x01`
//! (`Behavior::doAction`). The server must therefore **echo the value the client
//! advertised** (or the value it itself chose when it created the behavior); it
//! must not invent a constant. Callers pass `layer` explicitly for exactly this
//! reason.
//!
//! # Use against server-owned / displayed units only
//!
//! The `0x35` trailer's HP word (`flags & 0x02`) is not advisory: on an entity
//! whose control-mode byte `UnitBehavior+0xE5 == 1` (display — the client
//! reproduces it from the server) `EntitySynchInfo::Validate` runs a
//! zero-tolerance HP compare against `Unit+0x2F0`, and a contradiction latches
//! the sticky entity-manager error `CEM+0xAC0 = 1` for the session. These HP-asserting actions (`Kill`,
//! `Ressurect`, `Spawn`) are the intended channel for units the server owns and
//! the client merely displays.
//!
//! Do **not** send `Kill` (or any HP-asserting action) to a unit the receiving
//! client *simulates* (`UnitBehavior+0xE5 == 4`, e.g. the local avatar): its HP
//! and its `+0x7D` advance locally, so the server cannot know either, and a
//! trailer HP that disagrees with the client's simulated value hard-fails the
//! entity manager. `Remove` (`0x40`, zero payload) and `UnSpawn` (`0x0F`) are
//! the structural removals that read no value the server must guess.
//!
//! The claims here are derived from static analysis of `DungeonRunners.exe`;
//! no capture or third-party server is a source.

use crate::cem::END_MARKER;
use crate::mover::{CEM_COMPONENT_UPDATE, SYNCH_FLAG_HP};

/// The `Behavior::processUpdate` action sub-op (`updateType = 0x01`).
pub const ACTION_UPDATE_TYPE: u8 = 0x01;

// Action-registry class ids (`Action::Registry::registerClass`). The value is
// the `classID` byte after `layer`.
/// `MoveTo`.
pub const MOVE_TO_ACTION_CLASS: u8 = 0x01;
/// `Spawn`.
pub const SPAWN_ACTION_CLASS: u8 = 0x04;
/// `Activate` (reader shared with `Follow`).
pub const ACTIVATE_ACTION_CLASS: u8 = 0x06;
/// `KnockBack` (reader shared with `KnockDown`).
pub const KNOCKBACK_ACTION_CLASS: u8 = 0x0A;
/// `KnockDown`.
pub const KNOCKDOWN_ACTION_CLASS: u8 = 0x0B;
/// `Stun`.
pub const STUN_ACTION_CLASS: u8 = 0x0C;
/// `SearchForAttack` (reader shared with `AttackTarget2`).
pub const SEARCH_FOR_ATTACK_ACTION_CLASS: u8 = 0x0D;
/// `SpawnAnimation`.
pub const SPAWN_ANIMATION_ACTION_CLASS: u8 = 0x0E;
/// `UnSpawn`.
pub const UNSPAWN_ACTION_CLASS: u8 = 0x0F;
/// `Dodge`.
pub const DODGE_ACTION_CLASS: u8 = 0x10;
/// `WarpTo`.
pub const WARP_TO_ACTION_CLASS: u8 = 0x11;
/// `Ambush`.
pub const AMBUSH_ACTION_CLASS: u8 = 0x12;
/// `MoveInDirectionAction` (reader shared with `TurnAction`).
pub const MOVE_IN_DIRECTION_ACTION_CLASS: u8 = 0x13;
/// `TurnAction`.
pub const TURN_ACTION_CLASS: u8 = 0x14;
/// `Wander` (reader shared with `Immobilize`).
pub const WANDER_ACTION_CLASS: u8 = 0x15;
/// `Follow`.
pub const FOLLOW_ACTION_CLASS: u8 = 0x16;
/// `PlayAnimation`.
pub const PLAY_ANIMATION_ACTION_CLASS: u8 = 0x20;
/// `FaceTarget`.
pub const FACE_TARGET_ACTION_CLASS: u8 = 0x21;
/// `Wait`.
pub const WAIT_ACTION_CLASS: u8 = 0x22;
/// `Immobilize`.
pub const IMMOBILIZE_ACTION_CLASS: u8 = 0x23;
/// `Idle`.
pub const IDLE_ACTION_CLASS: u8 = 0x2F;
/// `Ressurect`.
pub const RESSURECT_ACTION_CLASS: u8 = 0x30;
/// `Remove` (the only zero-payload action).
pub const REMOVE_ACTION_CLASS: u8 = 0x40;
/// `Hide`.
pub const HIDE_ACTION_CLASS: u8 = 0x41;
/// `SetBlocking`.
pub const SET_BLOCKING_ACTION_CLASS: u8 = 0x45;
/// `Flee`.
pub const FLEE_ACTION_CLASS: u8 = 0x4F;
/// `UseTarget`.
pub const USE_TARGET_ACTION_CLASS: u8 = 0x50;
/// `UsePosition`.
pub const USE_POSITION_ACTION_CLASS: u8 = 0x51;
/// `Use`.
pub const USE_ACTION_CLASS: u8 = 0x52;
/// `UseItemTarget` (variable payload).
pub const USE_ITEM_TARGET_ACTION_CLASS: u8 = 0x53;
/// `UseItemPosition` (variable payload).
pub const USE_ITEM_POSITION_ACTION_CLASS: u8 = 0x54;
/// `UseItem` (variable payload).
pub const USE_ITEM_ACTION_CLASS: u8 = 0x55;
/// `DoEffect` (variable payload; reader/writer asymmetric).
pub const DO_EFFECT_ACTION_CLASS: u8 = 0x83;
/// `RetrieveItem`.
pub const RETRIEVE_ITEM_ACTION_CLASS: u8 = 0xA0;
/// `ConvertItemsToGold`.
pub const CONVERT_ITEMS_TO_GOLD_ACTION_CLASS: u8 = 0xA1;
/// `AttackTarget2`.
pub const ATTACK_TARGET2_ACTION_CLASS: u8 = 0xF0;
/// `Kill`.
pub const KILL_ACTION_CLASS: u8 = 0xFE;
/// `Die`.
pub const DIE_ACTION_CLASS: u8 = 0xFF;

/// Whether `class_id` is one of the 38 ids `Action::Registry` registers
/// (see the action-channel notes). An unregistered id makes the client's
/// `createAction` fail and the subsequent action dispatch dereference a null
/// instance, so builders here fail closed on it.
pub fn is_registered_action_class(class_id: u8) -> bool {
    matches!(
        class_id,
        0x01 | 0x04
            | 0x06
            | 0x0A
            | 0x0B
            | 0x0C
            | 0x0D
            | 0x0E
            | 0x0F
            | 0x10
            | 0x11
            | 0x12
            | 0x13
            | 0x14
            | 0x15
            | 0x16
            | 0x20
            | 0x21
            | 0x22
            | 0x23
            | 0x2F
            | 0x30
            | 0x40
            | 0x41
            | 0x45
            | 0x4F
            | 0x50
            | 0x51
            | 0x52
            | 0x53
            | 0x54
            | 0x55
            | 0x83
            | 0xA0
            | 0xA1
            | 0xF0
            | 0xFE
            | 0xFF
    )
}

/// The exact `readData` payload length for a fixed-width action class, or
/// `None` for the variable-width `UseItem*`/`DoEffect` readers
/// (see the action-channel notes). Unknown ids also return `None`; callers must
/// gate on [`is_registered_action_class`] first (as [`action_frame`] does).
///
/// The base reader (`Kill`/`Die`/`Ressurect`/`Stun`/`UnSpawn`/`Dodge`/`Idle`/
/// `Flee`) still consumes **one** byte — the `Action+0x6D` request token, not an
/// empty payload. `Remove` (`0x40`) is the sole zero-byte reader.
pub fn fixed_payload_len(class_id: u8) -> Option<usize> {
    match class_id {
        0x01 => Some(9),  // MoveTo
        0x04 => Some(15), // Spawn
        0x06 => Some(3),  // Activate
        0x0A => Some(5),  // KnockBack
        0x0B => Some(5),  // KnockDown
        0x0C => Some(1),  // Stun
        0x0D => Some(3),  // SearchForAttack
        0x0E => Some(5),  // SpawnAnimation
        0x0F => Some(1),  // UnSpawn
        0x10 => Some(1),  // Dodge
        0x11 => Some(13), // WarpTo
        0x12 => Some(13), // Ambush
        0x13 => Some(5),  // MoveInDirectionAction
        0x14 => Some(5),  // TurnAction
        0x15 => Some(3),  // Wander
        0x16 => Some(3),  // Follow
        0x20 => Some(17), // PlayAnimation
        0x21 => Some(2),  // FaceTarget
        0x22 => Some(4),  // Wait
        0x23 => Some(3),  // Immobilize
        0x2F => Some(1),  // Idle
        0x30 => Some(1),  // Ressurect
        0x40 => Some(0),  // Remove
        0x41 => Some(3),  // Hide
        0x45 => Some(1),  // SetBlocking
        0x4F => Some(1),  // Flee
        0x50 => Some(4),  // UseTarget
        0x51 => Some(14), // UsePosition
        0x52 => Some(2),  // Use
        0x53 => None,     // UseItemTarget  (variable)
        0x54 => None,     // UseItemPosition (variable)
        0x55 => None,     // UseItem         (variable)
        0x83 => None,     // DoEffect        (variable, asymmetric)
        0xA0 => Some(4),  // RetrieveItem
        0xA1 => Some(19), // ConvertItemsToGold
        0xF0 => Some(3),  // AttackTarget2
        0xFE => Some(1),  // Kill
        0xFF => Some(1),  // Die
        _ => None,
    }
}

/// A malformed action frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionError {
    /// Component id zero is the unassigned sentinel.
    ZeroComponentId,
    /// The class id is not one of the registered actions.
    UnknownClassId(u8),
    /// A fixed-width action carried the wrong number of payload bytes.
    PayloadWidth {
        /// The width the class's `readData` consumes.
        expected: usize,
        /// The width the caller supplied.
        got: usize,
    },
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionError::ZeroComponentId => write!(f, "component ID 0 is the unassigned sentinel"),
            ActionError::UnknownClassId(id) => {
                write!(f, "action class id {id:#04x} is not registered")
            }
            ActionError::PayloadWidth { expected, got } => write!(
                f,
                "action payload is {got} bytes, but its reader consumes {expected}"
            ),
        }
    }
}

impl std::error::Error for ActionError {}

/// Build the Action sub-op body `[0x01][layer][classID][payload]`.
///
/// This is the portion that follows the u16 `componentID` inside a `0x35`
/// ComponentUpdate; it does **not** include the `EntitySynchInfo` trailer
/// (which `processComponentUpdate` reads after the action) nor the `0x06`
/// stream terminator. `layer` is the target Behavior's `+0x7D` generation byte
/// (see the module docs) and `class_id` is one of the `*_ACTION_CLASS`
/// constants.
pub fn action_sub_op(layer: u8, class_id: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + payload.len());
    out.push(ACTION_UPDATE_TYPE);
    out.push(layer);
    out.push(class_id);
    out.extend_from_slice(payload);
    out
}

/// Build a standalone `0x35` Action frame:
/// `[0x35][componentID:u16][0x01][layer][classID][payload][trailer][0x06]`.
///
/// `component_id` is the unit's **Behavior component** id (never an entity id);
/// `layer` is that Behavior's `+0x7D` byte (see the module docs). `hp_wire` is
/// the x256 HP word for the synch trailer: `Some(hp)` writes `flags = 0x02`
/// followed by the u32 LE `hp`; `None` writes `flags = 0x00` and omits the word.
///
/// The action's `readData` runs before the trailer is validated, so for an
/// HP-mutating action the word must be the **post-action** value (`Kill` → 0;
/// `Ressurect`/`Spawn` → max HP × 256) when the compare is live. Fails
/// closed on a zero component id, an unregistered class id, or a payload whose
/// length disagrees with a fixed-width reader.
pub fn action_frame(
    component_id: u16,
    layer: u8,
    class_id: u8,
    payload: &[u8],
    hp_wire: Option<u32>,
) -> Result<Vec<u8>, ActionError> {
    if component_id == 0 {
        return Err(ActionError::ZeroComponentId);
    }
    if !is_registered_action_class(class_id) {
        return Err(ActionError::UnknownClassId(class_id));
    }
    if let Some(expected) = fixed_payload_len(class_id) {
        if payload.len() != expected {
            return Err(ActionError::PayloadWidth {
                expected,
                got: payload.len(),
            });
        }
    }

    let mut out = Vec::with_capacity(8 + payload.len() + 5);
    out.push(CEM_COMPONENT_UPDATE);
    out.extend_from_slice(&component_id.to_le_bytes());
    out.extend_from_slice(&action_sub_op(layer, class_id, payload));
    match hp_wire {
        Some(hp) => {
            out.push(SYNCH_FLAG_HP);
            out.extend_from_slice(&hp.to_le_bytes());
        }
        None => out.push(0x00),
    }
    out.push(END_MARKER);
    Ok(out)
}

/// `Kill` payload: the one-byte `Action+0x6D` token (base reader `0x526480`).
///
/// `Kill::start` sets the owning Unit's HP to exactly 0; it carries no damage
/// value. The trailer of a [`action_frame`] that uses this must therefore
/// be `Some(0)` when the synch compare is live.
pub fn kill_payload(token: u8) -> Vec<u8> {
    vec![token]
}

/// `Ressurect` payload: the one-byte token (base reader). `Unit::onRessurect`
/// sets HP to max HP × 256.
pub fn ressurect_payload(token: u8) -> Vec<u8> {
    vec![token]
}

/// `Die` payload: the one-byte token (base reader). `Die::start` drives an
/// animation only and does **not** touch HP — distinct from `Kill`.
pub fn die_payload(token: u8) -> Vec<u8> {
    vec![token]
}

/// `AttackTarget2` payload: `u8 token + u16 targetEntityId` (reader `0x52D320`,
/// shared with `SearchForAttack`). `target_entity_id` is the target
/// unit's entity id, resolved into the action's `Action+0x94` watcher.
pub fn attack_target2_payload(token: u8, target_entity_id: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(3);
    out.push(token);
    out.extend_from_slice(&target_entity_id.to_le_bytes());
    out
}

/// `Follow` payload: `u8 token + u16 targetEntityId` (reader `0x5227A0`, shared
/// with `Activate`).
pub fn follow_payload(token: u8, target_entity_id: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(3);
    out.push(token);
    out.extend_from_slice(&target_entity_id.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked `Kill` layout from the action-channel notes.
    const KILL_WORKED: [u8; 13] = [
        0x35, 0x05, 0xE0, 0x01, 0x00, 0xFE, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x06,
    ];

    #[test]
    fn kill_frame_matches_the_worked_layout() {
        let frame = action_frame(
            0xE005,
            0x00,
            KILL_ACTION_CLASS,
            &kill_payload(0x00),
            Some(0),
        )
        .expect("worked Kill frame");
        assert_eq!(frame, KILL_WORKED);
    }

    #[test]
    fn attack_target2_frame_matches_the_worked_layout() {
        // hp = current HP x256; choose 51200 -> 0x00C80000 (LE 00 00 C8 00).
        let frame = action_frame(
            0xE005,
            0x00,
            ATTACK_TARGET2_ACTION_CLASS,
            &attack_target2_payload(0x00, 0x1234),
            Some(0x00C8_0000),
        )
        .expect("worked AttackTarget2 frame");
        assert_eq!(
            frame,
            vec![
                0x35, 0x05, 0xE0, // 0x35 + componentID 0xE005
                0x01, 0x00, // update type + layer
                0xF0, // AttackTarget2
                0x00, 0x34, 0x12, // token + targetEntityId 0x1234
                0x02, 0x00, 0x00, 0xC8, 0x00, // synch trailer: flags + hp
                0x06, // CEM END
            ]
        );
    }

    #[test]
    fn sub_op_body_is_tag_layer_class_payload() {
        assert_eq!(
            action_sub_op(0x07, KILL_ACTION_CLASS, &[0x2A]),
            vec![0x01, 0x07, 0xFE, 0x2A]
        );
        assert_eq!(
            action_sub_op(0x00, FOLLOW_ACTION_CLASS, &[]),
            vec![0x01, 0x00, 0x16]
        );
    }

    #[test]
    fn specific_payloads_have_the_readdata_widths() {
        assert_eq!(fixed_payload_len(KILL_ACTION_CLASS), Some(1));
        assert_eq!(fixed_payload_len(DIE_ACTION_CLASS), Some(1));
        // Kill and Die differ in effect but not in reader shape.
        assert_eq!(kill_payload(0x00), vec![0x00]);
        assert_eq!(die_payload(0x00), vec![0x00]);
        assert_eq!(ressurect_payload(0x00), vec![0x00]);
        assert_eq!(fixed_payload_len(ATTACK_TARGET2_ACTION_CLASS), Some(3));
        assert_eq!(attack_target2_payload(0x00, 0x1234), vec![0x00, 0x34, 0x12]);
        assert_eq!(fixed_payload_len(FOLLOW_ACTION_CLASS), Some(3));
        assert_eq!(follow_payload(0x00, 0x1234), vec![0x00, 0x34, 0x12]);
        // Remove is the sole zero-payload action.
        assert_eq!(fixed_payload_len(REMOVE_ACTION_CLASS), Some(0));
        // The variable-width readers accept any length.
        assert_eq!(fixed_payload_len(DO_EFFECT_ACTION_CLASS), None);
        assert_eq!(fixed_payload_len(USE_ITEM_TARGET_ACTION_CLASS), None);
    }

    #[test]
    fn the_registered_table_covers_the_documented_ids() {
        for class_id in [
            KILL_ACTION_CLASS,
            RESSURECT_ACTION_CLASS,
            ATTACK_TARGET2_ACTION_CLASS,
            FOLLOW_ACTION_CLASS,
            DIE_ACTION_CLASS,
            DO_EFFECT_ACTION_CLASS,
            SPAWN_ACTION_CLASS,
            UNSPAWN_ACTION_CLASS,
            ACTIVATE_ACTION_CLASS,
            MOVE_TO_ACTION_CLASS,
            REMOVE_ACTION_CLASS,
        ] {
            assert!(is_registered_action_class(class_id), "{class_id:#04x}");
        }
        assert!(!is_registered_action_class(0x99));
        assert!(!is_registered_action_class(0x00));
    }

    #[test]
    fn frame_without_hp_carries_a_zero_flags_trailer() {
        let frame = action_frame(0xE005, 0x00, REMOVE_ACTION_CLASS, &[], None).unwrap();
        assert_eq!(
            frame,
            vec![0x35, 0x05, 0xE0, 0x01, 0x00, 0x40, 0x00, 0x06],
            "flags 0x00 omits the HP word"
        );
    }

    #[test]
    fn fail_closed_on_zero_component_and_unknown_class() {
        assert_eq!(
            action_frame(0, 0x00, KILL_ACTION_CLASS, &kill_payload(0), Some(0)),
            Err(ActionError::ZeroComponentId)
        );
        assert_eq!(
            action_frame(0xE005, 0x00, 0x99, &[0x00], Some(0)),
            Err(ActionError::UnknownClassId(0x99))
        );
    }

    #[test]
    fn fail_closed_on_wrong_payload_width() {
        assert_eq!(
            action_frame(0xE005, 0x00, KILL_ACTION_CLASS, &[0x00, 0x00], Some(0)),
            Err(ActionError::PayloadWidth {
                expected: 1,
                got: 2
            })
        );
        assert_eq!(
            action_frame(0xE005, 0x00, REMOVE_ACTION_CLASS, &[0x00], None),
            Err(ActionError::PayloadWidth {
                expected: 0,
                got: 1
            })
        );
        // A wrong width never emits a partial frame.
        assert!(action_frame(0xE005, 0x00, ATTACK_TARGET2_ACTION_CLASS, &[0x00], Some(0)).is_err());
    }

    #[test]
    fn trailer_hp_is_written_verbatim_not_rescaled() {
        // 50539 is the observed Warlock word; it is not a multiple of 256 and
        // must land on the wire as-is.
        let frame = action_frame(
            0xE005,
            0x00,
            ATTACK_TARGET2_ACTION_CLASS,
            &attack_target2_payload(0x00, 0x1234),
            Some(50_539),
        )
        .unwrap();
        assert_eq!(
            &frame[frame.len() - 5..frame.len() - 1],
            &50_539u32.to_le_bytes()
        );
        assert_eq!(*frame.last().unwrap(), END_MARKER);
    }

    #[test]
    fn layer_byte_is_echoed_unchanged() {
        let frame =
            action_frame(0xE005, 0x5A, KILL_ACTION_CLASS, &kill_payload(0), Some(0)).unwrap();
        assert_eq!(
            frame[4], 0x5A,
            "the caller's Behavior+0x7D is echoed verbatim"
        );
        assert_eq!(frame[3], ACTION_UPDATE_TYPE);
    }
}
