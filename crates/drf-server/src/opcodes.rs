// SPDX-License-Identifier: GPL-3.0-or-later

//! Unhandled-opcode telemetry.
//!
//! Every inbound frame funnels through `session`'s `dispatch`, so an unknown
//! `(channel, type)` or an unparsed sub-op used to vanish silently. This
//! registry gives each distinct unknown a stable key, a count, its first and
//! last sighting, the payload lengths seen, a deduplicated sample set and the
//! zone it appeared in.
//!
//! Two durable surfaces are written to `<dir>`:
//!
//! * `unhandled-opcodes.jsonl` — append-only, **one line per occurrence** with
//!   the full payload hex, monotonic sequence number and millisecond timestamp.
//!   This is the forensic record: it preserves ordering and every variant.
//! * `unhandled-opcodes.json` — an atomically rewritten summary keyed by
//!   `channel:type[:sub]`, with counts and bounded samples.
//!
//! Standard-library only. A known key costs one mutex lock, one append and a
//! couple of integer writes. Memory is bounded (`SAMPLES_PER_KEY` distinct
//! payloads per key); the JSONL is not, but is rotated by the operator.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Distinct full payloads retained in the summary per key.
const SAMPLES_PER_KEY: usize = 64;

/// One distinct unhandled frame shape.
#[derive(Clone, Debug)]
pub struct Entry {
    pub key: String,
    pub context: &'static str,
    pub channel: u8,
    pub mtype: u8,
    pub sub: Option<u8>,
    pub count: u64,
    pub first_seen: u64,
    pub last_seen: u64,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub zone: String,
    pub lengths: BTreeSet<usize>,
    pub samples: Vec<String>,
    pub samples_truncated: bool,
}

static REGISTRY: Mutex<BTreeMap<String, Entry>> = Mutex::new(BTreeMap::new());
static SUMMARY: OnceLock<PathBuf> = OnceLock::new();
static JOURNAL: OnceLock<Mutex<File>> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Point the registry at the server's persistence directory.
pub fn init(dir: &Path) {
    let summary = dir.join("unhandled-opcodes.json");
    if SUMMARY.set(summary).is_err() {
        return;
    }
    let journal = dir.join("unhandled-opcodes.jsonl");
    match OpenOptions::new().create(true).append(true).open(&journal) {
        Ok(file) => {
            let _ = JOURNAL.set(Mutex::new(file));
        }
        Err(error) => {
            eprintln!(
                "opcodes: cannot open journal {}: {error}; continuing without it",
                journal.display()
            );
        }
    }
}

fn key_for(channel: u8, mtype: u8, sub: Option<u8>) -> String {
    match sub {
        Some(sub) => format!("{channel:02x}:{mtype:02x}:{sub:02x}"),
        None => format!("{channel:02x}:{mtype:02x}"),
    }
}

fn hex(payload: &[u8]) -> String {
    let mut out = String::with_capacity(payload.len() * 2);
    for byte in payload {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn append_journal(line: &str) {
    let Some(journal) = JOURNAL.get() else {
        return;
    };
    let mut file = match journal.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    };
    if file.write_all(line.as_bytes()).is_err() || file.write_all(b"\n").is_err() {
        return;
    }
    let _ = file.flush();
}

/// Record one unhandled frame. `context` names the dispatch leg
/// (`dispatch`, `entity_channel`, `entity_request`) and `sub` is the sub-opcode
/// when the frame is a request envelope.
pub fn record(
    context: &'static str,
    channel: u8,
    mtype: u8,
    sub: Option<u8>,
    payload: &[u8],
    zone: &str,
) {
    let key = key_for(channel, mtype, sub);
    let ts_ms = now_ms();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let payload_hex = hex(payload);

    append_journal(&format!(
        "{{\"seq\":{seq},\"ts_ms\":{ts_ms},\"key\":\"{key}\",\"context\":\"{context}\",\
         \"channel\":{channel},\"type\":{mtype},\"sub\":{},\"zone\":\"{}\",\"len\":{},\
         \"hex\":\"{}\"}}",
        match sub {
            Some(sub) => sub.to_string(),
            None => "null".to_string(),
        },
        json_escape(zone),
        payload.len(),
        payload_hex
    ));

    let mut map = match REGISTRY.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    };
    let now_s = ts_ms / 1000;
    if let Some(entry) = map.get_mut(&key) {
        entry.count += 1;
        entry.last_seen = now_s;
        entry.last_seen_ms = ts_ms;
        entry.lengths.insert(payload.len());
        if entry.samples.len() < SAMPLES_PER_KEY {
            if !entry.samples.iter().any(|sample| sample == &payload_hex) {
                entry.samples.push(payload_hex);
            }
        } else if !entry.samples.iter().any(|sample| sample == &payload_hex) {
            entry.samples_truncated = true;
        }
    } else {
        let samples = vec![payload_hex.clone()];
        let mut lengths = BTreeSet::new();
        lengths.insert(payload.len());
        let entry = Entry {
            key: key.clone(),
            context,
            channel,
            mtype,
            sub,
            count: 1,
            first_seen: now_s,
            last_seen: now_s,
            first_seen_ms: ts_ms,
            last_seen_ms: ts_ms,
            zone: zone.to_string(),
            lengths,
            samples,
            samples_truncated: false,
        };
        eprintln!(
            "UNHANDLED {key} ctx={context} zone={zone} {}B hex={} -- no handler; recorded",
            payload.len(),
            payload_hex
        );
        map.insert(key, entry);
    }
    drop(map);
    flush();
}

