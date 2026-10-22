// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-class content derived from `game.pkg`.
//!
//! The character graph and the in-world avatar records carry a class's
//! starting kit as fixed-width content hashes, its skills, its profession and
//! its `Visuals` reference. Every one of those is a value the archive states;
//! this module derives them and refuses to substitute a placeholder.

use std::collections::HashMap;

use crate::corpus::{ContentError, Corpus};
use crate::hash::dfc_hash;

/// The three class bases `game.pkg` ships.
pub const CLASS_BASES: [&str; 3] = ["Fighter", "Ranger", "Warlock"];
/// The two sexes each base ships.
pub const CLASS_SEXES: [&str; 2] = ["Male", "Female"];

/// The item slots of a starting kit, in `StartingEquipment.gc` order.
pub const KIT_ITEM_IDS: [i64; 4] = [10, 6, 2, 7];
/// Every starting item is granted at level 1.
pub const KIT_ITEM_LEVELS: [i64; 4] = [1, 1, 1, 1];
/// The skill ids of a starting kit, in `StartingSkills.gc` order.
pub const KIT_SKILL_IDS: [i64; 4] = [105, 100, 108, 109];
/// Every starting skill is granted at level 1.
pub const KIT_SKILL_LEVELS: [i64; 4] = [1, 1, 1, 1];

/// The DFC root a starting skill carries when it is also a `Manipulators`
/// entry (`ActiveSkill` is a `Manipulator`; `PassiveSkill` is not).
pub const AVATAR_MANIPULATOR_SKILL_DFC: &str = "ActiveSkill";

/// The profession values the client's `ProfessionType` enum accepts.
pub const PROFESSION_TYPE_VALUES: [&str; 5] = ["NONE", "MAGE", "RANGER", "FIGHTER", "SUMMONER"];
/// The values the client's `ElementType` enum accepts.
pub const ELEMENT_TYPE_VALUES: [&str; 7] = [
    "NONE", "FIRE", "ICE", "SHADOW", "POISON", "DIVINE", "PHYSICAL",
];

/// One class's starting kit, as the character graph encodes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassKit {
    pub profession: u32,
    pub profession_fqn: String,
    pub skills: Vec<u32>,
    pub skill_fqns: Vec<String>,
    pub skill_ids: Vec<i64>,
    pub skill_levels: Vec<i64>,
    pub items: Vec<u32>,
    pub item_fqns: Vec<String>,
    pub types: Vec<u32>,
    pub item_dfc: Vec<String>,
    pub skill_dfc: Vec<String>,
}

/// One starting skill as the in-world `Skills`/`Manipulators` wire carries it:
/// the FQN, the per-entry numeric ID, and the level byte.
pub type WireSkill = (String, u32, u8);

/// A class's starting skills shaped for the `avatar.base.Skills` and
/// `Manipulators` records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassSkillWire {
    /// Every numbered starting skill, in `StartingSkills.gc` order.
    pub skills: Vec<WireSkill>,
    /// The profession FQN list (exactly one entry).
    pub professions: Vec<String>,
    /// The subset of `skills` whose DFC root is [`AVATAR_MANIPULATOR_SKILL_DFC`].
    pub manipulators: Vec<WireSkill>,
}

