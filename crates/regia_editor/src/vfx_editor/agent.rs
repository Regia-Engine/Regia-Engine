// SPDX-License-Identifier: MPL-2.0
//! The VFX editor's agent surface: `editor.vfx_open`, `editor.vfx_state`, `editor.vfx_edit` and
//! `editor.vfx_preview` (`book/agents/vfx.md`, "VFX editor").

use std::sync::atomic::{AtomicU64, Ordering};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::curves::{CURVES, CurveRef, GRADIENTS, GradientRef, KeyEdit};
use super::modules::{self, ModuleKind, ParamValue};
use super::preview::{PreviewError, PreviewRequest, render_at};
use super::sim::{emitter_costs, sim_hash};
use super::{
    HINT, VfxChange, VfxDoc, VfxEditorPanel, edit_effect, err, open_effect, redo, save_effect, undo,
};
use crate::command::UndoStack;
use crate::viewport_agent::Handler;

fn rpc(e: &crate::data_panels::PanelError) -> RpcError {
    e.rpc(HINT)
}

fn doc(world: &World) -> Result<&VfxDoc, RpcError> {
    world
        .get_resource::<VfxEditorPanel>()
        .and_then(VfxEditorPanel::doc)
        .ok_or_else(|| rpc(&err("no_effect", "no effect is open; call editor.vfx_open first")))
}

fn param_json(p: &modules::Param) -> Value {
    let mut out = json!({ "name": p.name, "value": p.value.to_json() });
    match p.kind {
        modules::ParamKind::Num { min, max } => {
            out["kind"] = json!("number");
            out["min"] = json!(min);
            out["max"] = json!(max);
        }
        modules::ParamKind::Int { min, max } => {
            out["kind"] = json!("integer");
            out["min"] = json!(min);
            out["max"] = json!(max);
        }
        modules::ParamKind::Bool => out["kind"] = json!("boolean"),
        modules::ParamKind::Choice(list) => {
            out["kind"] = json!("choice");
            out["choices"] = json!(list);
        }
        modules::ParamKind::Color => out["kind"] = json!("color"),
    }
    out
}

/// The open effect as JSON (`editor.vfx_state`'s result without the optional parts).
fn state_json(world: &World) -> Value {
    let Some(doc) = world
        .get_resource::<VfxEditorPanel>()
        .and_then(VfxEditorPanel::doc)
    else {
        return json!({ "open": false });
    };
    let def = &doc.state.def;
    let emitters: Vec<Value> = def
        .emitters
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let modules: Vec<Value> = doc
                .stack(i)
                .into_iter()
                .map(|(k, enabled)| {
                    json!({
                        "kind": k.name(),
                        "stage": k.stage().name(),
                        "enabled": enabled,
                        "required": k.required(),
                        "params": modules::params(e, k).iter().map(param_json).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let curves: serde_json::Map<String, Value> = CURVES
                .iter()
                .map(|c| {
                    (
                        c.name().to_owned(),
                        json!(c.keys(e).iter().map(|k| [k.t, k.v]).collect::<Vec<_>>()),
                    )
                })
                .collect();
            let gradients: serde_json::Map<String, Value> = GRADIENTS
                .iter()
                .map(|g| {
                    (
                        g.name().to_owned(),
                        json!(
                            g.keys(e)
                                .iter()
                                .map(|k| json!([k.t, k.color]))
                                .collect::<Vec<_>>()
                        ),
                    )
                })
                .collect();
            json!({
                "index": i,
                "name": e.name,
                "draw_list": modules::draw_list(e),
                "modules": modules,
                "curves": curves,
                "gradients": gradients,
            })
        })
        .collect();
    json!({
        "open": true,
        "path": doc.path,
        "dirty": doc.dirty(),
        "revision": doc.revision,
        "name": def.name,
        "version": def.version,
        "budget": def.system.budget,
        "fixed_bounds": def.system.bounds.map(|b| json!({ "min": b.min, "max": b.max })),
        "emitters": emitters,
        "disabled": doc.state.disabled.len(),
        "timeline": super::panel::timeline_json(def),
        "overview": world.get_resource::<VfxEditorPanel>().map(|p| super::panel::overview_json(p, def)),
        "preview_time": world.get_resource::<VfxEditorPanel>().map_or(0.0, |p| p.ui.time),
        "palette": modules::palette().iter().map(|k| k.name()).collect::<Vec<_>>(),
        "undo_depth": world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
        "redo_depth": world.get_resource::<UndoStack>().map_or(0, UndoStack::redo_depth),
    })
}

fn u64_param(params: &Value, key: &str) -> Result<Option<u64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a non-negative integer"))),
    }
}

