// SPDX-License-Identifier: MPL-2.0
//! Entity kinds and the project map on the agent protocol (`book/agents/entity-kinds.md`,
//! `docs/specs/101-entity-kinds-and-project-map.md` § 7, ADR-2488).
//!
//! - `kinds.list`: the registered kinds (code and data) by category, with registry notes.
//! - `kinds.of { entity }`: one entity's kind, derived role, families and unclassified flag.
//! - `kinds.lint { project_dir?, strictness? }`: the kind lint over the project's files.
//! - `project.describe { project_dir?, write?, out_dir? }`: the project map with the live system access.
//!
//! `scene.query_entities` (`kind` filter, `kind` per row) and `scene.get_entity` (`kind`, `role`,
//! `unclassified`) read [`entity_kind_json`].

use std::path::PathBuf;

use regia_app::prelude::*;
use regia_ecs::reflect::TypeRegistry;
use regia_scene::kind::lint::{Strictness, lint_project};
use regia_scene::kind::map::{build_map, write_map};
use regia_scene::kind::roles::{classify_entity, classify_entity_in};
use regia_scene::kind::scan::{self, AssetsStamp, KindCatalog, scan_kind_files, scan_project};
use regia_scene::kind::systems::{SystemAccessRecord, read_system_access};
use regia_scene::kind::{KindInfo, KindRegistry, kind_of, resolve_kind};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// An agent error with a kind code (`kind_*`, `describe_*`).
fn kind_error(code_num: i64, code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(code_num, code, message.into(), hint)
}

/// One kind as JSON.
fn kind_json(info: &KindInfo) -> Value {
    json!({
        "name": info.name,
        "icon": info.icon,
        "color": info.color_hex(),
        "category": info.category_name(),
        "description": info.description,
        "required": info.required,
        "source": info.source,
    })
}

/// The kind, role and unclassified flag of an entity (the `scene.*` additions). `registry` is the
/// type registry the caller already holds.
pub(crate) fn entity_kind_json(
    world: &World,
    registry: &TypeRegistry,
    entity: Entity,
) -> (Value, Value, bool) {
    let classification = classify_entity_in(world, Some(registry), entity);
    let kind = kind_of(world, entity);
    let unclassified = kind.is_none() && classification.is_multi_family();
    (
        kind.map_or(Value::Null, |k| Value::String(k.name)),
        classification
            .role
            .map_or(Value::Null, |r| Value::String(r.name().into())),
        unclassified,
    )
}

/// Whether an entity matches a `kind` filter: a kind name, `none` or `unclassified`. `registry` is
/// the type registry the caller already holds.
pub(crate) fn kind_matches(world: &World, registry: &TypeRegistry, entity: Entity, filter: &str) -> bool {
    let kind = kind_of(world, entity);
    match filter {
        "none" => kind.is_none(),
        "unclassified" => {
            kind.is_none() && classify_entity_in(world, Some(registry), entity).is_multi_family()
        }
        name => kind.is_some_and(|k| k.name == name),
    }
}

/// The memo of [`live_registry`]: the data-file scan is redone only when the project's assets
/// fingerprint or the code registry changed (review sprint-rev-spec-1-k finding 3 — the scan
/// walked `assets/` on every `kinds.list`/`kinds.of` call before).
#[derive(Resource, Default)]
struct LiveRegistryMemo {
    /// The project root the memo was built for.
    root: PathBuf,
    /// The assets fingerprint at scan time.
    stamp: Option<AssetsStamp>,
    /// The code registry at scan time (a kind registration or a project load invalidates).
    code: KindRegistry,
    /// The merged registry handed out on a memo hit.
    merged: KindRegistry,
    /// How many times the files were actually scanned; memo hits do not move it.
    scans: u64,
}

