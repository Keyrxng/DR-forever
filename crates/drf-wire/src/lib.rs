// SPDX-License-Identifier: GPL-3.0-or-later

//! drf-wire: Dungeon Runners transport segment framing (std-only).
//!
//! Every layout here was independently re-derived from the shipped client
//! binary (`DungeonRunners.exe`, PE32, ImageBase `0x400000`) by static
//! analysis. No game assets or proprietary source code are included in this
//! repository.
//!
//! # One segment format, both directions
//!
//! There is **no s2c format distinct from c2s**. The same encoder and the same
//! decoder serve both directions:
//!
//! - producer: `DFCSocketChannel::encodeMessageToSendBuffer` @ `0x00641B70`
//! - consumer: `DFCSocketChannel::updateReceiveQueue`     @ `0x006413A0`
//!
//! The first byte is a **flags** byte, not a direction tag. `0x0A` and `0x0E`
//! are simply the two flag combinations the client happens to emit
//! (`0x0A = 0x02|0x08`, `0x0E = 0x02|0x04|0x08`); the old `S2C_SEG`/`C2S_SEG`
//! constants and the `C2S_CONST = [b4 b3 b2 01]` "constant" were mis-slices of
//! a real segment header plus a real `MessageAddress`. See
//! This document is the protocol reference for the Rust implementation.
//!
//! # Segment layout
//!
//! ```text
//! offset  size  field
//! 0       1     flags   (bit0 invalid/kill, bit1 addr, bit2 frag, bit3 zlib)
//! 1       3     peer    (per-session token)
//! 4       3     size24 LE  = byte count of `body` (bytes after this header)
//! 7       1     reserved
//! 8       size  body:
//!                 [addr: 1B kind + u16le id]                    if flag bit1
//!                 [frag-count: u16le; count*(1B + u16le)]        if flag bit2
//!                 zlib: [u32le declen][zlib stream bytes]        if flag bit3
//!                 raw:  [data]                                   if flag bit3 clear
//! ```
//!
//! # The `peer` (header bytes 1..=3)
//!
//! `peer` occupies the same three header bytes the client's consumer never
//! reads: `updateReceiveQueue` @ `0x006413A0` tests only flag bit0
//! (`0x0064144A`) and masks `size24` at offset 4 (`0x0064145D`), so bytes 1..3
//! are opaque to the client parser. A per-session 3-byte token is placed
//! there on every game frame in both directions; the client's own hello and
//! ping frames use the same slot. [`Segment`] carries it so the byte stream
//! round-trips exactly. The value is a session property, not a constant
//! derivable from the client binary.
//!
//! Producer behavior (`0x00641B70`):
//! - reserve 8-byte header, write `[kind][id]` (1B+u16), flag `|=2`
//!   (`0x00641B9E` write(8); `0x00641BA7`..`0x00641BE1`).
//! - optional frag table `u16 count + count*(1B+u16)`, flag `|=4`
//!   (`0x00641BE3`..`0x00641C63`).
//! - message payload, then on successful `compress2` @ `0x006CBBF0` (called at
//!   `0x00641D4B`) flag `|=8`, drop the raw payload and write `u32 declen` +
//!   zlib bytes (`0x00641D7C`..`0x00641D99`).
//! - backpatch `size24 = written - 8` at header offset 4 (`0x00641DC5`..`0x00641DD4`).
//!
//! Consumer behavior (`0x006413A0`):
//! - read 8 bytes to `this+0xd4` (`0x00641438`); bit0 closes the socket
//!   (`0x0064144A`); `size24 = [this+0xd8] & 0xffffff` (`0x0064145D`, offset 4);
//!   `size24 > 0x100000` closes the socket (`0x00641469`).
//! - bit1 -> read addr `1B+u16` into `DFCMessage+0x54` (`0x006414F3`,
//!   `0x0064156D`..`0x006415CD`); bit2 -> `u16 count` (`0x00641639`) then
//!   `count*(1B+u16)` (`0x00641682`..`0x006416E0`); bit3 -> `u32 declen`
//!   (`0x0064170B`) then the rest is `uncompress`ed (`0x00641847`,
//!   `0x006D0EE0`); bit3 clear -> the rest is fed through raw (`0x0064186A`).
//!
//! # The `[01 0f 00][u32 declen][zlib]` claim
//!
//! False as an "inner game body magic". `01 0f 00` is a **`MessageAddress`**
//! (1-byte kind `0x01`, u16 id `0x000f`) parsed into `DFCMessage+0x54`; it is
//! data the server assigns, not a constant, and it sits *outside* the
//! decompressed payload. The real game body is the `DFCMessage` byte range
//! that `DFCMessage::writeTo` @ `0x0063E000` appends: a single game tag byte
//! plus fields (`drf-proto`'s domain), with no `01 0f 00` prefix.
//!
//! # zlib
//!
//! The client statically links zlib: `compress2` @ `0x006CBBF0` (-> `deflateInit`
//! `0x006CBF60`, `deflate` `0x006CC1B0`, `deflateEnd` `0x006CC8E0`; called at
//! `0x00641D4B`) and `uncompress` @ `0x006D0EE0` (-> `inflateInit`
//! `0x006CDB80`, `inflate` @ `0x006CDCE0`, `inflateEnd` `0x006CF200`; called at
//! `0x00641847`). Both are full **RFC 1950** zlib (2-byte header + DEFLATE +
//! Adler-32), so both directions require real inflate/deflate, not raw DEFLATE.
//!
//! The codec lives in [`zlib`]: a full bounded inflate (all DEFLATE block
//! types) and a minimal, legal stored-block deflate. It is dependency-free
//! because no crate registry/vendor source is available offline; the decision
//! is documented by the framing tests in this crate.

