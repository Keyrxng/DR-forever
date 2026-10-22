// SPDX-License-Identifier: GPL-3.0-or-later

//! FQN resolution and `extends`-chain walking over the `*.gc` corpus.
//!
//! A dotted DFC name (`items.pal.1HMacePAL.Normal001`) resolves to a file and
//! a block name: the longest prefix that names a `.gc` entry is the file, and
//! the rest is the block. The reader is case-insensitive because the wire hash
//! case-folds and the shipped filenames do not always match the FQN's case.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use crate::gc::{self, Node};
use crate::pkg::{Archive, PkgError};

/// A content lookup that `game.pkg` does not settle.
#[derive(Debug)]
pub enum ContentError {
    Pkg(PkgError),
    NotFound(String),
    Malformed(String),
    Blocked(String),
}

impl std::fmt::Display for ContentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContentError::Pkg(e) => write!(f, "{e}"),
            ContentError::NotFound(m) => write!(f, "game.pkg does not define {m}"),
            ContentError::Malformed(m) => write!(f, "game.pkg is malformed: {m}"),
            ContentError::Blocked(m) => write!(f, "game.pkg does not settle {m}"),
        }
    }
}

impl std::error::Error for ContentError {}

impl From<PkgError> for ContentError {
    fn from(e: PkgError) -> Self {
        ContentError::Pkg(e)
    }
}

/// One `* extends` entry of a Starting* list block.
#[derive(Debug, Clone)]
pub struct ListEntry {
    pub fqn: String,
    pub id: Option<i64>,
    pub level: Option<i64>,
    pub mods: Vec<String>,
}

enum Lookup {
    Miss,
    Extends(Option<String>),
}

/// FQN resolution over a `game.pkg` archive.
pub struct Corpus {
    archive: Arc<Archive>,
    trees: HashMap<String, Rc<Node>>,
    observed_extends: HashMap<String, String>,
}

impl Corpus {
    pub fn new(archive: Archive) -> Corpus {
        Self::from_shared(Arc::new(archive))
    }

    /// Build over an archive already owned by the server. This keeps the
    /// multi-megabyte index shared instead of cloning it per content query.
    pub fn from_shared(archive: Arc<Archive>) -> Corpus {
        Corpus {
            archive,
            trees: HashMap::new(),
            observed_extends: HashMap::new(),
        }
    }

    /// The archive this corpus reads from.
    pub fn archive(&self) -> &Archive {
        self.archive.as_ref()
    }

    /// Parse (and cache) a `.gc` entry, recording every `extends` target seen.
    pub fn tree(&mut self, rel_path: &str) -> Result<Rc<Node>, ContentError> {
        if let Some(node) = self.trees.get(rel_path) {
            return Ok(Rc::clone(node));
        }
        let bytes = self
            .archive
            .read_path(rel_path)?
            .ok_or_else(|| ContentError::NotFound(rel_path.to_string()))?;
        let text: String = bytes.iter().map(|&b| b as char).collect();
        let root = Rc::new(gc::parse(&text));
        let mut stack: Vec<&Node> = vec![&root];
        while let Some(node) = stack.pop() {
            if let Some(target) = &node.extends {
                self.observed_extends
                    .entry(target.clone())
                    .or_insert_with(|| rel_path.to_string());
            }
            for child in &node.children {
                stack.push(child);
            }
        }
        self.trees.insert(rel_path.to_string(), Rc::clone(&root));
        Ok(root)
    }

    fn candidates(fqn: &str) -> Vec<(String, String)> {
        let parts: Vec<&str> = fqn.split('.').collect();
        let mut out = Vec::new();
        for split in (1..=parts.len()).rev() {
            let rel = format!("{}.gc", parts[..split].join("/"));
            let first = parts[split..].join(".");
            out.push((rel, first));
        }
        out
    }

