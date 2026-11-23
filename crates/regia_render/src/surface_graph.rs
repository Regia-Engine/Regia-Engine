// SPDX-License-Identifier: MPL-2.0
//! Material-graph surface functions on the forward path (`specs/13` § 7, `specs/22` T2.4).
//!
//! A graph material carries a generated WGSL `material_surface` function. The renderer splices
//! it into `lab_forward.wgsl` under the `MATERIAL_GRAPH` define, so a graph replaces only the
//! surface stage (base colour, normal, roughness, metallic, occlusion, emissive, alpha) and the
//! lighting stays the one hand-written code path. Without a graph the define is off and the
//! forward shader is the same source as before.
//!
//! The binding contract the generator targets:
//! - group 2 binding 0, `material: LabMaterial`, gains `graph: array<vec4<f32>, 16>` after the
//!   standard block. Slot 0 is the built-ins (`x` = time in seconds); parameters use 1..16.
//! - the seven material textures ([`GRAPH_TEXTURE_BINDINGS`]) and the material sampler
//!   `tex_samp`. A graph texture may use any slot in any colour space.
//! - `frame` (camera, the `pcss.z` parallax profile gate) is readable.
//!
//! A graph may also carry a vertex stage, `material_vertex_offset(VertexOffsetInput) -> vec3`
//! (a world position offset in metres). It is spliced into every pass that positions the mesh
//! (forward and depth through `lab_forward.wgsl`, shadows through `lab_shadow.wgsl`, motion
//! vectors through `velocity.wgsl`) under `MATERIAL_GRAPH_VERTEX`, so the displaced surface is
//! drawn, depth-tested, shadowed and reprojected at the same place. A graph that reads the
//! second UV set compiles with `MATERIAL_GRAPH_UV1` and draws with a second vertex stream.
//!
//! A graph that reads `world_position` or `object_position` sees world positions, not the
//! frame's local ones, so a world-space pattern (triplanar, world noise, a wind phase) stays on
//! the same world point across a floating-origin rebase (spec 78 § 12, ADR-2435). The splice
//! renames its functions ([`GRAPH_WORLD_SURFACE_FN`], [`GRAPH_WORLD_VERTEX_FN`]) and adds an
//! adapter that adds the origin from [`graph_origin_slot`]; the renderer writes that slot
//! ([`SurfaceGraph::write_origin`]). The generated text, and so the pipeline key, is unchanged
//! by the adapter. Hand-written code that reads every parameter slot has no free slot and still
//! sees local positions; `regia_matgraph` refuses to compile that case (ADR-5005).
//!
//! The graph compiler lives in `regia_matgraph`; this module only knows WGSL text.

use std::collections::HashMap;
use std::sync::Arc;

use naga_oil::compose::ShaderDefValue;

use crate::{LabTexture, RenderError};

/// `vec4` slots in the graph parameter block (slot 0 holds the built-ins).
pub const GRAPH_PARAM_SLOTS: usize = 16;

/// Texture slots a graph can bind.
pub const GRAPH_TEXTURE_SLOTS: usize = 7;

/// WGSL names of the texture slots, in slot order (group 2 bindings 1, 3, 4, 5, 6, 7, 8).
pub const GRAPH_TEXTURE_BINDINGS: [&str; GRAPH_TEXTURE_SLOTS] = [
    "albedo_tex",
    "normal_tex",
    "occlusion_tex",
    "emissive_tex",
    "detail_tex",
    "detail_normal",
    "aux_tex",
];

/// WGSL name of the material sampler graph samples use.
pub const GRAPH_SAMPLER: &str = "tex_samp";

/// The line in `lab_forward.wgsl` that the generated surface function replaces.
pub const GRAPH_SOURCE_MARKER: &str = "// @material_graph_source";

/// The line in `lab_forward.wgsl`, `lab_shadow.wgsl` and `velocity.wgsl` that the generated
/// vertex stage (`material_vertex_offset`) replaces.
pub const GRAPH_VERTEX_MARKER: &str = "// @material_graph_vertex_source";

/// `vec4` slots of the standard material block that precedes the graph parameters in the
/// material uniform (the shadow and velocity hosts declare it as an opaque array).
pub const GRAPH_STANDARD_VEC4S: usize = 22;

/// Vertex layout of the second UV set (buffer slot 1, `@location(7)`, `f32x2`).
pub const UV1_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: 8,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 0,
        shader_location: 7,
    }],
};

/// Bytes of the graph parameter block appended to the material uniform.
pub const GRAPH_PARAM_BYTES: usize = GRAPH_PARAM_SLOTS * 16;

/// One texture bound by a graph material.
///
/// Upload order: [`Self::dynamic`], then [`Self::shared`], then [`Self::compressed`] when the
/// device can sample it, then [`Self::image`]. An empty slot still binds white. [`Self::rgba`]
/// is the RGBA8-only form the graph builder has always produced. A struct literal from outside
/// this crate should call [`Self::rgba`], [`Self::from_shared`] or [`Self::from_dynamic`], or
/// fill the remaining fields with [`Default`].
#[derive(Clone, Debug)]
pub struct GraphTexture {
    /// Pixels (RGBA8). Empty when [`Self::shared`] or [`Self::dynamic`] holds the image.
    pub image: LabTexture,
    /// Upload [`Self::image`] as `Rgba8UnormSrgb` (color) instead of `Rgba8Unorm` (data).
    /// A shared or dynamic image uses its own flag.
    pub srgb: bool,
    /// Cooked chain for this slot (BC7 sRGB for color, BC5 or BC7 linear for data). Uploaded
    /// natively when the device supports the format; otherwise [`Self::image`] or the chain's
    /// RGBA8 decode is used. Ignored when [`Self::shared`] already carries the same chain.
    pub compressed: Option<std::sync::Arc<crate::CompressedTexture>>,
    /// Image identity shared with every other material that holds this value.
    pub shared: Option<crate::SharedImage>,
    /// In-place level 0. The material bind group is not rebuilt when the host writes it.
    pub dynamic: Option<crate::DynamicTexture>,
    /// A counted texture asset (ADR-8610): the asset server's one GPU upload, bound in this
    /// slot's color space ([`Self::srgb`]). Wins over every other field while the asset is loaded;
    /// until then the slot draws the rest (white when empty).
    pub asset: Option<crate::AssetImage>,
}

