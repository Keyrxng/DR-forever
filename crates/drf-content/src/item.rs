// SPDX-License-Identifier: GPL-3.0-or-later

//! The authored `Item::readData` record and the bag occupancy it is placed in.
//!
//! This is the Rust half of the reference `item_corpus.author_item_record_by_class`
//! and `BagGrid`. Nothing here reads a capture: a class's footprint, level and
//! no-sell bit are derived from its `.gc` chain, and the record's field order is
//! `Item::readData` @0x581710's own. A value `game.pkg` does not settle is an
//! error, never a default.

use std::collections::HashSet;

use crate::corpus::{ContentError, Corpus};

/// `0xFF` + printable ASCIIZ, the `GCClassRegistry::readType` classname form.
pub const TYPE_TAG_CLASSNAME: u8 = 0xFF;

/// `Item::readData`'s fixed head after the classname/NUL: u32 + five u8.
pub const ITEM_WIRE_FIXED_LEN: usize = 9;

/// The per-instance soulbound bit on `Item+0x83` (`Item::isSoulBound`).
pub const ITEM_WIRE_FLAG_SOULBOUND: u8 = 0x01;
/// The per-instance no-sell bit on `Item+0x83` (`Item::isNoSell`).
pub const ITEM_WIRE_FLAG_NO_SELL: u8 = 0x02;
/// The wire bit gating the optional `Item+0x7C` u16; setting a flag under it
/// would move the record's length.
pub const ITEM_WIRE_FLAG_SOULBOUND_WORD: u8 = 0x04;

/// The `avatar.base.Inventory` archetype the bag geometry is read from.
pub const AVATAR_BAG_FQN: &str = "avatar.base.Inventory";

/// `ItemDesc.Quality`'s eight table slots, in the order `Item::getValue`
/// indexes them.
pub const QUALITY_NAMES: [&str; 8] = [
    "QUEST",
    "NORMAL",
    "SUPERIOR",
    "MAGICAL",
    "RARE",
    "UNIQUE",
    "COLLECTION",
    "MYTHIC",
];

/// `ItemDesc.Quality` when the item's own class chain states none.
pub const QUALITY_DEFAULT: usize = 1;

/// The index of a `Quality` spelling in [`QUALITY_NAMES`].
pub fn quality_index(name: &str) -> Option<usize> {
    let wanted = name.trim().to_uppercase();
    QUALITY_NAMES
        .iter()
        .position(|candidate| *candidate == wanted)
}

/// One authored `Item::readData` record, with the fields a caller re-addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRecord {
    /// The whole record: `0xFF` + classname + NUL + the `readData` body.
    pub record: Vec<u8>,
    /// The classname the record carries.
    pub class: String,
    /// `Item+0x68`, the server-minted per-instance id (`u32` on the wire).
    pub slot: u32,
    /// `Item+0x7f`, the level byte.
    pub level: u8,
    /// `Item+0x83`, the wire flags byte.
    pub flags: u8,
    /// The class's `InventoryWidth` (the bag footprint).
    pub width: u32,
    /// The class's `InventoryHeight` (the bag footprint).
    pub height: u32,
}

impl ItemRecord {
    /// OR the two per-instance flag bits into the record's flags byte.
    ///
    /// Mirrors the reference `sessions.quest._granted_item_entry`: `item.pkg`
    /// authors neither bit (they are per-instance), and setting them must not
    /// move the record's length, so the optional `Item+0x7C` u16 must stay
    /// absent.
    pub fn apply_instance_flags(
        &mut self,
        soulbound: bool,
        no_sell: bool,
    ) -> Result<(), ContentError> {
        let want = (u8::from(soulbound) * ITEM_WIRE_FLAG_SOULBOUND)
            | (u8::from(no_sell) * ITEM_WIRE_FLAG_NO_SELL);
        if want == 0 {
            return Ok(());
        }
        let at = self.flags_offset()?;
        if self.record[at] & ITEM_WIRE_FLAG_SOULBOUND_WORD != 0 {
            return Err(ContentError::Malformed(
                "granted item record already carries the optional Item+0x7C u16".to_string(),
            ));
        }
        self.record[at] |= want;
        self.flags = self.record[at];
        Ok(())
    }

