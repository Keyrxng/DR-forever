// SPDX-License-Identifier: GPL-3.0-or-later

//! CEM record bodies: entity create/remove, items, acquire and intervals.
//!
//! The layouts match the client's own readers: `processEntityCreate` reads a
//! `u16` entity id and a GC type (classname tag `0xFF`), `processEntityCreateInit`
//! reads the ItemObject tail, and the interval container reads three `u32`
//! budget words before the entity stream.

use std::fmt;

use crate::cem::{
    CEM_CHANNEL, CEM_CREATE, CEM_CREATE_INIT, CEM_ENTITY_INIT, CEM_INIT_SUB_ENTITIES, CEM_REMOVE,
    CONNECTED_CASE, END_MARKER, FLAG_NONUNIT, TYPE_TAG_CLASSNAME,
};
use crate::entity_init::WORLD_ENTITY_FLAGS_BASE;
use crate::item::item_readdata_body;

/// The classname an `ItemObject` create body carries.
pub const ITEMOBJECT_CLASS: &str = "ItemObject";

/// The `ItemObject` `WorldEntity` flags word: the base both the WorldEntity
/// constructor (`orl $0x6`) and the live 191-byte loot drop carry. Bit `0x04`
/// is the scene-attach gate `SimpleMapRenderer::render` tests, so a clear bit
/// would gate the drop off screen.
pub const ITEMOBJECT_WORLD_ENTITY_FLAGS: u32 = WORLD_ENTITY_FLAGS_BASE;

/// The `ItemObject` state byte a fresh ground drop's init carries
/// (`ItemObject::writeInit`, `ItemObject+0x101`). It is the spawn-animation
/// selector the client reads in `ItemObject::updateState @0x58A410`: state `0`
/// falls to `initState(1) @0x58A1E0`, which attaches a `FlipController` and
/// runs the pop/settle state machine, whereas `1`/`6` are inert "present"
/// states. The live 191-byte dungeon drop ships `0`, so a drop visibly flips
/// out and settles; the earlier default `1` left our drop static.
pub const ITEMOBJECT_STATE_DEFAULT: u8 = 0;

/// The default `ItemObject` owner word (`+0x104`, `w1`). Non-zero so the
/// owner-name field is present (`setOwningPlayer` shape).
pub const ITEMOBJECT_W1_DEFAULT: u32 = 1;

/// The `ItemObject` byte after `timer` (`ItemObject+0x118` bit 0, the byte
/// `ItemObject::readInit @0x58B0F0` stores and `writeInit @0x58AFF0` writes).
/// The client's `ItemObject::addToWorld(World*) @0x58A0A0` branches on it:
/// set selects `initState(0)` (the animated spawn), clear forces the static
/// `initState(6)`. The live dungeon drop carries `1`; our drop carried `0`,
/// which selected the static branch.
pub const ITEMOBJECT_SPAWN_FLAG: u8 = 1;

/// The default `ItemObject` despawn timer (`ItemObject+0x110`, the `timer`
/// init word). The live dungeon drop carries `27000`; `ItemObject::update
/// @0x58A140` retires a drop whose timer reaches `< 1`, so `0` is unsafe.
pub const ITEMOBJECT_TIMER_DEFAULT: u32 = 27_000;

/// The `(07,0x33)` pickup-acquire message type.
pub const CEM_ACQUIRE_NOTICE: u8 = 0x33;

/// The PathManager block the interval container carries after the budgets.
pub const INTERVAL_PATHMANAGER_BLOCK: [u8; 8] = [0x01, 0x00, 0x00, 0x00, 0x64, 0x00, 0x14, 0x00];

/// The default town interval budgets `(tick, interval_ms, budget)`.
pub const TOWN_BUDGET_TRIPLE: (u32, u32, u32) = (10_544_915, 33, 3);

