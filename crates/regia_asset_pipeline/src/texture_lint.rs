// SPDX-License-Identifier: MPL-2.0
//! Source and cooked texture evidence for the shared lint stage (ADR-8536).
//!
//! The probe reads container headers only. It checks source dimensions against the decode
//! budget before any pixel buffer would be reserved, then reports mip coverage, block-family
//! compatibility, role and color space, and the expected decoded RGBA byte count. Unknown
//! sizes stay unknown. This module does not resize source art and does not allocate a
//! decoded image.
//!
//! Header layouts follow the public KTX 2.0 specification and the documented DDS file layout.
//! See `docs/provenance.md`.

use std::io::Read;
use std::path::Path;

use regia_asset::TextureCookUsage;

use crate::texture_policy::TexturePolicy;
use crate::texture_settings::{TextureImporterSettings, usage_from_file_name};
use crate::texture_stats::{TextureFamily, surviving_mip_edge};

/// The source container could not be measured. Strict budget evidence fails.
pub const RULE_BUDGET_UNKNOWN: &str = "texture_budget_unknown";
/// A header dimension, mip count, or decoded size cannot be admitted.
pub const RULE_IMPOSSIBLE_DIMENSIONS: &str = "texture_impossible_dimensions";
/// A pass-through block chain is shorter than the pyramid of its surviving top level.
pub const RULE_MISSING_MIP: &str = "texture_missing_mip";
/// Container color space and texture roles disagree, or one file mixes color and data roles.
pub const RULE_USAGE_MISMATCH: &str = "texture_usage_mismatch";
/// The block family is not the family this platform's cook policy writes.
pub const RULE_WRONG_COMPRESSION: &str = "texture_wrong_compression";

/// Largest edge the DDS importer accepts. Duplicated here so the probe does not decode.
const MAX_EDGE: u32 = 1 << 15;
/// Bytes of header inspected when a glTF image is not the file being hashed.
const HEADER_PREFIX: usize = 4096;
const DDS_MAGIC: &[u8; 4] = b"DDS ";
const DDS_HEADER_LEN: usize = 124;
const DDS_DX10_LEN: usize = 20;
const DDSD_MIPMAPCOUNT: u32 = 0x2_0000;
const DDPF_FOURCC: u32 = 0x4;
const DDPF_RGB: u32 = 0x40;
const DDSCAPS2_CUBEMAP: u32 = 0x200;
const DDSCAPS2_VOLUME: u32 = 0x20_0000;
const D3D10_RESOURCE_DIMENSION_TEXTURE2D: u32 = 3;
const D3D10_RESOURCE_DIMENSION_TEXTURE3D: u32 = 4;
const KTX2_IDENTIFIER: [u8; 12] = [
    0xAB, 0x4B, 0x54, 0x58, 0x20, 0x32, 0x30, 0xBB, 0x0D, 0x0A, 0x1A, 0x0A,
];

/// One texture the shared stage can classify without a pixel decode.
#[derive(Clone, Debug)]
pub struct TextureLintRequest<'a> {
    /// Project-relative path. The extension selects the header parser.
    pub path: &'a str,
    /// Header bytes. A prefix is enough when [`Self::file_len`] is the real length.
    pub bytes: &'a [u8],
    /// Whole-file length. `None` means `bytes` is the whole file.
    pub file_len: Option<u64>,
    /// Cook platform: `android`, `linux`, `windows`, or `macos`.
    pub platform: &'a str,
    /// Project texture policy. Desktop still expects BC.
    pub policy: TexturePolicy,
    /// The source `.meta` settings. `max_size` selects the surviving mip.
    pub settings: TextureImporterSettings,
    /// Extra roles (a glTF image used by more than one slot). Empty uses the file name.
    pub usages: &'a [TextureCookUsage],
    /// Decode admission ceiling. Expected RGBA bytes above this are not reserved.
    pub max_decoded_bytes: u64,
}

/// One texture finding. The shared stage copies it into its own finding row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureLintFinding {
    /// Project-relative source path.
    pub path: String,
    /// Stable rule id.
    pub rule_id: String,
    /// Header location (`/format`, `/mips`, `/usage`, `/header/dimensions`, or `/`).
    pub location: String,
    /// What was measured.
    pub message: String,
}

/// Header evidence plus the admission counters. `buffers_allocated` stays zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureLintOutcome {
    /// Surviving width when the header named a nonzero edge. Never a stored zero.
    pub width: Option<u32>,
    /// Surviving height when the header named a nonzero edge. Never a stored zero.
    pub height: Option<u32>,
    /// Expected decoded RGBA bytes (`width * height * 4`) when that product is known.
    pub decoded_bytes: Option<u64>,
    /// Findings. One fixture in the acceptance test produces one rule.
    pub findings: Vec<TextureLintFinding>,
    /// Bytes reserved for a decode. Zero when the header is refused or unknown.
    pub decoded_bytes_reserved: u64,
    /// Pixel buffers allocated. The probe never allocates one.
    pub buffers_allocated: u64,
    /// True when a required size could not be measured. Strict evidence then fails.
    pub incomplete: bool,
    /// True when the probe rewrote source pixels. It never does.
    pub rewrites_source: bool,
    /// Existing `.meta` fingerprint for the reimport preview. Not pixel bytes.
    pub reimport_preview: Option<String>,
}