pub mod zlib;

/// Bytes of flags + peer + 24-bit size + reserved.
pub const SEG_HEADER_LEN: usize = 8;

/// Wire width of the per-session `peer` token (header bytes 1..=3).
pub const PEER_LEN: usize = 3;

/// Offset of the little-endian 24-bit `body` size within the header.
pub const SIZE_OFFSET: usize = 4;

/// The size field is 24 bits wide (`0x0064145D`: `AND ECX,0xffffff`).
pub const SIZE_MASK: u32 = 0x00ff_ffff;

/// Maximum admissible `size24`; larger closes the socket (`0x00641469`).
pub const MAX_SEGMENT_LEN: usize = 0x0010_0000;

/// Maximum admissible `declen`; larger closes the socket (`0x00641735`).
pub const MAX_DECLEN: u32 = 0x0010_0000;

/// Header flag bit0: client treats this segment as fatal (`0x0064144A`).
pub const FLAG_INVALID: u8 = 0x01;
/// Header flag bit1: a `MessageAddress` follows (`0x006414F3`).
pub const FLAG_ADDR: u8 = 0x02;
/// Header flag bit2: a frag table follows (`0x00641508`).
pub const FLAG_FRAG: u8 = 0x04;
/// Header flag bit3: the body tail is `[u32 declen][zlib]` (`0x0064151B`).
pub const FLAG_ZLIB: u8 = 0x08;

/// A `MessageAddress`: 1-byte kind + little-endian u16 id. Assigned by the
/// server in the connect handshake and echoed on every message
/// (`DFCMessageClient::processConnected` reads 1B -> `+0x30`, `2B` -> `+0x32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageAddress {
    pub kind: u8,
    pub id: u16,
}

impl MessageAddress {
    /// Wire width: `kind` (1) + `id` (2).
    pub const WIRE_LEN: usize = 3;

    pub fn new(kind: u8, id: u16) -> Self {
        MessageAddress { kind, id }
    }

    fn write_to(self, out: &mut Vec<u8>) {
        out.push(self.kind);
        out.extend_from_slice(&self.id.to_le_bytes());
    }

