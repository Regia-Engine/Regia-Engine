// SPDX-License-Identifier: MPL-2.0
//! Noise through `snow`: the two handshake sides and [`SecureLink`], one session's record layer.
//!
//! The handshake is `snow`'s. The record layer holds the two keys the handshake splits into
//! (Noise revision 34 § 5.2 `Split()`) and seals with `snow`'s own `ChaChaPoly` cipher, exactly as
//! `snow`'s stateless transport does, plus what that transport cannot do: move each direction to
//! the next key after a [`RekeyPolicy`] (ADR-2416, `super::rekey`).

use snow::params::NoiseParams;
use snow::{Builder, HandshakeState};

use super::rekey::{EPOCH_SHIFT, Key, RECORD_COUNTER_MASK, RecvKeys, RekeyPolicy, SendKeys};
use super::replay::ReplayWindow;
use super::{CryptoError, NOISE_PATTERN, SEALED_HEADER_BYTES, ServerIdentity, ServerKey};
use crate::socket::MAX_DATAGRAM_BYTES;

const TAG_BYTES: usize = 16;
/// The record counter stops one short of its field (Noise reserves the last nonce, revision 34
/// § 5.1; the top byte of the field is the key epoch, ADR-2416).
const COUNTER_LIMIT: u64 = RECORD_COUNTER_MASK;

/// What a sealed datagram carries. Inside the ciphertext, so an observer cannot tell a keepalive
/// from a goodbye by its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SealedKind {
    /// A `crate::connection::Connection` packet body.
    Payload = 1,
    /// A goodbye: one reason byte.
    Disconnect = 2,
    /// The client's join token.
    Join = 3,
    /// The server's `Accept` body.
    Accept = 4,
    /// The server's `Deny` body.
    Deny = 5,
}

impl SealedKind {
    /// The kind a plaintext kind byte names, `None` for a byte no kind uses.
    pub(crate) fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Payload,
            2 => Self::Disconnect,
            3 => Self::Join,
            4 => Self::Accept,
            5 => Self::Deny,
            _ => return None,
        })
    }
}

fn params() -> Result<NoiseParams, CryptoError> {
    NOISE_PATTERN.parse().map_err(|_| CryptoError::Unavailable)
}

fn hash_of(state: &HandshakeState) -> Result<[u8; 32], CryptoError> {
    state
        .get_handshake_hash()
        .try_into()
        .map_err(|_| CryptoError::Unavailable)
}

/// One session's keys, send counter and replay window.
pub struct SecureLink {
    send: SendKeys,
    recv: RecvKeys,
    policy: RekeyPolicy,
    token: u32,
    next_counter: u64,
    window: ReplayWindow,
    handshake_hash: [u8; 32],
    /// The resumption PSK of this link (`super::resume::derive_resume_psk`). Never printed.
    resume_psk: [u8; 32],
}

impl std::fmt::Debug for SecureLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureLink")
            .field("token", &self.token)
            .field("next_counter", &self.next_counter)
            .field("send_epoch", &self.send.epoch())
            .field("recv_epoch", &self.recv.epoch())
            .finish_non_exhaustive()
    }
}

impl SecureLink {
    pub(super) fn new(mut state: HandshakeState, token: u32) -> Result<Self, CryptoError> {
        if !state.is_handshake_finished() {
            return Err(CryptoError::Unavailable);
        }
        let handshake_hash = hash_of(&state)?;
        // `Split()`: the first key protects initiator-to-responder traffic, the second the reverse.
        let (to_responder, to_initiator) = state.dangerously_get_raw_split();
        let resume_psk = super::resume::derive_resume_psk(&handshake_hash, &to_responder, &to_initiator);
        let (send, recv) = if state.is_initiator() {
            (to_responder, to_initiator)
        } else {
            (to_initiator, to_responder)
        };
        Ok(Self {
            send: SendKeys::new(Key::new(send)?),
            recv: RecvKeys::new(Key::new(recv)?),
            policy: RekeyPolicy::default(),
            token,
            next_counter: 0,
            window: ReplayWindow::new(),
            handshake_hash,
            resume_psk,
        })
    }

    fn from_keys(
        send: [u8; 32],
        recv: [u8; 32],
        token: u32,
        handshake_hash: [u8; 32],
        resume_psk: [u8; 32],
    ) -> Result<Self, CryptoError> {
        Ok(Self {
            send: SendKeys::new(Key::new(send)?),
            recv: RecvKeys::new(Key::new(recv)?),
            policy: RekeyPolicy::default(),
            token,
            next_counter: 0,
            window: ReplayWindow::new(),
            handshake_hash,
            resume_psk,
        })
    }

