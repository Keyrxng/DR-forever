// SPDX-License-Identifier: GPL-3.0-or-later

//! Offline `.drfrec` replay + first-divergence diff harness.
//!
//! Given a recorded session (`.drfrec` v1, UTF-8 JSONL), this module
//! reconstructs the server state from the header, re-feeds the recorded
//! game-port c2s bytes through the REAL server, frames the fresh s2c transcript
//! and diffs it against the recorded s2c, reporting the FIRST divergence.
//!
//! The recording's frames are raw TCP chunks, not protocol messages, so both
//! sides are carved with the same [`drf_wire::Segment`] framer. Auth-port frames
//! are out of scope for P0: the game session adopts the header's account
//! directly (no encrypted handshake is replayed).
//!
//! # Determinism
//!
//! The only wall-clock-driven server output is the per-session `(07,0D)`
//! WorldInterval ticker (one every four 33 ms ticks). Its tick counter advances
//! with real time, so the number of intervals inside a capture window flaps run
//! to run. The harness turns the ticker OFF ([`Shared::set_ticker_enabled`]) and,
//! by default, drops `(07,0D)` WorldInterval frames from BOTH transcripts before
//! diffing (`solicited_only`). Everything else the server sends is a pure
//! function of the c2s input and the reconstructed store, so two runs are
//! byte-identical.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use drf_wire::Segment;

use crate::server::{Server, DEFAULT_ACCOUNT};
use crate::session::CONNECT_ACK;
use crate::store::Store;

/// The recorded frame directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

/// The recorded frame ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    Auth,
    Game,
}

/// One recorded raw TCP chunk.
#[derive(Debug, Clone)]
pub struct RecordedFrame {
    pub t: i64,
    pub dir: Direction,
    pub port: Port,
    pub hex: String,
    pub bytes: Vec<u8>,
}

/// The `.drfrec` header fields the harness consumes.
#[derive(Debug, Clone, Default)]
pub struct RecordingHeader {
    pub v: u32,
    pub created: Option<i64>,
    pub client_build_sha256: Option<String>,
    pub account: Option<String>,
    pub sqlid: Option<u32>,
    pub char_name: Option<String>,
    pub char_class: Option<String>,
    pub zone: Option<String>,
    pub seed: Option<u32>,
    pub seed_raw: Option<String>,
    pub store_path: Option<String>,
    pub store_sha256: Option<String>,
}

/// A parsed `.drfrec` recording.
#[derive(Debug, Clone)]
pub struct Recording {
    pub header: RecordingHeader,
    pub frames: Vec<RecordedFrame>,
    pub marker_count: usize,
    /// Path the recording was loaded from, if any. Used to find the
    /// `<rec-basename>.store.json` snapshot beside the `.drfrec`.
    pub path: Option<PathBuf>,
}

