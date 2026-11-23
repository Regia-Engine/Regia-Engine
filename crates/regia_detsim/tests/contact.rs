// SPDX-License-Identifier: MPL-2.0
//! Contact response for the lockstep broadphase (ADR-8563): mass-weighted pushes, restitution and
//! friction impulses, rams, and the grid pass against the all-pairs loop, bit for bit.

use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, Contact, DetBroadphase, FloorRule, contact};
use regia_detsim::contact::{
    self, ContactBody, ContactConfig, ContactPile, ContactStats, Ram, resolve, resolve_all, resolve_all_n2,
};
use regia_detsim::golden::{self, DetScenario};
use regia_detsim::math;

/// No bounce floor and no rams unless a test asks.
const PLAIN: ContactConfig = ContactConfig {
    ram_threshold_m_s: f32::INFINITY,
    bounce_min_speed_m_s: 0.0,
};

fn body(x: f32, z: f32, radius: f32) -> BroadBody {
    BroadBody::new([x, 0.0, z], radius, [-1.0, 1.0])
}

fn mass(kg: f32) -> ContactBody {
    ContactBody {
        max_push_m: f32::INFINITY,
        friction: 0.0,
        ..ContactBody::with_mass(kg)
    }
}

/// Resolve the one contact between bodies 0 and 1.
fn resolve_pair(
    bodies: &mut [BroadBody],
    props: &[ContactBody],
    vel: &mut [[f32; 3]],
    cfg: &ContactConfig,
) -> Option<Ram> {
    let c = contact(bodies, 0, 1, FloorRule::Bands).expect("the pair overlaps");
    resolve(bodies, props, vel, c, cfg)
}

fn rel(a: f32, b: f32) -> f32 {
    let scale = if a.abs() > b.abs() { a.abs() } else { b.abs() };
    if scale == 0.0 { 0.0 } else { (a - b).abs() / scale }
}

/// 64-bit LCG (Knuth's MMIX constants) for the crowds.
struct Lcg(u64);

impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn unit(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / 16_777_216.0)
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

#[test]
fn equal_masses_split_the_push() {
    // Along X: 0.5 m overlap, 0.25 m each.
    let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.5, 0.0, 1.0)];
    let mut vel = vec![[0.0; 3]; 2];
    let props = [mass(2.0), mass(2.0)];
    assert_eq!(resolve_pair(&mut bodies, &props, &mut vel, &PLAIN), None);
    assert_eq!(bodies[0].pos[0], -0.25);
    assert_eq!(bodies[1].pos[0], 1.75);
    assert_eq!(bodies[1].pos[0] - bodies[0].pos[0], 2.0);
    assert_eq!(vel, vec![[0.0; 3]; 2], "a resting pair gets no impulse");

    // Along a diagonal: the two moves are mirror images and the pair ends at touching distance.
    let mut bodies = vec![body(1.0, 2.0, 0.75), body(1.6, 2.8, 0.75)];
    let mut vel = vec![[0.0; 3]; 2];
    resolve_pair(&mut bodies, &props, &mut vel, &PLAIN);
    let move_a = [bodies[0].pos[0] - 1.0, bodies[0].pos[2] - 2.0];
    let move_b = [bodies[1].pos[0] - 1.6, bodies[1].pos[2] - 2.8];
    assert!(rel(-move_a[0], move_b[0]) < 1e-5, "{move_a:?} {move_b:?}");
    assert!(rel(-move_a[1], move_b[1]) < 1e-5, "{move_a:?} {move_b:?}");
    let dx = bodies[1].pos[0] - bodies[0].pos[0];
    let dz = bodies[1].pos[2] - bodies[0].pos[2];
    assert!(rel(math::sqrt(dx * dx + dz * dz), 1.5) < 1e-5);
    assert_eq!(bodies[0].pos[1], 0.0, "the solver never moves a body vertically");
}

