// SPDX-License-Identifier: MPL-2.0
#![allow(
    dead_code,
    reason = "each test binary module uses a different subset of the scene's helpers"
)]

//! The synthetic tunnel: a terrain with an open-cut ramp hole, a tube under the ground, a lip
//! around the cut, an interior volume, a fog shaft, a headlight with dust, and named slots for
//! the tunnel features (exposure zone, reverb zone, probe volume, interior fog, light shafts,
//! world pick layers; `fills.rs` plugs them in).

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::BakedGiVolume;
use regia_core::glam::{Mat4, Vec3};
use regia_ecs::prelude::World;
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, EvTarget, FogLight, FogVolume,
    FrameQuality, FrameRenderer, InteriorVolume, LabLocalLight, LabMaterial, LabMesh, LabScene, LabSpotLight,
    LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
    TimeOfDay, VolumetricFogSettings, shade_for_eye,
};
use regia_terrain::{
    CdlodConfig, HoleShape, HolesMask, LayerDesc, LayerTextures, MaterialParams, MinMaxPyramid, Rgba8Image,
    Terrain, TerrainLayers, TerrainMap, TerrainMaterial, TerrainSharedDraw,
};

/// Frame width in pixels.
pub const W: u32 = 320;
/// Frame height in pixels.
pub const H: u32 = 180;
const FOV_Y: f32 = 1.0;
const NEAR: f32 = 0.3;
const FAR: f32 = 1500.0;
/// Radius of the flat ground around the mouth (rolling hills beyond).
const FLAT_R: f32 = 150.0;

/// Half the tube's width, metres.
pub const HALF_WIDTH: f32 = 5.0;
/// Where the open ramp starts on the ground (z), metres.
pub const RAMP_START_Z: f32 = -20.0;
/// Where the covered tube starts (z), metres.
pub const TUBE_START_Z: f32 = 0.0;
/// Where the covered tube ends (z), metres.
pub const TUBE_END_Z: f32 = 80.0;
/// Tube floor height, metres.
pub const FLOOR_Y: f32 = -6.0;
/// Underside of the tube roof, metres (the roof sits just under the ground).
pub const ROOF_Y: f32 = -0.5;
/// Eye height above the road, metres.
const EYE_ABOVE_ROAD: f32 = 1.6;
/// Lip strip width on the inside of the cut, metres.
pub const LIP_WIDTH: f32 = 0.8;
/// How far the lip reaches under the ground past the cut edge, metres.
pub const LIP_OVERLAP: f32 = 0.25;
/// Height of the lip, metres: flush with the ground, 1 cm under it. Every surface of the cut
/// (trench walls, headwall, the ramp's top end) stops at this height, under the lip: a wall that
/// rose to the ground poked 1 cm up through the lip, and wherever the drawn hole edge overshoots
/// the cut (up to half a mask texel) the view hit that wall top instead of the lip's overlap, one
/// trench-coloured pixel per staircase step along both side edges (measured on lavapipe: 63 of
/// 131 edge samples at camera z -26, the same colour with the lip removed; lane v020r).
pub const LIP_Y: f32 = -0.01;

/// Which behaviour the frame renders with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variant {
    /// Today's default: no interior volume, no fog, the tube is lit as open sky.
    Old,
    /// The tunnel set: interior volume (interior shade), fog shaft, dust in the headlight beam.
    On,
}

impl Variant {
    /// Directory / label name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::On => "on",
        }
    }
}

/// Everything a slot filler may change before a frame renders.
pub struct TunnelParts {
    /// Sky, fog, interiors and fog lights.
    pub outdoor: OutdoorParams,
    /// The post stack (exposure).
    pub post: PostSettings,
    /// Frame quality knobs.
    pub quality: FrameQuality,
    /// Meshes and lights of the frame.
    pub lab: LabScene,
    /// The exposure-zone target for this frame (`FrameRenderer::set_exposure_target`); `None`
    /// leaves plain auto exposure.
    pub exposure_target: Option<EvTarget>,
    /// The baked GI volume the frame is lit with (`FrameRenderer::set_baked_gi`); `None` bakes
    /// nothing in.
    pub baked_gi: Option<Arc<BakedGiVolume>>,
}

