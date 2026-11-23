// SPDX-License-Identifier: MPL-2.0
//! CT-25: chunk cooks and named build profiles resolve their roots through one shared resolver.
//!
//! The project is the `fixtures/closure_roots` sample (two levels, UI layouts, a spawn table in an
//! unknown format) plus cooked assets written here. The tests above `mod reports` use only APIs
//! that predate the resolver, so they show the missing behavior on the base; `mod reports` checks
//! the root and why reports the resolver adds.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use regia_asset::registry::{AssetRegistry, RegistryEntry, cache_registry_path};
use regia_asset::{
    AssetId, AssetManifest, CookedHeader, ManifestEntry, cooked_cache_dir, cooked_file_name,
    encode_cooked_file,
};
// The commands are binary-private; compile their actual modules in this integration target.
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

use commands::{build_profile, content};
use manifest::BuildProfile;
use regia_core::id::{Guid, TypePathHash};

/// Every cooked asset of the sample, in registry order.
const COOKED: &[&str] = &[
    "assets/meshes/a.glb",
    "assets/meshes/b.glb",
    "assets/meshes/fallback.glb",
    "assets/meshes/side.glb",
    "assets/textures/crate.png",
    "assets/textures/café.png",
    "assets/textures/hud.png",
    "assets/textures/secret.png",
    "assets/textures/dyn/spawned.png",
    "assets/textures/orphan.png",
    "assets/textures/detail.png",
    "assets/levels/a.procgen/rocks.bin",
    "assets/levels/b.procgen/trees.bin",
];

/// What the sample must ship from `levels/a.level.ron` plus the declared `textures/dyn/**`.
const REQUIRED: &[&str] = &[
    "assets/meshes/a.glb",
    "assets/meshes/b.glb",
    "assets/meshes/fallback.glb",
    "assets/textures/crate.png",
    "assets/textures/café.png",
    "assets/textures/hud.png",
    "assets/textures/dyn/spawned.png",
    "assets/textures/detail.png",
    "assets/levels/a.procgen/rocks.bin",
    "assets/levels/b.procgen/trees.bin",
];

fn id_of(path: &str) -> AssetId {
    let n = COOKED.iter().position(|p| *p == path).unwrap_or(usize::MAX - 1);
    AssetId(Guid::from_u128(0x5000 + n as u128))
}

fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp-write");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn sample_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/closure_roots/assets")
}

/// The sample's raw files as `(path under assets/, bytes)`, sorted.
fn sample_files() -> Result<Vec<(String, Vec<u8>)>> {
    let root = sample_dir();
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(&root)?.to_string_lossy().replace('\\', "/");
                out.push((rel, std::fs::read(&path)?));
            }
        }
    }
    out.sort();
    Ok(out)
}

