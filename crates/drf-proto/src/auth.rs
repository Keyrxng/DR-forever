// SPDX-License-Identifier: GPL-3.0-or-later

//! Auth handshake and the auth connection cipher.
//!
//! Grounded on the client binary `DungeonRunners.exe` (PE32, ImageBase
//! `0x400000`, sha256 `f16f47302fa58ea30f4509363df877a3601adc85f99cdcbc3d27e9ce84ae1da8`)
//! by static analysis only; `game.pkg` is not an auth source. No network
//! captures are used by this crate.
//!
//! # What is client-derived here
//!
//! | item | evidence |
//! |---|---|
//! | Blowfish key `[;'.]94-31==-%&@!^+]\0` | bytes at VA `0x853728`, loaded by `MsgProtocolVer` @ `0x799FA0` and handed to `L2BlowfishEncrypt::InitializeBlowfish` @ `0x798880` (`0x79A03E`) |
//! | DES key `TEST\0\0\0\0` | bytes at VA `0x8534B4`, read by `L2DESEncrypt::DesKeyInit` @ `0x797630` (`0x79765B`), called from the auth-client setup @ `0x79AEED` |
//! | Blowfish cipher | `L2BlowfishEncrypt::Blowfish_encipher` @ `0x7983F0` / `Blowfish_decipher` @ `0x797F60`; P/S registers at `0x91F428` / `0x91F470`.. |
//! | 8-byte block, little-endian word pair | both routines load `[esi]` and `[esi+4]` as native `dword`s (`0x7983F1`, `0x798438`) |
//! | align-to-8 + XOR-checksum + 8-byte trailer | `L2Crypt::BlowFishEncryptPacket` @ `0x007B1B30` |
//! | checksum validation | `L2Crypt::BlowFishDecryptPacket` @ `0x007B1AE0` (`XOR` over `body[:-8]`, compare at `[edi+ecx*4]`) |
//! | `Login` type `0x00` | `linAQLoginPacket::s_packetType` @ `0x8504EA` |
//! | `LoginOk` type `0x03` | `linACLoginOkPacket::s_packetType` @ `0x8504D6` |
//! | `ServerList` type `0x04` | `linACSendServerListExPacket::s_packetType` @ `0x85045F` |
//! | `ServerListExt` type `0x05` | `linAQServerListExtPacket::s_packetType` @ `0x850453` |
//! | `AboutToPlay` type `0x02` | `linAQAboutToPlayPacket::s_packetType` @ `0x850447` |
//! | `PlayOk` type `0x07` | `linACPlayOkPacket::s_packetType` @ `0x850472` |
//! | `Login` body shape `[14B user][16B pass]` | `linAQLoginPacket::Serialize` @ `0x7993E0` (fixed 14 at `+0x10`, fixed 16 at `+0x1E` via `0x710130`) |
//! | `LoginOk` body `9×u32 + u8` | `linACLoginOkPacket::Serialize` @ `0x798F20` (nine `Serialize<u32>` @ `0x70ACB0`, one `Serialize<u8>` @ `0x70AC50`) |
//! | `AboutToPlay` body `u32 + u32 + u8` | `linAQAboutToPlayPacket::Serialize` @ `0x7967E0` |
//! | `PlayOk` body `u32 + u32 + u8` | `linACPlayOkPacket::Serialize` @ `0x7969E0`; `MsgPlayOk` @ `0x799C40` pushes `[+0x10]`, `[+0x14]`, byte `[+0x18]` |
//!
//! # Implementation-defined values
//!
//! Some values are needed to serve the client but are not protocol constants
//! in the binary:
//!
//! - The `PlayOk` second word is caller-supplied. The client reads the field
//!   (`RecvPlayOk` @ `0x61BA40`) but does not validate it.
//! - The `LoginOk` nine words and flag are bootstrap values chosen by this
//!   implementation. Only the *shape* (9×u32 + u8) is client-defined.
//! - The 3-byte greeting `03 00 00` ([`WELCOME`]) precedes `Login`. The
//!   client's first server-to-client operation is `ProtocolVer` (type `0x00`,
//!   `linACProtocolVerPacket` @ `0x8504DF`, `MsgProtocolVer` @ `0x799FA0`),
//!   which triggers Blowfish setup; the client will not send `Login` until it
//!   receives it.
//! - The game-port connect-ack is required before the client will continue,
//!   but its body is not binary-attested. The `0x0A` segment framing is
//!   client-derived and lives in `drf-wire`.
//! - The keepalive/ping body is answered with a bare `(07,06)` tick.
//!
//! The first `u16` length prefix used by [`frame`]/[`unframe`] is the auth
//! connection framing; it is required to serve a client but is not
//! independently attested by the binary.

/// Blowfish key, 20 ASCII bytes plus a NUL (VA `0x853728`, 21 bytes).
pub const BF_KEY: [u8; 21] = *b"[;'.]94-31==-%&@!^+]\0";

/// DES key for the login credential block (VA `0x8534B4`, 8 bytes).
pub const DES_KEY: [u8; 8] = *b"TEST\0\0\0\0";

/// c2s `Login` packet type (`linAQLoginPacket` @ `0x8504EA`).
pub const TYPE_LOGIN: u8 = 0x00;
/// s2c `ProtocolVer`; the client answers by initialising Blowfish
/// (`linACProtocolVerPacket` @ `0x8504DF`, `MsgProtocolVer` @ `0x799FA0`).
pub const TYPE_PROTOCOL_VER: u8 = 0x00;

/// The plaintext greeting the server sends the instant an auth connection is
/// accepted: a `[u16 total LE][type]` frame carrying the bare `ProtocolVer`
/// type (`03 00 00`). It is the one unencrypted auth packet -- it is what
/// makes the client initialise Blowfish, so it must precede every ciphered
/// packet. Without it the client sits in `DRAuthClient` state 1 and never
/// sends `Login`.
pub const WELCOME: [u8; 3] = [0x03, 0x00, 0x00];
/// c2s `AboutToPlay` (`linAQAboutToPlayPacket` @ `0x850447`).
pub const TYPE_ABOUT_TO_PLAY: u8 = 0x02;
/// s2c `LoginOk` (`linACLoginOkPacket` @ `0x8504D6`).
pub const TYPE_LOGIN_OK: u8 = 0x03;
/// s2c `ServerList` (`linACSendServerListExPacket` @ `0x85045F`).
pub const TYPE_SERVER_LIST: u8 = 0x04;
/// `ServerListExt` in both directions (`linAQServerListExtPacket` @ `0x850453`).
pub const TYPE_SERVER_LIST_EXT: u8 = 0x05;
/// s2c `PlayOk` (`linACPlayOkPacket` @ `0x850472`).
pub const TYPE_PLAY_OK: u8 = 0x07;

/// Fixed username field width in the `Login` body (`0x7993F5`).
pub const LOGIN_USERNAME_LEN: usize = 14;
/// Fixed password field width in the `Login` body (`0x799402`).
pub const LOGIN_PASSWORD_LEN: usize = 16;

