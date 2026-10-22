// SPDX-License-Identifier: GPL-3.0-or-later

//! Zone spawn resolution from `game.pkg`'s shipped `<zone>.zone` / `<zone>.world`.
//!
//! The client owns where a character arrives: `ZoneClient::EnterLoading`
//! resolves the zone's own `ZoneDef`, reads its `Name`, and loads that
//! `<name>.world`; the entry point is the `misc.Waypoint` the world places,
//! named by the ZoneDef's `RespawnSpawnPoint` when it states one and by the
//! canonical `Start` when it does not. This module reproduces the server-side
//! half of that read so the avatar record's pose words are a value `game.pkg`
//! states, never a constant.
//!
//! The GC grammar here is the reference parser subset the `.zone`/`.world` files
//! use: quoted strings, dotted identifiers, signed numbers and the `{};=,*`
//! punctuation. It is separate from [`crate::gc`] because those files carry
//! signed headings (`Heading = -50;`) and quoted labels the class parser does
//! not need to keep.

use crate::corpus::ContentError;
use crate::pkg::Archive;

/// The rung a spawn came from: a caller-supplied name.
pub const SPAWN_RUNG_NAMED_WAYPOINT: &str = "named-waypoint";
/// The rung a spawn came from: the ZoneDef's `RespawnSpawnPoint`.
pub const SPAWN_RUNG_RESPAWN: &str = "respawn-spawn-point";
/// The rung a spawn came from: the world's `Start` waypoint.
pub const SPAWN_RUNG_DEFAULT_START: &str = "default-start-waypoint";
/// The name the default-start rung looks for.
pub const SPAWN_DEFAULT_WAYPOINT_NAME: &str = "Start";

/// The rung a generated world's entry came from: its `ZoneStart` room node.
pub const SPAWN_RUNG_ZONE_START_ROOM: &str = "zone-start-room";
/// A PvP arena's authored `wp_*` start/respawn waypoint.
pub const SPAWN_RUNG_SPAWN_CLASS: &str = "spawn-class-waypoint";

/// The classname a world placement must carry to be a spawn waypoint.
const WAYPOINT_CLASS: &str = "misc.Waypoint";

/// The classname of the return portal synthesized for a GENERATED zone: the
/// hub variant (`misc.ZonePortal_hub.gc` states the warp-gate `Color`).
pub const GENERATED_RETURN_PORTAL_CLASS: &str = "misc.ZonePortal_hub";

/// The class synthesized for a generated room's Exit/Oneoff link (the
/// dungeon portal archetype the town also uses).
pub const GENERATED_ROOM_PORTAL_CLASS: &str = "misc.ZonePortal_agg";

/// A destination a placed world entity designates: the `Zone` its own
/// placement record states and the `SpawnPoint` it asks for (empty when the
/// record states none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The destination zone name, exactly as the placement states it.
    pub zone: String,
    /// The destination spawn-point NAME the placement asks for, or empty.
    pub spawn: String,
}

/// Does `zone`'s placement matching `(name, classname, wire)` state a `Zone`?
///
/// THE GENERAL RULE, asked of ONE ENTITY: a placed interactable designates a
/// destination if and only if its own record states a `Zone`; the destination
/// is that value and the arrival is the same record's `SpawnPoint`. There is
/// no class list in the question, so a world that places something this module
/// has never heard of is answered from its own file.
///
/// A GENERATED world places no `Entities` block, so there is no record to ask;
/// for a portal click its return leg is the start room's own
/// `LinkToZone`/`LinkToSpawn` (see [`generated_return_portal`]). `None` is the
/// ordinary answer for an NPC (its record states no `Zone`), for a
/// non-generated world with no matching placement, and for an unreadable
/// package: each is a refusal to answer here, and the caller asks its own next
/// question rather than being handed a guessed destination.
pub fn destination_for(
    archive: &Archive,
    zone: &str,
    name: Option<&str>,
    classname: Option<&str>,
    wire: Option<(i64, i64)>,
) -> Result<Option<Destination>, ContentError> {
    let Some(world_text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(None);
    };
    let file = parse(&world_text);
    let Some(root) = file.children.first() else {
        return Ok(None);
    };
    let placements = world_placements(&file);
    if let Some(placement) = identify_placement(&placements, name, classname, wire) {
        let Some(zone_value) = placement
            .prop("Zone")
            .map(unquote)
            .filter(|z| !z.is_empty())
        else {
            return Ok(None);
        };
        let spawn = placement
            .prop("SpawnPoint")
            .map(unquote)
            .unwrap_or_default();
        return Ok(Some(Destination {
            zone: zone_value,
            spawn,
        }));
    }
    // A GENERATED world places no `Entities` block, so its clicked return
    // portal has no per-entity record to ask: the start room's own
    // `LinkToZone`/`LinkToSpawn` IS the return leg. A hand-placed world with
    // no matching placement still refuses below, and a non-portal click never
    // travels.
    if stated_true(root, "Generated") && classname.is_none_or(is_portal_class) {
        if let Some(start) = start_room(archive, root) {
            if let Some((destination, spawn)) = generated_return_link(start) {
                return Ok(Some(Destination {
                    zone: destination,
                    spawn,
                }));
            }
        }
    }
    Ok(None)
}

/// Which placement a click names, strongest key first: `Name` (exact, then
/// case-insensitive), then wire `(x, y)` in the x256 domain, then `classname`
/// alone -- and only when exactly one placement of that class exists, because
/// "the first portal" is a guess in a world that places several.
fn identify_placement<'a>(
    placements: &'a [Placement],
    name: Option<&str>,
    classname: Option<&str>,
    wire: Option<(i64, i64)>,
) -> Option<&'a Placement> {
    let want_class = classname.unwrap_or("").to_ascii_lowercase();
    let cands: Vec<&Placement> = placements
        .iter()
        .filter(|placement| {
            want_class.is_empty() || placement.classname.to_ascii_lowercase() == want_class
        })
        .collect();
    if let Some(name) = name {
        if let Some(entry) = cands.iter().find(|entry| entry.name == name) {
            return Some(entry);
        }
        let folded = name.to_ascii_lowercase();
        if let Some(entry) = cands
            .iter()
            .find(|entry| !entry.name.is_empty() && entry.name.to_ascii_lowercase() == folded)
        {
            return Some(entry);
        }
    }
    if let Some((wx, wy)) = wire {
        let hits: Vec<&Placement> = cands
            .iter()
            .copied()
            .filter(|entry| entry.wire_pos() == Some((wx, wy)))
            .collect();
        if hits.len() == 1 {
            return Some(hits[0]);
        }
        if hits.len() > 1 {
            return None;
        }
    }
    if cands.len() == 1 {
        return Some(cands[0]);
    }
    None
}

/// One shipped `.zone` entry: the `ZoneDef` the client resolves.
///
/// Every field is a property `game.pkg` states in `<zone>.zone`, read with the
/// same names the client's `ZoneDef` registers. `name` is the value the client
/// looks the world up by (its `Name` property); `source` is the package path
/// the definition came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneDef {
    /// The `Name` property, exactly as the file states it.
    pub name: String,
    /// The human-readable `Label`, when the file states one.
    pub label: Option<String>,
    /// The `Private` flag (absent states false).
    pub private: bool,
    /// The `RespawnZone` name, when stated.
    pub respawn_zone: Option<String>,
    /// The `RespawnSpawnPoint` name, when stated.
    pub respawn_spawn_point: Option<String>,
    /// The `MinLevel` value, when stated.
    pub min_level: Option<i64>,
    /// The `MaxLevel` value, when stated.
    pub max_level: Option<i64>,
    /// The `IsTown` flag (absent states false).
    pub is_town: bool,
    /// The `MaxOccupancy` value, when stated.
    pub max_occupancy: Option<i64>,
    /// The `UpdateFrequency` value, when stated.
    pub update_frequency: Option<i64>,
    /// The package entry the definition was parsed from (`<zone>.zone`).
    pub source: String,
}

/// The shipped `ZoneDef` for `zone`, or `None` when the package has no such
/// entry (or the entry states no `Name`).
///
/// This mirrors the reference `ZoneCorpus.zone`: `<zone>.zone` is read
/// case-insensitively by the archive, the root block is the definition, and
/// every field is the property of the same name. A file that parses but states
/// no `Name` is a refusal, never another zone's definition.
pub fn zone_def(archive: &Archive, zone: &str) -> Result<Option<ZoneDef>, ContentError> {
    let source = format!("{zone}.zone");
    let Some(text) = read_text(archive, &source)? else {
        return Ok(None);
    };
    let file = parse(&text);
    let Some(root) = file.children.first() else {
        return Ok(None);
    };
    let Some(name) = root
        .prop("Name")
        .map(unquote)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let string_prop = |prop: &str| {
        root.prop(prop)
            .map(unquote)
            .filter(|value| !value.is_empty())
    };
    Ok(Some(ZoneDef {
        name,
        label: string_prop("Label"),
        private: stated_true(root, "Private"),
        respawn_zone: string_prop("RespawnZone"),
        respawn_spawn_point: string_prop("RespawnSpawnPoint"),
        min_level: root.prop("MinLevel").and_then(parse_i64),
        max_level: root.prop("MaxLevel").and_then(parse_i64),
        is_town: stated_true(root, "IsTown"),
        max_occupancy: root.prop("MaxOccupancy").and_then(parse_i64),
        update_frequency: root.prop("UpdateFrequency").and_then(parse_i64),
        source,
    }))
}

/// The avatar's shipped starting checkpoint classes, from
/// `avatar/base/StartingCheckpointList.gc`.
///
/// The file is a list of `* extends world.checkpoints.*Checkpoint` entries
/// (Town, Tutorial and PVPTown in the shipped content). The QuestManager
/// create carries this list so the obelisk dialog has its starting entries
/// even before the character visits another obelisk.
pub fn starting_checkpoints(archive: &Archive) -> Result<Vec<String>, ContentError> {
    let Some(text) = read_text(archive, "avatar/base/StartingCheckpointList.gc")? else {
        return Ok(Vec::new());
    };
    let root = crate::gc::parse(&text);
    let list = root
        .children
        .iter()
        .find(|node| node.name.eq_ignore_ascii_case("StartingCheckpointList"));
    let children = list
        .map(|node| node.children.as_slice())
        .unwrap_or(&root.children);
    let mut out = Vec::new();
    for child in children {
        if let Some(class) = &child.extends {
            if class.starts_with("world.checkpoints.") {
                out.push(class.clone());
            }
        }
    }
    Ok(out)
}

/// Resolve a checkpoint class **hash** (`GCClassRegistry`'s `0x04` + u32
/// type) back to its FQN.
///
/// The obelisk dialog serializes the selected checkpoint as its GC type,
/// which for a class object is `dfc_hash(FQN)`, not a classname string. The
/// scan is over the shipped `world/checkpoints/*Checkpoint.gc` classes; an
/// `...Entity` file is a world entity, not a travel destination.
pub fn checkpoint_class_by_hash(archive: &Archive, hash: u32) -> Option<String> {
    for entry in archive.entries() {
        if entry.suffix != ".gc" {
            continue;
        }
        let name = entry.name.replace('\\', "/");
        if !name.to_ascii_lowercase().starts_with("world/checkpoints/")
            || name.to_ascii_lowercase().ends_with("entity")
        {
            continue;
        }
        let fqn = name.replace('/', ".");
        if crate::dfc_hash(fqn.as_bytes()) == hash {
            return Some(fqn);
        }
    }
    None
}

