// SPDX-License-Identifier: MPL-2.0
//! The tunnel features plugged into the scene's slots ([`crate::scene::Slots::today`]).
//!
//! Each filler uses the feature's own public API, placed from the scene's [`TunnelGeometry`]:
//!
//! | Slot | Feature | What the filler does |
//! |---|---|---|
//! | `exposure_zone` | `regia_render::exposure_zones` | weighs an `ExposureZone` box over the tube at the eye and hands the combined target to the renderer |
//! | `reverb_zone` | `regia_audio::reverb_zone` | spawns an `AudioReverbZone::tube` box over the tube |
//! | `probe_volume` | `regia_render::baked_gi` + `regia_pt` CPU bake | installs a small baked GI volume of the tube, baked in-process once per test binary |
//! | `interior_fog` | `FogVolume::interior` | adds fog bounded by the tube's `InteriorVolume` |
//! | `light_shafts` | the froxel fog's sun shadow | the cut's air is lit by the sun through the opening; the rule the shaft uses |
//! | `world_pick` | `regia_pick::world` | spawns the tube's meshes on the tunnel pick layer |

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{BakedGiVolume, TerrainLayer, VolumeLayout};
use regia_audio::{AudioReverbZone, ReverbZoneShape};
use regia_core::glam::{Affine3A, Mat4, Vec3};
use regia_ecs::prelude::World;
use regia_pick::MapLayer;
use regia_pt::cpu_probe::CpuProbeTracer;
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume};
use regia_pt::{PtMaterial, scenes};
use regia_render::exposure_zones::{ZONE_OPEN_RANGE, combine_weighted_zones};
use regia_render::{
    CpuMesh, ExposureZone, ExposureZoneShape, FogSunShadowRule, FogVolume, LabMaterial, MeshRenderer,
};
use regia_scene::GlobalTransform;
use regia_world_geom::sections::{ZONE_EXPOSURE, ZONE_PAD_M};

use crate::scene::{
    self, CamFrame, FLOOR_Y, HALF_WIDTH, RAMP_START_Z, SlotFill, TUBE_END_Z, TUBE_START_Z, TunnelGeometry,
    TunnelParts, Variant,
};

/// The exposure the tube asks the eye for, EV100 (low and high end): the band the world cook
/// gives every tunnel section's zone (spec 121 § 5.6, `regia_world_geom::sections::ZONE_EXPOSURE`).
///
/// ADR-8629: the band and the blend are the cook's; the box placement and the speeds stay the
/// fixture's (ADR-3257). Before, the band was a hand tuning against the flat-floor tube (EV 5-10,
/// 40 m), which metered about EV 6; since the interior shade reads the tube's bake (ADR-7975) the
/// tube meters EV 7.7-10 within 40 m of the portal, that band no longer bound there, and the fast
/// eye made the turnaround 40 m in step harder than the lagging plain meter (27.5 against 20.2
/// levels). Deeper in, the tube meters under this band, and the zone holds the eye on it.
pub const TUBE_EV: (f32, f32) = (ZONE_EXPOSURE[0], ZONE_EXPOSURE[1]);
/// Where the zone's box starts along the tube (z, metres); it runs to the end cap. Starting
/// 15 m in puts the portal inside the blend band, so the range keeps narrowing while the camera
/// crosses the portal (ADR-3257). A box from the portal (the covered section, as the cook boxes
/// it) eases the entry by 1.2 levels only, at either speed: at weight 1 on the portal the eye has
/// nothing left to adapt while the roof darkens the picture (ADR-8629).
pub const TUBE_ZONE_START_Z: f32 = 15.0;
/// Metres outside the zone's box over which it fades in (the cook's blend, spec 121 § 5.6).
pub const TUBE_ZONE_BLEND_M: f32 = ZONE_EXPOSURE[2];
/// How far the zone's box reaches past the tube's free space on every side, metres (the cook's
/// pad, `regia_world_geom::sections::ZONE_PAD_M`).
pub const TUBE_ZONE_PAD_M: f32 = ZONE_PAD_M as f32;
/// The zone's adaptation speeds, EV per second (brighter, darker). A cooked zone carries no
/// speeds; with the zone defaults (2.0 and 1.5, spec 25 § 7.5) this drive's entry eases by at
/// most 1.3 levels in the five placements measured (ADR-8629): at 2 m a frame the roof darkens the
/// picture in one frame, and 1.5 EV/s moves the eye 14 % of its gap in a 0.1 s frame.
pub const TUBE_ZONE_SPEEDS: (f32, f32) = (8.0, 8.0);