/// Why a decrypted auth body is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// The ciphertext length is not a positive multiple of 8
    /// (`BlowFishDecryptPacket` @ `0x7B1AE3` rejects `len & 7`).
    NotBlockAligned,
    /// An inner shape did not have the bytes its reader consumes.
    Short,
}

// ---------------------------------------------------------------------------
// word swap
// ---------------------------------------------------------------------------

/// Swap each 8-byte block from two little-endian words to two big-endian
/// words. The client reads a block as native `dword`s (`[esi]`, `[esi+4]` at
/// `0x7983F1`/`0x798438`); carrying those two words as `u32` and re-emitting
/// them big-endian is byte-for-byte the same operation.
fn swap_le_be(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let whole = data.len() / 8 * 8;
    for chunk in data[..whole].as_chunks::<8>().0 {
        let v1 = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let v2 = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        out.extend_from_slice(&v1.to_be_bytes());
        out.extend_from_slice(&v2.to_be_bytes());
    }
    out.extend_from_slice(&data[whole..]);
    out
}

// ---------------------------------------------------------------------------
// Blowfish
// ---------------------------------------------------------------------------

/// Blowfish-ECB over big-endian block words, matching
/// `L2BlowfishEncrypt::InitializeBlowfish` @ `0x798880` and the encipher pair.
pub struct Blowfish {
    p: [u32; 18],
    s: [[u32; 256]; 4],
}

impl Blowfish {
    /// Expand `key` (1..=56 bytes) into the P/S tables. The client passes the
    /// 21 bytes of [`BF_KEY`], NUL included.
    pub fn new(key: &[u8]) -> Self {
        let mut p = BF_P;
        let mut s = [BF_S0, BF_S1, BF_S2, BF_S3];
        let mut j = 0usize;
        for word in p.iter_mut() {
            let mut w = 0u32;
            for _ in 0..4 {
                w = (w << 8) | key[j % key.len()] as u32;
                j += 1;
            }
            *word ^= w;
        }
        let mut l = 0u32;
        let mut r = 0u32;
        for i in (0..18).step_by(2) {
            let (a, b) = feistel(&p, &s, l, r);
            l = a;
            r = b;
            p[i] = l;
            p[i + 1] = r;
        }
        for bi in 0..4 {
            for i in (0..256).step_by(2) {
                let (a, b) = feistel(&p, &s, l, r);
                l = a;
                r = b;
                s[bi][i] = l;
                s[bi][i + 1] = r;
            }
        }
        Blowfish { p, s }
    }

    /// Encipher one 8-byte block, big-endian in and out (`enc8`).
    pub fn enc8(&self, block: &[u8]) -> [u8; 8] {
        let l = u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
        let r = u32::from_be_bytes([block[4], block[5], block[6], block[7]]);
        let (a, b) = self.enc(l, r);
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&a.to_be_bytes());
        out[4..].copy_from_slice(&b.to_be_bytes());
        out
    }

    /// Decipher one 8-byte block, big-endian in and out (`dec8`).
    pub fn dec8(&self, block: &[u8]) -> [u8; 8] {
        let l = u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
        let r = u32::from_be_bytes([block[4], block[5], block[6], block[7]]);
        let (a, b) = self.dec(l, r);
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&a.to_be_bytes());
        out[4..].copy_from_slice(&b.to_be_bytes());
        out
    }

    fn enc(&self, l: u32, r: u32) -> (u32, u32) {
        feistel(&self.p, &self.s, l, r)
    }

    fn dec(&self, l: u32, r: u32) -> (u32, u32) {
        feistel_inv(&self.p, &self.s, l, r)
    }
}

fn f(x: u32, s: &[[u32; 256]; 4]) -> u32 {
    let a = (x >> 24) as usize & 0xff;
    let b = (x >> 16) as usize & 0xff;
    let c = (x >> 8) as usize & 0xff;
    let d = x as usize & 0xff;
    (s[0][a].wrapping_add(s[1][b]) ^ s[2][c]).wrapping_add(s[3][d])
}

fn feistel(p: &[u32; 18], s: &[[u32; 256]; 4], mut l: u32, mut r: u32) -> (u32, u32) {
    l ^= p[0];
    for (round, &key) in p[1..17].iter().enumerate() {
        if (round + 1) & 1 == 1 {
            r ^= f(l, s) ^ key;
        } else {
            l ^= f(r, s) ^ key;
        }
    }
    r ^= p[17];
    (r, l)
}

fn feistel_inv(p: &[u32; 18], s: &[[u32; 256]; 4], mut l: u32, mut r: u32) -> (u32, u32) {
    l ^= p[17];
    for (round, &key) in p[1..17].iter().enumerate().rev() {
        if (round + 1) & 1 == 1 {
            l ^= f(r, s) ^ key;
        } else {
            r ^= f(l, s) ^ key;
        }
    }
    r ^= p[0];
    (r, l)
}

// ---------------------------------------------------------------------------
// auth packet cipher
// ---------------------------------------------------------------------------

/// Encipher an auth body exactly as `L2Crypt::BlowFishEncryptPacket` @
/// `0x007B1B30`: pad `type || payload` up to a multiple of 8, append the XOR
/// of its `u32` words, append one zero word, then Blowfish every 8-byte block
/// (word-swapped).
pub fn encrypt_packet(mtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(payload.len() + 1);
    msg.push(mtype);
    msg.extend_from_slice(payload);
    let rem = msg.len() % 8;
    if rem != 0 {
        msg.resize(msg.len() + (8 - rem), 0);
    }
    let checksum = msg.as_chunks::<4>().0.iter().fold(0u32, |acc, w| {
        acc ^ u32::from_le_bytes([w[0], w[1], w[2], w[3]])
    });
    msg.extend_from_slice(&checksum.to_le_bytes());
    msg.extend_from_slice(&[0u8; 4]);

    let swapped = swap_le_be(&msg);
    let mut enc = Vec::with_capacity(swapped.len());
    for block in swapped.as_chunks::<8>().0 {
        let mut b = [0u8; 8];
        b.copy_from_slice(block);
        enc.extend_from_slice(&bf().enc8(&b));
    }
    swap_le_be(&enc)
}

/// One decrypted auth body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthBody {
    /// The first decrypted byte: the packet type.
    pub mtype: u8,
    /// The decrypted message with the type byte and the 8-byte trailer
    /// removed. It still carries the cipher's zero padding to a multiple of 8.
    pub payload: Vec<u8>,
    /// Whether the XOR-checksum word matched (`sete al` @ `0x7B1B1F`). The
    /// client tolerates a mismatch in the reference server; callers decide.
    pub checksum_ok: bool,
}