/// Whether `path` is a texture source the lint stage classifies.
#[must_use]
pub fn is_texture_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "bmp" | "tga" | "dds" | "ktx2"
            )
        })
}

/// Cook platform for a lint target. `default` and any other name use desktop BC (`linux`).
#[must_use]
pub fn platform_for_target(target: &str) -> &'static str {
    match target {
        "android" => "android",
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        _ => "linux",
    }
}

/// Policy and decode-budget text mixed into the evidence settings hash.
///
/// # Errors
/// `Regia.toml` exists but the texture policy cannot be read.
pub fn project_stamp(project: &Path, target: &str) -> Result<String, String> {
    let platform = platform_for_target(target);
    let policy = TexturePolicy::for_project(project, platform)?;
    let budget = crate::content_lint::ContentLimits::default().max_decoded_bytes;
    Ok(format!(
        "platform={platform};policy={};budget={budget}",
        policy.key_text()
    ))
}

/// Fingerprint of the `.meta` settings beside `project`/`rel`.
#[must_use]
pub fn source_stamp(project: &Path, rel: &str) -> String {
    TextureImporterSettings::for_source(&project.join(rel)).fingerprint()
}

/// Reads at most 4096 header bytes plus the file length. This is not a pixel buffer.
///
/// # Errors
/// The file could not be opened or read.
pub fn read_texture_prefix(path: &Path) -> Result<(Vec<u8>, u64), String> {
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        .len();
    let take = usize::try_from(len.min(u64::try_from(HEADER_PREFIX).unwrap_or(u64::MAX))).unwrap_or(0);
    let mut buf = vec![0_u8; take];
    let mut filled = 0_usize;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(count) => filled = filled.saturating_add(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(format!("cannot read {}: {error}", path.display()));
            }
        }
    }
    buf.truncate(filled);
    Ok((buf, len))
}

/// Image URIs in a glTF JSON document and the roles that reference each URI.
///
/// Roles are deduplicated. A data URI is omitted. The function does not open files.
#[must_use]
pub fn gltf_external_usages(json: &[u8]) -> Vec<(String, Vec<TextureCookUsage>)> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(images) = value.get("images").and_then(|item| item.as_array()) else {
        return Vec::new();
    };
    let textures = value.get("textures").and_then(|item| item.as_array());
    let mut roles: Vec<(String, Vec<TextureCookUsage>)> = Vec::new();
    let Some(materials) = value.get("materials").and_then(|item| item.as_array()) else {
        return roles;
    };
    for material in materials {
        note_texture(
            &mut roles,
            images,
            textures,
            material,
            &["pbrMetallicRoughness", "baseColorTexture"],
            TextureCookUsage::ColorSrgb,
        );
        note_texture(
            &mut roles,
            images,
            textures,
            material,
            &["emissiveTexture"],
            TextureCookUsage::ColorSrgb,
        );
        note_texture(
            &mut roles,
            images,
            textures,
            material,
            &["normalTexture"],
            TextureCookUsage::NormalMap,
        );
        note_texture(
            &mut roles,
            images,
            textures,
            material,
            &["pbrMetallicRoughness", "metallicRoughnessTexture"],
            TextureCookUsage::ColorLinear,
        );
        note_texture(
            &mut roles,
            images,
            textures,
            material,
            &["occlusionTexture"],
            TextureCookUsage::Mask,
        );
    }
    roles.sort_by(|left, right| left.0.cmp(&right.0));
    roles
}

/// Usage-mismatch findings for a glTF whose images exist and carry two or more roles.
///
/// Missing URIs are skipped. The source file is not rewritten.
///
/// # Errors
/// The project texture policy or an existing image could not be read.
pub fn shared_usage_findings(
    project: &Path,
    source_rel: &str,
    document: &[u8],
    target: &str,
) -> Result<Vec<TextureLintFinding>, String> {
    let Some(json) = document_json(source_rel, document) else {
        return Ok(Vec::new());
    };
    let pairs: Vec<_> = gltf_external_usages(json)
        .into_iter()
        .filter(|(_, roles)| roles.len() >= 2)
        .collect();
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let platform = platform_for_target(target);
    let policy = TexturePolicy::for_project(project, platform)?;
    let budget = crate::content_lint::ContentLimits::default().max_decoded_bytes;
    let mut findings = Vec::new();
    for (uri, roles) in pairs {
        let Some(rel) = image_rel(source_rel, &uri) else {
            continue;
        };
        let abs = project.join(&rel);
        if !abs.is_file() {
            continue;
        }
        let (prefix, len) = read_texture_prefix(&abs)?;
        let settings = TextureImporterSettings::for_source(&abs);
        let outcome = assess(&TextureLintRequest {
            path: &rel,
            bytes: &prefix,
            file_len: Some(len),
            platform,
            policy,
            settings,
            usages: &roles,
            max_decoded_bytes: budget,
        });
        findings.extend(
            outcome
                .findings
                .into_iter()
                .filter(|finding| finding.rule_id == RULE_USAGE_MISMATCH),
        );
    }
    Ok(findings)
}

