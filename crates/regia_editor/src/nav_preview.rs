// SPDX-License-Identifier: MPL-2.0
//! The navigation preview (lane w9-viewport-flags-nav, ADR-7593): Show ▸ Build navigation
//! preview and `editor.nav_preview { action: "build" | "clear" }`.
//!
//! Show ▸ Navigation draws the world's `regia_nav::NavMesh` resource
//! ([`crate::overlays::navigation_segments`], ADR-7468), which games insert at run time, so in a
//! stock editor it drew nothing (rev-claude-57 outliner finding 2). The preview bakes one from the
//! level in the editor:
//!
//! - **Geometry:** the level's static collision, read by
//!   [`regia_framework::nav_world::level_triangles_in`], the same source the runtime nav world and
//!   the streaming cook take (static box, triangle-mesh and heightfield colliders on an entity with
//!   a static `RigidBody`, terrain surfaces, attached destructible chunks).
//! - **Settings:** the level's own nav settings, `LevelStreaming::nav` (the
//!   `StreamNavCook` the streaming cook bakes with), when the level has them; otherwise
//!   `regia_nav::NavMeshBuildParams::default()`. The reply names which (`settings`).
//! - **Editor-only:** the mesh goes into the `NavMesh` resource the overlay reads, with a
//!   [`NavPreview`] marker resource beside it. `NavMesh` is not reflected, so no level save,
//!   prefab or snapshot carries it, and the preview is no undo step. A `NavMesh` the preview did not
//!   insert (a game's) is never replaced or removed (`nav_preview_game_mesh`).
//!
//! Building turns Show ▸ Navigation on (in the menu and through the method alike), so the bake is
//! visible at once.
//!
//! **The bake is an editor job** (ADR-7664): `nav_preview:<n>` in `agent.job_status`, with the
//! background-task row `nav_preview` (`editor.cancel_task`). The editor thread ([`start_build`])
//! runs the size check and copies the level's nav sources out of the world
//! ([`regia_framework::nav_world::LevelNavSources`], ADR-8594: terrain heights and mesh collider
//! vertices are memory copies); the worker thread makes the triangles from that copy (the gather,
//! which on a big terrain is millions of triangles) and runs the voxel build; [`tick_job`] (every
//! frame) puts the mesh in place and the overlay draws it from the next frame. One bake runs at a
//! time (`nav_preview_busy`). A cancel (`editor.nav_preview { action: "cancel" }`,
//! `editor.cancel_job`, `clear`, a level change) ends the job `cancelled` at once and keeps the
//! preview that was there. The worker checks for the cancel before each source of the gather and
//! before the build, and stops there; a build already running cannot stop part-way, so it finishes
//! in the background and its mesh is dropped. A bake whose level is no longer open when it ends is
//! dropped too.
//!
//! **Size check:** before any triangle is gathered, the XZ rectangle of the terrains and of the
//! static box, triangle-mesh and heightfield colliders that feed the bake is held to the grid
//! budget one build allocates (`nav_preview_too_large`). Destructible chunks are not in the
//! estimate; the builder's own guard still refuses them.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_framework::nav_world::LevelNavSources;
use regia_nav::{NavAgentParams, NavMesh, NavMeshBuildParams, NavMeshBuilder};
use regia_scene::prelude::{LevelId, LevelManager};
use regia_scene::streaming::{LevelStreaming, StreamNavCook};
use serde_json::{Value, json};

use crate::editor_jobs::WorkerJob;
use crate::viewport_agent::Handler;
use crate::views::ViewportLayout;

/// The job kind of a bake (`nav_preview:<n>` in `agent.job_status`).
pub const NAV_JOB_KIND: &str = "nav_preview";
/// The background-task id of a running bake (`editor.cancel_task { id: "nav_preview" }`).
pub const NAV_TASK: &str = "nav_preview";
/// The name of a bake's worker thread (the gather and the voxel build run there, ADR-8594).
pub const NAV_WORKER_THREAD: &str = "regia-nav-preview";

/// Half side of the XZ box the bake gathers geometry in, meters: the whole level.
const GATHER_HALF_EXTENT: f32 = 1.0e9;

/// The most voxel columns one `regia_nav` build allocates (the builder's own guard, and the
/// streaming cook's `MAX_STREAMED_GRID_CELLS`).
const MAX_GRID_CELLS: f64 = regia_nav::MAX_STREAMED_GRID_CELLS as f64;

/// Marks the world's `NavMesh` resource as the editor's preview (and what it was built from).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct NavPreview {
    /// Polygons in the baked mesh.
    pub polys: usize,
    /// Source triangles the bake read.
    pub triangles: usize,
    /// Where the build settings came from: `level_streaming` or `default`.
    pub settings: &'static str,
    /// The build settings used.
    pub params: NavMeshBuildParams,
    /// The level the preview was baked from (`LevelManager::current` at the bake). A preview of
    /// another level is stale: it neither draws nor reports, and the method drops it.
    pub level: Option<LevelId>,
}

/// Why a preview could not be built or cleared.
#[derive(Debug, Clone, PartialEq)]
pub enum NavPreviewError {
    /// The level has no static collider or terrain to bake (`nav_preview_no_geometry`).
    NoGeometry,
    /// The world holds a `NavMesh` the preview did not insert (`nav_preview_game_mesh`).
    GameMesh,
    /// The builder refused the geometry or the settings (`nav_preview_build_failed`).
    BuildFailed(String),
    /// The level's extent needs about this many grid cells at the cell size, past what one
    /// build allocates (`nav_preview_too_large`).
    TooLarge(f64),
    /// A bake runs already (`nav_preview_busy`): a second build is refused, not queued.
    Busy(String),
    /// The worker thread could not start (`nav_preview_build_failed`).
    StartFailed(String),
}

impl NavPreviewError {
    /// The protocol error code (`data.code`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoGeometry => "nav_preview_no_geometry",
            Self::GameMesh => "nav_preview_game_mesh",
            Self::BuildFailed(_) => "nav_preview_build_failed",
            Self::TooLarge(_) => "nav_preview_too_large",
            Self::Busy(_) => "nav_preview_busy",
            Self::StartFailed(_) => "nav_preview_build_failed",
        }
    }

    fn rpc(&self) -> RpcError {
        let (message, hint) = match self {
            Self::NoGeometry => (
                "the level has no static collision to bake".to_owned(),
                "add a Collider with a static RigidBody (or terrain) to the level",
            ),
            Self::GameMesh => (
                "the world holds a NavMesh the preview did not build".to_owned(),
                "the game's own mesh already draws under Show ▸ Navigation",
            ),
            Self::BuildFailed(why) => (
                format!("the nav mesh build failed: {why}"),
                "check the level's nav settings (LevelStreaming.nav) and its collision",
            ),
            Self::TooLarge(cells) => (
                format!(
                    "the level needs about {cells:.0} nav grid cells, past the {MAX_GRID_CELLS} one build allocates"
                ),
                "raise the level's nav cell_size (LevelStreaming.nav)",
            ),
            Self::Busy(job) => (
                format!("{job} is still baking the navigation preview"),
                "wait for it (agent.job_status) or cancel it (editor.nav_preview { action: \"cancel\" })",
            ),
            Self::StartFailed(why) => (
                format!("the bake worker did not start: {why}"),
                "try again; see the editor log",
            ),
        };
        RpcError::new(INVALID_PARAMS, self.code(), message, hint)
    }
}

