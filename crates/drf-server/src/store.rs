// SPDX-License-Identifier: GPL-3.0-or-later

//! Account and character persistence (`s1_store.json`).
//!
//! The store owns the authoritative roster: which characters an account has,
//! and for each one its `sqlid`, name, avatar-class FQN, appearance bytes,
//! level, and current zone. It persists only the records the server needs,
//! written as JSON.
//!
//! The reader/writer is hand-written and std-only: the schema is small and
//! fixed, so a focused parser/serializer beats pulling in a JSON dependency.
//! Unlike a fresh-start loader, a file
//! that is present but malformed is rejected here with a typed error, because
//! silently discarding a damaged roster is a data-loss decision the server
//! must not make on its own.
//!
//! The writer emits a clean schema:
//!
//! ```json
//! {
//!   "next_sqlid": 2,
//!   "accounts": {
//!     "a": {
//!       "characters": {
//!         "1": {
//!           "name": "Hero",
//!           "class": "avatar.classes.WarlockFemale",
//!           "appearance": { "skin": 0, "face": 0, "face_feature": 0,
//!                           "hair": 0, "hair_color": 2 },
//!           "level": 1,
//!           "experience": 0,
//!           "attribute_points": 0,
//!           "strength": 0,
//!           "agility": 0,
//!           "toughness": 0,
//!           "power": 0,
//!           "zone": "town"
//!         }
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! The reader also accepts the reference original `s1_store.json` shape, where the
//! characters live in a top-level `characters` map keyed by sqlid, each record
//! carries its own `account` field, and the appearance bytes appear either as
//! a `creation.appearance` object or as an `opts_hex` string.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Maximum characters one account may own.
///
/// The client's `+0x9c < 3` gate is not a character cap: the live client
/// leaves Create enabled past three, so that gate is party/unknown. This cap
/// is ours, matching the reference raised value.
pub const MAX_CHARS_PER_ACCOUNT: usize = 8;

/// Level a freshly created character starts at.
pub const DEFAULT_LEVEL: u32 = 1;

/// Gold a character starts with, and the value a store record written before
/// gold was persisted falls back to. Matches the reference `World.AVATAR_GOLD_START`.
pub const DEFAULT_GOLD: u32 = 5000;

/// Progress-into-level a fresh character starts with (`Hero +0x320`).
pub const DEFAULT_EXPERIENCE: u32 = 0;

/// Bonus attribute points a fresh character starts with (`Hero +0x33c`).
pub const DEFAULT_ATTRIBUTE_POINTS: u32 = 0;

/// Zone a freshly created character is placed in.
///
/// The starter zone is the tutorial (`Tutorial.zone`, labelled "Dew Valley"):
/// a new character begins there, so this is the persisted zone a character
/// carries until an entity-driven transition moves it. The zone is read back
/// from the store on every join; nothing re-derives it from a zone list.
pub const DEFAULT_ZONE: &str = "tutorial";

/// The five optional appearance bytes carried by every character.
///
/// A `None` field means "not chosen"; a chosen field is an unmapped byte as
/// the client sent it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Appearance {
    pub skin: Option<u8>,
    pub face: Option<u8>,
    pub face_feature: Option<u8>,
    pub hair: Option<u8>,
    pub hair_color: Option<u8>,
}

/// The state a persisted quest row is in. The offer directory's completed
/// gate reads [`QuestState::TurnedIn`]; the join replays `Accepted`/`Complete`
/// rows and never `TurnedIn` (`sessions/quest.py:quest_journal_entries`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestState {
    Offered,
    Accepted,
    Complete,
    TurnedIn,
}

impl QuestState {
    /// The store's on-disk spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            QuestState::Offered => "offered",
            QuestState::Accepted => "accepted",
            QuestState::Complete => "complete",
            QuestState::TurnedIn => "turned-in",
        }
    }

    fn parse(text: &str) -> Option<QuestState> {
        match text {
            "offered" => Some(QuestState::Offered),
            "accepted" => Some(QuestState::Accepted),
            "complete" => Some(QuestState::Complete),
            "turned-in" => Some(QuestState::TurnedIn),
            _ => None,
        }
    }
}

/// One persisted objective of a held quest: the AddQuest body's `(flags, label)`
/// pair, with the store's progress already baked into the label text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestObjective {
    pub flags: u8,
    pub label: String,
}

/// One persisted quest row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestRecord {
    pub state: QuestState,
    pub instance_ref: u32,
    pub objectives: Vec<QuestObjective>,
}

/// One persisted bag row: the item's class, its grid cell and its stack count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BagItem {
    pub class: String,
    pub cell_x: u8,
    pub cell_y: u8,
    pub count: u32,
    /// The item's authored level byte, when the row carries one.
    pub level: Option<u8>,
    /// The item's per-instance flags byte, when the row carries one.
    pub flags: Option<u8>,
    /// Server-assigned `ItemModifier` children (class, level).
    pub modifiers: Vec<(String, u8)>,
}

/// One persisted worn item: its Equipment slot and the authored fields a
/// rejoin needs to rebuild the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquipmentItem {
    pub slot: u32,
    pub class: String,
    pub level: u8,
    pub flags: u8,
    /// Server-assigned `ItemModifier` children (class, level).
    pub modifiers: Vec<(String, u8)>,
}

/// One persisted hotbar assignment: a skill GC class bound to a hotbar slot
/// id (`0x64..=0x6D` = hotbar slots 1-10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotbarSlot {
    pub slot: u32,
    pub skill: String,
}

/// One persisted character record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Character {
    pub sqlid: u32,
    pub name: String,
    pub avatar_class: String,
    pub appearance: Appearance,
    pub level: u32,
    /// Progress into the current level (`Hero +0x320`), 0 until the client's
    /// first self-awarded kill. The client is XP-authoritative; this mirrors it
    /// so the avatar re-send carries the same word (never clobbers it).
    pub experience: u32,
    /// Unspent bonus attribute points (`Hero +0x33c`), 5 per level.
    pub attribute_points: u32,
    /// Unspent skill points (`Skills +0x70`), one per level after the first.
    pub skill_points: u32,
    /// Bought skill levels keyed by the skill FQN (`skills.generic.Butcher`).
    pub skill_levels: BTreeMap<String, u8>,
    /// Allocated Strength (`Hero +0x334`).
    pub strength: u32,
    /// Allocated Agility (`Hero +0x336`).
    pub agility: u32,
    /// Allocated Toughness (`Hero +0x338`).
    pub toughness: u32,
    /// Allocated Power (`Hero +0x33A`).
    pub power: u32,
    pub zone: String,
    /// Monster difficulty index (0 Normal, 1 Formidable, 2 Extreme,
    /// 3 Insane). The client sends the index on `(09,17)`.
    pub difficulty: u8,
    /// Quest rows keyed by the ten-hex-digit wire id (`049dd6befb`).
    pub quests: BTreeMap<String, QuestRecord>,
    /// The bag contents, in the order they were acquired.
    pub bag: Vec<BagItem>,
    /// The purse (`Container+0x74`).
    pub gold: u32,
    /// The worn items. `None` means "no persisted state": the join seeds the
    /// class starting kit as before. `Some([])` is an explicit naked avatar.
    pub equipment: Option<Vec<EquipmentItem>>,
    /// The hotbar assignments. `None` means "no persisted state": the join
    /// seeds the class starting slots. `Some([])` is an empty hotbar.
    pub hotbar: Option<Vec<HotbarSlot>>,
    /// The last zone left through a zone transfer, as
    /// `(zone, spawn point name)`. The obelisk dialog draws this as its
    /// Recent Zone Portal row; the `0x0C` request returns there. `None` is
    /// the client's "No Recent Zone Portal" state.
    pub recent_zone_portal: Option<(String, String)>,
    /// Every zone this character has entered at least once, in visit order.
    /// A non-faithful convenience: the join maps these to the game's real
    /// obelisk `Checkpoint` classes so the Saved Places list offers a
    /// fast-travel row for each dungeon the character has been to.
    pub visited_zones: Vec<String>,
}

