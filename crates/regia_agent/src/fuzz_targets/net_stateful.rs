// SPDX-License-Identifier: MPL-2.0
//! Stateful, structure-aware fuzz targets for `regia_net` (spec 120 § 15.5, audit row H6, ADR-8468).
//!
//! The older net targets feed one datagram (or one body twice) to a fresh endpoint, so the state the last DO27
//! blockers lived in (fragments stored ahead of a gap, a message that never ends, a session that moves address) is
//! never reached. These targets read an **op script**: a short program of fragments, honest messages cut into
//! fragments and put in any order, unreliable records, duplicates, idle keepalive runs and, against a server,
//! address changes, replays and raw hostile datagrams. A byte mutation of a script is still a meaningful
//! sequence, so the mutation loop explores sequences instead of mostly breaking a header.
//!
//! Every target checks its run against a model written from the spec, not from the code:
//!
//! - [`Shadow`] replays the reliable stream of `regia_net::connection` (in-order delivery of whole messages,
//!   a window of `RELIABLE_WINDOW` ids) and the bounds of spec 120 § 5.2: a fragment longer than one packet
//!   carries fails the link, a message over `max_reliable_message_bytes` fails it, and a gap the peer has not
//!   filled for `max(4 x RTO, 2 s)` of its own traffic fails it. The model never pins the exact deadline: it
//!   requires a failure only once the gap has stood for 4 s of traffic counted at the 100 ms minimum RTO (past
//!   every legal deadline), and it calls a failure wrong only while the same traffic counted at the 1 s maximum RTO
//!   is under 2 s (before every legal deadline).
//!   The scripts number their packets in sequence (no packet number is ever missing), so the loss stretch of the
//!   deadline (ADR-8521, at most 4x on a peer that loses packets) never applies and the band stays exact.
//! - The shared-port target routes each datagram with a model of the routing rules in the module docs of
//!   `regia_net::server_host::shared_port`.
//!
//! The checks use only API that existed before the reassembly caps (ADR-7981), so each target runs unchanged on
//! the code before them and finds the reassembly bomb there (the report of lane net-fuzz-coverage records it).
//!
//! Script layout (little endian): `"NSEQ"`, `u16` op count, then the ops. An op is a tag byte (read modulo the 11
//! ops, so a mutated tag is another op) and its fields; bytes after the last op are ignored:
//!
//! | tag | op | fields |
//! |---|---|---|
//! | 1 | `Frag` | `id u16, more u8, len u16, fill u8`: one reliable record, as given |
//! | 2 | `Msg` | `len u16, fill u8`: an honest message, cut into fragments under the next ids, waiting to be put |
//! | 3 | `Put` | `index u8`: puts waiting fragment `index % waiting` in the packet |
//! | 4 | `Unreliable` | `len u8, fill u8` |
//! | 5 | `Send` | `dt u8`: the packet leaves `dt` ms after the previous one |
//! | 6 | `Dup` | `dt u8`: the previous packet arrives again |
//! | 7 | `Idle` | `count u8, dt u8`: `count` keepalive (ping) packets, `dt` ms apart |
//! | 8 | `Rebind` | `port u8`: later datagrams come from another port of the client's address (endpoint) |
//! | 9 | `Raw` | `len u16, fill u8`: a datagram that is not a sealed record of the session (endpoint) |
//! | 10 | `Replay` | `back u8`: a sealed datagram sent before arrives again (endpoint) |
//! | 11 | `Answer` | `dt u8`: a keepalive that acknowledges every packet the server sent the client (against a server,
//! from the current port, so it answers a path-validation probe sent there; spec 120 § 10, ADR-8299) |

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::net::{Ipv4Addr, SocketAddr};

use regia_net::connection::{Channel, Connection, RELIABLE_WINDOW};
use regia_net::wire::{WireReader, WireWriter};
use regia_net::{LinkConfig, NetCodec};

use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

const fn bytes(
    name: &'static str,
    fixture: fn() -> Vec<u8>,
    load: fn(&[u8]) -> Result<Vec<u8>, String>,
) -> FuzzTarget {
    FuzzTarget {
        name,
        format: None,
        format_version: regia_net::REGIA_NET_WIRE_VERSION,
        round_trip: false,
        input: FuzzInput::Bytes { fixture, load },
    }
}

/// The registered stateful targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        bytes(
            "net.connection_sequence",
            connection_sequence_fixture,
            connection_sequence_load,
        ),
        bytes(
            "net.connection_sequence.hostile",
            connection_hostile_fixture,
            connection_sequence_load,
        ),
        bytes(
            "net.sealed_endpoint_sequence",
            sealed_sequence_fixture,
            sealed_sequence_load,
        ),
        bytes(
            "net.sealed_endpoint_sequence.hostile",
            sealed_hostile_fixture,
            sealed_sequence_load,
        ),
        bytes("net.shared_port_sequence", shared_port_fixture, shared_port_load),
        bytes("net.voice_relay_route", voice_fixture, voice_load),
        bytes("net.snapshot_sequence", snapshot_fixture, snapshot_load),
        FuzzTarget {
            name: "net.input_bundle_budget",
            format: None,
            format_version: regia_net::REGIA_NET_WIRE_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: input_bundle_budget_fixture,
                load: input_bundle_budget_load,
            },
        },
    ]
}

// ---- The op script --------------------------------------------------------------------------------------------

const MAGIC: &[u8; 4] = b"NSEQ";
/// Ops one script holds at most.
const MAX_OPS: usize = 1024;
/// Packets one script may make (idle runs included).
const MAX_PACKETS: usize = 8192;
/// Record bytes one script may make.
const MAX_SCRIPT_BYTES: usize = 4 * 1024 * 1024;
/// Records one packet holds at most (the connection refuses more than 128 as malformed).
const MAX_RECORDS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Frag { id: u16, more: bool, len: u16, fill: u8 },
    Msg { len: u16, fill: u8 },
    Put { index: u8 },
    Unreliable { len: u8, fill: u8 },
    Send { dt: u8 },
    Dup { dt: u8 },
    Idle { count: u8, dt: u8 },
    Rebind { port: u8 },
    Raw { len: u16, fill: u8 },
    Replay { back: u8 },
    Answer { dt: u8 },
}

fn parse_ops(b: &[u8]) -> Result<Vec<Op>, String> {
    let mut r = WireReader::new(b);
    if r.take(4).map_err(e)? != MAGIC {
        return Err("not an op script".into());
    }
    let count = usize::from(r.u16().map_err(e)?);
    if count == 0 || count > MAX_OPS {
        return Err(format!("{count} ops (1..={MAX_OPS})"));
    }
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        // Any tag byte is an op (read modulo the 11 ops), so a mutated tag is still a script.
        let op = match r.u8().map_err(e)?.wrapping_sub(1) % 11 + 1 {
            1 => Op::Frag {
                id: r.u16().map_err(e)?,
                more: r.u8().map_err(e)? != 0,
                len: r.u16().map_err(e)?,
                fill: r.u8().map_err(e)?,
            },
            2 => Op::Msg {
                len: r.u16().map_err(e)?,
                fill: r.u8().map_err(e)?,
            },
            3 => Op::Put {
                index: r.u8().map_err(e)?,
            },
            4 => Op::Unreliable {
                len: r.u8().map_err(e)?,
                fill: r.u8().map_err(e)?,
            },
            5 => Op::Send {
                dt: r.u8().map_err(e)?,
            },
            6 => Op::Dup {
                dt: r.u8().map_err(e)?,
            },
            7 => Op::Idle {
                count: r.u8().map_err(e)?,
                dt: r.u8().map_err(e)?,
            },
            8 => Op::Rebind {
                port: r.u8().map_err(e)?,
            },
            9 => Op::Raw {
                len: r.u16().map_err(e)?,
                fill: r.u8().map_err(e)?,
            },
            10 => Op::Replay {
                back: r.u8().map_err(e)?,
            },
            11 => Op::Answer {
                dt: r.u8().map_err(e)?,
            },
            _ => Op::Answer {
                dt: r.u8().map_err(e)?,
            },
        };
        ops.push(op);
    }
    // Bytes after the last op are ignored: an inserted byte shifts the ops, it does not void the script.
    Ok(ops)
}

fn encode_ops(ops: &[Op]) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.raw(MAGIC);
    w.u16(u16::try_from(ops.len()).unwrap_or(u16::MAX));
    for op in ops {
        match *op {
            Op::Frag { id, more, len, fill } => {
                w.u8(1);
                w.u16(id);
                w.u8(u8::from(more));
                w.u16(len);
                w.u8(fill);
            }
            Op::Msg { len, fill } => {
                w.u8(2);
                w.u16(len);
                w.u8(fill);
            }
            Op::Put { index } => {
                w.u8(3);
                w.u8(index);
            }
            Op::Unreliable { len, fill } => {
                w.u8(4);
                w.u8(len);
                w.u8(fill);
            }
            Op::Send { dt } => {
                w.u8(5);
                w.u8(dt);
            }
            Op::Dup { dt } => {
                w.u8(6);
                w.u8(dt);
            }
            Op::Idle { count, dt } => {
                w.u8(7);
                w.u8(count);
                w.u8(dt);
            }
            Op::Rebind { port } => {
                w.u8(8);
                w.u8(port);
            }
            Op::Raw { len, fill } => {
                w.u8(9);
                w.u16(len);
                w.u8(fill);
            }
            Op::Replay { back } => {
                w.u8(10);
                w.u8(back);
            }
            Op::Answer { dt } => {
                w.u8(11);
                w.u8(dt);
            }
        }
    }
    w.finish()
}

