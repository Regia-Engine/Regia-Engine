// SPDX-License-Identifier: MPL-2.0
//! The color field's eyedropper (P-133, ADR-8106): pick a pixel anywhere in the editor window,
//! the viewport's rendered frame included, into a color field.
//!
//! The drop button beside a color field's swatch arms it ([`arm`]). While armed, a transparent
//! layer over the whole window ([`frame`]) takes the next click: the window under it is not
//! hovered or clicked, the pointer is a crosshair and a ring marks the pick point. From the
//! keyboard the arrow keys move the ring (Shift: ten points a step) and Enter or Space picks;
//! Esc or a right click cancels.
//!
//! A pick does not read the screen back itself: it asks the window's present for a copy of the
//! finished frame ([`regia_render::ScreenshotRequests`], what `seeing.screenshot` uses), and the
//! next frame after the copy arrives writes the pixel's color ([`resolve`]): sRGB bytes decoded
//! to the linear color the field stores, the field's alpha (and a vector's `w`) kept, on every
//! selected entity that has the component, as one undo step. Hosts without a present loop answer
//! the request with [`crate::window_capture::serve_screenshot_requests`].
//!
//! Agents pick with `editor.color_eyedropper` (`book/agents/inspector.md`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ecs::reflect::{PartialReflect, ReflectPath, ReflectRef};
use regia_render::CapturedImage;
use serde_json::{Value, json};

use super::color;
use super::reflect_ui::Change;
use crate::data_panels::PanelError;
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/inspector.md";

/// Points the ring moves per arrow key (ten with Shift).
const KEY_STEP: f32 = 1.0;
/// The ring's radius, in points (it scales with the UI scale like every other point size).
const RING_RADIUS: f32 = 7.0;

/// Where a requested read-back lands (the present's callback runs off the world).
type Slot = Arc<Mutex<Option<Result<CapturedImage, String>>>>;

/// A pick waiting for its read-back.
#[derive(Debug, Clone)]
pub struct PendingPick {
    /// The window pixel (physical pixels, top-left origin).
    pub pixel: [u32; 2],
    slot: Slot,
}

/// The armed eyedropper of one color field.
#[derive(Resource, Debug, Clone)]
pub struct ColorEyedropper {
    /// The entities the pick is written to (those without the component are skipped).
    pub entities: Vec<Entity>,
    /// The component's type path or short name.
    pub component: String,
    /// The color field's reflect path.
    pub path: String,
    /// The keyboard pick point, in points (the pointer moves it too).
    pub cursor: Option<egui::Pos2>,
    /// The pick waiting for the window's read-back.
    pub pending: Option<PendingPick>,
    /// The egui frame the overlay last acted in (egui may run the Inspector twice a frame).
    acted_frame: Option<u64>,
}

/// The last finished pick (agents read it from `editor.color_eyedropper`).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct LastColorPick {
    /// The window pixel.
    pub pixel: [u32; 2],
    /// The color written, linear RGBA (`None` when the pick failed).
    pub color: Option<[f32; 4]>,
    /// Why the pick wrote nothing.
    pub error: Option<String>,
    /// Undo depth after the write.
    pub undoable: usize,
}

/// Arms the eyedropper for the color field at `path` of `component` on `entities` (a pick in
/// progress is dropped).
pub fn arm(world: &mut World, entities: Vec<Entity>, component: &str, path: &str) {
    world.insert_resource(ColorEyedropper {
        entities,
        component: component.to_owned(),
        path: path.to_owned(),
        cursor: None,
        pending: None,
        acted_frame: None,
    });
}

/// Disarms the eyedropper.
pub fn cancel(world: &mut World) {
    world.remove_resource::<ColorEyedropper>();
}

/// The reflect path of the field whose eyedropper is armed, when it belongs to `component` of
/// `entity` (the drawer shows the armed state).
#[must_use]
pub fn armed_path(world: &World, entity: Option<Entity>, component: &str) -> Option<String> {
    world
        .get_resource::<ColorEyedropper>()
        .filter(|armed| armed.component == component && entity.is_some_and(|e| armed.entities.contains(&e)))
        .map(|armed| armed.path.clone())
}

