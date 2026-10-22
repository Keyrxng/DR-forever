// SPDX-License-Identifier: GPL-3.0-or-later

//! The loot-phase wire: the `ItemObject` create+init body, the item's own
//! `Item::readData` body, and the `(07,0x33)` pickup-acquire notice.
//!
//! A drop is the client's `ItemObject::writeInit @0x58AFF0` layout (the
//! `WorldEntity` base, then the `ItemObject` fields, then the item class),
//! followed by the item's own `write_init_for_dropped_item` body
//! (`Item::readData @0x581710`, the reference `GCObject.WriteItemData`), and
//! ended by the `0x06` stream terminator. The live 191-byte dungeon drop in
//! `test_combat.py`'s `_COMBAT_DROP_HEX` is the byte reference: these tests
//! reconstruct it exactly, and pin the simple consumable form and the full
//! equipment form beside it.

use std::path::Path;

use drf_content::{Archive, Corpus};
use drf_proto::{
    build_acquire_notice, build_item_object_create_init, is_consumable_class,
    item_object_init_payload, CreateError, DroppedItemBody, CEM_ACQUIRE_NOTICE, ITEMOBJECT_CLASS,
    ITEMOBJECT_SPAWN_FLAG, ITEMOBJECT_STATE_DEFAULT, ITEMOBJECT_TIMER_DEFAULT,
    ITEMOBJECT_W1_DEFAULT, ITEMOBJECT_WORLD_ENTITY_FLAGS,
};

const POTION: &str = "items.consumables.Consumable_MajorHealthPotion";
const ARMOR: &str = "LeatherArmor3PAL.LeatherArmor3-2";

/// The live dungeon drop, verbatim (m0stub `_COMBAT_DROP_HEX`, live #303). Its
/// ent is `0x2002`; the payload's item class is `LeatherArmor3PAL.LeatherArmor3-2`
/// and its owner is `Keyrxng`. It is a complete single-record `07 08` CEM
/// stream: the `ItemObject` init, the item's `readData` body (two by-name
/// `ItemModifier` children), and the `0x06` end.
const LIVE_DROP: &str = concat!(
    "07080220ff4974656d4f626a6563740006000000378200005b5a0200ff09000000",
    "270000000000003f0300000000000078690000014b657972786e6700ff4c656174",
    "68657241726d6f723350414c2e4c65617468657241726d6f72332d320000000000",
    "ffff0117000002ff6974656d732e6d6f6470616c2e52616e6765724d6f6450414c",
    "2e42696e6465722e4d6f6431000114ff6974656d732e6d6f6470616c2e52616e67",
    "65724d6f6450414c2e5375706572696f722e4d6f643800011406"
);

/// `[07 08][u16 eid][0xFF]["ItemObject" NUL]` is the live drop's 16-byte head.
const LIVE_PREFIX: usize = 16;

/// The live drop's `ItemObject` init payload (after the create head, before the
/// `0x06` terminator).
fn live_payload() -> Vec<u8> {
    let bytes = hex(LIVE_DROP);
    bytes[LIVE_PREFIX..bytes.len() - 1].to_vec()
}

/// Our builder's default consumable roll: a potion dropped at pose
/// `(1000, 2000, 30, 40)`, level 10, owner `Hero`. The `state = 0` byte and
/// the `+0x118` flag `= 1` (with `timer = 27000`) are the client's
/// spawn-animation selector: the drop flips/settles instead of popping in
/// static (`ItemObject::updateState @0x58A410`).
const DEFAULT_INIT: &str = concat!(
    "06000000e8030000d00700001e000000280000000000000001000000000000007869",
    "0000014865726f00ff6974656d732e636f6e73756d61626c65732e436f6e73756d61",
    "626c655f4d616a6f724865616c7468506f74696f6e0000000000ffff010a0000",
);

/// The simple consumable form: `[u32 id=0][0xFF 0xFF 0x01][level 10]`
/// `[flags=0][count=0]`.
const CONSUMABLE_BODY: &str = "00000000ffff010a0000";

