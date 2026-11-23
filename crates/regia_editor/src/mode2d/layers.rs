// SPDX-License-Identifier: MPL-2.0
//! The 2D map's layer tree (ADR-7853): the tile and object layers of the loaded map, typed, with
//! their depth (the layer list is bottom first, so a layer's index is its draw depth and moving
//! it changes the depth), the active paint target, and object-layer editing (add, change, move,
//! delete objects with their properties). Every edit is one undo entry. `editor.tile_layers` is
//! the agent route; the 2D tab draws the same tree.

use regia_2d::tilemap::{
    LayerEditError, MapLayer, MapObject, ObjectLayer, ObjectShape, PropertyValue, layer_index_after_move,
};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{Mode2d, Mode2dError, mode2d_error, param, push_command};
use crate::command::EditorCommand;
use crate::viewport_agent::Handler;

/// One layer or object edit on the undo stack.
#[derive(Debug, Clone)]
pub enum LayerEditCommand {
    /// The layer at `from` moved to depth `to`.
    Move {
        /// The map document.
        path: PathBuf,
        /// Old index.
        from: usize,
        /// New index.
        to: usize,
    },
    /// The objects of one object layer changed.
    Objects {
        /// The map document.
        path: PathBuf,
        /// The layer.
        layer: usize,
        /// The layer before.
        before: ObjectLayer,
        /// The layer after.
        after: ObjectLayer,
    },
}

impl LayerEditCommand {
    fn path(&self) -> &PathBuf {
        match self {
            Self::Move { path, .. } | Self::Objects { path, .. } => path,
        }
    }

    fn write(&self, world: &mut World, undo: bool) {
        let Some(mut mode) = world.get_resource_mut::<Mode2d>() else {
            return;
        };
        if mode.doc.as_ref().is_none_or(|d| d.path != *self.path()) {
            tracing::warn!(target: "regia_editor::mode2d", "layer edit on {} skipped: another map is loaded", self.path().display());
            return;
        }
        match self {
            Self::Move { from, to, .. } => {
                let (from, to) = if undo { (*to, *from) } else { (*from, *to) };
                move_layer(&mut mode, from, to);
            }
            Self::Objects {
                layer, before, after, ..
            } => {
                let value = if undo { before } else { after };
                let mode = &mut *mode;
                if let Some(doc) = mode.doc.as_mut()
                    && let Some(MapLayer::Objects(slot)) = doc.map.layers.get_mut(*layer)
                {
                    *slot = value.clone();
                    doc.dirty = true;
                }
                if mode
                    .selected_object
                    .is_some_and(|id| !value.objects.iter().any(|o| o.id == id))
                {
                    mode.selected_object = None;
                }
            }
        }
    }
}

impl EditorCommand for LayerEditCommand {
    fn apply(&mut self, world: &mut World) {
        self.write(world, false);
    }

    fn revert(&mut self, world: &mut World) {
        self.write(world, true);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        match self {
            Self::Move { .. } => "Change layer depth".into(),
            Self::Objects { .. } => "Edit map objects".into(),
        }
    }
}

/// Moves a layer and keeps the active tile and object layers on the same layers.
fn move_layer(mode: &mut Mode2d, from: usize, to: usize) -> bool {
    let Some(doc) = mode.doc.as_mut() else {
        return false;
    };
    if !doc.map.move_layer(from, to) {
        return false;
    }
    doc.dirty = true;
    mode.layer = layer_index_after_move(mode.layer, from, to);
    mode.object_layer = mode.object_layer.map(|l| layer_index_after_move(l, from, to));
    true
}

fn edit_error(error: &LayerEditError) -> Mode2dError {
    match error {
        LayerEditError::NoLayer(_) | LayerEditError::NotObjectLayer(_) => {
            Mode2dError::BadLayer(error.to_string())
        }
        LayerEditError::NoObject { .. } => Mode2dError::NoObject(error.to_string()),
        LayerEditError::NotFinite => Mode2dError::BadParams(error.to_string()),
    }
}

impl Mode2d {
    /// Makes layer `layer` the active target: a tile layer becomes the paint layer, an object
    /// layer the edited object layer.
    pub fn select_layer(&mut self, layer: usize) -> Result<(), Mode2dError> {
        let doc = self.doc.as_ref().ok_or(Mode2dError::NoMap)?;
        match doc.map.layers.get(layer) {
            Some(MapLayer::Tiles(_)) => {
                self.layer = layer;
                self.object_layer = None;
                self.selected_object = None;
                Ok(())
            }
            Some(MapLayer::Objects(_)) => {
                if self.object_layer != Some(layer) {
                    self.selected_object = None;
                }
                self.object_layer = Some(layer);
                Ok(())
            }
            None => Err(Mode2dError::BadLayer(format!("no layer {layer}"))),
        }
    }

