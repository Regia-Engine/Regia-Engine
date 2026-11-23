// SPDX-License-Identifier: MPL-2.0
//! The Steam Workshop against the mock (ADR-2418): a subscribe leads to a `mods://` mount whose
//! files are readable, an update remounts in place, an unsubscribe unmounts, an upload round-trips
//! through the mock Workshop, the lobby key follows the mounted set, and the `steam.workshop_*`
//! agent methods answer with their codes. No Steam client, no GPU.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_asset::mods_vfs::{MODS_LOBBY_KEY, ModSource, ModsVfs};
use regia_platform::AppLifecycle;
use regia_steam::workshop::UploadStatus;
use regia_steam::{
    AppId, MockSteam, MockWorkshop, SteamBackend, SteamPlugin, SteamWorkshop, SteamWorkshopPlugin,
    WorkshopItemId, WorkshopService, WorkshopUpload, workshop_mod_id,
};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

fn files(entries: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    entries
        .iter()
        .map(|(p, t)| ((*p).to_owned(), t.as_bytes().to_vec()))
        .collect()
}

fn app_with(workshop: &Arc<MockWorkshop>) -> App {
    let mut app = App::new();
    app.insert_resource(AppLifecycle::default());
    app.add_plugins(SteamPlugin::new(AppId(480)).with_backend(MockSteam::shared() as Arc<dyn SteamBackend>));
    app.add_plugins(SteamWorkshopPlugin::with_service(
        Arc::clone(workshop) as Arc<dyn WorkshopService>
    ));
    app.finish_plugins();
    app.update();
    app
}

fn call(app: &mut App, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let extension = app.world().resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is registered"));
    handler(app.world_mut(), &params)
}

