// SPDX-License-Identifier: MPL-2.0
//! The impostor hand-over under a moving temporal resolve leaves no 1 px stipple (ADR-3146, lavapipe,
//! no goldens).
//!
//! One crown as large as the view stands halfway through its class's hand-over band: the mesh keeps
//! half of its pixels and the impostor the other half. The camera trucks 0.37 px a frame for 24 frames
//! and the last resolved frame is measured with `regia_render::alpha_hash::stipple_energy`, the
//! spectral test that proved the hashed alpha fix (ADR-1489). The impostor's table is darkened so that
//! the two sides differ the way a far capture differs from a lit mesh, only more: the residual is then
//! well above the 8-bit floor. Without temporal AA the threshold is read pixel by pixel. A held view
//! keeps interleaved gradient noise on both sides, so it renders what it rendered before.

use std::sync::Arc;

use regia_core::glam::Vec3;
use regia_render::alpha_hash::{hierarchy_phase, stipple_energy};
use regia_render::hair::hashed_threshold;
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabMaterial, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, ImpostorLayer, ImpostorSettings, ScatterField, ScatterLibrary, ScatterMeshAsset,
    ScatterMeshLod, ScatterSet, ScatterSource, ScatterView, TerrainLayers, TerrainMap, TerrainSharedDraw,
};

const W: u32 = 160;
const H: u32 = 120;
/// The measured square, centred in the frame.
const SIDE: usize = 96;
const FRAMES: u32 = 24;
const FOV: f32 = 0.8;
const FADE_START: f32 = 60.0;
const FADE_END: f32 = 140.0;
/// Halfway through the band: `(100 - 60) / (140 - 60)`.
const DISTANCE: f32 = 100.0;
/// Crown radius: wider than the view at [`DISTANCE`], so the crown fills the measured square.
const RADIUS: f32 = 50.0;
/// What the impostor's table is darkened by in the spectral tests.
const IMPOSTOR_SHADE: f32 = 0.35;

const SET: &str = r#"
ScatterSet(
    version: 2,
    seed: 5,
    classes: [
        ScatterClass(
            name: "giants",
            meshes: [
                ScatterMeshRef(mesh: "crown", weight: 1.0, min_scale: 1.0, max_scale: 1.0, scale_bias: 1.0),
            ],
            cell_size: 256.0,
            density: 1.0,
            contact_ao: 0.0,
            cast_shadows: false,
            fade_start: 60.0,
            fade_end: 140.0,
        ),
    ],
)
"#;

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [
            LayerDesc(name: "dust", albedo: (0.05, 0.05, 0.05), roughness: 0.92, tiling: 2.0,
                slope_min: 0.0, slope_max: 1.6, altitude_min: -1000.0, altitude_max: 5000.0,
                blend_sharpness: 0.15, triplanar: false),
        ])"#,
    )
    .expect("layers")
}

/// A lat-long sphere standing on the ground.
fn crown() -> ScatterMeshAsset {
    let (rows, cols) = (16u32, 24u32);
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for r in 0..=rows {
        let theta = std::f32::consts::PI * r as f32 / rows as f32;
        for c in 0..=cols {
            let phi = std::f32::consts::TAU * c as f32 / cols as f32;
            let n = [theta.sin() * phi.cos(), theta.cos(), theta.sin() * phi.sin()];
            vertices.push(LabVertex::from_pos_normal_uv(
                [n[0] * RADIUS, n[1] * RADIUS + RADIUS, n[2] * RADIUS],
                n,
                [c as f32 / cols as f32, r as f32 / rows as f32],
            ));
        }
    }
    let stride = cols + 1;
    for r in 0..rows {
        for c in 0..cols {
            let (a, b, d, e) = (
                r * stride + c,
                r * stride + c + 1,
                (r + 1) * stride + c,
                (r + 1) * stride + c + 1,
            );
            indices.extend_from_slice(&[a, b, d, b, e, d]);
        }
    }
    ScatterMeshAsset {
        lods: vec![ScatterMeshLod {
            vertices,
            indices,
            screen_size: 0.0,
        }],
        material: LabMaterial {
            base_color: [0.8, 0.8, 0.8, 1.0],
            roughness: 1.0,
            double_sided: true,
            ..LabMaterial::default()
        },
        parts: Vec::new(),
        shadow_lod: 0,
    }
}

/// Which sides of the hand-over draw.
#[derive(Clone, Copy, PartialEq)]
enum Sides {
    Both,
    MeshOnly,
    ImpostorOnly,
    Neither,
}

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    field: ScatterField,
    layer: ImpostorLayer,
    post: PostSettings,
    quality: FrameQuality,
    sides: Sides,
    /// The eye the view is measured from, before the truck.
    eye: Vec3,
}

