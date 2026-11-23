// SPDX-License-Identifier: MPL-2.0
//! Operator telemetry rows (spec 120 § 13.2, ADR-8182).
//!
//! A [`StatsReporter`] writes two kinds of rows to the tracing target `regia_net::stats`:
//!
//! * a peer row every [`PEER_ROW_MS`] per peer: RTT p50/p95, one-way jitter, loss in both
//!   directions, the input margin, late and missing inputs, the send rate and the grade;
//! * a server row every [`SERVER_ROW_MS`]: tick step and loop time p50/p95/p99 over the period,
//!   resident memory, peers by grade, and the summed send and receive rates.
//!
//! Dedicated servers turn it on ([`StatsConfig::dedicated_server`]); clients leave it off (the
//! default). Peer addresses are written as `redacted` unless the operator opts in
//! ([`StatsConfig::show_addresses`]). The rows are also returned, so a host or a test can read
//! them without a tracing subscriber.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::quality::{LinkGrade, PeerQuality};

/// How often a peer row is written, milliseconds.
pub const PEER_ROW_MS: u64 = 10_000;
/// How often the server row is written, milliseconds.
pub const SERVER_ROW_MS: u64 = 60_000;
/// Tick and loop timings one period keeps at most (a 128 Hz server over a minute fits).
const MAX_TIMINGS: usize = 8_192;

/// Whether and how a [`StatsReporter`] writes rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StatsConfig {
    /// Write rows. Off by default (clients); on for dedicated servers.
    pub enabled: bool,
    /// Write peer addresses instead of `redacted` (an operator opt-in).
    pub show_addresses: bool,
}

impl StatsConfig {
    /// On, addresses redacted: what a dedicated server uses.
    pub fn dedicated_server() -> Self {
        Self {
            enabled: true,
            show_addresses: false,
        }
    }
}

/// One peer's row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerStatsRow {
    /// Peer id.
    pub peer: u32,
    /// The address, or `redacted`.
    pub addr: String,
    /// The connection quality.
    pub quality: PeerQuality,
    /// Receive rate, bytes per second.
    pub recv_bps: u32,
}

impl PeerStatsRow {
    /// The row as one `key=value` line.
    pub fn line(&self) -> String {
        let q = &self.quality;
        format!(
            "peer={} addr={} grade={} rtt_p50_ms={:.1} rtt_p95_ms={:.1} jitter_ms={:.1} rttvar_ms={:.1} \
             loss_out={:.4} loss_in={:.4} loss_recv={:.4} margin_p5={} inputs_late={:.4} inputs_missing={:.4} \
             sent_bps={} recv_bps={}",
            self.peer,
            self.addr,
            q.grade,
            q.rtt_p50_ms,
            q.rtt_p95_ms,
            q.jitter_ms,
            q.rttvar_ms,
            q.loss_out,
            q.loss_in,
            q.loss_recv,
            q.input_margin_p5,
            q.inputs_late,
            q.inputs_missing,
            q.sent_bps,
            self.recv_bps
        )
    }
}

/// p50 / p95 / p99 of one timing, milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Percentiles {
    /// Median.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
}

impl Percentiles {
    fn of(samples: &mut [f64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_by(f64::total_cmp);
        let at = |p: usize| samples[(samples.len() - 1) * p / 100];
        Self {
            p50: at(50),
            p95: at(95),
            p99: at(99),
        }
    }
}

/// The server row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerStatsRow {
    /// Simulation step time per tick over the period.
    pub tick_step_ms: Percentiles,
    /// Whole loop iteration time (receive, steps, send) over the period.
    pub loop_ms: Percentiles,
    /// Ticks recorded in the period.
    pub ticks: u64,
    /// Resident memory, bytes; `None` where the platform does not say (only Linux and Android
    /// read it, from `/proc/self/status`).
    pub rss_bytes: Option<u64>,
    /// Peers per grade: good, fair, poor, interrupted.
    pub peers_by_grade: [u32; 4],
    /// Summed send rate to every peer, bytes per second.
    pub sent_bps: u64,
    /// Summed receive rate from every peer, bytes per second.
    pub recv_bps: u64,
}

