// SPDX-License-Identifier: MPL-2.0
//! Material graphs on the agent protocol (`book/agents/material-graph.md`, `specs/22` T2.4):
//! `material.graph_get` reads a graph (RON and JSON forms, node list), `material.graph_set`
//! replaces a graph or edits nodes and outputs in place, `material.compile` type-checks and
//! generates the WGSL surface function, with errors mapped back to node names.
//!
//! The graphs live in [`regia_matgraph::MaterialGraphLibrary`]; `material.graph_set` inserts
//! the library when the app has none.

use regia_app::prelude::*;
use regia_matgraph::{CompileErrors, MaterialGraph, MaterialGraphLibrary};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

fn missing_library() -> RpcError {
    RpcError::not_found(
        "no material graph library in this app",
        "add regia_matgraph::MaterialGraphPlugin, or create a graph with material.graph_set",
    )
}

fn missing_graph(library: &MaterialGraphLibrary, name: &str) -> RpcError {
    let names: Vec<&str> = library.names().collect();
    RpcError::not_found(
        format!("no material graph `{name}`"),
        if names.is_empty() {
            "the library is empty; create one with material.graph_set".to_string()
        } else {
            format!("graphs: {}", names.join(", "))
        },
    )
}

fn name_param(params: &Value) -> Result<&str, RpcError> {
    crate::params::opt_str(params, "name")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`name` (graph name) is required"))
}

fn errors_json(errors: &CompileErrors) -> Value {
    Value::Array(
        errors
            .0
            .iter()
            .map(|e| {
                json!({
                    "node": e.node,
                    "message": e.message,
                    // ADR-4901: a stable lowercase code and the authored node (the call that
                    // expanded a function node); `generated_code_invalid` maps to none.
                    "code": e.code(),
                    "authored_node": e.authored_node(),
                })
            })
            .collect(),
    )
}

fn graph_json(graph: &MaterialGraph) -> Result<Value, RpcError> {
    serde_json::to_value(graph).map_err(|e| RpcError::internal(format!("graph serialization failed: {e}")))
}

/// `material.graph_get`: without `name`, the graph list; with `name`, the graph as RON and as
/// JSON plus a flat node list (name, kind, inputs, texture).
pub fn graph_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let library = world
        .get_resource::<MaterialGraphLibrary>()
        .ok_or_else(missing_library)?;
    let Some(name) = crate::params::opt_str(params, "name")? else {
        let graphs: Vec<Value> = library
            .names()
            .filter_map(|name| {
                library.get(name).map(|entry| {
                    json!({
                        "name": name,
                        "revision": entry.revision,
                        "nodes": entry.graph.nodes.len(),
                        "params": entry.graph.params.len(),
                        "textures": entry.graph.textures.len(),
                    })
                })
            })
            .collect();
        return Ok(json!({ "graphs": graphs, "cache": cache_json(library) }));
    };
    let entry = library.get(name).ok_or_else(|| missing_graph(library, name))?;
    let ron = entry
        .graph
        .to_ron()
        .map_err(|e| RpcError::internal(format!("graph RON failed: {e}")))?;
    let nodes: Vec<Value> = entry
        .graph
        .nodes
        .iter()
        .map(|(node_name, node)| {
            json!({
                "name": node_name,
                "kind": node.kind(),
                "inputs": node.inputs(),
                "texture": node.texture(),
            })
        })
        .collect();
    Ok(json!({
        "name": name,
        "revision": entry.revision,
        "ron": ron,
        "graph": graph_json(&entry.graph)?,
        "nodes": nodes,
    }))
}

fn cache_json(library: &MaterialGraphLibrary) -> Value {
    let stats = library.cache_stats();
    json!({ "hits": stats.hits, "misses": stats.misses, "entries": stats.entries })
}

/// Merges `edits` into the object at `target[key]`; a `null` value removes the entry.
fn merge_object(target: &mut Value, key: &str, edits: &Value) -> Result<(), RpcError> {
    let Value::Object(edits) = edits else {
        return Err(RpcError::invalid_params(format!(
            "`{key}` must be an object (name → value, null removes)"
        )));
    };
    let slot = target
        .get_mut(key)
        .ok_or_else(|| RpcError::internal(format!("graph JSON has no `{key}`")))?;
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    if let Value::Object(map) = slot {
        for (name, value) in edits {
            if value.is_null() && key == "nodes" {
                map.remove(name);
            } else {
                map.insert(name.clone(), value.clone());
            }
        }
    }
    Ok(())
}