/// `exposure_zone`: an exposure zone box over the tube.
pub struct TubeExposureZone {
    /// The zone.
    pub zone: ExposureZone,
    /// Where it sits (the box's centre).
    pub at: GlobalTransform,
}

impl TubeExposureZone {
    /// The zone over the scene's interior volume, from [`TUBE_ZONE_START_Z`] to the end cap.
    /// `REGIA_TUNNEL_ZONE=ev_min,ev_max,blend,up,down,start_z` overrides the numbers (by hand, to
    /// compare tunings; the tests run the constants).
    pub fn new() -> Self {
        let interior = scene::interior_volume();
        let mut n = [
            TUBE_EV.0,
            TUBE_EV.1,
            TUBE_ZONE_BLEND_M,
            TUBE_ZONE_SPEEDS.0,
            TUBE_ZONE_SPEEDS.1,
            TUBE_ZONE_START_Z,
        ];
        if let Ok(v) = regia_core::brand::env_var("REGIA_TUNNEL_ZONE") {
            for (slot, value) in n.iter_mut().zip(v.split(',')) {
                if let Ok(value) = value.trim().parse() {
                    *slot = value;
                }
            }
        }
        Self {
            zone: ExposureZone {
                shape: ExposureZoneShape::Box {
                    half_extents: [
                        interior.half_extent[0] + TUBE_ZONE_PAD_M,
                        interior.half_extent[1] + TUBE_ZONE_PAD_M,
                        (TUBE_END_Z - n[5]) * 0.5 + TUBE_ZONE_PAD_M,
                    ],
                },
                ev_min: n[0],
                ev_max: n[1],
                blend_distance: n[2],
                adapt_up_speed: n[3],
                adapt_down_speed: n[4],
                priority: 0,
            },
            at: GlobalTransform(Affine3A::from_translation(Vec3::new(
                interior.center[0],
                interior.center[1],
                (TUBE_END_Z + n[5]) * 0.5,
            ))),
        }
    }

    /// Whether `eye` is inside the zone's box.
    pub fn contains(&self, eye: [f32; 3]) -> bool {
        self.zone.signed_distance(&self.at, eye) <= 0.0
    }

    /// The zone's weight at `eye`, as `regia_framework::exposure_zones` weighs it.
    pub fn weight(&self, eye: [f32; 3]) -> f32 {
        self.zone.weight_at(&self.at, eye)
    }
}

impl SlotFill for TubeExposureZone {
    fn apply(&self, geometry: &TunnelGeometry, parts: &mut TunnelParts) {
        // What `regia_framework::sync_exposure_zones` does for the active camera.
        let weight = self.weight(geometry.eye);
        parts.exposure_target = (weight.is_finite() && weight > 0.0)
            .then(|| combine_weighted_zones(&[(weight, self.zone)], ZONE_OPEN_RANGE));
    }
}

/// `reverb_zone`: the tube preset's reverb zone over the covered tube.
pub struct TubeReverbZone;

impl SlotFill for TubeReverbZone {
    fn place(&self, geometry: &TunnelGeometry, world: &mut World) {
        let interior = geometry.interior;
        world.spawn((
            AudioReverbZone::tube(ReverbZoneShape::Box {
                half_extents: Vec3::from_array(interior.half_extent),
            }),
            GlobalTransform(Affine3A::from_translation(Vec3::from_array(interior.center))),
        ));
    }
}

/// `probe_volume`: the tube's baked GI volume ([`tube_probe_volume`]).
pub struct TubeProbeVolume;

impl SlotFill for TubeProbeVolume {
    fn apply(&self, _geometry: &TunnelGeometry, parts: &mut TunnelParts) {
        parts.baked_gi = Some(tube_probe_volume());
        // The volume lights single-sample frames only (`FrameRenderer::set_baked_gi`).
        parts.quality.msaa_samples = 1;
    }
}

/// Probe spacing of the tube's bake, metres.
pub const PROBE_SPACING_M: f32 = 2.5;
/// Rays per probe of the tube's bake (reduced: a test fixture, not a shipping bake).
pub const PROBE_RAYS: u32 = 256;

