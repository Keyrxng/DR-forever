// SPDX-License-Identifier: GPL-3.0-or-later

//! drf-server binary: bind the auth and game ports and run.
//!
//! CLI only (no environment flags):
//!   --auth-port <u16>   auth listener port (default 2110)
//!   --game-port <u16>   game listener port (default 3503)
//!   --advertised-game-port <u16>  game port advertised in the auth
//!                       ServerList (default: the bound --game-port value)
//!   --dir <path>        persistence directory, holds `s1_store.json`
//!   --game-ip <ipv4>    address advertised in the auth ServerList
//!   --game-pki <path>   `game.pki`, with `--game-pkg`, for class content
//!   --game-pkg <path>   `game.pkg`
//!   --trust-client-sim  trust the paired client hook's per-entity
//!                       EntitySynchInfo::Validate bypass: re-enable the owner
//!                       move-ack in combat zones, and listen for mob death/HP
//!                       telemetry on udp :3504 (default off)

use std::path::PathBuf;
use std::process::ExitCode;

use drf_server::server::Server;
use drf_server::store::Store;

struct Options {
    auth_port: u16,
    game_port: u16,
    advertised_game_port: Option<u16>,
    dir: PathBuf,
    game_ip: String,
    game_pki: Option<PathBuf>,
    game_pkg: Option<PathBuf>,
    zone_start: Option<(i64, i64)>,
    trust_client_sim: bool,
    admin: bool,
}

fn main() -> ExitCode {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("drf-server: {message}");
            return ExitCode::from(2);
        }
    };

    if let Some((x, y)) = options.zone_start {
        drf_content::set_generated_pose_override(x, y);
    }

    let store_path = options.dir.join("s1_store.json");
    let store = match Store::open(&store_path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("drf-server: cannot open {}: {error}", store_path.display());
            return ExitCode::FAILURE;
        }
    };
    // Unknown inbound frame shapes are captured to
    // `<dir>/unhandled-opcodes.json` so a protocol gap is visible instead of
    // silently dropped.
    drf_server::opcodes::init(&options.dir);

    let content_paths = match (options.game_pki, options.game_pkg) {
        (Some(pki), Some(pkg)) => {
            // Fail fast on a bad pair: the same archive is reopened to derive
            // each class's content on demand.
            match drf_content::Archive::open(&pki, &pkg) {
                Ok(archive) => {
                    let _ = drf_content::Corpus::new(archive);
                    Some((pki, pkg))
                }
                Err(error) => {
                    eprintln!(
                        "drf-server: cannot open {} / {}: {error}",
                        pki.display(),
                        pkg.display()
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
        (None, None) => None,
        _ => {
            eprintln!("drf-server: --game-pki and --game-pkg must be given together");
            return ExitCode::from(2);
        }
    };

    let server = match Server::new(
        &format!("0.0.0.0:{}", options.auth_port),
        &format!("0.0.0.0:{}", options.game_port),
        &options.game_ip,
        store,
        content_paths,
    ) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("drf-server: cannot bind: {error}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(advertised) = options.advertised_game_port {
        server.set_advertised_game_port(advertised);
    }

    if options.trust_client_sim {
        server.set_trust_client_sim(true);
    }
    if options.admin {
        server.set_admin_commands(true);
    }
    // Generated dungeons get a fresh per-entry maze seed; the client builds
    // from the Assign word and the server derives the same layout from it.
    server.set_random_maze_seeds(true);
    server.set_content_loot(true);

    println!(
        "drf-server up: auth=:{} game=:{} advertised_game=:{} game_ip={} dir={} admin={}",
        server.auth_port(),
        server.game_port(),
        server.advertised_game_port(),
        options.game_ip,
        options.dir.display(),
        server.admin_commands()
    );
    server.serve();
    ExitCode::SUCCESS
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        auth_port: 2110,
        game_port: 3503,
        advertised_game_port: None,
        dir: PathBuf::from("./data"),
        game_ip: "127.0.0.1".to_string(),
        game_pki: None,
        game_pkg: None,
        zone_start: None,
        trust_client_sim: false,
        admin: false,
    };
    let mut args = args;
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--auth-port" => options.auth_port = parse_port(&value()?)?,
            "--game-port" => options.game_port = parse_port(&value()?)?,
            "--advertised-game-port" => options.advertised_game_port = Some(parse_port(&value()?)?),
            "--dir" => options.dir = PathBuf::from(value()?),
            "--game-ip" => options.game_ip = value()?,
            "--game-pki" => options.game_pki = Some(PathBuf::from(value()?)),
            "--game-pkg" => options.game_pkg = Some(PathBuf::from(value()?)),
            "--trust-client-sim" => options.trust_client_sim = true,
            "--admin" => options.admin = true,
            "--zone-start" => {
                let raw = value()?;
                let (x, y) = raw
                    .split_once(',')
                    .ok_or_else(|| format!("--zone-start needs X,Y, got {raw}"))?;
                let x = x
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "bad --zone-start X".to_string())?;
                let y = y
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "bad --zone-start Y".to_string())?;
                options.zone_start = Some((x, y));
            }
            "-h" | "--help" => {
                return Err("usage: drf-server [--auth-port N] [--game-port N] \
                    [--advertised-game-port N] [--dir PATH] [--game-ip IPV4] \
                    [--game-pki PATH --game-pkg PATH] [--trust-client-sim] [--admin]"
                    .to_string())
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(options)
}

fn parse_port(value: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("invalid port {value}"))
}
