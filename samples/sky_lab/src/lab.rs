// SPDX-License-Identifier: MIT-0
//! Headless rendering of a [`Scenario`]: the sky, the cloud layer, a plain and a ridge.

use std::io::Write;
use std::path::Path;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode,
};

use crate::{FrameState, Options, Scenario};

/// Camera position: 3 m above the plain.
const EYE: [f32; 3] = [0.0, 3.0, 0.0];
/// Camera target: north, tilted up so the cloud layer fills the upper half of the frame.
const TARGET: [f32; 3] = [0.0, 600.0, 1000.0];

/// Desktop High with the sky and cloud passes and nothing that changes pixels on its own
/// (no screen-space passes, no temporal anti-aliasing), so frames differ by the day cycle and
/// the weather only.
#[must_use]
pub fn quality() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gpu_driven = false;
    quality.ssao = SsaoMode::Off;
    quality.ssr = false;
    quality.screen_space_gi = false;
    quality.gi_probes = false;
    quality.gi = GiMode::Ambient;
    quality.contact_shadows = false;
    quality.volumetric_fog = false;
    quality.taau = false;
    quality.render_scale_permille = 1000;
    quality.csm_cascade_count = 1;
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 0;
    quality.reflection_probes = false;
    quality.parallax_occlusion = false;
    quality.screen_space_refraction = false;
    quality.volumetric_clouds = true;
    quality.physical_atmosphere = true;
    quality.near_sun_tile = 0;
    quality.translucent_shadow_cascades = 0;
    quality.translucent_local_shadows = false;
    quality.depth_velocity = false;
    quality.reflection_occlusion = false;
    quality.paged_sun_shadows = false;
    quality.cloud_reprojection = true;
    quality
}

fn quad(half: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 1.0, 0.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([-half, 0.0, -half], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([half, 0.0, -half], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([half, 0.0, half], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-half, 0.0, half], n, [0.0, 1.0]),
        ],
        vec![0, 2, 1, 0, 3, 2],
    )
}

/// A vertical slab facing -Z: a ridge on the horizon.
fn ridge(x: [f32; 2], top: f32, z: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 0.0, -1.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([x[0], 0.0, z], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([x[1], 0.0, z], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x[1], top, z], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x[0], top, z], n, [0.0, 1.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

fn material(color: [f32; 3]) -> LabMaterial {
    let mut mat = LabMaterial::EMPTY;
    mat.base_color = [color[0], color[1], color[2], 1.0];
    mat.roughness = 0.9;
    mat
}

/// The lab scene at `size`: an 16 km plain and a ridge 1.8 km north.
#[must_use]
pub fn lab_scene(size: (u32, u32)) -> LabScene {
    let (gv, gi) = quad(8000.0);
    let (rv, ri) = ridge([-900.0, 300.0], 260.0, 1800.0);
    LabScene {
        width: size.0,
        height: size.1,
        eye: EYE,
        target: TARGET,
        fov_y: 55.0_f32.to_radians(),
        near: 0.3,
        far: 40_000.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: 14.5,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        // The outdoor sky replaces this light with the sun of the frame's time of day.
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![material([0.22, 0.24, 0.16]), material([0.35, 0.3, 0.28])],
        meshes: vec![
            LabMesh {
                vertices: gv,
                indices: gi,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: rv,
                indices: ri,
                material: 1,
                ..LabMesh::unskinned()
            },
        ],
    }
}

/// One rendered frame's numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameRecord {
    /// Frame number.
    pub frame: u32,
    /// Hour of day.
    pub hour: f32,
    /// Sun elevation in degrees.
    pub sun_elevation_deg: f32,
    /// Cloud coverage this frame.
    pub coverage: f32,
    /// Mean luma of the image in 8-bit levels.
    pub mean_luma: f32,
}

/// The result of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Image width.
    pub width: u32,
    /// Image height.
    pub height: u32,
    /// One record per frame.
    pub frames: Vec<FrameRecord>,
}

impl Report {
    /// The report as JSON: `{ width, height, frames: [{ frame, hour, sun_elevation_deg,
    /// coverage, mean_luma }] }`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let frames: Vec<_> = self
            .frames
            .iter()
            .map(|f| {
                serde_json::json!({
                    "frame": f.frame,
                    "hour": f.hour,
                    "sun_elevation_deg": f.sun_elevation_deg,
                    "coverage": f.coverage,
                    "mean_luma": f.mean_luma,
                })
            })
            .collect();
        serde_json::json!({ "width": self.width, "height": self.height, "frames": frames })
    }
}

