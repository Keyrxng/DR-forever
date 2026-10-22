// SPDX-License-Identifier: GPL-3.0-or-later

//! The shipped quest corpus, read from `game.pkg`.
//!
//! This is the content half of the quest loop: each quest instance's own
//! `Description` block (level window, class gate, prerequisite paths,
//! repeatable flag, on-accept generator) and its objective blocks, plus BOTH
//! shipped giver edges. The wire half lives in `drf-proto`; nothing here names
//! an opcode or writes a byte.
//!
//! The two giver edges are NOT the same relation and are kept apart, exactly
//! as the content model keeps them:
//!
//! * listed  -- the NPC class's own `* extends AvailableQuest { Quest = ... }`
//!   blocks. This is where a character can ACCEPT.
//! * turn-in -- each quest's own `Description.NPC` / `NPC2` / `NPC3`. This is
//!   where it can be COMPLETED.
//!
//! [`GiverDirection::Union`] is their union (Townston's directory);
//! [`GiverDirection::Listed`] is the accept edge alone (Dew Valley's).
//!
//! Every value is a `game.pkg` statement. A quest/file that cannot be read or
//! that has no `Description`/`NPC` is skipped, exactly as the reference skips
//! it; an unreadable zone world yields an empty roster rather than another
//! zone's.

use std::collections::{HashMap, HashSet};

use crate::corpus::{ContentError, Corpus};
use crate::gc::{self, Node};
use crate::hash::dfc_hash;
use crate::pkg::Archive;
use crate::zone::derive_zone_npcs;

/// The `Description` key that names the item a quest hands over on ACCEPT.
pub const ON_ACCEPT_ITEM_KEY: &str = "OnAcceptItemGenerator";
/// The `Description` bit that marks the on-accept item soulbound.
pub const ON_ACCEPT_ITEMS_ARE_SOULBOUND: &str = "OnAcceptItemsAreSoulBound";
/// The `Description` bit that marks the on-accept item unsellable.
pub const ON_ACCEPT_ITEMS_ARE_NO_SELL: &str = "OnAcceptItemsAreNoSell";

/// `Description.NPC`, `NPC2`, `NPC3`: a quest is turned in at every one.
const NPC_KEYS: [&str; 3] = ["NPC", "NPC2", "NPC3"];

/// The objective base classes the client registers, by the canonical leaf of
/// their `quests.base.*` target. Content is not consistent about `GoTo` vs
/// `Goto`; matching is case-insensitive and the canonical spelling is what the
/// rest of the server sees.
const OBJECTIVE_KINDS: [&str; 4] = [
    "KillObjective",
    "ItemObjective",
    "GoToObjective",
    "ActivateObjective",
];

fn canonical_objective_kind(leaf: &str) -> Option<&'static str> {
    let low = leaf.to_ascii_lowercase();
    OBJECTIVE_KINDS
        .iter()
        .copied()
        .find(|kind| kind.eq_ignore_ascii_case(&low))
}

/// The reference level window defaults.
const DEFAULT_MIN_LEVEL: u16 = 1;
const DEFAULT_MAX_LEVEL: u16 = 100;

