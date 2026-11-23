// SPDX-License-Identifier: MPL-2.0
//! The viewport's measure tool (P-069, ADR-7769): `M` (the rebindable `viewport.measure`
//! command) enters measure mode in the active pane. A primary drag draws a distance line from
//! the press to the release with its length in level units (metres); a following click adds a
//! third point and the angle at the second point between the two legs. Holding the vertex-snap
//! key (`gizmo.vertex_snap`, V) snaps each point to the nearest mesh vertex under the cursor;
//! otherwise a point lands on the mesh or terrain under the cursor, else the ground plane.
//! A new press after a finished measure starts over. Escape (or `M` again) leaves the mode.
//!
//! While it is on, the primary button measures instead of box-selecting or picking. The agent
//! surface is `editor.measure` (`book/agents/viewport-tools.md`).

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;
use crate::views::ViewCamera;

/// A distance and, with a third point, an angle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// The first point.
    pub a: Vec3,
    /// The second point (the angle's vertex).
    pub b: Vec3,
    /// The third point, when one was placed.
    pub c: Option<Vec3>,
    /// `|b - a|`, level units (metres).
    pub distance: f32,
    /// `|c - b|`, with a third point.
    pub distance_bc: Option<f32>,
    /// The angle at `b` between `a - b` and `c - b`, degrees (0..=180); `None` without a third
    /// point or when a leg has zero length.
    pub angle_deg: Option<f32>,
}

/// Measures `a` to `b` and, with `c`, the angle at `b`.
#[must_use]
pub fn measure(a: Vec3, b: Vec3, c: Option<Vec3>) -> Measurement {
    let distance = (b - a).length();
    let distance_bc = c.map(|c| (c - b).length());
    let angle_deg = c.and_then(|c| {
        let (u, v) = (a - b, c - b);
        let denom = u.length() * v.length();
        (denom > 1e-12).then(|| (u.dot(v) / denom).clamp(-1.0, 1.0).acos().to_degrees())
    });
    Measurement {
        a,
        b,
        c,
        distance,
        distance_bc,
        angle_deg,
    }
}

/// The tool's state (a world resource).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct MeasureTool {
    /// Measure mode is on.
    pub active: bool,
    /// The placed points (0 to 3).
    pub points: Vec<Vec3>,
    /// A drag from `points[0]` is in progress: the live end point.
    pub live: Option<Vec3>,
    /// The pane the drag runs in.
    pane: Option<usize>,
    /// The egui frame whose click already acted: egui's second pass of a frame sees the same click
    /// (review sprint-w12-rev-g2 #2).
    clicked_frame: Option<u64>,
}

impl MeasureTool {
    /// Whether measure mode is on in `world`.
    #[must_use]
    pub fn is_active(world: &World) -> bool {
        world.get_resource::<Self>().is_some_and(|tool| tool.active)
    }

    /// The current measurement (the live drag end counts as the second point).
    #[must_use]
    pub fn measurement(&self) -> Option<Measurement> {
        let a = *self.points.first()?;
        let b = self.points.get(1).copied().or(self.live)?;
        Some(measure(a, b, self.points.get(2).copied()))
    }

    /// Turns the mode on or off; leaving clears the points.
    pub fn set_active(&mut self, on: bool) {
        self.active = on;
        if !on {
            self.points.clear();
            self.live = None;
            self.pane = None;
        }
    }

    /// A press at `point`: starts a new line unless a two-point measure waits for its third
    /// point. Returns whether it began a drag.
    pub fn press(&mut self, pane: usize, point: Vec3) -> bool {
        if self.points.len() == 2 {
            return false;
        }
        self.points = vec![point];
        self.live = Some(point);
        self.pane = Some(pane);
        true
    }

    /// A click (not a drag) at `point` in egui frame `frame`: the third point of a two-point measure,
    /// else the start of a new line. A second click in the same frame (egui's second UI pass) is
    /// ignored. Returns whether it acted.
    pub fn click(&mut self, point: Vec3, frame: u64) -> bool {
        if self.clicked_frame == Some(frame) {
            return false;
        }
        self.clicked_frame = Some(frame);
        if self.points.len() == 2 {
            self.release(point);
        } else {
            // A click with nothing placed (or a finished measure) starts a new line there.
            self.points = vec![point];
        }
        true
    }

    /// The pointer moved to `point` during the drag.
    pub fn drag_to(&mut self, point: Vec3) {
        if self.live.is_some() {
            self.live = Some(point);
        }
    }

