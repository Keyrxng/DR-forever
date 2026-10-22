// SPDX-License-Identifier: GPL-3.0-or-later

//! The in-world `avatar.base.Equipment` and `Manipulators` item payloads.
//!
//! `Equipment::readInit`/`writeInit` are one `GCObject::ReadChildData<Item>`
//! call each, so the create payload after its post byte IS the child list and
//! nothing else. `Manipulators::readInit` reads the same `Item::readData`
//! (`vtable+0xF0`) and then the item class's own `vtable+0x100` readState, so a
//! manipulator item entry is its Equipment entry followed by that body -- zero
//! bytes for every DFC root except the two weapon roots.
//!
//! The item fields come from [`drf_content`] (`game.pkg`); the field order and
//! the readState lengths are the client readers' own.

use std::fmt;

use drf_content::ClassItem;

use crate::cem::{END_MARKER, TYPE_TAG_CLASSNAME};

/// `Item+0x80..0x82` for an equipped item: no bag cell.
pub const EQUIPMENT_ITEM_GRID_BYTES: [u8; 3] = [0xFF, 0xFF, 0x01];

/// A Normal-tier item's flags byte, with no trailing u16.
pub const EQUIPMENT_ITEM_FLAGS_PLAIN: u8 = 0x00;

/// `ItemModifier::readData`'s flags byte for a server-assigned modifier.
pub const EQUIPMENT_MOD_BODY_FLAGS: u8 = 0x01;

/// The inclusive bound on an item's authored status-byte run.
pub const EQUIPMENT_MAX_AUTHORED_SLOTS: usize = 24;

/// The level byte of a starting item's server-assigned modifier.
pub const EQUIPMENT_STARTING_MOD_LEVEL: u8 = 1;

/// The in-world `Equipment` component classname.
pub const AVATAR_EQUIPMENT_CLASS: &str = "avatar.base.Equipment";

/// The post byte of the `Equipment` create payload (`[0x01][count]`).
const EQUIPMENT_POST: u8 = 0x01;

/// The `vtable+0x100` readState body a Manipulators item entry carries, keyed
/// on the item's DFC root. Each body is all-zero, the state a default-
/// constructed item of that class is in. A root absent here has no shown shape.
const MANIPULATOR_READSTATE_EMPTY: [(&str, &[u8]); 4] = [
    ("MeleeWeapon", &[0x00; 5]),
    ("RangedWeapon", &[0x00; 4]),
    ("Armor", b""),
    ("Item", b""),
];

/// The byte length each readState routine reads, asserted against the body.
const MANIPULATOR_READSTATE_LEN: [(&str, usize); 4] = [
    ("MeleeWeapon", 5),
    ("RangedWeapon", 4),
    ("Armor", 0),
    ("Item", 0),
];

/// `0xFF` + printable ASCII + NUL, the `readType` className form.
pub(crate) fn encode_list_cstr(value: &str) -> Vec<u8> {
    let raw = value.split('\u{0}').next().unwrap_or("");
    assert!(!raw.is_empty(), "class name is empty");
    assert!(
        raw.bytes().all(|b| (0x20..=0x7E).contains(&b)),
        "class name {value:?} is not printable ASCII"
    );
    let mut out = Vec::with_capacity(raw.len() + 2);
    out.push(TYPE_TAG_CLASSNAME);
    out.extend_from_slice(raw.as_bytes());
    out.push(0);
    out
}

/// A `ReadChildData` list count: one byte, with `0xFF` escaping a following u16.
pub(crate) fn push_list_count(out: &mut Vec<u8>, count: usize) {
    if count > 0xFE {
        out.push(0xFF);
        out.extend_from_slice(&(count as u16).to_le_bytes());
    } else {
        out.push(count as u8);
    }
}

/// The level to write for `item`: the caller's override, or the corpus level.
fn resolved_level(level: Option<u8>, item: &ClassItem) -> u8 {
    match level {
        Some(level) => level,
        None => {
            assert!(
                (0..=0xFF).contains(&item.level),
                "item level {} out of range",
                item.level
            );
            item.level as u8
        }
    }
}

