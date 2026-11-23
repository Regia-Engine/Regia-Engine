// SPDX-License-Identifier: MPL-2.0
//! `assets.texture_coverage` and `assets.texpack_dry_run` (ADR-3090, `book/agents/texture-coverage.md`).
//!
//! `assets.texture_coverage` decodes a cooked texture's mip chain (the runtime's CPU decoders: BCn,
//! ASTC, ETC2 / EAC) and reports, per level, the fraction of texels at or above an alpha-test
//! cutoff: the number that shows whether a far canopy keeps its leaves. The cutoff is the request's,
//! else the source `.meta`'s `alpha_cutoff`, else 0.5.
//!
//! `assets.texpack_dry_run` checks a `.texpack.ron` channel-pack recipe without cooking it: the
//! parse, the channels, every source (image headers only, nested recipes followed, cycles found)
//! and the output size, or the recipe's `texpack_*` error code. The pipeline is a desktop tool and
//! not a dependency of this crate, so the recipe is read here; a test holds both readers to the
//! same answers on the same files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{AssetServer, TextureCookUsage};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// A bad `cutoff` or `channel` (`assets.texture_coverage`).
pub const COVERAGE_BAD_PARAM: &str = "texture_coverage_bad_param";
/// The asset is not a cooked texture or image.
pub const COVERAGE_NOT_TEXTURE: &str = "texture_coverage_not_texture";
/// The texture's format has no CPU decoder (BC6H, HDR ASTC) or a level does not decode.
pub const COVERAGE_UNDECODABLE: &str = "texture_coverage_undecodable";
/// A bad `path` (`assets.texpack_dry_run`).
pub const DRY_RUN_BAD_PARAM: &str = "texpack_dry_run_bad_param";

/// The levels whose coverage counts toward `max_drop`: one texel is at most 1/64 of them.
const MIN_COUNTED_TEXELS: u64 = 64;
/// The drop under which `preserved` is reported (the cook's 2 % target).
const PRESERVED_DROP: f64 = 0.02;

fn bad(code: &'static str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(protocol::INVALID_PARAMS, code, message.into(), hint)
}

/// The smallest byte a shader's `value / 255 >= cutoff` passes (the cook's `cutoff_byte`).
fn cutoff_byte(cutoff: f64) -> u8 {
    // In f32, as the cook compares (a `.meta` cutoff is an f32).
    let cutoff = cutoff as f32;
    (1..=255u8)
        .find(|&b| f32::from(b) / 255.0 >= cutoff)
        .unwrap_or(255)
}

/// Largest `.meta` sidecar [`meta_cutoff`] reads: sidecars are a few hundred bytes, and the call is
/// read-scoped, so a huge planted sidecar must not be read whole on the world thread.
const MAX_META_BYTES: u64 = 1 << 20;

/// The `alpha_cutoff` of the source's `.meta` settings, if it sets one. A sidecar over
/// [`MAX_META_BYTES`] counts as setting none.
fn meta_cutoff(root: &Path, rel_path: &str) -> Option<f64> {
    #[derive(Deserialize)]
    struct Settings {
        #[serde(default)]
        alpha_cutoff: Option<f32>,
    }
    let source = rel_path.split('#').next()?;
    let source = regia_core::project_path::resolve_write(root, source).ok()?;
    let bytes =
        crate::req_path::read_file_capped(&regia_asset::meta_path_for(&source), MAX_META_BYTES).ok()?;
    let meta = regia_asset::MetaSidecar::from_ron(&bytes).ok()?;
    let settings: Settings = ron::from_str(meta.settings.as_deref()?).ok()?;
    settings
        .alpha_cutoff
        .map(f64::from)
        .filter(|c| c.is_finite() && *c > 0.0 && *c <= 1.0)
}

