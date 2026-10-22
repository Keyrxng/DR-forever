// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-proto` in-world `Equipment`/`Manipulators` item bytes against `game.pkg`.
//!
//! The assertions parse the built payload back into fields and re-encode it
//! independently. Content-dependent tests skip when the archive is unavailable.

use std::path::{Path, PathBuf};

use drf_content::{author_item_record_by_class, derive_class_items, Archive, ClassItem, Corpus};
use drf_proto::{
    build_component_update_stream, build_container_add_currency, build_container_item_add,
    class_starting_equipment_payload, class_starting_item_manipulators_bytes, equipment_item_entry,
    item_manipulator_entry, item_manipulator_readstate, CONTAINER_CURRENCY_BODY_LEN,
    CONTAINER_SUB_ADD_CURRENCY, CONTAINER_SUB_ITEM_ADD, EQUIPMENT_STARTING_MOD_LEVEL,
    MERCHANT_BAG_CONTAINER_ID,
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

const CLASSES: [(&str, &str); 3] = [
    ("avatar.classes.FighterMale", "Fighter"),
    ("avatar.classes.RangerFemale", "Ranger"),
    ("avatar.classes.WarlockFemale", "Warlock"),
];

#[test]
fn payload_lengths_are_fixed() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    for (aclass, base, equip_len, manip_len) in [
        ("avatar.classes.FighterMale", "Fighter", 216usize, 219usize),
        ("avatar.classes.RangerFemale", "Ranger", 179, 181),
        ("avatar.classes.WarlockFemale", "Warlock", 171, 174),
    ] {
        let items = derive_class_items(&mut corpus, base).unwrap();
        let equipment = class_starting_equipment_payload(&items, None, None);
        let manipulators = class_starting_item_manipulators_bytes(&items, None, None);
        assert_eq!(equipment.len(), equip_len, "{aclass} equipment length");
        assert_eq!(
            manipulators.len(),
            manip_len,
            "{aclass} manipulators length"
        );
    }
}

#[test]
fn equipment_payload_round_trips() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    for (aclass, base) in CLASSES {
        let items = derive_class_items(&mut corpus, base).unwrap();
        let payload = class_starting_equipment_payload(&items, None, None);
        let parsed = parse_equipment_payload(&payload, &items);
        // The parsed fields are the content's fields, at the corpus level.
        for (entry, item) in parsed.iter().zip(&items) {
            assert_eq!(entry.fqn, item.fqn);
            assert_eq!(entry.slot, item.slot);
            assert_eq!(entry.level, item.level as u8);
            assert_eq!(entry.authored, item.authored);
            assert_eq!(
                entry
                    .mods
                    .iter()
                    .map(|(m, _)| m.as_str())
                    .collect::<Vec<_>>(),
                item.mods.iter().map(|m| m.as_str()).collect::<Vec<_>>()
            );
            for (_, mod_level) in &entry.mods {
                assert_eq!(*mod_level, EQUIPMENT_STARTING_MOD_LEVEL);
            }
        }
        // Independent re-encode: header + each parsed entry must reproduce it.
        let rebuilt = encode_payload(&parsed);
        assert_eq!(rebuilt, payload, "{aclass} did not re-encode");
    }
}

#[test]
fn manipulators_are_equipment_plus_readstate() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    for (_, base) in CLASSES {
        let items = derive_class_items(&mut corpus, base).unwrap();
        let mut expected = Vec::new();
        for item in &items {
            expected.extend_from_slice(&equipment_item_entry(
                &item.fqn,
                item.slot,
                item.level as u8,
                item.authored,
                &item.mods,
                EQUIPMENT_STARTING_MOD_LEVEL,
            ));
            expected.extend_from_slice(&item_manipulator_readstate(&item.dfc).unwrap());
        }
        let manipulators = class_starting_item_manipulators_bytes(&items, None, None);
        assert_eq!(manipulators, expected);
        for item in &items {
            let entry = item_manipulator_entry(
                &item.fqn,
                item.slot,
                item.level as u8,
                item.authored,
                &item.mods,
                &item.dfc,
                EQUIPMENT_STARTING_MOD_LEVEL,
            );
            assert!(entry.starts_with(&equipment_item_entry(
                &item.fqn,
                item.slot,
                item.level as u8,
                item.authored,
                &item.mods,
                EQUIPMENT_STARTING_MOD_LEVEL,
            )));
        }
    }
}

