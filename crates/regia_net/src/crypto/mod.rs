// SPDX-License-Identifier: MPL-2.0
//! Transport encryption for the datagram endpoints (ADR-1901).
//!
//! Connected traffic is protected by the Noise Protocol Framework pattern
//! [`NOISE_PATTERN`] through the `snow` crate; this module holds no cryptographic primitive of its
//! own. What it adds around Noise is what UDP needs:
//!
//! - [`CryptoMode`], the switch: [`CryptoMode::Auto`] (the default) requires encryption for every
//!   non-loopback peer and leaves loopback plain. Each side decides from its own mode and the
//!   peer's address, so a network attacker has nothing to strip.
//! - [`ServerIdentity`] and [`ServerKey`]: the server's static key and what a client pins.
//! - [`SecureLink`]: one session's keys, an explicit 64-bit counter per datagram, and a
//!   [`ReplayWindow`] checked before decryption and updated only after authentication.
//! - [`PendingTable`] and [`HandshakeGovernor`]: curve work happens only after a valid cookie, and
//!   both the state and the work it can cost are bounded.
//! - [`RekeyPolicy`]: each direction moves to the Noise `REKEY` of its key after a number of
//!   records or bytes, with the key epoch in the record header (ADR-2416).
//! - [`TicketKeys`], [`ResumeCredential`] and [`ResumeFlight`]: the resumption ticket of a sealed
//!   accept and the one-round-trip resume it buys (wire 6, ADR-8858).
//!
//! ```text
//! client                                  server
//!   ConnectRequest(crypto offer)    ──▶    (stateless)
//!   ◀── Challenge(cookie, crypto choice)   (stateless)
//!   KeyInit(cookie, e)              ──▶    cookie checked, then curve work
//!   ◀── KeyReply(token, e, ee, s, es)      client checks the server key here
//!   Sealed(join token)              ──▶
//!   ◀── Sealed(Accept | Deny)
//! ```
//!
//! An unpinned client resists a passive listener only. See `book/agents/net-crypto.md` for what
//! this does and does not protect.

mod identity;
mod link;
mod pending;
mod player;
mod rekey;
mod replay;
mod resume;
mod wire;

use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::handshake::PasswordChallenge;

pub(crate) use identity::ServerKeyFile;
pub use identity::{SERVER_KEY_FORMAT_VERSION, ServerIdentity, ServerKey};
pub use link::{ClientHandshake, SealedKind, SecureLink, accept_handshake, fixture_link_pair};
pub use pending::{
    DEFAULT_HANDSHAKES_PER_S, HandshakeGovernor, MAX_PENDING_PER_IP, PENDING_TTL_MS, PendingHandshake,
    PendingRefusal, PendingTable,
};
#[cfg(test)]
pub(crate) use player::forge_zero_key_proof;
pub use player::{
    PLAYER_PROOF_BYTES, PLAYER_PROOF_PATTERN, PLAYER_PROOF_PROLOGUE, PlayerIdentity, PlayerKey,
    verify_player_proof,
};
pub use rekey::{EPOCH_SHIFT, MAX_EPOCH_SKIP, MIN_RECORDS_PER_KEY, RECORD_COUNTER_MASK, RekeyPolicy};
pub use replay::{REPLAY_WINDOW_COUNTERS, ReplayWindow};
pub use resume::{
    MAX_RESUME_ACCEPT_RECORD_BYTES, MAX_RESUME_HELLO_BYTES, MAX_RESUME_NOISE_BYTES, RESUME_INIT_BYTES,
    RESUME_INIT_FIELD_BYTES, RESUME_INIT_UNANSWERED_SENDS, RESUME_NOISE_PATTERN, RESUME_PSK_LABEL,
    RESUME_REPLY_NOISE_BYTES, RESUMPTION_TICKET_VERSION, ResumeCredential, ResumeEpochs, ResumeFlight,
    ResumeHello, ResumeInit, ResumeRefusal, ResumeReply, ResumeResponder, ResumeRetry, TicketClaims,
    TicketKeys, build_hash, derive_resume_psk, resume_prologue,
};
pub use wire::{KEY_INIT_BYTES, KeyInit, KeyReply, SealedHeader};