/// Mean luma (8-bit levels) of an RGBA8 image.
#[must_use]
pub fn mean_luma(pixels: &[u8]) -> f32 {
    let (chunks, _) = pixels.as_chunks::<4>();
    if chunks.is_empty() {
        return 0.0;
    }
    let sum: f64 = chunks
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .sum();
    (sum / chunks.len() as f64) as f32
}

/// A renderer and scene for one run. Frames must be rendered in order from frame 0 so the
/// cloud history converges the way it does in a game.
pub struct SkyLab {
    renderer: FrameRenderer,
    scene: LabScene,
    quality: FrameQuality,
}

impl SkyLab {
    /// A renderer for images of `size`.
    #[must_use]
    pub fn new(size: (u32, u32)) -> Self {
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_enabled(Some(false));
        // The camera is fixed; the history is reset only by an explicit cut.
        renderer.set_camera_cut_detection(None);
        Self {
            renderer,
            scene: lab_scene(size),
            quality: quality(),
        }
    }

    /// Renders `state` and returns RGBA8 pixels.
    ///
    /// # Errors
    /// The renderer's error text.
    #[tracing::instrument(skip_all, fields(frame = state.frame))]
    pub fn render(&mut self, device: &RenderDevice, state: &FrameState) -> Result<Vec<u8>, String> {
        self.renderer.set_outdoor(state.outdoor());
        let scene = self.scene.as_frame_scene_with_quality(self.quality);
        self.renderer
            .render_offscreen(device, &scene)
            .map(|result| result.pixels)
            .map_err(|err| format!("frame {}: {err}", state.frame))
    }
}

/// Writes an RGBA8 PNG through a sibling temp file and a rename.
///
/// # Errors
/// The IO or encoding error as text.
pub fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("png.tmp");
    let write = || -> Result<(), String> {
        let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(pixels).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())
    };
    if let Err(err) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {err}", path.display()));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes `text` to `path` through a sibling temp file and a rename.
fn write_text(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Renders every frame of `options.scenario`. With `options.out` set, writes `frame_NNNN.png`
/// every `options.every` frames (and on the last), plus `report.json`.
///
/// # Errors
/// A render, encoding or IO error as text.
#[tracing::instrument(skip_all)]
pub fn run(device: &RenderDevice, options: &Options) -> Result<Report, String> {
    let scenario: &Scenario = &options.scenario;
    if let Some(dir) = &options.out {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let (width, height) = options.size;
    let mut lab = SkyLab::new(options.size);
    let mut frames = Vec::new();
    for frame in 0..scenario.frames {
        let state = scenario.state_at(frame);
        let pixels = lab.render(device, &state)?;
        frames.push(FrameRecord {
            frame,
            hour: state.time.hour,
            sun_elevation_deg: state.sun_elevation_deg(),
            coverage: state.clouds.coverage,
            mean_luma: mean_luma(&pixels),
        });
        if let Some(dir) = &options.out
            && (frame.is_multiple_of(options.every) || frame + 1 == scenario.frames)
        {
            write_png(&dir.join(format!("frame_{frame:04}.png")), width, height, &pixels)?;
        }
    }
    let report = Report {
        width,
        height,
        frames,
    };
    if let Some(dir) = &options.out {
        write_text(&dir.join("report.json"), &report.to_json().to_string())?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_luma_of_flat_images() {
        assert_eq!(mean_luma(&[]), 0.0);
        assert_eq!(mean_luma(&[0, 0, 0, 255, 0, 0, 0, 255]), 0.0);
        let white = [255u8; 16];
        assert!((mean_luma(&white) - 255.0).abs() < 0.01);
    }

    #[test]
    fn the_quality_has_the_sky_and_the_clouds_on() {
        let q = quality();
        assert!(q.volumetric_clouds && q.physical_atmosphere && q.cloud_reprojection);
        assert!(!q.taau && q.ssao == SsaoMode::Off);
    }

    #[test]
    fn the_report_json_lists_every_frame() {
        let report = Report {
            width: 4,
            height: 2,
            frames: vec![FrameRecord {
                frame: 0,
                hour: 6.0,
                sun_elevation_deg: 1.5,
                coverage: 0.3,
                mean_luma: 10.0,
            }],
        };
        let json = report.to_json();
        assert_eq!(json["width"], 4);
        assert_eq!(json["frames"][0]["frame"], 0);
        assert!(
            json["frames"][0]["coverage"]
                .as_f64()
                .is_some_and(|v| (v - 0.3).abs() < 1e-6)
        );
    }

    #[test]
    fn the_lab_scene_has_the_requested_size() {
        let scene = lab_scene((64, 36));
        assert_eq!((scene.width, scene.height), (64, 36));
        assert_eq!(scene.meshes.len(), 2);
    }
}
