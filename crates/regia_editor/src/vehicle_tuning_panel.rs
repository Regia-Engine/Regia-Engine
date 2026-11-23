// SPDX-License-Identifier: MPL-2.0
//! The Vehicle Tuning panel (`Panel::VehicleTuning`, ADR-9760, `book/agents/vehicles.md`): typed
//! sections for a `.vehicle.ron`, its plots, Apply to a play car, the telemetry strip and recorded
//! runs side by side. The state is [`VehicleTuning`]; every button has an `editor.vehicle_tuning_*`
//! method that does the same.
//!
//! The sections are drawn from the configuration's own serde shape, so a field added to the format
//! shows up without a panel change; enum fields get a picker of their variants. Each edit goes through
//! [`VehicleTuning::set`]: a refused value leaves the asset as it was and shows its code beside the
//! field, and a drag is one undo step however many frames it lasts.
//!
//! Below the car, a **Trailer** and an **Aircraft** section open a `.trailer.ron` and a `.aircraft.ron`
//! with the same generic forms, undo and save (ADR-7745); the run row picks the proving-ground track
//! and whether the car tows the open trailer, and **Fly** trims and flies the open aircraft.
//! A **Bike** and a **Hover hull** section open a `.bike.ron` and a `.hoverhull.ron` the same way
//! (ADR-8630); **Ride** steps the open one on the proving ground for a second of fixed steps, and an
//! edit reaches the riding bike or hover craft at its next step.

use std::path::Path;

use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use regia::physics::vehicle::VehicleConfig;
use regia::physics::vehicle_drivetrain::Drivetrain;
use regia::physics::vehicle_proving::{ProvingScript, ProvingTrack};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::vehicle_tuning::assets::{TunedAsset, TuningDoc};
use crate::vehicle_tuning::{AssetKind, TuningError, VehicleTuning};
use crate::vehicle_tuning_plots as plots;

/// Colors of the plot series, cycled.
const SERIES: [Color32; 6] = [
    Color32::from_rgb(90, 170, 240),
    Color32::from_rgb(240, 150, 70),
    Color32::from_rgb(110, 210, 120),
    Color32::from_rgb(220, 100, 180),
    Color32::from_rgb(230, 220, 90),
    Color32::from_rgb(160, 140, 240),
];

/// Color of a refusal.
const REFUSED: Color32 = Color32::from_rgb(230, 90, 90);

/// One edit the frame collected: the field, the value, and whether a drag is making it.
type Edit = (String, Value, bool);

/// The UI-only state of the panel (the path box and the run length).
#[derive(Resource, Debug)]
pub struct VehicleTuningUi {
    /// The path box.
    pub path: String,
    /// The length of the next run, seconds.
    pub seconds: f32,
    /// The wheel the Wheels section edits.
    pub wheel: usize,
    /// The trailer section's path box.
    pub trailer_path: String,
    /// The aircraft section's path box.
    pub aircraft_path: String,
    /// The next flight's airspeed (m/s), altitude (m) and length (s).
    pub flight: [f32; 3],
    /// The bike section's path box.
    pub bike_path: String,
    /// The hover-hull section's path box.
    pub hover_path: String,
    /// The bike ride's held steer (rad) and speed (m/s).
    pub bike_ride: [f32; 2],
    /// The hover ride's held throttle, strafe and steer axes.
    pub hover_ride: [f32; 3],
}

impl Default for VehicleTuningUi {
    fn default() -> Self {
        Self {
            path: "assets/car.vehicle.ron".to_string(),
            seconds: 20.0,
            wheel: 0,
            trailer_path: "assets/box.trailer.ron".to_string(),
            aircraft_path: "assets/trainer.aircraft.ron".to_string(),
            flight: [50.0, 1000.0, 10.0],
            bike_path: "assets/city.bike.ron".to_string(),
            hover_path: "assets/scout.hoverhull.ron".to_string(),
            bike_ride: [0.1, 8.0],
            hover_ride: [1.0, 0.0, 0.0],
        }
    }
}

/// Draws the panel.
pub fn vehicle_tuning_panel(ui: &mut Ui, world: &mut World) {
    let playing = world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|s| s.mode.is_some());
    let mut local = world.remove_resource::<VehicleTuningUi>().unwrap_or_default();
    let frame = ui.ctx().cumulative_frame_nr();
    crate::vehicle_tuning::with_panel_frame(world, Some(frame), |panel, project| {
        draw(ui, panel, &mut local, project, playing)
    });
    world.insert_resource(local);
}

fn record(panel: &mut VehicleTuning, result: Result<(), TuningError>) {
    match result {
        Ok(()) => {}
        Err(e) => panel.error = Some(e),
    }
}