    /// Moves layer `from` to depth `to` (0 = bottom) and returns the undo command.
    pub fn set_layer_depth(
        &mut self,
        from: usize,
        to: usize,
    ) -> Result<Option<LayerEditCommand>, Mode2dError> {
        let doc = self.doc.as_ref().ok_or(Mode2dError::NoMap)?;
        let count = doc.map.layers.len();
        if from >= count || to >= count {
            return Err(Mode2dError::BadLayer(format!(
                "layer {from} to depth {to}: the map has {count} layers"
            )));
        }
        let path = doc.path.clone();
        if from == to {
            return Ok(None);
        }
        move_layer(self, from, to);
        Ok(Some(LayerEditCommand::Move { path, from, to }))
    }

    /// Runs `edit` on a copy of object layer `layer` and keeps it when it succeeds; returns the
    /// undo command and what `edit` returned.
    pub fn edit_objects<T>(
        &mut self,
        layer: usize,
        edit: impl FnOnce(&mut regia_2d::tilemap::TileMapAsset) -> Result<T, LayerEditError>,
    ) -> Result<(LayerEditCommand, T), Mode2dError> {
        let doc = self.doc.as_mut().ok_or(Mode2dError::NoMap)?;
        let before = doc
            .map
            .object_layer(layer)
            .cloned()
            .ok_or_else(|| Mode2dError::BadLayer(format!("layer {layer} is not an object layer")))?;
        let out = edit(&mut doc.map).map_err(|e| edit_error(&e))?;
        let after = doc
            .map
            .object_layer(layer)
            .cloned()
            .unwrap_or_else(|| before.clone());
        doc.dirty = true;
        Ok((
            LayerEditCommand::Objects {
                path: doc.path.clone(),
                layer,
                before,
                after,
            },
            out,
        ))
    }
}

/// A new rectangle object of `size` at map-pixel `position` (top-left, +Y down as in the file).
#[must_use]
pub fn new_object(name: &str, position: [f32; 2], size: [f32; 2]) -> MapObject {
    MapObject {
        id: 0,
        name: name.into(),
        class: String::new(),
        position,
        size,
        rotation: 0.0,
        shape: ObjectShape::Rect,
        properties: BTreeMap::new(),
    }
}

fn shape_name(shape: &ObjectShape) -> &'static str {
    match shape {
        ObjectShape::Rect => "rect",
        ObjectShape::Ellipse => "ellipse",
        ObjectShape::Point => "point",
        ObjectShape::Polygon(_) => "polygon",
        ObjectShape::Polyline(_) => "polyline",
        ObjectShape::Tile(_) => "tile",
    }
}

fn property_json(value: &PropertyValue) -> Value {
    match value {
        PropertyValue::Bool(b) => json!(b),
        PropertyValue::Int(i) => json!(i),
        PropertyValue::Float(f) => json!(f),
        PropertyValue::String(s) | PropertyValue::Color(s) | PropertyValue::File(s) => json!(s),
        PropertyValue::Object(id) => json!({ "object": id }),
    }
}

fn object_json(object: &MapObject) -> Value {
    json!({
        "id": object.id,
        "name": object.name,
        "class": object.class,
        "position": object.position,
        "size": object.size,
        "rotation": object.rotation,
        "shape": shape_name(&object.shape),
        "properties": object.properties.iter().map(|(k, v)| (k.clone(), property_json(v))).collect::<Map<_, _>>(),
    })
}

/// The layer tree of the loaded map.
#[must_use]
pub fn layers_json(mode: &Mode2d) -> Value {
    let Some(doc) = &mode.doc else {
        return json!({ "map": null, "layers": [], "active": null, "object_layer": null, "selected_object": null });
    };
    let layers: Vec<Value> = doc
        .map
        .layers
        .iter()
        .enumerate()
        .map(|(index, layer)| match layer {
            MapLayer::Tiles(t) => json!({
                "index": index, "depth": index, "name": t.name, "kind": "tiles", "visible": t.visible,
                "active": index == mode.layer,
            }),
            MapLayer::Objects(o) => json!({
                "index": index, "depth": index, "name": o.name, "kind": "objects", "visible": o.visible,
                "active": mode.object_layer == Some(index),
                "objects": o.objects.iter().map(object_json).collect::<Vec<_>>(),
            }),
        })
        .collect();
    json!({
        "map": doc.path.display().to_string(),
        "layers": layers,
        "active": mode.layer,
        "object_layer": mode.object_layer,
        "selected_object": mode.selected_object,
    })
}

