// SPDX-License-Identifier: MPL-2.0
//! A player's long-term identity on a lobby hub (NL-05, ADR-8607): an X25519 static key pair, and
//! a proof that binds it to one sealed link.
//!
//! The endpoints' handshake is `Noise_NX` (ADR-1901): the server proves its static key and the
//! client stays anonymous, so the hub's transport sees no client static key of its own. The client
//! therefore sends, inside the sealed link, one `Noise_X` handshake message
//! ([`PLAYER_PROOF_PATTERN`]) to the hub's static key. That message carries the client's static key
//! encrypted and authenticated by `es` and `ss` (Noise revision 34 § 7.4, pattern `X`): only the
//! holder of the client's private key, or the hub, can make it. Its payload is the sealed link's
//! handshake hash (Noise § 11.2, "channel binding"), so a proof made for one link fails on every
//! other link: a recorded or relayed proof is refused, and the player key the hub records is the
//! key the transport that carried it was bound to.
//!
//! Like the rest of `crypto`, this holds no primitive of its own: the message is `snow`'s.

use snow::params::{DHChoice, NoiseParams};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::{Builder, HandshakeState};

use super::{CryptoError, ServerIdentity, ServerKey};
use crate::wire::constant_time_eq;

/// The Noise protocol name of a player proof.
pub const PLAYER_PROOF_PATTERN: &str = "Noise_X_25519_ChaChaPoly_BLAKE2s";
/// The prologue every player proof is bound to.
pub const PLAYER_PROOF_PROLOGUE: &[u8] = b"regia.lobby_hub.player_proof v1";
/// Bytes of a player proof: `e` (32), the encrypted `s` (32 + 16), the encrypted binding
/// (32 + 16).
pub const PLAYER_PROOF_BYTES: usize = 32 + 48 + 48;

/// A player's public key: the player's identity on a hub. Text form: 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct PlayerKey(pub [u8; 32]);

impl PlayerKey {
    /// Parses the text form; upper-case digits are accepted.
    pub fn parse(text: &str) -> Option<Self> {
        ServerKey::parse(text).map(|k| Self(k.0))
    }

    /// The first eight hex digits (logs and reports).
    pub fn short(&self) -> String {
        let mut text = self.to_string();
        text.truncate(8);
        text
    }
}

impl std::fmt::Display for PlayerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&ServerKey(self.0), f)
    }
}

impl std::fmt::Debug for PlayerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlayerKey({})", self.short())
    }
}

/// A player's static key pair. The private half never leaves the process; only proofs do.
#[derive(Clone, PartialEq)]
pub struct PlayerIdentity(ServerIdentity);

impl std::fmt::Debug for PlayerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PlayerIdentity {{ public: {:?}, private: <hidden> }}",
            self.public()
        )
    }
}

impl PlayerIdentity {
    /// The pair for `private`. `None` when the curve code is unavailable.
    pub fn from_private(private: [u8; 32]) -> Option<Self> {
        ServerIdentity::from_private(private).map(Self)
    }

    /// A new key pair from the operating system's random source.
    ///
    /// # Errors
    ///
    /// [`CryptoError::Unavailable`] without system randomness.
    pub fn generate() -> Result<Self, CryptoError> {
        ServerIdentity::generate().map(Self)
    }

    /// Loads the player's key from `path`, or creates it there with a new key when the file does
    /// not exist (KI-NT-044, ADR-10069), so a player keeps one identity across connections and
    /// runs (friends lists and key-bound invites name it). The file is a `server-key` format file
    /// (`docs/formats/server-key.md`, the same three fields, frozen at version 1); it is private
    /// to its owner and a file other local accounts can read is refused, as
    /// [`ServerIdentity::load_or_create`] does. Keep it in the user's data directory, never in a
    /// project or a package.
    ///
    /// # Errors
    ///
    /// The path and the reason: unreadable, not a key file, a newer `format_version`, a public key
    /// that does not belong to the private one, or a file other accounts can access.
    pub fn load_or_create(path: &std::path::Path) -> Result<Self, String> {
        ServerIdentity::load_or_create(path).map(Self)
    }

    /// The public key: the identity the hub records.
    pub fn public(&self) -> PlayerKey {
        PlayerKey(self.0.public().0)
    }

    /// The static pair. Visible to the other `crypto` modules (binding proofs); not a public key
    /// export.
    pub(in crate::crypto) fn static_identity(&self) -> &ServerIdentity {
        &self.0
    }

