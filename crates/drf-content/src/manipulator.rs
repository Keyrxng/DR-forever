// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-creature `Manipulators` resolution from `game.pkg`.
//!
//! A monster's client-simulated brain attacks with whatever manipulators its
//! spawn stream declares: a Melee brain swings its `PrimaryWeapon`, a Ranged
//! brain fires it, a Caster brain casts its `CreatureBolt`. Ground truth is the
//! creature content — every creature's merged `Manipulators` block names its
//! concrete weapon/skill objects. This module resolves those blocks, ported
//! from the reference
//! `dr_pyserver.managers.creature_manipulators.manipulators_for`:
//!
//! * the creature's `extends` chain (the spawn entity, then its creature bases)
//!   is walked root-most first, merging every `Manipulators` child;
//! * a same-named slot (e.g. `PrimaryWeapon`) is overridden by the most-derived
//!   block that authors its own `extends`, exactly like the reference's
//!   `GCDatabase._merge_nodes`;
//! * selection is by NATIVE ROOT CLASS, not slot name: a row whose gc walks
//!   (via `extends`) to `ActiveSkill` is a skill block; `MeleeWeapon` /
//!   `RangedWeapon` are weapon blocks; anything else (`AttributeModifier`,
//!   `base.Effect`) is skipped — the client's manipulator reader would reject
//!   it and desync the stream;
//! * the emitted order is skills first, then weapons (the reference's
//!   live-proven order); duplicates (same gc) collapse.
//!
//! A creature that authors no emittable manipulator (e.g. the `Whisker Warg
//! Pup`, whose `PrimaryWeapon` authors only a `Description` and inherits its
//! `extends` from `creatures.base.UnitMelee` — which resolves to the generic
//! base melee weapon anyway) falls back to the generic base melee weapon
//! `creatures.base.weapons.melee` id `10`, exactly the reference default.
//!
//! Wire layouts (client-verified 2026-06-11) live in `drf_proto::mob`; this
//! module only names the entries.

use crate::corpus::Corpus;
use crate::gc::Node;

/// The `extends` target of the generic base melee weapon every plain melee
/// unit inherits (`creatures/base/UnitMelee.gc` `PrimaryWeapon`, `ID = 10`).
pub const DEFAULT_CREATURE_MELEE_GC: &str = "creatures.base.weapons.melee";
/// The base melee weapon's authored `ID`.
pub const DEFAULT_CREATURE_MELEE_ID: u32 = 10;

/// The concrete `MonsterBehavior2` subclasses a spawn may name. The client
/// resolves `base.MonsterBehavior` to `MonsterBehavior2`; a bare/native
/// behavior has no weapon hookup, so the join sanitizes to one of these.
pub const CREATURE_BEHAVIOR_MELEE: &str = "creatures.base.behavior.melee";
/// The ranged brain (fires its `RangedWeapon`).
pub const CREATURE_BEHAVIOR_RANGED: &str = "creatures.base.behavior.ranged";
/// The caster brain (casts its `ActiveSkill` blocks).
pub const CREATURE_BEHAVIOR_CASTER: &str = "creatures.base.behavior.caster";

/// How many `extends` hops the chain walk follows before giving up.
const EXTENDS_HOP_LIMIT: usize = 8;
/// An imported `Range` at or above this is treated as a ranged weapon when the
/// native root cannot be resolved (content authors melee 6-8, ranged 75-90).
const RANGED_RANGE_THRESHOLD: f64 = 20.0;

/// The wire body selector for one manipulator entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManipulatorKind {
    /// An `ActiveSkill`-rooted block: `<gcType> <id:u32> 0x00 <flags:0x00>`.
    Skill,
    /// A `MeleeWeapon`-rooted block: one trailing byte more than ranged.
    Melee,
    /// A `RangedWeapon`-rooted block.
    Ranged,
}

/// One manipulator block of the monster spawn stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatureManipulator {
    /// The authored gc instance the client instantiates (case preserved).
    pub gc_type: String,
    /// The authored `ID` row, or `0` when the block states none.
    pub id: u32,
    /// Which wire body to encode.
    pub kind: ManipulatorKind,
}

