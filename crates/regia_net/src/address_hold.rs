// SPDX-License-Identifier: MPL-2.0
//! Per-address slot holds for players that left holding a reconnect ticket (ADR-8733, amends the
//! per-address cap of ADR-3283).
//!
//! `ServerConfig::max_peers_per_ip` bounds how many peers one address block (an IPv4 address, an
//! IPv6 /64) may hold, so one machine cannot take every slot. Before ADR-8733 the count read only
//! the live peers: a player behind a carrier-grade NAT that left and came back two seconds later
//! found its slot taken by another player of the same address, and was refused `full` for as long
//! as that one stayed. Now a player that leaves holding a valid resume ticket (a timeout that made
//! it dormant, or its own goodbye on a server that keeps tickets) leaves a hold on its block for
//! the resume grace:
//!
//! - A join that presents the ticket from the same block uses the held slot: it is never counted
//!   as a new peer of the block, whatever the block holds by then. The hold is *claimed* by the
//!   new peer at that join and spent only when the client confirms its accept (ADR-8883, the
//!   ADR-8535 rule for every resume ticket): until then a retry with the same ticket after a lost
//!   accept is that same peer, and a claimed hold is not counted again (the peer it brought back
//!   is). A join from another block neither uses nor spends the hold.
//! - A fresh join (no matching ticket) counts the block's live peers *and* its holds. It is refused
//!   when the live peers alone reach the cap (as before), or when live peers and holds together
//!   would pass the cap by more than [`HELD_SLOT_SLACK`].
//!
//! So a block never holds more than `max_peers_per_ip + HELD_SLOT_SLACK` peers through joins
//! (live plus held), however its players come and go: the cap stays an anti-abuse bound, and the
//! leave-and-come-back loop that a plain "a ticket rejoin skips the cap" rule would open (leave
//! with a ticket, let a fresh join take the slot, come back over the cap, repeat) stops at the
//! slack. Holds are bounded in number ([`MAX_HOLDS`], oldest first) and in time (the grace).

use std::collections::VecDeque;
use std::net::IpAddr;

use crate::addr_block::same_block;
use crate::handshake::RESUME_TICKET_BYTES;
use crate::wire::constant_time_eq;

/// Held slots one fresh join may borrow from its block beyond the cap (ADR-8733): with 0 a held
/// slot is strictly reserved and the player waiting behind the cap (the ninth behind one CGNAT
/// address while the eighth is away for two seconds) never gets in while the holder keeps coming
/// back; with 1 the waiting player takes the slot, the holder still comes back, and the block ends
/// one over the cap, never more.
pub(crate) const HELD_SLOT_SLACK: usize = 1;

/// Holds the table keeps at most; past it the oldest is dropped first (its player's return is
/// then an ordinary join).
pub(crate) const MAX_HOLDS: usize = 4_096;

/// One player that left on purpose holding a resume ticket.
#[derive(Debug, Clone)]
struct Hold {
    ip: IpAddr,
    ticket: [u8; RESUME_TICKET_BYTES],
    until_ms: u64,
    /// A join presented the ticket from the hold's block and its peer has not confirmed yet: the
    /// peer stands for the slot, so the hold neither counts nor matches again.
    claimed: bool,
}

/// The holds of players that left on purpose with a ticket. Dormant players (timeouts) are held
/// by the server's own dormant table; this one covers the goodbye, whose peer is gone.
#[derive(Debug, Default)]
pub(crate) struct AddressHolds {
    /// Pushed with a monotonic expiry (one grace for every hold), so expired holds leave from
    /// the front.
    holds: VecDeque<Hold>,
}

impl AddressHolds {
    /// Holds a slot of `ip`'s block for `ticket` until `until_ms`. An all-zero ticket (a server
    /// that keeps no tickets) holds nothing.
    pub(crate) fn insert(&mut self, ip: IpAddr, ticket: [u8; RESUME_TICKET_BYTES], until_ms: u64) {
        if ticket.iter().all(|b| *b == 0) {
            return;
        }
        if self.holds.len() >= MAX_HOLDS {
            self.holds.pop_front();
        }
        self.holds.push_back(Hold {
            ip,
            ticket,
            until_ms,
            claimed: false,
        });
    }

