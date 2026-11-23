// SPDX-License-Identifier: MPL-2.0
//! Lobby list queries (spec 89 § 3.2) and their deterministic evaluation.
//!
//! The filters mirror Steam's lobby list filters (string and number comparisons, near-value sort,
//! open slots, distance, result count) so one query runs on either backend. The local backend
//! evaluates them with [`LobbyQuery::evaluate`]; Steam evaluates them on its servers.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use super::{LobbyError, LobbyInfo, LobbyVisibility, MAX_LIST_RESULTS, check_key, check_value};

/// A comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compare {
    /// Equal.
    Eq,
    /// Not equal (a missing key matches).
    Ne,
    /// Less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Ge,
}

impl Compare {
    /// Parses `eq`, `ne`, `lt`, `le`, `gt`, `ge`.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "eq" => Self::Eq,
            "ne" => Self::Ne,
            "lt" => Self::Lt,
            "le" => Self::Le,
            "gt" => Self::Gt,
            "ge" => Self::Ge,
            _ => return None,
        })
    }

    /// The stable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Gt => "gt",
            Self::Ge => "ge",
        }
    }

    /// Whether `actual <op> wanted` holds for this ordering of `actual` against `wanted`.
    fn holds(self, ord: Ordering) -> bool {
        match self {
            Self::Eq => ord == Ordering::Equal,
            Self::Ne => ord != Ordering::Equal,
            Self::Lt => ord == Ordering::Less,
            Self::Le => ord != Ordering::Greater,
            Self::Gt => ord == Ordering::Greater,
            Self::Ge => ord != Ordering::Less,
        }
    }
}

/// How far a lobby may be (Steam's region distance filter; the local backend treats every lobby
/// as close).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Distance {
    /// Same or nearby region.
    Close,
    /// The platform's default radius.
    #[default]
    Default,
    /// Farther regions too.
    Far,
    /// Anywhere.
    Worldwide,
}

impl Distance {
    /// Parses `close`, `default`, `far`, `worldwide`.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "close" => Self::Close,
            "default" => Self::Default,
            "far" => Self::Far,
            "worldwide" => Self::Worldwide,
            _ => return None,
        })
    }

    /// The stable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Close => "close",
            Self::Default => "default",
            Self::Far => "far",
            Self::Worldwide => "worldwide",
        }
    }
}

/// `key <op> value`, compared as bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StringFilter {
    /// The lobby data key.
    pub key: String,
    /// The value to compare with.
    pub value: String,
    /// The comparison.
    pub op: Compare,
}

/// `key <op> value`, the lobby's value parsed as `i32` (unparsable never matches).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumberFilter {
    /// The lobby data key.
    pub key: String,
    /// The number to compare with.
    pub value: i32,
    /// The comparison.
    pub op: Compare,
}

/// Sorts results closest to `value` on `key` first (no filtering).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NearFilter {
    /// The lobby data key.
    pub key: String,
    /// The target.
    pub value: i32,
}

/// A lobby list request (spec 89 § 3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyQuery {
    /// String comparisons; all must hold.
    pub strings: Vec<StringFilter>,
    /// Number comparisons; all must hold.
    pub numbers: Vec<NumberFilter>,
    /// Near-value sort keys; earlier ones dominate.
    pub near: Vec<NearFilter>,
    /// At least this many free slots.
    pub open_slots: Option<u32>,
    /// Region distance.
    pub distance: Distance,
    /// At most this many results, `1..=50`.
    pub max_results: u32,
}

impl Default for LobbyQuery {
    fn default() -> Self {
        Self {
            strings: Vec::new(),
            numbers: Vec::new(),
            near: Vec::new(),
            open_slots: None,
            distance: Distance::Default,
            max_results: MAX_LIST_RESULTS,
        }
    }
}

impl LobbyQuery {
    /// Adds `key == value` (builder style).
    pub fn with_string(mut self, key: &str, op: Compare, value: &str) -> Self {
        self.strings.push(StringFilter {
            key: key.into(),
            value: value.into(),
            op,
        });
        self
    }

    /// Adds `regia.mode == mode` (builder style, ADR-8897): lobbies whose host publishes that
    /// rule-set id or mode.
    pub fn with_mode(self, mode: &str) -> Self {
        self.with_string(super::keys::MODE, Compare::Eq, mode)
    }

    /// Adds `regia.playlist == playlist` (builder style, ADR-8897).
    pub fn with_playlist(self, playlist: &str) -> Self {
        self.with_string(super::keys::PLAYLIST, Compare::Eq, playlist)
    }

    /// Adds a number comparison (builder style).
    pub fn with_number(mut self, key: &str, op: Compare, value: i32) -> Self {
        self.numbers.push(NumberFilter {
            key: key.into(),
            value,
            op,
        });
        self
    }

