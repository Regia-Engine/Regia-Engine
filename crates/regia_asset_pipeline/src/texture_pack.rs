// SPDX-License-Identifier: MPL-2.0
//! Channel-pack recipes, `.texpack.ron` (`docs/formats/texpack-ron.md`, ADR-3090).
//!
//! A recipe builds one texture from up to four source images: each output channel (R, G, B, A)
//! is a named source's channel (`r`, `g`, `b`, `a` or `luma`), optionally inverted, or a
//! constant. The typical use is a packed ORM (occlusion, roughness, metalness) from three
//! grayscale maps, or a roughness map from a gloss map (`invert: true`).
//!
//! ```ron
//! (
//!     format_version: 1,
//!     sources: { "ao": "rock_ao.png", "rough": "rock_rough.png", "metal": "rock_metal.png" },
//!     r: Source(source: "ao", channel: "r"),
//!     g: Source(source: "rough", channel: "r"),
//!     b: Source(source: "metal", channel: "r"),
//!     a: Constant(1.0),
//!     size_mismatch: resize_to_largest,
//!     usage: Some(linear),
//! )
//! ```
//!
//! Sources are paths relative to the recipe (PNG, JPEG, TGA, BMP, or another `.texpack.ron`,
//! whose packed result is the input); sources of another size are resized to the common size by
//! the `size_mismatch` policy (or to `size` when set), or the recipe fails with
//! `texpack_size_mismatch`. The packed image then cooks exactly like a PNG with the recipe's own
//! `.meta` settings (`TextureImporterSettings`: compression, `max_size`, `alpha_cutoff`, ...),
//! so its cooked output is a `CookedTexture`. Every source file is an input of the cook key: an
//! edit to one recooks the recipe, and the live cooker recooks it when one is saved.
//!
//! Every failure carries a stable code ([`TexPackError::code`]), listed in the format page.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_asset::TextureCookUsage;
use serde::Deserialize;

use crate::texture_settings::TextureUsageSetting;

/// The current `.texpack.ron` format version.
pub const TEXPACK_FORMAT_VERSION: u32 = 1;
/// The file name suffix of a recipe.
pub const TEXPACK_SUFFIX: &str = ".texpack.ron";
/// The importer name a recipe's `.meta` and cook rows carry.
pub const TEXPACK_IMPORTER: &str = "texpack_ron";
/// The most sources one recipe may name.
pub const MAX_SOURCES: usize = 4;
/// The largest output edge, texels.
pub const MAX_EDGE: u32 = 16384;
/// How deep recipes may nest (a recipe whose source is a recipe ...).
pub const MAX_DEPTH: usize = 8;

/// Error codes (`docs/formats/texpack-ron.md`).
pub mod codes {
    /// The file is not UTF-8 or not a recipe (RON syntax, unknown field, wrong type).
    pub const PARSE: &str = "texpack_parse";
    /// `format_version` is not one this build reads.
    pub const VERSION: &str = "texpack_version";
    /// More than four sources, or a source name that is empty.
    pub const BAD_SOURCES: &str = "texpack_bad_sources";
    /// A channel names a source the recipe does not list.
    pub const UNKNOWN_SOURCE: &str = "texpack_unknown_source";
    /// A channel reads a channel name other than `r`, `g`, `b`, `a`, `luma`.
    pub const BAD_CHANNEL: &str = "texpack_bad_channel";
    /// A constant is not a finite number in 0..=1.
    pub const BAD_CONSTANT: &str = "texpack_bad_constant";
    /// A source path is absolute or leaves the project.
    pub const BAD_PATH: &str = "texpack_bad_path";
    /// A source file does not exist or cannot be read.
    pub const MISSING_SOURCE: &str = "texpack_missing_source";
    /// A source file is not an image the cook decodes.
    pub const DECODE: &str = "texpack_decode";
    /// The sources differ in size under `size_mismatch: error`.
    pub const SIZE_MISMATCH: &str = "texpack_size_mismatch";
    /// `size` has a zero edge or an edge above 16384, or no source and no `size` gives a size.
    pub const BAD_SIZE: &str = "texpack_bad_size";
    /// A recipe reaches itself through its sources, or nests deeper than eight recipes.
    pub const CYCLE: &str = "texpack_cycle";
}

