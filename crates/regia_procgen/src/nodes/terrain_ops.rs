// SPDX-License-Identifier: MPL-2.0
//! `terrain.terrace`, `terrain.crater`, `terrain.flatten_pad`, `terrain.domain_warp` and
//! `terrain.noise`: the heightfield operators of [`regia_terrain::ops`] as bake-only terrain edits
//! (DO27 absorb row C-18, ADR-8465).
//!
//! Each node takes a `Ground` item (from `in.ground`), runs its operator over the captured heights
//! and passes the changed ground on, so operators chain (`in.ground → terrain.domain_warp →
//! terrain.terrace → terrain.flatten_pad`). Each one also records a terrain edit (spec 64 § 7.3)
//! holding the samples it changed and their heights before it, in node order, so a chain applies in
//! order and restores in reverse bit for bit. Like `terrain.flatten_road` they need a `Baked`
//! generator with terrain edits allowed. Noise is evaluated at world positions computed from the
//! full-terrain sample index, so tiled runs agree where cells meet; in a tiled run a node records only
//! the samples its cell owns (spec 66 § 2).

use std::sync::Arc;

use regia_terrain::ops::{
    CraterParams, GridFrame, NoiseKind, NoiseParams, OpError, PadParams, TerraceParams, WarpParams,
};

use crate::data::{Bundle, DataItem, GroundData, Payload};
use crate::exec::{NodeCtx, TerrainEditGate, TerrainEditRecord};
use crate::nodes::roads::GROUND;
use crate::nodes::{
    BuiltinNode, NodeError, POINTS, PinSpec, SettingType as T, Settings, input, num, pin_out, pin_req, set,
};

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "terrain.terrace",
        "terrain",
        "Bake only: cuts the ground into flat treads `step_height_m` apart with jittered risers and slumped patches",
        &[
            num("step_height_m", T::Float, "6.0", 0.01, 1.0e4, "Vertical spacing of the treads, meters"),
            num("base_m", T::Float, "0.0", -1.0e5, 1.0e5, "Treads sit at base_m + k * step_height_m"),
            num("width_jitter", T::Float, "0.3", 0.0, 1.0, "How far noise shifts each riser, as a fraction of a step"),
            num("jitter_wavelength_m", T::Float, "150.0", 0.01, 1.0e6, "Wavelength of the jitter noise, meters"),
            num("edge_sharpness", T::Float, "0.8", 0.0, 1.0, "0 leaves the ground, 1 gives near-vertical risers"),
            num("collapse", T::Float, "0.0", 0.0, 1.0, "Share of the area slumped back to the original slope"),
            num("collapse_wavelength_m", T::Float, "120.0", 0.01, 1.0e6, "Wavelength of the collapse patches, meters"),
            num("seed", T::Int, "0", 0.0, 4_294_967_295.0, "Added to the node seed"),
        ],
        pins_ground,
        eval_terrace,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "terrain.crater",
        "terrain",
        "Bake only: an impact crater (bowl, rim, ejecta blanket, optional central peak) at each input point",
        &[
            num("radius_m", T::Float, "50.0", 0.01, 1.0e5, "Rim-crest radius, meters (times the point's scale x)"),
            num("depth_m", T::Float, "8.0", -1.0e4, 1.0e4, "Floor depth below the ground at the center, meters"),
            num("rim_height_m", T::Float, "2.0", -1.0e4, 1.0e4, "Rim crest height above the ground, meters"),
            num("ejecta_falloff", T::Float, "2.5", 0.0, 100.0, "Decay exponent of the ejecta blanket"),
            num("ejecta_extent", T::Float, "2.5", 1.0001, 100.0, "The blanket reaches this many radii"),
            num("central_peak_m", T::Float, "0.0", -1.0e4, 1.0e4, "Central peak height above the floor, meters"),
            num("central_peak_radius_frac", T::Float, "0.12", 0.001, 10.0, "Peak width as a fraction of the radius"),
        ],
        pins_points_ground,
        eval_crater,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "terrain.flatten_pad",
        "terrain",
        "Bake only: levels a circular or rectangular pad at each input point and feathers it into the ground",
        &[
            set("shape", T::Enum(&["circle", "rect"]), "circle", "The flat footprint"),
            num("radius_m", T::Float, "20.0", 0.01, 1.0e5, "Circle radius, meters (times the point's scale x)"),
            num("length_m", T::Float, "40.0", 0.01, 1.0e5, "Rectangle length along the point's heading, meters"),
            num("width_m", T::Float, "20.0", 0.01, 1.0e5, "Rectangle width across, meters"),
            num("rotation_deg", T::Float, "0.0", -360.0, 360.0, "Added to the point's yaw, degrees from +X toward +Z"),
            set("height", T::Enum(&["mean", "point", "fixed"]), "mean", "Pad height: mean ground inside, the point's y, or height_m"),
            num("height_m", T::Float, "0.0", -1.0e5, 1.0e5, "The pad height when `height` is fixed, meters"),
            num("feather_m", T::Float, "20.0", 0.0, 1.0e5, "Width of the blend outside the pad, meters"),
        ],
        pins_points_ground,
        eval_flatten_pad,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "terrain.domain_warp",
        "terrain",
        "Bake only: resamples the ground at noise-displaced positions (heights move, keep their range)",
        &[
            num("strength_m", T::Float, "40.0", 0.0, 1.0e5, "Largest horizontal displacement, meters"),
            num("wavelength_m", T::Float, "300.0", 0.01, 1.0e6, "Wavelength of the displacement, meters"),
            num("octaves", T::Int, "3", 1.0, 16.0, "Octaves of the displacement noise"),
            num("seed", T::Int, "0", 0.0, 4_294_967_295.0, "Added to the node seed"),
        ],
        pins_ground,
        eval_domain_warp,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "terrain.noise",
        "terrain",
        "Bake only: adds fBm or ridged noise with an optional directional grain to the ground",
        &[
            set("kind", T::Enum(&["fbm", "ridged"]), "fbm", "fBm (±amplitude) or ridged (0..amplitude)"),
            num("amplitude_m", T::Float, "20.0", -1.0e4, 1.0e4, "Noise amplitude, meters"),
            num("wavelength_m", T::Float, "400.0", 0.01, 1.0e6, "Wavelength of the first octave, meters"),
            num("octaves", T::Int, "5", 1.0, 16.0, "Octaves"),
            num("lacunarity", T::Float, "2.0", 0.01, 16.0, "Frequency multiplier per octave"),
            num("gain", T::Float, "0.5", 0.0, 4.0, "Amplitude multiplier per octave"),
            num("grain_angle_deg", T::Float, "0.0", -360.0, 360.0, "Direction features run along"),
            num("anisotropy", T::Float, "1.0", 1.0, 100.0, "Features this many times longer along the grain"),
            num("sharpness", T::Float, "2.0", 0.01, 16.0, "Ridged only: exponent on the ridge profile"),
            num("seed", T::Int, "0", 0.0, 4_294_967_295.0, "Added to the node seed"),
        ],
        pins_ground,
        eval_noise,
    )
    .uncached()
    .world(),
];

