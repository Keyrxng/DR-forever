// SPDX-License-Identifier: GPL-3.0-or-later

//! The authored Bling Gnome skill + summon contract, read from `game.pkg`.
//!
//! The runtime values are not a table: they are the skill's own
//! `Description`/`Effect` chain and the summon's `Behavior.Description`. The
//! profile is shared by the server tick and the proto packet builders so the
//! convert radius, ratio and gold multiplier cannot drift from the content.

use crate::corpus::ContentError;
use crate::zone::class_description_property;
use crate::Archive;

/// The one player skill that summons the Bling Gnome.
pub const BLING_GNOME_SKILL: &str = "skills.generic.SummonBlingGnome";

/// The authored henchman/summon class the skill's `SummonedUnitType` names.
pub const BLING_GNOME_SUMMON: &str = "creatures.summon.blinggnome.base.BlingGnome_Summon";

/// One derived Bling Gnome contract.
#[derive(Debug, Clone, PartialEq)]
pub struct BlingGnomeProfile {
    pub skill: String,
    pub summon: String,
    /// The `Behavior` component class (`<summon>.Behavior`).
    pub behavior: String,
    pub cooldown_seconds: f64,
    /// `SkillDesc.GoldValueMod` (Q8.8 on the wire).
    pub gold_value_mod: f64,
    /// `SpellModEffect.Duration` in seconds.
    pub effect_duration_seconds: f64,
    /// `SpellConvertItemsToGoldEffect.SearchRadius`.
    pub search_radius: u32,
    /// `BehaviorDesc.SearchRange` for passive gold sniffing.
    pub search_range: u32,
    /// `BehaviorDesc.ConversionRatio`.
    pub conversion_ratio: f64,
    /// `SummonDesc.Speed`.
    pub speed: f64,
    /// `SummonDesc.MaxHealth` scale on the henchman curve.
    pub max_health: f64,
}

fn number(archive: &Archive, class: &str, name: &str) -> Result<f64, ContentError> {
    let value = class_description_property(archive, class, name)?
        .ok_or_else(|| ContentError::Malformed(format!("{class} authors no {name}")))?;
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| ContentError::Malformed(format!("{class} has an unusable {name}")))
}

/// Derive the contract, or `None` when the package has no Bling Gnome skill.
pub fn bling_gnome_profile(archive: &Archive) -> Result<Option<BlingGnomeProfile>, ContentError> {
    let Some(summon) = class_description_property(archive, BLING_GNOME_SKILL, "SummonedUnitType")?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let behavior = format!("{summon}.Behavior");
    Ok(Some(BlingGnomeProfile {
        skill: BLING_GNOME_SKILL.to_string(),
        summon: summon.clone(),
        behavior: behavior.clone(),
        cooldown_seconds: number(archive, BLING_GNOME_SKILL, "CoolDown")?,
        gold_value_mod: number(archive, BLING_GNOME_SKILL, "GoldValueMod")?,
        effect_duration_seconds: number(archive, BLING_GNOME_SKILL, "Duration")?,
        search_radius: number(archive, BLING_GNOME_SKILL, "SearchRadius")? as u32,
        search_range: number(archive, &behavior, "SearchRange")? as u32,
        conversion_ratio: number(archive, &behavior, "ConversionRatio")?,
        speed: number(archive, &summon, "Speed")?,
        max_health: number(archive, &summon, "MaxHealth")?,
    }))
}