/// Asks the window's next present for its frame and remembers `pixel` (physical pixels) as the
/// pick. Nothing happens when the eyedropper is not armed.
pub fn request_pick(world: &mut World, pixel: [u32; 2]) {
    if world.get_resource::<ColorEyedropper>().is_none() {
        return;
    }
    let slot: Slot = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&slot);
    world
        .get_resource_or_insert_with(regia_render::ScreenshotRequests::default)
        .request(
            None,
            Box::new(move |result| {
                if let Ok(mut held) = sink.lock() {
                    *held = Some(result);
                }
            }),
        );
    if let Some(mut armed) = world.get_resource_mut::<ColorEyedropper>() {
        armed.pending = Some(PendingPick { pixel, slot });
    }
}

/// sRGB bytes → the linear color the field stores (alpha from the field).
#[must_use]
pub fn linear_of(pixel: [u8; 4]) -> [f32; 3] {
    [0, 1, 2].map(|c| color::decode(f32::from(pixel[c]) / 255.0))
}

/// The `f32` lanes of a color-like value: an array of 3 or 4 `f32` (`[f32; 3]`, `[f32; 4]`) or a
/// glam `Vec3` / `Vec3A` / `Vec4`, read through reflection (the property read gives a dynamic copy,
/// not the concrete type). The flag says whether the lanes are named `x`, `y`, `z`, `w`.
fn color_lanes(value: &dyn PartialReflect) -> Option<(Vec<f32>, bool)> {
    let lane = |v: Option<&dyn PartialReflect>| v.and_then(|v| v.try_downcast_ref::<f32>().copied());
    match value.reflect_ref() {
        ReflectRef::Array(array) if matches!(array.len(), 3 | 4) => {
            let lanes: Option<Vec<f32>> = (0..array.len()).map(|i| lane(array.get(i))).collect();
            lanes.map(|lanes| (lanes, false))
        }
        ReflectRef::Struct(fields) => {
            let glam_vector = value
                .get_represented_type_info()
                .map(|info| info.type_path())
                .is_some_and(|path| {
                    path.starts_with("glam::")
                        && matches!(path.rsplit("::").next(), Some("Vec3" | "Vec3A" | "Vec4"))
                });
            if !glam_vector {
                return None;
            }
            let names: &[&str] = if fields.field_len() == 4 {
                &["x", "y", "z", "w"]
            } else {
                &["x", "y", "z"]
            };
            let lanes: Option<Vec<f32>> = names.iter().map(|name| lane(fields.field(name))).collect();
            lanes.map(|lanes| (lanes, true))
        }
        _ => None,
    }
}

/// The color lanes `rgb` writes into `current` (its alpha and `w` stay): `(reflect path, value)`
/// for the three color lanes; `None` when `current` is not a color type ([`color_lanes`]).
fn color_value(current: &dyn PartialReflect, path: &str, rgb: [f32; 3]) -> Option<Vec<(String, f32)>> {
    let (_, named) = color_lanes(current)?;
    Some(
        (0..3)
            .map(|i| {
                let lane = if named {
                    let name = ["x", "y", "z"][i];
                    if path.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{path}.{name}")
                    }
                } else {
                    format!("{path}[{i}]")
                };
                (lane, rgb[i])
            })
            .collect(),
    )
}

/// Whether the field at `path` of `entity`'s `component` is a color the eyedropper can write.
#[must_use]
pub fn is_color_field(world: &World, entity: Entity, component: &str, path: &str) -> bool {
    crate::command::read_property(world, entity, component, path)
        .ok()
        .is_some_and(|current| color_value(current.as_ref(), path, [0.0; 3]).is_some())
}