/// A `Checkpoint` class's own destination: its `Description.Zone` and
/// `Description.SpawnPoint`.
///
/// Each obelisk entity's `Description.Checkpoint` names one of these classes
/// (`world.checkpoints.TutorialCheckpoint`, `...TownCheckpoint`, ...). The
/// class file itself states where the obelisk travels:
///
/// ```text
/// TutorialCheckpoint extends base.Checkpoint
/// {
///     Description { Order = 3; Zone = Tutorial; SpawnPoint = Waypoint; ... }
/// }
/// ```
///
/// `None` when the FQN does not name a readable class or its `Description`
/// states no `Zone`; it is a refusal, not a guess.
pub fn checkpoint_destination(
    archive: &Archive,
    class_name: &str,
) -> Result<Option<Destination>, ContentError> {
    let path = format!("{}.gc", class_name.trim().replace('.', "/"));
    let Some(text) = read_text(archive, &path)? else {
        return Ok(None);
    };
    let file = parse(&text);
    let Some(root) = file.children.first() else {
        return Ok(None);
    };
    let Some(description) = root
        .children
        .iter()
        .find(|child| child.name.eq_ignore_ascii_case("Description"))
    else {
        return Ok(None);
    };
    let Some(zone) = description
        .prop("Zone")
        .map(unquote)
        .filter(|zone| !zone.is_empty())
    else {
        return Ok(None);
    };
    let spawn = description
        .prop("SpawnPoint")
        .map(unquote)
        .unwrap_or_default();
    Ok(Some(Destination { zone, spawn }))
}

/// One shipped obelisk `Checkpoint` class: the class the client instantiates
/// for a Saved Places row, its own destination and its sort order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointDef {
    pub classname: String,
    pub zone: String,
    pub spawn: String,
    pub order: i32,
}

/// Every shipped `world/checkpoints/*Checkpoint` class with its destination.
/// `...Entity` files are placed world objects, not travel rows, and are
/// skipped; a class with no `Description.Zone` is not a destination.
pub fn checkpoint_defs(archive: &Archive) -> Result<Vec<CheckpointDef>, ContentError> {
    let mut out = Vec::new();
    for entry in archive.entries() {
        if entry.suffix != ".gc" {
            continue;
        }
        let name = entry.name.replace('\\', "/");
        let lower = name.to_ascii_lowercase();
        if !lower.starts_with("world/checkpoints/") || lower.ends_with("entity") {
            continue;
        }
        let classname = name.trim_end_matches(".gc").replace('/', ".");
        let Some(zone) = class_description_property(archive, &classname, "Zone")? else {
            continue;
        };
        if zone.is_empty() {
            continue;
        }
        let spawn =
            class_description_property(archive, &classname, "SpawnPoint")?.unwrap_or_default();
        let order = class_description_property(archive, &classname, "Order")?
            .and_then(|value| value.trim().parse::<i32>().ok())
            .unwrap_or(i32::MAX);
        out.push(CheckpointDef {
            classname,
            zone,
            spawn,
            order,
        });
    }
    Ok(out)
}

/// The `dungeonNN` / `pvp` / town key a zone shares with the checkpoint whose
/// `Description.Zone` names the same place. `dungeon01_level05_off1a` and
/// `dungeon01_level01` both key to `dungeon01`; `Dungeon09_level00` keys to
/// `dungeon09`, which is the destination of the (offset-named)
/// `Dungeon08Checkpoint`. `d06_l01...` normalizes to `dungeon06`.
fn zone_stem(zone: &str) -> String {
    let lower = zone.trim().to_ascii_lowercase();
    let first = lower.split('_').next().unwrap_or("");
    if first.starts_with("dungeon") {
        // The tutorial dungeon shares the tutorial checkpoint's zone.
        if first == "dungeon00" {
            return "tutorial".to_string();
        }
        return first.to_string();
    }
    let rest = &first[1..];
    if first.len() == 3 && first.starts_with('d') && rest.chars().all(|c| c.is_ascii_digit()) {
        return format!("dungeon{rest}");
    }
    if first == "pvp" {
        return "pvp".to_string();
    }
    first.to_string()
}

/// The real obelisk `Checkpoint` classes whose destination matches any zone
/// the character has visited, deduped and sorted by the class's `Order`.
///
/// The client can only draw a Saved Places row for a class that exists in
/// `game.pkg` (it instantiates it through the GC registry), so the
/// non-faithful "everywhere I've been" list is expressed as the shipped
/// checkpoint for each visited zone/dungeon rather than a synthetic row.
pub fn checkpoints_for_zones(
    archive: &Archive,
    zones: &[String],
) -> Result<Vec<String>, ContentError> {
    let defs = checkpoint_defs(archive)?;
    let mut chosen: Vec<(i32, String)> = Vec::new();
    for zone in zones {
        let stem = zone_stem(zone);
        if stem.is_empty() {
            continue;
        }
        let Some(def) = defs
            .iter()
            .filter(|def| zone_stem(&def.zone) == stem)
            .min_by_key(|def| def.order)
        else {
            continue;
        };
        if chosen
            .iter()
            .any(|(_, classname)| classname.eq_ignore_ascii_case(&def.classname))
        {
            continue;
        }
        chosen.push((def.order, def.classname.clone()));
    }
    chosen.sort_by_key(|(order, _)| *order);
    Ok(chosen.into_iter().map(|(_, classname)| classname).collect())
}

/// A resolved zone spawn pose, in the x256 fixed-point wire domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneSpawn {
    /// The rung that answered (`named-waypoint` / `respawn-spawn-point` /
    /// `default-start-waypoint`).
    pub rung: &'static str,
    /// The waypoint's own `Name` in the world file.
    pub name: String,
    /// `[x, y, z, heading]` in x256, the four `WorldEntity::readInit` pose
    /// words the avatar head carries.
    pub pose: [u32; 4],
}

/// Resolve `<zone>`'s spawn pose from its own `.zone`/`.world` content.
///
/// `None` is a refusal -- no readable `.world`, or no waypoint any rung names
/// -- never another zone's coordinates. `requested` is a spawn-point NAME (a
/// portal's `SpawnPoint`, an obelisk travel selection, a respawn argument);
/// the join passes `None`.
pub fn derive_zone_spawn(
    archive: &Archive,
    zone: &str,
    requested: Option<&str>,
) -> Result<Option<ZoneSpawn>, ContentError> {
    let Some(world_text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(None);
    };
    let placements = world_placements(&parse(&world_text));

    let respawn = match read_text(archive, &format!("{zone}.zone"))? {
        Some(text) => zone_root_prop(&parse(&text), "RespawnSpawnPoint"),
        None => None,
    };

    let try_rung = |name: &str, rung: &'static str| -> Result<Option<ZoneSpawn>, ContentError> {
        let Some(waypoint) = find_waypoint_resolved(archive, &placements, name) else {
            return Ok(None);
        };
        match waypoint.pose() {
            Some(pose) => Ok(Some(ZoneSpawn {
                rung,
                name: waypoint_name(archive, waypoint),
                pose,
            })),
            // A placement with no position is a refusal of the zone, exactly
            // as `zone_spawn_resolution` treats a None `wire_pose`.
            None => Ok(None),
        }
    };

    if let Some(want) = requested {
        if let Some(found) = try_rung(want, SPAWN_RUNG_NAMED_WAYPOINT)? {
            return Ok(Some(found));
        }
    }
    if let Some(respawn) = respawn.as_deref() {
        if let Some(found) = try_rung(respawn, SPAWN_RUNG_RESPAWN)? {
            return Ok(Some(found));
        }
    }
    if let Some(found) = try_rung(SPAWN_DEFAULT_WAYPOINT_NAME, SPAWN_RUNG_DEFAULT_START)? {
        return Ok(Some(found));
    }
    // A PvP arena may author its spawn points through a `wp_*` subclass
    // instead of a waypoint literally named `Start` (for example
    // `PVPGroupPracticeMatch` places `wp_ffa_start` and `wp_ffa_respawn`).
    // The client selects one of those authored points; use the first one in
    // world order so the avatar is not dropped at the origin.
    if let Some(waypoint) = find_waypoint_role(archive, &placements, "start") {
        return Ok(waypoint.pose().map(|pose| ZoneSpawn {
            rung: SPAWN_RUNG_SPAWN_CLASS,
            name: waypoint_name(archive, waypoint),
            pose,
        }));
    }
    if let Some(waypoint) = find_waypoint_role(archive, &placements, "respawn") {
        return Ok(waypoint.pose().map(|pose| ZoneSpawn {
            rung: SPAWN_RUNG_SPAWN_CLASS,
            name: waypoint_name(archive, waypoint),
            pose,
        }));
    }
    // A hand-placed zone names its entry with a `misc.Waypoint_StartRoom`
    // (e.g. `dungeon01_level08_boss` places `boss_spawn` at 190,240,30). That
    // class extends `misc.Waypoint` but is not named `misc.Waypoint`, so the
    // named rungs above cannot see it. Falling through to `None` here is what
    // dropped the avatar at the origin, outside the boss map.
    let Some(waypoint) = find_zone_start_waypoint(archive, &placements) else {
        return Ok(None);
    };
    Ok(waypoint.pose().map(|pose| ZoneSpawn {
        rung: SPAWN_RUNG_ZONE_START_ROOM,
        name: waypoint_name(archive, waypoint),
        pose,
    }))
}

/// True when `<zone>.world` states `Generated = true`.
pub fn zone_is_generated(archive: &Archive, zone: &str) -> Result<bool, ContentError> {
    let Some(text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(false);
    };
    let file = parse(&text);
    let Some(root) = file.children.first() else {
        return Ok(false);
    };
    Ok(root
        .prop("Generated")
        .map(unquote)
        .is_some_and(|value| value.eq_ignore_ascii_case("true")))
}

/// A generated world's entry: the `ZoneStart` room node's `SpawnName` and the
/// cell its grid cell denotes, as a pose in the x256 wire domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedZoneStart {
    /// The rung that answered (`zone-start-room`).
    pub rung: &'static str,
    /// The room node's own `SpawnName` (`start1` for `dungeon00_level01`).
    pub name: String,
    /// `[x, y, z, heading]` in x256, derived from the room's grid cell and the
    /// world's `TileSize`/`MazeWidth`/`MazeHeight`. This is the cell ORIGIN,
    /// exactly as the client's `MazeGenerator::BuildWorld` computes it:
    /// `((col - W/2)*TileSize, (row - H/2)*TileSize)`. The row (`GridY`) is the
    /// file's statement; the column is the maze RNG's when `GridX` is absent
    /// (see `maze::start_cell`), and its range-centre representative otherwise.
    pub pose: [u32; 4],
}

/// A test-only override for the generated start pose (x256 world units), set
/// once from the server CLI (`--zone-start X,Y`). When unset the pose is
/// derived from content. It exists to pin the client's exact stand-point on a
/// live run; production runs never set it.
static GENERATED_POSE_OVERRIDE: std::sync::OnceLock<[i64; 2]> = std::sync::OnceLock::new();

/// Set the generated-start pose override (idempotent; first call wins).
pub fn set_generated_pose_override(x: i64, y: i64) {
    let _ = GENERATED_POSE_OVERRIDE.set([x, y]);
}

/// Resolve a GENERATED world's entry pose from its own `.world` content.
///
/// A generated world (`Generated = true`) places no `Entities` block and no
/// `misc.Waypoint`; it names its entry with a `ZoneStart` room node whose cell
/// `MazeGenerator` computes. The NAME is the file's statement (`start1`); the
/// coordinate is the client's to carve, so the pose reported here is the room's
/// grid cell placed with the same `(cell - dim / 2) * TileSize` arithmetic the
/// client's `BuildWorld` uses, centred on the cell. Every term is read from
/// `game.pkg`: the room's `GridY` (and `GridX` when stated), the world's
/// `MazeWidth`/`MazeHeight`, and the world archetype's `TileSize`.
///
/// `None` is a refusal: not a generated world, no `ZoneStart` room, or a world
/// archetype that states no `TileSize`. It is never a constant and never
/// another zone's coordinate.
pub fn derive_generated_zone_start(
    archive: &Archive,
    zone: &str,
    requested: Option<&str>,
) -> Result<Option<GeneratedZoneStart>, ContentError> {
    derive_generated_zone_start_with_seed(archive, zone, requested, crate::maze::layout_seed(zone))
}