fn draw(
    ui: &mut Ui,
    panel: &mut VehicleTuning,
    local: &mut VehicleTuningUi,
    project: Option<&Path>,
    playing: bool,
) {
    ui.horizontal(|ui| {
        ui.strong("Vehicle Tuning");
        ui.weak(".vehicle.ron · plots · live apply · runs");
    });
    // editor.vehicle_tuning_open / _save / _undo / _redo.
    ui.horizontal(|ui| {
        ui.text_edit_singleline(&mut local.path);
        if ui.button("Open").clicked() {
            let result = panel.open(project, &local.path, false);
            record(panel, result);
        }
        if ui.button("New").clicked() {
            let result = panel.open(project, &local.path, true);
            record(panel, result);
        }
        let open = panel.asset.is_some();
        if ui.add_enabled(open, egui::Button::new("Save")).clicked() {
            let result = panel.save(project, None).map(|_| ());
            record(panel, result);
        }
        if ui
            .add_enabled(panel.undo_depth() > 0, egui::Button::new("Undo"))
            .clicked()
        {
            panel.undo();
        }
        if ui
            .add_enabled(panel.redo_depth() > 0, egui::Button::new("Redo"))
            .clicked()
        {
            panel.redo();
        }
        if panel.is_dirty() {
            ui.weak("● unsaved");
        }
    });
    if let Some(e) = &panel.error {
        let field = e.field.as_deref().map(|f| format!(" `{f}`")).unwrap_or_default();
        ui.colored_label(REFUSED, format!("Refused{field}: {}", e.message))
            .on_hover_text(format!("Agent error code: {}", e.code));
    }
    if let Some(notice) = &panel.notice {
        ui.weak(notice);
    }
    let Some(asset) = panel.asset.clone() else {
        ui.weak("Open a .vehicle.ron under the project, or New to start one.");
        ui.separator();
        other_sections(ui, panel, local, project);
        return;
    };
    // `VehicleAsset::validate` and the tire hints, with the codes `physics.vehicle_config_check` uses.
    match asset.validate() {
        Ok(()) => {
            ui.weak("valid");
        }
        Err(e) => {
            ui.colored_label(REFUSED, crate::data_panels::sentence(&e.to_string()))
                .on_hover_text(format!("Agent error code: {}", e.code()));
        }
    }
    for warning in regia::physics::vehicle_tire::tire_warnings(&asset.config) {
        ui.colored_label(
            Color32::from_rgb(230, 190, 80),
            crate::data_panels::sentence(&warning.message),
        )
        .on_hover_text(format!("Agent error code: {}", warning.code));
    }
    let config = serde_json::to_value(&asset.config).unwrap_or(Value::Null);
    let refused = panel.error.as_ref().and_then(|e| e.field.clone());
    let mut edits: Vec<Edit> = Vec::new();
    let mut drag_ended = false;
    egui::ScrollArea::vertical()
        .id_salt("vehicle-tuning-scroll")
        .show(ui, |ui| {
            let mut ctx = FieldCtx {
                edits: &mut edits,
                drag_ended: &mut drag_ended,
                refused: refused.as_deref(),
            };
            sections(ui, &mut ctx, &config, &asset.config, local, &asset.name);
            ui.separator();
            plots_section(ui, &mut ctx, &asset.config, local.wheel);
            ui.separator();
            play_section(ui, panel, playing);
            ui.separator();
            runs_section(ui, panel, local);
            ui.separator();
            other_sections(ui, panel, local, project);
        });
    for (field, value, drag) in edits {
        let result = panel.set(&field, value, drag);
        if let Err(e) = result {
            panel.error = Some(e);
        }
    }
    if drag_ended {
        panel.end_drag();
    }
    panel.plot_wheel = local.wheel;
}

/// What a field widget writes to.
struct FieldCtx<'a> {
    edits: &'a mut Vec<Edit>,
    drag_ended: &'a mut bool,
    refused: Option<&'a str>,
}

/// The variants a field of this name can switch between, as the JSON a pick writes.
fn variants(key: &str) -> Option<Vec<(&'static str, Value)>> {
    let choice = match key {
        "drivetrain" => vec![("Simple", json!("Simple")), ("Geared", json!({ "Geared": {} }))],
        "tire_model" => vec![("Damper", json!("Damper")), ("Slip", json!({ "Slip": {} }))],
        "front_differential" | "rear_differential" | "centre_differential" => vec![
            ("Open", json!("Open")),
            ("Locked", json!("Locked")),
            ("LimitedSlip", json!({ "LimitedSlip": { "torque_bias": 2.0 } })),
        ],
        "layout" => vec![
            ("Rwd", json!("Rwd")),
            ("Fwd", json!("Fwd")),
            ("Awd", json!("Awd")),
        ],
        "curve" => vec![
            ("Brush", json!("Brush")),
            (
                "MagicFormula",
                json!({ "MagicFormula": { "shape": 1.3, "curvature": 0.0 } }),
            ),
        ],
        "combined_slip" => vec![
            ("Ellipse", json!("Ellipse")),
            (
                "SlipWeighted",
                json!({ "SlipWeighted": { "shape": 1.0, "stiffness": 10.0 } }),
            ),
        ],
        _ => return None,
    };
    Some(choice)
}

/// The variant name of an externally tagged enum value.
fn variant_name(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s.as_str()),
        Value::Object(map) if map.len() == 1 => map.keys().next().map(String::as_str),
        _ => None,
    }
}

/// A number field: a drag value whose drag is one undo step.
fn number(ui: &mut Ui, ctx: &mut FieldCtx<'_>, path: &str, label: &str, value: f64) {
    let mut v = value;
    let speed = (value.abs() * 0.005).max(0.001);
    let response = ui
        .horizontal(|ui| {
            ui.label(label);
            let r = ui.add(egui::DragValue::new(&mut v).speed(speed));
            if ctx.refused == Some(path) {
                ui.colored_label(REFUSED, "refused");
            }
            r
        })
        .inner;
    if response.drag_started() || response.drag_stopped() {
        *ctx.drag_ended = true;
    }
    if response.changed() && v != value {
        ctx.edits.push((path.to_string(), json!(v), response.dragged()));
    }
}

