// SPDX-License-Identifier: MPL-2.0
//! What the cooked textures of one platform's cache are encoded as (ADR-527): per texture its
//! KTX2 format, ASTC block size, mip count and bytes, and totals per format family. `regia cook
//! --stats` prints it (`--json` for tools).
//!
//! The check that matters on Android: every block-compressed texture should be ASTC there (the
//! phones upload it as is, ADR-144), and BC there means the cache or pak was cooked for desktop
//! (the runtime decodes BC to RGBA8 on the CPU, four to eight times the memory). Desktop caches get
//! the opposite check. Format families are the Vulkan `VkFormat` ranges (Vulkan specification,
//! "Formats").

use std::path::Path;

use regia_asset::{AssetManifest, CookedTexture, decode_cooked_file};
use serde::Serialize;

/// The manifest type path of cooked textures.
const COOKED_TEXTURE_TYPE_PATH: &str = "regia_asset::CookedTexture";

/// A format family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureFamily {
    /// ASTC (LDR or HDR).
    Astc,
    /// BC1–BC7.
    Bc,
    /// ETC2 / EAC.
    Etc2,
    /// Not block compressed.
    Uncompressed,
    /// `VK_FORMAT_UNDEFINED` (a supercompressed universal format) or a format not listed here.
    Other,
}

impl TextureFamily {
    /// The family of a KTX2 `vkFormat` value (`None` = `VK_FORMAT_UNDEFINED`).
    #[must_use]
    pub fn of(vk_format: Option<u32>) -> Self {
        match vk_format {
            None => Self::Other,
            Some(131..=146) => Self::Bc,
            Some(147..=156) => Self::Etc2,
            Some(157..=184 | 1_000_066_000..=1_000_066_013) => Self::Astc,
            // PVRTC (1000054000..) and anything newer: not a family the engine cooks.
            Some(1_000_000_000..) => Self::Other,
            Some(_) => Self::Uncompressed,
        }
    }

    /// Display label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Astc => "ASTC",
            Self::Bc => "BC",
            Self::Etc2 => "ETC2",
            Self::Uncompressed => "uncompressed",
            Self::Other => "other",
        }
    }

    /// Whether `family` is a family `platform`'s cook writes: ASTC or ETC2 / EAC on Android (the
    /// project's `[cook.textures.android]` picks one, ADR-2063), BC on desktop, any elsewhere.
    #[must_use]
    pub fn native_on(family: Self, platform: &str) -> bool {
        match platform {
            "android" => matches!(family, Self::Astc | Self::Etc2),
            _ => Self::expected_for(platform).is_none_or(|e| e == family),
        }
    }

    /// The block-compressed family `platform`'s default cook writes (`None`: any is fine).
    #[must_use]
    pub fn expected_for(platform: &str) -> Option<Self> {
        match platform {
            "android" => Some(Self::Astc),
            "linux" | "windows" | "macos" => Some(Self::Bc),
            _ => None,
        }
    }
}

/// `ASTC_6x6_SRGB_BLOCK` → `6x6`.
fn astc_block(format_name: &str) -> Option<String> {
    let rest = format_name.strip_prefix("ASTC_")?;
    let block = rest.split('_').next()?;
    let (w, h) = block.split_once('x')?;
    (w.parse::<u8>().is_ok() && h.parse::<u8>().is_ok()).then(|| block.to_owned())
}

/// One cooked texture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextureFormatRow {
    /// Project-relative source path.
    pub path: String,
    /// Width of mip 0.
    pub width: u32,
    /// Height of mip 0.
    pub height: u32,
    /// Mip levels stored.
    pub levels: u32,
    /// The KTX2 format name (`ASTC_4x4_SRGB_BLOCK`, `BC7_SRGB_BLOCK`, `R8G8B8A8_UNORM`, …).
    pub format: String,
    /// Its family.
    pub family: TextureFamily,
    /// ASTC block footprint (`4x4`, `6x6`, …).
    pub astc_block: Option<String>,
    /// KTX2 bytes.
    pub bytes: u64,
}

/// Totals for one family (ASTC is split by block size).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextureFamilyTotal {
    /// `ASTC 4x4`, `BC`, `uncompressed`, …
    pub label: String,
    /// The family.
    pub family: TextureFamily,
    /// Textures.
    pub textures: usize,
    /// KTX2 bytes.
    pub bytes: u64,
}

