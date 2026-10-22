// SPDX-License-Identifier: GPL-3.0-or-later

//! PKI/PKG archive reader (`game.pki` index + `game.pkg` blob).
//!
//! The index is a 24-byte header (`<II16s`: magic `3`, sub-version, a 16-byte
//! GUID that must equal the package's first 16 bytes), then a zlib stream
//! holding a 21-entry directory count table, one 44-byte record per entry, a
//! `u32` string-table length, and the NUL-separated name table. Each entry's
//! kind (and therefore its suffix) is the directory it falls in; a zipped
//! entry's `comp_size` bytes at `data_off` inflate to `decomp_size`.
//!
//! `game.pkg` is hundreds of megabytes, so reads seek into the file rather
//! than mapping it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The archive format version this reader accepts (`MAGIC_V666`).
pub const MAGIC_V666: u32 = 3;

const HEADER_LEN: usize = 24;
const DIR_KINDS: usize = 21;

/// Suffixes indexed by directory kind, in the index's own directory order.
pub const SUFFIXES: [&str; 21] = [
    ".tga", ".bmp", ".dds", ".ttf", ".cobj", ".d3d", ".dre", ".wav", ".ogg", ".win", ".msc",
    ".mig", ".dict", ".gc", ".tile", ".world", ".zone", ".dat", ".fx", ".cfg", ".txt",
];

/// A malformed archive, a bad read, or a decompression failure.
#[derive(Debug)]
pub enum PkgError {
    Io(std::io::Error),
    Format(String),
    Zlib(String),
}

impl std::fmt::Display for PkgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PkgError::Io(e) => write!(f, "pkg io: {e}"),
            PkgError::Format(m) => write!(f, "pkg format: {m}"),
            PkgError::Zlib(m) => write!(f, "pkg zlib: {m}"),
        }
    }
}

impl std::error::Error for PkgError {}

impl From<std::io::Error> for PkgError {
    fn from(e: std::io::Error) -> Self {
        PkgError::Io(e)
    }
}

/// One indexed entry.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub kind: usize,
    pub suffix: &'static str,
    pub data_off: u64,
    pub comp_size: u32,
    pub decomp_size: u32,
    pub zipped: bool,
}

/// The index over a `game.pki` / `game.pkg` pair.
#[derive(Clone)]
pub struct Archive {
    pkg_path: PathBuf,
    entries: Vec<Entry>,
    by_name: HashMap<String, usize>,
}

impl Archive {
    /// Open the index and verify it belongs to the package.
    pub fn open(pki_path: &Path, pkg_path: &Path) -> Result<Archive, PkgError> {
        let raw = std::fs::read(pki_path)?;
        if raw.len() < HEADER_LEN {
            return Err(PkgError::Format("pki too small for header".into()));
        }
        let magic = u32::from_le_bytes(raw[0..4].try_into().unwrap());
        if magic != MAGIC_V666 {
            return Err(PkgError::Format(format!(
                "bad magic {magic}, want {MAGIC_V666}"
            )));
        }
        let guid = &raw[8..24];
        let mut head = [0u8; 16];
        File::open(pkg_path)?.read_exact(&mut head)?;
        if head != guid {
            return Err(PkgError::Format("guid mismatch pki vs pkg".into()));
        }
        let inflated = inflate_index(&raw[HEADER_LEN..])?;
        let entries = parse_index(&inflated)?;
        let mut by_name = HashMap::with_capacity(entries.len() * 2);
        // Archive names carry no extension; the kind's suffix supplies it.
        // Both the suffixed and the bare name resolve, the suffixed first.
        for (idx, entry) in entries.iter().enumerate() {
            let base = normalize(&entry.name);
            by_name
                .entry(format!("{base}{}", entry.suffix))
                .or_insert(idx);
            by_name.entry(base).or_insert(idx);
        }
        Ok(Archive {
            pkg_path: pkg_path.to_path_buf(),
            entries,
            by_name,
        })
    }

