// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-content`'s DERIVED zone destination against the real `game.pkg`.
//!
//! The expected destination is pinned as test data.
//!
//! Output: the portal placement resolves to
//! `{zone: dungeon00_level01, spawn: Start1}` and the checkpoint states no
//! `Zone` at all. Every test skips when the archive is absent, as the sibling
//! content tests do.

use std::path::{Path, PathBuf};

use drf_content::{
    checkpoints_for_zones, derive_generated_zone_start, derive_world_entities, derive_zone_spawn,
    destination_for, generated_return_portal, generated_room_world_entities, layout_seed,
    room_waypoint_position_with_seed, tile_world_entities, zone_def, Archive, PlacedKind,
    GENERATED_RETURN_PORTAL_CLASS, SPAWN_RUNG_NAMED_WAYPOINT, SPAWN_RUNG_SPAWN_CLASS,
    SPAWN_RUNG_ZONE_START_ROOM,
};

const PORTAL: &str = "misc.ZonePortal_agg";
const CHECKPOINT: &str = "world.checkpoints.TutorialCheckpointEntity";

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

fn archive() -> Option<Archive> {
    let (pki, pkg) = pkg_paths()?;
    Some(Archive::open(&pki, &pkg).expect("open game.pki/game.pkg"))
}

#[test]
fn the_tutorial_portal_designates_the_generated_dungeon() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let dest = destination_for(&archive, "tutorial", None, Some(PORTAL), None)
        .expect("read tutorial.world")
        .expect("the portal's own record states a Zone");
    assert_eq!(dest.zone, "dungeon00_level01");
    assert_eq!(dest.spawn, "Start1");
}

#[test]
fn the_tutorial_checkpoint_states_no_destination() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // The checkpoint is placed in the same world, and its own record states
    // no `Zone`: `None` is the ordinary answer for a non-portal, not a
    // failure.
    assert!(
        destination_for(&archive, "tutorial", None, Some(CHECKPOINT), None)
            .expect("read tutorial.world")
            .is_none()
    );
}

#[test]
fn a_generated_worlds_destination_is_its_start_rooms_return_leg() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // `dungeon00_level01.world` has no `Entities` block, so there is no
    // clicked record to ask; the return leg is the start room's own
    // `LinkToZone = tutorial` / `LinkToSpawn = start1`.
    let dest = destination_for(
        &archive,
        "dungeon00_level01",
        None,
        Some(GENERATED_RETURN_PORTAL_CLASS),
        None,
    )
    .expect("read dungeon00_level01.world")
    .expect("the generated start room states its return leg");
    assert_eq!(dest.zone, "tutorial");
    assert_eq!(dest.spawn, "start1");

    // A non-portal click in the same world never travels.
    assert!(
        destination_for(&archive, "dungeon00_level01", None, Some(CHECKPOINT), None)
            .expect("read dungeon00_level01.world")
            .is_none()
    );
}

/// The GENERATED zone's synthesized return portal: a `misc.ZonePortal_hub` at
/// the maze entry cell's own `misc.ZonePortal*` anchor -- cell (0,4) origin
/// (-800,800) plus the entry tile's `misc.zoneportal_hub` local (160,147,30),
/// carrying the start room's `LinkToZone`/`LinkToSpawn` link.
#[test]
fn a_generated_zone_ships_a_synthesized_return_portal() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let portal = generated_return_portal(&archive, "dungeon00_level01")
        .expect("read dungeon00_level01.world")
        .expect("a generated world synthesizes a return portal");
    assert_eq!(portal.classname, "misc.ZonePortal_hub");
    assert_eq!(portal.kind, PlacedKind::Portal);
    assert_eq!(
        portal.pose,
        [(-640i64 * 256) as u32, 947 * 256, 30 * 256, 0]
    );
    assert_eq!(portal.zone.as_deref(), Some("tutorial"));
    assert_eq!(portal.spawn_point.as_deref(), Some("start1"));
    assert_eq!(portal.color, Some(0x8200_ADFF));
    // The archetype's constructor defaults size the quad, not the placement.
    assert_eq!(portal.width, None);
    assert_eq!(portal.height, None);
}

