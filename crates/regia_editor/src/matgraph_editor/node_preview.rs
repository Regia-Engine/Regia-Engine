// SPDX-License-Identifier: MPL-2.0
//! Per-node previews (spec 80 § 12, ADR-9700): a small swatch of one node's value, made by
//! routing that node's output to an unlit base color in a derived graph and rendering it on
//! the preview sphere. The derived graph compiles on the fast path (no full shader
//! validation); swatches render only when the graph revision changes, at most
//! [`NODE_PREVIEWS_PER_FRAME`] per frame, and are cached by pipeline key (two nodes that
//! derive the same code and parameters share one swatch).
//!
//! These swatches are the editor's view of intermediate values; live debugging of graph values
//! in a running game (PIE) stays out of scope (spec 80 § 10).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_matgraph::{MaterialGraph, Node, SurfaceOutputs};
use regia_render::RenderDevice;

/// Swatches rendered per frame at most (the rest wait for the next frames).
pub const NODE_PREVIEWS_PER_FRAME: usize = 4;

/// Nodes whose derived graph is compiled per frame at most while planning (CPU bound).
pub const NODE_PREVIEW_PLANS_PER_FRAME: usize = 32;

/// Swatches the cache keeps at most (ADR-7451 (2)): past it, the least recently used swatch of
/// an off-screen node is dropped (its node renders again at the next graph revision). 256
/// swatches of 64 px are 4 MiB of textures. Swatches of nodes on screen are pinned (w11,
/// ADR-7730): when more than this many are visible the cache holds them all and drops only
/// off-screen ones.
pub const NODE_PREVIEW_SWATCH_CAP: usize = 256;

/// Swatch size the canvas draws, pixels.
pub const NODE_PREVIEW_SIZE: u32 = 64;

/// A free node name for the derived graph's helpers.
fn helper_name(graph: &MaterialGraph, base: &str) -> String {
    (0u32..)
        .map(|n| format!("{base}_{n}"))
        .find(|n| !graph.nodes.contains_key(n))
        .unwrap_or_else(|| base.to_string())
}

