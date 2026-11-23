// SPDX-License-Identifier: MPL-2.0
//! Cooked packs (ADR-1594): what each entry of a pak costs the runtime to load, the ship rules
//! over a pak, and counters of the forms an [`crate::AssetServer`] actually loaded.
//!
//! A *cooked pack* is a pak every entry of which loads with no source parse and no transcode:
//!
//! - meshes GPU-ready (`crate::gpu_mesh`, desktop paks): the load is one copy of the vertex bytes.
//!   A compact mesh (`CookedMesh`) is unpacked at load. [`crate::CookedMesh::to_cpu_mesh`] writes
//!   tangent xyz `(1, 0, 0)` and the stored sign; the renderer rebuilds MikkTSpace when it draws.
//!   That is the only form on Android, and the form a mesh with a skin or morph targets keeps (a
//!   `COLOR_0` mesh converts, ADR-7919);
//! - textures KTX2 in the platform's GPU block family (BC on desktop, ASTC on Android), with no
//!   KTX2 supercompression: the mip levels go from the pak to the upload queue as stored. When the
//!   device cannot sample the format, the CPU fallback is BC1, BC3, BC4 unorm, BC5 unorm, BC7,
//!   ASTC LDR, and unsigned ETC2 / EAC, decoded to RGBA8 at load. BC2, signed BC4 / BC5, BC6H,
//!   ASTC HDR, and signed EAC stay unbound. A supercompressed texture has no runtime transcoder;
//! - no glTF, PNG or JPEG anywhere: the runtime crates cannot read them (only
//!   `regia_asset_pipeline`, a tool crate, links the glTF and image decoders).
//!
//! `regia cook --ship` writes cooked packs and fails on an [`Severity::Error`] finding of
//! [`check_pak_summary`]; `regia package` runs the same check on the paks it stages for a shipping
//! build; the agent method `assets.cooked_pack` reports [`summarize_pak`] for every mounted pak and
//! the server's [`LoadForms`]. Book: `book/user-guide/cooked-packs.md`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_core::id::TypePathHash;
use serde::Serialize;

use crate::loader::AssetTypeTag;
use crate::pak::{PakPayload, PakReader};

/// The form a mesh entry is stored in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeshForm {
    /// GPU-ready (`crate::gpu_mesh`): the renderer's vertices and indices, every LOD; no decode.
    GpuReady,
    /// Compact `CookedMesh`: quantized streams. Expansion writes tangent xyz `(1, 0, 0)` and the
    /// stored sign; the renderer rebuilds MikkTSpace when it draws.
    Compact,
    /// A `CpuMesh` from a cook before M4 round 3: decoded at load; recook it.
    Legacy,
}

impl MeshForm {
    /// The form of a cooked payload with `type_tag` (`None`: not a mesh).
    #[must_use]
    pub fn of(type_tag: TypePathHash) -> Option<Self> {
        if type_tag == AssetTypeTag::gpu_mesh().0 {
            Some(Self::GpuReady)
        } else if type_tag == AssetTypeTag::mesh().0 {
            Some(Self::Compact)
        } else if type_tag == AssetTypeTag::mesh_legacy_cpu().0 {
            Some(Self::Legacy)
        } else {
            None
        }
    }

    /// Stable label (`gpu_ready`, `compact`, `legacy`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::GpuReady => "gpu_ready",
            Self::Compact => "compact",
            Self::Legacy => "legacy",
        }
    }
}

/// The GPU format family of a KTX2 texture, from its `vkFormat` (Vulkan specification, "Formats":
/// the BC, ETC2/EAC and ASTC enumerant ranges). The same table as the cooker's
/// `regia_asset_pipeline::texture_stats::TextureFamily`, which this crate cannot depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuFamily {
    /// BC1–BC7: desktop GPUs sample it as stored.
    Bc,
    /// ASTC (LDR or HDR): Android and Apple GPUs sample it as stored.
    Astc,
    /// ETC2 / EAC.
    Etc2,
    /// Not block compressed (`R8G8B8A8_UNORM`, …): uploaded as stored.
    Uncompressed,
    /// `VK_FORMAT_UNDEFINED` (a universal format that needs a transcoder) or a format the engine
    /// does not cook.
    Other,
}