/// The full equipment form for the live drop's own modifiers: id 0, unplaced
/// grid, level `0x17`, flags 0, one authored slot, count 2, then two by-name
/// `ItemModifier` children at level `0x14`.
const EQUIPMENT_BODY: &str = concat!(
    "00000000ffff0117000002ff6974656d732e6d6f6470616c2e52616e6765724d",
    "6f6450414c2e42696e6465722e4d6f6431000114ff6974656d732e6d6f647061",
    "6c2e52616e6765724d6f6450414c2e5375706572696f722e4d6f6438000114"
);

/// The live equipment modifiers, verbatim.
fn live_modifiers() -> Vec<String> {
    vec![
        "items.modpal.RangerModPAL.Binder.Mod1".to_string(),
        "items.modpal.RangerModPAL.Superior.Mod8".to_string(),
    ]
}

#[test]
fn consumable_and_equipment_are_the_two_drop_forms() {
    // The reference's `GCObject._is_consumable_type` keyword list decides.
    assert!(is_consumable_class(POTION));
    assert!(is_consumable_class("items.pal.SomeScroll"));
    assert!(!is_consumable_class(ARMOR));
    assert!(!is_consumable_class("ChainArmor1PAL.ChainArmor1-2"));

    // The simple consumable body.
    let mut consumable = Vec::new();
    DroppedItemBody::consumable(10).write(&mut consumable);
    assert_eq!(
        consumable,
        hex(CONSUMABLE_BODY),
        "the simple consumable form"
    );

    // The full equipment body: authored slot + the modifier children.
    let mut equipment = Vec::new();
    DroppedItemBody::equipment(0x17, 1, live_modifiers(), 0x14).write(&mut equipment);
    assert_eq!(equipment, hex(EQUIPMENT_BODY), "the full equipment form");

    // `for_class` picks the same form the reference's branch would. An armour
    // declares one `ItemAttributeModifier`, so its `Item::readData` body
    // carries one status byte before the (empty) rolled-modifier list.
    let mut auto = Vec::new();
    DroppedItemBody::for_class(ARMOR, 0x17, 1).write(&mut auto);
    assert_eq!(auto, hex("00000000ffff0117000000"));
    // A class that declares no item modifier has no status byte at all.
    let mut bare = Vec::new();
    DroppedItemBody::for_class(ARMOR, 0x17, 0).write(&mut bare);
    assert_eq!(bare, hex("00000000ffff01170000"));
}

#[test]
fn itemobject_init_is_the_full_world_entity_shape() {
    let payload = item_object_init_payload(
        POTION,
        [1000, 2000, 30, 40],
        "Hero",
        ITEMOBJECT_WORLD_ENTITY_FLAGS,
        ITEMOBJECT_STATE_DEFAULT,
        0,
        ITEMOBJECT_W1_DEFAULT,
        0,
        ITEMOBJECT_TIMER_DEFAULT,
        ITEMOBJECT_SPAWN_FLAG,
        &DroppedItemBody::consumable(10),
    )
    .expect("default init");
    assert_eq!(payload, hex(DEFAULT_INIT), "the pinned default init");
    // 90-byte wrapper + the 10-byte consumable body.
    assert_eq!(payload.len(), 100);

    // The WorldEntity base is the FIVE u32 quad plus the fb gate byte.
    assert_eq!(
        &payload[0..4],
        &ITEMOBJECT_WORLD_ENTITY_FLAGS.to_le_bytes(),
        "WorldEntity flags word, not the reference's single byte"
    );
    assert_eq!(&payload[4..8], &1000u32.to_le_bytes(), "x");
    assert_eq!(&payload[8..12], &2000u32.to_le_bytes(), "y");
    assert_eq!(&payload[12..16], &30u32.to_le_bytes(), "z");
    assert_eq!(&payload[16..20], &40u32.to_le_bytes(), "aux/heading");
    assert_eq!(payload[20], 0x00, "fb gate: no tails");

    // Then the ItemObject fields.
    assert_eq!(payload[21], ITEMOBJECT_STATE_DEFAULT, "state");
    assert_eq!(
        payload[21], 0,
        "state 0 selects the client's FlipController spawn animation"
    );
    assert_eq!(&payload[22..24], &0u16.to_le_bytes(), "slot");
    assert_eq!(&payload[24..28], &ITEMOBJECT_W1_DEFAULT.to_le_bytes(), "w1");
    assert_eq!(&payload[28..32], &0u32.to_le_bytes(), "w2");
    assert_eq!(
        &payload[32..36],
        &ITEMOBJECT_TIMER_DEFAULT.to_le_bytes(),
        "timer"
    );
    assert_eq!(
        payload[36], ITEMOBJECT_SPAWN_FLAG,
        "the +0x118 spawn flag is set so addToWorld picks initState(0)"
    );
    assert_eq!(
        &payload[37..42],
        b"Hero\0",
        "owner name, present because w1 != 0"
    );
    assert_eq!(payload[42], 0xFF, "item-desc type tag");
    assert_eq!(
        &payload[43..43 + POTION.len() + 1],
        format!("{POTION}\0").as_bytes(),
        "item class"
    );
    assert_eq!(
        &payload[43 + POTION.len() + 1..],
        hex(CONSUMABLE_BODY).as_slice(),
        "the item's own readData body follows the classname"
    );

    // Independent re-encode.
    assert_eq!(payload, encode_default_init(POTION));
}