#[test]
fn a_static_body_never_moves() {
    let cases: [(f32, f32, [f32; 3]); 5] = [
        (1.2, 0.0, [-10.0, 0.0, 0.0]), // a dynamic body slamming in
        (0.3, 0.9, [-4.0, 0.0, -7.0]), // a glancing hit
        (0.0, 0.0, [0.0, 0.0, 0.0]),   // coincident
        (f32::NAN, 0.0, [1.0, 0.0, 0.0]),
        (1.9, 0.1, [5.0, 0.0, 0.0]), // separating
    ];
    for (x, z, v) in cases {
        let kinematic_vel = [3.0, -1.0, 0.5];
        let mut bodies = vec![body(0.0, 0.0, 1.0), body(x, z, 1.0)];
        let mut vel = vec![kinematic_vel, v];
        let props = [
            ContactBody {
                friction: 1.0,
                ..ContactBody::STATIC
            },
            ContactBody {
                restitution: 1.0,
                friction: 1.0,
                ..mass(1.0)
            },
        ];
        let before = bodies[0];
        resolve_pair(&mut bodies, &props, &mut vel, &PLAIN);
        assert_eq!(
            bodies[0].pos.map(f32::to_bits),
            before.pos.map(f32::to_bits),
            "{x} {z}"
        );
        assert_eq!(
            vel[0].map(f32::to_bits),
            kinematic_vel.map(f32::to_bits),
            "{x} {z}"
        );
        if x.is_finite() {
            // The dynamic body takes the whole push and ends at touching distance.
            let dx = bodies[1].pos[0];
            let dz = bodies[1].pos[2];
            assert!(
                rel(math::sqrt(dx * dx + dz * dz), 2.0) < 1e-5,
                "{x} {z} {:?}",
                bodies[1].pos
            );
        }
    }

    // Two static bodies: no push, no impulse, even when they overlap and close.
    let mut bodies = vec![body(0.0, 0.0, 1.0), body(0.5, 0.0, 1.0)];
    let mut vel = vec![[4.0, 0.0, 0.0], [-4.0, 0.0, 0.0]];
    let before = (bodies.clone(), vel.clone());
    let cfg = ContactConfig {
        ram_threshold_m_s: 5.0,
        ..PLAIN
    };
    let ram = resolve_pair(&mut bodies, &[ContactBody::STATIC; 2], &mut vel, &cfg).expect("a ram");
    assert_eq!(ram.impulse, 0.0);
    assert_eq!(ram.closing_speed_m_s, 8.0);
    assert_eq!((bodies, vel), before);

    // Posts in a crowd stay put, bit for bit, over a long run.
    let mut pile = ContactPile::with_bodies("post_check", 300);
    let posts: Vec<(usize, [u32; 3])> = pile
        .props()
        .iter()
        .zip(pile.bodies())
        .enumerate()
        .filter(|(_, (p, _))| !p.is_dynamic())
        .map(|(i, (_, b))| (i, b.pos.map(f32::to_bits)))
        .collect();
    assert_eq!(posts.len(), 15);
    for tick in 0..600 {
        pile.step(tick);
    }
    assert!(pile.totals().pushes > 1000, "{:?}", pile.totals());
    for (i, bits) in posts {
        assert_eq!(pile.bodies()[i].pos.map(f32::to_bits), bits, "post {i}");
        assert_eq!(pile.velocities()[i], [0.0; 3], "post {i}");
    }
}

#[test]
fn heavy_pushes_light_by_mass_ratio() {
    for (heavy, light) in [(10.0, 1.0), (4.0, 0.5), (1000.0, 3.0)] {
        // Heavy body first, then the other way round (the split does not depend on slot order).
        for heavy_first in [true, false] {
            let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.2, 0.4, 1.0)];
            let mut vel = vec![[0.0; 3]; 2];
            let props = if heavy_first {
                [mass(heavy), mass(light)]
            } else {
                [mass(light), mass(heavy)]
            };
            let start = bodies.clone();
            resolve_pair(&mut bodies, &props, &mut vel, &PLAIN);
            let moved = |i: usize| {
                let dx = bodies[i].pos[0] - start[i].pos[0];
                let dz = bodies[i].pos[2] - start[i].pos[2];
                math::sqrt(dx * dx + dz * dz)
            };
            let (h, l) = if heavy_first { (0, 1) } else { (1, 0) };
            let ratio = moved(l) / moved(h);
            assert!(rel(ratio, heavy / light) < 1e-3, "{heavy}/{light}: {ratio}");
            let dx = bodies[1].pos[0] - bodies[0].pos[0];
            let dz = bodies[1].pos[2] - bodies[0].pos[2];
            assert!(rel(math::sqrt(dx * dx + dz * dz), 2.0) < 1e-5);
        }
    }

    // The velocity change splits the same way: a heavy hull barely slows, the light one is thrown.
    let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.9, 0.0, 1.0)];
    let mut vel = vec![[5.0, 0.0, 0.0], [0.0; 3]];
    let props = [mass(10.0), mass(1.0)];
    resolve_pair(&mut bodies, &props, &mut vel, &PLAIN);
    let dv_heavy = 5.0 - vel[0][0];
    let dv_light = vel[1][0];
    assert!(rel(dv_light / dv_heavy, 10.0) < 1e-4, "{vel:?}");
}

