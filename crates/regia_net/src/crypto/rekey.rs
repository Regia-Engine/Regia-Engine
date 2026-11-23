// SPDX-License-Identifier: MPL-2.0
//! Session rekeying (ADR-2416): each direction of a sealed link moves to a fresh key after a
//! number of records or bytes, with no message and no round trip.
//!
//! The next key is the Noise `REKEY` function of the current one (Noise revision 34 § 4.2 and
//! § 11.3): the first 32 bytes of `ENCRYPT(k, 2^64-1, "", zeros[32])`, computed with the session's
//! own `ChaChaPoly` through `snow`'s cipher resolver. The sender decides when (a [`RekeyPolicy`]
//! counts records and bytes, never time) and writes its key epoch in the top byte of every
//! record's counter field, so the receiver knows which key a record needs without trying them all.
//! Record counters keep running across epochs, so the one replay window covers every key.
//!
//! The receiver keeps three keys at most, as QUIC does for its key phase (RFC 9001 § 6): the
//! previous one for records the network reordered across a switch, the current one, and a short
//! chain of next ones, derived on first need and cached, so a forged epoch costs one failed
//! decryption and never a derivation per datagram. It switches only once a record under a newer
//! key authenticated, so a forged epoch byte moves nothing.

use snow::params::CipherChoice;
use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::types::Cipher;

use serde::{Deserialize, Serialize};

use super::CryptoError;

/// Bytes of a cipher key.
const KEY_BYTES: usize = 32;
/// Bytes of the authentication tag.
const TAG_BYTES: usize = 16;
/// The record counter occupies the low bits of the counter field; the top byte is the key epoch.
pub const EPOCH_SHIFT: u32 = 56;
/// Mask of the record counter inside the counter field.
pub const RECORD_COUNTER_MASK: u64 = (1 << EPOCH_SHIFT) - 1;
/// Keys a receiver may move ahead in one step: a sender that rekeyed this many times while every
/// record of those epochs was lost still gets through. Past it the records fail authentication.
pub const MAX_EPOCH_SKIP: u8 = 16;
/// Records a key protects at least, whatever the policy says, so a lossy link cannot fall more than
/// [`MAX_EPOCH_SKIP`] keys behind without losing `MAX_EPOCH_SKIP * MIN_RECORDS_PER_KEY` records in a
/// row.
pub const MIN_RECORDS_PER_KEY: u64 = 64;
/// Epoch differences at or above this read as an old key (the byte wrapped below the current one).
const STALE_FROM: u8 = 128;

/// When one direction of a sealed link switches to the next key. Both limits count what the
/// sender sealed under the current key; whichever is reached first triggers the switch. Tests and
/// tools set small values; nothing here reads a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RekeyPolicy {
    /// Records sealed under one key before the next (`0`: no record limit). At least
    /// [`MIN_RECORDS_PER_KEY`] records are sealed under every key.
    pub after_records: u64,
    /// Ciphertext bytes (body, inner kind and tag) sealed under one key before the next (`0`: no
    /// byte limit).
    pub after_bytes: u64,
}

impl RekeyPolicy {
    /// Never rekey: one key per direction for the whole session, as before ADR-2416.
    pub const NEVER: Self = Self {
        after_records: 0,
        after_bytes: 0,
    };

    /// True when a key that sealed `records` records and `bytes` bytes is due for replacement.
    pub fn due(&self, records: u64, bytes: u64) -> bool {
        if records < MIN_RECORDS_PER_KEY {
            return false;
        }
        (self.after_records > 0 && records >= self.after_records)
            || (self.after_bytes > 0 && bytes >= self.after_bytes)
    }
}

impl Default for RekeyPolicy {
    /// About a million records or a gibibyte per key, whichever comes first: hours of play at a
    /// typical tick rate, so a key compromised late in a long session exposes only its own share.
    fn default() -> Self {
        Self {
            after_records: 1 << 20,
            after_bytes: 1 << 30,
        }
    }
}

/// One cipher key and the cipher set to it. Never printed.
pub(super) struct Key {
    cipher: Box<dyn Cipher>,
}

impl Key {
    pub(super) fn new(bytes: [u8; KEY_BYTES]) -> Result<Self, CryptoError> {
        let mut cipher = DefaultResolver
            .resolve_cipher(&CipherChoice::ChaChaPoly)
            .ok_or(CryptoError::Unavailable)?;
        cipher.set(&bytes);
        Ok(Self { cipher })
    }

    /// Noise `REKEY(k)` (revision 34 § 4.2): the first 32 bytes of `ENCRYPT(k, maxnonce, "", zeros)`.
    pub(super) fn next(&self) -> Result<Self, CryptoError> {
        let mut out = [0u8; KEY_BYTES + TAG_BYTES];
        let n = self.cipher.encrypt(u64::MAX, &[], &[0u8; KEY_BYTES], &mut out);
        let bytes: [u8; KEY_BYTES] = out
            .get(..KEY_BYTES)
            .filter(|_| n == out.len())
            .and_then(|b| b.try_into().ok())
            .ok_or(CryptoError::Unavailable)?;
        Self::new(bytes)
    }

