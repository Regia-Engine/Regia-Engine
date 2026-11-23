// SPDX-License-Identifier: MPL-2.0
//! The named network profiles (ADR-7899, ADR-8030). Every number here is documented in
//! `book/agents/net-soak.md`; change both together.
//!
//! A profile describes one *client* access link (both directions; each direction's numbers are
//! one way), the outage and rebinding schedule laid over a run of a given length, and whether a
//! hostile peer joins. The server's link is clean in dedicated-server mode and a home uplink in
//! listen-server mode ([`listen_server_link`]); a profile may set the server's uplink itself
//! (`listen-uplink`).
//!
//! The first eight are the harness's (ADR-7899); the next twenty are spec 120 § 15.1's (ADR-8030):
//! the audit's nine (`high-rtt` .. `downstream-loss`) and eleven more. A *mixed* profile gives each
//! client the link of another profile by a smooth weighted round robin over the client index, so
//! every prefix of the clients is as close to the shares as whole clients allow. One profile needs
//! an engine part that does not exist yet and is declared but skipped with a reason
//! ([`Profile::needs`]): `relay-path` (relay v2). `lockstep-mix` is a lockstep match, not a
//! replication run ([`Profile::is_lockstep`]): the plan runs it through `lockstep::run_lockstep`
//! (ADR-8209), with the order-driven input mix, as `lockstep-mix+action` the action mix, or as
//! `lockstep-mix+bots` the wander-fire bot mix (ADR-8409).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde::{Deserialize, Serialize};

use super::impair::{Direction, GilbertElliott, LinkPhase, LinkSpec};

/// Every profile name, in report order: the harness's eight, then spec 120 § 15.1's twenty.
pub const PROFILE_NAMES: [&str; 28] = [
    "clean",
    "good-wifi",
    "bad-wifi",
    "mobile-4g",
    "transatlantic",
    "congested",
    "flappy",
    "hostile",
    "high-rtt",
    "rtt-step",
    "clock-drift",
    "nat-rebind",
    "cgnat",
    "wifi-to-cell",
    "long-outage",
    "bufferbloat",
    "downstream-loss",
    "asym-bufferbloat",
    "listen-uplink",
    "lossy-fat-pipe",
    "rtt-extreme",
    "mtu-1280",
    "mtu-blackhole",
    "flood",
    "forged-handshake",
    "relay-path",
    "lockstep-mix",
    "mixed",
];

/// The profiles spec 120 § 15.1 added (ADR-8030), in report order.
pub const NEW_PROFILE_NAMES: [&str; 20] = [
    "high-rtt",
    "rtt-step",
    "clock-drift",
    "nat-rebind",
    "cgnat",
    "wifi-to-cell",
    "long-outage",
    "bufferbloat",
    "downstream-loss",
    "asym-bufferbloat",
    "listen-uplink",
    "lossy-fat-pipe",
    "rtt-extreme",
    "mtu-1280",
    "mtu-blackhole",
    "flood",
    "forged-handshake",
    "relay-path",
    "lockstep-mix",
    "mixed",
];

/// Suffix that adds a bulk transfer to any profile (`congested+bulk`, the `rpc_p95_with_bulk`
/// bar's run).
pub const BULK_SUFFIX: &str = "+bulk";
/// Bytes of the bulk transfer each client receives in a `+bulk` run (spec 120 lane N203's 256 KiB).
pub const BULK_BYTES: usize = 256 * 1024;
/// The bulk transfer starts at this fraction of the run.
pub const BULK_AT: f64 = 0.5;
/// The lockstep profile: its runs are lockstep matches ([`Profile::is_lockstep`]).
pub const LOCKSTEP_MIX: &str = "lockstep-mix";
/// Suffix that switches the lockstep profile from the order-driven input mix to the action mix,
/// where half the seats change their aim on any tick (`lockstep-mix+action`; ADR-8209).
pub const ACTION_SUFFIX: &str = "+action";
/// Suffix that switches the lockstep profile to the wander-fire bot mix (`lockstep-mix+bots`;
/// ADR-8409). Only [`LOCKSTEP_MIX`] takes it.
pub const BOTS_SUFFIX: &str = "+bots";
/// Suffix that adds one hostile peer (the `hostile` profile's) to any profile: `mixed+hostile` is
/// the scale run's "one hostile peer" (spec 120 § 1.1 "It scales", N215, ADR-8454). The hostile
/// peer rides the good-wifi link of client 0's network position and is judged by the harness's
/// `hostile_dropped`, `others_unaffected` and `no_hijack` bars.
pub const HOSTILE_SUFFIX: &str = "+hostile";
/// Every variant suffix, in the order [`Profile::named`] strips them from the end of a name.
pub const VARIANT_SUFFIXES: [&str; 4] = [BOTS_SUFFIX, ACTION_SUFFIX, BULK_SUFFIX, HOSTILE_SUFFIX];

/// The downstream rate the sample game needs per client at full detail, bits per second: about
/// 140 entities of 12 bytes at 30 snapshots per second plus headers (`book/agents/net-soak.md`).
pub const GAME_DOWN_NEED_BPS: u64 = 450_000;
/// Downstream cap of the `congested` profile, bits per second: below [`GAME_DOWN_NEED_BPS`].
pub const CONGESTED_DOWN_BPS: u64 = 256_000;
/// Length of each `flappy` outage, milliseconds.
pub const FLAP_OUTAGE_MS: u64 = 3_000;
/// Time between the starts of two `flappy` outages, milliseconds.
pub const FLAP_PERIOD_MS: u64 = 20_000;
/// How far the long `flappy` outage runs past the engine's removal timeout, milliseconds.
pub const FLAP_LONG_PAST_TIMEOUT_MS: u64 = 2_000;

