// SPDX-License-Identifier: MPL-2.0
//! The datagrams the crypto path adds: `KeyInit`, `KeyReply` and the sealed record header.

use super::SEALED_HEADER_BYTES;
use crate::handshake::{MAGIC, PacketKind, begin};
use crate::wire::{WireError, WireReader};

/// `KeyInit` is padded to this size so it is never smaller than the `KeyReply` it asks for.
pub const KEY_INIT_BYTES: usize = 160;
const NOISE_MESSAGE_1_BYTES: usize = 32;
/// The Noise reply: `e` (32), encrypted `s` (48), the session token and its tag (4 + 16).
const NOISE_MESSAGE_2_BYTES: usize = 100;

/// `KeyInit`: the cookie echo and the first Noise message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInit {
    /// Echo of the request nonce.
    pub client_nonce: u64,
    /// Echo of the cookie.
    pub cookie: [u8; 16],
    /// The suite version.
    pub version: u8,
    /// Noise `-> e`.
    pub noise: [u8; NOISE_MESSAGE_1_BYTES],
}

impl KeyInit {
    /// The padded datagram.
    pub fn to_datagram(&self) -> Vec<u8> {
        let mut w = begin(PacketKind::KeyInit);
        w.u64(self.client_nonce);
        w.raw(&self.cookie);
        w.u8(self.version);
        w.raw(&self.noise);
        let mut out = w.finish();
        out.resize(KEY_INIT_BYTES, 0);
        out
    }

    /// Reads the bytes after the kind byte. Short (unpadded) datagrams are refused.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        if body.len() + 5 < KEY_INIT_BYTES {
            return Err(WireError::Invalid("key init is not padded"));
        }
        let mut r = WireReader::new(body);
        Ok(Self {
            client_nonce: r.u64()?,
            cookie: r.array()?,
            version: r.u8()?,
            noise: r.array()?,
        })
    }
}

/// `KeyReply`: the second Noise message. The session token travels inside it, so it is
/// authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyReply {
    /// Echo of the request nonce.
    pub client_nonce: u64,
    /// Noise `<- e, ee, s, es`, with the session token as its payload.
    pub noise: [u8; NOISE_MESSAGE_2_BYTES],
}

impl KeyReply {
    /// The datagram.
    pub fn to_datagram(&self) -> Vec<u8> {
        let mut w = begin(PacketKind::KeyReply);
        w.u64(self.client_nonce);
        w.raw(&self.noise);
        w.finish()
    }

    /// Reads the bytes after the kind byte.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(body);
        let out = Self {
            client_nonce: r.u64()?,
            noise: r.array()?,
        };
        r.finish()?;
        Ok(out)
    }
}

/// The clear part of a sealed datagram. Neither field needs associated data: a changed counter is
/// a different nonce and a changed token reaches a different key, so both fail the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealedHeader<'a> {
    /// The session token.
    pub token: u32,
    /// The counter field: the key epoch in the top byte, the record counter below it
    /// ([`SealedHeader::epoch`], [`SealedHeader::record_counter`]).
    pub counter: u64,
    /// Ciphertext and tag.
    pub ciphertext: &'a [u8],
}

impl<'a> SealedHeader<'a> {
    /// Splits the bytes after the kind byte. `None` when they are too short to hold a record.
    pub fn parse(body: &'a [u8]) -> Option<Self> {
        let (token, rest) = body.split_first_chunk::<4>()?;
        let (counter, ciphertext) = rest.split_first_chunk::<8>()?;
        Some(Self {
            token: u32::from_le_bytes(*token),
            counter: u64::from_le_bytes(*counter),
            ciphertext,
        })
    }

    /// The record counter: the low 56 bits of the counter field. It runs on across rekeys, so one
    /// replay window covers every key of the session (ADR-2416).
    pub fn record_counter(&self) -> u64 {
        self.counter & super::rekey::RECORD_COUNTER_MASK
    }

    /// The sender's key epoch, modulo 256: the top byte of the counter field (ADR-2416).
    pub fn epoch(&self) -> u8 {
        (self.counter >> super::rekey::EPOCH_SHIFT) as u8
    }

    /// Writes the header in front of a record.
    pub fn write(out: &mut [u8; SEALED_HEADER_BYTES], token: u32, counter: u64) {
        out[..4].copy_from_slice(&MAGIC);
        out[4] = PacketKind::Sealed as u8;
        out[5..9].copy_from_slice(&token.to_le_bytes());
        out[9..].copy_from_slice(&counter.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::split_datagram;

    #[test]
    fn key_init_round_trips_and_outweighs_the_reply() {
        let init = KeyInit {
            client_nonce: 9,
            cookie: [3; 16],
            version: 1,
            noise: [4; NOISE_MESSAGE_1_BYTES],
        };
        let bytes = init.to_datagram();
        let reply = KeyReply {
            client_nonce: 9,
            noise: [5; NOISE_MESSAGE_2_BYTES],
        }
        .to_datagram();
        assert_eq!(bytes.len(), KEY_INIT_BYTES);
        assert!(bytes.len() >= reply.len(), "no amplification");
        let (kind, body) = split_datagram(&bytes).expect("framing");
        assert_eq!(kind, PacketKind::KeyInit);
        assert_eq!(KeyInit::decode(body).expect("decode"), init);
        assert!(KeyInit::decode(&body[..body.len() - 1]).is_err(), "padding");
    }

    #[test]
    fn key_reply_round_trips_and_refuses_trailing_bytes() {
        let reply = KeyReply {
            client_nonce: 1,
            noise: [6; NOISE_MESSAGE_2_BYTES],
        };
        let mut bytes = reply.to_datagram();
        let (_, body) = split_datagram(&bytes).expect("framing");
        assert_eq!(KeyReply::decode(body).expect("decode"), reply);
        bytes.push(0);
        let (_, body) = split_datagram(&bytes).expect("framing");
        assert!(KeyReply::decode(body).is_err());
    }

    #[test]
    fn sealed_header_round_trips_and_needs_twelve_bytes() {
        let mut head = [0u8; SEALED_HEADER_BYTES];
        SealedHeader::write(&mut head, 0x0102_0304, 0x0506_0708_090A_0B0C);
        let (kind, body) = split_datagram(&head).expect("framing");
        assert_eq!(kind, PacketKind::Sealed);
        let parsed = SealedHeader::parse(body).expect("parse");
        assert_eq!(parsed.token, 0x0102_0304);
        assert_eq!(parsed.counter, 0x0506_0708_090A_0B0C);
        assert!(parsed.ciphertext.is_empty());
        assert!(SealedHeader::parse(&body[..11]).is_none());
    }
}
