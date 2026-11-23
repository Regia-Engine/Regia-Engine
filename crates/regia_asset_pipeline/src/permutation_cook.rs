// SPDX-License-Identifier: MPL-2.0
//! The cook's shader-permutation enumeration (master plan § 5.5a, ADR-2026).
//!
//! After a cook, every cooked material graph and instance in the asset database is read back
//! (cache hits included), its code key taken, and [`PermutationManifest::enumerate`] lists the
//! graph pipeline sets of every profile the platform runs, with each profile's budget check.
//! The manifest is written next to the asset manifest (`permutations.json`,
//! `docs/formats/permutations.md`); the cook report carries one summary line per profile and
//! `regia package` fails a shipping package over budget (`ship_check`).
//!
//! Budgets default per preset ([`PermutationBudget::for_preset`]); `Regia.toml` overrides them:
//!
//! ```toml
//! [cook.permutation_budget.MobileHigh]
//! max_pipelines = 512
//! max_compile_ms = 45000
//! max_shader_bytes = 100000000
//! ```
//!
//! `max_shader_bytes` is the ceiling of the profile's shipped forward-shader WGSL bytes
//! ([`regia_render::ShaderBytesBudget`], ADR-2664). `[cook] ship_platforms = ["linux"]` names the
//! platforms the project ships; a budget overrun on any other platform is a warning
//! (`ship_check`).
//!
//! [`graph_cook_checks`] adds what the pipeline count does not see (ADR-4901): each distinct
//! graph code's pipeline set is composed once and its bindings per stage are held to the limit
//! class of every profile of the platform (`mobile` for the Mobile presets, a gate; `desktop`
//! otherwise, reported), generated code a host module rejects is a problem (the one compile
//! failure no authored node maps to), and the static switches of the cooked graphs are counted
//! against `regia_matgraph::MAX_STATIC_SWITCHES`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regia_asset::decode_cooked_file;
use regia_matgraph::CookedMaterialGraph;
use regia_render::permutation_policy::{GraphPermutationInput, PERMUTATION_MANIFEST_FILE};
use regia_render::{
    PermutationBudget, PermutationManifest, RenderProfilePreset, ShaderBytesBudget, ShaderCostReport,
};

use crate::db::AssetDatabase;

/// Importers whose cooked payload is a [`CookedMaterialGraph`].
const GRAPH_IMPORTERS: [&str; 2] = ["matgraph_ron", "matinst_ron"];

/// The graph codes of every cooked material graph and instance in `db`, sorted by source path.
/// Unreadable cooked files are skipped with a warning (the cook already reported their source).
pub fn cooked_graph_inputs(
    db: &AssetDatabase,
    cache_dir: &Path,
    warnings: &mut Vec<String>,
) -> Vec<GraphPermutationInput> {
    cooked_graphs(db, cache_dir, warnings)
        .iter()
        .map(|(source, graph)| GraphPermutationInput::from_code(source.clone(), &graph.code()))
        .collect()
}

/// Every cooked material graph and instance in `db` with its source path, sorted by path.
/// Unreadable cooked files are skipped with a warning.
fn cooked_graphs(
    db: &AssetDatabase,
    cache_dir: &Path,
    warnings: &mut Vec<String>,
) -> Vec<(String, CookedMaterialGraph)> {
    let mut graphs = Vec::new();
    for (source, entry) in &db.entries {
        if !GRAPH_IMPORTERS.contains(&entry.importer.as_str()) {
            continue;
        }
        let path = cache_dir.join(&entry.cooked_rel);
        let decoded = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|bytes| {
            let (_, payload) = decode_cooked_file(&bytes).map_err(|e| e.to_string())?;
            CookedMaterialGraph::from_postcard(payload).map_err(|e| e.to_string())
        });
        match decoded {
            Ok(graph) => graphs.push((source.clone(), graph)),
            Err(err) => warnings.push(format!(
                "permutations: {source}: cooked graph not readable ({err}); not counted"
            )),
        }
    }
    graphs
}

/// What [`graph_cook_checks`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphCookChecks {
    /// Report lines: one per limit class the platform's profiles use, and the static switches.
    pub lines: Vec<String>,
    /// Failures (`regia package` fails a shipping package on any): a set over a gate class, or
    /// generated code a host module rejects. A set over a class that only reports (`desktop`,
    /// wgpu's portable defaults, which the engine's own forward shader passes by design on the
    /// adapter limits it requests; ADR-3002) is marked in that class's line instead.
    pub problems: Vec<String>,
}

