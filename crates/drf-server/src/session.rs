// SPDX-License-Identifier: GPL-3.0-or-later

//! One game connection: its state and its message handlers.
//!
//! The client opens the game connection with a raw `0x0A` hello and will not
//! speak until it gets the 23-byte connect-ack; a `0x0A` ping (keepalive) is
//! answered with a bare `(07,06)` tick. After that the read loop carves
//! `drf-wire` [`Segment`]s (c2s frames arrive flagged `0x0E`), inflates each
//! one to a `[channel][mtype][payload]` inner and dispatches. Every response is
//! a `[channel][mtype][payload]` inner re-wrapped in a segment addressed
//! `(0x01, 0x000F)` -- the address the reference's `game_build` writes.
//!
//! The handlers cover the playable surface only: character list `(04,03)`,
//! create `(04,02)`, select `(04,05)` + play burst, delete `(04,04)`, join
//! `(13,6)` (assign/ready/count/seed and the town join batch) and the periodic
//! `(07,06)` tick. The merchant BUY leg (`0x1E`) and quests are handled; the
//! attack `0x50` leg acks and tracks mob HP (with a placeholder damage model);
//! the paired client hook's `StockUnit::onDead` telemetry is the authoritative
//! kill signal that drops one `ItemObject` and sends the CEM remove, and the
//! same leg claims a tracked drop. Merchant SELL is deferred.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, Once, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use drf_content::{
    dfc_hash, train_cost, BagGrid, BlingGnomeProfile, CreatureManipulator, Destination,
    GiverDirection, LootItem, MersenneTwister, ModifierRoll, TrainerCatalog, TrainerSkill,
};
use drf_proto::{
    activate_ack_body, attack_ack, build_component_packet, build_component_update_stream,
    build_container_add_currency_notify, build_container_clear_active, build_container_item_add,
    build_container_item_quantity, build_container_item_remove, build_container_set_active,
    build_convert_action, build_entity_remove, build_entity_snapshot, build_equipment_item_add,
    build_equipment_item_remove, build_hero_stat_update, build_item_object_create_init,
    build_manipulators_add, build_manipulators_remove, build_merchant_component_record,
    build_minimal_avatar_init_payload_with_stats, build_mover_update, build_play_animation,
    build_quest_add_record, build_quest_available_update, build_quest_finalize,
    build_quest_objective_labels_update, build_quest_objective_update, build_quest_offer_ack,
    build_quest_ready_ack, build_quest_remove, build_skills_points_update,
    build_skills_skill_name_update, build_skills_skill_update, build_town_portal_create_init,
    build_unit_use_item_update, create_body, delete_ack_payload, derived_item_ids,
    derived_player_manager_components_with_journal_checkpoints_and_recent_portal,
    equipment_item_entry, follow_client_payload_wire, gnome_hp_wire, group_connected_0930_payload,
    instance_count_payload, item_manipulator_readstate, join_batch,
    join_player_init_body_with_role, last_mover_position, list_payload,
    minimal_avatar_equipment_component, minimal_avatar_equipment_component_with_entries,
    minimal_avatar_manipulators_component, minimal_avatar_manipulators_component_with_entries,
    minimal_avatar_modifiers_component, minimal_avatar_skills_component_with_skills,
    minimal_avatar_unit_behavior_component, minimal_avatar_unit_container_component_with_gold,
    mob_behavior_class_state, mob_class_state_scaled, mob_component_record,
    mob_manipulators_class_state, mob_modifiers_class_state, mob_mover_update_payload,
    mob_skills_class_state, mob_spawn_action_payload, npc_behavior_class_state,
    npc_component_record, npc_manipulators_class_state, npc_modifiers_class_state,
    owner_move_ack_wire, parse_attack_use, parse_attribute_request, parse_checkpoint_travel,
    parse_control_request, parse_interact_click, parse_mover, parse_position_cast,
    parse_quest_abandon, parse_quest_accept, parse_quest_query, parse_quest_turnin,
    parse_self_cast, placed_entity_class_state, position_cast_ack, quest_directory_body,
    quest_gc_type, quest_objective, readdress_item_record, remove_inner, reserve_item_ids,
    rosters_0301_payload, select_ack_payload, self_cast_ack, skill_manipulator_update_entry,
    transfer_payload, user_changed_group_0935_payload, world_interval_payload,
    Appearance as GraphAppearance, AttackUse, AttributeRequest, CheckpointTravel, ClassContent,
    DroppedItemBody, InteractClick, JournalRecord, ListRow, MoveAck, QuestManagerTail, QuestQuery,
    UnitScale, ANIM_ACTIVE_SKILL, ANIM_DEATH, ANIM_FIDGET_FIRST, ANIM_FIDGET_LAST, ANIM_PICKUP,
    EQUIPMENT_STARTING_MOD_LEVEL, JOIN_END_MARKER, MINIMAL_AVATAR_HP, MINIMAL_AVATAR_MP,
    MINIMAL_AVATAR_WE_FLAGS_ATTACH, MOB_MANIPULATORS_CLASS, MOB_MODIFIERS_CLASS, MOB_SKILLS_CLASS,
    NPC_BEHAVIOR_CLASS, NPC_MANIPULATORS_CLASS, NPC_MODIFIERS_CLASS, PLAYER_ROLE_ADMIN,
    PLAYER_ROLE_PLAYER, QUEST_SUB_ACCEPT, QUEST_SUB_TURNIN_COMPLETE, QUEST_SUB_TURNIN_QUERY,
    REQ_SPEND_ATTRIB, TOWN_BUDGET_TRIPLE, TRANSFER_REASON_DEFAULT,
};
use drf_wire::{MessageAddress, Payload, Segment, SegmentError};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::server::Shared;
use crate::store::{
    qid_from_hex, Appearance as StoreAppearance, BagItem, Character, EquipmentItem, HotbarSlot,
    QuestObjective, QuestState,
};
use crate::world::{assign_payload_with_seed, ZONE_TOWN};

/// One placed world entity the session ships: its entity id, class, `0x02`
/// class-state and (for an NPC) the three nested `0x32` component records.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlacedCreate {
    entity_id: u16,
    /// Index in the seeded placement vector this entity came from. It is the
    /// stable key for per-zone progress (opened doors / consumed chests)
    /// because entity ids are re-minted on every destination join.
    placement_index: usize,
    classname: String,
    /// Which class-state builder the entity's ownership needs; a portal's
    /// kind is what tells a click whose destination will not settle to hold
    /// rather than fall through to the quest directory.
    kind: drf_content::PlacedKind,
    /// The placement's `(x, y)` in the x256 wire domain: the key that
    /// identifies it when a click's classname matches more than one portal.
    wire: Option<(i64, i64)>,
    /// A portal's own destination, when its placement/a generation states it.
    zone: Option<String>,
    spawn_point: Option<String>,
    track_activating_players: bool,
    init: Vec<u8>,
    /// Full `0x32` records (tag included); the join strips the tag the same
    /// way the avatar's components are stripped.
    components: Vec<Vec<u8>>,
}

/// One generated mob the session ships: its entity id, class, the `0x02`
/// class state, the four nested `0x32` component records and the two `0x35`
/// trailers (SpawnAction / MoverUpdate).
///
/// The stream deliberately carries **no** `0x35 FollowClient` (`0x64`)
/// enrollment: the client's own `MonsterBehavior2` brain runs the mob's
/// proximity aggro, chase and attack with no server help, and the `0x64`
/// hand-off parks the mob in client-control mode awaiting movers the server
/// never sends. See `drf_proto::mob`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MobCreate {
    entity_id: u16,
    /// The mob's `Behavior` component id: any behavior action addresses THIS,
    /// never the entity id.
    behavior_id: u16,
    classname: String,
    init: Vec<u8>,
    /// Full `0x32` records (tag included); the join strips the tag.
    components: Vec<Vec<u8>>,
    /// The `0x35` SpawnAction payload (after the tag).
    spawn_action: Vec<u8>,
    /// The `0x35` MoverUpdate payload (after the tag).
    mover_update: Vec<u8>,
}

/// The server's own copy of one live mob's combat state.
///
/// Damage is client-side, so the server never sees a hit frame; it decrements
/// this tracked HP once per attack it accepts. Reaching zero only arms the
/// fallback: the client hook's death record (or [`MOB_DEATH_GRACE`]) finalizes
/// the entity.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedMob {
    /// Index in the seeded mob-spawn vector. The stable key for the per-zone
    /// defeated set, since entity ids are re-minted on re-entry.
    spawn_index: usize,
    /// The x256 HP the spawn shipped (the client's `getMaxHealth(level)<<8`).
    hp_wire: u32,
    /// The remaining HP in the same unscaled domain (`hp_wire / 256`).
    hp: u32,
    classname: String,
    /// The spawn level the server chose for this mob. The client's per-kill XP
    /// is a function of it (`min(mob,player)/player`), so the server mirrors
    /// the award against the same byte.
    level: u8,
    pose: [i32; 4],
}

/// One `ItemObject` drop this session has shipped and can still claim.
///
/// The client's `0x50` UseTarget naming a drop's entity id is the pickup leg:
/// the server removes the entity, sends the acquire notice and adds
/// `item_class` to the bag.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DropState {
    /// The real item GC class the drop's `ItemObject` init carried.
    item_class: String,
    /// The generated item level, when the content generator supplied one.
    item_level: Option<u8>,
    /// Rolled `ItemModifier` children, in slot order.
    modifiers: Vec<ModifierRoll>,
    /// `Currency` amount when this drop is a native currency object.
    amount: Option<u32>,
    /// The drop's x256 world pose; the gnome's search/convert radii read it.
    pose: [i32; 4],
}

/// One server-stepped Bling Gnome. It is not a mob: the client owns its
/// follow/fidget simulation, while the server owns its entity lifecycle, the
/// passive gold sniff and the active item-to-gold sweep.
#[derive(Debug, Clone)]
struct GnomeState {
    entity_id: u16,
    behavior_id: u16,
    modifiers_id: u16,
    manipulators_id: u16,
    hp_wire: u32,
    pos: [i32; 3],
    heading: i32,
    spawn_pos: [i32; 2],
    spawn_heading: i32,
    component_at: Option<Instant>,
    behavior_created: bool,
    last_follow: Instant,
    last_mover_at: Option<Instant>,
    mover: Option<([i32; 3], i32, bool)>,
    next_search: Instant,
    next_fidget: Instant,
    last_fidget: u16,
    active_until: Option<Instant>,
    convert_at: Option<Instant>,
    convert_queue: Vec<u16>,
    last_activate: Option<Instant>,
    items_converted: u32,
    gold_generated: u32,
    remove_at: Option<Instant>,
}

/// The main backpack's inventory child id (`MERCHANT_BAG_CONTAINER_ID`).
const MAIN_BAG_INVENTORY_ID: u8 = 0x0B;

/// The Equipment slot ids the client drags onto.
const EQUIPMENT_SLOTS: [u32; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 10, 11];

/// The hotbar slot ids (`0x64..=0x6D`, hotbar slots 1-10).
const HOTBAR_SLOTS: std::ops::RangeInclusive<u32> = 0x64..=0x6D;

/// One live bag entry. `record` is the authored `Item::readData` bytes exactly
/// as delivered, re-addressed whenever its id, cell or count changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BagEntry {
    id: u32,
    class: String,
    cell: (u8, u8),
    count: u32,
    level: u8,
    flags: u8,
    width: u32,
    height: u32,
    record: Vec<u8>,
    modifiers: Vec<ModifierRoll>,
}

/// One live worn item, keyed by its Equipment slot.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WornEntry {
    slot: u32,
    class: String,
    level: u8,
    flags: u8,
    width: u32,
    height: u32,
    /// The Equipment-list record (u32 id = slot, cell `FF FF`).
    record: Vec<u8>,
    /// The Manipulators record: `record` plus the class's readState body.
    manip_record: Vec<u8>,
    modifiers: Vec<ModifierRoll>,
}

/// One parsed c2s UnitContainer request on the avatar's own component.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UnitContainerRequest {
    Empty(u8),
    Drop(u32),
    Use(u32),
    UsePos { item: u32, x: i32, y: i32, z: i32 },
    UseTarget { item: u32, target: u16 },
    Get(u32),
    Put { inventory_id: u8, x: u8, y: u8 },
    Currency,
}

/// One parsed c2s Equipment request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EquipmentRequest {
    Equip { slot: u32, bag_eid: u32 },
    Unequip { slot: u32 },
}

/// One parsed c2s `Skills` request (hotbar place/remove).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillRequest {
    Buy { hash: u32 },
    Place { slot: u32, hash: u32 },
    Remove { slot: u32 },
}

/// One shipped merchant shelf entry: the class the placement instantiates,
/// its record level byte and its stack count (`Item+0x82`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct MerchantStockItem {
    class: String,
    level: u8,
    quantity: u8,
}

/// The `*.Merchant` component a join shipped for one vendor.
///
/// Keyed by the component id the client addresses; `items` binds every minted
/// item id to the shelf entry, so a later buy leg can price and author a
/// purchase against what this join actually shipped.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MerchantRegistration {
    npc_class: String,
    items: HashMap<u32, MerchantStockItem>,
}

/// One shipped `*.SkillTrainer` component and the authored rows it offers.
#[derive(Debug, Clone)]
struct TrainerRegistration {
    npc_class: String,
    skills: HashMap<u32, TrainerSkill>,
}

impl TrainerRegistration {
    fn from_catalog(npc_class: &str, catalog: &TrainerCatalog) -> Self {
        Self {
            npc_class: npc_class.to_string(),
            skills: catalog
                .skills
                .iter()
                .cloned()
                .map(|skill| (skill.hash, skill))
                .collect(),
        }
    }
}

/// A new character's purse, matching the reference `World.AVATAR_GOLD_START`.
const AVATAR_STARTING_GOLD: u32 = 5000;

/// The client's authored `MonsterDifficultySettings` factors, in Q8.
///
/// Health raises the client-computed max HP, damage multiplies the client's
/// outgoing hit, and loot is the server-side drop-count multiplier (the wire
/// value is also what the target tooltip prints).
const DIFFICULTY_SCALES: [UnitScale; 4] = [
    UnitScale::NORMAL,
    UnitScale {
        health: 0x180,
        damage: 0x135,
        loot: 0x180,
    },
    UnitScale {
        health: 0x200,
        damage: 0x168,
        loot: 0x200,
    },
    UnitScale {
        health: 0x280,
        damage: 0x1cf,
        loot: 0x280,
    },
];

/// Placeholder hit count used to derive one attack's damage.
///
/// Damage is client-side and the server has no hit/death frame, so this only
/// *arms* a fallback: it no longer declares death. The mob is despawned, XP is
/// mirrored, loot is dropped and the CEM remove is sent only when the client
/// hook reports the authoritative death (or the grace window expires).
const MOB_HITS_TO_KILL: u32 = 4;

/// One placeholder attack's damage, in the unscaled HP domain.
///
/// `hp_wire` is the spawn's x256 HP, so its raw max is `hp_wire / 256`. At
/// least one point is always removed, which guarantees the fallback arms even
/// for a mob whose derived max rounds down below [`MOB_HITS_TO_KILL`].
fn per_hit_damage(hp_wire: u32) -> u32 {
    (hp_wire / 256 / MOB_HITS_TO_KILL).max(1)
}

/// The selected difficulty's per-unit scale.
fn difficulty_scale(index: u8) -> UnitScale {
    DIFFICULTY_SCALES[usize::from(index.min(3))]
}

/// Apply the health factor exactly as the client's `Unit::computeAttributes`:
/// `scaled_max = (hp_wire / 256) * health / 256`, then back to x256.
fn scaled_hp_wire(hp_wire: u32, scale: UnitScale) -> u32 {
    let max = u64::from(hp_wire) * u64::from(scale.health) / 0x1_0000;
    u32::try_from(max.saturating_mul(256)).unwrap_or(u32::MAX)
}

/// UDP port the paired client hook sends death/HP records to. Started only
/// when the session is created with `--trust-client-sim`; the hook side is
/// `the client telemetry hook/src/drf_death.c` (`DRF_TELEMETRY`).
const HOOK_TELEMETRY_PORT: u16 = 3504;

/// How often a session polls its telemetry receiver. Also the socket read
/// timeout, so an idle connection still wakes to check fallback deadlines.
const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Grace window after the placeholder HP (or client HP telemetry) reaches zero
/// and before the server finalizes a death with no client death record. This is
/// the leak guard, not the normal path: the hook's `StockUnit::onDead` record
/// normally arrives first.
const MOB_DEATH_GRACE: Duration = Duration::from_millis(1200);

/// One client-reported mob death.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HookMobDeath {
    /// The wire entity id the server assigned in the mob's create.
    eid: u16,
    /// The client's unit level (`Unit+0x314`).
    level: u8,
    /// The client's death pose, x256 (`Unit+0x90..0x9C`).
    pose: [i32; 4],
}

/// A record from the client hook's UDP line protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookEvent {
    /// `DRF1 death <eid> <level> <x> <y> <z> <heading> <tick>`
    Death(HookMobDeath),
    /// `DRF1 hp <eid> <hp_wire> <tick>`
    Hp { eid: u16, hp_wire: u32 },
}

/// Live game sessions that want hook telemetry. The listener is process-wide
/// and broadcasts each record; a session accepts only ids it is tracking, and
/// wire entity ids are globally unique, so exactly one session acts on it.
static HOOK_SESSIONS: Mutex<Vec<(u64, mpsc::Sender<HookEvent>)>> = Mutex::new(Vec::new());

/// Opt-in per-packet hex logging. Disabled by default: at the 30 Hz mover rate
/// formatting and writing every frame is pure overhead on the session thread.
static PACKET_LOG: OnceLock<bool> = OnceLock::new();
/// Cached `DRF_HP_DIAG` check. `send` runs on every packet, so the environment
/// must not be consulted there.
static HP_DIAG: OnceLock<bool> = OnceLock::new();

fn packet_log_enabled() -> bool {
    *PACKET_LOG.get_or_init(|| std::env::var_os("DRF_PACKET_LOG").is_some())
}

fn hp_diag_enabled() -> bool {
    *HP_DIAG.get_or_init(|| std::env::var_os("DRF_HP_DIAG").is_some())
}
static HOOK_LISTENER: Once = Once::new();

/// The server-side loot table, rolled one entry per kill.
///
/// These three are the real drop classes the reference server's S3 loot table
/// names and that this client's `game.pkg` defines: the major health potion and
/// the two low-tier armours. They were sighted live as dungeon drops, so the
/// client can render each as a world `ItemObject`. Selection is a per-session
/// round-robin (the reference `World.spawn_drop`'s `_loot_next`), not a
/// per-mob roll: one drop per kill, deterministic and testable.
const LOOT_TABLE: [&str; 3] = [
    "items.consumables.Consumable_MajorHealthPotion",
    "LeatherArmor3PAL.LeatherArmor3-2",
    "ChainArmor1PAL.ChainArmor1-2",
];

/// `TrackedMob.spawn_index` for a runtime summon. These have no seeded spawn
/// row, so their deaths must not enter the per-zone defeated set.
const SUMMONED_MOB_SPAWN_INDEX: usize = usize::MAX;

/// The `MessageAddress` every server->client segment carries (the reference's
/// `game_build` writes `[01 0f 00]`).
const GAME_ADDR: MessageAddress = MessageAddress {
    kind: 0x01,
    id: 0x000F,
};

/// The server's 30 Hz clock. The client consumes one entity-channel message
/// per ~133 ms and triples its world clock on a backlog, so the world interval
/// is paced off this.
const TICK_INTERVAL: Duration = Duration::from_millis(33);

/// Send one `(07,0D)` WorldInterval every this many ticks (~133 ms).
const WORLD_INTERVAL_EVERY: u32 = 4;

/// The raw 23-byte connect-ack the client blocks on after its `0x0A` hello.
/// The body is not binary-attested (see `drf-proto::auth`), but the client
/// waits for it before sending anything else, so it is kept verbatim.
pub const CONNECT_ACK: [u8; 23] = [
    0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x02, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00,
    0x00, 0x00, 0x03, 0x00, 0xb4, 0xb3, 0xb2,
];

/// The player createinit's entity-bound `v1` tail word. Its semantics are
/// unknown; the reference join uses this value. It is never zero.
const PLAYER_JOIN_V1: u16 = 0x04A4;

/// The c2s `0x0A` ping body; a `0x0A` frame whose body equals this is the
/// client's keepalive, answered with a bare `(07,06)` tick.
const PING_BODY: [u8; 15] = [
    0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01,
];

/// Serve one game connection to completion (blocks).
pub fn serve_game(stream: TcpStream, shared: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let peer = stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "?".to_string());
    eprintln!("game {peer}: accepted");
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let writer = Arc::new(Mutex::new(stream));
    GameSession::new(reader, writer, shared).run();
    crate::opcodes::flush();
    eprintln!("game {peer}: closed");
}

/// What this login session has already done in one generated zone.
///
/// Layout seeds make the terrain stable across waypoint/obelisk re-entry; this
/// holds the state that used to be thrown away with the entities: defeated
/// spawns, the explored minimap bits, opened doors, and consumed interactives.
/// The keys are placement/spawn indices, not entity ids, because ids are
/// re-minted for every destination join while the seeded vectors are not.
#[derive(Debug, Clone, Default)]
struct ZoneProgress {
    /// The client's minimap geometry; `None` when this zone has no minimap.
    minimap: Option<drf_content::MazeMinimap>,
    /// Ready's explored words, one bit per 128-unit cell.
    explored: Vec<u32>,
    /// Spawn indices already killed this session.
    defeated_mobs: HashSet<usize>,
    /// Placement indices whose door update was already sent.
    opened_doors: HashSet<usize>,
    /// Placement indices whose authored loot/activation was consumed.
    consumed_places: HashSet<usize>,
}

/// A per-connection game session.
struct GameSession {
    reader: TcpStream,
    writer: Arc<Mutex<TcpStream>>,
    shared: Arc<Shared>,
    conn_id: u64,
    peer: Arc<Mutex<[u8; 3]>>,
    joined: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// The selected character's sqlid, once `(04,05)` has bound it.
    selected: Option<u32>,
    /// The session's zone token: the selected character's persisted zone.
    zone: String,
    /// Monster difficulty index (0 Normal, 1 Formidable, 2 Extreme,
    /// 3 Insane), persisted per character.
    difficulty: u8,
    /// The arrival spawn-point NAME the last transfer armed, consumed by the
    /// destination join so the arrival resolves by the name the portal asked
    /// for instead of the zone's default rung. `None` on a cold join.
    zone_spawn_request: Option<String>,
    /// An explicit x256 spawn position for the next join (the obelisk return
    /// to the waypoint scroll's cast point); consumed by `join`.
    zone_spawn_pos: Option<[i32; 2]>,
    avatar_eid: Option<u16>,
    player_eid: Option<u16>,
    /// The zone's placed world entities for this session: `(eid, classname,
    /// 0x02 class-state payload)`. Derived from `game.pkg` and allocated once
    /// per selection so a repeated join reuses the ids the client holds.
    placed_entities: Vec<PlacedCreate>,
    /// The generated zone's mob entities, built once per selection like the
    /// placed entities so a repeated join reuses the ids the client holds.
    mob_entities: Vec<MobCreate>,
    /// Live mobs this session can accept attacks against, keyed by entity id.
    /// Cleared with the entity set on selection and zone change.
    mob_state: HashMap<u16, TrackedMob>,
    /// Mobs whose placeholder HP (or client HP telemetry) reached zero, with
    /// the instant the grace window armed. The client death record finalizes
    /// first; this map only drives the leak guard. Kept separate from
    /// [`TrackedMob`] so the spawn-pose emission source stays untouched.
    mob_defeat_pending: HashMap<u16, Instant>,
    /// Death/HP records from the paired client hook (`--trust-client-sim`).
    /// Empty when the session was not created trusted.
    hook_rx: mpsc::Receiver<HookEvent>,
    /// Whether this session trusts the hook's death record. When set, the
    /// placeholder fallback only cleans up server state (no loot/XP): dropping
    /// loot on a timer would re-introduce the pre-death drop the hook fixes.
    /// When clear, the legacy placeholder finalize runs on the timer.
    hook_telemetry: bool,
    /// Live drops this session has shipped and can still claim, keyed by entity
    /// id. Cleared with the entity set on selection and zone change.
    drops: HashMap<u16, DropState>,
    /// The next [`LOOT_TABLE`] index a kill rolls (per-session round-robin).
    loot_cursor: usize,
    /// The session's MT19937 loot stream. The binary draws table gates,
    /// uniform picks, level jitter, modifiers and currency from one stream.
    loot_rng: MersenneTwister,
    /// The selected character's class level and class base, once joined. The
    /// offer directory is a function of `(zone, level, class, completed)`.
    selected_level: u16,
    /// The selected character's avatar-class FQN, once joined. Needed to derive
    /// the level-scaled HP word after a level-up.
    avatar_class: Option<String>,
    /// Progress into the current level (`Hero +0x320`). The client self-awards
    /// on a kill; the server mirrors the same math and persists it here.
    experience: u32,
    /// Unspent bonus attribute points (`Hero +0x33c`), 5 per mirrored level-up.
    attribute_points: u32,
    /// Allocated primary stats in client order: Strength (`Hero +0x334`),
    /// Agility (`+0x336`), Toughness (`+0x338`), Power (`+0x33A`).
    attributes: [u32; 4],
    class_base: Option<String>,
    /// The avatar's level-1 HP **wire** word (already x256) for the joined
    /// class. Shared with the ticker so the WorldInterval move-ack carries the
    /// same word as the `0x02` create and every `(07,35)` synch trailer.
    avatar_hp_wire: Arc<AtomicU32>,
    /// Whether the session is in a combat zone (anything but town/tutorial).
    /// Shared with the ticker: in a combat zone the avatar's synch compare has
    /// no client-side bypass, so the WorldInterval move-ack must not carry an
    /// avatar-HP trailer and is suppressed. See [`owner_move_ack_wire`].
    combat_zone: Arc<AtomicBool>,
    /// The player's nested QuestManager component id, once joined. Every quest
    /// response is addressed to it; answering an unregistered id is client
    /// error 9.
    quest_cid: Option<u16>,
    /// The avatar's `UnitContainer` component id, once joined. An on-accept
    /// item grant is a `0x35` add addressed to it.
    avatar_container_cid: Option<u16>,
    /// The avatar's `avatar.base.Equipment` component id, once joined.
    equipment_cid: Option<u16>,
    /// The avatar's `Manipulators` component id, once joined.
    manipulators_cid: Option<u16>,
    /// The avatar's `avatar.base.Skills` component id, once joined.
    skills_cid: Option<u16>,
    /// Hotbar slot id (`0x64..=0x6D`) -> skill GC class.
    skill_slots: BTreeMap<u32, String>,
    /// Every class skill (FQN, starting id, level) for hash resolution.
    skill_catalog: Vec<(String, u32, u8)>,
    /// Unspent skill points (`Skills +0x70`), one per level after the first.
    skill_points: u32,
    /// Bought skill levels keyed by the skill FQN.
    skill_levels: BTreeMap<String, u8>,
    /// The maze seed sent in this entry's Assign (0 for hand-placed zones).
    maze_seed: u32,
    /// Per-zone generated layouts for this login session. A zone keeps the
    /// same seed across waypoint/portal re-entry; selecting a character or
    /// clicking Reset Instances clears the map.
    maze_seeds: HashMap<String, u32>,
    /// Per-zone progress for those layouts: kills, minimap bits, doors and
    /// consumed interactives. Cleared with the seeds, never on a transfer.
    zone_progress: HashMap<String, ZoneProgress>,
    /// The session's bag occupancy. Empty for a fresh character; `None` when
    /// `game.pkg` states no bag geometry, and every add then holds.
    bag_grid: Option<BagGrid>,
    /// Live bag entries by their session-local u32 item id. The id is stable
    /// for the life of the entry; every client request resolves here.
    bag_entries: HashMap<u32, BagEntry>,
    /// The next bag item id to mint.
    next_bag_id: u32,
    /// The bag item currently on the cursor (selected by `0x28`), if any.
    cursor: Option<u32>,
    /// Live worn items by Equipment slot.
    worn: HashMap<u32, WornEntry>,
    /// Live town-portal entities by id -> destination zone.
    town_portals: HashMap<u16, String>,
    /// The return point saved by the last waypoint scroll: `(zone, [x,y])`.
    town_portal_return: Option<(String, [i32; 2])>,
    /// The last zone left through a zone transfer, shown in the obelisk
    /// dialog as Recent Zone Portal: `(zone, spawn point name)`.
    recent_zone_portal: Option<(String, String)>,
    /// The dialog the last click/query armed: `(qid u32, giver eid)`. A
    /// following accept is about this quest.
    dlg: Option<(u32, u16)>,
    /// Accepted quests: qid u32 -> instance id. The offer directory's fourth
    /// gate (a completed quest is not re-offered) reads the completed set.
    quest_instance: HashMap<u32, u32>,
    /// The item classes this session's bag holds, for `ItemObjective`
    /// satisfaction (a bag change restates every held quest's completion).
    bag_items: HashSet<String>,
    /// qids this character has completed (turned in). Empty for a fresh one.
    quest_completed: HashSet<u32>,
    /// The next quest instance id to mint.
    next_instance: u32,
    /// The avatar's `avatar.base.UnitBehavior` component id, once joined.
    /// Movement is addressed to this component, never the avatar entity.
    /// Shared with the ticker so the WorldInterval can echo the move-ack.
    avatar_ub_cid: Arc<Mutex<Option<u16>>>,
    /// The staged owner move-ack: the client's c2s mover records coalesced
    /// since the last WorldInterval, drained by the ticker into the local
    /// move-ack (an echo of the client's own records).
    pending_move: Arc<Mutex<MoveAck>>,
    /// Recurring per-unit payloads (gnome movers/actions/fidgets) drained into
    /// the 0x0D WorldInterval. The client expects these as interval messages,
    /// never standalone (live 2026-10-20 `ChannelType(53)` spam).
    pending_interval: Arc<Mutex<Vec<Vec<u8>>>>,
    /// The player's live position in x256 world units, updated from the last
    /// c2s mover record -- the client is avatar-authoritative and the server
    /// never drives the avatar. `None` until the current zone's first mover.
    /// A mob's loot drops at this pose, so a kill lands where the killer
    /// stands rather than at the server's untouched spawn anchor.
    player_pos: Option<[i32; 2]>,
    /// The z of the last position-targeted cast, in x256 world units. The
    /// client death hook carries the true corpse pose, but some hero/boss
    /// deaths never emit it; the timeout fallback then only has the spawn
    /// anchor, whose z is 0 on generated encounters. Use the last combat z so
    /// the resulting ground drops are not buried below the visible floor.
    last_combat_z: Option<i32>,
    /// Throttles the proximity half of `GoToObjective` so a 30 Hz mover
    /// stream does not reopen `game.pkg` on every frame.
    arrival_check_at: Option<Instant>,
    /// The next free merchant item id in the reserved `0x0001..=0x0FFF` band;
    /// zero is the unset sentinel. Shared across every vendor in the join.
    merchant_item_cursor: u32,
    /// The `*.Merchant` components this join shipped, keyed by component id.
    /// The buy leg resolves a purchase here; nothing mints an id it did not ship.
    merchant_components: HashMap<u16, MerchantRegistration>,
    /// The `*.SkillTrainer` components this join shipped, keyed by component
    /// id. Any ComponentUpdate on one is a train request, exactly as the
    /// client's `SkillTrainer::trainSkill` sends it.
    trainer_components: HashMap<u16, TrainerRegistration>,
    /// The session's purse (`Container+0x74` on the avatar's `UnitContainer`).
    /// The buy leg debits it and the join ships the word.
    gold: u32,
    /// The live Bling Gnome, when its skill is on the hotbar / summoned.
    gnome: Option<GnomeState>,
    /// The authored skill/summon contract, derived once per session.
    gnome_profile: Option<BlingGnomeProfile>,
    /// Whether the client's `0x0A` hello has been answered. The first such
    /// frame is the hello whatever its length; later `0x0A` frames are pings
    /// or a repeat hello.
    hello_done: bool,
}

impl GameSession {
    fn new(reader: TcpStream, writer: Arc<Mutex<TcpStream>>, shared: Arc<Shared>) -> GameSession {
        let conn_id = shared.alloc_conn();
        let peer = Arc::new(Mutex::new([0u8; 3]));
        let joined = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let pending_move = Arc::new(Mutex::new(MoveAck::default()));
        let pending_interval = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let avatar_ub_cid = Arc::new(Mutex::new(None));
        let avatar_hp_wire = Arc::new(AtomicU32::new(MINIMAL_AVATAR_HP));
        let combat_zone = Arc::new(AtomicBool::new(false));
        let hook_telemetry = shared.trust_client_sim();
        if shared.ticker_enabled() {
            spawn_ticker(
                Arc::clone(&writer),
                Arc::clone(&peer),
                Arc::clone(&joined),
                Arc::clone(&stop),
                Arc::clone(&pending_move),
                Arc::clone(&pending_interval),
                Arc::clone(&avatar_ub_cid),
                Arc::clone(&avatar_hp_wire),
                Arc::clone(&combat_zone),
                shared.trust_client_sim(),
            );
        }
        // A short read timeout wakes the loop for telemetry polling and death
        // fallback deadlines without a second thread owning session state.
        let _ = reader.set_read_timeout(Some(HOOK_POLL_INTERVAL));
        let (hook_tx, hook_rx) = mpsc::channel();
        if hook_telemetry {
            ensure_hook_listener();
            if let Ok(mut sessions) = HOOK_SESSIONS.lock() {
                // Stale senders are pruned by the listener on the next record
                // (send failure); no need for an is-closed probe here.
                sessions.push((conn_id, hook_tx));
            }
        }
        let gnome_profile = shared.bling_gnome_profile();
        GameSession {
            reader,
            writer,
            shared,
            conn_id,
            peer,
            joined,
            stop,
            selected: None,
            zone: String::new(),
            difficulty: 0,
            zone_spawn_request: None,
            zone_spawn_pos: None,
            selected_level: 1,
            avatar_class: None,
            experience: 0,
            attribute_points: 0,
            attributes: [0; 4],
            class_base: None,
            avatar_hp_wire,
            combat_zone,
            quest_cid: None,
            avatar_container_cid: None,
            equipment_cid: None,
            manipulators_cid: None,
            skills_cid: None,
            skill_slots: BTreeMap::new(),
            skill_catalog: Vec::new(),
            skill_points: 0,
            skill_levels: BTreeMap::new(),
            maze_seed: 0,
            maze_seeds: HashMap::new(),
            zone_progress: HashMap::new(),
            bag_grid: None,
            bag_entries: HashMap::new(),
            next_bag_id: 1,
            cursor: None,
            worn: HashMap::new(),
            town_portals: HashMap::new(),
            town_portal_return: None,
            recent_zone_portal: None,
            dlg: None,
            quest_instance: HashMap::new(),
            bag_items: HashSet::new(),
            quest_completed: HashSet::new(),
            next_instance: 1,
            avatar_eid: None,
            player_eid: None,
            placed_entities: Vec::new(),
            mob_entities: Vec::new(),
            mob_state: HashMap::new(),
            mob_defeat_pending: HashMap::new(),
            hook_rx,
            hook_telemetry,
            drops: HashMap::new(),
            loot_cursor: 0,
            loot_rng: MersenneTwister::new(
                (conn_id as u32)
                    ^ SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|duration| duration.subsec_nanos())
                        .unwrap_or(0)
                    ^ 0x6C6F_6F74,
            ),
            avatar_ub_cid,
            pending_move,
            pending_interval,
            player_pos: None,
            last_combat_z: None,
            arrival_check_at: None,
            merchant_item_cursor: 0,
            merchant_components: HashMap::new(),
            trainer_components: HashMap::new(),
            gold: AVATAR_STARTING_GOLD,
            gnome: None,
            gnome_profile,
            hello_done: false,
        }
    }

    fn run(&mut self) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        while !self.stop.load(Ordering::SeqCst) {
            // Telemetry and fallback deadlines are serviced on the session's
            // own thread so all mob/loot state stays single-owner.
            self.poll_hook_telemetry();
            self.check_mob_timeouts();
            self.gnome_tick(Instant::now());
            let read = match self.reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => break,
            };
            buffer.extend_from_slice(&chunk[..read]);
            while !buffer.is_empty() {
                // A raw c2s handshake frame is `[0x0A][peer3][u32 bodyLen]
                // [body]`: a ping's body is [`PING_BODY`] and the first hello
                // is whatever length it arrives at (later 8-byte ones repeat
                // it). Anything else flagged `0x0A` is an ordinary segment.
                if buffer[0] == 0x0A {
                    if buffer.len() < 8 {
                        break;
                    }
                    let body_len =
                        u32::from_le_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
                    if buffer.len() < 8 + body_len {
                        break;
                    }
                    let body = buffer[8..8 + body_len].to_vec();
                    let ping = body == PING_BODY.as_slice();
                    let hello = !self.hello_done || body_len == 8;
                    if ping || hello {
                        let peer = [buffer[1], buffer[2], buffer[3]];
                        buffer.drain(..8 + body_len);
                        self.handle_handshake(peer, &body, ping);
                        continue;
                    }
                }
                match Segment::decode(&buffer) {
                    Ok((segment, used)) => {
                        buffer.drain(..used);
                        self.handle_segment(segment);
                    }
                    Err(SegmentError::Incomplete) => break,
                    Err(SegmentError::Invalid) => {
                        self.stop.store(true, Ordering::SeqCst);
                        return;
                    }
                }
            }
        }
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Answer a raw c2s `0x0A` handshake frame: a ping gets a bare `(07,06)`
    /// tick, a hello gets the 23-byte connect-ack.
    fn handle_handshake(&mut self, peer: [u8; 3], body: &[u8], ping: bool) {
        if let Ok(mut current) = self.peer.lock() {
            *current = peer;
        }
        if ping {
            eprintln!("game {}: ping -> (07,06)", self.conn_id);
            self.send(0x07, 0x06, &[]);
        } else {
            self.hello_done = true;
            eprintln!(
                "game {}: hello {}B -> connect-ack",
                self.conn_id,
                body.len() + 8
            );
            if let Ok(mut writer) = self.writer.lock() {
                let _ = writer.write_all(&CONNECT_ACK);
            }
        }
    }

    fn handle_segment(&mut self, segment: Segment) {
        if let Ok(mut peer) = self.peer.lock() {
            *peer = segment.peer;
        }
        let Ok(inner) = segment.payload.inflate() else {
            return;
        };
        if inner.len() < 2 {
            return;
        }
        self.dispatch(inner[0], inner[1], &inner[2..]);
    }

    fn dispatch(&mut self, channel: u8, mtype: u8, payload: &[u8]) {
        if packet_log_enabled() {
            eprintln!(
                "game {}: ({channel:02x},{mtype:02x}) {}B hex={}",
                self.conn_id,
                payload.len(),
                payload
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
        }
        match (channel, mtype) {
            // The client's char-select handshake: `(04,00)` is answered with
            // an empty `(04,00)` before it will ask for the list `(04,03)`;
            // `(03,00)`/`(03,01)` are burst requests answered with eight zero
            // bytes; `(09,00)` re-requests the roster.
            (0x04, 0x00) => self.send(0x04, 0x00, &[]),
            (0x03, 0x00) => self.send(0x03, 0x00, &[0u8; 8]),
            (0x03, 0x01) => self.send(0x03, 0x01, &[0u8; 8]),
            (0x09, 0x00) => self.roster(),
            (0x09, 0x17) => self.set_monster_difficulty(payload),
            (0x09, 0x26) => self.reset_instances(),
            (0x04, 0x03) => self.char_list(),
            (0x04, 0x02) => self.create(payload),
            (0x04, 0x05) => self.select(payload),
            (0x04, 0x04) => self.delete(payload),
            (0x07, 0x04) => self.entity_request(payload),
            (0x07, 0x34) => self.entity_channel(payload),
            (0x0D, 0x06) => self.join(),
            (0x0D, 0x08) => self.respawn_request(),
            (0x0D, 0x0C) => self.use_recent_zone_portal(),
            // The client's own admin surfaces: `AdminUI::sendAdminCommand`
            // sends chat mtype 0x09, the `/admin` handler mtype 0x0D.
            (0x06, 0x09) | (0x06, 0x0D) => self.admin_chat(payload, mtype),
            _ => crate::opcodes::record("dispatch", channel, mtype, None, payload, &self.zone),
        }
    }

    /// `(07,04)` entity request: `Entity::sendRequest`'s
    /// `[u16 entityId][u8 requestType][body]` envelope.
    ///
    /// Only the attribute-allocation legs are handled; the client's other
    /// `(07,04)` requests (and any malformed/unknown frame) are ignored
    /// silently, matching the previous behaviour for an unhandled sub-op.
    fn entity_request(&mut self, payload: &[u8]) {
        // `Avatar::respawn` sends the avatar's Entity request type `0x15`
        // when the RespawnDialog's button is pressed. It is the c2s respawn
        // trigger; the reply is the normal `(13,2)` respawn transfer + assign.
        if payload.len() >= 3 && payload[2] == 0x15 {
            self.respawn_request();
            return;
        }
        match parse_attribute_request(payload) {
            Some(AttributeRequest::Spend {
                entity_id,
                stat,
                points,
            }) => self.spend_attribute_point(entity_id, stat, points),
            // Return/respec are parsed so the wire grammar stays exercised,
            // but only spend is in scope here.
            Some(AttributeRequest::Return { .. })
            | Some(AttributeRequest::Respec { .. })
            | None => {
                crate::opcodes::record(
                    "entity_request",
                    0x07,
                    0x04,
                    payload.get(2).copied(),
                    payload,
                    &self.zone,
                );
            }
        }
    }

    /// Travel to a `Checkpoint` class's own `Zone`/`SpawnPoint`.
    fn quest_checkpoint(&mut self, travel: CheckpointTravel) {
        let checkpoint = match travel {
            CheckpointTravel::Class(class) => class,
            CheckpointTravel::Hash(hash) => match self.shared.checkpoint_class_by_hash(hash) {
                Some(class) => class,
                None => {
                    eprintln!(
                        "game {}: obelisk checkpoint hash {hash:#010x} resolves to no class",
                        self.conn_id
                    );
                    return;
                }
            },
        };
        self.credit_activate_quests(&checkpoint);
        let Some(dest) = self.shared.checkpoint_destination(&checkpoint) else {
            eprintln!(
                "game {}: obelisk checkpoint {checkpoint} has no content destination",
                self.conn_id
            );
            return;
        };
        if self.begin_zone_transfer(&dest) {
            eprintln!(
                "game {}: obelisk {checkpoint} -> {} (spawn={})",
                self.conn_id,
                dest.zone,
                if dest.spawn.is_empty() {
                    "<zone default>"
                } else {
                    &dest.spawn
                }
            );
        }
    }

    /// QuestManager `0x0C`: the obelisk dialog's Recent Zone Portal row.
    /// The client sends no destination, so the server uses the zone this
    /// session last left through a transfer. The dialog only offers the row
    /// when the join carried an `AutoPortalZone`, so an empty state here is a
    /// stale/malicious request rather than something to guess at.
    fn use_recent_zone_portal(&mut self) {
        let Some((zone, spawn)) = self.recent_zone_portal.clone() else {
            eprintln!(
                "game {}: recent-zone-portal request with no saved portal",
                self.conn_id
            );
            return;
        };
        let dest = Destination { zone, spawn };
        if self.begin_zone_transfer(&dest) {
            eprintln!(
                "game {}: recent-zone-portal -> {} (spawn={})",
                self.conn_id,
                dest.zone,
                if dest.spawn.is_empty() {
                    "<zone default>"
                } else {
                    &dest.spawn
                }
            );
        }
    }

    /// The client's own admin control plane (`AdminUI::sendAdminCommand`
    /// mtype `0x09`, the `/admin` handler mtype `0x0D`).
    ///
    /// Every payload is recorded verbatim to the unhandled-opcode journal so
    /// the capture pass has each command string. `TransferSelf <zone>
    /// [spawn]` is applied because it is the escape hatch; the remaining
    /// buttons are logged only until their intended effect is agreed.
    fn admin_chat(&mut self, payload: &[u8], mtype: u8) {
        let text: String = payload
            .iter()
            .take_while(|&&byte| byte != 0)
            .map(|&byte| byte as char)
            .collect();
        let text = text.trim();
        eprintln!(
            "game {}: admin chat (06,{mtype:02x}) {}B text={text:?}",
            self.conn_id,
            payload.len()
        );
        crate::opcodes::record("admin_chat", 0x06, mtype, None, payload, &self.zone);

        let tokens: Vec<&str> = text.split_whitespace().collect();
        let tokens = if tokens
            .first()
            .is_some_and(|token| token.eq_ignore_ascii_case("/admin"))
        {
            &tokens[1..]
        } else {
            &tokens[..]
        };
        let Some(command) = tokens.first().copied() else {
            return;
        };
        if command.eq_ignore_ascii_case("TransferSelf") {
            let Some(raw_zone) = tokens.get(1).copied().filter(|zone| !zone.is_empty()) else {
                eprintln!("game {}: admin TransferSelf needs a zone", self.conn_id);
                return;
            };
            // The shipped AdminUI button sends the cut name `pvp_town`; the
            // live Pwnston zone is `pvp_start` (Label "Pwnston"). Normalize the
            // known button names and refuse anything the package cannot serve,
            // so a bad admin transfer cannot hang the client on the join.
            let zone = match raw_zone.to_ascii_lowercase().as_str() {
                "pvp_town" | "pwnston" => "pvp_start",
                "thehub" => "TheHub",
                _ => raw_zone,
            };
            if !self.shared.zone_exists(zone) {
                eprintln!(
                    "game {}: admin TransferSelf refused: zone {zone:?} is not in game.pkg",
                    self.conn_id
                );
                return;
            }
            let spawn = tokens.get(2).copied().unwrap_or("");
            let dest = Destination {
                zone: zone.to_string(),
                spawn: spawn.to_string(),
            };
            if self.begin_zone_transfer(&dest) {
                eprintln!(
                    "game {}: admin TransferSelf -> {zone}:{spawn}",
                    self.conn_id
                );
            }
        }
    }

    /// Respawn the dead avatar in its zone's `RespawnZone`.
    ///
    /// Triggered by the client's `Avatar::respawn` entity request (`0x15`)
    /// and by a c2s `(0x0D,0x08)` TransferToRespawn. A death in a dungeon
    /// crosses to town's `Respawn` graveyard waypoint; a death in the zone
    /// itself stays there (`RespawnSpawnPoint`/`Start`). The transfer uses
    /// reason `respawn`, then the client re-joins and the normal join spawn
    /// resolution places the fresh avatar.
    fn respawn_request(&mut self) {
        if self.avatar_eid.is_none() {
            // Already transferred: a second click/HP record must not mint a
            // second assign on a component set the client is dropping.
            return;
        }
        // Do NOT remove the dead avatar before the transfer. The zone assign
        // clears the source world client-side, and a remove that lands after
        // the new world is loaded names an entity the CEM no longer has:
        // `processEntityRemove Invalid EntityID` -> Code 6 (live crash at the
        // Tutorial respawn, 2026-10-20).
        let Some((zone, spawn)) = self.shared.zone_respawn(&self.zone) else {
            eprintln!(
                "game {}: respawn refused: no RespawnZone for zone={}",
                self.conn_id, self.zone
            );
            return;
        };
        let dest = Destination {
            zone,
            spawn: spawn.unwrap_or_default(),
        };
        if self.begin_zone_transfer_with_reason(&dest, b"respawn") {
            eprintln!(
                "game {}: respawn -> {} (spawn={})",
                self.conn_id,
                dest.zone,
                if dest.spawn.is_empty() {
                    "<zone default>"
                } else {
                    &dest.spawn
                }
            );
        }
    }

    /// Apply one `0x11` SpendAttribPoint request.
    ///
    /// The client sends the count it wants to move; the server validates it
    /// against the session's own unspent pool (never trusting the wire),
    /// persists the new allocation, then sends the `(07,03)` confirmation
    /// (`Hero::processUpdate` case `0x11`) that makes the client decrement
    /// `Hero+0x33c` and bump the chosen attribute. A mismatched entity id, an
    /// over-spend, or an unknown stat is dropped; the client's UI simply stays
    /// where it was.
    fn spend_attribute_point(&mut self, entity_id: u16, stat: u8, points: u8) {
        let Some(avatar_eid) = self.avatar_eid else {
            return;
        };
        if entity_id != avatar_eid {
            eprintln!(
                "game {}: stat spend for entity {entity_id:#06x} != avatar {avatar_eid:#06x}; ignored",
                self.conn_id
            );
            return;
        }
        if usize::from(stat) >= 4 {
            return;
        }
        let slot = usize::from(stat);
        let wanted = u32::from(points);
        if wanted == 0 || wanted > self.attribute_points {
            eprintln!(
                "game {}: stat spend wants {wanted} point(s) into stat {stat}, but only {} are unspent; ignored",
                self.conn_id, self.attribute_points
            );
            return;
        }
        self.attribute_points -= wanted;
        self.attributes[slot] = self.attributes[slot].saturating_add(wanted);
        self.persist_attributes();
        let body = build_hero_stat_update(
            avatar_eid,
            REQ_SPEND_ATTRIB,
            stat,
            points,
            self.avatar_hp_word(),
        );
        if let Some(body) = body {
            self.send(0x07, 0x03, &body);
        }
        eprintln!(
            "game {}: stat spend +{wanted} into stat {stat} (STR={} AGI={} TOUGH={} POW={} pts={})",
            self.conn_id,
            self.attributes[0],
            self.attributes[1],
            self.attributes[2],
            self.attributes[3],
            self.attribute_points
        );
    }

    /// `(07,34)` entity-request. The same envelope carries five client legs,
    /// told apart by their length and sub-opcode: the attack/use `0x50`
    /// (11B), the Interact-06 NPC click
    /// (`[u16 comp][01][responseId][06][session][u16 target]`, 10B), the
    /// QuestManager query (`[u16 qm][06][u32 npc][GcType qid]`, 14B) and
    /// accept/close (`[u16 qm][01|02]`, 5B) -- all on the avatar's UnitBehavior
    /// or the player's QuestManager component -- plus the movement
    /// `RequestClientControl` (`[u16 comp][0x64][on]`) and mover stream.
    fn entity_channel(&mut self, payload: &[u8]) {
        let mut inner = Vec::with_capacity(2 + payload.len());
        inner.push(0x07);
        inner.push(0x34);
        inner.extend_from_slice(payload);
        // Position skills (`0x51`): acknowledge the packed cast so the
        // client's action resolves; the effect is client-authoritative.
        if let Some(cast) = parse_position_cast(&inner) {
            let ub = self.avatar_ub_cid.lock().ok().and_then(|guard| *guard);
            if ub == Some(cast.component) {
                self.last_combat_z = Some(cast.z as i32);
                let ack = position_cast_ack(&inner, self.avatar_hp_word());
                self.send(0x07, 0x35, &ack[2..]);
                eprintln!(
                    "game {}: position-cast slot={} at ({},{},{}) acked",
                    self.conn_id, cast.action_id, cast.x, cast.y, cast.z
                );
                return;
            }
        }
        // Skill `Use` (`0x52`): the client's cast never resolves without the
        // action ack. It is the same echo the targeted `0x50` leg sends; the
        // effect itself is client-authoritative.
        if let Some(cast) = parse_self_cast(&inner) {
            let ub = self.avatar_ub_cid.lock().ok().and_then(|guard| *guard);
            if ub == Some(cast.component) {
                let ack = self_cast_ack(&inner, self.avatar_hp_word());
                self.send(0x07, 0x35, &ack[2..]);
                eprintln!(
                    "game {}: self-cast slot={} (response {}) acked",
                    self.conn_id, cast.slot, cast.response
                );
                let skill = self.skill_slots.get(&u32::from(cast.slot)).cloned();
                if skill.as_deref().is_some_and(Self::is_gnome_skill) {
                    self.gnome_toggle();
                }
                return;
            }
        }
        // Combat is its own `0x50` leg; it must never fall through to the
        // mover parser when the client is waiting for an ack.
        if let Some(attack) = parse_attack_use(&inner) {
            self.attack_use(&inner, &attack);
            return;
        }
        if let Some(click) = parse_interact_click(&inner) {
            self.interact_click(&inner, &click);
            return;
        }
        // The obelisk's "go": QuestManager::goToCheckpoint writes the selected
        // checkpoint's GC type and requests 0x07. The class's own
        // Description.Zone/SpawnPoint is the destination. Handled before the
        // cid-keyed quest legs because the client may address the checkpoint
        // object rather than the QuestManager.
        if let Some(checkpoint) = parse_checkpoint_travel(&inner) {
            self.quest_checkpoint(checkpoint);
            return;
        }
        if let Some(qref) = self.quest_cid {
            if let Some(query) = parse_quest_query(&inner, qref) {
                self.quest_query(&query);
                return;
            }
            if let Some(step) = parse_quest_accept(&inner, qref) {
                self.quest_accept(step);
                return;
            }
            if let Some(instance) = parse_quest_abandon(&inner, qref) {
                self.quest_abandon(instance);
                return;
            }
            if let Some((step, instance)) = parse_quest_turnin(&inner, qref) {
                self.quest_turnin(step, instance);
                return;
            }
        }
        // QuestManager `0x0A`: the obelisk's "Saved Places" return to the
        // waypoint-scroll cast zone.
        if let Some(qref) = self.quest_cid {
            if payload.len() >= 3
                && u16::from_le_bytes([payload[0], payload[1]]) == qref
                && payload[2] == 0x0A
            {
                if let Some((zone, pos)) = self.town_portal_return.clone() {
                    let dest = Destination {
                        zone,
                        spawn: String::new(),
                    };
                    if self.begin_zone_transfer(&dest) {
                        self.zone_spawn_pos = Some(pos);
                    }
                }
                return;
            }
            if payload.len() >= 3
                && u16::from_le_bytes([payload[0], payload[1]]) == qref
                && payload[2] == 0x0C
            {
                self.use_recent_zone_portal();
                return;
            }
        }
        if let Some((cid, request)) = parse_equipment_request(payload) {
            if self.equipment_cid == Some(cid) {
                self.handle_equipment_request(request);
                return;
            }
        }
        if let Some((cid, request)) = parse_unit_container_request(payload) {
            if self.avatar_container_cid == Some(cid) {
                self.handle_unit_container_request(request);
                return;
            }
        }
        if let Some((cid, player, ref_type, hash)) = parse_trainer_request(payload) {
            if self.trainer_components.contains_key(&cid) {
                self.trainer_train(cid, player, ref_type, hash);
                return;
            }
        }
        if let Some((cid, request)) = parse_skills_request(payload) {
            if self.skills_cid == Some(cid) {
                self.handle_skills_request(request);
                return;
            }
        }
        if let Some((cid, buyer, item_id)) = parse_merchant_buy(payload) {
            self.merchant_buy(cid, buyer, item_id);
            return;
        }
        if let Some((cid, entity_ref, item_id)) = parse_merchant_sell(payload) {
            self.merchant_sell(cid, entity_ref, item_id);
            return;
        }
        if let Some(request) = parse_control_request(&inner) {
            // The client names the COMPONENT it wants input authority over.
            // The only grant this session answers is the avatar's own
            // `avatar.base.UnitBehavior`, whose trailer carries the session's
            // avatar HP word. Every mob was already `0x64`-enrolled at spawn, so
            // a late request naming a mob's `Behavior` component is ignored: its
            // trailer would re-assert the mob's SPAWN HP, and the client's
            // locally-simulated mob HP (it self-rolls the player's damage) has
            // since diverged — the zero-tolerance `EntitySynchInfo::Validate`
            // (@0x5DD900) would fatal with "Entity synch error" and latch error
            // 1.
            if request.on {
                let ub = self.avatar_ub_cid.lock().ok().and_then(|guard| *guard);
                if ub == Some(request.entity_id) {
                    if let Ok(grant) =
                        follow_client_payload_wire(request.entity_id, true, self.avatar_hp_word())
                    {
                        self.send(0x07, 0x35, &grant);
                    }
                }
            }
            return;
        }
        if let Some(mover) = parse_mover(&inner) {
            // A `0x65` stream addresses a COMPONENT. The avatar's UnitBehavior is
            // the owner's own prediction stream; an enrolled mob emits its OWN
            // `0x65` stream on its behavior component. Only the avatar's is
            // echoed back as the local move-ack: echoing a mob's records on the
            // avatar's id would teleport the player, and folding a mob's pose
            // into `player_pos` would drop loot in the wrong place. An enrolled
            // mob's `0x65` is therefore tracked as no-op (its position is
            // client-authoritative).
            let avatar_ub = self.avatar_ub_cid.lock().ok().and_then(|guard| *guard);
            if avatar_ub == Some(mover.component) {
                // Track the newest position the client reported (the last
                // record of this batch) so a later kill can drop loot where the
                // player actually stands. The records are then echoed back
                // verbatim.
                if let Some(position) = last_mover_position(&mover.data) {
                    self.player_pos = Some(position);
                    self.mark_explored_position(position);
                }
                if let Ok(mut pending) = self.pending_move.lock() {
                    pending.queue(mover.session, mover.count, &mover.data);
                }
                if self
                    .arrival_check_at
                    .is_none_or(|at| at.elapsed() >= Duration::from_millis(500))
                {
                    self.arrival_check_at = Some(Instant::now());
                    self.credit_arrival_quests();
                }
            }
            // `parse_mover` claimed this frame. An enrolled mob's stream is
            // deliberately a no-op, not an unhandled opcode; journaling it
            // flushed a line per mob frame.
            return;
        }
        // No parser claimed this `(07,0x34)` sub-op; keep its shape so a new
        // client leg surfaces in `unhandled-opcodes.json` instead of vanishing.
        crate::opcodes::record(
            "entity_channel",
            0x07,
            0x34,
            payload.get(2).copied(),
            payload,
            &self.zone,
        );
    }

    /// The AVATAR's x256 HP word that every avatar-component synch trailer must
    /// carry. It is the joined class's observed level-1 word
    /// ([`drf_proto::avatar_hp_wire`]); the `0x02` create, every `(07,35)`
    /// trailer and the WorldInterval move-ack all read this one value, because
    /// `EntitySynchInfo::Validate` @`0x5DD900` fatals on any other word.
    fn avatar_hp_word(&self) -> u32 {
        self.avatar_hp_wire.load(Ordering::Relaxed)
    }

    /// The c2s `0x50` attack/use.
    ///
    /// The ack always goes out first -- the client blocks on it, so even an
    /// attack at an entity this session does not track is acknowledged. When
    /// the target is a live mob, one placeholder hit's damage is deducted from
    /// the server's tracked HP. Reaching zero no longer declares death: it
    /// arms [`GameSession::mob_defeat_pending`], and the client hook's
    /// `StockUnit::onDead` record (or the [`MOB_DEATH_GRACE`] fallback) is what
    /// actually runs XP, loot and the `(07,05)` CEM remove. The killer's own
    /// client owns the death animation; an immediate remove would pop the mob
    /// mid-animation, so the remove rides the death record too. A drop is never
    /// attacked: the client picks it up with the `0x06` activate, which routes
    /// through [`interact_click`](Self::interact_click) to
    /// [`pick_up_drop`](Self::pick_up_drop).
    fn attack_use(&mut self, inner: &[u8], attack: &AttackUse) {
        let hp_word = self.avatar_hp_word();
        let ack = attack_ack(inner, hp_word);
        if !ack.is_empty() {
            self.send(0x07, 0x35, &ack[2..]);
        }
        // A `0x50` that names the owner's own gnome is the active-skill
        // target (TargetType=HENCHMAN), not an attack swing. The ack above is
        // already sent, so only the convert window remains.
        if self
            .gnome
            .as_ref()
            .is_some_and(|g| attack.target == 0 || attack.target == g.entity_id)
        {
            self.gnome_activate();
            return;
        }
        // PURE-NATIVE MOB MODEL (bible §14.6 round 6n): the client's own
        // `MonsterBehavior2` brain proximity-aggros, chases to melee and attacks
        // with animation entirely on its own — the `0x50` attack path must NOT
        // enroll the mob. The deferred `0x64` FollowClient burst was the breaker:
        // it parked the mob in client-control mode chasing the avatar's centre
        // (run-through / foot-slide) so the native melee swing never played. The
        // `0x50` leg only acks and charges the server's placeholder HP, which is
        // now just a fallback timer (the client hook reports the real death).
        let armed = {
            let Some(mob) = self.mob_state.get_mut(&attack.target) else {
                return;
            };
            if self.mob_defeat_pending.contains_key(&attack.target) {
                return;
            }
            let damage = per_hit_damage(mob.hp_wire);
            mob.hp = mob.hp.saturating_sub(damage);
            if mob.hp == 0 {
                self.mob_defeat_pending
                    .insert(attack.target, Instant::now());
                true
            } else {
                false
            }
        };
        if armed {
            eprintln!(
                "game {}: mob eid={:#06x} placeholder HP spent by slot {:#04x}; \
                 awaiting client death telemetry (fallback {:?})",
                self.conn_id, attack.target, attack.slot, MOB_DEATH_GRACE
            );
        }
    }

    /// The loot phase's kill hook: roll one drop and ship its world entity.
    ///
    /// The drop is a normal `ItemObject` world entity at the mob's own death
    /// pose (x256). It is shipped as one combined `0x08` createinit whose
    /// classname is `ItemObject`, whose init body carries the real item GC
    /// class and then the item's own `Item::readData` body
    /// ([`DroppedItemBody`], the simple consumable form or the equipment form),
    /// and which ends on the `0x06` stream terminator -- the live drop's exact
    /// shape (see [`build_item_object_create_init`]). The owner name is the
    /// session's login, a per-session value. `drop_eid -> item_class` is
    /// recorded so a later `0x50` naming it is the pickup, not a mob hit.
    ///
    /// # Spawn animation
    ///
    /// The visible "pop and settle" of a native drop is client-side, driven by
    /// the init's own bytes, not by any action frame: the client's
    /// `ItemObject::updateState @0x58A410` runs `state == 0` through
    /// `initState(1) @0x58A1E0`, which attaches a `FlipController` and plays
    /// the drop in. `ItemObject::addToWorld @0x58A0A0` likewise picks the
    /// animated `initState(0)` only when the init's `+0x118` flag byte is set.
    /// [`build_item_object_create_init`] therefore ships the live drop's own
    /// `state = 0`, `+0x118 = 1`, `timer = 27000`. An `ItemObject` is a
    /// `WorldEntity`, not a `Unit`: it has no `Behavior` component, so the
    /// `SpawnAnimation`/`Spawn` actions cannot reach it.
    ///
    /// Fails closed and silent: with no avatar bound, no drop eid to allocate,
    /// or a builder refusal, nothing is sent and the kill simply leaves no
    /// drop. One drop per kill.
    /// Open the authored `DoorsToOpenOnDeath` when a boss/guard dies. Doors
    /// are matched by the class's own `Name` property (BossGate declares
    /// `Name = "Boss00ExitGate"`); the client's `Door::processUpdate` opens on
    /// type `0x0A`.
    fn open_doors_on_death(&mut self, mob_classname: &str) {
        let Some(names) = self
            .shared
            .class_description_property(mob_classname, "DoorsToOpenOnDeath")
        else {
            return;
        };
        let mut matches = Vec::new();
        for name in names
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            for placed in &self.placed_entities {
                if placed.kind != drf_content::PlacedKind::Door {
                    continue;
                }
                let Some(door_name) = self
                    .shared
                    .class_description_property(&placed.classname, "Name")
                else {
                    continue;
                };
                if door_name.eq_ignore_ascii_case(name) {
                    matches.push((placed.entity_id, door_name));
                }
            }
        }
        for (entity_id, door_name) in matches {
            self.open_door(entity_id);
            eprintln!(
                "game {}: door {door_name} opened on death of {mob_classname}",
                self.conn_id
            );
        }
    }

    /// Send one door's open update. `Door::processUpdate(0x0A)` flips the
    /// door byte and clears its blocking bit; the trailing sync byte is the
    /// CEM frame's normal `EntitySynchInfo` byte.
    fn open_door(&mut self, entity_id: u16) {
        if let Some(placement_index) = self
            .placed_entities
            .iter()
            .find(|placed| placed.entity_id == entity_id)
            .map(|placed| placed.placement_index)
        {
            self.mark_door_opened(placement_index);
        }
        let mut update = Vec::with_capacity(5);
        update.extend_from_slice(&entity_id.to_le_bytes());
        update.push(0x0A);
        update.push(0x00);
        update.push(drf_proto::cem::END_MARKER);
        self.send(0x07, 0x03, &update);
    }

    /// Apply an authored `UnlockObject` gate. Unlocked classes proceed; a
    /// locked class with no gate is scripted (boss/death gate) and refuses a
    /// click. A locked class with a gate proceeds only when the bag holds the
    /// item, consuming it when `ConsumedOnUnlock` says so (the content default
    /// is true).
    fn unlock_allows_activation(&mut self, classname: &str) -> bool {
        let locked = self.shared.class_is_locked(classname);
        let requirement = self.shared.class_unlock_requirement(classname);
        let Some(requirement) = requirement else {
            return !locked;
        };
        if self.bag_count(&requirement.object) == 0 {
            eprintln!(
                "game {}: {classname} locked; missing {}",
                self.conn_id, requirement.object
            );
            return false;
        }
        if requirement.consumed {
            self.consume_bag_class(&requirement.object, 1);
        }
        eprintln!(
            "game {}: {classname} unlocked with {}{}",
            self.conn_id,
            requirement.object,
            if requirement.consumed {
                " (consumed)"
            } else {
                ""
            }
        );
        true
    }

    /// Run the creature's authored `OnDeath` procs. Each proc rolls its own
    /// `Description.Chance` percent, then spawns every `SpellSpawnEffect`
    /// `SpawnUnit` at the death pose. The bonus-stage teleport rows name
    /// world objects, not creatures, and `mob_summon_profile` skips them.
    fn trigger_on_death_summons(&mut self, classname: &str, level: u8, pose: [i32; 4]) {
        let procs = self.shared.on_death_summons(classname);
        for proc in procs {
            if proc.chance < 100 && (self.loot_rng.generate() % 100 + 1) > proc.chance {
                continue;
            }
            for (unit, offset) in proc.units {
                if !self.shared.class_is_creature(&unit) {
                    continue;
                }
                let summon_level = (i32::from(level) + offset).clamp(1, 100) as u8;
                self.spawn_summoned_mob(&unit, summon_level, pose);
            }
        }
    }

    /// Run an NCI's `Description.SpellEffect` spawn (the relic guardian
    /// chests' `SummonRelicGuardian`). The client runs the effect's stun/sound
    /// locally; the server owns the spawned creature so it can be attacked and
    /// drop its authored `TreasureGenerator` relic. Returns whether at least
    /// one creature was shipped.
    fn trigger_nci_summons(&mut self, classname: &str, level: u8, pose: [i32; 4]) -> bool {
        let summons = self.shared.spell_effect_summons(classname);
        let mut spawned = false;
        for (unit, offset) in summons {
            if !self.shared.class_is_creature(&unit) {
                continue;
            }
            let summon_level = (i32::from(level) + offset).clamp(1, 100) as u8;
            spawned |= self.spawn_summoned_mob(&unit, summon_level, pose);
        }
        spawned
    }

    /// Ship one runtime creature spawn using the same create/init/components,
    /// SpawnAction and MoverUpdate sequence the join uses for seeded mobs.
    fn spawn_summoned_mob(&mut self, classname: &str, level: u8, pose: [i32; 4]) -> bool {
        let Some((hp_wire, behavior, manipulators)) =
            self.shared.mob_summon_profile(classname, level)
        else {
            eprintln!(
                "game {}: summon {classname} -> skipped (no creature profile)",
                self.conn_id
            );
            return false;
        };
        let wire_pose = [
            pose[0] as u32,
            pose[1] as u32,
            pose[2] as u32,
            pose[3] as u32,
        ];
        let scale = difficulty_scale(self.difficulty);
        let hp_wire = scaled_hp_wire(hp_wire, scale);
        let current_hp = (!scale.is_normal()).then_some(hp_wire);
        let Ok(init) = mob_class_state_scaled(wire_pose, level, current_hp, scale) else {
            return false;
        };
        let (entity_id, behavior_id, skills_id, manipulators_id, modifiers_id) = {
            let Ok(mut world) = self.shared.world.lock() else {
                return false;
            };
            (
                world.alloc_entity(),
                world.alloc_component(),
                world.alloc_component(),
                world.alloc_component(),
                world.alloc_component(),
            )
        };
        let behavior_class = drf_content::mob_behavior_class(behavior.as_deref(), &manipulators);
        let records = [
            (
                behavior_id,
                behavior_class.as_str(),
                mob_behavior_class_state(),
            ),
            (skills_id, MOB_SKILLS_CLASS, mob_skills_class_state()),
            (
                manipulators_id,
                MOB_MANIPULATORS_CLASS,
                mob_manipulators_class_state(&manipulators),
            ),
            (
                modifiers_id,
                MOB_MODIFIERS_CLASS,
                mob_modifiers_class_state(),
            ),
        ];
        let mut components = Vec::new();
        for (cid, class, state) in records {
            if let Ok(record) = mob_component_record(entity_id, cid, class, &state) {
                components.push(record);
            }
        }
        // A runtime create is ONE CEM message: `[07][01 create][02 init]
        // [32 components][35 spawn][35 mover][06]`. The client's
        // `ClientEntityManager::processMessage` reads type bytes until the
        // message ends with `0x06`; sending each record as its own
        // END-less frame makes it read past the payload and latch CEM error 2
        // (`Unexpected message size: 0`). This is the same single-message
        // shape `build_entity_snapshot` uses for the Bling Gnome.
        let Ok(create) = create_body(entity_id, classname) else {
            return false;
        };
        let mut inner = Vec::new();
        inner.push(0x07);
        inner.push(0x01);
        inner.extend_from_slice(&create);
        inner.push(0x02);
        inner.extend_from_slice(&entity_id.to_le_bytes());
        inner.extend_from_slice(&init);
        for record in components {
            if record.first() == Some(&0x32) {
                inner.push(0x32);
                inner.extend_from_slice(&record[1..]);
            }
        }
        inner.push(0x35);
        inner.extend_from_slice(&mob_spawn_action_payload(
            behavior_id,
            pose,
            entity_id,
            hp_wire,
        ));
        inner.push(0x35);
        inner.extend_from_slice(&mob_mover_update_payload(behavior_id, pose, hp_wire));
        inner.push(drf_proto::cem::END_MARKER);
        self.send_inner(&inner);
        self.mob_state.insert(
            entity_id,
            TrackedMob {
                spawn_index: SUMMONED_MOB_SPAWN_INDEX,
                hp_wire,
                hp: hp_wire / 256,
                classname: classname.to_string(),
                level,
                pose,
            },
        );
        eprintln!(
            "game {}: summoned {classname} eid={entity_id:#06x} L{level} hp={hp_wire} at {pose:?}",
            self.conn_id
        );
        true
    }

    /// The character level used for loot rolls. Capped to the u8 wire level;
    /// `selected_level` is normally 1..=100.
    fn player_loot_level(&self) -> u8 {
        u8::try_from(self.selected_level).unwrap_or(u8::MAX).max(1)
    }

    fn on_mob_death(&mut self, _eid: u16, pose: [i32; 4], classname: &str, mob_level: u8) {
        let Some(_avatar_eid) = self.avatar_eid else {
            return;
        };
        // Loot rolls use the character's level, not the zone/mob spawn level.
        // Capture it before `award_kill_experience`, so a level-up from this
        // kill cannot retroactively raise the drop that caused it.
        let loot_level = self.player_loot_level();
        // Kill objectives are server-credited from the same authoritative
        // death record that drives XP/loot. The client's own KillObjective
        // listener is not reliable for these placeholder mobs, so the server
        // owns the counter and restates the held quest.
        self.credit_kill_quests(classname);
        self.open_doors_on_death(classname);
        self.trigger_on_death_summons(classname, mob_level, pose);
        self.drop_kill_quest_items(classname, pose);
        // The client already self-awarded XP for this kill locally; the server
        // never sends a `0x0F` (that would double-award). It mirrors the same
        // arithmetic to keep its persisted level/XP in lockstep so the avatar
        // re-send on a zone transfer carries the client's true progress.
        self.award_kill_experience(classname, mob_level);

        // `pose` is the mob's own death pose the client hook reported (or its
        // spawn anchor on the fallback path). It is NOT the player's position:
        // the mob can die across the room, and the old player-pos substitution
        // dropped loot wherever the killer happened to stand. Only a degenerate
        // (0,0) record falls back to the last tracked mover position, then to
        // the pose we were given. Z/heading come from the unit too.
        let (x, y) = if pose[0] == 0 && pose[1] == 0 {
            self.player_pos
                .map(|[px, py]| (px, py))
                .unwrap_or((pose[0], pose[1]))
        } else {
            (pose[0], pose[1])
        };
        let z = if pose[2] == 0 {
            self.last_combat_z.filter(|z| *z != 0).unwrap_or(pose[2])
        } else {
            pose[2]
        };
        let pose = [x, y, z, pose[3]];

        let drops = if self.shared.content_loot() {
            let slots = self.shared.treasure_slots(classname);
            if slots.is_empty() {
                eprintln!(
                    "game {}: mob {classname} states no TreasureGenerator; no loot",
                    self.conn_id
                );
                return;
            }
            let loot_factor = difficulty_scale(self.difficulty).loot;
            self.shared.generate_slots_scaled(
                &slots,
                loot_level,
                &mut self.loot_rng,
                false,
                loot_factor,
            )
        } else {
            let (item_class, item_level) = self.roll_loot(loot_level, classname);
            vec![LootItem {
                class: item_class,
                level: item_level.unwrap_or(loot_level),
                modifiers: Vec::new(),
                amount: None,
            }]
        };

        for item in &drops {
            if let Some(drop_eid) = self.spawn_loot_item(item, pose) {
                eprintln!(
                    "game {}: mob {classname} dropped {} L{} ({} mods{}) as eid={drop_eid:#06x} at {pose:?}",
                    self.conn_id,
                    item.class,
                    item.level,
                    item.modifiers.len(),
                    item.amount
                        .map(|amount| format!(", {amount} gold"))
                        .unwrap_or_default(),
                );
            }
        }
    }

    /// Finalize one mob's death exactly once: mirror XP, ship the drop at the
    /// reported death pose, and (on the client-record path) send the `(07,05)`
    /// CEM remove.
    ///
    /// Called from the client death record (the normal path) or from the
    /// [`MOB_DEATH_GRACE`] timeout (the leak guard). The caller has already
    /// removed the id from [`GameSession::mob_state`], so a late duplicate
    /// record cannot reach here.
    fn finalize_mob_death(
        &mut self,
        eid: u16,
        pose: [i32; 4],
        classname: &str,
        mob_level: u8,
        send_remove: bool,
    ) {
        self.on_mob_death(eid, pose, classname, mob_level);
        if send_remove {
            // The remove is a separate `(07,05)` CEM frame, not the pickup's
            // `05` sub-tag inside a `(07,35)` stream. Only the client death
            // record proves the entity is still in the CEM corpse linger; the
            // timeout fallback may fire after the client already purged it, and
            // a remove for an unknown id latches CEM error 6.
            self.send_inner(&remove_inner(eid));
        }
    }

    /// Apply one hook record. Death is authoritative; HP only updates the
    /// tracked word and arms the same fallback the placeholder counter arms.
    fn apply_hook_event(&mut self, event: HookEvent) {
        match event {
            HookEvent::Death(death) => {
                let Some(mob) = self.mob_state.remove(&death.eid) else {
                    // Not ours (another session / already finalized / unknown
                    // id): wire ids are globally unique, so this is safe.
                    return;
                };
                self.mob_defeat_pending.remove(&death.eid);
                if mob.spawn_index != SUMMONED_MOB_SPAWN_INDEX {
                    self.mark_mob_defeated(mob.spawn_index);
                }
                self.mob_entities.retain(|mob| mob.entity_id != death.eid);
                let level = if death.level == 0 {
                    mob.level
                } else {
                    death.level
                };
                eprintln!(
                    "game {}: mob {} eid={:#06x} client death L{} pose={:?}",
                    self.conn_id, mob.classname, death.eid, level, death.pose
                );
                self.finalize_mob_death(death.eid, death.pose, &mob.classname, level, true);
            }
            HookEvent::Hp { eid, hp_wire } => {
                // The avatar's own HP is client-authoritative; a zero record
                // is the death, and the server answers it with the same
                // remove + respawn transfer the RespawnDialog's request
                // triggers. Mob HP keeps the placeholder fallback below.
                if Some(eid) == self.avatar_eid {
                    if hp_wire == 0 {
                        self.respawn_request();
                    }
                    return;
                }
                if let Some(mob) = self.mob_state.get_mut(&eid) {
                    mob.hp = hp_wire / 256;
                    if mob.hp == 0 {
                        self.mob_defeat_pending
                            .entry(eid)
                            .or_insert_with(Instant::now);
                    }
                }
            }
        }
    }

    /// Drain queued hook records without blocking. Called once per read-loop
    /// iteration, including on read timeouts.
    fn poll_hook_telemetry(&mut self) {
        while let Ok(event) = self.hook_rx.try_recv() {
            self.apply_hook_event(event);
        }
    }

    /// Finalize any mob whose death grace window has elapsed without a client
    /// death record, so a missing hook cannot leak the entity.
    fn check_mob_timeouts(&mut self) {
        self.check_mob_timeouts_at(Instant::now());
    }

    fn check_mob_timeouts_at(&mut self, now: Instant) {
        let expired: Vec<u16> = self
            .mob_defeat_pending
            .iter()
            .filter(|(_, armed)| now.saturating_duration_since(**armed) >= MOB_DEATH_GRACE)
            .map(|(&eid, _)| eid)
            .collect();
        for eid in expired {
            let Some(mob) = self.mob_state.remove(&eid) else {
                self.mob_defeat_pending.remove(&eid);
                continue;
            };
            self.mob_defeat_pending.remove(&eid);
            if mob.spawn_index != SUMMONED_MOB_SPAWN_INDEX {
                self.mark_mob_defeated(mob.spawn_index);
            }
            self.mob_entities.retain(|mob| mob.entity_id != eid);
            let pose = mob.pose;
            let classname = mob.classname;
            let level = mob.level;
            if self.hook_telemetry {
                // Trusted sessions normally wait for the client's death
                // record. When it never arrives (champion/hero deaths are the
                // observed gap), finalize the authored death as a fallback:
                // without this the elite drops and `DoorsToOpenOnDeath` are
                // both lost. Do not send a remove; the client may already have
                // purged the corpse and a remove would latch CEM error 6.
                eprintln!(
                    "game {}: mob {classname} eid={eid:#06x} no death record within \
                     {:?}; finalizing without remove",
                    self.conn_id, MOB_DEATH_GRACE
                );
                self.finalize_mob_death(eid, pose, &classname, level, false);
                continue;
            }
            eprintln!(
                "game {}: mob {classname} eid={eid:#06x} placeholder death timeout; \
                 finalizing at spawn pose {pose:?}",
                self.conn_id
            );
            self.finalize_mob_death(eid, pose, &classname, level, false);
        }
    }

    /// Mirror the client's self-awarded kill XP into the persisted character.
    ///
    /// The client owns XP (`Hero::onAddExperience` @`0x4F82F0` runs locally on
    /// the kill); the server recomputes the same award from the mob's spawn
    /// level and the player's level (`xp_per_kill`) and folds it in
    /// (`apply_xp`), granting 5 attribute points per level and refreshing the
    /// level-scaled HP word so every later synch trailer stays in lockstep.
    ///
    /// No server message is emitted: a `(07,03)` `0x0F` would double-award the
    /// client that already applied it. The reconciled level/XP ship with the
    /// next avatar `0x02` init (zone transfer / re-join).
    fn award_kill_experience(&mut self, classname: &str, mob_level: u8) {
        let player_level = u32::from(self.selected_level);
        let Some(creature) = self.shared.creature_xp_info(classname) else {
            eprintln!(
                "game {}: mob {classname} granted no XP: game.pkg does not settle its UnitDesc",
                self.conn_id
            );
            return;
        };
        let gained = drf_content::experience::xp_per_kill_with(
            &creature,
            u32::from(mob_level),
            player_level,
            drf_content::EXPERIENCE_MOD_DEFAULT,
        );
        if gained == 0 {
            return;
        }
        let outcome = drf_content::apply_xp(player_level, self.experience, gained);
        self.experience = outcome.experience;
        self.selected_level = u16::try_from(outcome.level).unwrap_or(u16::MAX);
        if outcome.levels_gained > 0 {
            self.attribute_points = self
                .attribute_points
                .saturating_add(outcome.levels_gained * drf_content::ATTRIBUTE_POINTS_PER_LEVEL);
            self.skill_points = self.skill_points.saturating_add(outcome.levels_gained);
            if let Some(skills_cid) = self.skills_cid {
                self.send_updates(&[build_skills_points_update(skills_cid, self.skill_points)]);
            }
            if let Some(avatar_class) = self.avatar_class.as_deref() {
                // Vitals: the client refills to the new level's max on
                // level-up; mirror the same level-scaled wire word.
                let hp = drf_proto::avatar_hp_wire_at_level(avatar_class, outcome.level);
                self.avatar_hp_wire.store(hp, Ordering::Relaxed);
            }
        }
        self.persist_progress();
        eprintln!(
            "game {}: kill vs L{mob_level} mob -> +{gained} XP (L{} xp={}, {} level(s), {} pts)",
            self.conn_id,
            self.selected_level,
            self.experience,
            outcome.levels_gained,
            self.attribute_points
        );
    }

    /// Write the session's mirrored leveling progress back to the store.
    fn persist_progress(&self) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        if let Ok(mut store) = self.shared.store.lock() {
            if let Err(err) = store.set_progress_and_skill_points(
                &account,
                sqlid,
                u32::from(self.selected_level),
                self.experience,
                self.attribute_points,
                self.skill_points,
            ) {
                eprintln!(
                    "game {}: progress persist for sqlid {sqlid} failed: {err}",
                    self.conn_id
                );
            }
        }
    }

    /// Write the session's unspent pool and four allocated stats back to the
    /// store. Called only after a spend has been validated against the pool, so
    /// the persisted `5 * (level - 1)` split always matches what was confirmed.
    fn persist_attributes(&self) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        if let Ok(mut store) = self.shared.store.lock() {
            if let Err(err) = store.set_attribute_allocation(
                &account,
                sqlid,
                self.attribute_points,
                self.attributes,
            ) {
                eprintln!(
                    "game {}: attribute persist for sqlid {sqlid} failed: {err}",
                    self.conn_id
                );
            }
        }
    }

    /// Claim the drop `drop_eid` in answer to the click `inner` (a c2s `07 34`
    /// `0x06` activate).
    ///
    /// The ack, the ground-entity remove and the bag add ride ONE `(07,35)`
    /// stream: the shared activate ack ([`activate_ack_body`]) carrying the
    /// avatar's own HP word, then a `05 <drop_eid>` remove sub-tag, then the
    /// UnitContainer `0x1E` item add ([`author_bag_item`]), then `06`.
    ///
    /// The remove is a sub-tag INSIDE the stream, not a separate `(07,05)`
    /// frame, and the ack's synch word is the avatar's, not a fixed literal: a
    /// mismatched avatar HP fatals in `EntitySynchInfo::Validate`@0x5DD900
    /// (`Last EntityManager error: 1`, ZoneClient status 117) and the split
    /// frames wedge the action state machine. There is NO separate acquire
    /// frame: CEM tag `0x33` is a `processComponentInit` (@0x5DB460) that reads
    /// a `u16 componentId`, so the drop's eid there is an invalid component id
    /// and faults the client. A drop that cannot be bagged (full bag / no
    /// game.pkg record) is left on the floor but still acked, so the click never
    /// wedges.
    fn pick_up_drop(&mut self, inner: &[u8], drop_eid: u16) {
        let Some(drop) = self.drops.remove(&drop_eid) else {
            return;
        };
        let Some(avatar_eid) = self.avatar_eid else {
            self.drops.insert(drop_eid, drop);
            return;
        };
        let hp = self.avatar_hp_word();
        let Some(mut body) = activate_ack_body(inner, hp) else {
            return;
        };

        if let Some(amount) = drop.amount {
            let Some(container_cid) = self.avatar_container_cid else {
                self.drops.insert(drop_eid, drop);
                return;
            };
            if let Ok(amount_i32) = i32::try_from(amount) {
                self.gold = self.gold.saturating_add(amount);
                self.persist_gold(self.gold);
                body.push(0x05);
                body.extend_from_slice(&drop_eid.to_le_bytes());
                let currency =
                    build_container_add_currency_notify(container_cid, amount_i32, avatar_eid);
                body.extend_from_slice(&currency[1..]);
                body.push(0x02);
                body.extend_from_slice(&hp.to_le_bytes());
                body.push(drf_proto::cem::END_MARKER);
                self.send(0x07, 0x35, &body);
                eprintln!(
                    "game {}: pickup drop eid={drop_eid:#06x} -> {amount} gold",
                    self.conn_id
                );
                return;
            }
        }

        let item_class = drop.item_class.clone();
        let item_level = drop.item_level;
        let modifiers: Vec<(String, u8)> = drop
            .modifiers
            .iter()
            .map(|modifier| (modifier.class.clone(), modifier.level))
            .collect();
        let add = match item_level {
            Some(level) => self.author_bag_item_at_level_with_mods(
                &item_class,
                level,
                false,
                false,
                &modifiers,
            ),
            None => self.author_bag_item_with_mods(&item_class, false, false, &modifiers),
        };
        let Some(add) = add else {
            // Left on the floor: ack alone (the action state machine still needs
            // its response) and keep tracking the drop so a later click can
            // retry.
            body.push(drf_proto::cem::END_MARKER);
            self.send(0x07, 0x35, &body);
            eprintln!(
                "game {}: pickup drop eid={drop_eid:#06x} -> {item_class} HELD (not bagged)",
                self.conn_id
            );
            self.drops.insert(drop_eid, drop);
            return;
        };
        // One stream: activate ack, the ground-entity remove as a `05 <eid>`
        // sub-tag, then the item add. `add` opens `07 35 ...`; its leading `07`
        // is dropped so the envelope's `35` stays the update tag.
        body.push(0x05);
        body.extend_from_slice(&drop_eid.to_le_bytes());
        body.extend_from_slice(&add[1..]);
        body.push(0x02);
        body.extend_from_slice(&hp.to_le_bytes());
        body.push(drf_proto::cem::END_MARKER);
        self.send(0x07, 0x35, &body);
        self.restate_item_objectives(&item_class);
        eprintln!(
            "game {}: pickup drop eid={drop_eid:#06x} -> {item_class}",
            self.conn_id
        );
    }

    /// Roll one item class for a kill.
    ///
    /// The binary rolls the content `ItemGenerator` (`HeroIG` for normal
    /// mobs, `ChampionIG`/`LegendIG` for rank 3 and bosses) at the character's
    /// level, so weapons, armour and other item kinds appear and carry the
    /// generated level. Tests with no content, and an empty generator result,
    /// fall back to the reference S3 table's three classes.
    fn roll_loot(&mut self, loot_level: u8, classname: &str) -> (String, Option<u8>) {
        if self.shared.content_loot() {
            let lower = classname.to_ascii_lowercase();
            // Rank 1/2 use the hero table; rank 3 and bosses the champion
            // table. Never the legend table on a normal kill.
            let generator = if lower.contains("boss") || lower.contains("rank3") {
                "ChampionIG"
            } else {
                "HeroIG"
            };
            let seed = self.loot_cursor as u64;
            self.loot_cursor = self.loot_cursor.wrapping_add(1);
            // Prebuilt items carry a class-authored modifier list that the
            // ground `ItemObject` body does not reproduce; drawing one
            // desyncs the client's `Item::readData`. Take the first ordinary
            // (non-prebuilt) class from the same draw instead.
            let items = self
                .shared
                .loot_items(generator, u16::from(loot_level.max(1)), 1, seed);
            if let Some((class, level)) = items.into_iter().next() {
                return (class, Some(level.max(1)));
            }
        }
        let item_class = LOOT_TABLE[self.loot_cursor % LOOT_TABLE.len()].to_string();
        self.loot_cursor = self.loot_cursor.wrapping_add(1);
        (item_class, None)
    }

    /// The c2s Interact-06 click. A tracked ground drop is claimed with the
    /// combined activate-ack stream ([`pick_up_drop`](Self::pick_up_drop));
    /// every other click (NPC / portal / quest giver) is answered with the bare
    /// activate ack ([`activate_ack_body`]) carrying the avatar's HP word, and
    /// then, when the clicked entity's own placement states a `Zone`, the click
    /// is a request to travel and the transfer fires. Otherwise, when the
    /// clicked NPC is a giver that lists exactly one offered quest, the 11B
    /// SetOfferedQuest arms its Accept. An NPC that lists none or several is
    /// answered with the ack alone.
    fn interact_click(&mut self, inner: &[u8], click: &InteractClick) {
        // A ground drop is claimed by the SAME `0x06` activate the client uses
        // for every click (C# checks IsDroppedItem before portals/NPCs). Its ack
        // is not sent here: it rides the same `(07,35)` stream as the ground
        // remove and the item add.
        if self.drops.contains_key(&click.target) {
            self.pick_up_drop(inner, click.target);
            return;
        }
        // Every other activate gets the shared activate ack, whose synch trailer
        // must carry the avatar's own HP word.
        if let Some(mut ack) = activate_ack_body(inner, self.avatar_hp_word()) {
            ack.push(drf_proto::cem::END_MARKER);
            self.send(0x07, 0x35, &ack);
        }
        if let Some(zone) = self.town_portals.remove(&click.target) {
            let dest = Destination {
                zone,
                spawn: String::new(),
            };
            self.begin_zone_transfer(&dest);
            return;
        }
        let clicked = self
            .placed_entities
            .iter()
            .find(|placed| placed.entity_id == click.target)
            .cloned();
        if let Some(placed) = &clicked {
            if placed.kind == drf_content::PlacedKind::Portal {
                if let Some(zone) = placed.zone.clone() {
                    let dest = Destination {
                        zone,
                        spawn: placed.spawn_point.clone().unwrap_or_default(),
                    };
                    if self.begin_zone_transfer(&dest) {
                        return;
                    }
                }
            }
            if let Some(dest) =
                self.shared
                    .zone_destination(&self.zone, None, Some(&placed.classname), placed.wire)
            {
                self.begin_zone_transfer(&dest);
                return;
            }
            if placed.kind == drf_content::PlacedKind::Portal {
                return;
            }
        }
        // Doors have their own update protocol (`Door::processUpdate(0x0A)`),
        // not the NCI activator body. The authored UnlockObject is the same
        // item gate for both; death gates state Locked with no UnlockObject
        // and are opened only by `DoorsToOpenOnDeath`.
        if let Some(placed) = &clicked {
            if placed.kind == drf_content::PlacedKind::Door {
                if self.unlock_allows_activation(&placed.classname) {
                    self.open_door(placed.entity_id);
                    eprintln!(
                        "game {}: door {} opened by click",
                        self.conn_id, placed.classname
                    );
                }
                return;
            }
        }
        // Unit-rooted interactives (chests, shrines, NCIs, teleports) are
        // activated by an entity update, not the NPC/quest path. The client's
        // NonCombatInteractive::processUpdate consumes opcode 0x0A and one
        // target entity id, then applies the class's authored SpellEffect.
        // Chests additionally run their authored ItemGenerators server-side.
        if let Some(placed) = &clicked {
            if placed.kind == drf_content::PlacedKind::WorldEntityUnit {
                if self.unlock_allows_activation(&placed.classname) {
                    self.activate_nci(placed.entity_id, &placed.classname);
                }
                return;
            }
        }
        let Some(qref) = self.quest_cid else {
            return;
        };
        let Some(classname) = clicked.map(|placed| placed.classname) else {
            return;
        };
        let qids = self.offered_by_npc(&classname);
        if qids.len() != 1 {
            return;
        }
        let qid = qids[0];
        let ack = build_quest_offer_ack(&quest_gc_type(qid), qref);
        self.send(0x07, 0x35, &ack[2..]);
        self.dlg = Some((qid, click.target));
    }

    /// Run a held quest's nested `KillDropTrigger`s for one death. The trigger
    /// is content, not a quest special case: its own `MonsterType` selects the
    /// kill and its `Chance` is rolled per matching death. The objective's bag
    /// count suppresses a second copy while the player already has one.
    fn drop_kill_quest_items(&mut self, classname: &str, pose: [i32; 4]) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let Some(corpus) = self.shared.quest_corpus() else {
            return;
        };
        let held: Vec<(u32, u32)> = self.quest_instance.iter().map(|(&q, &i)| (q, i)).collect();
        let mut pending = Vec::new();
        for (qid, _instance) in held {
            let Some(quest) = corpus.quest_by_qid(qid) else {
                continue;
            };
            let record = self
                .shared
                .store
                .lock()
                .ok()
                .and_then(|store| store.quest_get(&account, sqlid, qid));
            let Some(record) = record else {
                continue;
            };
            for (index, objective) in quest.objectives.iter().enumerate() {
                if objective.kill_drops.is_empty() {
                    continue;
                }
                if self.objective_satisfied(objective) {
                    continue;
                }
                let current = self
                    .objective_current(objective, record.objectives.get(index))
                    .unwrap_or(0);
                if current >= objective.required.max(1) {
                    continue;
                }
                for drop in &objective.kill_drops {
                    if !self.shared.class_is_kind_of(classname, &drop.monster) {
                        continue;
                    }
                    let held = self.bag_count(&drop.item)
                        + self
                            .drops
                            .values()
                            .filter(|state| state.item_class.eq_ignore_ascii_case(&drop.item))
                            .count() as u32;
                    if held >= objective.required.max(1) {
                        continue;
                    }
                    if drop.chance < 100 && (self.loot_rng.generate() % 100 + 1) > drop.chance {
                        continue;
                    }
                    pending.push(drop.item.clone());
                }
            }
        }
        for item_class in pending {
            if self.spawn_ground_drop(&item_class, 1, pose).is_some() {
                eprintln!(
                    "game {}: quest kill drop {item_class} from {classname}",
                    self.conn_id
                );
            }
        }
    }

    /// Drop the items a held quest's nested `ActivateDropTrigger` names when
    /// the matching world entity is activated (the Hermit's Lockbox gives the
    /// Hermit's Hoop). The bag remains the objective's authority; this only
    /// gives the player the ItemObject to pick up.
    fn activate_quest_drops(&mut self, entity_id: u16, classname: &str) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let Some(corpus) = self.shared.quest_corpus() else {
            return;
        };
        let held: Vec<(u32, u32)> = self.quest_instance.iter().map(|(&q, &i)| (q, i)).collect();
        let pose = self
            .placed_entities
            .iter()
            .find(|placed| placed.entity_id == entity_id)
            .and_then(|placed| {
                (placed.init.len() >= 20).then(|| {
                    [
                        i32::from_le_bytes(placed.init[4..8].try_into().unwrap()),
                        i32::from_le_bytes(placed.init[8..12].try_into().unwrap()),
                        i32::from_le_bytes(placed.init[12..16].try_into().unwrap()),
                        i32::from_le_bytes(placed.init[16..20].try_into().unwrap()),
                    ]
                })
            })
            .unwrap_or([0, 0, 0, 0]);
        for (qid, _instance) in held {
            let Some(quest) = corpus.quest_by_qid(qid) else {
                continue;
            };
            for objective in &quest.objectives {
                for drop in &objective.activate_drops {
                    if !self.shared.class_is_kind_of(classname, &drop.entity) {
                        continue;
                    }
                    let already_held = self.shared.store.lock().ok().is_some_and(|store| {
                        store.get_char(&account, sqlid).is_some_and(|character| {
                            character
                                .bag
                                .iter()
                                .any(|row| row.class.eq_ignore_ascii_case(&drop.item))
                        })
                    });
                    let already_dropped = self
                        .drops
                        .values()
                        .any(|state| state.item_class.eq_ignore_ascii_case(&drop.item));
                    if already_held || already_dropped {
                        continue;
                    }
                    if drop.chance < 100 && (self.loot_rng.generate() % 100 + 1) > drop.chance {
                        continue;
                    }
                    if self.spawn_ground_drop(&drop.item, 1, pose).is_some() {
                        eprintln!(
                            "game {}: quest activate drop {} from {}",
                            self.conn_id, drop.item, classname
                        );
                    }
                }
            }
        }
    }

    /// Activate a Unit-rooted NCI: ship the client's own update so its
    /// `SpellEffect` (shrine buff, chest open effect) runs, then generate and
    /// deliver the authored `ItemGenerator` slots (items and native currency).
    fn activate_nci(&mut self, entity_id: u16, classname: &str) {
        let Some(avatar_eid) = self.avatar_eid else {
            return;
        };
        let track = self
            .placed_entities
            .iter()
            .find(|placed| placed.entity_id == entity_id)
            .is_some_and(|placed| placed.track_activating_players);
        let mut update = Vec::with_capacity(13);
        update.extend_from_slice(&entity_id.to_le_bytes());
        update.push(0x0A);
        update.extend_from_slice(&u32::from(avatar_eid).to_le_bytes());
        if track {
            // `NonCombatInteractive::processUpdate` reads the activator
            // CharSQLID after the activating unit whenever the desc sets
            // `TrackActivatingPlayers` (bit 0x80). Omitting it makes the
            // client consume the sync byte and desync with Code 2.
            update.extend_from_slice(&self.selected.unwrap_or(0).to_le_bytes());
        }
        update.push(0x00); // EntitySynchInfo: no compared fields
        update.push(drf_proto::cem::END_MARKER);
        self.send(0x07, 0x03, &update);

        self.activate_quest_drops(entity_id, classname);
        self.credit_activate_quests(classname);

        // A teleport NCI (`TeleportZone`/`TeleportPoint`) runs its SpellEffect
        // locally and then asks the server to move the player. This is the
        // generic NCI activation leg, not a portal-specific click.
        if let Some(dest) = self.shared.nci_destination(classname) {
            if self.begin_zone_transfer(&dest) {
                eprintln!(
                    "game {}: NCI {classname} teleport -> {}:{}",
                    self.conn_id, dest.zone, dest.spawn
                );
                return;
            }
        }

        let placement = self
            .placed_entities
            .iter()
            .find(|placed| placed.entity_id == entity_id);
        let placement_index = placement.map(|placed| placed.placement_index);
        let pose = placement
            .and_then(|placed| {
                (placed.init.len() >= 20).then(|| {
                    [
                        i32::from_le_bytes(placed.init[4..8].try_into().unwrap()),
                        i32::from_le_bytes(placed.init[8..12].try_into().unwrap()),
                        i32::from_le_bytes(placed.init[12..16].try_into().unwrap()),
                        i32::from_le_bytes(placed.init[16..20].try_into().unwrap()),
                    ]
                })
            })
            .unwrap_or([0, 0, 0, 0]);
        let level = self.player_loot_level();

        // The relic guardian chests carry `SpellEffect = SummonRelicGuardian`;
        // the client only plays the burst/stun, so the server creates the
        // guardian here. The chest is spent once its guardian is out even
        // though it authors no `ItemGenerator` of its own.
        let summoned = self.trigger_nci_summons(classname, level, pose);

        let slots = self.shared.nci_loot_slots(classname);
        if slots.is_empty() {
            if summoned {
                if let Some(placement_index) = placement_index {
                    self.mark_place_consumed(placement_index);
                }
            }
            return;
        }
        // Some authored tables (the altars' TokenRewardRare<Class>IG plus
        // DefaultIG) can miss every chance gate. A one-shot altar that rolls
        // empty is indistinguishable from a broken object, so retry the whole
        // slot set a bounded number of times before leaving it unconsumed.
        let mut items = Vec::new();
        for _ in 0..8 {
            items = self
                .shared
                .generate_slots(&slots, level, &mut self.loot_rng, false);
            if !items.is_empty() {
                break;
            }
        }
        if items.is_empty() {
            eprintln!(
                "game {}: NCI {classname} rolled no loot after retries; leaving it active",
                self.conn_id
            );
            return;
        }
        if let Some(placement_index) = placement_index {
            self.mark_place_consumed(placement_index);
        }
        let mut drops = 0usize;
        for item in &items {
            if self.spawn_loot_item(item, pose).is_some() {
                drops += 1;
                eprintln!(
                    "game {}: NCI {classname} gave {} L{}{}",
                    self.conn_id,
                    item.class,
                    item.level,
                    item.amount
                        .map(|amount| format!(" ({amount} gold)"))
                        .unwrap_or_default()
                );
            }
        }
        eprintln!(
            "game {}: NCI {classname} gave {drops} drop(s)",
            self.conn_id
        );
    }

    /// The maze seed for a generated dungeon entry: a fresh random word in
    /// the binary, the deterministic CRC32 in tests (so expected layouts and
    /// spawn poses stay fixed).
    fn new_maze_seed(&self, zone: &str) -> u32 {
        if !self.shared.random_maze_seeds() {
            return drf_content::maze::layout_seed(zone);
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        let mut x =
            nanos ^ self.conn_id.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xBF58_476D_1CE4_E5B9;
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        (x as u32).max(1)
    }

    /// The layout seed for one generated zone in this login session. The
    /// first visit mints a seed; later visits reuse it, so leaving through a
    /// waypoint scroll and returning via the obelisk lands in the same
    /// explored dungeon. `reset_instances` (the nameplate menu) clears this
    /// map; a character selection also starts a fresh session.
    fn maze_seed_for(&mut self, zone: &str) -> u32 {
        if let Some(seed) = self.maze_seeds.get(zone) {
            return *seed;
        }
        let seed = self.new_maze_seed(zone);
        self.maze_seeds.insert(zone.to_string(), seed);
        seed
    }

    /// Build the per-zone progress record the first time this session enters a
    /// generated zone. The minimap geometry comes from the same seeded layout
    /// the client is building, so the Ready word count is the client's own
    /// `MiniMapExplored+0x40` rather than a guessed number.
    fn ensure_zone_progress(&mut self, zone: &str, seed: u32) {
        if self.zone_progress.contains_key(zone) || !self.shared.zone_is_generated(zone) {
            return;
        }
        let minimap = self.shared.maze_minimap_with_seed(zone, seed);
        let explored = minimap
            .as_ref()
            .map(|minimap| vec![0u32; minimap.words()])
            .unwrap_or_default();
        self.zone_progress.insert(
            zone.to_string(),
            ZoneProgress {
                minimap,
                explored,
                ..ZoneProgress::default()
            },
        );
    }

    /// Mark the player's current world point (x256) and its neighbours in the
    /// zone's minimap bits. The client does this locally while walking; the
    /// server mirrors it so a re-entry can restore the fog instead of starting
    /// from zero.
    fn mark_explored_position(&mut self, position: [i32; 2]) {
        let zone = self.zone.clone();
        let seed = self.maze_seed;
        if !self.zone_progress.contains_key(&zone) {
            self.ensure_zone_progress(&zone, seed);
        }
        let Some(progress) = self.zone_progress.get_mut(&zone) else {
            return;
        };
        let Some(minimap) = progress.minimap else {
            return;
        };
        minimap.mark_explored(
            &mut progress.explored,
            f64::from(position[0]) / 256.0,
            f64::from(position[1]) / 256.0,
        );
    }

    /// Remember one spawn index as killed, so a re-entry skips its entity.
    fn mark_mob_defeated(&mut self, spawn_index: usize) {
        let zone = self.zone.clone();
        self.zone_progress
            .entry(zone)
            .or_default()
            .defeated_mobs
            .insert(spawn_index);
    }

    /// Remember one placement index as opened, so re-entry reopens its door.
    fn mark_door_opened(&mut self, placement_index: usize) {
        let zone = self.zone.clone();
        self.zone_progress
            .entry(zone)
            .or_default()
            .opened_doors
            .insert(placement_index);
    }

    /// Remember one placement index as consumed, so a re-entry does not ship
    /// the chest/shrine again.
    fn mark_place_consumed(&mut self, placement_index: usize) {
        let zone = self.zone.clone();
        self.zone_progress
            .entry(zone)
            .or_default()
            .consumed_places
            .insert(placement_index);
    }

    /// The explored words to attach to Ready, only when this zone has a
    /// minimap. Cloned because `send` needs `&mut self`.
    fn explored_ready_words(&self) -> Option<Vec<u32>> {
        self.zone_progress
            .get(&self.zone)
            .and_then(|progress| progress.minimap.as_ref().map(|_| progress.explored.clone()))
    }

    /// `(09,17)` SetMonsterDifficulty: one byte, `0..=3`.
    ///
    /// The client's General Options applies the index; the reply is the
    /// `(09,55)` `processMonsterDifficultySet` record. The two reply bytes are
    /// `[index, personal]`; zero for `personal` prints the authored "You
    /// changed your group's difficulty setting" confirmation. Sending the
    /// pair in the other order made the client read the index as Normal and
    /// the mode byte as "personal".
    fn set_monster_difficulty(&mut self, payload: &[u8]) {
        let Some(&index) = payload.first() else {
            crate::opcodes::record("dispatch", 0x09, 0x17, None, payload, &self.zone);
            return;
        };
        if index > 3 {
            crate::opcodes::record("dispatch", 0x09, 0x17, None, payload, &self.zone);
            return;
        }
        self.difficulty = index;
        if let Some(sqlid) = self.selected {
            let account = self.shared.account();
            if let Ok(mut store) = self.shared.store.lock() {
                let _ = store.set_difficulty(&account, sqlid, index);
            }
        }
        self.send(0x09, 0x55, &[index, 0x00]);
        eprintln!(
            "game {}: difficulty -> {} (index {index})",
            self.conn_id,
            ["Normal", "Formidable", "Extreme", "Insane"][usize::from(index)]
        );
    }

    /// `GroupClient::resetInstances` (`(09,26)`): forget every generated
    /// layout and its progress this session explored. The next entry to each
    /// zone mints a new one; the current zone keeps its already-loaded layout
    /// until left.
    fn reset_instances(&mut self) {
        let cleared = self.maze_seeds.len();
        self.maze_seeds.clear();
        self.zone_progress.clear();
        eprintln!(
            "game {}: Reset Instances cleared {cleared} generated layout(s)",
            self.conn_id
        );
    }

    /// Fire the zone-transfer sequence for a destination-designating click:
    /// `(13,2)` transfer then `(13,0)` assign, as two separate writes so the
    /// client's per-frame function runs between them and before the tail.
    ///
    /// The client answers with its own c2s `(13,6)` Zone.Join, which is the
    /// barrier: [`join`](Self::join) then performs a FULL destination join
    /// (fresh component ids, a re-created avatar and player, the QuestManager
    /// and its journal, and the mover/ticker re-armed) and ships the
    /// destination's `(13,1)` ready and `(13,5)` instance count.
    ///
    /// Both payloads are built before a single byte is sent, so a builder
    /// that cannot produce its frame leaves the client where it is with zero
    /// frames. The zone moves with the assign because the assign IS the zone
    /// change on the wire, and the source zone's per-session state is reset
    /// here so the destination join cannot mint a frame on a component id the
    /// client is dropping.
    fn begin_zone_transfer(&mut self, dest: &Destination) -> bool {
        self.begin_zone_transfer_with_reason(dest, TRANSFER_REASON_DEFAULT)
    }

    /// [`begin_zone_transfer`](Self::begin_zone_transfer) with an explicit
    /// `(13,2)` reason. A portal hop sends `transfer`; a death/respawn sends
    /// `respawn` so the client runs its post-death transition before the
    /// assign.
    fn begin_zone_transfer_with_reason(&mut self, dest: &Destination, reason: &[u8]) -> bool {
        let origin_zone = self.zone.clone();
        let Ok(transfer) = transfer_payload(reason) else {
            return false;
        };
        let destination_zone = dest.zone.clone();
        self.maze_seed = if self.shared.zone_is_generated(&destination_zone) {
            self.maze_seed_for(&destination_zone)
        } else {
            0
        };
        let Ok(assign) = assign_payload_with_seed(&dest.zone, self.maze_seed) else {
            return false;
        };
        self.joined.store(false, Ordering::SeqCst);
        if let Ok(mut ub) = self.avatar_ub_cid.lock() {
            *ub = None;
        }
        // Serialize the interval queue against the transfer frames. The ticker
        // takes queued bodies and writes the 0x0D WorldInterval outside this
        // mutex; without holding it across the transfer, a packet queued for
        // the old zone can be written after the assign. The client then sees a
        // component update for a component it dropped during the zone load and
        // latches CEM error 9 (Invalid ComponentID).
        let mut interval_gate = self.pending_interval.lock();
        if let Ok(queue) = interval_gate.as_mut() {
            queue.clear();
        }
        self.send(0x0D, 0x02, &transfer);
        self.send(0x0D, 0x00, &assign);
        drop(interval_gate);
        self.zone = dest.zone.clone();
        let destination_zone = self.zone.clone();
        self.ensure_zone_progress(&destination_zone, self.maze_seed);
        if origin_zone != self.zone {
            // The row is the way back to where this transfer started. Keep the
            // spawn name empty so the destination's own default rung places
            // the avatar rather than a portal mouth it no longer owns.
            self.recent_zone_portal = Some((origin_zone, String::new()));
        }
        self.combat_zone
            .store(is_combat_zone(&self.zone), Ordering::SeqCst);
        self.zone_spawn_request = Some(dest.spawn.clone()).filter(|spawn| !spawn.is_empty());
        self.zone_spawn_pos = None;
        self.avatar_eid = None;
        self.player_eid = None;
        self.placed_entities.clear();
        self.mob_entities.clear();
        self.mob_state.clear();
        self.mob_defeat_pending.clear();
        self.drops.clear();
        self.gnome = None;
        self.town_portals.clear();
        self.player_pos = None;
        self.last_combat_z = None;
        self.quest_cid = None;
        self.avatar_container_cid = None;
        self.equipment_cid = None;
        self.manipulators_cid = None;
        self.skills_cid = None;
        self.skill_slots.clear();
        self.skill_catalog.clear();
        self.bag_grid = None;
        self.bag_entries.clear();
        self.cursor = None;
        self.worn.clear();
        self.dlg = None;
        self.merchant_components.clear();
        self.trainer_components.clear();
        if let Ok(mut pending) = self.pending_move.lock() {
            *pending = MoveAck::default();
        }
        if let Some(sqlid) = self.selected {
            let account = self.shared.account();
            if let Ok(mut store) = self.shared.store.lock() {
                let _ = store.set_zone(&account, sqlid, &dest.zone);
                let _ = store.record_visited_zone(&account, sqlid, &dest.zone);
                if let Some((zone, spawn)) = &self.recent_zone_portal {
                    let _ = store.set_recent_zone_portal(&account, sqlid, zone, spawn);
                }
            }
        }
        eprintln!(
            "game {}: zone transfer -> {} (spawn={})",
            self.conn_id,
            dest.zone,
            if dest.spawn.is_empty() {
                "<zone default>"
            } else {
                &dest.spawn
            }
        );
        true
    }

    /// The c2s QuestManager query (sub 0x06): the client naming which offered
    /// quest it wants. Answered with the same 11B SetOfferedQuest, but only
    /// when the queried NPC is a giver for it.
    fn quest_query(&mut self, query: &QuestQuery) {
        let Some(qref) = self.quest_cid else {
            return;
        };
        let qid = u32::from_le_bytes(query.qid[1..5].try_into().unwrap());
        let Some(classname) = self
            .placed_entities
            .iter()
            .find(|placed| u32::from(placed.entity_id) == query.npc_entity_id)
            .map(|placed| placed.classname.clone())
        else {
            return;
        };
        // A query for a quest the character already holds is a turn-in
        // view, not an offer. The client asks this way from an NPC dialog;
        // answer a ready quest with ShowTurnInDialog and leave an unready one
        // silent, exactly as the offer directory does.
        if let Some(&instance) = self.quest_instance.get(&qid) {
            if let Some(quest) = self
                .shared
                .quest_corpus()
                .and_then(|corpus| corpus.quest_by_qid(qid).cloned())
            {
                if self.quest_ready(&quest, instance, qid) {
                    let ack = build_quest_ready_ack(qref, instance);
                    self.send(0x07, 0x35, &ack[2..]);
                }
            }
            return;
        }
        if !self.offered_by_npc(&classname).contains(&qid) {
            return;
        }
        let ack = build_quest_offer_ack(&query.qid, qref);
        self.send(0x07, 0x35, &ack[2..]);
        self.dlg = Some((qid, query.npc_entity_id as u16));
    }

    /// Credit held `GoToObjective`s from the player's current zone and pose.
    /// An objective with only `TargetZoneName` completes on arrival; one with
    /// `TargetEntityName` also needs the named placement within `Range`.
    fn credit_arrival_quests(&mut self) {
        let Some(qref) = self.quest_cid else {
            return;
        };
        let Some(corpus) = self.shared.quest_corpus() else {
            return;
        };
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let held: Vec<(u32, u32)> = self.quest_instance.iter().map(|(&q, &i)| (q, i)).collect();
        let mut changed: Vec<(u32, u32)> = Vec::new();
        for (qid, instance) in held {
            let Some(quest) = corpus.quest_by_qid(qid) else {
                continue;
            };
            let Some(mut record) = self
                .shared
                .store
                .lock()
                .ok()
                .and_then(|store| store.quest_get(&account, sqlid, qid))
            else {
                continue;
            };
            let mut quest_changed = false;
            for (index, objective) in quest.objectives.iter().enumerate() {
                if objective.kind != "GoToObjective" {
                    continue;
                }
                let Some(stored) = record.objectives.get(index) else {
                    continue;
                };
                if stored.flags & 0x01 != 0 {
                    continue;
                }
                let zone_ok = objective
                    .target_zone
                    .as_deref()
                    .is_none_or(|zone| zone.eq_ignore_ascii_case(&self.zone));
                if !zone_ok {
                    continue;
                }
                let entity_ok = match objective.target_entity.as_deref() {
                    None | Some("") => true,
                    Some(target) => self.arrival_target_in_range(target, objective.range),
                };
                if !entity_ok {
                    continue;
                }
                let label = objective.wire_label(1);
                let Ok(mut store) = self.shared.store.lock() else {
                    continue;
                };
                if store
                    .quest_update_objective(
                        &account,
                        sqlid,
                        qid,
                        instance,
                        index,
                        0x01,
                        label.clone(),
                    )
                    .is_ok()
                {
                    record.objectives[index].flags = 0x01;
                    record.objectives[index].label = label;
                    quest_changed = true;
                    eprintln!(
                        "game {}: quest arrival qid={qid:#010x} instance={instance} objective={index} {}",
                        self.conn_id,
                        objective.label
                    );
                }
            }
            if quest_changed {
                changed.push((qid, instance));
            }
        }
        for (qid, instance) in changed {
            self.refresh_held_quest(qid, instance, qref);
        }
    }

    /// Is the named arrival target present near the current player pose?
    fn arrival_target_in_range(&self, target: &str, range: u32) -> bool {
        let Some([px, py]) = self.player_pos else {
            return false;
        };
        let range_wire = i64::from(range) * 256;
        for placed in &self.placed_entities {
            let cheap_match = placed
                .spawn_point
                .as_deref()
                .is_some_and(|spawn| spawn.eq_ignore_ascii_case(target))
                || placed
                    .classname
                    .rsplit('.')
                    .next()
                    .is_some_and(|leaf| leaf.eq_ignore_ascii_case(target));
            let name_matches = cheap_match
                || self
                    .shared
                    .class_description_property(&placed.classname, "Name")
                    .is_some_and(|name| name.eq_ignore_ascii_case(target));
            if !name_matches || placed.init.len() < 16 {
                continue;
            }
            let x = i64::from(i32::from_le_bytes(placed.init[4..8].try_into().unwrap()));
            let y = i64::from(i32::from_le_bytes(placed.init[8..12].try_into().unwrap()));
            let dx = x - i64::from(px);
            let dy = y - i64::from(py);
            if dx * dx + dy * dy <= range_wire * range_wire {
                return true;
            }
        }
        // Generated offshoot rooms carry their exit teleporter as a room-node
        // `WaypointName`, not a placed world entity. The client places that
        // waypoint at the room's tile, so target the room centre for a
        // `GotoObjective` such as `level05_off1a_teleporter`.
        if let Some([tx, ty]) =
            self.shared
                .generated_room_waypoint_position(&self.zone, target, self.maze_seed)
        {
            let dx = i64::from(tx) - i64::from(px);
            let dy = i64::from(ty) - i64::from(py);
            if dx * dx + dy * dy <= range_wire * range_wire {
                return true;
            }
        }
        false
    }

    /// Credit one activated world entity against every held
    /// `ActivateObjective` whose own `EntityType{,2..10}` list names it.
    fn credit_activate_quests(&mut self, classname: &str) {
        let Some(qref) = self.quest_cid else {
            return;
        };
        let Some(corpus) = self.shared.quest_corpus() else {
            return;
        };
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let held: Vec<(u32, u32)> = self.quest_instance.iter().map(|(&q, &i)| (q, i)).collect();
        let mut changed: Vec<(u32, u32)> = Vec::new();
        for (qid, instance) in held {
            let Some(quest) = corpus.quest_by_qid(qid) else {
                continue;
            };
            let Some(mut record) = self
                .shared
                .store
                .lock()
                .ok()
                .and_then(|store| store.quest_get(&account, sqlid, qid))
            else {
                continue;
            };
            let mut quest_changed = false;
            for (index, objective) in quest.objectives.iter().enumerate() {
                if !self.objective_matches_activation(objective, classname) {
                    continue;
                }
                let Some(stored) = record.objectives.get(index) else {
                    continue;
                };
                let current = Self::label_current(&stored.label).unwrap_or(0);
                if current >= objective.required.max(1) {
                    continue;
                }
                let next = current.saturating_add(1).min(objective.required.max(1));
                let flags = if next >= objective.required.max(1) {
                    0x01
                } else {
                    0x00
                };
                let label = objective.wire_label(next);
                let Ok(mut store) = self.shared.store.lock() else {
                    continue;
                };
                if store
                    .quest_update_objective(
                        &account,
                        sqlid,
                        qid,
                        instance,
                        index,
                        flags,
                        label.clone(),
                    )
                    .is_ok()
                {
                    record.objectives[index].flags = flags;
                    record.objectives[index].label = label;
                    quest_changed = true;
                    eprintln!(
                        "game {}: quest activate qid={qid:#010x} instance={instance} objective={index} {classname} {next}/{}",
                        self.conn_id,
                        objective.required
                    );
                }
            }
            if quest_changed {
                changed.push((qid, instance));
            }
        }
        for (qid, instance) in changed {
            self.refresh_held_quest(qid, instance, qref);
        }
    }

    /// Credit one killed monster against every held `KillObjective` whose
    /// authored `MonsterType{,2..21}` list names it. The persisted label is
    /// the counter (the same `(flags, label)` pair the client displays), and a
    /// changed quest is restated by RemoveQuest + AddQuest so the journal text
    /// repaints instead of waiting for a re-login.
    fn credit_kill_quests(&mut self, classname: &str) {
        let Some(qref) = self.quest_cid else {
            return;
        };
        let Some(corpus) = self.shared.quest_corpus() else {
            return;
        };
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let held: Vec<(u32, u32)> = self.quest_instance.iter().map(|(&q, &i)| (q, i)).collect();
        let mut changed: Vec<(u32, u32)> = Vec::new();
        for (qid, instance) in held {
            let Some(quest) = corpus.quest_by_qid(qid) else {
                continue;
            };
            let Some(mut record) = self
                .shared
                .store
                .lock()
                .ok()
                .and_then(|store| store.quest_get(&account, sqlid, qid))
            else {
                continue;
            };
            let mut quest_changed = false;
            for (index, objective) in quest.objectives.iter().enumerate() {
                if !self.objective_matches_monster(objective, classname) {
                    continue;
                }
                let Some(stored) = record.objectives.get(index) else {
                    continue;
                };
                let current = Self::label_current(&stored.label).unwrap_or(0);
                if current >= objective.required {
                    continue;
                }
                let next = current.saturating_add(1).min(objective.required);
                let flags = if next >= objective.required {
                    0x01
                } else {
                    0x00
                };
                let label = objective.wire_label(next);
                let Ok(mut store) = self.shared.store.lock() else {
                    continue;
                };
                if store
                    .quest_update_objective(
                        &account,
                        sqlid,
                        qid,
                        instance,
                        index,
                        flags,
                        label.clone(),
                    )
                    .is_ok()
                {
                    record.objectives[index].flags = flags;
                    record.objectives[index].label = label.clone();
                    quest_changed = true;
                    eprintln!(
                        "game {}: quest kill qid={qid:#010x} instance={instance}                          objective={index} {} {next}/{}",
                        self.conn_id, classname, objective.required
                    );
                }
            }
            if quest_changed {
                changed.push((qid, instance));
            }
        }
        for (qid, instance) in changed {
            self.refresh_held_quest(qid, instance, qref);
        }
    }

    /// Restate a held quest's completion. The persisted objective labels are
    /// updated first; this frame is the client's own repaint trigger. Using
    /// RemoveQuest+AddQuest here corrupts the client's quest graph when a
    /// death/respawn rebuilds the journal, so the supported UpdateQuest frame
    /// is the only one sent from the kill path.
    fn refresh_held_quest(&mut self, qid: u32, instance: u32, qref: u16) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let Some(quest) = self
            .shared
            .quest_corpus()
            .and_then(|corpus| corpus.quest_by_qid(qid).cloned())
        else {
            return;
        };
        let Some(mut record) = self
            .shared
            .store
            .lock()
            .ok()
            .and_then(|store| store.quest_get(&account, sqlid, qid))
        else {
            return;
        };
        // Item objectives are bag-derived, but the stored label is what the
        // journal displays. Bring it to the current count before restating;
        // the kind-1 update below then replaces the client's cached strings.
        for (index, objective) in quest.objectives.iter().enumerate() {
            if objective.kind != "ItemObjective" {
                continue;
            }
            let Some(stored) = record.objectives.get(index) else {
                continue;
            };
            let current = self
                .bag_count(objective.item.as_deref().unwrap_or(""))
                .min(objective.required.max(1));
            let flags = u8::from(current >= objective.required.max(1));
            let label = objective.wire_label(current);
            if stored.flags == flags && stored.label == label {
                continue;
            }
            if let Ok(mut store) = self.shared.store.lock() {
                if store
                    .quest_update_objective(
                        &account,
                        sqlid,
                        qid,
                        instance,
                        index,
                        flags,
                        label.clone(),
                    )
                    .is_ok()
                {
                    record.objectives[index].flags = flags;
                    record.objectives[index].label = label;
                }
            }
        }
        let all_done = self.quest_record_all_done(&quest, &record);
        if all_done {
            self.persist_quest_state(qid, QuestState::Complete);
        }
        // Kind 1 replaces the cached objective strings (the visible counters);
        // kind 0 then sets the all-complete bit. Empty-objective quests have
        // only the latter.
        if !record.objectives.is_empty() {
            let rows: Vec<(u8, Vec<u8>)> = record
                .objectives
                .iter()
                .map(|stored| (stored.flags, stored.label.as_bytes().to_vec()))
                .collect();
            let labels = build_quest_objective_labels_update(qref, instance, &rows);
            self.send(0x07, 0x35, &labels[2..]);
        }
        let update = build_quest_objective_update(qref, instance, all_done);
        self.send(0x07, 0x35, &update[2..]);
    }

    /// The c2s QuestManager accept (sub 0x01, empty payload). Commits the
    /// dialog's quest: an AddQuest with the quest's own authored objectives,
    /// then the quest is held. Sub 0x02 is the dialog close and is a no-op.
    fn quest_accept(&mut self, step: u8) {
        if step != QUEST_SUB_ACCEPT {
            return;
        }
        let Some(qref) = self.quest_cid else {
            return;
        };
        let Some((qid, _npc)) = self.dlg.take() else {
            return;
        };
        let Some(corpus) = self.shared.quest_corpus() else {
            return;
        };
        let Some(quest) = corpus.quest_by_qid(qid) else {
            return;
        };
        let objectives: Vec<(u8, Vec<u8>)> = quest
            .objectives
            .iter()
            .map(|objective| {
                quest_objective(&objective.label, Some(0), Some(objective.required), false)
            })
            .collect();
        let instance = self.next_instance;
        self.next_instance = self.next_instance.wrapping_add(1);
        let add =
            build_quest_add_record(&quest_gc_type(qid), qref, instance, &objectives, None, None);
        self.send(0x07, 0x35, &add[2..]);
        self.quest_instance.insert(qid, instance);
        self.persist_accepted(qid, instance, &objectives);
        eprintln!(
            "game {}: quest accept qid={qid:#010x} instance={instance} on_accept={:?}",
            self.conn_id, quest.on_accept_item
        );
        // The quest's own on-accept grant: author its item, place it in the
        // first free bag cell and deliver it as a UnitContainer add stream.
        if let Some(item_class) = quest.on_accept_item.clone() {
            self.grant_item(
                &item_class,
                quest.on_accept_soulbound,
                quest.on_accept_no_sell,
            );
            // The grant can satisfy another held quest's item objective.
            self.restate_item_objectives(&item_class);
        } else if quest
            .objectives
            .iter()
            .any(|objective| objective.kind == "ItemObjective")
        {
            // The bag may ALREADY satisfy an ItemObjective at accept time
            // (the King's Coin token exchanges). No bag mutation follows, so
            // without this restate the client keeps the 0/N AddQuest labels,
            // leaves the turn-in button disabled, and never sends 0x05 even
            // though the server's bag count is ready. A grant above already
            // sends the same restate.
            self.refresh_held_quest(qid, instance, qref);
        }
        // Re-send the offer directory with the just-accepted quest dropped, so
        // its giver loses the `!` marker (the capture pairs this with AddQuest).
        let update = build_quest_available_update(qref, &self.quest_directory());
        self.send(0x07, 0x35, &update[2..]);
    }

    /// Author one generated item into the bag with its rolled modifiers.
    fn author_bag_item_with_mods(
        &mut self,
        class_name: &str,
        soulbound: bool,
        no_sell: bool,
        modifiers: &[(String, u8)],
    ) -> Option<Vec<u8>> {
        self.add_item_updates_with_mods(class_name, 1, None, soulbound, no_sell, modifiers)
            .into_iter()
            .next()
    }

    fn author_bag_item_at_level_with_mods(
        &mut self,
        class_name: &str,
        level: u8,
        soulbound: bool,
        no_sell: bool,
        modifiers: &[(String, u8)],
    ) -> Option<Vec<u8>> {
        self.add_item_updates_with_mods(class_name, 1, Some(level), soulbound, no_sell, modifiers)
            .into_iter()
            .next()
    }

    /// Grant one item class into the bag as its own `(07,35)` stream.
    ///
    /// The synch trailer is the avatar's own: `0x02` + the x256 HP word.
    fn grant_item(&mut self, class_name: &str, soulbound: bool, no_sell: bool) {
        let updates = self.add_item_updates(class_name, 1, None, soulbound, no_sell);
        if updates.is_empty() {
            return;
        }
        let mut synch = vec![0x02];
        synch.extend_from_slice(&self.avatar_hp_word().to_le_bytes());
        let stream = build_component_update_stream(&updates, &synch);
        self.send(0x07, 0x35, &stream[1..]);
    }

    /// Grant one generator-produced loot item, preserving its rolled modifier
    /// children. Quest rewards are authored through `ItemModGenerator1..N`
    /// exactly like ground drops; `grant_item`'s bare class grant drops those
    /// children and renders a Rare base as a gray item.
    fn grant_loot_item(&mut self, item: &LootItem, soulbound: bool) {
        let modifiers: Vec<(String, u8)> = item
            .modifiers
            .iter()
            .map(|modifier| (modifier.class.clone(), modifier.level))
            .collect();
        let updates = self.add_item_updates_with_mods(
            &item.class,
            1,
            Some(item.level.max(1)),
            soulbound,
            false,
            &modifiers,
        );
        if updates.is_empty() {
            return;
        }
        let mut synch = vec![0x02];
        synch.extend_from_slice(&self.avatar_hp_word().to_le_bytes());
        let stream = build_component_update_stream(&updates, &synch);
        self.send(0x07, 0x35, &stream[1..]);
    }

    /// Grant a stack/total of one item class as one UnitContainer stream.
    fn grant_item_total(&mut self, class_name: &str, total: u32) {
        let updates = self.add_item_updates(class_name, total, None, false, false);
        if updates.is_empty() {
            return;
        }
        let mut synch = vec![0x02];
        synch.extend_from_slice(&self.avatar_hp_word().to_le_bytes());
        let stream = build_component_update_stream(&updates, &synch);
        self.send(0x07, 0x35, &stream[1..]);
    }

    /// Credit quest gold and ship the same `0x20` AddCurrency update a
    /// merchant payout uses, so the client's purse and floating text move.
    fn grant_quest_cash(&mut self, amount: u32) {
        if amount == 0 {
            return;
        }
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let Ok(delta) = i32::try_from(amount) else {
            return;
        };
        let update =
            build_container_add_currency_notify(container_cid, delta, self.avatar_eid.unwrap_or(0));
        self.gold = self.gold.saturating_add(amount);
        self.persist_gold(self.gold);
        self.send_updates(&[update]);
        eprintln!(
            "game {}: quest cash reward +{amount} gold (purse={})",
            self.conn_id, self.gold
        );
    }

    /// The one stacking-aware grant path.
    ///
    /// A stackable class (descriptor `MaxStackSize`, or `Consumable` with the
    /// reference's 10 fallback) tops up matching bag entries with `0x22`
    /// quantity updates before opening new cells with `0x1E`. Every mutation
    /// is persisted by a whole-bag snapshot, never appended. Returns the
    /// updates in wire order; an empty vec means nothing was granted.
    fn add_item_updates(
        &mut self,
        class_name: &str,
        total: u32,
        level: Option<u8>,
        soulbound: bool,
        no_sell: bool,
    ) -> Vec<Vec<u8>> {
        self.add_item_updates_with_mods(class_name, total, level, soulbound, no_sell, &[])
    }

    #[allow(clippy::too_many_arguments)]
    fn add_item_updates_with_mods(
        &mut self,
        class_name: &str,
        total: u32,
        level: Option<u8>,
        soulbound: bool,
        no_sell: bool,
        modifiers: &[(String, u8)],
    ) -> Vec<Vec<u8>> {
        let Some(container_cid) = self.avatar_container_cid else {
            eprintln!(
                "game {}: bag grant HELD: this join shipped no avatar UnitContainer",
                self.conn_id
            );
            return Vec::new();
        };
        if total == 0 {
            return Vec::new();
        }
        let max_stack = self.shared.item_max_stack(class_name).clamp(1, 0xFF);
        let mut remaining = total;
        let mut updates = Vec::new();

        if max_stack > 1 {
            let mut ids: Vec<u32> = self
                .bag_entries
                .iter()
                .filter(|(_, entry)| entry.class == class_name && entry.count < max_stack)
                .map(|(&id, _)| id)
                .collect();
            ids.sort_unstable();
            for id in ids {
                if remaining == 0 {
                    break;
                }
                let Some(entry) = self.bag_entries.get(&id) else {
                    continue;
                };
                let added = (max_stack - entry.count).min(remaining);
                if added == 0 {
                    continue;
                }
                let new_count = entry.count + added;
                let count = u8::try_from(new_count).unwrap_or(u8::MAX);
                let Ok(record) = readdress_item_record(&entry.record, id, entry.cell, Some(count))
                else {
                    continue;
                };
                if let Some(entry) = self.bag_entries.get_mut(&id) {
                    entry.count = new_count;
                    entry.record = record;
                }
                updates.push(build_container_item_quantity(container_cid, id, count));
                remaining -= added;
            }
        }

        while remaining > 0 {
            let stack = remaining.min(max_stack);
            let count = match u8::try_from(stack) {
                Ok(count) => count,
                Err(_) => break,
            };
            let id = self.alloc_bag_id();
            let mut record = match self.shared.author_item_record_with_mods(
                class_name,
                id,
                stack,
                level,
                (0, 0),
                modifiers,
            ) {
                Ok(Some(record)) => record,
                Ok(None) => {
                    eprintln!(
                        "game {}: bag grant HELD: game.pkg defines no Item block for {class_name}",
                        self.conn_id
                    );
                    break;
                }
                Err(err) => {
                    eprintln!(
                        "game {}: bag grant HELD: {class_name} -- {err}",
                        self.conn_id
                    );
                    break;
                }
            };
            if let Err(err) = record.apply_instance_flags(soulbound, no_sell) {
                eprintln!(
                    "game {}: bag grant HELD: {class_name} -- {err}",
                    self.conn_id
                );
                break;
            }
            let (width, height, level, flags) =
                (record.width, record.height, record.level, record.flags);
            let Some(cell) = ({
                let Some(grid) = self.bag_grid.as_mut() else {
                    eprintln!(
                        "game {}: bag grant HELD: this join has no bag grid",
                        self.conn_id
                    );
                    break;
                };
                grid.place(width, height)
            }) else {
                eprintln!(
                    "game {}: bag grant HELD: the bag has no free cell for {class_name}",
                    self.conn_id
                );
                break;
            };
            let record = match readdress_item_record(&record.record, id, cell, Some(count)) {
                Ok(record) => record,
                Err(err) => {
                    if let Some(grid) = self.bag_grid.as_mut() {
                        grid.rollback(cell.0, cell.1, width, height);
                    }
                    eprintln!(
                        "game {}: bag grant HELD: {class_name} -- {err}",
                        self.conn_id
                    );
                    break;
                }
            };
            let add = match build_container_item_add(container_cid, &record, id, cell, stack) {
                Ok(add) => add,
                Err(err) => {
                    if let Some(grid) = self.bag_grid.as_mut() {
                        grid.rollback(cell.0, cell.1, width, height);
                    }
                    eprintln!(
                        "game {}: bag grant HELD: {class_name} -- {err}",
                        self.conn_id
                    );
                    break;
                }
            };
            self.bag_entries.insert(
                id,
                BagEntry {
                    id,
                    class: class_name.to_string(),
                    cell,
                    count: stack,
                    level,
                    flags,
                    width,
                    height,
                    record,
                    modifiers: modifiers
                        .iter()
                        .map(|(class, level)| ModifierRoll {
                            class: class.clone(),
                            level: *level,
                        })
                        .collect(),
                },
            );
            updates.push(add);
            remaining -= stack;
        }

        if !updates.is_empty() {
            self.refresh_bag_classes();
            self.persist_bag();
        }
        updates
    }

    /// Mint the next session-local bag item id.
    fn alloc_bag_id(&mut self) -> u32 {
        let id = self.next_bag_id.max(1);
        self.next_bag_id = self.next_bag_id.wrapping_add(1).max(1);
        id
    }

    /// Recompute the class set the quest `ItemObjective` gates read.
    fn refresh_bag_classes(&mut self) {
        self.bag_items = self
            .bag_entries
            .values()
            .map(|entry| entry.class.clone())
            .collect();
    }

    /// The c2s `0x1E` buy: price the shipped shelf entry, debit the purse and
    /// deliver the item as one `(07,35)` stream (currency first, then the add).
    ///
    /// Fails closed and silent on every leg it cannot complete: a cid this join
    /// never shipped, an unknown item id, an uncomputable price, a purse that
    /// cannot cover it, or a full bag sends no frame. No stock-remove frame is
    /// sent: a static tab's entry is not removable.
    /// One c2s train request on a shipped `*.SkillTrainer` component.
    ///
    /// The client has already validated the row and sent
    /// `[player eid][ref type 0x04][skill hash]` (`SkillTrainer::trainSkill`).
    /// The server repeats the authored gates, debits gold, persists the new
    /// rank and confirms the skill level. A newly learned active skill enters
    /// the session catalogue immediately (hotbar placement resolves it) and
    /// the next join's `Skills`/`Manipulators` records.
    fn trainer_train(&mut self, cid: u16, player_eid: u32, ref_type: u8, hash: u32) {
        let Some(registration) = self.trainer_components.get(&cid) else {
            return;
        };
        let npc_class = registration.npc_class.clone();
        let Some(skill) = registration.skills.get(&hash).cloned() else {
            eprintln!(
                "game {}: trainer cid={cid:#06x} {npc_class} hash=0x{hash:08X}                  player={player_eid:#x} ref={ref_type:#04x} -> HELD (not offered)",
                self.conn_id
            );
            return;
        };
        if !self.trainer_class_allows(&skill) {
            eprintln!(
                "game {}: trainer {npc_class} {} -> HELD (class-exclusive passive {:?})",
                self.conn_id, skill.fqn, skill.profession
            );
            return;
        }
        let Some(skills_cid) = self.skills_cid else {
            return;
        };
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let catalog_level = self
            .skill_catalog
            .iter()
            .find(|(fqn, _, _)| fqn.eq_ignore_ascii_case(&skill.fqn))
            .map(|(_, _, level)| *level);
        let known = self.skill_levels.get(&skill.fqn).copied().or(catalog_level);
        let has_skill = known.is_some();
        let current = known.unwrap_or(0);
        let next = if has_skill {
            current.saturating_add(1)
        } else {
            1
        };
        if next == 0 || next as u32 > skill.max_level {
            eprintln!(
                "game {}: trainer {} rank {next} -> HELD (max {})",
                self.conn_id, skill.fqn, skill.max_level
            );
            return;
        }
        let value = self.shared.skill_value_per_level().unwrap_or(1113.621);
        let cost = train_cost(value, &skill, u32::from(next));
        if cost == 0 || self.gold < cost {
            eprintln!(
                "game {}: trainer {} rank {next} cost={cost} gold={} -> HELD",
                self.conn_id, skill.fqn, self.gold
            );
            return;
        }

        self.gold -= cost;
        self.skill_levels.insert(skill.fqn.clone(), next);
        // Keep the live catalogue in lockstep with the bought rank. The next
        // train request reads this entry, and hotbar placement resolves the
        // skill's manipulator level from it; leaving the old rank here made
        // every repeat purchase look like rank 2 again.
        if let Some((_, _, level)) = self
            .skill_catalog
            .iter_mut()
            .find(|(fqn, _, _)| fqn.eq_ignore_ascii_case(&skill.fqn))
        {
            *level = next;
        } else {
            let id = self.next_skill_id();
            self.skill_catalog.push((skill.fqn.clone(), id, next));
        }
        if let Some(sqlid) = self.selected {
            let account = self.shared.account();
            if let Ok(mut store) = self.shared.store.lock() {
                if let Err(err) =
                    store.set_skill_state(&account, sqlid, self.skill_points, &self.skill_levels)
                {
                    eprintln!(
                        "game {}: trainer skill persist for sqlid {sqlid} failed: {err}",
                        self.conn_id
                    );
                }
            }
        }
        self.persist_gold(self.gold);

        let level_update = build_skills_skill_name_update(skills_cid, &skill.fqn, next);
        let currency = build_container_add_currency_notify(
            container_cid,
            -(cost as i32),
            self.avatar_eid.unwrap_or(0),
        );
        let mut updates = vec![level_update];
        // A skill already on the hotbar also needs its manipulator entry
        // refreshed; the client's cast reads the level from that record.
        if let Some(slot) = self
            .skill_slots
            .iter()
            .find(|(_, fqn)| fqn.eq_ignore_ascii_case(&skill.fqn))
            .map(|(slot, _)| *slot)
        {
            if let Some(manipulators_cid) = self.manipulators_cid {
                updates.push(build_manipulators_add(
                    manipulators_cid,
                    &skill_manipulator_update_entry(&skill.fqn, slot, next),
                ));
            }
        }
        updates.push(currency);
        let mut synch = vec![0x02];
        synch.extend_from_slice(&self.avatar_hp_word().to_le_bytes());
        let stream = build_component_update_stream(&updates, &synch);
        self.send(0x07, 0x35, &stream[1..]);
        eprintln!(
            "game {}: trainer {npc_class} {} -> rank {next} cost={cost} gold={}              player={player_eid:#x} ref={ref_type:#04x}",
            self.conn_id, skill.fqn, self.gold
        );
    }

    /// Any class may train at any trainer; only a passive whose authored
    /// `ProfessionType` is a player class is exclusive to that class.
    fn trainer_class_allows(&self, skill: &TrainerSkill) -> bool {
        if !skill.passive {
            return true;
        }
        let Some(profession) = skill.profession.as_deref() else {
            return true;
        };
        let wanted = profession.trim();
        if !matches!(
            wanted.to_ascii_uppercase().as_str(),
            "FIGHTER" | "MAGE" | "RANGER"
        ) {
            return true;
        }
        self.avatar_class.as_deref().is_some_and(|class| {
            class
                .to_ascii_lowercase()
                .contains(&wanted.to_ascii_lowercase())
        })
    }

    /// The next free skill id in the skill-book range (200+), clear of the
    /// class kit's own numbered ids and the hotbar slots.
    fn next_skill_id(&self) -> u32 {
        self.skill_catalog
            .iter()
            .map(|(_, id, _)| *id)
            .filter(|id| *id >= 200)
            .max()
            .map_or(200, |id| id.saturating_add(1))
    }

    fn merchant_buy(&mut self, cid: u16, _buyer: u16, item_id: u32) {
        let Some(registration) = self.merchant_components.get(&cid) else {
            eprintln!(
                "game {}: merchant-buy cid={cid:#06x} -> HELD (not shipped)",
                self.conn_id
            );
            return;
        };
        let Some(stock) = registration.items.get(&item_id) else {
            eprintln!(
                "game {}: merchant-buy {} item={item_id} -> HELD (not on this shelf)",
                self.conn_id, registration.npc_class
            );
            return;
        };
        let npc_class = registration.npc_class.clone();
        let class = stock.class.clone();
        let (item_level, quantity) = (u32::from(stock.level), u32::from(stock.quantity));
        let Some(container_cid) = self.avatar_container_cid else {
            eprintln!(
                "game {}: merchant-buy {class} item={item_id} -> HELD (no UnitContainer)",
                self.conn_id
            );
            return;
        };
        let quote = match self.shared.merchant_price(
            &npc_class,
            &class,
            item_level,
            quantity,
            &[],
            Some(self.selected_level),
        ) {
            None => {
                eprintln!(
                    "game {}: merchant-buy {class} item={item_id} -> HELD (no game.pkg)",
                    self.conn_id
                );
                return;
            }
            Some(Err(err)) => {
                eprintln!(
                    "game {}: merchant-buy {class} item={item_id} -> HELD: {err}",
                    self.conn_id
                );
                return;
            }
            Some(Ok(quote)) => quote,
        };
        let Ok(price) = i32::try_from(quote.gold) else {
            return;
        };
        if price <= 0 || self.gold < price as u32 {
            eprintln!(
                "game {}: merchant-buy {class} item={item_id} price={price} \
                 gold={} -> HELD (insufficient)",
                self.conn_id, self.gold
            );
            return;
        }
        let item_updates = self.add_item_updates(&class, quantity, Some(stock.level), false, false);
        if item_updates.is_empty() {
            eprintln!(
                "game {}: merchant-buy {class} item={item_id} ({}) -> HELD \
                 (bag full or item would not author)",
                self.conn_id, quote.footprint
            );
            return;
        }
        let currency = build_container_add_currency_notify(
            container_cid,
            -price,
            self.avatar_eid.unwrap_or(0),
        );
        let mut updates = Vec::with_capacity(1 + item_updates.len());
        updates.push(currency);
        updates.extend(item_updates);
        let mut synch = vec![0x02];
        synch.extend_from_slice(&self.avatar_hp_word().to_le_bytes());
        let stream = build_component_update_stream(&updates, &synch);
        self.gold -= price as u32;
        self.send(0x07, 0x35, &stream[1..]);
        self.persist_gold(self.gold);
        // A purchase is a bag change, so a held quest whose `ItemObjective`
        // this item satisfies becomes turn-in ready on the client.
        self.restate_item_objectives(&class);
        eprintln!(
            "game {}: merchant-buy cid={cid:#06x} {class} item={item_id} \
             price={price} quantity={quantity} {} quality={} gold={}",
            self.conn_id, quote.footprint, quote.quality_name, self.gold
        );
    }

    /// Merchant `0x1F`: sell one bag item (the requested slot's whole stack).
    ///
    /// The frame is `[cid u16][0x1F][entityRef u16][item_id u32]`. The price is
    /// `Merchant::getSellValue` for one item (a stack sells for one, as the
    /// client's own formula does); the reply credits the purse and removes the
    /// bag node in one `(07,35)` stream.
    fn merchant_sell(&mut self, cid: u16, _entity_ref: u16, item_id: u32) {
        let Some(registration) = self.merchant_components.get(&cid) else {
            eprintln!(
                "game {}: merchant-sell cid={cid:#06x} -> HELD (not shipped)",
                self.conn_id
            );
            return;
        };
        let npc_class = registration.npc_class.clone();
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let Some(entry) = self.bag_entries.get(&item_id).cloned() else {
            eprintln!(
                "game {}: merchant-sell {npc_class} item={item_id} -> HELD (not in bag)",
                self.conn_id
            );
            return;
        };
        if entry.flags & drf_content::ITEM_WIRE_FLAG_NO_SELL != 0 {
            eprintln!(
                "game {}: merchant-sell {} item={item_id} -> HELD (NoSell flag)",
                self.conn_id, entry.class
            );
            return;
        }
        let price = match self.shared.merchant_sell_price(
            &npc_class,
            &entry.class,
            u32::from(entry.level),
            Some(self.selected_level),
        ) {
            Some(Ok(price)) => price,
            Some(Err(err)) => {
                eprintln!(
                    "game {}: merchant-sell {} item={item_id} -> HELD: {err}",
                    self.conn_id, entry.class
                );
                return;
            }
            None => {
                eprintln!(
                    "game {}: merchant-sell {} item={item_id} -> HELD (no game.pkg)",
                    self.conn_id, entry.class
                );
                return;
            }
        };
        let Ok(price) = i32::try_from(price.max(1)) else {
            return;
        };
        let cursor = self.cursor == Some(item_id);
        self.bag_entries.remove(&item_id);
        if let Some(grid) = self.bag_grid.as_mut() {
            grid.rollback(entry.cell.0, entry.cell.1, entry.width, entry.height);
        }
        if cursor {
            self.cursor = None;
        }
        self.refresh_bag_classes();
        self.gold = self.gold.saturating_add(price as u32);
        self.persist_bag();
        self.persist_gold(self.gold);
        let mut updates = Vec::with_capacity(2 + usize::from(cursor));
        if cursor {
            updates.push(build_container_clear_active(container_cid));
        }
        updates.push(build_container_item_remove(container_cid, item_id));
        updates.push(build_container_add_currency_notify(
            container_cid,
            price,
            self.avatar_eid.unwrap_or(0),
        ));
        self.send_updates(&updates);
        self.restate_item_objectives(&entry.class);
        eprintln!(
            "game {}: merchant-sell cid={cid:#06x} {npc_class} {} item={item_id} \
             price={price} gold={}",
            self.conn_id, entry.class, self.gold
        );
    }

    /// c2s QuestManager `0x03`: abandon the held quest with this instance.
    ///
    /// The client's `QuestManager::abandon` sends the instance ref and waits
    /// for RemoveQuest before dropping its local node. The persisted row is
    /// deleted so a re-login cannot replay the abandoned quest, and the
    /// available directory is re-sent so its giver's marker returns.
    fn quest_abandon(&mut self, instance: u32) {
        let Some(qref) = self.quest_cid else {
            return;
        };
        let Some(&qid) = self
            .quest_instance
            .iter()
            .find(|(_, &held)| held == instance)
            .map(|(qid, _)| qid)
        else {
            eprintln!(
                "game {}: quest abandon instance={instance} -> HELD (no held instance)",
                self.conn_id
            );
            return;
        };
        self.quest_instance.remove(&qid);
        if let Some(sqlid) = self.selected {
            let account = self.shared.account();
            if let Ok(mut store) = self.shared.store.lock() {
                if let Err(err) = store.remove_quest(&account, sqlid, qid) {
                    eprintln!(
                        "game {}: quest abandon persist for sqlid {sqlid} qid={qid:#010x} failed: {err}",
                        self.conn_id
                    );
                }
            }
        }
        let remove = build_quest_remove(qref, instance);
        self.send(0x07, 0x35, &remove[2..]);
        let update = build_quest_available_update(qref, &self.quest_directory());
        self.send(0x07, 0x35, &update[2..]);
        eprintln!(
            "game {}: quest abandon instance={instance} qid={qid:#010x}",
            self.conn_id
        );
    }

    /// A c2s turn-in leg for a held quest. The query (sub 0x04) is answered
    /// with the ShowTurnInDialog; the complete (sub 0x05) with FinalizeQuest
    /// then RemoveQuest, and the quest moves to the completed set (which the
    /// offer directory's prereq/completed gates read). The client only sends
    /// these once IT believes the quest is complete, so the held instance is
    /// the readiness check.
    fn quest_turnin(&mut self, step: u8, instance: u32) {
        let Some(qref) = self.quest_cid else {
            return;
        };
        let Some(&qid) = self
            .quest_instance
            .iter()
            .find(|(_, &held)| held == instance)
            .map(|(qid, _)| qid)
        else {
            eprintln!(
                "game {}: quest turn-in step={step:#04x} instance={instance} -> HELD \
                 (no held instance; held={:?})",
                self.conn_id,
                self.quest_instance.values().collect::<Vec<_>>()
            );
            return;
        };
        eprintln!(
            "game {}: quest turn-in step={step:#04x} instance={instance} qid={qid:#010x}",
            self.conn_id
        );
        if step == QUEST_SUB_TURNIN_QUERY {
            if let Some(quest) = self
                .shared
                .quest_corpus()
                .and_then(|corpus| corpus.quest_by_qid(qid).cloned())
            {
                if !self.quest_ready(&quest, instance, qid) {
                    eprintln!(
                        "game {}: quest turn-in query instance={instance} qid={qid:#010x} -> HELD (not ready)",
                        self.conn_id
                    );
                    return;
                }
            }
            let ack = build_quest_ready_ack(qref, instance);
            self.send(0x07, 0x35, &ack[2..]);
            eprintln!(
                "game {}: quest turn-in query instance={instance} qid={qid:#010x} -> ShowTurnInDialog",
                self.conn_id
            );
            return;
        }
        if step != QUEST_SUB_TURNIN_COMPLETE {
            return;
        }
        let quest = self
            .shared
            .quest_corpus()
            .and_then(|corpus| corpus.quest_by_qid(qid).cloned());
        if let Some(quest) = &quest {
            if !self.quest_ready(quest, instance, qid) {
                eprintln!(
                    "game {}: quest turn-in instance={instance} qid={qid:#010x} -> HELD \
                     (an objective is not complete)",
                    self.conn_id
                );
                return;
            }
        }
        let finalize = build_quest_finalize(qref, instance);
        self.send(0x07, 0x35, &finalize[2..]);
        let remove = build_quest_remove(qref, instance);
        self.send(0x07, 0x35, &remove[2..]);
        self.quest_instance.remove(&qid);
        self.quest_completed.insert(qid);
        self.persist_quest_state(qid, QuestState::TurnedIn);
        // Turn-in rewards: consume every `RemoveOnFinalize` objective (the
        // wishing well's King's Coin) and draw the authored reward generator.
        // `RewardItemsAreDropped` means the reward is a ground item the client
        // can pick up, not a bag grant.
        if let Some(quest) = &quest {
            // Reward order follows the client's own doReward: cash/token first,
            // then authored item generators, then the objective consumables.
            let reward_level = self.selected_level.clamp(quest.min_level, quest.max_level);
            let cash = (f32::from(reward_level) * 250.0 * quest.cash_reward).round();
            if cash.is_finite() && cash > 0.0 {
                self.grant_quest_cash(cash as u32);
            }
            if quest.token_reward > 0 {
                self.grant_item_total("QuestItemPAL.Token", quest.token_reward);
            }
            if quest.grant_xp_buff {
                // The authentic reward is the `QuestXPBonus` EXPMOD, not
                // direct XP (the client self-levels). Keep the parsed bit
                // visible until the modifier grant is wired.
                eprintln!(
                    "game {}: quest reward grants XP buff (qid={qid:#010x})",
                    self.conn_id
                );
            }
            for objective in &quest.objectives {
                if objective.remove_on_finalize {
                    if let Some(item) = &objective.item {
                        self.consume_bag_class(item, objective.required);
                    }
                }
            }
            let level = self.selected_level.max(1);
            let rolls = quest.num_reward_items.max(1);
            let pose = self
                .player_pos
                .map(|[x, y]| [x, y, 0, 0])
                .unwrap_or([0, 0, 0, 0]);
            // `NumRewardItems` rolls the WHOLE authored generator set, so two
            // generators with 1 roll yield one item from each. Use the authored
            // generator tree, not the flat class pool: the flat path discards
            // `ItemModGenerator1..N`, which is exactly what makes a Rare base
            // item a gray/no-modifier item on the client.
            let reward_level = u8::try_from(level).unwrap_or(u8::MAX).max(1);
            let mut reward_rng = MersenneTwister::new(
                u32::try_from((u64::from(qid) << 32) ^ u64::from(instance))
                    .unwrap_or(0)
                    .max(1),
            );
            for generator in &quest.reward_generators {
                for _ in 0..rolls {
                    let items = self.shared.generate_loot(
                        generator,
                        reward_level,
                        1,
                        &mut reward_rng,
                        false,
                    );
                    for item in items {
                        if quest.reward_items_dropped {
                            self.spawn_loot_item(&item, pose);
                        } else {
                            self.grant_loot_item(&item, quest.reward_soulbound);
                        }
                    }
                }
            }
        }
        // A turn-in is the only change that moves `completed`, so it is the
        // only moment the offered set can grow (the follow-up a prereq unlocks).
        let update = build_quest_available_update(qref, &self.quest_directory());
        self.send(0x07, 0x35, &update[2..]);
    }

    /// Restate every held quest's completion, computed from the bag. The client
    /// repaints a quest's marker only on this frame, so any bag change (a grant
    /// or a purchase) that satisfies a held `ItemObjective` must be followed by
    /// it -- otherwise the quest is complete on the server and grey on screen.
    fn restate_held_objectives(&mut self) {
        let Some(qref) = self.quest_cid else {
            return;
        };
        let held: Vec<(u32, u32)> = self.quest_instance.iter().map(|(&q, &i)| (q, i)).collect();
        for (qid, instance) in held {
            self.refresh_held_quest(qid, instance, qref);
        }
    }

    /// Restate only held quests whose authored `ItemObjective` names `class`.
    ///
    /// A bag mutation can only change a count for the class it touched, so
    /// walking every held quest after every purchase/sale floods the client
    /// with scores of unrelated objective frames and delays the merchant
    /// update behind them.
    fn restate_item_objectives(&mut self, class: &str) {
        let Some(qref) = self.quest_cid else {
            return;
        };
        let Some(corpus) = self.shared.quest_corpus() else {
            return;
        };
        let held: Vec<(u32, u32)> = self.quest_instance.iter().map(|(&q, &i)| (q, i)).collect();
        for (qid, instance) in held {
            let Some(quest) = corpus.quest_by_qid(qid) else {
                continue;
            };
            let matches = quest.objectives.iter().any(|objective| {
                objective.kind == "ItemObjective"
                    && objective
                        .item
                        .as_deref()
                        .is_some_and(|item| self.shared.class_is_kind_of(class, item))
            });
            if matches {
                self.refresh_held_quest(qid, instance, qref);
            }
        }
    }

    /// Re-deliver a held quest's authored on-accept item when the bag lost it.
    /// This is the same content grant the accept path uses; it only runs for a
    /// held quest whose own ItemObjective names that exact class.
    fn restore_held_on_accept_items(&mut self) {
        let Some(corpus) = self.shared.quest_corpus() else {
            return;
        };
        let held: Vec<u32> = self.quest_instance.keys().copied().collect();
        for qid in held {
            let Some(quest) = corpus.quest_by_qid(qid) else {
                continue;
            };
            let Some(item) = quest.on_accept_item.as_deref() else {
                continue;
            };
            let required = quest.objectives.iter().any(|objective| {
                objective
                    .item
                    .as_deref()
                    .is_some_and(|wanted| self.shared.class_is_kind_of(item, wanted))
            });
            if !required || self.bag_count(item) > 0 {
                continue;
            }
            eprintln!(
                "game {}: restoring missing on-accept item {item} for qid={qid:#010x}",
                self.conn_id
            );
            self.grant_item(item, quest.on_accept_soulbound, quest.on_accept_no_sell);
        }
    }

    /// Persist a quest accept: `accepted`, its minted instance id and the
    /// objectives the AddQuest carried.
    fn persist_accepted(&self, qid: u32, instance: u32, objectives: &[(u8, Vec<u8>)]) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let objectives: Vec<QuestObjective> = objectives
            .iter()
            .map(|(flags, label)| QuestObjective {
                flags: *flags,
                label: String::from_utf8_lossy(label).into_owned(),
            })
            .collect();
        if let Ok(mut store) = self.shared.store.lock() {
            let _ = store.mark_accepted(&account, sqlid, qid, instance, objectives);
        }
    }

    /// Persist one quest's state after a turn-in confirm.
    fn persist_quest_state(&self, qid: u32, state: QuestState) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        if let Ok(mut store) = self.shared.store.lock() {
            let _ = store.quest_set_state(&account, sqlid, qid, state);
        }
    }

    /// Replace the persisted bag with a snapshot of the live entries.
    ///
    /// A snapshot (not an append) is what keeps moves, merges and uses from
    /// accumulating stale rows; the row key is class/cell and the count is the
    /// live stack.
    fn persist_bag(&self) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let mut entries: Vec<&BagEntry> = self.bag_entries.values().collect();
        entries.sort_by_key(|entry| entry.id);
        let bag: Vec<BagItem> = entries
            .into_iter()
            .map(|entry| BagItem {
                class: entry.class.clone(),
                cell_x: entry.cell.0,
                cell_y: entry.cell.1,
                count: entry.count,
                level: Some(entry.level),
                flags: Some(entry.flags),
                modifiers: entry
                    .modifiers
                    .iter()
                    .map(|modifier| (modifier.class.clone(), modifier.level))
                    .collect(),
            })
            .collect();
        if let Ok(mut store) = self.shared.store.lock() {
            let _ = store.set_bag(&account, sqlid, bag);
        }
    }

    /// Replace the persisted worn set with a snapshot of the live slots.
    fn persist_worn(&self) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let mut slots: Vec<&WornEntry> = self.worn.values().collect();
        slots.sort_by_key(|entry| entry.slot);
        let equipment: Vec<EquipmentItem> = slots
            .into_iter()
            .map(|entry| EquipmentItem {
                slot: entry.slot,
                class: entry.class.clone(),
                level: entry.level,
                flags: entry.flags,
                modifiers: entry
                    .modifiers
                    .iter()
                    .map(|modifier| (modifier.class.clone(), modifier.level))
                    .collect(),
            })
            .collect();
        if let Ok(mut store) = self.shared.store.lock() {
            let _ = store.set_equipment(&account, sqlid, equipment);
        }
    }

    /// Author and install the worn set (the join rebuild path).
    ///
    /// `None` seeds the class starting kit; `Some` re-authors each persisted
    /// slot, preferring the class kit's authored/modifier data when the class
    /// is a starting item. That keeps the record length identical to what the
    /// client last saw (a starting weapon carries a rolled modifier).
    fn rebuild_worn(
        &mut self,
        persisted: Option<&[EquipmentItem]>,
        kit: Option<&[drf_content::ClassItem]>,
    ) {
        self.worn.clear();
        let Some(items) = persisted else {
            if let Some(kit) = kit {
                self.seed_class_worn(kit);
            }
            return;
        };
        for item in items {
            if let Some(entry) = self.author_worn_entry(item, kit) {
                self.worn.insert(entry.slot, entry);
            }
        }
    }

    /// Seed the live worn map from the class starting kit.
    ///
    /// A character with no persisted equipment ships the class kit verbatim, so
    /// the rewrite/unequip handlers still need live entries: without them a
    /// starting item cannot be dragged off and a picked-up item cannot replace
    /// the slot it occupies.
    fn seed_class_worn(&mut self, items: &[drf_content::ClassItem]) {
        for item in items {
            let Ok(slot) = u32::try_from(item.slot) else {
                continue;
            };
            if slot == 0 || self.worn.contains_key(&slot) {
                continue;
            }
            let level = u8::try_from(item.level).unwrap_or(1);
            let class = self
                .shared
                .canonical_item_class(&item.fqn)
                .unwrap_or_else(|| item.fqn.clone());
            let record = equipment_item_entry(
                &class,
                item.slot,
                level,
                item.authored,
                &item.mods,
                EQUIPMENT_STARTING_MOD_LEVEL,
            );
            let Ok(readstate) = item_manipulator_readstate(&item.dfc) else {
                continue;
            };
            let mut manip_record = record.clone();
            manip_record.extend_from_slice(&readstate);
            let (width, height) = self
                .shared
                .item_profile(&item.fqn)
                .map(|profile| (profile.width, profile.height))
                .unwrap_or((1, 1));
            self.worn.insert(
                slot,
                WornEntry {
                    slot,
                    class,
                    level,
                    flags: 0,
                    width,
                    height,
                    record,
                    manip_record,
                    modifiers: item
                        .mods
                        .iter()
                        .map(|class| ModifierRoll {
                            class: class.clone(),
                            level: EQUIPMENT_STARTING_MOD_LEVEL,
                        })
                        .collect(),
                },
            );
        }
    }

    /// Rebuild one worn item's Equipment and Manipulators records from its
    /// persisted class/level/flags.
    fn author_worn_entry(
        &self,
        item: &EquipmentItem,
        kit: Option<&[drf_content::ClassItem]>,
    ) -> Option<WornEntry> {
        let slot = item.slot;
        let kit_item = kit.and_then(|items| {
            items
                .iter()
                .find(|entry| entry.fqn == item.class && entry.slot == i64::from(slot))
        });
        let (record, dfc, width, height, level, flags) = if let Some(kit_item) = kit_item {
            // Re-author a starting item from the class kit, including its
            // authored status byte and rolled modifiers. Dropping those makes
            // the record shorter than the one the client last read and
            // desyncs the CEM stream ("Unknown message type(0)").
            let class = self
                .shared
                .canonical_item_class(&kit_item.fqn)
                .unwrap_or_else(|| kit_item.fqn.clone());
            let mut record = equipment_item_entry(
                &class,
                kit_item.slot,
                item.level,
                kit_item.authored,
                &kit_item.mods,
                EQUIPMENT_STARTING_MOD_LEVEL,
            );
            let flags_at = kit_item.fqn.len() + 10;
            if flags_at < record.len() {
                record[flags_at] = item.flags;
            }
            let (width, height) = self
                .shared
                .item_profile(&kit_item.fqn)
                .map(|profile| (profile.width, profile.height))
                .unwrap_or((1, 1));
            (
                record,
                kit_item.dfc.clone(),
                width,
                height,
                item.level,
                item.flags,
            )
        } else {
            let modifiers: Vec<(String, u8)> = item
                .modifiers
                .iter()
                .map(|(class, level)| (class.clone(), *level))
                .collect();
            let mut record = match self.shared.author_item_record_with_mods(
                &item.class,
                slot,
                1,
                Some(item.level),
                (0xFF, 0xFF),
                &modifiers,
            ) {
                Ok(Some(record)) => record,
                Ok(None) => {
                    eprintln!(
                        "game {}: worn item HELD: game.pkg defines no Item block for {}",
                        self.conn_id, item.class
                    );
                    return None;
                }
                Err(err) => {
                    eprintln!(
                        "game {}: worn item HELD: {} -- {err}",
                        self.conn_id, item.class
                    );
                    return None;
                }
            };
            if let Err(err) = record.set_flags(item.flags) {
                eprintln!(
                    "game {}: worn item HELD: {} -- {err}",
                    self.conn_id, item.class
                );
                return None;
            }
            let level = record.level;
            let flags = record.flags;
            let width = record.width;
            let height = record.height;
            let default_record =
                match readdress_item_record(&record.record, slot, (0xFF, 0xFF), Some(1)) {
                    Ok(record) => record,
                    Err(err) => {
                        eprintln!(
                            "game {}: worn item HELD: {} -- {err}",
                            self.conn_id, item.class
                        );
                        return None;
                    }
                };
            let dfc = self.shared.item_dfc_root(&item.class)?;
            (default_record, dfc, width, height, level, flags)
        };
        let readstate = item_manipulator_readstate(&dfc).ok()?;
        let mut manip_record = record.clone();
        manip_record.extend_from_slice(&readstate);
        Some(WornEntry {
            slot,
            class: item.class.clone(),
            level,
            flags,
            width,
            height,
            record,
            manip_record,
            modifiers: item
                .modifiers
                .iter()
                .map(|(class, level)| ModifierRoll {
                    class: class.clone(),
                    level: *level,
                })
                .collect(),
        })
    }

    /// Persist the purse after a debit or credit.
    fn persist_gold(&self, gold: u32) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        if let Ok(mut store) = self.shared.store.lock() {
            let _ = store.set_gold(&account, sqlid, gold);
        }
    }

    /// Persist the character's current zone.
    fn persist_zone(&self, zone: &str) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        if let Ok(mut store) = self.shared.store.lock() {
            let _ = store.set_zone(&account, sqlid, zone);
        }
    }

    /// Canonicalize a zone name before it is assigned. The shipped AdminUI
    /// still sends the cut `pvp_town` name; the live Pwnston zone is
    /// `pvp_start`.
    fn normalize_zone(zone: &str) -> String {
        match zone.trim().to_ascii_lowercase().as_str() {
            "pvp_town" | "pwnston" => "pvp_start".to_string(),
            "thehub" => "TheHub".to_string(),
            _ => zone.trim().to_string(),
        }
    }

    /// Replace a persisted zone the package cannot serve with Townston, so a
    /// bad stored name cannot wedge the client on its world-join screen.
    fn servable_zone(&self, zone: &str) -> String {
        let zone = Self::normalize_zone(zone);
        if zone.is_empty() {
            return ZONE_TOWN.to_string();
        }
        if self.shared.has_content() && !self.shared.zone_exists(&zone) {
            eprintln!(
                "game {}: persisted zone {zone:?} is not in game.pkg; falling back to town",
                self.conn_id
            );
            return ZONE_TOWN.to_string();
        }
        zone
    }

    /// Remember that this character has entered `zone`, for the non-faithful
    /// obelisk "everywhere I've been" list. Idempotent.
    fn persist_visited_zone(&self, zone: &str) {
        let Some(sqlid) = self.selected else {
            return;
        };
        if zone.trim().is_empty() {
            return;
        }
        let account = self.shared.account();
        if let Ok(mut store) = self.shared.store.lock() {
            let _ = store.record_visited_zone(&account, sqlid, zone);
        }
    }

    /// Seed the session's per-character state from the persisted record.
    ///
    /// The completed set (the offer directory's gate), the held quests and
    /// their instance ids (so a turn-in after a re-login resolves), the bag
    /// (so a held `ItemObjective` is already satisfied) and the purse.
    fn restore_character_state(&mut self, character: &Character) {
        self.recent_zone_portal = character.recent_zone_portal.clone();
        self.difficulty = character.difficulty.min(3);
        self.gold = character.gold;
        self.experience = character.experience;
        self.attribute_points = character.attribute_points;
        self.skill_points = character.skill_points;
        self.skill_levels = character.skill_levels.clone();
        self.attributes = [
            character.strength,
            character.agility,
            character.toughness,
            character.power,
        ];
        self.bag_items = character.bag.iter().map(|row| row.class.clone()).collect();
        self.quest_completed = character
            .quests
            .iter()
            .filter(|(_, record)| record.state == QuestState::TurnedIn)
            .filter_map(|(hex, _)| qid_from_hex(hex))
            .collect();
        self.quest_instance.clear();
        let mut highest = 0u32;
        for (hex, record) in &character.quests {
            highest = highest.max(record.instance_ref);
            if matches!(record.state, QuestState::Accepted | QuestState::Complete) {
                if let Some(qid) = qid_from_hex(hex) {
                    self.quest_instance.insert(qid, record.instance_ref);
                }
            }
        }
        self.next_instance = highest.saturating_add(1).max(1);
    }

    /// Re-deliver the persisted bag, addressed to the avatar's `UnitContainer`.
    ///
    /// Each row is re-authored from its class, minted a fresh session-local id,
    /// placed at the persisted cell, and streamed as one `(07,35)` update with
    /// the avatar's synch trailer. The live [`BagEntry`] map is populated as the
    /// frames are built, so a request naming an item resolves immediately after
    /// join. Called once per join after the join batch; this path never
    /// re-persists (the rows it is reading came from the store).
    fn reship_bag(&mut self, bag: &[BagItem]) {
        self.bag_entries.clear();
        self.cursor = None;
        if let Some(grid) = self.bag_grid.as_mut() {
            let (width, height) = grid.dimensions();
            *grid = BagGrid::new(width, height);
        }
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        // Collapse duplicate stackable rows persisted before stacking existed:
        // keep the first row's cell and fold later counts into it up to the
        // descriptor's max. The total occupied area only shrinks.
        let mut merged: Vec<BagItem> = Vec::new();
        for row in bag {
            let max_stack = self.shared.item_max_stack(&row.class);
            if max_stack > 1 {
                if let Some(existing) = merged.iter_mut().find(|out| {
                    out.class == row.class
                        && out.level == row.level
                        && out.flags == row.flags
                        && out.count < max_stack
                }) {
                    let added = (max_stack - existing.count).min(row.count);
                    existing.count += added;
                    let leftover = row.count - added;
                    if leftover > 0 {
                        let mut extra = row.clone();
                        extra.count = leftover;
                        merged.push(extra);
                    }
                    continue;
                }
            }
            merged.push(row.clone());
        }
        let mut updates = Vec::new();
        for row in &merged {
            let cell = (row.cell_x, row.cell_y);
            let id = self.alloc_bag_id();
            let modifiers: Vec<(String, u8)> = row
                .modifiers
                .iter()
                .map(|(class, level)| (class.clone(), *level))
                .collect();
            let mut authored = match self.shared.author_item_record_with_mods(
                &row.class, id, row.count, row.level, cell, &modifiers,
            ) {
                Ok(Some(record)) => record,
                Ok(None) => {
                    eprintln!(
                        "game {}: bag reship HELD: game.pkg defines no Item block for {}",
                        self.conn_id, row.class
                    );
                    continue;
                }
                Err(err) => {
                    eprintln!(
                        "game {}: bag reship HELD: {} -- {err}",
                        self.conn_id, row.class
                    );
                    continue;
                }
            };
            if let Some(flags) = row.flags {
                if let Err(err) = authored.set_flags(flags) {
                    eprintln!(
                        "game {}: bag reship HELD: {} -- {err}",
                        self.conn_id, row.class
                    );
                    continue;
                }
            }
            let count = u8::try_from(row.count).unwrap_or(u8::MAX);
            let record = match readdress_item_record(&authored.record, id, cell, Some(count)) {
                Ok(record) => record,
                Err(err) => {
                    eprintln!(
                        "game {}: bag reship HELD: {} -- {err}",
                        self.conn_id, row.class
                    );
                    continue;
                }
            };
            let add = match build_container_item_add(container_cid, &record, id, cell, row.count) {
                Ok(add) => add,
                Err(err) => {
                    eprintln!(
                        "game {}: bag reship HELD: {} -- {err}",
                        self.conn_id, row.class
                    );
                    continue;
                }
            };
            if let Some(grid) = self.bag_grid.as_mut() {
                grid.occupy(cell.0, cell.1, authored.width, authored.height);
            }
            self.bag_entries.insert(
                id,
                BagEntry {
                    id,
                    class: row.class.clone(),
                    cell,
                    count: row.count,
                    level: authored.level,
                    flags: authored.flags,
                    width: authored.width,
                    height: authored.height,
                    record,
                    modifiers: row
                        .modifiers
                        .iter()
                        .map(|(class, level)| ModifierRoll {
                            class: class.clone(),
                            level: *level,
                        })
                        .collect(),
                },
            );
            updates.push(add);
        }
        self.refresh_bag_classes();
        if updates.is_empty() {
            return;
        }
        let mut synch = vec![0x02];
        synch.extend_from_slice(&self.avatar_hp_word().to_le_bytes());
        let stream = build_component_update_stream(&updates, &synch);
        self.send(0x07, 0x35, &stream[1..]);
    }

    /// Frame and send a run of `07 35` component updates with the avatar's
    /// synch trailer after each one.
    fn send_updates(&self, updates: &[Vec<u8>]) {
        if updates.is_empty() {
            return;
        }
        let mut synch = vec![0x02];
        synch.extend_from_slice(&self.avatar_hp_word().to_le_bytes());
        let stream = build_component_update_stream(updates, &synch);
        self.send(0x07, 0x35, &stream[1..]);
    }

    /// Send an update run and trace its exact bytes. Used on the equipment
    /// paths, where one malformed record length lands the reader mid-payload
    /// (`Code 3`) and the resulting stream cannot be reconstructed from the
    /// server's item state alone.
    fn send_updates_traced(&self, updates: &[Vec<u8>], tag: &str) {
        if updates.is_empty() {
            return;
        }
        let mut synch = vec![0x02];
        synch.extend_from_slice(&self.avatar_hp_word().to_le_bytes());
        let stream = build_component_update_stream(updates, &synch);
        eprintln!(
            "game {}: {tag} stream {}B hex={}",
            self.conn_id,
            stream.len(),
            stream
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        self.send(0x07, 0x35, &stream[1..]);
    }

    /// Queue one recurring payload for the next `0x0D` WorldInterval.
    ///
    /// Full streams (`07 ... 06`) are reduced to their unframed body, which is
    /// exactly what the interval carries between the move-ack and the final
    /// `0x06`. Standalone `0x35 ...` updates are already in that form.
    fn queue_interval(&self, packet: &[u8]) {
        let body = packet
            .strip_prefix(&[0x07])
            .map(|rest| rest.strip_suffix(&[0x06]).unwrap_or(rest))
            .unwrap_or(packet);
        if let Ok(mut queue) = self.pending_interval.lock() {
            queue.push(body.to_vec());
        }
    }

    /// The first-fit free bag cell, mirrored from the client's
    /// `Inventory::findSlot` (x outer, y inner), ignoring `ignore`.
    fn first_free_cell(&self, width: u32, height: u32, ignore: &[u32]) -> Option<(u8, u8)> {
        let (bag_width, bag_height) = self.bag_grid.as_ref()?.dimensions();
        if !rectangle_in_bounds(0, 0, width, height, bag_width, bag_height) {
            return None;
        }
        for x in 0..bag_width {
            if u32::from(x) + width > u32::from(bag_width) {
                break;
            }
            for y in 0..bag_height {
                if u32::from(y) + height > u32::from(bag_height) {
                    break;
                }
                let free = self.bag_entries.values().all(|entry| {
                    ignore.contains(&entry.id)
                        || !rectangles_overlap(
                            (x, y, width, height),
                            (entry.cell.0, entry.cell.1, entry.width, entry.height),
                        )
                });
                if free {
                    return Some((x, y));
                }
            }
        }
        None
    }

    fn handle_equipment_request(&mut self, request: EquipmentRequest) {
        match request {
            EquipmentRequest::Equip { slot, bag_eid } => self.equip_item(slot, bag_eid),
            EquipmentRequest::Unequip { slot } => self.unequip_item(slot),
        }
    }

    fn handle_unit_container_request(&mut self, request: UnitContainerRequest) {
        match request {
            UnitContainerRequest::Get(item) => self.get_bag_item(item),
            UnitContainerRequest::Put { inventory_id, x, y } => {
                self.put_bag_item(inventory_id, x, y)
            }
            UnitContainerRequest::Use(item) => self.use_bag_item(item),
            UnitContainerRequest::Empty(_) | UnitContainerRequest::Currency => {}
            UnitContainerRequest::Drop(item) => self.drop_bag_item(item),
            UnitContainerRequest::UsePos { item, .. }
            | UnitContainerRequest::UseTarget { item, .. } => {
                eprintln!(
                    "game {}: bag use-at item={item} -> HELD (only direct use is handled)",
                    self.conn_id
                );
            }
        }
    }

    /// `UnitContainer 0x23`/`0x24`: drop the item onto the ground at the
    /// player's last mover pose, then register it so a click picks it back up.
    ///
    /// The bag node and the cursor clear in one `(07,35)` stream; the item
    /// itself ships as the live `ItemObject` create (`(07,08)`), the same
    /// builder the loot drop uses.
    fn drop_bag_item(&mut self, item: u32) {
        let Some(entry) = self.bag_entries.get(&item).cloned() else {
            return;
        };
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let Some([x, y]) = self.player_pos else {
            eprintln!(
                "game {}: bag drop item={item} -> HELD (no tracked player pose)",
                self.conn_id
            );
            return;
        };
        let Some(authored_slots) = self.shared.item_readdata_slots(&entry.class) else {
            eprintln!(
                "game {}: bag drop item={item} {} -> HELD (game.pkg cannot settle its Item::readData slots)",
                self.conn_id, entry.class
            );
            return;
        };
        let drop_eid = {
            let Ok(mut world) = self.shared.world.lock() else {
                return;
            };
            world.alloc_entity()
        };
        let owner = self.shared.account();
        let pose = [x as u32, y as u32, 0, 0];
        let level = entry.level.max(1);
        let modifier_classes: Vec<String> = entry
            .modifiers
            .iter()
            .map(|modifier| modifier.class.clone())
            .collect();
        let modifier_level = entry
            .modifiers
            .first()
            .map(|modifier| modifier.level)
            .unwrap_or(level)
            .max(1);
        let body = if drf_proto::is_consumable_class(&entry.class) {
            DroppedItemBody::consumable(level)
        } else {
            DroppedItemBody::equipment(level, authored_slots, modifier_classes, modifier_level)
        };
        let Ok(packet) = build_item_object_create_init(drop_eid, &entry.class, pose, &owner, &body)
        else {
            eprintln!(
                "game {}: bag drop item={item} {} -> HELD (item would not author)",
                self.conn_id, entry.class
            );
            return;
        };
        let cursor = self.cursor == Some(item);
        self.bag_entries.remove(&item);
        if let Some(grid) = self.bag_grid.as_mut() {
            grid.rollback(entry.cell.0, entry.cell.1, entry.width, entry.height);
        }
        if cursor {
            self.cursor = None;
        }
        self.refresh_bag_classes();
        self.persist_bag();
        self.restate_item_objectives(&entry.class);
        let mut updates = Vec::with_capacity(1 + usize::from(cursor));
        if cursor {
            updates.push(build_container_clear_active(container_cid));
        }
        updates.push(build_container_item_remove(container_cid, item));
        self.send_updates(&updates);
        self.send(0x07, 0x08, &packet);
        self.drops.insert(
            drop_eid,
            DropState {
                item_class: entry.class.clone(),
                item_level: Some(entry.level),
                modifiers: entry.modifiers.clone(),
                amount: None,
                pose: [x, y, 0, 0],
            },
        );
        eprintln!(
            "game {}: bag drop item={item} {} as eid={drop_eid:#06x} at {pose:?}",
            self.conn_id, entry.class
        );
    }

    /// Spawn one ground `ItemObject` at `pose` (quest reward loot and drops).
    fn spawn_ground_drop(&mut self, item_class: &str, level: u8, pose: [i32; 4]) -> Option<u16> {
        self.spawn_loot_item(
            &LootItem {
                class: item_class.to_string(),
                level: level.max(1),
                modifiers: Vec::new(),
                amount: None,
            },
            pose,
        )
    }

    /// Spawn a generated loot object. `Currency` uses the native
    /// `Currency::readData` tail; everything else uses the item class's own
    /// authored status run plus the rolled modifier list.
    fn spawn_loot_item(&mut self, item: &LootItem, pose: [i32; 4]) -> Option<u16> {
        // Content aliases (e.g. the Warlock starting staff) are not the
        // client's case-sensitive GC spelling. Author and describe the drop
        // with the canonical file/root spelling.
        let item_class = self
            .shared
            .canonical_item_class(&item.class)
            .unwrap_or_else(|| item.class.clone());
        let drop_eid = {
            let Ok(mut world) = self.shared.world.lock() else {
                return None;
            };
            world.alloc_entity()
        };
        let owner = self.shared.account();
        let wire_pose = [
            pose[0] as u32,
            pose[1] as u32,
            pose[2] as u32,
            pose[3] as u32,
        ];
        let body = if let Some(amount) = item.amount {
            DroppedItemBody::currency(amount)
        } else {
            let authored_slots = self.shared.item_readdata_slots(&item_class)?;
            let modifiers: Vec<String> = item
                .modifiers
                .iter()
                .map(|modifier| modifier.class.clone())
                .collect();
            if drf_proto::is_consumable_class(&item_class) {
                DroppedItemBody::consumable(item.level)
            } else {
                let modifier_level = item
                    .modifiers
                    .first()
                    .map(|modifier| modifier.level)
                    .unwrap_or(item.level)
                    .max(1);
                DroppedItemBody::equipment(item.level, authored_slots, modifiers, modifier_level)
            }
        };
        let Ok(packet) =
            build_item_object_create_init(drop_eid, &item_class, wire_pose, &owner, &body)
        else {
            return None;
        };
        self.send(0x07, 0x08, &packet);
        self.drops.insert(
            drop_eid,
            DropState {
                item_class: item_class.clone(),
                item_level: Some(item.level),
                modifiers: item.modifiers.clone(),
                amount: item.amount,
                pose,
            },
        );
        eprintln!(
            "game {}: reward drop {} as eid={drop_eid:#06x} at {pose:?}",
            self.conn_id, item_class
        );
        Some(drop_eid)
    }

    /// Consume `count` of `class` from the bag, emitting the container updates.
    fn consume_bag_class(&mut self, class: &str, mut count: u32) {
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let ids: Vec<u32> = self
            .bag_entries
            .iter()
            .filter(|(_, entry)| entry.class.eq_ignore_ascii_case(class))
            .map(|(&id, _)| id)
            .collect();
        let mut updates = Vec::new();
        for id in ids {
            if count == 0 {
                break;
            }
            let Some(entry) = self.bag_entries.get(&id).cloned() else {
                continue;
            };
            let take = count.min(entry.count);
            let left = entry.count - take;
            count -= take;
            if left == 0 {
                updates.push(build_container_item_remove(container_cid, id));
                self.bag_entries.remove(&id);
                if let Some(grid) = self.bag_grid.as_mut() {
                    grid.rollback(entry.cell.0, entry.cell.1, entry.width, entry.height);
                }
                if self.cursor == Some(id) {
                    self.cursor = None;
                }
            } else {
                let left_byte = u8::try_from(left).unwrap_or(u8::MAX);
                if let Ok(record) =
                    readdress_item_record(&entry.record, id, entry.cell, Some(left_byte))
                {
                    if let Some(current) = self.bag_entries.get_mut(&id) {
                        current.count = left;
                        current.record = record;
                    }
                    updates.push(build_container_item_quantity(container_cid, id, left_byte));
                }
            }
        }
        if !updates.is_empty() {
            self.refresh_bag_classes();
            self.persist_bag();
            self.send_updates(&updates);
            self.restate_item_objectives(class);
        }
    }

    /// `UnitContainer 0x28`: put the bag item on the cursor (the item stays in
    /// its cell; the cursor is an overlay).
    fn get_bag_item(&mut self, item: u32) {
        let Some(entry) = self.bag_entries.get(&item).cloned() else {
            return;
        };
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let count = u8::try_from(entry.count).unwrap_or(u8::MAX);
        let Ok(record) = readdress_item_record(&entry.record, item, entry.cell, Some(count)) else {
            return;
        };
        self.cursor = Some(item);
        self.send_updates(&[build_container_set_active(container_cid, &record)]);
        eprintln!(
            "game {}: bag GET item={item} {} -> UnitContainer 0x28",
            self.conn_id, entry.class
        );
    }

    /// `UnitContainer 0x29`: move the cursor item to the requested bag cell.
    ///
    /// A destination overlapping exactly one other entry swaps the two; more
    /// than one overlap is refused (there is no coherent single-item swap).
    fn put_bag_item(&mut self, inventory_id: u8, x: u8, y: u8) {
        if inventory_id != MAIN_BAG_INVENTORY_ID {
            return;
        }
        let Some(cursor) = self.cursor else {
            return;
        };
        let Some(moving) = self.bag_entries.get(&cursor).cloned() else {
            self.cursor = None;
            return;
        };
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let Some((bag_width, bag_height)) = self.bag_grid.as_ref().map(BagGrid::dimensions) else {
            return;
        };
        if !rectangle_in_bounds(x, y, moving.width, moving.height, bag_width, bag_height) {
            return;
        }
        let overlaps: Vec<BagEntry> = self
            .bag_entries
            .values()
            .filter(|entry| {
                entry.id != cursor
                    && rectangles_overlap(
                        (x, y, moving.width, moving.height),
                        (entry.cell.0, entry.cell.1, entry.width, entry.height),
                    )
            })
            .cloned()
            .collect();
        if overlaps.len() > 1 {
            eprintln!(
                "game {}: bag PUT item={cursor} -> HELD (destination overlaps {} entries)",
                self.conn_id,
                overlaps.len()
            );
            return;
        }
        let other = overlaps.into_iter().next();
        if let Some(other) = &other {
            let source = moving.cell;
            if !rectangle_in_bounds(
                source.0,
                source.1,
                other.width,
                other.height,
                bag_width,
                bag_height,
            ) {
                return;
            }
            let free = !rectangles_overlap(
                (source.0, source.1, other.width, other.height),
                (x, y, moving.width, moving.height),
            ) && self.bag_entries.values().all(|entry| {
                entry.id == cursor
                    || entry.id == other.id
                    || !rectangles_overlap(
                        (source.0, source.1, other.width, other.height),
                        (entry.cell.0, entry.cell.1, entry.width, entry.height),
                    )
            });
            if !free {
                return;
            }
        }
        let moving_count = u8::try_from(moving.count).unwrap_or(u8::MAX);
        let Ok(moving_record) =
            readdress_item_record(&moving.record, cursor, (x, y), Some(moving_count))
        else {
            return;
        };
        let other_record = match &other {
            Some(other) => {
                let count = u8::try_from(other.count).unwrap_or(u8::MAX);
                match readdress_item_record(&other.record, other.id, moving.cell, Some(count)) {
                    Ok(record) => Some(record),
                    Err(_) => return,
                }
            }
            None => None,
        };
        let mut updates = vec![
            build_container_clear_active(container_cid),
            build_container_item_remove(container_cid, cursor),
        ];
        if let Some(other) = &other {
            updates.push(build_container_item_remove(container_cid, other.id));
        }
        let Ok(add) =
            build_container_item_add(container_cid, &moving_record, cursor, (x, y), moving.count)
        else {
            return;
        };
        updates.push(add);
        if let (Some(other), Some(record)) = (&other, &other_record) {
            let Ok(add) =
                build_container_item_add(container_cid, record, other.id, moving.cell, other.count)
            else {
                return;
            };
            updates.push(add);
        }
        if let Some(grid) = self.bag_grid.as_mut() {
            grid.rollback(moving.cell.0, moving.cell.1, moving.width, moving.height);
            if let Some(other) = &other {
                grid.rollback(other.cell.0, other.cell.1, other.width, other.height);
            }
            grid.occupy(x, y, moving.width, moving.height);
            if let Some(other) = &other {
                grid.occupy(moving.cell.0, moving.cell.1, other.width, other.height);
            }
        }
        if let Some(entry) = self.bag_entries.get_mut(&cursor) {
            entry.cell = (x, y);
            entry.record = moving_record;
        }
        if let (Some(other), Some(record)) = (&other, other_record) {
            if let Some(entry) = self.bag_entries.get_mut(&other.id) {
                entry.cell = moving.cell;
                entry.record = record;
            }
        }
        self.cursor = None;
        self.persist_bag();
        self.send_updates(&updates);
        eprintln!(
            "game {}: bag PUT item={cursor} {} ({},{}) -> ({x},{y}){}",
            self.conn_id,
            moving.class,
            moving.cell.0,
            moving.cell.1,
            if other.is_some() { " with swap" } else { "" }
        );
    }

    /// `UnitContainer 0x25`: consume one from a consumable stack.
    fn use_bag_item(&mut self, item: u32) {
        let Some(entry) = self.bag_entries.get(&item).cloned() else {
            return;
        };
        let lower = entry.class.to_ascii_lowercase();
        let is_known_potion = lower.contains("potion") || lower.contains("consumable");
        if !is_known_potion && !self.shared.item_is_consumable(&entry.class) {
            eprintln!(
                "game {}: bag USE item={item} {} -> HELD (not consumable)",
                self.conn_id, entry.class
            );
            return;
        }
        let Some(avatar_eid) = self.avatar_eid else {
            return;
        };
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let count = u8::try_from(entry.count).unwrap_or(u8::MAX);
        let Ok(record) = readdress_item_record(&entry.record, item, entry.cell, Some(count)) else {
            return;
        };
        let mut synch = vec![0x02];
        synch.extend_from_slice(&self.avatar_hp_word().to_le_bytes());
        let use_frame = build_unit_use_item_update(avatar_eid, &record, &synch);
        let left = entry.count.saturating_sub(1);
        let stack_update = if left == 0 {
            build_container_item_remove(container_cid, item)
        } else {
            let left_byte = u8::try_from(left).unwrap_or(u8::MAX);
            match readdress_item_record(&entry.record, item, entry.cell, Some(left_byte)) {
                Ok(record) => {
                    if let Some(current) = self.bag_entries.get_mut(&item) {
                        current.count = left;
                        current.record = record;
                    }
                    build_container_item_quantity(container_cid, item, left_byte)
                }
                Err(_) => return,
            }
        };
        if left == 0 {
            self.bag_entries.remove(&item);
            if let Some(grid) = self.bag_grid.as_mut() {
                grid.rollback(entry.cell.0, entry.cell.1, entry.width, entry.height);
            }
            if self.cursor == Some(item) {
                self.cursor = None;
            }
        }
        self.refresh_bag_classes();
        self.persist_bag();
        self.send(0x07, 0x03, &use_frame[2..]);
        self.send_updates(&[stack_update]);
        self.restate_item_objectives(&entry.class);
        eprintln!(
            "game {}: bag USE item={item} {} stack {count} -> {left}",
            self.conn_id, entry.class
        );
        if lower.contains("townportal") || lower.contains("waypointscroll") {
            self.open_town_portal();
        }
    }

    /// Spawn the clickable blue town portal and save the return point.
    ///
    /// The portal is parented to the avatar and registered for the next
    /// activate click; the `QuestManager 0x0A` update lights the obelisk's
    /// "Saved Places" entry so a later click can return to this zone.
    fn open_town_portal(&mut self) {
        let Some(avatar_eid) = self.avatar_eid else {
            return;
        };
        let Some([x, y]) = self.player_pos else {
            return;
        };
        let entity_id = {
            let Ok(mut world) = self.shared.world.lock() else {
                return;
            };
            world.alloc_entity()
        };
        let zone = self.zone.clone();
        let Ok(packet) = build_town_portal_create_init(
            entity_id,
            avatar_eid,
            [x, y, 0],
            ZONE_TOWN,
            0,
            0x06,
            0x02,
        ) else {
            return;
        };
        self.town_portals.insert(entity_id, ZONE_TOWN.to_string());
        self.town_portal_return = Some((zone.clone(), [x, y]));
        self.send(0x07, 0x08, &packet);
        self.send_saved_place(&zone);
        eprintln!(
            "game {}: town portal eid={entity_id:#06x} at ({x},{y}) zone={zone}",
            self.conn_id
        );
    }

    /// QuestManager `0x0A`: record `zone` as the town's saved place for the
    /// obelisk's return. Sent when the scroll is cast and again after a town
    /// join, because the join's fresh QuestManager init resets the saved place.
    fn send_saved_place(&self, zone: &str) {
        let Some(quest_cid) = self.quest_cid else {
            return;
        };
        let mut body = vec![0x0A, 0x01];
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(zone.as_bytes());
        body.push(0);
        body.push(0);
        body.push(0x00);
        body.push(0x06);
        let mut update = vec![0x07, 0x35];
        update.extend_from_slice(&quest_cid.to_le_bytes());
        update.extend_from_slice(&body);
        self.send(0x07, 0x35, &update[2..]);
    }

    /// Equipment `0x28`: move a bag item onto the paper doll.
    fn equip_item(&mut self, slot: u32, bag_eid: u32) {
        if !EQUIPMENT_SLOTS.contains(&slot) {
            return;
        }
        let Some(equipment_cid) = self.equipment_cid else {
            return;
        };
        let Some(manipulators_cid) = self.manipulators_cid else {
            return;
        };
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let item = if bag_eid != 0 && self.bag_entries.contains_key(&bag_eid) {
            bag_eid
        } else if let Some(cursor) = self.cursor {
            cursor
        } else {
            eprintln!(
                "game {}: equipment EQUIP slot={slot} -> HELD (no item named and no cursor)",
                self.conn_id
            );
            return;
        };
        let Some(entry) = self.bag_entries.get(&item).cloned() else {
            return;
        };
        // Re-author from the class rather than trusting a persisted record:
        // starting-equipment aliases (e.g. `1HStaff1Pal`) and older rows may
        // carry the wrong case or length. The canonical class spelling is what
        // the client's case-sensitive GC registry resolves.
        let class = self
            .shared
            .canonical_item_class(&entry.class)
            .unwrap_or_else(|| entry.class.clone());
        let modifiers: Vec<(String, u8)> = entry
            .modifiers
            .iter()
            .map(|modifier| (modifier.class.clone(), modifier.level))
            .collect();
        let record = match self.shared.author_item_record_with_mods(
            &class,
            slot,
            1,
            Some(entry.level),
            (drf_proto::ITEM_GRID_UNPLACED, drf_proto::ITEM_GRID_UNPLACED),
            &modifiers,
        ) {
            Ok(Some(mut record)) => {
                if let Err(err) = record.set_flags(entry.flags) {
                    eprintln!(
                        "game {}: equipment EQUIP {} -> HELD: {err}",
                        self.conn_id, entry.class
                    );
                    return;
                }
                record.record
            }
            Ok(None) => {
                eprintln!(
                    "game {}: equipment EQUIP {} -> HELD (no Item block)",
                    self.conn_id, entry.class
                );
                return;
            }
            Err(err) => {
                eprintln!(
                    "game {}: equipment EQUIP {} -> HELD: {err}",
                    self.conn_id, entry.class
                );
                return;
            }
        };
        let Some(dfc) = self.shared.item_dfc_root(&class) else {
            eprintln!(
                "game {}: equipment EQUIP {} -> HELD (no DFC root for its manipulator)",
                self.conn_id, entry.class
            );
            return;
        };
        let Ok(readstate) = item_manipulator_readstate(&dfc) else {
            return;
        };
        let mut manip_record = record.clone();
        manip_record.extend_from_slice(&readstate);
        let old = self.worn.get(&slot).cloned();
        let old_cell = match &old {
            Some(old) => match self.first_free_cell(old.width, old.height, &[item]) {
                Some(cell) => Some(cell),
                None => {
                    eprintln!(
                        "game {}: equipment EQUIP slot={slot} -> HELD (no bag cell for the old item)",
                        self.conn_id
                    );
                    return;
                }
            },
            None => None,
        };
        let old_bag_id = if old.is_some() {
            Some(self.alloc_bag_id())
        } else {
            None
        };
        let old_record = match (&old, old_bag_id, old_cell) {
            (Some(old), Some(id), Some(cell)) => {
                match readdress_item_record(&old.record, id, cell, Some(1)) {
                    Ok(record) => Some(record),
                    Err(_) => return,
                }
            }
            _ => None,
        };
        let mut updates = Vec::new();
        if let (Some(_old), Some(id), Some(cell), Some(record)) =
            (&old, old_bag_id, old_cell, &old_record)
        {
            updates.push(build_equipment_item_remove(equipment_cid, slot));
            let Ok(add) = build_container_item_add(container_cid, record, id, cell, 1) else {
                return;
            };
            updates.push(add);
            updates.push(build_manipulators_remove(manipulators_cid, slot));
        }
        let Ok(new_add) = build_equipment_item_add(equipment_cid, &record, slot) else {
            return;
        };
        updates.push(new_add);
        updates.push(build_container_clear_active(container_cid));
        updates.push(build_container_item_remove(container_cid, item));
        // `Manipulators::processAddManipulator` @0x4FD2A0 reads only the
        // Item::readData record; the vtable+0x100 readState body belongs to
        // the JOIN's readInit shape, not this incremental add. Appending it
        // here leaves five bytes for the CEM reader to parse as the next
        // message type (live Code 3 on every weapon equip, 2026-10-20).
        updates.push(build_manipulators_add(manipulators_cid, &record));
        if let Some(moved) = self.bag_entries.remove(&item) {
            if let Some(grid) = self.bag_grid.as_mut() {
                grid.rollback(moved.cell.0, moved.cell.1, moved.width, moved.height);
            }
        }
        if let (Some(old), Some(id), Some(cell), Some(record)) =
            (old, old_bag_id, old_cell, old_record)
        {
            if let Some(grid) = self.bag_grid.as_mut() {
                grid.occupy(cell.0, cell.1, old.width, old.height);
            }
            self.bag_entries.insert(
                id,
                BagEntry {
                    id,
                    class: old.class,
                    cell,
                    count: 1,
                    level: old.level,
                    flags: old.flags,
                    width: old.width,
                    height: old.height,
                    record,
                    modifiers: old.modifiers.clone(),
                },
            );
        }
        self.worn.insert(
            slot,
            WornEntry {
                slot,
                class: class.clone(),
                level: entry.level,
                flags: entry.flags,
                width: entry.width,
                height: entry.height,
                record,
                manip_record,
                modifiers: entry.modifiers.clone(),
            },
        );
        self.cursor = None;
        self.refresh_bag_classes();
        self.persist_bag();
        self.persist_worn();
        self.send_updates_traced(&updates, "equipment-equip");
        eprintln!(
            "game {}: equipment EQUIP slot={slot} {} from bag item={item}",
            self.conn_id, entry.class
        );
    }

    /// Equipment `0x29`: move a worn item into the first free bag cell.
    fn unequip_item(&mut self, slot: u32) {
        if !EQUIPMENT_SLOTS.contains(&slot) {
            return;
        }
        let Some(equipment_cid) = self.equipment_cid else {
            return;
        };
        let Some(manipulators_cid) = self.manipulators_cid else {
            return;
        };
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let Some(worn) = self.worn.get(&slot).cloned() else {
            return;
        };
        let Some(cell) = self.first_free_cell(worn.width, worn.height, &[]) else {
            eprintln!(
                "game {}: equipment UNEQUIP slot={slot} -> HELD (bag full)",
                self.conn_id
            );
            return;
        };
        let id = self.alloc_bag_id();
        let Ok(record) = readdress_item_record(&worn.record, id, cell, Some(1)) else {
            return;
        };
        let updates = [
            build_equipment_item_remove(equipment_cid, slot),
            match build_container_item_add(container_cid, &record, id, cell, 1) {
                Ok(add) => add,
                Err(_) => return,
            },
            build_manipulators_remove(manipulators_cid, slot),
        ];
        self.worn.remove(&slot);
        if let Some(grid) = self.bag_grid.as_mut() {
            grid.occupy(cell.0, cell.1, worn.width, worn.height);
        }
        self.bag_entries.insert(
            id,
            BagEntry {
                id,
                class: worn.class.clone(),
                cell,
                count: 1,
                level: worn.level,
                flags: worn.flags,
                width: worn.width,
                height: worn.height,
                record,
                modifiers: worn.modifiers.clone(),
            },
        );
        self.refresh_bag_classes();
        self.persist_bag();
        self.persist_worn();
        self.send_updates_traced(&updates, "equipment-unequip");
        eprintln!(
            "game {}: equipment UNEQUIP slot={slot} {} -> bag item={id} cell=({},{})",
            self.conn_id, worn.class, cell.0, cell.1
        );
    }

    /// `Skills` `0x35`/`0x36`: bind a skill to a hotbar slot or clear it.
    fn handle_skills_request(&mut self, request: SkillRequest) {
        let Some(manipulators_cid) = self.manipulators_cid else {
            return;
        };
        match request {
            SkillRequest::Buy { hash } => {
                let Some(cid) = self.skills_cid else {
                    return;
                };
                let Some((fqn, _, starting_level)) = self
                    .skill_catalog
                    .iter()
                    .find(|(fqn, _, _)| dfc_hash(fqn.as_bytes()) == hash)
                    .cloned()
                else {
                    eprintln!(
                        "game {}: skill BUY hash=0x{hash:08X} -> HELD (unknown skill)",
                        self.conn_id
                    );
                    return;
                };
                if self.skill_points == 0 {
                    eprintln!(
                        "game {}: skill BUY {fqn} -> HELD (no unspent skill points)",
                        self.conn_id
                    );
                    return;
                }
                let current = self
                    .skill_levels
                    .get(&fqn)
                    .copied()
                    .unwrap_or(starting_level);
                let next = current.saturating_add(1);
                if next <= current || next == 255 {
                    return;
                }
                self.skill_levels.insert(fqn.clone(), next);
                if let Some((_, _, level)) = self
                    .skill_catalog
                    .iter_mut()
                    .find(|(known, _, _)| known.eq_ignore_ascii_case(&fqn))
                {
                    *level = next;
                }
                self.skill_points -= 1;
                if let Some(sqlid) = self.selected {
                    let account = self.shared.account();
                    if let Ok(mut store) = self.shared.store.lock() {
                        let _ = store.set_skill_state(
                            &account,
                            sqlid,
                            self.skill_points,
                            &self.skill_levels,
                        );
                    }
                }
                self.send_updates(&[
                    build_skills_skill_update(cid, hash, next),
                    build_skills_points_update(cid, self.skill_points),
                ]);
                eprintln!(
                    "game {}: skill BUY {fqn} -> level {next} ({} points left)",
                    self.conn_id, self.skill_points
                );
            }
            SkillRequest::Place { slot, hash } => {
                if !HOTBAR_SLOTS.contains(&slot) {
                    return;
                }
                let Some((fqn, _, level)) = self
                    .skill_catalog
                    .iter()
                    .find(|(fqn, _, _)| dfc_hash(fqn.as_bytes()) == hash)
                    .cloned()
                else {
                    eprintln!(
                        "game {}: hotbar PLACE slot={slot} hash=0x{hash:08X} -> HELD (unknown skill)",
                        self.conn_id
                    );
                    return;
                };
                let displaced = self
                    .skill_slots
                    .get(&slot)
                    .filter(|existing| *existing != &fqn)
                    .cloned();
                self.skill_slots.retain(|_, existing| existing != &fqn);
                self.skill_slots.insert(slot, fqn.clone());
                self.persist_hotbar();
                let record = skill_manipulator_update_entry(&fqn, slot, level);
                self.send_updates(&[build_manipulators_add(manipulators_cid, &record)]);
                eprintln!(
                    "game {}: hotbar PLACE slot={slot} = {fqn} (level {level})",
                    self.conn_id
                );
                if Self::is_gnome_skill(&fqn) {
                    self.gnome_spawn();
                }
                if displaced.as_deref().is_some_and(Self::is_gnome_skill)
                    && !self
                        .skill_slots
                        .values()
                        .any(|skill| Self::is_gnome_skill(skill))
                {
                    self.gnome_despawn(true);
                }
            }
            SkillRequest::Remove { slot } => {
                if !HOTBAR_SLOTS.contains(&slot) {
                    return;
                }
                let removed = self.skill_slots.remove(&slot);
                self.persist_hotbar();
                self.send_updates(&[build_manipulators_remove(manipulators_cid, slot)]);
                eprintln!("game {}: hotbar REMOVE slot={slot}", self.conn_id);
                if removed.as_deref().is_some_and(Self::is_gnome_skill)
                    && !self
                        .skill_slots
                        .values()
                        .any(|skill| Self::is_gnome_skill(skill))
                {
                    self.gnome_despawn(true);
                }
            }
        }
    }

    /// Is `fqn` the one server-summoned Bling Gnome skill?
    fn is_gnome_skill(fqn: &str) -> bool {
        fqn.eq_ignore_ascii_case(drf_content::BLING_GNOME_SKILL)
    }

    /// Spawn the gnome next to the owner and send its entity snapshot. The
    /// component packet follows on the next read-loop tick (the reference's
    /// 66 ms `DelayedBehaviorCreate`).
    fn gnome_spawn(&mut self) {
        let Some(profile) = self.gnome_profile.clone() else {
            return;
        };
        if self.gnome.is_some() {
            return;
        }
        let Some(avatar_eid) = self.avatar_eid else {
            return;
        };
        let owner_ref = self.player_eid.unwrap_or(avatar_eid);
        let level = u8::try_from(self.selected_level.clamp(1, 255)).unwrap_or(1);
        let hp_wire = gnome_hp_wire(level, profile.max_health);
        let [x, y] = self.player_pos.unwrap_or([0, 0]);
        let pos = [x.saturating_add(5 * 256), y.saturating_add(5 * 256), 0];
        let (entity_id, modifiers_id, manipulators_id, behavior_id) = {
            let Ok(mut world) = self.shared.world.lock() else {
                return;
            };
            (
                world.alloc_entity(),
                world.alloc_component(),
                world.alloc_component(),
                world.alloc_component(),
            )
        };
        let packet = build_entity_snapshot(entity_id, owner_ref, level, hp_wire, pos, 0);
        self.send_inner(&packet);
        let now = Instant::now();
        self.gnome = Some(GnomeState {
            entity_id,
            behavior_id,
            modifiers_id,
            manipulators_id,
            hp_wire,
            pos,
            heading: 0,
            spawn_pos: [pos[0], pos[1]],
            spawn_heading: 0,
            component_at: Some(now + Duration::from_millis(66)),
            behavior_created: false,
            last_follow: now,
            last_mover_at: None,
            mover: None,
            next_search: now + Duration::from_secs(1),
            next_fidget: now + Duration::from_secs(2),
            last_fidget: 0,
            active_until: None,
            convert_at: None,
            convert_queue: Vec::new(),
            last_activate: None,
            items_converted: 0,
            gold_generated: 0,
            remove_at: None,
        });
        eprintln!(
            "game {}: Bling Gnome summoned eid={entity_id:#06x} level={level} hp={hp_wire} at {pos:?}",
            self.conn_id
        );
    }

    /// Remove the gnome; `animate` plays its death action first and delays the
    /// CEM remove by the reference's 0.8 s.
    fn gnome_despawn(&mut self, animate: bool) {
        let Some((entity_id, behavior_id, hp_wire, created)) = self
            .gnome
            .as_ref()
            .map(|g| (g.entity_id, g.behavior_id, g.hp_wire, g.behavior_created))
        else {
            return;
        };
        if animate && created {
            let death = build_play_animation(behavior_id, hp_wire, ANIM_DEATH, 0x00, 24);
            self.queue_interval(&death);
            if let Some(g) = self.gnome.as_mut() {
                g.remove_at = Some(Instant::now() + Duration::from_millis(800));
            }
            eprintln!(
                "game {}: Bling Gnome death animation eid={entity_id:#06x}",
                self.conn_id
            );
        } else {
            let remove = build_entity_remove(entity_id);
            self.send_inner(&remove);
            self.gnome = None;
            eprintln!(
                "game {}: Bling Gnome removed eid={entity_id:#06x}",
                self.conn_id
            );
        }
    }

    /// Open the 10 s convert window, or summon the gnome when none is live.
    fn gnome_toggle(&mut self) {
        if self.gnome.is_some() {
            self.gnome_activate();
        } else {
            self.gnome_spawn();
        }
    }

    /// Cast the gnome's active skill: PlayAnimation 140 + the `0xA1` action.
    fn gnome_activate(&mut self) {
        let Some(profile) = self.gnome_profile.clone() else {
            return;
        };
        let now = Instant::now();
        let Some(g) = self.gnome.as_ref() else {
            return;
        };
        if !g.behavior_created || g.remove_at.is_some() {
            return;
        }
        if g.last_activate.is_some_and(|last| {
            now.duration_since(last) < Duration::from_secs_f64(profile.cooldown_seconds)
        }) {
            eprintln!("game {}: Bling Gnome cast HELD (cooldown)", self.conn_id);
            return;
        }
        let behavior_id = g.behavior_id;
        let hp_wire = g.hp_wire;
        let center = self.player_pos.unwrap_or([g.pos[0], g.pos[1]]);
        let anim = build_play_animation(behavior_id, hp_wire, ANIM_ACTIVE_SKILL, 0x06, 40);
        let action = build_convert_action(
            behavior_id,
            hp_wire,
            center,
            profile.search_radius.min(u32::from(u16::MAX)) as u16,
            profile.conversion_ratio,
        );
        self.queue_interval(&anim);
        self.queue_interval(&action);
        if let Some(g) = self.gnome.as_mut() {
            g.last_activate = Some(now);
            g.active_until = Some(now + Duration::from_secs_f64(profile.effect_duration_seconds));
            // Reference `_apply_active_conversion`: 0.15 s admission plus the
            // 2.05 s bounce before the first item is taken, so the active
            // animation and the gnome's pickup beats are visible.
            g.convert_at = Some(now + Duration::from_millis(2200));
            g.convert_queue.clear();
        }
        eprintln!(
            "game {}: Bling Gnome convert window open {}s radius={} ratio={:.2}",
            self.conn_id,
            profile.effect_duration_seconds,
            profile.search_radius,
            profile.conversion_ratio
        );
    }

    /// One read-loop tick (the session wakes on a 100 ms read timeout).
    fn gnome_tick(&mut self, now: Instant) {
        let Some(profile) = self.gnome_profile.clone() else {
            return;
        };
        let Some(g) = self.gnome.as_ref() else {
            return;
        };

        if g.remove_at.is_some_and(|at| now >= at) {
            let remove = build_entity_remove(g.entity_id);
            let eid = g.entity_id;
            self.send_inner(&remove);
            self.gnome = None;
            eprintln!("game {}: Bling Gnome removed eid={eid:#06x}", self.conn_id);
            return;
        }
        if g.remove_at.is_some() {
            return;
        }
        if g.component_at.is_some_and(|at| now >= at) {
            let packet = build_component_packet(
                g.entity_id,
                g.modifiers_id,
                g.manipulators_id,
                g.behavior_id,
                &profile.behavior,
                [g.pos[0], g.pos[1]],
                g.hp_wire,
            );
            self.send_inner(&packet);
            if let Some(g) = self.gnome.as_mut() {
                g.component_at = None;
                g.behavior_created = true;
            }
            eprintln!(
                "game {}: Bling Gnome behavior components shipped",
                self.conn_id
            );
        }

        self.gnome_follow(&profile, now);

        let expired = self
            .gnome
            .as_ref()
            .and_then(|g| g.active_until)
            .is_some_and(|at| now >= at);
        if expired {
            if let Some(g) = self.gnome.as_mut() {
                g.active_until = None;
                g.convert_queue.clear();
            }
        }

        let collect_due = self
            .gnome
            .as_ref()
            .and_then(|g| g.convert_at)
            .is_some_and(|at| now >= at);
        if collect_due {
            let candidates = self.gnome_convert_candidates(&profile);
            if let Some(g) = self.gnome.as_mut() {
                g.convert_at = None;
                g.convert_queue = candidates;
            }
        }
        let next = self
            .gnome
            .as_ref()
            .and_then(|g| g.convert_queue.first().copied());
        if let Some(eid) = next {
            if let Some(g) = self.gnome.as_mut() {
                g.convert_queue.remove(0);
            }
            self.gnome_convert_one(eid, &profile);
        }

        let search_due = self.gnome.as_ref().is_some_and(|g| now >= g.next_search);
        if search_due {
            if let Some(g) = self.gnome.as_mut() {
                g.next_search = now + Duration::from_secs(1);
            }
            self.gnome_pick_up_gold(&profile);
        }

        let fidget_due = self.gnome.as_ref().is_some_and(|g| {
            g.behavior_created && now >= g.next_fidget && g.active_until.is_none()
        });
        if fidget_due {
            let (behavior_id, hp_wire, last) = match self.gnome.as_ref() {
                Some(g) => (g.behavior_id, g.hp_wire, g.last_fidget),
                None => return,
            };
            let candidates = [
                ANIM_FIDGET_FIRST,
                ANIM_FIDGET_FIRST + 1,
                ANIM_FIDGET_FIRST + 2,
                ANIM_FIDGET_LAST,
            ];
            let index = (self.conn_id as usize + last as usize) % candidates.len();
            let mut anim = candidates[index];
            if anim == last {
                anim = candidates[(index + 1) % candidates.len()];
            }
            let packet = build_play_animation(behavior_id, hp_wire, anim, 0x08, 40);
            self.queue_interval(&packet);
            if let Some(g) = self.gnome.as_mut() {
                g.last_fidget = anim;
                g.next_fidget = now + Duration::from_secs(9);
            }
        }
    }

    fn gnome_follow(&mut self, profile: &BlingGnomeProfile, now: Instant) {
        let Some([px, py]) = self.player_pos else {
            return;
        };
        let Some(g) = self.gnome.as_ref() else {
            return;
        };
        if !g.behavior_created {
            return;
        }
        let mut pos = g.pos;
        let mut heading = g.heading;
        let dx = i64::from(px) - i64::from(pos[0]);
        let dy = i64::from(py) - i64::from(pos[1]);
        let dist = ((dx * dx + dy * dy) as f64).sqrt();
        let follow_radius = 30.0 * 256.0;
        let moved = dist > follow_radius;
        if moved {
            let elapsed = now
                .saturating_duration_since(g.last_follow)
                .as_secs_f64()
                .clamp(0.0, 0.132);
            let step = (profile.speed * 256.0 * elapsed).max(1.0).min(dist);
            let scale = step / dist;
            pos[0] = (i64::from(pos[0]) + (dx as f64 * scale) as i64) as i32;
            pos[1] = (i64::from(pos[1]) + (dy as f64 * scale) as i64) as i32;
            pos[2] = g.pos[2];
            heading = ((dy as f64).atan2(dx as f64).to_degrees() * 256.0) as i32;
        }
        let changed = moved || heading != g.heading;
        let send_due = g
            .last_mover_at
            .is_none_or(|at| now.duration_since(at) >= Duration::from_millis(300));
        let terminal_due = !moved && g.mover.is_some_and(|(_, _, terminal)| !terminal);
        let (old, old_heading) = g
            .mover
            .map(|(mpos, mheading, _)| (mpos, mheading))
            .unwrap_or(([g.spawn_pos[0], g.spawn_pos[1], 0], g.spawn_heading));
        if (changed || terminal_due) && send_due {
            let terminal = !moved;
            let packet = build_mover_update(
                g.behavior_id,
                g.hp_wire,
                old,
                old_heading,
                pos,
                heading,
                terminal,
            );
            self.queue_interval(&packet);
            if let Some(g) = self.gnome.as_mut() {
                g.last_follow = now;
                g.last_mover_at = Some(now);
                g.mover = Some((pos, heading, terminal));
            }
        }
        if let Some(g) = self.gnome.as_mut() {
            g.pos = pos;
            g.heading = heading;
            if moved {
                g.last_follow = now;
            }
        }
    }

    /// Gold drops are collected passively from the gnome or owner position.
    fn gnome_pick_up_gold(&mut self, profile: &BlingGnomeProfile) {
        if self.gnome.as_ref().is_some_and(|g| !g.behavior_created) {
            return;
        }
        let (Some(gpos), Some([px, py])) = (
            self.gnome.as_ref().map(|g| (g.pos[0], g.pos[1])),
            self.player_pos,
        ) else {
            return;
        };
        let radius = i64::from(profile.search_range) * 256;
        let radius_sq = radius * radius;
        let mut best: Option<(u16, u32, i64)> = None;
        for (eid, drop) in &self.drops {
            let Some(amount) = drop.amount else {
                continue;
            };
            let dx_g = i64::from(drop.pose[0]) - i64::from(gpos.0);
            let dy_g = i64::from(drop.pose[1]) - i64::from(gpos.1);
            let dx_p = i64::from(drop.pose[0]) - i64::from(px);
            let dy_p = i64::from(drop.pose[1]) - i64::from(py);
            let dg = dx_g * dx_g + dy_g * dy_g;
            let dp = dx_p * dx_p + dy_p * dy_p;
            let d = dg.min(dp);
            if d <= radius_sq && best.is_none_or(|(_, _, old)| d < old) {
                best = Some((*eid, amount, d));
            }
        }
        let Some((eid, amount, _)) = best else {
            return;
        };
        if self.drops.remove(&eid).is_none() {
            return;
        }
        let remove = build_entity_remove(eid);
        self.send_inner(&remove);
        self.grant_gnome_gold(amount, "gold pickup");
        if let Some(g) = self.gnome.as_mut() {
            g.items_converted = g.items_converted.saturating_add(1);
            g.gold_generated = g.gold_generated.saturating_add(amount);
        }
        eprintln!(
            "game {}: Bling Gnome picked up {amount}g eid={eid:#06x}",
            self.conn_id
        );
    }

    fn gnome_convert_candidates(&self, profile: &BlingGnomeProfile) -> Vec<u16> {
        let (Some(g), Some([px, py])) = (
            self.gnome.as_ref().map(|g| (g.pos[0], g.pos[1])),
            self.player_pos,
        ) else {
            return Vec::new();
        };
        let radius = i64::from(profile.search_radius) * 256;
        let radius_sq = radius * radius;
        let mut out = Vec::new();
        for (eid, drop) in &self.drops {
            if drop.amount.is_some() {
                continue;
            }
            let dx_g = i64::from(drop.pose[0]) - i64::from(g.0);
            let dy_g = i64::from(drop.pose[1]) - i64::from(g.1);
            let dx_p = i64::from(drop.pose[0]) - i64::from(px);
            let dy_p = i64::from(drop.pose[1]) - i64::from(py);
            if dx_g * dx_g + dy_g * dy_g > radius_sq && dx_p * dx_p + dy_p * dy_p > radius_sq {
                continue;
            }
            let modifiers: Vec<String> = drop
                .modifiers
                .iter()
                .map(|modifier| modifier.class.clone())
                .collect();
            let quality = self.shared.item_quality(&drop.item_class, &modifiers);
            // The client's ConvertItemsToGold filter rejects quality 0 (quest)
            // and 6 (relic/collection); everything else is fair game.
            if matches!(quality, Some(0 | 6)) {
                continue;
            }
            let class = drop.item_class.to_ascii_lowercase();
            if class.contains("questitem") {
                continue;
            }
            out.push(*eid);
        }
        out.sort_unstable();
        out
    }

    fn gnome_convert_one(&mut self, eid: u16, profile: &BlingGnomeProfile) {
        let Some(drop) = self.drops.remove(&eid) else {
            return;
        };
        let level = drop.item_level.unwrap_or(1);
        let modifiers: Vec<String> = drop
            .modifiers
            .iter()
            .map(|modifier| modifier.class.clone())
            .collect();
        let quality = self.shared.item_quality(&drop.item_class, &modifiers);
        let base = self
            .shared
            .item_native_value(
                &drop.item_class,
                u32::from(level),
                self.selected_level,
                quality,
            )
            .unwrap_or(1);
        let gold =
            ((f64::from(base) * profile.conversion_ratio * profile.gold_value_mod).max(1.0)) as u32;
        let pickup = self
            .gnome
            .as_ref()
            .map(|g| build_play_animation(g.behavior_id, g.hp_wire, ANIM_PICKUP, 0x00, 16));
        if let Some(packet) = pickup {
            self.queue_interval(&packet);
        }
        let remove = build_entity_remove(eid);
        self.send_inner(&remove);
        self.grant_gnome_gold(gold, "item conversion");
        if let Some(g) = self.gnome.as_mut() {
            g.items_converted = g.items_converted.saturating_add(1);
            g.gold_generated = g.gold_generated.saturating_add(gold);
        }
        eprintln!(
            "game {}: Bling Gnome converted {} L{} eid={eid:#06x} -> {gold}g",
            self.conn_id, drop.item_class, level
        );
    }

    fn grant_gnome_gold(&mut self, amount: u32, reason: &str) {
        if amount == 0 {
            return;
        }
        let Some(container_cid) = self.avatar_container_cid else {
            return;
        };
        let Ok(delta) = i32::try_from(amount) else {
            return;
        };
        let update =
            build_container_add_currency_notify(container_cid, delta, self.avatar_eid.unwrap_or(0));
        self.gold = self.gold.saturating_add(amount);
        self.persist_gold(self.gold);
        self.send_updates(&[update]);
        eprintln!(
            "game {}: Bling Gnome {reason} +{amount} gold (purse={})",
            self.conn_id, self.gold
        );
    }

    /// Replace the persisted hotbar with a snapshot of the live slot map.
    fn persist_hotbar(&self) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let mut slots: Vec<(&u32, &String)> = self.skill_slots.iter().collect();
        slots.sort_by_key(|(slot, _)| **slot);
        let rows: Vec<HotbarSlot> = slots
            .into_iter()
            .map(|(slot, skill)| HotbarSlot {
                slot: *slot,
                skill: skill.clone(),
            })
            .collect();
        if let Ok(mut store) = self.shared.store.lock() {
            let _ = store.set_hotbar(&account, sqlid, rows);
        }
    }

    /// Whether every authored objective is complete. This is the one
    /// satisfaction rule: each kind reads its own authority, never a shared
    /// per-quest counter.
    fn quest_ready(&self, quest: &drf_content::QuestDef, instance: u32, qid: u32) -> bool {
        if quest.objectives.is_empty() {
            return true;
        }
        let Some(sqlid) = self.selected else {
            return false;
        };
        let account = self.shared.account();
        let record = self
            .shared
            .store
            .lock()
            .ok()
            .and_then(|store| store.quest_get(&account, sqlid, qid));
        let Some(record) = record.filter(|record| record.instance_ref == instance) else {
            return false;
        };
        self.quest_record_all_done(quest, &record)
    }

    /// The current server-side count for one objective. Counted event kinds
    /// persist their `(flags, label)` pair; item/arrival kinds are derived.
    fn objective_current(
        &self,
        objective: &drf_content::QuestObjective,
        stored: Option<&QuestObjective>,
    ) -> Option<u32> {
        match objective.kind.as_str() {
            "ItemObjective" => Some(self.bag_count(objective.item.as_deref()?)),
            "KillObjective" | "ActivateObjective" => stored.map(|stored| {
                if stored.flags & 0x01 != 0 {
                    objective.required
                } else {
                    Self::label_current(&stored.label).unwrap_or(0)
                }
            }),
            "GoToObjective" => Some(u32::from(
                stored.is_some_and(|stored| stored.flags & 0x01 != 0),
            )),
            _ => None,
        }
    }

    fn quest_record_all_done(
        &self,
        quest: &drf_content::QuestDef,
        record: &crate::store::QuestRecord,
    ) -> bool {
        quest.objectives.is_empty()
            || quest
                .objectives
                .iter()
                .enumerate()
                .all(|(index, objective)| {
                    let required = objective.required.max(1);
                    self.objective_current(objective, record.objectives.get(index))
                        .is_some_and(|current| current >= required)
                })
    }

    fn bag_count(&self, item: &str) -> u32 {
        self.bag_entries
            .values()
            .filter(|entry| self.shared.class_is_kind_of(&entry.class, item))
            .map(|entry| entry.count)
            .sum()
    }

    /// The current count encoded in an authored objective label, e.g.
    /// `"Slay Dew Valley Wolves: 3 / 6"` -> `3`.
    fn label_current(label: &str) -> Option<u32> {
        let (_, after) = label.rsplit_once(": ")?;
        let (current, _) = after.split_once(" / ")?;
        current.trim().parse().ok()
    }

    /// Whether the bag satisfies one objective.
    fn objective_satisfied(&self, objective: &drf_content::QuestObjective) -> bool {
        objective.kind == "ItemObjective"
            && objective
                .item
                .as_deref()
                .is_some_and(|item| self.bag_count(item) >= objective.required.max(1))
    }

    /// The client's `IsKindOf` test for a killed unit against an objective's
    /// authored `MonsterType` entries. The content may name a base class while
    /// the hook reports the placed subclass.
    fn objective_matches_monster(
        &self,
        objective: &drf_content::QuestObjective,
        classname: &str,
    ) -> bool {
        if objective.kind != "KillObjective" {
            return false;
        }
        objective.monsters.is_empty()
            || objective
                .monsters
                .iter()
                .any(|monster| self.shared.class_is_kind_of(classname, monster))
    }

    /// The same inheritance-aware match for an `ActivateObjective`.
    fn objective_matches_entity(
        &self,
        objective: &drf_content::QuestObjective,
        classname: &str,
    ) -> bool {
        if objective.kind != "ActivateObjective" {
            return false;
        }
        objective.entities.is_empty()
            || objective
                .entities
                .iter()
                .any(|entity| self.shared.class_is_kind_of(classname, entity))
    }

    /// Whether an obelisk/checkpoint activation satisfies this objective.
    /// The client's travel class is the checkpoint (`TutorialCheckpoint`),
    /// while the objective names the placed entity
    /// (`TutorialCheckpointEntity`), whose `Description.Checkpoint` names the
    /// former. Both are the same activation.
    fn objective_matches_activation(
        &self,
        objective: &drf_content::QuestObjective,
        classname: &str,
    ) -> bool {
        if objective.kind != "ActivateObjective" {
            return false;
        }
        objective.entities.is_empty()
            || objective.entities.iter().any(|entity| {
                self.shared.class_is_kind_of(classname, entity)
                    || self.objective_matches_entity(objective, classname)
                    || self
                        .shared
                        .class_description_property(entity, "Checkpoint")
                        .is_some_and(|checkpoint| checkpoint.eq_ignore_ascii_case(classname))
            })
    }

    /// Every qid the served offer directory lists for `classname`, in the
    /// directory's own order. A quest already held is dropped, as the
    /// directory's own held filter does.
    fn offered_by_npc(&self, classname: &str) -> Vec<u32> {
        self.offer_groups()
            .into_iter()
            .filter(|(npc, _)| npc == classname)
            .flat_map(|(_, quests)| quests)
            .collect()
    }

    /// The session's offered qids per giver classname, from `game.pkg` and
    /// gated on `(zone, level, class, completed)`. LISTED in a content zone
    /// (where a character ACCEPTs), UNION in town. A held quest is dropped.
    fn offer_groups(&self) -> Vec<(String, Vec<u32>)> {
        let Some(corpus) = self.shared.quest_corpus() else {
            return Vec::new();
        };
        // Generated dungeon rooms place their quest givers in tile/world-entity
        // data, not in the zone's static `Entities` block, so the world-file
        // roster alone is empty there. The session's actual placed NPCs are
        // the authoritative giver set; keep the zone roster as the fallback
        // for a session that has not joined yet (tests / pre-join callers).
        let mut npcs = corpus.zone_npcs(&self.zone);
        for placed in &self.placed_entities {
            if placed.kind == drf_content::PlacedKind::Npc
                && !npcs
                    .iter()
                    .any(|npc| npc.eq_ignore_ascii_case(&placed.classname))
            {
                npcs.push(placed.classname.clone());
            }
        }
        let completed = self.quest_completed.clone();
        let direction = if self.zone == ZONE_TOWN {
            GiverDirection::Union
        } else {
            GiverDirection::Listed
        };
        corpus
            .offer_groups(
                self.selected_level,
                &npcs,
                self.class_base.as_deref(),
                direction,
                Some(&completed),
            )
            .into_iter()
            .map(|(npc, quests)| {
                let qids: Vec<u32> = quests
                    .into_iter()
                    .map(|quest| quest.qid)
                    .filter(|qid| !self.quest_instance.contains_key(qid))
                    .collect();
                (npc, qids)
            })
            .filter(|(_, qids)| !qids.is_empty())
            .collect()
    }

    /// The offer directory block for the player's QuestManager create: what
    /// draws the quest marker on each giver at join.
    fn quest_directory(&self) -> Vec<u8> {
        quest_directory_body(&self.offer_groups())
    }

    /// `(09,00)` roster re-request -> the `(09,30)`/`(09,35)` pair, the same
    /// shapes the select burst ships.
    fn roster(&mut self) {
        let sqlid = self.selected.unwrap_or(0);
        let account = self.shared.account();
        let character = {
            let Ok(store) = self.shared.store.lock() else {
                return;
            };
            store.get_char(&account, sqlid).cloned()
        };
        let (name, level) = match &character {
            Some(character) => (character.name.clone(), character.level),
            None => ("?".to_string(), 1),
        };
        self.send(0x09, 0x30, &group_connected_0930_payload(sqlid));
        let head = roster_head(self.conn_id);
        self.send(
            0x09,
            0x35,
            &user_changed_group_0935_payload(head, sqlid, &name, level),
        );
    }

    /// `(04,03)` GetList -> the character-graph list for the account.
    fn char_list(&mut self) {
        let account = self.shared.account();
        let chars: Vec<Character> = {
            let Ok(store) = self.shared.store.lock() else {
                return;
            };
            store.list_chars(&account).into_iter().cloned().collect()
        };
        let contents: Vec<Option<ClassContent>> = chars
            .iter()
            .map(|character| self.shared.content_for(&character.avatar_class))
            .collect();
        let appearances: Vec<GraphAppearance> = chars
            .iter()
            .map(|character| graph_appearance(&character.appearance))
            .collect();
        let rows: Vec<ListRow> = chars
            .iter()
            .enumerate()
            .map(|(index, character)| ListRow {
                sqlid: character.sqlid,
                name: &character.name,
                content: contents[index].as_ref(),
                appearance: Some(&appearances[index]),
            })
            .collect();
        let payload = list_payload(&rows);
        self.send(0x04, 0x03, &payload);
    }

    /// `(04,02)` Create -> store the character and reply with its graph.
    fn create(&mut self, payload: &[u8]) {
        let Some((name, class, opts)) = parse_create_request(payload) else {
            return;
        };
        let appearance = appearance_from_opts(&opts);
        let account = self.shared.account();
        let created = {
            let Ok(mut store) = self.shared.store.lock() else {
                return;
            };
            store.create_char(&account, &name, &class, appearance)
        };
        match created {
            Ok(sqlid) => {
                let content = self.shared.content_for(&class);
                let appearance = graph_appearance(&appearance);
                let body =
                    drf_proto::created_payload(sqlid, &name, content.as_ref(), Some(&appearance));
                self.send(0x04, 0x02, &body);
            }
            // The reference signals a full account with a lone 0xFF on this tag; any
            // other refusal (bad name/class, duplicate) is likewise a bare
            // refusal here.
            Err(_) => self.send(0x04, 0x02, &[0xFF]),
        }
    }

    /// `(04,05)` Select -> the ack plus the play burst (assign included).
    fn select(&mut self, payload: &[u8]) {
        let Some(sqlid_bytes) = payload.get(..4) else {
            return;
        };
        let sqlid = u32::from_le_bytes(sqlid_bytes.try_into().unwrap());
        let account = self.shared.account();
        let character = {
            let Ok(store) = self.shared.store.lock() else {
                return;
            };
            store.get_char(&account, sqlid).cloned()
        };
        let Some(character) = character else {
            self.send(0x04, 0x05, &select_ack_payload(false));
            return;
        };
        self.selected = Some(sqlid);
        let persisted_zone = character.zone.clone();
        self.zone = self.servable_zone(&persisted_zone);
        if self.zone != persisted_zone {
            self.persist_zone(&self.zone);
        }
        self.recent_zone_portal = character
            .recent_zone_portal
            .clone()
            .and_then(|(zone, spawn)| {
                let zone = Self::normalize_zone(&zone);
                if !self.shared.has_content() || self.shared.zone_exists(&zone) {
                    Some((zone, spawn))
                } else {
                    None
                }
            });
        self.maze_seeds.clear();
        self.zone_progress.clear();
        let selected_zone = self.zone.clone();
        self.maze_seed = if self.shared.zone_is_generated(&selected_zone) {
            self.maze_seed_for(&selected_zone)
        } else {
            0
        };
        self.ensure_zone_progress(&selected_zone, self.maze_seed);
        self.combat_zone
            .store(is_combat_zone(&self.zone), Ordering::SeqCst);
        // A new selection invalidates any entities a previous join spawned;
        // the next `(13,6)` mints a fresh avatar for this character.
        self.avatar_eid = None;
        self.player_eid = None;
        self.placed_entities.clear();
        self.mob_entities.clear();
        self.mob_state.clear();
        self.mob_defeat_pending.clear();
        self.drops.clear();
        self.town_portals.clear();
        self.player_pos = None;
        self.last_combat_z = None;
        self.quest_cid = None;
        self.avatar_container_cid = None;
        self.equipment_cid = None;
        self.manipulators_cid = None;
        self.skills_cid = None;
        self.skill_slots.clear();
        self.skill_catalog.clear();
        self.bag_entries.clear();
        self.cursor = None;
        self.worn.clear();
        self.town_portals.clear();
        self.town_portal_return = None;
        self.dlg = None;
        self.zone_spawn_request = None;
        self.zone_spawn_pos = None;
        self.joined.store(false, Ordering::SeqCst);
        self.send(0x04, 0x05, &select_ack_payload(true));
        self.send(0x03, 0x01, &rosters_0301_payload());
        self.send(0x09, 0x30, &group_connected_0930_payload(sqlid));
        let head = roster_head(self.conn_id);
        self.send(
            0x09,
            0x35,
            &user_changed_group_0935_payload(head, sqlid, &character.name, character.level),
        );
        if let Ok(assign) = assign_payload_with_seed(&self.zone, self.maze_seed) {
            self.send(0x0D, 0x00, &assign);
        }
    }

    /// `(04,04)` Delete -> store removal and the ack. The client sends
    /// `[u8 0x04][name ASCIIZ][u32 sqlid]`, so the sqlid is the trailing word.
    fn delete(&mut self, payload: &[u8]) {
        if payload.len() < 5 {
            return;
        }
        let sqlid = u32::from_le_bytes(payload[payload.len() - 4..].try_into().unwrap());
        let account = self.shared.account();
        let ok = {
            let Ok(mut store) = self.shared.store.lock() else {
                return;
            };
            store.delete_char(&account, sqlid).is_ok()
        };
        self.send(0x04, 0x04, &delete_ack_payload(sqlid, ok));
    }

    /// `(13,6)` Join -> spawn the session's entities and ship the town flood.
    fn join(&mut self) {
        let Some(sqlid) = self.selected else {
            return;
        };
        let account = self.shared.account();
        let character = {
            let Ok(store) = self.shared.store.lock() else {
                return;
            };
            store.get_char(&account, sqlid).cloned()
        };
        let Some(character) = character else {
            return;
        };
        let joined = self.zone.clone();
        let servable = self.servable_zone(&joined);
        if servable != joined {
            self.zone = servable;
            self.persist_zone(&self.zone);
        }
        self.restore_character_state(&character);
        let recent = self.recent_zone_portal.clone();
        self.recent_zone_portal = recent.and_then(|(zone, spawn)| {
            let zone = Self::normalize_zone(&zone);
            if !self.shared.has_content() || self.shared.zone_exists(&zone) {
                Some((zone, spawn))
            } else {
                None
            }
        });
        let name = character.name.clone();
        let avatar_class = character.avatar_class.clone();
        let level = character.level.clamp(1, 255) as u8;
        self.selected_level = level as u16;
        self.avatar_class = Some(avatar_class.clone());
        self.class_base = class_base_of(&avatar_class);
        let joined_zone = self.zone.clone();
        self.ensure_zone_progress(&joined_zone, self.maze_seed);
        // The joined word tracks the character's persisted level; the client
        // refills and recomputes to the same level-scaled max on level-up, so
        // every synch trailer must carry it (`EntitySynchInfo::Validate`).
        let hp_wire = drf_proto::avatar_hp_wire_at_level(&avatar_class, u32::from(level));
        self.avatar_hp_wire.store(hp_wire, Ordering::Relaxed);
        if drf_proto::avatar_level_one_hp_wire(&avatar_class).is_none() {
            eprintln!(
                "game {}: no observed level-1 avatar HP for {avatar_class}; \
                 using the legacy wire word {hp_wire}",
                self.conn_id
            );
        }

        // The zone's placed world entities, derived from `game.pkg`. The set is
        // the reference `ZonePlacement.created()`: the checkpoint, then the
        // portals, then the network NPCs. Derived before the entities below so
        // the entity-id order matches the create order (placed, player,
        // avatar) and the ids are minted once per selection, so a repeated
        // join reuses the ids the client still holds.
        let derived = if self.placed_entities.is_empty() {
            self.shared
                .zone_entities_with_seed(&self.zone, self.maze_seed)
        } else {
            Vec::new()
        };
        // The generated zone's mob spawns, derived once per selection like the
        // placed entities. Empty for a non-maze zone (every spawn is refused).
        let mob_spawns = if self.placed_entities.is_empty() && self.zone != ZONE_TOWN {
            self.shared
                .mob_spawns_with_seed(&self.zone, self.maze_seed, self.selected_level)
        } else {
            Vec::new()
        };
        // Each spawn's own weapon/skill manipulators, keyed by classname, so a
        // ranged brain has a bow to fire and a caster brain a bolt to cast.
        // Resolved in ONE archive pass; only needed when the mobs are about to
        // be minted (the `mob_entities.is_empty()` branch below).
        let mob_manipulators = if self.mob_entities.is_empty() {
            let classes: Vec<String> = mob_spawns
                .iter()
                .map(|spawn| spawn.classname.clone())
                .collect();
            self.shared.mob_manipulators_for(&classes)
        } else {
            HashMap::new()
        };
        // The one manipulator every NPC's component run needs, derived once.
        let manipulator = if derived.is_empty() {
            None
        } else {
            self.shared.npc_base_manipulator()
        };
        // Every placed vendor's derived `*.Merchant` tabs, keyed by class, so
        // the loop below ships the component beside the NPC's other three.
        let npc_classes: Vec<String> = derived
            .iter()
            .filter(|entity| entity.kind == drf_content::PlacedKind::Npc)
            .map(|entity| entity.classname.clone())
            .collect();
        let merchant_plan = self.shared.merchant_tabs_for(&npc_classes);
        // Trainer components carry only a reference; the client reads the
        // authored `SkillTrainerDesc` itself. The server keeps the same rows
        // to resolve a purchase hash and price it.
        let trainer_plan = self.shared.trainer_catalogs_for(&npc_classes);

        // A repeated `(13,6)` on the same session reuses the entities the
        // first join spawned, as the reference's established-avatar branch does.
        let (
            avatar_eid,
            player_eid,
            quest_cid,
            dialog_cid,
            skills_cid,
            equipment_cid,
            container_cid,
            manip_cid,
            modifiers_cid,
            ub_cid,
        ) = {
            let Ok(mut world) = self.shared.world.lock() else {
                return;
            };
            let consumed_places = self
                .zone_progress
                .get(&self.zone)
                .map(|progress| progress.consumed_places.clone())
                .unwrap_or_default();
            for (placement_index, entity) in derived.iter().enumerate() {
                if consumed_places.contains(&placement_index) {
                    continue;
                }
                if entity.classname.to_ascii_lowercase().contains("chest") {
                    eprintln!(
                        "game {}: join chest {} kind={:?} pose={:?}",
                        self.conn_id, entity.classname, entity.kind, entity.pose
                    );
                }
                let Ok(init) = placed_entity_class_state(entity) else {
                    continue;
                };
                let entity_id = world.alloc_entity();
                let mut components = Vec::new();
                // NPCs and Unit-rooted interactives (chests, shrines, NCIs,
                // teleports) are both `Unit`s. The client's activation-range
                // check calls `Unit::getBehavior` and dereferences it without
                // a null check (0x52295A); a Unit with no Behavior component
                // crashes on the first click. Ship the same Behavior run the
                // NPC path uses for both.
                if matches!(
                    entity.kind,
                    drf_content::PlacedKind::Npc | drf_content::PlacedKind::WorldEntityUnit
                ) {
                    if let Some(manipulator) = manipulator.as_ref() {
                        if let Ok(behavior) = npc_behavior_class_state(entity.pose) {
                            let manip_cid = world.alloc_component();
                            let behavior_cid = world.alloc_component();
                            let modifiers_cid = world.alloc_component();
                            let records = [
                                (
                                    manip_cid,
                                    NPC_MANIPULATORS_CLASS,
                                    npc_manipulators_class_state(&manipulator.fqn, manipulator.id),
                                ),
                                (behavior_cid, NPC_BEHAVIOR_CLASS, behavior),
                                (
                                    modifiers_cid,
                                    NPC_MODIFIERS_CLASS,
                                    npc_modifiers_class_state(),
                                ),
                            ];
                            for (cid, class, state) in records {
                                if let Ok(record) =
                                    npc_component_record(entity_id, cid, class, &state)
                                {
                                    components.push(record);
                                }
                            }
                        }
                    }
                    if let Some(tabs) = merchant_plan.get(&entity.classname) {
                        let item_count: u32 = tabs.iter().map(|tab| tab.items.len() as u32).sum();
                        let merchant_cid = world.alloc_component();
                        let first_item_id =
                            reserve_item_ids(&mut self.merchant_item_cursor, item_count);
                        components.push(build_merchant_component_record(
                            entity_id,
                            merchant_cid,
                            &entity.classname,
                            tabs,
                            first_item_id,
                        ));
                        let (ids, _next_id) = derived_item_ids(tabs, first_item_id);
                        let mut items = HashMap::new();
                        for tab in tabs {
                            for item in &tab.items {
                                if let Some(id) = ids.get(&(tab.index, item.index)) {
                                    items.insert(
                                        *id,
                                        MerchantStockItem {
                                            class: item.class.clone(),
                                            level: item.level,
                                            quantity: item.quantity,
                                        },
                                    );
                                }
                            }
                        }
                        self.merchant_components.insert(
                            merchant_cid,
                            MerchantRegistration {
                                npc_class: entity.classname.clone(),
                                items,
                            },
                        );
                    }
                    if let Some(catalog) = trainer_plan.get(&entity.classname) {
                        let trainer_cid = world.alloc_component();
                        if let Ok(record) = npc_component_record(
                            entity_id,
                            trainer_cid,
                            &catalog.component_class,
                            &[0x00],
                        ) {
                            components.push(record);
                            self.trainer_components.insert(
                                trainer_cid,
                                TrainerRegistration::from_catalog(&entity.classname, catalog),
                            );
                        }
                    }
                }
                self.placed_entities.push(PlacedCreate {
                    entity_id,
                    placement_index,
                    classname: entity.classname.clone(),
                    kind: entity.kind,
                    wire: Some((i64::from(entity.pose[0]), i64::from(entity.pose[1]))),
                    zone: entity.zone.clone(),
                    spawn_point: entity.spawn_point.clone(),
                    track_activating_players: entity.track_activating_players,
                    init,
                    components,
                });
            }
            // The generated mobs follow the placed entities, drawing entity and
            // component ids from the same allocators. A spawn whose HP could not
            // be derived is skipped: a missing mob is fine, a desynced stream is
            // a crash (its `0x35` trailers must equal the client's derived HP).
            if self.mob_entities.is_empty() {
                let scale = difficulty_scale(self.difficulty);
                let defeated_mobs = self
                    .zone_progress
                    .get(&self.zone)
                    .map(|progress| progress.defeated_mobs.clone())
                    .unwrap_or_default();
                for (spawn_index, spawn) in mob_spawns.iter().enumerate() {
                    if spawn.hp_wire == 0 || defeated_mobs.contains(&spawn_index) {
                        continue;
                    }
                    let entity_id = world.alloc_entity();
                    let behavior_id = world.alloc_component();
                    let skills_id = world.alloc_component();
                    let manipulators_id = world.alloc_component();
                    let modifiers_id = world.alloc_component();
                    let pose = [
                        spawn.pose[0] as u32,
                        spawn.pose[1] as u32,
                        spawn.pose[2] as u32,
                        spawn.pose[3] as u32,
                    ];
                    let hp_wire = scaled_hp_wire(spawn.hp_wire, scale);
                    let current_hp = (!scale.is_normal()).then_some(hp_wire);
                    let Ok(init) = mob_class_state_scaled(pose, spawn.level, current_hp, scale)
                    else {
                        continue;
                    };
                    let manipulators = mob_manipulators
                        .get(&spawn.classname)
                        .cloned()
                        .unwrap_or_else(|| vec![CreatureManipulator::default_melee()]);
                    // The creature's own brain, sanitized to a concrete
                    // `creatures.base.behavior.*` matched to its primary
                    // weapon: a Ranged weapon needs the Ranged brain to fire
                    // it and a skill-only Caster needs the Caster brain; the
                    // Melee brain only runs in and swings.
                    let behavior_class =
                        drf_content::mob_behavior_class(spawn.behavior.as_deref(), &manipulators);
                    let mut components = Vec::new();
                    let records = [
                        (
                            behavior_id,
                            behavior_class.as_str(),
                            mob_behavior_class_state(),
                        ),
                        (skills_id, MOB_SKILLS_CLASS, mob_skills_class_state()),
                        (
                            manipulators_id,
                            MOB_MANIPULATORS_CLASS,
                            mob_manipulators_class_state(&manipulators),
                        ),
                        (
                            modifiers_id,
                            MOB_MODIFIERS_CLASS,
                            mob_modifiers_class_state(),
                        ),
                    ];
                    for (cid, class, state) in records {
                        if let Ok(record) = mob_component_record(entity_id, cid, class, &state) {
                            components.push(record);
                        }
                    }
                    self.mob_state.insert(
                        entity_id,
                        TrackedMob {
                            spawn_index,
                            hp_wire,
                            hp: hp_wire / 256,
                            classname: spawn.classname.clone(),
                            level: spawn.level,
                            pose: spawn.pose,
                        },
                    );
                    self.mob_entities.push(MobCreate {
                        entity_id,
                        behavior_id,
                        classname: spawn.classname.clone(),
                        init,
                        components,
                        spawn_action: mob_spawn_action_payload(
                            behavior_id,
                            spawn.pose,
                            entity_id,
                            hp_wire,
                        ),
                        mover_update: mob_mover_update_payload(behavior_id, spawn.pose, hp_wire),
                    });
                }
            }
            let avatar = match self.avatar_eid {
                Some(eid) => eid,
                None => world.spawn_avatar(
                    self.conn_id,
                    sqlid,
                    name.as_bytes(),
                    Some(avatar_class.as_bytes()),
                ),
            };
            let player = match self.player_eid {
                Some(eid) => eid,
                None => world.spawn_player(self.conn_id, name.as_bytes()),
            };
            // The player's nested QuestManager/DialogManager ids and the
            // avatar's six component ids. A re-join allocates fresh ids; the
            // allocator is monotonic so a component id from a previous join is
            // never re-issued.
            let quest_cid = world.alloc_component();
            let dialog_cid = world.alloc_component();
            let skills_cid = world.alloc_component();
            let equipment_cid = world.alloc_component();
            let container_cid = world.alloc_component();
            let manip_cid = world.alloc_component();
            let modifiers_cid = world.alloc_component();
            let ub_cid = world.alloc_component();
            (
                avatar,
                player,
                quest_cid,
                dialog_cid,
                skills_cid,
                equipment_cid,
                container_cid,
                manip_cid,
                modifiers_cid,
                ub_cid,
            )
        };
        self.avatar_eid = Some(avatar_eid);
        self.player_eid = Some(player_eid);
        self.quest_cid = Some(quest_cid);
        self.avatar_container_cid = Some(container_cid);
        self.equipment_cid = Some(equipment_cid);
        self.manipulators_cid = Some(manip_cid);
        self.skills_cid = Some(skills_cid);
        let content = self.shared.content_for(&avatar_class);
        let items = content.as_ref().map(|content| content.items.as_slice());
        let skill_wire = content.as_ref().map(|content| &content.skill_wire);
        let persisted_worn = character.equipment.is_some();
        self.bag_grid = self.shared.bag_geometry().map(|(w, h)| BagGrid::new(w, h));
        self.rebuild_worn(character.equipment.as_deref(), items);
        if let Some(grid) = self.bag_grid.as_mut() {
            for row in &character.bag {
                if let Ok(Some(record)) = self.shared.author_item_record(
                    &row.class,
                    0,
                    row.count,
                    row.level,
                    (row.cell_x, row.cell_y),
                ) {
                    grid.occupy(row.cell_x, row.cell_y, record.width, record.height);
                }
            }
        }
        if let Ok(mut ub) = self.avatar_ub_cid.lock() {
            *ub = Some(ub_cid);
        }

        let mut creates: Vec<(u8, Vec<u8>)> = Vec::new();
        // Every zone ships the placed entities derived from `game.pkg`, each as
        // a bare `0x01` create followed by its `0x02` class-state (pose + level)
        // and, for NPCs, the nested components. Town is no longer special: its
        // checkpoint, portals, vendors and waypoints all come from `town.world`.
        for placed in &self.placed_entities {
            if let Ok(body) = create_body(placed.entity_id, &placed.classname) {
                creates.push((0x01, body));
            }
            let mut init = Vec::with_capacity(2 + placed.init.len());
            init.extend_from_slice(&placed.entity_id.to_le_bytes());
            init.extend_from_slice(&placed.init);
            creates.push((0x02, init));
            // An NPC's three nested components follow its `0x02`, exactly
            // like the avatar's six; `join_batch` writes the tag itself.
            for record in &placed.components {
                if record.first() == Some(&0x32) {
                    creates.push((0x32, record[1..].to_vec()));
                }
            }
        }
        // The generated mobs: create, class-state init, the four nested
        // components, then the two `0x35` trailers (SpawnAction + mover).
        // NO `0x64 FollowClient` enroll: the client's own `MonsterBehavior2`
        // brain proximity-aggros, chases and attacks with no server help,
        // and the `0x64` hand-off is what parks the mob in client-control
        // mode with no owner streaming movers (see `drf_proto::mob`).
        for mob in &self.mob_entities {
            if let Ok(body) = create_body(mob.entity_id, &mob.classname) {
                creates.push((0x01, body));
            }
            let mut init = Vec::with_capacity(2 + mob.init.len());
            init.extend_from_slice(&mob.entity_id.to_le_bytes());
            init.extend_from_slice(&mob.init);
            creates.push((0x02, init));
            for record in &mob.components {
                if record.first() == Some(&0x32) {
                    creates.push((0x32, record[1..].to_vec()));
                }
            }
            creates.push((0x35, mob.spawn_action.clone()));
            creates.push((0x35, mob.mover_update.clone()));
        }
        // The player is a `0x08` createinit whose body is `[eid][0xFF]
        // ["player" NUL][name NUL][27B tail]` with the nested managers. The
        // client's `Player::readInit` reads that shape; a character-graph node
        // here desyncs the record loop.
        let directory = self.quest_directory();
        let journal = held_journal(&character);
        // Non-faithful "everywhere I've been": remember this zone and offer a
        // real obelisk Checkpoint row for every zone the character has
        // entered, on top of the shipped starting checkpoints.
        self.persist_visited_zone(&self.zone);
        let mut visited_zones = character.visited_zones.clone();
        if !self.zone.trim().is_empty()
            && !visited_zones
                .iter()
                .any(|zone| zone.eq_ignore_ascii_case(&self.zone))
        {
            visited_zones.push(self.zone.clone());
        }
        let mut checkpoints = self.shared.starting_checkpoints();
        for checkpoint in self.shared.checkpoints_for_zones(&visited_zones) {
            if !checkpoints
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&checkpoint))
            {
                checkpoints.push(checkpoint);
            }
        }
        let checkpoint_refs: Vec<&str> = checkpoints.iter().map(String::as_str).collect();
        let recent_portal = self
            .recent_zone_portal
            .as_ref()
            .map(|(zone, spawn)| (zone.as_str(), spawn.as_str()));
        let managers =
            derived_player_manager_components_with_journal_checkpoints_and_recent_portal(
                player_eid,
                quest_cid,
                dialog_cid,
                &directory,
                self.next_instance,
                &journal,
                QuestManagerTail {
                    checkpoints: &checkpoint_refs,
                    recent_portal,
                },
            )
            .unwrap_or_default();
        // `--admin` sets the player permission byte the client's
        // `Avatar::IsAdmin` reads, arming `/admin` and `/adminui`.
        let player_role = if self.shared.admin_commands() {
            PLAYER_ROLE_ADMIN
        } else {
            PLAYER_ROLE_PLAYER
        };
        let player_body = join_player_init_body_with_role(
            player_eid,
            name.as_bytes(),
            sqlid as u16,
            PLAYER_JOIN_V1,
            player_role,
            &managers,
        )
        .unwrap_or_default();
        creates.push((0x08, player_body));

        // The avatar is a PLACED entity: a bare `0x01` create (so the client
        // `WorldObject::attach`s it), then its `0x02` class-state head, then
        // the six top-level `0x32` components the in-world HUD dereferences
        // without a NULL guard. The head chains `WorldEntity::readInit`'s
        // scene-attach flag and `Unit::readInit`'s owner link (the player's
        // eid), which is what lets `Player::getAvatar` find the avatar.
        if let Ok(body) = create_body(avatar_eid, &avatar_class) {
            creates.push((0x01, body));
        }
        let mut worn_slots: Vec<&WornEntry> = self.worn.values().collect();
        worn_slots.sort_by_key(|entry| entry.slot);
        let worn_equipment_entries: Vec<Vec<u8>> = worn_slots
            .iter()
            .map(|entry| entry.record.clone())
            .collect();
        let worn_manipulator_entries: Vec<Vec<u8>> = worn_slots
            .iter()
            .map(|entry| entry.manip_record.clone())
            .collect();
        let roster = self.shared.avatar_inventory_roster();
        // One-shot: the portal's SpawnPoint is consumed by this join, so a
        // later respawn inside the destination falls back to the zone's own
        // default rung instead of being teleported back to a portal mouth.
        let requested = self.zone_spawn_request.take();
        // A hand-placed zone's own waypoint wins. Only when no waypoint any
        // rung names resolves (a GENERATED world) does the zone-start room's
        // grid cell stand in, derived from the room's `GridY` and the world
        // archetype's `TileSize` -- never a constant.
        let hand_placed = self
            .shared
            .zone_spawn_pose_requested(&self.zone, requested.as_deref());
        // A generated world places no coordinate the server can state, so the
        // create carries the `0x15` warp gate and a `0x03 [eid][0x15]` update
        // follows: the client places the avatar at its own maze-computed
        // `ZoneStart` waypoint. A hand-placed zone ships neither.
        let generated = if hand_placed.is_none() {
            self.shared.zone_generated_start_with_seed(
                &self.zone,
                requested.as_deref(),
                self.maze_seed,
            )
        } else {
            None
        };
        let pose = self
            .zone_spawn_pos
            .take()
            // A degenerate stored position (the origin) is a missing record,
            // not a real arrival: fall back to the zone's own waypoint rather
            // than dropping the avatar at 0,0 outside the map.
            .filter(|position| position[0] != 0 || position[1] != 0)
            .map(|[x, y]| [x as u32, y as u32, 0, 0])
            .or_else(|| hand_placed.or_else(|| generated.as_ref().map(|start| start.pose)));
        if let Some(position) = pose {
            let position = [position[0] as i32, position[1] as i32];
            self.player_pos = Some(position);
            self.mark_explored_position(position);
        }
        let we_flags = MINIMAL_AVATAR_WE_FLAGS_ATTACH;
        // Re-ship the persisted XP and bonus points so the client's re-created
        // avatar keeps its locally-earned progress (`Hero +0x320`/`+0x33c`).
        let attribute_words = self
            .attributes
            .map(|value| u16::try_from(value).unwrap_or(u16::MAX));
        if let Ok(head) = build_minimal_avatar_init_payload_with_stats(
            avatar_eid,
            player_eid,
            avatar_class.as_bytes(),
            pose,
            Some(level),
            self.experience,
            u16::try_from(self.attribute_points).unwrap_or(u16::MAX),
            attribute_words,
            &[],
            we_flags,
            Some(hp_wire),
            Some(MINIMAL_AVATAR_MP),
        ) {
            let mut init = Vec::with_capacity(2 + head.len());
            init.extend_from_slice(&avatar_eid.to_le_bytes());
            init.extend_from_slice(&head);
            creates.push((0x02, init));
            // The class's starting skills ARE derived; the points/quota word
            // has no admissible source, so it rides 0 (the reference's own
            // `minimal_avatar_skills_component` default). An unknown class
            // (no `game.pkg`, no kit) ships the empty component unchanged.
            let mut skills: Vec<(String, u32, u8)> = skill_wire
                .map(|wire| wire.skills.clone())
                .unwrap_or_default();
            for (fqn, _, level) in &mut skills {
                if let Some(bought) = self.skill_levels.get(fqn) {
                    *level = (*bought).max(1);
                }
            }
            // Skills learned from a trainer are persisted by FQN, not by the
            // class kit. Append the ones this class did not start with so the
            // skill panel and hotbar can resolve them after a relogin.
            let mut learned_active: Vec<String> = Vec::new();
            for (fqn, level) in &self.skill_levels {
                if skills
                    .iter()
                    .any(|(known, _, _)| known.eq_ignore_ascii_case(fqn))
                {
                    continue;
                }
                skills.push((fqn.clone(), 0, (*level).max(1)));
                if !self.shared.skill_is_passive(fqn) {
                    learned_active.push(fqn.clone());
                }
            }
            // The reference server gives every character the promotional
            // Bling Gnome skill (its skillbook is then a harmless consumable).
            // Keep that server-design behavior: the skill is in the panel and
            // can be put on the hotbar, which is what summons the gnome.
            if let Some(profile) = self.gnome_profile.as_ref() {
                if !skills
                    .iter()
                    .any(|(known, _, _)| known.eq_ignore_ascii_case(&profile.skill))
                {
                    skills.push((profile.skill.clone(), 0, 1));
                    learned_active.push(profile.skill.clone());
                }
            }
            let mut next_learned_id = skills
                .iter()
                .map(|(_, id, _)| *id)
                .filter(|id| *id >= 200)
                .max()
                .map_or(200, |id| id.saturating_add(1));
            for (_, id, _) in &mut skills {
                if *id == 0 {
                    *id = next_learned_id;
                    next_learned_id = next_learned_id.saturating_add(1);
                }
            }
            let professions: &[String] = skill_wire
                .map(|wire| wire.professions.as_slice())
                .unwrap_or(&[]);
            self.skill_catalog = skills.clone();
            self.skill_slots = match character.hotbar.as_deref() {
                Some(rows) => rows
                    .iter()
                    .map(|row| (row.slot, row.skill.clone()))
                    .collect(),
                None => skill_wire
                    .map(|wire| {
                        wire.manipulators
                            .iter()
                            .filter(|(_, id, _)| HOTBAR_SLOTS.contains(id))
                            .map(|(fqn, id, _)| (*id, fqn.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            let mut manipulator_skills: Vec<(String, u32, u8)> = skill_wire
                .map(|wire| wire.manipulators.clone())
                .unwrap_or_default();
            for (fqn, _, level) in &mut manipulator_skills {
                if let Some(bought) = self.skill_levels.get(fqn) {
                    *level = (*bought).max(1);
                }
            }
            for fqn in learned_active {
                if manipulator_skills
                    .iter()
                    .any(|(known, _, _)| known.eq_ignore_ascii_case(&fqn))
                {
                    continue;
                }
                let level = self.skill_levels.get(&fqn).copied().unwrap_or(1).max(1);
                manipulator_skills.push((fqn, 0, level));
            }
            apply_skill_slots(&mut manipulator_skills, &self.skill_slots);
            let components = [
                minimal_avatar_skills_component_with_skills(
                    avatar_eid,
                    skills_cid,
                    self.skill_points,
                    &skills,
                    professions,
                ),
                if persisted_worn {
                    minimal_avatar_equipment_component_with_entries(
                        avatar_eid,
                        equipment_cid,
                        &worn_equipment_entries,
                    )
                } else {
                    minimal_avatar_equipment_component(avatar_eid, equipment_cid, items)
                },
                minimal_avatar_unit_container_component_with_gold(
                    avatar_eid,
                    container_cid,
                    &roster,
                    self.gold,
                ),
                if persisted_worn {
                    minimal_avatar_manipulators_component_with_entries(
                        avatar_eid,
                        manip_cid,
                        &worn_manipulator_entries,
                        &manipulator_skills,
                    )
                } else {
                    minimal_avatar_manipulators_component(
                        avatar_eid,
                        manip_cid,
                        items,
                        &manipulator_skills,
                    )
                },
                minimal_avatar_modifiers_component(avatar_eid, modifiers_cid),
                minimal_avatar_unit_behavior_component(avatar_eid, ub_cid, false),
            ];
            for mut record in components.into_iter().flatten() {
                // The builders return the whole `0x32` record, matching the
                // reference and the joined nested managers; `join_batch` writes the
                // record's tag itself, so drop it here.
                if record.first() == Some(&0x32) {
                    record.remove(0);
                }
                creates.push((0x32, record));
            }
            // The avatar needs its SpawnAction too. `Spawn::update` fires the
            // unit's spawn event, which runs `Unit::onSpawned` and sets
            // `Unit+0xA0 |= 0x400`. Without that bit,
            // `MonsterBehavior2::UpdateTargets`' keep test rejects the player
            // as a target on the very next tick, so a mob retaliates and
            // then immediately de-aggros. The action's pose is not used to move
            // the unit (the generated-world avatar is placed by its `0x15`
            // warp), so a missing pose can safely fall back to zero.
            creates.push((
                0x35,
                mob_spawn_action_payload(
                    ub_cid,
                    pose.unwrap_or([0, 0, 0, 0]).map(|v| v as i32),
                    avatar_eid,
                    hp_wire,
                ),
            ));
        }

        let refs: Vec<(u8, &[u8])> = creates
            .iter()
            .map(|(tag, body)| (*tag, body.as_slice()))
            .collect();
        let inner = join_batch(TOWN_BUDGET_TRIPLE, &refs, JOIN_END_MARKER);

        // The join batch is a complete inner (`[07][0D]...`), so it is sent
        // verbatim rather than through `send`, which would prepend the tag.
        self.send_inner(&inner);
        let explored = self.explored_ready_words();
        self.send(
            0x0D,
            0x01,
            &drf_proto::ready_payload_with_explored(sqlid, explored.as_deref()),
        );
        self.send(0x0D, 0x05, &instance_count_payload(0, 0));
        // Re-send the open state of doors this zone already opened. The door
        // entity still has to exist, so it is rebuilt closed above and this is
        // the same `0x0A` update a click/death would have sent.
        let opened_doors = self
            .zone_progress
            .get(&self.zone)
            .map(|progress| progress.opened_doors.clone())
            .unwrap_or_default();
        if !opened_doors.is_empty() {
            let door_ids: Vec<u16> = self
                .placed_entities
                .iter()
                .filter(|placed| opened_doors.contains(&placed.placement_index))
                .map(|placed| placed.entity_id)
                .collect();
            for entity_id in door_ids {
                self.open_door(entity_id);
            }
        }
        // Re-deliver the persisted bag AFTER the join's own frames, so the
        // `UnitContainer` component this addresses has been shipped. A bag
        // change is the client's one moment to re-read a held quest's
        // objectives, so its verdict is restated behind the reship.
        self.reship_bag(&character.bag);
        self.restore_held_on_accept_items();
        // The journal's AddQuest ran before the persisted bag was on the wire
        // and before any event objective was replayed. Restate every held
        // quest from the server's authorities; this also sets the client's
        // cached completion bit after a re-login.
        self.restate_held_objectives();
        // The `(07,0C)` seed leg is WITHHELD: its real body is a zone
        // descriptor table, not the bare `u32` the reference deprecated builder
        // emits, and that table is capture-derived. A 4-byte stub on the CEM
        // channel is read as a record and latches the entity manager
        // (`Unexpected message size: 0`). The verified join sends no seed.
        for (cid, registration) in &self.merchant_components {
            eprintln!(
                "game: {} *.Merchant cid={:#06x} item_ids={}",
                registration.npc_class,
                cid,
                registration.items.len()
            );
        }
        for (cid, registration) in &self.trainer_components {
            eprintln!(
                "game: {} *.SkillTrainer cid={:#06x} rows={}",
                registration.npc_class,
                cid,
                registration.skills.len()
            );
        }
        // The join's fresh QuestManager init clears the saved place, so a
        // scroll-cast character arriving in town gets it re-stated here -- the
        // obelisk's "Saved Places" entry must survive the zone transfer.
        if self.zone == ZONE_TOWN {
            if let Some((zone, _)) = self.town_portal_return.clone() {
                self.send_saved_place(&zone);
            }
        }
        self.spawn_return_portal_if_home();
        self.credit_arrival_quests();
        self.joined.store(true, Ordering::SeqCst);
    }

    /// After a zone join, if this is the zone the last waypoint scroll was cast
    /// in, spawn the visual-only return portal at the cast point and clear the
    /// saved state (one return trip per scroll).
    fn spawn_return_portal_if_home(&mut self) {
        let Some((zone, [x, y])) = self.town_portal_return.clone() else {
            return;
        };
        if zone != self.zone {
            return;
        }
        let Some(avatar_eid) = self.avatar_eid else {
            return;
        };
        let entity_id = {
            let Ok(mut world) = self.shared.world.lock() else {
                return;
            };
            world.alloc_entity()
        };
        let Ok(packet) = build_town_portal_create_init(
            entity_id,
            avatar_eid,
            [x, y, 0],
            ZONE_TOWN,
            0,
            0x04,
            0x01,
        ) else {
            return;
        };
        self.send(0x07, 0x08, &packet);
        self.town_portal_return = None;
        eprintln!(
            "game {}: return portal eid={entity_id:#06x} at ({x},{y})",
            self.conn_id
        );
    }

    /// Frame `payload` as `[channel][mtype][payload]` and send it.
    fn send(&self, channel: u8, mtype: u8, payload: &[u8]) {
        self.diag_avatar_hp(channel, mtype, payload);
        let mut inner = Vec::with_capacity(2 + payload.len());
        inner.push(channel);
        inner.push(mtype);
        inner.extend_from_slice(payload);
        self.send_inner(&inner);
    }

    /// `DRF_HP_DIAG`-gated trace of every `(07,35)` that carries the avatar's
    /// HP trailer, so the next live run pins which server send trips
    /// `EntitySynchInfo::Validate` after a mob damages the avatar. The trailer
    /// is `02 <hp:u32>`; the match is on the session's own wire word, so an
    /// unrelated `0x02` byte cannot false-positive. No-op unless the env var is
    /// set.
    fn diag_avatar_hp(&self, channel: u8, mtype: u8, payload: &[u8]) {
        if !hp_diag_enabled() {
            return;
        }
        if channel != 0x07 || mtype != 0x35 {
            return;
        }
        let hp = self.avatar_hp_word().to_le_bytes();
        if payload
            .windows(5)
            .any(|window| window[0] == 0x02 && window[1..5] == hp)
        {
            eprintln!(
                "game {}: AVATAR-HP-DIAG (07,35) len={} hex={}",
                self.conn_id,
                payload.len(),
                payload
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
        }
    }

    /// Wrap an already-assembled inner in a zlib segment and send it.
    fn send_inner(&self, inner: &[u8]) {
        let peer = self.peer.lock().map(|peer| *peer).unwrap_or([0u8; 3]);
        let segment = Segment::new(peer, Some(GAME_ADDR), Vec::new(), Payload::zlib(inner));
        let Ok(frame) = segment.encode() else {
            return;
        };
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(&frame);
        }
    }
}

/// `DRF_HP_DIAG`-gated trace of the ticker's spontaneous move-ack decision, so
/// a live run can confirm the combat-zone suppression is in effect and see
/// when an interval would have carried the avatar HP. No-op unless the env var
/// is set.
fn hp_diag(frame: &str, detail: &str) {
    if !hp_diag_enabled() {
        return;
    }
    eprintln!("AVATAR-HP-DIAG frame={frame} {detail}");
}

/// Parse one UDP line from the client hook. Strict and side-effect free.
///
/// `DRF1 death <eid:u16> <level:u8> <x:i32> <y:i32> <z:i32> <heading:i32> <tick:u32>`
/// `DRF1 hp    <eid:u16> <hp_wire:u32> <tick:u32>`
///
/// Unknown versions/types, bad widths and truncated lines return `None`.
fn parse_hook_record(line: &[u8]) -> Option<HookEvent> {
    let text = std::str::from_utf8(line).ok()?.trim();
    let mut fields = text.split_whitespace();
    if fields.next()? != "DRF1" {
        return None;
    }
    match fields.next()? {
        "death" => {
            let eid: u16 = fields.next()?.parse().ok()?;
            let level: u8 = fields.next()?.parse().ok()?;
            let x: i32 = fields.next()?.parse().ok()?;
            let y: i32 = fields.next()?.parse().ok()?;
            let z: i32 = fields.next()?.parse().ok()?;
            let heading: i32 = fields.next()?.parse().ok()?;
            fields.next()?.parse::<u32>().ok()?; // client tick (diagnostic)
            if eid == 0 {
                return None;
            }
            Some(HookEvent::Death(HookMobDeath {
                eid,
                level,
                pose: [x, y, z, heading],
            }))
        }
        "hp" => {
            let eid: u16 = fields.next()?.parse().ok()?;
            let hp_wire: u32 = fields.next()?.parse().ok()?;
            fields.next()?.parse::<u32>().ok()?; // client tick (diagnostic)
            if eid == 0 {
                return None;
            }
            Some(HookEvent::Hp { eid, hp_wire })
        }
        _ => None,
    }
}

/// Start the process-wide hook UDP listener once. Only a session created with
/// `--trust-client-sim` calls this.
fn ensure_hook_listener() {
    HOOK_LISTENER.call_once(|| match UdpSocket::bind(("0.0.0.0", HOOK_TELEMETRY_PORT)) {
        Ok(socket) => {
            eprintln!("hook telemetry: listening on udp :{HOOK_TELEMETRY_PORT}");
            thread::spawn(move || hook_listener_loop(socket));
        }
        Err(error) => eprintln!("hook telemetry: cannot bind udp :{HOOK_TELEMETRY_PORT}: {error}"),
    });
}

fn hook_listener_loop(socket: UdpSocket) {
    let mut buffer = [0u8; 512];
    loop {
        match socket.recv_from(&mut buffer) {
            Ok((read, _peer)) => {
                let Some(event) = parse_hook_record(&buffer[..read]) else {
                    continue;
                };
                let Ok(mut sessions) = HOOK_SESSIONS.lock() else {
                    continue;
                };
                sessions.retain(|(_, sender)| sender.send(event).is_ok());
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// The session's 30 Hz clock: while joined, one `(07,0D)` WorldInterval every
/// [`WORLD_INTERVAL_EVERY`] ticks. The interval carries the local move-ack (an
/// echo of the client's own mover records) and the budget triple/PathManager
/// watchdog the client's world clock needs.
///
/// `trust_client_sim` is the `--trust-client-sim` gate (see
/// [`owner_move_ack_wire`]): when the client-sim hook is installed, the
/// combat-zone suppression is lifted and the owner's verbatim echo rides the
/// interval even in a dungeon.
#[allow(clippy::too_many_arguments)]
fn spawn_ticker(
    writer: Arc<Mutex<TcpStream>>,
    peer: Arc<Mutex<[u8; 3]>>,
    joined: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    pending_move: Arc<Mutex<MoveAck>>,
    pending_interval: Arc<Mutex<Vec<Vec<u8>>>>,
    avatar_ub_cid: Arc<Mutex<Option<u16>>>,
    avatar_hp_wire: Arc<AtomicU32>,
    combat_zone: Arc<AtomicBool>,
    trust_client_sim: bool,
) {
    thread::spawn(move || {
        let mut tick: u32 = 0;
        let mut count: u32 = 0;
        while !stop.load(Ordering::SeqCst) {
            thread::sleep(TICK_INTERVAL);
            if !joined.load(Ordering::SeqCst) {
                continue;
            }
            count = count.wrapping_add(1);
            if !count.is_multiple_of(WORLD_INTERVAL_EVERY) {
                continue;
            }
            tick = tick.wrapping_add(1);
            let ub = avatar_ub_cid.lock().ok().and_then(|guard| *guard);
            let combat = combat_zone.load(Ordering::Relaxed);
            let ack = ub
                .map(|cid| {
                    let staged = pending_move.lock().ok().and_then(|mut guard| guard.take());
                    match staged {
                        Some((session, count, data)) => {
                            let ack = owner_move_ack_wire(
                                combat,
                                trust_client_sim,
                                cid,
                                session,
                                count,
                                &data,
                                avatar_hp_wire.load(Ordering::Relaxed),
                            );
                            hp_diag(
                                "move-ack",
                                if ack.is_empty() { "suppressed" } else { "sent" },
                            );
                            ack
                        }
                        None => Vec::new(),
                    }
                })
                .unwrap_or_default();
            // Keep the queue lock through the write. The zone-transfer path
            // holds the same lock while it clears old-zone bodies and sends
            // the 0x0D transfer; this orders the interval before the transfer,
            // never after it with stale component ids.
            let mut interval_gate = pending_interval.lock();
            let interval = interval_gate
                .as_mut()
                .map(|queue| std::mem::take(&mut **queue).concat())
                .unwrap_or_default();
            let payload = world_interval_payload(tick, &ack, &interval);
            let mut inner = Vec::with_capacity(2 + payload.len());
            inner.push(0x07);
            inner.push(0x0D);
            inner.extend_from_slice(&payload);
            let peer = peer.lock().map(|peer| *peer).unwrap_or([0u8; 3]);
            let segment = Segment::new(peer, Some(GAME_ADDR), Vec::new(), Payload::zlib(&inner));
            let Ok(frame) = segment.encode() else {
                continue;
            };
            let Ok(mut writer) = writer.lock() else {
                continue;
            };
            if writer.write_all(&frame).is_err() {
                break;
            }
            drop(interval_gate);
        }
    });
}

/// Do two bag rectangles overlap? Cells are top-left, sizes are 1-based.
fn rectangles_overlap(a: (u8, u8, u32, u32), b: (u8, u8, u32, u32)) -> bool {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;
    let ax = u32::from(ax);
    let ay = u32::from(ay);
    let bx = u32::from(bx);
    let by = u32::from(by);
    ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
}

/// Is the rectangle inside the bag?
fn rectangle_in_bounds(x: u8, y: u8, width: u32, height: u32, bag_w: u8, bag_h: u8) -> bool {
    width > 0
        && height > 0
        && u32::from(x) + width <= u32::from(bag_w)
        && u32::from(y) + height <= u32::from(bag_h)
}

/// Parse one c2s `07 34 <cid u16> <sub> <body>` UnitContainer request.
///
/// Exact lengths only: `0x21`/`0x22`/`0x2A` are empty (3B), the u32-id legs
/// are 7B, put-active is 6B, use-pos is 19B and use-target is 9B. Anything
/// else (including a zero cid) is not this family.
fn parse_unit_container_request(payload: &[u8]) -> Option<(u16, UnitContainerRequest)> {
    if payload.len() < 3 {
        return None;
    }
    let cid = u16::from_le_bytes(payload[0..2].try_into().ok()?);
    if cid == 0 {
        return None;
    }
    let sub = payload[2];
    let request = match sub {
        0x21 | 0x22 if payload.len() == 3 => UnitContainerRequest::Empty(sub),
        0x2A if payload.len() == 3 => UnitContainerRequest::Currency,
        0x23 | 0x24 if payload.len() == 7 => {
            let item = u32::from_le_bytes(payload[3..7].try_into().ok()?);
            if item == 0 {
                return None;
            }
            UnitContainerRequest::Drop(item)
        }
        0x25 if payload.len() == 7 => {
            let item = u32::from_le_bytes(payload[3..7].try_into().ok()?);
            if item == 0 {
                return None;
            }
            UnitContainerRequest::Use(item)
        }
        0x26 if payload.len() == 19 => UnitContainerRequest::UsePos {
            item: u32::from_le_bytes(payload[3..7].try_into().ok()?),
            x: i32::from_le_bytes(payload[7..11].try_into().ok()?),
            y: i32::from_le_bytes(payload[11..15].try_into().ok()?),
            z: i32::from_le_bytes(payload[15..19].try_into().ok()?),
        },
        0x27 if payload.len() == 9 => UnitContainerRequest::UseTarget {
            item: u32::from_le_bytes(payload[3..7].try_into().ok()?),
            target: u16::from_le_bytes(payload[7..9].try_into().ok()?),
        },
        0x28 if payload.len() == 7 => {
            let item = u32::from_le_bytes(payload[3..7].try_into().ok()?);
            if item == 0 {
                return None;
            }
            UnitContainerRequest::Get(item)
        }
        0x29 if payload.len() == 6 => UnitContainerRequest::Put {
            inventory_id: payload[3],
            x: payload[4],
            y: payload[5],
        },
        _ => return None,
    };
    Some((cid, request))
}

/// Parse one c2s `07 34 <cid u16> <sub> <body>` Equipment request.
///
/// `0x28` is `u32 slot + u32 bag item id` (11B); `0x29` is `u32 slot` (7B).
fn parse_equipment_request(payload: &[u8]) -> Option<(u16, EquipmentRequest)> {
    if payload.len() < 3 {
        return None;
    }
    let cid = u16::from_le_bytes(payload[0..2].try_into().ok()?);
    if cid == 0 {
        return None;
    }
    let request = match payload[2] {
        0x28 if payload.len() == 11 => {
            let slot = u32::from_le_bytes(payload[3..7].try_into().ok()?);
            if !EQUIPMENT_SLOTS.contains(&slot) {
                return None;
            }
            EquipmentRequest::Equip {
                slot,
                bag_eid: u32::from_le_bytes(payload[7..11].try_into().ok()?),
            }
        }
        0x29 if payload.len() == 7 => {
            let slot = u32::from_le_bytes(payload[3..7].try_into().ok()?);
            if !EQUIPMENT_SLOTS.contains(&slot) {
                return None;
            }
            EquipmentRequest::Unequip { slot }
        }
        _ => return None,
    };
    Some((cid, request))
}

/// Parse one c2s `07 34 <cid u16> <sub> <body>` Skills request.
///
/// `0x35` is `u32 slot + u8 typeFlag + u32 gcHash` (9B body) and `0x36` is
/// `u32 slot` (4B body); either may be followed by an `EntitySynchInfo` trailer
/// that this parser deliberately ignores.
fn parse_skills_request(payload: &[u8]) -> Option<(u16, SkillRequest)> {
    if payload.len() < 3 {
        return None;
    }
    let cid = u16::from_le_bytes(payload[0..2].try_into().ok()?);
    if cid == 0 {
        return None;
    }
    let request = match payload[2] {
        0x32 if payload.len() >= 8 && payload[3] == 0x04 => SkillRequest::Buy {
            hash: u32::from_le_bytes(payload[4..8].try_into().ok()?),
        },
        0x35 if payload.len() >= 12 => SkillRequest::Place {
            slot: u32::from_le_bytes(payload[3..7].try_into().ok()?),
            hash: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        },
        0x36 if payload.len() >= 7 => SkillRequest::Remove {
            slot: u32::from_le_bytes(payload[3..7].try_into().ok()?),
        },
        _ => return None,
    };
    Some((cid, request))
}

/// Override the class kit's skill ids with the live hotbar assignments.
///
/// Skills not on the hotbar get ids from 200 up (the reference's skill-book
/// range), so they stay in the manipulator list but off the bar.
fn apply_skill_slots(skills: &mut [(String, u32, u8)], slots: &BTreeMap<u32, String>) {
    let mut next_free = 200u32;
    for (fqn, id, _) in skills.iter_mut() {
        if let Some((slot, _)) = slots
            .iter()
            .find(|(_, assigned)| assigned.as_str() == fqn.as_str())
        {
            *id = *slot;
        } else {
            *id = next_free;
            next_free += 1;
        }
    }
}

/// The c2s `[u16 cid][0x1E][u16 buyer][u32 item]` buy body, or `None`.
///
/// The payload is the `(07,34)` body without its envelope, so it is the nine
/// bytes `07 34 | cid | 0x1E | buyer | item` carries after the tag. A `0x1F`
/// sell does not parse here and falls through.
/// The c2s trainer body: `[cid u16][0x32][player eid u32][ref type u8]
/// [skill hash u32]` (12 bytes), with an optional synch trailer.
///
/// The client's `SkillTrainer::trainSkill` writes the player id, the class-id
/// ref tag (`0x04`) and the skill's DJB2 hash; the live capture is the same
/// 14-byte frame.
fn parse_trainer_request(payload: &[u8]) -> Option<(u16, u32, u8, u32)> {
    if payload.len() < 12 {
        return None;
    }
    let cid = u16::from_le_bytes(payload[0..2].try_into().ok()?);
    if cid == 0 || payload[2] == 0x1E || payload[2] == 0x1F {
        return None;
    }
    let player = u32::from_le_bytes(payload[3..7].try_into().ok()?);
    let ref_type = payload[7];
    let hash = u32::from_le_bytes(payload[8..12].try_into().ok()?);
    if hash == 0 {
        return None;
    }
    Some((cid, player, ref_type, hash))
}

fn parse_merchant_buy(payload: &[u8]) -> Option<(u16, u16, u32)> {
    if payload.len() != 9 || payload[2] != 0x1E {
        return None;
    }
    let cid = u16::from_le_bytes(payload[0..2].try_into().ok()?);
    let buyer = u16::from_le_bytes(payload[3..5].try_into().ok()?);
    let item_id = u32::from_le_bytes(payload[5..9].try_into().ok()?);
    if cid == 0 || item_id == 0 {
        return None;
    }
    Some((cid, buyer, item_id))
}

/// The c2s `[u16 cid][0x1F][u16 entityRef][u32 item]` sell body, or `None`.
fn parse_merchant_sell(payload: &[u8]) -> Option<(u16, u16, u32)> {
    if payload.len() != 9 || payload[2] != 0x1F {
        return None;
    }
    let cid = u16::from_le_bytes(payload[0..2].try_into().ok()?);
    let entity_ref = u16::from_le_bytes(payload[3..5].try_into().ok()?);
    let item_id = u32::from_le_bytes(payload[5..9].try_into().ok()?);
    if cid == 0 || item_id == 0 {
        return None;
    }
    Some((cid, entity_ref, item_id))
}

/// The `(04,02)` request: `[name NUL][class NUL][opts...]`, the reference implementation's
/// `messages.parse_create_request`.
fn parse_create_request(payload: &[u8]) -> Option<(String, String, Vec<u8>)> {
    let name_end = payload.iter().position(|&byte| byte == 0)?;
    if name_end == 0 {
        return None;
    }
    let class_end = payload[name_end + 1..].iter().position(|&byte| byte == 0)? + name_end + 1;
    if class_end <= name_end + 1 {
        return None;
    }
    let name = String::from_utf8(payload[..name_end].to_vec()).ok()?;
    let class = String::from_utf8(payload[name_end + 1..class_end].to_vec()).ok()?;
    let opts = payload[class_end + 1..].to_vec();
    Some((name, class, opts))
}

/// The five appearance bytes from the create options, as the reference's
/// `messages.parse_appearance_options` reads them.
fn appearance_from_opts(opts: &[u8]) -> StoreAppearance {
    if opts.len() < 5 {
        return StoreAppearance::default();
    }
    StoreAppearance {
        skin: Some(opts[0]),
        face: Some(opts[1]),
        face_feature: Some(opts[2]),
        hair: Some(opts[3]),
        hair_color: Some(opts[4]),
    }
}

/// The store's appearance bytes as the character-graph node's properties.
fn graph_appearance(appearance: &StoreAppearance) -> GraphAppearance {
    GraphAppearance {
        skin: appearance.skin,
        face: appearance.face,
        face_feature: appearance.face_feature,
        hair: appearance.hair,
        hair_color: appearance.hair_color,
    }
}

/// The per-session roster word the `(09,35)` body opens with. It is a session
/// allocation artifact, not content; the high byte mirrors the reference's shape.
fn roster_head(conn_id: u64) -> [u8; 4] {
    ((conn_id as u32) | 0x8000_0000).to_le_bytes()
}

/// Whether a zone runs the avatar's HP synch compare, i.e. is a combat zone.
///
/// Town and the tutorial set the client's peaceful-zone bit
/// (`(*(behavior+0x11C)+0x95) & 1`, the same bit `0x4A4AE7` reads for
/// "Can't use in town."), which makes `EntitySynchInfo::Validate` skip the
/// avatar HP compare. Every other zone (dungeons) leaves the bit clear, so any
/// avatar-HP trailer the server sends to the owner is zero-tolerance validated
/// and fatals once a mob has damaged the avatar. The server knows the zone, so
/// it uses this to suppress the avatar-HP move-ack it would otherwise
/// originate spontaneously.
fn is_combat_zone(zone: &str) -> bool {
    zone != ZONE_TOWN && zone != "tutorial"
}

/// The class base FQN a stored class names (`avatar.classes.WarlockFemale` ->
/// `avatar.classes.WarlockBase`), or `None`. The six shipped `RequiredClass`
/// gates are written against this name; `None` means "apply no class gate".
fn class_base_of(aclass: &str) -> Option<String> {
    drf_content::CLASS_BASES
        .iter()
        .find(|base| aclass.contains(*base))
        .map(|base| format!("avatar.classes.{base}Base"))
}

/// The held journal the join installs in the QuestManager create.
///
/// Only `accepted` and `complete` rows replay; a `turned-in` quest left the
/// journal when it was handed in and its only trace is the completed set that
/// gates re-offering (`sessions/quest.py:quest_journal_entries`). Each row's
/// objectives are the ones the store recorded at accept, so a re-login ships
/// the same AddQuest body the accept did.
fn held_journal(character: &Character) -> Vec<JournalRecord> {
    character
        .quests
        .iter()
        .filter(|(_, record)| matches!(record.state, QuestState::Accepted | QuestState::Complete))
        .filter_map(|(hex, record)| {
            let qid = qid_from_hex(hex)?;
            Some(JournalRecord {
                qid: quest_gc_type(qid),
                instance_ref: record.instance_ref,
                // The stored state is authoritative after a turn-in, but a
                // kill/goTo objective can be complete while the row is still
                // `accepted`. The journal's AddQuest is what seeds the
                // client's cached completion bit, so derive it from the
                // objective evidence as well.
                all_done: record.state == QuestState::Complete
                    || record.objectives.is_empty()
                    || record
                        .objectives
                        .iter()
                        .all(|objective| objective.flags & 0x01 != 0),
                objectives: record
                    .objectives
                    .iter()
                    .map(|objective| (objective.flags, objective.label.as_bytes().to_vec()))
                    .collect(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};

    use crate::store::Store;
    use crate::world::World;

    #[test]
    fn difficulty_scales_match_the_client_fixed_point() {
        assert_eq!(difficulty_scale(0), UnitScale::NORMAL);
        assert_eq!(difficulty_scale(1).health, 0x180);
        assert_eq!(difficulty_scale(1).damage, 0x135);
        assert_eq!(difficulty_scale(2).loot, 0x200);
        assert_eq!(difficulty_scale(3).damage, 0x1cf);
        let base = 114 * 256;
        assert_eq!(scaled_hp_wire(base, UnitScale::NORMAL), base);
        assert_eq!(scaled_hp_wire(base, difficulty_scale(1)), 171 * 256);
        assert_eq!(scaled_hp_wire(base, difficulty_scale(2)), 228 * 256);
        assert_eq!(scaled_hp_wire(base, difficulty_scale(3)), 285 * 256);
    }

    /// Self-cleaning temp directory for a session's `s1_store.json`.
    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(tag: &str) -> TestDir {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let mut path = std::env::temp_dir();
            path.push(format!("drf-hook-{tag}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            TestDir { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn game_pkg_dir() -> Option<PathBuf> {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo = manifest.ancestors().nth(4)?;
        [
            repo.join("dr-client-666-testing"),
            repo.join("Dungeon_Runners_Client_666"),
        ]
        .into_iter()
        .find(|dir| dir.join("game.pkg").is_file())
    }

    /// A real loopback game connection plus a session wired to it. The ticker is
    /// off (it would interleave WorldIntervals) and the trust flag is off (the
    /// UDP listener must not bind during tests); records are injected directly.
    fn test_session(
        tag: &str,
        content: Option<(PathBuf, PathBuf)>,
    ) -> (GameSession, TcpStream, TestDir) {
        let dir = TestDir::new(tag);
        let store = Store::open(dir.path().join("s1_store.json")).expect("open store");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let client = TcpStream::connect(listener.local_addr().unwrap()).expect("connect loopback");
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("client read timeout");
        let (server, _) = listener.accept().expect("accept loopback");
        let writer = Arc::new(Mutex::new(server.try_clone().expect("clone server")));
        let shared = Arc::new(Shared::new(
            store,
            World::new(),
            "127.0.0.1".to_string(),
            0,
            content,
        ));
        shared.set_ticker_enabled(false);
        let session = GameSession::new(server, writer, shared);
        (session, client, dir)
    }

    /// Drain every complete segment the session wrote and inflate its inner.
    fn read_frames(stream: &mut TcpStream) -> Vec<Vec<u8>> {
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => raw.extend_from_slice(&chunk[..read]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(_) => break,
            }
        }
        let mut frames = Vec::new();
        while let Ok((segment, used)) = Segment::decode(&raw) {
            if let Ok(inner) = segment.payload.inflate() {
                frames.push(inner);
            }
            raw.drain(..used);
        }
        frames
    }

    fn attack_inner(target: u16) -> Vec<u8> {
        let mut inner = vec![0x07, 0x34, 0x00, 0x00, 0x00, 0x00, 0x50, 0x01, 0x0A];
        inner.extend_from_slice(&target.to_le_bytes());
        inner
    }

    fn attack(target: u16) -> AttackUse {
        AttackUse {
            target,
            slot: 0x0A,
            subtype: 0x50,
        }
    }

    fn tracked_mob(hp_wire: u32) -> TrackedMob {
        TrackedMob {
            spawn_index: 0,
            hp_wire,
            hp: hp_wire / 256,
            classname: "creatures.test.TestMob".to_string(),
            level: 1,
            pose: [100, 200, 0, 0],
        }
    }

    /// Build a session with one persisted level-2 character whose unspent pool
    /// is `points`, and install it as the selected avatar.
    fn spend_session(tag: &str, points: u32) -> (GameSession, TcpStream, TestDir, String, u32) {
        let (mut session, client, dir) = test_session(tag, None);
        let account = session.shared.account();
        let sqlid = {
            let mut store = session.shared.store.lock().expect("store");
            store.ensure_account(&account).expect("ensure account");
            let sqlid = store
                .create_char(
                    &account,
                    "Hero",
                    "avatar.classes.WarlockFemale",
                    StoreAppearance::default(),
                )
                .expect("create character");
            store
                .set_progress(&account, sqlid, 2, 0, points)
                .expect("seed progress");
            sqlid
        };
        let character = session
            .shared
            .store
            .lock()
            .expect("store")
            .get_char(&account, sqlid)
            .cloned()
            .expect("seeded character");
        session.selected = Some(sqlid);
        session.selected_level = 2;
        session.avatar_eid = Some(0x1000);
        session.restore_character_state(&character);
        (session, client, dir, account, sqlid)
    }

    #[test]
    fn spend_attribute_point_applies_persists_and_confirms() {
        let (mut session, mut client, dir, account, sqlid) = spend_session("spend", 5);
        session.dispatch(0x07, 0x04, &[0x00, 0x10, REQ_SPEND_ATTRIB, 2, 2]);

        assert_eq!(session.attribute_points, 3);
        assert_eq!(session.attributes, [0, 0, 2, 0]);

        let frames = read_frames(&mut client);
        let frame = frames
            .iter()
            .find(|frame| frame.len() >= 2 && frame[0] == 0x07 && frame[1] == 0x03)
            .expect("the spend is confirmed with a (07,03) entity update");
        assert_eq!(&frame[2..4], &0x1000u16.to_le_bytes());
        assert_eq!(frame[4], REQ_SPEND_ATTRIB);
        assert_eq!(frame[5], 2, "Toughness");
        assert_eq!(frame[6], 2, "two points");
        assert_eq!(frame[7], 0x02, "HP-bearing synch trailer");
        assert_eq!(
            u32::from_le_bytes(frame[8..12].try_into().unwrap()),
            session.avatar_hp_word()
        );
        assert_eq!(frame[12], 0x06, "CEM end");

        let reopened = Store::open(dir.path().join("s1_store.json")).expect("reopen");
        let hero = reopened
            .get_char(&account, sqlid)
            .expect("character survives");
        assert_eq!(hero.attribute_points, 3);
        assert_eq!(
            (hero.strength, hero.agility, hero.toughness, hero.power),
            (0, 0, 2, 0)
        );

        // The re-join head ships the same split (the client's `Hero::readInit`
        // slots `+0x334..+0x33c`), not zeros.
        let head = drf_proto::build_minimal_avatar_init_payload_with_stats(
            0x1000,
            0x1001,
            hero.avatar_class.as_bytes(),
            None,
            Some(hero.level as u8),
            hero.experience,
            hero.attribute_points as u16,
            [
                hero.strength as u16,
                hero.agility as u16,
                hero.toughness as u16,
                hero.power as u16,
            ],
            &[],
            drf_proto::MINIMAL_AVATAR_WE_FLAGS_ATTACH,
            Some(drf_proto::MINIMAL_AVATAR_HP),
            Some(drf_proto::MINIMAL_AVATAR_MP),
        )
        .expect("rejoin head");
        assert_eq!(&head[41..43], &0u16.to_le_bytes(), "+0x334 STR");
        assert_eq!(&head[43..45], &0u16.to_le_bytes(), "+0x336 AGI");
        assert_eq!(&head[45..47], &2u16.to_le_bytes(), "+0x338 TOUGH");
        assert_eq!(&head[47..49], &0u16.to_le_bytes(), "+0x33a POW");
        assert_eq!(&head[49..51], &3u16.to_le_bytes(), "+0x33c points");
    }

    #[test]
    fn over_spend_and_foreign_entity_ids_are_ignored() {
        let (mut session, mut client, dir, account, sqlid) = spend_session("overspend", 1);

        // More points than the session's pool: dropped, no confirmation.
        session.dispatch(0x07, 0x04, &[0x00, 0x10, REQ_SPEND_ATTRIB, 0, 5]);
        // A request naming an entity the session does not own: dropped.
        session.dispatch(0x07, 0x04, &[0x99, 0x99, REQ_SPEND_ATTRIB, 0, 1]);
        // An out-of-range stat: dropped.
        session.dispatch(0x07, 0x04, &[0x00, 0x10, REQ_SPEND_ATTRIB, 9, 1]);
        // An unknown request type: dropped.
        session.dispatch(0x07, 0x04, &[0x00, 0x10, 0x7f, 0, 1]);

        assert_eq!(session.attribute_points, 1);
        assert_eq!(session.attributes, [0, 0, 0, 0]);
        assert!(
            read_frames(&mut client)
                .iter()
                .all(|frame| !(frame.len() >= 2 && frame[0] == 0x07 && frame[1] == 0x03)),
            "no confirmation is sent for a rejected spend"
        );

        let reopened = Store::open(dir.path().join("s1_store.json")).expect("reopen");
        let hero = reopened
            .get_char(&account, sqlid)
            .expect("character survives");
        assert_eq!(hero.attribute_points, 1, "rejected spends do not persist");
        assert_eq!(
            (hero.strength, hero.agility, hero.toughness, hero.power),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn combat_zone_is_everything_but_town_and_tutorial() {
        // Town and the tutorial set the client's peaceful-zone bit, so the
        // avatar HP compare is skipped and an avatar-HP trailer is safe there.
        assert!(!is_combat_zone(ZONE_TOWN));
        assert!(!is_combat_zone("tutorial"));
        // Every dungeon leaves the bit clear, so the compare runs.
        assert!(is_combat_zone("dungeon00_level01"));
        assert!(is_combat_zone("dungeon01_level01"));
    }

    #[test]
    fn parses_the_hook_death_and_hp_lines() {
        assert_eq!(
            parse_hook_record(b"DRF1 death 4096 3 1024 2048 0 -32768 12345\n"),
            Some(HookEvent::Death(HookMobDeath {
                eid: 4096,
                level: 3,
                pose: [1024, 2048, 0, -32768],
            }))
        );
        assert_eq!(
            parse_hook_record(b"DRF1 hp 4096 5120 12345\n"),
            Some(HookEvent::Hp {
                eid: 4096,
                hp_wire: 5120,
            })
        );
        assert_eq!(parse_hook_record(b"DRF1 death 0 3 1 2 3 4 5\n"), None);
        assert_eq!(parse_hook_record(b"DRF2 death 1 3 1 2 3 4 5\n"), None);
        assert_eq!(parse_hook_record(b"DRF1 death 1 3 1\n"), None);
        assert_eq!(parse_hook_record(b""), None);
    }

    /// The 4-swing placeholder counter only *arms* the fallback: it must not
    /// mirror XP, allocate a drop or send a CEM remove on its own.
    #[test]
    fn placeholder_hits_do_not_emit_loot_or_remove() {
        let (mut session, mut client, _dir) = test_session("counter", None);
        session.avatar_eid = Some(0x1000);
        let eid = 0x2000;
        session.mob_state.insert(eid, tracked_mob(4 * 256));
        let inner = attack_inner(eid);

        for _ in 0..4 {
            session.attack_use(&inner, &attack(eid));
        }

        assert!(
            session.mob_defeat_pending.contains_key(&eid),
            "the counter arms a pending death instead of removing the mob"
        );
        assert!(session.drops.is_empty(), "no loot on the counter");
        assert_eq!(session.experience, 0, "no XP mirror on the counter");

        let frames = read_frames(&mut client);
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.len() >= 2 && frame[0] == 0x07 && frame[1] == 0x35)
                .count(),
            4,
            "every swing is still acked"
        );
        assert!(
            frames.iter().all(|frame| !(frame.len() >= 2
                && frame[0] == 0x07
                && (frame[1] == 0x08 || frame[1] == 0x05))),
            "neither a drop nor a CEM remove is emitted before the client death"
        );
    }

    /// A client `StockUnit::onDead` record finalizes the kill: XP is mirrored,
    /// the drop lands at the reported death pose, and the `(07,05)` remove is
    /// sent after the drop.
    #[test]
    fn client_death_record_emits_drop_at_death_pose_then_remove() {
        let Some(dir) = game_pkg_dir() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let (mut session, mut client, _tmp) =
            test_session("death", Some((dir.join("game.pki"), dir.join("game.pkg"))));
        session.avatar_eid = Some(0x1000);
        let eid = 0x2000;
        // High enough that one swing cannot arm the placeholder fallback.
        let mut mob = tracked_mob(10_000 * 256);
        // A real content creature so the content-driven XP mirror settles a
        // descriptor. The synthetic `creatures.test.TestMob` resolves nothing in
        // `game.pkg`, and the client itself awards no XP for an unsettled
        // descriptor, so the mirror faithfully grants nothing there.
        mob.classname = "world.dungeon00.mob.melee01.rank1".to_string();
        session.mob_state.insert(eid, mob);
        let inner = attack_inner(eid);
        session.attack_use(&inner, &attack(eid));
        assert!(!session.mob_defeat_pending.contains_key(&eid));

        let death_pose = [777, 888, 9, 0];
        session.apply_hook_event(HookEvent::Death(HookMobDeath {
            eid,
            level: 1,
            pose: death_pose,
        }));

        assert!(
            !session.mob_state.contains_key(&eid),
            "the mob is finalized"
        );
        assert_eq!(session.drops.len(), 1, "exactly one drop on the death");
        assert_eq!(session.experience, 500, "the kill XP is mirrored");

        let frames = read_frames(&mut client);
        let drop_at = frames
            .iter()
            .position(|frame| frame.len() >= 2 && frame[0] == 0x07 && frame[1] == 0x08)
            .expect("the death ships a drop");
        let remove_at = frames
            .iter()
            .position(|frame| frame.len() >= 5 && frame[0] == 0x07 && frame[1] == 0x05)
            .expect("the death ships a CEM remove");
        assert!(drop_at < remove_at, "drop precedes the remove");
        let drop = &frames[drop_at];
        assert_eq!(
            u32::from_le_bytes(drop[20..24].try_into().unwrap()),
            death_pose[0] as u32,
            "drop x is the client's death pose, not the player's position"
        );
        assert_eq!(
            u32::from_le_bytes(drop[24..28].try_into().unwrap()),
            death_pose[1] as u32,
            "drop y is the client's death pose"
        );
        let remove = &frames[remove_at];
        assert_eq!(
            &remove[2..4],
            &eid.to_le_bytes(),
            "the removed id is the mob"
        );
        assert_eq!(remove[4], 0x06, "remove ends on the stream terminator");
    }

    /// A trusted session whose death record never arrives must clean up and
    /// finalize through the same authored path. This fixture's class has no
    /// authored loot/UnitDesc, so it still emits neither.
    #[test]
    fn trusted_timeout_cleans_up_without_loot() {
        let (mut session, mut client, _dir) = test_session("leakguard", None);
        session.hook_telemetry = true;
        session.avatar_eid = Some(0x1000);
        let eid = 0x2000;
        session.mob_state.insert(eid, tracked_mob(10_000 * 256));

        session.apply_hook_event(HookEvent::Hp { eid, hp_wire: 0 });
        session.check_mob_timeouts_at(Instant::now() + MOB_DEATH_GRACE + Duration::from_millis(1));

        assert!(!session.mob_state.contains_key(&eid), "state is cleaned up");
        assert!(
            session.drops.is_empty(),
            "an unknown class has no authored loot"
        );
        let frames = read_frames(&mut client);
        assert!(
            frames.iter().all(|frame| !(frame.len() >= 2
                && frame[0] == 0x07
                && (frame[1] == 0x08 || frame[1] == 0x05))),
            "no drop and no remove on the leak-guard path"
        );
    }

    /// A scripted boss whose client death hook is missing must still open its
    /// authored `DoorsToOpenOnDeath` and ship its authored loot from the
    /// trusted timeout fallback (no remove, because the client may have purged
    /// the corpse).
    #[test]
    fn trusted_timeout_finalizes_and_opens_the_authored_death_door() {
        let Some(dir) = game_pkg_dir() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let (mut session, mut client, _tmp) = test_session(
            "trusted-door",
            Some((dir.join("game.pki"), dir.join("game.pkg"))),
        );
        session.hook_telemetry = true;
        session.avatar_eid = Some(0x1000);
        let gate_eid = 0x3000;
        session.placed_entities.push(PlacedCreate {
            entity_id: gate_eid,
            placement_index: 0,
            classname: "world.dungeon01.data.QuestGateD1L1".to_string(),
            kind: drf_content::PlacedKind::Door,
            wire: None,
            zone: None,
            spawn_point: None,
            track_activating_players: false,
            init: Vec::new(),
            components: Vec::new(),
        });
        let eid = 0x2000;
        session.mob_state.insert(
            eid,
            TrackedMob {
                spawn_index: 0,
                hp_wire: 10_000 * 256,
                hp: 10_000,
                classname: "world.dungeon01.mob.Quest.Q12_a3_Level01".to_string(),
                level: 7,
                pose: [100, 200, 0, 0],
            },
        );

        session.apply_hook_event(HookEvent::Hp { eid, hp_wire: 0 });
        session.check_mob_timeouts_at(Instant::now() + MOB_DEATH_GRACE + Duration::from_millis(1));

        assert!(
            !session.drops.is_empty(),
            "the fallback ships the authored drop"
        );
        let frames = read_frames(&mut client);
        assert!(
            frames.iter().any(|frame| {
                frame.len() >= 7
                    && frame[0] == 0x07
                    && frame[1] == 0x03
                    && frame[2..4] == gate_eid.to_le_bytes()
                    && frame[4] == 0x0A
            }),
            "the gate opens on the trusted fallback"
        );
    }

    #[test]
    fn authored_unlock_gate_requires_and_consumes_the_right_item() {
        let Some(dir) = game_pkg_dir() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let (mut session, _client, _tmp) = test_session(
            "unlock-gate",
            Some((dir.join("game.pki"), dir.join("game.pkg"))),
        );
        session.avatar_container_cid = Some(0x0302);
        let door = "Dungeon01Doors.Door_Squeakeasy";
        assert!(
            !session.unlock_allows_activation(door),
            "no passcode -> closed"
        );

        session.bag_entries.insert(
            1,
            BagEntry {
                id: 1,
                class: "Dungeon01ItemPAL.D01_Key_01".to_string(),
                cell: (0, 0),
                count: 1,
                level: 1,
                flags: 0,
                width: 1,
                height: 1,
                record: Vec::new(),
                modifiers: Vec::new(),
            },
        );
        assert!(session.unlock_allows_activation(door), "passcode -> open");
        assert_eq!(session.bag_count("Dungeon01ItemPAL.D01_Key_01"), 0);

        session.bag_entries.insert(
            2,
            BagEntry {
                id: 2,
                class: "QuestItemPAL2.D01_Q10_a1_01".to_string(),
                cell: (1, 0),
                count: 1,
                level: 1,
                flags: 0,
                width: 1,
                height: 1,
                record: Vec::new(),
                modifiers: Vec::new(),
            },
        );
        assert!(session.unlock_allows_activation("world.dungeon01.data.base.Fizz_Rock_NCI"));
        assert_eq!(
            session.bag_count("QuestItemPAL2.D01_Q10_a1_01"),
            1,
            "the mining pick is not consumed"
        );
    }

    #[test]
    fn generated_dungeon_npcs_offer_their_listed_quests() {
        let Some(dir) = game_pkg_dir() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let (mut session, _client, _tmp) = test_session(
            "generated-quest-giver",
            Some((dir.join("game.pki"), dir.join("game.pkg"))),
        );
        session.zone = "dungeon01_level01".to_string();
        session.selected_level = 7;
        session.placed_entities.push(PlacedCreate {
            entity_id: 0x2000,
            placement_index: 0,
            classname: "world.dungeon01.npc.questgiver_l01_c".to_string(),
            kind: drf_content::PlacedKind::Npc,
            wire: None,
            zone: None,
            spawn_point: None,
            track_activating_players: false,
            init: Vec::new(),
            components: Vec::new(),
        });

        let offered: Vec<u32> = session
            .offer_groups()
            .into_iter()
            .filter(|(npc, _)| npc.eq_ignore_ascii_case("world.dungeon01.npc.questgiver_l01_c"))
            .flat_map(|(_, qids)| qids)
            .collect();
        assert!(offered.contains(&drf_content::dfc_hash(
            "world.dungeon01.quest.Q13_a1".as_bytes()
        )));
        assert!(offered.contains(&drf_content::dfc_hash(
            "world.dungeon01.quest.Q14_a1".as_bytes()
        )));
    }

    #[test]
    fn generated_layouts_persist_until_reset_or_reselect() {
        let (mut session, _client, _tmp) = test_session("layout-persistence", None);
        let first = session.maze_seed_for("dungeon01_level01");
        let again = session.maze_seed_for("dungeon01_level01");
        assert_eq!(first, again, "the same session reuses a zone's layout");
        let other = session.maze_seed_for("dungeon01_level02");
        assert_ne!(first, other, "different zones get independent seeds");

        session.dispatch(0x09, 0x26, &[]);
        assert!(
            session.maze_seeds.is_empty(),
            "Reset Instances clears layouts"
        );
    }

    #[test]
    fn zone_progress_restores_fog_kills_doors_and_consumed_places() {
        let Some(dir) = game_pkg_dir() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let (mut session, _client, _tmp) = test_session(
            "zone-progress",
            Some((dir.join("game.pki"), dir.join("game.pkg"))),
        );
        session.zone = "dungeon01_level01".to_string();
        session.maze_seed = session.maze_seed_for("dungeon01_level01");
        let zone = session.zone.clone();
        session.ensure_zone_progress(&zone, session.maze_seed);
        assert_eq!(
            session.explored_ready_words().map(|words| words.len()),
            Some(17),
            "the Ready count is the client's minimap word count"
        );
        assert_eq!(
            session
                .explored_ready_words()
                .unwrap()
                .iter()
                .map(|word| word.count_ones())
                .sum::<u32>(),
            0
        );
        session.mark_explored_position([0, 0]);
        assert!(
            session
                .explored_ready_words()
                .unwrap()
                .iter()
                .any(|word| *word != 0),
            "walking marks the explored words"
        );

        session.mark_mob_defeated(3);
        session.mark_door_opened(4);
        session.mark_place_consumed(5);
        let progress = session.zone_progress.get("dungeon01_level01").unwrap();
        assert!(progress.defeated_mobs.contains(&3));
        assert!(progress.opened_doors.contains(&4));
        assert!(progress.consumed_places.contains(&5));

        session.reset_instances();
        assert!(
            session.zone_progress.is_empty(),
            "Reset Instances forgets progress"
        );
    }

    #[test]
    fn mob_loot_uses_the_character_level_not_the_spawn_level() {
        let Some(dir) = game_pkg_dir() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let (mut session, _client, _tmp) = test_session(
            "player-level-loot",
            Some((dir.join("game.pki"), dir.join("game.pkg"))),
        );
        session.shared.set_content_loot(true);
        session.avatar_eid = Some(0x1000);
        session.selected_level = 11;
        session.difficulty = 3;

        // Sissirat's authored spawn level in dungeon01 is 6. Its LegendGG
        // rows are Rich/VeryRich/StupidRich with MinLevel 9/12/15, so using
        // the mob level filters every LegendGG roll. At character level 11,
        // Rich is eligible and the two Insane-scaled slots must mint gold.
        session.on_mob_death(
            0x1015,
            [-41010, 12086, 10239, 0],
            "world.dungeon01.mob.boss",
            6,
        );
        let currencies = session
            .drops
            .values()
            .filter(|drop| drop.amount.is_some())
            .count();
        assert!(
            currencies >= 5,
            "expected the 2x LegendGG slots scaled to 5 on Insane to mint gold, got {currencies}"
        );
    }

    #[test]
    fn item_objective_counts_subclass_alias_in_bag() {
        let Some(dir) = game_pkg_dir() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let (mut session, _client, _tmp) = test_session(
            "quest-item-subclass",
            Some((dir.join("game.pki"), dir.join("game.pkg"))),
        );
        session.bag_entries.insert(
            7,
            BagEntry {
                id: 7,
                // Q12_a4's OnAcceptItemGenerator is this alias; its own
                // ItemObjective names the base D01_Q12_a4_01 class.
                class: "QuestItemPAL.D01Q12_5".to_string(),
                cell: (0, 0),
                count: 1,
                level: 1,
                flags: 0,
                width: 1,
                height: 1,
                record: Vec::new(),
                modifiers: Vec::new(),
            },
        );
        assert_eq!(
            session.bag_count("QuestItemPAL2.D01_Q12_a4_01"),
            1,
            "subclass aliases must satisfy a base-class ItemObjective"
        );
    }

    #[test]
    fn dropping_a_bag_item_keeps_its_modifiers_for_pickup() {
        let Some(dir) = game_pkg_dir() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let (mut session, _client, _tmp) = test_session(
            "drop-modifiers",
            Some((dir.join("game.pki"), dir.join("game.pkg"))),
        );
        session.avatar_container_cid = Some(0x0302);
        session.player_pos = Some([1024, 2048]);
        let modifiers = vec![ModifierRoll {
            class: "items.modifiers.TestModifier".to_string(),
            level: 9,
        }];
        session.bag_entries.insert(
            7,
            BagEntry {
                id: 7,
                class: "2HAxe1PAL.2HAxe1-1".to_string(),
                cell: (0, 0),
                count: 1,
                level: 9,
                flags: 0,
                width: 1,
                height: 1,
                record: Vec::new(),
                modifiers: modifiers.clone(),
            },
        );
        session.drop_bag_item(7);
        let drop = session.drops.values().next().expect("the item dropped");
        assert_eq!(drop.item_class, "2HAxe1PAL.2HAxe1-1");
        assert_eq!(drop.item_level, Some(9));
        assert_eq!(
            drop.modifiers, modifiers,
            "the ground item carries the bag mods"
        );
    }

    #[test]
    fn trainer_request_trainers_a_new_skill_and_debits_gold() {
        let Some(dir) = game_pkg_dir() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let (mut session, mut client, _tmp) = test_session(
            "trainer-request",
            Some((dir.join("game.pki"), dir.join("game.pkg"))),
        );
        let npc = "world.town.npc.TrainerMage";
        let catalog = session
            .shared
            .trainer_catalogs_for(&[npc.to_string()])
            .remove(npc)
            .expect("mage trainer catalog");
        let fire_bolt = catalog
            .skills
            .iter()
            .find(|skill| skill.fqn == "skills.generic.FireBolt")
            .expect("FireBolt row")
            .clone();
        let cid = 0x7001;
        session
            .trainer_components
            .insert(cid, TrainerRegistration::from_catalog(npc, &catalog));
        session.avatar_eid = Some(0x1000);
        session.avatar_class = Some("avatar.classes.MageMale".to_string());
        session.skills_cid = Some(0x0301);
        session.avatar_container_cid = Some(0x0302);
        session.gold = 100_000;

        session.trainer_train(cid, 0x1000, 0x04, fire_bolt.hash);
        session.trainer_train(cid, 0x1000, 0x04, fire_bolt.hash);
        session.trainer_train(cid, 0x1000, 0x04, fire_bolt.hash);

        let value = session.shared.skill_value_per_level().unwrap_or(1113.621);
        let expected: u32 = (1..=3)
            .map(|rank| train_cost(value, &fire_bolt, rank))
            .sum();
        assert_eq!(session.gold, 100_000 - expected);
        assert_eq!(
            session.skill_levels.get("skills.generic.FireBolt"),
            Some(&3)
        );
        assert!(session
            .skill_catalog
            .iter()
            .any(|(fqn, _, level)| fqn == "skills.generic.FireBolt" && *level == 3));

        let frames = read_frames(&mut client);
        assert!(frames.iter().any(|frame| {
            frame.len() > 8
                && frame[0] == 0x07
                && frame[1] == 0x35
                && frame
                    .windows(b"skills.generic.FireBolt\0".len())
                    .any(|window| window == b"skills.generic.FireBolt\0")
        }));
        assert!(
            frames.iter().any(|frame| {
                frame.windows(5).any(|window| {
                    window[0] == 0x35 && window[1..3] == [0x02, 0x03] && window[3] == 0x20
                })
            }),
            "the gold delta rides the same component stream"
        );
    }

    #[test]
    fn trainer_request_parser_reads_the_live_shape() {
        // 0734 cc0f 32 c2850000 04 <hash> — the 14-byte live trainer frame.
        let mut body = vec![0xcc, 0x0f, 0x32];
        body.extend_from_slice(&0x85c2u32.to_le_bytes());
        body.push(0x04);
        body.extend_from_slice(&0x997c6a0au32.to_le_bytes());
        assert_eq!(
            parse_trainer_request(&body),
            Some((0x0fcc, 0x85c2, 0x04, 0x997c6a0a))
        );
        assert_eq!(parse_trainer_request(&body[..11]), None);
    }

    /// Without `--trust-client-sim` the legacy placeholder model is preserved:
    /// HP telemetry arms the fallback and the expired grace window finalizes
    /// the death (drop + XP, still no remove).
    #[test]
    fn hp_zero_arms_and_the_legacy_timeout_finalizes() {
        let Some(dir) = game_pkg_dir() else {
            eprintln!("skip: no game.pkg on this machine");
            return;
        };
        let (mut session, mut client, _tmp) = test_session(
            "timeout",
            Some((dir.join("game.pki"), dir.join("game.pkg"))),
        );
        session.avatar_eid = Some(0x1000);
        let eid = 0x2000;
        session.mob_state.insert(eid, tracked_mob(10_000 * 256));

        session.apply_hook_event(HookEvent::Hp { eid, hp_wire: 0 });
        assert!(
            session.mob_defeat_pending.contains_key(&eid),
            "client HP zero arms the fallback"
        );

        session.check_mob_timeouts_at(Instant::now() + MOB_DEATH_GRACE + Duration::from_millis(1));
        assert!(
            !session.mob_state.contains_key(&eid),
            "the fallback finalizes"
        );
        assert_eq!(session.drops.len(), 1, "the fallback still drops loot");

        let frames = read_frames(&mut client);
        assert!(
            frames
                .iter()
                .any(|frame| frame.len() >= 2 && frame[0] == 0x07 && frame[1] == 0x08),
            "fallback emits the drop"
        );
        assert!(
            frames
                .iter()
                .all(|frame| !(frame.len() >= 2 && frame[0] == 0x07 && frame[1] == 0x05)),
            "the fallback does not send a remove that could latch CEM error 6"
        );
    }
    #[test]
    fn parses_unit_container_request_shapes() {
        let mut put = vec![0x34, 0x12, 0x29, 0x0B, 6, 2];
        assert_eq!(
            parse_unit_container_request(&put),
            Some((
                0x1234,
                UnitContainerRequest::Put {
                    inventory_id: 0x0B,
                    x: 6,
                    y: 2
                }
            ))
        );
        put.pop();
        assert_eq!(parse_unit_container_request(&put), None, "short put");

        let mut get = vec![0x34, 0x12, 0x28];
        get.extend_from_slice(&7u32.to_le_bytes());
        assert_eq!(
            parse_unit_container_request(&get),
            Some((0x1234, UnitContainerRequest::Get(7)))
        );
        get.push(0);
        assert_eq!(parse_unit_container_request(&get), None, "long get");

        let mut use_pos = vec![0x34, 0x12, 0x26];
        use_pos.extend_from_slice(&7u32.to_le_bytes());
        use_pos.extend_from_slice(&1i32.to_le_bytes());
        use_pos.extend_from_slice(&2i32.to_le_bytes());
        use_pos.extend_from_slice(&3i32.to_le_bytes());
        assert_eq!(
            parse_unit_container_request(&use_pos),
            Some((
                0x1234,
                UnitContainerRequest::UsePos {
                    item: 7,
                    x: 1,
                    y: 2,
                    z: 3
                }
            ))
        );
        use_pos.pop();
        assert_eq!(
            parse_unit_container_request(&use_pos),
            None,
            "short use-pos"
        );

        let mut use_target = vec![0x34, 0x12, 0x27];
        use_target.extend_from_slice(&7u32.to_le_bytes());
        use_target.extend_from_slice(&9u16.to_le_bytes());
        assert_eq!(
            parse_unit_container_request(&use_target),
            Some((
                0x1234,
                UnitContainerRequest::UseTarget { item: 7, target: 9 }
            ))
        );
        assert_eq!(
            parse_unit_container_request(&[0x34, 0x12, 0x21]),
            Some((0x1234, UnitContainerRequest::Empty(0x21)))
        );
        assert_eq!(
            parse_unit_container_request(&[0x34, 0x12, 0x22]),
            Some((0x1234, UnitContainerRequest::Empty(0x22)))
        );
        assert_eq!(
            parse_unit_container_request(&[0x34, 0x12, 0x2A]),
            Some((0x1234, UnitContainerRequest::Currency))
        );
        assert_eq!(
            parse_unit_container_request(&[0x34, 0x12, 0x23, 0, 0, 0, 0]),
            None,
            "a zero item id resolves nothing"
        );
        assert_eq!(
            parse_unit_container_request(&[0x00, 0x00, 0x21]),
            None,
            "a zero cid is not this family"
        );
    }

    #[test]
    fn known_inventory_cids_do_not_reach_the_unhandled_registry() {
        let (mut session, _client, _dir) = test_session("inventory-route", None);
        session.avatar_container_cid = Some(0xE100);
        session.equipment_cid = Some(0xE101);
        // UnitContainer 0x21 is a routed no-op; equipment 0x29 parses and then
        // holds on the empty worn map. Neither may fall through to `record`.
        session.dispatch(0x07, 0x34, &[0x00, 0xE1, 0x21]);
        session.dispatch(0x07, 0x34, &[0x01, 0xE1, 0x29, 8, 0, 0, 0]);
        let snapshot = crate::opcodes::snapshot_for_test();
        assert!(!snapshot.contains("\"key\":\"07:34:21\""), "{snapshot}");
        assert!(!snapshot.contains("\"key\":\"07:34:29\""), "{snapshot}");
    }

    #[test]
    fn parses_equipment_request_shapes() {
        let equip = [0x34, 0x12, 0x28, 8, 0, 0, 0, 7, 0, 0, 0];
        assert_eq!(
            parse_equipment_request(&equip),
            Some((
                0x1234,
                EquipmentRequest::Equip {
                    slot: 8,
                    bag_eid: 7
                }
            ))
        );
        let mut short = equip.to_vec();
        short.pop();
        assert_eq!(parse_equipment_request(&short), None, "short equip");
        let unequip = [0x34, 0x12, 0x29, 8, 0, 0, 0];
        assert_eq!(
            parse_equipment_request(&unequip),
            Some((0x1234, EquipmentRequest::Unequip { slot: 8 }))
        );
        let mut bad_slot = unequip;
        bad_slot[3] = 9;
        assert_eq!(
            parse_equipment_request(&bad_slot),
            None,
            "slot 9 is not valid"
        );
    }
}
