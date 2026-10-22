// SPDX-License-Identifier: GPL-3.0-or-later

//! The tutorial's placed world entities and their NPC component run, pinning
//! the `game.pkg`-derived walk, each entity's `0x02` class-state, and the three
//! nested `0x32` components against their derived forms.
//!
//! The expected class list, poses, and levels are pinned as test data.
//!
//! The `0x02` bytes are built by the reference framing builders
//! (`build_world_entity_class_state` / `build_unit_class_state` /
//! `build_stock_unit_class_state` / `build_zone_portal_class_state`) run with
//! the values this port ships: pose + level from `game.pkg`, the constructor
//! flag words (`0x28006` NPC / `0x7` checkpoint / `0x6` portal), the StockUnit
//! constructor defaults (`hero_power = 3600`, `npc_scale = 100`), `fb = 0`, and
//! the portal `Width`/`Height` from the placement or, when the world states
//! neither (`Tutorial.world`), the client archetype's constructor default
//! (`ZonePortal::ZonePortal` @0x5B0AB0: `0x28`/`0x28`).

use std::path::{Path, PathBuf};

use drf_content::{
    derive_merchant_tabs, derive_npc_base_manipulator, derive_world_entities,
    generate_generator_items, generate_merchant_items, Archive, Corpus, PlacedKind, QuestCorpus,
};
use drf_proto::{
    npc_behavior_class_state, npc_component_record, npc_manipulators_class_state,
    npc_modifiers_class_state, placed_entity_class_state, CHECKPOINT_WORLD_ENTITY_FLAGS,
    NPC_BEHAVIOR_CLASS, NPC_HERO_POWER, NPC_MANIPULATORS_CLASS, NPC_MODIFIERS_CLASS, NPC_SCALE,
    NPC_WORLD_ENTITY_FLAGS, PORTAL_DEFAULT_HEIGHT, PORTAL_DEFAULT_WIDTH, PORTAL_WORLD_ENTITY_FLAGS,
    WORLD_ENTITY_FLAGS_BASE,
};

const CHECKPOINT: &str = "world.checkpoints.TutorialCheckpointEntity";
const PORTAL: &str = "misc.ZonePortal_agg";

/// `(classname, pose x256, level)` in the world walk's create order.
const EXPECTED: [(&str, [u32; 4], Option<u8>); 10] = [
    (CHECKPOINT, [195072, 74240, 10240, 23040], None),
    (PORTAL, [245760, 184064, 15360, 0], None),
    (
        "world.tutorial.npc.Recruiter1",
        [215040, 113920, 10240, 0],
        Some(5),
    ),
    (
        "world.tutorial.npc.Recruiter2",
        [214528, 136960, 10240, 46080],
        Some(10),
    ),
    (
        "world.tutorial.npc.HermitVendor",
        [183296, 173824, 10240, 46080],
        Some(1),
    ),
    (
        "world.tutorial.npc.Adventurer1",
        [211200, 126720, 10240, 86528],
        Some(1),
    ),
    (
        "world.tutorial.npc.Adventurer2",
        [204800, 126464, 10240, 76800],
        Some(2),
    ),
    (
        "world.tutorial.npc.Adventurer3",
        [198400, 125440, 10240, 71680],
        Some(3),
    ),
    (
        "world.tutorial.npc.Guard",
        [215040, 176640, 10240, 46080],
        Some(10),
    ),
    (
        "world.tutorial.npc.HelperNoobosaur00",
        [186880, 123136, 10240, 0],
        Some(10),
    ),
];

/// The exact `0x02` class-state bytes per class, same order as [`EXPECTED`].
const EXPECTED_STATE: [&str; 10] = [
    "0700000000fa02000022010000280000005a000000",
    "0600000000c0030000cf0200003c000000000000005374617274310064756e67656f6e30305f6c6576656c303100280028000000ffff",
    "068002000048030000bd0100002800000000000000000500000000000000100e64000000000000000048030000bd010000280000",
    "0680020000460300001702000028000000b4000000000a00000000000000100e6400000000000000004603000017020000280000",
    "0680020000cc020000a702000028000000b4000000000100000000000000100e640000000000000000cc020000a7020000280000",
    "068002000039030000ef0100002800000052010000000100000000000000100e64000000000000000039030000ef010000280000",
    "068002000020030000ee010000280000002c010000000200000000000000100e64000000000000000020030000ee010000280000",
    "068002000007030000ea0100002800000018010000000300000000000000100e64000000000000000007030000ea010000280000",
    "068002000048030000b202000028000000b4000000000a00000000000000100e64000000000000000048030000b2020000280000",
    "0680020000da020000e10100002800000000000000000a00000000000000100e640000000000000000da020000e1010000280000",
];