/// The builder reconstructs the live 191-byte dungeon drop byte-for-byte once
/// it carries the item's own `readData` body: the wrapper's 79-byte head plus
/// the live tail (the fixed head and the two modifier entries).
#[test]
fn itemobject_init_reproduces_the_live_drop() {
    let body = DroppedItemBody::equipment(0x17, 1, live_modifiers(), 0x14);
    let ours = item_object_init_payload(
        ARMOR,
        [0x8237, 0x25a5b, 0x9ff, 0x2700],
        "Keyrxng",
        0x0000_0006,
        0,     // captured state
        0,     // captured slot
        0x33f, // captured w1 (the owner sqlid)
        0,     // captured w2
        27000, // captured timer
        1,     // captured lock
        &body,
    )
    .expect("live init");
    let live = live_payload();
    assert_eq!(ours, live, "the full live drop payload");
    assert_eq!(
        ours.len(),
        174,
        "create head 16 + payload 174 + end 1 = 191"
    );

    // The 79-byte wrapper head is unchanged; the tail is the item body.
    assert_eq!(&ours[..79], &live[..79], "the live wrapper head");
    assert_eq!(
        &ours[79..],
        hex(EQUIPMENT_BODY).as_slice(),
        "the live item body"
    );
}

#[test]
fn consumable_drop_is_the_full_07_08_body_06_stream() {
    let packet = build_item_object_create_init(
        0x2002,
        POTION,
        [1000, 2000, 30, 40],
        "Hero",
        &DroppedItemBody::consumable(10),
    )
    .expect("consumable drop");
    assert_eq!(
        packet,
        hex(concat!(
            "0220ff4974656d4f626a6563740006000000e8030000d00700001e00000028000000",
            "00000000010000000000000078690000014865726f00ff6974656d732e636f6e7375",
            "6d61626c65732e436f6e73756d61626c655f4d616a6f724865616c7468506f74696f",
            "6e0000000000ffff010a000006"
        )),
        "the pinned consumable drop"
    );
    // `[u16 eid][0xFF]["ItemObject" NUL]` + init payload + `0x06`.
    assert_eq!(&packet[0..2], &0x2002u16.to_le_bytes());
    assert_eq!(packet[2], 0xFF);
    assert_eq!(&packet[3..3 + ITEMOBJECT_CLASS.len() + 1], b"ItemObject\0");
    assert_eq!(&packet[14..packet.len() - 1], hex(DEFAULT_INIT).as_slice());
    assert_eq!(*packet.last().unwrap(), 0x06, "the stream terminator");
}

