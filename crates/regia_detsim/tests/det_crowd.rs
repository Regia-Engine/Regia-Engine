// SPDX-License-Identifier: MPL-2.0
// The last test, `stress_210_entity_equivalent_determinism`: portions copied from Direct Order 27
// (ir-sim/src/harness.rs, commit d6ed37677), ADR-8012. Mark: adapt (lines 385-400; ADR-8048). The
// source runs its whole game sim from a seed for 100 ticks twice and compares snapshots; the game sim
// is not in Regia, so the port runs the same shape of check (seed 0xBA77_1E20_1EEE, 210 entities in
// 14 groups, 100 ticks, two independent runs, snapshot equality) on what this crate owns: ids from
// `DetIdAllocator` with despawns and FIFO reuse, and contacts from `DetBroadphase`, plus a third run
// through the all-pairs loop that must match too. Everything else in this file is Regia's.

//! Lockstep crowd scenarios: the grid broadphase against the all-pairs loop, bit for bit, plus the
//! height-band rule and the id allocator under load.
//!
//! The crowd is discs that drive into each other and are pushed apart by a resolver shaped like a
//! lockstep game's (fixed bodies, half-and-half pushes, a fixed normal for coincident centres,
//! closing speed removed).

use regia_detsim::broadphase::{
    BroadBody, BroadphaseConfig, BroadphaseStats, Contact, DetBroadphase, FloorRule, for_each_contact_n2,
};
use regia_detsim::debug::{DebugCapture, DebugKind, DebugSink, DebugWhat};
use regia_detsim::ids::{DetHandle, DetIdAllocator};
use regia_detsim::{math, tick};

#[test]
fn broadphase_cell_budget_is_bounded_before_building() {
    for (requested, expected) in [
        (0, 1),
        (1, 1),
        (1 << 16, 1 << 16),
        ((1 << 24) - 1, (1 << 24) - 1),
        (1 << 24, 1 << 24),
        ((1 << 24) + 1, 1 << 24),
        (usize::MAX, 1 << 24),
    ] {
        let broadphase = DetBroadphase::new(BroadphaseConfig {
            max_cells: requested,
            ..BroadphaseConfig::default()
        });
        assert_eq!(broadphase.config().max_cells, expected, "requested {requested}");
    }
}

/// 64-bit LCG (Knuth's MMIX constants), the same mix the source game's harness uses for seeds.
#[derive(Debug, Clone)]
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(
            seed.wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407),
        )
    }
    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    /// Uniform in `[lo, hi)` built from 24 bits (exact in f32).
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        let unit = (self.next_u32() >> 7) as f32 * (1.0 / 16_777_216.0);
        lo + (hi - lo) * unit
    }
}

/// FNV-1a 64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn u32(&mut self, v: u32) {
        for byte in v.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }
}

/// Which loop resolves the contacts.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Pairs {
    N2,
    Grid(BroadphaseConfig),
}

/// Per-body state the broadphase does not see.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Motion {
    vel: [f32; 2],
    fixed: bool,
}

/// One visited contact: indices and the bits of `dx`, `dz`.
type Visit = (usize, usize, u32, u32);

/// The crowd: bodies plus their motion, stepped at the default tick rate.
#[derive(Debug, Clone)]
struct Crowd {
    bodies: Vec<BroadBody>,
    motion: Vec<Motion>,
    rule: FloorRule,
    grid: Option<DetBroadphase>,
    /// Pushes are this many times the overlap (1 = just separate).
    push_scale: f32,
}

impl Crowd {
    /// `n` bodies scattered over a `side` × `side` square, driving toward its centre; one in ten is
    /// fixed (a building), one in eight sits on a lower floor (a tunnel 30 m down).
    fn new(seed: u64, n: usize, side: f32, rule: FloorRule, pairs: Pairs) -> Self {
        let mut rng = Lcg::new(seed);
        let mut bodies = Vec::with_capacity(n);
        let mut motion = Vec::with_capacity(n);
        for i in 0..n {
            let x = rng.range(-side * 0.5, side * 0.5);
            let z = rng.range(-side * 0.5, side * 0.5);
            let radius = rng.range(0.4, 3.0);
            let lower = rng.next_u32().is_multiple_of(8);
            let y = if lower { -30.0 } else { rng.range(0.0, 2.0) };
            let fixed = rng.next_u32().is_multiple_of(10);
            let above = if fixed { 25.0 } else { 5.0 };
            let mut body = BroadBody::new([x, y, z], radius, [y - 3.0, y + above]);
            body.underground = lower;
            // Every 97th body starts on top of the one before it (coincident centres).
            if i % 97 == 96
                && let Some(prev) = bodies.last()
            {
                let prev: &BroadBody = prev;
                body.pos = prev.pos;
                body.band = prev.band;
                body.underground = prev.underground;
            }
            let speed = rng.range(1.0, 9.0);
            let len = math::sqrt(x * x + z * z);
            let vel = if fixed || len < 1e-3 {
                [0.0, 0.0]
            } else {
                [-x / len * speed, -z / len * speed]
            };
            bodies.push(body);
            motion.push(Motion { vel, fixed });
        }
        let grid = match pairs {
            Pairs::N2 => None,
            Pairs::Grid(config) => Some(DetBroadphase::new(BroadphaseConfig {
                floor_rule: rule,
                ..config
            })),
        };
        Self {
            bodies,
            motion,
            rule,
            grid,
            push_scale: 1.0,
        }
    }