#[test]
fn tutorial_walk_matches_the_derived_entity_set() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entities = derive_world_entities(&archive, "tutorial").expect("world walk");

    assert_eq!(entities.len(), EXPECTED.len(), "tutorial create set size");
    for (entity, (classname, pose, level)) in entities.iter().zip(EXPECTED.iter()) {
        assert_eq!(&entity.classname, classname);
        assert_eq!(&entity.pose, pose, "{classname} pose");
        assert_eq!(entity.level, *level, "{classname} level");
    }

    assert_eq!(entities[0].kind, PlacedKind::Checkpoint);
    assert_eq!(entities[1].kind, PlacedKind::Portal);
    assert!(entities[2..].iter().all(|e| e.kind == PlacedKind::Npc));

    let portal = &entities[1];
    assert_eq!(portal.spawn_point.as_deref(), Some("Start1"));
    assert_eq!(portal.zone.as_deref(), Some("dungeon00_level01"));
    assert_eq!(portal.color, Some(0xFFFF_0000));
    // `Tutorial.world` states no Width/Height on its portal; the class-state
    // builder supplies the client archetype default.
    assert_eq!(portal.width, None);
    assert_eq!(portal.height, None);
}

/// A portal's `Width`/`Height` are a class-state field the client renders and
/// collides from; the archetype's constructor default (`0x28`) is what a
/// placement stating neither ships. The bytes are the reference framing builder
/// `messages.build_zone_portal_class_state(0x6, x, y, z, 0, "Start1",
/// "dungeon00_level01", 40, 40, 0xFFFF0000, fb=0)`.
#[test]
fn portal_class_state_ships_the_archetype_default_size() {
    assert_eq!(PORTAL_DEFAULT_WIDTH, 0x28);
    assert_eq!(PORTAL_DEFAULT_HEIGHT, 0x28);

    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entities = derive_world_entities(&archive, "tutorial").expect("world walk");
    let portal = entities
        .iter()
        .find(|e| e.classname == PORTAL)
        .expect("tutorial portal");
    let bytes = placed_entity_class_state(portal).unwrap();
    // The tail is `spawn NUL` + `zone NUL` + u16 Width + u16 Height + u32 Color.
    assert_eq!(
        &bytes[bytes.len() - 8..bytes.len() - 4],
        &[0x28, 0x00, 0x28, 0x00]
    );
    assert_eq!(&bytes[bytes.len() - 4..], &0xFFFF_0000u32.to_le_bytes());
    assert_eq!(
        &bytes[bytes.len() - 2..],
        &[0xFF, 0xFF],
        "red, and non-zero"
    );
}

#[test]
fn tutorial_class_state_bytes_match_the_derived_framing() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entities = derive_world_entities(&archive, "tutorial").expect("world walk");
    assert_eq!(entities.len(), EXPECTED_STATE.len());

    for (entity, expected) in entities.iter().zip(EXPECTED_STATE.iter()) {
        let bytes = placed_entity_class_state(entity).expect("class state");
        assert_eq!(bytes, hex(expected), "{} class state", entity.classname);
        let flags = &bytes[0..4];
        let want = match entity.kind {
            PlacedKind::Checkpoint => CHECKPOINT_WORLD_ENTITY_FLAGS,
            PlacedKind::Portal => PORTAL_WORLD_ENTITY_FLAGS,
            PlacedKind::Npc => NPC_WORLD_ENTITY_FLAGS,
            PlacedKind::WorldEntity => {
                if entity.blocking {
                    WORLD_ENTITY_FLAGS_BASE | 1
                } else {
                    WORLD_ENTITY_FLAGS_BASE
                }
            }
            PlacedKind::WorldEntityUnit => {
                drf_proto::WORLD_ENTITY_FLAGS_BASE
                    | drf_proto::UNIT_FLAGS
                    | u32::from(entity.blocking)
            }
            PlacedKind::Door => drf_proto::WORLD_ENTITY_FLAGS_BASE | u32::from(entity.blocking),
        };
        assert_eq!(flags, &want.to_le_bytes(), "{} flags", entity.classname);
    }
}