/// A recipe that does not parse, plan or pack: a stable code and a message naming the problem.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct TexPackError {
    /// One of [`codes`].
    pub code: &'static str,
    /// What is wrong, with the file and field.
    pub message: String,
}

impl TexPackError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The stable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }
}

/// What one output channel holds.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum PackChannel {
    /// A source's channel.
    Source {
        /// The name of a `sources` entry.
        source: String,
        /// `r`, `g`, `b`, `a` or `luma` (Rec. 709 weights over the stored R, G, B).
        channel: String,
        /// Store `1 - value`.
        #[serde(default)]
        invert: bool,
    },
    /// The same value in every texel, 0 to 1.
    Constant(f32),
}

/// How sources of different sizes come to one size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeMismatch {
    /// Every source is resized to the largest width and the largest height among them (the
    /// default).
    #[default]
    ResizeToLargest,
    /// Every source is resized to the smallest width and the smallest height among them.
    ResizeToSmallest,
    /// Sources of different sizes fail the recipe (`texpack_size_mismatch`).
    Error,
}

fn black() -> PackChannel {
    PackChannel::Constant(0.0)
}

fn white() -> PackChannel {
    PackChannel::Constant(1.0)
}

/// A `.texpack.ron` recipe (format version 1).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TexturePackRecipe {
    /// Must be [`TEXPACK_FORMAT_VERSION`].
    pub format_version: u32,
    /// Up to four sources by name: paths relative to the recipe.
    #[serde(default)]
    pub sources: BTreeMap<String, String>,
    /// The red channel (default 0).
    #[serde(default = "black")]
    pub r: PackChannel,
    /// The green channel (default 0).
    #[serde(default = "black")]
    pub g: PackChannel,
    /// The blue channel (default 0).
    #[serde(default = "black")]
    pub b: PackChannel,
    /// The alpha channel (default 1).
    #[serde(default = "white")]
    pub a: PackChannel,
    /// How sources of different sizes are handled (default [`SizeMismatch::ResizeToLargest`]).
    #[serde(default)]
    pub size_mismatch: SizeMismatch,
    /// The output size `(width, height)`; every source is resized to it. Unset: the
    /// `size_mismatch` policy decides (required when every channel is a constant).
    #[serde(default)]
    pub size: Option<(u32, u32)>,
    /// How the packed texture is sampled (default `linear`); the recipe's `.meta` `usage`
    /// overrides it.
    #[serde(default)]
    pub usage: Option<TextureUsageSetting>,
}

/// The source channels a [`PackChannel::Source`] may read.
pub const CHANNEL_NAMES: [&str; 5] = ["r", "g", "b", "a", "luma"];

impl TexturePackRecipe {
    /// Parses and checks a recipe (version, sources, channels, constants, size).
    ///
    /// # Errors
    /// `texpack_parse`, `texpack_version`, `texpack_bad_sources`, `texpack_unknown_source`,
    /// `texpack_bad_channel`, `texpack_bad_constant` or `texpack_bad_size`.
    pub fn from_ron(text: &str) -> Result<Self, TexPackError> {
        // The version first, so a newer file says so instead of failing on a field it added.
        if let Some(v) = texpack_file_version(text)
            && v != TEXPACK_FORMAT_VERSION
        {
            return Err(TexPackError::new(
                codes::VERSION,
                format!("format_version {v}; this build reads {TEXPACK_FORMAT_VERSION}"),
            ));
        }
        let recipe: Self = ron::from_str(text).map_err(|e| TexPackError::new(codes::PARSE, e.to_string()))?;
        if recipe.format_version != TEXPACK_FORMAT_VERSION {
            return Err(TexPackError::new(
                codes::VERSION,
                format!(
                    "format_version {}; this build reads {TEXPACK_FORMAT_VERSION}",
                    recipe.format_version
                ),
            ));
        }
        recipe.validate()?;
        Ok(recipe)
    }

