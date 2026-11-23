// SPDX-License-Identifier: MPL-2.0
//! Spec 57 § 8 tests 4-14 on the runtime side: discovery, the registry and `ExtensionContext`
//! registrations, link checks, content mounts, feature packs, live reload, out-of-process tools
//! and the `[extensions]` table. Fixed app steps and counters; no GPU, no wall-clock budgets.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_app::prelude::*;
use regia_ecs::ecs::change_detection::DetectChanges;
use regia_ecs::ecs::message::MessageCursor;
use regia_ecs::reflect::AppTypeRegistry;
use regia_framework::extension::discover::{self, MANIFEST_FILE};
use regia_framework::extension::prelude::*;
use regia_framework::extension::registry::{ContentMounts, DynamicRecord};
use regia_framework::extension::{
    BuildState, ExtensionPlugin, ExtensionRegistry, ExtensionState, ExtensionsConfig, LinkedExtensions,
    PackState, RegistrationKind, RegistryInputs, reload_in_world,
};
use regia_render::{FrameRenderer, RuntimeRenderer};
use regia_scene::components::{Name, Tags};
use regia_test_utils::unique_temp_dir;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

fn manifest(dir: &Path, id: &str, body: &str) {
    write(
        &dir.join(MANIFEST_FILE),
        &format!("format_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\n{body}"),
    );
}

const CODE: &str = "[[code]]\ncrate = \"x\"\naudience = [\"game\"]\n";

#[derive(Resource, Default)]
struct PluginRan(u32);

struct MarkPlugin;
impl Plugin for MarkPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(PluginRan(1));
    }
    fn name(&self) -> &str {
        "mark"
    }
}

#[derive(Resource, Reflect, Default, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
struct DialogueSettings {
    max_line_chars: u32,
    voice: bool,
}

#[derive(Component, Reflect, Default, Debug, PartialEq)]
#[reflect(Component, Default)]
struct Wet(u32);

struct DlgLoader;
impl AssetLoaderDesc for DlgLoader {
    fn file_extensions(&self) -> &[&str] {
        &["dlg"]
    }
    fn type_name(&self) -> &str {
        "fixture::Dialogue"
    }
    fn load(&self, bytes: &[u8], _path: &str) -> Result<Box<dyn Reflect>, String> {
        Ok(Box::new(u32::try_from(bytes.len()).map_err(|e| e.to_string())?))
    }
}

fn hello(name: &str, reply: &'static str) -> AgentMethodDesc {
    AgentMethodDesc {
        name: name.to_owned(),
        summary: "Says hello.".into(),
        access: MethodAccess::Read,
        params_schema: json!({ "type": "object" }),
        result_schema: json!({ "type": "object" }),
        handler: Arc::new(move |_world: &mut World, _params: Value| Ok(json!({ "reply": reply }))),
    }
}

/// A pass that records nothing.
struct NoopPass(&'static str);
impl FramePass for NoopPass {
    fn name(&self) -> &str {
        self.0
    }
    fn slot(&self) -> GraphSlot {
        GraphSlot::AfterOpaque
    }
    fn record(&mut self, _ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        Ok(())
    }
}

/// Registers one of each kind.
#[derive(Default)]
struct Basic;
impl RegiaExtension for Basic {
    fn id(&self) -> &str {
        "fixture_basic"
    }
    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        cx.add_plugin(MarkPlugin)?;
        cx.declare_settings::<DialogueSettings>()?;
        cx.register_agent_method(hello("fixture_basic.hello", "basic"))?;
        cx.register_asset_loader(Box::new(DlgLoader))?;
        cx.declare_feature_pack(FeaturePackDesc {
            id: "rain".into(),
            tag: "outdoor".into(),
            components: vec![ComponentSpec {
                type_path: Wet::get_type_registration().type_info().type_path().to_owned(),
            }],
            requires_level_feature: "rain_weather".into(),
        })?;
        Ok(())
    }
}

/// Tries to take the basic extension's file extension and namespace.
#[derive(Default)]
struct Rival;
impl RegiaExtension for Rival {
    fn id(&self) -> &str {
        "fixture_rival"
    }
    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        cx.register_agent_method(hello("fixture_rival.ping", "rival"))?;
        cx.register_asset_loader(Box::new(DlgLoader))
    }
}

/// Registers without requesting the capability.
#[derive(Default)]
struct Sneaky;
impl RegiaExtension for Sneaky {
    fn id(&self) -> &str {
        "fixture_sneaky"
    }
    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        cx.register_agent_method(hello("fixture_sneaky.x", "x"))
    }
}

struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(tag: &str, regia_toml: &str) -> Self {
        let dir = unique_temp_dir(tag);
        write(&dir.join("Regia.toml"), regia_toml);
        Self { dir }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn app_for(project: &Path, linked: Vec<Arc<dyn RegiaExtension>>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.world_mut()
        .resource_mut::<AppTypeRegistry>()
        .write()
        .register::<Wet>();
    app.add_plugins(ExtensionPlugin::for_project(project.to_path_buf(), linked));
    app
}

fn registry(app: &App) -> &ExtensionRegistry {
    app.world().resource::<ExtensionRegistry>()
}

const GRANTS: &str = r#"
[extensions]
enabled = ["fixture_basic", "fixture_rival"]

[extensions.grants]
fixture_basic = ["agent_method", "asset_loader", "feature_pack"]
fixture_rival = ["agent_method", "asset_loader"]

[extensions.settings.fixture_basic]
max_line_chars = 240
colour = "blue"
"#;

fn basic_project(tag: &str) -> Project {
    let p = Project::new(tag, GRANTS);
    let ext = p.dir.join("extensions");
    manifest(
        &ext.join("fixture_basic"),
        "fixture_basic",
        &format!(
            "capabilities = [\"systems\", \"settings\", \"agent_method\", \"asset_loader\", \"feature_pack\"]\n{CODE}"
        ),
    );
    manifest(
        &ext.join("fixture_rival"),
        "fixture_rival",
        &format!("capabilities = [\"agent_method\", \"asset_loader\"]\n{CODE}"),
    );
    p
}

#[test]
fn discovery_is_ordered_and_stops_at_manifests() {
    let p = Project::new("ext-discover", "");
    let ext = p.dir.join("extensions");
    manifest(&ext.join("b_two"), "b_two", "");
    manifest(&ext.join("a_one"), "a_one", "");
    // Never descended into: a manifest below a manifest directory.
    manifest(&ext.join("a_one/inner"), "inner", "");
    manifest(&ext.join(".hidden/h_one"), "h_one", "");
    manifest(&ext.join("group/c_three"), "c_three", "");
    write(
        &ext.join("broken/regia-plugin.toml"),
        "format_version = 99\nid = \"broken\"\n",
    );
    manifest(&ext.join("wrong_dir"), "named_else", "");
    let found = discover::discover(std::slice::from_ref(&ext));
    let ids: Vec<&str> = found.manifests.iter().map(|m| m.manifest.id.as_str()).collect();
    assert_eq!(ids, ["a_one", "b_two", "c_three"]);
    let codes: Vec<(&str, &str)> = found
        .diagnostics
        .iter()
        .map(|d| (d.id.as_str(), d.code))
        .collect();
    assert_eq!(
        codes,
        [("broken", "manifest_too_new"), ("wrong_dir", "manifest_invalid")]
    );
    // The hash is of the bytes: the same text hashes the same in another directory.
    let again = discover::read_extension_dir(&ext.join("b_two")).unwrap();
    assert_eq!(again.hash, found.manifests[1].hash);
}

#[test]
fn registration_of_every_kind_and_conflicts() {
    let p = basic_project("ext-register");
    let mut app = app_for(&p.dir, vec![Arc::new(Basic), Arc::new(Rival)]);
    app.update();
    let r = registry(&app);
    assert_eq!(r.state("fixture_basic"), Some(ExtensionState::Active));
    assert_eq!(r.build_state("fixture_basic"), BuildState::Built);
    // The rival lost the `dlg` loader: registration_conflict, failed, registrations removed.
    assert_eq!(r.state("fixture_rival"), Some(ExtensionState::Failed));
    assert!(r.codes_for("fixture_rival").contains(&"registration_conflict"));
    assert!(r.registrations_of("fixture_rival").is_empty());
    assert!(r.agent_method("fixture_rival.ping").is_none());
    let (rival, state) = r.unavailable_provider("fixture_rival.ping").unwrap();
    assert_eq!((rival, state), ("fixture_rival", ExtensionState::Failed));
    // The first registrant stays functional.
    assert_eq!(
        r.owner_of(RegistrationKind::AssetLoader, "dlg"),
        Some("fixture_basic")
    );
    let loaded = r.loader_for(".DLG").unwrap().load(b"abc", "a.dlg").unwrap();
    assert_eq!(loaded.downcast_ref::<u32>(), Some(&3));
    let (owner, method) = r.agent_method("fixture_basic.hello").unwrap();
    assert_eq!(owner, "fixture_basic");
    let handler = Arc::clone(&method.handler);
    let kinds: Vec<&str> = r
        .registrations_of("fixture_basic")
        .iter()
        .map(|x| x.kind.name())
        .collect();
    assert_eq!(
        kinds,
        [
            "plugin",
            "asset_loader",
            "agent_method",
            "settings",
            "feature_pack"
        ]
    );
    assert!(
        r.registrations_of("fixture_basic")
            .iter()
            .any(|x| x.name == "ext/fixture_basic/mark")
    );
    // plugin.info lists them and the implicit grants.
    let info = r.info_json("fixture_basic").unwrap();
    assert_eq!(
        info["registrations"]["agent_method"],
        json!(["fixture_basic.hello"])
    );
    assert_eq!(info["registrations"]["asset_loader"], json!(["dlg"]));
    assert_eq!(info["packs"][0]["id"], "rain");
    let grants = info["granted"].as_array().unwrap();
    assert!(grants.contains(&json!({ "capability": "systems", "grant": "implicit" })));
    assert!(grants.contains(&json!({ "capability": "agent_method", "grant": "explicit" })));
    // Settings came from [extensions.settings.fixture_basic]; the unknown key is a warning.
    let s = app.world().resource::<DialogueSettings>();
    assert_eq!(s.max_line_chars, 240);
    assert!(
        registry(&app)
            .codes_for("fixture_basic")
            .contains(&"unknown_field")
    );
    assert_eq!(app.world().resource::<PluginRan>().0, 1);
    let out = handler(app.world_mut(), json!({})).unwrap();
    assert_eq!(out, json!({ "reply": "basic" }));
    // Events reached the message queue.
    let mut cursor = MessageCursor::default();
    let events: Vec<ExtensionEvent> = cursor
        .read(app.world().resource::<Messages<ExtensionEvent>>())
        .cloned()
        .collect();
    assert!(events.contains(&ExtensionEvent::Built {
        id: "fixture_basic".into(),
        phase: "default".into()
    }));
    assert!(events.contains(&ExtensionEvent::Failed {
        id: "fixture_rival".into(),
        code: "registration_conflict".into()
    }));
    assert!(matches!(
        events.first(),
        Some(ExtensionEvent::Resolved { count: 2, errors: 0 })
    ));
}

#[test]
fn capability_checks_and_link_checks() {
    let p = Project::new(
        "ext-caps",
        "[extensions]\nenabled = [\"fixture_sneaky\", \"unlinked\", \"passes\"]\n[extensions.grants]\npasses = [\"render_pass\"]\n",
    );
    let ext = p.dir.join("extensions");
    manifest(&ext.join("fixture_sneaky"), "fixture_sneaky", CODE);
    manifest(&ext.join("unlinked"), "unlinked", CODE);
    manifest(
        &ext.join("passes"),
        "passes",
        &format!("capabilities = [\"render_pass\"]\n{CODE}"),
    );
    manifest(&ext.join("spare"), "spare", CODE);

    #[derive(Default)]
    struct Passes;
    impl RegiaExtension for Passes {
        fn id(&self) -> &str {
            "passes"
        }
        fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
            cx.add_render_pass(Box::new(NoopPass("outline")))
        }
    }
    #[derive(Default)]
    struct Spare;
    impl RegiaExtension for Spare {
        fn id(&self) -> &str {
            "spare"
        }
        fn build(&self, _cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
            Ok(())
        }
    }
    let app = app_for(&p.dir, vec![Arc::new(Sneaky), Arc::new(Passes), Arc::new(Spare)]);
    let r = registry(&app);
    assert_eq!(r.state("fixture_sneaky"), Some(ExtensionState::Failed));
    assert_eq!(r.codes_for("fixture_sneaky"), ["capability_not_granted"]);
    assert_eq!(r.state("unlinked"), Some(ExtensionState::Failed));
    assert_eq!(r.codes_for("unlinked"), ["manifest_not_linked"]);
    assert_eq!(r.codes_for("spare"), ["linked_not_enabled"]);
    assert_eq!(r.state("spare"), Some(ExtensionState::Available));
    assert_eq!(r.state("passes"), Some(ExtensionState::Active));
    let list = r.list_json(Some("failed"));
    let failed: Vec<&str> = list["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(failed, ["fixture_sneaky", "unlinked"]);
}

#[test]
fn content_mounts_shadow_and_collide() {
    let p = Project::new("ext-content", "[extensions]\nenabled = [\"art_a\", \"art_b\"]\n");
    let ext = p.dir.join("extensions");
    manifest(&ext.join("art_a"), "art_a", "[[content]]\nroot = \"content\"\n");
    manifest(&ext.join("art_b"), "art_b", "[[content]]\nroot = \"content\"\n");
    write(&ext.join("art_a/content/tex/rock.png"), "a-rock");
    write(&ext.join("art_a/content/tex/shared.png"), "a-shared");
    write(&ext.join("art_b/content/tex/shared.png"), "b-shared");
    write(&ext.join("art_b/content/tex/sand.png"), "b-sand");
    write(&p.dir.join("assets/tex/sand.png"), "project-sand");
    let app = app_for(&p.dir, Vec::new());
    let r = registry(&app);
    assert_eq!(r.state("art_a"), Some(ExtensionState::Active));
    assert_eq!(r.codes_for("art_b"), ["content_collision"]);
    let mounts: &ContentMounts = r.mounts();
    let assets = p.dir.join("assets");
    let hit = mounts.resolve("tex/shared.png", Some(&assets)).unwrap();
    assert_eq!(hit.origin, "extension:art_a");
    assert_eq!(std::fs::read_to_string(&hit.path).unwrap(), "a-shared");
    // b lost the path even when asked by namespace.
    assert!(
        mounts
            .resolve("ext://art_b/tex/shared.png", Some(&assets))
            .is_none()
    );
    // The project's own file shadows the extension's.
    let sand = mounts.resolve("ext://art_b/tex/sand.png", Some(&assets)).unwrap();
    assert_eq!(sand.origin, "project");
    let rock = mounts.resolve("ext://art_a/tex/rock.png", None).unwrap();
    assert_eq!(rock.origin, "extension:art_a");
    // No escaping the mount.
    assert!(
        mounts
            .resolve("ext://art_a/../art_b/content/tex/sand.png", None)
            .is_none()
    );
    assert_eq!(mounts.list().len(), 2);
}

#[test]
fn feature_packs_follow_level_features_with_reference_counts() {
    let p = basic_project("ext-packs");
    let mut app = app_for(&p.dir, vec![Arc::new(Basic)]);
    // A pack's systems are gated by `pack_active(id)` (spec 57 § 3.3.8, ADR-5783).
    #[derive(Resource, Default)]
    struct RainTicks(u32);
    fn tick_rain(mut ticks: ResMut<RainTicks>) {
        ticks.0 += 1;
    }
    app.init_resource::<RainTicks>()
        .add_systems(Update, tick_rain.run_if(pack_active("rain")));
    let ticks = |app: &App| app.world().resource::<RainTicks>().0;
    let outdoor = app.world_mut().spawn(Tags::from_iter_str(["outdoor"])).id();
    let indoor = app.world_mut().spawn(Tags::from_iter_str(["indoor"])).id();
    let already = app
        .world_mut()
        .spawn((Tags::from_iter_str(["outdoor"]), Wet(7)))
        .id();
    app.update();
    let state = |app: &App| registry(app).packs().state("rain");
    assert_eq!(state(&app), Some(PackState::Declared));
    assert!(app.world().get::<Wet>(outdoor).is_none());
    assert_eq!(ticks(&app), 0, "a declared pack's systems do not run");

    fn reg(app: &mut App) -> Mut<'_, ExtensionRegistry> {
        app.world_mut().resource_mut::<ExtensionRegistry>()
    }
    reg(&mut app).enable_level_feature("rain_weather", "levels/a.level.ron");
    reg(&mut app).enable_level_feature("rain_weather", "levels/b.level.ron");
    app.update();
    assert_eq!(state(&app), Some(PackState::Active));
    assert_eq!(app.world().get::<Wet>(outdoor), Some(&Wet(0)));
    assert!(app.world().get::<Wet>(indoor).is_none());
    assert_eq!(app.world().get::<Wet>(already), Some(&Wet(7)));
    // An entity tagged later gets it at the next frame boundary.
    let late = app.world_mut().spawn(Tags::from_iter_str(["outdoor"])).id();
    app.update();
    assert_eq!(app.world().get::<Wet>(late), Some(&Wet(0)));
    // The activation is a message an agent can wait on (spec 57 § 3.5); messages live two
    // frames, so it is still readable one frame after the activation.
    let mut activated = MessageCursor::default();
    assert!(
        activated
            .read(app.world().resource::<Messages<ExtensionEvent>>())
            .any(|e| *e == ExtensionEvent::PackActivated { pack: "rain".into() })
    );
    assert_eq!(registry(&app).packs().applied_count("rain"), 3);

    // One level releases: still on.
    reg(&mut app).release_level("levels/a.level.ron");
    app.update();
    assert_eq!(state(&app), Some(PackState::Active));
    // The second releases: one withdrawing frame, components removed, pre-existing kept.
    reg(&mut app).disable_level_feature("rain_weather", "levels/b.level.ron");
    app.update();
    assert_eq!(state(&app), Some(PackState::Withdrawing));
    assert!(app.world().get::<Wet>(outdoor).is_none());
    assert!(app.world().get::<Wet>(late).is_none());
    assert_eq!(app.world().get::<Wet>(already), Some(&Wet(7)));
    app.update();
    assert_eq!(state(&app), Some(PackState::Declared));
    let ran = ticks(&app);
    assert!(ran > 0, "an active pack's systems ran");
    let mut cursor = MessageCursor::default();
    let events: Vec<ExtensionEvent> = cursor
        .read(app.world().resource::<Messages<ExtensionEvent>>())
        .cloned()
        .collect();
    assert!(events.contains(&ExtensionEvent::PackDeactivated { pack: "rain".into() }));
    app.update();
    app.update();
    assert_eq!(ticks(&app), ran, "back to declared: the systems stop");
}

