// SPDX-License-Identifier: MPL-2.0
//! Terrain drawn inside `FrameRenderer` on Desktop High.
//!
//! One depth buffer and one HDR target: a mesh in front of the heightfield
//! keeps its colour, the ground behind that mesh is not terrain, and the
//! silhouette is not a band of the clear colour. The heightfield also samples
//! the engine cascades, so a caster darkens the ground. Aerial in-scatter is
//! the sky, not a flat grey.

use std::path::PathBuf;

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{CdlodConfig, TerrainLayers, TerrainMap, TerrainSharedDraw};

fn pixel(pixels: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * w + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

fn is_red(p: [u8; 3]) -> bool {
    let (r, g, b) = (u16::from(p[0]), u16::from(p[1]), u16::from(p[2]));
    r > g + 25 && r > b + 25 && r > 40
}

fn is_green(p: [u8; 3]) -> bool {
    let (r, g, b) = (u16::from(p[0]), u16::from(p[1]), u16::from(p[2]));
    g > r + 8 && g > b + 4 && g > 28
}

fn is_clear_band(p: [u8; 3]) -> bool {
    p[0] < 12 && p[1] < 12 && p[2] < 18
}

fn luma(p: [u8; 3]) -> i32 {
    i32::from(p[0]) + i32::from(p[1]) + i32::from(p[2])
}

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"
        TerrainLayers(
            version: 1,
            wetness_default: 0.0,
            layers: [
                LayerDesc(
                    name: "ground",
                    albedo: (0.10, 0.48, 0.08),
                    roughness: 0.9,
                    tiling: 4.0,
                    slope_min: 0.0,
                    slope_max: 1.2,
                    altitude_min: -1000.0,
                    altitude_max: 1000.0,
                    blend_sharpness: 0.15,
                    triplanar: false,
                ),
            ],
        )
        "#,
    )
    .expect("layers")
}

fn flat_map() -> TerrainMap {
    TerrainMap::from_heights(32, 32, vec![0.0; 32 * 32], 48.0, 1.0, 0.0).expect("map")
}

/// Desktop High with the 0.67 performance budget a game can opt into
/// (TAAU, half-res SSGI, 6+8 PCSS, no full-res SSR). The preset itself is native.
fn desktop_high() -> FrameQuality {
    let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
    profile.render_scale = 0.67;
    FrameQuality::from_profile(&profile)
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: true,
        ..PostSettings::golden_deterministic()
    }
}

fn evidence_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../docs/evidence/terrain-runtime/{name}"))
}

/// Evidence is committed; a test run must not rewrite it (every lane that ran this
/// test conflicted with main). Re-bless with `REGIA_WRITE_EVIDENCE=1`.
fn write_png(path: &std::path::Path, width: u32, height: u32, pixels: &[u8]) {
    if regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        eprintln!("skip evidence {}: set REGIA_WRITE_EVIDENCE=1", path.display());
        return;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|err| panic!("evidence dir: {err}"));
    }
    let file = std::fs::File::create(path).unwrap_or_else(|err| panic!("create {}: {err}", path.display()));
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .unwrap_or_else(|err| panic!("png header: {err}"));
    writer
        .write_image_data(pixels)
        .unwrap_or_else(|err| panic!("png pixels: {err}"));
}

fn attach(device: &RenderDevice, renderer: &mut FrameRenderer) {
    let draw =
        TerrainSharedDraw::new(device, flat_map(), &layers(), CdlodConfig::default(), 3).expect("terrain");
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
}

struct Shot {
    eye: [f32; 3],
    target: [f32; 3],
    sun: [f32; 3],
    mesh: Option<(Vec<regia_render::LabVertex>, Vec<u32>)>,
}

fn render_shot(device: &RenderDevice, shot: &Shot) -> regia_render::ForwardResult {
    render_shot_with(device, shot, true, regia_render::ALPHA_OPAQUE)
}