/// Writes linear `rgb` into the color field at `path` of `component` on every entity of
/// `entities` that has it, keeping each entity's alpha, as one undo step. Returns the undo depth.
///
/// # Errors
///
/// `eyedropper_not_color` (no entity has a color field there) or the multi-edit's refusal.
pub fn write_color(
    world: &mut World,
    entities: &[Entity],
    component: &str,
    path: &str,
    rgb: [f32; 3],
) -> Result<usize, PanelError> {
    let holders: Vec<Entity> = entities
        .iter()
        .copied()
        .filter(|entity| crate::command::read_property(world, *entity, component, path).is_ok())
        .collect();
    let Some(&sample) = holders.first() else {
        return Err(PanelError::new(
            "eyedropper_not_color",
            format!("no entity has {component}.{path}"),
        ));
    };
    let field = crate::command::read_property(world, sample, component, path)
        .map_err(|e| PanelError::new("eyedropper_not_color", e.to_string()))?;
    let Some(lanes) = color_value(field.as_ref(), path, rgb) else {
        return Err(PanelError::new(
            "eyedropper_not_color",
            format!("{component}.{path} is not a color ([f32; 3], [f32; 4], Vec3 or Vec4)"),
        ));
    };
    let mut edited = crate::command::read_property(world, sample, component, "")
        .map_err(|e| PanelError::new("eyedropper_not_color", e.to_string()))?;
    // The rgb lanes only: each entity keeps its own alpha.
    for (lane, value) in &lanes {
        let target = lane
            .as_str()
            .reflect_element_mut(edited.as_mut())
            .map_err(|e| PanelError::new("eyedropper_not_color", e.to_string()))?;
        target
            .try_apply(value)
            .map_err(|e| PanelError::new("eyedropper_not_color", e.to_string()))?;
    }
    let changes: Vec<Change> = lanes
        .into_iter()
        .map(|(path, _)| Change { path, discrete: true })
        .collect();
    let report = super::multi::commit_drawer_changes(
        world,
        &holders,
        component,
        edited.as_ref(),
        &changes,
        Some(sample),
        &BTreeMap::new(),
    )
    .map_err(|e| PanelError::new(e.code, e.message))?;
    Ok(report.undo_depth)
}