/// One authored objective block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestObjective {
    /// The `quests.base.*` leaf (`KillObjective`, `ItemObjective`, ...).
    pub kind: String,
    /// The objective block's own `Name`.
    pub name: String,
    /// Its display `Label`.
    pub label: String,
    /// `RequiredQuantity` (or `RequiredKills`) -- the count's denominator.
    pub required: u32,
    /// `TargetZoneName`, when the objective states one.
    pub target_zone: Option<String>,
    /// `TargetEntityName`, the placement a `GoToObjective` must approach.
    pub target_entity: Option<String>,
    /// `Range`, the approach radius in world units (0 when unstated).
    pub range: u32,
    /// The item class an `ItemObjective` requires the player to HOLD
    /// (`ItemType`, else `Item`); `None` for every other kind.
    pub item: Option<String>,
    /// `RemoveOnFinalize = true`: consume `required` of `item` at turn-in
    /// (the wishing well's King's Coin).
    pub remove_on_finalize: bool,
    /// The `MonsterType{,2..21}` classes a `KillObjective` names. Empty for
    /// every other objective kind; an empty kill list is treated as "any
    /// monster" by the corpus, matching `QuestObjective::matches_monster`.
    pub monsters: Vec<String>,
    /// The `EntityType{,2..10}` classes an `ActivateObjective` names.
    pub entities: Vec<String>,
    /// Nested `ActivateDropTrigger` blocks: activating an entity of
    /// `entity` can drop `item` (the Hermit's Lockbox -> Hoop).
    pub activate_drops: Vec<ActivateDrop>,
    /// Nested `KillDropTrigger` blocks: killing a monster of `monster` can
    /// drop `item` (Q11_a5's Lucky Wolf's Paw).
    pub kill_drops: Vec<KillDrop>,
}

/// One authored `KillDropTrigger` under an objective.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillDrop {
    /// The `MonsterType` whose death runs the trigger.
    pub monster: String,
    /// The item class the trigger drops.
    pub item: String,
    /// Percent chance (`Chance`, default 100).
    pub chance: u32,
}

/// One authored `ActivateDropTrigger` under an objective.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivateDrop {
    /// The `EntityType` the player activates.
    pub entity: String,
    /// The item class that activation drops.
    pub item: String,
    /// Percent chance (`Chance`, default 100).
    pub chance: u32,
}

impl QuestObjective {
    /// The v666 objective text: `"<label>: <current> / <required>"`.
    ///
    /// The wire carries no `required` field (`messages.quest_objective`
    /// @8058); the denominator lives in this label text.
    pub fn wire_label(&self, current: u32) -> String {
        format!("{}: {} / {}", self.label, current, self.required)
    }

    /// Does this objective name the killed monster class? The comparison is
    /// case-insensitive, like the client's GC class lookup. An objective that
    /// names no MonsterType accepts any kill (the content's own "any" form).
    pub fn matches_monster(&self, classname: &str) -> bool {
        if self.kind != "KillObjective" {
            return false;
        }
        if self.monsters.is_empty() {
            return true;
        }
        self.monsters
            .iter()
            .any(|monster| monster.eq_ignore_ascii_case(classname))
    }

    /// Would activating `classname` count toward this `ActivateObjective`?
    pub fn matches_entity(&self, classname: &str) -> bool {
        if self.kind != "ActivateObjective" {
            return false;
        }
        if self.entities.is_empty() {
            return true;
        }
        self.entities
            .iter()
            .any(|entity| entity.eq_ignore_ascii_case(classname))
    }
}

/// One shipped quest, as its own `.gc` states it.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestDef {
    /// The dotted content path (`world.dungeon00.quest.Q11_a1`).
    pub fqn: String,
    /// djb2 of the FQN, case-folded. The wire quest id is `0x04` followed by
    /// this u32 little-endian (`quest_by_qid` takes the u32 alone).
    pub qid: u32,
    /// `MinLevel`, defaulting to 1.
    pub min_level: u16,
    /// `MaxLevel`, defaulting to 100.
    pub max_level: u16,
    /// `RequiredClass`, or `None` for every class.
    pub required_class: Option<String>,
    /// `RequiredQuest1` / `RequiredQuest2`, by content path.
    pub required_quests: Vec<String>,
    /// `Repeatable`: still an offer after it has been turned in.
    pub repeatable: bool,
    /// Extends `quests.base.QuestObsolete`: a retired quest body kept only so
    /// an existing holder can turn it in. It is never a live offer.
    pub obsolete: bool,
    /// `TokenReward`: King's Coins (`QuestItemPAL.Token`) granted on turn-in.
    pub token_reward: u32,
    /// `CashReward`: gold multiplier, inherited from the `quests.base.*` chain.
    pub cash_reward: f32,
    /// `GrantXPBuff`: the `QuestXPBonus` reward modifier.
    pub grant_xp_buff: bool,
    /// The objective blocks, in file order.
    pub objectives: Vec<QuestObjective>,
    /// `OnAcceptItemGenerator`, or `None`.
    pub on_accept_item: Option<String>,
    /// `OnAcceptItemsAreSoulBound`: the granted item's per-instance flag.
    pub on_accept_soulbound: bool,
    /// `OnAcceptItemsAreNoSell`: the granted item's per-instance flag.
    pub on_accept_no_sell: bool,
    /// The first `RewardItemGenerator`, kept for callers that only need the
    /// common one-generator case.
    pub reward_generator: Option<String>,
    /// `RewardItemGenerator{,2,3}`, in the order the body states them.
    pub reward_generators: Vec<String>,
    /// `RewardItemsAreSoulBound`: the per-instance flag on reward items.
    pub reward_soulbound: bool,
    /// `RewardItemsAreDropped`: spawn the reward as ground loot rather than
    /// adding it to the bag.
    pub reward_items_dropped: bool,
    /// `NumRewardItems`, defaulting to 1.
    pub num_reward_items: u32,
}