/// [`derive_generated_zone_start`] with the same per-entry seed the Assign
/// carried, so the server and client build the same maze.
pub fn derive_generated_zone_start_with_seed(
    archive: &Archive,
    zone: &str,
    requested: Option<&str>,
    seed: u32,
) -> Result<Option<GeneratedZoneStart>, ContentError> {
    let Some(world_text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(None);
    };
    let file = parse(&world_text);
    let Some(root) = file.children.first() else {
        return Ok(None);
    };
    let generated = root
        .prop("Generated")
        .map(unquote)
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    if !generated {
        return Ok(None);
    }
    let rooms: Vec<&Block> = root
        .children
        .iter()
        .filter(|node| is_room_node(&room_class(node)))
        .collect();
    let by_name = |want: &str| {
        rooms
            .iter()
            .copied()
            .find(|node| room_spawn(node).eq_ignore_ascii_case(want))
    };
    let start = requested
        .and_then(by_name)
        .or_else(|| {
            rooms
                .iter()
                .copied()
                .find(|node| is_zone_start(archive, node))
        })
        .or_else(|| {
            rooms
                .iter()
                .copied()
                .find(|node| is_start_room(&room_class(node)))
        })
        // A zone like `dungeon01_level01` declares no StartRoom and no
        // SpawnName: every room is a plain `roomnode`, but the first ones
        // still pin `GridX`/`GridY`. The reference's entry selector falls
        // back through "first room with a SpawnName", "tileset says start",
        // and finally the first room; do the same rather than returning None
        // and letting the avatar create keep its zero pose (off the maze).
        .or_else(|| {
            rooms
                .iter()
                .copied()
                .find(|node| !room_spawn(node).is_empty())
        })
        .or_else(|| {
            rooms.iter().copied().find(|node| {
                node.prop("TileSet")
                    .map(unquote)
                    .is_some_and(|tile| tile.to_lowercase().contains("start"))
            })
        })
        .or_else(|| rooms.first().copied());
    let Some(start) = start else {
        return Ok(None);
    };
    let name = room_spawn(start).to_string();
    let maze_w = root.prop("MazeWidth").and_then(parse_i64).unwrap_or(0);
    let maze_h = root.prop("MazeHeight").and_then(parse_i64).unwrap_or(0);
    if maze_w <= 0 || maze_h <= 0 {
        return Ok(None);
    }
    let grid_x = start.prop("GridX").and_then(parse_i64).unwrap_or(-1);
    let grid_y = start.prop("GridY").and_then(parse_i64).unwrap_or(-1);
    // The client clamps a stated grid value to the maze and treats an absent
    // one as "any cell in that row/column". The centre of the allowed range is
    // the content-derived representative of "any" (the exact column the maze
    // RNG draws is `maze::start_cell`'s answer; see that module).
    let gx = clamp_grid(grid_x, maze_w);
    let gy = clamp_grid(grid_y, maze_h);
    let Some(tile) = world_tile_size(archive, root)? else {
        return Ok(None);
    };
    if tile <= 0 {
        return Ok(None);
    }
    // The authoritative pose: the maze port's entry, which is the start room's
    // cell ORIGIN (`((col - W/2)*TileSize, (row - H/2)*TileSize)`, the client's
    // `MazeGenerator::BuildWorld` @0x5CE5A0 placement) PLUS the tile's own
    // authored `misc.Waypoint SpawnPoint` (`world_origin + authored_local`).
    // The room-grid origin below is the fallback when the maze cannot be read.
    // Centre of the selected cell, not its origin: the origin is a corner
    // of the tile and can be inside a wall. The reference's anchorless path
    // floor-snaps the centre for the same reason.
    let fallback = {
        let wx = (gx - maze_w / 2) as f64 * tile as f64 + tile as f64 / 2.0;
        let wy = (gy - maze_h / 2) as f64 * tile as f64 + tile as f64 / 2.0;
        [x256(wx), x256(wy), 0, 0]
    };
    let pose = match crate::maze::entry_position_with_seed(archive, zone, &name, seed) {
        Ok(Some(entry)) => [
            x256(entry.x),
            x256(entry.y),
            x256(entry.z),
            x256(entry.heading),
        ],
        _ => fallback,
    };
    let pose = match GENERATED_POSE_OVERRIDE.get().copied() {
        Some([ox, oy]) => [x256(ox as f64), x256(oy as f64), pose[2], pose[3]],
        None => pose,
    };
    Ok(Some(GeneratedZoneStart {
        rung: SPAWN_RUNG_ZONE_START_ROOM,
        name,
        pose,
    }))
}

/// A room node's class (`extends`, else the node name).
fn room_class(node: &Block) -> String {
    node.extends.clone().unwrap_or_else(|| node.name.clone())
}

/// A room node's own `SpawnName` (empty when it states none).
fn room_spawn(node: &Block) -> String {
    node.prop("SpawnName").map(unquote).unwrap_or_default()
}

/// A GENERATED world's start room: the room stating (or inheriting)
/// `ZoneStart = true`, else the `base.StartRoom`-rooted one. The same
/// selection [`derive_generated_zone_start`] makes, without the optional
/// caller-supplied spawn name.
fn start_room<'a>(archive: &Archive, root: &'a Block) -> Option<&'a Block> {
    let rooms: Vec<&Block> = root
        .children
        .iter()
        .filter(|node| is_room_node(&room_class(node)))
        .collect();
    rooms
        .iter()
        .copied()
        .find(|node| is_zone_start(archive, node))
        .or_else(|| {
            rooms
                .iter()
                .copied()
                .find(|node| is_start_room(&room_class(node)))
        })
}

/// The return leg a GENERATED world's start room node states: its
/// `LinkToZone` (required, non-empty) and its `LinkToSpawn` (may be empty).
fn generated_return_link(start: &Block) -> Option<(String, String)> {
    let destination = start
        .prop("LinkToZone")
        .map(unquote)
        .filter(|zone| !zone.is_empty())?;
    let spawn = start.prop("LinkToSpawn").map(unquote).unwrap_or_default();
    Some((destination, spawn))
}

/// The tail of `classname` (after the last `.`), lower-cased.
fn class_tail(classname: &str) -> String {
    classname
        .rsplit('.')
        .next()
        .unwrap_or(classname)
        .to_ascii_lowercase()
}

/// Does `classname` root at a world room node (`roomnode` / `linkroomnode` /
/// `startroom`)?
fn is_room_node(classname: &str) -> bool {
    matches!(
        class_tail(classname).as_str(),
        "roomnode" | "linkroomnode" | "startroom"
    )
}

/// Is this the zone's `base.StartRoom` archetype?
fn is_start_room(classname: &str) -> bool {
    matches!(class_tail(classname).as_str(), "startroom")
}

/// Does this room node state (or inherit from its archetype) `ZoneStart = true`?
fn is_zone_start(archive: &Archive, node: &Block) -> bool {
    if stated_true(node, "ZoneStart") {
        return true;
    }
    let class = room_class(node);
    let Ok(Some(text)) = read_text(archive, &class_gc_path(&class)) else {
        return false;
    };
    let file = parse(&text);
    file.children
        .first()
        .is_some_and(|archetype| stated_true(archetype, "ZoneStart"))
}

/// A `Name = true;` property.
fn stated_true(node: &Block, name: &str) -> bool {
    node.prop(name)
        .map(unquote)
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

/// The maze cell the client would accept for `value`: a stated cell clamped to
/// the grid, or the grid's centre when the property is absent (`-1`).
fn clamp_grid(value: i64, dim: i64) -> i64 {
    if value < 0 {
        dim / 2
    } else {
        value.min(dim - 1)
    }
}

/// Walk a world root's `extends` chain for the first `TileSize` (`game.pkg`'s
/// `base/World_<theme>.gc` states it).
pub(crate) fn world_tile_size(
    archive: &Archive,
    root: &Block,
) -> Result<Option<i64>, ContentError> {
    let mut current = root.extends.clone();
    for _ in 0..8 {
        let Some(class) = current else {
            return Ok(None);
        };
        let Some(text) = read_text(archive, &class_gc_path(&class))? else {
            return Ok(None);
        };
        let file = parse(&text);
        let Some(node) = file.children.first() else {
            return Ok(None);
        };
        if let Some(tile) = node.prop("TileSize").and_then(parse_i64) {
            return Ok(Some(tile));
        }
        current = node.extends.clone();
    }
    Ok(None)
}

/// An integer property.
fn parse_i64(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok()
}

// -- the placed world entities ---------------------------------------------

/// The kind of placed world entity, which selects its `0x02` class-state
/// builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacedKind {
    /// A `world.checkpoints.*` entity: `WorldEntity` class-state only.
    Checkpoint,
    /// A `misc.ZonePortal*` entity.
    Portal,
    /// A networked world entity (`world.*.npc.*`), StockUnit-rooted.
    Npc,
    /// A non-Unit `WorldEntity` interactive (plain world object).
    WorldEntity,
    /// A `Door`-rooted interactive: `WorldEntity` + its two door bytes.
    Door,
    /// A Unit-rooted interactive (`base.NonCombatInteractive`: chest, shrine,
    /// teleport, NCI). It ships the `Unit` tail and the NCI state bytes.
    WorldEntityUnit,
}

/// One placed world entity the server creates, with the values `game.pkg`
/// states: its class, its pose, and (per kind) its level and portal
/// destination/colour. Fields a capture would otherwise supply are not here;
/// the `0x02` builder ships their reader-neutral defaults instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedEntity {
    /// The archetype the world file places.
    pub classname: String,
    /// Which class-state builder the entity's `readInit` chain needs.
    pub kind: PlacedKind,
    /// `[x, y, z, heading]` in the x256 wire domain.
    pub pose: [u32; 4],
    /// Whether the world file stated a `Heading` (its absence authors 0).
    pub heading_stated: bool,
    /// The archetype's own `Level` (`game.pkg`), for NPCs.
    pub level: Option<u8>,
    /// A portal's `SpawnPoint` (`game.pkg` `Tutorial.world`).
    pub spawn_point: Option<String>,
    /// A portal's destination `Zone`.
    pub zone: Option<String>,
    /// A portal archetype's `Color` (`game.pkg`).
    pub color: Option<u32>,
    /// A portal placement's own `Width` (`game.pkg` world), when it states
    /// one. The `0x02` builder supplies the client archetype default when the
    /// placement is silent, because `ZonePortal::readInit` reads the field
    /// unconditionally and the visual is sized from it.
    pub width: Option<u16>,
    /// A portal placement's own `Height`, when it states one.
    pub height: Option<u16>,
    /// Whether the archetype states `Blocking = true` (WorldEntity bit 0).
    pub blocking: bool,
    /// `Description.TrackActivatingPlayers`: the NCI readInit's optional
    /// activator list. When set, the class state ends with the invalid-id
    /// sentinel (`0xFFFFFFFF`) for an empty list.
    pub track_activating_players: bool,
}

/// Synthesize the return-leg portal for a GENERATED zone.
///
/// A generated world places no `Entities` block, so there is no portal record
/// to lift; instead the `.world` `base.StartRoom` node states where the return
/// leads (`LinkToZone`) and which arrival it asks for (`LinkToSpawn`), and the
/// entry tile the maze placed states where to stand. The result is a
/// `misc.ZonePortal_hub` at the maze entry cell's own `misc.ZonePortal*` anchor
/// (`world_origin + authored_local`, from [`crate::maze::entry_portal_position`]),
/// carrying that link and the archetype's own `Color`.
///
/// `None` for a non-generated zone (the hand-placed path is unchanged), for a
/// world whose start room states no `LinkToZone`, or for an entry whose tile
/// places no `misc.ZonePortal*` at all. `width`/`height` stay `None`: the
/// client archetype's constructor defaults
/// (`drf_proto::placed_entity_class_state`) size the quad, exactly as for a
/// hand-placed portal that states neither.
pub fn generated_return_portal(
    archive: &Archive,
    zone: &str,
) -> Result<Option<PlacedEntity>, ContentError> {
    generated_return_portal_with_seed(archive, zone, crate::maze::layout_seed(zone))
}

