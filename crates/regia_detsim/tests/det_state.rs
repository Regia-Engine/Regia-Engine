// SPDX-License-Identifier: MPL-2.0
//! Saving and resuming a lockstep run (DC-3.7a): a run resumed from a snapshot at tick K steps to
//! the same per-tick hashes as the run that never stopped, and the id allocator, the desync ring
//! and the tick rate come back exactly.

use regia_detsim::arena::{ArenaScenario, ArenaSettings};
use regia_detsim::float::det_clamp;
use regia_detsim::golden::scenarios::{CollideScenario, SlopeScenario, SplitMix64, SteerScenario};
use regia_detsim::golden::{DetRun, DetScenario, TickRate};
use regia_detsim::harness::SimHarness;
use regia_detsim::hash::{RING_TICKS, SystemHashes, SystemRing};
use regia_detsim::ids::{DetHandle, DetId, DetIdAllocator};
use regia_detsim::state::{self, DetState};

/// Per-tick fingerprints of the uninterrupted run: index `t` is the state after `t` steps.
fn uninterrupted<S: DetScenario + DetState>(make: &impl Fn() -> S, run: DetRun) -> Vec<u64> {
    let mut h = SimHarness::new(make(), run).expect("harness");
    let mut out = vec![h.snapshot().fingerprint];
    while h.remaining() > 0 {
        h.step().expect("step");
        out.push(h.snapshot().fingerprint);
    }
    out
}

/// Run to `k`, save, resume in a fresh instance, step to the end; compare every tick from `k`.
fn check_resume<S: DetScenario + DetState>(make: impl Fn() -> S, run: DetRun, ks: &[u32]) {
    let full = uninterrupted(&make, run);
    let name = make().name().to_owned();
    for &k in ks {
        let mut first = SimHarness::new(make(), run).expect("harness");
        first.step_n(k).expect("to k");
        let before = first.snapshot();
        let bytes = first.save().expect("save");
        // Saving changes nothing: the fingerprint and a second save are the same.
        assert_eq!(first.snapshot(), before, "{name}: save changed the state at {k}");
        assert_eq!(
            first.save().expect("save again"),
            bytes,
            "{name}: save not repeatable"
        );
        drop(first);

        let mut resumed = SimHarness::<S>::resume(&bytes).expect("resume");
        assert_eq!(resumed.tick(), k);
        assert_eq!(resumed.run(), run);
        // The restored state is bit-identical: it saves to the same bytes.
        assert_eq!(
            resumed.save().expect("resave"),
            bytes,
            "{name}: restore not bit-identical at {k}"
        );
        let mut got = vec![resumed.snapshot().fingerprint];
        while resumed.remaining() > 0 {
            resumed.step().expect("step");
            got.push(resumed.snapshot().fingerprint);
        }
        let want = &full[k as usize..];
        if let Some(i) = want.iter().zip(&got).position(|(a, b)| a != b) {
            panic!("{name}: resumed at {k}, first diverging tick {}", k as usize + i);
        }
        assert_eq!(got.len(), want.len(), "{name}: resumed run length at {k}");
    }
}

#[test]
fn resume_from_a_snapshot_equals_the_uninterrupted_run() {
    let rate = TickRate::DEFAULT;
    // Steering: the input stream is mid-period at most of these ticks.
    check_resume(
        SteerScenario::new,
        DetRun::new(0x5eed_0001, 300, rate),
        &[0, 1, 29, 30, 31, 157, 299, 300],
    );
    check_resume(
        SlopeScenario::new,
        DetRun::new(0x5eed_0002, 240, rate),
        &[0, 1, 77, 239, 240],
    );
    // Disc collisions: contacts accumulate, so a lost counter or radius shows at once.
    check_resume(
        CollideScenario::new,
        DetRun::new(0x5eed_0003, 300, rate),
        &[0, 1, 64, 150, 299, 300],
    );
    // The AI arena (scratch buffers, cooldowns, seed and tick inside the state).
    let arena = || ArenaScenario::new(ArenaSettings::default()).expect("settings");
    check_resume(arena, DetRun::new(100, 400, rate), &[0, 1, 13, 200, 399, 400]);
    // Another rate: dt is part of the state.
    let hz120 = TickRate::new(120).expect("rate");
    check_resume(CollideScenario::new, DetRun::new(7, 200, hz120), &[0, 99, 200]);
}

