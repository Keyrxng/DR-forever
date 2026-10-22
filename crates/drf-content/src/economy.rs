// SPDX-License-Identifier: GPL-3.0-or-later

//! `Merchant::getBuyValue` / `Item::getValue`, derived from `game.pkg`.
//!
//! The arithmetic and the field sources are the reference `economy_corpus`: the
//! `GlobalKnobs.gc` singleton, the vendor's `.gc` `BuyValueMod` and `Level`,
//! and the item class's `extends` chain (`GoldValue`, `LevelOverride`,
//! `Quality`, `ScaleToObserverLevel`, `UselessToMerchant`). A value the package
//! does not settle is a named [`ContentError`], never a fallback number.

use std::collections::HashSet;

use crate::corpus::{ContentError, Corpus};
use crate::item::{
    chain_props, fixed32_q8, gc_int, item_profile, quality_index, ItemProfile, QUALITY_DEFAULT,
    QUALITY_NAMES,
};

/// `1.0 == 0x100`; every multiplier in the price paths is a Q8 `Fixed32`.
pub const Q8_ONE: i64 = 0x100;

/// `Merchant::getBuyValue`'s `UselessToMerchant` floor, 25.00.
pub const BUY_NOSELL_FLOOR: i64 = 0x1900;

/// The absolute floor both price paths share, 1.00.
pub const PRICE_FLOOR: i64 = 0x100;

/// The eight `Quality` table slots (`item::QUALITY_NAMES`).
pub const QUALITY_COUNT: usize = QUALITY_NAMES.len();

/// `VOUCHER`, one past both eight-entry knob tables.
pub const QUALITY_VOUCHER: usize = 8;

/// The `Merchant::Merchant` constructor's Q8 1.0 default for both modifiers.
pub const MERCHANT_VALUE_MOD_DEFAULT: i64 = Q8_ONE;

const GLOBAL_KNOBS_GC: &str = "GlobalKnobs.gc";
const MERCHANT_COMPONENT_CLASS_SUFFIX: &str = ".Merchant";
const KNOB_SUFFIXES: [&str; 8] = [
    "Quest",
    "Normal",
    "Superior",
    "Magical",
    "Rare",
    "Unique",
    "Collection",
    "Mythic",
];

/// The `GlobalKnobs.gc` values `Item::getValue` and `getBuyValue` read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Knobs {
    pub item_buy_value_modifier: i64,
    pub item_sell_value_modifier: i64,
    pub item_gold_value_per_level: i64,
    /// `GlobalKnobs.MemberGoldMod` (Q8 1.15). The currency generator's member
    /// base rate.
    pub member_gold_mod: i64,
    pub item_level_delta: [i64; QUALITY_COUNT],
    pub item_price_modifier: [i64; QUALITY_COUNT],
}

/// What one vendor's `.gc` states, with the constructor default tracked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerchantProfile {
    pub npc_fqn: String,
    pub buy_mod: i64,
    pub sell_mod: i64,
    pub level: i64,
    pub buy_mod_authored: bool,
    pub sell_mod_authored: bool,
}

/// The whole buy quote, including the provenance the caller logs and the bag
/// footprint it places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    pub gold: i64,
    pub width: u32,
    pub height: u32,
    pub quality_name: &'static str,
    pub buy_mod: i64,
    pub footprint: String,
}

fn mul_q8(value: i128, factor: i128) -> i128 {
    (value * factor) >> 8
}

fn sar8(value: i128) -> i64 {
    i64::from((value as i32) >> 8)
}

fn table(table: &[i64; QUALITY_COUNT], quality: Option<usize>) -> Option<i64> {
    quality
        .filter(|quality| *quality < QUALITY_COUNT)
        .map(|quality| table[quality])
}

fn prop<'a>(props: &'a [(String, String)], name: &str) -> Option<&'a str> {
    props
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn needed_q8(node: &crate::gc::Node, name: &str) -> Result<i64, ContentError> {
    let value = node
        .prop(name)
        .ok_or_else(|| ContentError::Blocked(format!("{GLOBAL_KNOBS_GC} states no {name}")))?;
    fixed32_q8(value).ok_or_else(|| {
        ContentError::Malformed(format!(
            "game.pkg: {GLOBAL_KNOBS_GC} states {name} = {value:?}, not a number"
        ))
    })
}