impl ClassKit {
    /// Shape this kit's skills for the in-world `Skills`/`Manipulators` wire.
    ///
    /// Fails closed on an ID or level the wire's field width cannot hold; it
    /// never truncates and never invents a profession.
    pub fn skill_wire(&self) -> Result<ClassSkillWire, ContentError> {
        let mut skills = Vec::with_capacity(self.skill_fqns.len());
        let mut manipulators = Vec::new();
        for (index, fqn) in self.skill_fqns.iter().enumerate() {
            let id = u32::try_from(self.skill_ids[index]).map_err(|_| {
                ContentError::Malformed(format!(
                    "{fqn}: skill ID {} is outside u32",
                    self.skill_ids[index]
                ))
            })?;
            let level = u8::try_from(self.skill_levels[index]).map_err(|_| {
                ContentError::Malformed(format!(
                    "{fqn}: skill level {} is outside u8",
                    self.skill_levels[index]
                ))
            })?;
            let skill = (fqn.clone(), id, level);
            if self.skill_dfc[index] == AVATAR_MANIPULATOR_SKILL_DFC {
                manipulators.push(skill.clone());
            }
            skills.push(skill);
        }
        if self.profession_fqn.is_empty() {
            return Err(ContentError::Malformed(
                "class kit names no profession FQN".into(),
            ));
        }
        Ok(ClassSkillWire {
            skills,
            professions: vec![self.profession_fqn.clone()],
            manipulators,
        })
    }
}

/// One starting item, as the in-world `Equipment` record needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassItem {
    pub fqn: String,
    pub slot: i64,
    pub level: i64,
    pub dfc: String,
    pub authored: usize,
    pub authored_fqns: Vec<String>,
    pub authored_terminal: String,
    pub mods: Vec<String>,
}

/// One class FQN and its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassFqn {
    pub base: String,
    pub hash: u32,
}

/// One starting skill with its element data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillElements {
    pub fqn: String,
    pub id: i64,
    pub level: i64,
    pub dfc: String,
    pub profession_type: String,
    pub element_type: String,
}

/// The per-class kit as the character graph encodes it.
pub fn derive_class_kit(corpus: &mut Corpus, base: &str) -> Result<ClassKit, ContentError> {
    if !CLASS_BASES.contains(&base) {
        return Err(ContentError::NotFound(format!("class base {base:?}")));
    }
    let base_fqn = format!("avatar.classes.{base}Base");
    let skills_fqn = require_prop(corpus, &base_fqn, "StartingSkillsList")?;
    let equip_fqn = require_prop(corpus, &base_fqn, "StartingEquipmentList")?;
    let skills = corpus.list_entries(&skills_fqn, false)?;
    let items = corpus.list_entries(&equip_fqn, false)?;

    let professions: Vec<&crate::corpus::ListEntry> =
        skills.iter().filter(|e| e.id.is_none()).collect();
    let numbered: Vec<&crate::corpus::ListEntry> =
        skills.iter().filter(|e| e.id.is_some()).collect();
    if professions.len() != 1 {
        return Err(ContentError::Malformed(format!(
            "{skills_fqn}: expected exactly one profession, got {}",
            professions.len()
        )));
    }
    let ids: Vec<i64> = numbered.iter().filter_map(|e| e.id).collect();
    if ids != KIT_SKILL_IDS {
        return Err(ContentError::Malformed(format!(
            "{skills_fqn}: skill IDs {ids:?} are not the isomorphic kit {KIT_SKILL_IDS:?}"
        )));
    }
    let item_ids: Vec<i64> = items.iter().filter_map(|e| e.id).collect();
    if item_ids != KIT_ITEM_IDS {
        return Err(ContentError::Malformed(format!(
            "{equip_fqn}: item slot IDs {item_ids:?} are not the isomorphic kit {KIT_ITEM_IDS:?}"
        )));
    }
    let levels: Vec<i64> = numbered.iter().filter_map(|e| e.level).collect();
    if levels != KIT_SKILL_LEVELS {
        return Err(ContentError::Malformed(format!(
            "{skills_fqn}: skill levels {levels:?} are not the isomorphic kit {KIT_SKILL_LEVELS:?}"
        )));
    }

    let profession_fqn = professions[0].fqn.clone();
    let skill_fqns: Vec<String> = numbered.iter().map(|e| e.fqn.clone()).collect();
    let skill_ids: Vec<i64> = numbered.iter().filter_map(|e| e.id).collect();
    let skill_levels: Vec<i64> = numbered.iter().filter_map(|e| e.level).collect();
    let item_fqns: Vec<String> = items.iter().map(|e| e.fqn.clone()).collect();
    let item_dfc: Vec<String> = item_fqns
        .iter()
        .map(|fqn| corpus.dfc_root(fqn))
        .collect::<Result<_, _>>()?;
    let skill_dfc: Vec<String> = skill_fqns
        .iter()
        .map(|fqn| corpus.dfc_root(fqn))
        .collect::<Result<_, _>>()?;

    Ok(ClassKit {
        profession: dfc_hash(profession_fqn.as_bytes()),
        profession_fqn,
        skills: skill_fqns.iter().map(|f| dfc_hash(f.as_bytes())).collect(),
        skill_fqns,
        skill_ids,
        skill_levels,
        items: item_fqns.iter().map(|f| dfc_hash(f.as_bytes())).collect(),
        item_fqns,
        types: item_dfc.iter().map(|f| dfc_hash(f.as_bytes())).collect(),
        item_dfc,
        skill_dfc,
    })
}