/// An active pack that already touched every matching entity must not walk `Tags` again while
/// nothing was inserted, edited or removed. A later insert, in-place edit or despawn still applies
/// to that entity only, and components already applied keep their change tick.
#[test]
fn framework2_perf_pack_scan() {
    let p = basic_project("ext-pack-idle");
    let mut app = app_for(&p.dir, vec![Arc::new(Basic)]);
    let outdoor = app.world_mut().spawn(Tags::from_iter_str(["outdoor"])).id();
    let indoor = app.world_mut().spawn(Tags::from_iter_str(["indoor"])).id();
    let already = app
        .world_mut()
        .spawn((Tags::from_iter_str(["outdoor"]), Wet(7)))
        .id();
    app.world_mut()
        .resource_mut::<ExtensionRegistry>()
        .enable_level_feature("rain_weather", "levels/a.level.ron");
    app.update();
    assert_eq!(registry(&app).packs().state("rain"), Some(PackState::Active));
    assert_eq!(registry(&app).packs().applied_count("rain"), 2);
    assert!(registry(&app).packs().tag_scans() >= 1, "activation walks tags");
    let scans = registry(&app).packs().tag_scans();
    let wet_tick = |app: &App, entity: Entity| {
        app.world()
            .get_entity(entity)
            .expect("entity")
            .get_ref::<Wet>()
            .expect("wet")
            .last_changed()
    };
    let outdoor_tick = wet_tick(&app, outdoor);
    let already_tick = wet_tick(&app, already);

    app.update();
    assert_eq!(
        registry(&app).packs().tag_scans(),
        scans,
        "idle frame walked every Tags entity again"
    );
    assert_eq!(wet_tick(&app, outdoor), outdoor_tick, "idle frame rewrote Wet");
    assert_eq!(registry(&app).packs().applied_count("rain"), 2);

    let late = app.world_mut().spawn(Tags::from_iter_str(["outdoor"])).id();
    app.update();
    assert!(
        registry(&app).packs().tag_scans() > scans,
        "a new tagged entity must be seen"
    );
    assert_eq!(app.world().get::<Wet>(late), Some(&Wet(0)));
    assert_eq!(registry(&app).packs().applied_count("rain"), 3);
    assert_eq!(
        wet_tick(&app, outdoor),
        outdoor_tick,
        "applying to a new entity rewrote an old one"
    );

    {
        let mut tags = app.world_mut().get_mut::<Tags>(indoor).expect("tags");
        tags.0.push(Name::new("outdoor"));
    }
    let scans = registry(&app).packs().tag_scans();
    app.update();
    assert!(
        registry(&app).packs().tag_scans() > scans,
        "an in-place tag edit must be seen"
    );
    assert_eq!(app.world().get::<Wet>(indoor), Some(&Wet(0)));
    assert_eq!(wet_tick(&app, already), already_tick);

    app.world_mut().despawn(late);
    app.update();
    assert_eq!(registry(&app).packs().applied_count("rain"), 3);
    assert!(
        app.world().get::<Wet>(late).is_none(),
        "despawn drops the packed component"
    );
}

static BUILDS: AtomicU32 = AtomicU32::new(0);

#[derive(Resource, Default)]
struct Counter(u32);

/// A reloadable extension: generation-tagged method, state carried through on_unload/on_load.
struct Live {
    tag: &'static str,
    fail: bool,
}
impl RegiaExtension for Live {
    fn id(&self) -> &str {
        "live_ext"
    }
    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        BUILDS.fetch_add(1, Ordering::Relaxed);
        cx.register_agent_method(hello(&format!("live_ext.{}", self.tag), "live"))?;
        if self.fail {
            return Err(ExtensionError::BuildFailed("fixture failure".into()));
        }
        // Systems need a restart during a live reload.
        if cx.add_plugin(MarkPlugin).is_err() {
            return Ok(());
        }
        Ok(())
    }
    fn on_unload(&self, world: &mut World) -> Option<DynamicStateBlob> {
        let n = world.get_resource::<Counter>().map_or(0, |c| c.0);
        Some(DynamicStateBlob(n.to_le_bytes().to_vec()))
    }
    fn on_load(&self, world: &mut World, prior: Option<DynamicStateBlob>) {
        let n = prior
            .and_then(|b| b.0.try_into().ok())
            .map_or(0, u32::from_le_bytes);
        world.insert_resource(Counter(n + 1));
    }
}

#[test]
fn reload_swaps_registrations_and_restores_on_failure() {
    let p = Project::new(
        "ext-reload",
        "[extensions]\nenabled = [\"live_ext\"]\n[extensions.grants]\nlive_ext = [\"agent_method\"]\n",
    );
    manifest(
        &p.dir.join("extensions/live_ext"),
        "live_ext",
        &format!("capabilities = [\"agent_method\", \"systems\"]\n{CODE}"),
    );
    let first = Live {
        tag: "one",
        fail: false,
    };
    let mut app = app_for(
        &p.dir,
        vec![Arc::new(Live {
            tag: "one",
            fail: false,
        })],
    );
    app.world_mut().insert_resource(Counter(5));
    assert!(registry(&app).agent_method("live_ext.one").is_some());

    let second = Live {
        tag: "two",
        fail: false,
    };
    let report = reload_in_world(app.world_mut(), &first, &second).unwrap();
    assert_eq!(report.generation, 1);
    assert_eq!(report.removed.get("agent_method"), Some(&1));
    assert_eq!(report.added.get("agent_method"), Some(&1));
    assert!(registry(&app).agent_method("live_ext.one").is_none());
    assert!(registry(&app).agent_method("live_ext.two").is_some());
    assert_eq!(app.world().resource::<Counter>().0, 6);

    let third = Live {
        tag: "three",
        fail: false,
    };
    let report = reload_in_world(app.world_mut(), &second, &third).unwrap();
    assert_eq!(report.generation, 2);
    assert_eq!(app.world().resource::<Counter>().0, 7);

    let broken = Live {
        tag: "four",
        fail: true,
    };
    let err = reload_in_world(app.world_mut(), &third, &broken).unwrap_err();
    assert_eq!(err.0, "reload_failed");
    let r = registry(&app);
    assert!(
        r.agent_method("live_ext.three").is_some(),
        "previous registrations restored"
    );
    assert!(r.agent_method("live_ext.four").is_none());
    assert_eq!(r.generation("live_ext"), 2);
    // The old copy keeps its state.
    assert_eq!(app.world().resource::<Counter>().0, 8);
    assert!(BUILDS.load(Ordering::Relaxed) >= 4);

    app.world_mut()
        .resource_mut::<ExtensionRegistry>()
        .record_dynamic(
            "live_ext",
            DynamicRecord {
                library: "/tmp/liblive_ext-3.so".into(),
                stamp: vec![("abi_epoch", "1".into())],
            },
        );
    let info = registry(&app).info_json("live_ext").unwrap();
    assert_eq!(info["generation"], 2);
    assert_eq!(info["dynamic"]["stamp"]["abi_epoch"], "1");
}

#[test]
fn registry_from_inputs_without_a_project() {
    let found = discover::discover(&[]);
    let r =
        ExtensionRegistry::resolve_from(RegistryInputs::for_host(None, ExtensionsConfig::default(), found));
    assert!(r.resolved().order.is_empty());
    assert_eq!(r.list_json(None)["extensions"], json!([]));
    assert!(r.info_json("nothing").is_none());
}

#[cfg(unix)]
mod tools {
    use std::time::Duration;

    use regia_framework::extension::tool::{ToolRun, run_tool};

    fn sh(script: &str, timeout_ms: u64, max_output: usize) -> ToolRun {
        ToolRun {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            stdin: b"input".to_vec(),
            cwd: None,
            timeout: Duration::from_millis(timeout_ms),
            max_output,
        }
    }

