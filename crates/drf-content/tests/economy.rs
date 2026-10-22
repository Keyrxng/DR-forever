// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-content`'s derived price against the real `game.pkg`.
//!
//! Every test skips when the archive is absent, as the sibling content tests
//! do. The pinned numbers are fixed test data.
//!
//! Output: the book prices at 1 (QUEST, 1x1) and the Minor Health Potion at 25
//! for a level-1 buyer and 85 at level 10 (NORMAL, 1x1). `knobs()` is
//! `buy=256 sell=51 gold/level=12800`, `levelDelta=[0,-12,-10,-7,-5,-2,0,3]`,
//! `priceMod=[33,66,143,273,519,1169,256,9960]`.

use std::path::{Path, PathBuf};

use drf_content::{economy, Archive, Corpus};

const VENDOR: &str = "world.tutorial.npc.HermitVendor";
const BOOK: &str = "QuestItemPAL2.D00_Q11_a2_01";
const POTION: &str = "items.consumables.Consumable_MinorHealthPotion";

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

#[test]
fn knobs_are_the_pinned_global_knobs() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let knobs = economy::knobs(&mut corpus).expect("GlobalKnobs.gc");
    assert_eq!(knobs.item_buy_value_modifier, 256);
    assert_eq!(knobs.item_sell_value_modifier, 51);
    assert_eq!(knobs.item_gold_value_per_level, 12800);
    assert_eq!(knobs.item_level_delta, [0, -12, -10, -7, -5, -2, 0, 3]);
    assert_eq!(
        knobs.item_price_modifier,
        [33, 66, 143, 273, 519, 1169, 256, 9960]
    );
}

#[test]
fn hermit_vendor_book_and_potion_prices_match_the_reference() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let book = economy::price(&mut corpus, VENDOR, BOOK, 0, 1, &[], Some(1)).expect("book price");
    assert_eq!(book.gold, 1);
    assert_eq!(book.quality_name, "QUEST");
    assert_eq!(book.footprint, "1x1");
    assert_eq!(book.buy_mod, 256);
    assert_eq!((book.width, book.height), (1, 1));

    let potion_l1 =
        economy::price(&mut corpus, VENDOR, POTION, 0, 1, &[], Some(1)).expect("potion price");
    assert_eq!(potion_l1.gold, 25, "the UselessToMerchant floor");
    assert_eq!(potion_l1.quality_name, "NORMAL");
    assert_eq!(potion_l1.footprint, "1x1");

    let potion_l10 =
        economy::price(&mut corpus, VENDOR, POTION, 0, 1, &[], Some(10)).expect("potion price");
    assert_eq!(potion_l10.gold, 85, "the potion scales to the buyer");
}

#[test]
fn hermit_vendor_profile_uses_the_constructor_default() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let profile = economy::merchant_profile(&mut corpus, VENDOR).expect("profile");
    assert_eq!(profile.buy_mod, 256);
    assert_eq!(profile.sell_mod, 256);
    assert_eq!(profile.level, 1);
    assert!(!profile.buy_mod_authored, "the .gc states no BuyValueMod");
    // The component spelling resolves the same block.
    let component =
        economy::merchant_profile(&mut corpus, &format!("{VENDOR}.Merchant")).expect("profile");
    assert_eq!(component, profile);
}

#[test]
fn an_unknown_item_class_is_a_named_blocker() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let err = economy::price(
        &mut corpus,
        VENDOR,
        "no.such.Item.Class",
        0,
        1,
        &[],
        Some(1),
    )
    .expect_err("unknown class");
    assert!(
        matches!(err, drf_content::ContentError::Blocked(_)),
        "got {err}"
    );
}