/// Which shipped giver edge a directory is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiverDirection {
    /// Both edges: the NPC's `AvailableQuest` list plus the quest's `NPC` keys.
    Union,
    /// The NPC's own `AvailableQuest` list only -- where a character ACCEPTs.
    Listed,
}

/// Every shipped quest, indexed by FQN and wire id, with both giver graphs.
#[derive(Clone)]
pub struct QuestCorpus {
    archive: Archive,
    quests: Vec<(QuestDef, Vec<String>)>,
    by_fqn_ci: HashMap<String, usize>,
    by_qid: HashMap<u32, usize>,
    givers: HashMap<String, Vec<usize>>,
    listed: HashMap<String, Vec<usize>>,
}

impl QuestCorpus {
    /// Read every quest instance and both giver edges from the archive.
    pub fn from_corpus(corpus: &mut Corpus) -> Result<QuestCorpus, ContentError> {
        let archive = corpus.archive().clone();
        let paths = gc_paths(&archive);

        let mut quests: Vec<(QuestDef, Vec<String>)> = Vec::new();
        let mut positions: HashMap<String, usize> = HashMap::new();
        for path in &paths {
            if !is_quest_path(path) {
                continue;
            }
            let Ok(tree) = corpus.tree(path) else {
                continue;
            };
            let Some((def, npcs)) = quest_from(path, &tree, corpus) else {
                continue;
            };
            if let Some(&idx) = positions.get(&def.fqn) {
                quests[idx] = (def, npcs);
            } else {
                positions.insert(def.fqn.clone(), quests.len());
                quests.push((def, npcs));
            }
        }

        let mut by_fqn_ci = HashMap::new();
        let mut by_qid = HashMap::new();
        for (idx, (def, _)) in quests.iter().enumerate() {
            by_fqn_ci.insert(def.fqn.to_ascii_lowercase(), idx);
            by_qid.insert(def.qid, idx);
        }

        // The canonical spellings are the town's placements, as the reference's
        // `_canonical_npc` reads `town_npcs()`. A name the town does not place
        // is left as written.
        let placed = town_placements(&archive);
        let mut listed: HashMap<String, HashSet<usize>> = HashMap::new();
        let mut turn_in: HashMap<String, HashSet<usize>> = HashMap::new();

        for (idx, (_, npcs)) in quests.iter().enumerate() {
            for npc in npcs {
                let name = canonical_npc(npc, &placed);
                turn_in.entry(name).or_default().insert(idx);
            }
        }

        for path in &paths {
            let low = path.to_ascii_lowercase();
            if !low.ends_with(".gc") || !low.contains("/npc/") {
                continue;
            }
            let Ok(tree) = corpus.tree(path) else {
                continue;
            };
            let npc_fqn = quest_fqn(path);
            let npc = canonical_npc(&npc_fqn, &placed);
            let mut nodes = Vec::new();
            gc::descendants(&tree, &mut nodes);
            for node in nodes {
                let Some(reference) = prop_last(node, "Quest") else {
                    continue;
                };
                let Some(&idx) = by_fqn_ci.get(&unquote(reference).to_ascii_lowercase()) else {
                    continue;
                };
                listed.entry(npc.clone()).or_default().insert(idx);
            }
        }

        let mut givers: HashMap<String, HashSet<usize>> = HashMap::new();
        for (npc, idxs) in &turn_in {
            givers.entry(npc.clone()).or_default().extend(idxs);
        }
        for (npc, idxs) in &listed {
            givers.entry(npc.clone()).or_default().extend(idxs);
        }

        let givers = sorted_table(givers, &quests);
        let listed = sorted_table(listed, &quests);

        Ok(QuestCorpus {
            archive,
            quests,
            by_fqn_ci,
            by_qid,
            givers,
            listed,
        })
    }

