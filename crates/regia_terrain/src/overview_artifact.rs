// SPDX-License-Identifier: MPL-2.0
//! The registered overview artifact (ADR-8596): an overview PNG and its engine descriptor
//! (`<name>.overview.json`, `docs/formats/terrain.md` § 16) published as a pair, cached by source
//! and settings, and cancellable until publication.
//!
//! The descriptor records the source identity (project path, the caller's input digest and a
//! digest of the decoded samples), every setting, the image-to-world mapping
//! ([`crate::overview::OverviewMapping`]), orientation, legend and scale
//! ([`crate::overview::OverviewLegend`]), the markers and where
//! they landed, the image's BLAKE3 and the cross-architecture comparison tolerance of a
//! presentation-only image ([`OVERVIEW_TOLERANCE`]).
//!
//! **Publication.** Both files are written to temporary names in the destination folder first;
//! the observer's last step ([`OverviewProgress::step`] `== steps`) is the commit point: a cancel
//! there or earlier removes the temporaries and leaves any earlier pair untouched. After it, the
//! PNG is renamed into place and then the descriptor (the commit record): a reader trusts a PNG
//! only when the descriptor beside it names its BLAKE3 ([`cached_overview`]), so an interrupted
//! rename never passes for a published pair. The descriptor's `descriptor_digest` covers every
//! other field of it ([`descriptor_digest`]), so a hand edit of the mapping or an added field is
//! a cache miss too (ADR-8871, plan W-19).
//!
//! **Cache.** [`overview_cache_key`] hashes the algorithm version, the decoded source, the
//! settings that change pixels and the markers. A publish whose key matches a descriptor whose
//! PNG still hashes to the recorded value bakes nothing and writes nothing.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use crate::overview::{
    LETTERBOX_RGB, MISSING_RGB, OverviewMarker, OverviewProgress, OverviewSettings, PlacedMarker,
    RegisteredOverview, bake_overview_registered, draw_markers,
};
use crate::source::{SourceError, TerrainSource};

/// Version of the descriptor's shape (`"overview_descriptor"`). Version 2 (ADR-8871, plan W-19)
/// adds `descriptor_digest`; a version-1 pair is not cached and bakes again.
pub const OVERVIEW_DESCRIPTOR_VERSION: u32 = 2;

/// Version of the pixels a given source and settings bake to. Bump it with any change that moves
/// a pixel: every cached overview then bakes again.
pub const OVERVIEW_ALGORITHM_VERSION: u32 = 1;

/// How far two bakes of one source and settings may differ across architectures and toolchains
/// (x86-64, aarch64; overview images are presentation-only, never simulation input).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverviewTolerance {
    /// Largest absolute difference of one 8-bit channel.
    pub max_channel_delta: u8,
    /// Largest fraction of pixels with any channel differing at all.
    pub max_differing_fraction: f64,
}

/// The declared tolerance: at most one 8-bit step on at most 0.1 % of the pixels. The bake is
/// plain IEEE-754 `f64` arithmetic and square roots (correctly rounded on both architectures), so
/// bakes normally match bit for bit; the bound covers a rounding tie flipping on a toolchain that
/// contracts multiply-adds.
pub const OVERVIEW_TOLERANCE: OverviewTolerance = OverviewTolerance {
    max_channel_delta: 1,
    max_differing_fraction: 0.001,
};

impl OverviewTolerance {
    /// Compares two RGB8 images of one size.
    ///
    /// # Errors
    /// What exceeds the tolerance (or a size mismatch), as text.
    pub fn compare(&self, a: &[u8], b: &[u8]) -> Result<(), String> {
        if a.len() != b.len() || !a.len().is_multiple_of(3) {
            return Err(format!("image sizes differ: {} and {} bytes", a.len(), b.len()));
        }
        let (mut differing, mut worst) = (0usize, 0u8);
        for (pa, pb) in a.as_chunks::<3>().0.iter().zip(b.as_chunks::<3>().0) {
            let delta = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
            worst = worst.max(delta);
            if delta > 0 {
                differing += 1;
            }
        }
        let pixels = a.len() / 3;
        if worst > self.max_channel_delta {
            return Err(format!(
                "a channel differs by {worst} (tolerance {})",
                self.max_channel_delta
            ));
        }
        if differing as f64 > self.max_differing_fraction * pixels as f64 {
            return Err(format!(
                "{differing} of {pixels} pixels differ (tolerance {} of the pixels)",
                self.max_differing_fraction
            ));
        }
        Ok(())
    }
}