/// The build settings for the open level and where they came from: the level's
/// `LevelStreaming::nav` when set, else the `regia_nav` defaults.
#[must_use]
pub fn build_params(world: &World) -> (NavMeshBuildParams, &'static str) {
    match world
        .get_resource::<LevelStreaming>()
        .and_then(|streaming| streaming.nav)
    {
        Some(cook) => (params_from_cook(&cook), "level_streaming"),
        None => (NavMeshBuildParams::default(), "default"),
    }
}

/// The `regia_nav` build parameters a `StreamNavCook` means (the streaming cook's mapping).
#[must_use]
pub fn params_from_cook(cook: &StreamNavCook) -> NavMeshBuildParams {
    NavMeshBuildParams {
        cell_size: cook.cell_size,
        cell_height: cook.cell_height,
        border_size: cook.border_size,
        agent: NavAgentParams {
            radius: cook.agent_radius,
            height: cook.agent_height,
            max_climb: cook.agent_max_climb,
            max_slope: cook.agent_max_slope,
        },
    }
}

/// `true` when the world's `NavMesh` (if any) is the preview's.
fn mesh_is_ours(world: &World) -> bool {
    world.contains_resource::<NavPreview>() || !world.contains_resource::<NavMesh>()
}

/// The level the editor has open (`LevelManager::current`).
fn current_level(world: &World) -> Option<LevelId> {
    world
        .get_resource::<LevelManager>()
        .and_then(LevelManager::current)
}

/// `true` when the world holds a preview baked from another level than the open one (File ▸
/// Open, New level): it must not draw over this level or report as active.
#[must_use]
pub fn is_stale(world: &World) -> bool {
    world
        .get_resource::<NavPreview>()
        .is_some_and(|preview| preview.level != current_level(world))
}

/// Removes a preview of another level (see [`is_stale`]). `true` when one was removed.
/// A running bake is left alone: its end checks the level itself ([`tick_job`]).
pub fn drop_stale(world: &mut World) -> bool {
    if !is_stale(world) {
        return false;
    }
    world.remove_resource::<NavPreview>();
    world.remove_resource::<NavMesh>();
    true
}

/// An XZ rectangle, `[min_x, min_z]` to `[max_x, max_z]`, in f64.
type Rect = ([f64; 2], [f64; 2]);

/// Grows `rect` to hold the point `(x, z)` (non-finite points are skipped).
fn grow(rect: &mut Option<Rect>, x: f64, z: f64) {
    if !(x.is_finite() && z.is_finite()) {
        return;
    }
    let (low, high) = rect.get_or_insert(([x, z], [x, z]));
    *low = [low[0].min(x), low[1].min(z)];
    *high = [high[0].max(x), high[1].max(z)];
}

/// The XZ rectangle the terrains cover, in the gather's (origin-relative) space. A hole that cuts
/// a terrain's whole border is not taken off the rectangle.
fn terrain_rect(world: &World, rect: &mut Option<Rect>) {
    let Some(mut terrains) = world.try_query::<&regia_terrain::Terrain>() else {
        return;
    };
    let offset = world
        .get_resource::<regia_scene::streaming::OriginOffset>()
        .map_or([0.0; 2], |o| [o.0[0], o.0[2]]);
    for terrain in terrains.iter(world) {
        let map = &terrain.map;
        if map.width() < 2 || map.height() < 2 {
            continue;
        }
        for [x, z] in [
            map.sample_to_world(0, 0),
            map.sample_to_world(map.width() - 1, map.height() - 1),
        ] {
            grow(rect, f64::from(x) - offset[0], f64::from(z) - offset[1]);
        }
    }
}

/// The XZ rectangle of the static colliders that feed the bake (rev-claude-64 (2) finding 3
/// extended, ADR-7664): box, triangle-mesh and heightfield colliders, not sensors, on an entity
/// with a static body, at the pose the gather places them ([`regia_framework::nav_world::world_pose`]
/// with the collider's offset). A box gives its 8 corners, a mesh its vertices, a heightfield the
/// 8 corners of its height range (exact for a level field, a bound for a tilted one). One pass
/// over the vertices, no allocation.
fn collider_rect(world: &World, rect: &mut Option<Rect>) {
    use regia_physics::{BodyKind, Collider, ColliderShape, RigidBody};
    let Some(mut colliders) = world.try_query::<(Entity, &Collider, &RigidBody)>() else {
        return;
    };
    let mut point = |p: Vec3| grow(rect, f64::from(p.x), f64::from(p.z));
    for (entity, collider, body) in colliders.iter(world) {
        if collider.sensor || body.kind != BodyKind::Static {
            continue;
        }
        let pose = regia_framework::nav_world::world_pose(world, entity) * collider.offset.compute_affine();
        let mut corners = |low: Vec3, high: Vec3| {
            for bits in 0..8u8 {
                let pick = |bit: u8, a: f32, b: f32| if bits & bit == 0 { a } else { b };
                point(pose.transform_point3(Vec3::new(
                    pick(1, low.x, high.x),
                    pick(2, low.y, high.y),
                    pick(4, low.z, high.z),
                )));
            }
        };
        match &collider.shape {
            ColliderShape::Box { half_extents } => corners(-*half_extents, *half_extents),
            ColliderShape::TriMesh { vertices, .. } => {
                for vertex in vertices {
                    point(pose.transform_point3(*vertex));
                }
            }
            ColliderShape::HeightField { heights, scale, .. } => {
                let (low, high) = heights
                    .iter()
                    .filter(|h| h.is_finite())
                    .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), h| {
                        (lo.min(*h), hi.max(*h))
                    });
                if low <= high {
                    corners(
                        Vec3::new(-0.5 * scale.x, low * scale.y, -0.5 * scale.z),
                        Vec3::new(0.5 * scale.x, high * scale.y, 0.5 * scale.z),
                    );
                }
            }
            _ => {}
        }
    }
}