#[test]
fn resume_refuses_a_snapshot_of_another_scenario_or_kind() {
    let run = DetRun::new(3, 50, TickRate::DEFAULT);
    let mut h = SimHarness::new(CollideScenario::new(), run).expect("harness");
    h.step_n(10).expect("step");
    let bytes = h.save().expect("save");
    let err = SimHarness::<SteerScenario>::resume(&bytes).err();
    assert_eq!(err.map(|e| e.code()), Some("bad_tag"));
    // A harness frame is not a bare state frame, and the other way round.
    assert_eq!(
        state::restore::<CollideScenario>(&bytes).err().map(|e| e.code()),
        Some("bad_value")
    );
    let bare = state::snapshot(h.state(), 10).expect("bare");
    assert_eq!(
        SimHarness::<CollideScenario>::resume(&bare)
            .err()
            .map(|e| e.code()),
        Some("bad_value")
    );
    // A bare snapshot of the scenario restores to the same body.
    let (tick, back) = state::restore::<CollideScenario>(&bare).expect("restore");
    assert_eq!(tick, 10);
    assert_eq!(
        state::to_bytes(&back).expect("body"),
        state::to_bytes(h.state()).expect("body")
    );
}

/// One seeded call on both allocators; the results must match.
#[derive(Debug, PartialEq)]
enum Call<I: DetId> {
    Alloc(Option<DetHandle<I>>),
    Free(bool),
    Reserve(bool),
}

fn random_call<I: DetId>(
    ids: &mut DetIdAllocator<I>,
    rng: &mut SplitMix64,
    live: &mut Vec<DetHandle<I>>,
) -> Call<I> {
    let cap = ids.cap().to_u32();
    match rng.next_u64() % 10 {
        0..=3 => {
            let h = ids.alloc_handle();
            if let Some(h) = h {
                live.push(h);
            }
            Call::Alloc(h)
        }
        4..=7 if !live.is_empty() => {
            let i = (rng.next_u64() % live.len() as u64) as usize;
            let h = live.swap_remove(i);
            Call::Free(ids.free_handle(h))
        }
        8 if !live.is_empty() => {
            // A stale or current handle, freed again: exercises generations.
            let i = (rng.next_u64() % live.len() as u64) as usize;
            let stale = DetHandle {
                id: live[i].id,
                generation: live[i].generation.wrapping_sub(1),
            };
            Call::Free(ids.free_handle(stale))
        }
        _ => {
            let raw = (rng.next_u64() % u64::from(cap)) as u32 + 1;
            let id = I::from_u32(raw).expect("in range");
            let ok = ids.reserve(id);
            if ok {
                live.push(ids.handle(id).expect("reserved is live"));
            }
            Call::Reserve(ok)
        }
    }
}

