// SPDX-License-Identifier: MPL-2.0
//! The plant preview (spec 68 § 8): a Plant graph open in the procedural panel shows the selected
//! node's skeleton or mesh in an asset preview viewport instead of the level, and a "variants"
//! strip shows six seeds side by side; clicking one sets the plant's seed (the seed the preview,
//! the export defaults and `editor.plant_preview` use).
//!
//! Each variant is one in-memory run of the graph with the run seed `first_seed + k` (the same
//! numbering `regia procgen plant --seed S --variants N` writes). Runs happen on the editor thread:
//! the agent method builds every variant at once; the panel builds at most one per editor tick, so
//! opening a heavy graph never stalls a frame for all six. Nothing is published to the level.
//!
//! When the editor has a render device and the shown node makes a mesh, each variant also gets a
//! lit thumbnail ([`render_lit`], post05w4 / ADR-7275): the mesh drawn by the batch thumbnail
//! renderer on the asset viewer's stage (key light, sky, editor exposure), bark and leaves in flat
//! colors, at most one per editor tick. Without a device, or for a skeleton, the viewport is drawn
//! with the egui painter from the skeleton (one stroke per branch segment, width from the branch
//! radius, side view on the X/Y plane, fitted to the plant's bounds), as before (ADR-7215).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_ecs::prelude::*;
use regia_procgen::data::Payload;
use regia_procgen::plant::{PlantMesh, PlantSkeleton};
use regia_procgen::{CancelToken, Evaluator, ProcGraph, RunInputs};

use super::doc::{DocError, ProcGraphDoc};
use crate::mesh_source::{LoadedModel, ModelPart};

/// Seeds the variants strip shows.
pub const VARIANT_COUNT: usize = 6;
/// Most branch segments one preview keeps for drawing (an even subsample beyond).
pub const MAX_SEGMENTS: usize = 20_000;
/// Side of a lit thumbnail, pixels (the viewport and the strip scale it).
pub const LIT_SIZE: u32 = 192;
/// Flat bark color of the lit thumbnail (linear RGB).
pub const BARK_COLOR: [f32; 4] = [0.30, 0.20, 0.12, 1.0];
/// Flat leaf color of the lit thumbnail (linear RGB).
pub const LEAF_COLOR: [f32; 4] = [0.16, 0.38, 0.10, 1.0];

/// One branch segment to draw: `(from, to, radius_m)` in plant space.
pub type Segment = ([f32; 3], [f32; 3], f32);

/// One built variant.
#[derive(Clone, Debug, PartialEq)]
pub struct PlantVariant {
    /// The run seed.
    pub seed: u32,
    /// The node whose plant data is shown.
    pub node: Option<u32>,
    /// Plants in the item.
    pub plants: usize,
    /// Skeleton nodes over all plants.
    pub nodes: usize,
    /// Height of the tallest plant, meters.
    pub height_m: f32,
    /// Widest crown, meters.
    pub crown_width_m: f32,
    /// Mesh triangles (0 for a skeleton).
    pub triangles: usize,
    /// Bounds `(min, max)` of the plant data.
    pub bounds: ([f32; 3], [f32; 3]),
    /// Branch segments to draw (at most [`MAX_SEGMENTS`]).
    pub segments: Vec<Segment>,
    /// Why the run failed, when it did (`code: message`).
    pub error: Option<String>,
}

/// The plant preview's state (world resource).
#[derive(Resource, Debug, Default)]
pub struct PlantPreview {
    /// The Plant graph previewed (project path), when one is.
    pub asset: Option<String>,
    /// The plant's seed: the variant the viewport shows.
    pub seed: u32,
    /// The strip's first seed: it shows `first_seed .. first_seed + VARIANT_COUNT`.
    pub first_seed: u32,
    /// The node whose skeleton or mesh is shown (`None`: the last node with plant data).
    pub node: Option<u32>,
    /// The strip's variants, by seed (built ones only).
    pub variants: BTreeMap<u32, PlantVariant>,
    /// Runs made so far (tests count them).
    pub builds: u64,
    /// What the built variants were made from: asset, graph text hash, node.
    key: Option<(String, u64, Option<u32>)>,
    /// The meshes of the built variants that have one, by seed (the lit thumbnails' subjects).
    models: BTreeMap<u32, Arc<LoadedModel>>,
    /// Lit thumbnails by seed, with a serial that changes whenever a picture is replaced.
    pub lit: BTreeMap<u32, LitThumb>,
    /// Lit thumbnails rendered so far.
    pub lit_renders: u64,
    serial: u64,
}