    /// The four channels in R, G, B, A order.
    #[must_use]
    pub fn channels(&self) -> [&PackChannel; 4] {
        [&self.r, &self.g, &self.b, &self.a]
    }

    /// The usage the packed texture cooks as when its `.meta` does not override it.
    #[must_use]
    pub fn usage(&self) -> TextureCookUsage {
        self.usage
            .map_or(TextureCookUsage::ColorLinear, TextureCookUsage::from)
    }

    /// The structural checks [`Self::from_ron`] runs (no file is read).
    ///
    /// # Errors
    /// As [`Self::from_ron`], without the parse and version codes.
    pub fn validate(&self) -> Result<(), TexPackError> {
        if self.sources.len() > MAX_SOURCES {
            return Err(TexPackError::new(
                codes::BAD_SOURCES,
                format!(
                    "{} sources; a recipe takes at most {MAX_SOURCES}",
                    self.sources.len()
                ),
            ));
        }
        if let Some((name, _)) = self
            .sources
            .iter()
            .find(|(name, path)| name.trim().is_empty() || path.trim().is_empty())
        {
            return Err(TexPackError::new(
                codes::BAD_SOURCES,
                format!("source `{name}`: the name and the path must not be empty"),
            ));
        }
        for (label, channel) in ["r", "g", "b", "a"].iter().zip(self.channels()) {
            match channel {
                PackChannel::Source { source, channel, .. } => {
                    if !self.sources.contains_key(source) {
                        let known: Vec<&str> = self.sources.keys().map(String::as_str).collect();
                        return Err(TexPackError::new(
                            codes::UNKNOWN_SOURCE,
                            format!("channel {label} reads source `{source}`; the recipe lists {known:?}"),
                        ));
                    }
                    if !CHANNEL_NAMES.contains(&channel.as_str()) {
                        return Err(TexPackError::new(
                            codes::BAD_CHANNEL,
                            format!(
                                "channel {label} reads `{source}`.`{channel}`; use one of {CHANNEL_NAMES:?}"
                            ),
                        ));
                    }
                }
                PackChannel::Constant(value) => {
                    if !value.is_finite() || !(0.0..=1.0).contains(value) {
                        return Err(TexPackError::new(
                            codes::BAD_CONSTANT,
                            format!("channel {label} constant {value} is not in 0..=1"),
                        ));
                    }
                }
            }
        }
        if let Some((w, h)) = self.size
            && (w == 0 || h == 0 || w > MAX_EDGE || h > MAX_EDGE)
        {
            return Err(TexPackError::new(
                codes::BAD_SIZE,
                format!("size ({w}, {h}): each edge must be 1 to {MAX_EDGE}"),
            ));
        }
        Ok(())
    }
}

/// Whether `path`'s file name ends with `.texpack.ron` (ignoring ASCII case).
#[must_use]
pub fn is_texpack(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| crate::cook::name_ends_with(n, TEXPACK_SUFFIX))
}

/// One source of a planned recipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedSource {
    /// The recipe's name for it.
    pub name: String,
    /// The resolved file.
    pub path: PathBuf,
    /// Its size, texels (a nested recipe's packed size).
    pub width: u32,
    /// Its height, texels.
    pub height: u32,
    /// The source is another `.texpack.ron`.
    pub recipe: bool,
    /// It is resized to the output size.
    pub resized: bool,
}