/// `assets.texture_coverage { asset, cutoff?, channel? }`.
///
/// # Errors
/// `F0016` (no server, unknown asset, not cooked), `texture_coverage_bad_param`,
/// `texture_coverage_not_texture`, `texture_coverage_undecodable`.
pub fn texture_coverage(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let cutoff = match params.get("cutoff") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_f64() {
            Some(c) if c.is_finite() && c > 0.0 && c <= 1.0 => Some(c),
            _ => {
                return Err(bad(
                    COVERAGE_BAD_PARAM,
                    "`cutoff` must be a number in (0, 1]",
                    "pass the material's alpha cutoff, e.g. 0.5, or leave it out",
                ));
            }
        },
    };
    let channel = match crate::params::opt_str(params, "channel")? {
        None => None,
        Some("a") => Some(3usize),
        Some("r") => Some(0),
        Some(other) => {
            return Err(bad(
                COVERAGE_BAD_PARAM,
                format!("`channel` `{other}` is not `a` or `r`"),
                "leave it out: alpha, or red for a mask texture",
            ));
        }
    };
    let root = crate::req_path::project_root(world);
    let Some(mut server) = world.get_resource_mut::<AssetServer>() else {
        return Err(crate::assets::no_asset_server());
    };
    let (path, id) = crate::assets::resolve_asset(&server, params)?;
    let (bytes, _) = server.read_cooked(id).map_err(|e| {
        RpcError::new(
            protocol::NOT_FOUND,
            "F0016",
            format!("{path}: no cooked file: {e}"),
            "run regia cook",
        )
    })?;
    let not_texture = |why: String| {
        RpcError::new(
            protocol::INVALID_PARAMS,
            COVERAGE_NOT_TEXTURE,
            format!("{path} is not a cooked texture: {why}"),
            "pass an image/.texpack.ron path or current texture id; shared external glTF images use the image/variant path; private images, including embedded images, use <gltf>#texture/<n>",
        )
    };
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).map_err(|e| not_texture(e.to_string()))?;
    let (usage, format, levels) = if let Ok(texture) = regia_asset::CookedTexture::from_postcard(payload) {
        let blocks = regia_render::CompressedTexture::from_ktx2(&texture.ktx2).map_err(|e| {
            RpcError::new(
                protocol::INVALID_PARAMS,
                COVERAGE_UNDECODABLE,
                format!("{path}: {e}"),
                "only block-compressed LDR textures decode on the CPU",
            )
        })?;
        let levels: Vec<(u32, u32, Vec<u8>)> = (0..)
            .map_while(|level| blocks.decode_level_rgba8(level))
            .map(|t| (t.width, t.height, t.rgba))
            .collect();
        if levels.is_empty() {
            return Err(RpcError::new(
                protocol::INVALID_PARAMS,
                COVERAGE_UNDECODABLE,
                format!("{path}: format {:?} has no CPU decoder", blocks.format),
                "BC6H and HDR ASTC carry no alpha test",
            ));
        }
        (texture.usage, format!("{:?}", blocks.format), levels)
    } else if let Ok(image) = regia_asset::CookedImage::from_postcard(payload) {
        (
            TextureCookUsage::ColorSrgb,
            "Rgba8".to_string(),
            vec![(image.width, image.height, image.rgba8)],
        )
    } else {
        return Err(not_texture(
            "the payload is neither a CookedTexture nor a CookedImage".into(),
        ));
    };
    if usage == TextureCookUsage::NormalMap && channel.is_none() {
        return Err(not_texture("a normal map carries no coverage".into()));
    }
    let channel = channel.unwrap_or(if usage == TextureCookUsage::Mask { 0 } else { 3 });
    let (cutoff, cutoff_source) = match cutoff {
        Some(c) => (c, "param"),
        None => match meta_cutoff(&root, &path) {
            Some(c) => (c, "meta"),
            None => (0.5, "default"),
        },
    };
    let t = cutoff_byte(cutoff);
    let mut rows = Vec::with_capacity(levels.len());
    let mut c0 = 0.0f64;
    let mut max_drop = 0.0f64;
    for (i, (w, h, px)) in levels.iter().enumerate() {
        let texels = px.as_chunks::<4>().0;
        let n = texels.len().max(1) as f64;
        let pass = texels.iter().filter(|p| p[channel] >= t).count() as f64;
        let mean = texels.iter().map(|p| f64::from(p[channel])).sum::<f64>() / n / 255.0;
        let coverage = pass / n;
        if i == 0 {
            c0 = coverage;
        } else if u64::from(*w) * u64::from(*h) >= MIN_COUNTED_TEXELS {
            max_drop = max_drop.max((coverage - c0).abs());
        }
        rows.push(json!({
            "level": i,
            "width": w,
            "height": h,
            "coverage": coverage,
            "mean": mean,
        }));
    }
    Ok(json!({
        "asset": id.to_string(),
        "path": path,
        "usage": format!("{usage:?}"),
        "format": format,
        "channel": if channel == 0 { "r" } else { "a" },
        "cutoff": cutoff,
        "cutoff_source": cutoff_source,
        "mip0_coverage": c0,
        "max_drop": max_drop,
        "preserved": max_drop <= PRESERVED_DROP,
        "levels": rows,
    }))
}

