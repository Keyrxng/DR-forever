// SPDX-License-Identifier: GPL-3.0-or-later

//! The local avatar's linked `0x02` class-state head and its six `0x32`
//! component records.
//!
//! A bare `0x01` avatar create carries no class state, so `Unit::readInit`'s
//! owner tail never runs, the join `Player` never gains its `Avatar` child,
//! `Player::getAvatar` returns NULL and `SkillsControl::initialize` faults at
//! `0x4A6392` with `ECX=0`. The minimum that links is a `0x02` head whose
//! `Unit::readInit` flag word sets bit `0x01` and carries the owner PLAYER's
//! entity id, followed by the six components the in-world HUD dereferences
//! without a NULL guard (`Skills` 0x4A6394, `Equipment` 0x438C42,
//! `UnitContainer` 0x48C7CB, `Manipulators` 0x4F9BC3, `Modifiers` 0x453C38,
//! `UnitBehavior` 0x5206A5).
//!
//! The head is the 56-byte chain `WorldEntity::readInit` (0x4D3E60) +
//! `Unit::readInit` (0x50A580) + `Hero::readInit` (0x4F8A20) +
//! `Avatar::readInit` (0x4EDBF0). `WorldEntity` flag bit 2 (`0x04`) is the
//! scene-attach gate `SimpleMapRenderer::render` (0x4D8280) tests, so the join
//! passes [`MINIMAL_AVATAR_WE_FLAGS_ATTACH`]. The pose words are the zone's
//! resolved spawn ([`drf_content::derive_zone_spawn`]); the level is the
//! store character's.

use std::fmt;

use drf_content::ClassItem;

use crate::item::{
    class_starting_equipment_payload, class_starting_item_manipulators_bytes, encode_list_cstr,
    push_list_count, AVATAR_EQUIPMENT_CLASS,
};
use crate::player::component_create_inner;

/// The class-state bytes of the minimal linked avatar head, before any
/// explicit HP/MP words (`WorldEntity` 21 + `Unit` 8 + `Hero` 24 + `Avatar` 3).
pub const MINIMAL_LINKED_AVATAR_HEAD_LEN: usize = 56;

/// `WorldEntity` flags word bit 2: the `SimpleMapRenderer::render` scene-attach
/// gate. Shipping 0 gates the local avatar off.
pub const MINIMAL_AVATAR_WE_FLAGS_ATTACH: u32 = 0x04;

/// `WorldEntity` flags word bit 11 (`0x800`), stored in `Unit+0xA0` by
/// `WorldEntity::readInit` @0x4D3E60. `Avatar::processUpdate` @0x4EDDF0 reads
/// it as the gate for update type [`AVATAR_UPDATE_PLACE_AT_START`]: with the
/// bit set it clears it, full-heals, and places the avatar at the zone's
/// start-room waypoint. A generated destination ships it so the client -- not
/// the server -- computes the arrival coordinate.
pub const AVATAR_WE_FLAGS_WARP_BIT: u32 = 0x800;

/// The scene-attach flags word plus the `0x15` warp gate, for a generated
/// destination whose start tile only the client's maze knows.
pub const MINIMAL_AVATAR_WE_FLAGS_WARP: u32 =
    MINIMAL_AVATAR_WE_FLAGS_ATTACH | AVATAR_WE_FLAGS_WARP_BIT;

/// `Avatar::processUpdate` update type `0x15` (the respawn-to-start-room
/// placement, gated on [`AVATAR_WE_FLAGS_WARP_BIT`]).
pub const AVATAR_UPDATE_PLACE_AT_START: u8 = 0x15;

/// `Hero::processUpdate` update type `0x0F`: `processUpdateAddExperience`
/// (`0x4F7CB0`). See [`build_add_experience_update`] -- the server does not
/// emit it because the client self-awards.
pub const AVATAR_UPDATE_ADD_EXPERIENCE: u8 = 0x0F;

/// The `[u16 avatar][0x15]` body of a `0x03` [`CEM_ENTITY_UPDATE`] record.
///
/// [`CEM_ENTITY_UPDATE`]: crate::cem::CEM_ENTITY_UPDATE
pub fn avatar_place_at_start_update(avatar_eid: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(3);
    out.extend_from_slice(&avatar_eid.to_le_bytes());
    out.push(AVATAR_UPDATE_PLACE_AT_START);
    out
}

/// `avatar/base/Avatar.gc`'s `HitPoints`, shipped as the explicit Unit HP slot.
pub const MINIMAL_AVATAR_HP: u32 = 51200;

/// The avatar's UNSCALED full HP ([`MINIMAL_AVATAR_HP`] / 256).
///
/// [`MINIMAL_AVATAR_HP`] is the x256 WIRE word the create carries. The synch
/// trailers (`hp_wire_u32`, which multiplies by 256) take the UNSCALED value,
/// so passing this yields the same word the create shipped -- passing the wire
/// word instead double-scales it and the client's `EntitySynchInfo::Validate`
/// faults on the mismatch.
pub const MINIMAL_AVATAR_HP_UNSCALED: u32 = MINIMAL_AVATAR_HP / 256;

