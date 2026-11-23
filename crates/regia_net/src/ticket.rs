// SPDX-License-Identifier: MPL-2.0
//! Store-neutral join ticket hook.
//!
//! A host that wants a store to vouch for a joining player installs a [`JoinTicketCheck`] on the
//! server. The server calls it after the password gate, after `NetGame::validate_join`, and after
//! the live-peer, per-address and capacity checks, immediately before the peer is added. A refusal
//! denies the join with [`TicketRefused::CODE`]. When a check opens a store session, the server
//! ends it from [`JoinTicketCheck::end_ticket`] if that join is then denied, and again when the
//! peer leaves. No hook means admission is unchanged. The trait is store-neutral: Steam's
//! implementation lives in `regia_steam` and is not linked here.

use std::fmt;

/// Refuses a join because the store ticket did not pass.
///
/// The wire reason stays `DenyReason::InvalidJoin`. The detail text, and `ClientEnd::as_str`,
/// are [`TicketRefused::CODE`], so a host can tell a ticket refusal from other bad join data
/// without a new deny byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TicketRefused;

impl TicketRefused {
    /// Agent, log and deny-detail code.
    pub const CODE: &'static str = "ticket_refused";
}

impl fmt::Display for TicketRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Self::CODE)
    }
}

impl std::error::Error for TicketRefused {}

/// What a store says about a join ticket.
///
/// [`TicketVerdict::Admit`] and [`TicketVerdict::Refuse`] are final.
/// [`TicketVerdict::Pending`] means the ticket parsed and the peer may enter, but the store can
/// still refuse later. The server kicks that peer when the hook reports the ticket from
/// [`JoinTicketCheck::take_refusals`], or when the host calls `NetServer::finish_ticket`. A hook
/// that only implements [`JoinTicketCheck::check_ticket`] never returns `Pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketVerdict {
    /// The ticket is acceptable. No later store callback is expected.
    Admit,
    /// Refuse the join now. The server denies with [`TicketRefused::CODE`].
    Refuse,
    /// The ticket parsed. The peer is admitted, and a later refusal kicks them.
    Pending,
}

/// Decides whether a join's store ticket may enter.
///
/// `ticket` is whatever `NetGame::join_ticket` returned. An empty slice is still the hook's
/// decision: some stores treat "no ticket" as a refusal, and a host with no hook never calls this.
pub trait JoinTicketCheck: Send + Sync {
    /// `Ok(())` admits the player. `Err` denies the join with [`TicketRefused::CODE`].
    ///
    /// The server calls [`Self::verdict`], whose default body calls this once.
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused>;

    /// The store's verdict. The default maps [`Self::check_ticket`] onto [`TicketVerdict::Admit`]
    /// or [`TicketVerdict::Refuse`].
    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        match self.check_ticket(ticket) {
            Ok(()) => TicketVerdict::Admit,
            Err(_) => TicketVerdict::Refuse,
        }
    }

    /// Ends the store session [`Self::verdict`] opened for `ticket`.
    ///
    /// The server calls this when that peer leaves, and when admission fails after a successful
    /// verdict. The default does nothing.
    fn end_ticket(&self, _ticket: &[u8]) {}

    /// Tickets a late store callback has refused.
    ///
    /// The server kicks each connected peer whose stored ticket is here. The default is empty.
    /// A hook that returns [`TicketVerdict::Pending`] reports the ticket here when the callback
    /// refuses it. Returning the same ticket again is harmless once the peer has been kicked:
    /// [`Self::end_ticket`] has already dropped that session.
    fn take_refusals(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

/// A shared hook: one store check installed on several servers (a `NetSession` that hosts again,
/// a `NetLink`, the dedicated host; KI-NT-047, ADR-10069). Every call goes to the inner check.
impl<T: JoinTicketCheck + ?Sized> JoinTicketCheck for std::sync::Arc<T> {
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused> {
        (**self).check_ticket(ticket)
    }

    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        (**self).verdict(ticket)
    }

    fn end_ticket(&self, ticket: &[u8]) {
        (**self).end_ticket(ticket);
    }

    fn take_refusals(&self) -> Vec<Vec<u8>> {
        (**self).take_refusals()
    }
}