    /// Adds a near-value sort key (builder style).
    pub fn with_near(mut self, key: &str, value: i32) -> Self {
        self.near.push(NearFilter {
            key: key.into(),
            value,
        });
        self
    }

    /// Checks keys, values and the result count.
    pub fn validate(&self) -> Result<(), LobbyError> {
        if self.max_results == 0 || self.max_results > MAX_LIST_RESULTS {
            return Err(LobbyError::QueryInvalid("max_results must be 1..=50".into()));
        }
        let keys = self
            .strings
            .iter()
            .map(|f| f.key.as_str())
            .chain(self.numbers.iter().map(|f| f.key.as_str()))
            .chain(self.near.iter().map(|f| f.key.as_str()));
        for key in keys {
            check_key(key).map_err(|_| LobbyError::QueryInvalid(format!("bad key `{key}`")))?;
        }
        for f in &self.strings {
            check_value(&f.value).map_err(|_| LobbyError::QueryInvalid("string value too long".into()))?;
        }
        if self.strings.len() + self.numbers.len() + self.near.len() > super::MAX_KEYS {
            return Err(LobbyError::QueryInvalid("at most 64 filters".into()));
        }
        Ok(())
    }

    /// Whether `lobby` passes every filter (visibility, joinable, full, open slots, string and
    /// number comparisons).
    pub fn matches(&self, lobby: &LobbyInfo) -> bool {
        if lobby.visibility != LobbyVisibility::Public || !lobby.joinable || lobby.open_slots() == 0 {
            return false;
        }
        if self.open_slots.is_some_and(|n| lobby.open_slots() < n) {
            return false;
        }
        let strings_ok = self.strings.iter().all(|f| match lobby.data.get(&f.key) {
            Some(v) => f.op.holds(v.as_bytes().cmp(f.value.as_bytes())),
            None => f.op == Compare::Ne,
        });
        let numbers_ok =
            self.numbers.iter().all(
                |f| match lobby.data.get(&f.key).map(|v| v.trim().parse::<i32>()) {
                    Some(Ok(v)) => f.op.holds(v.cmp(&f.value)),
                    Some(Err(_)) => false,
                    None => f.op == Compare::Ne,
                },
            );
        strings_ok && numbers_ok
    }

    /// One distance per near filter; `u64::MAX` for a missing or unparsable value (sorts last).
    fn near_key(&self, lobby: &LobbyInfo) -> Vec<u64> {
        self.near
            .iter()
            .map(
                |f| match lobby.data.get(&f.key).map(|v| v.trim().parse::<i32>()) {
                    Some(Ok(v)) => (i64::from(v) - i64::from(f.value)).unsigned_abs(),
                    _ => u64::MAX,
                },
            )
            .collect()
    }

