// SPDX-License-Identifier: MPL-2.0
//! Address join codes: a host address, a nonce and an expiry, beside the 6-character relay codes.
//!
//! The code is not a secret. The host password stays the access control. A player can read the
//! code out or type it; look-alike glyphs are the same set the relay codes already omit.
//!
//! Layout version 1, then base-31 with that alphabet. An IPv4 code is 46 characters and an IPv6
//! code is 65. A relay code is 6 characters, so the two never collide. Expiry is a unix second
//! the caller supplies: the code is valid while `now < expires_at` and expired at `now >= expires_at`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::join_code::{JOIN_CODE_ALPHABET, JOIN_CODE_LEN};

/// Default lifetime of a minted address code: 12 hours.
pub const ADDRESS_JOIN_TTL_DEFAULT_S: u64 = 12 * 60 * 60;
/// Characters in an IPv4 address code. One fewer does not fit the payload.
pub const ADDRESS_JOIN_IPV4_CHARS: usize = 46;
/// Characters in an IPv6 address code. One fewer does not fit the payload.
pub const ADDRESS_JOIN_IPV6_CHARS: usize = 65;

const ADDRESS_JOIN_VERSION: u8 = 1;
const FAMILY_V4: u8 = 4;
const FAMILY_V6: u8 = 6;
const CHECKSUM_LEN: usize = 4;
/// version, family, 4 address bytes, port, nonce, expiry.
const IPV4_BODY: usize = 24;
/// version, family, 16 address bytes, port, nonce, expiry.
const IPV6_BODY: usize = 36;
const IPV4_LEN: usize = IPV4_BODY + CHECKSUM_LEN;
const IPV6_LEN: usize = IPV6_BODY + CHECKSUM_LEN;

/// A decoded address join. The password is not in here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressJoin {
    /// Where the host is listening.
    pub addr: SocketAddr,
    /// Caller-chosen nonce. It is inside the code, so it is not a secret.
    pub nonce: u64,
    /// Unix second at which the code stops being accepted (`now >= expires_at` is expired).
    pub expires_at_unix_s: u64,
}

/// Why an address code was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressJoinError {
    /// The text is not an address code. A 6-character relay code lands here.
    NotAnAddressCode,
    /// The checksum does not match the payload. A one-character typo lands here.
    Checksum,
    /// The layout version is not one this build reads.
    BadVersion,
    /// The address family does not match the code width.
    BadAddress,
    /// `now_unix_s` is at or past the code's expiry.
    Expired {
        /// The unix second the code expires.
        expires_at_unix_s: u64,
    },
}

impl AddressJoinError {
    /// Agent and log code. Expired is `address_code_expired`; every other refusal is
    /// `address_code_invalid`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Expired { .. } => "address_code_expired",
            Self::NotAnAddressCode | Self::Checksum | Self::BadVersion | Self::BadAddress => {
                "address_code_invalid"
            }
        }
    }
}

impl std::fmt::Display for AddressJoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for AddressJoinError {}

/// An address code that expires `ttl_s` seconds after `now_unix_s`.
///
/// The caller passes the clock and the nonce, so a test needs no wall clock and no random source.
/// The addition saturates: a ttl that would overflow the unix second expires at `u64::MAX`.
pub fn mint_address_join(addr: SocketAddr, nonce: u64, now_unix_s: u64, ttl_s: u64) -> AddressJoin {
    AddressJoin {
        addr,
        nonce,
        expires_at_unix_s: now_unix_s.saturating_add(ttl_s),
    }
}

/// Encodes `join` as a typo-resistant code. IPv4 is [`ADDRESS_JOIN_IPV4_CHARS`] characters and
/// IPv6 is [`ADDRESS_JOIN_IPV6_CHARS`]. An IPv6 scope id is not stored.
pub fn encode_address_join(join: &AddressJoin) -> String {
    let packed = pack(join);
    let width = if packed.len() == IPV4_LEN {
        ADDRESS_JOIN_IPV4_CHARS
    } else {
        ADDRESS_JOIN_IPV6_CHARS
    };
    encode_base31(&packed, width)
}

