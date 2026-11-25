// SPDX-License-Identifier: MPL-2.0
//! Owner-to-server binding proofs and server-presence proofs (NL-07, spec 89 § 16.3).
//!
//! Both are one `Noise_X` message to the hub's static key, the same construction as
//! [`super::verify_player_proof`], with a different prologue and payload. This module holds no
//! primitive of its own: `snow` builds the message (Noise revision 34 § 7.4, pattern `X`).
//! A low-order static or ephemeral key is refused after the payload check, by the all-zero X25519
//! output test (RFC 7748 § 6.1).

use snow::params::{DHChoice, NoiseParams};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::{Builder, HandshakeState};

use super::{CryptoError, PlayerIdentity, PlayerKey, ServerIdentity, ServerKey};
use crate::wire::constant_time_eq;

/// Prologue for an owner-to-server binding proof. The hub constant is the same bytes.
pub const BINDING_PROLOGUE: &[u8] = b"regia.lobby_hub.server_binding v1";
/// Prologue for a server-presence proof. The hub constant is the same bytes.
pub const PRESENCE_PROLOGUE: &[u8] = b"regia.lobby_hub.server_presence v1";
/// Noise pattern both proofs use.
pub const BINDING_PATTERN: &str = "Noise_X_25519_ChaChaPoly_BLAKE2s";
/// Fixed binding payload: `version u8 = 1 || server_key [32] || expiry_ms u64 || serial u64`.
pub const BINDING_PAYLOAD_BYTES: usize = 49;
/// One binding proof: `e` (32), encrypted static (48), encrypted payload (49 + 16).
pub const BINDING_PROOF_BYTES: usize = 32 + 48 + (BINDING_PAYLOAD_BYTES + 16);

fn params() -> Result<NoiseParams, CryptoError> {
    BINDING_PATTERN.parse().map_err(|_| CryptoError::Unavailable)
}

fn initiator(
    prologue: &[u8],
    local: &ServerIdentity,
    hub: &ServerKey,
) -> Result<HandshakeState, CryptoError> {
    Builder::new(params()?)
        .prologue(prologue)
        .and_then(|b| b.local_private_key(local.private_bytes()))
        .and_then(|b| b.remote_public_key(&hub.0))
        .and_then(Builder::build_initiator)
        .map_err(|_| CryptoError::Unavailable)
}

fn responder(prologue: &[u8], hub: &ServerIdentity) -> Result<HandshakeState, CryptoError> {
    Builder::new(params()?)
        .prologue(prologue)
        .and_then(|b| b.local_private_key(hub.private_bytes()))
        .and_then(Builder::build_responder)
        .map_err(|_| CryptoError::Unavailable)
}

/// True when the X25519 output of the hub's private key and `public` is not all zeros.
///
/// Copied from the player-proof check (RFC 7748 § 6.1). The comparison does not branch on the
/// secret bytes.
fn contributes(hub: &ServerIdentity, public: &[u8; 32]) -> Result<bool, CryptoError> {
    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .ok_or(CryptoError::Unavailable)?;
    dh.set(hub.private_bytes());
    let mut shared = [0u8; 32];
    dh.dh(public, &mut shared).map_err(|_| CryptoError::Unavailable)?;
    Ok(!constant_time_eq(&shared, &[0u8; 32]))
}

fn read_proof(
    state: &mut HandshakeState,
    proof: &[u8],
    payload_len: usize,
) -> Result<(Vec<u8>, [u8; 32]), CryptoError> {
    let mut payload = vec![0u8; proof.len()];
    let n = state
        .read_message(proof, &mut payload)
        .map_err(|_| CryptoError::Auth)?;
    if n != payload_len {
        return Err(CryptoError::Malformed);
    }
    payload.truncate(n);
    let remote: [u8; 32] = state
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or(CryptoError::Unavailable)?;
    Ok((payload, remote))
}

/// The low-order check. Called only after the payload has been accepted or rejected, so a proof
/// for the wrong payload does not pay this extra X25519 (ADR-8734).
fn reject_low_order(hub: &ServerIdentity, proof: &[u8], remote: &[u8; 32]) -> Result<(), CryptoError> {
    let ephemeral = proof.first_chunk::<32>().ok_or(CryptoError::Malformed)?;
    if !contributes(hub, remote)? || !contributes(hub, ephemeral)? {
        return Err(CryptoError::WeakKey);
    }
    Ok(())
}

/// The owner's binding proof: one `Noise_X` message to `hub` carrying the 49-byte payload.
///
/// # Errors
///
/// [`CryptoError::Unavailable`] when the handshake cannot run.
pub fn prove_binding(
    owner: &PlayerIdentity,
    hub: &ServerKey,
    payload: &[u8; BINDING_PAYLOAD_BYTES],
) -> Result<Vec<u8>, CryptoError> {
    let mut state = initiator(BINDING_PROLOGUE, owner.static_identity(), hub)?;
    let mut out = vec![0u8; BINDING_PROOF_BYTES + 16];
    let n = state
        .write_message(payload, &mut out)
        .map_err(|_| CryptoError::Unavailable)?;
    out.truncate(n);
    Ok(out)
}