#[test]
fn head_on_restitution_conserves_momentum() {
    let (ma, mb) = (2.0_f32, 5.0_f32);
    for e in [0.0, 0.3, 0.7, 1.0] {
        // Head on, then with sideways velocity and friction (momentum holds on both axes).
        for (va, vb, mu) in [
            ([3.0, 0.0, 0.0], [-4.0, 0.0, 0.0], 0.0),
            ([3.0, 0.0, 1.5], [-4.0, 0.0, -2.0], 0.9),
            ([0.5, 0.0, 6.0], [-0.25, 0.0, -1.0], 0.2),
        ] {
            let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.8, 0.0, 1.0)];
            let mut vel = vec![va, vb];
            let props = [
                ContactBody {
                    restitution: e,
                    friction: mu,
                    ..mass(ma)
                },
                ContactBody {
                    restitution: e,
                    friction: mu,
                    ..mass(mb)
                },
            ];
            resolve_pair(&mut bodies, &props, &mut vel, &PLAIN);
            for axis in [0, 2] {
                let before = ma * va[axis] + mb * vb[axis];
                let after = ma * vel[0][axis] + mb * vel[1][axis];
                let scale = ma * va[axis].abs() + mb * vb[axis].abs();
                assert!(
                    (after - before).abs() <= 1e-5 * scale,
                    "e {e} axis {axis}: {before} -> {after}"
                );
            }
            // Newton's law of restitution along the normal (+X here).
            let closing = va[0] - vb[0];
            let leaving = vel[1][0] - vel[0][0];
            assert!(
                (leaving - e * closing).abs() <= 1e-5 * closing,
                "e {e}: closing {closing}, leaving {leaving}"
            );
            // Kinetic energy never grows.
            let ke = |v: &[[f32; 3]]| {
                0.5 * ma * (v[0][0] * v[0][0] + v[0][2] * v[0][2])
                    + 0.5 * mb * (v[1][0] * v[1][0] + v[1][2] * v[1][2])
            };
            let k0 = ke(&[va, vb]);
            assert!(ke(&vel) <= k0 * (1.0 + 1e-6), "e {e}: {k0} -> {}", ke(&vel));
            assert_eq!([vel[0][1], vel[1][1]], [0.0, 0.0]);
        }
    }
}

#[test]
fn restitution_zero_stops_closing() {
    let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.0, 1.0, 1.0)];
    let mut vel = vec![[6.0, 0.0, 2.0], [-1.0, 0.0, -3.0]];
    resolve_pair(&mut bodies, &[mass(3.0), mass(1.5)], &mut vel, &PLAIN);
    let n = [std::f32::consts::FRAC_1_SQRT_2; 2];
    let vn = (vel[1][0] - vel[0][0]) * n[0] + (vel[1][2] - vel[0][2]) * n[1];
    assert!(vn.abs() < 1e-5, "normal speed after {vn}");

    // A bouncy pair closing at or below the bounce floor stops too (no jitter in a resting pile).
    let cfg = ContactConfig {
        bounce_min_speed_m_s: 0.5,
        ..PLAIN
    };
    let bouncy = ContactBody {
        restitution: 1.0,
        ..mass(1.0)
    };
    let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.9, 0.0, 1.0)];
    let mut vel = vec![[0.25, 0.0, 0.0], [-0.25, 0.0, 0.0]];
    resolve_pair(&mut bodies, &[bouncy; 2], &mut vel, &cfg);
    assert_eq!(vel[0][0], vel[1][0], "{vel:?}");
    // Above the floor it bounces.
    let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.9, 0.0, 1.0)];
    let mut vel = vec![[0.5, 0.0, 0.0], [-0.5, 0.0, 0.0]];
    resolve_pair(&mut bodies, &[bouncy; 2], &mut vel, &cfg);
    assert_eq!(vel, vec![[-0.5, 0.0, 0.0], [0.5, 0.0, 0.0]]);
}