/// `sky` false leaves the IBL sky pass out, so nothing between the shared
/// terrain draw and the translucent loop re-binds the frame's view group.
fn render_shot_with(
    device: &RenderDevice,
    shot: &Shot,
    sky: bool,
    alpha_mode: u32,
) -> regia_render::ForwardResult {
    let mut lab = LabScene {
        width: 320,
        height: 180,
        eye: shot.eye,
        target: shot.target,
        fov_y: 0.8,
        near: 0.05,
        far: 80.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: sky,
        exposure_ev: 14.5,
        ambient_lux: 6_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new(shot.sun, [1.0, 0.97, 0.92], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [
                0.86,
                0.03,
                0.02,
                if alpha_mode == regia_render::ALPHA_OPAQUE {
                    1.0
                } else {
                    0.5
                },
            ],
            roughness: 0.55,
            alpha_mode,
            ..LabMaterial::default()
        }],
        meshes: Vec::new(),
    };
    if let Some((vertices, indices)) = &shot.mesh {
        lab.meshes.push(LabMesh {
            vertices: vertices.clone(),
            indices: indices.clone(),
            material: 0,
            ..LabMesh::unskinned()
        });
    }
    let post = post();
    lab.post = Some(post.clone());
    let mut frame = lab.as_frame_scene_with_quality(desktop_high());
    frame.post = Some(&post);
    frame.ibl = sky;
    let mut renderer = FrameRenderer::new();
    attach(device, &mut renderer);
    renderer
        .render_offscreen(device, &frame)
        .unwrap_or_else(|err| panic!("frame: {err}"))
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// DO27's hull canopies: an alpha-blended mesh over native terrain with the
/// sky pass off. The translucent loop runs right after the shared terrain
/// draw and must see the frame's own view group at slot 0 again, or the first
/// such frame fails wgpu validation (terrain frame layout vs the blend pipeline).
#[test]
fn translucent_mesh_over_native_terrain_without_sky() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let (box_verts, box_idx) = regia_render::box_mesh([0.55, 0.45, 0.55]);
    let box_verts = regia_render::translate_mesh(&box_verts, [0.0, 0.35, 0.0]);
    let shot = Shot {
        eye: [0.0, 2.2, 4.2],
        target: [0.0, 0.35, 0.0],
        sun: [0.25, -0.9, -0.2],
        mesh: Some((box_verts, box_idx)),
    };
    let image = render_shot_with(&device, &shot, false, regia_render::ALPHA_BLEND);
    let mut reds = 0u32;
    let mut greens = 0u32;
    for y in 0..image.height {
        for x in 0..image.width {
            let p = pixel(&image.pixels, image.width, x, y);
            if is_red(p) {
                reds += 1;
            } else if is_green(p) {
                greens += 1;
            }
        }
    }
    eprintln!("translucent over terrain: reds {reds} greens {greens}");
    assert!(reds > 40, "the blended box is not visible ({reds} red pixels)");
    assert!(
        greens > 200,
        "the terrain is not visible around the box ({greens} green pixels)"
    );
}

#[test]
fn terrain_and_mesh_occlude_on_desktop_high() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let quality = desktop_high();
    assert_eq!(quality.render_scale_permille, 670);
    assert!(quality.ssgi_half_res());
    assert!(quality.taau);

    let (box_verts, box_idx) = regia_render::box_mesh([0.55, 0.45, 0.55]);
    let box_verts = regia_render::translate_mesh(&box_verts, [0.0, 0.35, 0.0]);
    let shot = Shot {
        eye: [0.0, 2.2, 4.2],
        target: [0.0, 0.35, 0.0],
        sun: [0.25, -0.9, -0.2],
        mesh: Some((box_verts, box_idx)),
    };
    let with_box = render_shot(&device, &shot);
    let mut bare = shot;
    bare.mesh = None;
    let terrain_only = render_shot(&device, &bare);

    let mut reds = Vec::new();
    let mut occluded = 0u32;
    let mut halo = 0u32;
    let mut adjacent = 0u32;
    for y in 1..with_box.height - 1 {
        for x in 1..with_box.width - 1 {
            let here = pixel(&with_box.pixels, with_box.width, x, y);
            if !is_red(here) {
                continue;
            }
            reds.push((x, y));
            let behind = pixel(&terrain_only.pixels, terrain_only.width, x, y);
            if is_green(behind) {
                occluded += 1;
                assert!(
                    !is_green(here),
                    "pixel ({x},{y}) is terrain even though the box covers it: {here:?}"
                );
            }
            let neighbors = [
                pixel(&with_box.pixels, with_box.width, x - 1, y),
                pixel(&with_box.pixels, with_box.width, x + 1, y),
                pixel(&with_box.pixels, with_box.width, x, y - 1),
                pixel(&with_box.pixels, with_box.width, x, y + 1),
            ];
            if neighbors.iter().any(|n| is_green(*n)) {
                adjacent += 1;
            }
            if neighbors.iter().any(|n| is_clear_band(*n)) {
                halo += 1;
            }
        }
    }
    assert!(
        reds.len() > 40,
        "the box is not visible ({} red pixels)",
        reds.len()
    );
    let (sx, sy) = reds
        .iter()
        .fold((0u32, 0u32), |acc, (x, y)| (acc.0 + x, acc.1 + y));
    let cx = sx / reds.len() as u32;
    let cy = sy / reds.len() as u32;
    let center = pixel(&with_box.pixels, with_box.width, cx, cy);
    eprintln!(
        "occlusion reds {} occluded {occluded} adjacent {adjacent} halo {halo} centroid ({cx},{cy}) {center:?}",
        reds.len()
    );
    assert!(
        is_red(center) && !is_green(center),
        "the box centroid is not the box: {center:?} at ({cx},{cy})"
    );
    assert!(
        occluded > 20,
        "no terrain pixel sits behind the box (occluded {occluded})"
    );
    assert!(adjacent > 0, "terrain and the box do not share an edge");
    assert!(
        halo * 4 < adjacent.max(1),
        "a clear-color band separates terrain and the box (halo {halo}, adjacent {adjacent})"
    );
    let path = evidence_path("occlusion.png");
    write_png(&path, with_box.width, with_box.height, &with_box.pixels);
    eprintln!("wrote {}", path.display());

    // Tall caster. The sun travels toward the camera so the shadow falls on the
    // ground between the eye and the box. A fresh renderer is frame 0, so TAA
    // does not blend the two shots.
    let (tall_verts, tall_idx) = regia_render::box_mesh([0.4, 1.5, 0.4]);
    let tall_verts = regia_render::translate_mesh(&tall_verts, [0.0, 1.5, 0.0]);
    let csm_shot = Shot {
        eye: [0.4, 3.4, 8.0],
        target: [0.0, 0.2, 1.4],
        sun: [0.15, -1.0, 0.65],
        mesh: Some((tall_verts, tall_idx)),
    };
    let cast = render_shot(&device, &csm_shot);
    let mut open = csm_shot;
    open.mesh = None;
    let open = render_shot(&device, &open);
    let mut shadowed = 0u32;
    let mut lit = 0u32;
    for y in 0..cast.height {
        for x in 0..cast.width {
            let bare_px = pixel(&open.pixels, open.width, x, y);
            if !is_green(bare_px) {
                continue;
            }
            let here = pixel(&cast.pixels, cast.width, x, y);
            // Only ground: the red pillar itself (and its antialiased rim) is
            // darker than the lit grass too, and must not count as a shadow.
            if is_red(here) || i32::from(here[0]) > i32::from(here[1]) + 8 {
                continue;
            }
            let drop = luma(bare_px) - luma(here);
            if drop > 24 {
                shadowed += 1;
            } else if drop.abs() < 12 {
                lit += 1;
            }
        }
    }
    eprintln!("csm shadowed ground {shadowed} lit {lit}");
    assert!(
        shadowed > 100,
        "the caster does not darken the terrain ({shadowed} shadowed pixels) — the color pass is not sampling the engine CSM"
    );
    assert!(lit > 30, "no terrain pixel stays lit next to the shadow ({lit})");
    let csm_path = evidence_path("terrain-with-csm.png");
    write_png(&csm_path, cast.width, cast.height, &cast.pixels);
    eprintln!("wrote {}", csm_path.display());
}

