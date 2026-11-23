// SPDX-License-Identifier: MPL-2.0
//! The generic asset API of `docs/specs/12-assets.md` §§ 2-3 (ADR-7202): `AssetLoader` with
//! `LoadContext` dependency loading, async `load` / `load_path` on the task pools, load groups on
//! `LoadSets`, `AssetEvent`s from the server, reference-counted handles with a grace period counted
//! in frames, and a per-type memory budget. Every test steps the server by calls (no clocks, no
//! sleeps): `wait_for_loads` / `wait_for_next_load` block on the load tasks themselves, and
//! `update` is one frame.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_asset::{
    AssetEvent, AssetId, AssetLoadError, AssetLoader, AssetManifest, AssetServer, AssetServerSettings,
    AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, LoadContext, LoadSets, LoadSettings, LoadState,
    LoadedKind, ManifestEntry, PrepareAsset, StrongHandle, cooked_cache_dir, cooked_file_name,
    encode_cooked_file, pack_importer_version,
};
use regia_core::id::TypePathHash;

const NOTE_TYPE: &str = "asset_api_test::Note";

/// A test asset: a text and the notes it depends on, loaded through the context.
struct Note {
    text: String,
    deps: Vec<StrongHandle<Note>>,
}

struct NoteLoader;

impl AssetLoader for NoteLoader {
    type Asset = Note;

    fn type_tag(&self) -> AssetTypeTag {
        AssetTypeTag(TypePathHash::of(NOTE_TYPE))
    }

    fn load(&self, payload: &[u8], ctx: &mut LoadContext<'_>) -> Result<Note, AssetLoadError> {
        let text = std::str::from_utf8(payload).map_err(|e| AssetLoadError::Invalid(e.to_string()))?;
        let mut lines = text.lines();
        let body = lines.next().unwrap_or_default().to_owned();
        let mut deps = Vec::new();
        for line in lines {
            let id: AssetId = line
                .parse()
                .map_err(|_| AssetLoadError::Invalid(format!("bad dependency id {line:?}")))?;
            deps.push(ctx.load::<Note>(id)?);
        }
        Ok(Note { text: body, deps })
    }

    fn cpu_bytes(&self, asset: &Note) -> u64 {
        asset.text.len() as u64
    }

    fn release_cpu_data(&self, asset: &mut Note) {
        // The heavy CPU part goes; the dependency handles stay, so nothing the GPU copy uses unloads.
        asset.text = String::new();
    }
}

/// Stands in for a renderer upload: counts calls and reports four GPU bytes per text byte.
struct NoteUpload {
    calls: Arc<AtomicU32>,
}

impl PrepareAsset for NoteUpload {
    type Asset = Note;

    fn prepare(&self, _id: AssetId, asset: &Note) -> Result<u64, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if asset.text == "refuse" {
            return Err("upload refused".into());
        }
        Ok(asset.text.len() as u64 * 4)
    }
}

struct Project {
    root: PathBuf,
    cache: PathBuf,
}

impl Project {
    fn new(tag: &str) -> Self {
        let root = regia_test_utils::unique_temp_dir(tag);
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        Self { root, cache }
    }