/// A violent crowd: posts, a deck, pairs spawned on top of each other, one body with a NaN position
/// and bouncy, heavy and light bodies, through either loop.
struct Crowd {
    bodies: Vec<BroadBody>,
    props: Vec<ContactBody>,
    vel: Vec<[f32; 3]>,
}

impl Crowd {
    fn new(n: usize, seed: u64, nan_body: bool) -> Self {
        let mut rng = Lcg(seed);
        let side = math::sqrt(n as f32 * 4.0);
        let mut c = Crowd {
            bodies: Vec::with_capacity(n),
            props: Vec::with_capacity(n),
            vel: Vec::with_capacity(n),
        };
        for i in 0..n {
            let r = rng.range(0.3, 1.2);
            let y = if i % 9 == 4 { 3.0 } else { 0.0 };
            let (mut x, mut z) = (rng.range(0.0, side), rng.range(0.0, side));
            if i % 50 == 7 && i > 0 {
                // Spawned exactly on the previous body.
                x = c.bodies[i - 1].pos[0];
                z = c.bodies[i - 1].pos[2];
            }
            if nan_body && i == n / 2 {
                x = f32::NAN;
            }
            c.bodies.push(BroadBody::new([x, y, z], r, [y - 0.5, y + 1.0]));
            let props = if i % 31 == 0 {
                ContactBody::STATIC
            } else {
                ContactBody {
                    inv_mass: 1.0 / rng.range(0.2, 20.0),
                    restitution: rng.unit(),
                    friction: rng.range(0.0, 1.2),
                    max_push_m: if i % 3 == 0 { 0.05 } else { 0.4 },
                }
            };
            let v = [rng.range(-8.0, 8.0), 0.0, rng.range(-8.0, 8.0)];
            // Static posts stand still (a moving kinematic body may add energy to what it hits).
            c.vel.push(if props.is_dynamic() { v } else { [0.0; 3] });
            c.props.push(props);
        }
        c
    }

    /// Integrate and pull toward the crowd's centre; the same for both loops.
    fn integrate(&mut self, centre: f32, dt: f32) {
        for ((b, v), p) in self.bodies.iter_mut().zip(&mut self.vel).zip(&self.props) {
            if !p.is_dynamic() {
                continue;
            }
            v[0] += (centre - b.pos[0]) * 0.5 * dt;
            v[2] += (centre - b.pos[2]) * 0.5 * dt;
            b.pos[0] += v[0] * dt;
            b.pos[2] += v[2] * dt;
        }
    }

    fn bits(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.bodies.len() * 4);
        for (b, v) in self.bodies.iter().zip(&self.vel) {
            out.extend([b.pos[0], b.pos[2], v[0], v[2]].map(f32::to_bits));
        }
        out
    }
}

fn ram_bits(rams: &[Ram]) -> Vec<(usize, usize, u32, u32, u32)> {
    rams.iter()
        .map(|r| {
            (
                r.a,
                r.b,
                r.normal[0].to_bits(),
                r.closing_speed_m_s.to_bits(),
                r.impulse.to_bits(),
            )
        })
        .collect()
}

/// Run `ticks` of a crowd through the grid and the all-pairs loop, comparing every tick.
fn grid_matches_n2(n: usize, ticks: u32, seed: u64, nan_body: bool, rule: FloorRule) -> ContactStats {
    let mut grid_side = Crowd::new(n, seed, nan_body);
    let mut n2_side = Crowd::new(n, seed, nan_body);
    let mut grid = DetBroadphase::new(BroadphaseConfig {
        floor_rule: rule,
        ..BroadphaseConfig::default()
    });
    let cfg = ContactConfig {
        ram_threshold_m_s: 4.0,
        bounce_min_speed_m_s: 0.3,
    };
    let centre = math::sqrt(n as f32 * 4.0) * 0.5;
    let dt = 1.0 / 60.0;
    let (mut rams_grid, mut rams_n2) = (Vec::new(), Vec::new());
    let mut total = ContactStats::default();
    for tick in 0..ticks {
        grid_side.integrate(centre, dt);
        n2_side.integrate(centre, dt);
        rams_grid.clear();
        rams_n2.clear();
        let a = resolve_all(
            &mut grid,
            &mut grid_side.bodies,
            &grid_side.props,
            &mut grid_side.vel,
            &cfg,
            &mut rams_grid,
        )
        .expect("valid crowd");
        let b = resolve_all_n2(
            &mut n2_side.bodies,
            &n2_side.props,
            &mut n2_side.vel,
            &cfg,
            rule,
            &mut rams_n2,
        )
        .expect("valid crowd");
        assert_eq!(a, b, "stats differ at tick {tick}");
        assert_eq!(
            ram_bits(&rams_grid),
            ram_bits(&rams_n2),
            "rams differ at tick {tick}"
        );
        if grid_side.bits() != n2_side.bits() {
            let first = grid_side
                .bits()
                .iter()
                .zip(n2_side.bits())
                .position(|(x, y)| *x != y)
                .unwrap_or(0);
            panic!("state differs at tick {tick}, body {}", first / 4);
        }
        total.add(&a);
    }
    total
}

