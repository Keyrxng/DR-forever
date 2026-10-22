// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end server test: auth handshake, character list, select and join.
//!
//! The server is started on ephemeral ports in a background thread. The test
//! is its own client: it frames the auth connection with `drf_proto::auth` and
//! the game connection with `drf_wire`'s [`Segment`].

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use drf_content::{
    derive_class_kit, derive_merchant_tabs, derive_npc_base_manipulator, derive_world_entities,
    fill_generated_tabs, Archive, Corpus, PlacedKind,
};
use drf_proto::auth::{
    decode_login, decrypt_packet, frame, login_block, LoginOk, TYPE_LOGIN, TYPE_LOGIN_OK,
    TYPE_SERVER_LIST, WELCOME,
};
use drf_proto::{
    build_merchant_component_record, derived_item_ids, npc_behavior_class_state,
    npc_manipulators_class_state, npc_modifiers_class_state, parse_assign_payload,
    placed_entity_class_state, select_payload, NPC_BEHAVIOR_CLASS, NPC_MANIPULATORS_CLASS,
    NPC_MODIFIERS_CLASS,
};
use drf_server::server::{parse_server_list_payload, Server, Shared};
use drf_server::store::{Appearance, BagItem, EquipmentItem, QuestState, Store};
use drf_server::world::World;
use drf_wire::{MessageAddress, Payload, Segment};

const ACCOUNT: &str = "s1player";
const NAME: &str = "Hero";
const CLASS: &str = "avatar.classes.WarlockFemale";

/// The joined Warlock's observed level-1 avatar HP **wire** word (x256),
/// `50539 = 0x0000C56B`. Every avatar-owned synch trailer carries this word;
/// the `0x02` create carries it too. `51200` (the legacy archetype base) is the
/// mismatch `EntitySynchInfo::Validate` rejects.
const AVATAR_HP_WIRE: [u8; 4] = [0x6B, 0xC5, 0x00, 0x00];
/// The synch trailer: `0x02` + [`AVATAR_HP_WIRE`].
const AVATAR_SYNCH: [u8; 5] = [0x02, 0x6B, 0xC5, 0x00, 0x00];
/// The synch trailer followed by the `0x06` stream terminator.
const AVATAR_SYNCH_END: [u8; 6] = [0x02, 0x6B, 0xC5, 0x00, 0x00, 0x06];

/// A self-cleaning directory under the system temp dir.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let mut path = std::env::temp_dir();
        path.push(format!("drf-server-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The avatar's `avatar.base.UnitBehavior` component id from a join batch.
///
/// A `0x32` record is `[0x32][owner u16][cid u16][0xFF][classname NUL]...`, so
/// the cid sits three bytes before the classname. Mover records are routed by
/// this component id, so a test that wants the avatar's move-ack must address
/// it here rather than using a literal.
fn joined_unit_behavior_cid(batch: &[u8]) -> u16 {
    let classname = b"avatar.base.UnitBehavior\x00";
    let at = batch
        .windows(classname.len())
        .position(|window| window == classname)
        .expect("the join ships the avatar UnitBehavior");
    u16::from_le_bytes(batch[at - 3..at - 1].try_into().unwrap())
}

fn connect(port: u16) -> TcpStream {
    let mut last = None;
    for _ in 0..50 {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .expect("set read timeout");
                return stream;
            }
            Err(error) => {
                last = Some(error);
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
    panic!("could not connect to port {port}: {last:?}");
}

/// Consume the plaintext `ProtocolVer` greeting the server sends on accept.
fn expect_welcome(stream: &mut TcpStream) {
    let mut welcome = [0u8; 3];
    stream.read_exact(&mut welcome).expect("auth welcome");
    assert_eq!(welcome, WELCOME);
}

fn read_auth_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut length = [0u8; 2];
    stream.read_exact(&mut length).expect("auth length");
    let total = u16::from_le_bytes(length) as usize;
    let mut body = vec![0u8; total - 2];
    stream.read_exact(&mut body).expect("auth body");
    let decrypted = decrypt_packet(&body).expect("decrypt auth body");
    (decrypted.mtype, decrypted.payload)
}

/// A framed game client over `drf-wire` segments.
struct GameClient {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl GameClient {
    fn new(port: u16) -> GameClient {
        let mut stream = connect(port);
        // The client opens with a raw `0x0A` hello and blocks for the ack.
        let mut hello = vec![0x0a, 0x01, 0x02, 0x03];
        hello.extend_from_slice(&8u32.to_le_bytes());
        hello.extend_from_slice(&[0u8; 8]);
        stream.write_all(&hello).expect("send hello");
        let mut ack = [0u8; 23];
        stream.read_exact(&mut ack).expect("connect-ack");
        GameClient {
            stream,
            buffer: Vec::new(),
        }
    }

    fn send(&mut self, channel: u8, mtype: u8, payload: &[u8]) {
        let mut inner = vec![channel, mtype];
        inner.extend_from_slice(payload);
        let segment = Segment::new(
            [0x01, 0x02, 0x03],
            Some(MessageAddress::new(0x01, 0x000F)),
            Vec::new(),
            Payload::zlib(&inner),
        );
        self.stream
            .write_all(&segment.encode().expect("encode segment"))
            .expect("write segment");
    }

    fn next_inner(&mut self) -> Option<Vec<u8>> {
        loop {
            match Segment::decode(&self.buffer) {
                Ok((segment, used)) => {
                    self.buffer.drain(..used);
                    return segment.payload.inflate().ok();
                }
                Err(_) => {
                    let mut chunk = [0u8; 8192];
                    let read = self.stream.read(&mut chunk).ok()?;
                    if read == 0 {
                        return None;
                    }
                    self.buffer.extend_from_slice(&chunk[..read]);
                }
            }
        }
    }

    fn read_until(&mut self, channel: u8, mtype: u8) -> Vec<u8> {
        self.read_until_collect(channel, mtype).0
    }

    /// Read until `[channel][mtype]`, returning the match and every frame
    /// skipped on the way (so a test can assert a frame was NOT sent).
    fn read_until_collect(&mut self, channel: u8, mtype: u8) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut skipped = Vec::new();
        loop {
            let inner = self.next_inner().expect("server closed the game port");
            if inner.len() >= 2 && inner[0] == channel && inner[1] == mtype {
                return (inner, skipped);
            }
            skipped.push(inner);
        }
    }

    /// Read exactly `n` more frames. The session's ticker emits one `(07,0D)`
    /// WorldInterval per ~133 ms, so a bounded read always makes progress; a
    /// closed socket is the only failure.
    fn read_frames(&mut self, n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|_| self.next_inner().expect("server closed the game port"))
            .collect()
    }
}

/// Read `[u16 eid][0xFF][classname NUL]` at `pos`; returns `(eid, class, next)`.
fn read_create(inner: &[u8], pos: usize) -> (u16, Vec<u8>, usize) {
    let eid = u16::from_le_bytes(inner[pos + 1..pos + 3].try_into().unwrap());
    assert_eq!(inner[pos + 3], 0xFF, "create must carry the classname tag");
    let end = inner[pos + 4..]
        .iter()
        .position(|&byte| byte == 0)
        .expect("classname NUL")
        + pos
        + 4;
    (eid, inner[pos + 4..end].to_vec(), end + 1)
}

#[test]
fn auth_list_select_and_join_reach_the_avatar() {
    let dir = TempDir::new("e2e");
    let store_path = dir.path().join("s1_store.json");
    let mut store = Store::open(&store_path).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");
    assert_eq!(sqlid, 1);

    // No content paths: the graph nodes are bare, which is what the test parses.
    let server =
        Server::new("127.0.0.1:0", "127.0.0.1:0", "127.0.0.1", store, None).expect("bind server");
    let auth_port = server.auth_port();
    let game_port = server.game_port();
    assert_ne!(game_port, 0, "game port is ephemeral, never zero");
    thread::spawn(move || server.serve());

    // -- auth: Login -> LoginOk + ServerList ---------------------------------
    let mut auth = connect(auth_port);
    expect_welcome(&mut auth);
    auth.write_all(&frame(
        TYPE_LOGIN,
        &login_block(ACCOUNT.as_bytes(), b"sekret"),
    ))
    .expect("send login");
    let (login_type, login_payload) = read_auth_frame(&mut auth);
    assert_eq!(login_type, TYPE_LOGIN_OK);
    let login_ok = LoginOk::decode(&login_payload).expect("decode LoginOk");
    assert_eq!(login_ok.flag, 1);
    let (list_type, list_payload) = read_auth_frame(&mut auth);
    assert_eq!(list_type, TYPE_SERVER_LIST);
    let (count, _ip, advertised) =
        parse_server_list_payload(&list_payload).expect("decode ServerList");
    assert_eq!(count, 1);
    assert_eq!(advertised, game_port, "ServerList advertises the game port");
    // The credential block round-trips to the login name.
    let (user, _pwlen) = decode_login(&login_block(ACCOUNT.as_bytes(), b"sekret")).unwrap();
    assert_eq!(user, ACCOUNT.as_bytes());

    // -- game: char list -----------------------------------------------------
    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x03, &[]);
    let list = game.read_until(0x04, 0x03);
    let payload = &list[2..];
    assert_eq!(payload[0], 1, "one seeded character");
    let row_sqlid = u32::from_le_bytes(payload[1..5].try_into().unwrap());
    assert_eq!(row_sqlid, sqlid);
    let node = &payload[5..];
    assert_eq!(node[0], 0x2D, "player graph node version");
    let node_id = u32::from_le_bytes(node[5..9].try_into().unwrap());
    assert_eq!(node_id, sqlid);
    let name_end = node[9..].iter().position(|&b| b == 0).unwrap() + 9;
    assert_eq!(&node[9..name_end], NAME.as_bytes());

    // -- game: select --------------------------------------------------------
    game.send(0x04, 0x05, &select_payload(sqlid));
    let ack = game.read_until(0x04, 0x05);
    assert_eq!(ack.len(), 2, "select ack success is a bare tag");
    // The assign that closes the play burst names the character's persisted
    // zone -- the tutorial (Dew Valley) for a fresh character.
    let assign = game.read_until(0x0D, 0x00);
    let parsed = parse_assign_payload(&assign[2..]).expect("parse assign");
    assert_eq!(parsed.zone, b"tutorial");

    // -- game: join (tutorial) ----------------------------------------------
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);
    assert_eq!(&batch[..2], &[0x07, 0x0D]);
    // Prologue: 2 tag bytes + 12 budget bytes + 8 PathManager/header bytes.
    let mut pos = 2 + 12 + 8;
    // A content zone ships no server-placed world entities yet, so the first
    // record is the player.
    assert_eq!(batch[pos], 0x08, "player create-init tag");
    let player_eid = u16::from_le_bytes(batch[pos + 1..pos + 3].try_into().unwrap());
    assert_eq!(batch[pos + 3], 0xFF);
    assert_eq!(&batch[pos + 4..pos + 11], b"player\0");
    // The body is `[name NUL][27B tail][nested QuestManager+DialogManager]`,
    // NOT a character-graph node.
    let name_start = pos + 11;
    let name_end = batch[name_start..].iter().position(|&b| b == 0).unwrap() + name_start;
    assert_eq!(&batch[name_start..name_end], NAME.as_bytes());
    let tail = name_end + 1;
    assert_eq!(&batch[tail..tail + 4], &[0, 0, 0, 0]);
    assert_eq!(&batch[tail + 7..tail + 9], &0x0180u16.to_le_bytes());
    assert_eq!(batch[tail + 23], 1, "minimum item quality");
    // The nested pair: QuestManager (61B) abutting DialogManager (21B).
    let managers = tail + 27;
    assert_eq!(batch[managers], 0x32, "nested QuestManager tag");
    assert_eq!(batch[managers + 61], 0x32, "nested DialogManager tag");
    pos = managers + 82;

    // The avatar is a PLACED entity: its bare `0x01` create with the stored
    // class, then its `0x02` class-state head, then six top-level `0x32`
    // components. This server has no game.pkg, so the head's pose words are
    // zeroed, the starting kit is empty and the inventory roster is empty.
    assert_eq!(batch[pos], 0x01, "avatar create tag");
    let avatar_eid = u16::from_le_bytes(batch[pos + 1..pos + 3].try_into().unwrap());
    let (_eid, avatar_class, next) = read_create(&batch, pos);
    assert_eq!(avatar_class, CLASS.as_bytes());
    assert_ne!(avatar_eid, player_eid);
    pos = next;

    let head = drf_proto::build_minimal_avatar_init_payload(
        avatar_eid,
        player_eid,
        CLASS.as_bytes(),
        None,
        Some(1),
        &[],
        drf_proto::MINIMAL_AVATAR_WE_FLAGS_ATTACH,
        Some(drf_proto::avatar_hp_wire(CLASS)),
        Some(drf_proto::MINIMAL_AVATAR_MP),
    )
    .expect("avatar head");
    assert_eq!(batch[pos], 0x02, "avatar init tag");
    assert_eq!(
        u16::from_le_bytes(batch[pos + 1..pos + 3].try_into().unwrap()),
        avatar_eid
    );
    assert_eq!(&batch[pos + 3..pos + 3 + head.len()], head.as_slice());
    // The `0x02` create's Unit HP word is the class's observed level-1 wire
    // word (head offset 29: 4 we_flags + 16 pose + 1 WE flag + 1 unit_flags +
    // 1 level + 4 u16 pair + 2 owner). It must NOT be the legacy 51200.
    assert_eq!(
        &batch[pos + 3 + 29..pos + 3 + 33],
        &AVATAR_HP_WIRE,
        "the avatar create carries the observed Warlock level-1 HP word"
    );
    pos += 3 + head.len();

    // The component ids are allocated after the player's QuestManager
    // (0xE000) and DialogManager (0xE001).
    let components = [
        drf_proto::minimal_avatar_skills_component(avatar_eid, 0xE002, 0).unwrap(),
        drf_proto::minimal_avatar_equipment_component(avatar_eid, 0xE003, None).unwrap(),
        drf_proto::minimal_avatar_unit_container_component_with_gold(avatar_eid, 0xE004, &[], 5000)
            .unwrap(),
        drf_proto::minimal_avatar_manipulators_component(avatar_eid, 0xE005, None, &[]).unwrap(),
        drf_proto::minimal_avatar_modifiers_component(avatar_eid, 0xE006).unwrap(),
        drf_proto::minimal_avatar_unit_behavior_component(avatar_eid, 0xE007, false).unwrap(),
    ];
    for (index, component) in components.iter().enumerate() {
        assert_eq!(batch[pos], 0x32, "avatar component {index} tag");
        assert_eq!(&batch[pos..pos + component.len()], component.as_slice());
        pos += component.len();
    }

    // The avatar's SpawnAction: `Spawn::update` fires the unit's spawn event,
    // which runs `Unit::onSpawned` and sets `Unit+0xA0 |= 0x400`. Mob aggro
    // depends on that bit (`UpdateTargets`' keep test), and the head above was
    // built with no pose, so the action carries a zero pose.
    let spawn = drf_proto::mob_spawn_action_payload(
        0xE007,
        [0, 0, 0, 0],
        avatar_eid,
        drf_proto::avatar_hp_wire(CLASS),
    );
    assert_eq!(batch[pos], 0x35, "avatar SpawnAction tag");
    pos += 1;
    assert_eq!(&batch[pos..pos + spawn.len()], spawn.as_slice());
    pos += spawn.len();

    // A hand-placed zone ships no start-room warp: the `0x800` gate is clear
    // (the head comparison above) and no `0x03 [eid][0x15]` record rides.
    let mut warp = vec![0x03u8];
    warp.extend_from_slice(&avatar_eid.to_le_bytes());
    warp.push(drf_proto::AVATAR_UPDATE_PLACE_AT_START);
    assert!(
        !batch
            .windows(warp.len())
            .any(|window| window == warp.as_slice()),
        "a hand-placed zone ships no 0x15 warp"
    );

    // The join batch closes on the sub-entities marker and CONNECTED.
    assert_eq!(batch[pos], 0x1E, "sub-entities marker");
    assert_eq!(batch[pos + 1], 0x46, "CONNECTED join terminator");

    // The client's `RequestClientControl` is answered with the `(07,35)`
    // FollowClient grant, and its synch trailer must carry the same avatar HP
    // word the create did -- the crash was this trailer disagreeing with the
    // client's `Unit+0x2F0`.
    let ub_cid = 0xE007u16;
    let mut control = ub_cid.to_le_bytes().to_vec();
    control.extend_from_slice(&[0x64, 0x01]);
    game.send(0x07, 0x34, &control);
    let grant = game.read_until(0x07, 0x35);
    let mut expected_grant = vec![0x07, 0x35];
    expected_grant.extend_from_slice(&ub_cid.to_le_bytes());
    expected_grant.extend_from_slice(&[0x64, 0x01, 0x02]);
    expected_grant.extend_from_slice(&AVATAR_HP_WIRE);
    expected_grant.push(0x06);
    assert_eq!(
        grant, expected_grant,
        "the FollowClient grant carries the avatar's observed HP word"
    );
}

