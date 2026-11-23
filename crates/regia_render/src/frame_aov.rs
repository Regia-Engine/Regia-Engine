// SPDX-License-Identifier: MPL-2.0
//! Normal and roughness AOVs of the real frame (ADR-2951, `specs/27` § 2.2 "Terrain relief" and
//! "Decals").
//!
//! The forward frame has no G-buffer. [`capture_frame_aov`] renders the frame twice more through
//! the forward shader's own debug outputs (`FrameQuality::shadow_debug`): `5` writes the shading
//! normal every light of the pixel used (normal map, detail normal and any material input the
//! forward pass applies), `7` the perceptual roughness and metallic it used. Both outputs skip the
//! post stack, so the readback is the shader's value through the 8-bit unorm offscreen target
//! after the debug branch's own 2.2-power encode ([`gamma22_to_linear`]; normal error about 0.5°,
//! roughness about 1/255 near 0 and 1/115 near 1).
//!
//! What the AOV holds is what the forward pass shaded with: inputs that change the lit colour
//! after the forward pass (the projected decal layer, which relights the composited colour from
//! its own normal) are not in it, and the translucent layers sit out the two capture frames so
//! they do not composite over the debug output. `book/agents/frame-aov.md` and the spec 27
//! evidence page say what that means for the decal row.
//!
//! The capture is a test-and-capture path (the agent's `seeing.frame_aov`, the spec 27 suite):
//! two extra frames, never per game frame. The renderer's temporal history sees the debug frames;
//! capture before the beauty frame, or on a renderer kept for the capture.

use crate::{FrameRenderer, FrameScene, RenderDevice, RenderError};

/// `FrameQuality::shadow_debug` value of the shading-normal output (`n * 0.5 + 0.5`).
pub const NORMAL_DEBUG_MODE: u32 = 5;
/// `FrameQuality::shadow_debug` value of the roughness output (R perceptual roughness,
/// G metallic, B 0).
pub const ROUGHNESS_DEBUG_MODE: u32 = 7;

/// The frame's shading normal and roughness per pixel, row-major, top row first.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameAov {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// World shading normal (unit length where [`Self::coverage`] is set).
    pub normal: Vec<[f32; 3]>,
    /// Perceptual roughness, 0..=1.
    pub roughness: Vec<f32>,
    /// Metallic, 0..=1.
    pub metallic: Vec<f32>,
    /// Pixels the forward pass shaded: the decoded normal is unit length (within 10 %) and the
    /// roughness pass wrote a zero blue channel. Sky and background pixels are clear.
    pub coverage: Vec<bool>,
}

/// Mean and RMS angle (degrees) between `a` and `b` over `mask`: `(mean, rms, pixels)`.
#[must_use]
pub fn angular_error_deg(a: &[[f32; 3]], b: &[[f32; 3]], mask: &[bool]) -> (f32, f32, usize) {
    let mut sum = 0.0f64;
    let mut sq = 0.0f64;
    let mut n = 0usize;
    for ((x, y), m) in a.iter().zip(b).zip(mask) {
        if !*m {
            continue;
        }
        let lx = (x[0] * x[0] + x[1] * x[1] + x[2] * x[2]).sqrt();
        let ly = (y[0] * y[0] + y[1] * y[1] + y[2] * y[2]).sqrt();
        if lx <= 0.0 || ly <= 0.0 {
            continue;
        }
        let c = ((x[0] * y[0] + x[1] * y[1] + x[2] * y[2]) / (lx * ly)).clamp(-1.0, 1.0);
        let deg = f64::from(c.acos().to_degrees());
        sum += deg;
        sq += deg * deg;
        n += 1;
    }
    if n == 0 {
        return (0.0, 0.0, 0);
    }
    ((sum / n as f64) as f32, (sq / n as f64).sqrt() as f32, n)
}