/// Decipher an auth body exactly as `L2Crypt::BlowFishDecryptPacket` @
/// `0x007B1AE0`. `body` is the ciphertext without the `u16` length prefix.
pub fn decrypt_packet(body: &[u8]) -> Result<AuthBody, AuthError> {
    if body.is_empty() || !body.len().is_multiple_of(8) {
        return Err(AuthError::NotBlockAligned);
    }
    let swapped = swap_le_be(body);
    let mut plain = Vec::with_capacity(swapped.len());
    for block in swapped.as_chunks::<8>().0 {
        let mut b = [0u8; 8];
        b.copy_from_slice(block);
        plain.extend_from_slice(&bf().dec8(&b));
    }
    let plain = swap_le_be(&plain);

    let msg = &plain[..plain.len() - 8];
    let stored = u32::from_le_bytes([
        plain[plain.len() - 8],
        plain[plain.len() - 7],
        plain[plain.len() - 6],
        plain[plain.len() - 5],
    ]);
    let checksum = msg.as_chunks::<4>().0.iter().fold(0u32, |acc, w| {
        acc ^ u32::from_le_bytes([w[0], w[1], w[2], w[3]])
    });
    Ok(AuthBody {
        mtype: plain[0],
        payload: msg[1..].to_vec(),
        checksum_ok: checksum == stored,
    })
}

fn bf() -> &'static Blowfish {
    use std::sync::OnceLock;
    static BF: OnceLock<Blowfish> = OnceLock::new();
    BF.get_or_init(|| Blowfish::new(&BF_KEY))
}

/// The reference auth connection frame: `[u16 total LE][ciphertext]`, where
/// `total` counts the two length bytes. The prefix is not independently
/// attested by the binary (see the module note on unknowns).
pub fn frame(mtype: u8, payload: &[u8]) -> Vec<u8> {
    let body = encrypt_packet(mtype, payload);
    let total = (body.len() + 2) as u16;
    let mut out = Vec::with_capacity(body.len() + 2);
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Split one `[u16 total LE][body...]` frame from the front of `buf`. Returns
/// the ciphertext body and the number of bytes consumed.
pub fn unframe(buf: &[u8]) -> Result<(&[u8], usize), AuthError> {
    if buf.len() < 2 {
        return Err(AuthError::Short);
    }
    let total = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    if total < 2 || buf.len() < total {
        return Err(AuthError::Short);
    }
    Ok((&buf[2..total], total))
}

// ---------------------------------------------------------------------------
// DES (login credential block)
// ---------------------------------------------------------------------------

/// DES-ECB single key, matching `L2DESEncrypt` @ `0x797630`. Only the login
/// credential block uses it.
pub struct Des {
    sub: [u64; 16],
}

impl Des {
    /// Expand an 8-byte key into the 16 round subkeys.
    pub fn new(key: &[u8; 8]) -> Self {
        let k = u64::from_be_bytes(*key);
        let pc = perm(k, 64, &DES_PC1);
        let mut c = pc >> 28;
        let mut d = pc & 0x0fff_ffff;
        let mut sub = [0u64; 16];
        for (i, rot) in DES_ROT.iter().enumerate() {
            c = ((c << rot) | (c >> (28 - rot))) & 0x0fff_ffff;
            d = ((d << rot) | (d >> (28 - rot))) & 0x0fff_ffff;
            sub[i] = perm((c << 28) | d, 56, &DES_PC2);
        }
        Des { sub }
    }

    /// Encrypt one 8-byte block.
    pub fn enc8(&self, block: &[u8]) -> [u8; 8] {
        let mut keys = [0u64; 16];
        keys.copy_from_slice(&self.sub);
        crypt(&keys, block)
    }

    /// Decrypt one 8-byte block.
    pub fn dec8(&self, block: &[u8]) -> [u8; 8] {
        let mut keys = [0u64; 16];
        for (i, k) in self.sub.iter().rev().enumerate() {
            keys[i] = *k;
        }
        crypt(&keys, block)
    }
}

fn crypt(sub: &[u64; 16], block: &[u8]) -> [u8; 8] {
    let mut input = [0u8; 8];
    input.copy_from_slice(&block[..8]);
    let x = perm(u64::from_be_bytes(input), 64, &DES_IP);
    let mut l = x >> 32;
    let mut r = x & 0xffff_ffff;
    for k in sub {
        let e = perm(r, 32, &DES_E) ^ k;
        let mut s = 0u64;
        for i in 0..8 {
            let six = (e >> (42 - 6 * i)) & 0x3f;
            let row = ((six >> 4) & 2) | (six & 1);
            let col = (six >> 1) & 0xf;
            s = (s << 4) | DES_S[i * 64 + (row * 16 + col) as usize] as u64;
        }
        let next = r;
        r = l ^ perm(s, 32, &DES_P);
        l = next;
    }
    perm((r << 32) | l, 64, &DES_FP).to_be_bytes()
}

fn perm(bits: u64, nbits: u32, tab: &[u8]) -> u64 {
    let mut out = 0u64;
    for &p in tab {
        out = (out << 1) | ((bits >> (nbits - p as u32)) & 1);
    }
    out
}

// ---------------------------------------------------------------------------
// Login body
// ---------------------------------------------------------------------------

/// Build the 30-byte `Login` credential block: the client stores a 14-byte
/// username field and a 16-byte password field (`linAQLoginPacket::Serialize`
/// @ `0x7993E0`); DES enciphers the first three 8-byte blocks with [`DES_KEY`]
/// and the trailing 6 bytes stay plaintext.
pub fn login_block(username: &[u8], password: &[u8]) -> Vec<u8> {
    let mut plain = [0u8; LOGIN_USERNAME_LEN + LOGIN_PASSWORD_LEN];
    for (dst, src) in plain[..LOGIN_USERNAME_LEN].iter_mut().zip(username.iter()) {
        *dst = *src;
    }
    for (dst, src) in plain[LOGIN_USERNAME_LEN..].iter_mut().zip(password.iter()) {
        *dst = *src;
    }
    let des = Des::new(&DES_KEY);
    let mut out = Vec::with_capacity(30);
    for block in plain[..24].as_chunks::<8>().0 {
        let mut b = [0u8; 8];
        b.copy_from_slice(block);
        out.extend_from_slice(&des.enc8(&b));
    }
    out.extend_from_slice(&plain[24..30]);
    out
}

/// Recover `(username, password_len)` from a 30-byte `Login` credential block.
pub fn decode_login(block: &[u8]) -> Result<(Vec<u8>, usize), AuthError> {
    let block = block.get(..30).ok_or(AuthError::Short)?;
    let des = Des::new(&DES_KEY);
    let mut plain = Vec::with_capacity(30);
    for chunk in block[..24].as_chunks::<8>().0 {
        let mut b = [0u8; 8];
        b.copy_from_slice(chunk);
        plain.extend_from_slice(&des.dec8(&b));
    }
    plain.extend_from_slice(&block[24..30]);

    let user = plain[..LOGIN_USERNAME_LEN]
        .split(|&b| b == 0)
        .next()
        .unwrap_or(&[])
        .to_vec();
    let pw = &plain[LOGIN_USERNAME_LEN..];
    let pwlen = pw.split(|&b| b == 0).next().unwrap_or(&[]).len();
    Ok((user, pwlen))
}

// ---------------------------------------------------------------------------
// handshake bodies
// ---------------------------------------------------------------------------

/// `LoginOk` (`0x03`) body: nine `u32` words then one flag byte. The client
/// reads exactly this shape (`linACLoginOkPacket::Serialize` @ `0x798F20`).
/// The *values* are not client facts (see the module unknowns); pick them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginOk {
    pub words: [u32; 9],
    pub flag: u8,
}

