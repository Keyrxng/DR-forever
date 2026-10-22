// SPDX-License-Identifier: GPL-3.0-or-later

//! The character graph: the `(04,02)` created reply, the `(04,03)` list entry
//! and the join `0x08 player` body, as the client's own readers consume them.
//!
//! Every node is the shape `GCNode::readObject` @ `0x005EA0D0` reads -- a
//! `0x2D` version, the class id, an object id, an empty name, a child count,
//! a `GCNode+0x50` archetype id, then a `u32`-terminated property list. The
//! class ids are `dfc_hash` of the class name; the archetype id is a second
//! id space (the pkg archetype) and may differ from the class id.
//!
//! The class-dependent body (the starting kit, its skills, its `Visuals`)
//! comes from [`drf_content`]; nothing here is a capture or a table.

use std::collections::HashSet;

use drf_content::{dfc_hash, ClassItem, ClassSkillWire, ContentError, Corpus};

/// The graph object version byte (`DFC_NODE_VERSION`).
pub const NODE_VERSION: u8 = 0x2D;

const PLAYER_CLASS: &str = "Player";
const AVATAR_CLASS: &str = "Avatar";
const MODIFIERS_CLASS: &str = "Modifiers";
const UNITBEHAVIOR_CLASS: &str = "UnitBehavior";
const UNITBEHAVIORDESC_CLASS: &str = "UnitBehaviorDesc";
const AVATARDESC_CLASS: &str = "AvatarDesc";
const MANIPULATORS_CLASS: &str = "Manipulators";
const SKILLS_CLASS: &str = "Skills";
const EQUIPMENT_CLASS: &str = "Equipment";
const SKILL_PROFESSION_CLASS: &str = "SkillProfession";

/// The item slots the client has a name for (`EquipmentSlot` ids).
pub const EQUIPMENT_VALID_SLOTS: [i64; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 10, 11];

/// The length of one serialized item object.
pub const ITEM_ENTRY_LEN: usize = 90;

/// The base-`Item` property block every serialized item carries, 59 bytes.
///
/// Each `[u32 id]` is `dfc_hash` of the property's short name (the id the
/// client's writer caches into the property), and each value is the
/// default-constructed value. The block ends on the `ManipulatorID` id: its
/// value (the worn slot) follows the block, then [`item_tail`].
fn item_property_block() -> Vec<u8> {
    let mut out = Vec::with_capacity(59);
    let u32v = |out: &mut Vec<u8>, id: &str, v: u32| {
        out.extend_from_slice(&dfc_hash(id.as_bytes()).to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    };
    let u8v = |out: &mut Vec<u8>, id: &str, v: u8| {
        out.extend_from_slice(&dfc_hash(id.as_bytes()).to_le_bytes());
        out.push(v);
    };
    u32v(&mut out, "InventoryX", 255);
    u32v(&mut out, "InventoryY", 255);
    u32v(&mut out, "Level", 1);
    u8v(&mut out, "NoSell", 0);
    u32v(&mut out, "Quantity", 1);
    u8v(&mut out, "RequiresMembership", 0);
    u8v(&mut out, "SoulBound", 0);
    u32v(&mut out, "SoulBoundCountdown", 0xFFFF);
    out.extend_from_slice(&dfc_hash(b"ID").to_le_bytes());
    debug_assert_eq!(out.len(), 59);
    out
}

/// The item object trailer: the `Name` property (empty string) + terminator.
fn item_tail() -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.extend_from_slice(&dfc_hash(b"Name").to_le_bytes());
    out.push(0);
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// One 90-byte item object.
///
/// `root` is the item's DFC base class (`MeleeWeapon`/`RangedWeapon`/`Armor`),
/// which the readObject class id resolves through the native registry;
/// `fqn` is the item's pkg class, which the `GCNode+0x50` archetype resolves.
pub fn graph_item_entry(fqn: &str, root: &str, slot: i64, object_id: u32) -> Vec<u8> {
    assert!(
        EQUIPMENT_VALID_SLOTS.contains(&slot),
        "slot {slot} is not an EquipmentSlot id"
    );
    let classname = fqn.split('\u{0}').next().unwrap_or("");
    let mut out = Vec::with_capacity(ITEM_ENTRY_LEN);
    out.push(NODE_VERSION);
    out.extend_from_slice(&dfc_hash(root.as_bytes()).to_le_bytes());
    out.extend_from_slice(&object_id.to_le_bytes());
    out.push(0); // empty object name
    out.extend_from_slice(&0u32.to_le_bytes()); // no children
    out.extend_from_slice(&dfc_hash(classname.as_bytes()).to_le_bytes()); // archetype
    out.extend_from_slice(&item_property_block());
    out.extend_from_slice(&(slot as u32).to_le_bytes());
    out.extend_from_slice(&item_tail());
    debug_assert_eq!(out.len(), ITEM_ENTRY_LEN);
    out
}

/// The class-dependent content a graph node is built from.
#[derive(Debug, Clone)]
pub struct ClassContent {
    /// The stored class FQN (`avatar.classes.WarlockFemale`), whose hash is
    /// the Avatar node's `GCNode+0x50` archetype.
    pub aclass: String,
    /// The four numbered skills, as `(dfc root, archetype fqn)`.
    pub skills: Vec<(String, String)>,
    /// The profession entry's archetype fqn.
    pub profession_fqn: String,
    /// The four starting items.
    pub items: Vec<ClassItem>,
    /// The class's `Visuals` fqn.
    pub visuals: String,
    /// The starting skills shaped for the in-world `Skills`/`Manipulators`
    /// records.
    pub skill_wire: ClassSkillWire,
}

impl ClassContent {
    /// Derive the content for `aclass` from `game.pkg`.
    pub fn derive(corpus: &mut Corpus, aclass: &str) -> Result<ClassContent, ContentError> {
        let base = base_of(aclass);
        let kit = drf_content::derive_class_kit(corpus, base)?;
        let skill_wire = kit.skill_wire()?;
        let items = drf_content::derive_class_items(corpus, base)?;
        let visuals = drf_content::derive_class_visuals(corpus, aclass)?;
        let skills = kit
            .skill_dfc
            .iter()
            .cloned()
            .zip(kit.skill_fqns.iter().cloned())
            .collect();
        Ok(ClassContent {
            aclass: aclass.to_string(),
            skills,
            profession_fqn: kit.profession_fqn,
            items,
            visuals,
            skill_wire,
        })
    }
}

/// The class base (`Fighter`/`Ranger`/`Warlock`) a stored class names.
fn base_of(aclass: &str) -> &str {
    for base in drf_content::CLASS_BASES {
        if aclass.contains(base) {
            return base;
        }
    }
    ""
}

/// The Avatar's appearance bytes, as the store holds them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Appearance {
    pub skin: Option<u8>,
    pub face: Option<u8>,
    pub face_feature: Option<u8>,
    pub hair: Option<u8>,
    pub hair_color: Option<u8>,
}