fn pins_ground(_: &Settings) -> PinSpec {
    PinSpec::new(vec![pin_req("ground", GROUND)], vec![pin_out("out", GROUND)])
}

fn pins_points_ground(_: &Settings) -> PinSpec {
    PinSpec::new(
        vec![pin_req("in", POINTS), pin_req("ground", GROUND)],
        vec![pin_out("out", GROUND)],
    )
}

/// The terrain-edit gate, as `terrain.flatten_road` applies it.
fn gate(ctx: &NodeCtx<'_>, id: &str) -> Result<(), NodeError> {
    match ctx.terrain_edits() {
        TerrainEditGate::Allowed => Ok(()),
        TerrainEditGate::NotBaked => Err(NodeError::new(
            "flatten_runtime_forbidden",
            format!("{id} runs only in Baked generators"),
        )),
        TerrainEditGate::NotAllowed => Err(NodeError::new(
            "terrain_edits_not_allowed",
            "the generator (allow_terrain_edits) or the project ([procgen] terrain_edits) does not allow terrain edits",
        )),
    }
}

fn op_error(e: &OpError) -> NodeError {
    NodeError::new(e.code(), e.to_string())
}

/// The ground item on input pin `k` and its grid frame (full-terrain placement, window start).
fn ground_of(
    inputs: &[Bundle],
    k: usize,
    id: &str,
) -> Result<(DataItem, Arc<GroundData>, GridFrame), NodeError> {
    let bundle = input(inputs, k);
    let Some((item, g)) = bundle.0.iter().find_map(|i| match &i.payload {
        Payload::Ground(g) => Some((i.clone(), g.clone())),
        _ => None,
    }) else {
        return Err(NodeError::new(
            "ground_missing",
            format!("{id} needs a Ground item"),
        ));
    };
    let span = |n: u32| f64::from(n.max(2) - 1);
    let width = f64::from(g.world_width);
    let half = width * 0.5;
    let frame = GridFrame::new(
        g.size[0],
        g.size[1],
        [width / span(g.map_size[0]), width / span(g.map_size[1])],
        [-half, -half],
    )
    .and_then(|f| f.with_first(g.origin))
    .map_err(|e| op_error(&e))?;
    Ok((item, g, frame))
}