/// One record of a connection packet.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rec {
    Reliable { id: u16, more: bool, bytes: Vec<u8> },
    Unreliable(Vec<u8>),
    Ping,
}

/// What a script makes, in order.
#[derive(Debug, Clone)]
enum Step {
    /// A fresh packet (the next packet number) arriving at `at` ms.
    Packet { at: u64, records: Vec<Rec> },
    /// The previous packet again.
    Again { at: u64 },
    /// Later datagrams come from another port.
    Rebind { port: u8 },
    /// A datagram that is not a record of the session.
    Raw { at: u64, len: u16, fill: u8 },
    /// An earlier sealed datagram again.
    Replay { at: u64, back: u8 },
    /// A keepalive (the next packet number) that acknowledges what the client received.
    Answer { at: u64 },
}

/// The bytes of a fragment op or an unreliable record: a pattern that differs per byte and per `fill`.
fn pattern(len: usize, fill: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(fill))
        .collect()
}

/// Expands `ops` into steps. `message` makes an honest message of about `len` bytes and `cut` is the fragment size
/// of honest messages (what a real sender of that link uses).
fn expand(ops: &[Op], message: fn(usize, u8) -> Vec<u8>, cut: usize) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    let mut now = 0u64;
    let mut records: Vec<Rec> = Vec::new();
    let mut waiting: Vec<Rec> = Vec::new();
    let mut next_id = 0u16;
    let mut packets = 0usize;
    let mut made = 0usize;
    let count = |made: &mut usize, n: usize| -> Result<(), String> {
        *made += n;
        if *made > MAX_SCRIPT_BYTES {
            Err(format!("the script makes more than {MAX_SCRIPT_BYTES} B"))
        } else {
            Ok(())
        }
    };
    let push = |records: &mut Vec<Rec>, rec: Rec| -> Result<(), String> {
        if records.len() >= MAX_RECORDS {
            return Err(format!("more than {MAX_RECORDS} records in one packet"));
        }
        records.push(rec);
        Ok(())
    };
    for op in ops {
        if packets > MAX_PACKETS {
            return Err(format!("more than {MAX_PACKETS} packets"));
        }
        match *op {
            Op::Frag { id, more, len, fill } => {
                count(&mut made, usize::from(len))?;
                push(
                    &mut records,
                    Rec::Reliable {
                        id,
                        more,
                        bytes: pattern(usize::from(len), fill),
                    },
                )?;
            }
            Op::Msg { len, fill } => {
                let body = message(usize::from(len), fill);
                count(&mut made, body.len())?;
                let pieces: Vec<&[u8]> = body.chunks(cut.max(1)).collect();
                let last = pieces.len().saturating_sub(1);
                for (i, piece) in pieces.iter().enumerate() {
                    waiting.push(Rec::Reliable {
                        id: next_id,
                        more: i < last,
                        bytes: piece.to_vec(),
                    });
                    next_id = next_id.wrapping_add(1);
                }
            }
            Op::Put { index } => {
                if !waiting.is_empty() {
                    let rec = waiting.remove(usize::from(index) % waiting.len());
                    push(&mut records, rec)?;
                }
            }
            Op::Unreliable { len, fill } => {
                count(&mut made, usize::from(len))?;
                push(&mut records, Rec::Unreliable(pattern(usize::from(len), fill)))?;
            }
            Op::Send { dt } => {
                now += u64::from(dt);
                packets += 1;
                steps.push(Step::Packet {
                    at: now,
                    records: std::mem::take(&mut records),
                });
            }
            Op::Dup { dt } => {
                now += u64::from(dt);
                packets += 1;
                steps.push(Step::Again { at: now });
            }
            Op::Idle { count, dt } => {
                for _ in 0..count {
                    now += u64::from(dt);
                    packets += 1;
                    steps.push(Step::Packet {
                        at: now,
                        records: vec![Rec::Ping],
                    });
                }
            }
            Op::Rebind { port } => steps.push(Step::Rebind { port }),
            Op::Raw { len, fill } => {
                count(&mut made, usize::from(len))?;
                packets += 1;
                steps.push(Step::Raw { at: now, len, fill });
            }
            Op::Replay { back } => {
                packets += 1;
                steps.push(Step::Replay { at: now, back });
            }
            Op::Answer { dt } => {
                now += u64::from(dt);
                packets += 1;
                steps.push(Step::Answer { at: now });
            }
        }
    }
    if !records.is_empty() {
        now += 1;
        steps.push(Step::Packet { at: now, records });
    }
    Ok(steps)
}

/// The connection packet body `regia_net::connection` documents: flags (a four-byte packet number, no
/// acknowledgment frame), the packet number, then the records (`0x01 len u16 bytes`, `0x02`/`0x82 id u16 len u16
/// bytes`, `0x03`).
fn packet_body(pn: u64, records: &[Rec]) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.u8(2);
    w.u32(pn as u32);
    for rec in records {
        match rec {
            Rec::Reliable { id, more, bytes } => {
                w.u8(if *more { 0x82 } else { 0x02 });
                w.u16(*id);
                w.u16(bytes.len() as u16);
                w.raw(bytes);
            }
            Rec::Unreliable(bytes) => {
                w.u8(0x01);
                w.u16(bytes.len() as u16);
                w.raw(bytes);
            }
            Rec::Ping => w.u8(0x03),
        }
    }
    w.finish()
}

// ---- The reassembly model (spec 120 § 5.2) --------------------------------------------------------------------

/// The smallest retransmission timeout of a default link (`LinkConfig::resend_min_ms`).
const RTO_MIN_MS: u64 = 100;
/// The largest retransmission timeout of a default link (`LinkConfig::resend_max_ms`).
const RTO_MAX_MS: u64 = 1_000;
/// Spec 120 § 5.2: a gap is a stall after `max(4 x RTO, 2 s)` of the peer's traffic, each gap between two packets
/// counting at most one RTO. With the RTO between [`RTO_MIN_MS`] and [`RTO_MAX_MS`] the deadline is 2 to 4 s.
const STALL_FLOOR_MS: u64 = 2_000;
/// See [`STALL_FLOOR_MS`].
const STALL_CEILING_MS: u64 = 4 * RTO_MAX_MS;

/// What the model expects of one packet.
#[derive(Debug)]
enum Expect {
    /// Accepted, delivering these messages in order.
    Deliver(Vec<(Channel, Vec<u8>)>),
    /// Accepted delivering these, or a stall failure: the gap may be at a legal deadline.
    MayStall(Vec<(Channel, Vec<u8>)>),
    /// The link must fail on this packet (or have failed before it), for this reason.
    MustFail(String),
    /// The link failed before: every packet is an error.
    Failed,
}

/// The receive side of one reliable stream, as spec 120 § 5.2 and the connection's wire docs define it.
#[derive(Debug)]
struct Shadow {
    max_fragment: usize,
    max_message: usize,
    next_expected: u16,
    stored: HashMap<u16, (Vec<u8>, bool)>,
    stored_bytes: usize,
    assembling: Vec<u8>,
    failed: bool,
    last_rx: Option<u64>,
    /// Peer traffic since the last progress while something waited, each gap counted at most [`RTO_MIN_MS`].
    stall_low: u64,
    /// The same counted at most [`RTO_MAX_MS`] per gap.
    stall_high: u64,
}

impl Shadow {
    fn new(config: &LinkConfig) -> Self {
        Self {
            // The packet less its 9-byte header and the 5-byte reliable record header.
            max_fragment: config.max_packet_bytes.saturating_sub(9 + 5),
            max_message: config.max_reliable_message_bytes,
            next_expected: 0,
            stored: HashMap::new(),
            stored_bytes: 0,
            assembling: Vec::new(),
            failed: false,
            last_rx: None,
            stall_low: 0,
            stall_high: 0,
        }
    }

    fn waiting(&self) -> bool {
        !self.stored.is_empty() || !self.assembling.is_empty()
    }

    /// Bytes waiting (stored ahead of a gap plus the message in assembly).
    fn waiting_bytes(&self) -> usize {
        self.stored_bytes + self.assembling.len()
    }

