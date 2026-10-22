// SPDX-License-Identifier: GPL-3.0-or-later

//! Emit character-graph bytes for parity diffing against the reference builder.
//!
//! Usage: `cargo run -p drf-proto --example graph_bytes -- <game.pki> <game.pkg>`
//! Prints one hex line per surface for three classes.

use std::path::Path;

use drf_content::{Archive, Corpus};
use drf_proto::{
    created_payload, join_player_body, list_payload, player_graph_node, Appearance, ClassContent,
    ListRow,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let pki = args.next().ok_or("usage: <game.pki> <game.pkg>")?;
    let pkg = args.next().ok_or("usage: <game.pki> <game.pkg>")?;
    let archive = Archive::open(Path::new(&pki), Path::new(&pkg))?;
    let mut corpus = Corpus::new(archive);

    let appearance = Appearance {
        skin: Some(1),
        face: Some(2),
        face_feature: Some(0),
        hair: Some(3),
        hair_color: Some(4),
    };
    for aclass in [
        "avatar.classes.FighterMale",
        "avatar.classes.RangerFemale",
        "avatar.classes.WarlockFemale",
    ] {
        let content = ClassContent::derive(&mut corpus, aclass)?;
        let node = player_graph_node("tester1", 1, Some(&content), Some(&appearance));
        println!("{aclass} node {}", hex(&node));
        let rows = [ListRow {
            sqlid: 1,
            name: "tester1",
            content: Some(&content),
            appearance: Some(&appearance),
        }];
        println!("{aclass} list {}", hex(&list_payload(&rows)));
        println!(
            "{aclass} created {}",
            hex(&created_payload(
                1,
                "tester1",
                Some(&content),
                Some(&appearance)
            ))
        );
        println!(
            "{aclass} join {}",
            hex(&join_player_body(
                0x1001,
                "tester1",
                1,
                Some(&content),
                Some(&appearance)
            ))
        );
        let bare = player_graph_node("tester1", 1, None, None);
        println!("{aclass} bare {}", hex(&bare));
    }
    Ok(())
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