/// The binding layout of every distinct cooked graph code against the limit class of each
/// profile of `platform`, and the static switches of the cooked graphs (ADR-4901).
pub fn graph_cook_checks(
    db: &AssetDatabase,
    cache_dir: &Path,
    platform: &str,
    warnings: &mut Vec<String>,
) -> GraphCookChecks {
    let graphs = cooked_graphs(db, cache_dir, warnings);
    graph_checks_of(&graphs, platform, &regia_render::limit_classes())
}

/// [`graph_cook_checks`] over decoded graphs and the given classes (tests pass a tight class).
pub fn graph_checks_of(
    graphs: &[(String, CookedMaterialGraph)],
    platform: &str,
    classes: &[regia_render::LimitClass],
) -> GraphCookChecks {
    let _span = tracing::info_span!("cook_graph_checks", platform, graphs = graphs.len()).entered();
    let mut out = GraphCookChecks::default();
    // Class name → the platform's profiles held to it.
    let mut profiles: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for preset in regia_render::permutation_policy::platform_presets(platform) {
        profiles
            .entry(regia_render::preset_limit_class(preset))
            .or_default()
            .push(preset.name());
    }
    // Class name → the worst use of each metric over every code.
    let mut worst: BTreeMap<&'static str, BTreeMap<String, regia_render::MetricUse>> = BTreeMap::new();
    // Class name → the metrics some code is over on a report-only class.
    let mut over_report: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for (source, graph) in graphs {
        let code = graph.code();
        if !seen.insert(code.key()) {
            continue;
        }
        let usage = match regia_render::graph_code_usage(&code) {
            Ok(usage) => usage,
            Err(err) => {
                out.problems.push(format!(
                    "material graph {source}: generated_code_invalid: a host shader rejects the cooked code ({err}); recook"
                ));
                continue;
            }
        };
        for (&name, names) in &profiles {
            let Some(class) = classes.iter().find(|c| c.name == name) else {
                continue;
            };
            let (uses, over) = regia_render::graph_limit_check(&usage, class);
            for u in uses {
                let slot = worst.entry(name).or_default().entry(u.metric.clone());
                match slot {
                    std::collections::btree_map::Entry::Vacant(v) => {
                        v.insert(u);
                    }
                    std::collections::btree_map::Entry::Occupied(mut o) => {
                        if u.used > o.get().used {
                            o.insert(u);
                        }
                    }
                }
            }
            if over.is_empty() {
                continue;
            }
            if class.gate {
                out.problems.push(format!(
                    "material graph {source}: binding layout over the `{name}` limits ({}): {}",
                    names.join(", "),
                    over.join("; ")
                ));
            } else {
                let metrics = over_report.entry(name).or_default();
                for text in &over {
                    metrics.insert(text.split(' ').next().unwrap_or_default().to_string());
                }
            }
        }
    }
    for (name, metrics) in &worst {
        let used: Vec<String> = metrics
            .values()
            .filter(|m| m.used > 0)
            .map(|m| format!("{} {}/{}", m.metric, m.used, m.limit))
            .collect();
        let report_only = over_report.get(name).map_or(String::new(), |m| {
            format!(
                "; over this report-only class in {} (ADR-3002)",
                m.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        });
        out.lines.push(format!(
            "graph bindings vs `{name}` ({}): {} code(s), worst {}{report_only}",
            profiles.get(name).map(|p| p.join(", ")).unwrap_or_default(),
            seen.len(),
            used.join(", ")
        ));
    }
    let with_switches = graphs.iter().filter(|(_, g)| !g.switches.is_empty()).count();
    let most = graphs.iter().map(|(_, g)| g.switches.len()).max().unwrap_or(0);
    let permutations: BTreeSet<(Vec<&String>, Vec<bool>)> = graphs
        .iter()
        .filter(|(_, g)| !g.switches.is_empty())
        .map(|(_, g)| {
            (
                g.switches.keys().collect(),
                g.switches.values().copied().collect(),
            )
        })
        .collect();
    out.lines.push(format!(
        "static switches: {with_switches} of {} cooked graph(s) declare switches, {} switch permutation(s) cooked, most {most} in one graph (limit {})",
        graphs.len(),
        permutations.len(),
        regia_matgraph::MAX_STATIC_SWITCHES
    ));
    if most > regia_matgraph::MAX_STATIC_SWITCHES {
        out.problems.push(format!(
            "a cooked graph declares {most} static switches, over {}; recook",
            regia_matgraph::MAX_STATIC_SWITCHES
        ));
    }
    out
}

/// `[cook.permutation_budget.<Profile>]` tables of a `Regia.toml` text. A missing field keeps the
/// preset's default; an unknown profile name or a bad value is a warning.
pub fn budget_overrides(regia_toml: &str, warnings: &mut Vec<String>) -> BTreeMap<String, PermutationBudget> {
    let mut budgets = BTreeMap::new();
    let root: toml::Table = match regia_toml.parse() {
        Ok(root) => root,
        // The project file's own errors are reported by whoever reads it for the project.
        Err(_) => return budgets,
    };
    let Some(tables) = root
        .get("cook")
        .and_then(|cook| cook.get("permutation_budget"))
        .and_then(toml::Value::as_table)
    else {
        return budgets;
    };
    for (name, table) in tables {
        let Some(preset) = RenderProfilePreset::from_name(name).filter(|p| !p.is_offline()) else {
            warnings.push(format!(
                "Regia.toml [cook.permutation_budget.{name}]: not a real-time render profile; ignored"
            ));
            continue;
        };
        let mut budget = PermutationBudget::for_preset(preset);
        let field = |key: &str, warnings: &mut Vec<String>| -> Option<u64> {
            let value = table.get(key)?;
            match value.as_integer().and_then(|v| u64::try_from(v).ok()) {
                Some(v) => Some(v),
                None => {
                    warnings.push(format!(
                        "Regia.toml [cook.permutation_budget.{name}] {key} must be a non-negative integer; ignored"
                    ));
                    None
                }
            }
        };
        if let Some(v) = field("max_pipelines", warnings) {
            budget.max_pipelines = u32::try_from(v).unwrap_or(u32::MAX);
        }
        if let Some(v) = field("max_compile_ms", warnings) {
            budget.max_compile_ms = v;
        }
        budgets.insert(preset.name().to_string(), budget);
    }
    budgets
}

/// `[cook.permutation_budget.<Profile>] max_shader_bytes` of a `Regia.toml` text, by profile name
/// (ADR-2664). A bad value or an unknown profile is a warning ([`budget_overrides`] reports the
/// profile names, so only the value is checked here).
pub fn shader_budget_overrides(
    regia_toml: &str,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, ShaderBytesBudget> {
    let mut budgets = BTreeMap::new();
    let Ok(root) = regia_toml.parse::<toml::Table>() else {
        return budgets;
    };
    let Some(tables) = root
        .get("cook")
        .and_then(|cook| cook.get("permutation_budget"))
        .and_then(toml::Value::as_table)
    else {
        return budgets;
    };
    for (name, table) in tables {
        let Some(preset) = RenderProfilePreset::from_name(name).filter(|p| !p.is_offline()) else {
            continue;
        };
        let Some(value) = table.get("max_shader_bytes") else {
            continue;
        };
        match value.as_integer().and_then(|v| u64::try_from(v).ok()) {
            Some(max_wgsl_bytes) => {
                budgets.insert(preset.name().to_string(), ShaderBytesBudget { max_wgsl_bytes });
            }
            None => warnings.push(format!(
                "Regia.toml [cook.permutation_budget.{name}] max_shader_bytes must be a non-negative integer; ignored"
            )),
        }
    }
    budgets
}

/// `[cook] ship_platforms` of a `Regia.toml` text: the platform tags the project ships. `None`
/// when the key is absent or not a list of strings (the packaged platform is then the only one).
#[must_use]
pub fn ship_platforms(regia_toml: &str) -> Option<Vec<String>> {
    let root: toml::Table = regia_toml.parse().ok()?;
    let list = root.get("cook")?.get("ship_platforms")?.as_array()?;
    list.iter()
        .map(|v| v.as_str().map(|s| s.trim().to_ascii_lowercase()))
        .collect()
}

/// The shipped forward-shader cost of every profile of `platform`, with the project's
/// `max_shader_bytes` overrides (ADR-2664). Composing the shaders takes seconds, so the cook
/// report asks for it only through [`crate::CookReport::attach_shader_cost`] and `regia cook --size-report`.
pub fn project_shader_cost(
    project_root: &Path,
    platform: &str,
    warnings: &mut Vec<String>,
) -> ShaderCostReport {
    let _span = tracing::info_span!("cook_shader_cost", platform).entered();
    let toml = std::fs::read_to_string(project_root.join("Regia.toml")).unwrap_or_default();
    regia_render::shader_cost_report(platform, &shader_budget_overrides(&toml, warnings))
}

/// Enumerates the project's graph permutations for `platform` from the cooked cache.
pub fn project_permutations(
    project_root: &Path,
    db: &AssetDatabase,
    cache_dir: &Path,
    platform: &str,
    warnings: &mut Vec<String>,
) -> PermutationManifest {
    let _span = tracing::info_span!("cook_permutations").entered();
    let graphs = cooked_graph_inputs(db, cache_dir, warnings);
    let toml = std::fs::read_to_string(project_root.join("Regia.toml")).unwrap_or_default();
    let budgets = budget_overrides(&toml, warnings);
    PermutationManifest::enumerate(platform, &graphs, &budgets)
}

/// Writes `manifest` to `cache_dir/permutations.json` (atomically). The same list leaves the file
/// alone: a cook of an unchanged project rewrites no cooked file.
///
/// # Errors
///
/// The file cannot be written.
pub fn write_manifest(cache_dir: &Path, manifest: &PermutationManifest) -> std::io::Result<()> {
    let text = manifest.to_json().map_err(std::io::Error::other)?;
    let path = cache_dir.join(PERMUTATION_MANIFEST_FILE);
    if std::fs::read(&path).is_ok_and(|old| old == text.as_bytes()) {
        return Ok(());
    }
    crate::cook_ledger::write_atomic(&path, text.as_bytes())
}

/// Reads `cache_dir/permutations.json`; `Ok(None)` when the cache has none (cooked before
/// ADR-2026).
///
/// # Errors
///
/// The file exists but cannot be read or parsed.
pub fn read_manifest(cache_dir: &Path) -> Result<Option<PermutationManifest>, String> {
    let path = cache_dir.join(PERMUTATION_MANIFEST_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    PermutationManifest::from_json(&text)
        .map(Some)
        .map_err(|err| format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_tables_override_the_named_profiles_only() {
        let mut warnings = Vec::new();
        let budgets = budget_overrides(
            r#"
[cook.permutation_budget.MobileHigh]
max_pipelines = 512

[cook.permutation_budget.desktop_ultra]
max_compile_ms = 1000
max_pipelines = -3

[cook.permutation_budget.Cinematic]
max_pipelines = 1
"#,
            &mut warnings,
        );
        assert_eq!(budgets.len(), 2, "{budgets:?}");
        let mobile = budgets["MobileHigh"];
        assert_eq!(mobile.max_pipelines, 512);
        assert_eq!(mobile.max_compile_ms, PermutationBudget::MOBILE.max_compile_ms);
        let ultra = budgets["DesktopUltra"];
        assert_eq!(ultra.max_compile_ms, 1000);
        assert_eq!(ultra.max_pipelines, PermutationBudget::DESKTOP.max_pipelines);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("Cinematic")));
        assert!(warnings.iter().any(|w| w.contains("max_pipelines must be")));
    }

    #[test]
    fn shader_bytes_budgets_and_the_shipped_platforms_are_read() {
        let mut warnings = Vec::new();
        let budgets = shader_budget_overrides(
            "[cook]\nship_platforms = [\"Linux\", \"windows\"]\n\
             [cook.permutation_budget.MobileHigh]\nmax_shader_bytes = 4096\n\
             [cook.permutation_budget.DesktopLow]\nmax_shader_bytes = -1\n\
             [cook.permutation_budget.DesktopHigh]\nmax_pipelines = 3\n",
            &mut warnings,
        );
        assert_eq!(budgets.len(), 1, "{budgets:?}");
        assert_eq!(budgets["MobileHigh"].max_wgsl_bytes, 4096);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("DesktopLow") && warnings[0].contains("max_shader_bytes"));
        assert_eq!(
            ship_platforms("[cook]\nship_platforms = [\"Linux\", \"windows\"]\n"),
            Some(vec!["linux".to_string(), "windows".to_string()])
        );
        assert_eq!(ship_platforms("[cook]\nchunks = []\n"), None);
        assert_eq!(ship_platforms("[cook]\nship_platforms = [1]\n"), None);
    }

    #[test]
    fn no_budget_table_is_no_override() {
        let mut warnings = Vec::new();
        assert!(budget_overrides("[cook]\nchunks = []\n", &mut warnings).is_empty());
        assert!(budget_overrides("not toml [", &mut warnings).is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_cache_without_a_manifest_reads_none_and_a_bad_one_is_an_error() {
        let dir = regia_test_utils::unique_temp_dir("permutation-cook");
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(read_manifest(&dir), Ok(None));
        let manifest = PermutationManifest::enumerate("linux", &[], &BTreeMap::new());
        write_manifest(&dir, &manifest).expect("write");
        assert_eq!(read_manifest(&dir), Ok(Some(manifest)));
        std::fs::write(dir.join(PERMUTATION_MANIFEST_FILE), "{").expect("write");
        assert!(read_manifest(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn cooked(text: &str) -> CookedMaterialGraph {
        let graph = regia_matgraph::MaterialGraph::from_ron(text).expect("graph");
        CookedMaterialGraph::cook(&graph, None, &mut |_| None).expect("cook")
    }

    const SWITCHED: &str = r#"(
        format_version: 2,
        switches: [(name: "warm", default: true), (name: "wet", default: false)],
        nodes: {
            "a": Vec3((0.9, 0.4, 0.1)),
            "b": Vec3((0.1, 0.4, 0.9)),
            "pick": Switch(switch: "warm", on: "a", off: "b"),
        },
        output: (base_color: Some("pick")),
    )"#;

    /// ADR-4901: every class of the platform's profiles gets a line, the switches are counted,
    /// and a real graph fits both classes.
    #[test]
    fn graph_checks_report_each_class_and_the_static_switches() {
        let graphs = vec![("a.matgraph.ron".to_string(), cooked(SWITCHED))];
        for (platform, class) in [("android", "mobile"), ("linux", "desktop")] {
            let checks = graph_checks_of(&graphs, platform, &regia_render::limit_classes());
            assert!(checks.problems.is_empty(), "{platform}: {:?}", checks.problems);
            assert!(
                checks
                    .lines
                    .iter()
                    .any(|l| l.starts_with(&format!("graph bindings vs `{class}`"))
                        && l.contains("sampled_textures")),
                "{platform}: {:?}",
                checks.lines
            );
            let switches = checks.lines.last().expect("switch line");
            assert!(
                switches.starts_with("static switches: 1 of 1") && switches.contains("most 2 in one graph"),
                "{switches}"
            );
        }
    }

    /// ADR-4901: a set over a gate class is a problem (fails a shipping package) naming the graph,
    /// the class, the profiles and the metric; over a report-only class it is a warning.
    #[test]
    fn a_graph_over_a_class_limit_fails_explicitly() {
        let graphs = vec![("a.matgraph.ron".to_string(), cooked(SWITCHED))];
        let mut classes = regia_render::limit_classes();
        for class in &mut classes {
            if let Some(n) = class.limits.get_mut("sampled_textures") {
                n.value = 1;
            }
        }
        let checks = graph_checks_of(&graphs, "android", &classes);
        assert_eq!(checks.problems.len(), 1, "{:?}", checks.problems);
        let problem = &checks.problems[0];
        assert!(
            problem.contains("a.matgraph.ron")
                && problem.contains("`mobile`")
                && problem.contains("MobileLow")
                && problem.contains("sampled_textures"),
            "{problem}"
        );
        let checks = graph_checks_of(&graphs, "linux", &classes);
        assert!(
            checks.problems.is_empty(),
            "desktop reports only: {:?}",
            checks.problems
        );
        assert!(
            checks
                .lines
                .iter()
                .any(|l| l.starts_with("graph bindings vs `desktop`")
                    && l.contains("over this report-only class in sampled_textures")),
            "{:?}",
            checks.lines
        );
    }

    /// ADR-4901: cooked code a host shader rejects (no authored node maps to it) is an explicit
    /// `generated_code_invalid` problem, never skipped.
    #[test]
    fn cooked_code_a_host_rejects_is_an_explicit_problem() {
        let mut broken = cooked(SWITCHED);
        broken.surface_wgsl = "fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {\n    return nothing_here;\n}\n".into();
        let graphs = vec![("b.matgraph.ron".to_string(), broken)];
        let checks = graph_checks_of(&graphs, "linux", &regia_render::limit_classes());
        assert_eq!(checks.problems.len(), 1, "{:?}", checks.problems);
        assert!(
            checks.problems[0].contains("generated_code_invalid")
                && checks.problems[0].contains("b.matgraph.ron"),
            "{:?}",
            checks.problems
        );
    }
}