/// The PT scene the tube is baked in: the frame's meshes and sun (no headlight: the bake holds
/// the sun's bounce; the sky is black, as the tube's interior shade has it) plus the ground around the cut, which the frame draws as the
/// terrain (`SharedPassDraw`, not part of the extracted frame).
pub fn tube_pt_scene() -> regia_pt::PtScene {
    let cam = CamFrame {
        index: 0,
        eye: [0.0, FLOOR_Y + 1.6, 20.0],
        target: [0.0, FLOOR_Y + 1.6, 34.0],
        time: 0.0,
        inbound: true,
    };
    let (_, parts) = scene::base_parts_for(Variant::On, 0.0, 0.1, &cam);
    let mut lab = parts.lab;
    lab.local_lights.clear();
    let mut pt = from_frame(
        &lab.as_frame_scene_with_quality(parts.quality),
        &ExtractOptions::default(),
    );
    // The frame draws the tube double-sided, with its quads wound either way; the bake tells air
    // from rock by which side of a face a probe sees, so every face is turned toward the tube's
    // air (the lip and the cut's rim toward the sky) and lit by its face normal.
    for mesh in &mut pt.meshes {
        face_the_air(mesh);
    }
    let ground = pt.add_material(PtMaterial::lambert([0.4; 3]));
    let (x, r0, t0, far) = (HALF_WIDTH, RAMP_START_Z, TUBE_START_Z, 200.0);
    // Four quads at y = 0 around the cut, normals up.
    for [x0, z0, x1, z1] in [
        [-far, -far, -x, far],
        [x, -far, far, far],
        [-x, -far, x, r0],
        [-x, t0, x, far],
    ] {
        let mesh = pt.add_mesh(scenes::quad([
            [x0, 0.0, z0],
            [x0, 0.0, z1],
            [x1, 0.0, z1],
            [x1, 0.0, z0],
        ]));
        pt.add_instance(mesh, ground, Mat4::IDENTITY);
    }
    pt
}

/// Below this height a level face near the ground is the roof (above it, the lip).
const LIP_ROOF_SPLIT_Y: f32 = -0.2;
/// How far under [`LIP_ROOF_SPLIT_Y`] a level face still counts as the roof.
const ROOF_BAND_M: f32 = 0.6;

/// Winds each triangle of `mesh` so its face normal points into the tube's air: floors and the
/// lip up, the roof down, the walls toward the tube's axis and the faces across the tube
/// (headwall, end cap) toward the cut (−z). Vertex normals are cleared so the tracer uses the
/// face normal.
fn face_the_air(mesh: &mut regia_pt::PtMesh) {
    let p = |i: u32| Vec3::from_array(mesh.positions[i as usize]);
    let mut indices = mesh.indices.clone();
    for tri in indices.as_chunks_mut::<3>().0 {
        let (a, b, c) = (p(tri[0]), p(tri[1]), p(tri[2]));
        let n = (b - a).cross(c - a);
        let centre = (a + b + c) / 3.0;
        let want = if n.y.abs() > 0.5 * n.length() {
            // The roof (just under the ground) faces down; the floors and the lip face up.
            if centre.y < LIP_ROOF_SPLIT_Y && centre.y > -0.2 - ROOF_BAND_M {
                Vec3::NEG_Y
            } else {
                Vec3::Y
            }
        } else if n.z.abs() > 0.9 * n.length() {
            Vec3::NEG_Z
        } else {
            Vec3::new(-centre.x, 0.0, 0.0)
        };
        if n.dot(want) < 0.0 {
            tri.swap(1, 2);
        }
    }
    mesh.indices = indices;
    mesh.normals.fill([0.0; 3]);
}

/// The tube's probe layout: a ground-following layer over the ramp's last metres and the tube
/// floor (columns every [`PROBE_SPACING_M`] along the tube, 2.25 m across), four levels from
/// 0.5 m to 4.5 m above the road, so the floor's receivers sit under the lowest level the way
/// a terrain layer lights the ground.
pub fn tube_probe_layout() -> VolumeLayout {
    let (z0, z1) = (TUBE_START_Z - 4.0, TUBE_END_Z - 0.5);
    let nz = ((z1 - z0) / PROBE_SPACING_M).floor() as u32 + 1;
    let counts = [5u32, nz];
    let spacing = [2.25, PROBE_SPACING_M];
    let origin = [-4.5, z0];
    let ground = (0..nz)
        .flat_map(|k| {
            let z = origin[1] + k as f32 * spacing[1];
            std::iter::repeat_n(scene::road_y(z), counts[0] as usize)
        })
        .collect();
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin,
            spacing,
            counts,
            levels: vec![0.5, 1.75, 3.0, 4.5],
            ground,
        }),
        bricks: Vec::new(),
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    layout
}