    /// The proof for the sealed link to the hub whose key is `hub` and whose handshake hash is
    /// `binding`: one `Noise_X` message to `hub` carrying `binding`.
    ///
    /// # Errors
    ///
    /// [`CryptoError::Unavailable`] when the handshake cannot run.
    pub fn prove(&self, hub: &ServerKey, binding: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
        let mut state = Builder::new(params()?)
            .prologue(PLAYER_PROOF_PROLOGUE)
            .and_then(|b| b.local_private_key(self.0.private_bytes()))
            .and_then(|b| b.remote_public_key(&hub.0))
            .and_then(Builder::build_initiator)
            .map_err(|_| CryptoError::Unavailable)?;
        let mut out = vec![0u8; PLAYER_PROOF_BYTES + 16];
        let n = state
            .write_message(binding, &mut out)
            .map_err(|_| CryptoError::Unavailable)?;
        out.truncate(n);
        Ok(out)
    }
}

fn params() -> Result<NoiseParams, CryptoError> {
    PLAYER_PROOF_PATTERN.parse().map_err(|_| CryptoError::Unavailable)
}

fn responder(hub: &ServerIdentity) -> Result<HandshakeState, CryptoError> {
    Builder::new(params()?)
        .prologue(PLAYER_PROOF_PROLOGUE)
        .and_then(|b| b.local_private_key(hub.private_bytes()))
        .and_then(Builder::build_responder)
        .map_err(|_| CryptoError::Unavailable)
}

/// The hub's side: reads `proof` with the hub's `identity` and returns the player key it proves,
/// when its payload is this link's `binding`.
///
/// # Errors
///
/// [`CryptoError::Malformed`] for a proof of the wrong length, [`CryptoError::Auth`] for one that
/// does not authenticate under the hub's key, [`CryptoError::KeyMismatch`] for one bound to another
/// link (checked before the key, ADR-8734), [`CryptoError::WeakKey`] for one whose static or
/// ephemeral key is the zero key or another low-order point (ADR-8665).
pub fn verify_player_proof(
    identity: &ServerIdentity,
    binding: &[u8; 32],
    proof: &[u8],
) -> Result<PlayerKey, CryptoError> {
    if proof.len() != PLAYER_PROOF_BYTES {
        return Err(CryptoError::Malformed);
    }
    let mut state = responder(identity)?;
    let mut payload = [0u8; PLAYER_PROOF_BYTES];
    let n = state
        .read_message(proof, &mut payload)
        .map_err(|_| CryptoError::Auth)?;
    let remote: [u8; 32] = state
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or(CryptoError::Unavailable)?;
    // The binding first (ADR-8734): a proof for another link is refused either way, and this
    // refusal costs no more X25519. The binding is the sender's own link's hash, not a secret.
    if !constant_time_eq(&payload[..n], binding) {
        return Err(CryptoError::KeyMismatch);
    }
    // A low-order key (the zero key among them) makes the X25519 output all zeros whatever the
    // other private key is, so "authenticated by `ss`" proves nothing: anyone can compute zero.
    // RFC 7748 § 6.1 has the receiver check for the all-zero output and abort. The ephemeral is
    // the message's first 32 bytes, in the clear (Noise § 7.4, pattern `X`: `e, es, s, ss`).
    let ephemeral = proof.first_chunk::<32>().ok_or(CryptoError::Malformed)?;
    if !contributes(identity, &remote)? || !contributes(identity, ephemeral)? {
        return Err(CryptoError::WeakKey);
    }
    Ok(PlayerKey(remote))
}

/// True when the X25519 output of the hub's private key and `public` is not all zeros, that is,
/// `public` is not a low-order point (RFC 7748 § 6.1, "check whether the output is the all-zero
/// value"). The comparison does not branch on the secret bytes.
fn contributes(hub: &ServerIdentity, public: &[u8; 32]) -> Result<bool, CryptoError> {
    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .ok_or(CryptoError::Unavailable)?;
    dh.set(hub.private_bytes());
    let mut shared = [0u8; 32];
    dh.dh(public, &mut shared).map_err(|_| CryptoError::Unavailable)?;
    Ok(!constant_time_eq(&shared, &[0u8; 32]))
}

/// A Noise_X initiator whose static public key is a chosen low-order point and whose static DH
/// output is all zeros (the X25519 output of any private key and a low-order point).
///
/// snow 0.10 accepts an all-zero X25519 result, and `Builder::with_resolver` does not check that a
/// static public key matches its private key. A real `PlayerIdentity` cannot build this message:
/// its public key is a clamped base-point multiplication. Review tests use it against
/// `verify_player_proof`.
#[cfg(test)]
mod zero_proof {
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    use snow::types::{Cipher, Dh, Hash, Random};
    use snow::{Builder, Error};

    use super::{CryptoError, PLAYER_PROOF_BYTES, PLAYER_PROOF_PATTERN, PLAYER_PROOF_PROLOGUE, ServerKey};