fn f32_param(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    crate::data_panels::opt_f32(params, key)
}

fn str_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

/// The emitter `emitter` names: an index or a name.
fn emitter_index(world: &World, params: &Value) -> Result<usize, RpcError> {
    let doc = doc(world)?;
    let n = doc.state.def.emitters.len();
    match params.get("emitter") {
        None | Some(Value::Null) => Err(RpcError::invalid_params(
            "`emitter` (an index or a name) is required",
        )),
        Some(Value::String(name)) => doc
            .state
            .def
            .emitter_index(name)
            .ok_or_else(|| rpc(&err("unknown_emitter", format!("no emitter `{name}`")))),
        Some(v) => {
            let i = v
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`emitter` must be an index or a name"))?
                as usize;
            if i < n {
                Ok(i)
            } else {
                Err(rpc(&err(
                    "unknown_emitter",
                    format!("no emitter {i} (the effect has {n})"),
                )))
            }
        }
    }
}

fn module_param(params: &Value) -> Result<ModuleKind, RpcError> {
    let name = str_param(params, "module")
        .ok_or_else(|| RpcError::invalid_params("`module` (a module kind name) is required"))?;
    ModuleKind::from_name(name).ok_or_else(|| {
        rpc(&err(
            "unknown_module",
            format!(
                "unknown module `{name}`; known: {}",
                modules::ALL_KINDS.map(ModuleKind::name).join(", ")
            ),
        ))
    })
}

fn index_param(params: &Value) -> Result<usize, RpcError> {
    u64_param(params, "index")?
        .map(|i| i as usize)
        .ok_or_else(|| RpcError::invalid_params("`index` is required"))
}

fn value_param(params: &Value) -> Result<ParamValue, RpcError> {
    match params.get("value") {
        Some(Value::Bool(b)) => Ok(ParamValue::Bool(*b)),
        Some(Value::String(s)) => Ok(ParamValue::Text(s.clone())),
        Some(Value::Number(n)) => n
            .as_f64()
            .map(|f| ParamValue::Num(f as f32))
            .ok_or_else(|| RpcError::invalid_params("`value` must be a number, a boolean or a string")),
        _ => Err(RpcError::invalid_params(
            "`value` (a number, a boolean or a string) is required",
        )),
    }
}

fn curve_param(params: &Value) -> Result<CurveRef, RpcError> {
    let name = str_param(params, "curve").ok_or_else(|| RpcError::invalid_params("`curve` is required"))?;
    CurveRef::from_name(name).ok_or_else(|| {
        rpc(&err(
            "unknown_curve",
            format!(
                "unknown curve `{name}`; known: {}",
                CURVES.map(CurveRef::name).join(", ")
            ),
        ))
    })
}

fn gradient_param(params: &Value) -> Result<GradientRef, RpcError> {
    let name =
        str_param(params, "gradient").ok_or_else(|| RpcError::invalid_params("`gradient` is required"))?;
    GradientRef::from_name(name).ok_or_else(|| {
        rpc(&err(
            "unknown_curve",
            format!(
                "unknown gradient `{name}`; known: {}",
                GRADIENTS.map(GradientRef::name).join(", ")
            ),
        ))
    })
}

fn need_f32(params: &Value, key: &str) -> Result<f32, RpcError> {
    // A non-finite or missing value is the caller's mistake; out-of-range values go to the editor's
    // own checks (`vfx_editor_bad_value`).
    match params.get(key) {
        Some(v) => v
            .as_f64()
            .map(|f| f as f32)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number"))),
        None => Err(RpcError::invalid_params(format!("`{key}` is required"))),
    }
}

/// `min` or `max` of `set_fixed_bounds`: three finite numbers.
fn corner_param(params: &Value, key: &str) -> Result<[f32; 3], RpcError> {
    let bad = || RpcError::invalid_params(format!("`{key}` must be an array of three numbers [x, y, z]"));
    let list = params.get(key).and_then(Value::as_array).ok_or_else(bad)?;
    let mut out = [0.0f32; 3];
    if list.len() != 3 {
        return Err(bad());
    }
    for (o, v) in out.iter_mut().zip(list) {
        *o = v.as_f64().ok_or_else(bad)? as f32;
    }
    Ok(out)
}

