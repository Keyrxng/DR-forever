// SPDX-License-Identifier: GPL-3.0-or-later

//! Server-side loot generation, following the client's generator grammar.
//!
//! A `GCObjectGenerator<Item>` table is not a weighted list. Its children are
//! grouped by their `Chance` value (descending), each group gets one
//! `Random::generate() % Chance == 0` acceptance draw, and one survivor of a
//! passing group is picked uniformly. A row that produces nothing falls
//! through to the next group. `SingleItemGenerator` applies the item's level
//! override or a 0..=7 jitter, and `RandomItemGenerator` then draws each
//! authored `ItemModGenerator1..9` slot in order. `CurrencyGenerator` mints a
//! native `Currency` item.
//!
//! The numbers and field order are the client's own; the draw source is the
//! server's MT19937 stream. Nothing here reads a capture.

use crate::corpus::{ContentError, Corpus};
use crate::economy::{knobs as load_knobs, Knobs, Q8_ONE};
use crate::gc::{self, Node};
use crate::item::item_profile;
use crate::item::{fixed32_q8, gc_int};
use crate::maze::MersenneTwister;
use std::collections::HashSet;

/// One authored loot slot: a generator archetype and its roll/pass count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LootSlot {
    pub generator: String,
    pub count: u32,
}

/// One server-assigned `ItemModifier` child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifierRoll {
    pub class: String,
    pub level: u8,
}

/// One generated loot object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LootItem {
    /// The concrete item GC class, or `Currency` for a currency object.
    pub class: String,
    /// `Item+0x7f` (zero for `Currency`).
    pub level: u8,
    /// Rolled `ItemModifier` children in slot order.
    pub modifiers: Vec<ModifierRoll>,
    /// `Currency+0x88` for a native `Currency` object.
    pub amount: Option<u32>,
}

impl LootItem {
    pub fn is_currency(&self) -> bool {
        self.amount.is_some()
    }
}

/// The `StockUnitDesc` treasure slots a unit class states.
///
/// `TreasureGenerator`/`TreasureCount` (slot 1), then `TreasureGenerator2`..
/// `TreasureGenerator10` and their count properties. Counts default to 1 and
/// a zero count disables the slot. Properties live on the class's
/// `Description`/`StockUnitDesc` child, not necessarily the root block.
pub fn treasure_slots(
    corpus: &mut Corpus,
    unit_class: &str,
) -> Result<Vec<LootSlot>, ContentError> {
    let props = chain_props(corpus, unit_class);
    let mut out = Vec::new();
    for slot in 1..=10usize {
        let suffix = if slot == 1 {
            String::new()
        } else {
            slot.to_string()
        };
        let Some(generator) = prop(&props, &format!("TreasureGenerator{suffix}")) else {
            continue;
        };
        let generator = unquote(generator);
        if generator.is_empty() {
            continue;
        }
        let count = prop(&props, &format!("TreasureCount{suffix}"))
            .and_then(gc_int)
            .unwrap_or(1)
            .clamp(0, 255) as u32;
        if count > 0 {
            out.push(LootSlot { generator, count });
        }
    }
    Ok(out)
}

/// Draw every roll for a set of authored slots, mutating one RNG stream.
pub fn generate_slots(
    corpus: &mut Corpus,
    slots: &[LootSlot],
    level: u8,
    rng: &mut MersenneTwister,
    is_member: bool,
) -> Vec<LootItem> {
    generate_slots_scaled(corpus, slots, level, rng, is_member, 0x100)
}

/// Draw every roll with a Q8 loot multiplier (`0x100` = 1.0).
///
/// The authored `TreasureCount` is treated as an expected count: the integer
/// part always rolls, and the fractional part rolls one extra with probability
/// `fraction / 256`. This makes 1.5x/2.0x/2.5x apply to the same generator
/// without an always-round-up bias. Currency amounts stay authored; extra
/// count rolls extra currency objects, matching the client's table wording
/// ("+N% loot drops").
pub fn generate_slots_scaled(
    corpus: &mut Corpus,
    slots: &[LootSlot],
    level: u8,
    rng: &mut MersenneTwister,
    is_member: bool,
    loot_factor: u16,
) -> Vec<LootItem> {
    let mut out = Vec::new();
    let knob_values = load_knobs(corpus).ok();
    for slot in slots {
        let rolls = scaled_slot_count(slot.count, loot_factor, || rng.generate());
        for _ in 0..rolls {
            let mut path = HashSet::new();
            if let Some(item) = draw_generator(
                corpus,
                &slot.generator,
                level,
                rng,
                &mut path,
                0,
                is_member,
                knob_values.as_ref(),
            ) {
                out.push(item);
            }
        }
    }
    out
}

