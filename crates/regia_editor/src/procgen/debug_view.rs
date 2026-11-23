// SPDX-License-Identifier: MPL-2.0
//! Live preview and debugging (spec 67 § 3): the preview target and its debounced re-runs, the
//! debug-data cap, node visualization in the viewport, the data table (inspect) and isolate.
//!
//! Time is counted in editor ticks ([`tick`] runs once per editor frame), never wall clock: the
//! quiet period `procgen_refresh_ms` becomes a tick count at 60 Hz, so tests step ticks.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use regia_ecs::prelude::*;
use regia_procgen::data::{AttrColumn, Payload, PointSet, ShapeData};
use regia_procgen::world::{DebugOutputs, ProcGenRunState, ProcGenSettings, ProcGenState, ProcGenerator};
use regia_procgen::{Bundle, DataItem};

use super::doc::{DocError, ProcGraphDoc};

/// Editor ticks per second the quiet period is converted with.
pub const TICKS_PER_SECOND: u32 = 60;
/// Most points the viewport draws for visualized nodes; more are drawn as a uniform subsample.
pub const MAX_DRAWN_POINTS: usize = 200_000;
/// Debug data kept per generator (spec 67 § 3: 256 MiB, oldest nodes dropped first).
pub const DEBUG_CAP_BYTES: usize = 256 << 20;
/// Rows per data-table page.
pub const PAGE_ROWS: usize = 1_000;

/// Editor preferences of the procedural tools (not a frozen format).
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct ProcEditorPrefs {
    /// Quiet period before a preview re-run, milliseconds (default 150).
    pub procgen_refresh_ms: u32,
    /// Most tool-session re-runs per second (default 10).
    pub tool_refresh_hz: u32,
    /// Show the starter picker for "New procedural graph" (default true).
    pub starter_picker: bool,
    /// Placement tools edit only the selected entity (an unfit selection leaves the tool inert).
    pub edit_selected_only: bool,
    /// Per-tool hit filters (spec 67 § 5.3), by tool kind name.
    pub hit_filters: BTreeMap<String, super::tools::HitFilter>,
}

impl Default for ProcEditorPrefs {
    fn default() -> Self {
        Self {
            procgen_refresh_ms: 150,
            tool_refresh_hz: 10,
            starter_picker: true,
            edit_selected_only: false,
            hit_filters: BTreeMap::new(),
        }
    }
}

impl ProcEditorPrefs {
    /// The quiet period in editor ticks (at least one).
    #[must_use]
    pub fn refresh_ticks(&self) -> u32 {
        (self.procgen_refresh_ms * TICKS_PER_SECOND).div_ceil(1000).max(1)
    }
}

/// How a visualized node colors its points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorBy {
    /// Density, black (0) to white (1).
    Density,
    /// A scalar attribute as a ramp, or a name attribute as categorical colors.
    Attr(String),
    /// Categorical colors by the item's first tag.
    Tags,
}

/// How a visualized node draws its points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisForm {
    /// A small cross per point.
    Points,
    /// The oriented bounds of each point.
    Boxes,
    /// The point's local axes.
    Axes,
}

/// One visualized node's style.
#[derive(Clone, Debug, PartialEq)]
pub struct VisualizeStyle {
    /// Coloring.
    pub color_by: ColorBy,
    /// Form.
    pub form: VisForm,
    /// The node's own color (shapes, and the tint of `Tags`).
    pub color: [f32; 4],
}

/// One drawn curve or outline segment: `(node, from, to, linear RGBA)`.
pub type CurveSegment = (u32, [f32; 3], [f32; 3], [f32; 4]);

/// One visualized point the viewport draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisPoint {
    /// The node it belongs to.
    pub node: u32,
    /// World position.
    pub position: [f32; 3],
    /// Rotation `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// Scaled local bounds `(min, max)`.
    pub bounds: ([f32; 3], [f32; 3]),
    /// Linear RGBA.
    pub color: [f32; 4],
}