    /// Writes (or rewrites) a note's cooked file atomically.
    fn write_note(&self, id: AssetId, text: &str, deps: &[AssetId]) {
        let mut payload = text.to_owned();
        for d in deps {
            payload.push('\n');
            payload.push_str(&d.to_string());
        }
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of(NOTE_TYPE),
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: deps.to_vec(),
            payload_len: payload.len() as u64,
        };
        let bytes = encode_cooked_file(&header, payload.as_bytes()).expect("encode");
        write_atomic(&self.cache.join(cooked_file_name(id)), &bytes);
    }

    fn write_manifest(&self, entries: &[(&str, AssetId)]) {
        let manifest = AssetManifest {
            entries: entries
                .iter()
                .map(|(path, id)| ManifestEntry {
                    path: (*path).to_owned(),
                    id: *id,
                    type_path: NOTE_TYPE.to_owned(),
                })
                .collect(),
        };
        manifest.write_to_cache(&self.cache).expect("manifest");
    }

    fn server(&self) -> AssetServer {
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: self.root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("open");
        server.register_loader(NoteLoader);
        server.enable_events::<Note>();
        server
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

fn text_of(server: &AssetServer, handle: &StrongHandle<Note>) -> Option<String> {
    server.get(handle).map(|n| n.text.clone())
}

/// Event kinds and ids, in arrival order.
fn kinds(server: &mut AssetServer) -> Vec<(&'static str, Option<AssetId>)> {
    server
        .drain_events::<Note>()
        .into_iter()
        .map(|e| match e {
            AssetEvent::Added(h) => ("added", server.notes_id_of(h)),
            AssetEvent::Modified(h) => ("modified", server.notes_id_of(h)),
            AssetEvent::Removed(h) => ("removed", server.notes_id_of(h)),
            AssetEvent::LoadFailed { id, .. } => ("load_failed", Some(id)),
        })
        .collect()
}

/// Test-only helper: the id of a slab handle of the note store.
trait NotesIdOf {
    fn notes_id_of(&self, handle: regia_asset::Handle<Note>) -> Option<AssetId>;
}

impl NotesIdOf for AssetServer {
    fn notes_id_of(&self, handle: regia_asset::Handle<Note>) -> Option<AssetId> {
        self.assets::<Note>().and_then(|a| a.id_of_slot(&handle))
    }
}

/// Acceptance 1: a loader with two dependencies resolves both through the context, the edges are
/// recorded both ways, and a change of one dependency reloads it and the asset that uses it.
#[test]
fn a_loader_resolves_two_dependencies_through_the_context_and_reloads_when_one_changes() {
    let p = Project::new("asset-api-deps");
    let (a, b, c) = (
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
    );
    p.write_note(b, "bee", &[]);
    p.write_note(c, "sea", &[]);
    p.write_note(a, "ay", &[b, c]);
    let mut server = p.server();

    let ha = server.load::<Note>(a).expect("a loader is registered");
    assert_eq!(
        server.state(a),
        LoadState::Loading,
        "load returns before the bytes are read"
    );
    server.wait_for_loads();
    for id in [a, b, c] {
        assert_eq!(server.state(id), LoadState::Loaded, "{id}");
    }
    let note = server.get(&ha).expect("a is loaded");
    assert_eq!(note.text, "ay");
    let dep_texts: Vec<String> = note
        .deps
        .iter()
        .map(|h| text_of(&server, h).expect("dependency loaded"))
        .collect();
    assert_eq!(dep_texts, ["bee", "sea"]);
    assert_eq!(server.dependencies_of(a), vec![b, c]);
    assert_eq!(server.dependents_of(b), vec![a]);
    assert_eq!(server.dependents_of(c), vec![a]);
    // Only the handle inside `a` holds `b`.
    assert_eq!(server.strong_count(b), 1);
    let _ = kinds(&mut server);

    // `b` changes on disk: the hot-reload entry point reloads `b` and then `a`, which uses it.
    p.write_note(b, "bumblebee", &[]);
    assert_eq!(server.reload_id(b).expect("reload"), Some(LoadedKind::Generic));
    server.wait_for_loads();
    let note = server.get(&ha).expect("a is still loaded");
    assert_eq!(text_of(&server, &note.deps[0]).as_deref(), Some("bumblebee"));
    assert_eq!(server.generation(b), 1);
    assert_eq!(server.generation(a), 1, "the dependent reloaded too");
    assert_eq!(server.generation(c), 0, "the other dependency did not");
    assert_eq!(
        kinds(&mut server),
        vec![("modified", Some(b)), ("modified", Some(a))],
        "the dependency first, then its user"
    );
    assert_eq!(server.take_reloaded_dependents(), vec![a]);
}

/// Acceptance 2: `load_path` of a path the manifest does not list reports `asset_not_found`; `load`
/// of an id with no cooked bytes fails through its state and a `LoadFailed` event; a type without a
/// loader reports `asset_no_loader`. Nothing panics.
#[test]
fn loading_a_missing_path_reports_the_documented_error_without_panicking() {
    let p = Project::new("asset-api-missing");
    let present = AssetId::new_random();
    p.write_note(present, "here", &[]);
    p.write_manifest(&[("assets/here.note", present)]);
    let mut server = p.server();

    let err = server
        .load_path::<Note>("assets/nope.note")
        .expect_err("not in the manifest");
    assert!(matches!(err, AssetLoadError::NotFound(ref path) if path == "assets/nope.note"));
    assert_eq!(err.code(), "asset_not_found");

    let found = server.load_path::<Note>("assets/here.note").expect("listed");
    server.wait_for_loads();
    assert_eq!(text_of(&server, &found).as_deref(), Some("here"));

    let ghost = AssetId::new_random();
    let handle = server
        .load::<Note>(ghost)
        .expect("async: the failure arrives later");
    server.wait_for_loads();
    assert!(matches!(server.state(ghost), LoadState::Failed(_)));
    assert!(server.get(&handle).is_none());
    let events = kinds(&mut server);
    assert!(events.contains(&("load_failed", Some(ghost))), "{events:?}");

    struct Unregistered;
    let err = server
        .load::<Unregistered>(AssetId::new_random())
        .expect_err("no loader");
    assert_eq!(err.code(), "asset_no_loader");
}

/// Acceptance 3: a load group reports progress that never goes down, reads the same through
/// `LoadSets` (one progress source for loading screens), and completes exactly once.
#[test]
fn a_group_reports_monotonic_progress_and_completes_once() {
    let p = Project::new("asset-api-group");
    let ids: Vec<AssetId> = (0..4).map(|_| AssetId::new_random()).collect();
    for (i, id) in ids.iter().enumerate() {
        p.write_note(*id, &format!("n{i}"), &[]);
    }
    let mut server = p.server();
    let sets = LoadSets::default();
    server.set_load_sets(sets.clone());

    // One item is already resident before the group asks for it.
    let early = server.load::<Note>(ids[0]).expect("load");
    server.wait_for_loads();

    let group = server.load_group::<Note>(&ids).expect("group");
    let first = server.group_progress(&group).expect("known group");
    assert_eq!((first.loaded, first.total), (1, 4));
    assert!(!first.complete);
    let mut last = first.fraction;
    let mut completions = 0;
    while server.wait_for_next_load() {
        server.update();
        let p = server.group_progress(&group).expect("known group");
        assert!(p.fraction >= last, "progress went down: {last} -> {}", p.fraction);
        last = p.fraction;
        let shared = sets.load_progress(group.name()).expect("the group is a load set");
        assert_eq!(shared.fraction, p.fraction, "LoadSets reads the same progress");
        completions += server
            .take_completed_groups()
            .iter()
            .filter(|n| n.as_str() == group.name())
            .count();
    }
    for _ in 0..3 {
        server.update();
        completions += server
            .take_completed_groups()
            .iter()
            .filter(|n| n.as_str() == group.name())
            .count();
    }
    let done = server.group_progress(&group).expect("known group");
    assert!(done.complete);
    assert_eq!((done.loaded, done.total, done.fraction), (4, 4, 1.0));
    assert_eq!(completions, 1, "a group completes once");
    drop(early);
}

/// Acceptance 4: `Added`, `Modified` and `Removed` arrive in that order, from the generic store and
/// from the built-in slabs (`Assets::remove` still works and reports too).
#[test]
fn asset_events_arrive_in_order() {
    let p = Project::new("asset-api-events");
    let a = AssetId::new_random();
    p.write_note(a, "one", &[]);
    let mut server = p.server();
    server.set_unload_grace_frames(0);

    let h = server.load::<Note>(a).expect("load");
    server.wait_for_loads();
    p.write_note(a, "two", &[]);
    server.reload_id(a).expect("reload");
    assert_eq!(text_of(&server, &h).as_deref(), Some("two"));
    drop(h);
    server.update();
    assert_eq!(server.state(a), LoadState::NotLoaded);
    assert_eq!(
        kinds(&mut server),
        vec![("added", Some(a)), ("modified", Some(a)), ("removed", Some(a))]
    );
    assert!(kinds(&mut server).is_empty(), "drained");

    // The slab itself reports when its events are on.
    let mut slab: regia_asset::Assets<u32> = regia_asset::Assets::new();
    slab.enable_events();
    let id = AssetId::new_random();
    let h1 = slab.insert(id, 1);
    slab.insert(id, 2);
    assert!(slab.remove(id));
    let got: Vec<String> = slab
        .drain_events()
        .into_iter()
        .map(|e| match e {
            AssetEvent::Added(h) => format!("added {}", h == h1),
            AssetEvent::Modified(h) => format!("modified {}", h == h1),
            AssetEvent::Removed(h) => format!("removed {}", h == h1),
            AssetEvent::LoadFailed { .. } => "failed".into(),
        })
        .collect();
    assert_eq!(got, ["added true", "modified true", "removed true"]);
}

/// Acceptance 5: dropping the last strong handle unloads after exactly the grace frames; taking a
/// handle again inside the grace keeps the asset; a weak handle revives only inside the grace; the
/// unload releases the dependencies, which then run their own grace.
#[test]
fn dropping_the_last_strong_handle_unloads_after_exactly_the_grace_frames() {
    let p = Project::new("asset-api-grace");
    let (a, b) = (AssetId::new_random(), AssetId::new_random());
    p.write_note(b, "dep", &[]);
    p.write_note(a, "top", &[b]);
    let mut server = p.server();
    server.set_unload_grace_frames(3);
    assert_eq!(server.unload_grace_frames(), 3);

    let h = server.load::<Note>(a).expect("load");
    server.wait_for_loads();
    let weak = h.downgrade();
    assert_eq!(server.strong_count(a), 1);
    drop(h);
    for frame in 1..=2 {
        server.update();
        assert_eq!(
            server.state(a),
            LoadState::Loaded,
            "frame {frame}: still inside the grace"
        );
        assert!(server.is_pending_unload(a));
    }
    server.update();
    assert_eq!(server.state(a), LoadState::NotLoaded, "unloaded on grace frame 3");
    assert!(
        server.upgrade(&weak).is_none(),
        "nothing to revive after the unload"
    );
    // `a` held the only handle to `b`: its grace starts now.
    assert!(server.is_pending_unload(b));
    for _ in 0..2 {
        server.update();
        assert_eq!(server.state(b), LoadState::Loaded);
    }
    server.update();
    assert_eq!(server.state(b), LoadState::NotLoaded);

    // Re-acquired inside the grace: kept, with no second read.
    let h = server.load::<Note>(a).expect("load");
    server.wait_for_loads();
    let reads = server.generic_loads_started();
    let weak = h.downgrade();
    drop(h);
    server.update();
    server.update();
    let again = server.upgrade(&weak).expect("inside the grace");
    assert!(!server.is_pending_unload(a));
    let third = server.load::<Note>(a).expect("load");
    assert_eq!(
        server.generic_loads_started(),
        reads,
        "no new load for a live asset"
    );
    assert_eq!(server.strong_count(a), 2);
    for _ in 0..10 {
        server.update();
    }
    assert_eq!(text_of(&server, &again).as_deref(), Some("top"));
    drop((again, third));
    for _ in 0..3 {
        server.update();
    }
    assert_eq!(server.state(a), LoadState::NotLoaded);
}

/// Second priority: a per-type memory budget evicts unreferenced assets, oldest release first,
/// and never one that still has a strong handle.
#[test]
fn the_memory_budget_evicts_released_assets_oldest_first() {
    let p = Project::new("asset-api-budget");
    // Ten bytes each.
    let ids: Vec<AssetId> = (0..4).map(|_| AssetId::new_random()).collect();
    for (i, id) in ids.iter().enumerate() {
        p.write_note(*id, &format!("note-{i:05}"), &[]);
    }
    let mut server = p.server();
    server.set_unload_grace_frames(1000);
    let mut handles: Vec<Option<StrongHandle<Note>>> = ids
        .iter()
        .map(|id| Some(server.load::<Note>(*id).expect("load")))
        .collect();
    server.wait_for_loads();
    assert_eq!(server.memory_use::<Note>(), 40);

    // Release 2, then 0, then 1; 3 stays held.
    for i in [2, 0, 1] {
        handles[i] = None;
        server.update();
    }
    server.set_memory_budget::<Note>(Some(25));
    server.update();
    // Over by 15: the two oldest releases go (2, then 0); 1 is enough under budget to stay.
    assert_eq!(server.state(ids[2]), LoadState::NotLoaded);
    assert_eq!(server.state(ids[0]), LoadState::NotLoaded);
    assert_eq!(server.state(ids[1]), LoadState::Loaded);
    assert_eq!(server.memory_use::<Note>(), 20);
    assert_eq!(server.take_budget_evictions(), vec![ids[2], ids[0]]);

    server.set_memory_budget::<Note>(Some(5));
    server.update();
    assert_eq!(server.state(ids[1]), LoadState::NotLoaded);
    assert_eq!(
        server.state(ids[3]),
        LoadState::Loaded,
        "a held asset is never evicted"
    );
    assert_eq!(server.memory_use::<Note>(), 10);
    drop(handles);
}

/// The `assets.handles` report: refcounts, pending unloads and group progress, filtered by kind.
#[test]
fn the_handle_report_lists_counts_pending_unloads_and_groups() {
    let p = Project::new("asset-api-report");
    let (a, b) = (AssetId::new_random(), AssetId::new_random());
    p.write_note(a, "a", &[]);
    p.write_note(b, "b", &[]);
    let mut server = p.server();
    server.set_unload_grace_frames(5);
    let ha = server.load::<Note>(a).expect("load");
    let hb = server.load::<Note>(b).expect("load");
    let group = server.load_group::<Note>(&[a]).expect("group");
    server.wait_for_loads();
    drop(hb);
    server.update();

    let report = server.handle_report(Some("Note"));
    assert_eq!(report.kinds.len(), 1);
    let kind = &report.kinds[0];
    assert_eq!(kind.kind, "Note");
    assert_eq!((kind.loaded, kind.pending_unloads), (2, 1));
    let row_a = report.assets.iter().find(|r| r.id == a).expect("a listed");
    assert_eq!(row_a.strong, 2, "the handle and the group");
    assert_eq!(row_a.unload_in_frames, None);
    let row_b = report.assets.iter().find(|r| r.id == b).expect("b listed");
    assert_eq!((row_b.strong, row_b.unload_in_frames), (0, Some(4)));
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.groups[0].name, group.name());
    assert!(server.handle_report(Some("Mesh")).kinds.is_empty());
    drop(ha);
}