/// Classifies one texture header. Does not allocate a decoded buffer or rewrite `bytes`.
#[must_use]
pub fn assess(request: &TextureLintRequest<'_>) -> TextureLintOutcome {
    let mut outcome = TextureLintOutcome {
        width: None,
        height: None,
        decoded_bytes: None,
        findings: Vec::new(),
        decoded_bytes_reserved: 0,
        buffers_allocated: 0,
        incomplete: false,
        rewrites_source: false,
        reimport_preview: Some(request.settings.fingerprint()),
    };
    let file_len = request
        .file_len
        .unwrap_or_else(|| u64::try_from(request.bytes.len()).unwrap_or(u64::MAX));
    let parsed = match container_kind(request.path) {
        Some("png") => parse_png(request.bytes, file_len),
        Some("jpg" | "jpeg") => parse_jpeg(request.bytes, file_len),
        Some("bmp") => parse_bmp(request.bytes, file_len),
        Some("tga") => parse_tga(request.bytes, file_len),
        Some("dds") => parse_dds_header(request.bytes, file_len),
        Some("ktx2") => parse_ktx2(request.bytes, file_len),
        _ => Parsed::Unknown("the extension is not a texture container"),
    };
    match parsed {
        Parsed::Unknown(message) => mark_unknown(&mut outcome, request.path, message),
        Parsed::Illegal {
            width,
            height,
            message,
        } => {
            store_nonzero(&mut outcome, width, height);
            push_finding(
                &mut outcome,
                request.path,
                RULE_IMPOSSIBLE_DIMENSIONS,
                "/header/dimensions",
                message,
            );
        }
        Parsed::Ready(info) => finish_ready(&mut outcome, request, &info),
    }
    outcome
}

struct Info {
    width: u32,
    height: u32,
    levels: u32,
    family: TextureFamily,
    srgb: Option<bool>,
    checks_mips: bool,
    payload_short: bool,
    unmeasured: Option<&'static str>,
}

enum Parsed {
    Unknown(&'static str),
    Illegal {
        width: u32,
        height: u32,
        message: String,
    },
    Ready(Info),
}

fn finish_ready(outcome: &mut TextureLintOutcome, request: &TextureLintRequest<'_>, info: &Info) {
    if let Some(reason) = info.unmeasured {
        mark_unknown(outcome, request.path, reason);
        return;
    }
    if info.width == 0 || info.height == 0 || info.width > MAX_EDGE || info.height > MAX_EDGE {
        push_finding(
            outcome,
            request.path,
            RULE_IMPOSSIBLE_DIMENSIONS,
            "/header/dimensions",
            format!("size {}×{} is outside 1..={MAX_EDGE}", info.width, info.height),
        );
        return;
    }
    let levels = info.levels.max(1);
    let chain = full_chain(info.width, info.height);
    if levels > chain {
        push_finding(
            outcome,
            request.path,
            RULE_IMPOSSIBLE_DIMENSIONS,
            "/header/dimensions",
            format!(
                "{levels} mip levels for a {}×{} texture (at most {chain})",
                info.width, info.height
            ),
        );
        return;
    }
    let (width, height) = surviving_mip_edge(info.width, info.height, levels, request.settings.max_size);
    store_nonzero(outcome, width, height);
    let Some(expected) = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|px| px.checked_mul(4))
    else {
        mark_unknown(
            outcome,
            request.path,
            "the decoded size does not fit in the evidence counter",
        );
        return;
    };
    outcome.decoded_bytes = Some(expected);
    if expected > request.max_decoded_bytes {
        push_finding(
            outcome,
            request.path,
            RULE_IMPOSSIBLE_DIMENSIONS,
            "/header/dimensions",
            format!(
                "decoded size {expected} bytes exceeds the budget of {} bytes",
                request.max_decoded_bytes
            ),
        );
        return;
    }
    if info.payload_short {
        // A short block payload is not a measured image. Drop the provisional size.
        outcome.width = None;
        outcome.height = None;
        outcome.decoded_bytes = None;
        mark_unknown(
            outcome,
            request.path,
            "the mip payload is shorter than the header declares",
        );
        return;
    }
    outcome.decoded_bytes_reserved = expected;
    // `compress: Some(false)` decodes level 0 and cooks a new chain, so a short
    // stored chain is not a missing mip.
    let passthrough = info.checks_mips && request.settings.compress();
    if passthrough && levels >= 2 {
        let kept = kept_levels(info.width, info.height, levels, request.settings.max_size);
        let need = full_chain(width, height);
        if kept < need {
            push_finding(
                outcome,
                request.path,
                RULE_MISSING_MIP,
                "/mips",
                format!("pass-through keeps {kept} level(s) at {width}×{height}; a full chain has {need}"),
            );
        }
    }
    if matches!(
        info.family,
        TextureFamily::Astc | TextureFamily::Bc | TextureFamily::Etc2
    ) && !request.policy.accepts_family(info.family, request.platform)
    {
        push_finding(
            outcome,
            request.path,
            RULE_WRONG_COMPRESSION,
            "/format",
            format!(
                "{} is not the block family {} cooks ({})",
                info.family.label(),
                request.platform,
                request.policy.android_format.label()
            ),
        );
    }
    let roles = roles_of(request);
    if usage_mismatch(info.srgb, &roles) {
        push_finding(
            outcome,
            request.path,
            RULE_USAGE_MISMATCH,
            "/usage",
            format!("color space {:?} does not match roles {roles:?}", info.srgb),
        );
    }
}