impl GpuFamily {
    /// The family of a KTX2 `vkFormat` value (`0` is `VK_FORMAT_UNDEFINED`).
    #[must_use]
    pub fn of(vk_format: u32) -> Self {
        match vk_format {
            0 => Self::Other,
            131..=146 => Self::Bc,
            147..=156 => Self::Etc2,
            157..=184 | 1_000_066_000..=1_000_066_013 => Self::Astc,
            1_000_000_000.. => Self::Other,
            _ => Self::Uncompressed,
        }
    }

    /// Stable label (`bc`, `astc`, `etc2`, `uncompressed`, `other`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Bc => "bc",
            Self::Astc => "astc",
            Self::Etc2 => "etc2",
            Self::Uncompressed => "uncompressed",
            Self::Other => "other",
        }
    }

    /// Whether a texture of this family uploads as stored on `platform` (no CPU decode): BC on
    /// the desktop platforms, ASTC and ETC2 on Android, uncompressed everywhere. An unknown
    /// platform accepts every family but [`Self::Other`]. The tag is compared without regard to
    /// ASCII case (`Android` is `android`, as the cook treats it).
    #[must_use]
    pub fn native_on(self, platform: &str) -> bool {
        match (self, platform.trim().to_ascii_lowercase().as_str()) {
            (Self::Uncompressed, _) => true,
            (Self::Other, _) => false,
            (family, "android") => matches!(family, Self::Astc | Self::Etc2),
            (family, "linux" | "windows" | "macos") => family == Self::Bc,
            _ => true,
        }
    }
}

/// What one KTX2 texture entry holds (read from the KTX2 header: `vkFormat` at byte 12,
/// `supercompressionScheme` at byte 44; Khronos KTX 2.0 specification § 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureForm {
    /// The format family.
    pub family: GpuFamily,
    /// The raw `vkFormat`.
    pub vk_format: u32,
    /// The KTX2 supercompression scheme (`0` = none; `1` BasisLZ, `2` Zstandard, `3` ZLIB).
    pub supercompression: u32,
    /// The cook's mip streaming metadata, when the texture has it.
    pub mip_stream: Option<crate::MipStreamMeta>,
    /// Largest edge of level 0.
    pub full_px: u32,
}

/// Reads the [`TextureForm`] of a cooked texture file (header and payload). `None` when the file
/// is not a cooked texture or its KTX2 does not validate.
#[must_use]
pub fn texture_form(file: &PakPayload) -> Option<TextureForm> {
    let (_, texture) = crate::loader::load_texture_shared(file).ok()?;
    // `load_texture_shared` validated the KTX2 container, so the fixed header fields are present.
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(texture.ktx2.get(at..at + 4)?.try_into().ok()?))
    };
    let vk_format = word(12)?;
    Some(TextureForm {
        family: GpuFamily::of(vk_format),
        vk_format,
        supercompression: word(44)?,
        mip_stream: ktx2::Reader::new(&texture.ktx2[..])
            .ok()
            .and_then(|reader| crate::MipStreamMeta::from_reader(&reader)),
        full_px: texture.width.max(texture.height),
    })
}

/// Most example paths a [`CookedPackFinding`] lists.
pub const FINDING_EXAMPLES: usize = 8;

/// An RGBA8 image entry at least this large (bytes) is reported as `large_uncompressed_images`:
/// 256 × 256 texels. Smaller images (UI icons, LUT strips) are cheap either way.
pub const LARGE_IMAGE_BYTES: u64 = 256 * 256 * 4;

/// Entries and bytes of one kind in a pak.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PakKindStat {
    /// Entries of the kind.
    pub entries: usize,
    /// Uncompressed payload bytes.
    pub bytes: u64,
    /// Bytes the pak stores (lz4 entries are smaller).
    pub stored_bytes: u64,
}