    /// Overwrite the per-instance flags byte (level/flags survive a rejoin).
    pub fn set_flags(&mut self, flags: u8) -> Result<(), ContentError> {
        let at = self.flags_offset()?;
        self.record[at] = flags;
        self.flags = flags;
        Ok(())
    }

    /// The record offset of the `Item+0x83` flags byte.
    fn flags_offset(&self) -> Result<usize, ContentError> {
        let at = 1 + self.class.len() + 1 + ITEM_WIRE_FIXED_LEN - 1;
        if at >= self.record.len() {
            return Err(ContentError::Malformed(format!(
                "item record has no flags byte at {at}"
            )));
        }
        Ok(at)
    }
}

/// What `game.pkg` says about one item class, including the fields
/// `Item::getValue` prices from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemProfile {
    pub fqn: String,
    pub width: u32,
    pub height: u32,
    pub gold_value_q8: Option<i64>,
    pub level_override: i64,
    pub quality: Option<usize>,
    pub useless_to_merchant: bool,
    pub scale_to_observer_level: bool,
    pub no_sell: bool,
    /// `Description.MaxStackSize`, when the class states one.
    pub max_stack_size: Option<u32>,
    /// `Description.Consumable`, when the class states it.
    pub consumable: Option<bool>,
}

/// The chain-ordered properties of a DFC class, leaf first, first row winning.
///
/// A `.gc` block's own `Description` child is folded in beside its direct rows,
/// exactly as the reference `chain_properties` does: an archetype template carries
/// its footprint and level there.
pub(crate) fn chain_props(
    corpus: &mut Corpus,
    fqn: &str,
) -> Result<Vec<(String, String)>, ContentError> {
    let mut props: Vec<(String, String)> = Vec::new();
    let mut authored: HashSet<String> = HashSet::new();
    let mut current = fqn.to_string();
    for _ in 0..32 {
        let Ok(node) = corpus.block_chain(&current) else {
            break;
        };
        for (key, value) in &node.props {
            if authored.insert(key.clone()) {
                props.push((key.clone(), value.clone()));
            }
        }
        for child in &node.children {
            if child.name.eq_ignore_ascii_case("description") {
                for (key, value) in &child.props {
                    if authored.insert(key.clone()) {
                        props.push((key.clone(), value.clone()));
                    }
                }
            }
        }
        match node.extends.clone() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    Ok(props)
}

fn prop<'a>(props: &'a [(String, String)], name: &str) -> Option<&'a str> {
    props
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

pub(crate) fn gc_int(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok();
    }
    text.parse::<i64>()
        .ok()
        .or_else(|| text.parse::<f64>().ok().map(|value| value as i64))
}

/// A `.gc` `Fixed32` value in Q8, narrowed through `f32` like the client.
pub fn fixed32_q8(text: &str) -> Option<i64> {
    let value = text.trim().parse::<f32>().ok()?;
    Some((value * 256.0f32) as i64)
}

fn gc_bool(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("true")
}

fn needed(
    props: &[(String, String)],
    name: &str,
    fqn: &str,
    what: &str,
) -> Result<u32, ContentError> {
    let value = prop(props, name).ok_or_else(|| {
        ContentError::NotFound(format!(
            "game.pkg: {fqn} states no {name}, so the {what} is unknown"
        ))
    })?;
    let value = gc_int(value).ok_or_else(|| {
        ContentError::Malformed(format!(
            "game.pkg: {fqn} states {name} = {value:?}, not an integer"
        ))
    })?;
    if value < 0 {
        return Err(ContentError::Malformed(format!(
            "game.pkg: {fqn} states {name} = {value}"
        )));
    }
    Ok(value as u32)
}

