// SPDX-License-Identifier: GPL-3.0-or-later

//! Emit in-world `Equipment` and `Manipulators` item bytes for parity diffing.
//!
//! Usage: `cargo run -p drf-proto --example item_bytes -- <game.pki> <game.pkg>`
//! Prints one hex line per surface for Fighter/Ranger/Warlock.

use std::path::Path;

use drf_content::{derive_class_items, Archive, Corpus};
use drf_proto::{class_starting_equipment_payload, class_starting_item_manipulators_bytes};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let pki = args.next().ok_or("usage: <game.pki> <game.pkg>")?;
    let pkg = args.next().ok_or("usage: <game.pki> <game.pkg>")?;
    let archive = Archive::open(Path::new(&pki), Path::new(&pkg))?;
    let mut corpus = Corpus::new(archive);

    for (aclass, base) in [
        ("avatar.classes.FighterMale", "Fighter"),
        ("avatar.classes.RangerFemale", "Ranger"),
        ("avatar.classes.WarlockFemale", "Warlock"),
    ] {
        let items = derive_class_items(&mut corpus, base)?;
        let equipment = class_starting_equipment_payload(&items, None, None);
        let manipulators = class_starting_item_manipulators_bytes(&items, None, None);
        println!("{aclass} equipment {}", hex(&equipment));
        println!("{aclass} manipulators {}", hex(&manipulators));
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