fn mark_unknown(outcome: &mut TextureLintOutcome, path: &str, message: &str) {
    outcome.incomplete = true;
    outcome.width = None;
    outcome.height = None;
    outcome.decoded_bytes = None;
    outcome.decoded_bytes_reserved = 0;
    outcome
        .findings
        .retain(|finding| finding.rule_id != RULE_BUDGET_UNKNOWN);
    push_finding(outcome, path, RULE_BUDGET_UNKNOWN, "/", message);
}

fn store_nonzero(outcome: &mut TextureLintOutcome, width: u32, height: u32) {
    outcome.width = (width > 0).then_some(width);
    outcome.height = (height > 0).then_some(height);
}

fn push_finding(
    outcome: &mut TextureLintOutcome,
    path: &str,
    rule_id: &str,
    location: &str,
    message: impl Into<String>,
) {
    outcome.findings.push(TextureLintFinding {
        path: path.to_owned(),
        rule_id: rule_id.to_owned(),
        location: location.to_owned(),
        message: message.into(),
    });
}

fn roles_of(request: &TextureLintRequest<'_>) -> Vec<TextureCookUsage> {
    let mut roles = Vec::new();
    if let Some(setting) = request.settings.usage {
        push_role(&mut roles, TextureCookUsage::from(setting));
    }
    if request.usages.is_empty() {
        if roles.is_empty() {
            push_role(&mut roles, usage_from_file_name(Path::new(request.path)));
        }
    } else {
        for usage in request.usages {
            push_role(&mut roles, *usage);
        }
    }
    roles
}

fn push_role(roles: &mut Vec<TextureCookUsage>, usage: TextureCookUsage) {
    if !roles.contains(&usage) {
        roles.push(usage);
    }
}

fn is_srgb_role(usage: TextureCookUsage) -> bool {
    matches!(usage, TextureCookUsage::ColorSrgb | TextureCookUsage::Ui)
}

fn usage_mismatch(container_srgb: Option<bool>, roles: &[TextureCookUsage]) -> bool {
    if roles.is_empty() {
        return false;
    }
    let wants_srgb = roles.iter().copied().any(is_srgb_role);
    let wants_data = roles.iter().copied().any(|role| !is_srgb_role(role));
    if wants_srgb && wants_data {
        return true;
    }
    match container_srgb {
        None => false,
        Some(true) => wants_data,
        Some(false) => wants_srgb,
    }
}

fn full_chain(width: u32, height: u32) -> u32 {
    let edge = width.max(height).max(1);
    32 - edge.leading_zeros()
}

fn kept_levels(width: u32, height: u32, levels: u32, max_size: Option<u32>) -> u32 {
    let levels = levels.max(1);
    let max = max_size.filter(|edge| *edge > 0).unwrap_or(u32::MAX);
    let mut skip = 0_u32;
    while skip + 1 < levels && (width >> skip).max(height >> skip) > max {
        skip += 1;
    }
    levels.saturating_sub(skip).max(1)
}

fn extension_of(path: &str) -> Option<&str> {
    Path::new(path).extension().and_then(|ext| ext.to_str())
}