/// Observed level-1 avatar HP **wire** words, keyed by class base.
///
/// # Why a table and not a formula
///
/// `UnitDesc::getMaxHealth` (the `Tables.MaxHealth` curve, `0x0050FD80`) is
/// what `Unit::computeAttributes` calls through `desc` vtable slot `0xD0`.
/// `HeroDesc` (`0x008700C8`) and `AvatarDesc` (`0x0086E718`) both override that
/// slot with the zero stub `0x0057EE30` (`xor eax,eax; ret 4`), so
/// `Unit::computeAttributes` leaves `Unit+0x14C` (max HP) at **0** for an
/// avatar.
///
/// The visible maximum is then **added** by `Hero::endComputeAttributes`
/// (`0x004F7...`), which is the Hero attribute/modifier pipeline:
/// `HealthPerEnduranceMod` (`HeroDesc+0x188`) x `HealthPerEndurance`
/// (`GlobalKnobs` 25) x the Toughness/endurance attribute, plus
/// `HeroHealthPerLevel` (`GlobalKnobs` 16) x level. That pipeline needs both
/// RPGSettings and the client's live attribute block, and doc 36 plus the
/// reference record that it is not reconstructible from `game.pkg`; the value
/// is therefore an **observed** per-class word, not a game.pkg lookup.
///
/// # Units
///
/// The stored value is the x256 **wire word** -- the number the client holds in
/// `Unit+0x2F0` (`hp_x256`) and that `EntitySynchInfo::Validate` @`0x5DD900`
/// compares against the `(07,35)` synch trailer (`0x5DD9DC`/`0x5DD9E1`). It is
/// therefore already scaled and must never be passed through
/// [`hp_wire_u32`](crate::hp_wire_u32) again. `50539` is the `Local` word in
/// the 2026-10-18 WarlockFemale crash dump; `Unit+0x2F0` is not required to be a
/// multiple of 256 once damage/regen has moved it.
///
/// An unobserved class falls back to the legacy [`MINIMAL_AVATAR_HP`] word
/// (200 displayed, `avatar/base/Avatar.gc`'s `HitPoints`) rather than borrowing
/// another class's number.
pub const AVATAR_LEVEL_ONE_HP_WIRE: &[(&str, u32)] = &[("Warlock", 50539)];

/// The observed level-1 HP wire word for a stored class FQN, or `None` when
/// the class base has no observation.
pub fn avatar_level_one_hp_wire(aclass: &str) -> Option<u32> {
    let base = class_base(aclass)?;
    AVATAR_LEVEL_ONE_HP_WIRE
        .iter()
        .find(|(observed, _)| *observed == base)
        .map(|(_, word)| *word)
}

/// The level-1 HP wire word the server must ship for a stored class FQN,
/// defaulting to [`MINIMAL_AVATAR_HP`] when the base has no observation.
pub fn avatar_hp_wire(aclass: &str) -> u32 {
    avatar_level_one_hp_wire(aclass).unwrap_or(MINIMAL_AVATAR_HP)
}

/// The avatar max-HP wire word gained per level.
///
/// The client's attribute pipeline adds `HeroHealthPerLevel x level`
/// (`GlobalKnobs` 16) in `Hero::endComputeAttributes`; the reference ports that
/// as a flat `+4096` wire (`+16` unscaled HP) per level over the class's
/// observed level-1 word (`drf_pyserver/data/player_state.py`
/// `compute_avatar_max_hp_wire`: "reduces to the live-proven flat baseline
/// 68096 + 4096x(level-1)"). The client refills HP/MP to this max on every
/// level-up (`Hero::onAddExperience` @`0x4F82F0` writes
/// `Unit+0x2f0 = Unit+0x14c << 8`), so the synch trailers must track it or
/// `EntitySynchInfo::Validate` fatals on the first post-level-up action.
pub const HP_WIRE_PER_LEVEL: u32 = 4096;

/// The class's max-HP **wire** word at `level`, extrapolating the observed
/// level-1 word by [`HP_WIRE_PER_LEVEL`]. Level 1 is byte-identical to
/// [`avatar_hp_wire`], so a stored level-1 character is unchanged.
pub fn avatar_hp_wire_at_level(aclass: &str, level: u32) -> u32 {
    let level = level.max(1);
    avatar_hp_wire(aclass).saturating_add(HP_WIRE_PER_LEVEL.saturating_mul(level - 1))
}

/// The `Fighter`/`Ranger`/`Warlock` base of a stored class FQN.
fn class_base(aclass: &str) -> Option<&'static str> {
    let short = aclass.rsplit('.').next().unwrap_or(aclass);
    drf_content::CLASS_BASES
        .iter()
        .copied()
        .find(|base| short.starts_with(base))
}

/// `avatar/base/Avatar.gc`'s `ManaPoints`, shipped as the explicit Unit MP slot.
pub const MINIMAL_AVATAR_MP: u32 = 51200;

/// The `Skills` component classname (`game.pkg` `avatar.base.Skills`).
pub const AVATAR_SKILLS_CLASS: &str = "avatar.base.Skills";
/// The `Manipulators` component classname.
pub const AVATAR_MANIPULATORS_CLASS: &str = "Manipulators";
/// The `Modifiers` component classname.
pub const AVATAR_MODIFIERS_CLASS: &str = "Modifiers";
/// The `UnitContainer` component classname.
pub const AVATAR_UNIT_CONTAINER_CLASS: &str = "UnitContainer";
/// The `UnitBehavior` component classname (`game.pkg` `avatar.base.UnitBehavior`).
pub const AVATAR_UNIT_BEHAVIOR_CLASS: &str = "avatar.base.UnitBehavior";

/// The neutral `UnitBehavior::readInit` (0x51FE10) body: 4 B `Behavior` +
/// 10 B `UnitMover` (flags `0x40`) + 3 B own state.
pub const UNIT_BEHAVIOR_NEUTRAL_PAYLOAD: [u8; 17] = [
    0x00, 0x00, 0x00, 0x01, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01,
    0x00,
];

/// The payload offset `UnitBehavior::readInit` XORs into control-grant bit 0.
pub const UNIT_BEHAVIOR_FOLLOW_GRANT_PAYLOAD_OFF: usize = 0x0F;

