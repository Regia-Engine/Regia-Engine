// SPDX-License-Identifier: MPL-2.0
//! CT-24: shipping closure integrity is independent of advisory cook validation.

use std::path::{Path, PathBuf};

use anyhow::Result;
use regia_asset::registry::{AssetRegistry, RegistryEntry, cache_registry_path};
use regia_asset::{
    AssetId, AssetManifest, CookedHeader, ManifestEntry, cooked_cache_dir, cooked_file_name,
    encode_cooked_file,
};
use regia_asset_pipeline::chunks::OPTIONAL_REFERENCE_TAG;
// The commands are binary-private; compile their actual modules in this integration target
// rather than publishing a new library API solely for tests.
#[macro_use]
#[path = "../src/json_out.rs"]
pub mod json_out;
#[path = "../src/android/mod.rs"]
pub mod android;
#[path = "../src/commands/mod.rs"]
pub mod commands;
#[path = "../src/manifest.rs"]
pub mod manifest;
#[cfg(all(windows, test))]
#[path = "../src/owner_only.rs"]
pub mod owner_only;
#[path = "../src/process.rs"]
pub mod process;

use commands::{assets_validate, build_profile, content};
use manifest::BuildProfile;
use regia_core::id::TypePathHash;

struct Fixture {
    root: PathBuf,
    cache: PathBuf,
    ids: [AssetId; 3],
}

fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

