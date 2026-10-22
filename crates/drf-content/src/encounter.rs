// SPDX-License-Identifier: GPL-3.0-or-later

//! Dungeon mob spawn placement, ported from the reference
//! `dr_pyserver.managers.dungeon_spawner.generate_spawns` (and the encounter /
//! tile-marker / monster-health pieces it leans on).
//!
//! The spawner turns a generated maze (see [`crate::maze`]) into a list of mob
//! spawns. Every input is a `game.pkg` statement:
//!
//! * the level's main encounter table and each forced room node's own
//!   `EncounterType`, read from `<zone>.world` and the `*.enc` tables the world
//!   names ([`encounter_groups`]);
//! * each placed cell's `base.Encounter<N>` markers, read from its `.tile`
//!   ([`crate::maze::tile_encounter_markers`]);
//! * the mobile's level and wire HP, derived from its own `creatures.*`
//!   archetype chain plus the `MonsterHealth` curve in `Tables.gc`.
//!
//! The cell order is the client's (`sorted(cells, key=(-grid_y, grid_x))`), the
//! entry cell and the placed room cells are reserved, and each marker spawns
//! an *expanded pack* (the authored units plus weighted round-robin fill to a
//! per-spot budget) exactly as the reference's `expand_encounter_group` does.
//! Group offsets and headings are jittered with the reference's seeded
//! `System.Random` (`DotNetRandom`), so an instance is reproducible across
//! joiners; a mob's exact position is server-told anyway, so it need not match
//! the client.
//!
//! DERIVED SCOPE. The reference floor-snaps each pack through one
//! per-instance geometry pathmap built from **all** the maze's cells; this port
//! does the same via [`maze::DungeonPathMap`] (the client's 10-unit node grid,
//! banker's rounding, `NoWalk` rule and real tenths-of-a-unit floor heights), so
//! every spawn pose lands on the client's generated navmesh at the real floor
//! height. Snapping globally — not per tile — matters at cell edges, where the
//! reference may snap a member into a neighbouring cell; that is what lets the
//! client's chase path resolve a start node and actually run to the player.

use std::collections::HashMap;

use crate::corpus::ContentError;
use crate::gc;
use crate::maze::{self, MazeCell};
use crate::pkg::Archive;
use crate::zone::{
    class_is_creature, generated_room_world_entity_tables, hand_placed_world_entity_tables, parse,
    read_text, world_entity_table_choice,
};

/// Hard cap on mobs per generated level (reference `_MAX_MOBS_PER_LEVEL`).
pub const MAX_MOBS_PER_LEVEL: usize = 150;

/// The encounter spawn-budget (reference `_ENCOUNTER_DIFFICULTY_BUDGET`).
const ENCOUNTER_DIFFICULTY_BUDGET: f64 = 2.25;
/// Default encounter-area side when a marker carries no `<N>` budget.
const DEFAULT_ENCOUNTER_SIZE: f64 = 180.0;
/// Never clamp a pack tighter than this half-extent.
const MIN_GROUP_HALF: f64 = 30.0;
/// Per-spot group offsets so mobs at one marker do not stack.
const GROUP_OFFSETS: [(f64, f64); 13] = [
    (0.0, 0.0),
    (60.0, 0.0),
    (-60.0, 0.0),
    (0.0, 60.0),
    (0.0, -60.0),
    (45.0, 45.0),
    (-45.0, 45.0),
    (45.0, -45.0),
    (-45.0, -45.0),
    (90.0, 30.0),
    (-90.0, -30.0),
    (30.0, 90.0),
    (-30.0, -90.0),
];

/// One mob slot in an encounter group: the entity asset the client loads, the
/// concrete `creatures.*` row its stats resolve from (empty when the entity's
/// own file states no `extends`), and the authored spawn-budget weight.
#[derive(Debug, Clone, PartialEq)]
pub struct EncounterUnit {
    /// The `world.<dungeon>.mob.*` (or raw `creatures.*`) asset.
    pub entity: String,
    /// The concrete `creatures.*` class the entity extends (may be empty).
    pub creature: String,
    /// The authored `EncounterUnit.Difficulty` spawn-budget weight.
    pub difficulty: f64,
    /// The authored `EncounterUnit.LevelOffset` added to the zone base level.
    pub level_offset: i64,
}

/// One generated mob spawn: the client-loadable asset, its x256 pose
/// `[x, y, z, heading]`, its level byte and its x256 wire HP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobSpawn {
    /// The entity asset the client loads (`world.<dungeon>.mob.*.rankN`).
    pub classname: String,
    /// `[x, y, z, heading]` in the x256 wire domain.
    pub pose: [i32; 4],
    /// The Unit level byte (zone base level).
    pub level: u8,
    /// The x256 wire HP the client derives locally (`getMaxHealth(level)<<8`).
    /// Zero when the entity's creature stats could not be resolved (the
    /// caller skips those rather than shipping a desynced stream).
    pub hp_wire: u32,
    /// The creature's authored `Behavior` class (e.g.
    /// `creatures.base.behavior.Melee`), or `None` when its chain states none.
    /// The join sanitizes this to a concrete brain matched to the primary
    /// weapon kind (see `drf_content::mob_behavior_class`).
    pub behavior: Option<String>,
}

/// The `.gc` archive path for a dotted classname (`a.b.C` -> `a/b/C.gc`).
fn gc_path(classname: &str) -> String {
    format!("{}.gc", classname.replace('.', "/"))
}

