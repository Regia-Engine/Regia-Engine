// SPDX-License-Identifier: MPL-2.0
//! Server-side routing, a pure function over a membership table
//! (`docs/specs/59-voice-chat.md` § 2.10). No network involved.

use super::settings::{VoiceChannel, VoiceChannelKind};
use std::collections::{BTreeMap, BTreeSet};

/// Extra distance accepted on top of a proximity channel's radius (metres).
pub const PROXIMITY_SLACK_M: f32 = 5.0;
/// How long a proximity allow or deny is reused (spec 59 § 2.10 rule 3).
pub const PROXIMITY_CACHE_MS: u64 = 100;
/// Senders forwarded to one listener beyond `max_mixed_speakers`.
pub const LISTENER_CAP_EXTRA: usize = 2;

/// What the server knows about one connected player.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Member {
    /// Channels the player joined.
    pub channels: BTreeSet<u16>,
    /// Replicated world position.
    pub position: [f32; 3],
    /// Players the member blocks (mutual with the reverse list).
    pub blocked: BTreeSet<u16>,
    /// Senders the member asked not to receive (`mute_hint`).
    pub muted: BTreeSet<u16>,
    /// Game-defined relevancy hook result: `true` excludes the member from proximity channels
    /// (other level, dead).
    pub excluded: bool,
}

/// The server's membership table.
#[derive(Clone, Debug, Default)]
pub struct MembershipTable {
    /// Channel definitions by id.
    pub channels: BTreeMap<u16, VoiceChannel>,
    /// Members by session slot.
    pub members: BTreeMap<u16, Member>,
    /// `(sender, listener, channel)` to `(decided_at_ms, in range)`.
    proximity: BTreeMap<(u16, u16, u16), (u64, bool)>,
    /// `now_ms` of the last expiry sweep. Reads already ignore a stale entry, so the map is
    /// swept at most once per [`PROXIMITY_CACHE_MS`] instead of on every packet.
    proximity_swept_at: u64,
}

impl MembershipTable {
    /// Whether `a` or `b` blocks the other.
    #[must_use]
    pub fn blocked_either_way(&self, a: u16, b: u16) -> bool {
        let one = self.members.get(&a).is_some_and(|m| m.blocked.contains(&b));
        let two = self.members.get(&b).is_some_and(|m| m.blocked.contains(&a));
        one || two
    }

    /// The slots that receive a packet from `sender` on `channel`, ascending. Empty when the
    /// sender is unknown, not in the channel or the channel is undefined.
    #[must_use]
    pub fn recipients(&self, sender: u16, channel: u16) -> Vec<u16> {
        let _span = tracing::trace_span!("voice_chat_route").entered();
        let (Some(def), Some(from)) = (self.channels.get(&channel), self.members.get(&sender)) else {
            return Vec::new();
        };
        if !from.channels.contains(&channel) {
            return Vec::new();
        }
        let spatial =
            def.kind == VoiceChannelKind::Proximity || (def.kind == VoiceChannelKind::Custom && def.spatial);
        let reach = def.radius + PROXIMITY_SLACK_M;
        let mut out = Vec::new();
        for (&slot, to) in &self.members {
            if slot == sender || !to.channels.contains(&channel) {
                continue;
            }
            if self.blocked_either_way(sender, slot) || to.muted.contains(&sender) {
                continue;
            }
            if spatial && (to.excluded || from.excluded || out_of_reach(from.position, to.position, reach)) {
                continue;
            }
            out.push(slot);
        }
        out
    }

