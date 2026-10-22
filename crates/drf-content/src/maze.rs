// SPDX-License-Identifier: GPL-3.0-or-later

//! Client-faithful dungeon maze generator and player-entry resolver.
//!
//! A faithful, std-only port of `dr_pyserver.managers.maze` (itself a
//! client-faithful port of the game's C# `MazeGenerator`) plus the entry-cell
//! selection from `dr_pyserver.managers.dungeon_spawner` (`_entry_cell` /
//! `_authored_spawn` / `entry_position`). Where the Python reads a database for
//! the level parameters and room nodes, this port reads the same values from
//! the shipped `.world` text; the per-tileset variant tiles and the authored
//! anchors are read from the `.tile` / `.gc` entries of the same archive.
//!
//! Pipeline order is load-bearing -- the MT19937 draw sequence must match the
//! client per step or layouts diverge:
//!
//! ```text
//! PlaceRoomNodes -> GenerateCorridors -> ApplyForcedRoomExits -> Sparsify
//!   -> RemoveDeadEnds -> SyncOpeningsFromNativeBits -> BuildResult
//! ```
//!
//! RNG is MT19937 seeded with the per-level *layout* seed (`layout_seed`), the
//! CRC32 of the lowercased zone name -- the value the server also puts in the
//! zone-connect (Assign) u32 so the client builds the same maze.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::corpus::ContentError;
use crate::pkg::Archive;
use crate::zone::{parse, read_text, world_tile_size, Block};

/// Default maze cell stride when a world archetype states no `TileSize`.
const DEFAULT_TILE_SIZE: i64 = 400;

/// Inherited grid pins keyed by `TierN.Section.Name` (lowercased).
type BaseRoomGrids = HashMap<String, (Option<i64>, Option<i64>)>;

// Public opening-direction indices (stored per cell).
const NORTH: u8 = 0;
const EAST: u8 = 1;
const SOUTH: u8 = 2;
const WEST: u8 = 3;

// Native cell-bitfield direction bits (match C# DIR_*).
const DIR_NORTH: u8 = 0x01;
const DIR_SOUTH: u8 = 0x02;
const DIR_EAST: u8 = 0x04;
const DIR_WEST: u8 = 0x08;
const CELL_ROOM: u8 = 0x10;

const ORDERED_DIRS: [u8; 4] = [DIR_NORTH, DIR_EAST, DIR_SOUTH, DIR_WEST];
const EMPTY_NEIGHBOR_DIRS: [u8; 4] = [DIR_NORTH, DIR_SOUTH, DIR_WEST, DIR_EAST];

const OPEN_NORTH: u8 = 1 << NORTH;
const OPEN_EAST: u8 = 1 << EAST;
const OPEN_SOUTH: u8 = 1 << SOUTH;
const OPEN_WEST: u8 = 1 << WEST;

/// The kinds whose room node marks the player's arrival point.
const ENTRANCE_KINDS: [&str; 2] = ["startroom", "mainentrance"];

// -- IEEE CRC32 (zlib.crc32) --------------------------------------------------

const CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// Standard IEEE CRC32 of `data` (the value `zlib.crc32` returns).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = (crc >> 8) ^ CRC32_TABLE[((crc ^ byte as u32) & 0xFF) as usize];
    }
    !crc
}

/// Replay/test-only override for one zone's layout seed (idempotent; first
/// call wins). The key is the lowercased zone token.
///
/// The seed is normally the CRC32 of the zone name and is fully derived, so
/// only a caller replaying a recorded session whose server pinned a different
/// seed sets this. Nothing in the server calls it, so production behaviour is
/// unchanged unless the replay harness's `--seed` sets it.
static LAYOUT_SEED_OVERRIDE: std::sync::OnceLock<(String, u32)> = std::sync::OnceLock::new();

/// Pin `zone`'s maze layout seed for the rest of the process. Additive only;
/// see [`LAYOUT_SEED_OVERRIDE`].
pub fn set_layout_seed_override(zone: &str, seed: u32) {
    let _ = LAYOUT_SEED_OVERRIDE.set((zone.to_lowercase(), seed));
}

/// Deterministic per-level maze *layout* seed (uint32).
///
/// The client builds its maze from the uint32 the server sends in the
/// zone-connect packet (13/0x00); the generator must use the same value so the
/// terrain matches. A stable CRC32 of the lowercased zone name means every
/// joiner to one level agrees and different levels get different layouts.
///
/// A replay/test override pinned by [`set_layout_seed_override`] wins for its
/// zone so a recorded session's seed round-trips exactly.
pub fn layout_seed(zone: &str) -> u32 {
    let key = zone.to_lowercase();
    if let Some((zone_override, seed)) = LAYOUT_SEED_OVERRIDE.get() {
        if *zone_override == key {
            return *seed;
        }
    }
    crc32(key.as_bytes())
}

// -- MT19937 (binary-exact with the game's MersenneTwister) -------------------

/// MT19937 -- 32-bit output, binary-exact with the client's generator.
pub struct MersenneTwister {
    mt: [u32; 624],
    mti: i32,
}

impl MersenneTwister {
    const N: i32 = 624;
    const M: i32 = 397;
    const MATRIX_A: u32 = 0x9908_B0DF;
    const UPPER_MASK: u32 = 0x8000_0000;
    const LOWER_MASK: u32 = 0x7FFF_FFFF;
    const DEFAULT_SEED: u32 = 0x1105;

    /// A generator seeded with `seed`.
    pub fn new(seed: u32) -> MersenneTwister {
        let mut state = MersenneTwister {
            mt: [0u32; 624],
            mti: Self::N + 1,
        };
        state.seed(seed);
        state
    }

    fn seed(&mut self, s: u32) {
        self.mt[0] = s;
        let mut i = 1usize;
        while i < Self::N as usize {
            let prev = self.mt[i - 1];
            self.mt[i] = 1_812_433_253u32
                .wrapping_mul(prev ^ (prev >> 30))
                .wrapping_add(i as u32);
            i += 1;
        }
        self.mti = Self::N;
    }

    /// Generate a tempered 32-bit value.
    pub fn generate(&mut self) -> u32 {
        if self.mti >= Self::N {
            if self.mti == Self::N + 1 {
                self.seed(Self::DEFAULT_SEED);
            }
            self.twist();
        }
        let mut y = self.mt[self.mti as usize];
        self.mti += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9D2C_5680;
        y ^= (y << 15) & 0xEFC6_0000;
        y ^= y >> 18;
        y
    }

    /// C# `NextInt` -- `Generate(min, max-1)` (inclusive both ends).
    fn next_int(&mut self, min_inclusive: i64, max_exclusive: i64) -> i64 {
        if max_exclusive <= min_inclusive {
            return min_inclusive;
        }
        let min = min_inclusive;
        let max = max_exclusive - 1;
        let rng = ((max - min + 1) as u64) & 0xFFFF_FFFF;
        if rng == 0 {
            return min;
        }
        (self.generate() as u64 % rng) as i64 + min
    }

    fn twist(&mut self) {
        let n = Self::N as usize;
        for kk in 0..n {
            let y = (self.mt[kk] & Self::UPPER_MASK) | (self.mt[(kk + 1) % n] & Self::LOWER_MASK);
            self.mt[kk] = self.mt[(kk + Self::M as usize) % n] ^ (y >> 1);
            if y & 1 != 0 {
                self.mt[kk] ^= Self::MATRIX_A;
            }
        }
        self.mti = 0;
    }
}

// -- public result types ------------------------------------------------------

/// A designer-placed anchor inside a tile (local tile-frame offset + facing).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AuthoredAnchor {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub heading: f64,
}

/// A resolved maze entry pose in world units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntryPosition {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub heading: f64,
}

/// A room node actually placed into the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedRoomNode {
    pub source_index: i64,
    pub tile_set: String,
    pub tile_type: String,
    pub grid_x: i64,
    pub grid_y: i64,
}

/// One generated maze cell with its world-frame footprint.
#[derive(Debug, Clone, PartialEq)]
pub struct MazeCell {
    pub grid_x: i64,
    pub grid_y: i64,
    pub world_grid_y: i64,
    pub connections: String,
    pub tile_type: String,
    pub world_origin_x: f64,
    pub world_origin_y: f64,
    pub world_center_x: f64,
    pub world_center_y: f64,
    pub tile_size: f64,
}

/// The client's `MiniMapExplored` grid for a generated maze.
///
/// `World::initiateMiniMap` builds the explored bitset from the world's static
/// object bounds. For a generated maze those bounds are the placed cells'
/// footprints: the client pads each axis by 128 world units and snapshots one
/// bit per 128-unit cell, packing 32 cells into each `u32` word.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MazeMinimap {
    /// The left edge of the first minimap cell (the maze minimum minus 128).
    pub min_x: f64,
    /// The bottom edge of the first minimap cell (the maze minimum minus 128).
    pub min_y: f64,
    /// Explored cells across, including the two padded edge cells.
    pub columns: usize,
    /// Explored rows.
    pub rows: usize,
    /// `u32` words per row (`ceil(columns / 32)`).
    pub words_per_row: usize,
}

impl MazeMinimap {
    /// Total `u32` words in the Ready explored array.
    pub fn words(&self) -> usize {
        self.rows * self.words_per_row
    }

    /// The minimap cell containing a world point, or `None` when outside the
    /// padded map. World coordinates are ordinary world units (not x256).
    pub fn cell_at(&self, x: f64, y: f64) -> Option<(usize, usize)> {
        let gx = ((x - self.min_x) / 128.0).floor();
        let gy = ((y - self.min_y) / 128.0).floor();
        if !gx.is_finite() || !gy.is_finite() || gx < 0.0 || gy < 0.0 {
            return None;
        }
        let gx = gx as usize;
        let gy = gy as usize;
        if gx >= self.columns || gy >= self.rows {
            return None;
        }
        Some((gx, gy))
    }

    /// Mark one world point and its eight neighbours explored in `words`.
    pub fn mark_explored(&self, words: &mut [u32], x: f64, y: f64) {
        let Some((gx, gy)) = self.cell_at(x, y) else {
            return;
        };
        if words.len() < self.words() {
            return;
        }
        for dy in -1i64..=1 {
            let row = gy as i64 + dy;
            if row < 0 || row >= self.rows as i64 {
                continue;
            }
            for dx in -1i64..=1 {
                let col = gx as i64 + dx;
                if col < 0 || col >= self.columns as i64 {
                    continue;
                }
                let word = row as usize * self.words_per_row + col as usize / 32;
                let bit = col as usize % 32;
                words[word] |= 1u32 << bit;
            }
        }
    }
}

/// The client's minimap grid for `layout`, derived from the placed cells.
///
/// The exact client formula (from `MiniMapExplored::init`): pad each axis by
/// 128 units, snap to 128-unit cells, then pack 32 cells per word. A 5x5
/// `dungeon01_level01` maze yields 17 words; `dungeon00_level01` yields 18,
/// matching the counts in the client's own Ready warning.
pub fn minimap_for_layout(layout: &MazeLayout) -> Option<MazeMinimap> {
    if layout.cells.is_empty() {
        return None;
    }
    let min_x = layout
        .cells
        .iter()
        .map(|cell| cell.world_origin_x)
        .fold(f64::INFINITY, f64::min);
    let min_y = layout
        .cells
        .iter()
        .map(|cell| cell.world_origin_y)
        .fold(f64::INFINITY, f64::min);
    let max_x = layout
        .cells
        .iter()
        .map(|cell| cell.world_origin_x + cell.tile_size)
        .fold(f64::NEG_INFINITY, f64::max);
    let max_y = layout
        .cells
        .iter()
        .map(|cell| cell.world_origin_y + cell.tile_size)
        .fold(f64::NEG_INFINITY, f64::max);
    let columns = (((max_x + 128.0) - (min_x - 128.0)) / 128.0).floor() as i64 + 1;
    let rows = (((max_y + 128.0) - (min_y - 128.0)) / 128.0).floor() as i64 + 1;
    if columns <= 0 || rows <= 0 {
        return None;
    }
    let columns = usize::try_from(columns).ok()?;
    let rows = usize::try_from(rows).ok()?;
    Some(MazeMinimap {
        min_x: min_x - 128.0,
        min_y: min_y - 128.0,
        columns,
        rows,
        words_per_row: columns.div_ceil(32),
    })
}