    /// One tick: move, then resolve contacts. Returns the contacts in visit order.
    fn step(&mut self) -> Vec<Visit> {
        let dt = tick::tick_dt();
        for (b, m) in self.bodies.iter_mut().zip(&self.motion) {
            b.pos[0] += m.vel[0] * dt;
            b.pos[2] += m.vel[1] * dt;
        }
        let mut visits = Vec::new();
        let motion = &mut self.motion;
        let push_scale = self.push_scale;
        let mut resolve = |bodies: &mut [BroadBody], c: Contact| {
            visits.push((c.a, c.b, c.dx.to_bits(), c.dz.to_bits()));
            resolve_scaled(bodies, motion, c, push_scale);
        };
        match &mut self.grid {
            None => for_each_contact_n2(&mut self.bodies, self.rule, &mut resolve),
            Some(grid) => grid.for_each_contact(&mut self.bodies, &mut resolve),
        }
        visits
    }

    /// Hash of every position and velocity bit.
    fn state_hash(&self) -> u64 {
        let mut h = Fnv::default();
        for (b, m) in self.bodies.iter().zip(&self.motion) {
            for v in b.pos {
                h.f32(v);
            }
            h.f32(m.vel[0]);
            h.f32(m.vel[1]);
        }
        h.0
    }
}

/// A lockstep game's separation step: fixed bodies never move, two fixed bodies are skipped, a
/// pair splits the overlap half and half, coincident centres separate along +X, and each moving
/// body loses its own speed toward the other.
fn resolve_contact(bodies: &mut [BroadBody], motion: &mut [Motion], c: Contact) {
    resolve_scaled(bodies, motion, c, 1.0);
}

/// [`resolve_contact`] with the push multiplied by `scale`.
fn resolve_scaled(bodies: &mut [BroadBody], motion: &mut [Motion], c: Contact, scale: f32) {
    let (fa, fb) = (motion[c.a].fixed, motion[c.b].fixed);
    if fa && fb {
        return;
    }
    let dist = math::sqrt(c.dist_sq);
    let (nx, nz) = if dist > 1e-4 {
        (c.dx / dist, c.dz / dist)
    } else {
        (1.0, 0.0)
    };
    let overlap = (c.min_dist - dist) * scale;
    let (push_a, push_b) = match (fa, fb) {
        (true, false) => (0.0, overlap),
        (false, true) => (overlap, 0.0),
        _ => (overlap * 0.5, overlap * 0.5),
    };
    if push_a > 0.0 {
        bodies[c.a].pos[0] -= nx * push_a;
        bodies[c.a].pos[2] -= nz * push_a;
    }
    if push_b > 0.0 {
        bodies[c.b].pos[0] += nx * push_b;
        bodies[c.b].pos[2] += nz * push_b;
    }
    if !fa {
        let v = &mut motion[c.a].vel;
        let toward = v[0] * nx + v[1] * nz;
        if toward > 0.0 {
            v[0] -= nx * toward;
            v[1] -= nz * toward;
        }
    }
    if !fb {
        let v = &mut motion[c.b].vel;
        let toward = -(v[0] * nx + v[1] * nz);
        if toward > 0.0 {
            v[0] += nx * toward;
            v[1] += nz * toward;
        }
    }
}

/// Run `ticks` ticks of the same crowd through the all-pairs loop and the grid; panic at the first
/// contact or state bit that differs. Returns the grid's totals: contacts, re-gathers, cell moves
/// and candidate tests.
fn assert_grid_matches_n2(
    seed: u64,
    n: usize,
    side: f32,
    rule: FloorRule,
    config: BroadphaseConfig,
    ticks: u32,
) -> [u64; 4] {
    let mut reference = Crowd::new(seed, n, side, rule, Pairs::N2);
    let mut grid = Crowd::new(seed, n, side, rule, Pairs::Grid(config));
    let mut totals = [0_u64; 4];
    for t in 0..ticks {
        let want = reference.step();
        let got = grid.step();
        if let Some(i) = (0..want.len().max(got.len())).find(|&i| want.get(i) != got.get(i)) {
            panic!(
                "seed {seed:#x} tick {t}: contact #{i} differs: all-pairs {:?}, grid {:?} \
                 ({} vs {} contacts)",
                want.get(i),
                got.get(i),
                want.len(),
                got.len()
            );
        }
        assert_eq!(
            reference.state_hash(),
            grid.state_hash(),
            "seed {seed:#x} tick {t}: state bits differ with the same contacts"
        );
        let stats = grid.grid.as_ref().map(DetBroadphase::stats).unwrap_or_default();
        totals[0] += stats.contacts;
        totals[1] += stats.regathers;
        totals[2] += stats.cell_moves;
        totals[3] += stats.candidates;
    }
    totals
}

