// SPDX-License-Identifier: GPL-3.0-or-later

//! The `Merchant` block a vendor's `.gc` authors, DERIVED from `game.pkg`.
//!
//! A vendor ships no `*.Merchant` component on a content-served join until its
//! own `Merchant extends Merchant { ... }` block is read: the tabs are the
//! block's `extends MerchantInventory` children in declaration order, and each
//! tab's stock is its anonymous `* extends <item class>` children in
//! declaration order. Nothing here reads a capture, and a tab whose `ID` (or a
//! static entry's `InventoryX`/`InventoryY`) is absent is a refusal, never a
//! guessed byte.

use crate::corpus::{ContentError, Corpus};
use crate::gc::Node;
use std::collections::HashSet;

/// One `MerchantInventory` tab the `.gc` authors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerchantTab {
    /// The tab's index among the block's tabs, in declaration order.
    pub index: usize,
    /// The tab's `ID` row (`Inventory+0x65` on the wire).
    pub id: u8,
    /// The tab block's own name (`Misc`, `QuestItems`, ...).
    pub name: String,
    /// Whether the tab's stock is authored (`StaticContents = true`).
    pub static_contents: bool,
    /// `AutoGenerateItems = true`: fill the shelf from `item_generator`.
    pub auto_generate: bool,
    /// The tab's `ItemGenerator` archetype, when it names one.
    pub item_generator: Option<String>,
    /// The tab's authored item-level window.
    pub min_item_level: Option<u8>,
    pub max_item_level: Option<u8>,
    /// The authored stock. Empty for a generated tab until
    /// [`generate_merchant_items`] draws it.
    pub items: Vec<MerchantItem>,
}

/// One `StaticContents` placement on a tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerchantItem {
    /// The entry's index within its tab, in declaration order.
    pub index: usize,
    /// The item class the entry's `extends` target names.
    pub class: String,
    /// The authored `InventoryX` cell.
    pub inventory_x: u8,
    /// The authored `InventoryY` cell.
    pub inventory_y: u8,
    /// The authored `Quantity` (defaults to 1).
    pub quantity: u8,
    /// The instance level the shelf entry states (the `ItemTimeLine` phase).
    pub level: u8,
}

/// Does `npc_class`'s `.gc` author a `Merchant` block?
pub fn authors_merchant(corpus: &mut Corpus, npc_class: &str) -> bool {
    merchant_block(corpus, npc_class)
        .map(|block| block.is_some())
        .unwrap_or(false)
}

/// The `Merchant` block's `MerchantInventory` children, in declaration order.
///
/// `Ok(None)` when the class authors no `Merchant` block (the pkg does not make
/// it a vendor); `Ok(Some(tabs))` otherwise, with an empty list when the block
/// authors no tab. A tab without a usable `ID`, or a static entry without an
/// `InventoryX`/`InventoryY`, is a named [`ContentError`] rather than a guess.
pub fn derive_merchant_tabs(
    corpus: &mut Corpus,
    npc_class: &str,
) -> Result<Option<Vec<MerchantTab>>, ContentError> {
    let Some(block) = merchant_block(corpus, npc_class)? else {
        return Ok(None);
    };
    let mut tabs = Vec::new();
    for child in &block.children {
        if child.extends.as_deref() != Some("MerchantInventory") {
            continue;
        }
        let index = tabs.len();
        let Some(id) = child
            .prop("ID")
            .and_then(gc_int)
            .filter(|value| (0..=0xFF).contains(value))
        else {
            return Err(ContentError::Malformed(format!(
                "MerchantInventory {:?} authors no usable ID",
                child.name
            )));
        };
        let static_contents = child.prop("StaticContents").map(gc_bool).unwrap_or(false);
        let auto_generate = child
            .prop("AutoGenerateItems")
            .map(gc_bool)
            .unwrap_or(false);
        let item_generator = child
            .prop("ItemGenerator")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let min_item_level = child
            .prop("MinItemLevel")
            .and_then(gc_int)
            .and_then(|value| u8::try_from(value).ok());
        let max_item_level = child
            .prop("MaxItemLevel")
            .and_then(gc_int)
            .and_then(|value| u8::try_from(value).ok());
        let items = if static_contents {
            tab_items(child, index)?
        } else {
            Vec::new()
        };
        tabs.push(MerchantTab {
            index,
            id: id as u8,
            name: child.name.clone(),
            static_contents,
            auto_generate,
            item_generator,
            min_item_level,
            max_item_level,
            items,
        });
    }
    Ok(Some(tabs))
}

