// SPDX-License-Identifier: MPL-2.0
//! Pick from the stack under the cursor (P-073, ADR-7769): Alt+click in a viewport pane lists
//! every entity under the pointer, nearest first, with its kind icon; choosing a row selects it.
//! Also the agent surface of the viewport's other selection tools that have no pane of their
//! own: `editor.pick_stack`, `editor.select_similar` (P-065) and `editor.selection_root`
//! (P-074), and `editor.drop_preview`, the drop ghost's query (P-067). `editor.lasso_select` lives beside `editor.box_select` in
//! [`crate::viewport_agent`]. Book page: `book/agents/viewport-tools.md`.
//!
//! The stack is the front entity of the id buffer at the pixel (proxies and meshes), every mesh
//! the pixel's ray passes through (each mesh's BVH, the CPU fallback's cast without the
//! nearest-only cut), proxy entities whose position lies within [`PROXY_RADIUS_PX`] of the
//! pointer, and the terrain. Locked and editor-internal entities are left out.
//!
//! The list is keyboard operable: Up and Down move the highlight, Enter picks, Escape closes;
//! each row is also a focusable button. The `selection.pick_stack` command (rebindable in the
//! palette, no default chord) opens it under the pointer of the active pane without a click.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::selection::{RegionMode, Selection, SimilarBy};
use crate::viewport_agent::{Handler, P_HEIGHT, P_VIEW, P_WIDTH, entity_json, str_param};
use crate::views::ViewCamera;

/// How close (pane pixels) a proxy entity's position must project to the pointer to be listed.
pub const PROXY_RADIUS_PX: f32 = 10.0;

/// One entity under the cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct StackEntry {
    /// The entity a pick of this row selects (a model's owner, not its part).
    pub entity: Entity,
    /// Eye distance to the nearest point of it under the pixel, metres.
    pub distance: f32,
    /// The Outliner's kind glyph ([`crate::kind_icons::row_icon`]).
    pub glyph: &'static str,
    /// The row label (the Outliner's display name).
    pub name: String,
    /// The kind or role name (empty when it has none).
    pub kind: String,
}

/// Every entity under `pixel` of a `size` pane seen through `view`, nearest first (ties by
/// entity bits). `ids` is the pane's id buffer when it has one. See the module docs for what is
/// listed.
#[must_use]
pub fn pick_stack(
    world: &World,
    view: &ViewCamera,
    size: [u32; 2],
    ids: Option<&crate::picking::IdBuffer>,
    pixel: Vec2,
) -> Vec<StackEntry> {
    let _span = tracing::info_span!(target: "regia_editor", "pick_stack").entered();
    let size_v = Vec2::new(size[0] as f32, size[1] as f32);
    let (origin, direction) = view.ray(pixel, size_v);
    let start = (origin - view.eye).length();
    let mut hits: Vec<(Entity, f32)> = Vec::new();
    if let Some((entity, distance)) =
        ids.and_then(|ids| ids.at(pixel.x.max(0.0) as u32, pixel.y.max(0.0) as u32))
    {
        hits.push((entity, distance));
    }
    if direction.length_squared() > 0.5 {
        for entity in world
            .iter_entities()
            .filter(|e| e.contains::<regia_render::MeshRenderer>())
            .map(|e| e.id())
        {
            if !crate::viewport_ops::pickable(world, entity) {
                continue;
            }
            if let Some(hit) = crate::picking::raycast_entity(
                world,
                entity,
                origin,
                direction,
                crate::picking::FaceCull::None,
            ) {
                hits.push((hit.owner, start + hit.distance));
            }
        }
        if let Some((entity, t)) = crate::picking::pick_terrain(world, origin, direction, view.far) {
            hits.push((entity, start + t));
        }
    }
    // Proxies: entities without a mesh (and not a model whose parts draw), drawn as markers at
    // their position.
    let models: std::collections::HashSet<Entity> = world
        .iter_entities()
        .filter_map(|e| e.get::<crate::mesh_source::MeshPart>().map(|p| p.owner))
        .collect();
    for entity_ref in world.iter_entities() {
        let entity = entity_ref.id();
        if entity_ref.contains::<regia_render::MeshRenderer>()
            || entity_ref.contains::<regia_ecs::ecs::resource::IsResource>()
            || entity_ref.contains::<crate::mesh_source::MeshPart>()
            || models.contains(&entity)
        {
            continue;
        }
        let Some(position) = crate::viewport::world_position(world, entity) else {
            continue;
        };
        if let Some(p) = view.project(position, size_v)
            && (p - pixel).length() <= PROXY_RADIUS_PX
        {
            hits.push((entity, (position - view.eye).length()));
        }
    }
    hits.retain(|(entity, _)| {
        world.get_entity(*entity).is_ok()
            && !crate::visibility::is_hidden(world, *entity)
            && !crate::viewport_ops::is_locked(world, *entity)
    });
    hits.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.to_bits().cmp(&b.0.to_bits())));
    let mut out: Vec<StackEntry> = Vec::new();
    for (entity, distance) in hits {
        if out.iter().any(|e| e.entity == entity) {
            continue;
        }
        let icon = crate::kind_icons::row_icon(world, entity);
        out.push(StackEntry {
            entity,
            distance,
            glyph: icon.glyph,
            name: crate::outliner::display_name(world, entity),
            kind: icon
                .kind
                .clone()
                .or_else(|| icon.role.map(|r| r.name().to_owned()))
                .unwrap_or_default(),
        });
    }
    out
}