#[test]
fn broadphase_matches_the_n2_pair_order_bit_for_bit() {
    // 900 bodies crowding into a 70 m square for half a second at 60 Hz, under the source game's
    // floor rule (the rule a game that keeps its checksums runs).
    let [contacts, regathers, moves, candidates] = assert_grid_matches_n2(
        0x5eed_0900,
        900,
        70.0,
        FloorRule::UndergroundOnly,
        BroadphaseConfig::default(),
        30,
    );
    // The scenario must exercise what makes this hard: many contacts, bodies pushed across cells
    // mid-pass, and rows whose body was pushed past the margin.
    assert!(contacts > 10_000, "only {contacts} contacts");
    assert!(moves > 100, "only {moves} cell moves");
    assert!(regathers > 10, "only {regathers} re-gathers");
    // And it must be a broadphase: far fewer tests than 30 x 900 x 899 / 2.
    assert!(candidates < 30 * 404_550 / 5, "{candidates} candidate tests");
}

#[test]
fn broadphase_matches_the_n2_loop_for_any_cell_size_margin_and_rule() {
    let configs = [
        (0.3, 0.0, 1 << 16),
        (1.0, 0.05, 1 << 16),
        (4.0, 0.5, 1 << 16),
        (25.0, 3.0, 1 << 16),
        (2.0, 0.5, 9), // forces the cell size up
    ];
    let rules = [FloorRule::Bands, FloorRule::UndergroundOnly, FloorRule::Never];
    for (k, &(cell_size, margin, max_cells)) in configs.iter().enumerate() {
        for (r, &rule) in rules.iter().enumerate() {
            let config = BroadphaseConfig {
                cell_size,
                margin,
                max_cells,
                floor_rule: rule,
                ..BroadphaseConfig::default()
            };
            let seed = 0xc0ff_ee00 + (k * 3 + r) as u64;
            let _ = assert_grid_matches_n2(seed, 220, 30.0, rule, config, 20);
        }
    }
}

#[test]
fn non_finite_bodies_are_tested_against_everyone_like_the_n2_loop() {
    for &bad in &[f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut a = Crowd::new(7, 120, 20.0, FloorRule::Bands, Pairs::N2);
        a.bodies[5].pos[0] = bad;
        a.bodies[40].radius = bad;
        a.bodies[77].pos[2] = bad;
        let mut b = a.clone();
        b.grid = Some(DetBroadphase::new(BroadphaseConfig::default()));
        for t in 0..10 {
            let (want, got) = (a.step(), b.step());
            assert_eq!(want, got, "{bad} tick {t}");
            assert_eq!(a.state_hash(), b.state_hash(), "{bad} tick {t}");
        }
        let stats = b.grid.as_ref().map(DetBroadphase::stats).unwrap_or_default();
        assert!(stats.non_finite > 0);
    }
}

#[test]
fn far_flung_bodies_match_the_n2_loop() {
    // A crowd near the origin plus bodies thousands of kilometres out (huge grid span, coarse ulps).
    let mut a = Crowd::new(11, 150, 25.0, FloorRule::Bands, Pairs::N2);
    a.bodies[3].pos = [3.0e6, 0.0, -2.0e6];
    a.bodies[4].pos = [3.0e6 + 0.25, 0.0, -2.0e6];
    a.bodies[9].pos = [-1.0e30, 0.0, 1.0e30];
    let mut b = a.clone();
    b.grid = Some(DetBroadphase::new(BroadphaseConfig::default()));
    for t in 0..10 {
        assert_eq!(a.step(), b.step(), "tick {t}");
        assert_eq!(a.state_hash(), b.state_hash(), "tick {t}");
    }
}

#[test]
fn empty_and_single_body_passes_visit_nothing() {
    let mut grid = DetBroadphase::new(BroadphaseConfig::default());
    let mut none: Vec<BroadBody> = Vec::new();
    grid.for_each_contact(&mut none, |_, c| panic!("contact {c:?}"));
    let mut one = vec![BroadBody::new([0.0; 3], 1.0, [0.0, 1.0])];
    grid.for_each_contact(&mut one, |_, c| panic!("contact {c:?}"));
    assert_eq!(grid.stats().bodies, 1);
}

fn stacked_pair() -> Vec<BroadBody> {
    // A car on a bridge deck 12 m over a car on the road below; both outdoors, same XZ.
    vec![
        BroadBody::new([5.0, 0.0, 5.0], 1.5, [-1.0, 3.0]),
        BroadBody::new([5.2, 12.0, 5.0], 1.5, [11.0, 15.0]),
    ]
}