/// The ten-hex-digit wire id of a u32 quest hash (`0x04 || LE u32`).
pub fn qid_to_hex(qid: u32) -> String {
    let bytes = [
        0x04,
        qid as u8,
        (qid >> 8) as u8,
        (qid >> 16) as u8,
        (qid >> 24) as u8,
    ];
    let mut out = String::with_capacity(10);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The u32 quest hash a ten-hex-digit wire id names, or `None`.
pub fn qid_from_hex(hex: &str) -> Option<u32> {
    let bytes = hex.as_bytes();
    if bytes.len() != 10 || bytes[0] != b'0' || bytes[1] != b'4' {
        return None;
    }
    let mut raw = [0u8; 5];
    for i in 0..5 {
        let hi = hex_digit(bytes[i * 2]).ok()?;
        let lo = hex_digit(bytes[i * 2 + 1]).ok()?;
        raw[i] = (hi << 4) | lo;
    }
    Some(u32::from_le_bytes([raw[1], raw[2], raw[3], raw[4]]))
}

/// Everything that can go wrong opening or mutating a store.
#[derive(Debug)]
pub enum StoreError {
    /// The backing file could not be read or written.
    Io(io::Error),
    /// The file exists but is not a well-formed store.
    Malformed(String),
    /// The proposed name violates the 1-9 printable-ASCII rule.
    InvalidName,
    /// The proposed class is empty or carries a NUL byte.
    InvalidClass,
    /// A character with the proposed name already exists.
    DuplicateName,
    /// The account already owns [`MAX_CHARS_PER_ACCOUNT`] characters.
    AccountFull,
    /// No such character for this account.
    NoSuchCharacter,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "store io error: {e}"),
            StoreError::Malformed(m) => write!(f, "malformed store: {m}"),
            StoreError::InvalidName => write!(
                f,
                "character name must be 1-9 printable ASCII bytes without spaces"
            ),
            StoreError::InvalidClass => {
                write!(f, "avatar class must be non-empty and NUL-free")
            }
            StoreError::DuplicateName => write!(f, "a character with that name already exists"),
            StoreError::AccountFull => {
                write!(f, "account already has {MAX_CHARS_PER_ACCOUNT} characters")
            }
            StoreError::NoSuchCharacter => write!(f, "no such character for this account"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// Per-account roster.
#[derive(Debug, Default)]
struct Account {
    characters: BTreeMap<u32, Character>,
}

/// The in-memory store, backed by one JSON file.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    next_sqlid: u32,
    accounts: BTreeMap<String, Account>,
}

impl Store {
    /// Open (or start) the store at `path`.
    ///
    /// A missing file yields an empty store. A present but malformed file is
    /// rejected; it is never silently replaced.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let mut store = Store {
            path,
            next_sqlid: 0,
            accounts: BTreeMap::new(),
        };
        match fs::read(&store.path) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|_| {
                    StoreError::Malformed("store file is not valid UTF-8".to_string())
                })?;
                if !text.trim().is_empty() {
                    store.parse_into(&text)?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::Io(e)),
        }
        // Allocate above every sqlid the file already holds so a freshly
        // loaded store can never hand out a duplicate.
        if let Some(max) = store
            .accounts
            .values()
            .flat_map(|a| a.characters.keys().copied())
            .max()
        {
            if store.next_sqlid <= max {
                store.next_sqlid = max.saturating_add(1);
            }
        }
        if store.next_sqlid == 0 {
            store.next_sqlid = 1;
        }
        Ok(store)
    }

    /// Create the account if it does not already exist.
    pub fn ensure_account(&mut self, account: &str) -> Result<(), StoreError> {
        if !self.accounts.contains_key(account) {
            self.accounts
                .insert(account.to_string(), Account::default());
            self.save()?;
        }
        Ok(())
    }

    /// The account's characters, ordered by ascending sqlid.
    pub fn list_chars(&self, account: &str) -> Vec<&Character> {
        self.accounts
            .get(account)
            .map(|a| a.characters.values().collect())
            .unwrap_or_default()
    }

    /// Create a character and return its freshly allocated sqlid.
    ///
    /// Refusals (bad name/class, duplicate name, full account) leave the store
    /// exactly as it was. Names are unique across the whole store.
    pub fn create_char(
        &mut self,
        account: &str,
        name: &str,
        avatar_class: &str,
        appearance: Appearance,
    ) -> Result<u32, StoreError> {
        validate_name(name)?;
        validate_class(avatar_class)?;
        if self.name_taken(name) {
            return Err(StoreError::DuplicateName);
        }
        let count = self.accounts.get(account).map_or(0, |a| a.characters.len());
        if count >= MAX_CHARS_PER_ACCOUNT {
            return Err(StoreError::AccountFull);
        }
        let sqlid = self.alloc_sqlid();
        let character = Character {
            sqlid,
            name: name.to_string(),
            avatar_class: avatar_class.to_string(),
            appearance,
            level: DEFAULT_LEVEL,
            experience: DEFAULT_EXPERIENCE,
            attribute_points: DEFAULT_ATTRIBUTE_POINTS,
            skill_points: 0,
            skill_levels: BTreeMap::new(),
            strength: 0,
            agility: 0,
            toughness: 0,
            power: 0,
            zone: DEFAULT_ZONE.to_string(),
            difficulty: 0,
            quests: BTreeMap::new(),
            bag: Vec::new(),
            gold: DEFAULT_GOLD,
            equipment: None,
            hotbar: None,
            recent_zone_portal: None,
            visited_zones: Vec::new(),
        };
        self.accounts
            .entry(account.to_string())
            .or_default()
            .characters
            .insert(sqlid, character);
        self.save()?;
        Ok(sqlid)
    }

    /// Delete a character owned by `account`.
    pub fn delete_char(&mut self, account: &str, sqlid: u32) -> Result<(), StoreError> {
        let removed = self
            .accounts
            .get_mut(account)
            .and_then(|a| a.characters.remove(&sqlid));
        if removed.is_none() {
            return Err(StoreError::NoSuchCharacter);
        }
        self.save()?;
        Ok(())
    }

    /// Record the last zone left through a zone transfer, for the obelisk
    /// dialog's Recent Zone Portal row. The zone and spawn are both
    /// content-owned names; NULs are stripped so the wire cstrings stay clean.
    pub fn set_recent_zone_portal(
        &mut self,
        account: &str,
        sqlid: u32,
        zone: &str,
        spawn: &str,
    ) -> Result<(), StoreError> {
        let zone = zone.split('\u{0}').next().unwrap_or("");
        if zone.is_empty() {
            return Ok(());
        }
        let spawn = spawn.split('\u{0}').next().unwrap_or("");
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.recent_zone_portal = Some((zone.to_string(), spawn.to_string()));
        }
        self.save()?;
        Ok(())
    }

    /// Borrow a character owned by `account`.
    pub fn get_char(&self, account: &str, sqlid: u32) -> Option<&Character> {
        self.accounts
            .get(account)
            .and_then(|a| a.characters.get(&sqlid))
    }

    /// Record a zone the character has entered, for the non-faithful obelisk
    /// "everywhere I've been" list. Idempotent: an already-recorded zone is a
    /// no-op and does not rewrite the store file. NULs are stripped so the
    /// persisted name stays a clean content zone id.
    pub fn record_visited_zone(
        &mut self,
        account: &str,
        sqlid: u32,
        zone: &str,
    ) -> Result<(), StoreError> {
        let zone = zone.split('\u{0}').next().unwrap_or("").trim();
        if zone.is_empty() {
            return Ok(());
        }
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            if character
                .visited_zones
                .iter()
                .any(|known| known.eq_ignore_ascii_case(zone))
            {
                return Ok(());
            }
            character.visited_zones.push(zone.to_string());
        }
        self.save()?;
        Ok(())
    }

    /// Set the character's zone. A NUL-split empty zone falls back to `town`.
    pub fn set_zone(&mut self, account: &str, sqlid: u32, zone: &str) -> Result<(), StoreError> {
        let zone = zone.split('\u{0}').next().unwrap_or("");
        let zone = if zone.is_empty() { DEFAULT_ZONE } else { zone };
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.zone = zone.to_string();
        }
        self.save()?;
        Ok(())
    }

    /// Persist the monster-difficulty index (0..=3).
    pub fn set_difficulty(
        &mut self,
        account: &str,
        sqlid: u32,
        difficulty: u8,
    ) -> Result<(), StoreError> {
        if difficulty > 3 {
            return Err(StoreError::Malformed(
                "difficulty must be an integer in 0..=3".to_string(),
            ));
        }
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.difficulty = difficulty;
        }
        self.save()?;
        Ok(())
    }

    /// Commit the character's leveling progress: level, XP into that level and
    /// unspent bonus attribute points.
    ///
    /// The client self-awards XP on a kill; the server mirrors the same math and
    /// persists the result here so the avatar re-send on a zone transfer carries
    /// the client's true level and XP.
    pub fn set_progress(
        &mut self,
        account: &str,
        sqlid: u32,
        level: u32,
        experience: u32,
        attribute_points: u32,
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.level = level.max(1);
            character.experience = experience;
            character.attribute_points = attribute_points;
        }
        self.save()?;
        Ok(())
    }

    /// Commit progress and the skill-point pool in one atomic write. A kill
    /// awards both, and two `save()` calls copied and fsynced the whole roster
    /// twice for one event.
    pub fn set_progress_and_skill_points(
        &mut self,
        account: &str,
        sqlid: u32,
        level: u32,
        experience: u32,
        attribute_points: u32,
        skill_points: u32,
    ) -> Result<(), StoreError> {
        let level = level.max(1);
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            if character.level == level
                && character.experience == experience
                && character.attribute_points == attribute_points
                && character.skill_points == skill_points
            {
                return Ok(());
            }
            character.level = level;
            character.experience = experience;
            character.attribute_points = attribute_points;
            character.skill_points = skill_points;
        }
        self.save()?;
        Ok(())
    }

    /// Commit the character's unspent pool and four allocated primary stats.
    ///
    /// The session has already validated the request against the pool; this is
    /// the one write that keeps `Hero +0x33c` and
    /// `Hero +0x334/+0x336/+0x338/+0x33A` in lockstep so a re-join can ship
    /// both halves of the client's `5 * (level - 1)` invariant.
    pub fn set_attribute_allocation(
        &mut self,
        account: &str,
        sqlid: u32,
        attribute_points: u32,
        attributes: [u32; 4],
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.attribute_points = attribute_points;
            character.strength = attributes[0];
            character.agility = attributes[1];
            character.toughness = attributes[2];
            character.power = attributes[3];
        }
        self.save()?;
        Ok(())
    }

    /// Commit the unspent skill-point pool after a level-up or a purchase.
    pub fn set_skill_points(
        &mut self,
        account: &str,
        sqlid: u32,
        skill_points: u32,
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.skill_points = skill_points;
        }
        self.save()?;
        Ok(())
    }

    /// Commit the bought skill levels and the remaining skill-point pool.
    pub fn set_skill_state(
        &mut self,
        account: &str,
        sqlid: u32,
        skill_points: u32,
        skill_levels: &BTreeMap<String, u8>,
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.skill_points = skill_points;
            character.skill_levels = skill_levels.clone();
        }
        self.save()?;
        Ok(())
    }

    /// The character's full quest snapshot: state, instance id and objectives,
    /// keyed by the ten-hex-digit wire id.
    pub fn quest_snapshot(&self, account: &str, sqlid: u32) -> BTreeMap<String, QuestRecord> {
        self.get_char(account, sqlid)
            .map(|character| character.quests.clone())
            .unwrap_or_default()
    }

    /// Every quest `sqlid` has TURNED IN, as u32 hashes -- the offer
    /// directory's completed gate (`server.py:quest_completed_qids`).
    pub fn quest_completed_qids(&self, account: &str, sqlid: u32) -> HashSet<u32> {
        self.get_char(account, sqlid)
            .map(|character| {
                character
                    .quests
                    .iter()
                    .filter(|(_, record)| record.state == QuestState::TurnedIn)
                    .filter_map(|(hex, _)| qid_from_hex(hex))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The state record for one quest, or `None`.
    pub fn quest_get(&self, account: &str, sqlid: u32, qid: u32) -> Option<QuestRecord> {
        self.get_char(account, sqlid)
            .and_then(|character| character.quests.get(&qid_to_hex(qid)).cloned())
    }

    /// Commit a quest accept: `accepted`, its minted instance id and its
    /// objectives (`server.py:mark_accepted`).
    pub fn mark_accepted(
        &mut self,
        account: &str,
        sqlid: u32,
        qid: u32,
        instance_ref: u32,
        objectives: Vec<QuestObjective>,
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.quests.insert(
                qid_to_hex(qid),
                QuestRecord {
                    state: QuestState::Accepted,
                    instance_ref,
                    objectives,
                },
            );
        }
        self.save()?;
        Ok(())
    }

    /// Update one held objective's flags and label, keeping the persisted
    /// kill counter in the same `(flags, label)` pair the AddQuest replay
    /// carries.
    #[allow(clippy::too_many_arguments)]
    pub fn quest_update_objective(
        &mut self,
        account: &str,
        sqlid: u32,
        qid: u32,
        instance_ref: u32,
        index: usize,
        flags: u8,
        label: String,
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            let record = character
                .quests
                .get_mut(&qid_to_hex(qid))
                .ok_or(StoreError::NoSuchCharacter)?;
            if record.instance_ref != instance_ref {
                return Err(StoreError::Malformed(format!(
                    "quest {qid:#010x} instance {instance_ref} does not match the stored instance {}",
                    record.instance_ref
                )));
            }
            let objective = record.objectives.get_mut(index).ok_or_else(|| {
                StoreError::Malformed(format!("quest {qid:#010x} has no objective index {index}"))
            })?;
            objective.flags = flags;
            objective.label = label;
        }
        self.save()?;
        Ok(())
    }

    /// Delete one held quest row entirely (the client's Abandon).
    pub fn remove_quest(&mut self, account: &str, sqlid: u32, qid: u32) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.quests.remove(&qid_to_hex(qid));
        }
        self.save()?;
        Ok(())
    }

    /// Set one quest's state, creating the row when it does not exist
    /// (`server.py:quest_set_state`).
    pub fn quest_set_state(
        &mut self,
        account: &str,
        sqlid: u32,
        qid: u32,
        state: QuestState,
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            let record = character
                .quests
                .entry(qid_to_hex(qid))
                .or_insert_with(|| QuestRecord {
                    state,
                    instance_ref: 0,
                    objectives: Vec::new(),
                });
            record.state = state;
        }
        self.save()?;
        Ok(())
    }

    /// The character's persisted bag, or an empty list.
    pub fn get_bag(&self, account: &str, sqlid: u32) -> Vec<BagItem> {
        self.get_char(account, sqlid)
            .map(|character| character.bag.clone())
            .unwrap_or_default()
    }

    /// Replace the character's persisted bag.
    pub fn set_bag(
        &mut self,
        account: &str,
        sqlid: u32,
        bag: Vec<BagItem>,
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.bag = bag;
        }
        self.save()?;
        Ok(())
    }

    /// Insert or replace one bag row. Rows are keyed by class and cell; a move
    /// is a remove of the old row followed by an upsert at the new cell.
    pub fn upsert_bag_item(
        &mut self,
        account: &str,
        sqlid: u32,
        item: BagItem,
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            if let Some(row) = character.bag.iter_mut().find(|row| {
                row.class == item.class && row.cell_x == item.cell_x && row.cell_y == item.cell_y
            }) {
                *row = item;
            } else {
                character.bag.push(item);
            }
        }
        self.save()?;
        Ok(())
    }

    /// Remove the bag row at `class`/`cell`, if present.
    pub fn remove_bag_item(
        &mut self,
        account: &str,
        sqlid: u32,
        class: &str,
        cell: (u8, u8),
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.bag.retain(|row| {
                !(row.class == class && row.cell_x == cell.0 && row.cell_y == cell.1)
            });
        }
        self.save()?;
        Ok(())
    }

    /// The character's persisted worn items, or `None` when the row never
    /// stored an equipment set (the class starting kit applies).
    pub fn get_equipment(&self, account: &str, sqlid: u32) -> Option<Vec<EquipmentItem>> {
        self.get_char(account, sqlid)
            .and_then(|character| character.equipment.clone())
    }

    /// Replace the character's worn items with an explicit set.
    pub fn set_equipment(
        &mut self,
        account: &str,
        sqlid: u32,
        equipment: Vec<EquipmentItem>,
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.equipment = Some(equipment);
        }
        self.save()?;
        Ok(())
    }

    /// The character's persisted hotbar assignments, or `None` when the row
    /// never stored any (the class starting slots apply).
    pub fn get_hotbar(&self, account: &str, sqlid: u32) -> Option<Vec<HotbarSlot>> {
        self.get_char(account, sqlid)
            .and_then(|character| character.hotbar.clone())
    }

    /// Replace the character's hotbar assignments with an explicit set.
    pub fn set_hotbar(
        &mut self,
        account: &str,
        sqlid: u32,
        hotbar: Vec<HotbarSlot>,
    ) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.hotbar = Some(hotbar);
        }
        self.save()?;
        Ok(())
    }

    /// The character's persisted purse, or `None` for an unknown character.
    pub fn get_gold(&self, account: &str, sqlid: u32) -> Option<u32> {
        self.get_char(account, sqlid)
            .map(|character| character.gold)
    }

    /// Set the character's persisted purse.
    pub fn set_gold(&mut self, account: &str, sqlid: u32, gold: u32) -> Result<(), StoreError> {
        {
            let character = self
                .accounts
                .get_mut(account)
                .and_then(|a| a.characters.get_mut(&sqlid))
                .ok_or(StoreError::NoSuchCharacter)?;
            character.gold = gold;
        }
        self.save()?;
        Ok(())
    }

    /// Write the whole store atomically (temp file + rename).
    pub fn save(&self) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let text = format!("{}\n", serialize(&self.to_json()));
        // Keep the previous good roster beside the store. The atomic rename
        // below protects a reader from a torn write; this backup protects the
        // roster from a later bad save.
        if self.path.is_file() {
            let bak_os = self.path.as_os_str().to_owned();
            let bak = PathBuf::from(format!("{}.bak", bak_os.to_string_lossy()));
            let _ = fs::copy(&self.path, bak);
        }
        let mut tmp_os = self.path.as_os_str().to_owned();
        tmp_os.push(".tmp");
        let tmp = PathBuf::from(tmp_os);
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.flush()?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn name_taken(&self, name: &str) -> bool {
        self.accounts
            .values()
            .any(|a| a.characters.values().any(|c| c.name == name))
    }

    fn alloc_sqlid(&mut self) -> u32 {
        let sqlid = self.next_sqlid;
        self.next_sqlid = self.next_sqlid.saturating_add(1);
        sqlid
    }

    fn to_json(&self) -> Json {
        let mut accounts = Vec::with_capacity(self.accounts.len());
        for (name, account) in &self.accounts {
            let mut characters = Vec::with_capacity(account.characters.len());
            for (sqlid, character) in &account.characters {
                characters.push((sqlid.to_string(), character.to_json()));
            }
            accounts.push((
                name.clone(),
                Json::Obj(vec![("characters".to_string(), Json::Obj(characters))]),
            ));
        }
        Json::Obj(vec![
            ("next_sqlid".to_string(), Json::Num(self.next_sqlid as f64)),
            ("accounts".to_string(), Json::Obj(accounts)),
        ])
    }

    fn parse_into(&mut self, text: &str) -> Result<(), StoreError> {
        let root = parse_json(text)?;
        let root = root.as_obj().ok_or_else(|| {
            StoreError::Malformed("top-level store value must be an object".to_string())
        })?;
        if let Some(next) = find(root, "next_sqlid") {
            self.next_sqlid = next.as_u32().ok_or_else(|| {
                StoreError::Malformed("next_sqlid must be a non-negative integer".to_string())
            })?;
        }
        self.read_lab_characters(root)?;
        self.read_clean_accounts(root)?;
        Ok(())
    }

    fn read_lab_characters(&mut self, root: &[(String, Json)]) -> Result<(), StoreError> {
        let Some(chars) = find(root, "characters") else {
            return Ok(());
        };
        let chars = chars
            .as_obj()
            .ok_or_else(|| StoreError::Malformed("characters must be an object".to_string()))?;
        for (key, record) in chars {
            let sqlid = key.parse::<u32>().map_err(|_| {
                StoreError::Malformed(format!("character key {key:?} is not a u32 sqlid"))
            })?;
            let record = record.as_obj().ok_or_else(|| {
                StoreError::Malformed(format!("character {sqlid} must be an object"))
            })?;
            let account = get_str(record, "account").ok_or_else(|| {
                StoreError::Malformed(format!("character {sqlid} is missing account"))
            })?;
            let name = get_str(record, "name").ok_or_else(|| {
                StoreError::Malformed(format!("character {sqlid} is missing name"))
            })?;
            let class = get_str(record, "class").ok_or_else(|| {
                StoreError::Malformed(format!("character {sqlid} is missing class"))
            })?;
            let appearance = read_appearance(record)?;
            let level = read_level(record, sqlid)?;
            let experience = read_experience(record, sqlid)?;
            let attribute_points = read_attribute_points(record, sqlid)?;
            let skill_points = read_skill_points(record, level, sqlid)?;
            let skill_levels = read_skill_levels(record, sqlid)?;
            let strength = read_stat(record, sqlid, "strength")?;
            let agility = read_stat(record, sqlid, "agility")?;
            let toughness = read_stat(record, sqlid, "toughness")?;
            let power = read_stat(record, sqlid, "power")?;
            let zone = read_zone(record, sqlid)?;
            let difficulty = read_difficulty(record, sqlid)?;
            let quests = read_quests(record, sqlid)?;
            let bag = read_bag(record, sqlid)?;
            let gold = read_gold(record, sqlid)?;
            let equipment = read_equipment(record, sqlid)?;
            let hotbar = read_hotbar(record, sqlid)?;
            let recent_zone_portal = read_recent_zone_portal(record, sqlid)?;
            let visited_zones = read_visited_zones(record, sqlid)?;
            self.insert_loaded(
                account.to_string(),
                Character {
                    sqlid,
                    name: name.to_string(),
                    avatar_class: class.to_string(),
                    appearance,
                    level,
                    experience,
                    attribute_points,
                    skill_points,
                    skill_levels,
                    strength,
                    agility,
                    toughness,
                    power,
                    zone,
                    difficulty,
                    quests,
                    bag,
                    gold,
                    equipment,
                    hotbar,
                    recent_zone_portal,
                    visited_zones,
                },
            )?;
        }
        Ok(())
    }

    fn read_clean_accounts(&mut self, root: &[(String, Json)]) -> Result<(), StoreError> {
        let Some(accounts) = find(root, "accounts") else {
            return Ok(());
        };
        let accounts = accounts
            .as_obj()
            .ok_or_else(|| StoreError::Malformed("accounts must be an object".to_string()))?;
        for (account, value) in accounts {
            let value = value.as_obj().ok_or_else(|| {
                StoreError::Malformed(format!("account {account:?} must be an object"))
            })?;
            self.accounts.entry(account.clone()).or_default();
            // The reference account values only carry created_ts; skip them.
            let Some(chars) = find(value, "characters") else {
                continue;
            };
            let chars = chars.as_obj().ok_or_else(|| {
                StoreError::Malformed(format!("account {account:?} characters must be an object"))
            })?;
            for (key, record) in chars {
                let sqlid = key.parse::<u32>().map_err(|_| {
                    StoreError::Malformed(format!("character key {key:?} is not a u32 sqlid"))
                })?;
                let record = record.as_obj().ok_or_else(|| {
                    StoreError::Malformed(format!("character {sqlid} must be an object"))
                })?;
                let name = get_str(record, "name").ok_or_else(|| {
                    StoreError::Malformed(format!("character {sqlid} is missing name"))
                })?;
                let class = get_str(record, "class").ok_or_else(|| {
                    StoreError::Malformed(format!("character {sqlid} is missing class"))
                })?;
                let appearance = read_appearance(record)?;
                let level = read_level(record, sqlid)?;
                let experience = read_experience(record, sqlid)?;
                let attribute_points = read_attribute_points(record, sqlid)?;
                let skill_points = read_skill_points(record, level, sqlid)?;
                let skill_levels = read_skill_levels(record, sqlid)?;
                let strength = read_stat(record, sqlid, "strength")?;
                let agility = read_stat(record, sqlid, "agility")?;
                let toughness = read_stat(record, sqlid, "toughness")?;
                let power = read_stat(record, sqlid, "power")?;
                let zone = read_zone(record, sqlid)?;
                let difficulty = read_difficulty(record, sqlid)?;
                let quests = read_quests(record, sqlid)?;
                let bag = read_bag(record, sqlid)?;
                let gold = read_gold(record, sqlid)?;
                let equipment = read_equipment(record, sqlid)?;
                let hotbar = read_hotbar(record, sqlid)?;
                let recent_zone_portal = read_recent_zone_portal(record, sqlid)?;
                let visited_zones = read_visited_zones(record, sqlid)?;
                self.insert_loaded(
                    account.clone(),
                    Character {
                        sqlid,
                        name: name.to_string(),
                        avatar_class: class.to_string(),
                        appearance,
                        level,
                        experience,
                        attribute_points,
                        skill_points,
                        skill_levels,
                        strength,
                        agility,
                        toughness,
                        power,
                        zone,
                        difficulty,
                        quests,
                        bag,
                        gold,
                        equipment,
                        hotbar,
                        recent_zone_portal,
                        visited_zones,
                    },
                )?;
            }
        }
        Ok(())
    }

    fn insert_loaded(&mut self, account: String, character: Character) -> Result<(), StoreError> {
        if self
            .accounts
            .values()
            .any(|a| a.characters.contains_key(&character.sqlid))
        {
            return Err(StoreError::Malformed(format!(
                "duplicate sqlid {} in store",
                character.sqlid
            )));
        }
        self.accounts
            .entry(account)
            .or_default()
            .characters
            .insert(character.sqlid, character);
        Ok(())
    }
}