/// The u32 `UnitContainer` carries between its post byte and its gold word.
pub const AVATAR_CONTAINER_PREFIX_U32: u32 = 0x31;
/// The `UnitContainer` trailer byte.
pub const AVATAR_CONTAINER_TRAILER: u8 = 0x00;
/// The per-inventory gate byte an empty inventory record carries.
pub const AVATAR_INVENTORY_COUNT_BYTE: u8 = 0x01;

/// The post byte a component-create record carries before its class payload.
const AVATAR_COMPONENT_POST_BYTE: u8 = 0x01;

/// An avatar record shape violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarError {
    /// Entity id zero is the unassigned sentinel.
    ZeroEntityId,
    /// The avatar and its owner player must be distinct entities.
    SameEntityId,
    /// The avatar classname is empty or longer than 128 bytes.
    BadClassname,
    /// The level byte would be zero (`1..=255` is the domain).
    BadLevel,
    /// A component id is zero or equals the owner.
    BadComponentId,
    /// The inventory roster does not fit the one-byte child count.
    RosterTooLarge(usize),
    /// The class kit does not fit the one-byte manipulator count.
    TooManyItems(usize),
}

impl fmt::Display for AvatarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AvatarError::ZeroEntityId => write!(f, "entity ID 0 is the unassigned sentinel"),
            AvatarError::SameEntityId => {
                write!(f, "avatar and owner player entity IDs must differ")
            }
            AvatarError::BadClassname => {
                write!(f, "avatar classname must be 1..=128 bytes")
            }
            AvatarError::BadLevel => write!(f, "avatar level must be 1..=255"),
            AvatarError::BadComponentId => {
                write!(
                    f,
                    "component id must be non-zero and distinct from the owner"
                )
            }
            AvatarError::RosterTooLarge(count) => {
                write!(f, "inventory roster of {count} does not fit the u8 count")
            }
            AvatarError::TooManyItems(count) => {
                write!(
                    f,
                    "a starting kit of {count} items does not fit the u8 count"
                )
            }
        }
    }
}

impl std::error::Error for AvatarError {}

fn validate_ids(owner: u16, component_id: u16) -> Result<(), AvatarError> {
    if owner == 0 {
        return Err(AvatarError::ZeroEntityId);
    }
    if component_id == 0 || component_id == owner {
        return Err(AvatarError::BadComponentId);
    }
    Ok(())
}

/// The class payload a wire-`0x02` avatar createinit carries: the 56-byte
/// class-state head plus any trailing `components` verbatim.
///
/// This is [`build_minimal_avatar_init_payload_with_progress`] with a fresh
/// character's zero XP and zero bonus attribute points. `pose` is
/// `[x, y, z, heading]` in x256 (zeroed when `None`); `level` is the store
/// character's, defaulting to the schema's fresh-character 1. `hp`/`mp` append
/// their explicit Unit u32 slots (and set the flag bits); with them absent the
/// engine derives both from the archetype.
#[allow(clippy::too_many_arguments)]
pub fn build_minimal_avatar_init_payload(
    avatar_eid: u16,
    player_eid: u16,
    avatar_class: &[u8],
    pose: Option<[u32; 4]>,
    level: Option<u8>,
    components: &[u8],
    we_flags: u32,
    hp: Option<u32>,
    mp: Option<u32>,
) -> Result<Vec<u8>, AvatarError> {
    build_minimal_avatar_init_payload_with_progress(
        avatar_eid,
        player_eid,
        avatar_class,
        pose,
        level,
        0,
        0,
        components,
        we_flags,
        hp,
        mp,
    )
}

/// The avatar `0x02` class-state head carrying the character's persisted
/// `Hero` progress.
///
/// `Hero::readInit` (`0x4F8A20`) reads, right after the explicit HP/MP words,
/// a u32 XP (`Hero+0x320`), then six u16s (`+0x334`..`+0x33e`; the sixth,
/// `+0x33c`, is the bonus **attribute points**), then two u32s. `experience`
/// is the progress-into-current-level the client holds in `+0x320`; shipping it
/// (and the granted `attribute_points`) is what stops a zone-transfer avatar
/// re-send from clobbering the client's locally-earned progress with zeros
/// (`drf_pyserver/client_hook/hook.h`: "the zone-transfer Avatar re-send never
/// clobbers locally-earned experience").
#[allow(clippy::too_many_arguments)]
pub fn build_minimal_avatar_init_payload_with_progress(
    avatar_eid: u16,
    player_eid: u16,
    avatar_class: &[u8],
    pose: Option<[u32; 4]>,
    level: Option<u8>,
    experience: u32,
    attribute_points: u16,
    components: &[u8],
    we_flags: u32,
    hp: Option<u32>,
    mp: Option<u32>,
) -> Result<Vec<u8>, AvatarError> {
    build_minimal_avatar_init_payload_with_stats(
        avatar_eid,
        player_eid,
        avatar_class,
        pose,
        level,
        experience,
        attribute_points,
        [0; 4],
        components,
        we_flags,
        hp,
        mp,
    )
}

