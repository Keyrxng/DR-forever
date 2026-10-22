// SPDX-License-Identifier: GPL-3.0-or-later

//! Experience and leveling: the client's own curve and per-kill math, mirrored.
//!
//! The vanilla client is **authoritative for XP**: on a kill it self-awards
//! locally (`Player::doEvent` case `0x13` -> `Player::onApplyExperience`, which
//! re-issues the award as an internal `0x14` event to the Avatar ->
//! `Hero::onAddExperience`) and never sends the server an XP report over the
//! game protocol. The server therefore does not *grant* XP to the client; it
//! mirrors the same arithmetic to keep its persisted level in lockstep, and
//! re-ships the level/XP on the avatar `0x02` init (zone transfer) so the
//! re-sent avatar never clobbers the client's locally-earned progress.
//!
//! # The client's per-kill award
//!
//! `StockUnit::onDead` computes the award base from the creature's own
//! descriptor, not from a flat constant:
//!
//! ```text
//! desc = getStockUnitDesc(this)
//! if desc != null
//!    && desc[+0xC4] != 0 && != 2 && != 1000          // FactionID
//!    && ((desc[+0x23C] == -0x100 || == 0)            // Lifespan
//!        || unit[+0x33C] != 0)                       // live lifespan counter
//!    && (desc[+0x13C] & 2) != 0:                     // IsAlive
//!      base = desc[+0x104]                           // UnitDesc::Difficulty (Fixed32)
//!      base *= desc[+0x1D8]                          // ExperienceValueMult (Fixed32)
//!      base *= archetype[+0xB0]                      // RPGSettings::ExperienceMod (Fixed32)
//!      if base > 0: fire event 0x13 (base, unit_level)
//! ```
//!
//! The descriptor defaults come from the client constructors: `UnitDesc` sets
//! `+0x104` to `0x100` (1.0), `+0xC4` to 0, and `+0x13C` bit 1 (IsAlive) to 1;
//! `StockUnitDesc` sets `+0x1D8` to `0x100` (1.0) and `+0x23C` to 0.
//!
//! Every `Fixed32` multiply is the client's `x * y >> 8`, so the base is
//! **three** Fixed32 values composed with two truncating shifts.
//!
//! `archetype[+0xB0]` is the global `RPGSettings::ExperienceMod`. It defaults
//! to `0x100` (1.0), while `game.pkg`'s `GlobalKnobs` authors
//! `ExperienceMod = 5.0`, i.e. `0x500 = 1280`. That value is
//! [`GLOBAL_XP_SCALAR`] / [`BASE_XP`].
//!
//! `Hero::onAddExperience` then applies:
//!
//! ```text
//! if unit_level <= player_level - 5: return              // 5+ below: no XP
//! effective = min(unit_level, player_level)
//! ratio     = (effective << 8) << 8 / (player_level << 8)   // Fixed32 divide
//! product   = (base * ratio) >> 8                            // Fixed32 multiply
//! weighted  = (product * 100) >> 8
//! gain      = weighted * ExperienceMod% / 100                // hero +0x2B8
//! ```
//!
//! Both the ratio division and the two multiplies truncate, so the order of
//! operations below is intentional: a continuous `base * eff / player` gives a
//! different unit at some levels (for example, an L1 mob vs an L5 player is 99,
//! not 100).
//!
//! # Grounding
//!
//! The level curve is the `Tables.Experience` curve and the byte-exact port of
//! `HeroDesc::getRequiredExp`:
//!
//! * The curve keyframes are `(L2,10) (L3,25) (L4,45) (L5,65) (L100,5000)`,
//!   with `threshold(L) = curve(L) * 100`.
//! * The parity award is **500 XP** for a 1.0x1.0 creature.
//! * The same curve and arithmetic are used by the GPLv3-licensed
//!   `dr_pyserver` reference implementation.
//!
//! The parity 500 is not a flat per-kill constant: it is
//! `1.0 (Difficulty) x 1.0 (ExperienceValueMult) x 5.0 (GlobalKnobs
//! ExperienceMod)`, put through the client's Fixed32 truncations.

