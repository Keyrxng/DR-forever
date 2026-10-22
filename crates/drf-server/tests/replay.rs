// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end replay + first-divergence harness test.
//!
//! The test authors a SHORT synthetic `.drfrec`: a reference server (ticker
//! off) is driven with the same select/join frames `tests/server.rs` uses and
//! its raw s2c transcript is captured, then written as the recording's expected
//! s2c. Replaying that recording must agree, two runs must be byte-identical,
//! and a deliberately-corrupted expected s2c must make the `replay` binary exit
//! non-zero.
//!
//! Skips cleanly when no `game.pkg` is present, exactly as `tests/server.rs`.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use drf_proto::select_payload;
use drf_server::replay::{self, CaptureOptions, Recording};
use drf_server::server::Server;
use drf_server::session::CONNECT_ACK;
use drf_server::store::{Appearance, Store};

const ACCOUNT: &str = "s1player";
const NAME: &str = "Hero";
const CLASS: &str = "avatar.classes.WarlockFemale";

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
        path.push(format!(
            "drf-replay-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
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

fn connect(port: u16) -> TcpStream {
    let mut last = None;
    for _ in 0..100 {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .expect("set read timeout");
                return stream;
            }
            Err(error) => {
                last = Some(error);
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    panic!("could not connect to port {port}: {last:?}");
}

/// Read a socket until the peer closes (or the deadline passes).
fn read_to_close(stream: &mut TcpStream, deadline: Instant) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => out.extend_from_slice(&chunk[..read]),
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
    out
}

/// Drive a fresh server with `script` and return `(raw c2s, raw s2c)`.
fn reference_session(
    store_path: &Path,
    content: &(PathBuf, PathBuf),
    script: &[(u8, u8, Vec<u8>)],
) -> (Vec<u8>, Vec<u8>) {
    let store = Store::open(store_path).expect("open reference store");
    let server = Server::new(
        "127.0.0.1:0",
        "127.0.0.1:0",
        "127.0.0.1",
        store,
        Some(content.clone()),
    )
    .expect("bind reference server");
    server.set_ticker_enabled(false);
    let game_port = server.game_port();
    thread::spawn(move || server.serve());

    let mut stream = connect(game_port);
    let mut c2s = replay::client_hello();
    stream.write_all(&c2s).expect("send hello");
    for (channel, mtype, payload) in script {
        let segment = replay::client_segment(*channel, *mtype, payload);
        c2s.extend_from_slice(&segment);
        stream.write_all(&segment).expect("send segment");
    }
    // Close the write half: the server drains every c2s frame, answers, then
    // closes, so the capture has a deterministic end.
    stream.shutdown(Shutdown::Write).expect("shutdown write");
    let s2c = read_to_close(&mut stream, Instant::now() + Duration::from_secs(60));
    (c2s, s2c)
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Serialize a recording as `.drfrec` v1 JSONL.
fn write_recording(path: &Path, store_path: &Path, c2s: &[u8], s2c: &[u8]) {
    let zero = "0".repeat(64);
    let store = json_escape(&store_path.display().to_string());
    let store_sha = replay::sha256_hex(&fs::read(store_path).expect("read store for hash"));
    let mut out = String::new();
    out.push_str(&format!(
        "{{\"v\":1,\"kind\":\"header\",\"created\":1700000000,\
\"client_build_sha256\":\"{zero}\",\"account\":\"{ACCOUNT}\",\"sqlid\":1,\
\"char_name\":\"{NAME}\",\"char_class\":\"{CLASS}\",\"zone\":\"tutorial\",\
\"seed\":\"0x00000000\",\"store_path\":\"{store}\",\"store_sha256\":\"{store_sha}\",\
\"auth_listen\":22110,\"auth_target\":22111,\"game_listen\":23503,\
\"game_target\":23504}}\n"
    ));
    let mut t = 0i64;
    for chunk in c2s.chunks(7) {
        t += 3;
        out.push_str(&format!(
            "{{\"v\":1,\"kind\":\"frame\",\"t\":{t},\"dir\":\"c2s\",\"port\":\"game\",\
\"hex\":\"{}\"}}\n",
            replay::to_hex(chunk)
        ));
    }
    for chunk in s2c.chunks(7) {
        t += 3;
        out.push_str(&format!(
            "{{\"v\":1,\"kind\":\"frame\",\"t\":{t},\"dir\":\"s2c\",\"port\":\"game\",\
\"hex\":\"{}\"}}\n",
            replay::to_hex(chunk)
        ));
    }
    out.push_str("{\"v\":1,\"kind\":\"marker\",\"t\":1,\"event\":\"session.start\"}\n");
    fs::write(path, out).expect("write recording");
}

/// `game.pkg`'s directory, as the content crate's tests locate it.
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

/// The corruption rebuilds the expected s2c with one message's last byte
/// flipped, keeping every frame structurally valid so the diff (not framing)
/// catches it.
fn corrupt_s2c(s2c: &[u8]) -> Vec<u8> {
    let framed = replay::frame_server_transcript(s2c).expect("frame reference s2c");
    let mut messages: Vec<Vec<u8>> = framed.iter().map(|m| m.inner.clone()).collect();
    assert!(!messages.is_empty(), "reference session sent no messages");
    let first = &mut messages[0];
    let last = first.len() - 1;
    first[last] ^= 0xFF;
    let mut out = CONNECT_ACK.to_vec();
    for message in &messages {
        out.extend_from_slice(&replay::server_segment(message));
    }
    out
}

#[test]
fn replay_agrees_is_deterministic_and_catches_corruption() {
    let Some(dir) = game_pkg_dir() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let content = (dir.join("game.pki"), dir.join("game.pkg"));

    let temp = TempDir::new("harness");
    let store_path = temp.path().join("s1_store.json");
    let mut store = Store::open(&store_path).expect("open store");
    store.ensure_account(ACCOUNT).expect("ensure account");
    let sqlid = store
        .create_char(ACCOUNT, NAME, CLASS, Appearance::default())
        .expect("seed character");
    drop(store);

    let script = vec![
        (0x04u8, 0x05u8, select_payload(sqlid)),
        (0x0Du8, 0x06u8, Vec::new()),
    ];
    let (c2s, s2c) = reference_session(&store_path, &content, &script);
    assert!(
        s2c.starts_with(&CONNECT_ACK),
        "reference s2c opens on the ack"
    );
    assert!(
        s2c.len() > CONNECT_ACK.len() + 8,
        "reference s2c has at least one segment"
    );

    let good = temp.path().join("good.drfrec");
    write_recording(&good, &store_path, &c2s, &s2c);
    let corrupt = temp.path().join("corrupt.drfrec");
    write_recording(&corrupt, &store_path, &c2s, &corrupt_s2c(&s2c));

    // -- library: record -> replay -> diff agrees, and two runs are identical --
    let recording = Recording::load(&good).expect("load good recording");
    let options = CaptureOptions {
        content_paths: Some(content.clone()),
        ..CaptureOptions::default()
    };
    let first = replay::run(&recording, &options).expect("first replay");
    assert!(
        first.agrees(),
        "expected agreement:\n{}",
        first
            .divergence
            .as_ref()
            .map(|d| d.render())
            .unwrap_or_default()
    );
    assert!(first.got_len > 0, "the replay captured messages");
    let second = replay::run(&recording, &options).expect("second replay");
    assert_eq!(
        first.sha256, second.sha256,
        "two replays must be byte-identical"
    );

    // -- library: a corrupted expected s2c diverges --------------------------
    let corrupt_recording = Recording::load(&corrupt).expect("load corrupt recording");
    let bad = replay::run(&corrupt_recording, &options).expect("corrupt replay");
    assert!(!bad.agrees(), "corruption must be detected");
    assert_eq!(bad.divergence.as_ref().map(|d| d.index), Some(0));

    // -- binary: exit codes prove the harness contract -----------------------
    let bin = env!("CARGO_BIN_EXE_replay");
    let pki = content.0.display().to_string();
    let pkg = content.1.display().to_string();

    let good_status = Command::new(bin)
        .args([
            "--rec",
            good.to_str().unwrap(),
            "--twice",
            "--game-pki",
            &pki,
            "--game-pkg",
            &pkg,
        ])
        .status()
        .expect("run replay on good recording");
    assert!(
        good_status.success(),
        "good recording + --twice must exit 0"
    );

    let corrupt_status = Command::new(bin)
        .args([
            "--rec",
            corrupt.to_str().unwrap(),
            "--game-pki",
            &pki,
            "--game-pkg",
            &pkg,
        ])
        .status()
        .expect("run replay on corrupt recording");
    assert!(
        !corrupt_status.success(),
        "corrupt recording must exit non-zero"
    );
}
