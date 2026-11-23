// SPDX-License-Identifier: MPL-2.0
//! The Terrain panel's scatter brush class picker (`editor.terrain_scatter_brush`, page
//! `book/agents/terrain-scatter-paint.md`).
//!
//! Scatter mode (`9`) paints instances of one class of the level's scatter set. The class used to
//! be an index only the `,` / `.` keys moved; this module lists the classes by name and selects one
//! by name, for the panel's combo box and for agents. Everything is a plain function over the
//! `World`, so tests drive it without a display.

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_terrain::{MAX_PAINT_DENSITY, paintable_scatter_set};
use serde_json::{Value, json};

use crate::terrain_tool::TerrainTool;
use crate::viewport_agent::Handler;

/// A scatter brush request refused: a lowercase reason code and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScatterBrushError {
    /// `scatter_brush_no_classes`, `scatter_brush_unknown_class` or `scatter_brush_bad_density`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

/// The class names of the set Scatter mode paints from, in set order (empty with no set).
#[must_use]
pub fn class_names(world: &World) -> Vec<String> {
    paintable_scatter_set(world)
        .map(|set| set.classes.iter().map(|c| c.name.clone()).collect())
        .unwrap_or_default()
}

/// The index of the class Scatter mode paints, wrapped into the set (`None` with no classes).
/// With no [`TerrainTool`] resource yet (a fresh edit world) the default tool paints class 0.
#[must_use]
pub fn selected_class(world: &World) -> Option<usize> {
    let count = class_names(world).len();
    (count > 0).then(|| {
        world
            .get_resource::<TerrainTool>()
            .map_or(0, |tool| tool.scatter_class % count)
    })
}

/// Selects the class named `name` for the Scatter brush; returns its index.
///
/// # Errors
///
/// `scatter_brush_no_classes` when there is no scatter set, `scatter_brush_unknown_class` when the
/// set has no class of that name.
pub fn select_class(world: &mut World, name: &str) -> Result<usize, ScatterBrushError> {
    let names = class_names(world);
    if names.is_empty() {
        return Err(ScatterBrushError {
            code: "scatter_brush_no_classes",
            message: "no scatter set to paint from".into(),
        });
    }
    let Some(index) = names.iter().position(|n| n == name) else {
        return Err(ScatterBrushError {
            code: "scatter_brush_unknown_class",
            message: format!("no class `{name}` (classes: {})", names.join(", ")),
        });
    };
    world.init_resource::<TerrainTool>();
    world.resource_mut::<TerrainTool>().scatter_class = index;
    Ok(index)
}

/// Sets the Scatter brush density, instances per square meter.
///
/// # Errors
///
/// `scatter_brush_bad_density` unless `density` is finite, above 0 and at most
/// [`MAX_PAINT_DENSITY`].
pub fn set_density(world: &mut World, density: f32) -> Result<(), ScatterBrushError> {
    if !(density.is_finite() && density > 0.0 && density <= MAX_PAINT_DENSITY) {
        return Err(ScatterBrushError {
            code: "scatter_brush_bad_density",
            message: format!("density is above 0 and at most {MAX_PAINT_DENSITY}"),
        });
    }
    world.init_resource::<TerrainTool>();
    world.resource_mut::<TerrainTool>().scatter_density = density;
    Ok(())
}

/// The picker's state: `{ enabled, classes, class, class_index, density }`.
#[must_use]
pub fn state(world: &World) -> Value {
    let names = class_names(world);
    let index = selected_class(world);
    let tool = world.get_resource::<TerrainTool>();
    json!({
        "enabled": tool.is_some_and(|t| t.paint_scatter),
        "classes": names,
        "class": index.and_then(|i| names.get(i)),
        "class_index": index,
        "density": tool.map_or(TerrainTool::default().scatter_density, |t| t.scatter_density),
    })
}

/// The picker, drawn at the top of the Terrain panel's Scatter section: the Scatter mode toggle,
/// a combo box of the classes and the density.
pub fn picker_ui(ui: &mut egui::Ui, world: &mut World) {
    ui.heading("Scatter brush");
    let names = class_names(world);
    if names.is_empty() {
        ui.weak("No scatter set is loaded: open a level that names one, or a .scatter.ron below.");
        return;
    }
    let mut on = world
        .get_resource::<TerrainTool>()
        .is_some_and(|t| t.paint_scatter);
    if ui.checkbox(&mut on, "Scatter mode (9)").changed() {
        world.init_resource::<TerrainTool>();
        world
            .resource_mut::<TerrainTool>()
            .select_mode(if on { 9 } else { 1 });
    }
    let current = selected_class(world).unwrap_or(0);
    let mut chosen = None;
    egui::ComboBox::from_label("Class")
        .selected_text(names.get(current).map_or("", String::as_str))
        .show_ui(ui, |ui| {
            for (i, name) in names.iter().enumerate() {
                if ui.selectable_label(i == current, name).clicked() {
                    chosen = Some(name.clone());
                }
            }
        });
    if let Some(name) = chosen {
        // The name came from the set a moment ago; a refused pick leaves the brush as it was.
        let _ = select_class(world, &name);
    }
    let mut density = world
        .get_resource::<TerrainTool>()
        .map_or(0.1, |t| t.scatter_density);
    if ui
        .add(
            egui::Slider::new(&mut density, 0.001..=MAX_PAINT_DENSITY)
                .logarithmic(true)
                .text("per m²"),
        )
        .changed()
    {
        let _ = set_density(world, density);
    }
}