#[test]
fn resolve_all_matches_the_n2_reference_with_the_same_resolver() {
    // 900 bodies, 600 ticks, bit for bit, through the hand-made crowd.
    let total = grid_matches_n2(900, 600, 0x00c0_47ac_7900, false, FloorRule::Bands);
    assert!(total.pairs > 50_000, "{total:?}");
    assert!(total.capped_pushes > 100, "{total:?}");
    assert!(total.rams > 10, "{total:?}");
    // The other floor rules and a body with a NaN position (tested against everyone).
    grid_matches_n2(300, 200, 0x0bad_f00d, true, FloorRule::UndergroundOnly);
    grid_matches_n2(300, 200, 0x0bad_f00e, true, FloorRule::Never);

    // And the pile scenario: grid against the reference loop over its whole hash trace.
    let mut grid = ContactPile::with_bodies("contact_pile_900", 900);
    let mut n2 = ContactPile::with_bodies("contact_pile_900", 900);
    n2.use_reference_loop(true);
    let run = grid.default_run();
    assert_eq!(run.ticks, 600);
    let divergence = golden::compare(&mut grid, &mut n2, &run, None).expect("no injection");
    assert!(divergence.is_none(), "{divergence:?}");
    assert!(grid.totals().pairs > 100_000, "{:?}", grid.totals());
}