    /// The session token.
    pub fn token(&self) -> u32 {
        self.token
    }

    /// The Noise handshake hash: unique to this session, for channel binding.
    pub fn handshake_hash(&self) -> &[u8; 32] {
        &self.handshake_hash
    }

    /// The resumption PSK both ends of this link derive from its handshake (label
    /// [`super::RESUME_PSK_LABEL`], ADR-8858 A7): the secret a resumption ticket seals on the
    /// server and a [`super::ResumeCredential`] keeps on the client. It is never sent, and a
    /// rekey does not change it.
    pub fn resume_psk(&self) -> &[u8; 32] {
        &self.resume_psk
    }

    /// When this side moves its sending key on. The receiving side follows the peer's records
    /// whatever its own policy says.
    pub fn set_rekey_policy(&mut self, policy: RekeyPolicy) {
        self.policy = policy;
    }

    /// The sending side's rekey policy.
    pub fn rekey_policy(&self) -> RekeyPolicy {
        self.policy
    }

    /// Keys the sending direction moved past (0 until the first rekey).
    pub fn send_epoch(&self) -> u32 {
        self.send.epoch()
    }

    /// Keys the receiving direction moved past, following the peer.
    pub fn recv_epoch(&self) -> u32 {
        self.recv.epoch()
    }

    /// The newest record counter that authenticated from the peer, `None` before the first.
    pub fn newest_counter(&self) -> Option<u64> {
        self.window.newest()
    }

    /// Rekeys in both directions so far.
    pub fn rekeys(&self) -> u64 {
        u64::from(self.send.epoch()) + u64::from(self.recv.epoch())
    }

    /// Moves the sending direction to its next key now, whatever the policy says. The peer
    /// follows on the first record it opens under the new key.
    pub fn rekey_now(&mut self) -> Result<(), CryptoError> {
        self.send.force_next()
    }

    /// Builds one sealed datagram.
    pub fn seal(&mut self, kind: SealedKind, body: &[u8]) -> Result<Vec<u8>, CryptoError> {
        self.seal_kind_byte(kind as u8, body)
    }

    /// Seals `body` with kind byte `kind`, including a byte [`SealedKind`] does not name.
    ///
    /// Tests only. [`Self::seal`] is what production sends, and it only accepts a [`SealedKind`].
    #[cfg(test)]
    pub(crate) fn seal_raw_kind(&mut self, kind: u8, body: &[u8]) -> Result<Vec<u8>, CryptoError> {
        self.seal_kind_byte(kind, body)
    }

    fn seal_kind_byte(&mut self, kind: u8, body: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if self.next_counter >= COUNTER_LIMIT {
            return Err(CryptoError::Exhausted);
        }
        let plain_len = 1 + body.len();
        let total = SEALED_HEADER_BYTES + plain_len + TAG_BYTES;
        if total > MAX_DATAGRAM_BYTES {
            return Err(CryptoError::TooLarge);
        }
        let mut plain = [0u8; MAX_DATAGRAM_BYTES];
        plain[0] = kind;
        plain[1..plain_len].copy_from_slice(body);
        let mut out = vec![0u8; total];
        let (head, cipher) = out.split_at_mut(SEALED_HEADER_BYTES);
        let counter = self.next_counter;
        let (key, epoch) = self.send.key_for_next(&self.policy)?;
        let head: &mut [u8; SEALED_HEADER_BYTES] = head.try_into().map_err(|_| CryptoError::Unavailable)?;
        super::SealedHeader::write(head, self.token, (u64::from(epoch) << EPOCH_SHIFT) | counter);
        key.seal(counter, &plain[..plain_len], cipher)?;
        self.send.note_sealed(cipher.len());
        self.next_counter += 1;
        Ok(out)
    }

    /// Opens one record. The window and the key epoch are checked first, from the clear header;
    /// both move only after the tag verified.
    pub fn open(&mut self, header: super::SealedHeader<'_>) -> Result<(SealedKind, Vec<u8>), CryptoError> {
        let (kind, plain) = self.open_kind_byte(header)?;
        let kind = SealedKind::from_u8(kind).ok_or(CryptoError::Malformed)?;
        Ok((kind, plain))
    }