impl Rig {
    fn new(sides: Sides, taa: bool, impostor_shade: f32) -> Self {
        let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
        let map = TerrainMap::from_heights(65, 65, vec![0.0; 65 * 65], 1024.0, 1.0, 0.0).expect("map");
        let layers = layers();
        let set = ScatterSet::from_ron(SET, Some(&layers)).expect("scatter set");
        let mut library = ScatterLibrary::new();
        library.insert("crown".into(), crown());
        let source = ScatterSource {
            map: Arc::new(map.clone()),
            holes: None,
            layers: Some(layers.clone()),
            density_maps: regia_terrain::DensityMaps::new(),
        };
        let mut field = ScatterField::new(&set, source, &library, 0xC0).expect("field");
        field.set_background_streaming(false);
        // The far range ends well short of the next trees (256 m cells: 356 m behind this one), so only
        // this crown draws in the measured square: a background impostor seen through its discarded
        // pixels would read as drawn by it.
        let settings = ImpostorSettings {
            grid: 8,
            cell: 32,
            far_end_m: 250.0,
            ..ImpostorSettings::default()
        };
        field.set_impostors(Some(settings));
        field.settle_streaming([0.0, 50.0, 0.0]);
        // The crown nearest the middle of the map, seen from `DISTANCE` along +Z at its centre's height.
        let centre = field
            .instance_spheres()
            .into_iter()
            .map(|(centre, _)| Vec3::from_array(centre))
            .min_by(|a, b| a.length_squared().total_cmp(&b.length_squared()))
            .expect("a crown");
        let eye = centre + Vec3::Z * DISTANCE;
        field.settle_streaming(eye.to_array());
        let mut atlas = field.bake_impostors(Some(settings));
        for proto in &mut atlas.protos {
            for channel in &mut proto.tint[..3] {
                *channel *= impostor_shade;
            }
        }
        let mut layer = ImpostorLayer::new(&device.device, &device.queue, &atlas);
        layer.set_enabled(matches!(sides, Sides::Both | Sides::ImpostorOnly));
        let mut draw =
            TerrainSharedDraw::new(&device, map, &layers, CdlodConfig::default(), 5).expect("terrain");
        draw.set_impostors(Some(&layer));
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.0,
            bloom: false,
            taa,
            ..PostSettings::golden_deterministic()
        };
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.render_scale_permille = 1000;
        quality.gpu_driven = false;
        quality.screen_space_gi = false;
        quality.ssr = false;
        quality.volumetric_fog = false;
        quality.volumetric_clouds = false;
        quality.gi_probes = false;
        quality.taau = taa;
        quality.smaa = false;
        Self {
            device,
            renderer,
            field,
            layer,
            post,
            quality,
            sides,
            eye,
        }
    }

    /// One frame with the eye trucked `truck` metres along +X.
    fn frame(&mut self, truck: f32) -> Vec<u8> {
        let eye = (self.eye + Vec3::X * truck).to_array();
        let target = (self.eye + Vec3::X * truck - Vec3::Z).to_array();
        self.field.update(eye);
        self.layer.update(&self.device.queue, &self.field);
        let lab = LabScene {
            width: W,
            height: H,
            eye,
            target,
            fov_y: FOV,
            near: 0.5,
            far: 3000.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 14.0,
            ambient_lux: 9_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.2, -0.6, -0.75], [1.0, 1.0, 1.0], 60_000.0),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let mut scene = lab.as_frame_scene_with_quality(self.quality);
        scene.post = Some(&self.post);
        let view = ScatterView::look_at(eye, target, FOV, W as f32 / H as f32, 0.5, 3000.0)
            .with_sun([-0.2, 0.6, 0.75]);
        let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
        if matches!(self.sides, Sides::Both | Sides::MeshOnly) {
            self.field.collect(&view, &mut meshes);
        }
        scene.meshes.extend(meshes);
        self.renderer
            .render_offscreen(&self.device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels
    }

    /// The last frame of a truck at `px_per_frame`, resolved over [`FRAMES`] frames.
    fn truck(&mut self, px_per_frame: f32) -> Vec<u8> {
        let metres_per_px = 2.0 * DISTANCE * (FOV * 0.5).tan() / H as f32;
        let mut last = Vec::new();
        for frame in 0..FRAMES {
            last = self.frame(frame as f32 * px_per_frame * metres_per_px);
        }
        last
    }
}

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn square() -> impl Iterator<Item = (usize, usize)> {
    let (x0, y0) = ((W as usize - SIDE) / 2, (H as usize - SIDE) / 2);
    (y0..y0 + SIDE).flat_map(move |y| (x0..x0 + SIDE).map(move |x| (x, y)))
}