/// `material.graph_set`: create or replace a graph (`ron` string or `graph` JSON object), or
/// edit the stored one (`nodes`: name → node JSON, `null` deletes; `output`: slot → node name
/// or `null`; `params` / `textures`: full replacement arrays; `shading`: object). Returns the
/// new revision and the compile diagnostics, so an edit loop needs no separate call.
pub fn graph_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = name_param(params)?.to_string();
    let ron = params.get("ron");
    let whole = params.get("graph");
    let mut library = world.get_resource_or_insert_with(MaterialGraphLibrary::default);
    let graph = match (ron, whole) {
        (Some(_), Some(_)) => {
            return Err(RpcError::invalid_params("pass `ron` or `graph`, not both"));
        }
        (Some(text), None) => {
            let text = text
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`ron` must be a string"))?;
            MaterialGraph::from_ron(text).map_err(|e| RpcError::invalid_params(e.to_string()))?
        }
        (None, Some(value)) => serde_json::from_value::<MaterialGraph>(value.clone())
            .map_err(|e| RpcError::invalid_params(format!("`graph`: {e}")))?,
        (None, None) => {
            let edits = ["nodes", "output", "params", "textures", "shading"];
            if !edits.iter().any(|k| params.get(*k).is_some()) {
                return Err(RpcError::invalid_params(
                    "pass `ron`, `graph`, or edits (`nodes`, `output`, `params`, `textures`, `shading`)",
                ));
            }
            let base = match library.get(&name) {
                Some(entry) => entry.graph.clone(),
                None => MaterialGraph::new(name.clone()),
            };
            let mut value = graph_json(&base)?;
            if let Some(nodes) = params.get("nodes") {
                merge_object(&mut value, "nodes", nodes)?;
            }
            if let Some(output) = params.get("output") {
                merge_object(&mut value, "output", output)?;
            }
            for key in ["params", "textures", "shading"] {
                if let Some(replacement) = params.get(key) {
                    value[key] = replacement.clone();
                }
            }
            serde_json::from_value::<MaterialGraph>(value)
                .map_err(|e| RpcError::invalid_params(format!("edited graph is not valid: {e}")))?
        }
    };
    let revision = library.set(name.clone(), graph);
    let compiled = library
        .compile(&name)
        .ok_or_else(|| missing_graph(&library, &name))?;
    let (ok, errors, key) = match compiled {
        Ok((compiled, _)) => (
            true,
            Value::Array(Vec::new()),
            Some(format!("{:016x}", compiled.key)),
        ),
        Err(errors) => (false, errors_json(&errors), None),
    };
    Ok(json!({ "name": name, "revision": revision, "ok": ok, "key": key, "errors": errors }))
}

