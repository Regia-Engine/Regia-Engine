// SPDX-License-Identifier: MPL-2.0
//! The shader-permutation policy on the agent protocol (`book/agents/permutation-policy.md`,
//! ADR-2026, master plan § 5.5a): `render.permutations` reads the runtime renderer's permutation
//! guard (the cooked list it checks, admitted, unlisted and refused graph sets), the material
//! graph cache counters, the forward shader's composed permutations and the default budget of
//! every profile; `render.set_permutation_policy` installs a cooked list for a profile with an
//! enforcement mode, or turns the check off.

use regia_app::prelude::*;
use regia_render::permutation_policy::{GraphPermutationInput, PermutationGuardError};
use regia_render::{
    PermutationBudget, PermutationEnforcement, PermutationGuard, PermutationManifest, RenderProfilePreset,
    RuntimeRenderer,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself calls \
         FrameRenderer::permutation_stats and set_permutation_guard)",
    )
}

fn state(renderer: &regia_render::FrameRenderer) -> Value {
    let guard = renderer.permutation_stats();
    let graphs = renderer.material_graph_stats();
    let forward = regia_render::forward_permutation_report();
    let budgets: Vec<Value> = RenderProfilePreset::ALL
        .iter()
        .map(|preset| {
            let budget = PermutationBudget::for_preset(*preset);
            json!({
                "profile": preset.name(),
                "max_pipelines": budget.max_pipelines,
                "max_compile_ms": budget.max_compile_ms,
            })
        })
        .collect();
    json!({
        "guard": guard,
        "graphs": {
            "ready": graphs.ready,
            "pending": graphs.pending,
            "failed": graphs.failed,
            "built_on_frame_path": graphs.built_on_frame_path,
            "built_off_frame_path": graphs.built_off_frame_path,
            "fallback_draws": graphs.fallback_draws,
        },
        "forward": {
            "count": forward.permutations.len(),
            "budget": forward.budget,
            "over_budget": forward.over_budget,
            "undeclared": forward.undeclared,
        },
        "default_budgets": budgets,
    })
}

/// `render.permutations`: `{ guard: { enforcement, profile, source, listed_sets, listed_pipelines,
/// admitted, off_target, unlisted_built, refused, unlisted_keys, cooked: [{ profile, sets,
/// pipelines, modules, code_bytes, estimated_compile_ms, budget, over_budget }] }, graphs: { ready,
/// pending, failed, built_on_frame_path, built_off_frame_path, fallback_draws }, forward: { count,
/// budget, over_budget, undeclared }, default_budgets: [{ profile, max_pipelines, max_compile_ms }] }`.
///
/// With `shader_costs: true` (optional `platform`, default `linux`) the answer also holds
/// `shipped_shaders`: the cook report's per-profile shipped forward-shader cost (ADR-2664),
/// `{ format_version, platform, profiles: [{ profile, permutations, failed, wgsl_bytes,
/// spirv_bytes, compile_cost_ms, measured_naga_ms, budget, over_budget }] }`. `compile_cost_ms` is
/// an estimate; `measured_naga_ms` is this host's naga time. Composing the shaders takes seconds
/// the first time, which is why it is opt-in.
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    let mut answer = state(&renderer.0);
    // Wrong-typed `shader_costs` or `platform` is refused, not read as the default (ADR-2622).
    if crate::params::opt_bool(params, "shader_costs")?.unwrap_or(false) {
        let platform = crate::params::opt_str(params, "platform")?.unwrap_or("linux");
        if !regia_render::permutation_policy::SHIPPED_PLATFORMS.contains(&platform) {
            return Err(coded(
                INVALID_PARAMS,
                "permutation_unknown_platform",
                format!("unknown platform `{platform}`"),
                "use one of linux, windows, macos, android",
            ));
        }
        let report = regia_render::shader_cost_report(platform, &std::collections::BTreeMap::new());
        if let Some(object) = answer.as_object_mut() {
            object.insert("shipped_shaders".to_string(), json!(report));
        }
    }
    Ok(answer)
}

fn coded(jsonrpc: i64, code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(jsonrpc, code, message, hint)
}