    /// The release (drag end, `Some(point)`) or a click (`point` again): a drag finishes the
    /// line; a click on a finished line adds the third point.
    pub fn release(&mut self, point: Vec3) {
        if self.live.take().is_some() {
            if self.points.len() == 1 {
                self.points.push(point);
            }
        } else if self.points.len() == 2 {
            self.points.push(point);
        }
        self.pane = None;
    }
}

/// The point a pixel of a pane measures: the vertex under it with `snap`, else the mesh or
/// terrain surface under it, else the ground plane (or the orbit-target depth).
#[must_use]
pub fn point_under(
    world: &World,
    view: &ViewCamera,
    size: Vec2,
    pixel: Vec2,
    snap: bool,
    depth: f32,
) -> Vec3 {
    if snap && let Some(vertex) = crate::surface_tools::vertex_snap_at(world, view, size, pixel, &[]) {
        return vertex.position;
    }
    let (origin, direction) = view.ray(pixel, size);
    let mesh =
        crate::picking::raycast_world(world, origin, direction, crate::picking::FaceCull::None, &|e| {
            crate::viewport_ops::pickable(world, e)
        });
    let limit = mesh.as_ref().map_or(view.far, |hit| hit.distance);
    if let Some((_, t)) = crate::picking::pick_terrain(world, origin, direction, limit) {
        return origin + direction * t;
    }
    if let Some(hit) = mesh {
        return hit.position;
    }
    crate::pick_stack::ground_point(view, size, pixel, depth)
}

/// The pane a measure input frame runs in.
pub struct MeasurePane<'a> {
    /// The pane's view.
    pub view: &'a ViewCamera,
    /// The pane index.
    pub pane: usize,
    /// Screen point to pane buffer pixel.
    pub to_pixel: &'a dyn Fn(egui::Pos2) -> Vec2,
    /// The pane buffer's size in pixels.
    pub buffer: Vec2,
    /// The vertex-snap key is held.
    pub snap: bool,
}

/// The pane's measure input for one frame (called by the viewport while the tool is on):
/// press, drag and release of the primary button. Returns whether the tool owns the pointer
/// this frame.
pub fn viewport_input(
    ui: &egui::Ui,
    world: &mut World,
    response: &egui::Response,
    pane: &MeasurePane<'_>,
) -> bool {
    let Some(mut tool) = world.get_resource::<MeasureTool>().cloned() else {
        return false;
    };
    if !tool.active {
        return false;
    }
    let _span = tracing::info_span!(target: "regia_editor", "measure_input").entered();
    let view = pane.view;
    let depth = (view.target - view.eye).length();
    let at = |world: &World, pos: egui::Pos2| {
        point_under(world, view, pane.buffer, (pane.to_pixel)(pos), pane.snap, depth)
    };
    if response.drag_started_by(egui::PointerButton::Primary)
        && let Some(pos) = ui
            .input(|i| i.pointer.press_origin())
            .or_else(|| response.interact_pointer_pos())
    {
        let point = at(world, pos);
        tool.press(pane.pane, point);
    }
    if tool.live.is_some()
        && tool.pane == Some(pane.pane)
        && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
    {
        let point = at(world, pos);
        tool.drag_to(point);
    }
    if response.drag_stopped() && tool.live.is_some() {
        let end = tool.live.unwrap_or_default();
        tool.release(end);
    } else if ui.input(|i| i.pointer.primary_clicked())
        && response.contains_pointer()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let point = at(world, pos);
        tool.click(point, ui.ctx().cumulative_frame_nr());
    }
    world.insert_resource(tool);
    true
}