impl Character {
    fn to_json(&self) -> Json {
        let mut appearance = Vec::new();
        if let Some(v) = self.appearance.skin {
            appearance.push(("skin".to_string(), Json::Num(v as f64)));
        }
        if let Some(v) = self.appearance.face {
            appearance.push(("face".to_string(), Json::Num(v as f64)));
        }
        if let Some(v) = self.appearance.face_feature {
            appearance.push(("face_feature".to_string(), Json::Num(v as f64)));
        }
        if let Some(v) = self.appearance.hair {
            appearance.push(("hair".to_string(), Json::Num(v as f64)));
        }
        if let Some(v) = self.appearance.hair_color {
            appearance.push(("hair_color".to_string(), Json::Num(v as f64)));
        }
        let mut quests = Vec::with_capacity(self.quests.len());
        for (qid, record) in &self.quests {
            let objectives: Vec<Json> = record
                .objectives
                .iter()
                .map(|objective| {
                    Json::Obj(vec![
                        ("flags".to_string(), Json::Num(objective.flags as f64)),
                        ("label".to_string(), Json::Str(objective.label.clone())),
                    ])
                })
                .collect();
            let fields = vec![
                (
                    "state".to_string(),
                    Json::Str(record.state.as_str().to_string()),
                ),
                (
                    "instance_ref".to_string(),
                    Json::Num(record.instance_ref as f64),
                ),
                ("objectives".to_string(), Json::Arr(objectives)),
            ];
            quests.push((qid.clone(), Json::Obj(fields)));
        }
        let bag: Vec<Json> = self
            .bag
            .iter()
            .map(|row| {
                let mut fields = vec![
                    ("class".to_string(), Json::Str(row.class.clone())),
                    ("cell_x".to_string(), Json::Num(row.cell_x as f64)),
                    ("cell_y".to_string(), Json::Num(row.cell_y as f64)),
                    ("count".to_string(), Json::Num(row.count as f64)),
                ];
                if let Some(level) = row.level {
                    fields.push(("level".to_string(), Json::Num(level as f64)));
                }
                if let Some(flags) = row.flags {
                    fields.push(("flags".to_string(), Json::Num(flags as f64)));
                }
                if !row.modifiers.is_empty() {
                    fields.push(("mods".to_string(), modifiers_json(&row.modifiers)));
                }
                Json::Obj(fields)
            })
            .collect();
        let mut fields = vec![
            ("name".to_string(), Json::Str(self.name.clone())),
            ("class".to_string(), Json::Str(self.avatar_class.clone())),
            ("appearance".to_string(), Json::Obj(appearance)),
            ("level".to_string(), Json::Num(self.level as f64)),
            ("experience".to_string(), Json::Num(self.experience as f64)),
            (
                "attribute_points".to_string(),
                Json::Num(self.attribute_points as f64),
            ),
            (
                "skill_points".to_string(),
                Json::Num(self.skill_points as f64),
            ),
            (
                "skill_levels".to_string(),
                Json::Obj(
                    self.skill_levels
                        .iter()
                        .map(|(skill, level)| (skill.clone(), Json::Num(*level as f64)))
                        .collect(),
                ),
            ),
            ("strength".to_string(), Json::Num(self.strength as f64)),
            ("agility".to_string(), Json::Num(self.agility as f64)),
            ("toughness".to_string(), Json::Num(self.toughness as f64)),
            ("power".to_string(), Json::Num(self.power as f64)),
            ("zone".to_string(), Json::Str(self.zone.clone())),
            ("difficulty".to_string(), Json::Num(self.difficulty as f64)),
            ("quests".to_string(), Json::Obj(quests)),
            ("bag".to_string(), Json::Arr(bag)),
            ("gold".to_string(), Json::Num(self.gold as f64)),
        ];
        if let Some(equipment) = &self.equipment {
            let rows: Vec<Json> = equipment
                .iter()
                .map(|row| {
                    Json::Obj(vec![
                        ("slot".to_string(), Json::Num(row.slot as f64)),
                        ("class".to_string(), Json::Str(row.class.clone())),
                        ("level".to_string(), Json::Num(row.level as f64)),
                        ("flags".to_string(), Json::Num(row.flags as f64)),
                        ("mods".to_string(), modifiers_json(&row.modifiers)),
                    ])
                })
                .collect();
            fields.push(("equipment".to_string(), Json::Arr(rows)));
        }
        if let Some(hotbar) = &self.hotbar {
            let rows: Vec<Json> = hotbar
                .iter()
                .map(|row| {
                    Json::Obj(vec![
                        ("slot".to_string(), Json::Num(row.slot as f64)),
                        ("skill".to_string(), Json::Str(row.skill.clone())),
                    ])
                })
                .collect();
            fields.push(("hotbar".to_string(), Json::Arr(rows)));
        }
        if let Some((zone, spawn)) = &self.recent_zone_portal {
            fields.push((
                "recent_zone_portal".to_string(),
                Json::Obj(vec![
                    ("zone".to_string(), Json::Str(zone.clone())),
                    ("spawn".to_string(), Json::Str(spawn.clone())),
                ]),
            ));
        }
        if !self.visited_zones.is_empty() {
            fields.push((
                "visited_zones".to_string(),
                Json::Arr(
                    self.visited_zones
                        .iter()
                        .map(|zone| Json::Str(zone.clone()))
                        .collect(),
                ),
            ));
        }
        Json::Obj(fields)
    }
}

