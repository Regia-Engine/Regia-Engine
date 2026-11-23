// SPDX-License-Identifier: MPL-2.0
//! The SVG importer (`docs/specs/12-assets.md`, ADR-3350): `.svg` sources rasterize at cook time to
//! straight-alpha sRGB RGBA8 at the sizes the `.meta` asks for, and `.atlas.ron` sprites that name an
//! `.svg` rasterize at the 1x and 2x UI scales.
//!
//! - **Limits first** ([`SvgLimits`]): the document is size-checked, parsed with a node limit and no
//!   DTD, and walked once for anything the cook must never follow (external references, scripts,
//!   embedded images, text) *before* the renderer sees it. Every refusal carries a stable lowercase
//!   code ([`SvgImportError::code`]).
//! - **No ambient access**: the renderer is built without text, fonts, raster images or system fonts,
//!   and the document is handed to it already parsed, so no file, font or network lookup can happen.
//! - **Raster**: the tree is scaled to the target size (viewBox and `preserveAspectRatio` are applied
//!   by the parser) and drawn with `tiny-skia`; the premultiplied result is demultiplied to straight
//!   alpha, the convention PNG sources use. The UI premultiplies in linear light when it uploads
//!   (`regia_ui`'s atlas), so SVG icons filter like any other icon.
//!
//! The parser and rasterizer are the `svg_import` feature (on by default, ADR-7972). Without it the
//! limits, settings, errors and report types stay, the document, report and raster functions are not
//! compiled, and a cook of an `.svg` (or an atlas sprite naming one) answers
//! [`crate::ImportError::ImporterDisabled`].
//!
//! Original code from the public SVG 1.1 (Second Edition) and SVG 2 specifications (through `usvg`,
//! `resvg` and `tiny-skia`); see `docs/provenance.md`.

#[cfg(feature = "svg_import")]
use std::path::Path;

use serde::{Deserialize, Serialize};

#[cfg(feature = "svg_import")]
use crate::cook::CookError;
#[cfg(feature = "svg_import")]
use crate::db::AssetDatabase;
#[cfg(feature = "svg_import")]
use crate::texture_settings::TextureImporterSettings;

/// The importer name the cook records for SVG sources.
pub const SVG_IMPORTER: &str = "svg";

/// Largest document, bytes.
pub const SVG_MAX_BYTES: usize = 1 << 20;
/// Most XML nodes (elements, text runs and comments) a document may hold.
pub const SVG_MAX_NODES: u32 = 20_000;
/// Longest raster edge, pixels.
pub const SVG_MAX_RASTER_EDGE: u32 = 4096;
/// Most pixels a raster may hold.
pub const SVG_MAX_RASTER_PIXELS: u64 = 1 << 24;
/// The UI scales an atlas SVG sprite is rasterized at.
pub const SVG_ATLAS_SCALES: [u32; 2] = [1, 2];

/// The document limits (the defaults are the constants above).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SvgLimits {
    /// Largest document, bytes.
    pub max_bytes: usize,
    /// Most XML nodes.
    pub max_nodes: u32,
    /// Longest raster edge, pixels.
    pub max_edge: u32,
    /// Most raster pixels.
    pub max_pixels: u64,
}

impl Default for SvgLimits {
    fn default() -> Self {
        Self {
            max_bytes: SVG_MAX_BYTES,
            max_nodes: SVG_MAX_NODES,
            max_edge: SVG_MAX_RASTER_EDGE,
            max_pixels: SVG_MAX_RASTER_PIXELS,
        }
    }
}

