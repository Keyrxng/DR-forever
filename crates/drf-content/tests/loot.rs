// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::path::{Path, PathBuf};

use drf_content::{generate_loot, treasure_slots, Archive, Corpus, MersenneTwister};

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

fn corpus() -> Option<Corpus> {
    let (pki, pkg) = pkg_paths()?;
    Some(Corpus::new(
        Archive::open(&pki, &pkg).expect("open game.pki/game.pkg"),
    ))
}

#[test]
fn unit_treasure_slots_are_content_derived() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let slots = treasure_slots(&mut corpus, "base.MeleeUnit").expect("slots");
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0].generator, "DefaultIG");
    assert_eq!(slots[0].count, 1);
    assert_eq!(slots[1].generator, "DefaultGG");
    assert_eq!(slots[1].count, 1);
}

#[test]
fn table_rows_are_banded_and_use_the_flat_pal_classes() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let mut rng = MersenneTwister::new(0x1234_5678);
    let items = generate_loot(
        &mut corpus,
        "NormalLeatherArmor3IG",
        15,
        32,
        &mut rng,
        false,
    );
    assert!(!items.is_empty(), "the level-15 row must produce items");
    for item in &items {
        assert_eq!(item.class, "LeatherArmor3PAL.LeatherArmor3-2");
        assert!((15..=22).contains(&item.level), "level = {}", item.level);
        assert!(item.modifiers.is_empty());
    }
}

#[test]
fn weapons_roll_their_level_prefix_modifier() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let mut rng = MersenneTwister::new(7);
    let mut saw_weapon = false;
    for _ in 0..16 {
        let item = generate_loot(&mut corpus, "Normal1HWeaponIG", 15, 1, &mut rng, false);
        for item in item {
            if item.class.to_ascii_lowercase().contains("sword")
                || item.class.to_ascii_lowercase().contains("axe")
                || item.class.to_ascii_lowercase().contains("mace")
                || item.class.to_ascii_lowercase().contains("staff")
                || item.class.to_ascii_lowercase().contains("pick")
            {
                saw_weapon = true;
                assert_eq!(item.modifiers.len(), 1, "{item:?}");
                assert!(item.modifiers[0]
                    .class
                    .to_ascii_lowercase()
                    .contains("levelprefix"));
                assert_eq!(item.modifiers[0].level, 15);
            }
        }
    }
    assert!(saw_weapon, "no weapon generated");
}

#[test]
fn chest_table_generates_items_and_native_currency() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let mut rng = MersenneTwister::new(99);
    let items = generate_loot(
        &mut corpus,
        "TreasureChestSmallIG",
        15,
        200,
        &mut rng,
        false,
    );
    assert!(!items.is_empty());
    assert!(
        items.iter().any(|item| item.is_currency()),
        "no Currency row won"
    );
    assert!(
        items.iter().any(|item| !item.is_currency()),
        "no item row won"
    );
    for item in items.iter().filter(|item| item.is_currency()) {
        assert!(item.amount.unwrap_or(0) > 0);
        assert_eq!(item.class, "Currency");
    }
}

#[test]
fn generated_weapon_record_has_no_native_status_byte_and_has_rolled_mods() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let class = "items.pal.1HSwordPAL.Normal001";
    assert_eq!(
        corpus.item_readdata_slots(class).expect("weapon slots"),
        0,
        "the native weapon readState is not part of Item::readData"
    );
    let record = drf_content::author_item_record_with_mods(
        &mut corpus,
        class,
        7,
        1,
        Some(15),
        (0, 0),
        &[(
            "items.modpal.LevelPrefixModPAL.XLight01.Mod2".to_string(),
            15,
        )],
    )
    .expect("record")
    .expect("class exists");
    let body = &record.record[1 + class.len() + 1..];
    assert_eq!(&body[..4], &7u32.to_le_bytes());
    assert_eq!(body[8], 0, "no class-authored weapon status byte");
    assert_eq!(body[9], 1, "one rolled modifier");
    assert!(body[10..].starts_with(b"\xffitems.modpal.LevelPrefixModPAL.XLight01.Mod2"));
}

