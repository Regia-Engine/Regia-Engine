// SPDX-License-Identifier: MPL-2.0
//! Cooks a `.glasscrack.ron` crack set (`docs/formats/glass-crack.md`): decodes the stamp PNGs,
//! checks the texture rules and writes a [`CookedCrackSet`] blob the runtime loads into
//! `regia_render::glass::GlassCrackSets`.

use std::path::{Path, PathBuf};

use regia_asset::glasscrack::{CookedCrackSet, GlassCrackError, GlassCrackSet, StampPixels};

/// Why a crack set failed to cook.
#[derive(Debug, thiserror::Error)]
pub enum GlassCrackCookError {
    /// Reading a file failed.
    #[error("{path}: {source}")]
    Io {
        /// File.
        path: PathBuf,
        /// Cause.
        source: std::io::Error,
    },
    /// A stamp texture could not be decoded or breaks a size rule.
    #[error("{path}: {reason}")]
    Texture {
        /// File.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// The manifest or the pixel rules.
    #[error(transparent)]
    Format(#[from] GlassCrackError),
}

fn load_png(path: &Path) -> Result<image::DynamicImage, GlassCrackCookError> {
    let bytes = std::fs::read(path).map_err(|source| GlassCrackCookError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    image::load_from_memory(&bytes).map_err(|e| GlassCrackCookError::Texture {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

fn check_size(path: &Path, w: u32, h: u32) -> Result<u32, GlassCrackCookError> {
    if w != h || !w.is_power_of_two() || !(128..=1024).contains(&w) {
        return Err(GlassCrackCookError::Texture {
            path: path.to_path_buf(),
            reason: format!("{w}×{h}: stamps are square, a power of two, 128–1024"),
        });
    }
    Ok(w)
}

/// Tangent normal XY (RG8) from the gradient of R × A when a stamp has no normal map.
fn derived_normal(size: u32, mask: &[u8]) -> Vec<u8> {
    let n = size as usize;
    let h = |x: usize, y: usize| {
        let i = (y.min(n - 1) * n + x.min(n - 1)) * 4;
        f32::from(mask[i]) * f32::from(mask[i + 3]) / (255.0 * 255.0)
    };
    let mut out = vec![128u8; n * n * 2];
    for y in 0..n {
        for x in 0..n {
            let gx = h(x + 1, y) - h(x.saturating_sub(1), y);
            let gy = h(x, y + 1) - h(x, y.saturating_sub(1));
            let q = |v: f32| ((v.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0 + 0.5) as u8;
            // Image Y down → tangent Y up.
            out[(y * n + x) * 2] = q(-gx * 1.6);
            out[(y * n + x) * 2 + 1] = q(gy * 1.6);
        }
    }
    out
}

/// Decodes and validates the set at `manifest_path` (stamp paths are relative to it).
pub fn cook_glasscrack(manifest_path: &Path) -> Result<CookedCrackSet, GlassCrackCookError> {
    let text = std::fs::read_to_string(manifest_path).map_err(|source| GlassCrackCookError::Io {
        path: manifest_path.to_path_buf(),
        source,
    })?;
    let manifest = GlassCrackSet::from_ron(&text)?;
    let dir = manifest_path.parent().unwrap_or(Path::new("."));
    let mut stamps = Vec::new();
    for level in 1..=3u8 {
        for decl in manifest.stamps(level) {
            let mask_path = crate::cook::join_portable(dir, &decl.mask).map_err(|reason| {
                GlassCrackCookError::Texture {
                    path: PathBuf::from(&decl.mask),
                    reason,
                }
            })?;
            let mask_img = load_png(&mask_path)?.to_rgba8();
            let size = check_size(&mask_path, mask_img.width(), mask_img.height())?;
            let mask = mask_img.into_raw();
            let normal = match &decl.normal {
                Some(rel) => {
                    let normal_path = crate::cook::join_portable(dir, rel).map_err(|reason| {
                        GlassCrackCookError::Texture {
                            path: PathBuf::from(rel),
                            reason,
                        }
                    })?;
                    let img = load_png(&normal_path)?.to_rgb8();
                    if (img.width(), img.height()) != (size, size) {
                        return Err(GlassCrackCookError::Texture {
                            path: normal_path,
                            reason: format!("normal is {}×{}, mask is {size}²", img.width(), img.height()),
                        });
                    }
                    img.into_raw()
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .flat_map(|t| [t[0], t[1]])
                        .collect()
                }
                None => derived_normal(size, &mask),
            };
            stamps.push(StampPixels { size, mask, normal });
        }
    }
    let cooked = CookedCrackSet { manifest, stamps };
    cooked.validate_pixels()?;
    Ok(cooked)
}

/// [`cook_glasscrack`] as the cooked bytes (`.glasscrack` in a pak).
pub fn cook_glasscrack_bytes(manifest_path: &Path) -> Result<Vec<u8>, GlassCrackCookError> {
    Ok(cook_glasscrack(manifest_path)?.to_bytes()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(path: &Path, size: u32, color: image::ColorType, bytes: &[u8]) {
        image::save_buffer(path, bytes, size, size, color).expect("png");
    }

    /// The built-in laminate set written as authored files, cooked back to the same pixels.
    #[test]
    fn an_authored_set_cooks_to_its_pixels_and_holes_before_d3_fail() {
        let dir = std::env::temp_dir().join(format!("regia-glasscrack-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("stamps")).expect("dir");
        let mut set = regia_render::glass::procedural::laminate_set();
        let all: Vec<_> = (1..=3u8).flat_map(|l| set.manifest.stamps(l).to_vec()).collect();
        for (decl, px) in all.iter().zip(&set.stamps) {
            write_png(
                &dir.join(format!("stamps/{}_mask.png", decl.name)),
                px.size,
                image::ColorType::Rgba8,
                &px.mask,
            );
            let rgb: Vec<u8> = px
                .normal
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|t| [t[0], t[1], 255])
                .collect();
            write_png(
                &dir.join(format!("stamps/{}_n.png", decl.name)),
                px.size,
                image::ColorType::Rgb8,
                &rgb,
            );
        }
        for level in [&mut set.manifest.d1, &mut set.manifest.d2, &mut set.manifest.d3] {
            for decl in level.iter_mut() {
                decl.mask = format!("stamps/{}_mask.png", decl.name);
                decl.normal = Some(format!("stamps/{}_n.png", decl.name));
            }
        }
        let manifest = dir.join("canopy.glasscrack.ron");
        std::fs::write(&manifest, set.manifest.to_ron().expect("ron")).expect("write");
        let cooked = cook_glasscrack(&manifest).expect("cook");
        assert_eq!(cooked.stamps, set.stamps);
        let bytes = cook_glasscrack_bytes(&manifest).expect("bytes");
        assert_eq!(
            CookedCrackSet::from_bytes(&bytes).expect("load").stamps,
            set.stamps
        );

        // A hole painted into a D1 stamp is rejected.
        let d1 = &set.stamps[0];
        let mut holed = d1.mask.clone();
        holed[2] = 255;
        write_png(
            &dir.join(format!("stamps/{}_mask.png", all[0].name)),
            d1.size,
            image::ColorType::Rgba8,
            &holed,
        );
        assert!(matches!(
            cook_glasscrack(&manifest),
            Err(GlassCrackCookError::Format(_))
        ));
        // A non-square stamp is rejected.
        image::save_buffer(
            dir.join(format!("stamps/{}_mask.png", all[0].name)),
            &vec![0u8; 256 * 128 * 4],
            256,
            128,
            image::ColorType::Rgba8,
        )
        .expect("png");
        assert!(matches!(
            cook_glasscrack(&manifest),
            Err(GlassCrackCookError::Texture { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_normal_is_derived_from_the_cracks() {
        let n = 128u32;
        let mut mask = vec![0u8; (n * n * 4) as usize];
        for y in 0..n {
            let i = ((y * n + 64) * 4) as usize;
            mask[i] = 255;
            mask[i + 3] = 255;
        }
        let normal = derived_normal(n, &mask);
        // Left of the vertical crack the normal tilts one way, right of it the other.
        let at = |x: u32| normal[((64 * n + x) * 2) as usize];
        assert!(at(63) < 128 && at(65) > 128, "{} {}", at(63), at(65));
    }
}