/// Draws `value` (at `path`) as editable fields: numbers, booleans, vectors, nested sections and
/// enum pickers.
fn field(ui: &mut Ui, ctx: &mut FieldCtx<'_>, path: &str, key: &str, value: &Value) {
    if let Some(choices) = variants(key) {
        let current = variant_name(value).unwrap_or("?").to_string();
        ui.horizontal(|ui| {
            ui.label(key);
            egui::ComboBox::from_id_salt(("vt-variant", path))
                .selected_text(current.as_str())
                .show_ui(ui, |ui| {
                    for (name, json) in &choices {
                        if ui.selectable_label(current == *name, *name).clicked() && current != *name {
                            ctx.edits.push((path.to_string(), json.clone(), false));
                        }
                    }
                });
        });
        // The geared drivetrain's sections are drawn on their own below the picker.
        if key != "drivetrain"
            && let Value::Object(map) = value
            && let Some(inner) = map.values().next()
            && inner.is_object()
        {
            ui.indent(("vt-in", path), |ui| object_fields(ui, ctx, path, inner));
        }
        return;
    }
    match value {
        Value::Number(n) => number(ui, ctx, path, key, n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => {
            let mut on = *b;
            if ui.checkbox(&mut on, key).changed() {
                ctx.edits.push((path.to_string(), json!(on), false));
            }
        }
        Value::Array(items) if items.iter().all(Value::is_number) => {
            ui.horizontal(|ui| {
                ui.label(key);
                for (i, item) in items.iter().enumerate() {
                    let child = format!("{path}.{i}");
                    number(ui, ctx, &child, "", item.as_f64().unwrap_or(0.0));
                }
            });
        }
        Value::Array(items) => {
            egui::CollapsingHeader::new(format!("{key} ({})", items.len()))
                .id_salt(("vt-arr", path))
                .show(ui, |ui| {
                    for (i, item) in items.iter().enumerate() {
                        field(ui, ctx, &format!("{path}.{i}"), &i.to_string(), item);
                    }
                });
        }
        Value::Object(_) => {
            egui::CollapsingHeader::new(key)
                .id_salt(("vt-obj", path))
                .show(ui, |ui| object_fields(ui, ctx, path, value));
        }
        Value::Null => {
            ui.horizontal(|ui| {
                ui.label(key);
                ui.weak("none");
            });
        }
        Value::String(s) => {
            ui.horizontal(|ui| {
                ui.label(key);
                ui.weak(s);
            });
        }
    }
}

fn object_fields(ui: &mut Ui, ctx: &mut FieldCtx<'_>, path: &str, value: &Value) {
    if let Value::Object(map) = value {
        for (key, child) in map {
            let child_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            field(ui, ctx, &child_path, key, child);
        }
    }
}

/// An optional section (`aero`, `aids.abs`): a checkbox that writes `null` or the defaults.
fn optional(ui: &mut Ui, ctx: &mut FieldCtx<'_>, path: &str, label: &str, value: &Value) {
    let mut on = !value.is_null();
    if ui.checkbox(&mut on, label).changed() {
        ctx.edits
            .push((path.to_string(), if on { json!({}) } else { Value::Null }, false));
    }
    if on {
        ui.indent(("vt-opt", path), |ui| object_fields(ui, ctx, path, value));
    }
}

/// The typed sections: chassis, wheels, engine, gearbox, drivetrain and differentials, tire, anti-roll,
/// aero, aids.
fn sections(
    ui: &mut Ui,
    ctx: &mut FieldCtx<'_>,
    config: &Value,
    typed: &VehicleConfig,
    local: &mut VehicleTuningUi,
    name: &str,
) {
    let get = |key: &str| config.get(key).cloned().unwrap_or(Value::Null);
    egui::CollapsingHeader::new("Chassis")
        .default_open(true)
        .show(ui, |ui| {
            let mut text = name.to_string();
            ui.horizontal(|ui| {
                ui.label("name");
                if ui.text_edit_singleline(&mut text).lost_focus() && text != name {
                    ctx.edits.push(("name".to_string(), json!(text), false));
                }
            });
            for key in [
                "chassis_mass",
                "center_of_mass",
                "chassis_half_extents",
                "max_steer_angle",
                "max_brake_force",
                "max_engine_force",
                "rolling_resistance",
            ] {
                field(ui, ctx, key, key, &get(key));
            }
            let half = typed.chassis_half_extents;
            let sq = half * half;
            let inertia = [sq.y + sq.z, sq.x + sq.z, sq.x + sq.y].map(|s| s * typed.chassis_mass / 3.0);
            ui.weak(format!(
                "inertia (solid box): {:.0} / {:.0} / {:.0} kg·m²",
                inertia[0], inertia[1], inertia[2]
            ));
        });
    egui::CollapsingHeader::new(format!("Wheels ({})", typed.wheels.len()))
        .default_open(true)
        .show(ui, |ui| {
            let count = typed.wheels.len();
            local.wheel = local.wheel.min(count.saturating_sub(1));
            ui.horizontal(|ui| {
                for i in 0..count {
                    ui.selectable_value(&mut local.wheel, i, format!("wheel {i}"));
                }
            });
            if let Some(wheel) = config.get("wheels").and_then(|w| w.get(local.wheel)) {
                object_fields(ui, ctx, &format!("wheels.{}", local.wheel), wheel);
            }
        });
    let drivetrain = get("drivetrain");
    egui::CollapsingHeader::new("Drivetrain")
        .default_open(true)
        .show(ui, |ui| {
            field(ui, ctx, "drivetrain", "drivetrain", &drivetrain);
        });
    if let Some(geared) = drivetrain.get("Geared") {
        for (title, key) in [
            ("Engine", "engine"),
            ("Gearbox", "gearbox"),
            ("Clutch", "clutch"),
            ("Tire (lengthwise)", "tire"),
        ] {
            egui::CollapsingHeader::new(title)
                .default_open(key == "engine" || key == "gearbox")
                .show(ui, |ui| {
                    let section = geared.get(key).cloned().unwrap_or(Value::Null);
                    object_fields(ui, ctx, &format!("drivetrain.{key}"), &section);
                    if key == "gearbox" {
                        gear_buttons(ui, ctx, &section);
                    }
                    if key == "engine" {
                        curve_buttons(ui, ctx, &section);
                    }
                });
        }
        egui::CollapsingHeader::new("Differentials").show(ui, |ui| {
            for key in [
                "layout",
                "front_axle",
                "rear_axle",
                "front_differential",
                "rear_differential",
                "centre_differential",
                "front_torque_share",
                "wheel_inertia",
            ] {
                let v = geared.get(key).cloned().unwrap_or(Value::Null);
                field(ui, ctx, &format!("drivetrain.{key}"), key, &v);
            }
        });
    }
    egui::CollapsingHeader::new("Anti-roll").show(ui, |ui| {
        field(ui, ctx, "anti_roll", "anti_roll", &get("anti_roll"));
        ui.horizontal(|ui| {
            if ui.button("Add bar").clicked() {
                let mut bars = get("anti_roll").as_array().cloned().unwrap_or_default();
                bars.push(json!({ "left": 0, "right": 1, "stiffness": 10000.0 }));
                ctx.edits
                    .push(("anti_roll".to_string(), Value::Array(bars), false));
            }
            if ui.button("Remove last").clicked() {
                let mut bars = get("anti_roll").as_array().cloned().unwrap_or_default();
                bars.pop();
                ctx.edits
                    .push(("anti_roll".to_string(), Value::Array(bars), false));
            }
        });
    });
    egui::CollapsingHeader::new("Aero").show(ui, |ui| {
        optional(ui, ctx, "aero", "aero", &get("aero"));
    });
    egui::CollapsingHeader::new("Aids").show(ui, |ui| {
        let aids = get("aids");
        for key in ["abs", "traction_control"] {
            let v = aids.get(key).cloned().unwrap_or(Value::Null);
            optional(ui, ctx, &format!("aids.{key}"), key, &v);
        }
    });
}

/// Add or drop the top gear (`drivetrain.gearbox.forward_ratios`).
fn gear_buttons(ui: &mut Ui, ctx: &mut FieldCtx<'_>, gearbox: &Value) {
    let ratios = gearbox
        .get("forward_ratios")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    ui.horizontal(|ui| {
        if ui.button("Add gear").clicked() {
            let mut next = ratios.clone();
            let last = ratios.last().and_then(Value::as_f64).unwrap_or(1.0);
            next.push(json!(last * 0.8));
            ctx.edits.push((
                "drivetrain.gearbox.forward_ratios".to_string(),
                Value::Array(next),
                false,
            ));
        }
        if ui
            .add_enabled(ratios.len() > 1, egui::Button::new("Drop top gear"))
            .clicked()
        {
            let mut next = ratios.clone();
            next.pop();
            ctx.edits.push((
                "drivetrain.gearbox.forward_ratios".to_string(),
                Value::Array(next),
                false,
            ));
        }
    });
}

/// Add or drop a torque curve point (`drivetrain.engine.torque_curve`).
fn curve_buttons(ui: &mut Ui, ctx: &mut FieldCtx<'_>, engine: &Value) {
    let curve = engine
        .get("torque_curve")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    ui.horizontal(|ui| {
        if ui.button("Add point").clicked() {
            let mut next = curve.clone();
            let last = curve
                .last()
                .and_then(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
                .unwrap_or((1000.0, 100.0));
            next.push(json!([last.0 + 500.0, last.1]));
            ctx.edits.push((
                "drivetrain.engine.torque_curve".to_string(),
                Value::Array(next),
                false,
            ));
        }
        if ui
            .add_enabled(curve.len() > 2, egui::Button::new("Drop last point"))
            .clicked()
        {
            let mut next = curve.clone();
            next.pop();
            ctx.edits.push((
                "drivetrain.engine.torque_curve".to_string(),
                Value::Array(next),
                false,
            ));
        }
    });
}

/// A plot's frame: the rect, the data range and the mapping.
struct Frame {
    rect: Rect,
    x: (f32, f32),
    y: (f32, f32),
}

impl Frame {
    fn to_screen(&self, x: f32, y: f32) -> Pos2 {
        let fx = (x - self.x.0) / (self.x.1 - self.x.0).max(1e-6);
        let fy = (y - self.y.0) / (self.y.1 - self.y.0).max(1e-6);
        Pos2::new(
            self.rect.left() + fx * self.rect.width(),
            self.rect.bottom() - fy * self.rect.height(),
        )
    }

    fn y_at(&self, screen_y: f32) -> f32 {
        let fy = (self.rect.bottom() - screen_y) / self.rect.height().max(1.0);
        self.y.0 + fy * (self.y.1 - self.y.0)
    }
}

/// Draws line series in a small plot and returns its frame.
fn line_plot(ui: &mut Ui, title: &str, series: &[(String, Vec<(f32, f32)>)], x_label: &str) -> Frame {
    ui.label(title);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(420.0), 120.0), Sense::hover());
    let points = series.iter().flat_map(|(_, p)| p.iter());
    let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, 0.0_f32, f32::MIN);
    for (x, y) in points {
        x0 = x0.min(*x);
        x1 = x1.max(*x);
        y0 = y0.min(*y);
        y1 = y1.max(*y);
    }
    if x0 > x1 {
        (x0, x1, y1) = (0.0, 1.0, 1.0);
    }
    let frame = Frame {
        rect,
        x: (x0, x1),
        y: (y0, y1.max(y0 + 1e-3) * 1.05),
    };
    let painter = ui.painter_at(rect);
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, Color32::from_gray(80)),
        egui::StrokeKind::Inside,
    );
    for (i, (_, points)) in series.iter().enumerate() {
        let line: Vec<Pos2> = points.iter().map(|(x, y)| frame.to_screen(*x, *y)).collect();
        painter.add(egui::Shape::line(
            line,
            Stroke::new(1.5, SERIES[i % SERIES.len()]),
        ));
    }
    ui.horizontal_wrapped(|ui| {
        ui.weak(format!("x: {x_label} {x0:.0}–{x1:.0}, y max {:.0}", frame.y.1));
        for (i, (name, _)) in series.iter().enumerate() {
            ui.colored_label(SERIES[i % SERIES.len()], name);
        }
    });
    frame
}