/// The long `flappy` outage, on every fourth client, milliseconds: the engine's removal timeout
/// (`regia_net::TimeoutPolicy::default().timeout_ms`, ADR-7950) plus
/// [`FLAP_LONG_PAST_TIMEOUT_MS`]. It is derived, not a number of the kit's own, so it ends the
/// connection whatever the engine's timeout is.
pub fn flap_long_outage_ms() -> u64 {
    regia_net::TimeoutPolicy::default().timeout_ms + FLAP_LONG_PAST_TIMEOUT_MS
}
/// `clock-drift`: how fast (even clients) or slow (odd clients) a client clock runs, parts per
/// million (0.5 %).
pub const CLOCK_DRIFT_PPM: i64 = 5_000;
/// `long-outage`: when every client's outage starts, milliseconds.
pub const LONG_OUTAGE_AT_MS: u64 = 60_000;
/// `long-outage`: the outage lengths, by client index modulo 3, milliseconds.
pub const LONG_OUTAGES_MS: [u64; 3] = [11_000, 25_000, 45_000];
/// `nat-rebind`: time between two source-port changes, milliseconds.
pub const NAT_REBIND_EVERY_MS: u64 = 30_000;
/// `rtt-step`: `(at ms, one-way ms)` steps after the start (20 ms).
pub const RTT_STEPS: [(u64, u64); 2] = [(60_000, 125), (120_000, 20)];
/// `mtu-blackhole`: when the path MTU drops from 1,280 to 1,200, milliseconds.
pub const MTU_BLACKHOLE_AT_MS: u64 = 30_000;
/// `cgnat`: clients per group; in each group the first nine share one public IPv4 and the tenth
/// moves in from another address at [`CGNAT_MOVE_AT`] of the run.
pub const CGNAT_GROUP: usize = 10;
/// `cgnat`: the tenth client of a group moves in at this fraction of the run.
pub const CGNAT_MOVE_AT: f64 = 0.4;
/// `cgnat`: the server's resume grace, milliseconds: `regia_net::link::DEFAULT_RESUME_GRACE_MS`,
/// what a `NetLink` server keeps (ADR-8733).
pub const CGNAT_RESUME_GRACE_MS: u64 = 60_000;
/// `flappy` and `long-outage`: the server's resume grace, milliseconds:
/// `regia_net::link::DEFAULT_RESUME_GRACE_MS`, what a `NetLink` server keeps, and the window a
/// client's automatic reconnect runs for (`regia_net::reconnect::DEFAULT_RECONNECT_WINDOW_MS`).
pub const OUTAGE_RESUME_GRACE_MS: u64 = 60_000;
/// `wifi-to-cell`: the switch starts at this fraction of the run.
pub const WIFI_TO_CELL_AT: f64 = 0.4;
/// `wifi-to-cell`: the outage during the switch, milliseconds.
pub const WIFI_TO_CELL_OUTAGE_MS: u64 = 2_000;
/// `asym-bufferbloat`: the downstream bottleneck, bits per second.
pub const ASYM_DOWN_BPS: u64 = 4_000_000;
/// `asym-bufferbloat`: the cross traffic offered into the downstream queue, bits per second.
pub const ASYM_CROSS_BPS: u64 = 4_400_000;
/// `listen-uplink` (and listen mode): the host's uplink shared by every client, bits per second.
pub const LISTEN_UPLINK_BPS: u64 = 20_000_000;
/// The private sub-profile name of `rtt-extreme`'s extreme peer.
pub const RTT_EXTREME_PEER: &str = "@rtt-extreme-peer";

/// One named profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Name.
    pub name: String,
    /// What it stands for.
    pub summary: String,
    /// Client upstream.
    pub up: Direction,
    /// Client downstream.
    pub down: Direction,
    /// Short outages: `(first start ms, period ms, length ms)`.
    pub flaps: Option<(u64, u64, u64)>,
    /// One long outage at half the run, on every fourth client, milliseconds.
    pub long_outage_ms: Option<u64>,
    /// NAT rebinding once per client at this fraction of the run.
    pub rebind_at: Option<f64>,
    /// A hostile peer joins.
    pub hostile: bool,
    /// Per-client sub-profiles `(profile name, weight)`; empty for a uniform profile.
    #[serde(default)]
    pub mix: Vec<(String, u32)>,
    /// The server's uplink in either mode (`listen-uplink`); `None` keeps the mode's link.
    #[serde(default)]
    pub server_up: Option<Direction>,
    /// The gate and nightly plans run this profile at least this long, milliseconds (its schedule
    /// needs it); custom plans run what they ask for.
    #[serde(default)]
    pub min_duration_ms: u64,
    /// The gate and nightly plans run this profile with at least this many clients.
    #[serde(default)]
    pub min_clients: usize,
    /// An engine part the profile needs that does not exist yet (`relay v2`): the run is skipped
    /// with this reason until the owning lane lands.
    #[serde(default)]
    pub needs: Option<String>,
    /// One peer floods the server with valid sealed records (`flood`).
    #[serde(default)]
    pub flood_peer: bool,
    /// Forged `ConnectRequest`s flood the server from many /24s (`forged-handshake`).
    #[serde(default)]
    pub forged_handshake: bool,
    /// A bulk transfer to every client: `(start ms, bytes)`.
    #[serde(default)]
    pub bulk: Option<(u64, usize)>,
    /// Server and clients run path MTU discovery (`LinkConfig::mtu_probing`, spec 120 § 4.3,
    /// ADR-8300): `mtu-1280` and `mtu-blackhole`.
    #[serde(default)]
    pub mtu_probing: bool,
    /// Overrides how long the server keeps reconnect tickets, milliseconds
    /// ([`Profile::server_resume_grace_ms`]); `None` keeps the profile's own choice. The plans
    /// never set it: a measurement sets it to run a profile's rejoins through the one-round-trip
    /// resume (`docs/perf/net-resume-wire6.md`, ADR-10088).
    #[serde(default)]
    pub resume_grace_ms: Option<u64>,
}

fn wifi_good() -> (Direction, Direction) {
    let base = Direction {
        latency_ms: 4,
        jitter_ms: 4,
        loss: 0.002,
        reorder: 0.001,
        reorder_ms: 10,
        duplicate: 0.0005,
        queue_bytes: 64 * 1024,
        ..Direction::clean()
    };
    (
        Direction {
            bandwidth_bps: 20_000_000,
            ..base.clone()
        },
        Direction {
            bandwidth_bps: 50_000_000,
            ..base
        },
    )
}