/// The type path of a cooked sound (`regia_audio::CookedSound`, ADR-2788; named here by its string so
/// this crate does not depend on the audio crate).
const COOKED_SOUND_TYPE_PATH: &str = "regia_audio::CookedSound";
/// The type path of the loudness-only record a cook wrote before ADR-2788.
const SOUND_LOUDNESS_TYPE_PATH: &str = "regia_audio::SoundLoudness";

/// What the entries of one pak are, by load cost.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PakSummary {
    /// Pak name (file stem).
    pub name: String,
    /// Entries in the pak.
    pub entries: usize,
    /// Mesh entries by form label (`gpu_ready`, `compact`, `legacy`).
    pub meshes: BTreeMap<&'static str, usize>,
    /// Texture entries.
    pub textures: usize,
    /// Texture entries by [`GpuFamily`] label. Empty unless the textures were inspected.
    pub texture_families: BTreeMap<&'static str, usize>,
    /// Texture entries with KTX2 supercompression (inspected textures only).
    pub supercompressed_textures: usize,
    /// Texture entries without supercompression in a family the platform cannot sample as stored
    /// ([`GpuFamily::native_on`]; inspected textures only).
    pub texture_family_mismatches: usize,
    /// Texture entries with cooked mip streaming metadata (inspected textures only).
    pub mip_metadata_textures: usize,
    /// Of those, textures whose top levels add no detail and stay out of memory.
    pub mip_capped_textures: usize,
    /// Of those, textures an author's `.meta` keeps fully resident.
    pub unstreamed_textures: usize,
    /// Whether every texture entry was read and its KTX2 header inspected.
    pub textures_inspected: bool,
    /// Uncompressed RGBA8 image entries (`CookedImage`).
    pub images_rgba8: usize,
    /// RGBA8 image entries of at least [`LARGE_IMAGE_BYTES`].
    pub large_images_rgba8: usize,
    /// Every other entry (materials, models, skeletons, clips, graphs, terrain, …).
    pub other: usize,
    /// Entries and stored bytes per kind (`mesh_gpu_ready`, `mesh_compact`, `mesh_legacy`, `texture`,
    /// `image`, `audio`, `other`), from the table of contents. `bytes` is the uncompressed payload size
    /// and `stored_bytes` what the pak holds (they differ for an lz4 entry). `audio` counts the cooked
    /// sounds (source bytes plus loudness record, ADR-2788) and the older loudness-only records.
    pub kinds: BTreeMap<&'static str, PakKindStat>,
    /// Submesh ranges in the GPU-ready mesh entries (layout v2, ADR-2788). Counted only with
    /// `inspect_textures`, which reads every mesh header too.
    pub submesh_ranges: usize,
    /// GPU-ready mesh entries with at least one submesh range (inspected paks only).
    pub meshes_with_submeshes: usize,
    /// GPU-ready mesh entries with no submesh range (inspected paks only): a plain one-material mesh, or one packed
    /// in the older layout (v1), whose destruction chunks draw cut faces with the surface material.
    pub meshes_without_submeshes: usize,
    /// Texture entries that could not be read or did not validate.
    pub unreadable: usize,
    /// Up to [`FINDING_EXAMPLES`] manifest paths per finding code, for [`check_pak_summary`].
    #[serde(skip)]
    pub examples: BTreeMap<&'static str, Vec<String>>,
}

impl PakSummary {
    /// Mesh entries of `form`.
    #[must_use]
    pub fn meshes_of(&self, form: MeshForm) -> usize {
        self.meshes.get(form.label()).copied().unwrap_or(0)
    }

    fn note_example(&mut self, code: &'static str, path: Option<&str>) {
        let list = self.examples.entry(code).or_default();
        if let Some(path) = path
            && list.len() < FINDING_EXAMPLES
        {
            list.push(path.to_owned());
        }
    }
}

