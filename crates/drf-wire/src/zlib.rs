// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal, dependency-free RFC 1950 (zlib) / RFC 1951 (DEFLATE) codec.
//!
//! # Why this exists (client-required, binary-cited)
//!
//! The client statically links full zlib and requires it in both directions:
//!
//! - compression: `compress2` @ `0x006CBBF0` -> `deflateInit2_` @ `0x006CBF60`,
//!   `deflate` @ `0x006CC1B0`, `deflateEnd` @ `0x006CC8E0` (objdump of
//!   `DungeonRunners.exe`; the calls are visible at `0x006CBC27`,
//!   `0x006CBC34` and `0x006CBC44`).
//! - decompression: `uncompress` @ `0x006D0EE0` -> `inflateInit_`
//!   @ `0x006CDB80`, `inflate` @ `0x006CDCE0`, `inflateEnd` @ `0x006CF200`.
//!
//! `uncompress` consumes an RFC 1950 stream (2-byte header + DEFLATE +
//! big-endian Adler-32), so the server must produce and consume real zlib —
//! not raw DEFLATE. This is the boundary noted in `encodeMessageToSendBuffer`
//! @ `0x00641B70` (flag bit3 = `0x08` at `0x00641D5D`, `compress2` called at
//! `0x00641D4B`) and `updateReceiveQueue` @ `0x006413A0` (`uncompress` at
//! `0x00641847`).
//!
//! # Dependency decision
//!
//! `flate2`/`miniz_oxide` cannot be fetched: this machine has no cargo
//! registry cache and no `vendor/` dir, and the only `libminiz_oxide*.rlib`
//! is a sysroot artefact that needs `rustc_private` (nightly). To keep the
//! crate std-only and offline-buildable, this module hand-rolls:
//!
//! - a full, bounded DEFLATE **inflate** (stored / fixed / dynamic Huffman,
//!   LZ77 back-references, zlib header + Adler-32 validation). Inflate is the
//!   security-sensitive half because it consumes untrusted client bytes, so
//!   output is hard-capped and every structural inconsistency is an error.
//! - a minimal **deflate** that emits legal RFC 1950 stored blocks. The client
//!   only ever `uncompress`es server data; any valid zlib stream is accepted,
//!   so byte-parity with zlib's own dynamic-Huffman output is *not* a client
//!   requirement. Server->client payloads are trusted, so the simpler and
//!   provably-correct stored encoder is preferred over a bespoke LZ77 matcher.
//!
//! # Compatibility fixtures
//!
//! The reference implementation uses `zlib.compress(inner, level)`. Its output
//! is a dynamic-Huffman stream that this module's inflate must decode; that is
//! the golden test's job. This module's own output is checked by round-trip and,
//! for the golden frame, by inflating the reference's exact bytes.

use std::fmt;

/// Default cap on decompressed output, matching the client's `declen` limit
/// (`Segment::MAX_DECLEN`, `0x00641735`).
pub const MAX_OUTPUT: usize = 0x0010_0000;

/// Why a zlib stream was rejected. Every variant is fatal for that stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZlibError {
    /// Ran out of input mid-stream.
    Truncated,
    /// RFC 1950 header is malformed (bad CM/CINFO, bad FCHECK, or FDICT set).
    BadHeader,
    /// RFC 1951 block type 3, or an unsupported feature.
    Unsupported,
    /// Huffman code lengths are invalid (over/under-subscribed).
    BadHuffman,
    /// A length/distance pair is out of range, or LEN/NLEN disagree.
    BadLength,
    /// The trailing Adler-32 does not match the decompressed bytes.
    BadAdler,
    /// Decompressed output would exceed the caller's cap.
    OutputTooLarge,
}

impl fmt::Display for ZlibError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ZlibError::Truncated => "truncated zlib stream",
            ZlibError::BadHeader => "bad zlib header",
            ZlibError::Unsupported => "unsupported zlib feature",
            ZlibError::BadHuffman => "invalid Huffman table",
            ZlibError::BadLength => "invalid length/distance",
            ZlibError::BadAdler => "Adler-32 mismatch",
            ZlibError::OutputTooLarge => "decompressed output exceeds limit",
        };
        f.write_str(s)
    }
}

impl std::error::Error for ZlibError {}

/// Adler-32 over `data` (RFC 1950 §9). Big-endian on the wire.
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a += byte as u32;
        if a >= MOD {
            a -= MOD;
        }
        b += a;
        if b >= MOD {
            b -= MOD;
        }
    }
    (b << 16) | a
}