    /// One packet at `at`; `fresh` is false for a duplicate (its records are ignored, its time still counts).
    fn apply(&mut self, at: u64, records: &[Rec], fresh: bool) -> Expect {
        if self.failed {
            return Expect::Failed;
        }
        if let Some(len) = records.iter().find_map(|r| match r {
            Rec::Reliable { bytes, .. } if bytes.len() > self.max_fragment => Some(bytes.len()),
            _ => None,
        }) {
            self.failed = true;
            return Expect::MustFail(format!(
                "a reliable fragment of {len} B, longer than one packet carries ({} B), was accepted",
                self.max_fragment
            ));
        }
        let interval = self.last_rx.map_or(0, |last| at.saturating_sub(last));
        self.last_rx = Some(at);
        let before = self.next_expected;
        let mut out = Vec::new();
        if fresh {
            for rec in records {
                match rec {
                    Rec::Ping => {}
                    Rec::Unreliable(bytes) => out.push((Channel::Unreliable, bytes.clone())),
                    Rec::Reliable { id, more, bytes } => {
                        let ahead = usize::from(id.wrapping_sub(self.next_expected));
                        if ahead >= RELIABLE_WINDOW {
                            continue;
                        }
                        if !self.stored.contains_key(id) {
                            self.stored_bytes += bytes.len();
                            self.stored.insert(*id, (bytes.clone(), *more));
                        }
                        while let Some((frag, more)) = self.stored.remove(&self.next_expected) {
                            self.stored_bytes -= frag.len();
                            self.next_expected = self.next_expected.wrapping_add(1);
                            if self.assembling.len() + frag.len() > self.max_message {
                                self.failed = true;
                                return Expect::MustFail(format!(
                                    "a reliable message grew to {} B, over the {} B cap, and was accepted",
                                    self.assembling.len() + frag.len(),
                                    self.max_message
                                ));
                            }
                            self.assembling.extend_from_slice(&frag);
                            if !more {
                                out.push((Channel::GAME_RPC, std::mem::take(&mut self.assembling)));
                            }
                        }
                    }
                }
            }
        }
        if self.next_expected != before || !self.waiting() {
            self.stall_low = 0;
            self.stall_high = 0;
            return Expect::Deliver(out);
        }
        self.stall_low += interval.min(RTO_MIN_MS);
        self.stall_high += interval.min(RTO_MAX_MS);
        if self.stall_low >= STALL_CEILING_MS {
            self.failed = true;
            return Expect::MustFail(format!(
                "a reassembly gap stood for {} ms of the peer's traffic with {} B waiting and the link never failed \
                 (spec 120 § 5.2: max(4 x RTO, 2 s))",
                self.stall_low,
                self.waiting_bytes()
            ));
        }
        if self.stall_high >= STALL_FLOOR_MS {
            Expect::MayStall(out)
        } else {
            Expect::Deliver(out)
        }
    }
}

// ---- net.connection_sequence ------------------------------------------------------------------------------------

/// An honest message of a connection script: just its pattern.
fn plain_message(len: usize, fill: u8) -> Vec<u8> {
    pattern(len, fill)
}

fn link_config() -> LinkConfig {
    LinkConfig::default()
}

/// The fragment size of an honest sender of [`link_config`] with no endpoint header.
fn link_cut() -> usize {
    link_config().max_packet_bytes - 9 - 5
}

/// Seeded from the soak harness's `hostile` profile (good-wifi impairment plus a hostile peer's replays): two
/// messages of four and three fragments sent out of order across packets (the second message's fragments first),
/// one packet duplicated, an unreliable record per packet, then 5 s of keepalives at 100 ms. Every byte arrives,
/// so the model delivers both messages whole; one mutation (a lost `Put`, a flipped `more`, a long `Frag`) leaves
/// a gap or a bomb that the keepalives run past the stall deadline.
fn connection_sequence_fixture() -> Vec<u8> {
    let ops = [
        Op::Msg { len: 4_000, fill: 7 },
        Op::Msg { len: 3_000, fill: 9 },
        // Waiting: ids 0..=3 (message one), 4..=6 (message two).
        Op::Put { index: 6 },
        Op::Put { index: 5 },
        Op::Unreliable { len: 12, fill: 1 },
        Op::Send { dt: 20 },
        Op::Dup { dt: 3 },
        Op::Put { index: 4 },
        Op::Put { index: 3 },
        Op::Send { dt: 25 },
        Op::Put { index: 1 },
        Op::Unreliable { len: 40, fill: 2 },
        Op::Send { dt: 30 },
        Op::Put { index: 0 },
        Op::Put { index: 0 },
        Op::Send { dt: 15 },
        Op::Idle { count: 50, dt: 100 },
        Op::Frag {
            id: 7,
            more: false,
            len: 300,
            fill: 3,
        },
        Op::Send { dt: 10 },
        Op::Idle { count: 10, dt: 100 },
    ];
    encode_ops(&ops)
}

/// The reassembly bomb of the audit's experiment E6, in size: fragments 1..=200 (never id 0, the next expected
/// one) of a full packet each, then 5 s of keepalives. Before spec 120 § 5.2 the connection held them for the
/// whole session; the model requires the link to fail once the gap has stood past every legal deadline.
fn connection_hostile_fixture() -> Vec<u8> {
    let mut ops = Vec::new();
    for id in 1..=200u16 {
        ops.push(Op::Frag {
            id,
            more: true,
            len: link_cut() as u16,
            fill: id as u8,
        });
        ops.push(Op::Send { dt: 1 });
    }
    ops.push(Op::Idle { count: 50, dt: 100 });
    encode_ops(&ops)
}

/// Every step against one fresh default connection, checked against [`Shadow`] after every packet: the messages
/// it delivers, the failures spec 120 § 5.2 requires and the ones it forbids. Endpoint-only ops are ignored here.
fn connection_sequence_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let ops = parse_ops(b)?;
    let steps = expand(&ops, plain_message, link_cut())?;
    let config = link_config();
    let mut c = Connection::new(config.clone());
    let mut shadow = Shadow::new(c.config());
    let mut pn = 0u64;
    let mut last: Option<(Vec<u8>, Vec<Rec>)> = None;
    for step in steps {
        let (at, body, records, fresh) = match step {
            Step::Packet { at, records } => {
                let body = packet_body(pn, &records);
                pn += 1;
                last = Some((body.clone(), records.clone()));
                (at, body, records, true)
            }
            // Nothing was sent to acknowledge: a keepalive.
            Step::Answer { at } => {
                let records = vec![Rec::Ping];
                let body = packet_body(pn, &records);
                pn += 1;
                last = Some((body.clone(), records.clone()));
                (at, body, records, true)
            }
            Step::Again { at } => match &last {
                Some((body, records)) => (at, body.clone(), records.clone(), false),
                None => continue,
            },
            Step::Rebind { .. } | Step::Raw { .. } | Step::Replay { .. } => continue,
        };
        let expect = shadow.apply(at, &records, fresh);
        let result = c.receive(&body, at);
        check_link(&expect, &result, &mut c, &mut shadow, at)?;
    }
    Ok(Vec::new())
}

/// Compares one receive with the model's expectation; a contradiction is a finding (a panic).
fn check_link(
    expect: &Expect,
    result: &Result<(), regia_net::wire::WireError>,
    c: &mut Connection,
    shadow: &mut Shadow,
    at: u64,
) -> Result<(), String> {
    match (expect, result) {
        (Expect::Deliver(want) | Expect::MayStall(want), Ok(())) => {
            let mut got = Vec::new();
            while let Some(m) = c.next_message() {
                got.push(m);
            }
            assert!(
                got == *want,
                "at {at} ms the link delivered {} message(s) of {:?} B, the reliable stream model {} of {:?} B",
                got.len(),
                got.iter().map(|(_, m)| m.len()).collect::<Vec<_>>(),
                want.len(),
                want.iter().map(|(_, m)| m.len()).collect::<Vec<_>>()
            );
        }
        (Expect::MayStall(_), Err(_)) => shadow.failed = true,
        (Expect::Deliver(_), Err(err)) => {
            // invariant: a model finding, not input reaching a panic: the fuzz harness catches it and reports the seed.
            panic!(
                "at {at} ms an honest packet failed the link ({err}) with {} B waiting for {} ms of traffic \
                 (under every legal stall deadline)",
                shadow.waiting_bytes(),
                shadow.stall_high
            );
        }
        // invariant: a model finding, not input reaching a panic: the fuzz harness catches it and reports the seed.
        (Expect::MustFail(why), Ok(())) => panic!("at {at} ms {why}"),
        // invariant: a model finding, not input reaching a panic: the fuzz harness catches it and reports the seed.
        (Expect::Failed, Ok(())) => panic!("at {at} ms a failed link accepted a packet"),
        (Expect::MustFail(_) | Expect::Failed, Err(_)) => {}
    }
    Ok(())
}

// ---- net.sealed_endpoint_sequence -------------------------------------------------------------------------------

type SealedServer =
    regia_net::NetServer<regia_net::session::net_session::SessionWire, regia_net::LoopbackSocket>;

/// The fixed secret of the fuzzed server, so its cookies repeat.
const SERVER_SECRET: [u8; 32] = [0x43; 32];
/// The fixed client nonce.
const CLIENT_NONCE: u64 = 0x5EA1_ED00;
/// Out-of-order bytes past which the server's shared budget (spec 120 § 5.2) may refuse packets the model applied;
/// the model then stops requiring a drop (it would no longer know what the server stored).
const BUDGET_SAFE_BYTES: usize = 256 * 1024;