    /// Seals `plain` under `nonce` into `out` (`plain.len() + 16` bytes).
    pub(super) fn seal(&self, nonce: u64, plain: &[u8], out: &mut [u8]) -> Result<(), CryptoError> {
        if out.len() != plain.len() + TAG_BYTES {
            return Err(CryptoError::Unavailable);
        }
        self.cipher.encrypt(nonce, &[], plain, out);
        Ok(())
    }

    /// Opens `cipher` (at least a tag long) into `out` (as long as `cipher`); the plain length.
    pub(super) fn open(&self, nonce: u64, cipher: &[u8], out: &mut [u8]) -> Result<usize, CryptoError> {
        if cipher.len() < TAG_BYTES || out.len() < cipher.len() {
            return Err(CryptoError::Malformed);
        }
        self.cipher
            .decrypt(nonce, &[], cipher, out)
            .map_err(|_| CryptoError::Auth)
    }

    /// What this key seals for a fixed input: tells keys apart in tests without exposing them.
    #[cfg(test)]
    pub(super) fn fingerprint(&self) -> [u8; 4 + TAG_BYTES] {
        let mut out = [0u8; 4 + TAG_BYTES];
        self.cipher.encrypt(0, &[], b"keys", &mut out);
        out
    }
}

/// The sending direction: one key, its epoch and what it sealed.
pub(super) struct SendKeys {
    key: Key,
    epoch: u32,
    records: u64,
    bytes: u64,
}

impl SendKeys {
    pub(super) fn new(key: Key) -> Self {
        Self {
            key,
            epoch: 0,
            records: 0,
            bytes: 0,
        }
    }

    pub(super) fn epoch(&self) -> u32 {
        self.epoch
    }

    /// The key for the next record, moving to the next epoch first when `policy` says so. The old
    /// key is dropped at once: the sender never needs it again.
    pub(super) fn key_for_next(&mut self, policy: &RekeyPolicy) -> Result<(&Key, u8), CryptoError> {
        if policy.due(self.records, self.bytes) {
            self.key = self.key.next()?;
            self.epoch = self.epoch.wrapping_add(1);
            self.records = 0;
            self.bytes = 0;
        }
        Ok((&self.key, self.epoch as u8))
    }

    /// Counts one sealed record of `bytes` ciphertext bytes.
    pub(super) fn note_sealed(&mut self, bytes: usize) {
        self.records += 1;
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }

    /// Moves to the next key now, whatever the policy says.
    pub(super) fn force_next(&mut self) -> Result<(), CryptoError> {
        self.key = self.key.next()?;
        self.epoch = self.epoch.wrapping_add(1);
        self.records = 0;
        self.bytes = 0;
        Ok(())
    }
}

/// Which receive key a record needs.
pub(super) enum Pick {
    /// The current key.
    Current,
    /// The key before the current one.
    Previous,
    /// The key `n` epochs ahead (1 = the next one).
    Ahead(u8),
}

/// The receiving direction: previous, current and cached next keys.
pub(super) struct RecvKeys {
    current: Key,
    previous: Option<Key>,
    /// `ahead[i]` is the key `i + 1` epochs past the current one.
    ahead: Vec<Key>,
    epoch: u32,
    /// The lowest record counter that authenticated under the current key.
    first_counter: Option<u64>,
}

impl RecvKeys {
    pub(super) fn new(key: Key) -> Self {
        Self {
            current: key,
            previous: None,
            ahead: Vec::new(),
            epoch: 0,
            first_counter: None,
        }
    }

    pub(super) fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Picks the key for a record of `counter` that says it was sealed at `epoch_byte`, from the
    /// clear header only. A record that cannot be from an honest sender is refused here, before
    /// any cipher work: a newer key needs a counter past every record of the current key, an older
    /// one a counter before them.
    pub(super) fn pick(&self, epoch_byte: u8, counter: u64) -> Result<Pick, CryptoError> {
        let d = epoch_byte.wrapping_sub(self.epoch as u8);
        match d {
            0 => Ok(Pick::Current),
            d if d >= STALE_FROM => {
                let is_previous = d == u8::MAX
                    && self.previous.is_some()
                    && self.first_counter.is_some_and(|first| counter < first);
                if is_previous {
                    Ok(Pick::Previous)
                } else {
                    // Two keys or more behind, or a counter the previous key never sealed.
                    Err(CryptoError::Replay)
                }
            }
            d if d <= MAX_EPOCH_SKIP => {
                if self.first_counter.is_some_and(|first| counter <= first) {
                    Err(CryptoError::Auth)
                } else {
                    Ok(Pick::Ahead(d))
                }
            }
            _ => Err(CryptoError::Auth),
        }
    }