/// The plots; the torque curve's points drag up and down (one undo step per drag).
fn plots_section(ui: &mut Ui, ctx: &mut FieldCtx<'_>, config: &VehicleConfig, wheel: usize) {
    egui::CollapsingHeader::new("Plots")
        .default_open(true)
        .show(ui, |ui| {
            if let Drivetrain::Geared(drivetrain) = &config.drivetrain {
                let curve = plots::torque_power(&drivetrain.engine, 41);
                let torque: Vec<(f32, f32)> = curve.iter().map(|p| (p.rpm, p.torque_nm)).collect();
                let power: Vec<(f32, f32)> = curve.iter().map(|p| (p.rpm, p.power_kw)).collect();
                let frame = line_plot(
                    ui,
                    "Torque (N·m) against rpm — drag a point",
                    &[("torque".into(), torque)],
                    "rpm",
                );
                for (i, (rpm, nm)) in drivetrain.engine.torque_curve.iter().enumerate() {
                    let at = frame.to_screen(*rpm, *nm);
                    let id = ui.id().with(("vt-torque-point", i));
                    let response =
                        ui.interact(Rect::from_center_size(at, Vec2::splat(10.0)), id, Sense::drag());
                    ui.painter().circle_filled(at, 4.0, SERIES[1]);
                    if response.drag_started() || response.drag_stopped() {
                        *ctx.drag_ended = true;
                    }
                    if response.dragged()
                        && let Some(pointer) = response.interact_pointer_pos()
                    {
                        let value = frame.y_at(pointer.y).max(0.0);
                        ctx.edits.push((
                            format!("drivetrain.engine.torque_curve.{i}.1"),
                            json!(value),
                            true,
                        ));
                    }
                }
                line_plot(ui, "Power (kW) against rpm", &[("power".into(), power)], "rpm");
            }
            let gears: Vec<(String, Vec<(f32, f32)>)> = plots::wheel_force_per_gear(config, 41)
                .into_iter()
                .map(|curve| (format!("gear {}", curve.gear), curve.points))
                .collect();
            line_plot(ui, "Wheel force (N) against speed, per gear", &gears, "km/h");
            if let Some(tire) = config.wheels.get(wheel).and_then(|w| w.tire_model.slip()) {
                let points = plots::tire_side_force(tire, tire.nominal_load, 0.5, 41);
                line_plot(
                    ui,
                    &format!(
                        "Wheel {wheel}: side force (N) against slip angle at {:.0} N",
                        tire.nominal_load
                    ),
                    &[("side force".into(), points)],
                    "rad",
                );
            } else {
                ui.weak(format!("wheel {wheel} has the damper tire: no slip curve"));
            }
            if let Some(suspension) = plots::suspension_travel(config, wheel, 21) {
                let frame = line_plot(
                    ui,
                    &format!("Wheel {wheel}: spring force (N) against compression"),
                    &[("spring".into(), suspension.points.clone())],
                    "m",
                );
                let x = frame.to_screen(suspension.static_compression, 0.0).x;
                ui.painter()
                    .vline(x, frame.rect.y_range(), Stroke::new(1.0, SERIES[2]));
                ui.weak(format!(
                    "settles at {:.3} m of {:.3} m travel",
                    suspension.static_compression, suspension.travel
                ));
            }
        });
}

