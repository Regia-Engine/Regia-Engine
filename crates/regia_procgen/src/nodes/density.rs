// SPDX-License-Identifier: MPL-2.0
//! Density nodes (`density.*`, spec 63 § 5). Every result is clamped to [0, 1].

use regia_core::math::Vec3;
use regia_core::math::hash::{mix, pcg_hash};

use crate::data::{Bundle, Payload, PointSet};
use crate::detmath;
use crate::exec::{CHUNK, FiniteCheck, NodeCtx};
use crate::nodes::common::{read_num, rot};
use crate::nodes::{
    BuiltinNode, NodeError, SettingType as T, Settings, eval_curve, num, pins_points_inout, set,
};

const MODES: &[&str] = &["mul", "set", "add", "min", "max"];

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::in_place(
        "density.remap",
        "density",
        "Linear remap of density from `from` to `to` (collapsed `from` maps to the middle of `to`)",
        &[
            set("from", T::Range2, "[0.0, 1.0]", "Source range"),
            set("to", T::Range2, "[0.0, 1.0]", "Target range"),
            set(
                "outside",
                T::Enum(&["remap", "keep"]),
                "remap",
                "Values outside `from`: remap or keep",
            ),
        ],
        pins_points_inout,
        eval_remap,
    ),
    BuiltinNode::in_place(
        "density.curve",
        "density",
        "Density through a piecewise-linear curve",
        &[set(
            "curve",
            T::Curve,
            "[(0.0, 0.0), (1.0, 1.0)]",
            "(x, y) keys sorted by x",
        )],
        pins_points_inout,
        eval_curve_node,
    ),
    BuiltinNode::in_place(
        "density.noise",
        "density",
        "Value-noise fBm of position.xz / scale_m, combined by `mode`",
        &[
            num("scale_m", T::Float, "32.0", 0.01, 1.0e6, "Feature size, meters"),
            num("octaves", T::Int, "3", 1.0, 12.0, "Octaves"),
            num("gain", T::Float, "0.5", 0.0, 1.0, "Amplitude per octave"),
            num("lacunarity", T::Float, "2.0", 1.0, 8.0, "Frequency per octave"),
            set("mode", T::Enum(MODES), "mul", "Combine"),
            num("min", T::Float, "0.0", 0.0, 1.0, "Noise output low"),
            num("max", T::Float, "1.0", 0.0, 1.0, "Noise output high"),
        ],
        pins_points_inout,
        eval_noise,
    ),
    BuiltinNode::in_place(
        "density.ground_layer",
        "density",
        "A terrain layer's weight under each point (a `Ground` item from `in.ground` with `layers`), combined by `mode`",
        &[
            crate::nodes::set("layer", T::Name, "\"\"", "The layer"),
            crate::nodes::set("mode", T::Enum(MODES), "mul", "Combine"),
        ],
        |_| {
            crate::nodes::PinSpec::new(
                vec![
                    crate::nodes::pin_req("in", crate::nodes::POINTS),
                    crate::nodes::pin_req("ground", crate::nodes::kind(crate::data::DataKind::Ground)),
                ],
                vec![crate::nodes::pin_out("out", crate::nodes::POINTS)],
            )
        },
        eval_ground_layer,
    ),
    BuiltinNode::in_place(
        "density.facing",
        "density",
        "Thins by the angle between the point's up (or an attribute) and `dir`",
        &[
            set("dir", T::Vec3, "(0.0, 1.0, 0.0)", "Reference direction"),
            set(
                "from",
                T::Enum(&["rotation_up", "attr"]),
                "rotation_up",
                "Direction source",
            ),
            set("attr", T::Selector, "\"normal\"", "Attribute when `from: attr`"),
            num(
                "lo_deg",
                T::Float,
                "0.0",
                0.0,
                180.0,
                "Full density below this angle",
            ),
            num(
                "hi_deg",
                T::Float,
                "45.0",
                0.0,
                180.0,
                "Zero density above this angle",
            ),
            set("mode", T::Enum(MODES), "mul", "Combine"),
        ],
        pins_points_inout,
        eval_facing,
    ),
    BuiltinNode::in_place(
        "density.realize",
        "density",
        "Turns density into existence: random (u < d, slot 0) or threshold; kept points get density 1",
        &[
            set("mode", T::Enum(&["random", "threshold"]), "random", "Rule"),
            num(
                "threshold",
                T::Float,
                "0.5",
                0.0,
                1.0,
                "Threshold for `threshold`",
            ),
        ],
        pins_points_inout,
        eval_realize,
    ),
];