/// Rewrite `<dir>/unhandled-opcodes.json` atomically with the current registry.
pub fn flush() {
    let Some(path) = SUMMARY.get() else {
        return;
    };
    let snapshot = {
        let map = match REGISTRY.lock() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        };
        snapshot_json(&map, now_ms() / 1000)
    };
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, snapshot).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

fn snapshot_json(map: &BTreeMap<String, Entry>, updated_at: u64) -> String {
    let mut out = String::from("{\n  \"updated_at\": ");
    out.push_str(&updated_at.to_string());
    out.push_str(",\n  \"unhandled\": [\n");
    for (index, entry) in map.values().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str("    {");
        out.push_str(&format!("\"key\":\"{}\",", json_escape(&entry.key)));
        out.push_str(&format!("\"context\":\"{}\",", json_escape(entry.context)));
        out.push_str(&format!("\"channel\":{},", entry.channel));
        out.push_str(&format!("\"type\":{},", entry.mtype));
        match entry.sub {
            Some(sub) => out.push_str(&format!("\"sub\":{sub},")),
            None => out.push_str("\"sub\":null,"),
        }
        out.push_str(&format!("\"count\":{},", entry.count));
        out.push_str(&format!("\"first_seen\":{},", entry.first_seen));
        out.push_str(&format!("\"last_seen\":{},", entry.last_seen));
        out.push_str(&format!("\"first_seen_ms\":{},", entry.first_seen_ms));
        out.push_str(&format!("\"last_seen_ms\":{},", entry.last_seen_ms));
        out.push_str(&format!("\"zone\":\"{}\",", json_escape(&entry.zone)));
        let lengths: Vec<String> = entry.lengths.iter().map(|n| n.to_string()).collect();
        out.push_str(&format!("\"lengths\":[{}],", lengths.join(",")));
        out.push_str(&format!(
            "\"samples_truncated\":{},",
            entry.samples_truncated
        ));
        out.push_str("\"samples\":[");
        for (sample_index, sample) in entry.samples.iter().enumerate() {
            if sample_index > 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(sample);
            out.push('"');
        }
        out.push(']');
        out.push('}');
    }
    out.push_str("\n  ]\n}\n");
    out
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
pub fn snapshot_for_test() -> String {
    let map = match REGISTRY.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    };
    snapshot_json(&map, 0)
}

#[cfg(test)]
pub fn clear_for_test() {
    if let Ok(mut map) = REGISTRY.lock() {
        map.clear();
    }
    SEQ.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global; these tests must not clear it while a
    /// sibling is recording into it.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    #[test]
    fn dedupes_and_counts_by_key() {
        let _guard = serial();
        clear_for_test();
        record("dispatch", 0x07, 0x34, Some(0x1a), &[1, 2, 3], "z");
        record("dispatch", 0x07, 0x34, Some(0x1a), &[4, 5], "z");
        let json = snapshot_for_test();
        assert!(json.contains("\"key\":\"07:34:1a\""), "{json}");
        assert!(json.contains("\"count\":2"), "{json}");
        assert!(json.contains("\"channel\":7"), "{json}");
        assert!(json.contains("\"sub\":26"), "{json}");
        assert!(json.contains("\"samples\":[\"010203\",\"0405\"]"), "{json}");
        assert!(json.contains("\"lengths\":[2,3]"), "{json}");
    }

    #[test]
    fn identical_payloads_do_not_duplicate_samples() {
        let _guard = serial();
        clear_for_test();
        record("entity_channel", 0x07, 0x34, Some(0x28), &[0xaa; 7], "z");
        record("entity_channel", 0x07, 0x34, Some(0x28), &[0xaa; 7], "z");
        let json = snapshot_for_test();
        assert!(json.contains("\"count\":2"), "{json}");
        assert_eq!(json.matches("\"aaaaaaaaaaaaaa\"").count(), 1, "{json}");
    }

    #[test]
    fn full_payload_is_kept() {
        let _guard = serial();
        clear_for_test();
        let payload: Vec<u8> = (0..200).collect();
        record("entity_channel", 0x07, 0x34, Some(0x55), &payload, "z");
        let json = snapshot_for_test();
        assert!(json.contains("\"lengths\":[200]"), "{json}");
        assert!(!json.contains(".."), "{json}");
        assert_eq!(json.matches("\"000102").count(), 1, "{json}");
    }

    #[test]
    fn samples_are_bounded_per_key() {
        let _guard = serial();
        clear_for_test();
        for value in 0..(SAMPLES_PER_KEY as u16 + 4) {
            let bytes = value.to_le_bytes();
            record("entity_channel", 0x07, 0x34, Some(0x66), &bytes, "z");
        }
        let json = snapshot_for_test();
        assert!(json.contains("\"samples_truncated\":true"), "{json}");
        assert!(json.contains("\"count\":68"), "{json}");
    }
}