impl Fixture {
    fn new(unknown: bool, optional: bool, cycle: bool) -> Result<Self> {
        let root = regia_test_utils::unique_temp_dir("closure-integrity");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache)?;
        std::fs::create_dir_all(root.join("config"))?;
        atomic_write(&root.join("config/validation.ron"), "(cook: Warn)")?;
        atomic_write(
            &root.join("Regia.toml"),
            "[project]\nname = \"Integrity\"\nid = \"org.regia.integrity\"\n\
             [cook.chunk_options]\ncontent = \"closure\"\n\
             [cook.chunks.base]\nentry = [\"root.bin\"]\n",
        )?;
        // Descending IDs ensure diagnostics follow the selected root, not registry ID order.
        let ids = std::array::from_fn(|n| AssetId(regia_core::id::Guid::from_u128(3 - n as u128)));
        let paths = ["assets/root.bin", "assets/middle.bin", "assets/target.bin"];
        let deps = [
            vec![ids[1]],
            vec![ids[2]],
            if cycle { vec![ids[0]] } else { vec![] },
        ];
        let mut records = Vec::new();
        let mut manifest = AssetManifest::default();
        for i in 0..if unknown { 2 } else { 3 } {
            let type_path = "regia_asset::CookedData";
            let bytes = encode_cooked_file(
                &CookedHeader {
                    format_version: 1,
                    asset_id: ids[i],
                    type_tag: TypePathHash::of(type_path),
                    importer_version: 1,
                    dependencies: deps[i].clone(),
                    payload_len: 1,
                },
                &[42],
            )?;
            atomic_write(&cache.join(cooked_file_name(ids[i])), &bytes)?;
            let mut tags = Default::default();
            if optional && i == 1 {
                tags = [(OPTIONAL_REFERENCE_TAG.to_owned(), "target.bin".to_owned())].into();
            }
            records.push(RegistryEntry {
                id: ids[i],
                path: paths[i].into(),
                type_path: type_path.into(),
                dependencies: deps[i].clone(),
                tags,
                size: bytes.len() as u64,
            });
            manifest.entries.push(ManifestEntry {
                id: ids[i],
                path: paths[i].into(),
                type_path: type_path.into(),
            });
        }
        manifest.write_to_cache(&cache)?;
        AssetRegistry::from_entries(records).write_to(&cache_registry_path(&cache))?;
        Ok(Self { root, cache, ids })
    }

    fn spec(&self, exclude: bool) -> BuildProfile {
        BuildProfile {
            entry_levels: vec!["root.bin".into()],
            exclude: if exclude {
                vec!["target.bin".into()]
            } else {
                vec![]
            },
            ..Default::default()
        }
    }

    fn chunks(&self) -> Result<Vec<PathBuf>> {
        content::pack_project_chunks(
            &self.root,
            &self.cache,
            &self.root.join("chunks"),
            "linux",
            false,
            false,
        )
    }

    fn profile(&self, exclude: bool) -> Result<()> {
        let closure = build_profile::compute_closure(&self.root, "linux", Some(&self.spec(exclude)))?;
        build_profile::write_profile_pak(&self.root, "linux", &closure, &self.root.join("profile"), None)?;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn missing(error: anyhow::Error, detail: &str) {
    let text = format!("{error:#}");
    assert!(text.contains("content_dependency_missing"), "{text}");
    assert!(text.contains(detail), "{text}");
    assert!(text.contains("assets/root.bin"), "{text}");
    assert!(text.contains("assets/middle.bin"), "{text}");
}

#[test]
fn unknown_required_id_fails_chunk_shipping_even_with_warn() -> Result<()> {
    let f = Fixture::new(true, false, false)?;
    assets_validate::cook_validation_for(&f.root, "linux", true)?;
    missing(
        f.chunks().expect_err("unknown dependency must fail chunks"),
        "unknown_registry_id",
    );
    assert!(!f.root.join("chunks/base.fpak").exists());
    Ok(())
}

#[test]
fn unknown_required_id_fails_named_profile_even_with_warn() -> Result<()> {
    let f = Fixture::new(true, false, false)?;
    assets_validate::cook_validation_for(&f.root, "linux", true)?;
    missing(
        f.profile(false)
            .expect_err("unknown dependency must fail named profile"),
        "unknown_registry_id",
    );
    assert!(!f.root.join("profile/base.fpak").exists());
    Ok(())
}

#[test]
fn excluded_required_target_reports_the_root_to_edge_chain() -> Result<()> {
    let f = Fixture::new(false, false, false)?;
    missing(
        f.profile(true).expect_err("excluded required target"),
        "excluded_required_target",
    );
    Ok(())
}

#[test]
fn raw_level_selected_root_retains_its_required_edge_chain() -> Result<()> {
    let f = Fixture::new(false, false, false)?;
    // The shipped subgraph is cyclic, so its explicit raw-selected root cannot be inferred
    // from incoming edges. Keep the excluded target as the separate failing edge.
    let registry = content::project_registry(&f.cache)?;
    let records = registry.entries().cloned().map(|mut e| {
        if e.id == f.ids[1] {
            e.dependencies.push(f.ids[0]);
        }
        e
    });
    AssetRegistry::from_entries(records).write_to(&cache_registry_path(&f.cache))?;
    std::fs::create_dir_all(f.root.join("assets"))?;
    atomic_write(&f.root.join("assets/start.level.ron"), "(asset: \"root.bin\")")?;
    let mut spec = f.spec(true);
    spec.entry_levels = vec!["start.level.ron".into()];
    missing(
        build_profile::compute_closure(&f.root, "linux", Some(&spec))
            .expect_err("excluded dependency reached through a raw level"),
        "excluded_required_target",
    );
    Ok(())
}

#[test]
fn publication_recheck_retains_the_dependency_root_path() -> Result<()> {
    let f = Fixture::new(false, false, false)?;
    let closure = build_profile::compute_closure(&f.root, "linux", Some(&f.spec(false)))?;
    std::fs::remove_file(f.cache.join(cooked_file_name(f.ids[2])))?;
    missing(
        build_profile::write_profile_pak(&f.root, "linux", &closure, &f.root.join("profile"), None)
            .expect_err("missing dependency after closure computation"),
        "missing_cooked_file",
    );
    Ok(())
}

#[test]
fn declared_optional_exclusion_remains_legal() -> Result<()> {
    let f = Fixture::new(false, true, false)?;
    f.profile(true)?;
    f.chunks()?;
    Ok(())
}

#[test]
fn optional_excluded_target_does_not_require_a_cooked_file() -> Result<()> {
    let f = Fixture::new(false, true, false)?;
    std::fs::remove_file(f.cache.join(cooked_file_name(f.ids[2])))?;
    f.profile(true)?;
    Ok(())
}

#[test]
fn corrupt_registry_never_becomes_an_empty_success() -> Result<()> {
    let f = Fixture::new(false, false, false)?;
    atomic_write(&cache_registry_path(&f.cache), b"broken registry")?;
    for result in [f.chunks().map(|_| ()), f.profile(false)] {
        let text = format!("{:#}", result.expect_err("corrupt registry must fail shipping"));
        assert!(text.contains("content_registry_invalid"), "{text}");
    }
    Ok(())
}

#[test]
fn missing_cooked_target_is_distinct_from_unknown_registry_id() -> Result<()> {
    let f = Fixture::new(false, false, false)?;
    std::fs::remove_file(f.cache.join(cooked_file_name(f.ids[2])))?;
    missing(
        f.chunks().expect_err("missing cooked target"),
        "missing_cooked_file",
    );
    missing(
        f.profile(false).expect_err("missing cooked target"),
        "missing_cooked_file",
    );
    Ok(())
}

#[test]
fn dependency_cycles_ship_without_missing_dependency_diagnostics() -> Result<()> {
    let f = Fixture::new(false, false, true)?;
    f.chunks()?;
    f.profile(false)?;
    Ok(())
}

#[test]
fn graph_walk_stays_permissive_but_shipping_report_is_incomplete() -> Result<()> {
    use regia_asset_pipeline::chunks::{
        assign, check_assignment_integrity, check_references_with, optional_references,
    };
    let f = Fixture::new(true, false, true)?;
    let registry = AssetRegistry::read_from(&cache_registry_path(&f.cache))?.expect("fixture registry");
    let rules = content::project_rules(&f.root)?;
    let assignment = assign(&registry, &rules, &[])?;
    assert_eq!(assignment.chunk_of.len(), 2);
    let optional = optional_references(&registry);
    let report = check_assignment_integrity(&registry, &assignment, &rules, &optional, None);
    assert_eq!((report.visited_assets, report.examined_edges), (2, 2));
    assert_eq!(report.problems.len(), 1);
    assert_eq!(report.problems[0].detail, Some("unknown_registry_id"));
    assert_eq!(
        report.problems[0].root_path,
        ["assets/root.bin", "assets/middle.bin", &f.ids[2].to_string()]
    );
    assert_eq!(
        check_references_with(&registry, &assignment, &rules, &optional),
        report.problems
    );
    Ok(())
}

#[test]
fn an_excluded_assignment_requires_an_explicit_optional_tag() -> Result<()> {
    use regia_asset_pipeline::chunks::{assign, check_assignment_integrity, optional_references};
    for optional in [false, true] {
        let f = Fixture::new(false, optional, false)?;
        let registry = AssetRegistry::read_from(&cache_registry_path(&f.cache))?.expect("registry");
        let rules = content::project_rules(&f.root)?;
        let mut assignment = assign(&registry, &rules, &[])?;
        assignment.chunk_of.remove(&f.ids[2]);
        assignment.unshipped.insert(f.ids[2]);
        let report = check_assignment_integrity(
            &registry,
            &assignment,
            &rules,
            &optional_references(&registry),
            None,
        );
        assert_eq!(report.problems.is_empty(), optional);
        if let Some(p) = report.problems.first() {
            assert_eq!(p.detail, Some("excluded_required_target"));
            assert_eq!(
                p.root_path,
                ["assets/root.bin", "assets/middle.bin", "assets/target.bin"]
            );
        }
    }
    Ok(())
}

#[test]
fn cycles_and_shared_edges_have_bounded_work_in_either_registry_order() {
    use regia_asset_pipeline::chunks::check_dependency_integrity;
    use regia_core::id::Guid;
    const N: usize = 4096;
    let id = |n: usize| AssetId(Guid::from_u128(n as u128 + 1));
    let mut records: Vec<RegistryEntry> = (0..N)
        .map(|n| RegistryEntry {
            id: id(n),
            path: format!("assets/{n}.bin"),
            type_path: "regia_asset::CookedData".into(),
            dependencies: vec![id((n + 1) % N), id((n + 2) % N)],
            tags: Default::default(),
            size: 0,
        })
        .collect();
    let shipped = records.iter().map(|e| e.id).collect();
    for _ in 0..2 {
        let registry = AssetRegistry::from_entries(records.clone());
        let report = check_dependency_integrity(&registry, &[id(0)], &shipped, &Default::default(), None);
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert_eq!(report.visited_assets, N);
        assert_eq!(report.examined_edges, 2 * N);
        assert_eq!(report.tracked_assets, N);
        assert!(report.peak_queue <= N);
        records.reverse();
    }
}

#[test]
fn rejected_closures_leave_previous_paks_untouched() -> Result<()> {
    let f = Fixture::new(false, false, false)?;
    f.chunks()?;
    let closure = build_profile::compute_closure(&f.root, "linux", Some(&f.spec(false)))?;
    f.profile(false)?;
    let chunks = std::fs::read(f.root.join("chunks/base.fpak"))?;
    let profile = std::fs::read(f.root.join("profile/base.fpak"))?;
    std::fs::remove_file(f.cache.join(cooked_file_name(f.ids[2])))?;
    assert!(f.chunks().is_err());
    // Revalidate at publication even when the caller computed the closure before the loss.
    assert!(
        build_profile::write_profile_pak(&f.root, "linux", &closure, &f.root.join("profile"), None).is_err()
    );
    assert_eq!(std::fs::read(f.root.join("chunks/base.fpak"))?, chunks);
    assert_eq!(std::fs::read(f.root.join("profile/base.fpak"))?, profile);
    Ok(())
}

#[test]
fn chunk_json_exposes_codes_details_and_root_paths() -> Result<()> {
    let f = Fixture::new(true, false, false)?;
    let args = content::AssetsChunksArgs {
        project: f.root.clone(),
        platform: Some("linux".into()),
        json: true,
        max_size: None,
        max_total: None,
        measure: commands::assets_budget::Measure::Disk,
    };
    let report: serde_json::Value = serde_json::from_str(&content::chunks_report(&args)?)?;
    assert_eq!(report["complete"], false);
    assert_eq!(report["problems"][0]["reason"], "content_dependency_missing");
    assert_eq!(report["problems"][0]["detail"], "unknown_registry_id");
    assert_eq!(report["problems"][0]["root_path"][0], "assets/root.bin");
    atomic_write(&cache_registry_path(&f.cache), b"broken registry")?;
    let report: serde_json::Value = serde_json::from_str(&content::chunks_report(&args)?)?;
    assert_eq!(report["complete"], false);
    assert_eq!(report["problems"][0]["reason"], "content_registry_invalid");
    assert!(
        report["problems"][0]["root_path"][0]
            .as_str()
            .expect("path")
            .ends_with("asset_registry.bin")
    );
    Ok(())
}

#[test]
fn corrupt_registry_fails_even_when_advisory_validation_is_off() -> Result<()> {
    let f = Fixture::new(false, false, false)?;
    atomic_write(&f.root.join("config/validation.ron"), "(cook: Off)")?;
    atomic_write(&cache_registry_path(&f.cache), b"broken registry")?;
    let error =
        assets_validate::cook_validation_for(&f.root, "linux", true).expect_err("mandatory integrity");
    let report = json_out::failure("package_failed", &error.context("pak generation failed"));
    assert_eq!(report["error"]["code"], "content_registry_invalid");
    Ok(())
}

#[test]
fn package_diagnostics_survive_agent_job_reports_and_predicates() -> Result<()> {
    use regia_agent::cook_package::CliTally;
    use regia_agent::jobs::JobRegistry;
    use regia_agent::{AgentConfig, AgentPermissions, AgentPlugin, AgentSettings, LocalReply, call_local};
    use regia_app::prelude::*;
    use serde_json::{Value, json};

    let f = Fixture::new(true, false, false)?;
    let error = f
        .chunks()
        .expect_err("incomplete closure")
        .context("pak generation failed");
    let report = json_out::failure("package_failed", &error);
    let mut tally = CliTally::default();
    tally.feed(&report.to_string());
    let result = tally.result(Some(1), false, &["regia".into(), "package".into()]);
    assert_eq!(result["report"]["error"]["code"], "content_dependency_missing");
    assert_eq!(
        result["report"]["error"]["problems"][0]["root_path"][0],
        "assets/root.bin"
    );

    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ))
        .insert_resource(AgentSettings {
            project_dir: Some(f.root.clone()),
        })
        .add_plugins(AgentPlugin)
        .insert_resource(AgentPermissions {
            config: AgentConfig {
                rate_limit_per_second: u32::MAX,
                ..Default::default()
            },
            source: None,
        });
    app.finish_plugins();
    app.update();
    let (job, handle) = app
        .world_mut()
        .get_resource_or_insert_with::<JobRegistry>(Default::default)
        .start("package", "closure integrity fixture", false);
    handle.finish(result);
    let status = match call_local(app.world_mut(), "agent.job_status", json!({"job": job}), None) {
        LocalReply::Done(Ok(value)) => value,
        other => panic!("job status: {other:?}"),
    };
    assert_eq!(
        status["result"]["report"]["error"]["code"],
        "content_dependency_missing"
    );
    let frame = app.world().resource::<Time>().frame_count();
    app.world_mut()
        .resource_mut::<regia_agent::AgentEventLog>()
        .push_host(frame, "closure_integrity", status["result"]["report"].clone());
    let condition = json!({"all": [
        {"events_contain": {"kind": "closure_integrity", "contains": "content_dependency_missing"}},
        {"events_contain": {"kind": "closure_integrity", "contains": "assets/root.bin"}},
    ]});
    let pending = call_local(
        app.world_mut(),
        "play.wait_until",
        json!({"condition": condition, "timeout_ms": 10_000}),
        None,
    );
    // Fixed frame progress, never sleeps or a timing assertion. The condition is already true.
    for _ in 0..2 {
        app.update();
    }
    let response = match pending {
        LocalReply::Pending(receiver) => receiver.try_recv().expect("wait reply after fixed updates"),
        other => panic!("wait should defer: {other:?}"),
    };
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(
        response.result.as_ref().unwrap_or(&Value::Null)["satisfied"],
        true
    );
    match call_local(
        app.world_mut(),
        "play.expect",
        json!({"assertions": [{"name": "root and stable code", "condition": condition}]}),
        None,
    ) {
        LocalReply::Done(Ok(value)) => assert_eq!(value["pass"], true, "{value}"),
        other => panic!("expect: {other:?}"),
    }
    Ok(())
}