/// Combines a density with a factor by mode name; clamped to [0, 1].
pub(crate) fn combine(mode: &str, d: f32, f: f32) -> f32 {
    Combine::parse(mode).apply(d, f)
}

/// A density combine mode parsed once per node rather than matched by name per point.
#[derive(Clone, Copy)]
pub(crate) enum Combine {
    Set,
    Add,
    Min,
    Max,
    Mul,
}

impl Combine {
    /// The mode named `mode` (anything unknown multiplies, as before).
    pub(crate) fn parse(mode: &str) -> Self {
        match mode {
            "set" => Self::Set,
            "add" => Self::Add,
            "min" => Self::Min,
            "max" => Self::Max,
            _ => Self::Mul,
        }
    }

    /// `d` combined with `f`, clamped to [0, 1].
    pub(crate) fn apply(self, d: f32, f: f32) -> f32 {
        let v = match self {
            Self::Set => f,
            Self::Add => d + f,
            Self::Min => d.min(f),
            Self::Max => d.max(f),
            Self::Mul => d * f,
        };
        v.clamp(0.0, 1.0)
    }
}

/// Applies `f(set, i) -> new density` to every points item of input 0.
fn map_density(
    inputs: &mut [Bundle],
    mut f: impl FnMut(&PointSet, usize) -> Result<f32, NodeError>,
) -> Result<Vec<Bundle>, NodeError> {
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        if let Payload::Points(p) = &mut item.payload {
            let new: Vec<f32> = (0..p.len()).map(|i| f(p, i)).collect::<Result<_, _>>()?;
            for (d, n) in p.density.iter_mut().zip(new) {
                *d = n.clamp(0.0, 1.0);
            }
            item.last_written = Some("density".to_string());
        }
    }
    Ok(vec![out])
}

fn eval_ground_layer(
    ctx: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let name = s.text("layer");
    let mode = Combine::parse(s.text("mode"));
    let ground = crate::nodes::input_items(inputs, 1)
        .iter()
        .find_map(|i| match &i.payload {
            Payload::Ground(g) if g.layers.iter().any(|l| l.name == name) => Some(g.clone()),
            _ => None,
        });
    let mut out = crate::nodes::take_input(inputs, 0);
    let Some(ground) = ground else {
        return Err(NodeError::new(
            "layer_missing",
            format!("no ground item carries the layer `{name}`: name it in `in.ground`'s `layers`"),
        ));
    };
    ctx.finite_check = FiniteCheck::Density;
    for item in &mut out.0 {
        if let Payload::Points(p) = &mut item.payload {
            let density = p.density.make_mut();
            for (d, q) in density.iter_mut().zip(p.position.iter()) {
                let w = ground.layer_weight(name, q[0], q[2]).unwrap_or(0.0);
                *d = mode.apply(*d, w);
            }
            item.last_written = Some("density".to_string());
        }
    }
    Ok(vec![out])
}

fn eval_remap(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let [a, b] = s.range2("from");
    let [c, e] = s.range2("to");
    let keep = s.text("outside") == "keep";
    map_density(inputs, |p, i| {
        let d = f64::from(p.density[i]);
        let lo = a.min(b);
        let hi = a.max(b);
        if keep && (d < lo || d > hi) {
            return Ok(d as f32);
        }
        let v = if a == b {
            (c + e) * 0.5
        } else {
            c + (d - a) / (b - a) * (e - c)
        };
        Ok(v as f32)
    })
}

fn eval_curve_node(
    _: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let curve = s.curve("curve");
    map_density(inputs, |p, i| {
        Ok(eval_curve(&curve, f64::from(p.density[i])) as f32)
    })
}

/// The lattice hashes of the cell one octave read last. Points in spatial order stay in a cell for
/// many rows, so they pay for its hashes once.
#[derive(Clone, Copy)]
struct NoiseCell {
    key: Option<(i32, i32)>,
    /// Corners `(i, j)`, `(i + 1, j)`, `(i, j + 1)`, `(i + 1, j + 1)`.
    corners: [f64; 4],
}

