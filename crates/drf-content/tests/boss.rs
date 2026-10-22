// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::path::{Path, PathBuf};

use drf_content::{nci_item_generators, spawns, Archive};

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

#[test]
fn rattle_tooth_lair_spawns_the_boss_and_posse_from_content() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let mut spawns = spawns(&archive, "dungeon00_level03_boss").expect("spawns");
    spawns.sort_by(|a, b| a.classname.cmp(&b.classname));
    let classes: Vec<&str> = spawns
        .iter()
        .map(|spawn| spawn.classname.as_str())
        .collect();
    for want in [
        "world.dungeon00.mob.boss_posse.RattleTooth",
        "world.dungeon00.mob.boss_posse.Blademaster",
        "world.dungeon00.mob.boss_posse.Broodling",
        "world.dungeon00.mob.boss_posse.boss_guard",
    ] {
        assert!(classes.contains(&want), "missing {want}: {classes:?}");
    }
    assert!(
        spawns.iter().all(|spawn| spawn.hp_wire > 0),
        "every boss-arena spawn must have a live HP word"
    );
    let boss = spawns
        .iter()
        .find(|spawn| spawn.classname.ends_with(".RattleTooth"))
        .expect("boss");
    eprintln!("RattleTooth {:?}", boss);
    assert!(boss.level > 0, "boss level");
}

#[test]
fn boss_chest_authors_its_two_generators() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    for class in [
        "terrain.interactives.loot.BossChest",
        "terrain.interactives.loot.Chest_Boss",
    ] {
        let slots = nci_item_generators(&archive, class).expect("boss chest generators");
        assert_eq!(
            slots,
            vec![
                ("TreasureChestBossIG".to_string(), 1),
                ("DefaultIG".to_string(), 4)
            ],
            "{class}'s authored IG/count pairs"
        );
    }
}

#[test]
fn rattle_tooth_lair_places_gate_chests_portals_and_no_placeholders() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let entities = drf_content::derive_world_entities(&archive, "dungeon00_level03_boss")
        .expect("world entities");
    let classes: Vec<&str> = entities
        .iter()
        .map(|entity| entity.classname.as_str())
        .collect();
    for want in [
        "world.dungeon00.data.BossGate",
        "terrain.interactives.loot.BossChest",
        "terrain.interactives.loot.Chest_Boss",
        "world.dungeon00.data.teleport.toTutorial",
        "world.dungeon00.data.NewbieEnduranceShrine",
    ] {
        assert!(classes.contains(&want), "missing {want}: {classes:?}");
    }
    assert!(
        !classes.iter().any(|class| class.contains("BossFightNCI01")),
        "placeholder NCIs must not be emitted as real entities"
    );
}

#[test]
fn boss_gate_is_a_door_and_exit_nci_states_its_teleport() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let entities = drf_content::derive_world_entities(&archive, "dungeon00_level03_boss")
        .expect("world entities");
    let gate = entities
        .iter()
        .find(|entity| entity.classname == "world.dungeon00.data.BossGate")
        .expect("gate");
    assert_eq!(gate.kind, drf_content::PlacedKind::Door);
    let exit = entities
        .iter()
        .find(|entity| entity.classname == "world.dungeon00.data.teleport.toTutorial")
        .expect("exit");
    assert_eq!(exit.kind, drf_content::PlacedKind::WorldEntityUnit);
}

#[test]
fn rattle_tooth_exit_nci_states_tutorial_start() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    assert_eq!(
        drf_content::nci_teleport_destination(&archive, "world.dungeon00.data.teleport.toTutorial")
            .expect("destination"),
        Some(("tutorial".to_string(), "start".to_string()))
    );
}

#[test]
fn crossbow_item_body_shape_is_safe() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let mut corpus = drf_content::Corpus::new(archive);
    let class = "2HCrossbow1PAL.2HCrossbow1-1";
    eprintln!("slots={:?}", corpus.item_readdata_slots(class));
    eprintln!(
        "profile={:?}",
        drf_content::item_profile(&mut corpus, class)
    );
    eprintln!("dfc={:?}", corpus.dfc_root(class));
}

#[test]
fn dew_wolf_states_item_and_gold_treasure_slots() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let mut corpus = drf_content::Corpus::new(archive);
    eprintln!(
        "{:?}",
        drf_content::treasure_slots(&mut corpus, "world.dungeon00.mob.melee02.rank1")
    );
}

#[test]
fn dew_wolf_loot_includes_currency_and_item() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let mut corpus = drf_content::Corpus::new(archive);
    let slots =
        drf_content::treasure_slots(&mut corpus, "world.dungeon00.mob.melee02.rank1").unwrap();
    let mut rng = drf_content::MersenneTwister::new(42);
    let mut saw_currency = false;
    let mut saw_item = false;
    for _ in 0..64 {
        let items = drf_content::generate_slots(&mut corpus, &slots, 2, &mut rng, false);
        saw_currency |= items.iter().any(|item| item.amount.is_some());
        saw_item |= items.iter().any(|item| item.amount.is_none());
    }
    assert!(saw_currency, "DefaultGG never produced currency");
    assert!(saw_item, "DefaultIG never produced an item");
}

