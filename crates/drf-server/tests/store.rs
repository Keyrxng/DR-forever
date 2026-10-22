// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistence round-trip and refusal invariants.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use drf_server::store::{
    qid_from_hex, qid_to_hex, Appearance, BagItem, EquipmentItem, QuestObjective, QuestState,
    Store, StoreError, DEFAULT_ZONE, MAX_CHARS_PER_ACCOUNT,
};

/// A self-cleaning directory under the system temp dir.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let mut path = std::env::temp_dir();
        path.push(format!("drf-store-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn snapshot(store: &Store, account: &str) -> Vec<drf_server::store::Character> {
    store.list_chars(account).into_iter().cloned().collect()
}

#[test]
fn create_cap_delete_and_persistence_round_trip() {
    let dir = TempDir::new("roundtrip");
    let path = dir.path().join("s1_store.json");

    let mut store = Store::open(&path).expect("open fresh store");
    store.ensure_account("a").expect("ensure account");

    let appearance = Appearance {
        skin: Some(1),
        face: Some(2),
        face_feature: Some(3),
        hair: Some(4),
        hair_color: Some(2),
    };
    let first = store
        .create_char("a", "Alpha", "avatar.classes.WarlockFemale", appearance)
        .expect("create first");
    let second = store
        .create_char(
            "a",
            "Bravo",
            "avatar.classes.FighterMale",
            Appearance::default(),
        )
        .expect("create second");
    let third = store
        .create_char(
            "a",
            "Chuck",
            "avatar.classes.RangerMale",
            Appearance::default(),
        )
        .expect("create third");
    assert_eq!((first, second, third), (1, 2, 3));
    // Fill the roster to the per-account cap.
    for name in ["Delta", "Echo", "Foxtrot", "Golf", "Hotel"] {
        store
            .create_char(
                "a",
                name,
                "avatar.classes.WarlockFemale",
                Appearance::default(),
            )
            .expect("create within cap");
    }
    assert_eq!(store.list_chars("a").len(), MAX_CHARS_PER_ACCOUNT);

    // The next one is refused by the per-account cap.
    let before = snapshot(&store, "a");
    assert!(matches!(
        store.create_char(
            "a",
            "India",
            "avatar.classes.WarlockFemale",
            Appearance::default()
        ),
        Err(StoreError::AccountFull)
    ));
    // A duplicate name is refused; a bad name is refused.
    assert!(matches!(
        store.create_char(
            "a",
            "Alpha",
            "avatar.classes.WarlockFemale",
            Appearance::default()
        ),
        Err(StoreError::DuplicateName)
    ));
    assert!(matches!(
        store.create_char(
            "a",
            "",
            "avatar.classes.WarlockFemale",
            Appearance::default()
        ),
        Err(StoreError::InvalidName)
    ));
    assert!(matches!(
        store.create_char(
            "a",
            "has space",
            "avatar.classes.WarlockFemale",
            Appearance::default()
        ),
        Err(StoreError::InvalidName)
    ));
    assert!(matches!(
        store.create_char(
            "a",
            "1234567890",
            "avatar.classes.WarlockFemale",
            Appearance::default()
        ),
        Err(StoreError::InvalidName)
    ));
    assert!(matches!(
        store.create_char("a", "Echo", "", Appearance::default()),
        Err(StoreError::InvalidClass)
    ));
    // No refusal mutated the roster.
    assert_eq!(snapshot(&store, "a"), before);

    store.delete_char("a", second).expect("delete second");
    assert_eq!(store.list_chars("a").len(), MAX_CHARS_PER_ACCOUNT - 1);
    store.save().expect("explicit save");

    // Reopen the file and confirm the records survived.
    let reopened = Store::open(&path).expect("reopen store");
    let chars = reopened.list_chars("a");
    assert_eq!(chars.len(), MAX_CHARS_PER_ACCOUNT - 1);
    assert_eq!(chars[0].sqlid, 1);
    assert_eq!(chars[0].name, "Alpha");
    assert_eq!(chars[0].avatar_class, "avatar.classes.WarlockFemale");
    assert_eq!(chars[0].appearance.skin, Some(1));
    assert_eq!(chars[0].appearance.hair_color, Some(2));
    assert_eq!(chars[0].level, 1);
    assert_eq!(chars[0].experience, 0);
    assert_eq!(chars[0].attribute_points, 0);
    assert_eq!(chars[0].zone, DEFAULT_ZONE);
    assert_eq!(chars[1].sqlid, 3);
    assert_eq!(chars[1].name, "Chuck");

    // A sqlid allocated after reopening does not collide with the survivors.
    let mut reopened = reopened;
    assert_eq!(
        reopened
            .create_char(
                "a",
                "India",
                "avatar.classes.WarlockFemale",
                Appearance::default()
            )
            .expect("create after reopen"),
        (MAX_CHARS_PER_ACCOUNT as u32) + 1
    );
}

#[test]
fn set_zone_and_get_char_are_account_scoped() {
    let dir = TempDir::new("zone");
    let path = dir.path().join("s1_store.json");
    let mut store = Store::open(&path).expect("open");
    let sqlid = store
        .create_char(
            "a",
            "Hero",
            "avatar.classes.WarlockFemale",
            Appearance::default(),
        )
        .expect("create");

    assert_eq!(store.get_char("a", sqlid).unwrap().zone, DEFAULT_ZONE);
    store.set_zone("a", sqlid, "algate").expect("set zone");
    assert_eq!(store.get_char("a", sqlid).unwrap().zone, "algate");
    // Empty / NUL-prefixed zone falls back to the default.
    store.set_zone("a", sqlid, "\0junk").expect("set zone");
    assert_eq!(store.get_char("a", sqlid).unwrap().zone, DEFAULT_ZONE);
    // Another account cannot see or mutate the character.
    assert!(store.get_char("b", sqlid).is_none());
    assert!(matches!(
        store.set_zone("b", sqlid, "algate"),
        Err(StoreError::NoSuchCharacter)
    ));
}

#[test]
fn leveling_progress_survives_save_and_reload() {
    let dir = TempDir::new("progress");
    let path = dir.path().join("s1_store.json");
    let mut store = Store::open(&path).expect("open");
    let sqlid = store
        .create_char(
            "a",
            "Hero",
            "avatar.classes.WarlockFemale",
            Appearance::default(),
        )
        .expect("create");
    assert_eq!(store.get_char("a", sqlid).unwrap().experience, 0);
    assert_eq!(store.get_char("a", sqlid).unwrap().attribute_points, 0);

    // Mirror two self-awarded kills: 500 XP each, still L1 at 1000 (exactly the
    // threshold, so it crosses once -> +5 points, 0 carry).
    let first = drf_content::apply_xp(1, 0, drf_content::xp_per_kill(2, 1, 100));
    assert_eq!(
        (first.level, first.experience, first.levels_gained),
        (1, 500, 0)
    );
    let second = drf_content::apply_xp(
        first.level,
        first.experience,
        drf_content::xp_per_kill(2, 1, 100),
    );
    assert_eq!(
        (second.level, second.experience, second.levels_gained),
        (2, 0, 1)
    );
    let points = second.levels_gained * drf_content::ATTRIBUTE_POINTS_PER_LEVEL;
    store
        .set_progress("a", sqlid, second.level, second.experience, points)
        .expect("commit progress");

    let reopened = Store::open(&path).expect("reopen");
    let hero = reopened.get_char("a", sqlid).expect("character survives");
    assert_eq!(hero.level, 2);
    assert_eq!(hero.experience, 0);
    assert_eq!(hero.attribute_points, 5);

    // The writer emits the fields, so a record round-trips through the text.
    let text = fs::read_to_string(&path).expect("read store text");
    assert!(text.contains("\"experience\": 0"), "store:\n{text}");
    assert!(text.contains("\"attribute_points\": 5"), "store:\n{text}");

    // A record lacking the keys (a pre-XP store) loads with the zero defaults.
    fs::write(
        &path,
        br#"{"next_sqlid": 2, "accounts": {"a": {"characters": {"1": {
            "name": "Old", "class": "avatar.classes.FighterMale",
            "level": 4, "zone": "town" }}}}}"#,
    )
    .unwrap();
    let legacy = Store::open(&path).expect("legacy store loads");
    let old = legacy.get_char("a", 1).expect("legacy character");
    assert_eq!(old.level, 4);
    assert_eq!(old.experience, 0);
    assert_eq!(old.attribute_points, 0);
    assert_eq!(
        (old.strength, old.agility, old.toughness, old.power),
        (0, 0, 0, 0)
    );
}