#[test]
fn inherited_currency_rows_do_not_make_a_chest_empty() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let mut rng = MersenneTwister::new(0x5EED);
    for _ in 0..64 {
        let items = generate_loot(&mut corpus, "TreasureChestLargeIG", 5, 1, &mut rng, false);
        assert!(
            !items.is_empty(),
            "the Chance=1 group always has a usable Gold row"
        );
    }
}

#[test]
fn dungeon01_mobs_state_the_squeakeasy_passcode_slot() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let slots = treasure_slots(&mut corpus, "world.dungeon01.mob.upper.raceA.base2")
        .expect("mob treasure slots");
    assert!(
        slots
            .iter()
            .any(|slot| slot.generator == "Dungeon01_ItemIG.Entry1"),
        "the passcode slot is authored: {slots:?}"
    );
    let mut rng = MersenneTwister::new(0x1234_5678);
    let items = generate_loot(
        &mut corpus,
        "Dungeon01_ItemIG.Entry1",
        7,
        300,
        &mut rng,
        false,
    );
    assert!(
        items
            .iter()
            .any(|item| item.class == "Dungeon01ItemPAL.D01_Key_01"),
        "the passcode can drop at level 7"
    );
}

#[test]
fn prebuilt_weapons_count_their_class_modifier_status_bytes() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    // These classes author Mod1..Mod5 as ItemModifier children. Item::readData
    // reads one status byte per child before the rolled-modifier count; a zero
    // here makes the client read the count from the wrong byte and latch CEM
    // code 2 (`ReadChildData` at index 65502+).
    assert_eq!(
        corpus
            .item_readdata_slots("items.pal.1HStaffPAL.MythicPreBuilt004")
            .expect("prebuilt 004"),
        5
    );
    assert_eq!(
        corpus
            .item_readdata_slots("items.pal.1HStaffPAL.MythicPreBuilt003")
            .expect("prebuilt 003"),
        6
    );
}

#[test]
fn token_reward_generator_keeps_rare_base_and_item_modifiers() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let mut rng = MersenneTwister::new(7);
    let items = generate_loot(
        &mut corpus,
        "TokenRewardRareMageIG.Armor",
        11,
        1,
        &mut rng,
        false,
    );
    let item = items
        .first()
        .expect("token reward rolls a rare-or-better item");
    assert!(
        item.class.contains("MageBodyPAL.Rare"),
        "the fallback row is a Rare mage body: {}",
        item.class
    );
    assert!(
        !item.modifiers.is_empty(),
        "RareMageBodyIG authors ItemModGenerator1..5; dropping them renders the item gray"
    );
}

#[test]
fn token_reward_alias_resolves_amulet_and_ring() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    for (generator, expected) in [
        ("TokenRewardRareMageIG.Amulet", "AmuletPAL."),
        ("TokenRewardRareMageIG.Ring", "RingPAL."),
    ] {
        let mut rng = MersenneTwister::new(7);
        let items = generate_loot(&mut corpus, generator, 11, 1, &mut rng, false);
        let item = items
            .first()
            .unwrap_or_else(|| panic!("{generator} must resolve its LinkedGenerator alias"));
        assert!(
            item.class.starts_with(expected),
            "{generator} resolved to {}",
            item.class
        );
        assert!(
            !item.modifiers.is_empty(),
            "{generator} must keep its ItemModGenerator children"
        );
    }
}

#[test]
fn wishing_well_generators_do_not_return_empty_rolls() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    for generator in ["WishingWellIG", "OneTimeUseOnlyWishingWellIG"] {
        for seed in 1..=10 {
            let mut rng = MersenneTwister::new(seed);
            let items = generate_loot(&mut corpus, generator, 11, 1, &mut rng, false);
            assert!(
                !items.is_empty(),
                "{generator} seed {seed} returned no loot"
            );
        }
    }
}