fn unquote(value: &str) -> String {
    let text = value.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text[1..text.len() - 1].to_string()
    } else {
        text.to_string()
    }
}

fn leaf_eq(dotted: &str, want: &str) -> bool {
    dotted
        .rsplit('.')
        .next()
        .is_some_and(|leaf| leaf.eq_ignore_ascii_case(want))
}

/// Load an encounter table's groups (ordered) from a `world.*.enc.*` class.
///
/// Each `* extends Encounter` child is one group; each `* extends
/// EncounterUnit` grandchild is one mob slot. Mirrors the reference
/// `dungeon_encounters` row walk (ordered by group then unit).
pub fn encounter_groups(
    archive: &Archive,
    enc_type: &str,
) -> Result<Vec<Vec<EncounterUnit>>, ContentError> {
    if enc_type.trim().is_empty() {
        return Ok(Vec::new());
    }
    let Some(text) = read_text(archive, &gc_path(enc_type.trim()))? else {
        return Ok(Vec::new());
    };
    let file = gc::parse(&text);
    let Some(root) = file.children.first() else {
        return Ok(Vec::new());
    };
    let mut groups = Vec::new();
    for child in &root.children {
        if child.name != "<anonymous>" {
            continue;
        }
        if !child
            .extends
            .as_deref()
            .is_some_and(|e| leaf_eq(e, "Encounter"))
        {
            continue;
        }
        let mut units = Vec::new();
        for slot in &child.children {
            if slot.name != "<anonymous>" {
                continue;
            }
            if !slot
                .extends
                .as_deref()
                .is_some_and(|e| leaf_eq(e, "EncounterUnit"))
            {
                continue;
            }
            let entity = slot
                .prop("Type")
                .map(unquote)
                .unwrap_or_default()
                .trim()
                .to_string();
            if entity.is_empty() {
                continue;
            }
            let difficulty = slot
                .prop("Difficulty")
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(1.0);
            let level_offset = slot.prop("LevelOffset").and_then(parse_i64).unwrap_or(0);
            let creature = entity_extend(archive, &entity)?.unwrap_or_default();
            units.push(EncounterUnit {
                entity,
                creature,
                difficulty,
                level_offset,
            });
        }
        if !units.is_empty() {
            groups.push(units);
        }
    }
    Ok(groups)
}

/// The `extends` target of a dotted classname, or `None` when the file/block
/// is absent.
fn entity_extend(archive: &Archive, classname: &str) -> Result<Option<String>, ContentError> {
    Ok(class_node(archive, classname)?.and_then(|node| node.extends))
}

/// Resolve a dotted classname to its parsed block.
///
/// A class `a.b.C` lives either in its own file `a/b/C.gc` (a top-level object)
/// or as a nested block `C` inside `a/b.gc` (e.g. `Basic.Pup`); the object's own
/// file is probed first, then the parent file, mirroring the reference's
/// `tile_cobj_resolver.resolve_extends_path`.
fn class_node(archive: &Archive, classname: &str) -> Result<Option<gc::Node>, ContentError> {
    let dotted = classname.trim();
    if dotted.is_empty() {
        return Ok(None);
    }
    let leaf = dotted.rsplit('.').next().unwrap_or(dotted);
    let full = format!("{}.gc", dotted.replace('.', "/"));
    if let Some(text) = read_text(archive, &full)? {
        let file = gc::parse(&text);
        if let Some(node) = find_block(&file, leaf) {
            return Ok(Some(node.clone()));
        }
    }
    if let Some(dot) = dotted.rfind('.') {
        let parent = format!("{}.gc", dotted[..dot].replace('.', "/"));
        if let Some(text) = read_text(archive, &parent)? {
            let file = gc::parse(&text);
            if let Some(node) = find_block(&file, leaf) {
                return Ok(Some(node.clone()));
            }
        }
    }
    Ok(None)
}

/// Find the first block named `leaf` (case-insensitive) anywhere in a parsed
/// file, so nested objects (`Basic.Pup`) resolve like top-level ones.
fn find_block<'a>(file: &'a gc::Node, leaf: &str) -> Option<&'a gc::Node> {
    let mut stack: Vec<&gc::Node> = file.children.iter().collect();
    while let Some(node) = stack.pop() {
        if node.name.eq_ignore_ascii_case(leaf) {
            return Some(node);
        }
        stack.extend(node.children.iter());
    }
    None
}

/// A `Name = value` row on a block or, failing that, on any descendant. The
/// authored creature stats (`CreatureDifficulty`/`Difficulty`/`MaxHealth`) live
/// inside a `Description` child, not directly on the archetype block.
fn node_prop(node: &gc::Node, name: &str) -> Option<String> {
    if let Some(value) = node.prop(name) {
        return Some(value.to_string());
    }
    let mut stack: Vec<&gc::Node> = node.children.iter().collect();
    while let Some(child) = stack.pop() {
        if let Some(value) = child.prop(name) {
            return Some(value.to_string());
        }
        stack.extend(child.children.iter());
    }
    None
}

// -- level + HP derivation ----------------------------------------------------

/// The `MonsterHealth` curve from `Tables.gc` (level, value) pairs in Fixed32.
struct HealthCurve {
    points: Vec<(i64, i64)>,
}