/// [`build_minimal_avatar_init_payload_with_progress`] plus the four allocated
/// primary stats.
///
/// `attributes` is `[Strength, Agility, Toughness, Power]`, written to the
/// `Hero::readInit` u16 slots `+0x334`, `+0x336`, `+0x338`, `+0x33A`. They must
/// satisfy the client's invariant
/// `STR + AGI + TOUGH + POW + AttributePoints == 5 * (level - 1)`; the store
/// and the spend handler keep it true, and a re-join ships both halves so the
/// Attributes panel survives a zone transfer or re-login.
#[allow(clippy::too_many_arguments)]
pub fn build_minimal_avatar_init_payload_with_stats(
    avatar_eid: u16,
    player_eid: u16,
    avatar_class: &[u8],
    pose: Option<[u32; 4]>,
    level: Option<u8>,
    experience: u32,
    attribute_points: u16,
    attributes: [u16; 4],
    components: &[u8],
    we_flags: u32,
    hp: Option<u32>,
    mp: Option<u32>,
) -> Result<Vec<u8>, AvatarError> {
    if avatar_eid == 0 || player_eid == 0 {
        return Err(AvatarError::ZeroEntityId);
    }
    if avatar_eid == player_eid {
        return Err(AvatarError::SameEntityId);
    }
    let class = avatar_class.split(|&byte| byte == 0).next().unwrap_or(&[]);
    if class.is_empty() || class.len() > 128 {
        return Err(AvatarError::BadClassname);
    }
    let lvl = level.unwrap_or(1);
    if lvl == 0 {
        return Err(AvatarError::BadLevel);
    }
    let [x, y, z, aux] = pose.unwrap_or([0, 0, 0, 0]);
    let mut unit_flags = 0x01u8;
    if hp.is_some() {
        unit_flags |= 0x02;
    }
    if mp.is_some() {
        unit_flags |= 0x04;
    }
    let mut head = Vec::with_capacity(MINIMAL_LINKED_AVATAR_HEAD_LEN + 8);
    head.extend_from_slice(&we_flags.to_le_bytes());
    head.extend_from_slice(&x.to_le_bytes());
    head.extend_from_slice(&y.to_le_bytes());
    head.extend_from_slice(&z.to_le_bytes());
    head.extend_from_slice(&aux.to_le_bytes());
    head.push(0); // WorldEntity flag byte: no gated tails
    head.push(unit_flags);
    head.push(lvl);
    head.extend_from_slice(&0u16.to_le_bytes());
    head.extend_from_slice(&0u16.to_le_bytes());
    head.extend_from_slice(&player_eid.to_le_bytes()); // Unit owner link
    if let Some(value) = hp {
        head.extend_from_slice(&value.to_le_bytes());
    }
    if let Some(value) = mp {
        head.extend_from_slice(&value.to_le_bytes());
    }
    // Hero::readInit: u32 XP then six u16s; the sixth is the bonus attribute
    // points (`+0x33c`).
    head.extend_from_slice(&experience.to_le_bytes()); // Hero xp (+0x320)
    head.extend_from_slice(&attributes[0].to_le_bytes()); // +0x334 Strength
    head.extend_from_slice(&attributes[1].to_le_bytes()); // +0x336 Agility
    head.extend_from_slice(&attributes[2].to_le_bytes()); // +0x338 Toughness
    head.extend_from_slice(&attributes[3].to_le_bytes()); // +0x33a Power
    head.extend_from_slice(&attribute_points.to_le_bytes()); // +0x33c bonus points
    head.extend_from_slice(&0u16.to_le_bytes()); // +0x33e
    head.extend_from_slice(&0u32.to_le_bytes()); // Hero pool (+0x324)
    head.extend_from_slice(&0u32.to_le_bytes()); // Hero pool (+0x330)
    head.extend_from_slice(&[0u8; 3]); // Avatar trailing bytes
    let expect = MINIMAL_LINKED_AVATAR_HEAD_LEN
        + if hp.is_some() { 4 } else { 0 }
        + if mp.is_some() { 4 } else { 0 };
    debug_assert_eq!(head.len(), expect, "avatar head length drift");
    head.extend_from_slice(components);
    Ok(head)
}

/// The `processUpdateAddExperience` (`0x0F`) record body a server *would* ship
/// as a `(07,03)` [`CEM_ENTITY_UPDATE`](crate::cem::CEM_ENTITY_UPDATE).
///
/// **This server emits none.** The client self-awards XP on a kill, so a
/// server-sent `0x0F` would double-award. The grammar below is transcribed
/// from the client binary and lives here (unit-tested) as the verified shape:
///
/// * `ClientEntityManager::processEntityUpdate` (`0x5DAE30`) reads a u16
///   entity id and a u8 update type, then calls the entity's `processUpdate`
///   and finally reads the `EntitySynchInfo` trailer.
/// * `Hero::processUpdate` (`0x4F7C40`) case `0x0F` calls
///   `processUpdateAddExperience` (`0x4F7CB0`), which reads a u32 XP **base**
///   and a u8 **source (mob) level** and raises the internal `0x14` event ->
///   `Hero::onAddExperience` (`0x4F82F0`).
/// * The mandatory trailer is `[0x02][u32 hp_wire]` (the avatar's own word);
///   the `0x06` ends the CEM record stream.
///
/// So the full inner is
/// `07 03 <eid:u16> 0F <xp:u32> <sourceLevel:u8> 02 <hp:u32> 06`.
/// This function returns the `send(0x07, 0x03, ..)` payload (everything after
/// the `07 03` header), matching the `0x06`-terminated builder convention.
pub fn build_add_experience_update(
    avatar_eid: u16,
    xp: u32,
    source_level: u8,
    hp_wire: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + 1 + 4 + 1 + 5 + 1);
    out.extend_from_slice(&avatar_eid.to_le_bytes());
    out.push(AVATAR_UPDATE_ADD_EXPERIENCE);
    out.extend_from_slice(&xp.to_le_bytes());
    out.push(source_level);
    out.push(0x02);
    out.extend_from_slice(&hp_wire.to_le_bytes());
    out.push(crate::cem::END_MARKER);
    out
}