fn color_param(params: &Value) -> Result<String, RpcError> {
    str_param(params, "color")
        .map(str::to_owned)
        .ok_or_else(|| RpcError::invalid_params("`color` (an sRGB hex string) is required"))
}

/// The change an `editor.vfx_edit` call asks for.
fn change_of(world: &World, op: &str, params: &Value) -> Result<VfxChange, RpcError> {
    Ok(match op {
        "add_module" => VfxChange::AddModule(emitter_index(world, params)?, module_param(params)?),
        "remove_module" => VfxChange::RemoveModule(emitter_index(world, params)?, module_param(params)?),
        "enable_module" | "disable_module" => VfxChange::SetEnabled(
            emitter_index(world, params)?,
            module_param(params)?,
            op == "enable_module",
        ),
        "set_param" => {
            let param = str_param(params, "param")
                .ok_or_else(|| RpcError::invalid_params("`param` is required"))?
                .to_owned();
            VfxChange::SetParam(
                emitter_index(world, params)?,
                module_param(params)?,
                param,
                value_param(params)?,
            )
        }
        "curve_add_key" => VfxChange::Key(
            emitter_index(world, params)?,
            KeyEdit::AddKey(
                curve_param(params)?,
                need_f32(params, "t")?,
                need_f32(params, "v")?,
            ),
        ),
        "curve_move_key" => VfxChange::Key(
            emitter_index(world, params)?,
            KeyEdit::MoveKey(
                curve_param(params)?,
                index_param(params)?,
                need_f32(params, "t")?,
                need_f32(params, "v")?,
            ),
        ),
        "curve_delete_key" => VfxChange::Key(
            emitter_index(world, params)?,
            KeyEdit::DeleteKey(curve_param(params)?, index_param(params)?),
        ),
        "gradient_add_key" => VfxChange::Key(
            emitter_index(world, params)?,
            KeyEdit::AddColor(
                gradient_param(params)?,
                need_f32(params, "t")?,
                color_param(params)?,
            ),
        ),
        "gradient_move_key" => VfxChange::Key(
            emitter_index(world, params)?,
            KeyEdit::MoveColor(
                gradient_param(params)?,
                index_param(params)?,
                need_f32(params, "t")?,
                color_param(params)?,
            ),
        ),
        "gradient_delete_key" => VfxChange::Key(
            emitter_index(world, params)?,
            KeyEdit::DeleteColor(gradient_param(params)?, index_param(params)?),
        ),
        "add_emitter" => VfxChange::AddEmitter(
            str_param(params, "name")
                .ok_or_else(|| RpcError::invalid_params("`name` is required"))?
                .to_owned(),
        ),
        "remove_emitter" => VfxChange::RemoveEmitter(emitter_index(world, params)?),
        "move_emitter" => {
            let to = u64_param(params, "to")?
                .ok_or_else(|| RpcError::invalid_params("`to` (the new position) is required"))?;
            VfxChange::MoveEmitter(emitter_index(world, params)?, to as usize)
        }
        "set_budget" => {
            let b = u64_param(params, "value")?
                .ok_or_else(|| RpcError::invalid_params("`value` (the budget) is required"))?;
            VfxChange::Budget(u32::try_from(b).unwrap_or(u32::MAX))
        }
        "set_fixed_bounds" => VfxChange::FixedBounds(Some(regia_vfx::format::FixedBoundsDef {
            min: corner_param(params, "min")?,
            max: corner_param(params, "max")?,
        })),
        "clear_fixed_bounds" => VfxChange::FixedBounds(None),
        "fixed_bounds_from_measured" => {
            let panel = world.resource::<VfxEditorPanel>();
            let time = f32_param(params, "time")?.unwrap_or(panel.ui.time);
            if !(0.0..=super::preview::MAX_TIME).contains(&time) {
                return Err(rpc(&err(
                    "bad_value",
                    format!("`time` must be in 0..={} seconds", super::preview::MAX_TIME),
                )));
            }
            let seed = u64_param(params, "seed")?.unwrap_or(panel.ui.seed);
            let def = super::panel::preview_definition(panel, &doc(world)?.state.def);
            let fixed = super::panel::fixed_from_measured(&def, seed, time).ok_or_else(|| {
                rpc(&err(
                    "empty_bounds",
                    format!("nothing is alive at {time} s (seed {seed}) to measure"),
                ))
            })?;
            VfxChange::FixedBounds(Some(fixed))
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "`op` must be one of {}, got `{other}`",
                EDIT_OPS.join(", ")
            )));
        }
    })
}

