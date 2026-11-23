// SPDX-License-Identifier: MPL-2.0
//! Optional host password, stored only as a salted hash.
//!
//! The host keeps `verifier = BLAKE3-derive-key("regia_net password verifier v1", salt || password)`
//! and never the password. The challenge it sends carries the salt and a fresh nonce; the client
//! answers `proof = BLAKE3-keyed(verifier, nonce)`. The nonce is bound to the client's IP (not its
//! port, so a NAT port change mid-handshake does not break the join), its request nonce and the
//! cookie's time bucket (`crate::handshake::CookieMinter`), so a captured proof is useless from
//! another IP and expires with the cookie window. From another port of the same IP inside that
//! window the proof would still verify; the server refuses such a copy because it remembers the
//! (IP, request nonce) of every join answer that made a peer until the window closes, so one
//! answer makes at most one peer.
//!
//! This keeps casual strangers out of a private match. It is not an account system: anyone who
//! records one challenge and its proof can try passwords offline, so a host password should not be
//! reused elsewhere.

use crate::wire::constant_time_eq;

const VERIFIER_CONTEXT: &str = "regia_net password verifier v1";

/// The host side of a password check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordGate {
    salt: [u8; 16],
    verifier: Option<[u8; 32]>,
}

impl PasswordGate {
    /// No password: every join passes.
    pub fn open() -> Self {
        Self {
            salt: [0; 16],
            verifier: None,
        }
    }

    /// A gate for `password` with `salt` (use a random salt per host session).
    pub fn with_password(password: &str, salt: [u8; 16]) -> Self {
        Self {
            salt,
            verifier: Some(verifier(password, &salt)),
        }
    }

    /// True when no password is set.
    pub fn is_open(&self) -> bool {
        self.verifier.is_none()
    }

    /// The public salt.
    pub fn salt(&self) -> [u8; 16] {
        self.salt
    }

    /// Whether `proof` answers `nonce`. Always `true` for an open gate.
    pub fn accepts(&self, nonce: &[u8; 16], proof: &[u8; 32]) -> bool {
        match &self.verifier {
            None => true,
            Some(v) => constant_time_eq(&blake3::keyed_hash(v, nonce).as_bytes()[..], proof),
        }
    }
}

/// The salted verifier the host stores.
pub fn verifier(password: &str, salt: &[u8; 16]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key(VERIFIER_CONTEXT);
    h.update(salt);
    h.update(password.as_bytes());
    *h.finalize().as_bytes()
}

/// The client's answer to a challenge.
pub fn prove(password: &str, salt: &[u8; 16], nonce: &[u8; 16]) -> [u8; 32] {
    *blake3::keyed_hash(&verifier(password, salt), nonce).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_password_passes_wrong_fails() {
        let gate = PasswordGate::with_password("hunter2", [9; 16]);
        let nonce = [3; 16];
        assert!(gate.accepts(&nonce, &prove("hunter2", &gate.salt(), &nonce)));
        assert!(!gate.accepts(&nonce, &prove("hunter3", &gate.salt(), &nonce)));
        assert!(
            !gate.accepts(&[4; 16], &prove("hunter2", &gate.salt(), &nonce)),
            "proof is bound to the nonce"
        );
    }

    #[test]
    fn salt_changes_the_verifier_and_open_gate_accepts_all() {
        assert_ne!(verifier("pw", &[1; 16]), verifier("pw", &[2; 16]));
        let open = PasswordGate::open();
        assert!(open.is_open());
        assert!(open.accepts(&[0; 16], &[0; 32]));
    }
}