#[test]
fn default_gg_draws_currency() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let mut corpus = drf_content::Corpus::new(archive);
    let mut rng = drf_content::MersenneTwister::new(1);
    let items = drf_content::generate_loot(&mut corpus, "DefaultGG", 2, 3, &mut rng, false);
    eprintln!("{items:?}");
    assert!(!items.is_empty());
}

#[test]
fn staff_item_body_shape() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let mut corpus = drf_content::Corpus::new(archive);
    let class = "1HStaff1Pal.1HStaff1-1";
    eprintln!(
        "slots={:?} profile={:?} dfc={:?}",
        corpus.item_readdata_slots(class),
        drf_content::item_profile(&mut corpus, class),
        corpus.dfc_root(class)
    );
}

#[test]
fn warlock_starting_staff_shape() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let mut corpus = drf_content::Corpus::new(archive);
    let items = drf_content::derive_class_items(&mut corpus, "Warlock").expect("kit");
    for item in items {
        if item.slot == 10 {
            eprintln!("{item:?}");
        }
    }
    let class = "1HStaff1Pal.1HStaff1-1";
    eprintln!(
        "profile={:?} slots={:?}",
        drf_content::item_profile(&mut corpus, class),
        corpus.item_readdata_slots(class)
    );
}

#[test]
fn starting_staff_alias_canonicalizes_and_gets_melee_status() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let mut corpus = drf_content::Corpus::new(archive);
    assert_eq!(
        corpus
            .canonical_class_name("1HStaff1Pal.1HStaff1-1")
            .as_deref(),
        Some("1HStaff1PAL.1HStaff1-1")
    );
    assert_eq!(
        corpus
            .canonical_class_name("2HCrossbow1PAL.2HCrossbow1-1")
            .as_deref(),
        Some("2HCrossbow1PAL.2HCrossbow1-1")
    );
    assert_eq!(
        corpus
            .item_readdata_slots("1HStaff1Pal.1HStaff1-1")
            .unwrap(),
        0
    );
    assert_eq!(
        corpus
            .item_readdata_slots("2HCrossbow1PAL.2HCrossbow1-1")
            .unwrap(),
        0
    );
}

#[test]
fn boss_death_names_the_gate_class_name() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let boss_desc = drf_content::class_description_property(
        &archive,
        "world.dungeon00.mob.boss",
        "DoorsToOpenOnDeath",
    )
    .expect("boss property");
    assert_eq!(boss_desc.as_deref(), Some("Boss00ExitGate"));
    let gate_name =
        drf_content::class_description_property(&archive, "world.dungeon00.data.BossGate", "Name")
            .expect("gate property");
    assert_eq!(gate_name.as_deref(), Some("Boss00ExitGate"));
}

#[test]
fn kill_objective_parses_the_monster_type_list() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let mut corpus = drf_content::Corpus::new(archive);
    let quests = drf_content::QuestCorpus::from_corpus(&mut corpus).expect("quest corpus");
    let quest = quests
        .quest_by_fqn("world.dungeon00.quest.Q03_a2")
        .expect("Q03_a2");
    let wolves = &quest.objectives[0];
    assert_eq!(wolves.kind, "KillObjective");
    assert_eq!(wolves.required, 6);
    assert_eq!(
        wolves.monsters,
        vec![
            "world.dungeon00.mob.melee02.rank1".to_string(),
            "world.dungeon00.mob.melee02.rank2".to_string(),
            "world.dungeon00.mob.melee02.rank3".to_string(),
        ]
    );
    assert!(wolves.matches_monster("world.dungeon00.mob.melee02.rank2"));
    assert!(!wolves.matches_monster("world.dungeon00.mob.melee01.rank1"));
}

#[test]
fn looted_equipment_slot_counts() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let mut corpus = drf_content::Corpus::new(archive);
    for c in [
        "ScaleBoots2PAL.ScaleBoots2-1",
        "ScaleHelm1PAL.ScaleHelm1-1",
        "2HAxe1PAL.2HAxe1-1",
        "1HStaff1Pal.1HStaff1-1",
    ] {
        eprintln!(
            "{c}: slots={:?} dfc={:?}",
            corpus.item_readdata_slots(c),
            corpus.dfc_root(c)
        );
    }
}

#[test]
fn rattle_tooth_is_the_authored_boss_base_class() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    assert!(drf_content::class_is_kind_of(
        &archive,
        "world.dungeon00.mob.boss_posse.RattleTooth",
        "world.dungeon00.mob.boss",
    )
    .expect("class chain"));
}

#[test]
fn rattle_tooth_names_the_exit_gate_it_opens() {
    let Some((pki, pkg)) = pkg_paths() else {
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    assert_eq!(
        drf_content::class_description_property(
            &archive,
            "world.dungeon00.mob.boss_posse.RattleTooth",
            "DoorsToOpenOnDeath"
        )
        .expect("boss description"),
        Some("Boss00ExitGate".to_string())
    );
    assert_eq!(
        drf_content::class_description_property(&archive, "world.dungeon00.data.BossGate", "Name")
            .expect("gate class"),
        Some("Boss00ExitGate".to_string())
    );
}