    /// Drops the holds whose grace ended at `now_ms`.
    pub(crate) fn expire(&mut self, now_ms: u64) {
        while self.holds.front().is_some_and(|h| h.until_ms <= now_ms) {
            self.holds.pop_front();
        }
        // A grace changed at run time breaks the push order; drop any stragglers too.
        if self.holds.iter().any(|h| h.until_ms <= now_ms) {
            self.holds.retain(|h| h.until_ms > now_ms);
        }
    }

    /// Live unclaimed holds on `ip`'s block (a claimed one is counted as its peer).
    pub(crate) fn held_in_block(&self, ip: IpAddr) -> usize {
        self.holds
            .iter()
            .filter(|h| !h.claimed && same_block(h.ip, ip))
            .count()
    }

    /// The address of the unclaimed hold `ticket` names, if any. Every hold is compared, so the
    /// time taken does not say which one matched.
    pub(crate) fn find(&self, ticket: &[u8; RESUME_TICKET_BYTES]) -> Option<IpAddr> {
        if ticket.iter().all(|b| *b == 0) {
            return None;
        }
        let mut found = None;
        for h in &self.holds {
            if constant_time_eq(&h.ticket, ticket) && !h.claimed {
                found = Some(h.ip);
            }
        }
        found
    }

    /// Marks the unclaimed hold `ticket` names as claimed by a join that is not confirmed yet.
    /// True when there was one.
    pub(crate) fn claim(&mut self, ticket: &[u8; RESUME_TICKET_BYTES]) -> bool {
        let mut claimed = false;
        // Every hold is compared, as in `find`.
        for h in &mut self.holds {
            if constant_time_eq(&h.ticket, ticket) && !claimed && !h.claimed {
                h.claimed = true;
                claimed = true;
            }
        }
        claimed
    }

    /// Spends the hold `ticket` names (its player is back). True when there was one.
    pub(crate) fn take(&mut self, ticket: &[u8; RESUME_TICKET_BYTES]) -> bool {
        let before = self.holds.len();
        self.holds.retain(|h| !constant_time_eq(&h.ticket, ticket));
        self.holds.len() != before
    }

    /// Holds in the table.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.holds.len()
    }
}

