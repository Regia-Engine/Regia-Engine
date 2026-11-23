// SPDX-License-Identifier: MPL-2.0
//! Octahedral impostors for far scatter on the runtime frame path (lavapipe): a scatter field
//! with the impostor far field on, an [`ImpostorLayer`] bound to the shared terrain draw, the
//! whole frame rendered by `FrameRenderer` (depth prepass, cascades, SSAO, colour, post).
//!
//! - Off is free: a bound layer with the field's impostors off renders the same bytes as a
//!   draw that never heard of impostors.
//! - On adds the far field: rocks past the class's fade end — where nothing drew before —
//!   cover their projected positions.
//! - The hand-over does not pop: the same rocks drawn as meshes and as impostors cover the
//!   same pixels and print at the same brightness (tolerances below).

use regia_core::glam::{Vec3, Vec4};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use std::sync::Arc;

use regia_terrain::{
    CdlodConfig, ImpostorLayer, ImpostorSettings, RockStyle, ScatterField, ScatterLibrary, ScatterSet,
    ScatterSource, ScatterView, TerrainLayers, TerrainMap, TerrainSharedDraw, procedural_rock,
};

const W: u32 = 320;
const H: u32 = 180;
const FOV: f32 = 0.9;

fn set(fade_start: f32, fade_end: f32, shadows: bool) -> String {
    format!(
        r#"
ScatterSet(
    version: 2,
    seed: 31,
    classes: [
        ScatterClass(
            name: "boulders",
            meshes: [
                ScatterMeshRef(mesh: "rock_a", weight: 1.0, min_scale: 2.0, max_scale: 5.0, scale_bias: 1.5),
                ScatterMeshRef(mesh: "rock_b", weight: 1.0, min_scale: 2.0, max_scale: 5.0, scale_bias: 1.5),
            ],
            cell_size: 10.0,
            density: 0.5,
            slope_max: 0.8,
            contact_ao: 0.0,
            cast_shadows: {shadows},
            fade_start: {fade_start:.1},
            fade_end: {fade_end:.1},
        ),
    ],
)
"#
    )
}

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [
            LayerDesc(name: "dust", albedo: (0.62, 0.50, 0.38), roughness: 0.92, tiling: 2.0,
                slope_min: 0.0, slope_max: 1.6, altitude_min: -1000.0, altitude_max: 5000.0,
                blend_sharpness: 0.15, triplanar: false),
        ])"#,
    )
    .expect("layers")
}

/// 1 km of ground rising away from the camera (toward −Z), so far rocks face it.
fn map() -> TerrainMap {
    let n = 257u32;
    let world = 1024.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let z = (row as f32 / (n - 1) as f32 - 0.5) * world;
            let x = (col as f32 / (n - 1) as f32 - 0.5) * world;
            heights.push(-0.1 * z + 2.0 * (x * 0.02).sin() * (z * 0.017).cos());
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).expect("map")
}

fn settings() -> ImpostorSettings {
    ImpostorSettings {
        grid: 8,
        cell: 32,
        ..ImpostorSettings::default()
    }
}

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    field: ScatterField,
    layer: Option<ImpostorLayer>,
    map: TerrainMap,
    post: PostSettings,
    /// Camera (eye and target) shift along −Z, metres.
    shift: f32,
}

impl Rig {
    /// `bind`: bind an impostor layer to the terrain draw; `impostors`: the field's far field.
    fn new(device: RenderDevice, set_ron: &str, bind: bool, impostors: bool) -> Self {
        Self::with(device, set_ron, bind, impostors.then(settings))
    }