    /// Every entry, in archive order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Look an entry up by its (case- and slash-insensitive) path.
    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.by_name
            .get(&normalize(name))
            .map(|&idx| &self.entries[idx])
    }

    /// Read and decompress one entry.
    pub fn read(&self, entry: &Entry) -> Result<Vec<u8>, PkgError> {
        let want = if entry.zipped {
            entry.comp_size
        } else {
            entry.decomp_size
        } as usize;
        let mut file = File::open(&self.pkg_path)?;
        file.seek(SeekFrom::Start(entry.data_off))?;
        let mut data = vec![0u8; want];
        file.read_exact(&mut data)?;
        if !entry.zipped {
            return Ok(data);
        }
        let out = drf_wire::zlib::decompress(&data, entry.decomp_size as usize + 1)
            .map_err(|e| PkgError::Zlib(format!("{e:?}")))?;
        if out.len() != entry.decomp_size as usize {
            return Err(PkgError::Format(format!(
                "entry {} inflated to {} bytes, index says {}",
                entry.name,
                out.len(),
                entry.decomp_size
            )));
        }
        Ok(out)
    }

    /// Read one entry by path.
    pub fn read_path(&self, name: &str) -> Result<Option<Vec<u8>>, PkgError> {
        match self.find(name) {
            Some(entry) => self.read(entry).map(Some),
            None => Ok(None),
        }
    }
}

fn normalize(name: &str) -> String {
    name.replace('\\', "/").to_lowercase()
}

fn inflate_index(data: &[u8]) -> Result<Vec<u8>, PkgError> {
    // The index is small (a few MB at most); the cap is generous but bounded.
    const MAX_INDEX: usize = 64 << 20;
    drf_wire::zlib::decompress(data, MAX_INDEX).map_err(|e| PkgError::Zlib(format!("{e:?}")))
}

fn parse_index(inflated: &[u8]) -> Result<Vec<Entry>, PkgError> {
    let mut pos = 0usize;
    let mut counts = [0u32; DIR_KINDS];
    for slot in counts.iter_mut() {
        *slot = read_u32(inflated, &mut pos)?;
    }
    let total: usize = counts.iter().map(|&c| c as usize).sum();
    if total == 0 {
        return Err(PkgError::Format("no entries".into()));
    }
    let mut bounds = Vec::with_capacity(DIR_KINDS);
    let mut lo = 0u32;
    for &c in counts.iter() {
        bounds.push((lo, lo + c));
        lo += c;
    }

    let mut name_offsets = Vec::with_capacity(total);
    let mut entries = Vec::with_capacity(total);
    for gid in 0..total {
        let name_off = read_u32(inflated, &mut pos)?;
        let _checksum = read_u32(inflated, &mut pos)?;
        let comp_size = read_u32(inflated, &mut pos)?;
        let data_off = read_u32(inflated, &mut pos)?;
        let decomp_size = read_u32(inflated, &mut pos)?;
        let zipped_byte = read_u8(inflated, &mut pos)?;
        pos += 3; // reserved
        pos += 20; // five trailing u32s
        let kind = bounds
            .iter()
            .position(|&(lo, hi)| lo as usize <= gid && gid < hi as usize)
            .unwrap_or(DIR_KINDS - 1);
        name_offsets.push(name_off);
        entries.push(Entry {
            name: String::new(),
            kind,
            suffix: SUFFIXES.get(kind).copied().unwrap_or(""),
            data_off: data_off as u64,
            comp_size,
            decomp_size,
            zipped: zipped_byte > 0,
        });
    }

    let tab_size = read_u32(inflated, &mut pos)? as usize;
    if pos + tab_size > inflated.len() {
        return Err(PkgError::Format("string table truncated".into()));
    }
    let table = &inflated[pos..pos + tab_size];
    pos += tab_size;
    if pos != inflated.len() {
        return Err(PkgError::Format(format!(
            "trailing bytes after string table: {}",
            inflated.len() - pos
        )));
    }

    let mut off2name: HashMap<usize, String> = HashMap::with_capacity(total);
    let mut off = 0usize;
    for chunk in table.split(|&b| b == 0) {
        if off == table.len() {
            break;
        }
        off2name.insert(off, String::from_utf8_lossy(chunk).into_owned());
        off += chunk.len() + 1;
    }
    if off2name.len() != total {
        return Err(PkgError::Format(format!(
            "string count {} != entry count {}",
            off2name.len(),
            total
        )));
    }

    for (entry, &name_off) in entries.iter_mut().zip(name_offsets.iter()) {
        entry.name = off2name
            .get(&(name_off as usize))
            .cloned()
            .ok_or_else(|| PkgError::Format(format!("bad name_off {name_off}")))?;
    }
    Ok(entries)
}

fn read_u32(data: &[u8], pos: &mut usize) -> Result<u32, PkgError> {
    if *pos + 4 > data.len() {
        return Err(PkgError::Format("short read".into()));
    }
    let v = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    Ok(v)
}

fn read_u8(data: &[u8], pos: &mut usize) -> Result<u8, PkgError> {
    if *pos >= data.len() {
        return Err(PkgError::Format("short read".into()));
    }
    let v = data[*pos];
    *pos += 1;
    Ok(v)
}
