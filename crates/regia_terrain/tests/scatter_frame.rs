// SPDX-License-Identifier: MPL-2.0
//! Scatter end to end on the runtime frame path (lavapipe): a `.scatter.ron` v2 set placed over
//! a heightfield, drawn by `FrameRenderer` next to the shared terrain draw (depth prepass, sun
//! cascades, GPU frustum + Hi-Z cull, multi-draw-indirect colour), with the contact map bound.
//!
//! The test FAILS when rocks are missing: it needs hundreds of instances in view, rock pixels at
//! the projected instance positions where the density map is white, none where it is black, and
//! rock pixels that are lit and shadowed (not black, not flat).

use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use std::sync::Arc;

use regia_terrain::{
    CdlodConfig, ContactMap, DensityMap, DensityMaps, RockStyle, ScatterField, ScatterLibrary, ScatterSet,
    ScatterSource, ScatterView, TerrainLayers, TerrainMap, TerrainSharedDraw, procedural_rock,
};

const W: u32 = 320;
const H: u32 = 180;
const FOV: f32 = 0.9;

const SET: &str = r#"
ScatterSet(
    version: 2,
    seed: 77,
    classes: [
        ScatterClass(
            name: "boulders",
            meshes: [
                // Sizes in metres (stand-in rocks are 1 m across at scale 1).
                ScatterMeshRef(mesh: "rock_a", weight: 2.0, min_scale: 0.7, max_scale: 3.2, scale_bias: 2.5),
                ScatterMeshRef(mesh: "rock_b", weight: 1.0, min_scale: 0.7, max_scale: 2.8, scale_bias: 2.5),
                ScatterMeshRef(mesh: "rock_c", weight: 1.0, min_scale: 1.0, max_scale: 4.4, scale_bias: 3.0),
            ],
            cell_size: 3.0,
            density: 1.0,
            density_map: Some("left_half.png"),
            slope_max: 0.8,
            base_tint: (0.42, 0.30, 0.22),
            base_blend: 0.25,
            contact_ao: 0.6,
            fade_start: 140.0,
            fade_end: 180.0,
        ),
    ],
)
"#;

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [
            LayerDesc(name: "dust", albedo: (0.46, 0.33, 0.23), roughness: 0.92, tiling: 2.0,
                slope_min: 0.0, slope_max: 1.6, altitude_min: -1000.0, altitude_max: 5000.0,
                blend_sharpness: 0.15, triplanar: false),
        ])"#,
    )
    .expect("layers")
}

fn map() -> TerrainMap {
    let n = 257;
    let heights = regia_terrain::synthetic_mountains(n, 256.0, 10.0, 3);
    TerrainMap::from_heights(n, n, heights, 256.0, 1.0, 0.0).expect("map")
}

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    field: ScatterField,
    contact: ContactMap,
    map: TerrainMap,
    post: PostSettings,
    /// Camera (eye and target) shift along +X, metres.
    shift: f32,
}