/// The derived profile for `fqn`, or `None` when the corpus defines no block.
pub fn item_profile(corpus: &mut Corpus, fqn: &str) -> Result<Option<ItemProfile>, ContentError> {
    let name = fqn.split('\u{0}').next().unwrap_or("");
    let props = chain_props(corpus, name)?;
    if props.is_empty() {
        return Ok(None);
    }
    let gold_value_q8 = match prop(&props, "GoldValue") {
        Some(value) => Some(fixed32_q8(value).ok_or_else(|| {
            ContentError::Malformed(format!(
                "game.pkg: {name} states GoldValue = {value:?}, not a number"
            ))
        })?),
        None => None,
    };
    let quality = match prop(&props, "Quality") {
        Some(value) => Some(quality_index(value).ok_or_else(|| {
            ContentError::Blocked(format!(
                "{name}'s Quality = {value:?}, which is not one of {}",
                QUALITY_NAMES.join(", ")
            ))
        })?),
        None => None,
    };
    Ok(Some(ItemProfile {
        fqn: name.to_string(),
        width: needed(&props, "InventoryWidth", name, "width")?,
        height: needed(&props, "InventoryHeight", name, "height")?,
        gold_value_q8,
        level_override: prop(&props, "LevelOverride").and_then(gc_int).unwrap_or(0),
        quality,
        useless_to_merchant: prop(&props, "UselessToMerchant").is_some_and(gc_bool),
        scale_to_observer_level: prop(&props, "ScaleToObserverLevel").is_some_and(gc_bool),
        no_sell: prop(&props, "NoSell").is_some_and(gc_bool),
        max_stack_size: prop(&props, "MaxStackSize")
            .and_then(gc_int)
            .map(|value| value.max(0) as u32),
        consumable: prop(&props, "Consumable").map(gc_bool),
    }))
}

/// An `Item::readData` record for `class_name`, authored from `game.pkg`.
///
/// The layout is `[0xFF][classname][NUL]` + the server-minted u32 id + x, y,
/// quantity, level, flags, one `0x00` status byte per instantiated
/// `ItemModifier` child of the class, then the (empty) rolled-modifier count
/// (see `author_item_record` @14756). `level` defaults to the class's own
/// `LevelOverride`; the flags byte is 0 because the two per-instance bits are
/// not class properties and the optional soulbound u16 stays absent.
///
/// The status bytes are load-bearing: `Item::readData` @0x581710 hands the
/// record to `GCObject::ReadChildData<ItemModifier>` @0x583920, whose first loop
/// consumes one `ItemModifier::readData` body per archetype child before it
/// reads the rolled-modifier count. A consumable declares none; an armour class
/// such as `LeatherArmor3PAL.LeatherArmor3-2` declares one
/// (`BaseArmorClasses.Leather`'s `SpeedM`). Omitting them desyncs the CEM
/// stream.
///
/// `Ok(None)` when the corpus defines no block for the class; an error when the
/// block exists but does not state its footprint.
pub fn author_item_record_by_class(
    corpus: &mut Corpus,
    class_name: &str,
    item_id: u32,
    quantity: u32,
    level: Option<u8>,
    cell: (u8, u8),
) -> Result<Option<ItemRecord>, ContentError> {
    author_item_record_with_mods(corpus, class_name, item_id, quantity, level, cell, &[])
}

