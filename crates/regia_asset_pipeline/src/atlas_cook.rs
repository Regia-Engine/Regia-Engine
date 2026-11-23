// SPDX-License-Identifier: MPL-2.0
//! Sprite-atlas cook (`docs/specs/22-tier2.md` § T2.10): `.atlas.ron` → cooked
//! [`regia_2d::SpriteAtlas`].
//!
//! The source images (whole, or cut into grid cells) are packed with `texture_packer`'s skyline
//! packer into as many pages of at most `max_size` pixels as needed. Packing order is fixed
//! (tallest first, then widest, then by name), so the same sources always give the same pages. The
//! packer only places rectangles; the pages are composed here: each (optionally trimmed) sprite is
//! copied to its slot and its edge pixels are repeated `extrude` pixels outwards. Pages stay
//! uncompressed RGBA8 so pixel art is exact.

use std::path::Path;

use regia_2d::atlas::{AtlasSource, AtlasSprite, COOKED_ATLAS_VERSION, NineSlice, RgbaImage, SpriteAtlas};
use texture_packer::texture::Texture;
use texture_packer::{MultiTexturePacker, TexturePackerConfig};

/// Atlas cook errors.
#[derive(Debug, thiserror::Error)]
pub enum AtlasCookError {
    /// The `.atlas.ron` does not parse or validate.
    #[error("{0}")]
    Source(#[from] regia_2d::atlas::AtlasError),
    /// A source image is missing or not decodable.
    #[error("sprite `{name}`: image `{path}`: {reason}")]
    Image {
        /// Sprite name.
        name: String,
        /// Image path as written.
        path: String,
        /// What went wrong.
        reason: String,
    },
    /// A sprite does not fit a page, or the packer refused it.
    #[error("sprite `{name}` cannot be packed: {reason}")]
    Pack {
        /// Sprite name.
        name: String,
        /// What went wrong.
        reason: String,
    },
}

/// One image to pack.
pub(crate) struct Entry {
    name: String,
    image: image::RgbaImage,
    pivot: [f32; 2],
    nine_slice: Option<NineSlice>,
}

impl Entry {
    /// An entry from an already-decoded image (the Aseprite importer's strips).
    pub(crate) fn decoded(
        name: String,
        image: image::RgbaImage,
        pivot: [f32; 2],
        nine_slice: Option<NineSlice>,
    ) -> Self {
        Self {
            name,
            image,
            pivot,
            nine_slice,
        }
    }
}

/// `svg_import::atlas_rasters` as a message; without the `svg_import` feature the sprite is the
/// atlas's error, `importer_disabled` (ADR-7972).
fn svg_atlas_rasters(bytes: &[u8]) -> Result<Vec<crate::svg_import::SvgRaster>, String> {
    #[cfg(feature = "svg_import")]
    {
        crate::svg_import::atlas_rasters(bytes).map_err(|e| e.to_string())
    }
    #[cfg(not(feature = "svg_import"))]
    {
        let _ = bytes;
        Err(crate::disabled_importer("svg")
            .map(|e| e.to_string())
            .unwrap_or_default())
    }
}

/// Loads every source image of `source` (paths relative to `dir`), cutting grids into cells.
fn load_entries(source: &AtlasSource, dir: &Path) -> Result<Vec<Entry>, AtlasCookError> {
    let mut entries = Vec::new();
    for s in &source.sprites {
        let image_err = |reason: String| AtlasCookError::Image {
            name: s.name.clone(),
            path: s.image.clone(),
            reason,
        };
        let path = crate::cook::join_portable(dir, &s.image).map_err(image_err)?;
        let bytes = std::fs::read(&path).map_err(|e| image_err(e.to_string()))?;
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
        {
            // ADR-3350: an SVG sprite becomes one sprite per UI scale, named `name@<height px>`
            // (the icon atlas's size suffix), each rasterized at its own size so icons stay sharp.
            if s.grid.is_some() {
                let e = crate::svg_import::SvgImportError::AtlasUnsupported(
                    "an SVG sprite cannot be cut into a grid".into(),
                );
                return Err(image_err(e.to_string()));
            }
            let rasters = svg_atlas_rasters(&bytes).map_err(image_err)?;
            for (raster, k) in rasters.into_iter().zip(crate::svg_import::SVG_ATLAS_SCALES) {
                // 9-slice borders are written for 1x: they grow with the scale.
                let nine_slice = s.nine_slice.map(|b| NineSlice::from_array(b.map(|v| v * k)));
                let name = format!("{}@{}", s.name, raster.height);
                let Some(image) = image::RgbaImage::from_raw(raster.width, raster.height, raster.rgba8)
                else {
                    return Err(image_err(
                        "the rasterized sprite has the wrong byte length".into(),
                    ));
                };
                entries.push(Entry {
                    name,
                    image,
                    pivot: s.pivot,
                    nine_slice,
                });
            }
            continue;
        }
        let image = image::load_from_memory(&bytes)
            .map_err(|e| image_err(e.to_string()))?
            .to_rgba8();
        let nine_slice = s.nine_slice.map(NineSlice::from_array);
        match s.grid {
            None => entries.push(Entry {
                name: s.name.clone(),
                image,
                pivot: s.pivot,
                nine_slice,
            }),
            Some([cw, ch]) => {
                let (cols, rows) = (image.width() / cw, image.height() / ch);
                if cols == 0 || rows == 0 {
                    return Err(image_err(format!(
                        "{}x{} is smaller than one {cw}x{ch} grid cell",
                        image.width(),
                        image.height()
                    )));
                }
                for row in 0..rows {
                    for col in 0..cols {
                        let cell = image::imageops::crop_imm(&image, col * cw, row * ch, cw, ch).to_image();
                        entries.push(Entry {
                            name: format!("{}_{}", s.name, row * cols + col),
                            image: cell,
                            pivot: s.pivot,
                            nine_slice,
                        });
                    }
                }
            }
        }
    }
    Ok(entries)
}

/// Packs entries (source-file sprites or importer strips) into a cooked [`SpriteAtlas`]:
/// tallest first, then widest, then by name (deterministic), trimming and extruding as asked.
pub(crate) fn pack_entries(
    mut entries: Vec<Entry>,
    max_size: u32,
    padding: u32,
    extrude: u32,
    trim: bool,
) -> Result<SpriteAtlas, AtlasCookError> {
    // Tallest first, then widest, then by name: a good skyline order, and deterministic.
    entries.sort_by(|a, b| {
        b.image
            .height()
            .cmp(&a.image.height())
            .then(b.image.width().cmp(&a.image.width()))
            .then(a.name.cmp(&b.name))
    });
    let mut names = std::collections::BTreeSet::new();
    for e in &entries {
        if !names.insert(e.name.as_str()) {
            return Err(AtlasCookError::Pack {
                name: e.name.clone(),
                reason: "a sprite name collides with another sprite".into(),
            });
        }
    }

    // Trim ourselves: texture_packer refuses fully transparent images when trimming, and an empty
    // grid cell is legitimate (it becomes a 1×1 transparent sprite of the right source size).
    let trims: Vec<[u32; 4]> = entries
        .iter()
        .map(|e| {
            let full = [0, 0, e.image.width(), e.image.height()];
            if trim && e.nine_slice.is_none() {
                opaque_bounds(&e.image).unwrap_or([0, 0, 1.min(full[2]), 1.min(full[3])])
            } else {
                full
            }
        })
        .collect();
    let trimmed: Vec<image::RgbaImage> = entries
        .iter()
        .zip(&trims)
        .map(|(e, &[x, y, w, h])| image::imageops::crop_imm(&e.image, x, y, w, h).to_image())
        .collect();

    let config = TexturePackerConfig {
        max_width: max_size,
        max_height: max_size,
        allow_rotation: false,
        force_max_dimensions: false,
        border_padding: 0,
        texture_padding: padding,
        texture_extrusion: 0,
        trim: false,
        texture_outlines: false,
    };
    let mut packer: MultiTexturePacker<'_, image::RgbaImage, usize> = MultiTexturePacker::new_skyline(config);
    for (index, image) in trimmed.iter().enumerate() {
        let e = &entries[index];
        // Leave room for the extrusion on every side.
        let padded = image::RgbaImage::new(image.width() + 2 * extrude, image.height() + 2 * extrude);
        packer
            .pack_own(index, padded)
            .map_err(|err| AtlasCookError::Pack {
                name: e.name.clone(),
                reason: format!("{err:?} (max_size {max_size})"),
            })?;
    }

    let mut pages = Vec::new();
    let mut sprites = Vec::new();
    for (page_index, page) in packer.get_pages().iter().enumerate() {
        let (w, h) = (page.width().max(1), page.height().max(1));
        let mut out = RgbaImage {
            width: w,
            height: h,
            rgba8: vec![0; (w as usize) * (h as usize) * 4],
        };
        let mut frames: Vec<_> = page.get_frames().values().collect();
        frames.sort_by_key(|f| f.key);
        for frame in frames {
            let index = frame.key;
            let (e, image, trim) = (&entries[index], &trimmed[index], trims[index]);
            let x = frame.frame.x + extrude;
            let y = frame.frame.y + extrude;
            blit(
                &mut out,
                image,
                [0, 0, image.width(), image.height()],
                x,
                y,
                extrude,
            );
            sprites.push(AtlasSprite {
                name: e.name.clone(),
                page: page_index as u32,
                rect: [x, y, image.width(), image.height()],
                source_size: [e.image.width(), e.image.height()],
                trim_offset: [trim[0], trim[1]],
                pivot: e.pivot,
                nine_slice: e.nine_slice,
            });
        }
        pages.push(out);
    }
    if sprites.len() != entries.len() {
        return Err(AtlasCookError::Pack {
            name: entries.first().map(|e| e.name.clone()).unwrap_or_default(),
            reason: format!("the packer placed {} of {} sprites", sprites.len(), entries.len()),
        });
    }
    sprites.sort_by(|a, b| a.name.cmp(&b.name));
    let atlas = SpriteAtlas {
        version: COOKED_ATLAS_VERSION,
        pages,
        sprites,
    };
    atlas.validate()?;
    Ok(atlas)
}

/// Packs an `.atlas.ron` (its text and directory) into a cooked atlas.
pub fn cook_atlas(text: &str, dir: &Path) -> Result<SpriteAtlas, AtlasCookError> {
    let source = AtlasSource::from_ron(text)?;
    let entries = load_entries(&source, dir)?;
    pack_entries(
        entries,
        source.max_size,
        source.padding,
        source.extrude,
        source.trim,
    )
}

/// Copies `src`'s `rect` to `(x, y)` of `page` and repeats its border pixels `extrude` pixels out.
fn blit(page: &mut RgbaImage, src: &image::RgbaImage, rect: [u32; 4], x: u32, y: u32, extrude: u32) {
    let [sx, sy, w, h] = rect;
    if w == 0 || h == 0 {
        return;
    }
    let e = extrude as i64;
    for dy in -e..(i64::from(h) + e) {
        for dx in -e..(i64::from(w) + e) {
            let (px, py) = (i64::from(x) + dx, i64::from(y) + dy);
            if px < 0 || py < 0 || px >= i64::from(page.width) || py >= i64::from(page.height) {
                continue;
            }
            let cx = dx.clamp(0, i64::from(w) - 1) as u32;
            let cy = dy.clamp(0, i64::from(h) - 1) as u32;
            let texel = src.get_pixel(sx + cx, sy + cy).0;
            let at = ((py as u64 * u64::from(page.width) + px as u64) * 4) as usize;
            page.rgba8[at..at + 4].copy_from_slice(&texel);
        }
    }
}

/// The opaque bounds `[x, y, w, h]` of `image` (alpha > 0), or `None` when fully transparent.
fn opaque_bounds(image: &image::RgbaImage) -> Option<[u32; 4]> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, p) in image.enumerate_pixels() {
        if p.0[3] > 0 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + 1);
            y1 = y1.max(y + 1);
        }
    }
    (x0 < x1).then(|| [x0, y0, x1 - x0, y1 - y0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(path: &Path, w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba(f(x, y)));
        assert!(img.save(path).is_ok());
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("regia-atlas-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(std::fs::create_dir_all(&d).is_ok());
        d
    }

    #[test]
    fn packs_sprites_grids_and_keeps_pixels_exact() {
        let d = dir("basic");
        write_png(&d.join("a.png"), 5, 3, |x, y| {
            [x as u8 * 10, y as u8 * 20, 7, 255]
        });
        write_png(&d.join("sheet.png"), 8, 4, |x, _| {
            [if x < 4 { 255 } else { 0 }, 0, 0, 255]
        });
        write_png(&d.join("panel.png"), 6, 6, |_, _| [1, 2, 3, 255]);
        let text = r#"(version: 1, padding: 2, sprites: [
            (name: "a", image: "a.png", pivot: (0.5, 0.0)),
            (name: "run", image: "sheet.png", grid: Some((4, 4))),
            (name: "panel", image: "panel.png", nine_slice: Some((2, 2, 2, 2))),
        ])"#;
        let atlas = cook_atlas(text, &d).expect("cooks");
        let names: Vec<&str> = atlas.sprites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "panel", "run_0", "run_1"]);
        let a = atlas.sprite("a").expect("a");
        assert_eq!(a.pivot, [0.5, 0.0]);
        let page = &atlas.pages[a.page as usize];
        let [x, y, w, h] = a.rect;
        assert_eq!((w, h), (5, 3));
        for py in 0..h {
            for px in 0..w {
                let at = (((y + py) * page.width + x + px) * 4) as usize;
                assert_eq!(&page.rgba8[at..at + 4], &[px as u8 * 10, py as u8 * 20, 7, 255]);
            }
        }
        let run1 = atlas.sprite("run_1").expect("cell");
        let at = ((run1.rect[1] * page.width + run1.rect[0]) * 4) as usize;
        assert_eq!(page.rgba8[at], 0, "second cell is the right half of the sheet");
        assert!(atlas.sprite("panel").is_some_and(|p| p.nine_slice.is_some()));
        // Rectangles never overlap (with padding).
        for (i, s) in atlas.sprites.iter().enumerate() {
            for t in &atlas.sprites[i + 1..] {
                let apart = s.rect[0] + s.rect[2] <= t.rect[0]
                    || t.rect[0] + t.rect[2] <= s.rect[0]
                    || s.rect[1] + s.rect[3] <= t.rect[1]
                    || t.rect[1] + t.rect[3] <= s.rect[1];
                assert!(apart || s.page != t.page, "{} overlaps {}", s.name, t.name);
            }
        }
        // Deterministic.
        assert_eq!(cook_atlas(text, &d).expect("again"), atlas);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn trim_keeps_source_size_and_offset_and_extrude_repeats_edges() {
        let d = dir("trim");
        write_png(&d.join("dot.png"), 10, 10, |x, y| {
            if (3..6).contains(&x) && (4..6).contains(&y) {
                [9, 9, 9, 255]
            } else {
                [0; 4]
            }
        });
        let text = r#"(version: 1, trim: true, extrude: 1, sprites: [(name: "dot", image: "dot.png")])"#;
        let atlas = cook_atlas(text, &d).expect("cooks");
        let dot = atlas.sprite("dot").expect("dot");
        assert_eq!((dot.source_size, dot.trim_offset), ([10, 10], [3, 4]));
        assert_eq!((dot.rect[2], dot.rect[3]), (3, 2));
        let page = &atlas.pages[0];
        let left = (((dot.rect[1]) * page.width + dot.rect[0] - 1) * 4) as usize;
        assert_eq!(&page.rgba8[left..left + 4], &[9, 9, 9, 255], "extruded edge");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn errors_name_the_sprite() {
        let d = dir("err");
        let missing = r#"(version: 1, sprites: [(name: "ghost", image: "nope.png")])"#;
        let err = cook_atlas(missing, &d).expect_err("missing image");
        assert!(err.to_string().contains("ghost"));
        write_png(&d.join("big.png"), 40, 40, |_, _| [1; 4]);
        let too_big = r#"(version: 1, max_size: 32, sprites: [(name: "big", image: "big.png")])"#;
        assert!(matches!(
            cook_atlas(too_big, &d),
            Err(AtlasCookError::Pack { .. })
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8" viewBox="0 0 8 8">
        <rect x="2" y="2" width="4" height="4" fill="#ff0000"/></svg>"##;

    /// ADR-7972: without the `svg_import` feature an SVG sprite is the atlas's error, with the
    /// `importer_disabled` code and the sprite's name, never a silently missing sprite.
    #[cfg(not(feature = "svg_import"))]
    #[test]
    fn an_svg_sprite_answers_importer_disabled_without_svg_import() {
        let d = dir("svg-disabled");
        assert!(std::fs::write(d.join("home.svg"), ICON_SVG).is_ok());
        let text = r#"(version: 1, sprites: [(name: "home", image: "home.svg")])"#;
        let err = cook_atlas(text, &d).expect_err("no SVG importer in this build");
        let message = err.to_string();
        assert!(message.contains("importer_disabled"), "{message}");
        assert!(message.contains("home"), "{message}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(feature = "svg_import")]
    #[test]
    fn an_svg_sprite_is_rasterized_at_1x_and_2x_and_stays_deterministic() {
        let d = dir("svg");
        assert!(std::fs::write(d.join("home.svg"), ICON_SVG).is_ok());
        let text = r#"(version: 1, padding: 1, sprites: [(name: "home", image: "home.svg", nine_slice: Some((1, 1, 1, 1)))])"#;
        let atlas = cook_atlas(text, &d).expect("cooks");
        let names: Vec<&str> = atlas.sprites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["home@16", "home@8"]);
        let (one, two) = (
            atlas.sprite("home@8").expect("1x"),
            atlas.sprite("home@16").expect("2x"),
        );
        assert_eq!((one.rect[2], one.rect[3]), (8, 8));
        assert_eq!((two.rect[2], two.rect[3]), (16, 16));
        assert_eq!(one.nine_slice.map(|n| n.left), Some(1));
        assert_eq!(
            two.nine_slice.map(|n| n.left),
            Some(2),
            "borders scale with the raster"
        );
        let texel = |s: &regia_2d::atlas::AtlasSprite, x: u32, y: u32| {
            let page = &atlas.pages[s.page as usize];
            let at = (((s.rect[1] + y) * page.width + s.rect[0] + x) * 4) as usize;
            [
                page.rgba8[at],
                page.rgba8[at + 1],
                page.rgba8[at + 2],
                page.rgba8[at + 3],
            ]
        };
        assert_eq!(texel(one, 4, 4), [255, 0, 0, 255]);
        assert_eq!(texel(one, 1, 1), [0, 0, 0, 0]);
        assert_eq!(texel(two, 8, 8), [255, 0, 0, 255]);
        assert_eq!(texel(two, 3, 8), [0, 0, 0, 0]);
        assert_eq!(texel(two, 4, 8), [255, 0, 0, 255], "the 2x square starts at 4 px");
        assert_eq!(cook_atlas(text, &d).expect("again"), atlas);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(feature = "svg_import")]
    #[test]
    fn a_refused_svg_sprite_names_the_sprite_and_the_code() {
        let d = dir("svg-bad");
        let bad =
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><script>x()</script></svg>"##;
        assert!(std::fs::write(d.join("bad.svg"), bad).is_ok());
        assert!(std::fs::write(d.join("ok.svg"), ICON_SVG).is_ok());
        let text = r#"(version: 1, sprites: [(name: "bad", image: "bad.svg")])"#;
        let err = cook_atlas(text, &d).expect_err("refused").to_string();
        assert!(err.contains("bad") && err.contains("svg_script"), "{err}");
        let grid = r#"(version: 1, sprites: [(name: "g", image: "ok.svg", grid: Some((4, 4)))])"#;
        let err = cook_atlas(grid, &d).expect_err("grid").to_string();
        assert!(err.contains("svg_atlas_unsupported"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