/// Apply in play (`editor.vehicle_tuning_apply`) and the telemetry strip.
fn play_section(ui: &mut Ui, panel: &mut VehicleTuning, playing: bool) {
    egui::CollapsingHeader::new("Play")
        .default_open(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        playing && panel.pending_apply.is_none(),
                        egui::Button::new("Apply"),
                    )
                    .on_hover_text("send the edited config to the play car without a respawn")
                    .clicked()
                {
                    panel.pending_apply = Some(panel.target);
                }
                if !playing {
                    ui.weak("start play to apply");
                }
                match &panel.last_apply {
                    Some(Ok(v)) => {
                        ui.weak(format!("applied to {}", v["entity"]));
                    }
                    Some(Err(e)) => {
                        ui.colored_label(REFUSED, crate::data_panels::sentence(&e.message))
                            .on_hover_text(format!("Agent error code: {}", e.code));
                    }
                    None => {}
                }
            });
            let Some(last) = panel.telemetry.last() else {
                ui.weak("no telemetry yet");
                return;
            };
            ui.label(format!(
                "{:.1} km/h · {:.0} rpm · gear {}",
                last.speed_kmh, last.rpm, last.gear
            ));
            let speed: Vec<(f32, f32)> = panel
                .telemetry
                .iter()
                .map(|f| (f.steps as f32, f.speed_kmh))
                .collect();
            let rpm: Vec<(f32, f32)> = panel.telemetry.iter().map(|f| (f.steps as f32, f.rpm)).collect();
            line_plot(ui, "Speed (km/h)", &[("speed".into(), speed)], "step");
            line_plot(ui, "Engine (rpm)", &[("rpm".into(), rpm)], "step");
            egui::Grid::new("vt-wheels").striped(true).show(ui, |ui| {
                ui.strong("wheel");
                ui.strong("slip ratio");
                ui.strong("slip angle");
                ui.strong("load N");
                ui.end_row();
                for (i, (ratio, angle, load)) in last.wheels.iter().enumerate() {
                    ui.label(i.to_string());
                    ui.label(format!("{ratio:.3}"));
                    ui.label(format!("{angle:.3}"));
                    ui.label(format!("{load:.0}"));
                    ui.end_row();
                }
            });
        });
}

