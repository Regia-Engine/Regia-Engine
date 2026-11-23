// SPDX-License-Identifier: MPL-2.0
//! The editor side of the DCC scene feed (`docs/specs/60-dcc-scene-feed.md` § 3, § 6): the bake
//! methods, which need the asset pipeline, and the DCC feed panel. The panel is a thin client of
//! the functions the agent methods call, so what an agent does is what the panel shows.
//!
//! A bake plans on the main thread from a [`FeedSnapshot`], writes and cooks on a worker thread,
//! and finishes on the main thread when the editor frame (or the panel) sees the worker done.
//! `dcc_feed.bake_status` only reports that state; it does not apply the bake.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use regia_agent::dcc_feed::mesh::FeedMeshData;
use regia_agent::dcc_feed::methods::{self, refused};
use regia_agent::dcc_feed::net::lock;
use regia_agent::dcc_feed::registry::ItemState;
use regia_agent::dcc_feed::session::Phase;
use regia_agent::dcc_feed::{DccFeedRegistry, DccFeedServer, DccFeedSettings, FeedItem, wire::FeedKind};
use regia_agent::protocol::RpcError;
use regia_asset_pipeline::dcc_feed_bake::{
    BakeError, BakePlan, BakeRequest, FeedSnapshot, ImportOverrides, SnapshotClip, SnapshotItem,
    SnapshotMesh, SnapshotSkin, SnapshotSlot, SnapshotTexture, SnapshotTrack, plan_bake, write_glb,
};
use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_render::MeshSource;
use regia_scene::prelude::{EntityGuid, LevelMember, Transform};
use serde_json::{Value, json};

use crate::level_io::OpenLevel;

/// Takes over the built-in `dcc_feed.bake` and `dcc_feed.bake_status` rows, which answer `F0013`
/// in a build without the asset pipeline. Dispatch consults the extension first.
pub fn register(extension: &mut regia_agent::extension::AgentExtension) {
    if let Some(schema) = regia_agent::schema::method("dcc_feed.bake") {
        extension.register(schema.clone(), bake);
    }
    if let Some(schema) = regia_agent::schema::method("dcc_feed.bake_status") {
        extension.register(schema.clone(), bake_status);
    }
}

/// What the worker thread sends back.
type WorkerResult = Result<Vec<String>, BakeError>;

struct RunningBake {
    id: u64,
    worker: JoinHandle<WorkerResult>,
    /// `(guid, path, scale)` of each file.
    files: Vec<(Guid, String, f32)>,
    plain: Vec<Guid>,
    warnings: Vec<String>,
    replace_live: bool,
}

/// One finished bake, kept for `dcc_feed.bake_status`.
#[derive(Debug, Clone)]
struct FinishedBake {
    id: u64,
    report: Value,
}

/// Bake jobs: at most one runs, since each cook reads the whole project.
#[derive(Resource, Default)]
pub struct FeedBakeJobs {
    next_id: u64,
    running: Option<RunningBake>,
    finished: Vec<FinishedBake>,
    /// The panel's folder field.
    pub folder: String,
    /// The panel's import scale (0 keeps the sent scale).
    pub scale: f32,
    /// The panel's LOD count (0 generates none).
    pub lods: u32,
    /// The panel's collision check.
    pub collision: bool,
    /// The panel's record path field.
    pub record_path: String,
    /// The panel's replay path field.
    pub replay_path: String,
    /// The panel's "remember this client" check.
    pub remember: bool,
    /// The panel's token reveal.
    pub reveal_token: bool,
}

/// Most finished reports kept.
const FINISHED_KEPT: usize = 16;

/// A plain-data copy of `registry`'s items, in the bake's terms.
#[must_use]
pub fn snapshot_of(registry: &DccFeedRegistry) -> FeedSnapshot {
    let items = registry
        .table
        .iter()
        .map(|item| SnapshotItem {
            key: item.key.clone(),
            guid: item.guid,
            kind: item.kind.name().to_owned(),
            mesh: registry.meshes.get(&item.guid).map(|m| {
                let mut mesh = snapshot_mesh(&m.data);
                let skeleton = item.parent.and_then(|p| Some((p, registry.skeletons.get(&p)?)));
                mesh.skin = skeleton
                    .filter(|_| m.data.is_skinned())
                    .map(|(guid, s)| snapshot_skin(guid, &s.asset));
                mesh
            }),
        })
        .collect();
    let textures = registry
        .textures
        .iter()
        .filter_map(|(name, texture)| {
            let (container, bytes) = texture.encoded.as_ref()?;
            let mime = if *container == 0 {
                "image/png"
            } else {
                "image/jpeg"
            };
            Some((
                name.clone(),
                SnapshotTexture {
                    bytes: bytes.as_ref().clone(),
                    mime,
                },
            ))
        })
        .collect();
    let clips = registry
        .clips
        .iter()
        .map(|(name, clip)| SnapshotClip {
            name: name.clone(),
            item: clip.bound,
            tracks: clip
                .clip
                .tracks
                .iter()
                .map(|t| SnapshotTrack {
                    target: t.target.clone(),
                    property: t.property,
                    linear: t.interp == 1,
                    times: t.times.clone(),
                    values: t.values.clone(),
                })
                .collect(),
        })
        .collect();
    FeedSnapshot {
        items,
        textures,
        clips,
    }
}

fn snapshot_skin(guid: Guid, skeleton: &regia_animation::Skeleton) -> SnapshotSkin {
    SnapshotSkin {
        guid,
        names: skeleton.joint_names.clone(),
        parents: skeleton.parents.clone(),
        binds: skeleton
            .bind_pose
            .iter()
            .map(|b| {
                (
                    b.translation.to_array(),
                    b.rotation.to_array(),
                    b.scale.to_array(),
                )
            })
            .collect(),
        inverse_bind: skeleton
            .inverse_bind_matrices
            .iter()
            .map(|m| m.to_cols_array())
            .collect(),
    }
}