/// Like [`author_item_record_by_class`], but with rolled `ItemModifier`
/// children. Entries are `(classname, modifier_level)` in slot order. The
/// class's own instantiated modifiers remain the all-zero status run before
/// the count; these entries are the count-prefixed list.
#[allow(clippy::too_many_arguments)]
pub fn author_item_record_with_mods(
    corpus: &mut Corpus,
    class_name: &str,
    item_id: u32,
    quantity: u32,
    level: Option<u8>,
    cell: (u8, u8),
    modifiers: &[(String, u8)],
) -> Result<Option<ItemRecord>, ContentError> {
    let name = class_name.split('\u{0}').next().unwrap_or("");
    let name = corpus
        .canonical_class_name(name)
        .unwrap_or_else(|| name.to_string());
    let name = name.as_str();
    let Some(prof) = item_profile(corpus, name)? else {
        return Ok(None);
    };
    let level = level.unwrap_or(prof.level_override as u8);
    // `Item::readData`'s `GCObject::ReadChildData<ItemModifier>` loop reads one
    // status byte for every `ItemModifier` child the item's class instantiates
    // before it reads the rolled-modifier count. Omitting them makes the client
    // read the synch trailer's `0x02` as the count and then `0xC8` (the HP word's
    // second byte) as a type tag -- the `Invalid type tag 200` fatal. A
    // consumable declares none; an armour class like `LeatherArmor3-2` declares
    // one. The ground-drop path already carries these; the bag add must too.
    let authored_slots = corpus.item_readdata_slots(name)?;
    let mut record = Vec::with_capacity(
        1 + name.len() + 1 + ITEM_WIRE_FIXED_LEN + authored_slots + 1 + modifiers.len() * 32,
    );
    record.push(TYPE_TAG_CLASSNAME);
    record.extend_from_slice(name.as_bytes());
    record.push(0);
    record.extend_from_slice(&item_id.to_le_bytes());
    record.push(cell.0);
    record.push(cell.1);
    record.push(quantity as u8);
    record.push(level);
    record.push(0);
    record.extend(std::iter::repeat_n(0u8, authored_slots));
    push_item_modifier_count(&mut record, modifiers.len());
    for (modifier, modifier_level) in modifiers {
        record.push(TYPE_TAG_CLASSNAME);
        record.extend_from_slice(modifier.as_bytes());
        record.push(0);
        record.push(0x01);
        record.push((*modifier_level).max(1));
    }
    Ok(Some(ItemRecord {
        record,
        class: name.to_string(),
        slot: item_id,
        level,
        flags: 0,
        width: prof.width,
        height: prof.height,
    }))
}

fn push_item_modifier_count(out: &mut Vec<u8>, count: usize) {
    if count > 0xFE {
        out.push(0xFF);
        out.extend_from_slice(&(count as u16).to_le_bytes());
    } else {
        out.push(count as u8);
    }
}

/// The `(width, height)` of the avatar's backpack, from `game.pkg`.
///
/// `avatar/base/Inventory.gc` states `Width = 10; Height = 8;` in its
/// `Description` child. `None` when the block is absent; an error when it
/// states a dimension that is not a positive integer.
pub fn derive_bag_geometry(corpus: &mut Corpus) -> Result<Option<(u8, u8)>, ContentError> {
    let props = chain_props(corpus, AVATAR_BAG_FQN)?;
    if props.is_empty() {
        return Ok(None);
    }
    let width = needed(&props, "Width", AVATAR_BAG_FQN, "bag width")?;
    let height = needed(&props, "Height", AVATAR_BAG_FQN, "bag height")?;
    if width == 0 || height == 0 || width > 0xFF || height > 0xFF {
        return Err(ContentError::Malformed(format!(
            "game.pkg: {AVATAR_BAG_FQN} is {width}x{height}"
        )));
    }
    Ok(Some((width as u8, height as u8)))
}

/// Per-session bag occupancy, tracked server-side.
///
/// Seeded empty for a fresh character (the join ships no bag items), then
/// extended by every grant. `place` runs the client's own first-fit
/// (`Inventory::findSlot` 404727: x outer from 0, y inner from 0) so the server
/// and the client agree on the cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BagGrid {
    width: u8,
    height: u8,
    cells: HashSet<(u8, u8)>,
}