use std::collections::HashSet;

use crate::corpus::{ContentError, Corpus};
use crate::gc::Node;

/// The highest level the client will advance to (`Hero` archetype `+0xbc`).
pub const MAX_LEVEL: u32 = 100;

/// The `GetClientThreshold` multiplier (binary `imul ..., 0x64`).
pub const XP_MULTIPLIER: u32 = 100;

/// The `Tables.Experience` keyframes: `(level, "# of 1.0 monsters at your
/// level")`. An exact match to the curve read from live client memory and to
/// `player_state._XP_CURVE`.
pub const XP_CURVE: [(u32, u32); 5] = [(2, 10), (3, 25), (4, 45), (5, 65), (100, 5000)];

/// One in the client's Fixed32 (Q24.8) representation: `0x100` == 1.0.
pub const FIXED32_ONE: i32 = 0x100;

/// The client's global XP scalar, `RPGSettings::ExperienceMod` at
/// `archetype[+0xB0]`.
///
/// `GlobalKnobs` (which extends `RPGSettings`) authors `ExperienceMod = 5.0`
/// in `game.pkg`; `Fixed32(5.0) = 5 << 8 = 0x500 = 1280`. A 1.0x1.0 creature
/// therefore starts from 5.0 fixed, which the level-ratio and `/256` steps
/// turn into the measured 500 XP at parity.
pub const GLOBAL_XP_SCALAR: i32 = 0x500;

/// The per-kill base XP field (`0x500` = 1280), kept under its historical
/// name for callers that re-export it. See [`GLOBAL_XP_SCALAR`].
pub const BASE_XP: u32 = GLOBAL_XP_SCALAR as u32;

/// The default `ExperienceMod` percentage (`hero +0x2b8`), 100 for a
/// non-freeloader. Free players ride 85 in the reference (`-15%`).
pub const EXPERIENCE_MOD_PERCENT_DEFAULT: u32 = 100;

/// Historical alias for [`EXPERIENCE_MOD_PERCENT_DEFAULT`].
pub const EXPERIENCE_MOD_DEFAULT: u32 = EXPERIENCE_MOD_PERCENT_DEFAULT;

/// The measured XP a 1.0x1.0 same-level kill grants (`BASE_XP x 100 / 256`).
pub const XP_PER_KILL_AT_PARITY: u32 = BASE_XP * EXPERIENCE_MOD_DEFAULT / 256;

/// Attribute points granted per level (`avatar +0x33c += 5`).
pub const ATTRIBUTE_POINTS_PER_LEVEL: u32 = 5;

/// The client's per-creature inputs to the kill-XP gate and base.
///
/// These are the descriptor fields `StockUnit::onDead` reads plus the one live
/// unit-state word its `Lifespan` gate consults. Values are raw Fixed32
/// (Q24.8) where the client stores Fixed32, so a caller that reads content
/// must not convert them back to floats before calling
/// [`xp_per_kill_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatureXpInfo {
    /// `UnitDesc::Difficulty` (`desc +0x104`), Fixed32.
    pub difficulty: i32,
    /// `StockUnitDesc::ExperienceValueMult` (`desc +0x1D8`), Fixed32.
    pub experience_value_mult: i32,
    /// `UnitDesc::FactionID` (`desc +0xC4`).
    pub faction_id: i32,
    /// `StockUnitDesc::Lifespan` (`desc +0x23C`), Fixed32.
    pub lifespan: i32,
    /// `UnitDesc::IsAlive` (`desc +0x13C` bit 1).
    pub is_alive: bool,
    /// The live unit's lifespan counter (`unit +0x33C`). `onDead` only checks
    /// it for zero when the descriptor authors a non-zero, non-infinite
    /// lifespan. A live player kill happens before the timer expires, so
    /// [`creature_xp_info`] sets this to a non-zero value whenever `Lifespan`
    /// is authored.
    pub unit_lifespan: i16,
}