/// [`generated_return_portal`] with the per-entry maze seed.
pub fn generated_return_portal_with_seed(
    archive: &Archive,
    zone: &str,
    seed: u32,
) -> Result<Option<PlacedEntity>, ContentError> {
    let Some(world_text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(None);
    };
    let file = parse(&world_text);
    let Some(root) = file.children.first() else {
        return Ok(None);
    };
    if !stated_true(root, "Generated") {
        return Ok(None);
    }
    let Some(start) = start_room(archive, root) else {
        return Ok(None);
    };
    let Some((destination, spawn)) = generated_return_link(start) else {
        return Ok(None);
    };
    let spawn_name = room_spawn(start);
    if spawn_name.is_empty() {
        return Ok(None);
    }
    let Some(position) =
        crate::maze::entry_portal_position_with_seed(archive, zone, &spawn_name, seed)?
    else {
        return Ok(None);
    };
    let classname = GENERATED_RETURN_PORTAL_CLASS.to_string();
    let color = block_prop(archive, &class_gc_path(&classname), "Color")?
        .and_then(|value| parse_hex_u32(&value));
    Ok(Some(PlacedEntity {
        classname,
        kind: PlacedKind::Portal,
        pose: [
            x256(position.x),
            x256(position.y),
            x256(position.z),
            x256(position.heading),
        ],
        heading_stated: false,
        level: None,
        spawn_point: Some(spawn).filter(|value| !value.is_empty()),
        zone: Some(destination),
        color,
        width: None,
        height: None,
        blocking: false,
        track_activating_players: false,
    }))
}

/// The portal entity a generated room's own `LinkToZone`/`LinkToSpawn`
/// designates, at that room's tile portal anchor.
///
/// The start room's return leg is [`generated_return_portal`]'s `_hub`; this
/// covers every OTHER placed room (the Exit to the next level, the Oneoff
/// branches). `generated_return_portal` and this function are disjoint: the
/// start room is skipped by its `SpawnName`.
pub fn generated_room_portals(
    archive: &Archive,
    zone: &str,
    seed: u32,
) -> Result<Vec<PlacedEntity>, ContentError> {
    let Some(world_text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(Vec::new());
    };
    let file = parse(&world_text);
    let Some(root) = file.children.first() else {
        return Ok(Vec::new());
    };
    if !stated_true(root, "Generated") {
        return Ok(Vec::new());
    }
    let start_spawn = start_room(archive, root)
        .map(room_spawn)
        .unwrap_or_default();
    let Some(layout) = crate::maze::generate_layout_with_seed(archive, zone, seed)? else {
        return Ok(Vec::new());
    };
    let classname = GENERATED_ROOM_PORTAL_CLASS;
    let color = block_prop(archive, &class_gc_path(classname), "Color")?
        .and_then(|value| parse_hex_u32(&value));
    let mut out = Vec::new();
    for placed in &layout.placed_room_nodes {
        let Some(source) = layout
            .room_nodes
            .iter()
            .find(|node| node.source_index == placed.source_index)
        else {
            continue;
        };
        if !start_spawn.is_empty() && source.spawn_name.eq_ignore_ascii_case(&start_spawn) {
            continue;
        }
        let Some(destination) = source.link_to_zone.as_deref().filter(|z| !z.is_empty()) else {
            continue;
        };
        let Some(position) = crate::maze::room_portal_position_with_seed(
            archive,
            zone,
            placed.grid_x,
            placed.grid_y,
            seed,
        )?
        else {
            continue;
        };
        out.push(PlacedEntity {
            classname: classname.to_string(),
            kind: PlacedKind::Portal,
            pose: [
                x256(position.x),
                x256(position.y),
                x256(position.z),
                x256(position.heading),
            ],
            heading_stated: false,
            level: None,
            spawn_point: source.link_to_spawn.clone(),
            zone: Some(destination.to_string()),
            color,
            width: None,
            height: None,
            blocking: false,
            track_activating_players: false,
        });
    }
    Ok(out)
}

/// Derive the entities `<zone>.world` places, in create order.
///
/// The order is the reference `ZonePlacement.created()`: the checkpoint, then the
/// portals, then the network entities -- Townston's own order. The four
/// `misc.Waypoint`s stay out (a pathing anchor, not a networked entity), and a
/// placement with no `Position` is refused rather than put at the origin. A
/// GENERATED world places no `Entities` block; its single synthesized return
/// portal ([`generated_return_portal`]) is appended.
///
/// Every value is a `game.pkg` statement: the class and pose from the world
/// file, the NPC `Level` from the archetype's own file, the portal's
/// `SpawnPoint`/`Zone`/`Width`/`Height` from the world and its `Color` from
/// the archetype. A network entity whose archetype file is absent from
/// `game.pkg` (so its `Level` cannot be derived) is skipped, never given a
/// fabricated level.
pub fn derive_world_entities(
    archive: &Archive,
    zone: &str,
) -> Result<Vec<PlacedEntity>, ContentError> {
    let Some(world_text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(Vec::new());
    };
    let placements = world_placements(&parse(&world_text));

    let mut out = Vec::new();

    if let Some(placement) = placements
        .iter()
        .find(|placement| is_checkpoint_class(&placement.classname))
    {
        if let Some(pose) = placement.pose() {
            out.push(PlacedEntity {
                classname: placement.classname.clone(),
                kind: PlacedKind::Checkpoint,
                pose,
                heading_stated: placement.heading.is_some(),
                level: None,
                spawn_point: None,
                zone: None,
                color: None,
                width: None,
                height: None,
                blocking: true,
                track_activating_players: false,
            });
        }
    }

    for placement in placements
        .iter()
        .filter(|placement| is_portal_class(&placement.classname))
    {
        let Some(pose) = placement.pose() else {
            continue;
        };
        let color = block_prop(archive, &class_gc_path(&placement.classname), "Color")?
            .and_then(|value| parse_hex_u32(&value));
        out.push(PlacedEntity {
            classname: placement.classname.clone(),
            kind: PlacedKind::Portal,
            pose,
            heading_stated: placement.heading.is_some(),
            level: None,
            spawn_point: placement
                .prop("SpawnPoint")
                .map(unquote)
                .filter(|value| !value.is_empty()),
            zone: placement
                .prop("Zone")
                .map(unquote)
                .filter(|value| !value.is_empty()),
            color,
            width: placement.prop("Width").and_then(parse_u16),
            height: placement.prop("Height").and_then(parse_u16),
            blocking: false,
            track_activating_players: false,
        });
    }

    for placement in placements
        .iter()
        .filter(|placement| !is_non_network(archive, &placement.classname))
    {
        if is_encounter_marker(&placement.classname) {
            continue;
        }
        // A placeholder owns a `WorldEntityTable`; the concrete entity is
        // emitted below, not the placeholder itself.
        if placement_world_entity_table(archive, placement)?.is_some() {
            continue;
        }
        let Some(pose) = placement.pose() else {
            continue;
        };
        // Creatures are emitted by the mob-spawn path, never as a placed NPC.
        // Hand-placed quest chests (Q10_Chest_*) are StockUnits with combat
        // stats and must not become static NPCs.
        if class_is_creature(archive, &placement.classname)? {
            continue;
        }
        // Directly placed network entities keep their authored Level (the
        // town/tutorial NPCs). A Level-less class that resolves in the pkg is
        // a non-Unit interactive (chest, gate, shrine, NCI) and ships the
        // WorldEntity class-state. Creatures only exist behind encounter or
        // generator tables, never as a bare placement.
        if let Some(level) = class_level(archive, &placement.classname)? {
            out.push(PlacedEntity {
                classname: placement.classname.clone(),
                kind: PlacedKind::Npc,
                pose,
                heading_stated: placement.heading.is_some(),
                level: Some(level),
                spawn_point: None,
                zone: None,
                color: None,
                width: None,
                height: None,
                blocking: false,
                track_activating_players: false,
            });
        } else if class_is_npc(archive, &placement.classname)? {
            // StockUnit-rooted NPCs whose class states no Level still need the
            // NPC (52-byte) class state; level 1 is the Unit constructor's
            // default and the only value the content leaves available.
            out.push(PlacedEntity {
                classname: placement.classname.clone(),
                kind: PlacedKind::Npc,
                pose,
                heading_stated: placement.heading.is_some(),
                level: Some(1),
                spawn_point: None,
                zone: None,
                color: None,
                width: None,
                height: None,
                blocking: false,
                track_activating_players: false,
            });
        } else if class_block_node(archive, &placement.classname)?.is_some() {
            out.push(PlacedEntity {
                classname: placement.classname.clone(),
                kind: if class_is_door(archive, &placement.classname)? {
                    PlacedKind::Door
                } else if class_is_unit_interactive(archive, &placement.classname)? {
                    PlacedKind::WorldEntityUnit
                } else {
                    PlacedKind::WorldEntity
                },
                pose,
                heading_stated: placement.heading.is_some(),
                level: None,
                spawn_point: None,
                zone: None,
                color: None,
                width: None,
                height: None,
                blocking: class_blocking(archive, &placement.classname)?,
                track_activating_players: class_track_activating_players(
                    archive,
                    &placement.classname,
                )?,
            });
        }
    }

    for table in hand_placed_world_entity_tables(archive, zone)? {
        let Some(classname) = world_entity_table_choice(archive, &table.table, &table.key)? else {
            continue;
        };
        if table.baked_chest && classname.to_ascii_lowercase().contains("chest") {
            continue;
        }
        if let Some(entity) = placed_entity_for_class(archive, &classname, table.pose, false)? {
            out.push(entity);
        }
    }

    if let Some(portal) = generated_return_portal(archive, zone)? {
        out.push(portal);
    }

    Ok(out)
}

/// The classnames `<zone>.world` places for the server, in world-file order.
///
/// This is the quest directory's roster -- the reference
/// `ZonePlacement.npcs()`, exactly: the world's `Entities` block minus
/// `misc.*` and `world.checkpoints.*`. It is the same world walk
/// [`derive_world_entities`] performs, with one difference that matters for a
/// roster: a networked entity whose archetype states no `Level` anywhere in
/// its `extends` chain is still PLACED here, while [`derive_world_entities`]
/// refuses to create a unit at a level `game.pkg` does not state.
///
/// An unreadable world is an empty roster, never another zone's.
pub fn derive_zone_npcs(archive: &Archive, zone: &str) -> Result<Vec<String>, ContentError> {
    let Some(world_text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(Vec::new());
    };
    let placements = world_placements(&parse(&world_text));
    Ok(placements
        .into_iter()
        .filter(|placement| !is_non_network(archive, &placement.classname))
        .map(|placement| placement.classname)
        .collect())
}

pub(crate) fn read_text(archive: &Archive, path: &str) -> Result<Option<String>, ContentError> {
    match archive.read_path(path)? {
        Some(bytes) => Ok(Some(bytes.iter().map(|&b| b as char).collect())),
        None => Ok(None),
    }
}

/// The first `Level` stated walking a class's `extends` chain outward.
///
/// `world.town.npc.TrainerFighter` itself states no `Level`; its base
/// `world.town.npc.base.TrainerFighterBase` states `Level = 30` (Mage and
/// Ranger 20). The reference `ZoneCorpus.level_for` walks the same chain, so a
/// level the chain states is a level `game.pkg` states.
fn class_level(archive: &Archive, classname: &str) -> Result<Option<u8>, ContentError> {
    for node in class_nodes(archive, classname)? {
        if let Some(level) = node
            .prop("Level")
            .and_then(|value| value.trim().parse::<u8>().ok())
        {
            return Ok(Some(level));
        }
    }
    Ok(None)
}

/// One `WorldEntityGeneratorTable` placement: the table to resolve and the
/// pose it was placed at. Both hand-placed `RoomPlaceholderEntity` /
/// `PlaceholderEntity` records and generated room nodes state these; the
/// table itself chooses one concrete `WorldEntity` by `Chance`.
#[derive(Debug, Clone, PartialEq)]
pub struct TablePlacement {
    /// The placeholder's authored class (diagnostic; the table is the real
    /// content).
    pub classname: String,
    /// `WorldEntityGeneratorTable` classname.
    pub table: String,
    /// `[x, y, z, heading]` in the x256 wire domain.
    pub pose: [u32; 4],
    /// Stable selection key (`zone:placement:<index>:<name>`).
    pub key: String,
    /// The cell's own `Entities` block already places a chest. A table roll
    /// that also selects a chest is the same object twice, so it is skipped.
    pub baked_chest: bool,
}