/// Where the tunnel is, for slot fillers.
#[derive(Clone, Copy, Debug)]
pub struct TunnelGeometry {
    /// The interior box (the covered tube).
    pub interior: InteriorVolume,
    /// Centre of the open cut on the ground, `(x, z)`.
    pub mouth_center: [f32; 2],
    /// Half size of the open cut on the ground, `(x, z)`.
    pub mouth_half: [f32; 2],
    /// Camera of the frame being built.
    pub eye: [f32; 3],
    /// Seconds between two frames of the path (the exposure adaptation step).
    pub dt: f32,
}

/// A tunnel feature plugged into a [`Slot`]. Both hooks default to doing nothing.
pub trait SlotFill: Send + Sync {
    /// Called every frame, after the scene is built and before it renders.
    fn apply(&self, _geometry: &TunnelGeometry, _parts: &mut TunnelParts) {}

    /// Places the feature's entities in the tunnel's ECS world ([`tunnel_world`]): features that
    /// live on entities rather than in the frame (reverb zones, pick layers).
    fn place(&self, _geometry: &TunnelGeometry, _world: &mut World) {}
}

/// A named place in the scene for one tunnel feature. With no filler it is a no-op
/// ([`Slots::none`], the old behaviour); [`Slots::today`] fills every slot.
pub struct Slot {
    /// What the slot stands for.
    pub name: &'static str,
    /// The feature module that fills it.
    pub owner_lane: &'static str,
    /// The feature, once it exists.
    pub fill: Option<Box<dyn SlotFill>>,
}

impl Slot {
    /// An empty slot.
    pub fn empty(name: &'static str, owner_lane: &'static str) -> Self {
        Self {
            name,
            owner_lane,
            fill: None,
        }
    }

    /// True once a feature is plugged in.
    pub fn is_filled(&self) -> bool {
        self.fill.is_some()
    }
}

/// The slots of the tunnel scene (see the README next to this file).
pub struct Slots {
    /// A volume that pins the exposure inside and eases it across the mouth.
    pub exposure_zone: Slot,
    /// A reverb zone over the tube (audio; the frame is unaffected).
    pub reverb_zone: Slot,
    /// A reflection / GI probe volume for the tube.
    pub probe_volume: Slot,
    /// Fog that only exists inside the interior volume.
    pub interior_fog: Slot,
    /// Light shafts through the open cut.
    pub light_shafts: Slot,
    /// World pick layers: a ray into the mouth must hit the tube, not the terrain.
    pub world_pick: Slot,
}

impl Slots {
    /// Every slot empty: the scene without the tunnel features (the old path and the detector
    /// controls render with this, so their frames do not move when a feature changes).
    pub fn none() -> Self {
        Self {
            exposure_zone: Slot::empty("exposure zone", "regia_render::exposure_zones"),
            reverb_zone: Slot::empty("reverb zone", "regia_audio::reverb_zone"),
            probe_volume: Slot::empty("probe volume", "regia_render::baked_gi (regia_pt CPU bake)"),
            interior_fog: Slot::empty("interior fog", "regia_render::volumetrics (FogVolume::interior)"),
            light_shafts: Slot::empty("light shafts", "regia_render::fog (froxel sun shadow)"),
            world_pick: Slot::empty("world pick layers", "regia_pick::world"),
        }
    }

    /// Every slot filled with the feature that exists today (`fills.rs`).
    pub fn today() -> Self {
        let mut slots = Self::none();
        slots.exposure_zone.fill = Some(Box::new(crate::fills::TubeExposureZone::new()));
        slots.reverb_zone.fill = Some(Box::new(crate::fills::TubeReverbZone));
        slots.probe_volume.fill = Some(Box::new(crate::fills::TubeProbeVolume));
        slots.interior_fog.fill = Some(Box::new(crate::fills::InteriorFog));
        slots.light_shafts.fill = Some(Box::new(crate::fills::CutLightShafts));
        slots.world_pick.fill = Some(Box::new(crate::fills::TubePickLayer));
        slots
    }