/// The hub's side of a binding proof. The payload is returned as the fixed 49 bytes.
///
/// A proof of the wrong length is refused before `snow`. A payload that is not 49 bytes is
/// refused before the low-order check.
///
/// # Errors
///
/// [`CryptoError::Malformed`] for a bad length, [`CryptoError::Auth`] when the hub's key does not
/// open it, [`CryptoError::WeakKey`] for a low-order static or ephemeral key.
pub fn verify_binding_proof(
    hub: &ServerIdentity,
    proof: &[u8],
) -> Result<(PlayerKey, [u8; BINDING_PAYLOAD_BYTES]), CryptoError> {
    if proof.len() != BINDING_PROOF_BYTES {
        return Err(CryptoError::Malformed);
    }
    let mut state = responder(BINDING_PROLOGUE, hub)?;
    let (payload, remote) = read_proof(&mut state, proof, BINDING_PAYLOAD_BYTES)?;
    reject_low_order(hub, proof, &remote)?;
    let mut fixed = [0u8; BINDING_PAYLOAD_BYTES];
    fixed.copy_from_slice(&payload);
    Ok((PlayerKey(remote), fixed))
}

/// The server's presence proof: one `Noise_X` message to `hub` whose payload is the hub link's
/// handshake hash.
///
/// # Errors
///
/// [`CryptoError::Unavailable`] when the handshake cannot run.
pub fn prove_server_presence(
    server: &ServerIdentity,
    hub: &ServerKey,
    handshake_hash: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    let mut state = initiator(PRESENCE_PROLOGUE, server, hub)?;
    let mut out = vec![0u8; super::PLAYER_PROOF_BYTES + 16];
    let n = state
        .write_message(handshake_hash, &mut out)
        .map_err(|_| CryptoError::Unavailable)?;
    out.truncate(n);
    Ok(out)
}

/// The hub's side of a server-presence proof.
///
/// The handshake-hash compare is constant-time and runs before the low-order check (ADR-8734).
///
/// # Errors
///
/// [`CryptoError::Malformed`] for a bad length, [`CryptoError::Auth`] when the proof does not open,
/// [`CryptoError::KeyMismatch`] when the payload is another link's hash, [`CryptoError::WeakKey`]
/// for a low-order key.
pub fn verify_server_presence_proof(
    hub: &ServerIdentity,
    handshake_hash: &[u8; 32],
    proof: &[u8],
) -> Result<ServerKey, CryptoError> {
    if proof.len() != super::PLAYER_PROOF_BYTES {
        return Err(CryptoError::Malformed);
    }
    let mut state = responder(PRESENCE_PROLOGUE, hub)?;
    let (payload, remote) = read_proof(&mut state, proof, 32)?;
    if !constant_time_eq(&payload, handshake_hash) {
        return Err(CryptoError::KeyMismatch);
    }
    reject_low_order(hub, proof, &remote)?;
    Ok(ServerKey(remote))
}

/// A binding proof whose static key is `static_pub` (a low-order point) and whose static DH
/// output is all zeros. A real [`PlayerIdentity`] cannot build this: its public key is a clamped
/// base-point multiplication. Review tests use it against [`verify_binding_proof`].
#[doc(hidden)]
pub fn forge_low_order_binding(
    hub: &ServerKey,
    payload: &[u8; BINDING_PAYLOAD_BYTES],
    static_pub: [u8; 32],
    real_ephemeral: bool,
) -> Result<Vec<u8>, CryptoError> {
    forge::forge(
        BINDING_PROLOGUE,
        hub,
        payload,
        static_pub,
        real_ephemeral,
        BINDING_PROOF_BYTES,
    )
}

/// A server-presence proof whose static key is the zero point. Review tests only.
#[doc(hidden)]
pub fn forge_low_order_presence(
    hub: &ServerKey,
    handshake_hash: &[u8; 32],
    static_pub: [u8; 32],
    real_ephemeral: bool,
) -> Result<Vec<u8>, CryptoError> {
    forge::forge(
        PRESENCE_PROLOGUE,
        hub,
        handshake_hash,
        static_pub,
        real_ephemeral,
        super::PLAYER_PROOF_BYTES,
    )
}

/// The low-order initiator. Same shape as the player-proof forge: `snow` accepts an all-zero
/// X25519 result, and `Builder::with_resolver` does not check that a static public key matches
/// its private key.
mod forge {
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    use snow::types::{Cipher, Dh, Hash, Random};
    use snow::{Builder, Error};

    use super::{BINDING_PATTERN, CryptoError, ServerKey};