    #[test]
    fn tool_output_env_timeout_and_size_bound() {
        // Echoes stdin; the environment is cleared (HOME, like any secret, is not inherited).
        assert!(
            std::env::var_os("HOME").is_some(),
            "the test needs HOME set in the parent"
        );
        let out = run_tool(&sh("cat; printf \"[$HOME]\"", 20_000, 1 << 20)).unwrap();
        assert_eq!(out.status, Some(0));
        assert_eq!(out.stdout, b"input[]");
        // Killed at the timeout: the child would otherwise sleep far longer.
        let err = run_tool(&sh("sleep 600", 300, 1 << 20)).unwrap_err();
        assert_eq!(err.code(), "tool_timeout");
        // Killed when output passes the bound.
        let err = run_tool(&sh("yes regia", 20_000, 4096)).unwrap_err();
        assert_eq!(err.code(), "tool_output_too_large");
        // A missing command is a load failure.
        let mut missing = sh("", 1000, 10);
        missing.command = "/nonexistent/regia-tool".into();
        assert_eq!(run_tool(&missing).unwrap_err().code(), "load_failed");
    }
}

mod cooked_and_levels {
    use super::*;
    use regia_framework::extension::LevelFeatures;
    use regia_framework::extension::cooked::{COOKED_FILE, COOKED_FORMAT_VERSION, CookedExtensions};
    use regia_scene::level::LevelMember;

    #[test]
    fn a_cooked_record_that_disagrees_is_an_error() {
        let p = basic_project("ext-cooked");
        let mut app = app_for(&p.dir, vec![Arc::new(Basic), Arc::new(Rival)]);
        app.update();
        let r = registry(&app);
        let record = CookedExtensions::of(r.resolved(), &r.inputs().discovery.manifests);
        assert_eq!(record.format_version, COOKED_FORMAT_VERSION);
        assert_eq!(record.extensions.len(), 2);
        let text = record.to_ron().unwrap();
        assert_eq!(CookedExtensions::parse(&text).unwrap(), record);
        assert_eq!(
            record.mismatch(r.resolved(), &r.inputs().discovery.manifests),
            None
        );

        // The same record beside the paks of a project that now resolves one extension fewer.
        let pak_dir = p.dir.join("build/paks").join(r.inputs().platform.clone());
        write(&pak_dir.join(COOKED_FILE), &text);
        write(
            &p.dir.join("Regia.toml"),
            "[extensions]\nenabled = [\"fixture_rival\"]\n\n[extensions.grants]\nfixture_rival = [\"agent_method\", \"asset_loader\"]\n",
        );
        let app = app_for(&p.dir, vec![Arc::new(Rival)]);
        let codes: Vec<&str> = registry(&app).diagnostics().iter().map(|d| d.code).collect();
        assert!(codes.contains(&"extension_set_mismatch"), "{codes:?}");

        // A newer record in a shape this build cannot read is still too new, not malformed.
        let reshaped = "(format_version: 99, resolved_hash: 7, extensions: \"moved\", added: [1, 2])";
        assert!(
            CookedExtensions::parse(reshaped)
                .unwrap_err()
                .to_string()
                .contains("manifest_too_new")
        );

        let newer = text.replacen("format_version: 1", "format_version: 99", 1);
        assert!(
            CookedExtensions::parse(&newer)
                .unwrap_err()
                .to_string()
                .contains("manifest_too_new")
        );
        assert!(CookedExtensions::parse("(").is_err());
    }

    #[test]
    fn level_features_hold_packs_by_reference_while_a_level_is_loaded() {
        let p = basic_project("ext-level-features");
        let mut app = app_for(&p.dir, vec![Arc::new(Basic), Arc::new(Rival)]);
        let tagged = app.world_mut().spawn(Tags::from_iter_str(["outdoor"])).id();
        app.update();
        assert_eq!(registry(&app).packs().state("rain"), Some(PackState::Declared));

        // Two levels ask for the feature; the pack stays on until both are gone.
        let feature = || LevelFeatures {
            features: vec!["rain_weather".into()],
        };
        let a = app.world_mut().spawn((feature(), LevelMember(1))).id();
        let b = app.world_mut().spawn((feature(), LevelMember(2))).id();
        app.update();
        assert_eq!(registry(&app).packs().state("rain"), Some(PackState::Active));
        assert!(app.world().get::<Wet>(tagged).is_some());

        app.world_mut().despawn(a);
        app.update();
        assert_eq!(registry(&app).packs().state("rain"), Some(PackState::Active));

        app.world_mut().despawn(b);
        app.update();
        assert_eq!(registry(&app).packs().state("rain"), Some(PackState::Withdrawing));
        assert!(app.world().get::<Wet>(tagged).is_none());
        app.update();
        assert_eq!(registry(&app).packs().state("rain"), Some(PackState::Declared));
    }

    #[test]
    fn content_loads_through_mounts_and_registered_loaders() {
        let p = basic_project("ext-load-content");
        let ext = p.dir.join("extensions/fixture_basic");
        write(&ext.join("content/talk.dlg"), "12345");
        write(&ext.join("content/talk.bin"), "x");
        let text = std::fs::read_to_string(ext.join(MANIFEST_FILE)).unwrap();
        write(
            &ext.join(MANIFEST_FILE),
            &format!("{text}\n[[content]]\nroot = \"content\"\n"),
        );
        let mut app = app_for(&p.dir, vec![Arc::new(Basic), Arc::new(Rival)]);
        app.update();
        let r = registry(&app);
        let (hit, value) = r.load_content("ext://fixture_basic/talk.dlg", None).unwrap();
        assert_eq!(hit.origin, "extension:fixture_basic");
        assert_eq!(value.downcast_ref::<u32>(), Some(&5));
        assert!(
            r.load_content("ext://fixture_basic/talk.bin", None)
                .unwrap_err()
                .contains("no extension loads")
        );
        assert!(
            r.load_content("ext://fixture_basic/none.dlg", None)
                .unwrap_err()
                .contains("no content")
        );
        let info = r.info_json("fixture_basic").unwrap();
        assert_eq!(info["mounts"][0]["origin"], "extension:fixture_basic");
        assert_eq!(info["mounts"][0]["files"], 2);
    }
}

/// Spec 57 § 3.3.1, § 3.4 and § 7: where an extension's systems run, and that they really leave.
mod systems {
    use super::*;

    #[derive(Resource, Default)]
    struct Ran(Vec<&'static str>);

    fn ran_a(mut log: ResMut<Ran>) {
        log.0.push("a");
    }
    fn ran_b(mut log: ResMut<Ran>) {
        log.0.push("b");
    }
    fn ran_c(mut log: ResMut<Ran>) {
        log.0.push("c");
    }

    struct SystemPlugin;
    impl Plugin for SystemPlugin {
        fn build(&self, app: &mut App) {
            app.add_systems(Update, ran_c);
        }
        fn name(&self) -> &str {
            "system_plugin"
        }
    }

    #[derive(Clone, Copy)]
    enum Adds {
        Update(fn(ResMut<Ran>)),
        First(fn(ResMut<Ran>)),
        Twice,
        Plugin,
        PluginThenFail,
        Fail,
    }

    struct Sys {
        id: &'static str,
        adds: Adds,
    }
    impl RegiaExtension for Sys {
        fn id(&self) -> &str {
            self.id
        }
        fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
            match self.adds {
                Adds::Update(system) => cx.add_systems(Update, system),
                Adds::First(system) => cx.add_systems(First, system),
                Adds::Twice => {
                    cx.add_systems(Update, ran_a)?;
                    cx.add_systems(Update, ran_b)
                }
                Adds::Plugin => cx.add_plugin(SystemPlugin),
                Adds::PluginThenFail => {
                    cx.add_plugin(SystemPlugin)?;
                    Err(ExtensionError::BuildFailed("after the plugin".into()))
                }
                Adds::Fail => {
                    cx.add_systems(Update, ran_c)?;
                    Err(ExtensionError::BuildFailed("told to".into()))
                }
            }
        }
    }

    fn sys(id: &'static str, adds: Adds) -> Arc<dyn RegiaExtension> {
        Arc::new(Sys { id, adds })
    }

    fn system_project(tag: &str, toml: &str, bodies: &[(&str, &str)]) -> Project {
        let p = Project::new(tag, toml);
        for (id, body) in bodies {
            manifest(
                &p.dir.join("extensions").join(id),
                id,
                &format!("capabilities = [\"systems\"]\n{body}{CODE}"),
            );
        }
        p
    }

    fn app_with(project: &Path, linked: Vec<Arc<dyn RegiaExtension>>) -> App {
        let mut app = app_for(project, linked);
        app.init_resource::<Ran>();
        app
    }