/// Classifies every entry of `reader`. With `inspect_textures`, each texture entry is read (a view
/// of the mapping for an entry stored raw, a decompression for an lz4 one) and its KTX2 header
/// checked against `platform`; without it only the table of contents is read.
#[must_use]
pub fn summarize_pak(
    reader: &Arc<PakReader>,
    name: &str,
    platform: &str,
    inspect_textures: bool,
) -> PakSummary {
    let _span = tracing::info_span!("cooked_pack_summary", pak = name, inspect_textures).entered();
    let paths: BTreeMap<_, _> = reader
        .toc
        .manifest
        .iter()
        .map(|e| (e.id, e.path.as_str()))
        .collect();
    let mut out = PakSummary {
        name: name.to_owned(),
        entries: reader.toc.entries.len(),
        textures_inspected: inspect_textures,
        ..PakSummary::default()
    };
    let texture = AssetTypeTag::texture().0;
    let image = AssetTypeTag::image().0;
    let sounds = [
        TypePathHash::of(COOKED_SOUND_TYPE_PATH),
        TypePathHash::of(SOUND_LOUDNESS_TYPE_PATH),
    ];
    for entry in &reader.toc.entries {
        let path = paths.get(&entry.id).copied();
        let kind = match MeshForm::of(entry.type_tag) {
            Some(form) => match form {
                MeshForm::GpuReady => "mesh_gpu_ready",
                MeshForm::Compact => "mesh_compact",
                MeshForm::Legacy => "mesh_legacy",
            },
            None if entry.type_tag == texture => "texture",
            None if entry.type_tag == image => "image",
            None if sounds.contains(&entry.type_tag) => "audio",
            None => "other",
        };
        let stat = out.kinds.entry(kind).or_default();
        stat.entries += 1;
        // `size` is what the table of contents declares, unchecked until an entry is read: a
        // hostile pak's sizes must not overflow the sum (ADR-5869).
        stat.bytes = stat.bytes.saturating_add(entry.size);
        stat.stored_bytes = stat.stored_bytes.saturating_add(entry.compressed_size);
        if inspect_textures && kind == "mesh_gpu_ready" {
            let ranges = reader
                .read_shared(entry.id)
                .ok()
                .and_then(|file| {
                    let (_, payload) = crate::decode_cooked_file(&file).ok()?;
                    crate::GpuMeshView::parse(payload).ok().map(|v| v.submeshes.len())
                })
                .unwrap_or(0);
            out.submesh_ranges += ranges;
            if ranges > 0 {
                out.meshes_with_submeshes += 1;
            } else {
                out.meshes_without_submeshes += 1;
            }
        }
        if let Some(form) = MeshForm::of(entry.type_tag) {
            *out.meshes.entry(form.label()).or_default() += 1;
            match form {
                MeshForm::Compact => out.note_example(codes::COMPACT_MESHES, path),
                MeshForm::Legacy => out.note_example(codes::LEGACY_MESHES, path),
                MeshForm::GpuReady => {}
            }
        } else if entry.type_tag == texture {
            out.textures += 1;
            if !inspect_textures {
                continue;
            }
            let form = reader
                .read_shared(entry.id)
                .ok()
                .and_then(|file| texture_form(&file));
            let Some(form) = form else {
                out.unreadable += 1;
                out.note_example(codes::UNREADABLE_TEXTURES, path);
                continue;
            };
            *out.texture_families.entry(form.family.label()).or_default() += 1;
            if let Some(meta) = form.mip_stream {
                out.mip_metadata_textures += 1;
                out.mip_capped_textures += usize::from(meta.detail_px != 0 && meta.detail_px < form.full_px);
                out.unstreamed_textures += usize::from(!meta.stream);
            }
            if form.supercompression != 0 {
                out.supercompressed_textures += 1;
                out.note_example(codes::SUPERCOMPRESSED_TEXTURES, path);
            } else if !form.family.native_on(platform) {
                out.texture_family_mismatches += 1;
                out.note_example(codes::TEXTURE_FAMILY_MISMATCH, path);
            }
        } else if entry.type_tag == image {
            out.images_rgba8 += 1;
            if entry.size >= LARGE_IMAGE_BYTES {
                out.large_images_rgba8 += 1;
                out.note_example(codes::LARGE_UNCOMPRESSED_IMAGES, path);
            }
        } else {
            out.other += 1;
        }
    }
    out
}