fn code(err: &regia_agent::protocol::RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn a_mock_subscribe_mounts_the_item_and_its_files_are_readable() {
    let root = unique_temp_dir("workshop_install");
    let mock = MockWorkshop::shared(&root);
    let item = mock.publish(
        "Bigger guns",
        files(&[("data/units.ron", "(hp: 250)"), ("textures/gun.txt", "gun")]),
    );
    let mut app = app_with(&mock);
    // A base mod the game mounted itself: the Workshop item goes on top of it.
    app.world_mut()
        .resource_mut::<ModsVfs>()
        .mount(
            "base",
            ModSource::Memory(files(&[("data/units.ron", "(hp: 100)"), ("main.lua", "x")])),
        )
        .expect("base");
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .subscribe(item)
        .expect("subscribe");
    app.update();
    let id = workshop_mod_id(item);
    {
        let mods = app.world().resource::<ModsVfs>();
        assert_eq!(mods.order(), ["base".to_owned(), id.clone()]);
        assert_eq!(mods.read("mods://data/units.ron").expect("read"), b"(hp: 250)");
        assert_eq!(mods.read("mods://textures/gun.txt").expect("read"), b"gun");
        assert_eq!(mods.read("mods://main.lua").expect("read"), b"x");
        assert_eq!(mods.conflicts().len(), 1);
        assert_eq!(mods.conflicts()[0].winner, id);
    }
    let status = app.world().resource::<SteamWorkshop>().items()[&item].clone();
    assert!(status.subscribed);
    assert_eq!(status.mounted.as_deref(), Some(id.as_str()));
    assert_eq!(status.folder, Some(root.join(item.0.to_string())));

    // The author updates the item: it is installed again and remounted in the same place.
    let hash_before = app.world().resource::<ModsVfs>().content_set_hash();
    assert!(mock.update_item(item, files(&[("data/units.ron", "(hp: 300)")])));
    app.update();
    {
        let mods = app.world().resource::<ModsVfs>();
        assert_eq!(mods.order(), ["base".to_owned(), id.clone()]);
        assert_eq!(mods.read("mods://data/units.ron").expect("read"), b"(hp: 300)");
        assert!(!mods.exists("mods://textures/gun.txt"), "the update removed it");
        assert_ne!(mods.content_set_hash(), hash_before);
    }
    assert_eq!(app.world().resource::<SteamWorkshop>().items()[&item].installs, 2);

    // Unsubscribe: the item unmounts and the base shows through.
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .unsubscribe(item)
        .expect("unsubscribe");
    app.update();
    let mods = app.world().resource::<ModsVfs>();
    assert_eq!(mods.order(), ["base"]);
    assert_eq!(mods.read("mods://data/units.ron").expect("read"), b"(hp: 100)");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn an_upload_round_trips_through_the_mock_workshop() {
    let root = unique_temp_dir("workshop_upload_install");
    let content = unique_temp_dir("workshop_upload_content");
    std::fs::create_dir_all(content.join("maps")).expect("mkdir");
    std::fs::write(content.join("maps/arena.map"), "arena").expect("write");
    std::fs::write(content.join("luamod.ron"), "()").expect("write");
    let mock = MockWorkshop::shared(&root);
    let mut app = app_with(&mock);
    let mut upload = WorkshopUpload::new("Arena", &content);
    upload.tags = vec!["maps".into()];
    let request = app
        .world_mut()
        .resource_mut::<SteamWorkshop>()
        .upload(&upload)
        .expect("upload");
    app.update();
    let status = app.world().resource::<SteamWorkshop>().uploads()[&request].clone();
    let UploadStatus::Done { item, .. } = status else {
        panic!("upload not done: {status:?}");
    };
    let stored = mock.item(item).expect("on the mock Workshop");
    assert_eq!(
        stored.files.get("maps/arena.map").map(Vec::as_slice),
        Some(&b"arena"[..])
    );
    // The content hash rides in the metadata: the same hash a subscriber's mount computes.
    let mut local = ModsVfs::new();
    local.mount("x", ModSource::Dir(content.clone())).expect("mount");
    assert_eq!(
        stored.metadata,
        format!("regia.mods.content={}", local.mods()[0].content_hash.to_hex())
    );
    // Subscribing to one's own upload mounts it with that hash.
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .subscribe(item)
        .expect("subscribe");
    app.update();
    let mods = app.world().resource::<ModsVfs>();
    assert_eq!(mods.read("mods://maps/arena.map").expect("read"), b"arena");
    assert_eq!(mods.mods()[0].content_hash, local.mods()[0].content_hash);

    // A failing upload is reported, not fatal.
    mock.fail_next("k_EResultLimitExceeded");
    let request = app
        .world_mut()
        .resource_mut::<SteamWorkshop>()
        .upload(&upload)
        .expect("sent");
    app.update();
    assert!(matches!(
        app.world().resource::<SteamWorkshop>().uploads()[&request],
        UploadStatus::Failed(_)
    ));
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(content);
}

#[cfg(unix)]
#[test]
fn an_upload_with_a_link_in_its_folder_is_refused_before_sending() {
    let content = unique_temp_dir("workshop_upload_link");
    std::fs::write(content.join("ok.txt"), "ok").expect("write");
    std::os::unix::fs::symlink("/etc/hostname", content.join("leak.txt")).expect("symlink");
    let mock = MockWorkshop::shared(unique_temp_dir("workshop_link_install"));
    let mut app = app_with(&mock);
    let err = app
        .world_mut()
        .resource_mut::<SteamWorkshop>()
        .upload(&WorkshopUpload::new("Leak", &content))
        .expect_err("refused");
    assert_eq!(err.code(), "workshop_content");
    assert!(mock.uploads().is_empty(), "nothing was sent");
    let _ = std::fs::remove_dir_all(content);
}

#[test]
fn a_failed_install_keeps_the_previous_files_and_is_retried() {
    let root = unique_temp_dir("workshop-retry");
    let mock = MockWorkshop::shared(&root);
    let item = mock.publish("A", files(&[("a.txt", "v1")]));
    let mut app = app_with(&mock);
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .subscribe(item)
        .expect("subscribe");
    app.update();
    assert_eq!(
        std::fs::read(mock.install_info(item).expect("installed").folder.join("a.txt")).expect("file"),
        b"v1"
    );

    // An update whose install fails transiently: the previous files stay readable and the item
    // is not stranded — the next poll installs it again.
    assert!(mock.update_item(item, files(&[("a.txt", "v2")])));
    mock.fail_installs(1);
    app.update();
    let info = mock.install_info(item).expect("still installed");
    assert_eq!(
        std::fs::read(info.folder.join("a.txt")).expect("previous file readable"),
        b"v1",
        "a failed replace must not destroy the live install"
    );
    let status = app.world().resource::<SteamWorkshop>().items()[&item].clone();
    assert!(status.subscribed, "the item is still subscribed");
    assert!(status.error.is_some(), "the failure surfaced on the item");

    app.update();
    assert_eq!(
        std::fs::read(mock.install_info(item).expect("installed").folder.join("a.txt")).expect("file"),
        b"v2",
        "the retry installed the update"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn items_installed_before_the_session_mount_on_the_first_frame() {
    let root = unique_temp_dir("workshop_adopt");
    let mock = MockWorkshop::shared(&root);
    let item = mock.publish("Old", files(&[("a.txt", "a")]));
    mock.subscribe(item).expect("subscribe");
    let _ = mock.poll(); // installed by an earlier session
    let app = app_with(&mock);
    let mods = app.world().resource::<ModsVfs>();
    assert_eq!(mods.order(), [workshop_mod_id(item)]);
    assert_eq!(mods.read("mods://a.txt").expect("read"), b"a");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_quiet_workshop_frame_does_not_mark_the_resource_changed() {
    #[derive(Resource, Default)]
    struct Seen(bool);

    fn watch(workshop: Res<SteamWorkshop>, mut seen: ResMut<Seen>) {
        seen.0 = workshop.is_changed();
    }

    let root = unique_temp_dir("workshop_quiet");
    let mock = MockWorkshop::shared(&root);
    let mut app = app_with(&mock);
    app.insert_resource(Seen(false));
    app.add_systems(Update, watch);
    // The watcher's first run sees the resource as changed because it has never run.
    app.update();
    app.world_mut().resource_mut::<Seen>().0 = false;
    app.update();
    assert!(
        !app.world().resource::<Seen>().0,
        "a poll with nothing to mount must not remove and reinsert SteamWorkshop"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_lobby_key_follows_the_mounted_set() {
    use regia_net::lobby::{Lobbies, LocalLobbyHub};
    let root = unique_temp_dir("workshop_lobby");
    let mock = MockWorkshop::shared(&root);
    let item = mock.publish("Pack", files(&[("p.txt", "p")]));
    let hub = LocalLobbyHub::new();
    let mut app = app_with(&mock);
    let mut lobbies = Lobbies::new(Arc::new(hub.member(1, "host")));
    // No NetSession here: opt in to an empty identity (ADR-7881).
    lobbies.allow_unidentified(true);
    let spec = lobbies.default_spec();
    lobbies.create(spec).expect("create");
    lobbies.pump();
    assert!(lobbies.is_owner());
    app.insert_resource(lobbies);
    app.update();
    let key = |hub: &LocalLobbyHub| hub.lobbies()[0].data.get(MODS_LOBBY_KEY).cloned();
    let empty = ModsVfs::new().content_set_hash().to_hex();
    assert_eq!(key(&hub), Some(empty.clone()));
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .subscribe(item)
        .expect("subscribe");
    app.update();
    let mounted = app.world().resource::<ModsVfs>().content_set_hash().to_hex();
    assert_ne!(mounted, empty);
    assert_eq!(key(&hub), Some(mounted));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_agent_methods_answer_with_their_codes() {
    let root = unique_temp_dir("workshop_agent");
    let content = unique_temp_dir("workshop_agent_content");
    std::fs::write(content.join("readme.txt"), "hi").expect("write");
    let mock = MockWorkshop::shared(&root);
    let item = mock.publish("Theirs", files(&[("t.txt", "t")]));
    let mut app = app_with(&mock);

    let status = call(&mut app, "steam.workshop_status", json!({})).expect("status");
    assert_eq!(status["plugin"], json!(true));
    assert_eq!(status["service"], json!("mock"));
    assert_eq!(status["auto_mount"], json!(true));

    let sub = call(
        &mut app,
        "steam.workshop_subscribe",
        json!({ "item": item.0.to_string() }),
    )
    .expect("subscribe");
    assert_eq!(sub["mod_id"], json!(workshop_mod_id(item)));
    app.update();
    let status = call(&mut app, "steam.workshop_status", json!({})).expect("status");
    assert_eq!(status["items"][0]["mounted"], json!(true), "{status}");
    assert_eq!(status["items"][0]["installed"], json!(true), "{status}");

    let up = call(
        &mut app,
        "steam.workshop_upload",
        json!({ "title": "Mine", "content_dir": content.display().to_string(), "visibility": "unlisted", "tags": ["a"] }),
    )
    .expect("upload");
    let request = up["request"].clone();
    app.update();
    let status = call(&mut app, "steam.workshop_status", json!({})).expect("status");
    let row = status["uploads"]
        .as_array()
        .and_then(|u| u.iter().find(|r| r["request"] == request))
        .cloned()
        .expect("upload row");
    assert_eq!(row["state"], json!("done"), "{row}");

    for (method, params, expected) in [
        (
            "steam.workshop_upload",
            json!({ "title": "", "content_dir": content.display().to_string() }),
            "workshop_invalid",
        ),
        (
            "steam.workshop_upload",
            json!({ "title": "x", "content_dir": content.display().to_string(), "visibility": "loud" }),
            "F0002",
        ),
        ("steam.workshop_upload", json!({ "content_dir": "x" }), "F0002"),
        ("steam.workshop_subscribe", json!({ "item": "abc" }), "F0002"),
        ("steam.workshop_subscribe", json!({}), "F0002"),
        (
            "steam.workshop_unsubscribe",
            json!({ "item": 1 }),
            "workshop_unknown_item",
        ),
    ] {
        let err = call(&mut app, method, params.clone()).expect_err("refused");
        assert_eq!(code(&err), expected, "{method} {params}");
    }
    // A subscribe to an item the Workshop does not have fails on a later frame, not at once.
    call(&mut app, "steam.workshop_subscribe", json!({ "item": 7 })).expect("queued");
    app.update();
    let status = call(&mut app, "steam.workshop_status", json!({})).expect("status");
    let unknown = status["items"]
        .as_array()
        .and_then(|i| i.iter().find(|r| r["item"] == json!("7")))
        .cloned()
        .expect("row for 7");
    assert!(
        unknown["error"].as_str().is_some_and(|e| e.contains('7')),
        "{unknown}"
    );

    // Without the plugin every method reports plugin: false.
    let mut bare = App::new();
    regia_steam::workshop::SteamWorkshopPlugin::default().build(&mut bare);
    bare.world_mut().remove_resource::<SteamWorkshop>();
    let out = call(&mut bare, "steam.workshop_status", json!({})).expect("answers");
    assert_eq!(out["plugin"], json!(false));
    let _ = WorkshopItemId(0);
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(content);
}

#[test]
fn the_real_workshop_refuses_agent_uploads_unless_the_game_opts_in() {
    // No Steam client in tests: the unattached plugin has no service, and the agent upload gate
    // is checked before the service.
    let mut app = App::new();
    app.insert_resource(AppLifecycle::default());
    app.add_plugins(SteamPlugin::new(AppId(480)).with_backend(MockSteam::shared() as Arc<dyn SteamBackend>));
    app.add_plugins(SteamWorkshopPlugin::default());
    app.finish_plugins();
    app.update();
    let content = unique_temp_dir("workshop_gate");
    std::fs::write(content.join("a.txt"), "a").expect("write");
    let err = call(
        &mut app,
        "steam.workshop_upload",
        json!({ "title": "x", "content_dir": content.display().to_string() }),
    )
    .expect_err("gated");
    assert_eq!(code(&err), "workshop_upload_not_allowed");
    let workshop = app.world().resource::<SteamWorkshop>();
    assert_eq!(workshop.service_name(), "unavailable");
    let _ = std::fs::remove_dir_all(content);
}

/// Review findings (ADR-2418): the same subscriptions give the same load order whatever order the
/// installs land in, and a failed remount of an update leaves no stale mount behind.
#[test]
fn workshop_mounts_are_ordered_by_item_and_a_failed_update_unmounts() {
    use regia_asset::mods_vfs::ModsLimits;
    let root = unique_temp_dir("workshop_order");
    let mock = MockWorkshop::shared(&root);
    let first = mock.publish("A", files(&[("a.txt", "a")]));
    let second = mock.publish("B", files(&[("b.txt", "b")]));
    let mut app = App::new();
    app.insert_resource(AppLifecycle::default());
    app.insert_resource(ModsVfs::with_limits(ModsLimits {
        max_files_per_mod: 1,
        ..ModsLimits::default()
    }));
    app.add_plugins(SteamPlugin::new(AppId(480)).with_backend(MockSteam::shared() as Arc<dyn SteamBackend>));
    app.add_plugins(SteamWorkshopPlugin::with_service(
        Arc::clone(&mock) as Arc<dyn WorkshopService>
    ));
    app.finish_plugins();
    app.update();
    // The larger id installs first, the smaller one second: the order is still by id.
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .subscribe(second)
        .expect("subscribe");
    app.update();
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .subscribe(first)
        .expect("subscribe");
    app.update();
    assert_eq!(
        app.world().resource::<ModsVfs>().order(),
        [workshop_mod_id(first), workshop_mod_id(second)]
    );
    // An update the mount refuses (two files, the limit is one): the old mount goes too.
    assert!(mock.update_item(first, files(&[("a.txt", "a2"), ("c.txt", "c")])));
    app.update();
    assert_eq!(
        app.world().resource::<ModsVfs>().order(),
        [workshop_mod_id(second)]
    );
    let status = app.world().resource::<SteamWorkshop>().items()[&first].clone();
    assert_eq!(status.mounted, None);
    assert!(status.error.is_some_and(|e| e.contains("mods_too_large")));
    // Unsubscribing it now changes nothing; unsubscribing the other unmounts it.
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .unsubscribe(second)
        .expect("unsubscribe");
    app.update();
    assert!(app.world().resource::<ModsVfs>().is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn upload_paths_are_confined_and_mock_items_stay_in_their_folder() {
    let root = unique_temp_dir("workshop_confine");
    let mock = MockWorkshop::shared(&root);
    let mut app = app_with(&mock);
    let err = call(
        &mut app,
        "steam.workshop_upload",
        json!({ "title": "x", "content_dir": "/etc" }),
    )
    .expect_err("outside the roots");
    assert_eq!(code(&err), "F0002");
    assert_eq!(
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str),
        Some("path_outside_project")
    );
    assert!(mock.uploads().is_empty());
    // A mock item's paths that leave its folder are dropped, never written.
    let item = mock.publish("Evil", files(&[("../escape.txt", "x"), ("ok.txt", "ok")]));
    assert_eq!(mock.item(item).map(|i| i.files.len()), Some(1));
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .subscribe(item)
        .expect("subscribe");
    app.update();
    assert!(!root.join("escape.txt").exists());
    assert_eq!(
        app.world()
            .resource::<ModsVfs>()
            .read("mods://ok.txt")
            .expect("read"),
        b"ok"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// A relative content folder a game built from its working directory must reach the service as an
/// absolute path: the real ISteamUGC takes `SetItemContent`/`SetItemPreview` verbatim, so a
/// relative path would fail inside Steam as an opaque platform error (audit G, finding 1).
#[test]
fn relative_content_dir_is_made_absolute_before_upload() {
    // The working directory is process state: serialize against any other test that changes it.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    let root = unique_temp_dir("workshop_abs_install");
    let mock = MockWorkshop::shared(&root);

    // `check()` alone collapses a redundant absolute path to its canonical form.
    let content = unique_temp_dir("workshop_abs_content");
    std::fs::write(content.join("m.txt"), "m").expect("write");
    let mut upload = WorkshopUpload::new("Abs", content.join(".").join("m.txt").parent().expect("parent"));
    upload.check().expect("check");
    assert_eq!(
        upload.content_dir,
        std::fs::canonicalize(&content).expect("canonical")
    );

    // Against a scratch working directory, a relative content folder (and preview) comes out
    // absolute after the check and lands absolute on the service.
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    struct RestoreDir(std::path::PathBuf);
    impl Drop for RestoreDir {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }
    let cwd = std::env::current_dir().expect("cwd");
    let scratch = unique_temp_dir("workshop_abs_cwd");
    std::env::set_current_dir(&scratch).expect("chdir");
    let _restore = RestoreDir(cwd);
    std::fs::create_dir_all("rel_pack/maps").expect("mkdir");
    std::fs::write("rel_pack/maps/arena.map", "arena").expect("write");
    std::fs::write("preview.png", "png").expect("write");

    let mut upload = WorkshopUpload::new("Rel", "rel_pack");
    upload.preview = Some("preview.png".into());
    upload.check().expect("check");
    assert!(upload.content_dir.is_absolute(), "content_dir {upload:?}");
    assert_eq!(
        upload.content_dir,
        std::fs::canonicalize(scratch.join("rel_pack")).expect("canonical")
    );
    assert!(
        upload.preview.as_ref().is_some_and(|p| p.is_absolute()),
        "preview {upload:?}"
    );

    let mut app = app_with(&mock);
    let request = app
        .world_mut()
        .resource_mut::<SteamWorkshop>()
        .upload(&upload)
        .expect("upload");
    app.update();
    assert!(matches!(
        app.world().resource::<SteamWorkshop>().uploads()[&request],
        UploadStatus::Done { .. }
    ));
    let sent = mock.uploads().pop().expect("one upload");
    assert!(sent.content_dir.is_absolute(), "the service saw {sent:?}");
    assert!(
        sent.preview.as_ref().is_some_and(|p| p.is_absolute()),
        "the service saw {sent:?}"
    );

    // A relative folder that does not exist still fails the check.
    let mut missing = WorkshopUpload::new("Gone", "no_such_pack");
    assert_eq!(missing.check().expect_err("missing").code(), "workshop_invalid");
    assert!(mock.uploads().len() == 1, "the failed check sent nothing");

    // The entry point a game that never calls `check()` itself uses (review of
    // fix-steam-platform, finding 5): a relative upload sent straight through
    // `SteamWorkshop::upload` must still reach the service absolute — `upload` sends the
    // checked copy.
    std::fs::create_dir_all("rel_pack2").expect("mkdir");
    std::fs::write("rel_pack2/m.txt", "m2").expect("write");
    let direct = WorkshopUpload::new("Rel2", "rel_pack2");
    let request2 = app
        .world_mut()
        .resource_mut::<SteamWorkshop>()
        .upload(&direct)
        .expect("upload");
    app.update();
    assert!(matches!(
        app.world().resource::<SteamWorkshop>().uploads()[&request2],
        UploadStatus::Done { .. }
    ));
    let sent2 = mock.uploads().pop().expect("one upload");
    assert_eq!(
        sent2.content_dir,
        std::fs::canonicalize(scratch.join("rel_pack2")).expect("canonical")
    );
    assert_eq!(mock.uploads().len(), 2, "exactly the two uploads went out");
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(content);
    let _ = std::fs::remove_dir_all(scratch);
}

/// An update whose install folder is still open (a reader holds a file inside it) must still
/// install completely: the old content is renamed aside, not deleted — Windows refuses to delete
/// open files — and no reader sees a missing folder between the remove and the rename
/// (audit G, finding 2).
#[test]
fn an_update_installs_while_a_file_inside_is_open() {
    let root = unique_temp_dir("workshop_open_file");
    let mock = MockWorkshop::shared(&root);
    let item = mock.publish("Pack", files(&[("a.txt", "v1")]));
    let mut app = app_with(&mock);
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .subscribe(item)
        .expect("subscribe");
    app.update();
    let installed = root.join(item.0.to_string());
    assert_eq!(std::fs::read(installed.join("a.txt")).expect("read"), b"v1");

    // A reader keeps the installed file open across the update (fails the old
    // remove-dir-then-rename on Windows; exercises the swap path everywhere).
    let _held = std::fs::File::open(installed.join("a.txt")).expect("open");
    assert!(mock.update_item(item, files(&[("a.txt", "v2"), ("b.txt", "new")])));
    app.update();
    let status = app.world().resource::<SteamWorkshop>().items()[&item].clone();
    assert!(status.error.is_none(), "{status:?}");
    assert_eq!(status.installs, 2);
    assert_eq!(std::fs::read(installed.join("a.txt")).expect("read"), b"v2");
    assert_eq!(std::fs::read(installed.join("b.txt")).expect("read"), b"new");
    // No staging or trash residue is left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .expect("read root")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".staging-") || n.starts_with(".trash-"))
        .collect();
    assert_eq!(leftovers, Vec::<String>::new(), "{leftovers:?}");
    drop(_held);
    let _ = std::fs::remove_dir_all(root);
}

/// A stale `.trash-<id>-*` folder — an earlier update whose post-swap trash removal failed
/// while a reader held the old install open (the Windows case) — is swept by the next install
/// for that item, so such updates do not orphan one trash folder each (review of
/// fix-steam-platform, finding 4).
#[test]
fn a_stale_trash_folder_is_swept_by_the_next_install() {
    let root = unique_temp_dir("workshop_trash_sweep");
    let mock = MockWorkshop::shared(&root);
    let item = mock.publish("Pack", files(&[("a.txt", "v1")]));
    let mut app = app_with(&mock);
    app.world_mut()
        .resource_mut::<SteamWorkshop>()
        .subscribe(item)
        .expect("subscribe");
    app.update();
    // An orphan from an earlier update: only the next install sweeps it.
    let stale = root.join(format!(".trash-{}-0", item.0));
    std::fs::create_dir_all(stale.join("old")).expect("mkdir stale");
    std::fs::write(stale.join("old/a.txt"), "v0").expect("write stale");
    assert!(mock.update_item(item, files(&[("a.txt", "v2")])));
    app.update();
    let status = app.world().resource::<SteamWorkshop>().items()[&item].clone();
    assert!(status.error.is_none(), "{status:?}");
    assert_eq!(
        std::fs::read(root.join(item.0.to_string()).join("a.txt")).expect("read"),
        b"v2"
    );
    assert!(!stale.exists(), "the next install sweeps the stale trash");
    let _ = std::fs::remove_dir_all(root);
}