/// Second priority: `PrepareAsset` runs once per load and reload; the CPU copy is released after it
/// unless the load asked for `keep_cpu_data`; a later load that asks for it re-reads the CPU data.
/// A prepare error fails the load.
#[test]
fn prepare_asset_drops_the_cpu_copy_unless_keep_cpu_data() {
    let p = Project::new("asset-api-prepare");
    let (a, b, c, bad) = (
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
    );
    p.write_note(c, "dep", &[]);
    p.write_note(a, "hello", &[c]);
    p.write_note(b, "world!!", &[]);
    p.write_note(bad, "refuse", &[]);
    let mut server = p.server();
    let calls = Arc::new(AtomicU32::new(0));
    server.register_prepare(NoteUpload {
        calls: Arc::clone(&calls),
    });

    let ha = server.load::<Note>(a).expect("load");
    let hb = server
        .load_with::<Note>(b, LoadSettings { keep_cpu_data: true })
        .expect("load");
    server.wait_for_loads();
    assert_eq!(calls.load(Ordering::SeqCst), 3, "a, its dependency c, and b");
    let note = server.get(&ha).expect("a stays loaded");
    assert_eq!(note.text, "", "the CPU copy was released after the upload");
    assert_eq!(note.deps.len(), 1, "its dependency handles stay");
    assert_eq!(server.state(c), LoadState::Loaded);
    assert_eq!(text_of(&server, &hb).as_deref(), Some("world!!"));
    assert_eq!(server.cpu_data_kept(a), Some(false));
    assert_eq!(server.cpu_data_kept(b), Some(true));
    assert_eq!(server.gpu_memory_use::<Note>(), (5 + 7 + 3) * 4);
    assert_eq!(server.memory_use::<Note>(), 7, "only b keeps CPU bytes");

    // A reload prepares again and releases again.
    p.write_note(a, "hello again", &[c]);
    server.reload_id(a).expect("reload");
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(server.get(&ha).map(|n| n.text.as_str()), Some(""));
    assert_eq!(server.gpu_memory_use::<Note>(), (11 + 7 + 3) * 4);

    // Physics wants the CPU data after all: the load re-reads it and keeps it.
    let again = server
        .load_with::<Note>(a, LoadSettings { keep_cpu_data: true })
        .expect("load");
    server.wait_for_loads();
    assert_eq!(text_of(&server, &again).as_deref(), Some("hello again"));
    assert_eq!(server.cpu_data_kept(a), Some(true));

    let refused = server.load::<Note>(bad).expect("load");
    server.wait_for_loads();
    assert!(matches!(server.state(bad), LoadState::Failed(ref e) if e.contains("upload refused")));
    assert!(server.get(&refused).is_none());
}

