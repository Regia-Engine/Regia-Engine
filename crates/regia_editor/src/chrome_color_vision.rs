// SPDX-License-Identifier: MPL-2.0
//! View ▸ Color Vision (spec addendum § 21, ADR-7838): a color-vision filter for the editor
//! chrome and the viewport at once, so a designer checks panels and the scene with the same eyes.
//!
//! The filter is `regia_render::color_vision::ColorVisionFilter` (the published simulation
//! matrices of Machado, Oliveira and Fernandes 2009 and the Fidaner, Lin and Ozguven compensation,
//! ADR-2646). The chrome goes through it per vertex: after egui tessellates a frame,
//! [`filter_paint_jobs`] maps every vertex color of the meshes that sample egui's font atlas (text,
//! fills, strokes, icons) through `ColorVisionFilter::apply_display`, with the alpha kept. Meshes
//! that sample a user texture (the viewport image, thumbnails, previews) keep their pixels: the
//! viewport gets the same filter as a LUT on its views ([`apply_viewport_color_vision`], the
//! runtime's `render.color_vision` path), and pictures are content, not chrome.
//!
//! [`EditorColorVision`] is the editor's state; [`set_color_vision`] changes the chrome and the
//! viewport together (the View menu and `editor.color_vision` both call it). Off by default, and
//! off is the identity: nothing is touched.

use std::collections::HashMap;

use egui::Color32;
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_render::color_vision::{ColorVision, ColorVisionDeficiency, ColorVisionFilter, ColorVisionMode};
use serde_json::{Value, json};

/// The editor's color-vision filter (chrome and viewport). Off by default.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct EditorColorVision {
    /// The filter the chrome and the viewport use.
    pub filter: ColorVisionFilter,
}

/// One View ▸ Color Vision entry: its menu label and filter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorVisionChoice {
    /// The submenu label (US English).
    pub label: &'static str,
    /// The filter it selects.
    pub mode: ColorVisionMode,
    /// The deficiency it models.
    pub deficiency: ColorVisionDeficiency,
}

/// The View ▸ Color Vision entries, in menu order.
pub const CHOICES: [ColorVisionChoice; 7] = [
    ColorVisionChoice {
        label: "Off",
        mode: ColorVisionMode::Off,
        deficiency: ColorVisionDeficiency::Deuteranopia,
    },
    ColorVisionChoice {
        label: "Simulate protanopia",
        mode: ColorVisionMode::Simulate,
        deficiency: ColorVisionDeficiency::Protanopia,
    },
    ColorVisionChoice {
        label: "Simulate deuteranopia",
        mode: ColorVisionMode::Simulate,
        deficiency: ColorVisionDeficiency::Deuteranopia,
    },
    ColorVisionChoice {
        label: "Simulate tritanopia",
        mode: ColorVisionMode::Simulate,
        deficiency: ColorVisionDeficiency::Tritanopia,
    },
    ColorVisionChoice {
        label: "Compensate for protanopia",
        mode: ColorVisionMode::Compensate,
        deficiency: ColorVisionDeficiency::Protanopia,
    },
    ColorVisionChoice {
        label: "Compensate for deuteranopia",
        mode: ColorVisionMode::Compensate,
        deficiency: ColorVisionDeficiency::Deuteranopia,
    },
    ColorVisionChoice {
        label: "Compensate for tritanopia",
        mode: ColorVisionMode::Compensate,
        deficiency: ColorVisionDeficiency::Tritanopia,
    },
];

impl ColorVisionChoice {
    /// The agent parameters of this entry (`editor.color_vision`).
    #[must_use]
    pub fn params(&self) -> Value {
        if self.mode == ColorVisionMode::Off {
            json!({ "mode": "off" })
        } else {
            json!({ "mode": self.mode.as_str(), "deficiency": self.deficiency.as_str() })
        }
    }

    /// Whether `filter` is this entry (any off filter is Off).
    #[must_use]
    pub fn is(&self, filter: &ColorVisionFilter) -> bool {
        if filter.is_off() {
            return self.mode == ColorVisionMode::Off;
        }
        filter.mode == self.mode && filter.deficiency == self.deficiency
    }
}