fn bad(message: impl Into<String>) -> Mode2dError {
    Mode2dError::BadParams(message.into())
}

fn layer_param(mode: &Mode2d, params: &Value) -> Result<usize, Mode2dError> {
    let doc = mode.doc.as_ref().ok_or(Mode2dError::NoMap)?;
    match params.get("layer") {
        Some(Value::String(name)) => doc
            .map
            .layer_index(name)
            .ok_or_else(|| Mode2dError::BadLayer(format!("no layer named `{name}`"))),
        Some(v) => v
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .ok_or_else(|| bad("`layer` is a name or an index")),
        None => mode.object_layer.ok_or_else(|| bad("`layer` is required")),
    }
}

fn floats<const N: usize>(value: &Value, what: &str) -> Result<[f32; N], Mode2dError> {
    let items = value
        .as_array()
        .filter(|a| a.len() == N)
        .ok_or_else(|| bad(format!("`{what}` is {N} numbers")))?;
    let mut out = [0.0; N];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item
            .as_f64()
            .map(|f| f as f32)
            .filter(|f| f.is_finite())
            .ok_or_else(|| bad(format!("`{what}` holds finite numbers")))?;
    }
    Ok(out)
}

fn parse_property(value: &Value) -> Result<PropertyValue, Mode2dError> {
    match value {
        Value::Bool(b) => Ok(PropertyValue::Bool(*b)),
        Value::Number(n) => Ok(n.as_i64().map_or_else(
            || PropertyValue::Float(n.as_f64().unwrap_or(0.0)),
            PropertyValue::Int,
        )),
        Value::String(s) => Ok(PropertyValue::String(s.clone())),
        _ => Err(bad("a property value is a boolean, a number or a string")),
    }
}

/// Applies the fields of an `object` parameter to `object` (absent fields stay).
fn apply_object_fields(object: &mut MapObject, value: &Value) -> Result<(), Mode2dError> {
    let fields = value.as_object().ok_or_else(|| bad("`object` is an object"))?;
    for (key, value) in fields {
        match key.as_str() {
            "name" => {
                object.name = value
                    .as_str()
                    .ok_or_else(|| bad("`object.name` is a string"))?
                    .into()
            }
            "class" => {
                object.class = value
                    .as_str()
                    .ok_or_else(|| bad("`object.class` is a string"))?
                    .into()
            }
            "position" => object.position = floats(value, "object.position")?,
            "size" => object.size = floats(value, "object.size")?,
            "rotation" => object.rotation = floats::<1>(&json!([value]), "object.rotation")?[0],
            "shape" => {
                object.shape = match value.as_str() {
                    Some("rect") => ObjectShape::Rect,
                    Some("ellipse") => ObjectShape::Ellipse,
                    Some("point") => ObjectShape::Point,
                    _ => return Err(bad("`object.shape` is rect, ellipse or point")),
                }
            }
            "properties" => {
                let props = value
                    .as_object()
                    .ok_or_else(|| bad("`object.properties` is an object"))?;
                for (name, value) in props {
                    if value.is_null() {
                        object.properties.remove(name);
                    } else {
                        object.properties.insert(name.clone(), parse_property(value)?);
                    }
                }
            }
            other => return Err(bad(format!("unknown `object.{other}`"))),
        }
    }
    Ok(())
}

fn object_id(params: &Value) -> Result<u32, Mode2dError> {
    params
        .get("id")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| bad("`id` is an object id"))
}