    fn all(&self) -> [&Slot; 6] {
        [
            &self.exposure_zone,
            &self.reverb_zone,
            &self.probe_volume,
            &self.interior_fog,
            &self.light_shafts,
            &self.world_pick,
        ]
    }
}

/// One step of the scripted camera path.
#[derive(Clone, Copy, Debug)]
pub struct CamFrame {
    /// Frame index.
    pub index: usize,
    /// Eye position.
    pub eye: [f32; 3],
    /// Look-at point.
    pub target: [f32; 3],
    /// Seconds since the path started.
    pub time: f32,
    /// `true` while driving into the tunnel, `false` on the way out.
    pub inbound: bool,
}

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Road height under the camera at `z` (the ramp descends from the ground into the tube).
pub fn road_y(z: f32) -> f32 {
    if z <= RAMP_START_Z {
        0.0
    } else if z >= TUBE_START_Z {
        FLOOR_Y
    } else {
        FLOOR_Y * (z - RAMP_START_Z) / (TUBE_START_Z - RAMP_START_Z)
    }
}

/// The scripted camera path: drive from `-50` m to `+40` m inside the tube, turn around in
/// eight frames, drive back out. `speed` is m/s and `fps` the frame rate it is sampled at.
pub fn camera_path(speed: f32, fps: f32) -> Vec<CamFrame> {
    camera_path_to(speed, fps, PATH_TURN_Z)
}

/// Where [`camera_path`] turns around (z, metres): 40 m into the tube.
pub const PATH_TURN_Z: f32 = 40.0;

/// [`camera_path`] turning around at `turn_z` instead of [`PATH_TURN_Z`] (the same approach,
/// speed, eight-frame turn and drive back out).
pub fn camera_path_to(speed: f32, fps: f32, turn_z: f32) -> Vec<CamFrame> {
    let step = speed / fps;
    let (z0, z1) = (-50.0f32, turn_z);
    let mut frames = Vec::new();
    let mut push = |eye: Vec3, dir: Vec3, inbound: bool| {
        let index = frames.len();
        frames.push(CamFrame {
            index,
            eye: eye.to_array(),
            target: (eye + dir * 14.0).to_array(),
            time: index as f32 / fps,
            inbound,
        });
    };
    let look = |z: f32, sign: f32| {
        // Look along the road: toward the road height 14 m ahead.
        let ahead = road_y(z + sign * 14.0) - road_y(z);
        Vec3::new(0.0, ahead / 14.0, sign).normalize()
    };
    let mut z = z0;
    while z <= z1 {
        push(Vec3::new(0.0, road_y(z) + EYE_ABOVE_ROAD, z), look(z, 1.0), true);
        z += step;
    }
    let turn_z = z - step;
    for k in 1..=8 {
        let yaw = std::f32::consts::PI * k as f32 / 8.0;
        let dir = Vec3::new(yaw.sin(), 0.0, yaw.cos());
        push(
            Vec3::new(0.0, road_y(turn_z) + EYE_ABOVE_ROAD, turn_z),
            dir,
            false,
        );
    }
    let mut z = turn_z;
    while z >= z0 {
        push(
            Vec3::new(0.0, road_y(z) + EYE_ABOVE_ROAD, z),
            look(z, -1.0),
            false,
        );
        z -= step;
    }
    frames
}

/// The open cut in the ground: a 10 m × 20 m rectangle.
pub fn mouth_mask() -> HolesMask {
    HolesMask::from_shapes(vec![HoleShape::Rect {
        center: [0.0, (RAMP_START_Z + TUBE_START_Z) * 0.5],
        half_extents: [HALF_WIDTH, (TUBE_START_Z - RAMP_START_Z) * 0.5],
        rotation_radians: 0.0,
    }])
    .expect("mask")
}

const TERRAIN_N: u32 = 513;
const TERRAIN_WORLD: f32 = 512.0;

fn terrain_xz(i: usize) -> [f32; 2] {
    let n = TERRAIN_N as usize;
    let to_world = |c: usize| c as f32 / (n - 1) as f32 * TERRAIN_WORLD - TERRAIN_WORLD * 0.5;
    [to_world(i % n), to_world(i / n)]
}

