// SPDX-License-Identifier: MPL-2.0
//! The viewport's resolution preview (`specs/19` § 2; lane w11-editor-smalls, ADR-7744): an
//! overlay frame in the edit viewport that shows the picture a chosen play resolution would have
//! in a window of the pane's size, with title-safe and action-safe guides.
//!
//! The frame is [`crate::play_window::Letterbox::fit`], the function the play window uses for the
//! rect its picture lands in ([`frame_rect`] is the one call), so the overlay and the window
//! agree to the pixel at any pane size. The resolutions are the `play.resolution` presets
//! ([`crate::play_window::RESOLUTION_PRESETS`]) or a custom size.
//!
//! The toolbar's **Resolution** toggle and combo and `editor.viewport_resolution_preview {
//! preset | size | off }` drive one [`ResolutionPreview`] resource. It is editor view state: no
//! undo step, nothing saved, and play-in-editor is not changed by it.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::play_window::{Letterbox, PlayResolution, RESOLUTION_PRESETS};
use crate::viewport_agent::Handler;

/// The error reason of a `preset` or `size` that is unknown or out of range.
pub const RESOLUTION_PREVIEW_INVALID: &str = "viewport_resolution_invalid";
/// The error reason of a call that names more than one of `preset`, `size` and `off`.
pub const RESOLUTION_PREVIEW_CONFLICT: &str = "viewport_resolution_conflict";

/// The action-safe margin: the share of each side outside the outer guide (5 %).
pub const ACTION_SAFE_MARGIN: f64 = 0.05;
/// The title-safe margin: the share of each side outside the inner guide (10 %).
pub const TITLE_SAFE_MARGIN: f64 = 0.10;

/// The viewport's resolution preview: whether the frame draws and for which play resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Resource)]
pub struct ResolutionPreview {
    /// Whether the frame is drawn.
    pub on: bool,
    /// The play resolution it previews. [`PlayResolution::Window`] frames the whole pane.
    pub resolution: PlayResolution,
}

impl Default for ResolutionPreview {
    fn default() -> Self {
        Self {
            on: false,
            resolution: PlayResolution::preset("hd_1080").unwrap_or_default(),
        }
    }
}

/// Where the picture of `resolution` sits in a window (or pane) of `pane` pixels: the play
/// window's own letterbox rect.
#[must_use]
pub fn frame_rect(resolution: PlayResolution, pane: [u32; 2]) -> Letterbox {
    Letterbox::fit(resolution.render_size(pane), pane)
}

/// The action-safe and title-safe guide rects inside `frame`, outer first.
#[must_use]
pub fn safe_rects(frame: Letterbox) -> [Letterbox; 2] {
    let inset = |margin: f64| {
        let dx = (f64::from(frame.width) * margin).round() as u32;
        let dy = (f64::from(frame.height) * margin).round() as u32;
        Letterbox {
            x: frame.x + dx,
            y: frame.y + dy,
            width: frame.width.saturating_sub(2 * dx),
            height: frame.height.saturating_sub(2 * dy),
        }
    };
    [inset(ACTION_SAFE_MARGIN), inset(TITLE_SAFE_MARGIN)]
}

/// The pane's size in whole pixels (points rounded; an empty pane gives 0).
fn pane_pixels(rect: egui::Rect) -> [u32; 2] {
    let side = |v: f32| v.round().max(0.0) as u32;
    [side(rect.width()), side(rect.height())]
}

fn to_egui(pane: egui::Rect, rect: Letterbox) -> egui::Rect {
    egui::Rect::from_min_size(
        pane.min + egui::vec2(rect.x as f32, rect.y as f32),
        egui::vec2(rect.width as f32, rect.height as f32),
    )
}