/// The preview's state (world resource).
#[derive(Resource, Debug, Default)]
pub struct ProcPreview {
    /// The generator the panel previews on.
    pub target: Option<Entity>,
    /// The last target (the default when the selection has no generator).
    pub last_used: Option<Entity>,
    /// The target was chosen explicitly (the "Preview on" picker or `editor.procgen_preview`), so it
    /// no longer follows the default rule (the selection's generator, else the last used).
    pub pinned_target: bool,
    /// Ticks left in the quiet period before the next preview run (`None` = nothing pending).
    pub pending: Option<u32>,
    /// Preview runs requested (tests count these).
    pub runs: u64,
    /// In-flight runs a newer change cancelled.
    pub cancelled: u64,
    /// Visualized nodes and their styles.
    pub visualize: BTreeMap<u32, VisualizeStyle>,
    /// Points drawn this tick (after the cap).
    pub drawn: Vec<VisPoint>,
    /// Shapes drawn this tick (wire boxes), by node.
    pub drawn_shapes: Vec<(u32, ShapeData)>,
    /// Curves, outlines and mask grids drawn this tick.
    pub drawn_curves: Vec<CurveSegment>,
    /// Points the visualized nodes hold before the cap.
    pub total_points: usize,
    /// The draw is a uniform subsample (more than [`MAX_DRAWN_POINTS`]).
    pub subsampled: bool,
    /// Nodes whose debug data was dropped by the 256 MiB cap in the last run.
    pub dropped_debug: Vec<u32>,
    /// The isolate run's node and result (the generator's output is not replaced).
    pub isolate: Option<Isolate>,
    /// Preview quality tier fed to `flow.quality` (`None` = the running tier).
    pub tier: Option<regia_render::quality::QualityTier>,
    /// Editor camera as an anchor for `Anchored` generators (spec 66).
    pub generate_around_view: bool,
    /// Tiles overlay (spec 66 debug view).
    pub tiles_overlay: bool,
    /// Draw every `Shape` volume and `Spline` curve, not only the selected ones.
    pub show_volumes: bool,
    /// Outline every `BiomeArea` (spec 65 § 7); `preview_alone` areas in a second color.
    pub biome_overlay: bool,
    /// The other biome overlay views (spec 65 § 7): claim tint, blend bands, footprints and the
    /// filter view, each switched on its own.
    pub biome_views: super::biome_overlay::BiomeViews,
    /// The run id whose debug data was last trimmed.
    trimmed_run: u64,
    /// What the drawn lists were built from (debug data, styles, document revision); the tick
    /// rebuilds only when it changes.
    vis_key: Option<(usize, String, u64)>,
    /// Times the drawn lists were rebuilt (tests: a quiet tick does not rebuild).
    pub rebuilds: u64,
    /// Ticks since the target was last re-run by a tool session (rate limit).
    pub(crate) tool_ticks: u32,
}

/// "Preview up to here": the node and its run's debug data.
#[derive(Clone, Debug)]
pub struct Isolate {
    /// The isolated node.
    pub node: u32,
    /// Every node output of the isolated run.
    pub debug: Arc<DebugOutputs>,
    /// The failure, when the isolated run failed.
    pub error: Option<(Option<u32>, String, String)>,
}

/// Binds the preview target (a generator entity) and runs it after the quiet period.
///
/// # Errors
///
/// `procgen_not_generator`.
pub fn set_preview(world: &mut World, target: Option<Entity>) -> Result<(), DocError> {
    if let Some(e) = target
        && world.get::<ProcGenerator>(e).is_none()
    {
        return Err(DocError::new(
            "procgen_not_generator",
            format!("entity {} has no ProcGenerator", e.to_bits()),
        ));
    }
    let mut preview = world.get_resource_or_insert_with(ProcPreview::default);
    preview.target = target;
    if target.is_some() {
        preview.last_used = target;
    }
    preview.isolate = None;
    // Preview runs keep every node's output (spec 67 § 3); editor-only world.
    if let Some(mut settings) = world.get_resource_mut::<ProcGenSettings>() {
        settings.keep_debug = target.is_some();
    }
    if target.is_some() {
        schedule_preview(world);
    }
    Ok(())
}

