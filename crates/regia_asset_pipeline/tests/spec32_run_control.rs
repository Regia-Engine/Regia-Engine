// SPDX-License-Identifier: MPL-2.0
//! Spec 32 § 10 acceptance items 1-5 at the pipeline level (conformance audit, ADR-5632): the run
//! control of `ValidatorRegistry::run_with` (scopes, the cap, exclusions), the verdict contract
//! (`NotValidated`, severities, `warnings_fail`, a `Pass` carrying an `Error`, a `Fail` without a
//! finding), panic isolation, determinism, and the registry bridge's sub-asset sizes in a folder
//! group. The agent tests in `regia_editor::asset_validate_agent` cover the same rules through the
//! protocol; these pin the library the CLI, the editor and CI share. Synthetic sets, no disk.

use std::collections::BTreeMap;

use regia_asset::AssetId;
use regia_asset::registry::{AssetRegistry, RegistryEntry};
use regia_asset_pipeline::asset_audit::{GroupBy, SizeMapQuery, size_map};
use regia_asset_pipeline::asset_graph::{AssetEntry, AssetKind, AssetSet, DependencyGraph};
use regia_asset_pipeline::validators::{
    Finding, Outcome, RunSettings, Scope, Severity, Trigger, ValidationConfig, ValidationContext,
    ValidationReport, Validator, ValidatorRegistry, Verdict,
};

/// Passes every asset and records nothing: the rows show which assets a run gathered.
struct Touch;

impl Validator for Touch {
    fn name(&self) -> &str {
        "touch"
    }

    fn description(&self) -> &str {
        "passes every asset"
    }

    fn validate(&self, _ctx: &ValidationContext<'_>, _asset: &AssetEntry) -> Verdict {
        Verdict::pass()
    }
}

/// A registry holding only `validator`.
fn only(validator: Box<dyn Validator>) -> ValidatorRegistry {
    let mut registry = ValidatorRegistry::new();
    registry.register(validator).expect("a valid, unique name");
    registry
}

/// The assets a report has rows for, sorted and deduplicated.
fn assets(report: &ValidationReport) -> Vec<&str> {
    let mut seen: Vec<&str> = report.results.iter().map(|r| r.asset.as_str()).collect();
    seen.dedup();
    seen
}

fn settings(scope: Scope) -> RunSettings {
    RunSettings {
        scope,
        ..RunSettings::default()
    }
}

/// A small project: a level using two textures, one under a subfolder, a dev texture, and a
/// three-asset reference cycle.
fn project() -> AssetSet {
    let mut set = AssetSet::new();
    set.push(
        AssetEntry::new("assets/levels/a.scene", AssetKind::Scene)
            .with_references(["assets/tex/rock.png", "assets/tex/deep/moss.png"]),
    );
    set.push(AssetEntry::new("assets/tex/rock.png", AssetKind::Texture).with_id("id-rock"));
    set.push(AssetEntry::new("assets/tex/deep/moss.png", AssetKind::Texture));
    set.push(AssetEntry::new("assets/dev/x.png", AssetKind::Texture));
    set.push(
        AssetEntry::new("assets/loop/a.mat", AssetKind::Material).with_references(["assets/loop/b.mat"]),
    );
    set.push(
        AssetEntry::new("assets/loop/b.mat", AssetKind::Material).with_references(["assets/loop/c.mat"]),
    );
    set.push(
        AssetEntry::new("assets/loop/c.mat", AssetKind::Material).with_references(["assets/loop/a.mat"]),
    );
    set
}

/// Acceptance 2: a folder scope honors `recursive`, the project scope takes every asset, and the
/// run reports what it requested and checked.
#[test]
fn folder_scope_respects_recursive_and_project_takes_everything() {
    let registry = only(Box::new(Touch));
    let set = project();
    let flat = registry.run_with(
        &set,
        &settings(Scope::Folder {
            path: "assets/tex".into(),
            recursive: false,
        }),
    );
    assert_eq!(assets(&flat), ["assets/tex/rock.png"]);
    let deep = registry.run_with(
        &set,
        &settings(Scope::Folder {
            path: "assets/tex/".into(),
            recursive: true,
        }),
    );
    assert_eq!(assets(&deep), ["assets/tex/deep/moss.png", "assets/tex/rock.png"]);
    assert_eq!((deep.summary.requested, deep.summary.checked), (2, 2));
    // A folder name that is only a prefix of another folder does not match it.
    let prefix = registry.run_with(
        &set,
        &settings(Scope::Folder {
            path: "assets/te".into(),
            recursive: true,
        }),
    );
    assert!(prefix.results.is_empty());

    let all = registry.run_with(&set, &settings(Scope::Project));
    assert_eq!(all.summary.checked, set.assets.len());
    assert_eq!(all.summary.requested, set.assets.len());
    assert!(!all.summary.cap_reached);
}

