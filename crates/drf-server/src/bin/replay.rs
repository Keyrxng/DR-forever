// SPDX-License-Identifier: GPL-3.0-or-later

//! `replay`: run a recorded `.drfrec` session through the real server and diff
//! the resulting s2c transcript against the recording.
//!
//! Usage:
//!   replay --rec <file.drfrec> [--store PATH] [--seed HEX] [--twice]
//!          [--verbose] [--ticker] [--raw]
//!          [--game-pki PATH --game-pkg PATH] [--timeout-ms N]
//!
//! Exits 0 when the transcripts agree; 1 on any divergence; 2 on a usage or
//! setup error. `--twice` additionally runs the capture twice and fails when
//! the two captures are not byte-identical (the determinism gate).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use drf_server::replay::{CaptureOptions, Recording, ReplayReport};

struct Options {
    rec: PathBuf,
    store: Option<PathBuf>,
    seed: Option<u32>,
    twice: bool,
    verbose: bool,
    ticker: bool,
    raw: bool,
    game_pki: Option<PathBuf>,
    game_pkg: Option<PathBuf>,
    capture_timeout_ms: u64,
}

fn main() -> ExitCode {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("replay: {message}");
            return ExitCode::from(2);
        }
    };

    let recording = match Recording::load(&options.rec) {
        Ok(recording) => recording,
        Err(error) => {
            eprintln!("replay: {error}");
            return ExitCode::from(2);
        }
    };

    let content_paths = match content_paths(&options) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("replay: {error}");
            return ExitCode::from(2);
        }
    };

    let capture_options = CaptureOptions {
        store_path: options.store.clone(),
        seed: options.seed,
        content_paths,
        ticker: options.ticker,
        solicited_only: !options.raw,
        read_timeout: Duration::from_millis(options.capture_timeout_ms),
    };

    if options.verbose {
        eprintln!(
            "replay: {} game c2s frames, {} game s2c frames, ticker={}, solicited_only={}",
            recording
                .frames
                .iter()
                .filter(|f| f.port == drf_server::replay::Port::Game
                    && f.dir == drf_server::replay::Direction::ClientToServer)
                .count(),
            recording
                .frames
                .iter()
                .filter(|f| f.port == drf_server::replay::Port::Game
                    && f.dir == drf_server::replay::Direction::ServerToClient)
                .count(),
            options.ticker,
            !options.raw,
        );
    }

    let report = match drf_server::replay::run(&recording, &capture_options) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("replay: {error}");
            return ExitCode::FAILURE;
        }
    };
    describe(&report, &options);

    if options.twice {
        // `--twice` runs a second capture and asserts byte-identity.
        let second = match drf_server::replay::run(&recording, &capture_options) {
            Ok(report) => report,
            Err(error) => {
                eprintln!("replay: second capture failed: {error}");
                return ExitCode::FAILURE;
            }
        };
        if second.sha256 != report.sha256 {
            eprintln!(
                "replay: DETERMINISM FAILED: run1 sha256={} run2 sha256={}",
                report.sha256, second.sha256
            );
            return ExitCode::FAILURE;
        }
        println!(
            "replay: determinism OK: two runs produced identical s2c (sha256 {})",
            report.sha256
        );
    }

    if report.agrees() {
        println!(
            "replay: AGREE ({} messages, sha256 {})",
            report.got_len, report.sha256
        );
        ExitCode::SUCCESS
    } else {
        if let Some(divergence) = &report.divergence {
            eprintln!("{}", divergence.render());
        }
        ExitCode::FAILURE
    }
}

fn describe(report: &ReplayReport, options: &Options) {
    if report.auth_frames_skipped > 0 {
        eprintln!(
            "replay: note: {} recorded auth-port frames skipped (P0 replays the game port only)",
            report.auth_frames_skipped
        );
    }
    if report.world_intervals_dropped > 0 {
        eprintln!(
            "replay: note: {} `(07,0D)` WorldInterval ticker frames dropped before diffing \
             (wall-clock paced; use --raw to compare them)",
            report.world_intervals_dropped
        );
    }
    if options.verbose {
        eprintln!(
            "replay: expected {} messages, got {} (ticker={}, solicited_only={})",
            report.expected_len, report.got_len, options.ticker, !options.raw
        );
    }
}

/// Explicit `--game-pki/--game-pkg`, else auto-detect a client tree beside the
/// repository root (the `tests/server.rs` pattern). `None` serves bare nodes.
fn content_paths(options: &Options) -> Result<Option<(PathBuf, PathBuf)>, String> {
    match (&options.game_pki, &options.game_pkg) {
        (Some(pki), Some(pkg)) => Ok(Some((pki.clone(), pkg.clone()))),
        (None, None) => {
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
            let repo = manifest.ancestors().nth(4);
            let found = repo.and_then(|repo| {
                [
                    repo.join("dr-client-666-testing"),
                    repo.join("Dungeon_Runners_Client_666"),
                ]
                .into_iter()
                .find(|dir| dir.join("game.pkg").is_file())
            });
            Ok(found.map(|dir| (dir.join("game.pki"), dir.join("game.pkg"))))
        }
        _ => Err("--game-pki and --game-pkg must be given together".to_string()),
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut rec: Option<PathBuf> = None;
    let mut options = Options {
        rec: PathBuf::new(),
        store: None,
        seed: None,
        twice: false,
        verbose: false,
        ticker: false,
        raw: false,
        game_pki: None,
        game_pkg: None,
        capture_timeout_ms: 30_000,
    };
    let mut args = args;
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--rec" => rec = Some(PathBuf::from(value()?)),
            "--store" => options.store = Some(PathBuf::from(value()?)),
            "--seed" => {
                let raw = value()?;
                let seed =
                    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
                        u32::from_str_radix(hex, 16).map_err(|_| format!("bad --seed hex {raw}"))?
                    } else {
                        raw.parse::<u32>()
                            .map_err(|_| format!("bad --seed {raw}"))?
                    };
                options.seed = Some(seed);
            }
            "--twice" => options.twice = true,
            "--verbose" | "-v" => options.verbose = true,
            "--ticker" => options.ticker = true,
            "--raw" => options.raw = true,
            "--game-pki" => options.game_pki = Some(PathBuf::from(value()?)),
            "--game-pkg" => options.game_pkg = Some(PathBuf::from(value()?)),
            "--timeout-ms" => {
                options.capture_timeout_ms = value()?
                    .parse::<u64>()
                    .map_err(|_| "--timeout-ms needs an integer".to_string())?;
            }
            "-h" | "--help" => {
                return Err("usage: replay --rec <file.drfrec> [--store PATH] \
                    [--seed HEX] [--twice] [--verbose] [--ticker] [--raw] \
                    [--game-pki PATH --game-pkg PATH] [--timeout-ms N]"
                    .to_string())
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    options.rec = rec.ok_or_else(|| "--rec <file.drfrec> is required".to_string())?;
    Ok(options)
}