/// `Item::readData`'s body, everything after the type tag and classname.
///
/// Shared by the Equipment list (`ReadChildData<Item>`), the Manipulators item
/// entries (`Manipulators::readInit`) and a ground drop's own body
/// ([`crate::create::DroppedItemBody`]); all read the same routine, so they must
/// write the same bytes.
pub(crate) fn item_readdata_body(
    slot: i64,
    level: u8,
    authored: usize,
    mods: &[String],
    mod_level: u8,
) -> Vec<u8> {
    assert!(
        (0..=0xFFFF_FFFF).contains(&slot),
        "item slot {slot} is outside u32"
    );
    assert!(
        authored <= EQUIPMENT_MAX_AUTHORED_SLOTS,
        "item authored modifier count {authored} out of range"
    );
    let mut out = Vec::new();
    out.extend_from_slice(&(slot as u32).to_le_bytes());
    out.extend_from_slice(&EQUIPMENT_ITEM_GRID_BYTES);
    out.push(level);
    out.push(EQUIPMENT_ITEM_FLAGS_PLAIN);
    out.extend(std::iter::repeat_n(0u8, authored));
    push_list_count(&mut out, mods.len());
    for modifier in mods {
        out.extend_from_slice(&encode_list_cstr(modifier));
        out.push(EQUIPMENT_MOD_BODY_FLAGS);
        out.push(mod_level);
    }
    out
}

/// One `Item::readData` entry, in the client's own field order.
///
/// The layout is the type tag + ASCIIZ, the slot u32, the three grid bytes, the
/// level byte, the flags byte, `authored` status bytes, the modifier count, and
/// then each named modifier as tag + ASCIIZ + its two-byte body.
pub fn equipment_item_entry(
    fqn: &str,
    slot: i64,
    level: u8,
    authored: usize,
    mods: &[String],
    mod_level: u8,
) -> Vec<u8> {
    let mut out = encode_list_cstr(fqn);
    out.extend_from_slice(&item_readdata_body(slot, level, authored, mods, mod_level));
    out
}

/// One `Manipulators` item entry: the Equipment entry, then its readState.
///
/// `dfc` is the item's DFC root; it selects the readState body and nothing
/// else. An unknown root panics rather than framing by arithmetic.
pub fn item_manipulator_entry(
    fqn: &str,
    slot: i64,
    level: u8,
    authored: usize,
    mods: &[String],
    dfc: &str,
    mod_level: u8,
) -> Vec<u8> {
    let mut out = encode_list_cstr(fqn);
    out.extend_from_slice(&item_readdata_body(slot, level, authored, mods, mod_level));
    out.extend_from_slice(&item_manipulator_readstate(dfc).unwrap_or_else(|err| panic!("{err}")));
    out
}

/// The `[0x01][count]` + entries create payload of `avatar.base.Equipment`.
///
/// `level` overrides the corpus level for every item; `mod_level` overrides the
/// starting modifier level. Both default to the corpus's own values.
pub fn class_starting_equipment_payload(
    items: &[ClassItem],
    level: Option<u8>,
    mod_level: Option<u8>,
) -> Vec<u8> {
    let mod_level = mod_level.unwrap_or(EQUIPMENT_STARTING_MOD_LEVEL);
    let mut out = Vec::new();
    out.push(EQUIPMENT_POST);
    push_list_count(&mut out, items.len());
    for item in items {
        out.extend_from_slice(&equipment_item_entry(
            &item.fqn,
            item.slot,
            resolved_level(level, item),
            item.authored,
            &item.mods,
            mod_level,
        ));
    }
    out
}

/// The `Manipulators` item region for `items`, one entry per worn item.
pub fn class_starting_item_manipulators_bytes(
    items: &[ClassItem],
    level: Option<u8>,
    mod_level: Option<u8>,
) -> Vec<u8> {
    let mod_level = mod_level.unwrap_or(EQUIPMENT_STARTING_MOD_LEVEL);
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(&item_manipulator_entry(
            &item.fqn,
            item.slot,
            resolved_level(level, item),
            item.authored,
            &item.mods,
            &item.dfc,
            mod_level,
        ));
    }
    out
}

