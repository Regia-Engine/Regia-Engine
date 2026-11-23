// SPDX-License-Identifier: MPL-2.0
//! Voice packet wire format, version 1 (`docs/specs/59-voice-chat.md` § 2.8).
//!
//! Header, little-endian, 12 bytes: `version|flags` (1), `sender` (2), `channel` (2), `seq` (2),
//! `timestamp` (4, 48 kHz units), `codec|quality` (1). A packet with the `multi` flag starts its
//! payload with one count byte.

use super::error::VoiceChatError;
use serde::{Deserialize, Serialize};

/// Wire format version written by this build.
pub const PACKET_VERSION: u8 = 1;
/// Header size in bytes.
pub const HEADER_LEN: usize = 12;
/// Largest payload an Opus packet may carry (spec § 2.4).
pub const MAX_PAYLOAD: usize = 400;
/// Largest payload a PCM packet may carry. PCM is the test and loopback codec: one 48 kHz frame
/// is 1921 bytes, which is over the network MTU, so it must not be used on a real link at the
/// `High` profile (ADR-1647).
pub const MAX_PCM_PAYLOAD: usize = 4096;
/// Timestamp units per second (always 48 kHz regardless of profile).
pub const TIMESTAMP_RATE: u32 = 48_000;
/// Timestamp units in one 20 ms frame.
pub const FRAME_TIMESTAMP_STEP: u32 = 960;

/// Set on a multi-frame count byte when the first frame is a spare copy of a frame already sent, not
/// new audio (`redundancy`, spec § 2.8).
pub const MULTI_REDUNDANT: u8 = 0x80;
/// Frames in a multi-frame packet.
pub const MULTI_FRAMES: u8 = 2;

/// The two frames of a packet with the `multi` flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MultiFrames<'a> {
    /// The frame at the header's `seq`.
    pub first: &'a [u8],
    /// The frame at `seq + 1`.
    pub second: &'a [u8],
    /// The first frame is a spare copy.
    pub redundant: bool,
}

/// Appends a multi-frame payload: the count byte, the first frame's length (`u16` little-endian), then
/// both frames.
pub fn write_multi(first: &[u8], second: &[u8], redundant: bool, out: &mut Vec<u8>) {
    out.reserve(3 + first.len() + second.len());
    out.push(MULTI_FRAMES | if redundant { MULTI_REDUNDANT } else { 0 });
    out.extend_from_slice(&(first.len() as u16).to_le_bytes());
    out.extend_from_slice(first);
    out.extend_from_slice(second);
}

/// Parses a multi-frame payload.
///
/// # Errors
/// [`VoiceChatError::BadPacket`] for a count other than two or a length past the payload.
pub fn parse_multi(payload: &[u8]) -> Result<MultiFrames<'_>, VoiceChatError> {
    let Some((head, rest)) = payload.split_first_chunk::<3>() else {
        return Err(VoiceChatError::BadPacket(
            "multi-frame payload is too short".into(),
        ));
    };
    if head[0] & !MULTI_REDUNDANT != MULTI_FRAMES {
        return Err(VoiceChatError::BadPacket(format!(
            "multi-frame count {}",
            head[0]
        )));
    }
    let len = usize::from(u16::from_le_bytes([head[1], head[2]]));
    if len > rest.len() {
        return Err(VoiceChatError::BadPacket(
            "multi-frame length past the payload".into(),
        ));
    }
    let (first, second) = rest.split_at(len);
    Ok(MultiFrames {
        first,
        second,
        redundant: head[0] & MULTI_REDUNDANT != 0,
    })
}

/// Flag bits of the first header byte (high nibble).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceFlags(u8);

impl VoiceFlags {
    /// The talk spurt ended with this packet.
    pub const END_OF_TALK: u8 = 0x10;
    /// The sender restarted its encoder; flush the decoder.
    pub const RESET: u8 = 0x20;
    /// The sender marks the packet as priority speech.
    pub const PRIORITY: u8 = 0x40;
    /// The payload starts with a frame count byte (more than one frame).
    pub const MULTI: u8 = 0x80;

    /// Builds flags from raw bits; bits outside the flag nibble are ignored.
    #[must_use]
    pub fn from_bits(bits: u8) -> Self {
        Self(bits & 0xF0)
    }

    /// The raw flag bits (in the high nibble).
    #[must_use]
    pub fn bits(self) -> u8 {
        self.0
    }

    /// Whether every bit of `mask` is set.
    #[must_use]
    pub fn contains(self, mask: u8) -> bool {
        self.0 & mask == mask
    }

    /// Returns the flags with `mask` set or cleared.
    #[must_use]
    pub fn with(self, mask: u8, on: bool) -> Self {
        if on {
            Self(self.0 | (mask & 0xF0))
        } else {
            Self(self.0 & !mask)
        }
    }
}

/// Bandwidth profile (spec § 2.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VoiceQuality {
    /// 16 kHz, target 12 kbps.
    Narrow,
    /// 24 kHz, target 20 kbps.
    #[default]
    Standard,
    /// 48 kHz, target 32 kbps.
    High,
}

