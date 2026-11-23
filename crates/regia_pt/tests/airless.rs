// SPDX-License-Identifier: MPL-2.0
//! The reference trace of an airless map (DO27's Bore Twelve on Europa, 2026-10-08 report:
//! `seeing.reference_trace` rendered black there and traced terrain only).
//!
//! A Bore-Twelve-like fixture: an icy heightfield drawn through the terrain's shared draw, a
//! building and a vehicle hull as meshes, an 18 klx sun at 24° and Jupiter 14° across with
//! DO27's 1600 lx planet-shine fill, black sky with stars. The raster frame runs with the
//! scene export on; the tracer is fed the export and the shared draw's heightfield
//! (`extract::from_export`). The trace must not be black, its luminance must sit in the raster
//! frame's range, and both the building and the hull must be in the traced image (mask test on
//! the albedo AOV). Set `REGIA_PT_EVIDENCE` to a directory to keep both frames as PNGs.

use std::path::PathBuf;

use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_pt::{Aov, PathTracer, PtCache, PtSettings};
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer,
    LabMaterial, LabMesh, LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, SharedPassDraw, SkyBody,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

const W: u32 = 240;
const H: u32 = 135;
const EV: f32 = 12.9;
const FOV_Y: f32 = 0.9;
const SUN_LUX: f32 = 18_000.0;
const FILL_LUX: f32 = 1_600.0;
const BUILDING: [f32; 3] = [0.55, 0.12, 0.08];
const HULL: [f32; 3] = [0.10, 0.18, 0.40];
const WORLD: f32 = 400.0;
const N: u32 = 129;

fn height(x: f32, z: f32) -> f32 {
    // Rolling ice with a ridge behind the base.
    2.5 * (x * 0.031).sin() * (z * 0.027).cos() + 9.0 * (-((z + 120.0) / 40.0).powi(2)).exp()
}

/// Toward a direction given as azimuth (degrees clockwise from north = +Z, east = +X) and
/// elevation.
fn toward(azimuth_deg: f32, elevation_deg: f32) -> Vec3 {
    let (az, el) = (azimuth_deg.to_radians(), elevation_deg.to_radians());
    Vec3::new(az.sin() * el.cos(), el.sin(), az.cos() * el.cos())
}

fn eye() -> Vec3 {
    Vec3::new(-6.0, height(-6.0, 30.0) + 5.0, 30.0)
}

fn look_at() -> Vec3 {
    // Toward azimuth 135° (south-east), a little down: Jupiter in view, the sun to the left.
    eye() + toward(135.0, -4.0) * 40.0
}

fn mesh_at(size: [f32; 3], at: [f32; 2], material: usize) -> LabMesh {
    // `box_mesh` takes half extents.
    let (verts, indices) = regia_render::box_mesh([size[0] * 0.5, size[1] * 0.5, size[2] * 0.5]);
    let base = height(at[0], at[1]) - 0.3;
    let vertices = verts
        .into_iter()
        .map(|mut v| {
            v.position = [
                v.position[0] + at[0],
                v.position[1] + base + size[1] * 0.5,
                v.position[2] + at[1],
            ];
            v
        })
        .collect();
    LabMesh {
        vertices,
        indices,
        material,
        ..LabMesh::unskinned()
    }
}

fn building_at() -> [f32; 2] {
    let p = eye() + toward(158.0, 0.0) * 34.0;
    [p.x, p.z]
}

fn hull_at() -> [f32; 2] {
    let p = eye() + toward(132.0, 0.0) * 18.0;
    [p.x, p.z]
}

fn terrain(device: &RenderDevice) -> TerrainSharedDraw {
    let heights: Vec<f32> = (0..N * N)
        .map(|i| {
            let x = ((i % N) as f32 / (N - 1) as f32 - 0.5) * WORLD;
            let z = ((i / N) as f32 / (N - 1) as f32 - 0.5) * WORLD;
            height(x, z)
        })
        .collect();
    let map = TerrainMap::from_heights(N, N, heights, WORLD, 60.0, -30.0).expect("map");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ice".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 0.8,
            tiling: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.15,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let ice = [
        regia_terrain::linear_to_srgb8(0.55),
        regia_terrain::linear_to_srgb8(0.58),
        regia_terrain::linear_to_srgb8(0.62),
    ];
    let mut draw = TerrainSharedDraw::new(device, map, &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [ice[0], ice[1], ice[2], 255]),
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
    draw.set_relief(TerrainRelief::off());
    draw
}