/// Draws the preview over `pane`: the bars outside the frame dimmed, the frame outlined, the two
/// safe-area guides and a size label. Nothing when the preview is off.
pub fn paint(painter: &egui::Painter, pane: egui::Rect, preview: &ResolutionPreview) {
    if !preview.on {
        return;
    }
    let _span = tracing::trace_span!(target: "regia_editor", "resolution_preview").entered();
    let pixels = pane_pixels(pane);
    if pixels[0] == 0 || pixels[1] == 0 {
        return;
    }
    let frame = frame_rect(preview.resolution, pixels);
    let frame_pts = to_egui(pane, frame);
    let dim = egui::Color32::from_black_alpha(150);
    // The bars: full-width bands above and below, then left and right of the frame.
    let bars = [
        egui::Rect::from_min_max(pane.min, egui::pos2(pane.max.x, frame_pts.min.y)),
        egui::Rect::from_min_max(egui::pos2(pane.min.x, frame_pts.max.y), pane.max),
        egui::Rect::from_min_max(
            egui::pos2(pane.min.x, frame_pts.min.y),
            egui::pos2(frame_pts.min.x, frame_pts.max.y),
        ),
        egui::Rect::from_min_max(
            egui::pos2(frame_pts.max.x, frame_pts.min.y),
            egui::pos2(pane.max.x, frame_pts.max.y),
        ),
    ];
    for bar in bars {
        if bar.width() > 0.0 && bar.height() > 0.0 {
            painter.rect_filled(bar, 0.0, dim);
        }
    }
    painter.rect_stroke(
        frame_pts,
        0.0,
        egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 200, 80)),
        egui::StrokeKind::Inside,
    );
    let guides = [
        egui::Color32::from_rgba_unmultiplied(120, 220, 255, 150),
        egui::Color32::from_rgba_unmultiplied(120, 255, 160, 150),
    ];
    for (guide, color) in safe_rects(frame).into_iter().zip(guides) {
        if guide.width > 0 && guide.height > 0 {
            painter.rect_stroke(
                to_egui(pane, guide),
                0.0,
                egui::Stroke::new(1.0, color),
                egui::StrokeKind::Inside,
            );
        }
    }
    painter.text(
        frame_pts.left_top() + egui::vec2(6.0, 4.0),
        egui::Align2::LEFT_TOP,
        label(preview.resolution),
        egui::FontId::proportional(11.0),
        egui::Color32::from_rgb(255, 220, 140),
    );
}

/// The frame's label: `1920 × 1080` or `Window size`.
#[must_use]
pub fn label(resolution: PlayResolution) -> String {
    match resolution.size() {
        Some([w, h]) => format!("{w} × {h}"),
        None => "Window size".to_owned(),
    }
}

/// The toolbar's preset combo text.
#[must_use]
pub fn combo_text(resolution: PlayResolution) -> String {
    RESOLUTION_PRESETS
        .iter()
        .find(|p| p.resolution == resolution)
        .map_or_else(|| label(resolution), |p| p.label.to_owned())
}

fn state_json(world: &World, pane: Option<[u32; 2]>) -> Value {
    let preview = world
        .get_resource::<ResolutionPreview>()
        .copied()
        .unwrap_or_default();
    let mut state = json!({
        "on": preview.on,
        "preset": preview.resolution.preset_id(),
        "size": preview.resolution.size(),
    });
    if let (Some(pane), Some(object)) = (pane, state.as_object_mut()) {
        let frame = frame_rect(preview.resolution, pane);
        object.insert("pane".into(), json!(pane));
        object.insert("rect".into(), json!(frame.to_array()));
        object.insert(
            "safe_rects".into(),
            json!(safe_rects(frame).map(Letterbox::to_array)),
        );
    }
    state
}

fn invalid(message: String, hint: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, RESOLUTION_PREVIEW_INVALID, message, hint)
}