    /// [`Self::open`] without naming the kind: the kind byte and the body of a record whose tag
    /// verified. A kind byte [`SealedKind`] does not name is still `Ok` here, so the server can tell
    /// an authenticated record of an unknown kind (the peer's, and scored) from a record that failed
    /// its open (not provably the peer's, ADR-8606). The window and the key epoch move as in
    /// [`Self::open`].
    pub(crate) fn open_kind_byte(
        &mut self,
        header: super::SealedHeader<'_>,
    ) -> Result<(u8, Vec<u8>), CryptoError> {
        if self.token != header.token {
            return Err(CryptoError::Auth);
        }
        if header.ciphertext.len() < TAG_BYTES + 1 || header.ciphertext.len() > MAX_DATAGRAM_BYTES {
            return Err(CryptoError::Malformed);
        }
        let counter = header.record_counter();
        if counter >= COUNTER_LIMIT || !self.window.is_fresh(counter) {
            return Err(CryptoError::Replay);
        }
        let pick = self.recv.pick(header.epoch(), counter)?;
        let mut plain = vec![0u8; header.ciphertext.len()];
        let n = self
            .recv
            .key(&pick)?
            .open(counter, header.ciphertext, &mut plain)?;
        self.window.commit(counter);
        self.recv.commit(&pick, counter);
        let (&kind, _) = plain[..n].split_first().ok_or(CryptoError::Malformed)?;
        plain.truncate(n);
        plain.remove(0);
        Ok((kind, plain))
    }

    /// Opens a copy of a record this link already accepted, without touching the replay window or
    /// the key epochs: `Err(Replay)` unless the counter was already accepted and is still inside
    /// the window. Path validation (ADR-8299) uses it for the genuine copy of a record that a raced
    /// copy from another address delivered first: the copy shows the far end at its address, but
    /// its content must not be delivered twice.
    pub fn open_duplicate(
        &mut self,
        header: super::SealedHeader<'_>,
    ) -> Result<(SealedKind, Vec<u8>), CryptoError> {
        if self.token != header.token {
            return Err(CryptoError::Auth);
        }
        if header.ciphertext.len() < TAG_BYTES + 1 || header.ciphertext.len() > MAX_DATAGRAM_BYTES {
            return Err(CryptoError::Malformed);
        }
        let counter = header.record_counter();
        let in_window = self
            .window
            .newest()
            .is_some_and(|n| counter <= n && n - counter < super::REPLAY_WINDOW_COUNTERS);
        if counter >= COUNTER_LIMIT || !in_window || self.window.is_fresh(counter) {
            return Err(CryptoError::Replay);
        }
        let pick = self.recv.pick(header.epoch(), counter)?;
        let mut plain = vec![0u8; header.ciphertext.len()];
        let n = self
            .recv
            .key(&pick)?
            .open(counter, header.ciphertext, &mut plain)?;
        let (&kind, _) = plain[..n].split_first().ok_or(CryptoError::Malformed)?;
        let kind = SealedKind::from_u8(kind).ok_or(CryptoError::Malformed)?;
        plain.truncate(n);
        plain.remove(0);
        Ok((kind, plain))
    }
}

/// A client and a server link with keys derived from `seed`, so every run seals the same bytes.
/// For fuzz targets and tests only: anyone who knows `seed` can read and forge the traffic, and no
/// handshake authenticated either side.
pub fn fixture_link_pair(seed: &[u8; 32], token: u32) -> Result<(SecureLink, SecureLink), CryptoError> {
    let to_server = blake3::derive_key("regia_net fixture link to server v1", seed);
    let to_client = blake3::derive_key("regia_net fixture link to client v1", seed);
    let hash = blake3::derive_key("regia_net fixture link hash v1", seed);
    let psk = super::resume::derive_resume_psk(&hash, &to_server, &to_client);
    Ok((
        SecureLink::from_keys(to_server, to_client, token, hash, psk)?,
        SecureLink::from_keys(to_client, to_server, token, hash, psk)?,
    ))
}

/// The client's side of the handshake.
pub struct ClientHandshake {
    state: HandshakeState,
    message: [u8; 32],
    pin: Option<ServerKey>,
    server_key: Option<ServerKey>,
    token: Option<u32>,
}

impl std::fmt::Debug for ClientHandshake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientHandshake")
            .field("pin", &self.pin)
            .field("server_key", &self.server_key)
            .finish_non_exhaustive()
    }
}