/// A create-body shape violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateError {
    /// Entity id zero is the unassigned sentinel.
    ZeroEntityId,
    /// The classname is empty or longer than 128 bytes.
    BadClassname,
    /// The body does not contain enough bytes for its layout.
    TooShort(&'static str),
    /// The GC type tag is not the classname tag.
    BadTypeTag(u8),
    /// A NUL-terminated string is missing its terminator.
    MissingNul,
    /// The classname is empty.
    EmptyClassname,
    /// Bytes remain after a bare create body.
    TrailingBytes,
    /// The body is not an ItemObject create.
    NotItemObject,
    /// The ItemObject flag word is not zero.
    BadFlags(u8),
    /// The item-description tag is missing.
    MissingItemDescTag,
    /// The item description is not NUL-terminated.
    MissingItemDescTerminator,
    /// An item with `w1 != 0` has no owner name.
    EmptyOwner,
    /// A stream terminator is neither END nor CONNECTED.
    BadTerminator(u8),
    /// A rich record is not a createinit record.
    RichNotCreateInit(u8),
    /// A component names an owner outside the stream.
    ComponentOwnerNotInStream(u16),
}

impl fmt::Display for CreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CreateError::ZeroEntityId => write!(f, "entity ID 0 is the unassigned sentinel"),
            CreateError::BadClassname => write!(f, "entity classname shape"),
            CreateError::TooShort(what) => write!(f, "{what} too short"),
            CreateError::BadTypeTag(tag) => write!(f, "unsupported type-tag {tag:#04x}"),
            CreateError::MissingNul => write!(f, "classname missing NUL"),
            CreateError::EmptyClassname => write!(f, "create classname empty"),
            CreateError::TrailingBytes => write!(f, "bare create body must have zero tail"),
            CreateError::NotItemObject => write!(f, "not an ItemObject body"),
            CreateError::BadFlags(flags) => write!(f, "ItemObject must carry flags 0, got {flags}"),
            CreateError::MissingItemDescTag => write!(f, "missing item-desc tag"),
            CreateError::MissingItemDescTerminator => write!(f, "item desc not NUL-terminated"),
            CreateError::EmptyOwner => write!(f, "item owner name is empty"),
            CreateError::BadTerminator(end) => {
                write!(
                    f,
                    "CEM stream terminator must be 0x06 or 0x46, got {end:#04x}"
                )
            }
            CreateError::RichNotCreateInit(tag) => {
                write!(
                    f,
                    "rich entity record must be 0x08 createinit, got {tag:#04x}"
                )
            }
            CreateError::ComponentOwnerNotInStream(owner) => {
                write!(f, "component owner {owner} is not an entity in this stream")
            }
        }
    }
}

impl std::error::Error for CreateError {}

/// A parsed bare create body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateBody {
    pub entity_id: u16,
    pub classname: Vec<u8>,
}

/// A parsed ItemObject create body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemCreateBody {
    pub entity_id: u16,
    pub classname: Vec<u8>,
    pub flags: u8,
    pub hp: Option<u32>,
    pub state: u8,
    pub slot: u16,
    pub w1: u32,
    pub w2: u32,
    pub timer: u32,
    pub lock: u8,
    pub owner: Vec<u8>,
    pub itemdesc: Vec<u8>,
}

/// A parsed pickup-acquire payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquirePayload {
    pub drop_id: u16,
    pub avatar_id: u16,
    pub itemdesc: Vec<u8>,
}

fn asciiz(value: &str) -> &[u8] {
    value.split('\u{0}').next().unwrap_or("").as_bytes()
}

/// The bare entity create body: `[u16 id][0xFF][classname NUL]`.
pub fn create_body(entity_id: u16, classname: &str) -> Result<Vec<u8>, CreateError> {
    if entity_id == 0 {
        return Err(CreateError::ZeroEntityId);
    }
    let name = asciiz(classname);
    if name.is_empty() || name.len() > 128 {
        return Err(CreateError::BadClassname);
    }
    let mut out = Vec::with_capacity(3 + name.len() + 1);
    out.extend_from_slice(&entity_id.to_le_bytes());
    out.push(TYPE_TAG_CLASSNAME);
    out.extend_from_slice(name);
    out.push(0);
    Ok(out)
}

/// Parse a bare entity create body.
pub fn parse_create_body(body: &[u8]) -> Result<CreateBody, CreateError> {
    if body.len() < 5 {
        return Err(CreateError::TooShort("create body"));
    }
    let entity_id = u16::from_le_bytes([body[0], body[1]]);
    if entity_id == 0 {
        return Err(CreateError::ZeroEntityId);
    }
    if body[2] != TYPE_TAG_CLASSNAME {
        return Err(CreateError::BadTypeTag(body[2]));
    }
    let nul = body[3..]
        .iter()
        .position(|&byte| byte == 0)
        .map(|pos| pos + 3)
        .ok_or(CreateError::MissingNul)?;
    let classname = body[3..nul].to_vec();
    if classname.is_empty() {
        return Err(CreateError::EmptyClassname);
    }
    if nul + 1 != body.len() {
        return Err(CreateError::TrailingBytes);
    }
    Ok(CreateBody {
        entity_id,
        classname,
    })
}