/// The expected-count expansion for one authored `TreasureCount`.
fn scaled_slot_count(count: u32, loot_factor: u16, roll: impl FnOnce() -> u32) -> u32 {
    let scaled = u64::from(count) * u64::from(loot_factor);
    let whole = (scaled >> 8) as u32;
    let fraction = (scaled & 0xFF) as u32;
    let extra = fraction > 0 && roll() % 256 < fraction;
    whole + u32::from(extra)
}

/// Draw `count` loot objects from one generator.
pub fn generate_loot(
    corpus: &mut Corpus,
    generator: &str,
    level: u8,
    count: usize,
    rng: &mut MersenneTwister,
    is_member: bool,
) -> Vec<LootItem> {
    let slots = vec![LootSlot {
        generator: generator.to_string(),
        count: count.min(u32::MAX as usize) as u32,
    }];
    generate_slots(corpus, &slots, level, rng, is_member)
}

#[allow(clippy::too_many_arguments)]
fn draw_generator(
    corpus: &mut Corpus,
    generator: &str,
    level: u8,
    rng: &mut MersenneTwister,
    path: &mut HashSet<String>,
    depth: usize,
    is_member: bool,
    knobs: Option<&Knobs>,
) -> Option<LootItem> {
    if depth > 24 {
        return None;
    }
    let key = generator.trim().to_ascii_lowercase();
    if key.is_empty() || !path.insert(key.clone()) {
        return None;
    }
    let result = (|| {
        let node = corpus.block_chain(generator.trim()).ok()?;
        draw_node(corpus, &node, level, rng, path, depth, is_member, knobs)
    })();
    path.remove(&key);
    result
}

/// Draw from an already-resolved generator node.
///
/// Most content rows are children of the named generator. Two authored forms
/// are not: a node can be a bare alias with `LinkedGenerator`/`ItemGenerator`
/// on the node itself (`TokenRewardRareMageIG.Amulet`), and a table can embed
/// named generator tables as children (`WishingWellIG` -> `SuperiorIG`). Both
/// must be walked as rows; ignoring them made those rewards return nothing.
#[allow(clippy::too_many_arguments)]
fn draw_node(
    corpus: &mut Corpus,
    node: &Node,
    level: u8,
    rng: &mut MersenneTwister,
    path: &mut HashSet<String>,
    depth: usize,
    is_member: bool,
    knobs: Option<&Knobs>,
) -> Option<LootItem> {
    if depth > 24 {
        return None;
    }
    if has_target(corpus, node) {
        return draw_row(corpus, node, level, rng, path, depth + 1, is_member, knobs);
    }
    let rows = loot_rows(corpus, node);
    for (chance, group) in rows {
        if chance == 0 {
            continue;
        }
        if !rng.generate().is_multiple_of(chance) {
            continue;
        }
        let mut survivors: Vec<&Node> = group
            .iter()
            .copied()
            .filter(|row| {
                let (min, max) = row_level_range(corpus, row);
                i64::from(level) >= min && i64::from(level) <= max
            })
            .collect();
        if survivors.is_empty() {
            continue;
        }
        // A row can resolve to a sub-generator with no level-eligible item
        // (the Sword-in-the-Stone's Unique row below its timeline), or to a
        // nested table whose own chance group misses. Try every survivor of
        // this chance group before moving on, rather than letting one dead row
        // swallow the draw.
        while !survivors.is_empty() {
            let index = (rng.generate() as usize) % survivors.len();
            let row = survivors.remove(index);
            if let Some(item) = draw_row(corpus, row, level, rng, path, depth + 1, is_member, knobs)
            {
                return Some(item);
            }
        }
    }
    // A generator class inherits its base table's rows. `OneTimeUseOnlyWishingWellIG`
    // extends `LegendIG` and only authors gold rows itself; without this fallback
    // its authored gold chance groups can all miss and the free well use returns
    // nothing even though the client still has the Legend quality ladder.
    if let Some(parent) = node.extends.clone() {
        if let Ok(base) = corpus.block_chain(&parent) {
            return draw_node(corpus, &base, level, rng, path, depth + 1, is_member, knobs);
        }
    }
    None
}

