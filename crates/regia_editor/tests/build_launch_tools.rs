// SPDX-License-Identifier: MPL-2.0
//! Build/launch parity through the real dispatcher, without a device, window or GPU.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use serde_json::{Value, json};

struct Client {
    app: App,
    inbox: Sender<Inbound>,
}

impl Client {
    fn new(root: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.to_owned()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.to_owned()),
        });
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded();
        app.insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        permissions.config.permissions.insert("build".into());
        app.insert_resource(permissions);
        Self { app, inbox }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let (reply, answer) = crossbeam_channel::bounded(1);
        self.inbox
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("inbox");
        self.app.update();
        let response = answer.try_recv().expect("answered in one frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            _ => panic!("empty response"),
        }
    }
}

#[test]
fn launch_profiles_round_trip_through_dispatcher_and_project_file() {
    let root = regia_test_utils::unique_temp_dir("launch-profiles-agent");
    let mut client = Client::new(&root);
    let profile = json!({
        "name": "Desktop review", "target": "standalone", "build_profile": "dev",
        "cook": "incremental", "package": false, "device": null,
        "start_level": "levels/review.level.ron", "extra_arguments": ["--review", "two words"]
    });
    let saved = client.call("editor.launch_profiles", json!({"op":"save", "profile":profile}));
    assert!(saved.is_ok(), "save a named launch profile: {saved:?}");
    let mut reopened = Client::new(&root);
    let listed = reopened
        .call("editor.launch_profiles", json!({"op":"list"}))
        .expect("list");
    assert_eq!(listed["format_version"], 1);
    assert_eq!(listed["profiles"], json!([profile]));
    reopened
        .call(
            "editor.launch_profiles",
            json!({"op":"delete", "name":"Desktop review"}),
        )
        .expect("delete");
    assert_eq!(
        reopened.call("editor.launch_profiles", json!({})).expect("list")["profiles"],
        json!([])
    );
    drop((client, reopened));
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn platforms_panel_has_an_idle_status_without_running_tools() {
    let root = regia_test_utils::unique_temp_dir("platforms-agent");
    let mut client = Client::new(&root);
    let answer = client.call("editor.platform_check", json!({"op":"status"}));
    assert!(answer.is_ok(), "platform check route: {answer:?}");
    assert_eq!(answer.expect("status")["state"], "idle");
    drop(client);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn size_report_panel_has_an_idle_status_without_a_package() {
    let root = regia_test_utils::unique_temp_dir("size-report-agent");
    let mut client = Client::new(&root);
    let answer = client.call("editor.package_size_report", json!({"op":"status"}));
    assert!(answer.is_ok(), "package size route: {answer:?}");
    assert_eq!(answer.expect("status")["state"], "idle");
    drop(client);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(unix)]
fn fake_cli(root: &std::path::Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = root.join("fake-regia");
    regia_test_utils::write_atomic(&path, format!("#!/bin/sh\n{body}\n").as_bytes()).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("permissions");
    path
}

#[test]
#[cfg(unix)]
fn platforms_recheck_dispatches_a_job_preserves_failures_and_replaces_stale_results() {
    let root = regia_test_utils::unique_temp_dir("platform-check-job");
    let cli = fake_cli(
        &root,
        r#"printf '%s\n' '{"schema":1,"kind":"regia_doctor","host":"linux","ok":false,"checks":[{"section":"android","name":"sdk","status":"fail","code":"android_sdk_missing","detail":"SDK missing","fix":"Install the Android SDK"}]}'
exit 1"#,
    );
    let mut client = Client::new(&root);
    client
        .app
        .insert_resource(regia_agent::cook_package::RegiaCliProgram(cli));
    let started = client
        .call(
            "editor.platform_check",
            json!({"op":"recheck","platform":"android"}),
        )
        .expect("start");
    assert_eq!(started["state"], "running");
    client
        .app
        .world_mut()
        .resource_mut::<regia_editor::platforms_panel::PlatformsPanel>()
        .wait();
    let done = client
        .call("editor.platform_check", json!({"op":"status"}))
        .expect("status");
    assert_eq!(done["state"], "done");
    assert_eq!(done["reports"]["android"]["ok"], false);
    assert_eq!(
        done["reports"]["android"]["checks"][0]["fix"],
        "Install the Android SDK"
    );
    assert!(regia_editor::platforms_panel::target_failed(
        client.app.world(),
        regia_editor::launch::LaunchTarget::Android
    ));
    assert!(!regia_editor::platforms_panel::target_failed(
        client.app.world(),
        regia_editor::launch::LaunchTarget::Standalone
    ));
    let job = client
        .call("agent.job_status", json!({"job":started["job"]}))
        .expect("registry job");
    assert_eq!(job["state"], "done");
    fake_cli(
        &root,
        r#"printf '%s\n' '{"schema":1,"kind":"regia_doctor","host":"linux","ok":true,"checks":[{"section":"android","name":"sdk","status":"ok","code":"ok","detail":"SDK ready","fix":null}]}'"#,
    );
    client
        .call(
            "editor.platform_check",
            json!({"op":"recheck","platform":"android"}),
        )
        .expect("recheck");
    client
        .app
        .world_mut()
        .resource_mut::<regia_editor::platforms_panel::PlatformsPanel>()
        .wait();
    let done = client.call("editor.platform_check", json!({})).expect("status");
    assert_eq!(done["reports"]["android"]["ok"], true);
    assert!(!regia_editor::platforms_panel::target_failed(
        client.app.world(),
        regia_editor::launch::LaunchTarget::Android
    ));
    drop(client);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn desktop_size_report_dispatches_and_never_claims_unobserved_features_are_unused() {
    let root = regia_test_utils::unique_temp_dir("size-report-job");
    let bundle = root.join("bundle");
    std::fs::create_dir_all(bundle.join("assets")).expect("assets");
    regia_test_utils::write_atomic(&bundle.join("game"), b"\x7fELF12").expect("binary");
    regia_test_utils::write_atomic(&bundle.join("assets/music.ogg"), b"123").expect("audio");
    let mut client = Client::new(&root);
    let started = client
        .call(
            "editor.package_size_report",
            json!({"op":"refresh","path":"bundle"}),
        )
        .expect("start");
    assert_eq!(started["state"], "running");
    client
        .app
        .world_mut()
        .resource_mut::<regia_editor::size_report_panel::SizeReportPanel>()
        .wait();
    let done = client
        .call("editor.package_size_report", json!({}))
        .expect("status");
    assert_eq!(done["state"], "done");
    assert_eq!(done["report"]["content_bytes"], 9);
    assert_eq!(done["report"]["categories"]["engine_binary"], 6);
    assert_eq!(done["report"]["categories"]["audio"], 3);
    assert_eq!(done["unused_features"], json!([]));
    assert_eq!(done["feature_telemetry"], "unavailable");
    drop(client);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn launch_profiles_reject_future_data_without_rewriting_it() {
    let root = regia_test_utils::unique_temp_dir("launch-profiles-future");
    std::fs::create_dir_all(root.join("config")).expect("config");
    let path = root.join("config/launch_profiles.ron");
    let future = "(format_version:2,profiles:[])";
    regia_test_utils::write_atomic(&path, future.as_bytes()).expect("future file");
    let mut client = Client::new(&root);
    let error = client
        .call(
            "editor.launch_profiles",
            json!({"op":"save","profile":regia_editor::launch_profiles::LaunchProfile::default()}),
        )
        .expect_err("refuse future version");
    assert_eq!(error.data.expect("data")["code"], "launch_profiles_invalid");
    assert_eq!(std::fs::read_to_string(path).expect("unchanged"), future);
    drop(client);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn package_size_archive_reuses_cli_contract_and_accounts_for_overhead() {
    let root = regia_test_utils::unique_temp_dir("size-report-archive");
    let archive = root.join("package.apk");
    std::fs::write(&archive, b"placeholder").expect("archive path");
    let cli = fake_cli(
        &root,
        r#"test "$1" = apk-size || exit 2
printf '%s\n' '{"file_bytes":100,"entries":3,"largest":[{"name":"lib/arm64-v8a/libgame.so","compressed":40},{"name":"assets/music.ogg","compressed":20},{"name":"assets/core.fpak","compressed":10}],"warnings":[]}'"#,
    );
    let mut client = Client::new(&root);
    client
        .app
        .insert_resource(regia_agent::cook_package::RegiaCliProgram(cli));
    client
        .call(
            "editor.package_size_report",
            json!({"op":"refresh","path":archive}),
        )
        .expect("start");
    client
        .app
        .world_mut()
        .resource_mut::<regia_editor::size_report_panel::SizeReportPanel>()
        .wait();
    let result = client
        .call("editor.package_size_report", json!({}))
        .expect("report");
    assert_eq!(result["state"], "done");
    assert_eq!(result["report"]["content_bytes"], 100);
    assert_eq!(
        result["report"]["categories"],
        json!({"engine_binary":40,"audio":20,"paks":10,"archive_overhead":30})
    );
    drop(client);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn desktop_size_separates_pak_payload_kinds_from_container_overhead() {
    use regia_asset::{AssetId, AssetManifest, ManifestEntry};
    use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
    use regia_core::id::{Guid, TypePathHash};
    let root = regia_test_utils::unique_temp_dir("size-report-pak");
    let cache = root.join("cache");
    let bundle = root.join("bundle");
    std::fs::create_dir_all(&cache).expect("cache");
    std::fs::create_dir_all(&bundle).expect("bundle");
    let audio = AssetId(Guid::from_u128(1));
    let texture = AssetId(Guid::from_u128(2));
    let mut manifest = AssetManifest::default();
    let mut entries = Vec::new();
    for (id, path, ty, bytes) in [
        (audio, "assets/music.ogg", "AudioClip", vec![7; 1024]),
        (texture, "assets/paint.png", "CookedTexture", vec![9; 2048]),
    ] {
        std::fs::write(cache.join(regia_asset::cooked_file_name(id)), bytes).expect("payload");
        manifest.entries.push(ManifestEntry {
            path: path.into(),
            id,
            type_path: ty.into(),
        });
        entries.push((id, TypePathHash::of(ty)));
    }
    let pak = bundle.join("base.fpak");
    write_pak(
        &pak,
        &cache,
        &entries,
        &manifest,
        &PakWriteOptions {
            compress: false,
            ..Default::default()
        },
    )
    .expect("pak");
    let report = regia_editor::size_report_panel::directory_report(&bundle, || false).expect("report");
    let total = std::fs::metadata(pak).expect("metadata").len();
    assert_eq!(report.content_bytes, total);
    assert_eq!(report.categories["audio"], 1024);
    assert_eq!(report.categories["textures"], 2048);
    assert_eq!(report.categories["pak_overhead"], total - 3072);
    assert_eq!(report.categories.values().sum::<u64>(), total);
    assert_eq!(report.largest_assets.len(), 2);
    assert_eq!(report.largest_assets[0].path, "base.fpak::assets/paint.png");
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn diagnostic_files_refuse_symlinks_outside_the_project() {
    let root = regia_test_utils::unique_temp_dir("diagnostic-paths");
    let project = root.join("project");
    std::fs::create_dir_all(project.join("config")).expect("config");
    let external = root.join("external.ron");
    std::fs::write(&external, "(format_version:1,profiles:[])").expect("external");
    std::os::unix::fs::symlink(&external, project.join("config/launch_profiles.ron")).expect("profile link");
    assert!(regia_editor::launch_profiles::read(&project).is_err());
    assert!(regia_editor::size_report_panel::directory_report(&project, || false).is_err());
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn successful_editor_package_opens_and_starts_a_size_report_automatically() {
    let root = regia_test_utils::unique_temp_dir("automatic-package-size");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"Sizes\"\nid = \"dev.regia.sizes\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    let cli = fake_cli(
        &root,
        r#"out=''
while [ "$#" -gt 0 ]; do
  if [ "$1" = --out ]; then shift; out="$1"; fi
  shift
done
test -n "$out" || exit 2
mkdir -p "$out/game/assets"
printf '\177ELF12' > "$out/game/game"
printf '123' > "$out/game/assets/music.ogg"
printf '{}' > "$out/game/build_info.json"
printf 'tar' > "$out/game-linux.tar.gz"
printf '{"ok":true,"artifact":"%s/game-linux.tar.gz","dry_run":false}\n' "$out""#,
    );
    let mut client = Client::new(&root);
    client
        .app
        .insert_resource(regia_agent::cook_package::RegiaCliProgram(cli));
    client
        .call(
            "editor.package_start",
            json!({"target":"linux","profile":"dev","confirm":true}),
        )
        .expect("package");
    let outcome = client
        .app
        .world()
        .resource::<regia_editor::package_job::CliJobs>()
        .package
        .as_ref()
        .expect("package job")
        .wait()
        .expect("outcome");
    assert_eq!(outcome["ok"], true, "{outcome}");
    let started = client
        .call("editor.package_size_report", json!({}))
        .expect("auto report");
    assert_eq!(started["state"], "running");
    assert_eq!(started["open"], true);
    client
        .app
        .world_mut()
        .resource_mut::<regia_editor::size_report_panel::SizeReportPanel>()
        .wait();
    let done = client
        .call("editor.package_size_report", json!({}))
        .expect("done");
    assert_eq!(done["state"], "done");
    assert_eq!(done["report"]["content_bytes"], 11);
    assert_eq!(done["report"]["measurement"], "installed_contents");
    assert_eq!(done["report"]["path"], json!(root.join("dist/linux/game")));
    drop(client);
    std::fs::remove_dir_all(root).expect("cleanup");
}