impl Recording {
    /// Parse `.drfrec` v1 from UTF-8 JSONL text.
    pub fn parse(text: &str) -> Result<Recording, String> {
        let mut header = RecordingHeader::default();
        let mut header_seen = false;
        let mut frames = Vec::new();
        let mut marker_count = 0usize;
        for (index, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            let line_no = index + 1;
            let obj = parse_object(line)
                .map_err(|err| format!("line {line_no}: malformed JSON: {err}"))?;
            let kind =
                get_str(&obj, "kind").ok_or_else(|| format!("line {line_no}: missing \"kind\""))?;
            match kind.as_str() {
                "header" => {
                    let v = get_u32(&obj, "v").unwrap_or(0);
                    if v != 1 {
                        return Err(format!("line {line_no}: unsupported .drfrec version {v}"));
                    }
                    header.v = v;
                    header.created = get_i64(&obj, "created");
                    header.client_build_sha256 = get_str(&obj, "client_build_sha256");
                    header.account = get_str(&obj, "account");
                    header.sqlid = get_u32(&obj, "sqlid");
                    header.char_name = get_str(&obj, "char_name");
                    header.char_class = get_str(&obj, "char_class");
                    header.zone = get_str(&obj, "zone");
                    let seed_raw = get_str(&obj, "seed");
                    header.seed = seed_raw.as_deref().and_then(parse_seed);
                    header.seed_raw = seed_raw;
                    header.store_path = get_str(&obj, "store_path");
                    header.store_sha256 = get_str(&obj, "store_sha256");
                    header_seen = true;
                }
                "frame" => {
                    let t = get_i64(&obj, "t")
                        .ok_or_else(|| format!("line {line_no}: frame missing \"t\""))?;
                    let dir = match get_str(&obj, "dir").as_deref() {
                        Some("c2s") => Direction::ClientToServer,
                        Some("s2c") => Direction::ServerToClient,
                        other => {
                            return Err(format!("line {line_no}: frame has bad dir {other:?}"))
                        }
                    };
                    let port = match get_str(&obj, "port").as_deref() {
                        Some("auth") => Port::Auth,
                        Some("game") => Port::Game,
                        other => {
                            return Err(format!("line {line_no}: frame has bad port {other:?}"))
                        }
                    };
                    let hex = get_str(&obj, "hex")
                        .ok_or_else(|| format!("line {line_no}: frame missing \"hex\""))?;
                    let bytes = hex_to_bytes(&hex)
                        .map_err(|err| format!("line {line_no}: bad frame hex: {err}"))?;
                    frames.push(RecordedFrame {
                        t,
                        dir,
                        port,
                        hex,
                        bytes,
                    });
                }
                "marker" => marker_count += 1,
                other => return Err(format!("line {line_no}: unknown kind {other:?}")),
            }
        }
        if !header_seen {
            return Err("recording has no header line".to_string());
        }
        Ok(Recording {
            header,
            frames,
            marker_count,
            path: None,
        })
    }

    /// Load a `.drfrec` from disk.
    pub fn load(path: &Path) -> Result<Recording, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let mut recording = Recording::parse(&text)?;
        recording.path = Some(path.to_path_buf());
        Ok(recording)
    }
}

/// The `MessageAddress` every server->client segment carries (`game_build`'s
/// `[01 0f 00]`).
pub const GAME_ADDR: drf_wire::MessageAddress = drf_wire::MessageAddress {
    kind: 0x01,
    id: 0x000F,
};

/// The raw game-client hello (`0x0A` flag, peer `[1,2,3]`, eight zero bytes)
/// the server blocks on after accept. The raw first byte `0x0A` is
/// indistinguishable from a normal segment's flags byte, which is why the
/// server tests the body length for the initial hello.
pub fn client_hello() -> Vec<u8> {
    let mut hello = vec![0x0a, 0x01, 0x02, 0x03];
    hello.extend_from_slice(&8u32.to_le_bytes());
    hello.extend_from_slice(&[0u8; 8]);
    hello
}

/// One c2s game segment: peer `[1,2,3]`, addr `(0x01, 0x000F)`, zlib inner.
pub fn client_segment(channel: u8, mtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut inner = vec![channel, mtype];
    inner.extend_from_slice(payload);
    Segment::new(
        [0x01, 0x02, 0x03],
        Some(GAME_ADDR),
        Vec::new(),
        drf_wire::Payload::zlib(&inner),
    )
    .encode()
    .expect("encode c2s segment")
}

/// One s2c game segment as the server writes it (peer `[1,2,3]`).
pub fn server_segment(inner: &[u8]) -> Vec<u8> {
    Segment::new(
        [0x01, 0x02, 0x03],
        Some(GAME_ADDR),
        Vec::new(),
        drf_wire::Payload::zlib(inner),
    )
    .encode()
    .expect("encode s2c segment")
}

/// One framed server message (carved from the raw transcript).
#[derive(Debug, Clone)]
pub struct FramedMessage {
    /// Byte offset of the message's first byte in the raw transcript.
    pub start: usize,
    /// The inflated `[channel][mtype][payload]` inner.
    pub inner: Vec<u8>,
}

