// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-creature `Manipulators` resolution against the real `game.pkg`.
//!
//! Pins the native-root selection and the fallback for the tutorial dungeon's
//! Warg Pup (whose authored `PrimaryWeapon` block inherits the generic melee
//! weapon through `creatures.base.UnitMelee`), a ranged Whisker, a skill plus
//! weapon melee, and a caster boss. Skips when the archive is absent, like the
//! sibling content tests.

use std::path::{Path, PathBuf};

use drf_content::{
    creature_manipulators, Archive, Corpus, CreatureManipulator, ManipulatorKind,
    DEFAULT_CREATURE_MELEE_GC, DEFAULT_CREATURE_MELEE_ID,
};

fn pkg_paths() -> Option<(PathBuf, PathBuf)> {
    if let (Ok(pkg), Ok(pki)) = (std::env::var("DRF_GAME_PKG"), std::env::var("DRF_GAME_PKI")) {
        let pkg = PathBuf::from(pkg);
        let pki = PathBuf::from(pki);
        if pkg.is_file() && pki.is_file() {
            return Some((pki, pkg));
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.ancestors().nth(4)?;
    for dir in [
        repo.join("dr-client-666-testing"),
        repo.join("Dungeon_Runners_Client_666"),
        repo.join("dr-play"),
    ] {
        if dir.join("game.pkg").is_file() && dir.join("game.pki").is_file() {
            return Some((dir.join("game.pki"), dir.join("game.pkg")));
        }
    }
    None
}

fn corpus() -> Option<Corpus> {
    let (pki, pkg) = pkg_paths()?;
    Some(Corpus::new(
        Archive::open(&pki, &pkg).expect("open game.pki/game.pkg"),
    ))
}

fn kind_kinds(entries: &[CreatureManipulator]) -> Vec<ManipulatorKind> {
    entries.iter().map(|entry| entry.kind).collect()
}

#[test]
fn a_plain_melee_unit_resolves_the_generic_base_melee_weapon() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // Warg Pup: its `PrimaryWeapon` block authors only a Description and
    // inherits `extends creatures.base.weapons.melee` from UnitMelee.
    let entries = creature_manipulators(&mut corpus, "world.dungeon00.mob.melee01.rank1");
    assert_eq!(
        entries,
        vec![CreatureManipulator {
            gc_type: DEFAULT_CREATURE_MELEE_GC.to_string(),
            id: DEFAULT_CREATURE_MELEE_ID,
            kind: ManipulatorKind::Melee,
        }]
    );
}

#[test]
fn a_creature_with_its_own_weapon_ships_that_weapon_not_the_generic_one() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // Whisker Ratling (`melee03`) authors `Whisker_Broodling_Weapons.UnarmedWeapon`.
    let entries = creature_manipulators(&mut corpus, "world.dungeon00.mob.melee03.rank1");
    assert_eq!(
        entries,
        vec![CreatureManipulator {
            gc_type: "creatures.whiskers.broodling.base.Whisker_Broodling_Weapons.UnarmedWeapon"
                .to_string(),
            id: 10,
            kind: ManipulatorKind::Melee,
        }]
    );
}

#[test]
fn a_ranged_creature_resolves_a_ranged_weapon() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entries = creature_manipulators(
        &mut corpus,
        "creatures.whiskers.broodling.Basic.GruntRanged",
    );
    assert_eq!(kind_kinds(&entries), vec![ManipulatorKind::Ranged]);
    assert_eq!(
        entries[0].gc_type,
        "creatures.whiskers.broodling.base.Whisker_Broodling_Weapons.RangedRockWeapon"
    );
    assert_eq!(entries[0].id, 10);
}

#[test]
fn skills_are_emitted_before_weapons() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // melee02 inherits `skills.creature.CreatureRend` and the generic melee.
    let entries = creature_manipulators(&mut corpus, "world.dungeon00.mob.melee02.rank1");
    assert_eq!(
        kind_kinds(&entries),
        vec![ManipulatorKind::Skill, ManipulatorKind::Melee]
    );
    assert_eq!(entries[0].gc_type, "skills.creature.CreatureRend");
    assert_eq!(entries[1].gc_type, DEFAULT_CREATURE_MELEE_GC);
}

#[test]
fn a_caster_boss_ships_its_skill_blocks() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entries = creature_manipulators(&mut corpus, "world.dungeon01.mob.boss");
    assert!(!entries.is_empty(), "boss authors skills");
    assert!(entries
        .iter()
        .all(|entry| entry.kind == ManipulatorKind::Skill));
    // Its three authored anonymous skills are all present (not collapsed).
    assert!(entries
        .iter()
        .any(|entry| entry.gc_type == "skills.creature.CreatureBoltPoisonBoss"));
    assert!(entries
        .iter()
        .any(|entry| entry.gc_type
            == "creatures.whiskers.shaman.skills.boss.SummonPoisonBroodlings01"));
    assert!(entries
        .iter()
        .any(|entry| entry.gc_type == "skills.creature.CreatureDebuffPoison"));
}

#[test]
fn an_unresolvable_creature_falls_back_to_the_generic_melee_weapon() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entries = creature_manipulators(&mut corpus, "world.nope.mob.does_not_exist.rank1");
    assert_eq!(entries, vec![CreatureManipulator::default_melee()]);
}

#[test]
fn the_brain_is_sanitised_to_the_primary_weapon_kind() {
    use drf_content::{
        mob_behavior_class, ManipulatorKind, CREATURE_BEHAVIOR_CASTER, CREATURE_BEHAVIOR_MELEE,
        CREATURE_BEHAVIOR_RANGED,
    };

    let melee = vec![CreatureManipulator {
        gc_type: "creatures.base.weapons.melee".into(),
        id: 10,
        kind: ManipulatorKind::Melee,
    }];
    let ranged = vec![CreatureManipulator {
        gc_type: "creatures.mutants.puker.base.weapon.Fire".into(),
        id: 10,
        kind: ManipulatorKind::Ranged,
    }];
    let skill_only = vec![CreatureManipulator {
        gc_type: "skills.creature.CreatureBolt".into(),
        id: 0,
        kind: ManipulatorKind::Skill,
    }];

    // A concrete authored brain is preserved (lower-cased to the wire form).
    assert_eq!(
        mob_behavior_class(Some("creatures.base.behavior.Caster"), &melee),
        CREATURE_BEHAVIOR_CASTER
    );
    // A bare/native brain is not loadable; fall back by weapon kind.
    assert_eq!(
        mob_behavior_class(Some("base.MonsterBehavior"), &melee),
        CREATURE_BEHAVIOR_MELEE
    );
    assert_eq!(mob_behavior_class(None, &ranged), CREATURE_BEHAVIOR_RANGED);
    assert_eq!(
        mob_behavior_class(None, &skill_only),
        CREATURE_BEHAVIOR_CASTER
    );
}