/// The measure overlay: the legs, a dot on each point, the length at each leg's middle and the
/// angle at the vertex. In pane points (`size`), colors from the theme's overlay tokens.
#[must_use]
pub fn overlay_shapes(world: &World, view: &ViewCamera, size: Vec2) -> Vec<crate::overlays::OverlayShape> {
    use crate::overlays::OverlayShape;
    let Some(tool) = world.get_resource::<MeasureTool>() else {
        return Vec::new();
    };
    let Some(m) = tool.measurement().filter(|_| tool.active) else {
        return Vec::new();
    };
    let tokens = crate::theme::tokens(world).overlay;
    let (line, text) = (tokens.bounds, tokens.selection_outline);
    let mut shapes = Vec::new();
    let project = |p: Vec3| view.project(p, size);
    let leg = |from: Vec3, to: Vec3, length: f32, shapes: &mut Vec<OverlayShape>| {
        if let (Some(a), Some(b)) = (project(from), project(to)) {
            shapes.push(OverlayShape::Line {
                a,
                b,
                color: line,
                thickness: 0,
            });
            shapes.push(OverlayShape::Text {
                at: (a + b) * 0.5 + Vec2::new(6.0, -6.0),
                text: format!("{length:.3} m"),
                color: text,
            });
        }
    };
    leg(m.a, m.b, m.distance, &mut shapes);
    if let (Some(c), Some(bc)) = (m.c, m.distance_bc) {
        leg(m.b, c, bc, &mut shapes);
    }
    for p in [Some(m.a), Some(m.b), m.c].into_iter().flatten() {
        if let Some(center) = project(p) {
            shapes.push(OverlayShape::Circle {
                center,
                radius: 3.0,
                color: line,
            });
        }
    }
    if let (Some(angle), Some(vertex)) = (m.angle_deg, project(m.b)) {
        shapes.push(OverlayShape::Text {
            at: vertex + Vec2::new(8.0, 14.0),
            text: format!("{angle:.1}°"),
            color: text,
        });
    }
    shapes
}

fn vec3_param(params: &Value, name: &str) -> Result<Option<Vec3>, RpcError> {
    let Some(value) = params.get(name) else {
        return Ok(None);
    };
    let xyz = value
        .as_array()
        .filter(|a| a.len() == 3)
        .and_then(|a| {
            a.iter()
                .map(|v| v.as_f64().map(|v| v as f32))
                .collect::<Option<Vec<f32>>>()
        })
        .filter(|v| v.iter().all(|c| c.is_finite()))
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite [x, y, z]")))?;
    Ok(Some(Vec3::new(xyz[0], xyz[1], xyz[2])))
}

fn measurement_json(m: &Measurement) -> Value {
    json!({
        "a": m.a.to_array(),
        "b": m.b.to_array(),
        "c": m.c.map(|c| c.to_array()),
        "distance": m.distance,
        "distance_bc": m.distance_bc,
        "angle_deg": m.angle_deg,
        "units": "m",
    })
}

/// `editor.measure`.
fn measure_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (a, b, c) = (
        vec3_param(params, "a")?,
        vec3_param(params, "b")?,
        vec3_param(params, "c")?,
    );
    let mut tool = world.get_resource::<MeasureTool>().cloned().unwrap_or_default();
    if let Some(on) = params.get("active").and_then(Value::as_bool) {
        tool.set_active(on);
    }
    let result = match (a, b) {
        (Some(a), Some(b)) => {
            let m = measure(a, b, c);
            // Shown in the viewport like a measure drawn by hand.
            tool.active = true;
            tool.points = [Some(a), Some(b), c].into_iter().flatten().collect();
            tool.live = None;
            Some(m)
        }
        (None, None) if c.is_none() => tool.measurement(),
        _ => {
            return Err(RpcError::invalid_params(
                "pass both `a` and `b` (and optionally `c`), or none to read the tool",
            ));
        }
    };
    let active = tool.active;
    world.insert_resource(tool);
    Ok(json!({
        "active": active,
        "measurement": result.as_ref().map(measurement_json),
    }))
}

