// SPDX-License-Identifier: GPL-3.0-or-later

use drf_content::{gc, Archive, Corpus, QuestCorpus};
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let pki = a.next().ok_or("pki")?;
    let pkg = a.next().ok_or("pkg")?;
    let qids: Vec<u32> = a
        .filter_map(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .collect();
    let archive = Archive::open(Path::new(&pki), Path::new(&pkg))?;
    let mut corpus = Corpus::new(archive);
    let qc = QuestCorpus::from_corpus(&mut corpus)?;
    for qid in qids {
        let Some(def) = qc.quest_by_qid(qid) else {
            println!("{qid:#010x}: NOT FOUND");
            continue;
        };
        println!(
            "\n=== {qid:#010x} {} level {}..{}",
            def.fqn, def.min_level, def.max_level
        );
        for o in &def.objectives {
            println!("  parsed {:?}", o);
        }
        let path = format!("{}.gc", def.fqn.replace('.', "/"));
        match corpus.tree(&path) {
            Ok(tree) => {
                let mut nodes = Vec::new();
                gc::descendants(&tree, &mut nodes);
                for node in nodes {
                    let t = node.extends.as_deref().unwrap_or("");
                    if t.contains("Objective") || node.name.contains("Objective") {
                        println!(
                            "  node name={:?} extends={:?} props={:?}",
                            node.name, node.extends, node.props
                        );
                    }
                }
            }
            Err(e) => println!("  tree err {e:?}"),
        }
    }
    Ok(())
}