fn terrain_heights() -> Vec<f32> {
    (0..(TERRAIN_N * TERRAIN_N) as usize)
        .map(|i| {
            let [x, z] = terrain_xz(i);
            let t = (((x * x + z * z).sqrt() - FLAT_R) / 40.0).clamp(0.0, 1.0);
            6.0 * (x / 37.0).sin() * (z / 29.0).cos() * t * t * (3.0 - 2.0 * t)
        })
        .collect()
}

/// The collider's view of the cut: heightfield samples set to NaN inside the mask
/// (`HolesMask::cut_heights`) and how many were cut.
pub fn cut_collider_heights(mask: &HolesMask) -> (Vec<f32>, usize) {
    let mut heights = terrain_heights();
    let cut = mask.cut_heights(&mut heights, 0.0, terrain_xz);
    (heights, cut)
}

/// Grey ground. `holes` cuts the mouth through it.
pub fn terrain_draw(holes: bool) -> TerrainSharedDraw {
    let map = TerrainMap::from_heights(
        TERRAIN_N,
        TERRAIN_N,
        terrain_heights(),
        TERRAIN_WORLD,
        40.0,
        -20.0,
    )
    .expect("map");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let v = regia_terrain::linear_to_srgb8(0.4);
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    if holes {
        draw.set_holes(Some(&mouth_mask()));
    }
    draw
}

fn quad(corners: [[f32; 3]; 4], material: usize) -> LabMesh {
    let a = Vec3::from_array(corners[1]) - Vec3::from_array(corners[0]);
    let b = Vec3::from_array(corners[3]) - Vec3::from_array(corners[0]);
    let normal = a.cross(b).normalize_or_zero().to_array();
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

fn tri(corners: [[f32; 3]; 3], material: usize) -> LabMesh {
    let a = Vec3::from_array(corners[1]) - Vec3::from_array(corners[0]);
    let b = Vec3::from_array(corners[2]) - Vec3::from_array(corners[0]);
    let normal = a.cross(b).normalize_or_zero().to_array();
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2],
        material,
        ..LabMesh::unskinned()
    }
}

const MAT_TUBE: usize = 0;
const MAT_LIP: usize = 1;

/// The tube mesh and the lip ring around the cut. `lip_gap` pulls the lip's outer edge back
/// from the cut edge (metres): 0 leaves the usual under-ground overlap, a positive value leaves
/// a deliberate crack for the detector's negative control.
pub fn tunnel_meshes(lip_gap: f32) -> Vec<LabMesh> {
    let (x, r0, t0, t1) = (HALF_WIDTH, RAMP_START_Z, TUBE_START_Z, TUBE_END_Z);
    let (f, c, top) = (FLOOR_Y, ROOF_Y, LIP_Y);
    let mut meshes = vec![
        // Ramp floor and tube floor.
        quad([[-x, top, r0], [x, top, r0], [x, f, t0], [-x, f, t0]], MAT_TUBE),
        quad([[-x, f, t0], [x, f, t0], [x, f, t1], [-x, f, t1]], MAT_TUBE),
        // Ramp trench walls (triangles) and tube walls. The cut's surfaces stop under the lip.
        tri([[-x, top, r0], [-x, f, t0], [-x, top, t0]], MAT_TUBE),
        tri([[x, top, r0], [x, top, t0], [x, f, t0]], MAT_TUBE),
        quad([[-x, f, t0], [-x, f, t1], [-x, c, t1], [-x, c, t0]], MAT_TUBE),
        quad([[x, f, t0], [x, c, t0], [x, c, t1], [x, f, t1]], MAT_TUBE),
        // Roof, headwall above the portal, end cap.
        quad([[-x, c, t0], [-x, c, t1], [x, c, t1], [x, c, t0]], MAT_TUBE),
        quad([[-x, c, t0], [x, c, t0], [x, top, t0], [-x, top, t0]], MAT_TUBE),
        quad([[-x, f, t1], [x, f, t1], [x, c, t1], [-x, c, t1]], MAT_TUBE),
    ];
    // The lip: four strips flush with the ground (`LIP_Y`), on the inside of the cut and
    // reaching `LIP_OVERLAP` under the ground past the cut edge.
    let y = LIP_Y;
    let outer = x + LIP_OVERLAP - lip_gap;
    let inner = x - LIP_WIDTH;
    let lip = |a: [f32; 2], b: [f32; 2]| {
        quad(
            [[a[0], y, a[1]], [b[0], y, a[1]], [b[0], y, b[1]], [a[0], y, b[1]]],
            MAT_LIP,
        )
    };
    let (z_lo, z_hi) = (r0 - LIP_OVERLAP + lip_gap, t0 + LIP_OVERLAP - lip_gap);
    meshes.push(lip([-outer, z_lo], [-inner, z_hi]));
    meshes.push(lip([inner, z_lo], [outer, z_hi]));
    meshes.push(lip([-outer, z_lo], [outer, r0 + LIP_WIDTH]));
    meshes.push(lip([-outer, t0 - LIP_WIDTH], [outer, z_hi]));
    meshes
}

