// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-content`: the game content model, read from `game.pkg`.
//!
//! This crate reads the PKI/PKG archive, parses the GC configuration language
//! (`*.gc`), and derives the per-class starting kit used to build character
//! graphs and in-world avatar records.
//!
//! The archive is the only content source. Nothing here reads a network
//! capture, an extracted asset tree, or a hand-written table. Every derived
//! value must be stated by `game.pkg`; a derivation that cannot be settled is
//! an error, never a placeholder.

pub mod bling_gnome;
pub mod class;
pub mod corpus;
pub mod economy;
pub mod encounter;
pub mod experience;
pub mod gc;
pub mod hash;
pub mod item;
pub mod loot;
pub mod manipulator;
pub mod maze;
pub mod merchant;
pub mod pkg;
pub mod quest;
pub mod trainer;
pub mod zone;

pub use bling_gnome::{
    bling_gnome_profile, BlingGnomeProfile, BLING_GNOME_SKILL, BLING_GNOME_SUMMON,
};
pub use class::{
    derive_avatar_inventory_roster, derive_class_fqns, derive_class_items, derive_class_kit,
    derive_class_skill_elements, derive_class_visuals, derive_npc_base_manipulator, ClassFqn,
    ClassItem, ClassKit, ClassSkillWire, InventoryEntry, NpcManipulator, SkillElements, WireSkill,
    AVATAR_BASE_AVATAR_GC, AVATAR_INVENTORY_ID_BASE, AVATAR_MANIPULATOR_SKILL_DFC, CLASS_BASES,
    CLASS_SEXES,
};
pub use corpus::{ContentError, Corpus, ListEntry};
pub use economy::{
    best_modifier_quality, buy_price, item_get_value, item_level_delta, knobs, merchant_profile,
    modifier_quality, price, reconcile, sell_price, Knobs, MerchantProfile, Quote, QUALITY_COUNT,
    QUALITY_VOUCHER,
};
pub use encounter::{
    encounter_groups, mob_profile_at_level, spawns, spawns_with_seed, spawns_with_seed_reachable,
    EncounterUnit, MobSpawn, MAX_MOBS_PER_LEVEL,
};
pub use experience::{
    apply_xp, xp_per_kill, xp_threshold_for_level, XpProgress, ATTRIBUTE_POINTS_PER_LEVEL, BASE_XP,
    EXPERIENCE_MOD_DEFAULT, MAX_LEVEL, XP_CURVE, XP_MULTIPLIER, XP_PER_KILL_AT_PARITY,
};
pub use gc::Node;
pub use hash::dfc_hash;
pub use item::{
    author_item_record_by_class, author_item_record_with_mods, derive_bag_geometry, fixed32_q8,
    item_profile, quality_index, BagGrid, ItemProfile, ItemRecord, AVATAR_BAG_FQN,
    ITEM_WIRE_FIXED_LEN, ITEM_WIRE_FLAG_NO_SELL, ITEM_WIRE_FLAG_SOULBOUND,
    ITEM_WIRE_FLAG_SOULBOUND_WORD, QUALITY_DEFAULT, QUALITY_NAMES, TYPE_TAG_CLASSNAME,
};
pub use loot::{
    generate_loot, generate_slots, generate_slots_scaled, treasure_slots, LootItem, LootSlot,
    ModifierRoll,
};
pub use manipulator::{
    creature_manipulators, mob_behavior_class, CreatureManipulator, ManipulatorKind,
    CREATURE_BEHAVIOR_CASTER, CREATURE_BEHAVIOR_MELEE, CREATURE_BEHAVIOR_RANGED,
    DEFAULT_CREATURE_MELEE_GC, DEFAULT_CREATURE_MELEE_ID,
};
pub use maze::{
    crc32, entry_portal_position, entry_position, generate_layout, layout_seed, minimap_for_layout,
    resolve_entry, room_waypoint_position_with_seed, set_layout_seed_override,
    tile_encounter_markers, tile_world_entities, AuthoredAnchor, EntryPosition, MazeCell,
    MazeEntry, MazeLayout, MazeMinimap, MersenneTwister, PlacedRoomNode, TileEncounterMarker,
    TileWorldEntity,
};
pub use merchant::{
    authors_merchant, derive_merchant_tabs, fill_generated_tabs, generate_generator_items,
    generate_loot_items, generate_merchant_items, MerchantItem, MerchantTab,
};
pub use pkg::{Archive, Entry, PkgError};
pub use quest::{
    ActivateDrop, GiverDirection, QuestCorpus, QuestDef, QuestObjective,
    ON_ACCEPT_ITEMS_ARE_NO_SELL, ON_ACCEPT_ITEMS_ARE_SOULBOUND, ON_ACCEPT_ITEM_KEY,
};
pub use trainer::{
    derive_trainer, skill_is_passive, skill_value_per_level, train_cost, trainer_component_class,
    TrainerCatalog, TrainerSkill,
};
pub use zone::{
    checkpoint_class_by_hash, checkpoint_defs, checkpoint_destination, checkpoints_for_zones,
    class_description_property, class_is_creature, class_is_kind_of, class_is_locked,
    class_property, class_unlock_requirement, derive_generated_zone_start,
    derive_generated_zone_start_with_seed, derive_world_entities, derive_zone_npcs,
    derive_zone_spawn, destination_for, generated_return_portal, generated_return_portal_with_seed,
    generated_room_portals, generated_room_world_entities, hand_placed_world_entity_tables,
    nci_item_generators, nci_teleport_destination, on_death_summons, set_generated_pose_override,
    spell_effect_summons, starting_checkpoints, world_entity_table_choice, zone_def,
    zone_is_generated, Destination, GeneratedZoneStart, OnDeathSummon, PlacedEntity, PlacedKind,
    TablePlacement, UnlockRequirement, ZoneDef, ZoneSpawn, GENERATED_RETURN_PORTAL_CLASS,
    GENERATED_ROOM_PORTAL_CLASS, SPAWN_DEFAULT_WAYPOINT_NAME, SPAWN_RUNG_DEFAULT_START,
    SPAWN_RUNG_NAMED_WAYPOINT, SPAWN_RUNG_RESPAWN, SPAWN_RUNG_SPAWN_CLASS,
    SPAWN_RUNG_ZONE_START_ROOM,
};