fn container_kind(path: &str) -> Option<&'static str> {
    let ext = extension_of(path)?;
    let lower = ext.to_ascii_lowercase();
    Some(match lower.as_str() {
        "png" => "png",
        "jpg" => "jpg",
        "jpeg" => "jpeg",
        "bmp" => "bmp",
        "tga" => "tga",
        "dds" => "dds",
        "ktx2" => "ktx2",
        _ => return None,
    })
}

fn claim_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    bytes
        .get(offset..end)
        .and_then(|slice| slice.try_into().ok())
        .map(u32::from_le_bytes)
}

fn claim_u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    bytes
        .get(offset..end)
        .and_then(|slice| slice.try_into().ok())
        .map(u32::from_be_bytes)
}

fn claim_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    bytes
        .get(offset..end)
        .and_then(|slice| slice.try_into().ok())
        .map(u16::from_le_bytes)
}

fn claim_u16_be(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    bytes
        .get(offset..end)
        .and_then(|slice| slice.try_into().ok())
        .map(u16::from_be_bytes)
}

fn illegal_edge(width: u32, height: u32) -> Option<Parsed> {
    if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE {
        Some(Parsed::Illegal {
            width,
            height,
            message: format!("size {width}×{height} is outside 1..={MAX_EDGE}"),
        })
    } else {
        None
    }
}

fn has_prefix(bytes: &[u8], magic: &[u8]) -> bool {
    bytes.len() >= magic.len() && &bytes[..magic.len()] == magic
}

fn parse_png(bytes: &[u8], file_len: u64) -> Parsed {
    const SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    if bytes.len() < 24 || !has_prefix(bytes, &SIG) || bytes.get(12..16) != Some(b"IHDR".as_slice()) {
        return Parsed::Unknown("the PNG signature or IHDR is missing");
    }
    let Some(width) = claim_u32_be(bytes, 16) else {
        return Parsed::Unknown("the PNG width is missing");
    };
    let Some(height) = claim_u32_be(bytes, 20) else {
        return Parsed::Unknown("the PNG height is missing");
    };
    if let Some(illegal) = illegal_edge(width, height) {
        return illegal;
    }
    // Signature + IHDR chunk + IEND chunk. Pixel bytes are not required and are not read.
    let payload_short = file_len < 45;
    Parsed::Ready(Info {
        width,
        height,
        levels: 1,
        family: TextureFamily::Uncompressed,
        srgb: None,
        checks_mips: false,
        payload_short,
        unmeasured: None,
    })
}

fn parse_jpeg(bytes: &[u8], file_len: u64) -> Parsed {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Parsed::Unknown("the JPEG start marker is missing");
    }
    let mut index = 2_usize;
    while index + 1 < bytes.len() {
        if bytes[index] != 0xFF {
            index += 1;
            continue;
        }
        while index < bytes.len() && bytes[index] == 0xFF {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let marker = bytes[index];
        index += 1;
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            continue;
        }
        let Some(seg_len) = claim_u16_be(bytes, index) else {
            break;
        };
        let seg_len = usize::from(seg_len);
        if matches!(marker, 0xC0..=0xC2) {
            let Some(height) = claim_u16_be(bytes, index.saturating_add(3)) else {
                return Parsed::Unknown("the JPEG frame size is missing");
            };
            let Some(width) = claim_u16_be(bytes, index.saturating_add(5)) else {
                return Parsed::Unknown("the JPEG frame size is missing");
            };
            let width = u32::from(width);
            let height = u32::from(height);
            if let Some(illegal) = illegal_edge(width, height) {
                return illegal;
            }
            return Parsed::Ready(Info {
                width,
                height,
                levels: 1,
                family: TextureFamily::Uncompressed,
                srgb: None,
                checks_mips: false,
                payload_short: file_len < 4,
                unmeasured: None,
            });
        }
        if seg_len < 2 {
            return Parsed::Unknown("a JPEG segment length is shorter than itself");
        }
        index = index.saturating_add(seg_len);
    }
    Parsed::Unknown("the JPEG frame header was not in the inspected prefix")
}

fn parse_bmp(bytes: &[u8], file_len: u64) -> Parsed {
    if bytes.len() < 26 || !has_prefix(bytes, b"BM") {
        return Parsed::Unknown("the BMP signature is missing");
    }
    let Some(width) = claim_u32(bytes, 18) else {
        return Parsed::Unknown("the BMP width is missing");
    };
    let Some(height_bits) = claim_u32(bytes, 22) else {
        return Parsed::Unknown("the BMP height is missing");
    };
    let height = height_bits & 0x7FFF_FFFF;
    if let Some(illegal) = illegal_edge(width, height) {
        return illegal;
    }
    Parsed::Ready(Info {
        width,
        height,
        levels: 1,
        family: TextureFamily::Uncompressed,
        srgb: None,
        checks_mips: false,
        payload_short: file_len < 26,
        unmeasured: None,
    })
}