    fn frame(app: &mut App) -> Vec<&'static str> {
        app.update();
        std::mem::take(&mut app.world_mut().resource_mut::<Ran>().0)
    }

    const DEPENDS: &str = "[[dependency]]\nid = \"sys_b\"\nversion = \">=1\"\n";

    #[test]
    fn a_dependency_orders_system_sets_in_either_direction() {
        let toml = "[extensions]\nenabled = [\"sys_a\", \"sys_b\"]\n";
        let linked = || {
            vec![
                sys("sys_a", Adds::Update(ran_a)),
                sys("sys_b", Adds::Update(ran_b)),
            ]
        };
        let p = system_project("ext-order-dep", toml, &[("sys_a", DEPENDS), ("sys_b", "")]);
        assert_eq!(frame(&mut app_with(&p.dir, linked())), ["b", "a"]);
        let flipped = DEPENDS.replace("sys_b", "sys_a");
        let p = system_project("ext-order-dep2", toml, &[("sys_a", ""), ("sys_b", &flipped)]);
        assert_eq!(frame(&mut app_with(&p.dir, linked())), ["a", "b"]);
    }

    #[test]
    fn the_project_orders_extensions_without_a_dependency_edge() {
        let linked = || {
            vec![
                sys("sys_a", Adds::Update(ran_a)),
                sys("sys_b", Adds::Update(ran_b)),
            ]
        };
        let base = "[extensions]\nenabled = [\"sys_a\", \"sys_b\"]\n";
        let bodies = [("sys_a", ""), ("sys_b", "")];
        let p = system_project(
            "ext-order-before",
            &format!("{base}[extensions.order.sys_b]\nbefore = [\"sys_a\"]\n"),
            &bodies,
        );
        assert_eq!(frame(&mut app_with(&p.dir, linked())), ["b", "a"]);
        let p = system_project(
            "ext-order-after",
            &format!("{base}[extensions.order.sys_b]\nafter = [\"sys_a\"]\n"),
            &bodies,
        );
        assert_eq!(frame(&mut app_with(&p.dir, linked())), ["a", "b"]);
    }

    #[test]
    fn a_bad_order_is_reported_and_cannot_stop_the_schedule() {
        let p = system_project(
            "ext-order-cycle",
            "[extensions]\nenabled = [\"sys_a\", \"sys_b\"]\n\
             [extensions.order.sys_a]\nafter = [\"sys_b\"]\n\
             [extensions.order.sys_b]\nafter = [\"sys_a\", \"nobody\"]\n",
            &[("sys_a", ""), ("sys_b", "")],
        );
        let mut app = app_with(
            &p.dir,
            vec![
                sys("sys_a", Adds::Update(ran_a)),
                sys("sys_b", Adds::Update(ran_b)),
            ],
        );
        // The first entry stands; the one that closes the cycle is dropped.
        assert_eq!(frame(&mut app), ["b", "a"]);
        let codes: Vec<&str> = registry(&app).diagnostics().iter().map(|d| d.code).collect();
        assert!(codes.contains(&"dependency_cycle"), "{codes:?}");
        assert!(codes.contains(&"not_found"), "{codes:?}");
    }

    #[test]
    fn plugin_info_lists_the_schedules_an_extension_is_in() {
        let p = system_project(
            "ext-order-info",
            "[extensions]\nenabled = [\"sys_a\", \"sys_b\"]\n",
            &[("sys_a", ""), ("sys_b", "")],
        );
        let mut app = app_with(
            &p.dir,
            vec![sys("sys_a", Adds::Twice), sys("sys_b", Adds::Plugin)],
        );
        frame(&mut app);
        let r = registry(&app);
        for id in ["sys_a", "sys_b"] {
            assert_eq!(
                r.info_json(id).unwrap()["registrations"]["system_set"],
                json!(["Update"]),
                "{id}: a plugin's systems count too"
            );
        }
    }

    #[test]
    fn a_failed_build_leaves_no_systems_behind() {
        let p = system_project(
            "ext-fail-systems",
            "[extensions]\nenabled = [\"sys_a\", \"sys_b\"]\n",
            &[("sys_a", ""), ("sys_b", "")],
        );
        let mut app = app_with(
            &p.dir,
            vec![
                sys("sys_a", Adds::PluginThenFail),
                sys("sys_b", Adds::Update(ran_b)),
            ],
        );
        assert_eq!(frame(&mut app), ["b"]);
        assert_eq!(frame(&mut app), ["b"]);
        assert_eq!(registry(&app).state("sys_a"), Some(ExtensionState::Failed));
    }

    fn live_project(tag: &str) -> Project {
        system_project(tag, "[extensions]\nenabled = [\"sys_a\"]\n", &[("sys_a", "")])
    }

    fn copy(adds: Adds) -> Sys {
        Sys { id: "sys_a", adds }
    }

    #[test]
    fn a_reload_swaps_systems_at_the_frame_boundary() {
        let p = live_project("ext-reload-systems");
        let mut app = app_with(&p.dir, vec![sys("sys_a", Adds::Update(ran_a))]);
        assert_eq!(frame(&mut app), ["a"]);
        let (old, new) = (copy(Adds::Update(ran_a)), copy(Adds::Update(ran_b)));
        reload_in_world(app.world_mut(), &old, &new).unwrap();
        assert_eq!(
            frame(&mut app),
            ["b"],
            "the old copy's system is gone, not doubled"
        );
        assert_eq!(frame(&mut app), ["b"]);
        // And again: the set survives a removal.
        let third = copy(Adds::Update(ran_c));
        reload_in_world(app.world_mut(), &new, &third).unwrap();
        assert_eq!(frame(&mut app), ["c"]);
    }

    #[test]
    fn a_reload_removes_systems_a_plugin_added() {
        let p = live_project("ext-reload-plugin");
        let mut app = app_with(&p.dir, vec![sys("sys_a", Adds::Plugin)]);
        assert_eq!(frame(&mut app), ["c"]);
        let (old, new) = (copy(Adds::Plugin), copy(Adds::Update(ran_b)));
        reload_in_world(app.world_mut(), &old, &new).unwrap();
        assert_eq!(frame(&mut app), ["b"]);
    }

    #[test]
    fn a_failed_reload_keeps_the_old_systems_and_nothing_of_the_new() {
        let p = live_project("ext-reload-fail");
        let mut app = app_with(&p.dir, vec![sys("sys_a", Adds::Update(ran_a))]);
        assert_eq!(frame(&mut app), ["a"]);
        let old = copy(Adds::Update(ran_a));
        // Queues a system, then fails.
        let err = reload_in_world(app.world_mut(), &old, &copy(Adds::Fail)).unwrap_err();
        assert_eq!(err.0, "reload_failed");
        assert_eq!(frame(&mut app), ["a"]);
        // A plugin cannot be added to a running app; the old copy stays.
        let err = reload_in_world(app.world_mut(), &old, &copy(Adds::Plugin)).unwrap_err();
        assert_eq!(err.0, "reload_failed");
        assert!(err.1.contains("restart_required"), "{}", err.1);
        assert_eq!(frame(&mut app), ["a"]);
        assert_eq!(
            registry(&app).info_json("sys_a").unwrap()["registrations"]["system_set"],
            json!(["Update"])
        );
    }

    #[test]
    fn a_reload_of_a_system_in_the_running_schedule_waits_one_boundary() {
        let p = live_project("ext-reload-first");
        let mut app = app_with(&p.dir, vec![sys("sys_a", Adds::First(ran_a))]);
        assert_eq!(frame(&mut app), ["a"]);
        let (old, new) = (copy(Adds::First(ran_a)), copy(Adds::First(ran_b)));
        reload_in_world(app.world_mut(), &old, &new).unwrap();
        // `First` is out of `Schedules` at its own boundary, so the swap lands at the frame end.
        assert_eq!(frame(&mut app), ["a"]);
        assert_eq!(frame(&mut app), ["b"]);
    }
}

/// Spec 57 § 3.3.6: render passes through the frame graph's slots. No GPU: the graph records them
/// by name, and `regia_render`'s own tests cover what a registered pass does in a frame.
mod render_passes {
    use super::*;

    struct Passes {
        names: Vec<&'static str>,
        fail: bool,
    }
    impl RegiaExtension for Passes {
        fn id(&self) -> &str {
            "passes"
        }
        fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
            for name in &self.names {
                cx.add_render_pass(Box::new(NoopPass(name)))?;
            }
            if self.fail {
                return Err(ExtensionError::BuildFailed("told to".into()));
            }
            Ok(())
        }
    }

    fn passes(names: &[&'static str]) -> Passes {
        Passes {
            names: names.to_vec(),
            fail: false,
        }
    }

    fn project(tag: &str) -> Project {
        let p = Project::new(
            tag,
            "[extensions]\nenabled = [\"passes\"]\n[extensions.grants]\npasses = [\"render_pass\"]\n",
        );
        manifest(
            &p.dir.join("extensions/passes"),
            "passes",
            &format!("capabilities = [\"render_pass\"]\n{CODE}"),
        );
        p
    }

    fn in_graph(app: &App) -> Vec<String> {
        app.world().resource::<RuntimeRenderer>().0.registered_passes()
    }

    #[test]
    fn a_pass_waits_for_the_renderer_and_is_listed() {
        let p = project("ext-pass");
        let mut app = app_for(&p.dir, vec![Arc::new(passes(&["outline"]))]);
        app.update();
        let info = registry(&app).info_json("passes").unwrap();
        assert_eq!(info["registrations"]["render_pass"], json!(["outline"]));
        app.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        app.update();
        assert_eq!(in_graph(&app), ["outline"]);
    }

    #[test]
    fn a_malformed_name_is_refused_at_registration() {
        let p = project("ext-pass-name");
        let app = app_for(&p.dir, vec![Arc::new(passes(&["Not A Name"]))]);
        assert_eq!(registry(&app).state("passes"), Some(ExtensionState::Failed));
        assert_eq!(registry(&app).codes_for("passes"), ["render_pass_invalid"]);
    }

    #[test]
    fn a_pass_the_graph_refuses_warns_and_the_app_goes_on() {
        let p = project("ext-pass-builtin");
        let mut app = app_for(&p.dir, vec![Arc::new(passes(&["shadows", "outline"]))]);
        app.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        app.update();
        assert_eq!(in_graph(&app), ["outline"], "`shadows` is a built-in pass");
        let r = registry(&app);
        assert_eq!(r.state("passes"), Some(ExtensionState::Active));
        assert_eq!(r.codes_for("passes"), ["render_pass_invalid"]);
        assert_eq!(
            r.info_json("passes").unwrap()["registrations"]["render_pass"],
            json!(["outline"])
        );
    }

    #[test]
    fn a_reload_swaps_passes_and_a_failed_one_puts_them_back() {
        let p = project("ext-pass-reload");
        let mut app = app_for(&p.dir, vec![Arc::new(passes(&["one"]))]);
        app.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        app.update();
        let (old, new) = (passes(&["one"]), passes(&["two"]));
        reload_in_world(app.world_mut(), &old, &new).unwrap();
        assert_eq!(in_graph(&app), ["two"]);
        let broken = Passes {
            names: vec!["three"],
            fail: true,
        };
        let err = reload_in_world(app.world_mut(), &new, &broken).unwrap_err();
        assert_eq!(err.0, "reload_failed");
        assert_eq!(in_graph(&app), ["two"]);
        app.update();
        assert_eq!(
            in_graph(&app),
            ["two"],
            "nothing of the failed copy arrives later"
        );
    }
}