fn snapshot_mesh(data: &FeedMeshData) -> SnapshotMesh {
    SnapshotMesh {
        positions: data.positions.clone(),
        normals: data.normals.clone(),
        uv0: data.uv0.clone(),
        colors: data.colors.clone(),
        joints: data.joints.clone(),
        weights: data.weights.clone(),
        skin: None,
        indices: data.indices.clone(),
        submeshes: data.submeshes.clone(),
        slots: data
            .slots
            .iter()
            .map(|s| SnapshotSlot {
                name: s.name.clone(),
                base_color: s.base_color,
                metallic: s.metallic,
                roughness: s.roughness,
                emissive: s.emissive,
                textures: s.textures.clone(),
            })
            .collect(),
    }
}

fn from_bake(error: &BakeError) -> RpcError {
    refused(error.code(), error.to_string())
}

fn project_of(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| refused("internal", "no project is open"))
}

/// The items a request names.
fn select_items(world: &World, params: &Value) -> Result<Vec<Guid>, RpcError> {
    let registry = world
        .get_resource::<DccFeedRegistry>()
        .ok_or_else(|| refused("dcc_feed_disabled", "this app has no DccFeedPlugin"))?;
    let items = params
        .get("items")
        .ok_or_else(|| RpcError::invalid_params("`items` (keys, or \"all\") is required"))?;
    if items.as_str() == Some("all") {
        let session = params.get("session").and_then(Value::as_u64);
        return Ok(registry
            .table
            .iter()
            .filter(|i| session.is_none_or(|id| i.session == Some(id)))
            .map(|i| i.guid)
            .collect());
    }
    let keys = items
        .as_array()
        .ok_or_else(|| RpcError::invalid_params("`items` must be an array of keys or \"all\""))?;
    keys.iter()
        .map(|key| {
            let key = key
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`items` must hold strings"))?;
            registry
                .table
                .iter()
                .find(|i| i.key == key)
                .map(|i| i.guid)
                .ok_or_else(|| refused("item_not_found", format!("no feed item {key:?}")))
        })
        .collect()
}

fn overrides_of(params: &Value, warnings: &mut Vec<String>) -> ImportOverrides {
    let import = params.get("import").cloned().unwrap_or(Value::Null);
    if import.get("generate_tangents").is_some() {
        warnings
            .push("generate_tangents: the importer generates tangents when a material needs them".to_owned());
    }
    if import.get("forward").is_some() {
        warnings
            .push("forward: baked files keep object space; the project's import setting applies".to_owned());
    }
    let scale = import.get("scale").and_then(Value::as_f64).map(|s| s as f32);
    let scale = match scale {
        Some(s) if s.is_finite() && s > 0.0 => Some(s),
        Some(s) => {
            warnings.push(format!("import.scale {s}: not a finite number above 0; ignored"));
            None
        }
        None => None,
    };
    ImportOverrides {
        scale,
        lods: import
            .get("lods")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        collision: import.get("collision").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn plan_json(plan: &BakePlan) -> (Vec<Value>, Vec<String>) {
    let files = plan
        .files
        .iter()
        .map(|f| json!({ "key": f.key, "guid": f.guid.to_string(), "path": f.path }))
        .collect();
    let plain = plan.plain.iter().map(ToString::to_string).collect();
    (files, plain)
}

/// `dcc_feed.bake`.
pub fn bake(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // Normalize separators once: `check_relative` splits on `/` and `\`, but the plan strips
    // `/`-only, so a Windows-spelled folder would plan a backslash-bearing path and bake a
    // divergent layout per OS (audit G finding 11).
    let folder = params
        .get("folder")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`folder` (under assets/) is required"))?
        .replace('\\', "/");
    let root = project_of(world)?;
    if !folder.trim_matches('/').is_empty() {
        regia_core::project_path::check_relative(folder.trim_matches('/'))
            .map_err(|e| refused("path_denied", e.to_string()))?;
    }
    let items = select_items(world, params)?;
    let mut warnings = Vec::new();
    let request = BakeRequest {
        items,
        folder,
        import: overrides_of(params, &mut warnings),
        replace_live: params
            .get("replace_live")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        dry_run: params.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
    };
    if request.items.is_empty() {
        return Err(from_bake(&BakeError::NothingToBake));
    }
    let snapshot = snapshot_of(world.resource::<DccFeedRegistry>());
    let plan = plan_bake(&snapshot, &request).map_err(|e| from_bake(&e))?;
    warnings.extend(plan.warnings.iter().cloned());
    let (files, plain) = plan_json(&plan);
    if request.dry_run {
        return Ok(json!({ "dry_run": true, "files": files, "plain": plain, "warnings": warnings }));
    }
    for file in &plan.files {
        let target = regia_core::project_path::resolve_write(&root, &file.path)
            .map_err(|e| refused("path_denied", e.to_string()))?;
        if target.exists() {
            return Err(from_bake(&BakeError::TargetExists(file.path.clone())));
        }
    }
    let mut jobs = world.get_resource_or_insert_with(FeedBakeJobs::default);
    if jobs.running.is_some() {
        return Err(refused(
            "bake_running",
            "a bake is running; poll dcc_feed.bake_status",
        ));
    }
    jobs.next_id += 1;
    let id = jobs.next_id;
    let job_files = plan
        .files
        .iter()
        .map(|f| (f.guid, f.path.clone(), f.scale))
        .collect();
    let plain_items = plan.plain.clone();
    let lods = request.import.lods;
    let worker = std::thread::Builder::new()
        .name("dcc-feed-bake".into())
        .spawn(move || run_bake(&root, &plan, lods))
        .map_err(|e| refused("internal", e.to_string()))?;
    jobs.running = Some(RunningBake {
        id,
        worker,
        files: job_files,
        plain: plain_items,
        warnings: warnings.clone(),
        replace_live: request.replace_live,
    });
    Ok(json!({ "job": id, "files": files, "plain": plain, "warnings": warnings }))
}

/// Writes the plan's files, cooks the project and stamps the provenance. Worker thread.
fn run_bake(root: &Path, plan: &BakePlan, lods: Option<u32>) -> WorkerResult {
    let _span = tracing::info_span!("dcc_feed_bake").entered();
    let mut written: Vec<PathBuf> = Vec::new();
    let result = bake_files(root, plan, lods, &mut written);
    if result.is_err() {
        // A failed bake must not leave its own files behind: the retry would be refused with
        // `target_exists` for them (audit 11-03 editor M5). Only files this bake created are
        // removed, with the `.meta` next to each (written here or by the cook).
        for target in &written {
            if let Err(error) = std::fs::remove_file(target) {
                tracing::warn!(target: "regia_editor", "cannot remove {}: {error}", target.display());
            }
            let meta = regia_asset::meta_path_for(target);
            if meta.is_file() {
                let _ = std::fs::remove_file(meta);
            }
        }
    }
    result
}

fn bake_files(root: &Path, plan: &BakePlan, lods: Option<u32>, written: &mut Vec<PathBuf>) -> WorkerResult {
    for file in &plan.files {
        let target = root.join(&file.path);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(|e| BakeError::Io(format!("{}: {e}", dir.display())))?;
        }
        let mut bytes = Vec::new();
        write_glb(file, &mut bytes)?;
        let tmp = target.with_extension("glb.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| BakeError::Io(format!("{}: {e}", tmp.display())))?;
        let published = publish_no_replace(&tmp, &target);
        let _ = std::fs::remove_file(&tmp);
        match published {
            Ok(()) => written.push(target),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // The request-time check passed, but a file appeared since (audit 11-03 editor M4).
                return Err(BakeError::TargetExists(file.path.clone()));
            }
            Err(error) => return Err(BakeError::Io(format!("{}: {error}", target.display()))),
        }
    }
    if let Some(count) = lods {
        for target in written.iter() {
            write_lod_settings(target, count)?;
        }
    }
    let cook =
        regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(root.to_path_buf()))
            .map_err(|e| BakeError::Import(e.to_string()))?;
    let failed: Vec<String> = cook
        .errors
        .iter()
        .filter(|(path, _)| plan.files.iter().any(|f| f.path == *path))
        .map(|(path, message)| format!("{path}: {message}"))
        .collect();
    if !failed.is_empty() {
        return Err(BakeError::Import(failed.join("; ")));
    }
    let mut assets = Vec::new();
    for target in written.iter() {
        let provenance = regia_asset::Provenance {
            origin: Some(format!("dcc_feed: {}", target.display())),
            tool: regia_asset::ProvenanceTool::Generator,
            generator: Some("dcc_feed".to_owned()),
            created_utc: Some(crate::content::rfc3339_utc_now()),
            ..regia_asset::Provenance::default()
        };
        crate::content::stamp_provenance(target, &provenance).map_err(BakeError::Io)?;
        if let Ok(bytes) = std::fs::read(regia_asset::meta_path_for(target))
            && let Ok(meta) = regia_asset::MetaSidecar::from_ron(&bytes)
        {
            assets.push(meta.id.to_string());
        }
    }
    Ok(assets)
}

