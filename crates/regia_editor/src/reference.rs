// SPDX-License-Identifier: MPL-2.0
//! A reference image in a viewport pane (the "concept ghost", Tincal F4): a picture drawn
//! behind, over, differenced against or as an outline over the lit scene, in the live panel and
//! in captures (`book/agents/reference-image.md`).
//!
//! The compositing is a CPU pass over the pane's finished pixels, run by
//! [`render_pane`](crate::render::render_pane) only when the request carries a reference, so a
//! pane without one pays nothing. `Behind` needs to know which pixels the scene's meshes cover;
//! it reads that from the entity-id buffer (meshes and proxies; terrain and runtime-renderer
//! content do not write ids, so a reference drawn `Behind` shows through them).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_ecs::prelude::*;

use crate::picking::IdBuffer;

/// How the reference combines with the scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReferenceMode {
    /// Under the scene: the reference replaces the background (pixels no mesh covers).
    Behind,
    /// Over the scene, alpha-blended.
    #[default]
    Over,
    /// `|scene - reference|` per channel: black where they agree.
    Difference,
    /// The reference's outline drawn over the scene.
    Silhouette,
}

impl ReferenceMode {
    /// The stable protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Behind => "behind",
            Self::Over => "over",
            Self::Difference => "difference",
            Self::Silhouette => "silhouette",
        }
    }

    /// The mode a protocol name denotes.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "behind" => Some(Self::Behind),
            "over" => Some(Self::Over),
            "difference" => Some(Self::Difference),
            "silhouette" => Some(Self::Silhouette),
            _ => None,
        }
    }
}

/// A reference image to draw in a pane.
#[derive(Debug, Clone)]
pub struct ReferenceImage {
    /// The picture (shared, so cloning a request is cheap). Alpha, when present, masks it.
    pub texture: Arc<egui::ColorImage>,
    /// 0 = invisible, 1 = fully applied.
    pub opacity: f32,
    /// How it combines with the scene.
    pub mode: ReferenceMode,
    /// The pane rectangle the whole image is stretched into, `[u0, v0, u1, v1]` in 0..1 with
    /// the origin top-left. `[0, 0, 1, 1]` fills the pane; a smaller rectangle letterboxes.
    pub rect_uv: [f32; 4],
}

impl ReferenceImage {
    /// A reference filling the whole pane.
    #[must_use]
    pub fn new(texture: Arc<egui::ColorImage>, mode: ReferenceMode, opacity: f32) -> Self {
        Self {
            texture,
            opacity,
            mode,
            rect_uv: [0.0, 0.0, 1.0, 1.0],
        }
    }
}

/// The references the viewport panes draw, by pane index (the live panel and the agent's
/// captures read it).
#[derive(Resource, Debug, Clone, Default)]
pub struct ViewportReferences {
    /// Pane index → its reference.
    pub panes: BTreeMap<usize, ReferenceImage>,
}

/// Outline colour of [`ReferenceMode::Silhouette`] (sRGB).
const SILHOUETTE_RGB: [f32; 3] = [255.0, 60.0, 170.0];

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Bilinear sample of `image` at texel-space `(x, y)` (pixel centres at +0.5), straight
/// (unpremultiplied) sRGBA in 0..255.
#[allow(clippy::needless_range_loop)] // channel loops over parallel arrays
fn sample(image: &egui::ColorImage, x: f32, y: f32) -> [f32; 4] {
    let w = image.width();
    let h = image.height();
    let fx = (x - 0.5).clamp(0.0, (w - 1) as f32);
    let fy = (y - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let at = |px: usize, py: usize| {
        let c = image.pixels[py * w + px].to_srgba_unmultiplied();
        [f32::from(c[0]), f32::from(c[1]), f32::from(c[2]), f32::from(c[3])]
    };
    let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
    let mut out = [0.0; 4];
    for i in 0..4 {
        out[i] = lerp(lerp(a[i], b[i], tx), lerp(c[i], d[i], tx), ty);
    }
    out
}

/// Whether a pixel of the reference belongs to its subject: its alpha when the image has any
/// transparency, else a colour distance from the mean of the four corners (a flat backdrop).
struct Subject {
    use_alpha: bool,
    backdrop: [f32; 3],
}

impl Subject {
    #[allow(clippy::needless_range_loop)] // channel loops over parallel arrays
    fn of(image: &egui::ColorImage) -> Self {
        let (w, h) = (image.width(), image.height());
        let use_alpha = image.pixels.iter().any(|p| p.a() < 250);
        let mut backdrop = [0.0; 3];
        for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
            let c = image.pixels[y * w + x].to_srgba_unmultiplied();
            for i in 0..3 {
                backdrop[i] += f32::from(c[i]) / 4.0;
            }
        }
        Self { use_alpha, backdrop }
    }

    fn contains(&self, texel: [f32; 4]) -> bool {
        if self.use_alpha {
            texel[3] >= 128.0
        } else {
            let distance: f32 = (0..3).map(|i| (texel[i] - self.backdrop[i]).abs()).sum();
            distance > 48.0
        }
    }
}