impl GraphTexture {
    /// An RGBA8 slot, as the graph builder has always bound one.
    #[must_use]
    pub fn rgba(image: LabTexture, srgb: bool) -> Self {
        Self {
            image,
            srgb,
            compressed: None,
            shared: None,
            dynamic: None,
            asset: None,
        }
    }

    /// A counted texture asset sampled in color space `srgb` (ADR-8610): every material holding
    /// the same [`crate::AssetImage`] binds its one GPU texture, and no CPU pixels live here.
    #[must_use]
    pub fn from_asset(image: crate::AssetImage, srgb: bool) -> Self {
        Self {
            asset: Some(image),
            ..Self::rgba(
                LabTexture {
                    width: 0,
                    height: 0,
                    rgba: Vec::new(),
                },
                srgb,
            )
        }
    }

    /// A shared image. The RGBA8 copy stays inside the [`SharedImage`](crate::SharedImage);
    /// this slot does not clone those pixels.
    #[must_use]
    pub fn from_shared(image: crate::SharedImage) -> Self {
        let (width, height) = image.size();
        let srgb = image.is_srgb();
        let compressed = image.compressed();
        Self {
            image: LabTexture {
                width,
                height,
                rgba: Vec::new(),
            },
            srgb,
            compressed,
            shared: Some(image),
            dynamic: None,
            asset: None,
        }
    }

    /// A dynamic texture the host writes in place.
    #[must_use]
    pub fn from_dynamic(texture: crate::DynamicTexture) -> Self {
        let (width, height) = texture.size();
        let srgb = texture.is_srgb();
        Self {
            image: LabTexture {
                width,
                height,
                rgba: Vec::new(),
            },
            srgb,
            compressed: None,
            shared: None,
            dynamic: Some(texture),
            asset: None,
        }
    }
}

impl Default for GraphTexture {
    /// An empty linear slot. `..Default::default()` fills the fields a host's older struct
    /// literal does not name. [`GraphTexture::rgba`] is the constructor the graph builder uses.
    fn default() -> Self {
        Self::rgba(
            LabTexture {
                width: 0,
                height: 0,
                rgba: Vec::new(),
            },
            false,
        )
    }
}

/// The code of a compiled graph: what selects its pipelines.
///
/// Everything here enters [`GraphCode::key`]; two materials with equal code share one pipeline
/// set.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphCode {
    /// The generated `material_surface` function (no preprocessor directives).
    pub surface: Arc<str>,
    /// The generated `material_vertex_offset` function, when the graph offsets positions.
    pub vertex: Option<Arc<str>>,
    /// The graph reads the second UV set (`MATERIAL_GRAPH_UV1`, a second vertex stream).
    pub uv1: bool,
}

impl GraphCode {
    /// Surface-only code.
    #[must_use]
    pub fn surface(surface: impl Into<Arc<str>>) -> Self {
        Self {
            surface: surface.into(),
            vertex: None,
            uv1: false,
        }
    }

    /// Pipeline cache key. A surface-only graph's key is [`source_key`] of its surface text
    /// (unchanged from the first version of the contract).
    #[must_use]
    pub fn key(&self) -> u64 {
        if self.vertex.is_none() && !self.uv1 {
            return source_key(&self.surface);
        }
        let mut text = String::with_capacity(self.surface.len() + 64);
        text.push_str(&self.surface);
        text.push_str("\n// vertex stage\n");
        if let Some(vertex) = &self.vertex {
            text.push_str(vertex);
        }
        text.push_str(if self.uv1 { "\n// uv1\n" } else { "\n" });
        source_key(&text)
    }
}

/// A compiled surface graph as the renderer consumes it.
///
/// `key` identifies the generated code: two materials with the same key share one pipeline
/// set. Parameter values and textures are per material and do not change the key.
#[derive(Clone, Debug)]
pub struct SurfaceGraph {
    /// Pipeline cache key: [`GraphCode::key`] of the code fields.
    pub key: u64,
    /// The generated `material_surface` function (no preprocessor directives).
    pub wgsl: Arc<str>,
    /// The generated `material_vertex_offset` function (world position offset), if any.
    pub vertex_wgsl: Option<Arc<str>>,
    /// The graph reads the second UV set.
    pub uses_uv1: bool,
    /// The graph reads the time slot (`params[0].x`; `params[0].y` is the previous frame's
    /// time for motion vectors). With a material clock
    /// ([`crate::FrameRenderer::set_material_time`]) or a scene wind
    /// ([`crate::FrameRenderer::set_foliage_wind`]) the forward, shadow and velocity shaders
    /// read that one frame clock, and the renderer does not rewrite the block for slot 0 alone
    /// (ADR-5160). Wind is metres per second on world X and Z (ADR-2028). An unset clock and
    /// wind leave slot 0 as the material carries it.
    pub uses_time: bool,
    /// Largest world position offset in metres: culling bounds grow by this much.
    pub max_vertex_offset: f32,
    /// The parameter block, slot 0 = built-ins.
    pub params: [[f32; 4]; GRAPH_PARAM_SLOTS],
    /// Textures by slot; an empty slot binds white.
    pub textures: [Option<GraphTexture>; GRAPH_TEXTURE_SLOTS],
}

impl SurfaceGraph {
    /// A surface-only graph with zeroed parameters and no textures.
    #[must_use]
    pub fn new(wgsl: impl Into<Arc<str>>) -> Self {
        Self::from_code(&GraphCode::surface(wgsl))
    }