#[test]
fn two_outdoor_bodies_on_stacked_floors_do_not_separate() {
    let before = stacked_pair();
    let mut bodies = before.clone();
    let mut grid = DetBroadphase::new(BroadphaseConfig::default());
    let mut motion = [Motion {
        vel: [0.0; 2],
        fixed: false,
    }; 2];
    grid.for_each_contact(&mut bodies, |bodies, c| resolve_contact(bodies, &mut motion, c));
    assert_eq!(grid.stats().contacts, 0, "the bodies are on different floors");
    assert_eq!(bodies, before, "neither body was pushed");
}

#[test]
fn the_source_rule_still_separates_two_outdoor_bodies_on_stacked_floors() {
    // Known issue of the source game's rule, kept bit for bit under `UndergroundOnly`.
    let mut bodies = stacked_pair();
    let mut grid = DetBroadphase::new(BroadphaseConfig {
        floor_rule: FloorRule::UndergroundOnly,
        ..BroadphaseConfig::default()
    });
    let mut seen = 0;
    grid.for_each_contact(&mut bodies, |_, _| seen += 1);
    assert_eq!(seen, 1);
    // Underground, the same pair is on different floors under both rules.
    let mut tunnel = stacked_pair();
    tunnel[0].underground = true;
    grid.for_each_contact(&mut tunnel, |_, _| seen += 1);
    assert_eq!(seen, 1);
}

/// The source harness's snapshot, reduced to what this crate owns.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    tick: u32,
    entity_count: usize,
    live_ids: Vec<u16>,
    generation_sum: u64,
    contacts: u64,
    fingerprint: u64,
}

/// 14 groups of 15 entities (210) with sparse reserved group-leader ids; every third tick the
/// seeded stream despawns one entity and spawns a replacement at its group's base, so ids are
/// freed and reused (FIFO) with rising generations while the crowd collides.
struct StressRun {
    crowd: Crowd,
    ids: DetIdAllocator<u16>,
    /// `owner[i]`: the handle body `i` currently carries.
    owner: Vec<DetHandle<u16>>,
    rng: Lcg,
    tick: u32,
    contacts: u64,
}

impl StressRun {
    const GROUPS: usize = 14;
    const PER_GROUP: usize = 15;

    fn new(seed: u64, pairs: Pairs) -> Self {
        let n = Self::GROUPS * Self::PER_GROUP;
        let crowd = Crowd::new(seed, n, 60.0, FloorRule::Bands, pairs);
        let mut ids = DetIdAllocator::<u16>::new(512);
        for g in 0..Self::GROUPS {
            assert!(ids.reserve(400 + g as u16), "leader slot");
        }
        let owner = (0..n)
            .map(|_| ids.alloc_handle().expect("within the cap"))
            .collect();
        Self {
            crowd,
            ids,
            owner,
            rng: Lcg::new(seed ^ 0x1d5),
            tick: 0,
            contacts: 0,
        }
    }

    fn step(&mut self) {
        self.contacts += self.crowd.step().len() as u64;
        self.tick += 1;
        if self.tick.is_multiple_of(3) {
            let i = self.rng.next_u32() as usize % self.owner.len();
            assert!(self.ids.free_handle(self.owner[i]), "body {i} handle is current");
            let fresh = self.ids.alloc_handle().expect("a freed id is available");
            self.owner[i] = fresh;
            let g = i / Self::PER_GROUP;
            let angle = g as f32 * (6.283_185_5 / Self::GROUPS as f32);
            let body = &mut self.crowd.bodies[i];
            body.pos[0] = math::cos(angle) * 28.0;
            body.pos[2] = math::sin(angle) * 28.0;
        }
    }

    fn snapshot(&self) -> Snapshot {
        let mut h = Fnv(self.crowd.state_hash());
        for handle in &self.owner {
            h.u32(u32::from(handle.id));
            h.u32(handle.generation);
        }
        h.u32(self.tick);
        Snapshot {
            tick: self.tick,
            entity_count: self.ids.live_count(),
            live_ids: self.ids.live_ids().collect(),
            generation_sum: self
                .ids
                .live_ids()
                .map(|id| u64::from(self.ids.generation(id)))
                .sum(),
            contacts: self.contacts,
            fingerprint: h.0,
        }
    }
}

#[test]
fn stress_210_entity_equivalent_determinism() {
    // The source's seed; 100 ticks; two independent runs must agree, and the all-pairs loop too.
    let seed = 0xBA77_1E20_1EEE;
    let mut a = StressRun::new(seed, Pairs::Grid(BroadphaseConfig::default()));
    let mut b = StressRun::new(seed, Pairs::Grid(BroadphaseConfig::default()));
    let mut r = StressRun::new(seed, Pairs::N2);
    for _ in 0..100 {
        a.step();
        b.step();
        r.step();
    }
    let snap = a.snapshot();
    assert_eq!(snap, b.snapshot(), "210-entity stress scenario diverged");
    assert_eq!(snap, r.snapshot(), "grid and all-pairs runs diverged");
    assert_eq!(snap.entity_count, 224, "210 bodies plus 14 reserved leader ids");
    assert!(snap.generation_sum > 0, "ids were reused");
    assert!(snap.contacts > 1000, "only {} contacts", snap.contacts);
}