#[test]
fn npc_class_state_uses_constructor_defaults_not_capture_values() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entities = derive_world_entities(&archive, "tutorial").expect("world walk");
    let npc = entities
        .iter()
        .find(|e| e.classname == "world.tutorial.npc.Recruiter1")
        .expect("Recruiter1");
    let bytes = placed_entity_class_state(npc).unwrap();
    assert_eq!(bytes.len(), 52, "fb = 0 keeps the 52-byte form");
    assert_eq!(&bytes[0..4], &NPC_WORLD_ENTITY_FLAGS.to_le_bytes());
    assert_eq!(bytes[20], 0x00, "fb gate byte is clear (capture used 0x02)");
    assert_eq!(bytes[21], 0x00, "unitfb gate byte is clear");
    assert_eq!(bytes[22], 5, "level is the class's own");
    // The StockUnit tail starts at 27.
    assert_eq!(bytes[27], 0, "hero_respec ctor default (capture used 8)");
    assert_eq!(
        &bytes[30..32],
        &NPC_HERO_POWER.to_le_bytes(),
        "hero_power = 3600"
    );
    assert_eq!(bytes[32], NPC_SCALE, "npc_scale = 100");
}

#[test]
fn npc_components_match_the_derived_forms() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let mut corpus = Corpus::new(archive);
    let manipulator = derive_npc_base_manipulator(&mut corpus)
        .expect("npc/Base.gc")
        .expect("one manipulator");
    assert_eq!(manipulator.fqn, "npc.Base.Manipulators.PrimaryWeapon");
    assert_eq!(manipulator.id, 10);

    let manipulators = npc_manipulators_class_state(&manipulator.fqn, manipulator.id);
    assert_eq!(
        manipulators,
        hex("0101ff6e70632e426173652e4d616e6970756c61746f72732e5072696d617279576561706f6e000a000000ffff010000000000000000")
    );
    assert_eq!(manipulators.len(), 54);

    let modifiers = npc_modifiers_class_state();
    assert_eq!(modifiers, hex("01000000000000000000"));
    assert_eq!(modifiers.len(), 10);

    // Recruiter1: heading 0, pose (215040, 113920).
    let behavior = npc_behavior_class_state([215040, 113920, 10240, 0]).unwrap();
    assert_eq!(
        behavior,
        hex("0100000000000000000000000000010000002e00000000000000000000000048030000bd0100")
    );
    assert_eq!(behavior.len(), 38);

    // The full records: `[0x32][owner][cid][0xFF][class][NUL]` + class state.
    let owner = 0x102b_u16;
    let records = [
        (0xE010_u16, NPC_MANIPULATORS_CLASS, manipulators),
        (0xE011, NPC_BEHAVIOR_CLASS, behavior),
        (0xE012, NPC_MODIFIERS_CLASS, modifiers),
    ];
    for (cid, class, state) in &records {
        let record = npc_component_record(owner, *cid, class, state).unwrap();
        assert_eq!(record, expected_record(owner, *cid, class, state));
        assert_eq!(record[0], 0x32);
        assert_eq!(&record[1..3], &owner.to_le_bytes());
        assert_eq!(&record[3..5], &cid.to_le_bytes());
        assert_eq!(record[5], 0xFF);
        assert_eq!(&record[record.len() - state.len()..], state.as_slice());
    }
}