fn needed_int(node: &crate::gc::Node, name: &str) -> Result<i64, ContentError> {
    let value = node
        .prop(name)
        .ok_or_else(|| ContentError::Blocked(format!("{GLOBAL_KNOBS_GC} states no {name}")))?;
    gc_int(value).ok_or_else(|| {
        ContentError::Malformed(format!(
            "game.pkg: {GLOBAL_KNOBS_GC} states {name} = {value:?}, not an integer"
        ))
    })
}

/// The knobs, read out of `GlobalKnobs.gc`.
pub fn knobs(corpus: &mut Corpus) -> Result<Knobs, ContentError> {
    let root = corpus.tree(GLOBAL_KNOBS_GC)?;
    let node = root
        .children
        .first()
        .ok_or_else(|| ContentError::Blocked(format!("{GLOBAL_KNOBS_GC} defines no block")))?;
    let mut item_level_delta = [0i64; QUALITY_COUNT];
    let mut item_price_modifier = [0i64; QUALITY_COUNT];
    for (index, suffix) in KNOB_SUFFIXES.iter().enumerate() {
        item_level_delta[index] = needed_int(node, &format!("ItemLevelDelta{suffix}"))?;
        item_price_modifier[index] = needed_q8(node, &format!("ItemPriceModifier{suffix}"))?;
    }
    Ok(Knobs {
        item_buy_value_modifier: needed_q8(node, "ItemBuyValueModifier")?,
        item_sell_value_modifier: needed_q8(node, "ItemSellValueModifier")?,
        item_gold_value_per_level: needed_q8(node, "ItemGoldValuePerLevel")?,
        member_gold_mod: node
            .prop("MemberGoldMod")
            .and_then(fixed32_q8)
            .unwrap_or(Q8_ONE),
        item_level_delta,
        item_price_modifier,
    })
}

