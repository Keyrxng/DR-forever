// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::path::{Path, PathBuf};

use drf_content::{bling_gnome_profile, Archive, BLING_GNOME_SKILL, BLING_GNOME_SUMMON};

fn archive() -> Option<Archive> {
    let (pki, pkg) =
        if let (Ok(pki), Ok(pkg)) = (env::var("DRF_GAME_PKI"), env::var("DRF_GAME_PKG")) {
            (PathBuf::from(pki), PathBuf::from(pkg))
        } else {
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
            let repo = manifest.ancestors().nth(4)?;
            let dir = repo.join("dr-client-666-testing");
            (dir.join("game.pki"), dir.join("game.pkg"))
        };
    if !pki.is_file() || !pkg.is_file() {
        return None;
    }
    Archive::open(&pki, &pkg).ok()
}

#[test]
fn the_skill_and_summon_contract_is_read_from_content() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let profile = bling_gnome_profile(&archive)
        .expect("profile derivation")
        .expect("Bling Gnome content");
    assert_eq!(profile.skill, BLING_GNOME_SKILL);
    assert_eq!(profile.summon, BLING_GNOME_SUMMON);
    assert_eq!(
        profile.behavior,
        "creatures.summon.blinggnome.base.BlingGnome_Summon.Behavior"
    );
    assert!((profile.cooldown_seconds - 45.0).abs() < 0.001);
    assert!((profile.gold_value_mod - 2.3).abs() < 0.001);
    assert!((profile.effect_duration_seconds - 10.0).abs() < 0.001);
    assert_eq!(profile.search_radius, 200);
    assert_eq!(profile.search_range, 250);
    assert!((profile.conversion_ratio - 0.5).abs() < 0.001);
    assert!((profile.speed - 50.0).abs() < 0.001);
    assert!((profile.max_health - 0.3).abs() < 0.001);
}
