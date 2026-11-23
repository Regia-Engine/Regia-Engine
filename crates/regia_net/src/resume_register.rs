// SPDX-License-Identifier: MPL-2.0
//! The server's register of pending one-round-trip resumes (spec 120 §§ 10.3, 11.5; ADR-8858
//! A1, A3, A4, A5; ADR-8902).
//!
//! A `ResumeInit` that passes every check admits nobody: it leaves one [`PendingResume`] here,
//! the finished link, the stored `ResumeReply` and everything the admission will need. The peer
//! is admitted when the first sealed record under that link's keys arrives (key confirmation),
//! and only then.
//!
//! The register is keyed by the ticket (its key id and nonce, which the server chose and the
//! ticket's tag covers). Per ticket it keeps:
//!
//! - the highest flight number seen. Flights are ordered by that number, which only the holder
//!   of the PSK can raise, never by arrival: a flight at or below the highest that matches no
//!   pending row is a replay of an older flight and gets nothing ([`Flight::Stale`]);
//! - at most [`PENDING_RESUMES_PER_TICKET`] pending resumes. A flight above the highest takes a
//!   free place, or the place of the lower flight (its client gave that one up when it made the
//!   next);
//! - a byte-identical copy of a pending flight gets that flight's stored reply
//!   ([`Flight::Replay`]) and costs nothing else.
//!
//! A ticket's row leaves when one of its resumes is confirmed or when the ticket expires, never
//! to make room: a full register refuses a new ticket to the full handshake ([`Flight::Busy`]).
//! A pending resume lives `PENDING_TTL_MS`; its row, and the highest flight number in it, stay.

use std::collections::HashMap;
use std::net::SocketAddr;

use crate::crypto::{PENDING_TTL_MS, ResumeEpochs, SecureLink, TicketClaims};
use crate::handshake::{JoinToken, RESUME_TICKET_BYTES, RESUMPTION_TICKET_BYTES};
use crate::session::PeerId;

/// Pending resumes one ticket may have at once (ADR-8733 (3)(iv), ADR-8858 A3).
pub(crate) const PENDING_RESUMES_PER_TICKET: usize = 2;
/// Ticket rows per peer slot of the server (ADR-8858 A5; ADR-8623's chain bound).
pub(crate) const TICKET_ROWS_PER_PEER: usize = 8;
/// Fewest ticket rows a register holds, however small the server.
const MIN_TICKET_ROWS: usize = 8;

/// What names a ticket in the register: its key id and nonce (bytes 1..21 of the ticket). The
/// server chose them, no two tickets of one server share them, and the tag covers them.
pub(crate) type TicketId = [u8; 20];

/// The register key of an opened ticket.
pub(crate) fn ticket_id(ticket: &[u8; RESUMPTION_TICKET_BYTES]) -> TicketId {
    let mut id = [0u8; 20];
    id.copy_from_slice(&ticket[1..21]);
    id
}

/// Who a pending resume comes back as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeTarget {
    /// A dormant or live peer: the resume keeps its id.
    Peer(PeerId),
    /// The held slot of a player that left on purpose (ADR-8733 (1)): a new peer under the id
    /// issued for it when the flight was answered.
    HeldSlot(PeerId),
}

impl ResumeTarget {
    /// The peer id the stored accept names.
    pub(crate) fn peer(self) -> PeerId {
        match self {
            Self::Peer(peer) | Self::HeldSlot(peer) => peer,
        }
    }
}