fn bad(code: &str, message: String, not_found: bool) -> RpcError {
    RpcError {
        code: if not_found { NOT_FOUND } else { INVALID_PARAMS },
        message,
        data: Some(json!({
            "code": if not_found { "F0003" } else { "F0002" },
            "reason": code,
            "hint": "see book/agents/terrain-scatter-paint.md",
        })),
    }
}

fn refused(e: ScatterBrushError) -> RpcError {
    let not_found = matches!(e.code, "scatter_brush_no_classes" | "scatter_brush_unknown_class");
    bad(e.code, e.message, not_found)
}

fn handler(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // Parse and validate every parameter before any write: a refused call must leave the
    // brush exactly as it was, or the next mouse stroke would paint state the agent was
    // told was rejected.
    let class = params
        .get("class")
        .map(|v| {
            v.as_str()
                .ok_or_else(|| bad("scatter_brush_bad_class", "`class` is a class name".into(), false))
        })
        .transpose()?;
    let density = params
        .get("density")
        .map(|v| {
            v.as_f64()
                .ok_or_else(|| bad("scatter_brush_bad_density", "`density` is a number".into(), false))
        })
        .transpose()?;
    let enabled = params
        .get("enabled")
        .map(|v| {
            v.as_bool().ok_or_else(|| {
                bad(
                    "scatter_brush_bad_enabled",
                    "`enabled` is a boolean".into(),
                    false,
                )
            })
        })
        .transpose()?;
    if let Some(name) = class {
        let names = class_names(world);
        if names.is_empty() {
            return Err(refused(ScatterBrushError {
                code: "scatter_brush_no_classes",
                message: "no scatter set to paint from".into(),
            }));
        }
        if !names.iter().any(|n| n == name) {
            return Err(refused(ScatterBrushError {
                code: "scatter_brush_unknown_class",
                message: format!("no class `{name}` (classes: {})", names.join(", ")),
            }));
        }
    }
    let density = density.map(|d| d as f32);
    if let Some(d) = density
        && !(d.is_finite() && d > 0.0 && d <= MAX_PAINT_DENSITY)
    {
        return Err(refused(ScatterBrushError {
            code: "scatter_brush_bad_density",
            message: format!("density is above 0 and at most {MAX_PAINT_DENSITY}"),
        }));
    }
    // Every parameter is valid: apply them.
    if let Some(name) = class {
        select_class(world, name).map_err(refused)?;
    }
    if let Some(d) = density {
        set_density(world, d).map_err(refused)?;
    }
    if let Some(on) = enabled {
        world.init_resource::<TerrainTool>();
        world
            .resource_mut::<TerrainTool>()
            .select_mode(if on { 9 } else { 1 });
    }
    Ok(state(world))
}

