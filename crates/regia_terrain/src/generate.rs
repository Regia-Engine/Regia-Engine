// SPDX-License-Identifier: MPL-2.0
//! Deterministic synthetic heightfield generation: ridged fractal mountains plus droplet
//! hydraulic erosion and a thermal (talus) pass.
//!
//! The erosion model is a simplified droplet simulation in the family of Musgrave, Kolb &
//! Mace, "The Synthesis and Rendering of Eroded Fractal Terrains" (SIGGRAPH 1989): water
//! droplets roll downhill, pick up sediment proportional to slope × speed × water, and drop
//! it when the carrying capacity falls. The thermal pass moves material between cells that
//! exceed a talus angle, the way Musgrave et al. describe for creep. All randomness comes
//! from [`regia_core::math::Rng`] so a seed reproduces the map bit-for-bit on every client.

use regia_core::math::{Rng, det_powf};

use crate::noise::value_noise2;

/// The 16 exit directions `(cos θ, sin θ)` for `θ = k·2π/16`, as decimal f64 literals. The
/// same decimal string parses to the same bits on every platform (unlike libm `sin`/`cos`),
/// so a droplet that stalls on a flat cell leaves in a host-independent direction.
const EXIT_DIRS: [(f64, f64); 16] = [
    (1.0, 0.0),
    (0.923_879_532_511_286_7, 0.382_683_432_365_089_8),
    (std::f64::consts::FRAC_1_SQRT_2, std::f64::consts::FRAC_1_SQRT_2),
    (0.382_683_432_365_089_8, 0.923_879_532_511_286_7),
    (0.0, 1.0),
    (-0.382_683_432_365_089_8, 0.923_879_532_511_286_7),
    (-std::f64::consts::FRAC_1_SQRT_2, std::f64::consts::FRAC_1_SQRT_2),
    (-0.923_879_532_511_286_7, 0.382_683_432_365_089_8),
    (-1.0, 0.0),
    (-0.923_879_532_511_286_7, -0.382_683_432_365_089_8),
    (-std::f64::consts::FRAC_1_SQRT_2, -std::f64::consts::FRAC_1_SQRT_2),
    (-0.382_683_432_365_089_8, -0.923_879_532_511_286_7),
    (0.0, -1.0),
    (0.382_683_432_365_089_8, -0.923_879_532_511_286_7),
    (std::f64::consts::FRAC_1_SQRT_2, -std::f64::consts::FRAC_1_SQRT_2),
    (0.923_879_532_511_286_7, -0.382_683_432_365_089_8),
];

/// Parameters of the droplet hydraulic erosion pass.
#[derive(Clone, Copy, Debug)]
pub struct ErosionParams {
    /// Number of water droplets to simulate.
    pub droplets: u32,
    /// Maximum lifetime steps per droplet.
    pub max_steps: u32,
    /// Direction persistence: 0 = instantly downhill, 1 = never turns.
    pub inertia: f32,
    /// Sediment capacity per unit (slope × speed × water) multiplier.
    pub sediment_capacity: f32,
    /// Slope below which the capacity treats the ground as flat, metres/cell.
    pub min_slope: f32,
    /// Fraction of the capacity deficit eroded per step.
    pub erode_rate: f32,
    /// Fraction of the surplus sediment deposited per step.
    pub deposit_rate: f32,
    /// Water evaporated per step, fraction.
    pub evaporate: f32,
    /// Gravity for the speed update, m/s².
    pub gravity: f32,
}

impl Default for ErosionParams {
    fn default() -> Self {
        Self {
            droplets: 40_000,
            max_steps: 48,
            inertia: 0.06,
            sediment_capacity: 6.0,
            min_slope: 0.01,
            erode_rate: 0.35,
            deposit_rate: 0.25,
            evaporate: 0.02,
            gravity: 9.81,
        }
    }
}

/// Thermal erosion: cells steeper than `talus` (metres of height over one cell) leak material
/// toward their lowest neighbour. `iterations` sweeps; each moves `amount` of the excess.
///
/// Moves are collected per sweep and applied afterwards, so the visit order cannot bias the
/// result and every move keeps the total mass exactly.
///
/// A grid that is not `n × n` samples, or smaller than 3 × 3, is left untouched.
pub fn thermal_erode(heights: &mut [f32], n: u32, talus: f32, amount: f32, iterations: u32) {
    if n < 3 || heights.len() as u64 != u64::from(n) * u64::from(n) {
        return;
    }
    let mut field: Vec<f64> = heights.iter().map(|&h| h as f64).collect();
    thermal_erode_f64(&mut field, n, talus as f64, amount as f64, iterations);
    for (dst, src) in heights.iter_mut().zip(field) {
        *dst = src as f32;
    }
}

/// The f64 working core of [`thermal_erode`] (mass-exact moves).
fn thermal_erode_f64(field: &mut [f64], n: u32, talus: f64, amount: f64, iterations: u32) {
    let heights = field;
    let idx = |x: u32, z: u32| (z * n + x) as usize;
    for _ in 0..iterations {
        let mut moves: Vec<(usize, usize, f64)> = Vec::new();
        for z in 1..n - 1 {
            for x in 1..n - 1 {
                let h = heights[idx(x, z)];
                let mut best_d = 0.0f64;
                let mut lowest = (x, z);
                for (dx, dz) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                    let (nx, nz) = ((x as i32 + dx) as u32, (z as i32 + dz) as u32);
                    let hn = heights[idx(nx, nz)];
                    if h - hn > best_d {
                        best_d = h - hn;
                        lowest = (nx, nz);
                    }
                }
                if best_d > talus {
                    let move_h = (best_d - talus) * 0.5 * amount;
                    moves.push((idx(x, z), idx(lowest.0, lowest.1), move_h));
                }
            }
        }
        if moves.is_empty() {
            break;
        }
        for (from, to, move_h) in moves {
            heights[from] -= move_h;
            heights[to] += move_h;
        }
    }
}

