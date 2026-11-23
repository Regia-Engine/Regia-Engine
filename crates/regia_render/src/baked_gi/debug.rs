// SPDX-License-Identifier: MPL-2.0
//! Debugging views of the baked GI lookup (ADR-1109): what each probe holds, whether it is
//! usable, and how the corners of one lookup were weighted. Everything is a CPU query on the
//! installed volume, the same code the lookup mirror ([`super::sample_static_gi`]) runs, so a
//! view cannot drift from what the lookup does. The agent methods `render.get_gi_probes`,
//! `render.explain_gi` and `render.set_gi_debug_overlay` (`book/agents/gi-debug-views.md`) are
//! thin wrappers over this module.

use regia_asset::baked_gi::{BakedGiVolume, GI_COMPONENTS, GiComponent, ProbeFlags, TerrainLayer};

use super::{BakedGiSettings, GiEval, Part, StaticGiSample, SunRelight, lookup, sample_of};

/// Which layer of the volume a probe or a lookup part belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GiLayer {
    /// The terrain-following columns.
    Terrain,
    /// A detail brick.
    Brick,
}

impl GiLayer {
    /// The name the agent protocol uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Terrain => "terrain",
            Self::Brick => "brick",
        }
    }
}

/// One corner probe a lookup visited ([`trace_static_gi`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CornerTrace {
    /// Probe index in the volume.
    pub index: usize,
    /// World position of the probe (nominal position plus relocation when valid, else nominal).
    pub position: [f32; 3],
    /// The bake kept this probe. A rejected corner adds no light and no weight.
    pub valid: bool,
    /// A valid corner the wall mask dropped: a baked wall (ADR-366) or the leak guard (ADR-861)
    /// puts it on the other side from the receiver. It adds no light and no weight.
    pub walled: bool,
    /// The trilinear weight of the corner for the lookup point, before the visibility terms.
    pub trilinear: f32,
    /// How well the probe sees the receiver (0..=1; 0 for a rejected corner): its Chebyshev
    /// visibility after the leak control (ADR-1105), cubed as the corner weight takes it (the
    /// visible share the ADR-506 layer blend sums).
    pub visibility: f32,
    /// The final weight (facing, Chebyshev visibility and trilinear), 0 for a rejected or walled
    /// corner.
    pub weight: f32,
}

/// The eight corners of a lookup part, in corner order (empty for a part that found no cell).
fn corner_traces(part: &Part) -> Vec<CornerTrace> {
    if !part.cell {
        return Vec::new();
    }
    let c = &part.corners;
    (0..8)
        .map(|k| {
            let valid = c.weight[k].is_some();
            let kept = part.kept & (1 << k) != 0;
            CornerTrace {
                index: c.index[k],
                position: c.position[k],
                valid,
                walled: valid && !kept,
                trilinear: c.tri[k],
                visibility: if valid {
                    c.vis[k] / c.tri[k].max(0.001)
                } else {
                    0.0
                },
                weight: if kept { c.weight[k].unwrap_or(0.0) } else { 0.0 },
            }
        })
        .collect()
}

/// How one baked lookup was put together ([`trace_static_gi`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GiLookupTrace {
    /// The lookup result, identical to [`super::sample_static_gi_relit`].
    pub sample: StaticGiSample,
    /// The brick chosen for the point, as an index into the volume's bricks, when one holds it.
    pub brick: Option<usize>,
    /// The chosen brick is an interior brick: it alone lights the point.
    pub interior: bool,
    /// The coverage the brick layer took (0..=1). With `terrain_coverage` it sums to the coverage
    /// (an interior brick covers fully whatever its corners).
    pub brick_coverage: f32,
    /// The coverage the terrain layer took (0..=1).
    pub terrain_coverage: f32,
    /// How much of the brick cell sees the receiver (0..=1). The brick's light is weighted by
    /// its share times this visibility (plus a small floor) against the terrain's.
    pub brick_visibility: f32,
    /// How much of the terrain cell sees the receiver (0..=1).
    pub terrain_visibility: f32,
    /// Corners the brick layer visited (eight, or none without a brick).
    pub brick_corners: Vec<CornerTrace>,
    /// Sum of the brick corners' weights.
    pub brick_weight_sum: f32,
    /// Corners the terrain layer visited (eight; empty when it was not consulted or the point is
    /// outside its columns).
    pub terrain_corners: Vec<CornerTrace>,
    /// Sum of the terrain corners' weights.
    pub terrain_weight_sum: f32,
}