fn size_param(value: &Value) -> Result<[u32; 2], RpcError> {
    let side = |index: usize| {
        value
            .get(index)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
    match (side(0), side(1), value.as_array().map(Vec::len)) {
        (Some(w), Some(h), Some(2)) => Ok([w, h]),
        _ => Err(invalid(
            format!("size {value} is not [width, height]"),
            "size is two whole numbers, e.g. [1080, 1920]",
        )),
    }
}

/// `editor.viewport_resolution_preview { preset?, size?, off?, pane? }`.
fn resolution_preview_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let preset = params.get("preset").filter(|v| !v.is_null());
    let size = params.get("size").filter(|v| !v.is_null());
    let off = params.get("off").and_then(Value::as_bool) == Some(true);
    if usize::from(preset.is_some()) + usize::from(size.is_some()) + usize::from(off) > 1 {
        return Err(RpcError::new(
            INVALID_PARAMS,
            RESOLUTION_PREVIEW_CONFLICT,
            "preset, size and off are alternatives".to_owned(),
            "pass one of preset, size or off",
        ));
    }
    let pane = match params.get("pane").filter(|v| !v.is_null()) {
        Some(value) => Some(size_param(value)?),
        None => None,
    };
    let mut preview = world
        .get_resource::<ResolutionPreview>()
        .copied()
        .unwrap_or_default();
    if off {
        preview.on = false;
    } else if let Some(preset) = preset {
        let id = preset.as_str().unwrap_or_default();
        preview.resolution = PlayResolution::preset(id).ok_or_else(|| {
            invalid(
                format!("unknown preset {preset}"),
                "preset is one of window, hd_720, hd_1080, phone_9_16_portrait, phone_9_19_5_portrait, \
                 phone_9_19_5_landscape, tablet_3_4_portrait, tablet_3_4_landscape",
            )
        })?;
        preview.on = true;
    } else if let Some(size) = size {
        let [w, h] = size_param(size)?;
        preview.resolution = PlayResolution::custom(w, h).ok_or_else(|| {
            invalid(
                format!("size {w} x {h} is outside the allowed range"),
                "each side is 16 to 16384 pixels",
            )
        })?;
        preview.on = true;
    }
    world.insert_resource(preview);
    Ok(state_json(world, pane))
}

