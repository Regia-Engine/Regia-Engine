// SPDX-License-Identifier: MPL-2.0
//! A converged (or in-progress) reference image: the AOVs of `specs/27` § 1.2, metadata, and
//! writers for the layered linear EXR and the AgX preview PNG.

use std::path::Path;

use crate::PtError;
use crate::exr::{self, ExrChannel};

/// Arbitrary output variables, in the order the accumulation buffer stores them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Aov {
    /// Total radiance (cd/m²).
    Beauty,
    /// First-hit base colour.
    Albedo,
    /// First-hit shading normal (world).
    Normal,
    /// View depth of the first hit, metres (0 for sky).
    Depth,
    /// Sun, local lights and emissive surfaces reflected once at the first hit.
    Direct,
    /// Everything else reflected at the first hit through its diffuse lobe: sky light and all
    /// multi-bounce light (the GI term).
    IndirectDiffuse,
    /// The same through the specular lobes.
    IndirectSpecular,
    /// Emission seen directly by the camera.
    Emissive,
    /// Participating media: scattered and emitted radiance of the scene's media (zero when the
    /// scene has none).
    Volumetric,
    /// Background (sky and sun disc) seen directly by the camera.
    Sky,
    /// Sun visibility at the first hit, 0–1.
    Shadow,
    /// Per-pixel variance of the beauty mean (per channel).
    Variance,
    /// Samples per pixel.
    Spp,
}

impl Aov {
    /// Every AOV, in EXR layer order.
    pub const ALL: [Self; 13] = [
        Self::Beauty,
        Self::Albedo,
        Self::Normal,
        Self::Depth,
        Self::Direct,
        Self::IndirectDiffuse,
        Self::IndirectSpecular,
        Self::Emissive,
        Self::Volumetric,
        Self::Sky,
        Self::Shadow,
        Self::Variance,
        Self::Spp,
    ];

    /// Layer name (`specs/27` § 1.2).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Beauty => "beauty",
            Self::Albedo => "albedo",
            Self::Normal => "normal",
            Self::Depth => "depth",
            Self::Direct => "direct",
            Self::IndirectDiffuse => "indirect_diffuse",
            Self::IndirectSpecular => "indirect_specular",
            Self::Emissive => "emissive",
            Self::Volumetric => "volumetric",
            Self::Sky => "sky",
            Self::Shadow => "shadow",
            Self::Variance => "variance",
            Self::Spp => "spp",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }

    /// True for single-channel AOVs (written as `layer.Y`).
    #[must_use]
    pub fn is_scalar(self) -> bool {
        matches!(self, Self::Depth | Self::Shadow | Self::Spp)
    }
}

/// Provenance of a reference image (written into the EXR header and the JSON sidecar).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PtMetadata {
    /// [`crate::PtScene::content_hash`] as hex.
    pub scene_hash: String,
    /// Scene seed.
    pub seed: u32,
    /// Samples per pixel accumulated.
    pub spp: u32,
    /// Maximum path length.
    pub max_bounces: u32,
    /// Adapter name and backend.
    pub device: String,
    /// `git describe` of the build, when known (`REGIA_GIT_TAG` or empty).
    pub git_tag: String,
    /// Wall time of the trace, seconds.
    pub wall_seconds: f64,
    /// Samples rejected as non-finite (a tracer bug when non-zero).
    pub rejected_samples: u64,
    /// Triangles traced (all BLASes).
    pub triangles: u64,
    /// The denoiser that filtered `beauty` (`oidn 2.5 RT …`), empty for a raw trace. A
    /// denoised image is a preview only: [`crate::compare`] refuses it as a reference (§ 2).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub denoiser: String,
    /// Wall time of the denoise pass, seconds (0 for a raw trace).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub denoise_seconds: f64,
    /// Material graphs the trace composed and the ones it refused, by name (ADR-2725). Empty
    /// (and left out of the JSON) for a scene without graph materials.
    #[serde(default, skip_serializing_if = "crate::graph::PtGraphStats::is_empty")]
    pub surface_graphs: crate::graph::PtGraphStats,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's `skip_serializing_if` passes a reference.
fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// EXR header attribute and PNG `tEXt` keyword that mark a denoised image (value: the
/// denoiser). [`crate::compare::load`] reads it back, and also accepts the legacy spelling of
/// files written before ADR-8670 (`regia_core::brand::key_matches`).
pub const DENOISER_KEY: &str = "regia.denoiser";

/// Accumulated AOVs (means), row-major, top row first.
#[derive(Clone, Debug)]
pub struct PtImage {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Beauty radiance.
    pub beauty: Vec<[f32; 3]>,
    /// First-hit base colour.
    pub albedo: Vec<[f32; 3]>,
    /// First-hit normal.
    pub normal: Vec<[f32; 3]>,
    /// View depth.
    pub depth: Vec<f32>,
    /// Direct lighting at the first hit.
    pub direct: Vec<[f32; 3]>,
    /// Indirect through the diffuse lobe.
    pub indirect_diffuse: Vec<[f32; 3]>,
    /// Indirect through the specular lobes.
    pub indirect_specular: Vec<[f32; 3]>,
    /// Emission seen directly.
    pub emissive: Vec<[f32; 3]>,
    /// Participating media (zero when the scene has none).
    pub volumetric: Vec<[f32; 3]>,
    /// Background seen directly.
    pub sky: Vec<[f32; 3]>,
    /// Sun visibility at the first hit.
    pub shadow: Vec<f32>,
    /// Perceptual roughness the first hit shaded with (decals included; 0 for sky), ADR-2951.
    /// Not an EXR layer ([`Aov::ALL`] is unchanged): an image read back from EXR holds zeros.
    pub roughness: Vec<f32>,
    /// Variance of the beauty mean, per channel.
    pub variance: Vec<[f32; 3]>,
    /// Samples per pixel.
    pub spp: u32,
    /// Samples each pixel actually took when they differ (adaptive sampling, ADR-2433); empty
    /// for a uniform trace, where every pixel took `spp`. Row-major like the planes.
    pub spp_map: Vec<f32>,
    /// Provenance.
    pub metadata: PtMetadata,
}

impl PtImage {
    /// The RGB plane of `aov` (scalars are splatted to grey).
    #[must_use]
    pub fn rgb(&self, aov: Aov) -> Vec<[f32; 3]> {
        let scalar = |v: &[f32]| v.iter().map(|&x| [x; 3]).collect();
        match aov {
            Aov::Beauty => self.beauty.clone(),
            Aov::Albedo => self.albedo.clone(),
            Aov::Normal => self.normal.clone(),
            Aov::Depth => scalar(&self.depth),
            Aov::Direct => self.direct.clone(),
            Aov::IndirectDiffuse => self.indirect_diffuse.clone(),
            Aov::IndirectSpecular => self.indirect_specular.clone(),
            Aov::Emissive => self.emissive.clone(),
            Aov::Volumetric => self.volumetric.clone(),
            Aov::Sky => self.sky.clone(),
            Aov::Shadow => scalar(&self.shadow),
            Aov::Variance => self.variance.clone(),
            Aov::Spp if self.spp_map.len() == self.beauty.len() => {
                self.spp_map.iter().map(|&c| [c; 3]).collect()
            }
            Aov::Spp => vec![[self.spp as f32; 3]; self.beauty.len()],
        }
    }

    /// Mean of `aov` over the image, per channel.
    #[must_use]
    pub fn mean(&self, aov: Aov) -> [f64; 3] {
        let plane = self.rgb(aov);
        let n = plane.len().max(1) as f64;
        let mut s = [0.0f64; 3];
        for p in &plane {
            for k in 0..3 {
                s[k] += f64::from(p[k]);
            }
        }
        [s[0] / n, s[1] / n, s[2] / n]
    }