/// `dungeon00_level01`'s join pose: the zone names its entry (`start1`,
/// `zone-start-room`) and no waypoint carries a coordinate, so the destination
/// join falls back to the `ZoneStart` room's grid cell -- derived from the
/// room's `GridY` and the world archetype's `TileSize`, never a constant.
/// The `tutorial`'s own `misc.Waypoint Start` still wins, and no coordinate is
/// reported for the dungeon by the waypoint chain.
#[test]
fn generated_dungeon_start_pose_is_derived_from_the_room_grid() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("dungeon-start");
    let store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    let shared = Shared::new(
        store,
        World::new(),
        "127.0.0.1".to_string(),
        0,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    );

    // The waypoint chain refuses: the generated world states no coordinate.
    assert!(shared
        .zone_spawn_pose_requested("dungeon00_level01", Some("Start1"))
        .is_none());
    // The maze port's entry: cell (0,4) origin plus the tile's authored
    // `misc.Waypoint SpawnPoint`, in x256.
    let start = shared
        .zone_generated_start("dungeon00_level01", Some("Start1"))
        .expect("generated start");
    assert_eq!(start.name, "start1");
    assert_eq!(start.rung, "zone-start-room");
    assert_eq!(
        start.pose,
        [(-650i64 * 256) as u32, 870 * 256, 10 * 256, 180 * 256]
    );

    // A hand-placed zone's own waypoint always wins over the generated path.
    assert!(shared
        .zone_generated_start("tutorial", Some("Start1"))
        .is_none());
    assert_eq!(
        shared.zone_spawn_pose_requested("tutorial", Some("Start1")),
        Some([960 * 256, 660 * 256, 40 * 256, 135 * 256])
    );
}

/// The `(04,03)` node carries the class kit when a real `game.pkg` is present;
/// skips when the client tree is absent.
#[test]
fn char_list_carries_the_class_kit_from_game_pkg() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };

    let temp = TempDir::new("content");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let auth_port = server.auth_port();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut auth = connect(auth_port);
    expect_welcome(&mut auth);
    auth.write_all(&frame(
        TYPE_LOGIN,
        &login_block(ACCOUNT.as_bytes(), b"sekret"),
    ))
    .expect("send login");
    read_auth_frame(&mut auth);

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x03, &[]);
    let list = game.read_until(0x04, 0x03);
    let payload = &list[2..];
    assert_eq!(payload[0], 1);
    let node = &payload[5..];
    // The Avatar node's archetype id is dfc_hash of the stored class.
    let class_hash = drf_content::dfc_hash(CLASS.as_bytes()).to_le_bytes();
    assert!(
        node.windows(4).any(|window| window == class_hash),
        "kitted graph carries dfc_hash({CLASS})"
    );
}

/// The tutorial join ships `Tutorial.world`'s placed entities -- a `0x01`
/// create followed by a `0x02` class-state -- before the player record.
#[test]
fn tutorial_join_ships_the_placed_world_entities() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("world-entities");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let auth_port = server.auth_port();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut auth = connect(auth_port);
    expect_welcome(&mut auth);
    auth.write_all(&frame(
        TYPE_LOGIN,
        &login_block(ACCOUNT.as_bytes(), b"sekret"),
    ))
    .expect("send login");
    read_auth_frame(&mut auth);

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    let archive = Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("archive");
    let expected = derive_world_entities(&archive, "tutorial").expect("world walk");
    assert!(!expected.is_empty(), "tutorial places entities");
    let mut corpus =
        Corpus::new(Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("archive"));
    let manipulator = derive_npc_base_manipulator(&mut corpus)
        .unwrap()
        .expect("npc/Base manipulator");

    // Prologue: 2 tag bytes + 12 budget bytes + 8 PathManager/header bytes.
    let mut pos = 2 + 12 + 8;
    for entity in &expected {
        assert_eq!(batch[pos], 0x01, "{} create tag", entity.classname);
        let (eid, classname, next) = read_create(&batch, pos);
        assert_eq!(classname, entity.classname.as_bytes());
        pos = next;

        let payload = placed_entity_class_state(entity).expect("class state");
        assert_eq!(batch[pos], 0x02, "{} init tag", entity.classname);
        let init_eid = u16::from_le_bytes(batch[pos + 1..pos + 3].try_into().unwrap());
        assert_eq!(init_eid, eid);
        pos += 3;
        assert_eq!(
            &batch[pos..pos + payload.len()],
            payload.as_slice(),
            "{} class state",
            entity.classname
        );
        pos += payload.len();

        // An NPC ships three nested components after its `0x02`; the
        // checkpoint and portal ship none.
        if entity.kind != PlacedKind::Npc {
            continue;
        }
        let states = [
            (
                NPC_MANIPULATORS_CLASS,
                npc_manipulators_class_state(&manipulator.fqn, manipulator.id),
            ),
            (
                NPC_BEHAVIOR_CLASS,
                npc_behavior_class_state(entity.pose).unwrap(),
            ),
            (NPC_MODIFIERS_CLASS, npc_modifiers_class_state()),
        ];
        for (class, state) in &states {
            assert_eq!(batch[pos], 0x32, "{} component tag", entity.classname);
            assert_eq!(
                u16::from_le_bytes(batch[pos + 1..pos + 3].try_into().unwrap()),
                eid
            );
            assert_eq!(batch[pos + 5], 0xFF);
            let nul = batch[pos + 6..].iter().position(|&b| b == 0).unwrap() + pos + 6;
            assert_eq!(&batch[pos + 6..nul], class.as_bytes());
            let body = nul + 1;
            assert_eq!(&batch[body..body + state.len()], state.as_slice());
            pos = body + state.len();
        }

        // A vendor's DERIVED `*.Merchant` component follows its three.
        let merchant_tabs = derive_merchant_tabs(&mut corpus, &entity.classname)
            .expect("derive merchant tabs")
            .filter(|tabs| !tabs.is_empty());
        if let Some(mut tabs) = merchant_tabs {
            fill_generated_tabs(&mut corpus, &entity.classname, &mut tabs);
            assert_eq!(
                batch[pos], 0x32,
                "{} merchant component tag",
                entity.classname
            );
            assert_eq!(
                u16::from_le_bytes(batch[pos + 1..pos + 3].try_into().unwrap()),
                eid
            );
            assert_eq!(batch[pos + 5], 0xFF);
            let nul = batch[pos + 6..].iter().position(|&b| b == 0).unwrap() + pos + 6;
            let want = format!("{}.Merchant", entity.classname);
            assert_eq!(&batch[pos + 6..nul], want.as_bytes());
            let merchant_cid = u16::from_le_bytes(batch[pos + 3..pos + 5].try_into().unwrap());
            // The first vendor in this join mints from `first_item_id = 1`.
            let expected =
                build_merchant_component_record(eid, merchant_cid, &entity.classname, &tabs, 1);
            assert_eq!(
                &batch[pos..pos + expected.len()],
                expected.as_slice(),
                "{} merchant record",
                entity.classname
            );
            pos += expected.len();
        }
    }

    // The player create-init follows the placed entities.
    assert_eq!(batch[pos], 0x08, "player create-init after the placed set");
}

/// The tutorial join ships the stored class's starting skills: the
/// `avatar.base.Skills` component carries the four `(fqn, id, level)` entries
/// and the profession, and the `Manipulators` component ends with the
/// ActiveSkill tail.
#[test]
fn tutorial_join_ships_the_class_skills() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("class-skills");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let auth_port = server.auth_port();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut auth = connect(auth_port);
    expect_welcome(&mut auth);
    auth.write_all(&frame(
        TYPE_LOGIN,
        &login_block(ACCOUNT.as_bytes(), b"sekret"),
    ))
    .expect("send login");
    read_auth_frame(&mut auth);

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    let mut corpus =
        Corpus::new(Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("archive"));
    let kit = derive_class_kit(&mut corpus, "Warlock").expect("warlock kit");
    let wire = kit.skill_wire().expect("skill wire");

    // `avatar.base.Skills`: `[FF][classname NUL][01][u32 points][count]...`.
    let skills_class = b"\xffavatar.base.Skills\x00";
    let skills_at = batch
        .windows(skills_class.len())
        .position(|window| window == skills_class)
        .expect("the join ships the avatar avatar.base.Skills");
    let skills_body = skills_at + skills_class.len();
    assert_eq!(batch[skills_body], 0x01, "Skills post byte");
    let mut expected_skills = wire.skills.clone();
    expected_skills.push((drf_content::BLING_GNOME_SKILL.to_string(), 200, 1));
    let expected = drf_proto::build_avatar_skills_payload(0, &expected_skills, &wire.professions);
    assert_eq!(expected[1..5], 0u32.to_le_bytes(), "points ride 0");
    assert_eq!(
        &batch[skills_body..skills_body + expected.len()],
        expected.as_slice(),
        "the Skills component carries the Warlock kit and its profession"
    );

    // `Manipulators`: the payload ends with the ActiveSkill tail, and its count
    // byte is the worn items plus those tail entries.
    let manip_class = b"\xffManipulators\x00";
    let manip_at = batch
        .windows(manip_class.len())
        .position(|window| window == manip_class)
        .expect("the join ships the avatar Manipulators");
    let manip_body = manip_at + manip_class.len();
    assert_eq!(batch[manip_body], 0x01, "Manipulators post byte");
    let next_class = b"\xffModifiers\x00";
    let next_at = batch[manip_body..]
        .windows(next_class.len())
        .position(|window| window == next_class)
        .expect("Modifiers follows Manipulators")
        + manip_body;
    let next_record = next_at - 5;
    let mut manipulators = wire.manipulators.clone();
    manipulators.push((drf_content::BLING_GNOME_SKILL.to_string(), 200, 1));
    let mut tail = Vec::new();
    for (fqn, id, level) in &manipulators {
        tail.push(0xFF);
        tail.extend_from_slice(fqn.as_bytes());
        tail.push(0);
        tail.extend_from_slice(&id.to_le_bytes());
        tail.push(*level);
        tail.push(0);
    }
    assert!(!tail.is_empty(), "Warlock has ActiveSkill manipulators");
    assert_eq!(
        batch[manip_body + 1] as usize,
        kit.items.len() + manipulators.len(),
        "items plus ActiveSkill tail"
    );
    assert_eq!(
        &batch[next_record - tail.len()..next_record],
        tail.as_slice(),
        "the Manipulators component ends with the ActiveSkill tail"
    );
}