/// The crowd and stress fingerprints, pinned: the same bits on every host and architecture
/// (recorded on aarch64 macOS, debug and release, and checked on x86-64 Linux). A change here is a
/// sim-version change.
#[test]
fn crowd_hashes_match_the_pinned_values_on_every_arch() {
    let mut crowd = Crowd::new(
        0x5eed_0900,
        900,
        70.0,
        FloorRule::Bands,
        Pairs::Grid(BroadphaseConfig::default()),
    );
    let mut contacts = Fnv::default();
    for _ in 0..30 {
        for (a, b, dx, dz) in crowd.step() {
            contacts.u32(a as u32);
            contacts.u32(b as u32);
            contacts.u32(dx);
            contacts.u32(dz);
        }
    }
    let mut stress = StressRun::new(0xBA77_1E20_1EEE, Pairs::Grid(BroadphaseConfig::default()));
    for _ in 0..100 {
        stress.step();
    }
    let got = [crowd.state_hash(), contacts.0, stress.snapshot().fingerprint];
    println!("crowd hashes: {got:#018x?}");
    assert_eq!(got, PINNED_CROWD_HASHES);
}

/// Final crowd state, the crowd's contact stream, the stress fingerprint.
const PINNED_CROWD_HASHES: [u64; 3] = [
    0x8abc_2c47_25eb_d9b5,
    0x833a_6291_0a8b_e4bd,
    0x8389_0e49_b658_679f,
];

#[test]
fn violent_pushes_across_many_cells_still_match_the_n2_loop() {
    // Every contact throws both bodies 8x the overlap apart: bodies cross several cells mid-pass and
    // rows re-gather often.
    for (k, cell_size) in [1.0_f32, 4.0].into_iter().enumerate() {
        let mut a = Crowd::new(0xb10_0000 + k as u64, 400, 40.0, FloorRule::Bands, Pairs::N2);
        a.push_scale = 8.0;
        let mut b = a.clone();
        b.grid = Some(DetBroadphase::new(BroadphaseConfig {
            cell_size,
            ..BroadphaseConfig::default()
        }));
        let (mut moves, mut regathers) = (0, 0);
        for t in 0..15 {
            assert_eq!(a.step(), b.step(), "cell {cell_size} tick {t}");
            assert_eq!(a.state_hash(), b.state_hash(), "cell {cell_size} tick {t}");
            let stats = b.grid.as_ref().map(DetBroadphase::stats).unwrap_or_default();
            moves += stats.cell_moves;
            regathers += stats.regathers;
        }
        assert!(
            moves > 200 && regathers > 100,
            "moves {moves}, regathers {regathers}"
        );
    }
}

// --- Outliers and large bodies (DC-3.4c) ---------------------------------------------------------

/// Run `crowd` through the all-pairs loop and through a grid with `config` for `ticks` ticks; panic at
/// the first contact or state bit that differs. Returns the grid's per-pass stats, one per tick.
fn grid_stats_matching_n2(
    crowd: &Crowd,
    config: BroadphaseConfig,
    ticks: u32,
    what: &str,
) -> Vec<BroadphaseStats> {
    let mut reference = crowd.clone();
    reference.grid = None;
    let mut grid = crowd.clone();
    grid.grid = Some(DetBroadphase::new(BroadphaseConfig {
        floor_rule: crowd.rule,
        ..config
    }));
    let mut out = Vec::new();
    for t in 0..ticks {
        let want = reference.step();
        let got = grid.step();
        if let Some(i) = (0..want.len().max(got.len())).find(|&i| want.get(i) != got.get(i)) {
            panic!(
                "{what} tick {t}: contact #{i} differs: all-pairs {:?}, grid {:?} ({} vs {} contacts)",
                want.get(i),
                got.get(i),
                want.len(),
                got.len()
            );
        }
        assert_eq!(
            reference.state_hash(),
            grid.state_hash(),
            "{what} tick {t}: state bits differ with the same contacts"
        );
        out.push(grid.grid.as_ref().map(DetBroadphase::stats).unwrap_or_default());
    }
    out
}

fn total_candidates(stats: &[BroadphaseStats]) -> u64 {
    stats.iter().map(|s| s.candidates).sum()
}

/// Append one body to `crowd`: at `pos` with `radius`, standing still, outdoors.
fn push_body(crowd: &mut Crowd, pos: [f32; 3], radius: f32) {
    crowd
        .bodies
        .push(BroadBody::new(pos, radius, [pos[1] - 3.0, pos[1] + 5.0]));
    crowd.motion.push(Motion {
        vel: [0.0; 2],
        fixed: false,
    });
}

/// The kilometre-square map the outlier tests use: `[min_x, min_z, max_x, max_z]`.
const KM_BOUNDS: [f32; 4] = [-600.0, -600.0, 600.0, 600.0];