#[test]
fn equipment_drop_reproduces_the_live_capture() {
    let body = DroppedItemBody::equipment(0x17, 1, live_modifiers(), 0x14);
    // The live capture's `ItemObject` fields are its own (state 0, the owner
    // sqlid, a 27000-tick timer, a locked pick), so the full record is
    // reconstructed with `item_object_init_payload`, not the drop defaults.
    let payload = item_object_init_payload(
        ARMOR,
        [0x8237, 0x25a5b, 0x9ff, 0x2700],
        "Keyrxng",
        0x0000_0006,
        0,
        0,
        0x33f,
        0,
        27000,
        1,
        &body,
    )
    .expect("equipment drop");
    let mut inner = vec![0x07, 0x08, 0x02, 0x20, 0xFF];
    inner.extend_from_slice(b"ItemObject\0");
    inner.extend_from_slice(&payload);
    inner.push(0x06);
    assert_eq!(
        inner,
        hex(LIVE_DROP),
        "the live dungeon drop, byte-for-byte"
    );
}

#[test]
fn init_owner_is_gated_on_w1() {
    // w1 = 0 omits the owner entirely (an ownerless drop is legal).
    let ownerless = item_object_init_payload(
        POTION,
        [0; 4],
        "Hero",
        0,
        1,
        0,
        0,
        0,
        0,
        0,
        &DroppedItemBody::consumable(1),
    )
    .expect("w1 = 0");
    // The 0xFF item-desc tag follows the lock byte directly: no owner slot.
    assert_eq!(ownerless[37], 0xFF, "item-desc tag with no owner");
    assert_eq!(
        ownerless.len(),
        49 + POTION.len(),
        "no owner bytes, plus the item body"
    );
    // w1 != 0 with no owner is a refusal, not a substituted word.
    assert_eq!(
        item_object_init_payload(
            POTION,
            [0; 4],
            "",
            0,
            1,
            0,
            1,
            0,
            0,
            0,
            &DroppedItemBody::consumable(1)
        )
        .unwrap_err(),
        CreateError::EmptyOwner
    );
}

#[test]
fn create_init_rejects_bad_heads() {
    let body = DroppedItemBody::consumable(1);
    assert_eq!(
        build_item_object_create_init(0, POTION, [0; 4], "Hero", &body).unwrap_err(),
        CreateError::ZeroEntityId
    );
    assert_eq!(
        build_item_object_create_init(0x2002, "", [0; 4], "Hero", &body).unwrap_err(),
        CreateError::BadClassname
    );
}

#[test]
fn acquire_notice_is_the_tagged_07_33_record() {
    let notice = build_acquire_notice(0x2002, 0x1234, POTION).expect("acquire");
    assert_eq!(
        notice,
        hex(concat!(
            "073302203412",
            "6974656d732e636f6e73756d61626c65732e436f6e73756d61626c655f4d",
            "616a6f724865616c7468506f74696f6e00"
        )),
        "the (07,33) acquire"
    );
    assert_eq!(&notice[0..2], &[0x07, CEM_ACQUIRE_NOTICE]);
    assert_eq!(&notice[2..4], &0x2002u16.to_le_bytes(), "drop id");
    assert_eq!(&notice[4..6], &0x1234u16.to_le_bytes(), "avatar eid");
    assert_eq!(&notice[6..], format!("{POTION}\0").as_bytes(), "item desc");
}

/// An independent re-encode of the default init, so the pinned hex has a second
/// derivation.
fn encode_default_init(item_class: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&ITEMOBJECT_WORLD_ENTITY_FLAGS.to_le_bytes());
    out.extend_from_slice(&1000u32.to_le_bytes());
    out.extend_from_slice(&2000u32.to_le_bytes());
    out.extend_from_slice(&30u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.push(0);
    out.push(ITEMOBJECT_STATE_DEFAULT);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&ITEMOBJECT_W1_DEFAULT.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&ITEMOBJECT_TIMER_DEFAULT.to_le_bytes());
    out.push(ITEMOBJECT_SPAWN_FLAG);
    out.extend_from_slice(b"Hero\0");
    out.push(0xFF);
    out.extend_from_slice(item_class.as_bytes());
    out.push(0);
    out.extend_from_slice(&hex(CONSUMABLE_BODY));
    out
}