/// The report for one cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextureFormatStats {
    /// The platform the cache is for.
    pub platform: String,
    /// Every cooked texture, by path.
    pub textures: Vec<TextureFormatRow>,
    /// Totals, largest first.
    pub totals: Vec<TextureFamilyTotal>,
    /// Block-compressed textures in a family the platform does not cook to (paths).
    pub wrong_platform: Vec<String>,
    /// Textures that could not be read (path: error).
    pub unreadable: Vec<String>,
}

/// The mip that survives `max_size`, using the same skip as the DDS importer.
///
/// Leading levels drop while the larger edge is above `max_size`. The last stored
/// level always stays, so a 64×64 file with levels 64 and 32 and `max_size` 8 stays
/// 32×32. A zero input edge stays zero. `levels` of 0 is treated as one level.
#[must_use]
pub fn surviving_mip_edge(width: u32, height: u32, levels: u32, max_size: Option<u32>) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let levels = levels.max(1);
    let max = max_size.filter(|edge| *edge > 0).unwrap_or(u32::MAX);
    let mut skip = 0_u32;
    while skip + 1 < levels && (width >> skip).max(height >> skip) > max {
        skip += 1;
    }
    ((width >> skip).max(1), (height >> skip).max(1))
}

/// One row from a cooked texture's KTX2 bytes.
///
/// # Errors
///
/// The KTX2 container does not parse.
pub fn texture_row(path: &str, texture: &CookedTexture) -> Result<TextureFormatRow, String> {
    if let Some(reason) = regia_asset::hostile_ktx2_dfd(&texture.ktx2[..]) {
        return Err(reason.to_owned());
    }
    let reader = ktx2::Reader::new(&texture.ktx2[..]).map_err(|e| format!("{e:?}"))?;
    let header = reader.header();
    let format = header
        .format
        .map_or_else(|| "UNDEFINED".to_owned(), |f| format!("{f:?}"));
    Ok(TextureFormatRow {
        path: path.to_owned(),
        width: header.pixel_width,
        height: header.pixel_height,
        levels: header.level_count.max(1),
        family: TextureFamily::of(header.format.map(|f| f.value())),
        astc_block: astc_block(&format),
        format,
        bytes: texture.ktx2.len() as u64,
    })
}

/// Totals and the wrong-platform list from the rows.
#[must_use]
pub fn summarize(
    platform: &str,
    mut textures: Vec<TextureFormatRow>,
    unreadable: Vec<String>,
) -> TextureFormatStats {
    textures.sort_by(|a, b| a.path.cmp(&b.path));
    let mut totals: Vec<TextureFamilyTotal> = Vec::new();
    for row in &textures {
        let label = match &row.astc_block {
            Some(block) => format!("ASTC {block}"),
            None => row.family.label().to_owned(),
        };
        match totals.iter_mut().find(|t| t.label == label) {
            Some(t) => {
                t.textures += 1;
                t.bytes += row.bytes;
            }
            None => totals.push(TextureFamilyTotal {
                label,
                family: row.family,
                textures: 1,
                bytes: row.bytes,
            }),
        }
    }
    totals.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));
    let wrong_platform = textures
        .iter()
        .filter(|r| {
            matches!(
                r.family,
                TextureFamily::Astc | TextureFamily::Bc | TextureFamily::Etc2
            ) && !TextureFamily::native_on(r.family, platform)
        })
        .map(|r| r.path.clone())
        .collect();
    TextureFormatStats {
        platform: platform.to_owned(),
        textures,
        totals,
        wrong_platform,
        unreadable,
    }
}

/// Reads every cooked texture in `cache_dir` (the cooked cache of `platform`). `Ok(None)` when
/// the cache has no manifest or no textures.
///
/// # Errors
///
/// The manifest exists but cannot be read.
pub fn texture_format_stats(cache_dir: &Path, platform: &str) -> Result<Option<TextureFormatStats>, String> {
    let _span = tracing::info_span!("texture_format_stats").entered();
    let Some(manifest) = AssetManifest::read_from_cache(cache_dir).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let mut rows = Vec::new();
    let mut unreadable = Vec::new();
    for entry in manifest
        .entries
        .iter()
        .filter(|e| e.type_path == COOKED_TEXTURE_TYPE_PATH)
    {
        let file = cache_dir.join(format!("{}.fckd", entry.id));
        let row = std::fs::read(&file).map_err(|e| e.to_string()).and_then(|bytes| {
            let (_, payload) = decode_cooked_file(&bytes).map_err(|e| e.to_string())?;
            let texture = CookedTexture::from_postcard(payload).map_err(|e| e.to_string())?;
            texture_row(&entry.path, &texture)
        });
        match row {
            Ok(row) => rows.push(row),
            Err(err) => unreadable.push(format!("{}: {err}", entry.path)),
        }
    }
    if rows.is_empty() && unreadable.is_empty() {
        return Ok(None);
    }
    Ok(Some(summarize(platform, rows, unreadable)))
}