/// All rows grouped by equal `Chance`, descending. The level band is applied
/// after a group's acceptance draw, exactly as the client does.
fn loot_rows<'a>(corpus: &mut Corpus, node: &'a Node) -> Vec<(u32, Vec<&'a Node>)> {
    let mut rows: Vec<(u32, &Node)> = Vec::new();
    for child in &node.children {
        if !has_target(corpus, child) && !is_embedded_generator(child) {
            continue;
        }
        let chance = row_prop(corpus, child, "Chance")
            .and_then(|value| gc_int(&value))
            .unwrap_or(1)
            .clamp(0, u32::MAX as i64) as u32;
        if chance == 0 {
            continue;
        }
        rows.push((chance, child));
    }
    rows.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut out: Vec<(u32, Vec<&Node>)> = Vec::new();
    for (chance, row) in rows {
        match out.last_mut() {
            Some((value, group)) if *value == chance => group.push(row),
            _ => out.push((chance, vec![row])),
        }
    }
    out
}

fn has_target(corpus: &mut Corpus, node: &Node) -> bool {
    [
        "Item",
        "LinkedGenerator",
        "ItemGenerator",
        "ItemModifier",
        "GoldValue",
    ]
    .iter()
    .any(|name| row_prop(corpus, node, name).is_some())
}

/// A named child table authored inline, not reached through a link property.
/// Its own children are the rows, so the walker must descend into the node
/// itself rather than trying to resolve a class name for it.
fn is_embedded_generator(node: &Node) -> bool {
    !node.name.is_empty() && node.name != "<anonymous>" && !node.children.is_empty()
}