/// A variant's lit thumbnail.
#[derive(Clone, Debug)]
pub struct LitThumb {
    /// The picture ([`LIT_SIZE`] square, sRGB).
    pub image: Arc<egui::ColorImage>,
    /// Unique per picture (texture caches key on it).
    pub serial: u64,
}

impl LitThumb {
    /// `(coverage, mean_rgb)`: the share of pixels that differ from their row's first pixel (the
    /// backdrop at that height, so a sky gradient does not count), and the mean color of those
    /// pixels (0..255).
    #[must_use]
    pub fn coverage(&self) -> (f32, [f32; 3]) {
        let [w, h] = self.image.size;
        let px = &self.image.pixels;
        if w == 0 || h == 0 || px.len() != w * h {
            return (0.0, [0.0; 3]);
        }
        let mut sum = [0.0f64; 3];
        let mut n = 0usize;
        for row in px.chunks(w) {
            let bg = row[0];
            for p in row {
                let d = (i32::from(p.r()) - i32::from(bg.r())).abs()
                    + (i32::from(p.g()) - i32::from(bg.g())).abs()
                    + (i32::from(p.b()) - i32::from(bg.b())).abs();
                if d > 24 {
                    sum[0] += f64::from(p.r());
                    sum[1] += f64::from(p.g());
                    sum[2] += f64::from(p.b());
                    n += 1;
                }
            }
        }
        if n == 0 {
            return (0.0, [0.0; 3]);
        }
        let mean = sum.map(|s| (s / n as f64) as f32);
        (n as f32 / px.len() as f32, mean)
    }
}

impl PlantPreview {
    /// The seeds the strip shows.
    #[must_use]
    pub fn strip_seeds(&self) -> Vec<u32> {
        (0..VARIANT_COUNT as u32)
            .map(|k| self.first_seed.wrapping_add(k))
            .collect()
    }

    /// The variant the viewport shows (the plant's seed), when built.
    #[must_use]
    pub fn shown(&self) -> Option<&PlantVariant> {
        self.variants.get(&self.seed)
    }
}

/// The graph text of `asset`: the open document's graph when it is that file (unsaved edits
/// included), else the project file, else a shipped plant starter.
fn graph_text(world: &World, asset: &str) -> Option<String> {
    if let Some(d) = world.get_resource::<ProcGraphDoc>()
        && d.path.as_deref() == Some(asset)
    {
        return Some(d.graph.to_ron());
    }
    super::doc::graph_text(world, asset).or_else(|| regia_procgen::plant::starter(asset).map(str::to_string))
}