/// Acceptance 2: `WithDependencies` adds the transitive dependencies, terminates on a cycle and
/// lists each asset once; names may be ids.
#[test]
fn with_dependencies_follows_references_and_terminates_on_a_cycle() {
    let registry = only(Box::new(Touch));
    let set = project();
    let level = registry.run_with(
        &set,
        &settings(Scope::WithDependencies(vec!["assets/levels/a.scene".into()])),
    );
    assert_eq!(
        assets(&level),
        [
            "assets/levels/a.scene",
            "assets/tex/deep/moss.png",
            "assets/tex/rock.png"
        ]
    );
    let cycle = registry.run_with(
        &set,
        &settings(Scope::WithDependencies(vec!["assets/loop/b.mat".into()])),
    );
    assert_eq!(
        assets(&cycle),
        ["assets/loop/a.mat", "assets/loop/b.mat", "assets/loop/c.mat"]
    );
    assert_eq!(cycle.results.len(), 3, "each asset once");
    let by_id = registry.run_with(&set, &settings(Scope::Assets(vec!["id-rock".into()])));
    assert_eq!(assets(&by_id), ["assets/tex/rock.png"]);
}

/// Acceptance 2: a `Changed` path that no longer exists pulls in every asset still referencing
/// it, and `missing_references` fails those; a deleted path is not an unknown-asset row (the
/// caller's VCS diff names deletions on purpose).
#[test]
fn changed_with_a_deleted_path_pulls_in_its_referrers() {
    let registry = ValidatorRegistry::with_builtins(&ValidationConfig::default());
    let mut set = project();
    // The level still references a texture the change deleted.
    set.assets.retain(|a| a.path != "assets/tex/rock.png");
    let report = registry.run_with(
        &set,
        &RunSettings {
            scope: Scope::Changed(vec!["assets/tex/rock.png".into(), "assets/dev/x.png".into()]),
            trigger: Trigger::Save,
            only: Some(vec!["missing_references".into()]),
            ..RunSettings::default()
        },
    );
    assert_eq!(assets(&report), ["assets/dev/x.png", "assets/levels/a.scene"]);
    let level = report
        .results
        .iter()
        .find(|r| r.asset == "assets/levels/a.scene")
        .expect("the referrer is checked");
    assert_eq!(level.outcome, Outcome::Fail);
    assert!(
        level
            .findings
            .iter()
            .any(|f| f.code == "validate_reference_missing"),
        "{:?}",
        level.findings
    );
    assert!(
        report
            .all_findings()
            .all(|(_, f)| f.code != "validate_unknown_asset"),
        "a deletion is expected in a changed list"
    );
}

/// Acceptance 2: a named path or id the set cannot resolve counts in `requested` and yields a
/// run-level `validation` row with `validate_unknown_asset`; the run still succeeds and checks
/// the rest.
#[test]
fn an_unknown_scope_name_is_a_finding_and_the_run_continues() {
    let registry = only(Box::new(Touch));
    let set = project();
    let report = registry.run_with(
        &set,
        &settings(Scope::Assets(vec![
            "assets/tex/rock.png".into(),
            "assets/nope.png".into(),
        ])),
    );
    assert_eq!(report.summary.requested, 2);
    assert_eq!(report.summary.checked, 1);
    let row = report
        .results
        .iter()
        .find(|r| r.asset == "assets/nope.png")
        .expect("a run-level row");
    assert_eq!(row.validator, "validation");
    assert_eq!(row.findings.len(), 1);
    assert_eq!(row.findings[0].code, "validate_unknown_asset");
    assert!(
        report
            .results
            .iter()
            .any(|r| r.asset == "assets/tex/rock.png" && r.outcome == Outcome::Pass)
    );
}