    fn lookup(&mut self, fqn: &str) -> Result<Lookup, ContentError> {
        for (rel, first) in Self::candidates(fqn) {
            let Ok(tree) = self.tree(&rel) else {
                continue;
            };
            if first.is_empty() {
                return Ok(match tree.children.first() {
                    Some(child) => Lookup::Extends(child.extends.clone()),
                    None => Lookup::Miss,
                });
            }
            for child in &tree.children {
                if child.name == first {
                    return Ok(Lookup::Extends(child.extends.clone()));
                }
            }
            for child in descendants_of(&tree) {
                if child.name == first {
                    return Ok(Lookup::Extends(child.extends.clone()));
                }
            }
            return Ok(Lookup::Miss);
        }
        Ok(Lookup::Miss)
    }

    /// The DFC base class an FQN resolves to (e.g. `...Normal001` -> `Armor`).
    pub fn dfc_root(&mut self, fqn: &str) -> Result<String, ContentError> {
        self.dfc_root_at(fqn, 0)
    }

    fn dfc_root_at(&mut self, fqn: &str, depth: u32) -> Result<String, ContentError> {
        if depth > 32 {
            return Err(ContentError::Malformed(format!(
                "extends chain too deep at {fqn}"
            )));
        }
        match self.lookup(fqn)? {
            Lookup::Miss => {
                if self.observed_extends.contains_key(fqn) {
                    Ok(fqn.to_string())
                } else {
                    Err(ContentError::NotFound(format!(
                        "a .gc block or `extends` target named {fqn:?}"
                    )))
                }
            }
            Lookup::Extends(None) => Ok(fqn.to_string()),
            Lookup::Extends(Some(next)) => self.dfc_root_at(&next, depth + 1),
        }
    }

    /// Direct `* extends` children of a list block, in file order.
    pub fn list_entries(
        &mut self,
        fqn: &str,
        with_nested: bool,
    ) -> Result<Vec<ListEntry>, ContentError> {
        let holder = self.lookup_holder(fqn, false)?;
        let mut out = Vec::new();
        for child in &holder.children {
            let Some(target) = &child.extends else {
                continue;
            };
            let id = child.prop("ID").and_then(|v| v.trim().parse::<i64>().ok());
            let level = child
                .prop("Level")
                .and_then(|v| v.trim().parse::<i64>().ok());
            let mods = if with_nested {
                child
                    .children
                    .iter()
                    .filter(|n| n.extends.is_some() && gc::is_modifier(n))
                    .map(|n| n.extends.clone().unwrap())
                    .collect()
            } else {
                Vec::new()
            };
            out.push(ListEntry {
                fqn: target.clone(),
                id,
                level,
                mods,
            });
        }
        Ok(out)
    }

    /// Every `ItemModifier` child a DFC class chain instantiates, in order,
    /// paired with the terminal class the walk stopped at.
    pub fn authored_modifiers(
        &mut self,
        fqn: &str,
    ) -> Result<(Vec<(String, String)>, String), ContentError> {
        let mut seen = HashSet::new();
        self.authored_modifiers_at(fqn, 0, &mut seen)
    }

    fn authored_modifiers_at(
        &mut self,
        fqn: &str,
        depth: u32,
        seen: &mut HashSet<String>,
    ) -> Result<(Vec<(String, String)>, String), ContentError> {
        if depth > 24 || seen.contains(&fqn.to_lowercase()) {
            return Ok((Vec::new(), fqn.to_string()));
        }
        seen.insert(fqn.to_lowercase());
        let holder = match self.lookup_holder(fqn, true) {
            Ok(node) => node,
            Err(_) => return Ok((Vec::new(), fqn.to_string())),
        };
        let mut mods: Vec<(String, String)> = holder
            .children
            .iter()
            .filter(|c| gc::is_modifier(c))
            .map(|c| (c.name.clone(), c.extends.clone().unwrap_or_default()))
            .collect();
        let (rest, terminal) = match &holder.extends {
            Some(parent) => self.authored_modifiers_at(parent, depth + 1, seen)?,
            None => (Vec::new(), fqn.to_string()),
        };
        mods.extend(rest);
        Ok((mods, terminal))
    }

