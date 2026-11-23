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
