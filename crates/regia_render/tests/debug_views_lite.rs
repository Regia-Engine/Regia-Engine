// SPDX-License-Identifier: MPL-2.0
//! The shader debug view modes on the Mobile lite forward (`forward_mobile.wgsl`, ADR-7254), on
//! lavapipe.
//!
//! `DebugView::is_supported` promises every mode on every path. Lavapipe is no tile GPU, so each
//! check forces the lite tier (`FrameRenderer::set_forward_lite`) at its Mobile profile and
//! compares a view against the lit frame of the same scene at the same frame (a fresh renderer
//! each): ambient 0, no IBL and a fixed EV100, so the only light is the sun. No golden.

use regia_render::forward_lite::ForwardLite;
use regia_render::{
    DebugView, DebugViewMode, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, LabTexture, LabVertex, LightsViews, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset,
};

const W: u32 = 128;
const H: u32 = 96;

const TIERS: [(ForwardLite, RenderProfilePreset); 2] = [
    (ForwardLite::Low, RenderProfilePreset::MobileLow),
    (ForwardLite::High, RenderProfilePreset::MobileHigh),
];

/// The debug view modes the forward shaders draw (`lights_views::shading_view_code` above 0).
const SHADER_MODES: [DebugViewMode; 10] = [
    DebugViewMode::Unlit,
    DebugViewMode::Normals,
    DebugViewMode::BaseColor,
    DebugViewMode::Roughness,
    DebugViewMode::Metallic,
    DebugViewMode::AmbientOcclusion,
    DebugViewMode::LightComplexity,
    DebugViewMode::ShadowCascades,
    DebugViewMode::MotionVectors,
    DebugViewMode::MipLevel,
];

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn quality(preset: RenderProfilePreset) -> FrameQuality {
    FrameQuality::from_profile(&RenderProfile::from_preset(preset))
}

/// A quad in the XY plane centred on the origin, front face +Z, UV 0..1 across it.
fn quad(half_w: f32, half_h: f32) -> LabMesh {
    let corners = [
        ([-half_w, -half_h, 0.0], [0.0, 1.0]),
        ([half_w, -half_h, 0.0], [1.0, 1.0]),
        ([half_w, half_h, 0.0], [1.0, 0.0]),
        ([-half_w, half_h, 0.0], [0.0, 0.0]),
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

/// A flat mid-grey texture `size` texels on a side.
fn texture(size: u32) -> LabTexture {
    LabTexture {
        width: size,
        height: size,
        rgba: [200u8, 190, 180, 255].repeat((size * size) as usize),
    }
}

/// One partly metallic quad filling most of the view, sun with shadows, camera on +Z.
fn scene() -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.3, -0.2, -1.0], [1.0, 1.0, 1.0], 30_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.45, 0.3, 1.0],
            roughness: 0.9,
            metallic: 0.4,
            albedo: Some(texture(64)),
            ..LabMaterial::default()
        }],
        meshes: vec![quad(2.0, 1.5)],
    }
}

fn render(
    device: &RenderDevice,
    scene: &LabScene,
    tier: (ForwardLite, RenderProfilePreset),
    mode: DebugViewMode,
) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(Some(tier.0));
    renderer.set_lights_views(&LightsViews {
        shading_view: mode,
        ..LightsViews::default()
    });
    let frame = scene.as_frame_scene_with_quality(quality(tier.1));
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

/// Mean Rec. 709 luma of the centre fifth of the view.
fn centre(px: &[u8]) -> f64 {
    let (mut sum, mut n) = (0.0, 0.0);
    for y in 2 * H / 5..3 * H / 5 {
        for x in 2 * W / 5..3 * W / 5 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

/// Mean absolute difference of the colour channels over the centre half of the view.
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    let (mut sum, mut n) = (0.0, 0.0);
    for y in H / 4..3 * H / 4 {
        for x in W / 4..3 * W / 4 {
            let i = ((y * W + x) * 4) as usize;
            for c in 0..3 {
                sum += (f64::from(a[i + c]) - f64::from(b[i + c])).abs();
                n += 1.0;
            }
        }
    }
    sum / n
}

/// More than half of the centre half has a channel above 8.
fn non_black(px: &[u8]) -> bool {
    let (mut lit, mut n) = (0usize, 0usize);
    for y in H / 4..3 * H / 4 {
        for x in W / 4..3 * W / 4 {
            let i = ((y * W + x) * 4) as usize;
            n += 1;
            if px[i].max(px[i + 1]).max(px[i + 2]) > 8 {
                lit += 1;
            }
        }
    }
    lit * 2 > n
}

#[test]
fn every_shader_view_mode_draws_on_the_lite_forward() {
    let device = device();
    let scene = scene();
    for tier in TIERS {
        let lit = render(&device, &scene, tier, DebugViewMode::Lit);
        assert!(non_black(&lit), "{tier:?}: the lit control must be lit");
        for mode in SHADER_MODES {
            assert!(DebugView::is_supported(mode), "{mode:?} must be supported");
            let image = render(&device, &scene, tier, mode);
            let diff = mean_abs_diff(&image, &lit);
            eprintln!(
                "{:?} {mode:?}: centre luma {:.2}, mean diff from lit {diff:.2}",
                tier.0,
                centre(&image)
            );
            assert!(
                non_black(&image),
                "{:?} {mode:?} must draw a non-black image",
                tier.0
            );
            assert!(
                diff > 3.0,
                "{:?} {mode:?} must differ from the lit frame: {diff:.2}",
                tier.0
            );
        }
    }
}

#[test]
fn the_lite_mip_view_reads_the_texture_size() {
    // The same quad and footprint with a 256- and a 1024-texel base-colour texture: two mip levels
    // apart, so the heat colour differs.
    let device = device();
    for tier in TIERS {
        let mut small = scene();
        small.materials[0].albedo = Some(texture(256));
        let mut large = scene();
        large.materials[0].albedo = Some(texture(1024));
        let a = render(&device, &small, tier, DebugViewMode::MipLevel);
        let b = render(&device, &large, tier, DebugViewMode::MipLevel);
        let diff = mean_abs_diff(&a, &b);
        eprintln!("{:?} mip view 256 vs 1024 texels: mean diff {diff:.2}", tier.0);
        assert!(
            diff > 3.0,
            "{:?}: the mip view must read the texture's size: {diff:.2}",
            tier.0
        );
    }
}

#[test]
fn the_material_swap_views_draw_on_the_lite_forward() {
    // Overdraw and LOD coloring swap materials (`debug_shading`): unlit draws the lite path takes.
    let device = device();
    for tier in TIERS {
        let base = scene();
        let lit = render(&device, &base, tier, DebugViewMode::Lit);
        let mut lod = base.clone();
        lod.materials = vec![regia_render::debug_shading::lod_color_material(1)];
        let mut overdraw = base.clone();
        overdraw.materials = vec![regia_render::debug_shading::overdraw_material()];
        for (name, s) in [("lod_color", lod), ("overdraw", overdraw)] {
            let image = render(&device, &s, tier, DebugViewMode::Lit);
            let diff = mean_abs_diff(&image, &lit);
            eprintln!("{:?} {name}: mean diff from lit {diff:.2}", tier.0);
            assert!(
                non_black(&image),
                "{:?} {name} must draw a non-black image",
                tier.0
            );
            assert!(diff > 3.0, "{:?} {name} must differ from lit: {diff:.2}", tier.0);
        }
    }
}