/// The editor's current filter (off when unset).
#[must_use]
pub fn current(world: &World) -> ColorVisionFilter {
    world
        .get_resource::<EditorColorVision>()
        .map(|v| v.filter)
        .unwrap_or_default()
}

/// Sets the editor's filter for the chrome and the viewport (the viewport's `ColorVision` resource,
/// as `render.color_vision` sets it).
pub fn set_color_vision(world: &mut World, filter: ColorVisionFilter) {
    world.insert_resource(EditorColorVision { filter });
    world.get_resource_or_insert_with(ColorVision::default).filter = filter;
}

/// Puts the viewport's color-vision filter into every view of a frame the editor is about to render
/// (`crate::render` builds its views after extraction, so the runtime's `apply_color_vision`
/// system never sees them). Does nothing while the filter is off or unset.
pub fn apply_viewport_color_vision(world: &mut World, extracted: &mut regia_render::ExtractedFrame) {
    let Some(mut vision) = world.get_resource_mut::<ColorVision>() else {
        return;
    };
    if vision.filter.is_off() {
        return;
    }
    for view in &mut extracted.views {
        view.post.lut = vision.view_lut(view.post.lut.as_ref());
    }
}

/// One vertex color through the filter: unpremultiplied, display-encoded sRGB in, the same out,
/// alpha kept.
#[must_use]
pub fn filter_color(filter: &ColorVisionFilter, color: Color32) -> Color32 {
    if filter.is_off() || color.a() == 0 {
        return color;
    }
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    let out = filter.apply_display([f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0]);
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(byte(out[0]), byte(out[1]), byte(out[2]), a)
}

/// Filters the chrome of a tessellated frame in place: every vertex color of the meshes that sample
/// egui's own atlas (`TextureId::default()`). User textures (the viewport, thumbnails) and paint
/// callbacks are left alone. Returns how many vertices changed color.
pub fn filter_primitives(filter: &ColorVisionFilter, jobs: &mut [egui::ClippedPrimitive]) -> usize {
    if filter.is_off() {
        return 0;
    }
    let _span = tracing::trace_span!(target: "regia_editor", "chrome_color_vision").entered();
    // A frame has thousands of vertices but few distinct colors.
    let mut cache: HashMap<Color32, Color32> = HashMap::new();
    let mut changed = 0;
    for job in jobs {
        let egui::epaint::Primitive::Mesh(mesh) = &mut job.primitive else {
            continue;
        };
        if mesh.texture_id != egui::TextureId::default() {
            continue;
        }
        for vertex in &mut mesh.vertices {
            let out = *cache
                .entry(vertex.color)
                .or_insert_with(|| filter_color(filter, vertex.color));
            if out != vertex.color {
                vertex.color = out;
                changed += 1;
            }
        }
    }
    changed
}

/// [`filter_primitives`] with the world's [`EditorColorVision`] (the editor binary calls it
/// between tessellation and the egui render pass).
pub fn filter_paint_jobs(world: &World, jobs: &mut [egui::ClippedPrimitive]) -> usize {
    filter_primitives(&current(world), jobs)
}

/// Draws the View ▸ Color Vision submenu: one selectable entry per [`CHOICES`] row, the active one
/// selected. Keyboard: the menu is reached with the menu bar's keys and each entry is a focusable
/// button.
pub fn view_menu(ui: &mut egui::Ui, world: &mut World) {
    let active = current(world);
    let mut picked = None;
    crate::agent_parity::menu(ui, "Color Vision", |ui| {
        for choice in &CHOICES {
            if crate::agent_parity::action_selectable(
                ui,
                &crate::agent_parity::VIEW_COLOR_VISION,
                choice.label,
                choice.is(&active),
            )
            .clicked()
            {
                picked = Some(*choice);
                ui.close();
            }
        }
    });
    if let Some(choice) = picked {
        let filter = ColorVisionFilter {
            mode: choice.mode,
            deficiency: choice.deficiency,
            severity: 1.0,
        };
        set_color_vision(world, filter);
    }
}