/// The single-entity remove inner: `[07][05][u16 id][06]`.
pub fn remove_inner(entity_id: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.extend_from_slice(&[CEM_CHANNEL, CEM_REMOVE]);
    out.extend_from_slice(&entity_id.to_le_bytes());
    out.push(END_MARKER);
    out
}

/// The CEM entity-remove frame: `[07][05][u16 id][06]`.
///
/// An alias of [`remove_inner`] under the name the death path uses.
pub fn remove_frame(entity_id: u16) -> Vec<u8> {
    remove_inner(entity_id)
}

/// Parse a single-entity remove inner.
pub fn parse_remove_inner(inner: &[u8]) -> Result<u16, CreateError> {
    if inner.len() != 5 {
        return Err(CreateError::TooShort("remove inner"));
    }
    if inner[0] != CEM_CHANNEL || inner[1] != CEM_REMOVE {
        return Err(CreateError::BadTypeTag(inner[1]));
    }
    if inner[4] != END_MARKER {
        return Err(CreateError::BadTerminator(inner[4]));
    }
    Ok(u16::from_le_bytes([inner[2], inner[3]]))
}

/// The ItemObject create body.
///
/// Layout: `[u16 id][0xFF]["ItemObject" NUL][flags 0][state u8][slot u16]`
/// `[w1 u32][w2 u32][timer u32][lock u8][owner NUL iff w1 != 0]`
/// `[0xFF][itemdesc NUL]`.
#[allow(clippy::too_many_arguments)]
pub fn item_create_body(
    drop_id: u16,
    itemdesc: &str,
    owner: &str,
    state: u8,
    slot: u16,
    w1: u32,
    w2: u32,
    timer: u32,
    lock: u8,
) -> Result<Vec<u8>, CreateError> {
    if drop_id == 0 {
        return Err(CreateError::ZeroEntityId);
    }
    let desc = asciiz(itemdesc);
    let own = asciiz(owner);
    if own.is_empty() {
        return Err(CreateError::EmptyOwner);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&drop_id.to_le_bytes());
    out.push(TYPE_TAG_CLASSNAME);
    out.extend_from_slice(ITEMOBJECT_CLASS.as_bytes());
    out.push(0);
    out.push(FLAG_NONUNIT);
    out.push(state);
    out.extend_from_slice(&slot.to_le_bytes());
    out.extend_from_slice(&w1.to_le_bytes());
    out.extend_from_slice(&w2.to_le_bytes());
    out.extend_from_slice(&timer.to_le_bytes());
    out.push(lock);
    if w1 != 0 {
        out.extend_from_slice(own);
        out.push(0);
    }
    out.push(TYPE_TAG_CLASSNAME);
    out.extend_from_slice(desc);
    out.push(0);
    Ok(out)
}