impl Default for CreatureXpInfo {
    /// A plain, XP-eligible stock creature: 1.0 Difficulty, 1.0
    /// ExperienceValueMult, `FactionID = 1`, no lifespan, alive.
    ///
    /// This is the shape [`xp_per_kill`] assumes. Creature-specific callers
    /// should use [`creature_xp_info`] / [`xp_per_kill_with`] instead.
    fn default() -> Self {
        CreatureXpInfo {
            difficulty: FIXED32_ONE,
            experience_value_mult: FIXED32_ONE,
            faction_id: 1,
            lifespan: 0,
            is_alive: true,
            unit_lifespan: 0,
        }
    }
}

impl CreatureXpInfo {
    /// The client's entire `onDead` eligibility gate, in the same order.
    pub fn awards_experience(&self) -> bool {
        let faction_ok = self.faction_id != 0 && self.faction_id != 2 && self.faction_id != 1000;
        let lifespan_ok = self.lifespan == -0x100 || self.lifespan == 0 || self.unit_lifespan != 0;
        faction_ok && lifespan_ok && self.is_alive
    }

    /// The event `0x13` payload `base`, in Fixed32, exactly as `onDead`
    /// composes it: `((Difficulty * ExperienceValueMult) >> 8) * GlobalXP >>
    /// 8`. Zero when the descriptor gate rejects the unit or the base is not
    /// positive.
    pub fn base_xp_fixed32(&self) -> i64 {
        if !self.awards_experience() {
            return 0;
        }
        let scaled = (i64::from(self.difficulty) * i64::from(self.experience_value_mult)) >> 8;
        let scaled = scaled as i32;
        let base = (i64::from(scaled) * i64::from(GLOBAL_XP_SCALAR)) >> 8;
        base.max(0)
    }
}

/// Read a spawn FQN's `UnitDesc`/`StockUnitDesc` fields from `game.pkg`.
///
/// `classname` is the entity asset the spawn names (`world.dungeon00.mob.
/// melee01.rank1`), not necessarily the concrete `creatures.*` row: the walk
/// follows its `extends` chain exactly as the client instantiates it. Fields
/// are first-seen (the leaf overrides its parents), and missing fields keep
/// the client constructor defaults (`Difficulty`/`ExperienceValueMult` = 1.0,
/// `FactionID` = 0, `Lifespan` = 0, `IsAlive` = true).
///
/// `unit_lifespan` is set non-zero when the descriptor authors a lifespan,
/// because a player kill occurs while the spawn's timer is still running.
pub fn creature_xp_info(
    corpus: &mut Corpus,
    classname: &str,
) -> Result<CreatureXpInfo, ContentError> {
    let classname = classname.trim();
    if classname.is_empty() {
        return Err(ContentError::NotFound("an empty spawn classname".into()));
    }
    let mut difficulty = None;
    let mut experience_value_mult = None;
    let mut faction_id = None;
    let mut lifespan = None;
    let mut is_alive = None;
    let mut seen = HashSet::new();
    let mut current = classname.to_string();
    for _ in 0..32 {
        if current.is_empty() || !seen.insert(current.to_ascii_lowercase()) {
            break;
        }
        let Ok(node) = corpus.block_chain(&current) else {
            break;
        };
        if difficulty.is_none() {
            difficulty = descriptor_prop(&node, "Difficulty").map(parse_fixed32);
        }
        if experience_value_mult.is_none() {
            experience_value_mult =
                descriptor_prop(&node, "ExperienceValueMult").map(parse_fixed32);
        }
        if faction_id.is_none() {
            faction_id = descriptor_prop(&node, "FactionID")
                .and_then(|value| value.trim().parse::<i32>().ok());
        }
        if lifespan.is_none() {
            lifespan = descriptor_prop(&node, "Lifespan").map(parse_fixed32);
        }
        if is_alive.is_none() {
            is_alive = descriptor_prop(&node, "IsAlive").and_then(parse_bool);
        }
        current = node.extends.clone().unwrap_or_default();
    }
    let mut info = CreatureXpInfo {
        difficulty: difficulty.unwrap_or(FIXED32_ONE),
        experience_value_mult: experience_value_mult.unwrap_or(FIXED32_ONE),
        faction_id: faction_id.unwrap_or(0),
        lifespan: lifespan.unwrap_or(0),
        is_alive: is_alive.unwrap_or(true),
        unit_lifespan: 0,
    };
    if info.lifespan != 0 {
        info.unit_lifespan = 1;
    }
    Ok(info)
}

