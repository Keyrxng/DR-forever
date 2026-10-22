// SPDX-License-Identifier: GPL-3.0-or-later

//! drf-proto: the client-required message bodies, built from the client binary
//! and `game.pkg`.
//!
//! This crate carries the wire shapes the client's own readers consume. It is
//! grounded on the client binary (`DungeonRunners.exe`) and the package; no
//! capture, no observed server, and no hand-written table is a source here.
//!
//! The first module is [`graph`], the character graph (`(04,02)`/`(04,03)`
//! replies and the join `0x08 player` body). Further message families land
//! here as their client readers are transcribed.

pub mod action;
pub mod attack;
pub mod attribute;
pub mod auth;
pub mod avatar;
pub mod cem;
pub mod char;
pub mod create;
pub mod entity_init;
pub mod gnome;
pub mod graph;
pub mod item;
pub mod join;
pub mod merchant;
pub mod mob;
pub mod mover;
pub mod player;
pub mod quest;

pub use action::{
    action_frame, action_sub_op, attack_target2_payload, die_payload, fixed_payload_len,
    follow_payload, is_registered_action_class, kill_payload, ressurect_payload, ActionError,
    ACTION_UPDATE_TYPE, ACTIVATE_ACTION_CLASS, AMBUSH_ACTION_CLASS, ATTACK_TARGET2_ACTION_CLASS,
    CONVERT_ITEMS_TO_GOLD_ACTION_CLASS, DIE_ACTION_CLASS, DODGE_ACTION_CLASS,
    DO_EFFECT_ACTION_CLASS, FACE_TARGET_ACTION_CLASS, FLEE_ACTION_CLASS, FOLLOW_ACTION_CLASS,
    HIDE_ACTION_CLASS, IDLE_ACTION_CLASS, IMMOBILIZE_ACTION_CLASS, KILL_ACTION_CLASS,
    KNOCKBACK_ACTION_CLASS, KNOCKDOWN_ACTION_CLASS, MOVE_IN_DIRECTION_ACTION_CLASS,
    MOVE_TO_ACTION_CLASS, PLAY_ANIMATION_ACTION_CLASS, REMOVE_ACTION_CLASS, RESSURECT_ACTION_CLASS,
    RETRIEVE_ITEM_ACTION_CLASS, SEARCH_FOR_ATTACK_ACTION_CLASS, SET_BLOCKING_ACTION_CLASS,
    SPAWN_ACTION_CLASS, SPAWN_ANIMATION_ACTION_CLASS, STUN_ACTION_CLASS, TURN_ACTION_CLASS,
    UNSPAWN_ACTION_CLASS, USE_ACTION_CLASS, USE_ITEM_ACTION_CLASS, USE_ITEM_POSITION_ACTION_CLASS,
    USE_ITEM_TARGET_ACTION_CLASS, USE_POSITION_ACTION_CLASS, USE_TARGET_ACTION_CLASS,
    WAIT_ACTION_CLASS, WANDER_ACTION_CLASS, WARP_TO_ACTION_CLASS,
};
pub use attack::{
    attack_ack, parse_attack_use, parse_position_cast, parse_self_cast, position_cast_ack,
    self_cast_ack, AttackUse, PositionCast, SelfCast, ATTACK_ACK_FOOT_LEN, ATTACK_SUB_SELF_CAST,
    ATTACK_SUB_USE, ATTACK_SUB_USE_POSITION,
};
pub use attribute::{
    build_hero_stat_update, parse_attribute_request, AttributeRequest, ENTITY_REQUEST_TYPE,
    REQ_RESPEC, REQ_RETURN_ATTRIB, REQ_SPEND_ATTRIB, STAT_AGILITY, STAT_COUNT, STAT_POWER,
    STAT_STRENGTH, STAT_TOUGHNESS, SYNCH_FLAG_HP,
};
pub use auth::{
    decode_login, decrypt_packet, encrypt_packet, frame, login_block, unframe, AboutToPlay,
    AuthBody, AuthError, Blowfish, Des, LoginOk, PlayOk, BF_KEY, DES_KEY, LOGIN_PASSWORD_LEN,
    LOGIN_USERNAME_LEN, TYPE_ABOUT_TO_PLAY, TYPE_LOGIN, TYPE_LOGIN_OK, TYPE_PLAY_OK,
    TYPE_PROTOCOL_VER, TYPE_SERVER_LIST, TYPE_SERVER_LIST_EXT,
};
pub use avatar::{
    avatar_hp_wire, avatar_hp_wire_at_level, avatar_level_one_hp_wire,
    avatar_place_at_start_update, build_add_experience_update, build_avatar_skills_payload,
    build_minimal_avatar_init_payload, build_minimal_avatar_init_payload_with_progress,
    build_minimal_avatar_init_payload_with_stats, build_skills_points_update,
    build_skills_skill_name_update, build_skills_skill_update, minimal_avatar_equipment_component,
    minimal_avatar_equipment_component_with_entries, minimal_avatar_manipulators_component,
    minimal_avatar_manipulators_component_with_entries, minimal_avatar_modifiers_component,
    minimal_avatar_skills_component, minimal_avatar_skills_component_with_skills,
    minimal_avatar_unit_behavior_component, minimal_avatar_unit_container_component,
    minimal_avatar_unit_container_component_with_gold, skill_manipulator_entry,
    skill_manipulator_update_entry, AvatarError, AVATAR_CONTAINER_PREFIX_U32,
    AVATAR_CONTAINER_TRAILER, AVATAR_INVENTORY_COUNT_BYTE, AVATAR_LEVEL_ONE_HP_WIRE,
    AVATAR_MANIPULATORS_CLASS, AVATAR_MODIFIERS_CLASS, AVATAR_SKILLS_CLASS,
    AVATAR_UNIT_BEHAVIOR_CLASS, AVATAR_UNIT_CONTAINER_CLASS, AVATAR_UPDATE_ADD_EXPERIENCE,
    AVATAR_UPDATE_PLACE_AT_START, AVATAR_WE_FLAGS_WARP_BIT, HP_WIRE_PER_LEVEL, MINIMAL_AVATAR_HP,
    MINIMAL_AVATAR_HP_UNSCALED, MINIMAL_AVATAR_MP, MINIMAL_AVATAR_WE_FLAGS_ATTACH,
    MINIMAL_AVATAR_WE_FLAGS_WARP, MINIMAL_LINKED_AVATAR_HEAD_LEN,
    UNIT_BEHAVIOR_FOLLOW_GRANT_PAYLOAD_OFF, UNIT_BEHAVIOR_NEUTRAL_PAYLOAD,
};
pub use cem::{
    frame_inner, frame_inner_with_end, CemError, CEM_CHANNEL, CEM_CREATE, CEM_CREATE_INIT,
    CEM_ENTITY_INIT, CEM_ENTITY_UPDATE, CEM_INIT_SUB_ENTITIES, CEM_INTERVAL_CONTAINER, CEM_REMOVE,
    CEM_STREAM_END, CONNECTED_CASE, CONNECTED_MARKER, END_MARKER, FLAG_NONUNIT, FLAG_UNIT,
    JOIN_END_MARKER, JOIN_RECORD_HEADER, TYPE_TAG_CLASSNAME, TYPE_TAG_INVALID, TYPE_TAG_NONE,
    TYPE_TAG_U16, TYPE_TAG_U32, TYPE_TAG_U8,
};
pub use char::{
    delete_ack_payload, delete_payload, group_connected_0930_payload, rosters_0301_payload,
    select_ack_payload, select_payload, user_changed_group_0935_payload, ROSTERS_0301,
};
pub use create::{
    acquire_payload, build_acquire_notice, build_item_object_create_init,
    build_town_portal_create_init, create_body, interval_payload, interval_stream,
    is_consumable_class, item_create_body, item_object_init_payload, minimal_entity_stream,
    parse_acquire_payload, parse_create_body, parse_item_create_body, parse_remove_inner,
    remove_frame, remove_inner, AcquirePayload, CreateBody, CreateError, DroppedItemBody,
    ItemCreateBody, CEM_ACQUIRE_NOTICE, INTERVAL_PATHMANAGER_BLOCK, ITEMOBJECT_CLASS,
    ITEMOBJECT_SPAWN_FLAG, ITEMOBJECT_STATE_DEFAULT, ITEMOBJECT_TIMER_DEFAULT,
    ITEMOBJECT_W1_DEFAULT, ITEMOBJECT_WORLD_ENTITY_FLAGS, TOWN_BUDGET_TRIPLE,
};
pub use drf_content::dfc_hash;
pub use entity_init::{
    build_stock_unit_class_state, build_unit_class_state, build_world_entity_class_state,
    checkpoint_class_state, door_class_state, npc_behavior_class_state, npc_class_state,
    npc_component_record, npc_manipulators_class_state, npc_modifiers_class_state,
    placed_entity_class_state, world_entity_interactive_class_state,
    world_entity_unit_interactive_class_state, zone_portal_class_state, EntityInitError,
    CHECKPOINT_CLASS_STATE_LEN, CHECKPOINT_WORLD_ENTITY_FLAGS, NPC_BEHAVIOR_CLASS,
    NPC_CLASS_STATE_LEN, NPC_HERO_POWER, NPC_MANIPULATORS_CLASS, NPC_MODIFIERS_CLASS, NPC_SCALE,
    NPC_WORLD_ENTITY_FLAGS, PORTAL_DEFAULT_HEIGHT, PORTAL_DEFAULT_WIDTH, PORTAL_WORLD_ENTITY_FLAGS,
    UNIT_FLAGS, WORLD_ENTITY_CLASS_STATE_HEADER_LEN, WORLD_ENTITY_FLAGS_ATTACH,
    WORLD_ENTITY_FLAGS_BASE,
};
pub use gnome::{
    build_component_packet, build_convert_action, build_entity_remove, build_entity_snapshot,
    build_mover_update, build_play_animation, gnome_hp_wire, ANIM_ACTIVE_SKILL, ANIM_DEATH,
    ANIM_FIDGET_FIRST, ANIM_FIDGET_LAST, ANIM_PICKUP, ANIM_SPAWN, GNOME_BEHAVIOR_SUFFIX,
    GNOME_ENTITY_CLASS,
};
pub use graph::{
    avatar_graph_node, created_payload, graph_item_entry, join_player_body, list_payload,
    player_graph_node, Appearance, ClassContent, KeyMint, ListRow, EQUIPMENT_VALID_SLOTS,
    ITEM_ENTRY_LEN, NODE_VERSION,
};
pub use item::{
    build_component_update_stream, build_container_add_currency,
    build_container_add_currency_notify, build_container_clear_active, build_container_item_add,
    build_container_item_quantity, build_container_item_remove, build_container_set_active,
    build_equipment_item_add, build_equipment_item_remove, build_manipulators_add,
    build_manipulators_remove, build_unit_use_item_update, class_starting_equipment_payload,
    class_starting_item_manipulators_bytes, equipment_item_entry, item_manipulator_entry,
    item_manipulator_readstate, readdress_item_record, ItemError, AVATAR_EQUIPMENT_CLASS,
    CONTAINER_CURRENCY_BODY_LEN, CONTAINER_CURRENCY_KEY_UNDECODED,
    CONTAINER_CURRENCY_SOURCE_UNMEASURED_DEFAULT, CONTAINER_SUB_ADD_CURRENCY,
    CONTAINER_SUB_CLEAR_ACTIVE, CONTAINER_SUB_ITEM_ADD, CONTAINER_SUB_ITEM_QUANTITY,
    CONTAINER_SUB_ITEM_REMOVE, CONTAINER_SUB_SET_ACTIVE, ENTITY_SUB_USE_ITEM_UPDATE,
    EQUIPMENT_ITEM_FIXED_LEN, EQUIPMENT_ITEM_FLAGS_PLAIN, EQUIPMENT_ITEM_GRID_BYTES,
    EQUIPMENT_MAX_AUTHORED_SLOTS, EQUIPMENT_MOD_BODY_FLAGS, EQUIPMENT_STARTING_MOD_LEVEL,
    EQUIPMENT_SUB_ITEM_ADD, EQUIPMENT_SUB_ITEM_REMOVE, ITEM_GRID_UNPLACED, MANIPULATORS_SUB_ADD,
    MANIPULATORS_SUB_REMOVE, MERCHANT_BAG_CONTAINER_ID,
};
pub use join::{
    assign_payload, instance_count_payload, join_batch, parse_assign_payload, parse_ready_payload,
    parse_transfer_payload, ready_payload, ready_payload_with_explored, seed_payload,
    transfer_payload, AssignPayload, JoinError, ReadyPayload, ASSIGN_TOWN_TRAILER,
    TRANSFER_REASON_DEFAULT,
};
pub use merchant::{
    build_merchant_component_record, build_stock_payload, derived_item_ids,
    merchant_component_classname, reserve_item_ids, ITEM_READ_DATA_FIXED_LEN,
    MERCHANT_COMPONENT_CLASS_SUFFIX, MERCHANT_CURRENCY, MERCHANT_ITEM_ID_MAX, MERCHANT_ITEM_ID_MIN,
    MERCHANT_NEW_ITEM_COUNT, MERCHANT_NEW_TAB_COUNT, MERCHANT_POST_BYTE, MERCHANT_TABS_OFF,
    MERCHANT_TAB_HAS_DATA, MERCHANT_TAB_HEADER_LEN, MERCHANT_TAIL_LEN,
};
pub use mob::{
    mob_behavior_class_state, mob_class_state, mob_class_state_scaled,
    mob_class_state_with_current_health, mob_component_record, mob_enroll_payload,
    mob_manipulators_class_state, mob_modifiers_class_state, mob_mover_update_payload,
    mob_skills_class_state, mob_spawn_action_payload, UnitScale, MOB_BEHAVIOR_CLASS,
    MOB_CLASS_STATE_LEN, MOB_MANIPULATORS_CLASS, MOB_MODIFIERS_CLASS, MOB_SKILLS_CLASS,
    MOB_WORLD_ENTITY_FLAGS,
};
pub use mover::{
    follow_client_payload, follow_client_payload_wire, hp_wire_u32, last_mover_position, mover_ack,
    mover_ack_wire, owner_move_ack_wire, parse_control_request, parse_mover, placement_payload,
    placement_payload_wire, world_interval_payload, ControlRequest, MoveAck, MoverError,
    MoverStream, MOVE_ACK_MAX_RECORDS, MOVE_RECORD_SIZE, MOVE_SUB, MOVE_TYPE_PLACEMENT,
};
pub use player::{
    component_create_inner, derived_dialog_manager_record, derived_player_manager_components,
    derived_player_manager_components_with_journal,
    derived_player_manager_components_with_journal_and_checkpoints,
    derived_player_manager_components_with_journal_checkpoints_and_recent_portal,
    derived_quest_manager_record, derived_quest_manager_record_with_journal,
    derived_quest_manager_record_with_journal_and_checkpoints,
    derived_quest_manager_record_with_journal_checkpoints_and_recent_portal, join_player_init_body,
    join_player_init_body_with_role, quest_directory_body, real_player_init_payload,
    real_player_init_payload_with_role, JournalRecord, PlayerError, QuestManagerTail,
    PLAYER_CLASSNAME, PLAYER_INIT_TAIL_LEN, PLAYER_MINIMUM_ITEM_QUALITY, PLAYER_ROLE_ADMIN,
    PLAYER_ROLE_PLAYER, PLAYER_TAIL_V3,
};
pub use quest::{
    activate_ack_body, build_quest_add_record, build_quest_available_update, build_quest_finalize,
    build_quest_objective_labels_update, build_quest_objective_update, build_quest_offer_ack,
    build_quest_ready_ack, build_quest_remove, parse_checkpoint_travel, parse_interact_click,
    parse_quest_abandon, parse_quest_accept, parse_quest_checkpoint, parse_quest_query,
    parse_quest_turnin, quest_gc_type, quest_objective, CheckpointTravel, InteractClick,
    QuestQuery, QUEST_PENDING_OBJECTIVE_PLACEHOLDER, QUEST_SUB_ACCEPT, QUEST_SUB_AVAILABLE,
    QUEST_SUB_CLOSE, QUEST_SUB_FINALIZE, QUEST_SUB_OBJECTIVE_UPDATE, QUEST_SUB_QUERY,
    QUEST_SUB_SET_OFFERED, QUEST_SUB_TURNIN_COMPLETE, QUEST_SUB_TURNIN_QUERY,
    QUEST_ZERO_OBJECTIVE_PLACEHOLDER,
};
