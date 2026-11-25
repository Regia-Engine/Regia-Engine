// SPDX-License-Identifier: MPL-2.0
//! Per-address and per-identity request limits for the lobby hub process (NL-06, ADR-8648).
//!
//! The gate runs before the hub core, so it also runs before member-data limits. Each bucket is a
//! one-second window on the caller's clock. Idle buckets are dropped, and the maps are capped, so
//! a scan of addresses cannot grow the process without bound.

use std::collections::{BTreeMap, VecDeque};
use std::net::IpAddr;

use crate::crypto::PlayerKey;

use super::HubError;
use super::config::HubLimits;

/// How long a bucket keeps a timestamp, and how long an idle bucket stays in the map.
const WINDOW_MS: u64 = 1_000;
const IDLE_MS: u64 = 2_000;
/// Most addresses or identities remembered at once. Past this, the lowest key is dropped.
const TRACK_CAP: usize = 4_096;

/// The request gate. One per hub process.
#[derive(Debug, Clone)]
pub struct AdmitGate {
    limits: HubLimits,
    addresses: BTreeMap<IpAddr, VecDeque<u64>>,
    identities: BTreeMap<PlayerKey, VecDeque<u64>>,
}

impl Default for AdmitGate {
    fn default() -> Self {
        Self::new(HubLimits::default())
    }
}

impl AdmitGate {
    /// A gate with `limits`.
    #[must_use]
    pub fn new(limits: HubLimits) -> Self {
        Self {
            limits,
            addresses: BTreeMap::new(),
            identities: BTreeMap::new(),
        }
    }

    /// The configured limits.
    #[must_use]
    pub fn limits(&self) -> HubLimits {
        self.limits
    }

    /// Addresses that still hold a timestamp.
    #[must_use]
    pub fn tracked_addresses(&self) -> usize {
        self.addresses.len()
    }

    /// Admits one request from `addr` under `identity` at `now_ms`.
    ///
    /// A missing address skips the address bucket (a peer whose socket has no address yet). A
    /// missing identity skips the identity bucket (a hello that has not proven a key). Either
    /// limit refuses the request; the address limit is checked first.
    ///
    /// # Errors
    ///
    /// [`HubError::AddressRateLimited`] or [`HubError::IdentityRateLimited`].
    pub fn admit(
        &mut self,
        addr: Option<IpAddr>,
        identity: Option<&PlayerKey>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.evict(now_ms);
        if let Some(addr) = addr {
            let window = self.addresses.entry(addr).or_default();
            if !allow(window, now_ms, self.limits.per_address_per_sec) {
                return Err(HubError::AddressRateLimited);
            }
        }
        if let Some(identity) = identity {
            let window = self.identities.entry(*identity).or_default();
            if !allow(window, now_ms, self.limits.per_identity_per_sec) {
                return Err(HubError::IdentityRateLimited);
            }
        }
        self.evict(now_ms);
        Ok(())
    }

    fn evict(&mut self, now_ms: u64) {
        let keep = now_ms.saturating_sub(IDLE_MS);
        self.addresses
            .retain(|_, window| window.back().is_some_and(|t| *t >= keep));
        self.identities
            .retain(|_, window| window.back().is_some_and(|t| *t >= keep));
        while self.addresses.len() > TRACK_CAP {
            let Some(key) = self.addresses.keys().next().copied() else {
                break;
            };
            self.addresses.remove(&key);
        }
        while self.identities.len() > TRACK_CAP {
            let Some(key) = self.identities.keys().next().copied() else {
                break;
            };
            self.identities.remove(&key);
        }
    }
}

/// Records `now_ms` when the window holds fewer than `limit` stamps inside the last second.
fn allow(window: &mut VecDeque<u64>, now_ms: u64, limit: u32) -> bool {
    while window
        .front()
        .is_some_and(|t| now_ms.saturating_sub(*t) >= WINDOW_MS)
    {
        window.pop_front();
    }
    if window.len() >= usize::try_from(limit).unwrap_or(usize::MAX) {
        return false;
    }
    window.push_back(now_ms);
    true
}