/// What packing a recipe will do, without decoding any texel ([`plan_pack`]).
#[derive(Clone, Debug, PartialEq)]
pub struct PackPlan {
    /// The parsed recipe.
    pub recipe: TexturePackRecipe,
    /// Its sources, by name.
    pub sources: Vec<PlannedSource>,
    /// The output width, texels.
    pub width: u32,
    /// The output height, texels.
    pub height: u32,
    /// Every file the pack reads (the sources and, through nested recipes, theirs), in a stable
    /// order: the cook key's side inputs.
    pub inputs: Vec<PathBuf>,
}

/// An RGBA8 image a recipe packed ([`pack_rgba8`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedImage {
    /// Width, texels.
    pub width: u32,
    /// Height, texels.
    pub height: u32,
    /// Row-major RGBA8.
    pub rgba8: Vec<u8>,
    /// Every file the pack read ([`PackPlan::inputs`]).
    pub inputs: Vec<PathBuf>,
}

/// A stable identity for `path` for the cycle check: the canonical path when the file exists.
fn identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// The recipe at `path`, read and parsed.
fn read_recipe(path: &Path) -> Result<TexturePackRecipe, TexPackError> {
    let bytes = std::fs::read(path)
        .map_err(|e| TexPackError::new(codes::MISSING_SOURCE, format!("{}: {e}", path.display())))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| TexPackError::new(codes::PARSE, format!("{}: not UTF-8: {e}", path.display())))?;
    TexturePackRecipe::from_ron(text)
        .map_err(|e| TexPackError::new(e.code, format!("{}: {}", path.display(), e.message)))
}

/// The resolved file of every source of `recipe` (at `path`), by name.
fn resolve_sources(path: &Path, recipe: &TexturePackRecipe) -> Result<Vec<(String, PathBuf)>, TexPackError> {
    let dir = path.parent().unwrap_or_else(|| Path::new(""));
    recipe
        .sources
        .iter()
        .map(|(name, rel)| {
            crate::cook::join_portable(dir, rel)
                .map(|p| (name.clone(), lexical(&p)))
                .map_err(|e| {
                    TexPackError::new(
                        codes::BAD_PATH,
                        format!("{}: source `{name}`: {e}", path.display()),
                    )
                })
        })
        .collect()
}

/// `path` with `.` and `a/..` folded (`join_portable` already kept it inside the project), so a
/// source names the same file the watcher reports.
fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir if matches!(out.components().next_back(), Some(Component::Normal(_))) => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The output size of `recipe` from its sources' sizes.
fn output_size(
    path: &Path,
    recipe: &TexturePackRecipe,
    sizes: &[(String, u32, u32)],
) -> Result<(u32, u32), TexPackError> {
    if let Some(size) = recipe.size {
        return Ok(size);
    }
    let Some((_, w0, h0)) = sizes.first() else {
        return Err(TexPackError::new(
            codes::BAD_SIZE,
            format!(
                "{}: no source gives a size; set `size: Some((w, h))`",
                path.display()
            ),
        ));
    };
    let (w, h) = match recipe.size_mismatch {
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
                return Err(TexPackError::new(
                    codes::SIZE_MISMATCH,
                    format!(
                        "{}: source `{name}` is {w}x{h} but `{}` is {w0}x{h0} (size_mismatch: error; use resize_to_largest, resize_to_smallest or size)",
                        path.display(),
                        sizes[0].0
                    ),
                ));
            }
            (*w0, *h0)
        }
    };
    if w > MAX_EDGE || h > MAX_EDGE {
        return Err(TexPackError::new(
            codes::BAD_SIZE,
            format!("{}: output {w}x{h} is above {MAX_EDGE}", path.display()),
        ));
    }
    Ok((w, h))
}