/// A property on the descriptor block itself or in its `Description` child
/// (the client's `UnitDesc` composition puts these fields there).
fn descriptor_prop<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    if let Some(value) = node.prop(name) {
        return Some(value);
    }
    node.children
        .iter()
        .find(|child| child.name.eq_ignore_ascii_case("Description"))
        .and_then(|description| description.prop(name))
}

fn parse_fixed32(value: &str) -> i32 {
    value
        .trim()
        .parse::<f64>()
        .map(|value| (value * 256.0).round() as i32)
        .unwrap_or(FIXED32_ONE)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// The XP required to advance **from** `next_level - 1` **to** `next_level`.
///
/// Port of `HeroDesc::getRequiredExp` (`0x4FAF60`) / C#
/// `PlayerState.GetClientThreshold`: a Fixed8.8 curve lookup with a Fixed16.16
/// interpolation parameter, then `x 100`. Yields L2 = 1000, L3 = 2500,
/// L4 = 4500, L5 = 6500, matching the live reading.
pub fn xp_threshold_for_level(next_level: u32) -> u32 {
    let target = next_level << 8; // Fixed8.8
    let count = XP_CURVE.len();
    for i in 0..count {
        let (level, value) = XP_CURVE[i];
        let level_fixed = level << 8;
        let value_fixed = value << 8;
        if target <= level_fixed {
            if i == 0 {
                return value * XP_MULTIPLIER;
            }
            let (prev_level, prev_value) = XP_CURVE[i - 1];
            let prev_level_fixed = prev_level << 8;
            let prev_value_fixed = prev_value << 8;
            let delta = value_fixed - prev_value_fixed;
            // Fixed16.16 interpolation parameter (truncating integer division).
            // The product needs 64 bits: the (L5..L100) segment alone reaches
            // ~8.3e10, past u32.
            let t = (u64::from(target - prev_level_fixed) << 16)
                / u64::from(level_fixed - prev_level_fixed);
            let interp = u64::from(prev_value_fixed) + ((u64::from(delta) * t) >> 16);
            return ((interp >> 8) as u32) * XP_MULTIPLIER;
        }
    }
    XP_CURVE[count - 1].1 * XP_MULTIPLIER
}

/// The XP a kill of `monster_level` grants a `player_level` character under
/// `experience_mod` percent (100 = normal) for a plain 1.0x1.0 creature.
///
/// This is the compatibility form of [`xp_per_kill_with`] using
/// [`CreatureXpInfo::default`]. Creature-specific callers (the server kill
/// path) must pass the spawn's descriptor data instead.
pub fn xp_per_kill(monster_level: u32, player_level: u32, experience_mod: u32) -> u32 {
    xp_per_kill_with(
        &CreatureXpInfo::default(),
        monster_level,
        player_level,
        experience_mod,
    )
}

/// The XP a kill of `monster_level` grants a `player_level` character under
/// `experience_mod` percent, for the creature named by `creature`.
///
/// Port of the client's `StockUnit::onDead` plus
/// `Hero::onAddExperience` (`256813-256915` / `0x4F8392-0x4F842D`):
///
/// ```text
/// if unit_level <= player_level - 5: 0
/// effective = min(unit_level, player_level)
/// ratio     = ((effective << 8) << 8) / (player_level << 8)
/// product   = (base * ratio) >> 8
/// weighted  = (product * 100) >> 8
/// gain      = weighted * experience_mod / 100
/// ```
///
/// Every shift is an integer truncation; the client does not round. The
/// result is **not** clamped to 1, because the client does not clamp it
/// either.
pub fn xp_per_kill_with(
    creature: &CreatureXpInfo,
    monster_level: u32,
    player_level: u32,
    experience_mod: u32,
) -> u32 {
    if player_level == 0 {
        return 0;
    }
    let base = creature.base_xp_fixed32();
    if base <= 0 {
        return 0;
    }
    let mob = i64::from(monster_level);
    let player = i64::from(player_level);
    if mob + 5 <= player {
        return 0;
    }
    let effective = mob.min(player);
    let ratio = (effective << 16) / (player << 8);
    let product = (base * ratio) >> 8;
    let weighted = (product * 100) >> 8;
    let gain = weighted * i64::from(experience_mod) / 100;
    u32::try_from(gain.max(0)).unwrap_or(u32::MAX)
}

/// The result of folding a gain into a character's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XpProgress {
    /// The level after the award.
    pub level: u32,
    /// The progress-into-level after the award (`avatar +0x320`).
    pub experience: u32,
    /// How many levels the award crossed (0 when no level-up).
    pub levels_gained: u32,
}