/// `derive_world_entities` for the generated dungeon yields exactly the one
/// synthesized return portal; a hand-placed zone is unchanged.
#[test]
fn derive_world_entities_includes_the_generated_return_portal() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let entities = derive_world_entities(&archive, "dungeon00_level01").expect("dungeon walk");
    assert_eq!(entities.len(), 1, "the generated world ships one portal");
    assert_eq!(entities[0].classname, GENERATED_RETURN_PORTAL_CLASS);
    assert_eq!(entities[0].kind, PlacedKind::Portal);
    assert_eq!(entities[0].zone.as_deref(), Some("tutorial"));
    assert_eq!(entities[0].spawn_point.as_deref(), Some("start1"));

    // A hand-placed zone keeps the hand-placed path: no synthesized portal,
    // and the tutorial's own `misc.ZonePortal_agg` is unchanged.
    assert!(generated_return_portal(&archive, "tutorial")
        .expect("read tutorial.world")
        .is_none());
}

/// The expected value is pinned as test data.
///
/// Output: `zone-start-room start1 None`. The NAME resolves and the coordinate
/// is the client's; the graph below is the same `game.pkg` statement rendered
/// into the x256 wire domain by the room's grid cell and the world's
/// `TileSize`.
#[test]
fn the_generated_dungeon_start_is_derived_from_its_room_grid() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // The reference: the NAME and rung, with no entity/coordinate.
    assert!(
        derive_zone_spawn(&archive, "dungeon00_level01", Some("Start1"))
            .expect("read dungeon00_level01.world")
            .is_none()
    );
    // The derived start: `base.StartRoom` carries `SpawnName = start1`,
    // `GridY = 4`; the world states `MazeWidth = 4`, `MazeHeight = 5`; its
    // archetype `base.World_elmforest` states `TileSize = 400`.
    let start = derive_generated_zone_start(&archive, "dungeon00_level01", Some("Start1"))
        .expect("read dungeon00_level01.world")
        .expect("a generated world with a ZoneStart room resolves");
    assert_eq!(start.rung, SPAWN_RUNG_ZONE_START_ROOM);
    assert_eq!(start.name, "start1");
    // The maze port's entry: cell (0,4) origin (-800,800) plus the tile's
    // authored `misc.Waypoint SpawnPoint` (150,70,10, heading 180), in x256.
    assert_eq!(
        start.pose,
        [(-650i64 * 256) as u32, 870 * 256, 10 * 256, 180 * 256]
    );
}

#[test]
fn a_hand_placed_zone_is_not_reported_as_generated() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // `tutorial.world` is hand-placed and its own `misc.Waypoint Start` wins.
    assert!(
        derive_generated_zone_start(&archive, "tutorial", Some("Start1"))
            .expect("read tutorial.world")
            .is_none()
    );
}