fn server_addr() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::new(203, 0, 113, 9), 7777))
}

fn client_addr(port: u8) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::new(198, 51, 100, 7), 40_000 + u16::from(port)))
}

/// An honest session message: the game lane byte, then a `SessionWire` message (a byte vector) of about `len` B.
fn lane_message(len: usize, fill: u8) -> Vec<u8> {
    let mut out = vec![0u8];
    out.extend(
        pattern(len.saturating_sub(5), fill)
            .to_bytes()
            .unwrap_or_default(),
    );
    out
}

/// The fragment size of the real client on a sealed link of the default packet size.
fn sealed_cut() -> usize {
    link_config().max_packet_bytes - regia_net::crypto::SEALED_RESERVED_HEADER_BYTES - 9 - 5
}

/// A session: the server, the client's socket and its sealed link, and the peer the server admitted.
struct Sealed {
    net: regia_net::LoopbackNet,
    server: SealedServer,
    client: regia_net::LoopbackSocket,
    link: regia_net::crypto::SecureLink,
    peer: regia_net::PeerId,
}

/// Sends `datagram` from `client` and returns the server's first answer.
fn ask(
    server: &mut SealedServer,
    client: &mut regia_net::LoopbackSocket,
    datagram: &[u8],
    now: u64,
) -> Result<Vec<u8>, String> {
    use regia_net::DatagramSocket;
    client.send_to(datagram, server_addr()).map_err(e)?;
    server.receive(now);
    server.flush(now);
    let mut buf = vec![0u8; 4096];
    loop {
        match client.recv_from(&mut buf).map_err(e)? {
            Some((n, _)) if n >= 5 && buf[4] != regia_net::handshake::PacketKind::Payload as u8 => {
                buf.truncate(n);
                return Ok(buf);
            }
            Some(_) => {}
            None => return Err("the server did not answer".into()),
        }
    }
}

/// A real sealed handshake (Noise NX, `CryptoMode::Required`) against a fixed-secret server: connect request,
/// challenge, key init, key reply, the sealed join, the sealed accept.
fn sealed_session() -> Result<Sealed, String> {
    use regia_net::crypto::{ClientHandshake, KeyInit, KeyReply, SealedHeader, SealedKind};
    use regia_net::handshake::{self, Accept, Challenge, ConnectRequest, JoinToken, PacketKind};
    let net = regia_net::LoopbackNet::new();
    let socket = net.bind(server_addr()).map_err(e)?;
    let config = regia_net::ServerConfig {
        crypto: regia_net::CryptoMode::Required,
        max_handshakes_per_s: 0,
        handshakes_per_ip_per_s: 0,
        ..regia_net::ServerConfig::default()
    };
    let mut server = SealedServer::with_secret(socket, config, SERVER_SECRET);
    let mut client = net.bind(client_addr(0)).map_err(e)?;
    let protocol_id = regia_net::protocol_id::<regia_net::session::net_session::SessionWire>();
    let request = ConnectRequest {
        protocol_id,
        client_nonce: CLIENT_NONCE,
        crypto_version: regia_net::crypto::CRYPTO_VERSION,
        crypto_required: true,
    }
    .to_datagram();
    let reply = ask(&mut server, &mut client, &request, 1)?;
    let body = match handshake::split_datagram(&reply) {
        Some((PacketKind::Challenge, body)) => body,
        other => return Err(format!("expected a challenge, got {:?}", other.map(|(k, _)| k))),
    };
    let challenge = Challenge::decode(body).map_err(e)?;
    let prologue = regia_net::crypto::prologue(
        protocol_id,
        CLIENT_NONCE,
        challenge.crypto_version,
        &challenge.cookie,
        challenge.password.as_ref(),
    );
    let mut hs = ClientHandshake::start(&prologue, None).map_err(e)?;
    let init = KeyInit {
        client_nonce: CLIENT_NONCE,
        cookie: challenge.cookie,
        version: challenge.crypto_version,
        noise: *hs.message(),
    }
    .to_datagram();
    let reply = ask(&mut server, &mut client, &init, 2)?;
    let key_reply = match handshake::split_datagram(&reply) {
        Some((PacketKind::KeyReply, body)) => KeyReply::decode(body).map_err(e)?,
        other => return Err(format!("expected a key reply, got {:?}", other.map(|(k, _)| k))),
    };
    hs.read_reply(&key_reply.noise).map_err(e)?;
    let mut link = hs.into_link().map_err(e)?;
    let token = JoinToken {
        name: "sequence".into(),
        role: regia_net::PeerRole::Player,
        build: String::new(),
        password_proof: [0; 32],
        join_data: ().to_bytes().map_err(e)?,
        resume: None,
    };
    let mut w = WireWriter::new();
    token.encode(&mut w).map_err(e)?;
    let join = link.seal(SealedKind::Join, &w.finish()).map_err(e)?;
    let reply = ask(&mut server, &mut client, &join, 3)?;
    let header = match handshake::split_datagram(&reply) {
        Some((PacketKind::Sealed, body)) => SealedHeader::parse(body).ok_or("short sealed accept")?,
        other => {
            return Err(format!(
                "expected a sealed accept, got {:?}",
                other.map(|(k, _)| k)
            ));
        }
    };
    let (kind, plain) = link.open(header).map_err(e)?;
    if kind != SealedKind::Accept {
        return Err(format!("expected an accept, got {kind:?}"));
    }
    let accept = Accept::decode(&plain).map_err(e)?;
    while server.poll_event().is_some() {}
    Ok(Sealed {
        net,
        server,
        client,
        link,
        peer: regia_net::PeerId(u32::from(accept.peer_id)),
    })
}

/// Seeded from the soak harness's `hostile` profile: honest out-of-order messages, a replayed record, a garbage
/// datagram, an oversized datagram (64 KiB) and a forged record under the session's token, keepalives past the
/// stall deadline, then the session moves to another port (a NAT rebind), answers the server's probe there and keeps
/// sending.
fn sealed_sequence_fixture() -> Vec<u8> {
    let ops = [
        Op::Msg { len: 2_500, fill: 4 },
        Op::Put { index: 2 },
        Op::Put { index: 1 },
        Op::Send { dt: 10 },
        Op::Replay { back: 0 },
        Op::Raw { len: 300, fill: 0xAA },
        Op::Put { index: 0 },
        Op::Unreliable { len: 20, fill: 5 },
        Op::Send { dt: 10 },
        Op::Raw {
            len: 65_000,
            fill: 0x10,
        },
        Op::Raw { len: 120, fill: 0x31 },
        Op::Idle { count: 50, dt: 100 },
        Op::Answer { dt: 5 },
        Op::Rebind { port: 1 },
        Op::Msg { len: 900, fill: 6 },
        Op::Put { index: 0 },
        Op::Send { dt: 20 },
        // The server probed the new port: acknowledging the probe from there validates the path.
        Op::Answer { dt: 10 },
        Op::Idle { count: 5, dt: 100 },
        Op::Replay { back: 3 },
    ];
    encode_ops(&ops)
}

/// The E6 bomb through a sealed server: ids 1..=120 of a full sealed fragment each, never id 0, then keepalives.
fn sealed_hostile_fixture() -> Vec<u8> {
    let mut ops = Vec::new();
    for id in 1..=120u16 {
        ops.push(Op::Frag {
            id,
            more: true,
            len: sealed_cut() as u16,
            fill: id as u8,
        });
        ops.push(Op::Send { dt: 1 });
    }
    ops.push(Op::Idle { count: 50, dt: 100 });
    encode_ops(&ops)
}