/// The agent methods of the picker.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.terrain_scatter_brush",
            tool: "terrain_scatter_brush",
            description: "The Terrain panel's scatter brush: lists the scatter classes and the one Scatter mode \
                paints, and optionally picks a class by name, sets the density or turns Scatter mode on or off.",
            params: &[
                ParamSchema {
                    name: "class",
                    kind: "string",
                    required: false,
                    description: "Class name to paint (from `classes`).",
                },
                ParamSchema {
                    name: "density",
                    kind: "number",
                    required: false,
                    description: "Target instances per square meter, above 0 and at most 64.",
                },
                ParamSchema {
                    name: "enabled",
                    kind: "boolean",
                    required: false,
                    description: "true selects Scatter mode, false returns to Raise.",
                },
            ],
            result: "{ enabled, classes: [name], class, class_index, density }",
            mutating: true,
            permission: Permission::EditScene,
        },
        handler as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn world_with_set() -> World {
        let class = |name: &str| regia_terrain::ScatterClass {
            name: name.into(),
            ..regia_terrain::ScatterClass::default()
        };
        let mut world = World::new();
        world.insert_resource(regia_terrain::ScatterPaintSet(Arc::new(
            regia_terrain::ScatterSet {
                classes: vec![class("trees"), class("bushes"), class("rocks")],
                ..regia_terrain::ScatterSet::default()
            },
        )));
        world
    }

    #[test]
    fn the_picker_lists_classes_and_selects_by_name() {
        let mut world = world_with_set();
        assert_eq!(class_names(&world), ["trees", "bushes", "rocks"]);
        assert_eq!(select_class(&mut world, "rocks"), Ok(2));
        assert_eq!(selected_class(&world), Some(2));
        assert_eq!(world.resource::<TerrainTool>().scatter_class, 2);
        assert_eq!(
            select_class(&mut world, "nope").expect_err("refused").code,
            "scatter_brush_unknown_class"
        );
        assert_eq!(selected_class(&world), Some(2), "a refused pick changes nothing");
    }

    #[test]
    fn a_class_index_past_the_set_wraps_like_the_keys() {
        let mut world = world_with_set();
        world.init_resource::<TerrainTool>();
        world.resource_mut::<TerrainTool>().scatter_class = 4;
        assert_eq!(selected_class(&world), Some(1));
    }

    #[test]
    fn no_set_means_no_classes() {
        let mut world = World::new();
        assert!(class_names(&world).is_empty());
        assert_eq!(selected_class(&world), None);
        assert_eq!(
            select_class(&mut world, "a").expect_err("refused").code,
            "scatter_brush_no_classes"
        );
    }

    #[test]
    fn a_fresh_world_reports_the_default_class() {
        // No TerrainTool resource yet (the viewport inserts it on first use): the state must
        // still name class 0, like the panel picker displays.
        let world = world_with_set();
        assert_eq!(selected_class(&world), Some(0));
        assert_eq!(state(&world)["class"], "trees");
        assert_eq!(state(&world)["class_index"], 0);
        assert_eq!(state(&world)["density"], TerrainTool::default().scatter_density);
    }

    #[test]
    fn a_refused_mixed_call_changes_nothing() {
        // All parameters are validated before any is applied: a call with a valid `class` and
        // an invalid `density` (or vice versa) must leave the brush as it was.
        let mut world = world_with_set();
        let before = state(&world);
        let err = handler(&mut world, &json!({ "class": "rocks", "density": -1 })).expect_err("refused");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["reason"].as_str()),
            Some("scatter_brush_bad_density")
        );
        assert_eq!(state(&world), before, "a refused call changes nothing");
        let err = handler(&mut world, &json!({ "density": 2, "enabled": "yes" })).expect_err("refused");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["reason"].as_str()),
            Some("scatter_brush_bad_enabled")
        );
        assert_eq!(state(&world), before, "a refused call changes nothing");
        let err = handler(&mut world, &json!({ "class": "rocks", "enabled": 5 })).expect_err("refused");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["reason"].as_str()),
            Some("scatter_brush_bad_enabled")
        );
        assert_eq!(state(&world), before, "a refused call changes nothing");
    }

    #[test]
    fn density_is_bounded() {
        let mut world = world_with_set();
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY, MAX_PAINT_DENSITY + 1.0] {
            assert_eq!(
                set_density(&mut world, bad).expect_err("refused").code,
                "scatter_brush_bad_density"
            );
        }
        assert_eq!(set_density(&mut world, 2.5), Ok(()));
        assert_eq!(world.resource::<TerrainTool>().scatter_density, 2.5);
    }

    #[test]
    fn the_agent_method_picks_a_class_and_reports_the_state() {
        let mut world = world_with_set();
        let out = handler(
            &mut world,
            &json!({ "class": "bushes", "density": 0.5, "enabled": true }),
        )
        .expect("ok");
        assert_eq!(out["class"], "bushes");
        assert_eq!(out["class_index"], 1);
        assert_eq!(out["enabled"], true);
        assert_eq!(out["classes"], json!(["trees", "bushes", "rocks"]));
        let err = handler(&mut world, &json!({ "class": "zzz" })).expect_err("unknown");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["reason"].as_str()),
            Some("scatter_brush_unknown_class")
        );
        let err = handler(&mut world, &json!({ "density": "x" })).expect_err("bad");
        assert_eq!(err.data.as_ref().and_then(|d| d["code"].as_str()), Some("F0002"));
        assert_eq!(handler(&mut world, &json!({})).expect("read")["enabled"], true);
    }

    #[test]
    fn the_picked_class_is_the_class_a_stroke_paints() {
        // `begin_scatter` reads `scatter_class % classes`: the picker writes the same field.
        let mut world = world_with_set();
        select_class(&mut world, "bushes").expect("pick");
        let names = class_names(&world);
        let tool = world.resource::<TerrainTool>();
        assert_eq!(names[tool.scatter_class % names.len()], "bushes");
    }
}