impl LoginOk {
    pub const WIRE_LEN: usize = 9 * 4 + 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::WIRE_LEN);
        for w in self.words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.push(self.flag);
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Self, AuthError> {
        let body = payload.get(..Self::WIRE_LEN).ok_or(AuthError::Short)?;
        let mut words = [0u32; 9];
        for (i, w) in words.iter_mut().enumerate() {
            let o = i * 4;
            *w = u32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]);
        }
        Ok(LoginOk {
            words,
            flag: body[36],
        })
    }
}

/// `AboutToPlay` (`0x02`) request body: a `u64` session id then a server byte.
/// The client serialises two `u32`s at `+0x10`/`+0x14` then a byte
/// (`linAQAboutToPlayPacket::Serialize` @ `0x7967E0`); the reference reads it as
/// `u64` + `u8`, which is the same nine bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AboutToPlay {
    pub session: u64,
    pub server: u8,
}

impl AboutToPlay {
    pub const WIRE_LEN: usize = 9;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.session.to_le_bytes().to_vec();
        out.push(self.server);
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Self, AuthError> {
        let body = payload.get(..Self::WIRE_LEN).ok_or(AuthError::Short)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(&body[..8]);
        Ok(AboutToPlay {
            session: u64::from_le_bytes(b),
            server: body[8],
        })
    }
}

/// `PlayOk` (`0x07`) body: two `u32`s then a server byte
/// (`linACPlayOkPacket::Serialize` @ `0x7969E0`, `MsgPlayOk` @ `0x799C40`).
///
/// The second word is **not** fixed by the client. The reference writes `0x5678DEFA`
/// there, but that constant is absent from the binary and `RecvPlayOk` never
/// checks it, so this type carries the word as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayOk {
    /// The one-time key the server issues.
    pub key: u32,
    /// The second word. Client-unvalidated; value unproven.
    pub word: u32,
    /// The game server index.
    pub server: u8,
}

impl PlayOk {
    pub const WIRE_LEN: usize = 9;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.key.to_le_bytes().to_vec();
        out.extend_from_slice(&self.word.to_le_bytes());
        out.push(self.server);
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Self, AuthError> {
        let body = payload.get(..Self::WIRE_LEN).ok_or(AuthError::Short)?;
        Ok(PlayOk {
            key: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
            word: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
            server: body[8],
        })
    }
}

// ---------------------------------------------------------------------------
// client-derived tables
// ---------------------------------------------------------------------------