fn text_hash(text: &str) -> u64 {
    // FNV-1a: an equality key for "the graph changed", not a cryptographic hash.
    text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Points the preview at a Plant graph (and, optionally, a node and the strip's first seed). The
/// plant's seed becomes `first_seed` when the strip moves past it.
///
/// # Errors
///
/// `procgen_unknown_graph` (unreadable), `procgen_graph_invalid` (does not parse),
/// `procgen_wrong_domain` (not a Plant graph).
pub fn set_asset(
    world: &mut World,
    asset: &str,
    first_seed: Option<u32>,
    node: Option<u32>,
) -> Result<(), DocError> {
    let text = graph_text(world, asset)
        .ok_or_else(|| DocError::new("procgen_unknown_graph", format!("cannot read `{asset}`")))?;
    let g = ProcGraph::from_ron(&text)
        .map_err(|e| DocError::new("procgen_graph_invalid", format!("{asset}: {e}")))?;
    if g.flags.domain != regia_procgen::nodes::Domain::Plant {
        return Err(DocError::new(
            "procgen_wrong_domain",
            format!(
                "`{asset}` is a {} graph; the plant preview shows Plant graphs",
                g.flags.domain.name()
            ),
        ));
    }
    let mut p = world.get_resource_or_insert_with(PlantPreview::default);
    if p.asset.as_deref() != Some(asset) {
        p.asset = Some(asset.to_string());
        p.variants.clear();
        p.key = None;
    }
    p.node = node;
    if let Some(first) = first_seed {
        p.first_seed = first;
    }
    let seeds = p.strip_seeds();
    if !seeds.contains(&p.seed) {
        p.seed = p.first_seed;
    }
    Ok(())
}

/// Sets the plant's seed (a click on a variant of the strip): the viewport shows that seed.
pub fn set_seed(world: &mut World, seed: u32) {
    world.get_resource_or_insert_with(PlantPreview::default).seed = seed;
}

/// One run of the graph at `seed`, reduced to what the preview draws.
/// A plant mesh as a model the thumbnail renderer draws: one part per slot, bark and leaves in
/// [`BARK_COLOR`] and [`LEAF_COLOR`] (matte); leaf cards get their back faces too (they are one
/// sided). `None` for a mesh without triangles.
#[must_use]
pub fn plant_model(mesh: &PlantMesh) -> Option<LoadedModel> {
    let mut parts = Vec::new();
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for slot in &mesh.slots {
        if slot.indices.len() < 3 || slot.positions.is_empty() {
            continue;
        }
        let leaf = slot.kind == regia_procgen::plant::SlotKind::Leaf;
        let uv = |i: usize| slot.uvs.get(i).copied().unwrap_or([0.0; 2]);
        let normal = |i: usize| slot.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]);
        let mut vertices: Vec<regia_render::LabVertex> = slot
            .positions
            .iter()
            .enumerate()
            .map(|(i, p)| regia_render::LabVertex::from_pos_normal_uv(*p, normal(i), uv(i)))
            .collect();
        let count = slot.positions.len();
        let mut indices: Vec<u32> = slot
            .indices
            .iter()
            .copied()
            .filter(|i| (*i as usize) < count)
            .collect();
        indices.truncate(indices.len() / 3 * 3);
        if leaf {
            let base = vertices.len() as u32;
            vertices.extend(slot.positions.iter().enumerate().map(|(i, p)| {
                let n = normal(i);
                regia_render::LabVertex::from_pos_normal_uv(*p, [-n[0], -n[1], -n[2]], uv(i))
            }));
            let back: Vec<u32> = indices
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|t| [base + t[0], base + t[2], base + t[1]])
                .collect();
            indices.extend(back);
        }
        for p in &slot.positions {
            for a in 0..3 {
                lo[a] = lo[a].min(p[a]);
                hi[a] = hi[a].max(p[a]);
            }
        }
        let material = regia_render::LabMaterial {
            base_color: if leaf { LEAF_COLOR } else { BARK_COLOR },
            roughness: 0.85,
            metallic: 0.0,
            ..regia_render::LabMaterial::default()
        };
        parts.push(ModelPart {
            mesh: Arc::new(regia_render::CpuMesh { vertices, indices }),
            material: Arc::new(material),
            material_name: slot.material.clone(),
        });
    }
    if parts.is_empty() || !lo.iter().chain(&hi).all(|v| v.is_finite()) {
        return None;
    }
    Some(LoadedModel {
        parts,
        collision_parts: Vec::new(),
        aabb_min: regia_core::math::Vec3::from_array(lo),
        aabb_max: regia_core::math::Vec3::from_array(hi),
    })
}

/// The batch thumbnail renderer the lit plant thumbnails share (made on first use, per device).
#[derive(Resource, Default)]
pub struct PlantThumbRenderer(std::sync::Mutex<Option<crate::thumbnails::ThumbnailRenderer>>);

/// Renders the lit thumbnails the strip and the shown variant miss, when the world has a render
/// device: all of them with `budget` `None`, else at most `budget`. Returns how many were drawn.
pub fn render_lit(world: &mut World, budget: Option<usize>) -> usize {
    let Some(device) = world.get_resource::<regia_render::RenderDevice>().cloned() else {
        return 0;
    };
    let wanted: Vec<(u32, Arc<LoadedModel>)> = {
        let Some(p) = world.get_resource::<PlantPreview>() else {
            return 0;
        };
        let mut seeds = p.strip_seeds();
        if !seeds.contains(&p.seed) {
            seeds.insert(0, p.seed);
        }
        seeds
            .into_iter()
            .filter(|s| !p.lit.contains_key(s))
            .filter_map(|s| p.models.get(&s).map(|m| (s, m.clone())))
            .take(budget.unwrap_or(usize::MAX))
            .collect()
    };
    if wanted.is_empty() {
        return 0;
    }
    let _span = tracing::info_span!(target: "regia_editor", "procgen_plant_lit").entered();
    let jobs: Vec<crate::thumbnails::ThumbJob> = wanted
        .iter()
        .map(|(_, m)| crate::thumbnails::ThumbJob::model(m.clone(), LIT_SIZE))
        .collect();
    let images = {
        let holder = world.get_resource_or_insert_with(PlantThumbRenderer::default);
        let mut guard = holder.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard
            .as_ref()
            .is_none_or(|r| r.device().device_id() != device.device_id())
        {
            *guard = Some(crate::thumbnails::ThumbnailRenderer::new(&device));
        }
        guard.as_mut().map(|r| r.render(&jobs)).unwrap_or_default()
    };
    let mut p = world.resource_mut::<PlantPreview>();
    let mut drawn = 0;
    for ((seed, _), image) in wanted.into_iter().zip(images) {
        if image.size[0] == 0 || image.size[1] == 0 {
            continue;
        }
        p.serial += 1;
        let serial = p.serial;
        p.lit.insert(
            seed,
            LitThumb {
                image: Arc::new(image),
                serial,
            },
        );
        p.lit_renders += 1;
        drawn += 1;
    }
    drawn
}