/// Add `gained_xp` to `(level, experience)` and level up, carrying the
/// remainder.
///
/// Port of `Hero::onAddExperience` (`0x4F82F0`) / C# `PlayerState.AddExperience`:
/// each threshold is **deducted** from the total and the carry rolls into the
/// next level, up to [`MAX_LEVEL`]. The caller grants
/// [`ATTRIBUTE_POINTS_PER_LEVEL`] points per `levels_gained` and refills vitals
/// (the client refills HP/MP to the new max on every level-up).
pub fn apply_xp(level: u32, experience: u32, gained_xp: u32) -> XpProgress {
    let mut level = level.clamp(1, MAX_LEVEL);
    let mut experience = experience.saturating_add(gained_xp);
    let start = level;
    while level < MAX_LEVEL {
        let needed = xp_threshold_for_level(level + 1);
        if experience < needed {
            break;
        }
        experience -= needed;
        level += 1;
    }
    XpProgress {
        level,
        experience,
        levels_gained: level - start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn fixed(value: f64) -> i32 {
        (value * 256.0).round() as i32
    }

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

    fn corpus() -> Option<Corpus> {
        let (pki, pkg) = pkg_paths()?;
        let archive = crate::pkg::Archive::open(&pki, &pkg).expect("open game.pki/game.pkg");
        Some(Corpus::new(archive))
    }

    #[test]
    fn thresholds_match_the_live_reading() {
        // bible §14.2 / player_state: L2=1000, L3=2500, L4=4500, L5=6500.
        assert_eq!(xp_threshold_for_level(2), 1000);
        assert_eq!(xp_threshold_for_level(3), 2500);
        assert_eq!(xp_threshold_for_level(4), 4500);
        assert_eq!(xp_threshold_for_level(5), 6500);
        // The tail keyframe: 5000 monsters x 100.
        assert_eq!(xp_threshold_for_level(100), 500_000);
        // Monotonic across the whole curve.
        let mut prev = 0;
        for level in 2..=MAX_LEVEL {
            let t = xp_threshold_for_level(level);
            assert!(t > prev, "threshold must climb at L{level}: {t} <= {prev}");
            prev = t;
        }
    }

    #[test]
    fn per_kill_gain_is_500_at_parity() {
        // The measured live value: player L1 killing an L2 mob -> 500.
        assert_eq!(xp_per_kill(2, 1, EXPERIENCE_MOD_DEFAULT), 500);
        assert_eq!(xp_per_kill(1, 1, EXPERIENCE_MOD_DEFAULT), 500);
        // Effective level is capped at the player, so over-levelled mobs do not
        // grant more than parity.
        assert_eq!(xp_per_kill(50, 1, EXPERIENCE_MOD_DEFAULT), 500);
        // A monster exactly five levels below grants nothing.
        assert_eq!(xp_per_kill(5, 10, EXPERIENCE_MOD_DEFAULT), 0);
        // One level inside the window still grants.
        assert_eq!(xp_per_kill(6, 10, EXPERIENCE_MOD_DEFAULT), 298);
        // Under-levelled kills scale by min/player.
        assert_eq!(xp_per_kill(1, 2, EXPERIENCE_MOD_DEFAULT), 250);
        // The ExperienceMod lever scales the award without touching the curve.
        assert_eq!(xp_per_kill(1, 1, 85), 425);
    }

    #[test]
    fn truncation_order_matches_the_client() {
        // `((base * ratio) >> 8 * 100) >> 8`: a continuous base*eff/player
        // would give 100, but the client's two Fixed32 truncations give 99.
        assert_eq!(xp_per_kill(1, 5, EXPERIENCE_MOD_DEFAULT), 99);
        // A 2.0-Difficulty creature against an L3 player: 332, not 333.
        let veteran = CreatureXpInfo {
            difficulty: fixed(2.0),
            ..CreatureXpInfo::default()
        };
        assert_eq!(xp_per_kill_with(&veteran, 1, 3, 100), 332);
    }

    #[test]
    fn descriptor_gates_can_zero_the_award() {
        let parity = |info: &CreatureXpInfo| xp_per_kill_with(info, 1, 1, 100);
        // FactionID 0, 2 (henchmen) and 1000 (NPCs) are all excluded.
        for faction_id in [0, 2, 1000] {
            let info = CreatureXpInfo {
                faction_id,
                ..CreatureXpInfo::default()
            };
            assert_eq!(parity(&info), 0, "faction {faction_id}");
        }
        // Dead units award nothing.
        let dead = CreatureXpInfo {
            is_alive: false,
            ..CreatureXpInfo::default()
        };
        assert_eq!(parity(&dead), 0);
        // An authored lifespan with an expired/zero unit counter is excluded.
        let expired = CreatureXpInfo {
            lifespan: fixed(30.0),
            unit_lifespan: 0,
            ..CreatureXpInfo::default()
        };
        assert_eq!(parity(&expired), 0);
        // Infinite (-1.0) and absent lifespans pass without a unit counter.
        for lifespan in [0, -0x100] {
            let info = CreatureXpInfo {
                lifespan,
                unit_lifespan: 0,
                ..CreatureXpInfo::default()
            };
            assert_eq!(parity(&info), 500, "lifespan {lifespan}");
        }
        // Zero Difficulty or a zero ExperienceValueMult makes the base
        // non-positive, so no event fires.
        for info in [
            CreatureXpInfo {
                difficulty: 0,
                ..CreatureXpInfo::default()
            },
            CreatureXpInfo {
                experience_value_mult: 0,
                ..CreatureXpInfo::default()
            },
        ] {
            assert_eq!(parity(&info), 0);
        }
    }

    #[test]
    fn non_unit_multipliers_scale_the_base() {
        // 2.0 Difficulty x 1.5 ExperienceValueMult x 5.0 global = 15.0 fixed.
        let info = CreatureXpInfo {
            difficulty: fixed(2.0),
            experience_value_mult: fixed(1.5),
            ..CreatureXpInfo::default()
        };
        assert_eq!(info.base_xp_fixed32(), fixed(15.0) as i64);
        assert_eq!(xp_per_kill_with(&info, 1, 1, 100), 1500);
        // 7.0 Difficulty at parity is 3500 XP.
        let hard = CreatureXpInfo {
            difficulty: fixed(7.0),
            ..CreatureXpInfo::default()
        };
        assert_eq!(xp_per_kill_with(&hard, 1, 1, 100), 3500);
    }

    #[test]
    fn content_creature_descriptors_match_the_client() {
        let Some(mut corpus) = corpus() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        // melee01.rank1 extends Warg Pup -> UnitMelee_GruntRecruit (1.0).
        let pup = creature_xp_info(&mut corpus, "world.dungeon00.mob.melee01.rank1")
            .expect("melee01 descriptor");
        assert_eq!(pup.difficulty, fixed(1.0));
        assert_eq!(pup.experience_value_mult, fixed(1.0));
        assert_eq!(pup.faction_id, 1);
        assert_eq!(pup.lifespan, 0);
        assert!(pup.is_alive);
        assert_eq!(xp_per_kill_with(&pup, 2, 1, 100), 500);

        // melee02.rank1 extends Warg Grunt -> UnitMelee_GruntVeteran (2.0).
        let wolf = creature_xp_info(&mut corpus, "world.dungeon00.mob.melee02.rank1")
            .expect("melee02 descriptor");
        assert_eq!(wolf.difficulty, fixed(2.0));
        assert_eq!(wolf.experience_value_mult, fixed(1.0));
        assert_eq!(xp_per_kill_with(&wolf, 1, 1, 100), 1000);

        // Candy Golem authors Difficulty 7.0 and Lifespan 45; a live kill
        // still awards (the timer is running), and its base is 35.0 fixed.
        let candy = creature_xp_info(&mut corpus, "world.dungeon08.mob.CandyMonster")
            .expect("CandyMonster descriptor");
        assert_eq!(candy.difficulty, fixed(7.0));
        assert_eq!(candy.lifespan, fixed(45.0));
        assert_ne!(candy.unit_lifespan, 0);
        assert_eq!(candy.faction_id, 1);
        assert_eq!(xp_per_kill_with(&candy, 1, 1, 100), 3500);

        // A player-summoned unit is FactionID 2 and ExperienceValueMult 0.
        let summon = creature_xp_info(&mut corpus, "creatures.summon.snowman.base.Snowman_Summon")
            .expect("Snowman_Summon descriptor");
        assert_eq!(summon.faction_id, 2);
        assert_eq!(summon.experience_value_mult, 0);
        assert_eq!(xp_per_kill_with(&summon, 1, 1, 100), 0);
    }

    #[test]
    fn apply_carries_the_remainder_and_levels() {
        // 500 XP is under the L2 threshold of 1000: no level.
        let p = apply_xp(1, 0, 500);
        assert_eq!(
            p,
            XpProgress {
                level: 1,
                experience: 500,
                levels_gained: 0
            }
        );
        // 1000 XP crosses L2 exactly, leaving 0 carry.
        let p = apply_xp(1, 0, 1000);
        assert_eq!(
            p,
            XpProgress {
                level: 2,
                experience: 0,
                levels_gained: 1
            }
        );
        // A single award may cross several levels, deducting each threshold.
        // 1000 (L2) + 2500 (L3) = 3500 gets to L3 with 0 carry.
        let p = apply_xp(1, 0, 3500);
        assert_eq!(
            p,
            XpProgress {
                level: 3,
                experience: 0,
                levels_gained: 2
            }
        );
        // Carry rolls forward: 1000 + 100 leaves 100 into L2.
        let p = apply_xp(1, 0, 1100);
        assert_eq!(
            p,
            XpProgress {
                level: 2,
                experience: 100,
                levels_gained: 1
            }
        );
    }

    #[test]
    fn max_level_is_a_ceiling() {
        // A huge award stops at MAX_LEVEL; the excess is not silently lost into
        // a level that cannot exist.
        let p = apply_xp(MAX_LEVEL, 0, u32::MAX);
        assert_eq!(p.level, MAX_LEVEL);
        assert_eq!(p.levels_gained, 0);
    }
}