    /// The quest a wire qid names: the u32 djb2 of the FQN. The wire's 5-byte
    /// id is `0x04` followed by this u32 little-endian.
    pub fn quest_by_qid(&self, qid: u32) -> Option<&QuestDef> {
        self.by_qid.get(&qid).map(|&idx| &self.quests[idx].0)
    }

    /// The shipped quest an FQN names, case-insensitively.
    pub fn quest_by_fqn(&self, fqn: &str) -> Option<&QuestDef> {
        self.by_fqn_ci
            .get(&fqn.to_ascii_lowercase())
            .map(|&idx| &self.quests[idx].0)
    }

    /// The offer directory for `level`, restricted to the placed `npcs`.
    ///
    /// Gates, in the reference's order: (1) the level window, (2) `RequiredClass`
    /// (`None` on the quest means any class; a stated class needs a matching
    /// `class_base`), and, when `completed` is `Some`, (3) the
    /// `RequiredQuest1/2` chain (fail-closed on an unresolved path) and (4)
    /// [`Self::still_offers`] (a completed non-repeatable quest is withdrawn).
    ///
    /// Givers are sorted by classname and quests within a group by FQN, because
    /// the directory is a byte string downstream.
    pub fn offer_groups(
        &self,
        level: u16,
        npcs: &[String],
        class_base: Option<&str>,
        direction: GiverDirection,
        completed: Option<&HashSet<u32>>,
    ) -> Vec<(String, Vec<&QuestDef>)> {
        let table = match direction {
            GiverDirection::Union => &self.givers,
            GiverDirection::Listed => &self.listed,
        };
        // Content spellings differ between a quest giver's NPC file and the
        // placed entity (for example `QuestGiver_L01_C` vs
        // `questgiver_l01_c`), and GC lookups are case-insensitive. Match on
        // the folded name, then answer with the PLACED spelling so the
        // directory classname matches the entity the client holds.
        let placed: HashMap<String, &str> = npcs
            .iter()
            .map(|npc| (npc.to_ascii_lowercase(), npc.as_str()))
            .collect();
        let mut names: Vec<&String> = table.keys().collect();
        names.sort();

        let mut groups = Vec::new();
        for npc in names {
            let Some(placed_name) = placed.get(&npc.to_ascii_lowercase()) else {
                continue;
            };
            let mut keep = Vec::new();
            for &idx in &table[npc] {
                let quest = &self.quests[idx].0;
                if quest.obsolete {
                    continue;
                }
                if !(quest.min_level <= level && level <= quest.max_level) {
                    continue;
                }
                if !class_ok(quest, class_base) {
                    continue;
                }
                if let Some(done) = completed {
                    if !self.prereqs_met(quest, done) || !self.still_offers(quest, done) {
                        continue;
                    }
                }
                keep.push(quest);
            }
            if !keep.is_empty() {
                groups.push(((*placed_name).to_string(), keep));
            }
        }
        groups
    }