/// Read the character's quest rows. A missing `quests` object is the empty
/// snapshot; a malformed row is a typed error.
fn read_quests(
    record: &[(String, Json)],
    sqlid: u32,
) -> Result<BTreeMap<String, QuestRecord>, StoreError> {
    let Some(value) = find(record, "quests") else {
        return Ok(BTreeMap::new());
    };
    let object = value.as_obj().ok_or_else(|| {
        StoreError::Malformed(format!("character {sqlid} quests must be an object"))
    })?;
    let mut quests = BTreeMap::new();
    for (qid, row) in object {
        if qid_from_hex(qid).is_none() {
            return Err(StoreError::Malformed(format!(
                "character {sqlid} quest key {qid:?} is not a 5-byte wire id"
            )));
        }
        let row = row.as_obj().ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} quest {qid} must be an object"))
        })?;
        let state_text = get_str(row, "state").ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} quest {qid} is missing state"))
        })?;
        let state = QuestState::parse(state_text).ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} quest {qid} has unknown state {state_text:?}"
            ))
        })?;
        let instance_ref = match find(row, "instance_ref") {
            Some(value) => value.as_u32().ok_or_else(|| {
                StoreError::Malformed(format!(
                    "character {sqlid} quest {qid} instance_ref must be an integer"
                ))
            })?,
            None => 0,
        };
        let mut objectives = Vec::new();
        if let Some(value) = find(row, "objectives") {
            let list = value.as_arr().ok_or_else(|| {
                StoreError::Malformed(format!(
                    "character {sqlid} quest {qid} objectives must be an array"
                ))
            })?;
            for objective in list {
                let objective = objective.as_obj().ok_or_else(|| {
                    StoreError::Malformed(format!(
                        "character {sqlid} quest {qid} objective must be an object"
                    ))
                })?;
                let flags = find(objective, "flags")
                    .and_then(Json::as_u32)
                    .ok_or_else(|| {
                        StoreError::Malformed(format!(
                            "character {sqlid} quest {qid} objective flags must be an integer"
                        ))
                    })?;
                if flags > u32::from(u8::MAX) {
                    return Err(StoreError::Malformed(format!(
                        "character {sqlid} quest {qid} objective flags are out of byte range"
                    )));
                }
                let label = get_str(objective, "label").ok_or_else(|| {
                    StoreError::Malformed(format!(
                        "character {sqlid} quest {qid} objective is missing label"
                    ))
                })?;
                objectives.push(QuestObjective {
                    flags: flags as u8,
                    label: label.to_string(),
                });
            }
        }
        quests.insert(
            qid.clone(),
            QuestRecord {
                state,
                instance_ref,
                objectives,
            },
        );
    }
    Ok(quests)
}

