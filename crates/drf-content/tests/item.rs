// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-content`'s derived item record and bag geometry against `game.pkg`.
//!
//! The pinned bytes below are fixed test data.
//!
//! Outputs:
//! `raw ff51756573744974656d50414c322e4430305f5131315f61315f30310034120000000001010000`
//! `kw QuestItemPAL2.D00_Q11_a1_01 0x1234 1 0x0`
//! `patched ff51756573744974656d50414c322e4430305f5131315f61315f30310034120000000001010300 0x3`
//! `footprint 1 1`
//! `bag (10, 8)`
//!
//! Without a `game.pkg` on this machine every test skips, as the sibling
//! `quest.rs` test does.

use std::env;
use std::path::{Path, PathBuf};

use drf_content::{author_item_record_by_class, derive_bag_geometry, Archive, BagGrid, Corpus};

const Q11_A1_ITEM: &str = "QuestItemPAL2.D00_Q11_a1_01";
const RAW: &str = "ff51756573744974656d50414c322e4430305f5131315f61315f30310034120000000001010000";
const PATCHED: &str =
    "ff51756573744974656d50414c322e4430305f5131315f61315f30310034120000000001010300";

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn corpus() -> Option<Corpus> {
    let (pki, pkg) = pkg_paths()?;
    let archive = Archive::open(&pki, &pkg).expect("open game.pki/game.pkg");
    Some(Corpus::new(archive))
}

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
fn q11_a1_item_record_matches_the_reference() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let raw = author_item_record_by_class(&mut corpus, Q11_A1_ITEM, 0x1234, 1, None, (0, 0))
        .expect("author")
        .expect("game.pkg defines the Q11_a1 item");
    assert_eq!(hex(&raw.record), RAW, "authored record");
    assert_eq!(raw.class, Q11_A1_ITEM);
    assert_eq!(raw.slot, 0x1234);
    assert_eq!(raw.level, 1);
    assert_eq!(raw.flags, 0);
    assert_eq!((raw.width, raw.height), (1, 1));

    let mut patched = raw.clone();
    patched
        .apply_instance_flags(true, true)
        .expect("patch flags");
    assert_eq!(hex(&patched.record), PATCHED, "flags-patched record");
    assert_eq!(patched.flags, 0x03);
    assert_eq!(
        patched.record.len(),
        raw.record.len(),
        "patching the flags must not move the record's length"
    );
}

#[test]
fn bag_geometry_is_the_archive_footprint() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    assert_eq!(
        derive_bag_geometry(&mut corpus).expect("bag"),
        Some((10, 8))
    );
}

#[test]
fn bag_grid_placement_is_the_clients_first_fit() {
    let mut grid = BagGrid::new(10, 8);
    assert_eq!(grid.place(1, 1), Some((0, 0)));
    assert_eq!(grid.place(1, 1), Some((0, 1)));
    grid.rollback(0, 0, 1, 1);
    assert_eq!(grid.place(1, 1), Some((0, 0)));
    assert_eq!(grid.place(10, 8), None);
}
