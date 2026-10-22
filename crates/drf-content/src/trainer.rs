// SPDX-License-Identifier: GPL-3.0-or-later

//! The `SkillTrainer` block a class authors, and the `skills` rows it offers.
//!
//! A trainer's component is only a REFERENCE: the client already holds the
//! `SkillTrainerDesc` and its `AvailableSkill` list from `game.pkg`. The server
//! needs the same rows to resolve a purchase hash, price the next rank and
//! enforce the authored class-passive exclusivity. Nothing here reads a capture
//! or a hand-written skill table.

use crate::corpus::ContentError;
use crate::gc::Node;
use crate::zone::{class_description_property, class_is_kind_of, class_nodes_named};
use crate::Archive;

/// One purchasable rank row authored under a trainer's `SkillTrainerDesc`.
#[derive(Debug, Clone, PartialEq)]
pub struct TrainerSkill {
    /// The skill class FQN exactly as the trainer's `AvailableSkill.Skill` row
    /// spells it.
    pub fqn: String,
    /// The client's case-folded DJB2 id for `fqn`.
    pub hash: u32,
    /// `Description.GoldValueMod`.
    pub gold_value_mod: f64,
    /// `Description.RequiredLevel`.
    pub required_level: u32,
    /// `Description.RequiredLevelInc` in Q8 (`1.0 == 0x100`). The client's
    /// `SkillDesc::getCost` spaces ranks by this, not by `GoldValueMod`.
    pub required_level_inc_q8: i64,
    /// `Description.MaxSkillLevel` (the highest trainable rank).
    pub max_level: u32,
    /// `Description.ProfessionType`, when the skill states one.
    pub profession: Option<String>,
    /// Whether the class chain roots in a passive skill.
    pub passive: bool,
}

/// A trainer NPC's derived component and purchasable rows.
#[derive(Debug, Clone, PartialEq)]
pub struct TrainerCatalog {
    /// The component class the NPC create must reference
    /// (`<owner>.<SkillTrainer block name>`).
    pub component_class: String,
    /// Every `AvailableSkill`, in declaration order.
    pub skills: Vec<TrainerSkill>,
}

/// `SkillTrainerDesc` rows under `npc_class`'s `extends` chain.
///
/// `Ok(None)` when the class authors no `SkillTrainer` block. A trainer whose
/// block offers a skill with no usable authored price/rank is malformed, never
/// silently skipped.
pub fn derive_trainer(
    archive: &Archive,
    npc_class: &str,
) -> Result<Option<TrainerCatalog>, ContentError> {
    let mut trainer = None;
    for (owner, node) in class_nodes_named(archive, npc_class)? {
        if let Some(block) = node
            .children
            .iter()
            .find(|child| child.name.eq_ignore_ascii_case("SkillTrainer"))
        {
            trainer = Some((owner, block.clone()));
            break;
        }
    }
    let Some((owner, block)) = trainer else {
        return Ok(None);
    };

    let mut refs = Vec::new();
    collect_available_skills(&block, &mut refs);
    let mut skills = Vec::with_capacity(refs.len());
    for fqn in refs {
        skills.push(derive_skill(archive, &fqn)?);
    }
    Ok(Some(TrainerCatalog {
        component_class: format!("{owner}.{}", block.name),
        skills,
    }))
}

/// The component class a trainer NPC's create record references.
pub fn trainer_component_class(
    archive: &Archive,
    npc_class: &str,
) -> Result<Option<String>, ContentError> {
    Ok(derive_trainer(archive, npc_class)?.map(|catalog| catalog.component_class))
}

/// Whether the skill's `extends` chain roots in a passive class.
pub fn skill_is_passive(archive: &Archive, fqn: &str) -> Result<bool, ContentError> {
    Ok(class_is_kind_of(archive, fqn, "PassiveSkill")?
        || class_is_kind_of(archive, fqn, "ActivePassiveSkill")?)
}

/// The authored `SkillValuePerLevel` knob, or `None` when `game.pkg` omits it.
pub fn skill_value_per_level(archive: &Archive) -> Result<Option<f64>, ContentError> {
    Ok(
        match crate::zone::class_property(archive, "GlobalKnobs", "SkillValuePerLevel")? {
            Some(value) => value.trim().parse::<f64>().ok(),
            None => None,
        },
    )
}

/// The gold cost of the next rank for `skill`, using the client's own field
/// formula and the authored `SkillValuePerLevel` knob.
pub fn train_cost(value_per_level: f64, skill: &TrainerSkill, next_level: u32) -> u32 {
    if next_level == 0 || value_per_level <= 0.0 || skill.gold_value_mod <= 0.0 {
        return 1;
    }
    let value_q8 = (value_per_level * 256.0) as i64;
    let gvm_q8 = (skill.gold_value_mod * 256.0) as i64;
    let line_q8 = i64::from(skill.required_level) * 0x100
        + i64::from(next_level.saturating_sub(1)) * skill.required_level_inc_q8;
    let line_q8 = line_q8 & !0xFF;
    let cost_q8 = ((line_q8.saturating_mul(value_q8) >> 8).saturating_mul(gvm_q8)) >> 8;
    let cost = cost_q8 >> 8;
    if cost < 1 {
        1
    } else {
        u32::try_from(cost).unwrap_or(u32::MAX)
    }
}

fn unquote(value: &str) -> String {
    value.trim().trim_matches('"').to_string()
}

fn collect_available_skills(node: &Node, out: &mut Vec<String>) {
    for child in &node.children {
        if child
            .extends
            .as_deref()
            .is_some_and(|target| target.eq_ignore_ascii_case("AvailableSkill"))
        {
            if let Some(skill) = child.prop("Skill").map(unquote) {
                if !skill.is_empty() {
                    out.push(skill);
                }
            }
        }
        collect_available_skills(child, out);
    }
}

fn derive_skill(archive: &Archive, fqn: &str) -> Result<TrainerSkill, ContentError> {
    let number = |name: &str| -> Result<u32, ContentError> {
        let value = class_description_property(archive, fqn, name)?
            .ok_or_else(|| ContentError::Malformed(format!("{fqn} authors no {name}")))?;
        value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| *value > 0.0)
            .and_then(|value| u32::try_from(value as u64).ok())
            .ok_or_else(|| ContentError::Malformed(format!("{fqn} has an unusable {name}")))
    };
    let gold_value_mod = class_description_property(archive, fqn, "GoldValueMod")?
        .ok_or_else(|| ContentError::Malformed(format!("{fqn} authors no GoldValueMod")))?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| *value > 0.0)
        .ok_or_else(|| ContentError::Malformed(format!("{fqn} has an unusable GoldValueMod")))?;
    let required_level_inc_q8 = class_description_property(archive, fqn, "RequiredLevelInc")?
        .as_deref()
        .and_then(crate::item::fixed32_q8)
        .filter(|value| *value > 0)
        .ok_or_else(|| ContentError::Malformed(format!("{fqn} has no RequiredLevelInc")))?;
    let profession = class_description_property(archive, fqn, "ProfessionType")?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("NONE"));
    Ok(TrainerSkill {
        fqn: fqn.to_string(),
        hash: crate::dfc_hash(fqn.as_bytes()),
        gold_value_mod,
        required_level: number("RequiredLevel")?,
        required_level_inc_q8,
        max_level: number("MaxSkillLevel")?,
        profession,
        passive: skill_is_passive(archive, fqn)?,
    })
}
