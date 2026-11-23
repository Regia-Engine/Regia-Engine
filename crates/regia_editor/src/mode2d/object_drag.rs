// SPDX-License-Identifier: MPL-2.0
//! Moving map objects in the front pane (ADR-7853): with an object layer active in the layer
//! tree, a click selects the object under the pointer (or clears the selection), and a drag moves
//! it, snapping to the guides and to smart guides (edge and center alignment with the layer's
//! other objects, equal spacing to its neighbors) unless snapping is off or Alt is held. The
//! alignment lines and spacing hints are drawn while dragging. A drag is one undo entry.
//!
//! Object boxes ignore rotation. Dragging works on orthogonal maps; on other orientations objects
//! are moved from the 2D tab's fields or `editor.tile_layers`.

use regia_2d::tilemap::{MapLayer, MapObject, ObjectLayer, ObjectShape, TileMapAsset};
use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;

use super::guides::{self, Box2, SnapResult};
use super::layers::LayerEditCommand;
use super::{Mode2d, push_command};
use crate::views::ViewCamera;

/// An object drag in progress.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectDrag {
    /// The object layer.
    pub layer: usize,
    /// The object's id.
    pub id: u32,
    /// The level point the drag started at.
    pub grab: [f32; 2],
    /// The object's file position when the drag started.
    pub start: [f32; 2],
    /// The object layer before the drag (the undo entry's `before`).
    pub before: ObjectLayer,
    /// What the last move snapped to (drawn in the pane).
    pub snap: SnapResult,
}

/// The box of `object` in level units (+Y up from the map's bottom-left corner).
#[must_use]
pub fn object_box(map: &TileMapAsset, object: &MapObject) -> Box2 {
    let [ax, ay] = map.object_world_position(object);
    let [w, h] = object.size;
    match object.shape {
        // Tile objects are anchored at the bottom-left of the tile image.
        ObjectShape::Tile(_) => Box2 {
            min: [ax, ay],
            max: [ax + w, ay + h],
        },
        _ => Box2 {
            min: [ax, ay - h],
            max: [ax + w, ay],
        },
    }
}

/// The topmost object of `layer` whose box holds `point` (the last drawn wins).
#[must_use]
pub fn object_at(map: &TileMapAsset, layer: usize, point: [f32; 2], slack: f32) -> Option<u32> {
    let objects = map.object_layer(layer)?;
    objects.objects.iter().rev().find_map(|o| {
        let b = object_box(map, o);
        let inside = (0..2).all(|a| point[a] >= b.min[a] - slack && point[a] <= b.max[a] + slack);
        inside.then_some(o.id)
    })
}

impl Mode2d {
    /// Starts dragging object `id` of the active object layer from level point `grab`.
    pub fn begin_object_drag(&mut self, id: u32, grab: [f32; 2]) -> bool {
        let (Some(layer), Some(doc)) = (self.object_layer, self.doc.as_ref()) else {
            return false;
        };
        let Some(before) = doc.map.object_layer(layer).cloned() else {
            return false;
        };
        let Some(object) = before.objects.iter().find(|o| o.id == id) else {
            return false;
        };
        self.selected_object = Some(id);
        self.object_drag = Some(ObjectDrag {
            layer,
            id,
            grab,
            start: object.position,
            before,
            snap: SnapResult::default(),
        });
        true
    }

    /// Moves the dragged object so the grab point follows `to` (level units), snapped within
    /// `tolerance` level units unless `free`.
    pub fn drag_object_to(&mut self, to: [f32; 2], tolerance: f32, free: bool) {
        let Some(drag) = self.object_drag.as_mut() else {
            return;
        };
        let Some(doc) = self.doc.as_mut() else { return };
        let map = &doc.map;
        let Some(object) = drag.before.objects.iter().find(|o| o.id == drag.id) else {
            return;
        };
        let delta = [to[0] - drag.grab[0], to[1] - drag.grab[1]];
        let moved = object_box(map, object).offset(delta);
        let others: Vec<Box2> = drag
            .before
            .objects
            .iter()
            .filter(|o| o.id != drag.id)
            .map(|o| object_box(map, o))
            .collect();
        drag.snap = if free || !self.guides.snap {
            SnapResult::default()
        } else {
            guides::snap_move(moved, &others, &self.guides.guides, tolerance)
        };
        let d = [delta[0] + drag.snap.offset[0], delta[1] + drag.snap.offset[1]];
        // The file's y runs down.
        let position = [drag.start[0] + d[0], drag.start[1] - d[1]];
        if let Some(MapLayer::Objects(layer)) = doc.map.layers.get_mut(drag.layer)
            && let Some(o) = layer.objects.iter_mut().find(|o| o.id == drag.id)
            && o.position != position
        {
            o.position = position;
            doc.dirty = true;
        }
    }