/// Binds the preview target as an explicit choice (the "Preview on" picker, `editor.procgen_preview`):
/// it stays until the next explicit choice. `follow: true` returns to the default rule instead.
///
/// # Errors
///
/// `procgen_not_generator`.
pub fn choose_preview(world: &mut World, target: Option<Entity>, follow: bool) -> Result<(), DocError> {
    if follow {
        world
            .get_resource_or_insert_with(ProcPreview::default)
            .pinned_target = false;
        follow_default_target(world);
        return Ok(());
    }
    set_preview(world, target)?;
    world.resource_mut::<ProcPreview>().pinned_target = true;
    Ok(())
}

/// The default preview target (spec 67 § 3): the selected entity's generator, else the last one
/// used (while it still exists and has a generator).
#[must_use]
pub fn default_target(world: &World) -> Option<Entity> {
    let has_generator = |e: Entity| world.get::<ProcGenerator>(e).is_some();
    let selected = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
        .filter(|e| has_generator(*e));
    selected.or_else(|| {
        world
            .get_resource::<ProcPreview>()
            .and_then(|p| p.last_used)
            .filter(|e| world.get_entity(*e).is_ok() && has_generator(*e))
    })
}

/// While a graph is open and the target was not chosen explicitly, keeps the preview on
/// [`default_target`]. Called once per editor tick.
pub fn follow_default_target(world: &mut World) {
    let open = world
        .get_resource::<ProcGraphDoc>()
        .is_some_and(|d| d.path.is_some());
    let preview = world.get_resource::<ProcPreview>();
    if !open || preview.is_some_and(|p| p.pinned_target) {
        return;
    }
    let current = preview.and_then(|p| p.target);
    let Some(wanted) = default_target(world) else {
        return;
    };
    if current != Some(wanted) {
        let _ = set_preview(world, Some(wanted));
    }
}

/// The preview quality tier fed to `flow.quality` (spec 63 § 10): sets the generators' running
/// tier (`None` = High) and re-runs the preview after the quiet period.
pub fn set_tier(world: &mut World, tier: Option<regia_render::quality::QualityTier>) {
    world.get_resource_or_insert_with(ProcPreview::default).tier = tier;
    if let Some(mut settings) = world.get_resource_mut::<ProcGenSettings>() {
        settings.running_tier = tier.unwrap_or(regia_render::quality::QualityTier::High);
    }
    schedule_preview(world);
}

/// Starts (or restarts) the quiet period after an edit and cancels an in-flight preview run.
pub fn schedule_preview(world: &mut World) {
    let ticks = world.get_resource::<ProcEditorPrefs>().map_or_else(
        || ProcEditorPrefs::default().refresh_ticks(),
        ProcEditorPrefs::refresh_ticks,
    );
    let Some(target) = world.get_resource::<ProcPreview>().and_then(|p| p.target) else {
        return;
    };
    let in_flight = world
        .get::<ProcGenState>(target)
        .is_some_and(|s| s.state.in_flight());
    if in_flight && regia_procgen::world::cancel(world, target).is_some() {
        world.resource_mut::<ProcPreview>().cancelled += 1;
    }
    let mut preview = world.resource_mut::<ProcPreview>();
    preview.pending = Some(ticks);
    preview.isolate = None;
}

/// Whether the open document is valid and is (or is the graph of) the preview target's graph.
fn doc_runs(world: &World) -> bool {
    world
        .get_resource::<ProcGraphDoc>()
        .is_none_or(|d| d.errors.is_empty())
}