/// One in-memory run of the graph for `seed`: the variant's stats and drawing, and its mesh as a
/// model when the shown node makes one.
fn run_variant(
    world: &World,
    asset: &str,
    text: &str,
    seed: u32,
    node: Option<u32>,
) -> (PlantVariant, Option<LoadedModel>) {
    let mut out = PlantVariant {
        seed,
        node,
        plants: 0,
        nodes: 0,
        height_m: 0.0,
        crown_width_m: 0.0,
        triangles: 0,
        bounds: ([0.0; 3], [0.0; 3]),
        segments: Vec::new(),
        error: None,
    };
    let fail = |mut v: PlantVariant, code: &str, message: String| {
        v.error = Some(format!("{code}: {message}"));
        (v, None)
    };
    let g = match ProcGraph::from_ron(text) {
        Ok(g) => g,
        Err(e) => return fail(out, "procgen_graph_invalid", e.to_string()),
    };
    let registry = world
        .get_resource::<regia_procgen::world::ProcGenRegistry>()
        .map_or_else(
            || Arc::new(regia_procgen::ProcNodeRegistry::with_builtins()),
            |r| r.0.clone(),
        );
    let load = |p: &str| graph_text(world, p);
    let overrides = BTreeMap::new();
    let compiled = match g.compile_full(&registry, &overrides, &load) {
        Ok(c) => c,
        Err(e) => return fail(out, "procgen_graph_invalid", format!("{asset}: {e}")),
    };
    let subgraphs = regia_procgen::graph::collect_subgraphs_with(&g, &overrides, &load);
    let mut inputs =
        RunInputs::new(Arc::new(regia_procgen::exec::StaticSnapshot::default()), seed).with_graphs(subgraphs);
    inputs.keep_debug = true;
    let r = match Evaluator::new().run(&compiled, &inputs, &CancelToken::new()) {
        Ok(r) => r,
        Err(e) => return fail(out, e.code(), e.to_string()),
    };
    let debug = r.debug.unwrap_or_default();
    let plant_payload = |id: u32| {
        debug.get(&id).and_then(|outs| {
            outs.iter()
                .flat_map(|b| &b.0)
                .find(|i| matches!(i.payload, Payload::PlantSkeleton(_) | Payload::PlantMesh(_)))
                .map(|i| i.payload.clone())
        })
    };
    let found = match node {
        Some(id) => plant_payload(id).map(|p| (id, p)),
        None => compiled
            .nodes
            .iter()
            .rev()
            .find_map(|n| plant_payload(n.id).map(|p| (n.id, p))),
    };
    let Some((id, payload)) = found else {
        return fail(
            out,
            "procgen_unknown_node",
            "no node produced plant data".to_string(),
        );
    };
    out.node = Some(id);
    let (skeleton, mesh): (&PlantSkeleton, Option<&PlantMesh>) = match &payload {
        Payload::PlantSkeleton(s) => (s.as_ref(), None),
        Payload::PlantMesh(m) => (m.skeleton.as_ref(), Some(m.as_ref())),
        _ => return (out, None),
    };
    let model = mesh.and_then(plant_model);
    let stats = regia_procgen::plant::plant_stats(skeleton, mesh);
    out.plants = stats.len();
    out.nodes = skeleton.node_count();
    out.height_m = stats.iter().fold(0.0, |m, s| m.max(s.height_m as f32));
    out.crown_width_m = stats.iter().fold(0.0, |m, s| m.max(s.crown_width_m as f32));
    out.triangles = mesh.map_or(0, PlantMesh::triangles);
    out.bounds = skeleton.bounds();
    let total: usize = skeleton.plants.iter().map(|p| p.nodes.len()).sum();
    let step = total.div_ceil(MAX_SEGMENTS).max(1);
    let f = |p: [f64; 3]| [p[0] as f32, p[1] as f32, p[2] as f32];
    let mut k = 0usize;
    for plant in &skeleton.plants {
        for n in &plant.nodes {
            k += 1;
            if !k.is_multiple_of(step) {
                continue;
            }
            if let Some(parent) = n.parent.and_then(|i| plant.nodes.get(i as usize)) {
                out.segments
                    .push((f(parent.position), f(n.position), n.radius as f32));
            }
        }
    }
    (out, model)
}