/// `Width`/`Height` are the portal's own when its world states them. The
/// tutorial's placement states neither, so the class-state builder supplies the
/// archetype default; `town.world`'s `misc.ZonePortal_agg` states `70`/`60`.
/// The shipped dev/test ZoneDefs state these facts; the Rust reader is the
/// same read the reference `ZoneCorpus.zone` performs.
#[test]
fn dev_zone_defs_carry_their_shipped_facts() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };

    let debug = zone_def(&archive, "debug")
        .expect("read debug.zone")
        .expect("debug ZoneDef");
    assert_eq!(debug.name, "debug");
    assert_eq!(debug.label.as_deref(), Some("Debug Hub"));
    assert!(!debug.private);
    assert_eq!(debug.respawn_zone.as_deref(), Some("TheHub"));
    assert_eq!(debug.respawn_spawn_point, None);
    assert_eq!(debug.min_level, Some(1));
    assert_eq!(debug.max_level, Some(100));
    assert_eq!(debug.source, "debug.zone");

    let bug_hub = zone_def(&archive, "BugHub")
        .expect("read BugHub.zone")
        .expect("BugHub ZoneDef");
    assert_eq!(bug_hub.name, "BugHub");
    assert_eq!(bug_hub.label.as_deref(), Some("Bug Hub"));
    assert!(!bug_hub.private);
    assert_eq!(bug_hub.respawn_zone.as_deref(), Some("TheHub"));
    assert_eq!(bug_hub.respawn_spawn_point.as_deref(), Some("Start"));
    assert_eq!(bug_hub.min_level, None);
    assert_eq!(bug_hub.max_level, None);

    let nci_hub = zone_def(&archive, "TestNCIHub")
        .expect("read TestNCIHub.zone")
        .expect("TestNCIHub ZoneDef");
    assert_eq!(nci_hub.name, "TestNCIHub");
    assert_eq!(nci_hub.label.as_deref(), Some("NCI Hub"));
    assert!(!nci_hub.private);
    assert_eq!(nci_hub.respawn_zone.as_deref(), Some("TheHub"));

    let nci_test01 = zone_def(&archive, "nci_test01")
        .expect("read nci_test01.zone")
        .expect("nci_test01 ZoneDef");
    assert_eq!(nci_test01.name, "nci_test01");
    assert_eq!(nci_test01.label.as_deref(), Some("World Def NCI Test"));
    assert!(nci_test01.private);
    assert_eq!(nci_test01.respawn_zone.as_deref(), Some("BugHub"));
    assert_eq!(nci_test01.min_level, Some(1));
    assert_eq!(nci_test01.max_level, Some(100));

    let mobs = zone_def(&archive, "TestMobsCell")
        .expect("read TestMobsCell.zone")
        .expect("TestMobsCell ZoneDef");
    assert_eq!(mobs.name, "TestMobsCell");
    assert_eq!(mobs.label.as_deref(), Some("Creature Hub"));
    assert!(!mobs.private);
    assert_eq!(mobs.respawn_zone.as_deref(), Some("TheHub"));
    assert!(!mobs.is_town);
}

/// A dev zone describes itself, not the town: `Name` is the requested zone's
/// own, and the source is its own `.zone` entry.
#[test]
fn dev_zones_describe_themselves_not_the_town() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    for zone in ["debug", "BugHub", "TestNCIHub", "TestMobsCell"] {
        let def = zone_def(&archive, zone)
            .unwrap_or_else(|error| panic!("read {zone}.zone: {error}"))
            .unwrap_or_else(|| panic!("{zone} ZoneDef"));
        assert_eq!(def.name.to_ascii_lowercase(), zone.to_ascii_lowercase());
        assert_eq!(def.source, format!("{zone}.zone"));
        assert!(!def.is_town);
    }
}

/// `debug.world` places a `Start` waypoint; the ZoneDef itself states no
/// `RespawnSpawnPoint`, so the default-start rung answers.
#[test]
fn the_debug_zone_resolves_a_spawn() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let spawn = derive_zone_spawn(&archive, "debug", None)
        .expect("read debug.world")
        .expect("debug places a Start waypoint");
    assert_eq!(spawn.name, "Start");
}

/// `TestNCIHub.world` spells its Start waypoint `misc.waypoint` (lower-case).
/// The client resolves archetype names case-insensitively, so the spawn chain
/// must too; a case-sensitive compare refuses a zone whose own file states a
/// Start. This is the Rust half of the Python fix in `zone_corpus.py`.
#[test]
fn the_lower_case_waypoint_class_still_resolves_a_spawn() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let spawn = derive_zone_spawn(&archive, "TestNCIHub", None)
        .expect("read TestNCIHub.world")
        .expect("TestNCIHub places a lower-case Start waypoint");
    assert_eq!(spawn.name, "Start");
}

fn portal_destinations(archive: &Archive, zone: &str) -> Vec<String> {
    derive_world_entities(archive, zone)
        .unwrap_or_else(|error| panic!("walk {zone}.world: {error}"))
        .into_iter()
        .filter(|entity| entity.kind == PlacedKind::Portal)
        .map(|entity| {
            entity
                .zone
                .unwrap_or_else(|| panic!("{zone} places a portal with no Zone"))
        })
        .collect()
}

