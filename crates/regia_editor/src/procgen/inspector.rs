// SPDX-License-Identifier: MPL-2.0
//! The generator section of the regular inspector (spec 67 § 4): under a selected entity's
//! `ProcGenerator`, the graph's parameters with an override column (edit = override on this
//! generator, reset = remove; one undo entry each), Run / Cancel / Clear / Detach / Bake, the run
//! state and counts, the stale flag, and "Open graph" (opens the graph in the panel and previews
//! on this generator).

use std::collections::BTreeMap;

use regia_ecs::prelude::*;
use regia_procgen::SettingValue;
use regia_procgen::graph::{ParamSource, resolve_graph};
use regia_procgen::world::{ProcGenState, ProcGenerator, ProcOutput};

use super::doc::DocError;
use crate::command::{EditorCommand, UndoStack, resolve_entity};

/// One parameter row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamRow {
    /// Parameter name.
    pub name: String,
    /// Value type name.
    pub ty: String,
    /// The graph default (RON).
    pub default: String,
    /// This generator's override (RON), when set.
    pub generator: Option<String>,
    /// The value in effect (RON) and where it comes from: `generator`, `variant:<path>` or `default`.
    pub effective: (String, String),
}

/// The parameter rows of a generator's graph (variant chain resolved).
///
/// # Errors
///
/// `procgen_not_generator`, or the graph's load error code.
pub fn param_rows(world: &World, e: Entity) -> Result<Vec<ParamRow>, DocError> {
    let generator = world.get::<ProcGenerator>(e).ok_or_else(|| {
        DocError::new(
            "procgen_not_generator",
            format!("entity {} has no ProcGenerator", e.to_bits()),
        )
    })?;
    let resolved = resolve_graph(&generator.graph, &|p| super::doc::graph_text(world, p))
        .map_err(|err| DocError::new(&err.code, err.message))?;
    let parsed = generator.parsed_params().unwrap_or_default();
    let mut rows = Vec::new();
    for p in &resolved.graph.params {
        let own = generator.params.get(&p.name).cloned();
        let effective = if let Some(v) = parsed.get(&p.name) {
            (v.write(), "generator".to_string())
        } else if let Some((v, src)) = resolved.overrides.get(&p.name) {
            let src = match src {
                ParamSource::Variant(path) => format!("variant:{path}"),
                ParamSource::Generator => "generator".to_string(),
                ParamSource::Default => "default".to_string(),
            };
            (v.write(), src)
        } else {
            (p.default.write(), "default".to_string())
        };
        rows.push(ParamRow {
            name: p.name.clone(),
            ty: p.ty.name().to_string(),
            default: p.default.write(),
            generator: own,
            effective,
        });
    }
    Ok(rows)
}

/// A generator's parameter overrides before and after (one undo entry; the generator re-runs).
pub struct SetGeneratorParams {
    entity: u64,
    before: BTreeMap<String, String>,
    after: BTreeMap<String, String>,
}

impl SetGeneratorParams {
    fn set(world: &mut World, bits: u64, params: &BTreeMap<String, String>) {
        let Some(e) = resolve_entity(world, bits) else {
            return;
        };
        if let Some(mut g) = world.get_mut::<ProcGenerator>(e) {
            g.params = params.clone();
        }
        regia_procgen::world::request_run(world, e);
    }
}

impl EditorCommand for SetGeneratorParams {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, self.entity, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, self.entity, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<SetGeneratorParams>() else {
            return false;
        };
        let same_key = self.entity == next.entity
            && self.after.keys().eq(next.after.keys())
            && self.after.keys().eq(self.before.keys());
        if same_key {
            self.after = next.after.clone();
        }
        same_key
    }

    fn description(&self) -> String {
        "Procgen: generator parameter".to_string()
    }
}

/// Sets (RON text) or with `None` removes ("reset") one parameter override on generator `e`.
///
/// # Errors
///
/// `procgen_not_generator`, `procgen_unknown_param` (the graph does not declare it),
/// `procgen_param_value` (not RON).
pub fn set_generator_param(
    world: &mut World,
    e: Entity,
    name: &str,
    value: Option<String>,
) -> Result<(), DocError> {
    let rows = param_rows(world, e)?;
    if !rows.iter().any(|r| r.name == name) {
        return Err(DocError::new(
            "procgen_unknown_param",
            format!("the graph declares no parameter `{name}`"),
        ));
    }
    if let Some(v) = &value
        && SettingValue::parse(v).is_err()
    {
        return Err(DocError::new("procgen_param_value", format!("`{v}` is not RON")));
    }
    let before = world
        .get::<ProcGenerator>(e)
        .map(|g| g.params.clone())
        .unwrap_or_default();
    let mut after = before.clone();
    match value {
        Some(v) => {
            after.insert(name.to_string(), v);
        }
        None => {
            after.remove(name);
        }
    }
    if after == before {
        return Ok(());
    }
    let mut command = SetGeneratorParams {
        entity: e.to_bits(),
        before,
        after,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(command));
    Ok(())
}