fn outdoor() -> OutdoorParams {
    let mut planet = AtmosphereParameters::from_preset(AtmospherePreset::Airless);
    planet.sun_illuminance = SUN_LUX;
    let jupiter = toward(110.0, 17.0);
    OutdoorParams {
        planet,
        sky_body: Some(SkyBody {
            direction: jupiter.to_array(),
            fill_lux: Some(FILL_LUX),
            ..SkyBody::jupiter_from_europa()
        }),
        ..OutdoorParams::default()
    }
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Pixel of a world point in the W×H frame seen with vertical field of view `fov_y`.
fn project(p: Vec3, fov_y: f32) -> Option<(f32, f32)> {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(eye(), look_at(), Vec3::Y);
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(fov_y, W as f32 / H as f32, 0.1, 5000.0);
    let clip: Vec4 = (proj * view as Mat4) * p.extend(1.0);
    (clip.w > 0.0).then(|| {
        let ndc = clip.truncate() / clip.w;
        ((ndc.x * 0.5 + 0.5) * W as f32, (0.5 - ndc.y * 0.5) * H as f32)
    })
}

/// Pixels whose first-hit albedo is `albedo` (± 0.04), and how many fall in the screen box of
/// `size` at `at`.
fn mask(
    img: &regia_pt::PtImage,
    fov_y: f32,
    albedo: [f32; 3],
    size: [f32; 3],
    at: [f32; 2],
) -> (usize, usize) {
    let base = height(at[0], at[1]) - 0.3;
    let (mut lo, mut hi) = ((f32::MAX, f32::MAX), (f32::MIN, f32::MIN));
    for k in 0..8 {
        let c = Vec3::new(
            at[0] + if k & 1 == 0 { -0.5 } else { 0.5 } * size[0],
            base + if k & 2 == 0 { 0.0 } else { 1.0 } * size[1],
            at[1] + if k & 4 == 0 { -0.5 } else { 0.5 } * size[2],
        );
        if let Some((x, y)) = project(c, fov_y) {
            lo = (lo.0.min(x), lo.1.min(y));
            hi = (hi.0.max(x), hi.1.max(y));
        }
    }
    let (mut all, mut inside) = (0, 0);
    for (i, a) in img.albedo.iter().enumerate() {
        if (0..3).all(|k| (a[k] - albedo[k]).abs() < 0.04) {
            all += 1;
            let (x, y) = ((i as u32 % W) as f32 + 0.5, (i as u32 / W) as f32 + 0.5);
            if x >= lo.0 - 1.0 && x <= hi.0 + 1.0 && y >= lo.1 - 1.0 && y <= hi.1 + 1.0 {
                inside += 1;
            }
        }
    }
    (all, inside)
}

fn evidence_dir() -> Option<PathBuf> {
    regia_core::brand::env_var_os("REGIA_PT_EVIDENCE").map(PathBuf::from)
}

/// The raster frame of the fixture at `w`×`h` with the scene export on: display pixels,
/// linear HDR, the export and the terrain's description.
fn raster_frame(
    device: &RenderDevice,
    w: u32,
    h: u32,
) -> (
    Vec<u8>,
    regia_render::HdrCapture,
    regia_render::FrameExport,
    Option<regia_render::ReferenceHeightfield>,
) {
    let sun_toward = toward(185.0, 24.0);
    let post = PostSettings {
        exposure_ev: EV,
        ..PostSettings::golden_deterministic()
    };
    let lab = LabScene {
        width: w,
        height: h,
        eye: eye().to_array(),
        target: look_at().to_array(),
        fov_y: FOV_Y,
        near: 0.1,
        far: 5000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new((-sun_toward).to_array(), [1.0, 0.96, 0.92], SUN_LUX),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [BUILDING[0], BUILDING[1], BUILDING[2], 1.0],
                roughness: 0.7,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [HULL[0], HULL[1], HULL[2], 1.0],
                metallic: 0.5,
                roughness: 0.4,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            mesh_at([12.0, 8.0, 10.0], building_at(), 0),
            mesh_at([4.0, 2.5, 9.0], hull_at(), 1),
        ],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(outdoor());
    renderer.set_shared_draw(Some(
        Box::new(terrain(device)) as Box<dyn SharedPassDraw + Send + Sync>
    ));
    renderer.set_scene_export(true);
    renderer.request_hdr_capture();
    let raster = renderer.render_offscreen(device, &frame).expect("raster frame");
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .expect("the frame ran the post stack");
    let export = renderer.scene_export().cloned().expect("scene export");
    (raster.pixels, hdr, export, renderer.reference_heightfield())
}

#[test]
fn airless_map_with_jupiter_traces_lit_terrain_and_heroes() {
    let device = RenderDevice::headless().expect("headless GPU (lavapipe?)");
    let (raster, hdr, export, terrain) = raster_frame(&device, W, H);
    assert_eq!(export.instances.len(), 2, "both heroes exported");
    assert!(export.fill_light.is_some(), "the frame added Jupiter's fill");
    let terrain = terrain.expect("the shared draw describes its terrain");
    assert_eq!((terrain.width, terrain.depth), (N, N));

    let scene = regia_pt::extract::from_export(&export, Some(&terrain));
    assert_eq!(scene.instances.len(), 2);
    assert!(scene.terrain.is_some());
    assert_eq!(scene.sky_bodies.len(), 1);
    let body = &scene.sky_bodies[0];
    let fill = body.normal_irradiance() * body.light_scale;
    assert!(
        (fill - FILL_LUX).abs() < FILL_LUX * 1e-2,
        "Jupiter lights with the frame's fill: {fill} lx"
    );
    assert!(scene.media.layers.is_empty(), "no air");
    assert!(scene.stars > 0.0, "airless sky shows stars");
    assert!(
        matches!(scene.sky, regia_pt::PtSky::Table { .. }),
        "black sky table"
    );
    let sun = scene.sun.expect("the frame's sun");
    assert!((sun.illuminance - SUN_LUX).abs() < 1.0);

    let tracer = PathTracer::new(&device).expect("tracer");
    let settings = PtSettings {
        spp: 48,
        max_bounces: 4,
        ..PtSettings::default()
    };
    let mut cache = PtCache::new();
    let mut session = tracer
        .begin_cached(&scene, &settings, &mut cache)
        .expect("upload");
    session.run(settings.spp).expect("trace");
    let img = session.image().expect("readback");
    assert_eq!(img.metadata.rejected_samples, 0);
    eprintln!("export fov_y {:.4} (lens {FOV_Y})", scene.camera.fov_y);

    if let Some(dir) = evidence_dir() {
        std::fs::create_dir_all(&dir).expect("evidence dir");
        regia_pt::output::write_png(&dir.join("airless_raster.png"), W, H, &raster).expect("png");
        img.write_preview_png(&dir.join("airless_pt.png"), Aov::Beauty, EV)
            .expect("png");
        // The same shot at 960×540 for judging by eye (`REGIA_PT_EVIDENCE_SPP`, default 256).
        let spp = regia_core::brand::env_var("REGIA_PT_EVIDENCE_SPP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);
        let (big_raster, _, big_export, big_terrain) = raster_frame(&device, 960, 540);
        let big = regia_pt::extract::from_export(&big_export, big_terrain.as_ref());
        let big_settings = PtSettings {
            spp,
            max_bounces: 8,
            ..PtSettings::default()
        };
        let big_img = tracer.render(&big, &big_settings).expect("evidence trace");
        regia_pt::output::write_png(&dir.join("airless_raster_960.png"), 960, 540, &big_raster).expect("png");
        big_img
            .write_preview_png(&dir.join("airless_pt_960.png"), Aov::Beauty, EV)
            .expect("png");
        eprintln!("evidence in {} ({spp} spp at 960x540)", dir.display());
    }
    // Luminance in the raster frame's range (scene-referred cd/m², the sun disc clamped out).
    let cap = 50_000.0;
    let mean = |v: &mut dyn Iterator<Item = [f32; 3]>| {
        let (mut s, mut n) = (0.0f64, 0usize);
        for c in v {
            s += f64::from(luminance(c).min(cap));
            n += 1;
        }
        (s / n.max(1) as f64) as f32
    };
    let pt_mean = mean(&mut img.beauty.iter().copied());
    let rt_mean = mean(&mut hdr.rgb.iter().copied());
    let ground: Vec<usize> = (0..img.beauty.len()).filter(|&i| img.depth[i] > 0.0).collect();
    let pt_ground = mean(&mut ground.iter().map(|&i| img.beauty[i]));
    let rt_ground = mean(&mut ground.iter().map(|&i| hdr.rgb[i]));
    let lit_share =
        ground.iter().filter(|&&i| img.shadow[i] > 0.5).count() as f32 / ground.len().max(1) as f32;
    eprintln!(
        "PT mean {pt_mean:.1} cd/m², raster {rt_mean:.1}; on surfaces PT {pt_ground:.1}, raster {rt_ground:.1}; \
         {} surface px, {:.0} % sunlit",
        ground.len(),
        lit_share * 100.0
    );
    assert!(pt_mean > 1.0, "the trace is black ({pt_mean} cd/m²)");
    assert!(
        ground.len() > (W * H / 3) as usize,
        "terrain and heroes fill the lower frame"
    );
    let ratio = pt_ground / rt_ground.max(1e-3);
    assert!(
        (0.5..=2.0).contains(&ratio),
        "PT/raster surface luminance {ratio:.2}"
    );

    // The heroes are in the traced frame, where the raster draws them.
    let fov = scene.camera.fov_y;
    let (b_all, b_in) = mask(&img, fov, BUILDING, [12.0, 8.0, 10.0], building_at());
    let (h_all, h_in) = mask(&img, fov, HULL, [4.0, 2.5, 9.0], hull_at());
    eprintln!("building {b_in}/{b_all} px in its screen box, hull {h_in}/{h_all}");
    assert!(
        b_all > 150 && b_in * 10 >= b_all * 9,
        "building mask {b_in}/{b_all}"
    );
    assert!(h_all > 100 && h_in * 10 >= h_all * 9, "hull mask {h_in}/{h_all}");

    // Jupiter is seen against the black sky, lit on its sunward side, and lights the ground.
    let jupiter = toward(110.0, 17.0);
    let (jx, jy) = project(eye() + jupiter * 1000.0, fov).expect("Jupiter in front of the camera");
    // Brightest pixel over the disc (its sunward side; the far side is unlit in both).
    let radius_px = 7f32.to_radians() / (fov / H as f32);
    let disc = |rgb: &[[f32; 3]]| {
        let mut peak = 0.0f32;
        for (i, c) in rgb.iter().enumerate() {
            let (x, y) = ((i as u32 % W) as f32 + 0.5, (i as u32 / W) as f32 + 0.5);
            if (x - jx).hypot(y - jy) < radius_px * 0.9 {
                peak = peak.max(luminance(*c));
            }
        }
        peak
    };
    let (pt_j, rt_j) = (disc(&img.sky), disc(&hdr.rgb));
    eprintln!("Jupiter at ({jx:.0}, {jy:.0}): peak {pt_j:.1} cd/m² traced, {rt_j:.1} raster");
    assert!(pt_j > 1.0, "Jupiter is visible");
    assert!(
        (0.5..=2.0).contains(&(pt_j / rt_j.max(1e-3))),
        "Jupiter's radiance {pt_j} vs {rt_j}"
    );

    // A second trace of the same world from a moved camera rebuilds no bottom-level BVH.
    let mut moved = scene.clone();
    moved.camera.eye[0] += 3.0;
    let built = cache.blas_built();
    drop(
        tracer
            .begin_cached(&moved, &settings, &mut cache)
            .expect("upload"),
    );
    assert_eq!(cache.blas_built(), built);
}

/// The star field reaches the trace through [`regia_pt::extract::from_frame`] (the
/// `pt_plates` path): an empty airless scene with `ExtractOptions::stars` set traces a non-black
/// sky AOV that is all stars — the sky table is black and the sun is a delta (no disc) — and the
/// same scene without the star field is a black sky.
#[test]
fn from_frame_traces_the_airless_star_field() {
    use regia_pt::extract::{ExtractOptions, atmosphere_sky, from_frame, star_radiance};
    use regia_render::FrameScene;

    let device = RenderDevice::headless().expect("headless GPU (lavapipe?)");
    let planet = AtmosphereParameters::from_preset(AtmospherePreset::Airless);
    let sun = toward(185.0, 24.0);
    let toward_sun = sun.to_array();
    // Look up and away from the sun: the sky AOV is background only.
    let frame = FrameScene {
        width: 160,
        height: 96,
        eye: [0.0, 1.5, 0.0],
        target: [0.0, 60.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new((-sun).to_array(), [1.0; 3], SUN_LUX),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![],
    };
    let sky = atmosphere_sky(&planet, toward_sun, 1.5);
    let scene = from_frame(
        &frame,
        &ExtractOptions {
            sky: Some(sky),
            sun_angular_radius: 0.0,
            skip_blended: true,
            stars: star_radiance(&planet),
            ..ExtractOptions::default()
        },
    );
    assert!(scene.stars > 0.0);

    let tracer = PathTracer::new(&device).expect("tracer");
    let settings = PtSettings {
        spp: 1,
        max_bounces: 1,
        jitter: false,
        ..PtSettings::default()
    };
    let img = tracer.render(&scene, &settings).expect("trace");
    let stars = img.sky.iter().filter(|c| luminance(**c) > 0.0).count();
    eprintln!("{stars} star pixels in the sky AOV");
    assert!(
        stars > 0,
        "the airless star field must be visible through from_frame"
    );

    // Without the star field the same frame is a black sky.
    let mut black = scene.clone();
    black.stars = 0.0;
    let black_img = tracer.render(&black, &settings).expect("trace");
    assert!(
        black_img.sky.iter().all(|c| luminance(*c) == 0.0),
        "no stars means a black sky AOV"
    );
}