fn allocator_round_trip<I: DetId>(cap: I, seed: u64) {
    let mut ids = DetIdAllocator::<I>::new(cap);
    let mut rng = SplitMix64::new(seed);
    let mut live = Vec::new();
    for _ in 0..10_000 {
        random_call(&mut ids, &mut rng, &mut live);
    }
    // Leave a free list whose order matters: free a few live handles, picked by the stream.
    for _ in 0..5 {
        if live.is_empty() {
            break;
        }
        let i = (rng.next_u64() % live.len() as u64) as usize;
        ids.free_handle(live.swap_remove(i));
    }
    assert!(
        ids.free_count() > 0,
        "the run should leave a free list to keep in order"
    );
    let bytes = state::snapshot(&ids, 10_000).expect("snapshot");
    let (_, mut back): (u64, DetIdAllocator<I>) = state::restore(&bytes).expect("restore");
    assert_eq!(back, ids, "the loaded allocator equals the saved one");
    assert_eq!(back.live_count(), ids.live_count());

    // The next 10k calls give identical ids and handles on both.
    let mut rng_b = rng;
    let mut live_b = live.clone();
    for i in 0..10_000 {
        let a = random_call(&mut ids, &mut rng, &mut live);
        let b = random_call(&mut back, &mut rng_b, &mut live_b);
        assert_eq!(a, b, "call {i} after the restore");
    }
    assert_eq!(back, ids);
}

#[test]
fn allocator_round_trip_keeps_fifo_and_generations() {
    // A small cap so the range fills and the free list carries the reuse order.
    allocator_round_trip::<u16>(600, 1);
    allocator_round_trip::<u16>(u16::MAX, 2);
    allocator_round_trip::<u32>(2_000, 3);

    // FIFO order and generations, spelled out.
    let mut ids = DetIdAllocator::<u32>::new(8);
    let a = ids.alloc_handle().expect("a");
    let b = ids.alloc_handle().expect("b");
    let c = ids.alloc_handle().expect("c");
    ids.free(c.id);
    ids.free(a.id);
    let (_, mut back): (u64, DetIdAllocator<u32>) =
        state::restore(&state::snapshot(&ids, 0).expect("snapshot")).expect("restore");
    let first = back.alloc_handle().expect("reuse");
    let second = back.alloc_handle().expect("reuse");
    assert_eq!(
        (first.id, second.id),
        (c.id, a.id),
        "freed ids come back oldest first"
    );
    assert_eq!((first.generation, second.generation), (1, 1));
    assert!(!back.is_current(a) && back.is_current(b));
}

#[test]
fn allocator_load_refuses_broken_invariants() {
    use regia_detsim::state::{DetWriter, SnapshotKind};
    let frame = |body: DetWriter| {
        state::frame_bytes(
            SnapshotKind::State,
            <DetIdAllocator<u16> as DetState>::STATE_TAG,
            1,
            0,
            &body.into_bytes().expect("body"),
        )
        .expect("frame")
    };
    let base = |cap: u32, fresh: u64, free: &[u32], live: &[bool], gens: usize| {
        let mut w = DetWriter::new();
        w.u32(cap);
        w.u64(fresh);
        w.seq(free, |w, id| w.u32(*id));
        w.bits(live);
        w.seq(&vec![0u32; gens], |w, g| w.u32(*g));
        w
    };
    let cases = [
        ("cap 0", base(0, 1, &[], &[false], 1)),
        ("cap past u16", base(70_000, 1, &[], &[false], 1)),
        ("fresh 0", base(4, 0, &[], &[false], 1)),
        ("fresh past cap", base(4, 6, &[], &[false], 1)),
        ("empty tables", base(4, 1, &[], &[], 0)),
        ("table mismatch", base(4, 1, &[], &[false, true], 1)),
        ("table past cap", base(1, 2, &[], &[false, true, false], 3)),
        ("id 0 live", base(4, 2, &[], &[true, true], 2)),
        ("free 0", base(4, 3, &[0], &[false, true, false], 3)),
        ("free past table", base(4, 3, &[3], &[false, true, false], 3)),
        ("free live", base(4, 3, &[1], &[false, true, false], 3)),
        ("free twice", base(4, 3, &[2, 2], &[false, true, false], 3)),
    ];
    for (what, body) in cases {
        let err = state::restore::<DetIdAllocator<u16>>(&frame(body)).err();
        assert_eq!(err.map(|e| e.code()), Some("bad_value"), "{what}");
    }
    // The control: a consistent body loads.
    let ok = base(4, 3, &[2], &[false, true, false], 3);
    let (_, ids) = state::restore::<DetIdAllocator<u16>>(&frame(ok)).expect("consistent");
    assert_eq!((ids.live_count(), ids.free_count()), (1, 1));
}

