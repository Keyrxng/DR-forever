// SPDX-License-Identifier: GPL-3.0-or-later

//! Listeners, the auth handshake, and shared server state.
//!
//! The auth port uses `drf-proto::auth`'s `[u16 total LE][Blowfish body]`
//! framing. The game port uses `drf-wire`'s binary-derived [`Segment`] framing;
//! each inflated segment body is one `[channel][mtype][payload]` inner.
//!
//! One thread per connection and one accept thread per port is deliberate:
//! the server is small, `std::net` + `std::thread` need no runtime, and the
//! shared state is a small set of `Mutex`es.
//!
//! The auth connection opens with the plaintext [`WELCOME`] greeting the client
//! requires before it will cipher anything. The game connection answers the
//! client's raw `0x0A` hello with the 23-byte connect-ack (see
//! [`crate::session`]).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use drf_content::{
    author_item_record_by_class, checkpoint_class_by_hash as content_checkpoint_class_by_hash,
    checkpoint_destination as content_checkpoint_destination, derive_avatar_inventory_roster,
    derive_bag_geometry, derive_generated_zone_start, derive_generated_zone_start_with_seed,
    derive_merchant_tabs, derive_npc_base_manipulator, derive_trainer, derive_world_entities,
    derive_zone_spawn, destination_for, economy, fill_generated_tabs, generate_generator_items,
    generate_loot as content_generate_loot, generate_loot_items, generated_return_portal_with_seed,
    generated_room_portals, generated_room_world_entities, mob_profile_at_level,
    nci_item_generators, on_death_summons, room_waypoint_position_with_seed,
    spawns_with_seed_reachable, starting_checkpoints as content_starting_checkpoints,
    treasure_slots as content_treasure_slots, zone_def as content_zone_def, zone_is_generated,
    Archive, ContentError, Corpus, CreatureManipulator, Destination, GeneratedZoneStart,
    GiverDirection, ItemProfile, ItemRecord, LootItem, LootSlot, MerchantTab, MersenneTwister,
    NpcManipulator, OnDeathSummon, PlacedEntity, QuestCorpus, QuestDef, Quote, TrainerCatalog,
};
use drf_proto::auth::{
    decode_login, decrypt_packet, frame, AboutToPlay, LoginOk, PlayOk, TYPE_ABOUT_TO_PLAY,
    TYPE_LOGIN, TYPE_LOGIN_OK, TYPE_PLAY_OK, TYPE_SERVER_LIST, TYPE_SERVER_LIST_EXT, WELCOME,
};
use drf_proto::ClassContent;

use crate::session;
use crate::store::Store;
use crate::world::World;

/// Account a game session falls back to when no auth login has been seen.
pub const DEFAULT_ACCOUNT: &str = "s1player";

/// The `LoginOk` words used by this server. `drf-proto::auth` documents that
/// the client reads the shape (nine `u32` + flag), but the values are not
/// protocol constants; they are this implementation's bootstrap values.
const LOGIN_OK_WORDS: [u32; 9] = [
    0xFFEE_FFEE,
    0xAABB_AABB,
    0xDDCC_DDCC,
    0xBBCC_BBCC,
    0,
    0xFFFF_FFFF,
    0xFFFF_FFFF,
    0,
    0,
];

/// State shared by the auth and game connections.
pub struct Shared {
    /// Character persistence, serialised behind one lock.
    pub store: Mutex<Store>,
    /// The town entity registry and tick block.
    pub world: Mutex<World>,
    last_login: Mutex<Option<String>>,
    /// The parsed PKI index, opened once at startup. Every content query used
    /// to reopen and reinflate `game.pki`; the archive is immutable, so share
    /// it instead.
    archive: Option<Arc<Archive>>,
    content: Mutex<HashMap<String, ClassContent>>,
    /// The quest corpus, derived once from `game.pkg` and memoised behind an
    /// `Arc`: callers read it, never mutate it, and a deep `QuestCorpus` clone
    /// duplicates the entire archive index.
    quest_corpus: Mutex<Option<Arc<QuestCorpus>>>,
    game_ip: String,
    /// The port advertised in the auth `ServerList`. Defaults to the bound
    /// `game_port`; a proxy harness overrides it so the client connects to
    /// the proxy's game listener while the listener stays on `game_port`.
    advertised_game_port: AtomicU16,
    next_conn: AtomicU64,
    /// Whether each generated-dungeon entry gets a fresh random maze seed.
    /// The binary enables it; tests keep the deterministic CRC32 so their
    /// expected layouts stay fixed.
    random_maze_seeds: AtomicBool,
    /// Whether mob drops roll a content `ItemGenerator` (the binary) or the
    /// reference S3 test table (tests).
    content_loot: AtomicBool,
    /// Whether each game session spawns its 30 Hz `(07,0D)` WorldInterval
    /// ticker. Production leaves this on; the offline replay harness turns it
    /// off so a capture contains only solicited responses (the wall-clock
    /// ticker would otherwise interleave frames non-deterministically).
    ticker_enabled: AtomicBool,
    /// Whether the paired client is trusted to simulate its own units
    /// (`--trust-client-sim`). When set, the per-entity
    /// `EntitySynchInfo::Validate` hook bypasses the avatar's HP compare and
    /// the WorldInterval may carry the owner move-ack in combat zones. Default
    /// off: today's suppression. Read once when a game session is created.
    trust_client_sim: AtomicBool,
    /// Whether the join marks the avatar as an admin (`--admin`). This sets
    /// the player permission byte the client's `Avatar::IsAdmin` reads, which
    /// arms the shipped `/admin` and `/adminui` surfaces. Default off.
    admin_commands: AtomicBool,
}

