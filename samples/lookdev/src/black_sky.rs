// SPDX-License-Identifier: MIT-0
//! Airless-sky motion check (`do27_chase --motion pan` on Bore Twelve is the same failure).
//!
//! v0.1.3's TAAU motion sharpen widened its deringing clamp by a quarter of the log range.
//! A black neighbor sits at the log floor, so a lit edge pixel against an airless sky was
//! brightened by many times (up to about ×84) and read as a white line after the tonemap.
//! Lit-sky plates never showed it. A steady pan never shows it either: the history stays
//! trusted and the resolve softens the edge before the sharpen runs. This check settles a
//! pose, then jumps the camera far enough that the history weight falls off, and compares
//! that frame with a still of the same view.
//!
//! The still is the same pose with TAAU off, so it is the shading without the sharpen.
//! A held TAA resolve is several times darker at the edge than the pan even with the
//! sharpen forced off, and a lit face a few pixels inward can be a different side of the
//! hull, so neither of those is the reference. Along the silhouette, no pan pixel may
//! exceed 1.3× the brightest sample in the still's 3×3 neighborhood (the same bound the
//! sharpen's +25 % cap satisfies: the v0.1.4 clamp is 1.25× the pixel's own resolve, and
//! the resolve stays inside that neighborhood).

use regia_render::{
    AtmosphereParameters, DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode, HdrCapture,
    LabMaterial, LabMesh, LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, SsaoMode, TimeOfDay,
};

use crate::LookdevError;

/// Output of the jump. Large enough for a hull silhouette, small enough for lavapipe.
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
/// Yaw of the scored frame, radians. About twenty pixels of hull motion from the settled
/// pose, past the two pixels where velocity disagreement drops the history weight to zero.
const JUMP_YAW: f32 = 0.08;
/// A moving silhouette pixel may be this many times the still neighborhood's luminance.
///
/// A luminance ratio, not a per-device timing budget. It sits above the v0.1.4
/// sharpen cap (1.25× the pixel's own resolve), so a build that honors the cap
/// passes wherever the still neighborhood still covers that resolve. Smoke keeps
/// this row as a failure.
pub const SILHOUETTE_GAIN_MAX: f32 = 1.3;
/// Lit pixels required before the gain is meaningful. A black frame is not a pass.
const MIN_SILHOUETTE_PIXELS: u32 = 24;
/// Still-frame luminance below this is sky.
const SKY_LUMA: f32 = 0.05;
/// Still-frame luminance above this is the lit hull.
const FACE_LUMA: f32 = 0.5;