    /// A graph for `code` with zeroed parameters and no textures.
    #[must_use]
    pub fn from_code(code: &GraphCode) -> Self {
        Self {
            key: code.key(),
            wgsl: code.surface.clone(),
            vertex_wgsl: code.vertex.clone(),
            uses_uv1: code.uv1,
            uses_time: false,
            max_vertex_offset: 0.0,
            params: [[0.0; 4]; GRAPH_PARAM_SLOTS],
            textures: Default::default(),
        }
    }

    /// The code fields.
    #[must_use]
    pub fn code(&self) -> GraphCode {
        GraphCode {
            surface: self.wgsl.clone(),
            vertex: self.vertex_wgsl.clone(),
            uv1: self.uses_uv1,
        }
    }
}

/// FNV-1a 64 of a WGSL source: the pipeline cache key of a surface graph.
#[must_use]
pub fn source_key(wgsl: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in wgsl.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------------------------
// The floating origin (spec 78 § 12, ADR-2435)
// ---------------------------------------------------------------------------------------------

/// The generated surface function's opening, renamed to [`GRAPH_WORLD_SURFACE_FN`] when the graph
/// reads a position.
const SURFACE_FN: &str = "fn material_surface(";

/// The generated vertex stage's opening, renamed to [`GRAPH_WORLD_VERTEX_FN`] likewise.
const VERTEX_FN: &str = "fn material_vertex_offset(";

/// The name a position-reading graph's own surface function takes in the composed module: the
/// adapter `material_surface` calls it with world positions.
pub const GRAPH_WORLD_SURFACE_FN: &str = "material_surface_world";

/// The name a position-reading graph's own vertex stage takes in the composed module.
pub const GRAPH_WORLD_VERTEX_FN: &str = "material_vertex_offset_world";

/// The graph parameter slots `text` reads (`material.graph[N]`), as a bit set.
fn read_slots(text: &str) -> u32 {
    const NEEDLE: &str = "material.graph[";
    let mut slots = 0u32;
    for (at, _) in text.match_indices(NEEDLE) {
        let digits: String = text[at + NEEDLE.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        match digits.parse::<u32>() {
            Ok(slot) if slot < 32 => slots |= 1 << slot,
            // A computed index could read any slot: none is free.
            _ => return u32::MAX,
        }
    }
    slots
}

/// The stage reads a world position or the object's position.
fn reads_position(text: &str) -> bool {
    text.contains(".world_position") || text.contains(".object_position")
}

/// Which stages of `code` take the origin, and in which slot (computed once per code key).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OriginUse {
    slot: u8,
    surface: bool,
    vertex: bool,
}

fn origin_use_uncached(code: &GraphCode) -> Option<OriginUse> {
    let surface = reads_position(&code.surface) && code.surface.matches(SURFACE_FN).count() == 1;
    let vertex = code
        .vertex
        .as_deref()
        .is_some_and(|v| reads_position(v) && v.matches(VERTEX_FN).count() == 1);
    if !surface && !vertex {
        return None;
    }
    let read = read_slots(&code.surface) | code.vertex.as_deref().map_or(0, read_slots);
    // The highest slot the code never reads: parameters fill slots from 1 upward, so this is
    // almost always one no parameter holds, and the renderer overwrites it anyway.
    let slot = (1..GRAPH_PARAM_SLOTS)
        .rev()
        .find(|slot| read & (1 << slot) == 0)?;
    Some(OriginUse {
        slot: u8::try_from(slot).ok()?,
        surface,
        vertex,
    })
}

/// [`origin_use_uncached`] by [`GraphCode::key`]: the renderer asks once per graph material per
/// frame, and a scan of the generated text costs microseconds.
fn origin_use(key: u64, code: impl FnOnce() -> GraphCode) -> Option<OriginUse> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<u64, Option<OriginUse>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock()
        && let Some(found) = map.get(&key)
    {
        return *found;
    }
    let found = origin_use_uncached(&code());
    if let Ok(mut map) = cache.lock() {
        // Graph codes are few; a runaway editor session starts over instead of growing.
        if map.len() >= 4096 {
            map.clear();
        }
        map.insert(key, found);
    }
    found
}

/// The parameter slot a graph's composed module reads the world origin from: the world position
/// of the frame's local origin (`FrameRenderer::origin_offset`), which the adapter adds to
/// `world_position` and `object_position` before the graph sees them, so a pattern in world
/// space stays on the same world point across a floating-origin rebase (spec 78 § 12, ADR-2435).
///
/// `None` when the graph reads neither position (its composed module is unchanged). Hand-written
/// code that reads every slot also returns `None` and then sees local positions. `regia_matgraph`
/// refuses to compile that case (ADR-5005). The slot is the highest one the generated code does
/// not read, so it is a function of the code and [`GraphCode::key`] covers it.
#[must_use]
pub fn graph_origin_slot(code: &GraphCode) -> Option<usize> {
    origin_use(code.key(), || code.clone()).map(|u| usize::from(u.slot))
}

impl SurfaceGraph {
    /// [`graph_origin_slot`] of this graph's code (cached by [`Self::key`]).
    #[must_use]
    pub fn origin_slot(&self) -> Option<usize> {
        origin_use(self.key, || self.code()).map(|u| usize::from(u.slot))
    }

    /// Writes `origin` (the world position of the local origin, meters) into this graph's
    /// [`Self::origin_slot`] of `block`, the parameter block the renderer uploads. Nothing for a
    /// graph without an origin slot. Writes zeros at the origin, so a block at origin zero shades
    /// as it did before ADR-2435.
    pub fn write_origin(&self, block: &mut [[f32; 4]; GRAPH_PARAM_SLOTS], origin: [f32; 3]) {
        if let Some(slot) = self.origin_slot() {
            block[slot] = [origin[0], origin[1], origin[2], 0.0];
        }
    }
}