/// Runs droplet hydraulic erosion in place over an `n × n` height grid (row-major).
///
/// Remaining sediment when a droplet dies is deposited at its last cell, so the pass is
/// mass-conserving up to floating-point rounding (verified in the tests).
///
/// A grid that is not `n × n` samples, or smaller than 4 × 4, is left untouched. So is a
/// grid passed with non-finite [`ErosionParams`]: a NaN coefficient would poison the
/// bilinear weights and write NaN heights, and a caller that produces one has a bug to fix,
/// not terrain to erode.
pub fn droplet_erode(heights: &mut [f32], n: u32, params: &ErosionParams, seed: u64) {
    if n < 4 || heights.len() as u64 != u64::from(n) * u64::from(n) {
        return;
    }
    if ![
        params.inertia,
        params.sediment_capacity,
        params.min_slope,
        params.erode_rate,
        params.deposit_rate,
        params.evaporate,
        params.gravity,
    ]
    .iter()
    .all(|v| v.is_finite())
    {
        return;
    }
    // Work in f64: a pass adds and subtracts millions of small amounts on ~100 m values, and
    // f32 rounding alone drifts the total mass by tenths of a percent.
    let mut field: Vec<f64> = heights.iter().map(|&h| h as f64).collect();
    droplet_erode_f64(&mut field, n, params, seed);
    for (dst, src) in heights.iter_mut().zip(field) {
        *dst = src as f32;
    }
}

/// The f64 working core of [`droplet_erode`].
fn droplet_erode_f64(field: &mut [f64], n: u32, params: &ErosionParams, seed: u64) {
    let mut rng = Rng::new(seed);
    let (inertia, min_slope, capacity_k, erode_rate, deposit_rate, evaporate, gravity) = (
        params.inertia as f64,
        params.min_slope as f64,
        params.sediment_capacity as f64,
        params.erode_rate as f64,
        params.deposit_rate as f64,
        params.evaporate as f64,
        params.gravity as f64,
    );
    let sample = |field: &[f64], x: u32, z: u32| hgt(field, n, x, z);
    for _ in 0..params.droplets {
        let mut pos = [
            f64::from(rng.range(1.0..(n - 2) as f32)),
            f64::from(rng.range(1.0..(n - 2) as f32)),
        ];
        let mut dir = [0.0f64, 0.0f64];
        let mut speed = 1.0f64;
        let mut water = 1.0f64;
        let mut sediment = 0.0f64;
        for _ in 0..params.max_steps {
            let cell = [pos[0].floor() as u32, pos[1].floor() as u32];
            let offset = [pos[0] - f64::from(cell[0]), pos[1] - f64::from(cell[1])];
            let h00 = sample(field, cell[0], cell[1]);
            let h10 = sample(field, cell[0] + 1, cell[1]);
            let h01 = sample(field, cell[0], cell[1] + 1);
            let h11 = sample(field, cell[0] + 1, cell[1] + 1);
            // Bilinear height and gradient at the droplet.
            let height = h00 * (1.0 - offset[0]) * (1.0 - offset[1])
                + h10 * offset[0] * (1.0 - offset[1])
                + h01 * (1.0 - offset[0]) * offset[1]
                + h11 * offset[0] * offset[1];
            let grad_x = (h10 - h00) * (1.0 - offset[1]) + (h11 - h01) * offset[1];
            let grad_z = (h01 - h00) * (1.0 - offset[0]) + (h11 - h10) * offset[0];

            dir[0] = dir[0] * inertia - grad_x * (1.0 - inertia);
            dir[1] = dir[1] * inertia - grad_z * (1.0 - inertia);
            let len = (dir[0] * dir[0] + dir[1] * dir[1]).sqrt();
            if len < 1e-9 {
                // Flat pit: random exit direction from a fixed 16-way table. The entries are
                // decimal f64 literals (the same decimal string parses to the same bits on
                // every platform); libm `sin`/`cos` are not correctly rounded, so calling them
                // here would make the eroded map depend on the host's math library.
                dir = EXIT_DIRS[(rng.next_u32() & 15) as usize].into();
            } else {
                dir[0] /= len;
                dir[1] /= len;
            }
            let new_pos = [pos[0] + dir[0], pos[1] + dir[1]];
            if new_pos[0] < 1.0
                || new_pos[1] < 1.0
                || new_pos[0] > f64::from(n - 2)
                || new_pos[1] > f64::from(n - 2)
            {
                break;
            }
            let new_cell = [new_pos[0].floor() as u32, new_pos[1].floor() as u32];
            let new_offset = [
                new_pos[0] - f64::from(new_cell[0]),
                new_pos[1] - f64::from(new_cell[1]),
            ];
            let nh00 = sample(field, new_cell[0], new_cell[1]);
            let nh10 = sample(field, new_cell[0] + 1, new_cell[1]);
            let nh01 = sample(field, new_cell[0], new_cell[1] + 1);
            let nh11 = sample(field, new_cell[0] + 1, new_cell[1] + 1);
            let new_height = nh00 * (1.0 - new_offset[0]) * (1.0 - new_offset[1])
                + nh10 * new_offset[0] * (1.0 - new_offset[1])
                + nh01 * (1.0 - new_offset[0]) * new_offset[1]
                + nh11 * new_offset[0] * new_offset[1];
            let delta = new_height - height;

            let capacity = (-delta).max(min_slope) * speed * water * capacity_k;
            if sediment >= capacity || delta > 0.0 {
                // Depositing (or uphill): drop the surplus, spread over the cell corners.
                let drop = if delta > 0.0 {
                    sediment.min(delta)
                } else {
                    (sediment - capacity) * deposit_rate
                };
                deposit(field, n, new_cell, new_offset, drop);
                sediment -= drop;
            } else {
                // Erode the capacity deficit from a 3×3 brush. Eroding only the cell under
                // the droplet feeds back into its own next gradient and runs away into a
                // trench; the brush (centre weight 0.4, as in public droplet-erosion
                // implementations) keeps the feedback bounded. The per-step dig is also
                // capped absolutely.
                let erode = ((capacity - sediment) * erode_rate).min(-delta).min(0.5);
                let removed = erode_brush(field, n, new_cell, erode);
                sediment += removed;
            }

            speed = (speed * speed + (-delta) * gravity).max(0.0).sqrt().min(30.0);
            water *= 1.0 - evaporate;
            pos = new_pos;
            if water < 0.02 {
                break;
            }
        }
        // Deposit what the droplet still carries where it died (mass conservation).
        let cell = [pos[0].floor() as u32, pos[1].floor() as u32];
        let offset = [pos[0] - f64::from(cell[0]), pos[1] - f64::from(cell[1])];
        deposit(field, n, cell, offset, sediment);
    }
}