/// Decodes `code` at `now_unix_s`. Whitespace, dashes and lowercase are accepted.
///
/// Valid while `now_unix_s < expires_at`. Expired when `now_unix_s >= expires_at`, including the
/// instant the TTL lands on. A 6-character relay code is [`AddressJoinError::NotAnAddressCode`].
pub fn decode_address_join(code: &str, now_unix_s: u64) -> Result<AddressJoin, AddressJoinError> {
    let Some(text) = normalize(code) else {
        return Err(AddressJoinError::NotAnAddressCode);
    };
    let nbytes = match text.len() {
        JOIN_CODE_LEN => return Err(AddressJoinError::NotAnAddressCode),
        ADDRESS_JOIN_IPV4_CHARS => IPV4_LEN,
        ADDRESS_JOIN_IPV6_CHARS => IPV6_LEN,
        _ => return Err(AddressJoinError::NotAnAddressCode),
    };
    let Some(bytes) = decode_base31(&text, nbytes) else {
        return Err(AddressJoinError::NotAnAddressCode);
    };
    unpack(&bytes, now_unix_s)
}

fn pack(join: &AddressJoin) -> Vec<u8> {
    let (family, octets) = match join.addr.ip() {
        IpAddr::V4(ip) => (FAMILY_V4, ip.octets().to_vec()),
        IpAddr::V6(ip) => (FAMILY_V6, ip.octets().to_vec()),
    };
    let mut body = Vec::with_capacity(2 + octets.len() + 2 + 8 + 8 + CHECKSUM_LEN);
    body.push(ADDRESS_JOIN_VERSION);
    body.push(family);
    body.extend_from_slice(&octets);
    body.extend_from_slice(&join.addr.port().to_be_bytes());
    body.extend_from_slice(&join.nonce.to_be_bytes());
    body.extend_from_slice(&join.expires_at_unix_s.to_be_bytes());
    let sum = checksum(&body);
    body.extend_from_slice(&sum);
    body
}

fn unpack(bytes: &[u8], now_unix_s: u64) -> Result<AddressJoin, AddressJoinError> {
    let (addr_len, family) = match bytes.len() {
        IPV4_LEN => (4, FAMILY_V4),
        IPV6_LEN => (16, FAMILY_V6),
        _ => return Err(AddressJoinError::NotAnAddressCode),
    };
    let split = bytes.len() - CHECKSUM_LEN;
    let (body, sum) = bytes.split_at(split);
    if checksum(body) != sum {
        return Err(AddressJoinError::Checksum);
    }
    if body.first().copied() != Some(ADDRESS_JOIN_VERSION) {
        return Err(AddressJoinError::BadVersion);
    }
    if body.get(1).copied() != Some(family) {
        return Err(AddressJoinError::BadAddress);
    }
    let addr_end = 2 + addr_len;
    let ip = match family {
        FAMILY_V4 => {
            let octets: [u8; 4] = arr(body, 2)?;
            IpAddr::V4(Ipv4Addr::from(octets))
        }
        _ => {
            let octets: [u8; 16] = arr(body, 2)?;
            IpAddr::V6(Ipv6Addr::from(octets))
        }
    };
    let port = u16::from_be_bytes(arr(body, addr_end)?);
    let nonce = u64::from_be_bytes(arr(body, addr_end + 2)?);
    let expires_at_unix_s = u64::from_be_bytes(arr(body, addr_end + 10)?);
    if now_unix_s >= expires_at_unix_s {
        return Err(AddressJoinError::Expired { expires_at_unix_s });
    }
    Ok(AddressJoin {
        addr: SocketAddr::new(ip, port),
        nonce,
        expires_at_unix_s,
    })
}

fn arr<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N], AddressJoinError> {
    let end = at.checked_add(N).ok_or(AddressJoinError::BadAddress)?;
    bytes
        .get(at..end)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(AddressJoinError::BadAddress)
}

fn checksum(body: &[u8]) -> [u8; CHECKSUM_LEN] {
    let hash = blake3::hash(body);
    let mut out = [0u8; CHECKSUM_LEN];
    out.copy_from_slice(&hash.as_bytes()[..CHECKSUM_LEN]);
    out
}

fn normalize(raw: &str) -> Option<String> {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_whitespace() || ch == '-' {
            continue;
        }
        let upper = ch.to_ascii_uppercase();
        if !upper.is_ascii() {
            return None;
        }
        let byte = u8::try_from(u32::from(upper)).ok()?;
        if !JOIN_CODE_ALPHABET.contains(&byte) {
            return None;
        }
        out.push(upper);
    }
    Some(out)
}