/// A first divergence between the expected and captured transcripts.
#[derive(Debug, Clone)]
pub struct Divergence {
    /// Message index of the mismatch.
    pub index: usize,
    /// The expected inner, or `None` when the capture ran out first.
    pub expected: Option<Vec<u8>>,
    /// The captured inner, or `None` when the recording ran out first.
    pub got: Option<Vec<u8>>,
    /// The recorded c2s chunk whose timestamp precedes the divergent frame's
    /// source chunk -- the chunk that likely triggered it.
    pub preceding_c2s: Option<String>,
}

impl Divergence {
    /// A human-readable one-block report.
    pub fn render(&self) -> String {
        let expected = self
            .expected
            .as_deref()
            .map(to_hex)
            .unwrap_or_else(|| "<none: recording ended>".to_string());
        let got = self
            .got
            .as_deref()
            .map(to_hex)
            .unwrap_or_else(|| "<none: capture ended>".to_string());
        format!(
            "first divergence at s2c message #{}\n  expected: {expected}\n  got:      {got}\n  preceding c2s chunk: {}",
            self.index,
            self.preceding_c2s.as_deref().unwrap_or("<none>")
        )
    }
}

/// Capture/replay options.
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// Override the store path (else the header's `store_path`).
    pub store_path: Option<PathBuf>,
    /// Override the maze layout seed (else the header's `seed`).
    pub seed: Option<u32>,
    /// `(game.pki, game.pkg)` content paths; `None` serves bare nodes.
    pub content_paths: Option<(PathBuf, PathBuf)>,
    /// Spawn the `(07,0D)` WorldInterval ticker (default off for replay).
    pub ticker: bool,
    /// Drop `(07,0D)` WorldInterval frames from both transcripts before diffing.
    pub solicited_only: bool,
    /// How long to wait for the server to finish before giving up on a capture.
    pub read_timeout: Duration,
}

impl Default for CaptureOptions {
    fn default() -> CaptureOptions {
        CaptureOptions {
            store_path: None,
            seed: None,
            content_paths: None,
            ticker: false,
            solicited_only: true,
            read_timeout: Duration::from_secs(30),
        }
    }
}

/// One capture of the replayed server's s2c transcript.
#[derive(Debug, Clone)]
pub struct Capture {
    pub raw_s2c: Vec<u8>,
    pub framed: Vec<FramedMessage>,
}

/// The full replay + diff result.
#[derive(Debug, Clone)]
pub struct ReplayReport {
    pub expected_len: usize,
    pub got_len: usize,
    pub divergence: Option<Divergence>,
    /// sha256 over the captured (filtered) message transcript.
    pub sha256: String,
    pub auth_frames_skipped: usize,
    pub world_intervals_dropped: usize,
}

impl ReplayReport {
    pub fn agrees(&self) -> bool {
        self.divergence.is_none()
    }
}