/// The pixel `pixel` of a read-back, as sRGB bytes; `None` outside the image.
#[must_use]
pub fn pixel_of(image: &CapturedImage, pixel: [u32; 2]) -> Option<[u8; 4]> {
    if pixel[0] >= image.width || pixel[1] >= image.height {
        return None;
    }
    let at = (pixel[1] as usize * image.width as usize + pixel[0] as usize) * 4;
    let bytes = image.rgba.get(at..at + 4)?;
    Some([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Finishes a pick whose read-back arrived: writes the pixel's color (one undo step), remembers it
/// as a recent color and disarms. Returns the linear color written. The Inspector calls it once
/// per frame; `editor.color_eyedropper` calls it too.
pub fn resolve(world: &mut World) -> Option<[f32; 4]> {
    let armed = world.get_resource::<ColorEyedropper>()?;
    let pending = armed.pending.as_ref()?;
    let arrived = pending.slot.lock().ok().and_then(|mut held| held.take())?;
    let armed = armed.clone();
    let pixel = pending.pixel;
    world.remove_resource::<ColorEyedropper>();
    let _span = tracing::info_span!(target: "regia_editor", "color_eyedropper_resolve").entered();
    let picked = arrived.and_then(|image| {
        pixel_of(&image, pixel).ok_or_else(|| {
            format!(
                "pixel {}, {} is outside the {}x{} window",
                pixel[0], pixel[1], image.width, image.height
            )
        })
    });
    let outcome = picked.and_then(|bytes| {
        let rgb = linear_of(bytes);
        write_color(world, &armed.entities, &armed.component, &armed.path, rgb)
            .map(|undoable| ([rgb[0], rgb[1], rgb[2], 1.0], undoable))
            .map_err(|e| e.message)
    });
    let last = match outcome {
        Ok((rgba, undoable)) => {
            super::picker_links::remember_recent(world, rgba);
            LastColorPick {
                pixel,
                color: Some(rgba),
                error: None,
                undoable,
            }
        }
        Err(error) => {
            tracing::warn!(target: "regia_editor", "eyedropper: {error}");
            LastColorPick {
                pixel,
                color: None,
                error: Some(error),
                undoable: world
                    .get_resource::<crate::command::UndoStack>()
                    .map_or(0, crate::command::UndoStack::undo_depth),
            }
        }
    };
    let color = last.color;
    world.insert_resource(last);
    color
}

/// What the overlay saw this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Input {
    None,
    Cancel,
    Pick(egui::Pos2),
}

/// The keyboard and pointer input of an armed eyedropper: moves `cursor` and decides.
fn read_input(ctx: &egui::Context, cursor: &mut egui::Pos2, screen: egui::Rect) -> Input {
    let cancel = ctx.input_mut(|i| {
        i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) || i.pointer.secondary_clicked()
    });
    if cancel {
        return Input::Cancel;
    }
    if let Some(hover) = ctx.input(|i| {
        (i.pointer.delta() != egui::Vec2::ZERO)
            .then(|| i.pointer.hover_pos())
            .flatten()
    }) {
        *cursor = hover;
    }
    let arrows = [
        (egui::Key::ArrowLeft, egui::vec2(-1.0, 0.0)),
        (egui::Key::ArrowRight, egui::vec2(1.0, 0.0)),
        (egui::Key::ArrowUp, egui::vec2(0.0, -1.0)),
        (egui::Key::ArrowDown, egui::vec2(0.0, 1.0)),
    ];
    for (key, direction) in arrows {
        // Shift first: a plain match ignores an extra Shift.
        let (shifted, plain) = ctx.input_mut(|i| {
            let shifted = i.consume_key(egui::Modifiers::SHIFT, key);
            (shifted, i.consume_key(egui::Modifiers::NONE, key))
        });
        let step = if shifted { KEY_STEP * 10.0 } else { KEY_STEP };
        if plain || shifted {
            *cursor = screen.clamp(*cursor + direction * step);
        }
    }
    let keyed = ctx.input_mut(|i| {
        i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
            || i.consume_key(egui::Modifiers::NONE, egui::Key::Space)
    });
    if keyed {
        return Input::Pick(*cursor);
    }
    Input::None
}

/// The armed eyedropper's frame: finishes a pick whose read-back arrived, then (while armed and
/// not waiting) draws the overlay and takes a click, Enter or Space as the pick and Esc or a
/// right click as cancel. Call it once per frame from the Inspector.
pub fn frame(ctx: &egui::Context, world: &mut World) {
    let _ = resolve(world);
    let Some(armed) = world.get_resource::<ColorEyedropper>() else {
        return;
    };
    if armed.pending.is_some() {
        // The present copies the frame; repaint so the next frame writes it.
        ctx.request_repaint();
        return;
    }
    let frame_nr = ctx.cumulative_frame_nr();
    let acted = armed.acted_frame == Some(frame_nr);
    let screen = ctx.content_rect();
    let mut cursor = armed.cursor.unwrap_or_else(|| screen.center());
    let input = if acted {
        Input::None
    } else {
        read_input(ctx, &mut cursor, screen)
    };
    let id = egui::Id::new("inspector_color_eyedropper");
    // The layer over the whole window: nothing under it is hovered or clicked while armed.
    let clicked = egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .interactable(true)
        .show(ctx, |ui| {
            let response = ui.allocate_rect(screen, egui::Sense::click());
            // Named for assistive tech without a hover tooltip: a tooltip would sit in the picture.
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Other,
                    true,
                    "Eyedropper: click a pixel to pick its color (Esc cancels)",
                )
            });
            let response = response.on_hover_cursor(egui::CursorIcon::Crosshair);
            if response.clicked() {
                response.interact_pointer_pos()
            } else {
                None
            }
        })
        .inner;
    let input = match (input, clicked) {
        (Input::None, Some(at)) if !acted => Input::Pick(at),
        (input, _) => input,
    };
    let ppp = ctx.pixels_per_point();
    match input {
        Input::Cancel => {
            cancel(world);
            return;
        }
        Input::Pick(at) => {
            let pixel = [
                (at.x * ppp).max(0.0).floor() as u32,
                (at.y * ppp).max(0.0).floor() as u32,
            ];
            request_pick(world, pixel);
            if let Some(mut armed) = world.get_resource_mut::<ColorEyedropper>() {
                armed.acted_frame = Some(frame_nr);
                armed.cursor = Some(at);
            }
            ctx.request_repaint();
            // Nothing more is drawn this frame: the copy shows the window as it is.
            return;
        }
        Input::None => {}
    }
    if let Some(mut armed) = world.get_resource_mut::<ColorEyedropper>()
        && armed.cursor != Some(cursor)
    {
        armed.cursor = Some(cursor);
    }
    // The ring around the pick point leaves the point itself uncovered.
    let tokens = crate::theme::ui_tokens(ctx);
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, id.with("ring")));
    painter.circle_stroke(cursor, RING_RADIUS, egui::Stroke::new(1.5, tokens.accents.select));
    egui::Area::new(id.with("hint"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 8.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(
                    "Eyedropper: click a pixel, or move with the arrow keys and press Enter. Esc cancels.",
                );
            });
        });
}