/// The agent methods of this module.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.viewport_resolution_preview",
            tool: "viewport_resolution_preview",
            description: "The edit viewport's resolution preview (toolbar ▸ Resolution): an overlay frame showing the picture the play window would have at a chosen play resolution, scaled to fit the pane with the same rect function as the play window's letterbox, with action-safe (5 %) and title-safe (10 %) guides. `preset` (a play.resolution preset id) or `size` ([width, height]) turns it on; `off: true` turns it off; none reads the state. Pass `pane: [width, height]` to read the frame's rect in a pane of that size. Editor view state: no undo step, not saved.",
            params: &[
                ParamSchema {
                    name: "preset",
                    kind: "string",
                    required: false,
                    description: "A play.resolution preset id: window, hd_720, hd_1080, phone_9_16_portrait, phone_9_19_5_portrait, phone_9_19_5_landscape, tablet_3_4_portrait or tablet_3_4_landscape. Turns the preview on.",
                },
                ParamSchema {
                    name: "size",
                    kind: "array",
                    required: false,
                    description: "A custom [width, height], 16 to 16384 each. Turns the preview on.",
                },
                ParamSchema {
                    name: "off",
                    kind: "boolean",
                    required: false,
                    description: "true turns the preview off (the chosen resolution is kept).",
                },
                ParamSchema {
                    name: "pane",
                    kind: "array",
                    required: false,
                    description: "[width, height] of a pane in pixels: the reply adds the frame's rect and guides for that pane.",
                },
            ],
            result: "{ on, preset, size, pane?, rect?, safe_rects? } (rects are [x, y, width, height] in pane pixels)",
            // Editor view state like `editor.nav_preview`: read-scoped, never an edit.
            mutating: true,
            permission: Permission::Read,
        },
        resolution_preview_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_frame_equals_the_play_windows_letterbox() {
        for pane in [[800, 600], [1280, 720], [333, 777], [1, 1]] {
            for preset in RESOLUTION_PRESETS {
                let ours = frame_rect(preset.resolution, pane);
                let play = Letterbox::fit(preset.resolution.render_size(pane), pane);
                assert_eq!(ours, play, "{} in {pane:?}", preset.id);
            }
        }
    }

    #[test]
    fn known_frames_at_a_known_pane() {
        let fit = |id: &str, pane: [u32; 2]| {
            frame_rect(PlayResolution::preset(id).expect("preset"), pane).to_array()
        };
        assert_eq!(fit("hd_1080", [800, 600]), [0, 75, 800, 450]);
        assert_eq!(fit("hd_720", [800, 600]), [0, 75, 800, 450]);
        assert_eq!(fit("window", [800, 600]), [0, 0, 800, 600]);
        // 9:16 is 0.5625 wide per tall: the 600 px height gives 337.5 -> 338 wide.
        assert_eq!(fit("phone_9_16_portrait", [800, 600]), [231, 0, 338, 600]);
        assert_eq!(fit("tablet_3_4_portrait", [800, 600]), [175, 0, 450, 600]);
        assert_eq!(fit("tablet_3_4_landscape", [800, 600]), [0, 0, 800, 600]);
        let custom = PlayResolution::custom(1000, 500).expect("custom");
        assert_eq!(frame_rect(custom, [800, 600]).to_array(), [0, 100, 800, 400]);
    }

    #[test]
    fn safe_guides_sit_inside_the_frame() {
        let frame = Letterbox {
            x: 0,
            y: 75,
            width: 800,
            height: 450,
        };
        let [action, title] = safe_rects(frame);
        assert_eq!(action.to_array(), [40, 98, 720, 404]);
        assert_eq!(title.to_array(), [80, 120, 640, 360]);
    }

    #[test]
    fn off_by_default_and_the_method_drives_the_resource() {
        let mut world = World::new();
        let off = resolution_preview_method(&mut world, &json!({})).expect("read");
        assert_eq!(off["on"], false);
        let on = resolution_preview_method(
            &mut world,
            &json!({ "preset": "phone_9_16_portrait", "pane": [800, 600] }),
        )
        .expect("preset");
        assert_eq!(on["on"], true);
        assert_eq!(on["preset"], "phone_9_16_portrait");
        assert_eq!(on["size"], json!([1080, 1920]));
        assert_eq!(on["rect"], json!([231, 0, 338, 600]));
        let size = resolution_preview_method(&mut world, &json!({ "size": [1000, 500] })).expect("size");
        assert_eq!(size["preset"], "custom");
        assert_eq!(size["size"], json!([1000, 500]));
        let off = resolution_preview_method(&mut world, &json!({ "off": true })).expect("off");
        assert_eq!(off["on"], false);
        assert_eq!(off["size"], json!([1000, 500]), "off keeps the choice");
    }

    #[test]
    fn the_method_refuses_bad_input_with_codes() {
        let mut world = World::new();
        let code = |err: RpcError| {
            err.data
                .and_then(|d| d.get("code").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_default()
        };
        for params in [
            json!({ "preset": "nope" }),
            json!({ "size": [1, 1] }),
            json!({ "size": [1920] }),
            json!({ "size": "big" }),
        ] {
            let err = resolution_preview_method(&mut world, &params).expect_err("invalid");
            assert_eq!(code(err), RESOLUTION_PREVIEW_INVALID, "{params}");
        }
        let err = resolution_preview_method(&mut world, &json!({ "preset": "hd_720", "off": true }))
            .expect_err("conflict");
        assert_eq!(code(err), RESOLUTION_PREVIEW_CONFLICT);
        assert!(
            world.get_resource::<ResolutionPreview>().is_none(),
            "a refused call changes nothing"
        );
    }

    #[test]
    fn paint_outlines_the_frame_rect_and_nothing_when_off() {
        let ctx = egui::Context::default();
        let pane = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(800.0, 600.0));
        let outlines = |preview: &ResolutionPreview| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| paint(ui.painter(), pane, preview));
            output.textures_delta.clear();
            output
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::epaint::Shape::Rect(r) if r.stroke.width > 1.2 => Some(r.rect),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let mut preview = ResolutionPreview::default();
        assert!(outlines(&preview).is_empty(), "off draws nothing");
        preview.on = true;
        let frame = frame_rect(preview.resolution, [800, 600]);
        let found = outlines(&preview);
        assert_eq!(found, vec![to_egui(pane, frame)]);
    }
}
