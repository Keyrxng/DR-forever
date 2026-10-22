// SPDX-License-Identifier: GPL-3.0-or-later

//! `drf-content`'s DERIVED quest corpus against the real `game.pkg`.
//!
//! The pinned directory and quest record are fixed test data.
//!
//! No skip arm when `DRF_GAME_PKG`/`DRF_GAME_PKI` are set. Without them the
//! test looks where the sibling `game_pkg.rs` test looks, and skips only when
//! this machine has no game.pkg.

use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};

use drf_content::dfc_hash;
use drf_content::{Archive, Corpus, GiverDirection, QuestCorpus};

const Q11_A1: &str = "world.dungeon00.quest.Q11_a1";
const HELPER: &str = "world.tutorial.npc.HelperNoobosaur00";

fn pkg_paths() -> Option<(PathBuf, PathBuf)> {
    if let (Ok(pkg), Ok(pki)) = (env::var("DRF_GAME_PKG"), env::var("DRF_GAME_PKI")) {
        let pkg = PathBuf::from(pkg);
        let pki = PathBuf::from(pki);
        if pkg.is_file() && pki.is_file() {
            return Some((pki, pkg));
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.ancestors().nth(4)?;
    for dir in [
        repo.join("dr-client-666-testing"),
        repo.join("Dungeon_Runners_Client_666"),
        repo.join("dr-play"),
    ] {
        if dir.join("game.pkg").is_file() && dir.join("game.pki").is_file() {
            return Some((dir.join("game.pki"), dir.join("game.pkg")));
        }
    }
    None
}

fn quest_corpus() -> Option<QuestCorpus> {
    let (pki, pkg) = pkg_paths()?;
    let archive = Archive::open(&pki, &pkg).expect("open game.pki/game.pkg");
    let mut corpus = Corpus::new(archive);
    Some(QuestCorpus::from_corpus(&mut corpus).expect("derive quest corpus"))
}

#[test]
fn tutorial_level_one_listed_directory_is_the_derived_entry_quest() {
    let Some(quests) = quest_corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };

    let npcs = quests.zone_npcs("tutorial");
    assert_eq!(
        npcs,
        vec![
            "world.tutorial.npc.Recruiter1",
            "world.tutorial.npc.Recruiter2",
            "world.tutorial.npc.HermitVendor",
            "world.tutorial.npc.Adventurer1",
            "world.tutorial.npc.Adventurer2",
            "world.tutorial.npc.Adventurer3",
            "world.tutorial.npc.Guard",
            "world.tutorial.npc.HelperNoobosaur00",
        ],
        "tutorial placement roster"
    );

    let completed = HashSet::new();
    let groups = quests.offer_groups(1, &npcs, None, GiverDirection::Listed, Some(&completed));
    assert_eq!(groups.len(), 1, "exactly one giver at level 1");
    assert_eq!(groups[0].0, HELPER);
    assert_eq!(groups[0].1.len(), 1, "one offer from the giver");
    assert_eq!(groups[0].1[0].fqn, Q11_A1);

    // The accept edge, and only the accept edge: Q11_a1's own `NPC` keys name
    // Adventurer3 and Recruiter1, which are its TURN-IN and not its listers.
    assert_eq!(quests.quest_by_fqn(Q11_A1).unwrap().fqn, Q11_A1);
}

#[test]
fn the_level_window_alone_offers_the_larger_tutorial_set() {
    let Some(quests) = quest_corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let npcs = quests.zone_npcs("tutorial");

    let groups = quests.offer_groups(1, &npcs, None, GiverDirection::Listed, None);
    let total: usize = groups.iter().map(|(_, offered)| offered.len()).sum();
    assert_eq!(total, 11, "level-window-only offer count");
    assert_eq!(groups.len(), 7, "level-window-only giver count");

    let counts: Vec<(&str, usize)> = groups
        .iter()
        .map(|(npc, offered)| (npc.as_str(), offered.len()))
        .collect();
    assert_eq!(
        counts,
        vec![
            ("world.tutorial.npc.Adventurer1", 2),
            ("world.tutorial.npc.Adventurer2", 2),
            ("world.tutorial.npc.Adventurer3", 2),
            ("world.tutorial.npc.Guard", 1),
            ("world.tutorial.npc.HelperNoobosaur00", 1),
            ("world.tutorial.npc.Recruiter1", 2),
            ("world.tutorial.npc.Recruiter2", 1),
        ]
    );
}

#[test]
fn q11_a1_resolves_by_qid_with_the_reference_objective_and_grant() {
    let Some(quests) = quest_corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };

    let qid = dfc_hash(Q11_A1.as_bytes());
    assert_eq!(qid, 0xFBBE_D69D, "djb2 of the quest FQN");

    let quest = quests.quest_by_qid(qid).expect("Q11_a1 by wire qid");
    assert_eq!(quest.fqn, Q11_A1);
    assert_eq!(quest.min_level, 1);
    assert_eq!(quest.max_level, 100);
    assert_eq!(quest.required_class, None);
    assert!(quest.required_quests.is_empty());
    assert!(!quest.repeatable);
    assert_eq!(
        quest.on_accept_item.as_deref(),
        Some("QuestItemPAL2.D00_Q11_a1_01")
    );

    assert_eq!(quest.objectives.len(), 1);
    let objective = &quest.objectives[0];
    assert_eq!(objective.kind, "ItemObjective");
    assert_eq!(objective.name, "MainObjective1");
    assert_eq!(objective.label, "Ticket #54,000,601");
    assert_eq!(objective.required, 1);
    assert_eq!(objective.target_zone, None);
    assert_eq!(objective.wire_label(0), "Ticket #54,000,601: 0 / 1");
}