/// The `vtable+0x100` readState body a Manipulators item entry needs.
///
/// `dfc` is a DFC root; an unknown root is an error rather than a guessed
/// length, because a wrong length desyncs everything after it in the record.
pub fn item_manipulator_readstate(dfc: &str) -> Result<Vec<u8>, String> {
    let root = dfc.split('\u{0}').next().unwrap_or("");
    let Some((_, body)) = MANIPULATOR_READSTATE_EMPTY
        .iter()
        .find(|(name, _)| *name == root)
        .copied()
    else {
        let known = MANIPULATOR_READSTATE_EMPTY
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "no vtable+0x100 readState shape is known for DFC root {root:?} (known: {known})"
        ));
    };
    let want = MANIPULATOR_READSTATE_LEN
        .iter()
        .find(|(name, _)| *name == root)
        .map(|(_, len)| *len)
        .unwrap_or(0);
    if body.len() != want {
        return Err(format!(
            "the readState body for {root:?} is {} bytes, the routine reads {want}",
            body.len()
        ));
    }
    Ok(body.to_vec())
}

/// `Container::processAddCurrency`'s sub-opcode.
pub const CONTAINER_SUB_ADD_CURRENCY: u8 = 0x20;

/// The observed `Container` currency lookup key; not a caller-invented entity.
pub const CONTAINER_CURRENCY_KEY_UNDECODED: u32 = 0;

/// `CurrencySource` bucket 0, the one all ordinary gameplay captures carry.
pub const CONTAINER_CURRENCY_SOURCE_UNMEASURED_DEFAULT: u8 = 0;

/// `[i32 amount][u8 source][u32 key][u8 flag]`.
pub const CONTAINER_CURRENCY_BODY_LEN: usize = 10;

/// `Container::processAddItem`'s sub-opcode (`messages.CONTAINER_SUB_ITEM_ADD`).
pub const CONTAINER_SUB_ITEM_ADD: u8 = 0x1E;
/// The bag inventory child id the add names (`MERCHANT_BAG_CONTAINER_ID`).
pub const MERCHANT_BAG_CONTAINER_ID: u8 = 0x0B;
/// `Item+0x80`/`+0x81`'s unplaced sentinel; not a legal incremental-add cell.
pub const ITEM_GRID_UNPLACED: u8 = 0xFF;
/// `Item+0x80..0x83`'s width after the id word: five bytes (see `_eq_item`).
pub const EQUIPMENT_ITEM_FIXED_LEN: usize = 9;

/// A container-add shape violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemError {
    /// The bag cell is the unplaced sentinel or otherwise out of range.
    BadCell,
    /// The stack count does not fit `Item+0x82`.
    BadCount(u32),
    /// The record does not parse as an `Item::readData` head.
    BadRecord(String),
}

impl fmt::Display for ItemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemError::BadCell => write!(f, "bag cell is the unplaced sentinel"),
            ItemError::BadCount(count) => {
                write!(f, "stack count {count} does not fit the one-byte Item+0x82")
            }
            ItemError::BadRecord(why) => {
                write!(f, "item record is not an Item::readData body: {why}")
            }
        }
    }
}

impl std::error::Error for ItemError {}

/// `s2c [07 35][cid][0x20][i32 amount][u8 source][u32 key][u8 flag]`.
///
/// The flag is `source != 0`, as `Container::addCurrency` writes it. `handle`
/// is the caller's explicit lookup key; the buy leg passes
/// [`CONTAINER_CURRENCY_KEY_UNDECODED`].
pub fn build_container_add_currency(container_cid: u16, delta: i32, handle: u32) -> Vec<u8> {
    let source = CONTAINER_CURRENCY_SOURCE_UNMEASURED_DEFAULT;
    let mut out = Vec::with_capacity(2 + 2 + 1 + CONTAINER_CURRENCY_BODY_LEN);
    out.extend_from_slice(&[0x07, 0x35]);
    out.extend_from_slice(&container_cid.to_le_bytes());
    out.push(CONTAINER_SUB_ADD_CURRENCY);
    out.extend_from_slice(&delta.to_le_bytes());
    out.push(source);
    out.extend_from_slice(&handle.to_le_bytes());
    out.push(u8::from(source != 0));
    debug_assert_eq!(out.len(), 2 + 2 + 1 + CONTAINER_CURRENCY_BODY_LEN);
    out
}