#[test]
fn absent_legacy_registry_still_checks_declared_header_edges() -> Result<()> {
    let f = Fixture::new(true, false, false)?;
    std::fs::remove_file(cache_registry_path(&f.cache))?;
    missing(
        f.chunks().expect_err("legacy header dependency"),
        "unknown_registry_id",
    );
    missing(
        f.profile(false).expect_err("legacy header dependency"),
        "unknown_registry_id",
    );
    Ok(())
}

#[test]
fn a_wildcard_optional_tag_cannot_hide_an_unknown_id() -> Result<()> {
    let f = Fixture::new(true, false, false)?;
    let registry = AssetRegistry::read_from(&cache_registry_path(&f.cache))?.expect("registry");
    let records = registry.entries().cloned().map(|mut entry| {
        entry.tags.insert(OPTIONAL_REFERENCE_TAG.into(), "*".into());
        entry
    });
    AssetRegistry::from_entries(records).write_to(&cache_registry_path(&f.cache))?;
    missing(
        f.chunks().expect_err("unknown cannot match optional path glob"),
        "unknown_registry_id",
    );
    missing(
        f.profile(false)
            .expect_err("unknown cannot match optional path glob"),
        "unknown_registry_id",
    );
    Ok(())
}

#[test]
fn unshipped_dangling_components_do_not_block_a_complete_closure() -> Result<()> {
    let f = Fixture::new(false, false, false)?;
    let registry = AssetRegistry::read_from(&cache_registry_path(&f.cache))?.expect("registry");
    let mut records: Vec<_> = registry.entries().cloned().collect();
    records.push(RegistryEntry {
        id: AssetId::new_random(),
        path: "assets/unshipped.bin".into(),
        type_path: "regia_asset::CookedData".into(),
        dependencies: vec![AssetId::new_random()],
        tags: Default::default(),
        size: 0,
    });
    AssetRegistry::from_entries(records).write_to(&cache_registry_path(&f.cache))?;
    f.chunks()?;
    f.profile(false)?;
    Ok(())
}
