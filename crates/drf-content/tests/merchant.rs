// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-content`'s derived `Merchant` block against `game.pkg`.
//!
//! The vendor and its tabs are `game.pkg` facts; without the archive every test
//! here skips, as the sibling content tests do.

use std::path::{Path, PathBuf};

use drf_content::{authors_merchant, derive_merchant_tabs, fill_generated_tabs, Archive, Corpus};

const VENDOR: &str = "world.tutorial.npc.HermitVendor";
const NON_VENDOR: &str = "world.tutorial.npc.Guard";

fn corpus() -> Option<Corpus> {
    let (pki, pkg) = pkg_paths()?;
    let archive = Archive::open(&pki, &pkg).expect("open game.pki/game.pkg");
    Some(Corpus::new(archive))
}

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

#[test]
fn hermit_vendor_authors_three_tabs() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    assert!(authors_merchant(&mut corpus, VENDOR));
    let tabs = derive_merchant_tabs(&mut corpus, VENDOR)
        .expect("derive")
        .expect("HermitVendor authors a Merchant block");
    let names: Vec<&str> = tabs.iter().map(|tab| tab.name.as_str()).collect();
    assert_eq!(names, ["Misc", "Weapons", "QuestItems"]);
    let ids: Vec<u8> = tabs.iter().map(|tab| tab.id).collect();
    assert_eq!(ids, [1, 2, 3]);
    let statics: Vec<bool> = tabs.iter().map(|tab| tab.static_contents).collect();
    assert_eq!(statics, [true, false, true]);
    assert_eq!(tabs[0].items.len(), 8);
    assert!(tabs[1].items.is_empty());
    assert_eq!(tabs[2].items.len(), 1);
    assert_eq!(tabs[2].items[0].class, "QuestItemPAL2.D00_Q11_a2_01");
    // The component classname spelling resolves to the same block.
    assert!(authors_merchant(
        &mut corpus,
        "world.tutorial.npc.HermitVendor.Merchant"
    ));
}

#[test]
fn a_non_vendor_authors_no_merchant_block() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    assert!(!authors_merchant(&mut corpus, NON_VENDOR));
    assert_eq!(
        derive_merchant_tabs(&mut corpus, NON_VENDOR).expect("derive"),
        None
    );
}

/// Draykop's live elite01 copy has four `AutoGenerateItems` tabs and no
/// authored stock. Each tab must be filled from `MerchantSpecialEvent01IG`;
/// its `RareMerchantIG`/`UniqueMerchantIG` overrides inherit the concrete
/// item rows from `RareIG`/`UniqueIG`.
#[test]
fn amazon_vendor_generated_tabs_have_stock() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let npc = "world.elite01.npc.Amazon1";
    let mut tabs = derive_merchant_tabs(&mut corpus, npc)
        .expect("derive")
        .expect("Amazon1 authors a Merchant block");
    assert_eq!(tabs.len(), 4);
    fill_generated_tabs(&mut corpus, npc, &mut tabs);
    for tab in &tabs {
        assert!(
            !tab.items.is_empty(),
            "{} tab {:?} has no stock",
            npc,
            tab.name
        );
        assert!(tab.items.len() <= 8);
        for item in &tab.items {
            assert!(!item.class.is_empty());
            assert!(item.level <= tab.max_item_level.unwrap_or(u8::MAX));
        }
    }
}