/// `s2c [07 35][cid][0x20][i32 delta][u8 source=0][u32 unit][u8 1]` -- the
/// notify form that anchors the client's floating gold text on `unit`.
pub fn build_container_add_currency_notify(container_cid: u16, delta: i32, unit: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + 2 + 1 + CONTAINER_CURRENCY_BODY_LEN);
    out.extend_from_slice(&[0x07, 0x35]);
    out.extend_from_slice(&container_cid.to_le_bytes());
    out.push(CONTAINER_SUB_ADD_CURRENCY);
    out.extend_from_slice(&delta.to_le_bytes());
    out.push(CONTAINER_CURRENCY_SOURCE_UNMEASURED_DEFAULT);
    out.extend_from_slice(&u32::from(unit).to_le_bytes());
    out.push(1);
    out
}

/// `s2c [07 35][cid][0x1E][0x0B][record]` -- one UnitContainer item add.
///
/// `record` is an `Item::readData` body (`0xFF` + classname + NUL + the fixed
/// head). Two fields are re-addressed and only two, exactly as
/// `messages.build_container_item_add` @7380 does: the u32 id (`slot`) and the
/// `Item+0x80`/`+0x81` cell. `count` re-addresses `Item+0x82`. The result is
/// re-parsed so a splice that landed on the wrong byte fails here rather than
/// on a live client.
pub fn build_container_item_add(
    container_cid: u16,
    item_record: &[u8],
    slot: u32,
    cell: (u8, u8),
    count: u32,
) -> Result<Vec<u8>, ItemError> {
    let (x, y) = cell;
    if (x, y) == (ITEM_GRID_UNPLACED, ITEM_GRID_UNPLACED) {
        return Err(ItemError::BadCell);
    }
    if count == 0 || count > 0xFF {
        return Err(ItemError::BadCount(count));
    }
    let out = readdress_item_record(item_record, slot, cell, Some(count as u8))?;
    let mut add = Vec::with_capacity(6 + out.len());
    add.extend_from_slice(&[0x07, 0x35]);
    add.extend_from_slice(&container_cid.to_le_bytes());
    add.push(CONTAINER_SUB_ITEM_ADD);
    add.push(MERCHANT_BAG_CONTAINER_ID);
    add.extend_from_slice(&out);
    Ok(add)
}

/// Re-address the id word and grid bytes of an authored `Item::readData`
/// record, re-parsing the result so a splice that landed on the wrong byte
/// fails here rather than on a live client.
///
/// The class, level and flags bytes are copied through unchanged; only the u32
/// id, `Item+0x80`/`+0x81` cell and (when `count` is `Some`) `Item+0x82` are
/// written. The equipment (id = slot, cell `0xFF 0xFF`) and active-item forms
/// are the same record with different addresses.
pub fn readdress_item_record(
    item_record: &[u8],
    item_id: u32,
    cell: (u8, u8),
    count: Option<u8>,
) -> Result<Vec<u8>, ItemError> {
    let (x, y) = cell;
    if item_record.first() != Some(&TYPE_TAG_CLASSNAME) {
        return Err(ItemError::BadRecord(
            "does not open with the 0xFF type tag".to_string(),
        ));
    }
    let nul = item_record[1..]
        .iter()
        .position(|&byte| byte == 0)
        .map(|pos| pos + 1)
        .ok_or_else(|| ItemError::BadRecord("classname is not NUL terminated".to_string()))?;
    if nul == 1 {
        return Err(ItemError::BadRecord("classname is empty".to_string()));
    }
    if !item_record[1..nul]
        .iter()
        .all(|&byte| (0x20..0x7F).contains(&byte))
    {
        return Err(ItemError::BadRecord(
            "classname is not printable ASCII".to_string(),
        ));
    }
    let item_off = nul + 1;
    if item_off + EQUIPMENT_ITEM_FIXED_LEN > item_record.len() {
        return Err(ItemError::BadRecord(
            "head is shorter than Item::readData".to_string(),
        ));
    }
    let class = &item_record[1..nul];
    let level = item_record[item_off + 7];
    let flags = item_record[item_off + 8];
    let mut out = item_record.to_vec();
    out[item_off..item_off + 4].copy_from_slice(&item_id.to_le_bytes());
    out[item_off + 4] = x;
    out[item_off + 5] = y;
    if let Some(count) = count {
        out[item_off + 6] = count;
    }
    if &out[1..nul] != class || out[item_off + 7] != level || out[item_off + 8] != flags {
        return Err(ItemError::BadRecord(
            "re-addressing moved a byte outside the id and cell".to_string(),
        ));
    }
    if (out[item_off + 4], out[item_off + 5]) != (x, y) {
        return Err(ItemError::BadRecord(
            "re-addressed record lost the bag cell".to_string(),
        ));
    }
    if let Some(count) = count {
        if out[item_off + 6] != count {
            return Err(ItemError::BadRecord(
                "re-addressed record lost the stack count".to_string(),
            ));
        }
    }
    if u32::from_le_bytes(out[item_off..item_off + 4].try_into().unwrap()) != item_id {
        return Err(ItemError::BadRecord(
            "re-addressed record lost the id word".to_string(),
        ));
    }
    Ok(out)
}

