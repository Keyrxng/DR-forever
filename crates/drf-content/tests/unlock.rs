// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::path::{Path, PathBuf};

use drf_content::{class_is_locked, class_unlock_requirement, Archive, UnlockRequirement};

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
fn authored_unlock_objects_drive_doors_and_ncis() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");

    let squeakeasy = "Dungeon01Doors.Door_Squeakeasy";
    assert!(class_is_locked(&archive, squeakeasy).unwrap());
    assert_eq!(
        class_unlock_requirement(&archive, squeakeasy).unwrap(),
        Some(UnlockRequirement {
            object: "Dungeon01ItemPAL.D01_Key_01".to_string(),
            consumed: true,
        })
    );

    let fizz_rock = "world.dungeon01.data.base.Fizz_Rock_NCI";
    assert!(class_is_locked(&archive, fizz_rock).unwrap());
    assert_eq!(
        class_unlock_requirement(&archive, fizz_rock).unwrap(),
        Some(UnlockRequirement {
            object: "QuestItemPAL2.D01_Q10_a1_01".to_string(),
            consumed: false,
        })
    );

    let boss_gate = "world.dungeon01.data.QuestGateD1L1";
    assert!(class_is_locked(&archive, boss_gate).unwrap());
    assert_eq!(class_unlock_requirement(&archive, boss_gate).unwrap(), None);

    let sword = "terrain.interactives.loot.SwordInStone_Mythic_01";
    assert!(!class_is_locked(&archive, sword).unwrap());
    assert_eq!(class_unlock_requirement(&archive, sword).unwrap(), None);
}