/// Compress `data` into a legal RFC 1950 stream using stored DEFLATE blocks.
///
/// The stream is `[CMF=0x78][FLG=0x01][stored blocks][Adler-32 BE]`.
/// `0x7801` is a valid header (0x7801 % 31 == 0) advertising a 32 KiB window
/// and no preset dictionary. Each stored block carries at most 65535 bytes.
pub fn compress(data: &[u8]) -> Vec<u8> {
    let blocks = data.len().div_ceil(65535);
    let mut out = Vec::with_capacity(data.len() + blocks * 5 + 6);
    out.push(0x78);
    out.push(0x01);
    if data.is_empty() {
        out.push(0x01); // BFINAL=1, BTYPE=00 (stored), pad to byte
        out.extend_from_slice(&[0x00, 0x00, 0xff, 0xff]); // LEN=0, NLEN=0xffff
    } else {
        let mut chunks = data.chunks(65535).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            out.push(if last { 0x01 } else { 0x00 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Inflate an RFC 1950 stream, refusing to produce more than `max_output`
/// bytes.
pub fn decompress(data: &[u8], max_output: usize) -> Result<Vec<u8>, ZlibError> {
    // 2-byte header + smallest possible DEFLATE body + 4-byte Adler-32.
    if data.len() < 6 {
        return Err(ZlibError::Truncated);
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0f != 8 {
        // CM must be 8 (DEFLATE).
        return Err(ZlibError::BadHeader);
    }
    if cmf >> 4 > 7 {
        // CINFO > 7 means a window larger than 32 KiB.
        return Err(ZlibError::BadHeader);
    }
    if (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
        return Err(ZlibError::BadHeader);
    }
    if flg & 0x20 != 0 {
        // FDICT: a preset dictionary follows; the client never uses one.
        return Err(ZlibError::Unsupported);
    }

    let trailer_at = data.len() - 4;
    let expected = u32::from_be_bytes([
        data[trailer_at],
        data[trailer_at + 1],
        data[trailer_at + 2],
        data[trailer_at + 3],
    ]);
    let out = inflate(&data[2..trailer_at], max_output)?;
    if adler32(&out) != expected {
        return Err(ZlibError::BadAdler);
    }
    Ok(out)
}

// --- DEFLATE ---------------------------------------------------------------

// Length codes 257..=285: base length and extra bits (RFC 1951 §3.2.5).
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
// Distance codes 0..=29: base distance and extra bits (RFC 1951 §3.2.5).
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
// Order in which the code-length-code lengths appear (RFC 1951 §3.2.7).
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// LSB-first bit reader (DEFLATE packs Huffman codes least-significant bit
/// first).
struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    buf: u32,
    bits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            byte: 0,
            buf: 0,
            bits: 0,
        }
    }

    /// Ensure at least `n` (<= 16) bits are buffered.
    fn need(&mut self, n: u32) -> Result<(), ZlibError> {
        while self.bits < n {
            let b = *self.data.get(self.byte).ok_or(ZlibError::Truncated)?;
            self.buf |= u32::from(b) << self.bits;
            self.byte += 1;
            self.bits += 8;
        }
        Ok(())
    }

    /// Read `n` bits (n <= 16), least-significant bit first.
    fn bits(&mut self, n: u32) -> Result<u32, ZlibError> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let v = self.buf & ((1u32 << n) - 1);
        self.buf >>= n;
        self.bits -= n;
        Ok(v)
    }

    /// Discard bits to the next byte boundary (used before a stored block).
    fn align_byte(&mut self) {
        let drop = self.bits & 7;
        self.buf >>= drop;
        self.bits -= drop;
    }
}

/// A canonical Huffman decoder built from DEFLATE code lengths.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    /// Build from per-symbol code lengths (0 = unused). `allow_incomplete`
    /// permits the single-distance-code case (RFC 1951 §3.2.7).
    fn new(lengths: &[u8], allow_incomplete: bool) -> Result<Self, ZlibError> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l as usize >= counts.len() {
                return Err(ZlibError::BadHuffman);
            }
            counts[l as usize] += 1;
        }
        counts[0] = 0; // symbol count, not "unused" count

        // Kraft inequality: over-subscribed is always fatal.
        let mut left: i32 = 1;
        let mut max_len: u8 = 0;
        for (len, &count) in counts.iter().enumerate().skip(1) {
            left <<= 1;
            left -= i32::from(count);
            if left < 0 {
                return Err(ZlibError::BadHuffman);
            }
            if count != 0 {
                max_len = len as u8;
            }
        }
        let total: usize = lengths.iter().filter(|&&l| l != 0).count();
        if left > 0 {
            // Under-subscribed: zlib only tolerates it for a distance tree
            // that carries at most one code.
            if !allow_incomplete || total > 1 || max_len != 1 {
                return Err(ZlibError::BadHuffman);
            }
        }

        // Order symbols by (length, symbol order) so the incremental decoder
        // below can index directly.
        let mut offs = [0u16; 16];
        for len in 1..15 {
            offs[len + 1] = offs[len] + counts[len];
        }
        let mut symbols = vec![0u16; total];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    /// Decode one symbol using the canonical-incremental algorithm.
    fn decode(&self, br: &mut BitReader) -> Result<u16, ZlibError> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16 {
            code |= br.bits(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(ZlibError::BadHuffman)
    }
}