/// Clicking the one tutorial quest giver (`HelperNoobosaur00`) is answered
/// with the 1:1 `(07,35)` echo and the 11B SetOfferedQuest; the accept that
/// follows is answered with an AddQuest carrying the quest's own objectives.
#[test]
fn clicking_the_tutorial_giver_offers_and_accepts_its_quest() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("quest-click");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let auth_port = server.auth_port();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut auth = connect(auth_port);
    expect_welcome(&mut auth);
    auth.write_all(&frame(
        TYPE_LOGIN,
        &login_block(ACCOUNT.as_bytes(), b"sekret"),
    ))
    .expect("send login");
    read_auth_frame(&mut auth);

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    // The component ids are allocated after the NPC component run: three per
    // NPC plus one `*.Merchant` for every vendor that authors a non-empty
    // block, then the player's QuestManager/DialogManager, then the avatar's
    // six.
    let entities = derive_world_entities(
        &Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("archive"),
        "tutorial",
    )
    .expect("world walk");
    let mut corpus =
        Corpus::new(Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("archive"));
    let npc_count = entities
        .iter()
        .filter(|entity| entity.kind == PlacedKind::Npc)
        .count();
    let merchant_count = entities
        .iter()
        .filter(|entity| entity.kind == PlacedKind::Npc)
        .filter(|entity| {
            derive_merchant_tabs(&mut corpus, &entity.classname)
                .map(|tabs| tabs.is_some_and(|tabs| !tabs.is_empty()))
                .unwrap_or(false)
        })
        .count();
    let base = 0xE000u16 + (npc_count as u16) * 3 + merchant_count as u16;
    let quest_cid = base;
    let ub_cid = base + 7;

    // The giver's entity id: its `0x01` create is `01 <eid> FF <classname> 00`.
    let helper = b"world.tutorial.npc.HelperNoobosaur00\x00";
    let start = batch
        .windows(helper.len())
        .position(|window| window == helper)
        .expect("HelperNoobosaur00 is placed in the tutorial");
    let helper_eid = u16::from_le_bytes(batch[start - 3..start - 1].try_into().unwrap());
    assert_eq!(batch[start - 4], 0x01, "create tag precedes the classname");
    assert_eq!(
        batch[start - 1],
        0xFF,
        "classname tag precedes the classname"
    );

    // The QuestManager create carries the offer directory, so the giver
    // classname appears a second time (its `0x01` create is the first).
    assert!(
        batch.windows(helper.len()).filter(|w| *w == helper).count() >= 2,
        "the offer directory names the giver in the QuestManager create"
    );

    // -- the Interact-06 click ----------------------------------------------
    let mut click = ub_cid.to_le_bytes().to_vec();
    click.extend_from_slice(&[0x01, 0x01, 0x06, 0x01]);
    click.extend_from_slice(&helper_eid.to_le_bytes());
    assert_eq!(click.len(), 8, "click body is 8B (10B inner)");
    game.send(0x07, 0x34, &click);

    // The shared activate ack: the c2s body 1:1, then the avatar's synch
    // trailer `02` + the x256 HP word 51200 (`00 c8 00 00`) and the `06` end.
    let echo = game.read_until(0x07, 0x35);
    let mut expected_echo = vec![0x07, 0x35];
    expected_echo.extend_from_slice(&click);
    expected_echo.extend_from_slice(&AVATAR_SYNCH_END);
    assert_eq!(echo, expected_echo, "1:1 click echo");

    // The offer: SetOfferedQuest for Q11_a1 (qid 0xFBBED69D).
    let offer = game.read_until(0x07, 0x35);
    let mut expected_offer = vec![0x07, 0x35];
    expected_offer.extend_from_slice(&quest_cid.to_le_bytes());
    expected_offer.extend_from_slice(&[0x04, 0x04, 0x9D, 0xD6, 0xBE, 0xFB, 0x00, 0x06]);
    assert_eq!(offer, expected_offer, "SetOfferedQuest for Q11_a1");

    // -- the accept ---------------------------------------------------------
    let mut accept = quest_cid.to_le_bytes().to_vec();
    accept.push(0x01);
    game.send(0x07, 0x34, &accept);

    let add = game.read_until(0x07, 0x35);
    assert_eq!(&add[..2], &[0x07, 0x35], "AddQuest tag");
    assert_eq!(
        u16::from_le_bytes(add[2..4].try_into().unwrap()),
        quest_cid,
        "AddQuest is addressed to the QuestManager"
    );
    assert_eq!(add[4], 0x01, "AddQuest sub-opcode");
    assert_eq!(&add[5..10], &[0x04, 0x9D, 0xD6, 0xBE, 0xFB], "quest id");
    let count = add[15];
    assert!(count >= 1, "an AddQuest never carries zero objectives");
    // The objective label is the quest's own authored text with its count.
    assert!(
        add.windows(b"Ticket #54,000,601: 0 / 1".len())
            .any(|window| window == b"Ticket #54,000,601: 0 / 1"),
        "the authored objective label is on the wire"
    );
    assert_eq!(add[add.len() - 1], 0x06, "AddQuest END marker");

    // The on-accept grant follows the AddQuest and precedes the directory
    // update: the quest's own `OnAcceptItemGenerator` item, delivered as a
    // UnitContainer `0x35` add stream with the avatar's synch trailer.
    let grant = game.read_until(0x07, 0x35);
    assert_eq!(&grant[..2], &[0x07, 0x35], "grant stream tag");
    let container_cid = base + 4;
    assert_eq!(
        u16::from_le_bytes(grant[2..4].try_into().unwrap()),
        container_cid,
        "grant is addressed to the avatar UnitContainer"
    );
    assert_eq!(grant[4], 0x1E, "grant sub-opcode (item add)");
    assert_eq!(grant[5], 0x0B, "grant names the bag inventory child");
    assert_eq!(grant[6], 0xFF, "the item record opens with its type tag");
    let nul = grant[6..].iter().position(|&b| b == 0).unwrap() + 6;
    assert_eq!(
        &grant[7..nul],
        b"QuestItemPAL2.D00_Q11_a1_01",
        "the granted item is the quest's own class"
    );
    // The flags byte is the ninth of the fixed head; Q11_a1 marks its item
    // soulbound AND no-sell, so the wire byte is 0x03.
    assert_eq!(grant[nul + 9], 0x03, "per-instance flags are patched in");
    assert_eq!(grant[grant.len() - 1], 0x06, "grant stream EndStream");
    let instance = u32::from_le_bytes(add[10..14].try_into().unwrap());

    // The objective restate follows. Kind 1 replaces the cached objective
    // strings (the visible count), then kind 0 sets all-complete so the client
    // repaints its marker.
    let labels = game.read_until(0x07, 0x35);
    assert_eq!(&labels[..2], &[0x07, 0x35]);
    assert_eq!(
        u16::from_le_bytes(labels[2..4].try_into().unwrap()),
        quest_cid
    );
    assert_eq!(labels[4], 0x03, "UpdateQuest sub-opcode");
    assert_eq!(
        u32::from_le_bytes(labels[5..9].try_into().unwrap()),
        instance
    );
    assert_eq!(labels[9], 0x01, "UpdateQuest kind readObjectives");
    assert_eq!(labels[10], 1, "one objective");
    assert!(
        labels
            .windows(b"Ticket #54,000,601: 1 / 1".len())
            .any(|window| window == b"Ticket #54,000,601: 1 / 1"),
        "the restated label carries the bag count"
    );

    let objective = game.read_until(0x07, 0x35);
    let mut expected_objective = vec![0x07, 0x35];
    expected_objective.extend_from_slice(&quest_cid.to_le_bytes());
    expected_objective.push(0x03);
    expected_objective.extend_from_slice(&instance.to_le_bytes());
    expected_objective.extend_from_slice(&[0x00, 0x01, 0x00, 0x06]);
    assert_eq!(objective, expected_objective, "objective restate complete");

    // The AvailableQuestUpdate follows: the directory with the accepted quest
    // dropped, so its giver loses the marker. With no quest left it is empty.
    let update = game.read_until(0x07, 0x35);
    let mut expected_update = vec![0x07, 0x35];
    expected_update.extend_from_slice(&quest_cid.to_le_bytes());
    expected_update.extend_from_slice(&[0x07, 0x00, 0x00, 0x06]);
    assert_eq!(update, expected_update, "AvailableQuestUpdate withdraws it");

    // -- the turn-in: query -> ShowTurnInDialog, complete -> finalize+remove --
    let mut turnin_query = quest_cid.to_le_bytes().to_vec();
    turnin_query.push(0x04);
    turnin_query.extend_from_slice(&instance.to_le_bytes());
    game.send(0x07, 0x34, &turnin_query);
    let ready = game.read_until(0x07, 0x35);
    let mut expected_ready = vec![0x07, 0x35];
    expected_ready.extend_from_slice(&quest_cid.to_le_bytes());
    expected_ready.push(0x06);
    expected_ready.extend_from_slice(&instance.to_le_bytes());
    expected_ready.extend_from_slice(&[0x00, 0x06]);
    assert_eq!(ready, expected_ready, "ShowTurnInDialog");

    let mut turnin_complete = quest_cid.to_le_bytes().to_vec();
    turnin_complete.push(0x05);
    turnin_complete.extend_from_slice(&instance.to_le_bytes());
    turnin_complete.push(0x00);
    game.send(0x07, 0x34, &turnin_complete);
    let finalize = game.read_until(0x07, 0x35);
    let mut expected_finalize = vec![0x07, 0x35];
    expected_finalize.extend_from_slice(&quest_cid.to_le_bytes());
    expected_finalize.push(0x08);
    expected_finalize.extend_from_slice(&instance.to_le_bytes());
    expected_finalize.extend_from_slice(&[0x00, 0x06]);
    assert_eq!(finalize, expected_finalize, "FinalizeQuest");
    let remove = game.read_until(0x07, 0x35);
    let mut expected_remove = vec![0x07, 0x35];
    expected_remove.extend_from_slice(&quest_cid.to_le_bytes());
    expected_remove.push(0x02);
    expected_remove.extend_from_slice(&instance.to_le_bytes());
    expected_remove.extend_from_slice(&[0x00, 0x06]);
    assert_eq!(remove, expected_remove, "RemoveQuest");

    // The inherited `quests.base.Quest.CashReward = 0.5` is paid through the
    // same UnitContainer `0x20` the merchant uses.
    let cash = game.read_until(0x07, 0x35);
    assert_eq!(&cash[..2], &[0x07, 0x35], "quest cash tag");
    assert_eq!(
        u16::from_le_bytes(cash[2..4].try_into().unwrap()),
        container_cid,
        "quest cash is addressed to the UnitContainer"
    );
    assert_eq!(cash[4], 0x20, "quest cash sub-opcode");
    assert_eq!(
        i32::from_le_bytes(cash[5..9].try_into().unwrap()),
        125,
        "level-1 Quest base CashReward 0.5 at 250 gold/level"
    );

    // The turn-in consumes Q11_a1's help ticket (`RemoveOnFinalize`), so a
    // UnitContainer removal stream precedes the republished directory.
    let consumed = game.read_until(0x07, 0x35);
    assert_eq!(&consumed[..2], &[0x07, 0x35], "ticket removal tag");
    assert_eq!(
        u16::from_le_bytes(consumed[2..4].try_into().unwrap()),
        container_cid,
        "ticket removal is addressed to the UnitContainer"
    );
    assert_eq!(consumed[4], 0x1F, "ticket removal sub-opcode");

    // The turn-in republishes the directory, which now GROWS: completing
    // Q11_a1 unlocks the follow-ups it gated.
    let republished = game.read_until(0x07, 0x35);
    assert_eq!(&republished[..2], &[0x07, 0x35]);
    assert_eq!(
        u16::from_le_bytes(republished[2..4].try_into().unwrap()),
        quest_cid
    );
    assert_eq!(republished[4], 0x07, "AvailableQuestUpdate after turn-in");
    assert!(
        republished.len() > 7,
        "the unlocked follow-ups are now offered"
    );

    // Re-clicking the giver is answered with the echo alone -- the quest is
    // finished and unrepeatable, so no second SetOfferedQuest follows.
    game.send(0x07, 0x34, &click);
    let echo_again = game.read_until(0x07, 0x35);
    assert_eq!(echo_again, expected_echo, "re-click echoes, no re-offer");
}

/// Buying the tutorial HermitVendor's book (`item_id` 9) is answered with one
/// `(07,35)` stream carrying the currency debit and the item add, and no
/// stock-remove frame.
#[test]
fn buying_the_tutorial_book_charges_and_adds_it() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("merchant-buy");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let auth_port = server.auth_port();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut auth = connect(auth_port);
    expect_welcome(&mut auth);
    auth.write_all(&frame(
        TYPE_LOGIN,
        &login_block(ACCOUNT.as_bytes(), b"sekret"),
    ))
    .expect("send login");
    read_auth_frame(&mut auth);

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    // The vendor's derived `*.Merchant` component: `[0x32][owner][cid][0xFF]
    // [classname]`, so the cid is the u16 before the type tag.
    let merchant_class = b"world.tutorial.npc.HermitVendor.Merchant\x00";
    let merchant_at = batch
        .windows(merchant_class.len())
        .position(|window| window == merchant_class)
        .expect("the tutorial join ships HermitVendor's Merchant");
    assert_eq!(batch[merchant_at - 1], 0xFF, "classname tag");
    let merchant_cid =
        u16::from_le_bytes(batch[merchant_at - 3..merchant_at - 1].try_into().unwrap());

    // The avatar's UnitContainer component, which is where gold and the bag
    // live; its cid is the u16 before its type tag.
    let container_class = b"UnitContainer\x00";
    let container_at = batch
        .windows(container_class.len())
        .position(|window| window == container_class)
        .expect("the join ships the avatar UnitContainer");
    assert_eq!(batch[container_at - 1], 0xFF, "classname tag");
    let container_cid = u16::from_le_bytes(
        batch[container_at - 3..container_at - 1]
            .try_into()
            .unwrap(),
    );

    // The avatar entity id, from its `01 <eid> FF <class>` create.
    let avatar_at = batch
        .windows(CLASS.len())
        .position(|window| window == CLASS.as_bytes())
        .expect("the avatar create names the stored class");
    assert_eq!(batch[avatar_at - 1], 0xFF);
    assert_eq!(batch[avatar_at - 4], 0x01, "avatar create tag");
    let avatar_eid = u16::from_le_bytes(batch[avatar_at - 3..avatar_at - 1].try_into().unwrap());

    // `07 34 | cid | 0x1E | buyer | item_id`; resolve the QuestItems tab's
    // single placement from the same generated shelves the join shipped.
    let archive = Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("archive");
    let mut corpus = Corpus::new(archive);
    let mut tabs = derive_merchant_tabs(&mut corpus, "world.tutorial.npc.HermitVendor")
        .expect("derive merchant tabs")
        .expect("HermitVendor authors a Merchant block");
    fill_generated_tabs(&mut corpus, "world.tutorial.npc.HermitVendor", &mut tabs);
    let (item_ids, _) = derived_item_ids(&tabs, 1);
    let quest_tab = tabs
        .iter()
        .find(|tab| tab.name == "QuestItems")
        .expect("QuestItems tab");
    let book_id = item_ids[&(quest_tab.index, quest_tab.items[0].index)];
    let mut buy = merchant_cid.to_le_bytes().to_vec();
    buy.push(0x1E);
    buy.extend_from_slice(&avatar_eid.to_le_bytes());
    buy.extend_from_slice(&book_id.to_le_bytes());
    assert_eq!(buy.len(), 9, "the buy payload is nine bytes");
    game.send(0x07, 0x34, &buy);

    let stream = game.read_until(0x07, 0x35);
    assert_eq!(&stream[..2], &[0x07, 0x35], "one component-update stream");
    // Update 1: the currency debit on the avatar's UnitContainer.
    assert_eq!(
        stream[2..4],
        container_cid.to_le_bytes(),
        "currency addressee"
    );
    assert_eq!(stream[4], 0x20, "AddCurrency sub-opcode");
    assert_eq!(
        i32::from_le_bytes(stream[5..9].try_into().unwrap()),
        -1,
        "the book prices at 1 gold"
    );
    assert_eq!(stream[9], 0x00, "unmeasured source bucket");
    assert_eq!(
        u32::from_le_bytes(stream[10..14].try_into().unwrap()),
        u32::from(avatar_eid),
        "the notify key is the avatar the floating gold anchors on"
    );
    assert_eq!(stream[14], 0x01, "notify flag");
    // The synch trailer is `0x02` + the x256 HP word, and that word must equal
    // the one the avatar create shipped (51200), never a second x256 scale.
    assert_eq!(&stream[15..20], &AVATAR_SYNCH, "synch");
    // Update 2: the item add.
    assert_eq!(stream[20], 0x35, "second update opens");
    assert_eq!(
        stream[21..23],
        container_cid.to_le_bytes(),
        "item addressee"
    );
    assert_eq!(stream[23], 0x1E, "item add sub-opcode");
    assert_eq!(stream[24], 0x0B, "bag inventory child");
    assert_eq!(stream[25], 0xFF, "item record type tag");
    let nul = stream[26..].iter().position(|&b| b == 0).unwrap() + 26;
    assert_eq!(
        &stream[26..nul],
        b"QuestItemPAL2.D00_Q11_a2_01",
        "the book's own class"
    );
    assert_eq!(stream[nul + 7], 1, "Item+0x82 stack count");
    assert_eq!(stream[nul + 8], 0, "the stock record's level byte");
    assert_eq!(*stream.last().unwrap(), 0x06, "EndStream");

    // No stock-remove frame: nothing on the merchant carries 0x1F. The world
    // ticker keeps sending intervals, so scan for a short window rather than
    // expecting silence.
    game.stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("short timeout");
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        let Some(inner) = game.next_inner() else {
            break;
        };
        if inner.len() >= 5 && inner[0] == 0x07 && inner[1] == 0x35 {
            let cid = u16::from_le_bytes(inner[2..4].try_into().unwrap());
            assert!(
                !(cid == merchant_cid && inner[4] == 0x1F),
                "a static tab sends no stock-remove frame"
            );
        }
    }
}

/// A re-login replays the persisted state: a `turned-in` quest is not
/// re-offered, and a persisted bag row plus the purse are shipped.
#[test]
fn join_replays_turned_in_gate_and_reships_the_bag() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("persist-join");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");
    // Q11_a1 was turned in on a previous session; the character holds its
    // on-accept item and a purse of 4321.
    store
        .quest_set_state(ACCOUNT, sqlid, 0xFBBE_D69D, QuestState::TurnedIn)
        .expect("persist turned-in quest");
    store
        .set_bag(
            ACCOUNT,
            sqlid,
            vec![BagItem {
                class: "QuestItemPAL2.D00_Q11_a1_01".to_string(),
                cell_x: 4,
                cell_y: 3,
                count: 1,
                level: None,
                flags: None,
                modifiers: Vec::new(),
            }],
        )
        .expect("persist bag");
    store.set_gold(ACCOUNT, sqlid, 4321).expect("persist gold");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let auth_port = server.auth_port();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut auth = connect(auth_port);
    expect_welcome(&mut auth);
    auth.write_all(&frame(
        TYPE_LOGIN,
        &login_block(ACCOUNT.as_bytes(), b"sekret"),
    ))
    .expect("send login");
    read_auth_frame(&mut auth);

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    // The persisted purse is in the UnitContainer the join shipped.
    let container_class = b"UnitContainer\x00";
    let container_at = batch
        .windows(container_class.len())
        .position(|window| window == container_class)
        .expect("the join ships the avatar UnitContainer");
    let gold = u32::from_le_bytes(
        batch[container_at + 19..container_at + 23]
            .try_into()
            .unwrap(),
    );
    assert_eq!(gold, 4321, "the persisted purse is shipped");

    // The turn-in of a held quest was not replayed, so the journal is empty
    // and the bag reship is the first `(07,35)` the join sends.
    let stream = game.read_until(0x07, 0x35);
    assert_eq!(&stream[..2], &[0x07, 0x35], "bag reship stream tag");
    assert_eq!(stream[4], 0x1E, "bag reship is an item add");
    assert_eq!(stream[5], 0x0B, "bag add names the bag child");
    assert_eq!(stream[6], 0xFF, "the item record opens with its type tag");
    let nul = stream[6..].iter().position(|&b| b == 0).unwrap() + 6;
    assert_eq!(
        &stream[7..nul],
        b"QuestItemPAL2.D00_Q11_a1_01",
        "the persisted class is reshipped"
    );
    let item_off = nul + 1;
    assert_eq!(
        (
            stream[item_off + 4],
            stream[item_off + 5],
            stream[item_off + 6]
        ),
        (4, 3, 1),
        "the persisted cell and count are reshipped"
    );

    // Clicking HelperNoobosaur00 echoes but offers nothing: Q11_a1 is
    // turned in and unrepeatable, so the completed gate excludes it.
    let entities = derive_world_entities(
        &Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("archive"),
        "tutorial",
    )
    .expect("world walk");
    let mut corpus =
        Corpus::new(Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("archive"));
    let npc_count = entities
        .iter()
        .filter(|entity| entity.kind == PlacedKind::Npc)
        .count();
    let merchant_count = entities
        .iter()
        .filter(|entity| entity.kind == PlacedKind::Npc)
        .filter(|entity| {
            derive_merchant_tabs(&mut corpus, &entity.classname)
                .map(|tabs| tabs.is_some_and(|tabs| !tabs.is_empty()))
                .unwrap_or(false)
        })
        .count();
    let ub_cid = 0xE000u16 + (npc_count as u16) * 3 + merchant_count as u16 + 7;
    let helper = b"world.tutorial.npc.HelperNoobosaur00\x00";
    let start = batch
        .windows(helper.len())
        .position(|window| window == helper)
        .expect("HelperNoobosaur00 is placed in the tutorial");
    let helper_eid = u16::from_le_bytes(batch[start - 3..start - 1].try_into().unwrap());

    let mut click = ub_cid.to_le_bytes().to_vec();
    click.extend_from_slice(&[0x01, 0x01, 0x06, 0x01]);
    click.extend_from_slice(&helper_eid.to_le_bytes());
    game.send(0x07, 0x34, &click);

    let echo = game.read_until(0x07, 0x35);
    let mut expected_echo = vec![0x07, 0x35];
    expected_echo.extend_from_slice(&click);
    expected_echo.extend_from_slice(&AVATAR_SYNCH_END);
    assert_eq!(echo, expected_echo, "1:1 click echo");

    // No SetOfferedQuest follows: the finished quest is not re-offered.
    game.stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("short timeout");
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        let Some(inner) = game.next_inner() else {
            break;
        };
        assert!(
            !(inner.len() >= 5 && inner[0] == 0x07 && inner[1] == 0x35),
            "a turned-in quest is not re-offered"
        );
    }
}

