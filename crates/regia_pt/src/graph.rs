// SPDX-License-Identifier: MPL-2.0
//! Material graphs in the reference path tracer (`specs/27` § 4.2 stage 5, ADR-2725).
//!
//! A graph material reaches the forward pass as a generated WGSL `material_surface` function
//! (`regia_render::surface_graph`). The tracer evaluates the same text: each distinct graph of a
//! scene is translated for a compute closest-hit context, renamed with a per-graph suffix, and
//! spliced into `pt.wgsl` between [`PT_GRAPH_BEGIN`] and [`PT_GRAPH_END`] with a generated
//! `pt_graph_dispatch` switch on the graph id the material carries. Lighting stays the tracer's.
//!
//! **Translation.** The forward contract binds a uniform block and seven textures; a compute
//! kernel has neither. The translator rewrites, token by token:
//!
//! - `material.graph[i]` → `pt_graph_param(u32(i))`, the material's parameter block, which the
//!   pack step writes into the tracer's word buffer (64 words) followed by the seven texture
//!   indices ([`GRAPH_BLOCK_WORDS`]).
//! - `textureSample(t, s, uv)` and `textureSampleLevel(t, s, uv, lod)` → `pt_graph_tex(t, uv)` and
//!   `pt_graph_tex_level(t, uv, lod)`; a binding name becomes its slot number, the sampler `0u`,
//!   and the helper parameter types `texture_2d<f32>` and `sampler` become `u32`.
//! - **Derivatives.** A closest hit has no pixel quad, so `textureSample`'s implicit level
//!   is not defined. The tracer has no mip chains: every sample is the bilinear top level
//!   (the fixed-LOD option of the brief), the same filter its standard materials use. An
//!   explicit level is accepted and ignored. Explicit derivative builtins (`dpdx`, `dpdy`,
//!   `fwidth` and their variants), gradient and bias sampling, texel loads, gathers and
//!   `discard` are refused by name ([`PtGraphRefusal`]).
//! - `frame.pcss.z` (the parallax profile gate) → `1.0`: parallax is on, as on the profiles
//!   that ship it. Any other `frame` or `material` read is refused.
//!
//! A refused graph is not composed; its materials trace with their standard factors and the
//! refusal is reported in [`PtGraphStats`] (`PtMetadata::surface_graphs`), never silently.
//! Graphs with a vertex stage (a world position offset the tracer's static BVH does not apply)
//! and graphs that read the second UV set (the tracer's meshes carry one) are refused too.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::scene::PtScene;

/// First line of the replaceable graph region in `pt.wgsl`.
pub const PT_GRAPH_BEGIN: &str = "// @pt_material_graphs_begin";

/// Last line of the replaceable graph region in `pt.wgsl`.
pub const PT_GRAPH_END: &str = "// @pt_material_graphs_end";

/// Words of one graph material's block in the tracer's word buffer: the 16 `vec4` parameter
/// slots (64 words, raw `f32` bits) then the seven graph texture slots as tracer texture indices
/// (`0xffff_ffff` = unbound, sampled as white), padded to 72.
pub const GRAPH_BLOCK_WORDS: usize = 72;

/// Texture binding names of the forward contract, in slot order (`GRAPH_TEXTURE_BINDINGS`).
const BINDINGS: [&str; regia_render::surface_graph::GRAPH_TEXTURE_SLOTS] =
    regia_render::surface_graph::GRAPH_TEXTURE_BINDINGS;