/// The graph's surface text as spliced: renamed, with the adapter that adds the origin.
fn spliced_surface(code: &GraphCode, origin: Option<OriginUse>) -> String {
    match origin {
        Some(u) if u.surface => {
            let renamed = code
                .surface
                .replacen(SURFACE_FN, &format!("fn {GRAPH_WORLD_SURFACE_FN}("), 1);
            format!(
                "{renamed}
// The floating origin (ADR-2435): the graph sees world positions.
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {{
    let graph_origin = material.graph[{slot}].xyz;
    return {GRAPH_WORLD_SURFACE_FN}(regia::pbr::surface::SurfaceInput(
        in.world_position + graph_origin,
        in.world_normal,
        in.uv,
        in.view_dir,
        in.vertex_color,
        in.world_tangent,
        in.camera_distance,
        in.second_uv,
        in.object_position + graph_origin,
        in.facing,
    ));
}}
",
                slot = u.slot
            )
        }
        _ => code.surface.to_string(),
    }
}

/// The surface stage of `code` as every host splices it: the generated text, renamed behind the
/// floating-origin adapter when the graph reads a position (ADR-2435), so the adapter reads the
/// origin from [`graph_origin_slot`]. The reference path tracer composes this text into its
/// kernels (`regia_pt`, ADR-2725).
///
/// # Errors
///
/// [`RenderError::Shader`] for a preprocessor directive in the generated source.
pub fn surface_source(code: &GraphCode) -> Result<String, RenderError> {
    check_directives(&code.surface)?;
    Ok(spliced_surface(code, origin_use(code.key(), || code.clone())))
}

/// The graph's vertex stage as spliced (see [`spliced_surface`]).
fn spliced_vertex(vertex: &str, origin: Option<OriginUse>) -> String {
    match origin {
        Some(u) if u.vertex => {
            let renamed = vertex.replacen(VERTEX_FN, &format!("fn {GRAPH_WORLD_VERTEX_FN}("), 1);
            format!(
                "{renamed}
// The floating origin (ADR-2435): the graph sees world positions; the offset is a vector.
fn material_vertex_offset(in: regia::pbr::surface::VertexOffsetInput) -> vec3<f32> {{
    let graph_origin = material.graph[{slot}].xyz;
    return {GRAPH_WORLD_VERTEX_FN}(regia::pbr::surface::VertexOffsetInput(
        in.world_position + graph_origin,
        in.world_normal,
        in.uv,
        in.second_uv,
        in.vertex_color,
        in.object_position + graph_origin,
        in.time,
    ));
}}
",
                slot = u.slot
            )
        }
        _ => vertex.to_string(),
    }
}

/// Host composition only: a swizzle of `material.graph[0]` becomes the same swizzle of
/// `graph_clock()`. The path tracer composes [`surface_source`], which does not call this.
fn rewrite_graph_clock(source: &str) -> String {
    const NEEDLE: &str = "material.graph[0].";
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(at) = rest.find(NEEDLE) {
        out.push_str(&rest[..at]);
        rest = &rest[at + NEEDLE.len()..];
        let swizzle_len = rest
            .chars()
            .take_while(|c| matches!(c, 'x' | 'y' | 'z' | 'w'))
            .count();
        if swizzle_len == 0 {
            out.push_str(NEEDLE);
            continue;
        }
        out.push_str("graph_clock().");
        out.push_str(&rest[..swizzle_len]);
        rest = &rest[swizzle_len..];
    }
    out.push_str(rest);
    out
}

fn check_directives(text: &str) -> Result<(), RenderError> {
    if text.lines().any(|line| line.trim_start().starts_with('#')) {
        return Err(RenderError::Shader(
            "material graph source must not contain preprocessor directives".into(),
        ));
    }
    Ok(())
}

/// The forward shader with `surface_wgsl` spliced in at [`GRAPH_SOURCE_MARKER`].
///
/// # Errors
///
/// [`RenderError::Shader`] when the generated source contains a preprocessor directive (`#`
/// at the start of a line), which would be interpreted by the shader composer.
pub fn forward_source_with_graph(surface_wgsl: &str) -> Result<String, RenderError> {
    forward_source_with_code(&GraphCode::surface(surface_wgsl))
}

/// The forward shader with both stages of `code` spliced in.
///
/// # Errors
///
/// As [`forward_source_with_graph`].
pub fn forward_source_with_code(code: &GraphCode) -> Result<String, RenderError> {
    check_directives(&code.surface)?;
    let origin = origin_use(code.key(), || code.clone());
    let source = include_str!("../shaders/lab_forward.wgsl").replacen(
        GRAPH_SOURCE_MARKER,
        &rewrite_graph_clock(&spliced_surface(code, origin)),
        1,
    );
    match &code.vertex {
        Some(vertex) => {
            check_directives(vertex)?;
            Ok(source.replacen(
                GRAPH_VERTEX_MARKER,
                &rewrite_graph_clock(&spliced_vertex(vertex, origin)),
                1,
            ))
        }
        None => Ok(source),
    }
}

/// The shadow (`lab_shadow.wgsl`) or velocity (`velocity.wgsl`) shader with the vertex stage of
/// `code` spliced in. `None` when the graph has no vertex stage (those passes then use their
/// standard pipelines).
///
/// # Errors
///
/// [`RenderError::Shader`] for a preprocessor directive in the generated source.
pub fn vertex_host_source(code: &GraphCode, velocity: bool) -> Result<Option<String>, RenderError> {
    let Some(vertex) = &code.vertex else {
        return Ok(None);
    };
    check_directives(vertex)?;
    let host = if velocity {
        include_str!("../shaders/velocity.wgsl")
    } else {
        include_str!("../shaders/lab_shadow.wgsl")
    };
    let origin = origin_use(code.key(), || code.clone());
    Ok(Some(host.replacen(
        GRAPH_VERTEX_MARKER,
        &rewrite_graph_clock(&spliced_vertex(vertex, origin)),
        1,
    )))
}