    /// Ends the drag; the undo command when the object moved.
    pub fn end_object_drag(&mut self) -> Option<LayerEditCommand> {
        let drag = self.object_drag.take()?;
        let doc = self.doc.as_ref()?;
        let after = doc.map.object_layer(drag.layer)?.clone();
        (after != drag.before).then(|| LayerEditCommand::Objects {
            path: doc.path.clone(),
            layer: drag.layer,
            before: drag.before,
            after,
        })
    }
}

/// The pane's pointer input while an object layer is active: select, drag with snapping. True
/// when the object layer took the input (no tile tool runs then).
pub(super) fn object_input(
    ui: &egui::Ui,
    world: &mut World,
    mode: &mut Mode2d,
    response: &egui::Response,
    view: &ViewCamera,
    size: Vec2,
) -> bool {
    let Some(layer) = mode.object_layer else {
        return false;
    };
    let orthogonal = mode
        .doc
        .as_ref()
        .is_some_and(|d| d.map.orientation.is_orthogonal());
    let primary = egui::PointerButton::Primary;
    let local = |p: egui::Pos2| {
        let l = p - response.rect.left_top();
        Vec2::new(l.x, l.y)
    };
    let units_per_px = view.units_per_pixel(size.y);
    let at =
        |mode: &Mode2d, p: Option<egui::Pos2>| p.map(local).and_then(|p| mode.level_at_pixel(view, p, size));
    if mode.object_drag.is_none() && (response.drag_started_by(primary) || response.clicked_by(primary)) {
        let press = at(mode, ui.input(|i| i.pointer.press_origin()));
        let hit = press.and_then(|p| {
            mode.doc
                .as_ref()
                .and_then(|d| object_at(&d.map, layer, p, units_per_px * 2.0))
        });
        match (hit, press) {
            (Some(id), Some(grab)) if orthogonal && response.drag_started_by(primary) => {
                mode.begin_object_drag(id, grab);
            }
            (Some(id), _) => mode.selected_object = Some(id),
            (None, _) => mode.selected_object = None,
        }
    }
    if mode.object_drag.is_some() {
        let free = ui.input(|i| i.modifiers.alt);
        if let Some(to) = at(
            mode,
            response
                .hover_pos()
                .or_else(|| ui.input(|i| i.pointer.latest_pos())),
        ) {
            mode.drag_object_to(to, units_per_px * guides::SNAP_PX, free);
        }
        if (response.drag_stopped_by(primary) || !ui.input(|i| i.pointer.primary_down()))
            && let Some(command) = mode.end_object_drag()
        {
            push_command(world, Box::new(command));
        }
    }
    true
}

/// Paints the active object layer's boxes (the selected one in the selection color) and, while
/// dragging, the smart-guide lines and spacing hints, in theme colors.
pub(super) fn paint_objects(
    painter: &egui::Painter,
    rect: egui::Rect,
    mode: &Mode2d,
    view: &ViewCamera,
    size: Vec2,
    tokens: &crate::theme::ThemeTokens,
) {
    let (Some(layer), Some(doc)) = (mode.object_layer, mode.doc.as_ref()) else {
        return;
    };
    let Some(objects) = doc.map.object_layer(layer) else {
        return;
    };
    let px = |p: [f32; 2]| {
        view.project(mode.origin + Vec3::new(p[0], p[1], 0.0), size)
            .map(|v| rect.min + egui::vec2(v.x, v.y))
    };
    for object in &objects.objects {
        let b = object_box(&doc.map, object);
        let (Some(a), Some(c)) = (px([b.min[0], b.max[1]]), px([b.max[0], b.min[1]])) else {
            continue;
        };
        let selected = mode.selected_object == Some(object.id);
        let color = if selected {
            tokens.overlay.selection_outline
        } else {
            tokens.overlay.bounds
        };
        painter.rect_stroke(
            egui::Rect::from_two_pos(a, c),
            0.0,
            egui::Stroke::new(if selected { 2.0 } else { 1.0 }, color),
            egui::StrokeKind::Middle,
        );
    }
    if let Some(drag) = &mode.object_drag {
        let line = egui::Stroke::new(1.0, tokens.overlay.gizmo_ring);
        let hint = egui::Stroke::new(1.0, tokens.overlay.grid_major);
        for (lines, stroke) in [(&drag.snap.lines, line), (&drag.snap.spacing, hint)] {
            for l in lines {
                if let (Some(a), Some(b)) = (px(l.a), px(l.b)) {
                    painter.line_segment([a, b], stroke);
                }
            }
        }
    }
}