fn modifiers_json(modifiers: &[(String, u8)]) -> Json {
    Json::Arr(
        modifiers
            .iter()
            .map(|(class, level)| {
                Json::Obj(vec![
                    ("class".to_string(), Json::Str(class.clone())),
                    ("level".to_string(), Json::Num(*level as f64)),
                ])
            })
            .collect(),
    )
}

fn read_modifiers(object: &[(String, Json)], sqlid: u32) -> Result<Vec<(String, u8)>, StoreError> {
    let Some(value) = find(object, "mods") else {
        return Ok(Vec::new());
    };
    let list = value.as_arr().ok_or_else(|| {
        StoreError::Malformed(format!("character {sqlid} item mods must be an array"))
    })?;
    let mut out = Vec::with_capacity(list.len());
    for row in list {
        let row = row.as_obj().ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} item mod must be an object"))
        })?;
        let class = get_str(row, "class").ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} item mod is missing class"))
        })?;
        let level = byte_field(row, "level", sqlid)?;
        out.push((class.to_string(), level));
    }
    Ok(out)
}

/// Read the character's bag rows. A missing `bag` array is empty.
fn read_bag(record: &[(String, Json)], sqlid: u32) -> Result<Vec<BagItem>, StoreError> {
    let Some(value) = find(record, "bag") else {
        return Ok(Vec::new());
    };
    let list = value
        .as_arr()
        .ok_or_else(|| StoreError::Malformed(format!("character {sqlid} bag must be an array")))?;
    let mut bag = Vec::with_capacity(list.len());
    for row in list {
        let row = row.as_obj().ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} bag row must be an object"))
        })?;
        let class = get_str(row, "class").ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} bag row is missing class"))
        })?;
        let cell_x = byte_field(row, "cell_x", sqlid)?;
        let cell_y = byte_field(row, "cell_y", sqlid)?;
        let count = find(row, "count").and_then(Json::as_u32).ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} bag row count must be an integer"
            ))
        })?;
        let level = optional_byte_field(row, "level", sqlid)?;
        let flags = optional_byte_field(row, "flags", sqlid)?;
        let modifiers = read_modifiers(row, sqlid)?;
        bag.push(BagItem {
            class: class.to_string(),
            cell_x,
            cell_y,
            count,
            level,
            flags,
            modifiers,
        });
    }
    Ok(bag)
}

