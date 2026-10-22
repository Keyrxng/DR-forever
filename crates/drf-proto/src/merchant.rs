// SPDX-License-Identifier: GPL-3.0-or-later

//! The DERIVED `*.Merchant` component record, built from `game.pkg` tabs.
//!
//! The record is a `0x32` component-create whose class is `<npc class>.Merchant`
//! and whose body is the `Merchant::readInit` stock payload: the
//! `Container::readInit` prologue (`[post][u32 next_id][u32 currency]`), then
//! each tab's `[id][has-data]` and, for a `StaticContents` tab, each entry's
//! ten-byte `Item::readData`, then the `<Inventory>`-level new-tab count and
//! `Merchant::readInit`'s own tail. Every byte mirrors the reference's
//! `merchant_content.build_stock_payload`; nothing here reads a capture.

use std::collections::HashMap;

use drf_content::MerchantTab;

/// The component classname suffix a vendor's `Merchant` block resolves to.
pub const MERCHANT_COMPONENT_CLASS_SUFFIX: &str = ".Merchant";

/// The `Merchant` component-create prologue byte.
pub const MERCHANT_POST_BYTE: u8 = 0x01;

/// `Container::readInit`'s currency word (`Container+0x74`); zero.
pub const MERCHANT_CURRENCY: u32 = 0;

/// Where the tab loop starts (`1 + 4 + 4`).
pub const MERCHANT_TABS_OFF: usize = 9;

/// One tab's `(id, has-data)` header width.
pub const MERCHANT_TAB_HEADER_LEN: usize = 2;

/// The `<Inventory>`-level new-`MerchantInventory` var-len count, after the
/// existing-tabs loop: zero, because the client attaches every tab from the
/// `.gc` it already holds.
pub const MERCHANT_NEW_TAB_COUNT: u8 = 0x00;

/// `Merchant::readInit`'s own three tail bytes (`flags` + `+0x8A` u16).
pub const MERCHANT_TAIL_LEN: usize = 3;

/// `Item::readData`'s fixed head after the id: five `u8`.
pub const ITEM_READ_DATA_FIXED_LEN: usize = 9;

/// A `StaticContents` tab's has-data flag.
pub const MERCHANT_TAB_HAS_DATA: u8 = 0x01;

/// A new-`Item` var-len count of zero.
pub const MERCHANT_NEW_ITEM_COUNT: u8 = 0x00;

/// The reserved band merchant ITEM ids are minted from (a third id namespace,
/// clear of entity ids at `0x1000` and component ids at `0xE000`).
pub const MERCHANT_ITEM_ID_MIN: u32 = 0x0001;
/// The last id in the merchant ITEM id band.
pub const MERCHANT_ITEM_ID_MAX: u32 = 0x0FFF;

/// `<npc class>.Merchant`, from either the NPC's own spelling or the component's.
pub fn merchant_component_classname(npc_class: &str) -> String {
    if npc_class.ends_with(MERCHANT_COMPONENT_CLASS_SUFFIX) {
        npc_class.to_string()
    } else {
        format!("{npc_class}{MERCHANT_COMPONENT_CLASS_SUFFIX}")
    }
}

/// `first_item_id` upward, in `(tab index, entry index)` construction order.
///
/// Returns the id table plus the `Container+0x70` counter the client's own
/// minter would have left behind (`next_id`).
pub fn derived_item_ids(
    tabs: &[MerchantTab],
    first_item_id: u32,
) -> (HashMap<(usize, usize), u32>, u32) {
    let mut ids = HashMap::new();
    let mut next_id = first_item_id;
    for tab in tabs {
        for item in &tab.items {
            ids.insert((tab.index, item.index), next_id);
            next_id = next_id.wrapping_add(1);
        }
    }
    (ids, next_id)
}

/// Reserve `count` consecutive merchant item ids from `cursor`.
///
/// Returns the first id; the run is `first .. first + count - 1`. Zero when
/// `count` is zero (and the cursor is untouched). The band is
/// [`MERCHANT_ITEM_ID_MIN`]..=[`MERCHANT_ITEM_ID_MAX`]; a cursor of `0` is the
/// unset sentinel and a run that would leave the band restarts at the minimum,
/// exactly as the reference's per-session allocator does.
pub fn reserve_item_ids(cursor: &mut u32, count: u32) -> u32 {
    if count == 0 {
        return 0;
    }
    if *cursor < MERCHANT_ITEM_ID_MIN || (*cursor).saturating_add(count) > MERCHANT_ITEM_ID_MAX {
        *cursor = MERCHANT_ITEM_ID_MIN;
    }
    let first = *cursor;
    *cursor = cursor.saturating_add(count);
    first
}

/// The `Merchant` class payload: `[post][u32 next_id][u32 currency]`, the tab
/// loop, the new-tab count and the tail. Byte-for-byte per `build_stock_payload`.
pub fn build_stock_payload(tabs: &[MerchantTab], first_item_id: u32) -> Vec<u8> {
    let (ids, next_id) = derived_item_ids(tabs, first_item_id);
    let mut out = Vec::new();
    out.push(MERCHANT_POST_BYTE);
    out.extend_from_slice(&next_id.to_le_bytes());
    out.extend_from_slice(&MERCHANT_CURRENCY.to_le_bytes());
    for tab in tabs {
        out.push(tab.id);
        out.push(MERCHANT_TAB_HAS_DATA);
        if !tab.static_contents {
            out.push(MERCHANT_NEW_ITEM_COUNT);
            continue;
        }
        for item in &tab.items {
            let id = ids.get(&(tab.index, item.index)).copied().unwrap_or(0);
            out.extend_from_slice(&item_read_data(
                id,
                item.inventory_x,
                item.inventory_y,
                item.quantity,
                item.level,
            ));
        }
        out.push(MERCHANT_NEW_ITEM_COUNT);
    }
    out.push(MERCHANT_NEW_TAB_COUNT);
    out.push(0x00);
    out.extend_from_slice(&0u16.to_le_bytes());
    debug_assert!(
        out.len()
            >= MERCHANT_TABS_OFF + tabs.len() * MERCHANT_TAB_HEADER_LEN + 1 + MERCHANT_TAIL_LEN
    );
    out
}

/// The full `0x32` component-create record for `npc_class`'s derived stock.
pub fn build_merchant_component_record(
    owner: u16,
    component_id: u16,
    npc_class: &str,
    tabs: &[MerchantTab],
    first_item_id: u32,
) -> Vec<u8> {
    let payload = build_stock_payload(tabs, first_item_id);
    let classname = merchant_component_classname(npc_class);
    let (post, body) = payload
        .split_first()
        .map(|(post, body)| (*post, body))
        .unwrap_or((MERCHANT_POST_BYTE, &[]));
    crate::player::component_create_inner(owner, component_id, classname.as_bytes(), post, body)
}

/// One `Item::readData` body: `u32 id`, x, y, quantity, level, flags, then the
/// var-len new-`ItemModifier` count. All fields but the id are zero/`Quantity`
/// for a `.gc`-authored static placement, which is the ten-byte width
/// `Item::readData` @0x581710 reads with `flags = 0`.
fn item_read_data(
    item_id: u32,
    inventory_x: u8,
    inventory_y: u8,
    quantity: u8,
    level: u8,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(ITEM_READ_DATA_FIXED_LEN + 1);
    out.extend_from_slice(&item_id.to_le_bytes());
    out.push(inventory_x);
    out.push(inventory_y);
    out.push(quantity);
    out.push(level);
    out.push(0); // flags (the optional Item+0x7C u16 stays absent)
    out.push(0); // new ItemModifier count
    out
}