/// Builds the strip's missing variants (and the plant's seed when it is outside the strip): all of
/// them with `budget` `None`, else at most `budget` runs. Returns the runs made. Variants rebuild
/// when the graph text or the node changes.
pub fn build(world: &mut World, budget: Option<usize>) -> usize {
    let Some(asset) = world.get_resource::<PlantPreview>().and_then(|p| p.asset.clone()) else {
        return 0;
    };
    let _span = tracing::info_span!(target: "regia_editor", "procgen_plant_preview").entered();
    let text = graph_text(world, &asset).unwrap_or_default();
    let node = world.get_resource::<PlantPreview>().and_then(|p| p.node);
    let key = (asset.clone(), text_hash(&text), node);
    let wanted: Vec<u32> = {
        let mut p = world.resource_mut::<PlantPreview>();
        if p.key.as_ref() != Some(&key) {
            p.variants.clear();
            p.models.clear();
            p.lit.clear();
            p.key = Some(key);
        }
        let mut seeds = p.strip_seeds();
        if !seeds.contains(&p.seed) {
            seeds.insert(0, p.seed);
        }
        seeds
            .into_iter()
            .filter(|s| !p.variants.contains_key(s))
            .collect()
    };
    let mut runs = 0;
    for seed in wanted {
        if budget.is_some_and(|b| runs >= b) {
            break;
        }
        let (v, model) = run_variant(world, &asset, &text, seed, node);
        runs += 1;
        let mut p = world.resource_mut::<PlantPreview>();
        p.builds += 1;
        p.variants.insert(seed, v);
        p.lit.remove(&seed);
        match model {
            Some(m) => {
                p.models.insert(seed, Arc::new(m));
            }
            None => {
                p.models.remove(&seed);
            }
        }
    }
    runs
}

/// One editor tick: follows the open document (a Plant graph previews itself, on the selected
/// node when it has plant data) and builds at most one variant.
pub(crate) fn tick(world: &mut World) {
    let open = world.get_resource::<ProcGraphDoc>().and_then(|d| {
        let path = d.path.clone()?;
        (d.graph.flags.domain == regia_procgen::nodes::Domain::Plant).then(|| {
            let node = (d.selection.len() == 1)
                .then(|| d.selection.iter().next().copied())
                .flatten();
            (path, node)
        })
    });
    if let Some((path, node)) = open {
        let (same_asset, same_node) = world.get_resource::<PlantPreview>().map_or((false, false), |p| {
            (p.asset.as_deref() == Some(path.as_str()), p.node == node)
        });
        if !(same_asset && same_node) {
            let first = world.get_resource::<PlantPreview>().map(|p| p.first_seed);
            let _ = set_asset(world, &path, first, node);
        }
    }
    // One variant run, else one lit thumbnail per tick (never both: each can take a frame).
    if build(world, Some(1)) == 0 {
        render_lit(world, Some(1));
    }
}

/// A preview image of a variant drawn into `rect` (side view, X right and Y up, fitted).
fn paint_variant(painter: &egui::Painter, rect: egui::Rect, v: &PlantVariant, selected: bool) {
    let bg = if selected {
        egui::Color32::from_rgb(52, 58, 70)
    } else {
        egui::Color32::from_rgb(30, 32, 38)
    };
    painter.rect_filled(rect, 4.0, bg);
    if let Some(err) = &v.error {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            err.split(':').next().unwrap_or("error"),
            egui::FontId::proportional(10.0),
            egui::Color32::from_rgb(230, 100, 80),
        );
        return;
    }
    let (lo, hi) = v.bounds;
    let w = (hi[0] - lo[0]).max(1e-3);
    let h = (hi[1] - lo[1]).max(1e-3);
    let inner = rect.shrink(6.0);
    let scale = (inner.width() / w).min(inner.height() / h);
    let cx = (lo[0] + hi[0]) * 0.5;
    let to_screen = |p: [f32; 3]| {
        egui::pos2(
            inner.center().x + (p[0] - cx) * scale,
            inner.bottom() - (p[1] - lo[1]) * scale,
        )
    };
    let bark = egui::Color32::from_rgb(170, 130, 90);
    for (a, b, r) in &v.segments {
        let width = (r * 2.0 * scale).clamp(0.5, 6.0);
        painter.line_segment([to_screen(*a), to_screen(*b)], egui::Stroke::new(width, bark));
    }
}