#[test]
fn system_ring_and_tick_rate_round_trip() {
    let mut ring = SystemRing::<3>::new();
    for tick in 0..(RING_TICKS as u64 + 17) {
        let words = [tick, tick.wrapping_mul(0x9e37_79b9), !tick];
        ring.push(tick, SystemHashes { words });
    }
    let bytes = state::snapshot(&ring, 7).expect("snapshot");
    let (_, back): (u64, SystemRing<3>) = state::restore(&bytes).expect("restore");
    assert_eq!(back.len(), RING_TICKS);
    let last = RING_TICKS as u64 + 16;
    assert_eq!(back.window(last, 96), ring.window(last, 96));
    // Another system count is refused.
    assert_eq!(
        state::restore::<SystemRing<4>>(&bytes).err().map(|e| e.code()),
        Some("bad_value")
    );

    let rate = TickRate::new(144).expect("rate");
    let (_, back): (u64, TickRate) = state::restore(&state::snapshot(&rate, 0).expect("save")).expect("load");
    assert_eq!(back, rate);
    let zero = state::frame_bytes(
        state::SnapshotKind::State,
        TickRate::STATE_TAG,
        TickRate::STATE_VERSION,
        0,
        &0u32.to_le_bytes(),
    )
    .expect("frame");
    assert_eq!(
        state::restore::<TickRate>(&zero).err().map(|e| e.code()),
        Some("bad_value")
    );
}

/// A crowd pushed apart through the grid broadphase: bodies and config are the state, the grid is a
/// cache rebuilt on resume (the pattern the F-07 scale-soak scenario's impl follows).
#[derive(Clone)]
struct Crowd {
    bodies: Vec<regia_detsim::broadphase::BroadBody>,
    vel: Vec<[f32; 2]>,
    config: regia_detsim::broadphase::BroadphaseConfig,
    grid: regia_detsim::broadphase::DetBroadphase,
    rng: SplitMix64,
    contacts: u64,
}

impl Crowd {
    fn new(seed: u64) -> Self {
        use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, DetBroadphase, FloorRule};
        let mut rng = SplitMix64::new(seed);
        let bodies: Vec<BroadBody> = (0..160)
            .map(|i| {
                let mut b = BroadBody::new([rng.signed() * 20.0, 0.0, rng.signed() * 20.0], 0.6, [0.0, 2.0]);
                b.underground = i % 7 == 0;
                b
            })
            .collect();
        let vel = (0..bodies.len()).map(|_| [rng.signed(), rng.signed()]).collect();
        let config = BroadphaseConfig {
            cell_size: 1.5,
            floor_rule: FloorRule::UndergroundOnly,
            ..BroadphaseConfig::default()
        };
        Crowd {
            bodies,
            vel,
            config,
            grid: DetBroadphase::new(config),
            rng,
            contacts: 0,
        }
    }

    fn step(&mut self) {
        let dt = TickRate::DEFAULT.dt();
        for (b, v) in self.bodies.iter_mut().zip(&mut self.vel) {
            v[0] += self.rng.signed() * 0.1;
            v[1] += self.rng.signed() * 0.1;
            b.pos[0] = regia_detsim::float::det_clamp(b.pos[0] + v[0] * dt, -25.0, 25.0);
            b.pos[2] = regia_detsim::float::det_clamp(b.pos[2] + v[1] * dt, -25.0, 25.0);
            b.pos[0] = det_clamp(b.pos[0] + v[0] * dt, -25.0, 25.0);
            b.pos[2] = det_clamp(b.pos[2] + v[1] * dt, -25.0, 25.0);
        }
        let mut contacts = 0u64;
        self.grid.for_each_contact(&mut self.bodies, |bodies, c| {
            contacts += 1;
            let dist = regia_detsim::math::sqrt(c.dist_sq);
            let push = if dist > 1.0e-6 {
                (c.min_dist - dist) * 0.5 / dist
            } else {
                0.0
            };
            bodies[c.a].pos[0] -= c.dx * push;
            bodies[c.a].pos[2] -= c.dz * push;
            bodies[c.b].pos[0] += c.dx * push;
            bodies[c.b].pos[2] += c.dz * push;
        });
        self.contacts += contacts;
    }

    fn hash(&self) -> u64 {
        let mut mix = regia_detsim::hash::Mix::new(7);
        for b in &self.bodies {
            mix.u64(u64::from(b.pos[0].to_bits()));
            mix.u64(u64::from(b.pos[2].to_bits()));
        }
        mix.u64(self.contacts);
        mix.finish()
    }
}