    struct ZeroDh {
        static_pub: [u8; 32],
        real_ephemeral: bool,
        inner: Box<dyn Dh>,
        privkey: [u8; 32],
        pubkey: [u8; 32],
        real: bool,
    }

    impl Dh for ZeroDh {
        fn name(&self) -> &'static str {
            "25519"
        }

        fn pub_len(&self) -> usize {
            32
        }

        fn priv_len(&self) -> usize {
            32
        }

        fn set(&mut self, privkey: &[u8]) {
            self.privkey = [0; 32];
            let n = privkey.len().min(32);
            self.privkey[..n].copy_from_slice(&privkey[..n]);
            self.pubkey = self.static_pub;
            self.real = false;
        }

        fn generate(&mut self, rng: &mut dyn Random) -> Result<(), Error> {
            if self.real_ephemeral {
                self.inner.generate(rng)?;
                self.pubkey.copy_from_slice(self.inner.pubkey());
                self.real = true;
            } else {
                self.pubkey = [0; 32];
            }
            Ok(())
        }

        fn pubkey(&self) -> &[u8] {
            &self.pubkey
        }

        fn privkey(&self) -> &[u8] {
            &self.privkey
        }

        fn dh(&self, pubkey: &[u8], out: &mut [u8]) -> Result<(), Error> {
            if self.real {
                return self.inner.dh(pubkey, out);
            }
            out.fill(0);
            Ok(())
        }
    }

    struct ZeroResolver {
        static_pub: [u8; 32],
        real_ephemeral: bool,
    }

    impl CryptoResolver for ZeroResolver {
        fn resolve_rng(&self) -> Option<Box<dyn Random>> {
            DefaultResolver.resolve_rng()
        }

        fn resolve_dh(&self, choice: &snow::params::DHChoice) -> Option<Box<dyn Dh>> {
            Some(Box::new(ZeroDh {
                static_pub: self.static_pub,
                real_ephemeral: self.real_ephemeral,
                inner: DefaultResolver.resolve_dh(choice)?,
                privkey: [0; 32],
                pubkey: [0; 32],
                real: false,
            }))
        }

        fn resolve_hash(&self, choice: &snow::params::HashChoice) -> Option<Box<dyn Hash>> {
            DefaultResolver.resolve_hash(choice)
        }

        fn resolve_cipher(&self, choice: &snow::params::CipherChoice) -> Option<Box<dyn Cipher>> {
            DefaultResolver.resolve_cipher(choice)
        }
    }

    pub(super) fn forge(
        prologue: &[u8],
        hub: &ServerKey,
        payload: &[u8],
        static_pub: [u8; 32],
        real_ephemeral: bool,
        out_len: usize,
    ) -> Result<Vec<u8>, CryptoError> {
        let params = BINDING_PATTERN.parse().map_err(|_| CryptoError::Unavailable)?;
        let resolver = ZeroResolver {
            static_pub,
            real_ephemeral,
        };
        let mut state = Builder::with_resolver(params, Box::new(resolver))
            .prologue(prologue)
            .and_then(|b| b.local_private_key(&[1u8; 32]))
            .and_then(|b| b.remote_public_key(&hub.0))
            .and_then(Builder::build_initiator)
            .map_err(|_| CryptoError::Unavailable)?;
        let mut out = vec![0u8; out_len + 16];
        let n = state
            .write_message(payload, &mut out)
            .map_err(|_| CryptoError::Unavailable)?;
        out.truncate(n);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binding_names_its_owner_and_a_zero_key_does_not() {
        let hub = ServerIdentity::from_private([3; 32]).expect("curve");
        let owner = PlayerIdentity::from_private([5; 32]).expect("curve");
        let payload = [1u8; BINDING_PAYLOAD_BYTES];
        let proof = prove_binding(&owner, &hub.public(), &payload).expect("proof");
        assert_eq!(proof.len(), BINDING_PROOF_BYTES);
        let (key, got) = verify_binding_proof(&hub, &proof).expect("opens");
        assert_eq!(key, owner.public());
        assert_eq!(got, payload);
        let forged = forge_low_order_binding(&hub.public(), &payload, [0; 32], false).expect("forge");
        assert_eq!(verify_binding_proof(&hub, &forged), Err(CryptoError::WeakKey));
    }

    #[test]
    fn a_presence_proof_fits_only_its_link() {
        let hub = ServerIdentity::from_private([4; 32]).expect("curve");
        let server = ServerIdentity::from_private([6; 32]).expect("curve");
        let hash = [9u8; 32];
        let proof = prove_server_presence(&server, &hub.public(), &hash).expect("proof");
        assert_eq!(proof.len(), super::super::PLAYER_PROOF_BYTES);
        assert_eq!(
            verify_server_presence_proof(&hub, &hash, &proof),
            Ok(server.public())
        );
        assert_eq!(
            verify_server_presence_proof(&hub, &[8; 32], &proof),
            Err(CryptoError::KeyMismatch)
        );
    }
}