/// The steps through a real sealed session (fixed secret, real Noise handshake), the connection bodies sealed
/// under the session's keys. Checked after every datagram: the server never holds more than the one peer, the
/// peer's address is always one the client used (no hijack by a replay or a forged record), a stream that spec
/// 120 § 5.2 says must fail (an over-long fragment, a gap past every deadline) has had its peer dropped, and a
/// path-validation probe the client acknowledged from the probed port has moved the peer there (spec 120 § 10).
fn sealed_sequence_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use regia_net::DatagramSocket;
    use regia_net::crypto::SealedKind;
    let ops = parse_ops(b)?;
    let steps = expand(&ops, lane_message, sealed_cut())?;
    let mut s = sealed_session()?;
    let mut shadow = Shadow::new(&link_config());
    let mut used: HashSet<SocketAddr> = HashSet::from([client_addr(0)]);
    let mut sockets: HashMap<u8, regia_net::LoopbackSocket> = HashMap::new();
    let mut port = 0u8;
    let mut sent: Vec<Vec<u8>> = Vec::new();
    let mut last: Option<Vec<u8>> = None;
    // A rebind or an unsealable packet leaves the model unsure of what the server applied: from then on it only
    // checks what holds whatever was applied.
    let mut sure = true;
    let mut must_be_gone: Option<String> = None;
    let mut pn = 0u64;
    let base = 10u64;
    // Server packet numbers the client received, and the ports where a probe (a packet sent to a port that was not
    // the peer's validated address then) arrived.
    let mut seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut probed: HashSet<u8> = HashSet::new();
    // ADR-8299: the server trial-opens at most `REBIND_ATTEMPTS_PER_S` records per second from one candidate address,
    // and forgets a candidate that sent nothing authentic for `PATH_CANDIDATE_IDLE_MS`. Trial opens per second over
    // every candidate port (the server's per-address buckets may collide, so the sum is the safe bound), and when
    // each port last sent a genuine record while it was not the peer's address.
    let mut trials: HashMap<u64, u32> = HashMap::new();
    let mut last_genuine: HashMap<u8, u64> = HashMap::new();
    for step in steps {
        let mut answers_probe = false;
        let (at, datagram) = match step {
            Step::Packet { at, records } => {
                let body = packet_body(pn, &records);
                pn += 1;
                let Ok(datagram) = s.link.seal(SealedKind::Payload, &body) else {
                    sure = false;
                    continue;
                };
                if sure {
                    match shadow.apply(at, &records, true) {
                        Expect::MustFail(why) => must_be_gone = must_be_gone.or(Some(why)),
                        Expect::MayStall(_) | Expect::Deliver(_) | Expect::Failed => {}
                    }
                    if shadow.waiting_bytes() > BUDGET_SAFE_BYTES {
                        sure = false;
                    }
                }
                last = Some(datagram.clone());
                sent.push(datagram.clone());
                (at, datagram)
            }
            // A byte-identical copy of the last sealed record: the server's replay window refuses it before the
            // connection sees it, so it is not the peer's traffic and the model counts no stall time for it (spec
            // 120 § 5.2, ADR-8717). The plain connection target counts a duplicate: its connection sees one.
            Step::Again { at } => match &last {
                Some(datagram) => (at, datagram.clone()),
                None => continue,
            },
            Step::Rebind { port: p } => {
                port = p;
                sure = false;
                // Only a probe of the address the client now sends from is the server's current candidate.
                probed.clear();
                if port != 0 && !sockets.contains_key(&port) {
                    sockets.insert(port, s.net.bind(client_addr(port)).map_err(e)?);
                }
                used.insert(client_addr(port));
                continue;
            }
            Step::Raw { at, len, fill } => {
                let mut d = pattern(usize::from(len), fill);
                if fill & 1 == 1 && d.len() >= 17 {
                    // A forged record: the magic, the sealed kind and the session's token, then garbage.
                    d[..4].copy_from_slice(&regia_net::handshake::MAGIC);
                    d[4] = regia_net::handshake::PacketKind::Sealed as u8;
                    d[5..9].copy_from_slice(&s.link.token().to_le_bytes());
                }
                (at, d)
            }
            Step::Replay { at, back } => {
                let Some(d) = sent.iter().rev().nth(usize::from(back)) else {
                    continue;
                };
                (at, d.clone())
            }
            Step::Answer { at } => {
                let body = answer_body(pn, &seen);
                pn += 1;
                let Ok(datagram) = s.link.seal(SealedKind::Payload, &body) else {
                    continue;
                };
                if sure && let Expect::MustFail(why) = shadow.apply(at, &[Rec::Ping], true) {
                    must_be_gone = must_be_gone.or(Some(why));
                }
                answers_probe = probed.contains(&port);
                sent.push(datagram.clone());
                (at, datagram)
            }
        };
        let now = base + at;
        let addr_before = s.server.session().peer(s.peer).map(|p| p.addr);
        if answers_probe {
            // A record past the trial budget is refused, and a candidate idle too long is gone: no move is owed.
            let tried = trials.get(&(now / 1_000)).copied().unwrap_or(0);
            let fresh = last_genuine
                .get(&port)
                .is_some_and(|t| now.saturating_sub(*t) < regia_net::path::PATH_CANDIDATE_IDLE_MS);
            answers_probe = tried < regia_net::server::REBIND_ATTEMPTS_PER_S && fresh;
        }
        if addr_before.is_some_and(|a| a != client_addr(port)) && carries_token(&datagram, s.link.token()) {
            let tried = trials.entry(now / 1_000).or_default();
            // Only a record within the budget is opened and refreshes the candidate.
            if *tried < regia_net::server::REBIND_ATTEMPTS_PER_S && sent.last() == Some(&datagram) {
                last_genuine.insert(port, now);
            }
            *tried += 1;
        }
        let socket = if port == 0 {
            &mut s.client
        } else {
            sockets.get_mut(&port).ok_or("no socket for the port")?
        };
        socket.send_to(&datagram, server_addr()).map_err(e)?;
        s.server.receive(now);
        s.server.flush(now);
        while s.server.poll_event().is_some() {}
        let peers: Vec<(regia_net::PeerId, SocketAddr)> =
            s.server.session().peers().map(|p| (p.id, p.addr)).collect();
        assert!(
            peers.len() <= 1,
            "one client made {} peers: {peers:?}",
            peers.len()
        );
        for (_, addr) in &peers {
            assert!(
                used.contains(addr),
                "the peer moved to {addr}, an address the client never used"
            );
        }
        if let Some(why) = &must_be_gone {
            assert!(
                s.server.session().peer(s.peer).is_none(),
                "the server kept the peer after {why}"
            );
        }
        let at_port = s.server.session().peer(s.peer).map(|p| p.addr);
        if answers_probe && let Some(addr) = at_port {
            assert!(
                addr == client_addr(port),
                "the client acknowledged the probe sent to {} from there, and the peer stayed at {addr}",
                client_addr(port)
            );
        }
        // Read what the server sent (so the loopback queues stay small): the packet numbers the client saw, and
        // which ports were probed.
        let mut buf = [0u8; 2048];
        let mut inbox = vec![(0u8, &mut s.client)];
        inbox.extend(sockets.iter_mut().map(|(p, socket)| (*p, socket)));
        for (p, socket) in inbox {
            while let Ok(Some((n, _))) = socket.recv_from(&mut buf) {
                let Some(number) = server_packet_number(&mut s.link, &buf[..n], seen.last().copied()) else {
                    continue;
                };
                seen.insert(number);
                if at_port.is_some_and(|a| a != client_addr(p)) {
                    probed.insert(p);
                }
            }
        }
    }
    Ok(Vec::new())
}

/// True when `d` is a sealed record under session `token` (what the server trial-opens from a new address).
fn carries_token(d: &[u8], token: u32) -> bool {
    d.len() >= regia_net::crypto::SEALED_HEADER_BYTES
        && d[..4] == regia_net::handshake::MAGIC
        && d[4] == regia_net::handshake::PacketKind::Sealed as u8
        && d[5..9] == token.to_le_bytes()
}

/// The packet number of a sealed connection packet the server sent the client, if `d` is one that opens.
fn server_packet_number(
    link: &mut regia_net::crypto::SecureLink,
    d: &[u8],
    largest: Option<u64>,
) -> Option<u64> {
    use regia_net::crypto::{SealedHeader, SealedKind};
    use regia_net::handshake::{PacketKind, split_datagram};
    let (PacketKind::Sealed, body) = split_datagram(d)? else {
        return None;
    };
    let (SealedKind::Payload, plain) = link.open(SealedHeader::parse(body)?).ok()? else {
        return None;
    };
    let mut r = WireReader::new(&plain);
    let flags = r.u8().ok()?;
    let (truncated, len) = match flags & 0x03 {
        0 => (u64::from(r.u8().ok()?), 1),
        1 => (u64::from(r.u16().ok()?), 2),
        2 => (u64::from(r.u32().ok()?), 4),
        _ => return None,
    };
    Some(regia_net::ack::decode_packet_number(largest, truncated, len))
}

/// A keepalive packet numbered `pn` whose acknowledgment frame names every packet in `seen` (newest 32 runs).
fn answer_body(pn: u64, seen: &std::collections::BTreeSet<u64>) -> Vec<u8> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for &n in seen.iter().rev() {
        match ranges.last_mut() {
            Some((lo, _)) if *lo == n + 1 => *lo = n,
            _ => ranges.push((n, n)),
        }
    }
    let mut w = WireWriter::new();
    let Some(&(_, largest)) = ranges.first() else {
        return packet_body(pn, &[Rec::Ping]);
    };
    // Flags: a four-byte packet number and an acknowledgment frame (0x04).
    w.u8(0x02 | 0x04);
    w.u32(pn as u32);
    regia_net::ack::AckFrame {
        largest,
        delay_us: 0,
        arrival_ms: 0,
        ranges,
        timestamps: Vec::new(),
    }
    .encode(&mut w, false, 512);
    w.u8(0x03);
    w.finish()
}

// ---- net.shared_port_sequence -----------------------------------------------------------------------------------

/// The link game's protocol id on the fuzzed shared port.
const LINK_PROTOCOL: u64 = 0x11AA_22BB_33CC_44DD;
/// The session protocol id (any other).
const SESSION_PROTOCOL: u64 = 0x0101_0202_0303_0404;
/// The largest datagram an endpoint reads (`regia_net::socket::MAX_DATAGRAM_BYTES`).
const MAX_DATAGRAM: usize = 2048;
/// Ops one shared-port script holds at most.
const MAX_PORT_OPS: usize = 256;