/// The Noise protocol name.
pub const NOISE_PATTERN: &str = "Noise_NX_25519_ChaChaPoly_BLAKE2s";
/// The suite version carried in `ConnectRequest` and `Challenge`. `0` means plain. Version 2
/// (ADR-2416) adds rekeying: the top byte of a record's counter field is the sender's key epoch,
/// which a version 1 peer would read as part of the counter.
pub const CRYPTO_VERSION: u8 = 2;
/// Bytes of the header in front of a sealed datagram: magic, kind, session token, counter.
pub const SEALED_HEADER_BYTES: usize = crate::handshake::CONNECTED_HEADER_BYTES + 8;
/// Bytes a sealed datagram adds to the plain form of the same packet: counter, inner kind, tag.
pub const SEALED_OVERHEAD_BYTES: usize = 8 + 1 + 16;
/// The endpoint header length a sealed connection reserves in every packet.
pub const SEALED_RESERVED_HEADER_BYTES: usize =
    crate::handshake::CONNECTED_HEADER_BYTES + SEALED_OVERHEAD_BYTES;
/// A client that only reads the plain field of a `ConnectRequest` offers this.
pub const PLAIN_VERSION: u8 = 0;

/// Whether a link is encrypted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CryptoMode {
    /// Never encrypt. A peer that requires encryption is refused.
    Off,
    /// Require encryption for non-loopback peers; loopback stays plain unless the other side
    /// requires it.
    #[default]
    Auto,
    /// Require encryption for every peer, loopback included.
    Required,
}

impl CryptoMode {
    /// True when this side refuses a plain link to `peer`.
    pub fn requires(self, peer: SocketAddr) -> bool {
        match self {
            Self::Off => false,
            Self::Auto => !is_loopback(peer),
            Self::Required => true,
        }
    }

    /// True when this side can run the suite at all.
    pub fn can_encrypt(self) -> bool {
        self != Self::Off
    }

    /// Lowercase name for `server.toml`, logs and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Required => "required",
        }
    }

    /// The mode for a lowercase name.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "off" => Self::Off,
            "auto" => Self::Auto,
            "required" => Self::Required,
            _ => return None,
        })
    }
}

/// True for a loopback address, including IPv4-mapped IPv6 (`::ffff:127.0.0.1`).
pub fn is_loopback(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// True when `mode` leaves a link to `peer` plain whatever the other side does, so a secret must
/// not cross it. A loopback peer stays on this machine.
pub fn secret_would_be_plain(mode: CryptoMode, peer: SocketAddr) -> bool {
    mode == CryptoMode::Off && !is_loopback(peer)
}

/// What a server does with one peer's offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiated {
    /// A plain link.
    Plain,
    /// A sealed link at [`CRYPTO_VERSION`].
    Sealed,
    /// Refuse the join.
    Refuse(crate::handshake::DenyReason),
}

/// Decides one link from the server's mode, whether it can run the suite at all, the peer's
/// address and the client's offer. Plain is chosen only when neither side requires a seal. A
/// client that requires one ignores a plain challenge and keeps waiting, so this function cannot
/// be talked into a downgrade.
pub fn negotiate(
    mode: CryptoMode,
    can_encrypt: bool,
    peer: SocketAddr,
    offered_version: u8,
    client_required: bool,
) -> Negotiated {
    use crate::handshake::DenyReason;
    let server_required = mode.requires(peer);
    let required = server_required || client_required;
    if offered_version == PLAIN_VERSION {
        return if server_required {
            Negotiated::Refuse(DenyReason::EncryptionRequired)
        } else {
            Negotiated::Plain
        };
    }
    if !can_encrypt {
        // A server that requires a seal but cannot run the suite (no identity) refuses the join
        // instead of falling back to a plain link (audit-v035-net L1). Plain is only for a link
        // neither side requires sealed. A client that requires a seal ignores that challenge.
        return if server_required {
            Negotiated::Refuse(DenyReason::CryptoVersionMismatch)
        } else {
            Negotiated::Plain
        };
    }
    if offered_version != CRYPTO_VERSION {
        return if required {
            Negotiated::Refuse(DenyReason::CryptoVersionMismatch)
        } else {
            Negotiated::Plain
        };
    }
    if required {
        Negotiated::Sealed
    } else {
        Negotiated::Plain
    }
}