    /// [`Self::recipients`] with the proximity distance cached for [`PROXIMITY_CACHE_MS`].
    /// Blocks, mutes and exclusion are read on this call. Only the distance is cached.
    ///
    /// A warm call allocates only the result list, the same as [`Self::recipients`]. Membership
    /// rows are borrowed, not cloned: this runs once per voice packet.
    #[must_use]
    pub fn recipients_at(&mut self, sender: u16, channel: u16, now_ms: u64) -> Vec<u16> {
        let _span = tracing::trace_span!("voice_chat_route").entered();
        let (spatial, reach) = {
            let Some(def) = self.channels.get(&channel) else {
                return Vec::new();
            };
            let spatial = def.kind == VoiceChannelKind::Proximity
                || (def.kind == VoiceChannelKind::Custom && def.spatial);
            (spatial, def.radius + PROXIMITY_SLACK_M)
        };
        let (from_excluded, from_position, in_channel) = {
            let Some(from) = self.members.get(&sender) else {
                return Vec::new();
            };
            (from.excluded, from.position, from.channels.contains(&channel))
        };
        if !in_channel {
            return Vec::new();
        }
        let MembershipTable {
            members,
            proximity,
            proximity_swept_at,
            ..
        } = self;
        let mut out = Vec::new();
        for (&slot, to) in members.iter() {
            if slot == sender || !to.channels.contains(&channel) {
                continue;
            }
            // The sender's own block list is applied after this loop: `members` cannot be
            // looked up again while `iter` borrows it, and cloning the set would allocate
            // on every packet.
            if to.blocked.contains(&sender) || to.muted.contains(&sender) {
                continue;
            }
            if spatial {
                if to.excluded || from_excluded {
                    continue;
                }
                let key = (sender, slot, channel);
                let in_range = match proximity.get(&key).copied() {
                    Some((at, ok)) if now_ms.saturating_sub(at) < PROXIMITY_CACHE_MS => ok,
                    _ => {
                        let ok = !out_of_reach(from_position, to.position, reach);
                        proximity.insert(key, (now_ms, ok));
                        ok
                    }
                };
                if !in_range {
                    continue;
                }
            }
            out.push(slot);
        }
        if let Some(from) = members.get(&sender) {
            out.retain(|slot| !from.blocked.contains(slot));
        }
        if now_ms.saturating_sub(*proximity_swept_at) >= PROXIMITY_CACHE_MS {
            proximity.retain(|_, (at, _)| now_ms.saturating_sub(*at) < PROXIMITY_CACHE_MS);
            *proximity_swept_at = now_ms;
        }
        out
    }
}

/// True when the pair is beyond `reach`, or when the distance is not finite (a NaN position must
/// not sit inside every radius).
fn out_of_reach(a: [f32; 3], b: [f32; 3], reach: f32) -> bool {
    let d = distance(a, b);
    !d.is_finite() || d > reach
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// A sender competing for a listener's mixed slots.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    /// Sender slot.
    pub sender: u16,
    /// Channel priority class (higher wins; spec default team 3 > proximity 2 > party 1 > global 0).
    pub priority: u8,
    /// Recent energy in dBFS.
    pub energy_db: f32,
}

/// Ranks candidates for the listener cap: priority class, then energy, then lower sender id
/// (spec § 2.4). Returns the first `cap` senders, ascending by sender id so the mix order is
/// deterministic (spec § 7).
#[must_use]
pub fn rank_speakers(candidates: &[Candidate], cap: usize) -> Vec<u16> {
    rank_speakers_sticky(candidates, cap, &[], 0.0)
}

/// Energy bonus (dB) a speaker already in the audible set gets when the set is re-ranked, so a
/// near-equal newcomer does not displace it (spec § 2.4 hysteresis).
pub const RANK_STAY_IN_MARGIN_DB: f32 = 3.0;

/// [`rank_speakers`] with hysteresis: senders in `current` count `margin_db` louder within their
/// priority class, so the audible set only changes when a challenger is clearly louder.
#[must_use]
pub fn rank_speakers_sticky(
    candidates: &[Candidate],
    cap: usize,
    current: &[u16],
    margin_db: f32,
) -> Vec<u16> {
    let mut sorted = Vec::new();
    let mut kept = Vec::new();
    rank_speakers_sticky_into(candidates, cap, current, margin_db, &mut sorted, &mut kept);
    kept
}

/// [`rank_speakers_sticky`] into reused buffers. `sorted` holds the ranked candidates; `kept`
/// receives the chosen sender ids, ascending.
pub fn rank_speakers_sticky_into(
    candidates: &[Candidate],
    cap: usize,
    current: &[u16],
    margin_db: f32,
    sorted: &mut Vec<Candidate>,
    kept: &mut Vec<u16>,
) {
    let effective = |c: &Candidate| {
        c.energy_db
            + if current.contains(&c.sender) {
                margin_db
            } else {
                0.0
            }
    };
    sorted.clear();
    sorted.extend_from_slice(candidates);
    sorted.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(effective(b).total_cmp(&effective(a)))
            .then(a.sender.cmp(&b.sender))
    });
    kept.clear();
    kept.extend(sorted.iter().take(cap).map(|c| c.sender));
    kept.sort_unstable();
}