/// Why an SVG could not be imported. [`Self::code`] is the stable agent-protocol code; the display
/// text starts with it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SvgImportError {
    /// The document is larger than the byte limit.
    #[error("svg_too_large: {0}")]
    TooLarge(String),
    /// The bytes are not UTF-8 text (a compressed `.svgz` is refused too).
    #[error("svg_not_utf8: {0}")]
    NotUtf8(String),
    /// The XML does not parse, or has a DTD.
    #[error("svg_xml: {0}")]
    Xml(String),
    /// The document has more nodes than the limit.
    #[error("svg_too_many_nodes: {0}")]
    TooManyNodes(String),
    /// A reference leaves the document (a file, a network URL, a `data:` payload).
    #[error("svg_external_ref: {0}")]
    ExternalRef(String),
    /// A script element or an event-handler attribute.
    #[error("svg_script: {0}")]
    Script(String),
    /// The document contains text; outline it before import (ADR-3350).
    #[error("svg_text_unsupported: {0}")]
    TextUnsupported(String),
    /// An `<image>` or `<feImage>` element.
    #[error("svg_image_unsupported: {0}")]
    ImageUnsupported(String),
    /// The renderer's parser refused the document.
    #[error("svg_parse: {0}")]
    Parse(String),
    /// The document has no drawable size.
    #[error("svg_empty: {0}")]
    Empty(String),
    /// A requested raster is larger than the limits.
    #[error("svg_raster_too_large: {0}")]
    RasterTooLarge(String),
    /// A `.meta` setting is out of range.
    #[error("svg_settings: {0}")]
    Settings(String),
    /// An atlas SVG sprite combined with a feature it does not support (a grid).
    #[error("svg_atlas_unsupported: {0}")]
    AtlasUnsupported(String),
}

impl SvgImportError {
    /// The stable lowercase code (`svg_*`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooLarge(_) => "svg_too_large",
            Self::NotUtf8(_) => "svg_not_utf8",
            Self::Xml(_) => "svg_xml",
            Self::TooManyNodes(_) => "svg_too_many_nodes",
            Self::ExternalRef(_) => "svg_external_ref",
            Self::Script(_) => "svg_script",
            Self::TextUnsupported(_) => "svg_text_unsupported",
            Self::ImageUnsupported(_) => "svg_image_unsupported",
            Self::Parse(_) => "svg_parse",
            Self::Empty(_) => "svg_empty",
            Self::RasterTooLarge(_) => "svg_raster_too_large",
            Self::Settings(_) => "svg_settings",
            Self::AtlasUnsupported(_) => "svg_atlas_unsupported",
        }
    }
}

/// The `.meta` `settings` of an SVG (RON; unknown keys, such as the texture keys, are ignored here).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SvgSettings {
    /// Raster sizes: the longest edge of each, pixels. The cooked texture is the largest; smaller
    /// sizes are listed in the report and reached through mips.
    pub sizes_px: Vec<u32>,
    /// Multiplier on the document's own size, used when `sizes_px` is empty (default 1).
    pub scale: Option<f32>,
}

impl SvgSettings {
    /// Parses a `.meta` `settings` string; empty means the defaults.
    ///
    /// # Errors
    ///
    /// [`SvgImportError::Settings`] when the text does not parse.
    pub fn from_meta_string(settings: Option<&str>) -> Result<Self, SvgImportError> {
        let Some(s) = settings.filter(|s| !s.trim().is_empty()) else {
            return Ok(Self::default());
        };
        ron::from_str(s).map_err(|e| SvgImportError::Settings(format!("{e}")))
    }

    /// A stable text for the cook's settings hash.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        format!("sizes_px={:?};scale={:?}", self.sizes_px, self.scale)
    }
}

/// A rasterized image: straight-alpha sRGB RGBA8, row-major, top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvgRaster {
    /// Width, pixels.
    pub width: u32,
    /// Height, pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba8: Vec<u8>,
}

#[cfg(feature = "svg_import")]
/// A checked, parsed document.
pub struct SvgDocument {
    tree: usvg::Tree,
    limits: SvgLimits,
    nodes: u32,
}

/// What the importer saw in one document.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SvgReport {
    /// Source path as given.
    pub path: String,
    /// Document bytes.
    pub bytes: usize,
    /// XML nodes.
    pub nodes: u32,
    /// The document's own size (after viewBox handling), user units.
    pub intrinsic_size: [f32; 2],
    /// The raster sizes the settings ask for, `[width, height]` pixels, largest first.
    pub raster_sizes: Vec<[u32; 2]>,
    /// The size of the cooked texture, `[width, height]` pixels.
    pub cooked_size: [u32; 2],
    /// The UI scales an atlas sprite of this file is rasterized at.
    pub atlas_scales: Vec<u32>,
    /// `null` when the import succeeded, else the error code.
    pub error: Option<String>,
    /// The error text when the import failed.
    pub message: Option<String>,
}