#[test]
fn hermit_lockbox_objective_names_its_activate_drop() {
    let Some(quests) = quest_corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let quest = quests
        .quest_by_fqn("world.dungeon00.quest.Q03_a1")
        .expect("The Hermit's Hoop");
    let drops = &quest.objectives[0].activate_drops;
    assert_eq!(drops.len(), 1);
    assert_eq!(drops[0].entity, "world.dungeon00.data.Entity_HermitLockbox");
    assert_eq!(drops[0].item, "QuestItemPAL2.D00_Q03_a1_01");
    assert_eq!(drops[0].chance, 100);
}

#[test]
fn base_quest_cash_and_token_rewards_are_inherited() {
    let Some(quests) = quest_corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let q03 = quests
        .quest_by_fqn("world.dungeon00.quest.Q03_a2")
        .expect("Slayeth Wolves, Verily");
    assert_eq!(q03.cash_reward, 0.5);
    assert_eq!(q03.token_reward, 0);
    assert!(q03.grant_xp_buff);
    let token_base = quests
        .quest_by_fqn("world.dungeon00.quest.Q01_a3")
        .expect("QuestToken base");
    assert_eq!(token_base.token_reward, 1);
    assert_eq!(token_base.cash_reward, 1.0);
    let token = quests
        .quest_by_fqn("world.dungeon00.quest.Q01_a3")
        .expect("Token turn-in");
    assert_eq!(token.token_reward, 1);
    assert_eq!(token.cash_reward, 1.0);
}

#[test]
fn lucky_wolf_objective_names_its_kill_drop_triggers() {
    let Some(quests) = quest_corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let quest = quests
        .quest_by_fqn("world.dungeon00.quest.Q11_a5")
        .expect("But Was It Lucky For the Wolf?");
    let objective = &quest.objectives[0];
    assert_eq!(objective.kind, "ItemObjective");
    assert_eq!(
        objective.item.as_deref(),
        Some("QuestItemPAL2.D00_Q11_a5_01")
    );
    assert_eq!(objective.required, 1);
    assert_eq!(objective.kill_drops.len(), 3);
    assert_eq!(
        objective.kill_drops[0].monster,
        "world.dungeon00.mob.melee02.rank2"
    );
    assert_eq!(objective.kill_drops[0].chance, 80);
    assert_eq!(objective.kill_drops[0].item, "QuestItemPAL2.D00_Q11_a5_01");

    // Q03_a4 ("More Wolf Pelts") spells the same trigger drop as `ItemType`.
    let more_pelts = quests
        .quest_by_fqn("world.dungeon00.quest.Q03_a4")
        .expect("More Wolf Pelts");
    let pelts = more_pelts
        .objectives
        .iter()
        .find(|objective| objective.item.as_deref() == Some("QuestItemPAL2.D00_Q03_a3_01"))
        .expect("pelt objective");
    assert_eq!(pelts.required, 20);
    assert_eq!(pelts.kill_drops.len(), 7);
    assert!(pelts
        .kill_drops
        .iter()
        .all(|drop| drop.item == "QuestItemPAL2.D00_Q03_a3_01"));
}

#[test]
fn gos_are_case_folded_to_the_registered_objective_kind() {
    let Some(quests) = quest_corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let quest = quests
        .quest_by_fqn("world.dungeon01.quest.Q04_a2")
        .expect("GotoObjective content");
    assert!(!quest.objectives.is_empty());
    assert!(quest
        .objectives
        .iter()
        .all(|objective| objective.kind == "GoToObjective"));
}

#[test]
fn rattle_lair_arrival_names_the_boss_spawn() {
    let Some(quests) = quest_corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let objective = &quests
        .quest_by_fqn("world.dungeon00.quest.Q02_a1")
        .expect("The Rattle Lair")
        .objectives[0];
    assert_eq!(objective.kind, "GoToObjective");
    assert_eq!(objective.target_zone.as_deref(), Some("dungeon00_level03"));
    assert_eq!(objective.target_entity.as_deref(), Some("boss_spawn"));
    assert_eq!(objective.range, 200);
}

#[test]
fn obsolete_recall_quests_are_not_live_offers() {
    let Some(quests) = quest_corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let old = quests
        .quest_by_fqn("world.dungeon02.quest.Q08_a1")
        .expect("old Agrock's Army");
    assert!(old.obsolete, "Q08_a1 extends QuestObsolete");
    let revised = quests
        .quest_by_fqn("world.dungeon02.quest.Q08_a1_rev")
        .expect("revised Agrock arc");
    assert!(!revised.obsolete);
    assert_eq!(revised.min_level, 15);

    let npcs = quests.zone_npcs("town");
    let completed = HashSet::new();
    let level_seven = quests.offer_groups(7, &npcs, None, GiverDirection::Union, Some(&completed));
    assert!(
        level_seven
            .iter()
            .flat_map(|(_, offered)| offered.iter())
            .all(|quest| !quest.obsolete),
        "no retired recall body is advertised"
    );

    let level_fifteen =
        quests.offer_groups(15, &npcs, None, GiverDirection::Union, Some(&completed));
    assert!(
        level_fifteen
            .iter()
            .flat_map(|(_, offered)| offered.iter())
            .any(|quest| quest.fqn == "world.dungeon02.quest.Q08_a1_rev"),
        "the revised arc is the live offer at its level"
    );
}
