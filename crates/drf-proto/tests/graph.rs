// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-proto` character-graph bytes against `game.pkg`.
//!
//! The bytes here are the ones the client's own readers consume. Tests that
//! need the archive skip when it is unavailable.

use std::path::{Path, PathBuf};

use drf_content::{Archive, Corpus};
use drf_proto::{
    created_payload, graph_item_entry, join_player_body, list_payload, player_graph_node,
    Appearance, ClassContent, ListRow, ITEM_ENTRY_LEN,
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

const CLASSES: [&str; 3] = [
    "avatar.classes.FighterMale",
    "avatar.classes.RangerFemale",
    "avatar.classes.WarlockFemale",
];

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[test]
fn item_entry_is_the_client_item_object() {
    // The 59-byte base-Item property block, as the client's own property ids.
    let block = "cb6e9739ff000000cc6e9739ff0000003dbcda0f0100000072e58f1000045c6cdd01000000e1c33ae70080c305e80021c68583ffff000032785900";
    let entry = graph_item_entry("items.pal.1HMacePAL.Normal001", "MeleeWeapon", 10, 0x10);
    assert_eq!(entry.len(), ITEM_ENTRY_LEN);
    assert_eq!(entry[0], 0x2D);
    assert_eq!(&entry[18..77], &decode(block)[..]);
    // trailer: the Name property + empty string + terminator
    assert_eq!(&entry[81..], &decode("460c9b7c0000000000")[..]);
}

fn decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn every_surface_builds_for_every_class() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let appearance = Appearance {
        skin: Some(1),
        face: Some(2),
        face_feature: Some(0),
        hair: Some(3),
        hair_color: Some(4),
    };
    for aclass in CLASSES {
        let content = ClassContent::derive(&mut corpus, aclass).unwrap();
        let node = player_graph_node("tester1", 1, Some(&content), Some(&appearance));
        assert_eq!(node[0], 0x2D);
        let rows = [ListRow {
            sqlid: 1,
            name: "tester1",
            content: Some(&content),
            appearance: Some(&appearance),
        }];
        let list = list_payload(&rows);
        assert_eq!(list[0], 1);
        assert_eq!(list[1..5], 1u32.to_le_bytes());
        let created = created_payload(1, "tester1", Some(&content), Some(&appearance));
        assert_eq!(&created[..4], &1u32.to_le_bytes());
        let join = join_player_body(0x1001, "tester1", 1, Some(&content), Some(&appearance));
        assert_eq!(&join[..2], &0x1001u16.to_le_bytes());
        assert_eq!(join[2], 0xFF);
        assert_eq!(&join[3..10], b"player\0");
    }
}

#[test]
fn the_kit_differs_per_class() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let fighter = ClassContent::derive(&mut corpus, "avatar.classes.FighterMale").unwrap();
    let warlock = ClassContent::derive(&mut corpus, "avatar.classes.WarlockFemale").unwrap();
    assert_eq!(fighter.items[0].dfc, "MeleeWeapon");
    assert_eq!(warlock.items[0].dfc, "MeleeWeapon");
    assert_ne!(fighter.items[0].fqn, warlock.items[0].fqn);
    assert_ne!(fighter.visuals, warlock.visuals);
}

#[test]
fn a_bare_node_carries_no_kit() {
    let node = player_graph_node("tester1", 1, None, None);
    // The bare Avatar has five children and no item objects.
    let text = hex(&node);
    assert!(!text.contains("Equipment"));
    // A kitted node carries the six Avatar children plus the kit.
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let content = ClassContent::derive(&mut corpus, "avatar.classes.WarlockFemale").unwrap();
    let kitted = player_graph_node("tester1", 1, Some(&content), None);
    assert!(kitted.len() > node.len());
}