/// One answered `ResumeInit`, waiting for its key confirmation.
#[derive(Debug)]
pub(crate) struct PendingResume {
    /// The flight's number (the first field of its hello).
    pub(crate) flight: u32,
    /// The first Noise message, so a byte-identical copy is told from a new flight.
    pub(crate) noise: Vec<u8>,
    /// The stored `ResumeReply` datagram.
    pub(crate) reply: Vec<u8>,
    /// The finished link.
    pub(crate) link: SecureLink,
    /// The flight's request nonce.
    pub(crate) client_nonce: u64,
    /// The address the flight proved (inside the ticket's block, or by its cookie).
    pub(crate) from: SocketAddr,
    /// What the ticket said.
    pub(crate) claims: TicketClaims,
    /// The join the hello made.
    pub(crate) join: JoinToken,
    /// Who it comes back as.
    pub(crate) target: ResumeTarget,
    /// The 16-byte resume ticket of the stored accept.
    pub(crate) ticket: [u8; RESUME_TICKET_BYTES],
    /// The resumption ticket of the stored accept.
    pub(crate) resumption: Option<[u8; RESUMPTION_TICKET_BYTES]>,
    /// The epoch of `resumption` (0: none was minted).
    pub(crate) accept_epoch: u32,
    /// The epochs of a new peer ([`ResumeTarget::HeldSlot`]); a resumed peer keeps its own.
    pub(crate) epochs: ResumeEpochs,
    /// When the flight was answered.
    pub(crate) created_ms: u64,
}

/// Where a flight stands against its ticket's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flight {
    /// Above every flight seen with this ticket: it may be answered.
    New,
    /// At or below the highest flight seen and matching no pending row: a replay of an older
    /// flight. No answer, no curve work, no charge.
    Stale,
    /// A ticket the register has no row for, and no room to make one.
    Busy,
}

#[derive(Debug)]
struct Row {
    highest: u32,
    expires_ms: u64,
    pending: Vec<PendingResume>,
}

/// The register. See the module docs.
#[derive(Debug)]
pub(crate) struct ResumeRegister {
    rows: HashMap<TicketId, Row>,
    /// Session token of a pending link -> its ticket, so the confirming record finds its row in
    /// one probe. Every value names a row that holds a pending resume with that token.
    by_token: HashMap<u32, TicketId>,
    capacity: usize,
}

impl ResumeRegister {
    /// A register for a server with `max_peers` peers.
    pub(crate) fn new(max_peers: usize) -> Self {
        Self {
            rows: HashMap::new(),
            by_token: HashMap::new(),
            capacity: max_peers
                .saturating_mul(TICKET_ROWS_PER_PEER)
                .max(MIN_TICKET_ROWS),
        }
    }

    /// Ticket rows held.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when no resume is pending (rows may remain).
    #[cfg(test)]
    pub(crate) fn no_pending(&self) -> bool {
        self.by_token.is_empty()
    }

    /// Pending resumes over every ticket.
    pub(crate) fn pending_total(&self) -> usize {
        self.rows.values().map(|row| row.pending.len()).sum()
    }

    /// Pending resumes of `ticket`.
    #[cfg(test)]
    pub(crate) fn pending_of(&self, ticket: &TicketId) -> usize {
        self.rows.get(ticket).map_or(0, |row| row.pending.len())
    }

    /// The most ticket rows the register holds.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// The stored reply of the pending flight of `ticket` whose first Noise message is `noise`,
    /// byte for byte (a lost reply). The whole register is not searched: one row, at most two
    /// comparisons.
    pub(crate) fn replay(&self, ticket: &TicketId, noise: &[u8]) -> Option<&[u8]> {
        self.rows
            .get(ticket)?
            .pending
            .iter()
            .find(|p| p.noise == noise)
            .map(|p| p.reply.as_slice())
    }

    /// Where `flight` stands against `ticket`'s row. Call after [`Self::replay`] found nothing.
    pub(crate) fn classify(&mut self, ticket: &TicketId, flight: u32, now_ms: u64) -> Flight {
        if let Some(row) = self.rows.get(ticket) {
            return if flight > row.highest {
                Flight::New
            } else {
                Flight::Stale
            };
        }
        if self.rows.len() >= self.capacity {
            self.expire(now_ms);
        }
        if self.rows.len() >= self.capacity {
            Flight::Busy
        } else {
            Flight::New
        }
    }