/// Draw a generated tab's shelf from its `ItemGenerator` archetype.
///
/// `MerchantWeaponIG` (and every sibling) is an `ItemGeneratorTable`: its
/// children name linked generator tables, which in turn name concrete items
/// (`items.pal.1HAxePAL.Normal001`, ...) and an `ItemTimeLine.*Phase*` block
/// that states the item's `MinLevel`. The draw walks those links, keeps the
/// concrete classes whose phase level is inside the tab's
/// `MinItemLevel`/`MaxItemLevel`, and shuffles a shelf out of them. The
/// generator's modifier children (`ItemModGenerator*`) are not applied, so a
/// generated shelf entry is the base item class at its phase level.
///
/// `seed` makes the draw stable per vendor/tab: the same component re-opens
/// with the same shelf, while different vendors get different shelves.
pub fn generate_merchant_items(
    corpus: &mut Corpus,
    tab: &MerchantTab,
    seed: u64,
) -> Vec<MerchantItem> {
    if tab.static_contents || !tab.auto_generate {
        return Vec::new();
    }
    let Some(generator) = tab.item_generator.as_deref() else {
        return Vec::new();
    };
    let mut pool: Vec<(String, u8)> = Vec::new();
    let mut visited = HashSet::new();
    collect_generator(corpus, generator, 0, &mut visited, &mut pool);
    pool.sort();
    pool.dedup();
    if pool.is_empty() {
        return Vec::new();
    }

    let min = tab.min_item_level.unwrap_or(0);
    let max = tab.max_item_level.unwrap_or(u8::MAX);
    let mut chosen: Vec<(String, u8)> = pool
        .iter()
        .filter(|(_, level)| *level >= min && *level <= max)
        .cloned()
        .collect();
    if chosen.is_empty() {
        // The phase bands are wider than the tab window (e.g. 6..20 with
        // bands at 11 and 22): keep the nearest band the window can reach.
        let nearest = pool
            .iter()
            .filter(|(_, level)| *level <= max)
            .max_by_key(|(_, level)| *level)
            .or_else(|| pool.iter().min_by_key(|(_, level)| *level));
        if let Some(entry) = nearest {
            chosen.push(entry.clone());
        } else {
            return Vec::new();
        }
    }

    let mut state = seed | 1;
    let mut next = || {
        state = state
            .wrapping_mul(0x5851_F42D_4C95_7F2D)
            .wrapping_add(0x1405_7B7E_F767_814F);
        state
    };
    for i in (1..chosen.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        chosen.swap(i, j);
    }
    chosen.truncate(8.min(chosen.len()));
    chosen
        .into_iter()
        .enumerate()
        .map(|(index, (class, level))| MerchantItem {
            index,
            class,
            inventory_x: (index % 10) as u8,
            inventory_y: (index / 10) as u8,
            quantity: 1,
            level,
        })
        .collect()
}

/// Draw `count` concrete items from an arbitrary `ItemGenerator` archetype.
///
/// Quest rewards (the wishing well's `WishingWellIG`/`OneTimeUseOnlyWishingWellIG`)
/// use the same generator grammar as merchants but are not a tab, so this
/// shares [`collect_generator`] and picks the highest item phase at or below
/// `level` (the level band the player can use), then shuffles that band.
pub fn generate_generator_items(
    corpus: &mut Corpus,
    generator: &str,
    level: u16,
    count: usize,
    seed: u64,
) -> Vec<(String, u8)> {
    if count == 0 {
        return Vec::new();
    }
    let mut pool = Vec::new();
    let mut visited = HashSet::new();
    collect_generator(corpus, generator, 0, &mut visited, &mut pool);
    pool.sort();
    pool.dedup();
    if pool.is_empty() {
        return Vec::new();
    }
    let max_level = level.min(u16::from(u8::MAX)) as u8;
    let band = pool
        .iter()
        .map(|(_, level)| *level)
        .filter(|level| *level <= max_level)
        .max()
        .unwrap_or_else(|| pool.iter().map(|(_, level)| *level).min().unwrap_or(0));
    let mut chosen: Vec<(String, u8)> = pool
        .into_iter()
        .filter(|(_, item_level)| *item_level == band)
        .collect();
    if chosen.is_empty() {
        return Vec::new();
    }
    let mut state = seed | 1;
    let mut next = || {
        state = state
            .wrapping_mul(0x5851_F42D_4C95_7F2D)
            .wrapping_add(0x1405_7B7E_F767_814F);
        state
    };
    for i in (1..chosen.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        chosen.swap(i, j);
    }
    chosen.truncate(count.min(chosen.len()));
    chosen
}