/// `Container::processItemRemove`'s sub-opcode.
pub const CONTAINER_SUB_ITEM_REMOVE: u8 = 0x1F;
/// `Container::processUpdateQuantity`'s sub-opcode.
pub const CONTAINER_SUB_ITEM_QUANTITY: u8 = 0x22;
/// `UnitContainer::processSetActiveItem`'s sub-opcode.
pub const CONTAINER_SUB_SET_ACTIVE: u8 = 0x28;
/// `UnitContainer::processClearActiveItem`'s sub-opcode.
pub const CONTAINER_SUB_CLEAR_ACTIVE: u8 = 0x29;
/// `Equipment::processAddItem`'s sub-opcode.
pub const EQUIPMENT_SUB_ITEM_ADD: u8 = 0x28;
/// `Equipment::processRemoveItem`'s sub-opcode.
pub const EQUIPMENT_SUB_ITEM_REMOVE: u8 = 0x29;
/// `Manipulators::processAddManipulator`'s sub-opcode.
pub const MANIPULATORS_SUB_ADD: u8 = 0x00;
/// `Manipulators::processRemoveManipulator`'s sub-opcode.
pub const MANIPULATORS_SUB_REMOVE: u8 = 0x01;
/// `Unit::processUseItemUpdate`'s entity sub-opcode.
pub const ENTITY_SUB_USE_ITEM_UPDATE: u8 = 0x08;

fn component_update_head(cid: u16, sub: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.extend_from_slice(&[0x07, 0x35]);
    out.extend_from_slice(&cid.to_le_bytes());
    out.push(sub);
    out
}

/// `s2c [07 35][cid][0x1F][id u32]` -- one UnitContainer item removal.
pub fn build_container_item_remove(container_cid: u16, item_id: u32) -> Vec<u8> {
    let mut out = component_update_head(container_cid, CONTAINER_SUB_ITEM_REMOVE);
    out.extend_from_slice(&item_id.to_le_bytes());
    out
}

/// `s2c [07 35][cid][0x22][id u32][count u8]` -- a stack count update.
pub fn build_container_item_quantity(container_cid: u16, item_id: u32, count: u8) -> Vec<u8> {
    let mut out = component_update_head(container_cid, CONTAINER_SUB_ITEM_QUANTITY);
    out.extend_from_slice(&item_id.to_le_bytes());
    out.push(count);
    out
}

/// `s2c [07 35][cid][0x28][record]` -- the bag item is on the cursor.
///
/// No inventory child byte precedes the record; `processSetActiveItem` reads a
/// bare `Container::readItem`.
pub fn build_container_set_active(container_cid: u16, item_record: &[u8]) -> Vec<u8> {
    let mut out = component_update_head(container_cid, CONTAINER_SUB_SET_ACTIVE);
    out.extend_from_slice(item_record);
    out
}