    /// Filters and orders `lobbies` (spec 89 § 3.2): near distance in filter order, then member
    /// count descending, then lobby id ascending; at most `max_results`. Member lists are cleared
    /// (listings carry counts only).
    pub fn evaluate<'a>(&self, lobbies: impl IntoIterator<Item = &'a LobbyInfo>) -> Vec<LobbyInfo> {
        let mut hits: Vec<(Vec<u64>, &LobbyInfo)> = lobbies
            .into_iter()
            .filter(|l| self.matches(l))
            .map(|l| (self.near_key(l), l))
            .collect();
        hits.sort_by(|(ka, a), (kb, b)| {
            ka.cmp(kb)
                .then(b.member_count.cmp(&a.member_count))
                .then(a.id.cmp(&b.id))
        });
        hits.into_iter()
            .take(self.max_results as usize)
            .map(|(_, l)| LobbyInfo {
                members: Vec::new(),
                ..l.clone()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::lobby::{LobbyId, MemberId};

    fn lobby(id: u64, members: u32, data: &[(&str, &str)]) -> LobbyInfo {
        LobbyInfo {
            id: LobbyId(id),
            owner: MemberId(1),
            visibility: LobbyVisibility::Public,
            max_members: 4,
            joinable: true,
            members: Vec::new(),
            member_count: members,
            data: data
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
            server: None,
        }
    }

    fn ids(v: &[LobbyInfo]) -> Vec<u64> {
        v.iter().map(|l| l.id.0).collect()
    }

    #[test]
    fn every_comparison_on_strings_and_numbers() {
        let set = [
            lobby(1, 1, &[("map", "alpha"), ("skill", "10")]),
            lobby(2, 1, &[("map", "beta"), ("skill", "20")]),
            lobby(3, 1, &[("map", "gamma"), ("skill", "x")]),
            lobby(4, 1, &[]),
        ];
        let q = |op, v| LobbyQuery::default().with_string("map", op, v);
        assert_eq!(ids(&q(Compare::Eq, "beta").evaluate(&set)), [2]);
        assert_eq!(
            ids(&q(Compare::Ne, "beta").evaluate(&set)),
            [1, 3, 4],
            "missing key matches ne"
        );
        assert_eq!(ids(&q(Compare::Lt, "beta").evaluate(&set)), [1]);
        assert_eq!(ids(&q(Compare::Le, "beta").evaluate(&set)), [1, 2]);
        assert_eq!(ids(&q(Compare::Gt, "beta").evaluate(&set)), [3]);
        assert_eq!(ids(&q(Compare::Ge, "beta").evaluate(&set)), [2, 3]);
        let n = |op, v| LobbyQuery::default().with_number("skill", op, v);
        assert_eq!(ids(&n(Compare::Eq, 10).evaluate(&set)), [1]);
        assert_eq!(
            ids(&n(Compare::Ne, 10).evaluate(&set)),
            [2, 4],
            "unparsable never matches"
        );
        assert_eq!(ids(&n(Compare::Lt, 20).evaluate(&set)), [1]);
        assert_eq!(ids(&n(Compare::Le, 20).evaluate(&set)), [1, 2]);
        assert_eq!(ids(&n(Compare::Gt, 10).evaluate(&set)), [2]);
        assert_eq!(ids(&n(Compare::Ge, 10).evaluate(&set)), [1, 2]);
    }

    #[test]
    fn hidden_closed_and_full_lobbies_are_never_listed() {
        let mut private = lobby(1, 1, &[]);
        private.visibility = LobbyVisibility::Private;
        let mut closed = lobby(2, 1, &[]);
        closed.joinable = false;
        let full = lobby(3, 4, &[]);
        let three_free = lobby(4, 1, &[]);
        let one_free = lobby(5, 3, &[]);
        let set = [private, closed, full, three_free, one_free];
        assert_eq!(ids(&LobbyQuery::default().evaluate(&set)), [5, 4]);
        let q = LobbyQuery {
            open_slots: Some(2),
            ..LobbyQuery::default()
        };
        assert_eq!(ids(&q.evaluate(&set)), [4]);
    }

    /// Spec 89 § 3.2: only `Public` lobbies are listed; friends-only, private and invisible ones
    /// never are, whatever the filters.
    #[test]
    fn only_public_lobbies_are_listed() {
        let set: Vec<LobbyInfo> = [
            LobbyVisibility::Public,
            LobbyVisibility::FriendsOnly,
            LobbyVisibility::Private,
            LobbyVisibility::Invisible,
        ]
        .into_iter()
        .zip(1..)
        .map(|(visibility, id)| LobbyInfo {
            visibility,
            ..lobby(id, 1, &[("mode", "duel")])
        })
        .collect();
        assert_eq!(ids(&LobbyQuery::default().evaluate(&set)), [1]);
        let q = LobbyQuery::default().with_string("mode", Compare::Eq, "duel");
        assert_eq!(ids(&q.evaluate(&set)), [1]);
    }

    #[test]
    fn near_sort_then_members_then_id() {
        let set = [
            lobby(1, 1, &[("skill", "50"), ("ping", "9")]),
            lobby(2, 2, &[("skill", "55"), ("ping", "1")]),
            lobby(3, 1, &[("skill", "45"), ("ping", "1")]),
            lobby(4, 3, &[("skill", "45"), ("ping", "1")]),
            lobby(5, 3, &[]),
        ];
        let q = LobbyQuery::default().with_near("skill", 50).with_near("ping", 0);
        // skill distance 0 first; then distance 5 (ids 2, 3, 4) by ping (all 1) then members desc
        // (4 has 3, 2 has 2, 3 has 1); missing keys last.
        assert_eq!(ids(&q.evaluate(&set)), [1, 4, 2, 3, 5]);
        let capped = LobbyQuery { max_results: 2, ..q };
        assert_eq!(ids(&capped.evaluate(&set)), [1, 4]);
        // Same input, same order: no hidden state.
        assert_eq!(capped.evaluate(&set), capped.evaluate(&set));
    }

    #[test]
    fn validation_bounds() {
        assert!(LobbyQuery::default().validate().is_ok());
        let zero = LobbyQuery {
            max_results: 0,
            ..LobbyQuery::default()
        };
        assert_eq!(zero.validate().map_err(|e| e.code()), Err("query_invalid"));
        let many = LobbyQuery {
            max_results: 51,
            ..LobbyQuery::default()
        };
        assert!(many.validate().is_err());
        assert!(
            LobbyQuery::default()
                .with_string("", Compare::Eq, "x")
                .validate()
                .is_err()
        );
        for c in [
            Compare::Eq,
            Compare::Ne,
            Compare::Lt,
            Compare::Le,
            Compare::Gt,
            Compare::Ge,
        ] {
            assert_eq!(Compare::from_name(c.name()), Some(c));
        }
        for d in [
            Distance::Close,
            Distance::Default,
            Distance::Far,
            Distance::Worldwide,
        ] {
            assert_eq!(Distance::from_name(d.name()), Some(d));
        }
    }
}