/// One editor tick: counts down the quiet period and requests the preview run, trims debug data,
/// rebuilds the visualization and draws it.
#[tracing::instrument(target = "regia_editor", skip_all, name = "procgen_preview_tick")]
pub fn tick(world: &mut World) {
    let Some(mut preview) = world.get_resource_mut::<ProcPreview>() else {
        return;
    };
    preview.tool_ticks = preview.tool_ticks.saturating_add(1);
    let fire = match preview.pending {
        Some(0) | Some(1) => {
            preview.pending = None;
            true
        }
        Some(n) => {
            preview.pending = Some(n - 1);
            false
        }
        None => false,
    };
    let target = preview.target;
    if let Some(e) = target {
        if world.get_entity(e).is_err() {
            world.resource_mut::<ProcPreview>().target = None;
        } else if fire && doc_runs(world) && regia_procgen::world::request_run(world, e) {
            world.resource_mut::<ProcPreview>().runs += 1;
        }
    }
    trim_debug(world);
    let key = visualization_key(world);
    if world
        .get_resource::<ProcPreview>()
        .is_some_and(|p| p.vis_key != Some(key.clone()))
    {
        rebuild_visualization(world);
    }
    draw_visualization(world);
}

/// Bytes a bundle holds (an estimate: element columns and attribute values).
fn bundle_bytes(b: &Bundle) -> usize {
    b.0.iter()
        .map(|i| match &i.payload {
            Payload::Points(p) => {
                p.len() * (12 + 16 + 12 + 16 + 4 + 4 + 24 + 4) + p.attrs.len() * p.len() * 8
            }
            Payload::Table(t) => t.rows * t.columns.len() * 8,
            _ => 64,
        })
        .sum()
}

/// Applies the debug cap to the preview target's last run: nodes are dropped oldest first (lowest
/// in evaluation order, which the run report lists) until the rest fit.
fn trim_debug(world: &mut World) {
    let Some(target) = world.get_resource::<ProcPreview>().and_then(|p| p.target) else {
        return;
    };
    let Some(st) = world.get::<ProcGenState>(target) else {
        return;
    };
    if st.run_id == world.resource::<ProcPreview>().trimmed_run || st.state != ProcGenRunState::Ready {
        return;
    }
    let run_id = st.run_id;
    let Some(debug) = st.debug.clone() else {
        world.resource_mut::<ProcPreview>().trimmed_run = run_id;
        return;
    };
    let order: Vec<u32> = st
        .report
        .as_ref()
        .map(|r| r.nodes.iter().map(|n| n.id).collect())
        .unwrap_or_else(|| debug.keys().copied().collect());
    let (kept, dropped) = cap_debug(&debug, &order, DEBUG_CAP_BYTES);
    if !dropped.is_empty()
        && let Some(mut st) = world.get_mut::<ProcGenState>(target)
    {
        st.debug = Some(Arc::new(kept));
    }
    let mut preview = world.resource_mut::<ProcPreview>();
    preview.trimmed_run = run_id;
    preview.dropped_debug = dropped;
}

/// Keeps the newest nodes' outputs (by `order`, oldest first) within `cap` bytes; returns the kept
/// map and the dropped node ids.
#[must_use]
pub fn cap_debug(debug: &DebugOutputs, order: &[u32], cap: usize) -> (DebugOutputs, Vec<u32>) {
    let size = |id: &u32| {
        debug
            .get(id)
            .map_or(0, |v| v.iter().map(bundle_bytes).sum::<usize>())
    };
    let mut total: usize = debug.keys().map(size).sum();
    let mut dropped = Vec::new();
    for id in order {
        if total <= cap {
            break;
        }
        if debug.contains_key(id) {
            total -= size(id);
            dropped.push(*id);
        }
    }
    let kept = debug
        .iter()
        .filter(|(k, _)| !dropped.contains(k))
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    (kept, dropped)
}

/// The debug outputs the panel shows: the isolate run when isolating, else the preview target's.
#[must_use]
pub fn debug_outputs(world: &World) -> Option<Arc<DebugOutputs>> {
    let preview = world.get_resource::<ProcPreview>()?;
    if let Some(iso) = &preview.isolate {
        return Some(iso.debug.clone());
    }
    world.get::<ProcGenState>(preview.target?)?.debug.clone()
}

