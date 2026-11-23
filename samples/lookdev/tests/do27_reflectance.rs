// SPDX-License-Identifier: MIT-0
//! DO27's cooked terrain material prints its authored reflectance (terrain round 5).
//!
//! Needs `REGIA_DO27_CONTENT` (DO27's read-only content drop); without it the test
//! prints a note and passes. A flat patch at an altitude where only Switchbacks' `dust`
//! layer applies is drawn with the cooked (BC7) material through `FrameRenderer`, next to a
//! grey box whose base colour is that layer's authored albedo (the albedo texture's linear
//! mean × the layer tint × the local macro tint about its map mean). Under the same sun the
//! two must print the same luma within 5 %.

use std::path::Path;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{CdlodConfig, TerrainMap, TerrainRelief, TerrainSharedDraw};

const W: u32 = 320;
const H: u32 = 180;

fn srgb_to_linear(b: u8) -> f64 {
    f64::from(regia_terrain::srgb8_to_linear(b))
}

/// Linear mean RGB of an sRGB PNG.
fn linear_mean(path: &Path) -> [f64; 3] {
    let (_, _, px) = lookdev::default_look::read_png(path).unwrap_or_else(|err| panic!("{err}"));
    let mut s = [0.0; 3];
    let n = (px.len() / 4) as f64;
    for p in px.as_chunks::<4>().0 {
        for c in 0..3 {
            s[c] += srgb_to_linear(p[c]);
        }
    }
    s.map(|v| v / n)
}

/// Linear RGB of the macro map at world (x, z) over the 1280 m map (row 0 = −Z), averaged
/// over a `radius`-texel box.
fn macro_at(path: &Path, x: f32, z: f32, radius: i64) -> [f64; 3] {
    let (w, h, px) = lookdev::default_look::read_png(path).unwrap_or_else(|err| panic!("{err}"));
    let u = ((x + 640.0) / 1280.0 * w as f32) as i64;
    let v = ((z + 640.0) / 1280.0 * h as f32) as i64;
    let mut s = [0.0; 3];
    let mut n = 0.0;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let (xx, yy) = ((u + dx).clamp(0, w as i64 - 1), (v + dy).clamp(0, h as i64 - 1));
            let i = ((yy * w as i64 + xx) * 4) as usize;
            for c in 0..3 {
                s[c] += srgb_to_linear(px[i + c]);
            }
            n += 1.0;
        }
    }
    s.map(|v| v / n)
}

fn luma(p: &[u8]) -> f64 {
    0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2])
}

fn rect_luma(px: &[u8], rect: [f32; 4]) -> f64 {
    let (x0, x1) = ((rect[0] * W as f32) as u32, (rect[2] * W as f32) as u32);
    let (y0, y1) = ((rect[1] * H as f32) as u32, (rect[3] * H as f32) as u32);
    let (mut s, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            s += luma(&px[i..i + 4]);
            n += 1.0;
        }
    }
    s / n
}

#[test]
fn cooked_switchbacks_dust_prints_its_authored_reflectance() {
    let Some(content) = lookdev::do27::content_dir() else {
        eprintln!("skip: REGIA_DO27_CONTENT is not set");
        return;
    };
    let map_dir = content.join("maps/switchbacks");
    let layers = lookdev::do27::load_layers(&map_dir).unwrap_or_else(|err| panic!("{err}"));
    let (cooked, params, tiles) = lookdev::do27::load_cooked(&map_dir).unwrap_or_else(|err| panic!("{err}"));
    let dust = layers
        .layers
        .iter()
        .find(|l| l.name == "dust")
        .expect("Switchbacks has a dust layer");
    // At −150 m on flat ground only `dust` applies (dune_sand ends at −170 m; gravel and
    // scree need slope).
    let altitude = -150.0f32;
    let n = 257u32;
    let map = TerrainMap::from_heights(n, n, vec![altitude; (n * n) as usize], 1280.0, 400.0, -400.0)
        .expect("flat patch");
    let device = regia_terrain::gpu::headless_bcn_device().unwrap_or_else(|err| panic!("{err}"));
    let mut draw = TerrainSharedDraw::new(&device, map, &layers, CdlodConfig::default(), 6).expect("terrain");
    draw.set_material_cooked(&cooked, params, tiles, &[])
        .expect("cooked");
    draw.set_relief(TerrainRelief::off());

    let centre = [-200.0f32, 120.0];
    let detail = linear_mean(&map_dir.join("textures/detail/dust/albedo.png"));
    let macro_png = map_dir.join("textures/macro_albedo.png");
    let macro_mean = linear_mean(&macro_png);
    let macro_here = macro_at(&macro_png, centre[0] - 6.0, centre[1], 6);
    let strength = f64::from(params.macro_strength);
    let expected: [f64; 3] = std::array::from_fn(|c| {
        let tint = 1.0 + (macro_here[c] / macro_mean[c].max(1e-4) - 1.0) * strength;
        detail[c] * f64::from(dust.albedo[c]) * tint
    });
    eprintln!("dust authored albedo here {expected:.3?} (detail {detail:.3?}, macro tint about mean)");

    let post = PostSettings {
        exposure_ev: 14.0,
        ..PostSettings::golden_deterministic()
    };
    let (verts, idx) = regia_render::box_mesh([4.0, 0.25, 4.0]);
    let verts = regia_render::translate_mesh(&verts, [centre[0] + 5.0, altitude + 0.25, centre[1]]);
    let scene = LabScene {
        width: W,
        height: H,
        eye: [centre[0], altitude + 16.0, centre[1] + 0.3],
        target: [centre[0], altitude, centre[1]],
        fov_y: 0.9,
        near: 0.1,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.1, -1.0, 0.05], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [expected[0] as f32, expected[1] as f32, expected[2] as f32, 1.0],
            roughness: 0.92,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: verts,
            indices: idx,
            material: 0,
            ..LabMesh::unskinned()
        }],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let px = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
    if std::env::var_os("REGIA_WRITE_EVIDENCE").is_some() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/do27_reflectance");
        let _ = std::fs::create_dir_all(&dir);
        let _ = lookdev::default_look::write_png(&dir.join("dust_probe.png"), W, H, &px);
    }
    let ground = rect_luma(&px, [0.05, 0.25, 0.3, 0.75]);
    let mesh = rect_luma(&px, [0.62, 0.4, 0.78, 0.6]);
    eprintln!(
        "cooked dust ground luma {ground:.1} vs authored-albedo mesh {mesh:.1} (ratio {:.3})",
        ground / mesh
    );
    assert!(
        mesh > 20.0 && mesh < 240.0,
        "probe exposure out of range: {mesh:.1}"
    );
    assert!(
        (ground / mesh - 1.0).abs() < 0.05,
        "ground {ground:.1} vs mesh {mesh:.1}"
    );
}