impl ClientHandshake {
    /// Starts a handshake and writes the first Noise message.
    pub fn start(prologue: &[u8; 32], pin: Option<ServerKey>) -> Result<Self, CryptoError> {
        let mut state = Builder::new(params()?)
            .prologue(prologue)
            .and_then(Builder::build_initiator)
            .map_err(|_| CryptoError::Unavailable)?;
        // `snow` asks for room for a tag even though the first message has no key yet.
        let mut scratch = [0u8; 32 + TAG_BYTES];
        let n = state
            .write_message(&[], &mut scratch)
            .map_err(|_| CryptoError::Unavailable)?;
        let message: [u8; 32] = scratch[..n].try_into().map_err(|_| CryptoError::Unavailable)?;
        Ok(Self {
            state,
            message,
            pin,
            server_key: None,
            token: None,
        })
    }

    /// The first Noise message (kept: `KeyInit` is resent with the same `e`).
    pub fn message(&self) -> &[u8; 32] {
        &self.message
    }

    /// Reads the server's reply. [`CryptoError::Auth`] leaves the handshake usable, so a forged
    /// reply cannot end a connect attempt; [`CryptoError::KeyMismatch`] means a server proved a key
    /// that is not the pinned one, and the caller must stop.
    pub fn read_reply(&mut self, reply: &[u8]) -> Result<ServerKey, CryptoError> {
        let mut payload = [0u8; 64];
        let n = self
            .state
            .read_message(reply, &mut payload)
            .map_err(|_| CryptoError::Auth)?;
        let token: [u8; 4] = payload[..n].try_into().map_err(|_| CryptoError::Malformed)?;
        let remote: [u8; 32] = self
            .state
            .get_remote_static()
            .and_then(|k| k.try_into().ok())
            .ok_or(CryptoError::Unavailable)?;
        if let Some(pin) = self.pin
            && !pin.matches(&remote)
        {
            return Err(CryptoError::KeyMismatch);
        }
        let key = ServerKey(remote);
        self.server_key = Some(key);
        self.token = Some(u32::from_le_bytes(token));
        Ok(key)
    }

    /// The finished session, under the token the reply carried. Call after
    /// [`ClientHandshake::read_reply`] succeeded.
    pub fn into_link(self) -> Result<SecureLink, CryptoError> {
        let token = self.token.ok_or(CryptoError::Unavailable)?;
        SecureLink::new(self.state, token)
    }
}