/// Why a crypto operation failed. Every failure drops the datagram or ends the handshake; none
/// panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CryptoError {
    /// The datagram did not authenticate: tampered, truncated or under another key.
    #[error("the datagram did not authenticate")]
    Auth,
    /// The counter was already seen or fell behind the replay window.
    #[error("the datagram is a replay")]
    Replay,
    /// The bytes are too short or have an unknown inner kind.
    #[error("the sealed datagram is malformed")]
    Malformed,
    /// The session used every counter.
    #[error("the session used every counter")]
    Exhausted,
    /// The server's key is not the pinned key.
    #[error("the server's key is not the pinned key")]
    KeyMismatch,
    /// The record does not fit a datagram.
    #[error("the record does not fit a datagram")]
    TooLarge,
    /// The handshake could not run (no system randomness, bad parameters).
    #[error("the handshake could not run")]
    Unavailable,
    /// A key in a player proof is the zero key or another low-order point: the X25519 output is
    /// all zeros whatever the hub's key is, so the proof shows no private key (ADR-8665).
    #[error("a key in the proof is a low-order point")]
    WeakKey,
}

impl CryptoError {
    /// Lowercase code for logs and the agent protocol.
    pub fn code(self) -> &'static str {
        match self {
            Self::Auth => "auth_failed",
            Self::Replay => "replay",
            Self::Malformed => "malformed",
            Self::Exhausted => "counter_exhausted",
            Self::KeyMismatch => "server_key_mismatch",
            Self::TooLarge => "record_too_large",
            Self::Unavailable => "crypto_unavailable",
            Self::WeakKey => "weak_key",
        }
    }
}

/// The Noise prologue: both sides hash what they saw of the plain negotiation, so a change to any
/// of it fails the handshake (Noise revision 34 § 6, channel binding of prior messages).
pub fn prologue(
    protocol_id: u64,
    client_nonce: u64,
    version: u8,
    cookie: &[u8; 16],
    password: Option<&PasswordChallenge>,
) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("regia_net noise prologue v1");
    h.update(&protocol_id.to_le_bytes());
    h.update(&client_nonce.to_le_bytes());
    h.update(&[version]);
    h.update(cookie);
    match password {
        None => {
            h.update(&[0]);
        }
        Some(p) => {
            h.update(&[1]);
            h.update(&p.salt);
            h.update(&p.nonce);
        }
    }
    *h.finalize().as_bytes()
}

/// The password nonce of one sealed session: the server's nonce mixed with the Noise handshake
/// hash, so a proof made for one session is useless on any other (channel binding).
pub fn bind_password_nonce(nonce: &[u8; 16], handshake_hash: &[u8; 32]) -> [u8; 16] {
    let mut h = blake3::Hasher::new_derive_key("regia_net password channel binding v1");
    h.update(nonce);
    h.update(handshake_hash);
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}