/// The state-less Behavior omits the capture-measured StateMachine list: the
/// gate bit `0x10` is clear, so the payload is 38 bytes, not the capture's 76.
#[test]
fn npc_behavior_is_the_state_less_substitute() {
    let behavior = npc_behavior_class_state([215040, 113920, 10240, 46080]).unwrap();
    assert_eq!(behavior.len(), 38);
    assert_ne!(behavior[18], 0x3E, "StateMachine gate is not the capture's");
    assert_eq!(behavior[18] & 0x10, 0, "StateMachine list gate is clear");
    assert_eq!(behavior[18], 0x2E);
    // UnitMover gate is neutral and the heading rides twice.
    assert_eq!(behavior[5], 0x00);
    assert_eq!(&behavior[6..10], &46080u32.to_le_bytes());
    assert_eq!(&behavior[10..14], &46080u32.to_le_bytes());
    // MonsterBehavior2 tail restates the X/Y pose.
    assert_eq!(behavior[29], 0x00);
    assert_eq!(&behavior[30..34], &215040u32.to_le_bytes());
    assert_eq!(&behavior[34..38], &113920u32.to_le_bytes());
}

/// The town walk must create every network NPC, including the three trainers
/// whose `Level` only their base class states. `misc.Waypoint` stays out: the
/// client owns its pathing anchors, and creating one as a world entity crashes
/// the join (the `Unit`-less entity's `getUnit` answers NULL).
#[test]
fn town_walk_creates_all_network_npcs_with_chain_levels() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entities = derive_world_entities(&archive, "town").expect("world walk");
    let npcs: Vec<_> = entities
        .iter()
        .filter(|e| e.kind == PlacedKind::Npc)
        .collect();
    assert_eq!(npcs.len(), 24, "town network NPCs");
    let level = |suffix: &str| {
        npcs.iter()
            .find(|e| e.classname.ends_with(suffix))
            .and_then(|e| e.level)
    };
    assert_eq!(level("TrainerFighter"), Some(30));
    assert_eq!(level("TrainerMage"), Some(20));
    assert_eq!(level("TrainerRanger"), Some(20));
    assert!(
        !entities
            .iter()
            .any(|e| e.classname.eq_ignore_ascii_case("misc.Waypoint")),
        "waypoints stay client-side"
    );
}

/// A generated tab (`AutoGenerateItems = true`) authors no item list; its
/// `ItemGenerator` chain still names concrete item classes, and the tab's
/// level window selects the phases. The shelf must not be empty.
#[test]
fn generated_weapon_tab_draws_stock_from_its_item_generator() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let mut corpus = Corpus::new(archive);
    let tabs = derive_merchant_tabs(&mut corpus, "world.town.npc.VendorWeapon1")
        .expect("vendor content")
        .expect("Merchant block");
    let tab = tabs
        .iter()
        .find(|tab| tab.auto_generate && tab.name == "Weapons")
        .expect("generated Weapons tab");
    assert!(tab.items.is_empty(), "the .gc authors no list");
    let items = generate_merchant_items(&mut corpus, tab, 1);
    assert!(!items.is_empty(), "generated shelf");
    assert!(items.len() <= 8);
    for item in &items {
        assert!(!item.class.is_empty());
        assert!(
            item.level >= 6 && item.level <= 20,
            "{} level {} outside the tab window",
            item.class,
            item.level
        );
    }
}

/// Algernon's dungeon declares `RespawnZone = town`; the town world's
/// graveyard is the `Respawn` waypoint, not the `Start` waypoint the town
/// states as its own `RespawnSpawnPoint`.
#[test]
fn algernon_respawns_into_the_town_graveyard() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let dungeon = drf_content::zone_def(&archive, "dungeon01_level01")
        .expect("dungeon01_level01.zone")
        .expect("Algernon states a ZoneDef");
    assert_eq!(dungeon.respawn_zone.as_deref(), Some("town"));
    let town = drf_content::zone_def(&archive, "town")
        .expect("town.zone")
        .expect("Townston states a ZoneDef");
    assert_eq!(town.respawn_spawn_point.as_deref(), Some("Start"));
    let graveyard = drf_content::derive_zone_spawn(&archive, "town", Some("Respawn"))
        .expect("town.world")
        .expect("town places a Respawn waypoint");
    assert_eq!(graveyard.rung, "named-waypoint");
    assert_eq!(graveyard.name, "Respawn");
    assert!(graveyard.pose[0] > 0 && graveyard.pose[1] != 0);
}

#[test]
fn the_shipped_starting_checkpoints_are_town_tutorial_and_pvp() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let list = drf_content::starting_checkpoints(&archive).expect("StartingCheckpointList.gc");
    assert_eq!(
        list,
        vec![
            "world.checkpoints.TownCheckpoint".to_string(),
            "world.checkpoints.TutorialCheckpoint".to_string(),
            "world.checkpoints.PVPTownCheckpoint".to_string(),
        ]
    );
}