/// Keeps the changes the run's cell owns, records them as one terrain edit and returns the changed
/// ground for the next node.
fn commit(
    ctx: &mut NodeCtx<'_>,
    item: &DataItem,
    g: &GroundData,
    frame: &GridFrame,
    mut heights: Vec<f32>,
) -> Bundle {
    let cols = frame.cols as usize;
    if let Some(tile) = ctx.tile() {
        for (i, h) in heights.iter_mut().enumerate() {
            let [x, z] = frame.position((i % cols) as u32, (i / cols) as u32);
            if !tile.owns(x, z) {
                *h = g.heights[i];
            }
        }
    }
    if let Some([c0, r0, c1, r1]) = regia_terrain::ops::changed_rect(frame, &g.heights, &heights) {
        let mut rec = TerrainEditRecord {
            rect: [
                g.origin[0] + c0,
                g.origin[1] + r0,
                g.origin[0] + c1,
                g.origin[1] + r1,
            ],
            ..TerrainEditRecord::default()
        };
        for r in r0 as usize..r1 as usize {
            let row = r * cols;
            rec.samples
                .extend_from_slice(&heights[row + c0 as usize..row + c1 as usize]);
            rec.original
                .extend_from_slice(&g.heights[row + c0 as usize..row + c1 as usize]);
        }
        ctx.count("terrain_op_samples", rec.samples.len() as u64);
        ctx.output().terrain_edits.push(rec);
    }
    let changed = GroundData { heights, ..g.clone() };
    Bundle::one(item.with_payload(Payload::Ground(Arc::new(changed))))
}

/// The node seed plus the `seed` setting.
fn seed_of(ctx: &NodeCtx<'_>, s: &Settings) -> u32 {
    ctx.node_seed.wrapping_add(s.i("seed") as u32)
}

fn eval_terrace(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.terrain_terrace").entered();
    gate(ctx, "terrain.terrace")?;
    let (item, g, frame) = ground_of(inputs, 0, "terrain.terrace")?;
    let mut p = TerraceParams::new(s.f("step_height_m"));
    p.seed = seed_of(ctx, s);
    p.base_m = s.f("base_m");
    p.width_jitter = s.f("width_jitter");
    p.jitter_wavelength_m = s.f("jitter_wavelength_m");
    p.edge_sharpness = s.f("edge_sharpness");
    p.collapse = s.f("collapse");
    p.collapse_wavelength_m = s.f("collapse_wavelength_m");
    let out = regia_terrain::ops::terrace(&frame, &g.heights, &p).map_err(|e| op_error(&e))?;
    Ok(vec![commit(ctx, &item, &g, &frame, out)])
}

fn eval_domain_warp(
    ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.terrain_domain_warp").entered();
    gate(ctx, "terrain.domain_warp")?;
    let (item, g, frame) = ground_of(inputs, 0, "terrain.domain_warp")?;
    let mut p = WarpParams::new(seed_of(ctx, s), s.f("strength_m"), s.f("wavelength_m"));
    p.octaves = s.i("octaves").clamp(1, 16) as u32;
    let out = regia_terrain::ops::domain_warp(&frame, &g.heights, &p).map_err(|e| op_error(&e))?;
    Ok(vec![commit(ctx, &item, &g, &frame, out)])
}

