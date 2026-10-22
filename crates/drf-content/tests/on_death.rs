// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::path::{Path, PathBuf};

use drf_content::{
    class_is_creature, derive_world_entities, mob_profile_at_level, on_death_summons, spawns,
    spell_effect_summons, Archive,
};

fn pkg_paths() -> Option<(PathBuf, PathBuf)> {
    if let (Ok(pkg), Ok(pki)) = (env::var("DRF_GAME_PKG"), env::var("DRF_GAME_PKI")) {
        let pkg = PathBuf::from(pkg);
        let pki = PathBuf::from(pki);
        if pkg.is_file() && pki.is_file() {
            return Some((pki, pkg));
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.ancestors().nth(4)?;
    for dir in [repo.join("dr-client-666-testing"), repo.join("dr-play")] {
        if dir.join("game.pkg").is_file() && dir.join("game.pki").is_file() {
            return Some((dir.join("game.pki"), dir.join("game.pkg")));
        }
    }
    None
}

#[test]
fn q10_chest_is_a_mob_and_authors_its_death_summons() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let procs =
        on_death_summons(&archive, "world.dungeon06.mob.Q10_Chest_1").expect("on-death procs");
    assert_eq!(procs.len(), 2, "Q10_Chest_1 authors two OnDeath rows");
    let units: Vec<&str> = procs
        .iter()
        .flat_map(|proc| proc.units.iter().map(|(unit, _)| unit.as_str()))
        .collect();
    assert!(units.contains(&"world.dungeon06.mob.Q10_Green"));
    assert!(units.contains(&"world.dungeon06.mob.Q10_Blue"));
    assert_eq!(procs.iter().map(|proc| proc.chance).max(), Some(2));
    assert!(class_is_creature(&archive, "world.dungeon06.mob.Q10_Chest_1").unwrap());
    assert!(!class_is_creature(&archive, "world.tutorial.npc.Adventurer1").unwrap());
    assert!(
        mob_profile_at_level(&archive, "world.dungeon06.mob.Q10_Green", 26)
            .expect("profile")
            .is_some_and(|(hp_wire, _)| hp_wire > 0)
    );

    let spawns = spawns(&archive, "d06_l01_off1a_q10_a").expect("hand-placed spawns");
    let chests: Vec<_> = spawns
        .iter()
        .filter(|spawn| spawn.classname == "world.dungeon06.mob.Q10_Chest_1")
        .collect();
    assert_eq!(chests.len(), 13, "every hand-placed Q10 chest is a mob");
    assert!(
        chests.iter().all(|spawn| spawn.hp_wire == 256),
        "the placement's HitPoints wins over the zone curve"
    );

    let entities = derive_world_entities(&archive, "d06_l01_off1a_q10_a").expect("world entities");
    assert!(
        !entities
            .iter()
            .any(|entity| entity.classname.contains("Q10_Chest")),
        "creatures must not also become static NPCs"
    );
}

#[test]
fn guardian_relic_chest_summons_its_relic_guardian() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");

    let summons = spell_effect_summons(&archive, "world.dungeon01.data.relic.Guardian.Md01_A")
        .expect("guardian chest summons");
    assert_eq!(summons.len(), 1, "one guardian per chest");
    let (unit, offset) = &summons[0];
    assert_eq!(unit, "world.dungeon01.mob.relic.racea.base1");
    assert_eq!(*offset, 0);
    assert!(
        class_is_creature(&archive, unit).unwrap(),
        "the summoned guardian must ship as a creature"
    );
    assert!(
        mob_profile_at_level(&archive, unit, 10)
            .expect("profile")
            .is_some_and(|(hp_wire, _)| hp_wire > 0),
        "the summoned guardian needs a usable profile"
    );

    let plain = spell_effect_summons(&archive, "world.dungeon01.data.relic.Chest.Md01")
        .expect("plain chest summons");
    assert!(
        plain.is_empty(),
        "a plain relic chest only runs RelicStun; no spawn"
    );
}