/// `BugHub.world` places five portals, one per destination.
#[test]
fn bug_hub_exposes_five_portal_links() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    assert_eq!(
        portal_destinations(&archive, "BugHub"),
        [
            "TheHub",
            "pathtest01",
            "nci_test01",
            "nci_test02",
            "nci_test03"
        ]
    );
}

/// `TestMobsCell.world` places ten portals.
#[test]
fn test_mobs_cell_exposes_ten_portal_links() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    assert_eq!(portal_destinations(&archive, "TestMobsCell").len(), 10);
}

#[test]
fn a_portals_stated_size_is_derived_from_its_own_world() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let tutorial = derive_world_entities(&archive, "tutorial").expect("tutorial walk");
    let portal = tutorial
        .iter()
        .find(|entity| entity.classname == PORTAL)
        .expect("tutorial portal");
    assert_eq!(portal.width, None, "Tutorial.world states no Width");
    assert_eq!(portal.height, None, "Tutorial.world states no Height");

    let town = derive_world_entities(&archive, "town").expect("town walk");
    let dungeon_portal = town
        .iter()
        .find(|entity| entity.classname == PORTAL)
        .expect("town misc.ZonePortal_agg");
    assert_eq!(dungeon_portal.width, Some(70));
    assert_eq!(dungeon_portal.height, Some(60));
}

#[test]
fn generated_room_tile_entities_include_the_quest_lockbox() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let seed = drf_content::layout_seed("dungeon00_level01");
    let entities = drf_content::generated_room_world_entities(&archive, "dungeon00_level01", seed)
        .expect("derive generated room entities");
    let lockbox = entities
        .iter()
        .find(|entity| {
            entity
                .classname
                .eq_ignore_ascii_case("world.dungeon00.data.Entity_HermitLockbox")
        })
        .expect("elmforest_questfindring_* places Samford's Lockbox");
    assert_eq!(lockbox.kind, PlacedKind::WorldEntityUnit);
    assert_eq!(
        lockbox.pose,
        [355 * 256, (400 + 171) * 256, 10 * 256, 120 * 256]
    );
    assert!(lockbox.heading_stated);
    assert!(
        entities
            .iter()
            .filter(|entity| entity
                .classname
                .eq_ignore_ascii_case("terrain.misc.interactives.chest"))
            .count()
            == 2,
        "tutorial_loot_* places two ordinary chests"
    );
}

#[test]
fn generated_room_encounter_markers_are_not_world_entities() {
    let Some(archive) = archive() else {
        return;
    };
    let seed = drf_content::layout_seed("dungeon00_level01");
    let entities =
        drf_content::generated_room_world_entities(&archive, "dungeon00_level01", seed).unwrap();
    assert!(
        entities.iter().all(|entity| {
            !entity
                .classname
                .rsplit('.')
                .next()
                .unwrap_or(&entity.classname)
                .to_ascii_lowercase()
                .starts_with("encounter")
        }),
        "base.Encounter<N> tile markers are spawned by the mob path"
    );
    assert!(entities
        .iter()
        .filter(|entity| entity
            .classname
            .eq_ignore_ascii_case("terrain.misc.interactives.chest"))
        .all(|entity| entity.kind == PlacedKind::WorldEntityUnit));
}

#[test]
fn dungeon01_generated_rooms_include_chests_as_nci_interactives() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let seed = layout_seed("dungeon01_level01");
    let entities = generated_room_world_entities(&archive, "dungeon01_level01", seed)
        .expect("generated room entities");
    assert!(
        entities.iter().any(|entity| {
            entity.kind == PlacedKind::WorldEntityUnit
                && entity.classname.to_ascii_lowercase().contains("chest")
        }),
        "the loot room table selects a chest: {:?}",
        entities
            .iter()
            .map(|e| (&e.classname, e.kind))
            .collect::<Vec<_>>()
    );
}

