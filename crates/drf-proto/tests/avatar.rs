// SPDX-License-Identifier: GPL-3.0-or-later

//! Byte parity for the avatar `0x02` head and its six `0x32` components.
//!
//! The expected bytes are pinned as test data.
//!
//! The head and the class-independent components are compared without
//! `game.pkg`; the Equipment, UnitContainer, Manipulators and Skills records
//! are derived from `game.pkg` and compared when it is present.

use std::path::{Path, PathBuf};

use drf_content::{
    derive_avatar_inventory_roster, derive_class_items, derive_class_kit, derive_zone_spawn,
    Archive, Corpus,
};
use drf_proto::{
    build_avatar_skills_payload, build_minimal_avatar_init_payload,
    minimal_avatar_equipment_component, minimal_avatar_manipulators_component,
    minimal_avatar_modifiers_component, minimal_avatar_skills_component,
    minimal_avatar_unit_behavior_component, minimal_avatar_unit_container_component,
    MINIMAL_AVATAR_HP, MINIMAL_AVATAR_MP, MINIMAL_AVATAR_WE_FLAGS_ATTACH,
};

const EID: u16 = 0x1001;
const PID: u16 = 0x1000;
const CLASS: &[u8] = b"avatar.classes.WarlockFemale";

/// `world units * 256`, the tutorial `misc.Waypoint Start` pose.
const TUTORIAL_POSE: [u32; 4] = [184320, 117760, 10240, 0xFFFF_CE00];

const HEAD_HEX: &str = "0400000000d0020000cc01000028000000ceffff00070100000000001000c8000000c80000000000000000000000000000000000000000000000000000000000";
const SKILLS_HEX: &str = "32011000e0ff6176617461722e626173652e536b696c6c730001000000000000";
const EQUIPMENT_HEX: &str = "32011001e0ff6176617461722e626173652e45717569706d656e74000104ff314853746166663150616c2e31485374616666312d31000a000000ffff01010000ff6974656d732e70616c2e4d616765426f647950414c2e4e6f726d616c3030310006000000ffff0101000000ff6974656d732e70616c2e4d616765476c6f76657350414c2e4e6f726d616c3030320002000000ffff0101000000ff6974656d732e70616c2e4d616765426f6f747350414c2e4e6f726d616c3030320007000000ffff0101000000";
const CONTAINER_HEX: &str = "32011002e0ff556e6974436f6e7461696e65720001310000000000000009ff6176617461722e626173652e496e76656e746f7279000b0100ff6176617461722e626173652e42616e6b000c0100ff6176617461722e626173652e5472616465496e76656e746f7279000d0100ff6176617461722e626173652e42616e6b32000e0100ff6176617461722e626173652e42616e6b33000f0100ff6176617461722e626173652e42616e6b3400100100ff6176617461722e626173652e42616e6b3500110100ff6176617461722e626173652e42616e6b3600120100ff6176617461722e626173652e42616e6b370013010000";
const SKILLS_PAYLOAD_HEX: &str = concat!(
    "010000000004ff736b696c6c732e67656e657269632e46697265426f6c74006900000001ff736b696c6c732e67656e657269",
    "632e536861646f774c696768746e696e67006400000001ff736b696c6c732e67656e657269632e4d616765436c6173735061",
    "7373697665006c00000001ff736b696c6c732e67656e657269632e4d6167696344616d6167654d6f6450617373697665006d",
    "0000000101ff736b696c6c732e70726f66657373696f6e732e5761726c6f636b00",
);
const MANIPULATORS_HEX: &str = concat!(
    "32011003e0ff4d616e6970756c61746f7273000106ff314853746166663150616c2e31485374616666312d31000a000000ff",
    "ff010100000000000000ff6974656d732e70616c2e4d616765426f647950414c2e4e6f726d616c3030310006000000ffff01",
    "01000000ff6974656d732e70616c2e4d616765476c6f76657350414c2e4e6f726d616c3030320002000000ffff0101000000",
    "ff6974656d732e70616c2e4d616765426f6f747350414c2e4e6f726d616c3030320007000000ffff0101000000ff736b696c",
    "6c732e67656e657269632e46697265426f6c7400690000000100ff736b696c6c732e67656e657269632e536861646f774c69",
    "6768746e696e6700640000000100",
);
const MODIFIERS_HEX: &str = "32011004e0ff4d6f646966696572730001000000000000000000";
const UNIT_BEHAVIOR_HEX: &str =
    "32011005e0ff6176617461722e626173652e556e69744265686176696f7200000000000140000000000000000001000000";

fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    assert_eq!(digits.len() % 2, 0, "odd hex length");
    let value = |b: u8| match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => panic!("non-hex digit {b:?}"),
    };
    digits
        .chunks(2)
        .map(|pair| value(pair[0]) << 4 | value(pair[1]))
        .collect()
}

#[test]
fn head_matches_the_lab_reference() {
    let head = build_minimal_avatar_init_payload(
        EID,
        PID,
        CLASS,
        Some(TUTORIAL_POSE),
        Some(1),
        &[],
        MINIMAL_AVATAR_WE_FLAGS_ATTACH,
        Some(MINIMAL_AVATAR_HP),
        Some(MINIMAL_AVATAR_MP),
    )
    .unwrap();
    assert_eq!(head, hex(HEAD_HEX));
}

#[test]
fn class_independent_components_match_the_lab_reference() {
    assert_eq!(
        minimal_avatar_skills_component(EID, 0xE000, 0).unwrap(),
        hex(SKILLS_HEX)
    );
    assert_eq!(
        minimal_avatar_modifiers_component(EID, 0xE004).unwrap(),
        hex(MODIFIERS_HEX)
    );
    assert_eq!(
        minimal_avatar_unit_behavior_component(EID, 0xE005, false).unwrap(),
        hex(UNIT_BEHAVIOR_HEX)
    );
}

#[test]
fn content_components_match_the_lab_reference() {
    let Some(dir) = pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let archive =
        Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("open archive");
    let mut corpus = Corpus::new(archive);
    let items = derive_class_items(&mut corpus, "Warlock").unwrap();
    let kit = derive_class_kit(&mut corpus, "Warlock").unwrap();
    let skill_wire = kit.skill_wire().unwrap();
    let roster: Vec<(String, u8)> = derive_avatar_inventory_roster(&mut corpus)
        .unwrap()
        .into_iter()
        .map(|entry| (entry.fqn, entry.id))
        .collect();

    assert_eq!(
        minimal_avatar_equipment_component(EID, 0xE001, Some(&items)).unwrap(),
        hex(EQUIPMENT_HEX)
    );
    assert_eq!(
        minimal_avatar_unit_container_component(EID, 0xE002, &roster).unwrap(),
        hex(CONTAINER_HEX)
    );
    assert_eq!(
        minimal_avatar_manipulators_component(EID, 0xE003, Some(&items), &skill_wire.manipulators)
            .unwrap(),
        hex(MANIPULATORS_HEX)
    );
}

#[test]
fn skills_payload_matches_the_lab_reference() {
    let Some(dir) = pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let archive =
        Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("open archive");
    let mut corpus = Corpus::new(archive);
    let kit = derive_class_kit(&mut corpus, "Warlock").unwrap();
    let skill_wire = kit.skill_wire().unwrap();
    let payload = build_avatar_skills_payload(0, &skill_wire.skills, &skill_wire.professions);
    assert_eq!(payload, hex(SKILLS_PAYLOAD_HEX));
}

#[test]
fn tutorial_spawn_pose_is_derived_from_game_pkg() {
    let Some(dir) = pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let archive =
        Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("open archive");
    let spawn = derive_zone_spawn(&archive, "tutorial", None)
        .unwrap()
        .expect("tutorial resolves a spawn");
    assert_eq!(spawn.name, "Start");
    assert_eq!(spawn.pose, TUTORIAL_POSE);
}

fn pkg_dir() -> Option<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.ancestors().nth(4)?;
    [
        repo.join("dr-client-666-testing"),
        repo.join("Dungeon_Runners_Client_666"),
    ]
    .into_iter()
    .find(|dir| dir.join("game.pkg").is_file())
}