    /// The static half reports `static_pub` and outputs zeros. The ephemeral half is a real
    /// X25519 key pair when `real_ephemeral` is set (so `es` is a real secret and only `ss` is
    /// forged), else the zero point with zero outputs too.
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
            // The public half stays the chosen point, whatever private key was set.
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

    /// A proof whose static and ephemeral keys are the zero point and whose DH outputs are zero.
    pub(crate) fn forge_zero_key_proof(hub: &ServerKey, binding: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
        forge_low_order_proof(hub, binding, [0; 32], false)
    }

    /// A proof whose static key is `static_pub` (a low-order point) with a zero `ss`, and a real
    /// or zero ephemeral.
    pub(crate) fn forge_low_order_proof(
        hub: &ServerKey,
        binding: &[u8; 32],
        static_pub: [u8; 32],
        real_ephemeral: bool,
    ) -> Result<Vec<u8>, CryptoError> {
        let params = PLAYER_PROOF_PATTERN
            .parse()
            .map_err(|_| CryptoError::Unavailable)?;
        let resolver = ZeroResolver {
            static_pub,
            real_ephemeral,
        };
        let mut state = Builder::with_resolver(params, Box::new(resolver))
            .prologue(PLAYER_PROOF_PROLOGUE)
            .and_then(|b| b.local_private_key(&[1u8; 32]))
            .and_then(|b| b.remote_public_key(&hub.0))
            .and_then(Builder::build_initiator)
            .map_err(|_| CryptoError::Unavailable)?;
        let mut out = vec![0u8; PLAYER_PROOF_BYTES + 16];
        let n = state
            .write_message(binding, &mut out)
            .map_err(|_| CryptoError::Unavailable)?;
        out.truncate(n);
        Ok(out)
    }
}