/// Which click opens the list (ADR-7769). Alt by default; `Off` leaves Alt+click a plain pick
/// (the `selection.pick_stack` command still opens it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StackClick {
    /// Alt+click (Shift and Ctrl may be held too: they choose the [`RegionMode`]).
    #[default]
    Alt,
    /// No click opens it.
    Off,
}

/// The pick-stack preferences (a world resource; absent means the defaults).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PickStackSettings {
    /// Which click opens the list.
    pub click: StackClick,
}

/// Whether a click with these modifiers opens the list.
#[must_use]
pub fn click_opens(world: &World, alt: bool) -> bool {
    match world
        .get_resource::<PickStackSettings>()
        .copied()
        .unwrap_or_default()
        .click
    {
        StackClick::Alt => alt,
        StackClick::Off => false,
    }
}

/// A request from the `selection.pick_stack` command: the active pane opens the list under its
/// pointer (or its center) on the next frame.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct PickStackRequest;

/// The open list (a world resource while it is shown).
#[derive(Resource, Debug, Clone)]
pub struct PickStackPopup {
    /// The pane widget that opened it (its egui id: main and instance panels never share one).
    pub pane: egui::Id,
    /// Where it opens (screen points).
    pub at: [f32; 2],
    /// The rows, nearest first.
    pub entries: Vec<StackEntry>,
    /// The highlighted row.
    pub cursor: usize,
    /// How the pick changes the selection (the modifiers at the click).
    pub mode: RegionMode,
    /// The egui frame whose keys were handled (egui may run a panel twice in one frame).
    last_frame: u64,
}

impl PickStackPopup {
    /// A list at `at` for the pane widget `pane`.
    #[must_use]
    pub fn new(pane: egui::Id, at: [f32; 2], entries: Vec<StackEntry>, mode: RegionMode) -> Self {
        Self {
            pane,
            at,
            entries,
            cursor: 0,
            mode,
            last_frame: u64::MAX,
        }
    }
}

/// What a key or a row click did to an open list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupKey {
    /// Highlight the row above.
    Up,
    /// Highlight the row below.
    Down,
    /// Pick the highlighted row.
    Pick,
    /// Close without picking.
    Close,
}

/// Applies `key` to the open list: moves the highlight, or picks (selects the row with the
/// list's mode) or closes, removing the list. Returns the picked entity.
pub fn popup_key(world: &mut World, key: PopupKey) -> Option<Entity> {
    let mut popup = world.get_resource::<PickStackPopup>()?.clone();
    let rows = popup.entries.len();
    match key {
        PopupKey::Up => popup.cursor = popup.cursor.checked_sub(1).unwrap_or(rows.saturating_sub(1)),
        PopupKey::Down => {
            popup.cursor = if popup.cursor + 1 >= rows {
                0
            } else {
                popup.cursor + 1
            }
        }
        PopupKey::Pick => {
            world.remove_resource::<PickStackPopup>();
            let entry = popup.entries.get(popup.cursor)?;
            let mut selection = world.get_resource::<Selection>().cloned().unwrap_or_default();
            selection.apply_region(&[entry.entity], popup.mode);
            world.insert_resource(selection);
            return Some(entry.entity);
        }
        PopupKey::Close => {
            world.remove_resource::<PickStackPopup>();
            return None;
        }
    }
    world.insert_resource(popup);
    None
}