/// A self-cleaning temp directory.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut path = std::env::temp_dir();
        path.push(format!("drf-replay-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// sha256 of a store file.
fn store_sha256(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|err| format!("cannot read store {}: {err}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

/// Pick the store to replay: the `--store` override, else the header's
/// `store_path` when it still matches `store_sha256`, else the
/// `<rec-basename>.store.json` snapshot beside the `.drfrec`.
fn resolve_store_source(rec: &Recording, opts: &CaptureOptions) -> Result<PathBuf, String> {
    if let Some(path) = opts.store_path.clone() {
        return Ok(path);
    }
    let store_path = rec.header.store_path.as_ref().map(PathBuf::from);
    let expected = rec.header.store_sha256.as_deref();
    if let Some(path) = store_path.as_ref() {
        match expected {
            // Legacy recordings without a hash: honour the header path.
            None => return Ok(path.clone()),
            Some(expected) => {
                if let Ok(actual) = store_sha256(path) {
                    if actual == expected {
                        return Ok(path.clone());
                    }
                }
            }
        }
    }

    let snapshot = rec
        .path
        .as_ref()
        .map(|path| path.with_extension("store.json"))
        .ok_or_else(|| {
            "recording was not loaded from a file, so no beside-file store snapshot can be \
             found; pass --store <path>"
                .to_string()
        })?;
    if snapshot.is_file() {
        if let Some(expected) = expected {
            let actual = store_sha256(&snapshot)?;
            if actual != expected {
                return Err(format!(
                    "store snapshot {} has sha256 {actual}, but the recording header has \
                     store_sha256 {expected}; refusing to replay against a different store",
                    snapshot.display()
                ));
            }
        }
        return Ok(snapshot);
    }

    match store_path {
        Some(path) => Err(format!(
            "recorded store {} does not match the header's store_sha256 ({}), and no snapshot \
             {} exists; pass --store <path>",
            path.display(),
            expected.unwrap_or("<none>"),
            snapshot.display()
        )),
        None => Err(format!(
            "recording header has no store_path and no snapshot {} exists; pass --store <path>",
            snapshot.display()
        )),
    }
}

/// Reconstruct the session's store: copy the recorded store into a temp dir,
/// pin the character's zone, and pin the maze seed.
fn prepare_store(rec: &Recording, opts: &CaptureOptions) -> Result<(TempDir, Store), String> {
    let source = resolve_store_source(rec, opts)?;
    let temp = TempDir::new("store");
    let copy = temp.path().join("s1_store.json");
    match std::fs::read(&source) {
        Ok(bytes) => {
            std::fs::write(&copy, &bytes)
                .map_err(|err| format!("cannot stage store copy: {err}"))?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("cannot read store {}: {err}", source.display())),
    }
    let mut store = Store::open(&copy).map_err(|err| format!("cannot open store: {err}"))?;

    let account = rec
        .header
        .account
        .clone()
        .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
    let zone = rec.header.zone.clone();
    let sqlid = rec.header.sqlid.or_else(|| {
        let name = rec.header.char_name.as_ref()?;
        store
            .list_chars(&account)
            .into_iter()
            .find(|character| character.name == *name)
            .map(|character| character.sqlid)
    });
    if let (Some(sqlid), Some(zone)) = (sqlid, zone.as_ref()) {
        store
            .set_zone(&account, sqlid, zone)
            .map_err(|err| format!("cannot set character zone: {err}"))?;
    }
    if let Some(seed) = opts.seed.or(rec.header.seed) {
        // The town and tutorial assign a zero seed by construction; only a
        // generated maze consumes `layout_seed`, so pinning only matters there.
        if let Some(zone) = zone.as_ref() {
            if zone != "town" && zone != "tutorial" {
                drf_content::set_layout_seed_override(zone, seed);
            }
        }
    }
    Ok((temp, store))
}

fn connect(port: u16) -> Result<TcpStream, String> {
    let mut last = None;
    for _ in 0..100 {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                last = Some(error);
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    Err(format!("could not connect to game port {port}: {last:?}"))
}

/// Replay `rec` through a fresh server and capture its s2c transcript.
pub fn capture(rec: &Recording, opts: &CaptureOptions) -> Result<Capture, String> {
    let c2s: Vec<u8> = rec
        .frames
        .iter()
        .filter(|frame| frame.dir == Direction::ClientToServer && frame.port == Port::Game)
        .flat_map(|frame| frame.bytes.iter().copied())
        .collect();
    if c2s.is_empty() {
        return Err("recording has no game-port c2s frames".to_string());
    }

    let (temp, store) = prepare_store(rec, opts)?;

    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        opts.content_paths.clone(),
    )
    .map_err(|err| format!("cannot bind replay server: {err}"))?;
    server.set_ticker_enabled(opts.ticker);
    // The game session adopts the account the auth login would otherwise set,
    // so the recorded character resolves without replaying the cipher.
    let account = rec
        .header
        .account
        .clone()
        .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
    server.shared().set_login(&account);
    let game_port = server.game_port();
    // The staged store copy must outlive the server's (lazily saving) store.
    thread::spawn(move || {
        let _keep = temp;
        server.serve();
    });

    let mut stream = connect(game_port)?;
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|err| format!("cannot set read timeout: {err}"))?;
    stream
        .write_all(&c2s)
        .map_err(|err| format!("cannot send c2s: {err}"))?;
    // Shutting down the write half makes the server read EOF after it has
    // processed every queued c2s frame and close, giving a deterministic end
    // to the capture (rather than a wall-clock idle guess).
    let _ = stream.shutdown(Shutdown::Write);

    let mut raw_s2c = Vec::new();
    let deadline = Instant::now() + opts.read_timeout;
    let mut chunk = [0u8; 65536];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => raw_s2c.extend_from_slice(&chunk[..read]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                if Instant::now() >= deadline {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let framed = frame_server_transcript(&raw_s2c)?;
    Ok(Capture { raw_s2c, framed })
}

/// Carve a raw server transcript into its messages: skip the leading raw
/// 23-byte connect-ack, then decode [`Segment`]s and inflate each payload.
pub fn frame_server_transcript(raw: &[u8]) -> Result<Vec<FramedMessage>, String> {
    let mut offset = 0usize;
    if raw.starts_with(&CONNECT_ACK) {
        offset = CONNECT_ACK.len();
    }
    let mut out = Vec::new();
    while offset < raw.len() {
        match Segment::decode(&raw[offset..]) {
            Ok((segment, used)) => {
                let inner = segment
                    .payload
                    .inflate()
                    .map_err(|_| format!("segment at offset {offset} failed to inflate"))?;
                out.push(FramedMessage {
                    start: offset,
                    inner,
                });
                offset += used;
            }
            // A trailing partial segment: the capture ended mid-frame. Stop.
            Err(drf_wire::SegmentError::Incomplete) => break,
            Err(drf_wire::SegmentError::Invalid) => {
                return Err(format!("invalid segment at offset {offset}"))
            }
        }
    }
    Ok(out)
}

/// The `(07,0D)` WorldInterval ticker frame. It is the only `(07,0D)` message
/// that ends on the `0x06` END marker (a join batch ends on the `0x46`
/// CONNECTED marker).
pub fn is_world_interval(inner: &[u8]) -> bool {
    inner.len() >= 3 && inner[0] == 0x07 && inner[1] == 0x0D && *inner.last().unwrap() == 0x06
}

fn filter_solicited<'a>(
    messages: impl Iterator<Item = &'a FramedMessage>,
) -> (Vec<&'a FramedMessage>, usize) {
    let mut kept = Vec::new();
    let mut dropped = 0usize;
    for message in messages {
        if is_world_interval(&message.inner) {
            dropped += 1;
        } else {
            kept.push(message);
        }
    }
    (kept, dropped)
}

/// The raw game-port s2c chunks with their recorded timestamps, for attributing
/// a divergence to the c2s chunk that preceded it.
fn recorded_s2c(rec: &Recording) -> (Vec<u8>, Vec<(usize, i64)>) {
    let mut bytes = Vec::new();
    let mut chunks = Vec::new();
    for frame in &rec.frames {
        if frame.dir == Direction::ServerToClient && frame.port == Port::Game {
            chunks.push((bytes.len(), frame.t));
            bytes.extend_from_slice(&frame.bytes);
        }
    }
    (bytes, chunks)
}

/// The latest recorded c2s chunk at or before `t`, as hex.
fn preceding_c2s(rec: &Recording, t: i64) -> Option<String> {
    rec.frames
        .iter()
        .rfind(|frame| {
            frame.dir == Direction::ClientToServer && frame.port == Port::Game && frame.t <= t
        })
        .map(|frame| frame.hex.clone())
}

/// The three parts of a first divergence: index, expected, got.
pub type DivergenceParts = (usize, Option<Vec<u8>>, Option<Vec<u8>>);

/// Find the first message-level divergence between two transcripts.
pub fn first_divergence(expected: &[Vec<u8>], got: &[Vec<u8>]) -> Option<DivergenceParts> {
    let max = expected.len().max(got.len());
    for index in 0..max {
        let want = expected.get(index);
        let have = got.get(index);
        if want != have {
            return Some((index, want.cloned(), have.cloned()));
        }
    }
    None
}

fn sha256_of_transcript(messages: &[Vec<u8>]) -> String {
    let mut buf = Vec::new();
    for message in messages {
        buf.extend_from_slice(&(message.len() as u32).to_le_bytes());
        buf.extend_from_slice(message);
    }
    sha256_hex(&buf)
}

/// Replay once, frame both sides, and diff. `solicited_only` drops WorldInterval
/// ticker frames from both transcripts.
pub fn run(rec: &Recording, opts: &CaptureOptions) -> Result<ReplayReport, String> {
    let capture = capture(rec, opts)?;

    let (raw_expected, chunk_times) = recorded_s2c(rec);
    let expected_framed = frame_server_transcript(&raw_expected)?;
    let auth_frames_skipped = rec
        .frames
        .iter()
        .filter(|frame| frame.dir == Direction::ClientToServer && frame.port == Port::Auth)
        .count()
        + rec
            .frames
            .iter()
            .filter(|frame| frame.dir == Direction::ServerToClient && frame.port == Port::Auth)
            .count();

    let mut world_intervals_dropped = 0usize;
    let (expected_msgs, got_msgs): (Vec<Vec<u8>>, Vec<Vec<u8>>) = if opts.solicited_only {
        let (expected_kept, dropped_expected) = filter_solicited(expected_framed.iter());
        let (got_kept, dropped_got) = filter_solicited(capture.framed.iter());
        world_intervals_dropped = dropped_expected + dropped_got;
        (
            expected_kept.iter().map(|m| m.inner.clone()).collect(),
            got_kept.iter().map(|m| m.inner.clone()).collect(),
        )
    } else {
        (
            expected_framed.iter().map(|m| m.inner.clone()).collect(),
            capture.framed.iter().map(|m| m.inner.clone()).collect(),
        )
    };

    let divergence = first_divergence(&expected_msgs, &got_msgs).map(|(index, expected, got)| {
        let source_offset = expected_framed.get(index).map(|m| m.start).unwrap_or(0);
        let t = chunk_time_at(&chunk_times, source_offset);
        Divergence {
            index,
            expected,
            got,
            preceding_c2s: t.and_then(|t| preceding_c2s(rec, t)),
        }
    });

    Ok(ReplayReport {
        expected_len: expected_msgs.len(),
        got_len: got_msgs.len(),
        divergence,
        sha256: sha256_of_transcript(&got_msgs),
        auth_frames_skipped,
        world_intervals_dropped,
    })
}

fn chunk_time_at(chunk_times: &[(usize, i64)], offset: usize) -> Option<i64> {
    chunk_times
        .iter()
        .rfind(|(start, _)| *start <= offset)
        .map(|(_, t)| *t)
}

// -- minimal JSON -------------------------------------------------------------

#[derive(Debug, Clone)]
enum Json {
    Str(String),
    Num(String),
    Bool,
    Null,
}

fn parse_object(line: &str) -> Result<Vec<(String, Json)>, String> {
    let mut parser = JsonParser {
        bytes: line.as_bytes(),
        pos: 0,
    };
    parser.parse_object()
}

fn get_str(obj: &[(String, Json)], key: &str) -> Option<String> {
    obj.iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| match v {
            Json::Str(s) => Some(s.clone()),
            _ => None,
        })
}

fn get_i64(obj: &[(String, Json)], key: &str) -> Option<i64> {
    obj.iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| match v {
            Json::Num(n) => n
                .parse::<i64>()
                .ok()
                .or_else(|| n.parse::<f64>().ok().map(|f| f as i64)),
            _ => None,
        })
}

fn get_u32(obj: &[(String, Json)], key: &str) -> Option<u32> {
    get_i64(obj, key).and_then(|value| u32::try_from(value).ok())
}

/// Parse `"0xXXXXXXXX"` (or a decimal string) into a `u32` seed.
fn parse_seed(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        raw.parse::<u32>().ok()
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn parse_object(&mut self) -> Result<Vec<(String, Json)>, String> {
        self.skip_ws();
        self.expect(b'{')?;
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'}') {
                self.pos += 1;
                break;
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = self.parse_value()?;
            out.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                other => {
                    return Err(format!(
                        "expected ',' or '}}', found {other:?} at byte {}",
                        self.pos
                    ))
                }
            }
        }
        Ok(out)
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'"') => Ok(Json::Str(self.parse_string()?)),
            Some(b't') => {
                self.keyword("true")?;
                Ok(Json::Bool)
            }
            Some(b'f') => {
                self.keyword("false")?;
                Ok(Json::Bool)
            }
            Some(b'n') => {
                self.keyword("null")?;
                Ok(Json::Null)
            }
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            other => Err(format!("bad value start {other:?} at byte {}", self.pos)),
        }
    }

    fn keyword(&mut self, word: &str) -> Result<(), String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(())
        } else {
            Err(format!("expected {word:?} at byte {}", self.pos))
        }
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        ) {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "non-UTF-8 number".to_string())?;
        if text.is_empty() {
            return Err(format!("empty number at byte {start}"));
        }
        Ok(Json::Num(text.to_string()))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let byte = *self
                .bytes
                .get(self.pos)
                .ok_or_else(|| "unterminated string".to_string())?;
            self.pos += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let escape = *self
                        .bytes
                        .get(self.pos)
                        .ok_or_else(|| "unterminated escape".to_string())?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.parse_unicode_escape()?),
                        other => return Err(format!("bad escape \\{}", other as char)),
                    }
                }
                _ => {
                    // Re-decode the UTF-8 byte sequence starting here.
                    let rest = &self.bytes[self.pos - 1..];
                    let text =
                        std::str::from_utf8(rest).map_err(|_| "non-UTF-8 string".to_string())?;
                    let ch = text
                        .chars()
                        .next()
                        .ok_or_else(|| "empty string byte".to_string())?;
                    self.pos += ch.len_utf8() - 1;
                    out.push(ch);
                }
            }
        }
        Ok(out)
    }

    fn parse_unicode_escape(&mut self) -> Result<char, String> {
        let hex = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| "short \\u escape".to_string())?;
        let code = u32::from_str_radix(
            std::str::from_utf8(hex).map_err(|_| "bad \\u escape".to_string())?,
            16,
        )
        .map_err(|_| "bad \\u escape".to_string())?;
        self.pos += 4;
        // A high surrogate must be followed by a low surrogate escape.
        if (0xD800..=0xDBFF).contains(&code) {
            if self.bytes.get(self.pos) == Some(&b'\\')
                && self.bytes.get(self.pos + 1) == Some(&b'u')
            {
                let before = self.pos;
                self.pos += 2;
                let low = self
                    .bytes
                    .get(self.pos..self.pos + 4)
                    .ok_or_else(|| "short low surrogate".to_string())?;
                if let Ok(low_code) = u32::from_str_radix(
                    std::str::from_utf8(low).map_err(|_| "bad low surrogate".to_string())?,
                    16,
                ) {
                    if (0xDC00..=0xDFFF).contains(&low_code) {
                        self.pos += 4;
                        let combined = 0x1_0000 + ((code - 0xD800) << 10) + (low_code - 0xDC00);
                        return Ok(char::from_u32(combined).unwrap_or('\u{FFFD}'));
                    }
                }
                self.pos = before;
            }
            return Ok('\u{FFFD}');
        }
        Ok(char::from_u32(code).unwrap_or('\u{FFFD}'))
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!(
                "expected {:?} at byte {}, found {:?}",
                byte as char,
                self.pos,
                self.peek().map(|b| b as char)
            ))
        }
    }
}