    /// The placed NPC classnames in `zone`, in world-file order.
    ///
    /// One world reader: this delegates to [`derive_zone_npcs`], the same
    /// `Entities`-block walk `zone_corpus.placement(zone).npcs()` performs --
    /// the world's placements minus its client-side-only classes. An
    /// unreadable world is an empty roster, never another zone's.
    pub fn zone_npcs(&self, zone: &str) -> Vec<String> {
        derive_zone_npcs(&self.archive, zone).unwrap_or_default()
    }

    /// Has this character already FINISHED this quest, unrepeatably?
    fn still_offers(&self, quest: &QuestDef, done: &HashSet<u32>) -> bool {
        !done.contains(&quest.qid) || quest.repeatable
    }

    /// Is every `RequiredQuest1/2` path resolved and in `done`?
    ///
    /// Fail closed: an unresolved path is an unsatisfiable gate, so the quest
    /// is withheld rather than advertised and refused on click.
    fn prereqs_met(&self, quest: &QuestDef, done: &HashSet<u32>) -> bool {
        for reference in &quest.required_quests {
            let Some(&idx) = self.by_fqn_ci.get(&reference.to_ascii_lowercase()) else {
                return false;
            };
            if !done.contains(&self.quests[idx].0.qid) {
                return false;
            }
        }
        true
    }
}

/// The `RequiredClass` gate. `None` on the quest means every class may take
/// it; a stated class needs a matching `class_base`.
fn class_ok(quest: &QuestDef, class_base: Option<&str>) -> bool {
    match &quest.required_class {
        None => true,
        Some(want) => class_base.is_some_and(|base| want.eq_ignore_ascii_case(base)),
    }
}

/// Sort each giver's quest indices by FQN, as the reference does at load time.
fn sorted_table(
    table: HashMap<String, HashSet<usize>>,
    quests: &[(QuestDef, Vec<String>)],
) -> HashMap<String, Vec<usize>> {
    table
        .into_iter()
        .map(|(npc, idxs)| {
            let mut idxs: Vec<usize> = idxs.into_iter().collect();
            idxs.sort_by(|&a, &b| quests[a].0.fqn.cmp(&quests[b].0.fqn));
            (npc, idxs)
        })
        .collect()
}