/// A generated maze: its cells and the room nodes the RNG placed.
#[derive(Debug, Clone, PartialEq)]
pub struct MazeLayout {
    pub cells: Vec<MazeCell>,
    pub placed_room_nodes: Vec<PlacedRoomNode>,
    /// Every parsed room node, including the link a generated room's own
    /// `LinkToZone`/`LinkToSpawn` states.
    pub room_nodes: Vec<MazeRoomNode>,
    pub tile_size: i64,
}

/// One `.world` room node as the portal derivation needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MazeRoomNode {
    pub source_index: i64,
    pub node_kind: String,
    pub tile_set: String,
    pub grid_x: Option<i64>,
    pub grid_y: Option<i64>,
    pub chance: i64,
    pub spawn_name: String,
    pub link_to_zone: Option<String>,
    pub link_to_spawn: Option<String>,
    /// `WaypointName`, the named arrival/exit anchor a `GoToObjective` can
    /// target (the offshoot rooms' `*_off1a_teleporter`).
    pub waypoint_name: Option<String>,
    /// `EncounterWorldEntityTable` / `...2` / `...3`, keyed by selector.
    pub encounter_world_entity_tables: Vec<(u8, String)>,
}

/// The resolved entry cell and its authored anchor.
#[derive(Debug, Clone, PartialEq)]
pub struct MazeEntry {
    pub grid_x: i64,
    pub grid_y: i64,
    pub world_origin_x: f64,
    pub world_origin_y: f64,
    pub world_center_x: f64,
    pub world_center_y: f64,
    pub tile_type: String,
    /// The authored `SpawnPoint` waypoint (else the `ZonePortal` anchor).
    pub anchor: Option<AuthoredAnchor>,
}

/// One requested room placement (C# `RoomNodeSpec`).
#[derive(Debug, Clone)]
struct RoomNodeSpec {
    tile_set: String,
    grid_x: Option<i64>,
    grid_y: Option<i64>,
    chance: i64,
    source_index: i64,
}

/// One `*.world` room node (the fields the maze and entry resolver need).
#[derive(Debug, Clone)]
struct WorldRoomNode {
    source_index: i64,
    node_kind: String,
    tile_set: String,
    grid_x: Option<i64>,
    grid_y: Option<i64>,
    chance: i64,
    spawn_name: String,
    link_to_zone: Option<String>,
    link_to_spawn: Option<String>,
    waypoint_name: Option<String>,
    encounter_world_entity_tables: Vec<(u8, String)>,
}

// -- tile content reads -------------------------------------------------------

/// The final path component of an archive path (leaf filename).
fn leaf_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The final dotted component of a class path (`misc.waypoint` -> `waypoint`).
fn dotted_leaf(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

/// Does `rem` match `(?:1[nsew])+(?:_[a-z])?` in full?
fn variant_suffix_matches(rem: &str) -> bool {
    let b = rem.as_bytes();
    let mut i = 0usize;
    let mut dirs = 0usize;
    while i + 1 < b.len() && b[i] == b'1' && matches!(b[i + 1], b'n' | b's' | b'e' | b'w') {
        i += 2;
        dirs += 1;
    }
    if dirs == 0 {
        return false;
    }
    if i == b.len() {
        return true;
    }
    b[i] == b'_' && i + 2 == b.len() && b[i + 1].is_ascii_lowercase()
}

/// A room-node `tile_set`'s concrete variant tile leaves, sorted alphabetically
/// (the client's per-tileset variant order).
fn variants_for(archive: &Archive, tile_set: &str) -> Vec<String> {
    if tile_set.is_empty() {
        return Vec::new();
    }
    let base = tile_set.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for entry in archive.entries() {
        if entry.suffix != ".tile" {
            continue;
        }
        let leaf = leaf_name(&entry.name).to_lowercase();
        if let Some(rem) = leaf.strip_prefix(&base) {
            if variant_suffix_matches(rem) {
                out.push(leaf);
            }
        }
    }
    out.sort();
    out
}

/// Read a concrete tile's own text (by leaf match), if the archive holds one.
fn read_tile_text(archive: &Archive, tile_type: &str) -> Result<Option<String>, ContentError> {
    if tile_type.is_empty() {
        return Ok(None);
    }
    let want = tile_type.to_lowercase();
    for entry in archive.entries() {
        if entry.suffix != ".tile" {
            continue;
        }
        if leaf_name(&entry.name).to_lowercase() == want {
            return read_text(archive, &format!("{}{}", entry.name, entry.suffix));
        }
    }
    Ok(None)
}

/// Read a dotted `extends` target (`tiles.x.y.z` -> `.gc` then `.tile`).
fn read_extends_text(archive: &Archive, dotted: &str) -> Result<Option<String>, ContentError> {
    if dotted.is_empty() {
        return Ok(None);
    }
    let rel = dotted.replace('.', "/");
    for ext in [".gc", ".tile"] {
        if let Some(text) = read_text(archive, &format!("{rel}{ext}"))? {
            return Ok(Some(text));
        }
    }
    Ok(None)
}

/// One placed sub-asset within a tile: its source path + local transform.
struct Placement {
    extends_path: String,
    x: f64,
    y: f64,
    z: f64,
    heading: f64,
    heading_stated: bool,
    name: String,
}

fn parse_vector3(raw: &str) -> Option<(f64, f64, f64)> {
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let x = parts[0].trim().parse::<f64>().ok()?;
    let y = parts[1].trim().parse::<f64>().ok()?;
    let z = parts[2].trim().parse::<f64>().ok()?;
    Some((x, y, z))
}

fn parse_f64(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok()
}

fn parse_i64(raw: &str) -> Option<i64> {
    raw.trim().parse::<i64>().ok()
}

fn unquote(value: &str) -> String {
    let text = value.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text[1..text.len() - 1].to_string()
    } else {
        text.to_string()
    }
}

/// Flatten a tile node's placements into `out` (anonymous blocks with a
/// `Position`), matching the reference `_collect`: anonymous children first, then
/// named children.
fn collect(node: &Block, out: &mut Vec<Placement>) {
    if node.name == "<anonymous>" {
        if let (Some(extends), Some(position)) = (node.extends.as_deref(), node.prop("Position")) {
            if let Some((x, y, z)) = parse_vector3(position) {
                out.push(Placement {
                    extends_path: extends.to_string(),
                    x,
                    y,
                    z,
                    heading: node.prop("Heading").and_then(parse_f64).unwrap_or(0.0),
                    heading_stated: node.prop("Heading").is_some(),
                    name: node.prop("Name").map(unquote).unwrap_or_default(),
                });
            }
        }
    }
    for child in &node.children {
        if child.name == "<anonymous>" {
            collect(child, out);
        }
    }
    for child in &node.children {
        if child.name != "<anonymous>" {
            collect(child, out);
        }
    }
}

/// Flattened placements inherited from a tile's `extends` base, recursively.
fn load_base_placements(
    archive: &Archive,
    extends: Option<&str>,
    depth: usize,
    out: &mut Vec<Placement>,
) -> Result<(), ContentError> {
    if depth >= 6 {
        return Ok(());
    }
    let Some(extends) = extends else {
        return Ok(());
    };
    if extends.is_empty() {
        return Ok(());
    }
    let Some(text) = read_extends_text(archive, extends)? else {
        return Ok(());
    };
    let file = parse(&text);
    let Some(top) = file.children.first() else {
        return Ok(());
    };
    load_base_placements(archive, top.extends.as_deref(), depth + 1, out)?;
    collect(top, out);
    Ok(())
}

/// The `misc.Waypoint` named `SpawnPoint` (else the first `misc.Waypoint`).
fn player_spawn_anchor(placements: &[Placement]) -> Option<AuthoredAnchor> {
    let mut fallback = None;
    for p in placements {
        if !dotted_leaf(&p.extends_path).eq_ignore_ascii_case("waypoint") {
            continue;
        }
        let anchor = AuthoredAnchor {
            x: p.x,
            y: p.y,
            z: p.z,
            heading: p.heading,
        };
        if p.name.eq_ignore_ascii_case("spawnpoint") {
            return Some(anchor);
        }
        if fallback.is_none() {
            fallback = Some(anchor);
        }
    }
    fallback
}

/// The first `misc.ZonePortal*` model placement (warp-gate position).
fn zone_portal_anchor(placements: &[Placement]) -> Option<AuthoredAnchor> {
    for p in placements {
        if dotted_leaf(&p.extends_path)
            .to_lowercase()
            .contains("zoneportal")
        {
            return Some(AuthoredAnchor {
                x: p.x,
                y: p.y,
                z: p.z,
                heading: p.heading,
            });
        }
    }
    None
}

/// `(player_spawn, zone_portal)` authored anchors for a tile, from its own
/// content plus its inherited base shell.
fn tile_authored_anchors(
    archive: &Archive,
    tile_type: &str,
) -> Result<(Option<AuthoredAnchor>, Option<AuthoredAnchor>), ContentError> {
    let Some(text) = read_tile_text(archive, tile_type)? else {
        return Ok((None, None));
    };
    let file = parse(&text);
    let Some(top) = file.children.first() else {
        return Ok((None, None));
    };
    let mut placements = Vec::new();
    load_base_placements(archive, top.extends.as_deref(), 0, &mut placements)?;
    collect(top, &mut placements);
    Ok((
        player_spawn_anchor(&placements),
        zone_portal_anchor(&placements),
    ))
}

/// The authored local `(x, y, z, heading)` for the player's arrival.
fn authored_spawn(
    archive: &Archive,
    tile_type: &str,
) -> Result<Option<AuthoredAnchor>, ContentError> {
    let (player, portal) = tile_authored_anchors(archive, tile_type)?;
    Ok(player.or(portal))
}

/// One encounter spawn point baked into a `.tile` (local tile frame).
///
/// A tile's spawn points are placements extending ``base.Encounter<N>``
/// (``N`` = the authored encounter-area budget hint; 0 when absent). The
/// position is a local offset added to the cell origin, exactly as the client
/// reads it. This is the port of the reference
/// `dungeon_spawner._tile_encounter_markers`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileEncounterMarker {
    /// Local x offset from the cell origin.
    pub x: f64,
    /// Local y offset from the cell origin.
    pub y: f64,
    /// Local z (floor height) offset.
    pub z: f64,
    /// Local facing.
    pub heading: f64,
    /// The `N` of `base.Encounter<N>` (0 when the class states no digits).
    pub budget: u32,
}

/// The encounter spawn points a tile carries, in file (placement) order,
/// including those inherited from its base shell.
pub fn tile_encounter_markers(
    archive: &Archive,
    tile_type: &str,
) -> Result<Vec<TileEncounterMarker>, ContentError> {
    let Some(text) = read_tile_text(archive, tile_type)? else {
        return Ok(Vec::new());
    };
    let file = parse(&text);
    let Some(top) = file.children.first() else {
        return Ok(Vec::new());
    };
    let mut placements = Vec::new();
    load_base_placements(archive, top.extends.as_deref(), 0, &mut placements)?;
    collect(top, &mut placements);
    let mut out = Vec::new();
    for p in &placements {
        if let Some(budget) = encounter_budget(&p.extends_path) {
            out.push(TileEncounterMarker {
                x: p.x,
                y: p.y,
                z: p.z,
                heading: p.heading,
                budget,
            });
        }
    }
    Ok(out)
}

