// SPDX-License-Identifier: MPL-2.0
//! `lockstep_bot`: one process of a real-socket lockstep match (ADR-8146).
//!
//! ```text
//! lockstep_bot host --port <udp port> --seats <n> --ticks <t> [--bind <ip>]
//! lockstep_bot seat --server <ip:port> --seat <i> --ticks <t> [--bind <ip>] [--mix orders|action|bots] [--seed <n>] [--inject-desync-at <tick>]
//! ```
//!
//! The host is a dedicated observer: a `NetServer` on a real UDP socket, a `LockstepHost` and its
//! own simulation. Each seat is a `NetClient`, a `LockstepSeat` and a simulation. The host prints
//! one line per event (`desync <json>`, `located <tick> <voter> <first tick>`, `probed <json>`),
//! then `verdict PASS|FAIL`, an `upload` line and `done <ticks>`. The process exits 0 when the
//! match finishes, including when the verdict is `FAIL` (a desync); the deadline is exit 2.
//! `--bind` defaults to `127.0.0.1`. `--mix` defaults to `action`. `--seed` defaults to `0x5eed`
//! (24301). `--inject-desync-at` perturbs that seat's simulation after the tick; it exists only
//! in a build with the `desync-inject` feature.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use regia_net::lockstep::net::{host_receive, seat_receive, send_host, send_seat};
use regia_net::lockstep::{HostEvent, HostRole, LockstepConfig, LockstepHost, LockstepSeat, SeatId};
use regia_net::{
    ClientConfig, ClientEvent, NetClient, NetServer, PeerId, ServerConfig, ServerEvent, UdpTransport,
};
use regia_net_soak::impair::IP_UDP_OVERHEAD;
use regia_net_soak::lockstep::{InputMix, InputSource, LockGame, Track};

/// A process that has not finished after this long gives up (exit code 2).
const DEADLINE: Duration = Duration::from_secs(90);

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn number(args: &[String], name: &str) -> Option<u64> {
    arg(args, name)?.parse().ok()
}

fn bind_ip(args: &[String]) -> Result<std::net::IpAddr, String> {
    let text = arg(args, "--bind").unwrap_or_else(|| "127.0.0.1".to_string());
    text.parse().map_err(|e| format!("--bind: {e}"))
}

fn mix_of(args: &[String]) -> Result<InputMix, String> {
    match arg(args, "--mix").as_deref() {
        None | Some("action") => Ok(InputMix::ACTION),
        Some("orders") => Ok(InputMix::ORDERS),
        Some("bots") | Some("wander-fire") => Ok(InputMix::WANDER_FIRE),
        Some(other) => Err(format!(
            "--mix {other}: expected orders, action, bots or wander-fire"
        )),
    }
}

fn seed_of(args: &[String]) -> u64 {
    number(args, "--seed").unwrap_or(0x5eed)
}

fn config() -> LockstepConfig {
    LockstepConfig {
        // A verdict by majority, without the replay round: the test wants the seat named.
        self_check: false,
        ..LockstepConfig::default()
    }
}