impl CreatureManipulator {
    /// The generic base melee weapon, the fallback when a creature authors no
    /// emittable manipulator.
    pub fn default_melee() -> CreatureManipulator {
        CreatureManipulator {
            gc_type: DEFAULT_CREATURE_MELEE_GC.to_string(),
            id: DEFAULT_CREATURE_MELEE_ID,
            kind: ManipulatorKind::Melee,
        }
    }
}

/// The manipulator blocks to send for `creature_fqn` (the spawn entity's
/// classname, e.g. `world.dungeon00.mob.melee01.rank1`).
///
/// Never fails: an unresolvable chain, or a creature that authors nothing
/// emittable, yields the generic base melee weapon.
pub fn creature_manipulators(corpus: &mut Corpus, creature_fqn: &str) -> Vec<CreatureManipulator> {
    let mut chain = Vec::new();
    let mut current = creature_fqn.trim().to_string();
    for _ in 0..EXTENDS_HOP_LIMIT {
        if current.is_empty() {
            break;
        }
        let Ok(node) = corpus.block_chain(&current) else {
            break;
        };
        let next = node.extends.clone();
        chain.push(node);
        match next {
            Some(target) => current = target,
            None => break,
        }
    }
    // `block_chain` returns the leaf first; the reference's merge is root-most
    // first, so reverse it.
    chain.reverse();

    // Named slots keep their root-most position and are overridden (in place)
    // by a derived block that authors its own `extends`.
    let mut named: Vec<(String, CreatureManipulator)> = Vec::new();
    let mut anonymous: Vec<CreatureManipulator> = Vec::new();
    for node in &chain {
        let Some(manipulators) = node
            .children
            .iter()
            .find(|child| child.name.eq_ignore_ascii_case("Manipulators"))
        else {
            continue;
        };
        // A `Manipulators extends <block>` node inherits its base block's
        // slots just like the creature chain does (e.g. `UnitStock`'s
        // `Manipulators`). Walk that chain root-most first, then process the
        // node's own block so its slots override the inherited ones.
        let mut blocks = vec![manipulators.clone()];
        let mut base = manipulators.extends.clone();
        for _ in 0..EXTENDS_HOP_LIMIT {
            let Some(target) = base else {
                break;
            };
            let Ok(inherited) = corpus.block_chain(&target) else {
                break;
            };
            base = inherited.extends.clone();
            blocks.push(inherited.as_ref().clone());
        }
        blocks.reverse();
        for block in &blocks {
            collect_manipulators(corpus, block, &mut named, &mut anonymous);
        }
    }

    let mut entries: Vec<CreatureManipulator> = named.into_iter().map(|(_, entry)| entry).collect();
    entries.extend(anonymous);
    // First row wins per gc_type, like the reference reader.
    let mut seen = Vec::new();
    entries.retain(|entry| {
        let key = entry.gc_type.to_ascii_lowercase();
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });

    // Skills first, weapons last (the live-proven order).
    let (mut skills, weapons): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|entry| entry.kind == ManipulatorKind::Skill);
    skills.extend(weapons);

    if skills.is_empty() {
        skills.push(CreatureManipulator::default_melee());
    }
    skills
}

/// Merge one authored `Manipulators` block's emittable rows into the slot
/// lists. A named slot is overridden in place by the most-derived block that
/// authors its own `extends`; anonymous slots (`* extends <gc>`) accumulate.
fn collect_manipulators(
    corpus: &mut Corpus,
    block: &Node,
    named: &mut Vec<(String, CreatureManipulator)>,
    anonymous: &mut Vec<CreatureManipulator>,
) {
    for child in &block.children {
        let Some(gc) = child.extends.clone() else {
            continue;
        };
        let slot = child.name.to_ascii_lowercase();
        let is_anonymous = slot == "<anonymous>";
        let Some(kind) = kind_of(corpus, &gc, &slot) else {
            continue;
        };
        let entry = CreatureManipulator {
            gc_type: gc,
            id: child
                .prop("ID")
                .and_then(|value| value.trim().parse::<u32>().ok())
                .unwrap_or(0),
            kind,
        };
        if is_anonymous {
            anonymous.push(entry);
        } else if let Some(existing) = named.iter_mut().find(|(name, _)| *name == slot) {
            existing.1 = entry;
        } else {
            named.push((slot, entry));
        }
    }
}