fn measured_luma(pixels: &[u8]) -> Vec<f32> {
    let lin = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    square()
        .map(|(x, y)| {
            let i = (y * W as usize + x) * 4;
            0.2126 * lin(pixels[i]) + 0.7152 * lin(pixels[i + 1]) + 0.0722 * lin(pixels[i + 2])
        })
        .collect()
}

/// Whether `pixels` differs from the frame without that side at `(x, y)`.
fn drawn(pixels: &[u8], without: &[u8], x: usize, y: usize) -> bool {
    let i = (y * W as usize + x) * 4;
    (0..3).any(|k| pixels[i + k].abs_diff(without[i + k]) > 4)
}

/// The instance's share of the impostor: the band both sides read.
fn band(eye: Vec3, centre: Vec3) -> f32 {
    ((eye.distance(centre) - FADE_START) / (FADE_END - FADE_START)).clamp(0.0, 1.0)
}

/// The square's pixels the impostor drew although `threshold` discards them, and those it left although
/// `threshold` keeps them, leaving out pixels within 1/512 of the band (float rounding, not the phase). The
/// first count is the phase test: the right phase never draws a discarded pixel. The second also holds the
/// card's silhouette gaps (a pixel the capture did not cover draws on neither threshold), so it is
/// reported, not asserted.
fn phase_misses(
    with: &[u8],
    without: &[u8],
    band: f32,
    threshold: impl Fn(usize, usize) -> f32,
) -> (usize, usize) {
    let (mut drawn_discarded, mut kept_missing) = (0, 0);
    for (x, y) in square() {
        let t = threshold(x, y);
        if (t - band).abs() <= 1.0 / 512.0 {
            continue;
        }
        match (t < band, drawn(with, without, x, y)) {
            (false, true) => drawn_discarded += 1,
            (true, false) => kept_missing += 1,
            _ => {}
        }
    }
    (drawn_discarded, kept_missing)
}

/// How many pixels of the square the impostor drew.
fn drawn_count(with: &[u8], without: &[u8]) -> usize {
    square().filter(|&(x, y)| drawn(with, without, x, y)).count()
}

#[test]
fn the_mirrored_hierarchy_phase_is_the_forward_passes() {
    const FORWARD: &str = include_str!("../../regia_render/shaders/lab_forward.wgsl");
    const IMPOSTOR: &str = include_str!("../src/gpu/impostor.wgsl");
    let function = |source: &str, name: &str| {
        let start = source
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("fn {name} missing"));
        let end = source[start..].find("\n}\n").map_or(source.len(), |e| start + e);
        source[start..end]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    for name in ["alpha_hash_pcg", "alpha_hash_phase"] {
        assert_eq!(
            function(FORWARD, name),
            function(IMPOSTOR, name),
            "impostor.wgsl's copy of {name} drifted from lab_forward.wgsl: the two sides of the \
             hand-over would no longer draw the same threshold"
        );
    }
}

/// Without temporal AA the noise frame stays 0, so the impostor's threshold is the phase itself: IGN on
/// a repeated view, the hierarchy on a moved one. The mesh side is pinned by `regia_render`'s
/// `lod_dither_stipple`; here the impostor keeps the complement of the same phase. The first frame is a
/// warm-up: on a fresh renderer it is not the steady frame (1219 mismatches on the base as at the merged
/// head), so the comparisons start after it.
#[test]
fn the_impostor_threshold_is_the_cpu_phase_held_and_moving() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut with = Rig::new(Sides::ImpostorOnly, false, 1.0);
    let mut without = Rig::new(Sides::Neither, false, 1.0);
    let trucks = [0.0, 0.0, 0.4, 0.4];
    let a: Vec<Vec<u8>> = trucks.iter().map(|&t| with.frame(t)).collect();
    let b: Vec<Vec<u8>> = trucks.iter().map(|&t| without.frame(t)).collect();
    let centre = with
        .field
        .instance_spheres()
        .into_iter()
        .map(|(centre, _)| Vec3::from_array(centre))
        .min_by(|p, q| {
            p.distance_squared(with.eye)
                .total_cmp(&q.distance_squared(with.eye))
        })
        .expect("a crown");
    let band_at = |t: f32| band(with.eye + Vec3::X * t, centre);
    let ign = |x: usize, y: usize| hashed_threshold([x as f32 + 0.5, y as f32 + 0.5], 0.0);
    let hierarchy = |x: usize, y: usize| hierarchy_phase([x as u32, y as u32]);
    let held_first = phase_misses(&a[1], &b[1], band_at(0.0), ign);
    let held = phase_misses(&a[3], &b[3], band_at(0.4), ign);
    let moving = phase_misses(&a[2], &b[2], band_at(0.4), hierarchy);
    let moving_ign = phase_misses(&a[2], &b[2], band_at(0.4), ign);
    let moving_drawn = drawn_count(&a[2], &b[2]);
    eprintln!(
        "band {:.3}/{:.3}; (drawn though discarded, kept but missing): repeated first view {held_first:?}, \
         held {held:?}, moving on the hierarchy {moving:?}, moving read as IGN {moving_ign:?}; \
         moving drawn {moving_drawn} of {}",
        band_at(0.0),
        band_at(0.4),
        SIDE * SIDE
    );
    assert_eq!(
        held_first.0, 0,
        "a repeated first view must keep interleaved gradient noise"
    );
    assert_eq!(held.0, 0, "a held view must keep interleaved gradient noise");
    assert_eq!(moving.0, 0, "a moving view must take the hierarchy phase");
    assert!(
        moving_drawn > SIDE * SIDE / 4,
        "the impostor draws a share of the square near the band: {moving_drawn}"
    );
    assert!(
        moving_ign.0 > moving_drawn / 4,
        "a moving view still draws the interleaved gradient threshold: {} of {moving_drawn}",
        moving_ign.0
    );
}