#[test]
fn loot_tile_chests_are_networked_world_entities() {
    let Some((pki, pkg)) = pkg_paths() else {
        eprintln!("skip: no game.pkg");
        return;
    };
    let archive = Archive::open(&pki, &pkg).expect("open content");
    let chests: Vec<_> = tile_world_entities(&archive, "cave_small_loot_1e_A")
        .expect("loot tile entities")
        .into_iter()
        .filter(|entity| entity.classname.to_ascii_lowercase().contains("chest"))
        .collect();
    assert_eq!(
        chests.len(),
        2,
        "the loot tile bakes two chests: {chests:?}"
    );
}

#[test]
fn generated_minimap_matches_the_client_ready_count() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    for (zone, expected_words) in [("dungeon01_level01", 17usize), ("dungeon00_level01", 18)] {
        let layout = drf_content::maze::generate_layout(&archive, zone)
            .expect("read generated world")
            .expect("generated maze");
        let minimap = drf_content::minimap_for_layout(&layout).expect("minimap geometry");
        assert_eq!(
            minimap.words(),
            expected_words,
            "{zone}: MiniMapExplored::ReadExploredBits expects this many u32 words"
        );
        assert_eq!(minimap.words(), minimap.rows * minimap.words_per_row);
    }
}

#[test]
fn minimap_marks_the_players_cell_and_neighbours() {
    let minimap = drf_content::maze::MazeMinimap {
        min_x: -128.0,
        min_y: -128.0,
        columns: 40,
        rows: 3,
        words_per_row: 2,
    };
    let mut words = vec![0u32; minimap.words()];
    minimap.mark_explored(&mut words, 0.0, 0.0);
    assert_eq!(words.len(), 6);
    assert!(
        words.iter().any(|word| *word != 0),
        "the current cell is marked"
    );
    assert_eq!(words.iter().map(|word| word.count_ones()).sum::<u32>(), 9);
}

#[test]
fn off1a_does_not_ship_placeholder_or_empty_encounter_entities() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let zone = "dungeon01_level01_off1a";
    let seed = drf_content::layout_seed(zone);
    let mut entities = drf_content::derive_world_entities(&archive, zone).unwrap();
    entities.extend(drf_content::generated_room_portals(&archive, zone, seed).unwrap());
    entities.extend(drf_content::generated_room_world_entities(&archive, zone, seed).unwrap());
    for entity in &entities {
        let lower = entity.classname.to_ascii_lowercase();
        assert!(
            !lower.starts_with("base.placeholder"),
            "placeholder must be resolved through its table, not shipped: {}",
            entity.classname
        );
        assert!(
            lower != "base.encounter_empty",
            "empty encounter marker must not be a world entity: {}",
            entity.classname
        );
    }
    assert!(
        entities
            .iter()
            .any(|entity| entity.classname.contains("Chest_")),
        "off1a still ships its baked chests"
    );
}

#[test]
fn visited_zones_map_to_the_shipped_obelisk_checkpoints() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let zones = vec![
        "dungeon01_level05".to_string(),
        "dungeon01_level04_off1a".to_string(),
        "Dungeon09_level00".to_string(),
        "Town".to_string(),
        "not_a_real_zone".to_string(),
    ];
    let checkpoints = checkpoints_for_zones(&archive, &zones).expect("checkpoints");
    // dungeon01_level05 and its off1a dedupe to the one dungeon01 checkpoint;
    // dungeon09 maps to the offset-named Dungeon08Checkpoint (its
    // `Description.Zone` is Dungeon09_level00); town is TownCheckpoint.
    assert!(checkpoints
        .iter()
        .any(|class| class == "world.checkpoints.Dungeon01Checkpoint"));
    assert!(checkpoints
        .iter()
        .any(|class| class == "world.checkpoints.Dungeon08Checkpoint"));
    assert!(checkpoints
        .iter()
        .any(|class| class == "world.checkpoints.TownCheckpoint"));
    assert_eq!(
        checkpoints
            .iter()
            .filter(|class| class.eq_ignore_ascii_case("world.checkpoints.Dungeon01Checkpoint"))
            .count(),
        1,
        "duplicate visited levels collapse to one row"
    );
    assert!(
        !checkpoints
            .iter()
            .any(|class| class.to_ascii_lowercase().contains("not_a_real")),
        "an unknown zone contributes no row"
    );
}