impl NoiseCell {
    const EMPTY: Self = Self {
        key: None,
        corners: [0.0; 4],
    };
}

/// Value noise on an integer lattice hashed from the node seed; smoothstep interpolation.
/// `seed_hash` is `pcg_hash` of the octave's lattice seed.
#[inline]
fn value_noise(x: f64, z: f64, seed_hash: u32, cell: &mut NoiseCell) -> f64 {
    let (fx, fz) = (detmath::floor(x), detmath::floor(z));
    let (ix, iz) = (fx as i64 as i32, fz as i64 as i32);
    let (tx, tz) = (x - fx, z - fz);
    if cell.key != Some((ix, iz)) {
        // A corner is `mix(mix(i, j), seed)`, that is `pcg_hash(pcg_hash(i ^ pcg_hash(j)) ^ pcg_hash(seed))`:
        // the seed's hash is per octave and each row's hash serves two corners. Dividing a 24-bit
        // integer by 2^24 is exact, so multiplying by the inverse gives the same bits.
        let corner = |i: i32, row: u32| {
            f64::from(pcg_hash(pcg_hash(i as u32 ^ row) ^ seed_hash) >> 8) * (1.0 / 16_777_216.0)
        };
        // The lattice wraps at the `i32` edge (a release build always did): `+ 1` on `i32::MAX`
        // panicked in a debug build (corr-procgen).
        let (ix1, iz1) = (ix.wrapping_add(1), iz.wrapping_add(1));
        let (row0, row1) = (pcg_hash(iz as u32), pcg_hash(iz1 as u32));
        cell.corners = [
            corner(ix, row0),
            corner(ix1, row0),
            corner(ix, row1),
            corner(ix1, row1),
        ];
        cell.key = Some((ix, iz));
    }
    let [h00, h10, h01, h11] = cell.corners;
    let sx = tx * tx * (3.0 - 2.0 * tx);
    let sz = tz * tz * (3.0 - 2.0 * tz);
    let a = h00 + (h10 - h00) * sx;
    let b = h01 + (h11 - h01) * sx;
    a + (b - a) * sz
}

/// Normalized value-noise fBm in [0, 1].
pub(crate) fn fbm(x: f64, z: f64, octaves: i64, gain: f64, lacunarity: f64, seed: u32) -> f64 {
    Fbm::new(octaves, gain, lacunarity, seed).eval(x, z)
}

/// [`fbm`] with its per-octave amplitude, frequency and lattice seed hash computed once (the same
/// values in the same order, so results are bit-identical).
pub(crate) struct Fbm {
    /// `(amplitude, frequency, lattice seed hash)` per octave.
    octaves: Vec<(f64, f64, u32)>,
    norm: f64,
}

impl Fbm {
    pub(crate) fn new(octaves: i64, gain: f64, lacunarity: f64, seed: u32) -> Self {
        let (mut norm, mut amp, mut freq) = (0.0, 1.0, 1.0);
        let mut list = Vec::new();
        for o in 0..octaves.max(1) {
            list.push((amp, freq, pcg_hash(mix(seed, o as u32))));
            norm += amp;
            amp *= gain;
            freq *= lacunarity;
        }
        Self { octaves: list, norm }
    }

    pub(crate) fn eval(&self, x: f64, z: f64) -> f64 {
        let mut sum = 0.0;
        for &(amp, freq, seed_hash) in &self.octaves {
            let mut cell = NoiseCell::EMPTY;
            sum += amp * value_noise(x * freq, z * freq, seed_hash, &mut cell);
        }
        if self.norm > 0.0 { sum / self.norm } else { 0.0 }
    }

    /// [`Self::eval`] over many points with per-octave cells kept between them; the result does not
    /// depend on the order points are read in.
    fn sampler(&self) -> FbmSampler<'_> {
        FbmSampler {
            fbm: self,
            cells: vec![NoiseCell::EMPTY; self.octaves.len()],
        }
    }
}

/// See [`Fbm::sampler`].
struct FbmSampler<'a> {
    fbm: &'a Fbm,
    cells: Vec<NoiseCell>,
}