/// Parse an ItemObject create body.
pub fn parse_item_create_body(body: &[u8]) -> Result<ItemCreateBody, CreateError> {
    if body.len() < 3 {
        return Err(CreateError::TooShort("item body"));
    }
    let entity_id = u16::from_le_bytes([body[0], body[1]]);
    if entity_id == 0 {
        return Err(CreateError::ZeroEntityId);
    }
    if body[2] != TYPE_TAG_CLASSNAME {
        return Err(CreateError::BadTypeTag(body[2]));
    }
    let nul = body[3..]
        .iter()
        .position(|&byte| byte == 0)
        .map(|pos| pos + 3)
        .ok_or(CreateError::MissingNul)?;
    if &body[3..nul] != ITEMOBJECT_CLASS_BYTES {
        return Err(CreateError::NotItemObject);
    }

    let off = nul + 1;
    if body.len() < off + 17 {
        return Err(CreateError::TooShort("item body"));
    }
    let flags = body[off];
    if flags != FLAG_NONUNIT {
        return Err(CreateError::BadFlags(flags));
    }
    let state = body[off + 1];
    let slot = u16::from_le_bytes([body[off + 2], body[off + 3]]);
    let w1 = u32::from_le_bytes([body[off + 4], body[off + 5], body[off + 6], body[off + 7]]);
    let w2 = u32::from_le_bytes([body[off + 8], body[off + 9], body[off + 10], body[off + 11]]);
    let timer = u32::from_le_bytes([
        body[off + 12],
        body[off + 13],
        body[off + 14],
        body[off + 15],
    ]);
    let lock = body[off + 16];
    let mut rest = &body[off + 17..];

    let owner = if w1 != 0 {
        let onul = rest
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(CreateError::MissingNul)?;
        let owner = rest[..onul].to_vec();
        if owner.is_empty() {
            return Err(CreateError::EmptyOwner);
        }
        rest = &rest[onul + 1..];
        owner
    } else {
        Vec::new()
    };

    if rest.is_empty() || rest[0] != TYPE_TAG_CLASSNAME {
        return Err(CreateError::MissingItemDescTag);
    }
    if rest.last() != Some(&0) {
        return Err(CreateError::MissingItemDescTerminator);
    }
    let itemdesc = rest[1..rest.len() - 1].to_vec();

    Ok(ItemCreateBody {
        entity_id,
        classname: ITEMOBJECT_CLASS_BYTES.to_vec(),
        flags,
        hp: None,
        state,
        slot,
        w1,
        w2,
        timer,
        lock,
        owner,
        itemdesc,
    })
}

const ITEMOBJECT_CLASS_BYTES: &[u8] = ITEMOBJECT_CLASS.as_bytes();

/// One ground drop's own `Item::readData` body, appended after the
/// `ItemObject` init's item-class NUL.
///
/// This is the body the client's `Item::readData @0x581710` reads and the body
/// the reference `GCObject.WriteInitForDroppedItem` / `WriteItemData` writes
/// (`reference/external/DRS-NET/.../AuthoredData/GCObject.cs`). Layout:
///
/// ```text
/// [u32 container_id][u8 x=0xFF][u8 y=0xFF][u8 quantity][u8 level]
/// [u8 flags=0][authored_slots x 0x00][count]
/// [(0xFF classname NUL)(0x01)(mod_level)]...
/// ```
///
/// The container-add record in [`crate::item`] is the SAME field order but a
/// different framing: its id and grid cell are re-addressed per bag slot,
/// whereas a drop is unplaced (`0xFF 0xFF`) with id `0`. The reference's simple
/// consumable branch (`write_init_for_dropped_item`) is this body with no
/// modifier children; the equipment branch adds them.
///
/// The equipment form's `count` + `ItemModifier` children ARE the reference's
/// "ScaleMod block". The reference Python's `_write_scale_mod_block` writes that
/// child list as by-hash refs (`0x04 <u32>`) with a `0x03 0x15 <u32>` body and a
/// trailing by-name ScaleMod child; the reference C# `WriteItemData` and the
/// live client capture both write `0x04 <u32 hash> <flags> [<level>]`, and the
/// live capture our own `item.rs` shares writes by-name (`0xFF <classname>
/// 0x01 <level>`). This port follows the live capture / `item.rs` form; the
/// client's `GCClassRegistry::readType` accepts either tag.
///
/// The three grid bytes follow the live dungeon drop and
/// [`crate::item::EQUIPMENT_ITEM_GRID_BYTES`] (the unplaced sentinel); the
/// reference C# `WriteItemData` call passes `(0, 0)` for a drop, but the live
/// client accepted the sentinel and a ground drop is not in a bag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedItemBody {
    /// `Item+0x68`, the server-minted id; `0` for a fresh ground drop.
    pub item_id: u32,
    /// `Item+0x7f`, the item's effective level.
    pub level: u8,
    /// `ItemReadDataSlotCount - 1`, the item's own instantiated modifier slots.
    pub authored_slots: usize,
    /// The item's rolled `ItemModifier` classnames, in slot order.
    pub modifiers: Vec<String>,
    /// `ItemModifier`'s level byte (the level the affixes roll at).
    pub modifier_level: u8,
    /// `Currency::readData`'s trailing u32 amount, for a native `Currency`
    /// object. `None` for every ordinary item.
    pub currency_amount: Option<u32>,
}