    fn with(device: RenderDevice, set_ron: &str, bind: bool, impostors: Option<ImpostorSettings>) -> Self {
        let map = map();
        let layers = layers();
        let set = ScatterSet::from_ron(set_ron, Some(&layers)).expect("scatter set");
        let mut library = ScatterLibrary::new();
        let colour = [0.30, 0.24, 0.20];
        library.insert("rock_a".into(), procedural_rock(1, RockStyle::angular(colour)));
        library.insert("rock_b".into(), procedural_rock(2, RockStyle::rounded(colour)));
        let source = ScatterSource {
            map: Arc::new(map.clone()),
            holes: None,
            layers: Some(layers.clone()),
            density_maps: regia_terrain::DensityMaps::new(),
        };
        let mut field = ScatterField::new(&set, source, &library, 0xB7).expect("field");
        field.set_background_streaming(false);
        if impostors.is_some() {
            field.set_impostors(impostors);
        }
        let mut draw = TerrainSharedDraw::new(&device, map.clone(), &layers, CdlodConfig::default(), 5)
            .expect("terrain");
        let layer = bind.then(|| {
            let atlas = field.bake_impostors(Some(impostors.unwrap_or_else(settings)));
            ImpostorLayer::new(&device.device, &device.queue, &atlas)
        });
        draw.set_impostors(layer.as_ref());
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.0,
            bloom: false,
            taa: false,
            ..PostSettings::default()
        };
        Self {
            device,
            renderer,
            field,
            layer,
            map,
            post,
            shift: 0.0,
        }
    }

    fn camera(&self) -> ([f32; 3], [f32; 3]) {
        let (z, tz) = (420.0 - self.shift, -200.0 - self.shift);
        let ground = self.map.sample_bilinear(0.0, z);
        let target_ground = self.map.sample_bilinear(0.0, tz);
        ([0.0, ground + 14.0, z], [0.0, target_ground, tz])
    }

    fn frame(&mut self, scatter: bool) -> Vec<u8> {
        let (eye, target) = self.camera();
        self.field.set_enabled(scatter);
        self.field.update(eye);
        if let Some(layer) = self.layer.as_mut() {
            layer.update(&self.device.queue, &self.field);
        }
        let lab = LabScene {
            width: W,
            height: H,
            eye,
            target,
            fov_y: FOV,
            near: 0.1,
            far: 3000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 9_000.0,
            double_sided: false,
            post: Some(self.post.clone()),
            directional: DirectionalLightUniform::new([0.45, -0.6, -0.65], [1.0, 0.95, 0.88], 90_000.0),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.taau = false;
        quality.render_scale_permille = 1000;
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&self.post);
        let view = ScatterView::look_at(eye, target, FOV, W as f32 / H as f32, 0.1, 3000.0)
            .with_sun([-0.45, 0.6, 0.65]);
        let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
        self.field.collect(&view, &mut meshes);
        scene.meshes.extend(meshes);
        self.renderer
            .render_offscreen(&self.device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels
    }

    /// Screen pixel of each resident instance centre farther than `min_d` from the eye.
    fn projected(&self, min_d: f32, max_d: f32) -> Vec<(u32, u32)> {
        let (eye, target) = self.camera();
        let view = regia_core::glam::camera::rh::view::look_at_mat4(
            Vec3::from_array(eye),
            Vec3::from_array(target),
            Vec3::Y,
        );
        let proj =
            regia_core::glam::camera::rh::proj::directx::perspective(FOV, W as f32 / H as f32, 0.1, 3000.0);
        let vp = proj * view;
        let mut out = Vec::new();
        for (centre, _) in self.field.instance_spheres() {
            let d = Vec3::from_array(centre).distance(Vec3::from_array(eye));
            if d < min_d || d > max_d {
                continue;
            }
            let c = vp * Vec4::new(centre[0], centre[1], centre[2], 1.0);
            if c.w <= 0.0 {
                continue;
            }
            let (x, y) = (c.x / c.w, c.y / c.w);
            if x.abs() > 0.95 || y.abs() > 0.95 {
                continue;
            }
            out.push((
                ((x * 0.5 + 0.5) * W as f32) as u32,
                ((0.5 - y * 0.5) * H as f32) as u32,
            ));
        }
        out
    }
}

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"))
}

fn luma(p: &[u8]) -> f32 {
    0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
}