/// Draws the open list for the pane widget `pane` (nothing when none is open for it) and handles its keys and
/// row clicks. Rows scale with the UI scale (egui points) and use the theme's text colors.
pub fn popup_ui(ui: &mut egui::Ui, world: &mut World, pane: egui::Id) {
    let Some(popup) = world.get_resource::<PickStackPopup>().cloned() else {
        return;
    };
    if popup.pane != pane {
        return;
    }
    let ctx = ui.ctx().clone();
    let frame_nr = ctx.cumulative_frame_nr();
    if popup.last_frame != frame_nr {
        let mut keyed = None;
        ctx.input_mut(|input| {
            for (key, action) in [
                (egui::Key::ArrowUp, PopupKey::Up),
                (egui::Key::ArrowDown, PopupKey::Down),
                (egui::Key::Enter, PopupKey::Pick),
                (egui::Key::Escape, PopupKey::Close),
            ] {
                if keyed.is_none() && input.consume_key(egui::Modifiers::NONE, key) {
                    keyed = Some(action);
                }
            }
        });
        if let Some(mut open) = world.get_resource_mut::<PickStackPopup>() {
            open.last_frame = frame_nr;
        }
        if let Some(key) = keyed {
            popup_key(world, key);
        }
    }
    let Some(popup) = world.get_resource::<PickStackPopup>().cloned() else {
        return;
    };
    let mut clicked = None;
    let area = egui::Area::new(egui::Id::new(("pick-stack", pane)))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(popup.at[0], popup.at[1]))
        .show(&ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new("Objects under the cursor").strong());
                for (row, entry) in popup.entries.iter().enumerate() {
                    let text = format!("{} {}  ({:.1} m)", entry.glyph, entry.name, entry.distance);
                    // The highlighted row is strong and underlined (no color of its own).
                    let text = if row == popup.cursor {
                        egui::RichText::new(text).strong().underline()
                    } else {
                        egui::RichText::new(text)
                    };
                    let response = crate::agent_parity::action_button_text(
                        ui,
                        &crate::agent_parity::VIEWPORT_PICK_STACK_ROW,
                        text,
                    )
                    .on_hover_text(if entry.kind.is_empty() {
                        "no kind"
                    } else {
                        entry.kind.as_str()
                    });
                    if row == popup.cursor && !ui.ctx().memory(|m| m.focused().is_some()) {
                        response.request_focus();
                    }
                    if response.clicked() {
                        clicked = Some(row);
                    }
                }
            });
        });
    if let Some(row) = clicked {
        if let Some(mut open) = world.get_resource_mut::<PickStackPopup>() {
            open.cursor = row;
        }
        popup_key(world, PopupKey::Pick);
    } else if ctx.input(|i| i.pointer.any_pressed())
        && ctx
            .input(|i| i.pointer.interact_pos())
            .is_some_and(|p| !area.response.rect.contains(p))
    {
        popup_key(world, PopupKey::Close);
    }
}

// ---------------------------------------------------------------------------------------------
// Agent surface

/// An invalid-params error with a lowercase `reason` code (listed on the book page).
fn refused(message: impl Into<String>, reason: &str) -> RpcError {
    let mut err = RpcError::invalid_params(message);
    if let Some(Value::Object(data)) = err.data.as_mut() {
        data.insert("reason".into(), Value::String(reason.into()));
    }
    err
}

fn entry_json(world: &World, entry: &StackEntry) -> Value {
    let mut out = entity_json(world, entry.entity);
    out["distance"] = json!(entry.distance);
    out["kind"] = json!(entry.kind);
    out["glyph"] = json!(entry.glyph);
    out
}