/// The size of the image file at `path` (header only).
fn image_size(path: &Path) -> Result<(u32, u32), TexPackError> {
    if !path.is_file() {
        return Err(TexPackError::new(
            codes::MISSING_SOURCE,
            format!("{}: no such file", path.display()),
        ));
    }
    let reader = image::ImageReader::open(path)
        .map_err(|e| TexPackError::new(codes::MISSING_SOURCE, format!("{}: {e}", path.display())))?;
    let reader = with_format(reader, path)
        .map_err(|e| TexPackError::new(codes::DECODE, format!("{}: {e}", path.display())))?;
    reader
        .into_dimensions()
        .map_err(|e| TexPackError::new(codes::DECODE, format!("{}: {e}", path.display())))
}

/// `reader` with its format from the content, or from the extension for TGA (which has no magic
/// number).
fn with_format<R: std::io::BufRead + std::io::Seek>(
    reader: image::ImageReader<R>,
    path: &Path,
) -> std::io::Result<image::ImageReader<R>> {
    let tga = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tga"));
    if tga {
        let mut reader = reader;
        reader.set_format(image::ImageFormat::Tga);
        Ok(reader)
    } else {
        reader.with_guessed_format()
    }
}

/// Plans the recipe at `path`: parses it and every nested recipe, resolves and sizes the sources
/// (image headers only) and computes the output size. The dry run behind the agent's
/// `assets.texpack_dry_run` and the cook's dependency list.
///
/// # Errors
/// Any [`codes`] value except `texpack_decode` of texel data (a header that does not read is
/// `texpack_decode`).
pub fn plan_pack(path: &Path) -> Result<PackPlan, TexPackError> {
    let _span = tracing::info_span!("texpack_plan").entered();
    plan_inner(path, &mut Vec::new())
}

fn enter(path: &Path, stack: &mut Vec<PathBuf>) -> Result<(), TexPackError> {
    let id = identity(path);
    if let Some(at) = stack.iter().position(|p| *p == id) {
        let chain: Vec<String> = stack[at..]
            .iter()
            .chain(std::iter::once(&id))
            .map(|p| p.display().to_string())
            .collect();
        return Err(TexPackError::new(
            codes::CYCLE,
            format!("recipes reach themselves: {}", chain.join(" -> ")),
        ));
    }
    if stack.len() >= MAX_DEPTH {
        return Err(TexPackError::new(
            codes::CYCLE,
            format!("{}: recipes nest deeper than {MAX_DEPTH}", path.display()),
        ));
    }
    stack.push(id);
    Ok(())
}

fn plan_inner(path: &Path, stack: &mut Vec<PathBuf>) -> Result<PackPlan, TexPackError> {
    enter(path, stack)?;
    let recipe = read_recipe(path)?;
    let resolved = resolve_sources(path, &recipe)?;
    let mut sizes = Vec::with_capacity(resolved.len());
    let mut inputs = Vec::new();
    let mut nested = Vec::with_capacity(resolved.len());
    for (name, file) in &resolved {
        inputs.push(file.clone());
        let (w, h, recipe_source) = if is_texpack(file) {
            if !file.is_file() {
                return Err(TexPackError::new(
                    codes::MISSING_SOURCE,
                    format!(
                        "{}: source `{name}`: {}: no such file",
                        path.display(),
                        file.display()
                    ),
                ));
            }
            let inner = plan_inner(file, stack)?;
            inputs.extend(inner.inputs.iter().cloned());
            (inner.width, inner.height, true)
        } else {
            let (w, h) = image_size(file).map_err(|e| {
                TexPackError::new(
                    e.code,
                    format!("{}: source `{name}`: {}", path.display(), e.message),
                )
            })?;
            (w, h, false)
        };
        sizes.push((name.clone(), w, h));
        nested.push(recipe_source);
    }
    let (width, height) = output_size(path, &recipe, &sizes)?;
    stack.pop();
    let sources = resolved
        .into_iter()
        .zip(sizes)
        .zip(nested)
        .map(|(((name, path), (_, w, h)), recipe)| PlannedSource {
            name,
            path,
            width: w,
            height: h,
            recipe,
            resized: (w, h) != (width, height),
        })
        .collect();
    Ok(PackPlan {
        recipe,
        sources,
        width,
        height,
        inputs,
    })
}