    /// Value of `aov` at pixel (x, y).
    #[must_use]
    pub fn at(&self, aov: Aov, x: u32, y: u32) -> [f32; 3] {
        let i = (y * self.width + x) as usize;
        self.rgb(aov).get(i).copied().unwrap_or([0.0; 3])
    }

    fn metadata_pairs(&self) -> Vec<(String, String)> {
        let m = &self.metadata;
        let mut pairs = vec![
            ("regia.scene_hash".into(), m.scene_hash.clone()),
            ("regia.seed".into(), m.seed.to_string()),
            ("regia.spp".into(), m.spp.to_string()),
            ("regia.max_bounces".into(), m.max_bounces.to_string()),
            ("regia.device".into(), m.device.clone()),
            ("regia.git_tag".into(), m.git_tag.clone()),
            ("regia.wall_seconds".into(), format!("{:.3}", m.wall_seconds)),
            ("regia.rejected_samples".into(), m.rejected_samples.to_string()),
            ("regia.triangles".into(), m.triangles.to_string()),
        ];
        if m.denoise_seconds != 0.0 {
            pairs.push(("regia.denoise_seconds".into(), m.denoise_seconds.to_string()));
        }
        if !m.denoiser.is_empty() {
            pairs.push((DENOISER_KEY.into(), m.denoiser.clone()));
        }
        pairs
    }

    /// Text chunks for the preview PNG: the denoiser mark when there is one.
    fn png_text(&self) -> Vec<(String, String)> {
        if self.metadata.denoiser.is_empty() {
            Vec::new()
        } else {
            vec![(DENOISER_KEY.into(), self.metadata.denoiser.clone())]
        }
    }

    /// Layered linear EXR (FLOAT channels): `beauty.R/G/B`, `depth.Y`, … one layer per AOV in
    /// `aovs` (all of them when empty), plus the metadata as header strings.
    ///
    /// # Errors
    ///
    /// File IO failed.
    pub fn write_exr(&self, path: &Path, aovs: &[Aov]) -> Result<(), PtError> {
        let list: Vec<Aov> = if aovs.is_empty() {
            Aov::ALL.to_vec()
        } else {
            aovs.to_vec()
        };
        let mut channels = Vec::new();
        for aov in list {
            let plane = self.rgb(aov);
            if aov.is_scalar() {
                channels.push(ExrChannel {
                    name: format!("{}.Y", aov.name()),
                    data: plane.iter().map(|p| p[0]).collect(),
                });
            } else {
                for (k, c) in ["R", "G", "B"].iter().enumerate() {
                    channels.push(ExrChannel {
                        name: format!("{}.{c}", aov.name()),
                        data: plane.iter().map(|p| p[k]).collect(),
                    });
                }
            }
        }
        exr::write(path, self.width, self.height, &channels, &self.metadata_pairs())
            .map_err(|e| PtError::Io(e.to_string()))
    }