impl DetState for Crowd {
    const STATE_TAG: [u8; 8] = *b"crowd\0\0\0";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut state::DetWriter) {
        w.seq(&self.bodies, |w, b| w.state(b));
        w.seq(&self.vel, |w, v| {
            w.f32(v[0]);
            w.f32(v[1]);
        });
        w.state(&self.config);
        w.state(&self.rng);
        w.u64(self.contacts);
    }
    fn load(r: &mut state::DetReader<'_>) -> Result<Self, state::DetStateError> {
        let bodies = r.seq(|r| r.state())?;
        let vel = r.seq(|r| Ok([r.f32()?, r.f32()?]))?;
        let config: regia_detsim::broadphase::BroadphaseConfig = r.state()?;
        Ok(Crowd {
            bodies,
            vel,
            config,
            grid: regia_detsim::broadphase::DetBroadphase::new(config),
            rng: r.state()?,
            contacts: r.u64()?,
        })
    }
}

#[test]
fn a_rebuilt_broadphase_resumes_the_same_contacts() {
    let ticks = 240;
    let mut full = Crowd::new(11);
    let mut want = vec![full.hash()];
    for _ in 0..ticks {
        full.step();
        want.push(full.hash());
    }
    assert!(
        full.contacts > 100,
        "the crowd should collide: {} contacts",
        full.contacts
    );
    for k in [0usize, 1, 57, 120, 239] {
        let mut first = Crowd::new(11);
        for _ in 0..k {
            first.step();
        }
        let bytes = state::snapshot(&first, k as u64).expect("snapshot");
        drop(first);
        let (_, mut resumed): (u64, Crowd) = state::restore(&bytes).expect("restore");
        let mut got = vec![resumed.hash()];
        for _ in k..ticks {
            resumed.step();
            got.push(resumed.hash());
        }
        assert_eq!(got, want[k..].to_vec(), "resumed at {k}");
    }
}

// --- H1 (review det-core-review-c): hash containers put to bytes that depend on the entries only. ---

mod hashed {
    use std::collections::{HashMap, HashSet};
    use std::hash::{BuildHasher, Hasher};

    use serde::{Deserialize, Serialize};

    use regia_detsim::state::{DetReadLimits, DetReader, DetWriter};

    /// A hasher with an explicit seed, so two maps with different seeds walk their buckets in
    /// different orders on every run (`RandomState` does too, but not reproducibly).
    #[derive(Debug, Clone, Default)]
    pub struct Seeded(pub u64);

    pub struct SeededHasher(u64);

    impl Hasher for SeededHasher {
        fn finish(&self) -> u64 {
            let mut z = self.0;
            z ^= z >> 33;
            z = z.wrapping_mul(0xff51_afd7_ed55_8ccd);
            z ^ (z >> 33)
        }
        fn write(&mut self, bytes: &[u8]) {
            for b in bytes {
                self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }

    impl BuildHasher for Seeded {
        type Hasher = SeededHasher;
        fn build_hasher(&self) -> SeededHasher {
            SeededHasher(self.0.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xcbf2_9ce4_8422_2325)
        }
    }

    pub fn put<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
        let mut w = DetWriter::new();
        w.put(value);
        w.into_bytes().expect("put")
    }