    fn read_from(buf: &[u8]) -> Option<Self> {
        let bytes = buf.get(..Self::WIRE_LEN)?;
        Some(MessageAddress {
            kind: bytes[0],
            id: u16::from_le_bytes([bytes[1], bytes[2]]),
        })
    }
}

/// The segment body tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    /// Flag bit3 clear: the remaining body bytes verbatim (`0x0064186A`).
    Raw(Vec<u8>),
    /// Flag bit3 set: `[u32 declen][zlib stream]` (`0x0064170B`/`0x00641D7C`).
    /// `declen` is the exact decompressed length the client validates against
    /// `uncompress`'s result (`0x00641858`).
    Zlib { declen: u32, data: Vec<u8> },
}

impl Payload {
    /// Build a `Zlib` payload by compressing `inner` and recording its length
    /// as `declen` (the value the client checks against `uncompress`'s result
    /// at `0x00641858`).
    pub fn zlib(inner: &[u8]) -> Payload {
        Payload::Zlib {
            declen: inner.len() as u32,
            data: zlib::compress(inner),
        }
    }

    /// If this is a `Zlib` payload, inflate it and validate `declen`. `Raw`
    /// payloads return `Ok` with their bytes borrowed-free (cloned).
    pub fn inflate(&self) -> Result<Vec<u8>, SegmentError> {
        match self {
            Payload::Raw(bytes) => Ok(bytes.clone()),
            Payload::Zlib { declen, data } => {
                let out = zlib::decompress(data, MAX_DECLEN as usize)
                    .map_err(|_| SegmentError::Invalid)?;
                if out.len() as u32 != *declen {
                    return Err(SegmentError::Invalid);
                }
                Ok(out)
            }
        }
    }
}

/// One transport segment. The same shape is used in both directions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Per-session token in header bytes 1..=3; `[0, 0, 0]`
    /// for a peerless/zeroed frame.
    pub peer: [u8; PEER_LEN],
    /// Destination/source address, present iff flag bit1.
    pub addr: Option<MessageAddress>,
    /// Frag table entries, present iff flag bit2 (never by itself).
    pub frags: Vec<MessageAddress>,
    /// Body tail: zlib-wrapped or raw.
    pub payload: Payload,
}

/// Why a buffer is not a (complete, valid) segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentError {
    /// Fewer bytes than the header/body needs: read more and retry.
    Incomplete,
    /// Structurally impossible: fatal (`FLAG_INVALID`, oversize, bad frag count).
    Invalid,
}

impl Segment {
    pub fn new(
        peer: [u8; PEER_LEN],
        addr: Option<MessageAddress>,
        frags: Vec<MessageAddress>,
        payload: Payload,
    ) -> Self {
        Segment {
            peer,
            addr,
            frags,
            payload,
        }
    }

    /// The flags byte this segment encodes to. Bits are derived from the
    /// fields; the client sets exactly these (`0x00641BC0`, `0x00641C09`,
    /// `0x00641D5D`).
    pub fn flags(&self) -> u8 {
        let mut f = 0u8;
        if self.addr.is_some() {
            f |= FLAG_ADDR;
        }
        if !self.frags.is_empty() {
            f |= FLAG_FRAG;
        }
        if matches!(self.payload, Payload::Zlib { .. }) {
            f |= FLAG_ZLIB;
        }
        f
    }