impl Profile {
    /// The profile called `name`. `<profile>+bulk` is that profile plus a [`BULK_BYTES`] transfer
    /// to every client at [`BULK_AT`] of the run (`bulk` is set for a 60 s run; the plan moves it
    /// with [`Profile::fit_to`]); `<profile>+hostile` adds one hostile peer ([`HOSTILE_SUFFIX`]).
    /// `lockstep-mix+action` is the lockstep profile on the same links
    /// with the action input mix; `lockstep-mix+bots` is the same profile on the wander-fire bot
    /// mix (ADR-8409). No other profile takes either suffix, a lockstep profile takes no bulk
    /// transfer, and the two suffixes do not stack.
    pub fn named(name: &str) -> Option<Self> {
        if let Some(base) = name.strip_suffix(BOTS_SUFFIX) {
            if base != LOCKSTEP_MIX {
                return None;
            }
            let mut p = Self::named(base)?;
            p.name = name.to_string();
            p.summary = format!(
                "{} with the wander-fire bot input mix (a held heading, fire pulses and target lead)",
                p.summary
            );
            return Some(p);
        }
        if let Some(base) = name.strip_suffix(ACTION_SUFFIX) {
            if base != LOCKSTEP_MIX {
                return None;
            }
            let mut p = Self::named(base)?;
            p.name = name.to_string();
            p.summary = format!(
                "{} with the action input mix (half the seats change their aim on any tick)",
                p.summary
            );
            return Some(p);
        }
        if let Some(base) = name.strip_suffix(BULK_SUFFIX) {
            let mut p = Self::named(base)?;
            if p.is_lockstep() {
                return None;
            }
            p.name = name.to_string();
            p.summary = format!(
                "{} plus a {} KiB bulk transfer to every client at half the run",
                p.summary,
                BULK_BYTES / 1024
            );
            p.bulk = Some(((60_000.0 * BULK_AT) as u64, BULK_BYTES));
            return Some(p);
        }
        if let Some(base) = name.strip_suffix(HOSTILE_SUFFIX) {
            let mut p = Self::named(base)?;
            if p.hostile {
                return None;
            }
            p.name = name.to_string();
            p.summary = format!("{} plus one hostile peer (the hostile profile's acts)", p.summary);
            p.hostile = true;
            return Some(p);
        }
        let p = |summary: &str, up: Direction, down: Direction| Profile {
            name: name.to_string(),
            summary: summary.to_string(),
            up,
            down,
            flaps: None,
            long_outage_ms: None,
            rebind_at: None,
            hostile: false,
            mix: Vec::new(),
            server_up: None,
            min_duration_ms: 0,
            min_clients: 0,
            needs: None,
            flood_peer: false,
            forged_handshake: false,
            bulk: None,
            mtu_probing: false,
            resume_grace_ms: None,
        };
        let one_way = |latency_ms: u64, jitter_ms: u64, loss: f64| Direction {
            latency_ms,
            jitter_ms,
            loss,
            ..Direction::clean()
        };
        Some(match name {
            "clean" => {
                let d = Direction {
                    latency_ms: 1,
                    ..Direction::clean()
                };
                p("1 ms one way, nothing else", d.clone(), d)
            }
            "good-wifi" => {
                let (up, down) = wifi_good();
                p(
                    "4 ms + 0-4 ms jitter, 0.2 % loss, 0.1 % reorder, 0.05 % dup, 20/50 Mbit/s",
                    up,
                    down,
                )
            }
            "bad-wifi" => {
                let d = Direction {
                    latency_ms: 8,
                    jitter_ms: 30,
                    loss: 0.01,
                    burst: Some(GilbertElliott {
                        p_good_to_bad: 0.01,
                        p_bad_to_good: 0.3,
                        loss_good: 0.0,
                        loss_bad: 0.6,
                    }),
                    reorder: 0.01,
                    reorder_ms: 30,
                    duplicate: 0.005,
                    queue_bytes: 64 * 1024,
                    ..Direction::clean()
                };
                p(
                    "8 ms + 0-30 ms jitter, 1 % random + Gilbert-Elliott bursts (~3 % total), 1 % reorder, 2/6 Mbit/s",
                    Direction {
                        bandwidth_bps: 2_000_000,
                        ..d.clone()
                    },
                    Direction {
                        bandwidth_bps: 6_000_000,
                        ..d
                    },
                )
            }
            "mobile-4g" => {
                let d = Direction {
                    latency_ms: 35,
                    jitter_ms: 25,
                    loss: 0.005,
                    burst: Some(GilbertElliott {
                        p_good_to_bad: 0.005,
                        p_bad_to_good: 0.2,
                        loss_good: 0.0,
                        loss_bad: 0.5,
                    }),
                    reorder: 0.005,
                    reorder_ms: 40,
                    duplicate: 0.001,
                    queue_bytes: 128 * 1024,
                    mtu: 1232,
                    ..Direction::clean()
                };
                let mut prof = p(
                    "35 ms + 0-25 ms jitter, 0.5 % + bursts, MTU 1232, 2/8 Mbit/s with a 128 KiB queue, one NAT rebinding",
                    Direction {
                        bandwidth_bps: 2_000_000,
                        ..d.clone()
                    },
                    Direction {
                        bandwidth_bps: 8_000_000,
                        ..d
                    },
                );
                prof.rebind_at = Some(0.4);
                prof
            }
            "transatlantic" => {
                let d = Direction {
                    latency_ms: 45,
                    jitter_ms: 6,
                    loss: 0.003,
                    reorder: 0.002,
                    reorder_ms: 15,
                    queue_bytes: 64 * 1024,
                    ..Direction::clean()
                };
                p(
                    "45 ms + 0-6 ms jitter one way (~100 ms RTT), 0.3 % loss, 20/50 Mbit/s",
                    Direction {
                        bandwidth_bps: 20_000_000,
                        ..d.clone()
                    },
                    Direction {
                        bandwidth_bps: 50_000_000,
                        ..d
                    },
                )
            }
            "congested" => {
                let d = Direction {
                    latency_ms: 15,
                    jitter_ms: 10,
                    queue_bytes: 16 * 1024,
                    ..Direction::clean()
                };
                p(
                    "15 ms + 0-10 ms jitter, downstream capped at 256 kbit/s (below the game's ~450 kbit/s) behind a 16 KiB drop-tail queue, 1 Mbit/s up",
                    Direction {
                        bandwidth_bps: 1_000_000,
                        ..d.clone()
                    },
                    Direction {
                        bandwidth_bps: CONGESTED_DOWN_BPS,
                        ..d
                    },
                )
            }
            "flappy" => {
                let (up, down) = wifi_good();
                let long = flap_long_outage_ms();
                let mut prof = p(
                    &format!(
                        "good-wifi plus a 3 s outage every 20 s, and one {} s outage ({} s beyond the engine's {} s removal timeout) on every fourth client",
                        long / 1000,
                        FLAP_LONG_PAST_TIMEOUT_MS / 1000,
                        (long - FLAP_LONG_PAST_TIMEOUT_MS) / 1000
                    ),
                    up,
                    down,
                );
                prof.flaps = Some((15_000, FLAP_PERIOD_MS, FLAP_OUTAGE_MS));
                prof.long_outage_ms = Some(long);
                prof
            }
            "hostile" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "good-wifi plus one hostile peer: over-cap and malformed inputs and messages, replays, garbage and oversized datagrams, floods",
                    up,
                    down,
                );
                prof.hostile = true;
                prof
            }
            "high-rtt" => p(
                "225 ms (even clients) or 325 ms (odd clients) one way (RTT 450 / 650 ms), 0-20 ms jitter, 1 % loss",
                one_way(225, 20, 0.01),
                one_way(225, 20, 0.01),
            ),
            "rtt-step" => {
                let mut prof = p(
                    "20 ms one way + 0-5 ms jitter, stepping to 125 ms at 60 s and back to 20 ms at 120 s",
                    one_way(20, 5, 0.0),
                    one_way(20, 5, 0.0),
                );
                prof.min_duration_ms = 180_000;
                prof
            }
            "clock-drift" => {
                let (up, down) = wifi_good();
                p(
                    "good-wifi; even clients' clocks run 0.5 % fast, odd clients' 0.5 % slow against the server's",
                    up,
                    down,
                )
            }
            "nat-rebind" => {
                let (up, down) = wifi_good();
                p(
                    "good-wifi; every client's source port changes every 30 s, without an outage",
                    up,
                    down,
                )
            }
            "cgnat" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "good-wifi; in each group of 10 clients nine share one public IPv4 (distinct ports) and the tenth moves in from another IP at 40 % of the run",
                    up,
                    down,
                );
                prof.min_clients = CGNAT_GROUP;
                prof
            }
            "wifi-to-cell" => {
                let (up, down) = wifi_good();
                p(
                    "Wi-Fi (15 ms one way, 0.5 % loss) to cellular (60 ms, 3 % loss) at 40 % of the run: a 2 s outage, a new public IP, then the cellular numbers",
                    Direction {
                        latency_ms: 15,
                        loss: 0.005,
                        ..up
                    },
                    Direction {
                        latency_ms: 15,
                        loss: 0.005,
                        ..down
                    },
                )
            }
            "long-outage" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "good-wifi; at 60 s a two-way outage of 11 s, 25 s or 45 s (client index mod 3)",
                    up,
                    down,
                );
                prof.min_duration_ms = 120_000;
                prof
            }
            "bufferbloat" => {
                let d = Direction {
                    latency_ms: 20,
                    bandwidth_bps: 1_000_000,
                    queue_bytes: 125_000,
                    ..Direction::clean()
                };
                p(
                    "1 Mbit/s bottleneck each way behind a 1 s (125,000-byte) drop-tail queue, no random loss, 20 ms one way",
                    d.clone(),
                    d,
                )
            }
            "downstream-loss" => p(
                "30 ms one way; 0 % loss up, 15 % random loss down",
                one_way(30, 0, 0.0),
                one_way(30, 0, 0.15),
            ),
            "asym-bufferbloat" => {
                let (up, down) = wifi_good();
                p(
                    "good-wifi up; down 4 Mbit/s behind a 1 s (500,000-byte) drop-tail queue kept full by cross traffic offered at 4.4 Mbit/s",
                    up,
                    Direction {
                        bandwidth_bps: ASYM_DOWN_BPS,
                        queue_bytes: 500_000,
                        cross_bps: ASYM_CROSS_BPS,
                        ..down
                    },
                )
            }
            "listen-uplink" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "good-wifi clients; the server's uplink is 20 Mbit/s shared by every client behind a 128 KiB drop-tail queue (5 ms), in either mode",
                    up,
                    down,
                );
                prof.server_up = Some(listen_server_link().up);
                prof
            }
            "lossy-fat-pipe" => {
                let d = Direction {
                    latency_ms: 20,
                    loss: 0.05,
                    bandwidth_bps: 100_000_000,
                    queue_bytes: 64 * 1024,
                    ..Direction::clean()
                };
                p(
                    "100 Mbit/s each way, 5 % random loss, 20 ms one way, no standing queue (64 KiB buffer = 5 ms at 100 Mbit/s)",
                    d.clone(),
                    d,
                )
            }
            "rtt-extreme" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "good-wifi, except one client in 8 on 1,500 ms one way with 20 % loss each way",
                    up,
                    down,
                );
                prof.mix = vec![("good-wifi".into(), 7), (RTT_EXTREME_PEER.into(), 1)];
                prof
            }
            "mtu-1280" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "good-wifi with a path MTU of 1,280 bytes; path MTU probing on",
                    Direction { mtu: 1_280, ..up },
                    Direction { mtu: 1_280, ..down },
                );
                prof.mtu_probing = true;
                prof
            }
            "mtu-blackhole" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "good-wifi with a path MTU of 1,280 bytes, then 1,200 from 30 s with every larger datagram dropped silently; path MTU probing on",
                    Direction { mtu: 1_280, ..up },
                    Direction { mtu: 1_280, ..down },
                );
                prof.mtu_probing = true;
                prof
            }
            "flood" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "good-wifi plus one peer sending 100,000 datagrams/s of valid sealed records from 10 s for 10 s",
                    up,
                    down,
                );
                prof.flood_peer = true;
                prof
            }
            "forged-handshake" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "good-wifi plus 100,000 forged ConnectRequests/s from 1,000 /24s for the whole run",
                    up,
                    down,
                );
                prof.forged_handshake = true;
                prof
            }
            "relay-path" => {
                let (up, down) = wifi_good();
                let mut prof = p("every client through the relay, good-wifi on each leg", up, down);
                prof.needs = Some("relay v2".into());
                prof
            }
            "lockstep-mix" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "8/16/40 lockstep seats on a mix of good-wifi, bad-wifi, mobile-4g and transatlantic",
                    up,
                    down,
                );
                prof.mix = vec![
                    ("good-wifi".into(), 1),
                    ("bad-wifi".into(), 1),
                    ("mobile-4g".into(), 1),
                    ("transatlantic".into(), 1),
                ];
                // The `lockstep_upload` bar is stated over a five-minute run (ADR-8209).
                prof.min_duration_ms = super::lockstep::UPLOAD_BAR_RUN_MS;
                prof
            }
            "mixed" => {
                let (up, down) = wifi_good();
                let mut prof = p(
                    "40 % good-wifi, 25 % bad-wifi, 15 % mobile-4g, 10 % transatlantic, 5 % flappy, 5 % congested",
                    up,
                    down,
                );
                prof.mix = vec![
                    ("good-wifi".into(), 40),
                    ("bad-wifi".into(), 25),
                    ("mobile-4g".into(), 15),
                    ("transatlantic".into(), 10),
                    ("flappy".into(), 5),
                    ("congested".into(), 5),
                ];
                prof
            }
            _ => return None,
        })
    }

    /// Whether a run of this profile is a lockstep match (`lockstep-mix` and its `+action` and
    /// `+bots` variants): the plan hands it to the lockstep soak instead of the replication
    /// workload, and its clients are the match's seats. One suffix only.
    pub fn is_lockstep(&self) -> bool {
        let base = self
            .name
            .strip_suffix(BOTS_SUFFIX)
            .or_else(|| self.name.strip_suffix(ACTION_SUFFIX))
            .unwrap_or(&self.name);
        base == LOCKSTEP_MIX
    }

    /// The input mix of a lockstep profile by name: `bots` for the `+bots` variant, `action` for
    /// the `+action` variant, else `orders`; `None` for a replication profile.
    pub fn lockstep_mix(&self) -> Option<&'static str> {
        if !self.is_lockstep() {
            None
        } else if self.name.ends_with(BOTS_SUFFIX) {
            Some("bots")
        } else if self.name.ends_with(ACTION_SUFFIX) {
            Some("action")
        } else {
            Some("orders")
        }
    }

    /// The sub-profile of client `index`: an entry of [`Profile::mix`] chosen by a smooth weighted
    /// round robin over the index (deterministic, and every prefix of the clients holds the shares
    /// as closely as whole clients allow), or the profile's own name.
    pub fn client_profile(&self, index: usize) -> String {
        let total: i64 = self.mix.iter().map(|(_, w)| i64::from(*w)).sum();
        if total <= 0 {
            return self.name.clone();
        }
        let mut current = vec![0i64; self.mix.len()];
        let mut pick = 0;
        for _ in 0..=index {
            for (c, (_, w)) in current.iter_mut().zip(&self.mix) {
                *c += i64::from(*w);
            }
            pick = 0;
            for (i, c) in current.iter().enumerate() {
                if *c > current[pick] {
                    pick = i;
                }
            }
            current[pick] -= total;
        }
        self.mix[pick].0.clone()
    }

    /// How long the profile's server keeps a player's reconnect ticket, milliseconds
    /// (`ServerConfig::resume_grace_ms`): `cgnat` and `asym-bufferbloat` run the 60 s a `NetLink`
    /// server keeps by default. `cgnat`'s rejoin is the per-address hold under test (ADR-8733);
    /// `asym-bufferbloat`'s is the one-round-trip ticket resume (wire 6, ADR-8733 (3), ADR-8902):
    /// behind its 1 s queue the three round trips of a full handshake cannot make the reconnect
    /// grace, and a server that keeps no tickets seals none into its accept. `flappy` and
    /// `long-outage` keep tickets too: their outages beyond the removal timeout are what the
    /// dormant resume is for (spec 120 § 10.3, ADR-10093). Every other profile keeps the transport
    /// default, no tickets (0), so its runs are unchanged. A set [`Profile::resume_grace_ms`]
    /// overrides all of this.
    pub fn server_resume_grace_ms(&self) -> u64 {
        if let Some(ms) = self.resume_grace_ms {
            ms
        } else {
            match self.name.as_str() {
                "cgnat" | "asym-bufferbloat" => CGNAT_RESUME_GRACE_MS,
                "flappy" | "long-outage" => OUTAGE_RESUME_GRACE_MS,
                _ => 0,
            }
        }
    }

    /// The public address client `index` starts on, when the profile places it (`cgnat`); `None`
    /// keeps the workload's own address.
    pub fn client_addr(&self, index: usize) -> Option<SocketAddr> {
        if self.name != "cgnat" {
            return None;
        }
        let (group, member) = (index / CGNAT_GROUP, index % CGNAT_GROUP);
        Some(if member < CGNAT_GROUP - 1 {
            SocketAddr::new(cgnat_ip(group), 40_000 + member as u16)
        } else {
            SocketAddr::from((
                [198, 51, (group / 250) as u8 + 100, (group % 250) as u8 + 1],
                40_000,
            ))
        })
    }

    /// The access link of client `index` for a run of `duration_ms`.
    pub fn client_link(&self, index: usize, duration_ms: u64) -> LinkSpec {
        let sub = self.client_profile(index);
        if sub == RTT_EXTREME_PEER {
            let d = Direction {
                latency_ms: 1_500,
                loss: 0.2,
                ..Direction::clean()
            };
            return LinkSpec {
                up: d.clone(),
                down: d,
                ..LinkSpec::default()
            };
        }
        if sub != self.name {
            return Profile::named(&sub)
                .map_or_else(LinkSpec::default, |p| p.client_link(index, duration_ms));
        }
        let mut outages = Vec::new();
        // Clients do not all flap in the same millisecond: shift each by up to one second.
        let shift = (index as u64 * 137) % 1_000;
        if let Some((first, period, len)) = self.flaps {
            let mut start = first + shift;
            while start + len < duration_ms {
                outages.push((start, start + len));
                start += period.max(len + 1);
            }
        }
        if let Some(len) = self.long_outage_ms
            && index % 4 == 3
        {
            let start = duration_ms / 2 + shift;
            // The long outage replaces any short one it overlaps.
            outages.retain(|&(s, e)| e <= start || s >= start + len);
            outages.push((start, start + len));
            outages.sort_unstable();
        }
        let mut rebinds_ms = self
            .rebind_at
            .map(|f| vec![(duration_ms as f64 * f) as u64 + index as u64 * 97])
            .unwrap_or_default();
        let mut link = LinkSpec {
            up: self.up.clone(),
            down: self.down.clone(),
            ..LinkSpec::default()
        };
        match self.name.as_str() {
            "high-rtt" if index % 2 == 1 => {
                link.up.latency_ms = 325;
                link.down.latency_ms = 325;
            }
            "rtt-step" => {
                for (at_ms, latency_ms) in RTT_STEPS {
                    link.phases.push(LinkPhase {
                        at_ms,
                        up: Direction {
                            latency_ms,
                            ..self.up.clone()
                        },
                        down: Direction {
                            latency_ms,
                            ..self.down.clone()
                        },
                    });
                }
            }
            "clock-drift" => {
                link.clock_ppm = if index.is_multiple_of(2) {
                    CLOCK_DRIFT_PPM
                } else {
                    -CLOCK_DRIFT_PPM
                };
            }
            "nat-rebind" => {
                let mut t = NAT_REBIND_EVERY_MS + index as u64 * 97;
                while t < duration_ms {
                    rebinds_ms.push(t);
                    t += NAT_REBIND_EVERY_MS;
                }
            }
            "cgnat" if index % CGNAT_GROUP == CGNAT_GROUP - 1 => {
                link.ip_moves.push((
                    (duration_ms as f64 * CGNAT_MOVE_AT) as u64,
                    cgnat_ip(index / CGNAT_GROUP),
                ));
            }
            "wifi-to-cell" => {
                let at = (duration_ms as f64 * WIFI_TO_CELL_AT) as u64 + shift;
                outages.push((at, at + WIFI_TO_CELL_OUTAGE_MS));
                let cell_ip = IpAddr::V4(Ipv4Addr::new(
                    172,
                    20,
                    (index / 250) as u8,
                    (index % 250) as u8 + 1,
                ));
                link.ip_moves.push((at + WIFI_TO_CELL_OUTAGE_MS / 2, cell_ip));
                link.phases.push(LinkPhase {
                    at_ms: at + WIFI_TO_CELL_OUTAGE_MS,
                    up: Direction {
                        latency_ms: 60,
                        loss: 0.03,
                        ..self.up.clone()
                    },
                    down: Direction {
                        latency_ms: 60,
                        loss: 0.03,
                        ..self.down.clone()
                    },
                });
            }
            "long-outage" => {
                let len = LONG_OUTAGES_MS[index % LONG_OUTAGES_MS.len()];
                outages.push((LONG_OUTAGE_AT_MS, LONG_OUTAGE_AT_MS + len));
            }
            "mtu-blackhole" => {
                link.phases.push(LinkPhase {
                    at_ms: MTU_BLACKHOLE_AT_MS,
                    up: Direction {
                        mtu: 1_200,
                        ..self.up.clone()
                    },
                    down: Direction {
                        mtu: 1_200,
                        ..self.down.clone()
                    },
                });
            }
            _ => {}
        }
        link.outages = outages;
        link.rebinds_ms = rebinds_ms;
        link
    }

    /// The server's link in `mode` for this profile: the mode's own link with the profile's
    /// uplink when it sets one.
    pub fn server_link(&self, listen: bool) -> LinkSpec {
        let mut link = if listen {
            listen_server_link()
        } else {
            LinkSpec::default()
        };
        if let Some(up) = &self.server_up {
            link.up = up.clone();
        }
        link
    }

    /// Moves the run-relative parts (the bulk transfer) to a run of `duration_ms`.
    pub fn fit_to(&mut self, duration_ms: u64) {
        if let Some((_, bytes)) = self.bulk {
            self.bulk = Some(((duration_ms as f64 * BULK_AT) as u64, bytes));
        }
    }
}

