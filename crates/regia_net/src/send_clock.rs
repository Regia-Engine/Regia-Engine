// SPDX-License-Identifier: MPL-2.0
//! Send cadence helpers: which simulation ticks send state, and which clients get a full state.
//!
//! The simulation ticks at `tick_rate_hz`; state goes out every `tick_rate_hz / send_hz` ticks.
//! When a game sends occasional full states between deltas, spreading them over clients keeps one
//! send tick from carrying a full state to everyone at once.

/// Simulation ticks between sends: `tick_rate_hz / send_hz`, at least 1.
pub fn send_interval_ticks(tick_rate_hz: u32, send_hz: u32) -> u64 {
    u64::from((tick_rate_hz.max(1) / send_hz.max(1)).max(1))
}

/// True when `sim_tick` is a send tick for `send_interval`.
pub fn is_send_tick(sim_tick: u64, send_interval: u64) -> bool {
    send_interval > 0 && sim_tick.is_multiple_of(send_interval)
}

/// Stagger period `send_hz / full_hz`, at least 1.
pub fn stagger_period(send_hz: u32, full_hz: u32) -> u64 {
    u64::from((send_hz.max(1) / full_hz.max(1)).max(1))
}

/// Whether the client with `client_index` gets a full state on send tick number `send_tick`.
pub fn client_full_due(client_index: u64, send_tick: u64, period: u64) -> bool {
    let period = period.max(1);
    client_index % period == send_tick % period
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_and_gates() {
        assert_eq!(send_interval_ticks(60, 20), 3);
        assert_eq!(send_interval_ticks(60, 30), 2);
        assert_eq!(send_interval_ticks(10, 30), 1, "never below one tick");
        assert!(is_send_tick(0, 3));
        assert!(!is_send_tick(1, 3));
        assert!(is_send_tick(6, 3));
        assert!(!is_send_tick(6, 0));
    }

    #[test]
    fn stagger_spreads_full_states() {
        let period = stagger_period(20, 2);
        assert_eq!(period, 10);
        for send_tick in 0..20 {
            let fulls = (0..16u64)
                .filter(|&c| client_full_due(c, send_tick, period))
                .count();
            assert!((1..16).contains(&fulls), "tick {send_tick}: {fulls} fulls");
        }
    }
}