/// Acceptance 3: `max_assets` cuts the path-sorted list and sets `cap_reached`; exclusions apply
/// to gathered assets, never to named ones.
#[test]
fn the_cap_cuts_the_sorted_list_and_only_gathered_assets_are_excluded() {
    let registry = only(Box::new(Touch));
    let set = project();
    let capped = registry.run_with(
        &set,
        &RunSettings {
            max_assets: Some(3),
            ..RunSettings::default()
        },
    );
    assert_eq!(
        assets(&capped),
        ["assets/dev/x.png", "assets/levels/a.scene", "assets/loop/a.mat"]
    );
    assert!(capped.summary.cap_reached);
    assert_eq!(capped.summary.requested, set.assets.len());
    assert_eq!(capped.summary.checked, 3);
    let exact = registry.run_with(
        &set,
        &RunSettings {
            max_assets: Some(set.assets.len()),
            ..RunSettings::default()
        },
    );
    assert!(
        !exact.summary.cap_reached,
        "a cap equal to the count cuts nothing"
    );

    let excluded = registry.run_with(
        &set,
        &RunSettings {
            exclude: vec!["assets/dev/**".into(), "assets/loop/**".into()],
            ..RunSettings::default()
        },
    );
    assert_eq!(excluded.summary.skipped_excluded, 4);
    assert!(assets(&excluded).iter().all(|p| !p.starts_with("assets/dev/")));
    // Named directly, an excluded path still runs; its gathered dependencies honor the exclusion.
    let named = registry.run_with(
        &set,
        &RunSettings {
            scope: Scope::WithDependencies(vec!["assets/loop/a.mat".into(), "assets/levels/a.scene".into()]),
            exclude: vec!["assets/loop/**".into(), "assets/tex/deep/**".into()],
            ..RunSettings::default()
        },
    );
    assert_eq!(
        assets(&named),
        [
            "assets/levels/a.scene",
            "assets/loop/a.mat",
            "assets/tex/rock.png"
        ]
    );
    assert_eq!(named.summary.skipped_excluded, 3);
}

/// Answers by path: the verdict-contract cases of acceptance 4.
struct Contract;

impl Validator for Contract {
    fn name(&self) -> &str {
        "contract"
    }

    fn description(&self) -> &str {
        "returns a fixed verdict per asset"
    }

    fn validate(&self, _ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        match asset.path.as_str() {
            "skip" => Verdict::not_validated("not my kind"),
            "warn" => Verdict::pass_with(vec![Finding::new("w_code", Severity::Warning, "careful")]),
            "info" => Verdict::pass_with(vec![Finding::new("i_code", Severity::Info, "noted")]),
            "pass_error" => Verdict::pass_with(vec![Finding::error("e_code", "a pass that is not")]),
            "bare_fail" => Verdict {
                outcome: Outcome::Fail,
                messages: Vec::new(),
                findings: Vec::new(),
            },
            "skip_with_findings" => Verdict {
                outcome: Outcome::NotValidated,
                messages: vec!["skipped".into()],
                findings: vec![Finding::error("ignored", "dropped by the runner")],
            },
            "legacy" => Verdict::fail(vec!["old style".into()]),
            _ => Verdict::pass(),
        }
    }
}

fn contract_set() -> AssetSet {
    let mut set = AssetSet::new();
    for path in [
        "skip",
        "warn",
        "info",
        "pass_error",
        "bare_fail",
        "skip_with_findings",
        "legacy",
        "ok",
    ] {
        set.push(AssetEntry::new(path, AssetKind::Other));
    }
    set
}