    pub fn take<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> T {
        let mut r = DetReader::new(bytes, DetReadLimits::DEFAULT).expect("reader");
        let value = r.take().expect("take");
        r.finish().expect("every byte read");
        value
    }

    /// The order a `for` loop walks `map` in (the order `put` used to write).
    pub fn walk<K: Copy, V, S>(map: &HashMap<K, V, S>) -> Vec<K> {
        let mut keys = Vec::new();
        // A `for` over the entries: `keys()` is on the crate's disallowed list (hash order).
        for entry in map {
            keys.push(*entry.0);
        }
        keys
    }

    pub fn map_with(seed: u64) -> HashMap<u32, u32, Seeded> {
        let mut m = HashMap::with_hasher(Seeded(seed));
        for i in 0..64u32 {
            m.insert(i.wrapping_mul(2_654_435_761), i);
        }
        m
    }

    pub fn set_with(seed: u64) -> HashSet<u64, Seeded> {
        let mut s = HashSet::with_hasher(Seeded(seed));
        for i in 0..64u64 {
            s.insert(i * 0x1_0000_0001);
        }
        s
    }

    /// A derived game state with hash containers at every depth.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    pub struct World {
        pub tick: u64,
        pub owners: HashMap<String, Vec<u32>, Seeded>,
        pub zones: Vec<HashSet<u16, Seeded>>,
        pub scores: Option<HashMap<u8, f32, Seeded>>,
    }

    pub fn world(seed: u64) -> World {
        let mut owners = HashMap::with_hasher(Seeded(seed));
        let mut zones = Vec::new();
        let mut scores = HashMap::with_hasher(Seeded(seed ^ 7));
        for i in 0..24u32 {
            owners.insert(format!("unit-{i}"), vec![i, i * 3]);
            scores.insert(i as u8, i as f32 * 0.5 - 3.0);
        }
        for z in 0..3u16 {
            let mut zone = HashSet::with_hasher(Seeded(seed.wrapping_add(u64::from(z))));
            for c in 0..20u16 {
                zone.insert(c * 37 + z);
            }
            zones.push(zone);
        }
        World {
            tick: 600,
            owners,
            zones,
            scores: Some(scores),
        }
    }
}

#[test]
fn same_hash_map_entries_put_to_the_same_bytes() {
    // Two seeds: the buckets walk in different orders (so the test bites), the bytes are equal.
    let a = hashed::map_with(1);
    let b = hashed::map_with(2);
    assert_eq!(a, b);
    assert_ne!(
        hashed::walk(&a),
        hashed::walk(&b),
        "the seeds must change the walk"
    );
    assert_eq!(hashed::put(&a), hashed::put(&b));
    // The process's own RandomState: every `HashMap::new()` gets a fresh seed.
    let first = hashed::put(
        &(0..32u32)
            .map(|i| (i, i))
            .collect::<std::collections::HashMap<_, _>>(),
    );
    for _ in 0..16 {
        let again: std::collections::HashMap<u32, u32> = (0..32u32).map(|i| (i, i)).collect();
        assert_eq!(hashed::put(&again), first);
    }
    // The bytes read back into the same entries.
    let back: std::collections::HashMap<u32, u32> = hashed::take(&hashed::put(&a));
    assert_eq!(back.len(), a.len());
    for (k, v) in &back {
        assert_eq!(a.get(k), Some(v));
    }
}

