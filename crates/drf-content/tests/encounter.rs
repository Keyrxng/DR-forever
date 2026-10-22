// SPDX-License-Identifier: GPL-3.0-or-later

//! The generated-dungeon mob spawner against the real `game.pkg`.
//!
//! Pins `dungeon00_level01`'s spawn count, class distribution, per-mob level
//! and x256 wire HP, and one mob's exact pose. Every test skips when the
//! archive is absent, as the sibling content tests do.

use std::path::{Path, PathBuf};

use drf_content::{spawns, spawns_with_seed_reachable, Archive};

const MELEE01: &str = "world.dungeon00.mob.melee01.rank1";
const MELEE02: &str = "world.dungeon00.mob.melee02.rank1";
const MELEE03: &str = "world.dungeon00.mob.melee03.rank1";

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
fn dungeon00_level01_spawns_match_the_reference() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let list = spawns(&archive, "dungeon00_level01").expect("read dungeon00_level01");

    // The reference `generate_spawns` yields 46 mobs: 22 Pups, 18 Ratlings and
    // 6 Grunts.
    assert_eq!(list.len(), 46, "spawn count");
    let count = |class: &str| list.iter().filter(|spawn| spawn.classname == class).count();
    assert_eq!(count(MELEE01), 22, "melee01 count");
    assert_eq!(count(MELEE03), 18, "melee03 count");
    assert_eq!(count(MELEE02), 6, "melee02 count");

    // Every mob is level 2; the wire HP is the client-derived
    // `getMaxHealth(level)<<8` (29184 for the RECRUIT Pup/Ratling, 58624 for
    // the VETERAN Grunt leader).
    for spawn in &list {
        assert_eq!(spawn.level, 2, "{} level", spawn.classname);
        let want = if spawn.classname == MELEE02 {
            58624
        } else {
            29184
        };
        assert_eq!(spawn.hp_wire, want, "{} hp", spawn.classname);
    }
}

/// The first emitted spawn is the `questfindring` leader room's pack, at its
/// tile's `base.encounter100` marker plus the seeded group jitter, then
/// floor-snapped onto the per-instance pathmap: cell origin `(0, 400)` +
/// marker `(340, 180)` + offset, snapped to `(344.588, 571.958)`, z from the
/// node floor. The exact numbers come from the reference's `DotNetRandom`
/// (`seed ^ 0x5F5F5F5F`) and its spiral snap.
#[test]
fn dungeon00_level01_first_spawn_pose_is_pinned() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let list = spawns(&archive, "dungeon00_level01").expect("read dungeon00_level01");
    let first = list.first().expect("at least one spawn");
    assert_eq!(first.classname, MELEE01);
    assert_eq!(first.pose, [88214, 146421, 2560, 17352]);
}

#[test]
fn a_non_maze_zone_spawns_no_mobs() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    // `tutorial.world` states no `MazeWidth`, so it is not a generated maze.
    let list = spawns(&archive, "tutorial").expect("read tutorial");
    assert!(list.is_empty(), "a hand-placed zone spawns no maze mobs");
}

/// Every generated spawn sits on the per-instance walkable pathmap floor, at
/// that node's real collision height. This is the invariant the client's
/// chase path needs: `UnitMover::MoveToPoint` resolves its start node with
/// `PathMap::GetClosestNode`, which silently `StopMoving`s when no walkable
/// node is within one node of the pose.
#[test]
fn dungeon00_level01_spawns_land_on_walkable_floor() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let layout = drf_content::generate_layout(&archive, "dungeon00_level01")
        .expect("read dungeon00_level01.world")
        .expect("a generated maze level");
    let list = spawns(&archive, "dungeon00_level01").expect("read dungeon00_level01");
    assert!(!list.is_empty());
    let pathmap = drf_content::maze::build_path_map(&archive, &layout.cells)
        .expect("build pathmap")
        .expect("a pathmap for a generated level");

    for spawn in &list {
        let x = f64::from(spawn.pose[0]) / 256.0;
        let y = f64::from(spawn.pose[1]) / 256.0;
        let z = f64::from(spawn.pose[2]) / 256.0;
        assert!(
            pathmap.is_walkable(x, y),
            "{} pose ({x},{y}) is not on the walkable pathmap",
            spawn.classname
        );
        let ground = pathmap.height_at(x, y).expect("walkable node height");
        assert!(
            (ground - z).abs() < 0.01,
            "{} pose z {z} != pathmap floor {ground}",
            spawn.classname
        );
    }
}