/// The plugin wiring: `register_asset_loader` forwards the store's events as messages, a load group
/// reports through the app's `LoadSets` and one `LoadGroupCompleted`, and the grace counts
/// `app.update()` frames.
#[test]
fn the_asset_plugin_forwards_events_and_counts_the_grace_in_frames() {
    use regia_app::prelude::*;
    use regia_asset::{AssetAppExt, AssetPlugin, LoadGroupCompleted};

    let p = Project::new("asset-api-app");
    let (a, b) = (AssetId::new_random(), AssetId::new_random());
    p.write_note(a, "top", &[]);
    p.write_note(b, "grouped", &[]);
    let mut app = App::new();
    app.add_plugins(AssetPlugin {
        settings: AssetServerSettings {
            project_root: p.root.clone(),
            platform: "linux".into(),
            content_dir: None,
        },
    });
    app.register_asset_loader(NoteLoader);
    app.finish_plugins();

    let (handle, group) = {
        let mut server = app.world_mut().resource_mut::<AssetServer>();
        server.set_unload_grace_frames(2);
        let handle = server.load::<Note>(a).expect("load");
        let group = server.load_group::<Note>(&[b]).expect("group");
        server.wait_for_loads();
        (handle, group)
    };
    app.update();
    let added = app
        .world_mut()
        .resource_mut::<Messages<AssetEvent<Note>>>()
        .drain()
        .filter(|e| matches!(e, AssetEvent::Added(_)))
        .count();
    assert_eq!(added, 2);
    let completed: Vec<String> = app
        .world_mut()
        .resource_mut::<Messages<LoadGroupCompleted>>()
        .drain()
        .map(|m| m.name)
        .collect();
    assert_eq!(completed, [group.name().to_owned()]);
    let shared = app
        .world()
        .resource::<LoadSets>()
        .load_progress(group.name())
        .expect("the group is a set of the app's registry");
    assert_eq!(shared.fraction, 1.0);

    drop(handle);
    app.update();
    assert_eq!(app.world().resource::<AssetServer>().state(a), LoadState::Loaded);
    app.update();
    assert_eq!(
        app.world().resource::<AssetServer>().state(a),
        LoadState::NotLoaded
    );
    let removed = app
        .world_mut()
        .resource_mut::<Messages<AssetEvent<Note>>>()
        .drain()
        .filter(|e| matches!(e, AssetEvent::Removed(_)))
        .count();
    assert_eq!(removed, 1);
    drop(group);
}