/// The grid cells the level's bake needs at `params`: the XZ rectangle of the terrains and the
/// static colliders ([`terrain_rect`], [`collider_rect`]) with the builder's border and padding
/// (`regia_nav`'s grid-size guard, the streaming cook's estimate). `None` without either. The
/// gathered geometry covers this rectangle, so an estimate past [`MAX_GRID_CELLS`] means the
/// builder would refuse; the check runs before the gather, which on a big terrain or mesh reads
/// millions of triangles (rev-claude-64 (2) finding 3).
fn level_cells(world: &World, params: &NavMeshBuildParams) -> Option<f64> {
    let mut rect = None;
    terrain_rect(world, &mut rect);
    collider_rect(world, &mut rect);
    let (low, high) = rect?;
    let border = f64::from(params.border_size);
    let cell = f64::from(params.cell_size);
    let along = |extent: f64| (extent + 2.0 * border) / cell + 2.0;
    Some(along(high[0] - low[0]) * along(high[1] - low[1]))
}

/// What the editor thread hands the bake: the settings and a copy of the level's nav sources
/// (no triangle yet: [`gather_triangles`] makes them, on the bake worker for a job).
#[derive(Debug, Clone)]
struct Prepared {
    params: NavMeshBuildParams,
    settings: &'static str,
    sources: LevelNavSources,
}

/// The editor-thread half of a bake: refuses a game's mesh and a level past the grid budget
/// (before any triangle is read), then copies the level's nav sources out of the world
/// (ADR-8594). A level with no source at all is refused here; one whose sources give no triangle
/// is refused by [`gather_triangles`].
fn prepare(world: &mut World) -> Result<Prepared, NavPreviewError> {
    drop_stale(world);
    if !mesh_is_ours(world) {
        return Err(NavPreviewError::GameMesh);
    }
    let (params, settings) = build_params(world);
    // Non-finite settings are the builder's error, not a size.
    if let Some(cells) = level_cells(world, &params)
        && cells.is_finite()
        && cells > MAX_GRID_CELLS
    {
        return Err(NavPreviewError::TooLarge(cells));
    }
    let (min, max) = gather_box();
    let sources = LevelNavSources::capture(world, min, max);
    if sources.is_empty() {
        return Err(NavPreviewError::NoGeometry);
    }
    Ok(Prepared {
        params,
        settings,
        sources,
    })
}

/// The XZ box the bake gathers geometry in: the whole level.
fn gather_box() -> (Vec3, Vec3) {
    (Vec3::splat(-GATHER_HALF_EXTENT), Vec3::splat(GATHER_HALF_EXTENT))
}

/// Why the worker half of a bake ([`NavPreviewJob::job`]) ended without a mesh.
#[derive(Debug, Clone, PartialEq)]
pub enum BakeFail {
    /// A cancel came during the gather or before the build.
    Cancelled,
    /// The sources gave no triangle.
    NoGeometry,
    /// The builder refused.
    Build(String),
}

/// Makes the triangles of `sources` (the gather), asking `go_on` before each source: `None` when
/// it said stop.
fn gather_triangles(
    sources: &LevelNavSources,
    go_on: impl FnMut(usize, usize) -> bool,
) -> Option<Vec<[Vec3; 3]>> {
    let _span = tracing::info_span!(target: "regia_editor", "nav_preview_gather").entered();
    #[cfg(test)]
    tests::GATHERS_ON_THIS_THREAD.with(|count| count.set(count.get() + 1));
    let (min, max) = gather_box();
    let mut triangles = Vec::new();
    sources
        .triangles_in(min, max, &mut triangles, go_on)
        .then_some(triangles)
}

/// Puts a baked mesh in place as the preview of `level`, baked from `triangles` source
/// triangles with `params` (from `settings`).
fn install(
    world: &mut World,
    mesh: NavMesh,
    (params, settings, triangles): (NavMeshBuildParams, &'static str, usize),
    level: Option<LevelId>,
) -> NavPreview {
    let preview = NavPreview {
        polys: mesh.polys.len(),
        triangles,
        settings,
        params,
        level,
    };
    world.insert_resource(mesh);
    world.insert_resource(preview.clone());
    preview
}

/// Bakes the preview from the level's static collision on the calling thread and inserts it
/// (replacing an earlier preview), without a job: for tools and tests. The editor's menu item and
/// `editor.nav_preview` bake through [`start_build`]. Does not touch the show flags;
/// [`build_and_show`] also turns Show ▸ Navigation on. Refused while a bake job runs.
///
/// # Errors
/// [`NavPreviewError::Busy`] while a bake job runs, [`NavPreviewError::GameMesh`] when a game's
/// mesh is in the world, [`NavPreviewError::TooLarge`] when the level's terrains and static
/// colliders need more grid cells than one build allocates (checked before any geometry is read),
/// [`NavPreviewError::NoGeometry`] when the level has no static collision,
/// [`NavPreviewError::BuildFailed`] when the builder refuses.
pub fn build(world: &mut World) -> Result<NavPreview, NavPreviewError> {
    let _span = tracing::info_span!(target: "regia_editor", "nav_preview_build").entered();
    if let Some(job) = world.get_resource::<NavPreviewJob>() {
        return Err(NavPreviewError::Busy(job.job.id.clone()));
    }
    let prepared = prepare(world)?;
    let triangles = gather_triangles(&prepared.sources, |_, _| true).unwrap_or_default();
    if triangles.is_empty() {
        return Err(NavPreviewError::NoGeometry);
    }
    let mesh = bake_with(world)(&prepared.params, &triangles).map_err(NavPreviewError::BuildFailed)?;
    let level = current_level(world);
    let record = (prepared.params, prepared.settings, triangles.len());
    Ok(install(world, mesh, record, level))
}

/// The voxel build a bake runs: `regia_nav`'s builder by default. A test installs a counted fake
/// gated on a channel it releases.
pub type NavBakeFn =
    std::sync::Arc<dyn Fn(&NavMeshBuildParams, &[[Vec3; 3]]) -> Result<NavMesh, String> + Send + Sync>;

/// The build the preview's bakes run (an edit-world resource; absent means `regia_nav`'s).
#[derive(Resource, Clone)]
pub struct NavBaker(pub NavBakeFn);

impl Default for NavBaker {
    fn default() -> Self {
        Self(std::sync::Arc::new(
            |params: &NavMeshBuildParams, triangles: &[[Vec3; 3]]| {
                NavMeshBuilder::new(params.clone())
                    .build(triangles)
                    .map_err(|e| e.to_string())
            },
        ))
    }
}

impl std::fmt::Debug for NavBaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NavBaker")
    }
}

fn bake_with(world: &World) -> NavBakeFn {
    world.get_resource::<NavBaker>().cloned().unwrap_or_default().0
}

/// What a bake worker answers: the mesh and the source triangle count it was built from, or why
/// there is none.
type BakeOutcome = Result<(NavMesh, usize), BakeFail>;