/// Read an optional byte-valued bag row field.
fn optional_byte_field(
    object: &[(String, Json)],
    key: &str,
    sqlid: u32,
) -> Result<Option<u8>, StoreError> {
    let Some(value) = find(object, key) else {
        return Ok(None);
    };
    let value = value.as_u32().ok_or_else(|| {
        StoreError::Malformed(format!(
            "character {sqlid} bag row {key} must be an integer"
        ))
    })?;
    u8::try_from(value).map(Some).map_err(|_| {
        StoreError::Malformed(format!(
            "character {sqlid} bag row {key} is out of byte range"
        ))
    })
}

/// Read the character's worn items. A missing `equipment` key is `None`
/// (seed the class starting kit); an explicit empty array is `Some([])`.
fn read_equipment(
    record: &[(String, Json)],
    sqlid: u32,
) -> Result<Option<Vec<EquipmentItem>>, StoreError> {
    let Some(value) = find(record, "equipment") else {
        return Ok(None);
    };
    let list = value.as_arr().ok_or_else(|| {
        StoreError::Malformed(format!("character {sqlid} equipment must be an array"))
    })?;
    let mut equipment = Vec::with_capacity(list.len());
    for row in list {
        let row = row.as_obj().ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} equipment row must be an object"))
        })?;
        let slot = find(row, "slot").and_then(Json::as_u32).ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} equipment row slot must be an integer"
            ))
        })?;
        let class = get_str(row, "class").ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} equipment row is missing class"))
        })?;
        let level = byte_field(row, "level", sqlid)?;
        let flags = byte_field(row, "flags", sqlid)?;
        let modifiers = read_modifiers(row, sqlid)?;
        equipment.push(EquipmentItem {
            slot,
            class: class.to_string(),
            level,
            flags,
            modifiers,
        });
    }
    Ok(Some(equipment))
}

/// Read the character's hotbar assignments. A missing `hotbar` key is `None`
/// (seed the class starting slots); an explicit empty array is `Some([])`.
fn read_hotbar(
    record: &[(String, Json)],
    sqlid: u32,
) -> Result<Option<Vec<HotbarSlot>>, StoreError> {
    let Some(value) = find(record, "hotbar") else {
        return Ok(None);
    };
    let list = value.as_arr().ok_or_else(|| {
        StoreError::Malformed(format!("character {sqlid} hotbar must be an array"))
    })?;
    let mut hotbar = Vec::with_capacity(list.len());
    for row in list {
        let row = row.as_obj().ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} hotbar row must be an object"))
        })?;
        let slot = find(row, "slot").and_then(Json::as_u32).ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} hotbar row slot must be an integer"
            ))
        })?;
        let skill = get_str(row, "skill").ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} hotbar row is missing skill"))
        })?;
        hotbar.push(HotbarSlot {
            slot,
            skill: skill.to_string(),
        });
    }
    Ok(Some(hotbar))
}

/// Read the character's progress-into-level. A missing `experience` key is/// Read the character's progress-into-level. A missing `experience` key is
/// [`DEFAULT_EXPERIENCE`] (a record written before XP was persisted).
fn read_experience(record: &[(String, Json)], sqlid: u32) -> Result<u32, StoreError> {
    match find(record, "experience") {
        Some(value) => value.as_u32().ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} experience must be a non-negative integer"
            ))
        }),
        None => Ok(DEFAULT_EXPERIENCE),
    }
}

/// Read the character's unspent bonus attribute points. A missing
/// `attribute_points` key is [`DEFAULT_ATTRIBUTE_POINTS`].
fn read_attribute_points(record: &[(String, Json)], sqlid: u32) -> Result<u32, StoreError> {
    match find(record, "attribute_points") {
        Some(value) => value.as_u32().ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} attribute_points must be a non-negative integer"
            ))
        }),
        None => Ok(DEFAULT_ATTRIBUTE_POINTS),
    }
}

/// Read the character's unspent skill points. A missing `skill_points` key
/// is `level - 1` (one skill point per level after the first, the value the
/// shipped level-25 capture carries as 24).
fn read_skill_points(record: &[(String, Json)], level: u32, sqlid: u32) -> Result<u32, StoreError> {
    match find(record, "skill_points") {
        Some(value) => value.as_u32().ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} skill_points must be a non-negative integer"
            ))
        }),
        None => Ok(level.saturating_sub(1)),
    }
}