impl ServerStatsRow {
    /// The row as one `key=value` line.
    pub fn line(&self) -> String {
        let [good, fair, poor, interrupted] = self.peers_by_grade;
        format!(
            "server ticks={} tick_step_ms_p50={:.3} tick_step_ms_p95={:.3} tick_step_ms_p99={:.3} \
             loop_ms_p50={:.3} loop_ms_p95={:.3} loop_ms_p99={:.3} rss_bytes={} peers_good={good} \
             peers_fair={fair} peers_poor={poor} peers_interrupted={interrupted} sent_bps={} recv_bps={}",
            self.ticks,
            self.tick_step_ms.p50,
            self.tick_step_ms.p95,
            self.tick_step_ms.p99,
            self.loop_ms.p50,
            self.loop_ms.p95,
            self.loop_ms.p99,
            self.rss_bytes
                .map_or_else(|| "unknown".to_owned(), |b| b.to_string()),
            self.sent_bps,
            self.recv_bps
        )
    }
}

/// One peer as the reporter sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerSample {
    /// Peer id.
    pub peer: u32,
    /// Its address as text.
    pub addr: String,
    /// Its quality.
    pub quality: PeerQuality,
    /// Its receive rate, bytes per second.
    pub recv_bps: u32,
}

/// Rows due on one [`StatsReporter::poll`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatsRows {
    /// Peer rows (every peer, when the peer period passed).
    pub peers: Vec<PeerStatsRow>,
    /// The server row, when the server period passed.
    pub server: Option<ServerStatsRow>,
}

/// Writes the operator rows. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct StatsReporter {
    config: StatsConfig,
    next_peer_ms: Option<u64>,
    next_server_ms: Option<u64>,
    tick_ms: VecDeque<f64>,
    loop_ms: VecDeque<f64>,
    ticks: u64,
}

fn push_capped(v: &mut VecDeque<f64>, ms: f64) {
    if v.len() >= MAX_TIMINGS {
        v.pop_front();
    }
    v.push_back(ms);
}

impl StatsReporter {
    /// A reporter with `config`.
    pub fn new(config: StatsConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// The configuration.
    pub fn config(&self) -> StatsConfig {
        self.config
    }

    /// Records one simulation step's duration, milliseconds.
    pub fn record_tick(&mut self, step_ms: f64) {
        if self.config.enabled {
            push_capped(&mut self.tick_ms, step_ms);
            self.ticks += 1;
        }
    }

    /// Records one loop iteration's duration, milliseconds.
    pub fn record_loop(&mut self, loop_ms: f64) {
        if self.config.enabled {
            push_capped(&mut self.loop_ms, loop_ms);
        }
    }

    /// Whether [`StatsReporter::poll`] would write or start anything at `now_ms`, so a host
    /// gathers its peer samples only then.
    pub fn due(&self, now_ms: u64) -> bool {
        self.config.enabled
            && (self.next_peer_ms.is_none_or(|t| now_ms >= t)
                || self.next_server_ms.is_none_or(|t| now_ms >= t))
    }

    /// Writes the rows that are due at `now_ms` (the first poll starts both periods) and returns
    /// them; nothing when disabled.
    pub fn poll(&mut self, now_ms: u64, peers: &[PeerSample]) -> StatsRows {
        let mut rows = StatsRows::default();
        if !self.config.enabled {
            return rows;
        }
        let next_peer = *self.next_peer_ms.get_or_insert(now_ms + PEER_ROW_MS);
        let next_server = *self.next_server_ms.get_or_insert(now_ms + SERVER_ROW_MS);
        if now_ms >= next_peer {
            self.next_peer_ms = Some(now_ms + PEER_ROW_MS);
            for p in peers {
                let row = PeerStatsRow {
                    peer: p.peer,
                    addr: if self.config.show_addresses {
                        p.addr.clone()
                    } else {
                        "redacted".to_owned()
                    },
                    quality: p.quality,
                    recv_bps: p.recv_bps,
                };
                tracing::info!(target: "regia_net::stats", "{}", row.line());
                rows.peers.push(row);
            }
        }
        if now_ms >= next_server {
            self.next_server_ms = Some(now_ms + SERVER_ROW_MS);
            let mut by_grade = [0u32; 4];
            for p in peers {
                let i = match p.quality.grade {
                    LinkGrade::Good => 0,
                    LinkGrade::Fair => 1,
                    LinkGrade::Poor => 2,
                    LinkGrade::Interrupted => 3,
                };
                by_grade[i] += 1;
            }
            let mut tick: Vec<f64> = self.tick_ms.drain(..).collect();
            let mut looped: Vec<f64> = self.loop_ms.drain(..).collect();
            let row = ServerStatsRow {
                tick_step_ms: Percentiles::of(&mut tick),
                loop_ms: Percentiles::of(&mut looped),
                ticks: std::mem::take(&mut self.ticks),
                rss_bytes: resident_bytes(),
                peers_by_grade: by_grade,
                sent_bps: peers.iter().map(|p| u64::from(p.quality.sent_bps)).sum(),
                recv_bps: peers.iter().map(|p| u64::from(p.recv_bps)).sum(),
            };
            tracing::info!(target: "regia_net::stats", "{}", row.line());
            rows.server = Some(row);
        }
        rows
    }
}

/// This process's resident memory, bytes (Linux and Android: `VmRSS` of `/proc/self/status`).
pub fn resident_bytes() -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        parse_vm_rss(&status)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        None
    }
}