impl Shared {
    /// Build the shared state. `content_paths` is the `(game.pki, game.pkg)`
    /// pair used to derive per-class graph content; `None` serves bare nodes.
    pub fn new(
        store: Store,
        world: World,
        game_ip: String,
        game_port: u16,
        content_paths: Option<(PathBuf, PathBuf)>,
    ) -> Shared {
        // Build the quest corpus once, at startup: it parses every quest
        // `.gc` in `game.pkg` and would otherwise cost the first join seconds.
        let archive = content_paths
            .as_ref()
            .and_then(|(pki, pkg)| Archive::open(pki, pkg).ok())
            .map(Arc::new);
        let quest_corpus = archive.as_ref().and_then(|archive| {
            let mut corpus = Corpus::from_shared(Arc::clone(archive));
            QuestCorpus::from_corpus(&mut corpus).ok().map(Arc::new)
        });
        Shared {
            store: Mutex::new(store),
            world: Mutex::new(world),
            last_login: Mutex::new(None),
            archive,
            content: Mutex::new(HashMap::new()),
            quest_corpus: Mutex::new(quest_corpus),
            game_ip,
            advertised_game_port: AtomicU16::new(game_port),
            next_conn: AtomicU64::new(1),
            ticker_enabled: AtomicBool::new(true),
            trust_client_sim: AtomicBool::new(false),
            admin_commands: AtomicBool::new(false),
            random_maze_seeds: AtomicBool::new(false),
            content_loot: AtomicBool::new(false),
        }
    }