/// Packs through a reload and through a tag that goes away (spec § 3.3.8, § 3.4).
mod pack_life {
    use super::*;

    struct Rainy(Vec<ComponentSpec>);
    impl RegiaExtension for Rainy {
        fn id(&self) -> &str {
            "rainy"
        }
        fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
            if self.0.is_empty() {
                return Ok(());
            }
            cx.declare_feature_pack(FeaturePackDesc {
                id: "rain".into(),
                tag: "outdoor".into(),
                components: self.0.clone(),
                requires_level_feature: "rain_weather".into(),
            })
        }
    }

    fn wet() -> Vec<ComponentSpec> {
        vec![ComponentSpec {
            type_path: Wet::get_type_registration().type_info().type_path().to_owned(),
        }]
    }

    fn app_on(p: &Project) -> (App, Entity) {
        manifest(
            &p.dir.join("extensions/rainy"),
            "rainy",
            &format!("capabilities = [\"feature_pack\"]\n{CODE}"),
        );
        let mut app = app_for(&p.dir, vec![Arc::new(Rainy(wet()))]);
        let outdoor = app.world_mut().spawn(Tags::from_iter_str(["outdoor"])).id();
        app.world_mut()
            .resource_mut::<ExtensionRegistry>()
            .enable_level_feature("rain_weather", "levels/a.level.ron");
        app.update();
        assert_eq!(app.world().get::<Wet>(outdoor), Some(&Wet(0)));
        (app, outdoor)
    }

    const TOML: &str =
        "[extensions]\nenabled = [\"rainy\"]\n[extensions.grants]\nrainy = [\"feature_pack\"]\n";

    #[test]
    fn what_a_pack_added_stays_tracked_across_a_reload() {
        let p = Project::new("ext-pack-reload", TOML);
        let (mut app, outdoor) = app_on(&p);
        reload_in_world(app.world_mut(), &Rainy(wet()), &Rainy(wet())).unwrap();
        app.update();
        assert_eq!(registry(&app).packs().state("rain"), Some(PackState::Active));
        assert_eq!(registry(&app).packs().applied_count("rain"), 1);
        app.world_mut()
            .resource_mut::<ExtensionRegistry>()
            .release_level("levels/a.level.ron");
        app.update();
        assert!(
            app.world().get::<Wet>(outdoor).is_none(),
            "the new copy withdraws what the old copy added"
        );
    }

    #[test]
    fn a_pack_the_new_copy_dropped_withdraws_at_the_reload() {
        let p = Project::new("ext-pack-dropped", TOML);
        let (mut app, outdoor) = app_on(&p);
        reload_in_world(app.world_mut(), &Rainy(wet()), &Rainy(Vec::new())).unwrap();
        assert!(app.world().get::<Wet>(outdoor).is_none());
        assert_eq!(registry(&app).packs().state("rain"), None);
    }

    #[test]
    fn an_entity_that_loses_the_tag_loses_the_pack() {
        let p = Project::new("ext-pack-tag", TOML);
        let (mut app, outdoor) = app_on(&p);
        app.world_mut()
            .entity_mut(outdoor)
            .insert(Tags::from_iter_str(["indoor"]));
        app.update();
        assert!(app.world().get::<Wet>(outdoor).is_none());
        assert_eq!(registry(&app).packs().applied_count("rain"), 0);
    }
}

/// A packaged game: no `Regia.toml`, no manifests on disk (spec § 2.1, § 9).
mod packaged {
    use super::*;

    const CODED: &str = "format_version = 1\nid = \"coded\"\nversion = \"1.0.0\"\ncapabilities = [\"agent_method\"]\n\n[[code]]\ncrate = \"coded\"\naudience = [\"game\"]\n";
    const ART: &str =
        "format_version = 1\nid = \"art\"\nversion = \"2.0.0\"\n\n[[content]]\nroot = \"content\"\n";
    const CONFIG: &str =
        "[extensions]\nenabled = [\"coded\", \"art\"]\n[extensions.grants]\ncoded = [\"agent_method\"]\n";

    #[derive(Default)]
    struct Coded;
    impl RegiaExtension for Coded {
        fn id(&self) -> &str {
            "coded"
        }
        fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
            let mount = cx.mount_content(Path::new("."));
            if self.id() != "coded" {
                return mount.map(|_| ());
            }
            cx.register_agent_method(hello("coded.hello", "linked"))
        }
    }

    fn linked() -> LinkedExtensions {
        LinkedExtensions {
            extensions: vec![Arc::new(Coded)],
            manifests: vec![CODED, ART],
            config: CONFIG,
        }
    }

    fn app_with(plugin: ExtensionPlugin) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(plugin);
        app
    }

    #[test]
    fn the_linked_set_builds_with_nothing_on_disk() {
        let mut app = app_with(ExtensionPlugin::for_game(None, linked()));
        app.update();
        let r = registry(&app);
        assert_eq!(r.inputs().project_dir, None);
        assert_eq!(r.build_state("coded"), BuildState::Built);
        assert_eq!(r.state("art"), Some(ExtensionState::Active));
        assert!(
            r.agent_method("coded.hello").is_some(),
            "the grant came from the binary"
        );
        assert!(r.diagnostics().is_empty(), "{:?}", r.diagnostics());
        assert_eq!(r.resolved().order.len(), 2);
        assert_eq!(r.mounts().list().len(), 0, "the paks hold the content");
    }

    #[test]
    fn a_project_that_is_not_there_counts_as_packaged() {
        let dir = unique_temp_dir("ext-no-project");
        let app = app_with(ExtensionPlugin::for_game(Some(dir.join("gone")), linked()));
        assert_eq!(registry(&app).build_state("coded"), BuildState::Built);
    }

    #[test]
    fn a_project_on_disk_wins_over_the_linked_set() {
        let p = Project::new("ext-dev-run", "[extensions]\nenabled = [\"coded\"]\n");
        std::fs::create_dir_all(p.dir.join("extensions/coded")).unwrap();
        std::fs::write(p.dir.join("extensions/coded").join(MANIFEST_FILE), CODED).unwrap();
        let app = app_with(ExtensionPlugin::for_game(Some(p.dir.clone()), linked()));
        let r = registry(&app);
        assert_eq!(r.inputs().project_dir, Some(p.dir.clone()));
        let codes = r.codes_for("coded");
        assert!(
            codes.contains(&"capability_not_granted"),
            "the project's own grants apply: {codes:?}"
        );
        assert_eq!(r.state("coded"), Some(ExtensionState::Failed));
    }

    #[test]
    fn a_damaged_embedded_manifest_is_a_diagnostic() {
        let mut set = linked();
        set.manifests.push("not toml = = =");
        let app = app_with(ExtensionPlugin::for_game(None, set));
        let r = registry(&app);
        assert!(r.diagnostics().iter().any(|d| d.code == "manifest_invalid"));
        assert_eq!(r.build_state("coded"), BuildState::Built, "the rest still builds");
    }

    #[test]
    fn the_table_survives_a_trip_through_the_generated_text() {
        let mut config = ExtensionsConfig::from_project_toml(
            "[extensions]\nenabled = [\"a\"]\ndisabled = [\"b\"]\n\
             [extensions.platform.android]\ndisabled = [\"c\"]\n\
             [extensions.grants]\na = [\"process\"]\n\
             [extensions.settings.a]\nmax = 3\n\
             [extensions.order.a]\nafter = [\"d\"]\n",
        )
        .unwrap();
        let text = config.to_project_toml().unwrap();
        assert_eq!(ExtensionsConfig::from_project_toml(&text).unwrap(), config);
        config.enabled.clear();
        assert_ne!(ExtensionsConfig::from_project_toml(&text).unwrap(), config);
    }

    #[test]
    fn plugin_list_reports_build_time_for_code_only() {
        let app = app_with(ExtensionPlugin::for_game(None, linked()));
        let list = registry(&app).list_json(None);
        let of = |id: &str| {
            list["extensions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"] == id)
                .unwrap()["load_ms"]
                .clone()
        };
        assert!(of("coded").is_number());
        assert!(of("art").is_null());
    }
}