/// The running bake (an edit-world resource; one at a time).
#[derive(Resource, Debug)]
pub struct NavPreviewJob {
    /// The worker job (`nav_preview:<n>`).
    pub job: WorkerJob<BakeOutcome>,
    /// The level the bake was gathered from.
    pub level: Option<LevelId>,
    /// Turn Show ▸ Navigation on when it ends (the menu item and the method).
    pub show: bool,
    /// The settings and where they came from (the worker gathers the triangles and reports their
    /// count with the mesh).
    record: (NavMeshBuildParams, &'static str),
}

/// Starts a bake as job `nav_preview:<n>` (the menu item and `editor.nav_preview { action:
/// "build" }`): the size check and the copy of the level's nav sources run here; the gather (the
/// triangles) and the voxel build run on a worker thread (ADR-8594), which stops at the next
/// source, or before the build, once the job is cancelled.
/// [`tick_job`] puts the mesh in place (and, with `show`, turns Show ▸ Navigation on) when it ends.
///
/// # Errors
/// [`NavPreviewError::Busy`] while a bake runs (refused, not queued); otherwise as [`build`]'s
/// checks ([`NavPreviewError::GameMesh`], [`NavPreviewError::TooLarge`],
/// [`NavPreviewError::NoGeometry`]); [`NavPreviewError::StartFailed`] when the worker cannot start.
pub fn start_build(world: &mut World, show: bool) -> Result<String, NavPreviewError> {
    let _span = tracing::info_span!(target: "regia_editor", "nav_preview_start").entered();
    if let Some(job) = world.get_resource::<NavPreviewJob>() {
        return Err(NavPreviewError::Busy(job.job.id.clone()));
    }
    let Prepared {
        params,
        settings,
        sources,
    } = prepare(world)?;
    let bake = bake_with(world);
    let record = (params.clone(), settings);
    let job = WorkerJob::spawn(
        world,
        NAV_JOB_KIND,
        NAV_TASK,
        format!(
            "baking the navigation preview ({} collision sources)",
            sources.len()
        ),
        NAV_WORKER_THREAD,
        move |cx| {
            let triangles = gather_triangles(&sources, |done, total| {
                if cx.cancelled() {
                    return false;
                }
                // The gather is the first half of the progress bar; the build has no steps.
                let fraction = done as f64 / total.max(1) as f64 * 0.5;
                cx.progress(
                    Some(fraction),
                    &format!("reading the level's collision ({done} of {total})"),
                );
                true
            })
            .ok_or(BakeFail::Cancelled)?;
            drop(sources);
            if triangles.is_empty() {
                return Err(BakeFail::NoGeometry);
            }
            if cx.cancelled() {
                return Err(BakeFail::Cancelled);
            }
            cx.progress(
                Some(0.5),
                &format!("building the navigation mesh from {} triangles", triangles.len()),
            );
            let mesh = bake(&params, &triangles).map_err(BakeFail::Build)?;
            Ok((mesh, triangles.len()))
        },
    )
    .map_err(NavPreviewError::StartFailed)?;
    let id = job.id.clone();
    let level = current_level(world);
    world.insert_resource(NavPreviewJob {
        job,
        level,
        show,
        record,
    });
    Ok(id)
}

/// How a bake job ended, as [`tick_job`] and [`finish_now`] apply it.
#[derive(Debug, Clone, PartialEq)]
pub enum BakeEnd {
    /// The mesh is in place.
    Built(NavPreview),
    /// Cancelled, or the open level changed (the preview that was there stays; `reason`:
    /// `nav_preview_cancelled` or `nav_preview_level_changed`).
    Cancelled(&'static str),
    /// The builder refused, the worker stopped, or a game's mesh appeared meanwhile.
    Failed(NavPreviewError),
}

/// Applies a bake's end (`outcome`, or `None` for a cancel that does not wait for the worker) to
/// the world and ends its job.
fn finish(world: &mut World, job: &NavPreviewJob, outcome: Option<Result<BakeOutcome, String>>) -> BakeEnd {
    let end = match outcome {
        None => BakeEnd::Cancelled("nav_preview_cancelled"),
        Some(_) if job.level != current_level(world) => BakeEnd::Cancelled("nav_preview_level_changed"),
        Some(_) if !mesh_is_ours(world) => BakeEnd::Failed(NavPreviewError::GameMesh),
        Some(Ok(Err(BakeFail::Cancelled))) => BakeEnd::Cancelled("nav_preview_cancelled"),
        Some(Ok(Err(BakeFail::NoGeometry))) => BakeEnd::Failed(NavPreviewError::NoGeometry),
        Some(Ok(Ok((mesh, triangles)))) => {
            let (params, settings) = job.record.clone();
            let preview = install(world, mesh, (params, settings, triangles), job.level);
            if job.show {
                world
                    .get_resource_or_insert_with(ViewportLayout::default)
                    .show
                    .navigation = true;
            }
            BakeEnd::Built(preview)
        }
        Some(Ok(Err(BakeFail::Build(why))) | Err(why)) => BakeEnd::Failed(NavPreviewError::BuildFailed(why)),
    };
    match &end {
        BakeEnd::Built(preview) => {
            tracing::info!(target: "regia_editor", polys = preview.polys, "navigation preview built");
            job.job.end(world, Ok((state_json(world), false)));
        }
        BakeEnd::Cancelled(reason) => {
            job.job
                .end(world, Ok((json!({ "cancelled": true, "reason": reason }), true)));
        }
        BakeEnd::Failed(error) => {
            tracing::warn!(target: "regia_editor", code = error.code(), "navigation preview: {error:?}");
            job.job.end(world, Err((error.code(), error.rpc().message)));
        }
    }
    end
}

/// Polls the running bake (every frame, [`crate::editor_jobs::tick`]): a cancel ends it at once
/// (the worker's mesh is dropped when it comes), a returned worker's mesh goes in place. Never
/// waits.
pub fn tick_job(world: &mut World) {
    let Some(mut job) = world.remove_resource::<NavPreviewJob>() else {
        return;
    };
    if job.job.context().cancelled() {
        finish(world, &job, None);
        return;
    }
    match job.job.take() {
        Some(outcome) => {
            finish(world, &job, Some(outcome));
        }
        None => {
            let message = job.job.context().message();
            job.job.sync_row(world, &message, job.job.context().fraction());
            world.insert_resource(job);
        }
    }
}

/// Waits for the running bake and applies its end: the blocking `editor.nav_preview { action:
/// "build" }` (no `async`). `None` without a running bake.
pub fn finish_now(world: &mut World) -> Option<BakeEnd> {
    let mut job = world.remove_resource::<NavPreviewJob>()?;
    let outcome = job.job.join();
    let outcome = if job.job.context().cancelled() {
        None
    } else {
        outcome
    };
    Some(finish(world, &job, outcome))
}

/// Cancels the running bake (its job ends `cancelled` at once). `true` when one ran.
pub fn cancel(world: &mut World) -> bool {
    let Some(job) = world.get_resource::<NavPreviewJob>() else {
        return false;
    };
    job.job.cancel();
    tick_job(world);
    true
}

/// [`build`], then Show ▸ Navigation on in the world's [`ViewportLayout`]: the job-less form of
/// what the menu item and `editor.nav_preview { action: "build" }` do through
/// `start_build(world, true)`.
///
/// # Errors
/// As [`build`].
pub fn build_and_show(world: &mut World) -> Result<NavPreview, NavPreviewError> {
    let preview = build(world)?;
    world
        .get_resource_or_insert_with(ViewportLayout::default)
        .show
        .navigation = true;
    Ok(preview)
}

/// Removes the preview, and cancels a running bake (so it cannot put one back). `Ok(false)` when
/// there was no preview.
///
/// # Errors
/// [`NavPreviewError::GameMesh`] when the world's `NavMesh` is a game's (it stays).
pub fn clear(world: &mut World) -> Result<bool, NavPreviewError> {
    if !mesh_is_ours(world) {
        return Err(NavPreviewError::GameMesh);
    }
    cancel(world);
    let had = world.remove_resource::<NavPreview>().is_some();
    if had {
        world.remove_resource::<NavMesh>();
    }
    Ok(had)
}

fn state_json(world: &World) -> Value {
    let preview = world.get_resource::<NavPreview>();
    let game_mesh = preview.is_none() && world.contains_resource::<NavMesh>();
    let navigation = world
        .get_resource::<ViewportLayout>()
        .is_some_and(|layout| layout.show.navigation);
    let building = world
        .get_resource::<NavPreviewJob>()
        .map(|job| job.job.id.clone());
    let mut state = match preview {
        Some(p) => json!({
            "active": true,
            "polys": p.polys,
            "triangles": p.triangles,
            "settings": p.settings,
            "cell_size": p.params.cell_size,
            "agent_radius": p.params.agent.radius,
            "game_mesh": false,
            "navigation": navigation,
        }),
        None => json!({
            "active": false,
            "polys": 0,
            "triangles": 0,
            "game_mesh": game_mesh,
            "navigation": navigation,
        }),
    };
    if let Some(object) = state.as_object_mut() {
        object.insert("building".into(), json!(building));
    }
    state
}

/// `editor.nav_preview { action?, async? }`. `build` is job `nav_preview:<n>` (ADR-7664): with
/// `async` the reply is the state at once (`building` names the job), otherwise the method waits
/// for the bake; both add `job`.
fn nav_preview_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // A finished bake lands first; a preview of the level that was open before reports nothing
    // and goes.
    tick_job(world);
    drop_stale(world);
    let mut job = None;
    match params.get("action").map(|a| a.as_str()) {
        None => {}
        Some(Some("build")) => {
            let id = start_build(world, true).map_err(|e| e.rpc())?;
            if params.get("async").and_then(Value::as_bool) != Some(true) {
                match finish_now(world) {
                    Some(BakeEnd::Failed(error)) => return Err(error.rpc()),
                    Some(BakeEnd::Cancelled(reason)) => {
                        return Err(RpcError::new(
                            INVALID_PARAMS,
                            reason,
                            format!("{id} ended without a preview ({reason})"),
                            "build again",
                        ));
                    }
                    Some(BakeEnd::Built(_)) | None => {}
                }
            }
            job = Some(id);
        }
        Some(Some("clear")) => {
            clear(world).map_err(|e| e.rpc())?;
        }
        Some(Some("cancel")) => {
            cancel(world);
        }
        Some(other) => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "nav_preview_unknown_action",
                format!("unknown action {other:?}"),
                "action is \"build\", \"cancel\" or \"clear\" (omit it to read the state)",
            ));
        }
    }
    let mut state = state_json(world);
    if let (Some(id), Some(object)) = (job, state.as_object_mut()) {
        object.insert("job".into(), json!(id));
    }
    Ok(state)
}