impl GiLookupTrace {
    /// The corners' share of their layer's light (weight over the layer's weight sum), in the
    /// order of `corners`. Zeros when the layer's weights sum to nothing.
    #[must_use]
    pub fn normalized(corners: &[CornerTrace], sum: f32) -> Vec<f32> {
        corners
            .iter()
            .map(|c| if sum > 0.0 { c.weight / sum } else { 0.0 })
            .collect()
    }
}

/// Runs [`super::sample_static_gi_relit`] and keeps how it chose its corners: which probes each
/// layer visited, which the bake had rejected, which a wall dropped, and the weight each took.
#[must_use]
pub fn trace_static_gi(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    scales: &[[f32; 3]; GI_COMPONENTS],
    relight: &SunRelight,
    p: [f32; 3],
    n: [f32; 3],
    v: [f32; 3],
) -> GiLookupTrace {
    let found = lookup(volume, settings, scales, relight.ground_factor(), p, n, v);
    let (terrain_corners, terrain_weight_sum, terrain_visibility) =
        found.terrain.as_ref().map_or((Vec::new(), 0.0, 0.0), |t| {
            (corner_traces(t), t.wsum, t.visibility())
        });
    GiLookupTrace {
        sample: sample_of(found.sh, settings, n),
        brick: found.brick.brick,
        interior: found.brick.interior,
        brick_coverage: found.taken[0],
        terrain_coverage: found.taken[1],
        brick_visibility: found.brick.visibility(),
        terrain_visibility,
        brick_weight_sum: found.brick.wsum,
        brick_corners: corner_traces(&found.brick),
        terrain_corners,
        terrain_weight_sum,
    }
}

/// Which probes [`probe_debug_points`] lists.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeQuery {
    /// Centre of the search sphere, world metres.
    pub center: [f32; 3],
    /// Radius of the search sphere; infinite lists the whole volume.
    pub radius: f32,
    /// Most probes returned, nearest first.
    pub max: usize,
    /// The surface normal the irradiance is evaluated for (unit).
    pub normal: [f32; 3],
    /// Which probes count as matches.
    pub filter: ProbeFilter,
}

/// Which probes a [`ProbeQuery`] matches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProbeFilter {
    /// Every probe.
    #[default]
    All,
    /// Probes the bake kept.
    Valid,
    /// Probes the bake rejected (under the terrain or inside geometry).
    Invalid,
    /// Probes the bake moved out of geometry.
    Relocated,
}

impl ProbeFilter {
    /// The filter named `name` (`"all"`, `"valid"`, `"invalid"` or `"relocated"`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "all" => Some(Self::All),
            "valid" => Some(Self::Valid),
            "invalid" => Some(Self::Invalid),
            "relocated" => Some(Self::Relocated),
            _ => None,
        }
    }

    fn keeps(self, flags: u16) -> bool {
        match self {
            Self::All => true,
            Self::Valid => flags & ProbeFlags::VALID != 0,
            Self::Invalid => flags & ProbeFlags::VALID == 0,
            Self::Relocated => flags & ProbeFlags::RELOCATED != 0,
        }
    }
}

/// Probe counts of a volume by validity flag ([`volume_summary`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VolumeSummary {
    /// All probes.
    pub total: usize,
    /// Probes the bake kept.
    pub valid: usize,
    /// Probes the bake rejected.
    pub invalid: usize,
    /// Probes the bake moved out of geometry.
    pub relocated: usize,
    /// Probes rejected under the terrain.
    pub under_terrain: usize,
    /// Probes rejected inside geometry.
    pub inside_geometry: usize,
    /// Terrain-layer probes.
    pub terrain_probes: usize,
    /// Detail bricks.
    pub bricks: usize,
    /// Interior bricks.
    pub interior_bricks: usize,
}