impl HealthCurve {
    /// `Tables.gc`'s `MonsterHealth` curve; when the file is absent, the two
    /// authored points the reference's `monster_curves.py` transcribes
    /// (L1 -> 60.5, L100 -> 5452). Both are the same content values.
    fn from_archive(archive: &Archive) -> Result<HealthCurve, ContentError> {
        if let Some(text) = read_text(archive, "Tables.gc")? {
            let file = gc::parse(&text);
            if let Some(node) = find_block(&file, "MonsterHealth") {
                let mut points = Vec::new();
                for entry in &node.children {
                    let level = entry.prop("Level").and_then(parse_i64);
                    let value = entry.prop("Value").and_then(parse_f64);
                    if let (Some(level), Some(value)) = (level, value) {
                        points.push((level * 256, fixed32(value)));
                    }
                }
                if !points.is_empty() {
                    points.sort_by_key(|&(level, _)| level);
                    return Ok(HealthCurve { points });
                }
            }
        }
        Ok(HealthCurve {
            points: vec![(256, 15488), (100 * 256, 1395712)],
        })
    }

    /// Linear-interpolate the Fixed32 base HP at `level` (C# `CurveTableEntry`).
    fn base_fixed32(&self, level: i64) -> i64 {
        let key = level.clamp(1, 110) << 8;
        let points = &self.points;
        if points.is_empty() {
            return 0;
        }
        if key <= points[0].0 {
            return points[0].1;
        }
        if key >= points[points.len() - 1].0 {
            return points[points.len() - 1].1;
        }
        for window in points.windows(2) {
            let (k0, v0) = window[0];
            let (k1, v1) = window[1];
            if key <= k1 {
                let frac = ((key - k0) * 65536) / (k1 - k0);
                return v0 + ((v1 - v0) * frac) / 65536;
            }
        }
        points[points.len() - 1].1
    }
}

/// The `creatures.*` archetype stats a mob's HP/behavior resolve from.
#[derive(Debug, Default, Clone)]
struct CreatureStats {
    tier: Option<String>,
    difficulty: Option<f64>,
    max_health: Option<f64>,
    behavior: Option<String>,
}

/// Walk a `creatures.*` class's `extends` chain (leaf first) collecting the
/// first-seen `CreatureDifficulty`/`Difficulty`/`MaxHealth` and the `Behavior`
/// block's class.
fn creature_stats(archive: &Archive, creature: &str) -> Result<CreatureStats, ContentError> {
    let mut stats = CreatureStats::default();
    let mut current = creature.trim().to_string();
    for _ in 0..8 {
        if current.is_empty() {
            break;
        }
        let Some(node) = class_node(archive, &current)? else {
            break;
        };
        if stats.tier.is_none() {
            stats.tier = node_prop(&node, "CreatureDifficulty").map(|v| v.trim().to_string());
        }
        if stats.difficulty.is_none() {
            stats.difficulty =
                node_prop(&node, "Difficulty").and_then(|v| v.trim().parse::<f64>().ok());
        }
        if stats.max_health.is_none() {
            stats.max_health =
                node_prop(&node, "MaxHealth").and_then(|v| v.trim().parse::<f64>().ok());
        }
        if stats.behavior.is_none() {
            stats.behavior = node
                .children
                .iter()
                .find(|child| child.name.eq_ignore_ascii_case("Behavior"))
                .and_then(|child| child.extends.clone());
        }
        current = node.extends.clone().unwrap_or_default();
    }
    Ok(stats)
}

/// The case-insensitive difficulty-tier HP multiplier (C# `DifficultyModifiers`).
fn tier_modifier(tier: Option<&str>) -> f64 {
    match tier.map(str::to_ascii_uppercase).as_deref() {
        Some("FODDER") => 0.5,
        Some("RECRUIT") => 1.0,
        Some("VETERAN") => 2.0,
        Some("WARMONGER") => 2.5,
        Some("CHAMPION") => 4.0,
        Some("HERO") => 7.0,
        Some("DUNGEON_BOSS") | Some("BOSS") => 8.0,
        _ => 1.0,
    }
}

/// The baseline mob level for a zone.
///
/// A live join scales the mob to the character's level inside the zone's
/// authored `[MinLevel, MaxLevel]` window (the range the content is designed
/// for; HDoL clamps a level-15 character up to 25). The reference spawner has
/// no player level, so it keeps the legacy `dungeonNN -> NN*4+2` ladder for
/// its pinned outputs; the authored minimum is the fallback for non-dungeon
/// generated zones.
fn zone_base_level(archive: &Archive, zone: &str, player_level: Option<u16>) -> i64 {
    let def = crate::zone::zone_def(archive, zone).ok().flatten();
    if let Some(player_level) = player_level {
        let min = def.as_ref().and_then(|def| def.min_level).unwrap_or(1);
        let max = def.as_ref().and_then(|def| def.max_level).unwrap_or(100);
        return i64::from(player_level).clamp(min, max).clamp(1, 100);
    }
    let lower = zone.to_ascii_lowercase();
    if lower.starts_with("dungeon") && lower.len() >= 9 {
        if let Ok(number) = lower[7..9].parse::<i64>() {
            return number * 4 + 2;
        }
    }
    def.and_then(|def| def.min_level).unwrap_or(2)
}

/// The exact C# `MonsterHealthTable.CalculateHP` (Fixed32 integer math).
fn calculate_hp(base_fixed32: i64, difficulty: f64, health_mod: f64) -> i64 {
    let diff_fixed = (difficulty.max(0.0) * 256.0) as i64;
    let mod_fixed = (health_mod.max(0.0) * 256.0) as i64;
    let hp_fixed = (base_fixed32 * mod_fixed) >> 8;
    let hp_fixed = (hp_fixed * diff_fixed) >> 8;
    (hp_fixed >> 8).max(1)
}