fn differs(a: &[u8], b: &[u8], i: usize) -> bool {
    let d: i32 = (0..3)
        .map(|k| (i32::from(a[i * 4 + k]) - i32::from(b[i * 4 + k])).abs())
        .sum();
    d > 18
}

fn write_evidence(name: &str, pixels: &[u8]) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE") else {
        return;
    };
    let dir = std::path::Path::new(&dir).join("scatter_impostors");
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::File::create(dir.join(name)) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_bound_layer_with_impostors_off_renders_the_same_bytes() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ron = set(100.0, 140.0, true);
    let a = Rig::new(device(), &ron, false, false).frame(true);
    let b = Rig::new(device(), &ron, false, false).frame(true);
    assert!(a == b, "control: two identical rigs must render the same bytes");
    let mut bound = Rig::new(device(), &ron, true, false);
    let c = bound.frame(true);
    assert_eq!(bound.layer.as_ref().map(ImpostorLayer::count), Some(0));
    assert!(
        a == c,
        "a bound impostor layer with the far field off changed the frame"
    );
    // A disabled layer with the far field on uploads instances but records no draw: the
    // meshes are then the only difference from the plain frame (dithered across the band,
    // never shrunk), so it differs only inside the band's rows.
    let mut disabled = Rig::new(device(), &ron, true, true);
    if let Some(layer) = disabled.layer.as_mut() {
        layer.set_enabled(false);
    }
    let d = disabled.frame(true);
    assert!(disabled.layer.as_ref().is_some_and(|l| l.count() > 0));
    let changed = (0..(W * H) as usize).filter(|&i| differs(&a, &d, i)).count();
    assert!(
        changed < (W * H) as usize / 20,
        "{changed} pixels changed with the layer disabled"
    );
}

#[test]
fn far_rocks_draw_as_impostors_past_the_fade_end() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ron = set(100.0, 140.0, true);
    let mut plain = Rig::new(device(), &ron, false, false);
    let bare = plain.frame(false);
    let meshes_only = plain.frame(true);
    let mut far = Rig::new(device(), &ron, true, true);
    let with_impostors = far.frame(true);
    write_evidence("bare.png", &bare);
    write_evidence("meshes_only.png", &meshes_only);
    write_evidence("impostors.png", &with_impostors);
    let resident = far.layer.as_ref().map_or(0, ImpostorLayer::count);
    assert!(resident > 2000, "only {resident} impostor instances resident");
    // Rock centres well past the fade end (the near ones still draw as meshes in both) and
    // near enough to stay above the shrink-out size at 180 px (a 2–5 m rock is a few pixels).
    let points = far.projected(170.0, 280.0);
    assert!(
        points.len() > 100,
        "only {} far rock centers on screen",
        points.len()
    );
    let hit = |img: &[u8]| {
        points
            .iter()
            .filter(|&&(x, y)| {
                // The centre or a neighbour (a far rock is a few pixels).
                [(0i32, 0i32), (1, 0), (-1, 0), (0, 1), (0, -1)]
                    .iter()
                    .any(|(dx, dy)| {
                        let (px, py) = (x as i32 + dx, y as i32 + dy);
                        px >= 0
                            && py >= 0
                            && (px as u32) < W
                            && (py as u32) < H
                            && differs(img, &bare, (py as u32 * W + px as u32) as usize)
                    })
            })
            .count() as f32
            / points.len() as f32
    };
    let before = hit(&meshes_only);
    let after = hit(&with_impostors);
    println!("far rock centers covered: meshes only {before:.2}, with impostors {after:.2}");
    assert!(
        before < 0.1,
        "meshes already covered {before} of the far rocks (fade end ignored?)"
    );
    assert!(after > 0.5, "impostors covered only {after} of the far rocks");
    // Far rocks are lit stone: not black, not blown out, with some shading spread.
    let lum: Vec<f32> = (0..(W * H) as usize)
        .filter(|&i| differs(&with_impostors, &meshes_only, i))
        .map(|i| luma(&with_impostors[i * 4..i * 4 + 4]))
        .collect();
    assert!(lum.len() > 300, "only {} impostor pixels", lum.len());
    let mean = lum.iter().sum::<f32>() / lum.len() as f32;
    assert!(mean > 15.0 && mean < 235.0, "impostor mean luma {mean}");
}

