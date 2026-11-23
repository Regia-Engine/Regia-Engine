// SPDX-License-Identifier: MPL-2.0
//! Two real `regia_net` connections over the crate's impairment model (ADR-8521, spec 120 § 5.2 and § 15.5).
//!
//! The stateful targets of ADR-8468 drive one connection with a scripted peer that never resends, so they check
//! the reassembly stall deadline only against senders that withhold. This target runs two honest
//! `Connection`s, each polling every 10 ms, through a `NetConditioner` per direction (delay, jitter, loss,
//! duplicates, reordering) plus one two-way outage, and carries reliable messages of up to 20 KB both ways. The
//! sender's real loss detection, probe timeouts, outage recovery and pacing do the resending. The model is the
//! honest-link contract: every message arrives once and in order, neither link ever fails, both queues drain.
//! An honest link that fails (`reassembly_stalled` was the one this target was written for) is a finding.
//!
//! Script layout (little endian), every field read modulo the honest envelope of spec 120 § 5.2's acceptance
//! (the envelope of `connection::unify_tests::seeded_links_deliver_in_order_and_keep_the_in_flight_accounting_exact`),
//! so a mutated value is another honest link:
//!
//! | field | type | read as |
//! |---|---|---|
//! | magic | `"NPAR"` | |
//! | seed | `u32` | conditioner seeds (`seed`, `seed + 1`) |
//! | delay | `u16` | one-way delay, `% 300` ms |
//! | jitter | `u8` | `% 80` ms |
//! | loss | `u8` | `% 30` per cent |
//! | duplicate | `u8` | `% 5` per cent |
//! | reorder | `u8` | `% 30` per cent, by up to `reorder_max` |
//! | reorder_max | `u8` | `1 + value % 150` ms |
//! | outage_at | `u16` | `2,000 + value % 8,000` ms |
//! | outage_len | `u16` | `% 6,001` ms (two ways) |
//! | count | `u8` | messages, `1..=MAX_MESSAGES` |
//! | message | `side u8, at u8, len u16` | sent by end `side % 2` at `at % 100 x 100` ms, `len % 20,001` bytes (at least 4) |
//!
//! Bytes after the last message are ignored.

use std::net::SocketAddr;

use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::connection::{Channel, Connection, LinkConfig};
use regia_net::wire::{WireReader, WireWriter};

use super::{FuzzInput, FuzzTarget};

const MAGIC: &[u8; 4] = b"NPAR";
/// Messages one script sends (both ends together).
const MAX_MESSAGES: usize = 48;
/// Virtual time a run may take before an undelivered message is a finding: the sending phase is at most 10 s,
/// an outage 6 s, and the slowest honest repair on the envelope's worst path is a few seconds.
const RUN_LIMIT_MS: u64 = 120_000;
/// The poll and delivery step of both ends.
const STEP_MS: u64 = 10;

/// The registered target.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "net.link_pair_impaired",
        format: None,
        format_version: regia_net::REGIA_NET_WIRE_VERSION,
        round_trip: false,
        input: FuzzInput::Bytes {
            fixture: link_pair_fixture,
            load: link_pair_load,
        },
    }]
}

/// One impaired link and its traffic, decoded from a script.
#[derive(Debug, Clone, PartialEq)]
struct Script {
    seed: u32,
    delay: u16,
    jitter: u8,
    loss: u8,
    duplicate: u8,
    reorder: u8,
    reorder_max: u8,
    outage_at: u16,
    outage_len: u16,
    /// (side, at, len) as written.
    messages: Vec<(u8, u8, u16)>,
}