/// Recorded runs side by side (`editor.vehicle_tuning_run`).
fn runs_section(ui: &mut Ui, panel: &mut VehicleTuning, local: &mut VehicleTuningUi) {
    egui::CollapsingHeader::new("Runs")
        .default_open(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut local.seconds)
                        .range(1.0..=120.0)
                        .suffix(" s"),
                );
                for script in [ProvingScript::Accel, ProvingScript::Skidpad] {
                    if ui.button(format!("Run {}", script.name())).clicked()
                        && let Err(e) = panel.run(script, local.seconds, None)
                    {
                        panel.error = Some(e);
                    }
                }
            });
            // editor.vehicle_tuning_run { track, tow }: kept for later runs.
            ui.horizontal(|ui| {
                ui.label("track");
                egui::ComboBox::from_id_salt("vt-track")
                    .selected_text(panel.track.name())
                    .show_ui(ui, |ui| {
                        for track in ProvingTrack::ALL {
                            if ui.selectable_label(panel.track == track, track.name()).clicked() {
                                panel.track = track;
                            }
                        }
                    });
                let can_tow = panel.others.trailer.asset.is_some();
                let mut tow = panel.tow && can_tow;
                if ui
                    .add_enabled(can_tow, egui::Checkbox::new(&mut tow, "tow the open trailer"))
                    .changed()
                {
                    panel.tow = tow;
                }
            });
            if panel.runs.is_empty() {
                ui.weak("no runs yet");
                return;
            }
            egui::Grid::new("vt-runs").striped(true).show(ui, |ui| {
                for head in [
                    "run", "script", "0-100 s", "top km/h", "lat g", "upshifts", "config", "track",
                    "contacts", "trailer",
                ] {
                    ui.strong(head);
                }
                ui.end_row();
                for run in &panel.runs {
                    ui.label(run.label.as_str());
                    ui.label(run.run.script.name());
                    ui.label(
                        run.run
                            .zero_to_100_s
                            .map_or("—".to_string(), |t| format!("{t:.2}")),
                    );
                    ui.label(format!("{:.1}", run.run.top_speed_kmh));
                    ui.label(format!("{:.2}", run.run.max_lateral_g));
                    ui.label(run.run.upshifts.to_string());
                    ui.weak(&run.config_digest[..8.min(run.config_digest.len())]);
                    ui.label(run.track.name());
                    let (min, max, changes) = run.contacts;
                    ui.label(format!("{min}-{max} ({changes} changes)"));
                    ui.label(run.trailer.as_ref().map_or("—".to_string(), |(t, _)| {
                        format!(
                            "{:.0} kg, {:.1}°",
                            t.chassis_mass,
                            t.max_articulation.to_degrees()
                        )
                    }));
                    ui.end_row();
                }
            });
            let traces: Vec<(String, Vec<(f32, f32)>)> = panel
                .runs
                .iter()
                .map(|run| {
                    (
                        run.label.clone(),
                        run.run.samples.iter().map(|s| (s.time, s.speed_kmh)).collect(),
                    )
                })
                .collect();
            line_plot(ui, "Speed (km/h) against time, every run", &traces, "s");
        });
}