/// The manifest of `params`: `manifest` (the cooked JSON object), `path` (a `permutations.json`)
/// or `graphs` (a list of `{ key, vertex?, double_sided? }`, enumerated here with the default
/// budgets for `platform`, default `linux`).
fn manifest_from(world: &World, params: &Value) -> Result<PermutationManifest, RpcError> {
    let unreadable = |message: String| {
        coded(
            INVALID_PARAMS,
            "permutation_manifest_unreadable",
            message,
            "pass the cooked permutations.json object as `manifest`, its file as `path`, or `graphs`",
        )
    };
    if let Some(manifest) = params.get("manifest") {
        let text = serde_json::to_string(manifest).map_err(|e| unreadable(e.to_string()))?;
        return PermutationManifest::from_json(&text).map_err(|e| unreadable(e.to_string()));
    }
    if let Some(path) = crate::params::opt_str(params, "path")? {
        // ADR-1816: the path is project-relative. Absolute, `..` and a symlink out are
        // `path_outside_project`; a missing path or a FIFO is F0003 and is not read.
        let file = crate::req_path::project_file(world, "path", path)?;
        let bytes = crate::req_path::read_file_capped(&file, crate::req_path::MAX_READ_BYTES)
            .map_err(|e| unreadable(format!("{path}: {e}")))?;
        let text = String::from_utf8(bytes).map_err(|e| unreadable(format!("{path}: {e}")))?;
        return PermutationManifest::from_json(&text).map_err(|e| unreadable(format!("{path}: {e}")));
    }
    if let Some(graphs) = crate::params::opt_array(params, "graphs")? {
        let mut inputs = Vec::with_capacity(graphs.len());
        for graph in graphs {
            let key = crate::params::opt_str(graph, "key")?
                .and_then(|k| u64::from_str_radix(k, 16).ok())
                .ok_or_else(|| unreadable(format!("graph `key` must be 16 hex digits: {graph}")))?;
            inputs.push(GraphPermutationInput {
                key,
                name: format!("agent:{key:016x}"),
                vertex: crate::params::opt_bool(graph, "vertex")?.unwrap_or(false),
                uv1: false,
                code_bytes: 0,
                double_sided: crate::params::opt_bool(graph, "double_sided")?.unwrap_or(false),
            });
        }
        let platform = crate::params::opt_str(params, "platform")?.unwrap_or("linux");
        return Ok(PermutationManifest::enumerate(
            platform,
            &inputs,
            &std::collections::BTreeMap::new(),
        ));
    }
    Err(unreadable(
        "one of `manifest`, `path` or `graphs` is required (or enforcement \"off\")".into(),
    ))
}