/// Clicking the tutorial `misc.ZonePortal_agg` answers with the 1:1 echo and
/// the travel sequence: a `(13,2)` transfer and a `(13,0)` assign naming the
/// portal's own destination, as TWO separate frames. The client's own
/// `(13,6)` then joins `dungeon00_level01` with a fresh, empty world.
#[test]
fn clicking_the_tutorial_portal_transfers_to_the_dungeon() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("zone-transfer");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let auth_port = server.auth_port();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut auth = connect(auth_port);
    expect_welcome(&mut auth);
    auth.write_all(&frame(
        TYPE_LOGIN,
        &login_block(ACCOUNT.as_bytes(), b"sekret"),
    ))
    .expect("send login");
    read_auth_frame(&mut auth);

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    // The component ids are allocated after the NPC component run, exactly as
    // the quest-click test computes them.
    let entities = derive_world_entities(
        &Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("archive"),
        "tutorial",
    )
    .expect("world walk");
    let mut corpus =
        Corpus::new(Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("archive"));
    let npc_count = entities
        .iter()
        .filter(|entity| entity.kind == PlacedKind::Npc)
        .count();
    let merchant_count = entities
        .iter()
        .filter(|entity| entity.kind == PlacedKind::Npc)
        .filter(|entity| {
            derive_merchant_tabs(&mut corpus, &entity.classname)
                .map(|tabs| tabs.is_some_and(|tabs| !tabs.is_empty()))
                .unwrap_or(false)
        })
        .count();
    let ub_cid = 0xE000u16 + (npc_count as u16) * 3 + merchant_count as u16 + 7;

    // The portal's entity id, from its `01 <eid> FF <classname> 00` create.
    let portal = b"misc.ZonePortal_agg\x00";
    let start = batch
        .windows(portal.len())
        .position(|window| window == portal)
        .expect("the tutorial places misc.ZonePortal_agg");
    assert_eq!(batch[start - 4], 0x01, "create tag precedes the classname");
    assert_eq!(
        batch[start - 1],
        0xFF,
        "classname tag precedes the classname"
    );
    let portal_eid = u16::from_le_bytes(batch[start - 3..start - 1].try_into().unwrap());

    // -- the portal click ---------------------------------------------------
    let mut click = ub_cid.to_le_bytes().to_vec();
    click.extend_from_slice(&[0x01, 0x01, 0x06, 0x01]);
    click.extend_from_slice(&portal_eid.to_le_bytes());
    assert_eq!(click.len(), 8, "click body is 8B (10B inner)");
    game.send(0x07, 0x34, &click);

    // The 1:1 echo goes out first.
    let echo = game.read_until(0x07, 0x35);
    assert_eq!(&echo[..2], &[0x07, 0x35]);
    assert_eq!(
        &echo[2..2 + click.len()],
        click.as_slice(),
        "1:1 click echo"
    );

    // (13,2) transfer: `0d02 "transfer\0"`.
    let transfer = game.read_until(0x0D, 0x02);
    assert_eq!(
        transfer,
        vec![0x0D, 0x02, b't', b'r', b'a', b'n', b's', b'f', b'e', b'r', 0x00],
        "the transfer record is the reason string, NUL-terminated"
    );

    // (13,0) assign: the portal's own destination, in the empty town shape.
    let assign = game.read_until(0x0D, 0x00);
    assert_eq!(
        assign,
        vec![
            0x0D, 0x00, // channel, type
            b'd', b'u', b'n', b'g', b'e', b'o', b'n', b'0', b'0', b'_', b'l', b'e', b'v', b'e',
            b'l', b'0', b'1', 0x00, // zone NUL
            0x2D, 0xBD, 0x0C, 0x0A, // u32 seed = layout_seed("dungeon00_level01")
            0x00, // u8 GC-type count
            0x00, 0x00, 0x00, 0x00, // u32 trailer
        ],
        "the assign names dungeon00_level01 with the maze seed"
    );
    let parsed = parse_assign_payload(&assign[2..]).expect("parse assign");
    assert_eq!(parsed.zone, b"dungeon00_level01");
    assert_eq!(
        parsed.seed,
        drf_content::maze::layout_seed("dungeon00_level01")
    );

    // -- the client's own (13,6) joins the destination ----------------------
    game.send(0x0D, 0x06, &[]);
    let dest_batch = game.read_until(0x07, 0x0D);
    assert_eq!(&dest_batch[..2], &[0x07, 0x0D]);
    // The generated destination places no `Entities` block, so the only world
    // entity it ships is the synthesized return portal: a `0x01` create whose
    // classname is `misc.ZonePortal_hub`. It leads the join, before the
    // re-created player.
    let pos = 2 + 12 + 8;
    assert_eq!(
        dest_batch[pos], 0x01,
        "the return portal create leads the join"
    );
    let return_portal = b"misc.ZonePortal_hub\x00";
    assert!(
        dest_batch
            .windows(return_portal.len())
            .any(|window| window == return_portal),
        "the generated destination ships its synthesized return portal"
    );
    let mut tutorial_portal_create = vec![0x01];
    tutorial_portal_create.extend_from_slice(&portal_eid.to_le_bytes());
    assert!(
        !dest_batch
            .windows(tutorial_portal_create.len())
            .any(|window| window == tutorial_portal_create),
        "the destination join ships none of the tutorial's placed portal entities"
    );

    // The generated destination names no coordinate the server can state, so
    // the avatar create carries the `0x800` start-room warp gate and a
    // `0x03 [eid][0x15]` entity update follows: the client places the avatar
    // at its own maze-computed `ZoneStart` waypoint. A hand-placed zone ships
    // neither.
    let cls = dest_batch
        .windows(CLASS.len())
        .position(|window| window == CLASS.as_bytes())
        .expect("the destination creates the stored avatar class");
    assert_eq!(
        dest_batch[cls - 4],
        0x01,
        "create tag precedes the classname"
    );
    assert_eq!(
        dest_batch[cls - 1],
        0xFF,
        "classname tag precedes the classname"
    );
    let avatar_eid = u16::from_le_bytes(dest_batch[cls - 3..cls - 1].try_into().unwrap());
    let mut init = vec![0x02u8];
    init.extend_from_slice(&avatar_eid.to_le_bytes());
    let init_at = cls
        + dest_batch[cls..]
            .windows(init.len())
            .position(|window| window == init.as_slice())
            .expect("avatar class-state head");
    assert_eq!(
        u32::from_le_bytes(dest_batch[init_at + 3..init_at + 7].try_into().unwrap()),
        drf_proto::MINIMAL_AVATAR_WE_FLAGS_ATTACH,
        "the destination ships the scene-attach flags"
    );
    let ready = game.read_until(0x0D, 0x01);
    assert_eq!(
        &ready[..2],
        &[0x0D, 0x01],
        "the destination join ships Ready"
    );
    let count = game.read_until(0x0D, 0x05);
    assert_eq!(
        &count[..2],
        &[0x0D, 0x05],
        "the destination join ships InstanceCount"
    );

    // The zone moved with the assign and was persisted.
    let persisted = fs::read_to_string(temp.path().join("s1_store.json")).expect("read store");
    assert!(
        persisted.contains("\"zone\": \"dungeon00_level01\""),
        "the destination zone is persisted; store:\n{persisted}"
    );

    // The destination join's QuestManager carries the Recent Zone Portal row
    // for the zone the transfer left: tutorial, with the third legacy string
    // empty. The client draws this row only when AutoPortalZone is non-empty.
    let qm_class = b"QuestManager\0";
    let qm_at = dest_batch
        .windows(qm_class.len())
        .position(|window| window == qm_class)
        .expect("the destination join nests QuestManager");
    let quest_cid = u16::from_le_bytes(dest_batch[qm_at - 3..qm_at - 1].try_into().unwrap());
    assert!(
        dest_batch
            .windows(b"tutorial\0\0\0".len())
            .any(|window| window == b"tutorial\0\0\0"),
        "AutoPortalZone=tutorial + empty spawn/legacy strings ride the join"
    );

    // The row's "go" is the empty QuestManager 0x0C request; the server
    // returns to the zone it saved, which is the tutorial here.
    let mut recent = quest_cid.to_le_bytes().to_vec();
    recent.extend_from_slice(&[0x0C, 0x01, 0x00, 0x00, 0x00]);
    game.send(0x07, 0x34, &recent);
    let back = game.read_until(0x0D, 0x02);
    assert_eq!(&back[..2], &[0x0D, 0x02], "recent portal starts a transfer");
    let back_assign = game.read_until(0x0D, 0x00);
    assert_eq!(
        &back_assign[2..10],
        b"tutorial",
        "recent portal returns to the saved zone"
    );
}

/// A left-click attack (`07 34 ... 50`) is acknowledged even when the target
/// is unknown, and attacking a tracked mob enough times arms the death path
/// once the server's own HP copy is exhausted. With no hook telemetry in this
/// harness the grace fallback finalizes server state and ships the drop, but it
/// deliberately does not send the `(07,05)` remove (the client may already have
/// purged the entity). Telemetry-driven ordering is unit-tested in
/// `session::tests`.
#[test]
fn attacking_a_mob_is_acked_and_drops_loot_when_tracked_hp_is_spent() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("combat");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");
    store
        .set_zone(ACCOUNT, sqlid, "dungeon00_level01")
        .expect("seed the generated dungeon zone");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let shared = server.shared();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let Some(spawn) = shared
        .mob_spawns("dungeon00_level01")
        .into_iter()
        .find(|spawn| spawn.hp_wire > 0)
    else {
        eprintln!("skip: dungeon00_level01 generated no mobs");
        return;
    };

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    let needle = format!("{}\0", spawn.classname);
    let at = batch
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())
        .expect("the join ships the chosen mob class");
    assert_eq!(batch[at - 1], 0xFF, "classname tag precedes the mob class");
    let mob_eid = u16::from_le_bytes(batch[at - 3..at - 1].try_into().unwrap());
    let ent = u32::from(mob_eid);

    let mut body = ent.to_le_bytes().to_vec();
    body.extend_from_slice(&[0x50, 0x01, 0x0A]);
    body.extend_from_slice(&mob_eid.to_le_bytes());
    game.send(0x07, 0x34, &body);

    let ack = game.read_until(0x07, 0x35);
    let mut expected = vec![0x07, 0x35];
    expected.extend_from_slice(&body);
    expected.extend_from_slice(&[0x02]);
    expected.extend_from_slice(&AVATAR_HP_WIRE);
    expected.push(0x06);
    assert_eq!(
        ack, expected,
        "the attack ack echoes the use body and carries the avatar HP word"
    );

    // Each accepted attack removes at least one tracked point, so the spawn's
    // raw max HP bounds the number of attacks needed to reach zero.
    let raw_hp = spawn.hp_wire / 256;
    for _ in 1..raw_hp {
        let mut body = ent.to_le_bytes().to_vec();
        body.extend_from_slice(&[0x50, 0x01, 0x0A]);
        body.extend_from_slice(&mob_eid.to_le_bytes());
        game.send(0x07, 0x34, &body);
    }

    let (drop, skipped) = game.read_until_collect(0x07, 0x08);
    assert!(
        skipped
            .iter()
            .all(|frame| !(frame.len() >= 2 && frame[0] == 0x07 && frame[1] == 0x05)),
        "no remove is sent before the death drop"
    );
    assert_eq!(&drop[..2], &[0x07, 0x08], "the kill ships its loot drop");
    // The fallback does not send a `(07,05)` remove: by the time it fires the
    // client may already have purged the entity, and removing an unknown id
    // latches CEM error 6. The telemetry path sends it at the death record.
    // The server never originates a `(07,03)` entity update for the kill: the
    // client self-awarded its XP, so a `0x0F` AddExperience here would
    // double-award it. (The one legitimate `0x03` the protocol has -- the
    // generated-zone `0x15` avatar placement -- is not sent for a kill.)
    assert!(
        skipped
            .iter()
            .all(|frame| !(frame.len() >= 2 && frame[0] == 0x07 && frame[1] == 0x03)),
        "the server must not send an entity update (0x0F AddExperience) on a kill"
    );
    // The mirrored award is persisted: an L1 player gets the flat 500 for any
    // mob level (the ratio caps at the player), and does not level on one kill.
    let persisted = fs::read_to_string(temp.path().join("s1_store.json")).expect("read store");
    assert!(
        persisted.contains("\"experience\": 500"),
        "the mirrored kill XP is persisted; store:\n{persisted}"
    );
    assert!(
        persisted.contains("\"level\": 1"),
        "one 500-XP kill does not reach the L2 threshold of 1000; store:\n{persisted}"
    );
}

/// A `0x64` control request that names a MOB's `Behavior` component is ignored.
///
/// The mobs ship pure-native (no `0x64` enroll; bible §14.6 round 6n) and the
/// client's own `MonsterBehavior2` brain runs them. A c2s
/// `[behavior_cid][0x64][01]` must not be answered with a fresh
/// `35 <cid> 64 01 02 <spawn hp> 06`: once the client has self-rolled the
/// player's damage, its local mob HP no longer matches the stale spawn word, so
/// the zero-tolerance `EntitySynchInfo::Validate` would fatal ("Entity synch
/// error", error 1). The server therefore only ever grants the avatar's own
/// UnitBehavior (covered by [`auth_list_select_and_join_reach_the_avatar`]) and
/// never originates a mob component update in response to a mob request.
#[test]
fn mob_control_request_is_ignored_so_the_server_never_asserts_mob_hp() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("control-route");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");
    store
        .set_zone(ACCOUNT, sqlid, "dungeon00_level01")
        .expect("seed the generated dungeon zone");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let shared = server.shared();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let Some(spawn) = shared
        .mob_spawns("dungeon00_level01")
        .into_iter()
        .find(|spawn| spawn.hp_wire > 0)
    else {
        eprintln!("skip: dungeon00_level01 generated no mobs");
        return;
    };

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    // The mob's Behavior component: `32 <owner> <cid> FF <class> 00`.
    let behavior = format!("{}\0", drf_proto::MOB_BEHAVIOR_CLASS);
    let bat = batch
        .windows(behavior.len())
        .position(|window| window == behavior.as_bytes())
        .expect("the join ships the mob's Behavior component");
    assert_eq!(batch[bat - 1], 0xFF, "classname tag precedes the class");
    let behavior_cid = u16::from_le_bytes(batch[bat - 3..bat - 1].try_into().unwrap());
    let owner = u16::from_le_bytes(batch[bat - 5..bat - 3].try_into().unwrap());
    assert_ne!(behavior_cid, 0);
    assert_ne!(owner, 0);

    // The client asks for control of the mob's Behavior component.
    let mut control = behavior_cid.to_le_bytes().to_vec();
    control.extend_from_slice(&[0x64, 0x01]);
    game.send(0x07, 0x34, &control);

    // No grant is owed: the server must not address the mob's Behavior component
    // with a `0x35` (its trailer would assert the stale spawn HP).
    let prefix = {
        let mut prefix = vec![0x07u8, 0x35];
        prefix.extend_from_slice(&behavior_cid.to_le_bytes());
        prefix
    };
    for frame in game.read_frames(6) {
        assert!(
            frame.len() < prefix.len() || frame[..prefix.len()] != prefix[..],
            "the server sent a mob Behavior component update: {frame:02x?}"
        );
    }
    let _ = spawn;
}