    /// The account a game session serves: the last auth login, else
    /// [`DEFAULT_ACCOUNT`].
    pub fn account(&self) -> String {
        self.last_login
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string())
    }

    /// Record the auth login name; the game session adopts it.
    pub fn set_login(&self, user: &str) {
        if let Ok(mut guard) = self.last_login.lock() {
            *guard = Some(user.to_string());
        }
    }

    /// Allocate a connection id (the world's session key).
    pub fn alloc_conn(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// The shared PKI index, opened once at startup.
    fn archive(&self) -> Option<Arc<Archive>> {
        self.archive.as_ref().map(Arc::clone)
    }

    /// Whether a `game.pki`/`game.pkg` pair was supplied. Zone validation is
    /// skipped without it, so a content-less test harness keeps its seeded
    /// zone names.
    pub fn has_content(&self) -> bool {
        self.archive.is_some()
    }

    /// The advertised `(game_ip, game_port)` for the auth `ServerList`.
    ///
    /// `game_port` is the port the client is told to connect to; it defaults
    /// to the bound listener port but can be overridden with
    /// [`set_advertised_game_port`](Self::set_advertised_game_port) so game
    /// traffic flows through a proxy.
    pub fn game_endpoint(&self) -> (&str, u16) {
        (
            &self.game_ip,
            self.advertised_game_port.load(Ordering::SeqCst),
        )
    }

    /// Override the port advertised in the auth `ServerList` without moving
    /// the game listener, which stays bound to the original `game_port`.
    ///
    /// Defaults to the bound port; only a proxied harness calls this.
    pub fn set_advertised_game_port(&self, port: u16) {
        self.advertised_game_port.store(port, Ordering::SeqCst);
    }

    /// The port advertised in the auth `ServerList` (see
    /// [`game_endpoint`](Self::game_endpoint)).
    pub fn advertised_game_port(&self) -> u16 {
        self.advertised_game_port.load(Ordering::SeqCst)
    }

    /// Enable content-generator mob loot (the binary does; tests keep the S3
    /// table).
    pub fn set_content_loot(&self, enabled: bool) {
        self.content_loot.store(enabled, Ordering::SeqCst);
    }

    /// Whether mob drops roll a content `ItemGenerator`.
    pub fn content_loot(&self) -> bool {
        self.content_loot.load(Ordering::SeqCst)
    }

    /// Enable per-entry random maze seeds (the binary does; tests do not).
    pub fn set_random_maze_seeds(&self, enabled: bool) {
        self.random_maze_seeds.store(enabled, Ordering::SeqCst);
    }

    /// Whether generated dungeons get a fresh per-entry maze seed.
    pub fn random_maze_seeds(&self) -> bool {
        self.random_maze_seeds.load(Ordering::SeqCst)
    }

    /// Enable or disable the per-session `(07,0D)` WorldInterval ticker.
    ///
    /// Only a CLI (or the offline replay harness) calls this; the production
    /// server never does, so the default stays `true`. Read once when a game
    /// session is created.
    pub fn set_ticker_enabled(&self, enabled: bool) {
        self.ticker_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Whether the per-session WorldInterval ticker is currently enabled.
    pub fn ticker_enabled(&self) -> bool {
        self.ticker_enabled.load(Ordering::SeqCst)
    }

    /// Enable or disable trust in the client's own unit simulation
    /// (`--trust-client-sim`).
    ///
    /// Only a CLI calls this. When set, the paired client hook bypasses the
    /// avatar's HP compare and the owner move-ack is allowed in combat zones;
    /// default off keeps today's suppression. Read once when a game session is
    /// created, so it must be set before the connection is accepted.
    pub fn set_trust_client_sim(&self, enabled: bool) {
        self.trust_client_sim.store(enabled, Ordering::SeqCst);
    }

    /// Whether the paired client is trusted to simulate its own units.
    pub fn trust_client_sim(&self) -> bool {
        self.trust_client_sim.load(Ordering::SeqCst)
    }

    /// Enable or disable marking the avatar as an admin on join
    /// (`--admin`). Read once when a game session is created, so it must be
    /// set before the connection is accepted.
    pub fn set_admin_commands(&self, enabled: bool) {
        self.admin_commands.store(enabled, Ordering::SeqCst);
    }

    /// Whether the join marks the avatar as an admin.
    pub fn admin_commands(&self) -> bool {
        self.admin_commands.load(Ordering::SeqCst)
    }

    /// Derive (and memoise) the character-graph content for `aclass`, or
    /// `None` when no `game.pkg` was supplied or the class does not resolve.
    pub fn content_for(&self, aclass: &str) -> Option<ClassContent> {
        if let Ok(cache) = self.content.lock() {
            if let Some(content) = cache.get(aclass) {
                return Some(content.clone());
            }
        }
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        let content = ClassContent::derive(&mut corpus, aclass).ok()?;
        if let Ok(mut cache) = self.content.lock() {
            cache.insert(aclass.to_string(), content.clone());
        }
        Some(content)
    }

    /// The `game.pkg` spawn pose for `zone`, in the x256 wire domain.
    ///
    /// `None` when no `game.pkg` was supplied or the zone resolves no
    /// waypoint; the caller then ships the head with its pose words zeroed.
    pub fn zone_spawn_pose(&self, zone: &str) -> Option<[u32; 4]> {
        self.zone_spawn_pose_requested(zone, None)
    }

    /// The `game.pkg` spawn pose for `zone`, resolving `requested` (a
    /// portal's `SpawnPoint`) as the zone's own name for its entry point.
    ///
    /// `None` is the refusal, exactly as [`zone_spawn_pose`](Self::zone_spawn_pose):
    /// no `game.pkg`, or no waypoint any rung of the chain names. A generated
    /// destination names its entry with a `ZoneStart` room node and no
    /// coordinate, so it returns `None` and the join zeroes the pose rather
    /// than inventing an arrival.
    pub fn zone_spawn_pose_requested(
        &self,
        zone: &str,
        requested: Option<&str>,
    ) -> Option<[u32; 4]> {
        let archive = self.archive()?;
        derive_zone_spawn(&archive, zone, requested)
            .ok()
            .flatten()
            .map(|spawn| spawn.pose)
    }

    /// The avatar's shipped starting checkpoint classes
    /// (`avatar/base/StartingCheckpointList.gc`), for the QuestManager create.
    pub fn starting_checkpoints(&self) -> Vec<String> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        content_starting_checkpoints(&archive).unwrap_or_default()
    }

    /// Resolve an obelisk checkpoint GC hash (`0x04` + u32) to its FQN.
    pub fn checkpoint_class_by_hash(&self, hash: u32) -> Option<String> {
        let archive = self.archive()?;
        content_checkpoint_class_by_hash(&archive, hash)
    }

    /// A `Checkpoint` class's travel destination (`Zone`, `SpawnPoint`).
    ///
    /// The obelisk dialog sends the selected checkpoint's GC type; the class
    /// file itself states where that obelisk goes. `None` when `game.pkg` is
    /// absent or the class states no `Description.Zone`.
    pub fn checkpoint_destination(&self, class_name: &str) -> Option<Destination> {
        let archive = self.archive()?;
        content_checkpoint_destination(&archive, class_name)
            .ok()
            .flatten()
    }

    /// The shipped obelisk `Checkpoint` classes matching any visited zone, in
    /// `Order` sequence. Non-faithful "everywhere I've been" list.
    pub fn checkpoints_for_zones(&self, zones: &[String]) -> Vec<String> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        drf_content::checkpoints_for_zones(&archive, zones).unwrap_or_default()
    }

    /// The zone and spawn-point NAME a dead character returns to.
    ///
    /// `RespawnZone` is the destination; a death **inside** it keeps that
    /// zone's own `RespawnSpawnPoint` (town: `Start`), while a death **in
    /// another zone** (the dungeons) returns to the destination's graveyard
    /// waypoint, `Respawn` (the name `town.world` places at the
    /// `Townston_graveyard` static). The join resolves the name through
    /// [`zone_spawn_pose_requested`](Self::zone_spawn_pose_requested), which
    /// falls back to the destination's `RespawnSpawnPoint` and `Start` when a
    /// world places no `Respawn` waypoint.
    pub fn zone_respawn(&self, zone: &str) -> Option<(String, Option<String>)> {
        let archive = self.archive()?;
        let current = content_zone_def(&archive, zone).ok().flatten()?;
        let stated = current
            .respawn_zone
            .clone()
            .unwrap_or_else(|| crate::world::ZONE_TOWN.to_string());
        // `!pvp` is a shipped sentinel, not a zone (thirteen PvP arenas state
        // `RespawnZone = !pvp`). A dangling or unservable destination is the
        // same class of content bug; respawning in place is safer than sending
        // an assign the client can never join.
        let servable = !stated.starts_with('!') && self.zone_exists(&stated);
        let destination = if servable { stated } else { zone.to_string() };
        let same_zone = destination.eq_ignore_ascii_case(zone);
        let spawn = if same_zone {
            current.respawn_spawn_point.clone()
        } else {
            Some("Respawn".to_string())
        };
        Some((destination, spawn))
    }

    /// Whether `zone`'s `.world` declares `Generated = true`.
    pub fn zone_is_generated(&self, zone: &str) -> bool {
        let Some(archive) = self.archive() else {
            return false;
        };
        zone_is_generated(&archive, zone).unwrap_or(false)
    }

    /// Whether the package can serve `zone`: a readable `.zone` def or a
    /// generated `.world`. Used to refuse an admin transfer to a name the
    /// content does not describe, which would otherwise hang the client on the
    /// join.
    pub fn zone_exists(&self, zone: &str) -> bool {
        let Some(archive) = self.archive() else {
            return false;
        };
        content_zone_def(&archive, zone).ok().flatten().is_some()
            || zone_is_generated(&archive, zone).unwrap_or(false)
    }

    /// A generated offshoot room's authored `WaypointName` in x256 world
    /// coordinates, or `None` when this zone/seed has no such waypoint.
    pub fn generated_room_waypoint_position(
        &self,
        zone: &str,
        name: &str,
        seed: u32,
    ) -> Option<[i32; 2]> {
        let archive = self.archive()?;
        room_waypoint_position_with_seed(&archive, zone, name, seed)
            .ok()
            .flatten()
            .map(|[x, y]| [(x * 256.0).round() as i32, (y * 256.0).round() as i32])
    }

    /// A GENERATED destination's entry with the per-entry maze seed the
    /// Assign carried.
    pub fn zone_generated_start_with_seed(
        &self,
        zone: &str,
        requested: Option<&str>,
        seed: u32,
    ) -> Option<GeneratedZoneStart> {
        let archive = self.archive()?;
        derive_generated_zone_start_with_seed(&archive, zone, requested, seed)
            .ok()
            .flatten()
    }

    /// The client's minimap grid for a generated `zone` with this maze seed.
    ///
    /// `None` for a hand-placed zone, an absent `game.pkg`, or a world that
    /// places no cells. The count is the `u16` word count the client's
    /// `MiniMapExplored::ReadExploredBits` compares against its own `+0x40`.
    pub fn maze_minimap_with_seed(
        &self,
        zone: &str,
        seed: u32,
    ) -> Option<drf_content::MazeMinimap> {
        let archive = self.archive()?;
        let layout = drf_content::maze::generate_layout_with_seed(&archive, zone, seed)
            .ok()
            .flatten()?;
        drf_content::minimap_for_layout(&layout)
    }

    /// The generated mob spawns for `zone` with the per-entry maze seed.
    pub fn mob_spawns_with_seed(
        &self,
        zone: &str,
        seed: u32,
        player_level: u16,
    ) -> Vec<drf_content::MobSpawn> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        spawns_with_seed_reachable(&archive, zone, seed, player_level).unwrap_or_default()
    }

    /// A GENERATED destination's entry, from its own `.world`: the `ZoneStart`
    /// room node's `SpawnName` and the cell-derived pose.
    ///
    /// `None` when no `game.pkg` was supplied, the world is not generated, it
    /// places no `ZoneStart` room, or its archetype states no `TileSize`. The
    /// join uses this only when [`zone_spawn_pose`](Self::zone_spawn_pose)
    /// refuses, so a hand-placed zone's own waypoint always wins.
    pub fn zone_generated_start(
        &self,
        zone: &str,
        requested: Option<&str>,
    ) -> Option<GeneratedZoneStart> {
        let archive = self.archive()?;
        derive_generated_zone_start(&archive, zone, requested)
            .ok()
            .flatten()
    }

    /// The destination `zone`'s placement matching `(name, classname, wire)`
    /// states, from `game.pkg`.
    ///
    /// `None` when no `game.pkg` was supplied, the world cannot be read, the
    /// clicked entity states no `Zone` (an NPC), or the click does not
    /// identify one placement. Every `None` is a refusal to invent a
    /// destination, never another zone's.
    pub fn zone_destination(
        &self,
        zone: &str,
        name: Option<&str>,
        classname: Option<&str>,
        wire: Option<(i64, i64)>,
    ) -> Option<Destination> {
        let archive = self.archive()?;
        destination_for(&archive, zone, name, classname, wire)
            .ok()
            .flatten()
    }

    /// Whether a spawned unit class is the authored type or inherits it.
    /// Kill objectives name the content base (`world.dungeon00.mob.boss`)
    /// while the client hook reports the placement class
    /// (`world.dungeon00.mob.boss_posse.RattleTooth`).
    pub fn class_is_kind_of(&self, classname: &str, target: &str) -> bool {
        let Some(archive) = self.archive() else {
            return classname.eq_ignore_ascii_case(target);
        };
        drf_content::class_is_kind_of(&archive, classname, target).unwrap_or(false)
    }

    /// One property from a class's `extends` chain, read from `game.pkg`.
    pub fn class_property(&self, classname: &str, name: &str) -> Option<String> {
        let archive = self.archive()?;
        drf_content::class_property(&archive, classname, name)
            .ok()
            .flatten()
    }

    /// One property from a class's chain or its nested `Description` blocks.
    pub fn class_description_property(&self, classname: &str, name: &str) -> Option<String> {
        let archive = self.archive()?;
        drf_content::class_description_property(&archive, classname, name)
            .ok()
            .flatten()
    }

    /// Whether a door/NCI class is authored `Locked`.
    pub fn class_is_locked(&self, classname: &str) -> bool {
        let Some(archive) = self.archive() else {
            return false;
        };
        drf_content::class_is_locked(&archive, classname).unwrap_or(false)
    }

    /// The item gate a locked door/NCI states, read from `game.pkg`.
    pub fn class_unlock_requirement(
        &self,
        classname: &str,
    ) -> Option<drf_content::UnlockRequirement> {
        let archive = self.archive()?;
        drf_content::class_unlock_requirement(&archive, classname)
            .ok()
            .flatten()
    }

    /// The entities `<zone>.world` places, derived from `game.pkg`.
    ///
    /// Empty when no `game.pkg` was supplied or the world cannot be read; the
    /// join then ships no placed entities (the pre-existing behaviour).
    pub fn zone_entities(&self, zone: &str) -> Vec<PlacedEntity> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        derive_world_entities(&archive, zone).unwrap_or_default()
    }

    /// The generated mob spawns for `zone`, derived from `game.pkg`.
    ///
    /// Empty when no `game.pkg` was supplied, the zone is not a generated maze,
    /// or its content cannot be read. The join appends these to the batch after
    /// the placed entities.
    pub fn mob_spawns(&self, zone: &str) -> Vec<drf_content::MobSpawn> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        drf_content::spawns(&archive, zone).unwrap_or_default()
    }

    /// `<zone>.world`'s entities, with a generated dungeon's portals derived
    /// from the entry's own seed (the start return and every linked room).
    pub fn zone_entities_with_seed(&self, zone: &str, seed: u32) -> Vec<PlacedEntity> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        let mut entities = derive_world_entities(&archive, zone).unwrap_or_default();
        if !zone_is_generated(&archive, zone).unwrap_or(false) {
            return entities;
        }
        entities.retain(|entity| entity.classname != drf_content::GENERATED_RETURN_PORTAL_CLASS);
        if let Ok(Some(portal)) = generated_return_portal_with_seed(&archive, zone, seed) {
            entities.push(portal);
        }
        entities.extend(generated_room_portals(&archive, zone, seed).unwrap_or_default());
        entities.extend(generated_room_world_entities(&archive, zone, seed).unwrap_or_default());
        entities
    }

    /// Generated room portals (Exit/Oneoff links) for this entry's maze seed.
    pub fn generated_room_portals(&self, zone: &str, seed: u32) -> Vec<PlacedEntity> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        drf_content::generated_room_portals(&archive, zone, seed).unwrap_or_default()
    }

    /// The manipulator blocks each spawn's creature authors, keyed by the
    /// spawn classname (`game.pkg`'s merged `Manipulators` block).
    ///
    /// One archive open for the whole set. A classname that resolves nothing
    /// still gets an entry: the generic base melee weapon fallback. Empty when
    /// no `game.pkg` was supplied; the join then ships the fallback for every
    /// mob.
    pub fn mob_manipulators_for(
        &self,
        classnames: &[String],
    ) -> HashMap<String, Vec<CreatureManipulator>> {
        let mut out = HashMap::new();
        let Some(archive) = self.archive() else {
            return out;
        };
        let mut corpus = Corpus::from_shared(archive);
        for class in classnames {
            if out.contains_key(class) {
                continue;
            }
            let entries = drf_content::creature_manipulators(&mut corpus, class);
            out.insert(class.clone(), entries);
        }
        out
    }

    /// The one manipulator `game.pkg`'s `npc/Base.gc` declares, or `None`.
    ///
    /// `None` when no `game.pkg` was supplied or the archetype cannot be read;
    /// the NPC component run is then not shipped.
    pub fn npc_base_manipulator(&self) -> Option<NpcManipulator> {
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        derive_npc_base_manipulator(&mut corpus).ok().flatten()
    }

    /// The `*.Merchant` tabs `game.pkg` authors for each class in `npc_classes`,
    /// keyed by class, with vendors that author no non-empty block omitted.
    ///
    /// One archive open for the whole set; `None` (or no `game.pkg`) is empty.
    pub fn merchant_tabs_for(&self, npc_classes: &[String]) -> HashMap<String, Vec<MerchantTab>> {
        let mut out = HashMap::new();
        let Some(archive) = self.archive() else {
            return out;
        };
        let mut corpus = Corpus::from_shared(archive);
        for class in npc_classes {
            if let Ok(Some(mut tabs)) = derive_merchant_tabs(&mut corpus, class) {
                // `AutoGenerateItems` tabs author only a generator and a level
                // window; their shelf is drawn here. Without this every
                // generated vendor (including Draykop's Amazon stock) ships
                // empty.
                fill_generated_tabs(&mut corpus, class, &mut tabs);
                if !tabs.is_empty() {
                    out.insert(class.clone(), tabs);
                }
            }
        }
        out
    }

    /// Every trainer NPC in `npc_classes` whose class authors a
    /// `SkillTrainer` block, keyed by NPC class. One archive open for the set.
    pub fn trainer_catalogs_for(&self, npc_classes: &[String]) -> HashMap<String, TrainerCatalog> {
        let mut out = HashMap::new();
        let Some(archive) = self.archive() else {
            return out;
        };
        for class in npc_classes {
            if let Ok(Some(catalog)) = derive_trainer(&archive, class) {
                if !catalog.skills.is_empty() {
                    out.insert(class.clone(), catalog);
                }
            }
        }
        out
    }

    /// The authored Bling Gnome skill/summon contract, when the package has it.
    pub fn bling_gnome_profile(&self) -> Option<drf_content::BlingGnomeProfile> {
        let archive = self.archive()?;
        drf_content::bling_gnome_profile(&archive).ok().flatten()
    }

    /// Whether a skill class roots in `PassiveSkill`/`ActivePassiveSkill`.
    pub fn skill_is_passive(&self, fqn: &str) -> bool {
        let Some(archive) = self.archive() else {
            return fqn.to_ascii_lowercase().contains("passive");
        };
        drf_content::skill_is_passive(&archive, fqn).unwrap_or(false)
    }

    /// The authored `SkillValuePerLevel` gold knob, if `game.pkg` states one.
    pub fn skill_value_per_level(&self) -> Option<f64> {
        let archive = self.archive()?;
        drf_content::skill_value_per_level(&archive).ok().flatten()
    }

    /// Draw `count` quest-reward classes from an `ItemGenerator` archetype
    /// (`WishingWellIG`, ...), at the highest phase the level can use.
    pub fn reward_items(
        &self,
        generator: &str,
        level: u16,
        count: u32,
        seed: u64,
    ) -> Vec<(String, u8)> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        let mut corpus = Corpus::from_shared(archive);
        generate_generator_items(&mut corpus, generator, level, count as usize, seed)
    }

    /// The authored `TreasureGenerator1..10` slots a unit class states.
    pub fn treasure_slots(&self, unit_class: &str) -> Vec<LootSlot> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        let mut corpus = Corpus::from_shared(archive);
        content_treasure_slots(&mut corpus, unit_class).unwrap_or_default()
    }

    /// Generate all rolls from a set of authored slots on one MT19937 stream.
    pub fn generate_slots(
        &self,
        slots: &[LootSlot],
        level: u8,
        rng: &mut MersenneTwister,
        is_member: bool,
    ) -> Vec<LootItem> {
        self.generate_slots_scaled(slots, level, rng, is_member, 0x100)
    }

    /// Generate all rolls with a Q8 loot-count multiplier.
    pub fn generate_slots_scaled(
        &self,
        slots: &[LootSlot],
        level: u8,
        rng: &mut MersenneTwister,
        is_member: bool,
        loot_factor: u16,
    ) -> Vec<LootItem> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        let mut corpus = Corpus::from_shared(archive);
        drf_content::generate_slots_scaled(&mut corpus, slots, level, rng, is_member, loot_factor)
    }

    /// Generate `count` rolls from one generator on one MT19937 stream.
    pub fn generate_loot(
        &self,
        generator: &str,
        level: u8,
        count: usize,
        rng: &mut MersenneTwister,
        is_member: bool,
    ) -> Vec<LootItem> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        let mut corpus = Corpus::from_shared(archive);
        content_generate_loot(&mut corpus, generator, level, count, rng, is_member)
    }

    /// The destination an NCI's `TeleportZone`/`TeleportPoint` states.
    pub fn nci_destination(&self, classname: &str) -> Option<Destination> {
        let archive = self.archive()?;
        let (zone, spawn) = drf_content::nci_teleport_destination(&archive, classname)
            .ok()
            .flatten()?;
        Some(Destination { zone, spawn })
    }

    /// The canonical client spelling of an item class.
    pub fn canonical_item_class(&self, class_name: &str) -> Option<String> {
        let archive = self.archive()?;
        let corpus = Corpus::from_shared(archive);
        corpus.canonical_class_name(class_name)
    }

    /// Is this class a spawned combat creature (as opposed to an NPC or a
    /// world-object `SpawnUnit` such as the bonus-stage teleport)?
    pub fn class_is_creature(&self, classname: &str) -> bool {
        let Some(archive) = self.archive() else {
            return false;
        };
        drf_content::class_is_creature(&archive, classname).unwrap_or(false)
    }

    /// The `OnDeath` procs a creature class states, with their `SpawnUnit`
    /// children and chance percent.
    pub fn on_death_summons(&self, classname: &str) -> Vec<OnDeathSummon> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        on_death_summons(&archive, classname).unwrap_or_default()
    }

    /// The `SpawnUnit` classes a class's `Description.SpellEffect` summons when
    /// activated (the relic guardian chests' `SummonRelicGuardian`).
    pub fn spell_effect_summons(&self, classname: &str) -> Vec<(String, i32)> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        drf_content::spell_effect_summons(&archive, classname).unwrap_or_default()
    }

    /// The HP/behavior/manipulators a runtime summon needs. The level is the
    /// dead unit's level plus the proc's `LevelOffset`, not the zone base.
    pub fn mob_summon_profile(
        &self,
        classname: &str,
        level: u8,
    ) -> Option<(u32, Option<String>, Vec<CreatureManipulator>)> {
        let archive = self.archive()?;
        let (hp_wire, behavior) = mob_profile_at_level(&archive, classname, level).ok()??;
        let mut corpus = Corpus::from_shared(archive);
        let manipulators = drf_content::creature_manipulators(&mut corpus, classname);
        Some((hp_wire, behavior, manipulators))
    }

    /// The authored loot slots an NCI (chest) class states.
    pub fn nci_loot_slots(&self, classname: &str) -> Vec<LootSlot> {
        self.nci_item_generators(classname)
            .into_iter()
            .map(|(generator, count)| LootSlot { generator, count })
            .collect()
    }

    /// Draw loot through the authored generator tree (rarity/level/chance
    /// aware), unlike the flat quest-reward pool.
    pub fn loot_items(
        &self,
        generator: &str,
        level: u16,
        count: usize,
        seed: u64,
    ) -> Vec<(String, u8)> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        let mut corpus = Corpus::from_shared(archive);
        generate_loot_items(&mut corpus, generator, level, count, seed)
    }

    /// The `ItemGenerator`/`ItemCount` pairs an NCI class states (chests).
    pub fn nci_item_generators(&self, classname: &str) -> Vec<(String, u32)> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        nci_item_generators(&archive, classname).unwrap_or_default()
    }

    /// The avatar's nine-inventory roster, derived from `game.pkg`.
    ///
    /// Empty when no `game.pkg` was supplied or the archetype cannot be read;
    /// the container record then ships an empty roster.
    pub fn avatar_inventory_roster(&self) -> Vec<(String, u8)> {
        let Some(archive) = self.archive() else {
            return Vec::new();
        };
        let mut corpus = Corpus::from_shared(archive);
        derive_avatar_inventory_roster(&mut corpus)
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| (entry.fqn, entry.id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The avatar's backpack `(width, height)`, from `game.pkg`.
    ///
    /// `None` when no `game.pkg` was supplied or its `avatar.base.Inventory`
    /// block states no footprint; every bag add then holds.
    pub fn bag_geometry(&self) -> Option<(u8, u8)> {
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        derive_bag_geometry(&mut corpus).ok().flatten()
    }

    /// An authored `Item::readData` record for `class_name`, from `game.pkg`.
    ///
    /// `None` when no `game.pkg` was supplied or the corpus defines no block
    /// for the class; an error when the block states no footprint.
    pub fn author_item_record(
        &self,
        class_name: &str,
        item_id: u32,
        quantity: u32,
        level: Option<u8>,
        cell: (u8, u8),
    ) -> Result<Option<ItemRecord>, ContentError> {
        let Some(archive) = self.archive() else {
            return Ok(None);
        };
        let mut corpus = Corpus::from_shared(archive);
        author_item_record_by_class(&mut corpus, class_name, item_id, quantity, level, cell)
    }

    /// The item's best authored quality index (0 Quest .. 7 Mythic); the
    /// gnome convert filter uses the native quality byte, not the class name.
    pub fn item_quality(&self, class_name: &str, modifier_fqns: &[String]) -> Option<usize> {
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        let prof = drf_content::item_profile(&mut corpus, class_name)
            .ok()
            .flatten()?;
        let wire = prof.quality.unwrap_or(drf_content::QUALITY_DEFAULT);
        drf_content::best_modifier_quality(&mut corpus, &prof, modifier_fqns, Some(wire)).ok()
    }

    /// `Item::getValue` in native (not sell-capped) form, as the gnome's
    /// item-to-gold conversion reads it.
    pub fn item_native_value(
        &self,
        class_name: &str,
        item_level: u32,
        observer_level: u16,
        quality: Option<usize>,
    ) -> Option<u32> {
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        let prof = drf_content::item_profile(&mut corpus, class_name)
            .ok()
            .flatten()?;
        let knobs = drf_content::knobs(&mut corpus).ok()?;
        let value =
            drf_content::item_get_value(&knobs, &prof, item_level, observer_level, quality, false)
                .ok()?;
        u32::try_from(value.max(1)).ok()
    }

    /// An authored item record with server-assigned modifier children.
    pub fn author_item_record_with_mods(
        &self,
        class_name: &str,
        item_id: u32,
        quantity: u32,
        level: Option<u8>,
        cell: (u8, u8),
        modifiers: &[(String, u8)],
    ) -> Result<Option<drf_content::ItemRecord>, ContentError> {
        let Some(archive) = self.archive() else {
            return Ok(None);
        };
        let mut corpus = Corpus::from_shared(archive);
        drf_content::author_item_record_with_mods(
            &mut corpus,
            class_name,
            item_id,
            quantity,
            level,
            cell,
            modifiers,
        )
    }

    /// The item class's `game.pkg` profile, or `None` when no `game.pkg` was
    /// supplied or the class does not resolve.
    pub fn item_profile(&self, class_name: &str) -> Option<ItemProfile> {
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        drf_content::item_profile(&mut corpus, class_name)
            .ok()
            .flatten()
    }

    /// The maximum stack the item class states, with the reference's 10 for a
    /// consumable that states no `MaxStackSize`. `1` means not stackable; a
    /// class `game.pkg` does not settle is also `1` (fail closed, no merge).
    pub fn item_max_stack(&self, class_name: &str) -> u32 {
        match self.item_profile(class_name) {
            Some(profile) => match profile.max_stack_size {
                Some(max) if max > 0 => max.max(1),
                _ if profile.consumable == Some(true) => 10,
                _ => 1,
            },
            None => 1,
        }
    }

    /// Whether the item class's own `Description.Consumable` is true.
    pub fn item_is_consumable(&self, class_name: &str) -> bool {
        self.item_profile(class_name)
            .is_some_and(|profile| profile.consumable == Some(true))
    }

    /// The DFC root of an item class (the key a `Manipulators` entry's
    /// readState body is chosen by).
    pub fn item_dfc_root(&self, class_name: &str) -> Option<String> {
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        corpus.dfc_root(class_name).ok()
    }

    /// The number of `ItemModifier`-derived status bytes `class_name`'s
    /// `Item::readData` body carries, from `game.pkg`.
    ///
    /// `None` when no `game.pkg` was supplied or the class does not resolve;
    /// the drop path then fails closed rather than emit an under-length body.
    pub fn item_readdata_slots(&self, class_name: &str) -> Option<usize> {
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        corpus.item_readdata_slots(class_name).ok()
    }

    /// The client's per-creature kill-XP descriptor fields for `classname`,
    /// from `game.pkg`. `None` when no `game.pkg` was supplied or the spawn
    /// does not resolve; the kill path then mirrors no award rather than
    /// inventing one.
    pub fn creature_xp_info(
        &self,
        classname: &str,
    ) -> Option<drf_content::experience::CreatureXpInfo> {
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        drf_content::experience::creature_xp_info(&mut corpus, classname).ok()
    }

    /// The whole buy quote for a vendor purchase, from `game.pkg`.
    ///
    /// `observer_level` is the BUYER's level. `None` when no `game.pkg` was
    /// supplied; an error when the package does not settle the price.
    pub fn merchant_price(
        &self,
        merchant_class: &str,
        item_class: &str,
        item_level: u32,
        count: u32,
        modifier_fqns: &[String],
        observer_level: Option<u16>,
    ) -> Option<Result<Quote, ContentError>> {
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        Some(economy::price(
            &mut corpus,
            merchant_class,
            item_class,
            item_level,
            count,
            modifier_fqns,
            observer_level,
        ))
    }

    /// The client's sell value for one item class, or `None` when no
    /// `game.pkg` was supplied.
    pub fn merchant_sell_price(
        &self,
        merchant_class: &str,
        item_class: &str,
        item_level: u32,
        observer_level: Option<u16>,
    ) -> Option<Result<i64, ContentError>> {
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        Some(economy::sell_price(
            &mut corpus,
            merchant_class,
            item_class,
            item_level,
            &[],
            observer_level,
        ))
    }

    /// The quest corpus, derived once from `game.pkg` and memoised. `None`
    /// when no `game.pkg` was supplied or it cannot be read.
    pub fn quest_corpus(&self) -> Option<Arc<QuestCorpus>> {
        if let Ok(cache) = self.quest_corpus.lock() {
            if let Some(corpus) = cache.as_ref() {
                return Some(Arc::clone(corpus));
            }
        }
        let archive = self.archive()?;
        let mut corpus = Corpus::from_shared(archive);
        let quests = Arc::new(QuestCorpus::from_corpus(&mut corpus).ok()?);
        if let Ok(mut cache) = self.quest_corpus.lock() {
            *cache = Some(Arc::clone(&quests));
        }
        Some(quests)
    }

    /// The offer directory for a character in `zone`: the placed NPCs' quests,
    /// gated by level window, class and the completed-prereq chain.
    ///
    /// `direction` is [`GiverDirection::Listed`] for a content zone (where a
    /// character ACCEPTs) and [`GiverDirection::Union`] for town. Empty when
    /// no corpus is available.
    pub fn quest_offer_directory(
        &self,
        zone: &str,
        level: u16,
        class_base: Option<&str>,
        direction: GiverDirection,
        completed: Option<&std::collections::HashSet<u32>>,
    ) -> Vec<(String, Vec<QuestDef>)> {
        let Some(corpus) = self.quest_corpus() else {
            return Vec::new();
        };
        let npcs = corpus.zone_npcs(zone);
        corpus
            .offer_groups(level, &npcs, class_base, direction, completed)
            .into_iter()
            .map(|(npc, quests)| (npc, quests.into_iter().cloned().collect()))
            .collect()
    }
}

/// A bound server: two listeners over one [`Shared`].
pub struct Server {
    auth_listener: TcpListener,
    game_listener: TcpListener,
    shared: Arc<Shared>,
}

impl Server {
    /// Bind the auth and game listeners and build the shared state.
    ///
    /// The game port is read back from the bound socket, so `127.0.0.1:0`
    /// advertises the ephemeral port it actually got.
    pub fn new(
        auth_addr: &str,
        game_addr: &str,
        game_ip: &str,
        store: Store,
        content_paths: Option<(PathBuf, PathBuf)>,
    ) -> std::io::Result<Server> {
        let auth_listener = TcpListener::bind(auth_addr)?;
        let game_listener = TcpListener::bind(game_addr)?;
        let game_port = game_listener.local_addr()?.port();
        let shared = Arc::new(Shared::new(
            store,
            World::new(),
            game_ip.to_string(),
            game_port,
            content_paths,
        ));
        Ok(Server {
            auth_listener,
            game_listener,
            shared,
        })
    }

    /// The bound auth port.
    pub fn auth_port(&self) -> u16 {
        self.auth_listener
            .local_addr()
            .map(|addr| addr.port())
            .unwrap_or(0)
    }

    /// The bound game port.
    pub fn game_port(&self) -> u16 {
        self.game_listener
            .local_addr()
            .map(|addr| addr.port())
            .unwrap_or(0)
    }

    /// A handle to the shared state (for tests and callers that seed it).
    pub fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    /// Enable or disable the per-session WorldInterval ticker (see
    /// [`Shared::set_ticker_enabled`]). Must be set before the game connection
    /// that will be replayed is accepted.
    pub fn set_ticker_enabled(&self, enabled: bool) {
        self.shared.set_ticker_enabled(enabled);
    }

    /// Enable or disable trust in the client's own unit simulation (see
    /// [`Shared::set_trust_client_sim`]). Must be set before the game
    /// connection is accepted.
    pub fn set_random_maze_seeds(&self, enabled: bool) {
        self.shared.set_random_maze_seeds(enabled);
    }

    /// Enable content-generator mob loot.
    pub fn set_content_loot(&self, enabled: bool) {
        self.shared.set_content_loot(enabled);
    }

    pub fn set_trust_client_sim(&self, enabled: bool) {
        self.shared.set_trust_client_sim(enabled);
    }

    /// Whether the paired client is trusted to simulate its own units.
    pub fn trust_client_sim(&self) -> bool {
        self.shared.trust_client_sim()
    }

    /// Enable or disable the join admin flag (see
    /// [`Shared::set_admin_commands`]). Must be set before the game
    /// connection is accepted.
    pub fn set_admin_commands(&self, enabled: bool) {
        self.shared.set_admin_commands(enabled);
    }

    /// Whether the join marks the avatar as an admin.
    pub fn admin_commands(&self) -> bool {
        self.shared.admin_commands()
    }

    /// Override the game port advertised in the auth `ServerList`, keeping
    /// the bound listener on [`game_port`](Self::game_port) (see
    /// [`Shared::set_advertised_game_port`]). Must be set before the auth
    /// handshake that will read it.
    pub fn set_advertised_game_port(&self, port: u16) {
        self.shared.set_advertised_game_port(port);
    }

    /// The game port advertised in the auth `ServerList` (defaults to the
    /// bound [`game_port`](Self::game_port)).
    pub fn advertised_game_port(&self) -> u16 {
        self.shared.advertised_game_port()
    }

    /// Run the accept loops. Blocks until the game listener fails.
    pub fn serve(self) {
        let auth_listener = self.auth_listener;
        let auth_shared = Arc::clone(&self.shared);
        thread::spawn(move || {
            for stream in auth_listener.incoming().flatten() {
                let shared = Arc::clone(&auth_shared);
                thread::spawn(move || {
                    let _ = handle_auth(stream, shared);
                });
            }
        });
        for stream in self.game_listener.incoming().flatten() {
            let shared = Arc::clone(&self.shared);
            thread::spawn(move || session::serve_game(stream, shared));
        }
    }
}

/// The auth connection: Login -> LoginOk + ServerList, ServerListExt ->
/// ServerList, AboutToPlay -> PlayOk.
fn handle_auth(mut stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let peer = peer_name(&stream);
    eprintln!("auth {peer}: accepted");
    // The plaintext ProtocolVer greeting must precede every ciphered packet:
    // it is what makes the client initialise Blowfish and send `Login`.
    stream.write_all(&WELCOME)?;
    loop {
        let mut length = [0u8; 2];
        if stream.read_exact(&mut length).is_err() {
            eprintln!("auth {peer}: closed");
            return Ok(());
        }
        let total = u16::from_le_bytes(length) as usize;
        if !(2..=4096).contains(&total) {
            eprintln!("auth {peer}: bad frame length {total}; closing");
            return Ok(());
        }
        let mut body = vec![0u8; total - 2];
        stream.read_exact(&mut body)?;
        let decrypted = match decrypt_packet(&body) {
            Ok(decrypted) => decrypted,
            Err(_) => continue,
        };
        match decrypted.mtype {
            TYPE_LOGIN => {
                let Ok((user, _pwlen)) = decode_login(&decrypted.payload) else {
                    continue;
                };
                let user = String::from_utf8_lossy(&user).to_string();
                if let Ok(mut store) = shared.store.lock() {
                    let _ = store.ensure_account(&user);
                }
                shared.set_login(&user);
                let login_ok = LoginOk {
                    words: LOGIN_OK_WORDS,
                    flag: 1,
                };
                stream.write_all(&frame(TYPE_LOGIN_OK, &login_ok.encode()))?;
                let (ip, port) = shared.game_endpoint();
                stream.write_all(&frame(TYPE_SERVER_LIST, &server_list_payload(ip, port)))?;
                eprintln!("auth {peer}: Login user={user:?} -> LoginOk + ServerList({ip}:{port})");
            }
            TYPE_SERVER_LIST_EXT => {
                let (ip, port) = shared.game_endpoint();
                stream.write_all(&frame(TYPE_SERVER_LIST, &server_list_payload(ip, port)))?;
                eprintln!("auth {peer}: ServerListExt -> ServerList({ip}:{port})");
            }
            TYPE_ABOUT_TO_PLAY => {
                let request = AboutToPlay::decode(&decrypted.payload).unwrap_or(AboutToPlay {
                    session: 0,
                    server: 0,
                });
                let play_ok = PlayOk {
                    key: next_key(),
                    word: 0,
                    server: request.server,
                };
                stream.write_all(&frame(TYPE_PLAY_OK, &play_ok.encode()))?;
                eprintln!(
                    "auth {peer}: AboutToPlay session={:#018x} server={} -> PlayOk",
                    request.session, request.server
                );
            }
            other => eprintln!("auth {peer}: unknown type {other:#04x} ignored"),
        }
    }
}

/// A connection's `ip:port` for logging, or `?` when the socket has no peer.
fn peer_name(stream: &TcpStream) -> String {
    stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "?".to_string())
}