struct Fixture {
    root: PathBuf,
    cache: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    /// The sample with `Regia.toml` = `toml`; `reversed` writes the files, the manifest and the
    /// registry in reverse order (same content).
    fn new(toml: &str, reversed: bool) -> Result<Self> {
        let root = regia_test_utils::unique_temp_dir("closure-root-parity");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache)?;
        atomic_write(&root.join("Regia.toml"), toml)?;
        let mut files = sample_files()?;
        let mut cooked: Vec<&str> = COOKED.to_vec();
        if reversed {
            files.reverse();
            cooked.reverse();
        }
        for (rel, bytes) in &files {
            atomic_write(&root.join("assets").join(rel), bytes)?;
        }
        let mut records = Vec::new();
        let mut manifest = AssetManifest::default();
        for path in cooked {
            let id = id_of(path);
            let dependencies = if path == "assets/meshes/a.glb" {
                vec![id_of("assets/textures/detail.png")]
            } else {
                Vec::new()
            };
            let type_path = "regia_asset::CookedData";
            let bytes = encode_cooked_file(
                &CookedHeader {
                    format_version: 1,
                    asset_id: id,
                    type_tag: TypePathHash::of(type_path),
                    importer_version: 1,
                    dependencies: dependencies.clone(),
                    payload_len: 1,
                },
                &[7],
            )?;
            atomic_write(&cache.join(cooked_file_name(id)), &bytes)?;
            records.push(RegistryEntry {
                id,
                path: path.into(),
                type_path: type_path.into(),
                dependencies,
                tags: Default::default(),
                size: bytes.len() as u64,
            });
            manifest.entries.push(ManifestEntry {
                id,
                path: path.into(),
                type_path: type_path.into(),
            });
        }
        manifest.write_to_cache(&cache)?;
        AssetRegistry::from_entries(records).write_to(&cache_registry_path(&cache))?;
        Ok(Self { root, cache })
    }

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) -> Result<()> {
        atomic_write(&self.root.join("assets").join(rel), bytes)
    }

    fn chunks_json(&self) -> Result<serde_json::Value> {
        let args = content::AssetsChunksArgs {
            project: self.root.clone(),
            platform: Some("linux".into()),
            json: true,
            max_size: None,
            max_total: None,
            measure: commands::assets_budget::Measure::Disk,
        };
        Ok(serde_json::from_str(&content::chunks_report(&args)?)?)
    }

    /// The cooked paths the chunk cook ships (every registry asset not listed as unshipped).
    fn chunk_shipped(&self) -> Result<BTreeSet<String>> {
        let report = self.chunks_json()?;
        let unshipped: BTreeSet<String> = report["unshipped"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        Ok(COOKED
            .iter()
            .map(|p| (*p).to_owned())
            .filter(|p| !unshipped.contains(p))
            .collect())
    }

    fn pack_chunks(&self) -> Result<Vec<PathBuf>> {
        content::pack_project_chunks(
            &self.root,
            &self.cache,
            &self.root.join("chunks"),
            "linux",
            false,
            false,
        )
    }

    fn profile(&self, include: &[&str]) -> Result<build_profile::Closure> {
        let spec = BuildProfile {
            entry_levels: vec!["levels/a.level.ron".into()],
            include: include.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        };
        build_profile::compute_closure(&self.root, "linux", Some(&spec))
    }

    fn profile_shipped(&self, include: &[&str]) -> Result<BTreeSet<String>> {
        Ok(self
            .profile(include)?
            .assets
            .into_iter()
            .map(|a| a.path)
            .collect())
    }

    fn why(&self, asset: &str) -> Result<serde_json::Value> {
        let args = content::AssetsWhyArgs {
            asset: asset.into(),
            project: self.root.clone(),
            platform: Some("linux".into()),
            json: true,
        };
        Ok(serde_json::from_str(&content::why_report(&args)?)?)
    }
}

const HEADER: &str =
    "[project]\nname = \"Roots\"\nid = \"org.regia.roots\"\nstartup_level = \"levels/a.level.ron\"\n";

/// Closure mode with `entry` globs on `base`.
fn closure_toml(entries: &[&str]) -> String {
    let list: Vec<String> = entries.iter().map(|e| format!("{e:?}")).collect();
    format!(
        "{HEADER}[cook.chunk_options]\ncontent = \"closure\"\n[cook.chunks.base]\nentry = [{}]\n",
        list.join(", ")
    )
}