#[test]
fn readstate_is_keyed_on_the_dfc_root() {
    assert_eq!(
        item_manipulator_readstate("MeleeWeapon").unwrap(),
        vec![0u8; 5]
    );
    assert_eq!(
        item_manipulator_readstate("RangedWeapon").unwrap(),
        vec![0u8; 4]
    );
    assert_eq!(
        item_manipulator_readstate("Armor").unwrap(),
        Vec::<u8>::new()
    );
    assert_eq!(
        item_manipulator_readstate("Item").unwrap(),
        Vec::<u8>::new()
    );
    assert!(item_manipulator_readstate("Sword").is_err());
    assert!(item_manipulator_readstate("").is_err());
    // Only the bytes before the NUL name the class.
    assert_eq!(
        item_manipulator_readstate("RangedWeapon\u{0}junk").unwrap(),
        vec![0u8; 4]
    );
}

struct ParsedEntry {
    fqn: String,
    slot: i64,
    level: u8,
    authored: usize,
    mods: Vec<(String, u8)>,
}

fn parse_equipment_payload(payload: &[u8], items: &[ClassItem]) -> Vec<ParsedEntry> {
    let mut c = Cursor::new(payload);
    assert_eq!(c.u8(), 0x01, "post byte");
    let count = c.list_count();
    assert_eq!(count, items.len(), "item count");
    let mut out = Vec::with_capacity(count);
    for item in items {
        let fqn = c.cstr("item classname");
        let slot = c.u32() as i64;
        assert_eq!(c.take(3), &[0xFF, 0xFF, 0x01], "grid bytes");
        let level = c.u8();
        assert_eq!(c.u8(), 0x00, "flags byte");
        let authored = c.take(item.authored);
        assert!(authored.iter().all(|&b| b == 0), "authored status bytes");
        let mod_count = c.list_count();
        assert_eq!(mod_count, item.mods.len(), "modifier count");
        let mut mods = Vec::with_capacity(mod_count);
        for expected in &item.mods {
            let name = c.cstr("modifier classname");
            assert_eq!(&name, expected);
            assert_eq!(c.u8(), 0x01, "modifier body flags");
            mods.push((name, c.u8()));
        }
        out.push(ParsedEntry {
            fqn,
            slot,
            level,
            authored: item.authored,
            mods,
        });
    }
    assert_eq!(c.pos, payload.len(), "payload not fully consumed");
    out
}

fn encode_payload(entries: &[ParsedEntry]) -> Vec<u8> {
    let mut out = vec![0x01];
    if entries.len() > 0xFE {
        out.push(0xFF);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    } else {
        out.push(entries.len() as u8);
    }
    for entry in entries {
        out.push(0xFF);
        out.extend_from_slice(entry.fqn.as_bytes());
        out.push(0);
        out.extend_from_slice(&(entry.slot as u32).to_le_bytes());
        out.extend_from_slice(&[0xFF, 0xFF, 0x01]);
        out.push(entry.level);
        out.push(0);
        out.extend(std::iter::repeat_n(0u8, entry.authored));
        if entry.mods.len() > 0xFE {
            out.push(0xFF);
            out.extend_from_slice(&(entry.mods.len() as u16).to_le_bytes());
        } else {
            out.push(entry.mods.len() as u8);
        }
        for (name, mod_level) in &entry.mods {
            out.push(0xFF);
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            out.push(0x01);
            out.push(*mod_level);
        }
    }
    out
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Cursor<'a> {
        Cursor { bytes, pos: 0 }
    }

    fn u8(&mut self) -> u8 {
        let value = self.bytes[self.pos];
        self.pos += 1;
        value
    }

    fn u32(&mut self) -> u32 {
        let value = u32::from_le_bytes(self.take(4).try_into().unwrap());
        value
    }

    fn list_count(&mut self) -> usize {
        let first = self.u8();
        if first == 0xFF {
            let lo = self.u8() as usize;
            let hi = self.u8() as usize;
            lo | (hi << 8)
        } else {
            first as usize
        }
    }

    fn take(&mut self, n: usize) -> &'a [u8] {
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        slice
    }

    fn cstr(&mut self, what: &str) -> String {
        assert_eq!(self.u8(), 0xFF, "{what} type tag");
        let start = self.pos;
        while self.bytes[self.pos] != 0 {
            self.pos += 1;
        }
        let text = String::from_utf8(self.bytes[start..self.pos].to_vec()).unwrap();
        self.pos += 1;
        text
    }
}