impl Script {
    fn parse(b: &[u8]) -> Result<Self, String> {
        let mut r = WireReader::new(b);
        let e = |err: regia_net::wire::WireError| err.to_string();
        if r.take(4).map_err(e)? != MAGIC {
            return Err("not a link-pair script".into());
        }
        let mut s = Self {
            seed: r.u32().map_err(e)?,
            delay: r.u16().map_err(e)?,
            jitter: r.u8().map_err(e)?,
            loss: r.u8().map_err(e)?,
            duplicate: r.u8().map_err(e)?,
            reorder: r.u8().map_err(e)?,
            reorder_max: r.u8().map_err(e)?,
            outage_at: r.u16().map_err(e)?,
            outage_len: r.u16().map_err(e)?,
            messages: Vec::new(),
        };
        let count = usize::from(r.u8().map_err(e)?);
        if count == 0 || count > MAX_MESSAGES {
            return Err(format!("{count} messages (1..={MAX_MESSAGES})"));
        }
        for _ in 0..count {
            s.messages
                .push((r.u8().map_err(e)?, r.u8().map_err(e)?, r.u16().map_err(e)?));
        }
        // Bytes after the last message are ignored, so an inserted byte shifts fields, it does not void the script.
        Ok(s)
    }

    fn encode(&self) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.raw(MAGIC);
        w.u32(self.seed);
        w.u16(self.delay);
        w.u8(self.jitter);
        w.u8(self.loss);
        w.u8(self.duplicate);
        w.u8(self.reorder);
        w.u8(self.reorder_max);
        w.u16(self.outage_at);
        w.u16(self.outage_len);
        w.u8(self.messages.len() as u8);
        for &(side, at, len) in &self.messages {
            w.u8(side);
            w.u8(at);
            w.u16(len);
        }
        w.finish()
    }

    /// The conditioner of one direction, inside the honest envelope.
    fn link(&self, seed: u64) -> NetConditioner {
        NetConditioner::new(ConditionerConfig {
            enabled: true,
            seed,
            loss: f64::from(self.loss % 30) / 100.0,
            duplicate: f64::from(self.duplicate % 5) / 100.0,
            reorder: f64::from(self.reorder % 30) / 100.0,
            reorder_max_ms: 1 + u64::from(self.reorder_max % 150),
            delay_ms: u64::from(self.delay % 300),
            jitter_ms: u64::from(self.jitter % 80),
            ..ConditionerConfig::default()
        })
    }

    /// Messages in send order: (time, side, length).
    fn schedule(&self) -> Vec<(u64, usize, usize)> {
        let mut out: Vec<(u64, usize, usize)> = self
            .messages
            .iter()
            .map(|&(side, at, len)| {
                (
                    u64::from(at % 100) * 100,
                    usize::from(side % 2),
                    usize::from(len % 20_001).max(4),
                )
            })
            .collect();
        out.sort_by_key(|&(at, side, _)| (at, side));
        out
    }
}

/// A harsh link inside the envelope: 200 ms each way with 60 ms jitter, 20 % loss, 2 % duplicates, 15 %
/// reordered by up to 60 ms, a 6 s two-way outage from 3 s, and 20 KB, 3 KB and small messages both ways. On the
/// deadline of ADR-7981 alone this link stalls out (`reassembly stalled`; run on 151061fcb by lane
/// net-reassembly-stall-fix).
fn link_pair_fixture() -> Vec<u8> {
    let mut messages = Vec::new();
    for i in 0..40u8 {
        let len = match i % 5 {
            0 => 20_000,
            1 => 3_000,
            _ => 60,
        };
        messages.push((i % 2, i * 2, len));
    }
    Script {
        seed: 5,
        delay: 200,
        jitter: 60,
        loss: 20,
        duplicate: 2,
        reorder: 15,
        reorder_max: 59,
        outage_at: 1_000,
        outage_len: 6_000,
        messages,
    }
    .encode()
}