/// The egui texture of a lit thumbnail, kept in the context's temp data per seed and replaced when
/// the picture's serial changes.
fn lit_texture(ctx: &egui::Context, seed: u32, lit: &LitThumb) -> egui::TextureHandle {
    let id = egui::Id::new(("procgen_plant_lit", seed));
    if let Some((serial, texture)) = ctx.data(|d| d.get_temp::<(u64, egui::TextureHandle)>(id))
        && serial == lit.serial
    {
        return texture;
    }
    let texture = ctx.load_texture(
        format!("procgen_plant_lit_{seed}"),
        (*lit.image).clone(),
        egui::TextureOptions::LINEAR,
    );
    ctx.data_mut(|d| d.insert_temp(id, (lit.serial, texture.clone())));
    texture
}

/// Draws a variant into `rect`: its lit thumbnail when there is one, else the skeleton strokes.
fn draw_variant(ui: &egui::Ui, p: &PlantPreview, rect: egui::Rect, v: &PlantVariant, selected: bool) {
    match p.lit.get(&v.seed).filter(|_| v.error.is_none()) {
        Some(lit) => {
            let texture = lit_texture(ui.ctx(), v.seed, lit);
            let painter = ui.painter();
            painter.image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            if selected {
                painter.rect_stroke(
                    rect,
                    4.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 160, 230)),
                    egui::StrokeKind::Inside,
                );
            }
        }
        None => paint_variant(ui.painter(), rect, v, selected),
    }
}

/// The asset preview viewport and the variants strip for the previewed Plant graph. A click on a
/// variant pushes the plant's seed into `clicked`.
pub(crate) fn plant_ui(ui: &mut egui::Ui, world: &World, clicked: &mut Option<u32>) {
    let Some(p) = world.get_resource::<PlantPreview>() else {
        return;
    };
    if p.asset.is_none() {
        return;
    }
    ui.horizontal(|ui| {
        let size = egui::vec2(220.0, 220.0);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        match p.shown() {
            Some(v) => draw_variant(ui, p, rect, v, false),
            None => {
                ui.painter().rect_filled(rect, 4.0, egui::Color32::from_rgb(30, 32, 38));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "building…",
                    egui::FontId::proportional(11.0),
                    egui::Color32::GRAY,
                );
            }
        }
        ui.vertical(|ui| {
            if let Some(v) = p.shown() {
                ui.label(format!(
                    "seed {} · node {} · {} plant(s) · {} nodes · {:.2} m tall · crown {:.2} m · {} triangles",
                    v.seed,
                    v.node.map_or_else(|| "-".to_string(), |n| n.to_string()),
                    v.plants,
                    v.nodes,
                    v.height_m,
                    v.crown_width_m,
                    v.triangles
                ));
            }
            ui.label("Variants");
            ui.horizontal(|ui| {
                for seed in p.strip_seeds() {
                    let (rect, response) = ui.allocate_exact_size(egui::vec2(96.0, 110.0), egui::Sense::click());
                    let thumb = egui::Rect::from_min_size(rect.min, egui::vec2(96.0, 96.0));
                    match p.variants.get(&seed) {
                        Some(v) => draw_variant(ui, p, thumb, v, seed == p.seed),
                        None => {
                            ui.painter().rect_filled(thumb, 4.0, egui::Color32::from_rgb(30, 32, 38));
                        }
                    }
                    ui.painter().text(
                        egui::pos2(rect.center().x, rect.bottom()),
                        egui::Align2::CENTER_BOTTOM,
                        format!("{seed}"),
                        egui::FontId::proportional(10.0),
                        egui::Color32::LIGHT_GRAY,
                    );
                    if response.on_hover_text("Use this seed").clicked() {
                        *clicked = Some(seed);
                    }
                }
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_strip_shows_six_consecutive_seeds_and_wraps() {
        let p = PlantPreview {
            first_seed: u32::MAX - 1,
            ..PlantPreview::default()
        };
        assert_eq!(p.strip_seeds(), vec![u32::MAX - 1, u32::MAX, 0, 1, 2, 3]);
    }
}