impl VoiceQuality {
    /// Codec sample rate of the profile.
    #[must_use]
    pub fn sample_rate(self) -> u32 {
        match self {
            Self::Narrow => 16_000,
            Self::Standard => 24_000,
            Self::High => 48_000,
        }
    }

    /// Samples in one 20 ms frame.
    #[must_use]
    pub fn frame_samples(self) -> usize {
        (self.sample_rate() / 50) as usize
    }

    /// Target bit rate in bits per second.
    #[must_use]
    pub fn target_bps(self) -> u32 {
        match self {
            Self::Narrow => 12_000,
            Self::Standard => 20_000,
            Self::High => 32_000,
        }
    }

    /// The low-nibble code in the wire format.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Narrow => 0,
            Self::Standard => 1,
            Self::High => 2,
        }
    }

    /// Parses the low-nibble code.
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Narrow),
            1 => Some(Self::Standard),
            2 => Some(Self::High),
            _ => None,
        }
    }

    /// Lowercase name used by the agent protocol and config.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Narrow => "narrow",
            Self::Standard => "standard",
            Self::High => "high",
        }
    }
}

/// Codec id carried in the high nibble of the quality byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum VoiceCodecId {
    /// 16-bit little-endian PCM with a 1-byte mode header.
    #[default]
    Pcm,
    /// Opus (a [`VoiceCodecProvider`](super::VoiceCodecProvider) brings it; `regia_media` feature
    /// `voice-opus`).
    Opus,
}

impl VoiceCodecId {
    /// Lowercase name used by the agent protocol.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pcm => "pcm",
            Self::Opus => "opus",
        }
    }

    /// The high-nibble code.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Pcm => 0,
            Self::Opus => 1,
        }
    }

    /// Parses the high-nibble code.
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Pcm),
            1 => Some(Self::Opus),
            _ => None,
        }
    }
}

/// A parsed voice packet borrowing its payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoicePacket<'a> {
    /// Flag bits.
    pub flags: VoiceFlags,
    /// Session slot of the sender (the server overwrites it).
    pub sender: u16,
    /// Channel id.
    pub channel: u16,
    /// Sequence number, wraps at 65536.
    pub seq: u16,
    /// Timestamp in 48 kHz units, wraps at 2^32.
    pub timestamp: u32,
    /// Bandwidth profile.
    pub quality: VoiceQuality,
    /// Codec id.
    pub codec: VoiceCodecId,
    /// Codec payload (after the header).
    pub payload: &'a [u8],
}

impl<'a> VoicePacket<'a> {
    /// Parses a packet. Rejects short buffers, version 0 or a version newer than
    /// [`PACKET_VERSION`], unknown codec or profile codes and payloads over the codec's cap
    /// ([`MAX_PAYLOAD`] for Opus, [`MAX_PCM_PAYLOAD`] for PCM). All four flag bits are defined,
    /// so none is unknown.
    ///
    /// # Errors
    /// [`VoiceChatError::BadPacket`] with the reason.
    pub fn parse(bytes: &'a [u8]) -> Result<VoicePacket<'a>, VoiceChatError> {
        let Some((header, payload)) = bytes.split_first_chunk::<HEADER_LEN>() else {
            return Err(VoiceChatError::BadPacket(format!(
                "{} bytes is shorter than the header",
                bytes.len()
            )));
        };
        let version = header[0] & 0x0F;
        if version == 0 || version > PACKET_VERSION {
            return Err(VoiceChatError::BadPacket(format!("unknown version {version}")));
        }
        let quality = VoiceQuality::from_code(header[11] & 0x0F)
            .ok_or_else(|| VoiceChatError::BadPacket(format!("unknown quality {}", header[11] & 0x0F)))?;
        let codec = VoiceCodecId::from_code(header[11] >> 4)
            .ok_or_else(|| VoiceChatError::BadPacket(format!("unknown codec {}", header[11] >> 4)))?;
        let cap = if codec == VoiceCodecId::Pcm {
            MAX_PCM_PAYLOAD
        } else {
            MAX_PAYLOAD
        };
        if payload.len() > cap {
            return Err(VoiceChatError::BadPacket(format!(
                "payload of {} bytes exceeds {cap}",
                payload.len()
            )));
        }
        Ok(VoicePacket {
            flags: VoiceFlags::from_bits(header[0]),
            sender: u16::from_le_bytes([header[1], header[2]]),
            channel: u16::from_le_bytes([header[3], header[4]]),
            seq: u16::from_le_bytes([header[5], header[6]]),
            timestamp: u32::from_le_bytes([header[7], header[8], header[9], header[10]]),
            quality,
            codec,
            payload,
        })
    }

    /// Appends the serialized packet to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.reserve(HEADER_LEN + self.payload.len());
        out.push(PACKET_VERSION | self.flags.bits());
        out.extend_from_slice(&self.sender.to_le_bytes());
        out.extend_from_slice(&self.channel.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.timestamp.to_le_bytes());
        out.push(self.quality.code() | (self.codec.code() << 4));
        out.extend_from_slice(self.payload);
    }
}