/// Draws `reference` into `frame`. `ids` is the pane's entity-id buffer, required by
/// [`ReferenceMode::Behind`] (without it the call leaves the frame alone: nothing says which
/// pixels the scene covers).
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_reference")]
#[allow(clippy::needless_range_loop)] // channel loops over parallel arrays
pub fn apply_reference(frame: &mut egui::ColorImage, ids: Option<&IdBuffer>, reference: &ReferenceImage) {
    let (w, h) = (frame.width(), frame.height());
    let texture = &reference.texture;
    if w == 0 || h == 0 || texture.width() == 0 || texture.height() == 0 {
        return;
    }
    let opacity = reference.opacity.clamp(0.0, 1.0);
    if opacity <= 0.0 {
        return;
    }
    let [u0, v0, u1, v1] = reference.rect_uv;
    let (du, dv) = (u1 - u0, v1 - v0);
    if du <= 0.0 || dv <= 0.0 {
        return;
    }
    let behind = reference.mode == ReferenceMode::Behind;
    if behind && ids.is_none_or(|ids| ids.width as usize != w || ids.height as usize != h) {
        return;
    }
    // The reference's texel at every pane pixel (None outside the rectangle).
    let texel_at = |x: usize, y: usize| -> Option<[f32; 4]> {
        let u = (x as f32 + 0.5) / w as f32;
        let v = (y as f32 + 0.5) / h as f32;
        let (tu, tv) = ((u - u0) / du, (v - v0) / dv);
        if !(0.0..=1.0).contains(&tu) || !(0.0..=1.0).contains(&tv) {
            return None;
        }
        Some(sample(
            texture,
            tu * texture.width() as f32,
            tv * texture.height() as f32,
        ))
    };
    if reference.mode == ReferenceMode::Silhouette {
        silhouette(frame, &texel_at, &Subject::of(texture), opacity);
        return;
    }
    for y in 0..h {
        for x in 0..w {
            if behind && ids.is_some_and(|ids| ids.ids[y * w + x] != 0) {
                continue;
            }
            let Some(texel) = texel_at(x, y) else { continue };
            let weight = opacity * texel[3] / 255.0;
            if weight <= 0.0 {
                continue;
            }
            let scene = frame.pixels[y * w + x].to_srgba_unmultiplied();
            let mut out = [0u8; 4];
            for i in 0..3 {
                let s = f32::from(scene[i]);
                let target = if reference.mode == ReferenceMode::Difference {
                    (s - texel[i]).abs()
                } else {
                    texel[i]
                };
                out[i] = lerp(s, target, weight).round().clamp(0.0, 255.0) as u8;
            }
            out[3] = scene[3];
            frame.pixels[y * w + x] = egui::Color32::from_rgba_unmultiplied(out[0], out[1], out[2], out[3]);
        }
    }
}