/// The `avatar.base.Skills` create payload body.
///
/// `[post 0x01][u32 points][u8 skill count][(cstr fqn)(u32 id)(u8 level)]…`
/// `[u8 profession count][(cstr fqn)]…`, the shape `Skills::readInit`
/// @0x541830 consumes (the post byte is the one `component_create_inner`
/// re-emits, so this is the whole payload a create carries).
pub fn build_avatar_skills_payload(
    points: u32,
    skills: &[(String, u32, u8)],
    professions: &[String],
) -> Vec<u8> {
    assert!(
        skills.len() <= 0xFE,
        "authored Skills list longer than the u8 count"
    );
    assert!(
        professions.len() <= 0xFE,
        "authored Skills profession list longer than the u8 count"
    );
    let mut out = Vec::new();
    out.push(AVATAR_COMPONENT_POST_BYTE);
    out.extend_from_slice(&points.to_le_bytes());
    out.push(skills.len() as u8);
    for (fqn, id, level) in skills {
        out.extend_from_slice(&encode_list_cstr(fqn));
        out.extend_from_slice(&id.to_le_bytes());
        out.push(*level);
    }
    out.push(professions.len() as u8);
    for fqn in professions {
        out.extend_from_slice(&encode_list_cstr(fqn));
    }
    out
}

/// The avatar's `avatar.base.Skills` record carrying a class's starting skills.
///
/// `component_create_inner` re-emits the post byte, so only the payload's
/// `[u32 points][count][entries][professions]` tail is passed to it. An empty
/// `skills`/`professions` pair is byte-identical to
/// [`minimal_avatar_skills_component`]'s empty body.
pub fn minimal_avatar_skills_component_with_skills(
    owner: u16,
    component_id: u16,
    points: u32,
    skills: &[(String, u32, u8)],
    professions: &[String],
) -> Result<Vec<u8>, AvatarError> {
    validate_ids(owner, component_id)?;
    let full = build_avatar_skills_payload(points, skills, professions);
    Ok(component_create_inner(
        owner,
        component_id,
        AVATAR_SKILLS_CLASS.as_bytes(),
        AVATAR_COMPONENT_POST_BYTE,
        &full[1..],
    ))
}

/// The avatar's `avatar.base.Skills` record: `[quota u32][empty][empty]`.
///
/// The quota (the `Skills+0x70` points word) has no admissible source and rides
/// the caller's value, conventionally 0.
pub fn minimal_avatar_skills_component(
    owner: u16,
    component_id: u16,
    quota: u32,
) -> Result<Vec<u8>, AvatarError> {
    minimal_avatar_skills_component_with_skills(owner, component_id, quota, &[], &[])
}

/// The avatar's `avatar.base.Equipment` record.
///
/// `items` is the stored class's starting kit; `None` is the naked character
/// and ships the empty list.
pub fn minimal_avatar_equipment_component(
    owner: u16,
    component_id: u16,
    items: Option<&[ClassItem]>,
) -> Result<Vec<u8>, AvatarError> {
    validate_ids(owner, component_id)?;
    let payload = match items {
        None => vec![0u8],
        Some(items) => {
            // `class_starting_equipment_payload` returns `[0x01 post][count]`
            // and `component_create_inner` re-emits the post byte, so only the
            // `[count][entries]` body is passed here.
            let full = class_starting_equipment_payload(items, None, None);
            full[1..].to_vec()
        }
    };
    Ok(component_create_inner(
        owner,
        component_id,
        AVATAR_EQUIPMENT_CLASS.as_bytes(),
        AVATAR_COMPONENT_POST_BYTE,
        &payload,
    ))
}

/// The avatar's `avatar.base.Equipment` record from already-authored entries.
///
/// Each entry is the full `Item::readData` record (type tag + classname +
/// fields) the join is shipping, with its u32 id already re-addressed to the
/// equipment slot (`0xFF 0xFF` cell). Used when the character's worn state is
/// persisted; `None` is the class-kit path in
/// [`minimal_avatar_equipment_component`].
pub fn minimal_avatar_equipment_component_with_entries(
    owner: u16,
    component_id: u16,
    entries: &[Vec<u8>],
) -> Result<Vec<u8>, AvatarError> {
    validate_ids(owner, component_id)?;
    let mut payload = Vec::new();
    push_list_count(&mut payload, entries.len());
    for entry in entries {
        payload.extend_from_slice(entry);
    }
    Ok(component_create_inner(
        owner,
        component_id,
        AVATAR_EQUIPMENT_CLASS.as_bytes(),
        AVATAR_COMPONENT_POST_BYTE,
        &payload,
    ))
}

/// The avatar's `Manipulators` record from already-authored item entries.
///
/// `item_entries` are the same records as the Equipment list plus each class's
/// `vtable+0x100` readState body; `skills` are the ActiveSkill tail entries.
pub fn minimal_avatar_manipulators_component_with_entries(
    owner: u16,
    component_id: u16,
    item_entries: &[Vec<u8>],
    skills: &[(String, u32, u8)],
) -> Result<Vec<u8>, AvatarError> {
    validate_ids(owner, component_id)?;
    let total = item_entries.len() + skills.len();
    if total > 0xFF {
        return Err(AvatarError::TooManyItems(total));
    }
    let tail = manipulator_skill_tail_bytes(skills);
    let mut payload =
        Vec::with_capacity(1 + item_entries.iter().map(Vec::len).sum::<usize>() + tail.len());
    payload.push(total as u8);
    for entry in item_entries {
        payload.extend_from_slice(entry);
    }
    payload.extend_from_slice(&tail);
    Ok(component_create_inner(
        owner,
        component_id,
        AVATAR_MANIPULATORS_CLASS.as_bytes(),
        AVATAR_COMPONENT_POST_BYTE,
        &payload,
    ))
}