/// 3×3 erosion brush weights (centre 0.4, edge 0.1, corner 0.05; sum 1).
const BRUSH: [[f64; 3]; 3] = [[0.05, 0.10, 0.05], [0.10, 0.40, 0.10], [0.05, 0.10, 0.05]];

/// Removes `amount` metres (× brush weights) from the 3×3 cells around `cell`, skipping
/// out-of-bounds cells. Returns the volume actually removed (mass-exact bookkeeping).
fn erode_brush(field: &mut [f64], n: u32, cell: [u32; 2], amount: f64) -> f64 {
    let mut removed = 0.0f64;
    for dz in -1i32..=1 {
        for dx in -1i32..=1 {
            let nx = cell[0] as i32 + dx;
            let nz = cell[1] as i32 + dz;
            if nx < 0 || nz < 0 || nx >= n as i32 || nz >= n as i32 {
                continue;
            }
            let w = BRUSH[(dz + 1) as usize][(dx + 1) as usize];
            field[(nz as u32 * n + nx as u32) as usize] -= amount * w;
            removed += amount * w;
        }
    }
    removed
}

/// Height at an integer cell of the row-major `n`-wide grid.
fn hgt(heights: &[f64], n: u32, x: u32, z: u32) -> f64 {
    heights[(z * n + x) as usize]
}

/// Spreads `amount` metres of deposition over the four corners of a cell, bilinear weights.
fn deposit(heights: &mut [f64], n: u32, cell: [u32; 2], offset: [f64; 2], amount: f64) {
    let w = [
        (1.0 - offset[0]) * (1.0 - offset[1]),
        offset[0] * (1.0 - offset[1]),
        (1.0 - offset[0]) * offset[1],
        offset[0] * offset[1],
    ];
    let corners = [
        (cell[0], cell[1]),
        (cell[0] + 1, cell[1]),
        (cell[0], cell[1] + 1),
        (cell[0] + 1, cell[1] + 1),
    ];
    for ((x, z), w) in corners.iter().zip(w) {
        if *x < n && *z < n {
            heights[(*z * n + *x) as usize] += amount * w;
        }
    }
}

/// Ridged fractal value noise in `0..=1`: octaves of `ridge = (1 − |2n−1|)²`.
///
/// The ridged form (sharp crests from inverting the noise around its mean) follows
/// Musgrave et al. 1989's observation that mountains erode into ridges; the value-noise
/// lattice and quintic fade are Perlin, "Improving Noise" (2002).
pub fn ridged_fbm(x: f32, y: f32, seed: u32, octaves: u32) -> f32 {
    let mut sum = 0.0f32;
    let mut amp = 1.0f32;
    let mut norm = 0.0f32;
    let mut freq = 1.0f32;
    for octave in 0..octaves {
        let n = value_noise2(x * freq, y * freq, seed.wrapping_add(octave * 101));
        let ridge = 1.0 - (2.0 * n - 1.0).abs();
        sum += ridge * ridge * amp;
        norm += amp;
        amp *= 0.5;
        freq *= 2.07;
    }
    // Zero octaves sum nothing: 0, not 0 / 0 = NaN (test-quality-sweep-2).
    if norm > 0.0 { sum / norm } else { 0.0 }
}

/// Generates metre-valued heights (row-major, row 0 = −Z edge, map centred on the origin)
/// for a synthetic mountainous island: domain-warped ridged fBm with a radial falloff.
///
/// The result is deterministic per `seed`; `height_scale` metres at the highest peak.
///
/// Returns an empty `Vec` — the inputs can only come from constants in tests, samples and
/// the editor viewport — when `n` is 0 or 1 (the sample spacing divides by `n − 1`) or
/// `n · n` does not fit a `u32` (`n` above 65 535), or when `world_width` is not finite or
/// not positive: any of those would write NaN heights or panic on the allocation.
pub fn synthetic_mountains(n: u32, world_width: f32, height_scale: f32, seed: u64) -> Vec<f32> {
    if !(2..=65_535).contains(&n) || !world_width.is_finite() || world_width <= 0.0 {
        return Vec::new();
    }
    let noise_seed = seed as u32;
    let half = world_width * 0.5;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world_width - half;
            let z = row as f32 / (n - 1) as f32 * world_width - half;
            // Normalised radial distance with a square-ish blend so the coast is not a circle.
            let nx = x / half;
            let nz = z / half;
            let r = (nx * nx + nz * nz).sqrt() * 0.55 + nx.abs().max(nz.abs()) * 0.45;
            let falloff = 1.0 - smoothstep01((r - 0.55) / 0.45);
            // Domain warp breaks up the straight lattice artefacts of value noise.
            let wx = value_noise2(x * 0.004 + 31.7, z * 0.004, noise_seed ^ 0x7a11) * 220.0;
            let wz = value_noise2(x * 0.004, z * 0.004 + 11.3, noise_seed ^ 0x3d2f) * 220.0;
            let ridge = ridged_fbm((x + wx) * 0.0016, (z + wz) * 0.0016, noise_seed, 6);
            let base = value_noise2(x * 0.011, z * 0.011, noise_seed ^ 0x55aa) * 0.18;
            // The deterministic powf kernel (no platform libm): a seed must reproduce the
            // map bit-for-bit on every OS, and glibc, the Windows CRT and bionic disagree.
            let h01 = det_powf(ridge * 0.92 + base, 1.35) * falloff;
            heights.push(h01 * height_scale);
        }
    }
    heights
}