/// Turns visualization of `node` on or off; returns the visualized node ids.
pub fn visualize(world: &mut World, node: u32, style: Option<VisualizeStyle>) -> Vec<u32> {
    let mut preview = world.get_resource_or_insert_with(ProcPreview::default);
    match style {
        Some(s) => {
            preview.visualize.insert(node, s);
        }
        None => {
            preview.visualize.remove(&node);
        }
    }
    let ids = preview.visualize.keys().copied().collect();
    rebuild_visualization(world);
    ids
}

/// A distinct color per index (a colorblind-safe set, then repeats).
#[must_use]
pub fn categorical(i: usize) -> [f32; 4] {
    super::panel::KIND_COLORS[i % super::panel::KIND_COLORS.len()]
}

fn ramp(t: f64) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0) as f32;
    [t, t, t, 1.0]
}

fn point_colors(item: &DataItem, set: &PointSet, style: &VisualizeStyle) -> Vec<[f32; 4]> {
    match &style.color_by {
        ColorBy::Density => set.density.iter().map(|d| ramp(f64::from(*d))).collect(),
        ColorBy::Tags => {
            let c = item.tags.iter().next().map_or(style.color, |t| {
                categorical(
                    t.bytes()
                        .fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize)),
                )
            });
            vec![c; set.len()]
        }
        ColorBy::Attr(name) => match set.attrs.get(name) {
            Some(AttrColumn::Name(names)) => {
                let mut ids: BTreeMap<&str, usize> = BTreeMap::new();
                for n in names {
                    let next = ids.len();
                    ids.entry(n.as_str()).or_insert(next);
                }
                names.iter().map(|n| categorical(ids[n.as_str()])).collect()
            }
            Some(col) => {
                let v: Vec<f64> = (0..set.len()).map(|i| scalar_of(col, i)).collect();
                let (lo, hi) = v.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), x| {
                    (a.min(*x), b.max(*x))
                });
                let span = if hi > lo { hi - lo } else { 1.0 };
                v.iter().map(|x| ramp((x - lo) / span)).collect()
            }
            None => vec![style.color; set.len()],
        },
    }
}

fn scalar_of(col: &AttrColumn, i: usize) -> f64 {
    match col {
        AttrColumn::Bool(v) => v.get(i).map_or(0.0, |b| f64::from(u8::from(*b))),
        AttrColumn::Int(v) => v.get(i).map_or(0.0, |x| *x as f64),
        AttrColumn::Float(v) => v.get(i).copied().unwrap_or(0.0),
        AttrColumn::Vec2(v) => v.get(i).map_or(0.0, |x| x[0]),
        AttrColumn::Vec3(v) => v.get(i).map_or(0.0, |x| x[0]),
        AttrColumn::Vec4(v) => v.get(i).map_or(0.0, |x| x[0]),
        _ => 0.0,
    }
}

/// The nodes drawn: visualized ones plus every `debug.show` node of the open graph.
fn drawn_nodes(world: &World) -> BTreeMap<u32, VisualizeStyle> {
    let mut nodes = world
        .get_resource::<ProcPreview>()
        .map(|p| p.visualize.clone())
        .unwrap_or_default();
    if let Some(doc) = world.get_resource::<ProcGraphDoc>() {
        for (id, n) in &doc.graph.nodes {
            if n.ty == "debug.show" && !nodes.contains_key(id) {
                nodes.insert(
                    *id,
                    VisualizeStyle {
                        color_by: ColorBy::Tags,
                        form: VisForm::Points,
                        color: [1.0, 0.0, 1.0, 1.0],
                    },
                );
            }
        }
    }
    nodes
}