    /// The number of `ItemModifier`-derived children a DFC class chain
    /// declares: the status bytes `Item::readData` reads before the rolled
    /// modifier child list.
    ///
    /// `Item::readData` @`0x581710` hands the item to
    /// `GCObject::ReadChildData<ItemModifier>` @`0x583920`, whose first loop
    /// reads one status byte for every instantiated `ItemModifier` child of the
    /// item's class before it reads the rolled-modifier count
    /// (`ItemAttributeModifier::readData` @`0x588AE0` reads at least its flags
    /// byte). A class with N such children therefore needs N status bytes on the
    /// wire. A child of a non-`ItemModifier` type — the potion's
    /// `AttributeModifier` effect — consumes none.
    ///
    /// Only children the DFC class chain actually instantiates count here.
    /// A weapon's native `MeleeWeapon`/`RangedWeapon` readState (4/5 bytes) is
    /// appended only to a Manipulators entry, never to `Item::readData`; the
    /// old +1 for weapons therefore shifted the rolled-modifier count and
    /// desynced every generated weapon drop.
    pub fn item_readdata_slots(&mut self, fqn: &str) -> Result<usize, ContentError> {
        if self.lookup_holder(fqn, true).is_err() {
            return Err(ContentError::NotFound(format!("a .gc class named {fqn:?}")));
        }
        let mut seen = HashSet::new();
        let (slots, _declared_attr, _root) = self.item_readdata_slots_at(fqn, 0, &mut seen)?;
        // Only children the DFC class chain actually instantiates count here.
        // A weapon's Manipulators readState (4/5 bytes) is not part of
        // Item::readData. The starting staff's Equipment record (no status
        // byte) is accepted on join, so melee does not get an invented slot.
        Ok(slots)
    }

    fn item_readdata_slots_at(
        &mut self,
        fqn: &str,
        depth: u32,
        seen: &mut HashSet<String>,
    ) -> Result<(usize, bool, String), ContentError> {
        if depth > 24 || !seen.insert(fqn.to_lowercase()) {
            return Ok((0, false, fqn.to_string()));
        }
        // `block_chain` resolves a nested class by its leaf (the same way the
        // client's registry does); `lookup_holder` can stop at the file root
        // for prebuilt classes such as `1HStaffPAL.MythicPreBuilt004`, hiding
        // the `Mod1..Mod5` children. That made the wire record omit their
        // status bytes, so the client read the rolled-modifier count from the
        // middle of the record (`ReadChildData` at index 65502+) and latched
        // CEM code 2.
        let Ok(holder) = self.block_chain(fqn) else {
            return Ok((0, false, fqn.to_string()));
        };
        let mut slots = 0;
        let mut declared_attr = false;
        for child in &holder.children {
            let Some(target) = &child.extends else {
                continue;
            };
            if let Some(item_attr) = self.item_modifier_kind(target, 0) {
                slots += 1;
                declared_attr |= item_attr;
            }
        }
        let (inherited, inherited_attr, root) = match &holder.extends {
            Some(parent) => self.item_readdata_slots_at(parent, depth + 1, seen)?,
            None => (0, false, fqn.to_string()),
        };
        Ok((slots + inherited, declared_attr || inherited_attr, root))
    }

    /// Whether `target`'s `extends` chain terminates at a native item-modifier
    /// class. `Some(true)` when the terminal is `ItemAttributeModifier`,
    /// `Some(false)` for another `ItemModifier` subtype, `None` otherwise.
    fn item_modifier_kind(&mut self, target: &str, depth: u32) -> Option<bool> {
        if depth > 24 {
            return None;
        }
        // Resolve nested/aliased modpal classes through the same class-chain
        // reader as the item itself. `lookup_holder_extends` can miss a
        // prebuilt modifier's nested base, which made the prebuilt item look
        // like it had no status bytes.
        if let Ok(node) = self.block_chain(target) {
            if let Some(parent) = node.extends.clone() {
                return self.item_modifier_kind(&parent, depth + 1);
            }
        }
        let short = target.rsplit('.').next().unwrap_or(target);
        match short.to_ascii_lowercase().as_str() {
            "itemattributemodifier" => Some(true),
            "itemmodifier" | "scalemod" => Some(false),
            _ => None,
        }
    }