/// The x256 HP and authored behavior a creature class resolves to at an
/// explicit level. Runtime summons use the dead unit's level, not the zone's
/// base, so they cannot go through [`mob_level_hp`].
pub fn mob_profile_at_level(
    archive: &Archive,
    classname: &str,
    level: u8,
) -> Result<Option<(u32, Option<String>)>, ContentError> {
    let Some(creature) = entity_extend(archive, classname)? else {
        return Ok(None);
    };
    let stats = creature_stats(archive, &creature)?;
    let multiplier = stats
        .difficulty
        .unwrap_or_else(|| tier_modifier(stats.tier.as_deref()));
    let health_mod = stats.max_health.unwrap_or(1.0);
    let curve = HealthCurve::from_archive(archive)?;
    let raw = calculate_hp(
        curve.base_fixed32(i64::from(level.clamp(1, 100))),
        multiplier,
        health_mod,
    );
    Ok(Some(((raw as u32).wrapping_mul(256), stats.behavior)))
}

/// The `(level, hp_wire, authored behavior)` a mob's classname resolves to,
/// or `None` when its creature stats cannot be read.
fn mob_level_hp(
    archive: &Archive,
    base_level: i64,
    classname: &str,
    curve: &HealthCurve,
    level_offset: i64,
) -> Result<Option<(u8, u32, Option<String>)>, ContentError> {
    let level = (base_level + level_offset).clamp(1, 100);
    let Some(creature) = entity_extend(archive, classname)? else {
        return Ok(None);
    };
    let stats = creature_stats(archive, &creature)?;
    let multiplier = stats
        .difficulty
        .unwrap_or_else(|| tier_modifier(stats.tier.as_deref()));
    let health_mod = stats.max_health.unwrap_or(1.0);
    let raw = calculate_hp(curve.base_fixed32(level), multiplier, health_mod);
    Ok(Some((
        level as u8,
        (raw as u32).wrapping_mul(256),
        stats.behavior,
    )))
}

// -- placement ----------------------------------------------------------------

/// The main `EncounterTable` a generated world names (`<zone>.world`), or
/// `None` when the world states none.
fn world_encounter_table(archive: &Archive, zone: &str) -> Result<Option<String>, ContentError> {
    let Some(text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(None);
    };
    let file = parse(&text);
    Ok(file
        .children
        .first()
        .and_then(|root| root.prop("EncounterTable"))
        .map(unquote)
        .filter(|value| !value.is_empty()))
}

/// Map each `*.world` room node's `source_index` (file order, matching the
/// maze's own numbering) to its `EncounterType`, when it states one.
fn world_room_encounters(
    archive: &Archive,
    zone: &str,
) -> Result<HashMap<i64, String>, ContentError> {
    let Some(text) = read_text(archive, &format!("{zone}.world"))? else {
        return Ok(HashMap::new());
    };
    let file = parse(&text);
    let Some(root) = file.children.first() else {
        return Ok(HashMap::new());
    };
    let mut out = HashMap::new();
    let mut source_index = 0i64;
    for child in &root.children {
        if child.name != "<anonymous>" {
            continue;
        }
        let tile_set = child.prop("TileSet").map(unquote).unwrap_or_default();
        if tile_set.is_empty() {
            continue;
        }
        if let Some(enc) = child
            .prop("EncounterType")
            .map(unquote)
            .filter(|value| !value.is_empty())
        {
            out.insert(source_index, enc);
        }
        source_index += 1;
    }
    Ok(out)
}

/// The x256 wire value of a world float (reference `int(pos * 256)`).
fn x256(value: f64) -> i32 {
    (value * 256.0) as i32
}

/// A x256 word back to a signed world float. Table placements keep their pose
/// as `u32` words, so a negative Y (Rattle Tooth's lair) must be reinterpreted
/// as i32 before the f64 conversion.
fn wire_float(word: u32) -> f64 {
    f64::from(word as i32) / 256.0
}

/// The signed reinterpretation of a u32 (the maze's layout seed as C# `int`).
fn to_int32(value: u32) -> i32 {
    value as i32
}

/// A faithful port of .NET's subtractive `System.Random` (the jitter source).
struct DotNetRandom {
    seed_array: [i32; 56],
    inext: usize,
    inextp: usize,
}

const MBIG: i64 = 2_147_483_647;
const MSEED: i64 = 161_803_398;

impl DotNetRandom {
    fn new(seed: i32) -> DotNetRandom {
        let subtraction = if seed == i32::MIN {
            MBIG
        } else {
            (seed as i64).abs()
        };
        let mut seed_array = [0i32; 56];
        let mut mj = MSEED - subtraction;
        seed_array[55] = mj as i32;
        let mut mk = 1i64;
        for i in 1..55i64 {
            let ii = ((21 * i) % 55) as usize;
            seed_array[ii] = mk as i32;
            mk = mj - mk;
            if mk < 0 {
                mk += MBIG;
            }
            mj = seed_array[ii] as i64;
        }
        for _ in 0..4 {
            for i in 1..56usize {
                let other = seed_array[1 + (i + 30) % 55] as i64;
                let mut value = seed_array[i] as i64 - other;
                if value < 0 {
                    value += MBIG;
                }
                seed_array[i] = value as i32;
            }
        }
        DotNetRandom {
            seed_array,
            inext: 0,
            inextp: 21,
        }
    }

