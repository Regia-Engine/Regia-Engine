// SPDX-License-Identifier: MIT-0
//! `render_lab post_volumes [--out DIR]`: a camera walks into a convex-hull post volume (a
//! wedge with a slanted roof, format `post-volume` v3) that grades the view with a warm LUT and
//! a saturation override, blended across a 2 m band (spec 30 § 10, ADR-2323).
//!
//! Each step evaluates the volume at the eye on the CPU (`PostVolume::weight_at`), blends the
//! scalar overrides (`blend_post_volumes`) and the table (`blend_luts`) as the runtime does, renders
//! headless and writes `post_volumes_<step>.png`. One line per step goes to stdout:
//! `post_volumes: step <i> eye_z <z> weight <w> mean_red <r> mean_blue <b>`, so a test can check
//! that the grade rises with the weight without any golden image.

use std::path::PathBuf;
use std::sync::Arc;

use regia_render::post_volume_luts::{LutContribution, blend_luts};
use regia_render::post_volumes::{VolumeContribution, blend_post_volumes};
use regia_render::{
    ColorLut, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    PostOverrides, PostSettings, PostVolume, PostVolumeShape, RenderDevice, RenderProfile,
    RenderProfilePreset, box_mesh, translate_mesh, uv_sphere,
};
use regia_scene::GlobalTransform;

const W: u32 = 160;
const H: u32 = 90;

/// Eye positions along +Z, from outside the band to inside the wedge.
pub const EYE_Z: [f32; 5] = [7.0, 3.75, 3.0, 2.25, 1.0];

/// The wedge: |x| ≤ 3, |z| ≤ 2, y ≥ -1.5 and a roof `y + 0.25 z ≤ 2` (local space), placed with
/// its origin at (0, 1.5, 0), blended over 2 m, with a warm table and more saturation.
pub fn wedge() -> PostVolume {
    PostVolume {
        shape: PostVolumeShape::ConvexHull {
            planes: vec![
                [1.0, 0.0, 0.0, 3.0],
                [-1.0, 0.0, 0.0, 3.0],
                [0.0, 0.0, 1.0, 2.0],
                [0.0, 0.0, -1.0, 2.0],
                [0.0, -1.0, 0.0, 1.5],
                [0.0, 1.0, 0.25, 2.0],
            ],
        },
        blend_distance: 2.0,
        priority: 1,
        overrides: PostOverrides {
            saturation: Some(1.25),
            ..PostOverrides::default()
        },
        lut: Some("looks/warm.cube".into()),
        ..PostVolume::default()
    }
}

fn warm_lut() -> Arc<ColorLut> {
    Arc::new(ColorLut::from_fn(17, |c| {
        [(c[0] * 1.3).min(1.0), c[1], c[2] * 0.7]
    }))
}

fn scene(eye_z: f32, post: PostSettings) -> LabScene {
    let (sv, si) = uv_sphere(1.0, 32, 16);
    let (bv, bi) = box_mesh([6.0, 0.1, 12.0]);
    let mat = |c: [f32; 3], r: f32| LabMaterial {
        base_color: [c[0], c[1], c[2], 1.0],
        roughness: r,
        ..LabMaterial::default()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.5, eye_z],
        target: [0.0, 0.8, eye_z - 4.0],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.4, -0.8, -0.3], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![mat([0.55, 0.5, 0.45], 0.5), mat([0.5, 0.5, 0.5], 0.8)],
        meshes: vec![
            LabMesh {
                vertices: translate_mesh(&sv, [0.0, 1.0, eye_z - 4.0]),
                indices: si,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: translate_mesh(&bv, [0.0, -0.1, 0.0]),
                indices: bi,
                material: 1,
                ..LabMesh::unskinned()
            },
        ],
    }
}

/// `(mean red, mean blue)` of RGBA8 pixels, 0–255.
fn mean_red_blue(pixels: &[u8]) -> (f32, f32) {
    let (chunks, _) = pixels.as_chunks::<4>();
    let n = chunks.len().max(1) as f32;
    let red = chunks.iter().map(|p| f32::from(p[0])).sum::<f32>() / n;
    let blue = chunks.iter().map(|p| f32::from(p[2])).sum::<f32>() / n;
    (red, blue)
}

/// Runs the walk. `args` are the words after `post_volumes`.
pub fn run(args: &[String]) -> Result<(), String> {
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map_or_else(|| PathBuf::from("target/render-lab"), PathBuf::from);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let device = RenderDevice::headless().map_err(|e| format!("no GPU adapter: {e}"))?;
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopLow));
    let volume = wedge();
    let world = GlobalTransform(glam::Affine3A::from_translation(glam::Vec3::new(0.0, 1.5, 0.0)));
    let lut = warm_lut();
    let mut renderer = FrameRenderer::new();
    for (step, &eye_z) in EYE_Z.iter().enumerate() {
        let weight = volume.weight_at(&world, [0.0, 1.5, eye_z]);
        let mut post = PostSettings {
            exposure_ev: 14.0,
            ..PostSettings::golden_deterministic()
        };
        let mut scalars = [VolumeContribution {
            weight,
            priority: volume.priority,
            key: 1,
            overrides: volume.overrides,
        }];
        blend_post_volumes(&mut post, &mut scalars);
        post.lut = blend_luts(
            None,
            &mut [LutContribution {
                weight,
                priority: volume.priority,
                key: 1,
                lut: lut.clone(),
            }],
        );
        let frame = renderer
            .render_offscreen(&device, &scene(eye_z, post).as_frame_scene_with_quality(quality))
            .map_err(|e| format!("step {step}: {e}"))?;
        let (red, blue) = mean_red_blue(&frame.pixels);
        let path = out.join(format!("post_volumes_{step}.png"));
        image::save_buffer(&path, &frame.pixels, W, H, image::ColorType::Rgba8)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        println!(
            "post_volumes: step {step} eye_z {eye_z} weight {weight:.3} mean_red {red:.2} mean_blue {blue:.2}"
        );
    }
    Ok(())
}