// -- the on-accept grant: `build_container_item_add` --------------------------
//
// Pinned fixture for the grant-add and component-update stream surfaces.
// The expected values are checked in by this project; no external server or
// capture is required to run the test.
//
// Outputs:
const GRANT_ADD: &str =
    "073506e01e0bff51756573744974656d50414c322e4430305f5131315f61315f30310034120000000001010300";
const GRANT_STREAM: &str = "3506e01e0bff51756573744974656d50414c322e4430305f5131315f61315f30310034120000000001010300020000c80006";

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The `0x20` currency leg against its pinned fixture:
/// `073509e020ffffffff000000000000` (15 bytes = 2+2+1+10).
#[test]
fn add_currency_matches_the_reference() {
    let add = build_container_add_currency(0xE009, -1, 0);
    assert_eq!(hex(&add), "073509e020ffffffff000000000000");
    assert_eq!(&add[0..2], &[0x07, 0x35], "add tag");
    assert_eq!(
        &add[2..4],
        &0xE009u16.to_le_bytes(),
        "addressed to the container"
    );
    assert_eq!(add[4], CONTAINER_SUB_ADD_CURRENCY, "currency sub-opcode");
    assert_eq!(
        i32::from_le_bytes(add[5..9].try_into().unwrap()),
        -1,
        "signed amount"
    );
    assert_eq!(add[9], 0, "unmeasured source bucket");
    assert_eq!(
        u32::from_le_bytes(add[10..14].try_into().unwrap()),
        0,
        "key"
    );
    assert_eq!(add[14], 0, "flag is source != 0");
    assert_eq!(add.len(), 2 + 2 + 1 + CONTAINER_CURRENCY_BODY_LEN);
}

#[test]
fn q11_a1_grant_add_matches_the_reference() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let mut record = author_item_record_by_class(
        &mut corpus,
        "QuestItemPAL2.D00_Q11_a1_01",
        0x1234,
        1,
        None,
        (0, 0),
    )
    .expect("author")
    .expect("the Q11_a1 item resolves");
    record.apply_instance_flags(true, true).expect("flags");

    let add = build_container_item_add(0xE006, &record.record, 0x1234, (0, 0), 1).expect("add");
    assert_eq!(hex(&add), GRANT_ADD, "container item add");
    assert_eq!(&add[0..2], &[0x07, 0x35], "add tag");
    assert_eq!(
        &add[2..4],
        &0xE006u16.to_le_bytes(),
        "addressed to the container"
    );
    assert_eq!(add[4], CONTAINER_SUB_ITEM_ADD, "add sub-opcode");
    assert_eq!(add[5], MERCHANT_BAG_CONTAINER_ID, "bag child id");
    assert_eq!(
        add[add.len() - 1],
        0x00,
        "record modifier count closes the add"
    );

    let mut synch = vec![0x02];
    synch.extend_from_slice(&0x00C8_0000u32.to_le_bytes());
    let stream = build_component_update_stream(&[add], &synch);
    assert_eq!(hex(&stream), GRANT_STREAM, "component update stream");
    assert_eq!(stream[0], 0x35, "the stream opens with 0x35 for the caller");
    assert_eq!(stream[stream.len() - 1], 0x06, "EndStream");
}