/// Constructs the tracer cannot evaluate in a closest-hit kernel, with the reason.
const REFUSED: &[(&str, &str)] = &[
    (
        "dpdx",
        "screen-space derivatives: a compute closest hit has no pixel quad",
    ),
    (
        "dpdy",
        "screen-space derivatives: a compute closest hit has no pixel quad",
    ),
    (
        "dpdxFine",
        "screen-space derivatives: a compute closest hit has no pixel quad",
    ),
    (
        "dpdyFine",
        "screen-space derivatives: a compute closest hit has no pixel quad",
    ),
    (
        "dpdxCoarse",
        "screen-space derivatives: a compute closest hit has no pixel quad",
    ),
    (
        "dpdyCoarse",
        "screen-space derivatives: a compute closest hit has no pixel quad",
    ),
    (
        "fwidth",
        "screen-space derivatives: a compute closest hit has no pixel quad",
    ),
    (
        "fwidthFine",
        "screen-space derivatives: a compute closest hit has no pixel quad",
    ),
    (
        "fwidthCoarse",
        "screen-space derivatives: a compute closest hit has no pixel quad",
    ),
    (
        "textureSampleBias",
        "mip bias: the tracer samples the top level only",
    ),
    (
        "textureSampleGrad",
        "gradient sampling: the tracer samples the top level only",
    ),
    (
        "textureSampleCompare",
        "depth comparison sampling has no tracer texture",
    ),
    (
        "textureSampleCompareLevel",
        "depth comparison sampling has no tracer texture",
    ),
    (
        "textureSampleBaseClampToEdge",
        "external textures have no tracer texture",
    ),
    ("textureGather", "texel gathers have no tracer texture"),
    ("textureGatherCompare", "texel gathers have no tracer texture"),
    ("textureLoad", "texel loads have no tracer texture"),
    (
        "textureDimensions",
        "texture sizes are not exposed to a traced graph",
    ),
    ("textureNumLevels", "the tracer's textures have no mip chain"),
    ("discard", "a compute kernel cannot discard"),
];

/// A graph the tracer did not compose, by the construct that stopped it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtGraphRefusal {
    /// Index of the material in `PtScene::materials`.
    pub material: usize,
    /// The construct: a WGSL builtin (`dpdx`), `material_vertex_offset`, `second_uv`,
    /// `frame`, `material` or `compose` (the composer's own error).
    pub construct: String,
    /// Why the tracer cannot evaluate it.
    pub reason: String,
}

/// What the tracer did with the scene's material graphs (`PtMetadata::surface_graphs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtGraphStats {
    /// Distinct graphs composed into the kernels.
    pub graphs: u32,
    /// Materials shaded by a composed graph.
    pub materials: u32,
    /// Graph materials traced with their standard factors instead, each with its reason.
    pub refusals: Vec<PtGraphRefusal>,
}

impl PtGraphStats {
    /// True when the scene has no graph material at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.graphs == 0 && self.materials == 0 && self.refusals.is_empty()
    }
}

/// The graphs of one scene: the translated sources in graph-id order and each material's id.
#[derive(Clone, Debug, Default)]
pub(crate) struct GraphSet {
    /// Translated, renamed graph sources; index = graph id.
    pub sources: Vec<String>,
    /// Per material of the scene: its graph id when a composed graph shades it.
    pub ids: Vec<Option<u32>>,
    /// Counts and refusals.
    pub stats: PtGraphStats,
}