/// Resolve a class block. The file is the longest dotted prefix that names a
/// real `.gc`, and the remaining dotted segments are nested blocks inside it.
/// `world.dungeon00.data.BossFightNCI01.RattleTooth` lives inside
/// `.../BossFightNCI01.gc`; the two-level
/// `skills.creature.spelleffect.SummonRelicGuardian.dungeon01.Guardian_2HAxe`
/// lives inside `.../SummonRelicGuardian.gc` under `Dungeon01`. This is the
/// same resolution the encounter spawner uses for creature archetypes.
fn class_block_node(
    archive: &Archive,
    classname: &str,
) -> Result<Option<crate::gc::Node>, ContentError> {
    let dotted = classname.trim();
    if dotted.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&str> = dotted.split('.').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return Ok(None);
    }
    for split in (1..=parts.len()).rev() {
        let path = format!("{}.gc", parts[..split].join("/"));
        let Some(text) = read_text(archive, &path)? else {
            continue;
        };
        let file = crate::gc::parse(&text);
        let remaining = &parts[split..];
        let resolved = if remaining.is_empty() {
            find_class_block(&file, parts[parts.len() - 1]).cloned()
        } else {
            let mut current = find_class_block(&file, remaining[0]);
            if let Some(node) = current {
                for name in &remaining[1..] {
                    match find_class_block(node, name) {
                        Some(next) => current = Some(next),
                        None => {
                            current = None;
                            break;
                        }
                    }
                }
            }
            current.cloned()
        };
        if let Some(node) = resolved {
            return Ok(Some(node));
        }
    }
    Ok(None)
}

fn find_class_block<'a>(file: &'a crate::gc::Node, leaf: &str) -> Option<&'a crate::gc::Node> {
    let mut stack: Vec<&crate::gc::Node> = file.children.iter().collect();
    while let Some(node) = stack.pop() {
        if node.name.eq_ignore_ascii_case(leaf) {
            return Some(node);
        }
        stack.extend(node.children.iter());
    }
    None
}

/// Every class block on an archetype's `extends` chain, leaf first.
fn class_nodes(archive: &Archive, classname: &str) -> Result<Vec<crate::gc::Node>, ContentError> {
    Ok(class_nodes_named(archive, classname)?
        .into_iter()
        .map(|(_, node)| node)
        .collect())
}

/// The same walk as [`class_nodes`], keeping the FQN that resolved each block.
/// A nested block's component class is `<owner FQN>.<block name>`, so the
/// owner spelling matters (e.g. a trainer's `SkillTrainer` lives on its base).
pub(crate) fn class_nodes_named(
    archive: &Archive,
    classname: &str,
) -> Result<Vec<(String, crate::gc::Node)>, ContentError> {
    let mut out = Vec::new();
    let mut current = classname.trim().to_string();
    for _ in 0..32 {
        let node = class_block_node(archive, &current)?;
        let Some(node) = node else {
            break;
        };
        out.push((current.clone(), node.clone()));
        let Some(parent) = node.extends.as_deref() else {
            break;
        };
        if parent.is_empty() || parent.eq_ignore_ascii_case(&current) {
            break;
        }
        current = parent.to_string();
    }
    Ok(out)
}

/// The `TeleportZone` / `TeleportPoint` an NCI class's `Description` states.
/// This uses the same `extends`-chain walk as the other class-property
/// derivations, but descends into `Description` because the teleport fields
/// live there (e.g. `toTutorial`).
pub fn nci_teleport_destination(
    archive: &Archive,
    classname: &str,
) -> Result<Option<(String, String)>, ContentError> {
    fn find(node: &crate::gc::Node, name: &str) -> Option<String> {
        if let Some(value) = node.prop(name) {
            return Some(value.to_string());
        }
        for child in &node.children {
            if child.name.eq_ignore_ascii_case("Description") {
                if let Some(value) = child.prop(name) {
                    return Some(value.to_string());
                }
            }
            if let Some(value) = find(child, name) {
                return Some(value);
            }
        }
        None
    }
    for node in class_nodes(archive, classname)? {
        let zone = find(&node, "TeleportZone").map(|value| unquote(&value));
        if let Some(zone) = zone.filter(|value| !value.is_empty()) {
            let point = find(&node, "TeleportPoint")
                .map(|value| unquote(&value))
                .unwrap_or_default();
            return Ok(Some((zone, point)));
        }
    }
    Ok(None)
}