    /// Encode to wire bytes. Header bytes 1..=3 carry [`Segment::peer`]; the
    /// client never reads them (`updateReceiveQueue` only tests bit0 and the
    /// size field at offset 4), but the reference framing
    /// round-trips them, so they are preserved verbatim.
    pub fn encode(&self) -> Result<Vec<u8>, SegmentError> {
        let mut body = Vec::new();
        if let Some(addr) = self.addr {
            addr.write_to(&mut body);
        }
        if !self.frags.is_empty() {
            let n = u16::try_from(self.frags.len()).map_err(|_| SegmentError::Invalid)?;
            body.extend_from_slice(&n.to_le_bytes());
            for frag in &self.frags {
                frag.write_to(&mut body);
            }
        }
        match &self.payload {
            Payload::Raw(bytes) => body.extend_from_slice(bytes),
            Payload::Zlib { declen, data } => {
                body.extend_from_slice(&declen.to_le_bytes());
                body.extend_from_slice(data);
            }
        }
        if body.len() > MAX_SEGMENT_LEN {
            return Err(SegmentError::Invalid);
        }
        let size = body.len() as u32;

        let mut out = Vec::with_capacity(SEG_HEADER_LEN + body.len());
        out.push(self.flags());
        out.extend_from_slice(&self.peer); // header[1..4] per-session peer
        out.extend_from_slice(&size.to_le_bytes()[..3]); // header[4..7] size24
        out.push(0); // header[7] reserved
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decode one segment from the front of `buf`, returning it and the number
    /// of bytes consumed. Trailing bytes are left to the caller.
    pub fn decode(buf: &[u8]) -> Result<(Self, usize), SegmentError> {
        let header = buf.get(..SEG_HEADER_LEN).ok_or(SegmentError::Incomplete)?;
        let flags = header[0];
        let peer = [header[1], header[2], header[3]];
        if flags & FLAG_INVALID != 0 {
            return Err(SegmentError::Invalid);
        }
        let size = u32::from_le_bytes([header[4], header[5], header[6], 0]) as usize;
        if size > MAX_SEGMENT_LEN {
            return Err(SegmentError::Invalid);
        }
        let total = SEG_HEADER_LEN + size;
        let body = buf
            .get(SEG_HEADER_LEN..total)
            .ok_or(SegmentError::Incomplete)?;

        let mut cur = 0usize;
        let addr = if flags & FLAG_ADDR != 0 {
            let a = MessageAddress::read_from(&body[cur..]).ok_or(SegmentError::Invalid)?;
            cur += MessageAddress::WIRE_LEN;
            Some(a)
        } else {
            None
        };
        let mut frags = Vec::new();
        if flags & FLAG_FRAG != 0 {
            let count_bytes = body.get(cur..cur + 2).ok_or(SegmentError::Invalid)?;
            let count = u16::from_le_bytes([count_bytes[0], count_bytes[1]]) as usize;
            cur += 2;
            frags.reserve(count);
            for _ in 0..count {
                let f = MessageAddress::read_from(&body[cur..]).ok_or(SegmentError::Invalid)?;
                cur += MessageAddress::WIRE_LEN;
                frags.push(f);
            }
        }
        let payload = if flags & FLAG_ZLIB != 0 {
            let declen_bytes = body.get(cur..cur + 4).ok_or(SegmentError::Invalid)?;
            let declen = u32::from_le_bytes([
                declen_bytes[0],
                declen_bytes[1],
                declen_bytes[2],
                declen_bytes[3],
            ]);
            if declen > MAX_DECLEN {
                return Err(SegmentError::Invalid);
            }
            cur += 4;
            Payload::Zlib {
                declen,
                data: body[cur..].to_vec(),
            }
        } else {
            Payload::Raw(body[cur..].to_vec())
        };

        Ok((
            Segment {
                peer,
                addr,
                frags,
                payload,
            },
            total,
        ))
    }

    /// The verbatim `MessageAddress` a decoded segment carried, if any.
    pub fn addr(&self) -> Option<MessageAddress> {
        self.addr
    }
}

/// Carve every complete segment from `buf`. A trailing incomplete segment is
/// not an error (more bytes may arrive); a structurally invalid segment is.
pub fn split_stream(buf: &[u8]) -> Result<Vec<Segment>, SegmentError> {
    let mut out = Vec::new();
    let mut off = 0;
    while buf.len() - off >= SEG_HEADER_LEN {
        match Segment::decode(&buf[off..]) {
            Ok((seg, consumed)) => {
                out.push(seg);
                off += consumed;
            }
            Err(SegmentError::Incomplete) => break,
            Err(err) => return Err(err),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors laid out strictly per the binary-derived format above.
    // zlib payload bytes are RFC 1950 streams (what the client's `uncompress`
    // @ 0x006D0EE0 consumes) and are decoded by `crate::zlib`.

    /// flags ADDR, addr=(kind 0x01,id 0x000f), raw payload `07 06`. size24 = 5.
    const ADDR_RAW: [u8; 13] = [
        0x02, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x01, 0x0f, 0x00, 0x07, 0x06,
    ];
    /// flags ADDR|FRAG|ZLIB, addr=(0x11,0x2233), frags=[(0x44,0x5566)],
    /// declen=3, zlib stream of `07 34 61`. size24 = 0x17.
    const ADDR_FRAG_ZLIB: [u8; 31] = [
        0x0e, 0x00, 0x00, 0x00, 0x17, 0x00, 0x00, 0x00, 0x11, 0x33, 0x22, 0x01, 0x00, 0x44, 0x66,
        0x55, 0x03, 0x00, 0x00, 0x00, 0x78, 0x9c, 0x63, 0x37, 0x49, 0x04, 0x00, 0x00, 0xe1, 0x00,
        0x9d,
    ];
    /// flags ZLIB only, no addr/frag, declen=2, zlib stream of `07 06`.
    const ZLIB_ONLY: [u8; 22] = [
        0x08, 0x00, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x78, 0x9c, 0x63,
        0x67, 0x03, 0x00, 0x00, 0x16, 0x00, 0x0e,
    ];
    /// flags 0, empty body: only the 8-byte header.
    const EMPTY: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

    #[test]
    fn addr_raw_roundtrip() {
        let (seg, consumed) = Segment::decode(&ADDR_RAW).unwrap();
        assert_eq!(consumed, ADDR_RAW.len());
        assert_eq!(seg.flags(), FLAG_ADDR);
        assert_eq!(seg.addr, Some(MessageAddress::new(0x01, 0x000f)));
        assert!(seg.frags.is_empty());
        assert_eq!(seg.payload, Payload::Raw(vec![0x07, 0x06]));
        assert_eq!(seg.encode().unwrap(), ADDR_RAW);
    }

    #[test]
    fn addr_frag_zlib_roundtrip() {
        let (seg, consumed) = Segment::decode(&ADDR_FRAG_ZLIB).unwrap();
        assert_eq!(consumed, ADDR_FRAG_ZLIB.len());
        assert_eq!(seg.flags(), FLAG_ADDR | FLAG_FRAG | FLAG_ZLIB);
        assert_eq!(seg.addr, Some(MessageAddress::new(0x11, 0x2233)));
        assert_eq!(seg.frags, vec![MessageAddress::new(0x44, 0x5566)]);
        let Payload::Zlib { declen, data } = &seg.payload else {
            panic!("expected zlib payload");
        };
        assert_eq!(*declen, 3);
        // RFC 1950 stream of `07 34 61`.
        assert_eq!(
            data,
            &[0x78, 0x9c, 0x63, 0x37, 0x49, 0x04, 0x00, 0x00, 0xe1, 0x00, 0x9d]
        );
        assert_eq!(seg.encode().unwrap(), ADDR_FRAG_ZLIB);
    }

    #[test]
    fn zlib_only_roundtrip() {
        let (seg, consumed) = Segment::decode(&ZLIB_ONLY).unwrap();
        assert_eq!(consumed, ZLIB_ONLY.len());
        assert_eq!(seg.flags(), FLAG_ZLIB);
        assert_eq!(seg.addr, None);
        assert!(seg.frags.is_empty());
        assert_eq!(
            seg.payload,
            Payload::Zlib {
                declen: 2,
                data: vec![0x78, 0x9c, 0x63, 0x67, 0x03, 0x00, 0x00, 0x16, 0x00, 0x0e],
            }
        );
        assert_eq!(seg.encode().unwrap(), ZLIB_ONLY);
    }

    #[test]
    fn empty_segment_roundtrip() {
        let (seg, consumed) = Segment::decode(&EMPTY).unwrap();
        assert_eq!(consumed, 8);
        assert_eq!(seg.addr, None);
        assert!(seg.frags.is_empty());
        assert_eq!(seg.payload, Payload::Raw(Vec::new()));
        assert_eq!(seg.encode().unwrap(), EMPTY);
    }

    #[test]
    fn peer_bytes_are_preserved_not_ignored() {
        // header[1..4] carry the per-session peer; the client parser never
        // reads them (`0x0064144A` tests bit0; `0x0064145D` masks the size at
        // offset 4), but they survive a decode/encode round-trip.
        let mut buf = ADDR_RAW;
        buf[1] = 0xdd;
        buf[2] = 0x7b;
        buf[3] = 0xff;
        buf[7] = 0xff; // header[7] truly is ignored
        let (seg, consumed) = Segment::decode(&buf).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(seg.peer, [0xdd, 0x7b, 0xff]);
        assert_eq!(seg.addr, Some(MessageAddress::new(0x01, 0x000f)));
        assert_eq!(seg.payload, Payload::Raw(vec![0x07, 0x06]));
        // header[7] is the one byte encoder and decoder both drop.
        let mut expect = buf;
        expect[7] = 0;
        assert_eq!(seg.encode().unwrap(), expect);
    }

    /// Exact bytes from the reference framing:
    /// the reference `game_build(b'\x01\x02\x03', 0x07, 0x35, b'a')`.
    /// Note the layout: flags `0x0A` (ADDR|ZLIB), peer `01 02 03`,
    /// size24 `0x12`, body `[01 0f 00][declen=3][zlib]`.
    const REFERENCE_GAME_BUILD: [u8; 26] = [
        0x0a, 0x01, 0x02, 0x03, 0x12, 0x00, 0x00, 0x00, 0x01, 0x0f, 0x00, 0x03, 0x00, 0x00, 0x00,
        0x78, 0xda, 0x63, 0x37, 0x4d, 0x04, 0x00, 0x00, 0xe3, 0x00, 0x9e,
    ];

    #[test]
    fn decodes_reference_game_build_frame() {
        let (seg, consumed) = Segment::decode(&REFERENCE_GAME_BUILD).unwrap();
        assert_eq!(consumed, REFERENCE_GAME_BUILD.len());
        assert_eq!(seg.flags(), FLAG_ADDR | FLAG_ZLIB);
        assert_eq!(seg.peer, [0x01, 0x02, 0x03]);
        assert_eq!(seg.addr, Some(MessageAddress::new(0x01, 0x000f)));
        assert!(seg.frags.is_empty());
        let Payload::Zlib { declen, .. } = &seg.payload else {
            panic!("expected zlib payload");
        };
        assert_eq!(*declen, 3);
        // inner = [channel][mtype][payload] = 07 35 61
        assert_eq!(seg.payload.inflate().unwrap(), vec![0x07, 0x35, 0x61]);
        // Re-encoding the decoded segment reproduces the reference bytes
        // exactly (the zlib stream is preserved verbatim).
        assert_eq!(seg.encode().unwrap(), REFERENCE_GAME_BUILD);
    }

    #[test]
    fn builds_reference_shaped_frame_with_own_zlib() {
        // Our compressor's bytes differ from zlib's dynamic-Huffman output
        // (stored vs. Huffman), but the frame must have the reference's exact
        // shape and must inflate back to the same inner.
        let inner = [0x07u8, 0x35, 0x61];
        let seg = Segment::new(
            [0x01, 0x02, 0x03],
            Some(MessageAddress::new(0x01, 0x000f)),
            Vec::new(),
            Payload::zlib(&inner),
        );
        let frame = seg.encode().unwrap();
        assert_eq!(frame[0], 0x0a);
        assert_eq!(&frame[1..4], &[0x01, 0x02, 0x03]);
        assert_eq!(&frame[4..7], &[frame.len() as u8 - 8, 0x00, 0x00]);
        assert_eq!(&frame[8..11], &[0x01, 0x0f, 0x00]);
        assert_eq!(
            u32::from_le_bytes([frame[11], frame[12], frame[13], frame[14]]),
            3
        );
        // And it round-trips through our own decoder.
        let (back, n) = Segment::decode(&frame).unwrap();
        assert_eq!(n, frame.len());
        assert_eq!(back.peer, [0x01, 0x02, 0x03]);
        assert_eq!(back.payload.inflate().unwrap(), inner);
    }

    #[test]
    fn payload_zlib_roundtrip_and_declen_check() {
        let inner = b"channel-zero payload".to_vec();
        let p = Payload::zlib(&inner);
        let Payload::Zlib { declen, .. } = &p else {
            panic!("expected zlib");
        };
        assert_eq!(*declen as usize, inner.len());
        assert_eq!(p.inflate().unwrap(), inner);

        // A mismatched declen is rejected.
        let bad = Payload::Zlib {
            declen: 9999,
            data: match &p {
                Payload::Zlib { data, .. } => data.clone(),
                _ => unreachable!(),
            },
        };
        assert_eq!(bad.inflate(), Err(SegmentError::Invalid));
    }

    #[test]
    fn rejects_invalid_flag() {
        let mut buf = ADDR_RAW;
        buf[0] |= FLAG_INVALID;
        assert_eq!(Segment::decode(&buf), Err(SegmentError::Invalid));
    }

    #[test]
    fn rejects_oversize_size24() {
        // size24 = 0x100001 > MAX_SEGMENT_LEN; client closes the socket.
        let mut buf = ADDR_RAW.to_vec();
        buf[4] = 0x01;
        buf[5] = 0x00;
        buf[6] = 0x10;
        assert_eq!(Segment::decode(&buf), Err(SegmentError::Invalid));
    }

    #[test]
    fn rejects_oversize_declen() {
        let mut buf = ZLIB_ONLY.to_vec();
        buf[8] = 0x01;
        buf[9] = 0x00;
        buf[10] = 0x10;
        assert_eq!(Segment::decode(&buf), Err(SegmentError::Invalid));
    }

    #[test]
    fn short_bytes_are_incomplete() {
        assert_eq!(
            Segment::decode(&ADDR_RAW[..7]),
            Err(SegmentError::Incomplete)
        );
        assert_eq!(
            Segment::decode(&ADDR_RAW[..12]),
            Err(SegmentError::Incomplete)
        );
        assert_eq!(Segment::decode(&[]), Err(SegmentError::Incomplete));
    }

    #[test]
    fn frag_table_must_fit() {
        // flags claim a frag table but the size does not cover it.
        let mut buf = EMPTY.to_vec();
        buf[0] = FLAG_FRAG;
        // size24 stays 0, so no room for the u16 count.
        assert_eq!(Segment::decode(&buf), Err(SegmentError::Invalid));
    }

    #[test]
    fn split_stream_carves_complete_and_stops_at_partial() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&ADDR_RAW);
        buf.extend_from_slice(&ZLIB_ONLY);
        buf.extend_from_slice(&ADDR_FRAG_ZLIB[..10]); // partial trailer
        let segs = split_stream(&buf).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].encode().unwrap(), ADDR_RAW);
        assert_eq!(segs[1].encode().unwrap(), ZLIB_ONLY);
    }

    #[test]
    fn split_stream_propagates_invalid() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&ADDR_RAW);
        buf.push(0xff); // not a valid header start (bit0 set) -> Invalid
                        // Need 8 header bytes for decode to see the flag; pad.
        buf.extend_from_slice(&[0u8; 7]);
        // First segment is valid; the second byte run begins with 0xff flags.
        assert_eq!(split_stream(&buf), Err(SegmentError::Invalid));
    }

    #[test]
    fn split_stream_empty_and_garbage_only() {
        assert!(split_stream(&[]).unwrap().is_empty());
        assert!(split_stream(&[0x00, 0x00, 0x01]).unwrap().is_empty());
    }
}