/// Every `op` of `editor.vfx_edit`.
pub const EDIT_OPS: &[&str] = &[
    "preview_emitter",
    "scrub",
    "add_module",
    "remove_module",
    "enable_module",
    "disable_module",
    "set_param",
    "curve_add_key",
    "curve_move_key",
    "curve_delete_key",
    "gradient_add_key",
    "gradient_move_key",
    "gradient_delete_key",
    "add_emitter",
    "remove_emitter",
    "move_emitter",
    "set_budget",
    "set_fixed_bounds",
    "clear_fixed_bounds",
    "fixed_bounds_from_measured",
    "save",
    "undo",
    "redo",
];

fn vfx_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")
        .ok_or_else(|| RpcError::invalid_params("`path` (a project-relative .vfx.ron) is required"))?;
    open_effect(world, path).map_err(|e| rpc(&e))?;
    Ok(state_json(world))
}

fn vfx_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut out = state_json(world);
    if out["open"] == json!(false) {
        return Ok(out);
    }
    let seed = u64_param(params, "seed")?.unwrap_or(1);
    if let Some(steps) = u64_param(params, "sim_steps")? {
        let def =
            super::panel::preview_definition(world.resource::<VfxEditorPanel>(), &doc(world)?.state.def);
        let h = sim_hash(&def, seed, u32::try_from(steps).unwrap_or(u32::MAX));
        out["sim"] = json!({ "steps": h.steps, "seed": seed, "particles": h.particles, "hash": format!("{:016x}", h.hash) });
    }
    if let Some(time) = f32_param(params, "cost_time")? {
        if !(0.0..=super::preview::MAX_TIME).contains(&time) {
            return Err(rpc(&err(
                "bad_value",
                format!("`cost_time` must be in 0..={} seconds", super::preview::MAX_TIME),
            )));
        }
        let def =
            super::panel::preview_definition(world.resource::<VfxEditorPanel>(), &doc(world)?.state.def);
        out["costs"] = json!(
            emitter_costs(&def, seed, time, None)
                .iter()
                .map(cost_json)
                .collect::<Vec<_>>()
        );
    }
    if let Some(time) = f32_param(params, "bounds_time")? {
        if !(0.0..=super::preview::MAX_TIME).contains(&time) {
            return Err(rpc(&err("bad_value", "bounds_time must be in 0..=30 seconds")));
        }
        let panel = world.resource::<VfxEditorPanel>();
        let def = super::panel::preview_definition(panel, &doc(world)?.state.def);
        out["bounds"] = super::panel::bounds_json(&def, seed, time);
    }
    Ok(out)
}

fn cost_json(c: &super::sim::EmitterCost) -> Value {
    json!({
        "emitter": c.name,
        "alive": c.alive,
        "spawned_per_sec": c.spawned_per_sec,
        "draw_list": c.draw_list,
        "draw_calls": c.draw_calls,
        "gpu_ms": c.gpu_ms,
        "gpu_source": if c.gpu_ms.is_some() { "timestamps" } else { "cpu_counts" },
    })
}