/// The concrete `MonsterBehavior2` subclass a spawn should use.
///
/// `authored` is the creature's own `Behavior` class (the first concrete
/// `extends` along the spawn entity's chain). The client must instantiate a
/// concrete `creatures.base.behavior.*` brain: a ranged weapon with the Melee
/// brain (or a skill-only caster with it) never fires and only runs at the
/// player. The reference `monsters.spawn_monster` sanitises exactly this way,
/// matching a non-concrete/bare `MonsterBehavior2` to the primary weapon kind:
///
/// * a ranged weapon → [`CREATURE_BEHAVIOR_RANGED`];
/// * no weapon at all → [`CREATURE_BEHAVIOR_CASTER`];
/// * otherwise → [`CREATURE_BEHAVIOR_MELEE`].
///
/// The result is lower-cased, the wire form the reference's `write_gc_type`
/// produces for every non-entity classname.
pub fn mob_behavior_class(authored: Option<&str>, entries: &[CreatureManipulator]) -> String {
    if let Some(authored) = authored {
        let lower = authored.trim().to_ascii_lowercase();
        if lower.starts_with("creatures.base.behavior.") && lower.len() > 25 {
            return lower;
        }
    }
    let weapons: Vec<&CreatureManipulator> = entries
        .iter()
        .filter(|entry| entry.kind != ManipulatorKind::Skill)
        .collect();
    match weapons.first().map(|entry| entry.kind) {
        Some(ManipulatorKind::Ranged) => CREATURE_BEHAVIOR_RANGED.to_string(),
        None => CREATURE_BEHAVIOR_CASTER.to_string(),
        _ => CREATURE_BEHAVIOR_MELEE.to_string(),
    }
}

/// The wire kind for one authored row, or `None` to skip it.
///
/// Driven by the gc's native root class. A weapon whose extends chain cannot be
/// resolved falls back to the authored `Range` stat (melee 6-8, ranged 75-90).
fn kind_of(corpus: &mut Corpus, gc: &str, slot: &str) -> Option<ManipulatorKind> {
    match corpus.dfc_root(gc) {
        Ok(root) => {
            let root = root.to_ascii_lowercase();
            match root.as_str() {
                "activeskill" => Some(ManipulatorKind::Skill),
                "meleeweapon" => Some(ManipulatorKind::Melee),
                "rangedweapon" => Some(ManipulatorKind::Ranged),
                _ => None,
            }
        }
        Err(_) if slot == "primaryweapon" => {
            let range = weapon_range(corpus, gc).unwrap_or(0.0);
            Some(if range >= RANGED_RANGE_THRESHOLD {
                ManipulatorKind::Ranged
            } else {
                ManipulatorKind::Melee
            })
        }
        Err(_) => None,
    }
}

/// The first authored `Range` along a weapon's `extends` chain, if any.
fn weapon_range(corpus: &mut Corpus, gc: &str) -> Option<f64> {
    let mut current = gc.to_string();
    for _ in 0..EXTENDS_HOP_LIMIT {
        if current.is_empty() {
            break;
        }
        let Ok(node) = corpus.block_chain(&current) else {
            break;
        };
        for child in &node.children {
            if child.name.eq_ignore_ascii_case("Description") {
                if let Some(value) = child.prop("Range") {
                    if let Ok(range) = value.trim().parse::<f64>() {
                        return Some(range);
                    }
                }
            }
        }
        match node.extends.clone() {
            Some(target) => current = target,
            None => break,
        }
    }
    None
}
