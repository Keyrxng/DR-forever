// SPDX-License-Identifier: GPL-3.0-or-later

//! Emit char/join payloads for parity diffing against the reference builder.
//!
//! Usage: `cargo run --offline -q -p drf-proto --example char_join_bytes`

use drf_proto::{
    assign_payload, create_body, delete_ack_payload, delete_payload, group_connected_0930_payload,
    instance_count_payload, join_batch, ready_payload, rosters_0301_payload, seed_payload,
    select_ack_payload, select_payload, user_changed_group_0935_payload,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("select {}", hex(&select_payload(831)));
    println!("select_ack_ok {}", hex(&select_ack_payload(true)));
    println!("select_ack_fail {}", hex(&select_ack_payload(false)));
    println!("rosters_0301 {}", hex(&rosters_0301_payload()));
    println!("connected_0930 {}", hex(&group_connected_0930_payload(831)));
    println!(
        "changed_group_0935 {}",
        hex(&user_changed_group_0935_payload(
            [0xA9, 0x02, 0x00, 0x80],
            831,
            "Keyrxng",
            25,
        ))
    );
    println!("delete {}", hex(&delete_payload(831)));
    println!("delete_ack_ok {}", hex(&delete_ack_payload(831, true)));
    println!("delete_ack_fail {}", hex(&delete_ack_payload(831, false)));
    println!(
        "assign_town {}",
        hex(&assign_payload("town", 0, &[], None)?)
    );
    println!(
        "assign_algate {}",
        hex(&assign_payload(
            "algate",
            0,
            &["world.town.npc.Bank"],
            Some(&[1, 2, 3, 4])
        )?)
    );
    println!("ready {}", hex(&ready_payload(831)));
    println!("count_zero {}", hex(&instance_count_payload(0, 0)));
    println!("count_three_two {}", hex(&instance_count_payload(3, 2)));
    println!("seed {}", hex(&seed_payload(0x1234_5678)));

    let first = create_body(0x1234, "Player")?;
    let second = create_body(0x1235, "Avatar")?;
    println!(
        "join_batch {}",
        hex(&join_batch(
            (10_544_915, 33, 3),
            &[(0x01, &first), (0x08, &second)],
            0x46,
        ))
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