fn vfx_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = str_param(params, "op").ok_or_else(|| RpcError::invalid_params("`op` is required"))?;
    let extra = match op {
        "preview_emitter" => {
            let index = emitter_index(world, params)?;
            let name = doc(world)?.state.def.emitters[index].name.clone();
            let optional_bool = |key: &str| -> Result<Option<bool>, RpcError> {
                params
                    .get(key)
                    .map(|v| {
                        v.as_bool()
                            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be boolean")))
                    })
                    .transpose()
            };
            let enabled = optional_bool("enabled")?;
            let solo = optional_bool("solo")?;
            let mut panel = world.resource_mut::<VfxEditorPanel>();
            let enabled = enabled.unwrap_or_else(|| !panel.ui.preview_disabled.contains(&name));
            let solo = solo.unwrap_or_else(|| panel.ui.preview_solo.contains(&name));
            super::panel::set_preview_emitter(&mut panel, &name, enabled, solo);
            json!({})
        }
        "scrub" => {
            doc(world)?;
            let time = need_f32(params, "time")?;
            super::panel::scrub(&mut world.resource_mut::<VfxEditorPanel>(), time).map_err(|e| rpc(&e))?;
            json!({})
        }
        "save" => {
            let report = save_effect(world).map_err(|e| rpc(&e))?;
            json!({ "written": report.written, "disabled_not_saved": report.disabled_not_saved })
        }
        "undo" => json!({ "undone": undo(world) }),
        "redo" => json!({ "redone": redo(world) }),
        _ => {
            doc(world)?;
            let change = change_of(world, op, params)?;
            let changed = edit_effect(world, &change).map_err(|e| rpc(&e))?;
            json!({ "changed": changed })
        }
    };
    let mut out = state_json(world);
    if let (Some(map), Some(more)) = (out.as_object_mut(), extra.as_object()) {
        map.extend(more.clone());
    }
    Ok(out)
}

static PNG_COUNTER: AtomicU64 = AtomicU64::new(0);

fn vfx_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("vfx_editor.agent_preview").entered();
    let doc = doc(world)?.clone();
    let time = f32_param(params, "time")?.unwrap_or(1.0);
    let seed = u64_param(params, "seed")?.unwrap_or(1);
    let width = u32::try_from(u64_param(params, "width")?.unwrap_or(256)).unwrap_or(u32::MAX);
    let height = u32::try_from(u64_param(params, "height")?.unwrap_or(144)).unwrap_or(u32::MAX);
    let want_png = params.get("png").and_then(Value::as_bool).unwrap_or(false);
    let want_bounds = params.get("bounds").and_then(Value::as_bool).unwrap_or(false);
    super::preview::check_request(width, height, time).map_err(|e| preview_rpc(&e))?;
    let device = crate::agent_ext::viewport_device(world).map_err(|e| {
        rpc(&err(
            "no_gpu",
            format!("no GPU device for the preview: {}", e.message),
        ))
    })?;
    let mut panel = world.remove_resource::<VfxEditorPanel>().unwrap_or_default();
    let request = PreviewRequest {
        revision: doc.revision,
        seed,
        time,
        width,
        height,
    };
    let def = super::panel::preview_definition(&panel, &doc.state.def);
    let result = render_at(&mut panel.preview, &device, &def, &request).cloned();
    // Opt-in GPU bounds (P-225): one blocking reduction of the session's pool after the frame.
    let gpu_bounds = if want_bounds && result.is_ok() {
        panel
            .preview
            .as_mut()
            .and_then(super::preview::PreviewSession::gpu_bounds)
    } else {
        None
    };
    world.insert_resource(panel);
    let frame = result.map_err(|e| preview_rpc(&e))?;
    let mut out = json!({
        "path": doc.path,
        "revision": doc.revision,
        "time": frame.time,
        "steps": frame.steps,
        "seed": frame.seed,
        "width": frame.width,
        "height": frame.height,
        "coverage": frame.coverage,
        "gpu_alive": frame.counters[3],
        "counters": { "additive": frame.counters[0], "alpha": frame.counters[1], "mesh": frame.counters[2], "alive": frame.counters[3] },
        "gpu_ms": frame.gpu_ms,
        "costs": emitter_costs(&def, seed, frame.time, frame.gpu_ms).iter().map(cost_json).collect::<Vec<_>>(),
    });
    if want_bounds {
        let bounds =
            gpu_bounds.ok_or_else(|| rpc(&err("preview_failed", "the GPU bounds could not be read back")))?;
        out["gpu_bounds"] = super::panel::gpu_bounds_json(&def, &bounds);
    }
    if want_png {
        let bytes = super::preview::png_bytes(&frame.pixels, frame.width, frame.height)
            .map_err(|e| rpc(&err("preview_failed", format!("png: {e}"))))?;
        let dir = std::env::temp_dir().join("regia-vfx-preview");
        std::fs::create_dir_all(&dir)
            .map_err(|e| rpc(&err("io_failed", format!("{}: {e}", dir.display()))))?;
        let n = PNG_COUNTER.fetch_add(1, Ordering::Relaxed);
        let file = dir.join(format!("preview-{}-{n}.png", std::process::id()));
        let tmp = dir.join(format!("preview-{}-{n}.png.tmp", std::process::id()));
        std::fs::write(&tmp, &bytes)
            .and_then(|()| std::fs::rename(&tmp, &file))
            .map_err(|e| rpc(&err("io_failed", format!("{}: {e}", file.display()))))?;
        out["png"] = json!(file.display().to_string());
    }
    Ok(out)
}