/// Runs the script; a broken honest-link contract panics (a finding), a malformed script is an error.
fn link_pair_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let script = Script::parse(b)?;
    let schedule = script.schedule();
    let to = SocketAddr::from(([10, 0, 0, 1], 1));
    let mut wires = [
        script.link(u64::from(script.seed)),
        script.link(u64::from(script.seed) + 1),
    ];
    let mut ends = [
        Connection::new(LinkConfig::default()),
        Connection::new(LinkConfig::default()),
    ];
    let outage_from = 2_000 + u64::from(script.outage_at % 8_000);
    let outage_to = outage_from + u64::from(script.outage_len % 6_001);
    let (mut sent, mut got) = ([0u32; 2], [0u32; 2]);
    let mut next = 0;
    let mut now = 0u64;
    loop {
        while let Some(&(at, side, len)) = schedule.get(next)
            && at <= now
        {
            let mut m = vec![side as u8; len];
            m[..4].copy_from_slice(&sent[side].to_le_bytes());
            // A full queue is backpressure, not a contract breach: the message is offered again next step.
            if ends[side].send(Channel::GAME_RPC, m, 0).is_err() {
                break;
            }
            sent[side] += 1;
            next += 1;
        }
        let out = now >= outage_from && now < outage_to;
        for side in 0..2 {
            let polled = ends[side].poll(now, &[]);
            let mut arrived = Vec::new();
            for p in polled {
                if !out {
                    arrived.extend(wires[side].process(p, to, now));
                }
            }
            arrived.extend(wires[side].release_due(now));
            let peer = &mut ends[1 - side];
            for (p, _) in arrived {
                if let Err(err) = peer.receive(&p, now) {
                    // invariant: a model finding, not input reaching a panic: the fuzz harness catches it and reports the seed.
                    panic!(
                        "at {now} ms an honest link failed ({err}, {:?}): stall {} ms of a {} ms deadline, factor {}",
                        peer.failure(),
                        peer.stats().reassembly_stall_ms,
                        peer.stats().reassembly_stall_deadline_ms,
                        peer.reassembly_stall_factor()
                    );
                }
            }
            while let Some((channel, m)) = peer.next_message() {
                assert!(
                    channel == Channel::GAME_RPC,
                    "at {now} ms an unreliable message appeared"
                );
                let n = u32::from_le_bytes([m[0], m[1], m[2], m[3]]);
                assert!(
                    n == got[1 - side],
                    "at {now} ms end {} got message {n}, expected {} (once, in order)",
                    1 - side,
                    got[1 - side]
                );
                got[1 - side] += 1;
            }
        }
        if next == schedule.len()
            && got[0] == sent[1]
            && got[1] == sent[0]
            && ends.iter().all(|c| !c.has_unacked_reliable())
        {
            break;
        }
        assert!(
            now < RUN_LIMIT_MS,
            "after {RUN_LIMIT_MS} ms of virtual time {}/{} and {}/{} messages arrived (outage {outage_from}..{outage_to})",
            got[1],
            sent[0],
            got[0],
            sent[1]
        );
        now += STEP_MS;
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_round_trips_and_the_fixture_holds_the_contract() {
        let fixture = link_pair_fixture();
        let script = Script::parse(&fixture).expect("fixture parses");
        assert_eq!(script.encode(), fixture);
        assert_eq!(script.messages.len(), 40);
        link_pair_load(&fixture).expect("an honest impaired link delivers everything");
    }

    /// 96 honest links around the fixture (24 seeds on four paths from 150 ms with 28 % loss to 280 ms with 75 ms
    /// jitter): 27 of them stalled out on the ADR-7981 deadline alone; none may now.
    #[test]
    fn the_honest_envelope_holds_on_every_seeded_path() {
        for seed in 1..=24u32 {
            for (delay, loss, jitter) in [(200u16, 20u8, 60u8), (250, 25, 70), (150, 28, 40), (280, 15, 75)] {
                let mut s = Script::parse(&link_pair_fixture()).expect("fixture parses");
                s.seed = seed;
                s.delay = delay;
                s.loss = loss;
                s.jitter = jitter;
                link_pair_load(&s.encode()).expect("an honest link delivers everything");
            }
        }
    }

    #[test]
    fn a_truncated_or_foreign_script_is_rejected() {
        let fixture = link_pair_fixture();
        assert!(link_pair_load(&fixture[..10]).is_err());
        assert!(link_pair_load(b"NSEQ\x01\x00").is_err());
        let mut none = fixture[..19].to_vec();
        none.push(0);
        assert!(Script::parse(&none).is_err(), "zero messages");
    }
}
