// SPDX-License-Identifier: MPL-2.0
//! The in-process presence soak (NL-06). One thousand sessions connect, register (create or
//! join), chat and drop, repeated on the caller's clock. Samples are taken after each cycle's
//! outboxes are drained, so a directory snapshot cannot inflate the bound.

use super::wire::ChatScope;
use super::{HubError, HubServer, HubSettings};
use crate::lobby::{LobbyId, LobbySpec, MemberId};

/// Sessions in one cycle.
pub const SOAK_PRESENCES: usize = 1_000;
/// Cycles. Ten cycles of 1 000 simulated seconds is 10 000 seconds.
pub const SOAK_CYCLES: u32 = 10;
/// Simulated milliseconds the soak advances (10 000 seconds).
pub const SOAK_SIMULATED_MS: u64 = 10_000_000;
/// `state_bytes()` must stay under this after each drained cycle, connected or idle.
pub const SOAK_STATE_BOUND: usize = 8 * 1024 * 1024;
/// The counting allocator's rise over the start of the soak must stay under this.
pub const SOAK_ALLOC_BOUND: usize = 32 * 1024 * 1024;
/// Lobbies opened per cycle (the default lobby cap).
const SOAK_LOBBIES: usize = 256;

/// What one soak run measured. Bytes are samples after the cycle's events were drained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoakReport {
    /// Cycles completed.
    pub cycles: u32,
    /// Simulated milliseconds advanced.
    pub simulated_ms: u64,
    /// `state_bytes()` after the first cycle's sessions were dropped.
    pub state_after_first_cycle: usize,
    /// `state_bytes()` after the last cycle's sessions were dropped.
    pub state_at_end: usize,
    /// Largest `state_bytes()` sampled (connected, outboxes drained, and after the drop).
    pub peak_state_bytes: usize,
    /// Allocator bytes at the start, as the callback reported them.
    pub alloc_at_start: usize,
    /// Allocator bytes after the first cycle's drop.
    pub alloc_after_first_cycle: usize,
    /// Allocator bytes after the last cycle's drop.
    pub alloc_at_end: usize,
    /// Largest allocator sample.
    pub peak_alloc_bytes: usize,
}

/// Runs the 1 000-presence soak on the test clock.
///
/// `live_alloc` reports the counting allocator's live bytes. The function calls it after each
/// cycle's temporary buffers are dropped. Ten cycles cover [`SOAK_SIMULATED_MS`] milliseconds:
/// each cycle is 1 000 seconds, and the sessions are gone before the next one starts.
///
/// # Errors
///
/// A connect, register, join or chat was refused. That is a broken scenario, not a full hub.
pub fn run_hub_soak_1000(live_alloc: &dyn Fn() -> usize) -> Result<SoakReport, HubError> {
    let alloc_at_start = live_alloc();
    let mut hub = HubServer::with_secret(HubSettings::default(), [7u8; 32]);
    let mut state_after_first_cycle = 0;
    let mut alloc_after_first_cycle = 0;
    let mut state_at_end = 0;
    let mut alloc_at_end = alloc_at_start;
    let mut peak_state_bytes = 0;
    let mut peak_alloc_bytes = alloc_at_start;
    for cycle in 0..SOAK_CYCLES {
        let now_ms = u64::from(cycle) * 1_000_000;
        let connected = churn(&mut hub, now_ms)?;
        let state = hub.state_bytes();
        let alloc = live_alloc();
        peak_state_bytes = peak_state_bytes.max(state);
        peak_alloc_bytes = peak_alloc_bytes.max(alloc);
        for id in connected {
            hub.disconnect(id);
        }
        let _expired = hub.expire(now_ms.saturating_add(1));
        let state = hub.state_bytes();
        let alloc = live_alloc();
        peak_state_bytes = peak_state_bytes.max(state);
        peak_alloc_bytes = peak_alloc_bytes.max(alloc);
        if cycle == 0 {
            state_after_first_cycle = state;
            alloc_after_first_cycle = alloc;
        }
        state_at_end = state;
        alloc_at_end = alloc;
    }
    Ok(SoakReport {
        cycles: SOAK_CYCLES,
        simulated_ms: SOAK_SIMULATED_MS,
        state_after_first_cycle,
        state_at_end,
        peak_state_bytes,
        alloc_at_start,
        alloc_after_first_cycle,
        alloc_at_end,
        peak_alloc_bytes,
    })
}

/// Connects [`SOAK_PRESENCES`], registers them (create while under the lobby cap, otherwise
/// join), chats once in that lobby, and drains every outbox. Returns the member ids still connected.
fn churn(hub: &mut HubServer, now_ms: u64) -> Result<Vec<MemberId>, HubError> {
    let mut ids = Vec::with_capacity(SOAK_PRESENCES);
    for i in 0..SOAK_PRESENCES {
        ids.push(hub.connect(&format!("p{i}"), now_ms)?);
    }
    let spec = LobbySpec {
        max_members: 8,
        ..LobbySpec::default()
    };
    let mut lobbies = Vec::with_capacity(SOAK_LOBBIES);
    for id in ids.iter().take(SOAK_LOBBIES) {
        lobbies.push(hub.create(*id, &spec, now_ms)?);
    }
    for (index, id) in ids.iter().enumerate().skip(SOAK_LOBBIES) {
        let lobby = lobbies[index % lobbies.len()];
        hub.join(*id, lobby, None, now_ms)?;
    }
    for (index, id) in ids.iter().enumerate() {
        let lobby: LobbyId = if index < SOAK_LOBBIES {
            lobbies[index]
        } else {
            lobbies[index % lobbies.len()]
        };
        hub.chat(*id, ChatScope::Lobby(lobby), "hi", now_ms)?;
    }
    hub.flush();
    for id in &ids {
        let _events = hub.take_events(*id);
    }
    Ok(ids)
}