    fn sample(&mut self) -> f64 {
        let mut inext = self.inext + 1;
        if inext >= 56 {
            inext = 1;
        }
        let mut inextp = self.inextp + 1;
        if inextp >= 56 {
            inextp = 1;
        }
        let mut value = self.seed_array[inext] as i64 - self.seed_array[inextp] as i64;
        if value == MBIG {
            value -= 1;
        }
        if value < 0 {
            value += MBIG;
        }
        self.seed_array[inext] = value as i32;
        self.inext = inext;
        self.inextp = inextp;
        value as f64 * (1.0 / MBIG as f64)
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + self.sample() * (hi - lo)
    }
}

/// FNV-1a (32-bit) of a spawn-spot key (reference `_stable_spot_seed`).
fn stable_spot_seed(key: &str) -> u32 {
    let mut hash: u32 = 2_166_136_261;
    for ch in key.chars() {
        hash ^= ch as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

/// The per-spot difficulty budget in `[1.125, 2.25]` (reference
/// `_resolve_spot_budget`).
fn resolve_spot_budget(spot_seed: u32) -> f64 {
    let mut hash = spot_seed.wrapping_mul(2_654_435_761);
    hash ^= hash >> 15;
    let frac = f64::from(hash & 0xFFFF) / 65535.0;
    ENCOUNTER_DIFFICULTY_BUDGET * (0.5 + 0.5 * frac)
}

/// Expand one encounter group into a full pack for a spawn spot (reference
/// `expand_encounter_group`): every authored unit once, then weighted units
/// round-robin until the hashed budget is spent.
fn expand_encounter_group(group: &[EncounterUnit], spot_seed: u32) -> Vec<EncounterUnit> {
    let mut result: Vec<EncounterUnit> = group.to_vec();
    let weighted: Vec<EncounterUnit> = group
        .iter()
        .filter(|unit| unit.difficulty > 0.0)
        .cloned()
        .collect();
    if weighted.is_empty() {
        return result;
    }
    let spent: f64 = weighted.iter().map(|unit| unit.difficulty).sum();
    let spot_budget = spent.max(resolve_spot_budget(spot_seed));
    let min_weight = weighted
        .iter()
        .map(|unit| unit.difficulty)
        .fold(f64::INFINITY, f64::min);

    let mut spent = spent;
    let mut fill_index = 0usize;
    while spot_budget - spent >= min_weight && result.len() < 16 {
        let unit = &weighted[fill_index % weighted.len()];
        if spent + unit.difficulty > spot_budget {
            break;
        }
        result.push(unit.clone());
        spent += unit.difficulty;
        fill_index += 1;
    }
    result
}

/// The stateful spawn builder: holds the jitter RNG, the emitted spawns and a
/// One resolved `(level, hp_wire, behavior)` spawn profile.
type SpawnProfile = Option<(u8, u32, Option<String>)>;

/// per-classname/offset level/HP cache.
struct SpawnGen<'a> {
    archive: &'a Archive,
    base_level: i64,
    curve: HealthCurve,
    rng: DotNetRandom,
    spawns: Vec<MobSpawn>,
    stats: HashMap<(String, i64), SpawnProfile>,
}

impl SpawnGen<'_> {
    fn level_hp(&mut self, classname: &str, level_offset: i64) -> SpawnProfile {
        let key = (classname.to_string(), level_offset);
        if let Some(cached) = self.stats.get(&key) {
            return cached.clone();
        }
        let resolved = mob_level_hp(
            self.archive,
            self.base_level,
            classname,
            &self.curve,
            level_offset,
        )
        .ok()
        .flatten();
        self.stats.insert(key, resolved.clone());
        resolved
    }

    /// Emit one already-selected table entity at its authored pose. Used by
    /// the generic `WorldEntityGeneratorTable` / `EncounterWorldEntityTable`
    /// path, where the content names one concrete creature rather than an
    /// encounter pack.
    fn emit_one(&mut self, classname: &str, x: f64, y: f64, z: f64, key: &str) {
        if self.spawns.len() >= MAX_MOBS_PER_LEVEL {
            return;
        }
        let (level, hp_wire, behavior) = self.level_hp(classname, 0).unwrap_or((0, 0, None));
        if hp_wire == 0 {
            return;
        }
        let _ = key;
        let heading = self.rng.range(0.0, 360.0);
        self.spawns.push(MobSpawn {
            classname: classname.to_string(),
            pose: [x256(x), x256(y), x256(z), x256(heading)],
            level,
            hp_wire,
            behavior,
        });
    }

    /// Emit one expanded pack at a marker (reference `_emit_group`).
    ///
    /// The pack base and every member are snapped onto the per-instance
    /// collision-derived pathmap before the pose is emitted. The client's chase
    /// path asks its own `PathMap` for the closest walkable node at the mob's
    /// pose; a pose nudged into a wall by a group offset (or the seeded jitter)
    /// has no such node and the path request silently fails (`MoveToPoint` ->
    /// `StopMoving`), which is the aggro-in-place/leash-back symptom. Snapping
    /// keeps every spawn on the client's navmesh.
    #[allow(clippy::too_many_arguments)]
    fn emit_group(
        &mut self,
        pathmap: Option<&maze::DungeonPathMap>,
        group: &[EncounterUnit],
        world_x: f64,
        world_y: f64,
        fallback_z: f64,
        budget: u32,
        spot_key: &str,
    ) {
        let group = expand_encounter_group(group, stable_spot_seed(spot_key));
        let (base_x, base_y) = match pathmap {
            Some(pathmap) => match pathmap.find_walkable_spot(world_x, world_y, 250.0) {
                (x, y, true) => (x, y),
                _ => return,
            },
            None => (world_x, world_y),
        };
        let half = ((if budget == 0 {
            DEFAULT_ENCOUNTER_SIZE
        } else {
            f64::from(budget)
        }) / 2.0
            - 10.0)
            .max(MIN_GROUP_HALF);
        for (slot, unit) in group.iter().enumerate() {
            if self.spawns.len() >= MAX_MOBS_PER_LEVEL {
                return;
            }
            let offset = GROUP_OFFSETS[slot % GROUP_OFFSETS.len()];
            let ox = (offset.0 + self.rng.range(-12.0, 12.0)).clamp(-half, half);
            let oy = (offset.1 + self.rng.range(-12.0, 12.0)).clamp(-half, half);
            let heading = self.rng.range(0.0, 360.0);
            let (sx, sy) = match pathmap {
                Some(pathmap) => match pathmap.find_walkable_spot(base_x + ox, base_y + oy, 60.0) {
                    (x, y, true) => (x, y),
                    _ => continue,
                },
                None => (base_x + ox, base_y + oy),
            };
            let z = pathmap
                .map(|pathmap| pathmap.get_height_at(sx, sy, fallback_z))
                .unwrap_or(fallback_z);
            let (level, hp_wire, behavior) = self
                .level_hp(&unit.entity, unit.level_offset)
                .unwrap_or((0, 0, None));
            self.spawns.push(MobSpawn {
                classname: unit.entity.clone(),
                pose: [x256(sx), x256(sy), x256(z), x256(heading)],
                level,
                hp_wire,
                behavior,
            });
        }
    }
}