impl GraphSet {
    /// Cache key of the composed kernels (0 for no graphs: the base module).
    ///
    /// FNV-1a of the sources in order, the same bytes `source_key` hashes for one string.
    /// Each source is hashed in place: a `begin` does not copy every graph's WGSL.
    pub fn key(&self) -> u64 {
        if self.sources.is_empty() {
            return 0;
        }
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for source in &self.sources {
            #[cfg(test)]
            tests::note_key_source();
            for byte in source.bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }
}

/// A refused construct: `(construct, reason)`.
type Refused = (String, String);

fn refused(construct: &str, reason: &str) -> Refused {
    (construct.to_string(), reason.to_string())
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Index just past the bracket that closes the one at `open` (`(` or `[`), counting both kinds.
fn closing(src: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in src.bytes().enumerate().skip(open) {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Arguments of a call whose text between the parentheses is `inner`, split at top-level commas.
fn split_args(inner: &str) -> Vec<&str> {
    let mut args = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in inner.bytes().enumerate() {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b',' if depth == 0 => {
                args.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if !inner[start..].trim().is_empty() {
        args.push(&inner[start..]);
    }
    args
}

/// The first non-space byte at or after `at`.
fn skip_space(src: &str, mut at: usize) -> usize {
    let b = src.as_bytes();
    while at < b.len() && b[at].is_ascii_whitespace() {
        at += 1;
    }
    at
}

/// Function names `src` declares (`fn name(`).
fn declared_functions(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut names = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            i = src[i..].find('\n').map_or(b.len(), |e| i + e);
            continue;
        }
        if is_ident_start(b[i]) && (i == 0 || !is_ident(b[i - 1])) {
            let start = i;
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
            if &src[start..i] == "fn" {
                let at = skip_space(src, i);
                let mut end = at;
                while end < b.len() && is_ident(b[end]) {
                    end += 1;
                }
                if end > at {
                    names.push(src[at..end].to_string());
                }
            }
            continue;
        }
        i += 1;
    }
    names
}

/// Rewrites `src` for the tracer (module docs), renaming the identifiers in `rename`.
fn rewrite(src: &str, rename: &HashMap<String, String>) -> Result<String, Refused> {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len() + src.len() / 4);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = src[i..].find('\n').map_or(b.len(), |e| i + e);
            out.push_str(&src[i..end]);
            i = end;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let end = src[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
            out.push_str(&src[i..end]);
            i = end;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (is_ident(b[i]) || b[i] == b'.') {
                i += 1;
            }
            out.push_str(&src[start..i]);
            continue;
        }
        if !is_ident_start(c) {
            let ch = src[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let start = i;
        while i < b.len() && is_ident(b[i]) {
            i += 1;
        }
        let ident = &src[start..i];
        // A member (`in.uv`, `.x`) or a path segment (`regia::pbr`) is never rewritten.
        let prev = src[..start].trim_end();
        if prev.ends_with('.') || prev.ends_with("::") {
            out.push_str(ident);
            continue;
        }
        match ident {
            "material" => {
                let rest = &src[i..];
                let Some(tail) = rest.strip_prefix(".graph") else {
                    return Err(refused(
                        "material",
                        "the standard material block is not readable from a traced graph",
                    ));
                };
                let open = i + ".graph".len();
                let open = skip_space(src, open);
                if b.get(open) != Some(&b'[') || tail.is_empty() {
                    return Err(refused("material", "only indexed graph slots are readable"));
                }
                let close = closing(src, open).ok_or_else(|| refused("material", "unbalanced brackets"))?;
                let index = rewrite(&src[open + 1..close - 1], rename)?;
                let _ = write!(out, "pt_graph_param(u32({index}))");
                i = close;
            }
            "frame" => {
                let rest = &src[i..];
                if let Some(after) = rest.strip_prefix(".pcss.z") {
                    // `.pcss.zw` or `.pcss.z_` would be another member: refuse those.
                    if after.bytes().next().is_some_and(is_ident) {
                        return Err(refused("frame", "the tracer has no raster frame uniform"));
                    }
                    out.push_str("1.0");
                    i += ".pcss.z".len();
                } else {
                    return Err(refused("frame", "the tracer has no raster frame uniform"));
                }
            }
            "textureSample" | "textureSampleLevel" => {
                let open = skip_space(src, i);
                if b.get(open) != Some(&b'(') {
                    return Err(refused(ident, "not a call"));
                }
                let close = closing(src, open).ok_or_else(|| refused(ident, "unbalanced parentheses"))?;
                let args = split_args(&src[open + 1..close - 1]);
                let level = ident == "textureSampleLevel";
                if args.len() != if level { 4 } else { 3 } {
                    return Err(refused(ident, "unexpected argument count"));
                }
                let texture = rewrite(args[0].trim(), rename)?;
                let uv = rewrite(args[2].trim(), rename)?;
                if level {
                    let lod = rewrite(args[3].trim(), rename)?;
                    let _ = write!(out, "pt_graph_tex_level({texture}, {uv}, {lod})");
                } else {
                    let _ = write!(out, "pt_graph_tex({texture}, {uv})");
                }
                i = close;
            }
            "texture_2d" => {
                let rest = src[i..].trim_start();
                let Some(after) = rest.strip_prefix("<f32>") else {
                    return Err(refused("texture_2d", "only float textures are traced"));
                };
                out.push_str("u32");
                i = b.len() - after.len();
            }
            "sampler" => out.push_str("u32"),
            "tex_samp" => out.push_str("0u"),
            other => {
                if let Some((_, why)) = REFUSED.iter().find(|(name, _)| *name == other) {
                    return Err(refused(other, why));
                }
                if let Some(slot) = BINDINGS.iter().position(|name| *name == other) {
                    let _ = write!(out, "{slot}u");
                } else if let Some(renamed) = rename.get(other) {
                    out.push_str(renamed);
                } else {
                    out.push_str(other);
                }
            }
        }
    }
    Ok(out)
}

/// The graph surface `surface` (a spliced `material_surface` and its helpers) as the tracer
/// composes it under graph id `id`: every function it declares gains the suffix `_g{id}`, so
/// `material_surface` becomes `material_surface_g{id}`.
///
/// # Errors
///
/// The refused construct and the reason (module docs).
pub fn translate(surface: &str, id: u32) -> Result<String, (String, String)> {
    let rename: HashMap<String, String> = declared_functions(surface)
        .into_iter()
        .map(|name| {
            let renamed = format!("{name}_g{id}");
            (name, renamed)
        })
        .collect();
    if !rename.contains_key("material_surface") {
        return Err(refused(
            "material_surface",
            "the graph declares no surface function",
        ));
    }
    rewrite(surface, &rename)
}

/// Stand-ins for the tracer's graph helpers, so one translated graph composes and validates on
/// its own (a refusal names the graph instead of failing the whole tracer module).
const PROBE_PRELUDE: &str = "#import regia::pbr::surface
fn pt_graph_param(i: u32) -> vec4<f32> { return vec4<f32>(f32(i)); }
fn pt_graph_tex(slot: u32, uv: vec2<f32>) -> vec4<f32> { return vec4<f32>(uv, f32(slot), 1.0); }
fn pt_graph_tex_level(slot: u32, uv: vec2<f32>, lod: f32) -> vec4<f32> { return vec4<f32>(uv, f32(slot), lod); }
";

const PROBE_ENTRY: &str = "
@group(0) @binding(0) var<storage, read_write> pt_graph_probe: array<vec4<f32>>;
@compute @workgroup_size(1)
fn pt_graph_probe_main() {
    let s = material_surface_g0(regia::pbr::surface::SurfaceInput(vec3<f32>(0.0), vec3<f32>(0.0, 1.0, 0.0), vec2<f32>(0.0), vec3<f32>(0.0, 1.0, 0.0), vec4<f32>(1.0), vec4<f32>(1.0, 0.0, 0.0, 1.0), 1.0, vec2<f32>(0.0), vec3<f32>(0.0), 1.0));
    pt_graph_probe[0] = vec4<f32>(s.base_color, s.alpha);
    pt_graph_probe[1] = vec4<f32>(s.normal, s.roughness);
    pt_graph_probe[2] = vec4<f32>(s.emissive, s.metallic + s.occlusion + s.unlit);
}
";

/// Composes and validates one translated graph (id 0) against the stand-in helpers.
fn probe(translated: &str) -> Result<(), String> {
    let source = format!("{PROBE_PRELUDE}{translated}{PROBE_ENTRY}");
    let module = regia_render::compose_source(&source, "pt_graph_probe.wgsl", HashMap::new())
        .map_err(|e| e.to_string())?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map(|_| ())
    .map_err(|e| e.emit_to_string(""))
}

/// Translated-and-checked graphs by source key: a session re-traces the same graphs often.
fn checked(surface: &str) -> Result<(), Refused> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<u64, Result<(), Refused>>>> = OnceLock::new();
    let key = regia_render::surface_graph::source_key(surface);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock()
        && let Some(found) = map.get(&key)
    {
        return found.clone();
    }
    let found = translate(surface, 0).and_then(|t| probe(&t).map_err(|e| refused("compose", &e)));
    if let Ok(mut map) = cache.lock() {
        if map.len() >= 1024 {
            map.clear();
        }
        map.insert(key, found.clone());
    }
    found
}

/// Whether the tracer will compose this graph entry, running the same checks `graph_set`
/// refuses on: a vertex offset, a second UV set, or a surface the translator/validator
/// rejects (`checked`). A refused entry is not composed; its material keeps its standard
/// factors (`graph.rs` module docs), so callers that zero a standard input for composed
/// graphs (extraction zeroes the emissive, ADR-2725) gate on this.
pub(crate) fn composes(entry: &crate::scene::PtSurfaceGraph) -> bool {
    if entry.vertex_offset || entry.uses_uv1 {
        return false;
    }
    checked(&entry.surface).is_ok()
}

/// Collects the scene's graphs: one id per distinct surface text in first-use order, the
/// refusals, and each material's id.
pub(crate) fn graph_set(scene: &PtScene) -> GraphSet {
    let mut set = GraphSet {
        ids: vec![None; scene.materials.len()],
        ..GraphSet::default()
    };
    if scene.surface_graphs.is_empty() {
        return set;
    }
    let mut by_text: HashMap<Arc<str>, u32> = HashMap::new();
    for graph in &scene.surface_graphs {
        let material = graph.material;
        if material >= scene.materials.len() || set.ids[material].is_some() {
            continue;
        }
        let refusal = if graph.vertex_offset {
            Some(refused(
                "material_vertex_offset",
                "a world position offset: the tracer's geometry is the undisplaced mesh",
            ))
        } else if graph.uses_uv1 {
            Some(refused("second_uv", "the tracer's meshes carry one UV set"))
        } else {
            checked(&graph.surface).err()
        };
        if let Some((construct, reason)) = refusal {
            tracing::warn!(target: "regia_pt", material, %construct, %reason, "material graph refused by the tracer");
            set.stats.refusals.push(PtGraphRefusal {
                material,
                construct,
                reason,
            });
            continue;
        }
        let next = set.sources.len() as u32;
        let id = *by_text.entry(graph.surface.clone()).or_insert(next);
        if id == next {
            match translate(&graph.surface, id) {
                Ok(text) => set.sources.push(text),
                Err((construct, reason)) => {
                    by_text.remove(&graph.surface);
                    set.stats.refusals.push(PtGraphRefusal {
                        material,
                        construct,
                        reason,
                    });
                    continue;
                }
            }
        }
        set.ids[material] = Some(id);
        set.stats.materials += 1;
    }
    set.stats.graphs = set.sources.len() as u32;
    set
}

/// The dispatch region for `set`: the translated graphs, `PT_GRAPHS` and the switch.
fn region(set: &GraphSet) -> String {
    let mut text = String::new();
    for source in &set.sources {
        text.push_str(source);
        text.push('\n');
    }
    let _ = writeln!(text, "const PT_GRAPHS: u32 = {}u;", set.sources.len());
    text.push_str(
        "fn pt_graph_dispatch(id: u32, in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {\n    switch id {\n",
    );
    for id in 0..set.sources.len() {
        let _ = writeln!(
            text,
            "        case {id}u: {{ return material_surface_g{id}(in); }}"
        );
    }
    text.push_str("        default: { return regia::pbr::surface::default_surface(); }\n    }\n}\n");
    text
}

/// `base` (`pt.wgsl`) with the graph region replaced by the graphs of `set`.
///
/// # Errors
///
/// The markers are missing from `base`.
pub(crate) fn composed_source(base: &str, set: &GraphSet) -> Result<String, String> {
    let begin = base
        .find(PT_GRAPH_BEGIN)
        .ok_or_else(|| format!("pt.wgsl has no `{PT_GRAPH_BEGIN}`"))?;
    let end = base
        .find(PT_GRAPH_END)
        .ok_or_else(|| format!("pt.wgsl has no `{PT_GRAPH_END}`"))?;
    if end < begin {
        return Err("pt.wgsl's graph markers are out of order".into());
    }
    let mut out =
        String::with_capacity(base.len() + set.sources.iter().map(String::len).sum::<usize>() + 512);
    out.push_str(&base[..begin + PT_GRAPH_BEGIN.len()]);
    out.push('\n');
    out.push_str(&region(set));
    out.push_str(&base[end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    thread_local! {
        static COUNT_KEY_SOURCES: Cell<bool> = const { Cell::new(false) };
        static KEY_SOURCES: Cell<u32> = const { Cell::new(0) };
    }

    pub(super) fn note_key_source() {
        if COUNT_KEY_SOURCES.get() {
            KEY_SOURCES.set(KEY_SOURCES.get() + 1);
        }
    }

    /// The kernel cache key is the FNV of the sources in order. Hashing `sources.concat()`
    /// copies every graph's WGSL; the count is one walk per source, and stays 0 when `key`
    /// hashes a single joined string instead.
    #[test]
    fn perfaud_kernel_key_does_not_join_the_sources() {
        let set = GraphSet {
            sources: vec!["abc".into(), "de".into()],
            ids: vec![Some(0)],
            stats: PtGraphStats::default(),
        };
        COUNT_KEY_SOURCES.set(true);
        KEY_SOURCES.set(0);
        assert_eq!(set.key(), regia_render::surface_graph::source_key("abcde"));
        let walked = KEY_SOURCES.get();
        COUNT_KEY_SOURCES.set(false);
        assert_eq!(
            walked, 2,
            "key() walked {walked} sources; hashing one joined string walks none"
        );
    }

    const SURFACE: &str = r"
// Generated by regia_matgraph. Do not edit.
fn mg_triplanar(t: texture_2d<f32>, s: sampler, p: vec3<f32>, n: vec3<f32>, sharpness: f32) -> vec4<f32> {
    let w0 = pow(abs(n), vec3<f32>(sharpness));
    let w = w0 / max(w0.x + w0.y + w0.z, 1e-5);
    let x = textureSample(t, s, p.zy);
    let y = textureSample(t, s, p.xz);
    let z = textureSampleLevel(t, s, p.xy, 0.0);
    return x * w.x + y * w.y + z * w.z;
}
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    let v0: vec4<f32> = mg_triplanar(detail_tex, tex_samp, in.world_position * material.graph[2].x, in.world_normal, 4.0);
    let v1: vec3<f32> = material.graph[1].xyz * textureSample(albedo_tex, tex_samp, in.uv * vec2<f32>(2.0, 1e-6)).rgb;
    let mg_normal = in.world_normal;
    return regia::pbr::surface::Surface(v1 * v0.rgb, 1.0, 0.0, 0.5, mg_normal, 1.0, vec3<f32>(0.0), 0.0);
}
";

    #[test]
    fn the_translation_reads_the_tracer_block_and_renames_every_function() {
        let text = translate(SURFACE, 3).expect("translates");
        assert!(text.contains("fn material_surface_g3("), "{text}");
        assert!(text.contains("fn mg_triplanar_g3(t: u32, s: u32,"), "{text}");
        assert!(text.contains("mg_triplanar_g3(4u, 0u,"), "{text}");
        assert!(text.contains("pt_graph_param(u32(2)).x"), "{text}");
        assert!(
            text.contains("pt_graph_tex(0u, in.uv * vec2<f32>(2.0, 1e-6))"),
            "{text}"
        );
        assert!(text.contains("pt_graph_tex(t, p.zy)"), "{text}");
        assert!(text.contains("pt_graph_tex_level(t, p.xy, 0.0)"), "{text}");
        // Members and paths are left alone.
        assert!(text.contains("in.world_normal"), "{text}");
        assert!(text.contains("regia::pbr::surface::Surface("), "{text}");
        assert!(!text.contains("material.graph"), "{text}");
        assert!(!text.contains("texture_2d"), "{text}");
        assert!(!text.contains("tex_samp"), "{text}");
    }

    #[test]
    fn the_translation_composes_and_validates() {
        let text = translate(SURFACE, 0).expect("translates");
        probe(&text).expect("composes");
        assert_eq!(checked(SURFACE), Ok(()));
    }

    #[test]
    fn derivatives_and_unknown_reads_are_refused_by_name() {
        let with = |body: &str| {
            SURFACE.replace(
                "let mg_normal = in.world_normal;",
                &format!("let mg_normal = in.world_normal;\n    {body}"),
            )
        };
        for (body, construct) in [
            ("let d = dpdx(in.uv.x);", "dpdx"),
            ("let d = fwidth(in.uv);", "fwidth"),
            (
                "let d = textureSampleGrad(albedo_tex, tex_samp, in.uv, vec2<f32>(0.0), vec2<f32>(0.0));",
                "textureSampleGrad",
            ),
            (
                "let d = textureSampleBias(albedo_tex, tex_samp, in.uv, 1.0);",
                "textureSampleBias",
            ),
            ("let d = textureLoad(albedo_tex, vec2<i32>(0), 0);", "textureLoad"),
            ("let d = frame.camera_pos;", "frame"),
            ("let d = material.base_color;", "material"),
        ] {
            let err = translate(&with(body), 0).expect_err(body);
            assert_eq!(err.0, construct, "{body}: {err:?}");
            assert!(!err.1.is_empty());
        }
        // The parallax gate is the one frame read the tracer answers.
        let gate = with("let gate = frame.pcss.z;");
        assert!(translate(&gate, 0).expect("gate").contains("let gate = 1.0;"));
    }

    #[test]
    fn a_broken_graph_is_refused_as_compose() {
        let broken = SURFACE.replace(
            "return regia::pbr::surface::Surface(",
            "return regia::pbr::surface::Nope(",
        );
        let err = checked(&broken).expect_err("broken");
        assert_eq!(err.0, "compose");
    }

    #[test]
    fn composes_mirrors_graph_sets_refusal() {
        let entry = |surface: &str, vertex_offset: bool| crate::scene::PtSurfaceGraph {
            material: 0,
            surface: std::sync::Arc::from(surface),
            params: [[0.0; 4]; regia_render::surface_graph::GRAPH_PARAM_SLOTS],
            textures: [None; regia_render::surface_graph::GRAPH_TEXTURE_SLOTS],
            vertex_offset,
            uses_uv1: false,
            uses_time: false,
        };
        assert!(composes(&entry(SURFACE, false)), "the plain graph composes");
        let fwidth = SURFACE.replace(
            "let mg_normal = in.world_normal;",
            "let mg_normal = in.world_normal;\n    let d = fwidth(in.uv);",
        );
        assert!(
            !composes(&entry(&fwidth, false)),
            "a derivative read is refused, so the standard factors stay"
        );
        assert!(
            !composes(&entry(SURFACE, true)),
            "a vertex offset is refused, so the standard factors stay"
        );
    }

    #[test]
    fn the_region_replaces_the_markers_and_dispatches_by_id() {
        let base = format!("a\n{PT_GRAPH_BEGIN}\nold\n{PT_GRAPH_END}\nb\n");
        let set = GraphSet {
            sources: vec![
                translate(SURFACE, 0).expect("0"),
                translate(SURFACE, 1).expect("1"),
            ],
            ids: vec![Some(0), Some(1)],
            stats: PtGraphStats::default(),
        };
        let out = composed_source(&base, &set).expect("markers");
        assert!(!out.contains("old"));
        assert!(out.contains("const PT_GRAPHS: u32 = 2u;"));
        assert!(out.contains("case 1u: { return material_surface_g1(in); }"));
        assert!(out.starts_with("a\n") && out.ends_with(&format!("{PT_GRAPH_END}\nb\n")));
        assert_ne!(set.key(), 0);
        assert_eq!(GraphSet::default().key(), 0);
    }

    #[test]
    fn the_shipped_kernel_carries_the_markers_once() {
        let source = include_str!("../shaders/pt.wgsl");
        assert_eq!(source.matches(PT_GRAPH_BEGIN).count(), 1);
        assert_eq!(source.matches(PT_GRAPH_END).count(), 1);
    }

    /// A back face hands the graph the tangent negated with its handedness, as the forward
    /// pass's `graph_surface_input` does (`back_face_tangent` and the negated `w`); passing the
    /// front tangent with the flipped normal mirrors a graph normal map's X axis on back faces.
    #[test]
    fn graph_surface_negates_the_back_face_tangent_like_the_forward_pass() {
        let source = include_str!("../shaders/pt.wgsl");
        let start = source.find("fn graph_surface(").expect("graph_surface");
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").expect("end of graph_surface")];
        assert!(
            body.contains("s.tangent * facing,"),
            "graph_surface must pass the tangent negated on a back face: {body}"
        );
        let forward = include_str!("../../regia_render/shaders/lab_forward.wgsl");
        let start = forward
            .find("fn graph_surface_input(")
            .expect("graph_surface_input");
        let input = &forward[start..start + 1200.min(forward.len() - start)];
        assert!(
            input.contains("back_face_tangent(")
                && input.contains("select(-in.world_tangent.w, in.world_tangent.w"),
            "the forward pass's rule this mirrors changed: {input}"
        );
    }
}