fn set(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

#[test]
fn equivalent_profile_and_chunk_declarations_ship_the_same_cooked_ids() -> Result<()> {
    let f = Fixture::new(&closure_toml(&["textures/dyn/*.png"]), false)?;
    let chunks = f.chunk_shipped()?;
    let profile = f.profile_shipped(&["textures/dyn/**"])?;
    assert_eq!(chunks, profile, "chunk cook and named profile disagree");
    assert_eq!(profile, set(REQUIRED));
    Ok(())
}

#[test]
fn escaped_and_unicode_references_resolve() -> Result<()> {
    let f = Fixture::new(&closure_toml(&["textures/dyn/*.png"]), false)?;
    let chunks = f.chunk_shipped()?;
    let profile = f.profile_shipped(&["textures/dyn/**"])?;
    for path in ["assets/textures/crate.png", "assets/textures/café.png"] {
        assert!(chunks.contains(path), "chunk cook misses {path}");
        assert!(profile.contains(path), "profile misses {path}");
    }
    Ok(())
}

#[test]
fn typed_non_reference_strings_create_no_dependency() -> Result<()> {
    let f = Fixture::new(&closure_toml(&["textures/dyn/*.png"]), false)?;
    for (side, shipped) in [
        ("chunk cook", f.chunk_shipped()?),
        ("profile", f.profile_shipped(&["textures/dyn/**"])?),
    ] {
        assert!(
            !shipped.contains("assets/textures/secret.png"),
            "{side}: a UI node name is not a dependency"
        );
        assert!(!shipped.contains("assets/textures/orphan.png"), "{side}");
    }
    Ok(())
}

#[test]
fn startup_ui_and_procedural_bakes_stay_included() -> Result<()> {
    let f = Fixture::new(&closure_toml(&["textures/dyn/*.png"]), false)?;
    for (side, shipped) in [
        ("chunk cook", f.chunk_shipped()?),
        ("profile", f.profile_shipped(&["textures/dyn/**"])?),
    ] {
        for path in [
            "assets/textures/hud.png",
            "assets/levels/a.procgen/rocks.bin",
            "assets/levels/b.procgen/trees.bin",
        ] {
            assert!(shipped.contains(path), "{side} misses {path}");
        }
    }
    // With every asset shipping, the boot set is pinned in base and a travel target's bake is not.
    let toml = format!(
        "{HEADER}[cook.chunks.extra]\ninclude = [\"textures/hud.png\", \"levels/**\", \"meshes/b.glb\"]\n\
         delivery = \"on_demand\"\n"
    );
    let f = Fixture::new(&toml, false)?;
    assert_eq!(
        f.why("assets/textures/hud.png")?["chunk"],
        "base",
        "startup UI picture"
    );
    assert_eq!(
        f.why("assets/levels/a.procgen/rocks.bin")?["chunk"],
        "base",
        "startup bake"
    );
    assert_eq!(
        f.why("assets/levels/b.procgen/trees.bin")?["chunk"],
        "extra",
        "travel bake"
    );
    assert_eq!(f.why("assets/meshes/b.glb")?["chunk"], "extra", "travel mesh");
    Ok(())
}

#[test]
fn unreadable_required_raw_data_fails() -> Result<()> {
    let broken: [(&str, &[u8]); 3] = [
        (
            "levels/a.level.ron",
            b"(entities: [(components: {\"m\": (path: \"meshes/a.glb)})])",
        ),
        ("levels/b.level.ron", b"(entities: [\xff\xfe])"),
        (
            "ui/hud.ui.ron",
            b"(version: 9, root: (children: [(image: (asset: 7))]))",
        ),
    ];
    for (rel, bytes) in broken {
        let f = Fixture::new(&closure_toml(&["textures/dyn/*.png"]), false)?;
        f.write(rel, bytes)?;
        let chunks = f.pack_chunks();
        let profile = f.profile(&["textures/dyn/**"]);
        for (side, result) in [
            ("chunk cook", chunks.map(|_| ())),
            ("profile", profile.map(|_| ())),
        ] {
            let text = format!("{:#}", result.expect_err(rel));
            assert!(text.contains("content_root_malformed"), "{side} {rel}: {text}");
            assert!(text.contains(rel), "{side} {rel}: {text}");
        }
        assert!(
            !f.root.join("chunks/base.fpak").exists(),
            "{rel}: nothing published"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new(&closure_toml(&["textures/dyn/*.png"]), false)?;
        let level = f.root.join("assets/levels/b.level.ron");
        std::fs::set_permissions(&level, std::fs::Permissions::from_mode(0o000))?;
        // A privileged user reads it anyway; only an actually unreadable file is the case here.
        if std::fs::read(&level).is_err() {
            for result in [f.pack_chunks().map(|_| ()), f.profile(&[]).map(|_| ())] {
                let text = format!("{:#}", result.expect_err("unreadable level"));
                assert!(text.contains("content_root_unreadable"), "{text}");
            }
        }
        std::fs::set_permissions(&level, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

#[test]
fn dynamic_roots_include_declared_content() -> Result<()> {
    // Undeclared: the level-select level and its mesh do not ship.
    let f = Fixture::new(&closure_toml(&["textures/dyn/*.png"]), false)?;
    assert!(!f.chunk_shipped()?.contains("assets/meshes/side.glb"));
    assert!(
        !f.profile_shipped(&["textures/dyn/**"])?
            .contains("assets/meshes/side.glb")
    );
    // Declared through the existing `entry` / `include` settings, both ship the same content.
    let f = Fixture::new(
        &closure_toml(&["textures/dyn/*.png", "levels/side.level.ron"]),
        false,
    )?;
    let chunks = f.chunk_shipped()?;
    let profile = f.profile_shipped(&["textures/dyn/**", "levels/side.level.ron"])?;
    assert!(chunks.contains("assets/meshes/side.glb"), "{chunks:?}");
    assert!(chunks.contains("assets/textures/dyn/spawned.png"));
    assert_eq!(chunks, profile);
    let closure = f.profile(&["textures/dyn/**", "levels/side.level.ron"])?;
    assert!(closure.raw.iter().any(|r| r.path == "levels/side.level.ron"));
    Ok(())
}

mod reports {
    use super::*;
    use regia_asset_pipeline::closure_roots::{RootDecl, RootInputs, RootKind, RootReport, resolve_roots};

    fn record<'a>(report: &'a serde_json::Value, target: &str) -> &'a serde_json::Value {
        report["roots"]["records"]
            .as_array()
            .and_then(|r| r.iter().find(|r| r["target"] == target))
            .unwrap_or(&serde_json::Value::Null)
    }

    #[test]
    fn every_root_records_why_it_exists() -> Result<()> {
        let f = Fixture::new(
            &closure_toml(&["textures/dyn/*.png", "levels/side.level.ron"]),
            false,
        )?;
        let report = f.chunks_json()?;
        let spawned = record(&report, "assets/textures/dyn/spawned.png");
        assert_eq!(spawned["kind"], "declared");
        assert_eq!(spawned["declared_by"], "[cook.chunks.base] entry[0]");
        assert_eq!(spawned["boot"], false);
        let side = record(&report, "assets/meshes/side.glb");
        assert_eq!(side["kind"], "reference");
        assert_eq!(side["declared_by"], "[cook.chunks.base] entry[1]");
        assert_eq!(side["via"], "levels/side.level.ron");
        let hud = record(&report, "assets/textures/hud.png");
        assert_eq!(
            (hud["kind"].as_str(), hud["boot"].as_bool()),
            (Some("reference"), Some(true))
        );
        assert_eq!(hud["via"], "ui/hud.ui.ron");
        let rocks = record(&report, "assets/levels/a.procgen/rocks.bin");
        assert_eq!(rocks["kind"], "level_bake");
        assert_eq!(rocks["declared_by"], "[project] startup_level");
        let trees = record(&report, "assets/levels/b.procgen/trees.bin");
        assert_eq!(
            (trees["kind"].as_str(), trees["boot"].as_bool()),
            (Some("level_bake"), Some(false))
        );
        assert!(record(&report, "assets/textures/secret.png").is_null());
        // Coverage: the spawn table had only the fallback scan, so it is not complete.
        assert_eq!(report["roots"]["coverage"]["fallback"][0], "config/spawn.table");
        assert_eq!(report["roots"]["coverage"]["complete"], false);
        assert_eq!(report["roots"]["coverage"]["typed"], 5);

        let why = f.why("assets/textures/dyn/spawned.png")?;
        assert_eq!(why["root"]["declared_by"], "[cook.chunks.base] entry[0]");

        let closure = f.profile(&["textures/dyn/**", "levels/side.level.ron"])?;
        let text = build_profile::roots_report(&closure);
        assert!(
            text.contains("declared     assets/textures/dyn/spawned.png  <- [profiles] include[0]"),
            "{text}"
        );
        assert!(
            text.contains("fallback scan: assets/config/spawn.table"),
            "{text}"
        );
        assert!(text.contains("(incomplete"), "{text}");
        Ok(())
    }

    #[test]
    fn shuffled_registry_and_file_order_yield_identical_reports() -> Result<()> {
        let toml = closure_toml(&["textures/dyn/*.png", "levels/side.level.ron"]);
        let reports = [false, true].map(|reversed| -> Result<_> {
            let f = Fixture::new(&toml, reversed)?;
            let chunks = f.chunks_json()?;
            let whys = COOKED.iter().map(|p| f.why(p)).collect::<Result<Vec<_>>>()?;
            let closure = f.profile(&["textures/dyn/**", "levels/side.level.ron"])?;
            Ok((
                chunks["roots"].clone(),
                whys,
                build_profile::roots_report(&closure),
            ))
        });
        let [a, b] = reports;
        let (a, b) = (a?, b?);
        assert_eq!(a.0, b.0, "chunk root reports");
        assert_eq!(a.1, b.1, "why reports");
        assert_eq!(a.2, b.2, "profile root reports");
        Ok(())
    }

    #[test]
    fn graph_visit_counters_are_bounded() -> Result<()> {
        const N: usize = 256;
        let root = regia_test_utils::unique_temp_dir("closure-root-counters");
        let assets = root.join("assets");
        let mut raw = std::collections::BTreeMap::new();
        let mut cooked = Vec::new();
        for i in 0..N {
            // Each level names the next, the previous, itself and its mesh: a dense cycle.
            let text = format!(
                "(next: \"levels/l{}.level.ron\", back: \"levels/l{}.level.ron\", me: \"levels/l{i}.level.ron\", \
                 mesh: \"meshes/m{i}.glb\")",
                (i + 1) % N,
                (i + N - 1) % N
            );
            atomic_write(&assets.join(format!("levels/l{i}.level.ron")), &text)?;
            raw.insert(format!("levels/l{i}.level.ron"), text.len() as u64);
            cooked.push((
                AssetId(Guid::from_u128(i as u128 + 1)),
                format!("assets/meshes/m{i}.glb"),
            ));
        }
        let decls = vec![
            RootDecl::new(RootKind::EntryLevel, "a", "levels/l0.level.ron"),
            RootDecl::new(RootKind::Declared, "b", "levels/l7*.level.ron"),
        ];
        let mut seen: Option<RootReport> = None;
        for _ in 0..2 {
            let inputs = RootInputs {
                assets_dir: assets.clone(),
                raw: &raw,
                cooked: &cooked,
                exclude: &|_| false,
            };
            let ev = resolve_roots(&inputs, &decls, true)?;
            assert_eq!(ev.counters.files_read, N, "each level is read once");
            assert_eq!(ev.counters.strings_examined, 4 * N);
            assert_eq!(ev.counters.references, 4 * N);
            assert!(ev.counters.peak_queue <= N);
            assert_eq!(ev.cooked.len(), N);
            let report = ev.report(&|id| id.to_string());
            if let Some(seen) = &seen {
                assert_eq!(seen, &report, "declaration and registry order do not matter");
            }
            seen = Some(report);
            cooked.reverse();
        }
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn root_reports_reach_agent_predicates() -> Result<()> {
        use regia_agent::{
            AgentConfig, AgentPermissions, AgentPlugin, AgentSettings, LocalReply, call_local,
        };
        use regia_app::prelude::*;
        use serde_json::{Value, json};

        let f = Fixture::new(&closure_toml(&["textures/dyn/*.png"]), false)?;
        let report = f.chunks_json()?;
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
        let frame = app.world().resource::<Time>().frame_count();
        app.world_mut()
            .resource_mut::<regia_agent::AgentEventLog>()
            .push_host(frame, "closure_roots", report["roots"].clone());
        let condition = json!({"all": [
            {"events_contain": {"kind": "closure_roots", "contains": "[cook.chunks.base] entry[0]"}},
            {"events_contain": {"kind": "closure_roots", "contains": "config/spawn.table"}},
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
            json!({"assertions": [{"name": "declared root and fallback coverage", "condition": condition}]}),
            None,
        ) {
            LocalReply::Done(Ok(value)) => assert_eq!(value["pass"], true, "{value}"),
            other => panic!("expect: {other:?}"),
        }
        Ok(())
    }
}