/// One weighted draw from an authored `ItemGenerator` table.
///
/// Unlike [`generate_generator_items`], this walks the table the way the
/// content is written: each node's children are filtered by their own and
/// inherited `MinLevel`/`MaxLevel`, one child is chosen by `Chance`, and a
/// `LinkedGenerator`/`ItemGenerator` child is followed. That is the authored
/// rarity/weapon/armour distribution, instead of flattening every reachable
/// `Item` into one pool. Returns the item class and the caller's level.
pub fn generate_loot_items(
    corpus: &mut Corpus,
    generator: &str,
    level: u16,
    count: usize,
    seed: u64,
) -> Vec<(String, u8)> {
    if count == 0 {
        return Vec::new();
    }
    let mut state = seed | 1;
    let mut out = Vec::new();
    for _ in 0..count {
        let mut path = Vec::new();
        if let Some(class) = loot_draw(
            corpus,
            generator,
            i64::from(level),
            &mut state,
            0,
            &mut path,
        ) {
            out.push((class, level.min(u16::from(u8::MAX)) as u8));
        }
    }
    out
}

fn loot_draw(
    corpus: &mut Corpus,
    generator: &str,
    level: i64,
    state: &mut u64,
    depth: usize,
    path: &mut Vec<String>,
) -> Option<String> {
    if depth > 16 {
        return None;
    }
    let key = generator.trim().to_ascii_lowercase();
    if key.is_empty() || path.iter().any(|seen| seen == &key) {
        return None;
    }
    path.push(key);
    let result = (|| {
        let node = corpus.block_chain(generator).ok()?;
        let mut choices: Vec<(&Node, f64)> = Vec::new();
        for child in &node.children {
            let has_target = child.prop("Item").is_some()
                || child.prop("LinkedGenerator").is_some()
                || child.prop("ItemGenerator").is_some();
            if !has_target {
                continue;
            }
            let (min, max) = loot_level_range(corpus, child);
            if level < min || level > max {
                continue;
            }
            let chance = child.prop("Chance").and_then(gc_float).unwrap_or(1.0);
            if chance > 0.0 {
                choices.push((child, chance));
            }
        }
        if choices.is_empty() {
            return None;
        }
        let mut remaining = choices;
        while !remaining.is_empty() {
            let total: f64 = remaining.iter().map(|(_, chance)| *chance).sum();
            let mut pick = (next_loot_u64(state) as f64 / u64::MAX as f64) * total;
            let index = remaining
                .iter()
                .position(|(_, chance)| {
                    if pick < *chance {
                        true
                    } else {
                        pick -= *chance;
                        false
                    }
                })
                .unwrap_or(remaining.len() - 1);
            let (child, _) = remaining.remove(index);
            if let Some(item) = child.prop("Item") {
                let class = item.trim().trim_matches('"').to_string();
                if !class.is_empty() {
                    return Some(class);
                }
                continue;
            }
            for property in ["LinkedGenerator", "ItemGenerator"] {
                if let Some(next) = child.prop(property) {
                    let next = next.trim().trim_matches('"');
                    if let Some(item) = loot_draw(corpus, next, level, state, depth + 1, path) {
                        return Some(item);
                    }
                }
            }
        }
        None
    })();
    path.pop();
    result
}

fn next_loot_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(0x5851_F42D_4C95_7F2D)
        .wrapping_add(0x1405_7B7E_F767_814F);
    *state
}

fn loot_level_range(corpus: &mut Corpus, child: &Node) -> (i64, i64) {
    let mut min = child.prop("MinLevel").and_then(gc_int);
    let mut max = child.prop("MaxLevel").and_then(gc_int);
    let mut current = child.extends.clone();
    for _ in 0..16 {
        let Some(parent) = current else {
            break;
        };
        let Ok(node) = corpus.block_chain(&parent) else {
            break;
        };
        if min.is_none() {
            min = node.prop("MinLevel").and_then(gc_int);
        }
        if max.is_none() {
            max = node.prop("MaxLevel").and_then(gc_int);
        }
        current = node.extends.clone();
    }
    (min.unwrap_or(0), max.unwrap_or(i64::MAX))
}

fn gc_float(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok()
}