/// A generated mob's join stream is PURE NATIVE: it ships no `0x64` enroll.
///
/// Bible §14.6 round 6n (live-confirmed): the client's own `MonsterBehavior2`
/// brain already proximity-aggros, chases to melee (~5u) and attacks with
/// animation with NO server help. The `35 <behavior-cid> 64 01 02 <hp>`
/// FollowClient block (`UnitBehavior+0xE5 = 4`) was the breaker — it parked the
/// mob in client-control mode chasing the avatar's centre, so the native melee
/// swing never played. The join therefore ships ONLY the two native trailers,
/// SpawnAction (`0x04`) and MoverUpdate (`0x65`).
#[test]
fn a_generated_mob_create_stream_is_pure_native() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("mob-native");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");
    store
        .set_zone(ACCOUNT, sqlid, "dungeon00_level01")
        .expect("seed the generated dungeon zone");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let shared = server.shared();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let Some(spawn) = shared
        .mob_spawns("dungeon00_level01")
        .into_iter()
        .find(|spawn| spawn.hp_wire > 0)
    else {
        eprintln!("skip: dungeon00_level01 generated no mobs");
        return;
    };

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    // The mob's Behavior component: `32 <owner> <cid> FF <class> 00`.
    let behavior = format!("{}\0", drf_proto::MOB_BEHAVIOR_CLASS);
    let bat = batch
        .windows(behavior.len())
        .position(|window| window == behavior.as_bytes())
        .expect("the join ships the mob's Behavior component");
    assert_eq!(batch[bat - 1], 0xFF, "classname tag precedes the class");
    let behavior_cid = u16::from_le_bytes(batch[bat - 3..bat - 1].try_into().unwrap());
    assert_ne!(behavior_cid, 0);

    // PURE-NATIVE (bible §14.6 round 6n): the join must NOT enroll the mob. The
    // `35 <cid> 64 01 02` FollowClient block parked the mob in client-control
    // mode chasing the avatar's centre, so its native melee swing never played.
    let mut enroll = vec![0x35u8];
    enroll.extend_from_slice(&behavior_cid.to_le_bytes());
    enroll.extend_from_slice(&[0x64, 0x01, 0x02]);
    assert!(
        !batch.windows(enroll.len()).any(|window| window == enroll),
        "the join must NOT enroll mob behavior {behavior_cid:#06x} (pure native)"
    );

    // Both native trailers ARE present: `35 <cid> 04` (SpawnAction) and
    // `35 <cid> 65` (MoverUpdate).
    for sub in [0x04u8, 0x65u8] {
        let mut want = vec![0x35u8];
        want.extend_from_slice(&behavior_cid.to_le_bytes());
        want.push(sub);
        assert!(
            batch.windows(want.len()).any(|window| window == want),
            "the join is missing the 0x{sub:02x} trailer for {behavior_cid:#06x}"
        );
    }
    let _ = spawn;
}

/// The player's first `0x50` against a live mob is acked and the server sends
/// the mob NO behavior action.
///
/// NATIVE-BRAIN MODEL: the client's own monster brain owns aggro/chase/attack
/// (the mob shipped `0x64`-enrolled at spawn), so the server installs nothing.
/// A server `Follow` (`0x16`) would displace the native attack, and any
/// HP-asserting action trailer would carry the stale spawn word the client's
/// locally-simulated mob HP no longer matches, tripping
/// `EntitySynchInfo::Validate` (error 1). The `0x50` leg only acks and charges
/// the server's placeholder HP for the loot/death/XP hook.
#[test]
fn first_attack_on_a_mob_acks_and_sends_no_behavior_action() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("mob-aggro");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");
    store
        .set_zone(ACCOUNT, sqlid, "dungeon00_level01")
        .expect("seed the generated dungeon zone");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let shared = server.shared();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let Some(spawn) = shared
        .mob_spawns("dungeon00_level01")
        .into_iter()
        .find(|spawn| spawn.hp_wire > 0)
    else {
        eprintln!("skip: dungeon00_level01 generated no mobs");
        return;
    };

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    let needle = format!("{}\0", spawn.classname);
    let at = batch
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())
        .expect("the join ships the chosen mob class");
    let mob_eid = u16::from_le_bytes(batch[at - 3..at - 1].try_into().unwrap());

    let behavior = format!("{}\0", drf_proto::MOB_BEHAVIOR_CLASS);
    let bat = batch
        .windows(behavior.len())
        .position(|window| window == behavior.as_bytes())
        .expect("the join ships the mob's Behavior component");
    let behavior_cid = u16::from_le_bytes(batch[bat - 3..bat - 1].try_into().unwrap());

    let mut body = u32::from(mob_eid).to_le_bytes().to_vec();
    body.extend_from_slice(&[0x50, 0x01, 0x0A]);
    body.extend_from_slice(&mob_eid.to_le_bytes());
    game.send(0x07, 0x34, &body);

    // The ack echoes the use body and carries the avatar HP word.
    let ack = game.read_until(0x07, 0x35);
    let mut expected_ack = vec![0x07, 0x35];
    expected_ack.extend_from_slice(&body);
    expected_ack.push(0x02);
    expected_ack.extend_from_slice(&AVATAR_HP_WIRE);
    expected_ack.push(0x06);
    assert_eq!(ack, expected_ack, "the attack ack echoes the use body");

    // The server sends the mob no behavior action: the client brain owns it.
    // Scan the next few frames (the ticker's `(07,0D)` intervals) for any
    // `35 <behavior-cid> 04 <class>` component action.
    let mut prefix = vec![0x07u8, 0x35];
    prefix.extend_from_slice(&behavior_cid.to_le_bytes());
    prefix.push(0x04);
    for frame in game.read_frames(6) {
        assert!(
            frame.len() < prefix.len() || frame[..prefix.len()] != prefix[..],
            "the server installed a behavior action on the native mob: {frame:02x?}"
        );
    }
    let _ = spawn;
}