fn host(args: &[String]) -> Result<(), String> {
    let port = number(args, "--port").ok_or("--port")? as u16;
    let seats = number(args, "--seats").ok_or("--seats")? as u16;
    let ticks = number(args, "--ticks").ok_or("--ticks")?;
    let addr = SocketAddr::new(bind_ip(args)?, port);
    let socket = UdpTransport::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
    let config = config();
    let mut server = NetServer::<LockGame, _>::new(
        socket,
        ServerConfig {
            max_peers: usize::from(seats) + 2,
            max_peers_per_ip: usize::from(seats) + 2,
            tick_rate_hz: config.tick_rate_hz,
            ..ServerConfig::default()
        },
    );
    let mut lockstep = LockstepHost::<LockGame>::new(config.clone(), HostRole::Observer);
    for seat in 0..seats {
        lockstep.add_seat(SeatId(seat));
    }
    let mut track = Track::new();
    let mut seat_of: BTreeMap<PeerId, SeatId> = BTreeMap::new();
    let mut peer_of: BTreeMap<SeatId, PeerId> = BTreeMap::new();
    // The sample simulation's floats need the default environment on this thread.
    regia_detsim::fpu::ensure_default_fp_env();
    let clock = Instant::now();
    loop {
        let now = clock.elapsed().as_millis() as u64;
        if clock.elapsed() > DEADLINE {
            return Err("host deadline".into());
        }
        server.receive(now);
        while let Some(event) = server.poll_event() {
            match event {
                ServerEvent::PeerJoined { peer, join, .. } => {
                    let seat = SeatId(join as u16);
                    seat_of.insert(peer, seat);
                    peer_of.insert(seat, peer);
                }
                ServerEvent::Message {
                    peer,
                    channel,
                    message,
                } => {
                    if let Some(seat) = seat_of.get(&peer) {
                        let _ = host_receive(&mut lockstep, *seat, channel, &message, now);
                    }
                }
                _ => {}
            }
        }
        if !lockstep.started() && peer_of.len() == usize::from(seats) {
            lockstep.start(now);
        }
        let mut built = false;
        while let Some(frame) = lockstep.poll_frame(now) {
            built = true;
            track.step(&frame, config.hash_every);
            lockstep.record_state(frame.tick, &track.sim);
        }
        while let Some(event) = lockstep.poll_event() {
            match event {
                HostEvent::Desync(verdict) => {
                    let json = serde_json::to_string(&verdict).map_err(|e| e.to_string())?;
                    println!("desync {json}");
                }
                HostEvent::DesyncLocated {
                    tick,
                    voter,
                    first_diverged_tick,
                } => println!("located {tick} {voter} {first_diverged_tick}"),
                HostEvent::DesyncProbed(report) => {
                    let json = serde_json::to_string(&report).map_err(|e| e.to_string())?;
                    println!("probed {json}");
                }
                _ => {}
            }
        }
        if built {
            send_host(
                &mut server,
                &mut lockstep,
                |seat| peer_of.get(&seat).copied(),
                now,
            );
        }
        if built || !lockstep.started() {
            server.flush(now);
        }
        if lockstep.tick() >= ticks {
            let status = lockstep.status(now);
            let payload = status.datagram_bytes_sent;
            let datagrams = status.datagrams_sent;
            let overhead = u64::try_from(IP_UDP_OVERHEAD).unwrap_or(28);
            let wire = payload.saturating_add(datagrams.saturating_mul(overhead));
            let elapsed = now.max(1);
            let wire_bps = wire.saturating_mul(8).saturating_mul(1_000) / elapsed;
            let verdict = if status.desyncs == 0 { "PASS" } else { "FAIL" };
            println!("verdict {verdict}");
            println!(
                "upload payload_bytes={payload} datagrams={datagrams} wire_bytes={wire} \
                 elapsed_ms={elapsed} wire_bps={wire_bps} desyncs={}",
                status.desyncs
            );
            println!("done {}", lockstep.tick());
            // Let the last frames leave before the socket closes.
            std::thread::sleep(Duration::from_millis(50));
            server.shutdown();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(feature = "desync-inject")]
fn inject_at(args: &[String]) -> Result<Option<u64>, String> {
    Ok(number(args, "--inject-desync-at"))
}

#[cfg(not(feature = "desync-inject"))]
fn inject_at(args: &[String]) -> Result<Option<u64>, String> {
    if arg(args, "--inject-desync-at").is_some() {
        return Err("--inject-desync-at needs a build with the desync-inject feature".into());
    }
    Ok(None)
}

#[cfg(feature = "desync-inject")]
fn perturb(track: &mut Track) {
    track.sim.perturb();
}

#[cfg(not(feature = "desync-inject"))]
fn perturb(_track: &mut Track) {}

fn seat(args: &[String]) -> Result<(), String> {
    let server: SocketAddr = arg(args, "--server")
        .ok_or("--server")?
        .parse()
        .map_err(|e| format!("--server: {e}"))?;
    let index = number(args, "--seat").ok_or("--seat")? as u16;
    let ticks = number(args, "--ticks").ok_or("--ticks")?;
    let inject = inject_at(args)?;
    let mix = mix_of(args)?;
    let seed = seed_of(args);
    // `--check-flags`: the flags parse in this build; nothing is connected.
    if args.iter().any(|a| a == "--check-flags") {
        return Ok(());
    }
    let socket = UdpTransport::bind(SocketAddr::new(bind_ip(args)?, 0)).map_err(|e| format!("bind: {e}"))?;
    let config = config();
    let mut client = NetClient::<LockGame, _>::connect(
        socket,
        server,
        ClientConfig {
            name: format!("seat{index}"),
            ..ClientConfig::default()
        },
        &u32::from(index),
    )
    .map_err(|e| format!("connect: {e:?}"))?;
    let mut lockstep = LockstepSeat::<LockGame>::new(config.clone(), SeatId(index));
    let mut track = Track::new();
    let mut input = InputSource::new(seed, index, mix);
    let mut connected = false;
    let mut first_sent = false;
    regia_detsim::fpu::ensure_default_fp_env();
    let clock = Instant::now();
    loop {
        let now = clock.elapsed().as_millis() as u64;
        if clock.elapsed() > DEADLINE {
            return Err(format!("seat {index} deadline"));
        }
        client.receive(now);
        while let Some(event) = client.poll_event() {
            match event {
                ClientEvent::Connected { .. } => connected = true,
                // The host closes when the match is over.
                ClientEvent::Ended(_) => return Ok(()),
                ClientEvent::Message { channel, message } => {
                    let _ = seat_receive(&mut lockstep, channel, &message, now);
                }
                _ => {}
            }
        }
        if connected {
            if !first_sent {
                first_sent = true;
                let motion = track.sim.motion();
                let _ = lockstep.submit_input(&input.next_input_with(&motion));
            }
            while let Some(frame) = lockstep.next_frame() {
                track.step(&frame, config.hash_every);
                if inject == Some(frame.tick) {
                    perturb(&mut track);
                }
                lockstep.record_state(frame.tick, &track.sim);
                let motion = track.sim.motion();
                let _ = lockstep.submit_input(&input.next_input_with(&motion));
            }
            while lockstep.poll_event().is_some() {}
            send_seat(&mut client, &mut lockstep, now);
        }
        client.flush(now);
        if lockstep.tick() >= ticks + 600 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("host") => host(&args),
        Some("seat") => seat(&args),
        _ => Err("usage: lockstep_bot host|seat ... (see the file's header)".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("lockstep_bot: {why}");
            ExitCode::from(2)
        }
    }
}