/// Publishes `tmp` at `target` without ever replacing an existing file: a hard link fails with
/// `AlreadyExists` when `target` exists (a rename would replace it). On a file system without
/// hard links the bytes are copied into a `create_new` file instead.
fn publish_no_replace(tmp: &Path, target: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(tmp, target) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)?;
            let mut from = std::fs::File::open(tmp)?;
            if let Err(error) = std::io::copy(&mut from, &mut out).and_then(|_| out.sync_all()) {
                drop(out);
                let _ = std::fs::remove_file(target);
                return Err(error);
            }
            Ok(())
        }
        other => other,
    }
}

/// Writes a `.meta` with LOD generation on, before the first cook reads it.
fn write_lod_settings(target: &Path, count: u32) -> Result<(), BakeError> {
    let mut meta = regia_asset::MetaSidecar::new("gltf", regia_asset::AssetId::new_random());
    meta.settings = Some(format!("(generate_lods: Some((count: {count})))"));
    let text = meta.to_ron_string().map_err(|e| BakeError::Io(e.to_string()))?;
    std::fs::write(regia_asset::meta_path_for(target), text).map_err(|e| BakeError::Io(e.to_string()))
}

/// Finishes a done worker on the main thread: the entities become ordinary content.
pub fn pump_bakes(world: &mut World) {
    let Some(mut jobs) = world.get_resource_mut::<FeedBakeJobs>() else {
        return;
    };
    if !jobs.running.as_ref().is_some_and(|job| job.worker.is_finished()) {
        return;
    }
    if let Some(job) = jobs.running.take() {
        finish_bake(world, job);
    }
}

/// Waits for the worker (it has finished, outside tests) and applies its result.
fn finish_bake(world: &mut World, job: RunningBake) {
    let RunningBake {
        id,
        worker,
        files,
        plain,
        warnings,
        replace_live,
    } = job;
    let finished = worker
        .join()
        .unwrap_or_else(|_| Err(BakeError::Io("the bake thread panicked".to_owned())));
    let report = match finished {
        Ok(assets) => {
            let converted = if replace_live {
                convert_items(world, &files, &plain)
            } else {
                Vec::new()
            };
            let glb_paths: Vec<&str> = files.iter().map(|(_, path, _)| path.as_str()).collect();
            json!({
                "job": id,
                "state": "done",
                "assets": assets,
                "glb_paths": glb_paths,
                "converted": converted,
                "warnings": warnings,
            })
        }
        Err(error) => json!({
            "job": id,
            "state": "failed",
            "reason": error.code(),
            "error": error.to_string(),
            "warnings": warnings,
        }),
    };
    let mut jobs = world.resource_mut::<FeedBakeJobs>();
    if jobs.finished.len() >= FINISHED_KEPT {
        jobs.finished.remove(0);
    }
    jobs.finished.push(FinishedBake { id, report });
}