/// Fill every generated tab in `tabs` from `npc_class`'s content.
///
/// The seed is the class's FNV-1a hash, so one vendor's shelf is stable: the
/// same component re-opens with the same classes and levels.
pub fn fill_generated_tabs(corpus: &mut Corpus, npc_class: &str, tabs: &mut [MerchantTab]) {
    let mut seed = 0xcbf2_9ce4_8422_2325u64;
    for byte in npc_class.bytes() {
        seed ^= u64::from(byte);
        seed = seed.wrapping_mul(0x0000_0100_0000_01B3);
    }
    for tab in tabs {
        if tab.auto_generate && tab.items.is_empty() {
            tab.items = generate_merchant_items(corpus, tab, seed ^ tab.index as u64);
        }
    }
}

/// Walk one generator node and append every concrete `Item` it reaches.
fn collect_generator(
    corpus: &mut Corpus,
    name: &str,
    depth: usize,
    visited: &mut HashSet<String>,
    out: &mut Vec<(String, u8)>,
) {
    if depth > 16 || !visited.insert(name.to_ascii_lowercase()) {
        return;
    }
    let Ok(node) = corpus.block_chain(name) else {
        return;
    };
    // A merchant generator can override a base table (`RareMerchantIG
    // extends RareIG`). The override block usually carries only the rows it
    // changes, so walk the inherited table too instead of treating the
    // override as the whole generator.
    if let Some(parent) = node.extends.as_deref() {
        collect_generator(corpus, parent, depth + 1, visited, out);
    }
    for child in &node.children {
        if let Some(item) = child.prop("Item") {
            let class = item.trim().trim_matches('"').to_string();
            if !class.is_empty() {
                let level = child
                    .prop("MinLevel")
                    .and_then(gc_int)
                    .and_then(|value| u8::try_from(value).ok())
                    .or_else(|| {
                        child
                            .extends
                            .as_deref()
                            .and_then(|target| timeline_level(corpus, target))
                    })
                    .unwrap_or(0);
                out.push((class, level));
            }
        }
        if let Some(linked) = child.prop("LinkedGenerator") {
            collect_generator(corpus, linked.trim(), depth + 1, visited, out);
        }
        if let Some(inner) = child.prop("ItemGenerator") {
            collect_generator(corpus, inner.trim(), depth + 1, visited, out);
        }
    }
}

/// The `MinLevel` an `ItemTimeLine.*Phase*` block states.
fn timeline_level(corpus: &mut Corpus, target: &str) -> Option<u8> {
    let node = corpus.block_chain(target).ok()?;
    node.prop("MinLevel")
        .and_then(gc_int)
        .and_then(|value| u8::try_from(value).ok())
}

/// The `Merchant` block `npc_class`'s `.gc` authors, or `None`.
fn merchant_block(corpus: &mut Corpus, npc_class: &str) -> Result<Option<Node>, ContentError> {
    let class = npc_class.strip_suffix(".Merchant").unwrap_or(npc_class);
    let Ok(root) = corpus.block_chain(class) else {
        return Ok(None);
    };
    Ok(root
        .children
        .iter()
        .find(|child| child.name.eq_ignore_ascii_case("Merchant"))
        .cloned())
}

/// A static tab's anonymous `* extends <class>` entries, in declaration order.
fn tab_items(tab: &Node, tab_index: usize) -> Result<Vec<MerchantItem>, ContentError> {
    let mut items = Vec::new();
    for child in &tab.children {
        if !child.name.is_empty() && child.name != "<anonymous>" {
            continue;
        }
        let Some(class) = child.extends.clone() else {
            continue;
        };
        let index = items.len();
        let inventory_x = child.prop("InventoryX").and_then(gc_int);
        let inventory_y = child.prop("InventoryY").and_then(gc_int);
        let (Some(inventory_x), Some(inventory_y)) = (inventory_x, inventory_y) else {
            return Err(ContentError::Malformed(format!(
                "{} tab {tab_index} entry {index} ({class}) authors no \
                 InventoryX/InventoryY",
                tab.name
            )));
        };
        let quantity = child.prop("Quantity").and_then(gc_int).unwrap_or(1);
        items.push(MerchantItem {
            index,
            class,
            inventory_x: (inventory_x & 0xFF) as u8,
            inventory_y: (inventory_y & 0xFF) as u8,
            quantity: (quantity & 0xFF) as u8,
            level: 0,
        });
    }
    Ok(items)
}

fn gc_int(text: &str) -> Option<i64> {
    let text = text.trim();
    text.parse::<i64>()
        .ok()
        .or_else(|| text.parse::<f64>().ok().map(|value| value as i64))
}

fn gc_bool(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_uppercase().as_str(),
        "TRUE" | "1" | "YES"
    )
}
