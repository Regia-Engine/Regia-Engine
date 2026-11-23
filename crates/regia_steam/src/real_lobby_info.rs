// SPDX-License-Identifier: MPL-2.0
//! The [`LobbyInfo`] the real Steam lobby backend builds from what ISteamMatchmaking returned: a
//! lobby's raw key/value data and its scalar facts. No `steamworks` types, so the hostile-data
//! property test below runs without the feature (rc1 security review finding 6).
//!
//! Steam data comes from other players (the lobby owner writes it), so nothing here trusts it:
//! the data keeps at most `MAX_KEYS * 2` entries (the first ones in Steam's index order), the
//! launched server is read only from a `regia.server` value that parses, and counts wider than
//! `u32` saturate instead of wrapping.

use std::collections::BTreeMap;

use regia_net::lobby::{
    LobbyId, LobbyInfo, LobbyMember, LobbyServer, LobbyVisibility, MAX_KEYS, MemberId, keys,
};

/// The most lobby data entries one [`LobbyInfo`] keeps (Steam's own cap plus headroom; the
/// backend reads no more indices than this).
pub(crate) const MAX_DATA_ENTRIES: usize = MAX_KEYS * 2;

/// What the backend knows about a lobby besides its key/value data.
#[derive(Debug, Clone)]
pub(crate) struct LobbyFacts {
    /// Its id.
    pub id: LobbyId,
    /// The current owner.
    pub owner: MemberId,
    /// The visibility this client saw it created or joined with.
    pub visibility: LobbyVisibility,
    /// Whether others may join.
    pub joinable: bool,
    /// Steam's member limit, `None` when Steam does not report one.
    pub max_members: Option<usize>,
    /// Steam's member count.
    pub member_count: usize,
    /// The members (empty for a listing).
    pub members: Vec<LobbyMember>,
}

/// Builds the lobby from its facts and raw data pairs (in Steam's index order).
pub(crate) fn lobby_info(facts: LobbyFacts, data: impl IntoIterator<Item = (String, String)>) -> LobbyInfo {
    let data: BTreeMap<String, String> = data.into_iter().take(MAX_DATA_ENTRIES).collect();
    let server = data.get(keys::SERVER).and_then(|s| LobbyServer::from_key(s));
    LobbyInfo {
        id: facts.id,
        owner: facts.owner,
        visibility: facts.visibility,
        max_members: facts.max_members.map_or(0, saturate),
        joinable: facts.joinable,
        member_count: saturate(facts.member_count),
        members: facts.members,
        data,
        server,
    }
}