#[test]
fn pvp_start_is_a_zone_and_the_cut_pvp_town_is_not() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let pwnston = zone_def(&archive, "pvp_start")
        .expect("read pvp_start.zone")
        .expect("pvp_start is a shipped ZoneDef");
    assert_eq!(pwnston.label.as_deref(), Some("Pwnston"));
    assert!(
        zone_def(&archive, "pvp_town")
            .expect("probe pvp_town")
            .is_none(),
        "the AdminUI's pvp_town name is cut content; the live zone is pvp_start"
    );
}

#[test]
fn sissirat_lair_spawns_on_its_start_room_waypoint() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // The boss world's entry is `misc.Waypoint_StartRoom` named `boss_spawn`
    // at 190,240,30 heading 90. The level07 portal requests `boss_spawn`, and
    // a spawn-less entry (obelisk/return) must fall back to the same room.
    let requested = derive_zone_spawn(&archive, "dungeon01_level08_boss", Some("boss_spawn"))
        .expect("read boss world")
        .expect("boss_spawn resolves");
    assert_eq!(requested.name, "boss_spawn");
    assert_eq!(requested.pose, [190 * 256, 240 * 256, 30 * 256, 90 * 256]);
    assert_eq!(requested.rung, SPAWN_RUNG_NAMED_WAYPOINT);

    let fallback = derive_zone_spawn(&archive, "dungeon01_level08_boss", None)
        .expect("read boss world")
        .expect("start-room fallback resolves");
    assert_eq!(fallback.name, "boss_spawn");
    assert_eq!(fallback.rung, SPAWN_RUNG_ZONE_START_ROOM);
}

#[test]
fn pvp_practice_match_resolves_its_ffa_spawn_class() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // PVPGroupPracticeMatch names its respawn point `Start` and the Pwnston
    // teleporter asks for `pvp`, but its world authors `wp_ffa_start` /
    // `wp_ffa_respawn` waypoint subclasses instead of a `Start` placement.
    // The join must land on one of those authored points, never (0,0).
    let requested = derive_zone_spawn(&archive, "PVPGroupPracticeMatch", Some("pvp"))
        .expect("read practice match world")
        .expect("ffa start waypoint resolves");
    assert_eq!(requested.name, "ffa_start");
    assert_eq!(requested.rung, SPAWN_RUNG_SPAWN_CLASS);
    assert_ne!(requested.pose[0], 0);
    assert_ne!(requested.pose[1], 0);

    let fallback = derive_zone_spawn(&archive, "PVPGroupPracticeMatch", None)
        .expect("read practice match world")
        .expect("zone default resolves");
    assert_eq!(fallback.name, "ffa_start");
}

#[test]
fn pvp_practice_waypoints_are_not_network_entities() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // PVPGroupPracticeMatch places only terrain plus `wp_ffa_start` /
    // `wp_ffa_respawn` pathing anchors. Those are client-side waypoints, not
    // CEM entities; shipping one corrupts the destination join (client reads
    // a class-state byte as top-level CEM type 49).
    let entities = derive_world_entities(&archive, "PVPGroupPracticeMatch")
        .expect("read practice match world");
    assert!(
        entities.is_empty(),
        "waypoint placements must not become networked entities: {entities:?}"
    );
}

#[test]
fn generated_offshoot_exit_waypoint_names_a_room_position() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // Q04_a2's "Explored the Mutant Operations Center" targets the generated
    // room node's WaypointName, not a placed entity. The position must resolve
    // from the maze layout so proximity credit can fire.
    let seed = layout_seed("dungeon01_level05_off1a");
    let position = room_waypoint_position_with_seed(
        &archive,
        "dungeon01_level05_off1a",
        "level05_off1a_teleporter",
        seed,
    )
    .expect("read generated layout")
    .expect("exit waypoint is placed");
    assert!(position[0].is_finite() && position[1].is_finite());
    assert_ne!(position, [0.0, 0.0]);
}
