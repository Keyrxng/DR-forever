// SPDX-License-Identifier: GPL-3.0-or-later

//! Emit CEM bodies and framing for parity diffing against the reference builder.
//!
//! Usage: `cargo run --offline -q -p drf-proto --example cem_bytes`

use drf_proto::{
    acquire_payload, create_body, frame_inner, interval_payload, interval_stream, item_create_body,
    remove_inner, CEM_CREATE, CONNECTED_CASE, INTERVAL_PATHMANAGER_BLOCK, TOWN_BUDGET_TRIPLE,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let first = create_body(0x1234, "world.town.npc.OldMan1")?;
    let second = create_body(0x1235, "world.town.npc.VendorWeapon1")?;

    println!("create {}", hex(&first));
    println!(
        "batch {}",
        hex(&frame_inner(&[
            (CEM_CREATE, &first),
            (CEM_CREATE, &second)
        ])?)
    );
    println!(
        "item {}",
        hex(&item_create_body(
            0x2001,
            "items.consumables.Consumable_MajorHealthPotion",
            "tester1",
            1,
            0,
            1,
            0,
            0,
            0,
        )?)
    );
    println!("remove {}", hex(&remove_inner(0x1234)));
    println!("interval {}", hex(&interval_payload(10_544_915, 33, 3)));
    println!(
        "stream {}",
        hex(&interval_stream(
            &[(0x1234, "world.town.npc.OldMan1")],
            TOWN_BUDGET_TRIPLE,
            Some(&INTERVAL_PATHMANAGER_BLOCK),
            CONNECTED_CASE,
            &[],
            &[],
            &[],
            &[],
        )?)
    );
    println!(
        "acquire {}",
        hex(&acquire_payload(
            0x2001,
            0x1234,
            "items.consumables.Consumable_MajorHealthPotion"
        )?)
    );
    Ok(())
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