/// The files the recipe at `path` reads, best effort (a recipe that does not plan names the
/// sources it can resolve): the live cooker's and the cook key's dependency list.
#[must_use]
pub fn recipe_inputs(path: &Path) -> Vec<PathBuf> {
    if let Ok(plan) = plan_pack(path) {
        return plan.inputs;
    }
    let Ok(recipe) = read_recipe(path) else {
        return Vec::new();
    };
    let dir = path.parent().unwrap_or_else(|| Path::new(""));
    recipe
        .sources
        .values()
        .filter_map(|rel| crate::cook::join_portable(dir, rel).ok())
        .map(|p| lexical(&p))
        .collect()
}

/// Packs the recipe at `path` into an RGBA8 image (sources decoded, resized, channels taken).
///
/// # Errors
/// Any [`codes`] value.
pub fn pack_rgba8(path: &Path) -> Result<PackedImage, TexPackError> {
    let _span = tracing::info_span!("texpack_pack").entered();
    pack_inner(path, &mut Vec::new())
}

/// A decoded RGBA8 source: width, height and the texels.
type DecodedRgba = (u32, u32, Vec<u8>);

fn decode_image(path: &Path) -> Result<DecodedRgba, TexPackError> {
    let bytes = std::fs::read(path)
        .map_err(|e| TexPackError::new(codes::MISSING_SOURCE, format!("{}: {e}", path.display())))?;
    let reader = with_format(image::ImageReader::new(std::io::Cursor::new(bytes)), path)
        .map_err(|e| TexPackError::new(codes::DECODE, format!("{}: {e}", path.display())))?;
    let img = reader
        .decode()
        .map_err(|e| TexPackError::new(codes::DECODE, format!("{}: {e}", path.display())))?
        .into_rgba8();
    let (w, h) = img.dimensions();
    Ok((w, h, img.into_raw()))
}

fn pack_inner(path: &Path, stack: &mut Vec<PathBuf>) -> Result<PackedImage, TexPackError> {
    enter(path, stack)?;
    let recipe = read_recipe(path)?;
    let resolved = resolve_sources(path, &recipe)?;
    let mut images: Vec<(String, DecodedRgba)> = Vec::with_capacity(resolved.len());
    let mut inputs = Vec::new();
    for (name, file) in &resolved {
        inputs.push(file.clone());
        let image = if is_texpack(file) {
            if !file.is_file() {
                return Err(TexPackError::new(
                    codes::MISSING_SOURCE,
                    format!(
                        "{}: source `{name}`: {}: no such file",
                        path.display(),
                        file.display()
                    ),
                ));
            }
            let inner = pack_inner(file, stack)?;
            inputs.extend(inner.inputs.iter().cloned());
            (inner.width, inner.height, inner.rgba8)
        } else {
            if !file.is_file() {
                return Err(TexPackError::new(
                    codes::MISSING_SOURCE,
                    format!(
                        "{}: source `{name}`: {}: no such file",
                        path.display(),
                        file.display()
                    ),
                ));
            }
            decode_image(file).map_err(|e| {
                TexPackError::new(
                    e.code,
                    format!("{}: source `{name}`: {}", path.display(), e.message),
                )
            })?
        };
        images.push((name.clone(), image));
    }
    let sizes: Vec<(String, u32, u32)> = images.iter().map(|(n, (w, h, _))| (n.clone(), *w, *h)).collect();
    let (width, height) = output_size(path, &recipe, &sizes)?;
    stack.pop();
    let images: BTreeMap<String, Vec<u8>> = images
        .into_iter()
        .map(|(name, (w, h, px))| {
            let px = if (w, h) == (width, height) {
                px
            } else {
                resize_rgba8(w, h, &px, width, height)
            };
            (name, px)
        })
        .collect();
    let texels = width as usize * height as usize;
    let mut rgba8 = vec![0u8; texels * 4];
    for (out_channel, channel) in recipe.channels().into_iter().enumerate() {
        match channel {
            PackChannel::Constant(value) => {
                let v = (value.clamp(0.0, 1.0) * 255.0).round() as u8;
                for px in rgba8.as_chunks_mut::<4>().0 {
                    px[out_channel] = v;
                }
            }
            PackChannel::Source {
                source,
                channel,
                invert,
            } => {
                // `validate` checked the name and the channel.
                let Some(src) = images.get(source) else {
                    return Err(TexPackError::new(codes::UNKNOWN_SOURCE, source.clone()));
                };
                for (out, px) in rgba8
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(src.as_chunks::<4>().0)
                {
                    let v = read_channel(px, channel);
                    out[out_channel] = if *invert { 255 - v } else { v };
                }
            }
        }
    }
    Ok(PackedImage {
        width,
        height,
        rgba8,
        inputs,
    })
}