/// Opens the generator's graph in the panel and previews on the generator ("Open graph").
///
/// # Errors
///
/// `procgen_not_generator`, and [`super::doc::open`]'s errors.
pub fn open_generator_graph(world: &mut World, e: Entity) -> Result<String, DocError> {
    let path = world
        .get::<ProcGenerator>(e)
        .map(|g| g.graph.clone())
        .ok_or_else(|| {
            DocError::new(
                "procgen_not_generator",
                format!("entity {} has no ProcGenerator", e.to_bits()),
            )
        })?;
    super::install_if_missing(world);
    super::doc::open(world, &path)?;
    world.resource_mut::<super::ProcGraphDoc>().breadcrumb.clear();
    super::debug_view::set_preview(world, Some(e))?;
    Ok(path)
}

enum Action {
    Run,
    Cancel,
    Clear,
    Detach,
    Bake,
    Open,
    Set(String, Option<String>),
}

/// The generator section drawn under the `ProcGenerator` component in the inspector.
pub fn generator_inspector(ui: &mut egui::Ui, world: &mut World, e: Entity) {
    let mut actions = Vec::new();
    {
        let world: &World = world;
        let st = world.get::<ProcGenState>(e);
        let counts = world.get::<ProcOutput>(e).map(|o| {
            (
                o.0.instance_sets.iter().map(|s| s.instances.len()).sum::<usize>(),
                o.0.prefabs.len(),
            )
        });
        ui.horizontal(|ui| {
            ui.label(format!("state: {}", st.map_or("idle", |s| s.state.name())));
            if let Some((inst, prefabs)) = counts {
                ui.weak(format!("{inst} instance(s), {prefabs} prefab(s)"));
            }
            if st.is_some_and(|s| s.stale) {
                ui.colored_label(egui::Color32::from_rgb(240, 180, 60), "stale")
                    .on_hover_text("an input changed since the last run (terrain, an entity, a graph file or a source generator)");
            }
        });
        if let Some((node, code, msg)) = st.and_then(|s| s.last_error.as_ref()) {
            ui.colored_label(
                egui::Color32::from_rgb(230, 80, 70),
                format!("node {node:?}: {code}: {msg}"),
            );
        }
        ui.horizontal(|ui| {
            for (label, action) in [
                ("Run", Action::Run),
                ("Cancel", Action::Cancel),
                ("Clear", Action::Clear),
                ("Detach", Action::Detach),
                ("Bake", Action::Bake),
                ("Open graph", Action::Open),
            ] {
                if ui.small_button(label).clicked() {
                    actions.push(action);
                }
            }
        });
        match param_rows(world, e) {
            Ok(rows) if !rows.is_empty() => {
                egui::Grid::new(("procgen-gen-params", e.to_bits()))
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("parameter");
                        ui.strong("value");
                        ui.strong("override");
                        ui.end_row();
                        for r in rows {
                            ui.label(&r.name)
                                .on_hover_text(format!("{} · default {}", r.ty, r.default));
                            ui.weak(format!("{} ({})", r.effective.0, r.effective.1));
                            let id = egui::Id::new(("procgen-gen-param", e.to_bits(), r.name.clone()));
                            let mut text = ui
                                .data(|d| d.get_temp::<String>(id))
                                .or_else(|| r.generator.clone())
                                .unwrap_or_default();
                            ui.horizontal(|ui| {
                                let edit = ui.add(egui::TextEdit::singleline(&mut text).desired_width(90.0));
                                if edit.changed() {
                                    ui.data_mut(|d| d.insert_temp(id, text.clone()));
                                }
                                if edit.lost_focus()
                                    && !text.is_empty()
                                    && Some(&text) != r.generator.as_ref()
                                {
                                    actions.push(Action::Set(r.name.clone(), Some(text.clone())));
                                    ui.data_mut(|d| d.remove::<String>(id));
                                }
                                if r.generator.is_some() && ui.small_button("reset").clicked() {
                                    actions.push(Action::Set(r.name.clone(), None));
                                    ui.data_mut(|d| d.remove::<String>(id));
                                }
                            });
                            ui.end_row();
                        }
                    });
            }
            Ok(_) => {}
            Err(err) => {
                ui.weak(err.to_string());
            }
        }
    }
    for a in actions {
        let result = match a {
            Action::Run => {
                regia_procgen::world::request_run(world, e);
                Ok(())
            }
            Action::Cancel => {
                regia_procgen::world::cancel(world, e);
                Ok(())
            }
            Action::Clear => {
                regia_procgen::world::clear(world, e);
                Ok(())
            }
            Action::Detach => regia_procgen::world::detach(world, e)
                .map(|_| ())
                .map_err(|(c, m)| DocError::new(&c, m)),
            Action::Bake => regia_procgen::world::bake(world, e)
                .map(|_| ())
                .map_err(|(c, m)| DocError::new(&c, m)),
            Action::Open => open_generator_graph(world, e).map(|_| ()),
            Action::Set(name, v) => set_generator_param(world, e, &name, v),
        };
        if let Err(err) = result {
            tracing::warn!(target: "regia_editor", "procgen generator: {err}");
            if let Some(mut d) = world.get_resource_mut::<super::ProcGraphDoc>() {
                d.toast = err.to_string();
            }
        }
    }
}