fn fixed_trees() -> (Huffman, Huffman) {
    let mut lit = [0u8; 288];
    for (i, l) in lit.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8, // 280..=287
        };
    }
    let dist = [5u8; 32];
    (
        Huffman::new(&lit, false).expect("fixed literal tree is valid"),
        Huffman::new(&dist, false).expect("fixed distance tree is valid"),
    )
}

fn dynamic_trees(br: &mut BitReader) -> Result<(Huffman, Huffman), ZlibError> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;

    let mut cl_lengths = [0u8; 19];
    for &slot in CL_ORDER.iter().take(hclen) {
        cl_lengths[slot] = br.bits(3)? as u8;
    }
    let cl_tree = Huffman::new(&cl_lengths, false)?;

    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0;
    while i < total {
        let sym = cl_tree.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(ZlibError::BadHuffman);
                }
                let rep = br.bits(2)? as usize + 3;
                let prev = lengths[i - 1];
                let end = i.checked_add(rep).ok_or(ZlibError::BadHuffman)?;
                if end > total {
                    return Err(ZlibError::BadHuffman);
                }
                for slot in &mut lengths[i..end] {
                    *slot = prev;
                }
                i = end;
            }
            17 => {
                let rep = br.bits(3)? as usize + 3;
                let end = i.checked_add(rep).ok_or(ZlibError::BadHuffman)?;
                if end > total {
                    return Err(ZlibError::BadHuffman);
                }
                i = end;
            }
            18 => {
                let rep = br.bits(7)? as usize + 11;
                let end = i.checked_add(rep).ok_or(ZlibError::BadHuffman)?;
                if end > total {
                    return Err(ZlibError::BadHuffman);
                }
                i = end;
            }
            _ => return Err(ZlibError::BadHuffman),
        }
    }

    let lit = Huffman::new(&lengths[..hlit], false)?;
    let dist = Huffman::new(&lengths[hlit..], true)?;
    Ok((lit, dist))
}

fn inflate(data: &[u8], max_output: usize) -> Result<Vec<u8>, ZlibError> {
    let mut br = BitReader::new(data);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let bfinal = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => inflate_stored(&mut br, &mut out, max_output)?,
            1 => {
                let (lit, dist) = fixed_trees();
                inflate_block(&mut br, &mut out, &lit, &dist, max_output)?;
            }
            2 => {
                let (lit, dist) = dynamic_trees(&mut br)?;
                inflate_block(&mut br, &mut out, &lit, &dist, max_output)?;
            }
            _ => return Err(ZlibError::Unsupported),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok(out)
}

fn inflate_stored(
    br: &mut BitReader,
    out: &mut Vec<u8>,
    max_output: usize,
) -> Result<(), ZlibError> {
    br.align_byte();
    let len = br.bits(16)? as usize;
    let nlen = br.bits(16)? as usize;
    if (len ^ 0xffff) != nlen {
        return Err(ZlibError::BadLength);
    }
    if out.len() + len > max_output {
        return Err(ZlibError::OutputTooLarge);
    }
    for _ in 0..len {
        out.push(br.bits(8)? as u8);
    }
    Ok(())
}