/// Rebuilds [`ProcPreview::drawn`] from the debug outputs (capped at [`MAX_DRAWN_POINTS`] with a
/// uniform subsample).
pub fn rebuild_visualization(world: &mut World) {
    let _span = tracing::info_span!(target: "regia_editor", "procgen_visualize_rebuild").entered();
    let key = visualization_key(world);
    let nodes = drawn_nodes(world);
    let debug = debug_outputs(world);
    let mut all: Vec<VisPoint> = Vec::new();
    let mut shapes = Vec::new();
    let mut curves = Vec::new();
    if let Some(debug) = debug {
        for (node, style) in &nodes {
            let Some(bundles) = debug.get(node) else {
                continue;
            };
            for item in bundles.iter().flat_map(|b| b.0.iter()) {
                match &item.payload {
                    Payload::Points(set) => {
                        let colors = point_colors(item, set, style);
                        for i in 0..set.len() {
                            let s = set.scale[i];
                            let (lo, hi) = (set.bounds_min[i], set.bounds_max[i]);
                            all.push(VisPoint {
                                node: *node,
                                position: set.position[i],
                                rotation: set.rotation[i],
                                bounds: (
                                    [lo[0] * s[0], lo[1] * s[1], lo[2] * s[2]],
                                    [hi[0] * s[0], hi[1] * s[1], hi[2] * s[2]],
                                ),
                                color: colors.get(i).copied().unwrap_or(style.color),
                            });
                        }
                    }
                    Payload::Shape(shape) => shapes.push((*node, *shape)),
                    Payload::Mask(mask) => mask_lines(world, *node, mask, style.color, &mut curves),
                    _ => {}
                }
            }
        }
    }
    let total = all.len();
    let drawn = subsample(all, MAX_DRAWN_POINTS);
    let mut preview = world.get_resource_or_insert_with(ProcPreview::default);
    preview.total_points = total;
    preview.subsampled = total > MAX_DRAWN_POINTS;
    preview.drawn = drawn;
    preview.drawn_shapes = shapes;
    preview.drawn_curves = curves;
    preview.vis_key = Some(key);
    preview.rebuilds += 1;
}

/// The inputs of the drawn lists: the debug data shown (by pointer), the drawn nodes and styles,
/// and the document revision (`debug.show` nodes come from the document).
fn visualization_key(world: &World) -> (usize, String, u64) {
    let data = debug_outputs(world).map_or(0, |d| Arc::as_ptr(&d) as usize);
    let styles = format!("{:?}", drawn_nodes(world));
    let rev = world.get_resource::<ProcGraphDoc>().map_or(0, |d| d.revision);
    (data, styles, rev)
}

/// The mask grid drawn per mask item: at most this many cells per side.
const MASK_GRID: u32 = 48;

/// A mask as a grid of ground crosses colored by value (dark to the node color), at the terrain
/// height when the level has terrain, else at y = 0.
fn mask_lines(
    world: &World,
    node: u32,
    mask: &regia_procgen::data::MaskData,
    color: [f32; 4],
    out: &mut Vec<CurveSegment>,
) {
    if mask.width == 0 || mask.height == 0 {
        return;
    }
    let terrain = world
        .iter_entities()
        .find_map(|e| e.get::<regia_terrain::Terrain>().map(|t| &t.map));
    let [x0, z0, x1, z1] = mask.rect;
    let (cols, rows) = (mask.width.clamp(1, MASK_GRID), mask.height.clamp(1, MASK_GRID));
    let (dx, dz) = ((x1 - x0) / cols as f32, (z1 - z0) / rows as f32);
    let half = 0.35 * dx.min(dz).abs();
    for r in 0..rows {
        for c in 0..cols {
            let (x, z) = (x0 + (c as f32 + 0.5) * dx, z0 + (r as f32 + 0.5) * dz);
            let (mc, mr) = (
                ((c as f32 + 0.5) / cols as f32 * mask.width as f32) as usize,
                ((r as f32 + 0.5) / rows as f32 * mask.height as f32) as usize,
            );
            let v = mask
                .values
                .get(mr.min(mask.height as usize - 1) * mask.width as usize + mc.min(mask.width as usize - 1))
                .copied()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let y = terrain.map_or(0.0, |t| t.sample_bilinear(x, z)) + 0.05;
            let tint = [color[0] * v, color[1] * v, color[2] * v, 0.5];
            out.push((node, [x - half, y, z], [x + half, y, z], tint));
            out.push((node, [x, y, z - half], [x, y, z + half], tint));
        }
    }
}