fn cgnat_ip(group: usize) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(
        100,
        64,
        (group / 250) as u8,
        (group % 250) as u8 + 1,
    ))
}

/// The server's link in listen-server mode: a home connection, 5 ms one way, 20 Mbit/s up shared by
/// every client behind a 128 KiB drop-tail queue, 100 Mbit/s down.
pub fn listen_server_link() -> LinkSpec {
    LinkSpec {
        up: Direction {
            latency_ms: 5,
            bandwidth_bps: LISTEN_UPLINK_BPS,
            queue_bytes: 128 * 1024,
            ..Direction::clean()
        },
        down: Direction {
            latency_ms: 5,
            bandwidth_bps: 100_000_000,
            queue_bytes: 256 * 1024,
            ..Direction::clean()
        },
        ..LinkSpec::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_resolves_and_unknown_does_not() {
        for name in PROFILE_NAMES {
            let p = Profile::named(name).expect(name);
            assert_eq!(p.name, name);
        }
        assert!(Profile::named("dial-up").is_none());
    }

    #[test]
    fn congested_is_below_the_game_need() {
        let p = Profile::named("congested").expect("congested");
        assert!(p.down.bandwidth_bps < GAME_DOWN_NEED_BPS);
    }

    #[test]
    fn flappy_schedule_has_short_and_long_outages() {
        let p = Profile::named("flappy").expect("flappy");
        let short = p.client_link(0, 120_000);
        assert!(short.outages.len() >= 5, "{:?}", short.outages);
        assert!(short.outages.iter().all(|(s, e)| e - s == FLAP_OUTAGE_MS));
        let long = p.client_link(3, 120_000);
        assert!(long.outages.iter().any(|(s, e)| e - s == flap_long_outage_ms()));
        assert!(long.outages.windows(2).all(|w| w[0].1 <= w[1].0), "no overlap");
    }

    /// The long `flappy` outage is longer than the engine's removal timeout, whatever that is, and
    /// the short ones are shorter: the kit has no timeout number of its own (ADR-7950).
    #[test]
    fn the_long_flappy_outage_follows_the_engines_removal_timeout() {
        let policy = regia_net::TimeoutPolicy::default();
        assert_eq!(
            flap_long_outage_ms(),
            policy.timeout_ms + FLAP_LONG_PAST_TIMEOUT_MS
        );
        assert!(flap_long_outage_ms() > policy.timeout_ms);
        assert!(FLAP_OUTAGE_MS < policy.timeout_ms && FLAP_OUTAGE_MS >= policy.interrupt_ms);
        let p = Profile::named("flappy").expect("flappy");
        assert_eq!(p.long_outage_ms, Some(flap_long_outage_ms()));
        assert!(
            p.summary
                .contains(&format!("one {} s outage", flap_long_outage_ms() / 1000))
                && p.summary
                    .contains(&format!("{} s removal timeout", policy.timeout_ms / 1000)),
            "{}",
            p.summary
        );
        // `long-outage` keeps one outage the policy survives and two it does not.
        let survived = LONG_OUTAGES_MS
            .iter()
            .filter(|&&ms| ms < policy.timeout_ms)
            .count();
        let removed = LONG_OUTAGES_MS
            .iter()
            .filter(|&&ms| ms > policy.timeout_ms)
            .count();
        assert_eq!((survived, removed), (1, 2), "{LONG_OUTAGES_MS:?}");
    }

    #[test]
    fn mobile_rebinds_once_per_client() {
        let p = Profile::named("mobile-4g").expect("mobile");
        assert_eq!(p.client_link(5, 60_000).rebinds_ms, vec![24_000 + 5 * 97]);
    }

    use crate::impair::{DropCause, ImpairedNet};
    use regia_net::DatagramSocket;

    /// Every arrival `(ms, receiver, first bytes)`.
    type Arrivals = Vec<(u64, usize, Vec<u8>)>;

    /// Drives two clients of `name` against a clean server for `ms` virtual milliseconds (a 30 Hz
    /// 600-byte stream each way) and returns every arrival and each client's counters.
    fn drive(name: &str, seed: u64, ms: u64) -> (Arrivals, Vec<crate::impair::EndpointStats>) {
        let p = Profile::named(name).expect(name);
        let net = ImpairedNet::new(seed);
        let server_addr = SocketAddr::from(([10, 0, 0, 1], 7777));
        let server = net
            .add_endpoint(server_addr, p.server_link(false))
            .expect("server");
        let mut ss = net.socket(server);
        let clients: Vec<_> = (0..2)
            .map(|i| {
                let a = p
                    .client_addr(i)
                    .unwrap_or_else(|| SocketAddr::from(([10, 1, 0, i as u8 + 1], 40_000)));
                net.add_endpoint(a, p.client_link(i, ms)).expect("client")
            })
            .collect();
        let mut got = Vec::new();
        let mut buf = vec![0u8; 2048];
        for t in 0..=ms {
            net.advance_to_ms(t);
            if t % 33 == 0 {
                for (i, &c) in clients.iter().enumerate() {
                    let mut sc = net.socket(c);
                    let payload = vec![(t % 251) as u8; 600];
                    sc.send_to(&payload, server_addr).expect("send");
                    ss.send_to(&payload, net.public_addr(c)).expect("send");
                    let _ = i;
                }
            }
            for (i, &c) in clients.iter().enumerate() {
                let mut sc = net.socket(c);
                while let Some((n, _)) = sc.recv_from(&mut buf).expect("io") {
                    got.push((t, i, buf[..n.min(4)].to_vec()));
                }
            }
            while let Some((n, from)) = ss.recv_from(&mut buf).expect("io") {
                got.push((t, 100 + usize::from(from.port() % 7), buf[..n.min(4)].to_vec()));
            }
        }
        (got, clients.iter().map(|&c| net.stats(c)).collect())
    }

    /// Every spec 120 profile's model is seeded: the same seed gives the same arrivals and drops,
    /// and the random ones change with the seed.
    #[test]
    fn every_new_profile_has_a_seeded_fate() {
        for name in NEW_PROFILE_NAMES {
            let (a, sa) = drive(name, 7, 4_000);
            let (b, sb) = drive(name, 7, 4_000);
            assert_eq!(a, b, "{name}: same seed, same arrivals");
            assert_eq!(sa, sb, "{name}: same seed, same counters");
            assert!(!a.is_empty(), "{name}: something arrives");
            let random = ["high-rtt", "downstream-loss", "lossy-fat-pipe", "rtt-extreme"];
            if random.contains(&name) {
                assert_ne!(a, drive(name, 8, 4_000).0, "{name}: another seed, another fate");
            }
        }
    }

    #[test]
    fn new_profiles_have_the_numbers_spec_120_states() {
        let n = |name: &str| Profile::named(name).expect(name);
        // high-rtt: 225 / 325 ms one way, 20 ms jitter, 1 % loss.
        let h = n("high-rtt");
        assert_eq!(
            (
                h.client_link(0, 60_000).up.latency_ms,
                h.client_link(1, 60_000).down.latency_ms
            ),
            (225, 325)
        );
        assert_eq!((h.up.jitter_ms, h.up.loss), (20, 0.01));
        // rtt-step: 20 ms + 5 ms jitter, 125 ms at 60 s, 20 ms at 120 s.
        let l = n("rtt-step").client_link(0, 180_000);
        assert_eq!((l.up.latency_ms, l.up.jitter_ms), (20, 5));
        assert_eq!(l.directions_at(59_999).0.latency_ms, 20);
        assert_eq!(l.directions_at(60_000).1.latency_ms, 125);
        assert_eq!(l.directions_at(120_000).0.latency_ms, 20);
        // clock-drift: +0.5 % and -0.5 %: 3,000 virtual ms over 10 min.
        let c = n("clock-drift");
        assert_eq!(c.client_link(0, 600_000).local_ms(600_000), 603_000);
        assert_eq!(c.client_link(1, 600_000).local_ms(600_000), 597_000);
        // nat-rebind: every 30 s, no outage.
        let r = n("nat-rebind").client_link(2, 120_000);
        assert_eq!(r.rebinds_ms, vec![30_194, 60_194, 90_194]);
        assert!(r.outages.is_empty());
        // cgnat: nine clients share one IPv4 on distinct ports; the tenth moves in at 40 %.
        let g = n("cgnat");
        let shared: Vec<SocketAddr> = (0..9).map(|i| g.client_addr(i).expect("addr")).collect();
        assert!(shared.iter().all(|a| a.ip() == shared[0].ip()));
        assert_eq!(
            shared
                .iter()
                .map(|a| a.port())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            9
        );
        let mover = g.client_addr(9).expect("addr");
        assert_ne!(mover.ip(), shared[0].ip());
        assert_eq!(g.client_link(9, 60_000).ip_moves, vec![(24_000, shared[0].ip())]);
        assert!(g.client_link(3, 60_000).ip_moves.is_empty());
        // wifi-to-cell: 2 s outage, new IP, 15 -> 60 ms, 0.5 -> 3 % loss.
        let w = n("wifi-to-cell").client_link(0, 60_000);
        assert_eq!(w.outages, vec![(24_000, 26_000)]);
        assert_eq!(w.ip_moves.len(), 1);
        assert_eq!((w.up.latency_ms, w.up.loss), (15, 0.005));
        let (cu, cd) = w.directions_at(26_000);
        assert_eq!(
            (cu.latency_ms, cu.loss, cd.latency_ms, cd.loss),
            (60, 0.03, 60, 0.03)
        );
        // long-outage: 11 s, 25 s and 45 s two-way at 60 s.
        let o = n("long-outage");
        let lens: Vec<(u64, u64)> = (0..3).map(|i| o.client_link(i, 120_000).outages[0]).collect();
        assert_eq!(lens, vec![(60_000, 71_000), (60_000, 85_000), (60_000, 105_000)]);
        // bufferbloat: 1 Mbit/s, 1 s drop-tail queue, no loss, 20 ms.
        let b = n("bufferbloat");
        for d in [&b.up, &b.down] {
            assert_eq!(
                (d.bandwidth_bps, d.queue_bytes, d.loss, d.latency_ms),
                (1_000_000, 125_000, 0.0, 20)
            );
            assert_eq!(d.queue_bytes as u64 * 8, d.bandwidth_bps, "one second of queue");
        }
        // downstream-loss: 0 % up, 15 % down, 30 ms.
        let dl = n("downstream-loss");
        assert_eq!(
            (dl.up.loss, dl.down.loss, dl.up.latency_ms, dl.down.latency_ms),
            (0.0, 0.15, 30, 30)
        );
        // asym-bufferbloat: good-wifi up; 4 Mbit/s down, 1 s queue, 4.4 Mbit/s cross traffic.
        let a = n("asym-bufferbloat");
        assert_eq!(a.up, n("good-wifi").up);
        assert_eq!(
            (a.down.bandwidth_bps, a.down.queue_bytes, a.down.cross_bps),
            (4_000_000, 500_000, 4_400_000)
        );
        // listen-uplink: the host's 20 Mbit/s uplink, 128 KiB queue, in either mode.
        let lu = n("listen-uplink");
        for listen in [false, true] {
            let up = lu.server_link(listen).up;
            assert_eq!((up.bandwidth_bps, up.queue_bytes), (20_000_000, 128 * 1024));
        }
        // lossy-fat-pipe: 100 Mbit/s, 5 % random loss, 20 ms.
        let f = n("lossy-fat-pipe");
        assert_eq!(
            (f.down.bandwidth_bps, f.down.loss, f.down.latency_ms, f.down.burst),
            (100_000_000, 0.05, 20, None)
        );
        // rtt-extreme: one peer in 8 at 1,500 ms one way and 20 % loss.
        let x = n("rtt-extreme");
        let extreme: Vec<usize> = (0..32)
            .filter(|&i| x.client_profile(i) == RTT_EXTREME_PEER)
            .collect();
        assert_eq!(extreme.len(), 4);
        assert_eq!((0..8).filter(|i| extreme.contains(i)).count(), 1);
        let e = x.client_link(extreme[0], 60_000);
        assert_eq!(
            (e.up.latency_ms, e.up.loss, e.down.latency_ms, e.down.loss),
            (1_500, 0.2, 1_500, 0.2)
        );
        // mtu-1280 / mtu-blackhole: 1,280, then 1,200 from 30 s.
        assert_eq!(n("mtu-1280").up.mtu, 1_280);
        let m = n("mtu-blackhole").client_link(0, 60_000);
        assert_eq!(
            (m.directions_at(29_999).0.mtu, m.directions_at(30_000).1.mtu),
            (1_280, 1_200)
        );
        // flood, forged-handshake, relay-path, lockstep-mix.
        assert!(n("flood").flood_peer && n("forged-handshake").forged_handshake);
        assert_eq!(n("relay-path").needs.as_deref(), Some("relay v2"));
        // lockstep-mix runs since ADR-8209: no missing part, and five minutes in the gate and
        // nightly plans (the run length the upload bar is stated over).
        let lockstep = n("lockstep-mix");
        assert_eq!(
            (lockstep.needs.as_deref(), lockstep.min_duration_ms),
            (None, 300_000)
        );
        // mixed: 40/25/15/10/5/5 % of 40 clients exactly.
        let mixed = n("mixed");
        let mut counts = std::collections::BTreeMap::new();
        for i in 0..40 {
            *counts.entry(mixed.client_profile(i)).or_insert(0) += 1;
        }
        let want: std::collections::BTreeMap<String, i32> = [
            ("good-wifi", 16),
            ("bad-wifi", 10),
            ("mobile-4g", 6),
            ("transatlantic", 4),
            ("flappy", 2),
            ("congested", 2),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(counts, want);
        // A mixed client gets its sub-profile's link.
        let first_bad = (0..40)
            .find(|&i| mixed.client_profile(i) == "bad-wifi")
            .expect("bad-wifi client");
        assert_eq!(mixed.client_link(first_bad, 60_000).down, n("bad-wifi").down);
    }

    #[test]
    fn mtu_blackhole_drops_what_no_longer_fits() {
        let (_, stats) = drive("mtu-blackhole", 1, 100);
        assert_eq!(
            stats[0].drops_of(DropCause::Mtu),
            0,
            "600-byte datagrams fit either way"
        );
        let p = Profile::named("mtu-blackhole").expect("p");
        let net = ImpairedNet::new(1);
        let a = net
            .add_endpoint(SocketAddr::from(([10, 1, 0, 1], 1)), p.client_link(0, 60_000))
            .expect("a");
        let b = net
            .add_endpoint(SocketAddr::from(([10, 0, 0, 1], 1)), LinkSpec::default())
            .expect("b");
        let mut sa = net.socket(a);
        sa.send_to(&[0u8; 1_250], net.public_addr(b)).expect("send");
        net.advance_to_ms(30_000);
        sa.send_to(&[0u8; 1_250], net.public_addr(b)).expect("send");
        assert_eq!(
            net.stats(a).drops_of(DropCause::Mtu),
            1,
            "1,250 bytes fit before 30 s, not after"
        );
    }

    #[test]
    fn action_suffix_is_the_lockstep_mix_on_the_same_links() {
        let base = Profile::named("lockstep-mix").expect("lockstep-mix");
        let action = Profile::named("lockstep-mix+action").expect("variant");
        assert_eq!(action.name, "lockstep-mix+action");
        assert_eq!(
            (&action.mix, action.min_duration_ms, &action.needs),
            (&base.mix, base.min_duration_ms, &None)
        );
        for seat in 0..40 {
            assert_eq!(action.client_profile(seat), base.client_profile(seat));
            assert_eq!(
                action.client_link(seat, 300_000),
                base.client_link(seat, 300_000),
                "seat {seat}: the same link"
            );
        }
        // Only the lockstep profile has an input mix to switch, and a lockstep run has no bulk
        // transfer.
        assert!(Profile::named("clean+action").is_none());
        assert!(Profile::named("nope+action").is_none());
        assert!(Profile::named("lockstep-mix+bulk").is_none());
        assert!(Profile::named("lockstep-mix+action+bulk").is_none());
        assert!(Profile::named("lockstep-mix+action+bots").is_none());
        let bots = Profile::named("lockstep-mix+bots").expect("bots");
        assert!(bots.is_lockstep());
        assert_eq!(bots.lockstep_mix(), Some("bots"));
        assert_eq!(bots.min_duration_ms, base.min_duration_ms);
        assert!(Profile::named("clean+bots").is_none());
    }

    #[test]
    fn bulk_suffix_adds_a_transfer_at_half_the_run() {
        let mut p = Profile::named("congested+bulk").expect("variant");
        assert_eq!(p.down, Profile::named("congested").expect("congested").down);
        p.fit_to(300_000);
        assert_eq!(p.bulk, Some((150_000, BULK_BYTES)));
        assert!(Profile::named("nope+bulk").is_none());
    }
}