/// Full parity: every emitted `dungeon00_level01` pose (x, y, z, heading in
/// x256 wire units) equals the reference `generate_spawns` output. The expected
/// values were computed with the reference algorithm — `pathmap_builder` over
/// the generated cells, its `_find_walkable_spot` spiral, its `DotNetRandom`
/// jitter (`seed ^ 0x5F5F5F5F`), and the client's heightmap scale (tenths) —
/// then hard-coded here so any drift in the navmesh or snap shows up as a diff.
#[test]
fn dungeon00_level01_spawn_poses_match_the_reference() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let list = spawns(&archive, "dungeon00_level01").expect("read dungeon00_level01");
    let got: Vec<[i32; 4]> = list.iter().map(|spawn| spawn.pose).collect();
    let expected: Vec<[i32; 4]> = vec![
        [88214, 146421, 2560, 17352],
        [97280, 146268, 2560, 49932],
        [74240, 152998, 7680, 75807],
        [86801, 158720, 2560, 21484],
        [-116043, 52526, 2560, 60217],
        [-104327, 51731, 2560, 73043],
        [-135572, 53770, 7680, 18635],
        [-118001, 65046, 2560, 74268],
        [-125639, 146084, 2560, 72184],
        [-165226, 169976, 2560, 49689],
        [-157440, 176247, 2560, 58396],
        [-44852, 121116, 2560, 27286],
        [-29030, 121384, 2560, 19528],
        [-58176, 119300, 2560, 86200],
        [-87365, 167805, 2560, 25357],
        [-76800, 168512, 2560, 64454],
        [161002, 118354, 2560, 52206],
        [175721, 121408, 2560, 27349],
        [145239, 120756, 2560, 15494],
        [160557, 137600, 2560, 2759],
        [121908, 163976, 2560, 43332],
        [128640, 168261, 2560, 30454],
        [-43507, 73303, 2560, 61706],
        [-32676, 75778, 2560, 45768],
        [-78864, 28076, 2560, 15446],
        [-69120, 26619, 2560, 12354],
        [-89155, 32212, 2560, 91040],
        [61443, 55945, 7680, 79576],
        [74240, 53421, 7680, 40419],
        [159678, 75745, 2560, 5404],
        [170921, 76249, 2560, 79318],
        [150441, 75512, 2560, 21059],
        [160466, 83565, 2560, 43050],
        [128172, 29401, 2560, 57675],
        [-143688, -51113, 7680, 32443],
        [-48722, -61139, 7680, 24139],
        [-37801, -64675, 7680, 29645],
        [53457, -61839, 7680, 5842],
        [64598, -61281, 7680, 23022],
        [149135, -24203, 2560, 75275],
        [162407, -24498, 2560, 55524],
        [133801, -24702, 2560, 78539],
        [149315, -8302, 2560, 58104],
        [151177, -36546, 2560, 46293],
        [-121854, -158191, 2560, 4336],
        [-165327, -137311, 2560, 69886],
    ];
    assert_eq!(got, expected);
}

/// Every spawn carries the creature's authored behavior class, so the join
/// can ship the right `MonsterBehavior2` brain (a Ranged weapon with the Melee
/// brain only runs at the player).
#[test]
fn spawns_carry_the_authored_behavior_class() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let list = spawns(&archive, "dungeon00_level01").expect("read dungeon00_level01");
    for spawn in &list {
        assert_eq!(
            spawn.behavior.as_deref(),
            Some("creatures.base.behavior.Melee"),
            "{} behavior",
            spawn.classname
        );
    }
}