/// `s2c [07 35][cid][0x29]` -- the cursor is empty.
pub fn build_container_clear_active(container_cid: u16) -> Vec<u8> {
    component_update_head(container_cid, CONTAINER_SUB_CLEAR_ACTIVE)
}

/// `s2c [07 35][cid][0x28][record]` -- fill an Equipment slot.
///
/// The record's own u32 id is the slot (`processAddItem` reads it from there),
/// so it is re-addressed to `slot` with the unplaced `0xFF 0xFF` cell.
pub fn build_equipment_item_add(
    equipment_cid: u16,
    item_record: &[u8],
    slot: u32,
) -> Result<Vec<u8>, ItemError> {
    let record = readdress_item_record(
        item_record,
        slot,
        (ITEM_GRID_UNPLACED, ITEM_GRID_UNPLACED),
        Some(1),
    )?;
    let mut out = component_update_head(equipment_cid, EQUIPMENT_SUB_ITEM_ADD);
    out.extend_from_slice(&record);
    Ok(out)
}

/// `s2c [07 35][cid][0x29][slot u32]` -- clear an Equipment slot.
pub fn build_equipment_item_remove(equipment_cid: u16, slot: u32) -> Vec<u8> {
    let mut out = component_update_head(equipment_cid, EQUIPMENT_SUB_ITEM_REMOVE);
    out.extend_from_slice(&slot.to_le_bytes());
    out
}

/// `s2c [07 35][cid][0x00][manipulator record]` -- add one worn-item visual.
pub fn build_manipulators_add(manipulators_cid: u16, manip_record: &[u8]) -> Vec<u8> {
    let mut out = component_update_head(manipulators_cid, MANIPULATORS_SUB_ADD);
    out.extend_from_slice(manip_record);
    out
}

/// `s2c [07 35][cid][0x01][slot u32]` -- remove one worn-item visual.
pub fn build_manipulators_remove(manipulators_cid: u16, slot: u32) -> Vec<u8> {
    let mut out = component_update_head(manipulators_cid, MANIPULATORS_SUB_REMOVE);
    out.extend_from_slice(&slot.to_le_bytes());
    out
}

/// `s2c [07 03][eid][0x08][record][synch][0x06]` -- the entity use update.
///
/// The `0x08` marker precedes the item record; `synch` is the avatar's
/// `[0x02][hp u32]` trailer and the frame ends on the CEM stream terminator.
pub fn build_unit_use_item_update(entity_id: u16, item_record: &[u8], synch: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + item_record.len() + synch.len());
    out.extend_from_slice(&[0x07, 0x03]);
    out.extend_from_slice(&entity_id.to_le_bytes());
    out.push(ENTITY_SUB_USE_ITEM_UPDATE);
    out.extend_from_slice(item_record);
    out.extend_from_slice(synch);
    out.push(END_MARKER);
    out
}