/// The trailer and aircraft sections.
fn other_sections(
    ui: &mut Ui,
    panel: &mut VehicleTuning,
    local: &mut VehicleTuningUi,
    project: Option<&Path>,
) {
    egui::CollapsingHeader::new("Trailer (.trailer.ron)")
        .id_salt("vt-trailer")
        .show(ui, |ui| {
            doc_section(
                ui,
                &mut panel.others.trailer,
                &mut local.trailer_path,
                project,
                AssetKind::Trailer,
            );
            if panel.others.trailer.asset.is_some() {
                ui.weak("Tick \"tow the open trailer\" in Runs to hitch it behind the car.");
            }
        });
    egui::CollapsingHeader::new("Aircraft (.aircraft.ron)")
        .id_salt("vt-aircraft")
        .show(ui, |ui| {
            doc_section(
                ui,
                &mut panel.others.aircraft,
                &mut local.aircraft_path,
                project,
                AssetKind::Aircraft,
            );
            flights_section(ui, panel, local);
        });
    egui::CollapsingHeader::new("Bike (.bike.ron)")
        .id_salt("vt-bike")
        .show(ui, |ui| {
            doc_section(
                ui,
                &mut panel.others.bike,
                &mut local.bike_path,
                project,
                AssetKind::Bike,
            );
            ride_section(ui, panel, local, AssetKind::Bike);
        });
    egui::CollapsingHeader::new("Hover hull (.hoverhull.ron)")
        .id_salt("vt-hoverhull")
        .show(ui, |ui| {
            doc_section(
                ui,
                &mut panel.others.hoverhull,
                &mut local.hover_path,
                project,
                AssetKind::HoverHull,
            );
            ride_section(ui, panel, local, AssetKind::HoverHull);
        });
}

/// The proving-ground ride row of the bike or hover-hull section (`editor.vehicle_tuning_ride`): the
/// held input, **Ride 1 s** (60 fixed steps), **Respawn**, and the ride's state.
fn ride_section(ui: &mut Ui, panel: &mut VehicleTuning, local: &mut VehicleTuningUi, kind: AssetKind) {
    use crate::vehicle_tuning::assets::RideInput;
    let bike = kind == AssetKind::Bike;
    let open = if bike {
        panel.others.bike.asset.is_some()
    } else {
        panel.others.hoverhull.asset.is_some()
    };
    let mut request = None;
    ui.horizontal(|ui| {
        if bike {
            let [steer, speed] = &mut local.bike_ride;
            ui.add(
                egui::DragValue::new(steer)
                    .range(-1.2..=1.2)
                    .speed(0.01)
                    .prefix("steer "),
            );
            ui.add(egui::DragValue::new(speed).range(-150.0..=150.0).suffix(" m/s"));
        } else {
            let [throttle, strafe, steer] = &mut local.hover_ride;
            ui.add(
                egui::DragValue::new(throttle)
                    .range(-1.0..=1.0)
                    .speed(0.01)
                    .prefix("throttle "),
            );
            ui.add(
                egui::DragValue::new(strafe)
                    .range(-1.0..=1.0)
                    .speed(0.01)
                    .prefix("strafe "),
            );
            ui.add(
                egui::DragValue::new(steer)
                    .range(-1.0..=1.0)
                    .speed(0.01)
                    .prefix("steer "),
            );
        }
        if ui.add_enabled(open, egui::Button::new("Ride 1 s")).clicked() {
            request = Some(false);
        }
        if ui.add_enabled(open, egui::Button::new("Respawn")).clicked() {
            request = Some(true);
        }
    });
    if let Some(respawn) = request {
        let input = if bike {
            let [steer, speed] = local.bike_ride;
            RideInput {
                steer,
                speed,
                drive: 1.0,
                ..RideInput::default()
            }
        } else {
            let [throttle, strafe, steer] = local.hover_ride;
            RideInput {
                steer,
                throttle,
                strafe,
                ..RideInput::default()
            }
        };
        let steps = if respawn { 1 } else { 60 };
        if let Err(e) = panel.ride(kind, steps, input, respawn) {
            panel.refuse_in(kind, e);
        }
    }
    if bike {
        match &panel.others.bike_ride {
            Some(ride) => {
                let s = ride.state;
                ui.label(format!(
                    "step {}: lean {:.3} rad, speed {:.1} m/s{}",
                    ride.steps,
                    s.lean,
                    s.speed,
                    if s.fallen { ", fallen" } else { "" }
                ));
            }
            None => {
                ui.weak("not riding: Ride spawns the bike upright on the flat proving ground");
            }
        }
    } else {
        match &panel.others.hover_ride {
            Some(ride) => {
                let s = &ride.state;
                let [vx, _, vz] = s.velocity;
                ui.label(format!(
                    "tick {}: {:.1} m/s, yaw {:.2} rad, altitude {:.2} m{}",
                    ride.steps,
                    (vx * vx + vz * vz).sqrt(),
                    s.yaw,
                    s.altitude,
                    if s.airborne { ", airborne" } else { "" }
                ));
            }
            None => {
                ui.weak("not riding: Ride spawns the hull at its ride height over the flat proving ground");
            }
        }
    }
}