/// `material.compile`: type-check and generate the surface function (and the vertex stage of a
/// graph with a world position offset) for the declared switch defaults or the `switches`
/// overrides. Returns the layout (parameter slots, texture slots, node types), the pipeline
/// key, whether the compile cache hit, and the WGSL when `include_wgsl` is true. A failed
/// compile is a successful call with `ok: false` and node-mapped `errors`.
pub fn compile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = name_param(params)?;
    let include_wgsl = crate::params::opt_bool(params, "include_wgsl")?.unwrap_or(false);
    let mut switches = std::collections::BTreeMap::new();
    if let Some(given) = crate::params::opt_object(params, "switches")? {
        for (switch, value) in given {
            let Some(value) = value.as_bool() else {
                return Err(RpcError::invalid_params(format!(
                    "switch `{switch}` must be a boolean"
                )));
            };
            switches.insert(switch.clone(), value);
        }
    }
    let mut library = world
        .get_resource_mut::<MaterialGraphLibrary>()
        .ok_or_else(missing_library)?;
    let result = library
        .compile_with(name, &switches)
        .ok_or_else(|| missing_graph(&library, name))?;
    let cache = cache_json(&library);
    Ok(match result {
        Ok((compiled, hit)) => {
            let params: Vec<Value> = compiled
                .params
                .iter()
                .map(|p| json!({ "name": p.name, "slot": p.slot, "type": format!("{:?}", p.ty) }))
                .collect();
            let textures: Vec<Value> = compiled
                .textures
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "slot": t.slot,
                        "binding": regia_render::surface_graph::GRAPH_TEXTURE_BINDINGS[t.slot],
                        "color_space": format!("{:?}", t.color_space),
                    })
                })
                .collect();
            let node_types: Map<String, Value> = compiled
                .node_types
                .iter()
                .map(|(n, t)| (n.clone(), json!(format!("{t:?}"))))
                .collect();
            let mut out = json!({
                "name": name,
                "ok": true,
                "key": format!("{:016x}", compiled.key),
                "graph_hash": format!("{:016x}", compiled.graph_hash),
                "cache_hit": hit,
                "switches": compiled.switches,
                "vertex_stage": compiled.vertex_wgsl.is_some(),
                "uses_uv1": compiled.uses_uv1,
                "uses_time": compiled.uses_time,
                "params": params,
                "textures": textures,
                "node_types": node_types,
                "wgsl_lines": compiled.wgsl.lines().count(),
                // ADR-4901: the expansion's size against its bounds.
                "lowered_nodes": compiled.lowered_nodes,
                "function_calls": compiled.function_calls,
                "limits": {
                    "max_lowered_nodes": regia_matgraph::MAX_LOWERED_NODES,
                    "max_static_switches": regia_matgraph::MAX_STATIC_SWITCHES,
                },
                "errors": [],
                "cache": cache,
            });
            if include_wgsl {
                out["wgsl"] = json!(compiled.wgsl.as_ref());
                if let Some(vertex) = &compiled.vertex_wgsl {
                    out["vertex_wgsl"] = json!(vertex.as_ref());
                }
            }
            out
        }
        Err(errors) => json!({
            "name": name,
            "ok": false,
            "errors": errors_json(&errors),
            "cache": cache,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINT: &str = r#"(
        format_version: 1,
        params: [(name: "tint", value: Color((0.8, 0.2, 0.1)))],
        nodes: { "tint": Param("tint") },
        output: (base_color: Some("tint")),
    )"#;

    #[test]
    fn set_get_compile_round_trip() {
        let mut world = World::new();
        assert!(graph_get(&mut world, &json!({})).is_err(), "no library yet");
        let set = graph_set(&mut world, &json!({ "name": "paint", "ron": TINT })).expect("set");
        assert_eq!(set["ok"], json!(true), "{set}");
        assert_eq!(set["revision"], json!(1));
        let list = graph_get(&mut world, &json!({})).expect("list");
        assert_eq!(list["graphs"][0]["name"], json!("paint"));
        let got = graph_get(&mut world, &json!({ "name": "paint" })).expect("get");
        assert_eq!(got["nodes"][0]["kind"], json!("Param"));
        assert!(got["ron"].as_str().is_some_and(|r| r.contains("tint")));
        let compiled =
            compile(&mut world, &json!({ "name": "paint", "include_wgsl": true })).expect("compile");
        assert_eq!(compiled["ok"], json!(true), "{compiled}");
        assert_eq!(
            compiled["cache_hit"],
            json!(true),
            "graph_set already compiled it"
        );
        assert_eq!(compiled["params"][0]["slot"], json!(1));
        assert!(
            compiled["wgsl"]
                .as_str()
                .is_some_and(|w| w.contains("material.graph[1].xyz"))
        );
    }

    #[test]
    fn node_edits_apply_and_errors_name_the_node() {
        let mut world = World::new();
        graph_set(&mut world, &json!({ "name": "paint", "ron": TINT })).expect("set");
        // Add a vertex-colour multiply, then break it.
        let edit = graph_set(
            &mut world,
            &json!({
                "name": "paint",
                "nodes": {
                    "vcol": "VertexColor",
                    "rgb": { "Swizzle": { "input": "vcol", "channels": "rgb" } },
                    "base": { "Mul": ["tint", "rgb"] },
                },
                "output": { "base_color": "base" },
            }),
        )
        .expect("edit");
        assert_eq!(edit["ok"], json!(true), "{edit}");
        let broken = graph_set(
            &mut world,
            &json!({ "name": "paint", "nodes": { "rgb": { "Swizzle": { "input": "vcol", "channels": "rgbaq" } } } }),
        )
        .expect("broken edit is stored");
        assert_eq!(broken["ok"], json!(false));
        assert_eq!(broken["errors"][0]["node"], json!("rgb"), "{broken}");
        let deleted = graph_set(
            &mut world,
            &json!({ "name": "paint", "nodes": { "rgb": null, "base": null }, "output": { "base_color": "tint" } }),
        )
        .expect("delete");
        assert_eq!(deleted["ok"], json!(true), "{deleted}");
    }

    #[test]
    fn bad_requests_are_rejected() {
        let mut world = World::new();
        assert!(
            graph_set(&mut world, &json!({ "ron": TINT })).is_err(),
            "name required"
        );
        assert!(
            graph_set(&mut world, &json!({ "name": "x" })).is_err(),
            "nothing to set"
        );
        assert!(graph_set(&mut world, &json!({ "name": "x", "ron": TINT, "graph": {} })).is_err());
        assert!(graph_set(&mut world, &json!({ "name": "x", "ron": "(format_version: 7)" })).is_err());
        assert!(
            graph_set(
                &mut world,
                &json!({ "name": "x", "nodes": { "a": { "Nope": 1 } } })
            )
            .is_err()
        );
        assert!(
            compile(&mut world, &json!({ "name": "x" })).is_err(),
            "unknown graph"
        );
    }

    const SWITCHED: &str = r#"(
        format_version: 2,
        switches: [(name: "red", default: true)],
        nodes: {
            "red": Vec3((1.0, 0.0, 0.0)),
            "blue": Vec3((0.0, 0.0, 1.0)),
            "pick": Switch(switch: "red", on: "red", off: "blue"),
            "lift": Vec3((0.0, 0.1, 0.0)),
        },
        output: (base_color: Some("pick"), world_position_offset: Some("lift")),
    )"#;

    #[test]
    fn compile_takes_switch_overrides_and_reports_the_vertex_stage() {
        let mut world = World::new();
        world.insert_resource(MaterialGraphLibrary::new());
        graph_set(&mut world, &json!({ "name": "s", "ron": SWITCHED })).expect("set");
        let on = compile(&mut world, &json!({ "name": "s", "include_wgsl": true })).expect("compile");
        assert_eq!(on["ok"], json!(true), "{on}");
        assert_eq!(on["vertex_stage"], json!(true));
        assert_eq!(on["switches"]["red"], json!(true));
        assert!(
            on["vertex_wgsl"]
                .as_str()
                .is_some_and(|w| w.contains("material_vertex_offset"))
        );
        let off =
            compile(&mut world, &json!({ "name": "s", "switches": { "red": false } })).expect("compile");
        assert_eq!(off["switches"]["red"], json!(false));
        assert_ne!(on["key"], off["key"]);
        assert!(compile(&mut world, &json!({ "name": "s", "switches": { "red": 1 } })).is_err());
        let ghost =
            compile(&mut world, &json!({ "name": "s", "switches": { "ghost": true } })).expect("call");
        assert_eq!(ghost["ok"], json!(false), "{ghost}");
    }

    /// ADR-4901: a compile reports its expansion against the bounds, and every error carries a
    /// stable code and the authored node (the call node for a node inside a function).
    #[test]
    fn compile_reports_bounds_and_errors_carry_codes_and_authored_nodes() {
        const CALLED: &str = r#"(
            format_version: 4,
            functions: {
                "halve": Inline((
                    inputs: [(name: "x", ty: Vec3)],
                    nodes: { "x": Input("x"), "k": Float(0.5), "half": Mul("x", "k") },
                    outputs: { "out": "half" },
                )),
            },
            nodes: {
                "c": Vec3((0.8, 0.4, 0.2)),
                "h": Call(function: "halve", args: { "x": "c" }),
            },
            output: (base_color: Some("h")),
        )"#;
        let mut world = World::new();
        world.insert_resource(MaterialGraphLibrary::new());
        let set = graph_set(&mut world, &json!({ "name": "f", "ron": CALLED })).expect("set");
        assert_eq!(set["ok"], json!(true), "{set}");
        let ok = compile(&mut world, &json!({ "name": "f" })).expect("compile");
        assert_eq!(ok["function_calls"], json!(1), "{ok}");
        assert!(ok["lowered_nodes"].as_u64().unwrap_or(0) >= 4, "{ok}");
        assert_eq!(
            ok["limits"]["max_lowered_nodes"],
            json!(regia_matgraph::MAX_LOWERED_NODES)
        );
        let broken = graph_set(
            &mut world,
            &json!({ "name": "f", "nodes": { "h": { "Call": { "function": "halve", "args": { "x": "missing" } } } } }),
        )
        .expect("stored");
        assert_eq!(broken["ok"], json!(false), "{broken}");
        let errors = broken["errors"].as_array().cloned().unwrap_or_default();
        assert!(!errors.is_empty(), "{broken}");
        for e in &errors {
            let code = e["code"].as_str().unwrap_or("");
            assert!(regia_matgraph::COMPILE_ERROR_CODES.contains(&code), "{e}");
        }
        assert!(
            errors.iter().any(|e| e["authored_node"] == json!("h")),
            "an error points at the authored call: {broken}"
        );
    }
}