/// Runs one `editor.tile_layers` action on `mode`; the undo command (if anything changed) and the
/// extra result fields.
fn run_action(mode: &mut Mode2d, params: &Value) -> Result<(Option<LayerEditCommand>, Value), Mode2dError> {
    let action = params.get("action").map_or(Some("list"), Value::as_str);
    match action {
        Some("list") => {
            mode.doc.as_ref().ok_or(Mode2dError::NoMap)?;
            Ok((None, Value::Null))
        }
        Some("select") => {
            let layer = layer_param(mode, params)?;
            mode.select_layer(layer)?;
            if let Some(id) = params.get("id") {
                let id = object_id(&json!({ "id": id }))?;
                let found = mode
                    .doc
                    .as_ref()
                    .and_then(|d| d.map.object_layer(layer))
                    .is_some_and(|l| l.objects.iter().any(|o| o.id == id));
                if !found {
                    return Err(Mode2dError::NoObject(format!("layer {layer} has no object {id}")));
                }
                mode.selected_object = Some(id);
            }
            Ok((None, Value::Null))
        }
        Some("move") => {
            let layer = layer_param(mode, params)?;
            let to = params
                .get("depth")
                .and_then(Value::as_u64)
                .and_then(|d| usize::try_from(d).ok())
                .ok_or_else(|| bad("`depth` is the new index, 0 = bottom"))?;
            Ok((mode.set_layer_depth(layer, to)?, Value::Null))
        }
        Some("object_add") => {
            let layer = layer_param(mode, params)?;
            let mut object = new_object("", [0.0, 0.0], [0.0, 0.0]);
            apply_object_fields(&mut object, params.get("object").unwrap_or(&json!({})))?;
            let (command, id) = mode.edit_objects(layer, |map| map.add_object(layer, object))?;
            mode.object_layer = Some(layer);
            mode.selected_object = Some(id);
            Ok((Some(command), json!(id)))
        }
        Some("object_set") => {
            let layer = layer_param(mode, params)?;
            let id = object_id(params)?;
            let doc = mode.doc.as_ref().ok_or(Mode2dError::NoMap)?;
            let mut object = doc
                .map
                .object_layer(layer)
                .ok_or_else(|| Mode2dError::BadLayer(format!("layer {layer} is not an object layer")))?
                .objects
                .iter()
                .find(|o| o.id == id)
                .cloned()
                .ok_or_else(|| Mode2dError::NoObject(format!("layer {layer} has no object {id}")))?;
            apply_object_fields(
                &mut object,
                params.get("object").ok_or_else(|| bad("`object` is required"))?,
            )?;
            let (command, _) = mode.edit_objects(layer, |map| map.replace_object(layer, id, object))?;
            Ok((Some(command), json!(id)))
        }
        Some("object_delete") => {
            let layer = layer_param(mode, params)?;
            let id = object_id(params)?;
            let (command, _) = mode.edit_objects(layer, |map| map.remove_object(layer, id))?;
            if mode.selected_object == Some(id) {
                mode.selected_object = None;
            }
            Ok((Some(command), json!(id)))
        }
        _ => Err(bad(
            "`action` is list, select, move, object_add, object_set or object_delete",
        )),
    }
}

/// Runs a layer action from the 2D tab or the agent and pushes its undo entry.
pub fn layer_action(world: &mut World, params: &Value) -> Result<Value, Mode2dError> {
    let mut mode = world.remove_resource::<Mode2d>().unwrap_or_default();
    let result = run_action(&mut mode, params);
    world.insert_resource(mode);
    let (command, id) = result?;
    if let Some(command) = command {
        push_command(world, Box::new(command));
    }
    let mut out = layers_json(world.resource::<Mode2d>());
    if !id.is_null() {
        out["id"] = id;
    }
    Ok(out)
}

/// `editor.tile_layers`.
fn tile_layers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    layer_action(world, params).map_err(|e| mode2d_error(&e))
}

const PARAMS: &[ParamSchema] = &[
    param(
        "action",
        "string",
        false,
        "list (default), select, move, object_add, object_set or object_delete.",
    ),
    param(
        "layer",
        "string",
        false,
        "Layer name or index (default for object actions: the active object layer).",
    ),
    param(
        "depth",
        "integer",
        false,
        "move: the layer's new index, 0 = bottom (drawn first).",
    ),
    param(
        "id",
        "integer",
        false,
        "select, object_set, object_delete: the object id.",
    ),
    param(
        "object",
        "object",
        false,
        "object_add / object_set: {name?, class?, position?: [x, y], size?: [w, h], rotation?, shape?: rect|ellipse|point, \
         properties?: {key: bool|number|string|null}} in map pixels (+Y down).",
    ),
];

/// The layer tree's agent method.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.tile_layers",
            tool: "tile_layers",
            description: "The 2D map's layer tree: lists the tile and object layers (kind, depth, objects with \
                their properties), selects the active paint layer or object layer (and an object), moves a layer to \
                another depth, and adds, changes or deletes objects of an object layer. Each change is one undo entry. \
                Errors in data.mode2d_error: mode2d_bad_params, mode2d_no_map, mode2d_bad_layer, mode2d_no_object.",
            params: PARAMS,
            result: "{ map, layers: [{ index, depth, name, kind, visible, active, objects? }], active, object_layer, selected_object, id? }",
            mutating: true,
            permission: Permission::EditScene,
        },
        tile_layers as Handler,
    )]
}