fn materials() -> Vec<LabMaterial> {
    vec![
        LabMaterial {
            base_color: [0.45, 0.30, 0.18, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        },
        // The lip is green so the crack detector can tell a crack from a blend: every
        // ground–lip blend (grey to green) stays about 40 levels or more from the trench's brown
        // and the sky's blue. With a yellow lip the grey–yellow blends ran through tan, within
        // 30 levels of the lit trench, and a 0.5 m gap read as a blend on most samples (34 of 131
        // flagged on lavapipe).
        LabMaterial {
            base_color: [0.10, 0.60, 0.10, 1.0],
            roughness: 0.8,
            ..LabMaterial::default()
        },
    ]
}

/// The covered tube as an interior volume.
pub fn interior_volume() -> InteriorVolume {
    let half = [
        HALF_WIDTH,
        (ROOF_Y - FLOOR_Y) * 0.5,
        (TUBE_END_Z - TUBE_START_Z) * 0.5,
    ];
    InteriorVolume::with_floor(
        [0.0, (ROOF_Y + FLOOR_Y) * 0.5, (TUBE_END_Z + TUBE_START_Z) * 0.5],
        half,
    )
}

/// Frames [`TunnelScene::warm_up`] renders: the two-frame histogram readback and the outright
/// first sample take three; the rest cover a readback that lands a frame late (the histogram
/// poll does not block). The view does not change, so later samples meter the same value.
const WARM_UP_FRAMES: usize = 6;

/// The scene builder: the pieces of one tunnel and the slots for the ones that do not exist yet.
pub struct TunnelScene {
    /// Behaviour the frames render with.
    pub variant: Variant,
    /// Slots for features other lanes are building.
    pub slots: Slots,
    /// Lip gap (metres) for the crack detector's negative control.
    pub lip_gap: f32,
    /// Whether the terrain mesh is cut (off for the detector's other negative control).
    pub holes: bool,
    /// Seconds between two frames (the exposure zone's adaptation step).
    pub dt: f32,
    renderer: FrameRenderer,
}

/// The tunnel scene of `variant`: the old path renders without the tunnel features
/// ([`Slots::none`]), the tunnel set with every slot filled ([`Slots::today`]).
pub fn tunnel_scene(variant: Variant) -> TunnelScene {
    let slots = match variant {
        Variant::Old => Slots::none(),
        Variant::On => Slots::today(),
    };
    TunnelScene::new(variant, slots, 0.0, true)
}

impl TunnelScene {
    /// A scene with explicit slots, lip gap and cut switch.
    pub fn new(variant: Variant, slots: Slots, lip_gap: f32, holes: bool) -> Self {
        let mut renderer = FrameRenderer::new();
        // Diagnostic: `REGIA_AERIAL_INTERIORS=0` draws the aerial perspective as before ADR-4757 (the before
        // plates; the air inside the bore then takes the sky's in-scatter).
        if regia_core::brand::env_var_os("REGIA_AERIAL_INTERIORS").is_some_and(|v| v == "0") {
            renderer.set_aerial_interiors(false);
        }
        // The path steps 1-2 m a frame: the automatic cut detection would snap exposure.
        renderer.set_camera_cut_detection(None);
        renderer.set_shared_draw(Some(
            Box::new(terrain_draw(holes)) as Box<dyn SharedPassDraw + Send + Sync>
        ));
        Self {
            variant,
            slots,
            lip_gap,
            holes,
            dt: 0.1,
            renderer,
        }
    }

    /// The renderer (exposure report, baked GI lookups).
    pub fn renderer(&self) -> &FrameRenderer {
        &self.renderer
    }

    /// Names and owner lanes of the slots that are still empty.
    pub fn empty_slots(&self) -> Vec<(&'static str, &'static str)> {
        self.slots
            .all()
            .iter()
            .filter(|slot| !slot.is_filled())
            .map(|slot| (slot.name, slot.owner_lane))
            .collect()
    }

    /// Renders `cam` until auto exposure has metered it, discarding the frames.
    ///
    /// A fresh renderer applies `PostSettings::exposure_ev` until its first histogram comes back
    /// (two frames later) and adopts that sample outright: the old path's frame 2 jumped 67
    /// levels (187 to 120) on lavapipe from that alone, 40 m before the mouth. That start-up
    /// step is the renderer's documented first-view behaviour (`camera_cut.rs` covers it); the
    /// path measures the drive, so it starts the way a game does, exposure settled on the
    /// first view.
    pub fn warm_up(&mut self, cam: &CamFrame) {
        for _ in 0..WARM_UP_FRAMES {
            let _ = self.render(cam);
        }
    }

    /// Renders the frame at `cam`: RGBA8 pixels.
    pub fn render(&mut self, cam: &CamFrame) -> Vec<u8> {
        self.render_tweaked(cam, &|_, _| {})
    }

    /// The frame's pieces at `cam` before any slot runs.
    pub fn base_parts(&self, cam: &CamFrame) -> (TunnelGeometry, TunnelParts) {
        base_parts_for(self.variant, self.lip_gap, self.dt, cam)
    }

    /// The frame's pieces at `cam` with every filled slot applied.
    pub fn parts(&self, cam: &CamFrame) -> (TunnelGeometry, TunnelParts) {
        let (geometry, mut parts) = self.base_parts(cam);
        for slot in self.slots.all() {
            if let Some(fill) = &slot.fill {
                fill.apply(&geometry, &mut parts);
            }
        }
        (geometry, parts)
    }

    /// Renders the frame at `cam` after `tweak` has changed the slotted pieces (a test's
    /// controlled variation: fixed exposure, one fog volume, no headlight).
    pub fn render_tweaked(
        &mut self,
        cam: &CamFrame,
        tweak: &dyn Fn(&TunnelGeometry, &mut TunnelParts),
    ) -> Vec<u8> {
        let (geometry, mut parts) = self.parts(cam);
        tweak(&geometry, &mut parts);
        self.render_parts(parts)
    }

    /// Renders prepared pieces.
    pub fn render_parts(&mut self, parts: TunnelParts) -> Vec<u8> {
        let TunnelParts {
            outdoor,
            post,
            quality,
            lab,
            exposure_target,
            baked_gi,
        } = parts;
        // A zone target steps the eye by the path's frame time, not the wall clock; without one
        // the frame is exposed exactly as without zones (and the step is unused).
        if exposure_target.is_some() {
            self.renderer.set_exposure_dt(Some(self.dt));
        }
        self.renderer.set_exposure_target(exposure_target);
        let same = match (&baked_gi, self.renderer.baked_gi()) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.renderer.set_baked_gi(baked_gi);
        }
        let mut frame = lab.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        self.renderer.set_outdoor(outdoor);
        self.renderer
            .render_offscreen(device(), &frame)
            .expect("tunnel frame")
            .pixels
    }
}

