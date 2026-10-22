// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-content` against a real `game.pkg`.
//!
//! Content tests skip when the archive is unavailable. Set `DRF_GAME_PKI` and
//! `DRF_GAME_PKG` to test a specific installation.

use std::path::{Path, PathBuf};

use drf_content::{
    derive_class_fqns, derive_class_items, derive_class_kit, derive_class_visuals, Archive, Corpus,
    CLASS_BASES,
};

fn pkg_dir() -> Option<PathBuf> {
    if let (Ok(pki), Ok(pkg)) = (std::env::var("DRF_GAME_PKI"), std::env::var("DRF_GAME_PKG")) {
        let pki = PathBuf::from(pki);
        let pkg = PathBuf::from(pkg);
        if pki.is_file() && pkg.is_file() {
            return pkg.parent().map(Path::to_path_buf);
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.ancestors().nth(4)?;
    [
        repo.join("dr-client-666-testing"),
        repo.join("Dungeon_Runners_Client_666"),
    ]
    .into_iter()
    .find(|dir| dir.join("game.pkg").is_file() && dir.join("game.pki").is_file())
}

fn corpus() -> Option<Corpus> {
    let dir = pkg_dir()?;
    let archive = Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).ok()?;
    Some(Corpus::new(archive))
}

#[test]
fn archive_indexes_the_client_content() {
    let Some(dir) = pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let archive =
        Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("open archive");
    assert!(archive.entries().len() > 30_000);
    assert!(archive.find("avatar/classes/WarlockFemale.gc").is_some());
    // Archive names carry no extension; the kind supplies it.
    assert!(archive.find("avatar\\classes\\WarlockFemale").is_some());
}

#[test]
fn every_class_kit_is_isomorphic_and_resolves_its_dfc_roots() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    for base in CLASS_BASES {
        let kit = derive_class_kit(&mut corpus, base).expect("derive kit");
        assert_eq!(kit.skill_fqns.len(), 4, "{base} skills");
        assert_eq!(kit.item_fqns.len(), 4, "{base} items");
        assert_eq!(kit.skill_ids, vec![105, 100, 108, 109], "{base} skill ids");
        assert_eq!(kit.skill_levels, vec![1, 1, 1, 1]);
        for dfc in &kit.skill_dfc {
            assert!(
                dfc == "ActiveSkill" || dfc == "PassiveSkill",
                "{base} skill root {dfc:?}"
            );
        }
        for dfc in &kit.item_dfc {
            assert!(
                dfc == "MeleeWeapon" || dfc == "RangedWeapon" || dfc == "Armor",
                "{base} item root {dfc:?}"
            );
        }
    }
}

#[test]
fn class_fqns_are_the_six_shipped_classes() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let fqns = derive_class_fqns(&mut corpus).expect("derive class fqns");
    assert_eq!(fqns.len(), 6);
    // The graph writes these hashes as the Avatar archetype; they are pinned
    // here so a corpus swap cannot slip a different class in.
    assert_eq!(fqns["avatar.classes.FighterMale"].hash, 0x74F0_57D6);
    assert_eq!(fqns["avatar.classes.WarlockFemale"].hash, 0xB16B_B16B);
    assert_eq!(fqns["avatar.classes.RangerMale"].hash, 0xD4F3_75CC);
}

#[test]
fn visuals_are_per_sex_not_per_class() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    for (fqn, want) in [
        (
            "avatar.classes.FighterMale",
            "avatar.races.humanmale.HumanMaleVisuals",
        ),
        (
            "avatar.classes.WarlockFemale",
            "avatar.races.humanfemale.HumanFemaleVisuals",
        ),
    ] {
        assert_eq!(derive_class_visuals(&mut corpus, fqn).unwrap(), want);
    }
}

#[test]
fn class_items_carry_slot_level_and_modifier_count() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let items = derive_class_items(&mut corpus, "Warlock").expect("warlock items");
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].slot, 10);
    assert_eq!(items[0].dfc, "MeleeWeapon");
    assert_eq!(items[0].level, 1);
    for item in &items {
        assert_eq!(item.level, 1);
    }
}