/// The agent methods of the measure tool.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.measure",
            tool: "measure",
            description: "The viewport's measure tool (M): the distance a to b in level units (metres) and, with c, \
                the angle at b. Shows the measure in the viewport. Without points, reads the tool's current \
                measure; `active` turns measure mode on or off.",
            params: &[
                ParamSchema {
                    name: "a",
                    kind: "array",
                    required: false,
                    description: "First point [x, y, z], world space.",
                },
                ParamSchema {
                    name: "b",
                    kind: "array",
                    required: false,
                    description: "Second point (the angle's vertex).",
                },
                ParamSchema {
                    name: "c",
                    kind: "array",
                    required: false,
                    description: "Optional third point: adds the angle at b and the length b to c.",
                },
                ParamSchema {
                    name: "active",
                    kind: "boolean",
                    required: false,
                    description: "Enter (true) or leave (false) measure mode; leaving clears the points.",
                },
            ],
            result: "{ active, measurement: { a, b, c, distance, distance_bc, angle_deg, units } | null }",
            mutating: false,
            permission: Permission::Read,
        },
        measure_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review sprint-w12-rev-g2 #2: the third click of a measure stores the angle once; egui's
    /// second pass of the frame does not restart the line.
    #[test]
    fn a_repeated_click_in_one_frame_keeps_the_angle() {
        let mut tool = MeasureTool::default();
        tool.set_active(true);
        assert!(tool.click(Vec3::Z, 1));
        assert!(!tool.click(Vec3::X, 1), "the second pass of frame 1");
        assert_eq!(tool.points, [Vec3::Z]);
        // The first leg is a drag.
        assert!(tool.press(0, Vec3::ZERO));
        tool.drag_to(Vec3::X);
        tool.release(Vec3::X);
        assert_eq!(tool.points, [Vec3::ZERO, Vec3::X]);
        assert!(tool.click(Vec3::Y, 3));
        assert_eq!(tool.points, [Vec3::ZERO, Vec3::X, Vec3::Y], "the third point");
        assert!(!tool.click(Vec3::Z, 3), "the second pass of frame 3");
        assert_eq!(tool.points.len(), 3, "the angle stays");
        assert!(tool.measurement().is_some_and(|m| m.angle_deg.is_some()));
        assert!(tool.click(Vec3::Z, 4));
        assert_eq!(tool.points, [Vec3::Z], "a later click starts a new line");
    }

    #[test]
    fn distance_and_angle() {
        let m = measure(
            Vec3::new(3.0, 0.0, 0.0),
            Vec3::ZERO,
            Some(Vec3::new(0.0, 0.0, 4.0)),
        );
        assert!((m.distance - 3.0).abs() < 1e-6);
        assert_eq!(m.distance_bc, Some(4.0));
        assert!((m.angle_deg.expect("angle") - 90.0).abs() < 1e-4);
        let straight = measure(Vec3::X, Vec3::ZERO, Some(Vec3::NEG_X));
        assert!((straight.angle_deg.expect("angle") - 180.0).abs() < 1e-3);
        assert_eq!(
            measure(Vec3::ZERO, Vec3::ZERO, Some(Vec3::X)).angle_deg,
            None,
            "a zero leg has no angle"
        );
        assert_eq!(measure(Vec3::ZERO, Vec3::Y, None).angle_deg, None);
    }

    /// The press-drag-release line, the third click and the restart, then Escape-like leave.
    #[test]
    fn press_drag_release_then_a_click_adds_the_angle() {
        let mut tool = MeasureTool::default();
        tool.set_active(true);
        assert!(tool.press(0, Vec3::ZERO));
        tool.drag_to(Vec3::new(1.0, 0.0, 0.0));
        assert!(
            (tool.measurement().expect("live").distance - 1.0).abs() < 1e-6,
            "live length"
        );
        tool.drag_to(Vec3::new(2.0, 0.0, 0.0));
        tool.release(Vec3::new(2.0, 0.0, 0.0));
        assert_eq!(tool.points.len(), 2);
        assert!(
            !tool.press(0, Vec3::ONE),
            "a finished line waits for its third point"
        );
        tool.release(Vec3::new(2.0, 0.0, 2.0));
        let m = tool.measurement().expect("three points");
        assert!((m.angle_deg.expect("angle") - 90.0).abs() < 1e-4);
        assert!(tool.press(0, Vec3::ONE), "after the angle a press starts over");
        assert_eq!(tool.points, vec![Vec3::ONE]);
        tool.set_active(false);
        assert!(tool.points.is_empty() && tool.measurement().is_none());
    }

    /// ADR-7769: the lasso's L and the measure tool's M were free: no other command's built-in
    /// shortcut and no fixed editor key is either chord, and a user binding of another command to
    /// M lists the measure tool as what it shadows.
    #[test]
    fn the_l_and_m_chords_are_free_of_other_built_in_keys() {
        use crate::command_palette::{COMMANDS, CommandKeymap, KeyChord};
        for (own, key) in [("selection.lasso", "L"), ("viewport.measure", "M")] {
            let chord = KeyChord::parse(key).expect("chord");
            for def in COMMANDS.iter().filter(|def| def.id != own) {
                assert_ne!(
                    KeyChord::parse(def.shortcut),
                    Some(chord),
                    "{} also uses {key}",
                    def.id
                );
            }
            assert!(
                !crate::editor_keymap::FIXED_EDITOR_KEYS
                    .iter()
                    .any(|(keys, _)| KeyChord::parse(keys) == Some(chord)),
                "{key} is a fixed editor key"
            );
        }
        let mut keymap = CommandKeymap::default();
        keymap
            .bindings
            .insert("view.frame_selection", KeyChord::parse("M").expect("chord"));
        let conflicts = crate::editor_keymap::conflicts(&keymap);
        assert!(
            conflicts.iter().any(|c| c.shadows.contains("viewport.measure")),
            "{conflicts:?}"
        );
    }
}