#[test]
fn same_hash_set_entries_put_to_the_same_bytes() {
    let a = hashed::set_with(11);
    let b = hashed::set_with(12);
    assert_eq!(a, b);
    let walk = |s: &std::collections::HashSet<u64, hashed::Seeded>| {
        let mut out = Vec::new();
        for v in s {
            out.push(*v);
        }
        out
    };
    assert_ne!(walk(&a), walk(&b), "the seeds must change the walk");
    assert_eq!(hashed::put(&a), hashed::put(&b));
    let first = hashed::put(&(0..32u64).collect::<std::collections::HashSet<_>>());
    for _ in 0..16 {
        let again: std::collections::HashSet<u64> = (0..32u64).collect();
        assert_eq!(hashed::put(&again), first);
    }
    // Ascending element bytes: the count, then each little-endian u64 by its byte string.
    let bytes = hashed::put(&a);
    assert_eq!(&bytes[..4], &64u32.to_le_bytes());
    let (elements, rest) = bytes[4..].as_chunks::<8>();
    assert!(rest.is_empty());
    assert!(elements.windows(2).all(|w| w[0] < w[1]));
    let back: std::collections::HashSet<u64> = hashed::take(&bytes);
    assert_eq!(back.len(), 64);
    for v in &a {
        assert!(back.contains(v));
    }
}

#[test]
fn a_derived_state_with_nested_hash_containers_puts_the_same_bytes() {
    let a = hashed::world(3);
    let b = hashed::world(4_000_000_007);
    assert_eq!(a, b);
    let bytes = hashed::put(&a);
    assert_eq!(bytes, hashed::put(&b));
    let back: hashed::World = hashed::take(&bytes);
    assert_eq!(back, a);
    // Ordered sequences keep their order: the zone list is a Vec, written zone by zone.
    let mut reversed = hashed::world(3);
    reversed.zones.reverse();
    assert_ne!(hashed::put(&reversed), bytes);
}

// --- R1 (review det-core-review-c): DetRng is a DetState that keeps its stream key. ---

fn assert_det_state<T: DetState>() {}

/// Every kind of draw, from the generator and from its streams (and a stream of a stream).
fn draws(rng: &mut regia_detsim::rng::DetRng) -> Vec<u64> {
    let mut out = Vec::new();
    for _ in 0..8 {
        out.push(rng.next_u64());
    }
    out.push(u64::from(rng.range_u32(1_000)));
    out.push(u64::from(rng.unit_f32().to_bits()));
    out.push(rng.unit_f64().to_bits());
    for id in [0, 1, 3, u64::MAX] {
        let mut s = rng.stream(id);
        out.push(s.next_u64());
        out.push(s.range_u64(97));
        out.push(s.stream(9).next_u64());
    }
    out
}

#[test]
fn det_rng_resumes_its_numbers_and_its_streams() {
    use regia_detsim::rng::DetRng;
    assert_det_state::<DetRng>();

    let mut live = DetRng::seed_from(0x5eed).stream(4);
    for _ in 0..37 {
        live.next_u64();
    }
    let bytes = state::snapshot(&live, 37).expect("snapshot");
    let (tick, mut resumed): (u64, DetRng) = state::restore(&bytes).expect("restore");
    assert_eq!(tick, 37);
    assert_eq!(resumed, live);
    // Restore, then draw: the same numbers and the same streams as the generator that never stopped.
    let mut uninterrupted = live.clone();
    assert_eq!(draws(&mut resumed), draws(&mut uninterrupted));

    // The body is the key, then the four words, and equals the serde form `put` writes.
    let body = state::to_bytes(&live).expect("body");
    let mut expected = live.key().to_le_bytes().to_vec();
    for word in live.state() {
        expected.extend_from_slice(&word.to_le_bytes());
    }
    assert_eq!(body, expected);
    let mut w = state::DetWriter::new();
    w.put(&live);
    assert_eq!(w.into_bytes().expect("put"), body);

    // The hazard R1 names: the four words alone (any other key) draw the same `next_u64`, but every
    // stream is a different generator. Saving the key is what keeps them.
    let words_only = DetRng::from_state(0, live.state()).expect("nonzero state");
    assert_eq!(words_only.clone().next_u64(), live.clone().next_u64());
    assert_ne!(words_only.stream(1).next_u64(), live.stream(1).next_u64());
}