/// The frame's pieces of `variant` at `cam` before any slot runs (`lip_gap` as
/// [`tunnel_meshes`], `dt` the path's frame step).
pub fn base_parts_for(
    variant: Variant,
    lip_gap: f32,
    dt: f32,
    cam: &CamFrame,
) -> (TunnelGeometry, TunnelParts) {
    let on = variant == Variant::On;

    let volume = interior_volume();
    let dir = (Vec3::from_array(cam.target) - Vec3::from_array(cam.eye)).normalize();
    let headlight = LabLocalLight::Spot(LabSpotLight {
        position: cam.eye,
        direction: dir.to_array(),
        color: [1.0, 0.95, 0.85],
        intensity: 40_000.0,
        range: 70.0,
        inner_angle: 0.15,
        outer_angle: 0.45,
        cast_shadows: false,
        source_radius: None,
    });
    let interiors = if on { vec![volume] } else { Vec::new() };
    let shade = shade_for_eye(cam.eye, &interiors, 0.0, true);

    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.interior_ambient = shade.interior;
    let post = PostSettings {
        auto_exposure: true,
        exposure_ev: 13.0,
        ..PostSettings::golden_deterministic()
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye: cam.eye,
        target: cam.target,
        fov_y: FOV_Y,
        near: NEAR,
        far: FAR,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 13.0,
        ambient_lux: shade.ambient_lux,
        double_sided: true,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.25, -0.8, 0.55], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: vec![headlight],
        materials: materials(),
        meshes: tunnel_meshes(lip_gap),
    };
    let mut outdoor = OutdoorParams {
        time: Some(TimeOfDay::noon()),
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Earth),
        interiors,
        ..OutdoorParams::default()
    };
    if on {
        // Dust in the headlight beam inside the tube, and a shaft in the open cut.
        outdoor.fog = VolumetricFogSettings {
            density: 0.0,
            ..VolumetricFogSettings::default()
        };
        outdoor.volumes = vec![
            FogVolume::Box {
                center: [0.0, -3.2, 20.0],
                extents: [HALF_WIDTH, 2.7, 20.0],
                density: 0.01,
            },
            FogVolume::Box {
                center: [0.0, -3.0, -10.0],
                extents: [HALF_WIDTH, 3.0, 10.0],
                density: 0.02,
            },
        ];
        // The headlight itself: 40 000 cd in its cone, as the forward pass lights it
        // (ADR-1365; before, the fog saw a 40 000 / 4π cd omni lamp).
        outdoor.lights = vec![FogLight::from_local(&headlight)];
    }
    let parts = TunnelParts {
        outdoor,
        post,
        quality,
        lab,
        exposure_target: None,
        baked_gi: None,
    };
    (geometry(cam.eye, dt), parts)
}