/// Generate a procedural dungeon level's mob spawns (reference
/// `generate_spawns`), in the order they are emitted.
///
/// Returns an empty list for a non-maze zone, a level with no resolvable
/// encounter table, or an unreadable world. Every spawn's pose is the cell
/// origin plus the tile's own `base.Encounter<N>` marker plus the pack offset,
/// then snapped onto the tile's collision-derived walkable floor (X/Y) with the
/// real floor height (Z), so it lands on the client's generated navmesh.
pub fn spawns(archive: &Archive, zone: &str) -> Result<Vec<MobSpawn>, ContentError> {
    spawns_with_seed(archive, zone, maze::layout_seed(zone))
}

/// [`spawns`] with the per-entry maze seed the server put in the Assign.
pub fn spawns_with_seed(
    archive: &Archive,
    zone: &str,
    seed: u32,
) -> Result<Vec<MobSpawn>, ContentError> {
    spawns_with_seed_impl(archive, zone, seed, None, false)
}

/// [`spawns_with_seed`] for a live join, with unreachable collision islands
/// relocated onto the main walkable region.
///
/// The reference generator is kept byte-for-byte by [`spawns_with_seed`]; this
/// variant applies the server's playability guard so a mob cannot be stranded
/// on a raised prop, in a sealed pocket, or outside the client's navigation
/// mesh.
pub fn spawns_with_seed_reachable(
    archive: &Archive,
    zone: &str,
    seed: u32,
    player_level: u16,
) -> Result<Vec<MobSpawn>, ContentError> {
    spawns_with_seed_impl(archive, zone, seed, Some(player_level), true)
}