/// Every archive entry as `<name><suffix>`, normalized and de-duplicated the
/// way the reference's `PkgReader.entries()` is.
fn gc_paths(archive: &Archive) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for entry in archive.entries() {
        let path = format!("{}{}", entry.name.replace('\\', "/"), entry.suffix);
        if seen.insert(path.to_ascii_lowercase()) {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// `world/dungeon00/quest/Q11_a1.gc` is a quest instance.
fn is_quest_path(path: &str) -> bool {
    let low = path.to_ascii_lowercase();
    low.ends_with(".gc") && (low.contains("/quest/") || low.contains("/helpernoobosaur/"))
}

/// `world/dungeon00/quest/Q11_a1.gc` -> `world.dungeon00.quest.Q11_a1`.
fn quest_fqn(path: &str) -> String {
    let rel = path.replace('\\', "/");
    let rel = if rel.to_ascii_lowercase().ends_with(".gc") {
        &rel[..rel.len() - 3]
    } else {
        &rel
    };
    rel.replace('/', ".")
}

/// The `Description` properties a class inherits through its `extends` chain,
/// base-first, so the quest's own direct properties override them.
fn inherited_description_props(corpus: &mut Corpus, classname: &str) -> Vec<(String, String)> {
    let path = format!("{}.gc", classname.replace('.', "/"));
    let Ok(tree) = corpus.tree(&path) else {
        return Vec::new();
    };
    let Some(root) = tree.children.first() else {
        return Vec::new();
    };
    let mut props = root
        .extends
        .as_deref()
        .map(|base| inherited_description_props(corpus, base))
        .unwrap_or_default();
    if let Some(desc) = root
        .children
        .iter()
        .find(|child| child.name == "Description")
    {
        props.extend(desc.props.clone());
    }
    props
}

/// The last value of a property row in a merged description list.
fn merged_prop<'a>(props: &'a [(String, String)], name: &str) -> Option<&'a str> {
    props
        .iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Parse one quest file, or `None` when it is not a quest instance.
fn quest_from(path: &str, tree: &Node, corpus: &mut Corpus) -> Option<(QuestDef, Vec<String>)> {
    let root = tree.children.first()?;
    let desc = root
        .children
        .iter()
        .find(|child| child.name == "Description")?;
    // Quest reward defaults live on the base class's static `Description`
    // (`Quest`, `QuestCash1`, `QuestToken`, ...), not on the quest's own
    // block. Merge the chain so CashReward/TokenReward/GrantXPBuff are read
    // exactly where the client's class inheritance reads them.
    let mut merged_desc = root
        .extends
        .as_deref()
        .map(|base| inherited_description_props(corpus, base))
        .unwrap_or_default();
    merged_desc.extend(desc.props.clone());

    let npcs: Vec<String> = NPC_KEYS
        .iter()
        .filter_map(|key| merged_prop(&merged_desc, key))
        .map(unquote)
        .filter(|name| !name.is_empty())
        .collect();
    if npcs.is_empty() {
        return None;
    }

    let mut required_quests = Vec::new();
    for key in ["RequiredQuest1", "RequiredQuest2"] {
        if let Some(value) = merged_prop(&merged_desc, key) {
            let value = unquote(value);
            if !value.is_empty() {
                required_quests.push(value);
            }
        }
    }

    let mut nodes = Vec::new();
    gc::descendants(root, &mut nodes);
    let mut objectives = Vec::new();
    for node in nodes {
        let target = node.extends.as_deref().unwrap_or("");
        if !target.starts_with("quests.base.") {
            continue;
        }
        let kind = target.rsplit('.').next().unwrap_or("");
        let Some(kind) = canonical_objective_kind(kind) else {
            continue;
        };
        let required = prop_last(node, "RequiredQuantity")
            .or_else(|| prop_last(node, "RequiredKills"))
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(1);
        let target_zone = prop_last(node, "TargetZoneName")
            .map(unquote)
            .filter(|zone| !zone.is_empty());
        let target_entity = prop_last(node, "TargetEntityName")
            .map(unquote)
            .filter(|entity| !entity.is_empty());
        let range = prop_last(node, "Range")
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let item = prop_last(node, "ItemType")
            .or_else(|| prop_last(node, "Item"))
            .map(unquote)
            .filter(|item| !item.is_empty());
        let mut monsters = Vec::new();
        for suffix in [String::new()]
            .into_iter()
            .chain((2..=21).map(|n| n.to_string()))
        {
            if let Some(value) = prop_last(node, &format!("MonsterType{suffix}")) {
                let value = unquote(value);
                if !value.is_empty() {
                    monsters.push(value);
                }
            }
        }
        let mut entities = Vec::new();
        for suffix in [String::new()]
            .into_iter()
            .chain((2..=10).map(|n| n.to_string()))
        {
            if let Some(value) = prop_last(node, &format!("EntityType{suffix}")) {
                let value = unquote(value);
                if !value.is_empty() {
                    entities.push(value);
                }
            }
        }
        let mut trigger_nodes = Vec::new();
        gc::descendants(node, &mut trigger_nodes);
        let mut activate_drops = Vec::new();
        let mut kill_drops = Vec::new();
        for trigger in trigger_nodes {
            let Some(extends) = trigger.extends.as_deref() else {
                continue;
            };
            let trigger_kind = extends.rsplit('.').next().unwrap_or("");
            let chance = prop_last(trigger, "Chance")
                .and_then(|value| value.trim().parse::<u32>().ok())
                .unwrap_or(100);
            // Q03_a4's repeatable pelt triggers name the same item as
            // `ItemType`; Q03_a3 and Q11_a5 use `Item`. Both are authored
            // spellings of the trigger's drop, not different semantics.
            let drop_item = prop_last(trigger, "Item")
                .or_else(|| prop_last(trigger, "ItemType"))
                .map(unquote)
                .filter(|value| !value.is_empty());
            if trigger_kind.eq_ignore_ascii_case("ActivateDropTrigger") {
                let entity = prop_last(trigger, "EntityType")
                    .map(unquote)
                    .filter(|value| !value.is_empty());
                if let (Some(entity), Some(drop_item)) = (entity, drop_item) {
                    activate_drops.push(ActivateDrop {
                        entity,
                        item: drop_item,
                        chance,
                    });
                }
            } else if trigger_kind.eq_ignore_ascii_case("KillDropTrigger") {
                let monster = prop_last(trigger, "MonsterType")
                    .map(unquote)
                    .filter(|value| !value.is_empty());
                if let (Some(monster), Some(drop_item)) = (monster, drop_item) {
                    kill_drops.push(KillDrop {
                        monster,
                        item: drop_item,
                        chance,
                    });
                }
            }
        }
        objectives.push(QuestObjective {
            kind: kind.to_string(),
            name: prop_last(node, "Name").map(unquote).unwrap_or_default(),
            label: prop_last(node, "Label").map(unquote).unwrap_or_default(),
            required,
            target_zone,
            target_entity,
            range,
            item,
            // `ItemObjective::onFinalize` consumes when the body does not say
            // otherwise; the property is absent on three shipped objectives.
            remove_on_finalize: kind == "ItemObjective"
                && prop_last(node, "RemoveOnFinalize")
                    .map(|value| as_bool(Some(value)))
                    .unwrap_or(true),
            monsters,
            entities,
            activate_drops,
            kill_drops,
        });
    }

    let fqn = quest_fqn(path);
    let qid = dfc_hash(fqn.as_bytes());
    let obsolete =
        crate::zone::class_is_kind_of(corpus.archive(), &fqn, "quests.base.QuestObsolete")
            .unwrap_or(false);
    let def = QuestDef {
        qid,
        fqn,
        min_level: merged_prop(&merged_desc, "MinLevel")
            .and_then(parse_u16)
            .unwrap_or(DEFAULT_MIN_LEVEL),
        max_level: merged_prop(&merged_desc, "MaxLevel")
            .and_then(parse_u16)
            .unwrap_or(DEFAULT_MAX_LEVEL),
        required_class: merged_prop(&merged_desc, "RequiredClass")
            .map(unquote)
            .filter(|class| !class.is_empty()),
        required_quests,
        repeatable: as_bool(merged_prop(&merged_desc, "Repeatable")),
        obsolete,
        token_reward: merged_prop(&merged_desc, "TokenReward")
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(0),
        cash_reward: merged_prop(&merged_desc, "CashReward")
            .and_then(|value| value.trim().parse::<f32>().ok())
            .unwrap_or(0.5),
        grant_xp_buff: as_bool(merged_prop(&merged_desc, "GrantXPBuff")),
        objectives,
        on_accept_item: merged_prop(&merged_desc, ON_ACCEPT_ITEM_KEY)
            .map(unquote)
            .filter(|item| !item.is_empty()),
        on_accept_soulbound: as_bool(merged_prop(&merged_desc, ON_ACCEPT_ITEMS_ARE_SOULBOUND)),
        on_accept_no_sell: as_bool(merged_prop(&merged_desc, ON_ACCEPT_ITEMS_ARE_NO_SELL)),
        reward_generator: merged_prop(&merged_desc, "RewardItemGenerator")
            .map(unquote)
            .filter(|item| !item.is_empty()),
        reward_generators: [
            "RewardItemGenerator",
            "RewardItemGenerator2",
            "RewardItemGenerator3",
        ]
        .iter()
        .filter_map(|key| merged_prop(&merged_desc, key))
        .map(unquote)
        .filter(|item| !item.is_empty())
        .collect(),
        reward_soulbound: as_bool(merged_prop(&merged_desc, "RewardItemsAreSoulBound")),
        reward_items_dropped: as_bool(merged_prop(&merged_desc, "RewardItemsAreDropped")),
        num_reward_items: merged_prop(&merged_desc, "NumRewardItems")
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(1),
    };
    Some((def, npcs))
}

/// The town's placed NPC spellings, `lowercase -> exact`, for canonicalising
/// the quest-side `NPC` keys.
fn town_placements(archive: &Archive) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(names) = derive_zone_npcs(archive, "town") else {
        return out;
    };
    for name in names {
        if !name.starts_with("world.town.npc.") {
            continue;
        }
        out.insert(name.to_ascii_lowercase(), name);
    }
    out
}