/// One texel's `channel` (`r`, `g`, `b`, `a`, `luma`).
fn read_channel(px: &[u8; 4], channel: &str) -> u8 {
    match channel {
        "r" => px[0],
        "g" => px[1],
        "b" => px[2],
        "a" => px[3],
        // Rec. 709 luma over the stored values.
        _ => (0.2126 * f32::from(px[0]) + 0.7152 * f32::from(px[1]) + 0.0722 * f32::from(px[2]))
            .round()
            .clamp(0.0, 255.0) as u8,
    }
}

/// The source taps and weights of each of `dst` output samples along an edge of `src` texels:
/// an area average of the source texels the output texel covers when shrinking, a linear
/// interpolation between the two nearest texel centres when growing.
fn axis_weights(src: u32, dst: u32) -> Vec<Vec<(usize, f32)>> {
    let scale = f64::from(src) / f64::from(dst);
    (0..dst)
        .map(|i| {
            if src > dst {
                let (x0, x1) = (f64::from(i) * scale, f64::from(i + 1) * scale);
                let mut taps = Vec::new();
                let mut s = x0.floor() as u32;
                while f64::from(s) < x1 && s < src {
                    let lo = f64::from(s).max(x0);
                    let hi = f64::from(s + 1).min(x1);
                    if hi > lo {
                        taps.push((s as usize, ((hi - lo) / scale) as f32));
                    }
                    s += 1;
                }
                taps
            } else {
                let c = ((f64::from(i) + 0.5) * scale - 0.5).max(0.0);
                let s0 = (c.floor() as u32).min(src - 1);
                let s1 = (s0 + 1).min(src - 1);
                let t = (c - f64::from(s0)) as f32;
                if s0 == s1 || t <= 0.0 {
                    vec![(s0 as usize, 1.0)]
                } else {
                    vec![(s0 as usize, 1.0 - t), (s1 as usize, t)]
                }
            }
        })
        .collect()
}

