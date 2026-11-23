// SPDX-License-Identifier: MPL-2.0
//! The mip-level debug view on the bindless texture-table path (ADR-863, ADR-7254), on lavapipe.
//!
//! Its own test binary: the table is asked for once per process
//! (`texture_table::set_device_level_request`) before the device is created. The view must read
//! each texture's real edge from the table: a 256- and a 1024-texel base-colour texture on the same
//! quad sit two mip levels apart, so their frames differ. No golden.

use regia_render::forward_lite::ForwardLite;
use regia_render::{
    DebugViewMode, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabTexture, LabVertex, LightsViews, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 128;
const H: u32 = 96;

fn device() -> RenderDevice {
    regia_render::texture_table::set_device_level_request(regia_render::texture_table::TEXTURE_TABLE_LEVEL);
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn quad() -> LabMesh {
    let corners = [
        ([-2.0, -1.5, 0.0], [0.0, 1.0]),
        ([2.0, -1.5, 0.0], [1.0, 1.0]),
        ([2.0, 1.5, 0.0], [1.0, 0.0]),
        ([-2.0, 1.5, 0.0], [0.0, 0.0]),
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

fn scene(texels: u32) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.3, -0.2, -1.0], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.45, 0.3, 1.0],
            roughness: 0.9,
            albedo: Some(LabTexture {
                width: texels,
                height: texels,
                rgba: [200u8, 190, 180, 255].repeat((texels * texels) as usize),
            }),
            ..LabMaterial::default()
        }],
        meshes: vec![quad()],
    }
}

/// The mip view of `scene`, and whether the frame drew through the texture table.
fn render(
    device: &RenderDevice,
    scene: &LabScene,
    quality: FrameQuality,
    lite: Option<ForwardLite>,
) -> (Vec<u8>, bool) {
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(lite);
    renderer.set_lights_views(&LightsViews {
        shading_view: DebugViewMode::MipLevel,
        ..LightsViews::default()
    });
    let frame = scene.as_frame_scene_with_quality(quality);
    let pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    (pixels, renderer.texture_table_stats().is_some())
}

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

#[test]
fn the_table_mip_view_reads_each_textures_size() {
    let device = device();
    let mobile = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileHigh));
    for (name, quality, lite) in [
        ("full forward", FrameQuality::LAB, None),
        ("lite forward", mobile, Some(ForwardLite::High)),
    ] {
        let (small, small_table) = render(&device, &scene(256), quality, lite);
        let (large, large_table) = render(&device, &scene(1024), quality, lite);
        assert!(
            small_table && large_table,
            "{name}: the frame must draw through the texture table (does the device carry it?)"
        );
        let diff = mean_abs_diff(&small, &large);
        eprintln!("{name}: table mip view 256 vs 1024 texels, mean diff {diff:.2}");
        assert!(
            diff > 3.0,
            "{name}: the table mip view must read the texture's size: {diff:.2}"
        );
    }
}
