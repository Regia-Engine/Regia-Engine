// SPDX-License-Identifier: MPL-2.0
//! Delegated-server binding bytes (spec 89 § 16.3, ADR-10378).
//!
//! This module owns the fixed 49-byte payload and the hub's check of a binding or presence proof.
//! Noise runs in `crypto::binding`. A revocation set lives on the hub core.

use super::HubError;
use crate::crypto::{PlayerKey, ServerIdentity, ServerKey};
use crate::wire::{WireError, WireReader, WireWriter};

/// Noise prologue for an owner-to-server binding proof.
pub const SERVER_BINDING_PROLOGUE: &[u8] = b"regia.lobby_hub.server_binding v1";
/// Noise prologue for the server session's presence proof.
pub const SERVER_PRESENCE_PROLOGUE: &[u8] = b"regia.lobby_hub.server_presence v1";
/// Fixed binding payload: `version u8 = 1 || server_key [32] || expiry_ms u64 || serial u64`.
pub const BINDING_PAYLOAD_BYTES: usize = 49;
/// One binding proof on the wire (a Noise `X` message). Read into a stack buffer of this size.
pub const BINDING_PROOF_BYTES: usize = 145;
/// Most owner proofs one `RegisterServer` carries.
pub const MAX_BINDING_OWNERS: usize = 8;
/// Most ban keys on one server presence.
pub const MAX_SERVER_BANS: usize = 256;
/// Most ban keys on one lobby.
pub const MAX_LOBBY_BANS: usize = 64;
/// Longest game-version string on a server presence or a server card.
pub const MAX_HUB_GAME_VERSION_BYTES: usize = 32;
/// Grace a server omits. Behavior lanes apply it; the codec does not invent it.
pub const GRACE_MS_DEFAULT: u32 = 600_000;
/// Largest grace duration on the wire, 24 hours. `0` means no deadline.
pub const GRACE_MS_MAX: u32 = 86_400_000;
/// Furthest a binding expiry may sit past `now_ms`: 366 days. Farther is `hub_binding_invalid`.
pub const BINDING_MAX_HORIZON_MS: u64 = 366 * 24 * 60 * 60 * 1000;

const _: () = assert!(MAX_HUB_GAME_VERSION_BYTES == crate::version_probe::MAX_GAME_VERSION_BYTES);

/// `true` when `ms` is `0` (no deadline) or `1..=GRACE_MS_MAX`.
///
/// The decoder rejects anything else (ADR-10378). Spec 89 § 16.2 says `RegisterServer` clamps a
/// larger value to 24 h; that clamp belongs to the behavior lane, and only for a value this
/// function already accepted.
#[must_use]
pub fn grace_in_range(ms: u32) -> bool {
    ms == 0 || (1..=GRACE_MS_MAX).contains(&ms)
}

/// The fixed payload inside a binding proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingClaims {
    /// Payload version. Only `1` is legal.
    pub version: u8,
    /// The dedicated server this owner binds.
    pub server_key: ServerKey,
    /// Expiry on the hub clock, milliseconds. `0` means no expiry.
    pub expiry_ms: u64,
    /// The owner's revocation id for this proof.
    pub serial: u64,
}

/// What a server publishes with `RegisterServer`. Proof bytes are not checked here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPresence {
    /// The server-presence Noise message, exactly [`crate::crypto::PLAYER_PROOF_BYTES`] bytes.
    pub server_proof: Vec<u8>,
    /// Owner binding proofs, each exactly [`BINDING_PROOF_BYTES`] bytes. Empty means ownerless.
    pub proofs: Vec<Vec<u8>>,
    /// Seats the server will take. `0` is legal on the wire; the behavior lane checks the range.
    pub capacity: u16,
    /// Seats in use.
    pub load: u16,
    /// The server's game protocol number.
    pub protocol: u32,
    /// The server's game version text, at most [`MAX_HUB_GAME_VERSION_BYTES`] bytes.
    pub game_version: String,
    /// When set, the server offers only the relay.
    pub relay_only: bool,
    /// Grace after the owner leaves, milliseconds. `0` means no deadline.
    pub grace_ms: u32,
    /// Player keys this server refuses. At most [`MAX_SERVER_BANS`].
    pub bans: Vec<PlayerKey>,
}