/// How long one AskReach stamp counts (spec 89 § 16.5).
pub(crate) const ASK_WINDOW_MS: u64 = 10_000;
/// Asks one player key may make inside [`ASK_WINDOW_MS`]. `HubSettings::sanitized` does not raise this.
pub(crate) const ASK_PER_IDENTITY_CAP: u32 = 4;
/// Asks one source address may make inside [`ASK_WINDOW_MS`].
pub(crate) const ASK_PER_ADDRESS_CAP: u32 = 16;
/// Open asks (`Asked` or `Planned`) one player key may hold.
pub(crate) const ASK_INFLIGHT_PER_IDENTITY_CAP: u32 = 2;
/// Open asks the hub may hold.
pub(crate) const ASK_INFLIGHT_HUB_CAP: u32 = 64;

const ASK_IDLE_MS: u64 = 20_000;
const ASK_TRACK_CAP: usize = 4_096;

/// AskReach windows on the hub core (NL-09). Separate from [`AdmitGate`]'s one-second buckets.
///
/// The address window is checked first. A refusal there does not stamp the key. A missing address
/// skips the address window. A missing player key skips the key window. One stamp is recorded per
/// window only when the ask is admitted.
#[derive(Debug, Clone, Default)]
pub(crate) struct AskWindows {
    addresses: BTreeMap<IpAddr, VecDeque<u64>>,
    keys: BTreeMap<PlayerKey, VecDeque<u64>>,
}

impl AskWindows {
    /// Admits one ask at `now_ms`.
    ///
    /// # Errors
    ///
    /// [`HubError::AskRateLimited`] when the address window or the key window is full.
    pub(crate) fn admit(
        &mut self,
        addr: Option<IpAddr>,
        key: Option<PlayerKey>,
        now_ms: u64,
        per_address: u32,
        per_key: u32,
    ) -> Result<(), HubError> {
        self.evict(now_ms);
        if let Some(addr) = addr
            && !fits(self.addresses.get(&addr), now_ms, per_address)
        {
            return Err(HubError::AskRateLimited);
        }
        if let Some(key) = key
            && !fits(self.keys.get(&key), now_ms, per_key)
        {
            return Err(HubError::AskRateLimited);
        }
        if let Some(addr) = addr {
            stamp(self.addresses.entry(addr).or_default(), now_ms);
        }
        if let Some(key) = key {
            stamp(self.keys.entry(key).or_default(), now_ms);
        }
        self.evict(now_ms);
        Ok(())
    }

    /// A rough byte count for [`HubServer::state_bytes`](super::HubServer::state_bytes).
    pub(crate) fn bytes_estimate(&self) -> usize {
        (self.addresses.len() + self.keys.len()) * 48
    }

    fn evict(&mut self, now_ms: u64) {
        let keep = now_ms.saturating_sub(ASK_IDLE_MS);
        self.addresses
            .retain(|_, window| window.back().is_some_and(|t| *t >= keep));
        self.keys
            .retain(|_, window| window.back().is_some_and(|t| *t >= keep));
        while self.addresses.len() > ASK_TRACK_CAP {
            let Some(key) = self.addresses.keys().next().copied() else {
                break;
            };
            self.addresses.remove(&key);
        }
        while self.keys.len() > ASK_TRACK_CAP {
            let Some(key) = self.keys.keys().next().copied() else {
                break;
            };
            self.keys.remove(&key);
        }
    }
}

/// True when `window` holds fewer than `limit` stamps inside the AskReach window.
fn fits(window: Option<&VecDeque<u64>>, now_ms: u64, limit: u32) -> bool {
    let Some(window) = window else {
        return true;
    };
    let live = window
        .iter()
        .filter(|t| now_ms.saturating_sub(**t) < ASK_WINDOW_MS)
        .count();
    live < usize::try_from(limit).unwrap_or(usize::MAX)
}

fn stamp(window: &mut VecDeque<u64>, now_ms: u64) {
    while window
        .front()
        .is_some_and(|t| now_ms.saturating_sub(*t) >= ASK_WINDOW_MS)
    {
        window.pop_front();
    }
    window.push_back(now_ms);
}