/// Killing a dungeon mob ships ONE `ItemObject` drop createinit; left-clicking
/// the drop is a `0x06` BehaviourActionActivate answered with ONE `(07,35)`
/// stream carrying the activate ack (with the avatar's HP word), a `05 <drop>`
/// remove sub-tag and a UnitContainer item add -- the item lands in the bag.
///
/// The drop pose is the client-reported death pose when the hook is present;
/// this harness has no telemetry, so the grace fallback uses the mob's spawn
/// anchor. It never uses the player's live position (a mover in this test
/// proves that a stray player move cannot move the drop).
#[test]
fn killing_a_mob_drops_loot_and_picking_it_up_bags_it() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("loot");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");
    store
        .set_zone(ACCOUNT, sqlid, "dungeon00_level01")
        .expect("seed the generated dungeon zone");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let shared = server.shared();
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let assign = game.read_until(0x0D, 0x00);
    let assign_seed = parse_assign_payload(&assign[2..])
        .expect("parse assign")
        .seed;
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    let spawns: Vec<_> = shared
        .mob_spawns_with_seed("dungeon00_level01", assign_seed, 1)
        .into_iter()
        .filter(|spawn| spawn.hp_wire > 0)
        .collect();
    if spawns.len() < 2 {
        eprintln!("skip: dungeon00_level01 generated fewer than two live mobs");
        return;
    }
    let spawn_a = &spawns[0];
    let spawn_b = &spawns[1];

    // The avatar eid and its UnitContainer cid, from the join's own creates.
    let avatar_at = batch
        .windows(CLASS.len())
        .position(|window| window == CLASS.as_bytes())
        .expect("the join creates the stored avatar class");
    assert_eq!(batch[avatar_at - 4], 0x01, "create tag precedes the class");
    assert_eq!(
        batch[avatar_at - 1],
        0xFF,
        "classname tag precedes the class"
    );
    let _avatar_eid = u16::from_le_bytes(batch[avatar_at - 3..avatar_at - 1].try_into().unwrap());
    let container_class = b"UnitContainer\x00";
    let container_at = batch
        .windows(container_class.len())
        .position(|window| window == container_class)
        .expect("the join ships the avatar UnitContainer");
    let container_cid = u16::from_le_bytes(
        batch[container_at - 3..container_at - 1]
            .try_into()
            .unwrap(),
    );
    let ub_class = b"avatar.base.UnitBehavior\x00";
    let ub_at = batch
        .windows(ub_class.len())
        .position(|window| window == ub_class)
        .expect("the join ships the avatar UnitBehavior");
    let avatar_ub_cid = u16::from_le_bytes(batch[ub_at - 3..ub_at - 1].try_into().unwrap());

    // The join creates the generated mobs in spawn order, so the first two
    // `01 <eid> FF <classname> 00` creates whose class is a live spawn are
    // `spawns[0]` and `spawns[1]`.
    let spawn_classes: std::collections::HashSet<&str> = spawns
        .iter()
        .map(|spawn| spawn.classname.as_str())
        .collect();
    let mut mob_eids: Vec<u16> = Vec::new();
    let mut scan = 0usize;
    while scan + 4 <= batch.len() {
        if batch[scan] == 0x01 && batch[scan + 3] == 0xFF {
            if let Some(end) = batch[scan + 4..].iter().position(|&byte| byte == 0) {
                if let Ok(name) = std::str::from_utf8(&batch[scan + 4..scan + 4 + end]) {
                    if spawn_classes.contains(name) {
                        mob_eids.push(u16::from_le_bytes(
                            batch[scan + 1..scan + 3].try_into().unwrap(),
                        ));
                    }
                }
            }
        }
        scan += 1;
    }
    assert!(mob_eids.len() >= 2, "the join ships the generated mobs");
    let mob_eid = mob_eids[0];
    let mob_b_eid = mob_eids[1];

    // PURE-NATIVE (bible §14.6 round 6n): each generated mob's behavior
    // component ships NO `0x64` FollowClient enroll — the client's own
    // `MonsterBehavior2` brain owns aggro/chase/attack. Only the two native
    // trailers (SpawnAction `0x04`, MoverUpdate `0x65`) address the component.
    let behavior_class = format!("{}\0", drf_proto::MOB_BEHAVIOR_CLASS);
    let behavior_needle = behavior_class.as_bytes();
    let mut behavior_cids = Vec::new();
    let mut at = 0usize;
    while let Some(found) = batch[at..]
        .windows(behavior_needle.len())
        .position(|window| window == behavior_needle)
    {
        let class_at = at + found;
        behavior_cids.push(u16::from_le_bytes(
            batch[class_at - 3..class_at - 1].try_into().unwrap(),
        ));
        at = class_at + behavior_needle.len();
    }
    assert_eq!(
        behavior_cids.len(),
        mob_eids.len(),
        "one behavior component per generated mob"
    );
    for (cid, _spawn) in behavior_cids.iter().zip(&spawns) {
        let mut enroll = vec![0x35];
        enroll.extend_from_slice(&cid.to_le_bytes());
        enroll.extend_from_slice(&[0x64, 0x01, 0x02]);
        assert!(
            !batch.windows(enroll.len()).any(|window| window == enroll),
            "mob behavior cid {cid:#06x} must NOT be enrolled (pure native)"
        );
        // The two native trailers still address the behavior component.
        let mut mover = vec![0x35];
        mover.extend_from_slice(&cid.to_le_bytes());
        mover.extend_from_slice(&[0x65]);
        assert!(
            batch.windows(mover.len()).any(|window| window == mover),
            "mob behavior cid {cid:#06x} still ships its MoverUpdate"
        );
    }

    // Kill the first mob BEFORE any mover: the session has no tracked player
    // position yet, so the drop must fall back to the mob's spawn anchor.
    let raw_hp = spawn_a.hp_wire / 256;
    for _ in 0..raw_hp {
        let mut body = u32::from(mob_eid).to_le_bytes().to_vec();
        body.extend_from_slice(&[0x50, 0x01, 0x0A]);
        body.extend_from_slice(&mob_eid.to_le_bytes());
        game.send(0x07, 0x34, &body);
    }
    let (drop, skipped) = game.read_until_collect(0x07, 0x08);
    assert!(
        skipped
            .iter()
            .all(|frame| !(frame.len() >= 2 && frame[0] == 0x07 && frame[1] == 0x05)),
        "no CEM remove: the killer's client despawns the mob itself"
    );

    // The death hook ships ONE drop: a combined `0x08` createinit whose
    // classname is ItemObject and whose init carries the real item class.
    assert_eq!(&drop[..2], &[0x07, 0x08], "drop createinit tag");
    let drop_eid = u16::from_le_bytes(drop[2..4].try_into().unwrap());
    assert_ne!(drop_eid, mob_eid, "the drop is its own entity");
    assert_eq!(drop[4], 0xFF, "drop createinit classname tag");
    assert_eq!(&drop[5..16], b"ItemObject\0", "the drop entity class");
    // The init: WorldEntity base, item fields, owner, `0xFF`, item class.
    assert_eq!(
        u32::from_le_bytes(drop[16..20].try_into().unwrap()),
        0x0000_0006,
        "the drop ships the WorldEntity flags word"
    );
    assert_eq!(
        u32::from_le_bytes(drop[20..24].try_into().unwrap()),
        spawn_a.pose[0] as u32,
        "with no mover seen, the drop falls back to the mob's spawn x"
    );
    assert_eq!(
        u32::from_le_bytes(drop[24..28].try_into().unwrap()),
        spawn_a.pose[1] as u32,
        "with no mover seen, the drop falls back to the mob's spawn y"
    );
    assert_eq!(drop[36], 0x00, "fb gate: no WorldEntity tails");
    assert_eq!(drop[37], 0, "state 0 selects the client's spawn animation");
    assert_eq!(
        u32::from_le_bytes(drop[40..44].try_into().unwrap()),
        1,
        "w1 = 1"
    );
    assert_eq!(
        u32::from_le_bytes(drop[48..52].try_into().unwrap()),
        27000,
        "the native drop's 27000-tick despawn timer"
    );
    assert_eq!(
        drop[52], 0x01,
        "the +0x118 spawn flag is set so addToWorld animates the drop"
    );
    let owner_end = drop[53..].iter().position(|&b| b == 0).unwrap() + 53;
    assert_eq!(
        &drop[53..owner_end],
        ACCOUNT.as_bytes(),
        "the owner is the login"
    );
    assert_eq!(drop[owner_end + 1], 0xFF, "item-desc tag");
    let item_start = owner_end + 2;
    let item_end = drop[item_start..].iter().position(|&b| b == 0).unwrap() + item_start;
    let item_class = std::str::from_utf8(&drop[item_start..item_end]).expect("item class UTF-8");
    assert_eq!(
        item_class, "items.consumables.Consumable_MajorHealthPotion",
        "the first kill rolls the table's first entry"
    );

    // Left-click the drop: the `0x06` BehaviourActionActivate the client sends
    // for EVERY click (NPC / portal / chest / drop). Shape (10B inner):
    // `07 34 | u16 component | 01 | responseId | 06 | session | u16 target`.
    let body = {
        let mut body = vec![0xE0, 0xE0, 0x01, 0x01, 0x06, 0x0B];
        body.extend_from_slice(&drop_eid.to_le_bytes());
        body
    };
    game.send(0x07, 0x34, &body);

    // The ack, the ground-entity remove and the bag add all ride ONE `(07,35)`
    // stream. The kill loop over-attacked a dead mob, so its late acks are still
    // buffered: skip every `07 35` whose body is not the pickup's own.
    let stream = loop {
        let candidate = game.read_until(0x07, 0x35);
        if candidate.len() >= 2 + body.len() && candidate[2..2 + body.len()] == body[..] {
            break candidate;
        }
    };
    assert_eq!(
        &stream[..2],
        &[0x07, 0x35],
        "the combined pickup stream tag"
    );

    // (1) The activate ack: the click body 1:1, then the AVATAR's synch trailer
    // `02` + the x256 HP word 51200 (`00 c8 00 00`) -- never a fixed literal.
    let ack_end = 2 + body.len();
    assert_eq!(&stream[2..ack_end], &body[..], "the click is echoed 1:1");
    assert_eq!(
        &stream[ack_end..ack_end + 5],
        &AVATAR_SYNCH,
        "the ack carries the avatar's HP word"
    );

    // (2) The ground-entity remove as a `05 <eid>` sub-tag INSIDE the stream.
    let remove_at = ack_end + 5;
    assert_eq!(stream[remove_at], 0x05, "the remove sub-tag");
    assert_eq!(
        &stream[remove_at + 1..remove_at + 3],
        &drop_eid.to_le_bytes(),
        "the drop entity is removed, and only it"
    );

    // (3) The bag add, addressed to the avatar UnitContainer.
    let add_at = remove_at + 3;
    assert_eq!(stream[add_at], 0x35, "the item add update opens");
    assert_eq!(
        &stream[add_at + 1..add_at + 3],
        &container_cid.to_le_bytes(),
        "the add is addressed to the avatar UnitContainer"
    );
    assert_eq!(stream[add_at + 3], 0x1E, "the bag item-add sub-opcode");
    assert_eq!(stream[add_at + 4], 0x0B, "the bag inventory child id");
    assert!(
        stream[add_at + 5..]
            .windows(item_class.len())
            .any(|window| window == item_class.as_bytes()),
        "the bag add carries the dropped item class"
    );
    // The add's own synch trailer, then the single stream terminator.
    assert_eq!(
        &stream[stream.len() - 6..stream.len() - 1],
        &AVATAR_SYNCH,
        "the item add carries the avatar's HP word too"
    );
    assert_eq!(*stream.last().unwrap(), 0x06, "EndStream");

    // There is NO acquire frame: CEM tag 0x33 is a processComponentInit that
    // reads a u16 componentId, so the drop's eid there would fault the client.

    // Clicking the (now gone) drop again is acked and changes nothing: the
    // forgotten drop gets the BARE activate ack, with no `05` remove sub-tag and
    // no second item add.
    game.send(0x07, 0x34, &body);
    let second = loop {
        let candidate = game.read_until(0x07, 0x35);
        if candidate.len() >= 2 + body.len() && candidate[2..2 + body.len()] == body[..] {
            break candidate;
        }
    };
    let mut expected_bare = vec![0x07, 0x35];
    expected_bare.extend_from_slice(&body);
    expected_bare.extend_from_slice(&AVATAR_SYNCH_END);
    assert_eq!(
        second, expected_bare,
        "a forgotten drop gets the bare ack, no remove and no add"
    );

    // -- a mover must NOT move the next drop --------------------------------
    // The server still tracks the player's live position, but loot now uses
    // the mob's death pose (or the spawn anchor on the fallback path). Send a
    // c2s mover and prove the second kill's drop stays at the spawn anchor.
    // One mover record is 13 bytes: `[type u8][heading i32][X i32][Y i32]`
    // (reference `movement.py::_handle_client_move`), so X rides the record's
    // bytes 5..9 and Y its 9..13.
    let player_x = 123 * 256i32;
    let player_y = 456 * 256i32;
    let mut mover = avatar_ub_cid.to_le_bytes().to_vec();
    mover.extend_from_slice(&[0x65, 0x07, 0x01]);
    mover.push(0x00); // move type
    mover.extend_from_slice(&0i32.to_le_bytes()); // heading
    mover.extend_from_slice(&player_x.to_le_bytes());
    mover.extend_from_slice(&player_y.to_le_bytes());
    assert_eq!(mover.len(), 5 + 13, "one 13-byte mover record");
    game.send(0x07, 0x34, &mover);

    let raw_hp_b = spawn_b.hp_wire / 256;
    for _ in 0..raw_hp_b {
        let mut body = u32::from(mob_b_eid).to_le_bytes().to_vec();
        body.extend_from_slice(&[0x50, 0x01, 0x0A]);
        body.extend_from_slice(&mob_b_eid.to_le_bytes());
        game.send(0x07, 0x34, &body);
    }
    let (drop_b, skipped_b) = game.read_until_collect(0x07, 0x08);
    assert!(
        skipped_b
            .iter()
            .all(|frame| !(frame.len() >= 2 && frame[0] == 0x07 && frame[1] == 0x05)),
        "no remove is sent before the second death drop"
    );
    assert_eq!(&drop_b[..2], &[0x07, 0x08], "second drop createinit tag");
    let drop_b_eid = u16::from_le_bytes(drop_b[2..4].try_into().unwrap());
    assert_ne!(drop_b_eid, drop_eid, "the second drop is its own entity");
    // This harness has no hook UDP death record, so the grace fallback uses the
    // mob's spawn anchor. With telemetry the drop lands at the client-reported
    // death pose instead (session::tests covers that path); it never uses the
    // player's position.
    assert_eq!(
        u32::from_le_bytes(drop_b[20..24].try_into().unwrap()),
        spawn_b.pose[0] as u32,
        "the fallback drop sits at the mob's spawn x"
    );
    assert_eq!(
        u32::from_le_bytes(drop_b[24..28].try_into().unwrap()),
        spawn_b.pose[1] as u32,
        "the fallback drop sits at the mob's spawn y"
    );

    // The second roll is LeatherArmor3PAL.LeatherArmor3-2, whose class declares
    // one ItemModifier (`BaseArmorClasses.Leather`'s SpeedM). Its `readData`
    // body must carry one status byte before the (empty) rolled-modifier list,
    // or the client reads the `0x06` EndStream as a count of six modifiers.
    let item_class_b = "LeatherArmor3PAL.LeatherArmor3-2";
    let class_at_b = drop_b
        .windows(item_class_b.len())
        .position(|window| window == item_class_b.as_bytes())
        .expect("the second drop carries the armour class");
    let body_at_b = class_at_b + item_class_b.len() + 1;
    assert_eq!(
        &drop_b[body_at_b..body_at_b + 4],
        &0u32.to_le_bytes(),
        "a fresh ground drop's item id is 0"
    );
    assert_eq!(
        &drop_b[body_at_b + 4..body_at_b + 7],
        &[0xFF, 0xFF, 0x01],
        "the drop is unplaced (the client ctor's own 0xFF sentinel)"
    );
    assert_eq!(drop_b[body_at_b + 8], 0x00, "item flags");
    assert_eq!(
        drop_b[body_at_b + 9],
        0x00,
        "the armour's one ItemModifier status byte"
    );
    assert_eq!(
        drop_b[body_at_b + 10],
        0x00,
        "an empty rolled-modifier list"
    );
    assert_eq!(
        drop_b[body_at_b + 11],
        0x06,
        "EndStream follows the completed equipment body"
    );

    // Click the ARMOUR drop. Its bag add must carry the class's one
    // `ItemModifier` status byte before the empty rolled-modifier list. The
    // pre-fix add omitted it, so the client's `ReadChildData<ItemModifier>` read
    // the synch trailer's `0x02` as a modifier count of two and then the `0xC5`
    // (the avatar HP word's second byte, 50539 = 0xC56B) as a type tag -- the
    // live `GCClassRegistry::readType Invalid type tag 200` fatal, on exactly
    // this `LeatherArmor3PAL.LeatherArmor3-2` drop.
    let body_b = {
        let mut body = vec![0xE0, 0xE0, 0x01, 0x01, 0x06, 0x0B];
        body.extend_from_slice(&drop_b_eid.to_le_bytes());
        body
    };
    game.send(0x07, 0x34, &body_b);

    let stream_b = loop {
        let candidate = game.read_until(0x07, 0x35);
        if candidate.len() >= 2 + body_b.len() && candidate[2..2 + body_b.len()] == body_b[..] {
            break candidate;
        }
    };
    assert_eq!(
        &stream_b[..2],
        &[0x07, 0x35],
        "the armour pickup stream tag"
    );

    // (1) Activate ack, then (2) the `05 <eid>` ground remove as a top-level
    // CEM record (the live pickup batch in the reference bible rides the remove
    // in-frame exactly this way), then (3) the add.
    let ack_end_b = 2 + body_b.len();
    assert_eq!(
        &stream_b[2..ack_end_b],
        &body_b[..],
        "the armour click is echoed"
    );
    // Two mirrored kills (500 XP each) crossed the L2 threshold, so the
    // avatar's level-scaled HP word moved with it (the client self-levels on the
    // same award; shipping the stale L1 word would fail the synch compare).
    let mut expected_synch = vec![0x02u8];
    expected_synch.extend_from_slice(&drf_proto::avatar_hp_wire_at_level(CLASS, 2).to_le_bytes());
    assert_eq!(
        &stream_b[ack_end_b..ack_end_b + 5],
        expected_synch.as_slice(),
        "the armour ack carries the level-2 avatar HP word"
    );
    let remove_at_b = ack_end_b + 5;
    assert_eq!(stream_b[remove_at_b], 0x05, "the armour remove record tag");
    assert_eq!(
        &stream_b[remove_at_b + 1..remove_at_b + 3],
        &drop_b_eid.to_le_bytes(),
        "the armour drop entity is removed, and only it"
    );
    let add_at_b = remove_at_b + 3;
    assert_eq!(stream_b[add_at_b], 0x35, "the armour add update opens");
    assert_eq!(
        &stream_b[add_at_b + 1..add_at_b + 3],
        &container_cid.to_le_bytes(),
        "the armour add is addressed to the avatar UnitContainer"
    );
    assert_eq!(stream_b[add_at_b + 3], 0x1E, "the bag item-add sub-opcode");
    assert_eq!(stream_b[add_at_b + 4], 0x0B, "the bag inventory child id");
    let class_at_bb = stream_b
        .windows(item_class_b.len())
        .position(|window| window == item_class_b.as_bytes())
        .expect("the armour add carries the dropped class");
    let body_at_bb = class_at_bb + item_class_b.len() + 1;
    assert_eq!(
        stream_b[body_at_bb + 8],
        0x00,
        "the armour add's flags byte"
    );
    assert_eq!(
        stream_b[body_at_bb + 9],
        0x00,
        "the armour's one ItemModifier status byte precedes the rolled-modifier count"
    );
    assert_eq!(
        stream_b[body_at_bb + 10],
        0x00,
        "an empty rolled-modifier list"
    );
    assert_eq!(
        &stream_b[body_at_bb + 11..body_at_bb + 16],
        expected_synch.as_slice(),
        "the completed armour body is followed by the add's synch trailer"
    );
    assert_eq!(*stream_b.last().unwrap(), 0x06, "EndStream");
}

/// A running server plus the character it serves, for the inventory tests.
struct InventoryHarness {
    dir: TempDir,
    shared: Arc<Shared>,
    game_port: u16,
    sqlid: u32,
}

impl InventoryHarness {
    fn new(
        tag: &str,
        zone: &str,
        bag: Vec<BagItem>,
        equipment: Option<Vec<EquipmentItem>>,
    ) -> Option<InventoryHarness> {
        let dir = game_pkg_dir()?;
        let temp = TempDir::new(tag);
        let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
        store.ensure_account(ACCOUNT).expect("ensure account");
        let sqlid = store
            .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
            .expect("seed character");
        store.set_zone(ACCOUNT, sqlid, zone).expect("seed zone");
        if !bag.is_empty() {
            store.set_bag(ACCOUNT, sqlid, bag).expect("seed bag");
        }
        if let Some(equipment) = equipment {
            store
                .set_equipment(ACCOUNT, sqlid, equipment)
                .expect("seed equipment");
        }
        let server = Server::new(
            "127.0.0.1:0",
            "127.0.0.1:0",
            "127.0.0.1",
            store,
            Some((dir.join("game.pki"), dir.join("game.pkg"))),
        )
        .expect("bind server");
        let shared = server.shared();
        let game_port = server.game_port();
        thread::spawn(move || server.serve());
        Some(InventoryHarness {
            dir: temp,
            shared,
            game_port,
            sqlid,
        })
    }

    fn join(&self) -> (GameClient, Vec<u8>) {
        let mut game = GameClient::new(self.game_port);
        game.send(0x04, 0x05, &select_payload(self.sqlid));
        let _ack = game.read_until(0x04, 0x05);
        let _assign = game.read_until(0x0D, 0x00);
        game.send(0x0D, 0x06, &[]);
        let batch = game.read_until(0x07, 0x0D);
        (game, batch)
    }

    fn store(&self) -> Store {
        Store::open(self.dir.path().join("s1_store.json")).expect("reopen store")
    }
}

fn joined_cid(batch: &[u8], classname: &[u8]) -> u16 {
    let mut needle = vec![0xFF];
    needle.extend_from_slice(classname);
    needle.push(0);
    let tag = batch
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
        .unwrap_or_else(|| {
            panic!(
                "join does not ship {:?}",
                String::from_utf8_lossy(classname)
            )
        });
    u16::from_le_bytes(batch[tag - 2..tag].try_into().unwrap())
}

/// `(id, x, y, count)` from an `Item::readData` record at `record`.
fn item_fields(record: &[u8]) -> (u32, u8, u8, u8) {
    assert_eq!(record[0], 0xFF, "item record opens with its type tag");
    let nul = record[1..].iter().position(|&b| b == 0).unwrap() + 1;
    let off = nul + 1;
    (
        u32::from_le_bytes(record[off..off + 4].try_into().unwrap()),
        record[off + 4],
        record[off + 5],
        record[off + 6],
    )
}

/// The (id, record offset) of an item class inside a component stream.
fn item_in_stream(stream: &[u8], classname: &[u8]) -> (u32, usize) {
    let mut needle = classname.to_vec();
    needle.push(0);
    let at = stream
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
        .unwrap_or_else(|| {
            panic!(
                "stream does not carry {:?}",
                String::from_utf8_lossy(classname)
            )
        });
    let record = at - 1;
    assert_eq!(stream[record], 0xFF, "item-desc tag precedes the class");
    (item_fields(&stream[record..]).0, record)
}

/// The offset of `classname`'s first occurrence in a component stream.
fn class_offset(stream: &[u8], classname: &[u8]) -> usize {
    let mut needle = classname.to_vec();
    needle.push(0);
    stream
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
        .unwrap_or_else(|| {
            panic!(
                "stream does not carry {:?}",
                String::from_utf8_lossy(classname)
            )
        })
}