/// At most `cap` elements, evenly spread over the input (deterministic: element `k * len / cap`).
#[must_use]
pub fn subsample<T: Copy>(all: Vec<T>, cap: usize) -> Vec<T> {
    if all.len() <= cap {
        return all;
    }
    let len = all.len();
    (0..cap).map(|k| all[k * len / cap]).collect()
}

/// Pushes the visualization into the viewport's debug lines (when the world has a
/// [`regia_render::DebugDraw`]).
fn draw_visualization(world: &mut World) {
    let Some(preview) = world.get_resource::<ProcPreview>() else {
        return;
    };
    if preview.drawn.is_empty() && preview.drawn_shapes.is_empty() && preview.drawn_curves.is_empty() {
        return;
    }
    let forms: BTreeMap<u32, VisForm> = preview.visualize.iter().map(|(k, v)| (*k, v.form)).collect();
    let selected_row = world
        .get_resource::<super::ProcInspect>()
        .and_then(|i| i.selected_point());
    let mut lines = Vec::with_capacity(preview.drawn.len() * 3);
    for p in &preview.drawn {
        let form = forms.get(&p.node).copied().unwrap_or(VisForm::Points);
        point_lines(p, form, &mut lines);
    }
    if let Some((pos, _)) = selected_row {
        let h = 0.6;
        lines.push((
            [pos[0], pos[1], pos[2]],
            [pos[0], pos[1] + 2.0 * h, pos[2]],
            [1.0, 0.85, 0.0, 1.0],
        ));
    }
    for (_, shape) in &preview.drawn_shapes {
        let (lo, hi) = shape.world_aabb();
        box_lines(lo, hi, [0.2, 0.8, 1.0, 1.0], &mut lines);
    }
    for (_, a, b, c) in &preview.drawn_curves {
        lines.push((*a, *b, *c));
    }
    if let Some(mut draw) = world.get_resource_mut::<regia_render::DebugDraw>() {
        for (a, b, c) in lines {
            draw.line(a, b, c);
        }
    }
}

type Line = ([f32; 3], [f32; 3], [f32; 4]);

fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let r = regia_core::math::Quat::from_xyzw(q[0], q[1], q[2], q[3]);
    (r * regia_core::math::Vec3::from(v)).to_array()
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn point_lines(p: &VisPoint, form: VisForm, out: &mut Vec<Line>) {
    match form {
        VisForm::Points => {
            let h = 0.15;
            for axis in [[h, 0.0, 0.0], [0.0, h, 0.0], [0.0, 0.0, h]] {
                let neg = [-axis[0], -axis[1], -axis[2]];
                out.push((add(p.position, neg), add(p.position, axis), p.color));
            }
        }
        VisForm::Axes => {
            for (axis, color) in [
                ([0.5, 0.0, 0.0], [1.0, 0.2, 0.2, 1.0]),
                ([0.0, 0.5, 0.0], [0.2, 1.0, 0.2, 1.0]),
                ([0.0, 0.0, 0.5], [0.2, 0.4, 1.0, 1.0]),
            ] {
                out.push((p.position, add(p.position, rotate(p.rotation, axis)), color));
            }
        }
        VisForm::Boxes => {
            let (lo, hi) = p.bounds;
            let corner = |i: usize| {
                let c = [
                    if i & 1 == 0 { lo[0] } else { hi[0] },
                    if i & 2 == 0 { lo[1] } else { hi[1] },
                    if i & 4 == 0 { lo[2] } else { hi[2] },
                ];
                add(p.position, rotate(p.rotation, c))
            };
            for (a, b) in BOX_EDGES {
                out.push((corner(a), corner(b), p.color));
            }
        }
    }
}

const BOX_EDGES: [(usize, usize); 12] = [
    (0, 1),
    (2, 3),
    (4, 5),
    (6, 7),
    (0, 2),
    (1, 3),
    (4, 6),
    (5, 7),
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7),
];