/// The world's [`KindRegistry`] plus the data kinds of the app's project (`.kind.ron` under
/// `<project root>/assets`; nothing is read without an `assets` folder). Declarations the registry
/// already holds (the editor loads them when it opens a project) change nothing; unreadable files
/// become notes (spec 101 § 2.3, ADR-5609). Memoised on the assets tree's fingerprint, so an
/// agent script calling `kinds.list`/`kinds.of` in a loop pays one directory walk per change, not
/// per call.
fn live_registry(world: &mut World) -> KindRegistry {
    let code = world.get_resource::<KindRegistry>().cloned().unwrap_or_default();
    let root = crate::req_path::project_root(world);
    let stamp = scan::assets_stamp(&root);
    if let Some(memo) = world.get_resource::<LiveRegistryMemo>()
        && memo.root == root
        && memo.stamp == stamp
        && memo.code.kinds() == code.kinds()
        && memo.code.notes() == code.notes()
    {
        return memo.merged.clone();
    }
    let (kinds, errors) = scan_kind_files(&root);
    let mut merged = code.clone();
    merged.register_data_files(&kinds);
    for (path, code, message) in &errors {
        merged.note_file_error(path, code, message);
    }
    let scans = world
        .get_resource::<LiveRegistryMemo>()
        .map_or(0, |memo| memo.scans)
        + 1;
    world.insert_resource(LiveRegistryMemo {
        root,
        stamp,
        code,
        merged: merged.clone(),
        scans,
    });
    merged
}

/// `kinds.list`.
pub(crate) fn list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let registry = live_registry(world);
    let categories: Vec<Value> = registry
        .by_category()
        .into_iter()
        .map(|(category, kinds)| {
            json!({
                "category": category,
                "kinds": kinds.into_iter().map(kind_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "count": registry.kinds().len(),
        "categories": categories,
        "notes": registry.notes(),
    }))
}

/// `kinds.of`.
pub(crate) fn of(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(kind_error(
            NOT_FOUND,
            "kind_not_found",
            format!("entity {} does not exist", entity.to_bits()),
            "use scene.query_entities",
        ));
    }
    let classification = classify_entity(world, entity);
    let kind = resolve_kind(&live_registry(world), world, entity);
    let unclassified = kind.is_none() && classification.is_multi_family();
    Ok(json!({
        "entity": entity.to_bits(),
        "kind": kind.as_ref().map(|k| k.name.clone()),
        "declared": kind.as_ref().map(|k| k.info.is_some()),
        "info": kind.as_ref().and_then(|k| k.info.as_ref()).map(kind_json),
        "via_prefab": kind.as_ref().is_some_and(|k| k.via_prefab),
        "conflicts": kind.as_ref().map(|k| k.conflicts.clone()).unwrap_or_default(),
        "role": classification.role.map(|r| r.name()),
        "roles": classification.roles.iter().map(|r| r.name()).collect::<Vec<_>>(),
        "families": classification.families.iter().map(|f| f.name()).collect::<Vec<_>>(),
        "unclassified": unclassified,
    }))
}

/// The project directory: the `project_dir` parameter (confined to the allowed roots), else the
/// app's project root.
fn project_dir(world: &World, params: &Value) -> Result<PathBuf, RpcError> {
    let dir = match crate::params::opt_str(params, "project_dir")? {
        Some(dir) => crate::req_path::allowed_path(world, "project_dir", dir)?,
        None => crate::req_path::project_root(world),
    };
    if !dir.is_dir() {
        return Err(kind_error(
            INVALID_PARAMS,
            "describe_no_project",
            format!("{} is not a project directory", dir.display()),
            "pass project_dir (a folder with Regia.toml and assets/)",
        ));
    }
    Ok(dir)
}