fn smoothstep01(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact post-fix output of [`droplet_on_a_plateau_exits_at_a_pinned_cell`]
    /// (changed-sample index, `f32` bits): the pinned identity of the eroded map.
    const PLATEAU_CHANGED: &[(u32, u32)] = &[
        (0, 1084199224),
        (1, 1084612758),
        (2, 1085049516),
        (3, 1085571319),
        (4, 1086110840),
        (5, 1086693529),
        (6, 1087192036),
        (7, 1087252948),
        (8, 1087293714),
        (9, 1088016145),
        (10, 1089014588),
        (11, 1089839906),
        (12, 1090389695),
        (13, 1090650102),
        (14, 1090920104),
        (15, 1091216697),
        (16, 1091476744),
        (17, 1091710813),
        (18, 1091894719),
        (19, 1092086156),
        (20, 1092385113),
        (21, 1092753985),
        (22, 1093106946),
        (23, 1093389086),
        (24, 1093627217),
        (25, 1093892423),
        (26, 1094178580),
        (33, 1084374170),
        (34, 1087137325),
        (35, 1086248392),
        (36, 1087843759),
        (37, 1089601833),
        (38, 1089779020),
        (39, 1088009998),
        (40, 1088841482),
        (41, 1088863669),
        (42, 1089745101),
        (43, 1090297268),
        (44, 1088938431),
        (45, 1093449747),
        (46, 1091136367),
        (47, 1092051504),
        (48, 1091040384),
        (49, 1092505922),
        (50, 1092040992),
        (51, 1093157072),
        (52, 1091985979),
        (53, 1092788023),
        (54, 1093357383),
        (55, 1093276304),
        (56, 1093489443),
        (57, 1094039424),
        (58, 1094609264),
        (59, 1094255463),
        (60, 1094538433),
        (61, 1094829762),
        (66, 1084567999),
        (67, 1086727914),
        (68, 1088181546),
        (69, 1093538727),
        (70, 1086948325),
        (71, 1087603515),
        (72, 1091059710),
        (73, 1090997788),
        (74, 1088554349),
        (75, 1089605751),
        (76, 1087790208),
        (77, 1088382668),
        (78, 1090374124),
        (79, 1090889894),
        (80, 1091603339),
        (81, 1090744696),
        (82, 1092033951),
        (83, 1091935789),
        (84, 1093064479),
        (85, 1091886301),
        (86, 1092010360),
        (87, 1093080346),
        (88, 1093235138),
        (89, 1093497734),
        (90, 1093862413),
        (91, 1094134807),
        (92, 1094295176),
        (93, 1094567050),
        (94, 1094946179),
        (99, 1084847817),
        (100, 1087759612),
        (101, 1085350924),
        (102, 1089938441),
        (103, 1086912408),
        (104, 1087310599),
        (105, 1090587353),
        (106, 1088218631),
        (107, 1092002142),
        (108, 1091521755),
        (109, 1090944136),
        (110, 1088274475),
        (111, 1089147288),
        (112, 1090595699),
        (113, 1090885289),
        (114, 1091213828),
        (115, 1091443280),
        (116, 1092035578),
        (117, 1092400329),
        (118, 1093381417),
        (119, 1093087434),
        (120, 1092543733),
        (121, 1092802198),
        (122, 1093327074),
        (123, 1093910735),
        (124, 1094258995),
        (125, 1094523903),
        (126, 1094800577),
        (127, 1095091906),
        (132, 1085174953),
        (133, 1088845370),
        (134, 1088571388),
        (135, 1086556873),
        (136, 1091288092),
        (137, 1087270558),
        (138, 1088107112),
        (139, 1089382423),
        (140, 1088785053),
        (141, 1089080643),
        (142, 1091468595),
        (143, 1094304819),
        (144, 1088272454),
        (145, 1090469989),
        (146, 1091271425),
        (147, 1091450285),
        (148, 1091712141),
        (149, 1092053747),
        (150, 1092383674),
        (151, 1092765036),
        (152, 1093057071),
        (153, 1093275408),
        (154, 1093413967),
        (155, 1093502277),
        (156, 1093921597),
        (157, 1094299378),
        (158, 1094684035),
        (165, 1085506620),
        (166, 1086106122),
        (167, 1086534251),
        (168, 1086626834),
        (169, 1088775604),
        (170, 1089825898),
        (171, 1088021053),
        (172, 1089326860),
        (173, 1088988865),
        (174, 1093250760),
        (175, 1092654577),
        (176, 1094398509),
        (177, 1088825237),
        (178, 1089902201),
        (179, 1091793109),
        (180, 1091212104),
        (181, 1091671127),
        (182, 1092353707),
        (183, 1092378117),
        (184, 1092720926),
        (185, 1093140614),
        (186, 1093456694),
        (187, 1093739231),
        (188, 1093972963),
        (189, 1094237022),
        (190, 1094535669),
        (191, 1094829762),
        (198, 1085773987),
        (199, 1086123210),
        (200, 1086407025),
        (201, 1086960246),
        (202, 1087915887),
        (203, 1088948707),
        (204, 1088745498),
        (205, 1088568213),
        (206, 1089334710),
        (207, 1090148690),
        (208, 1090656359),
        (209, 1091312999),
        (210, 1089162590),
        (211, 1090461483),
        (212, 1095343088),
        (213, 1092120491),
        (214, 1090975103),
        (215, 1094573322),
        (216, 1092475770),
        (217, 1092931966),
        (218, 1092998124),
        (219, 1093394562),
        (220, 1093846497),
        (221, 1094147003),
        (222, 1094434806),
        (231, 1085855419),
        (232, 1090333943),
        (233, 1092311429),
        (234, 1090865648),
        (235, 1088102872),
        (237, 1089141391),
        (238, 1089514810),
        (239, 1089864363),
        (240, 1090539292),
        (241, 1090859794),
        (242, 1090854970),
        (243, 1090931789),
        (244, 1090782077),
        (245, 1091097864),
        (246, 1093234755),
        (247, 1090730306),
        (248, 1092796513),
        (249, 1092750076),
        (250, 1093295553),
        (251, 1093094786),
        (252, 1093630873),
        (253, 1093872255),
        (254, 1094107804),
        (255, 1094502843),
        (256, 1094829762),
        (264, 1085960483),
        (265, 1085647328),
        (266, 1086373107),
        (267, 1086940757),
        (268, 1088471270),
        (269, 1088853925),
        (271, 1089969527),
        (272, 1090412845),
        (273, 1090633031),
        (274, 1090904961),
        (275, 1091231242),
        (276, 1092550743),
        (277, 1092362129),
        (278, 1092814938),
        (279, 1092099078),
        (280, 1091953861),
        (281, 1092312467),
        (282, 1092574324),
        (283, 1092316564),
        (284, 1093635812),
        (285, 1093663673),
        (286, 1093947742),
        (287, 1094167503),
        (288, 1094503115),
        (289, 1094933688),
        (297, 1086374048),
        (298, 1087893361),
        (299, 1091161205),
        (300, 1087992541),
        (301, 1087342007),
        (302, 1088800302),
        (303, 1089556629),
        (304, 1090202567),
        (305, 1090636176),
        (306, 1090872358),
        (307, 1091107824),
        (308, 1091296947),
        (309, 1091605453),
        (310, 1091901879),
        (311, 1092123157),
        (312, 1092180894),
        (313, 1092394154),
        (314, 1092758986),
        (315, 1093549382),
        (316, 1093092284),
        (317, 1093050127),
        (318, 1093624708),
        (319, 1094080725),
        (320, 1094280711),
        (321, 1094636369),
        (322, 1095034426),
        (323, 1095333079),
        (324, 1095617930),
        (330, 1086796106),
        (331, 1087372480),
        (332, 1088068008),
        (333, 1087785749),
        (334, 1088144088),
        (335, 1088603000),
        (336, 1089350243),
        (337, 1090299927),
        (338, 1090739408),
        (339, 1091033887),
        (340, 1091291177),
        (341, 1091532420),
        (342, 1091802251),
        (343, 1092085297),
        (344, 1092332193),
        (345, 1092447552),
        (346, 1092396905),
        (347, 1093015549),
        (348, 1093368999),
        (349, 1093505867),
        (350, 1093466682),
        (351, 1093329666),
        (352, 1093773820),
        (353, 1094366802),
        (354, 1094866119),
        (355, 1095107849),
        (356, 1095337555),
        (357, 1095696912),
        (358, 1096011049),
        (363, 1087049059),
        (364, 1088015242),
        (365, 1089990188),
        (366, 1088364242),
        (367, 1088997184),
        (368, 1089273507),
        (369, 1089918303),
        (370, 1090280504),
        (371, 1090746066),
        (372, 1091078708),
        (373, 1091337105),
        (374, 1091620045),
        (375, 1091934618),
        (378, 1092694835),
        (379, 1092904550),
        (380, 1093219123),
        (382, 1093768824),
        (383, 1093920559),
        (384, 1093525860),
        (385, 1093457023),
        (386, 1094479753),
        (387, 1095001967),
        (388, 1095283710),
        (389, 1095464059),
        (390, 1095734154),
        (391, 1096083419),
        (392, 1096372455),
        (393, 1096664770),
        (396, 1087319975),
        (397, 1088409997),
        (398, 1088982636),
        (399, 1088440404),
        (400, 1089759693),
        (401, 1089906547),
        (402, 1090419515),
        (403, 1090659369),
        (404, 1090859348),
        (405, 1091000123),
        (406, 1091168792),
        (407, 1091487976),
        (408, 1100964481),
        (409, 1101004301),
        (415, 1100983786),
        (416, 1100980431),
        (417, 1094029460),
        (418, 1093649154),
        (419, 1094779004),
        (420, 1094923099),
        (421, 1095351424),
        (422, 1095722621),
        (423, 1095953860),
        (424, 1096119754),
        (425, 1096400085),
        (426, 1096781187),
        (429, 1087592660),
        (430, 1091060685),
        (431, 1091922513),
        (432, 1088694583),
        (433, 1089729210),
        (434, 1090169240),
        (435, 1090644039),
        (436, 1090891108),
        (437, 1091096636),
        (438, 1091175746),
        (439, 1091304215),
        (440, 1091618050),
        (441, 1100961490),
        (442, 1101003825),
        (443, 1101004750),
        (444, 1101004675),
        (445, 1101004644),
        (446, 1101004237),
        (447, 1101003972),
        (448, 1100983736),
        (449, 1101098367),
        (450, 1093923098),
        (451, 1094250216),
        (452, 1097885433),
        (453, 1095066989),
        (454, 1095416426),
        (455, 1095878983),
        (456, 1096139620),
        (457, 1096371593),
        (458, 1096634599),
        (459, 1096926914),
        (462, 1087821468),
        (463, 1088321080),
        (464, 1089364828),
        (465, 1089651834),
        (466, 1089562577),
        (467, 1090334906),
        (468, 1090742455),
        (469, 1091030856),
        (470, 1091289027),
        (471, 1091508512),
        (472, 1091734672),
        (473, 1092012264),
        (474, 1100990699),
        (475, 1101004361),
        (476, 1101004662),
        (477, 1101004324),
        (478, 1101004500),
        (479, 1101001753),
        (480, 1100998631),
        (481, 1100988686),
        (482, 1100807439),
        (483, 1093982786),
        (484, 1094051761),
        (485, 1098722424),
        (486, 1095194665),
        (487, 1095562459),
        (488, 1095961496),
        (489, 1096275770),
        (495, 1088038875),
        (496, 1092102083),
        (497, 1090324885),
        (498, 1089408807),
        (499, 1089291879),
        (500, 1090102550),
        (501, 1090771975),
        (502, 1091111562),
        (503, 1091399562),
        (504, 1091686665),
        (507, 1101004801),
        (508, 1101004798),
        (509, 1101004723),
        (510, 1101004675),
        (511, 1101005832),
        (512, 1101005215),
        (513, 1100992109),
        (514, 1100991375),
        (515, 1101195215),
        (516, 1094138834),
        (517, 1094026113),
        (518, 1096618915),
        (519, 1095270101),
        (520, 1095694211),
        (521, 1096021530),
        (522, 1096366532),
        (523, 1096645914),
        (524, 1096929185),
        (528, 1088175603),
        (529, 1090218261),
        (530, 1089720547),
        (531, 1092739425),
        (532, 1089845685),
        (533, 1089950988),
        (534, 1090911156),
        (535, 1091151885),
        (536, 1091430880),
        (537, 1091761714),
        (538, 1092047941),
        (539, 1092339406),
        (544, 1101004965),
        (545, 1101005826),
        (546, 1101001976),
        (547, 1101000783),
        (548, 1101179735),
        (549, 1094282930),
        (550, 1094659368),
        (551, 1095511049),
        (552, 1095714101),
        (553, 1095980289),
        (554, 1096234171),
        (555, 1096425614),
        (556, 1096644125),
        (557, 1097000983),
        (558, 1097320130),
        (561, 1088318850),
        (562, 1088945765),
        (563, 1091645976),
        (564, 1090304644),
        (565, 1089318887),
        (566, 1089945089),
        (567, 1090848417),
        (568, 1091249472),
        (569, 1091592198),
        (570, 1091802096),
        (571, 1092076481),
        (572, 1092455836),
        (581, 1101337141),
        (582, 1095201721),
        (583, 1094582528),
        (584, 1095660765),
        (585, 1095781856),
        (586, 1096061134),
        (587, 1096347396),
        (588, 1096597178),
        (589, 1096738093),
        (590, 1097038931),
        (591, 1097436547),
        (594, 1088631774),
        (595, 1089761545),
        (596, 1089649687),
        (597, 1090288705),
        (598, 1090579176),
        (599, 1090807728),
        (600, 1090810573),
        (601, 1091058488),
        (602, 1091538757),
        (603, 1091911925),
        (604, 1092282894),
        (605, 1092601550),
        (614, 1101187473),
        (615, 1098813430),
        (616, 1096721529),
        (617, 1097411218),
        (618, 1095926619),
        (619, 1095969661),
        (620, 1096325604),
        (621, 1096706721),
        (622, 1096974507),
        (623, 1097272940),
        (624, 1097582274),
        (627, 1088966079),
        (628, 1091424833),
        (629, 1091134522),
        (630, 1089702812),
        (631, 1092094175),
        (632, 1091132694),
        (633, 1091286225),
        (634, 1091320317),
        (635, 1091123253),
        (636, 1091771499),
        (637, 1092296757),
        (638, 1092690513),
        (640, 1100994885),
        (641, 1100984970),
        (642, 1100994885),
        (644, 1100991693),
        (645, 1100978586),
        (646, 1100991693),
        (647, 1101045636),
        (648, 1095754944),
        (649, 1095401935),
        (650, 1096262673),
        (651, 1095932286),
        (652, 1096149620),
        (653, 1096492911),
        (654, 1096700639),
        (655, 1097032534),
        (656, 1097435950),
        (660, 1089262136),
        (661, 1096700749),
        (662, 1090370828),
        (663, 1090842304),
        (664, 1095227106),
        (665, 1091289164),
        (666, 1091554238),
        (667, 1091719631),
        (668, 1091767610),
        (669, 1091638960),
        (670, 1091966176),
        (671, 1093223811),
        (672, 1101139068),
        (673, 1100890882),
        (674, 1100744202),
        (675, 1101273044),
        (676, 1101327420),
        (677, 1101019358),
        (678, 1101189831),
        (679, 1101307354),
        (680, 1101004937),
        (681, 1095375650),
        (682, 1095591687),
        (683, 1095935189),
        (684, 1096120445),
        (685, 1096308825),
        (686, 1096547489),
        (687, 1096856235),
        (688, 1097256997),
        (689, 1097581975),
        (693, 1089596232),
        (694, 1092924303),
        (695, 1090708564),
        (696, 1090684702),
        (697, 1095142770),
        (698, 1091204306),
        (699, 1091671324),
        (701, 1092165223),
        (702, 1092312294),
        (703, 1092461015),
        (704, 1092673661),
        (705, 1092736260),
        (706, 1092999928),
        (707, 1092741930),
        (708, 1093872948),
        (709, 1093800020),
        (710, 1094430435),
        (711, 1095379153),
        (712, 1095512782),
        (713, 1095117504),
        (714, 1095486142),
        (715, 1095817891),
        (716, 1096121097),
        (717, 1096356917),
        (718, 1096491609),
        (719, 1096737711),
        (720, 1097090845),
        (721, 1097409620),
        (722, 1097714035),
        (726, 1089877455),
        (727, 1091553270),
        (728, 1093403638),
        (729, 1091496057),
        (730, 1091006192),
        (731, 1091079484),
        (732, 1091599486),
        (733, 1091869511),
        (734, 1092173900),
        (735, 1092529448),
        (736, 1092784571),
        (737, 1092820918),
        (738, 1092792049),
        (739, 1092828519),
        (740, 1093524183),
        (741, 1094086267),
        (742, 1093794599),
        (743, 1094115272),
        (744, 1094615311),
        (745, 1095054086),
        (746, 1095258211),
        (747, 1095450494),
        (748, 1095813952),
        (749, 1096216875),
        (750, 1096521296),
        (751, 1096770610),
        (752, 1097006640),
        (753, 1097185679),
        (754, 1097443416),
        (755, 1097801834),
        (756, 1098106562),
        (759, 1090142760),
        (760, 1090943304),
        (761, 1090719174),
        (762, 1090721098),
        (763, 1091501514),
        (764, 1090817075),
        (765, 1091147866),
        (766, 1091359100),
        (767, 1091765626),
        (768, 1092367714),
        (769, 1092878292),
        (770, 1093156701),
        (771, 1093426998),
        (772, 1093776855),
        (773, 1098148528),
        (774, 1097461055),
        (775, 1094201951),
        (776, 1094509157),
        (777, 1094879209),
        (778, 1095281447),
        (779, 1095560630),
        (780, 1095784015),
        (781, 1095964422),
        (782, 1096253777),
        (783, 1096623279),
        (784, 1096926914),
        (785, 1097190141),
        (786, 1097424750),
        (787, 1097657192),
        (788, 1097858945),
        (789, 1098222979),
        (792, 1090474739),
        (793, 1090914513),
        (794, 1091152352),
        (795, 1091203321),
        (796, 1091255505),
        (797, 1091174819),
        (798, 1091384313),
        (799, 1091540211),
        (800, 1091707176),
        (801, 1092393552),
        (802, 1092806089),
        (803, 1093186605),
        (804, 1093559324),
        (805, 1093894041),
        (807, 1094419535),
        (808, 1094613418),
        (809, 1094803658),
        (810, 1095112312),
        (811, 1095331842),
        (812, 1095718617),
        (813, 1096010561),
        (814, 1096245783),
        (815, 1096478704),
        (816, 1096679843),
        (817, 1097043331),
        (820, 1097844418),
        (821, 1098091907),
        (822, 1098368706),
        (828, 1091422853),
        (829, 1091632052),
        (830, 1091740890),
        (831, 1091650171),
        (832, 1091963608),
        (833, 1092371191),
        (834, 1092581800),
        (835, 1093091983),
        (836, 1093307033),
        (837, 1093471604),
        (838, 1093921071),
        (839, 1094240701),
        (840, 1094553044),
        (841, 1094833350),
        (842, 1095063884),
        (843, 1095219404),
        (844, 1095427884),
        (845, 1095808277),
        (846, 1096140806),
        (848, 1096664770),
        (849, 1096912259),
        (850, 1097189058),
        (863, 1092009089),
        (864, 1091999549),
        (865, 1092114884),
        (866, 1092573865),
        (867, 1092967657),
        (868, 1093305590),
        (869, 1093592456),
        (870, 1093805596),
        (871, 1093916346),
        (872, 1094117202),
        (873, 1094562677),
        (874, 1094911325),
        (875, 1095210785),
        (876, 1095438412),
        (877, 1095587702),
        (878, 1095854565),
        (879, 1096218074),
        (880, 1096511529),
        (881, 1096799105),
        (896, 1092201490),
        (897, 1092399627),
        (898, 1092598673),
        (899, 1092780050),
        (900, 1093002575),
        (901, 1093333965),
        (902, 1093701710),
        (903, 1094008836),
        (904, 1094221738),
        (905, 1094331942),
        (906, 1094492058),
        (907, 1094866888),
        (908, 1095289427),
        (909, 1095601699),
        (910, 1095851838),
        (911, 1096084981),
        (912, 1096267837),
        (913, 1096554255),
        (914, 1096906868),
        (915, 1097197753),
        (931, 1092864479),
        (932, 1093089124),
        (933, 1093302464),
        (934, 1093453055),
        (935, 1093647829),
        (936, 1094103395),
        (937, 1094422936),
        (938, 1094655112),
        (939, 1094853968),
        (940, 1095024761),
        (941, 1095328000),
        (942, 1095730447),
        (944, 1096272167),
        (945, 1096508878),
        (946, 1096761755),
        (947, 1096980099),
        (948, 1097255909),
        (949, 1097563449),
        (966, 1093520276),
        (967, 1093740824),
        (968, 1093958903),
        (969, 1094174717),
        (970, 1094552963),
        (972, 1095091906),
        (973, 1095323659),
        (974, 1095584721),
        (975, 1095877255),
        (979, 1096935609),
        (980, 1097124837),
        (981, 1097192069),
        (982, 1097661043),
        (1001, 1094174402),
        (1002, 1094421891),
        (1003, 1094698690),
        (1013, 1097301305),
        (1014, 1097529971),
        (1015, 1097825593),
    ];

    #[test]
    fn mountains_are_deterministic_and_bounded() {
        let a = synthetic_mountains(129, 1024.0, 260.0, 42);
        let b = synthetic_mountains(129, 1024.0, 260.0, 42);
        assert_eq!(a, b, "same seed must reproduce the map");
        let c = synthetic_mountains(129, 1024.0, 260.0, 43);
        assert_ne!(a, c, "different seed must differ");
        assert_eq!(a.len(), 129 * 129);
        for h in &a {
            assert!((0.0..=260.0).contains(h), "height {h} out of range");
        }
        // There is real relief: at least 30% of the range in use.
        let max = a.iter().copied().fold(f32::MIN, f32::max);
        assert!(max > 0.3 * 260.0, "map too flat: max {max}");
        // Edges fall to sea level (the island falloff).
        let edge = a[65]; // (row 0, col 65): the middle of the north edge
        assert!(edge < 0.05 * 260.0, "edge height {edge}");
    }

    #[test]
    fn droplet_erosion_is_mass_conserving() {
        let mut a = synthetic_mountains(97, 512.0, 120.0, 7);
        let before: f64 = a.iter().map(|&h| h as f64).sum();
        let params = ErosionParams {
            droplets: 3000,
            ..Default::default()
        };
        droplet_erode(&mut a, 97, &params, 7);
        let after: f64 = a.iter().map(|&h| h as f64).sum();
        let rel = ((after - before) / before).abs();
        assert!(rel < 1e-3, "mass not conserved: relative change {rel}");
    }

    #[test]
    fn droplet_erosion_carves_and_stays_bounded() {
        let mut a = synthetic_mountains(129, 512.0, 120.0, 11);
        let before = ruggedness(&a);
        let (min_before, max_before) = a
            .iter()
            .fold((f32::MAX, f32::MIN), |(mn, mx), &h| (mn.min(h), mx.max(h)));
        droplet_erode(
            &mut a,
            129,
            &ErosionParams {
                droplets: 2000,
                sediment_capacity: 2.0,
                erode_rate: 0.2,
                ..Default::default()
            },
            11,
        );
        let after = ruggedness(&a);
        // The pass changed the field materially...
        assert!(
            (after - before).abs() / before > 0.05,
            "erosion should reshape the terrain: {after} vs {before}"
        );
        // ...but stays bounded: no trench/ridge runaway, and total variation within 4×.
        assert!(
            after < before * 4.0,
            "erosion exploded variation: {after} vs {before}"
        );
        let (min_after, max_after) = a
            .iter()
            .fold((f32::MAX, f32::MIN), |(mn, mx), &h| (mn.min(h), mx.max(h)));
        assert!(min_after > min_before - 20.0 && max_after < max_before + 20.0);
    }

    /// Total variation (sum of |Δh| between 4-neighbours) — a ruggedness proxy.
    fn ruggedness(heights: &[f32]) -> f64 {
        let n = (heights.len() as f64).sqrt() as u32;
        let mut tv = 0.0f64;
        for z in 0..n - 1 {
            for x in 0..n - 1 {
                let h = heights[(z * n + x) as usize] as f64;
                tv += (heights[((z + 1) * n + x) as usize] as f64 - h).abs();
                tv += (heights[(z * n + x + 1) as usize] as f64 - h).abs();
            }
        }
        tv
    }

    #[test]
    fn thermal_pass_flattens_oversteep_cells() {
        // A single spike in a flat field.
        let n = 33u32;
        let mut h = vec![10.0f32; (n * n) as usize];
        h[(16 * n + 16) as usize] = 20.0;
        thermal_erode(&mut h, n, 0.05, 1.0, 8);
        let spike = h[(16 * n + 16) as usize];
        assert!(spike < 20.0, "spike must settle: {spike}");
        let after: f64 = h.iter().map(|&v| f64::from(v)).sum();
        assert!(
            (-1e-2..=1e-2).contains(&(after - 10900.0)),
            "thermal pass must keep mass: {after}"
        );
        assert!(h.iter().all(|&v| (10.0 - 1e-4..=20.0).contains(&v)));
    }

    /// audit sprint-audit-g-terrain3-determinism (Medium): on a flat plateau the droplet's exit
    /// direction must come from the engine's fixed 16-way table, not from libm `f64::sin`/`cos`
    /// (not correctly rounded; glibc and the Windows CRT disagree on many inputs). The changed
    /// samples and their exact `f32` bits are pinned so a host math library can never steer the
    /// eroded map.
    #[test]
    fn droplet_on_a_plateau_exits_at_a_pinned_cell() {
        let n = 33u32;
        let mut h = vec![0.0f32; (n * n) as usize];
        for z in 0..n {
            for x in 0..n {
                h[(z * n + x) as usize] = if (12..21).contains(&x) && (12..21).contains(&z) {
                    20.0
                } else {
                    // Sloped apron around the plateau: non-zero gradient everywhere off it.
                    5.0 + x as f32 * 0.25 + z as f32 * 0.125
                };
            }
        }
        let before = h.clone();
        droplet_erode(
            &mut h,
            n,
            &ErosionParams {
                droplets: 64,
                max_steps: 48,
                ..Default::default()
            },
            7,
        );
        let changed: Vec<(u32, u32)> = h
            .iter()
            .zip(&before)
            .enumerate()
            .filter(|(_, (a, b))| a.to_bits() != b.to_bits())
            .map(|(i, (a, _))| (i as u32, a.to_bits()))
            .collect();
        assert!(!changed.is_empty(), "the erosion pass must change the apron");
        assert_eq!(changed, PLATEAU_CHANGED);
    }

    #[test]
    fn erosion_ignores_malformed_grids_instead_of_panicking() {
        let mut empty: Vec<f32> = Vec::new();
        thermal_erode(&mut empty, 0, 0.05, 1.0, 2);
        droplet_erode(&mut empty, 0, &ErosionParams::default(), 1);
        let mut short = vec![1.0f32; 10];
        thermal_erode(&mut short, 8, 0.05, 1.0, 2);
        droplet_erode(&mut short, 8, &ErosionParams::default(), 1);
        assert!(short.iter().all(|&v| v == 1.0));
        let mut tiny = vec![1.0f32; 4];
        droplet_erode(&mut tiny, 2, &ErosionParams::default(), 1);
        assert_eq!(tiny, vec![1.0f32; 4]);
    }

    #[test]
    fn droplet_erode_ignores_non_finite_params_instead_of_writing_nan() {
        // A NaN in any coefficient poisons the bilinear weights and the deposit brush
        // (0 * NaN is NaN), writing NaN heights. Non-finite params are the caller's bug:
        // refuse the pass, like a malformed grid, and leave the heights untouched.
        for params in [
            ErosionParams {
                inertia: f32::NAN,
                ..Default::default()
            },
            ErosionParams {
                sediment_capacity: f32::NAN,
                ..Default::default()
            },
            ErosionParams {
                evaporate: f32::NAN,
                ..Default::default()
            },
        ] {
            let mut h = vec![5.0f32; 33 * 33];
            droplet_erode(&mut h, 33, &params, 7);
            assert!(
                h.iter().all(|&v| v.to_bits() == 5.0f32.to_bits()),
                "params {params:?} changed the grid"
            );
        }
    }

    #[test]
    fn synthetic_mountains_refuses_degenerate_arguments() {
        // n = 1 divides by n − 1 (NaN heights); n · n must fit a u32 or the allocation
        // overflows and panics in debug builds; a non-finite or non-positive world width
        // gives NaN/inf coordinates. Refuse instead of writing garbage.
        assert!(synthetic_mountains(1, 1024.0, 260.0, 42).is_empty());
        assert!(synthetic_mountains(0, 1024.0, 260.0, 42).is_empty());
        assert!(synthetic_mountains(65_536, 1024.0, 260.0, 42).is_empty());
        for w in [0.0, -32.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                synthetic_mountains(33, w, 260.0, 42).is_empty(),
                "world width {w}"
            );
        }
        // Valid arguments still generate the full grid.
        assert_eq!(synthetic_mountains(33, 1024.0, 260.0, 42).len(), 33 * 33);
    }

    #[test]
    fn mountains_use_the_deterministic_powf_kernel() {
        // Platform libm `powf` is not correctly rounded (glibc, the Windows CRT and Android's
        // bionic disagree on some inputs), and this seeded path feeds tests, samples and the
        // editor viewport: recompute the shape with the engine's deterministic kernel and
        // require bit equality on every sample. (On the build host's glibc the platform kernel
        // deviates from the deterministic one on 14 of these 16641 samples.)
        let n = 129u32;
        let world_width = 1024.0f32;
        let height_scale = 260.0f32;
        let seed = 42u64;
        let got = synthetic_mountains(n, world_width, height_scale, seed);
        assert_eq!(got.len(), (n * n) as usize);
        let noise_seed = seed as u32;
        let half = world_width * 0.5;
        let mut i = 0usize;
        for row in 0..n {
            for col in 0..n {
                let x = col as f32 / (n - 1) as f32 * world_width - half;
                let z = row as f32 / (n - 1) as f32 * world_width - half;
                let nx = x / half;
                let nz = z / half;
                let r = (nx * nx + nz * nz).sqrt() * 0.55 + nx.abs().max(nz.abs()) * 0.45;
                let falloff = 1.0 - smoothstep01((r - 0.55) / 0.45);
                let wx = value_noise2(x * 0.004 + 31.7, z * 0.004, noise_seed ^ 0x7a11) * 220.0;
                let wz = value_noise2(x * 0.004, z * 0.004 + 11.3, noise_seed ^ 0x3d2f) * 220.0;
                let ridge = ridged_fbm((x + wx) * 0.0016, (z + wz) * 0.0016, noise_seed, 6);
                let base = value_noise2(x * 0.011, z * 0.011, noise_seed ^ 0x55aa) * 0.18;
                let want = det_powf(ridge * 0.92 + base, 1.35) * falloff * height_scale;
                assert_eq!(
                    got[i].to_bits(),
                    want.to_bits(),
                    "sample {i} (col {col}, row {row})"
                );
                i += 1;
            }
        }
    }
}