/// The per-class starting kit as the in-world `Equipment` record needs it.
pub fn derive_class_items(corpus: &mut Corpus, base: &str) -> Result<Vec<ClassItem>, ContentError> {
    if !CLASS_BASES.contains(&base) {
        return Err(ContentError::NotFound(format!("class base {base:?}")));
    }
    let equip_fqn = require_prop(
        corpus,
        &format!("avatar.classes.{base}Base"),
        "StartingEquipmentList",
    )?;
    let entries = corpus.list_entries(&equip_fqn, true)?;
    let item_ids: Vec<i64> = entries.iter().filter_map(|e| e.id).collect();
    if item_ids != KIT_ITEM_IDS {
        return Err(ContentError::Malformed(format!(
            "{equip_fqn}: item slot IDs {item_ids:?} are not the isomorphic kit {KIT_ITEM_IDS:?}"
        )));
    }
    let levels: Vec<i64> = entries.iter().filter_map(|e| e.level).collect();
    if levels != KIT_ITEM_LEVELS {
        return Err(ContentError::Malformed(format!(
            "{equip_fqn}: item levels {levels:?} are not the isomorphic kit {KIT_ITEM_LEVELS:?}"
        )));
    }
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let (authored, terminal) = corpus.authored_modifiers(&entry.fqn)?;
        out.push(ClassItem {
            dfc: corpus.dfc_root(&entry.fqn)?,
            slot: entry.id.unwrap_or(0),
            level: entry.level.unwrap_or(0),
            authored: authored.len(),
            authored_fqns: authored.iter().map(|(_, fqn)| fqn.clone()).collect(),
            authored_terminal: terminal,
            mods: entry.mods,
            fqn: entry.fqn,
        });
    }
    Ok(out)
}

/// `(profession_type, element_type)` for one skill FQN, as `game.pkg` says.
pub fn derive_skill_elements(
    corpus: &mut Corpus,
    skill_fqn: &str,
) -> Result<(String, String), ContentError> {
    let prof = corpus
        .prop(skill_fqn, "ProfessionType")?
        .ok_or_else(|| ContentError::NotFound(format!("{skill_fqn} ProfessionType row")))?;
    let elem = corpus
        .prop(skill_fqn, "ElementType")?
        .ok_or_else(|| ContentError::NotFound(format!("{skill_fqn} ElementType row")))?;
    let prof = prof.trim().to_string();
    let elem = elem.trim().to_string();
    if !PROFESSION_TYPE_VALUES.contains(&prof.as_str()) {
        return Err(ContentError::Malformed(format!(
            "{skill_fqn}: ProfessionType {prof:?} is not in the client's domain"
        )));
    }
    if !ELEMENT_TYPE_VALUES.contains(&elem.as_str()) {
        return Err(ContentError::Malformed(format!(
            "{skill_fqn}: ElementType {elem:?} is not in the client's domain"
        )));
    }
    Ok((prof, elem))
}