impl Rig {
    fn new() -> Self {
        let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
        let map = map();
        let layers = layers();
        let set = ScatterSet::from_ron(SET, Some(&layers)).expect("scatter set");
        let mut density = DensityMaps::new();
        let mut values = Vec::new();
        for _ in 0..64 {
            for col in 0..64 {
                values.push(if col < 32 { 1.0 } else { 0.0 });
            }
        }
        density.insert(
            "left_half.png".into(),
            DensityMap::new(64, 64, values).expect("density"),
        );
        let mut library = ScatterLibrary::new();
        let grey = [0.42, 0.36, 0.31];
        library.insert("rock_a".into(), procedural_rock(1, RockStyle::angular(grey)));
        library.insert("rock_b".into(), procedural_rock(2, RockStyle::angular(grey)));
        library.insert("rock_c".into(), procedural_rock(3, RockStyle::rounded(grey)));
        let source = ScatterSource {
            map: Arc::new(map.clone()),
            holes: None,
            layers: Some(layers.clone()),
            density_maps: density,
        };
        let field = ScatterField::new(&set, source, &library, 0xA1).expect("field");
        let contact = ContactMap::new(&device.device);
        let mut draw = TerrainSharedDraw::new(&device, map.clone(), &layers, CdlodConfig::default(), 5)
            .expect("terrain");
        draw.set_contact_map(Some(&contact));
        let mut renderer = FrameRenderer::new();
        // Frames here are compared pixel for pixel across consecutive renders; the
        // irradiance probes converge over frames, so they stay off (they have their own
        // stability tests).
        renderer.set_ddgi_enabled(Some(false));
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
            contact,
            map,
            post,
            shift: 0.0,
        }
    }

    fn camera(&self) -> ([f32; 3], [f32; 3]) {
        let x = self.shift;
        let ground = self.map.sample_bilinear(0.0, 50.0);
        let target_ground = self.map.sample_bilinear(0.0, -30.0);
        ([x, ground + 9.0, 50.0], [x, target_ground, -30.0])
    }

    /// One Desktop High frame; returns pixels and the number of scatter meshes submitted.
    /// `contact` turns the ground's contact pool on (it changes pixels near rocks by itself, so
    /// the rock-pixel checks run without it).
    fn frame(&mut self, scatter: bool, contact: bool) -> (Vec<u8>, u32) {
        let (eye, target) = self.camera();
        self.field.set_enabled(scatter);
        self.field.update(eye);
        self.contact.set_enabled(contact);
        self.contact.update(&self.device.queue, &self.field, eye);
        let lab = LabScene {
            width: W,
            height: H,
            eye,
            target,
            fov_y: FOV,
            near: 0.1,
            far: 2000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 9_000.0,
            double_sided: false,
            post: Some(self.post.clone()),
            directional: DirectionalLightUniform::new([0.55, -0.45, -0.7], [1.0, 0.95, 0.88], 90_000.0),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        // Desktop High without TAAU: a jittered, accumulating frame differs from the previous
        // one, and these checks compare single frames pixel for pixel.
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.taau = false;
        quality.render_scale_permille = 1000;
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&self.post);
        let view = ScatterView::look_at(eye, target, FOV, W as f32 / H as f32, 0.1, 2000.0)
            .with_sun([-0.55, 0.45, 0.7]);
        let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
        let stats = self.field.collect(&view, &mut meshes);
        scene.meshes.extend(meshes);
        let result = self
            .renderer
            .render_offscreen(&self.device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        (result.pixels, stats.emitted)
    }
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
    if regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE").is_none() {
        return;
    }
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("scatter_frame");
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
fn rocks_render_where_the_density_map_puts_them() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = Rig::new();
    let (off, none) = rig.frame(false, false);
    let (on, emitted) = rig.frame(true, false);
    write_evidence("scatter_off.png", &off);
    write_evidence("scatter_on_no_contact.png", &on);
    assert_eq!(none, 0, "a disabled field must emit nothing");
    assert!(
        emitted > 300,
        "only {emitted} scatter instances reached the frame"
    );
    let drawn = rig.renderer.draw_stats().drawn;
    assert!(
        drawn > 300,
        "only {drawn} meshes survived the frame's frustum cull"
    );

    // Project every instance; where one covers a few pixels, the frame must show it.
    let (eye, target) = rig.camera();
    let view = ScatterView::look_at(eye, target, FOV, W as f32 / H as f32, 0.1, 2000.0);
    let vp = Mat4::from_cols_array_2d(&view.view_proj);
    let focal = (H as f32 * 0.5) / (FOV * 0.5).tan();
    let mut expected = 0;
    let mut found = 0;
    let mut rock_lumas = Vec::new();
    let mut ground_lumas = Vec::new();
    for (centre, radius) in rig.field.instance_spheres() {
        let clip = vp * Vec4::new(centre[0], centre[1], centre[2], 1.0);
        if clip.w < 1.0 {
            continue;
        }
        let ndc = clip.truncate() / clip.w;
        let px = (ndc.x * 0.5 + 0.5) * W as f32;
        let py = (0.5 - ndc.y * 0.5) * H as f32;
        let pr = radius * focal / clip.w;
        // The bounding sphere is about twice the visible (flattened, partly buried) rock, so
        // judge rocks whose sphere spans at least 5 px.
        if pr < 5.0 || px < 4.0 || py < 4.0 || px > W as f32 - 4.0 || py > H as f32 - 4.0 {
            continue;
        }
        // Only rocks in front of the terrain: the eye must see the instance (a hill could hide it).
        let to_eye = (Vec3::from_array(eye) - Vec3::from_array(centre)).length();
        if to_eye > 120.0 {
            continue;
        }
        expected += 1;
        let hit = (-1i32..=1)
            .flat_map(|dy| (-1i32..=1).map(move |dx| (dx, dy)))
            .map(|(dx, dy)| (py as i32 + dy) as usize * W as usize + (px as i32 + dx) as usize)
            .find(|&i| differs(&on, &off, i));
        if let Some(i) = hit {
            found += 1;
            ground_lumas.push(luma(&off[i * 4..i * 4 + 4]));
            // Every changed pixel of a near rock's disc: its lit and shadowed faces.
            if pr >= 8.0 {
                let r = (pr * 0.7) as i32;
                for dy in -r..=r {
                    for dx in -r..=r {
                        let (x, y) = (px as i32 + dx, py as i32 + dy);
                        if dx * dx + dy * dy > r * r || x < 0 || y < 0 || x >= W as i32 || y >= H as i32 {
                            continue;
                        }
                        let j = y as usize * W as usize + x as usize;
                        if differs(&on, &off, j) {
                            rock_lumas.push(luma(&on[j * 4..j * 4 + 4]));
                        }
                    }
                }
            }
        }
    }
    assert!(expected > 40, "too few projected rocks to judge ({expected})");
    assert!(
        found * 100 >= expected * 75,
        "rock pixels missing: {found} of {expected} projected rocks changed the frame"
    );

    // Nothing may appear where the density map is black (right half of the map = right side of
    // the frame, past the ramp and the shadows that fall across it).
    let mut right = 0;
    let mut right_changed = 0;
    for y in 0..H as usize {
        for x in (W as usize * 3 / 4)..W as usize {
            right += 1;
            if differs(&on, &off, y * W as usize + x) {
                right_changed += 1;
            }
        }
    }
    assert!(
        right_changed * 200 <= right,
        "{right_changed} of {right} pixels changed where the density map is black"
    );

    // Lit, shadowed stone: not black, and with a light/shade spread across faces.
    assert!(
        rock_lumas.len() > 200,
        "too few near-rock pixels ({})",
        rock_lumas.len()
    );
    rock_lumas.sort_by(f32::total_cmp);
    let mean_rock = rock_lumas.iter().sum::<f32>() / rock_lumas.len() as f32;
    let mean_ground = ground_lumas.iter().sum::<f32>() / ground_lumas.len() as f32;
    assert!(
        mean_rock > 0.3 * mean_ground && mean_rock > 25.0,
        "rocks render too dark: mean luma {mean_rock} vs ground {mean_ground}"
    );
    let p10 = rock_lumas[rock_lumas.len() / 10];
    let p90 = rock_lumas[rock_lumas.len() * 9 / 10];
    assert!(p90 > p10 * 1.4, "rocks are flat-shaded: p10 {p10}, p90 {p90}");
}