/// One registered overview to publish.
#[derive(Clone, Debug)]
pub struct OverviewJob<'a> {
    /// The decoded source.
    pub source: &'a TerrainSource,
    /// Its project-relative path, recorded in the descriptor.
    pub source_path: String,
    /// The caller's digest of the source's input files (`terrain.inspect_source`'s inputs
    /// digest), recorded and part of the cache key; empty when the caller has none.
    pub source_digest: String,
    /// The bake settings.
    pub settings: OverviewSettings,
    /// Markers drawn over the image, in order.
    pub markers: Vec<OverviewMarker>,
}

/// What [`publish_overview`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct PublishedOverview {
    /// The pair was already up to date: nothing was baked or written.
    pub cached: bool,
    /// The descriptor as published (or as found, when cached).
    pub descriptor: Value,
    /// The PNG's path.
    pub png_path: PathBuf,
    /// The descriptor's path ([`descriptor_path`]).
    pub descriptor_path: PathBuf,
}

/// The descriptor beside an overview PNG: `maps/area.png` → `maps/area.overview.json`.
#[must_use]
pub fn descriptor_path(png: &Path) -> PathBuf {
    let stem = png
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    png.with_file_name(format!("{stem}.overview.json"))
}

/// BLAKE3 (hex) of the decoded source: grid, datum, scale, spacing, first sample, counts and
/// which samples hold data. Two sources with the same digest bake the same overview.
#[must_use]
pub fn source_content_digest(source: &TerrainSource) -> String {
    let mut hasher = blake3::Hasher::new();
    let [cols, rows] = source.dimensions();
    hasher.update(b"regia.terrain_source.v1");
    hasher.update(&cols.to_le_bytes());
    hasher.update(&rows.to_le_bytes());
    hasher.update(&source.meters_per_count().to_bits().to_le_bytes());
    hasher.update(&source.zero_counts().to_le_bytes());
    for v in source.spacing_m().into_iter().chain(source.sample0()) {
        hasher.update(&v.to_bits().to_le_bytes());
    }
    for c in source.counts() {
        hasher.update(&c.to_le_bytes());
    }
    if source.missing_samples() > 0 {
        hasher.update(b"mask");
        for row in 0..rows {
            for col in 0..cols {
                hasher.update(&[u8::from(source.is_valid(col, row))]);
            }
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// The settings as the descriptor and the cache key record them (`max_pixels`, a budget that
/// moves no pixel, is left out).
#[must_use]
pub fn settings_json(settings: &OverviewSettings) -> Value {
    json!({
        "size": settings.size,
        "fit": settings.fit.as_str(),
        "relief": settings.relief,
        "contour_step_m": settings.contour_step_m,
        "blur_passes": settings.blur_passes,
        "light": settings.light,
        "ambient": settings.ambient,
        "contrast": settings.contrast,
        "frame_pixels": settings.frame_pixels,
    })
}

fn markers_json(markers: &[OverviewMarker]) -> Value {
    Value::Array(
        markers
            .iter()
            .map(|m| json!({ "label": m.label, "world_xz": m.world_xz, "rgb": m.rgb }))
            .collect(),
    )
}

/// The cache key of a job: BLAKE3 (hex) of the algorithm version, the input digest, the decoded
/// source ([`source_content_digest`]), the settings ([`settings_json`]) and the markers. Any change
/// to one of them is a different key.
#[must_use]
pub fn overview_cache_key(job: &OverviewJob<'_>) -> String {
    let key = json!({
        "algorithm": OVERVIEW_ALGORITHM_VERSION,
        "input_digest": job.source_digest,
        "source": source_content_digest(job.source),
        "settings": settings_json(&job.settings),
        "markers": markers_json(&job.markers),
    });
    let mut hasher = blake3::Hasher::new();
    hasher.update(key.to_string().as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// The descriptor key that holds [`descriptor_digest`].
const DESCRIPTOR_DIGEST_KEY: &str = "descriptor_digest";

/// BLAKE3 (hex) of everything in `descriptor` but its `descriptor_digest`: the compact JSON of
/// the value with every object's keys in sorted order, so the digest does not depend on how a
/// file orders its keys or spaces its text (ADR-8871, plan W-19).
#[must_use]
pub fn descriptor_digest(descriptor: &Value) -> String {
    fn sorted(value: &Value, top: bool) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map
                    .keys()
                    .filter(|k| !(top && k.as_str() == DESCRIPTOR_DIGEST_KEY))
                    .collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for key in keys {
                    out.insert(key.clone(), sorted(&map[key.as_str()], false));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(|v| sorted(v, false)).collect()),
            other => other.clone(),
        }
    }
    blake3::hash(sorted(descriptor, true).to_string().as_bytes())
        .to_hex()
        .to_string()
}

/// The published descriptor beside `png` when it is current: its version and `cache_key` match,
/// its `descriptor_digest` is the digest of the rest of it ([`descriptor_digest`]: an edited,
/// added or removed field anywhere is not current) and the PNG on disk still hashes to the
/// BLAKE3 it records. `None` otherwise (missing, stale, edited or half-published).
#[must_use]
pub fn cached_overview(png: &Path, cache_key: &str) -> Option<Value> {
    let text = std::fs::read(descriptor_path(png)).ok()?;
    let descriptor: Value = serde_json::from_slice(&text).ok()?;
    if descriptor.get("overview_descriptor").and_then(Value::as_u64)
        != Some(u64::from(OVERVIEW_DESCRIPTOR_VERSION))
        || descriptor.get("cache_key").and_then(Value::as_str) != Some(cache_key)
        || descriptor.get(DESCRIPTOR_DIGEST_KEY).and_then(Value::as_str)
            != Some(descriptor_digest(&descriptor).as_str())
    {
        return None;
    }
    let recorded = descriptor.pointer("/image/blake3").and_then(Value::as_str)?;
    let bytes = std::fs::read(png).ok()?;
    (blake3::hash(&bytes).to_hex().as_str() == recorded).then_some(descriptor)
}

/// The descriptor of a bake (see the module documentation for each field), ending with its
/// `descriptor_digest` ([`descriptor_digest`]).
#[must_use]
pub fn descriptor_json(
    job: &OverviewJob<'_>,
    cache_key: &str,
    png_file_name: &str,
    png: &[u8],
    baked: &RegisteredOverview,
    placed: &[PlacedMarker],
) -> Value {
    let source = job.source;
    let [cols, rows] = source.dimensions();
    let mapping = &baked.mapping;
    let legend = &baked.legend;
    let [width, height] = baked.image.dimensions();
    let markers: Vec<Value> = job
        .markers
        .iter()
        .zip(placed)
        .map(|(m, p)| json!({ "label": m.label, "world_xz": m.world_xz, "rgb": m.rgb, "pixel": p.pixel }))
        .collect();
    let mut descriptor = json!({
        "overview_descriptor": OVERVIEW_DESCRIPTOR_VERSION,
        "algorithm": OVERVIEW_ALGORITHM_VERSION,
        "cache_key": cache_key,
        "presentation_only": true,
        "image": {
            "file": png_file_name, "width": width, "height": height,
            "bytes": png.len(), "blake3": blake3::hash(png).to_hex().to_string(),
            "missing_pixels": baked.image.missing_pixels(),
        },
        "source": {
            "path": job.source_path, "input_digest": job.source_digest,
            "content_digest": source_content_digest(source),
            "cols": cols, "rows": rows, "spacing_m": source.spacing_m(), "sample0_m": source.sample0(),
            "missing_samples": source.missing_samples(),
        },
        "settings": settings_json(&job.settings),
        "mapping": {
            "image_size": mapping.image_size,
            "content_px": mapping.content_px,
            "world_min": mapping.world_min,
            "world_max": mapping.world_max,
            "meters_per_pixel": mapping.meters_per_pixel(),
            "pixel_origin": "top-left corner of pixel (0, 0); pixel centers at +0.5",
            "u_axis": "+X (east)",
            "v_axis": "-Z (south)",
        },
        "orientation": { "north_up": true, "row0": "north", "col0": "west" },
        "legend": {
            "elevation_range_m": legend.elevation_range_m,
            "brightness": "70 % at elevation_range_m[0] to 100 % at elevation_range_m[1], times hillshade",
            "contour_step_m": job.settings.contour_step_m,
            "missing_rgb": MISSING_RGB,
            "letterbox_rgb": LETTERBOX_RGB,
            "scale_bar_m": legend.scale_bar_m,
            "scale_bar_px": legend.scale_bar_px,
        },
        "markers": markers,
        "tolerance": {
            "max_channel_delta": OVERVIEW_TOLERANCE.max_channel_delta,
            "max_differing_fraction": OVERVIEW_TOLERANCE.max_differing_fraction,
        },
    });
    let digest = descriptor_digest(&descriptor);
    if let Value::Object(map) = &mut descriptor {
        map.insert(DESCRIPTOR_DIGEST_KEY.to_string(), Value::String(digest));
    }
    descriptor
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_beside(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()))
}

fn io(e: std::io::Error) -> SourceError {
    SourceError(format!("height_io_write: {e}"))
}

/// Removes the temporaries when dropped unless disarmed (cancel, error and panic paths alike).
struct Temps(Vec<PathBuf>);

impl Drop for Temps {
    fn drop(&mut self) {
        for path in &self.0 {
            // Best effort: a temporary that cannot be removed is a hidden `.tmp` file, never a
            // published overview.
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Bakes `job` and publishes the PNG at `png` with its descriptor ([`descriptor_path`]), unless
/// [`cached_overview`] finds the pair current. `observer` sees the bake's steps plus one last
/// commit step; [`ControlFlow::Break`] at any of them cancels (`terrain_overview_cancelled`) and
/// nothing is published.
///
/// # Errors
/// The bake's refusals, the cancel, `height_io_write` on a write or rename failure.
#[tracing::instrument(skip_all, target = "regia_terrain")]
pub fn publish_overview(
    job: &OverviewJob<'_>,
    png: &Path,
    mut observer: impl FnMut(OverviewProgress) -> ControlFlow<()>,
) -> Result<PublishedOverview, SourceError> {
    let key = overview_cache_key(job);
    let desc_path = descriptor_path(png);
    if let Some(descriptor) = cached_overview(png, &key) {
        return Ok(PublishedOverview {
            cached: true,
            descriptor,
            png_path: png.to_path_buf(),
            descriptor_path: desc_path,
        });
    }
    let mut last = 0;
    let mut baked = bake_overview_registered(job.source, &job.settings, |p| {
        last = p.steps + 1;
        observer(OverviewProgress {
            step: p.step,
            steps: p.steps + 1,
        })
    })?;
    let placed = draw_markers(&mut baked.image, &baked.mapping, &job.markers);
    let bytes = baked.image.encode_png()?;
    let file_name = png
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let descriptor = descriptor_json(job, &key, &file_name, &bytes, &baked, &placed);
    let text =
        serde_json::to_vec_pretty(&descriptor).map_err(|e| SourceError(format!("height_io_write: {e}")))?;
    if let Some(dir) = png.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut temps = Temps(vec![temp_beside(png), temp_beside(&desc_path)]);
    std::fs::write(&temps.0[0], &bytes).map_err(io)?;
    std::fs::write(&temps.0[1], &text).map_err(io)?;
    if observer(OverviewProgress {
        step: last,
        steps: last,
    })
    .is_break()
    {
        return Err(SourceError(format!(
            "terrain_overview_cancelled: the overview was cancelled before publication (step {last} of {last})"
        )));
    }
    std::fs::rename(&temps.0[0], png).map_err(io)?;
    std::fs::rename(&temps.0[1], &desc_path).map_err(io)?;
    temps.0.clear();
    Ok(PublishedOverview {
        cached: false,
        descriptor,
        png_path: png.to_path_buf(),
        descriptor_path: desc_path,
    })
}