impl DroppedItemBody {
    /// The simple consumable form: no modifier children.
    pub fn consumable(level: u8) -> DroppedItemBody {
        DroppedItemBody {
            item_id: 0,
            level,
            authored_slots: 0,
            modifiers: Vec::new(),
            modifier_level: level,
            currency_amount: None,
        }
    }

    /// The equipment form: `modifiers` follow the count as `ItemModifier`
    /// children. `authored_slots` is the item's own instantiated slot count
    /// (`ItemReadDataSlotCount - 1`); pass `0` when it is not derived.
    pub fn equipment(
        level: u8,
        authored_slots: usize,
        modifiers: Vec<String>,
        modifier_level: u8,
    ) -> DroppedItemBody {
        DroppedItemBody {
            item_id: 0,
            level,
            authored_slots,
            modifiers,
            modifier_level,
            currency_amount: None,
        }
    }

    /// The native `Currency` form: an empty `Item::readData` body followed by
    /// `Currency+0x88`'s u32 amount (`Currency::readData` tail).
    pub fn currency(amount: u32) -> DroppedItemBody {
        DroppedItemBody {
            item_id: 0,
            level: 0,
            authored_slots: 0,
            modifiers: Vec::new(),
            modifier_level: 1,
            currency_amount: Some(amount),
        }
    }

    /// The form for `class`: the simple consumable form for a potion / scroll /
    /// voucher / ... class, else the equipment form with no rolled mods.
    ///
    /// `authored_slots` is the class's own `ItemModifier`-derived child count
    /// (`Corpus::item_readdata_slots`, the status bytes `Item::readData` reads
    /// before the rolled-modifier list). It is ignored for a consumable, whose
    /// simple form has no status byte. Passing `0` for equipment is only
    /// correct for a class that declares no item modifier at all — the value
    /// is what routes the client's `ReadChildData<ItemModifier>` loop, so a
    /// wrong count desyncs the rest of the CEM stream.
    ///
    /// The keyword list is the reference `GCObject._is_consumable_type`'s
    /// (minus its native-class terms, which this server does not carry).
    pub fn for_class(class: &str, level: u8, authored_slots: usize) -> DroppedItemBody {
        if is_consumable_class(class) {
            DroppedItemBody::consumable(level)
        } else {
            DroppedItemBody::equipment(level, authored_slots, Vec::new(), level)
        }
    }

    /// Append the body (`Item::readData`, after the classname and NUL).
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&item_readdata_body(
            i64::from(self.item_id),
            self.level,
            self.authored_slots,
            &self.modifiers,
            self.modifier_level,
        ));
        if let Some(amount) = self.currency_amount {
            out.extend_from_slice(&amount.to_le_bytes());
        }
    }
}

/// Whether `class` names a consumable (the reference
/// `GCObject._is_consumable_type` keyword list).
///
/// `ActiveItem` / `ActiveSkill` / `PassiveSkill` are also consumable in the
/// reference, but this server does not carry an item's native class, so the
/// keyword terms alone decide.
pub fn is_consumable_class(class: &str) -> bool {
    let lower = class.to_ascii_lowercase();
    [
        "potion",
        "scroll",
        "skillbook",
        "voucher",
        "townportal",
        "questitem",
        "itempal",
        "consumable",
    ]
    .iter()
    .any(|term| lower.contains(term))
}