fn parse_tga(bytes: &[u8], file_len: u64) -> Parsed {
    if bytes.len() < 18 || file_len < 18 {
        return Parsed::Unknown("the TGA header is shorter than 18 bytes");
    }
    let kind = bytes[2];
    if !matches!(kind, 1..=3 | 9..=11) {
        return Parsed::Unknown("the TGA image type is not a measured 2D image");
    }
    let Some(width) = claim_u16(bytes, 12) else {
        return Parsed::Unknown("the TGA width is missing");
    };
    let Some(height) = claim_u16(bytes, 14) else {
        return Parsed::Unknown("the TGA height is missing");
    };
    let width = u32::from(width);
    let height = u32::from(height);
    if let Some(illegal) = illegal_edge(width, height) {
        return illegal;
    }
    Parsed::Ready(Info {
        width,
        height,
        levels: 1,
        family: TextureFamily::Uncompressed,
        srgb: None,
        checks_mips: false,
        payload_short: false,
        unmeasured: None,
    })
}

fn parse_dds_header(bytes: &[u8], file_len: u64) -> Parsed {
    if bytes.len() < 4 || !has_prefix(bytes, DDS_MAGIC) {
        return Parsed::Unknown("the DDS magic is missing");
    }
    if bytes.len() < 20 {
        return Parsed::Unknown("the DDS header does not contain width and height");
    }
    let Some(height) = claim_u32(bytes, 12) else {
        return Parsed::Unknown("the DDS height is missing");
    };
    let Some(width) = claim_u32(bytes, 16) else {
        return Parsed::Unknown("the DDS width is missing");
    };
    if let Some(illegal) = illegal_edge(width, height) {
        return illegal;
    }
    if file_len < u64::try_from(4 + DDS_HEADER_LEN).unwrap_or(u64::MAX) || bytes.len() < 4 + DDS_HEADER_LEN {
        return Parsed::Unknown("the DDS header is truncated");
    }
    if claim_u32(bytes, 4) != Some(DDS_HEADER_LEN as u32) {
        return Parsed::Unknown("the DDS header size field is wrong");
    }
    let flags = claim_u32(bytes, 8).unwrap_or(0);
    let levels = if flags & DDSD_MIPMAPCOUNT != 0 {
        claim_u32(bytes, 28).unwrap_or(1).max(1)
    } else {
        1
    };
    let chain = full_chain(width, height);
    if levels > chain {
        return Parsed::Illegal {
            width,
            height,
            message: format!("{levels} mip levels for a {width}×{height} texture (at most {chain})"),
        };
    }
    let caps2 = claim_u32(bytes, 4 + 108).unwrap_or(0);
    if caps2 & DDSCAPS2_VOLUME != 0 {
        return ready_unmeasured(width, height, levels, "a volume texture has no measured 2D size");
    }
    if caps2 & DDSCAPS2_CUBEMAP != 0 {
        return ready_unmeasured(
            width,
            height,
            levels,
            "a cube map is not measured as one 2D image",
        );
    }
    let pf_flags = claim_u32(bytes, 4 + 76).unwrap_or(0);
    let fourcc = bytes.get(84..88).unwrap_or(&[]);
    let (family, srgb, block_bytes, header_end) = if pf_flags & DDPF_FOURCC != 0 {
        match fourcc {
            b"DXT1" => (TextureFamily::Bc, None, 8_u32, 4 + DDS_HEADER_LEN),
            b"DXT2" | b"DXT3" | b"DXT4" | b"DXT5" => (TextureFamily::Bc, None, 16, 4 + DDS_HEADER_LEN),
            b"ATI1" | b"BC4U" => (TextureFamily::Bc, Some(false), 8, 4 + DDS_HEADER_LEN),
            b"ATI2" | b"BC5U" => (TextureFamily::Bc, Some(false), 16, 4 + DDS_HEADER_LEN),
            b"DX10" => {
                let dx10_at = 4 + DDS_HEADER_LEN;
                if bytes.len() < dx10_at + DDS_DX10_LEN
                    || file_len < u64::try_from(dx10_at + DDS_DX10_LEN).unwrap_or(u64::MAX)
                {
                    return Parsed::Unknown("the DX10 header is missing");
                }
                let dxgi = claim_u32(bytes, dx10_at).unwrap_or(0);
                let dimension = claim_u32(bytes, dx10_at + 4).unwrap_or(0);
                if dimension == D3D10_RESOURCE_DIMENSION_TEXTURE3D {
                    return ready_unmeasured(
                        width,
                        height,
                        levels,
                        "a volume texture has no measured 2D size",
                    );
                }
                if dimension != D3D10_RESOURCE_DIMENSION_TEXTURE2D {
                    return ready_unmeasured(
                        width,
                        height,
                        levels,
                        "the DX10 resource is not a measured 2D texture",
                    );
                }
                let array_size = claim_u32(bytes, dx10_at + 12).unwrap_or(0);
                if array_size > 1 {
                    return ready_unmeasured(
                        width,
                        height,
                        levels,
                        "a texture array is not measured as one 2D image",
                    );
                }
                match dxgi_info(dxgi) {
                    Some((family, srgb, block_bytes)) => (family, srgb, block_bytes, dx10_at + DDS_DX10_LEN),
                    None => {
                        return ready_unmeasured(
                            width,
                            height,
                            levels,
                            "the DXGI format is not a measured texture",
                        );
                    }
                }
            }
            _ => return ready_unmeasured(width, height, levels, "the DDS FourCC is not a measured texture"),
        }
    } else if pf_flags & DDPF_RGB != 0 {
        (TextureFamily::Uncompressed, None, 0, 4 + DDS_HEADER_LEN)
    } else {
        return ready_unmeasured(width, height, levels, "the DDS pixel format is not measured");
    };
    let payload_short =
        block_bytes > 0 && payload_short(width, height, levels, block_bytes, file_len, header_end);
    Parsed::Ready(Info {
        width,
        height,
        levels,
        family,
        srgb,
        checks_mips: block_bytes > 0,
        payload_short,
        unmeasured: None,
    })
}