/// Where a datagram should go, by the routing rules of the shared port's module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dest {
    None,
    Session,
    Link,
    Both,
}

/// A model of the routing tables: an address claimed by a half, an address a half challenged, a token a half
/// sends sealed records under.
#[derive(Default)]
struct PortModel {
    routes: HashMap<SocketAddr, Dest>,
    pending: HashMap<SocketAddr, Dest>,
    tokens: HashMap<u32, Dest>,
}

impl PortModel {
    fn route(&self, d: &[u8], from: SocketAddr) -> Dest {
        if d.len() > MAX_DATAGRAM {
            return Dest::None;
        }
        if regia_net::version_probe::is_probe_request(d) {
            return self
                .routes
                .get(&from)
                .or_else(|| self.pending.get(&from))
                .copied()
                .unwrap_or(Dest::Link);
        }
        if d.len() < 5 || d[..4] != regia_net::handshake::MAGIC {
            return Dest::None;
        }
        let Some(kind) = regia_net::handshake::PacketKind::from_u8(d[4]) else {
            return Dest::None;
        };
        use regia_net::handshake::PacketKind as K;
        match kind {
            K::ConnectRequest => match regia_net::handshake::ConnectRequest::decode(&d[5..]) {
                Ok(req) if req.protocol_id == LINK_PROTOCOL => Dest::Link,
                _ => Dest::Session,
            },
            K::KeyInit | K::ChallengeResponse => self.pending.get(&from).copied().unwrap_or(Dest::Both),
            _ => {
                if kind == K::Sealed
                    && let Some(h) = regia_net::crypto::SealedHeader::parse(&d[5..])
                    && let Some(dest) = self.tokens.get(&h.token)
                {
                    return *dest;
                }
                self.routes
                    .get(&from)
                    .or_else(|| self.pending.get(&from))
                    .copied()
                    .unwrap_or(Dest::Session)
            }
        }
    }

    fn sent(&mut self, half: Dest, d: &[u8], to: SocketAddr) {
        use regia_net::handshake::PacketKind as K;
        if d.len() < 5 || d[..4] != regia_net::handshake::MAGIC {
            return;
        }
        match K::from_u8(d[4]) {
            Some(K::Challenge) => {
                self.pending.insert(to, half);
            }
            Some(kind @ (K::Accept | K::KeyReply | K::Payload | K::Sealed)) => {
                self.pending.remove(&to);
                self.routes.insert(to, half);
                if kind == K::Sealed
                    && let Some(h) = regia_net::crypto::SealedHeader::parse(&d[5..])
                {
                    self.tokens.insert(h.token, half);
                }
            }
            _ => {}
        }
    }
}

/// One shared-port op: a datagram in from source `src`, or a datagram out from a half to `src`.
#[derive(Debug, Clone, Copy)]
struct PortOp {
    /// 0: in; 1: the session half sends; 2: the link half sends.
    dir: u8,
    src: u8,
    kind: u8,
    token: u32,
    len: u16,
}

fn port_datagram(op: &PortOp) -> Vec<u8> {
    use regia_net::handshake::{ConnectRequest, PacketKind as K};
    let kinds = [
        K::ConnectRequest,
        K::Challenge,
        K::ChallengeResponse,
        K::Accept,
        K::Payload,
        K::Sealed,
        K::KeyInit,
        K::KeyReply,
        K::Disconnect,
    ];
    let len = usize::from(op.len);
    match op.kind % 12 {
        0 | 1 => {
            let id = if op.kind.is_multiple_of(12) {
                LINK_PROTOCOL
            } else {
                SESSION_PROTOCOL
            };
            ConnectRequest {
                protocol_id: id,
                client_nonce: u64::from(op.token),
                crypto_version: 0,
                crypto_required: false,
            }
            .to_datagram()
        }
        // A version probe request.
        2 => regia_net::version_probe::ProbeRequest {
            nonce: u64::from(op.token),
        }
        .to_datagram(),
        // Foreign bytes.
        3 => pattern(len, op.token as u8),
        k => {
            let kind = kinds[usize::from(k - 4) % kinds.len()];
            let mut d = regia_net::handshake::MAGIC.to_vec();
            d.push(kind as u8);
            d.extend_from_slice(&op.token.to_le_bytes());
            d.extend_from_slice(&[0; 8]);
            d.extend(pattern(len, op.kind));
            d
        }
    }
}

fn parse_port_ops(b: &[u8]) -> Result<Vec<PortOp>, String> {
    let mut r = WireReader::new(b);
    if r.take(4).map_err(e)? != b"NPRT" {
        return Err("not a shared-port script".into());
    }
    let count = usize::from(r.u16().map_err(e)?);
    if count == 0 || count > MAX_PORT_OPS {
        return Err(format!("{count} ops (1..={MAX_PORT_OPS})"));
    }
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        ops.push(PortOp {
            dir: r.u8().map_err(e)? % 3,
            src: r.u8().map_err(e)? % 8,
            kind: r.u8().map_err(e)?,
            token: r.u32().map_err(e)?,
            len: r.u16().map_err(e)?,
        });
    }
    if !r.is_empty() {
        return Err("bytes after the last op".into());
    }
    Ok(ops)
}

fn encode_port_ops(ops: &[PortOp]) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.raw(b"NPRT");
    w.u16(ops.len() as u16);
    for op in ops {
        w.u8(op.dir);
        w.u8(op.src);
        w.u8(op.kind);
        w.u32(op.token);
        w.u16(op.len);
    }
    w.finish()
}

/// Two clients and a scanner: connect requests for each layer, the halves' challenges and accepts, sealed records
/// under two tokens, a version probe, a rebound client's sealed record from a new port, foreign bytes, a 64 KiB
/// datagram (the soak harness's hostile oversized datagram) and payloads from claimed and unclaimed addresses.
fn shared_port_fixture() -> Vec<u8> {
    let op = |dir, src, kind, token, len| PortOp {
        dir,
        src,
        kind,
        token,
        len,
    };
    encode_port_ops(&[
        op(0, 0, 0, 1, 0),      // client 0 asks for the link
        op(2, 0, 5, 1, 0),      // the link half challenges it
        op(0, 0, 6, 1, 30),     // its challenge response
        op(2, 0, 9, 77, 40),    // the link half sends a sealed record under token 77
        op(0, 1, 1, 2, 0),      // client 1 asks for the session
        op(1, 1, 5, 2, 0),      // challenge
        op(1, 1, 7, 0, 10),     // accept
        op(0, 1, 8, 0, 64),     // payload from client 1
        op(0, 2, 9, 77, 80),    // client 0's record, rebound to port 2
        op(0, 3, 2, 9, 0),      // a version probe from a scanner
        op(0, 4, 3, 5, 40),     // foreign bytes
        op(0, 5, 8, 0, 65_000), // an oversized datagram
        op(0, 6, 10, 3, 32),    // a key init with no challenge row
        op(0, 0, 8, 0, 100),    // payload from client 0
    ])
}

/// The ops through `SharedPort::split` on a loopback socket; after each one both halves are read empty and every
/// datagram must have gone exactly where [`PortModel`] says (a datagram over the endpoint limit nowhere).
fn shared_port_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use regia_net::DatagramSocket;
    use regia_net::server_host::shared_port::SharedPort;
    let ops = parse_port_ops(b)?;
    let net = regia_net::LoopbackNet::new();
    let host = SocketAddr::from((Ipv4Addr::new(10, 1, 0, 1), 27_015));
    let socket = net.bind(host).map_err(e)?;
    let (_port, mut session, mut link) = SharedPort::split(Box::new(socket), LINK_PROTOCOL);
    let mut sources: Vec<regia_net::LoopbackSocket> = (0..8u16)
        .map(|i| net.bind(SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), 50_000 + i))))
        .collect::<Result<_, _>>()
        .map_err(e)?;
    let mut model = PortModel::default();
    let mut buf = vec![0u8; 65_536];
    for op in &ops {
        let d = port_datagram(op);
        let src = &mut sources[usize::from(op.src)];
        let src_addr = src.local_addr().map_err(e)?;
        match op.dir {
            0 => {
                let want = model.route(&d, src_addr);
                src.send_to(&d, host).map_err(e)?;
                let mut got = [false; 2];
                for (i, half) in [&mut session, &mut link].into_iter().enumerate() {
                    while let Some((n, from)) = half.recv_from(&mut buf).map_err(e)? {
                        assert!(
                            !got[i],
                            "one datagram reached the {} half twice",
                            ["session", "link"][i]
                        );
                        assert!(
                            from == src_addr && buf[..n] == d[..],
                            "the {} half read {n} B from {from}, not the {} B datagram from {src_addr}",
                            ["session", "link"][i],
                            d.len()
                        );
                        got[i] = true;
                    }
                }
                let route = match got {
                    [false, false] => Dest::None,
                    [true, false] => Dest::Session,
                    [false, true] => Dest::Link,
                    [true, true] => Dest::Both,
                };
                assert!(
                    route == want,
                    "a {} B datagram (kind byte {:?}) from {src_addr} went to {route:?}, the routing rules say {want:?}",
                    d.len(),
                    d.get(4)
                );
                // Drain what the halves sent back to the source.
                while src.recv_from(&mut buf).map_err(e)?.is_some() {}
            }
            dir => {
                let (half, dest) = if dir == 1 {
                    (&mut session, Dest::Session)
                } else {
                    (&mut link, Dest::Link)
                };
                if d.len() <= MAX_DATAGRAM {
                    half.send_to(&d, src_addr).map_err(e)?;
                    model.sent(dest, &d, src_addr);
                }
                while src.recv_from(&mut buf).map_err(e)?.is_some() {}
            }
        }
    }
    Ok(Vec::new())
}