    /// The client-visible canonical spelling of a class name, derived from the
    /// archive entry and block that actually resolved it. The content has
    /// aliases such as `1HStaff1Pal.1HStaff1-1` whose file/root is
    /// `1HStaff1PAL`; the client's GC registry is case-sensitive, so records
    /// must carry the canonical spelling.
    pub fn canonical_class_name(&self, fqn: &str) -> Option<String> {
        let parts: Vec<&str> = fqn.split('.').collect();
        for split in (1..=parts.len()).rev() {
            let rel = parts[..split].join("/");
            let Some(entry) = self
                .archive
                .entries()
                .iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(&rel))
            else {
                continue;
            };
            let base = entry.name.replace('/', ".");
            if split == parts.len() {
                return Some(base);
            }
            let rest = parts[split..].join(".");
            return Some(format!("{base}.{rest}"));
        }
        None
    }

    /// The `Node` chain a dotted FQN names, walking the parts case-insensitively.
    pub fn block_chain(&mut self, fqn: &str) -> Result<Rc<Node>, ContentError> {
        let parts: Vec<&str> = fqn.split('.').collect();
        for split in (1..=parts.len()).rev() {
            let rel = format!("{}.gc", parts[..split].join("/"));
            let Ok(tree) = self.tree(&rel) else {
                continue;
            };
            let Some(root) = tree.children.first() else {
                continue;
            };
            let mut node = Rc::new(root.clone());
            for name in &parts[split..] {
                let found = node
                    .children
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(name))
                    .or_else(|| {
                        descendants_of(&node)
                            .into_iter()
                            .find(|c| c.name.eq_ignore_ascii_case(name))
                    })
                    .map(|c| Rc::new(c.clone()));
                match found {
                    Some(next) => node = next,
                    None => {
                        return Err(ContentError::NotFound(format!(
                            "block {name:?} under {fqn:?} in {rel}"
                        )))
                    }
                }
            }
            return Ok(node);
        }
        Err(ContentError::NotFound(fqn.to_string()))
    }

    /// The `extends` target of the block an FQN names, or `None` if the FQN
    /// names no block. Used to check that a class file reaches its own base.
    pub fn lookup_holder_extends(&mut self, fqn: &str) -> Option<String> {
        self.lookup_holder(fqn, false)
            .ok()
            .and_then(|n| n.extends.clone())
    }

    fn lookup_holder(&mut self, fqn: &str, deep: bool) -> Result<Rc<Node>, ContentError> {
        for (rel, first) in Self::candidates(fqn) {
            let Ok(tree) = self.tree(&rel) else {
                continue;
            };
            if first.is_empty() {
                if tree.children.len() == 1 {
                    return Ok(Rc::new(tree.children[0].clone()));
                }
                return Err(ContentError::Malformed(format!(
                    "{rel} is not a single-class file"
                )));
            }
            for child in &tree.children {
                if child.name.eq_ignore_ascii_case(&first) {
                    return Ok(Rc::new(child.clone()));
                }
            }
            if deep {
                for child in descendants_of(&tree) {
                    if child.name.eq_ignore_ascii_case(&first) {
                        return Ok(Rc::new(child.clone()));
                    }
                }
            }
            return Err(ContentError::NotFound(format!("block {first:?} in {rel}")));
        }
        Err(ContentError::NotFound(fqn.to_string()))
    }

    /// Value of a `Name = ...;` row, looking one nested block down.
    pub fn prop(&mut self, fqn: &str, name: &str) -> Result<Option<String>, ContentError> {
        for (rel, first) in Self::candidates(fqn) {
            let Ok(tree) = self.tree(&rel) else {
                continue;
            };
            for child in &tree.children {
                if !first.is_empty() && !child.name.eq_ignore_ascii_case(&first) {
                    continue;
                }
                if let Some(v) = child.prop(name) {
                    return Ok(Some(v.to_string()));
                }
                for sub in &child.children {
                    if let Some(v) = sub.prop(name) {
                        return Ok(Some(v.to_string()));
                    }
                }
            }
            return Ok(None);
        }
        Ok(None)
    }
}

fn descendants_of(node: &Rc<Node>) -> Vec<&Node> {
    let mut out = Vec::new();
    let mut stack: Vec<&Node> = node.children.iter().rev().collect();
    while let Some(child) = stack.pop() {
        for grand in child.children.iter().rev() {
            stack.push(grand);
        }
        out.push(child);
    }
    out
}