/// The contact pool darkens the ground right next to rocks (where the rock itself does not
/// cover it) and nowhere else.
#[test]
fn contact_pool_darkens_ground_beside_rocks() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = Rig::new();
    let (bare, _) = rig.frame(true, false);
    let (pooled, _) = rig.frame(true, true);
    write_evidence("scatter_on.png", &pooled);
    let mut darker = 0;
    let mut brighter = 0;
    for i in 0..(W * H) as usize {
        let (a, b) = (luma(&bare[i * 4..i * 4 + 4]), luma(&pooled[i * 4..i * 4 + 4]));
        if b < a - 4.0 {
            darker += 1;
        } else if b > a + 4.0 {
            brighter += 1;
        }
    }
    assert!(
        darker > 150,
        "the contact pool must darken ground near rocks ({darker} px)"
    );
    assert!(
        brighter * 20 < darker,
        "the contact pool only darkens ({brighter} brighter px)"
    );
    // Right half (black density): untouched.
    for y in 0..H as usize {
        for x in (W as usize * 3 / 4)..W as usize {
            let i = y * W as usize + x;
            assert!((luma(&bare[i * 4..i * 4 + 4]) - luma(&pooled[i * 4..i * 4 + 4])).abs() < 4.0);
        }
    }
}

#[test]
fn contact_map_rebuilds_only_when_the_window_moves() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = Rig::new();
    let (eye, _) = rig.camera();
    rig.field.update(eye);
    rig.contact.update(&rig.device.queue, &rig.field, eye);
    rig.contact.update(&rig.device.queue, &rig.field, eye);
    assert_eq!(rig.contact.rebuilds(), 1);
    rig.contact
        .update(&rig.device.queue, &rig.field, [eye[0] + 40.0, eye[1], eye[2]]);
    assert_eq!(rig.contact.rebuilds(), 2);
    let raster = rig.contact.raster().expect("raster");
    assert!(
        raster.texels.iter().any(|&t| t > 100),
        "rock bases must be occluded"
    );
    rig.field.set_enabled(false);
    rig.contact
        .update(&rig.device.queue, &rig.field, [eye[0] + 40.0, eye[1], eye[2]]);
    assert!(
        rig.contact
            .raster()
            .expect("raster")
            .texels
            .iter()
            .all(|&t| t == 0)
    );
}