#[test]
fn det_rng_load_refuses_the_all_zero_state() {
    use regia_detsim::rng::DetRng;
    let mut body = 0x5eed_u64.to_le_bytes().to_vec();
    body.extend_from_slice(&[0; 32]);
    let err = state::from_bytes::<DetRng>(&body, state::DetReadLimits::DEFAULT).err();
    assert_eq!(err.as_ref().map(|e| e.code()), Some("bad_value"), "{err:?}");
    assert!(format!("{err:?}").contains("det_rng_zero_state"), "{err:?}");
    // One byte short is truncated, one byte long is trailing input.
    let good = state::to_bytes(&DetRng::seed_from(1)).expect("body");
    assert!(state::from_bytes::<DetRng>(&good[..39], state::DetReadLimits::DEFAULT).is_err());
    let mut long = good.clone();
    long.push(0);
    assert!(state::from_bytes::<DetRng>(&long, state::DetReadLimits::DEFAULT).is_err());
    assert_eq!(
        state::from_bytes::<DetRng>(&good, state::DetReadLimits::DEFAULT).expect("load"),
        DetRng::seed_from(1)
    );
}

// --- H2/H3 (review det-core-review-c): the fingerprint covers every word the snapshot body stores. ---

fn fingerprint_of(s: &dyn DetScenario) -> u64 {
    let mut h = regia_detsim::golden::StateHasher::new();
    s.hash_state(&mut h);
    h.finish()
}

/// Flip the low bit of every 4-byte word of the saved body. Each flip that still loads must move
/// the fingerprint: a stored word the fingerprint skips is a desync the hash cannot see
/// (`DetScenario::hash_state`: "feed every word that a later tick reads"; the snapshot body holds
/// exactly what the next step reads, which `resume_from_a_snapshot_equals_the_uninterrupted_run`
/// proves). Returns how many flipped bodies loaded.
fn body_words_move_the_fingerprint<S: DetScenario + DetState>(
    make: impl Fn() -> S,
    run: DetRun,
    at: u32,
) -> usize {
    let mut h = SimHarness::new(make(), run).expect("harness");
    h.step_n(at).expect("step");
    let body = state::to_bytes(h.state()).expect("body");
    assert!(
        body.len().is_multiple_of(4),
        "every field of these bodies is 4 or 8 bytes"
    );
    let base = fingerprint_of(h.state());
    let mut loaded = 0;
    let mut blind = Vec::new();
    for word in 0..body.len() / 4 {
        let mut flipped = body.clone();
        flipped[word * 4] ^= 1;
        let Ok(back) = state::from_bytes::<S>(&flipped, state::DetReadLimits::DEFAULT) else {
            continue; // a length prefix: the reader refuses the body
        };
        loaded += 1;
        if fingerprint_of(&back) == base {
            blind.push(word);
        }
    }
    assert!(
        blind.is_empty(),
        "{}: body words {blind:?} do not move the fingerprint",
        make().name()
    );
    loaded
}

#[test]
fn every_stored_word_moves_the_fingerprint() {
    let run = |seed| DetRun::new(seed, 120, TickRate::DEFAULT);
    // steer: 4 cars x (7 tuning + 6 pose), 4 x 3 inputs, the stream (2 words) and dt.
    assert_eq!(
        body_words_move_the_fingerprint(SteerScenario::new, run(11), 45),
        4 * 13 + 4 * 3 + 2 + 1
    );
    // slope: 4 terrain words, 8 sleds x 6, dt.
    assert_eq!(
        body_words_move_the_fingerprint(SlopeScenario::new, run(12), 45),
        4 + 8 * 6 + 1
    );
    // collide: 24 discs x 5, the contact count and dt.
    assert_eq!(
        body_words_move_the_fingerprint(CollideScenario::new, run(13), 45),
        24 * 5 + 1 + 1
    );
}