    /// The key `pick` names, deriving next keys when the chain is short.
    pub(super) fn key(&mut self, pick: &Pick) -> Result<&Key, CryptoError> {
        match *pick {
            Pick::Current => Ok(&self.current),
            Pick::Previous => self.previous.as_ref().ok_or(CryptoError::Replay),
            Pick::Ahead(n) => {
                let n = usize::from(n);
                while self.ahead.len() < n {
                    let next = self.ahead.last().unwrap_or(&self.current).next()?;
                    self.ahead.push(next);
                }
                self.ahead.get(n - 1).ok_or(CryptoError::Unavailable)
            }
        }
    }

    /// Records that a record of `counter` authenticated under `pick`; a newer key becomes current.
    pub(super) fn commit(&mut self, pick: &Pick, counter: u64) {
        match *pick {
            Pick::Previous => {}
            Pick::Current => {
                self.first_counter = Some(self.first_counter.map_or(counter, |f| f.min(counter)));
            }
            Pick::Ahead(n) => {
                let n = usize::from(n);
                if n == 0 || self.ahead.len() < n {
                    return;
                }
                let mut passed: Vec<Key> = self.ahead.drain(..n).collect();
                let Some(newest) = passed.pop() else {
                    return;
                };
                let old_current = std::mem::replace(&mut self.current, newest);
                // The key just before the new one: the old current key after one step, else the
                // last key passed over.
                self.previous = Some(passed.pop().unwrap_or(old_current));
                self.epoch = self.epoch.wrapping_add(n as u32);
                self.first_counter = Some(counter);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rekey_is_the_noise_rekey_function() {
        // REKEY(k) = ENCRYPT(k, 2^64-1, "", zeros[32])[..32], checked against the cipher's own
        // default `rekey`, which `snow` implements from the same section of the specification.
        let key = Key::new([7; KEY_BYTES]).expect("cipher");
        let next = key.next().expect("rekey");
        let mut reference = DefaultResolver
            .resolve_cipher(&CipherChoice::ChaChaPoly)
            .expect("cipher");
        reference.set(&[7; KEY_BYTES]);
        reference.rekey();
        let mut a = [0u8; 4 + TAG_BYTES];
        let mut b = [0u8; 4 + TAG_BYTES];
        next.seal(3, b"same", &mut a).expect("seal");
        reference.encrypt(3, &[], b"same", &mut b);
        assert_eq!(a, b, "the derived key is the one REKEY gives");
        assert_ne!(next.fingerprint(), key.fingerprint());
        assert_eq!(
            next.next().expect("rekey").fingerprint(),
            key.next().expect("rekey").next().expect("rekey").fingerprint()
        );
    }

    #[test]
    fn the_policy_counts_records_and_bytes_and_keeps_a_floor() {
        let policy = RekeyPolicy {
            after_records: 100,
            after_bytes: 10_000,
        };
        assert!(!policy.due(99, 0));
        assert!(policy.due(100, 0));
        assert!(policy.due(70, 10_000), "bytes first");
        assert!(!policy.due(MIN_RECORDS_PER_KEY - 1, u64::MAX), "the floor");
        assert!(!RekeyPolicy::NEVER.due(u64::MAX, u64::MAX));
        let bytes_only = RekeyPolicy {
            after_records: 0,
            after_bytes: 1,
        };
        assert!(bytes_only.due(MIN_RECORDS_PER_KEY, 1));
    }

    #[test]
    fn picking_follows_the_epoch_byte_and_the_counter() {
        let mut keys = RecvKeys::new(Key::new([1; KEY_BYTES]).expect("cipher"));
        assert!(matches!(keys.pick(0, 5), Ok(Pick::Current)));
        assert!(matches!(keys.pick(1, 5), Ok(Pick::Ahead(1))));
        assert!(matches!(keys.pick(MAX_EPOCH_SKIP, 5), Ok(Pick::Ahead(n)) if n == MAX_EPOCH_SKIP));
        assert_eq!(keys.pick(MAX_EPOCH_SKIP + 1, 5).err(), Some(CryptoError::Auth));
        assert_eq!(
            keys.pick(u8::MAX, 5).err(),
            Some(CryptoError::Replay),
            "no previous key yet"
        );
        keys.commit(&Pick::Current, 10);
        let ahead = keys.pick(1, 11).expect("ahead");
        keys.key(&ahead).expect("derive");
        keys.commit(&ahead, 11);
        assert_eq!(keys.epoch(), 1);
        assert!(
            matches!(keys.pick(0, 9), Ok(Pick::Previous)),
            "a late record of epoch 0"
        );
        assert_eq!(
            keys.pick(0, 12).err(),
            Some(CryptoError::Replay),
            "epoch 0 never sealed past 11"
        );
        assert_eq!(
            keys.pick(2, 11).err(),
            Some(CryptoError::Auth),
            "a newer key needs a newer counter"
        );
        assert_eq!(
            keys.pick(u8::MAX, 1).err(),
            Some(CryptoError::Replay),
            "two keys back"
        );
    }
}
