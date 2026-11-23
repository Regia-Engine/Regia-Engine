// SPDX-License-Identifier: MPL-2.0
//! Resume tickets and reconnect tokens a client may still hold because it never confirmed the
//! answer that replaced them (ADR-8535, ADR-8623).
//!
//! A resume mints a new ticket (transport) or token (session) and sends it in an answer the client
//! may not have heard yet, or heard while a datagram of an earlier attempt, carrying an older
//! one, is still in flight. Until the client shows it holds the newest one (its first
//! authenticated payload, or a message only a welcomed client sends), every ticket of that chain
//! still names the same peer. One deep was not enough: two resumes before a confirmation dropped
//! the first ticket, and the attempt still carrying it became a second join.
//!
//! Bounded both ways:
//! - At most [`MAX_UNCONFIRMED`] rows per peer. Each row is one resume the client did not confirm.
//!   Past that the row presented or superseded longest ago goes first; an attempt carrying it is a
//!   fresh join, the answer the spec already gives a spent ticket.
//! - Each row lives for the caller's lifetime (the resume grace) after it was last superseded or
//!   presented. A ticket a client keeps presenting stays good while it keeps trying; one nobody
//!   presents lapses one grace window after it was replaced, never later.
//!
//! The whole chain is spent at once when the client confirms.

/// Bytes of a resume ticket (transport) and of a reconnect token (session): both 16.
pub(crate) const TOKEN_BYTES: usize = 16;

const _: () = assert!(
    TOKEN_BYTES == crate::handshake::RESUME_TICKET_BYTES
        && TOKEN_BYTES == crate::session::proto::RECONNECT_TOKEN_BYTES
);

/// Unconfirmed tickets one peer keeps at most. Each is one resume in a row that the client did not
/// confirm; the wifi-to-cell and long-outage profiles need two or three. Eight leaves room for a
/// link that loses several confirmations in a row and keeps each compare pass at nine 16-byte
/// constant-time compares per held peer.
pub(crate) const MAX_UNCONFIRMED: usize = 8;

/// The unconfirmed tickets of one peer, each with the server clock at which it lapses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UnconfirmedTokens {
    rows: Vec<([u8; TOKEN_BYTES], u64)>,
}

impl UnconfirmedTokens {
    /// No row: the client confirmed, or never resumed.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Rows held, lapsed or not.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// The chain after a resume of this peer that presented `presented`, when the peer's newest
    /// ticket was `newest`: the newest one is superseded by the ticket about to be minted, and the
    /// presented one is shown to be still held. Both are good until `now_ms + lifetime_ms`; the
    /// other rows keep their own expiry. Lapsed rows are dropped, and past [`MAX_UNCONFIRMED`] the
    /// row that lapses first goes.
    pub(crate) fn after_resume(
        mut self,
        newest: [u8; TOKEN_BYTES],
        presented: [u8; TOKEN_BYTES],
        now_ms: u64,
        lifetime_ms: u64,
    ) -> Self {
        let until = now_ms.saturating_add(lifetime_ms);
        self.rows.retain(|&(_, lapse)| lapse > now_ms);
        for token in [newest, presented] {
            if token == [0; TOKEN_BYTES] {
                continue;
            }
            match self.rows.iter_mut().find(|(t, _)| *t == token) {
                Some(row) => row.1 = row.1.max(until),
                None => self.rows.push((token, until)),
            }
        }
        while self.rows.len() > MAX_UNCONFIRMED {
            let Some(first) = self
                .rows
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, lapse))| *lapse)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.rows.remove(first);
        }
        self
    }

    /// True when `token` is one of the rows and has not lapsed at `now_ms`. Every row is compared,
    /// so the time taken does not say which one matched. The all-zero token never matches.
    pub(crate) fn matches(&self, token: &[u8; TOKEN_BYTES], now_ms: u64) -> bool {
        let mut hit = false;
        for (row, lapse) in &self.rows {
            hit |= crate::wire::constant_time_eq(row, token) & (*lapse > now_ms);
        }
        hit & (*token != [0; TOKEN_BYTES])
    }

    /// The client confirmed the newest ticket: every older one is spent for good.
    pub(crate) fn confirm(&mut self) {
        self.rows.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: u8) -> [u8; TOKEN_BYTES] {
        [n; TOKEN_BYTES]
    }

    #[test]
    fn a_chain_keeps_every_unconfirmed_ticket() {
        let chain = UnconfirmedTokens::default()
            .after_resume(t(1), t(1), 0, 1_000)
            .after_resume(t(2), t(2), 10, 1_000)
            .after_resume(t(3), t(1), 20, 1_000);
        for n in 1..=3 {
            assert!(chain.matches(&t(n), 30), "ticket {n}");
        }
        assert!(!chain.matches(&t(4), 30));
        assert!(!chain.matches(&[0; TOKEN_BYTES], 30));
    }

    #[test]
    fn a_row_lapses_one_lifetime_after_it_was_last_seen() {
        let chain = UnconfirmedTokens::default()
            .after_resume(t(1), t(1), 0, 1_000)
            .after_resume(t(2), t(2), 500, 1_000);
        assert!(chain.matches(&t(1), 999));
        assert!(!chain.matches(&t(1), 1_000), "superseded at 0, lifetime 1000");
        assert!(chain.matches(&t(2), 1_499));
        // Presenting a row again keeps it good while the client keeps trying.
        let chain = chain.after_resume(t(3), t(1), 900, 1_000);
        assert!(chain.matches(&t(1), 1_899));
        assert!(!chain.matches(&t(1), 1_900));
        // A lapsed row is dropped at the next resume.
        let chain = chain.after_resume(t(4), t(4), 1_950, 1_000);
        assert!(!chain.rows.iter().any(|(row, _)| *row == t(1)));
    }

    #[test]
    fn the_chain_is_bounded_and_the_stalest_row_goes_first() {
        let mut chain = UnconfirmedTokens::default();
        // The client keeps presenting ticket 1 while every new accept is lost.
        for n in 0..20u8 {
            chain = chain.after_resume(t(n + 2), t(1), u64::from(n), 1_000);
            assert!(chain.len() <= MAX_UNCONFIRMED);
            assert!(
                chain.matches(&t(1), u64::from(n)),
                "the ticket still presented stays"
            );
        }
        assert_eq!(chain.len(), MAX_UNCONFIRMED);
        assert!(!chain.matches(&t(2), 20), "the oldest unheard ticket went first");
        assert!(chain.matches(&t(21), 20), "the newest superseded ticket stays");
    }

    #[test]
    fn a_confirmation_spends_the_whole_chain() {
        let mut chain = UnconfirmedTokens::default()
            .after_resume(t(1), t(1), 0, 1_000)
            .after_resume(t(2), t(1), 1, 1_000);
        chain.confirm();
        assert!(chain.is_empty());
        assert!(!chain.matches(&t(1), 2));
        assert!(!chain.matches(&t(2), 2));
    }
}
