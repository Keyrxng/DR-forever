// SPDX-License-Identifier: GPL-3.0-or-later

//! Print the per-class content `drf-content` derives from a `game.pkg`.
//!
//! Usage: `cargo run -p drf-content --example derive_classes -- <game.pki> <game.pkg>`
//! The output is the parity fixture the reference `class_corpus.py` is diffed
//! against, so the two derivations cannot drift.

use std::path::Path;

use drf_content::{
    derive_class_fqns, derive_class_items, derive_class_kit, derive_class_skill_elements,
    derive_class_visuals, Archive, Corpus, CLASS_BASES, CLASS_SEXES,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let pki = args.next().ok_or("usage: <game.pki> <game.pkg>")?;
    let pkg = args.next().ok_or("usage: <game.pki> <game.pkg>")?;
    let archive = Archive::open(Path::new(&pki), Path::new(&pkg))?;
    let mut corpus = Corpus::new(archive);

    let mut bases: Vec<&str> = CLASS_BASES.to_vec();
    bases.sort_unstable();
    for base in bases {
        let kit = derive_class_kit(&mut corpus, base)?;
        println!("== {base}");
        println!(
            "   profession {:<40} 0x{:08x}",
            kit.profession_fqn, kit.profession
        );
        for (index, fqn) in kit.skill_fqns.iter().enumerate() {
            println!(
                "   skill[{index}] {:<14} {:<40} 0x{:08x}",
                kit.skill_dfc[index], fqn, kit.skills[index]
            );
        }
        for (index, fqn) in kit.item_fqns.iter().enumerate() {
            println!(
                "   item[{index}]  {:<14} {:<40} 0x{:08x}",
                kit.item_dfc[index], fqn, kit.items[index]
            );
        }
        for (index, skill) in derive_class_skill_elements(&mut corpus, base)?
            .iter()
            .enumerate()
        {
            println!(
                "   elem[{index}] {} {} {} {} {}",
                skill.fqn, skill.id, skill.level, skill.dfc, skill.element_type
            );
        }
        for item in derive_class_items(&mut corpus, base)? {
            println!(
                "   kit  {} slot={} level={} dfc={} authored={} mods={:?}",
                item.fqn, item.slot, item.level, item.dfc, item.authored, item.mods
            );
        }
    }
    let mut fqns: Vec<(String, _)> = derive_class_fqns(&mut corpus)?.into_iter().collect();
    fqns.sort_by(|a, b| a.0.cmp(&b.0));
    for (fqn, info) in &fqns {
        println!("class {:<32} {:<8} 0x{:08x}", fqn, info.base, info.hash);
    }
    for base in CLASS_BASES {
        for sex in CLASS_SEXES {
            let fqn = format!("avatar.classes.{base}{sex}");
            if let Ok(visuals) = derive_class_visuals(&mut corpus, &fqn) {
                println!("visuals {:<32} {}", fqn, visuals);
            }
        }
    }
    Ok(())
}