// ---------------------------------------------------------------------------------------------
// Agent surface

fn err(code: &str, message: impl Into<String>) -> RpcError {
    PanelError::new(code, message).rpc(HINT)
}

fn state_json(world: &World) -> Value {
    let armed = world.get_resource::<ColorEyedropper>();
    let last = world.get_resource::<LastColorPick>().map(|last| {
        json!({
            "pixel": last.pixel,
            "color": last.color.map(|c| color::to_hex(c, true)),
            "error": last.error,
            "undoable": last.undoable,
        })
    });
    json!({
        "armed": armed.is_some(),
        "pending": armed.is_some_and(|armed| armed.pending.is_some()),
        "field": armed.map(|armed| json!({
            "entities": armed.entities.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
            "component": armed.component,
            "path": armed.path,
        })),
        "last": last,
    })
}

/// `editor.color_eyedropper`.
fn color_eyedropper(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _ = resolve(world);
    if params.get("cancel").and_then(Value::as_bool) == Some(true) {
        cancel(world);
        return Ok(state_json(world));
    }
    let Some(pixel) = params.get("pixel").filter(|v| !v.is_null()) else {
        return Ok(state_json(world));
    };
    let pixel = pixel
        .as_array()
        .filter(|xy| xy.len() == 2)
        .and_then(|xy| {
            Some([
                u32::try_from(xy[0].as_u64()?).ok()?,
                u32::try_from(xy[1].as_u64()?).ok()?,
            ])
        })
        .ok_or_else(|| err("eyedropper_bad_params", "`pixel` takes [x, y] window pixels"))?;
    let component = params
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| err("eyedropper_bad_params", "`component` is required with `pixel`"))?;
    let path = params.get("path").and_then(Value::as_str).ok_or_else(|| {
        err(
            "eyedropper_bad_params",
            "`path` (the color field) is required with `pixel`",
        )
    })?;
    let entities = super::pinned::entities_param(world, params)?;
    if entities.is_empty() {
        return Err(err(
            "eyedropper_bad_params",
            "no entities: give `entities` or select some",
        ));
    }
    if !entities
        .iter()
        .any(|entity| is_color_field(world, *entity, component, path))
    {
        return Err(err(
            "eyedropper_not_color",
            format!("{component}.{path} is not a color field of the entities"),
        ));
    }
    arm(world, entities, component, path);
    request_pick(world, pixel);
    Ok(state_json(world))
}

const PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "entities",
        kind: "array",
        required: false,
        description: "Entities whose color field takes the pick (absent: the selection).",
    },
    ParamSchema {
        name: "component",
        kind: "string",
        required: false,
        description: "Component type path or short name (`PointLight`); required with `pixel`.",
    },
    ParamSchema {
        name: "path",
        kind: "string",
        required: false,
        description: "The color field's reflect path (`color`); required with `pixel`.",
    },
    ParamSchema {
        name: "pixel",
        kind: "array",
        required: false,
        description: "[x, y] in the window's pixels (as seeing.screenshot returns them): arm the eyedropper and \
            pick that pixel at the next present. Without it the call only reports (and finishes a pick that arrived).",
    },
    ParamSchema {
        name: "cancel",
        kind: "boolean",
        required: false,
        description: "Disarm the eyedropper (Esc).",
    },
];

/// The method.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.color_eyedropper",
            tool: "color_eyedropper",
            description: "The color field's eyedropper: with `pixel` it picks that window pixel (viewport included) \
                into the color field at the window's next present, on every given entity that has it, keeping each \
                one's alpha, as one undo step. Call again without `pixel` until `pending` is false; `last` holds the \
                color written. `cancel` disarms it.",
            params: PARAMS,
            result: "{ armed, pending, field: { entities, component, path } | null, last: { pixel, color: \"#rrggbbaa\" | null, error, undoable } | null }",
            mutating: true,
            permission: Permission::EditScene,
        },
        color_eyedropper as Handler,
    )]
}