/// What an outside audit found (2026-10-30): extensions that fail or point somewhere they should
/// not must cost only themselves.
mod hardening {
    use super::*;

    #[derive(Resource, Default)]
    struct Ran(u32);

    fn ran(mut count: ResMut<Ran>) {
        count.0 += 1;
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Panics {
        Nowhere,
        InBuild,
        InFinish,
        InOnUnload,
        InOnLoad,
    }

    struct Fragile {
        id: &'static str,
        panics: Panics,
    }
    impl RegiaExtension for Fragile {
        fn id(&self) -> &str {
            self.id
        }
        fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
            if self.panics == Panics::InBuild {
                cx.add_systems(Update, ran)?;
                panic!("in build");
            }
            cx.register_agent_method(hello(&format!("{}.hello", self.id), "x"))
        }
        fn finish(&self, cx: &mut ExtensionContext<'_>) {
            let _ = cx.id();
            assert!(self.panics != Panics::InFinish, "in finish");
        }
        fn on_unload(&self, _world: &mut World) -> Option<DynamicStateBlob> {
            assert!(self.panics != Panics::InOnUnload, "in on_unload");
            None
        }
        fn on_load(&self, _world: &mut World, _prior: Option<DynamicStateBlob>) {
            assert!(self.panics != Panics::InOnLoad, "in on_load");
        }
    }

    fn fragile(id: &'static str, panics: Panics) -> Fragile {
        Fragile { id, panics }
    }

    fn project(tag: &str) -> Project {
        let p = Project::new(
            tag,
            "[extensions]\nenabled = [\"aa_frail\", \"bb_fine\"]\n[extensions.grants]\naa_frail = [\"agent_method\"]\nbb_fine = [\"agent_method\"]\n",
        );
        for id in ["aa_frail", "bb_fine"] {
            manifest(
                &p.dir.join("extensions").join(id),
                id,
                &format!("capabilities = [\"agent_method\", \"systems\"]\n{CODE}"),
            );
        }
        p
    }

    fn app_with(p: &Project, first: Panics) -> App {
        let mut app = app_for(
            &p.dir,
            vec![
                Arc::new(fragile("aa_frail", first)),
                Arc::new(fragile("bb_fine", Panics::Nowhere)),
            ],
        );
        app.init_resource::<Ran>();
        app
    }

    #[test]
    fn a_panic_in_build_costs_only_that_extension() {
        let p = project("ext-panic-build");
        let mut app = app_with(&p, Panics::InBuild);
        app.update();
        let r = registry(&app);
        assert_eq!(r.state("aa_frail"), Some(ExtensionState::Failed));
        assert_eq!(r.codes_for("aa_frail"), ["load_failed"]);
        assert!(
            r.agent_method("bb_fine.hello").is_some(),
            "the next one still built"
        );
        assert_eq!(app.world().resource::<Ran>().0, 0, "its system left with it");
    }

    #[test]
    fn a_panic_in_finish_costs_only_that_extension() {
        let p = project("ext-panic-finish");
        let app = app_with(&p, Panics::InFinish);
        let r = registry(&app);
        assert_eq!(r.state("aa_frail"), Some(ExtensionState::Failed));
        assert!(
            r.agent_method("aa_frail.hello").is_none(),
            "its registrations left"
        );
        assert!(r.agent_method("bb_fine.hello").is_some());
    }

    fn live(tag: &str, old: Panics) -> (Project, App, Fragile) {
        let p = project(tag);
        let app = app_with(&p, Panics::Nowhere);
        (p, app, fragile("aa_frail", old))
    }

    #[test]
    fn a_panic_in_on_unload_changes_nothing() {
        let (_p, mut app, old) = live("ext-panic-unload", Panics::InOnUnload);
        let new = fragile("aa_frail", Panics::Nowhere);
        let err = reload_in_world(app.world_mut(), &old, &new).unwrap_err();
        assert_eq!(err.0, "reload_failed");
        assert!(err.1.contains("on_unload"), "{}", err.1);
        let r = app
            .world()
            .get_resource::<ExtensionRegistry>()
            .expect("still in the world");
        assert_eq!(r.generation("aa_frail"), 0);
        assert!(r.agent_method("aa_frail.hello").is_some());
    }

    #[test]
    fn a_panic_in_the_new_build_restores_the_old_copy() {
        let (_p, mut app, old) = live("ext-panic-rebuild", Panics::Nowhere);
        let new = fragile("aa_frail", Panics::InBuild);
        let err = reload_in_world(app.world_mut(), &old, &new).unwrap_err();
        assert_eq!(err.0, "reload_failed");
        app.update();
        let r = registry(&app);
        assert!(r.agent_method("aa_frail.hello").is_some(), "restored");
        assert_eq!(r.generation("aa_frail"), 0);
        assert_eq!(
            app.world().resource::<Ran>().0,
            0,
            "nothing of the new copy arrived"
        );
    }

    #[test]
    fn a_panic_in_on_load_keeps_the_swap_and_says_so() {
        let (_p, mut app, old) = live("ext-panic-onload", Panics::Nowhere);
        let new = fragile("aa_frail", Panics::InOnLoad);
        let report = reload_in_world(app.world_mut(), &old, &new).unwrap();
        assert_eq!(report.generation, 1);
        assert!(registry(&app).codes_for("aa_frail").contains(&"load_failed"));
    }