/// Read the bought skill levels (FQN -> level). A missing object is empty.
fn read_skill_levels(
    record: &[(String, Json)],
    sqlid: u32,
) -> Result<BTreeMap<String, u8>, StoreError> {
    let Some(value) = find(record, "skill_levels") else {
        return Ok(BTreeMap::new());
    };
    let object = value.as_obj().ok_or_else(|| {
        StoreError::Malformed(format!("character {sqlid} skill_levels must be an object"))
    })?;
    let mut levels = BTreeMap::new();
    for (skill, level) in object {
        let value = level.as_u32().ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} skill_levels[{skill}] must be a non-negative integer"
            ))
        })?;
        levels.insert(skill.clone(), u8::try_from(value.min(255)).unwrap_or(255));
    }
    Ok(levels)
}

/// Read one allocated primary stat. A missing key is `0` (a record written
/// before per-stat allocation was persisted).
fn read_stat(record: &[(String, Json)], sqlid: u32, key: &str) -> Result<u32, StoreError> {
    match find(record, key) {
        Some(value) => value.as_u32().ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} {key} must be a non-negative integer"
            ))
        }),
        None => Ok(0),
    }
}

/// Read the character's purse. A missing `gold` key is [`DEFAULT_GOLD`].
fn read_gold(record: &[(String, Json)], sqlid: u32) -> Result<u32, StoreError> {
    match find(record, "gold") {
        Some(value) => value.as_u32().ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} gold must be a non-negative integer"
            ))
        }),
        None => Ok(DEFAULT_GOLD),
    }
}

fn byte_field(object: &[(String, Json)], key: &str, sqlid: u32) -> Result<u8, StoreError> {
    let value = find(object, key).and_then(Json::as_u32).ok_or_else(|| {
        StoreError::Malformed(format!(
            "character {sqlid} bag row {key} must be an integer"
        ))
    })?;
    u8::try_from(value).map_err(|_| {
        StoreError::Malformed(format!(
            "character {sqlid} bag row {key} is out of byte range"
        ))
    })
}

fn validate_name(name: &str) -> Result<(), StoreError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 9 {
        return Err(StoreError::InvalidName);
    }
    if bytes.iter().any(|&b| !(0x21..=0x7E).contains(&b)) {
        return Err(StoreError::InvalidName);
    }
    Ok(())
}

fn validate_class(class: &str) -> Result<(), StoreError> {
    if class.is_empty() || class.as_bytes().contains(&0) {
        return Err(StoreError::InvalidClass);
    }
    Ok(())
}

fn read_level(record: &[(String, Json)], sqlid: u32) -> Result<u32, StoreError> {
    match find(record, "level") {
        Some(v) => v.as_u32().ok_or_else(|| {
            StoreError::Malformed(format!("character {sqlid} level must be an integer"))
        }),
        None => Ok(DEFAULT_LEVEL),
    }
}

/// Read the persisted recent-zone-portal row. A missing object is `None`
/// (records written before the row existed), and a malformed object is a
/// typed store error rather than a silently dropped destination.
fn read_recent_zone_portal(
    record: &[(String, Json)],
    sqlid: u32,
) -> Result<Option<(String, String)>, StoreError> {
    let Some(value) = find(record, "recent_zone_portal") else {
        return Ok(None);
    };
    let object = value.as_obj().ok_or_else(|| {
        StoreError::Malformed(format!(
            "character {sqlid} recent_zone_portal must be an object"
        ))
    })?;
    let zone = get_str(object, "zone")
        .filter(|zone| !zone.is_empty())
        .ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} recent_zone_portal.zone must be a non-empty string"
            ))
        })?;
    let spawn = get_str(object, "spawn").unwrap_or("");
    Ok(Some((zone.to_string(), spawn.to_string())))
}

/// Read the persisted visited-zone list. A missing array is empty (records
/// written before the feature existed); a malformed row is a typed error
/// rather than a silently dropped destination. Order is preserved and
/// duplicates are collapsed case-insensitively.
fn read_visited_zones(record: &[(String, Json)], sqlid: u32) -> Result<Vec<String>, StoreError> {
    let Some(value) = find(record, "visited_zones") else {
        return Ok(Vec::new());
    };
    let rows = value.as_arr().ok_or_else(|| {
        StoreError::Malformed(format!("character {sqlid} visited_zones must be an array"))
    })?;
    let mut out: Vec<String> = Vec::new();
    for row in rows {
        let zone = row.as_str().ok_or_else(|| {
            StoreError::Malformed(format!(
                "character {sqlid} visited_zones entries must be strings"
            ))
        })?;
        let zone = zone.split('\u{0}').next().unwrap_or("").trim();
        if zone.is_empty() || out.iter().any(|known| known.eq_ignore_ascii_case(zone)) {
            continue;
        }
        out.push(zone.to_string());
    }
    Ok(out)
}

fn read_zone(record: &[(String, Json)], sqlid: u32) -> Result<String, StoreError> {
    match find(record, "zone") {
        Some(v) => Ok(v
            .as_str()
            .ok_or_else(|| {
                StoreError::Malformed(format!("character {sqlid} zone must be a string"))
            })?
            .to_string()),
        None => Ok(DEFAULT_ZONE.to_string()),
    }
}

/// Read the persisted monster-difficulty index. Missing means Normal (the
/// constructor default); out-of-range values are a typed error so a bad edit
/// cannot silently change combat.
fn read_difficulty(record: &[(String, Json)], sqlid: u32) -> Result<u8, StoreError> {
    let Some(value) = find(record, "difficulty") else {
        return Ok(0);
    };
    let number = value.as_f64().ok_or_else(|| {
        StoreError::Malformed(format!("character {sqlid} difficulty must be a number"))
    })?;
    if !(0.0..=3.0).contains(&number) || number.fract() != 0.0 {
        return Err(StoreError::Malformed(format!(
            "character {sqlid} difficulty must be an integer in 0..=3"
        )));
    }
    Ok(number as u8)
}

/// Read the appearance bytes from a record.
///
/// The reference stores them under `creation.appearance`, or as `opts_hex` (with a
/// matching `creation.options_hex`); the clean schema stores them directly as
/// `appearance`. All three shapes are accepted.
fn read_appearance(record: &[(String, Json)]) -> Result<Appearance, StoreError> {
    if let Some(creation) = find(record, "creation").and_then(Json::as_obj) {
        if let Some(appearance) = find(creation, "appearance") {
            return parse_appearance(appearance);
        }
        if let Some(hex) = get_str(creation, "options_hex") {
            return Ok(appearance_from_bytes(&decode_hex(hex)?));
        }
    }
    if let Some(appearance) = find(record, "appearance") {
        return parse_appearance(appearance);
    }
    if let Some(hex) = get_str(record, "opts_hex") {
        return Ok(appearance_from_bytes(&decode_hex(hex)?));
    }
    Ok(Appearance::default())
}

fn parse_appearance(value: &Json) -> Result<Appearance, StoreError> {
    let object = value
        .as_obj()
        .ok_or_else(|| StoreError::Malformed("appearance must be an object".to_string()))?;
    Ok(Appearance {
        skin: optional_u8(object, "skin")?,
        face: optional_u8(object, "face")?,
        face_feature: optional_u8(object, "face_feature")?,
        hair: optional_u8(object, "hair")?,
        hair_color: optional_u8(object, "hair_color")?,
    })
}

fn optional_u8(object: &[(String, Json)], key: &str) -> Result<Option<u8>, StoreError> {
    let Some(value) = find(object, key) else {
        return Ok(None);
    };
    let n = value
        .as_f64()
        .ok_or_else(|| StoreError::Malformed(format!("appearance.{key} must be an integer")))?;
    if n.fract() != 0.0 || !(0.0..=255.0).contains(&n) {
        return Err(StoreError::Malformed(format!(
            "appearance.{key} is out of byte range"
        )));
    }
    Ok(Some(n as u8))
}