/// Counts the volume's probes by validity flag. Reads the meta records only.
#[must_use]
pub fn volume_summary(volume: &BakedGiVolume) -> VolumeSummary {
    let mut out = VolumeSummary {
        total: volume.probe_count(),
        terrain_probes: volume.terrain.as_ref().map_or(0, TerrainLayer::probe_count),
        bricks: volume.bricks.len(),
        interior_bricks: volume.interior_brick_count(),
        ..VolumeSummary::default()
    };
    for i in 0..out.total {
        let f = volume.probe_meta(i).flags;
        if f & ProbeFlags::VALID != 0 {
            out.valid += 1;
        } else {
            out.invalid += 1;
        }
        out.relocated += usize::from(f & ProbeFlags::RELOCATED != 0);
        out.under_terrain += usize::from(f & ProbeFlags::UNDER_TERRAIN != 0);
        out.inside_geometry += usize::from(f & ProbeFlags::INSIDE_GEOMETRY != 0);
    }
    out
}

/// The world box around the volume's nominal probe positions (`None` for an empty volume).
#[must_use]
pub fn volume_bounds(volume: &BakedGiVolume) -> Option<([f32; 3], [f32; 3])> {
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    let mut grow = |p: [f32; 3]| {
        for ((l, h), x) in lo.iter_mut().zip(hi.iter_mut()).zip(p) {
            *l = l.min(x);
            *h = h.max(x);
        }
    };
    if let Some(t) = &volume.terrain
        && t.counts[0] > 0
        && t.counts[1] > 0
        && !t.levels.is_empty()
    {
        let far = [
            t.origin[0] + (t.counts[0] - 1) as f32 * t.spacing[0],
            t.origin[1] + (t.counts[1] - 1) as f32 * t.spacing[1],
        ];
        let (g_lo, g_hi) = t
            .ground
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(l, h), g| {
                (l.min(*g), h.max(*g))
            });
        let top = t.levels.iter().copied().fold(0.0f32, f32::max);
        let bottom = t.levels.iter().copied().fold(f32::INFINITY, f32::min);
        if g_lo.is_finite() {
            grow([t.origin[0], g_lo + bottom, t.origin[1]]);
            grow([far[0], g_hi + top, far[1]]);
        }
    }
    for b in &volume.bricks {
        grow(b.origin);
        grow(std::array::from_fn(|a| {
            b.origin[a] + b.counts[a].saturating_sub(1) as f32 * b.spacing[a]
        }));
    }
    lo[0].is_finite().then_some((lo, hi))
}

/// One probe as the debug views show it.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeDebugPoint {
    /// Probe index in the volume.
    pub index: usize,
    /// The layer the probe belongs to.
    pub layer: GiLayer,
    /// For a brick probe, the brick's index.
    pub brick: Option<usize>,
    /// The probe sits in an interior brick.
    pub interior: bool,
    /// Where the bake placed the probe (nominal position plus relocation).
    pub position: [f32; 3],
    /// The raw `ProbeFlags` bits.
    pub flags: u16,
    /// The bake kept the probe (`ProbeFlags::VALID`).
    pub valid: bool,
    /// Moved by the bake out of geometry (`ProbeFlags::RELOCATED`).
    pub relocated: bool,
    /// Rejected under the terrain (`ProbeFlags::UNDER_TERRAIN`).
    pub under_terrain: bool,
    /// Rejected inside geometry (`ProbeFlags::INSIDE_GEOMETRY`).
    pub inside_geometry: bool,
    /// Irradiance / π of all components for `ProbeQuery::normal`, scaled by the frame's
    /// component factors; zero for an invalid probe.
    pub irradiance_over_pi: [f32; 3],
    /// Mean of the probe's distance moments over its texels, metres: how far it sees on average.
    pub mean_distance: f32,
}