// ---------------------------------------------------------------------------------------------
// `.texpack.ron` (the pipeline's `texture_pack` reader, kept in step by a test)
// ---------------------------------------------------------------------------------------------

const TEXPACK_FORMAT_VERSION: u32 = 1;
const TEXPACK_SUFFIX: &str = ".texpack.ron";
const MAX_SOURCES: usize = 4;
const MAX_EDGE: u32 = 16384;
const MAX_DEPTH: usize = 8;
const CHANNEL_NAMES: [&str; 5] = ["r", "g", "b", "a", "luma"];

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
enum PackChannel {
    Source {
        source: String,
        channel: String,
        #[serde(default)]
        invert: bool,
    },
    Constant(f32),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SizeMismatch {
    #[default]
    ResizeToLargest,
    ResizeToSmallest,
    Error,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Usage {
    Color,
    Linear,
    Normal,
    Mask,
    Ui,
}

fn black() -> PackChannel {
    PackChannel::Constant(0.0)
}

fn white() -> PackChannel {
    PackChannel::Constant(1.0)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    format_version: u32,
    #[serde(default)]
    sources: BTreeMap<String, String>,
    #[serde(default = "black")]
    r: PackChannel,
    #[serde(default = "black")]
    g: PackChannel,
    #[serde(default = "black")]
    b: PackChannel,
    #[serde(default = "white")]
    a: PackChannel,
    #[serde(default)]
    size_mismatch: SizeMismatch,
    #[serde(default)]
    size: Option<(u32, u32)>,
    #[serde(default)]
    usage: Option<Usage>,
}

/// A recipe problem: its `texpack_*` code and message.
#[derive(Debug)]
struct PackError {
    code: &'static str,
    message: String,
}

fn err(code: &'static str, message: impl Into<String>) -> PackError {
    PackError {
        code,
        message: message.into(),
    }
}

/// The version a recipe's text declares (a text scan, as the pipeline's `texpack_file_version`).
fn file_version(text: &str) -> Option<u32> {
    let mut rest = text;
    while let Some(at) = rest.find("format_version") {
        let before = &rest[..at];
        let after = &rest[at + "format_version".len()..];
        rest = after;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        if before[line_start..].contains("//") {
            continue;
        }
        let Some(after) = after.trim_start().strip_prefix(':') else {
            continue;
        };
        let digits: String = after
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        return digits.parse().ok();
    }
    None
}

fn parse_recipe(text: &str) -> Result<Recipe, PackError> {
    if let Some(v) = file_version(text)
        && v != TEXPACK_FORMAT_VERSION
    {
        return Err(err(
            "texpack_version",
            format!("format_version {v}; this build reads {TEXPACK_FORMAT_VERSION}"),
        ));
    }
    let recipe: Recipe = ron::from_str(text).map_err(|e| err("texpack_parse", e.to_string()))?;
    if recipe.format_version != TEXPACK_FORMAT_VERSION {
        return Err(err(
            "texpack_version",
            format!("format_version {}", recipe.format_version),
        ));
    }
    if recipe.sources.len() > MAX_SOURCES {
        return Err(err(
            "texpack_bad_sources",
            format!("{} sources; at most {MAX_SOURCES}", recipe.sources.len()),
        ));
    }
    if recipe
        .sources
        .iter()
        .any(|(n, p)| n.trim().is_empty() || p.trim().is_empty())
    {
        return Err(err("texpack_bad_sources", "a source name or path is empty"));
    }
    for (label, channel) in ["r", "g", "b", "a"]
        .iter()
        .zip([&recipe.r, &recipe.g, &recipe.b, &recipe.a])
    {
        match channel {
            PackChannel::Source { source, channel, .. } => {
                if !recipe.sources.contains_key(source) {
                    return Err(err(
                        "texpack_unknown_source",
                        format!("channel {label} reads unknown source `{source}`"),
                    ));
                }
                if !CHANNEL_NAMES.contains(&channel.as_str()) {
                    return Err(err(
                        "texpack_bad_channel",
                        format!("channel {label} reads `{channel}`; use one of {CHANNEL_NAMES:?}"),
                    ));
                }
            }
            PackChannel::Constant(v) => {
                if !v.is_finite() || !(0.0..=1.0).contains(v) {
                    return Err(err(
                        "texpack_bad_constant",
                        format!("channel {label} constant {v} is not in 0..=1"),
                    ));
                }
            }
        }
    }
    if let Some((w, h)) = recipe.size
        && (w == 0 || h == 0 || w > MAX_EDGE || h > MAX_EDGE)
    {
        return Err(err(
            "texpack_bad_size",
            format!("size ({w}, {h}): each edge must be 1 to {MAX_EDGE}"),
        ));
    }
    Ok(recipe)
}

fn is_texpack(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(TEXPACK_SUFFIX))
}

/// `rel` (a recipe's source path, `\` or `/` separated) joined onto the recipe's folder, confined
/// to the project, with `.` and `a/..` folded.
fn resolve_source(root: &Path, dir: &Path, rel: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let rel = rel.replace('\\', "/");
    let drive = rel.len() >= 2 && rel.as_bytes()[0].is_ascii_alphabetic() && rel.as_bytes()[1] == b':';
    if rel.starts_with('/') || drive {
        return Err(format!("`{rel}` is absolute; use a path relative to the recipe"));
    }
    let mut out = PathBuf::new();
    for part in dir.join(&rel).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir if matches!(out.components().next_back(), Some(Component::Normal(_))) => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if !out.starts_with(root) {
        return Err(format!("`{rel}` leaves the project"));
    }
    Ok(out)
}

/// The size in an image file's header: PNG, JPEG, BMP or TGA.
fn header_size(path: &Path) -> Result<(u32, u32), PackError> {
    let bytes = crate::req_path::read_file_capped(path, 64 << 20)
        .map_err(|e| err("texpack_missing_source", format!("{}: {e}", path.display())))?;
    let undecodable = || {
        err(
            "texpack_decode",
            format!("{}: not a PNG, JPEG, BMP or TGA header", path.display()),
        )
    };
    let be32 = |b: &[u8], i: usize| {
        b.get(i..i + 4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    let be16 = |b: &[u8], i: usize| {
        b.get(i..i + 2)
            .map(|s| u32::from(u16::from_be_bytes([s[0], s[1]])))
    };
    let le16 = |b: &[u8], i: usize| {
        b.get(i..i + 2)
            .map(|s| u32::from(u16::from_le_bytes([s[0], s[1]])))
    };
    let le32 = |b: &[u8], i: usize| {
        b.get(i..i + 4)
            .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return match (be32(&bytes, 16), be32(&bytes, 20)) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Ok((w, h)),
            _ => Err(undecodable()),
        };
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        // Walk the markers to a start-of-frame.
        let mut i = 2;
        while i + 4 <= bytes.len() {
            if bytes[i] != 0xFF {
                return Err(undecodable());
            }
            let marker = bytes[i + 1];
            let len = be16(&bytes, i + 2).ok_or_else(undecodable)? as usize;
            let sof = (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker);
            if sof {
                return match (be16(&bytes, i + 7), be16(&bytes, i + 5)) {
                    (Some(w), Some(h)) if w > 0 && h > 0 => Ok((w, h)),
                    _ => Err(undecodable()),
                };
            }
            i += 2 + len;
        }
        return Err(undecodable());
    }
    if bytes.starts_with(b"BM") {
        return match (le32(&bytes, 18), le32(&bytes, 22)) {
            (Some(w), Some(h)) if w > 0 && h != 0 => Ok((w.unsigned_abs(), h.unsigned_abs())),
            _ => Err(undecodable()),
        };
    }
    let tga = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tga"));
    if tga {
        return match (le16(&bytes, 12), le16(&bytes, 14)) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Ok((w, h)),
            _ => Err(undecodable()),
        };
    }
    Err(undecodable())
}

/// What the dry run found for one recipe.
struct Plan {
    recipe: Recipe,
    width: u32,
    height: u32,
    sources: Vec<Value>,
    inputs: Vec<PathBuf>,
}

fn identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn plan(root: &Path, path: &Path, stack: &mut Vec<PathBuf>) -> Result<Plan, PackError> {
    let id = identity(path);
    if stack.contains(&id) {
        return Err(err(
            "texpack_cycle",
            format!("{} reaches itself through its sources", path.display()),
        ));
    }
    if stack.len() >= MAX_DEPTH {
        return Err(err(
            "texpack_cycle",
            format!("recipes nest deeper than {MAX_DEPTH}"),
        ));
    }
    stack.push(id);
    let bytes = crate::req_path::read_file_capped(path, 1 << 20)
        .map_err(|e| err("texpack_missing_source", format!("{}: {e}", path.display())))?;
    let text = std::str::from_utf8(&bytes).map_err(|e| err("texpack_parse", format!("not UTF-8: {e}")))?;
    let recipe = parse_recipe(text)?;
    let dir = path.parent().unwrap_or(root);
    let mut sizes = Vec::new();
    let mut inputs = Vec::new();
    let mut found = Vec::new();
    for (name, rel) in &recipe.sources {
        let file = resolve_source(root, dir, rel)
            .map_err(|e| err("texpack_bad_path", format!("source `{name}`: {e}")))?;
        inputs.push(file.clone());
        if !file.is_file() {
            return Err(err(
                "texpack_missing_source",
                format!("source `{name}`: {}: no such file", file.display()),
            ));
        }
        let nested = is_texpack(&file);
        let (w, h) = if nested {
            let inner = plan(root, &file, stack)?;
            inputs.extend(inner.inputs);
            (inner.width, inner.height)
        } else {
            header_size(&file).map_err(|e| err(e.code, format!("source `{name}`: {}", e.message)))?
        };
        sizes.push((name.clone(), w, h));
        found.push((name.clone(), file, w, h, nested));
    }
    stack.pop();
    let (width, height) = match (recipe.size, sizes.first()) {
        (Some(size), _) => size,
        (None, None) => return Err(err("texpack_bad_size", "no source gives a size; set `size`")),
        (None, Some((first, w0, h0))) => match recipe.size_mismatch {
            SizeMismatch::ResizeToLargest => (
                sizes.iter().map(|s| s.1).max().unwrap_or(*w0),
                sizes.iter().map(|s| s.2).max().unwrap_or(*h0),
            ),
            SizeMismatch::ResizeToSmallest => (
                sizes.iter().map(|s| s.1).min().unwrap_or(*w0),
                sizes.iter().map(|s| s.2).min().unwrap_or(*h0),
            ),
            SizeMismatch::Error => {
                if let Some((name, w, h)) = sizes.iter().find(|s| (s.1, s.2) != (*w0, *h0)) {
                    return Err(err(
                        "texpack_size_mismatch",
                        format!("source `{name}` is {w}x{h} but `{first}` is {w0}x{h0}"),
                    ));
                }
                (*w0, *h0)
            }
        },
    };
    if width > MAX_EDGE || height > MAX_EDGE {
        return Err(err(
            "texpack_bad_size",
            format!("output {width}x{height} is above {MAX_EDGE}"),
        ));
    }
    let rel = |p: &Path| {
        p.strip_prefix(root).map_or_else(
            |_| p.display().to_string(),
            |r| r.to_string_lossy().replace('\\', "/"),
        )
    };
    let sources = found
        .iter()
        .map(|(name, file, w, h, nested)| {
            json!({
                "name": name,
                "path": rel(file),
                "width": w,
                "height": h,
                "recipe": nested,
                "resized": (*w, *h) != (width, height),
            })
        })
        .collect();
    Ok(Plan {
        recipe,
        width,
        height,
        sources,
        inputs,
    })
}

fn describe(channel: &PackChannel) -> String {
    match channel {
        PackChannel::Constant(v) => format!("{v}"),
        PackChannel::Source {
            source,
            channel,
            invert: false,
        } => format!("{source}.{channel}"),
        PackChannel::Source {
            source,
            channel,
            invert: true,
        } => format!("1 - {source}.{channel}"),
    }
}

/// `assets.texpack_dry_run { path }`: the plan of a `.texpack.ron`, or its error code.
///
/// # Errors
/// `texpack_dry_run_bad_param` for a missing or non-recipe `path`; `path_outside_project`.
pub fn texpack_dry_run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(rel) = crate::params::opt_str(params, "path")?.filter(|s| !s.is_empty()) else {
        return Err(bad(
            DRY_RUN_BAD_PARAM,
            "`path` (a project-relative .texpack.ron) is required",
            "pass e.g. assets/rocks/rock_orm.texpack.ron",
        ));
    };
    let root = crate::req_path::project_root(world);
    let path = crate::req_path::project_path_in(&root, "path", rel)?;
    if !is_texpack(&path) {
        return Err(bad(
            DRY_RUN_BAD_PARAM,
            format!("`{rel}` is not a .texpack.ron"),
            "channel-pack recipes end in .texpack.ron",
        ));
    }
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let _span = tracing::info_span!("texpack_dry_run").entered();
    Ok(match plan(&root, &path, &mut Vec::new()) {
        Ok(plan) => {
            let usage = match plan.recipe.usage {
                None | Some(Usage::Linear) => "linear",
                Some(Usage::Color) => "color",
                Some(Usage::Normal) => "normal",
                Some(Usage::Mask) => "mask",
                Some(Usage::Ui) => "ui",
            };
            let inputs: Vec<String> = plan
                .inputs
                .iter()
                .map(|p| {
                    p.strip_prefix(&root).map_or_else(
                        |_| p.display().to_string(),
                        |r| r.to_string_lossy().replace('\\', "/"),
                    )
                })
                .collect();
            json!({
                "path": rel,
                "ok": true,
                "width": plan.width,
                "height": plan.height,
                "usage": usage,
                "channels": {
                    "r": describe(&plan.recipe.r),
                    "g": describe(&plan.recipe.g),
                    "b": describe(&plan.recipe.b),
                    "a": describe(&plan.recipe.a),
                },
                "sources": plan.sources,
                "inputs": inputs,
            })
        }
        Err(e) => json!({
            "path": rel,
            "ok": false,
            "code": e.code,
            "message": e.message,
        }),
    })
}

#[cfg(test)]
mod tests;