/// The tube's baked GI volume: [`tube_pt_scene`] baked over [`tube_probe_layout`] by the CPU
/// probe tracer at [`PROBE_RAYS`] rays a probe, once per test binary (deterministic: fixed seed,
/// fixed layout; nothing is written to disk).
pub fn tube_probe_volume() -> Arc<BakedGiVolume> {
    static VOLUME: OnceLock<Arc<BakedGiVolume>> = OnceLock::new();
    VOLUME
        .get_or_init(|| {
            let started = std::time::Instant::now();
            let settings = GiBakeSettings {
                trace: ProbeTraceSettings {
                    rays_per_probe: PROBE_RAYS,
                    samples: 1,
                    max_bounces: 2,
                    seed: 11,
                    ..ProbeTraceSettings::default()
                },
                ..GiBakeSettings::default()
            };
            let layout = tube_probe_layout();
            let bake = match bake_gi_volume(
                &CpuProbeTracer::new(),
                &tube_pt_scene(),
                &layout,
                &settings,
                |_, _, _| true,
            ) {
                Ok(bake) => bake,
                Err(err) => panic!("CPU bake of the tube: {err}"),
            };
            eprintln!(
                "tube probe volume: {} probes, CPU bake {:.1} s, {:?}",
                layout.probe_count(),
                started.elapsed().as_secs_f32(),
                bake.report
            );
            Arc::new(bake.volume)
        })
        .clone()
}

/// Fog density of the interior fog, 1/m.
pub const INTERIOR_FOG_DENSITY: f32 = 0.008;

/// `interior_fog`: fog bounded by the tube's interior volume.
pub struct InteriorFog;

impl SlotFill for InteriorFog {
    fn apply(&self, geometry: &TunnelGeometry, parts: &mut TunnelParts) {
        parts
            .outdoor
            .volumes
            .push(FogVolume::interior(geometry.interior, INTERIOR_FOG_DENSITY));
    }
}

/// The froxel fog's sun-shadow rule the shaft uses.
pub const SHAFT_RULE: FogSunShadowRule = FogSunShadowRule::Calibrated;

/// `light_shafts`: the sun lights the cut's air through the opening and the roof's shadow ends
/// the shaft; the fog reads the sun shadow with [`SHAFT_RULE`].
pub struct CutLightShafts;

/// [`SHAFT_RULE`], or `REGIA_SHAFT_RULE=legacy|calibrated` (by hand, to compare the rules).
pub fn shaft_rule() -> FogSunShadowRule {
    match regia_core::brand::env_var("REGIA_SHAFT_RULE").ok().as_deref() {
        Some("legacy") => FogSunShadowRule::Legacy,
        Some("calibrated") => FogSunShadowRule::Calibrated,
        _ => SHAFT_RULE,
    }
}

impl SlotFill for CutLightShafts {
    fn apply(&self, _geometry: &TunnelGeometry, parts: &mut TunnelParts) {
        parts.outdoor.fog.sun_shadow = shaft_rule();
    }
}

/// The pick layer the tube's meshes are on.
pub const TUBE_LAYER: u8 = 1;

/// `world_pick`: the tube's render meshes as pick entities on [`TUBE_LAYER`].
pub struct TubePickLayer;

impl SlotFill for TubePickLayer {
    fn place(&self, _geometry: &TunnelGeometry, world: &mut World) {
        let material = Arc::new(LabMaterial::default());
        for mesh in scene::tunnel_meshes(0.0) {
            let cpu = Arc::new(CpuMesh {
                vertices: mesh.vertices,
                indices: mesh.indices,
            });
            world.spawn((
                MeshRenderer::new(cpu, material.clone()),
                GlobalTransform(Affine3A::IDENTITY),
                MapLayer(TUBE_LAYER),
            ));
        }
    }
}