fn parse_ktx2(bytes: &[u8], file_len: u64) -> Parsed {
    if bytes.len() < 12 || !has_prefix(bytes, &KTX2_IDENTIFIER) {
        return Parsed::Unknown("the KTX 2.0 identifier is missing");
    }
    if bytes.len() < 80 || file_len < 80 {
        return Parsed::Unknown("the KTX 2.0 header is truncated");
    }
    let vk = claim_u32(bytes, 12).unwrap_or(0);
    let width = claim_u32(bytes, 20).unwrap_or(0);
    let height = claim_u32(bytes, 24).unwrap_or(0);
    let depth = claim_u32(bytes, 28).unwrap_or(0);
    let layers = claim_u32(bytes, 32).unwrap_or(0);
    let faces = claim_u32(bytes, 36).unwrap_or(0);
    let levels = claim_u32(bytes, 40).unwrap_or(0).max(1);
    let supercompression = claim_u32(bytes, 44).unwrap_or(0);
    if let Some(illegal) = illegal_edge(width, height) {
        return illegal;
    }
    let chain = full_chain(width, height);
    if levels > chain {
        return Parsed::Illegal {
            width,
            height,
            message: format!("{levels} mip levels for a {width}×{height} texture (at most {chain})"),
        };
    }
    if depth > 1 {
        return ready_unmeasured(width, height, levels, "a volume texture has no measured 2D size");
    }
    if faces > 1 || layers > 1 {
        return ready_unmeasured(
            width,
            height,
            levels,
            "a cube or array texture is not measured as one 2D image",
        );
    }
    if supercompression != 0 {
        return ready_unmeasured(
            width,
            height,
            levels,
            "a supercompressed texture has no measured block size",
        );
    }
    let family = if vk == 0 {
        TextureFamily::Other
    } else {
        TextureFamily::of(Some(vk))
    };
    let (srgb, block_bytes) = ktx_color(vk);
    let header_end =
        80_usize.saturating_add(usize::try_from(levels.saturating_mul(24)).unwrap_or(usize::MAX));
    let payload_short =
        block_bytes > 0 && payload_short(width, height, levels, block_bytes, file_len, header_end);
    Parsed::Ready(Info {
        width,
        height,
        levels,
        family,
        srgb,
        checks_mips: block_bytes > 0,
        payload_short,
        unmeasured: None,
    })
}

fn ready_unmeasured(width: u32, height: u32, levels: u32, reason: &'static str) -> Parsed {
    Parsed::Ready(Info {
        width,
        height,
        levels,
        family: TextureFamily::Other,
        srgb: None,
        checks_mips: false,
        payload_short: false,
        unmeasured: Some(reason),
    })
}

fn payload_short(
    width: u32,
    height: u32,
    levels: u32,
    block_bytes: u32,
    file_len: u64,
    header_end: usize,
) -> bool {
    let Some(payload) = block_payload(width, height, levels, block_bytes) else {
        return true;
    };
    let header = u64::try_from(header_end).unwrap_or(u64::MAX);
    file_len < header.saturating_add(payload)
}

fn block_payload(width: u32, height: u32, levels: u32, block_bytes: u32) -> Option<u64> {
    let mut total = 0_u64;
    let mut x = width;
    let mut y = height;
    for _ in 0..levels.max(1) {
        let blocks_x = u64::from(x.div_ceil(4));
        let blocks_y = u64::from(y.div_ceil(4));
        let level = blocks_x
            .checked_mul(blocks_y)?
            .checked_mul(u64::from(block_bytes))?;
        total = total.checked_add(level)?;
        x = (x / 2).max(1);
        y = (y / 2).max(1);
    }
    Some(total)
}