// -- hex ----------------------------------------------------------------------

fn nibble(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(format!("bad hex digit {:?}", other as char)),
    }
}

/// Decode a lowercase-hex string into bytes.
pub fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    let digits = hex.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return Err(format!("odd hex length {}", digits.len()));
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Ok(out)
}

/// Lowercase-hex encode bytes.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((byte & 0x0F) as u32, 16).unwrap());
    }
    out
}

// -- sha256 -------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// A std-only SHA-256 of `data`, lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for block in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut out = String::with_capacity(64);
    for word in state {
        for byte in word.to_be_bytes() {
            out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
            out.push(char::from_digit((byte & 0x0F) as u32, 16).unwrap());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn parses_a_minimal_recording() {
        let text = concat!(
            "{\"v\":1,\"kind\":\"header\",\"account\":\"s1player\",\"sqlid\":1,",
            "\"zone\":\"tutorial\",\"seed\":\"0x0A0CBD2D\",\"store_path\":\"/tmp/x.json\"}\n",
            "{\"v\":1,\"kind\":\"frame\",\"t\":5,\"dir\":\"c2s\",\"port\":\"game\",\"hex\":\"0a01\"}\n",
            "{\"v\":1,\"kind\":\"frame\",\"t\":9,\"dir\":\"s2c\",\"port\":\"auth\",\"hex\":\"00ff\"}\n",
            "{\"v\":1,\"kind\":\"marker\",\"t\":7,\"event\":\"join.enter\"}\n",
        );
        let rec = Recording::parse(text).expect("parse");
        assert_eq!(rec.header.account.as_deref(), Some("s1player"));
        assert_eq!(rec.header.seed, Some(0x0A0C_BD2D));
        assert_eq!(rec.frames.len(), 2);
        assert_eq!(rec.frames[0].dir, Direction::ClientToServer);
        assert_eq!(rec.frames[1].port, Port::Auth);
        assert_eq!(rec.marker_count, 1);
    }

    #[test]
    fn world_interval_classifier_distinguishes_join() {
        let join = {
            let mut frame = vec![0x07u8, 0x0D];
            frame.extend_from_slice(&[0u8; 30]);
            frame.push(0x46);
            frame
        };
        assert!(!is_world_interval(&join));
        let interval = {
            let mut frame = vec![0x07u8, 0x0D];
            frame.extend_from_slice(&[0u8; 18]);
            frame.push(0x06);
            frame
        };
        assert!(is_world_interval(&interval));
    }

    #[test]
    fn first_divergence_reports_the_first_mismatch() {
        let expected = vec![vec![1, 2], vec![3, 4], vec![5]];
        let got = vec![vec![1, 2], vec![9, 9], vec![5]];
        let (index, want, have) = first_divergence(&expected, &got).expect("divergence");
        assert_eq!(index, 1);
        assert_eq!(want, Some(vec![3, 4]));
        assert_eq!(have, Some(vec![9, 9]));
        assert!(first_divergence(&expected, &expected).is_none());
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(to_hex(&[0x0a, 0xff, 0x00]), "0aff00");
        assert_eq!(hex_to_bytes("0aff00").unwrap(), vec![0x0a, 0xff, 0x00]);
        assert!(hex_to_bytes("abc").is_err());
    }

    #[test]
    fn drifted_header_store_falls_back_to_beside_file_snapshot() {
        let temp = TempDir::new("snapshot");
        let live = temp.path().join("live_store.json");
        let recording_path = temp.path().join("session.drfrec");
        let snapshot = temp.path().join("session.store.json");
        std::fs::write(&live, b"{\"live\":true}").expect("write live store");
        std::fs::write(&snapshot, b"{\"snapshot\":true}").expect("write snapshot");
        let hash = sha256_hex(b"{\"snapshot\":true}");
        let text = format!(
            "{{\"v\":1,\"kind\":\"header\",\"account\":\"s1player\",\
             \"store_path\":\"{}\",\"store_sha256\":\"{hash}\"}}\n",
            live.display()
        );
        std::fs::write(&recording_path, text).expect("write recording");

        let recording = Recording::load(&recording_path).expect("load recording");
        let resolved = resolve_store_source(&recording, &CaptureOptions::default())
            .expect("snapshot fallback");
        assert_eq!(resolved, snapshot);

        let override_path = temp.path().join("override.json");
        let options = CaptureOptions {
            store_path: Some(override_path.clone()),
            ..CaptureOptions::default()
        };
        assert_eq!(
            resolve_store_source(&recording, &options).expect("--store override"),
            override_path
        );

        std::fs::remove_file(&snapshot).expect("remove snapshot");
        let error = resolve_store_source(&recording, &CaptureOptions::default())
            .expect_err("a missing snapshot must be an error");
        assert!(error.contains("no snapshot"), "unexpected error: {error}");
    }
}