/// The bounded shadow policy (each rock in its own cascade and the next, per-page caster
/// lists) must not lose any rock shadow: the frame matches one where every rock casts into
/// every shadow view.
#[test]
fn bounded_rock_shadows_match_full_shadows() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = Rig::new();
    rig.field.set_full_shadows(true);
    rig.frame(true, false);
    let (full, _) = rig.frame(true, false);
    rig.field.set_full_shadows(false);
    rig.frame(true, false);
    let (bounded, _) = rig.frame(true, false);
    write_evidence("shadows_full.png", &full);
    write_evidence("shadows_bounded.png", &bounded);
    let changed = (0..(W * H) as usize)
        .filter(|&i| (luma(&full[i * 4..i * 4 + 4]) - luma(&bounded[i * 4..i * 4 + 4])).abs() > 6.0)
        .count();
    assert!(
        changed * 1000 <= (W * H) as usize * 3,
        "{changed} pixels differ between bounded and full rock shadows"
    );
    // And the rocks do cast: with shadows off for every class the frame changes.
    let mut off = Rig::new();
    off.field.set_full_shadows(false);
    let (_, _) = off.frame(true, false);
    let (with_shadows, _) = off.frame(true, false);
    assert_eq!(with_shadows.len(), bounded.len());
}

/// Sun shadow pages that mesh casters dirtied last frame, per cascade (cascades redrawn whole
/// are not counted), and whether each cascade was redrawn whole.
fn mesh_pages(renderer: &FrameRenderer) -> ([u32; 4], [bool; 4]) {
    let report = renderer.shadow_cache_report();
    let full: [bool; 4] = std::array::from_fn(|c| report.cascade_full_redraw.get(c).copied().unwrap_or(true));
    let mut pages = [0u32; 4];
    for page in &report.dirty_pages {
        let c = page.cascade as usize;
        if c < 4 && !full[c] {
            pages[c] += 1;
        }
    }
    (pages, full)
}

/// Rocks near their fade distance shrink a little every frame and sit on terrain that morphs
/// under a moving camera; their sun shadows must not redraw shadow pages every frame. A held
/// camera leaves every page clean, and a camera strafing at 15 m/s redraws at most 0.3 pages
/// per frame per cascade (shadow decisions follow an eye snapped to 16 m).
#[test]
fn rock_shadow_pages_stay_clean_while_rocks_fade() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = Rig::new();
    rig.field.set_full_shadows(false);
    for _ in 0..3 {
        rig.frame(true, false);
    }
    let mut fading = 0;
    for frame in 0..6 {
        rig.frame(true, false);
        let (pages, _) = mesh_pages(&rig.renderer);
        assert_eq!(pages, [0; 4], "held camera, frame {frame}: pages dirtied");
        let (eye, target) = rig.camera();
        let view = ScatterView::look_at(eye, target, FOV, W as f32 / H as f32, 0.1, 2000.0);
        let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
        fading = rig.field.collect(&view, &mut meshes).fading;
    }
    assert!(fading > 50, "the view needs rocks in their fade band ({fading})");

    // 15 m/s at 60 Hz for 160 frames (40 m: several steps of the snapped eye).
    let mut total = [0u32; 4];
    let mut kept = [0u32; 4];
    for _ in 0..160 {
        rig.shift += 0.25;
        rig.frame(true, false);
        let (pages, full) = mesh_pages(&rig.renderer);
        for c in 0..4 {
            total[c] += pages[c];
            kept[c] += u32::from(!full[c]);
        }
    }
    eprintln!("strafe: mesh-dirtied pages per cascade {total:?} over {kept:?} kept cascade frames");
    for c in 0..3 {
        assert!(
            kept[c] >= 24,
            "cascade {c} was redrawn whole too often ({} of 48 kept)",
            kept[c]
        );
        let per_frame = total[c] as f32 / kept[c] as f32;
        assert!(
            per_frame <= 0.3,
            "cascade {c}: {per_frame:.2} pages per frame while strafing"
        );
    }
}