fn preview_rpc(e: &PreviewError) -> RpcError {
    match e {
        PreviewError::BadValue(m) => rpc(&err("bad_value", m.clone())),
        PreviewError::Gpu(m) => rpc(&err("preview_failed", m.clone())),
    }
}

const OPEN_PARAMS: &[ParamSchema] = &[ParamSchema {
    name: "path",
    kind: "string",
    required: true,
    description: "Project-relative .vfx.ron to open (replaces the open effect; the undo history stays).",
}];

const STATE_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "bounds_time",
        kind: "number",
        required: false,
        description: "Measured AABBs at this time (0..=30 seconds), per emitter and system: every live particle grown by its drawn size, every motion module; plus the authored fixed bounds when set (mode `fixed`).",
    },
    ParamSchema {
        name: "sim_steps",
        kind: "number",
        required: false,
        description: "Also run the CPU reference simulation this many 1/60 s steps and return `sim: { particles, hash }` (the A/B fingerprint).",
    },
    ParamSchema {
        name: "cost_time",
        kind: "number",
        required: false,
        description: "Also return the per-emitter cost rows at this time in seconds (CPU counts).",
    },
    ParamSchema {
        name: "seed",
        kind: "number",
        required: false,
        description: "Instance seed for sim_steps and cost_time (default 1).",
    },
];

const EDIT_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "enabled",
        kind: "boolean",
        required: false,
        description: "preview_emitter: enable this emitter in the preview only.",
    },
    ParamSchema {
        name: "solo",
        kind: "boolean",
        required: false,
        description: "preview_emitter: isolate this emitter in the preview only.",
    },
    ParamSchema {
        name: "time",
        kind: "number",
        required: false,
        description: "scrub: pause and seek the panel preview to 0..=30 seconds; fixed_bounds_from_measured: the \
            measuring time (default the preview time).",
    },
    ParamSchema {
        name: "op",
        kind: "string",
        required: true,
        description: "add_module, remove_module, enable_module, disable_module, set_param, curve_add_key, curve_move_key, \
            curve_delete_key, gradient_add_key, gradient_move_key, gradient_delete_key, add_emitter, remove_emitter, \
            move_emitter, set_budget, set_fixed_bounds, clear_fixed_bounds, fixed_bounds_from_measured, preview_emitter, \
            scrub, save, undo or redo.",
    },
    ParamSchema {
        name: "emitter",
        kind: "string",
        required: false,
        description: "The emitter: its name or its index.",
    },
    ParamSchema {
        name: "module",
        kind: "string",
        required: false,
        description: "Module kind (the `palette` names plus spawn, init, render, events).",
    },
    ParamSchema {
        name: "param",
        kind: "string",
        required: false,
        description: "set_param: the parameter name (see the module's `params` in editor.vfx_state).",
    },
    ParamSchema {
        name: "value",
        kind: "string",
        required: false,
        description: "set_param: a number, boolean or string; set_budget: the budget.",
    },
    ParamSchema {
        name: "curve",
        kind: "string",
        required: false,
        description: "curve_*: size_over_life, alpha_over_life, rotation_over_life or ribbon_width.",
    },
    ParamSchema {
        name: "gradient",
        kind: "string",
        required: false,
        description: "gradient_*: init_color, color_over_life or ribbon_color.",
    },
    ParamSchema {
        name: "index",
        kind: "number",
        required: false,
        description: "curve_move_key, curve_delete_key, gradient_move_key, gradient_delete_key: the key index.",
    },
    ParamSchema {
        name: "t",
        kind: "number",
        required: false,
        description: "Key time in 0..=1.",
    },
    ParamSchema {
        name: "v",
        kind: "number",
        required: false,
        description: "curve keys: the value.",
    },
    ParamSchema {
        name: "color",
        kind: "string",
        required: false,
        description: "gradient keys: #rrggbb or #rrggbbaa.",
    },
    ParamSchema {
        name: "name",
        kind: "string",
        required: false,
        description: "add_emitter: the new emitter's name.",
    },
    ParamSchema {
        name: "to",
        kind: "number",
        required: false,
        description: "move_emitter: the new position.",
    },
    ParamSchema {
        name: "min",
        kind: "array",
        required: false,
        description: "set_fixed_bounds: the box's lower corner [x, y, z], metres in the effect's local space.",
    },
    ParamSchema {
        name: "max",
        kind: "array",
        required: false,
        description: "set_fixed_bounds: the box's upper corner [x, y, z] (>= min on every axis).",
    },
    ParamSchema {
        name: "seed",
        kind: "number",
        required: false,
        description: "fixed_bounds_from_measured: the instance seed (default the preview seed).",
    },
];