/// One document's bar (path, Open, New, Save, Undo, Redo), status and form; every button has the
/// `editor.vehicle_tuning_*` method with `asset` set to the kind.
fn doc_section<A: TunedAsset>(
    ui: &mut Ui,
    doc: &mut TuningDoc<A>,
    path: &mut String,
    project: Option<&Path>,
    kind: AssetKind,
) {
    ui.horizontal(|ui| {
        ui.text_edit_singleline(path);
        if ui.button("Open").clicked()
            && let Err(e) = doc.open(project, path, false)
        {
            doc.error = Some(e);
        }
        if ui.button("New").clicked()
            && let Err(e) = doc.open(project, path, true)
        {
            doc.error = Some(e);
        }
        let open = doc.asset.is_some();
        if ui.add_enabled(open, egui::Button::new("Save")).clicked()
            && let Err(e) = doc.save(project, None)
        {
            doc.error = Some(e);
        }
        if ui
            .add_enabled(!doc.undo.is_empty(), egui::Button::new("Undo"))
            .clicked()
        {
            doc.undo();
        }
        if ui
            .add_enabled(!doc.redo.is_empty(), egui::Button::new("Redo"))
            .clicked()
        {
            doc.redo();
        }
        if doc.is_dirty() {
            ui.weak("● unsaved");
        }
    });
    if let Some(e) = &doc.error {
        let field = e.field.as_deref().map(|f| format!(" `{f}`")).unwrap_or_default();
        ui.colored_label(REFUSED, format!("Refused{field}: {}", e.message))
            .on_hover_text(format!("Agent error code: {}", e.code));
    }
    if let Some(notice) = &doc.notice {
        ui.weak(notice);
    }
    let Some(asset) = doc.asset.clone() else {
        ui.weak(format!(
            "Open a {} under the project, or New to start one.",
            A::EXT
        ));
        return;
    };
    match asset.validate() {
        Ok(()) => {
            ui.weak("valid");
        }
        Err(e) => {
            ui.colored_label(REFUSED, crate::data_panels::sentence(&e.message))
                .on_hover_text(format!("Agent error code: {}", e.code));
        }
    }
    let config = serde_json::to_value(asset.config()).unwrap_or(Value::Null);
    let refused = doc.error.as_ref().and_then(|e| e.field.clone());
    let mut edits: Vec<Edit> = Vec::new();
    let mut drag_ended = false;
    {
        let mut ctx = FieldCtx {
            edits: &mut edits,
            drag_ended: &mut drag_ended,
            refused: refused.as_deref(),
        };
        let mut name = asset.name().to_string();
        ui.horizontal(|ui| {
            ui.label("name");
            if ui.text_edit_singleline(&mut name).lost_focus() && name != asset.name() {
                ctx.edits.push(("name".to_string(), json!(name), false));
            }
        });
        ui.push_id(("vt-doc", kind.name()), |ui| {
            object_fields(ui, &mut ctx, "", &config);
        });
    }
    for (field, value, drag) in edits {
        if let Err(e) = doc.set(&field, value, drag) {
            doc.error = Some(e);
        }
    }
    if drag_ended {
        doc.end_drag();
    }
}

/// The flight test row and the recorded flights (`editor.vehicle_tuning_fly`).
fn flights_section(ui: &mut Ui, panel: &mut VehicleTuning, local: &mut VehicleTuningUi) {
    let open = panel.others.aircraft.asset.is_some();
    ui.horizontal(|ui| {
        let [speed, altitude, seconds] = &mut local.flight;
        ui.add(egui::DragValue::new(speed).range(5.0..=300.0).suffix(" m/s"));
        ui.add(egui::DragValue::new(altitude).range(0.0..=15_000.0).suffix(" m"));
        ui.add(egui::DragValue::new(seconds).range(1.0..=120.0).suffix(" s"));
        if ui.add_enabled(open, egui::Button::new("Fly")).clicked() {
            let [speed, altitude, seconds] = local.flight;
            if let Err(e) = panel.fly(speed, altitude, seconds, None) {
                panel.others.aircraft.error = Some(e);
            }
        }
    });
    if panel.others.flights.is_empty() {
        ui.weak("no flights yet: Fly trims the aircraft for level flight and flies the trim");
        return;
    }
    egui::Grid::new("vt-flights").striped(true).show(ui, |ui| {
        for head in [
            "flight",
            "m/s",
            "alt m",
            "alpha °",
            "throttle",
            "Δalt m",
            "Δspeed %",
            "bank °",
            "mass kg",
        ] {
            ui.strong(head);
        }
        ui.end_row();
        for f in &panel.others.flights {
            ui.label(f.label.as_str());
            ui.label(format!("{:.0}", f.speed));
            ui.label(format!("{:.0}", f.altitude));
            ui.label(format!("{:.2}", f.alpha_deg));
            ui.label(format!("{:.2}", f.throttle));
            ui.label(format!("{:.2}", f.max_altitude_change_m));
            ui.label(format!("{:.2}", f.max_speed_change_pct));
            ui.label(format!("{:.2}", f.max_bank_deg));
            ui.label(format!("{:.0}", f.mass));
            ui.end_row();
        }
    });
}
