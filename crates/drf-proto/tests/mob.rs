// SPDX-License-Identifier: GPL-3.0-or-later

//! The mob create stream shape: a create, a class-state init, four nested
//! components and two `0x35` trailers (SpawnAction + MoverUpdate). The stream
//! ships NO `0x64` FollowClient enroll (bible §14.6 round 6n): the client's own
//! native `MonsterBehavior2` brain proximity-aggros, chases and attacks with no
//! server help, and the `0x64` hand-off was what broke the native melee swing.
//!
//! This mirrors how `drf-server`'s join assembles a generated zone's mobs, so
//! the record order and the HP trailer cannot drift from the builders.

use drf_content::CreatureManipulator;
use drf_proto::{
    create_body, frame_inner, mob_behavior_class_state, mob_class_state,
    mob_class_state_with_current_health, mob_component_record, mob_manipulators_class_state,
    mob_modifiers_class_state, mob_mover_update_payload, mob_skills_class_state,
    mob_spawn_action_payload, MOB_BEHAVIOR_CLASS, MOB_MANIPULATORS_CLASS, MOB_MODIFIERS_CLASS,
    MOB_SKILLS_CLASS,
};

const CLASS: &str = "world.dungeon00.mob.melee01.rank1";

#[test]
fn a_mob_stream_orders_create_init_components_then_trailers() {
    let entity = 0x1000u16;
    let behavior = 0xE000u16;
    let skills = 0xE001u16;
    let manipulators = 0xE002u16;
    let modifiers = 0xE003u16;
    let pose = [89194i32, 145598, 2560, 17352];
    let upose = [
        pose[0] as u32,
        pose[1] as u32,
        pose[2] as u32,
        pose[3] as u32,
    ];
    let hp = 29184u32;

    let mut creates: Vec<(u8, Vec<u8>)> = Vec::new();
    creates.push((0x01, create_body(entity, CLASS).unwrap()));
    let mut init = entity.to_le_bytes().to_vec();
    init.extend_from_slice(&mob_class_state(upose, 2).unwrap());
    creates.push((0x02, init));

    let components = [
        (behavior, MOB_BEHAVIOR_CLASS, mob_behavior_class_state()),
        (skills, MOB_SKILLS_CLASS, mob_skills_class_state()),
        (
            manipulators,
            MOB_MANIPULATORS_CLASS,
            mob_manipulators_class_state(&[CreatureManipulator::default_melee()]),
        ),
        (modifiers, MOB_MODIFIERS_CLASS, mob_modifiers_class_state()),
    ];
    for (cid, class, state) in components {
        let record = mob_component_record(entity, cid, class, &state).unwrap();
        assert_eq!(record[0], 0x32);
        creates.push((0x32, record[1..].to_vec()));
    }
    creates.push((0x35, mob_spawn_action_payload(behavior, pose, entity, hp)));
    creates.push((0x35, mob_mover_update_payload(behavior, pose, hp)));

    let refs: Vec<(u8, &[u8])> = creates
        .iter()
        .map(|(tag, body)| (*tag, body.as_slice()))
        .collect();
    let inner = frame_inner(&refs).expect("frame the stream");

    // One `[07]` channel, the create first, and the MoverUpdate's HP trailer
    // (`0x02` + x256 HP) last.
    assert_eq!(&inner[0..2], &[0x07, 0x01]);
    // The END marker follows the last trailer, so the synch word sits just
    // before it.
    assert_eq!(
        &inner[inner.len() - 6..inner.len() - 1],
        &[0x02, 0x00, 0x72, 0x00, 0x00]
    );
    assert_eq!(*inner.last().unwrap(), 0x06);

    // SpawnAction and MoverUpdate each carry the client-derivable HP word.
    let wanted = [0x02, 0x00, 0x72, 0x00, 0x00];
    let hits = inner
        .windows(wanted.len())
        .filter(|window| *window == wanted)
        .count();
    assert_eq!(hits, 2, "SpawnAction + MoverUpdate trailers");

    // PURE-NATIVE (bible §14.6 round 6n): the stream must NOT carry the
    // `35 <behavior:u16> 64 01 02 <hp:u32>` FollowClient block — enrolling the
    // mob parks it in client-control mode chasing the avatar's centre, so its
    // native melee swing never plays.
    let mut enroll = vec![0x35u8];
    enroll.extend_from_slice(&behavior.to_le_bytes());
    enroll.extend_from_slice(&[0x64, 0x01, 0x02]);
    assert!(
        !inner.windows(enroll.len()).any(|window| window == enroll),
        "the mob create stream must NOT enroll the client brain (pure native)"
    );
}

#[test]
fn explicit_mob_health_sets_the_unit_flag_and_current_hp_word() {
    let hp = 43392u32;
    let state = mob_class_state_with_current_health([1, 2, 3, 4], 2, Some(hp)).unwrap();
    // WorldEntity header (21) + unit_flags + level + two u16 fields, then the
    // optional current-HP word before the 25-byte StockUnit tail.
    let unit = 21;
    assert_eq!(
        state[unit], 0x02,
        "unit_flags advertises explicit current HP"
    );
    assert_eq!(state[unit + 1], 2, "level");
    assert_eq!(
        &state[unit + 6..unit + 10],
        &hp.to_le_bytes(),
        "current-HP word follows the unit tail"
    );
}