/// Acceptance 4: `NotValidated` never fails and records no findings; an `Error` fails, a `Warning`
/// does not unless `warnings_fail`; `exit_failed(strict)` honors both; a `Pass` carrying an
/// `Error` becomes `Fail`; a `Fail` without an `Error` gets `validate_no_finding`; `Verdict::fail`
/// findings take the validator's name as their code.
#[test]
fn the_verdict_contract_and_severities() {
    let registry = only(Box::new(Contract));
    let set = contract_set();
    let report = registry.run(&set);
    let row = |path: &str| {
        report
            .results
            .iter()
            .find(|r| r.asset == path)
            .unwrap_or_else(|| panic!("a row for {path}"))
    };
    assert_eq!(row("skip").outcome, Outcome::NotValidated);
    assert_eq!(row("skip_with_findings").outcome, Outcome::NotValidated);
    assert!(
        row("skip_with_findings").findings.is_empty(),
        "NotValidated records no findings"
    );
    assert_eq!(row("warn").outcome, Outcome::Pass);
    assert_eq!(row("info").outcome, Outcome::Pass);
    assert_eq!(
        row("pass_error").outcome,
        Outcome::Fail,
        "a Pass with an Error is a Fail"
    );
    let bare = row("bare_fail");
    assert_eq!(bare.outcome, Outcome::Fail);
    assert_eq!(bare.findings.len(), 1);
    assert_eq!(bare.findings[0].code, "validate_no_finding");
    assert_eq!(bare.findings[0].severity, Severity::Error);
    let legacy = row("legacy");
    assert_eq!(legacy.findings[0].code, "contract");
    assert_eq!(legacy.messages, vec!["old style"]);
    assert_eq!(row("ok").outcome, Outcome::Pass);
    assert_eq!(report.summary.failed, 3);
    assert_eq!(report.summary.not_validated, 2);
    assert_eq!(report.summary.warnings, 1);
    assert!(report.exit_failed(false));

    // Without the failing assets: a warning fails only a strict run.
    let mut calm = AssetSet::new();
    for path in ["skip", "warn", "info", "ok"] {
        calm.push(AssetEntry::new(path, AssetKind::Other));
    }
    let lenient = registry.run(&calm);
    assert_eq!(lenient.summary.failed, 0);
    assert!(!lenient.exit_failed(false));
    assert!(lenient.exit_failed(true));
    let strict = registry.run_with(
        &calm,
        &RunSettings {
            warnings_fail: true,
            ..RunSettings::default()
        },
    );
    assert_eq!(
        strict.summary.failed, 1,
        "warnings_fail fails the warned asset only"
    );
    assert_eq!(
        strict
            .results
            .iter()
            .find(|r| r.outcome == Outcome::Fail)
            .map(|r| r.asset.as_str()),
        Some("warn")
    );
}

/// Panics on one asset.
struct Explodes;

impl Validator for Explodes {
    fn name(&self) -> &str {
        "explodes"
    }

    fn description(&self) -> &str {
        "panics on the bomb"
    }

    fn validate(&self, _ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        assert!(asset.path != "bomb", "validator bug on purpose");
        Verdict::pass()
    }
}

/// Acceptance 5: a panicking validator yields `validate_validator_panicked` on that asset, its
/// other assets and every other validator's rows are intact, and the run completes.
#[test]
fn a_panicking_validator_is_isolated() {
    let mut registry = ValidatorRegistry::new();
    registry.register(Box::new(Touch)).expect("register");
    registry.register(Box::new(Explodes)).expect("register");
    let mut set = AssetSet::new();
    for path in ["a", "bomb", "c"] {
        set.push(AssetEntry::new(path, AssetKind::Other));
    }
    let report = registry.run(&set);
    assert_eq!(report.results.len(), 6, "two validators over three assets");
    let broken: Vec<_> = report.failures().collect();
    assert_eq!(broken.len(), 1);
    assert_eq!(
        (broken[0].asset.as_str(), broken[0].validator.as_str()),
        ("bomb", "explodes")
    );
    assert_eq!(broken[0].findings[0].code, "validate_validator_panicked");
    assert_eq!(broken[0].findings[0].severity, Severity::Error);
    assert_eq!(report.summary.passed, 5);
}

/// Spec 32 § 4.3 and § 9: output order is (asset path, validator name) and two runs over the
/// same input give byte-identical JSON, whatever order the set lists its assets in.
#[test]
fn reports_are_ordered_and_byte_identical() {
    let registry = ValidatorRegistry::with_builtins(&ValidationConfig::default());
    let set = project();
    let mut reversed = project();
    reversed.assets.reverse();
    let one = registry.run(&set).to_json().expect("json");
    let two = registry.run(&set).to_json().expect("json");
    let three = registry.run(&reversed).to_json().expect("json");
    assert_eq!(one, two);
    assert_eq!(one, three, "set order does not reach the report");
    let report = registry.run(&set);
    let keys: Vec<(&str, &str)> = report
        .results
        .iter()
        .map(|r| (r.asset.as_str(), r.validator.as_str()))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted);
}