/// One networked world entity baked into a `.tile`'s `Entities` block.
///
/// Generated rooms bake their non-creature entities here rather than in the
/// `<zone>.world` `Entities` block: quest NCIs such as the Hermit's Lockbox,
/// ordinary chests, and occasionally vendors. Creature markers
/// (`base.Encounter<N>`) and client-only `misc.*` helpers are filtered by the
/// caller. The position is local to the tile; add the placed cell's origin.
#[derive(Debug, Clone, PartialEq)]
pub struct TileWorldEntity {
    /// The dotted archetype the placement extends.
    pub classname: String,
    /// Local x offset from the cell origin.
    pub x: f64,
    /// Local y offset from the cell origin.
    pub y: f64,
    /// Local z (floor height).
    pub z: f64,
    /// Local facing.
    pub heading: f64,
    /// Whether the placement stated `Heading` (its absence authors 0).
    pub heading_stated: bool,
}

/// The world entities a tile's `Entities` block places, in file order,
/// including those inherited from its base shell.
///
/// Only `Entities` is read. The tile's `Map` block is static client dressing
/// (`lights`, terrain, clutter) and must never become a networked entity.
pub fn tile_world_entities(
    archive: &Archive,
    tile_type: &str,
) -> Result<Vec<TileWorldEntity>, ContentError> {
    let Some(text) = read_tile_text(archive, tile_type)? else {
        return Ok(Vec::new());
    };
    let file = parse(&text);
    let Some(top) = file.children.first() else {
        return Ok(Vec::new());
    };
    let mut placements = Vec::new();
    load_base_entity_placements(archive, top.extends.as_deref(), 0, &mut placements)?;
    collect_entities(top, &mut placements);
    Ok(placements
        .into_iter()
        .map(|p| TileWorldEntity {
            classname: p.extends_path,
            x: p.x,
            y: p.y,
            z: p.z,
            heading: p.heading,
            heading_stated: p.heading_stated,
        })
        .collect())
}

/// Collect only a top-level `Entities` block's anonymous placements.
fn collect_entities(node: &Block, out: &mut Vec<Placement>) {
    for child in &node.children {
        if child.name.eq_ignore_ascii_case("Entities") {
            collect(child, out);
        }
    }
}

/// Flattened placements inherited from a tile base's own `Entities` block.
fn load_base_entity_placements(
    archive: &Archive,
    extends: Option<&str>,
    depth: usize,
    out: &mut Vec<Placement>,
) -> Result<(), ContentError> {
    if depth >= 6 {
        return Ok(());
    }
    let Some(extends) = extends else {
        return Ok(());
    };
    if extends.is_empty() {
        return Ok(());
    }
    let Some(text) = read_extends_text(archive, extends)? else {
        return Ok(());
    };
    let file = parse(&text);
    let Some(top) = file.children.first() else {
        return Ok(());
    };
    load_base_entity_placements(archive, top.extends.as_deref(), depth + 1, out)?;
    collect_entities(top, out);
    Ok(())
}

/// The `N` of a placement extending a class whose leaf is `encounter<N>`
/// (case-insensitive; empty digits are 0), or `None` for any other class.
fn encounter_budget(extends_path: &str) -> Option<u32> {
    let leaf = dotted_leaf(extends_path).to_lowercase();
    let rest = leaf.strip_prefix("encounter")?;
    if rest.chars().all(|c| c.is_ascii_digit()) {
        Some(rest.parse::<u32>().unwrap_or(0))
    } else {
        None
    }
}

// -- tile ground/walkability --------------------------------------------------

/// The client PathMap's node resolution: one node per 10 world units.
pub const PATH_NODE_SIZE: f64 = 10.0;
/// A collision heightmap surface taller than this (world units) is a wall.
///
/// `.cobj` heightmap entries are in **tenths** of a world unit: the client's
/// `HybridCollisionObject::getHeight` divides the bilinear result by 10 and
/// `collideHeight` scales each sample by `0.1`. The reference compares the raw entry against 30
/// (tenths), i.e. 3 world units; the client's threshold is 30 *world* units,
/// so the raw comparison is `> 300`.
const WALL_HEIGHT_THRESHOLD: f64 = 30.0;
const WALL_HEIGHT_THRESHOLD_RAW: f64 = WALL_HEIGHT_THRESHOLD * 10.0;
/// The vertical band a sub-shape-2 bbox must overlap to block walking.
const WALKING_Z_MIN: f64 = 0.0;
const WALKING_Z_MAX: f64 = WALL_HEIGHT_THRESHOLD;

/// Round half to even, matching Python's `round()` / C# `Math.Round()`.
///
/// The reference pathmap builder stamps collision cells with
/// `round((world - min) / 10)`. Rust's `f64::round` rounds halves away from
/// zero, which shifts every cell whose centre lands on a half-node boundary
/// (`world ≡ 5 mod 10`) one node in the +x/+y direction. `.cobj` sub-shape
/// cells are 5 units, so this is not a corner case: half of every stamp lands
/// exactly on the boundary. Banker's rounding is load-bearing for parity.
fn round_half_even(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let floor = value.floor();
    if (value - floor - 0.5).abs() < 1e-9 {
        if floor.rem_euclid(2.0) == 0.0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        value.round()
    }
}

/// One `HybridCollisionObject`: the two collision sub-shapes of a `.cobj`.
struct CobjGrid {
    cell_size1: i32,
    origin_x1: i32,
    origin_y1: i32,
    width1: i32,
    height1: i32,
    heightmap: Vec<u16>,
    cell_size2: i32,
    origin_x2: i32,
    origin_y2: i32,
    origin_z2: i32,
    width2: i32,
    height2: i32,
    cells: Vec<Vec<(i16, i16)>>,
}