/// Every obelisk's `Checkpoint` class states its own destination, so the
/// client can travel between remembered obelisks without a synthetic portal.
#[test]
fn obelisk_checkpoints_state_their_own_destination() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let tutorial =
        drf_content::checkpoint_destination(&archive, "world.checkpoints.TutorialCheckpoint")
            .expect("TutorialCheckpoint.gc")
            .expect("the tutorial checkpoint states a Zone");
    assert_eq!(tutorial.zone, "Tutorial");
    assert_eq!(tutorial.spawn, "Waypoint");
    let town = drf_content::checkpoint_destination(&archive, "world.checkpoints.TownCheckpoint")
        .expect("TownCheckpoint.gc")
        .expect("the town checkpoint states a Zone");
    assert_eq!(town.zone, "Town");
    assert_eq!(town.spawn, "Waypoint");
    let spawn = drf_content::derive_zone_spawn(&archive, "tutorial", Some("Waypoint"))
        .expect("tutorial.world")
        .expect("tutorial places its Waypoint");
    assert_eq!(spawn.name, "Waypoint");
    assert_eq!(spawn.rung, "named-waypoint");
}

#[test]
fn the_obelisk_gc_hash_resolves_to_the_tutorial_checkpoint_class() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    assert_eq!(
        drf_content::checkpoint_class_by_hash(&archive, 0xBBBB_4DE0).as_deref(),
        Some("world.checkpoints.TutorialCheckpoint")
    );
    assert_eq!(
        drf_content::checkpoint_class_by_hash(&archive, 0x9217_56F4).as_deref(),
        Some("world.checkpoints.TownCheckpoint")
    );
    assert_eq!(
        drf_content::checkpoint_class_by_hash(&archive, 0x1234_5678),
        None
    );
}

#[test]
fn hand_placed_boss_lair_spawns_its_encounter_table() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let spawns = drf_content::spawns_with_seed(&archive, "dungeon00_level03_boss", 1234)
        .expect("Rattle Tooth's Lair");
    assert!(
        !spawns.is_empty(),
        "the hand-placed base.Encounter markers spawn their EncounterTable"
    );
    assert!(spawns.iter().all(|spawn| spawn.hp_wire > 0));
}

#[test]
fn boss_chest_authors_its_item_generators() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let generators =
        drf_content::nci_item_generators(&archive, "terrain.interactives.loot.BossChest")
            .expect("boss chest generators");
    assert!(generators.contains(&("TreasureChestBossIG".to_string(), 1)));
    assert!(generators.contains(&("DefaultIG".to_string(), 4)));
}

#[test]
fn hand_placed_boss_lair_derives_its_interactives_and_posse() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entities =
        derive_world_entities(&archive, "dungeon00_level03_boss").expect("Rattle Tooth's Lair");
    for classname in [
        "terrain.interactives.loot.BossChest",
        "terrain.interactives.loot.Chest_Boss",
        "world.dungeon00.data.BossGate",
        "world.dungeon00.data.teleport.toTutorial",
        "world.dungeon00.data.NewbieEnduranceShrine",
        "world.dungeon00.npc.NCIPortrait",
    ] {
        let entity = entities
            .iter()
            .find(|entity| entity.classname == classname)
            .unwrap_or_else(|| panic!("{classname} is placed"));
        assert!(
            matches!(
                entity.kind,
                PlacedKind::WorldEntity | PlacedKind::Door | PlacedKind::WorldEntityUnit
            ),
            "{classname}"
        );
    }
    let gate = entities
        .iter()
        .find(|entity| entity.classname == "world.dungeon00.data.BossGate")
        .unwrap();
    assert_eq!(gate.kind, PlacedKind::Door);
    assert_eq!(
        drf_proto::placed_entity_class_state(gate).unwrap().len(),
        23
    );
    let chest = entities
        .iter()
        .find(|entity| entity.classname == "terrain.interactives.loot.BossChest")
        .unwrap();
    assert_eq!(chest.kind, PlacedKind::WorldEntityUnit);
    assert_eq!(
        drf_proto::placed_entity_class_state(chest).unwrap().len(),
        31
    );

    let spawns = drf_content::spawns_with_seed(&archive, "dungeon00_level03_boss", 1234)
        .expect("boss posse");
    for suffix in ["RattleTooth", "Blademaster", "Broodling", "boss_guard"] {
        assert!(
            spawns.iter().any(|spawn| spawn.classname.ends_with(suffix)),
            "{suffix} placeholders become mobs"
        );
    }
}