fn bad(code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn snapshot(world: &World) -> Value {
    let filter = current(world);
    let viewport = world.get_resource::<ColorVision>().map(|v| v.filter);
    json!({
        "mode": filter.mode.as_str(),
        "deficiency": filter.deficiency.as_str(),
        "severity": filter.severity(),
        "active": !filter.is_off(),
        "chrome": !filter.is_off(),
        "viewport": viewport.is_some_and(|v| !v.is_off()),
        "choices": CHOICES.iter().map(|c| c.label).collect::<Vec<_>>(),
    })
}

/// `editor.color_vision { mode?, deficiency?, severity? }`: with no parameters it only reports.
fn editor_color_vision(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let object = match params {
        Value::Null => serde_json::Map::new(),
        Value::Object(object) => object.clone(),
        _ => {
            return Err(bad(
                "color_vision_bad_params",
                "expected an object",
                "pass { mode?, deficiency?, severity? }",
            ));
        }
    };
    if let Some(key) = object
        .keys()
        .find(|k| !matches!(k.as_str(), "mode" | "deficiency" | "severity"))
    {
        return Err(bad(
            "color_vision_bad_params",
            format!("unknown parameter `{key}`"),
            "pass { mode?, deficiency?, severity? }",
        ));
    }
    let text = |key: &str| -> Result<Option<String>, RpcError> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(bad(
                "color_vision_bad_params",
                format!("`{key}` must be a string"),
                "pass { mode?, deficiency?, severity? }",
            )),
        }
    };
    let mode = text("mode")?
        .map(|m| {
            ColorVisionMode::parse(&m).ok_or_else(|| {
                bad(
                    "color_vision_bad_mode",
                    format!("unknown mode `{m}`"),
                    "\"off\", \"simulate\" or \"compensate\"",
                )
            })
        })
        .transpose()?;
    let deficiency = text("deficiency")?
        .map(|d| {
            ColorVisionDeficiency::parse(&d).ok_or_else(|| {
                bad(
                    "color_vision_bad_deficiency",
                    format!("unknown deficiency `{d}`"),
                    "\"protanopia\", \"deuteranopia\" or \"tritanopia\"",
                )
            })
        })
        .transpose()?;
    let severity = match object.get("severity") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_f64() {
            Some(s) if s.is_finite() && (0.0..=1.0).contains(&s) => Some(s as f32),
            _ => {
                return Err(bad(
                    "color_vision_bad_severity",
                    "`severity` must be a number from 0 to 1",
                    "1 is full dichromacy; 0.5 approximates a milder anomaly",
                ));
            }
        },
    };
    if mode.is_some() || deficiency.is_some() || severity.is_some() {
        let now = current(world);
        set_color_vision(
            world,
            ColorVisionFilter {
                mode: mode.unwrap_or(now.mode),
                deficiency: deficiency.unwrap_or(now.deficiency),
                severity: severity.unwrap_or(now.severity),
            },
        );
    }
    Ok(snapshot(world))
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// `editor.color_vision` (`book/agents/theme.md`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.color_vision",
            tool: "color_vision",
            description: "Read or change the editor's color-vision filter (View ▸ Color Vision): simulate or \
                compensate protanopia, deuteranopia or tritanopia on the editor chrome (every panel color) and \
                the viewport at once, with the published matrices render.color_vision uses. No parameters only \
                reports. Errors: color_vision_bad_params, color_vision_bad_mode, color_vision_bad_deficiency, \
                color_vision_bad_severity.",
            params: &[
                ParamSchema {
                    name: "mode",
                    kind: "string",
                    required: false,
                    description: "\"off\" | \"simulate\" | \"compensate\".",
                },
                ParamSchema {
                    name: "deficiency",
                    kind: "string",
                    required: false,
                    description: "\"protanopia\" | \"deuteranopia\" | \"tritanopia\".",
                },
                ParamSchema {
                    name: "severity",
                    kind: "number",
                    required: false,
                    description: "0 to 1 (default 1, full dichromacy).",
                },
            ],
            result: "{ mode, deficiency, severity, active, chrome, viewport, choices: [label] }",
            mutating: true,
            permission: Permission::EditScene,
        },
        editor_color_vision as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh_job(texture: egui::TextureId, colors: &[Color32]) -> egui::ClippedPrimitive {
        let mut mesh = egui::epaint::Mesh::with_texture(texture);
        for &color in colors {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: egui::Pos2::ZERO,
                uv: egui::Pos2::ZERO,
                color,
            });
        }
        egui::ClippedPrimitive {
            clip_rect: egui::Rect::EVERYTHING,
            primitive: egui::epaint::Primitive::Mesh(mesh),
        }
    }

    fn colors(job: &egui::ClippedPrimitive) -> Vec<Color32> {
        match &job.primitive {
            egui::epaint::Primitive::Mesh(mesh) => mesh.vertices.iter().map(|v| v.color).collect(),
            egui::epaint::Primitive::Callback(_) => Vec::new(),
        }
    }

    #[test]
    fn off_leaves_every_vertex_alone() {
        let red = Color32::from_rgb(220, 30, 30);
        let mut jobs = vec![mesh_job(egui::TextureId::default(), &[red])];
        assert_eq!(filter_primitives(&ColorVisionFilter::default(), &mut jobs), 0);
        assert_eq!(colors(&jobs[0]), [red]);
    }

    /// The chrome goes through the same filter as the renderer's LUT: one vertex color equals
    /// `apply_display` of it, red and green collapse toward one hue under deuteranopia, gray stays
    /// gray, alpha is kept, and a user texture (the viewport image) is not touched.
    #[test]
    fn simulation_moves_chrome_colors_through_the_published_matrices() {
        let filter = ColorVisionFilter::new(ColorVisionMode::Simulate, ColorVisionDeficiency::Deuteranopia);
        let red = Color32::from_rgb(220, 40, 40);
        let green = Color32::from_rgb(40, 200, 60);
        let gray = Color32::from_gray(128);
        let faded = Color32::from_rgba_unmultiplied(220, 40, 40, 128);
        let mut jobs = vec![
            mesh_job(egui::TextureId::default(), &[red, green, gray, faded]),
            mesh_job(egui::TextureId::User(7), &[red]),
        ];
        let changed = filter_primitives(&filter, &mut jobs);
        assert!(changed >= 3, "{changed}");
        let out = colors(&jobs[0]);
        let want = filter.apply_display([220.0 / 255.0, 40.0 / 255.0, 40.0 / 255.0]);
        for (got, want) in [out[0].r(), out[0].g(), out[0].b()].into_iter().zip(want) {
            assert!(
                (f32::from(got) - want * 255.0).abs() <= 1.0,
                "{out:?} vs {want:?}"
            );
        }
        // Red and green are much closer after the simulation (the confusion it shows).
        let dist = |a: Color32, b: Color32| {
            let d = |x: u8, y: u8| (f32::from(x) - f32::from(y)).powi(2);
            (d(a.r(), b.r()) + d(a.g(), b.g()) + d(a.b(), b.b())).sqrt()
        };
        assert!(dist(out[0], out[1]) < dist(red, green) * 0.7);
        assert!(out[2].r().abs_diff(128) <= 1 && out[2].g().abs_diff(128) <= 1);
        assert_eq!(out[3].a(), 128, "alpha kept");
        assert_eq!(colors(&jobs[1]), [red], "user textures keep their pixels");
    }

    #[test]
    fn every_menu_choice_round_trips_through_the_agent_method() {
        let mut world = World::new();
        let out = editor_color_vision(&mut world, &json!({})).expect("report");
        assert_eq!(out["mode"], "off");
        assert_eq!(out["active"], false);
        for choice in &CHOICES {
            let out = editor_color_vision(&mut world, &choice.params()).expect("set");
            assert_eq!(out["mode"], choice.mode.as_str());
            assert!(choice.is(&current(&world)), "{}", choice.label);
            assert_eq!(out["viewport"], json!(choice.mode != ColorVisionMode::Off));
            assert_eq!(
                world.resource::<ColorVision>().filter,
                current(&world),
                "chrome and viewport share the filter"
            );
        }
        for (params, code) in [
            (json!({ "mode": "blur" }), "color_vision_bad_mode"),
            (
                json!({ "deficiency": "achromatopsia" }),
                "color_vision_bad_deficiency",
            ),
            (json!({ "severity": 2 }), "color_vision_bad_severity"),
            (json!({ "chrome": true }), "color_vision_bad_params"),
            (json!(5), "color_vision_bad_params"),
        ] {
            let err = editor_color_vision(&mut world, &params).expect_err("refused");
            assert_eq!(
                err.data.as_ref().and_then(|d| d.get("code")),
                Some(&json!(code)),
                "{params}"
            );
        }
    }
}