/// A row's property, following its own `extends` chain. Chest rows such as
/// `HeroGG.Average` inherit `GoldValue` from `DefaultGG.Average`; reading only
/// the direct block made the whole gold branch produce nothing.
fn row_prop(corpus: &mut Corpus, row: &Node, name: &str) -> Option<String> {
    let mut node = row.clone();
    for _ in 0..32 {
        if let Some(value) = node.prop(name) {
            return Some(value.to_string());
        }
        let Some(extends) = node.extends.clone() else {
            break;
        };
        let Ok(parent) = corpus.block_chain(&extends) else {
            break;
        };
        node = parent.as_ref().clone();
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    corpus: &mut Corpus,
    row: &Node,
    level: u8,
    rng: &mut MersenneTwister,
    path: &mut HashSet<String>,
    depth: usize,
    is_member: bool,
    knobs: Option<&Knobs>,
) -> Option<LootItem> {
    if row_prop(corpus, row, "GoldValue").is_some() {
        return draw_currency(corpus, row, level, rng, is_member, knobs);
    }
    if let Some(item_class) = row_prop(corpus, row, "Item") {
        let class = unquote(item_class);
        if class.is_empty() {
            return None;
        }
        let item_level = item_level_for(corpus, &class, level, rng);
        let modifiers = draw_modifiers(corpus, row, level, rng, path, depth);
        return Some(LootItem {
            class,
            level: item_level,
            modifiers,
            amount: None,
        });
    }
    if let Some(next) = row_prop(corpus, row, "LinkedGenerator") {
        return draw_generator(
            corpus,
            &unquote(next),
            level,
            rng,
            path,
            depth,
            is_member,
            knobs,
        );
    }
    if let Some(next) = row_prop(corpus, row, "ItemGenerator") {
        let mut item = draw_generator(
            corpus,
            &unquote(next),
            level,
            rng,
            path,
            depth,
            is_member,
            knobs,
        )?;
        if item.amount.is_none() {
            item.modifiers
                .extend(draw_modifiers(corpus, row, level, rng, path, depth));
        }
        return Some(item);
    }
    if is_embedded_generator(row) {
        return draw_node(corpus, row, level, rng, path, depth, is_member, knobs);
    }
    None
}

/// `SingleItemGenerator`: `LevelOverride`, else requested level + `rng & 7`.
fn item_level_for(corpus: &mut Corpus, class: &str, level: u8, rng: &mut MersenneTwister) -> u8 {
    let override_level = item_profile(corpus, class)
        .ok()
        .flatten()
        .map(|profile| profile.level_override)
        .unwrap_or(0);
    if override_level != 0 {
        return (override_level & 0xFF) as u8;
    }
    level.wrapping_add((rng.generate() & 7) as u8)
}

/// Draw the `ItemModGenerator1..9` slots of a `RandomItemGenerator` row.
fn draw_modifiers(
    corpus: &mut Corpus,
    row: &Node,
    level: u8,
    rng: &mut MersenneTwister,
    path: &mut HashSet<String>,
    depth: usize,
) -> Vec<ModifierRoll> {
    let mut out = Vec::new();
    for slot in 1..=9 {
        let Some(generator) = row_prop(corpus, row, &format!("ItemModGenerator{slot}")) else {
            continue;
        };
        let generator = unquote(generator);
        if generator.is_empty() {
            continue;
        }
        if let Some(class) = draw_modifier(corpus, &generator, level, rng, path, depth + 1) {
            out.push(ModifierRoll { class, level });
        }
    }
    out
}

fn draw_modifier(
    corpus: &mut Corpus,
    generator: &str,
    level: u8,
    rng: &mut MersenneTwister,
    path: &mut HashSet<String>,
    depth: usize,
) -> Option<String> {
    if depth > 24 {
        return None;
    }
    let key = format!("mod:{}", generator.trim().to_ascii_lowercase());
    if !path.insert(key.clone()) {
        return None;
    }
    let result = (|| {
        let node = corpus.block_chain(generator.trim()).ok()?;
        let rows = loot_rows(corpus, &node);
        for (chance, group) in rows {
            if chance == 0 || !rng.generate().is_multiple_of(chance) {
                continue;
            }
            let mut survivors: Vec<&Node> = group
                .into_iter()
                .filter(|row| {
                    let (min, max) = row_level_range(corpus, row);
                    i64::from(level) >= min && i64::from(level) <= max
                })
                .collect();
            if survivors.is_empty() {
                continue;
            }
            while !survivors.is_empty() {
                let index = (rng.generate() as usize) % survivors.len();
                let row = survivors.remove(index);
                if let Some(class) = row_prop(corpus, row, "ItemModifier") {
                    let class = unquote(class);
                    if !class.is_empty() {
                        return Some(class);
                    }
                }
                if let Some(next) = row_prop(corpus, row, "LinkedGenerator") {
                    if let Some(found) =
                        draw_modifier(corpus, &unquote(next), level, rng, path, depth + 1)
                    {
                        return Some(found);
                    }
                }
                if let Some(next) = row_prop(corpus, row, "ItemGenerator") {
                    if let Some(found) =
                        draw_modifier(corpus, &unquote(next), level, rng, path, depth + 1)
                    {
                        return Some(found);
                    }
                }
            }
        }
        None
    })();
    path.remove(&key);
    result
}

fn draw_currency(
    corpus: &mut Corpus,
    row: &Node,
    level: u8,
    rng: &mut MersenneTwister,
    is_member: bool,
    knobs: Option<&Knobs>,
) -> Option<LootItem> {
    let knobs = knobs.cloned().or_else(|| load_knobs(corpus).ok())?;
    let gold = row_prop(corpus, row, "GoldValue")
        .and_then(|value| fixed32_q8(&value))
        .unwrap_or(Q8_ONE);
    let volatility = row_prop(corpus, row, "Volatility")
        .and_then(|value| fixed32_q8(&value))
        .unwrap_or(0x80);
    let forced = row_prop(corpus, row, "ForcedLevel")
        .and_then(|value| gc_int(&value))
        .unwrap_or(0);
    let requested = if forced != 0 {
        forced
    } else {
        i64::from(level)
    };
    let level_fixed = (requested.clamp(1, 255) << 8).max(Q8_ONE);
    let member_base = if is_member {
        knobs.member_gold_mod.max(1)
    } else {
        Q8_ONE
    };
    let base = fixed_mul(
        fixed_mul(
            fixed_mul(knobs.item_gold_value_per_level.max(0), member_base),
            level_fixed,
        ),
        gold.max(0),
    );
    let spread = fixed_mul(base, volatility.max(0));
    let lo = ((base - spread).max(Q8_ONE)) >> 8;
    let hi = (base + spread) >> 8;
    let amount = if hi <= lo {
        lo.max(1) as u32
    } else {
        (lo + (rng.generate() as i64 % (hi - lo))).max(1) as u32
    };
    Some(LootItem {
        class: "Currency".to_string(),
        level: 0,
        modifiers: Vec::new(),
        amount: Some(amount),
    })
}

fn fixed_mul(a: i64, b: i64) -> i64 {
    ((i128::from(a) * i128::from(b)) >> 8) as i64
}

fn row_level_range(corpus: &mut Corpus, node: &Node) -> (i64, i64) {
    let mut min = node.prop("MinLevel").and_then(gc_int);
    let mut max = node.prop("MaxLevel").and_then(gc_int);
    let mut current = node.extends.clone();
    for _ in 0..32 {
        let Some(parent) = current else {
            break;
        };
        let Ok(parent_node) = corpus.block_chain(&parent) else {
            break;
        };
        if min.is_none() {
            min = parent_node.prop("MinLevel").and_then(gc_int);
        }
        if max.is_none() {
            max = parent_node.prop("MaxLevel").and_then(gc_int);
        }
        current = parent_node.extends.clone();
    }
    (min.unwrap_or(1), max.unwrap_or(100))
}

/// Properties from a class chain, leaf first. Nested `Description` /
/// `StockUnitDesc` blocks are folded in because unit stats live there.
fn chain_props(corpus: &mut Corpus, fqn: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = fqn.to_string();
    for _ in 0..32 {
        let Ok(node) = corpus.block_chain(&current) else {
            break;
        };
        collect_props(&node, &mut out, &mut seen);
        match node.extends.clone() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    out
}

fn collect_props(node: &Node, out: &mut Vec<(String, String)>, seen: &mut HashSet<String>) {
    for (key, value) in &node.props {
        if seen.insert(key.clone()) {
            out.push((key.clone(), value.clone()));
        }
    }
    for child in &node.children {
        if child.name.eq_ignore_ascii_case("Description")
            || child.name.eq_ignore_ascii_case("StockUnitDesc")
            || child.name.eq_ignore_ascii_case("ItemDesc")
        {
            collect_props(child, out, seen);
        } else {
            // `static Description` parses as a wrapper block; descend one
            // level so its inner Description is still found.
            for inner in &child.children {
                if inner.name.eq_ignore_ascii_case("Description")
                    || inner.name.eq_ignore_ascii_case("StockUnitDesc")
                    || inner.name.eq_ignore_ascii_case("ItemDesc")
                {
                    collect_props(inner, out, seen);
                }
            }
        }
    }
}

fn prop<'a>(props: &'a [(String, String)], name: &str) -> Option<&'a str> {
    props
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn unquote(value: impl AsRef<str>) -> String {
    let text = value.as_ref().trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text[1..text.len() - 1].to_string()
    } else {
        text.to_string()
    }
}

// Keep the `gc` import meaningful for future row predicates while making the
// module's intent explicit.
#[allow(dead_code)]
fn is_anonymous(node: &gc::Node) -> bool {
    node.name == "<anonymous>"
}

#[cfg(test)]
mod tests {
    use super::scaled_slot_count;

    #[test]
    fn scaled_slot_count_uses_expected_value_rounding() {
        assert_eq!(scaled_slot_count(5, 0x100, || 0), 5);
        assert_eq!(scaled_slot_count(2, 0x180, || 0), 3);
        assert_eq!(scaled_slot_count(1, 0x180, || 0), 2);
        assert_eq!(scaled_slot_count(1, 0x180, || 127), 2);
        assert_eq!(scaled_slot_count(1, 0x180, || 128), 1);
        assert_eq!(scaled_slot_count(1, 0x280, || 0), 3);
        assert_eq!(scaled_slot_count(0, 0x280, || 0), 0);
    }
}