/// Whether a fresh join (no ticket of its own block) is refused by the per-address cap: `live`
/// peers and `held` slots on its block, `cap` the configured `max_peers_per_ip` (above 0).
pub(crate) fn fresh_join_refused(live: usize, held: usize, cap: usize) -> bool {
    // Admitting it makes `live + held + 1` on the block: at most `cap + HELD_SLOT_SLACK`.
    live >= cap || live + held >= cap + HELD_SLOT_SLACK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn t(n: u8) -> [u8; RESUME_TICKET_BYTES] {
        [n; RESUME_TICKET_BYTES]
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(100, 64, 0, last))
    }

    #[test]
    fn a_hold_is_found_by_its_ticket_and_spent_once() {
        let mut h = AddressHolds::default();
        h.insert(ip(1), t(7), 1_000);
        assert_eq!(h.find(&t(7)), Some(ip(1)));
        assert_eq!(h.find(&t(8)), None);
        assert_eq!(h.held_in_block(ip(1)), 1);
        assert_eq!(h.held_in_block(ip(2)), 0);
        assert!(h.take(&t(7)));
        assert!(!h.take(&t(7)), "a hold is spent once");
        assert_eq!(h.held_in_block(ip(1)), 0);
    }

    /// ADR-8883: a claimed hold stands for the peer that claimed it: it is neither counted nor
    /// found again, and it is spent once (at the confirmation, or when that peer is gone).
    #[test]
    fn a_claimed_hold_is_not_counted_or_found_again() {
        let mut h = AddressHolds::default();
        h.insert(ip(1), t(7), 1_000);
        h.insert(ip(1), t(8), 1_000);
        assert!(h.claim(&t(7)));
        assert!(!h.claim(&t(7)), "one claim per hold");
        assert_eq!(h.find(&t(7)), None);
        assert_eq!(h.find(&t(8)), Some(ip(1)));
        assert_eq!(h.held_in_block(ip(1)), 1, "the claimed slot is its peer's now");
        assert!(h.take(&t(7)));
        assert!(!h.take(&t(7)));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn a_zero_ticket_holds_nothing() {
        let mut h = AddressHolds::default();
        h.insert(ip(1), [0; RESUME_TICKET_BYTES], 1_000);
        assert_eq!(h.len(), 0);
        assert_eq!(h.find(&[0; RESUME_TICKET_BYTES]), None);
    }

    #[test]
    fn a_hold_lapses_at_the_end_of_its_grace() {
        let mut h = AddressHolds::default();
        h.insert(ip(1), t(1), 1_000);
        h.insert(ip(1), t(2), 2_000);
        h.expire(999);
        assert_eq!(h.held_in_block(ip(1)), 2);
        h.expire(1_000);
        assert_eq!(h.held_in_block(ip(1)), 1);
        assert_eq!(h.find(&t(1)), None);
        h.expire(5_000);
        assert_eq!(h.len(), 0);
    }

    #[test]
    fn the_table_is_bounded_and_the_oldest_hold_goes_first() {
        let mut h = AddressHolds::default();
        for i in 0..=MAX_HOLDS {
            let mut ticket = [1; RESUME_TICKET_BYTES];
            ticket[..8].copy_from_slice(&(i as u64).to_le_bytes());
            h.insert(ip(1), ticket, 10_000 + i as u64);
        }
        assert_eq!(h.len(), MAX_HOLDS);
        let mut first = [1; RESUME_TICKET_BYTES];
        first[..8].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(h.find(&first), None, "the oldest hold was dropped");
    }

    #[test]
    fn an_ipv6_hold_covers_its_64() {
        let mut h = AddressHolds::default();
        let a: IpAddr = "2001:db8:1:2::10".parse().expect("ip");
        let b: IpAddr = "2001:db8:1:2::99".parse().expect("ip");
        let c: IpAddr = "2001:db8:1:3::10".parse().expect("ip");
        h.insert(a, t(3), 1_000);
        assert_eq!(h.held_in_block(b), 1);
        assert_eq!(h.held_in_block(c), 0);
    }

    /// The bound: live peers alone never pass the cap through fresh joins, and live plus held
    /// never pass it by more than the slack.
    #[test]
    fn fresh_joins_borrow_at_most_the_slack() {
        let cap = 8;
        // No holds: exactly the old rule.
        assert!(!fresh_join_refused(7, 0, cap));
        assert!(fresh_join_refused(8, 0, cap));
        // One holder away: the waiting player borrows its slot.
        assert!(!fresh_join_refused(7, 1, cap));
        // Two away: one borrows, the next is refused.
        assert!(!fresh_join_refused(6, 2, cap));
        assert!(fresh_join_refused(7, 2, cap));
        // Whatever comes and goes, live + held stays within cap + slack after an admitted join.
        for live in 0..=cap + 2 {
            for held in 0..=cap + 2 {
                if !fresh_join_refused(live, held, cap) {
                    assert!(live < cap && live + held < cap + HELD_SLOT_SLACK);
                }
            }
        }
    }

    /// The leave-and-come-back loop the slack stops: a holder leaves, a fresh join takes the slot,
    /// the holder comes back; repeated, the block never passes cap + slack.
    #[test]
    fn churn_never_grows_a_block_past_the_slack() {
        let cap = 8;
        let (mut live, mut held) = (cap, 0usize);
        for _ in 0..100 {
            // Everyone who can leaves with a ticket, then fresh joins fill what they may.
            let away = live;
            live -= away;
            held += away;
            while !fresh_join_refused(live, held, cap) {
                live += 1;
            }
            // Every holder comes back (a return is never refused by the cap).
            live += held;
            held = 0;
            assert!(live <= cap + HELD_SLOT_SLACK, "live {live}");
        }
    }
}