    /// Stores an answered flight that [`Self::classify`] called [`Flight::New`]. With two already
    /// pending, the one with the lower flight number goes (returned, so the caller can count it).
    /// `ticket_expires_ms` is the ticket's own expiry: the row lives until then.
    pub(crate) fn insert(
        &mut self,
        ticket: TicketId,
        ticket_expires_ms: u64,
        pending: PendingResume,
    ) -> Option<PendingResume> {
        let row = self.rows.entry(ticket).or_insert_with(|| Row {
            highest: 0,
            expires_ms: ticket_expires_ms,
            pending: Vec::with_capacity(PENDING_RESUMES_PER_TICKET),
        });
        row.highest = row.highest.max(pending.flight);
        let mut replaced = None;
        if row.pending.len() >= PENDING_RESUMES_PER_TICKET
            && let Some(lowest) = row
                .pending
                .iter()
                .enumerate()
                .min_by_key(|(_, p)| p.flight)
                .map(|(i, _)| i)
        {
            let old = row.pending.swap_remove(lowest);
            if self.by_token.get(&old.link.token()) == Some(&ticket) {
                self.by_token.remove(&old.link.token());
            }
            replaced = Some(old);
        }
        self.by_token.insert(pending.link.token(), ticket);
        row.pending.push(pending);
        replaced
    }

    /// The pending resume whose link has session `token`.
    pub(crate) fn get_mut(&mut self, token: u32) -> Option<&mut PendingResume> {
        let ticket = self.by_token.get(&token)?;
        self.rows
            .get_mut(ticket)?
            .pending
            .iter_mut()
            .find(|p| p.link.token() == token)
    }

    /// True when a pending resume's link has session `token`.
    pub(crate) fn has_token(&self, token: u32) -> bool {
        self.by_token.contains_key(&token)
    }

    /// Takes the pending resume with session `token` out of its row. The row stays, with its
    /// highest flight number, so the flight cannot be played again.
    pub(crate) fn take(&mut self, token: u32) -> Option<(TicketId, PendingResume)> {
        let ticket = self.by_token.remove(&token)?;
        let row = self.rows.get_mut(&ticket)?;
        let at = row.pending.iter().position(|p| p.link.token() == token)?;
        Some((ticket, row.pending.swap_remove(at)))
    }

    /// A resume of `ticket` was confirmed: the row leaves with whatever else was pending in it
    /// (the peer's epoch has made the ticket dead by now, ADR-8858 A2, A5). Returns how many
    /// other pending resumes went with it.
    pub(crate) fn confirmed(&mut self, ticket: &TicketId) -> usize {
        let Some(row) = self.rows.remove(ticket) else {
            return 0;
        };
        for p in &row.pending {
            if self.by_token.get(&p.link.token()) == Some(ticket) {
                self.by_token.remove(&p.link.token());
            }
        }
        row.pending.len()
    }