impl FbmSampler<'_> {
    #[inline]
    fn eval(&mut self, x: f64, z: f64) -> f64 {
        let mut sum = 0.0;
        for (&(amp, freq, seed_hash), cell) in self.fbm.octaves.iter().zip(&mut self.cells) {
            sum += amp * value_noise(x * freq, z * freq, seed_hash, cell);
        }
        if self.fbm.norm > 0.0 {
            sum / self.fbm.norm
        } else {
            0.0
        }
    }
}

fn eval_noise(ctx: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let mut out = crate::nodes::take_input(inputs, 0);
    let scale = s.f("scale_m");
    let (oct, gain, lac) = (s.i("octaves"), s.f("gain"), s.f("lacunarity"));
    let (lo, hi) = (s.f("min"), s.f("max"));
    let mode = Combine::parse(s.text("mode"));
    let noise = Fbm::new(oct, gain, lac, ctx.node_seed);
    ctx.finite_check = FiniteCheck::Density;
    for item in &mut out.0 {
        if let Payload::Points(p) = &mut item.payload {
            // Chunks of 4096 points in parallel, each written in place: every point's value depends
            // only on that point, so the result is the serial one (spec 62 § 5.1).
            let jobs: Vec<_> = p
                .position
                .chunks(CHUNK)
                .zip(p.density.chunks_mut(CHUNK))
                .collect();
            ctx.for_each_job(jobs, |(pos, dens)| {
                let mut sampler = noise.sampler();
                for ([x, _, z], d) in pos.iter().zip(dens) {
                    let n = sampler.eval(f64::from(*x) / scale, f64::from(*z) / scale);
                    *d = mode.apply(*d, (lo + (hi - lo) * n) as f32).clamp(0.0, 1.0);
                }
            })?;
            item.last_written = Some("density".to_string());
        }
    }
    Ok(vec![out])
}

fn eval_facing(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let dir = Vec3::from(s.v3("dir").map(|v| v as f32));
    let (lo, hi) = (s.f("lo_deg"), s.f("hi_deg"));
    let mode = s.text("mode").to_string();
    let from_attr = s.text("from") == "attr";
    let sel = s.sel("attr");
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let attr = if from_attr {
            Some(read_num(item, &sel)?)
        } else {
            None
        };
        if let Some(a) = &attr
            && a.width != 3
        {
            return Err(NodeError::new("attr_type_mismatch", "`attr` must be a Vec3"));
        }
        if let Payload::Points(p) = &mut item.payload {
            for i in 0..p.len() {
                let up = match &attr {
                    Some(a) => {
                        let v = a.get(i);
                        Vec3::new(v[0] as f32, v[1] as f32, v[2] as f32)
                    }
                    None => rot(p, i) * Vec3::Y,
                };
                let denom = up.length() * dir.length();
                let cos = if denom > 0.0 {
                    f64::from(up.dot(dir) / denom)
                } else {
                    1.0
                };
                let theta = detmath::acos(cos.clamp(-1.0, 1.0)).to_degrees();
                let f = if hi <= lo || theta >= hi {
                    // A swapped or empty window thins everything (spec 63: 0 above `hi_deg`).
                    0.0
                } else if theta <= lo {
                    1.0
                } else {
                    1.0 - (theta - lo) / (hi - lo)
                };
                p.density[i] = combine(&mode, p.density[i], f as f32);
            }
            item.last_written = Some("density".to_string());
        }
    }
    Ok(vec![out])
}

fn eval_realize(
    ctx: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let random = s.text("mode") == "random";
    let t = s.f("threshold") as f32;
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        if let Payload::Points(p) = &item.payload {
            let keep: Vec<bool> = (0..p.len())
                .map(|i| {
                    let d = p.density[i];
                    if random {
                        ctx.draw(p.seed[i], 0) < f64::from(d)
                    } else {
                        d >= t
                    }
                })
                .collect();
            let (mut kept, _) = p.partition(&keep);
            kept.density.iter_mut().for_each(|d| *d = 1.0);
            item.payload = Payload::Points(kept);
        }
    }
    Ok(vec![out])
}

#[cfg(test)]
mod noise_tests {
    use super::*;

