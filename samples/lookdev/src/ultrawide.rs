// SPDX-License-Identifier: MIT-0
//! A 3840×1600 frame, the owner's play resolution, beside the 1440p and 4K benches.
//!
//! The bench's default sizes include [`crate::do27_bench::ULTRAWIDE`]. This renders one
//! lit frame at that size so a lavapipe run can check the resolution without DO27 content.
//! The smoke list judges the same size with the existing color check on a tunnel plate.

use regia_render::{DirectionalLightUniform, FrameRenderer, LabMaterial, LabMesh, LabScene, RenderDevice};

use crate::LookdevError;
use crate::do27_bench::ULTRAWIDE;

/// One ultrawide frame.
#[derive(Clone, Debug, PartialEq)]
pub struct UltrawideReport {
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// Samples read back.
    pub pixels: usize,
    /// Mean Rec. 709 luminance, 0–255.
    pub mean: f32,
    /// Standard deviation of that luminance.
    pub stddev: f32,
    /// The frame is the ultrawide size and is not a flat field.
    pub passed: bool,
    /// One line for the smoke row.
    pub reason: String,
}

impl UltrawideReport {
    /// The smoke row's evidence line.
    #[must_use]
    pub fn evidence(&self) -> String {
        format!(
            "{}x{} pixels {}, mean {:.1} stddev {:.1}",
            self.width, self.height, self.pixels, self.mean, self.stddev
        )
    }
}

/// Renders one frame at [`ULTRAWIDE`] and checks the size and that the image is not flat.
///
/// # Errors
///
/// The frame failed to render.
#[tracing::instrument(skip_all)]
pub fn run(device: &RenderDevice) -> Result<UltrawideReport, LookdevError> {
    let _span = tracing::info_span!("ultrawide_frame", width = ULTRAWIDE.0, height = ULTRAWIDE.1).entered();
    let scene = scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    let frame = renderer.render_offscreen(device, &scene.as_frame_scene())?;
    let (mean, stddev) = luma_stats(&frame.pixels);
    let pixels = frame.pixels.len() / 4;
    let size_ok = frame.width == ULTRAWIDE.0
        && frame.height == ULTRAWIDE.1
        && pixels == (ULTRAWIDE.0 * ULTRAWIDE.1) as usize;
    let passed = size_ok && mean.is_finite() && stddev.is_finite() && stddev > 0.0;
    let reason = if !size_ok {
        format!(
            "frame is {}x{} ({} pixels), want {}x{}",
            frame.width, frame.height, pixels, ULTRAWIDE.0, ULTRAWIDE.1
        )
    } else if !mean.is_finite() || !stddev.is_finite() {
        "the ultrawide frame has a non-finite sample".to_owned()
    } else if stddev <= 0.0 {
        "the ultrawide frame is a flat field".to_owned()
    } else {
        format!(
            "{}x{} frame is not flat (mean {mean:.1}, stddev {stddev:.1})",
            frame.width, frame.height
        )
    };
    Ok(UltrawideReport {
        width: frame.width,
        height: frame.height,
        pixels,
        mean,
        stddev,
        passed,
        reason,
    })
}

fn luma_stats(pixels: &[u8]) -> (f32, f32) {
    let mut count = 0u32;
    let mut sum = 0.0f64;
    let mut square = 0.0f64;
    for chunk in pixels.chunks(4) {
        if chunk.len() < 3 {
            break;
        }
        let lum = 0.2126 * f64::from(chunk[0]) + 0.7152 * f64::from(chunk[1]) + 0.0722 * f64::from(chunk[2]);
        if !lum.is_finite() {
            return (f32::NAN, f32::NAN);
        }
        sum += lum;
        square += lum * lum;
        count += 1;
    }
    if count == 0 {
        return (0.0, 0.0);
    }
    let n = f64::from(count);
    let mean = sum / n;
    let var = (square / n - mean * mean).max(0.0);
    (mean as f32, var.sqrt() as f32)
}

fn scene() -> LabScene {
    let (wall_v, wall_i) = regia_render::box_mesh([6.0, 2.4, 0.08]);
    let (box_v, box_i) = regia_render::box_mesh([0.7, 0.45, 0.35]);
    let box_model = regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(-0.8, -0.2, 1.1))
        .to_cols_array_2d();
    LabScene {
        width: ULTRAWIDE.0,
        height: ULTRAWIDE.1,
        eye: [0.0, 0.2, 3.4],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.7,
        near: 0.05,
        far: 40.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.15], [1.0, 1.0, 1.0], 8_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.22, 0.24, 0.28, 1.0],
                roughness: 0.8,
                unlit: true,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.72, 0.34, 0.16, 1.0],
                roughness: 0.5,
                unlit: true,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            LabMesh {
                vertices: wall_v,
                indices: wall_i,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: box_v,
                indices: box_i,
                material: 1,
                model: box_model,
                ..LabMesh::unskinned()
            },
        ],
    }
}