/// Big-endian `bytes` as `width` base-31 digits, most significant first. A zero payload is
/// `width` copies of the alphabet's first character.
fn encode_base31(bytes: &[u8], width: usize) -> String {
    let mut magnitude = bytes.to_vec();
    let mut digits = vec![0u8; width];
    for slot in digits.iter_mut().rev() {
        let mut rem = 0u16;
        for byte in &mut magnitude {
            let cur = (rem << 8) | u16::from(*byte);
            // `cur` is at most (30 << 8) | 255 = 7935, and 7935 / 31 = 255, so the quotient fits.
            *byte = u8::try_from(cur / 31).unwrap_or(u8::MAX);
            rem = cur % 31;
        }
        *slot = u8::try_from(rem).unwrap_or(0);
    }
    digits
        .into_iter()
        .map(|digit| char::from(JOIN_CODE_ALPHABET[usize::from(digit)]))
        .collect()
}

/// `text` (already normalized) as a big-endian integer of `nbytes`. `None` when a digit is
/// outside the alphabet or the integer does not fit in `nbytes`.
fn decode_base31(text: &str, nbytes: usize) -> Option<Vec<u8>> {
    let mut magnitude = vec![0u8; nbytes];
    for ch in text.chars() {
        let digit = alphabet_pos(ch)?;
        let mut carry = u16::from(digit);
        for byte in magnitude.iter_mut().rev() {
            let cur = u16::from(*byte) * 31 + carry;
            *byte = u8::try_from(cur & 0xff).unwrap_or(0);
            carry = cur >> 8;
        }
        if carry != 0 {
            return None;
        }
    }
    Some(magnitude)
}

fn alphabet_pos(ch: char) -> Option<u8> {
    let upper = ch.to_ascii_uppercase();
    if !upper.is_ascii() {
        return None;
    }
    let byte = u8::try_from(u32::from(upper)).ok()?;
    JOIN_CODE_ALPHABET
        .iter()
        .position(|candidate| *candidate == byte)
        .and_then(|index| u8::try_from(index).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `31^nchars >= 256^nbytes`, by comparing the two powers as little-endian byte strings.
    /// Dividing a running ratio would truncate, and these powers do not fit in `u128`.
    fn base31_width_fits(nbytes: u32, nchars: u32) -> bool {
        fn pow_bytes(base: u32, exp: u32) -> Vec<u8> {
            let mut magnitude = vec![1u8];
            for _ in 0..exp {
                let mut carry = 0u32;
                for byte in &mut magnitude {
                    let cur = u32::from(*byte) * base + carry;
                    // The next limb is the high byte of this product. Shifting the
                    // previous carry would drop it, and a base of 256 then collapses.
                    *byte = (cur & 0xff) as u8;
                    carry = cur >> 8;
                }
                while carry > 0 {
                    magnitude.push((carry & 0xff) as u8);
                    carry >>= 8;
                }
            }
            magnitude
        }
        let width = pow_bytes(31, nchars);
        let payload = pow_bytes(256, nbytes);
        let len = width.len().max(payload.len());
        for index in (0..len).rev() {
            let left = width.get(index).copied().unwrap_or(0);
            let right = payload.get(index).copied().unwrap_or(0);
            if left != right {
                return left > right;
            }
        }
        true
    }

    #[test]
    fn widths_are_the_minimum_that_fit() {
        assert!(base31_width_fits(IPV4_LEN as u32, ADDRESS_JOIN_IPV4_CHARS as u32));
        assert!(!base31_width_fits(
            IPV4_LEN as u32,
            (ADDRESS_JOIN_IPV4_CHARS - 1) as u32
        ));
        assert!(base31_width_fits(IPV6_LEN as u32, ADDRESS_JOIN_IPV6_CHARS as u32));
        assert!(!base31_width_fits(
            IPV6_LEN as u32,
            (ADDRESS_JOIN_IPV6_CHARS - 1) as u32
        ));
    }

    #[test]
    fn a_zero_payload_encodes_as_the_first_alphabet_character() {
        let code = encode_base31(&[0u8; IPV4_LEN], ADDRESS_JOIN_IPV4_CHARS);
        assert_eq!(code, "A".repeat(ADDRESS_JOIN_IPV4_CHARS));
        let back = decode_base31(&code, IPV4_LEN).expect("fits");
        assert!(back.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn default_ttl_is_twelve_hours() {
        assert_eq!(ADDRESS_JOIN_TTL_DEFAULT_S, 12 * 60 * 60);
    }
}