/// Finding codes of [`check_pak_summary`] (stable: agents and the CLI match on them).
pub mod codes {
    /// Mesh entries stored compact in a desktop pak (warning).
    pub const COMPACT_MESHES: &str = "compact_meshes";
    /// Mesh entries from a cook before M4 round 3 (error).
    pub const LEGACY_MESHES: &str = "legacy_meshes";
    /// Textures whose family the platform cannot sample as stored (error).
    pub const TEXTURE_FAMILY_MISMATCH: &str = "texture_family_mismatch";
    /// Textures with KTX2 supercompression, which the runtime has no transcoder for (error).
    pub const SUPERCOMPRESSED_TEXTURES: &str = "supercompressed_textures";
    /// Texture entries that do not read or validate (error).
    pub const UNREADABLE_TEXTURES: &str = "unreadable_textures";
    /// RGBA8 images of 256 × 256 texels or more (warning).
    pub const LARGE_UNCOMPRESSED_IMAGES: &str = "large_uncompressed_images";
}

/// How bad a [`CookedPackFinding`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The pak does work at load a cooked pack avoids, by design or by choice (skinned meshes stay
    /// compact; a UI image may stay RGBA8). `regia cook --ship` prints it and succeeds.
    Warning,
    /// An entry the runtime transcodes, cannot load, or loads from an outdated cook. `regia cook
    /// --ship` and a shipping `regia package` fail.
    Error,
}

/// One rule of [`check_pak_summary`] that a pak breaks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CookedPackFinding {
    /// Stable code ([`codes`]).
    pub code: &'static str,
    /// How bad it is.
    pub severity: Severity,
    /// Entries affected.
    pub count: usize,
    /// What it costs and how to fix it.
    pub message: String,
    /// Up to [`FINDING_EXAMPLES`] manifest paths of affected entries.
    pub examples: Vec<String>,
}

/// The cooked-pack rules over one pak's [`PakSummary`] for `platform`, errors first.
#[must_use]
pub fn check_pak_summary(summary: &PakSummary, platform: &str) -> Vec<CookedPackFinding> {
    let mut out = Vec::new();
    let mut push = |code: &'static str, severity, count: usize, message: String| {
        if count > 0 {
            out.push(CookedPackFinding {
                code,
                severity,
                count,
                message,
                examples: summary.examples.get(code).cloned().unwrap_or_default(),
            });
        }
    };
    push(
        codes::LEGACY_MESHES,
        Severity::Error,
        summary.meshes_of(MeshForm::Legacy),
        "meshes from a cook before M4 round 3: recook the project (`regia cook --clean --ship`)".into(),
    );
    push(
        codes::TEXTURE_FAMILY_MISMATCH,
        Severity::Error,
        summary.texture_family_mismatches,
        format!(
            "textures in a format {platform} GPUs cannot sample as stored: the runtime decodes them to RGBA8 on \
             the CPU at load (or cannot load them). Cook the paks for {platform} (`regia cook --platform {platform}`)"
        ),
    );
    push(
        codes::SUPERCOMPRESSED_TEXTURES,
        Severity::Error,
        summary.supercompressed_textures,
        "KTX2 textures with supercompression (BasisLZ, Zstandard, ZLIB): the runtime has no transcoder; cook \
         the source image instead of shipping a prebuilt .ktx2"
            .into(),
    );
    push(
        codes::UNREADABLE_TEXTURES,
        Severity::Error,
        summary.unreadable,
        "texture entries that do not read or validate: recook the project".into(),
    );
    if !platform.trim().eq_ignore_ascii_case("android") {
        push(
            codes::COMPACT_MESHES,
            Severity::Warning,
            summary.meshes_of(MeshForm::Compact),
            "meshes stored compact: each is unpacked and gets tangents at load. `regia cook --ship` (or \
             `--gpu-meshes`) stores them GPU-ready, skinned and morphing meshes included (GPU mesh v3); \
             a pak cooked before v3 keeps those compact until it is cooked again"
                .into(),
        );
    }
    push(
        codes::LARGE_UNCOMPRESSED_IMAGES,
        Severity::Warning,
        summary.large_images_rgba8,
        "RGBA8 images of 256 x 256 or more: uploaded as stored, but four to eight times the memory of a \
         block-compressed texture; set `compress: true` in their .meta"
            .into(),
    );
    out.sort_by_key(|f| std::cmp::Reverse(f.severity));
    out
}