#[test]
fn rams_come_out_in_pair_order_and_respect_the_threshold() {
    // Three pairs, listed out of slot order: (1, 4) closes at 8 m/s, (0, 3) at exactly the
    // threshold (6 m/s, not a ram: the test is strict), (2, 5) at 12 m/s.
    let mut bodies = vec![
        body(0.0, 0.0, 1.0),  // 0
        body(10.0, 0.0, 1.0), // 1
        body(20.0, 0.0, 1.0), // 2
        body(1.5, 0.0, 1.0),  // 3, touching 0
        body(10.0, 1.5, 1.0), // 4, touching 1 along +Z
        body(21.5, 0.0, 1.0), // 5, touching 2
    ];
    let mut vel = vec![
        [3.0, 0.0, 0.0],
        [0.0, 0.0, 4.0],
        [6.0, 0.0, 0.0],
        [-3.0, 0.0, 0.0],
        [0.0, 0.0, -4.0],
        [-6.0, 0.0, 0.0],
    ];
    let props = [mass(1.0); 6];
    let cfg = ContactConfig {
        ram_threshold_m_s: 6.0,
        bounce_min_speed_m_s: 0.0,
    };
    let mut rams = vec![Ram {
        a: 99,
        b: 100,
        normal: [0.0; 3],
        closing_speed_m_s: 0.0,
        impulse: 0.0,
    }];
    let mut grid = DetBroadphase::default();
    let stats = resolve_all(&mut grid, &mut bodies, &props, &mut vel, &cfg, &mut rams).expect("valid scene");
    assert_eq!(stats.pairs, 3);
    assert_eq!(stats.rams, 2);
    assert_eq!(rams.len(), 3, "rams are appended, never cleared");
    assert_eq!((rams[1].a, rams[1].b), (1, 4));
    assert_eq!(rams[1].normal, [0.0, 0.0, 1.0]);
    assert_eq!(rams[1].closing_speed_m_s, 8.0);
    assert_eq!(rams[1].impulse, 4.0, "restitution 0, two 1 kg bodies: j = 8 / 2");
    assert_eq!((rams[2].a, rams[2].b), (2, 5));
    assert_eq!(rams[2].normal, [1.0, 0.0, 0.0]);
    assert_eq!(rams[2].closing_speed_m_s, 12.0);

    // In a crowd: rams come out in ascending (a, b) order, every one above the threshold, and
    // exactly the contacts that closed faster than it (measured before each resolve).
    let mut crowd = Crowd::new(600, 0x7a_3500, false);
    let cfg = ContactConfig {
        ram_threshold_m_s: 3.0,
        bounce_min_speed_m_s: 0.3,
    };
    let mut grid = DetBroadphase::default();
    let mut rams = Vec::new();
    let mut seen = 0_u64;
    let centre = math::sqrt(600.0_f32 * 4.0) * 0.5;
    for _ in 0..240 {
        crowd.integrate(centre, 1.0 / 60.0);
        // The expected rams, from an all-pairs pass on a copy, measuring the closing speed of each
        // contact before resolving it.
        let mut copy_bodies = crowd.bodies.clone();
        let mut copy_vel = crowd.vel.clone();
        let mut expected = Vec::new();
        regia_detsim::broadphase::for_each_contact_n2(
            &mut copy_bodies,
            FloorRule::Bands,
            |bodies, c: Contact| {
                let closing = closing_speed(&copy_vel, &c);
                let ram = resolve(bodies, &crowd.props, &mut copy_vel, c, &cfg);
                assert_eq!(ram.is_some(), closing > 3.0, "{c:?} closing {closing}");
                if let Some(r) = ram {
                    assert_eq!(r.closing_speed_m_s, closing);
                    expected.push((c.a, c.b));
                }
            },
        );
        rams.clear();
        let stats = resolve_all(
            &mut grid,
            &mut crowd.bodies,
            &crowd.props,
            &mut crowd.vel,
            &cfg,
            &mut rams,
        )
        .expect("valid crowd");
        let got: Vec<(usize, usize)> = rams.iter().map(|r| (r.a, r.b)).collect();
        assert_eq!(got, expected);
        assert!(got.windows(2).all(|w| w[0] < w[1]), "{got:?}");
        assert!(rams.iter().all(|r| r.closing_speed_m_s > 3.0));
        assert_eq!(stats.rams, rams.len() as u64);
        seen += stats.rams;
    }
    assert!(seen > 20, "the crowd must ram: {seen}");
}

/// Closing speed of a contact as the solver measures it (the same steps).
fn closing_speed(vel: &[[f32; 3]], c: &Contact) -> f32 {
    let dist = math::sqrt(c.dist_sq);
    let (nx, nz) = if dist >= contact::DEGENERATE_DIST_M {
        (c.dx / dist, c.dz / dist)
    } else {
        (1.0, 0.0)
    };
    let (va, vb) = (vel[c.a], vel[c.b]);
    -((vb[0] - va[0]) * nx + (vb[2] - va[2]) * nz)
}