/// A ranged creature resolves the Ranged brain, a melee creature the Melee
/// brain (`dungeon02` authors both in one table).
#[test]
fn ranged_and_melee_creatures_carry_their_own_brain() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let list = spawns(&archive, "dungeon02_level01").expect("read dungeon02_level01");
    let behavior = |needle: &str| {
        list.iter()
            .find(|spawn| spawn.classname.eq_ignore_ascii_case(needle))
            .and_then(|spawn| spawn.behavior.clone())
    };
    assert_eq!(
        behavior("world.Dungeon02.mob.upper.RaceA.base1").as_deref(),
        Some("creatures.base.behavior.Melee")
    );
    assert_eq!(
        behavior("world.Dungeon02.mob.upper.RaceA.base2").as_deref(),
        Some("creatures.base.behavior.Ranged")
    );
}

/// The live join uses the reachability guard: every generated spawn must be on
/// a connected walkable floor component, not on an isolated prop/platform or
/// sealed pocket. The reference `spawns` output is deliberately unchanged.
#[test]
fn live_spawns_are_relocated_to_a_walkable_floor_component() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let zone = "elite01_intro";
    let seed = drf_content::layout_seed(zone);
    let layout = drf_content::maze::generate_layout_with_seed(&archive, zone, seed)
        .expect("layout")
        .expect("generated layout");
    let pathmap = drf_content::maze::build_path_map(&archive, &layout.cells)
        .expect("pathmap")
        .expect("pathmap");
    let components = pathmap.walkable_components();
    assert!(!components.is_empty());
    let list = spawns_with_seed_reachable(&archive, zone, seed, 15).expect("reachable spawns");
    assert!(!list.is_empty(), "the live guard must not empty the level");
    for spawn in &list {
        let x = f64::from(spawn.pose[0]) / 256.0;
        let y = f64::from(spawn.pose[1]) / 256.0;
        assert!(
            pathmap.component_at(x, y, &components).is_some(),
            "{} at ({x},{y}) is not on a walkable floor component",
            spawn.classname
        );
    }
}

/// Legendary zones state `MinLevel = 25`; their mobs must not fall through to
/// the old tutorial fallback. `LevelOffset` on a gatekeeper adds to that base.
#[test]
fn legendary_zone_mobs_use_the_authored_minimum_level() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    for zone in [
        "elite01_intro",
        "elite01_stage01_F_level02",
        "amazon_dungeon",
    ] {
        let list = spawns(&archive, zone).expect("legendary spawns");
        assert!(!list.is_empty(), "{zone} has spawns");
        assert!(
            list.iter().all(|spawn| spawn.level >= 25),
            "{zone} has a mob below the authored MinLevel: {:?}",
            list.iter()
                .filter(|spawn| spawn.level < 25)
                .map(|spawn| (&spawn.classname, spawn.level))
                .collect::<Vec<_>>()
        );
    }
    let intro = spawns(&archive, "elite01_intro").expect("intro spawns");
    let gatekeeper = intro
        .iter()
        .find(|spawn| spawn.classname.ends_with("gatekeeper01"))
        .expect("intro gatekeeper");
    assert_eq!(gatekeeper.level, 27, "base 25 + LevelOffset 2");
}

/// A live join scales the mob level to the character inside the zone's
/// authored band, instead of using the old `dungeonNN -> NN*4+2` heuristic.
#[test]
fn live_mob_levels_follow_the_character_inside_the_zone_band() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let cases = [
        ("dungeon00_level01", 1, 1, 5),
        ("dungeon01_level01", 15, 15, 15),
        ("dungeon01_level01", 25, 20, 20),
        ("dungeon16_level01", 90, 90, 90),
        ("elite01_intro", 15, 25, 27),
        ("elite01_intro", 50, 50, 52),
    ];
    for (zone, player_level, min, max) in cases {
        let seed = drf_content::layout_seed(zone);
        let list =
            spawns_with_seed_reachable(&archive, zone, seed, player_level).expect("live spawns");
        assert!(!list.is_empty(), "{zone} has live spawns");
        for spawn in &list {
            assert!(
                spawn.level >= min && spawn.level <= max,
                "{zone} at player {player_level}: {} level {} outside {min}..={max}",
                spawn.classname,
                spawn.level
            );
        }
    }
}