/// A back-reference key allocator: distinct within one message.
///
/// `DFCInputStream::readObject` reads the key at @ `0x62BBFE` and inserts
/// `map[id] = object`; no branch inspects its value, so the only rule is
/// uniqueness within the message. `take` scans up from zero for the first key
/// no one has taken.
pub struct KeyMint {
    used: HashSet<u32>,
    next: u32,
}

impl KeyMint {
    pub fn new(reserved: impl IntoIterator<Item = u32>) -> KeyMint {
        KeyMint {
            used: reserved.into_iter().collect(),
            next: 0,
        }
    }

    pub fn take(&mut self) -> u32 {
        while self.used.contains(&self.next) {
            self.next = self.next.wrapping_add(1);
        }
        let key = self.next;
        self.used.insert(key);
        self.next = self.next.wrapping_add(1);
        key
    }

    pub fn take_n(&mut self, n: usize) -> Vec<u32> {
        (0..n).map(|_| self.take()).collect()
    }
}

fn asciiz(value: &str) -> Vec<u8> {
    let bytes = value.split('\u{0}').next().unwrap_or("");
    let mut out = bytes.as_bytes().to_vec();
    out.push(0);
    out
}

/// The generic GCNode prefix: version, class id, object id, class name.
fn gcnode_only(class_name: &str, node_id: u32) -> (Vec<u8>, u32) {
    let class_id = dfc_hash(class_name.as_bytes());
    let mut out = Vec::new();
    out.push(NODE_VERSION);
    out.extend_from_slice(&class_id.to_le_bytes());
    out.extend_from_slice(&node_id.to_le_bytes());
    out.extend_from_slice(&asciiz(class_name));
    (out, class_id)
}