fn spawns_with_seed_impl(
    archive: &Archive,
    zone: &str,
    seed: u32,
    player_level: Option<u16>,
    reachable_only: bool,
) -> Result<Vec<MobSpawn>, ContentError> {
    // A hand-placed world (Rattle Tooth's Lair and the other boss/oneoff
    // worlds) authors its enemies as `base.Encounter` markers in its
    // `Entities` block, each naming the root or its own `EncounterTable`.
    // Those are not maze cells, so handle them before the generated path.
    if let Some(world_text) = read_text(archive, &format!("{zone}.world"))? {
        let file = parse(&world_text);
        if let Some(root) = file.children.first() {
            let generated = root
                .prop("Generated")
                .map(unquote)
                .is_some_and(|value| value.eq_ignore_ascii_case("true"));
            if !generated {
                return hand_placed_spawns(archive, zone, root, seed, player_level);
            }
        }
    }
    let Some(layout) = maze::generate_layout_with_seed(archive, zone, seed)? else {
        return Ok(Vec::new());
    };
    let Some(table) = world_encounter_table(archive, zone)? else {
        return Ok(Vec::new());
    };
    let groups = encounter_groups(archive, &table)?;
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let room_encounters = world_room_encounters(archive, zone)?;
    let curve = HealthCurve::from_archive(archive)?;
    let base_level = zone_base_level(archive, zone, player_level);
    let pathmap = maze::build_path_map(archive, &layout.cells)?;

    let entry = maze::resolve_entry_with_seed(archive, zone, "", seed)
        .ok()
        .flatten()
        .map(|entry| (entry.grid_x, entry.grid_y))
        .or_else(|| {
            layout
                .cells
                .iter()
                .map(|cell| (cell.grid_x, cell.grid_y))
                .max_by_key(|&(x, y)| (x, -y))
        });

    let mut gen = SpawnGen {
        archive,
        base_level,
        curve,
        rng: DotNetRandom::new(to_int32(maze::layout_seed(zone)) ^ 0x5F5F_5F5F),
        spawns: Vec::new(),
        stats: HashMap::new(),
    };

    // Step 1: each placed room node emits its OWN encounter at its cell.
    let mut room_cells: Vec<(i64, i64)> = Vec::new();
    for placed in &layout.placed_room_nodes {
        room_cells.push((placed.grid_x, placed.grid_y));
        if Some((placed.grid_x, placed.grid_y)) == entry {
            continue;
        }
        let Some(encounter) = room_encounters.get(&placed.source_index) else {
            continue;
        };
        let node_groups = encounter_groups(archive, encounter)?;
        if node_groups.is_empty() {
            continue;
        }
        let index = (placed.source_index.unsigned_abs() as usize) % node_groups.len();
        let group = &node_groups[index];
        let cell = layout
            .cells
            .iter()
            .find(|cell| cell.grid_x == placed.grid_x && cell.grid_y == placed.grid_y);
        let Some(cell) = cell else {
            continue;
        };
        let markers = maze::tile_encounter_markers(archive, &cell.tile_type)?;
        let (mx, my, mz, budget) = markers
            .first()
            .map(|m| (m.x, m.y, m.z, m.budget))
            .unwrap_or_else(|| tile_center(cell));
        gen.emit_group(
            pathmap.as_ref(),
            group,
            cell.world_origin_x + mx,
            cell.world_origin_y + my,
            mz,
            budget,
            &format!("{zone}:leader:{}", placed.source_index),
        );
    }

    // Step 2: fill the remaining cells with the main table, in the client's
    // order (top-to-bottom, left-to-right).
    let mut sorted: Vec<&MazeCell> = layout.cells.iter().collect();
    sorted.sort_by_key(|cell| (-cell.grid_y, cell.grid_x));
    let mut enc_idx = 0usize;
    for cell in sorted {
        if Some((cell.grid_x, cell.grid_y)) == entry {
            continue;
        }
        if room_cells.contains(&(cell.grid_x, cell.grid_y)) {
            continue;
        }
        let markers = maze::tile_encounter_markers(archive, &cell.tile_type)?;
        for marker in markers {
            if gen.spawns.len() >= MAX_MOBS_PER_LEVEL {
                break;
            }
            let group = &groups[enc_idx % groups.len()];
            gen.emit_group(
                pathmap.as_ref(),
                group,
                cell.world_origin_x + marker.x,
                cell.world_origin_y + marker.y,
                marker.z,
                marker.budget,
                &format!("{zone}:enc:{enc_idx}"),
            );
            enc_idx += 1;
        }
    }

    // Generated room nodes can name a `WorldEntityGeneratorTable` instead of
    // (or beside) an `EncounterType`. Creature choices are extra mobs at the
    // room cell; non-creatures are emitted by the entity derivation.
    for table in generated_room_world_entity_tables(archive, zone, seed)? {
        let Some(classname) = world_entity_table_choice(archive, &table.table, &table.key)? else {
            continue;
        };
        if class_is_creature(archive, &classname)? {
            gen.emit_one(
                &classname,
                wire_float(table.pose[0]),
                wire_float(table.pose[1]),
                wire_float(table.pose[2]),
                &table.key,
            );
        }
    }

    if reachable_only {
        if let Some(pathmap) = pathmap.as_ref() {
            relocate_unreachable_spawns(pathmap, &layout.cells, &mut gen.spawns, zone);
        }
    }
    Ok(gen.spawns)
}

/// Move spawns off isolated collision islands and back onto the largest
/// connected walkable region.
///
/// The generated pose is only as good as the collision pathmap. Raised props,
/// sealed side rooms and tile geometry that is not on the client's navmesh can
/// still produce a locally "walkable" node that no unit can path to. Keeping
/// those spawns makes a kill objective or a room clear impossible. Relocate
/// each one to the nearest open node in the main region; drop it only when no
/// such node exists within the search radius.
fn relocate_unreachable_spawns(
    pathmap: &maze::DungeonPathMap,
    cells: &[MazeCell],
    spawns: &mut Vec<MobSpawn>,
    zone: &str,
) {
    let components = pathmap.walkable_components();
    let Some((largest_index, _)) = components
        .iter()
        .enumerate()
        .max_by_key(|(_, component)| component.len())
    else {
        return;
    };
    let mut kept = Vec::with_capacity(spawns.len());
    let mut moved = 0usize;
    let mut dropped = 0usize;
    for mut spawn in spawns.drain(..) {
        let x = f64::from(spawn.pose[0]) / 256.0;
        let y = f64::from(spawn.pose[1]) / 256.0;
        // Keep the mob on its own cell's floor layer when that layer contains
        // the cell centre. A cell can contain two collision components (a
        // raised prop/upper surface and the actual room floor); using the
        // cell centre avoids moving an entire room just because the global
        // largest component lies elsewhere.
        let cell = cells.iter().find(|cell| {
            x >= cell.world_origin_x
                && x < cell.world_origin_x + cell.tile_size
                && y >= cell.world_origin_y
                && y < cell.world_origin_y + cell.tile_size
        });
        let target_index = cell
            .and_then(|cell| {
                pathmap.component_at(
                    cell.world_origin_x + cell.tile_size / 2.0,
                    cell.world_origin_y + cell.tile_size / 2.0,
                    &components,
                )
            })
            .unwrap_or(largest_index);
        let component = &components[target_index];
        let (mut sx, mut sy, mut found) =
            pathmap.find_walkable_spot_in_component(x, y, 2500.0, component);
        if !found {
            // The cell's centre layer can be a small disconnected piece (for
            // example a one-way ledge). Fall back to the largest region so a
            // quest mob is never silently deleted.
            (sx, sy, found) =
                pathmap.find_walkable_spot_in_component(x, y, 2500.0, &components[largest_index]);
        }
        if !found {
            dropped += 1;
            continue;
        }
        if (sx - x).abs() > f64::EPSILON || (sy - y).abs() > f64::EPSILON {
            moved += 1;
        }
        spawn.pose[0] = x256(sx);
        spawn.pose[1] = x256(sy);
        spawn.pose[2] = x256(pathmap.get_height_at(sx, sy, f64::from(spawn.pose[2]) / 256.0));
        kept.push(spawn);
    }
    *spawns = kept;
    if moved != 0 || dropped != 0 {
        eprintln!(
            "spawn placement {zone}: relocated {moved} unreachable spawn(s), dropped {dropped}"
        );
    }
}