    /// Forgets pending resumes past their lifetime and the rows of expired tickets. Returns how
    /// many pending resumes lapsed unconfirmed.
    pub(crate) fn expire(&mut self, now_ms: u64) -> usize {
        let mut lapsed = 0;
        let by_token = &mut self.by_token;
        self.rows.retain(|ticket, row| {
            let dead = now_ms >= row.expires_ms;
            row.pending.retain(|p| {
                let keep = !dead && now_ms.saturating_sub(p.created_ms) < PENDING_TTL_MS;
                if !keep {
                    lapsed += 1;
                    if by_token.get(&p.link.token()) == Some(ticket) {
                        by_token.remove(&p.link.token());
                    }
                }
                keep
            });
            !dead
        });
        lapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr_block::AddrBlock;
    use crate::crypto::{
        CRYPTO_VERSION, ResumeCredential, ResumeFlight, ResumeHello, ResumeResponder, build_hash,
    };
    use crate::handshake::PeerRole;

    const PSK: [u8; 32] = [0x42; 32];
    const TICKET: [u8; RESUMPTION_TICKET_BYTES] = [7; RESUMPTION_TICKET_BYTES];

    fn addr() -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 2], 5000))
    }

    fn claims() -> TicketClaims {
        TicketClaims {
            peer: 3,
            epoch: 1,
            psk: PSK,
            block: AddrBlock::of(addr().ip()).key_bytes(),
            issued_ms: 0,
            expires_ms: 60_000,
            protocol_id: 9,
            build_hash: build_hash(""),
            password_verified: false,
            resume_id: [5; RESUME_TICKET_BYTES],
        }
    }

    /// A real pending resume: the flight of `credential`, answered with `token`.
    fn pending(credential: &ResumeCredential, token: u32, created_ms: u64) -> PendingResume {
        let hello = ResumeHello {
            flight: 0,
            name: "ada".into(),
            role: PeerRole::Player,
            build: String::new(),
            join_data: Vec::new(),
        };
        let flight =
            ResumeFlight::start(credential, 9, u64::from(token), CRYPTO_VERSION, hello).expect("flight");
        let responder = ResumeResponder::open(
            &PSK,
            9,
            u64::from(token),
            CRYPTO_VERSION,
            credential.ticket(),
            flight.message(),
        )
        .expect("opens");
        let number = responder.hello().flight;
        let join = responder.hello().clone().into_join_token(claims().resume_id);
        let (link, noise) = responder.reply(token).expect("reply");
        PendingResume {
            flight: number,
            noise: flight.message().to_vec(),
            reply: noise.to_vec(),
            link,
            client_nonce: u64::from(token),
            from: addr(),
            claims: claims(),
            join,
            target: ResumeTarget::Peer(PeerId(3)),
            ticket: [6; RESUME_TICKET_BYTES],
            resumption: None,
            accept_epoch: 0,
            epochs: ResumeEpochs::default(),
            created_ms,
        }
    }

    fn id(n: u8) -> TicketId {
        [n; 20]
    }

    #[test]
    fn the_ticket_id_is_the_key_id_and_the_nonce() {
        let mut ticket = [0u8; RESUMPTION_TICKET_BYTES];
        for (i, b) in ticket.iter_mut().enumerate() {
            *b = i as u8;
        }
        let id = ticket_id(&ticket);
        assert_eq!(id[0], 1);
        assert_eq!(id[19], 20);
    }

    /// ADR-8858 A3: flights are ordered by their own number. A higher one takes a free place; a
    /// third replaces the lower of the two; one at or below the highest is stale.
    #[test]
    fn a_ticket_keeps_two_pending_resumes_and_the_lower_flight_goes_first() {
        let credential = ResumeCredential::new(TICKET, PSK, None);
        let mut r = ResumeRegister::new(4);
        assert_eq!(r.classify(&id(1), 1, 0), Flight::New);
        let first = pending(&credential, 101, 0);
        let first_noise = first.noise.clone();
        assert_eq!(first.flight, 1);
        assert!(r.insert(id(1), 60_000, first).is_none());
        assert_eq!(r.classify(&id(1), 1, 0), Flight::Stale, "its own number again");
        assert_eq!(r.classify(&id(1), 2, 0), Flight::New);
        assert!(r.insert(id(1), 60_000, pending(&credential, 102, 0)).is_none());
        assert_eq!(r.pending_of(&id(1)), 2);
        assert!(r.replay(&id(1), &first_noise).is_some());

        // A third flight: the register still holds two, and the one that went is flight 1.
        assert_eq!(r.classify(&id(1), 3, 0), Flight::New);
        let replaced = r
            .insert(id(1), 60_000, pending(&credential, 103, 0))
            .expect("the lower flight goes");
        assert_eq!(replaced.flight, 1);
        assert_eq!(r.pending_of(&id(1)), 2);
        assert!(!r.has_token(101) && r.has_token(102) && r.has_token(103));
        assert!(
            r.replay(&id(1), &first_noise).is_none(),
            "no stored reply any more"
        );
        // The replaced flight, played again, is stale: it never comes back over a newer one.
        assert_eq!(r.classify(&id(1), 1, 0), Flight::Stale);
        assert_eq!(r.classify(&id(1), 3, 0), Flight::Stale);
        assert_eq!(r.len(), 1);
    }

    /// ADR-8858 A5: a row leaves when a resume of its ticket is confirmed or when the ticket
    /// expires, never to make room; a pending resume lapses after `PENDING_TTL_MS` and its row
    /// keeps the highest flight number.
    #[test]
    fn rows_leave_by_confirmation_or_expiry_and_a_full_register_is_busy() {
        let credential = ResumeCredential::new(TICKET, PSK, None);
        let mut r = ResumeRegister::new(1);
        assert_eq!(r.capacity(), MIN_TICKET_ROWS);
        for n in 0..MIN_TICKET_ROWS as u8 {
            assert_eq!(r.classify(&id(n), 1, 0), Flight::New);
            r.insert(
                id(n),
                60_000 + u64::from(n),
                pending(&credential, 200 + u32::from(n), 0),
            );
        }
        assert_eq!(r.len(), MIN_TICKET_ROWS);
        assert_eq!(
            r.classify(&id(99), 1, 0),
            Flight::Busy,
            "never evicted to make room"
        );
        assert_eq!(
            r.classify(&id(0), 9, 0),
            Flight::New,
            "a known ticket still flies"
        );

        // The pending resumes lapse; the rows and their flight numbers stay.
        assert_eq!(r.expire(PENDING_TTL_MS - 1), 0);
        assert_eq!(r.expire(PENDING_TTL_MS), MIN_TICKET_ROWS);
        assert!(r.no_pending());
        assert_eq!(r.len(), MIN_TICKET_ROWS);
        // Row 0 saw flight 1 (the credential counts its flights across every row here).
        assert_eq!(r.classify(&id(0), 1, PENDING_TTL_MS), Flight::Stale);
        assert_eq!(r.classify(&id(99), 1, PENDING_TTL_MS), Flight::Busy);

        // A confirmation frees its row, and with it the sibling flight.
        r.insert(id(0), 60_000, pending(&credential, 300, PENDING_TTL_MS));
        r.insert(id(0), 60_000, pending(&credential, 301, PENDING_TTL_MS));
        let (ticket, taken) = r.take(300).expect("pending");
        assert_eq!((ticket, taken.link.token()), (id(0), 300));
        assert_eq!(r.confirmed(&ticket), 1, "the sibling goes with the row");
        assert!(!r.has_token(301));
        assert_eq!(r.len(), MIN_TICKET_ROWS - 1);
        assert_eq!(r.classify(&id(99), 1, PENDING_TTL_MS), Flight::New);

        // Ticket expiry frees the rest, one by one as each ticket's time comes.
        assert_eq!(r.expire(60_001), 0);
        assert_eq!(r.len(), MIN_TICKET_ROWS - 2, "row 1 expired at 60,001");
        r.expire(70_000);
        assert_eq!(r.len(), 0);
    }

    /// A failed admission takes the pending resume out and leaves the row: the flight cannot be
    /// played again, and a higher one still can.
    #[test]
    fn a_taken_resume_leaves_its_row_behind() {
        let credential = ResumeCredential::new(TICKET, PSK, None);
        let mut r = ResumeRegister::new(4);
        r.insert(id(1), 60_000, pending(&credential, 7, 0));
        assert!(r.get_mut(7).is_some());
        assert!(r.take(7).is_some());
        assert!(r.take(7).is_none());
        assert!(r.get_mut(7).is_none());
        assert_eq!((r.len(), r.pending_of(&id(1))), (1, 0));
        assert_eq!(r.classify(&id(1), 1, 0), Flight::Stale);
        assert_eq!(r.classify(&id(1), 2, 0), Flight::New);
    }
}
