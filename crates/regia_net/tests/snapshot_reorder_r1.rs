// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.1 (R1), through the public API of `regia_net::snapshot_delta` only: the standalone
//! snapshot codec never moves the shown world backward on a late or repeated packet, and never
//! replaces or shrinks the shown world with a packet the sender's byte budget cut short.
//!
//! Everything is seeded and runs in ticks: no wall clock, no sleeps, no random source but the
//! xorshift below.

use std::collections::BTreeMap;

use regia_net::bits::{BitReader, BitWriter};
use regia_net::model::{NetId, NetTick};
use regia_net::snapshot_delta::{
    EntitySnapshot, FieldDef, FieldKind, FieldValue, OUTER_HEADER_BYTES, Seq, SnapshotDecoder,
    SnapshotEncoder, SnapshotError, SnapshotSchema, WorldSnapshot, crc32_ieee,
};

/// A budget no packet of these tests reaches.
const BIG: usize = 1 << 16;

/// xorshift64 with a multiply on the way out.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn roll(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        (self.roll() >> 11) % n.max(1)
    }

    fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
}

/// Integer fields only: what decodes is exactly what was encoded, so worlds compare with `==`.
fn schema() -> SnapshotSchema {
    SnapshotSchema::new(vec![
        FieldDef::new("hp", FieldKind::U32),
        FieldDef::new("ammo", FieldKind::U16),
        FieldDef::new("alive", FieldKind::Bool),
    ])
    .expect("schema")
}

fn fields(salt: u32) -> Vec<FieldValue> {
    vec![
        FieldValue::U32(salt),
        FieldValue::U16((salt % 60_000) as u16),
        FieldValue::Bool(salt.is_multiple_of(2)),
    ]
}

fn entity(id: u32, salt: u32) -> EntitySnapshot {
    EntitySnapshot::new(NetId(id), fields(salt))
}