/// The avatar's `UnitContainer` record, with the game.pkg inventory roster.
///
/// Each `roster` entry names its own inventory class so the client's desc
/// lookup (`getNextInventoryByType`) resolves; `id` is the child's u8 id.
pub fn minimal_avatar_unit_container_component(
    owner: u16,
    component_id: u16,
    roster: &[(String, u8)],
) -> Result<Vec<u8>, AvatarError> {
    minimal_avatar_unit_container_component_with_gold(owner, component_id, roster, 0)
}

/// The avatar's `UnitContainer` record with an explicit `Container+0x74` gold
/// word, matching the purse the session serves.
pub fn minimal_avatar_unit_container_component_with_gold(
    owner: u16,
    component_id: u16,
    roster: &[(String, u8)],
    gold: u32,
) -> Result<Vec<u8>, AvatarError> {
    validate_ids(owner, component_id)?;
    if roster.len() > 0xFF {
        return Err(AvatarError::RosterTooLarge(roster.len()));
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(&AVATAR_CONTAINER_PREFIX_U32.to_le_bytes());
    payload.extend_from_slice(&gold.to_le_bytes());
    payload.push(roster.len() as u8);
    for (fqn, id) in roster {
        payload.extend_from_slice(&encode_list_cstr(fqn));
        payload.push(*id);
        payload.push(AVATAR_INVENTORY_COUNT_BYTE);
        payload.push(0); // empty item count
    }
    payload.push(AVATAR_CONTAINER_TRAILER);
    Ok(component_create_inner(
        owner,
        component_id,
        AVATAR_UNIT_CONTAINER_CLASS.as_bytes(),
        AVATAR_COMPONENT_POST_BYTE,
        &payload,
    ))
}

/// The ActiveSkill `Manipulators` tail entries: one
/// `[0xFF][ASCIIZ][u32 id][u8 level][u8 0x00]` per skill.
fn manipulator_skill_tail_bytes(skills: &[(String, u32, u8)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (fqn, id, level) in skills {
        out.extend_from_slice(&skill_manipulator_entry(fqn, *id, *level));
    }
    out
}

/// One `Manipulators` ActiveSkill **readInit** entry:
/// `[0xFF][fqn NUL][u32 slot][u8 level][u8 0]`.
pub fn skill_manipulator_entry(fqn: &str, slot: u32, level: u8) -> Vec<u8> {
    let mut out = skill_manipulator_update_entry(fqn, slot, level);
    out.push(0);
    out
}

/// One `Manipulators` ActiveSkill **update** (`0x00` Add) entry:
/// `[0xFF][fqn NUL][u32 slot][u8 level]`.
///
/// The update form has no trailing zero byte before the synch trailer. Adding
/// one makes the client read the trailer's HP word as a new entity id
/// (`Invalid EntityID(63488)`).
pub fn skill_manipulator_update_entry(fqn: &str, slot: u32, level: u8) -> Vec<u8> {
    let mut out = encode_list_cstr(fqn);
    out.extend_from_slice(&slot.to_le_bytes());
    out.push(level.max(1));
    out
}

/// The avatar's `Manipulators` record: the class's worn-item entries, then the
/// ActiveSkill tail.
///
/// The `Equipment` record drives the inventory/icon list, but the avatar's
/// mounted armour visuals come from these manipulator entries; `None` ships no
/// item entries. `skills` are the class's ActiveSkill manipulators and are
/// appended after the items, exactly as the client's `Manipulators::readInit`
/// @0x4FD050 reads them.
pub fn minimal_avatar_manipulators_component(
    owner: u16,
    component_id: u16,
    items: Option<&[ClassItem]>,
    skills: &[(String, u32, u8)],
) -> Result<Vec<u8>, AvatarError> {
    validate_ids(owner, component_id)?;
    let (count, entries) = match items {
        None => (0usize, Vec::new()),
        Some(items) => {
            if items.len() > 0xFF {
                return Err(AvatarError::TooManyItems(items.len()));
            }
            (
                items.len(),
                class_starting_item_manipulators_bytes(items, None, None),
            )
        }
    };
    let total = count + skills.len();
    if total > 0xFF {
        return Err(AvatarError::TooManyItems(total));
    }
    let tail = manipulator_skill_tail_bytes(skills);
    let mut payload = Vec::with_capacity(1 + entries.len() + tail.len());
    payload.push(total as u8);
    payload.extend_from_slice(&entries);
    payload.extend_from_slice(&tail);
    Ok(component_create_inner(
        owner,
        component_id,
        AVATAR_MANIPULATORS_CLASS.as_bytes(),
        AVATAR_COMPONENT_POST_BYTE,
        &payload,
    ))
}

/// The avatar's empty `Modifiers` record: `[u32 0][u32 0][0 children]`.
pub fn minimal_avatar_modifiers_component(
    owner: u16,
    component_id: u16,
) -> Result<Vec<u8>, AvatarError> {
    validate_ids(owner, component_id)?;
    Ok(component_create_inner(
        owner,
        component_id,
        AVATAR_MODIFIERS_CLASS.as_bytes(),
        AVATAR_COMPONENT_POST_BYTE,
        &[0u8; 9],
    ))
}

/// The avatar's `avatar.base.UnitBehavior` record (post byte 0, 17-byte body).
///
/// `follow_grant` MUST start clear for the local player: `getClientBehavior`
/// only calls `RequestClientControl` when bit 0 is clear, so a pre-set bit
/// suppresses the client's control request.
pub fn minimal_avatar_unit_behavior_component(
    owner: u16,
    component_id: u16,
    follow_grant: bool,
) -> Result<Vec<u8>, AvatarError> {
    validate_ids(owner, component_id)?;
    let mut payload = UNIT_BEHAVIOR_NEUTRAL_PAYLOAD;
    payload[UNIT_BEHAVIOR_FOLLOW_GRANT_PAYLOAD_OFF] = u8::from(follow_grant);
    Ok(component_create_inner(
        owner,
        component_id,
        AVATAR_UNIT_BEHAVIOR_CLASS.as_bytes(),
        0,
        &payload,
    ))
}

/// `s2c [07 35][Skills cid][0x32][0x04][u32 skill hash][level]` -- one skill
/// level update (`Skills::processUpdateSkill`).
pub fn build_skills_skill_update(component_id: u16, skill_hash: u32, level: u8) -> Vec<u8> {
    let mut out = vec![0x07, 0x35];
    out.extend_from_slice(&component_id.to_le_bytes());
    out.push(0x32);
    out.push(0x04);
    out.extend_from_slice(&skill_hash.to_le_bytes());
    out.push(level);
    out
}

/// `s2c [07 35][Skills cid][0x32][0xFF][cstr skill][level]` -- one named
/// skill level update (`Skills::processUpdateSkill`'s class-id form). The
/// trainer path uses this shape; hotbar/UI updates use the u32-hash form.
pub fn build_skills_skill_name_update(component_id: u16, skill: &str, level: u8) -> Vec<u8> {
    let mut out = vec![0x07, 0x35];
    out.extend_from_slice(&component_id.to_le_bytes());
    out.push(0x32);
    out.push(0xFF);
    out.extend_from_slice(skill.as_bytes());
    out.push(0);
    out.push(level);
    out
}

/// `s2c [07 35][Skills cid][0x33][u32 points]` -- the unspent skill-point
/// quota (`Skills::processUpdateSkillPoints`).
pub fn build_skills_points_update(component_id: u16, points: u32) -> Vec<u8> {
    let mut out = vec![0x07, 0x35];
    out.extend_from_slice(&component_id.to_le_bytes());
    out.push(0x33);
    out.extend_from_slice(&points.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_is_56_bytes_without_hpmp_and_64_with() {
        let bare = build_minimal_avatar_init_payload(
            0x1001,
            0x1000,
            b"avatar.classes.WarlockFemale",
            None,
            Some(1),
            &[],
            0,
            None,
            None,
        )
        .unwrap();
        assert_eq!(bare.len(), MINIMAL_LINKED_AVATAR_HEAD_LEN);
        let with_hpmp = build_minimal_avatar_init_payload(
            0x1001,
            0x1000,
            b"avatar.classes.WarlockFemale",
            None,
            Some(1),
            &[],
            MINIMAL_AVATAR_WE_FLAGS_ATTACH,
            Some(MINIMAL_AVATAR_HP),
            Some(MINIMAL_AVATAR_MP),
        )
        .unwrap();
        assert_eq!(with_hpmp.len(), 64);
        assert_eq!(
            &with_hpmp[0..4],
            &MINIMAL_AVATAR_WE_FLAGS_ATTACH.to_le_bytes()
        );
        assert_eq!(with_hpmp[21], 0x07, "owner + HP + MP unit flags");
    }

    #[test]
    fn warp_flags_word_carries_the_attach_and_gate_bits() {
        assert_eq!(MINIMAL_AVATAR_WE_FLAGS_WARP, 0x804);
        assert_eq!(
            MINIMAL_AVATAR_WE_FLAGS_WARP & MINIMAL_AVATAR_WE_FLAGS_ATTACH,
            MINIMAL_AVATAR_WE_FLAGS_ATTACH
        );
        assert_eq!(
            MINIMAL_AVATAR_WE_FLAGS_WARP & AVATAR_WE_FLAGS_WARP_BIT,
            AVATAR_WE_FLAGS_WARP_BIT
        );
    }

    #[test]
    fn hp_wire_climbs_by_4096_per_level_over_the_observed_base() {
        // Level 1 is byte-identical to the observed word (no regression).
        assert_eq!(
            avatar_hp_wire_at_level("avatar.classes.WarlockFemale", 1),
            avatar_hp_wire("avatar.classes.WarlockFemale")
        );
        assert_eq!(
            avatar_hp_wire_at_level("avatar.classes.WarlockFemale", 2),
            50_539 + HP_WIRE_PER_LEVEL
        );
        // Level 0 clamps to level 1 rather than underflowing.
        assert_eq!(
            avatar_hp_wire_at_level("avatar.classes.WarlockFemale", 0),
            50_539
        );
    }

    #[test]
    fn progress_head_carries_xp_and_attribute_points() {
        let head = build_minimal_avatar_init_payload_with_progress(
            0x1001,
            0x1000,
            b"avatar.classes.WarlockFemale",
            None,
            Some(3),
            1234,
            15,
            &[],
            MINIMAL_AVATAR_WE_FLAGS_ATTACH,
            Some(MINIMAL_AVATAR_HP),
            Some(MINIMAL_AVATAR_MP),
        )
        .unwrap();
        assert_eq!(head.len(), 64);
        // Hero xp is the u32 after the HP (+29) and MP (+33) words.
        assert_eq!(&head[37..41], &1234u32.to_le_bytes());
        // Bonus attribute points are `+0x33c`, the fifth u16 (bytes 49..51).
        assert_eq!(&head[49..51], &15u16.to_le_bytes());
        // The legacy builder ships the same head with zero progress.
        let bare = build_minimal_avatar_init_payload(
            0x1001,
            0x1000,
            b"avatar.classes.WarlockFemale",
            None,
            Some(3),
            &[],
            MINIMAL_AVATAR_WE_FLAGS_ATTACH,
            Some(MINIMAL_AVATAR_HP),
            Some(MINIMAL_AVATAR_MP),
        )
        .unwrap();
        assert_eq!(&bare[37..41], &0u32.to_le_bytes());
        assert_eq!(&bare[49..51], &0u16.to_le_bytes());
    }

    #[test]
    fn stats_head_carries_the_allocated_attributes() {
        let head = build_minimal_avatar_init_payload_with_stats(
            0x1001,
            0x1000,
            b"avatar.classes.WarlockFemale",
            None,
            Some(3),
            1234,
            0,
            [1, 2, 3, 4],
            &[],
            MINIMAL_AVATAR_WE_FLAGS_ATTACH,
            Some(MINIMAL_AVATAR_HP),
            Some(MINIMAL_AVATAR_MP),
        )
        .unwrap();
        // `+0x334`/`+0x336`/`+0x338`/`+0x33a` are the four u16s before the
        // `+0x33c` bonus word.
        assert_eq!(&head[41..43], &1u16.to_le_bytes());
        assert_eq!(&head[43..45], &2u16.to_le_bytes());
        assert_eq!(&head[45..47], &3u16.to_le_bytes());
        assert_eq!(&head[47..49], &4u16.to_le_bytes());
        assert_eq!(&head[49..51], &0u16.to_le_bytes());
        // The invariant the client maintains across level-ups.
        let allocated: u16 = [1u16, 2, 3, 4].iter().sum();
        assert_eq!(allocated, 5 * (3 - 1));
    }

    #[test]
    fn add_experience_record_is_the_verified_grammar() {
        // `07 03 <eid:u16> 0F <xp:u32> <sourceLevel:u8> 02 <hp:u32> 06`.
        let body = build_add_experience_update(0x1014, 500, 2, 50_539);
        let mut full = vec![0x07, 0x03];
        full.extend_from_slice(&body);
        assert_eq!(
            full,
            vec![
                0x07, 0x03, // channel 07, CEM_ENTITY_UPDATE
                0x14, 0x10, // eid 0x1014 LE
                0x0F, // processUpdateAddExperience
                0xF4, 0x01, 0x00, 0x00, // xp 500 LE
                0x02, // source (mob) level
                0x02, // EntitySynchInfo trailer
                0x6B, 0xC5, 0x00, 0x00, // hp wire 50539 LE
                0x06, // CEM stream end
            ]
        );
    }

    #[test]
    fn place_at_start_update_is_the_eid_plus_0x15() {
        assert_eq!(
            avatar_place_at_start_update(0x1014),
            vec![0x14, 0x10, AVATAR_UPDATE_PLACE_AT_START]
        );
        assert_eq!(AVATAR_UPDATE_PLACE_AT_START, 0x15);
    }

    #[test]
    fn unit_owner_word_is_the_player_eid() {
        let head = build_minimal_avatar_init_payload(
            0x1001,
            0x1000,
            b"c",
            Some([1, 2, 3, 4]),
            Some(1),
            &[],
            0,
            None,
            None,
        )
        .unwrap();
        assert_eq!(head[21], 0x01, "owner-link unit flag");
        assert_eq!(&head[27..29], &0x1000u16.to_le_bytes());
    }

    #[test]
    fn level_one_hp_is_per_class_and_defaults_to_the_legacy_word() {
        // The Warlock's observed crash word is 50539 = 0xC56B, the x256 value
        // the client holds in Unit+0x2F0. Male and Female share the base.
        assert_eq!(
            avatar_level_one_hp_wire("avatar.classes.WarlockFemale"),
            Some(50_539)
        );
        assert_eq!(
            avatar_level_one_hp_wire("avatar.classes.WarlockMale"),
            Some(50_539)
        );
        assert_eq!(avatar_hp_wire("avatar.classes.WarlockFemale"), 50_539);
        // An unobserved base falls back to the legacy archetype word, never
        // another class's number.
        assert_eq!(avatar_level_one_hp_wire("avatar.classes.FighterMale"), None);
        assert_eq!(
            avatar_hp_wire("avatar.classes.FighterMale"),
            MINIMAL_AVATAR_HP
        );
        assert_eq!(avatar_level_one_hp_wire("not.a.class"), None);
        assert_eq!(avatar_hp_wire("not.a.class"), MINIMAL_AVATAR_HP);
    }

    #[test]
    fn ids_and_bounds_are_refused() {
        assert_eq!(
            build_minimal_avatar_init_payload(0, 1, b"c", None, None, &[], 0, None, None),
            Err(AvatarError::ZeroEntityId)
        );
        assert_eq!(
            build_minimal_avatar_init_payload(1, 1, b"c", None, None, &[], 0, None, None),
            Err(AvatarError::SameEntityId)
        );
        assert_eq!(
            build_minimal_avatar_init_payload(1, 2, b"", None, None, &[], 0, None, None),
            Err(AvatarError::BadClassname)
        );
        assert_eq!(
            build_minimal_avatar_init_payload(1, 2, b"c", None, Some(0), &[], 0, None, None),
            Err(AvatarError::BadLevel)
        );
        assert_eq!(
            minimal_avatar_skills_component(0, 1, 0),
            Err(AvatarError::ZeroEntityId)
        );
        assert_eq!(
            minimal_avatar_skills_component(1, 1, 0),
            Err(AvatarError::BadComponentId)
        );
    }
}