impl BagGrid {
    /// An empty grid of `width` x `height`.
    pub fn new(width: u8, height: u8) -> BagGrid {
        BagGrid {
            width,
            height,
            cells: HashSet::new(),
        }
    }

    /// The bag's `(width, height)`.
    pub fn dimensions(&self) -> (u8, u8) {
        (self.width, self.height)
    }

    /// Reserve the first-fit cell for a `width` x `height` item.
    ///
    /// `None` when the item does not fit. The reservation happens here, before
    /// any frame is built, so a failed grant cannot leave a hole.
    pub fn place(&mut self, width: u32, height: u32) -> Option<(u8, u8)> {
        if width == 0 || height == 0 {
            return None;
        }
        let spot = self.first_fit(width, height)?;
        for cell in rectangle(spot.0, spot.1, width, height) {
            self.cells.insert(cell);
        }
        Some(spot)
    }

    /// Undo a `place`, for a grant that then failed to build.
    pub fn rollback(&mut self, x: u8, y: u8, width: u32, height: u32) {
        for cell in rectangle(x, y, width, height) {
            self.cells.remove(&cell);
        }
    }

    /// Reserve a rectangle a restored bag row already occupies, so a later
    /// grant's first-fit does not land on it.
    pub fn occupy(&mut self, x: u8, y: u8, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        for cell in rectangle(x, y, width, height) {
            self.cells.insert(cell);
        }
    }

    fn first_fit(&self, width: u32, height: u32) -> Option<(u8, u8)> {
        let bag_width = i64::from(self.width);
        let bag_height = i64::from(self.height);
        let width = i64::from(width);
        let height = i64::from(height);
        for x in 0..bag_width {
            if x + width > bag_width {
                break;
            }
            for y in 0..bag_height {
                if y + height > bag_height {
                    break;
                }
                let free = rectangle(x as u8, y as u8, width as u32, height as u32)
                    .all(|cell| !self.cells.contains(&cell));
                if free {
                    return Some((x as u8, y as u8));
                }
            }
        }
        None
    }
}

fn rectangle(x: u8, y: u8, width: u32, height: u32) -> impl Iterator<Item = (u8, u8)> {
    let xs = x as u32;
    let ys = y as u32;
    (0..width)
        .flat_map(move |dx| (0..height).map(move |dy| (xs + dx, ys + dy)))
        .map(|(x, y)| (x as u8, y as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_fit_is_x_outer_then_y_inner() {
        let mut grid = BagGrid::new(10, 8);
        assert_eq!(grid.place(1, 1), Some((0, 0)));
        assert_eq!(grid.place(1, 1), Some((0, 1)));
        grid.rollback(0, 0, 1, 1);
        assert_eq!(grid.place(1, 1), Some((0, 0)));
        // A 10x8 block no longer fits: (0, 1) is still taken.
        assert_eq!(grid.place(10, 8), None);
    }

    #[test]
    fn dimensions_and_occupy_reserve_the_right_rectangle() {
        let mut grid = BagGrid::new(3, 2);
        assert_eq!(grid.dimensions(), (3, 2));
        grid.occupy(0, 0, 1, 2);
        assert_eq!(grid.place(2, 1), Some((1, 0)));
        grid.rollback(1, 0, 2, 1);
        grid.rollback(0, 0, 1, 2);
        assert_eq!(grid.place(2, 1), Some((0, 0)));
    }

    #[test]
    fn a_rectangle_reserves_every_cell() {
        let mut grid = BagGrid::new(4, 4);
        assert_eq!(grid.place(2, 2), Some((0, 0)));
        assert_eq!(grid.place(2, 2), Some((0, 2)));
        assert_eq!(grid.place(2, 2), Some((2, 0)));
        assert_eq!(grid.place(2, 2), Some((2, 2)));
        assert_eq!(grid.place(1, 1), None);
        grid.rollback(0, 0, 2, 2);
        assert_eq!(grid.place(1, 1), Some((0, 0)));
    }
}