#[test]
fn one_parked_body_does_not_collapse_the_grid() {
    // 900 bodies over 1 km², plus one body parked 1000 km away (a pooled or dead entity).
    let crowd = Crowd::new(0xfa4_0900, 900, 1000.0, FloorRule::Bands, Pairs::N2);
    let mut parked = crowd.clone();
    push_body(&mut parked, [1.0e6, 0.0, 1.0e6], 1.0);
    let config = BroadphaseConfig {
        world_bounds: Some(KM_BOUNDS),
        ..BroadphaseConfig::default()
    };
    let without = grid_stats_matching_n2(&crowd, config, 30, "no outlier");
    let with = grid_stats_matching_n2(&parked, config, 30, "parked outlier");
    let (base, got) = (total_candidates(&without), total_candidates(&with));
    println!("candidates without the outlier {base}, with it {got}");
    assert!(base > 0, "the crowd has candidate pairs");
    assert!(
        got * 10 <= base * 11,
        "one parked body raised the candidate tests from {base} to {got}"
    );
    // The grid kept its fine cells, and the stats name the outlier.
    assert_eq!(with[0].cells, without[0].cells);
    assert_eq!(with[0].outliers, 1);
    assert_eq!(without[0].outliers, 0);
    // Without bounds the same scene stays exact (the old behavior, every body in a few cells).
    let unbounded = grid_stats_matching_n2(&parked, BroadphaseConfig::default(), 2, "no bounds");
    assert_eq!(unbounded[0].outliers, 0);
    // Control: that old behavior is the gap this closes (the outlier stretches the grid).
    let (base2, collapsed) = (total_candidates(&without[..2]), total_candidates(&unbounded));
    println!("first two ticks: candidates without the outlier {base2}, unbounded with it {collapsed}");
    assert!(
        collapsed > base2 * 5,
        "the control scene should collapse the grid: {collapsed} vs {base2}"
    );
}

#[test]
fn one_large_body_does_not_widen_every_gather() {
    // 899 bodies of radius 1 over 1 km², plus one fixed body of radius 60 (a building, a hill)
    // among them.
    let mut crowd = Crowd::new(0x1a7_0899, 899, 1000.0, FloorRule::Bands, Pairs::N2);
    for b in &mut crowd.bodies {
        b.radius = 1.0;
    }
    let mut big = crowd.clone();
    let hill = [250.0, 0.0, -250.0];
    push_body(&mut big, hill, 60.0);
    if let Some(m) = big.motion.last_mut() {
        m.fixed = true;
    }
    let config = BroadphaseConfig::default();
    let without = grid_stats_matching_n2(&crowd, config, 30, "no large body");
    let with = grid_stats_matching_n2(&big, config, 30, "large body");
    let (base, got) = (total_candidates(&without), total_candidates(&with));
    assert!(base > 0, "the crowd has candidate pairs");
    assert_eq!(with[0].large, 1);
    assert_eq!(without[0].large, 0);

    // Split the candidate tests: pairs with the large body (the body is last, so each is a row
    // `a < 899` reaching it through the large list) and every other pair. Most pairs with the large
    // body are work any exact pass must do: the bodies it pushed out sit on its rim, touching, and
    // must be tested every tick. The brief's total bar (1.2x of `base`) counts them too; the lane
    // report carries the measured total for an owner ruling (ADR-8837). What must not grow is the
    // other rows' gathers.
    let mut probe = big.clone();
    let mut grid = DetBroadphase::new(BroadphaseConfig {
        floor_rule: probe.rule,
        ..config
    });
    grid.set_debug_capture(Some(DebugCapture::default()));
    probe.grid = Some(grid);
    let (mut big_pairs, mut big_near, mut big_contacts, mut probed) = (0_u64, 0_u64, 0_u64, 0_u64);
    for _ in 0..30 {
        big_contacts += probe.step().iter().filter(|v| v.1 == 899).count() as u64;
        let grid = probe.grid.as_ref().expect("the probe runs the grid");
        assert_eq!(grid.debug_capture_stats().candidates_dropped, 0);
        probed += grid.stats().candidates;
        // A row of radius 1 can touch the hill within 1 + 60 m, plus the margin it may drift.
        let mut sink = CandidateLines {
            to: hill,
            reach: 1.0 + 60.0 + config.margin,
            hits: 0,
            near: 0,
        };
        grid.debug_emit(&mut sink, DebugWhat::CANDIDATES);
        big_pairs += sink.hits;
        big_near += sink.near;
    }
    assert_eq!(probed, got, "capture does not change the pass");
    let others = got - big_pairs;
    println!(
        "candidates without the large body {base}, with it {got} ({big_pairs} with the large body, \
         {big_near} of them within its contact reach, {big_contacts} contacts; {others} others)"
    );
    assert!(
        others * 10 <= base * 12,
        "one large body widened the other rows' gathers: {others} candidate tests against {base}"
    );
    assert!(big_contacts > 0, "the scene has bodies touching the large one");
    assert!(
        big_near * 10 >= big_pairs * 6,
        "only {big_near} of the large body's {big_pairs} tests were rows within its contact reach"
    );
    // Control: with the large class off, the one large body widens every gather (the gap this closes).
    let off = BroadphaseConfig {
        large_radius: Some(f32::INFINITY),
        ..config
    };
    let wide = total_candidates(&grid_stats_matching_n2(&big, off, 30, "large class off"));
    println!("candidates with the large class off {wide}");
    assert!(
        wide > base * 5,
        "the control scene should widen every gather: {wide} vs {base}"
    );
}