/// The derived graph that shows node `node` of `graph` as an unlit base color: every other
/// output cleared, the node's value widened or narrowed to RGB (a float as gray, a `Vec2` as
/// red/green, a `Vec4`'s RGB). The declarations stay, so parameters and textures read as in
/// the graph.
///
/// # Errors
///
/// No such node, or no RGB form of it compiles (the node itself has an error).
pub fn derived_graph(graph: &MaterialGraph, node: &str) -> Result<MaterialGraph, String> {
    if !graph.nodes.contains_key(node) {
        return Err(format!("no node `{node}`"));
    }
    let mut base = graph.clone();
    base.output = SurfaceOutputs::default();
    base.shading.unlit = true;
    let gray = helper_name(&base, "preview_rgb");
    let zero = helper_name(&base, "preview_zero");
    let width = regia_matgraph::compile_unvalidated(graph)
        .ok()
        .and_then(|c| c.node_types.get(node).map(|t| t.width()));
    // The node's width when the graph compiles; otherwise try every form.
    let order: Vec<usize> = match width {
        Some(w) => vec![w],
        None => vec![3, 4, 1, 2],
    };
    let mut last = String::new();
    for w in order {
        let mut g = base.clone();
        let driver = match w {
            3 => node.to_string(),
            4 => {
                g.nodes.insert(
                    gray.clone(),
                    Node::Swizzle {
                        input: node.to_string(),
                        channels: "xyz".into(),
                    },
                );
                gray.clone()
            }
            2 => {
                g.nodes.insert(zero.clone(), Node::Float(0.0));
                g.nodes
                    .insert(gray.clone(), Node::Append(node.to_string(), zero.clone()));
                gray.clone()
            }
            _ => {
                g.nodes.insert(
                    gray.clone(),
                    Node::Swizzle {
                        input: node.to_string(),
                        channels: "xxx".into(),
                    },
                );
                gray.clone()
            }
        };
        g.output.base_color = Some(driver);
        match regia_matgraph::compile_unvalidated(&g) {
            Ok(_) => return Ok(g),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("`{node}` has no previewable value: {last}"))
}

/// The cache key of a derived graph: its pipeline key plus its parameter defaults and texture
/// paths (same key, same swatch).
///
/// # Errors
///
/// The derived graph does not compile.
pub fn swatch_key(derived: &MaterialGraph) -> Result<u64, String> {
    let compiled = regia_matgraph::compile_unvalidated(derived).map_err(|e| e.to_string())?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    compiled.key.hash(&mut h);
    for slot in regia_matgraph::default_params(derived, 0.0) {
        for v in slot {
            v.to_bits().hash(&mut h);
        }
    }
    for t in &derived.textures {
        t.name.hash(&mut h);
        t.path.hash(&mut h);
    }
    Ok(h.finish())
}

/// A rendered swatch.
#[derive(Clone)]
pub struct NodeSwatch {
    /// The image.
    pub image: egui::ColorImage,
    /// Its cache key ([`swatch_key`]).
    pub key: u64,
    /// Mean display RGB of the sphere's pixels, 0..1.
    pub mean_rgb: [f32; 3],
}

/// Renders node `node`'s swatch (`size` pixels square, at least 64) on the preview sphere.
///
/// # Errors
///
/// [`derived_graph`] failed, a texture failed to load, or the render failed.
#[tracing::instrument(target = "regia_editor", skip_all, name = "matgraph_node_preview")]
pub fn render_node_preview(
    device: &RenderDevice,
    graph: &MaterialGraph,
    node: &str,
    dir: Option<&Path>,
    root: Option<&Path>,
    size: u32,
) -> Result<NodeSwatch, String> {
    let resolved = super::model::resolved(graph, dir, root)?;
    let derived = derived_graph(&resolved, node)?;
    let key = swatch_key(&derived)?;
    let material = super::preview::graph_lab_material_fast(&derived, dir, root)?;
    let side = size.clamp(64, 1024) as f32;
    let image = crate::material_editor::render_sphere_preview(device, &material, [side, side], 0)
        .ok_or_else(|| "the node preview render failed (see the log)".to_string())?;
    let mean_rgb = super::preview::measure(&image).mean_rgb;
    Ok(NodeSwatch { image, key, mean_rgb })
}

/// Distinct red levels in the central quarter of an image (all sphere): many for a varying
/// value, a few for a constant (shading is off).
#[must_use]
pub fn gray_levels(image: &egui::ColorImage) -> usize {
    let [w, h] = image.size;
    let mut seen = std::collections::BTreeSet::new();
    for y in h * 3 / 8..h * 5 / 8 {
        for x in w * 3 / 8..w * 5 / 8 {
            if let Some(p) = image.pixels.get(y * w + x) {
                seen.insert(p.r());
            }
        }
    }
    seen.len()
}

/// One cached swatch and when it was last used.
struct CachedSwatch {
    texture: egui::TextureHandle,
    /// The cache clock at its last use (settled, drawn or planned).
    used: AtomicU64,
}

/// The panel's per-node swatches: the key each node's swatch has at a graph revision, and the
/// swatches by key, at most [`NODE_PREVIEW_SWATCH_CAP`] of them (least recently used first out,
/// ADR-7451 (2)).
#[derive(Default)]
pub struct NodePreviewCache {
    /// node → (graph revision it was planned at, key, or the reason it has none).
    nodes: BTreeMap<String, (u64, Result<u64, String>)>,
    /// key → the uploaded swatch.
    swatches: HashMap<u64, CachedSwatch>,
    /// Advances on every settle; a use stamps the swatch with it.
    clock: u64,
    /// The nodes on screen (w11): their swatches are never evicted.
    visible: BTreeSet<String>,
}

impl NodePreviewCache {
    /// The nodes whose swatch must render now: at most `cap` of the nodes planned at another
    /// revision whose key has no swatch yet, with their keys. Nodes whose key is already cached
    /// (or that cannot preview) are settled here without a render.
    pub fn plan(&mut self, graph: &MaterialGraph, revision: u64, cap: usize) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        self.nodes.retain(|n, _| graph.nodes.contains_key(n));
        let mut looked = 0usize;
        for name in graph.nodes.keys() {
            if out.len() >= cap || looked >= NODE_PREVIEW_PLANS_PER_FRAME {
                break;
            }
            if self.nodes.get(name).is_some_and(|(r, _)| *r == revision) {
                continue;
            }
            looked += 1;
            let key = derived_graph(graph, name).and_then(|d| swatch_key(&d));
            let render = match &key {
                Ok(k) => match self.swatches.get(k) {
                    Some(cached) => {
                        cached.used.store(self.clock, Ordering::Relaxed);
                        false
                    }
                    None => !out.iter().any(|(_, o)| o == k),
                },
                Err(_) => false,
            };
            match key {
                Ok(k) if render => out.push((name.clone(), k)),
                other => {
                    self.nodes.insert(name.clone(), (revision, other));
                }
            }
        }
        out
    }

    /// Whether every node of `graph` has its swatch (or its reason) for `revision`.
    #[must_use]
    pub fn settled(&self, graph: &MaterialGraph, revision: u64) -> bool {
        graph
            .nodes
            .keys()
            .all(|n| self.nodes.get(n).is_some_and(|(r, _)| *r == revision))
    }

    /// Records a rendered (or failed) swatch for `node` at `revision`.
    pub fn settle(
        &mut self,
        node: &str,
        revision: u64,
        key: Result<u64, String>,
        texture: Option<egui::TextureHandle>,
    ) {
        // The node's key first, so a visible node's new swatch counts as pinned when it lands.
        let swatch = match (&key, texture) {
            (Ok(k), Some(texture)) => Some((*k, texture)),
            _ => None,
        };
        self.nodes.insert(node.to_string(), (revision, key));
        if let Some((k, texture)) = swatch {
            self.clock += 1;
            self.swatches.insert(
                k,
                CachedSwatch {
                    texture,
                    used: AtomicU64::new(self.clock),
                },
            );
            self.evict();
        }
    }

    /// Drops the least recently used swatches past [`NODE_PREVIEW_SWATCH_CAP`], never one a
    /// visible node shows ([`Self::set_visible`]): with more visible swatches than the cap, every
    /// off-screen one goes and the visible ones stay. The newest swatch has the highest stamp,
    /// so it always stays; equal stamps go by key.
    fn evict(&mut self) {
        let over = self.swatches.len().saturating_sub(NODE_PREVIEW_SWATCH_CAP);
        if over == 0 {
            return;
        }
        let pinned: HashSet<u64> = self
            .visible
            .iter()
            .filter_map(|n| match self.nodes.get(n) {
                Some((_, Ok(key))) => Some(*key),
                _ => None,
            })
            .collect();
        let mut by_use: Vec<(u64, u64)> = self
            .swatches
            .iter()
            .filter(|(key, _)| !pinned.contains(key))
            .map(|(key, cached)| (cached.used.load(Ordering::Relaxed), *key))
            .collect();
        by_use.sort_unstable();
        for (_, key) in by_use.into_iter().take(over) {
            self.swatches.remove(&key);
        }
    }

    /// The swatch node `node` shows (a stale one until its new one renders). A lookup counts as a
    /// use: a swatch the canvas draws every frame is never the least recently used.
    #[must_use]
    pub fn swatch(&self, node: &str) -> Option<&egui::TextureHandle> {
        match self.nodes.get(node) {
            Some((_, Ok(key))) => self.swatches.get(key).map(|cached| {
                cached.used.store(self.clock, Ordering::Relaxed);
                &cached.texture
            }),
            _ => None,
        }
    }

    /// Swatches held now (at most [`NODE_PREVIEW_SWATCH_CAP`], or the visible nodes' swatches
    /// when more than that are on screen).
    #[must_use]
    pub fn swatch_count(&self) -> usize {
        self.swatches.len()
    }

    /// Why node `node` has no swatch, if it has none for a known reason.
    #[must_use]
    pub fn reason(&self, node: &str) -> Option<&str> {
        match self.nodes.get(node) {
            Some((_, Err(e))) => Some(e.as_str()),
            _ => None,
        }
    }

    /// Sets the nodes the canvas shows now (w11): their swatches are pinned, so the cap only
    /// ever evicts swatches of off-screen nodes, and a graph with more visible nodes than
    /// [`NODE_PREVIEW_SWATCH_CAP`] keeps every visible swatch (the cache then holds more than
    /// the cap until the view moves). The canvas calls this once per drawn frame.
    pub fn set_visible(&mut self, nodes: impl IntoIterator<Item = String>) {
        self.visible.clear();
        self.visible.extend(nodes);
    }

    /// The nodes [`Self::set_visible`] last named.
    #[must_use]
    pub fn visible_count(&self) -> usize {
        self.visible.len()
    }

    /// Drops every swatch (the toggle went off, or another document opened).
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.swatches.clear();
        self.visible.clear();
        self.clock = 0;
    }
}