/// The server's side: reads the first Noise message and writes the reply, which carries `token`.
/// Runs two curve multiplications beyond key generation, so callers gate it behind a valid cookie.
pub fn accept_handshake(
    identity: &ServerIdentity,
    prologue: &[u8; 32],
    message: &[u8],
    token: u32,
) -> Result<(SecureLink, [u8; 100]), CryptoError> {
    let mut state = Builder::new(params()?)
        .prologue(prologue)
        .and_then(|b| b.local_private_key(identity.private_bytes()))
        .and_then(Builder::build_responder)
        .map_err(|_| CryptoError::Unavailable)?;
    let mut scratch = [0u8; 64];
    state
        .read_message(message, &mut scratch)
        .map_err(|_| CryptoError::Auth)?;
    let mut reply = [0u8; 100];
    let n = state
        .write_message(&token.to_le_bytes(), &mut reply)
        .map_err(|_| CryptoError::Unavailable)?;
    if n != reply.len() {
        return Err(CryptoError::Unavailable);
    }
    Ok((SecureLink::new(state, token)?, reply))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{MAX_EPOCH_SKIP, REPLAY_WINDOW_COUNTERS, SEALED_OVERHEAD_BYTES, SealedHeader};
    use crate::handshake::split_datagram;

    const TOKEN: u32 = 0x5EA1;

    fn pair() -> (SecureLink, SecureLink) {
        let identity = ServerIdentity::generate().expect("random");
        let hs = ClientHandshake::start(&[1; 32], Some(identity.public())).expect("start");
        let (server, reply) = accept_handshake(&identity, &[1; 32], hs.message(), TOKEN).expect("accept");
        let mut hs = hs;
        assert_eq!(hs.read_reply(&reply).expect("reply"), identity.public());
        (hs.into_link().expect("link"), server)
    }

    fn open(link: &mut SecureLink, datagram: &[u8]) -> Result<(SealedKind, Vec<u8>), CryptoError> {
        let (_, body) = split_datagram(datagram).ok_or(CryptoError::Malformed)?;
        link.open(SealedHeader::parse(body).ok_or(CryptoError::Malformed)?)
    }

    #[test]
    fn both_directions_round_trip_and_both_sides_share_the_hash() {
        let (mut client, mut server) = pair();
        assert_eq!(client.handshake_hash(), server.handshake_hash());
        let up = client.seal(SealedKind::Payload, b"up").expect("seal");
        assert_eq!(
            up.len(),
            crate::handshake::CONNECTED_HEADER_BYTES + SEALED_OVERHEAD_BYTES + 2
        );
        assert_eq!(open(&mut server, &up), Ok((SealedKind::Payload, b"up".to_vec())));
        let down = server.seal(SealedKind::Deny, b"down").expect("seal");
        assert_eq!(open(&mut client, &down), Ok((SealedKind::Deny, b"down".to_vec())));
    }

    #[test]
    fn empty_bodies_and_the_largest_body_seal() {
        let (mut client, mut server) = pair();
        let empty = client.seal(SealedKind::Disconnect, &[]).expect("seal");
        assert_eq!(
            open(&mut server, &empty),
            Ok((SealedKind::Disconnect, Vec::new()))
        );
        let max = MAX_DATAGRAM_BYTES - SEALED_HEADER_BYTES - 1 - TAG_BYTES;
        let big = client.seal(SealedKind::Payload, &vec![7; max]).expect("seal");
        assert_eq!(big.len(), MAX_DATAGRAM_BYTES);
        assert_eq!(open(&mut server, &big).expect("open").1.len(), max);
        assert_eq!(
            client.seal(SealedKind::Payload, &vec![7; max + 1]),
            Err(CryptoError::TooLarge)
        );
    }

    #[test]
    fn every_flipped_byte_is_refused() {
        let (mut client, mut server) = pair();
        let good = client.seal(SealedKind::Payload, b"message").expect("seal");
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x40;
            assert!(open(&mut server, &bad).is_err(), "byte {i}");
        }
        assert_eq!(
            open(&mut server, &good).expect("open").1,
            b"message",
            "still fresh"
        );
    }

    #[test]
    fn every_truncation_is_refused() {
        let (mut client, mut server) = pair();
        let good = client.seal(SealedKind::Payload, b"message").expect("seal");
        for n in 0..good.len() {
            assert!(open(&mut server, &good[..n]).is_err(), "{n} bytes");
        }
    }

    #[test]
    fn replays_are_refused_and_reordering_is_not() {
        let (mut client, mut server) = pair();
        let first = client.seal(SealedKind::Payload, b"1").expect("seal");
        let second = client.seal(SealedKind::Payload, b"2").expect("seal");
        assert!(open(&mut server, &second).is_ok(), "arrives first");
        assert!(open(&mut server, &first).is_ok(), "arrives late");
        assert_eq!(open(&mut server, &first), Err(CryptoError::Replay));
        assert_eq!(open(&mut server, &second), Err(CryptoError::Replay));
    }

    #[test]
    fn a_forged_counter_cannot_move_the_window() {
        let (mut client, mut server) = pair();
        let good = client.seal(SealedKind::Payload, b"x").expect("seal");
        let mut forged = good.clone();
        forged[9..17].copy_from_slice(&(COUNTER_LIMIT - 1).to_le_bytes());
        assert_eq!(open(&mut server, &forged), Err(CryptoError::Auth));
        assert_eq!(server.newest_counter(), None, "failed auth does not commit");
        assert_eq!(server.recv_epoch(), 0, "failed auth does not advance keys");
        let mut out_of_range = good.clone();
        out_of_range[9..17].copy_from_slice(&COUNTER_LIMIT.to_le_bytes());
        assert_eq!(open(&mut server, &out_of_range), Err(CryptoError::Replay));
        assert_eq!(
            server.newest_counter(),
            None,
            "out-of-range counter does not commit"
        );
        assert_eq!(
            server.recv_epoch(),
            0,
            "out-of-range counter does not advance keys"
        );
        assert!(
            open(&mut server, &good).is_ok(),
            "the genuine low counter remains usable after a forged high counter"
        );
    }

    #[test]
    fn another_sessions_key_is_refused() {
        let (mut client, _) = pair();
        let (_, mut other) = pair();
        let datagram = client.seal(SealedKind::Payload, b"x").expect("seal");
        assert_eq!(open(&mut other, &datagram), Err(CryptoError::Auth));
    }

    #[test]
    fn a_reflected_record_is_refused() {
        let (mut client, _server) = pair();
        let own = client.seal(SealedKind::Payload, b"x").expect("seal");
        assert_eq!(
            open(&mut client, &own),
            Err(CryptoError::Auth),
            "directions use separate keys"
        );
    }

    #[test]
    fn the_last_counter_is_never_used() {
        let (mut client, mut server) = pair();
        client.next_counter = COUNTER_LIMIT - 1;
        let last = client.seal(SealedKind::Payload, b"x").expect("seal");
        assert_eq!(open(&mut server, &last), Ok((SealedKind::Payload, b"x".to_vec())));
        assert_eq!(
            client.seal(SealedKind::Payload, b"y"),
            Err(CryptoError::Exhausted)
        );
        assert_eq!(client.next_counter, COUNTER_LIMIT, "exhaustion never wraps");
        assert_eq!(
            client.seal(SealedKind::Payload, b"z"),
            Err(CryptoError::Exhausted),
            "the exhausted nonce is never reused"
        );
    }

    #[test]
    fn a_different_prologue_fails_the_handshake() {
        let identity = ServerIdentity::generate().expect("random");
        let mut hs = ClientHandshake::start(&[1; 32], None).expect("start");
        let (_, reply) = accept_handshake(&identity, &[2; 32], hs.message(), TOKEN).expect("accept");
        assert_eq!(hs.read_reply(&reply).map(|_| ()), Err(CryptoError::Auth));
    }

    #[test]
    fn a_wrong_pin_stops_at_the_reply() {
        let identity = ServerIdentity::generate().expect("random");
        let other = ServerIdentity::generate().expect("random");
        let mut hs = ClientHandshake::start(&[1; 32], Some(other.public())).expect("start");
        let (_, reply) = accept_handshake(&identity, &[1; 32], hs.message(), TOKEN).expect("accept");
        assert_eq!(hs.read_reply(&reply).map(|_| ()), Err(CryptoError::KeyMismatch));
    }

    #[test]
    fn a_forged_reply_leaves_the_handshake_usable() {
        let identity = ServerIdentity::generate().expect("random");
        let mut hs = ClientHandshake::start(&[1; 32], Some(identity.public())).expect("start");
        let (mut server, reply) = accept_handshake(&identity, &[1; 32], hs.message(), TOKEN).expect("accept");
        let mut forged = reply;
        forged[70] ^= 1;
        assert_eq!(hs.read_reply(&forged).map(|_| ()), Err(CryptoError::Auth));
        assert_eq!(
            hs.read_reply(&[0; 3]).map(|_| ()),
            Err(CryptoError::Auth),
            "short"
        );
        hs.read_reply(&reply).expect("the genuine reply still works");
        let mut client = hs.into_link().expect("link");
        assert_eq!(client.token(), TOKEN, "the token comes out of the reply");
        let up = client.seal(SealedKind::Join, b"j").expect("seal");
        assert_eq!(open(&mut server, &up), Ok((SealedKind::Join, b"j".to_vec())));
    }

    // ---- Rekeying (ADR-2416) ----------------------------------------------------------------

    fn small_policy(after_bytes: u64) -> RekeyPolicy {
        RekeyPolicy {
            after_records: 0,
            after_bytes,
        }
    }

    /// The raw handshake states of one session, for checks against `snow`'s own transport.
    fn raw_pair() -> (HandshakeState, HandshakeState) {
        let identity = ServerIdentity::generate().expect("random");
        let mut client = Builder::new(params().expect("params"))
            .prologue(&[1; 32])
            .and_then(Builder::build_initiator)
            .expect("initiator");
        let mut server = Builder::new(params().expect("params"))
            .prologue(&[1; 32])
            .and_then(|b| b.local_private_key(identity.private_bytes()))
            .and_then(Builder::build_responder)
            .expect("responder");
        let mut m1 = [0u8; 64];
        let n = client.write_message(&[], &mut m1).expect("m1");
        let mut scratch = [0u8; 64];
        server.read_message(&m1[..n], &mut scratch).expect("read m1");
        let mut m2 = [0u8; 128];
        let n = server.write_message(&TOKEN.to_le_bytes(), &mut m2).expect("m2");
        client.read_message(&m2[..n], &mut scratch).expect("read m2");
        (client, server)
    }

    /// Before any rekey the record layer is byte for byte `snow`'s stateless transport, in both
    /// directions, so ADR-1901's wire is unchanged for epoch 0.
    #[test]
    fn epoch_zero_records_are_snows_transport_records() {
        let (client_state, mut server_state) = raw_pair();
        let mut client = SecureLink::new(client_state, TOKEN).expect("link");
        // Splitting is a pure function of the state, so the server's transport is unaffected.
        let _ = server_state.dangerously_get_raw_split();
        let server = server_state.into_stateless_transport_mode().expect("transport");
        for (i, body) in [&b"first"[..], b"", &[9u8; 300]].into_iter().enumerate() {
            let datagram = client.seal(SealedKind::Payload, body).expect("seal");
            let (_, rest) = split_datagram(&datagram).expect("framing");
            let header = SealedHeader::parse(rest).expect("header");
            assert_eq!(header.counter, i as u64);
            let mut out = vec![0u8; header.ciphertext.len()];
            let n = server
                .read_message(header.counter, header.ciphertext, &mut out)
                .expect("snow opens our record");
            assert_eq!(out[0], SealedKind::Payload as u8);
            assert_eq!(&out[1..n], body);
            // And the other way: a record `snow` sealed opens here.
            let mut plain = vec![SealedKind::Deny as u8];
            plain.extend_from_slice(body);
            let mut sealed = vec![0u8; plain.len() + TAG_BYTES];
            let n = server
                .write_message(i as u64, &plain, &mut sealed)
                .expect("snow seals");
            let mut head = [0u8; SEALED_HEADER_BYTES];
            SealedHeader::write(&mut head, TOKEN, i as u64);
            let mut datagram = head.to_vec();
            datagram.extend_from_slice(&sealed[..n]);
            assert_eq!(
                open(&mut client, &datagram),
                Ok((SealedKind::Deny, body.to_vec()))
            );
        }
    }

    /// A byte-count rekey in both directions: every record arrives exactly once, both sides end on
    /// the same epochs, and a replay of a record from before the switch is still refused.
    #[test]
    fn a_rekey_at_a_byte_count_loses_and_replays_nothing() {
        let (mut client, mut server) = pair();
        client.set_rekey_policy(small_policy(4096));
        server.set_rekey_policy(small_policy(8192));
        let mut sent_up = Vec::new();
        for i in 0..2000u32 {
            let body = i.to_le_bytes().repeat(8);
            let up = client.seal(SealedKind::Payload, &body).expect("seal");
            assert_eq!(
                open(&mut server, &up),
                Ok((SealedKind::Payload, body.clone())),
                "up {i}"
            );
            let down = server.seal(SealedKind::Payload, &body).expect("seal");
            assert_eq!(
                open(&mut client, &down),
                Ok((SealedKind::Payload, body)),
                "down {i}"
            );
            sent_up.push(up);
        }
        // 2000 records of 49 ciphertext bytes, 4096 bytes and at least 64 records per key.
        assert_eq!(client.send_epoch(), 2000 / 84, "client keys");
        assert_eq!(server.recv_epoch(), client.send_epoch(), "the server followed");
        assert_eq!(server.send_epoch(), 2000 / 168, "server keys");
        assert_eq!(client.recv_epoch(), server.send_epoch(), "the client followed");
        assert_eq!(
            client.rekeys(),
            u64::from(client.send_epoch() + client.recv_epoch())
        );
        for (i, old) in sent_up
            .iter()
            .enumerate()
            .rev()
            .take(REPLAY_WINDOW_COUNTERS as usize)
        {
            assert_eq!(open(&mut server, old), Err(CryptoError::Replay), "replay of {i}");
        }
    }

    /// Records reordered across a switch: the late ones open under the previous key, the early
    /// ones under the next, and the replay window still sees each once.
    #[test]
    fn records_reordered_across_a_rekey_all_open_once() {
        let (mut client, mut server) = pair();
        client.set_rekey_policy(RekeyPolicy {
            after_records: 100,
            after_bytes: 0,
        });
        let records: Vec<Vec<u8>> = (0..300u32)
            .map(|i| client.seal(SealedKind::Payload, &i.to_le_bytes()).expect("seal"))
            .collect();
        assert_eq!(client.send_epoch(), 2);
        // Deliver in blocks of 16, each block backwards: 15..0, 31..16, ... crossing 100 and 200.
        let mut order: Vec<usize> = Vec::new();
        for block in (0..records.len()).collect::<Vec<_>>().chunks(16) {
            order.extend(block.iter().rev());
        }
        for &i in &order {
            assert_eq!(
                open(&mut server, &records[i]),
                Ok((SealedKind::Payload, (i as u32).to_le_bytes().to_vec())),
                "record {i}"
            );
        }
        for &i in &order {
            assert_eq!(
                open(&mut server, &records[i]),
                Err(CryptoError::Replay),
                "again {i}"
            );
        }
        assert_eq!(server.recv_epoch(), 2);
    }

    /// Whole epochs lost: the receiver steps ahead through the chain, up to the limit.
    #[test]
    fn a_receiver_catches_up_over_lost_epochs_up_to_the_limit() {
        let (mut client, mut server) = pair();
        for _ in 0..MAX_EPOCH_SKIP {
            client.rekey_now().expect("rekey");
        }
        let far = client.seal(SealedKind::Payload, b"far").expect("seal");
        assert_eq!(
            open(&mut server, &far),
            Ok((SealedKind::Payload, b"far".to_vec()))
        );
        assert_eq!(server.recv_epoch(), u32::from(MAX_EPOCH_SKIP));
        for _ in 0..=MAX_EPOCH_SKIP {
            client.rekey_now().expect("rekey");
        }
        let too_far = client.seal(SealedKind::Payload, b"x").expect("seal");
        assert_eq!(open(&mut server, &too_far), Err(CryptoError::Auth));
        assert_eq!(server.recv_epoch(), u32::from(MAX_EPOCH_SKIP), "nothing moved");
    }

    /// A forged epoch byte on a genuine record fails the tag and moves neither the key nor the
    /// window; the genuine record still opens afterwards.
    #[test]
    fn a_forged_epoch_moves_nothing() {
        let (mut client, mut server) = pair();
        let good = client.seal(SealedKind::Payload, b"x").expect("seal");
        let field = SEALED_HEADER_BYTES - 1;
        for epoch in [1u8, 2, MAX_EPOCH_SKIP, 0x80, u8::MAX] {
            let mut forged = good.clone();
            forged[field] = epoch;
            assert!(open(&mut server, &forged).is_err(), "epoch {epoch}");
            assert_eq!(server.recv_epoch(), 0, "epoch {epoch}");
        }
        assert_eq!(open(&mut server, &good), Ok((SealedKind::Payload, b"x".to_vec())));
    }

    /// The sender drops its old key at the switch: a record sealed afterwards is never under it.
    #[test]
    fn records_after_a_rekey_need_the_new_key() {
        let (raw_client, mut raw_server) = raw_pair();
        let mut linked = SecureLink::new(raw_client, TOKEN).expect("link");
        let _ = raw_server.dangerously_get_raw_split();
        let old_key_reader = raw_server.into_stateless_transport_mode().expect("transport");
        linked.rekey_now().expect("rekey");
        let datagram = linked.seal(SealedKind::Payload, b"secret").expect("seal");
        let (_, rest) = split_datagram(&datagram).expect("framing");
        let header = SealedHeader::parse(rest).expect("header");
        assert_eq!(header.epoch(), 1);
        let mut out = vec![0u8; header.ciphertext.len()];
        assert!(
            old_key_reader
                .read_message(header.record_counter(), header.ciphertext, &mut out)
                .is_err(),
            "the epoch 0 key cannot read epoch 1"
        );
    }

    #[test]
    fn fixture_pairs_repeat_and_talk() {
        let (mut a, mut b) = fixture_link_pair(&[3; 32], TOKEN).expect("pair");
        let (mut a2, _) = fixture_link_pair(&[3; 32], TOKEN).expect("pair");
        let (mut other, _) = fixture_link_pair(&[4; 32], TOKEN).expect("pair");
        let up = a.seal(SealedKind::Payload, b"same").expect("seal");
        assert_eq!(
            up,
            a2.seal(SealedKind::Payload, b"same").expect("seal"),
            "deterministic"
        );
        assert_ne!(up, other.seal(SealedKind::Payload, b"same").expect("seal"));
        assert_eq!(open(&mut b, &up), Ok((SealedKind::Payload, b"same".to_vec())));
        let down = b.seal(SealedKind::Accept, b"back").expect("seal");
        assert_eq!(open(&mut a, &down), Ok((SealedKind::Accept, b"back".to_vec())));
    }

    #[test]
    fn garbage_first_messages_are_refused_without_panicking() {
        let identity = ServerIdentity::generate().expect("random");
        for n in [0usize, 1, 31] {
            assert!(
                accept_handshake(&identity, &[1; 32], &vec![0; n], TOKEN).is_err(),
                "{n}"
            );
        }
    }
}