/// Lists the probes of `volume` inside the query sphere, nearest to its centre first, with
/// their validity flags, position and the light they hold. Returns the probes (at most
/// `query.max`) and how many probes matched in all.
#[must_use]
pub fn probe_debug_points(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    scales: &[[f32; 3]; GI_COMPONENTS],
    query: &ProbeQuery,
) -> (Vec<ProbeDebugPoint>, usize) {
    let layout = volume.layout();
    let terrain_count = layout.terrain.as_ref().map_or(0, TerrainLayer::probe_count);
    let mut hits: Vec<(f32, usize, [f32; 3])> = Vec::new();
    for i in 0..volume.probe_count() {
        let Some(nominal) = layout.nominal_position(i) else {
            continue;
        };
        let o = volume.probe_meta(i).offset;
        let pos = [nominal[0] + o[0], nominal[1] + o[1], nominal[2] + o[2]];
        if !query.filter.keeps(volume.probe_meta(i).flags) {
            continue;
        }
        let d = (0..3)
            .map(|a| (pos[a] - query.center[a]).powi(2))
            .sum::<f32>()
            .sqrt();
        if d <= query.radius {
            hits.push((d, i, pos));
        }
    }
    let matched = hits.len();
    hits.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    hits.truncate(query.max);
    let points =
        hits.into_iter()
            .map(|(_, i, position)| {
                let meta = volume.probe_meta(i);
                let valid = meta.flags & ProbeFlags::VALID != 0;
                let brick = if i < terrain_count {
                    None
                } else {
                    volume.bricks.iter().position(|b| {
                        i >= b.first_probe as usize && i < b.first_probe as usize + b.probe_count()
                    })
                };
                let moments = volume.probe_moments(i);
                let mean_distance = if moments.is_empty() {
                    0.0
                } else {
                    moments.iter().map(|m| m[0]).sum::<f32>() / moments.len() as f32
                };
                ProbeDebugPoint {
                    index: i,
                    layer: if brick.is_some() {
                        GiLayer::Brick
                    } else {
                        GiLayer::Terrain
                    },
                    brick,
                    interior: brick.is_some_and(|b| volume.bricks[b].is_interior()),
                    position,
                    flags: meta.flags,
                    valid,
                    relocated: meta.flags & ProbeFlags::RELOCATED != 0,
                    under_terrain: meta.flags & ProbeFlags::UNDER_TERRAIN != 0,
                    inside_geometry: meta.flags & ProbeFlags::INSIDE_GEOMETRY != 0,
                    irradiance_over_pi: if valid {
                        probe_irradiance(volume, settings, scales, i, query.normal)
                    } else {
                        [0.0; 3]
                    },
                    mean_distance,
                }
            })
            .collect();
    (points, matched)
}

