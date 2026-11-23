// SPDX-License-Identifier: MIT-0
//! Terrain takes the engine SSAO (look-r2 round 3): a box resting on shared-draw terrain
//! darkens the ground at its base, as it would a mesh floor. Before round 3 the shared
//! terrain shader never read the AO texture, so the ground at a contact was identical with
//! SSAO on and off and rock/prop contacts read as pasted on.

use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::camera::rh::view::look_at_mat4;
use regia_core::glam::{Vec3, Vec4};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw, SsaoMode, box_mesh, translate_mesh,
};
use regia_terrain::{CdlodConfig, LayerDesc, TerrainLayers, TerrainMap, TerrainSharedDraw};

const W: u32 = 320;
const H: u32 = 180;
const EYE: [f32; 3] = [5.0, 2.2, 5.0];
const TARGET: [f32; 3] = [0.0, 0.4, 0.0];
const FOV: f32 = 0.9;

fn render(device: &RenderDevice, ssao: SsaoMode) -> Vec<u8> {
    let n = 129;
    let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 64.0, 1.0, 0.0).expect("flat map");
    let mut layer = LayerDesc::ground();
    layer.albedo = [0.5, 0.42, 0.34];
    layer.slope_max = 1.6;
    let layers = TerrainLayers {
        layers: vec![layer],
        ..TerrainLayers::default()
    };
    let draw = TerrainSharedDraw::new(device, map, &layers, CdlodConfig::default(), 4).expect("terrain");
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let (v, i) = box_mesh([1.0, 1.0, 1.0]);
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 11.0,
        bloom: false,
        ..PostSettings::default()
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV,
        near: 0.1,
        far: 500.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 11.0,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post),
        // A weak sun from behind the box: the ground in front of it is lit by the sky.
        directional: DirectionalLightUniform::new([0.3, -0.5, 0.8], [1.0; 3], 500.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 0.8,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&v, [0.0, 1.0, 0.0]),
            indices: i,
            material: 0,
            ..LabMesh::unskinned()
        }],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.ssao = ssao;
    quality.taau = false;
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut pixels = Vec::new();
    for _ in 0..3 {
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    pixels
}

/// Screen pixel of a world point.
fn project(p: Vec3) -> (f32, f32) {
    let view = look_at_mat4(Vec3::from_array(EYE), Vec3::from_array(TARGET), Vec3::Y);
    let proj = perspective(FOV, W as f32 / H as f32, 0.1, 500.0);
    let c = proj * view * Vec4::new(p.x, p.y, p.z, 1.0);
    let ndc = c.truncate() / c.w;
    ((ndc.x * 0.5 + 0.5) * W as f32, (0.5 - ndc.y * 0.5) * H as f32)
}

fn mean_luma(pixels: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * f32::from(pixels[i])
                + 0.7152 * f32::from(pixels[i + 1])
                + 0.0722 * f32::from(pixels[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

#[test]
fn ssao_darkens_the_terrain_at_a_box_contact() {
    let device = RenderDevice::headless().expect("lavapipe");
    let on = render(&device, SsaoMode::GtaoHalfRes);
    let off = render(&device, SsaoMode::Off);
    if std::env::var_os("REGIA_WRITE_EVIDENCE").is_some() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/look_r2_tests");
        let _ = std::fs::create_dir_all(&dir);
        let _ = lookdev::default_look::write_png(&dir.join("terrain_ssao_on.png"), W, H, &on);
        let _ = lookdev::default_look::write_png(&dir.join("terrain_ssao_off.png"), W, H, &off);
    }
    // The ground just below the box's front bottom corner on screen (the contact).
    let (cx, cy) = project(Vec3::new(1.0, 0.0, 1.0));
    let (x0, y0) = ((cx - 10.0).max(0.0) as u32, (cy + 1.0) as u32);
    let (x1, y1) = (((cx + 10.0) as u32).min(W), ((cy + 4.0) as u32).min(H));
    let lit_off = mean_luma(&off, x0, y0, x1, y1);
    let lit_on = mean_luma(&on, x0, y0, x1, y1);
    // Far ground, away from any contact, must not change much.
    let (fx, fy) = project(Vec3::new(-6.0, 0.0, 3.0));
    let far_off = mean_luma(&off, fx as u32 - 4, fy as u32 - 2, fx as u32 + 4, fy as u32 + 2);
    let far_on = mean_luma(&on, fx as u32 - 4, fy as u32 - 2, fx as u32 + 4, fy as u32 + 2);
    // The whole contact band below the box (12 rows), integrated: robust to which row the
    // half-resolution AO edge lands on.
    let (bx0, by0) = ((cx - 20.0).max(0.0) as u32, (cy - 3.0).max(0.0) as u32);
    let (bx1, by1) = (((cx + 20.0) as u32).min(W), ((cy + 13.0) as u32).min(H));
    let band_off = mean_luma(&off, bx0, by0, bx1, by1);
    let band_on = mean_luma(&on, bx0, by0, bx1, by1);
    eprintln!(
        "contact luma off {lit_off:.1} on {lit_on:.1} ({:.2} %); band {:.2} %; open ground off {far_off:.1} on {far_on:.1}",
        100.0 * (1.0 - lit_on / lit_off),
        100.0 * (1.0 - band_on / band_off)
    );
    // Relative darkening, the same on lavapipe and NVIDIA since round 7 (the AO pass maps AO
    // texels to depth texels in integer arithmetic; the old float mapping rounded the edge
    // row differently per driver: lavapipe 4.3 %, NVIDIA 3.4 %, now both 3.4–3.5 %).
    // The 3.5 % bar was set on lavapipe's rounding; 2.5 % holds on both with margin.
    assert!(
        lit_on < lit_off * 0.975,
        "the terrain at the box contact is not occluded: {lit_on:.1} with SSAO vs {lit_off:.1} without"
    );
    assert!(
        band_on < band_off * 0.99,
        "the contact band is not occluded: {band_on:.1} with SSAO vs {band_off:.1} without"
    );
    assert!(
        (far_on - far_off).abs() < far_off * 0.05,
        "SSAO darkened open ground: {far_on:.1} vs {far_off:.1}"
    );
}