/// RGBA8 sRGB pixels decoded to linear `[0, 1]` RGB (IEC 61966-2-1; alpha dropped): the value a
/// debug view wrote into an sRGB target before the hardware encode (the terrain's
/// `render_normal_view`).
#[must_use]
pub fn srgb8_to_linear(rgba: &[u8]) -> Vec<[f32; 3]> {
    let decode = |b: u8| {
        let c = f32::from(b) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    rgba.as_chunks::<4>()
        .0
        .iter()
        .map(|p| [decode(p[0]), decode(p[1]), decode(p[2])])
        .collect()
}

/// RGBA8 pixels a 2.2 power encoded (`regia::utils::encode_srgb`, what the forward shader's debug
/// branch writes into the 8-bit unorm offscreen target) decoded to linear `[0, 1]` RGB.
#[must_use]
pub fn gamma22_to_linear(rgba: &[u8]) -> Vec<[f32; 3]> {
    let decode = |b: u8| (f32::from(b) / 255.0).powf(2.2);
    rgba.as_chunks::<4>()
        .0
        .iter()
        .map(|p| [decode(p[0]), decode(p[1]), decode(p[2])])
        .collect()
}

/// Decodes the two debug outputs as `render_offscreen` reads them back (RGBA8, the forward
/// shader's 2.2-power encode, [`gamma22_to_linear`]).
///
/// # Errors
///
/// The buffers do not hold `width × height` pixels.
pub fn decode_frame_aov(
    width: u32,
    height: u32,
    normal_rgba: &[u8],
    rough_rgba: &[u8],
) -> Result<FrameAov, RenderError> {
    let n = width as usize * height as usize;
    if normal_rgba.len() != n * 4 || rough_rgba.len() != n * 4 {
        return Err(RenderError::Readback(format!(
            "frame AOV: {} and {} bytes for {width}x{height}",
            normal_rgba.len(),
            rough_rgba.len()
        )));
    }
    let nl = gamma22_to_linear(normal_rgba);
    let rl = gamma22_to_linear(rough_rgba);
    let mut aov = FrameAov {
        width,
        height,
        normal: Vec::with_capacity(n),
        roughness: Vec::with_capacity(n),
        metallic: Vec::with_capacity(n),
        coverage: Vec::with_capacity(n),
    };
    for ((e, r), raw) in nl.iter().zip(&rl).zip(rough_rgba.as_chunks::<4>().0) {
        let v = [e[0] * 2.0 - 1.0, e[1] * 2.0 - 1.0, e[2] * 2.0 - 1.0];
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        let unit = (0.9..=1.1).contains(&len);
        aov.normal.push(if len > 0.0 {
            [v[0] / len, v[1] / len, v[2] / len]
        } else {
            [0.0; 3]
        });
        aov.roughness.push(r[0]);
        aov.metallic.push(r[1]);
        // Mode 7 writes blue 0 exactly: compare the stored byte, since after the decode any byte
        // up to 14 is below 0.5/255 and a pipeline that ignores the mode draws its beauty colour.
        aov.coverage.push(unit && raw[2] == 0);
    }
    Ok(aov)
}

/// Renders `scene` through the forward shader's normal and roughness outputs on `renderer` and
/// decodes them. The scene's other settings stay as given (use the beauty frame's quality, so the
/// same materials, maps and LOD shade).
///
/// # Errors
///
/// A frame failed to render or read back.
pub fn capture_frame_aov(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    scene: &FrameScene<'_>,
) -> Result<FrameAov, RenderError> {
    let _span = tracing::info_span!("frame_aov_capture").entered();
    let mut debug = scene.clone();
    // The debug outputs skip the post stack; a debug frame that still names post settings stops
    // in the post stack with no HDR view (`frame/post_stack.rs`), so the two frames carry none.
    debug.post = None;
    // The translucent layers (particles, the projected decal pass) would composite over the debug
    // output as over a colour: they sit out the two frames and come back after.
    let layers = renderer.translucent_layers().to_vec();
    renderer.set_translucent_layers(Vec::new());
    // The lite forward shader of the Mobile tiers (tile GPUs) never reads the debug mode: a
    // lite-tier capture would decode lit colour as normals and roughness. The two frames force
    // the full shader (its normal/roughness inputs are the ones every tier shades with), then
    // the renderer's tier choice comes back.
    let tier = renderer.forward_lite_override();
    renderer.set_forward_lite(Some(crate::forward_lite::ForwardLite::Off));
    debug.quality.shadow_debug = NORMAL_DEBUG_MODE;
    let normal = renderer.render_offscreen(device, &debug);
    debug.quality.shadow_debug = ROUGHNESS_DEBUG_MODE;
    let rough = normal
        .as_ref()
        .ok()
        .map(|_| renderer.render_offscreen(device, &debug));
    renderer.set_translucent_layers(layers);
    renderer.set_forward_lite(tier);
    let normal = normal?;
    let rough =
        rough.unwrap_or_else(|| Err(RenderError::Readback("frame AOV: no roughness frame".into())))?;
    decode_frame_aov(normal.width, normal.height, &normal.pixels, &rough.pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The forward debug branch's encode: `regia::utils::encode_srgb`, a 2.2 power.
    fn encode(v: f32) -> u8 {
        (v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8
    }

    #[test]
    fn the_srgb_decode_is_the_exact_curve() {
        let px = [0u8, 10, 188, 255, 255, 255, 255, 255];
        let lin = srgb8_to_linear(&px);
        assert_eq!(lin[0][0], 0.0);
        assert!((lin[0][1] - 10.0 / 255.0 / 12.92).abs() < 1e-6);
        assert!((lin[0][2] - 0.5029).abs() < 1e-3, "{}", lin[0][2]);
        assert!((lin[1][0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn decodes_a_tilted_normal_and_a_roughness_within_quantization() {
        let n = [0.3f32, 0.9, -0.3162];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        let n = n.map(|c| c / len);
        let px_n = [
            encode(n[0] * 0.5 + 0.5),
            encode(n[1] * 0.5 + 0.5),
            encode(n[2] * 0.5 + 0.5),
            255,
        ];
        let px_r = [encode(0.35), encode(1.0), 0, 255];
        // Background: a sky colour, blue set.
        let sky = [40u8, 80, 200, 255];
        let normal_rgba = [px_n, sky].concat();
        let rough_rgba = [px_r, sky].concat();
        let aov = decode_frame_aov(2, 1, &normal_rgba, &rough_rgba).expect("sizes");
        assert_eq!(aov.coverage, vec![true, false]);
        let (_, rms, pixels) = angular_error_deg(&aov.normal, &[n, n], &aov.coverage);
        assert_eq!(pixels, 1);
        assert!(rms < 0.5, "{rms}");
        assert!(
            (aov.roughness[0] - 0.35).abs() < 1.0 / 255.0,
            "{}",
            aov.roughness[0]
        );
        assert!((aov.metallic[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_beauty_pixel_with_a_low_blue_byte_is_not_covered() {
        // A pipeline that ignores mode 7 draws its beauty colour in both frames. Bytes
        // (186, 186, 10) decode to a near-unit normal, and blue byte 10 decodes below 0.5/255.
        let px = [186u8, 186, 10, 255];
        let aov = decode_frame_aov(1, 1, &px, &px).expect("sizes");
        let n = aov.normal[0];
        let decoded_len = {
            let e = gamma22_to_linear(&px)[0];
            let v = e.map(|c| c * 2.0 - 1.0);
            (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
        };
        assert!((0.9..=1.1).contains(&decoded_len), "{decoded_len} {n:?}");
        assert!(gamma22_to_linear(&px)[0][2] < 0.5 / 255.0);
        assert_eq!(aov.coverage, vec![false]);
    }

    #[test]
    fn wrong_sizes_are_an_error() {
        assert!(decode_frame_aov(2, 2, &[0; 4], &[0; 16]).is_err());
    }

    #[test]
    fn angle_of_perpendicular_normals_is_ninety() {
        let (mean, rms, n) = angular_error_deg(&[[1.0, 0.0, 0.0]], &[[0.0, 1.0, 0.0]], &[true]);
        assert_eq!(n, 1);
        assert!((mean - 90.0).abs() < 1e-3 && (rms - 90.0).abs() < 1e-3);
    }
}