fn has_update(stream: &[u8], cid: u16, sub: u8) -> bool {
    let mut needle = vec![0x35];
    needle.extend_from_slice(&cid.to_le_bytes());
    needle.push(sub);
    stream.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn bag_get_put_moves_the_cell_and_persists() {
    let bag = vec![BagItem {
        class: "QuestItemPAL2.D00_Q11_a1_01".to_string(),
        cell_x: 0,
        cell_y: 0,
        count: 1,
        level: None,
        flags: None,
        modifiers: Vec::new(),
    }];
    let Some(harness) = InventoryHarness::new("bag-move", "tutorial", bag, None) else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let (mut game, batch) = harness.join();
    let container = joined_cid(&batch, b"UnitContainer");
    let reship = game.read_until(0x07, 0x35);
    let (item, _) = item_in_stream(&reship, b"QuestItemPAL2.D00_Q11_a1_01");

    // Select -> UnitContainer 0x28 carrying the same bag id.
    let mut get = container.to_le_bytes().to_vec();
    get.push(0x28);
    get.extend_from_slice(&item.to_le_bytes());
    game.send(0x07, 0x34, &get);
    let active = game.read_until(0x07, 0x35);
    assert_eq!(&active[2..4], &container.to_le_bytes());
    assert_eq!(active[4], 0x28, "set active");
    assert_eq!(
        item_fields(&active[5..]).0,
        item,
        "the bag id is re-stamped"
    );

    // Put at (6, 2): clear active, remove the old node, add at the new cell.
    let put = [container.to_le_bytes().as_slice(), &[0x29, 0x0B, 6, 2]].concat();
    game.send(0x07, 0x34, &put);
    let stream = game.read_until(0x07, 0x35);
    assert_eq!(&stream[2..4], &container.to_le_bytes());
    assert_eq!(stream[4], 0x29, "clear active first");
    assert!(has_update(&stream, container, 0x1F), "old node removed");
    assert!(has_update(&stream, container, 0x1E), "new cell added");
    let (moved_id, record) = item_in_stream(&stream, b"QuestItemPAL2.D00_Q11_a1_01");
    assert_eq!(moved_id, item, "the id is stable across the move");
    let moved = item_fields(&stream[record..]);
    assert_eq!((moved.1, moved.2), (6, 2), "new cell");

    let store = harness.store();
    let bag = store.get_bag(ACCOUNT, harness.sqlid);
    assert_eq!(bag.len(), 1);
    assert_eq!((bag[0].cell_x, bag[0].cell_y), (6, 2));
}

#[test]
fn bag_put_swaps_when_the_destination_holds_another_item() {
    let a = "QuestItemPAL2.D00_Q11_a1_01";
    let b = "QuestItemPAL2.D00_Q11_a2_01";
    let bag = vec![
        BagItem {
            class: a.to_string(),
            cell_x: 0,
            cell_y: 0,
            count: 1,
            level: None,
            flags: None,
            modifiers: Vec::new(),
        },
        BagItem {
            class: b.to_string(),
            cell_x: 1,
            cell_y: 0,
            count: 1,
            level: None,
            flags: None,
            modifiers: Vec::new(),
        },
    ];
    let Some(harness) = InventoryHarness::new("bag-swap", "tutorial", bag, None) else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let (mut game, batch) = harness.join();
    let container = joined_cid(&batch, b"UnitContainer");
    let reship = game.read_until(0x07, 0x35);
    let (a_id, _) = item_in_stream(&reship, a.as_bytes());
    let (b_id, _) = item_in_stream(&reship, b.as_bytes());

    let mut get = container.to_le_bytes().to_vec();
    get.push(0x28);
    get.extend_from_slice(&a_id.to_le_bytes());
    game.send(0x07, 0x34, &get);
    let _active = game.read_until(0x07, 0x35);

    let mut put = container.to_le_bytes().to_vec();
    put.extend_from_slice(&[0x29, 0x0B, 1, 0]);
    game.send(0x07, 0x34, &put);
    let stream = game.read_until(0x07, 0x35);
    for id in [a_id, b_id] {
        let mut remove = vec![0x35];
        remove.extend_from_slice(&container.to_le_bytes());
        remove.push(0x1F);
        remove.extend_from_slice(&id.to_le_bytes());
        assert!(
            stream.windows(remove.len()).any(|window| window == remove),
            "the swap removes both old nodes"
        );
    }
    let (_, a_record) = item_in_stream(&stream, a.as_bytes());
    let (_, b_record) = item_in_stream(&stream, b.as_bytes());
    let a_fields = item_fields(&stream[a_record..]);
    let b_fields = item_fields(&stream[b_record..]);
    assert_eq!(
        (a_fields.1, a_fields.2),
        (1, 0),
        "the moved item takes the target cell"
    );
    assert_eq!(
        (b_fields.1, b_fields.2),
        (0, 0),
        "the displaced item takes the source cell"
    );

    let store = harness.store();
    let bag = store.get_bag(ACCOUNT, harness.sqlid);
    assert_eq!(
        bag.iter()
            .find(|row| row.class == a)
            .map(|row| (row.cell_x, row.cell_y)),
        Some((1, 0))
    );
    assert_eq!(
        bag.iter()
            .find(|row| row.class == b)
            .map(|row| (row.cell_x, row.cell_y)),
        Some((0, 0))
    );
}

#[test]
fn equip_moves_a_bag_item_to_the_paper_doll_and_survives_rejoin() {
    let bag = vec![
        BagItem {
            class: "LeatherArmor3PAL.LeatherArmor3-2".to_string(),
            cell_x: 0,
            cell_y: 0,
            count: 1,
            level: None,
            flags: None,
            modifiers: Vec::new(),
        },
        BagItem {
            class: "QuestItemPAL2.D00_Q11_a1_01".to_string(),
            cell_x: 2,
            cell_y: 0,
            count: 1,
            level: None,
            flags: None,
            modifiers: Vec::new(),
        },
    ];
    let Some(harness) = InventoryHarness::new("equip", "tutorial", bag, None) else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let (mut game, batch) = harness.join();
    let container = joined_cid(&batch, b"UnitContainer");
    let equipment = joined_cid(&batch, b"avatar.base.Equipment");
    let manipulators = joined_cid(&batch, b"Manipulators");
    let reship = game.read_until(0x07, 0x35);
    let (item, _) = item_in_stream(&reship, b"LeatherArmor3PAL.LeatherArmor3-2");

    let mut get = container.to_le_bytes().to_vec();
    get.push(0x28);
    get.extend_from_slice(&item.to_le_bytes());
    game.send(0x07, 0x34, &get);
    let _active = game.read_until(0x07, 0x35);

    let mut equip = equipment.to_le_bytes().to_vec();
    equip.push(0x28);
    equip.extend_from_slice(&8u32.to_le_bytes());
    equip.extend_from_slice(&item.to_le_bytes());
    game.send(0x07, 0x34, &equip);
    let stream = game.read_until(0x07, 0x35);
    assert_eq!(&stream[2..4], &equipment.to_le_bytes());
    assert_eq!(stream[4], 0x28, "Equipment add leads");
    assert!(has_update(&stream, container, 0x29), "cursor cleared");
    assert!(has_update(&stream, container, 0x1F), "bag node removed");
    assert!(has_update(&stream, manipulators, 0x00), "visual added");
    let (worn_slot, record) = item_in_stream(&stream, b"LeatherArmor3PAL.LeatherArmor3-2");
    assert_eq!(worn_slot, 8, "the record id is the destination slot");
    let worn = item_fields(&stream[record..]);
    assert_eq!((worn.1, worn.2), (0xFF, 0xFF));

    let store = harness.store();
    assert!(
        store
            .get_bag(ACCOUNT, harness.sqlid)
            .iter()
            .all(|row| row.class != "LeatherArmor3PAL.LeatherArmor3-2"),
        "the equipped row left the bag"
    );
    let worn = store
        .get_equipment(ACCOUNT, harness.sqlid)
        .expect("explicit worn set");
    assert!(worn
        .iter()
        .any(|row| row.slot == 8 && row.class == "LeatherArmor3PAL.LeatherArmor3-2"));

    // Re-login: the worn set and the remaining bag row are both reshipped.
    let (mut game2, batch2) = harness.join();
    assert!(batch2
        .windows(b"LeatherArmor3PAL.LeatherArmor3-2\0".len())
        .any(|window| window == b"LeatherArmor3PAL.LeatherArmor3-2\0"));
    let reship2 = game2.read_until(0x07, 0x35);
    let (_, _) = item_in_stream(&reship2, b"QuestItemPAL2.D00_Q11_a1_01");
}

#[test]
fn unequip_places_the_worn_item_in_the_bag_and_persists() {
    let equipment = vec![EquipmentItem {
        slot: 8,
        class: "LeatherArmor3PAL.LeatherArmor3-2".to_string(),
        level: 1,
        flags: 0,
        modifiers: Vec::new(),
    }];
    let Some(harness) = InventoryHarness::new("unequip", "tutorial", Vec::new(), Some(equipment))
    else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let (mut game, batch) = harness.join();
    let container = joined_cid(&batch, b"UnitContainer");
    let equipment_cid = joined_cid(&batch, b"avatar.base.Equipment");
    let manipulators = joined_cid(&batch, b"Manipulators");
    assert!(batch
        .windows(b"LeatherArmor3PAL.LeatherArmor3-2\0".len())
        .any(|window| window == b"LeatherArmor3PAL.LeatherArmor3-2\0"));

    let mut unequip = equipment_cid.to_le_bytes().to_vec();
    unequip.push(0x29);
    unequip.extend_from_slice(&8u32.to_le_bytes());
    game.send(0x07, 0x34, &unequip);
    let stream = game.read_until(0x07, 0x35);
    assert_eq!(&stream[2..4], &equipment_cid.to_le_bytes());
    assert_eq!(stream[4], 0x29, "Equipment remove leads");
    assert!(has_update(&stream, container, 0x1E), "bag add follows");
    assert!(has_update(&stream, manipulators, 0x01), "visual removed");
    let (bag_id, record) = item_in_stream(&stream, b"LeatherArmor3PAL.LeatherArmor3-2");
    assert_ne!(bag_id, 8, "a fresh bag id is minted");
    assert_ne!(item_fields(&stream[record..]).1, 0xFF, "placed in a cell");

    let store = harness.store();
    assert_eq!(
        store.get_equipment(ACCOUNT, harness.sqlid),
        Some(Vec::new()),
        "an explicit empty worn set is persisted"
    );
    let bag = store.get_bag(ACCOUNT, harness.sqlid);
    assert_eq!(bag.len(), 1);
    assert_eq!(bag[0].class, "LeatherArmor3PAL.LeatherArmor3-2");

    let (mut game2, _batch2) = harness.join();
    let reship = game2.read_until(0x07, 0x35);
    let (_, _) = item_in_stream(&reship, b"LeatherArmor3PAL.LeatherArmor3-2");
}

#[test]
fn potions_stack_on_pickup_and_use_decrements_then_removes() {
    let potion = "items.consumables.Consumable_MajorHealthPotion";
    let bag = vec![BagItem {
        class: potion.to_string(),
        cell_x: 0,
        cell_y: 0,
        count: 1,
        level: None,
        flags: None,
        modifiers: Vec::new(),
    }];
    let Some(harness) = InventoryHarness::new("potion", "dungeon00_level01", bag, None) else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let Some(spawn) = harness
        .shared
        .mob_spawns("dungeon00_level01")
        .into_iter()
        .find(|spawn| spawn.hp_wire > 0)
    else {
        eprintln!("skip: dungeon00_level01 generated no mobs");
        return;
    };
    let (mut game, batch) = harness.join();
    let container = joined_cid(&batch, b"UnitContainer");
    let ub = joined_cid(&batch, b"avatar.base.UnitBehavior");
    let reship = game.read_until(0x07, 0x35);
    let (item, _) = item_in_stream(&reship, potion.as_bytes());

    let needle = format!("{}\0", spawn.classname);
    let at = batch
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())
        .expect("join ships the chosen mob");
    let mob_eid = u16::from_le_bytes(batch[at - 3..at - 1].try_into().unwrap());
    let raw_hp = spawn.hp_wire / 256;
    for _ in 0..raw_hp {
        let mut body = u32::from(mob_eid).to_le_bytes().to_vec();
        body.extend_from_slice(&[0x50, 0x01, 0x0A]);
        body.extend_from_slice(&mob_eid.to_le_bytes());
        game.send(0x07, 0x34, &body);
    }
    let (drop, _) = game.read_until_collect(0x07, 0x08);
    let drop_eid = u16::from_le_bytes(drop[2..4].try_into().unwrap());

    let mut click = ub.to_le_bytes().to_vec();
    click.extend_from_slice(&[0x01, 0x01, 0x06, 0x0B]);
    click.extend_from_slice(&drop_eid.to_le_bytes());
    game.send(0x07, 0x34, &click);

    // The second potion merges into the seeded stack (0x22, count 2).
    let merge = loop {
        let candidate = game.read_until(0x07, 0x35);
        let mut want = vec![0x35];
        want.extend_from_slice(&container.to_le_bytes());
        want.extend_from_slice(&[0x22]);
        want.extend_from_slice(&item.to_le_bytes());
        want.push(2);
        if candidate.windows(want.len()).any(|window| window == want) {
            break candidate;
        }
    };
    assert!(
        !merge.is_empty(),
        "the pickup merged into the existing stack"
    );
    assert_eq!(
        harness.store().get_bag(ACCOUNT, harness.sqlid)[0].count,
        2,
        "the merged stack is persisted"
    );

    // First use: entity 0x08 frame, then 0x22 count 1.
    let mut use_one = container.to_le_bytes().to_vec();
    use_one.push(0x25);
    use_one.extend_from_slice(&item.to_le_bytes());
    game.send(0x07, 0x34, &use_one);
    let used = game.read_until(0x07, 0x03);
    assert_eq!(used[4], 0x08, "entity use marker");
    assert!(used[5..].starts_with(&[0xFF]));
    assert!(used[5..]
        .windows(potion.len())
        .any(|window| window == potion.as_bytes()));
    let quantity = loop {
        let candidate = game.read_until(0x07, 0x35);
        let mut want = vec![0x35];
        want.extend_from_slice(&container.to_le_bytes());
        want.extend_from_slice(&[0x22]);
        want.extend_from_slice(&item.to_le_bytes());
        want.push(1);
        if candidate.windows(want.len()).any(|window| window == want) {
            break candidate;
        }
    };
    assert_eq!(
        &quantity[quantity.len() - 6..],
        &[0x02, 0x6B, 0xC5, 0x00, 0x00, 0x06]
    );
    assert_eq!(harness.store().get_bag(ACCOUNT, harness.sqlid)[0].count, 1);

    // Second use: 0x1F removes the bag row.
    game.send(0x07, 0x34, &use_one);
    let _used = game.read_until(0x07, 0x03);
    let removed = loop {
        let candidate = game.read_until(0x07, 0x35);
        let mut want = vec![0x35];
        want.extend_from_slice(&container.to_le_bytes());
        want.extend_from_slice(&[0x1F]);
        want.extend_from_slice(&item.to_le_bytes());
        if candidate.windows(want.len()).any(|window| window == want) {
            break candidate;
        }
    };
    assert!(!removed.is_empty());
    assert_eq!(harness.store().get_bag(ACCOUNT, harness.sqlid), Vec::new());
}

/// Locate a client tree with `game.pkg`, as the content crate's tests do.
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

/// The `--advertised-game-port` override changes only the auth `ServerList`
/// payload: the bound game listener keeps its port, and the port the client is
/// told to connect to is the override. Defaults to the bound port.
#[test]
fn advertised_game_port_overrides_only_the_server_list_payload() {
    // The builder/parser round-trip is the payload contract.
    let payload = drf_server::server::server_list_payload("10.20.30.40", 0x1234);
    let (count, ip, port) = parse_server_list_payload(&payload).expect("decode ServerList");
    assert_eq!(count, 1);
    assert_eq!(ip, std::net::Ipv4Addr::new(10, 20, 30, 40));
    assert_eq!(port, 0x1234);

    let dir = TempDir::new("advertised-port");
    let store = Store::open(dir.path().join("s1_store.json")).expect("open store");
    let server =
        Server::new("127.0.0.1:0", "127.0.0.1:0", "127.0.0.1", store, None).expect("bind server");
    let bind_port = server.game_port();
    assert_ne!(bind_port, 0, "game port is ephemeral, never zero");
    // Default: the advertised port equals the bound port.
    assert_eq!(server.advertised_game_port(), bind_port);
    assert_eq!(server.shared().game_endpoint().1, bind_port);

    // Set: the advertised port differs and the listener binding is untouched.
    let advertised = if bind_port == 0xBEEF { 0x1234 } else { 0xBEEF };
    server.set_advertised_game_port(advertised);
    assert_eq!(server.advertised_game_port(), advertised);
    assert_ne!(server.advertised_game_port(), bind_port);
    assert_eq!(
        server.game_port(),
        bind_port,
        "the bound listener is unchanged"
    );

    // The auth handshake the client sees advertises the override, not the bind.
    let auth_port = server.auth_port();
    thread::spawn(move || server.serve());

    let mut auth = connect(auth_port);
    expect_welcome(&mut auth);
    auth.write_all(&frame(TYPE_LOGIN, &login_block(b"player", b"pw")))
        .expect("send login");
    let (login_type, _login_payload) = read_auth_frame(&mut auth);
    assert_eq!(login_type, TYPE_LOGIN_OK);
    let (list_type, list_payload) = read_auth_frame(&mut auth);
    assert_eq!(list_type, TYPE_SERVER_LIST);
    let (_, _, sent) = parse_server_list_payload(&list_payload).expect("decode ServerList");
    assert_eq!(sent, advertised, "ServerList carries the advertised port");
    assert_ne!(sent, bind_port, "and not the bound game port");
}