fn canonical_npc(name: &str, placed: &HashMap<String, String>) -> String {
    placed
        .get(&name.to_ascii_lowercase())
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

/// The last value of a property row, as the reference's dict-backed parser keeps
/// the last duplicate a file states.
fn prop_last<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.props
        .iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn unquote(value: &str) -> String {
    let text = value.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text[1..text.len() - 1].to_string()
    } else {
        text.to_string()
    }
}

fn parse_u16(value: &str) -> Option<u16> {
    value.trim().parse::<u16>().ok()
}

fn as_bool(value: Option<&str>) -> bool {
    value.unwrap_or("").trim().eq_ignore_ascii_case("true")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_label_carries_the_required_count() {
        let objective = QuestObjective {
            kind: "ItemObjective".to_string(),
            name: "MainObjective1".to_string(),
            label: "Ticket #54,000,601".to_string(),
            required: 1,
            target_zone: None,
            target_entity: None,
            range: 0,
            item: None,
            remove_on_finalize: false,
            monsters: Vec::new(),
            entities: Vec::new(),
            activate_drops: Vec::new(),
            kill_drops: Vec::new(),
        };
        assert_eq!(objective.wire_label(0), "Ticket #54,000,601: 0 / 1");
        assert_eq!(objective.wire_label(3), "Ticket #54,000,601: 3 / 1");
    }

    #[test]
    fn class_gate_needs_a_matching_base_when_stated() {
        let mut quest = QuestDef {
            fqn: "a".to_string(),
            qid: 1,
            min_level: 1,
            max_level: 100,
            required_class: None,
            required_quests: Vec::new(),
            repeatable: false,
            obsolete: false,
            token_reward: 0,
            cash_reward: 0.5,
            grant_xp_buff: false,
            objectives: Vec::new(),
            on_accept_item: None,
            on_accept_soulbound: false,
            on_accept_no_sell: false,
            reward_generator: None,
            reward_generators: Vec::new(),
            reward_soulbound: false,
            reward_items_dropped: false,
            num_reward_items: 1,
        };
        assert!(class_ok(&quest, None));
        quest.required_class = Some("Fighter".to_string());
        assert!(!class_ok(&quest, None));
        assert!(class_ok(&quest, Some("fighter")));
        assert!(!class_ok(&quest, Some("Mage")));
    }

    #[test]
    fn quest_fqn_preserves_case_and_strips_the_extension() {
        assert_eq!(
            quest_fqn("world/dungeon00/quest/Q11_a1.gc"),
            "world.dungeon00.quest.Q11_a1"
        );
        assert_eq!(
            quest_fqn("world\\Dungeon04\\quest\\Q02_a4.GC"),
            "world.Dungeon04.quest.Q02_a4"
        );
    }

    #[test]
    fn only_instance_paths_are_quests() {
        assert!(is_quest_path("world/dungeon00/quest/Q11_a1.gc"));
        assert!(is_quest_path("quests/base/HelperNoobosaur/Q101_a1.gc"));
        assert!(!is_quest_path("world/dungeon00/npc/NCIPortrait.gc"));
        assert!(!is_quest_path("world/dungeon00/quest/Q11_a1.world"));
    }
}