fn entry(n: u128, path: &str, ty: &str, deps: &[AssetId], size: u64) -> RegistryEntry {
    RegistryEntry {
        id: AssetId(regia_core::id::Guid::from_u128(n)),
        path: path.to_owned(),
        type_path: ty.to_owned(),
        dependencies: deps.to_vec(),
        tags: BTreeMap::new(),
        size,
    }
}

/// Acceptance 1: a sub-asset (`file#fragment`) is its own entry with the registry's size 0, so a
/// folder group of the parent file counts the file's bytes once.
#[test]
fn bridge_sub_assets_are_not_double_counted_in_a_folder_group() {
    let file = entry(2, "meshes/rock.glb", "x::CookedMesh", &[], 300);
    let sub = entry(3, "meshes/rock.glb#mesh0", "x::CookedMesh", &[file.id], 0);
    let scene = entry(1, "levels/a.scene", "x::SceneAsset", &[sub.id, file.id], 10);
    let set = AssetSet::from_registry(&AssetRegistry::from_entries([scene, file, sub]));
    assert_eq!(set.assets.len(), 3, "the sub-asset stays a separate entry");
    assert_eq!(set.get("meshes/rock.glb#mesh0").map(|a| a.cooked_bytes), Some(0));
    let graph = DependencyGraph::build(&set);
    let map = size_map(
        &set,
        &graph,
        &SizeMapQuery {
            roots: vec!["levels/a.scene".into()],
            group_by: GroupBy::Folder(1),
            ..SizeMapQuery::default()
        },
    )
    .expect("size map");
    assert_eq!(map.total_bytes, 310);
    let meshes = map
        .rows
        .iter()
        .find(|r| r.name == "meshes")
        .expect("a meshes row");
    assert_eq!((meshes.assets, meshes.bytes), (2, 300));
    assert_eq!(map.rows.iter().map(|r| r.bytes).sum::<u64>(), map.total_bytes);
}

/// Spec 32 § 4.4: the ADR-1568 built-ins report their failures with the stable codes
/// `validate_name_invalid`, `validate_reference_missing`, `validate_id_duplicate` and
/// `validate_asset_orphaned` at `Error` severity with outcome `Fail`; the two budget built-ins
/// keep their validator name as the code.
#[test]
fn builtin_failures_carry_their_spec_codes() {
    let config = ValidationConfig {
        max_texture_dimension: 64,
        mesh_triangle_budget: 10,
        ..ValidationConfig::default()
    };
    let registry = ValidatorRegistry::with_builtins(&config);
    let mut set = AssetSet::new();
    set.roots = vec!["levels/main.scene".into()];
    set.push(
        AssetEntry::new("levels/main.scene", AssetKind::Scene).with_references([
            "tex/a.png",
            "tex/b.png",
            "tex/gone.png",
            "meshes/big.mesh",
            "tex/Bad Name.png",
        ]),
    );
    set.push(
        AssetEntry::new("tex/a.png", AssetKind::Texture)
            .with_id("same")
            .with_texture_size(128, 128),
    );
    set.push(AssetEntry::new("tex/b.png", AssetKind::Texture).with_id("same"));
    set.push(AssetEntry::new("tex/Bad Name.png", AssetKind::Texture));
    set.push(AssetEntry::new("meshes/big.mesh", AssetKind::Mesh).with_triangles(11));
    set.push(AssetEntry::new("tex/loose.png", AssetKind::Texture));
    let report = registry.run(&set);
    let code_of = |validator: &str, asset: &str| {
        let row = report
            .results
            .iter()
            .find(|r| r.validator == validator && r.asset == asset)
            .unwrap_or_else(|| panic!("a {validator} row for {asset}"));
        assert_eq!(row.outcome, Outcome::Fail, "{validator} {asset}");
        assert!(row.findings.iter().all(|f| f.severity == Severity::Error));
        row.findings.first().map(|f| f.code.clone()).unwrap_or_default()
    };
    assert_eq!(
        code_of("naming_rules", "tex/Bad Name.png"),
        "validate_name_invalid"
    );
    assert_eq!(
        code_of("missing_references", "levels/main.scene"),
        "validate_reference_missing"
    );
    assert_eq!(code_of("duplicate_ids", "tex/a.png"), "validate_id_duplicate");
    assert_eq!(code_of("duplicate_ids", "tex/b.png"), "validate_id_duplicate");
    assert_eq!(
        code_of("orphaned_assets", "tex/loose.png"),
        "validate_asset_orphaned"
    );
    assert_eq!(code_of("texture_size_limits", "tex/a.png"), "texture_size_limits");
    assert_eq!(
        code_of("mesh_triangle_budget", "meshes/big.mesh"),
        "mesh_triangle_budget"
    );
}