/// `rgba` (`w`×`h`) resized to `nw`×`nh`: separable, area-averaged when shrinking and bilinear
/// when growing, deterministic.
#[must_use]
pub fn resize_rgba8(w: u32, h: u32, rgba: &[u8], nw: u32, nh: u32) -> Vec<u8> {
    let (w, h, nw, nh) = (w.max(1), h.max(1), nw.max(1), nh.max(1));
    let xw = axis_weights(w, nw);
    let yw = axis_weights(h, nh);
    // Horizontal pass into f32 rows.
    let mut rows = vec![0f32; nw as usize * h as usize * 4];
    for y in 0..h as usize {
        for (x, taps) in xw.iter().enumerate() {
            let o = (y * nw as usize + x) * 4;
            for &(sx, wt) in taps {
                let i = (y * w as usize + sx) * 4;
                for c in 0..4 {
                    rows[o + c] += wt * f32::from(rgba.get(i + c).copied().unwrap_or(0));
                }
            }
        }
    }
    let mut out = vec![0u8; nw as usize * nh as usize * 4];
    for (y, taps) in yw.iter().enumerate() {
        for x in 0..nw as usize {
            let o = (y * nw as usize + x) * 4;
            let mut acc = [0f32; 4];
            for &(sy, wt) in taps {
                let i = (sy * nw as usize + x) * 4;
                for (c, a) in acc.iter_mut().enumerate() {
                    *a += wt * rows[i + c];
                }
            }
            for (c, a) in acc.iter().enumerate() {
                out[o + c] = a.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// What cooking a recipe produced.
pub(crate) struct CookedPack {
    pub(crate) id: regia_asset::AssetId,
    pub(crate) settings_hash: [u8; 32],
    pub(crate) type_path: String,
    pub(crate) payload: Vec<u8>,
}

/// Cooks the recipe at `source` (its text `bytes`): packed, then cooked like a PNG with the
/// recipe's `.meta` settings. The settings hash covers the recipe text, the settings, the usage and
/// every input file's bytes.
pub(crate) fn cook_texpack(
    source: &Path,
    bytes: &[u8],
    platform: &str,
) -> Result<CookedPack, crate::cook::CookError> {
    use crate::cook::CookError;
    let meta =
        crate::db::AssetDatabase::load_or_create_meta(source, TEXPACK_IMPORTER).map_err(CookError::Db)?;
    let err = |e: TexPackError| CookError::TexturePack(e.to_string());
    let text = std::str::from_utf8(bytes)
        .map_err(|e| err(TexPackError::new(codes::PARSE, format!("not UTF-8: {e}"))))?;
    let recipe = TexturePackRecipe::from_ron(text).map_err(err)?;
    let settings =
        crate::texture_settings::TextureImporterSettings::from_meta_string(meta.settings.as_deref());
    let usage = settings.usage_or(recipe.usage());
    let packed = pack_rgba8(source).map_err(err)?;
    let own = format!("{};texpack;usage={usage:?};", settings.fingerprint());
    let mut own = own.into_bytes();
    own.extend_from_slice(bytes);
    let settings_hash = crate::cook::input_files_hash(&own, packed.inputs.iter().cloned());
    let image = regia_asset::CookedImage {
        width: packed.width,
        height: packed.height,
        rgba8: packed.rgba8,
    };
    let (type_path, payload) = crate::cook::cook_rgba8_payload(image, &settings, usage, platform)?;
    Ok(CookedPack {
        id: meta.id,
        settings_hash,
        type_path,
        payload,
    })
}

/// The versions a built-in migration step leaves (none: v1 is the first version).
#[must_use]
pub fn texpack_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of `.texpack.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn texpack_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<TexturePackRecipe>(),
        serde_variant_names::<PackChannel>(),
        serde_variant_names::<SizeMismatch>(),
        serde_variant_names::<TextureUsageSetting>(),
    ])
}

/// The version a `.texpack.ron`'s text declares (`regia upgrade-project`), `None` when it is not a
/// recipe.
#[must_use]
pub fn texpack_file_version(text: &str) -> Option<u32> {
    // A text scan, not a parse: a file of a newer version may hold fields this build rejects.
    let mut rest = text;
    while let Some(at) = rest.find("format_version") {
        let before = &rest[..at];
        let after = &rest[at + "format_version".len()..];
        rest = after;
        // Skip a mention inside a line comment.
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        if before[line_start..].contains("//") {
            continue;
        }
        let after = after.trim_start();
        let Some(after) = after.strip_prefix(':') else {
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

/// The checked-in example recipe (`docs/formats/texpack.example.ron`; the crate keeps its own
/// copy, a test holds the two byte-identical).
pub const EXAMPLE_RECIPE: &str = include_str!("../fixtures/texpack.example.ron");

#[cfg(test)]
mod tests;