/// The per-class skills with their element data, in `StartingSkills` order.
pub fn derive_class_skill_elements(
    corpus: &mut Corpus,
    base: &str,
) -> Result<Vec<SkillElements>, ContentError> {
    let kit = derive_class_kit(corpus, base)?;
    let mut out = Vec::with_capacity(kit.skill_fqns.len());
    for (index, fqn) in kit.skill_fqns.iter().enumerate() {
        let (profession_type, element_type) = derive_skill_elements(corpus, fqn)?;
        out.push(SkillElements {
            fqn: fqn.clone(),
            id: kit.skill_ids[index],
            level: kit.skill_levels[index],
            dfc: kit.skill_dfc[index].clone(),
            profession_type,
            element_type,
        });
    }
    Ok(out)
}

/// `{fqn: ClassFqn}` for the six classes `game.pkg` ships.
pub fn derive_class_fqns(corpus: &mut Corpus) -> Result<HashMap<String, ClassFqn>, ContentError> {
    let mut found = HashMap::new();
    for base in CLASS_BASES {
        for sex in CLASS_SEXES {
            let fqn = format!("avatar.classes.{base}{sex}");
            let rel = format!("avatar/classes/{base}{sex}.gc");
            if corpus.tree(&rel).is_err() {
                continue;
            }
            let extends = corpus
                .lookup_holder_extends(&fqn)
                .ok_or_else(|| ContentError::Malformed(format!("{fqn} has no block")))?;
            let want = format!("{}base", base.to_lowercase());
            if !extends.to_lowercase().ends_with(&want) {
                return Err(ContentError::Malformed(format!(
                    "{fqn} does not extend {base}Base (found {extends:?})"
                )));
            }
            found.insert(
                fqn.clone(),
                ClassFqn {
                    base: base.to_string(),
                    hash: dfc_hash(fqn.as_bytes()),
                },
            );
        }
    }
    Ok(found)
}

/// `avatar/base/Avatar.gc`, the archetype the inventory roster is read from.
pub const AVATAR_BASE_AVATAR_GC: &str = "avatar/base/Avatar.gc";

/// The first id an `Inventory` roster entry carries (`AVATAR_INVENTORY_ID_BASE`).
pub const AVATAR_INVENTORY_ID_BASE: u8 = 0x0B;

/// One `UnitContainer` child the avatar's own archetype declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryEntry {
    /// The inventory class FQN (`avatar.base.Inventory`).
    pub fqn: String,
    /// The id the child carries (`0x0B`, `0x0C`, ...), in roster order.
    pub id: u8,
}

/// The nine inventories a level-1 avatar holds, derived from `game.pkg`.
///
/// Read from `avatar/base/Avatar.gc`'s `Default*Object` rows, in file order,
/// keeping only those whose `avatar/base/<leaf>.gc` block extends `Inventory`.
/// `Skills`, `Equipment` and `UnitBehavior` share the same list but are not
/// container children, so the extends test is the discriminator. The id base
/// is a constant because nothing in `game.pkg` states it.
pub fn derive_avatar_inventory_roster(
    corpus: &mut Corpus,
) -> Result<Vec<InventoryEntry>, ContentError> {
    let tree = corpus.tree(AVATAR_BASE_AVATAR_GC)?;
    let mut declared = Vec::new();
    collect_default_objects(&tree, &mut declared);
    let mut out = Vec::new();
    for fqn in declared {
        if !avatar_base_extends_inventory(corpus, &fqn)? {
            continue;
        }
        let id = AVATAR_INVENTORY_ID_BASE + out.len() as u8;
        out.push(InventoryEntry { fqn, id });
    }
    Ok(out)
}

/// Every `Default*Object = avatar.base.*;` row in a block tree, in file order.
fn collect_default_objects(node: &crate::gc::Node, out: &mut Vec<String>) {
    for (key, value) in &node.props {
        if key.starts_with("Default") && value.starts_with("avatar.base.") {
            out.push(value.clone());
        }
    }
    for child in &node.children {
        collect_default_objects(child, out);
    }
}