/// The first `name` property walking a class's `extends` chain outward.
pub fn class_property(
    archive: &Archive,
    classname: &str,
    name: &str,
) -> Result<Option<String>, ContentError> {
    for node in class_nodes(archive, classname)? {
        if let Some(value) = node.prop(name) {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

/// The first `name` property on a class chain or inside its `Description`
/// blocks. `DoorsToOpenOnDeath` is authored inside `Description`, unlike the
/// direct `Name`/`Level` fields.
pub fn class_description_property(
    archive: &Archive,
    classname: &str,
    name: &str,
) -> Result<Option<String>, ContentError> {
    for node in class_nodes(archive, classname)? {
        if let Some(value) = find_property(&node, name) {
            return Ok(Some(unquote(&value)));
        }
    }
    Ok(None)
}

/// The first `name` property anywhere under `node` (its own fields and its
/// nested `Description`/`Object` blocks).
fn find_property(node: &crate::gc::Node, name: &str) -> Option<String> {
    if let Some(value) = node.prop(name) {
        return Some(value.to_string());
    }
    for child in &node.children {
        if let Some(value) = find_property(child, name) {
            return Some(value);
        }
    }
    None
}

/// Whether `classname` is `target` or inherits it through its `extends`
/// chain. The client's `IsKindOf` uses this relation when matching a killed
/// unit against an authored `MonsterType`, which may name a base class.
pub fn class_is_kind_of(
    archive: &Archive,
    classname: &str,
    target: &str,
) -> Result<bool, ContentError> {
    if classname.eq_ignore_ascii_case(target) {
        return Ok(true);
    }
    for node in class_nodes(archive, classname)? {
        if let Some(parent) = node.extends.as_deref() {
            if parent.eq_ignore_ascii_case(target) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Does a class's chain root in `creatures.*` or place it under a `.mob.`
/// namespace? This is the content's own split between a spawned combat unit
/// and an authored world object/NPC; no per-zone roster is involved.
pub fn class_is_creature(archive: &Archive, classname: &str) -> Result<bool, ContentError> {
    let dotted = classname.trim().to_ascii_lowercase();
    let mut chain = vec![dotted.clone()];
    for node in class_nodes(archive, classname)? {
        if let Some(target) = node.extends.as_deref() {
            chain.push(target.to_ascii_lowercase());
        }
    }
    // A vendor/NPC base can itself root in a shared creature ancestry; the
    // authored `.npc.` namespace wins over the creature chain.
    if chain.iter().any(|name| name.contains(".npc.")) {
        return Ok(false);
    }
    if chain
        .iter()
        .any(|name| name.starts_with("creatures.") || name.contains(".mob."))
    {
        return Ok(true);
    }
    for stat in ["CreatureDifficulty", "MaxHealth", "Difficulty"] {
        if class_property(archive, classname, stat)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// One authored `OnDeath` proc's summon units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnDeathSummon {
    /// The proc's `Description.Chance`; 100 means always.
    pub chance: u32,
    /// `(SpawnUnit class, LevelOffset)` pairs authored in the SpellEffect tree.
    pub units: Vec<(String, i32)>,
}

/// Every `OnDeath` modifier a creature class carries, with its `SpawnUnit`
/// children. Modifier rows are authored in the class's `Modifiers` block as
/// `* extends <skill>`; only rows whose target is a
/// `skills.creature.proc.base.OnDeath` are returned. Non-creature targets
/// (the bonus-stage teleport NCIs) are filtered by the runtime caller.
pub fn on_death_summons(
    archive: &Archive,
    classname: &str,
) -> Result<Vec<OnDeathSummon>, ContentError> {
    let mut out = Vec::new();
    for node in class_nodes(archive, classname)? {
        for modifiers in node
            .children
            .iter()
            .filter(|child| child.name.eq_ignore_ascii_case("Modifiers"))
        {
            for row in &modifiers.children {
                let Some(skill) = row.extends.as_deref() else {
                    continue;
                };
                if !class_is_kind_of(archive, skill, "skills.creature.proc.base.OnDeath")? {
                    continue;
                }
                let chance = class_description_property(archive, skill, "Chance")?
                    .and_then(|value| value.trim().parse::<u32>().ok())
                    .unwrap_or(100)
                    .min(100);
                if chance == 0 {
                    continue;
                }
                let mut units = Vec::new();
                for skill_node in class_nodes(archive, skill)? {
                    collect_spawn_units(&skill_node, &mut units);
                }
                if !units.is_empty() {
                    out.push(OnDeathSummon { chance, units });
                }
            }
        }
    }
    Ok(out)
}

/// The `SpawnUnit` leaves under one effect/skill node, with their
/// `LevelOffset` when authored.
fn collect_spawn_units(node: &crate::gc::Node, out: &mut Vec<(String, i32)>) {
    if let Some(unit) = node.prop("SpawnUnit") {
        let unit = unquote(unit);
        if !unit.is_empty() {
            let offset = node
                .prop("LevelOffset")
                .and_then(|value| value.trim().parse::<i32>().ok())
                .unwrap_or(0);
            if !out
                .iter()
                .any(|(known, _)| known.eq_ignore_ascii_case(&unit))
            {
                out.push((unit, offset));
            }
        }
    }
    for child in &node.children {
        collect_spawn_units(child, out);
    }
}

/// The `SpawnUnit` classes a class's `Description.SpellEffect` summons when the
/// NCI is activated. A plain relic chest states the base `RelicStun` effect
/// (no spawn); a guardian chest overrides it with
/// `SummonRelicGuardian.<zone>.Guardian_*`, whose nested `SpellSpawnEffect`
/// names the guardian mob. Walking the chain leaf-first means the override wins
/// over the base effect.
pub fn spell_effect_summons(
    archive: &Archive,
    classname: &str,
) -> Result<Vec<(String, i32)>, ContentError> {
    for node in class_nodes(archive, classname)? {
        let Some(effect) = find_property(&node, "SpellEffect").map(|value| unquote(&value)) else {
            continue;
        };
        if effect.is_empty() {
            continue;
        }
        let mut units = Vec::new();
        for effect_node in class_nodes(archive, &effect)? {
            collect_spawn_units(&effect_node, &mut units);
        }
        if !units.is_empty() {
            return Ok(units);
        }
    }
    Ok(Vec::new())
}

/// The `Blocking` a class chain states (a `WorldEntity` collision bit).
pub fn class_blocking(archive: &Archive, classname: &str) -> Result<bool, ContentError> {
    Ok(class_property(archive, classname, "Blocking")?
        .as_deref()
        .map(unquote)
        .is_some_and(|value| value.eq_ignore_ascii_case("true") || value == "1"))
}

/// Does the archetype root at `base.Door`? The client's `Door::readInit`
/// reads two bytes after `WorldEntity`.
pub fn class_is_door(archive: &Archive, classname: &str) -> Result<bool, ContentError> {
    for node in class_nodes(archive, classname)? {
        let name = node.name.to_ascii_lowercase();
        let parent = node.extends.as_deref().unwrap_or("").to_ascii_lowercase();
        if name == "door"
            || name.ends_with(".door")
            || parent == "base.door"
            || parent.ends_with(".door")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The authored unlock gate shared by `Door` and `NonCombatInteractive`
/// classes: `Description.Locked`, `Description.UnlockObject`, and the
/// unlocked item's `ItemDesc.ConsumedOnUnlock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlockRequirement {
    /// The item GC class that must be in the bag.
    pub object: String,
    /// Whether the item is consumed on a successful unlock (content default
    /// is `true`, as the shipped item comments state).
    pub consumed: bool,
}

/// Is the class authored as a locked interactive/door?
pub fn class_is_locked(archive: &Archive, classname: &str) -> Result<bool, ContentError> {
    Ok(class_description_property(archive, classname, "Locked")?
        .is_some_and(|value| value.eq_ignore_ascii_case("true") || value.trim() == "1"))
}

/// The item gate a locked door/NCI states, if any. A locked class with no
/// `UnlockObject` is scripted (for example a boss gate opened by
/// `DoorsToOpenOnDeath`) and clicking it must not open it.
pub fn class_unlock_requirement(
    archive: &Archive,
    classname: &str,
) -> Result<Option<UnlockRequirement>, ContentError> {
    let Some(object) = class_description_property(archive, classname, "UnlockObject")? else {
        return Ok(None);
    };
    let object = object.trim().to_string();
    if object.is_empty() {
        return Ok(None);
    }
    let consumed = class_description_property(archive, &object, "ConsumedOnUnlock")?
        .as_deref()
        .map(|value| value.eq_ignore_ascii_case("true") || value.trim() == "1")
        .unwrap_or(true);
    Ok(Some(UnlockRequirement { object, consumed }))
}

/// Does the class chain root in a StockUnit / the authored `.npc.` namespace?
/// Those entities carry the 25-byte StockUnit tail even when the class states
/// no `Level` (the dungeon quest givers are the shipped example); treating
/// them as a plain NonCombatInteractive truncates the class state and desyncs
/// every record after it.
pub fn class_is_npc(archive: &Archive, classname: &str) -> Result<bool, ContentError> {
    // The `.npc.` namespace is not sufficient: portraits and other
    // NonCombatInteractives live under `world.dungeonNN.npc` too. The
    // StockUnit ancestry is the class shape that needs the 52-byte NPC state.
    for node in class_nodes(archive, classname)? {
        let parent = node.extends.as_deref().unwrap_or("").to_ascii_lowercase();
        if parent == "base.stockunit"
            || parent == "stockunit"
            || node.name.eq_ignore_ascii_case("StockUnit")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Does the archetype carry the `Unit` behavior/description of
/// `base.NonCombatInteractive`? Chests, shrines, teleports and quest NCIs do;
/// a plain `Door` does not. This is the content's own class shape, used only
/// to choose the `WorldEntity` vs `WorldEntity + Unit` class-state tail.
pub fn class_is_unit_interactive(archive: &Archive, classname: &str) -> Result<bool, ContentError> {
    if class_level(archive, classname)?.is_some() {
        return Ok(false);
    }
    for node in class_nodes(archive, classname)? {
        if node
            .children
            .iter()
            .any(|child| child.name.eq_ignore_ascii_case("Behavior"))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A `WorldEntityGeneratorTable` class's `SingleWorldEntityGenerator` rows.
/// `Chance` is the client's acceptance denominator (1 = always, 4 = 1-in-4),
/// not a weight: `GCObjectGeneratorTable<WorldEntity>::ChanceSort` orders the
/// rows by descending `Chance`, each equal-chance group gets one
/// `Random::generate() % Chance == 0` draw, and one survivor of a passing
/// group is picked uniformly (the same grammar as loot generators).
fn world_entity_table_choices(
    archive: &Archive,
    table: &str,
) -> Result<Vec<(String, u32)>, ContentError> {
    let Some(node) = class_block_node(archive, table)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for child in &node.children {
        let is_generator = child.extends.as_deref().is_some_and(|extends| {
            extends
                .rsplit('.')
                .next()
                .unwrap_or(extends)
                .eq_ignore_ascii_case("SingleWorldEntityGenerator")
        });
        let entity = child
            .prop("WorldEntity")
            .map(unquote)
            .filter(|value| !value.is_empty());
        if !is_generator && entity.is_none() {
            continue;
        }
        if let Some(entity) = entity {
            let chance = child
                .prop("Chance")
                .and_then(|value| value.trim().parse::<u32>().ok())
                .unwrap_or(1);
            if chance > 0 {
                out.push((entity, chance));
            }
        }
    }
    if out.is_empty() {
        if let Some(entity) = node
            .prop("WorldEntity")
            .map(unquote)
            .filter(|value| !value.is_empty())
        {
            out.push((entity, 1));
        }
    }
    Ok(out)
}

/// Pick one `WorldEntity` from a generator table with the client's chance
/// grammar, but deterministically keyed by `key` so the entity and mob
/// derivations agree for one room/placement.
pub fn world_entity_table_choice(
    archive: &Archive,
    table: &str,
    key: &str,
) -> Result<Option<String>, ContentError> {
    let choices = world_entity_table_choices(archive, table)?;
    if choices.is_empty() {
        return Ok(None);
    }
    let mut choices = choices;
    choices.sort_by_key(|(_, chance)| std::cmp::Reverse(*chance));
    let mut groups: Vec<(u32, Vec<String>)> = Vec::new();
    for (entity, chance) in choices {
        match groups.last_mut() {
            Some((value, group)) if *value == chance => group.push(entity),
            _ => groups.push((chance, vec![entity])),
        }
    }
    for (index, (chance, group)) in groups.iter().enumerate() {
        let draw = stable_key_hash(key, index as u32);
        if !draw.is_multiple_of(*chance) {
            continue;
        }
        let pick = stable_key_hash(key, !(index as u32)) as usize % group.len();
        return Ok(Some(group[pick].clone()));
    }
    Ok(None)
}

/// FNV-1a over the table key plus a salt, for deterministic table choices.
fn stable_key_hash(key: &str, salt: u32) -> u32 {
    let mut hash = 2_166_136_261u32;
    for byte in key.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash ^= salt;
    hash = hash.wrapping_mul(16_777_619);
    hash
}

/// `WorldEntityTable`/`WorldEntityTable2`/`WorldEntityTable3` on a class, as
/// selected by its `TableSelector` (default 1).
fn class_world_entity_table(
    archive: &Archive,
    classname: &str,
) -> Result<Option<String>, ContentError> {
    let selector = class_property(archive, classname, "TableSelector")?
        .and_then(|value| value.trim().parse::<u8>().ok())
        .unwrap_or(1)
        .clamp(1, 3);
    let suffix = match selector {
        1 => "",
        2 => "2",
        _ => "3",
    };
    let name = format!("WorldEntityTable{suffix}");
    Ok(class_property(archive, classname, &name)?
        .as_deref()
        .map(unquote)
        .filter(|value| !value.is_empty()))
}

/// The table a hand-placed record names, on the record or its archetype.
fn placement_world_entity_table(
    archive: &Archive,
    placement: &Placement,
) -> Result<Option<String>, ContentError> {
    for name in [
        "EncounterWorldEntityTable",
        "EncounterWorldEntityTable2",
        "EncounterWorldEntityTable3",
        "WorldEntityTable",
        "WorldEntityTable2",
        "WorldEntityTable3",
    ] {
        if let Some(table) = placement
            .prop(name)
            .map(unquote)
            .filter(|value| !value.is_empty())
        {
            return Ok(Some(table));
        }
    }
    class_world_entity_table(archive, &placement.classname)
}

/// Hand-placed `WorldEntityGeneratorTable` records (boss-fight placeholders,
/// `PlaceholderEntity`, etc.) in world-file order.
pub fn hand_placed_world_entity_tables(
    archive: &Archive,
    zone: &str,
) -> Result<Vec<TablePlacement>, ContentError> {
    let Some(world_text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(Vec::new());
    };
    let placements = world_placements(&parse(&world_text));
    let mut out = Vec::new();
    for (index, placement) in placements.iter().enumerate() {
        if is_non_network(archive, &placement.classname)
            || is_encounter_marker(&placement.classname)
        {
            continue;
        }
        let Some(table) = placement_world_entity_table(archive, placement)? else {
            continue;
        };
        let Some(pose) = placement.pose() else {
            continue;
        };
        out.push(TablePlacement {
            classname: placement.classname.clone(),
            table,
            pose,
            key: format!("{zone}:placement:{index}:{}", placement.name),
            baked_chest: false,
        });
    }
    Ok(out)
}

/// Generated room nodes that name an `EncounterWorldEntityTable*`, at their
/// placed cell's centre.
pub fn generated_room_world_entity_tables(
    archive: &Archive,
    zone: &str,
    seed: u32,
) -> Result<Vec<TablePlacement>, ContentError> {
    let Some(world_text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(Vec::new());
    };
    let file = parse(&world_text);
    let Some(root) = file.children.first() else {
        return Ok(Vec::new());
    };
    if !stated_true(root, "Generated") {
        return Ok(Vec::new());
    }
    let Some(layout) = crate::maze::generate_layout_with_seed(archive, zone, seed)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for placed in &layout.placed_room_nodes {
        let Some(source) = layout
            .room_nodes
            .iter()
            .find(|node| node.source_index == placed.source_index)
        else {
            continue;
        };
        let Some(cell) = layout
            .cells
            .iter()
            .find(|cell| cell.grid_x == placed.grid_x && cell.grid_y == placed.grid_y)
        else {
            continue;
        };
        // A tile that bakes its own network entities (the loot rooms' two
        // chests) already supplies the interactable; a table roll at the room
        // centre would add a second, often embedded, object. Use the tile's
        // authored Z for the tables that remain.
        let tile_entities = crate::maze::tile_world_entities(archive, &cell.tile_type)?;
        let baked_chest = tile_entities.iter().any(|tile| {
            tile.classname.to_ascii_lowercase().contains("chest")
                && !is_non_network(archive, &tile.classname)
        });
        let floor_z = tile_entities.first().map(|tile| tile.z).unwrap_or(0.0);
        for (selector, table) in &source.encounter_world_entity_tables {
            out.push(TablePlacement {
                classname: source.node_kind.clone(),
                table: table.clone(),
                pose: [
                    x256(cell.world_center_x),
                    x256(cell.world_center_y),
                    x256(floor_z),
                    0,
                ],
                key: format!("{zone}:room:{}:{selector}", source.source_index),
                baked_chest,
            });
        }
    }
    Ok(out)
}

/// The authored loot generators an `base.NonCombatInteractive` class states
/// (`ItemGenerator`, `ItemGenerator2`..`5`, each with its own `ItemCount`).
/// Chests use this; a shrine states none and returns an empty list.
pub fn nci_item_generators(
    archive: &Archive,
    classname: &str,
) -> Result<Vec<(String, u32)>, ContentError> {
    let mut slots: [Option<(String, u32)>; 5] = Default::default();
    for node in class_nodes(archive, classname)? {
        let descriptions: Vec<&crate::gc::Node> = node
            .children
            .iter()
            .filter(|child| child.name.eq_ignore_ascii_case("Description"))
            .collect();
        for (selector, slot) in slots.iter_mut().enumerate() {
            if slot.is_some() {
                continue;
            }
            let suffix = if selector == 0 {
                String::new()
            } else {
                (selector + 1).to_string()
            };
            let generator = descriptions
                .iter()
                .find_map(|description| description.prop(&format!("ItemGenerator{suffix}")))
                .or_else(|| node.prop(&format!("ItemGenerator{suffix}")))
                .map(unquote)
                .filter(|value| !value.is_empty());
            let Some(generator) = generator else {
                continue;
            };
            let count = descriptions
                .iter()
                .find_map(|description| description.prop(&format!("ItemCount{suffix}")))
                .or_else(|| node.prop(&format!("ItemCount{suffix}")))
                .and_then(|value| value.trim().parse::<u32>().ok())
                .unwrap_or(1);
            if count > 0 {
                *slot = Some((generator, count));
            }
        }
    }
    Ok(slots.into_iter().flatten().collect())
}

/// The concrete non-creature world entities generated room tables select
/// (vendors, chests, shrines, teleports). Creatures are emitted by the
/// encounter path instead.
pub fn generated_room_world_entities(
    archive: &Archive,
    zone: &str,
    seed: u32,
) -> Result<Vec<PlacedEntity>, ContentError> {
    let mut out = Vec::new();
    for table in generated_room_world_entity_tables(archive, zone, seed)? {
        let Some(classname) = world_entity_table_choice(archive, &table.table, &table.key)? else {
            continue;
        };
        if table.baked_chest && classname.to_ascii_lowercase().contains("chest") {
            continue;
        }
        if let Some(entity) = placed_entity_for_class(archive, &classname, table.pose, false)? {
            out.push(entity);
        }
    }

    // Generated room tiles bake their own `Entities` block: quest NCIs (the
    // Hermit's Lockbox on `elmforest_questfindring_*`), ordinary chests, and
    // the occasional vendor. `tile_encounter_markers` already covers the
    // `base.Encounter<N>` placements; this is the rest of that same block.
    // It must run for EVERY generated cell, not only authored room nodes:
    // loot-room tiles such as `cave_small_loot_*` carry the visible chests in
    // their own `Entities` block and are chosen by the maze, not listed in the
    // zone's `roomnode` set. The `Map` block is deliberately not read -- it is
    // client-only dressing.
    let Some(layout) = crate::maze::generate_layout_with_seed(archive, zone, seed)? else {
        return Ok(out);
    };
    for cell in &layout.cells {
        for tile in crate::maze::tile_world_entities(archive, &cell.tile_type)? {
            if is_encounter_marker(&tile.classname) || is_non_network(archive, &tile.classname) {
                continue;
            }
            let pose = [
                x256(cell.world_origin_x + tile.x),
                x256(cell.world_origin_y + tile.y),
                x256(tile.z),
                x256(tile.heading),
            ];
            if let Some(entity) =
                placed_entity_for_class(archive, &tile.classname, pose, tile.heading_stated)?
            {
                out.push(entity);
            }
        }
    }
    Ok(out)
}

/// Is this placement a mob marker (`base.Encounter`) rather than a world
/// entity? Encounter markers are spawned by the encounter path.
fn is_encounter_marker(classname: &str) -> bool {
    let leaf = classname.rsplit('.').next().unwrap_or(classname);
    if leaf.eq_ignore_ascii_case("Encounter") || leaf.eq_ignore_ascii_case("Encounter_Empty") {
        return true;
    }
    let Some(digits) = leaf
        .get(..9)
        .filter(|prefix| prefix.eq_ignore_ascii_case("Encounter"))
        .map(|_| &leaf[9..])
    else {
        return false;
    };
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// `Description.TrackActivatingPlayers`, the NCI readInit bit that appends
/// the optional activator list.
pub fn class_track_activating_players(
    archive: &Archive,
    classname: &str,
) -> Result<bool, ContentError> {
    Ok(
        class_description_property(archive, classname, "TrackActivatingPlayers")?
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("true") || value.trim() == "1"),
    )
}

/// The placed entity a table-selected class becomes, or `None` when it is a
/// creature (those go through the mob/encounter path).
fn placed_entity_for_class(
    archive: &Archive,
    classname: &str,
    pose: [u32; 4],
    heading_stated: bool,
) -> Result<Option<PlacedEntity>, ContentError> {
    if is_checkpoint_class(classname) || is_portal_class(classname) {
        return Ok(None);
    }
    if class_is_creature(archive, classname)? {
        return Ok(None);
    }
    if class_is_npc(archive, classname)? {
        return Ok(Some(PlacedEntity {
            classname: classname.to_string(),
            kind: PlacedKind::Npc,
            pose,
            heading_stated,
            level: Some(class_level(archive, classname)?.unwrap_or(1)),
            spawn_point: None,
            zone: None,
            color: None,
            width: None,
            height: None,
            blocking: false,
            track_activating_players: false,
        }));
    }
    let Some(level) = class_level(archive, classname)? else {
        if class_block_node(archive, classname)?.is_none() {
            return Ok(None);
        }
        return Ok(Some(PlacedEntity {
            classname: classname.to_string(),
            kind: if class_is_door(archive, classname)? {
                PlacedKind::Door
            } else if class_is_unit_interactive(archive, classname)? {
                PlacedKind::WorldEntityUnit
            } else {
                PlacedKind::WorldEntity
            },
            pose,
            heading_stated,
            level: None,
            spawn_point: None,
            zone: None,
            color: None,
            width: None,
            height: None,
            blocking: class_blocking(archive, classname)?,
            track_activating_players: class_track_activating_players(archive, classname)?,
        }));
    };
    Ok(Some(PlacedEntity {
        classname: classname.to_string(),
        kind: PlacedKind::Npc,
        pose,
        heading_stated,
        level: Some(level),
        spawn_point: None,
        zone: None,
        color: None,
        width: None,
        height: None,
        blocking: false,
        track_activating_players: false,
    }))
}

/// `<classname>` as a `game.pkg` path (`misc.ZonePortal_agg` -> `...gc`).
fn class_gc_path(classname: &str) -> String {
    format!("{}.gc", classname.replace('.', "/"))
}

/// The first block's `prop` in a class file (`Level`, `Color`), via the GC
/// parser. `Level = 5;` and `Color = 0xFFFF0000;` are both single tokens there.
fn block_prop(archive: &Archive, path: &str, name: &str) -> Result<Option<String>, ContentError> {
    let Some(bytes) = archive.read_path(path)? else {
        return Ok(None);
    };
    let text: String = bytes.iter().map(|&b| b as char).collect();
    let node = crate::gc::parse(&text);
    Ok(node
        .children
        .first()
        .and_then(|root| root.prop(name))
        .map(str::to_string))
}

fn parse_hex_u32(value: &str) -> Option<u32> {
    let text = value.trim();
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u32::from_str_radix(digits, 16).ok()
}

/// A `Width = 70;` / `Height = 60;` placement property.
fn parse_u16(value: &str) -> Option<u16> {
    value.trim().parse::<u16>().ok()
}

fn is_portal_class(classname: &str) -> bool {
    classname
        .to_ascii_lowercase()
        .starts_with("misc.zoneportal")
}

fn is_checkpoint_class(classname: &str) -> bool {
    classname
        .to_ascii_lowercase()
        .starts_with("world.checkpoints.")
}

/// The client-side-only prefixes a world places for itself (`misc.*` and the
/// checkpoints); everything else is a networked entity.
///
/// Waypoint subclasses outside `misc.*` (the PvP arenas' `wp_*` families) are
/// pathing anchors too, not entities the server may create. Shipping them as
/// network records desynchronises the CEM stream on arena entry.
fn is_non_network(archive: &Archive, classname: &str) -> bool {
    let lower = classname.to_ascii_lowercase();
    lower.starts_with("misc.")
        || lower.starts_with("world.checkpoints.")
        || lower.starts_with("base.placeholder")
        || lower == "base.encounter_empty"
        || class_is_waypoint(archive, classname)
}

/// Whether a class resolves to the waypoint base class, including subclasses
/// outside the `misc.Waypoint_*` namespace.
fn class_is_waypoint(archive: &Archive, classname: &str) -> bool {
    if is_waypoint_class(classname) {
        return true;
    }
    class_nodes_named(archive, classname)
        .ok()
        .is_some_and(|nodes| {
            nodes.iter().any(|(_, node)| {
                node.extends.as_deref().is_some_and(|parent| {
                    parent
                        .rsplit('.')
                        .next()
                        .is_some_and(|leaf| leaf.eq_ignore_ascii_case("Waypoint"))
                })
            })
        })
}

/// The root block's `prop`, as the parser reads a `.zone` ZoneDef.
fn zone_root_prop(file: &Block, name: &str) -> Option<String> {
    file.children
        .first()
        .and_then(|root| root.prop(name))
        .map(str::to_string)
}

/// One `* extends <class>` placement under a world's `Entities` block.
struct Placement {
    classname: String,
    name: String,
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
    heading: Option<f64>,
    props: Vec<(String, String)>,
}

impl Placement {
    /// `(x, y, z, heading)` in x256, or `None` when no position is stated.
    fn pose(&self) -> Option<[u32; 4]> {
        let x = self.x?;
        let y = self.y?;
        Some([
            x256(x),
            x256(y),
            x256(self.z.unwrap_or(0.0)),
            x256(self.heading.unwrap_or(0.0)),
        ])
    }

    /// `(x, y)` in the x256 wire domain, or `None` when no position is stated.
    fn wire_pos(&self) -> Option<(i64, i64)> {
        Some((x256(self.x?) as i64, x256(self.y?) as i64))
    }

    fn prop(&self, name: &str) -> Option<&str> {
        self.props
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Every placement under a world file's `Entities` block, in file order.
fn world_placements(file: &Block) -> Vec<Placement> {
    let Some(root) = file.children.first() else {
        return Vec::new();
    };
    let Some(entities) = root.child_ci("Entities") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for node in &entities.children {
        let target = node.extends.clone().unwrap_or_else(|| node.name.clone());
        if target.is_empty() || target.to_ascii_lowercase().starts_with("worldobjectgroup") {
            continue;
        }
        let (x, y, z) = node
            .prop("Position")
            .map(parse_triple)
            .unwrap_or((None, None, None));
        out.push(Placement {
            classname: target,
            name: node.prop("Name").map(unquote).unwrap_or_default(),
            x,
            y,
            z,
            heading: node.prop("Heading").and_then(parse_int_f64),
            props: node.props.clone(),
        });
    }
    out
}

/// Exact match of a waypoint by name first, then case-insensitive, as
/// `ZonePlacement.waypoint`. A waypoint is `misc.Waypoint` or any
/// `misc.Waypoint_*` subclass (`misc.Waypoint_StartRoom` is the common
/// start-room subclass).
/// A placement's spawn name, preferring its own `Name` and falling back to
/// the class's `Name` (PvP `wp_*` classes state the name in the class file).
fn waypoint_name(archive: &Archive, placement: &Placement) -> String {
    if !placement.name.is_empty() {
        return placement.name.clone();
    }
    class_property(archive, &placement.classname, "Name")
        .ok()
        .flatten()
        .map(|value| unquote(&value))
        .unwrap_or_default()
}

fn find_waypoint<'a>(placements: &'a [Placement], name: &str) -> Option<&'a Placement> {
    let want = name.to_ascii_lowercase();
    let mut fallback = None;
    for placement in placements
        .iter()
        .filter(|placement| is_waypoint_class(&placement.classname))
    {
        if placement.name == name {
            return Some(placement);
        }
        if fallback.is_none() && placement.name.to_ascii_lowercase() == want {
            fallback = Some(placement);
        }
    }
    fallback
}

/// Like [`find_waypoint`], but also accepts waypoint subclasses outside the
/// `misc.Waypoint_*` namespace (the PvP arena's `world.pvp.data.wp_*`).
fn find_waypoint_resolved<'a>(
    archive: &Archive,
    placements: &'a [Placement],
    name: &str,
) -> Option<&'a Placement> {
    find_waypoint(placements, name).or_else(|| {
        let want = name.to_ascii_lowercase();
        placements
            .iter()
            .filter(|placement| is_waypoint_placement(archive, placement))
            .find(|placement| placement.name.to_ascii_lowercase() == want)
    })
}

/// The first `misc.Waypoint_StartRoom`-style placement (a waypoint whose class
/// states `ZoneStart = true`), used as a hand-placed zone's entry when no
/// named/`Start` waypoint answers.
fn find_zone_start_waypoint<'a>(
    archive: &Archive,
    placements: &'a [Placement],
) -> Option<&'a Placement> {
    placements.iter().find(|placement| {
        is_waypoint_placement(archive, placement)
            && (placement
                .classname
                .eq_ignore_ascii_case("misc.Waypoint_StartRoom")
                || class_property(archive, &placement.classname, "ZoneStart")
                    .ok()
                    .flatten()
                    .is_some_and(|value| value.eq_ignore_ascii_case("true") || value.trim() == "1"))
    })
}

/// The first authored waypoint whose own name or class leaf carries `role`
/// (`start`/`respawn`), used by PvP arenas whose spawn classes are not named
/// `Start`.
fn find_waypoint_role<'a>(
    archive: &Archive,
    placements: &'a [Placement],
    role: &str,
) -> Option<&'a Placement> {
    let role = role.to_ascii_lowercase();
    placements.iter().find(|placement| {
        if !is_waypoint_placement(archive, placement) {
            return false;
        }
        // `misc.Waypoint_*` subclasses are handled by the named rungs above
        // (including the `StartRoom` fallback); this role fallback exists for
        // authored waypoint families outside that namespace, such as
        // `world.pvp.data.wp_ffa_start`.
        if is_waypoint_class(&placement.classname) {
            return false;
        }
        let class_leaf = placement
            .classname
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        placement.name.to_ascii_lowercase().contains(&role) || class_leaf.contains(&role)
    })
}

/// Whether a placement resolves to the waypoint base class. `game.pkg` names
/// several waypoint subclasses outside the `misc.Waypoint_*` namespace (the
/// PvP arena's `world.pvp.data.wp_*` is one), so checking only the class name
/// misses spawn points the zone actually authored.
fn is_waypoint_placement(archive: &Archive, placement: &Placement) -> bool {
    class_is_waypoint(archive, &placement.classname)
}

/// Whether `classname` is `misc.Waypoint` or a `misc.Waypoint_*` subclass.
fn is_waypoint_class(classname: &str) -> bool {
    let lower = classname.trim().to_ascii_lowercase();
    let base = WAYPOINT_CLASS.to_ascii_lowercase();
    lower == base || lower.starts_with(&format!("{base}_"))
}

/// `722` -> `722.0`, or `None` when the value is not an integer.
fn parse_int_f64(value: &str) -> Option<f64> {
    value.trim().parse::<i64>().ok().map(|v| v as f64)
}

/// The first three numbers of a `Position = x, y, z;` row.
fn parse_triple(value: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
    let numbers: Vec<f64> = value
        .split_whitespace()
        .filter_map(|token| token.parse::<f64>().ok())
        .collect();
    (
        numbers.first().copied(),
        numbers.get(1).copied(),
        numbers.get(2).copied(),
    )
}

/// A world-unit quantity as the x256 the client reads.
fn x256(value: f64) -> u32 {
    (value * 256.0).round() as i64 as u32
}

fn unquote(value: &str) -> String {
    let text = value.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text[1..text.len() - 1].to_string()
    } else {
        text.to_string()
    }
}

// -- the GC grammar subset -------------------------------------------------

#[derive(Debug, Default, Clone)]
pub(crate) struct Block {
    pub(crate) name: String,
    pub(crate) extends: Option<String>,
    pub(crate) props: Vec<(String, String)>,
    pub(crate) children: Vec<Block>,
}

impl Block {
    /// The value of a `Name = ...;` row (the last one wins, like the parser's dict).
    pub(crate) fn prop(&self, name: &str) -> Option<&str> {
        self.props
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn child_ci(&self, name: &str) -> Option<&Block> {
        self.children
            .iter()
            .find(|child| child.name.eq_ignore_ascii_case(name))
    }
}

pub(crate) fn parse(text: &str) -> Block {
    let tokens = tokenize(text);
    let mut parser = Parser { tokens, pos: 0 };
    let mut root = Block {
        name: "<file>".to_string(),
        ..Default::default()
    };
    parser.body(&mut root);
    root
}

struct Parser {
    tokens: Vec<String>,
    pos: usize,
}

impl Parser {
    fn peek(&self, ahead: usize) -> Option<&str> {
        self.tokens
            .get(self.pos + ahead)
            .map(|token| token.as_str())
    }

    fn body(&mut self, node: &mut Block) {
        while self.pos < self.tokens.len() {
            let token = self.tokens[self.pos].clone();
            match token.as_str() {
                "}" => {
                    self.pos += 1;
                    return;
                }
                "{" | "=" | ";" | "*" => {
                    self.pos += 1;
                    continue;
                }
                "extends" => {
                    self.pos += 1;
                    let target = self.peek(0).map(str::to_string);
                    self.pos += 1;
                    let mut child = Block {
                        name: "<anonymous>".to_string(),
                        extends: target,
                        ..Default::default()
                    };
                    if self.peek(0) == Some("{") {
                        self.pos += 1;
                        self.body(&mut child);
                    }
                    node.children.push(child);
                    continue;
                }
                _ => {}
            }
            if self.peek(1) == Some("=") {
                self.pos += 2;
                let value = self.value();
                node.props.push((token, value));
                continue;
            }
            self.pos += 1;
            let name = token;
            let mut extends = None;
            if self.peek(0) == Some("extends") {
                self.pos += 1;
                extends = self.peek(0).map(str::to_string);
                self.pos += 1;
            }
            let mut child = Block {
                name,
                extends,
                ..Default::default()
            };
            let has_body = self.peek(0) == Some("{");
            if has_body {
                self.pos += 1;
                self.body(&mut child);
            }
            node.children.push(child);
        }
    }

    fn value(&mut self) -> String {
        let mut out = Vec::new();
        while self.pos < self.tokens.len() && self.peek(0) != Some(";") {
            out.push(self.tokens[self.pos].clone());
            self.pos += 1;
        }
        self.pos += 1;
        out.join(" ")
    }
}

fn tokenize(text: &str) -> Vec<String> {
    let stripped = strip_comments(text);
    let chars: Vec<char> = stripped.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
            continue;
        }
        if matches!(c, '{' | '}' | '=' | ';' | ',' | '*') {
            out.push(c.to_string());
            i += 1;
            continue;
        }
        let starts_number = c.is_ascii_digit()
            || ((c == '-' || c == '+') && next_is_digit(&chars, i + 1))
            || (c == '.' && next_is_digit(&chars, i + 1));
        if starts_number {
            let start = i;
            if c == '-' || c == '+' {
                i += 1;
            }
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i < chars.len() && chars[i] == '.' && next_is_digit(&chars, i + 1) {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            out.push(chars[start..i].iter().collect());
            continue;
        }
        if c.is_ascii_alphanumeric() || c == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && is_ident_continue(chars[i]) {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
            continue;
        }
        i += 1;
    }
    out
}

fn next_is_digit(chars: &[char], at: usize) -> bool {
    chars.get(at).is_some_and(|c| c.is_ascii_digit())
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// Blank `//` and `/* */` comments without touching quoted strings.
fn strip_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '"' {
                j += 1;
            }
            let end = if j < chars.len() { j + 1 } else { j };
            out.extend(&chars[i..end]);
            i = end;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_signed_positions_and_headings() {
        let text = r#"
            * extends base.World {
                Entities {
                    Name = GCObject;
                    * extends misc.Waypoint {
                        Heading = -50;
                        Position = 720,460,40;
                        Name = Start;
                    }
                    * extends misc.Waypoint {
                        Heading = 90;
                        Position = 480,-191,50;
                        Name = "Quoted";
                    }
                    * extends misc.ZonePortal_agg {
                        Position = 1,2,3;
                        Name = Portal;
                    }
                }
            }
        "#;
        let placements = world_placements(&parse(text));
        let waypoints: Vec<&Placement> = placements
            .iter()
            .filter(|placement| placement.classname == WAYPOINT_CLASS)
            .collect();
        assert_eq!(waypoints.len(), 2);
        assert_eq!(waypoints[0].name, "Start");
        assert_eq!(
            waypoints[0].pose(),
            Some([184320, 117760, 10240, 0xFFFF_CE00])
        );
        assert_eq!(waypoints[1].name, "Quoted");
        assert_eq!(
            waypoints[1].pose(),
            Some([122880, 0xFFFF_4100, 12800, 23040])
        );
    }

    #[test]
    fn a_generated_worlds_room_nodes_and_their_spawn_names_are_read() {
        let text = r#"
            * extends base.world_elmforest {
                Generated = true;
                MazeWidth = 4;
                MazeHeight = 5;
                Map { }
                * extends base.StartRoom {
                    LinkToSpawn = start1;
                    SpawnName = start1;
                    GridY = 4;
                }
                * extends linkroomnode {
                    LinkToSpawn = start2;
                    SpawnName = start2;
                    GridY = 0;
                }
                * extends roomnode {
                    TileSet = tutorial_loot_;
                    GridY = 2;
                }
            }
        "#;
        let file = parse(text);
        let root = file.children.first().unwrap();
        let rooms: Vec<&Block> = root
            .children
            .iter()
            .filter(|node| is_room_node(&room_class(node)))
            .collect();
        assert_eq!(rooms.len(), 3);
        assert_eq!(room_spawn(rooms[0]), "start1");
        assert_eq!(room_spawn(rooms[1]), "start2");
        assert_eq!(room_spawn(rooms[2]), "");
        assert!(is_start_room(&room_class(rooms[0])));
        assert!(!is_start_room(&room_class(rooms[1])));
        // The maze grid the client clamps against.
        assert_eq!(clamp_grid(4, 5), 4);
        assert_eq!(clamp_grid(9, 5), 4);
        assert_eq!(clamp_grid(-1, 5), 2);
    }

    #[test]
    fn zone_root_prop_reads_the_anonymous_zone_def() {
        let text = "* extends ZoneDef {\n Name = Town;\n RespawnSpawnPoint = Start;\n}\n";
        let root = parse(text);
        assert_eq!(
            zone_root_prop(&root, "RespawnSpawnPoint").as_deref(),
            Some("Start")
        );
        assert_eq!(zone_root_prop(&root, "Name").as_deref(), Some("Town"));
    }

    #[test]
    fn find_waypoint_is_exact_then_case_insensitive() {
        let placements = vec![Placement {
            classname: WAYPOINT_CLASS.to_string(),
            name: "Start1".to_string(),
            x: Some(1.0),
            y: Some(2.0),
            z: None,
            heading: None,
            props: Vec::new(),
        }];
        assert!(find_waypoint(&placements, "Start").is_none());
        assert_eq!(find_waypoint(&placements, "start1").unwrap().name, "Start1");
        assert_eq!(find_waypoint(&placements, "Start1").unwrap().name, "Start1");
    }

    fn portal(name: &str, x: f64, y: f64, zone: &str, spawn: &str) -> Placement {
        Placement {
            classname: "misc.ZonePortal_agg".to_string(),
            name: name.to_string(),
            x: Some(x),
            y: Some(y),
            z: Some(0.0),
            heading: None,
            props: vec![
                ("Zone".to_string(), zone.to_string()),
                ("SpawnPoint".to_string(), spawn.to_string()),
            ],
        }
    }

    #[test]
    fn identify_prefers_name_then_wire_then_a_single_class() {
        let placements = vec![
            portal("A", 1.0, 2.0, "zoneA", "spawnA"),
            portal("B", 3.0, 4.0, "zoneB", "spawnB"),
        ];
        // Name is the strongest key.
        assert_eq!(
            identify_placement(&placements, Some("B"), Some("misc.ZonePortal_agg"), None)
                .unwrap()
                .prop("Zone"),
            Some("zoneB")
        );
        // Wire identifies a placement whose name the click does not carry.
        assert_eq!(
            identify_placement(
                &placements,
                None,
                Some("misc.ZonePortal_agg"),
                Some((256, 512))
            )
            .unwrap()
            .prop("Zone"),
            Some("zoneA")
        );
        // Two same-class placements with no name or wire is ambiguous.
        assert!(identify_placement(&placements, None, Some("misc.ZonePortal_agg"), None).is_none());
        // A class filter that matches nothing is no answer.
        assert!(identify_placement(&placements, None, Some("misc.Waypoint"), None).is_none());
    }

    #[test]
    fn identify_answers_a_sole_placement_by_classname_alone() {
        let placements = vec![portal(
            "ZonePortal",
            960.0,
            719.0,
            "dungeon00_level01",
            "Start1",
        )];
        let found = identify_placement(&placements, None, Some("misc.zoneportal_agg"), None)
            .expect("one portal of this class resolves");
        assert_eq!(found.prop("Zone"), Some("dungeon00_level01"));
        assert_eq!(found.prop("SpawnPoint"), Some("Start1"));
    }
}