/// The navigation preview agent method.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.nav_preview",
            tool: "nav_preview",
            description: "The viewport's navigation preview (Show ▸ Build navigation preview): `build` bakes a nav mesh from the level's static collision with the level's nav settings (LevelStreaming.nav, else the defaults) into an editor-only NavMesh resource that is never saved, and turns Show ▸ Navigation on; `cancel` stops a running bake (the preview that was there stays); `clear` removes the preview and cancels a running bake; no action reads the state. The bake is job nav_preview:<n> (agent.job_status, editor.cancel_job): the reply's `job` names it; with async: true the reply comes at once (`building` names the running job), otherwise when the bake ends. One bake at a time (nav_preview_busy). A level whose terrains and static colliders need more nav grid cells than one build allocates is refused before any geometry is read (nav_preview_too_large). A game's own NavMesh is never replaced (nav_preview_game_mesh).",
            params: &[
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: false,
                    description: "build, cancel or clear; omitted reads the state.",
                },
                ParamSchema {
                    name: "async",
                    kind: "boolean",
                    required: false,
                    description: "With build: true answers at once while the bake runs as a job; absent or false waits for it.",
                },
            ],
            result: "{ active, polys, triangles, settings?, cell_size?, agent_radius?, game_mesh, navigation, building, job? }",
            // A build is a whole-level gather on the UI thread plus a bake job: rate-limited like
            // an edit (rev-claude-64 (2) finding 1), yet read-scoped, as the preview is editor view state.
            mutating: true,
            permission: Permission::Read,
        },
        nav_preview_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::GlobalTransform;
    use regia_scene::prelude::Transform;

    fn static_box(world: &mut World, center: Vec3, half: Vec3) {
        let transform = Transform::from_translation(center);
        world.spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            regia_physics::RigidBody::static_body(),
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box { half_extents: half },
                ..regia_physics::Collider::default()
            },
        ));
    }

    #[test]
    fn the_floor_bakes_and_clear_removes_only_the_preview() {
        let mut world = World::new();
        static_box(&mut world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        let preview = build_and_show(&mut world).expect("bakes");
        assert!(preview.polys > 0);
        assert_eq!(preview.triangles, 12, "one box");
        assert_eq!(preview.settings, "default");
        assert_eq!(world.resource::<NavMesh>().polys.len(), preview.polys);
        assert!(
            world.resource::<ViewportLayout>().show.navigation,
            "the flag turns on"
        );
        assert_eq!(clear(&mut world), Ok(true));
        assert!(!world.contains_resource::<NavMesh>());
        assert_eq!(clear(&mut world), Ok(false), "nothing left to clear");
    }

    #[test]
    fn dynamic_bodies_and_sensors_are_not_ground() {
        let mut world = World::new();
        let transform = Transform::IDENTITY;
        world.spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            regia_physics::RigidBody::default(),
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box {
                    half_extents: Vec3::new(4.0, 0.1, 4.0),
                },
                ..regia_physics::Collider::default()
            },
        ));
        world.spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            regia_physics::RigidBody::static_body(),
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box {
                    half_extents: Vec3::new(4.0, 0.1, 4.0),
                },
                sensor: true,
                ..regia_physics::Collider::default()
            },
        ));
        assert_eq!(build(&mut world), Err(NavPreviewError::NoGeometry));
        assert!(!world.contains_resource::<NavMesh>());
    }

    #[test]
    fn the_level_streaming_nav_settings_drive_the_bake() {
        let mut world = World::new();
        static_box(&mut world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        let cook = StreamNavCook {
            cell_size: 0.25,
            agent_radius: 0.5,
            ..StreamNavCook::default()
        };
        world.insert_resource(LevelStreaming {
            nav: Some(cook),
            ..LevelStreaming::default()
        });
        let preview = build(&mut world).expect("bakes");
        assert_eq!(preview.settings, "level_streaming");
        assert_eq!(preview.params, params_from_cook(&cook));
        assert!((preview.params.cell_size - 0.25).abs() < f32::EPSILON);
        assert!((preview.params.agent.radius - 0.5).abs() < f32::EPSILON);
        // The radius erodes the walkable area: every vertex stays well inside the 4 m slab (a
        // 0.5 m radius on a 0.25 m grid leaves the edge near 3.5 m; 3.8 m allows a cell of slack).
        for poly in &world.resource::<NavMesh>().polys {
            for v in &poly.vertices {
                assert!(v.x.abs() <= 3.8 && v.z.abs() <= 3.8, "{v}");
            }
        }
    }

    /// rev-claude-64 (2) finding 1: `build` runs a whole-level bake on the UI thread, so the
    /// method counts against the mutating-call rate limit (dispatch admits only `mutating`
    /// methods against it). It stays read-scoped: the preview is editor-only view state.
    #[test]
    fn the_method_is_rate_limited_but_read_scoped() {
        let methods = methods();
        let schema = &methods[0].0;
        assert_eq!(schema.name, "editor.nav_preview");
        assert!(schema.mutating, "a build is a multi-second bake: rate-limit it");
        assert_eq!(schema.permission, Permission::Read);
    }

    fn scene_app() -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app
    }

    /// rev-claude-64 (2) finding 2: a preview built on one level neither draws nor reports on
    /// the next level (File ▸ Open, New level), even on a path that does not reset the edit
    /// state; the method drops it.
    #[test]
    fn a_preview_of_another_level_is_dropped() {
        let mut app = scene_app();
        let world = app.world_mut();
        crate::level_io::new_level(world);
        static_box(world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        build_and_show(world).expect("bakes");
        assert!(!crate::overlays::navigation_segments(world).is_empty());
        crate::level_io::new_level(world);
        assert!(
            crate::overlays::navigation_segments(world).is_empty(),
            "the old level's polygons must not draw over the new level"
        );
        let state = nav_preview_method(world, &json!({})).expect("reads");
        assert_eq!(state["active"], false, "{state}");
        assert_eq!(state["polys"], 0, "{state}");
        assert_eq!(state["game_mesh"], false, "{state}");
        assert!(!world.contains_resource::<NavPreview>());
        assert!(!world.contains_resource::<NavMesh>());
        // A build on the new level works as usual.
        let state = nav_preview_method(world, &json!({ "action": "build" })).expect("builds");
        assert_eq!(state["active"], true, "{state}");
    }

    /// The editor's level flows reset the edit state after a level replaced the world (even the
    /// same level re-opened): the preview goes with it.
    #[test]
    fn reset_edit_state_drops_the_preview_but_never_a_game_mesh() {
        let mut app = scene_app();
        let world = app.world_mut();
        crate::level_io::new_level(world);
        static_box(world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        build(world).expect("bakes");
        crate::level_io::reset_edit_state(world);
        assert!(!world.contains_resource::<NavPreview>());
        assert!(!world.contains_resource::<NavMesh>());
        // A mesh without the preview marker is a game's.
        build(world).expect("bakes");
        world.remove_resource::<NavPreview>();
        crate::level_io::reset_edit_state(world);
        assert!(world.contains_resource::<NavMesh>(), "a game's mesh stays");
    }

    /// rev-claude-64 (2) finding 3: a terrain whose extent alone needs more grid cells than the
    /// builder allocates is refused from its size, before any triangle is gathered.
    #[test]
    fn a_terrain_too_large_for_the_grid_is_refused_before_the_gather() {
        let mut world = World::new();
        let n = 33u32;
        let width = 100_000.0;
        let map = regia_terrain::TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], width, 1.0, 0.0)
            .expect("map");
        let terrain = regia_terrain::Terrain::new(
            &map.to_heightfield(),
            width,
            1.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            1,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        world.spawn(terrain);
        match build(&mut world) {
            Err(NavPreviewError::TooLarge(cells)) => {
                assert!(cells > MAX_GRID_CELLS, "{cells}");
            }
            other => panic!("expected nav_preview_too_large, got {other:?}"),
        }
        assert!(!world.contains_resource::<NavMesh>());
        // The same terrain at a sane size bakes.
        let small =
            regia_terrain::TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 16.0, 1.0, 0.0)
                .expect("map");
        let mut world = World::new();
        world.spawn(
            regia_terrain::Terrain::new(
                &small.to_heightfield(),
                16.0,
                1.0,
                0.0,
                regia_terrain::CdlodConfig::default(),
                1,
                regia_terrain::TerrainLayers::default(),
            )
            .expect("terrain"),
        );
        assert!(build(&mut world).is_ok());
    }

    #[test]
    fn error_codes_are_protocol_safe() {
        for e in [
            NavPreviewError::NoGeometry,
            NavPreviewError::GameMesh,
            NavPreviewError::BuildFailed("x".into()),
            NavPreviewError::TooLarge(1.0e9),
            NavPreviewError::Busy("nav_preview:1".into()),
            NavPreviewError::StartFailed("x".into()),
        ] {
            let code = e.code();
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
            assert_eq!(e.rpc().data.as_ref().and_then(|d| d["code"].as_str()), Some(code));
        }
    }

    // ---- ADR-7664: the bake as an editor job, the size check over colliders -----------------

    fn huge_static(world: &mut World, shape: regia_physics::ColliderShape) {
        let transform = Transform::IDENTITY;
        world.spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            regia_physics::RigidBody::static_body(),
            regia_physics::Collider {
                shape,
                ..regia_physics::Collider::default()
            },
        ));
    }

    /// Item (2): the early size check covers the static colliders that feed the bake, with the
    /// terrains' budget: a box, a triangle mesh or a heightfield collider whose extent needs more
    /// grid cells than one build allocates is `nav_preview_too_large` before any triangle is read
    /// (at the base only terrains were checked; these reached the gather and the builder).
    #[test]
    fn a_mesh_too_large_for_the_grid_is_refused_before_the_gather() {
        let wide = 100_000.0_f32;
        let shapes = [
            regia_physics::ColliderShape::Box {
                half_extents: Vec3::new(wide, 0.1, wide),
            },
            regia_physics::ColliderShape::TriMesh {
                vertices: vec![
                    Vec3::new(-wide, 0.0, -wide),
                    Vec3::new(wide, 0.0, -wide),
                    Vec3::new(wide, 0.0, wide),
                ],
                indices: vec![[0, 1, 2]],
            },
            regia_physics::ColliderShape::HeightField {
                nx: 2,
                nz: 2,
                heights: vec![0.0; 4],
                scale: Vec3::new(2.0 * wide, 1.0, 2.0 * wide),
            },
        ];
        for shape in shapes {
            let name = format!("{shape:?}").chars().take(12).collect::<String>();
            let mut world = World::new();
            huge_static(&mut world, shape);
            match build(&mut world) {
                Err(NavPreviewError::TooLarge(cells)) => assert!(cells > MAX_GRID_CELLS, "{name}: {cells}"),
                other => panic!("{name}: expected nav_preview_too_large, got {other:?}"),
            }
            assert!(!world.contains_resource::<NavMesh>());
        }
        // Two small boxes far apart need the grid between them too: the rectangle is the union.
        let mut world = World::new();
        static_box(&mut world, Vec3::new(-wide, 0.0, -wide), Vec3::new(1.0, 0.1, 1.0));
        static_box(&mut world, Vec3::new(wide, 0.0, wide), Vec3::new(1.0, 0.1, 1.0));
        assert!(matches!(build(&mut world), Err(NavPreviewError::TooLarge(_))));
        // A sensor or a dynamic body does not feed the bake, so it does not count either.
        let mut world = World::new();
        static_box(&mut world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        let transform = Transform::IDENTITY;
        world.spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            regia_physics::RigidBody::default(),
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box {
                    half_extents: Vec3::new(wide, 0.1, wide),
                },
                ..regia_physics::Collider::default()
            },
        ));
        assert!(
            build(&mut world).is_ok(),
            "the 4 m floor bakes beside a huge dynamic box"
        );
    }

    /// Item (2): `editor.nav_preview { action: "build" }` keeps its blocking reply (the preview
    /// is in place) and names its job, `nav_preview:<n>`.
    #[test]
    fn nav_preview_build_names_its_job() {
        let mut world = World::new();
        static_box(&mut world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        let state = nav_preview_method(&mut world, &json!({ "action": "build" })).expect("builds");
        assert_eq!(state["active"], true, "{state}");
        assert!(
            state["job"]
                .as_str()
                .is_some_and(|job| job.starts_with("nav_preview:")),
            "{state}"
        );
        assert_eq!(state["navigation"], true, "{state}");
    }

    /// A counted bake gated on a channel the test releases (it then runs the real builder).
    struct GatedBake {
        bakes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        entered: std::sync::mpsc::Receiver<()>,
        release: std::sync::mpsc::Sender<()>,
    }

    fn gated_bake(world: &mut World) -> GatedBake {
        let bakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let counted = std::sync::Arc::clone(&bakes);
        world.insert_resource(NavBaker(std::sync::Arc::new(
            move |params: &NavMeshBuildParams, triangles: &[[Vec3; 3]]| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                entered_tx.send(()).ok();
                gate.lock().map_err(|_| "gate".to_string())?.recv().ok();
                NavMeshBuilder::new(params.clone())
                    .build(triangles)
                    .map_err(|e| e.to_string())
            },
        )));
        GatedBake {
            bakes,
            entered,
            release,
        }
    }

    fn nav_job_state(world: &World, id: &str) -> regia_agent::jobs::JobState {
        world
            .resource::<crate::editor_jobs::EditorJobs>()
            .get(id)
            .map(regia_agent::jobs::JobHandle::state)
            .expect("registered")
    }

    /// Item (2): the menu's bake no longer runs on the UI thread. `start_build` returns while the
    /// build is inside the gated fake; frames poll without blocking; a second build is refused
    /// (`nav_preview_busy`); once released, the poll puts the mesh in place, turns Show ▸
    /// Navigation on and ends the job, so the overlay draws it.
    #[test]
    fn the_bake_runs_as_a_job_and_the_overlay_updates_when_it_ends() {
        let mut world = World::new();
        static_box(&mut world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        let bake = gated_bake(&mut world);
        let id = start_build(&mut world, true).expect("starts");
        assert!(id.starts_with("nav_preview:"), "{id}");
        bake.entered.recv().expect("the bake started");
        for _ in 0..3 {
            crate::editor_jobs::tick(&mut world);
        }
        assert_eq!(nav_job_state(&world, &id), regia_agent::jobs::JobState::Running);
        assert!(!world.contains_resource::<NavMesh>(), "nothing in place yet");
        assert!(crate::overlays::navigation_segments(&world).is_empty());
        let state = nav_preview_method(&mut world, &json!({})).expect("state");
        assert_eq!(state["building"], json!(id), "{state}");
        assert!(
            world
                .resource::<crate::tasks::BackgroundTasks>()
                .rows()
                .iter()
                .any(|row| row.id == NAV_TASK)
        );
        assert_eq!(
            start_build(&mut world, true),
            Err(NavPreviewError::Busy(id.clone()))
        );
        let err = nav_preview_method(&mut world, &json!({ "action": "build" })).expect_err("busy");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("nav_preview_busy")
        );
        assert!(matches!(build(&mut world), Err(NavPreviewError::Busy(_))));

        bake.release.send(()).expect("release");
        world.resource_mut::<NavPreviewJob>().job.wait();
        crate::editor_jobs::tick(&mut world);
        assert_eq!(nav_job_state(&world, &id), regia_agent::jobs::JobState::Done);
        assert!(world.resource::<NavPreview>().polys > 0);
        assert!(
            world.resource::<ViewportLayout>().show.navigation,
            "the flag turns on"
        );
        assert!(
            !crate::overlays::navigation_segments(&world).is_empty(),
            "the overlay draws it"
        );
        assert!(!world.contains_resource::<NavPreviewJob>());
        assert!(
            world
                .resource::<crate::tasks::BackgroundTasks>()
                .rows()
                .is_empty()
        );
        assert_eq!(bake.bakes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    thread_local! {
        /// How many gathers ran on this thread. Each test runs on its own thread, so a test reads
        /// only its own count (ADR-8847: a probe subscriber's callsite interest is process-wide,
        /// and another test's thread could cache "never" for the gather span, so the full-order
        /// run counted 0 gathers).
        pub(super) static GATHERS_ON_THIS_THREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// v06-editor-usability-b item 1 (ADR-8594, the w11 "nav bake gather on the UI thread" gap):
    /// the gather runs on the bake worker. Counting the gathers on the calling (editor) thread
    /// (`GATHERS_ON_THIS_THREAD`, ADR-8847), `start_build` runs none there, while the job-less
    /// `build` (the control) runs one on the calling thread; the worker's bake sees every triangle of the level, on the worker
    /// thread, and the preview reports their count. The world can change while the worker reads:
    /// the bake reads the copy taken when it started.
    #[test]
    fn the_gather_runs_on_the_bake_worker_not_the_editor_thread() {
        let at_start = GATHERS_ON_THIS_THREAD.with(std::cell::Cell::get);
        let gathers = || GATHERS_ON_THIS_THREAD.with(std::cell::Cell::get) - at_start;

        let mut world = World::new();
        static_box(&mut world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        static_box(&mut world, Vec3::new(20.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        build(&mut world).expect("the control bakes");
        assert_eq!(gathers(), 1, "the job-less build gathers on the calling thread");

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let bake = gated_bake(&mut world);
        let inner = world.resource::<NavBaker>().0.clone();
        let record = std::sync::Arc::clone(&seen);
        world.insert_resource(NavBaker(std::sync::Arc::new(
            move |params: &NavMeshBuildParams, triangles: &[[Vec3; 3]]| {
                if let Ok(mut seen) = record.lock() {
                    seen.push((std::thread::current().name().map(str::to_owned), triangles.len()));
                }
                inner(params, triangles)
            },
        )));
        let id = start_build(&mut world, true).expect("starts");
        assert_eq!(gathers(), 1, "start_build read no triangle on the editor thread");
        // The level changes while the worker runs: its copy still holds both boxes.
        let colliders: Vec<Entity> = world
            .query_filtered::<Entity, With<regia_physics::Collider>>()
            .iter(&world)
            .collect();
        for entity in colliders {
            world.despawn(entity);
        }
        bake.entered.recv().expect("the bake started");
        bake.release.send(()).expect("release");
        world.resource_mut::<NavPreviewJob>().job.wait();
        crate::editor_jobs::tick(&mut world);
        assert_eq!(gathers(), 1, "nor did the poll that put the mesh in place");
        assert_eq!(nav_job_state(&world, &id), regia_agent::jobs::JobState::Done);
        assert_eq!(world.resource::<NavPreview>().triangles, 24, "two boxes");
        let seen = seen.lock().expect("seen").clone();
        assert_eq!(seen, vec![(Some(NAV_WORKER_THREAD.to_owned()), 24)]);
    }

    /// ADR-8594: sources that give no triangle end the job `nav_preview_no_geometry` (the worker
    /// finds out, not the editor thread), and the bake is never called.
    #[test]
    fn sources_without_triangles_end_the_job_with_no_geometry() {
        let mut world = World::new();
        world.spawn((
            Transform::IDENTITY,
            GlobalTransform(Transform::IDENTITY.compute_affine()),
            regia_physics::RigidBody::static_body(),
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::TriMesh {
                    vertices: Vec::new(),
                    indices: Vec::new(),
                },
                ..regia_physics::Collider::default()
            },
        ));
        let bake = gated_bake(&mut world);
        let id = start_build(&mut world, true).expect("a source: the job starts");
        world.resource_mut::<NavPreviewJob>().job.wait();
        crate::editor_jobs::tick(&mut world);
        assert_eq!(nav_job_state(&world, &id), regia_agent::jobs::JobState::Failed);
        assert_eq!(bake.bakes.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!world.contains_resource::<NavMesh>());
        let err = nav_preview_method(&mut world, &json!({ "action": "build" })).expect_err("no geometry");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("nav_preview_no_geometry")
        );
    }

    /// Item (2): a cancel ends the job `cancelled` at once and keeps the preview that was there;
    /// the worker's late mesh is dropped.
    #[test]
    fn a_cancel_keeps_the_old_preview() {
        let mut world = World::new();
        static_box(&mut world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        let before = build(&mut world).expect("a first preview");
        let old_polys = world.resource::<NavMesh>().polys.len();
        static_box(&mut world, Vec3::new(20.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        let bake = gated_bake(&mut world);
        let out =
            nav_preview_method(&mut world, &json!({ "action": "build", "async": true })).expect("async");
        let id = out["job"].as_str().expect("job").to_string();
        assert_eq!(out["building"], json!(id), "{out}");
        bake.entered.recv().expect("the bake started");
        let out = nav_preview_method(&mut world, &json!({ "action": "cancel" })).expect("cancel");
        assert_eq!(out["building"], Value::Null, "{out}");
        assert_eq!(nav_job_state(&world, &id), regia_agent::jobs::JobState::Cancelled);
        assert_eq!(world.resource::<NavPreview>(), &before, "the old preview stays");
        assert_eq!(world.resource::<NavMesh>().polys.len(), old_polys);
        bake.release.send(()).expect("let the detached worker end");
        crate::editor_jobs::tick(&mut world);
        assert_eq!(
            world.resource::<NavPreview>(),
            &before,
            "its late mesh is dropped"
        );
        // editor.cancel_job reaches a bake too.
        let bake = gated_bake(&mut world);
        let id = start_build(&mut world, true).expect("again");
        bake.entered.recv().expect("started");
        crate::editor_jobs::cancel_job(&mut world, &json!({ "id": id })).expect("cancel_job");
        crate::editor_jobs::tick(&mut world);
        assert_eq!(nav_job_state(&world, &id), regia_agent::jobs::JobState::Cancelled);
        bake.release.send(()).expect("release");
    }

    /// Item (2): a bake whose level is no longer open when it ends is dropped (the job ends
    /// `cancelled`); a level reset (`clear`) cancels a running bake at once.
    #[test]
    fn a_level_change_drops_the_running_bake() {
        let mut app = scene_app();
        let world = app.world_mut();
        crate::level_io::new_level(world);
        static_box(world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        let bake = gated_bake(world);
        let id = start_build(world, true).expect("starts");
        bake.entered.recv().expect("started");
        crate::level_io::new_level(world);
        bake.release.send(()).expect("release");
        world.resource_mut::<NavPreviewJob>().job.wait();
        crate::editor_jobs::tick(world);
        assert_eq!(nav_job_state(world, &id), regia_agent::jobs::JobState::Cancelled);
        assert!(!world.contains_resource::<NavPreview>());
        assert!(!world.contains_resource::<NavMesh>());

        static_box(world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(4.0, 0.1, 4.0));
        let bake = gated_bake(world);
        let id = start_build(world, true).expect("starts");
        bake.entered.recv().expect("started");
        crate::level_io::reset_edit_state(world);
        assert_eq!(nav_job_state(world, &id), regia_agent::jobs::JobState::Cancelled);
        assert!(!world.contains_resource::<NavPreviewJob>());
        bake.release.send(()).expect("release");
    }
}