fn saturate(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn step(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.step() % n.max(1)
        }

        fn text(&mut self, max_len: u64) -> String {
            let len = self.below(max_len + 1);
            (0..len)
                .map(|_| match self.below(8) {
                    0 => '\0',
                    1 => char::from_u32(0x80 + self.below(0x2000) as u32).unwrap_or('?'),
                    2 => ':',
                    3 => '.',
                    _ => char::from(b'0' + self.below(75) as u8),
                })
                .collect()
        }
    }

    /// The reference: `n` clamped to `u32` through `u64`.
    fn clamp_u32(n: usize) -> u32 {
        let wide = u64::try_from(n).unwrap_or(u64::MAX).min(u64::from(u32::MAX));
        u32::try_from(wide).unwrap_or(u32::MAX)
    }

    const BAD_SERVERS: &[&str] = &[
        "",
        "udp:",
        "steam:",
        "udp:1.2.3.4",
        "udp:1.2.3.4:99999",
        "udp:1.2.3.4:-1",
        "udp:256.0.0.1:80",
        "udp:[::1]",
        "steam:0",
        "steam:-5",
        "steam:abc",
        "steam:18446744073709551616",
        "steam:1e9",
        "steam: 7",
        "tcp:1.2.3.4:80",
        "udp:1.2.3.4:80\0",
    ];

    const GOOD_SERVERS: &[&str] = &["udp:127.0.0.1:7777", "udp:[::1]:80", "steam:76561197960287930"];

    fn facts(rng: &mut Rng) -> LobbyFacts {
        let wide = [0, 1, 250, usize::MAX / 2 + 1, usize::MAX];
        LobbyFacts {
            id: LobbyId(rng.step()),
            owner: MemberId(rng.step()),
            visibility: [
                LobbyVisibility::Public,
                LobbyVisibility::FriendsOnly,
                LobbyVisibility::Private,
                LobbyVisibility::Invisible,
            ][rng.below(4) as usize],
            joinable: rng.below(2) == 0,
            max_members: (rng.below(6) != 0).then(|| wide[rng.below(5) as usize]),
            member_count: wide[rng.below(5) as usize],
            members: Vec::new(),
        }
    }

    /// Hostile Steam data (oversize keys and values, NUL bytes, up to three times the entry cap,
    /// duplicate keys, a `regia.server` with bad numbers) never panics, keeps the first
    /// `MAX_DATA_ENTRIES` pairs, reads a server only from a value that parses, and saturates
    /// counts. 600 seeded maps; deterministic.
    #[test]
    fn hostile_lobby_data_builds_a_bounded_lobby() {
        let mut rng = Rng(0x5eed_1ab0_c0ff_ee01);
        for case in 0..600 {
            let facts = facts(&mut rng);
            let entries = match case % 4 {
                0 => rng.below(MAX_DATA_ENTRIES as u64 + 1),
                1 => (MAX_KEYS * 2) as u64,
                2 => (MAX_KEYS * 2) as u64 + 1,
                _ => rng.below(MAX_DATA_ENTRIES as u64 * 3 + 1),
            };
            let mut pairs: Vec<(String, String)> = Vec::new();
            for _ in 0..entries {
                let key = match rng.below(6) {
                    0 => keys::SERVER.to_owned(),
                    1 => rng.text(600),
                    2 => keys::ALL[rng.below(keys::ALL.len() as u64) as usize].to_owned(),
                    _ => rng.text(24),
                };
                let value = match rng.below(6) {
                    0 => BAD_SERVERS[rng.below(BAD_SERVERS.len() as u64) as usize].to_owned(),
                    1 => GOOD_SERVERS[rng.below(GOOD_SERVERS.len() as u64) as usize].to_owned(),
                    2 => rng.text(20_000),
                    _ => rng.text(32),
                };
                pairs.push((key, value));
            }
            let expected: BTreeMap<String, String> = pairs.iter().take(MAX_DATA_ENTRIES).cloned().collect();
            let want = facts.clone();
            let info = lobby_info(facts, pairs);
            assert!(info.data.len() <= MAX_DATA_ENTRIES, "case {case}");
            assert_eq!(info.data, expected, "case {case}");
            assert_eq!(
                info.server,
                expected.get(keys::SERVER).and_then(|s| LobbyServer::from_key(s)),
                "case {case}"
            );
            if let Some(text) = expected.get(keys::SERVER)
                && BAD_SERVERS.contains(&text.as_str())
            {
                assert_eq!(info.server, None, "case {case}: `{text}` is not a server");
            }
            assert_eq!((info.id, info.owner), (want.id, want.owner), "case {case}");
            assert_eq!((info.visibility, info.joinable), (want.visibility, want.joinable));
            assert_eq!(info.max_members, want.max_members.map_or(0, clamp_u32));
            assert_eq!(info.member_count, clamp_u32(want.member_count));
        }
    }

    /// A good server reads back; counts wider than `u32` saturate rather than wrap (on a 64-bit
    /// host `usize::MAX / 2 + 1` truncated by `as u32` is 0).
    #[test]
    fn a_good_server_reads_and_wide_counts_saturate() {
        let facts = LobbyFacts {
            id: LobbyId(9),
            owner: MemberId(7),
            visibility: LobbyVisibility::Public,
            joinable: true,
            max_members: Some(usize::MAX),
            member_count: usize::MAX / 2 + 1,
            members: Vec::new(),
        };
        let info = lobby_info(facts, [(keys::SERVER.to_owned(), "steam:42".to_owned())]);
        assert_eq!(info.server, Some(LobbyServer::Steam(42)));
        assert_eq!(
            (info.max_members, info.member_count),
            (clamp_u32(usize::MAX), clamp_u32(usize::MAX / 2 + 1))
        );
        assert_eq!(info.max_members, u32::MAX);
    }
}