pub(crate) const BF_P: [u32; 18] = [
    0x243f6a88, 0x85a308d3, 0x13198a2e, 0x03707344, 0xa4093822, 0x299f31d0, 0x082efa98, 0xec4e6c89,
    0x452821e6, 0x38d01377, 0xbe5466cf, 0x34e90c6c, 0xc0ac29b7, 0xc97c50dd, 0x3f84d5b5, 0xb5470917,
    0x9216d5d9, 0x8979fb1b,
];
pub(crate) const BF_S0: [u32; 256] = [
    0xd1310ba6, 0x98dfb5ac, 0x2ffd72db, 0xd01adfb7, 0xb8e1afed, 0x6a267e96, 0xba7c9045, 0xf12c7f99,
    0x24a19947, 0xb3916cf7, 0x0801f2e2, 0x858efc16, 0x636920d8, 0x71574e69, 0xa458fea3, 0xf4933d7e,
    0x0d95748f, 0x728eb658, 0x718bcd58, 0x82154aee, 0x7b54a41d, 0xc25a59b5, 0x9c30d539, 0x2af26013,
    0xc5d1b023, 0x286085f0, 0xca417918, 0xb8db38ef, 0x8e79dcb0, 0x603a180e, 0x6c9e0e8b, 0xb01e8a3e,
    0xd71577c1, 0xbd314b27, 0x78af2fda, 0x55605c60, 0xe65525f3, 0xaa55ab94, 0x57489862, 0x63e81440,
    0x55ca396a, 0x2aab10b6, 0xb4cc5c34, 0x1141e8ce, 0xa15486af, 0x7c72e993, 0xb3ee1411, 0x636fbc2a,
    0x2ba9c55d, 0x741831f6, 0xce5c3e16, 0x9b87931e, 0xafd6ba33, 0x6c24cf5c, 0x7a325381, 0x28958677,
    0x3b8f4898, 0x6b4bb9af, 0xc4bfe81b, 0x66282193, 0x61d809cc, 0xfb21a991, 0x487cac60, 0x5dec8032,
    0xef845d5d, 0xe98575b1, 0xdc262302, 0xeb651b88, 0x23893e81, 0xd396acc5, 0x0f6d6ff3, 0x83f44239,
    0x2e0b4482, 0xa4842004, 0x69c8f04a, 0x9e1f9b5e, 0x21c66842, 0xf6e96c9a, 0x670c9c61, 0xabd388f0,
    0x6a51a0d2, 0xd8542f68, 0x960fa728, 0xab5133a3, 0x6eef0b6c, 0x137a3be4, 0xba3bf050, 0x7efb2a98,
    0xa1f1651d, 0x39af0176, 0x66ca593e, 0x82430e88, 0x8cee8619, 0x456f9fb4, 0x7d84a5c3, 0x3b8b5ebe,
    0xe06f75d8, 0x85c12073, 0x401a449f, 0x56c16aa6, 0x4ed3aa62, 0x363f7706, 0x1bfedf72, 0x429b023d,
    0x37d0d724, 0xd00a1248, 0xdb0fead3, 0x49f1c09b, 0x075372c9, 0x80991b7b, 0x25d479d8, 0xf6e8def7,
    0xe3fe501a, 0xb6794c3b, 0x976ce0bd, 0x04c006ba, 0xc1a94fb6, 0x409f60c4, 0x5e5c9ec2, 0x196a2463,
    0x68fb6faf, 0x3e6c53b5, 0x1339b2eb, 0x3b52ec6f, 0x6dfc511f, 0x9b30952c, 0xcc814544, 0xaf5ebd09,
    0xbee3d004, 0xde334afd, 0x660f2807, 0x192e4bb3, 0xc0cba857, 0x45c8740f, 0xd20b5f39, 0xb9d3fbdb,
    0x5579c0bd, 0x1a60320a, 0xd6a100c6, 0x402c7279, 0x679f25fe, 0xfb1fa3cc, 0x8ea5e9f8, 0xdb3222f8,
    0x3c7516df, 0xfd616b15, 0x2f501ec8, 0xad0552ab, 0x323db5fa, 0xfd238760, 0x53317b48, 0x3e00df82,
    0x9e5c57bb, 0xca6f8ca0, 0x1a87562e, 0xdf1769db, 0xd542a8f6, 0x287effc3, 0xac6732c6, 0x8c4f5573,
    0x695b27b0, 0xbbca58c8, 0xe1ffa35d, 0xb8f011a0, 0x10fa3d98, 0xfd2183b8, 0x4afcb56c, 0x2dd1d35b,
    0x9a53e479, 0xb6f84565, 0xd28e49bc, 0x4bfb9790, 0xe1ddf2da, 0xa4cb7e33, 0x62fb1341, 0xcee4c6e8,
    0xef20cada, 0x36774c01, 0xd07e9efe, 0x2bf11fb4, 0x95dbda4d, 0xae909198, 0xeaad8e71, 0x6b93d5a0,
    0xd08ed1d0, 0xafc725e0, 0x8e3c5b2f, 0x8e7594b7, 0x8ff6e2fb, 0xf2122b64, 0x8888b812, 0x900df01c,
    0x4fad5ea0, 0x688fc31c, 0xd1cff191, 0xb3a8c1ad, 0x2f2f2218, 0xbe0e1777, 0xea752dfe, 0x8b021fa1,
    0xe5a0cc0f, 0xb56f74e8, 0x18acf3d6, 0xce89e299, 0xb4a84fe0, 0xfd13e0b7, 0x7cc43b81, 0xd2ada8d9,
    0x165fa266, 0x80957705, 0x93cc7314, 0x211a1477, 0xe6ad2065, 0x77b5fa86, 0xc75442f5, 0xfb9d35cf,
    0xebcdaf0c, 0x7b3e89a0, 0xd6411bd3, 0xae1e7e49, 0x00250e2d, 0x2071b35e, 0x226800bb, 0x57b8e0af,
    0x2464369b, 0xf009b91e, 0x5563911d, 0x59dfa6aa, 0x78c14389, 0xd95a537f, 0x207d5ba2, 0x02e5b9c5,
    0x83260376, 0x6295cfa9, 0x11c81968, 0x4e734a41, 0xb3472dca, 0x7b14a94a, 0x1b510052, 0x9a532915,
    0xd60f573f, 0xbc9bc6e4, 0x2b60a476, 0x81e67400, 0x08ba6fb5, 0x571be91f, 0xf296ec6b, 0x2a0dd915,
    0xb6636521, 0xe7b9f9b6, 0xff34052e, 0xc5855664, 0x53b02d5d, 0xa99f8fa1, 0x08ba4799, 0x6e85076a,
];
pub(crate) const BF_S1: [u32; 256] = [
    0x4b7a70e9, 0xb5b32944, 0xdb75092e, 0xc4192623, 0xad6ea6b0, 0x49a7df7d, 0x9cee60b8, 0x8fedb266,
    0xecaa8c71, 0x699a17ff, 0x5664526c, 0xc2b19ee1, 0x193602a5, 0x75094c29, 0xa0591340, 0xe4183a3e,
    0x3f54989a, 0x5b429d65, 0x6b8fe4d6, 0x99f73fd6, 0xa1d29c07, 0xefe830f5, 0x4d2d38e6, 0xf0255dc1,
    0x4cdd2086, 0x8470eb26, 0x6382e9c6, 0x021ecc5e, 0x09686b3f, 0x3ebaefc9, 0x3c971814, 0x6b6a70a1,
    0x687f3584, 0x52a0e286, 0xb79c5305, 0xaa500737, 0x3e07841c, 0x7fdeae5c, 0x8e7d44ec, 0x5716f2b8,
    0xb03ada37, 0xf0500c0d, 0xf01c1f04, 0x0200b3ff, 0xae0cf51a, 0x3cb574b2, 0x25837a58, 0xdc0921bd,
    0xd19113f9, 0x7ca92ff6, 0x94324773, 0x22f54701, 0x3ae5e581, 0x37c2dadc, 0xc8b57634, 0x9af3dda7,
    0xa9446146, 0x0fd0030e, 0xecc8c73e, 0xa4751e41, 0xe238cd99, 0x3bea0e2f, 0x3280bba1, 0x183eb331,
    0x4e548b38, 0x4f6db908, 0x6f420d03, 0xf60a04bf, 0x2cb81290, 0x24977c79, 0x5679b072, 0xbcaf89af,
    0xde9a771f, 0xd9930810, 0xb38bae12, 0xdccf3f2e, 0x5512721f, 0x2e6b7124, 0x501adde6, 0x9f84cd87,
    0x7a584718, 0x7408da17, 0xbc9f9abc, 0xe94b7d8c, 0xec7aec3a, 0xdb851dfa, 0x63094366, 0xc464c3d2,
    0xef1c1847, 0x3215d908, 0xdd433b37, 0x24c2ba16, 0x12a14d43, 0x2a65c451, 0x50940002, 0x133ae4dd,
    0x71dff89e, 0x10314e55, 0x81ac77d6, 0x5f11199b, 0x043556f1, 0xd7a3c76b, 0x3c11183b, 0x5924a509,
    0xf28fe6ed, 0x97f1fbfa, 0x9ebabf2c, 0x1e153c6e, 0x86e34570, 0xeae96fb1, 0x860e5e0a, 0x5a3e2ab3,
    0x771fe71c, 0x4e3d06fa, 0x2965dcb9, 0x99e71d0f, 0x803e89d6, 0x5266c825, 0x2e4cc978, 0x9c10b36a,
    0xc6150eba, 0x94e2ea78, 0xa5fc3c53, 0x1e0a2df4, 0xf2f74ea7, 0x361d2b3d, 0x1939260f, 0x19c27960,
    0x5223a708, 0xf71312b6, 0xebadfe6e, 0xeac31f66, 0xe3bc4595, 0xa67bc883, 0xb17f37d1, 0x018cff28,
    0xc332ddef, 0xbe6c5aa5, 0x65582185, 0x68ab9802, 0xeecea50f, 0xdb2f953b, 0x2aef7dad, 0x5b6e2f84,
    0x1521b628, 0x29076170, 0xecdd4775, 0x619f1510, 0x13cca830, 0xeb61bd96, 0x0334fe1e, 0xaa0363cf,
    0xb5735c90, 0x4c70a239, 0xd59e9e0b, 0xcbaade14, 0xeecc86bc, 0x60622ca7, 0x9cab5cab, 0xb2f3846e,
    0x648b1eaf, 0x19bdf0ca, 0xa02369b9, 0x655abb50, 0x40685a32, 0x3c2ab4b3, 0x319ee9d5, 0xc021b8f7,
    0x9b540b19, 0x875fa099, 0x95f7997e, 0x623d7da8, 0xf837889a, 0x97e32d77, 0x11ed935f, 0x16681281,
    0x0e358829, 0xc7e61fd6, 0x96dedfa1, 0x7858ba99, 0x57f584a5, 0x1b227263, 0x9b83c3ff, 0x1ac24696,
    0xcdb30aeb, 0x532e3054, 0x8fd948e4, 0x6dbc3128, 0x58ebf2ef, 0x34c6ffea, 0xfe28ed61, 0xee7c3c73,
    0x5d4a14d9, 0xe864b7e3, 0x42105d14, 0x203e13e0, 0x45eee2b6, 0xa3aaabea, 0xdb6c4f15, 0xfacb4fd0,
    0xc742f442, 0xef6abbb5, 0x654f3b1d, 0x41cd2105, 0xd81e799e, 0x86854dc7, 0xe44b476a, 0x3d816250,
    0xcf62a1f2, 0x5b8d2646, 0xfc8883a0, 0xc1c7b6a3, 0x7f1524c3, 0x69cb7492, 0x47848a0b, 0x5692b285,
    0x095bbf00, 0xad19489d, 0x1462b174, 0x23820e00, 0x58428d2a, 0x0c55f5ea, 0x1dadf43e, 0x233f7061,
    0x3372f092, 0x8d937e41, 0xd65fecf1, 0x6c223bdb, 0x7cde3759, 0xcbee7460, 0x4085f2a7, 0xce77326e,
    0xa6078084, 0x19f8509e, 0xe8efd855, 0x61d99735, 0xa969a7aa, 0xc50c06c2, 0x5a04abfc, 0x800bcadc,
    0x9e447a2e, 0xc3453484, 0xfdd56705, 0x0e1e9ec9, 0xdb73dbd3, 0x105588cd, 0x675fda79, 0xe3674340,
    0xc5c43465, 0x713e38d8, 0x3d28f89e, 0xf16dff20, 0x153e21e7, 0x8fb03d4a, 0xe6e39f2b, 0xdb83adf7,
];
pub(crate) const BF_S2: [u32; 256] = [
    0xe93d5a68, 0x948140f7, 0xf64c261c, 0x94692934, 0x411520f7, 0x7602d4f7, 0xbcf46b2e, 0xd4a20068,
    0xd4082471, 0x3320f46a, 0x43b7d4b7, 0x500061af, 0x1e39f62e, 0x97244546, 0x14214f74, 0xbf8b8840,
    0x4d95fc1d, 0x96b591af, 0x70f4ddd3, 0x66a02f45, 0xbfbc09ec, 0x03bd9785, 0x7fac6dd0, 0x31cb8504,
    0x96eb27b3, 0x55fd3941, 0xda2547e6, 0xabca0a9a, 0x28507825, 0x530429f4, 0x0a2c86da, 0xe9b66dfb,
    0x68dc1462, 0xd7486900, 0x680ec0a4, 0x27a18dee, 0x4f3ffea2, 0xe887ad8c, 0xb58ce006, 0x7af4d6b6,
    0xaace1e7c, 0xd3375fec, 0xce78a399, 0x406b2a42, 0x20fe9e35, 0xd9f385b9, 0xee39d7ab, 0x3b124e8b,
    0x1dc9faf7, 0x4b6d1856, 0x26a36631, 0xeae397b2, 0x3a6efa74, 0xdd5b4332, 0x6841e7f7, 0xca7820fb,
    0xfb0af54e, 0xd8feb397, 0x454056ac, 0xba489527, 0x55533a3a, 0x20838d87, 0xfe6ba9b7, 0xd096954b,
    0x55a867bc, 0xa1159a58, 0xcca92963, 0x99e1db33, 0xa62a4a56, 0x3f3125f9, 0x5ef47e1c, 0x9029317c,
    0xfdf8e802, 0x04272f70, 0x80bb155c, 0x05282ce3, 0x95c11548, 0xe4c66d22, 0x48c1133f, 0xc70f86dc,
    0x07f9c9ee, 0x41041f0f, 0x404779a4, 0x5d886e17, 0x325f51eb, 0xd59bc0d1, 0xf2bcc18f, 0x41113564,
    0x257b7834, 0x602a9c60, 0xdff8e8a3, 0x1f636c1b, 0x0e12b4c2, 0x02e1329e, 0xaf664fd1, 0xcad18115,
    0x6b2395e0, 0x333e92e1, 0x3b240b62, 0xeebeb922, 0x85b2a20e, 0xe6ba0d99, 0xde720c8c, 0x2da2f728,
    0xd0127845, 0x95b794fd, 0x647d0862, 0xe7ccf5f0, 0x5449a36f, 0x877d48fa, 0xc39dfd27, 0xf33e8d1e,
    0x0a476341, 0x992eff74, 0x3a6f6eab, 0xf4f8fd37, 0xa812dc60, 0xa1ebddf8, 0x991be14c, 0xdb6e6b0d,
    0xc67b5510, 0x6d672c37, 0x2765d43b, 0xdcd0e804, 0xf1290dc7, 0xcc00ffa3, 0xb5390f92, 0x690fed0b,
    0x667b9ffb, 0xcedb7d9c, 0xa091cf0b, 0xd9155ea3, 0xbb132f88, 0x515bad24, 0x7b9479bf, 0x763bd6eb,
    0x37392eb3, 0xcc115979, 0x8026e297, 0xf42e312d, 0x6842ada7, 0xc66a2b3b, 0x12754ccc, 0x782ef11c,
    0x6a124237, 0xb79251e7, 0x06a1bbe6, 0x4bfb6350, 0x1a6b1018, 0x11caedfa, 0x3d25bdd8, 0xe2e1c3c9,
    0x44421659, 0x0a121386, 0xd90cec6e, 0xd5abea2a, 0x64af674e, 0xda86a85f, 0xbebfe988, 0x64e4c3fe,
    0x9dbc8057, 0xf0f7c086, 0x60787bf8, 0x6003604d, 0xd1fd8346, 0xf6381fb0, 0x7745ae04, 0xd736fccc,
    0x83426b33, 0xf01eab71, 0xb0804187, 0x3c005e5f, 0x77a057be, 0xbde8ae24, 0x55464299, 0xbf582e61,
    0x4e58f48f, 0xf2ddfda2, 0xf474ef38, 0x8789bdc2, 0x5366f9c3, 0xc8b38e74, 0xb475f255, 0x46fcd9b9,
    0x7aeb2661, 0x8b1ddf84, 0x846a0e79, 0x915f95e2, 0x466e598e, 0x20b45770, 0x8cd55591, 0xc902de4c,
    0xb90bace1, 0xbb8205d0, 0x11a86248, 0x7574a99e, 0xb77f19b6, 0xe0a9dc09, 0x662d09a1, 0xc4324633,
    0xe85a1f02, 0x09f0be8c, 0x4a99a025, 0x1d6efe10, 0x1ab93d1d, 0x0ba5a4df, 0xa186f20f, 0x2868f169,
    0xdcb7da83, 0x573906fe, 0xa1e2ce9b, 0x4fcd7f52, 0x50115e01, 0xa70683fa, 0xa002b5c4, 0x0de6d027,
    0x9af88c27, 0x773f8641, 0xc3604c06, 0x61a806b5, 0xf0177a28, 0xc0f586e0, 0x006058aa, 0x30dc7d62,
    0x11e69ed7, 0x2338ea63, 0x53c2dd94, 0xc2c21634, 0xbbcbee56, 0x90bcb6de, 0xebfc7da1, 0xce591d76,
    0x6f05e409, 0x4b7c0188, 0x39720a3d, 0x7c927c24, 0x86e3725f, 0x724d9db9, 0x1ac15bb4, 0xd39eb8fc,
    0xed545578, 0x08fca5b5, 0xd83d7cd3, 0x4dad0fc4, 0x1e50ef5e, 0xb161e6f8, 0xa28514d9, 0x6c51133c,
    0x6fd5c7e7, 0x56e14ec4, 0x362abfce, 0xddc6c837, 0xd79a3234, 0x92638212, 0x670efa8e, 0x406000e0,
];
pub(crate) const BF_S3: [u32; 256] = [
    0x3a39ce37, 0xd3faf5cf, 0xabc27737, 0x5ac52d1b, 0x5cb0679e, 0x4fa33742, 0xd3822740, 0x99bc9bbe,
    0xd5118e9d, 0xbf0f7315, 0xd62d1c7e, 0xc700c47b, 0xb78c1b6b, 0x21a19045, 0xb26eb1be, 0x6a366eb4,
    0x5748ab2f, 0xbc946e79, 0xc6a376d2, 0x6549c2c8, 0x530ff8ee, 0x468dde7d, 0xd5730a1d, 0x4cd04dc6,
    0x2939bbdb, 0xa9ba4650, 0xac9526e8, 0xbe5ee304, 0xa1fad5f0, 0x6a2d519a, 0x63ef8ce2, 0x9a86ee22,
    0xc089c2b8, 0x43242ef6, 0xa51e03aa, 0x9cf2d0a4, 0x83c061ba, 0x9be96a4d, 0x8fe51550, 0xba645bd6,
    0x2826a2f9, 0xa73a3ae1, 0x4ba99586, 0xef5562e9, 0xc72fefd3, 0xf752f7da, 0x3f046f69, 0x77fa0a59,
    0x80e4a915, 0x87b08601, 0x9b09e6ad, 0x3b3ee593, 0xe990fd5a, 0x9e34d797, 0x2cf0b7d9, 0x022b8b51,
    0x96d5ac3a, 0x017da67d, 0xd1cf3ed6, 0x7c7d2d28, 0x1f9f25cf, 0xadf2b89b, 0x5ad6b472, 0x5a88f54c,
    0xe029ac71, 0xe019a5e6, 0x47b0acfd, 0xed93fa9b, 0xe8d3c48d, 0x283b57cc, 0xf8d56629, 0x79132e28,
    0x785f0191, 0xed756055, 0xf7960e44, 0xe3d35e8c, 0x15056dd4, 0x88f46dba, 0x03a16125, 0x0564f0bd,
    0xc3eb9e15, 0x3c9057a2, 0x97271aec, 0xa93a072a, 0x1b3f6d9b, 0x1e6321f5, 0xf59c66fb, 0x26dcf319,
    0x7533d928, 0xb155fdf5, 0x03563482, 0x8aba3cbb, 0x28517711, 0xc20ad9f8, 0xabcc5167, 0xccad925f,
    0x4de81751, 0x3830dc8e, 0x379d5862, 0x9320f991, 0xea7a90c2, 0xfb3e7bce, 0x5121ce64, 0x774fbe32,
    0xa8b6e37e, 0xc3293d46, 0x48de5369, 0x6413e680, 0xa2ae0810, 0xdd6db224, 0x69852dfd, 0x09072166,
    0xb39a460a, 0x6445c0dd, 0x586cdecf, 0x1c20c8ae, 0x5bbef7dd, 0x1b588d40, 0xccd2017f, 0x6bb4e3bb,
    0xdda26a7e, 0x3a59ff45, 0x3e350a44, 0xbcb4cdd5, 0x72eacea8, 0xfa6484bb, 0x8d6612ae, 0xbf3c6f47,
    0xd29be463, 0x542f5d9e, 0xaec2771b, 0xf64e6370, 0x740e0d8d, 0xe75b1357, 0xf8721671, 0xaf537d5d,
    0x4040cb08, 0x4eb4e2cc, 0x34d2466a, 0x0115af84, 0xe1b00428, 0x95983a1d, 0x06b89fb4, 0xce6ea048,
    0x6f3f3b82, 0x3520ab82, 0x011a1d4b, 0x277227f8, 0x611560b1, 0xe7933fdc, 0xbb3a792b, 0x344525bd,
    0xa08839e1, 0x51ce794b, 0x2f32c9b7, 0xa01fbac9, 0xe01cc87e, 0xbcc7d1f6, 0xcf0111c3, 0xa1e8aac7,
    0x1a908749, 0xd44fbd9a, 0xd0dadecb, 0xd50ada38, 0x0339c32a, 0xc6913667, 0x8df9317c, 0xe0b12b4f,
    0xf79e59b7, 0x43f5bb3a, 0xf2d519ff, 0x27d9459c, 0xbf97222c, 0x15e6fc2a, 0x0f91fc71, 0x9b941525,
    0xfae59361, 0xceb69ceb, 0xc2a86459, 0x12baa8d1, 0xb6c1075e, 0xe3056a0c, 0x10d25065, 0xcb03a442,
    0xe0ec6e0e, 0x1698db3b, 0x4c98a0be, 0x3278e964, 0x9f1f9532, 0xe0d392df, 0xd3a0342b, 0x8971f21e,
    0x1b0a7441, 0x4ba3348c, 0xc5be7120, 0xc37632d8, 0xdf359f8d, 0x9b992f2e, 0xe60b6f47, 0x0fe3f11d,
    0xe54cda54, 0x1edad891, 0xce6279cf, 0xcd3e7e6f, 0x1618b166, 0xfd2c1d05, 0x848fd2c5, 0xf6fb2299,
    0xf523f357, 0xa6327623, 0x93a83531, 0x56cccd02, 0xacf08162, 0x5a75ebb5, 0x6e163697, 0x88d273cc,
    0xde966292, 0x81b949d0, 0x4c50901b, 0x71c65614, 0xe6c6c7bd, 0x327a140a, 0x45e1d006, 0xc3f27b9a,
    0xc9aa53fd, 0x62a80f00, 0xbb25bfe2, 0x35bdd2f6, 0x71126905, 0xb2040222, 0xb6cbcf7c, 0xcd769c2b,
    0x53113ec0, 0x1640e3d3, 0x38abbd60, 0x2547adf0, 0xba38209c, 0xf746ce76, 0x77afa1c5, 0x20756060,
    0x85cbfe4e, 0x8ae88dd8, 0x7aaaf9b0, 0x4cf9aa7e, 0x1948c25c, 0x02fb8a8c, 0x01c36ae4, 0xd6ebe1f9,
    0x90d4f869, 0xa65cdea0, 0x3f09252d, 0xc208e69f, 0xb74e6132, 0xce77e25b, 0x578fdfe3, 0x3ac372e6,
];
pub(crate) const DES_IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
pub(crate) const DES_FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
pub(crate) const DES_E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18,
    19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