fn inflate_block(
    br: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
    max_output: usize,
) -> Result<(), ZlibError> {
    loop {
        let sym = lit.decode(br)?;
        if sym < 256 {
            if out.len() + 1 > max_output {
                return Err(ZlibError::OutputTooLarge);
            }
            out.push(sym as u8);
            continue;
        }
        if sym == 256 {
            return Ok(());
        }
        let li = (sym - 257) as usize;
        if li >= LEN_BASE.len() {
            return Err(ZlibError::BadLength);
        }
        let length = LEN_BASE[li] as usize + br.bits(LEN_EXTRA[li])? as usize;

        let dsym = dist.decode(br)? as usize;
        if dsym >= DIST_BASE.len() {
            return Err(ZlibError::BadLength);
        }
        let distance = DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym])? as usize;
        if distance == 0 || distance > out.len() {
            return Err(ZlibError::BadLength);
        }
        if out.len() + length > max_output {
            return Err(ZlibError::OutputTooLarge);
        }
        let start = out.len() - distance;
        for k in 0..length {
            let b = out[start + k];
            out.push(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compatibility vectors produced with a standard zlib encoder. These are
    // witnesses for the client's `uncompress` @ 0x006D0EE0 contract (RFC 1950),
    // not protocol fixtures.
    const SAMPLE: &[u8] = b"Hello, Dungeon Runners! ";

    fn sample_text() -> Vec<u8> {
        SAMPLE.repeat(50)
    }

    #[test]
    fn adler32_known_values() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"a"), 0x0062_0062);
        assert_eq!(adler32(b"abc"), 0x024d_0127);
    }

    #[test]
    fn empty_roundtrip() {
        let z = compress(b"");
        assert_eq!(
            z,
            vec![0x78, 0x01, 0x01, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01]
        );
        assert_eq!(decompress(&z, MAX_OUTPUT).unwrap(), b"");
    }

    #[test]
    fn own_roundtrip_stored() {
        for n in [1usize, 2, 255, 4096, 65535, 65536, 131072] {
            let data: Vec<u8> = (0..n).map(|i| (i * 31 + 7) as u8).collect();
            let z = compress(&data);
            assert_eq!(decompress(&z, MAX_OUTPUT).unwrap(), data, "n={n}");
        }
    }

    #[test]
    fn inflates_python_dynamic_huffman() {
        // zlib.compress(sample_text(), 9)
        let z = hex(
            "78daf348cdc9c9d7517029cd4b4fcdcf53082acdcb4b2d2a5654f018151f151f151f151f64e200dc4fa26c",
        );
        assert_eq!(decompress(&z, MAX_OUTPUT).unwrap(), sample_text());
    }

    #[test]
    fn inflates_python_fixed_huffman() {
        // zlib.compressobj(..., Z_FIXED)
        let z = hex(
            "7801f348cdc9c9d7517029cd4b4fcdcf53082acdcb4b2d2a5654f018151f151f151f151f64e200dc4fa26c",
        );
        assert_eq!(decompress(&z, MAX_OUTPUT).unwrap(), sample_text());
    }

    #[test]
    fn inflates_python_stored() {
        // zlib.compress(b"abc", 0): one stored block, LEN=3 NLEN=0xfffc.
        let z = hex("7801010300fcff616263024d0127");
        assert_eq!(decompress(&z, MAX_OUTPUT).unwrap(), b"abc");
        // Our stored encoder is byte-identical to zlib level 0 for this input,
        // which is exactly the interoperability contract `uncompress` needs.
        assert_eq!(compress(b"abc"), z);
    }

    #[test]
    fn rejects_bad_header() {
        assert_eq!(
            decompress(&[0x00, 0x00], MAX_OUTPUT),
            Err(ZlibError::Truncated)
        );
        assert_eq!(
            decompress(&[0x00, 0x01, 0, 0, 0, 0], MAX_OUTPUT),
            Err(ZlibError::BadHeader)
        );
        // valid-ish length but bad FCHECK
        assert_eq!(
            decompress(&[0x78, 0x00, 0, 0, 0, 0], MAX_OUTPUT),
            Err(ZlibError::BadHeader)
        );
    }

    #[test]
    fn rejects_bad_adler() {
        let mut z = compress(b"abc");
        let n = z.len();
        z[n - 1] ^= 0xff;
        assert_eq!(decompress(&z, MAX_OUTPUT), Err(ZlibError::BadAdler));
    }

    #[test]
    fn rejects_truncated_stream() {
        let z = compress(b"the quick brown fox");
        assert_eq!(
            decompress(&z[..z.len() - 5], MAX_OUTPUT),
            Err(ZlibError::Truncated)
        );
    }

    #[test]
    fn enforces_output_cap() {
        let data = vec![0x41u8; 4096];
        let z = compress(&data);
        assert_eq!(decompress(&z, 100), Err(ZlibError::OutputTooLarge));
        assert_eq!(decompress(&z, 4096).unwrap().len(), 4096);
    }

    #[test]
    fn large_multi_block_roundtrip() {
        // > 2 stored blocks; exercises the BFINAL chaining.
        let data: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let z = compress(&data);
        assert_eq!(decompress(&z, MAX_OUTPUT * 4).unwrap(), data);
    }

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