/// The `ItemObject` `writeInit` class-state payload (the `0x02`/`0x08` body).
///
/// The reader is `ItemObject::writeInit @0x58AFF0`: the `WorldEntity` base
/// (`u32 flags, u32 x, u32 y, u32 z, u32 aux/heading, u8 fb`; `fb = 0` so no
/// gated tails), then `u8 state, u16 slot, u32 w1, u32 w2, u32 timer, u8
/// spawn_flag`, the owner name (`ASCIIZ`, only when `w1 != 0`), the item's real
/// GC class as a `0xFF`-tagged `ASCIIZ`, and then the item's own
/// `write_init_for_dropped_item` body ([`DroppedItemBody`]).
///
/// `spawn_flag` is the `ItemObject+0x118` bit 0 byte (`ItemObject::addToWorld
/// @0x58A0A0` reads it to pick the animated `initState(0)` over the static
/// `initState(6)`); see [`ITEMOBJECT_SPAWN_FLAG`]. It is NOT the pickup lock.
///
/// This is the byte-faithful form the reference `messages.build_item_create_body`
/// is NOT: that builder omits the 20-byte `WorldEntity` pose quad (five `u32`),
/// the `fb` gate byte, AND the item's own `Item::readData` body, so its single
/// "flags 0" byte lands where the pose begins and its read runs off the end of
/// the drop. The live 191-byte dungeon drop ships the full base
/// (`flags = 0x6`, `fb = 0`) and the full item body, which is what this emits.
///
/// The item's real GC class goes here, NOT in the entity-create classname: the
/// entity is an `ItemObject`.
#[allow(clippy::too_many_arguments)]
pub fn item_object_init_payload(
    item_class: &str,
    pose: [u32; 4],
    owner: &str,
    flags: u32,
    state: u8,
    slot: u16,
    w1: u32,
    w2: u32,
    timer: u32,
    spawn_flag: u8,
    body: &DroppedItemBody,
) -> Result<Vec<u8>, CreateError> {
    let desc = asciiz(item_class);
    if desc.is_empty() {
        return Err(CreateError::BadClassname);
    }
    let own = asciiz(owner);
    if w1 != 0 && own.is_empty() {
        return Err(CreateError::EmptyOwner);
    }
    let [x, y, z, aux] = pose;
    let mut out = Vec::with_capacity(20 + 1 + 17 + own.len() + 1 + desc.len() + 2 + 16);
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&z.to_le_bytes());
    out.extend_from_slice(&aux.to_le_bytes());
    out.push(0); // fb: no gated tails
    out.push(state);
    out.extend_from_slice(&slot.to_le_bytes());
    out.extend_from_slice(&w1.to_le_bytes());
    out.extend_from_slice(&w2.to_le_bytes());
    out.extend_from_slice(&timer.to_le_bytes());
    out.push(spawn_flag);
    if w1 != 0 {
        out.extend_from_slice(own);
        out.push(0);
    }
    out.push(TYPE_TAG_CLASSNAME);
    out.extend_from_slice(desc);
    out.push(0);
    body.write(&mut out);
    Ok(out)
}

/// The full `ItemObject` create+init body: the `0x08` CEM createinit record
/// after its `[07][08]` tag and before the `0x06` stream end.
///
/// Layout: `[u16 eid][0xFF]["ItemObject" NUL]` + [`item_object_init_payload`]
/// with the drop defaults (`flags = 0x6`, `state = 0`, `slot = 0`, `w1 = 1`,
/// `w2 = 0`, `timer = 27000`, owner-flag byte `= 1`), then the `0x06`
/// terminator. `state = 0` and the `+0x118` owner-flag byte `= 1` are the
/// client's spawn-animation selector (see [`ITEMOBJECT_STATE_DEFAULT`] and
/// [`ITEMOBJECT_SPAWN_FLAG`]); `w1 = 1` carries the owner name. These are the
/// live 191-byte dungeon drop's own values, so the drop plays the native
/// pop/settle flip instead of popping in static. (The byte is not the client's
/// pickup lock: `ItemObject::validatePick`/`ItemIsLocked` read the watcher
/// words at `+0xe4`/`+0xf0`/`+0xc8`/`+0xd4`, not `+0x118`.)
///
/// The `[07 08]` channel/mtype pair is NOT written here -- the caller's `send`
/// prepends it -- but the `0x06` is, because this is a complete single-record
/// CEM stream: the live drop is `07 08 ... 06`, and a createinit with no item
/// body and no terminator is what desynced the client.
///
/// ## Framing: the combined `0x08`, not the reference's `0x01` + `0x02`
///
/// The reference Python (`dr_pyserver/managers/loot.py::write_create_and_position`)
/// frames a drop as a `0x01` create (classname `itemobject`, lowercase) followed
/// by a `0x02` init whose body is a reverse-engineered drop-controller header
/// (`u32 0x06`, then `0xF7`, `u16 0x2233`, ...) -- NOT the plain
/// `ItemObject::writeInit` field order. The reference itself marks that header
/// UNVERIFIED-LIVE. The live 191-byte client capture (`_COMBAT_DROP_HEX`) is the
/// opposite: one combined `0x08` createinit, classname `ItemObject` (capital),
/// whose body IS the plain `WorldEntity` + `ItemObject` readInit order (the five
/// `u32` pose words, the `fb` gate, `state`/`slot`/`w1`/`w2`/`timer`/spawn-flag,
/// the owner, the item class, then the item body). The live client accepted that
/// capture and this server's existing drop framing already matched it, so the
/// combined `0x08` + readInit order is the one ported and only the missing item
/// body (and terminator) were added.
pub fn build_item_object_create_init(
    drop_eid: u16,
    item_class: &str,
    pose: [u32; 4],
    owner: &str,
    body: &DroppedItemBody,
) -> Result<Vec<u8>, CreateError> {
    if drop_eid == 0 {
        return Err(CreateError::ZeroEntityId);
    }
    let payload = item_object_init_payload(
        item_class,
        pose,
        owner,
        ITEMOBJECT_WORLD_ENTITY_FLAGS,
        ITEMOBJECT_STATE_DEFAULT,
        0,
        ITEMOBJECT_W1_DEFAULT,
        0,
        ITEMOBJECT_TIMER_DEFAULT,
        ITEMOBJECT_SPAWN_FLAG,
        body,
    )?;
    let mut out = Vec::with_capacity(4 + ITEMOBJECT_CLASS.len() + 1 + payload.len());
    out.extend_from_slice(&drop_eid.to_le_bytes());
    out.push(TYPE_TAG_CLASSNAME);
    out.extend_from_slice(ITEMOBJECT_CLASS.as_bytes());
    out.push(0);
    out.extend_from_slice(&payload);
    out.push(END_MARKER);
    Ok(out)
}