    /// Reads back an EXR written by [`Self::write_exr`]: every layer present, missing layers as
    /// zeros (`spp` and `shadow` default to the metadata's spp and 1), metadata from the header.
    ///
    /// # Errors
    ///
    /// The file is unreadable or has no `beauty` layer.
    pub fn read_exr(path: &Path) -> Result<Self, PtError> {
        let img = exr::read(path).map_err(|e| PtError::Io(format!("{}: {e}", path.display())))?;
        let n = (img.width as usize) * (img.height as usize);
        let beauty = img
            .rgb(Aov::Beauty.name())
            .ok_or_else(|| PtError::Io(format!("{} has no beauty layer", path.display())))?;
        let rgb = |aov: Aov| img.rgb(aov.name()).unwrap_or_else(|| vec![[0.0; 3]; n]);
        let scalar = |aov: Aov, default: f32| {
            img.channel(&format!("{}.Y", aov.name()))
                .map_or_else(|| vec![default; n], <[f32]>::to_vec)
        };
        // Current keys first, the legacy keys of files written before ADR-8670 second.
        let attr = |key: &str| {
            regia_core::brand::find_key(img.attributes.iter().map(|(k, v)| (k.as_str(), v)), key)
                .cloned()
                .unwrap_or_default()
        };
        let num = |key: &str| attr(key).parse::<f64>().unwrap_or(0.0);
        let metadata = PtMetadata {
            scene_hash: attr("regia.scene_hash"),
            seed: num("regia.seed") as u32,
            spp: num("regia.spp") as u32,
            max_bounces: num("regia.max_bounces") as u32,
            device: attr("regia.device"),
            git_tag: attr("regia.git_tag"),
            wall_seconds: num("regia.wall_seconds"),
            rejected_samples: num("regia.rejected_samples") as u64,
            triangles: attr("regia.triangles").parse::<u64>().unwrap_or(0),
            denoiser: attr(DENOISER_KEY),
            denoise_seconds: num("regia.denoise_seconds"),
            // The EXR header does not carry graph stats; the JSON sidecar does.
            surface_graphs: crate::graph::PtGraphStats::default(),
        };
        Ok(Self {
            width: img.width,
            height: img.height,
            beauty,
            albedo: rgb(Aov::Albedo),
            normal: rgb(Aov::Normal),
            depth: scalar(Aov::Depth, 0.0),
            direct: rgb(Aov::Direct),
            indirect_diffuse: rgb(Aov::IndirectDiffuse),
            indirect_specular: rgb(Aov::IndirectSpecular),
            emissive: rgb(Aov::Emissive),
            volumetric: rgb(Aov::Volumetric),
            sky: rgb(Aov::Sky),
            shadow: scalar(Aov::Shadow, 1.0),
            // Not an EXR layer (ADR-2951): a reloaded image has no roughness.
            roughness: vec![0.0; n],
            variance: rgb(Aov::Variance),
            spp: metadata.spp,
            spp_map: {
                let map = scalar(Aov::Spp, metadata.spp as f32);
                if map.iter().all(|&c| c == metadata.spp as f32) {
                    Vec::new()
                } else {
                    map
                }
            },
            metadata,
        })
    }

    /// 8-bit preview of `aov` through the runtime's display transform (exposure at `ev100`,
    /// AgX, the default look), so a reference and a real-time frame share one tonemap.
    #[must_use]
    pub fn preview_rgba8(&self, aov: Aov, ev100: f32) -> Vec<u8> {
        preview_rgba8(&self.rgb(aov), ev100)
    }

    /// Writes [`Self::preview_rgba8`] as a PNG (a denoised image carries [`DENOISER_KEY`] as a
    /// `tEXt` chunk, so the preview cannot pass for a reference either).
    ///
    /// # Errors
    ///
    /// Encoding or file IO failed.
    pub fn write_preview_png(&self, path: &Path, aov: Aov, ev100: f32) -> Result<(), PtError> {
        write_png_with_text(
            path,
            self.width,
            self.height,
            &self.preview_rgba8(aov, ev100),
            &self.png_text(),
        )
    }

    /// Writes the metadata as pretty JSON.
    ///
    /// # Errors
    ///
    /// File IO failed.
    pub fn write_metadata_json(&self, path: &Path) -> Result<(), PtError> {
        let text = serde_json::to_string_pretty(&self.metadata).map_err(|e| PtError::Io(e.to_string()))?;
        exr::write_file_atomic(path, text.as_bytes()).map_err(|e| PtError::Io(e.to_string()))
    }
}