#[test]
fn attribute_allocation_survives_save_and_reload() {
    let dir = TempDir::new("attributes");
    let path = dir.path().join("s1_store.json");
    let mut store = Store::open(&path).expect("open");
    let sqlid = store
        .create_char(
            "a",
            "Hero",
            "avatar.classes.WarlockFemale",
            Appearance::default(),
        )
        .expect("create");
    store
        .set_progress("a", sqlid, 3, 120, 10)
        .expect("seed progress");
    store
        .set_attribute_allocation("a", sqlid, 7, [1, 0, 2, 0])
        .expect("allocate");

    let reopened = Store::open(&path).expect("reopen");
    let hero = reopened.get_char("a", sqlid).expect("character survives");
    assert_eq!(hero.attribute_points, 7);
    assert_eq!(
        (hero.strength, hero.agility, hero.toughness, hero.power),
        (1, 0, 2, 0)
    );
    // The client invariant: allocated + unspent == 5 * (level - 1).
    let allocated: u32 = [hero.strength, hero.agility, hero.toughness, hero.power]
        .iter()
        .sum();
    assert_eq!(allocated + hero.attribute_points, 5 * (hero.level - 1));

    let text = fs::read_to_string(&path).expect("read store text");
    assert!(text.contains("\"strength\": 1"), "store:\n{text}");
    assert!(text.contains("\"toughness\": 2"), "store:\n{text}");

    // A foreign account cannot mutate the row.
    assert!(matches!(
        store.set_attribute_allocation("b", sqlid, 0, [0, 0, 0, 0]),
        Err(StoreError::NoSuchCharacter)
    ));
}