/// A world as `(id, fields)` in id order: what a decoder can know of it (priorities do not travel).
fn plain(world: &WorldSnapshot) -> Vec<(NetId, Vec<FieldValue>)> {
    let mut out: Vec<_> = world.entities.iter().map(|e| (e.id, e.fields.clone())).collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

fn ids(world: Option<&WorldSnapshot>) -> Vec<NetId> {
    world.map_or_else(Vec::new, |w| w.entities.iter().map(|e| e.id).collect())
}

fn serial_newer(a: Seq, b: Seq) -> bool {
    (a.0.wrapping_sub(b.0) as i32) > 0
}

/// Sequences an encoder assigns before the mapped stream wraps (see [`to_wire`]).
const WRAP_LEAD: u32 = 41;
/// The wire sequence of the encoder's sequence 1 in a wrapped run.
const WRAP_START: u32 = u32::MAX - (WRAP_LEAD - 1);

/// The sequence a stream that began at [`WRAP_START`] would carry for the encoder's `seq`: it
/// counts up to `u32::MAX` and goes on at 1, skipping 0 as the encoder does.
fn to_wire(seq: u32) -> u32 {
    if seq <= WRAP_LEAD {
        WRAP_START + (seq - 1)
    } else {
        seq - WRAP_LEAD
    }
}

fn from_wire(wire: u32) -> u32 {
    if wire >= WRAP_START {
        wire - WRAP_START + 1
    } else {
        wire + WRAP_LEAD
    }
}

/// Rewrites a packet's sequence and baseline with [`to_wire`] and seals it again. The encoder has
/// no public way to start near `u32::MAX`; the packet layout is public (module docs), varints are
/// whole bytes, so the records keep their alignment and the padding its length.
fn resequence(packet: &[u8]) -> Vec<u8> {
    let body = packet.get(OUTER_HEADER_BYTES..).expect("packet body");
    let mut r = BitReader::new(body);
    let mut w = BitWriter::with_capacity(body.len() + 8);
    w.write_bits(r.read_bits(32).expect("layout hash"), 32);
    w.write_bits(r.read_bits(8).expect("flags"), 8);
    let seq = u32::try_from(r.read_varint().expect("seq")).expect("seq fits u32");
    let tick = r.read_varint().expect("tick");
    let baseline = u32::try_from(r.read_varint().expect("baseline")).expect("baseline fits u32");
    w.write_varint(u64::from(to_wire(seq)));
    w.write_varint(tick);
    w.write_varint(if baseline == 0 {
        0
    } else {
        u64::from(to_wire(baseline))
    });
    while r.remaining_bits() > 0 {
        let n = r.remaining_bits().min(32) as u32;
        w.write_bits(r.read_bits(n).expect("record bits"), n);
    }
    let body = w.finish();
    let total = u32::try_from(OUTER_HEADER_BYTES + body.len()).expect("packet length");
    let mut out = packet.get(..2).expect("magic and version").to_vec();
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&crc32_ieee(&body).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// What a stress run saw: the tests assert that the hard cases really happened.
#[derive(Debug, Default)]
struct Outcome {
    /// Packets decoded after a newer one (reordered or repeated).
    late: u32,
    /// Packets refused for a baseline the decoder no longer holds.
    baseline_missing: u32,
    /// Full packets the budget cut short, as encoded.
    partial_fulls_sent: u32,
    /// Incomplete packets merged into a world that was already shown.
    merges: u32,
    /// Acknowledgments that reached the encoder older than the one before.
    acks_backward: u32,
    /// Complete packets checked against the server's world of their tick.
    exact: u32,
    /// The newest wire sequence the decoder showed.
    last_wire_seq: u32,
}

const STRESS_TICKS: u32 = 640;
/// From here on the channel is clean: no loss, no reorder, no repeats, one tick each way.
const CLEAN_FROM: u32 = STRESS_TICKS - 200;
/// From here on the server's world stands still, so the stream can converge exactly.
const FREEZE_FROM: u32 = STRESS_TICKS - 150;
/// Baselines both ends hold: small, so the two acknowledgment stalls overflow the encoder's ring.
const CAPACITY: usize = 16;
/// Bytes per packet: about a sixth of the world's full state.
const BUDGET: usize = 220;

/// During these ticks no acknowledgment gets through: longer than the ring holds.
fn acks_stalled(tick: u32) -> bool {
    (100..160).contains(&tick) || (300..345).contains(&tick)
}

/// One seeded run: loss, reorder, repeats and acknowledgment stalls under a byte budget smaller
/// than the world, then a clean channel. `wrap` carries the stream across the `u32` sequence wrap.
/// Panics on the first decode that breaks an R1 invariant.
fn stress(seed: u64, wrap: bool) -> Outcome {
    let mut rng = Rng::new(seed);
    let mut out = Outcome::default();
    let mut enc = SnapshotEncoder::with_capacity(schema(), CAPACITY);
    let mut dec = SnapshotDecoder::with_capacity(schema(), CAPACITY);
    let mut world = WorldSnapshot::new((1..=120).map(|i| entity(i, i)).collect());
    let mut next_id = 121u32;
    // Per-entity send urgency: grows every tick, starts again when a packet carried the entity
    // (the priority accumulators of spec 120 § 8.3, so what a budget omits rotates).
    let mut urgency: BTreeMap<NetId, u32> = BTreeMap::new();
    // The server's world at each tick, as a decoder may know it.
    let mut history: BTreeMap<u32, Vec<(NetId, Vec<FieldValue>)>> = BTreeMap::new();
    // In flight: `(due tick, bytes)` down, `(due tick, sequence)` up.
    let mut down: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut up: Vec<(u32, Seq)> = Vec::new();
    let mut ack: Option<Seq> = None;
    let mut shown_tick = 0u32;

    for tick in 1..=STRESS_TICKS {
        let noisy = tick < CLEAN_FROM;

        // Acknowledgments that arrive now, in arrival order: a reordered one moves `ack` back.
        let (due, later): (Vec<_>, Vec<_>) = up.drain(..).partition(|(at, _)| *at <= tick);
        up = later;
        for (_, seq) in due {
            if ack.is_some_and(|held| serial_newer(held, seq)) {
                out.acks_backward += 1;
            }
            ack = Some(seq);
        }

        // The server's tick.
        if tick < FREEZE_FROM {
            for i in 0..rng.below(6) {
                let at = rng.below(world.entities.len() as u64) as usize;
                if let Some(e) = world.entities.get_mut(at) {
                    e.fields = fields(tick * 1000 + i as u32);
                }
            }
            if tick.is_multiple_of(25) {
                world.entities.push(entity(next_id, tick));
                next_id += 1;
            }
            if tick.is_multiple_of(40) && world.entities.len() > 60 {
                let at = rng.below(world.entities.len() as u64) as usize;
                let gone = world.entities.swap_remove(at);
                urgency.remove(&gone.id);
            }
        }
        for e in &mut world.entities {
            let u = urgency.entry(e.id).or_insert(0);
            *u += 1;
            e.priority = *u;
        }
        history.insert(tick, plain(&world));

        let (packet, report) = enc
            .encode_with_report(NetTick(tick), &world, ack, BUDGET)
            .expect("encode");
        assert!(report.bytes <= BUDGET, "seed {seed} tick {tick}: over the budget");
        if report.is_full && !report.complete {
            out.partial_fulls_sent += 1;
        }
        for id in &report.included {
            urgency.insert(*id, 0);
        }
        let bytes = if wrap { resequence(&packet) } else { packet };
        if noisy {
            if !rng.chance(100) {
                down.push((tick + 1 + rng.below(6) as u32, bytes.clone()));
            }
            if rng.chance(30) {
                down.push((tick + 1 + rng.below(8) as u32, bytes));
            }
        } else {
            down.push((tick + 1, bytes));
        }

        // Packets that arrive now, by due tick (a later packet with a shorter delay comes first).
        let (mut due, later): (Vec<_>, Vec<_>) = down.drain(..).partition(|(at, _)| *at <= tick);
        down = later;
        due.sort_by_key(|(at, _)| *at);
        let mut decoded_any = false;
        for (_, bytes) in due {
            let before_seq = dec.last_seq();
            let before_base = dec.latest_baseline();
            let before_world = dec.current().cloned();
            let before_ids = ids(dec.current());
            let before_count = dec.baseline_count();
            let was_rebasing = dec.is_rebasing();
            let d = match dec.decode(&bytes) {
                Ok(d) => d,
                Err(SnapshotError::BaselineMissing { .. }) => {
                    // A late packet whose baseline left the ring: refused, nothing changes.
                    out.baseline_missing += 1;
                    assert_eq!(dec.last_seq(), before_seq, "seed {seed} tick {tick}");
                    assert_eq!(dec.current(), before_world.as_ref(), "seed {seed} tick {tick}");
                    assert_eq!(dec.baseline_count(), before_count, "seed {seed} tick {tick}");
                    continue;
                }
                Err(e) => panic!("seed {seed} tick {tick}: unexpected decode error {e:?}"),
            };
            decoded_any = true;
            let now = dec.last_seq().expect("a decoded packet leaves a last_seq");
            // R1 (a): neither the shown sequence nor the acknowledged baseline moves back.
            if let Some(before) = before_seq {
                assert!(
                    !serial_newer(before, now),
                    "seed {seed} tick {tick}: last_seq moved back from {before:?} to {now:?}"
                );
            }
            if let (Some(before), Some(base)) = (before_base, dec.latest_baseline()) {
                assert!(
                    !serial_newer(before, base),
                    "seed {seed} tick {tick}: latest_baseline moved back from {before:?} to {base:?}"
                );
            }
            if before_seq == Some(now) {
                // Late or repeated: stored as a baseline only.
                out.late += 1;
                assert_eq!(
                    dec.current(),
                    before_world.as_ref(),
                    "seed {seed} tick {tick}: a late packet {:?} changed the shown world",
                    d.seq
                );
                assert_eq!(Some(&d.snapshot), dec.current(), "seed {seed} tick {tick}");
                assert!(
                    d.added.is_empty() && d.updated.is_empty() && d.removed.is_empty(),
                    "seed {seed} tick {tick}: a late packet reported changes"
                );
                continue;
            }
            assert_eq!(d.seq, now, "seed {seed} tick {tick}");
            assert!(
                d.tick.0 > shown_tick,
                "seed {seed} tick {tick}: the shown tick moved back from {shown_tick} to {}",
                d.tick.0
            );
            shown_tick = d.tick.0;
            out.last_wire_seq = now.0;
            assert_eq!(Some(&d.snapshot), dec.current(), "seed {seed} tick {tick}");
            if d.complete {
                // A complete packet is the server's whole world of its tick: both ends' baselines agree.
                out.exact += 1;
                assert!(!dec.is_rebasing(), "seed {seed} tick {tick}");
                assert_eq!(
                    Some(&plain(&d.snapshot)),
                    history.get(&d.tick.0),
                    "seed {seed} tick {tick}: a complete packet {:?} is not the server's world",
                    d.seq
                );
            } else if d.is_full || was_rebasing {
                // R1 (b): a full packet cut short by the budget, and every incomplete packet until
                // the world settles, hides nothing but what the packet itself removed.
                if !before_ids.is_empty() {
                    out.merges += 1;
                }
                let shown = ids(dec.current());
                for id in &before_ids {
                    assert!(
                        d.removed.contains(id) || shown.binary_search(id).is_ok(),
                        "seed {seed} tick {tick}: {id:?} vanished at the cut-short packet {:?} \
                         (full: {}, re-basing before: {was_rebasing})",
                        d.seq,
                        d.is_full
                    );
                }
            }
        }

        // The client acknowledges its newest baseline.
        if decoded_any && let Some(base) = dec.latest_baseline() {
            let seq = Seq(if wrap { from_wire(base.0) } else { base.0 });
            if !noisy {
                up.push((tick + 1, seq));
            } else if !acks_stalled(tick) && !rng.chance(100) {
                up.push((tick + 1 + rng.below(4) as u32, seq));
            }
        }
    }

    // A clean channel and a still world for 150 ticks: the decoder shows exactly the server's world.
    assert!(
        !dec.is_rebasing(),
        "seed {seed}: still re-basing after the clean phase"
    );
    assert_eq!(
        dec.current().map(plain),
        Some(plain(&world)),
        "seed {seed}: the decoder's world differs from the server's after the clean phase"
    );
    out
}

/// R1 (a) and (b) together on a hostile channel: 10 % loss, up to five ticks of reorder, 3 %
/// repeats, 10 % lost acknowledgments that also arrive out of order, and two acknowledgment stalls
/// longer than the baseline ring, under a byte budget of about a sixth of the world. At every
/// decode: `last_seq()`, the shown tick and `latest_baseline()` never move back; a late or repeated
/// packet leaves `current()` untouched; a full packet the budget cut short (and every incomplete
/// packet while re-basing) hides nothing it did not remove; a complete packet is exactly the
/// server's world of its tick. After 200 clean ticks the decoder's world equals the server's.
#[test]
fn loss_reorder_repeats_and_ack_stalls_never_move_the_world_back_or_wipe_it() {
    let mut total = Outcome::default();
    for seed in 1..=12u64 {
        let out = stress(seed, false);
        total.late += out.late;
        total.baseline_missing += out.baseline_missing;
        total.partial_fulls_sent += out.partial_fulls_sent;
        total.merges += out.merges;
        total.acks_backward += out.acks_backward;
        total.exact += out.exact;
        assert!(
            out.partial_fulls_sent > 0,
            "precondition, seed {seed}: no full packet was cut short ({out:?})"
        );
    }
    assert!(total.late > 200, "precondition: too few late packets ({total:?})");
    assert!(
        total.merges > 20,
        "precondition: too few merged packets ({total:?})"
    );
    assert!(
        total.acks_backward > 20,
        "precondition: too few reordered acknowledgments ({total:?})"
    );
    assert!(
        total.exact > 1000,
        "precondition: too few complete packets ({total:?})"
    );
}

/// R1 (a) across the `u32` sequence wrap: the same hostile channel on a stream whose sequences run
/// from `u32::MAX - 40` through `u32::MAX` to 1 and on (0 is never assigned). Order is serial-number
/// order, so a packet from before the wrap that arrives after it is late, not new, and no packet
/// near the wrap is taken for a restarted stream.
#[test]
fn the_sequence_wrap_under_reorder_never_moves_the_world_back() {
    for seed in 21..=26u64 {
        let out = stress(seed, true);
        assert!(
            out.last_wire_seq < WRAP_START && out.last_wire_seq > 400,
            "precondition, seed {seed}: the stream did not pass the wrap ({out:?})"
        );
        assert!(
            out.late > 5,
            "precondition, seed {seed}: too few late packets ({out:?})"
        );
    }
}

/// R1 (b) at a restart. `SnapshotDecoder::decode` documents that a full packet more than
/// `capacity()` sequences behind "is taken as a restarted stream and resets the decoder first".
/// A reset decoder shows only what the new stream's packet carried. Today the implicit restart
/// clears the baselines but keeps the old stream's shown world, so a restarted stream's first
/// packet, cut short by the budget, is merged into the previous server's entities and they stay
/// shown (and `is_rebasing()` reads true) until a complete packet arrives.
#[test]
fn an_implicit_restart_shows_only_the_restarted_stream() {
    let capacity = 4;
    let old_world = WorldSnapshot::new((1000..1010).map(|i| entity(i, i)).collect());
    let mut old = SnapshotEncoder::with_capacity(schema(), capacity);
    let mut dec = SnapshotDecoder::with_capacity(schema(), capacity);
    for tick in 1..=8 {
        let packet = old
            .encode(NetTick(tick), &old_world, None, BIG)
            .expect("old stream");
        dec.decode(&packet).expect("decode the old stream");
    }
    assert_eq!(dec.last_seq(), Some(Seq(8)), "precondition");

    // A new server process: sequences start again at 1, and its first packet does not fit.
    let new_world = WorldSnapshot::new((1..=40).map(|i| entity(i, i)).collect());
    let whole = SnapshotEncoder::with_capacity(schema(), capacity)
        .encode(NetTick(1), &new_world, None, BIG)
        .expect("probe")
        .len();
    let mut new = SnapshotEncoder::with_capacity(schema(), capacity);
    let (packet, report) = new
        .encode_with_report(NetTick(1), &new_world, None, whole / 2)
        .expect("the restarted stream's seq 1");
    assert!(
        report.is_full && !report.complete,
        "precondition: a full packet cut short"
    );

    let mut fresh = SnapshotDecoder::with_capacity(schema(), capacity);
    let expected = fresh
        .decode(&packet)
        .expect("a new decoder takes the packet")
        .snapshot;
    let d = dec
        .decode(&packet)
        .expect("the old decoder takes the packet as a restart");
    assert_eq!(dec.last_seq(), Some(Seq(1)), "precondition: taken as a restart");
    assert!(
        !d.snapshot.entities.iter().any(|e| e.id.0 >= 1000),
        "the previous stream's entities are still shown after the restart: {:?}",
        ids(Some(&d.snapshot))
    );
    assert_eq!(d.snapshot, expected, "a restart shows what a new decoder shows");
    assert_eq!(dec.is_rebasing(), fresh.is_rebasing());
}