/// A group over an asset that was loaded and unloaded earlier starts at 0: residency is tracked
/// for grouped ids only, and a dropped group's ids leave the registry's resident list.
#[test]
fn a_new_group_does_not_count_an_asset_unloaded_before_it() {
    let p = Project::new("asset-api-group-stale");
    let a = AssetId::new_random();
    p.write_note(a, "once", &[]);
    let mut server = p.server();
    let sets = LoadSets::default();
    server.set_load_sets(sets.clone());
    server.set_unload_grace_frames(0);
    let key = a.to_string();

    // Outside a group: nothing is recorded.
    let h = server.load::<Note>(a).expect("load");
    server.wait_for_loads();
    assert!(!sets.item_resident(regia_asset::LoadKind::Other, &key));
    drop(h);
    server.update();
    assert_eq!(server.state(a), LoadState::NotLoaded);

    // A group loads it; dropping the group unloads it and clears its residency.
    let group = server.load_group::<Note>(&[a]).expect("group");
    server.wait_for_loads();
    assert!(sets.item_resident(regia_asset::LoadKind::Other, &key));
    drop(group);
    server.update();
    server.update();
    assert_eq!(server.state(a), LoadState::NotLoaded);
    assert!(!sets.item_resident(regia_asset::LoadKind::Other, &key));

    let again = server.load_group::<Note>(&[a]).expect("group");
    let first = server.group_progress(&again).expect("known");
    assert_eq!((first.loaded, first.total, first.complete), (0, 1, false));
    server.wait_for_loads();
    server.update();
    assert!(server.group_progress(&again).expect("known").complete);
}