/// `render.set_permutation_policy { enforcement, profile?, manifest? | path? | graphs?, platform? }`:
/// installs the cooked list of `profile` with `enforcement` (`warn` or `fallback`), or turns the
/// check off (`off`, nothing else needed). Answers the new `render.permutations` state.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = crate::params::opt_str(params, "enforcement")?.unwrap_or("warn");
    let enforcement = PermutationEnforcement::from_name(name).ok_or_else(|| {
        coded(
            INVALID_PARAMS,
            "permutation_bad_enforcement",
            format!("unknown enforcement `{name}`"),
            "use \"off\", \"warn\" (dev: build and warn) or \"fallback\" (shipping: standard surface)",
        )
    })?;
    let guard = if enforcement == PermutationEnforcement::Off {
        PermutationGuard::off()
    } else {
        let manifest = manifest_from(world, params)?;
        let profile = crate::params::opt_str(params, "profile")?
            .unwrap_or_else(|| RenderProfilePreset::platform_default().name());
        let source = crate::params::opt_str(params, "path")?.unwrap_or("agent");
        PermutationGuard::from_manifest(&manifest, profile, enforcement)
            .map(|guard| guard.with_source(source))
            .map_err(|err| match err {
                PermutationGuardError::UnknownProfile(_) => coded(
                    NOT_FOUND,
                    "permutation_unknown_profile",
                    err.to_string(),
                    "name a profile the manifest's platform runs (render.permutations default_budgets)",
                ),
                PermutationGuardError::BadKey(_) => coded(
                    INVALID_PARAMS,
                    "permutation_manifest_unreadable",
                    err.to_string(),
                    "recook: the manifest lists graph keys as 16 hex digits",
                ),
            })?
    };
    let mut renderer = world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(no_renderer)?;
    renderer.0.set_permutation_guard(guard);
    Ok(state(&renderer.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::FrameRenderer;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        world
    }

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("code")?.as_str()
    }

    #[test]
    fn reads_the_guard_the_graph_cache_and_the_budgets() {
        let mut world = world();
        let got = get(&mut world, &json!({})).expect("read");
        assert_eq!(got["guard"]["enforcement"], "off");
        assert_eq!(got["guard"]["refused"], 0);
        assert_eq!(got["graphs"]["fallback_draws"], 0);
        assert_eq!(got["forward"]["budget"], 256);
        let budgets = got["default_budgets"].as_array().expect("budgets");
        assert_eq!(budgets.len(), RenderProfilePreset::ALL.len());
        assert_eq!(budgets[0]["profile"], "MobileLow");
        assert_eq!(
            budgets[0]["max_pipelines"],
            PermutationBudget::MOBILE.max_pipelines
        );
    }

    #[test]
    fn shader_costs_are_opt_in_and_labelled() {
        let mut world = world();
        let plain = get(&mut world, &json!({})).expect("state");
        assert!(plain.get("shipped_shaders").is_none());
        let got = get(
            &mut world,
            &json!({ "shader_costs": true, "platform": "android" }),
        )
        .expect("costs");
        let profiles = got["shipped_shaders"]["profiles"].as_array().expect("profiles");
        assert_eq!(profiles.len(), 2, "MobileLow and MobileHigh");
        assert_eq!(profiles[0]["profile"], "MobileLow");
        assert_eq!(profiles[0]["failed"], 0);
        assert!(profiles[0]["permutations"].as_u64().unwrap_or(0) > 0);
        assert!(profiles[0]["wgsl_bytes"].as_u64().unwrap_or(0) > 0);
        assert!(profiles[0]["spirv_bytes"].as_u64().unwrap_or(0) > 0);
        assert!(profiles[0]["compile_cost_ms"].as_u64().unwrap_or(0) > 0);
        assert!(
            profiles[0]["measured_naga_ms"].is_number(),
            "kept apart from the estimate"
        );
        let err =
            get(&mut world, &json!({ "shader_costs": true, "platform": "amiga" })).expect_err("refused");
        assert_eq!(code(&err), Some("permutation_unknown_platform"));
    }

    #[test]
    fn installs_a_list_and_turns_it_off() {
        let mut world = world();
        let got = set(
            &mut world,
            &json!({
                "enforcement": "fallback",
                "profile": "DesktopLow",
                "graphs": [{ "key": "00000000000000aa" }, { "key": "00000000000000bb", "vertex": true }],
            }),
        )
        .expect("install");
        assert_eq!(got["guard"]["enforcement"], "fallback");
        assert_eq!(got["guard"]["profile"], "DesktopLow");
        assert_eq!(got["guard"]["listed_sets"], 2);
        // Desktop Low draws GPU-driven: 10 for the surface graph + 13 for the vertex-stage graph.
        assert_eq!(got["guard"]["listed_pipelines"], 23);
        assert_eq!(got["guard"]["source"], "agent");
        // Every profile's cooked totals, Linux here: the three Desktop presets and the Deck.
        let cooked = got["guard"]["cooked"].as_array().expect("cooked");
        assert_eq!(cooked.len(), 4);
        assert_eq!(cooked[0]["profile"], "DesktopLow");
        assert_eq!(cooked[0]["sets"], 2);
        assert_eq!(cooked[0]["over_budget"], false);
        let manifest = PermutationManifest::enumerate("android", &[], &std::collections::BTreeMap::new());
        let got = set(
            &mut world,
            &json!({ "enforcement": "warn", "profile": "MobileHigh", "manifest": manifest }),
        )
        .expect("install inline");
        assert_eq!(got["guard"]["listed_sets"], 0);
        let got = set(&mut world, &json!({ "enforcement": "off" })).expect("off");
        assert_eq!(got["guard"]["enforcement"], "off");
        assert!(got["guard"]["profile"].is_null());
    }

    #[test]
    fn errors_carry_their_codes() {
        let mut world = world();
        let cases = [
            (json!({ "enforcement": "strict" }), "permutation_bad_enforcement"),
            (
                json!({ "enforcement": "warn" }),
                "permutation_manifest_unreadable",
            ),
            (
                json!({ "enforcement": "warn", "graphs": [{ "key": "zz" }] }),
                "permutation_manifest_unreadable",
            ),
            (
                json!({ "enforcement": "warn", "manifest": { "format_version": 1 } }),
                "permutation_manifest_unreadable",
            ),
            (
                json!({ "enforcement": "warn", "profile": "MobileHigh", "graphs": [] }),
                "permutation_unknown_profile",
            ),
        ];
        for (params, want) in cases {
            let err = set(&mut world, &params).expect_err("refused");
            assert_eq!(code(&err), Some(want), "{params}: {err:?}");
        }
    }

    #[test]
    fn without_a_renderer_it_is_not_found() {
        let mut world = World::new();
        assert_eq!(get(&mut world, &json!({})).expect_err("none").code, NOT_FOUND);
        let err = set(&mut world, &json!({ "enforcement": "off" })).expect_err("none");
        assert_eq!(err.code, NOT_FOUND);
    }

    fn path_reason(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("reason")?.as_str()
    }

    /// `path` is project-relative (ADR-1816): a file inside the project loads, and an absolute
    /// path, `..`, a missing file and (on Unix) a symlink out or a FIFO do not.
    #[test]
    fn permutation_policy_path_is_confined() {
        let root = regia_test_utils::unique_temp_dir("agent-perm-policy");
        let manifest = PermutationManifest::enumerate("linux", &[], &std::collections::BTreeMap::new());
        let text = manifest.to_json().expect("manifest json");
        let dir = root.join("policy");
        std::fs::create_dir_all(&dir).expect("policy dir");
        std::fs::write(dir.join("list.json"), &text).expect("write manifest");

        let mut world = world();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let got = set(
            &mut world,
            &json!({
                "enforcement": "warn",
                "profile": "DesktopLow",
                "path": "policy/list.json",
            }),
        )
        .expect("in project");
        assert_eq!(got["guard"]["enforcement"], "warn");
        assert_eq!(got["guard"]["source"], "policy/list.json");

        let absolute = dir.join("list.json");
        let err = set(
            &mut world,
            &json!({
                "enforcement": "warn",
                "path": absolute.to_string_lossy(),
            }),
        )
        .expect_err("absolute");
        assert_eq!(code(&err), Some("F0002"));
        assert_eq!(path_reason(&err), Some("path_outside_project"));

        let err = set(
            &mut world,
            &json!({ "enforcement": "warn", "path": "../list.json" }),
        )
        .expect_err("parent");
        assert_eq!(path_reason(&err), Some("path_outside_project"));

        let err = set(
            &mut world,
            &json!({ "enforcement": "warn", "path": "policy/missing.json" }),
        )
        .expect_err("missing");
        assert_eq!(code(&err), Some("F0003"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = regia_test_utils::unique_temp_dir("agent-perm-out");
            std::fs::write(outside.join("list.json"), "{}").expect("outside file");
            symlink(outside.join("list.json"), root.join("escape.json")).expect("symlink");
            let err = set(
                &mut world,
                &json!({ "enforcement": "warn", "path": "escape.json" }),
            )
            .expect_err("symlink");
            assert_eq!(path_reason(&err), Some("path_outside_project"));
            let still = std::fs::read_to_string(outside.join("list.json")).expect("untouched");
            assert_eq!(still, "{}");

            let fifo = root.join("pipe.json");
            let made = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo");
            assert!(made.success(), "mkfifo");
            let err =
                set(&mut world, &json!({ "enforcement": "warn", "path": "pipe.json" })).expect_err("fifo");
            assert_eq!(code(&err), Some("F0003"), "{err:?}");
            let _ = std::fs::remove_dir_all(&outside);
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