fn eval_noise(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.terrain_noise").entered();
    gate(ctx, "terrain.noise")?;
    let (item, g, frame) = ground_of(inputs, 0, "terrain.noise")?;
    let mut p = NoiseParams::new(
        seed_of(ctx, s),
        s.f("amplitude_m"),
        s.f("wavelength_m"),
        s.i("octaves").clamp(1, 16) as u32,
    );
    p.lacunarity = s.f("lacunarity");
    p.gain = s.f("gain");
    p.grain_angle_deg = s.f("grain_angle_deg");
    p.anisotropy = s.f("anisotropy");
    let kind = if s.text("kind") == "ridged" {
        NoiseKind::Ridged {
            sharpness: s.f("sharpness"),
        }
    } else {
        NoiseKind::Fbm
    };
    let out = regia_terrain::ops::add_noise(&frame, &g.heights, &p, kind).map_err(|e| op_error(&e))?;
    Ok(vec![commit(ctx, &item, &g, &frame, out)])
}

/// The points of input pin 0, in item order.
fn points_of(inputs: &[Bundle]) -> Vec<crate::data::Point> {
    input(inputs, 0)
        .0
        .iter()
        .filter_map(|i| match &i.payload {
            Payload::Points(ps) => Some((0..ps.len()).map(|i| ps.point(i)).collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Yaw of a point's rotation about +Y, degrees from +X toward +Z.
fn yaw_deg(q: [f32; 4]) -> f64 {
    // The local +X axis turned by the quaternion, projected onto XZ.
    let [x, y, z, w] = q.map(f64::from);
    let ax = 1.0 - 2.0 * (y * y + z * z);
    let az = 2.0 * (x * z - w * y);
    if ax == 0.0 && az == 0.0 {
        return 0.0;
    }
    f64::from(regia_core::math::detmath::det_atan2(az as f32, ax as f32)).to_degrees()
}

fn eval_crater(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.terrain_crater").entered();
    gate(ctx, "terrain.crater")?;
    let (item, g, frame) = ground_of(inputs, 1, "terrain.crater")?;
    let mut heights = g.heights.clone();
    for pt in points_of(inputs) {
        ctx.check_cancel()?;
        let scale = f64::from(pt.scale[0]).abs();
        let mut p = CraterParams::new(
            [f64::from(pt.position[0]), f64::from(pt.position[2])],
            s.f("radius_m") * if scale > 0.0 { scale } else { 1.0 },
            s.f("depth_m"),
            s.f("rim_height_m"),
        );
        p.ejecta_falloff = s.f("ejecta_falloff");
        p.ejecta_extent = s.f("ejecta_extent");
        p.central_peak_m = s.f("central_peak_m");
        p.central_peak_radius_frac = s.f("central_peak_radius_frac");
        heights = regia_terrain::ops::crater(&frame, &heights, &p).map_err(|e| op_error(&e))?;
    }
    Ok(vec![commit(ctx, &item, &g, &frame, heights)])
}

fn eval_flatten_pad(
    ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.terrain_flatten_pad").entered();
    gate(ctx, "terrain.flatten_pad")?;
    let (item, g, frame) = ground_of(inputs, 1, "terrain.flatten_pad")?;
    let mut heights = g.heights.clone();
    for pt in points_of(inputs) {
        ctx.check_cancel()?;
        let center = [f64::from(pt.position[0]), f64::from(pt.position[2])];
        let mut p = if s.text("shape") == "rect" {
            PadParams::rect(
                center,
                [s.f("length_m"), s.f("width_m")],
                yaw_deg(pt.rotation) + s.f("rotation_deg"),
            )
        } else {
            let scale = f64::from(pt.scale[0]).abs();
            PadParams::circle(center, s.f("radius_m") * if scale > 0.0 { scale } else { 1.0 })
        };
        p.feather_m = s.f("feather_m");
        p.height_m = match s.text("height") {
            "point" => Some(f64::from(pt.position[1])),
            "fixed" => Some(s.f("height_m")),
            _ => None,
        };
        match regia_terrain::ops::flatten_pad(&frame, &heights, &p) {
            Ok(out) => heights = out,
            Err(OpError::PadEmpty) => {
                ctx.warn(
                    "pad_empty",
                    format!(
                        "the pad at ({:.1}, {:.1}) covers no captured sample; skipped",
                        center[0], center[1]
                    ),
                );
            }
            Err(e) => return Err(op_error(&e)),
        }
    }
    Ok(vec![commit(ctx, &item, &g, &frame, heights)])
}