/// The `items.townportal.TownPortalBlue` create-init body for `(07,0x08)`.
///
/// The portal is parented to the owning avatar (`initFlags 0x01`) and trails
/// `[target zone][empty cstring][state][u32 0][zone GUID]`, exactly the C#
/// `SpawnTownPortalWithRemoval` layout (not the zone-portal init).
pub fn build_town_portal_create_init(
    entity_id: u16,
    avatar_id: u16,
    pos: [i32; 3],
    target_zone: &str,
    zone_guid: u32,
    flags: u32,
    state: u8,
) -> Result<Vec<u8>, CreateError> {
    if entity_id == 0 {
        return Err(CreateError::ZeroEntityId);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&entity_id.to_le_bytes());
    out.push(TYPE_TAG_CLASSNAME);
    out.extend_from_slice(b"items.townportal.TownPortalBlue\0");
    out.extend_from_slice(&flags.to_le_bytes());
    for value in pos {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&0i32.to_le_bytes()); // heading
    out.push(0x01); // initFlags: has parent
    out.extend_from_slice(&avatar_id.to_le_bytes());
    out.extend_from_slice(target_zone.as_bytes());
    out.push(0);
    out.push(0); // empty name/spawn cstring
    out.push(state);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&zone_guid.to_le_bytes());
    out.push(END_MARKER);
    Ok(out)
}

/// The tagged `(07,0x33)` pickup-acquire notice:
/// `[07][33][drop u16][avatar u16][itemdesc NUL]`.
///
/// Emitted after the drop's `07 05 <drop> 06` remove when the player claims a
/// drop; the item desc is the class the drop's init carried. The body is
/// [`acquire_payload`].
pub fn build_acquire_notice(
    drop_id: u16,
    avatar_eid: u16,
    itemdesc: &str,
) -> Result<Vec<u8>, CreateError> {
    let body = acquire_payload(drop_id, avatar_eid, itemdesc)?;
    let mut out = Vec::with_capacity(2 + body.len());
    out.extend_from_slice(&[CEM_CHANNEL, CEM_ACQUIRE_NOTICE]);
    out.extend_from_slice(&body);
    Ok(out)
}

/// The pickup-acquire payload: `[drop u16][avatar u16][itemdesc NUL]`.
pub fn acquire_payload(
    drop_id: u16,
    avatar_eid: u16,
    itemdesc: &str,
) -> Result<Vec<u8>, CreateError> {
    let mut out = Vec::new();
    out.extend_from_slice(&drop_id.to_le_bytes());
    out.extend_from_slice(&avatar_eid.to_le_bytes());
    out.extend_from_slice(asciiz(itemdesc));
    out.push(0);
    Ok(out)
}

/// Parse a pickup-acquire payload.
pub fn parse_acquire_payload(pay: &[u8]) -> Result<AcquirePayload, CreateError> {
    if pay.len() < 5 || pay.last() != Some(&0) {
        return Err(CreateError::TooShort("acquire payload"));
    }
    Ok(AcquirePayload {
        drop_id: u16::from_le_bytes([pay[0], pay[1]]),
        avatar_id: u16::from_le_bytes([pay[2], pay[3]]),
        itemdesc: pay[4..pay.len() - 1].to_vec(),
    })
}