/// The project's name from `Regia.toml` (`[project] name`), else the folder name.
fn project_name(dir: &std::path::Path) -> String {
    let manifest = dir.join(regia_core::brand::MANIFEST_FILE);
    if let Ok(text) = std::fs::read_to_string(manifest)
        && let Ok(value) = text.parse::<toml::Table>()
        && let Some(name) = value
            .get("project")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
    {
        return name.to_string();
    }
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The project's `[kinds] strictness` (`off` when absent or unreadable).
fn project_strictness(dir: &std::path::Path) -> Strictness {
    let manifest = dir.join(regia_core::brand::MANIFEST_FILE);
    std::fs::read_to_string(manifest)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|t| {
            t.get("kinds")
                .and_then(|k| k.get("strictness"))
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or_default()
}

/// `kinds.lint`.
pub(crate) fn lint(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dir = project_dir(world, params)?;
    let strictness = match crate::params::opt_str(params, "strictness")? {
        Some(text) => text.parse::<Strictness>().map_err(|e| {
            kind_error(
                INVALID_PARAMS,
                "kind_bad_param",
                e,
                "strictness is off, warn or deny",
            )
        })?,
        None => project_strictness(&dir),
    };
    let files = scan_project(&dir);
    let catalog = KindCatalog::new(world.get_resource::<KindRegistry>(), &files);
    let findings = lint_project(&files, &catalog, strictness);
    let errors = findings
        .iter()
        .filter(|f| f.severity == regia_scene::kind::lint::Severity::Error)
        .count();
    Ok(json!({
        "strictness": strictness.name(),
        "levels": files.levels.len(),
        "prefabs": files.prefabs.len(),
        "findings": findings,
        "errors": errors,
        "passed": errors == 0,
    }))
}

/// `project.describe`.
pub(crate) fn describe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dir = project_dir(world, params)?;
    // Read before `write` decides whether it is used, so a wrong type is always refused.
    let out_dir = crate::params::opt_str(params, "out_dir")?;
    // The agent runs inside `PreUpdate`, which is out of the world now: merge the copy `Last` took.
    let mut systems = read_system_access(world);
    let pending = match world.get_resource_mut::<SystemAccessRecord>() {
        Some(mut record) => match record.table() {
            Some(table) => {
                systems.merge(table);
                false
            }
            None => {
                record.request();
                true
            }
        },
        None => false,
    };
    // A running schedule is out of the world and `read_system_access` cannot list it. Until `Last`
    // has recorded it, name the agent's own schedule as unread so the answer says it is incomplete
    // (spec 101 § 6.3, ADR-5609). The name is the dispatch schedule's, one shared constant
    // (`AGENT_DISPATCH_SCHEDULE`); its test fails if the dispatch site moves without it.
    const AGENT_SCHEDULE: &str = crate::AGENT_DISPATCH_SCHEDULE;
    if pending
        && !systems.schedules.iter().any(|s| s.schedule == AGENT_SCHEDULE)
        && !systems.unread.iter().any(|u| u == AGENT_SCHEDULE)
    {
        systems.unread.push(AGENT_SCHEDULE.to_string());
        systems.unread.sort();
    }
    let files = scan_project(&dir);
    let registry = world.get_resource::<KindRegistry>().cloned();
    let catalog = KindCatalog::new(registry.as_ref(), &files);
    let notes: Vec<String> = registry.map(|r| r.notes().to_vec()).unwrap_or_default();
    let map = build_map(&project_name(&dir), &files, &catalog, Some(&systems), &notes);
    let markdown = map.to_markdown();
    let mut written = Value::Null;
    if crate::params::opt_bool(params, "write")?.unwrap_or(false) {
        let out = match out_dir {
            Some(out) => crate::req_path::allowed_path(world, "out_dir", out)?,
            None => dir.clone(),
        };
        let (md, json_path) = write_map(&out, &map).map_err(|e| {
            kind_error(
                crate::protocol::INTERNAL_ERROR,
                "describe_write_failed",
                format!("cannot write the project map into {}: {e}", out.display()),
                "check that out_dir is writable",
            )
        })?;
        written = json!({ "markdown": md.display().to_string(), "json": json_path.display().to_string() });
    }
    let map_json = serde_json::to_value(&map)
        .map_err(|e| RpcError::internal(format!("cannot serialize the project map: {e}")))?;
    Ok(json!({
        "map": map_json,
        "markdown": markdown,
        "written": written,
        "record_pending": pending,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::kind::KindAppExt as _;

    #[derive(Component, Reflect, Default)]
    #[reflect(Component, Default)]
    struct Turret;

    regia_scene::entity_kind!(
        Turret,
        "Turret",
        icon = "weapon",
        color = [0x90, 0x20, 0x20],
        description = "Shoots at players",
        category = "AI"
    );

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.init_resource::<SystemAccessRecord>();
        app.register_kind::<Turret>();
        app
    }

    #[test]
    fn list_of_and_scene_queries_report_kinds() {
        let mut app = app();
        let turret = app.world_mut().spawn(Turret).id();
        let odd = app
            .world_mut()
            .spawn((
                regia_render::PointLight::default(),
                regia_audio::AudioEmitter::default(),
            ))
            .id();
        let world = app.world_mut();
        let listed = list(world, &json!({})).expect("list");
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["categories"][0]["category"], "AI");
        assert_eq!(listed["categories"][0]["kinds"][0]["name"], "Turret");

        let of_turret = of(world, &json!({ "entity": turret.to_bits() })).expect("of");
        assert_eq!(of_turret["kind"], "Turret");
        assert_eq!(of_turret["unclassified"], false);
        let of_odd = of(world, &json!({ "entity": odd.to_bits() })).expect("of");
        assert_eq!(of_odd["kind"], Value::Null);
        assert_eq!(of_odd["role"], "point");
        assert_eq!(of_odd["unclassified"], true);

        let missing = of(world, &json!({ "entity": Entity::PLACEHOLDER.to_bits() })).expect_err("gone");
        assert_eq!(
            missing
                .data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("kind_not_found")
        );

        let rows = crate::scene::query_entities(world, &json!({ "kind": "Turret" })).expect("query");
        assert_eq!(rows["count"], 1);
        assert_eq!(rows["entities"][0]["kind"], "Turret");
        let rows = crate::scene::query_entities(world, &json!({ "kind": "unclassified" })).expect("query");
        assert_eq!(rows["count"], 1);
        assert_eq!(rows["entities"][0]["entity"], odd.to_bits());
        let entity = crate::scene::get_entity(world, &json!({ "entity": odd.to_bits() })).expect("get");
        assert_eq!(entity["kind"], Value::Null);
        assert_eq!(entity["role"], "point");
        assert_eq!(entity["unclassified"], true);
    }

    fn project() -> regia_test_utils::TempDir {
        let root = regia_test_utils::TempDir::new("agent_kinds_project");
        std::fs::create_dir_all(root.join("assets/levels")).expect("mkdir");
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname = \"Turrets\"\n\n[kinds]\nstrictness = \"deny\"\n",
        )
        .expect("manifest");
        std::fs::write(
            root.join("assets/levels/a.level.ron"),
            r#"SceneFile(format_version: 8, entities: [
                (guid: "00000000-0000-4000-8000-000000000001", name: "T",
                    components: { "regia_agent::kinds::tests::Turret": () }),
                (guid: "00000000-0000-4000-8000-000000000002", name: "Loose",
                    components: { "my_game::Ammo": () }),
            ])"#,
        )
        .expect("level");
        root
    }

    #[test]
    fn lint_reads_the_project_strictness_and_knows_live_code_kinds() {
        let mut app = app();
        let root = project();
        let world = app.world_mut();
        let dir = root.display().to_string();
        let result = lint(world, &json!({ "project_dir": dir })).expect("lint");
        assert_eq!(result["strictness"], "deny");
        assert_eq!(result["errors"], 1, "{result:#}");
        assert_eq!(result["findings"][0]["code"], "kind_missing");
        assert_eq!(result["findings"][0]["name"], "Loose");
        let warn = lint(world, &json!({ "project_dir": dir, "strictness": "warn" })).expect("lint");
        assert_eq!(warn["passed"], true);
        let bad = lint(world, &json!({ "project_dir": dir, "strictness": "max" })).expect_err("bad");
        assert_eq!(
            bad.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("kind_bad_param")
        );
    }

    /// Spec 101 § 2.3 / § 7, ADR-5609: `kinds.list` lists the project's data kinds next to the code
    /// kinds and notes drift; `kinds.of` reports a tagged entity's declaration.
    #[test]
    fn list_and_of_know_the_project_data_kinds() {
        let mut app = app();
        let root = regia_test_utils::TempDir::new("agent_kinds_data");
        std::fs::create_dir_all(root.join("assets/kinds")).expect("mkdir");
        std::fs::write(
            root.join("assets/kinds/pickup.kind.ron"),
            r##"(format_version: 1, name: "Pickup", icon: "pickup", color: "#30c050", description: "Something to pick up")"##,
        )
        .expect("pickup");
        std::fs::write(
            root.join("assets/kinds/turret.kind.ron"),
            r##"(format_version: 1, name: "Turret", icon: "enemy", color: "#902020", description: "Shoots at players", category: Some("AI"))"##,
        )
        .expect("turret");
        app.insert_resource(crate::AgentSettings {
            project_dir: Some(root.path().to_path_buf()),
        });
        let tagged = app
            .world_mut()
            .spawn(regia_scene::kind::KindTag("Pickup".into()))
            .id();
        let world = app.world_mut();
        let listed = list(world, &json!({})).expect("list");
        assert_eq!(listed["count"], 2, "{listed:#}");
        assert_eq!(listed["categories"][0]["kinds"][0]["name"], "Turret");
        assert_eq!(listed["categories"][0]["kinds"][0]["source"]["type"], "code");
        assert_eq!(listed["categories"][1]["category"], "Uncategorized");
        assert_eq!(listed["categories"][1]["kinds"][0]["name"], "Pickup");
        assert_eq!(listed["categories"][1]["kinds"][0]["source"]["type"], "data");
        let notes = listed["notes"].as_array().expect("notes");
        assert!(
            notes
                .iter()
                .filter_map(Value::as_str)
                .any(|n| n.contains("turret.kind.ron") && n.contains("icon")),
            "drift note: {notes:?}"
        );
        let of_tagged = of(world, &json!({ "entity": tagged.to_bits() })).expect("of");
        assert_eq!(of_tagged["kind"], "Pickup");
        assert_eq!(of_tagged["declared"], true);
        assert_eq!(of_tagged["info"]["icon"], "pickup");
        assert!(
            world.resource::<KindRegistry>().get("Pickup").is_none(),
            "a read method does not change the app's registry"
        );
    }

    /// Review sprint-rev-spec-1-k finding 3: `kinds.list`/`kinds.of` memoise the assets scan on the
    /// tree's fingerprint — a second call re-scans nothing, an in-place edit is seen, and a new
    /// file (which moves the entry count even where timestamps are coarse) is seen.
    #[test]
    fn list_and_of_memoise_the_assets_scan() {
        let mut app = app();
        let root = regia_test_utils::TempDir::new("agent_kinds_memo");
        std::fs::create_dir_all(root.join("assets/kinds")).expect("mkdir");
        std::fs::write(
            root.join("assets/kinds/pickup.kind.ron"),
            r##"(format_version: 1, name: "Pickup", icon: "pickup", color: "#30c050", description: "Something to pick up")"##,
        )
        .expect("pickup");
        app.insert_resource(crate::AgentSettings {
            project_dir: Some(root.path().to_path_buf()),
        });
        let tagged = app
            .world_mut()
            .spawn(regia_scene::kind::KindTag("Pickup".into()))
            .id();
        let pickup_icon = |answer: &Value| {
            answer["categories"]
                .as_array()
                .expect("categories")
                .iter()
                .flat_map(|c| c["kinds"].as_array().cloned().unwrap_or_default())
                .find(|k| k["name"] == "Pickup")
                .and_then(|k| k["icon"].as_str().map(str::to_owned))
        };
        let world = app.world_mut();
        let first = list(world, &json!({})).expect("list");
        assert_eq!(first["count"], 2, "the code kind plus Pickup: {first:#}");
        assert_eq!(pickup_icon(&first).as_deref(), Some("pickup"));
        assert_eq!(world.resource::<LiveRegistryMemo>().scans, 1);

        // The second call is a memo hit: same answer, no re-scan.
        let second = list(world, &json!({})).expect("list");
        assert_eq!(second, first);
        assert_eq!(
            world.resource::<LiveRegistryMemo>().scans,
            1,
            "the memoised call re-scanned the assets tree"
        );

        // An edit moves the fingerprint: the next call re-scans and the change shows in both
        // `kinds.list` and `kinds.of`.
        std::fs::write(
            root.join("assets/kinds/pickup.kind.ron"),
            r##"(format_version: 1, name: "Pickup", icon: "gem", color: "#30c050", description: "Something to pick up")"##,
        )
        .expect("edit");
        let third = list(world, &json!({})).expect("list");
        assert_eq!(world.resource::<LiveRegistryMemo>().scans, 2);
        assert_eq!(pickup_icon(&third).as_deref(), Some("gem"), "{third:#}");
        let of_tagged = of(world, &json!({ "entity": tagged.to_bits() })).expect("of");
        assert_eq!(of_tagged["info"]["icon"], "gem");

        // A new file moves the entry count too, so it is seen even on coarse-timestamp filesystems.
        std::fs::write(
            root.join("assets/kinds/door.kind.ron"),
            r##"(format_version: 1, name: "Door", icon: "door", color: "#806040")"##,
        )
        .expect("add");
        let fourth = list(world, &json!({})).expect("list");
        assert_eq!(world.resource::<LiveRegistryMemo>().scans, 3);
        assert_eq!(fourth["count"], 3, "{fourth:#}");
    }

    /// The answers of a `project.describe` served from inside `PreUpdate`, as the agent serves it.
    #[derive(Resource, Default)]
    struct Answers(Vec<Value>);

    /// Spec 101 § 6.3: served inside `PreUpdate`, the first answer is `record_pending: true` and lists
    /// `PreUpdate` (out of the world) under `unread_schedules`; `Last` then records it once, and the
    /// next answer has `PreUpdate`'s systems and no unread `PreUpdate`.
    #[test]
    fn describe_inside_pre_update_completes_after_the_next_last() {
        let mut app = app();
        app.add_systems(
            Last,
            regia_scene::kind::systems::record_system_access
                .run_if(regia_scene::kind::systems::record_pending),
        );
        app.init_resource::<Answers>();
        let temp = project();
        let dir = temp.display().to_string();
        app.add_systems(PreUpdate, move |world: &mut World| {
            let answer = describe(world, &json!({ "project_dir": dir })).expect("describe");
            world.resource_mut::<Answers>().0.push(answer);
        });
        app.update();
        app.update();
        let answers = &app.world().resource::<Answers>().0;
        assert_eq!(answers.len(), 2);
        let in_pre_update = |answer: &Value| {
            answer["map"]["exclusive_systems"]
                .as_array()
                .expect("exclusive systems")
                .iter()
                .any(|s| s["schedule"] == "PreUpdate")
        };
        let unread = |answer: &Value| {
            answer["map"]["unread_schedules"]
                .as_array()
                .expect("unread")
                .iter()
                .any(|s| s == "PreUpdate")
        };
        assert_eq!(answers[0]["record_pending"], true);
        assert!(
            !in_pre_update(&answers[0]),
            "PreUpdate is running, so it is out of the world"
        );
        assert!(
            unread(&answers[0]),
            "and the first answer says so: {:#}",
            answers[0]["map"]["unread_schedules"]
        );
        assert_eq!(answers[1]["record_pending"], false);
        assert!(
            in_pre_update(&answers[1]),
            "Last recorded PreUpdate's exclusive describe system"
        );
        assert!(
            !unread(&answers[1]),
            "{:#}",
            answers[1]["map"]["unread_schedules"]
        );
    }

    /// Review sprint-rev-spec-1-k finding 2: `describe`'s pending `unread` entry names the dispatch
    /// schedule through [`crate::AGENT_DISPATCH_SCHEDULE`]; this fails if the dispatch site
    /// (`AgentPlugin`'s `add_systems(PreUpdate, process_requests)`) ever moves to another label
    /// without updating the constant.
    #[test]
    fn dispatch_schedule_name_matches() {
        assert_eq!(format!("{:?}", PreUpdate), crate::AGENT_DISPATCH_SCHEDULE);
    }

    #[test]
    fn describe_lists_systems_writes_the_map_and_is_stable() {
        fn aim(q: Query<&Turret>) {
            let _ = q.iter().count();
        }
        let mut app = app();
        app.add_systems(Update, aim);
        app.update();
        let root = project();
        let dir = root.display().to_string();
        let first =
            describe(app.world_mut(), &json!({ "project_dir": dir, "write": true })).expect("describe");
        let second =
            describe(app.world_mut(), &json!({ "project_dir": dir, "write": true })).expect("describe");
        assert_eq!(first["map"], second["map"], "two runs give the same map");
        assert_eq!(first["markdown"], second["markdown"]);
        assert_eq!(first["map"]["project"], "Turrets");
        assert_eq!(first["map"]["systems_source"], "live");
        assert!(root.join("project-map.md").is_file());
        assert!(root.join("project-map.json").is_file());
        let kinds = first["map"]["kinds"].as_array().expect("kinds");
        assert_eq!(kinds[0]["name"], "Turret");
        assert_eq!(kinds[0]["levels"][0]["level"], "assets/levels/a.level.ron");
        let no_project = describe(
            app.world_mut(),
            &json!({ "project_dir": root.join("nope").display().to_string() }),
        )
        .expect_err("missing");
        assert!(no_project.data.is_some());
    }
}