/// Whether any finding is an [`Severity::Error`].
#[must_use]
pub fn has_errors(findings: &[CookedPackFinding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Error)
}

/// Counts of the forms an [`crate::AssetServer`] loaded (and its [`crate::PayloadReader`]s on
/// worker threads). Shared through an `Arc`; every count only grows.
#[derive(Debug, Default)]
pub struct LoadFormCounters {
    gpu_meshes: AtomicU64,
    compact_meshes: AtomicU64,
    legacy_meshes: AtomicU64,
    textures: AtomicU64,
    images_rgba8: AtomicU64,
    pak_loads: AtomicU64,
    cache_loads: AtomicU64,
}

impl LoadFormCounters {
    /// Counts one mesh load of `form`.
    pub fn note_mesh(&self, form: MeshForm) {
        let counter = match form {
            MeshForm::GpuReady => &self.gpu_meshes,
            MeshForm::Compact => &self.compact_meshes,
            MeshForm::Legacy => &self.legacy_meshes,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts one KTX2 texture load.
    pub fn note_texture(&self) {
        self.textures.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts one RGBA8 image load.
    pub fn note_image(&self) {
        self.images_rgba8.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts where one load's bytes came from: a pak (`true`) or the dev cooked cache.
    pub fn note_source(&self, from_pak: bool) {
        let counter = if from_pak {
            &self.pak_loads
        } else {
            &self.cache_loads
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// The counts now.
    #[must_use]
    pub fn snapshot(&self) -> LoadForms {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        LoadForms {
            gpu_meshes: get(&self.gpu_meshes),
            compact_meshes: get(&self.compact_meshes),
            legacy_meshes: get(&self.legacy_meshes),
            textures: get(&self.textures),
            images_rgba8: get(&self.images_rgba8),
            pak_loads: get(&self.pak_loads),
            cache_loads: get(&self.cache_loads),
            textures_cpu_decoded: textures_cpu_decoded(),
        }
    }
}

/// A snapshot of [`LoadFormCounters`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LoadForms {
    /// GPU-ready meshes loaded (no decode).
    pub gpu_meshes: u64,
    /// Compact meshes loaded (unpacked, tangents generated).
    pub compact_meshes: u64,
    /// Legacy `CpuMesh` entries loaded.
    pub legacy_meshes: u64,
    /// KTX2 textures loaded.
    pub textures: u64,
    /// RGBA8 images loaded.
    pub images_rgba8: u64,
    /// Mesh, texture and image loads served by a mounted pak.
    pub pak_loads: u64,
    /// Mesh, texture and image loads served by the dev cooked cache (`.regia/cooked`): none in a
    /// shipped game.
    pub cache_loads: u64,
    /// Block-compressed textures decoded to RGBA8 on the CPU because the device cannot sample
    /// their format, process-wide ([`note_texture_cpu_decode`]).
    pub textures_cpu_decoded: u64,
}

impl LoadForms {
    /// Whether everything counted loaded as a cooked pack promises: no compact or legacy mesh, no
    /// CPU texture decode, nothing from the dev cache.
    #[must_use]
    pub fn cooked_only(&self) -> bool {
        self.compact_meshes == 0
            && self.legacy_meshes == 0
            && self.textures_cpu_decoded == 0
            && self.cache_loads == 0
    }
}

static TEXTURES_CPU_DECODED: AtomicU64 = AtomicU64::new(0);

/// Counts one block-compressed texture decoded to RGBA8 on the CPU (the renderer calls this when
/// a device cannot sample a texture's format). Process-wide: the renderer holds no server.
pub fn note_texture_cpu_decode() {
    TEXTURES_CPU_DECODED.fetch_add(1, Ordering::Relaxed);
}

/// Process-wide count of [`note_texture_cpu_decode`] calls.
#[must_use]
pub fn textures_cpu_decoded() -> u64 {
    TEXTURES_CPU_DECODED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sec-review-assets-agent (ADR-5869) finding 3: the summary summed each entry's declared
    /// `size`, which the reader does not check until the entry is read; two entries of 2^63 bytes
    /// overflowed the sum (a panic with overflow checks on: the dev and test profiles).
    #[test]
    fn hostile_declared_sizes_do_not_overflow_the_summary() {
        use crate::{FPAK_BLOB_ALIGNMENT, FPAK_VERSION, PakEntry, PakToc, encode_pak_header};
        let mut data = vec![0_u8; 60];
        data.resize(data.len().next_multiple_of(FPAK_BLOB_ALIGNMENT as usize), 0);
        let offset = data.len() as u64;
        data.extend_from_slice(&[0xAB; 16]);
        let entry = |n: u128| PakEntry {
            id: crate::AssetId(regia_core::id::Guid(n)),
            type_tag: TypePathHash::of("regia_asset::Blob"),
            offset,
            size: 1 << 63,
            compressed_size: 16,
            block_table_offset: 0,
        };
        let toc = PakToc {
            entries: vec![entry(1), entry(2)],
            manifest: Vec::new(),
            dependencies: Vec::new(),
        };
        let toc_offset = data.len() as u64;
        let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
        data.extend_from_slice(&toc_bytes);
        let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
        let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_bytes.len() as u64, &hash);
        data[..60].copy_from_slice(&header);
        let len = data.len() as u64;
        let reader = Arc::new(
            PakReader::from_bytes(data, "hostile.fpak", len).expect("the TOC itself is well formed"),
        );
        let summary = summarize_pak(&reader, "hostile", "linux", true);
        assert_eq!(summary.entries, 2);
        assert_eq!(summary.kinds.get("other").map(|k| k.bytes), Some(u64::MAX));
    }

    #[test]
    fn families_follow_the_vulkan_ranges() {
        assert_eq!(GpuFamily::of(0), GpuFamily::Other);
        assert_eq!(GpuFamily::of(37), GpuFamily::Uncompressed, "R8G8B8A8_UNORM");
        assert_eq!(GpuFamily::of(131), GpuFamily::Bc, "BC1_RGB_UNORM");
        assert_eq!(GpuFamily::of(146), GpuFamily::Bc, "BC7_SRGB");
        assert_eq!(GpuFamily::of(147), GpuFamily::Etc2);
        assert_eq!(GpuFamily::of(157), GpuFamily::Astc, "ASTC_4x4_UNORM");
        assert_eq!(GpuFamily::of(1_000_066_000), GpuFamily::Astc, "ASTC HDR");
        assert_eq!(GpuFamily::of(1_000_054_000), GpuFamily::Other, "PVRTC");
    }

    #[test]
    fn native_families_per_platform() {
        assert!(GpuFamily::Bc.native_on("linux"));
        assert!(GpuFamily::Bc.native_on("windows"));
        assert!(!GpuFamily::Bc.native_on("android"));
        assert!(GpuFamily::Astc.native_on("android"));
        assert!(!GpuFamily::Astc.native_on("linux"));
        assert!(GpuFamily::Uncompressed.native_on("android"));
        assert!(!GpuFamily::Other.native_on("linux"));
        // audit-seq (High): `Android` fell to the unknown-platform arm and accepted BC.
        assert!(!GpuFamily::Bc.native_on("Android"));
        assert!(GpuFamily::Astc.native_on("ANDROID"));
        assert!(!GpuFamily::Astc.native_on("Linux"));
    }

    #[test]
    fn mesh_forms_come_from_the_type_tag() {
        assert_eq!(MeshForm::of(AssetTypeTag::gpu_mesh().0), Some(MeshForm::GpuReady));
        assert_eq!(MeshForm::of(AssetTypeTag::mesh().0), Some(MeshForm::Compact));
        assert_eq!(
            MeshForm::of(AssetTypeTag::mesh_legacy_cpu().0),
            Some(MeshForm::Legacy)
        );
        assert_eq!(MeshForm::of(AssetTypeTag::texture().0), None);
    }

    fn summary(meshes: &[(MeshForm, usize)], families: &[(GpuFamily, usize)], platform: &str) -> PakSummary {
        let mut s = PakSummary {
            name: "base".into(),
            textures_inspected: true,
            ..PakSummary::default()
        };
        for (form, n) in meshes {
            s.meshes.insert(form.label(), *n);
        }
        for (family, n) in families {
            s.texture_families.insert(family.label(), *n);
            s.textures += n;
            if !family.native_on(platform) {
                s.texture_family_mismatches += n;
            }
        }
        s
    }

    #[test]
    fn a_cooked_desktop_pak_has_no_findings() {
        let s = summary(&[(MeshForm::GpuReady, 3)], &[(GpuFamily::Bc, 5)], "linux");
        assert!(check_pak_summary(&s, "linux").is_empty());
    }

    #[test]
    fn android_tag_case_does_not_change_the_compact_mesh_finding() {
        let s = summary(&[(MeshForm::Compact, 2)], &[(GpuFamily::Astc, 1)], "Android");
        assert!(check_pak_summary(&s, "Android").is_empty());
        assert!(check_pak_summary(&s, " ANDROID ").is_empty());
        let desktop = check_pak_summary(&s, "Linux");
        assert_eq!(desktop.len(), 1, "{desktop:?}");
        assert_eq!(desktop[0].code, codes::COMPACT_MESHES);
    }

    #[test]
    fn bc_on_android_is_an_error_and_compact_meshes_are_fine_there() {
        let s = summary(
            &[(MeshForm::Compact, 2)],
            &[(GpuFamily::Bc, 4), (GpuFamily::Astc, 1)],
            "android",
        );
        let findings = check_pak_summary(&s, "android");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].code, codes::TEXTURE_FAMILY_MISMATCH);
        assert_eq!(findings[0].count, 4);
        assert!(has_errors(&findings));
    }

    #[test]
    fn compact_meshes_on_desktop_only_warn() {
        let s = summary(&[(MeshForm::Compact, 2), (MeshForm::GpuReady, 1)], &[], "windows");
        let findings = check_pak_summary(&s, "windows");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, codes::COMPACT_MESHES);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(!has_errors(&findings));
    }

    #[test]
    fn errors_sort_before_warnings() {
        let mut s = summary(&[(MeshForm::Compact, 1), (MeshForm::Legacy, 1)], &[], "linux");
        s.large_images_rgba8 = 1;
        // A supercompressed texture counts in its family but not as a mismatch.
        s.supercompressed_textures = 1;
        s.texture_families.insert(GpuFamily::Other.label(), 1);
        s.textures = 1;
        let findings = check_pak_summary(&s, "linux");
        let order: Vec<_> = findings.iter().map(|f| (f.code, f.severity)).collect();
        assert_eq!(
            order,
            [
                (codes::LEGACY_MESHES, Severity::Error),
                (codes::SUPERCOMPRESSED_TEXTURES, Severity::Error),
                (codes::COMPACT_MESHES, Severity::Warning),
                (codes::LARGE_UNCOMPRESSED_IMAGES, Severity::Warning),
            ],
            "a supercompressed texture is reported once, not also as a family mismatch"
        );
    }

    #[test]
    fn counters_count_and_judge() {
        let counters = LoadFormCounters::default();
        counters.note_mesh(MeshForm::GpuReady);
        counters.note_texture();
        counters.note_source(true);
        counters.note_source(true);
        let forms = counters.snapshot();
        assert_eq!((forms.gpu_meshes, forms.textures, forms.pak_loads), (1, 1, 2));
        // The CPU decode counter is process-wide, so judge the per-server part only here.
        assert!(forms.compact_meshes == 0 && forms.cache_loads == 0);
        counters.note_mesh(MeshForm::Compact);
        assert!(!counters.snapshot().cooked_only());
    }
}