#[test]
fn shared_terrain_aerial_is_not_flat_grey() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let mut lab = LabScene {
        width: 160,
        height: 90,
        eye: [0.0, 6.0, 14.0],
        target: [0.0, 0.0, -6.0],
        fov_y: 0.9,
        near: 0.1,
        far: 80.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.9, -0.3], [1.0, 0.97, 0.92], 110_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    };
    lab.post = Some(post.clone());
    let base = FogSettings {
        enabled: true,
        density: 0.035,
        height_falloff: 0.01,
        base_height: 0.0,
        color: [0.45, 0.45, 0.45],
        sky_inscatter: false,
    };
    let mut flat = lab.as_frame_scene_with_quality(FrameQuality::LAB);
    flat.post = Some(&post);
    flat.ibl = true;
    flat.fog = base;
    let mut aerial_fog = base;
    aerial_fog.sky_inscatter = true;
    let mut aerial = lab.as_frame_scene_with_quality(FrameQuality::LAB);
    aerial.post = Some(&post);
    aerial.ibl = true;
    aerial.fog = aerial_fog;

    let mut flat_renderer = FrameRenderer::new();
    attach(&device, &mut flat_renderer);
    let flat_image = flat_renderer
        .render_offscreen(&device, &flat)
        .unwrap_or_else(|err| panic!("flat fog: {err}"));
    let mut aerial_renderer = FrameRenderer::new();
    attach(&device, &mut aerial_renderer);
    let aerial_image = aerial_renderer
        .render_offscreen(&device, &aerial)
        .unwrap_or_else(|err| panic!("aerial: {err}"));

    // A ground pixel well in front of the horizon, not the sky band.
    let x = flat_image.width / 2;
    let y = flat_image.height * 2 / 3;
    let flat_px = pixel(&flat_image.pixels, flat_image.width, x, y);
    let aerial_px = pixel(&aerial_image.pixels, aerial_image.width, x, y);
    eprintln!("fog pixel ({x},{y}) flat {flat_px:?} aerial {aerial_px:?}");
    assert!(
        is_green(flat_px) || luma(flat_px) > 40,
        "the fog sample missed the terrain: {flat_px:?}"
    );
    let flat_ratio = f32::from(flat_px[2]) / f32::from(flat_px[0].max(1));
    let aerial_ratio = f32::from(aerial_px[2]) / f32::from(aerial_px[0].max(1));
    assert!(
        aerial_ratio > flat_ratio + 0.08,
        "aerial in-scatter is not bluer than flat gray (flat {flat_px:?} ratio {flat_ratio:.3}, aerial {aerial_px:?} ratio {aerial_ratio:.3})"
    );
}