/// The `ServerList` payload: one entry `[count, lastId, id][ip u32 LE]`
/// `[port u32 LE][flags, online u16 LE, 0x00FF u16, 0x01]`.
///
/// The server-list payload used by this implementation; there is no
/// `drf-proto` builder for it.
pub fn server_list_payload(game_ip: &str, game_port: u16) -> Vec<u8> {
    let ip = game_ip.parse::<Ipv4Addr>().unwrap_or(Ipv4Addr::LOCALHOST);
    let mut out = Vec::with_capacity(18);
    out.extend_from_slice(&[0x01, 0x00, 0x00]);
    out.extend_from_slice(&ip.octets());
    out.extend_from_slice(&(u32::from(game_port)).to_le_bytes());
    out.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0xFF, 0x00, 0x01]);
    out
}

/// Parse a `ServerList` payload back to `(count, ip, port)`; the inverse of
/// the parser for the fields it reads.
pub fn parse_server_list_payload(payload: &[u8]) -> Option<(u8, Ipv4Addr, u16)> {
    let count = *payload.first()?;
    let ip = Ipv4Addr::new(
        *payload.get(3)?,
        *payload.get(4)?,
        *payload.get(5)?,
        *payload.get(6)?,
    );
    let port = u32::from_le_bytes(payload.get(7..11)?.try_into().ok()?) as u16;
    Some((count, ip, port))
}

/// A one-time `PlayOk` key. The client does not validate it; a clock-and-
/// counter mix is enough to make it distinct per handshake.
fn next_key() -> u32 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    (nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)) as u32
}