// ---- net.voice_relay_route --------------------------------------------------------------------------------------

/// Peers one voice script may name.
const VOICE_PEERS: u8 = 6;

fn voice_ops(b: &[u8]) -> Result<Vec<[u8; 6]>, String> {
    let mut r = WireReader::new(b);
    if r.take(4).map_err(e)? != b"NVOX" {
        return Err("not a voice script".into());
    }
    let count = usize::from(r.u16().map_err(e)?);
    if count == 0 || count > 512 {
        return Err(format!("{count} ops (1..=512)"));
    }
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        let op: [u8; 6] = r.take(6).map_err(e)?.try_into().map_err(e)?;
        ops.push(op);
    }
    if !r.is_empty() {
        return Err("bytes after the last op".into());
    }
    Ok(ops)
}

/// A voice packet from the relay's own encoder: channel, sequence, `len` bytes of payload.
fn voice_packet(channel: u16, seq: u16, len: usize) -> Vec<u8> {
    use regia_audio::voice_chat::{VoiceCodecId, VoiceFlags, VoicePacket, VoiceQuality};
    let payload = pattern(len, seq as u8);
    let mut out = Vec::new();
    VoicePacket {
        flags: VoiceFlags::from_bits(0),
        sender: 0,
        channel,
        seq,
        timestamp: u32::from(seq) * 960,
        quality: VoiceQuality::Standard,
        codec: VoiceCodecId::Pcm,
        payload: &payload,
    }
    .write(&mut out);
    out
}

/// Three peers join the team and nearby channels, talk in order and out of order, one leaves, one rejoins, a
/// stranger sends, a garbage packet and a 64 KiB one arrive.
fn voice_fixture() -> Vec<u8> {
    let mut w = WireWriter::new();
    w.raw(b"NVOX");
    // op: kind, peer, a, b, c, dt
    let ops: &[[u8; 6]] = &[
        [0, 0, 0, 0, 0, 0],
        [0, 1, 0, 0, 0, 0],
        [0, 2, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0],
        [1, 1, 0, 0, 0, 0],
        [1, 2, 1, 0, 0, 0],
        [2, 0, 0, 1, 60, 20],
        [2, 1, 0, 2, 60, 20],
        [2, 0, 0, 3, 60, 20],
        [2, 1, 0, 1, 60, 5],
        [3, 2, 0, 0, 0, 20],
        [4, 3, 0, 0, 0, 20],
        [2, 1, 0, 4, 255, 20],
        [5, 0, 0, 0, 0, 0],
        [2, 0, 0, 5, 60, 20],
        [0, 0, 0, 0, 0, 0],
        [1, 0, 1, 0, 0, 0],
        [2, 0, 1, 6, 60, 20],
    ];
    w.u16(ops.len() as u16);
    for op in ops {
        w.raw(op);
    }
    w.finish()
}

/// The ops against one `VoiceRelay` (default settings: the team, nearby, party and all channels): add, join, route,
/// garbage, an oversized packet, leave. After each route every copy queued for a listener carries the sender's
/// authenticated slot, never goes back to the sender, and only goes to a peer the relay holds.
fn voice_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use regia_audio::voice_chat::{VoiceChatSettings, VoiceControl};
    use regia_net::PeerId;
    use regia_net::voice::{VoiceRelay, VoiceRelaySettings};
    let ops = voice_ops(b)?;
    let mut relay = VoiceRelay::new(VoiceRelaySettings::default(), &VoiceChatSettings::default());
    let mut now = 1_000u64;
    let peer = |i: u8| PeerId(100 + u32::from(i % VOICE_PEERS));
    for [kind, p, a, seq, len, dt] in ops {
        now += u64::from(dt);
        let who = peer(p);
        match kind % 6 {
            0 => {
                let _ = relay.add_peer(who);
            }
            1 => relay.control(who, &VoiceControl::Join(1 + u16::from(a % 4))),
            2 => {
                let bytes = voice_packet(1 + u16::from(a % 4), u16::from(seq), usize::from(len));
                route_checked(&mut relay, who, &bytes, now);
            }
            3 => route_checked(&mut relay, who, &pattern(usize::from(len) + 3, seq), now),
            4 => {
                let mut big = voice_packet(1 + u16::from(a % 4), u16::from(seq), 60);
                big.resize(65_000, seq);
                route_checked(&mut relay, who, &big, now);
            }
            _ => relay.remove_peer(who),
        }
    }
    Ok(Vec::new())
}

fn route_checked(relay: &mut regia_net::voice::VoiceRelay, from: regia_net::PeerId, bytes: &[u8], now: u64) {
    let slot = relay.slot(from);
    relay.route(from, bytes, now);
    let held: HashSet<regia_net::PeerId> = relay.peers().map(|(p, _)| p).collect();
    for (to, packets) in relay.take_outgoing() {
        assert!(
            held.contains(&to),
            "a copy queued for {to:?}, a peer the relay does not hold"
        );
        assert!(to != from, "a packet from {from:?} was queued back to its sender");
        // invariant: a model finding, not input reaching a panic: the fuzz harness catches it and reports the seed.
        let slot = slot.unwrap_or_else(|| panic!("{from:?} has no slot, yet its packet was forwarded"));
        for p in packets {
            assert!(
                p.get(1..3) == Some(&slot.to_le_bytes()[..]),
                "a forwarded copy does not carry the sender's slot {slot}"
            );
        }
    }
    let _ = relay.take_notices();
}

// ---- net.input_bundle_budget ------------------------------------------------------------------------------------

/// An input bundle at the datagram budget: large legal samples (a 600-byte and a 280-byte command buffer, then
/// small ones), so the mutations reach the length words near the cap rather than the tiny bundle of
/// `net.input_bundle`.
fn input_bundle_budget_fixture() -> Vec<u8> {
    let mut w = WireWriter::new();
    regia_net::tick::InputBundle::<Vec<u8>> {
        first_tick: 9_000,
        inputs: vec![pattern(600, 1), pattern(280, 2), pattern(40, 3), Vec::new()],
    }
    .encode(&mut w);
    w.finish()
}

/// Decodes the bundle whole (bytes after it are an error, as the server reads it) and encodes it again; a bundle
/// that decodes must fit one datagram (`MAX_DATAGRAM_BYTES`) when encoded again.
fn input_bundle_budget_load(d: &[u8]) -> Result<Vec<u8>, String> {
    let mut r = WireReader::new(d);
    let bundle = regia_net::tick::InputBundle::<Vec<u8>>::decode(&mut r).map_err(e)?;
    r.finish().map_err(e)?;
    let mut w = WireWriter::new();
    bundle.encode(&mut w);
    let out = w.finish();
    assert!(
        out.len() <= MAX_DATAGRAM,
        "a decoded input bundle encodes to {} B, over one datagram",
        out.len()
    );
    Ok(out)
}

// ---- net.snapshot_sequence -------------------------------------------------------------------------------------

/// Entity ids a snapshot script uses (`1..=SNAPSHOT_IDS`).
const SNAPSHOT_IDS: u8 = 12;

fn snapshot_schema() -> Result<regia_net::SnapshotSchema, String> {
    use regia_core::math::Vec3;
    use regia_net::{FieldDef, FieldKind};
    regia_net::SnapshotSchema::new(vec![
        FieldDef::new("alive", FieldKind::Bool),
        FieldDef::new("ammo", FieldKind::U16),
        FieldDef::new("drift", FieldKind::I32),
        FieldDef::new(
            "health",
            FieldKind::Float {
                min: 0.0,
                max: 100.0,
                bits: 10,
            },
        ),
        FieldDef::new(
            "pos",
            FieldKind::Vec3Quantised {
                min: Vec3::splat(-512.0),
                max: Vec3::splat(512.0),
                bits: 18,
            },
        ),
        FieldDef::new("stance", FieldKind::Enum { variants: 5 }),
    ])
    .map_err(e)
}