/// Complete a GCNode: child count, children, archetype id, property records,
/// then the property-loop terminator.
fn finish_gcnode_only(
    mut out: Vec<u8>,
    class_id: u32,
    base_type_id: Option<u32>,
    children: &[Vec<u8>],
    properties: &[(u32, u32)],
    str_properties: &[(u32, &str)],
) -> Vec<u8> {
    out.extend_from_slice(&(children.len() as u32).to_le_bytes());
    for child in children {
        out.extend_from_slice(child);
    }
    out.extend_from_slice(&base_type_id.unwrap_or(class_id).to_le_bytes());
    for (id, value) in properties {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    for (id, text) in str_properties {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&asciiz(text));
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

fn build_skill_node(skill_dfc: &str, archetype_fqn: &str, node_id: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(NODE_VERSION);
    out.extend_from_slice(&dfc_hash(skill_dfc.as_bytes()).to_le_bytes());
    out.extend_from_slice(&node_id.to_le_bytes());
    out.push(0); // empty object name
    out.extend_from_slice(&0u32.to_le_bytes()); // no children
    out.extend_from_slice(&dfc_hash(archetype_fqn.as_bytes()).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // property terminator
    out
}

fn build_modifiers_node(node_id: u32) -> Vec<u8> {
    let (prefix, class_id) = gcnode_only(MODIFIERS_CLASS, node_id);
    finish_gcnode_only(prefix, class_id, None, &[], &[], &[])
}

fn build_unitbehavior_desc_node(node_id: u32) -> Vec<u8> {
    let (prefix, class_id) = gcnode_only(UNITBEHAVIORDESC_CLASS, node_id);
    finish_gcnode_only(prefix, class_id, None, &[], &[], &[])
}

fn build_unitbehavior_node(node_id: u32, desc_id: u32) -> Vec<u8> {
    let (prefix, class_id) = gcnode_only(UNITBEHAVIOR_CLASS, node_id);
    let children = [build_unitbehavior_desc_node(desc_id)];
    finish_gcnode_only(prefix, class_id, None, &children, &[], &[])
}

fn build_avatardesc_node(node_id: u32, visuals: Option<&str>) -> Vec<u8> {
    let (prefix, class_id) = gcnode_only(AVATARDESC_CLASS, node_id);
    match visuals {
        None => finish_gcnode_only(prefix, class_id, None, &[], &[], &[]),
        Some(text) => {
            let prop_id = dfc_hash(b"Visuals");
            finish_gcnode_only(prefix, class_id, None, &[], &[], &[(prop_id, text)])
        }
    }
}

fn build_manipulators_node(node_id: u32, items: &[Vec<u8>], skills: &[Vec<u8>]) -> Vec<u8> {
    let (prefix, class_id) = gcnode_only(MANIPULATORS_CLASS, node_id);
    let mut children: Vec<Vec<u8>> = items.to_vec();
    children.extend_from_slice(skills);
    finish_gcnode_only(prefix, class_id, None, &children, &[], &[])
}

fn build_skills_node(node_id: u32, professions: &[Vec<u8>]) -> Vec<u8> {
    let (prefix, class_id) = gcnode_only(SKILLS_CLASS, node_id);
    finish_gcnode_only(prefix, class_id, None, professions, &[], &[])
}

fn build_equipment_node(node_id: u32, items: &[Vec<u8>]) -> Vec<u8> {
    let (prefix, class_id) = gcnode_only(EQUIPMENT_CLASS, node_id);
    finish_gcnode_only(prefix, class_id, None, items, &[], &[])
}

/// The appearance property records, in the client's string-table order.
fn appearance_records(appearance: Option<&Appearance>) -> Vec<(u32, u32)> {
    let Some(appearance) = appearance else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, value) in [
        ("Skin", appearance.skin),
        ("Face", appearance.face),
        ("FaceFeature", appearance.face_feature),
        ("Hair", appearance.hair),
        ("HairColor", appearance.hair_color),
    ] {
        if let Some(value) = value {
            out.push((dfc_hash(name.as_bytes()), value as u32));
        }
    }
    out
}

/// The `Avatar` child of a `(04,02)`/`(04,03)` Player node.
///
/// `keys` is the message's key allocator, threaded in so the Avatar's children
/// and the item/skill nodes draw from the SAME id space as the Player node.
pub fn avatar_graph_node(
    node_id: u32,
    keys: &mut KeyMint,
    content: Option<&ClassContent>,
    appearance: Option<&Appearance>,
) -> Vec<u8> {
    let with_skills = content.is_some();
    let n_children = if with_skills { 6 } else { 5 };
    let child_ids = keys.take_n(n_children);

    let mut skill_nodes: Vec<Vec<u8>> = Vec::new();
    let mut profession_nodes: Vec<Vec<u8>> = Vec::new();
    let mut equipment_items: Vec<Vec<u8>> = Vec::new();
    let mut manipulator_items: Vec<Vec<u8>> = Vec::new();
    let visuals = content.map(|c| c.visuals.as_str());

    if let Some(content) = content {
        for (dfc, fqn) in &content.skills {
            skill_nodes.push(build_skill_node(dfc, fqn, keys.take()));
        }
        profession_nodes.push(build_skill_node(
            SKILL_PROFESSION_CLASS,
            &content.profession_fqn,
            keys.take(),
        ));
        for item in &content.items {
            equipment_items.push(graph_item_entry(
                &item.fqn,
                &item.dfc,
                item.slot,
                keys.take(),
            ));
        }
        for item in &content.items {
            manipulator_items.push(graph_item_entry(
                &item.fqn,
                &item.dfc,
                item.slot,
                keys.take(),
            ));
        }
    }

    let children: Vec<Vec<u8>> = if with_skills {
        vec![
            build_modifiers_node(child_ids[0]),
            build_unitbehavior_node(child_ids[1], keys.take()),
            build_avatardesc_node(child_ids[2], visuals),
            build_manipulators_node(child_ids[3], &manipulator_items, &skill_nodes),
            build_skills_node(child_ids[4], &profession_nodes),
            build_equipment_node(child_ids[5], &equipment_items),
        ]
    } else {
        vec![
            build_modifiers_node(child_ids[0]),
            build_unitbehavior_node(child_ids[1], keys.take()),
            build_avatardesc_node(child_ids[2], None),
            build_manipulators_node(child_ids[3], &[], &[]),
            build_equipment_node(child_ids[4], &[]),
        ]
    };

    let (prefix, class_id) = gcnode_only(AVATAR_CLASS, node_id);
    let base_type_id = content.map(|c| dfc_hash(c.aclass.as_bytes()));
    finish_gcnode_only(
        prefix,
        class_id,
        base_type_id,
        &children,
        &appearance_records(appearance),
        &[],
    )
}

/// One `(04,02)`/`(04,03)` character-graph Player node.
pub fn player_graph_node(
    name: &str,
    node_id: u32,
    content: Option<&ClassContent>,
    appearance: Option<&Appearance>,
) -> Vec<u8> {
    let player_id = dfc_hash(PLAYER_CLASS.as_bytes());
    let mut keys = KeyMint::new([node_id]);
    let avatar_id = keys.take();
    let raw = asciiz(name);

    let mut out = Vec::new();
    out.push(NODE_VERSION);
    out.extend_from_slice(&player_id.to_le_bytes());
    out.extend_from_slice(&node_id.to_le_bytes());
    out.extend_from_slice(&asciiz(name)); // GCObject+0x10: the tab name
    out.extend_from_slice(&0u32.to_le_bytes()); // GCObject child count
    out.extend_from_slice(&player_id.to_le_bytes()); // GCNode base-type id
    out.extend_from_slice(&0u32.to_le_bytes()); // property terminator
    out.extend_from_slice(&raw); // Player+0xC0
    out.push(0); // Player+0xC4
    out.extend_from_slice(&0u32.to_le_bytes()); // Player+0xA8
    out.extend_from_slice(&0u32.to_le_bytes()); // Player+0xAC
    out.extend_from_slice(&avatar_graph_node(
        avatar_id, &mut keys, content, appearance,
    )); // +0xB0
    out.push(0); // Player+0xA0
    out.push(0); // helper flag
    out.push(0); // helper string
    out.push(0); // helper mode
    out.push(0); // helper +0x31
    out.extend_from_slice(&0u32.to_le_bytes()); // Player+0x100
    out
}

/// The `(04,02)` created reply body: `[u32 sqlid][name][node]`.
pub fn created_payload(
    sqlid: u32,
    name: &str,
    content: Option<&ClassContent>,
    appearance: Option<&Appearance>,
) -> Vec<u8> {
    let mut out = sqlid.to_le_bytes().to_vec();
    out.extend_from_slice(&asciiz(name));
    out.extend_from_slice(&player_graph_node(name, sqlid, content, appearance));
    out
}

/// One `(04,03)` list row: `(sqlid, name[, content[, appearance]])`.
pub struct ListRow<'a> {
    pub sqlid: u32,
    pub name: &'a str,
    pub content: Option<&'a ClassContent>,
    pub appearance: Option<&'a Appearance>,
}

/// The `(04,03)` list body: `[u8 count]` then per row `[u32 sqlid][node]`.
pub fn list_payload(rows: &[ListRow]) -> Vec<u8> {
    assert!(
        rows.len() <= 255,
        "too many characters for a one-byte count"
    );
    let mut out = vec![rows.len() as u8];
    for row in rows {
        out.extend_from_slice(&row.sqlid.to_le_bytes());
        out.extend_from_slice(&player_graph_node(
            row.name,
            row.sqlid,
            row.content,
            row.appearance,
        ));
    }
    out
}

/// The join `0x08 player` body: `[u16 eid][0xFF]["player" NUL][node]`.
pub fn join_player_body(
    entity_id: u16,
    name: &str,
    sqlid: u32,
    content: Option<&ClassContent>,
    appearance: Option<&Appearance>,
) -> Vec<u8> {
    assert_ne!(entity_id, 0, "entity ID 0 is the unassigned sentinel");
    let mut out = Vec::new();
    out.extend_from_slice(&entity_id.to_le_bytes());
    out.push(0xFF);
    out.extend_from_slice(b"player\0");
    out.extend_from_slice(&player_graph_node(name, sqlid, content, appearance));
    out
}