    #[cfg(unix)]
    #[test]
    fn a_content_root_that_is_a_link_out_reads_nothing() {
        let p = Project::new("ext-link-root", "[extensions]\nenabled = [\"art\"]\n");
        let outside = unique_temp_dir("ext-link-outside");
        write(&outside.join("id_rsa"), "SECRET");
        let dir = p.dir.join("extensions/art");
        manifest(&dir, "art", "[[content]]\nroot = \"content\"\n");
        std::os::unix::fs::symlink(&outside, dir.join("content")).unwrap();
        let app = app_for(&p.dir, Vec::new());
        let r = registry(&app);
        assert!(r.mounts().resolve("ext://art/id_rsa", None).is_none());
        assert!(r.mounts().list().is_empty());
        assert_eq!(r.codes_for("art"), ["path_escape"]);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_swapped_for_a_link_after_the_mount_reads_nothing() {
        let root = unique_temp_dir("ext-link-swap");
        let outside = unique_temp_dir("ext-link-swap-out");
        write(&outside.join("secret"), "SECRET");
        write(&root.join("a.txt"), "fine");
        let mut mounts = ContentMounts::default();
        mounts.mount("art", &root).unwrap();
        assert!(mounts.resolve("ext://art/a.txt", None).is_some());
        std::fs::remove_file(root.join("a.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), root.join("a.txt")).unwrap();
        assert!(mounts.resolve("ext://art/a.txt", None).is_none());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn mounting_a_root_again_replaces_it_and_two_roots_both_count() {
        let base = unique_temp_dir("ext-remount");
        write(&base.join("one/a.txt"), "a");
        write(&base.join("two/b.txt"), "b");
        let mut mounts = ContentMounts::default();
        mounts.mount("art", &base.join("one")).unwrap();
        mounts.mount("art", &base.join("two")).unwrap();
        assert!(mounts.resolve("ext://art/a.txt", None).is_some());
        assert!(
            mounts.resolve("ext://art/b.txt", None).is_some(),
            "two roots of one extension"
        );
        write(&base.join("one/new.txt"), "n");
        mounts.mount("art", &base.join("one")).unwrap();
        assert_eq!(mounts.list().len(), 2);
        assert!(
            mounts.resolve("ext://art/new.txt", None).is_some(),
            "the new listing is the one used"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn claim_is_only_for_kinds_a_host_keeps() {
        struct Squatter;
        impl RegiaExtension for Squatter {
            fn id(&self) -> &str {
                "squatter"
            }
            fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
                assert_eq!(
                    cx.claim(RegistrationKind::AgentMethod, "dialogue.say")
                        .unwrap_err()
                        .code(),
                    "not_supported"
                );
                assert_eq!(
                    cx.claim(RegistrationKind::Importer, "glb").unwrap_err().code(),
                    "capability_not_granted"
                );
                Ok(())
            }
        }
        let p = Project::new("ext-claim", "[extensions]\nenabled = [\"squatter\"]\n");
        manifest(&p.dir.join("extensions/squatter"), "squatter", CODE);
        let app = app_for(&p.dir, vec![Arc::new(Squatter)]);
        let r = registry(&app);
        assert_eq!(r.build_state("squatter"), BuildState::Built);
        assert!(
            r.owner_of(RegistrationKind::AgentMethod, "dialogue.say")
                .is_none()
        );
    }

    /// A timeout reaches what the tool started: the grandchild is gone, and its pipes with it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_timeout_reaches_the_tools_children() {
        use regia_framework::extension::tool::{ToolRun, run_tool};
        let dir = unique_temp_dir("ext-tool-group");
        let pid_file = dir.join("grandchild");
        let run = ToolRun {
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "/bin/sh -c 'echo $$ > {} ; exec sleep 600' & wait",
                    pid_file.display()
                ),
            ],
            stdin: Vec::new(),
            cwd: None,
            timeout: std::time::Duration::from_millis(1500),
            max_output: 1 << 16,
        };
        assert_eq!(run_tool(&run).unwrap_err().code(), "tool_timeout");
        let pid = std::fs::read_to_string(&pid_file).expect("the grandchild started");
        let stat = format!("/proc/{}/stat", pid.trim());
        // Death is not instant after the signal; a bounded count of looks, no clock.
        let gone = (0..200_000).any(|_| {
            std::fs::read_to_string(&stat).map_or(true, |text| {
                text.rsplit(") ").next().is_some_and(|rest| rest.starts_with('Z'))
            })
        });
        assert!(gone, "the grandchild outlived the timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Spec 57 § 3.3.5: an extension's declared settings use `regia_framework::settings`
/// persistence: a change is written to the user settings file, and on the next start the saved
/// value wins over the `[extensions.settings.<id>]` table of `Regia.toml`.
#[test]
fn extension_settings_persist_to_the_user_settings_file_and_win_over_the_project_table() {
    use regia_framework::settings::{SettingsPath, SettingsPersistence, SettingsPlugin};

    let p = basic_project("ext-settings-persist");
    let user = unique_temp_dir("ext-settings-user");
    let path = user.join("settings.ron");
    let make = || {
        let mut app = App::new();
        app.insert_resource(SettingsPath(Some(path.clone())));
        app.add_plugins((MinimalPlugins, SettingsPlugin));
        app.world_mut()
            .resource_mut::<AppTypeRegistry>()
            .write()
            .register::<Wet>();
        app.add_plugins(ExtensionPlugin::for_project(
            p.dir.clone(),
            vec![Arc::new(Basic), Arc::new(Rival)],
        ));
        app
    };
    let mut app = make();
    app.update();
    assert_eq!(app.world().resource::<DialogueSettings>().max_line_chars, 240);
    app.world_mut().resource_mut::<DialogueSettings>().max_line_chars = 96;
    app.update();
    app.update();
    app.world_mut().resource_mut::<SettingsPersistence>().flush();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        text.contains("fixture_basic") && text.contains("96"),
        "the extension's settings are not in the user settings file: {text:?}"
    );
    drop(app);

    let mut again = make();
    again.update();
    assert_eq!(
        again.world().resource::<DialogueSettings>().max_line_chars,
        96,
        "the saved value wins over [extensions.settings.fixture_basic]"
    );
    // An untouched field keeps its value; nothing else is rewritten.
    assert!(!again.world().resource::<DialogueSettings>().voice);
    let _ = std::fs::remove_dir_all(&user);
}

/// Spec 57 § 3.5: a live reload posts `ExtensionEvent::Reloaded` with the new generation, which a
/// system (or `wait_until`) reads as a message after the frame; a failed reload posts none.
#[test]
fn a_reload_posts_the_reloaded_message_with_its_generation() {
    let p = Project::new(
        "ext-reloaded-msg",
        "[extensions]\nenabled = [\"live_ext\"]\n[extensions.grants]\nlive_ext = [\"agent_method\"]\n",
    );
    manifest(
        &p.dir.join("extensions/live_ext"),
        "live_ext",
        &format!("capabilities = [\"agent_method\", \"systems\"]\n{CODE}"),
    );
    let one = Live {
        tag: "one",
        fail: false,
    };
    let mut app = app_for(
        &p.dir,
        vec![Arc::new(Live {
            tag: "one",
            fail: false,
        })],
    );
    app.update();
    let mut cursor = MessageCursor::default();
    let _ = cursor
        .read(app.world().resource::<Messages<ExtensionEvent>>())
        .count();
    let two = Live {
        tag: "two",
        fail: false,
    };
    reload_in_world(app.world_mut(), &one, &two).unwrap();
    let broken = Live {
        tag: "three",
        fail: true,
    };
    assert!(reload_in_world(app.world_mut(), &two, &broken).is_err());
    app.update();
    let reloaded: Vec<ExtensionEvent> = cursor
        .read(app.world().resource::<Messages<ExtensionEvent>>())
        .filter(|e| matches!(e, ExtensionEvent::Reloaded { .. }))
        .cloned()
        .collect();
    assert_eq!(
        reloaded,
        vec![ExtensionEvent::Reloaded {
            id: "live_ext".into(),
            generation: 1
        }]
    );
}

/// Registers the same agent method name twice.
#[derive(Default)]
struct Twice;
impl RegiaExtension for Twice {
    fn id(&self) -> &str {
        "fixture_twice"
    }
    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        cx.register_agent_method(hello("fixture_twice.hello", "first"))?;
        cx.register_agent_method(hello("fixture_twice.hello", "second"))
    }
}

/// Spec 57 § 3.3.4 / § 8.7: a second registration of an agent method name the extension already
/// holds is `registration_conflict`; the build fails and the method is not left half-registered.
#[test]
fn the_same_agent_method_name_twice_is_a_registration_conflict() {
    let p = Project::new(
        "ext-method-twice",
        "[extensions]\nenabled = [\"fixture_twice\"]\n[extensions.grants]\nfixture_twice = [\"agent_method\"]\n",
    );
    manifest(
        &p.dir.join("extensions/fixture_twice"),
        "fixture_twice",
        &format!("capabilities = [\"agent_method\"]\n{CODE}"),
    );
    let app = app_for(&p.dir, vec![Arc::new(Twice)]);
    let r = registry(&app);
    let built = r.build_state("fixture_twice");
    assert!(
        matches!(built, BuildState::Failed("registration_conflict", _)),
        "{built:?}"
    );
    assert!(
        r.agent_method("fixture_twice.hello").is_none(),
        "a failed build withdraws its methods"
    );
}

/// Spec 57 § 4.1: a crate-published extension's manifest `id` must equal the crate's
/// `[package.metadata.regia] id`; a mismatch is `manifest_invalid` at discovery (the scan goes on),
/// a matching id and a crate without the table are discovered.
#[test]
fn a_crate_whose_metadata_regia_id_differs_from_the_manifest_is_refused() {
    let p = Project::new("ext-metadata-id", "");
    let ext = p.dir.join("extensions");
    let cargo = |id: &str| {
        format!("[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[package.metadata.regia]\nid = \"{id}\"\n")
    };
    manifest(&ext.join("a_good"), "a_good", CODE);
    write(&ext.join("a_good/Cargo.toml"), &cargo("a_good"));
    manifest(&ext.join("b_bad"), "b_bad", CODE);
    write(&ext.join("b_bad/Cargo.toml"), &cargo("someone_else"));
    manifest(&ext.join("c_plain"), "c_plain", CODE);
    write(
        &ext.join("c_plain/Cargo.toml"),
        "[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
    );
    let found = discover::discover(&[ext]);
    let ids: Vec<&str> = found.manifests.iter().map(|m| m.manifest.id.as_str()).collect();
    assert_eq!(ids, ["a_good", "c_plain"]);
    assert_eq!(found.diagnostics.len(), 1, "{:?}", found.diagnostics);
    assert_eq!(found.diagnostics[0].code, "manifest_invalid");
    assert_eq!(found.diagnostics[0].id, "b_bad");
    assert!(
        found.diagnostics[0].message.contains("someone_else"),
        "{}",
        found.diagnostics[0].message
    );
}

/// Spec 57 § 2.3: an extension whose code entries all name other audiences, with no content and
/// no tools, loads nothing in this host and is reported `filtered` (with an info diagnostic), not
/// `active`; one that also mounts content stays active.
#[test]
fn an_extension_with_no_code_for_this_audience_is_filtered() {
    let p = Project::new(
        "ext-audience-filtered",
        "[extensions]\nenabled = [\"ed_only\", \"ed_content\"]\n",
    );
    let ext = p.dir.join("extensions");
    let editor_code = "[[code]]\ncrate = \"ed\"\naudience = [\"editor\"]\n";
    manifest(&ext.join("ed_only"), "ed_only", editor_code);
    manifest(
        &ext.join("ed_content"),
        "ed_content",
        &format!("{editor_code}\n[[content]]\nroot = \"content\"\n"),
    );
    std::fs::create_dir_all(ext.join("ed_content/content")).unwrap();
    let app = app_for(&p.dir, Vec::new());
    let r = registry(&app);
    assert_eq!(r.state("ed_only"), Some(ExtensionState::Filtered));
    assert_eq!(r.state("ed_content"), Some(ExtensionState::Active));
    let list = r.list_json(None);
    let state_of = |id: &str| {
        list["extensions"]
            .as_array()
            .and_then(|a| a.iter().find(|e| e["id"] == id))
            .map(|e| e["state"].clone())
    };
    assert_eq!(state_of("ed_only"), Some(json!("filtered")));
    assert_eq!(state_of("ed_content"), Some(json!("active")));
}