/// The mesh and its impostor never both draw a pixel, on a moving view as on a held one: they take one
/// threshold. A pixel neither draws is where the mesh's silhouette and its card's differ (the card is a
/// baked capture), whichever side the threshold gives it to, so those are reported, not asserted. The first frame is a warm-up (`lod_dither_stipple`,
/// `STEPS`).
#[test]
fn mesh_and_impostor_never_both_draw_a_pixel_on_a_moving_view() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let trucks = [0.0, 0.0, 0.4, 0.4];
    let run = |sides: Sides| -> Vec<Vec<u8>> {
        let mut rig = Rig::new(sides, false, 1.0);
        trucks.iter().map(|&t| rig.frame(t)).collect()
    };
    let mesh = run(Sides::MeshOnly);
    let impostor = run(Sides::ImpostorOnly);
    let neither = run(Sides::Neither);
    for frame in 1..trucks.len() {
        let (mut twice, mut none, mut by_mesh, mut by_impostor) = (0, 0, 0, 0);
        for (x, y) in square() {
            let m = drawn(&mesh[frame], &neither[frame], x, y);
            let i = drawn(&impostor[frame], &neither[frame], x, y);
            twice += usize::from(m && i);
            none += usize::from(!m && !i);
            by_mesh += usize::from(m);
            by_impostor += usize::from(i);
        }
        eprintln!(
            "frame {frame}: mesh {by_mesh}, impostor {by_impostor}, both {twice}, neither {none} of {}",
            SIDE * SIDE
        );
        assert!(
            by_mesh > SIDE * SIDE / 8 && by_impostor > SIDE * SIDE / 8,
            "frame {frame}: both sides draw part of the crown (mesh {by_mesh}, impostor {by_impostor})"
        );
        assert_eq!(twice, 0, "frame {frame}: {twice} pixels drawn by both sides");
    }
}

#[test]
fn a_moving_hand_over_resolves_without_a_lattice_stipple() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pixels = Rig::new(Sides::Both, true, IMPOSTOR_SHADE).truck(0.37);
    let luma = measured_luma(&pixels);
    let energy = stipple_energy(&luma, SIDE, SIDE);
    let mean = luma.iter().sum::<f32>() / luma.len() as f32;
    eprintln!("hand-over band 0.5, 0.37 px/frame: {energy:?}; mean luma {mean:.4}");
    assert!(mean > 0.01, "the crown must be lit ({mean})");
    assert!(
        energy.lattice < 0.08,
        "the resolved band keeps {:.1} % of its fine-scale power on the IGN lattice: {energy:?}",
        energy.lattice * 100.0
    );
    assert!(
        energy.peak_ratio < 80.0,
        "one frequency dominates the resolved band (a lattice): {energy:?}"
    );
}

/// A held view keeps interleaved gradient noise on both sides: its converged residual stays on the IGN
/// lattice, the evidence that the held flag reached the impostor and the mesh alike.
#[test]
fn a_held_hand_over_keeps_the_interleaved_gradient_phase() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let held = measured_luma(&Rig::new(Sides::Both, true, IMPOSTOR_SHADE).truck(0.0));
    let moving = measured_luma(&Rig::new(Sides::Both, true, IMPOSTOR_SHADE).truck(0.37));
    let (held, moving) = (
        stipple_energy(&held, SIDE, SIDE),
        stipple_energy(&moving, SIDE, SIDE),
    );
    eprintln!("hand-over held: {held:?}; moving: {moving:?}");
    assert!(
        held.peak_ratio > 150.0 && held.peak_ratio > 3.0 * moving.peak_ratio,
        "a held band should keep the IGN phase (its lattice peak): held {held:?}, moving {moving:?}"
    );
}