/// `N` bytes from the operating system's random source.
pub(crate) fn os_random<const N: usize>() -> Option<[u8; N]> {
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    let mut rng = DefaultResolver.resolve_rng()?;
    let mut out = [0u8; N];
    rng.try_fill_bytes(&mut out).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_requires_every_non_loopback_peer() {
        let lan = SocketAddr::from(([192, 168, 1, 5], 7777));
        let public = SocketAddr::from(([8, 8, 8, 8], 7777));
        let local = SocketAddr::from(([127, 0, 0, 1], 7777));
        let local_other = SocketAddr::from(([127, 9, 9, 9], 7777));
        let local6 = SocketAddr::from(([0u16, 0, 0, 0, 0, 0, 0, 1], 7777));
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:7777".parse().expect("addr");
        let mapped_public: SocketAddr = "[::ffff:8.8.8.8]:7777".parse().expect("addr");
        for peer in [lan, public, mapped_public] {
            assert!(CryptoMode::default().requires(peer), "{peer}");
        }
        for peer in [local, local_other, local6, mapped] {
            assert!(!CryptoMode::default().requires(peer), "{peer}");
            assert!(CryptoMode::Required.requires(peer), "{peer}");
        }
        assert!(!CryptoMode::Off.requires(public));
    }

    #[test]
    fn negotiation_table() {
        use crate::handshake::DenyReason::{CryptoVersionMismatch, EncryptionRequired};
        use CryptoMode::{Auto, Off, Required};
        use Negotiated::{Plain, Refuse, Sealed};
        let lan = SocketAddr::from(([10, 0, 0, 2], 1));
        let local = SocketAddr::from(([127, 0, 0, 1], 1));
        let v = CRYPTO_VERSION;
        let cases = [
            // Mode, can encrypt, peer, offer, client requires, outcome.
            (Auto, true, lan, v, true, Sealed),
            (Auto, true, lan, v, false, Sealed),
            (Auto, true, local, v, false, Plain),
            (Auto, true, local, v, true, Sealed),
            (Required, true, local, v, false, Sealed),
            (Off, false, lan, v, true, Plain),
            (Off, false, lan, PLAIN_VERSION, false, Plain),
            (Auto, true, lan, PLAIN_VERSION, false, Refuse(EncryptionRequired)),
            (
                Required,
                true,
                local,
                PLAIN_VERSION,
                false,
                Refuse(EncryptionRequired),
            ),
            (Auto, true, local, PLAIN_VERSION, false, Plain),
            (Auto, true, lan, 9, true, Refuse(CryptoVersionMismatch)),
            (Auto, true, lan, 9, false, Refuse(CryptoVersionMismatch)),
            (Auto, true, local, 9, true, Refuse(CryptoVersionMismatch)),
            (Auto, true, local, 9, false, Plain),
            // A server that requires a seal and cannot encrypt refuses (audit-v035-net L1).
            (Required, false, lan, v, true, Refuse(CryptoVersionMismatch)),
            (Required, false, lan, v, false, Refuse(CryptoVersionMismatch)),
            (Required, false, local, v, false, Refuse(CryptoVersionMismatch)),
            (Auto, false, lan, v, false, Refuse(CryptoVersionMismatch)),
            // Nothing on the server side requires a seal: plain (a requiring client ends it).
            (Auto, false, local, v, false, Plain),
            (Off, false, lan, v, false, Plain),
        ];
        for (mode, can, peer, offer, required, expect) in cases {
            assert_eq!(
                negotiate(mode, can, peer, offer, required),
                expect,
                "{mode:?} can {can} {peer} offer {offer} required {required}"
            );
        }
    }

    #[test]
    fn mode_names_round_trip() {
        for mode in [CryptoMode::Off, CryptoMode::Auto, CryptoMode::Required] {
            assert_eq!(CryptoMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(CryptoMode::parse("on"), None);
    }

    #[test]
    fn secrets_stay_off_plain_remote_links_only() {
        let lan = SocketAddr::from(([10, 0, 0, 2], 1));
        let local = SocketAddr::from(([127, 0, 0, 1], 1));
        assert!(secret_would_be_plain(CryptoMode::Off, lan));
        assert!(!secret_would_be_plain(CryptoMode::Off, local));
        assert!(!secret_would_be_plain(CryptoMode::Auto, lan));
        assert!(!secret_would_be_plain(CryptoMode::Required, lan));
    }

    #[test]
    fn prologue_binds_every_negotiated_field() {
        let cookie = [7; 16];
        let pw = PasswordChallenge {
            salt: [1; 16],
            nonce: [2; 16],
        };
        let base = prologue(1, 2, 1, &cookie, Some(&pw));
        assert_eq!(base, prologue(1, 2, 1, &cookie, Some(&pw)));
        assert_ne!(base, prologue(9, 2, 1, &cookie, Some(&pw)), "protocol id");
        assert_ne!(base, prologue(1, 9, 1, &cookie, Some(&pw)), "client nonce");
        assert_ne!(base, prologue(1, 2, 9, &cookie, Some(&pw)), "version");
        assert_ne!(base, prologue(1, 2, 1, &[8; 16], Some(&pw)), "cookie");
        assert_ne!(base, prologue(1, 2, 1, &cookie, None), "password dropped");
        let other = PasswordChallenge {
            salt: [3; 16],
            nonce: [2; 16],
        };
        assert_ne!(base, prologue(1, 2, 1, &cookie, Some(&other)), "salt");
    }

    #[test]
    fn password_binding_depends_on_the_session() {
        assert_ne!(
            bind_password_nonce(&[1; 16], &[2; 32]),
            bind_password_nonce(&[1; 16], &[3; 32])
        );
    }

    #[test]
    fn os_random_gives_different_bytes() {
        let a = os_random::<32>().expect("system randomness");
        let b = os_random::<32>().expect("system randomness");
        assert_ne!(a, b);
    }
}