    /// The formula as the spec gives it: every corner hashed in full, `floor` and the division by
    /// 2^24 written out.
    fn plain_value_noise(x: f64, z: f64, seed: u32) -> f64 {
        let (fx, fz) = (x.floor(), z.floor());
        let (ix, iz) = (fx as i64 as i32, fz as i64 as i32);
        let (tx, tz) = (x - fx, z - fz);
        let h = |i: i32, j: i32| f64::from(mix(mix(i as u32, j as u32), seed) >> 8) / 16_777_216.0;
        let sx = tx * tx * (3.0 - 2.0 * tx);
        let sz = tz * tz * (3.0 - 2.0 * tz);
        let (ix1, iz1) = (ix.wrapping_add(1), iz.wrapping_add(1));
        let a = h(ix, iz) + (h(ix1, iz) - h(ix, iz)) * sx;
        let b = h(ix, iz1) + (h(ix1, iz1) - h(ix, iz1)) * sx;
        a + (b - a) * sz
    }

    /// A lattice cell at the `i32` edge: `+ 1` on `i32::MAX` used to overflow (a panic in a debug
    /// build); the lattice wraps as a release build always did, and the sampler still matches the
    /// plain formula (corr-procgen).
    #[test]
    fn noise_at_the_i32_lattice_edge_wraps_without_a_panic() {
        let edge = f64::from(i32::MAX) + 0.5;
        for (x, z) in [(edge, 0.25), (0.25, edge), (edge, edge), (-edge - 1.0, edge)] {
            let fbm = Fbm::new(1, 0.5, 2.0, 7);
            let v = fbm.eval(x, z);
            assert!((0.0..=1.0).contains(&v), "{v} at ({x}, {z})");
            assert_eq!(
                v.to_bits(),
                plain_fbm(x, z, 1, 0.5, 2.0, 7).to_bits(),
                "({x}, {z})"
            );
            assert_eq!(v.to_bits(), fbm.sampler().eval(x, z).to_bits(), "({x}, {z})");
        }
    }

    fn plain_fbm(x: f64, z: f64, octaves: i64, gain: f64, lacunarity: f64, seed: u32) -> f64 {
        let (mut sum, mut norm, mut amp, mut freq) = (0.0, 0.0, 1.0, 1.0);
        for o in 0..octaves.max(1) {
            sum += amp * plain_value_noise(x * freq, z * freq, mix(seed, o as u32));
            norm += amp;
            amp *= gain;
            freq *= lacunarity;
        }
        if norm > 0.0 { sum / norm } else { 0.0 }
    }

    /// One-shot evaluation and the cached sampler give the plain formula's bits, whatever order the
    /// points come in: walks that stay in a cell, jumps across cells, negative and large coordinates.
    #[test]
    fn the_cached_sampler_matches_the_plain_formula_bit_for_bit() {
        let mut bits = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            bits ^= bits << 13;
            bits ^= bits >> 7;
            bits ^= bits << 17;
            bits
        };
        let mut points: Vec<(f64, f64)> = Vec::new();
        for i in 0..2000 {
            let v = f64::from(i) * 0.013;
            points.push((v, -v * 0.5));
        }
        for i in 0..2000 {
            points.push((f64::from(i) - 1000.0, f64::from(-i)));
        }
        for _ in 0..4000 {
            let (a, b) = (next(), next());
            points.push(((a >> 20) as f64 * 1e-3 - 4e9, (b >> 20) as f64 * 1e-3 - 4e9));
        }
        for &(x, z) in &[
            (0.0, -0.0),
            (-0.0, 0.0),
            (1e12, -1e12),
            (5e-324, -5e-324),
            (2.5e9, 2.5e9),
        ] {
            points.push((x, z));
        }
        for (octaves, gain, lacunarity, seed) in
            [(1, 0.5, 2.0, 7), (3, 0.5, 2.0, 0xABCD_1234), (5, 0.8, 1.7, 0)]
        {
            let noise = Fbm::new(octaves, gain, lacunarity, seed);
            let mut sampler = noise.sampler();
            for &(x, z) in &points {
                let want = plain_fbm(x, z, octaves, gain, lacunarity, seed).to_bits();
                assert_eq!(noise.eval(x, z).to_bits(), want, "eval ({x:e}, {z:e})");
                assert_eq!(sampler.eval(x, z).to_bits(), want, "sampler ({x:e}, {z:e})");
            }
        }
    }
}