/// Display-encodes linear radiance at `ev100` with the runtime's AgX + default look.
#[must_use]
pub fn preview_rgba8(linear: &[[f32; 3]], ev100: f32) -> Vec<u8> {
    let exposure = regia_render::exposure_from_ev100(ev100);
    let mut out = Vec::with_capacity(linear.len() * 4);
    for c in linear {
        let exposed = [
            (c[0] * exposure).clamp(0.0, 1.0e4),
            (c[1] * exposure).clamp(0.0, 1.0e4),
            (c[2] * exposure).clamp(0.0, 1.0e4),
        ];
        let d = regia_render::display_look(
            regia_render::tonemap_agx(exposed),
            regia_render::DEFAULT_LOOK_CONTRAST,
            regia_render::DEFAULT_LOOK_SATURATION,
        );
        for v in d {
            out.push((v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
        }
        out.push(255);
    }
    out
}

/// Writes tightly packed RGBA8 as a PNG.
///
/// # Errors
///
/// Encoding or file IO failed.
pub fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), PtError> {
    write_png_with_text(path, width, height, rgba, &[])
}

/// [`write_png`] with `tEXt` chunks (`(keyword, text)`, Latin-1, keyword 1–79 bytes).
///
/// # Errors
///
/// Encoding or file IO failed.
pub fn write_png_with_text(
    path: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
    text: &[(String, String)],
) -> Result<(), PtError> {
    let mut encoded = Vec::new();
    let mut enc = png::Encoder::new(&mut encoded, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    for (keyword, value) in text {
        enc.add_text_chunk(keyword.clone(), value.clone())
            .map_err(|e| PtError::Io(e.to_string()))?;
    }
    let mut writer = enc.write_header().map_err(|e| PtError::Io(e.to_string()))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| PtError::Io(e.to_string()))?;
    writer.finish().map_err(|e| PtError::Io(e.to_string()))?;
    // Encode fully first, then replace the file: a bad chunk or a short buffer keeps the old one.
    exr::write_file_atomic(path, &encoded).map_err(|e| PtError::Io(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> PtImage {
        let n = 4;
        PtImage {
            width: 2,
            height: 2,
            beauty: vec![[1.0, 2.0, 3.0]; n],
            albedo: vec![[0.5; 3]; n],
            normal: vec![[0.0, 1.0, 0.0]; n],
            depth: vec![4.0; n],
            direct: vec![[0.25; 3]; n],
            indirect_diffuse: vec![[0.125; 3]; n],
            indirect_specular: vec![[0.0; 3]; n],
            emissive: vec![[0.0; 3]; n],
            volumetric: vec![[0.0; 3]; n],
            sky: vec![[0.0; 3]; n],
            shadow: vec![1.0; n],
            roughness: vec![0.5; n],
            variance: vec![[0.01; 3]; n],
            spp: 16,
            spp_map: Vec::new(),
            metadata: PtMetadata {
                seed: 3,
                spp: 16,
                ..PtMetadata::default()
            },
        }
    }

    #[test]
    fn aov_names_round_trip() {
        for aov in Aov::ALL {
            assert_eq!(Aov::from_name(aov.name()), Some(aov));
        }
        assert_eq!(Aov::from_name("nope"), None);
    }

    #[test]
    fn exr_has_every_layer() {
        let dir = std::env::temp_dir().join(format!("regia_pt_out_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("tiny.exr");
        tiny().write_exr(&path, &[]).expect("write");
        let img = crate::exr::read(&path).expect("read");
        for aov in Aov::ALL {
            let first = if aov.is_scalar() {
                format!("{}.Y", aov.name())
            } else {
                format!("{}.R", aov.name())
            };
            assert!(img.channel(&first).is_some(), "{first}");
        }
        assert_eq!(img.channel("spp.Y").map(|c| c[0]), Some(16.0));
        assert!(img.attributes.iter().any(|(k, v)| k == "regia.seed" && v == "3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// specs/27 § 1.2: the EXR carries the scene hash, seed, spp, device, git tag and wall time.
    #[test]
    fn exr_round_trips_every_spec_metadata_field() {
        let dir = regia_test_utils::unique_temp_dir("pt_spec_meta");
        let mut img = tiny();
        img.metadata.scene_hash = "9f3a0c1d".into();
        img.metadata.device = "llvmpipe (LLVM 19, 256 bits)".into();
        img.metadata.git_tag = "v0.5.0-rc1".into();
        img.metadata.wall_seconds = 12.5;
        img.metadata.max_bounces = 12;
        let path = dir.join("spec.exr");
        img.write_exr(&path, &[]).expect("write");
        let raw = crate::exr::read(&path).expect("read");
        for key in [
            "regia.scene_hash",
            "regia.seed",
            "regia.spp",
            "regia.device",
            "regia.git_tag",
            "regia.wall_seconds",
        ] {
            assert!(raw.attributes.iter().any(|(k, _)| k == key), "{key} is written");
        }
        let legacy = regia_core::brand::LEGACY_KEY_PREFIX;
        assert!(
            !raw.attributes.iter().any(|(k, _)| k.starts_with(legacy)),
            "ADR-8670: no legacy key is written"
        );
        let back = PtImage::read_exr(&path).expect("read");
        assert_eq!(back.metadata.scene_hash, "9f3a0c1d");
        assert_eq!(back.metadata.seed, 3);
        assert_eq!(back.metadata.spp, 16);
        assert_eq!(back.metadata.max_bounces, 12);
        assert_eq!(back.metadata.device, "llvmpipe (LLVM 19, 256 bits)");
        assert_eq!(back.metadata.git_tag, "v0.5.0-rc1");
        assert_eq!(back.metadata.wall_seconds, 12.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-8670: an EXR written before the key rename (every attribute under the legacy prefix)
    /// still reads back with all of its metadata.
    #[test]
    fn exr_written_with_legacy_keys_still_reads_back() {
        let dir = regia_test_utils::unique_temp_dir("pt_legacy_keys");
        let mut img = tiny();
        img.metadata.scene_hash = "9f3a0c1d".into();
        img.metadata.device = "llvmpipe".into();
        img.metadata.git_tag = "v0.5.0".into();
        img.metadata.wall_seconds = 2.5;
        img.metadata.triangles = 42;
        img.metadata.denoiser = "atrous 5".into();
        img.metadata.denoise_seconds = 0.25;
        let legacy = regia_core::brand::LEGACY_KEY_PREFIX;
        let pairs: Vec<(String, String)> = img
            .metadata_pairs()
            .into_iter()
            .map(|(k, v)| {
                let rest = k.strip_prefix("regia").expect("every key is engine-owned");
                (format!("{legacy}{rest}"), v)
            })
            .collect();
        assert!(pairs.iter().all(|(k, _)| k.starts_with(legacy)));
        let channels: Vec<ExrChannel> = ["R", "G", "B"]
            .iter()
            .enumerate()
            .map(|(c, name)| ExrChannel {
                name: format!("beauty.{name}"),
                data: img.beauty.iter().map(|p| p[c]).collect(),
            })
            .collect();
        let path = dir.join("old.exr");
        exr::write(&path, img.width, img.height, &channels, &pairs).expect("write");
        let back = PtImage::read_exr(&path).expect("read");
        assert_eq!(back.metadata.scene_hash, "9f3a0c1d");
        assert_eq!(back.metadata.seed, img.metadata.seed);
        assert_eq!(back.metadata.spp, img.metadata.spp);
        assert_eq!(back.metadata.max_bounces, img.metadata.max_bounces);
        assert_eq!(back.metadata.device, "llvmpipe");
        assert_eq!(back.metadata.git_tag, "v0.5.0");
        assert_eq!(back.metadata.wall_seconds, 2.5);
        assert_eq!(back.metadata.triangles, 42);
        assert_eq!(back.metadata.denoiser, "atrous 5");
        assert_eq!(back.metadata.denoise_seconds, 0.25);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_png_or_exr_write_keeps_the_previous_file() {
        let dir = regia_test_utils::unique_temp_dir("pt_atomic");
        let png = dir.join("a.png");
        write_png(&png, 1, 1, &[1, 2, 3, 255]).expect("first png");
        let before = std::fs::read(&png).expect("read");
        // An empty tEXt keyword is invalid.
        let bad = write_png_with_text(&png, 1, 1, &[9, 9, 9, 255], &[(String::new(), "x".into())]);
        assert!(matches!(bad, Err(PtError::Io(_))));
        // A short pixel buffer is invalid.
        assert!(write_png(&png, 1, 1, &[1]).is_err());
        assert_eq!(std::fs::read(&png).expect("read"), before);
        let exr_path = dir.join("a.exr");
        tiny().write_exr(&exr_path, &[]).expect("first exr");
        let before = std::fs::read(&exr_path).expect("read");
        // Channel sample counts that do not match the size fail before any file is touched.
        let c = ExrChannel {
            name: "Y".into(),
            data: vec![0.0],
        };
        assert!(exr::write(&exr_path, 2, 2, &[c], &[]).is_err());
        assert_eq!(std::fs::read(&exr_path).expect("read"), before);
        let leftovers = std::fs::read_dir(&dir).expect("dir").count();
        assert_eq!(leftovers, 2, "no temporary files remain");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exr_round_trips_triangles_and_denoise_seconds() {
        let dir = regia_test_utils::unique_temp_dir("pt_meta");
        let mut img = tiny();
        img.metadata.triangles = 12_345;
        img.metadata.denoise_seconds = 0.125;
        let path = dir.join("meta.exr");
        img.write_exr(&path, &[]).expect("write");
        let back = PtImage::read_exr(&path).expect("read");
        assert_eq!(back.metadata.triangles, 12_345);
        assert_eq!(back.metadata.denoise_seconds, 0.125);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exr_round_trips_and_carries_the_denoiser_mark() {
        let dir = std::env::temp_dir().join(format!("regia_pt_rt_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let raw = dir.join("raw.exr");
        tiny().write_exr(&raw, &[]).expect("write");
        let back = PtImage::read_exr(&raw).expect("read");
        assert_eq!(back.beauty, tiny().beauty);
        assert_eq!(back.depth, tiny().depth);
        assert_eq!((back.spp, back.metadata.seed), (16, 3));
        assert!(back.metadata.denoiser.is_empty());
        let mut marked = tiny();
        marked.metadata.denoiser = "oidn test".into();
        let exr_path = dir.join("marked.exr");
        let png_path = dir.join("marked.png");
        marked.write_exr(&exr_path, &[Aov::Beauty]).expect("write");
        marked
            .write_preview_png(&png_path, Aov::Beauty, 0.0)
            .expect("png");
        assert_eq!(
            PtImage::read_exr(&exr_path).expect("read").metadata.denoiser,
            "oidn test"
        );
        for path in [&exr_path, &png_path] {
            let loaded = crate::compare::load(path, "beauty").expect("load");
            assert_eq!(
                loaded.denoiser.as_deref(),
                Some("oidn test"),
                "{}",
                path.display()
            );
        }
        // A raw preview PNG carries no mark.
        let raw_png = dir.join("raw.png");
        tiny().write_preview_png(&raw_png, Aov::Beauty, 0.0).expect("png");
        assert_eq!(
            crate::compare::load(&raw_png, "beauty").expect("load").denoiser,
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_maps_mid_grey_near_half() {
        // EV100 0 → exposure 1/1.2; 0.18 × 1.2 lands on AgX's middle grey.
        let px = preview_rgba8(&[[0.18 * 1.2; 3]], 0.0);
        assert!((100..=160).contains(&px[0]), "{}", px[0]);
        assert_eq!(px[3], 255);
    }
}