/// A WorldInterval's move-ack is suppressed in a combat zone and kept in town.
///
/// `EntitySynchInfo::Validate` (`0x5DD900`) runs the avatar's zero-tolerance HP
/// compare only while the client's peaceful-zone bit (`(*(behavior+0x11C)+0x95)
/// & 1`, the same bit `0x4A4AE7` reads for "Can't use in town.") is clear. A
/// dungeon leaves it clear, so the server must not originate an avatar-HP
/// trailer there: the `0x35` grammar makes the trailer mandatory, so the whole
/// move-ack is dropped and the interval carries only the budget/PathManager
/// watchdog. Town/tutorial set the bit, so the echo (and its avatar-HP trailer)
/// is sent as before.
#[test]
fn combat_zone_move_ack_is_suppressed_but_town_keeps_it() {
    for (zone, expect_ack) in [("tutorial", true), ("dungeon00_level01", false)] {
        let dir = TempDir::new("move-ack-zone");
        let mut store = Store::open(dir.path().join("s1_store.json")).expect("open store");
        store.ensure_account(ACCOUNT).expect("ensure account");
        let sqlid = store
            .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
            .expect("seed character");
        store.set_zone(ACCOUNT, sqlid, zone).expect("set zone");

        let server = Server::new("127.0.0.1:0", "127.0.0.1:0", "127.0.0.1", store, None)
            .expect("bind server");
        let game_port = server.game_port();
        thread::spawn(move || server.serve());

        let mut game = GameClient::new(game_port);
        game.send(0x04, 0x05, &select_payload(sqlid));
        let _ack = game.read_until(0x04, 0x05);
        let _assign = game.read_until(0x0D, 0x00);
        game.send(0x0D, 0x06, &[]);
        let batch = game.read_until(0x07, 0x0D);
        let ub_cid = joined_unit_behavior_cid(&batch);

        // Stage one mover record addressed to the joined avatar UnitBehavior.
        // The server routes mover records by component, so the addressee must
        // be the avatar's own cid (a mob's records are not echoed as the
        // avatar's).
        let mut mover = ub_cid.to_le_bytes().to_vec();
        mover.extend_from_slice(&[0x65, 0x00, 0x01]);
        mover.extend_from_slice(&[0u8; 13]);
        game.send(0x07, 0x34, &mover);

        // The ticker drains the staged move on its next interval (every 4th
        // 33 ms tick); read a few so the staged move is not missed.
        let mut saw_ack = false;
        for _ in 0..6 {
            let interval = game.read_until(0x07, 0x0D);
            if interval.len() > 23 && interval[22..].contains(&0x35) {
                saw_ack = true;
            }
        }
        assert_eq!(
            saw_ack, expect_ack,
            "zone {zone}: the move-ack must follow the peaceful-zone split"
        );
    }
}

/// `--trust-client-sim` re-enables the owner move-ack in a combat zone.
///
/// With the paired client hook installed the avatar's HP compare is bypassed,
/// so the server may carry the verbatim owner echo in a dungeon. Without the
/// flag the same zone stays suppressed. Either way the interval keeps the
/// budget triple and the PathManager watchdog the client's world clock needs.
#[test]
fn hooked_client_move_ack_is_sent_in_a_combat_zone() {
    // The `(07,0D)` body after the tag: `[u32 tick][u32 0x21][u32 0x03]`
    // `[8 PathManager]`. The move-ack, when present, starts at offset 20.
    const PATHMANAGER: [u8; 8] = [0x01, 0x00, 0x00, 0x00, 0x64, 0x00, 0x14, 0x00];
    for (trust, expect_ack) in [(false, false), (true, true)] {
        let dir = TempDir::new("trust-client-sim");
        let mut store = Store::open(dir.path().join("s1_store.json")).expect("open store");
        store.ensure_account(ACCOUNT).expect("ensure account");
        let sqlid = store
            .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
            .expect("seed character");
        store
            .set_zone(ACCOUNT, sqlid, "dungeon00_level01")
            .expect("set zone");

        let server = Server::new("127.0.0.1:0", "127.0.0.1:0", "127.0.0.1", store, None)
            .expect("bind server");
        server.set_trust_client_sim(trust);
        assert_eq!(server.trust_client_sim(), trust);
        let game_port = server.game_port();
        thread::spawn(move || server.serve());

        let mut game = GameClient::new(game_port);
        game.send(0x04, 0x05, &select_payload(sqlid));
        let _ack = game.read_until(0x04, 0x05);
        let _assign = game.read_until(0x0D, 0x00);
        game.send(0x0D, 0x06, &[]);
        let batch = game.read_until(0x07, 0x0D);
        let ub_cid = joined_unit_behavior_cid(&batch);

        let mut mover = ub_cid.to_le_bytes().to_vec();
        mover.extend_from_slice(&[0x65, 0x00, 0x01]);
        mover.extend_from_slice(&[0u8; 13]);
        game.send(0x07, 0x34, &mover);

        let mut saw_ack = false;
        for _ in 0..6 {
            let interval = game.read_until(0x07, 0x0D);
            assert_eq!(&interval[0..2], &[0x07, 0x0D]);
            assert_eq!(&interval[6..10], &0x21u32.to_le_bytes(), "budget 0x21");
            assert_eq!(&interval[10..14], &0x03u32.to_le_bytes(), "budget 0x03");
            assert_eq!(&interval[14..22], &PATHMANAGER, "PathManager watchdog");
            if interval.len() > 22 && interval[22..].contains(&0x35) {
                saw_ack = true;
            }
        }
        assert_eq!(
            saw_ack, expect_ack,
            "trust_client_sim={trust}: dungeon move-ack follows the hook gate"
        );
    }
}

/// Hotbar place (`Skills` 0x35) binds a skill to a Manipulators slot and
/// removes it again with 0x36, persisting both sides of the change.
#[test]
fn hotbar_place_and_remove_persist() {
    let Some(harness) = InventoryHarness::new("hotbar", "tutorial", Vec::new(), None) else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let (mut game, batch) = harness.join();
    let skills = joined_cid(&batch, b"avatar.base.Skills");
    let manipulators = joined_cid(&batch, b"Manipulators");

    // dfc_hash("skills.generic.FireBolt") -- the Warlock's starting skill.
    let hash = 0x4024_3947u32;
    let mut place = skills.to_le_bytes().to_vec();
    place.push(0x35);
    place.extend_from_slice(&0x65u32.to_le_bytes());
    place.push(0x04);
    place.extend_from_slice(&hash.to_le_bytes());
    game.send(0x07, 0x34, &place);

    let stream = game.read_until(0x07, 0x35);
    assert!(has_update(&stream, manipulators, 0x00), "Manipulators add");
    let at = class_offset(&stream, b"skills.generic.FireBolt");
    let slot_at = at + b"skills.generic.FireBolt".len() + 1;
    assert_eq!(
        u32::from_le_bytes(stream[slot_at..slot_at + 4].try_into().unwrap()),
        0x65,
        "the skill entry carries the hotbar slot"
    );
    let level_at = slot_at + 4;
    assert_eq!(stream[level_at], 1, "skill level");
    assert_eq!(
        stream[level_at + 1],
        0x02,
        "the synch trailer follows the level with no trailing readInit byte"
    );

    let stored = harness
        .store()
        .get_hotbar(ACCOUNT, harness.sqlid)
        .expect("explicit hotbar");
    assert!(stored
        .iter()
        .any(|row| row.slot == 0x65 && row.skill == "skills.generic.FireBolt"));

    let mut remove = skills.to_le_bytes().to_vec();
    remove.push(0x36);
    remove.extend_from_slice(&0x65u32.to_le_bytes());
    game.send(0x07, 0x34, &remove);

    let stream = game.read_until(0x07, 0x35);
    assert!(
        has_update(&stream, manipulators, 0x01),
        "Manipulators remove"
    );
    let stored = harness
        .store()
        .get_hotbar(ACCOUNT, harness.sqlid)
        .expect("explicit hotbar");
    assert!(stored.iter().all(|row| row.slot != 0x65));
}

/// Merchant `0x1F` sells a bag item: one `(07,35)` stream credits the purse
/// and removes the bag node, and both survive a rejoin.
#[test]
fn selling_a_bag_item_credits_gold_and_removes_it() {
    let class = "LeatherArmor3PAL.LeatherArmor3-2";
    let bag = vec![
        BagItem {
            class: class.to_string(),
            cell_x: 0,
            cell_y: 0,
            count: 1,
            level: None,
            flags: None,
            modifiers: Vec::new(),
        },
        BagItem {
            class: "items.consumables.Consumable_MajorHealthPotion".to_string(),
            cell_x: 0,
            cell_y: 1,
            count: 1,
            level: None,
            flags: None,
            modifiers: Vec::new(),
        },
    ];
    let Some(harness) = InventoryHarness::new("sell", "tutorial", bag, None) else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let (mut game, batch) = harness.join();
    let container = joined_cid(&batch, b"UnitContainer");
    let merchant_class = b"world.tutorial.npc.HermitVendor.Merchant\x00";
    let at = batch
        .windows(merchant_class.len())
        .position(|window| window == merchant_class)
        .expect("the tutorial join ships HermitVendor's Merchant");
    let merchant = u16::from_le_bytes(batch[at - 3..at - 1].try_into().unwrap());
    let reship = game.read_until(0x07, 0x35);
    let (item, _) = item_in_stream(&reship, class.as_bytes());

    let gold_before = harness
        .store()
        .get_gold(ACCOUNT, harness.sqlid)
        .unwrap_or(0);
    let mut sell = merchant.to_le_bytes().to_vec();
    sell.push(0x1F);
    sell.extend_from_slice(&0u16.to_le_bytes());
    sell.extend_from_slice(&item.to_le_bytes());
    game.send(0x07, 0x34, &sell);

    let stream = game.read_until(0x07, 0x35);
    assert!(has_update(&stream, container, 0x20), "gold credit");
    assert!(has_update(&stream, container, 0x1F), "bag remove");

    let store = harness.store();
    assert!(
        store
            .get_bag(ACCOUNT, harness.sqlid)
            .iter()
            .all(|row| row.class != class),
        "the sold row left the persisted bag"
    );
    assert!(
        store.get_gold(ACCOUNT, harness.sqlid).unwrap_or(0) > gold_before,
        "the purse was credited"
    );

    let (mut game2, _) = harness.join();
    let reship2 = game2.read_until(0x07, 0x35);
    assert!(
        !reship2
            .windows(class.len())
            .any(|window| window == class.as_bytes()),
        "a sold item is not re-shipped"
    );
}

/// Non-faithful feature: a character's visited zones become obelisk
/// `Checkpoint` rows on the next join, alongside the shipped starting
/// checkpoints.
#[test]
fn join_ships_a_checkpoint_for_every_visited_zone() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("visited-checkpoints");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");
    store.set_zone(ACCOUNT, sqlid, "town").expect("seed zone");
    store
        .record_visited_zone(ACCOUNT, sqlid, "dungeon01_level05")
        .expect("visit dungeon01");
    store
        .record_visited_zone(ACCOUNT, sqlid, "Dungeon09_level00")
        .expect("visit dungeon09");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    for class in [
        "world.checkpoints.Dungeon01Checkpoint",
        "world.checkpoints.Dungeon08Checkpoint",
        "world.checkpoints.TownCheckpoint",
    ] {
        let mut needle = vec![0xFF];
        needle.extend_from_slice(class.as_bytes());
        needle.push(0);
        assert!(
            batch
                .windows(needle.len())
                .any(|window| window == needle.as_slice()),
            "join should ship the visited-zone checkpoint {class}"
        );
    }

    // The join also records the current zone (town) for the next time.
    let persisted = fs::read_to_string(temp.path().join("s1_store.json")).expect("read store");
    assert!(
        persisted.contains("visited_zones") && persisted.contains("\"town\""),
        "the join records the current zone; store:\n{persisted}"
    );
}

/// `--admin` sets the player permission byte (`Player+0xA0`) the client's
/// `Avatar::IsAdmin` reads, which arms `/admin` and `/adminui`.
#[test]
fn admin_flag_sets_the_player_permission_byte() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("admin-flag");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    server.set_admin_commands(true);
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let _assign = game.read_until(0x0D, 0x00);
    game.send(0x0D, 0x06, &[]);
    let batch = game.read_until(0x07, 0x0D);

    let mut marker = vec![0xFF];
    marker.extend_from_slice(b"player\0");
    let player_at = batch
        .windows(marker.len())
        .position(|window| window == marker.as_slice())
        .expect("join ships the player create-init");
    let name_start = player_at + marker.len();
    let name_end = batch[name_start..]
        .iter()
        .position(|&byte| byte == 0)
        .expect("player name terminator")
        + name_start;
    let tail = name_end + 1;
    assert_eq!(batch[tail + 8], 0, "the admin flag must clear Player+0xA0");
}

/// Pwnston (`pvp_start`) is a hand-placed town-like zone; the join must ship
/// the player and avatar records so the client leaves the loading screen.
#[test]
fn pvp_start_join_ships_player_and_avatar() {
    let Some(harness) = InventoryHarness::new("pvp-start-join", "pvp_start", Vec::new(), None)
    else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let (_game, batch) = harness.join();
    assert!(
        batch
            .windows(b"\xFFplayer\0".len())
            .any(|w| w == b"\xFFplayer\0"),
        "pvp_start join ships the player record"
    );
    let mut avatar = vec![0xFF];
    avatar.extend_from_slice(CLASS.as_bytes());
    avatar.push(0);
    assert!(
        batch.windows(avatar.len()).any(|w| w == avatar.as_slice()),
        "pvp_start join ships the avatar class"
    );
}

/// A character persisted in the cut `pvp_town` name is normalized to the live
/// `pvp_start` zone before the select assign, so a bad admin transfer cannot
/// wedge the world-join screen.
#[test]
fn cut_pvp_town_normalizes_to_pvp_start_on_select() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("pvp-town-select");
    let mut store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");
    store
        .set_zone(ACCOUNT, sqlid, "pvp_town")
        .expect("seed cut zone");

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    )
    .expect("bind server");
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut game = GameClient::new(game_port);
    game.send(0x04, 0x05, &select_payload(sqlid));
    let _ack = game.read_until(0x04, 0x05);
    let assign = game.read_until(0x0D, 0x00);
    let parsed = parse_assign_payload(&assign[2..]).expect("parse assign");
    assert_eq!(parsed.zone, b"pvp_start");
}

#[test]
fn pvp_sentinel_respawn_stays_in_the_match() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let temp = TempDir::new("pvp-sentinel-respawn");
    let store = Store::open(temp.path().join("s1_store.json")).expect("open store");
    let shared = Shared::new(
        store,
        World::new(),
        "127.0.0.1".to_string(),
        0,
        Some((dir.join("game.pki"), dir.join("game.pkg"))),
    );
    let (zone, spawn) = shared
        .zone_respawn("PVPGroupPracticeMatch")
        .expect("practice match respawn resolves");
    assert_eq!(zone, "PVPGroupPracticeMatch");
    assert_eq!(spawn.as_deref(), Some("Start"));
}

#[test]
fn pvp_match_join_omits_authored_waypoints() {
    let Some(harness) =
        InventoryHarness::new("pvp-match-join", "PVPGroupPracticeMatch", Vec::new(), None)
    else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let (_game, batch) = harness.join();
    assert!(
        !batch.windows(b"wp_ffa".len()).any(|w| w == b"wp_ffa"),
        "PvP waypoint classes must not be shipped as CEM entities"
    );
    assert!(
        batch
            .windows(b"\xFFplayer\0".len())
            .any(|w| w == b"\xFFplayer\0"),
        "the match join still ships the player"
    );
}

#[test]
fn difficulty_request_is_acked_and_persisted() {
    let Some(harness) = InventoryHarness::new("difficulty-ack", "town", Vec::new(), None) else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let (mut game, _batch) = harness.join();
    game.send(0x09, 0x17, &[0x02]);
    let reply = game.read_until(0x09, 0x55);
    assert_eq!(reply, vec![0x09, 0x55, 0x02, 0x00]);
    assert_eq!(
        harness
            .store()
            .get_char(ACCOUNT, harness.sqlid)
            .unwrap()
            .difficulty,
        2
    );
}
