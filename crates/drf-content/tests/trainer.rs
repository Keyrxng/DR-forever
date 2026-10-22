// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::path::{Path, PathBuf};

use drf_content::{derive_trainer, skill_value_per_level, Archive};

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
fn town_trainers_author_their_component_and_rows_from_content() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    for (npc, component, min_rows) in [
        (
            "world.town.npc.TrainerFighter",
            "world.town.npc.base.TrainerFighterBase.SkillTrainer",
            20,
        ),
        (
            "world.town.npc.TrainerMage",
            "world.town.npc.base.TrainerMageBase.SkillTrainer",
            20,
        ),
        (
            "world.town.npc.TrainerRanger",
            "world.town.npc.base.TrainerRangerBase.SkillTrainer",
            18,
        ),
    ] {
        let catalog = derive_trainer(&archive, npc)
            .expect("trainer derivation")
            .expect("trainer block");
        assert_eq!(catalog.component_class, component);
        assert!(
            catalog.skills.len() >= min_rows,
            "{npc} rows: {}",
            catalog.skills.len()
        );
        assert!(catalog.skills.iter().all(|skill| {
            skill.hash != 0
                && skill.gold_value_mod > 0.0
                && skill.required_level > 0
                && skill.max_level > 0
        }));
    }
    let knob = skill_value_per_level(&archive).expect("knob");
    assert!(knob.is_some_and(|value| (value - 1113.621).abs() < 0.001));
}

#[test]
fn fire_bolt_row_is_the_authored_price_and_passive_rule() {
    let Some(archive) = archive() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let catalog = derive_trainer(&archive, "world.town.npc.TrainerMage")
        .expect("trainer")
        .expect("mage trainer");
    let fire_bolt = catalog
        .skills
        .iter()
        .find(|skill| skill.fqn.eq_ignore_ascii_case("skills.generic.FireBolt"))
        .expect("FireBolt is offered");
    assert_eq!(fire_bolt.required_level, 3);
    assert_eq!(fire_bolt.max_level, 20);
    assert!(!fire_bolt.passive);
    let value = skill_value_per_level(&archive).expect("knob").unwrap();
    assert_eq!(drf_content::train_cost(value, fire_bolt, 1), 3340);
    assert_eq!(drf_content::train_cost(value, fire_bolt, 2), 8908);
    let fighter = derive_trainer(&archive, "world.town.npc.TrainerFighter")
        .expect("trainer")
        .expect("fighter trainer");
    let passive = fighter
        .skills
        .iter()
        .find(|skill| {
            skill
                .fqn
                .eq_ignore_ascii_case("skills.generic.FighterClassPassive")
        })
        .expect("class passive is offered");
    assert!(passive.passive);
    assert_eq!(passive.profession.as_deref(), Some("FIGHTER"));
}