/// `editor.pick_stack`.
fn pick_stack_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let coord = |name: &str| {
        params
            .get(name)
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite())
            .map(|v| v as f32)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` is required")))
    };
    let pixel = Vec2::new(coord("x")?, coord("y")?);
    let request = crate::viewport_agent::pane_request(world, params)?;
    crate::mesh_source::resolve_mesh_sources(world);
    let cpu = params.get("cpu").and_then(Value::as_bool).unwrap_or(false)
        || crate::agent_ext::viewport_device(world).is_err();
    let (view, ids) = if cpu {
        (ViewCamera::of(request.kind, &request.camera), None)
    } else {
        let frame = crate::viewport_agent::id_frame(world, params)?;
        (frame.view, frame.ids)
    };
    let entries = pick_stack(world, &view, [request.width, request.height], ids.as_ref(), pixel);
    let rows: Vec<Value> = entries.iter().map(|e| entry_json(world, e)).collect();
    let mut selected = Value::Null;
    if let Some(index) = params.get("select").and_then(Value::as_u64) {
        let entry = entries.get(index as usize).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "`select` {index} is past the {} entries under the cursor",
                entries.len()
            ))
        })?;
        let mode = match str_param(params, "mode") {
            Some(name) => RegionMode::parse(name).ok_or_else(|| {
                RpcError::invalid_params(format!("unknown mode `{name}` (set, add, subtract, toggle)"))
            })?,
            None => RegionMode::Replace,
        };
        let mut selection = world.get_resource::<Selection>().cloned().unwrap_or_default();
        selection.apply_region(&[entry.entity], mode);
        world.insert_resource(selection);
        selected = json!(entry.entity.to_bits());
    }
    Ok(json!({
        "entities": rows,
        "method": if cpu { "cpu" } else { "gpu" },
        "selected": selected,
    }))
}

/// `editor.select_similar`.
fn select_similar_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = str_param(params, "by").unwrap_or("kind");
    let by = SimilarBy::parse(name).ok_or_else(|| {
        RpcError::invalid_params(format!("unknown `by` `{name}` (kind, components, asset, layer)"))
    })?;
    let found = crate::selection::select_similar(world, by).ok_or_else(|| {
        refused(
            "nothing is selected: select one or more entities to compare with",
            "nothing_selected",
        )
    })?;
    let rows: Vec<Value> = found.iter().map(|e| entity_json(world, *e)).collect();
    Ok(json!({ "by": by.name(), "entities": rows }))
}

/// `editor.selection_root`.
fn selection_root_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (entity bits) is required"))?;
    let entity = crate::command::entity_from_bits(bits)
        .filter(|e| world.get_entity(*e).is_ok())
        .ok_or_else(|| refused(format!("no entity {bits}"), "no_entity"))?;
    if let Some(set) = params.get("set").and_then(Value::as_bool) {
        set_selection_root(world, entity, set);
    }
    let probe = params
        .get("probe")
        .and_then(Value::as_u64)
        .and_then(crate::command::entity_from_bits)
        .filter(|e| world.get_entity(*e).is_ok());
    let mut out = entity_json(world, entity);
    out["root"] = json!(
        world
            .get::<crate::selection::EditorSelectionRoot>(entity)
            .is_some()
    );
    if let Some(probe) = probe {
        out["click_selects"] = json!(crate::selection::selection_root_of(world, probe, false).to_bits());
    }
    Ok(out)
}

/// Marks or unmarks `entity` as a selection root ([`crate::selection::EditorSelectionRoot`]) as
/// one undo step (which also marks the level dirty), registering the type so the level saves
/// it. Without a type registry (a bare world) the marker is set directly. Returns whether it
/// changed.
pub fn set_selection_root(world: &mut World, entity: Entity, on: bool) -> bool {
    use crate::command::EditorCommand;
    let Ok(entity_ref) = world.get_entity(entity) else {
        return false;
    };
    let had = entity_ref.contains::<crate::selection::EditorSelectionRoot>();
    if had == on {
        return false;
    }
    crate::selection::register_selection_types(world);
    let path = std::any::type_name::<crate::selection::EditorSelectionRoot>();
    if world.contains_resource::<regia_ecs::reflect::AppTypeRegistry>() {
        let command: Option<Box<dyn EditorCommand>> = if on {
            crate::command::AddComponent::capture(
                world,
                entity,
                path,
                Box::new(crate::selection::EditorSelectionRoot),
            )
            .ok()
            .map(|c| Box::new(c) as Box<dyn EditorCommand>)
        } else {
            crate::command::RemoveComponent::capture(world, entity, path)
                .ok()
                .map(|c| Box::new(c) as Box<dyn EditorCommand>)
        };
        if let Some(mut command) = command {
            command.apply(world);
            world
                .get_resource_or_insert_with(crate::command::UndoStack::new)
                .push(command);
            return world
                .get::<crate::selection::EditorSelectionRoot>(entity)
                .is_some()
                == on;
        }
    }
    let mut entity_mut = world.entity_mut(entity);
    if on {
        entity_mut.insert(crate::selection::EditorSelectionRoot);
    } else {
        entity_mut.remove::<crate::selection::EditorSelectionRoot>();
    }
    true
}

/// `editor.drop_preview`.
fn drop_preview_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (an asset under assets/) is required"))?
        .to_owned();
    let coord = |name: &str, default: f32| {
        params
            .get(name)
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite())
            .map_or(default, |v| v as f32)
    };
    let request = crate::viewport_agent::pane_request(world, params)?;
    let size = Vec2::new(request.width as f32, request.height as f32);
    let pixel = Vec2::new(coord("x", size.x * 0.5), coord("y", size.y * 0.5));
    let view = ViewCamera::of(request.kind, &request.camera);
    let (origin, direction) = view.ray(pixel, size);
    world.insert_resource(crate::content::ViewportDropAim {
        origin,
        direction,
        target_depth: (view.target - origin).dot(direction),
    });
    crate::mesh_source::resolve_mesh_sources(world);
    let preview = crate::content::preview_for_path(world, &path);
    let ghost = preview.map(|(transform, (min, max))| {
        json!({
            "translation": transform.translation.to_array(),
            "rotation": transform.rotation.to_array(),
            "scale": transform.scale.to_array(),
            "bounds_min": min.to_array(),
            "bounds_max": max.to_array(),
        })
    });
    let mut out = json!({ "path": path, "places": ghost.is_some(), "ghost": ghost });
    if params.get("place").and_then(Value::as_bool).unwrap_or(false) {
        let toast = crate::content::drop_at_aim(world, &path)
            .map_err(|reason| refused(format!("{reason}: open a project and a level first"), reason))?;
        out["placed"] = json!(toast);
    }
    Ok(out)
}

/// The agent methods of this module and of select similar and the selection root.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.drop_preview",
                tool: "drop_preview",
                description: "Where a Content Browser drag of an asset over a pane pixel would place it (the viewport's \
                    drop ghost): the placement transform and the asset's bounds, spawning nothing. With place: true, \
                    drops it there as a release does (one undo step).",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "The asset, project-relative (assets/...).",
                    },
                    ParamSchema {
                        name: "x",
                        kind: "number",
                        required: false,
                        description: "Pixel x (default the pane center).",
                    },
                    ParamSchema {
                        name: "y",
                        kind: "number",
                        required: false,
                        description: "Pixel y (default the pane center).",
                    },
                    P_WIDTH,
                    P_HEIGHT,
                    P_VIEW,
                    ParamSchema {
                        name: "place",
                        kind: "boolean",
                        required: false,
                        description: "Also drop it there (what the release does).",
                    },
                ],
                result: "{ path, places, ghost: { translation, rotation, scale, bounds_min, bounds_max } | null, placed? }; \
                    error codes drop_no_project, drop_no_level",
                mutating: true,
                permission: Permission::EditScene,
            },
            drop_preview_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.pick_stack",
                tool: "pick_stack",
                description: "Every entity under a pane pixel, nearest first (the viewport's Alt+click list): the id \
                    buffer's front entity, every mesh the pixel's ray passes through, proxies within 10 px and the \
                    terrain. Optionally selects one row.",
                params: &[
                    ParamSchema {
                        name: "x",
                        kind: "number",
                        required: true,
                        description: "Pixel x.",
                    },
                    ParamSchema {
                        name: "y",
                        kind: "number",
                        required: true,
                        description: "Pixel y.",
                    },
                    P_WIDTH,
                    P_HEIGHT,
                    P_VIEW,
                    ParamSchema {
                        name: "select",
                        kind: "number",
                        required: false,
                        description: "Row index to select (what choosing a row in the list does).",
                    },
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "With select: set (default), add, subtract or toggle.",
                    },
                    ParamSchema {
                        name: "cpu",
                        kind: "boolean",
                        required: false,
                        description: "Skip the id buffer (also used without a GPU adapter).",
                    },
                ],
                result: "{ entities: [{entity, name, distance, kind, glyph}], method: gpu | cpu, selected: bits | null }",
                mutating: false,
                permission: Permission::Read,
            },
            pick_stack_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.select_similar",
                tool: "select_similar",
                description: "Replace the selection with every level entity similar to a selected one (Edit > Select): \
                    same kind (declared kind, else role; every light type is one kind), same components, same \
                    asset (mesh, material, sound or prefab) or same layer. Locked, hidden and part entities are skipped.",
                params: &[ParamSchema {
                    name: "by",
                    kind: "string",
                    required: false,
                    description: "kind (default), components, asset or layer.",
                }],
                result: "{ by, entities: [{entity, name}] }; error code nothing_selected",
                mutating: false,
                permission: Permission::Read,
            },
            select_similar_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.selection_root",
                tool: "selection_root",
                description: "Read or set the selection-root marker (EditorSelectionRoot, saved with the level): a \
                    viewport click on a descendant selects the nearest marked ancestor; Ctrl+click picks the child.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "Entity bits.",
                    },
                    ParamSchema {
                        name: "set",
                        kind: "boolean",
                        required: false,
                        description: "true marks, false unmarks; omit to read.",
                    },
                    ParamSchema {
                        name: "probe",
                        kind: "number",
                        required: false,
                        description: "A descendant: the result's click_selects is what a plain click on it selects.",
                    },
                ],
                result: "{ entity, name, root, click_selects? }; error code no_entity",
                mutating: true,
                permission: Permission::EditScene,
            },
            selection_root_method as Handler,
        ),
    ]
}

/// The point `pixel` of a pane lands on when nothing is hit: the ground plane y = 0, else the
/// point at `depth` along the ray. Shared by the measure tool.
#[must_use]
pub fn ground_point(view: &ViewCamera, size: Vec2, pixel: Vec2, depth: f32) -> Vec3 {
    let (origin, direction) = view.ray(pixel, size);
    if direction.y.abs() > 1e-6 {
        let t = -origin.y / direction.y;
        if t > 0.0 && t.is_finite() {
            return origin + direction * t;
        }
    }
    origin + direction * depth
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_keys_wrap_pick_and_close() {
        let mut world = World::new();
        let ents: Vec<Entity> = (0..3).map(|_| world.spawn_empty().id()).collect();
        let entries = ents
            .iter()
            .map(|e| StackEntry {
                entity: *e,
                distance: 1.0,
                glyph: "",
                name: String::new(),
                kind: String::new(),
            })
            .collect();
        world.insert_resource(PickStackPopup::new(
            egui::Id::NULL,
            [0.0, 0.0],
            entries,
            RegionMode::Replace,
        ));
        popup_key(&mut world, PopupKey::Up);
        assert_eq!(
            world.resource::<PickStackPopup>().cursor,
            2,
            "Up wraps to the last row"
        );
        popup_key(&mut world, PopupKey::Down);
        popup_key(&mut world, PopupKey::Down);
        assert_eq!(world.resource::<PickStackPopup>().cursor, 1);
        assert_eq!(popup_key(&mut world, PopupKey::Pick), Some(ents[1]));
        assert!(
            !world.contains_resource::<PickStackPopup>(),
            "a pick closes the list"
        );
        assert_eq!(world.resource::<Selection>().entities(), &[ents[1]]);
        world.insert_resource(PickStackPopup::new(
            egui::Id::NULL,
            [0.0, 0.0],
            Vec::new(),
            RegionMode::Replace,
        ));
        assert_eq!(popup_key(&mut world, PopupKey::Close), None);
        assert!(!world.contains_resource::<PickStackPopup>());
    }
}