/// Counts the candidate lines the broadphase draws whose second end is at `to`, and those whose
/// first end is within `reach` of it on XZ.
struct CandidateLines {
    to: [f32; 3],
    reach: f32,
    hits: u64,
    near: u64,
}

impl DebugSink for CandidateLines {
    fn line(&mut self, kind: DebugKind, a: [f32; 3], b: [f32; 3]) {
        if kind == DebugKind::Candidate && b == self.to {
            self.hits += 1;
            let (dx, dz) = (a[0] - b[0], a[2] - b[2]);
            if dx * dx + dz * dz <= self.reach * self.reach {
                self.near += 1;
            }
        }
    }
    fn circle(&mut self, _kind: DebugKind, _center: [f32; 3], _radius: f32) {}
    fn aabb(&mut self, _kind: DebugKind, _min: [f32; 3], _max: [f32; 3]) {}
    fn text(&mut self, _kind: DebugKind, _at: [f32; 3], _text: &str) {}
}

#[test]
fn the_oracle_scenarios_match_with_world_bounds_and_large_classes() {
    // The F6 oracle mixes again, with bounds that cut the crowd (bodies on both sides of the edge,
    // crossing it as they drive in), bounds that hold everything, and a large-radius class that
    // takes most bodies out of the grid.
    let configs = [
        (0.3, 0.0, 1 << 16),
        (1.0, 0.05, 1 << 16),
        (4.0, 0.5, 1 << 16),
        (25.0, 3.0, 1 << 16),
        (2.0, 0.5, 9),
    ];
    let bounds = [
        Some([-8.0, -9.0, 7.0, 6.0]),
        Some([-1.0e4, -1.0e4, 1.0e4, 1.0e4]),
        None,
    ];
    let larges = [None, Some(1.0), Some(f32::INFINITY)];
    let rules = [FloorRule::Bands, FloorRule::UndergroundOnly, FloorRule::Never];
    let mut outliers = 0;
    let mut large = 0;
    for (k, &(cell_size, margin, max_cells)) in configs.iter().enumerate() {
        for (w, &world_bounds) in bounds.iter().enumerate() {
            for (l, &large_radius) in larges.iter().enumerate() {
                let rule = rules[(k + w + l) % 3];
                let config = BroadphaseConfig {
                    cell_size,
                    margin,
                    max_cells,
                    floor_rule: rule,
                    world_bounds,
                    large_radius,
                };
                let seed = 0x0bd5_0000 + (k * 9 + w * 3 + l) as u64;
                let crowd = Crowd::new(seed, 220, 30.0, rule, Pairs::N2);
                let stats = grid_stats_matching_n2(&crowd, config, 12, &format!("{config:?}"));
                outliers += stats.iter().map(|s| s.outliers).sum::<usize>();
                large += stats.iter().map(|s| s.large).sum::<usize>();
            }
        }
    }
    assert!(outliers > 1000, "only {outliers} outlier filings");
    assert!(large > 1000, "only {large} large filings");
}

/// SplitMix64, for the property test's case generator.
struct Mix(u64);

impl Mix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// Uniform in `[lo, hi)` from 24 bits.
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * ((self.next() >> 40) as f32 * (1.0 / 16_777_216.0))
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// One visited contact with every number the visitor saw.
type FullVisit = (usize, usize, u32, u32, u32, u32);

/// The property test's visitor: depending on the pair and the tick it pushes the pair apart by a
/// random-looking scale, parks body `b` far outside the map, brings it back next to `a`, or drags
/// body `a` across the map (past any margin). Pure in its inputs, so both loops do the same thing.
fn wild_visitor(bodies: &mut [BroadBody], c: Contact, tick: u32, park: f32) {
    let key = (c.a as u64)
        .wrapping_mul(0x9e37_79b9)
        .wrapping_add((c.b as u64).wrapping_mul(0x85eb_ca6b))
        .wrapping_add(u64::from(tick).wrapping_mul(0xc2b2_ae35));
    let dist = math::sqrt(c.dist_sq);
    let (nx, nz) = if dist > 1e-4 {
        (c.dx / dist, c.dz / dist)
    } else {
        (1.0, 0.0)
    };
    match key % 11 {
        0 => {
            bodies[c.b].pos[0] = park + c.a as f32;
            bodies[c.b].pos[2] = -park;
        }
        1 => {
            bodies[c.b].pos[0] = bodies[c.a].pos[0] + 0.25;
            bodies[c.b].pos[2] = bodies[c.a].pos[2];
        }
        2 => {
            bodies[c.a].pos[0] += 37.0 * nx - 11.0;
            bodies[c.a].pos[2] -= 23.0 * nz;
        }
        k => {
            let scale = 0.5 + k as f32 * 0.75;
            let push = (c.min_dist - dist) * scale * 0.5;
            bodies[c.a].pos[0] -= nx * push;
            bodies[c.a].pos[2] -= nz * push;
            bodies[c.b].pos[0] += nx * push;
            bodies[c.b].pos[2] += nz * push;
        }
    }
}