/// Spec 32 § 4.2 bump rule (ADR-1668 took it: `tags` arrived after v1 shipped): a version 1 set
/// still loads, with no tags, and is upgraded in memory, so writing it back records the current
/// version (`ASSET_SET_FORMAT_VERSION`'s contract: the version written into every serialized
/// set) instead of claiming v1 for a file that may now carry v2 fields. Fixed by ADR-5632.
#[test]
fn a_version_1_set_loads_and_is_written_back_at_the_current_version() {
    use regia_asset_pipeline::asset_graph::ASSET_SET_FORMAT_VERSION;
    let v1 = r#"(format_version: 1, assets: [(path: "a.png", kind: texture, cooked_bytes: 4)])"#;
    let mut set = AssetSet::from_ron(v1).expect("v1 still loads");
    assert!(set.assets[0].tags.is_empty());
    assert_eq!(set.format_version, ASSET_SET_FORMAT_VERSION, "upgraded in memory");
    set.assets[0].tags.insert("team".into(), "art".into());
    let text = set.to_ron().expect("writes");
    assert!(
        text.contains(&format!("format_version: {ASSET_SET_FORMAT_VERSION}")),
        "{text}"
    );
    let back = AssetSet::from_ron(&text).expect("reads back");
    assert_eq!(back, set);
}

/// Spec 32 § 5.3 as refined by ADR-5632: every non-root member is shared or exclusive, a member
/// reached only through a shared member is shared (deleting the root would not free it), and the
/// closure's bytes split exactly into roots, shared and exclusive.
#[test]
fn a_member_behind_a_shared_member_is_shared_and_the_bytes_split_exactly() {
    let mut set = AssetSet::new();
    set.push(
        AssetEntry::new("root.scene", AssetKind::Scene)
            .with_references(["a.mat", "own.png"])
            .with_cooked_bytes(1),
    );
    set.push(
        AssetEntry::new("other.scene", AssetKind::Scene)
            .with_references(["a.mat"])
            .with_cooked_bytes(2),
    );
    set.push(
        AssetEntry::new("a.mat", AssetKind::Material)
            .with_references(["b.png"])
            .with_cooked_bytes(10),
    );
    set.push(AssetEntry::new("b.png", AssetKind::Texture).with_cooked_bytes(100));
    set.push(AssetEntry::new("own.png", AssetKind::Texture).with_cooked_bytes(1000));
    let graph = DependencyGraph::build(&set);
    let map = size_map(
        &set,
        &graph,
        &SizeMapQuery {
            roots: vec!["root.scene".into()],
            ..SizeMapQuery::default()
        },
    )
    .expect("size map");
    assert_eq!(map.closure_assets, 4, "other.scene is outside the closure");
    assert_eq!(map.total_bytes, 1111);
    assert_eq!(map.roots_bytes, 1);
    assert_eq!(
        map.exclusive_bytes, 1000,
        "only own.png is freed by deleting the root"
    );
    assert_eq!(
        map.shared_bytes, 110,
        "a.mat is used from outside, b.png only through it"
    );
    assert_eq!(
        map.roots_bytes + map.shared_bytes + map.exclusive_bytes,
        map.total_bytes
    );
    let row = |name: &str| map.rows.iter().find(|r| r.name == name).expect("a row");
    assert_eq!(
        (row("b.png").shared_bytes, row("b.png").exclusive_bytes),
        (100, 0)
    );
    assert_eq!(
        (row("own.png").shared_bytes, row("own.png").exclusive_bytes),
        (0, 1000)
    );
}