/// Shader defines of a graph forward module (`MATERIAL_GRAPH` plus `extra`).
pub(crate) fn graph_defs(extra: &[&str]) -> HashMap<String, ShaderDefValue> {
    let mut defs = HashMap::new();
    defs.insert("MATERIAL_GRAPH".to_string(), ShaderDefValue::Bool(true));
    for name in extra {
        defs.insert((*name).to_string(), ShaderDefValue::Bool(true));
    }
    defs
}

/// Defines of a graph module for `code`: [`graph_defs`] plus the stage flags.
pub(crate) fn code_defs(code: &GraphCode, extra: &[&str]) -> HashMap<String, ShaderDefValue> {
    let mut defs = graph_defs(extra);
    if code.vertex.is_some() {
        defs.insert("MATERIAL_GRAPH_VERTEX".to_string(), ShaderDefValue::Bool(true));
    }
    if code.uv1 {
        defs.insert("MATERIAL_GRAPH_UV1".to_string(), ShaderDefValue::Bool(true));
    }
    defs
}

/// Defines of a graph shadow/velocity module (the vertex stage only).
pub(crate) fn vertex_host_defs(code: &GraphCode, extra: &[&str]) -> HashMap<String, ShaderDefValue> {
    let mut defs = HashMap::new();
    defs.insert("MATERIAL_GRAPH_VERTEX".to_string(), ShaderDefValue::Bool(true));
    if code.uv1 {
        defs.insert("MATERIAL_GRAPH_UV1".to_string(), ShaderDefValue::Bool(true));
    }
    for name in extra {
        defs.insert((*name).to_string(), ShaderDefValue::Bool(true));
    }
    defs
}

fn validate(module: &naga::Module, what: &str) -> Result<(), RenderError> {
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(module)
    .map_err(|err| RenderError::Shader(format!("material graph ({what}): {err}")))?;
    Ok(())
}

/// Composes and validates the forward shader with a generated surface function: the same
/// module the renderer builds for a graph material's opaque/mask pipelines.
///
/// # Errors
///
/// [`RenderError::Shader`] with the composer's or naga's message.
pub fn compose_surface_graph(surface_wgsl: &str) -> Result<naga::Module, RenderError> {
    let code = GraphCode::surface(surface_wgsl);
    let source = forward_source_with_code(&code)?;
    let module = crate::shader_lib::compose_source(
        &source,
        "lab_forward_graph.wgsl",
        code_defs(&code, &["ALPHA_TEST"]),
    )?;
    validate(&module, "forward")?;
    Ok(module)
}

/// Composes and validates every module a graph's pipelines are built from: the forward module
/// (CPU-object and instanced GPU-driven variants), and with a vertex stage the shadow (both
/// variants) and velocity modules. This is the check `regia_matgraph::compile` and the cook
/// run, so a shipped graph never meets the shader composer's errors at run time.
///
/// # Errors
///
/// [`RenderError::Shader`] naming the failing module.
pub fn validate_graph_code(code: &GraphCode) -> Result<(), RenderError> {
    let forward = forward_source_with_code(code)?;
    for (extra, what) in [
        (&["ALPHA_TEST"][..], "forward"),
        (
            &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST"][..],
            "forward, instanced",
        ),
        (&["GPU_DRIVEN", "ALPHA_TEST"][..], "depth, instanced"),
    ] {
        let module =
            crate::shader_lib::compose_source(&forward, "lab_forward_graph.wgsl", code_defs(code, extra))?;
        validate(&module, what)?;
    }
    if let Some(shadow) = vertex_host_source(code, false)? {
        for (extra, what) in [(&[][..], "shadow"), (&["GPU_DRIVEN"][..], "shadow, instanced")] {
            let module = crate::shader_lib::compose_source(
                &shadow,
                "lab_shadow_graph.wgsl",
                vertex_host_defs(code, extra),
            )?;
            validate(&module, what)?;
        }
    }
    if let Some(velocity) = vertex_host_source(code, true)? {
        let module =
            crate::shader_lib::compose_source(&velocity, "velocity_graph.wgsl", vertex_host_defs(code, &[]))?;
        validate(&module, "velocity")?;
    }
    Ok(())
}

/// A minimal generated-style surface (tint × albedo × vertex colour), used by the shader
/// validation gate to cover the `MATERIAL_GRAPH` forward module.
pub(crate) const SAMPLE_SURFACE: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    let tint = material.graph[1].xyz;
    let albedo = textureSample(albedo_tex, tex_samp, in.uv);
    return regia::pbr::surface::Surface(
        albedo.rgb * tint * in.vertex_color.rgb,
        albedo.a,
        0.0,
        0.5,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        0.0,
    );
}
";

/// A generated-style vertex stage (a time-driven sway scaled by a parameter and a level-0
/// texture fetch), for the composition tests and the shader validation gate.
pub(crate) const SAMPLE_VERTEX: &str = r"
fn material_vertex_offset(in: regia::pbr::surface::VertexOffsetInput) -> vec3<f32> {
    let mask = textureSampleLevel(aux_tex, tex_samp, in.second_uv, 0.0).r;
    let sway = sin(in.time + in.object_position.x) * material.graph[2].x * mask;
    return in.world_normal * sway * in.vertex_color.r;
}
";

/// Shader statistics of a graph's colour pass on one target (w12, ADR-7851): what the material
/// graph editor's Stats strip and `editor.matgraph_cost` show per render profile. Counted on the
/// composed naga IR, so the numbers are the same on every device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphShaderStats {
    /// IR expressions plus statements (without `Emit`) of the fragment entry `fs_main` and of
    /// every function it calls, each function once.
    pub instructions: u32,
    /// The same count for the generated graph code alone (`material_surface`,
    /// `material_vertex_offset` and the helpers they call).
    pub graph_instructions: u32,
    /// Texture sample expressions reachable from the fragment entry.
    pub texture_samples: u32,
    /// Of those, the ones in the generated graph code.
    pub graph_texture_samples: u32,
    /// Bound resources the fragment entry reaches (textures, samplers, buffers).
    pub bindings: u32,
    /// Of those, samplers.
    pub samplers: u32,
}