/// Writes the 49-byte payload. A version other than 1 is refused.
///
/// # Errors
///
/// [`WireError::Invalid`] when `claims.version` is not 1.
pub fn encode_binding_payload(claims: &BindingClaims) -> Result<[u8; BINDING_PAYLOAD_BYTES], WireError> {
    if claims.version != 1 {
        return Err(WireError::Invalid("hub binding version"));
    }
    let mut w = WireWriter::new();
    w.u8(claims.version);
    w.raw(&claims.server_key.0);
    w.u64(claims.expiry_ms);
    w.u64(claims.serial);
    let bytes = w.finish_checked()?;
    let mut out = [0u8; BINDING_PAYLOAD_BYTES];
    if bytes.len() != BINDING_PAYLOAD_BYTES {
        return Err(WireError::Invalid("hub binding payload"));
    }
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Reads a 49-byte payload. A short buffer is [`WireError::Truncated`]. Extra bytes are
/// [`WireError::Trailing`]. Version must be 1.
///
/// # Errors
///
/// [`WireError`] when the buffer is not the fixed layout.
pub fn decode_binding_payload(bytes: &[u8]) -> Result<BindingClaims, WireError> {
    let mut r = WireReader::new(bytes);
    let version = r.u8()?;
    if version != 1 {
        return Err(WireError::Invalid("hub binding version"));
    }
    let claims = BindingClaims {
        version,
        server_key: ServerKey(r.array()?),
        expiry_ms: r.u64()?,
        serial: r.u64()?,
    };
    r.finish()?;
    Ok(claims)
}

/// Verifies one owner binding proof and returns the owner key plus the payload.
///
/// `expiry_ms == 0` does not expire. Any other expiry must be `<= now_ms + 366 days` or the proof
/// is [`HubError::BindingInvalid`]. An expiry that is already past is [`HubError::BindingExpired`].
/// A proof that does not open, or a low-order key, is [`HubError::BindingInvalid`].
///
/// # Errors
///
/// [`HubError::BindingInvalid`] or [`HubError::BindingExpired`].
pub fn verify_binding(
    identity: &ServerIdentity,
    proof: &[u8],
    now_ms: u64,
) -> Result<(PlayerKey, BindingClaims), HubError> {
    let (player, payload) = crate::crypto::binding::verify_binding_proof(identity, proof)
        .map_err(|_| HubError::BindingInvalid)?;
    let claims = decode_binding_payload(&payload).map_err(|_| HubError::BindingInvalid)?;
    if claims.expiry_ms != 0 {
        let horizon = now_ms.saturating_add(BINDING_MAX_HORIZON_MS);
        if claims.expiry_ms > horizon {
            return Err(HubError::BindingInvalid);
        }
        if claims.expiry_ms <= now_ms {
            return Err(HubError::BindingExpired);
        }
    }
    Ok((player, claims))
}

/// Verifies the server-presence proof against the hub link's handshake hash.
///
/// A proof that does not open, a hash for another link, or a low-order key is
/// [`HubError::BindingInvalid`]. The link stays up; this is not `hub_identity_mismatch`.
///
/// # Errors
///
/// [`HubError::BindingInvalid`].
pub fn verify_server_presence(
    identity: &ServerIdentity,
    handshake_hash: &[u8; 32],
    proof: &[u8],
) -> Result<ServerKey, HubError> {
    crate::crypto::binding::verify_server_presence_proof(identity, handshake_hash, proof)
        .map_err(|_| HubError::BindingInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_is_49_bytes_and_round_trips() {
        let claims = BindingClaims {
            version: 1,
            server_key: ServerKey([0x44; 32]),
            expiry_ms: 0,
            serial: 9,
        };
        let bytes = encode_binding_payload(&claims).expect("encodes");
        assert_eq!(bytes.len(), BINDING_PAYLOAD_BYTES);
        assert_eq!(decode_binding_payload(&bytes).expect("decodes"), claims);
        assert!(matches!(
            decode_binding_payload(&bytes[..48]),
            Err(WireError::Truncated { .. })
        ));
        let mut extra = bytes.to_vec();
        extra.push(0);
        assert!(matches!(
            decode_binding_payload(&extra),
            Err(WireError::Trailing(1))
        ));
        assert!(encode_binding_payload(&BindingClaims { version: 2, ..claims }).is_err());
    }

    #[test]
    fn grace_rejects_a_duration_past_24_hours() {
        assert!(grace_in_range(0));
        assert!(grace_in_range(1));
        assert!(grace_in_range(GRACE_MS_DEFAULT));
        assert!(grace_in_range(GRACE_MS_MAX));
        assert!(!grace_in_range(GRACE_MS_MAX + 1));
    }

    #[test]
    fn a_binding_round_trips_and_a_zero_key_is_invalid() {
        use crate::crypto::{PlayerIdentity, binding};
        let hub = ServerIdentity::from_private([3; 32]).expect("curve");
        let owner = PlayerIdentity::from_private([5; 32]).expect("curve");
        let claims = BindingClaims {
            version: 1,
            server_key: ServerKey([0x44; 32]),
            expiry_ms: 0,
            serial: 9,
        };
        let payload = encode_binding_payload(&claims).expect("payload");
        let proof = binding::prove_binding(&owner, &hub.public(), &payload).expect("proof");
        assert_eq!(verify_binding(&hub, &proof, 1_000), Ok((owner.public(), claims)));
        assert_eq!(
            binding::BINDING_PROLOGUE,
            SERVER_BINDING_PROLOGUE,
            "the crypto prologue is the hub prologue"
        );
        assert_eq!(binding::PRESENCE_PROLOGUE, SERVER_PRESENCE_PROLOGUE);
        let forged = binding::forge_low_order_binding(&hub.public(), &payload, [0; 32], true).expect("forge");
        assert_eq!(
            verify_binding(&hub, &forged, 1_000)
                .expect_err("low order")
                .code(),
            "hub_binding_invalid"
        );
        let soon = BindingClaims {
            expiry_ms: 50,
            ..claims
        };
        let soon_payload = encode_binding_payload(&soon).expect("payload");
        let soon_proof = binding::prove_binding(&owner, &hub.public(), &soon_payload).expect("proof");
        assert_eq!(
            verify_binding(&hub, &soon_proof, 50).expect_err("past").code(),
            "hub_binding_expired"
        );
        let far = BindingClaims {
            expiry_ms: 1_000u64.saturating_add(BINDING_MAX_HORIZON_MS).saturating_add(1),
            ..claims
        };
        let far_payload = encode_binding_payload(&far).expect("payload");
        let far_proof = binding::prove_binding(&owner, &hub.public(), &far_payload).expect("proof");
        assert_eq!(
            verify_binding(&hub, &far_proof, 1_000)
                .expect_err("horizon")
                .code(),
            "hub_binding_invalid"
        );
    }
}
