// SPDX-License-Identifier: GPL-3.0-or-later

//! Town-world assertions pinned against fixed test data.

use std::collections::HashSet;

use drf_server::world::{
    assign_payload, AvatarRecord, EntityAllocator, StaticKind, TickState, TownNpc, TownPortal,
    TownStatic, World, ENTITY_ID_MIN, NPC_CLASS_OLDMAN, NPC_CLASS_VENDOR_P, NPC_CLASS_VENDOR_W,
    PORTAL_CLASS, TICK_BUDGET_DEFAULT, TICK_INTERVAL_MS_DEFAULT, TOWN_CHECKPOINT_CLASS,
    TOWN_WAYPOINT_CLASS, ZONE_TOWN,
};

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[test]
fn allocator_never_returns_zero_and_never_reuses_a_live_id() {
    let mut allocator = EntityAllocator::new();
    let mut live = HashSet::new();
    for _ in 0..4096 {
        let eid = allocator.alloc().expect("u16 id space is not exhausted");
        assert_ne!(eid, 0, "0 is the unassigned sentinel");
        assert!(
            eid >= ENTITY_ID_MIN,
            "id {eid:#06x} is below the allocator domain"
        );
        assert!(live.insert(eid), "reused a live id {eid:#06x}");
        assert!(allocator.contains(eid));
    }

    let freed = *live.iter().next().unwrap();
    allocator.free(freed);
    assert!(!allocator.contains(freed));
}

#[test]
fn tick_state_advances_by_the_interval_and_budget() {
    let mut tick = TickState::new();
    assert_eq!(TICK_INTERVAL_MS_DEFAULT, 33);
    assert_eq!(TICK_BUDGET_DEFAULT, 11);
    assert_eq!(tick.snapshot(), (0, 33, 11));

    assert_eq!(tick.next_tick(), (1, 33, 11));
    assert_eq!(tick.next_tick(), (2, 33, 11));
    assert_eq!(tick.snapshot(), (2, 33, 11));

    let custom = TickState::with_budget(66, 4);
    assert_eq!(custom.snapshot(), (0, 66, 4));
}

#[test]
fn the_town_assign_payload_is_the_lab_references_bytes() {
    // The live town shape: `74 6f 77 6e` "town", NUL, the zero seed (`u32`),
    // a zero GC-type count, and the four-byte trailer.
    let payload = assign_payload(ZONE_TOWN).unwrap();
    assert_eq!(hex(&payload), "746f776e00000000000000000000");

    let parsed = drf_proto::parse_assign_payload(&payload).unwrap();
    assert_eq!(parsed.zone, b"town");
    assert_eq!(parsed.seed, 0);
    assert!(parsed.gc_types.is_empty());
    assert_eq!(parsed.trailer, vec![0u8; 4]);
}

#[test]
fn the_town_entity_set_is_the_expected_classes_and_allocator_eids() {
    let world = World::new();

    let statics: &[TownStatic] = world.town_statics();
    assert_eq!(statics.len(), 5, "1 checkpoint + 4 waypoints");
    assert_eq!(statics[0].classname, TOWN_CHECKPOINT_CLASS);
    assert_eq!(statics[0].kind, StaticKind::Checkpoint);
    for waypoint in &statics[1..] {
        assert_eq!(waypoint.classname, TOWN_WAYPOINT_CLASS);
        assert_eq!(waypoint.kind, StaticKind::Waypoint);
    }

    let npcs: &[TownNpc] = world.town_npcs();
    assert_eq!(npcs.len(), 3);
    let classes: HashSet<&str> = npcs.iter().map(|npc| npc.classname).collect();
    assert_eq!(
        classes,
        HashSet::from([NPC_CLASS_OLDMAN, NPC_CLASS_VENDOR_W, NPC_CLASS_VENDOR_P])
    );

    let portals: &[TownPortal] = world.portals();
    assert_eq!(portals.len(), 1);
    assert_eq!(portals[0].classname, PORTAL_CLASS);

    let mut ids: Vec<u16> = statics.iter().map(|s| s.entity_id).collect();
    ids.extend(npcs.iter().map(|npc| npc.entity_id));
    ids.extend(portals.iter().map(|portal| portal.entity_id));

    let distinct: HashSet<u16> = ids.iter().copied().collect();
    assert_eq!(distinct.len(), ids.len(), "entity ids are not distinct");
    for eid in ids {
        assert_ne!(eid, 0);
        assert!(
            eid >= ENTITY_ID_MIN,
            "id {eid:#06x} is below the allocator domain"
        );
        assert!(world.entities.contains(eid));
    }
}

#[test]
fn spawn_allocates_distinct_session_entities() {
    let mut world = World::new();
    let avatar = world.spawn_avatar(1, 831, b"Hero", None);
    let player = world.spawn_player(1, b"Hero");
    let warlock = world.spawn_avatar(2, 832, b"Alt", Some(b"avatar.classes.WarlockFemale"));

    for eid in [avatar, player, warlock] {
        assert_ne!(eid, 0);
        assert!(eid >= ENTITY_ID_MIN);
    }
    assert_eq!(
        HashSet::from([avatar, player, warlock]).len(),
        3,
        "session entities collide"
    );

    assert_eq!(
        world.avatar(1).unwrap(),
        &AvatarRecord {
            entity_id: avatar,
            sqlid: 831,
            name: b"Hero".to_vec(),
            avatar_class: b"Avatar".to_vec(),
        }
    );
    assert_eq!(
        world.avatar(2).unwrap().avatar_class,
        b"avatar.classes.WarlockFemale".to_vec()
    );
    assert_eq!(world.player(1).unwrap().entity_id, player);
}