fn box_lines(lo: [f32; 3], hi: [f32; 3], color: [f32; 4], out: &mut Vec<Line>) {
    let corner = |i: usize| {
        [
            if i & 1 == 0 { lo[0] } else { hi[0] },
            if i & 2 == 0 { lo[1] } else { hi[1] },
            if i & 4 == 0 { lo[2] } else { hi[2] },
        ]
    };
    for (a, b) in BOX_EDGES {
        out.push((corner(a), corner(b), color));
    }
}

/// "Preview up to here" (spec 67 § 3): runs the node and its ancestors for the preview target
/// without replacing its output; `None` ends isolation.
///
/// # Errors
///
/// `procgen_no_preview`, `procgen_not_open`, `procgen_unknown_node`.
pub fn isolate(world: &mut World, node: Option<u32>) -> Result<Option<Isolate>, DocError> {
    let Some(node) = node else {
        if let Some(mut p) = world.get_resource_mut::<ProcPreview>() {
            p.isolate = None;
        }
        rebuild_visualization(world);
        return Ok(None);
    };
    let target = world
        .get_resource::<ProcPreview>()
        .and_then(|p| p.target)
        .ok_or_else(|| DocError::new("procgen_no_preview", "bind a preview target first"))?;
    let doc = world
        .get_resource::<ProcGraphDoc>()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    if !doc.graph.nodes.contains_key(&node) {
        return Err(DocError::new("procgen_unknown_node", format!("no node {node}")));
    }
    let keep = ancestors(&doc.graph, node);
    let mut g = doc.graph.clone();
    let drop: BTreeSet<u32> = g.nodes.keys().filter(|k| !keep.contains(k)).copied().collect();
    super::doc::remove_nodes(&mut g, &drop);
    let result = regia_procgen::world::evaluate_detached(world, target, &g);
    let iso = match result {
        Ok(r) => Isolate {
            node,
            debug: Arc::new(r.debug.unwrap_or_default()),
            error: None,
        },
        Err(e) => Isolate {
            node,
            debug: Arc::new(BTreeMap::new()),
            error: Some(e),
        },
    };
    {
        let mut preview = world.resource_mut::<ProcPreview>();
        preview.isolate = Some(iso.clone());
        preview.visualize.entry(node).or_insert(VisualizeStyle {
            color_by: ColorBy::Density,
            form: VisForm::Points,
            color: [1.0, 1.0, 1.0, 1.0],
        });
    }
    rebuild_visualization(world);
    Ok(Some(iso))
}

/// `node` and every node it reads from (through edges, reroutes included).
#[must_use]
pub fn ancestors(g: &regia_procgen::ProcGraph, node: u32) -> BTreeSet<u32> {
    let mut keep = BTreeSet::from([node]);
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        for e in g.edges.iter().filter(|e| e.to.0 == n) {
            if keep.insert(e.from.0) {
                stack.push(e.from.0);
            }
        }
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mask_draws_a_capped_grid_of_crosses() {
        let world = World::new();
        let mask = regia_procgen::data::MaskData {
            rect: [0.0, 0.0, 10.0, 20.0],
            width: 4,
            height: 100,
            values: (0..400).map(|i| (i % 2) as f32).collect(),
            bilinear: false,
            second: Vec::new(),
        };
        let mut out = Vec::new();
        mask_lines(&world, 7, &mask, [1.0, 1.0, 1.0, 1.0], &mut out);
        assert_eq!(
            out.len(),
            2 * 4 * MASK_GRID as usize,
            "rows capped at {MASK_GRID}"
        );
        assert!(
            out.iter().all(|(n, a, _, _)| *n == 7 && a[1] == 0.05),
            "at y = 0 without terrain"
        );
        let empty = regia_procgen::data::MaskData {
            width: 0,
            values: Vec::new(),
            ..mask
        };
        let mut none = Vec::new();
        mask_lines(&world, 7, &empty, [1.0; 4], &mut none);
        assert!(none.is_empty());
    }
}