/// The human-readable report.
#[must_use]
pub fn format_texture_stats(stats: &TextureFormatStats) -> String {
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let mut out = format!(
        "cooked textures ({}): {} textures, {:.2} MiB\n",
        stats.platform,
        stats.textures.len(),
        mib(stats.textures.iter().map(|t| t.bytes).sum())
    );
    for t in &stats.totals {
        out.push_str(&format!(
            "  {:<16} {:>5} textures {:>9.2} MiB\n",
            t.label,
            t.textures,
            mib(t.bytes)
        ));
    }
    if !stats.wrong_platform.is_empty() {
        out.push_str(&format!(
            "  warning: textures_cooked_for_other_platform: {} texture(s) are not {} (first: {})\n",
            stats.wrong_platform.len(),
            if stats.platform == "android" {
                "ASTC or ETC2"
            } else {
                TextureFamily::expected_for(&stats.platform).map_or("?", TextureFamily::label)
            },
            stats.wrong_platform[0]
        ));
    }
    for u in &stats.unreadable {
        out.push_str(&format!("  unreadable: {u}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surviving_edge_keeps_the_last_stored_level() {
        assert_eq!(surviving_mip_edge(64, 64, 2, Some(8)), (32, 32));
        assert_eq!(surviving_mip_edge(64, 64, 1, Some(8)), (64, 64));
        assert_eq!(surviving_mip_edge(64, 64, 2, None), (64, 64));
        assert_eq!(surviving_mip_edge(0, 64, 2, Some(8)), (0, 0));
    }

    fn row(path: &str, format: &str, family: TextureFamily, bytes: u64) -> TextureFormatRow {
        TextureFormatRow {
            path: path.into(),
            width: 64,
            height: 64,
            levels: 7,
            format: format.into(),
            family,
            astc_block: astc_block(format),
            bytes,
        }
    }

    #[test]
    fn families_follow_the_vulkan_format_ranges() {
        let of = |f: ktx2::Format| TextureFamily::of(Some(f.value()));
        assert_eq!(of(ktx2::Format::BC1_RGB_UNORM_BLOCK), TextureFamily::Bc);
        assert_eq!(of(ktx2::Format::BC7_SRGB_BLOCK), TextureFamily::Bc);
        assert_eq!(of(ktx2::Format::ETC2_R8G8B8_UNORM_BLOCK), TextureFamily::Etc2);
        assert_eq!(of(ktx2::Format::ASTC_4x4_UNORM_BLOCK), TextureFamily::Astc);
        assert_eq!(of(ktx2::Format::ASTC_12x12_SRGB_BLOCK), TextureFamily::Astc);
        assert_eq!(of(ktx2::Format::R8G8B8A8_SRGB), TextureFamily::Uncompressed);
        assert_eq!(TextureFamily::of(None), TextureFamily::Other);
    }

    #[test]
    fn astc_block_names_parse() {
        assert_eq!(astc_block("ASTC_6x6_SRGB_BLOCK").as_deref(), Some("6x6"));
        assert_eq!(astc_block("ASTC_10x8_UNORM_BLOCK").as_deref(), Some("10x8"));
        assert_eq!(astc_block("BC7_SRGB_BLOCK"), None);
        assert_eq!(astc_block("ASTC_junk"), None);
    }

    #[test]
    fn summary_splits_astc_by_block_and_flags_desktop_formats_on_android() {
        let stats = summarize(
            "android",
            vec![
                row("b.png", "ASTC_6x6_SRGB_BLOCK", TextureFamily::Astc, 300),
                row("a.png", "ASTC_4x4_SRGB_BLOCK", TextureFamily::Astc, 1000),
                row("c.png", "ASTC_4x4_UNORM_BLOCK", TextureFamily::Astc, 500),
                row("d.png", "BC7_SRGB_BLOCK", TextureFamily::Bc, 900),
                row("ui.png", "R8G8B8A8_SRGB", TextureFamily::Uncompressed, 64),
            ],
            vec!["e.png: bad ktx2".into()],
        );
        assert_eq!(stats.textures[0].path, "a.png", "sorted by path");
        let totals: Vec<(&str, usize, u64)> = stats
            .totals
            .iter()
            .map(|t| (t.label.as_str(), t.textures, t.bytes))
            .collect();
        assert_eq!(
            totals,
            [
                ("ASTC 4x4", 2, 1500),
                ("BC", 1, 900),
                ("ASTC 6x6", 1, 300),
                ("uncompressed", 1, 64)
            ]
        );
        assert_eq!(stats.wrong_platform, ["d.png"]);
        let text = format_texture_stats(&stats);
        assert!(
            text.contains(
                "textures_cooked_for_other_platform: 1 texture(s) are not ASTC or ETC2 (first: d.png)"
            ),
            "{text}"
        );
        assert!(text.contains("unreadable: e.png: bad ktx2"), "{text}");
    }

    #[test]
    fn desktop_caches_expect_bc_and_unknown_platforms_expect_nothing() {
        let rows = || vec![row("a.png", "ASTC_4x4_SRGB_BLOCK", TextureFamily::Astc, 10)];
        assert_eq!(summarize("linux", rows(), vec![]).wrong_platform, ["a.png"]);
        assert!(summarize("web", rows(), vec![]).wrong_platform.is_empty());
        // An ETC2 cook (`[cook.textures.android] format = "etc2"`, ADR-2063) is native on Android
        // and wrong on desktop.
        let etc2 = || vec![row("e.png", "ETC2_R8G8B8_SRGB_BLOCK", TextureFamily::Etc2, 10)];
        assert!(summarize("android", etc2(), vec![]).wrong_platform.is_empty());
        assert_eq!(summarize("windows", etc2(), vec![]).wrong_platform, ["e.png"]);
    }

    #[test]
    fn a_cache_without_manifest_has_no_stats() {
        let dir = regia_test_utils::unique_temp_dir("texture-stats");
        assert_eq!(texture_format_stats(&dir, "android"), Ok(None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End to end on a real Android cook of one PNG: the row reads back as ASTC with mips.
    #[test]
    fn reads_an_android_cooked_texture() {
        let rgba = vec![200u8; 16 * 16 * 4];
        let texture = crate::texture_compress::cook_texture_rgba8(
            16,
            16,
            &rgba,
            regia_asset::TextureCookUsage::ColorSrgb,
            "android",
            crate::texture_compress::TextureCookQuality::High,
        )
        .expect("android cook");
        let row = texture_row("t.png", &texture).expect("row");
        assert_eq!(row.family, TextureFamily::Astc);
        assert_eq!(row.astc_block.as_deref(), Some("4x4"));
        assert!(row.levels > 1, "mips: {row:?}");
    }

    // --- Independent aggregation oracle (deeper validation of the v0.3.0-v0.5.0 reporting). ---

    /// A small deterministic PRNG (LCG, Numerical Recipes constants) so permutations and byte
    /// sizes are reproducible run to run without any wall clock or hardware dependence.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 11
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A deterministic permutation of `items` (Fisher-Yates driven by the LCG).
    fn permuted(items: &[TextureFormatRow], seed: u64) -> Vec<TextureFormatRow> {
        let mut out = items.to_vec();
        let mut rng = Lcg(seed);
        for i in (1..out.len()).rev() {
            let j = rng.below(i as u64 + 1) as usize;
            out.swap(i, j);
        }
        out
    }

    /// The expected totals computed by an independent grouping: a `BTreeMap` keyed by the report
    /// label (`ASTC 4x4`, `BC`, ...), ordered by the report's rule (descending bytes, then label
    /// ascending on ties). Written straight from the contract, not by calling `summarize`.
    fn oracle_totals(rows: &[TextureFormatRow]) -> Vec<TextureFamilyTotal> {
        let mut groups: std::collections::BTreeMap<String, (TextureFamily, usize, u64)> =
            std::collections::BTreeMap::new();
        for r in rows {
            let label = match &r.astc_block {
                Some(block) => format!("ASTC {block}"),
                None => r.family.label().to_owned(),
            };
            let entry = groups.entry(label).or_insert((r.family, 0, 0));
            entry.1 += 1;
            entry.2 += r.bytes;
        }
        let mut totals: Vec<TextureFamilyTotal> = groups
            .into_iter()
            .map(|(label, (family, textures, bytes))| TextureFamilyTotal {
                label,
                family,
                textures,
                bytes,
            })
            .collect();
        totals.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));
        totals
    }

    /// The expected wrong-platform paths for `platform`, computed straight from the platform
    /// policy: block-compressed families the platform's cook does not write.
    fn oracle_wrong_platform(rows: &[TextureFormatRow], platform: &str) -> Vec<String> {
        let mut paths: Vec<String> = rows
            .iter()
            .filter(|r| {
                matches!(
                    r.family,
                    TextureFamily::Astc | TextureFamily::Bc | TextureFamily::Etc2
                ) && !TextureFamily::native_on(r.family, platform)
            })
            .map(|r| r.path.clone())
            .collect();
        paths.sort();
        paths
    }

    /// A mixed population: BC / ASTC (four block sizes) / ETC2 + EAC / uncompressed / unknown
    /// (`Other`), with zero-byte rows so every family keeps a represented zero total.
    fn mixed_population() -> Vec<TextureFormatRow> {
        let mut rng = Lcg(0x5EED);
        let specs: &[(&str, TextureFamily)] = &[
            ("BC1_RGB_SRGB_BLOCK", TextureFamily::Bc),
            ("BC3_SRGB_BLOCK", TextureFamily::Bc),
            ("BC5_UNORM_BLOCK", TextureFamily::Bc),
            ("BC7_SRGB_BLOCK", TextureFamily::Bc),
            ("ASTC_4x4_SRGB_BLOCK", TextureFamily::Astc),
            ("ASTC_4x4_UNORM_BLOCK", TextureFamily::Astc),
            ("ASTC_5x5_SRGB_BLOCK", TextureFamily::Astc),
            ("ASTC_6x6_SRGB_BLOCK", TextureFamily::Astc),
            ("ASTC_6x6_UNORM_BLOCK", TextureFamily::Astc),
            ("ASTC_8x8_SRGB_BLOCK", TextureFamily::Astc),
            ("ETC2_R8G8B8_SRGB_BLOCK", TextureFamily::Etc2),
            ("ETC2_R8G8B8A8_UNORM_BLOCK", TextureFamily::Etc2),
            ("EAC_R11_UNORM_BLOCK", TextureFamily::Etc2),
            ("EAC_R11G11_UNORM_BLOCK", TextureFamily::Etc2),
            ("R8G8B8A8_SRGB", TextureFamily::Uncompressed),
            ("R8_UNORM", TextureFamily::Uncompressed),
            ("UNDEFINED", TextureFamily::Other),
        ];
        let mut rows = Vec::with_capacity(specs.len() + 4);
        for (i, (format, family)) in specs.iter().enumerate() {
            let bytes = rng.below(4096);
            rows.push(row(&format!("tex/{i:02}.png"), format, *family, bytes));
        }
        // Zero-byte rows: a family total of 0 bytes must still be listed.
        rows.push(row("zero/bc.png", "BC7_UNORM_BLOCK", TextureFamily::Bc, 0));
        rows.push(row(
            "zero/astc.png",
            "ASTC_4x4_UNORM_BLOCK",
            TextureFamily::Astc,
            0,
        ));
        rows.push(row(
            "zero/etc2.png",
            "ETC2_R8G8B8_UNORM_BLOCK",
            TextureFamily::Etc2,
            0,
        ));
        rows.push(row(
            "zero/raw.png",
            "R8G8B8A8_UNORM",
            TextureFamily::Uncompressed,
            0,
        ));
        rows
    }

    /// `summarize` must match an independently grouped oracle for every platform and every
    /// permutation of the input rows: family counts, exact byte sums, descending-bytes totals
    /// with label ties, path sorting, wrong-platform membership and unreadable passthrough.
    #[test]
    fn summary_matches_an_independent_grouping_oracle_across_permutations() {
        let population = mixed_population();
        let mut sorted_paths: Vec<String> = population.iter().map(|r| r.path.clone()).collect();
        sorted_paths.sort();
        let total_bytes: u64 = population.iter().map(|r| r.bytes).sum();
        // Unreadable entries pass through in input order (the report contract preserves them).
        let unreadable = vec![
            "broken/one.png: ktx2 magic".to_owned(),
            "broken/two.png: unexpected eof".to_owned(),
        ];
        for platform in ["linux", "windows", "macos", "android", "haiku"] {
            let expected_totals = oracle_totals(&population);
            let expected_wrong = oracle_wrong_platform(&population, platform);
            // Sanity anchors on top of the oracle: desktops expect BC, Android expects ASTC/ETC2,
            // an unknown platform expects nothing.
            let bc_paths: Vec<String> = population
                .iter()
                .filter(|r| r.family == TextureFamily::Bc)
                .map(|r| r.path.clone())
                .collect();
            match platform {
                "android" => assert_eq!(expected_wrong, bc_paths),
                "linux" | "windows" | "macos" => {
                    let mut non_bc = population
                        .iter()
                        .filter(|r| matches!(r.family, TextureFamily::Astc | TextureFamily::Etc2))
                        .map(|r| r.path.clone())
                        .collect::<Vec<_>>();
                    non_bc.sort();
                    assert_eq!(expected_wrong, non_bc);
                }
                _ => assert!(expected_wrong.is_empty()),
            }
            for seed in [0u64, 1, 7, 42, 1_000_003] {
                let stats = summarize(platform, permuted(&population, seed), unreadable.clone());
                let paths: Vec<&str> = stats.textures.iter().map(|t| t.path.as_str()).collect();
                assert_eq!(paths, sorted_paths, "platform {platform} seed {seed}: sorted");
                assert_eq!(stats.totals, expected_totals, "platform {platform} seed {seed}");
                assert_eq!(
                    stats.wrong_platform, expected_wrong,
                    "platform {platform} seed {seed}"
                );
                assert_eq!(
                    stats.textures.iter().map(|t| t.bytes).sum::<u64>(),
                    total_bytes,
                    "platform {platform} seed {seed}: no bytes lost or double-counted"
                );
                assert_eq!(stats.unreadable, unreadable, "unreadable order preserved");
            }
        }
    }

    /// Two labels with exactly equal byte sums order by label ascending.
    #[test]
    fn equal_byte_ties_break_by_label() {
        let stats = summarize(
            "android",
            vec![
                row("a.png", "ETC2_R8G8B8_SRGB_BLOCK", TextureFamily::Etc2, 100),
                row("b.png", "BC7_SRGB_BLOCK", TextureFamily::Bc, 100),
            ],
            vec![],
        );
        let labels: Vec<&str> = stats.totals.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["BC", "ETC2"], "equal bytes: label ascending");
    }

    /// Rows whose bytes are all zero still appear, ordered by label.
    #[test]
    fn all_zero_byte_rows_are_listed_last_by_label() {
        let stats = summarize(
            "android",
            vec![
                row("z2.png", "BC7_SRGB_BLOCK", TextureFamily::Bc, 0),
                row("z1.png", "ASTC_8x8_UNORM_BLOCK", TextureFamily::Astc, 0),
            ],
            vec![],
        );
        let labels: Vec<&str> = stats.totals.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["ASTC 8x8", "BC"], "all bytes 0: label ascending");
        assert!(stats.totals.iter().all(|t| t.bytes == 0 && t.textures == 1));
    }

    /// Partitioning the rows must not change anything: raw rows recombined give the single
    /// summary exactly, and per-partition summaries sum to it (counts, bytes, totals per label,
    /// wrong-platform union, unreadable list) with nothing double-counted.
    #[test]
    fn partitions_recombine_to_the_single_summary() {
        let population = mixed_population();
        let unreadable = vec![
            "broken/one.png: ktx2 magic".to_owned(),
            "broken/two.png: unexpected eof".to_owned(),
            "broken/three.png: dfd".to_owned(),
        ];
        let platform = "android";
        let single = summarize(platform, population.clone(), unreadable.clone());

        let mut row_parts: Vec<Vec<TextureFormatRow>> = vec![Vec::new(); 3];
        for (i, r) in population.iter().enumerate() {
            row_parts[i % 3].push(r.clone());
        }
        let mut unreadable_parts: Vec<Vec<String>> = vec![Vec::new(); 3];
        for (i, u) in unreadable.iter().enumerate() {
            unreadable_parts[i % 3].push(u.clone());
        }

        // Recombined raw rows reproduce the single summary exactly (summarize sorts, so the
        // partition order does not matter).
        let recombined = summarize(
            platform,
            row_parts.concat(),
            unreadable_parts.iter().flatten().cloned().collect(),
        );
        assert_eq!(recombined, single);

        // Per-partition summaries sum to the single summary without double counting.
        let parts: Vec<TextureFormatStats> = row_parts
            .iter()
            .cloned()
            .zip(unreadable_parts.iter().cloned())
            .map(|(rows, un)| summarize(platform, rows, un))
            .collect();
        assert_eq!(
            parts.iter().map(|p| p.textures.len()).sum::<usize>(),
            single.textures.len()
        );
        assert_eq!(
            parts
                .iter()
                .flat_map(|p| p.textures.iter().map(|t| t.bytes))
                .sum::<u64>(),
            single.textures.iter().map(|t| t.bytes).sum::<u64>()
        );
        let mut combined: std::collections::BTreeMap<String, (usize, u64)> =
            std::collections::BTreeMap::new();
        for p in &parts {
            for t in &p.totals {
                let entry = combined.entry(t.label.clone()).or_insert((0, 0));
                entry.0 += t.textures;
                entry.1 += t.bytes;
            }
        }
        let single_map: std::collections::BTreeMap<String, (usize, u64)> = single
            .totals
            .iter()
            .map(|t| (t.label.clone(), (t.textures, t.bytes)))
            .collect();
        assert_eq!(combined, single_map, "partition totals sum to the single totals");
        let mut union_wrong: Vec<String> = parts
            .iter()
            .flat_map(|p| p.wrong_platform.iter().cloned())
            .collect();
        union_wrong.sort();
        assert_eq!(union_wrong, single.wrong_platform, "wrong-platform union");
        assert_eq!(
            parts
                .iter()
                .flat_map(|p| p.unreadable.iter().cloned())
                .collect::<Vec<_>>(),
            single.unreadable,
            "unreadable entries split across partitions recombine exactly once"
        );
    }

    /// An empty population reports nothing but still formats.
    #[test]
    fn empty_populations_report_nothing() {
        let stats = summarize("android", vec![], vec![]);
        assert!(stats.textures.is_empty());
        assert!(stats.totals.is_empty());
        assert!(stats.wrong_platform.is_empty());
        assert!(stats.unreadable.is_empty());
        let text = format_texture_stats(&stats);
        assert!(text.contains("0 textures, 0.00 MiB"), "{text}");
        assert!(!text.contains("warning:"), "{text}");
        assert!(!text.contains("unreadable:"), "{text}");
    }

    /// `texture_row` on bounded fixtures from the pipeline's own KTX2 writer: the header's format,
    /// mip count and dimensions are reflected, and the byte count is the complete container.
    #[test]
    fn texture_row_reads_bounded_writer_fixtures_exactly() {
        // BC7 sRGB, 8x8, two levels: 4 blocks (64 B) then 1 block (16 B).
        let bc7 = crate::write_ktx2_bcn(8, 8, crate::BcnFormat::Bc7Srgb, &[vec![0u8; 64], vec![0u8; 16]])
            .expect("bc7 ktx2");
        let row = texture_row("bc7.png", &cooked_texture(&bc7)).expect("bc7 row");
        assert_eq!(row.format, "BC7_SRGB_BLOCK");
        assert_eq!(row.family, TextureFamily::Bc);
        assert!(row.astc_block.is_none());
        assert_eq!((row.width, row.height, row.levels), (8, 8, 2));
        assert_eq!(row.bytes, bc7.len() as u64, "complete container byte count");

        // ASTC 6x6 linear, 12x12, one level: 2x2 blocks of 16 B.
        let astc = crate::write_ktx2_astc(12, 12, crate::AstcBlock::Block6x6, false, &[vec![0u8; 64]])
            .expect("astc ktx2");
        let row = texture_row("astc.png", &cooked_texture(&astc)).expect("astc row");
        assert_eq!(row.format, "ASTC_6x6_UNORM_BLOCK");
        assert_eq!(row.family, TextureFamily::Astc);
        assert_eq!(row.astc_block.as_deref(), Some("6x6"));
        assert_eq!((row.width, row.height, row.levels), (12, 12, 1));
        assert_eq!(row.bytes, astc.len() as u64);

        // ETC2 RGB8 sRGB, 4x4, one 8-byte block.
        let etc2 = crate::write_ktx2_etc2(4, 4, crate::etc2::Etc2Kind::Rgb8, true, &[vec![0u8; 8]])
            .expect("etc2 ktx2");
        let row = texture_row("etc2.png", &cooked_texture(&etc2)).expect("etc2 row");
        assert_eq!(row.format, "ETC2_R8G8B8_SRGB_BLOCK");
        assert_eq!(row.family, TextureFamily::Etc2);
        assert_eq!((row.width, row.height, row.levels), (4, 4, 1));
        assert_eq!(row.bytes, etc2.len() as u64);
    }

    /// Malformed KTX2 payloads (truncated container, garbage, empty) return a useful error.
    #[test]
    fn texture_row_rejects_malformed_ktx2_with_a_useful_error() {
        let bc7 = crate::write_ktx2_bcn(4, 4, crate::BcnFormat::Bc7Srgb, &[vec![0u8; 16]]).expect("bc7 ktx2");
        let mut truncated = bc7.clone();
        truncated.truncate(bc7.len() - 3);
        for (name, bytes) in [
            ("truncated", truncated),
            ("garbage", vec![0xAAu8; 64]),
            ("empty", Vec::new()),
        ] {
            let err = texture_row(name, &cooked_texture(&bytes)).expect_err(name);
            assert!(!err.is_empty(), "{name}: the error must carry detail");
        }
    }

    /// sec-review-assets-agent (ADR-5869): a DFD texel-block dimension of 0xFF panicked the ktx2
    /// parser (an abort under panic = "abort"); `texture_row` takes a `CookedTexture` a caller may
    /// have built without `validate`, so it runs the guard itself.
    #[test]
    fn texture_row_refuses_a_hostile_dfd() {
        let mut bc7 =
            crate::write_ktx2_bcn(4, 4, crate::BcnFormat::Bc7Srgb, &[vec![0u8; 16]]).expect("bc7 ktx2");
        let dfd_offset = u32::from_le_bytes(bc7[48..52].try_into().expect("4 bytes")) as usize;
        bc7[dfd_offset + 4 + ktx2::dfd::BlockHeader::LENGTH + 4] = 0xFF;
        let err = texture_row("hostile", &cooked_texture(&bc7)).expect_err("hostile");
        assert!(err.contains("texel-block dimension"), "{err}");
        let err = crate::texture_compress::transcode_bcn_ktx2_to_astc(
            &bc7,
            crate::TextureCookQuality::default(),
            crate::texture_compress::AstcContent::Rgb,
        )
        .expect_err("hostile");
        assert!(err.to_string().contains("texel-block dimension"), "{err}");
    }

    /// Long and non-ASCII paths remain fully represented in the formatted report: every offender
    /// appears and no path corrupts or merges output lines.
    #[test]
    fn format_stats_represent_long_and_non_ascii_paths() {
        let long = format!("assets/{}", "d".repeat(300));
        let rows = vec![
            row(&long, "BC7_SRGB_BLOCK", TextureFamily::Bc, 10),
            row(
                "テクスチャ/色_ü.png",
                "ASTC_4x4_SRGB_BLOCK",
                TextureFamily::Astc,
                20,
            ),
            row("🦀/crab.png", "BC5_UNORM_BLOCK", TextureFamily::Bc, 30),
            row(
                "naïve_normal.png",
                "R8G8B8A8_UNORM",
                TextureFamily::Uncompressed,
                40,
            ),
        ];
        let unreadable = vec![
            format!("{long}: ktx2 truncated"),
            "🦀/crab.png: not a ktx2".to_owned(),
        ];
        let stats = summarize("android", rows, unreadable.clone());
        let text = format_texture_stats(&stats);
        for offender in stats.wrong_platform.iter().chain(stats.unreadable.iter()) {
            assert!(
                text.contains(offender.as_str()),
                "missing {offender:?} in:\n{text}"
            );
        }
        // Header, one line per total, one warning line, one line per unreadable entry.
        let expected_lines = 1 + stats.totals.len() + 1 + stats.unreadable.len();
        assert_eq!(
            text.lines().count(),
            expected_lines,
            "no path may corrupt or merge lines:\n{text}"
        );
    }

    /// Wrap KTX2 container bytes in a cooked texture the way a pak entry would.
    fn cooked_texture(ktx2: &[u8]) -> CookedTexture {
        CookedTexture {
            width: 0,
            height: 0,
            usage: regia_asset::TextureCookUsage::ColorSrgb,
            ktx2: ktx2.to_vec().into(),
        }
    }
}