#[test]
fn the_hand_over_keeps_coverage_and_brightness() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The same placement (fade ranges do not move rocks) drawn all as meshes and all as
    // impostors, without cast shadows so only the rocks themselves differ from bare ground.
    let device = device();
    let mut meshes = Rig::new(device.clone(), &set(5000.0, 5100.0, false), true, true);
    let mut cards = Rig::new(device, &set(1.0, 2.0, false), true, true);
    let bare = meshes.frame(false);
    let as_meshes = meshes.frame(true);
    let as_cards = cards.frame(true);
    write_evidence("handover_meshes.png", &as_meshes);
    write_evidence("handover_impostors.png", &as_cards);
    // Compare the rows below the horizon band (y ≥ 0.55 H, rocks within ~250 m): above it
    // most rocks are under 2 × min_pixels at 180 px, where the card shrinks out by design and
    // the mesh rasterises to isolated dark pixels.
    let first = (W * (H * 55 / 100)) as usize;
    let mask = |img: &[u8]| -> Vec<usize> {
        (first..(W * H) as usize)
            .filter(|&i| differs(img, &bare, i))
            .collect()
    };
    let (m, c) = (mask(&as_meshes), mask(&as_cards));
    let mean = |img: &[u8], px: &[usize]| {
        px.iter().map(|&i| luma(&img[i * 4..i * 4 + 4])).sum::<f32>() / px.len().max(1) as f32
    };
    let (lm, lc) = (mean(&as_meshes, &m), mean(&as_cards, &c));
    let both = m.iter().filter(|i| c.binary_search(i).is_ok()).count();
    println!(
        "hand-over: mesh pixels {} impostor pixels {} overlap {both}; mean luma mesh {lm:.1} impostor {lc:.1}",
        m.len(),
        c.len()
    );
    assert!(m.len() > 500, "only {} mesh rock pixels", m.len());
    let ratio = c.len() as f32 / m.len() as f32;
    assert!(
        (0.9..1.1).contains(&ratio),
        "impostor coverage {ratio:.2}× the mesh's"
    );
    assert!(
        both as f32 > 0.85 * m.len() as f32,
        "impostors cover {both} of {} mesh pixels",
        m.len()
    );
    assert!(
        (lc / lm - 1.0).abs() < 0.06,
        "impostor brightness {lc:.1} vs mesh {lm:.1}"
    );
}

#[test]
fn a_moving_camera_uploads_tiles_not_the_whole_field() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A 300 m far range on a 1 km map: walking 120 m streams tiles in and out.
    let near = ImpostorSettings {
        far_end_m: 300.0,
        ..settings()
    };
    let ron = set(100.0, 140.0, true);
    let device = device();
    let mut walk = Rig::with(device.clone(), &ron, true, Some(near));
    walk.frame(true);
    let first = walk.layer.as_ref().map_or(0, ImpostorLayer::rebuilds);
    walk.shift = 120.0;
    let moved = walk.frame(true);
    let layer = walk.layer.as_ref().expect("layer");
    let mut expected = Vec::new();
    walk.field.impostor_instances(&mut expected);
    assert_eq!(
        layer.count() as usize,
        expected.len(),
        "live slots = the field's instances"
    );
    assert_eq!(layer.rebuilds(), first, "a walk re-uploaded the whole field");
    let mut fresh = Rig::with(device, &ron, true, Some(near));
    fresh.shift = 120.0;
    let direct = fresh.frame(true);
    // Same instances, different slot order: only exact depth ties may resolve differently.
    let changed = (0..(W * H) as usize)
        .filter(|&i| differs(&moved, &direct, i))
        .count();
    println!("walked vs fresh: {changed} pixels differ");
    assert!(
        changed < (W * H) as usize / 200,
        "{changed} pixels differ from a fresh field"
    );
}