#[test]
fn malformed_file_is_rejected() {
    let dir = TempDir::new("malformed");
    let path = dir.path().join("s1_store.json");

    fs::write(&path, b"{ this is not json").unwrap();
    assert!(matches!(Store::open(&path), Err(StoreError::Malformed(_))));

    fs::write(&path, br#"{"accounts": 5}"#).unwrap();
    assert!(matches!(Store::open(&path), Err(StoreError::Malformed(_))));

    fs::write(
        &path,
        br#"{"characters": {"1": {"account": "a", "class": "x"}}}"#,
    )
    .unwrap();
    assert!(matches!(Store::open(&path), Err(StoreError::Malformed(_))));
}
#[test]
fn quests_bag_and_gold_survive_save_and_reload() {
    let dir = TempDir::new("avatar-state");
    let path = dir.path().join("s1_store.json");
    let mut store = Store::open(&path).expect("open");
    store.ensure_account("a").expect("ensure account");
    let sqlid = store
        .create_char(
            "a",
            "Hero",
            "avatar.classes.WarlockFemale",
            Appearance::default(),
        )
        .expect("create");

    // A fresh character starts with the server's purse and no rows.
    assert_eq!(store.get_gold("a", sqlid), Some(5000));
    assert!(store.get_bag("a", sqlid).is_empty());
    assert!(store.quest_snapshot("a", sqlid).is_empty());

    let accepted_qid = 0xFBBE_D69D;
    let turned_in_qid = 0x1122_3344;
    store
        .mark_accepted(
            "a",
            sqlid,
            accepted_qid,
            7,
            vec![QuestObjective {
                flags: 0,
                label: "Ticket #54,000,601: 0 / 1".to_string(),
            }],
        )
        .expect("mark accepted");
    store
        .quest_set_state("a", sqlid, turned_in_qid, QuestState::TurnedIn)
        .expect("turn in");
    store
        .set_bag(
            "a",
            sqlid,
            vec![BagItem {
                class: "QuestItemPAL2.D00_Q11_a1_01".to_string(),
                cell_x: 0,
                cell_y: 0,
                count: 1,
                level: None,
                flags: None,
                modifiers: Vec::new(),
            }],
        )
        .expect("set bag");
    store.set_gold("a", sqlid, 4321).expect("set gold");

    let reopened = Store::open(&path).expect("reopen");
    let snapshot = reopened.quest_snapshot("a", sqlid);
    assert_eq!(snapshot.len(), 2);
    let accepted = snapshot
        .get(&qid_to_hex(accepted_qid))
        .expect("accepted row survived");
    assert_eq!(accepted.state, QuestState::Accepted);
    assert_eq!(accepted.instance_ref, 7);
    assert_eq!(accepted.objectives.len(), 1);
    assert_eq!(accepted.objectives[0].label, "Ticket #54,000,601: 0 / 1");
    assert_eq!(
        snapshot.get(&qid_to_hex(turned_in_qid)).unwrap().state,
        QuestState::TurnedIn
    );
    let completed = reopened.quest_completed_qids("a", sqlid);
    assert!(completed.contains(&turned_in_qid));
    assert!(!completed.contains(&accepted_qid));
    assert_eq!(reopened.get_gold("a", sqlid), Some(4321));
    let bag = reopened.get_bag("a", sqlid);
    assert_eq!(bag.len(), 1);
    assert_eq!(bag[0].class, "QuestItemPAL2.D00_Q11_a1_01");
    assert_eq!((bag[0].cell_x, bag[0].cell_y, bag[0].count), (0, 0, 1));

    // The hex key is the five-byte wire id, not the bare hash.
    assert_eq!(qid_to_hex(accepted_qid), "049dd6befb");
    assert_eq!(qid_from_hex("049dd6befb"), Some(accepted_qid));
    assert_eq!(qid_from_hex("nope"), None);
}

#[test]
fn equipment_and_bag_level_flags_round_trip() {
    let dir = TempDir::new("equipment");
    let path = dir.path().join("s1_store.json");
    let mut store = Store::open(&path).expect("open store");
    store.ensure_account("a").expect("ensure account");
    let sqlid = store
        .create_char(
            "a",
            "Alpha",
            "avatar.classes.WarlockFemale",
            Appearance::default(),
        )
        .expect("create");
    assert_eq!(store.get_equipment("a", sqlid), None, "fresh = class kit");

    store
        .upsert_bag_item(
            "a",
            sqlid,
            BagItem {
                class: "LeatherArmor3PAL.LeatherArmor3-2".to_string(),
                cell_x: 3,
                cell_y: 4,
                count: 2,
                level: Some(5),
                flags: Some(3),
                modifiers: Vec::new(),
            },
        )
        .expect("upsert bag");
    store
        .set_equipment(
            "a",
            sqlid,
            vec![EquipmentItem {
                slot: 8,
                class: "LeatherArmor3PAL.LeatherArmor3-2".to_string(),
                level: 5,
                flags: 3,
                modifiers: Vec::new(),
            }],
        )
        .expect("set equipment");

    let reopened = Store::open(&path).expect("reopen");
    let bag = reopened.get_bag("a", sqlid);
    assert_eq!(bag.len(), 1);
    assert_eq!((bag[0].cell_x, bag[0].cell_y, bag[0].count), (3, 4, 2));
    assert_eq!((bag[0].level, bag[0].flags), (Some(5), Some(3)));
    assert_eq!(
        reopened.get_equipment("a", sqlid),
        Some(vec![EquipmentItem {
            slot: 8,
            class: "LeatherArmor3PAL.LeatherArmor3-2".to_string(),
            level: 5,
            flags: 3,
            modifiers: Vec::new(),
        }])
    );

    let mut store = reopened;
    store
        .remove_bag_item("a", sqlid, "LeatherArmor3PAL.LeatherArmor3-2", (3, 4))
        .expect("remove bag");
    store
        .set_equipment("a", sqlid, Vec::new())
        .expect("explicit empty");
    let reopened = Store::open(&path).expect("reopen empty");
    assert!(reopened.get_bag("a", sqlid).is_empty());
    assert_eq!(reopened.get_equipment("a", sqlid), Some(Vec::new()));
}
