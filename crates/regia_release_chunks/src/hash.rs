// SPDX-License-Identifier: MPL-2.0
//! SHA-256, SHA-1 (FIPS 180-4) and FNV-1a 64. Original code from the public specifications.
//! Known answers: SHA-256 and SHA-1 of the empty string and of `abc`; FNV-1a 64 of the empty
//! string is the offset basis.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::ChunkError;

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// SHA-256 of `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finish()
}

/// Lowercase hex of [`sha256`].
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    hex_encode(&sha256(data))
}

/// SHA-256 of a file, read in blocks.
///
/// # Errors
///
/// `io_error` when the file cannot be read.
pub fn sha256_file(path: &Path) -> Result<[u8; 32], ChunkError> {
    let mut file = File::open(path).map_err(|err| ChunkError::io(path, &err))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).map_err(|err| ChunkError::io(path, &err))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish())
}

/// SHA-1 of `data`.
#[must_use]
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finish()
}

/// Bucket index: the first four SHA-1 bytes of `rel`, big-endian, modulo `buckets`.
///
/// # Errors
///
/// `rules_invalid` when `buckets` is 0.
pub fn bucket_index(rel: &str, buckets: u32) -> Result<u32, ChunkError> {
    if buckets == 0 {
        return Err(ChunkError::new("rules_invalid", "bucket count is 0"));
    }
    let digest = sha1(rel.as_bytes());
    let word = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    Ok(word % buckets)
}

/// FNV-1a 64. The empty input hashes to the offset basis `0xcbf29ce484222325`.
#[must_use]
pub fn fnv1a64(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// 16 lowercase hex digits of [`fnv1a64`].
#[must_use]
pub fn content_hash(data: &[u8]) -> String {
    format!("{:016x}", fnv1a64(data))
}

/// Mesh id: [`content_hash`] of `vertices` followed by each index as a little-endian `u32`,
/// then 8 hex digits of the vertex-byte length.
///
/// # Errors
///
/// `rules_invalid` when the vertex slice is longer than `u32::MAX`.
pub fn mesh_id(vertices: &[u8], indices: &[u32]) -> Result<String, ChunkError> {
    let geom = u32::try_from(vertices.len())
        .map_err(|_| ChunkError::new("rules_invalid", "vertex bytes do not fit in 8 hex digits"))?;
    let mut buf = Vec::with_capacity(vertices.len() + indices.len() * 4);
    buf.extend_from_slice(vertices);
    for index in indices {
        buf.extend_from_slice(&index.to_le_bytes());
    }
    Ok(format!("{}{geom:08x}", content_hash(&buf)))
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    filled: usize,
    bytes: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            block: [0; 64],
            filled: 0,
            bytes: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let space = 64 - self.filled;
            let take = space.min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            self.bytes += take as u64;
            data = &data[take..];
            if self.filled == 64 {
                compress256(&mut self.state, &self.block);
                self.filled = 0;
            }
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let bits = self.bytes.wrapping_mul(8);
        self.update(&[0x80]);
        if self.filled > 56 {
            let pad = 64 - self.filled;
            self.update(&[0u8; 64][..pad]);
        }
        let zeros = 56 - self.filled;
        self.update(&[0u8; 64][..zeros]);
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (slot, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *slot = word.to_be_bytes();
        }
        out
    }
}

fn compress256(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut words = [0u32; 64];
    for (index, slot) in words[..16].iter_mut().enumerate() {
        let at = index * 4;
        *slot = u32::from_be_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]]);
    }
    for index in 16..64 {
        let s0 =
            words[index - 15].rotate_right(7) ^ words[index - 15].rotate_right(18) ^ (words[index - 15] >> 3);
        let s1 =
            words[index - 2].rotate_right(17) ^ words[index - 2].rotate_right(19) ^ (words[index - 2] >> 10);
        words[index] = words[index - 16]
            .wrapping_add(s0)
            .wrapping_add(words[index - 7])
            .wrapping_add(s1);
    }
    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];
    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[index])
            .wrapping_add(words[index]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

struct Sha1 {
    state: [u32; 5],
    block: [u8; 64],
    filled: usize,
    bytes: u64,
}

impl Sha1 {
    fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0],
            block: [0; 64],
            filled: 0,
            bytes: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let space = 64 - self.filled;
            let take = space.min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            self.bytes += take as u64;
            data = &data[take..];
            if self.filled == 64 {
                compress1(&mut self.state, &self.block);
                self.filled = 0;
            }
        }
    }

    fn finish(mut self) -> [u8; 20] {
        let bits = self.bytes.wrapping_mul(8);
        self.update(&[0x80]);
        if self.filled > 56 {
            let pad = 64 - self.filled;
            self.update(&[0u8; 64][..pad]);
        }
        let zeros = 56 - self.filled;
        self.update(&[0u8; 64][..zeros]);
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 20];
        for (slot, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *slot = word.to_be_bytes();
        }
        out
    }
}

fn compress1(state: &mut [u32; 5], block: &[u8; 64]) {
    let mut words = [0u32; 80];
    for (index, slot) in words[..16].iter_mut().enumerate() {
        let at = index * 4;
        *slot = u32::from_be_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]]);
    }
    for index in 16..80 {
        words[index] =
            (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16]).rotate_left(1);
    }
    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    for (index, word) in words.iter().enumerate() {
        let (f, k) = if index < 20 {
            ((b & c) | (!b & d), 0x5a82_7999)
        } else if index < 40 {
            (b ^ c ^ d, 0x6ed9_eba1)
        } else if index < 60 {
            ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc)
        } else {
            (b ^ c ^ d, 0xca62_c1d6)
        };
        let temp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
}