/// The interval budgets payload: `[u32 tick][u32 interval_ms][u32 budget]`.
pub fn interval_payload(tick: u32, interval_ms: u32, budget: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&tick.to_le_bytes());
    out.extend_from_slice(&interval_ms.to_le_bytes());
    out.extend_from_slice(&budget.to_le_bytes());
    out
}

/// The smallest legal entity stream.
///
/// `raw` supplies complete records (type byte included) emitted verbatim;
/// `rich` supplies `(id, tag, payload)` createinit records; `inits` supplies
/// class payloads emitted as `0x02` init records after a create.
pub fn minimal_entity_stream(
    entities: &[(u16, &str)],
    end: u8,
    components: &[(u16, &[u8])],
    rich: &[(u16, u8, &[u8])],
    inits: &[(u16, &[u8])],
    raw: &[(u16, &[u8])],
) -> Result<Vec<u8>, CreateError> {
    let mut out = Vec::new();

    for &(entity_id, classname) in entities {
        if entity_id == 0 {
            return Err(CreateError::ZeroEntityId);
        }
        let name = asciiz(classname);
        if name.is_empty() || name.len() > 128 {
            return Err(CreateError::BadClassname);
        }

        if let Some(record) = raw
            .iter()
            .find(|entry| entry.0 == entity_id)
            .map(|entry| entry.1)
        {
            out.extend_from_slice(record);
            if let Some(init) = inits
                .iter()
                .find(|entry| entry.0 == entity_id)
                .map(|entry| entry.1)
            {
                out.push(CEM_ENTITY_INIT);
                out.extend_from_slice(&entity_id.to_le_bytes());
                out.extend_from_slice(init);
            }
            continue;
        }

        if let Some(&(_, tag, payload)) = rich.iter().find(|entry| entry.0 == entity_id) {
            if tag != CEM_CREATE_INIT {
                return Err(CreateError::RichNotCreateInit(tag));
            }
            out.push(tag);
            out.extend_from_slice(&entity_id.to_le_bytes());
            out.push(TYPE_TAG_CLASSNAME);
            out.extend_from_slice(name);
            out.push(0);
            out.extend_from_slice(payload);
            continue;
        }

        out.push(CEM_CREATE);
        out.extend_from_slice(&entity_id.to_le_bytes());
        out.push(TYPE_TAG_CLASSNAME);
        out.extend_from_slice(name);
        out.push(0);
        if let Some(init) = inits
            .iter()
            .find(|entry| entry.0 == entity_id)
            .map(|entry| entry.1)
        {
            out.push(CEM_ENTITY_INIT);
            out.extend_from_slice(&entity_id.to_le_bytes());
            out.extend_from_slice(init);
        }
    }

    for &(owner, blob) in components {
        if !entities.iter().any(|entry| entry.0 == owner) {
            return Err(CreateError::ComponentOwnerNotInStream(owner));
        }
        out.extend_from_slice(blob);
    }

    if end != END_MARKER && end != CONNECTED_CASE {
        return Err(CreateError::BadTerminator(end));
    }
    if end == CONNECTED_CASE {
        out.push(CEM_INIT_SUB_ENTITIES);
    }
    out.push(end);
    Ok(out)
}

/// The interval stream body: budgets, PathManager block, entities, terminator.
#[allow(clippy::too_many_arguments)]
pub fn interval_stream(
    entities: &[(u16, &str)],
    budget_triple: (u32, u32, u32),
    pathmanager: Option<&[u8]>,
    end: u8,
    components: &[(u16, &[u8])],
    rich: &[(u16, u8, &[u8])],
    inits: &[(u16, &[u8])],
    raw: &[(u16, &[u8])],
) -> Result<Vec<u8>, CreateError> {
    let (tick, interval_ms, budget) = budget_triple;
    let mut out = interval_payload(tick, interval_ms, budget);
    if let Some(pathmanager) = pathmanager {
        out.extend_from_slice(pathmanager);
    }
    out.extend_from_slice(&minimal_entity_stream(
        entities, end, components, rich, inits, raw,
    )?);
    Ok(out)
}