pub(crate) const DES_P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];
pub(crate) const DES_PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];
pub(crate) const DES_PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52,
    31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
pub(crate) const DES_ROT: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
pub(crate) const DES_S: [u8; 512] = [
    14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11,
    9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9, 1, 7, 5,
    11, 3, 14, 10, 0, 6, 13, 15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15,
    2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13,
    8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9, 10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11,
    4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1,
    2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12, 7, 13, 14, 3, 0, 6,
    9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9, 10, 6,
    9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7,
    2, 14, 2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0,
    15, 10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
    2, 13, 6, 15, 0, 9, 10, 4, 5, 3, 12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15,
    4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13,
    11, 6, 4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13, 4, 11, 2, 14, 15, 0, 8, 13, 3, 12,
    9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3,
    7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12, 13, 2,
    8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14,
    9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10, 8, 13, 15, 12,
    9, 0, 3, 5, 6, 11,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Eric Young's Blowfish known-answer vectors (independent of the game).
    #[test]
    fn blowfish_known_answers() {
        let bf = Blowfish::new(b"abcdefghijklmnopqrstuvwxyz");
        assert_eq!(hex(&bf.enc8(b"BLOWFISH")), "324ed0fef413a203");
        assert_eq!(
            hex(&bf.dec8(&unhex("324ed0fef413a203"))),
            "424c4f5746495348"
        );

        let zero = Blowfish::new(&[0u8; 8]);
        assert_eq!(hex(&zero.enc8(&[0u8; 8])), "4ef997456198dd78");
    }

    // The P-array seed is pi (`BF_P[0]`; the client's tables live at
    // `0x91F428`.. after `InitializeBlowfish` @ `0x798880`).
    #[test]
    fn blowfish_pi_seed() {
        assert_eq!(BF_P[0], 0x243f_6a88);
        assert_eq!(BF_P[17], 0x8979_fb1b);
    }

    // Golden bodies produced by the client algorithm (L2Crypt @ 0x7B1B30);
    // the reference `auth_encrypt` emits the same bytes behind a u16 length.
    #[test]
    fn encrypt_packet_golden() {
        assert_eq!(
            hex(&encrypt_packet(TYPE_ABOUT_TO_PLAY, &[1, 2, 3, 4])),
            "cce0fabd187bbc93598230579c0139f0"
        );
        assert_eq!(
            hex(&encrypt_packet(
                TYPE_PLAY_OK,
                &[0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44, 0x05]
            )),
            "1740a9c644a418f208e95fc066a06878266041c8524e248c"
        );
    }

    #[test]
    fn packet_roundtrip_and_checksum() {
        let payload = b"round trip";
        let body = encrypt_packet(TYPE_LOGIN, payload);
        let dec = decrypt_packet(&body).unwrap();
        assert_eq!(dec.mtype, TYPE_LOGIN);
        assert!(dec.checksum_ok);
        assert_eq!(&dec.payload[..payload.len()], payload);

        // flipping a ciphertext byte breaks the checksum but not the length.
        let mut bad = body.clone();
        bad[0] ^= 0xff;
        assert!(!decrypt_packet(&bad).unwrap().checksum_ok);
    }

    #[test]
    fn frame_roundtrip() {
        let framed = frame(TYPE_LOGIN_OK, &[9, 8, 7]);
        assert_eq!(
            u16::from_le_bytes([framed[0], framed[1]]) as usize,
            framed.len()
        );
        let (body, used) = unframe(&framed).unwrap();
        assert_eq!(used, framed.len());
        let dec = decrypt_packet(body).unwrap();
        assert_eq!(dec.mtype, TYPE_LOGIN_OK);
        assert_eq!(&dec.payload[..3], &[9, 8, 7]);
    }

    #[test]
    fn decrypt_rejects_unaligned() {
        assert_eq!(decrypt_packet(&[]), Err(AuthError::NotBlockAligned));
        assert_eq!(decrypt_packet(&[0u8; 9]), Err(AuthError::NotBlockAligned));
    }

    // FIPS PUB 46-3 known answer: key 133457799BBCDFF1, pt 0123456789ABCDEF.
    #[test]
    fn des_known_answer() {
        let des = Des::new(&unhex("133457799bbcdff1").try_into().unwrap());
        assert_eq!(
            hex(&des.enc8(&unhex("0123456789abcdef"))),
            "85e813540f0ab405"
        );
        assert_eq!(
            hex(&des.dec8(&unhex("85e813540f0ab405"))),
            "0123456789abcdef"
        );
    }

    #[test]
    fn login_block_golden_and_roundtrip() {
        assert_eq!(
            hex(&login_block(b"testuser", b"testpass12")),
            "dcbc3305d2e466765f1c8dcb89a1643e41f2846e3ed308fa000000000000"
        );
        let (user, pwlen) = decode_login(&login_block(b"testuser", b"testpass12")).unwrap();
        assert_eq!(user, b"testuser");
        assert_eq!(pwlen, 10);
    }

    #[test]
    fn login_ok_roundtrip() {
        let ok = LoginOk {
            words: [0, 1, 2, 3, 4, 5, 6, 7, 8],
            flag: 1,
        };
        let bytes = ok.encode();
        assert_eq!(bytes.len(), LoginOk::WIRE_LEN);
        assert_eq!(LoginOk::decode(&bytes), Ok(ok));
        assert_eq!(LoginOk::decode(&bytes[..4]), Err(AuthError::Short));
    }

    #[test]
    fn about_to_play_roundtrip() {
        let msg = AboutToPlay {
            session: 0x1122_3344_5566_7788,
            server: 3,
        };
        let bytes = msg.encode();
        assert_eq!(bytes.len(), AboutToPlay::WIRE_LEN);
        assert_eq!(&bytes[..8], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(bytes[8], 3);
        assert_eq!(AboutToPlay::decode(&bytes), Ok(msg));
    }

    #[test]
    fn play_ok_roundtrip() {
        let ok = PlayOk {
            key: 0xdead_beef,
            word: 0,
            server: 2,
        };
        let bytes = ok.encode();
        assert_eq!(bytes.len(), PlayOk::WIRE_LEN);
        assert_eq!(PlayOk::decode(&bytes), Ok(ok));
    }
}