/// Turns baked items into ordinary entities (spec § 2.4 rule 6): saved, editable, no longer
/// addressed by the stream, with a fresh guid so a later declaration of the key gets a new entity.
fn convert_items(world: &mut World, files: &[(Guid, String, f32)], plain: &[Guid]) -> Vec<String> {
    let level = world.get_resource::<OpenLevel>().map(|open| open.id);
    let mut converted = Vec::new();
    let plain = plain.iter().map(|guid| (*guid, None));
    let meshes = files
        .iter()
        .map(|(guid, path, scale)| (*guid, Some((path.as_str(), *scale))));
    for (guid, file) in plain.chain(meshes).collect::<Vec<_>>() {
        let Some(entity) = regia_agent::dcc_feed::methods::mark_baked(world, guid) else {
            continue;
        };
        let mut e = world.entity_mut(entity);
        e.insert(EntityGuid(Guid::new_random()));
        if let Some(level) = level {
            e.insert(LevelMember::from_level(level));
        }
        if let Some((path, scale)) = file {
            e.insert(MeshSource {
                mesh: path.trim_start_matches("assets/").to_owned(),
                material: String::new(),
            });
            if let Some(mut transform) = e.get_mut::<Transform>()
                && scale != 1.0
            {
                transform.scale /= scale;
            }
        }
        converted.push(guid.to_string());
    }
    converted
}

/// `dcc_feed.bake_status`.
pub fn bake_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = params
        .get("job")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`job` (from dcc_feed.bake) is required"))?;
    let jobs = world.get_resource::<FeedBakeJobs>();
    if jobs
        .and_then(|j| j.running.as_ref())
        .is_some_and(|job| job.id == id)
    {
        return Ok(json!({ "job": id, "state": "running" }));
    }
    jobs.and_then(|j| j.finished.iter().find(|f| f.id == id))
        .map(|f| f.report.clone())
        .ok_or_else(|| refused("job_not_found", format!("no bake job {id}")))
}

/// The prompt for clients waiting in `pending` while the editor has focus (spec § 6).
pub fn accept_prompts(ctx: &egui::Context, world: &mut World) {
    let Some(server) = world.get_resource::<DccFeedServer>() else {
        return;
    };
    if !server.is_live() || !ctx.input(|i| i.focused) {
        return;
    }
    let pending: Vec<(u64, String, String)> = lock(server.core())
        .sessions
        .iter()
        .filter(|s| s.phase == Phase::Pending)
        .map(|s| (s.id, s.client.clone(), s.dcc.clone()))
        .collect();
    if pending.is_empty() {
        return;
    }
    let mut action = None;
    egui::Window::new("DCC feed")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -12.0))
        .show(ctx, |ui| {
            for (id, client, dcc) in &pending {
                ui.label(format!("{client} ({dcc}) wants to stream into this scene."));
                ui.horizontal(|ui| {
                    if ui.button("Accept").clicked() {
                        action = Some(Action::Accept(*id, false));
                    }
                    if ui.button("Always accept").clicked() {
                        action = Some(Action::Accept(*id, true));
                    }
                    if ui.button("Reject").clicked() {
                        action = Some(Action::Reject(*id));
                    }
                });
            }
        });
    if let Some(action) = action {
        run_action(world, action);
    }
}

// ---------------------------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------------------------

/// What a panel click asks for; run after the UI, when the world is free.
enum Action {
    Listen(bool),
    Rotate,
    Accept(u64, bool),
    Reject(u64),
    Bake(Value),
    Record(Value),
    Replay(Value),
}

/// The state icon and word of an item: never color alone (spec § 2.2).
fn state_mark(state: ItemState) -> &'static str {
    match state {
        ItemState::Declared => "◌ declared",
        ItemState::Active => "● active",
        ItemState::Stalled => "◐ stalled",
        ItemState::Detached => "○ detached",
        ItemState::Baked => "■ baked",
        ItemState::Removed => "\u{1f5d9} removed",
    }
}

/// The text the outliner adds to a feed-owned entity.
#[must_use]
pub fn outliner_badge(world: &World, entity: Entity) -> Option<String> {
    let item = world.get::<FeedItem>(entity)?;
    Some(match item.state {
        ItemState::Stalled => "FEED stalled".to_owned(),
        ItemState::Detached => "FEED detached".to_owned(),
        _ => "FEED".to_owned(),
    })
}

/// The DCC feed panel.
pub fn dcc_feed_panel(ui: &mut egui::Ui, world: &mut World) {
    pump_bakes(world);
    let mut actions = Vec::new();
    if world.get_resource::<DccFeedServer>().is_none() {
        ui.weak("The DCC feed is not in this build.");
        return;
    }
    world.get_resource_or_insert_with(FeedBakeJobs::default);
    world.resource_scope(|world, mut jobs: Mut<FeedBakeJobs>| {
        draw(ui, world, &mut jobs, &mut actions);
    });
    for action in actions {
        run_action(world, action);
    }
}

fn run_action(world: &mut World, action: Action) {
    let result = match action {
        Action::Listen(enabled) => methods::listen(world, &json!({ "enabled": enabled })),
        Action::Rotate => methods::rotate_token(world, &Value::Null),
        Action::Accept(session, remember) => {
            methods::accept(world, &json!({ "session": session, "remember": remember }))
        }
        Action::Reject(session) => methods::disconnect(world, &json!({ "session": session })),
        Action::Bake(params) => bake(world, &params),
        Action::Record(params) => methods::record(world, &params),
        Action::Replay(params) => methods::replay(world, &params),
    };
    if let Err(error) = result {
        tracing::error!(target: "regia_editor", "DCC feed: {}", error.message);
    }
}