/// Spawn every hand-placed world's `base.Encounter` marker from its
/// `EncounterTable`; the same table/groups/HP/behavior derivation the
/// generated path uses, anchored at the marker's own `Position`.
fn hand_placed_spawns(
    archive: &Archive,
    zone: &str,
    root: &crate::zone::Block,
    seed: u32,
    player_level: Option<u16>,
) -> Result<Vec<MobSpawn>, ContentError> {
    let root_table = root.prop("EncounterTable").map(unquote).unwrap_or_default();
    let Some(entities) = root
        .children
        .iter()
        .find(|child| child.name.eq_ignore_ascii_case("Entities"))
    else {
        return Ok(Vec::new());
    };
    let curve = HealthCurve::from_archive(archive)?;
    let base_level = zone_base_level(archive, zone, player_level);
    let mut gen = SpawnGen {
        archive,
        base_level,
        curve,
        rng: DotNetRandom::new(to_int32(seed) ^ 0x5F5F_5F5F),
        spawns: Vec::new(),
        stats: HashMap::new(),
    };
    let mut index = 0usize;
    for child in &entities.children {
        let Some(extends) = child.extends.as_deref() else {
            continue;
        };
        if !leaf_eq(extends, "Encounter") {
            // A hand-placed creature is a mob at its own `Position`; NPCs and
            // interactives are shipped by `derive_world_entities` instead.
            if class_is_creature(archive, extends)? {
                let position = child.prop("Position").map(unquote).unwrap_or_default();
                let values: Vec<f64> = position.split(',').filter_map(parse_f64).collect();
                if values.len() < 3 {
                    continue;
                }
                let (level, computed_hp, behavior) =
                    gen.level_hp(extends, 0).unwrap_or((0, 0, None));
                let hp_wire = child
                    .prop("HitPoints")
                    .and_then(parse_i64)
                    .map(|value| value.max(1) as u32)
                    .unwrap_or(computed_hp);
                if hp_wire == 0 {
                    continue;
                }
                let heading = child.prop("Heading").and_then(parse_f64).unwrap_or(0.0);
                gen.spawns.push(MobSpawn {
                    classname: extends.to_string(),
                    pose: [
                        x256(values[0]),
                        x256(values[1]),
                        x256(values[2]),
                        x256(heading),
                    ],
                    level,
                    hp_wire,
                    behavior,
                });
            }
            continue;
        }
        let table = child
            .prop("EncounterTable")
            .map(unquote)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| root_table.clone());
        let groups = encounter_groups(archive, &table)?;
        if groups.is_empty() {
            continue;
        }
        let position = child.prop("Position").map(unquote).unwrap_or_default();
        let values: Vec<f64> = position.split(',').filter_map(parse_f64).collect();
        if values.len() < 3 {
            continue;
        }
        let group = &groups[index % groups.len()];
        index += 1;
        let name = child.prop("Name").map(unquote).unwrap_or_default();
        gen.emit_group(
            None,
            group,
            values[0],
            values[1],
            values[2],
            0,
            &format!("{zone}:hand:{index}:{name}"),
        );
    }
    // `RoomPlaceholderEntity` / `PlaceholderEntity` records name a
    // `WorldEntityGeneratorTable`; a creature choice is a mob at that pose.
    for table in hand_placed_world_entity_tables(archive, zone)? {
        let Some(classname) = world_entity_table_choice(archive, &table.table, &table.key)? else {
            continue;
        };
        if class_is_creature(archive, &classname)? {
            gen.emit_one(
                &classname,
                wire_float(table.pose[0]),
                wire_float(table.pose[1]),
                wire_float(table.pose[2]),
                &table.key,
            );
        }
    }
    Ok(gen.spawns)
}

/// A forced room's tile-centre fallback (reference `_tile_center`).
fn tile_center(cell: &MazeCell) -> (f64, f64, f64, u32) {
    let half = cell.tile_size / 2.0;
    (half, half, 0.0, 0)
}

/// A world-unit quantity as the Fixed32 (x256) integer the curve math uses.
fn fixed32(value: f64) -> i64 {
    (value * 256.0).round() as i64
}

fn parse_i64(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok()
}

fn parse_f64(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok()
}