/// Where the tunnel is, seen from `eye`.
pub fn geometry(eye: [f32; 3], dt: f32) -> TunnelGeometry {
    TunnelGeometry {
        interior: interior_volume(),
        mouth_center: [0.0, (RAMP_START_Z + TUBE_START_Z) * 0.5],
        mouth_half: [HALF_WIDTH, (TUBE_START_Z - RAMP_START_Z) * 0.5],
        eye,
        dt,
    }
}

/// The terrain as an ECS entity: the same heightfield and cut as the drawn ground.
pub fn terrain_component() -> Terrain {
    let map = TerrainMap::from_heights(
        TERRAIN_N,
        TERRAIN_N,
        terrain_heights(),
        TERRAIN_WORLD,
        40.0,
        -20.0,
    )
    .expect("map");
    let minmax = MinMaxPyramid::build(&map, 4).expect("pyramid");
    Terrain {
        map,
        cdlod: CdlodConfig::default(),
        root_level: 4,
        minmax,
        layers: TerrainLayers::default(),
        holes: Some(mouth_mask()),
        water: None,
    }
}

/// The tunnel as an ECS world: the cut terrain, then every filled slot's entities
/// ([`SlotFill::place`]).
pub fn tunnel_world(slots: &Slots) -> World {
    let mut world = World::new();
    world.spawn(terrain_component());
    let geometry = geometry([0.0, 1.6, -50.0], 0.1);
    for slot in slots.all() {
        if let Some(fill) = &slot.fill {
            fill.place(&geometry, &mut world);
        }
    }
    world
}

/// The renderer's camera pair for `cam`: view-projection (right-handed look-at, DirectX
/// perspective), as the frame uses.
pub fn view_proj(cam: &CamFrame) -> Mat4 {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(cam.eye),
        Vec3::from_array(cam.target),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, NEAR, FAR);
    proj * view
}