const PREVIEW_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "time",
        kind: "number",
        required: false,
        description: "Seconds into the effect, 0..=30 (default 1). An earlier time than the last call restarts the session.",
    },
    ParamSchema {
        name: "seed",
        kind: "number",
        required: false,
        description: "Instance seed (default 1).",
    },
    ParamSchema {
        name: "width",
        kind: "number",
        required: false,
        description: "Frame width, 32..=1024 (default 256).",
    },
    ParamSchema {
        name: "height",
        kind: "number",
        required: false,
        description: "Frame height, 32..=1024 (default 144).",
    },
    ParamSchema {
        name: "png",
        kind: "boolean",
        required: false,
        description: "Also write the frame as a PNG in a temp dir and return its path (default false).",
    },
    ParamSchema {
        name: "bounds",
        kind: "boolean",
        required: false,
        description: "Also reduce the GPU pool to per-emitter bounds (blocking; `gpu_bounds`, default false).",
    },
];

/// The VFX editor's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.vfx_open",
                tool: "vfx_open",
                description: "Open a .vfx.ron in the VFX editor (spec 26 § 3) and return its emitters and module stacks.",
                params: OPEN_PARAMS,
                result: "{ open, path, dirty, revision, name, version, budget, fixed_bounds, emitters: [{ index, name, draw_list, \
                    modules: [{ kind, stage, enabled, required, params: [{ name, kind, value, min?, max?, choices? }] }], \
                    curves: { name: [[t, v]] }, gradients: { name: [[t, color]] } }], disabled, palette, undo_depth, redo_depth }",
                mutating: true,
                permission: Permission::Assets,
            },
            vfx_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vfx_state",
                tool: "vfx_state",
                description: "The open effect in the VFX editor: module stacks, parameters, curves and gradients; \
                    optionally a fixed-seed CPU simulation fingerprint and per-emitter cost rows.",
                params: STATE_PARAMS,
                result: "editor.vfx_open's result, plus sim?: { steps, seed, particles, hash } and costs?: [{ emitter, \
                    alive, spawned_per_sec, draw_list, draw_calls, gpu_ms, gpu_source }], timeline, overview, preview_time, bounds?: { mode, fixed, fixed_world, kind, source, space, time, seed, system, emitters }",
                mutating: false,
                permission: Permission::Read,
            },
            vfx_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vfx_edit",
                tool: "vfx_edit",
                description: "Edit the open effect in the VFX editor: add, remove, enable or disable a module (the A/B \
                    toggle), set a parameter, edit curve and gradient keys, add, remove or move emitters (each one undo \
                    step, validated by the .vfx.ron validator; a refused edit changes nothing), save, undo or redo. Preview-only preview_emitter and scrub do not dirty the asset or enter undo.",
                params: EDIT_PARAMS,
                result: "editor.vfx_open's result, plus changed? (bool), written? and disabled_not_saved? (save), \
                    undone?/redone?",
                mutating: true,
                permission: Permission::Assets,
            },
            vfx_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vfx_preview",
                tool: "vfx_preview",
                description: "Render the open effect at a time and seed through the offscreen VFX harness (sky, lit \
                    ground, the GPU simulation) and return a frame summary with per-emitter cost; optionally a PNG path.",
                params: PREVIEW_PARAMS,
                result: "{ path, revision, time, steps, seed, width, height, coverage, gpu_alive, counters: { additive, \
                    alpha, mesh, alive }, gpu_ms, costs: [...], png?, gpu_bounds?: { source, kind, space, system, \
                    emitters: [{ name, aabb }] } }",
                mutating: false,
                permission: Permission::Read,
            },
            vfx_preview as Handler,
        ),
    ]
}