fn snapshot_entity(id: u8, step: u8) -> regia_net::EntitySnapshot {
    use regia_core::math::Vec3;
    use regia_net::FieldValue;
    let f = f32::from(id) * 7.0 + f32::from(step) * 1.5;
    regia_net::EntitySnapshot::new(
        regia_net::NetId(u32::from(id)),
        vec![
            FieldValue::Bool(step.is_multiple_of(3)),
            FieldValue::U16(u16::from(step) * 11),
            FieldValue::I32(i32::from(step) - 100),
            FieldValue::F32(f % 100.0),
            FieldValue::Vec3(Vec3::new(f, -f * 0.5, f32::from(step))),
            FieldValue::Enum(u32::from(step % 5)),
        ],
    )
}

/// A server and a client of snapshot deltas (spec 120 § 8.1): spawns, updates and despawns, encodes against what the
/// client acknowledged, and deliveries that drop, reorder and duplicate packets.
fn snapshot_ops(b: &[u8]) -> Result<Vec<[u8; 4]>, String> {
    let mut r = WireReader::new(b);
    if r.take(4).map_err(e)? != b"NSNP" {
        return Err("not a snapshot script".into());
    }
    let count = usize::from(r.u16().map_err(e)?);
    if count == 0 || count > 512 {
        return Err(format!("{count} ops (1..=512)"));
    }
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        ops.push(<[u8; 4]>::try_from(r.take(4).map_err(e)?).map_err(e)?);
    }
    if !r.is_empty() {
        return Err("bytes after the last op".into());
    }
    Ok(ops)
}

/// Ops (`kind, a, b, c`): 0 spawn or update `a` at step `b`; 1 despawn `a`; 2 encode with the client's latest
/// baseline as the ack (`a % 4 == 1`: no ack; `== 2`: the oldest snapshot the client decoded; `== 3`: sequence `b`,
/// which the client may never have seen); 3 deliver in-flight packet `b` (`c & 1`: keep it in flight, so it comes
/// again).
fn snapshot_fixture() -> Vec<u8> {
    let ops: &[[u8; 4]] = &[
        [0, 1, 0, 0],
        [0, 2, 0, 0],
        [0, 3, 0, 0],
        [2, 1, 0, 0],
        [3, 0, 0, 0],
        [0, 1, 1, 0],
        [0, 4, 0, 0],
        [2, 0, 0, 0],
        [0, 2, 5, 0],
        [1, 3, 0, 0],
        [2, 0, 0, 0],
        // The second delta first, then the first one twice.
        [3, 0, 1, 0],
        [3, 0, 0, 1],
        [3, 0, 0, 0],
        [0, 5, 9, 0],
        [2, 2, 0, 0],
        [2, 3, 40, 0],
        [3, 0, 1, 0],
        [3, 0, 0, 0],
        [0, 1, 200, 0],
        [2, 0, 0, 0],
        [3, 0, 0, 0],
    ];
    let mut w = WireWriter::new();
    w.raw(b"NSNP");
    w.u16(ops.len() as u16);
    for op in ops {
        w.raw(op);
    }
    w.finish()
}

/// Every packet the client decodes as its newest must be exactly the world the server encoded at that sequence number,
/// quantised (whatever baseline it was a delta against, in whatever order it arrived); a late or repeated packet only
/// becomes a baseline and leaves the shown world as it was (ADR-7983); a packet the client cannot apply is refused,
/// never applied wrong.
fn snapshot_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let ops = snapshot_ops(b)?;
    let schema = snapshot_schema()?;
    let mut encoder = regia_net::SnapshotEncoder::new(schema.clone());
    let mut decoder = regia_net::SnapshotDecoder::new(schema.clone());
    let mut world: std::collections::BTreeMap<u8, u8> = std::collections::BTreeMap::new();
    let mut expected: HashMap<u32, regia_net::WorldSnapshot> = HashMap::new();
    let mut in_flight: Vec<Vec<u8>> = Vec::new();
    let mut decoded: Vec<u32> = Vec::new();
    let mut tick = 100u32;
    for [kind, a, b, c] in ops {
        let id = 1 + a % SNAPSHOT_IDS;
        match kind % 4 {
            0 => {
                world.insert(id, b);
            }
            1 => {
                world.remove(&id);
            }
            2 => {
                let ack = match a % 4 {
                    0 => decoder.latest_baseline(),
                    1 => None,
                    2 => decoded.iter().min().map(|s| regia_net::Seq(*s)),
                    _ => Some(regia_net::Seq(u32::from(b))),
                };
                let snapshot = regia_net::WorldSnapshot::new(
                    world
                        .iter()
                        .map(|(id, step)| snapshot_entity(*id, *step))
                        .collect(),
                );
                let seq = encoder.next_seq().0;
                tick += 1;
                let (packet, report) = encoder
                    .encode_with_report(regia_net::NetTick(tick), &snapshot, ack, 8 * 1024)
                    .map_err(|err| format!("bug: the encoder refused a valid world: {err}"))?;
                if report.complete {
                    let mut want = schema.quantize_snapshot(&snapshot).map_err(e)?;
                    want.entities.sort_by_key(|en| en.id);
                    expected.insert(seq, want);
                }
                in_flight.push(packet);
            }
            _ => {
                if in_flight.is_empty() {
                    continue;
                }
                let i = usize::from(b) % in_flight.len();
                let packet = if c & 1 == 1 {
                    in_flight[i].clone()
                } else {
                    in_flight.remove(i)
                };
                let shown_before = decoder.current().cloned();
                let Ok(got) = decoder.decode(&packet) else {
                    continue;
                };
                decoded.push(got.seq.0);
                if decoder.last_seq() != Some(got.seq) {
                    // Late or repeated (ADR-7983, R1): stored as a baseline only; the shown world
                    // never moves back. Its stored state is checked when a later delta names it.
                    assert!(
                        Some(&got.snapshot) == shown_before.as_ref(),
                        "late snapshot {} moved the shown world",
                        got.seq.0
                    );
                    continue;
                }
                if let Some(want) = expected.get(&got.seq.0) {
                    assert!(
                        got.snapshot == *want,
                        "snapshot {} (a {} against {:?}) decoded to {} entities {:?}, the server encoded {} {:?}",
                        got.seq.0,
                        if got.is_full { "full packet" } else { "delta" },
                        got.baseline.map(|s| s.0),
                        got.snapshot.len(),
                        got.snapshot.ids().map(|i| i.0).collect::<Vec<_>>(),
                        want.len(),
                        want.ids().map(|i| i.0).collect::<Vec<_>>()
                    );
                }
            }
        }
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_round_trip() {
        for fixture in [connection_sequence_fixture(), sealed_sequence_fixture()] {
            let ops = parse_ops(&fixture).expect("parses");
            assert_eq!(encode_ops(&ops), fixture);
        }
    }

    #[test]
    fn the_shadow_delivers_an_out_of_order_message_once() {
        let mut s = Shadow::new(&link_config());
        let frag = |id, more| Rec::Reliable {
            id,
            more,
            bytes: vec![id as u8; 10],
        };
        assert!(matches!(s.apply(0, &[frag(1, false)], true), Expect::Deliver(m) if m.is_empty()));
        match s.apply(10, &[frag(0, true)], true) {
            Expect::Deliver(m) => {
                assert_eq!(m, vec![(Channel::GAME_RPC, [vec![0; 10], vec![1; 10]].concat())])
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_shadow_requires_a_stalled_gap_to_fail() {
        let mut s = Shadow::new(&link_config());
        let gap = Rec::Reliable {
            id: 1,
            more: true,
            bytes: vec![1; 10],
        };
        let _ = s.apply(0, &[gap], true);
        let mut verdicts = Vec::new();
        for i in 1..=40u64 {
            verdicts.push(s.apply(i * 100, &[Rec::Ping], true));
        }
        assert!(
            matches!(verdicts[18], Expect::Deliver(_)),
            "1.9 s: under the floor"
        );
        assert!(
            matches!(verdicts[19], Expect::MayStall(_)),
            "2 s: a legal deadline"
        );
        assert!(
            matches!(verdicts[39], Expect::MustFail(_)),
            "4 s: past every deadline"
        );
    }

    /// ADR-8717: the registry's `net.sealed_endpoint_sequence` iteration 83. A gap opens, the peer re-sends its
    /// sealed record byte for byte twice (224 and 119 ms later), then keepalives every 100 ms. The server's replay
    /// window refuses the copies and its own keepalive is never answered, so its stall deadline is 4 s (ADR-8658)
    /// of the traffic it opened. Counting the copies put the model 200 ms ahead of the server ("kept the peer
    /// after a reassembly gap stood for 4000 ms"); the server must still drop the peer once 4 s of that traffic
    /// has passed, which the load checks after every datagram.
    #[test]
    fn a_resent_sealed_record_counts_no_stall_time() {
        let ops = [
            Op::Frag {
                id: 1,
                more: true,
                len: 300,
                fill: 1,
            },
            Op::Send { dt: 10 },
            Op::Dup { dt: 224 },
            Op::Dup { dt: 119 },
            Op::Idle { count: 60, dt: 100 },
        ];
        assert_eq!(sealed_sequence_load(&encode_ops(&ops)), Ok(Vec::new()));
    }
}