#[test]
fn nan_and_coincident_bodies_separate_finitely() {
    let props = [
        ContactBody {
            max_push_m: 0.3,
            ..mass(1.0)
        },
        ContactBody {
            max_push_m: 0.3,
            ..mass(1.0)
        },
    ];

    // Coincident: the lower slot goes to -X, the higher to +X, each by its capped share.
    let mut bodies = vec![body(5.0, 5.0, 1.0), body(5.0, 5.0, 1.0)];
    let mut vel = vec![[0.0, 0.0, 1.0], [0.0, 0.0, -1.0]];
    resolve_pair(&mut bodies, &props, &mut vel, &PLAIN);
    assert_eq!(bodies[0].pos, [4.7, 0.0, 5.0]);
    assert_eq!(bodies[1].pos, [5.3, 0.0, 5.0]);
    assert!(vel.iter().flatten().all(|v| v.is_finite()), "{vel:?}");
    // Slot order, not argument order, picks the side: the same bodies swapped land mirrored.
    let mut bodies = vec![body(-2.0, 0.0, 0.5), body(-2.0, 0.0, 0.5)];
    let mut vel = vec![[0.0; 3]; 2];
    resolve_pair(&mut bodies, &props, &mut vel, &PLAIN);
    assert!(bodies[0].pos[0] < -2.0 && bodies[1].pos[0] > -2.0);

    // Closer than DEGENERATE_DIST_M but not equal: the same fixed axis.
    let mut bodies = vec![body(1.0, 1.0, 1.0), body(1.0, 1.0 + 1e-7, 1.0)];
    let mut vel = vec![[0.0; 3]; 2];
    resolve_pair(&mut bodies, &props, &mut vel, &PLAIN);
    assert!(bodies[1].pos[0] > bodies[0].pos[0]);
    assert!(bodies.iter().all(|b| b.pos.iter().all(|v| v.is_finite())));

    // A body with a NaN position: the broadphase reports the pair (NaN never skips); the finite
    // body is pushed along the fixed axis by a finite amount and keeps a finite velocity.
    for nan_slot in [0, 1] {
        let mut bodies = vec![body(0.0, 0.0, 1.0), body(3.0, 0.0, 1.0)];
        bodies[nan_slot].pos[0] = f32::NAN;
        let mut vel = vec![[2.0, 0.0, 0.0], [-2.0, 0.0, 0.0]];
        let c = contact(&bodies, 0, 1, FloorRule::Bands).expect("NaN counts as touching");
        assert!(c.dist_sq.is_nan());
        resolve(&mut bodies, &props, &mut vel, c, &PLAIN);
        let ok = 1 - nan_slot;
        assert!(bodies[ok].pos.iter().all(|v| v.is_finite()), "{:?}", bodies[ok]);
        assert!(vel.iter().flatten().all(|v| v.is_finite()), "{vel:?}");
        let expected = if ok == 0 { -0.3 } else { 3.3 };
        assert_eq!(bodies[ok].pos[0], expected);
    }

    // A NaN velocity skips the impulse (the other body's velocity stays finite), and a NaN or
    // infinite radius pushes nothing.
    let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.0, 0.0, 1.0)];
    let mut vel = vec![[f32::NAN, 0.0, 0.0], [-1.0, 0.0, 0.0]];
    resolve_pair(&mut bodies, &props, &mut vel, &PLAIN);
    assert_eq!(vel[1], [-1.0, 0.0, 0.0]);
    for radius in [f32::NAN, f32::INFINITY] {
        let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.0, 0.0, radius)];
        let mut vel = vec![[0.0; 3]; 2];
        if let Some(c) = contact(&bodies, 0, 1, FloorRule::Bands) {
            resolve(&mut bodies, &props, &mut vel, c, &PLAIN);
        }
        assert_eq!(bodies[0].pos, [0.0, 0.0, 0.0], "radius {radius}");
        assert!(vel.iter().flatten().all(|v| v.is_finite()));
    }

    // A whole crowd with a NaN body and stacked spawns: every other body stays finite.
    let mut crowd = Crowd::new(200, 0x0a0a, true);
    let mut grid = DetBroadphase::default();
    let mut rams = Vec::new();
    for _ in 0..300 {
        crowd.integrate(14.0, 1.0 / 60.0);
        resolve_all(
            &mut grid,
            &mut crowd.bodies,
            &crowd.props,
            &mut crowd.vel,
            &ContactConfig::default(),
            &mut rams,
        )
        .expect("valid crowd");
    }
    for (i, (b, v)) in crowd.bodies.iter().zip(&crowd.vel).enumerate() {
        if i == 100 {
            continue;
        }
        assert!(b.pos[0].is_finite() && b.pos[2].is_finite(), "body {i}: {b:?}");
        assert!(v.iter().all(|x| x.is_finite()), "body {i}: {v:?}");
    }
}

#[test]
fn bodies_on_different_floors_never_touch() {
    // A body on a deck over another: Bands keeps them apart, Never (height-blind) pushes them.
    for (rule, touches) in [(FloorRule::Bands, false), (FloorRule::Never, true)] {
        let mut bodies = vec![
            BroadBody::new([0.0, 0.0, 0.0], 1.0, [-0.5, 1.0]),
            BroadBody::new([0.5, 3.0, 0.0], 1.0, [2.5, 4.0]),
        ];
        let mut vel = vec![[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0]];
        let mut grid = DetBroadphase::new(BroadphaseConfig {
            floor_rule: rule,
            ..BroadphaseConfig::default()
        });
        let mut rams = Vec::new();
        let stats = resolve_all(
            &mut grid,
            &mut bodies,
            &[mass(1.0); 2],
            &mut vel,
            &PLAIN,
            &mut rams,
        )
        .expect("valid");
        assert_eq!(stats.pairs, u64::from(touches), "{rule:?}");
        assert_eq!(bodies[0].pos[0] != 0.0, touches, "{rule:?}");
    }
}