/// Irradiance / π of probe `i` for normal `n`: the components summed after their frame factors
/// and the intensity, then clamped at 0, as the lookup does for its blended SH.
fn probe_irradiance(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    scales: &[[f32; 3]; GI_COMPONENTS],
    i: usize,
    n: [f32; 3],
) -> [f32; 3] {
    let mut total = [[0.0f32; 4]; 3];
    for comp in GiComponent::ALL {
        let k = scales[comp as usize];
        let sh = volume.probe_sh(i, comp);
        for ((t, s), kc) in total.iter_mut().zip(&sh).zip(k) {
            for (a, b) in t.iter_mut().zip(s) {
                *a += b * kc * settings.intensity;
            }
        }
    }
    GiEval::of(settings).irradiance(&total, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, DetailBrick, ProbeData, ProbeMeta, SH_Y00, ShRgb, VolumeLayout,
    };

    /// Radiance SH of a uniform radiance `l` (no L1).
    fn uniform(l: [f32; 3]) -> ShRgb {
        let k = 4.0 * std::f32::consts::PI * SH_Y00;
        [
            [l[0] * k, 0.0, 0.0, 0.0],
            [l[1] * k, 0.0, 0.0, 0.0],
            [l[2] * k, 0.0, 0.0, 0.0],
        ]
    }

    fn probe(sky: [f32; 3], flags: u16) -> ProbeData {
        ProbeData {
            sh: [
                uniform([0.0; 3]),
                uniform(sky),
                uniform([0.0; 3]),
                uniform([0.0; 3]),
            ],
            moments: vec![[16.0, 256.0]; 16],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags,
            },
        }
    }

    /// A 4×4 column terrain with three levels over flat ground at 10 m, and optionally a brick.
    fn volume(flags: impl Fn(usize) -> u16, sky: impl Fn(usize) -> [f32; 3], brick: bool) -> BakedGiVolume {
        let terrain = TerrainLayer {
            origin: [0.0, 0.0],
            spacing: [4.0, 4.0],
            counts: [4, 4],
            levels: vec![1.0, 2.0, 4.0],
            ground: vec![10.0; 16],
        };
        let mut layout = VolumeLayout {
            terrain: Some(terrain),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        if brick {
            layout.bricks.push(DetailBrick {
                origin: [4.0, 10.5, 4.0],
                spacing: [1.0, 1.0, 1.0],
                counts: [3, 3, 3],
                first_probe: 0,
                flags: 0,
            });
            layout.assign_brick_offsets();
        }
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|i| probe(sky(i), flags(i)))
            .collect();
        BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume")
    }

    fn sky_scales() -> [[f32; 3]; GI_COMPONENTS] {
        [[0.0; 3], [1.0; 3], [0.0; 3], [0.0; 3]]
    }

    const UP: [f32; 3] = [0.0, 1.0, 0.0];

    #[test]
    fn the_trace_reproduces_the_lookup_and_its_weights_sum() {
        let vol = volume(|_| ProbeFlags::VALID, |_| [2.0, 1.0, 0.5], false);
        let s = BakedGiSettings::default();
        let p = [6.0, 11.5, 6.0];
        let trace = trace_static_gi(&vol, &s, &sky_scales(), &SunRelight::NONE, p, UP, UP);
        assert_eq!(
            trace.sample,
            super::super::sample_static_gi(&vol, &s, &sky_scales(), p, UP, UP)
        );
        assert_eq!(trace.brick, None);
        assert!(trace.brick_corners.is_empty());
        assert_eq!(trace.terrain_corners.len(), 8);
        assert!(trace.terrain_corners.iter().all(|c| c.valid && c.weight > 0.0));
        let tri: f32 = trace.terrain_corners.iter().map(|c| c.trilinear).sum();
        assert!((tri - 1.0).abs() < 1e-4, "trilinear weights sum to 1: {tri}");
        let shares = GiLookupTrace::normalized(&trace.terrain_corners, trace.terrain_weight_sum);
        assert!((shares.iter().sum::<f32>() - 1.0).abs() < 1e-4, "{shares:?}");
        assert!((trace.terrain_coverage - trace.sample.coverage).abs() < 1e-5);
        assert_eq!(trace.brick_coverage, 0.0);
    }

    #[test]
    fn a_rejected_corner_is_listed_with_no_weight() {
        // Every probe of column (1, 1) is rejected under the terrain.
        let vol = volume(
            |i| {
                if (i / 3) == 5 {
                    ProbeFlags::UNDER_TERRAIN
                } else {
                    ProbeFlags::VALID
                }
            },
            |_| [1.0; 3],
            false,
        );
        let s = BakedGiSettings::default();
        let trace = trace_static_gi(
            &vol,
            &s,
            &sky_scales(),
            &SunRelight::NONE,
            [5.0, 11.5, 5.0],
            UP,
            UP,
        );
        let rejected: Vec<_> = trace.terrain_corners.iter().filter(|c| !c.valid).collect();
        assert!(!rejected.is_empty(), "{trace:?}");
        assert!(rejected.iter().all(|c| c.weight == 0.0 && (c.index / 3) == 5));
        assert_eq!(trace.terrain_corners.len(), 8, "invalid corners are still listed");
        let kept: f32 = trace.terrain_corners.iter().map(|c| c.weight).sum();
        assert!((kept - trace.terrain_weight_sum).abs() < 1e-5);
    }

    #[test]
    fn a_brick_point_reports_its_brick_and_blends_with_the_terrain() {
        let vol = volume(|_| ProbeFlags::VALID, |_| [1.0; 3], true);
        let s = BakedGiSettings::default();
        let inside = trace_static_gi(
            &vol,
            &s,
            &sky_scales(),
            &SunRelight::NONE,
            [5.0, 11.5, 5.0],
            UP,
            UP,
        );
        assert_eq!(inside.brick, Some(0));
        assert!(!inside.interior);
        assert!(!inside.brick_corners.is_empty());
        assert!(inside.brick_coverage > 0.0);
        assert!((inside.brick_coverage + inside.terrain_coverage - inside.sample.coverage).abs() < 1e-4);
        let outside = trace_static_gi(
            &vol,
            &s,
            &sky_scales(),
            &SunRelight::NONE,
            [11.0, 11.5, 11.0],
            UP,
            UP,
        );
        assert_eq!(outside.brick, None);
        assert_eq!(outside.brick_coverage, 0.0);
    }

    #[test]
    fn a_walled_corner_is_listed_with_no_weight() {
        // Every probe of column x = 1 has a wall halfway toward column x = 2 (x = 6 m): a
        // receiver at x = 5 takes only the column-1 corners (ADR-366).
        let vol = volume(
            |i| {
                if (i / 3) % 4 == 1 {
                    ProbeFlags::VALID | ProbeFlags::wall(0, 0.5)
                } else {
                    ProbeFlags::VALID
                }
            },
            |_| [1.0; 3],
            false,
        );
        let s = BakedGiSettings::default();
        let p = [5.0, 11.5, 5.0];
        let trace = trace_static_gi(&vol, &s, &sky_scales(), &SunRelight::NONE, p, UP, UP);
        assert_eq!(
            trace.sample,
            super::super::sample_static_gi(&vol, &s, &sky_scales(), p, UP, UP)
        );
        assert_eq!(trace.terrain_corners.len(), 8);
        assert!(trace.terrain_corners.iter().all(|c| c.valid), "{trace:?}");
        let walled: Vec<_> = trace.terrain_corners.iter().filter(|c| c.walled).collect();
        assert_eq!(walled.len(), 4, "{trace:?}");
        assert!(walled.iter().all(|c| c.weight == 0.0 && (c.index / 3) % 4 == 2));
        assert!(
            trace
                .terrain_corners
                .iter()
                .filter(|c| !c.walled)
                .all(|c| c.weight > 0.0 && (c.index / 3) % 4 == 1)
        );
        let kept: f32 = trace.terrain_corners.iter().map(|c| c.weight).sum();
        assert!((kept - trace.terrain_weight_sum).abs() < 1e-5);
        assert!(trace.terrain_visibility > 0.99, "open moments see every receiver");
    }

    #[test]
    fn the_trace_follows_the_sun_relight() {
        // Sun bounce lit from below (L1 pointing down), which the relight scales with the sun.
        let layout = volume(|_| ProbeFlags::VALID, |_| [1.0; 3], true).layout();
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|_| {
                let mut d = probe([0.0; 3], ProbeFlags::VALID);
                d.sh[0] = [[1.0, 0.0, -0.8, 0.0]; 3];
                d
            })
            .collect();
        let vol = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume");
        let s = BakedGiSettings::default();
        let scales = [[1.0; 3]; GI_COMPONENTS];
        let low_sun = SunRelight {
            baked_up: 0.8,
            frame_up: 0.2,
        };
        for p in [[5.0, 11.5, 5.0], [9.0, 11.5, 9.0]] {
            let trace = trace_static_gi(&vol, &s, &scales, &low_sun, p, UP, UP);
            let relit = super::super::sample_static_gi_relit(&vol, &s, &scales, &low_sun, p, UP, UP);
            assert_eq!(trace.sample, relit);
            let as_baked = super::super::sample_static_gi(&vol, &s, &scales, p, UP, UP);
            assert!(
                relit.irradiance_over_pi[0] < as_baked.irradiance_over_pi[0],
                "a lower sun dims the bounce from below: {relit:?} vs {as_baked:?}"
            );
        }
    }

    #[test]
    fn bounds_cover_the_terrain_and_the_bricks() {
        let vol = volume(|_| ProbeFlags::VALID, |_| [1.0; 3], true);
        let (lo, hi) = volume_bounds(&vol).expect("bounds");
        // Columns span 0..12 on X and Z; ground 10 m, levels 1..4 m.
        assert_eq!(lo, [0.0, 10.5, 0.0]);
        assert_eq!(hi, [12.0, 14.0, 12.0]);
        assert_eq!(ProbeFilter::parse("relocated"), Some(ProbeFilter::Relocated));
        assert_eq!(ProbeFilter::parse("nope"), None);
    }

    #[test]
    fn probe_points_filter_sort_and_flag() {
        let vol = volume(
            |i| match i {
                0 => ProbeFlags::UNDER_TERRAIN,
                1 => ProbeFlags::VALID | ProbeFlags::RELOCATED,
                2 => ProbeFlags::INSIDE_GEOMETRY,
                _ => ProbeFlags::VALID,
            },
            |_| [2.0; 3],
            true,
        );
        let s = BakedGiSettings::default();
        let all = ProbeQuery {
            center: [0.0, 10.0, 0.0],
            radius: f32::INFINITY,
            max: usize::MAX,
            normal: UP,
            filter: ProbeFilter::All,
        };
        let (points, matched) = probe_debug_points(&vol, &s, &sky_scales(), &all);
        assert_eq!(matched, vol.probe_count());
        assert_eq!(points.len(), vol.probe_count());
        let by = |i: usize| points.iter().find(|p| p.index == i).expect("probe listed");
        assert!(!by(0).valid && by(0).under_terrain && by(0).irradiance_over_pi == [0.0; 3]);
        assert!(by(1).valid && by(1).relocated && by(1).irradiance_over_pi[0] > 1.9);
        assert!(!by(2).valid && by(2).inside_geometry);
        assert_eq!(by(0).layer, GiLayer::Terrain);
        let first_brick = vol.bricks[0].first_probe as usize;
        assert_eq!(by(first_brick).layer, GiLayer::Brick);
        assert_eq!(by(first_brick).brick, Some(0));
        assert!((by(4).mean_distance - 16.0).abs() < 0.01);
        // A small sphere keeps only nearby probes, nearest first, and `max` truncates.
        let near = ProbeQuery {
            center: [0.0, 11.0, 0.0],
            radius: 1.5,
            max: 1,
            normal: UP,
            filter: ProbeFilter::All,
        };
        let (few, matched) = probe_debug_points(&vol, &s, &sky_scales(), &near);
        assert!(matched >= 1 && matched < vol.probe_count());
        assert_eq!(few.len(), 1);
        let invalid = ProbeQuery {
            filter: ProbeFilter::Invalid,
            ..all
        };
        let (bad, matched) = probe_debug_points(&vol, &s, &sky_scales(), &invalid);
        assert_eq!(matched, 2);
        assert!(bad.iter().all(|p| !p.valid));
        let relocated = ProbeQuery {
            filter: ProbeFilter::Relocated,
            ..all
        };
        let (moved, _) = probe_debug_points(&vol, &s, &sky_scales(), &relocated);
        assert_eq!(moved.iter().map(|p| p.index).collect::<Vec<_>>(), vec![1]);
        let sum = volume_summary(&vol);
        assert_eq!(sum.total, vol.probe_count());
        assert_eq!(
            (sum.invalid, sum.relocated, sum.under_terrain, sum.inside_geometry),
            (2, 1, 1, 1)
        );
        assert_eq!(sum.valid + sum.invalid, sum.total);
        assert_eq!((sum.bricks, sum.interior_bricks, sum.terrain_probes), (1, 0, 48));
        // Intensity scales the light, a zero factor removes a component.
        let dark = [[0.0; 3]; GI_COMPONENTS];
        let (none, _) = probe_debug_points(&vol, &s, &dark, &all);
        assert!(none.iter().all(|p| p.irradiance_over_pi == [0.0; 3]));
    }
}