fn appearance_from_bytes(bytes: &[u8]) -> Appearance {
    if bytes.len() < 5 {
        return Appearance::default();
    }
    Appearance {
        skin: Some(bytes[0]),
        face: Some(bytes[1]),
        face_feature: Some(bytes[2]),
        hair: Some(bytes[3]),
        hair_color: Some(bytes[4]),
    }
}

fn decode_hex(text: &str) -> Result<Vec<u8>, StoreError> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(StoreError::Malformed(
            "opts_hex has an odd number of digits".to_string(),
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_digit(bytes[i])?;
        let lo = hex_digit(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Result<u8, StoreError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(StoreError::Malformed(
            "opts_hex has a non-hex digit".to_string(),
        )),
    }
}

fn find<'a>(object: &'a [(String, Json)], key: &str) -> Option<&'a Json> {
    object.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn get_str<'a>(object: &'a [(String, Json)], key: &str) -> Option<&'a str> {
    find(object, key).and_then(Json::as_str)
}

// -- minimal JSON ---------------------------------------------------------
//
// The store schema is small and fixed, but reading the reference file means
// skipping values this module does not model (`created_ts`, `last_login`,
// `quests`, ...), so the reader is a small general JSON parser over the
// document and the extractors above pick out the fields they need.

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn as_obj(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(pairs) => Some(pairs),
            _ => None,
        }
    }

    fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    fn as_u32(&self) -> Option<u32> {
        let n = self.as_f64()?;
        if n.fract() == 0.0 && (0.0..=u32::MAX as f64).contains(&n) {
            Some(n as u32)
        } else {
            None
        }
    }
}

fn parse_json(text: &str) -> Result<Json, StoreError> {
    Parser {
        bytes: text.as_bytes(),
        pos: 0,
    }
    .parse()
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn parse(mut self) -> Result<Json, StoreError> {
        self.skip_ws();
        let value = self.value(0)?;
        self.skip_ws();
        if self.pos != self.bytes.len() {
            return self.fail("trailing data after JSON value");
        }
        Ok(value)
    }

    fn skip_ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn fail<T>(&self, message: impl Into<String>) -> Result<T, StoreError> {
        Err(StoreError::Malformed(message.into()))
    }

    fn value(&mut self, depth: usize) -> Result<Json, StoreError> {
        if depth > 128 {
            return self.fail("JSON nesting too deep");
        }
        self.skip_ws();
        let Some(&byte) = self.bytes.get(self.pos) else {
            return self.fail("unexpected end of JSON");
        };
        match byte {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => Ok(Json::Str(self.string()?)),
            b't' => {
                self.literal("true")?;
                Ok(Json::Bool(true))
            }
            b'f' => {
                self.literal("false")?;
                Ok(Json::Bool(false))
            }
            b'n' => {
                self.literal("null")?;
                Ok(Json::Null)
            }
            b'-' | b'0'..=b'9' => self.number(),
            other => self.fail(format!("unexpected byte 0x{other:02x} in JSON")),
        }
    }

    fn literal(&mut self, literal: &str) -> Result<(), StoreError> {
        if self.bytes[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            Ok(())
        } else {
            self.fail(format!("expected {literal:?}"))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, StoreError> {
        self.pos += 1;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(Json::Obj(pairs));
        }
        loop {
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b'"') {
                return self.fail("expected object key string");
            }
            let key = self.string()?;
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b':') {
                return self.fail("expected ':' after object key");
            }
            self.pos += 1;
            let value = self.value(depth + 1)?;
            pairs.push((key, value));
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return self.fail("expected ',' or '}' in object"),
            }
        }
        Ok(Json::Obj(pairs))
    }

    fn array(&mut self, depth: usize) -> Result<Json, StoreError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return self.fail("expected ',' or ']' in array"),
            }
        }
        Ok(Json::Arr(items))
    }

    fn string(&mut self) -> Result<String, StoreError> {
        self.pos += 1;
        let mut out = Vec::new();
        loop {
            let Some(&byte) = self.bytes.get(self.pos) else {
                return self.fail("unterminated string");
            };
            self.pos += 1;
            match byte {
                b'"' => break,
                b'\\' => self.escape(&mut out)?,
                0x00..=0x1F => return self.fail("unescaped control character in string"),
                other => out.push(other),
            }
        }
        String::from_utf8(out)
            .map_err(|_| StoreError::Malformed("string is not valid UTF-8".into()))
    }

    fn escape(&mut self, out: &mut Vec<u8>) -> Result<(), StoreError> {
        let Some(&byte) = self.bytes.get(self.pos) else {
            return self.fail("unterminated escape sequence");
        };
        self.pos += 1;
        match byte {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let scalar = self.unicode_scalar()?;
                let ch = char::from_u32(scalar)
                    .ok_or_else(|| StoreError::Malformed("invalid unicode escape".into()))?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            _ => return self.fail("invalid escape sequence"),
        }
        Ok(())
    }

    fn unicode_scalar(&mut self) -> Result<u32, StoreError> {
        let hi = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&hi) {
            if self.bytes.get(self.pos..self.pos + 2) != Some(b"\\u") {
                return self.fail("high surrogate without low surrogate");
            }
            self.pos += 2;
            let lo = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&lo) {
                return self.fail("invalid low surrogate");
            }
            Ok(0x10000 + ((hi - 0xD800) as u32) * 0x400 + (lo - 0xDC00) as u32)
        } else if (0xDC00..=0xDFFF).contains(&hi) {
            self.fail("unexpected low surrogate")
        } else {
            Ok(hi as u32)
        }
    }

    fn hex4(&mut self) -> Result<u16, StoreError> {
        if self.pos + 4 > self.bytes.len() {
            return self.fail("truncated \\u escape");
        }
        let mut value: u16 = 0;
        for _ in 0..4 {
            let digit = hex_digit(self.bytes[self.pos])?;
            value = value * 16 + digit as u16;
            self.pos += 1;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<Json, StoreError> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        if !matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
            return self.fail("number has no leading digit");
        }
        while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.bytes.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if !matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                return self.fail("number has no digits after '.'");
            }
            while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                return self.fail("number has no exponent digits");
            }
            while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| StoreError::Malformed(format!("invalid number {text:?}")))
    }
}

fn serialize(value: &Json) -> String {
    let mut out = String::new();
    write_json(value, 0, &mut out);
    out
}

fn write_json(value: &Json, indent: usize, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => {
            if n.fract() == 0.0 && n.is_finite() && n.abs() < 9.0e15 {
                out.push_str(&(*n as i64).to_string());
            } else {
                out.push_str(&n.to_string());
            }
        }
        Json::Str(s) => write_json_string(s, out),
        Json::Arr(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                push_indent(indent + 1, out);
                write_json(item, indent + 1, out);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(indent, out);
            out.push(']');
        }
        Json::Obj(pairs) => {
            if pairs.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (key, item)) in pairs.iter().enumerate() {
                push_indent(indent + 1, out);
                write_json_string(key, out);
                out.push_str(": ");
                write_json(item, indent + 1, out);
                if i + 1 < pairs.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(indent, out);
            out.push('}');
        }
    }
}

fn push_indent(level: usize, out: &mut String) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write_json_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trips_through_the_parser() {
        let value = parse_json(r#"{"a": [1, 2.5, true, null, "x\u0041"]}"#).unwrap();
        let object = value.as_obj().unwrap();
        let items = match find(object, "a").unwrap() {
            Json::Arr(items) => items,
            other => panic!("expected array, got {other:?}"),
        };
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].as_u32(), Some(1));
        assert_eq!(items[4].as_str(), Some("xA"));
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(matches!(
            parse_json("{ this is not json"),
            Err(StoreError::Malformed(_))
        ));
        assert!(matches!(
            parse_json("{\"a\": }"),
            Err(StoreError::Malformed(_))
        ));
        assert!(matches!(
            parse_json("{} trailing"),
            Err(StoreError::Malformed(_))
        ));
        assert!(matches!(
            parse_json("\"unterminated"),
            Err(StoreError::Malformed(_))
        ));
    }

    #[test]
    fn name_validation_matches_the_seed_rule() {
        assert!(validate_name("A").is_ok());
        assert!(validate_name("123456789").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("1234567890").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("tab\tchar").is_err());
        assert!(validate_name("caf\u{e9}").is_err());
    }
}