#[test]
fn invalid_settings_are_refused_before_anything_moves() {
    let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.0, 0.0, 1.0)];
    let mut vel = vec![[1.0, 0.0, 0.0], [0.0; 3]];
    let before = (bodies.clone(), vel.clone());
    let mut grid = DetBroadphase::default();
    let mut rams = Vec::new();
    let springy = ContactBody {
        restitution: 1.5,
        ..ContactBody::default()
    };
    let err = resolve_all(
        &mut grid,
        &mut bodies,
        &[ContactBody::default(), springy],
        &mut vel,
        &ContactConfig::default(),
        &mut rams,
    )
    .expect_err("restitution above 1 adds energy");
    assert_eq!(err.code(), "contact_bad_body");
    assert!(err.to_string().contains("body 1"), "{err}");
    let err = resolve_all_n2(
        &mut bodies,
        &[ContactBody::default(); 2],
        &mut vel,
        &ContactConfig {
            bounce_min_speed_m_s: -1.0,
            ..ContactConfig::default()
        },
        FloorRule::Bands,
        &mut rams,
    )
    .expect_err("negative bounce floor");
    assert_eq!(err.code(), "contact_bad_config");
    assert_eq!((bodies, vel), before);
    assert!(rams.is_empty());
}

#[test]
fn energy_never_grows_in_a_closed_pile() {
    // No pull and no drag: contacts alone may only lose kinetic energy (restitution <= 1, friction
    // capped), up to rounding.
    let mut crowd = Crowd::new(400, 0xe0e0, false);
    let mut grid = DetBroadphase::default();
    let mut rams = Vec::new();
    let ke = |c: &Crowd| -> f64 {
        c.vel
            .iter()
            .zip(&c.props)
            .filter(|(_, p)| p.is_dynamic())
            .map(|(v, p)| 0.5 / f64::from(p.inv_mass) * f64::from(v[0] * v[0] + v[2] * v[2]))
            .sum()
    };
    for tick in 0..300 {
        for (b, v) in crowd.bodies.iter_mut().zip(&crowd.vel) {
            b.pos[0] += v[0] / 60.0;
            b.pos[2] += v[2] / 60.0;
        }
        let before = ke(&crowd);
        resolve_all(
            &mut grid,
            &mut crowd.bodies,
            &crowd.props,
            &mut crowd.vel,
            &PLAIN,
            &mut rams,
        )
        .expect("valid crowd");
        let after = ke(&crowd);
        assert!(after <= before * (1.0 + 1e-5), "tick {tick}: {before} -> {after}");
    }
}

#[test]
fn a_kinematic_body_drives_what_it_hits() {
    // A kinematic ram (inv_mass 0, moving at 10 m/s) hits a resting 2 kg crate: the crate takes the
    // whole push and leaves at the ram's speed (restitution 0) or twice it (restitution 1); the
    // ram's velocity is never changed.
    for (e, expected) in [(0.0, 10.0), (1.0, 20.0)] {
        let mut bodies = vec![body(0.0, 0.0, 1.0), body(1.5, 0.0, 1.0)];
        let mut vel = vec![[10.0, 0.0, 0.0], [0.0; 3]];
        let props = [
            ContactBody {
                restitution: e,
                ..ContactBody::STATIC
            },
            mass(2.0),
        ];
        let cfg = ContactConfig {
            ram_threshold_m_s: 5.0,
            ..PLAIN
        };
        let ram = resolve_pair(&mut bodies, &props, &mut vel, &cfg).expect("10 m/s is a ram");
        assert_eq!(vel[0], [10.0, 0.0, 0.0]);
        assert_eq!(bodies[0].pos, [0.0, 0.0, 0.0]);
        assert_eq!(bodies[1].pos[0], 2.0, "the crate takes the whole 0.5 m push");
        assert_eq!(vel[1][0], expected, "e {e}");
        assert_eq!(
            ram.impulse,
            2.0 * expected,
            "impulse = crate mass x its velocity change"
        );
    }
}
