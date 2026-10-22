// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-proto`'s derived `*.Merchant` record against `game.pkg`.
//!
//! The pinned payload is fixed test data.
//!
//! Output (112 B):
//! `010a0000000000000001010100000000000100000002000000000201000000030000000004010000000400000001000100000005000000010201000000060000000104050000000700000002000a0000000800000002020a000000000201000301090000000000010000000000000000`
//!
//! Without a `game.pkg` on this machine the content-dependent test skips.

use std::path::{Path, PathBuf};

use drf_content::{derive_merchant_tabs, Archive, Corpus, MerchantItem, MerchantTab};
use drf_proto::{
    build_merchant_component_record, build_stock_payload, derived_item_ids, reserve_item_ids,
    MERCHANT_ITEM_ID_MAX, MERCHANT_ITEM_ID_MIN,
};

const VENDOR: &str = "world.tutorial.npc.HermitVendor";
const PAYLOAD_HEX: &str = "010a0000000000000001010100000000000100000002000000000201000000030000000004010000000400000001000100000005000000010201000000060000000104050000000700000002000a0000000800000002020a000000000201000301090000000000010000000000000000";

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
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

fn item(index: usize, class: &str, x: u8, y: u8, quantity: u8) -> MerchantItem {
    MerchantItem {
        index,
        class: class.to_string(),
        inventory_x: x,
        inventory_y: y,
        quantity,
        level: 0,
    }
}

#[test]
fn hermit_vendor_payload_matches_the_reference() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open game.pki/game.pkg");
    let mut corpus = Corpus::new(archive);
    let tabs = derive_merchant_tabs(&mut corpus, VENDOR)
        .expect("derive HermitVendor tabs")
        .expect("game.pkg authors a Merchant block");

    assert_eq!(tabs.len(), 3, "HermitVendor authors three tabs");
    assert_eq!((tabs[0].id, tabs[0].name.as_str()), (1, "Misc"));
    assert_eq!(tabs[0].items.len(), 8, "Misc ships eight consumables");
    assert!(tabs[0].static_contents);
    assert_eq!((tabs[1].id, tabs[1].name.as_str()), (2, "Weapons"));
    assert!(!tabs[1].static_contents, "Weapons is a generated tab");
    assert!(tabs[1].items.is_empty(), "a generated tab ships empty");
    assert_eq!((tabs[2].id, tabs[2].name.as_str()), (3, "QuestItems"));
    assert_eq!(tabs[2].items.len(), 1);
    assert_eq!(tabs[2].items[0].class, "QuestItemPAL2.D00_Q11_a2_01");
    assert_eq!(
        (tabs[2].items[0].inventory_x, tabs[2].items[0].inventory_y),
        (0, 0)
    );
    assert_eq!(tabs[2].items[0].quantity, 1);
    for (tab_index, tab) in tabs.iter().enumerate() {
        assert_eq!(tab.index, tab_index);
    }

    assert_eq!(hex(&build_stock_payload(&tabs, 1)), PAYLOAD_HEX);
}

#[test]
fn hermit_vendor_item_ids_are_minted_in_construction_order() {
    let tabs = vec![
        MerchantTab {
            index: 0,
            id: 1,
            name: "Misc".to_string(),
            static_contents: true,
            auto_generate: false,
            item_generator: None,
            min_item_level: None,
            max_item_level: None,
            items: vec![
                item(0, "items.consumables.Consumable_MinorHealthPotion", 0, 0, 1),
                item(1, "items.consumables.Consumable_MinorManaPotion", 0, 2, 1),
            ],
        },
        MerchantTab {
            index: 1,
            id: 2,
            name: "Weapons".to_string(),
            static_contents: false,
            auto_generate: true,
            item_generator: None,
            min_item_level: None,
            max_item_level: None,
            items: Vec::new(),
        },
        MerchantTab {
            index: 2,
            id: 3,
            name: "QuestItems".to_string(),
            static_contents: true,
            auto_generate: false,
            item_generator: None,
            min_item_level: None,
            max_item_level: None,
            items: vec![item(0, "QuestItemPAL2.D00_Q11_a2_01", 0, 0, 1)],
        },
    ];
    let (ids, next_id) = derived_item_ids(&tabs, 1);
    assert_eq!(ids.get(&(0, 0)), Some(&1));
    assert_eq!(ids.get(&(0, 1)), Some(&2));
    assert_eq!(ids.get(&(2, 0)), Some(&3));
    assert_eq!(next_id, 4);
}

#[test]
fn reserve_item_ids_is_monotonic_in_the_reserved_band() {
    let mut cursor = 0;
    assert_eq!(reserve_item_ids(&mut cursor, 9), MERCHANT_ITEM_ID_MIN);
    assert_eq!(cursor, 10, "all nine ids are counted, not just the first");
    assert_eq!(reserve_item_ids(&mut cursor, 5), 10);
    assert_eq!(cursor, 15);
    cursor = MERCHANT_ITEM_ID_MAX;
    assert_eq!(reserve_item_ids(&mut cursor, 1), MERCHANT_ITEM_ID_MIN);
    assert_eq!(
        reserve_item_ids(&mut cursor, 0),
        0,
        "an empty run mints nothing"
    );
    assert_eq!(cursor, MERCHANT_ITEM_ID_MIN + 1);
}

#[test]
fn component_record_frames_the_payload_as_a_nested_0x32() {
    let tabs = vec![MerchantTab {
        index: 0,
        id: 7,
        name: "QuestItems".to_string(),
        static_contents: true,
        auto_generate: false,
        item_generator: None,
        min_item_level: None,
        max_item_level: None,
        items: vec![item(0, "QuestItemPAL2.D00_Q11_a2_01", 0, 0, 1)],
    }];
    let record = build_merchant_component_record(0x1234, 0xE0FF, VENDOR, &tabs, 1);
    assert_eq!(record[0], 0x32);
    assert_eq!(&record[1..3], &0x1234u16.to_le_bytes());
    assert_eq!(&record[3..5], &0xE0FFu16.to_le_bytes());
    assert_eq!(record[5], 0xFF);
    let nul = record[6..].iter().position(|&b| b == 0).unwrap() + 6;
    assert_eq!(&record[6..nul], b"world.tutorial.npc.HermitVendor.Merchant");
    assert_eq!(&record[nul + 1..], build_stock_payload(&tabs, 1).as_slice());
}