/// `VmRSS:  1234 kB` in a `/proc/<pid>/status` text, in bytes.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(peer: u32, grade: LinkGrade) -> PeerSample {
        PeerSample {
            peer,
            addr: format!("203.0.113.{peer}:7777"),
            quality: PeerQuality {
                grade,
                sent_bps: 1_000,
                ..PeerQuality::default()
            },
            recv_bps: 500,
        }
    }

    #[test]
    fn rows_come_every_ten_and_sixty_seconds_with_addresses_redacted() {
        let mut r = StatsReporter::new(StatsConfig::dedicated_server());
        let peers = [
            sample(1, LinkGrade::Good),
            sample(2, LinkGrade::Poor),
            sample(3, LinkGrade::Interrupted),
        ];
        let mut peer_rows = 0;
        let mut server_rows = Vec::new();
        for s in 0..=120u64 {
            for _ in 0..30 {
                r.record_tick(1.0 + (s % 4) as f64);
                r.record_loop(2.0);
            }
            let rows = r.poll(s * 1_000, &peers);
            peer_rows += rows.peers.len();
            for row in &rows.peers {
                assert_eq!(row.addr, "redacted");
                assert!(!row.line().contains("203.0.113"), "{}", row.line());
            }
            server_rows.extend(rows.server);
        }
        // 120 s: twelve peer periods of three peers, two server periods.
        assert_eq!(peer_rows, 12 * 3);
        assert_eq!(server_rows.len(), 2);
        let row = &server_rows[0];
        assert_eq!(row.peers_by_grade, [1, 0, 1, 1]);
        // The first poll (t = 0) opens the period after that second's ticks were recorded.
        assert_eq!(row.ticks, 61 * 30);
        assert_eq!(row.loop_ms.p99, 2.0);
        assert!(row.tick_step_ms.p50 >= 1.0 && row.tick_step_ms.p99 == 4.0);
        assert_eq!((row.sent_bps, row.recv_bps), (3_000, 1_500));
        assert!(row.line().starts_with("server ticks=1830 "), "{}", row.line());
    }

    #[test]
    fn clients_write_nothing_and_operators_may_opt_into_addresses() {
        let mut off = StatsReporter::default();
        off.record_tick(1.0);
        assert_eq!(off.poll(0, &[sample(1, LinkGrade::Good)]), StatsRows::default());
        assert_eq!(
            off.poll(600_000, &[sample(1, LinkGrade::Good)]),
            StatsRows::default()
        );
        let mut on = StatsReporter::new(StatsConfig {
            enabled: true,
            show_addresses: true,
        });
        assert!(on.due(0));
        assert!(
            on.poll(0, &[]).peers.is_empty(),
            "the first poll starts the periods"
        );
        assert!(!on.due(PEER_ROW_MS - 1));
        assert!(on.due(PEER_ROW_MS));
        assert!(!off.due(0), "a disabled reporter is never due");
        let rows = on.poll(PEER_ROW_MS, &[sample(4, LinkGrade::Fair)]);
        assert_eq!(rows.peers[0].addr, "203.0.113.4:7777");
        assert!(rows.peers[0].line().contains("grade=fair"));
    }

    #[test]
    fn vm_rss_parses_kilobytes() {
        assert_eq!(
            parse_vm_rss("Name:\tx\nVmRSS:\t  2048 kB\nVmSwap: 0 kB\n"),
            Some(2048 * 1024)
        );
        assert_eq!(parse_vm_rss("Name:\tx\n"), None);
        #[cfg(target_os = "linux")]
        assert!(resident_bytes().is_some_and(|b| b > 0));
    }
}