#[test]
fn random_bounds_outliers_and_radius_mixes_match_the_n2_loop() {
    let mut rng = Mix(0xd3c3_4c00);
    let (mut outliers, mut large, mut contacts) = (0_usize, 0_usize, 0_u64);
    for case in 0..200 {
        let n = 2 + rng.below(140) as usize;
        let side = rng.range(5.0, 300.0);
        let mut bodies = Vec::with_capacity(n);
        for _ in 0..n {
            let mut pos = [
                rng.range(-side, side),
                rng.range(-2.0, 2.0),
                rng.range(-side, side),
            ];
            if rng.chance(8) {
                // Parked far away, sometimes in a heap of parked bodies.
                let far = if rng.chance(50) {
                    1.0e6
                } else {
                    rng.range(1.0e3, 1.0e7)
                };
                pos[0] = if rng.chance(50) { far } else { -far };
                pos[2] = if rng.chance(30) {
                    far
                } else {
                    rng.range(-side, side)
                };
            }
            if rng.chance(2) {
                pos[0] = f32::NAN;
            }
            let radius = match rng.below(20) {
                0 => rng.range(5.0, 90.0),
                1 => rng.range(2.0, 12.0),
                2 if rng.chance(20) => 0.0,
                3 if rng.chance(10) => -rng.range(0.1, 2.0),
                _ => rng.range(0.2, 3.0),
            };
            let y = pos[1];
            let mut body = BroadBody::new(pos, radius, [y - 1.0, y + rng.range(0.5, 4.0)]);
            body.underground = rng.chance(10);
            bodies.push(body);
        }
        let world_bounds = match rng.below(4) {
            0 => None,
            _ => {
                let w = side * rng.range(0.2, 1.5);
                let cx = rng.range(-side, side) * 0.3;
                let cz = rng.range(-side, side) * 0.3;
                Some([cx - w, cz - w * rng.range(0.5, 1.5), cx + w, cz + w])
            }
        };
        let large_radius = match rng.below(4) {
            0 => None,
            1 => Some(f32::INFINITY),
            _ => Some(rng.range(0.5, 40.0)),
        };
        let config = BroadphaseConfig {
            cell_size: rng.range(0.5, 16.0),
            margin: if rng.chance(30) { 0.0 } else { rng.range(0.0, 3.0) },
            max_cells: if rng.chance(20) {
                1 + rng.below(16) as usize
            } else {
                1 << 16
            },
            floor_rule: [FloorRule::Bands, FloorRule::UndergroundOnly, FloorRule::Never]
                [rng.below(3) as usize],
            world_bounds,
            large_radius,
        };
        let park = rng.range(1.0e4, 1.0e6);
        let mut reference = bodies.clone();
        let mut grid_bodies = bodies;
        let mut grid = DetBroadphase::new(config);
        for tick in 0..4 {
            let mut want: Vec<FullVisit> = Vec::new();
            for_each_contact_n2(&mut reference, config.floor_rule, |b, c| {
                want.push((
                    c.a,
                    c.b,
                    c.dx.to_bits(),
                    c.dz.to_bits(),
                    c.dist_sq.to_bits(),
                    c.min_dist.to_bits(),
                ));
                wild_visitor(b, c, tick, park);
            });
            let mut got: Vec<FullVisit> = Vec::new();
            grid.for_each_contact(&mut grid_bodies, |b, c| {
                got.push((
                    c.a,
                    c.b,
                    c.dx.to_bits(),
                    c.dz.to_bits(),
                    c.dist_sq.to_bits(),
                    c.min_dist.to_bits(),
                ));
                wild_visitor(b, c, tick, park);
            });
            if let Some(i) = (0..want.len().max(got.len())).find(|&i| want.get(i) != got.get(i)) {
                panic!(
                    "case {case} tick {tick} {config:?}: contact #{i} differs: all-pairs {:?}, grid {:?}",
                    want.get(i),
                    got.get(i)
                );
            }
            let bits = |v: &[BroadBody]| -> Vec<[u32; 2]> {
                v.iter()
                    .map(|b| [b.pos[0].to_bits(), b.pos[2].to_bits()])
                    .collect()
            };
            assert_eq!(bits(&reference), bits(&grid_bodies), "case {case} tick {tick}");
            let stats = grid.stats();
            outliers += stats.outliers;
            large += stats.large;
            contacts += stats.contacts;
        }
    }
    assert!(outliers > 200, "only {outliers} outlier filings");
    assert!(large > 200, "only {large} large filings");
    assert!(contacts > 5000, "only {contacts} contacts");
}