#[test]
fn generated_room_encounter_world_entity_table_spawns_its_vendor() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entities = drf_content::generated_room_world_entities(&archive, "dungeon16_level03", 1234)
        .expect("generated room entities");
    let vendor = entities
        .iter()
        .find(|entity| entity.classname == "world.dungeon16.npc.Vendor_L03")
        .expect("the merchant room's table selects its vendor");
    assert_eq!(vendor.kind, PlacedKind::Npc);
    assert_eq!(vendor.level, Some(80));
}

/// The repeatable wishing-well quest is the coin->loot feature: one
/// `QuestItemPAL.Token` (King's Coin) with `RemoveOnFinalize`, and two
/// `WishingWellIG` rewards dropped on the ground.
#[test]
fn wishing_well_repeatable_consumes_a_coin_and_draws_loot() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let mut corpus = Corpus::new(archive);
    let quests = QuestCorpus::from_corpus(&mut corpus).expect("quest corpus");
    let quest = quests
        .quest_by_fqn("world.town.quest.well.base_r")
        .expect("well base_r")
        .clone();
    assert!(quest.reward_items_dropped);
    assert_eq!(quest.reward_generator.as_deref(), Some("WishingWellIG"));
    assert_eq!(quest.num_reward_items, 2);
    let coin = quest
        .objectives
        .iter()
        .find(|objective| objective.kind == "ItemObjective")
        .expect("King's Coin objective");
    assert_eq!(coin.item.as_deref(), Some("QuestItemPAL.Token"));
    assert!(coin.remove_on_finalize);
    assert_eq!(coin.required, 1);
    let rewards = generate_generator_items(&mut corpus, "WishingWellIG", 30, 2, 7);
    assert!(
        !rewards.is_empty(),
        "WishingWellIG resolves to concrete items"
    );
    assert!(rewards.iter().all(|(_, level)| *level <= 30));
}

fn expected_record(owner: u16, cid: u16, classname: &str, class_state: &[u8]) -> Vec<u8> {
    let mut out = vec![0x32];
    out.extend_from_slice(&owner.to_le_bytes());
    out.extend_from_slice(&cid.to_le_bytes());
    out.push(0xFF);
    out.extend_from_slice(classname.as_bytes());
    out.push(0);
    out.extend_from_slice(class_state);
    out
}

fn open_archive() -> Option<Archive> {
    let dir = pkg_dir()?;
    Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).ok()
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

fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    assert!(digits.len().is_multiple_of(2), "odd hex length");
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
fn generated_room_quest_nci_ships_the_activator_sentinel() {
    let Some(archive) = open_archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let seed = drf_content::layout_seed("dungeon00_level01");
    let entities = drf_content::generated_room_world_entities(&archive, "dungeon00_level01", seed)
        .expect("generated room entities");
    let lockbox = entities
        .iter()
        .find(|entity| {
            entity
                .classname
                .eq_ignore_ascii_case("world.dungeon00.data.Entity_HermitLockbox")
        })
        .expect("the questfindring tile places the lockbox");
    assert!(lockbox.track_activating_players);
    let state = drf_proto::placed_entity_class_state(lockbox).expect("lockbox class state");
    assert_eq!(state.len(), 35, "31B NCI + zero activator sentinel");
    assert_eq!(&state[state.len() - 4..], &[0, 0, 0, 0]);

    let chest = entities
        .iter()
        .find(|entity| {
            entity
                .classname
                .eq_ignore_ascii_case("terrain.misc.interactives.chest")
        })
        .expect("the loot tile places chests");
    assert!(!chest.track_activating_players);
    assert_eq!(
        drf_proto::placed_entity_class_state(chest)
            .expect("chest class state")
            .len(),
        31
    );
}