/// An equipment drop's `Item::readData` body comes from the class's own
/// `ItemModifier` children, which `game.pkg` settles. `LeatherArmor3-2`
/// declares one `ItemAttributeModifier` (the `BaseArmorClasses.Leather`
/// `SpeedM`), so the client's `ReadChildData<ItemModifier>` loop reads one
/// status byte before it reads the rolled-modifier count. The pre-fix body
/// omitted that byte, so the client read the `0x06` EndStream as a count of
/// six modifiers and over-read the stream (Zone communication error, code 2).
#[test]
fn an_equipment_drop_carries_the_class_modifier_slot_byte() {
    let Some(mut corpus) = corpus() else {
        eprintln!("skip: no game.pkg on this machine");
        return;
    };
    let slots = corpus
        .item_readdata_slots(ARMOR)
        .expect("LeatherArmor3-2 resolves");
    assert_eq!(
        slots, 1,
        "LeatherArmor3-2 declares one ItemAttributeModifier (SpeedM)"
    );
    assert_eq!(
        corpus
            .item_readdata_slots("ChainArmor1PAL.ChainArmor1-2")
            .expect("chain armour resolves"),
        0,
        "BaseArmorClasses.Chain declares no ItemModifier, so chain armour has no slot"
    );
    assert_eq!(
        corpus
            .item_readdata_slots(POTION)
            .expect("the potion resolves"),
        0,
        "the potion's Modifier is an AttributeModifier, not an ItemModifier"
    );
    assert_eq!(
        corpus
            .item_readdata_slots("items.pal.1HMacePAL.Normal001")
            .expect("the mace resolves"),
        0,
        "a weapon's native readState is Manipulators-only; Item::readData has no extra slot"
    );

    // The pinned equipment body: id 0, unplaced grid, level 0x17, flags 0, the
    // one class slot, then an empty rolled-modifier list.
    let mut body = Vec::new();
    DroppedItemBody::for_class(ARMOR, 0x17, slots).write(&mut body);
    assert_eq!(body, hex("00000000ffff0117000000"), "the equipment body");

    // In the full packet the body is followed by EndStream. The pre-fix body
    // was exactly this minus the slot byte, which put `0x06` where the client
    // reads the modifier count.
    let packet = build_item_object_create_init(
        0x1047,
        ARMOR,
        [1000, 2000, 30, 40],
        "Hero",
        &DroppedItemBody::for_class(ARMOR, 0x17, slots),
    )
    .expect("equipment drop");
    let class_at = packet
        .windows(ARMOR.len())
        .position(|window| window == ARMOR.as_bytes())
        .expect("the init carries the item class");
    let body_at = class_at + ARMOR.len() + 1;
    assert_eq!(
        &packet[body_at..body_at + 11],
        hex("00000000ffff0117000000").as_slice(),
        "the item body carries the slot byte before the count"
    );
    assert_eq!(
        packet[body_at + 11],
        0x06,
        "the EndStream follows the completed body, not the modifier count"
    );
}

/// Open `game.pkg` when a client tree is present.
fn corpus() -> Option<Corpus> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.ancestors().nth(4)?;
    let dir = [
        repo.join("dr-client-666-testing"),
        repo.join("Dungeon_Runners_Client_666"),
    ]
    .into_iter()
    .find(|dir| dir.join("game.pkg").is_file())?;
    let archive =
        Archive::open(&dir.join("game.pki"), &dir.join("game.pkg")).expect("open game.pkg");
    Some(Corpus::new(archive))
}

fn hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex length");
    let value = |b: u8| match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => panic!("non-hex digit {b:?}"),
    };
    text.bytes()
        .collect::<Vec<_>>()
        .chunks(2)
        .map(|pair| value(pair[0]) << 4 | value(pair[1]))
        .collect()
}