/// Rec. 709 luminance of one linear RGB sample.
#[must_use]
pub fn luma(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// How a jumped frame compares with the still of its pose.
#[derive(Clone, Debug, PartialEq)]
pub struct SilhouetteReport {
    /// Edge pixels scored.
    pub silhouette_pixels: u32,
    /// Largest pan / still-neighborhood luminance on those pixels.
    pub max_gain: f32,
    /// Non-finite samples in either frame.
    pub non_finite: u32,
    /// The check holds.
    pub passed: bool,
    /// One line for the smoke row.
    pub reason: String,
}

impl SilhouetteReport {
    /// The smoke row's evidence line.
    #[must_use]
    pub fn evidence(&self) -> String {
        format!(
            "silhouette pixels {}, max gain {:.2}× (cap {SILHOUETTE_GAIN_MAX}), non-finite {}",
            self.silhouette_pixels, self.max_gain, self.non_finite
        )
    }
}

/// Scores `moving` against `still` (same length). A silhouette pixel is a lit still sample
/// with a sky neighbor. Its gain is the pan luminance divided by the brightest still sample
/// in the 3×3 neighborhood, which is the shading the sharpen is allowed to approach.
#[must_use]
pub fn score_silhouettes(
    still: &[[f32; 3]],
    moving: &[[f32; 3]],
    width: u32,
    height: u32,
) -> SilhouetteReport {
    let pixels = (width as usize).saturating_mul(height as usize);
    if still.len() != pixels || moving.len() != pixels || width < 3 || height < 3 {
        return SilhouetteReport {
            silhouette_pixels: 0,
            max_gain: 0.0,
            non_finite: 0,
            passed: false,
            reason: "the still and moving frames do not match".to_owned(),
        };
    }
    let mut non_finite = 0u32;
    for (s, m) in still.iter().zip(moving.iter()) {
        if s.iter().any(|c| !c.is_finite()) || m.iter().any(|c| !c.is_finite()) {
            non_finite += 1;
        }
    }
    let mut edges = 0u32;
    let mut max_gain = 1.0f32;
    let mut worst_moving = 0.0f32;
    let mut worst_bound = 0.0f32;
    if non_finite == 0 {
        for y in 1..height - 1 {
            for x in 1..width - 1 {
                let index = (y * width + x) as usize;
                if luma(still[index]) <= FACE_LUMA || !sky_neighbor(still, width, x, y) {
                    continue;
                }
                edges += 1;
                let bound = neighborhood_max(still, width, height, x, y);
                if bound <= SKY_LUMA {
                    continue;
                }
                let moving_l = luma(moving[index]);
                let gain = moving_l / bound;
                if gain.is_finite() {
                    if gain > max_gain {
                        max_gain = gain;
                        worst_moving = moving_l;
                        worst_bound = bound;
                    }
                } else {
                    non_finite += 1;
                }
            }
        }
    }
    let passed = non_finite == 0 && edges >= MIN_SILHOUETTE_PIXELS && max_gain <= SILHOUETTE_GAIN_MAX;
    let reason = if non_finite > 0 {
        format!("{non_finite} non-finite pixels")
    } else if edges < MIN_SILHOUETTE_PIXELS {
        format!(
            "only {edges} silhouette pixels (need {MIN_SILHOUETTE_PIXELS}); the hull is not against the sky"
        )
    } else if max_gain > SILHOUETTE_GAIN_MAX {
        format!(
            "a silhouette pixel is {max_gain:.2}× its still neighborhood (cap {SILHOUETTE_GAIN_MAX}; pan {worst_moving:.1}, still {worst_bound:.1})"
        )
    } else {
        format!("silhouette gain {max_gain:.2}× stays within {SILHOUETTE_GAIN_MAX} and every pixel is finite")
    };
    SilhouetteReport {
        silhouette_pixels: edges,
        max_gain,
        non_finite,
        passed,
        reason,
    }
}

/// True when a 4-neighbor of `(x, y)` is sky in `still`.
fn sky_neighbor(still: &[[f32; 3]], width: u32, x: u32, y: u32) -> bool {
    [(-1, 0), (1, 0), (0, -1), (0, 1)].into_iter().any(|(dx, dy)| {
        let nx = x as i32 + dx;
        let ny = y as i32 + dy;
        let index = (ny as u32 * width + nx as u32) as usize;
        luma(still[index]) < SKY_LUMA
    })
}

/// Brightest luminance in the 3×3 around `(x, y)`, including the pixel itself.
fn neighborhood_max(frame: &[[f32; 3]], width: u32, height: u32, x: u32, y: u32) -> f32 {
    let mut max_l = 0.0f32;
    for dy in -1i32..=1 {
        for dx in -1i32..=1 {
            let sx = x as i32 + dx;
            let sy = y as i32 + dy;
            if sx < 0 || sy < 0 || sx >= width as i32 || sy >= height as i32 {
                continue;
            }
            max_l = max_l.max(luma(frame[(sy as u32 * width + sx as u32) as usize]));
        }
    }
    max_l
}

/// Renders the jump and the still on `device` and scores the hull edge.
///
/// # Errors
///
/// A frame or the HDR readback failed, or the readback was missing.
#[tracing::instrument(skip_all)]
pub fn run(device: &RenderDevice) -> Result<SilhouetteReport, LookdevError> {
    let _span = tracing::info_span!("black_sky_motion").entered();
    let scene = hull_scene();
    let post = post();
    let mut frame = scene.as_frame_scene_with_quality(quality());
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(airless_sky());
    renderer.set_ddgi_enabled(Some(false));
    let (eye, target) = pose(0.0);
    frame.eye = eye;
    frame.target = target;
    let _ = render_hdr(device, &mut renderer, &frame)?;
    let (eye, target) = pose(JUMP_YAW);
    frame.eye = eye;
    frame.target = target;
    let moving = render_hdr(device, &mut renderer, &frame)?;

    let mut still_quality = quality();
    still_quality.taau = false;
    let mut still_frame = scene.as_frame_scene_with_quality(still_quality);
    still_frame.post = Some(&post);
    still_frame.eye = eye;
    still_frame.target = target;
    let mut still_renderer = FrameRenderer::new();
    still_renderer.set_outdoor(airless_sky());
    still_renderer.set_ddgi_enabled(Some(false));
    // The first frame of a fresh renderer builds the sky tables. The second is the still.
    let mut still = None;
    for _ in 0..2 {
        still = Some(render_hdr(device, &mut still_renderer, &still_frame)?);
    }
    let still = still.ok_or_else(|| LookdevError::Config("the still view produced no frame".into()))?;
    if moving.width != still.width || moving.height != still.height {
        return Err(LookdevError::Config("still and moving sizes differ".into()));
    }
    Ok(score_silhouettes(
        &still.rgb,
        &moving.rgb,
        still.width,
        still.height,
    ))
}

fn render_hdr(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    frame: &regia_render::FrameScene<'_>,
) -> Result<HdrCapture, LookdevError> {
    renderer.request_hdr_capture();
    let _ = renderer.render_offscreen(device, frame)?;
    renderer
        .take_hdr_capture(device)?
        .ok_or_else(|| LookdevError::Config("the frame has no linear HDR capture".into()))
}

fn airless_sky() -> OutdoorParams {
    let mut planet = AtmosphereParameters::airless();
    // Stars are real on an airless moon. This check is the black sky behind the hull, so a
    // star pixel is not an edge. Daytime radiance 0 leaves the night field, which the sunlit
    // exposure does not lift.
    planet.day_star_radiance = 0.0;
    OutdoorParams {
        time: Some(TimeOfDay::noon()),
        planet,
        planet_surface: Some(false),
        ..OutdoorParams::default()
    }
}

fn pose(yaw: f32) -> ([f32; 3], [f32; 3]) {
    let distance = 9.0;
    let eye = [distance * yaw.sin(), 1.5, distance * yaw.cos()];
    ([eye[0], eye[1], eye[2]], [0.0, 1.2, 0.0])
}

fn hull_scene() -> LabScene {
    let (vertices, indices) = regia_render::box_mesh([1.4, 0.9, 0.35]);
    LabScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 1.5, 9.0],
        target: [0.0, 1.2, 0.0],
        fov_y: 0.7,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.15, -1.0, 0.2], [1.0, 0.98, 0.95], 80_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.55, 0.56, 0.58, 1.0],
            roughness: 0.6,
            metallic: 0.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn quality() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.ssao = SsaoMode::Off;
    quality.gi = GiMode::Ambient;
    quality.gi_probes = false;
    quality.reflection_probes = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.physical_atmosphere = true;
    quality.taau = true;
    quality.render_scale_permille = 1000;
    quality.horizontal_plus = false;
    quality.contact_shadows = false;
    quality.gpu_driven = false;
    quality.paged_sun_shadows = false;
    quality.near_sun_tile = 0;
    quality.screen_space_refraction = false;
    quality.emissive_lights = 0;
    quality.msaa_samples = 1;
    quality
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 12.0,
        bloom: true,
        taa: false,
        ..PostSettings::golden_deterministic()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgb: [f32; 3]) -> Vec<[f32; 3]> {
        vec![rgb; (w * h) as usize]
    }

    fn lit_block(w: u32, h: u32) -> Vec<[f32; 3]> {
        let mut frame = solid(w, h, [0.0, 0.0, 0.0]);
        for y in 4..28 {
            for x in 8..24 {
                frame[(y * w + x) as usize] = [2.0, 2.0, 2.0];
            }
        }
        frame
    }

    #[test]
    fn a_brightened_edge_fails_and_a_quiet_edge_passes() {
        let w = 32u32;
        let h = 32u32;
        let still = lit_block(w, h);
        let mut moving = still.clone();
        let quiet = score_silhouettes(&still, &moving, w, h);
        assert!(quiet.passed, "{}", quiet.reason);
        assert!(
            quiet.silhouette_pixels >= MIN_SILHOUETTE_PIXELS,
            "{}",
            quiet.reason
        );
        assert!(quiet.max_gain <= SILHOUETTE_GAIN_MAX, "{}", quiet.reason);
        // The lit column against the sky on its left. Lift it the way the old clamp did.
        for y in 4..28 {
            moving[(y * w + 8) as usize] = [8.0, 8.0, 8.0];
        }
        let blown = score_silhouettes(&still, &moving, w, h);
        assert!(!blown.passed, "{}", blown.reason);
        assert!(blown.max_gain > SILHOUETTE_GAIN_MAX, "{}", blown.reason);
    }

    #[test]
    fn a_pixel_as_bright_as_its_neighbor_passes() {
        // The still's inward neighbor is the bright one. A pan pixel that matches it is
        // inside the 3×3, which is what a one-pixel jitter does, not a white line.
        let w = 32u32;
        let h = 32u32;
        let mut still = lit_block(w, h);
        for y in 4..28 {
            still[(y * w + 9) as usize] = [6.0, 6.0, 6.0];
        }
        let mut moving = still.clone();
        for y in 4..28 {
            moving[(y * w + 8) as usize] = [6.0, 6.0, 6.0];
        }
        let report = score_silhouettes(&still, &moving, w, h);
        assert!(report.passed, "{}", report.reason);
        for y in 4..28 {
            moving[(y * w + 8) as usize] = [12.0, 12.0, 12.0];
        }
        let blown = score_silhouettes(&still, &moving, w, h);
        assert!(!blown.passed, "{}", blown.reason);
    }

    #[test]
    fn a_non_finite_pixel_fails() {
        let w = 32u32;
        let h = 32u32;
        let still = lit_block(w, h);
        let mut moving = still.clone();
        // Interior of the scan, not an unscored corner.
        moving[(w + 1) as usize] = [f32::NAN, 0.0, 0.0];
        let report = score_silhouettes(&still, &moving, w, h);
        assert!(!report.passed, "{}", report.reason);
        assert!(report.non_finite > 0, "{}", report.reason);
    }

    #[test]
    fn gain_cap_is_above_the_v014_sharpen_limit() {
        // v0.1.4 caps brightening at +25 % (log2(1.25) ≈ 0.32). 1.3× still fails the old
        // clamp (several times) and passes that cap.
        const {
            assert!(SILHOUETTE_GAIN_MAX > 1.25);
            assert!(SILHOUETTE_GAIN_MAX < 1.5);
        }
    }
}