/// The forward module a graph's opaque colour pass compiles on a target: the GPU-driven
/// instanced variant when `instanced`, else the CPU-object module; linear output, no traced sun
/// (the defines `graph_pipelines::build_set` uses).
///
/// # Errors
///
/// [`RenderError::Shader`] with the composer's message.
pub fn compose_graph_forward(code: &GraphCode, instanced: bool) -> Result<naga::Module, RenderError> {
    let forward = forward_source_with_code(code)?;
    let extra: &[&str] = if instanced {
        &["GPU_DRIVEN", "GPU_INSTANCED", "skip_srgb_encode"]
    } else {
        &["ALPHA_TEST", "skip_srgb_encode"]
    };
    crate::shader_lib::compose_source(&forward, "lab_forward_graph.wgsl", code_defs(code, extra))
}

/// Counts [`GraphShaderStats`] of `code`'s colour pass ([`compose_graph_forward`]).
///
/// # Errors
///
/// [`RenderError::Shader`] when the module does not compose or has no `fs_main`.
pub fn graph_shader_stats(code: &GraphCode, instanced: bool) -> Result<GraphShaderStats, RenderError> {
    let module = compose_graph_forward(code, instanced)?;
    let entry = module
        .entry_points
        .iter()
        .find(|e| e.stage == naga::ShaderStage::Fragment && e.name == "fs_main")
        .ok_or_else(|| RenderError::Shader("the graph forward module has no fs_main".into()))?;
    let all = ir_tally(&module, Some(&entry.function), &[]);
    let graph_roots: Vec<naga::Handle<naga::Function>> = module
        .functions
        .iter()
        .filter(|(_, f)| {
            f.name
                .as_deref()
                .is_some_and(|n| n.contains("material_surface") || n.contains("material_vertex_offset"))
        })
        .map(|(h, _)| h)
        .collect();
    let graph = ir_tally(&module, None, &graph_roots);
    let mut bindings = 0;
    let mut samplers = 0;
    for handle in &all.globals {
        let global = &module.global_variables[*handle];
        if global.binding.is_some() {
            bindings += 1;
            if matches!(module.types[global.ty].inner, naga::TypeInner::Sampler { .. }) {
                samplers += 1;
            }
        }
    }
    Ok(GraphShaderStats {
        instructions: all.instructions,
        graph_instructions: graph.instructions,
        texture_samples: all.samples,
        graph_texture_samples: graph.samples,
        bindings,
        samplers,
    })
}

/// What [`ir_tally`] counted.
#[derive(Default)]
struct IrTally {
    instructions: u32,
    samples: u32,
    globals: std::collections::BTreeSet<naga::Handle<naga::GlobalVariable>>,
}

/// Counts `entry` (when given) and the functions `roots`, then every function they call
/// (each function once).
fn ir_tally(
    module: &naga::Module,
    entry: Option<&naga::Function>,
    roots: &[naga::Handle<naga::Function>],
) -> IrTally {
    let mut tally = IrTally::default();
    let mut queue: Vec<naga::Handle<naga::Function>> = roots.to_vec();
    let mut seen = std::collections::BTreeSet::new();
    if let Some(entry) = entry {
        tally_function(entry, &mut tally, &mut queue);
    }
    while let Some(handle) = queue.pop() {
        if seen.insert(handle) {
            tally_function(&module.functions[handle], &mut tally, &mut queue);
        }
    }
    tally
}

fn tally_function(
    function: &naga::Function,
    tally: &mut IrTally,
    calls: &mut Vec<naga::Handle<naga::Function>>,
) {
    let expressions = u32::try_from(function.expressions.len()).unwrap_or(u32::MAX);
    tally.instructions = tally
        .instructions
        .saturating_add(expressions)
        .saturating_add(count_block(&function.body, calls));
    for (_, expression) in function.expressions.iter() {
        match expression {
            naga::Expression::ImageSample { .. } => tally.samples += 1,
            naga::Expression::GlobalVariable(handle) => {
                tally.globals.insert(*handle);
            }
            _ => {}
        }
    }
}