#[cfg(test)]
pub(crate) use zero_proof::{forge_low_order_proof, forge_zero_key_proof};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proof_names_its_player_and_only_fits_its_link() {
        let hub = ServerIdentity::from_private([3; 32]).expect("curve");
        let ann = PlayerIdentity::from_private([5; 32]).expect("curve");
        let binding = [7u8; 32];
        let proof = ann.prove(&hub.public(), &binding).expect("proof");
        assert_eq!(proof.len(), PLAYER_PROOF_BYTES);
        assert_eq!(verify_player_proof(&hub, &binding, &proof), Ok(ann.public()));
        // Another link's hash: refused.
        assert_eq!(
            verify_player_proof(&hub, &[8; 32], &proof),
            Err(CryptoError::KeyMismatch)
        );
        // Another hub: does not authenticate.
        let other = ServerIdentity::from_private([4; 32]).expect("curve");
        assert_eq!(
            verify_player_proof(&other, &binding, &proof),
            Err(CryptoError::Auth)
        );
        // A flipped bit anywhere: does not authenticate.
        for i in [0, 40, PLAYER_PROOF_BYTES - 1] {
            let mut bad = proof.clone();
            bad[i] ^= 1;
            assert!(verify_player_proof(&hub, &binding, &bad).is_err(), "byte {i}");
        }
        assert_eq!(
            verify_player_proof(&hub, &binding, &proof[..10]),
            Err(CryptoError::Malformed)
        );
        assert_eq!(PlayerKey::parse(&ann.public().to_string()), Some(ann.public()));
    }

    /// The verifier is stateless, and a proof is not a credential for the other role: presenting
    /// the player's own key as the hub does not authenticate.
    #[test]
    fn a_proof_verifies_twice_and_does_not_reflect() {
        let hub = ServerIdentity::from_private([3; 32]).expect("curve");
        let as_hub = ServerIdentity::from_private([5; 32]).expect("curve");
        let ann = PlayerIdentity::from_private([5; 32]).expect("curve");
        let binding = [7u8; 32];
        let proof = ann.prove(&hub.public(), &binding).expect("proof");
        assert_eq!(verify_player_proof(&hub, &binding, &proof), Ok(ann.public()));
        assert_eq!(
            verify_player_proof(&hub, &binding, &proof),
            Ok(ann.public()),
            "a second check of the same proof is the same answer"
        );
        assert_eq!(
            verify_player_proof(&as_hub, &binding, &proof),
            Err(CryptoError::Auth),
            "the player's key is not the hub the proof was made for"
        );
    }

    /// A Noise_X message with a zero static public key and a zero DH output is not a proof of any
    /// player key. Asserts the safe property: it fails on a verifier that accepts `PlayerKey([0; 32])`.
    #[test]
    fn a_zero_dh_proof_is_not_a_player_key() {
        let hub = ServerIdentity::from_private([3; 32]).expect("curve");
        let other = ServerIdentity::from_private([4; 32]).expect("curve");
        let binding = [7u8; 32];
        let other_binding = [8u8; 32];
        let proof = forge_zero_key_proof(&hub.public(), &binding).expect("zero-key builder");
        let again = forge_zero_key_proof(&other.public(), &other_binding).expect("zero-key builder");
        assert_eq!(proof.len(), PLAYER_PROOF_BYTES, "a full-length forged message");
        assert_eq!(again.len(), PLAYER_PROOF_BYTES);
        assert!(
            verify_player_proof(&hub, &binding, &proof).is_err(),
            "accepted {:?} for a zero DH on this hub and binding",
            verify_player_proof(&hub, &binding, &proof)
        );
        assert!(
            verify_player_proof(&other, &other_binding, &again).is_err(),
            "accepted {:?} for a zero DH on another hub and another binding",
            verify_player_proof(&other, &other_binding, &again)
        );
        assert_eq!(
            verify_player_proof(&hub, &binding, &proof),
            Err(CryptoError::WeakKey),
            "the refusal has its own code"
        );
        assert_eq!(CryptoError::WeakKey.code(), "weak_key");
    }

    /// ADR-8734: the binding is compared before the low-order check, so a forged zero-key proof
    /// sent on another link is refused as `KeyMismatch` (no X25519 past the Noise read), and on
    /// its own link as `WeakKey`. Either is refused.
    #[test]
    fn a_wrong_binding_is_refused_before_the_key_check() {
        let hub = ServerIdentity::from_private([3; 32]).expect("curve");
        let binding = [7u8; 32];
        let proof = forge_zero_key_proof(&hub.public(), &binding).expect("zero-key builder");
        assert_eq!(
            verify_player_proof(&hub, &[8; 32], &proof),
            Err(CryptoError::KeyMismatch)
        );
        assert_eq!(
            verify_player_proof(&hub, &binding, &proof),
            Err(CryptoError::WeakKey)
        );
    }

    /// Low-order X25519 points other than zero (RFC 7748 § 6.1's all-zero check exists for
    /// them): `u = 1` and an order-8 point. With a real ephemeral, `es` is a true secret and only
    /// `ss` is forged, so this is the static-key check alone; with a zero ephemeral, both.
    #[test]
    fn a_low_order_static_key_is_not_a_player_key() {
        let hub = ServerIdentity::from_private([3; 32]).expect("curve");
        let binding = [7u8; 32];
        let one = {
            let mut u = [0u8; 32];
            u[0] = 1;
            u
        };
        let order_eight: [u8; 32] = [
            0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f, 0xc4, 0x6a,
            0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16, 0x5f, 0x49, 0xb8, 0x00,
        ];
        for point in [one, order_eight] {
            // The point really is low order: the hub's X25519 with it is all zeros.
            assert_eq!(contributes(&hub, &point), Ok(false), "{point:02x?}");
            for real_ephemeral in [true, false] {
                let proof = forge_low_order_proof(&hub.public(), &binding, point, real_ephemeral)
                    .expect("low-order builder");
                assert_eq!(proof.len(), PLAYER_PROOF_BYTES);
                assert_eq!(
                    verify_player_proof(&hub, &binding, &proof),
                    Err(CryptoError::WeakKey),
                    "point {point:02x?}, real ephemeral {real_ephemeral}"
                );
            }
        }
        // A real player's key contributes, so honest proofs are unchanged.
        let ann = PlayerIdentity::from_private([5; 32]).expect("curve");
        assert_eq!(contributes(&hub, &ann.public().0), Ok(true));
    }
}

#[cfg(test)]
mod identity_file {
    use super::PlayerIdentity;

    /// KI-NT-044 (ADR-10069): the player's key survives runs in a `server-key` format file.
    #[test]
    fn a_player_identity_file_keeps_one_key_across_loads() {
        let dir = regia_test_utils::unique_temp_dir("player-key");
        let path = dir.join("player.key");
        let first = PlayerIdentity::load_or_create(&path).expect("created");
        assert!(path.exists(), "no file was written");
        let again = PlayerIdentity::load_or_create(&path).expect("loaded");
        assert_eq!(again.public(), first.public(), "a second load made a new key");
        let text = std::fs::read_to_string(&path).expect("read");
        let as_server = crate::crypto::ServerIdentity::from_file_text(&text).expect("a server-key file");
        assert_eq!(as_server.public().0, first.public().0);
        std::fs::write(&path, "not a key").expect("overwrite");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
        }
        assert!(
            PlayerIdentity::load_or_create(&path).is_err(),
            "a broken file was replaced or accepted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