/// Does `avatar/base/<leaf>.gc` extend `Inventory`?
fn avatar_base_extends_inventory(corpus: &mut Corpus, fqn: &str) -> Result<bool, ContentError> {
    let leaf = fqn.rsplit('.').next().unwrap_or("");
    let path = format!("avatar/base/{leaf}.gc");
    let Ok(tree) = corpus.tree(&path) else {
        return Ok(false);
    };
    let Some(root) = tree.children.first() else {
        return Ok(false);
    };
    Ok(root
        .extends
        .as_deref()
        .map(|target| target.rsplit('.').next().unwrap_or("") == "Inventory")
        .unwrap_or(false))
}

/// One manipulator `npc/Base.gc` declares for `npc.Base.Manipulators`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpcManipulator {
    /// The instance FQN the client instantiates (`npc.Base.Manipulators.
    /// PrimaryWeapon`).
    pub fqn: String,
    /// The child block's own `ID` row.
    pub id: u32,
}

/// The one manipulator `game.pkg`'s `npc/Base.gc` states.
///
/// The block reads `Manipulators extends Manipulators { PrimaryWeapon extends
/// creatures.base.weapons.melee { ID = 10; } }`. The instance FQN is the
/// file's own path plus the block chain (the same identity the client
/// instantiates); the ID is the child's own `ID` row. `None` when the block,
/// a child, or its `ID` is absent.
pub fn derive_npc_base_manipulator(
    corpus: &mut Corpus,
) -> Result<Option<NpcManipulator>, ContentError> {
    let path = "npc/Base.gc";
    let Ok(tree) = corpus.tree(path) else {
        return Ok(None);
    };
    let Some(root) = tree.children.first() else {
        return Ok(None);
    };
    let Some(manipulators) = root
        .children
        .iter()
        .find(|child| child.name.eq_ignore_ascii_case("Manipulators"))
    else {
        return Ok(None);
    };
    let Some(entry) = manipulators
        .children
        .iter()
        .find(|child| child.extends.is_some())
    else {
        return Ok(None);
    };
    let Some(id) = entry
        .prop("ID")
        .and_then(|value| value.trim().parse::<u32>().ok())
    else {
        return Ok(None);
    };
    let file_fqn = path.trim_end_matches(".gc").replace('/', ".");
    Ok(Some(NpcManipulator {
        fqn: format!("{file_fqn}.Manipulators.{}", entry.name),
        id,
    }))
}

/// The `Visuals = ` FQN for one stored class, as `game.pkg` states it.
pub fn derive_class_visuals(corpus: &mut Corpus, aclass: &str) -> Result<String, ContentError> {
    let stored = aclass.split('\u{0}').next().unwrap_or("");
    let short = stored.rsplit('.').next().unwrap_or("");
    if short.is_empty() {
        return Err(ContentError::NotFound(format!("class named {stored:?}")));
    }
    let rel = format!("avatar/classes/{short}.gc");
    if corpus.tree(&rel).is_err() {
        return Err(ContentError::NotFound(format!(
            ".gc entry {rel:?} for class {stored:?}"
        )));
    }
    let value = corpus
        .prop(stored, "Visuals")?
        .ok_or_else(|| ContentError::NotFound(format!("{stored} Visuals row")))?;
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(ContentError::NotFound(format!("{stored} Visuals row")));
    }
    let target = format!("{}.gc", value.replace('.', "/"));
    if corpus.tree(&target).is_err() {
        return Err(ContentError::NotFound(format!(
            "{stored}: Visuals {value:?} names no .gc entry"
        )));
    }
    Ok(value)
}

fn require_prop(corpus: &mut Corpus, fqn: &str, name: &str) -> Result<String, ContentError> {
    corpus
        .prop(fqn, name)?
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| ContentError::NotFound(format!("{fqn} {name} row")))
}