/// Statements of `block` and its nested blocks, without `Emit` (it only scopes expressions);
/// the functions it calls go to `calls`.
fn count_block(block: &naga::Block, calls: &mut Vec<naga::Handle<naga::Function>>) -> u32 {
    let mut n = 0u32;
    for statement in block.iter() {
        match statement {
            naga::Statement::Emit(_) => continue,
            naga::Statement::Block(inner) => n += count_block(inner, calls),
            naga::Statement::If { accept, reject, .. } => {
                n += count_block(accept, calls) + count_block(reject, calls);
            }
            naga::Statement::Switch { cases, .. } => {
                for case in cases {
                    n += count_block(&case.body, calls);
                }
            }
            naga::Statement::Loop { body, continuing, .. } => {
                n += count_block(body, calls) + count_block(continuing, calls);
            }
            naga::Statement::Call { function, .. } => calls.push(*function),
            _ => {}
        }
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_shader_stats_count_the_graph_and_the_whole_pass() {
        let plain = graph_shader_stats(&GraphCode::surface(SAMPLE_SURFACE), false).expect("stats");
        assert_eq!(plain.graph_texture_samples, 1, "{plain:?}");
        assert!(plain.graph_instructions > 0, "{plain:?}");
        assert!(plain.instructions > plain.graph_instructions, "{plain:?}");
        assert!(plain.texture_samples >= plain.graph_texture_samples, "{plain:?}");
        assert!(
            plain.samplers >= 1 && plain.bindings > plain.samplers,
            "{plain:?}"
        );
        // Deterministic, and the instanced variant composes too.
        assert_eq!(
            graph_shader_stats(&GraphCode::surface(SAMPLE_SURFACE), false).expect("again"),
            plain
        );
        let instanced = graph_shader_stats(&GraphCode::surface(SAMPLE_SURFACE), true).expect("instanced");
        assert_eq!(instanced.graph_instructions, plain.graph_instructions);
        // A second sample in the graph shows in both counts.
        let two = SAMPLE_SURFACE.replace(
            "let tint = material.graph[1].xyz;",
            "let tint = material.graph[1].xyz * textureSample(albedo_tex, tex_samp, in.uv * 2.0).rgb;",
        );
        let more = graph_shader_stats(&GraphCode::surface(two.as_str()), false).expect("two samples");
        assert_eq!(more.graph_texture_samples, 2);
        assert_eq!(more.texture_samples, plain.texture_samples + 1);
        assert!(more.graph_instructions > plain.graph_instructions);
    }

    #[test]
    fn a_constant_surface_composes_and_validates() {
        compose_surface_graph(SAMPLE_SURFACE).expect("constant surface graph");
    }

    #[test]
    fn the_marker_is_present_once() {
        let source = include_str!("../shaders/lab_forward.wgsl");
        assert_eq!(source.matches(GRAPH_SOURCE_MARKER).count(), 1);
    }

    #[test]
    fn directives_in_generated_code_are_rejected() {
        let err = forward_source_with_graph("#define X\nfn material_surface() {}").expect_err("should fail");
        assert!(err.to_string().contains("preprocessor"), "{err}");
    }

    #[test]
    fn a_broken_surface_reports_a_shader_error() {
        let err = compose_surface_graph("fn material_surface(in: f32) -> f32 { return in; }")
            .expect_err("should fail");
        assert!(matches!(err, RenderError::Shader(_)), "{err}");
    }

    #[test]
    fn the_key_follows_the_source() {
        assert_eq!(source_key("a"), source_key("a"));
        assert_ne!(source_key("a"), source_key("b"));
        assert_eq!(SurfaceGraph::new("a").key, source_key("a"));
        let mut code = GraphCode::surface("a");
        code.uv1 = true;
        assert_ne!(code.key(), source_key("a"));
        let uv1_key = code.key();
        code.vertex = Some("b".into());
        assert_ne!(code.key(), uv1_key);
    }

    #[test]
    fn the_vertex_markers_are_present_once_per_host() {
        for host in [
            include_str!("../shaders/lab_forward.wgsl"),
            include_str!("../shaders/lab_shadow.wgsl"),
            include_str!("../shaders/velocity.wgsl"),
        ] {
            assert_eq!(host.matches(GRAPH_VERTEX_MARKER).count(), 1);
        }
    }

    #[test]
    fn a_vertex_stage_and_uv1_compose_in_every_host() {
        let code = GraphCode {
            surface: SAMPLE_SURFACE.replace("in.uv)", "in.second_uv)").into(),
            vertex: Some(SAMPLE_VERTEX.into()),
            uv1: true,
        };
        validate_graph_code(&code).expect("vertex stage + uv1 in forward, shadow and velocity");
        validate_graph_code(&GraphCode::surface(SAMPLE_SURFACE)).expect("surface only");
    }

    /// A generated-style surface that reads the world position (a world-space checker) and the
    /// object position, with two parameters.
    const WORLD_SURFACE: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    let cell = floor(in.world_position * material.graph[1].x);
    let check = fract((cell.x + cell.z) * 0.5) * 2.0;
    let tint = material.graph[2].xyz * (0.5 + 0.5 * fract(in.object_position.x));
    return regia::pbr::surface::Surface(
        tint * check,
        1.0,
        0.0,
        0.5,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        0.0,
    );
}
";

    /// A vertex stage whose sway phase is the world position.
    const WORLD_VERTEX: &str = r"
fn material_vertex_offset(in: regia::pbr::surface::VertexOffsetInput) -> vec3<f32> {
    return in.world_normal * sin(in.time + in.world_position.x + in.object_position.z) * material.graph[3].x;
}
";

    #[test]
    fn read_slots_finds_every_literal_index_and_gives_up_on_a_computed_one() {
        assert_eq!(
            read_slots("material.graph[0].x + material.graph[12].xyz"),
            (1 << 0) | (1 << 12)
        );
        assert_eq!(read_slots("no graph here"), 0);
        assert_eq!(read_slots("material.graph[i].x"), u32::MAX);
    }

    /// Time reads in a spliced graph become `graph_clock()`; a later slot stays on the material.
    #[test]
    fn time_reads_move_to_the_frame_clock() {
        let code = GraphCode {
            surface: r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    let t = material.graph[0].x + material.graph[0].zw.x;
    let p = material.graph[3].xyz;
    return regia::pbr::surface::surface_default(in, p * t);
}
"
            .into(),
            vertex: Some(
                r"
fn material_vertex_offset(in: regia::pbr::surface::VertexOffsetInput) -> vec3<f32> {
    return in.world_normal * material.graph[0].x + vec3<f32>(material.graph[0].y);
}
"
                .into(),
            ),
            uv1: false,
        };
        let forward = forward_source_with_code(&code).expect("forward");
        assert!(
            forward.contains("fn graph_clock()"),
            "the forward host defines the clock"
        );
        assert!(forward.contains("graph_clock().x"));
        assert!(forward.contains("graph_clock().zw"));
        assert!(forward.contains("graph_clock().y"));
        assert!(forward.contains("material.graph[3].xyz"));
        assert!(
            !forward.contains("material.graph[0]."),
            "a slot-0 swizzle stayed on the material"
        );
        let shadow = vertex_host_source(&code, false)
            .expect("shadow")
            .expect("vertex stage");
        assert!(shadow.contains("fn graph_clock()"));
        assert!(shadow.contains("graph_clock().x"));
        assert!(!shadow.contains("material.graph[0]."));
        let velocity = vertex_host_source(&code, true)
            .expect("velocity")
            .expect("vertex stage");
        assert!(velocity.contains("fn graph_clock()"));
        assert!(velocity.contains("graph_clock().y"));
        assert!(!velocity.contains("material.graph[0]."));
    }

    #[test]
    fn the_origin_slot_is_the_highest_slot_the_code_does_not_read() {
        let surface = GraphCode::surface(WORLD_SURFACE);
        assert_eq!(graph_origin_slot(&surface), Some(GRAPH_PARAM_SLOTS - 1));
        // The foliage sway's slots (14 and 15) are read: the origin moves below them.
        let both = GraphCode {
            surface: WORLD_SURFACE
                .replace("graph[2]", "graph[15]")
                .replace("graph[1]", "graph[14]")
                .into(),
            vertex: Some(WORLD_VERTEX.into()),
            uv1: false,
        };
        assert_eq!(graph_origin_slot(&both), Some(13));
        // No position read: no slot, and the module is the one it always was.
        assert_eq!(graph_origin_slot(&GraphCode::surface(SAMPLE_SURFACE)), None);
        let plain =
            include_str!("../shaders/lab_forward.wgsl").replacen(GRAPH_SOURCE_MARKER, SAMPLE_SURFACE, 1);
        assert_eq!(forward_source_with_graph(SAMPLE_SURFACE).expect("source"), plain);
        // Every slot read: no slot is free. Hand-written code then sees local positions.
        // regia_matgraph refuses to compile that graph.
        let all = (1..GRAPH_PARAM_SLOTS)
            .map(|s| format!("material.graph[{s}].x + "))
            .collect::<Vec<_>>()
            .concat();
        let full = WORLD_SURFACE.replace("material.graph[1].x", &format!("({all}0.0)"));
        assert_eq!(graph_origin_slot(&GraphCode::surface(full)), None);
    }

    #[test]
    fn a_position_reading_graph_takes_world_positions_in_every_host() {
        let code = GraphCode {
            surface: WORLD_SURFACE.into(),
            vertex: Some(WORLD_VERTEX.into()),
            uv1: false,
        };
        validate_graph_code(&code).expect("adapter in forward, shadow and velocity");
        let slot = graph_origin_slot(&code).expect("slot");
        let forward = forward_source_with_code(&code).expect("forward");
        assert!(
            forward.contains(&format!("fn {GRAPH_WORLD_SURFACE_FN}(")),
            "renamed surface"
        );
        assert!(
            forward.contains(&format!("fn {GRAPH_WORLD_VERTEX_FN}(")),
            "renamed vertex stage"
        );
        assert!(forward.contains(&format!("let graph_origin = material.graph[{slot}].xyz;")));
        assert!(forward.contains("in.world_position + graph_origin"));
        for velocity in [false, true] {
            let host = vertex_host_source(&code, velocity)
                .expect("host")
                .expect("vertex stage");
            assert!(host.contains(&format!("fn {GRAPH_WORLD_VERTEX_FN}(")));
        }
        // The pipeline key is the code's: the adapter is a function of it.
        assert_eq!(SurfaceGraph::from_code(&code).key, code.key());
    }

    /// The adapter's sum, evaluated on the CPU as the shader does (`f32`): a world point shades
    /// with the same input before and after a 10 km rebase. Whole cells are exact in `f32`, so
    /// the sums agree bit for bit.
    #[test]
    fn the_same_world_point_reads_the_same_position_after_a_ten_kilometre_rebase() {
        let graph = SurfaceGraph::from_code(&GraphCode::surface(WORLD_SURFACE));
        let slot = graph.origin_slot().expect("slot");
        let adapter = |local: [f32; 3], block: &[[f32; 4]; GRAPH_PARAM_SLOTS]| -> [f32; 3] {
            std::array::from_fn(|a| local[a] + block[slot][a])
        };
        let world = [10_000.37f32, 2.5, -4_999.81];
        // Before: origin zero, the local position is the world one.
        let mut before = graph.params;
        graph.write_origin(&mut before, [0.0; 3]);
        assert_eq!(before[slot], [0.0; 4], "origin zero writes zeros");
        let seen_before = adapter(world, &before);
        // A rebase by whole 1024 m cells (10 240 m, 0, -5 120 m): transforms move by -shift.
        let shift = [10_240.0f32, 0.0, -5_120.0];
        let local: [f32; 3] = std::array::from_fn(|a| world[a] - shift[a]);
        let mut after = graph.params;
        graph.write_origin(&mut after, shift);
        let seen_after = adapter(local, &after);
        assert_eq!(seen_before, seen_after);
        assert_eq!(seen_before, world);
        // Without the origin the pattern would read a point 10 km away.
        assert_ne!(adapter(local, &before), world);
    }

    /// ADR-347: a graph's forward module has the transmit multiply's entry point (the graph set's
    /// `transmit_mul`), and that `fs_transmit` evaluates the graph's surface function for its
    /// base colour instead of the standard block's colour, which a graph material leaves at the
    /// default grey (graph glass lost its hue).
    #[test]
    fn the_graph_transmit_entry_takes_the_graph_surface() {
        let module = compose_surface_graph(SAMPLE_SURFACE).expect("graph module");
        let entry = module
            .entry_points
            .iter()
            .find(|entry| entry.name == "fs_transmit")
            .expect("fs_transmit in the graph module");
        let calls_surface = entry.function.body.iter().any(|statement| match statement {
            naga::Statement::Call { function, .. } => module.functions[*function]
                .name
                .as_deref()
                .is_some_and(|name| name.contains("material_surface")),
            _ => false,
        });
        assert!(calls_surface, "fs_transmit does not evaluate the graph surface");
    }
}