fn dxgi_info(dxgi: u32) -> Option<(TextureFamily, Option<bool>, u32)> {
    Some(match dxgi {
        27 | 90 => (TextureFamily::Uncompressed, None, 0),
        28 | 87 => (TextureFamily::Uncompressed, Some(false), 0),
        29 | 91 => (TextureFamily::Uncompressed, Some(true), 0),
        70 => (TextureFamily::Bc, None, 8),
        71 => (TextureFamily::Bc, Some(false), 8),
        72 => (TextureFamily::Bc, Some(true), 8),
        73 => (TextureFamily::Bc, None, 16),
        74 => (TextureFamily::Bc, Some(false), 16),
        75 => (TextureFamily::Bc, Some(true), 16),
        76 => (TextureFamily::Bc, None, 16),
        77 => (TextureFamily::Bc, Some(false), 16),
        78 => (TextureFamily::Bc, Some(true), 16),
        79..=81 => (TextureFamily::Bc, Some(false), 8),
        82..=84 => (TextureFamily::Bc, Some(false), 16),
        94..=96 => (TextureFamily::Bc, Some(false), 16),
        97 => (TextureFamily::Bc, None, 16),
        98 => (TextureFamily::Bc, Some(false), 16),
        99 => (TextureFamily::Bc, Some(true), 16),
        _ => return None,
    })
}

fn ktx_color(vk: u32) -> (Option<bool>, u32) {
    if vk == 0 {
        return (None, 0);
    }
    let family = TextureFamily::of(Some(vk));
    match family {
        TextureFamily::Bc => {
            let srgb = match vk {
                132 | 134 | 136 | 138 | 146 => Some(true),
                131 | 133 | 135 | 137 | 145 => Some(false),
                139..=144 => Some(false),
                _ => None,
            };
            let block = if matches!(vk, 131..=134 | 139 | 140) {
                8
            } else {
                16
            };
            (srgb, block)
        }
        TextureFamily::Etc2 => {
            let srgb = match vk {
                148 | 150 | 152 => Some(true),
                147 | 149 | 151 | 153..=156 => Some(false),
                _ => None,
            };
            (srgb, 0)
        }
        TextureFamily::Astc => {
            let srgb = if (157..=184).contains(&vk) {
                Some(vk.is_multiple_of(2))
            } else {
                Some(false)
            };
            (srgb, 16)
        }
        TextureFamily::Uncompressed | TextureFamily::Other => (None, 0),
    }
}

fn note_texture(
    roles: &mut Vec<(String, Vec<TextureCookUsage>)>,
    images: &[serde_json::Value],
    textures: Option<&Vec<serde_json::Value>>,
    material: &serde_json::Value,
    path: &[&str],
    usage: TextureCookUsage,
) {
    let Some(index) = dig(material, path)
        .and_then(|value| value.get("index"))
        .and_then(|value| value.as_u64())
    else {
        return;
    };
    let Some(textures) = textures else {
        return;
    };
    let Some(source) = textures
        .get(usize::try_from(index).unwrap_or(usize::MAX))
        .and_then(|texture| texture.get("source"))
        .and_then(|value| value.as_u64())
    else {
        return;
    };
    let Some(uri) = images
        .get(usize::try_from(source).unwrap_or(usize::MAX))
        .and_then(|image| image.get("uri"))
        .and_then(|value| value.as_str())
    else {
        return;
    };
    if uri.starts_with("data:") {
        return;
    }
    if let Some((_, found)) = roles.iter_mut().find(|(name, _)| name == uri) {
        push_role(found, usage);
    } else {
        roles.push((uri.to_owned(), vec![usage]));
    }
}

fn dig<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn document_json<'a>(path: &str, bytes: &'a [u8]) -> Option<&'a [u8]> {
    if extension_of(path).is_some_and(|ext| ext.eq_ignore_ascii_case("gltf")) {
        return Some(bytes);
    }
    if !extension_of(path).is_some_and(|ext| ext.eq_ignore_ascii_case("glb")) {
        return None;
    }
    if bytes.len() < 20 || !has_prefix(bytes, b"glTF") || bytes.get(16..20) != Some(b"JSON".as_slice()) {
        return None;
    }
    let len = usize::try_from(u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?)).ok()?;
    bytes.get(20..20 + len)
}

fn image_rel(source_rel: &str, uri: &str) -> Option<String> {
    if uri.starts_with("data:") || uri.starts_with('/') || uri.contains('\\') || uri.contains("..") {
        return None;
    }
    let parent = Path::new(source_rel).parent().unwrap_or(Path::new(""));
    let joined = parent.join(uri);
    let mut parts = Vec::new();
    for component in joined.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}