/// The merchant profile for a vendor classname, accepting both the component
/// spelling (`<npc>.Merchant`) and the NPC's own `.gc` spelling.
pub fn merchant_profile(
    corpus: &mut Corpus,
    npc_class: &str,
) -> Result<MerchantProfile, ContentError> {
    let fqn = npc_class
        .strip_suffix(MERCHANT_COMPONENT_CLASS_SUFFIX)
        .unwrap_or(npc_class);
    let mut props: Vec<(String, String)> = Vec::new();
    let mut authored: HashSet<String> = HashSet::new();
    let mut current = fqn.to_string();
    for _ in 0..32 {
        let Ok(node) = corpus.block_chain(&current) else {
            break;
        };
        for child in &node.children {
            if child.name.eq_ignore_ascii_case("Merchant") {
                for (key, value) in &child.props {
                    if authored.insert(key.clone()) {
                        props.push((key.clone(), value.clone()));
                    }
                }
            }
        }
        for (key, value) in &node.props {
            if authored.insert(key.clone()) {
                props.push((key.clone(), value.clone()));
            }
        }
        match node.extends.clone() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    if props.is_empty() {
        return Err(ContentError::Blocked(format!(
            "a .gc block for vendor {fqn:?}"
        )));
    }
    let buy_mod_authored = prop(&props, "BuyValueMod").is_some();
    let sell_mod_authored = prop(&props, "SellValueMod").is_some();
    let buy_mod = match prop(&props, "BuyValueMod") {
        Some(value) => fixed32_q8(value).ok_or_else(|| {
            ContentError::Malformed(format!(
                "game.pkg: {fqn} states BuyValueMod = {value:?}, not a number"
            ))
        })?,
        None => MERCHANT_VALUE_MOD_DEFAULT,
    };
    let sell_mod = match prop(&props, "SellValueMod") {
        Some(value) => fixed32_q8(value).ok_or_else(|| {
            ContentError::Malformed(format!(
                "game.pkg: {fqn} states SellValueMod = {value:?}, not a number"
            ))
        })?,
        None => MERCHANT_VALUE_MOD_DEFAULT,
    };
    let level = match prop(&props, "Level") {
        Some(value) => gc_int(value).ok_or_else(|| {
            ContentError::Malformed(format!(
                "game.pkg: {fqn} states Level = {value:?}, not an integer"
            ))
        })?,
        None => 0,
    };
    if !(0..=0xFFFF).contains(&level) {
        return Err(ContentError::Blocked(format!(
            "{fqn} states Level = {level}, which is not a u16"
        )));
    }
    Ok(MerchantProfile {
        npc_fqn: fqn.to_string(),
        buy_mod,
        sell_mod,
        level,
        buy_mod_authored,
        sell_mod_authored,
    })
}

/// The `Quality` a modifier class states, or `None` when it states none.
pub fn modifier_quality(
    corpus: &mut Corpus,
    modifier_fqn: &str,
) -> Result<Option<usize>, ContentError> {
    let props = chain_props(corpus, modifier_fqn)?;
    let Some(value) = prop(&props, "Quality") else {
        return Ok(None);
    };
    quality_index(value).map(Some).ok_or_else(|| {
        ContentError::Blocked(format!(
            "{modifier_fqn}'s Quality = {value:?}, which is not one of {}",
            QUALITY_NAMES.join(", ")
        ))
    })
}

/// `Item::getBestModifierQuality`, reproduced.
pub fn best_modifier_quality(
    corpus: &mut Corpus,
    _prof: &ItemProfile,
    modifier_fqns: &[String],
    wire_quality: Option<usize>,
) -> Result<usize, ContentError> {
    if let Some(quality) = wire_quality {
        if quality != QUALITY_DEFAULT {
            return Ok(quality);
        }
    }
    let mut best = QUALITY_DEFAULT;
    for modifier in modifier_fqns {
        if let Some(value) = modifier_quality(corpus, modifier)? {
            if value > best {
                best = value;
            }
        }
    }
    Ok(best)
}

/// `Item+0x78`: `ItemLevelDelta[quality]`, or 0 when out of range.
pub fn item_level_delta(knobs: &Knobs, quality: Option<usize>) -> i64 {
    table(&knobs.item_level_delta, quality).unwrap_or(0)
}

/// `Item::getValue(vendorLevel, isSell)` -> the integer `L`.
pub fn item_get_value(
    knobs: &Knobs,
    prof: &ItemProfile,
    item_level: u32,
    observer_level: u16,
    quality: Option<usize>,
    is_sell: bool,
) -> Result<i64, ContentError> {
    let Some(quality) = quality else {
        return Err(ContentError::Blocked(format!(
            "{}'s quality, so ItemLevelDelta and ItemPriceModifier are unknown",
            prof.fqn
        )));
    };
    let Some(gold_value_q8) = prof.gold_value_q8 else {
        return Err(ContentError::Blocked(format!(
            "{} states no GoldValue, so the price is not computable",
            prof.fqn
        )));
    };
    let level = i64::from(observer_level) & 0xFFFF;
    let override_value = prof.level_override & 0xFF;
    let mut q8 = if prof.scale_to_observer_level {
        level << 8
    } else if override_value != 0 {
        override_value << 8
    } else {
        let total = i64::from(item_level & 0xFF) + item_level_delta(knobs, Some(quality));
        let level = if total < 1 { 1 } else { total & 0xFF };
        level << 8
    };
    if q8 < Q8_ONE {
        q8 = Q8_ONE;
    }
    if is_sell && level > 0 {
        let cap = (level + 5) << 8;
        if q8 > cap {
            q8 = cap;
        }
    }
    let mut price = mul_q8(i128::from(knobs.item_gold_value_per_level), i128::from(q8));
    let mut modifier = i128::from(gold_value_q8.max(0));
    if override_value == 0 {
        if let Some(factor) = table(&knobs.item_price_modifier, Some(quality)) {
            modifier = mul_q8(modifier, i128::from(factor));
        }
    }
    price = mul_q8(price, modifier);
    Ok(sar8(price).max(1))
}

/// `Merchant::getBuyValue` -> whole gold for a stack of `count`.
pub fn buy_price(
    knobs: &Knobs,
    prof: &ItemProfile,
    item_level: u32,
    count: u32,
    merchant: &MerchantProfile,
    quality: usize,
    observer_level: u16,
) -> Result<i64, ContentError> {
    let value = item_get_value(
        knobs,
        prof,
        item_level,
        observer_level,
        Some(quality),
        false,
    )?;
    let mut price = mul_q8(
        i128::from(value) << 8,
        i128::from(knobs.item_buy_value_modifier),
    );
    price = mul_q8(price, i128::from(merchant.buy_mod));
    if prof.useless_to_merchant && price < i128::from(BUY_NOSELL_FLOOR) {
        price = i128::from(BUY_NOSELL_FLOOR);
    }
    price = mul_q8(price, i128::from(count & 0xFF) << 8);
    if price < i128::from(PRICE_FLOOR) {
        price = i128::from(PRICE_FLOOR);
    }
    Ok(sar8(price))
}

/// The client's sell value for one item (`Item::getValue(level, isSell=true)`).
///
/// Merchant-independent, like `HandleSellItem`, and capped at the matching
/// single-item buy price so a sale can never pay more than a purchase.
pub fn sell_price(
    corpus: &mut Corpus,
    merchant_class: &str,
    item_class: &str,
    item_level: u32,
    modifier_fqns: &[String],
    observer_level: Option<u16>,
) -> Result<i64, ContentError> {
    let Some(prof) = item_profile(corpus, item_class)? else {
        return Err(ContentError::Blocked(format!(
            "a .gc block for item class {item_class:?}, so it has no price"
        )));
    };
    let merchant = merchant_profile(corpus, merchant_class)?;
    let knobs = knobs(corpus)?;
    let wire_quality = prof.quality.unwrap_or(QUALITY_DEFAULT);
    let quality = best_modifier_quality(corpus, &prof, modifier_fqns, Some(wire_quality))?;
    let level = observer_level.unwrap_or(merchant.level as u16);
    let sell = item_get_value(&knobs, &prof, item_level, level, Some(quality), true)?;
    let buy = buy_price(&knobs, &prof, item_level, 1, &merchant, quality, level)?;
    Ok(sell.min(buy).max(1))
}

/// The whole buy price for a merchant record, with the footprint the bag uses.
pub fn price(
    corpus: &mut Corpus,
    merchant_class: &str,
    item_class: &str,
    item_level: u32,
    count: u32,
    modifier_fqns: &[String],
    observer_level: Option<u16>,
) -> Result<Quote, ContentError> {
    let Some(prof) = item_profile(corpus, item_class)? else {
        return Err(ContentError::Blocked(format!(
            "a .gc block for item class {item_class:?}, so it has no footprint or price"
        )));
    };
    let merchant = merchant_profile(corpus, merchant_class)?;
    let knobs = knobs(corpus)?;
    let wire_quality = prof.quality.unwrap_or(QUALITY_DEFAULT);
    let quality = best_modifier_quality(corpus, &prof, modifier_fqns, Some(wire_quality))?;
    let level = observer_level.unwrap_or(merchant.level as u16);
    let gold = buy_price(&knobs, &prof, item_level, count, &merchant, quality, level)?;
    Ok(Quote {
        gold,
        width: prof.width,
        height: prof.height,
        quality_name: QUALITY_NAMES.get(quality).copied().unwrap_or("VOUCHER"),
        buy_mod: merchant.buy_mod,
        footprint: format!("{}x{}", prof.width, prof.height),
    })
}

/// Can `observed` be a `getBuyValue` result at this `BuyValueMod`?
pub fn reconcile(observed: i64, buy_mod: i64) -> Result<Option<(i64, i64)>, ContentError> {
    if buy_mod <= 0 {
        return Err(ContentError::Blocked(format!(
            "a positive BuyValueMod, got {buy_mod}"
        )));
    }
    let low = observed << 8;
    let high = low + Q8_ONE - 1;
    let lo = low.div_euclid(buy_mod) + i64::from(low.rem_euclid(buy_mod) != 0);
    let hi = high / buy_mod;
    Ok((lo <= hi).then_some((lo, hi)))
}