/// The panel's import settings as the `import` param.
fn import_params(jobs: &FeedBakeJobs) -> Value {
    let mut import = json!({ "collision": jobs.collision });
    if jobs.scale > 0.0 {
        import["scale"] = json!(jobs.scale);
    }
    if jobs.lods > 0 {
        import["lods"] = json!(jobs.lods);
    }
    import
}

fn draw(ui: &mut egui::Ui, world: &World, jobs: &mut FeedBakeJobs, actions: &mut Vec<Action>) {
    let (Some(server), Some(registry), Some(settings)) = (
        world.get_resource::<DccFeedServer>(),
        world.get_resource::<DccFeedRegistry>(),
        world.get_resource::<DccFeedSettings>(),
    ) else {
        return;
    };
    let listening = !server.listeners().is_empty();
    ui.horizontal(|ui| {
        ui.strong("DCC feed");
        let mut on = listening;
        if ui.checkbox(&mut on, "Listening").changed() {
            actions.push(Action::Listen(on));
        }
    });
    for listener in server.listeners() {
        ui.horizontal(|ui| {
            ui.weak(listener.transport);
            let mut address = listener.address.clone();
            ui.add(egui::TextEdit::singleline(&mut address).desired_width(f32::INFINITY));
        });
    }
    if settings.allow_remote {
        ui.colored_label(
            egui::Color32::YELLOW,
            "⚠ allow_remote is on: the protocol has no TLS. Use an SSH tunnel or a VPN.",
        );
    }
    ui.horizontal(|ui| {
        ui.checkbox(&mut jobs.reveal_token, "Show token");
        if ui.button("Rotate token").clicked() {
            actions.push(Action::Rotate);
        }
    });
    let core = lock(server.core());
    if jobs.reveal_token {
        let mut token = core.token().to_owned();
        ui.add(egui::TextEdit::singleline(&mut token).desired_width(f32::INFINITY));
    }
    ui.separator();
    ui.strong("Sessions");
    ui.checkbox(&mut jobs.remember, "Remember this client");
    egui::Grid::new("dcc-feed-sessions").striped(true).show(ui, |ui| {
        for session in core.sessions.iter() {
            ui.label(session.id.to_string());
            ui.label(&session.client);
            ui.label(&session.dcc);
            ui.label(session.phase.name());
            if session.phase == Phase::Pending {
                if ui.button("Accept").clicked() {
                    actions.push(Action::Accept(session.id, jobs.remember));
                }
                if ui.button("Reject").clicked() {
                    actions.push(Action::Reject(session.id));
                }
            } else if session.phase == Phase::Active && ui.button("Disconnect").clicked() {
                actions.push(Action::Reject(session.id));
            }
            ui.end_row();
        }
    });
    let now_s = core.now_s;
    let recording = core.recording.is_some();
    let replay = core.replay_progress();
    drop(core);
    ui.separator();
    ui.horizontal(|ui| {
        ui.strong("Items");
        ui.add(egui::TextEdit::singleline(&mut jobs.folder).hint_text("folder under assets/"));
        let can_bake = !jobs.folder.is_empty() && jobs.running.is_none();
        if ui.add_enabled(can_bake, egui::Button::new("Bake all")).clicked() {
            actions.push(Action::Bake(
                json!({ "items": "all", "folder": jobs.folder, "import": import_params(jobs) }),
            ));
        }
    });
    ui.horizontal(|ui| {
        ui.label("Scale");
        ui.add(
            egui::DragValue::new(&mut jobs.scale)
                .speed(0.01)
                .range(0.0..=1000.0),
        );
        ui.label("LODs");
        ui.add(egui::DragValue::new(&mut jobs.lods).range(0..=4));
        ui.checkbox(&mut jobs.collision, "Collision");
    });
    egui::Grid::new("dcc-feed-items").striped(true).show(ui, |ui| {
        for item in registry.table.iter() {
            ui.label(&item.key);
            ui.label(item.kind.name());
            ui.label(state_mark(item.state));
            ui.label(item.frames_applied.to_string());
            ui.label(format!("{:.1} s", (now_s - item.last_frame_s).max(0.0)));
            let can_bake = !jobs.folder.is_empty() && jobs.running.is_none();
            let bakeable = !matches!(item.kind, FeedKind::Skeleton | FeedKind::Generic);
            if bakeable && ui.add_enabled(can_bake, egui::Button::new("Bake...")).clicked() {
                actions.push(Action::Bake(json!({
                    "items": [item.key],
                    "folder": jobs.folder,
                    "import": import_params(jobs),
                })));
            }
            ui.end_row();
        }
    });
    ui.separator();
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut jobs.record_path).hint_text("take.rfdrec"));
        if recording {
            if ui.button("Stop").clicked() {
                actions.push(Action::Record(json!({ "action": "stop" })));
            }
        } else if ui
            .add_enabled(!jobs.record_path.is_empty(), egui::Button::new("Record"))
            .clicked()
        {
            actions.push(Action::Record(
                json!({ "action": "start", "path": jobs.record_path }),
            ));
        }
    });
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut jobs.replay_path).hint_text("take.rfdrec"));
        if ui
            .add_enabled(!jobs.replay_path.is_empty(), egui::Button::new("Replay"))
            .clicked()
        {
            actions.push(Action::Replay(json!({ "path": jobs.replay_path })));
        }
        if let Some((done, total)) = replay {
            ui.label(format!("{done} / {total}"));
        }
    });
    if let Some(last) = jobs.finished.last() {
        ui.weak(format!(
            "Last bake: {} ({} files)",
            last.report["state"].as_str().unwrap_or("?"),
            last.report["glb_paths"].as_array().map_or(0, Vec::len)
        ));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::{Arc, Mutex};

    use regia_agent::dcc_feed::core::FeedCore;
    use regia_agent::dcc_feed::session::{SessionId, Transport};
    use regia_agent::dcc_feed::wire::{
        Elem, FrameInfo, Hello, ItemDeclare, MeshSection, MeshUpdate, Message, TransformBlock,
        TransformFrame, section,
    };
    use regia_agent::dcc_feed::{DccFeedClock, DccFeedPlugin, item_guid};
    use regia_app::App;
    use regia_core::math::{Mat4, Quat, Vec3};
    use regia_render::{LabMaterial, MeshRenderer, MeshSource};
    use regia_scene::prelude::{FeedOwned, snapshot_scene};
    use regia_scene::spawn::SnapshotFilter;

    use super::*;

    #[test]
    fn a_non_finite_or_non_positive_scale_override_is_dropped_with_a_warning() {
        // 1e40 does not fit an f32: the old filter kept it and every baked vertex became
        // infinite. Such a scale is dropped with a warning; finite positive scales are kept.
        for (scale, kept) in [(1e40f64, false), (0.0, false), (-2.0, false), (2.5, true)] {
            let params = serde_json::json!({ "import": { "scale": scale } });
            let mut warnings = Vec::new();
            let overrides = overrides_of(&params, &mut warnings);
            assert_eq!(overrides.scale.is_some(), kept, "scale {scale}");
            assert_eq!(warnings.is_empty(), kept, "scale {scale}");
        }
    }

    fn floats(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    fn sec(section_id: u8, elem: Elem, count: u32, data: Vec<u8>) -> MeshSection {
        MeshSection {
            section_id,
            elem,
            index: 0,
            count,
            data,
        }
    }

    fn info(frame_id: u64) -> FrameInfo {
        FrameInfo {
            frame_id,
            source_time_ns: 0,
            timecode: None,
        }
    }

    fn app(dir: &Path) -> (App, Arc<Mutex<FeedCore>>, SessionId) {
        let settings = DccFeedSettings {
            auto_accept: vec!["tool".to_owned()],
            ..DccFeedSettings::default()
        };
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.add_plugins(DccFeedPlugin {
            project_dir: Some(dir.to_path_buf()),
        });
        crate::mesh_source::register_level_types(app.world_mut());
        let server = DccFeedServer::new(&settings, Some(dir.to_path_buf()));
        let core = server.core().clone();
        app.insert_resource(settings);
        app.insert_resource(server);
        app.insert_resource(DccFeedClock {
            fixed_step_s: Some(1.0 / 60.0),
            now_s: 0.0,
        });
        app.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.to_path_buf()),
        });
        let session = {
            let mut c = lock(&core);
            let s = c.open(Transport::Unix, "unix", None);
            c.submit(
                s,
                1,
                40,
                Message::Hello(Hello {
                    min_version: 1,
                    max_version: 1,
                    client_name: "tool".to_owned(),
                    client_version: "1".to_owned(),
                    dcc: "test".to_owned(),
                    space: "gltf".to_owned(),
                    unit_scale: 1.0,
                    caps: 0,
                    token: String::new(),
                }),
            );
            s
        };
        (app, core, session)
    }

    fn declare(core: &Mutex<FeedCore>, s: SessionId, handle: u32, key: &str) {
        lock(core).submit(
            s,
            2,
            40,
            Message::ItemDeclare(ItemDeclare {
                handle,
                key: key.to_owned(),
                kind: regia_agent::dcc_feed::wire::FeedKind::Mesh,
                parent_handle: 0,
                display_name: key.to_owned(),
                variant: 0,
                channels: Vec::new(),
            }),
        );
    }

    fn slot_table() -> Vec<u8> {
        let mut out = Vec::new();
        let string = |out: &mut Vec<u8>, t: &str| {
            out.extend_from_slice(&(t.len() as u16).to_le_bytes());
            out.extend_from_slice(t.as_bytes());
        };
        string(&mut out, "Paint");
        out.extend(floats(&[1.0, 0.5, 0.25, 1.0, 0.0, 0.5, 0.0, 0.0, 0.0]));
        for _ in 0..4 {
            string(&mut out, "");
        }
        out
    }

    fn mesh(core: &Mutex<FeedCore>, s: SessionId, handle: u32) {
        let block = TransformBlock {
            translation: [1.0, 2.0, 3.0],
            rotation: [
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
            ],
            scale: [2.0; 3],
            world_space: false,
        };
        let positions = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.25];
        let indices: Vec<u8> = [0u32, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect();
        let mut c = lock(core);
        c.submit(
            s,
            3,
            64,
            Message::Transform(TransformFrame {
                handle,
                info: info(1),
                xf: block,
            }),
        );
        c.submit(
            s,
            4,
            64,
            Message::Mesh(MeshUpdate {
                handle,
                info: info(2),
                update_mode: 0,
                sections: vec![
                    sec(section::POSITIONS, Elem::F32, 3, floats(&positions)),
                    sec(section::INDICES, Elem::U32, 3, indices),
                    sec(section::MATERIAL_SLOTS, Elem::U8, 1, slot_table()),
                ],
            }),
        );
    }

    fn world_points(transform: &Transform, points: impl Iterator<Item = [f32; 3]>) -> Vec<Vec3> {
        let m =
            Mat4::from_scale_rotation_translation(transform.scale, transform.rotation, transform.translation);
        points.map(|p| m.transform_point3(Vec3::from(p))).collect()
    }

    fn run_to_end(world: &mut World) {
        let job = world
            .resource_mut::<FeedBakeJobs>()
            .running
            .take()
            .expect("running");
        finish_bake(world, job);
    }

    /// `dcc_feed.bake_status` is a read: a finished worker stays `running` until the editor frame
    /// (or the panel) calls [`pump_bakes`].
    #[test]
    fn bake_status_does_not_apply_a_finished_worker() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_bake_status");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let (mut app, core, s) = app(&dir);
        declare(&core, s, 1, "Hull");
        mesh(&core, s, 1);
        app.update();
        let world = app.world_mut();
        let entity = world
            .resource::<DccFeedRegistry>()
            .table
            .get(item_guid("Hull"))
            .and_then(|i| i.entity)
            .expect("entity");
        assert!(world.get::<FeedOwned>(entity).is_some());
        let started = bake(world, &json!({ "items": ["Hull"], "folder": "live" })).expect("bake");
        let job = started["job"].as_u64().expect("job");
        while !world
            .resource::<FeedBakeJobs>()
            .running
            .as_ref()
            .is_some_and(|running| running.worker.is_finished())
        {
            std::thread::yield_now();
        }
        let report = bake_status(world, &json!({ "job": job })).expect("status");
        assert_eq!(report["state"], json!("running"), "{report}");
        assert!(
            world.get::<FeedOwned>(entity).is_some(),
            "the status poll does not convert the live mesh"
        );
        let method = regia_agent::schema::METHODS
            .iter()
            .find(|method| method.name == "dcc_feed.bake_status")
            .expect("schema");
        assert!(!method.mutating);
        assert!(matches!(method.permission, regia_agent::schema::Permission::Read));
        pump_bakes(world);
        let report = bake_status(world, &json!({ "job": job })).expect("done");
        assert_eq!(report["state"], json!("done"), "{report}");
        assert!(world.get::<FeedOwned>(entity).is_none());
        assert_eq!(
            world.get::<MeshSource>(entity).map(|m| m.mesh.as_str()),
            Some("live/Hull.glb")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec § 8 test 12: the file loads through the normal path, every world position stays, the
    /// entity becomes ordinary and is saved, the provenance is stamped, and `dry_run` writes nothing.
    #[test]
    fn a_bake_turns_a_live_mesh_into_a_saved_asset_at_the_same_pose() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_bake");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let (mut app, core, s) = app(&dir);
        declare(&core, s, 1, "Hull");
        mesh(&core, s, 1);
        app.update();
        let world = app.world_mut();
        let entity = world
            .resource::<DccFeedRegistry>()
            .table
            .get(item_guid("Hull"))
            .and_then(|i| i.entity)
            .expect("entity");
        let live = *world.get::<Transform>(entity).expect("transform");
        assert_ne!(live.rotation, Quat::IDENTITY);
        let renderer = world.get::<MeshRenderer>(entity).expect("renderer");
        let before = world_points(&live, renderer.mesh.vertices.iter().map(|v| v.position));

        let params = json!({ "items": ["Hull"], "folder": "live", "dry_run": true });
        let dry = bake(world, &params).expect("dry run");
        assert_eq!(dry["files"][0]["path"], json!("assets/live/Hull.glb"));
        assert!(
            !dir.join("assets/live/Hull.glb").exists(),
            "dry_run writes nothing"
        );

        let missing = json!({ "items": ["Nope"], "folder": "live" });
        let reason = bake(world, &missing).expect_err("unknown").data.expect("data")["reason"].clone();
        assert_eq!(reason, json!("item_not_found"));
        let escape = json!({ "items": "all", "folder": "../../x" });
        let reason = bake(world, &escape).expect_err("escape").data.expect("data")["reason"].clone();
        assert_eq!(reason, json!("path_denied"));

        let started = bake(world, &json!({ "items": ["Hull"], "folder": "live" })).expect("bake");
        let job = started["job"].as_u64().expect("job");
        run_to_end(world);
        let report = bake_status(world, &json!({ "job": job })).expect("status");
        assert_eq!(report["state"], json!("done"), "{report}");
        assert_eq!(report["glb_paths"], json!(["assets/live/Hull.glb"]));

        let path = dir.join("assets/live/Hull.glb");
        let material = Arc::new(LabMaterial::default());
        let model =
            crate::mesh_source::load_gltf_model(&path, &material).expect("loads through the normal path");
        assert_eq!(model.parts[0].material_name, "Paint");
        let after_transform = *world.get::<Transform>(entity).expect("transform");
        let after = world_points(
            &after_transform,
            model.parts[0].mesh.vertices.iter().map(|v| v.position),
        );
        for (a, b) in before.iter().zip(&after) {
            assert!(a.distance(*b) < 1e-5, "{a} {b}");
        }
        assert_eq!(before.len(), after.len());

        assert!(world.get::<FeedOwned>(entity).is_none(), "ordinary content");
        assert!(world.get::<FeedItem>(entity).is_none());
        assert_eq!(
            world.get::<MeshSource>(entity).map(|m| m.mesh.as_str()),
            Some("live/Hull.glb")
        );
        let file = snapshot_scene(world, &SnapshotFilter::default()).expect("snapshot");
        assert_eq!(file.entities.len(), 1, "save level writes it now");
        let meta_bytes = std::fs::read(regia_asset::meta_path_for(&path)).expect("meta");
        let meta = regia_asset::MetaSidecar::from_ron(&meta_bytes).expect("parses");
        let provenance = meta.provenance.expect("stamped");
        assert_eq!(provenance.generator.as_deref(), Some("dcc_feed"));
        assert_eq!(provenance.tool, regia_asset::ProvenanceTool::Generator);

        // The bake did not claim the key: a later declaration is a fresh entity next to it.
        declare(&core, s, 2, "Hull");
        app.update();
        let again = app
            .world()
            .resource::<DccFeedRegistry>()
            .table
            .get(item_guid("Hull"))
            .and_then(|i| i.entity)
            .expect("entity");
        assert_ne!(again, entity);
        // A second bake of the same target is refused until the file moves.
        mesh(&core, s, 2);
        app.update();
        let again = bake(app.world_mut(), &json!({ "items": ["Hull"], "folder": "live" }));
        let reason = again.expect_err("exists").data.expect("data")["reason"].clone();
        assert_eq!(reason, json!("target_exists"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit G finding 11: `folder` arrives from the agent unnormalized; `check_relative`
    /// splits on `/` and `\` but the plan strips `/`-only, so `assets\live` planned a
    /// backslash-bearing path and baked a divergent layout per OS. The folder is normalized to
    /// `/` before both.
    #[test]
    fn bake_folder_with_backslash_is_normalized() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_folder_backslash");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let (mut app, core, s) = app(&dir);
        declare(&core, s, 1, "Hull");
        mesh(&core, s, 1);
        app.update();
        let dry = bake(
            app.world_mut(),
            &json!({ "items": ["Hull"], "folder": "assets\\live", "dry_run": true }),
        )
        .expect("dry run");
        assert_eq!(dry["files"][0]["path"], json!("assets/live/Hull.glb"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hull_plan(tag: &str) -> (PathBuf, BakePlan) {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let (mut app, core, s) = app(&dir);
        declare(&core, s, 1, "Hull");
        mesh(&core, s, 1);
        app.update();
        let snapshot = snapshot_of(app.world().resource::<DccFeedRegistry>());
        let request = BakeRequest {
            folder: "live".to_owned(),
            ..BakeRequest::default()
        };
        let plan = plan_bake(&snapshot, &request).expect("plan");
        assert_eq!(plan.files[0].path, "assets/live/Hull.glb");
        (dir, plan)
    }

    /// Audit 11-03 editor M4: the request-time `TargetExists` check was not repeated on the
    /// worker, whose rename replaced a file that appeared in between.
    #[test]
    fn a_bake_never_replaces_a_file_that_appeared_after_the_check() {
        let (dir, plan) = hull_plan("dcc_feed_bake_no_replace");
        let target = dir.join("assets/live/Hull.glb");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("dirs");
        std::fs::write(&target, b"the user's own file").expect("write");
        let result = run_bake(&dir, &plan, None);
        assert_eq!(
            std::fs::read(&target).expect("read"),
            b"the user's own file",
            "the worker replaced an existing file"
        );
        assert!(
            matches!(result, Err(BakeError::TargetExists(ref path)) if path == "assets/live/Hull.glb"),
            "{result:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit 11-03 editor M5: a bake that failed after writing its files left them behind, so
    /// the retry was refused with `TargetExists` for the failed attempt's own files.
    #[test]
    fn a_failed_bake_removes_the_files_it_wrote() {
        let (dir, plan) = hull_plan("dcc_feed_bake_cleanup");
        let target = dir.join("assets/live/Hull.glb");
        // The `.meta` path is taken by a directory, so writing the LOD settings fails after the
        // `.glb` was written.
        std::fs::create_dir_all(regia_asset::meta_path_for(&target)).expect("blocker");
        let result = run_bake(&dir, &plan, Some(2));
        assert!(result.is_err(), "{result:?}");
        assert!(!target.exists(), "the failed bake left its .glb behind");
        assert!(
            !target.with_extension("glb.tmp").exists(),
            "the failed bake left its temp file behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_snapshot_bakes_to_the_same_bytes() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_bake_bytes");
        let (mut app, core, s) = app(&dir);
        declare(&core, s, 1, "Hull");
        mesh(&core, s, 1);
        app.update();
        let snapshot = snapshot_of(app.world().resource::<DccFeedRegistry>());
        let request = BakeRequest {
            folder: "live".to_owned(),
            ..BakeRequest::default()
        };
        let bytes = |plan: &BakePlan| {
            let mut out = Vec::new();
            write_glb(&plan.files[0], &mut out).expect("write");
            out
        };
        let first = plan_bake(&snapshot, &request).expect("plan");
        let second = plan_bake(&snapshot, &request).expect("plan");
        assert_eq!(bytes(&first), bytes(&second));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_editor_refuses_edits_of_a_feed_owned_entity() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_editor_guard");
        let (mut app, core, s) = app(&dir);
        declare(&core, s, 1, "Hull");
        app.update();
        let world = app.world_mut();
        let entity = world
            .resource::<DccFeedRegistry>()
            .table
            .get(item_guid("Hull"))
            .and_then(|i| i.entity)
            .expect("entity");
        let result = crate::hierarchy_ops::delete_entities(world, &[entity]);
        assert!(
            matches!(result, Err(crate::hierarchy_ops::HierarchyError::FeedOwned(_))),
            "{result:?}"
        );
        assert_eq!(outliner_badge(world, entity).as_deref(), Some("FEED"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec § 2.2 and § 6: every item state shows an icon and its word, the icons differ, and
    /// `stalled` and `detached` are told apart without color.
    #[test]
    fn item_states_are_marked_by_icon_and_word() {
        let states = [
            (ItemState::Declared, "declared"),
            (ItemState::Active, "active"),
            (ItemState::Stalled, "stalled"),
            (ItemState::Detached, "detached"),
            (ItemState::Baked, "baked"),
            (ItemState::Removed, "removed"),
        ];
        let mut icons = std::collections::BTreeSet::new();
        for (state, word) in states {
            let (icon, text) = state_mark(state).split_once(' ').expect("icon, space, word");
            assert_eq!(text, word);
            assert!(
                !icon.is_empty() && !icon.chars().any(char::is_alphanumeric),
                "{icon:?}"
            );
            icons.insert(icon);
        }
        assert_eq!(icons.len(), states.len(), "one icon per state");
    }
}