#[cfg(feature = "svg_import")]
fn node_count_error(limits: &SvgLimits) -> SvgImportError {
    SvgImportError::TooManyNodes(format!("more than {} XML nodes", limits.max_nodes))
}

#[cfg(feature = "svg_import")]
/// Whether an `href`-like value stays inside the document.
fn local_ref(value: &str) -> bool {
    value.trim().starts_with('#')
}

#[cfg(feature = "svg_import")]
/// The first `url(...)` argument in `text` that points outside the document, if any.
fn external_url(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(at) = rest.find("url(") {
        let after = &rest[at + 4..];
        let end = after.find(')').unwrap_or(after.len());
        let arg = after[..end].trim().trim_matches(|c| c == '"' || c == '\'').trim();
        if !arg.starts_with('#') {
            return Some(arg.chars().take(60).collect());
        }
        rest = &after[end.min(after.len())..];
    }
    if lower.contains("@import") {
        return Some("@import".to_string());
    }
    None
}

#[cfg(feature = "svg_import")]
/// Walks the parsed XML for everything the cook must refuse.
fn scan(doc: &roxmltree::Document) -> Result<(), SvgImportError> {
    for node in doc.descendants().filter(|n| n.is_element()) {
        let name = node.tag_name().name().to_ascii_lowercase();
        match name.as_str() {
            "script" => return Err(SvgImportError::Script("a <script> element".into())),
            "foreignobject" => {
                return Err(SvgImportError::ExternalRef("a <foreignObject> element".into()));
            }
            "text" | "tspan" | "textpath" | "tref" => {
                return Err(SvgImportError::TextUnsupported(format!(
                    "a <{}> element: convert the text to outlines before import",
                    node.tag_name().name()
                )));
            }
            "image" | "feimage" => {
                return Err(SvgImportError::ImageUnsupported(format!(
                    "a <{}> element: embedded and linked images are not imported",
                    node.tag_name().name()
                )));
            }
            "style" => {
                let css: String = node.children().filter_map(|c| c.text()).collect();
                if let Some(url) = external_url(&css) {
                    return Err(SvgImportError::ExternalRef(format!(
                        "a style sheet references `{url}`"
                    )));
                }
            }
            _ => {}
        }
        for attr in node.attributes() {
            let key = attr.name().to_ascii_lowercase();
            let value = attr.value();
            if key.starts_with("on") {
                return Err(SvgImportError::Script(format!(
                    "the event handler `{}`",
                    attr.name()
                )));
            }
            if value.trim_start().to_ascii_lowercase().starts_with("javascript:") {
                return Err(SvgImportError::Script(format!(
                    "a javascript: value in `{}`",
                    attr.name()
                )));
            }
            if key == "href" && !local_ref(value) {
                return Err(SvgImportError::ExternalRef(format!(
                    "`{}` points outside the document: `{}`",
                    attr.name(),
                    value.chars().take(60).collect::<String>()
                )));
            }
            if let Some(url) = external_url(value) {
                return Err(SvgImportError::ExternalRef(format!(
                    "`{}` references `{url}`",
                    attr.name()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(feature = "svg_import")]
impl SvgDocument {
    /// Checks and parses `bytes` under the default [`SvgLimits`].
    ///
    /// # Errors
    ///
    /// Any [`SvgImportError`] of the document checks.
    pub fn parse(bytes: &[u8]) -> Result<Self, SvgImportError> {
        Self::parse_with(bytes, SvgLimits::default())
    }

    /// Checks and parses `bytes` under `limits`.
    ///
    /// # Errors
    ///
    /// Any [`SvgImportError`] of the document checks.
    pub fn parse_with(bytes: &[u8], limits: SvgLimits) -> Result<Self, SvgImportError> {
        let _span = tracing::info_span!("svg_parse", bytes = bytes.len()).entered();
        if bytes.len() > limits.max_bytes {
            return Err(SvgImportError::TooLarge(format!(
                "{} bytes, the limit is {}",
                bytes.len(),
                limits.max_bytes
            )));
        }
        let text = std::str::from_utf8(bytes).map_err(|e| SvgImportError::NotUtf8(e.to_string()))?;
        let options = roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: limits.max_nodes,
        };
        let doc = roxmltree::Document::parse_with_options(text, options).map_err(|e| match e {
            roxmltree::Error::NodesLimitReached => node_count_error(&limits),
            other => SvgImportError::Xml(other.to_string()),
        })?;
        let nodes = u32::try_from(doc.descendants().count()).unwrap_or(u32::MAX);
        if nodes > limits.max_nodes {
            return Err(node_count_error(&limits));
        }
        scan(&doc)?;
        // No resource directory, and resolvers that find nothing: the renderer cannot read a file.
        let options = usvg::Options {
            resources_dir: None,
            image_href_resolver: usvg::ImageHrefResolver {
                resolve_data: Box::new(|_, _, _| None),
                resolve_string: Box::new(|_, _| None),
            },
            ..usvg::Options::default()
        };
        let tree =
            usvg::Tree::from_xmltree(&doc, &options).map_err(|e| SvgImportError::Parse(e.to_string()))?;
        let size = tree.size();
        if !(size.width().is_finite()
            && size.height().is_finite()
            && size.width() > 0.0
            && size.height() > 0.0)
        {
            return Err(SvgImportError::Empty(
                "the document has no positive width and height".into(),
            ));
        }
        Ok(Self { tree, limits, nodes })
    }

    /// The document's own size (width, height), user units.
    #[must_use]
    pub fn intrinsic_size(&self) -> [f32; 2] {
        let s = self.tree.size();
        [s.width(), s.height()]
    }

    /// XML nodes in the document.
    #[must_use]
    pub fn nodes(&self) -> u32 {
        self.nodes
    }

    /// The pixel size of the raster whose longest edge is `edge` (the other edge keeps the aspect
    /// ratio, at least 1).
    ///
    /// # Errors
    ///
    /// [`SvgImportError::RasterTooLarge`] over the limits, [`SvgImportError::Settings`] for 0.
    pub fn size_for_edge(&self, edge: u32) -> Result<[u32; 2], SvgImportError> {
        if edge == 0 {
            return Err(SvgImportError::Settings("a raster size of 0".into()));
        }
        let [w, h] = self.intrinsic_size();
        let k = f64::from(edge) / f64::from(w.max(h));
        let (px, py) = if w >= h {
            (edge, ((f64::from(h) * k).round() as u32).max(1))
        } else {
            (((f64::from(w) * k).round() as u32).max(1), edge)
        };
        self.check_size(px, py)
    }

    /// The pixel size at `scale` times the document's own size.
    ///
    /// # Errors
    ///
    /// [`SvgImportError::RasterTooLarge`] over the limits, [`SvgImportError::Settings`] for a scale
    /// that is not a positive finite number.
    pub fn size_for_scale(&self, scale: f32) -> Result<[u32; 2], SvgImportError> {
        if !(scale.is_finite() && scale > 0.0) {
            return Err(SvgImportError::Settings(format!("scale {scale} is not positive")));
        }
        let [w, h] = self.intrinsic_size();
        let to_px = |v: f32| {
            let px = (f64::from(v) * f64::from(scale)).round();
            if px >= f64::from(u32::MAX) {
                u32::MAX
            } else {
                (px as u32).max(1)
            }
        };
        self.check_size(to_px(w), to_px(h))
    }

    fn check_size(&self, w: u32, h: u32) -> Result<[u32; 2], SvgImportError> {
        let l = &self.limits;
        if w > l.max_edge || h > l.max_edge || u64::from(w) * u64::from(h) > l.max_pixels {
            return Err(SvgImportError::RasterTooLarge(format!(
                "{w}x{h} px, the limits are {} px an edge and {} px in all",
                l.max_edge, l.max_pixels
            )));
        }
        Ok([w, h])
    }

    /// Rasterizes the document into a `w` x `h` image (the content is scaled to fill it).
    ///
    /// # Errors
    ///
    /// [`SvgImportError::RasterTooLarge`] over the limits, [`SvgImportError::Empty`] for a zero size.
    pub fn rasterize(&self, w: u32, h: u32) -> Result<SvgRaster, SvgImportError> {
        let _span = tracing::info_span!("svg_rasterize", w, h).entered();
        self.check_size(w, h)?;
        let mut pixmap = tiny_skia::Pixmap::new(w, h)
            .ok_or_else(|| SvgImportError::Empty(format!("a {w}x{h} raster cannot be allocated")))?;
        let [iw, ih] = self.intrinsic_size();
        let transform = tiny_skia::Transform::from_scale(w as f32 / iw, h as f32 / ih);
        resvg::render(&self.tree, transform, &mut pixmap.as_mut());
        let mut rgba8 = pixmap.take();
        demultiply(&mut rgba8);
        Ok(SvgRaster {
            width: w,
            height: h,
            rgba8,
        })
    }
}

#[cfg(feature = "svg_import")]
/// Premultiplied RGBA8 to straight alpha, rounding to nearest. Fully transparent pixels become 0.
fn demultiply(pixels: &mut [u8]) {
    for p in pixels.as_chunks_mut::<4>().0 {
        let a = u32::from(p[3]);
        if a == 0 {
            *p = [0; 4];
        } else if a != 255 {
            for c in &mut p[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

#[cfg(feature = "svg_import")]
/// The raster sizes `settings` ask for, largest first, plus the one that is cooked.
///
/// # Errors
///
/// The size errors of [`SvgDocument::size_for_edge`] and [`SvgDocument::size_for_scale`].
pub fn plan_sizes(doc: &SvgDocument, settings: &SvgSettings) -> Result<Vec<[u32; 2]>, SvgImportError> {
    let mut sizes = if settings.sizes_px.is_empty() {
        vec![doc.size_for_scale(settings.scale.unwrap_or(1.0))?]
    } else {
        settings
            .sizes_px
            .iter()
            .map(|&e| doc.size_for_edge(e))
            .collect::<Result<Vec<_>, _>>()?
    };
    sizes.sort_by(|a, b| (b[0].max(b[1])).cmp(&a[0].max(a[1])).then(b.cmp(a)));
    sizes.dedup();
    Ok(sizes)
}

#[cfg(feature = "svg_import")]
/// Builds the report of `bytes` under `settings` (also the failure report: `error` set).
#[must_use]
pub fn report(path: &str, bytes: &[u8], settings: &SvgSettings) -> SvgReport {
    let mut r = SvgReport {
        path: path.to_string(),
        bytes: bytes.len(),
        nodes: 0,
        intrinsic_size: [0.0; 2],
        raster_sizes: vec![],
        cooked_size: [0; 2],
        atlas_scales: SVG_ATLAS_SCALES.to_vec(),
        error: None,
        message: None,
    };
    let result = SvgDocument::parse(bytes).and_then(|doc| {
        r.nodes = doc.nodes();
        r.intrinsic_size = doc.intrinsic_size();
        r.raster_sizes = plan_sizes(&doc, settings)?;
        r.cooked_size = r.raster_sizes.first().copied().unwrap_or([0; 2]);
        Ok(())
    });
    if let Err(e) = result {
        r.error = Some(e.code().to_string());
        r.message = Some(e.to_string());
    }
    r
}

#[cfg(feature = "svg_import")]
/// The atlas rasters of one SVG sprite: `(name suffix pixel height, raster)` per UI scale. The 1x
/// raster is the document's own size rounded to whole pixels.
///
/// # Errors
///
/// Any [`SvgImportError`] of the document checks or the size limits.
pub fn atlas_rasters(bytes: &[u8]) -> Result<Vec<SvgRaster>, SvgImportError> {
    let doc = SvgDocument::parse(bytes)?;
    SVG_ATLAS_SCALES
        .iter()
        .map(|&k| {
            let [w, h] = doc.size_for_scale(k as f32)?;
            doc.rasterize(w, h)
        })
        .collect()
}

#[cfg(feature = "svg_import")]
/// The cook of one SVG source: what `cook.rs` records.
pub(crate) struct SvgCook {
    pub(crate) settings_hash: [u8; 32],
    pub(crate) id: regia_asset::AssetId,
    pub(crate) type_path: String,
    pub(crate) payload: Vec<u8>,
}

#[cfg(feature = "svg_import")]
/// Cooks an `.svg`: rasterizes the largest size in its settings and compresses it like any
/// other RGBA8 texture (`.meta` texture settings apply).
pub(crate) fn cook_svg(source: &Path, bytes: &[u8], platform: &str) -> Result<SvgCook, CookError> {
    let _span =
        tracing::info_span!(target: "regia_asset_pipeline", "cook_svg", path = %source.display()).entered();
    let fail = |e: SvgImportError| CookError::Import(crate::ImportError::Image(e.to_string()));
    let meta = AssetDatabase::load_or_create_meta(source, "svg").map_err(CookError::Db)?;
    let svg_settings = SvgSettings::from_meta_string(meta.settings.as_deref()).map_err(fail)?;
    let texture_settings = TextureImporterSettings::from_meta_string(meta.settings.as_deref());
    let doc = SvgDocument::parse(bytes).map_err(fail)?;
    let sizes = plan_sizes(&doc, &svg_settings).map_err(fail)?;
    let [w, h] = sizes.first().copied().unwrap_or([1, 1]);
    let raster = doc.rasterize(w, h).map_err(fail)?;
    let image = regia_asset::CookedImage {
        width: raster.width,
        height: raster.height,
        rgba8: raster.rgba8,
    };
    let usage = texture_settings.usage_or(regia_asset::TextureCookUsage::ColorSrgb);
    let (type_path, payload) = crate::cook::cook_rgba8_payload(image, &texture_settings, usage, platform)?;
    let settings_hash = *blake3::hash(
        format!(
            "{};{};svg_importer={}",
            texture_settings.fingerprint(),
            svg_settings.fingerprint(),
            SVG_IMPORTER_VERSION
        )
        .as_bytes(),
    )
    .as_bytes();
    Ok(SvgCook {
        settings_hash,
        id: meta.id,
        type_path,
        payload,
    })
}

/// Bumped when the importer's output changes for the same source.
pub const SVG_IMPORTER_VERSION: u32 = 1;

#[cfg(all(test, feature = "svg_import"))]
mod tests {
    use super::*;

    const SQUARE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">
        <rect x="4" y="4" width="8" height="8" fill="#ff0000"/></svg>"##;

    fn px(r: &SvgRaster, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * r.width + x) * 4) as usize;
        [r.rgba8[at], r.rgba8[at + 1], r.rgba8[at + 2], r.rgba8[at + 3]]
    }

    fn raster(svg: &str, w: u32, h: u32) -> SvgRaster {
        SvgDocument::parse(svg.as_bytes())
            .expect("parses")
            .rasterize(w, h)
            .expect("rasterizes")
    }

    fn err_code(svg: &str) -> &'static str {
        SvgDocument::parse(svg.as_bytes())
            .err()
            .map_or("ok", |e| e.code())
    }

    #[test]
    fn a_simple_path_has_known_pixels() {
        let r = raster(SQUARE, 16, 16);
        assert_eq!(px(&r, 8, 8), [255, 0, 0, 255], "inside the square");
        assert_eq!(px(&r, 1, 1), [0, 0, 0, 0], "outside is transparent black");
        assert_eq!(px(&r, 3, 8), [0, 0, 0, 0]);
        assert_eq!(px(&r, 4, 8), [255, 0, 0, 255], "the edge pixel is fully covered");
    }

    #[test]
    fn a_gradient_runs_across_the_shape() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="8" viewBox="0 0 32 8">
            <defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="0">
            <stop offset="0" stop-color="#000000"/><stop offset="1" stop-color="#ffffff"/></linearGradient></defs>
            <rect width="32" height="8" fill="url(#g)"/></svg>"##;
        let r = raster(svg, 32, 8);
        let (left, mid, right) = (px(&r, 0, 4), px(&r, 16, 4), px(&r, 31, 4));
        assert!(left[0] < 16 && right[0] > 240, "{left:?} {right:?}");
        assert!((i32::from(mid[0]) - 128).abs() <= 12, "{mid:?}");
        assert!(left[3] == 255 && mid[3] == 255 && right[3] == 255);
    }

    #[test]
    fn the_view_box_scales_the_drawing_to_the_target() {
        // A 0..8 view box with a 4x4 square at the origin: at 32 px it covers the top-left 16 px.
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8" viewBox="0 0 8 8">
            <rect width="4" height="4" fill="#00ff00"/></svg>"##;
        let r = raster(svg, 32, 32);
        assert_eq!(px(&r, 8, 8), [0, 255, 0, 255]);
        assert_eq!(px(&r, 24, 24), [0, 0, 0, 0]);
        assert_eq!(px(&r, 15, 15), [0, 255, 0, 255]);
        assert_eq!(px(&r, 17, 17), [0, 0, 0, 0]);
        let doc = SvgDocument::parse(svg.as_bytes()).expect("parses");
        assert_eq!(doc.size_for_edge(24).expect("size"), [24, 24]);
        assert_eq!(doc.size_for_scale(2.0).expect("size"), [16, 16]);
    }

    #[test]
    fn a_view_box_without_a_size_uses_its_extent() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 10">
            <rect width="20" height="10" fill="#0000ff"/></svg>"##;
        let doc = SvgDocument::parse(svg.as_bytes()).expect("parses");
        assert_eq!(doc.intrinsic_size(), [20.0, 10.0]);
        assert_eq!(doc.size_for_edge(40).expect("size"), [40, 20]);
    }

    #[test]
    fn transparent_edges_are_straight_alpha() {
        // A half-transparent red rect: straight alpha keeps the colour at full red.
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4" viewBox="0 0 4 4">
            <rect width="4" height="4" fill="#ff0000" fill-opacity="0.5"/></svg>"##;
        let r = raster(svg, 4, 4);
        let p = px(&r, 2, 2);
        assert!((i32::from(p[3]) - 128).abs() <= 1, "{p:?}");
        assert_eq!(
            (p[0], p[1], p[2]),
            (255, 0, 0),
            "demultiplied back to full red: {p:?}"
        );
        // A partly covered edge pixel keeps its colour and gets partial alpha.
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4" viewBox="0 0 4 4">
            <rect x="0.5" width="3.5" height="4" fill="#ff0000"/></svg>"##;
        let r = raster(svg, 4, 4);
        let edge = px(&r, 0, 2);
        assert!(edge[3] > 100 && edge[3] < 160, "{edge:?}");
        assert!(edge[0] >= 250 && edge[1] == 0, "{edge:?}");
    }

    #[test]
    fn two_rasterizations_give_the_same_bytes() {
        let a = raster(SQUARE, 24, 24);
        let b = raster(SQUARE, 24, 24);
        assert_eq!(a, b);
    }

    #[test]
    fn external_references_are_refused() {
        let href = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="4" height="4">
            <use xlink:href="other.svg#a"/></svg>"##;
        assert_eq!(err_code(href), "svg_external_ref");
        let net = r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">
            <rect width="4" height="4" fill="url(http://example.com/g.svg#x)"/></svg>"##;
        assert_eq!(err_code(net), "svg_external_ref");
        let css = r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">
            <style>@import url("http://example.com/a.css");</style></svg>"##;
        assert_eq!(err_code(css), "svg_external_ref");
        let local = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="4" height="4">
            <defs><rect id="a" width="2" height="2"/></defs><use xlink:href="#a"/></svg>"##;
        assert_eq!(err_code(local), "ok", "a same-document reference is fine");
    }

    #[test]
    fn marker_orient_is_not_an_event_handler() {
        // `orient` is a standard marker attribute; only `on...` event handlers are `svg_script`.
        for orient in ["auto", "auto-start-reverse", "45"] {
            let svg = format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><defs><marker id="m" markerWidth="2" markerHeight="2" refX="1" refY="1" orient="{orient}"><rect width="2" height="2"/></marker></defs><path d="M1 1 L7 7" stroke="black" marker-end="url(#m)"/></svg>"##
            );
            assert_eq!(err_code(&svg), "ok", "orient=\"{orient}\"");
        }
    }

    #[test]
    fn scripts_images_and_text_are_refused_with_their_codes() {
        let script = r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><script>alert(1)</script></svg>"##;
        assert_eq!(err_code(script), "svg_script");
        let handler = r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="2" height="2" onclick="x()"/></svg>"##;
        assert_eq!(err_code(handler), "svg_script");
        let image = r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><image width="2" height="2"/></svg>"##;
        assert_eq!(err_code(image), "svg_image_unsupported");
        let text = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="10"><text x="1" y="8">Hi</text></svg>"##;
        assert_eq!(err_code(text), "svg_text_unsupported");
        assert_eq!(err_code("not xml"), "svg_xml");
        let dtd = r##"<!DOCTYPE svg [<!ENTITY a "b">]><svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"/>"##;
        assert_eq!(err_code(dtd), "svg_xml");
    }

    #[test]
    fn oversized_documents_and_rasters_are_refused() {
        let big = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><!-- {} --></svg>"##,
            "x".repeat(SVG_MAX_BYTES)
        );
        assert_eq!(err_code(&big), "svg_too_large");
        let nodes = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">{}</svg>"##,
            "<g/>".repeat(SVG_MAX_NODES as usize + 1)
        );
        assert_eq!(err_code(&nodes), "svg_too_many_nodes");
        let doc = SvgDocument::parse(SQUARE.as_bytes()).expect("parses");
        let too_wide = doc
            .size_for_edge(SVG_MAX_RASTER_EDGE + 1)
            .expect_err("over the edge limit");
        assert_eq!(too_wide.code(), "svg_raster_too_large");
        assert_eq!(
            doc.size_for_scale(1.0e9).expect_err("huge").code(),
            "svg_raster_too_large"
        );
        assert_eq!(doc.size_for_edge(0).expect_err("zero").code(), "svg_settings");
        assert_eq!(
            doc.size_for_scale(f32::NAN).expect_err("nan").code(),
            "svg_settings"
        );
        let tight = SvgLimits {
            max_pixels: 100,
            ..SvgLimits::default()
        };
        let doc = SvgDocument::parse_with(SQUARE.as_bytes(), tight).expect("parses");
        assert_eq!(
            doc.rasterize(16, 16).expect_err("over the pixel limit").code(),
            "svg_raster_too_large"
        );
    }

    #[test]
    fn settings_pick_the_sizes() {
        let doc = SvgDocument::parse(SQUARE.as_bytes()).expect("parses");
        let s = SvgSettings::from_meta_string(Some("(sizes_px: [16, 64, 32, 64])")).expect("parses");
        assert_eq!(
            plan_sizes(&doc, &s).expect("plan"),
            vec![[64, 64], [32, 32], [16, 16]]
        );
        let s = SvgSettings::from_meta_string(Some("(scale: Some(3.0))")).expect("parses");
        assert_eq!(plan_sizes(&doc, &s).expect("plan"), vec![[48, 48]]);
        assert_eq!(
            plan_sizes(&doc, &SvgSettings::default()).expect("plan"),
            vec![[16, 16]]
        );
        assert!(SvgSettings::from_meta_string(Some("(sizes_px: oops")).is_err());
        // Texture keys in the same settings are ignored here.
        assert!(SvgSettings::from_meta_string(Some("(max_size: Some(64), sizes_px: [8])")).is_ok());
    }

    #[test]
    fn atlas_rasters_cover_the_ui_scales() {
        let rs = atlas_rasters(SQUARE.as_bytes()).expect("rasters");
        assert_eq!(
            rs.iter().map(|r| (r.width, r.height)).collect::<Vec<_>>(),
            vec![(16, 16), (32, 32)]
        );
        assert_eq!(px(&rs[1], 16, 16), [255, 0, 0, 255]);
        assert_eq!(px(&rs[1], 7, 16), [0, 0, 0, 0]);
        assert_eq!(px(&rs[1], 8, 16), [255, 0, 0, 255], "the 2x square starts at 8");
    }

    #[test]
    fn the_report_names_an_error_code() {
        let ok = report("a.svg", SQUARE.as_bytes(), &SvgSettings::default());
        assert_eq!((ok.error.as_deref(), ok.cooked_size), (None, [16, 16]));
        let bad = report("b.svg", b"<svg", &SvgSettings::default());
        assert_eq!(bad.error.as_deref(), Some("svg_xml"));
    }
}