/// Outlines the reference's subject: subject pixels with a 4-neighbour outside it (the edge of
/// the rectangle counts as outside).
#[allow(clippy::needless_range_loop)] // channel loops over parallel arrays
fn silhouette(
    frame: &mut egui::ColorImage,
    texel_at: &impl Fn(usize, usize) -> Option<[f32; 4]>,
    subject: &Subject,
    opacity: f32,
) {
    let (w, h) = (frame.width(), frame.height());
    let mask: Vec<bool> = (0..w * h)
        .map(|i| texel_at(i % w, i / w).is_some_and(|t| subject.contains(t)))
        .collect();
    let inside = |x: isize, y: isize| {
        x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h && mask[y as usize * w + x as usize]
    };
    for y in 0..h {
        for x in 0..w {
            if !mask[y * w + x] {
                continue;
            }
            let (xi, yi) = (x as isize, y as isize);
            let edge =
                !(inside(xi - 1, yi) && inside(xi + 1, yi) && inside(xi, yi - 1) && inside(xi, yi + 1));
            if !edge {
                continue;
            }
            let scene = frame.pixels[y * w + x].to_srgba_unmultiplied();
            let mut out = scene;
            for i in 0..3 {
                out[i] = lerp(f32::from(scene[i]), SILHOUETTE_RGB[i], opacity)
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
            frame.pixels[y * w + x] = egui::Color32::from_rgba_unmultiplied(out[0], out[1], out[2], out[3]);
        }
    }
}

/// Decodes an image file's bytes (PNG, JPEG, …) into a reference texture.
///
/// # Errors
///
/// The bytes are not a supported image.
pub fn decode_image(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    let rgba = image::load_from_memory(bytes)
        .map_err(|err| format!("not a decodable image: {err}"))?
        .to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Standard base64 (RFC 4648 § 4); whitespace and padding are ignored.
///
/// # Errors
///
/// A character outside the alphabet.
pub fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b' ' | b'\n' | b'\r' | b'\t' => continue,
            other => return Err(format!("invalid base64 character `{}`", other as char)),
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, rgba: [u8; 4]) -> egui::ColorImage {
        egui::ColorImage::filled(
            [w, h],
            egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]),
        )
    }

    fn px(image: &egui::ColorImage, x: usize, y: usize) -> [u8; 4] {
        image.pixels[y * image.width() + x].to_srgba_unmultiplied()
    }

    fn reference(texture: egui::ColorImage, mode: ReferenceMode, opacity: f32) -> ReferenceImage {
        ReferenceImage::new(Arc::new(texture), mode, opacity)
    }

    /// An id buffer covering the left half of a `w`×`h` pane.
    fn left_half_ids(w: usize, h: usize) -> IdBuffer {
        let mut ids = vec![0; w * h];
        for y in 0..h {
            for x in 0..w / 2 {
                ids[y * w + x] = 1;
            }
        }
        IdBuffer {
            width: w as u32,
            height: h as u32,
            ids,
            distance: vec![1.0; w * h],
            entities: Vec::new(),
        }
    }

    #[test]
    fn over_blends_by_opacity() {
        let mut frame = solid(8, 8, [100, 100, 100, 255]);
        apply_reference(
            &mut frame,
            None,
            &reference(solid(4, 4, [200, 0, 0, 255]), ReferenceMode::Over, 0.5),
        );
        assert_eq!(px(&frame, 3, 3), [150, 50, 50, 255]);
    }

    #[test]
    fn zero_opacity_and_empty_rect_leave_the_frame() {
        let before = solid(8, 8, [10, 20, 30, 255]);
        let mut frame = before.clone();
        apply_reference(
            &mut frame,
            None,
            &reference(solid(2, 2, [255; 4]), ReferenceMode::Over, 0.0),
        );
        let mut empty = reference(solid(2, 2, [255; 4]), ReferenceMode::Over, 1.0);
        empty.rect_uv = [0.5, 0.5, 0.5, 0.9];
        apply_reference(&mut frame, None, &empty);
        assert_eq!(frame.pixels, before.pixels);
    }

    #[test]
    fn rect_uv_letterboxes() {
        let mut frame = solid(8, 8, [0, 0, 0, 255]);
        let mut r = reference(solid(2, 2, [255, 255, 255, 255]), ReferenceMode::Over, 1.0);
        r.rect_uv = [0.5, 0.0, 1.0, 1.0];
        apply_reference(&mut frame, None, &r);
        assert_eq!(px(&frame, 1, 4), [0, 0, 0, 255]);
        assert_eq!(px(&frame, 6, 4), [255, 255, 255, 255]);
    }

    #[test]
    fn behind_shows_only_where_no_mesh_covers() {
        let mut frame = solid(8, 8, [50, 60, 70, 255]);
        let ids = left_half_ids(8, 8);
        apply_reference(
            &mut frame,
            Some(&ids),
            &reference(solid(4, 4, [255, 0, 0, 255]), ReferenceMode::Behind, 1.0),
        );
        assert_eq!(
            px(&frame, 1, 4),
            [50, 60, 70, 255],
            "covered pixels keep the scene"
        );
        assert_eq!(
            px(&frame, 6, 4),
            [255, 0, 0, 255],
            "background shows the reference"
        );
    }

    #[test]
    fn behind_without_ids_changes_nothing() {
        let before = solid(8, 8, [50, 60, 70, 255]);
        let mut frame = before.clone();
        apply_reference(
            &mut frame,
            None,
            &reference(solid(4, 4, [255, 0, 0, 255]), ReferenceMode::Behind, 1.0),
        );
        assert_eq!(frame.pixels, before.pixels);
    }

    #[test]
    fn difference_is_black_when_equal_and_absolute_otherwise() {
        let mut frame = solid(8, 8, [100, 150, 200, 255]);
        apply_reference(
            &mut frame,
            None,
            &reference(solid(4, 4, [100, 150, 200, 255]), ReferenceMode::Difference, 1.0),
        );
        assert_eq!(px(&frame, 4, 4), [0, 0, 0, 255]);
        let mut frame = solid(8, 8, [100, 150, 200, 255]);
        apply_reference(
            &mut frame,
            None,
            &reference(solid(4, 4, [130, 100, 255, 255]), ReferenceMode::Difference, 1.0),
        );
        assert_eq!(px(&frame, 4, 4), [30, 50, 55, 255]);
    }

    /// A 16×16 reference: opaque white square on a transparent field.
    fn square() -> egui::ColorImage {
        let mut image = solid(16, 16, [0, 0, 0, 0]);
        for y in 4..12 {
            for x in 4..12 {
                image.pixels[y * 16 + x] = egui::Color32::WHITE;
            }
        }
        image
    }

    #[test]
    fn silhouette_outlines_the_subject_and_leaves_the_interior() {
        let mut frame = solid(16, 16, [0, 0, 0, 255]);
        apply_reference(
            &mut frame,
            None,
            &reference(square(), ReferenceMode::Silhouette, 1.0),
        );
        assert_eq!(px(&frame, 4, 8), [255, 60, 170, 255], "edge pixel");
        assert_eq!(px(&frame, 8, 8), [0, 0, 0, 255], "interior untouched");
        assert_eq!(px(&frame, 1, 1), [0, 0, 0, 255], "background untouched");
    }

    #[test]
    fn silhouette_of_an_opaque_image_uses_the_backdrop_colour() {
        let mut image = solid(16, 16, [20, 20, 20, 255]);
        for y in 4..12 {
            for x in 4..12 {
                image.pixels[y * 16 + x] = egui::Color32::from_rgb(220, 220, 220);
            }
        }
        let mut frame = solid(16, 16, [0, 0, 0, 255]);
        apply_reference(
            &mut frame,
            None,
            &reference(image, ReferenceMode::Silhouette, 1.0),
        );
        assert_eq!(px(&frame, 4, 8), [255, 60, 170, 255]);
        assert_eq!(px(&frame, 8, 8), [0, 0, 0, 255]);
    }

    #[test]
    fn transparent_reference_pixels_do_not_draw() {
        let mut frame = solid(16, 16, [9, 9, 9, 255]);
        apply_reference(&mut frame, None, &reference(square(), ReferenceMode::Over, 1.0));
        assert_eq!(px(&frame, 1, 1), [9, 9, 9, 255]);
        assert_eq!(px(&frame, 8, 8), [255, 255, 255, 255]);
    }

    #[test]
    fn base64_round_trips_and_rejects_garbage() {
        assert_eq!(base64_decode("Zm9vYmFy").expect("ok"), b"foobar");
        assert_eq!(base64_decode("Zm9vYg==").expect("ok"), b"foob");
        assert_eq!(base64_decode("Zm9v\nYmE=").expect("ok"), b"fooba");
        assert!(base64_decode("Zm9v!").is_err());
    }

    #[test]
    fn mode_names_round_trip() {
        for mode in [
            ReferenceMode::Behind,
            ReferenceMode::Over,
            ReferenceMode::Difference,
            ReferenceMode::Silhouette,
        ] {
            assert_eq!(ReferenceMode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(ReferenceMode::from_name("x"), None);
    }
}