fn read_i32(bytes: &[u8], pos: &mut usize) -> Option<i32> {
    let end = pos.checked_add(4)?;
    let value = i32::from_le_bytes(bytes.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(value)
}

fn read_u16(bytes: &[u8], pos: &mut usize) -> Option<u16> {
    let end = pos.checked_add(2)?;
    let value = u16::from_le_bytes(bytes.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(value)
}

fn read_i16(bytes: &[u8], pos: &mut usize) -> Option<i16> {
    read_u16(bytes, pos).map(|v| v as i16)
}

/// Parse a `HybridCollisionObject` `.cobj` (the format the client's
/// `CollisionObject::readObject` reads). `None` on any malformed/truncated file.
fn parse_cobj(bytes: &[u8]) -> Option<CobjGrid> {
    const HEADER: usize = 27;
    if bytes.len() < HEADER || &bytes[1..22] != b"HybridCollisionObject" {
        return None;
    }
    let mut pos = HEADER;
    let cell_size1 = read_i32(bytes, &mut pos)?;
    let origin_x1 = read_i32(bytes, &mut pos)?;
    let origin_y1 = read_i32(bytes, &mut pos)?;
    let width1 = read_i32(bytes, &mut pos)?;
    let height1 = read_i32(bytes, &mut pos)?;
    if !(0..=1024).contains(&width1) || !(0..=1024).contains(&height1) {
        return None;
    }
    let count1 = (width1 as usize).checked_mul(height1 as usize)?;
    let mut heightmap = Vec::with_capacity(count1);
    for _ in 0..count1 {
        heightmap.push(read_u16(bytes, &mut pos)?);
    }
    let cell_size2 = read_i32(bytes, &mut pos)?;
    let origin_x2 = read_i32(bytes, &mut pos)?;
    let origin_y2 = read_i32(bytes, &mut pos)?;
    let origin_z2 = read_i32(bytes, &mut pos)?;
    let width2 = read_i32(bytes, &mut pos)?;
    let height2 = read_i32(bytes, &mut pos)?;
    let _depth2 = read_i32(bytes, &mut pos)?;
    if !(0..=1024).contains(&width2) || !(0..=1024).contains(&height2) {
        return None;
    }
    let count2 = (width2 as usize).checked_mul(height2 as usize)?;
    let mut cells = Vec::with_capacity(count2);
    for _ in 0..count2 {
        let bbox_count = read_u16(bytes, &mut pos)? as usize;
        let mut bboxes = Vec::with_capacity(bbox_count);
        for _ in 0..bbox_count {
            bboxes.push((read_i16(bytes, &mut pos)?, read_i16(bytes, &mut pos)?));
        }
        cells.push(bboxes);
    }
    Some(CobjGrid {
        cell_size1,
        origin_x1,
        origin_y1,
        width1,
        height1,
        heightmap,
        cell_size2,
        origin_x2,
        origin_y2,
        origin_z2,
        width2,
        height2,
        cells,
    })
}

/// Index the archive's `.cobj` files by leaf filename (case-folded).
fn cobj_index(archive: &Archive) -> HashMap<String, &crate::pkg::Entry> {
    let mut out = HashMap::new();
    for entry in archive.entries() {
        if entry.suffix != ".cobj" {
            continue;
        }
        out.insert(leaf_name(&entry.name).to_lowercase(), entry);
    }
    out
}

/// A per-instance walkability grid built exactly like the client
/// `PathMap::Build` / the reference `pathmap_builder.build`.
///
/// Nodes are 10 world units. Every placed `.cobj` is transformed by its
/// placement `Position` + `Heading` and stamped into the grid. Sub-shape-1
/// heightmap cells taller than [`WALL_HEIGHT_THRESHOLD`] (and `NoWalk` floors)
/// are blocked; the rest record the real world floor height
/// (`placement.z + heightmap/10`). Sub-shape-2 bbox stacks overlapping the
/// walking band block their cell. A node is walkable only when some floor
/// geometry covers it and nothing blocks it (uncovered holes are not walkable).
#[derive(Debug, Clone)]
pub struct DungeonPathMap {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    grid_w: i32,
    grid_h: i32,
    floor: HashMap<(i32, i32), f64>,
    blocked: HashSet<(i32, i32)>,
    fallback_open: HashSet<(i32, i32)>,
}

impl DungeonPathMap {
    /// World point → node index, matching the reference `round()` (banker's).
    fn node_key(&self, x: f64, y: f64) -> (i32, i32) {
        (
            round_half_even((x - self.min_x) / PATH_NODE_SIZE) as i32,
            round_half_even((y - self.min_y) / PATH_NODE_SIZE) as i32,
        )
    }

    fn in_bounds(&self, key: (i32, i32)) -> bool {
        key.0 >= 0 && key.0 < self.grid_w && key.1 >= 0 && key.1 < self.grid_h
    }

    fn covered(&self, key: (i32, i32)) -> bool {
        self.floor.contains_key(&key) || self.fallback_open.contains(&key)
    }

    /// Is the node at world `(x, y)` covered by floor and unblocked?
    pub fn is_walkable(&self, x: f64, y: f64) -> bool {
        let key = self.node_key(x, y);
        self.covered(key) && !self.blocked.contains(&key)
    }

    /// The exact node height at `(x, y)`, or `None` when that node is not
    /// walkable (the reference's `height_at` also searches outward; see
    /// [`Self::get_height_at`]).
    pub fn height_at(&self, x: f64, y: f64) -> Option<f64> {
        let key = self.node_key(x, y);
        if self.blocked.contains(&key) || !self.covered(key) {
            return None;
        }
        Some(self.floor.get(&key).copied().unwrap_or(0.0))
    }

    /// The node height at `(x, y)`, searching outward to radius 3 for a
    /// walkable node like the reference `PathMap.get_height_at`, else
    /// `default_height`.
    pub fn get_height_at(&self, x: f64, y: f64, default_height: f64) -> f64 {
        if x < self.min_x || x > self.max_x || y < self.min_y || y > self.max_y {
            return default_height;
        }
        let (gx, gy) = self.node_key(x, y);
        if let Some(height) = self.node_height(gx, gy) {
            return height;
        }
        for radius in 1i32..=3 {
            for dx in -radius..=radius {
                for dy in -radius..=radius {
                    if dx.abs() != radius && dy.abs() != radius {
                        continue;
                    }
                    if let Some(height) = self.node_height(gx + dx, gy + dy) {
                        return height;
                    }
                }
            }
        }
        default_height
    }

    fn node_height(&self, gx: i32, gy: i32) -> Option<f64> {
        let key = (gx, gy);
        if !self.in_bounds(key) || self.blocked.contains(&key) || !self.covered(key) {
            return None;
        }
        Some(self.floor.get(&key).copied().unwrap_or(0.0))
    }

    /// All 4-connected regions of walkable nodes.
    ///
    /// The collision map can contain isolated floor islands (raised props,
    /// sealed side rooms, geometry that is not on the client's navigation
    /// mesh). Callers use the regions to keep spawns on a floor layer that a
    /// unit can actually reach.
    pub fn walkable_components(&self) -> Vec<HashSet<(i32, i32)>> {
        let mut seen: HashSet<(i32, i32)> = HashSet::new();
        let mut components = Vec::new();
        for &start in self.floor.keys().chain(self.fallback_open.iter()) {
            if seen.contains(&start) || !self.walkable_node(start) {
                continue;
            }
            let mut region = HashSet::new();
            let mut stack = vec![start];
            while let Some(key) = stack.pop() {
                if !seen.insert(key) || !self.walkable_node(key) {
                    continue;
                }
                region.insert(key);
                let (gx, gy) = key;
                for next in [(gx + 1, gy), (gx - 1, gy), (gx, gy + 1), (gx, gy - 1)] {
                    if self.in_bounds(next) && !seen.contains(&next) && self.walkable_node(next) {
                        stack.push(next);
                    }
                }
            }
            if !region.is_empty() {
                components.push(region);
            }
        }
        components
    }

    /// The largest 4-connected region of walkable nodes.
    pub fn largest_walkable_component(&self) -> HashSet<(i32, i32)> {
        self.walkable_components()
            .into_iter()
            .max_by_key(HashSet::len)
            .unwrap_or_default()
    }

    /// The component containing world `(x, y)`, if its node is walkable.
    pub fn component_at(
        &self,
        x: f64,
        y: f64,
        components: &[HashSet<(i32, i32)>],
    ) -> Option<usize> {
        let key = self.node_key(x, y);
        components
            .iter()
            .position(|component| component.contains(&key))
    }

    fn walkable_node(&self, key: (i32, i32)) -> bool {
        self.in_bounds(key) && self.covered(key) && !self.blocked.contains(&key)
    }

    /// A node is open inside `component`: it and its ±20 cardinal neighbours
    /// are all walkable members of that region.
    fn open_in_component(&self, x: f64, y: f64, component: &HashSet<(i32, i32)>) -> bool {
        let reach = 2.0 * PATH_NODE_SIZE;
        let key = self.node_key(x, y);
        component.contains(&key)
            && [
                key,
                self.node_key(x + reach, y),
                self.node_key(x - reach, y),
                self.node_key(x, y + reach),
                self.node_key(x, y - reach),
            ]
            .iter()
            .all(|node| component.contains(node) && self.walkable_node(*node))
    }

    /// Snap a point to the nearest open node **inside `component`**.
    ///
    /// This is [`find_walkable_spot`](Self::find_walkable_spot) with a
    /// reachability constraint. Returns `(x, y, false)` when no member of the
    /// component is reachable within `radius`.
    pub fn find_walkable_spot_in_component(
        &self,
        x: f64,
        y: f64,
        radius: f64,
        component: &HashSet<(i32, i32)>,
    ) -> (f64, f64, bool) {
        if self.open_in_component(x, y, component) {
            return (x, y, true);
        }
        let mut r = 5.0;
        while r <= radius {
            let mut angle = 0.0f64;
            while angle < 360.0 {
                let rad = angle.to_radians();
                let tx = x + rad.cos() * r;
                let ty = y + rad.sin() * r;
                if self.open_in_component(tx, ty, component) {
                    return (tx, ty, true);
                }
                angle += 20.0;
            }
            r += 5.0;
        }
        (x, y, false)
    }

    /// A node is "open" when it and its two-cardinal-neighbour nodes (±20 world
    /// units) are all walkable (reference `_find_walkable_spot._open`).
    fn open(&self, x: f64, y: f64) -> bool {
        let reach = 2.0 * PATH_NODE_SIZE;
        self.is_walkable(x, y)
            && self.is_walkable(x + reach, y)
            && self.is_walkable(x - reach, y)
            && self.is_walkable(x, y + reach)
            && self.is_walkable(x, y - reach)
    }

    /// Snap a world point to the nearest point whose node and ±20 cardinal
    /// neighbours are walkable, spiralling out like the reference
    /// `_find_walkable_spot`. Returns the **sampled point** (not its node
    /// centre) and `false` when nothing is found inside `radius`.
    pub fn find_walkable_spot(&self, x: f64, y: f64, radius: f64) -> (f64, f64, bool) {
        if self.open(x, y) {
            return (x, y, true);
        }
        let mut r = 5.0;
        while r <= radius {
            let mut angle = 0.0f64;
            while angle < 360.0 {
                let rad = angle.to_radians();
                let tx = x + rad.cos() * r;
                let ty = y + rad.sin() * r;
                if self.open(tx, ty) {
                    return (tx, ty, true);
                }
                angle += 20.0;
            }
            r += 5.0;
        }
        (x, y, false)
    }

    /// Build a per-instance pathmap from the exact maze cells the client
    /// renders. `None` only for an empty cell list.
    pub fn build(archive: &Archive, cells: &[MazeCell]) -> Result<Option<Self>, ContentError> {
        if cells.is_empty() {
            return Ok(None);
        }
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for cell in cells {
            min_x = min_x.min(cell.world_origin_x);
            min_y = min_y.min(cell.world_origin_y);
            max_x = max_x.max(cell.world_origin_x + cell.tile_size);
            max_y = max_y.max(cell.world_origin_y + cell.tile_size);
        }
        let grid_w = (((max_x - min_x) / PATH_NODE_SIZE).ceil() as i32) + 1;
        let grid_h = (((max_y - min_y) / PATH_NODE_SIZE).ceil() as i32) + 1;

        let cobjs = cobj_index(archive);
        let mut floor: HashMap<(i32, i32), f64> = HashMap::new();
        let mut blocked: HashSet<(i32, i32)> = HashSet::new();
        let mut fallback_open: HashSet<(i32, i32)> = HashSet::new();

        for cell in cells {
            let coords = mark_cell_footprint(cell, min_x, min_y, grid_w, grid_h);
            let Some(text) = read_tile_text(archive, &cell.tile_type)? else {
                fallback_open.extend(coords);
                continue;
            };
            let file = parse(&text);
            let Some(top) = file.children.first() else {
                fallback_open.extend(coords);
                continue;
            };
            let mut placements = Vec::new();
            load_base_placements(archive, top.extends.as_deref(), 0, &mut placements)?;
            collect(top, &mut placements);

            let floor_before = floor.len();
            for placement in &placements {
                let Some(entry) = cobjs.get(&dotted_leaf(&placement.extends_path).to_lowercase())
                else {
                    continue;
                };
                let Ok(bytes) = archive.read(entry) else {
                    continue;
                };
                let Some(cobj) = parse_cobj(&bytes) else {
                    continue;
                };
                apply_subshape1(
                    &cobj,
                    placement,
                    cell,
                    min_x,
                    min_y,
                    grid_w,
                    grid_h,
                    &mut floor,
                    &mut blocked,
                );
                apply_subshape2(
                    &cobj,
                    placement,
                    cell,
                    min_x,
                    min_y,
                    grid_w,
                    grid_h,
                    &mut blocked,
                );
            }
            if floor.len() == floor_before {
                fallback_open.extend(coords);
            }
        }

        Ok(Some(Self {
            min_x,
            min_y,
            max_x,
            max_y,
            grid_w,
            grid_h,
            floor,
            blocked,
            fallback_open,
        }))
    }

    /// Number of floor-covered nodes (test/diagnostic aid).
    pub fn floor_node_count(&self) -> usize {
        self.floor.len()
    }
}

/// Mark a cell's square footprint and return its coords.
fn mark_cell_footprint(
    cell: &MazeCell,
    min_x: f64,
    min_y: f64,
    grid_w: i32,
    grid_h: i32,
) -> Vec<(i32, i32)> {
    let gx0 = ((cell.world_origin_x - min_x) / PATH_NODE_SIZE).floor() as i32;
    let gy0 = ((cell.world_origin_y - min_y) / PATH_NODE_SIZE).floor() as i32;
    let gx1 = ((cell.world_origin_x + cell.tile_size - min_x) / PATH_NODE_SIZE).ceil() as i32;
    let gy1 = ((cell.world_origin_y + cell.tile_size - min_y) / PATH_NODE_SIZE).ceil() as i32;
    let mut coords = Vec::new();
    for gx in gx0.max(0)..gx1.min(grid_w) {
        for gy in gy0.max(0)..gy1.min(grid_h) {
            coords.push((gx, gy));
        }
    }
    coords
}

#[allow(clippy::too_many_arguments)]
fn apply_subshape1(
    cobj: &CobjGrid,
    placement: &Placement,
    cell: &MazeCell,
    min_x: f64,
    min_y: f64,
    grid_w: i32,
    grid_h: i32,
    floor: &mut HashMap<(i32, i32), f64>,
    blocked: &mut HashSet<(i32, i32)>,
) {
    if cobj.cell_size1 <= 0 || cobj.width1 <= 0 || cobj.height1 <= 0 {
        return;
    }
    let cs = f64::from(cobj.cell_size1);
    let rad = placement.heading.to_radians();
    let (sin, cos) = rad.sin_cos();
    let nowalk = dotted_leaf(&placement.extends_path)
        .to_lowercase()
        .contains("nowalk");
    let reach = if cs > PATH_NODE_SIZE {
        ((cs / (2.0 * PATH_NODE_SIZE)).ceil() as i32).max(1)
    } else {
        0
    };

    for cy in 0..cobj.height1 {
        for cx in 0..cobj.width1 {
            let h = f64::from(cobj.heightmap[(cy * cobj.width1 + cx) as usize]);
            let lx = f64::from(cobj.origin_x1) + (f64::from(cx) + 0.5) * cs;
            let ly = f64::from(cobj.origin_y1) + (f64::from(cy) + 0.5) * cs;
            let rx = lx * cos - ly * sin;
            let ry = lx * sin + ly * cos;
            let wx = cell.world_origin_x + placement.x + rx;
            let wy = cell.world_origin_y + placement.y + ry;
            let gx = round_half_even((wx - min_x) / PATH_NODE_SIZE) as i32;
            let gy = round_half_even((wy - min_y) / PATH_NODE_SIZE) as i32;
            if gx < 0 || gx >= grid_w || gy < 0 || gy >= grid_h {
                continue;
            }
            let key = (gx, gy);

            if h > WALL_HEIGHT_THRESHOLD_RAW || nowalk {
                blocked.insert(key);
                continue;
            }

            // Heightmap samples are tenths of a world unit.
            let world_h = placement.z + h / 10.0;
            for ngx in (gx - reach)..=(gx + reach) {
                for ngy in (gy - reach)..=(gy + reach) {
                    if ngx < 0 || ngx >= grid_w || ngy < 0 || ngy >= grid_h {
                        continue;
                    }
                    let slot = floor.entry((ngx, ngy)).or_insert(f64::NEG_INFINITY);
                    if world_h > *slot {
                        *slot = world_h;
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_subshape2(
    cobj: &CobjGrid,
    placement: &Placement,
    cell: &MazeCell,
    min_x: f64,
    min_y: f64,
    grid_w: i32,
    grid_h: i32,
    blocked: &mut HashSet<(i32, i32)>,
) {
    if cobj.cell_size2 <= 0 || cobj.width2 <= 0 || cobj.height2 <= 0 || cobj.cells.is_empty() {
        return;
    }
    let cs = f64::from(cobj.cell_size2);
    let rad = placement.heading.to_radians();
    let (sin, cos) = rad.sin_cos();
    let base_z = f64::from(cobj.origin_z2) + placement.z;

    for cy in 0..cobj.height2 {
        for cx in 0..cobj.width2 {
            let bboxes = &cobj.cells[(cy * cobj.width2 + cx) as usize];
            if bboxes.is_empty() {
                continue;
            }
            let blocks = bboxes.iter().any(|&(low, high)| {
                let world_low = f64::from(low) + base_z;
                let world_high = f64::from(high) + base_z;
                world_low < WALKING_Z_MAX && world_high > WALKING_Z_MIN
            });
            if !blocks {
                continue;
            }
            let lx = f64::from(cobj.origin_x2) + (f64::from(cx) + 0.5) * cs;
            let ly = f64::from(cobj.origin_y2) + (f64::from(cy) + 0.5) * cs;
            let rx = lx * cos - ly * sin;
            let ry = lx * sin + ly * cos;
            let wx = cell.world_origin_x + placement.x + rx;
            let wy = cell.world_origin_y + placement.y + ry;
            let gx = round_half_even((wx - min_x) / PATH_NODE_SIZE) as i32;
            let gy = round_half_even((wy - min_y) / PATH_NODE_SIZE) as i32;
            if gx < 0 || gx >= grid_w || gy < 0 || gy >= grid_h {
                continue;
            }
            blocked.insert((gx, gy));
        }
    }
}

/// A tile's client-PathMap-compatible ground and walkability in the tile's
/// local frame. This is a single-cell [`DungeonPathMap`]; it exists so callers
/// (and tests) can reason about one tile without the whole maze.
#[derive(Debug, Clone)]
pub struct TileGroundMap {
    map: DungeonPathMap,
    tile_size: f64,
}

impl TileGroundMap {
    /// Is the node at local `(x, y)` covered by floor and unblocked?
    pub fn is_walkable(&self, x: f64, y: f64) -> bool {
        self.map.is_walkable(x, y)
    }

    /// The floor height at local `(x, y)`, or `None` when the node is not
    /// walkable.
    pub fn height_at(&self, x: f64, y: f64) -> Option<f64> {
        self.map.height_at(x, y)
    }

    /// The number of floor nodes (test/diagnostic aid).
    pub fn floor_node_count(&self) -> usize {
        self.map.floor_node_count()
    }

    /// The tile side length this map was built for.
    pub fn tile_size(&self) -> f64 {
        self.tile_size
    }

    /// Snap a local point to a walkable spot, spiralling out like the
    /// reference `_find_walkable_spot`. Returns the sampled point itself.
    pub fn snap_walkable(&self, x: f64, y: f64, radius: f64) -> Option<(f64, f64)> {
        let (sx, sy, found) = self.map.find_walkable_spot(x, y, radius);
        found.then_some((sx, sy))
    }
}

/// Build a tile's [`TileGroundMap`] from its own content (plus inherited base
/// placements). The tile is treated as a one-cell maze anchored at `(0, 0)`, so
/// node indices match the reference's world-space grid for that tile.
pub fn tile_ground_map(
    archive: &Archive,
    tile_type: &str,
    tile_size: f64,
) -> Result<Option<TileGroundMap>, ContentError> {
    let cell = MazeCell {
        grid_x: 0,
        grid_y: 0,
        world_grid_y: 0,
        connections: String::new(),
        tile_type: tile_type.to_string(),
        world_origin_x: 0.0,
        world_origin_y: 0.0,
        world_center_x: tile_size / 2.0,
        world_center_y: tile_size / 2.0,
        tile_size,
    };
    let Some(map) = DungeonPathMap::build(archive, &[cell])? else {
        return Ok(None);
    };
    Ok(Some(TileGroundMap { map, tile_size }))
}

/// Build the per-instance pathmap for a generated maze (`None` for no cells).
pub fn build_path_map(
    archive: &Archive,
    cells: &[MazeCell],
) -> Result<Option<DungeonPathMap>, ContentError> {
    DungeonPathMap::build(archive, cells)
}

// -- world reads --------------------------------------------------------------

/// Derive the corridor-tile prefix from the world base type
/// (`base.world_elmforest` -> `elmforest_tileset_`).
fn tile_prefix_for_base(world_base: &str) -> String {
    let base = world_base.trim();
    if base.len() >= 11 && base[..11].eq_ignore_ascii_case("base.world_") {
        return format!("{}_tileset_", base[11..].to_lowercase());
    }
    "elmforest_tileset_".to_string()
}

/// The base room classes' inherited grid pins (`world.base.room.*`).
fn load_base_room_grids(archive: &Archive) -> Result<BaseRoomGrids, ContentError> {
    let mut out = HashMap::new();
    for tier in ["Tier1", "Tier2", "Tier3"] {
        let path = format!("world/base/room/{tier}.gc");
        let Some(text) = read_text(archive, &path)? else {
            continue;
        };
        let file = parse(&text);
        let Some(root) = file.children.first() else {
            continue;
        };
        for section in &root.children {
            if section.name == "<anonymous>" {
                continue;
            }
            for room in &section.children {
                if room.name == "<anonymous>" {
                    continue;
                }
                let gx = room.prop("GridX").and_then(parse_i64);
                let gy = room.prop("GridY").and_then(parse_i64);
                if gx.is_none() && gy.is_none() {
                    continue;
                }
                let key = format!("{tier}.{}.{}", section.name, room.name).to_lowercase();
                out.insert(key, (gx, gy));
            }
        }
    }
    Ok(out)
}

/// The room nodes of a generated world, in file order.
fn parse_room_nodes(archive: &Archive, root: &Block) -> Result<Vec<WorldRoomNode>, ContentError> {
    let base_grids = load_base_room_grids(archive)?;
    let mut rows = Vec::new();
    let mut source_index = 0i64;
    for child in &root.children {
        if child.name != "<anonymous>" {
            continue;
        }
        let Some(tile_set) = child.prop("TileSet").map(unquote) else {
            continue;
        };
        if tile_set.is_empty() {
            continue;
        }
        let kind = child
            .extends
            .as_deref()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_lowercase();
        let mut grid_x = child.prop("GridX").and_then(parse_i64);
        let mut grid_y = child.prop("GridY").and_then(parse_i64);
        if let Some(extends) = child.extends.as_deref() {
            let low = extends.to_lowercase();
            if let Some(rest) = low.strip_prefix("world.base.room.") {
                if let Some((bx, by)) = base_grids.get(rest) {
                    if grid_x.is_none() {
                        grid_x = *bx;
                    }
                    if grid_y.is_none() {
                        grid_y = *by;
                    }
                }
            }
        }
        let chance = child.prop("Chance").and_then(parse_i64).unwrap_or(100);
        rows.push(WorldRoomNode {
            source_index,
            node_kind: kind,
            tile_set,
            grid_x,
            grid_y,
            chance,
            spawn_name: child.prop("SpawnName").map(unquote).unwrap_or_default(),
            link_to_zone: child
                .prop("LinkToZone")
                .map(unquote)
                .filter(|value| !value.is_empty()),
            link_to_spawn: child
                .prop("LinkToSpawn")
                .map(unquote)
                .filter(|value| !value.is_empty()),
            waypoint_name: child
                .prop("WaypointName")
                .map(unquote)
                .filter(|value| !value.is_empty()),
            encounter_world_entity_tables: [1u8, 2, 3]
                .into_iter()
                .filter_map(|selector| {
                    let name = if selector == 1 {
                        "EncounterWorldEntityTable".to_string()
                    } else {
                        format!("EncounterWorldEntityTable{selector}")
                    };
                    child
                        .prop(&name)
                        .map(unquote)
                        .filter(|value| !value.is_empty())
                        .map(|value| (selector, value))
                })
                .collect(),
        });
        source_index += 1;
    }
    Ok(rows)
}

/// Level parameters read from a generated `.world`.
struct LevelDef {
    maze_width: i64,
    maze_height: i64,
    maze_randomness: i64,
    maze_sparseness: i64,
    maze_dead_end_removal_chance: i64,
    tile_prefix: String,
}

fn parse_level(root: &Block) -> Option<LevelDef> {
    let maze_width = root.prop("MazeWidth").and_then(parse_i64).unwrap_or(0);
    let maze_height = root.prop("MazeHeight").and_then(parse_i64).unwrap_or(0);
    if maze_width <= 0 || maze_height <= 0 {
        return None;
    }
    let world_base = root.extends.clone().unwrap_or_default();
    Some(LevelDef {
        maze_width,
        maze_height,
        maze_randomness: root.prop("MazeRandomness").and_then(parse_i64).unwrap_or(0),
        maze_sparseness: root.prop("MazeSparseness").and_then(parse_i64).unwrap_or(0),
        maze_dead_end_removal_chance: root
            .prop("MazeDeadEndRemovalChance")
            .and_then(parse_i64)
            .unwrap_or(0),
        tile_prefix: tile_prefix_for_base(&world_base),
    })
}

// -- the maze generator -------------------------------------------------------

/// Deterministic room-node + corridor carver, byte-faithful to the reference.
struct MazeGenerator<'a> {
    archive: &'a Archive,
    width: i64,
    height: i64,
    randomness: i64,
    sparseness: i64,
    dead_end_removal_chance: i64,
    rng: MersenneTwister,
    cells: Vec<u8>,
    occupied: Vec<bool>,
    forced_exits: Vec<u8>,
    room_tile_types: Vec<Option<String>>,
    openings: Vec<u8>,
    room_nodes: Vec<RoomNodeSpec>,
    placed_room_nodes: Vec<PlacedRoomNode>,
    tile_size: i64,
}

impl<'a> MazeGenerator<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        archive: &'a Archive,
        width: i64,
        height: i64,
        seed: u32,
        randomness: i64,
        sparseness: i64,
        dead_end_removal_chance: i64,
        tile_size: i64,
    ) -> MazeGenerator<'a> {
        let total = (width * height) as usize;
        MazeGenerator {
            archive,
            width,
            height,
            randomness,
            sparseness,
            dead_end_removal_chance,
            rng: MersenneTwister::new(seed),
            cells: vec![0u8; total],
            occupied: vec![false; total],
            forced_exits: vec![0u8; total],
            room_tile_types: vec![None; total],
            openings: vec![0u8; total],
            room_nodes: Vec::new(),
            placed_room_nodes: Vec::new(),
            tile_size,
        }
    }

    fn add_room_node(
        &mut self,
        tile_set: String,
        grid_x: Option<i64>,
        grid_y: Option<i64>,
        chance: i64,
        source_index: i64,
    ) {
        if tile_set.is_empty() {
            return;
        }
        self.room_nodes.push(RoomNodeSpec {
            tile_set,
            grid_x,
            grid_y,
            chance,
            source_index,
        });
    }

    fn generate(&mut self, tile_set_prefix: &str) -> Vec<MazeCell> {
        self.place_room_nodes();
        self.generate_corridors();
        self.apply_forced_room_exits();
        self.sparsify();
        self.remove_dead_ends();
        self.sync_openings_from_native_bits();
        self.build_result(tile_set_prefix)
    }

    // RNG helpers.
    fn random_direction(&mut self) -> u8 {
        match self.rng.generate() & 3 {
            0 => DIR_NORTH,
            1 => DIR_SOUTH,
            2 => DIR_WEST,
            _ => DIR_EAST,
        }
    }

    fn roll100(&mut self) -> i64 {
        (self.rng.generate() % 100) as i64 + 1
    }

    // grid primitives.
    fn key(&self, x: i64, y: i64) -> usize {
        (y * self.width + x) as usize
    }

    fn in_bounds(&self, x: i64, y: i64) -> bool {
        x >= 0 && x < self.width && y >= 0 && y < self.height
    }

    fn cell_byte(&self, x: i64, y: i64) -> u8 {
        let k = self.key(x, y);
        let mut bits = self.cells[k];
        if self.occupied[k] {
            bits |= self.forced_exits[k] | CELL_ROOM;
        }
        bits
    }

    fn set_dir(&mut self, x: i64, y: i64, direction: u8) {
        if self.in_bounds(x, y) {
            let k = self.key(x, y);
            self.cells[k] |= direction;
        }
    }

    fn unset_dir(&mut self, x: i64, y: i64, direction: u8) {
        if self.in_bounds(x, y) {
            let k = self.key(x, y);
            self.cells[k] &= !direction;
        }
    }

    fn neighbor(&self, x: i64, y: i64, direction: u8) -> (i64, i64) {
        match direction {
            DIR_NORTH => (x, y - 1),
            DIR_SOUTH => (x, y + 1),
            DIR_EAST => (x + 1, y),
            DIR_WEST => (x - 1, y),
            _ => (x, y),
        }
    }

    fn is_dead_end(bits: u8) -> bool {
        matches!(bits, DIR_NORTH | DIR_SOUTH | DIR_EAST | DIR_WEST)
    }

    // room-node placement.
    fn place_room_nodes(&mut self) {
        self.placed_room_nodes.clear();
        let specs = self.room_nodes.clone();
        for spec in specs {
            if spec.chance <= 0 || self.roll100() > spec.chance {
                continue;
            }

            let mut variants = variants_for(self.archive, &spec.tile_set);
            while !variants.is_empty() {
                let variant_index = self.rng.next_int(0, variants.len() as i64) as usize;
                let tile_type = variants.remove(variant_index);

                let exits = parse_exit_bits(&spec.tile_set, &tile_type);
                let candidates = self.room_candidates(&spec, exits);
                if candidates.is_empty() {
                    continue;
                }

                let ci = self.rng.next_int(0, candidates.len() as i64) as usize;
                let (cx, cy) = candidates[ci];
                let k = self.key(cx, cy);
                self.occupied[k] = true;
                self.forced_exits[k] = exits;
                self.room_tile_types[k] = Some(tile_type.clone());
                self.placed_room_nodes.push(PlacedRoomNode {
                    source_index: spec.source_index,
                    tile_set: spec.tile_set.clone(),
                    tile_type,
                    grid_x: cx,
                    grid_y: cy,
                });
                break;
            }
        }
    }

    fn room_candidates(&self, spec: &RoomNodeSpec, exits: u8) -> Vec<(i64, i64)> {
        let mut min_x = 0i64;
        let mut max_x = self.width - 1;
        let mut min_y = 0i64;
        let mut max_y = self.height - 1;

        if let Some(x) = spec.grid_x {
            if x < 0 {
                return Vec::new();
            }
            let x = x.min(self.width - 1);
            min_x = x;
            max_x = x;
        }
        if let Some(y) = spec.grid_y {
            if y < 0 {
                return Vec::new();
            }
            let y = y.min(self.height - 1);
            min_y = y;
            max_y = y;
        }

        let mut candidates = Vec::new();
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                if self.occupied[self.key(x, y)] {
                    continue;
                }
                if !self.forced_exits_fit(x, y, exits) {
                    continue;
                }
                if !self.map_fully_connected_with_candidate(x, y, exits) {
                    continue;
                }
                candidates.push((x, y));
            }
        }
        candidates
    }

    fn forced_exits_fit(&self, x: i64, y: i64, exits: u8) -> bool {
        if exits == 0 {
            return true;
        }
        for direction in ORDERED_DIRS {
            if exits & direction == 0 {
                continue;
            }
            let (nx, ny) = self.neighbor(x, y, direction);
            if !self.in_bounds(nx, ny) {
                return false;
            }
        }
        true
    }

    fn map_fully_connected_with_candidate(
        &self,
        candidate_x: i64,
        candidate_y: i64,
        candidate_exits: u8,
    ) -> bool {
        let total = (self.width * self.height) as usize;
        let mut visited = vec![false; total];
        visited[self.key(candidate_x, candidate_y)] = true;
        let mut queue: VecDeque<(i64, i64)> = VecDeque::new();
        queue.push_back((candidate_x, candidate_y));

        while let Some((cx, cy)) = queue.pop_front() {
            let bits = self.candidate_cell_byte(cx, cy, candidate_x, candidate_y, candidate_exits);
            let mut dirs = bits & 0x0F;
            let current_solid = bits != 0;
            if bits == 0 {
                dirs = 0x0F;
            }

            for direction in EMPTY_NEIGHBOR_DIRS {
                if dirs & direction == 0 {
                    continue;
                }
                let (nx, ny) = self.neighbor(cx, cy, direction);
                if !self.in_bounds(nx, ny) {
                    if current_solid {
                        return false;
                    }
                    continue;
                }
                let neighbor_bits =
                    self.candidate_cell_byte(nx, ny, candidate_x, candidate_y, candidate_exits);
                if neighbor_bits != 0 && (neighbor_bits & opposite_dir(direction)) == 0 {
                    if current_solid {
                        return false;
                    }
                    continue;
                }
                let key = self.key(nx, ny);
                if !visited[key] {
                    visited[key] = true;
                    queue.push_back((nx, ny));
                }
            }
        }
        visited.iter().all(|&seen| seen)
    }

    fn candidate_cell_byte(
        &self,
        x: i64,
        y: i64,
        candidate_x: i64,
        candidate_y: i64,
        candidate_exits: u8,
    ) -> u8 {
        if x == candidate_x && y == candidate_y {
            return CELL_ROOM | (candidate_exits & 0x0F);
        }
        let k = self.key(x, y);
        if self.occupied[k] {
            return CELL_ROOM | (self.forced_exits[k] & 0x0F) | (self.cells[k] & 0x0F);
        }
        self.cells[k]
    }

    // corridor generation.
    fn generate_corridors(&mut self) {
        let mut empty_remaining = self.width * self.height - 1;
        for y in 0..self.height {
            for x in 0..self.width {
                if self.occupied[self.key(x, y)] {
                    empty_remaining -= 1;
                }
            }
        }
        if empty_remaining < 0 {
            empty_remaining = 0;
        }

        let mut current = self.get_random_empty_cell();
        let mut has_current = current.is_some();
        let mut use_corridor_fallback = false;
        let mut last_dir = 0u8;
        let mut straight_streak = 0i64;

        while empty_remaining > 0 {
            if !has_current {
                current = if use_corridor_fallback {
                    self.get_random_corridor_cell()
                } else {
                    self.get_random_empty_cell()
                };
                use_corridor_fallback = false;
                let Some(cell) = current else {
                    break;
                };
                current = Some(cell);
                has_current = true;
            }

            let (x, y) = current.unwrap();
            let current_bits = self.cell_byte(x, y);
            let mut blocked = self.blocked_directions(x, y, current_bits);
            let mut chosen_dir = 0u8;

            if current_bits & CELL_ROOM == 0 {
                if self.roll100() > self.randomness
                    && self.can_continue_dir(x, y, last_dir, straight_streak)
                {
                    chosen_dir = last_dir;
                    straight_streak += 1;
                } else {
                    straight_streak = 0;
                }
            }

            let mut accepted = false;
            loop {
                if chosen_dir != 0 && (blocked & chosen_dir) == 0 {
                    let (nx, ny) = self.neighbor(x, y, chosen_dir);
                    self.set_dir(x, y, chosen_dir);
                    self.set_dir(nx, ny, opposite_dir(chosen_dir));
                    current = Some((nx, ny));
                    has_current = true;
                    last_dir = chosen_dir;
                    empty_remaining -= 1;
                    accepted = true;
                    break;
                }

                chosen_dir = self.random_direction();
                let (nnx, nny) = self.neighbor(x, y, chosen_dir);
                if !self.in_bounds(nnx, nny) {
                    blocked |= chosen_dir;
                } else if self.cell_byte(nnx, nny) == 0 {
                    continue;
                } else {
                    blocked |= chosen_dir;
                }

                if blocked & 0x0F == 0x0F {
                    break;
                }
            }

            if !accepted {
                has_current = false;
                use_corridor_fallback = true;
            }
        }
    }

    fn blocked_directions(&self, x: i64, y: i64, bits: u8) -> u8 {
        let mut blocked = bits & 0x0F;
        if bits & CELL_ROOM != 0 {
            blocked = !bits & 0x0F;
        }
        if y <= 0 {
            blocked |= DIR_NORTH;
        }
        if y + 1 >= self.height {
            blocked |= DIR_SOUTH;
        }
        if x <= 0 {
            blocked |= DIR_WEST;
        }
        if x + 1 >= self.width {
            blocked |= DIR_EAST;
        }
        blocked
    }

    fn can_continue_dir(&self, x: i64, y: i64, direction: u8, streak: i64) -> bool {
        if direction == 0 || streak >= self.straight_limit(direction) {
            return false;
        }
        let (nx, ny) = self.neighbor(x, y, direction);
        self.in_bounds(nx, ny) && self.cell_byte(nx, ny) == 0
    }

    fn straight_limit(&self, direction: u8) -> i64 {
        if direction == DIR_EAST || direction == DIR_WEST {
            if self.width <= 1 {
                0
            } else {
                self.width / 2
            }
        } else if direction == DIR_NORTH || direction == DIR_SOUTH {
            if self.height <= 1 {
                0
            } else {
                self.height / 2
            }
        } else {
            0
        }
    }

    fn get_random_empty_cell(&mut self) -> Option<(i64, i64)> {
        let mut candidates = Vec::new();
        for y in 0..self.height {
            for x in 0..self.width {
                if self.cell_byte(x, y) == 0 {
                    candidates.push((x, y));
                }
            }
        }
        if candidates.is_empty() {
            return None;
        }
        let i = self.rng.next_int(0, candidates.len() as i64) as usize;
        Some(candidates[i])
    }

    fn get_random_corridor_cell(&mut self) -> Option<(i64, i64)> {
        let mut normal = Vec::new();
        let mut forced = Vec::new();

        for y in 0..self.height {
            for x in 0..self.width {
                let bits = self.cell_byte(x, y);
                if bits & 0x0F == 0 {
                    continue;
                }
                let empty_mask = self.empty_neighbor_mask(x, y);
                if empty_mask == 0 {
                    continue;
                }
                if self.occupied[self.key(x, y)] {
                    if empty_mask & bits & 0x0F != 0 {
                        forced.push((x, y));
                    }
                    continue;
                }
                normal.push((x, y));
            }
        }

        if !normal.is_empty() {
            let i = self.rng.next_int(0, normal.len() as i64) as usize;
            return Some(normal[i]);
        }
        if !forced.is_empty() {
            let i = self.rng.next_int(0, forced.len() as i64) as usize;
            return Some(forced[i]);
        }
        None
    }

    fn get_random_dead_end_cell(&mut self) -> Option<(i64, i64)> {
        let mut candidates = Vec::new();
        for y in 0..self.height {
            for x in 0..self.width {
                let k = self.key(x, y);
                if !self.occupied[k] && Self::is_dead_end(self.cells[k]) {
                    candidates.push((x, y));
                }
            }
        }
        if candidates.is_empty() {
            return None;
        }
        let i = self.rng.next_int(0, candidates.len() as i64) as usize;
        Some(candidates[i])
    }

    fn empty_neighbor_mask(&self, x: i64, y: i64) -> u8 {
        let mut mask = 0u8;
        for direction in EMPTY_NEIGHBOR_DIRS {
            let (nx, ny) = self.neighbor(x, y, direction);
            if self.in_bounds(nx, ny) && self.cell_byte(nx, ny) == 0 {
                mask |= direction;
            }
        }
        mask
    }

    // post-passes.
    fn apply_forced_room_exits(&mut self) {
        for y in 0..self.height {
            for x in 0..self.width {
                let exits = self.forced_exits[self.key(x, y)];
                if exits == 0 {
                    continue;
                }
                for direction in ORDERED_DIRS {
                    let (nx, ny) = self.neighbor(x, y, direction);
                    if !self.in_bounds(nx, ny) {
                        continue;
                    }
                    if exits & direction != 0 {
                        self.set_dir(x, y, direction);
                        self.set_dir(nx, ny, opposite_dir(direction));
                    } else if self.cells[self.key(x, y)] & direction != 0 {
                        self.unset_dir(x, y, direction);
                        self.unset_dir(nx, ny, opposite_dir(direction));
                    }
                }
            }
        }
    }

    fn sparsify(&mut self) {
        if self.sparseness <= 0 {
            return;
        }
        for _ in 0..self.sparseness {
            let Some(cell) = self.get_random_dead_end_cell() else {
                return;
            };
            let (x, y) = cell;
            let direction = self.cells[self.key(x, y)];
            let (nx, ny) = self.neighbor(x, y, direction);
            if !self.in_bounds(nx, ny) || self.occupied[self.key(nx, ny)] {
                continue;
            }
            let k = self.key(x, y);
            self.cells[k] = 0;
            self.unset_dir(nx, ny, opposite_dir(direction));
        }
    }

    fn remove_dead_ends(&mut self) {
        for y in 0..self.height {
            for x in 0..self.width {
                let k = self.key(x, y);
                if self.occupied[k] || !Self::is_dead_end(self.cells[k]) {
                    continue;
                }
                if self.roll100() > self.dead_end_removal_chance {
                    continue;
                }

                let (mut cx, mut cy) = (x, y);
                loop {
                    let current_bits = self.cells[self.key(cx, cy)];
                    let mut blocked = 0u8;
                    let mut chosen_dir = 0u8;
                    let mut nxt = (0i64, 0i64);

                    while chosen_dir == 0 {
                        let direction = self.random_direction();
                        let (cand_x, cand_y) = self.neighbor(cx, cy, direction);
                        let is_blocked =
                            if !self.in_bounds(cand_x, cand_y) || current_bits == direction {
                                true
                            } else {
                                self.occupied[self.key(cand_x, cand_y)]
                            };
                        if is_blocked {
                            blocked |= direction;
                        } else {
                            chosen_dir = direction;
                            nxt = (cand_x, cand_y);
                        }

                        if blocked & 0x0F == 0x0F {
                            break;
                        }
                    }

                    if chosen_dir == 0 {
                        break;
                    }

                    self.set_dir(cx, cy, chosen_dir);
                    self.set_dir(nxt.0, nxt.1, opposite_dir(chosen_dir));
                    cx = nxt.0;
                    cy = nxt.1;
                    if self.cells[self.key(cx, cy)] != opposite_dir(chosen_dir) {
                        break;
                    }
                }
            }
        }
    }

    fn sync_openings_from_native_bits(&mut self) {
        for y in 0..self.height {
            for x in 0..self.width {
                let k = self.key(x, y);
                let bits = self.cell_byte(x, y);
                let mut opens = 0u8;
                if bits & DIR_NORTH != 0 {
                    opens |= OPEN_NORTH;
                }
                if bits & DIR_EAST != 0 {
                    opens |= OPEN_EAST;
                }
                if bits & DIR_SOUTH != 0 {
                    opens |= OPEN_SOUTH;
                }
                if bits & DIR_WEST != 0 {
                    opens |= OPEN_WEST;
                }
                self.openings[k] = opens;
            }
        }
    }

    fn build_result(&self, tile_set_prefix: &str) -> Vec<MazeCell> {
        let half_grid_x = self.width / 2;
        let half_grid_y = self.height / 2;
        let tile_size = self.tile_size;

        let mut cells = Vec::with_capacity((self.width * self.height) as usize);
        for y in 0..self.height {
            for x in 0..self.width {
                let opens = self.openings[self.key(x, y)];
                let mut conns = String::new();
                if opens & OPEN_NORTH != 0 {
                    conns.push_str("1n");
                }
                if opens & OPEN_WEST != 0 {
                    conns.push_str("1e");
                }
                if opens & OPEN_SOUTH != 0 {
                    conns.push_str("1s");
                }
                if opens & OPEN_EAST != 0 {
                    conns.push_str("1w");
                }
                if conns.is_empty() {
                    conns = "0n".to_string();
                }

                let world_grid_y = y;
                let cell_ox = (x - half_grid_x) as f64 * tile_size as f64;
                let cell_oy = (world_grid_y - half_grid_y) as f64 * tile_size as f64;

                let tile_type = match &self.room_tile_types[self.key(x, y)] {
                    Some(t) => t.clone(),
                    None => format!("{tile_set_prefix}{conns}_a"),
                };

                cells.push(MazeCell {
                    grid_x: x,
                    grid_y: y,
                    world_grid_y,
                    connections: conns,
                    tile_type,
                    world_origin_x: cell_ox,
                    world_origin_y: cell_oy,
                    world_center_x: cell_ox + tile_size as f64 / 2.0,
                    world_center_y: cell_oy + tile_size as f64 / 2.0,
                    tile_size: tile_size as f64,
                });
            }
        }
        cells
    }
}

fn opposite_dir(direction: u8) -> u8 {
    match direction {
        DIR_NORTH => DIR_SOUTH,
        DIR_SOUTH => DIR_NORTH,
        DIR_EAST => DIR_WEST,
        DIR_WEST => DIR_EAST,
        _ => 0,
    }
}

/// The exit bitfield a room variant's tile-name suffix denotes.
///
/// Native `TileLibrary::char2Direction` mirrors tile-name e/w against maze bits
/// (`1e` -> WEST, `1w` -> EAST).
fn parse_exit_bits(tile_set: &str, tile_type: &str) -> u8 {
    let mut suffix = tile_type.to_string();
    if !tile_set.is_empty() && suffix.to_lowercase().starts_with(&tile_set.to_lowercase()) {
        suffix = suffix[tile_set.len()..].to_string();
    }
    let mut bits = 0u8;
    if suffix.contains("1n") {
        bits |= DIR_NORTH;
    }
    if suffix.contains("1s") {
        bits |= DIR_SOUTH;
    }
    if suffix.contains("1e") {
        bits |= DIR_WEST;
    }
    if suffix.contains("1w") {
        bits |= DIR_EAST;
    }
    bits
}

// -- entry resolution ---------------------------------------------------------

/// A built maze plus the room-node defs needed for entry selection.
struct Layout {
    cells: Vec<MazeCell>,
    placed: Vec<PlacedRoomNode>,
    room_nodes: Vec<WorldRoomNode>,
    by_grid: HashMap<(i64, i64), usize>,
    tile_size: i64,
}

impl Layout {
    /// The `MazeCell` index the player should arrive in.
    fn entry_index(&self, spawn_point: &str) -> Option<usize> {
        let placed_by_source: HashMap<i64, usize> = self
            .placed
            .iter()
            .enumerate()
            .map(|(i, p)| (p.source_index, i))
            .collect();

        let cell_for = |rn: &WorldRoomNode| -> Option<usize> {
            let pi = *placed_by_source.get(&rn.source_index)?;
            let p = &self.placed[pi];
            self.by_grid.get(&(p.grid_x, p.grid_y)).copied()
        };

        if !spawn_point.is_empty() {
            let want = spawn_point.to_lowercase();
            for rn in &self.room_nodes {
                if !rn.spawn_name.is_empty() && rn.spawn_name.to_lowercase() == want {
                    if let Some(cell) = cell_for(rn) {
                        return Some(cell);
                    }
                }
            }
        }

        for kind in ENTRANCE_KINDS {
            for rn in &self.room_nodes {
                if rn.node_kind == kind {
                    if let Some(cell) = cell_for(rn) {
                        return Some(cell);
                    }
                }
            }
        }

        for rn in &self.room_nodes {
            if !rn.spawn_name.is_empty() {
                if let Some(cell) = cell_for(rn) {
                    return Some(cell);
                }
            }
        }

        for rn in &self.room_nodes {
            if rn.tile_set.to_lowercase().contains("start") {
                if let Some(cell) = cell_for(rn) {
                    return Some(cell);
                }
            }
        }

        for rn in &self.room_nodes {
            if let Some(cell) = cell_for(rn) {
                return Some(cell);
            }
        }
        None
    }
}

/// Generate a zone's maze from its own `.world` content. `None` for a
/// non-maze (no `MazeWidth`) or unreadable world.
pub fn generate_layout(archive: &Archive, zone: &str) -> Result<Option<MazeLayout>, ContentError> {
    generate_layout_with_seed(archive, zone, layout_seed(zone))
}

/// [`generate_layout`] with an explicit per-entry seed (the value the server
/// puts in the zone-connect Assign so the client builds the same maze).
pub fn generate_layout_with_seed(
    archive: &Archive,
    zone: &str,
    seed: u32,
) -> Result<Option<MazeLayout>, ContentError> {
    let Some(layout) = build_layout_with_seed(archive, zone, seed)? else {
        return Ok(None);
    };
    Ok(Some(MazeLayout {
        cells: layout.cells,
        placed_room_nodes: layout.placed,
        room_nodes: layout
            .room_nodes
            .into_iter()
            .map(|node| MazeRoomNode {
                source_index: node.source_index,
                node_kind: node.node_kind,
                tile_set: node.tile_set,
                grid_x: node.grid_x,
                grid_y: node.grid_y,
                chance: node.chance,
                spawn_name: node.spawn_name,
                link_to_zone: node.link_to_zone,
                link_to_spawn: node.link_to_spawn,
                waypoint_name: node.waypoint_name,
                encounter_world_entity_tables: node.encounter_world_entity_tables,
            })
            .collect(),
        tile_size: layout.tile_size,
    }))
}

fn build_layout_with_seed(
    archive: &Archive,
    zone: &str,
    seed: u32,
) -> Result<Option<Layout>, ContentError> {
    let Some(world_text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(None);
    };
    let file = parse(&world_text);
    let Some(root) = file.children.first() else {
        return Ok(None);
    };
    let Some(level) = parse_level(root) else {
        return Ok(None);
    };
    let tile_size = world_tile_size(archive, root)?.unwrap_or(DEFAULT_TILE_SIZE);
    let room_nodes = parse_room_nodes(archive, root)?;

    let mut generator = MazeGenerator::new(
        archive,
        level.maze_width,
        level.maze_height,
        seed,
        level.maze_randomness,
        level.maze_sparseness,
        level.maze_dead_end_removal_chance,
        tile_size,
    );
    for rn in &room_nodes {
        generator.add_room_node(
            rn.tile_set.clone(),
            rn.grid_x,
            rn.grid_y,
            rn.chance,
            rn.source_index,
        );
    }
    let cells = generator.generate(&level.tile_prefix);
    let by_grid = cells
        .iter()
        .enumerate()
        .map(|(i, c)| ((c.grid_x, c.grid_y), i))
        .collect();
    Ok(Some(Layout {
        cells,
        placed: generator.placed_room_nodes,
        room_nodes,
        by_grid,
        tile_size,
    }))
}

/// Resolve the player's entry cell and its authored anchor for a maze zone.
pub fn resolve_entry(
    archive: &Archive,
    zone: &str,
    spawn_point: &str,
) -> Result<Option<MazeEntry>, ContentError> {
    resolve_entry_with_seed(archive, zone, spawn_point, layout_seed(zone))
}

/// [`resolve_entry`] with an explicit per-entry seed.
pub fn resolve_entry_with_seed(
    archive: &Archive,
    zone: &str,
    spawn_point: &str,
    seed: u32,
) -> Result<Option<MazeEntry>, ContentError> {
    let Some(layout) = build_layout_with_seed(archive, zone, seed)? else {
        return Ok(None);
    };
    let Some(index) = layout.entry_index(spawn_point) else {
        return Ok(None);
    };
    let cell = &layout.cells[index];
    let anchor = authored_spawn(archive, &cell.tile_type)?;
    Ok(Some(MazeEntry {
        grid_x: cell.grid_x,
        grid_y: cell.grid_y,
        world_origin_x: cell.world_origin_x,
        world_origin_y: cell.world_origin_y,
        world_center_x: cell.world_center_x,
        world_center_y: cell.world_center_y,
        tile_type: cell.tile_type.clone(),
        anchor,
    }))
}

/// The player's arrival pose for a generated dungeon.
///
/// Matches `dr_pyserver.dungeon_spawner.entry_position`: the entry cell's
/// authored `SpawnPoint` waypoint (`world_origin + authored_local`, with the
/// authored Z and heading). `None` when the zone is not a generated maze, no
/// entry cell resolves, or the entry tile carries no authored anchor at all
/// (the reference's anchorless pathmap floor-snap fallback is not ported).
pub fn entry_position(
    archive: &Archive,
    zone: &str,
    spawn_point: &str,
) -> Result<Option<EntryPosition>, ContentError> {
    entry_position_with_seed(archive, zone, spawn_point, layout_seed(zone))
}

/// [`entry_position`] with an explicit per-entry seed.
pub fn entry_position_with_seed(
    archive: &Archive,
    zone: &str,
    spawn_point: &str,
    seed: u32,
) -> Result<Option<EntryPosition>, ContentError> {
    let Some(entry) = resolve_entry_with_seed(archive, zone, spawn_point, seed)? else {
        return Ok(None);
    };
    let Some(anchor) = entry.anchor else {
        return Ok(None);
    };
    Ok(Some(EntryPosition {
        x: entry.world_origin_x + anchor.x,
        y: entry.world_origin_y + anchor.y,
        z: anchor.z,
        heading: anchor.heading,
    }))
}

/// The return-leg `misc.ZonePortal*` model's world pose for a generated
/// dungeon's entry cell: the maze entry cell's ORIGIN plus the entry tile's own
/// `misc.ZonePortal*` local placement (the tile's authored model anchor, its
/// heading included) -- NOT the player's `misc.Waypoint SpawnPoint` that
/// [`entry_position`] reads.
///
/// This is where the client's `ZonePortalObject` for the start room is built:
/// `MazeGenerator::BuildWorld` places the tile at the cell origin, so the
/// portal inside it rides at `world_origin + authored_local`.
pub fn entry_portal_position(
    archive: &Archive,
    zone: &str,
    spawn_point: &str,
) -> Result<Option<EntryPosition>, ContentError> {
    entry_portal_position_with_seed(archive, zone, spawn_point, layout_seed(zone))
}

/// [`entry_portal_position`] with an explicit per-entry seed.
pub fn entry_portal_position_with_seed(
    archive: &Archive,
    zone: &str,
    spawn_point: &str,
    seed: u32,
) -> Result<Option<EntryPosition>, ContentError> {
    let Some(layout) = build_layout_with_seed(archive, zone, seed)? else {
        return Ok(None);
    };
    let Some(index) = layout.entry_index(spawn_point) else {
        return Ok(None);
    };
    let cell = &layout.cells[index];
    let Some(anchor) = tile_authored_anchors(archive, &cell.tile_type)?.1 else {
        return Ok(None);
    };
    Ok(Some(EntryPosition {
        x: cell.world_origin_x + anchor.x,
        y: cell.world_origin_y + anchor.y,
        z: anchor.z,
        heading: anchor.heading,
    }))
}

/// The world-frame position of a generated room's authored `WaypointName`.
///
/// Offshoot rooms name their exit teleporter there (`level05_off1a_teleporter`)
/// and quest `GotoObjective`s target that name. The client places the waypoint
/// at the room's tile; the centre is the content-derived approximation when
/// the tile's teleporter is a placeholder rather than a `misc.ZonePortal`.
pub fn room_waypoint_position_with_seed(
    archive: &Archive,
    zone: &str,
    name: &str,
    seed: u32,
) -> Result<Option<[f64; 2]>, ContentError> {
    let Some(layout) = build_layout_with_seed(archive, zone, seed)? else {
        return Ok(None);
    };
    let Some(node) = layout.room_nodes.iter().find(|node| {
        node.waypoint_name
            .as_deref()
            .is_some_and(|waypoint| waypoint.eq_ignore_ascii_case(name))
    }) else {
        return Ok(None);
    };
    let Some(placed) = layout
        .placed
        .iter()
        .find(|placed| placed.source_index == node.source_index)
    else {
        return Ok(None);
    };
    let Some(cell) = layout
        .cells
        .iter()
        .find(|cell| cell.grid_x == placed.grid_x && cell.grid_y == placed.grid_y)
    else {
        return Ok(None);
    };
    Ok(Some([cell.world_center_x, cell.world_center_y]))
}

/// The portal anchor inside a placed room's own tile (`world_origin + the
/// tile's `misc.ZonePortal*` local offset`), for generated room exits.
pub fn room_portal_position_with_seed(
    archive: &Archive,
    zone: &str,
    grid_x: i64,
    grid_y: i64,
    seed: u32,
) -> Result<Option<EntryPosition>, ContentError> {
    let Some(layout) = build_layout_with_seed(archive, zone, seed)? else {
        return Ok(None);
    };
    let Some(cell) = layout
        .cells
        .iter()
        .find(|cell| cell.grid_x == grid_x && cell.grid_y == grid_y)
    else {
        return Ok(None);
    };
    let Some(anchor) = tile_authored_anchors(archive, &cell.tile_type)?.1 else {
        return Ok(None);
    };
    Ok(Some(EntryPosition {
        x: cell.world_origin_x + anchor.x,
        y: cell.world_origin_y + anchor.y,
        z: anchor.z,
        heading: anchor.heading,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

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
    fn crc32_matches_zlib() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"dungeon00_level01"), 168_607_021);
        assert_eq!(layout_seed("dungeon00_level01"), 168_607_021);
        assert_eq!(layout_seed("DUNGEON00_LEVEL01"), 168_607_021);
        assert_eq!(layout_seed("dungeon00_level01"), 0x0A0C_BD2D);
    }

    #[test]
    fn mt19937_matches_canonical_vectors() {
        // mt19937ar seeded with 5489: the canonical first five outputs.
        let mut rng = MersenneTwister::new(5489);
        let got = [
            rng.generate(),
            rng.generate(),
            rng.generate(),
            rng.generate(),
            rng.generate(),
        ];
        assert_eq!(
            got,
            [
                3_499_211_612,
                581_869_302,
                3_890_346_734,
                3_586_334_585,
                545_404_204,
            ]
        );
    }

    #[test]
    fn mt19937_next_int_is_inclusive_on_both_ends() {
        let mut rng = MersenneTwister::new(1234);
        for _ in 0..1000 {
            let v = rng.next_int(0, 4);
            assert!((0..=3).contains(&v));
        }
        assert_eq!(rng.next_int(7, 7), 7);
    }

    #[test]
    fn dungeon00_level01_layout_matches_the_reference() {
        let Some(archive) = archive() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let layout = generate_layout(&archive, "dungeon00_level01")
            .expect("read dungeon00_level01.world")
            .expect("a generated maze level");
        assert_eq!(layout.tile_size, 400);
        let placed: Vec<(i64, String, i64, i64)> = layout
            .placed_room_nodes
            .iter()
            .map(|p| (p.source_index, p.tile_type.clone(), p.grid_x, p.grid_y))
            .collect();
        assert_eq!(
            placed,
            vec![
                (0, "elmforest_hub_1n".to_string(), 0, 4),
                (1, "elmforest_down_1e".to_string(), 1, 0),
                (2, "elmforest_questfindring_1w".to_string(), 2, 3),
                (3, "tutorial_loot_1w".to_string(), 0, 2),
            ]
        );
    }

    #[test]
    fn dungeon00_level01_start1_entry_matches_the_reference() {
        let Some(archive) = archive() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let entry = resolve_entry(&archive, "dungeon00_level01", "start1")
            .expect("read dungeon00_level01.world")
            .expect("start1 resolves an entry cell");
        assert_eq!((entry.grid_x, entry.grid_y), (0, 4));
        assert_eq!(entry.world_origin_x, -800.0);
        assert_eq!(entry.world_origin_y, 800.0);
        assert_eq!(entry.tile_type, "elmforest_hub_1n");
        assert_eq!(
            entry.anchor,
            Some(AuthoredAnchor {
                x: 150.0,
                y: 70.0,
                z: 10.0,
                heading: 180.0,
            })
        );

        let pose = entry_position(&archive, "dungeon00_level01", "start1")
            .expect("read dungeon00_level01.world")
            .expect("start1 resolves a pose");
        assert_eq!(
            pose,
            EntryPosition {
                x: -650.0,
                y: 870.0,
                z: 10.0,
                heading: 180.0,
            }
        );
    }

    #[test]
    fn a_non_maze_zone_resolves_no_entry() {
        let Some(archive) = archive() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        // `tutorial.world` states no `MazeWidth`.
        assert!(entry_position(&archive, "tutorial", "Start1")
            .expect("read tutorial.world")
            .is_none());
    }

    /// The client's `MazeGenerator::BuildWorld` places a cell at
    /// `((col - W/2) * TileSize, (row - H/2) * TileSize)` and a tile's
    /// encounter marker at `cell_origin + marker.Position`. This pins that
    /// arithmetic for `dungeon00_level01` and proves every marker's authored Z
    /// is exactly the collision floor height (`placement.z + heightmap`) and
    /// lies on a walkable node of the tile's `PathMap` geometry.
    #[test]
    fn dungeon00_level01_encounter_markers_are_on_the_tile_floor() {
        let Some(archive) = archive() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let layout = generate_layout(&archive, "dungeon00_level01")
            .expect("read dungeon00_level01.world")
            .expect("a generated maze level");

        // Client-derived marker world poses for cell (0,3):
        // origin (-800, 400) + `elmforest_tileset_1s1w_a` markers
        // (320,180,10) and (160,270,10).
        let cell = layout
            .cells
            .iter()
            .find(|cell| cell.grid_x == 0 && cell.grid_y == 3)
            .expect("cell (0,3)");
        assert_eq!(cell.tile_type, "elmforest_tileset_1s1w_a");
        let markers = tile_encounter_markers(&archive, &cell.tile_type)
            .expect("read elmforest_tileset_1s1w_a");
        let world: Vec<(f64, f64, f64)> = markers
            .iter()
            .map(|m| (cell.world_origin_x + m.x, cell.world_origin_y + m.y, m.z))
            .collect();
        assert_eq!(world, vec![(-480.0, 580.0, 10.0), (-640.0, 670.0, 10.0)]);

        // Every encounter marker in the generated level sits on the tile's
        // walkable floor, at exactly the floor height the client will use.
        let mut checked = 0;
        for cell in &layout.cells {
            let markers = tile_encounter_markers(&archive, &cell.tile_type)
                .expect("read tile encounter markers");
            if markers.is_empty() {
                continue;
            }
            let map = tile_ground_map(&archive, &cell.tile_type, cell.tile_size)
                .expect("read tile collision geometry")
                .unwrap_or_else(|| panic!("{} has no collision geometry", cell.tile_type));
            assert!(map.floor_node_count() > 0);
            for marker in markers {
                assert!(
                    map.is_walkable(marker.x, marker.y),
                    "{} marker ({}, {}) is not walkable",
                    cell.tile_type,
                    marker.x,
                    marker.y
                );
                assert_eq!(
                    map.height_at(marker.x, marker.y),
                    Some(marker.z),
                    "{} marker ({}, {}) floor height",
                    cell.tile_type,
                    marker.x,
                    marker.y
                );
                checked += 1;
            }
        }
        assert!(checked >= 18, "checked {checked} encounter markers");
    }
}