/// N synch-terminated ComponentUpdates and one EndStream, for one `07 35`.
///
/// Each update must open `[07 35]`; its `0x07` is dropped here and re-added by
/// the caller's `send`. `synch` is repeated after EVERY update, because the
/// client's `processComponentUpdate` reads a trailer after each one. The stream
/// opens with `0x35` (so the caller strips it) and ends on `0x06`.
pub fn build_component_update_stream(updates: &[Vec<u8>], synch: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for update in updates {
        assert!(
            update.len() >= 2 && update[0] == 0x07 && update[1] == 0x35,
            "component update head is not [07 35]: {:?}",
            &update[..update.len().min(2)]
        );
        out.extend_from_slice(&update[1..]);
        out.extend_from_slice(synch);
    }
    out.push(END_MARKER);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Vec<u8> {
        let mut out = encode_list_cstr("items.test.Sample");
        out.extend_from_slice(&7u32.to_le_bytes());
        out.extend_from_slice(&[1, 2, 3, 4, 0]);
        out.extend_from_slice(&[0, 0]);
        out
    }

    fn id_and_cell(record: &[u8]) -> (u32, u8, u8, u8) {
        let nul = record[1..].iter().position(|&b| b == 0).unwrap() + 1;
        let off = nul + 1;
        (
            u32::from_le_bytes(record[off..off + 4].try_into().unwrap()),
            record[off + 4],
            record[off + 5],
            record[off + 6],
        )
    }

    #[test]
    fn readdress_writes_only_the_id_cell_and_count() {
        let original = record();
        let changed = readdress_item_record(&original, 0x1122_3344, (5, 6), Some(9)).unwrap();
        assert_eq!(id_and_cell(&changed), (0x1122_3344, 5, 6, 9));
        assert_eq!(
            &changed[..1 + "items.test.Sample".len() + 1],
            &original[..1 + "items.test.Sample".len() + 1]
        );
        assert_eq!(
            &changed[changed.len() - 2..],
            &original[original.len() - 2..]
        );
    }

    #[test]
    fn container_updates_round_trip() {
        let rec = readdress_item_record(&record(), 42, (1, 2), Some(3)).unwrap();
        let add = build_container_item_add(0xE006, &rec, 42, (1, 2), 3).unwrap();
        assert_eq!(
            &add[..6],
            &[
                0x07,
                0x35,
                0x06,
                0xE0,
                CONTAINER_SUB_ITEM_ADD,
                MERCHANT_BAG_CONTAINER_ID
            ]
        );
        assert_eq!(&add[6..], rec.as_slice());

        assert_eq!(
            build_container_item_remove(0xE006, 42),
            vec![
                0x07,
                0x35,
                0x06,
                0xE0,
                CONTAINER_SUB_ITEM_REMOVE,
                42,
                0,
                0,
                0
            ]
        );
        assert_eq!(
            build_container_item_quantity(0xE006, 42, 2),
            vec![
                0x07,
                0x35,
                0x06,
                0xE0,
                CONTAINER_SUB_ITEM_QUANTITY,
                42,
                0,
                0,
                0,
                2
            ]
        );
        let active = build_container_set_active(0xE006, &rec);
        assert_eq!(
            &active[..5],
            &[0x07, 0x35, 0x06, 0xE0, CONTAINER_SUB_SET_ACTIVE]
        );
        assert_eq!(&active[5..], rec.as_slice());
        assert_eq!(
            build_container_clear_active(0xE006),
            vec![0x07, 0x35, 0x06, 0xE0, CONTAINER_SUB_CLEAR_ACTIVE]
        );
    }

    #[test]
    fn equipment_and_manipulator_updates_re_address_the_slot() {
        let rec = record();
        let add = build_equipment_item_add(0xE005, &rec, 8).unwrap();
        assert_eq!(&add[..5], &[0x07, 0x35, 0x05, 0xE0, EQUIPMENT_SUB_ITEM_ADD]);
        assert_eq!(id_and_cell(&add[5..]), (8, 0xFF, 0xFF, 1));
        assert_eq!(
            build_equipment_item_remove(0xE005, 8),
            vec![
                0x07,
                0x35,
                0x05,
                0xE0,
                EQUIPMENT_SUB_ITEM_REMOVE,
                8,
                0,
                0,
                0
            ]
        );
        assert_eq!(
            build_manipulators_remove(0xE007, 8),
            vec![0x07, 0x35, 0x07, 0xE0, MANIPULATORS_SUB_REMOVE, 8, 0, 0, 0]
        );
        let manip = build_manipulators_add(0xE007, &add[5..]);
        assert_eq!(&manip[..5], &[0x07, 0x35, 0x07, 0xE0, MANIPULATORS_SUB_ADD]);
        assert_eq!(&manip[5..], &add[5..]);
    }

    #[test]
    fn use_update_puts_the_marker_before_the_record() {
        let rec = record();
        let synch = [0x02, 0x34, 0x12, 0x00, 0x00];
        let use_frame = build_unit_use_item_update(0x1000, &rec, &synch);
        assert_eq!(&use_frame[..4], &[0x07, 0x03, 0x00, 0x10]);
        assert_eq!(use_frame[4], ENTITY_SUB_USE_ITEM_UPDATE);
        assert_eq!(&use_frame[5..5 + rec.len()], rec.as_slice());
        assert_eq!(
            &use_frame[5 + rec.len()..],
            &[0x02, 0x34, 0x12, 0x00, 0x00, 0x06]
        );
    }
}
