// SPDX-License-Identifier: MPL-2.0
//! The Inspector panel: reflection-driven properties of the selection (`specs/19` § 2).
//!
//! Every field edit goes through the [`UndoStack`] as a [`SetProperty`] command, so inspector
//! edits are undoable exactly like gizmo drags and agent edits. The drawer is generic over
//! reflection ([`reflect_ui`], ported from bevy-inspector-egui): structs, tuple structs, tuples,
//! lists, arrays, maps, sets, enums with data (variant switch with defaults), `Option`, numbers,
//! strings, glam vectors/matrices, quaternions (Euler degrees) and colours. Per-field options
//! come from the editor attributes (`@Range`, `@Step`, `@Units`, `@Hidden`, `@ReadOnly`,
//! `@Multiline`, `@ColorHdr`, `@AssetFilter`, ...) and the unit/colour name conventions below.

use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::change_detection::Tick;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, PartialReflect, ReflectComponent, ReflectPath, ReflectRef};

use crate::command::EditorCommand as _;
use crate::command::{AddComponent, RemoveComponent, SetProperty, UndoStack};
use crate::dnd::ContentKind;
use crate::selection::Selection;

pub mod actions;
pub mod asset_field_agent;
pub mod categories;
pub mod clipboard;
pub mod color;
pub mod color_eyedropper;
pub mod condition;
pub mod curve_drawer;
pub mod defaults;
pub mod dropdown;
pub mod egui_utils;
pub mod expr;
pub mod flags;
pub mod impls;
pub mod inline_asset;
pub mod make_unique;
pub mod multi;
pub mod options;
pub mod picker_agent;
pub mod picker_links;
pub mod pinned;
pub mod reflect_ui;
pub mod search;
pub mod tooltip;

pub use multi::{
    EditError, EditReport, add_component_to, apply_prefab_override, commit_widget_changes,
    component_ui_multi, edit_value, inspect_entities, presence_suffix, remove_component_from,
    revert_prefab_override, set_transforms,
};
pub use reflect_ui::{Change, InspectorContext, InspectorUi, ListOp};

/// A field value in a widget-friendly form (the pre-port classifier, kept for the colour and
/// number views the tests pin).
#[derive(Debug)]
#[allow(dead_code)] // some payloads are only read by the tests' pattern matches
enum FieldView {
    /// A number field.
    Number(f64),
    /// A boolean field.
    Bool(bool),
    /// A string field.
    Str(String),
    /// A `Vec3` field.
    Vec3(Vec3),
    /// A quaternion field.
    Quat(Quat),
    /// A linear-RGB colour field: `[f32; 3]`/`[f32; 4]` (light colours) or a `Vec3`/`Vec4` whose
    /// name says colour (`is_color_name`). The `alpha` flag tells the widget kind.
    Color {
        /// Linear RGB(A), channels possibly above 1 (HDR light colours).
        rgba: [f32; 4],
        /// Whether the fourth channel is authored.
        alpha: bool,
    },
    /// An enum field: variant names and the active one.
    Enum {
        /// All variant names.
        variants: Vec<String>,
        /// The active variant's name.
        current: String,
    },
    /// Anything else: nested fields still get drawn when the value is a struct.
    Opaque,
}

/// Whether a field name means a colour (`specs/19` § 2 "per-type custom drawers: colors").
#[must_use]
pub fn is_color_name(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    lowered.ends_with("color")
        || lowered.ends_with("colour")
        || lowered == "tint"
        || lowered == "emissive"
        || lowered.starts_with("emissive_")
}

/// The physical unit a field is edited in, inferred from the field's (and its component's)
/// name (`specs/19` § 2 "per-type custom drawers: ... units"). Engine conventions: angles and
/// fovs are radians, distances metres, light intensities in physical units (`specs/13` § 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FieldUnit {
    /// No unit.
    #[default]
    None,
    /// Metres (positions, radii, clip planes).
    Metres,
    /// Metres per second (speeds).
    MetresPerSecond,
    /// Degrees — the stored value is **radians**, the widget converts.
    Degrees,
    /// Lux (directional-light illuminance).
    Lux,
    /// Lumens (point-light power).
    Lumens,
    /// Candela (spot intensity).
    Candela,
    /// Hertz.
    Hertz,
}

impl FieldUnit {
    /// The label suffix.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Metres => " m",
            Self::MetresPerSecond => " m/s",
            Self::Degrees => "°",
            Self::Lux => " lx",
            Self::Lumens => " lm",
            Self::Candela => " cd",
            Self::Hertz => " Hz",
        }
    }
}

/// Infers a field's unit from its name and its component's type path.
#[must_use]
pub fn field_unit(component: &str, field: &str) -> FieldUnit {
    let lowered = field.to_ascii_lowercase();
    let component = component.to_ascii_lowercase();
    // Angles are stored in radians (regia_render::Camera, regia_framework::CameraRig,
    // SpotLight cone angles); artists think in degrees.
    if lowered.starts_with("fov")
        || lowered == "pitch"
        || lowered == "yaw"
        || lowered == "roll"
        || lowered == "heading"
        || lowered.starts_with("angle")
        || lowered.ends_with("_angle")
    {
        return FieldUnit::Degrees;
    }
    if lowered == "illuminance" || component.contains("directionallight") && lowered == "intensity" {
        return FieldUnit::Lux;
    }
    if component.contains("pointlight") && lowered == "intensity" {
        return FieldUnit::Lumens;
    }
    if component.contains("spotlight") && lowered == "intensity" {
        return FieldUnit::Candela;
    }
    if lowered.ends_with("frequency_hz") {
        return FieldUnit::Hertz;
    }
    if lowered == "speed" || lowered == "velocity" {
        return FieldUnit::MetresPerSecond;
    }
    if matches!(
        lowered.as_str(),
        "translation"
            | "position"
            | "distance"
            | "radius"
            | "range"
            | "near"
            | "far"
            | "ortho_size"
            | "amplitude"
    ) || lowered.ends_with("_distance")
    {
        return FieldUnit::Metres;
    }
    FieldUnit::None
}

/// Reads a field into a widget-friendly view.
#[cfg_attr(not(test), allow(dead_code))]
fn view_field(value: &dyn PartialReflect) -> FieldView {
    if let Some(b) = value.try_downcast_ref::<bool>() {
        return FieldView::Bool(*b);
    }
    if let Some(s) = value.try_downcast_ref::<String>() {
        return FieldView::Str(s.clone());
    }
    if let Some(v) = value.try_downcast_ref::<Vec3>() {
        return FieldView::Vec3(*v);
    }
    if let Some(q) = value.try_downcast_ref::<Quat>() {
        return FieldView::Quat(*q);
    }
    // Light and material colours are `[f32; 3]`/`[f32; 4]` linear RGB(A).
    if let Some(c) = value.try_downcast_ref::<[f32; 3]>() {
        return FieldView::Color {
            rgba: [c[0], c[1], c[2], 1.0],
            alpha: false,
        };
    }
    if let Some(c) = value.try_downcast_ref::<[f32; 4]>() {
        return FieldView::Color {
            rgba: *c,
            alpha: true,
        };
    }
    if let Some(n) = number_of(value) {
        return FieldView::Number(n);
    }
    if let ReflectRef::Enum(e) = value.reflect_ref() {
        let variants = value
            .get_represented_type_info()
            .and_then(|info| match info {
                regia_ecs::reflect::TypeInfo::Enum(enum_info) => {
                    Some(enum_info.variant_names().iter().map(|n| n.to_string()).collect())
                }
                _ => None,
            })
            .unwrap_or_default();
        return FieldView::Enum {
            variants,
            current: e.variant_name().to_string(),
        };
    }
    FieldView::Opaque
}

/// Reads a reflected number as f64 (floats and ints).
#[cfg_attr(not(test), allow(dead_code))]
fn number_of(value: &dyn PartialReflect) -> Option<f64> {
    if let Some(v) = value.try_downcast_ref::<f64>() {
        return Some(*v);
    }
    if let Some(v) = value.try_downcast_ref::<f32>() {
        return Some(f64::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<i64>() {
        return Some(*v as f64);
    }
    if let Some(v) = value.try_downcast_ref::<u64>() {
        return Some(*v as f64);
    }
    if let Some(v) = value.try_downcast_ref::<i32>() {
        return Some(f64::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<u32>() {
        return Some(f64::from(*v));
    }
    None
}

/// Quaternion → Euler degrees, XYZ order (standard rotation-matrix formulas; see
/// `docs/provenance.md`).
pub(crate) fn quat_to_euler_deg(q: &Quat) -> [f32; 3] {
    // R = Rz(z) · Ry(y) · Rx(x) (the yaw-pitch-roll / ZYX composition); extraction per the
    // standard rotation-matrix formulas (see docs/provenance.md).
    let m = regia_core::math::Mat3::from_quat(*q);
    let rx = f32::atan2(m.y_axis.z, m.z_axis.z);
    let ry = f32::atan2(
        -m.x_axis.z,
        f32::sqrt(m.x_axis.x * m.x_axis.x + m.x_axis.y * m.x_axis.y),
    );
    let rz = f32::atan2(m.x_axis.y, m.x_axis.x);
    [rx.to_degrees(), ry.to_degrees(), rz.to_degrees()]
}

/// Draws the Inspector for `world`'s selection (or its locked entity), and the pinned Inspector
/// windows.
#[tracing::instrument(target = "regia_editor", skip_all, name = "inspector_panel")]
pub fn inspector_panel(ui: &mut egui::Ui, world: &mut World) {
    // An armed entity eyedropper takes the entity picked since the last frame (ADR-7786).
    let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
    let _ = picker_links::resolve_eyedropper(world, escape);
    // An armed color eyedropper takes a window pixel (P-133, ADR-8106).
    color_eyedropper::frame(ui.ctx(), world);
    draw_inspector(ui, world);
    // The pinned windows once per pass, however many Inspector instances draw (P-003).
    let pass = ui.ctx().cumulative_pass_nr();
    let pinned_pass = egui::Id::new("inspector-pinned-windows-pass");
    if ui.ctx().data(|d| d.get_temp::<u64>(pinned_pass)) != Some(pass) {
        ui.ctx().data_mut(|d| d.insert_temp(pinned_pass, pass));
        pinned::draw_pinned_windows(ui.ctx(), world);
    }
    // After the fields asked for their pictures: keep repainting while one is on its way, as the
    // Content Browser grid does, so a picture shows without the grid open (rev-claude-59 finding 8).
    if thumbnails_pending(world) {
        ui.ctx().request_repaint();
    }
}

/// The search box's widget id (Ctrl+F focuses it).
pub const SEARCH_BOX_ID: &str = "inspector_property_search";

/// Entities with more components than this get a component filter box.
pub const COMPONENT_FILTER_MIN: usize = 6;

/// [`inspector_panel`]'s drawing.
fn draw_inspector(ui: &mut egui::Ui, world: &mut World) {
    let selection = world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .clone();
    let locked = pinned::locked_entity(world);
    // The kept component copies belong to one inspected entity (ADR-7326).
    let single = match locked {
        Some(entity) => Some(entity),
        None if selection.len() > 1 => None,
        None => selection.primary(),
    };
    retain_cached_values(world, single);
    prepare_thumbnails(world);
    if locked.is_none() && selection.len() > 1 {
        inspector_panel_multi(ui, world, &selection);
        return;
    }
    let Some(entity) = single else {
        ui.centered_and_justified(|ui| {
            ui.weak("(nothing selected)");
        });
        return;
    };
    if world.get_entity(entity).is_err() {
        ui.weak("(entity gone)");
        return;
    }
    inspector_header(ui, world, entity, locked.is_some());
    draw_entity(ui, world, entity, false);
}

/// The header: title, lock, Open pinned inspector, the property search and Modified only.
fn inspector_header(ui: &mut egui::Ui, world: &mut World, entity: Entity, locked: bool) {
    ui.horizontal(|ui| {
        ui.strong("Inspector");
        let mut lock = locked;
        let response =
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::INSPECTOR_LOCK, &mut lock);
        let spoken = if locked {
            "Unlock the Inspector (follow the selection)"
        } else {
            "Lock the Inspector to this entity"
        };
        if crate::a11y_audit::name(response, egui::WidgetType::Button, spoken).changed() {
            pinned::set_lock(world, lock.then_some(entity));
        }
        let response = crate::agent_parity::action_button_text(
            ui,
            &crate::agent_parity::INSPECTOR_OPEN_PINNED,
            "\u{1f5d7}",
        );
        if crate::a11y_audit::name(response, egui::WidgetType::Button, "Open pinned inspector").clicked() {
            pinned::open_pinned(world, entity);
        }
        // Raw: hidden fields and structural components, read-only (inspector::clipboard, P-120).
        let mut raw = world
            .get_resource::<clipboard::InspectorRawView>()
            .is_some_and(|raw| raw.0);
        let response =
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::INSPECTOR_RAW_VIEW, &mut raw);
        if response
            .on_hover_text("Show hidden fields and structural components, read-only")
            .changed()
        {
            world.insert_resource(clipboard::InspectorRawView(raw));
        }
    });
    clipboard::draw_notice(ui, world);
    search_controls(ui, world);
}

/// The property search box (Ctrl+F, Esc clears) and **Modified only**, for one entity or a
/// multi-selection (P-110, P-116).
fn search_controls(ui: &mut egui::Ui, world: &mut World) {
    let mut search = world
        .get_resource_or_insert_with(search::InspectorSearch::default)
        .clone();
    let before = search.clone();
    // Each Inspector instance has its own search box (P-003; the plain id for instance 0).
    let search_id = crate::panel_instances::scoped_id(world, SEARCH_BOX_ID);
    // Ctrl+F while the pointer is over the Inspector, or the keyboard focus is on one of its
    // widgets (review sprint-v06-rev-late-k #18), focuses search.
    let clip = ui.clip_rect();
    let hovered = ui.ctx().pointer_hover_pos().is_some_and(|pos| clip.contains(pos));
    let layer = ui.layer_id();
    let focused = ui.memory(|m| m.has_focus(search_id))
        || ui
            .memory(|m| m.focused())
            .and_then(|id| ui.ctx().read_response(id))
            .is_some_and(|r| r.layer_id == layer && clip.contains(r.rect.center()));
    if (hovered || focused)
        && ui.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::F,
            ))
        })
    {
        ui.memory_mut(|m| m.request_focus(search_id));
    }
    ui.horizontal(|ui| {
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let response = ui.add(
            egui::TextEdit::singleline(&mut search.query)
                .id(search_id)
                .hint_text("Search properties (Ctrl+F)")
                .desired_width(
                    (ui.available_width() - ui.spacing().interact_size.x * 4.0)
                        .max(ui.spacing().interact_size.x * 2.0),
                ),
        );
        let response = crate::a11y_audit::name(response, egui::WidgetType::TextEdit, "Search properties");
        if escape && (response.has_focus() || response.lost_focus()) {
            search.query.clear();
        }
        crate::agent_parity::action_toggle(
            ui,
            &crate::agent_parity::INSPECTOR_MODIFIED_ONLY,
            &mut search.modified_only,
        )
        .on_hover_text("Show only fields that differ from their default (or the prefab's value)");
    });
    ui.separator();
    if search != before {
        world.insert_resource(search);
    }
}

/// The entity's name, or its id when it has none.
pub(crate) fn entity_label(world: &World, entity: Entity) -> String {
    world
        .get::<regia_scene::prelude::Name>(entity)
        .map(|n| n.as_str().to_string())
        .unwrap_or_else(|| format!("{:016x}", entity.to_bits()))
}

/// Draws one entity's components (the Inspector body, also a pinned Inspector window's).
/// `window`: a pinned window, which draws every field (no search or Modified only filter).
pub(crate) fn draw_entity(ui: &mut egui::Ui, world: &mut World, entity: Entity, window: bool) {
    if world.get_entity(entity).is_err() {
        ui.weak("(entity gone)");
        return;
    }
    pinned::ensure_pins_loaded(world);
    let name = entity_label(world, entity);
    ui.label(format!("Entity: {name}"));
    let parent = world.get::<ChildOf>(entity).map(|c| c.parent().to_bits());
    ui.weak(format!(
        "id {} · parent {}",
        entity.to_bits(),
        parent.unwrap_or(0)
    ))
    .on_hover_text("Parent 0 = root");
    crate::level_variant::inspector_entity_header(ui, world, entity);
    ui.separator();
    // Live data of the DCC feed is read-only until a bake (specs/60 § 2.4).
    if let Some(owned) = world.get::<regia_scene::prelude::FeedOwned>(entity) {
        ui.label(format!(
            "FEED: owned by the DCC feed item {:?}. Bake it to edit it here.",
            owned.key
        ));
        ui.separator();
        ui.disable();
    }

    // Shared, not copied: a project of 50 000 assets would otherwise copy the list every frame.
    let assets = world
        .get_resource::<AssetCatalog>()
        .map(|catalog| std::sync::Arc::clone(&catalog.entries))
        .unwrap_or_default();
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let components = component_names(world, &registry, entity);
    drop(registry);
    let mut search = if window {
        search::InspectorSearch::default()
    } else {
        world
            .get_resource::<search::InspectorSearch>()
            .cloned()
            .unwrap_or_default()
    };
    let shown_count = components.iter().filter(|(_, full)| !is_structural(full)).count();
    if !window && shown_count > COMPONENT_FILTER_MIN {
        let response = ui.add(
            egui::TextEdit::singleline(&mut search.component_filter)
                .hint_text("Filter components")
                .desired_width(f32::INFINITY),
        );
        let response = crate::a11y_audit::name(response, egui::WidgetType::TextEdit, "Filter components");
        if response.changed() {
            world
                .get_resource_or_insert_with(search::InspectorSearch::default)
                .component_filter
                .clone_from(&search.component_filter);
        }
    }
    let component_filter = search.component_filter.trim().to_owned();

    // The component a header's (or its body's) Remove asked for this frame (ADR-7472).
    let mut remove: Option<String> = None;
    let mut drawn = 0usize;
    let raw = world
        .get_resource::<clipboard::InspectorRawView>()
        .is_some_and(|raw| raw.0);
    for (short, full) in &components {
        if is_structural(full) {
            // Structural; edited through the outliner / level flows. The raw view shows it
            // read-only (P-120).
            if raw && !window {
                egui::CollapsingHeader::new(format!("{short} (structural, read-only)"))
                    .id_salt(("raw", full))
                    .show(ui, |ui| {
                        ui.add_enabled_ui(false, |ui| {
                            component_ui_with(ui, world, entity, full, std::sync::Arc::clone(&assets), None);
                        });
                    });
            }
            continue;
        }
        if !window
            && shown_count > COMPONENT_FILTER_MIN
            && !component_filter.is_empty()
            && regia_ui::fuzzy::fuzzy_match(&component_filter, short).is_none()
        {
            continue;
        }
        let filter = component_row_filter(world, entity, full, &search);
        if filter.as_ref().is_some_and(|filter| !filter.has_rows()) {
            continue;
        }
        drawn += 1;
        let removable = component_removable(short, full);
        // Salted by the full path: two components may share a short name.
        let header = egui::CollapsingHeader::new(short.clone())
            .id_salt(full)
            .default_open(*full == "regia_scene::transform::Transform");
        // A search or Modified only expands every component with a matching row.
        let header = if filter.is_some() {
            header.open(Some(true))
        } else {
            header
        };
        let shown = header.show(ui, |ui| {
            ui.horizontal(|ui| {
                clipboard::component_tools_row(ui, world, entity, full, short);
                if removable && remove_component_button(ui, short) {
                    remove = Some(full.clone());
                }
            });
            // Overrides whose field row drew a marker (with its menu) get no second row
            // below; every other override keeps one (ADR-7744).
            let (_, marked) =
                component_ui_with(ui, world, entity, full, std::sync::Arc::clone(&assets), filter);
            multi::prefab_override_controls(ui, world, &[entity], full, &marked);
            crate::inspector_variant::variant_override_controls(ui, world, entity, full);
            crate::level_variant::inspector_controls(ui, world, entity, full);
            // Procedural generators: parameters with overrides, run controls (spec 67 § 4).
            if *full == "regia_procgen::world::ProcGenerator" {
                ui.separator();
                crate::procgen::inspector::generator_inspector(ui, world, entity);
            }
            // A placed destructible: what its asset holds (ADR-9750).
            if *full == "regia_physics::destruction::source::DestructibleSource" {
                ui.separator();
                crate::fracture_tools::inspector_section(ui, world, entity);
            }
            // Gait and foot placement authoring: ranges, leg pickers, presets, .gait.ron (ADR-8806).
            if *full == <regia_animation::gait_rig::GaitRig as regia_ecs::reflect::TypePath>::type_path() {
                ui.separator();
                crate::anim::locomotion_panel::gait_rig_section(ui, world, entity);
            }
            if *full == <regia_animation::foot_ik::FootPlacement as regia_ecs::reflect::TypePath>::type_path()
            {
                ui.separator();
                crate::anim::locomotion_panel::foot_placement_section(ui, world, entity);
            }
        });
        shown.header_response.context_menu(|ui| {
            if clipboard::component_menu_items(ui, world, entity, full) {
                ui.close();
            }
            if removable && remove_component_menu_item(ui, short) {
                remove = Some(full.clone());
                ui.close();
            }
        });
    }
    if drawn == 0 && (search.filtering() || !component_filter.is_empty()) {
        ui.weak(if search.modified_only && !search.searching() {
            "No field differs from its default"
        } else {
            "No field matches"
        });
    }
    if let Some(full) = remove {
        remove_single_component(world, entity, &full);
    }

    // Add components (each component's header removes it).
    if !window {
        ui.separator();
        clipboard::paste_new_button(ui, world, entity);
        categories::add_component_picker(ui, world, &[entity]);
    }
}

/// Structural components the Inspector does not list (the outliner and level flows edit them).
pub(crate) fn is_structural(full: &str) -> bool {
    full == "regia_scene::components::EntityGuid"
        || full == "regia_scene::level::LevelMember"
        || full == "regia_scene::level::Persistent"
}

/// The search / Modified only row filter of `entity`'s component `full` (`None` when no filter is
/// on, or the component cannot be reflected).
fn component_row_filter(
    world: &World,
    entity: Entity,
    full: &str,
    search: &search::InspectorSearch,
) -> Option<search::RowFilter> {
    if !search.filtering() {
        return None;
    }
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let reflect = registry
        .get_with_type_path(full)?
        .data::<ReflectComponent>()?
        .clone();
    drop(registry);
    let value = reflect.reflect(world.get_entity(entity).ok()?)?;
    search::row_filter(world, entity, full, value.as_partial_reflect(), search)
}

/// Keeps the thumbnail cache asset pickers draw from current: the Content Browser's cache
/// (created when a project is open and nothing made it yet), pointed at the project when it has
/// no root yet, and pumped
/// once per inspector frame. Without a project there are no pictures and no cache.
fn prepare_thumbnails(world: &mut World) {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone());
    if root.is_none() && !world.contains_resource::<crate::thumb_cache::ThumbCache>() {
        return;
    }
    let mut thumbs = world.get_resource_or_insert_with(crate::thumb_cache::ThumbCache::default);
    let thumbs = thumbs.bypass_change_detection();
    // The Content Browser points the cache at its project; only a cache with none takes ours.
    if thumbs.root().is_none() && root.is_some() {
        thumbs.set_root(root.as_deref());
    }
    thumbs.pump();
    // Model and material pictures render on the GPU: a few per frame here too, so an asset field
    // gets its picture without the Content Browser grid open (rev-claude-59 finding 8).
    if thumbs.gpu_waiting() == 0 {
        return;
    }
    let Some(device) = world.get_resource::<regia_render::RenderDevice>().cloned() else {
        return;
    };
    let library = world
        .get_resource_or_insert_with(crate::mesh_source::MeshLibrary::default)
        .clone();
    if let Some(mut thumbs) = world.get_resource_mut::<crate::thumb_cache::ThumbCache>() {
        thumbs.bypass_change_detection().render_meshes(
            &device,
            &library,
            crate::content::MESH_THUMBS_PER_FRAME,
        );
    }
}

/// Whether a picture the thumbnail cache was asked for is still on its way: queued, with the
/// worker, or waiting for a GPU this world has.
fn thumbnails_pending(world: &World) -> bool {
    world
        .get_resource::<crate::thumb_cache::ThumbCache>()
        .is_some_and(|thumbs| {
            thumbs.busy()
                || (thumbs.gpu_waiting() > 0 && world.contains_resource::<regia_render::RenderDevice>())
        })
}

/// Whether the single-entity Inspector offers Remove for a component: everything but the
/// transform and the name (the outliner owns those) and the structural level markers.
fn component_removable(short: &str, full: &str) -> bool {
    !matches!(short, "Name" | "EntityGuid" | "LevelMember" | "Persistent")
        && full != "regia_scene::transform::Transform"
}

/// The Remove button at the top of a component's body (`Remove <short>`, ADR-7472).
fn remove_component_button(ui: &mut egui::Ui, short: &str) -> bool {
    let spoken = format!("Remove {short}");
    ui.horizontal(|ui| {
        let response = crate::agent_parity::action_button_text(
            ui,
            &crate::agent_parity::INSPECTOR_REMOVE_COMPONENT,
            "\u{1f5d9} Remove",
        );
        crate::a11y_audit::name(response, egui::WidgetType::Button, &spoken).clicked()
    })
    .inner
}

/// The Remove item of a component header's context menu (`Remove <short>`, ADR-7472).
fn remove_component_menu_item(ui: &mut egui::Ui, short: &str) -> bool {
    crate::agent_parity::action_button_text(
        ui,
        &crate::agent_parity::INSPECTOR_REMOVE_COMPONENT,
        format!("Remove {short}"),
    )
    .clicked()
}

/// Removes component `full` (type path) from `entity` as one undo step, like
/// `editor.remove_component`.
fn remove_single_component(world: &mut World, entity: Entity, full: &str) {
    // `RemoveComponent::capture` clones dynamically where `reflect_clone` cannot (components
    // holding `[f32; 3]`): without it the removal could not be undone. The full path, not the
    // short name: two components may share a short name.
    match RemoveComponent::capture(world, entity, full) {
        Ok(mut command) => {
            command.apply(world);
            let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
            stack.seal();
            stack.push(Box::new(command));
            stack.seal();
        }
        Err(err) => tracing::warn!(target: "regia_editor", "remove component: {err}"),
    }
}

/// Inspector for two or more entities (ADR-3948). Equal fields show the sample value; differing
/// fields show a mixed marker. A component on `k` of `n` stays editable on those `k`.
#[tracing::instrument(target = "regia_editor", skip_all, name = "inspector_multi")]
fn inspector_panel_multi(ui: &mut egui::Ui, world: &mut World, selection: &Selection) {
    let entities: Vec<Entity> = selection.entities().to_vec();
    let primary = selection.primary();
    let selected = entities.len();
    ui.horizontal(|ui| {
        ui.strong("Inspector");
        ui.weak(format!("{selected} selected"));
    });
    // Search and Modified only filter the multi-selection's rows too (P-110, P-116).
    search_controls(ui, world);
    let search = world
        .get_resource::<search::InspectorSearch>()
        .cloned()
        .unwrap_or_default();
    if entities
        .iter()
        .any(|entity| world.get::<regia_scene::prelude::FeedOwned>(*entity).is_some())
    {
        ui.label(
            "FEED: one or more selected entities are owned by the DCC feed. Bake them to edit them here.",
        );
        ui.separator();
        ui.disable();
    }

    let assets = world
        .get_resource::<AssetCatalog>()
        .map(|catalog| std::sync::Arc::clone(&catalog.entries))
        .unwrap_or_default();
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut union: std::collections::BTreeMap<String, (String, usize)> = std::collections::BTreeMap::new();
    for entity in &entities {
        for (short, full) in component_names(world, &registry, *entity) {
            let entry = union.entry(full).or_insert((short, 0));
            entry.1 += 1;
        }
    }
    let mut rows: Vec<(String, String, usize)> = union
        .iter()
        .map(|(full, (short, count))| (short.clone(), full.clone(), *count))
        .collect();
    rows.sort();
    drop(registry);

    let mut drawn = 0usize;
    for (short, full, present) in &rows {
        if full == "regia_scene::components::EntityGuid"
            || full == "regia_scene::level::LevelMember"
            || full == "regia_scene::level::Persistent"
        {
            continue;
        }
        let filter = multi::multi_row_filter(world, &entities, primary, full, &search);
        if filter.as_ref().is_some_and(|filter| !filter.has_rows()) {
            continue;
        }
        drawn += 1;
        let title = match presence_suffix(*present, selected) {
            Some(suffix) => format!("{short} {suffix}"),
            None => short.clone(),
        };
        let header = egui::CollapsingHeader::new(title)
            .id_salt(full)
            .default_open(full == "regia_scene::transform::Transform");
        // A search or Modified only expands every component with a matching row.
        let header = if filter.is_some() {
            header.open(Some(true))
        } else {
            header
        };
        header.show(ui, |ui| {
            multi::component_ui_multi_filtered(
                ui,
                world,
                &entities,
                primary,
                full,
                std::sync::Arc::clone(&assets),
                filter,
            );
            if full == "regia_procgen::world::ProcGenerator"
                && primary
                    .is_some_and(|entity| crate::command::read_property(world, entity, full, "").is_ok())
            {
                ui.separator();
                if let Some(entity) = primary {
                    crate::procgen::inspector::generator_inspector(ui, world, entity);
                }
            }
        });
    }

    if drawn == 0 && search.filtering() {
        ui.weak(if search.modified_only && !search.searching() {
            "No field differs from its default"
        } else {
            "No field matches"
        });
    }
    ui.separator();
    // The searchable add-component picker: a type on every selected entity is grayed.
    categories::add_component_picker(ui, world, &entities);
    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
        ui.separator();
        ui.horizontal(|ui| {
            ui.menu_button("Remove component", |ui| {
                for (short, full, _) in &rows {
                    if matches!(
                        short.as_str(),
                        "Name" | "EntityGuid" | "LevelMember" | "Persistent"
                    ) || full == "regia_scene::transform::Transform"
                    {
                        continue;
                    }
                    if ui.button(short).clicked() {
                        if let Err(err) = remove_component_from(world, &entities, full) {
                            tracing::warn!(target: "regia_editor", "remove component: {err}");
                        }
                        ui.close();
                    }
                }
            });
        });
    });
}

/// `(short, full)` names of the entity's components, sorted.
fn component_names(
    world: &World,
    registry: &regia_ecs::reflect::TypeRegistry,
    entity: Entity,
) -> Vec<(String, String)> {
    let Ok(infos) = world.inspect_entity(entity) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = infos
        .map(|(_, info)| info.name().to_string())
        .map(|full| {
            let short = registry
                .get_with_type_path(&full)
                .map(|r| r.type_info().type_path_table().short_path().to_string())
                .unwrap_or_else(|| full.rsplit("::").next().unwrap_or(&full).to_string());
            (short, full)
        })
        .collect();
    out.sort();
    out
}

/// Adds a component with its reflected default, undoable.
fn add_default_component(world: &mut World, entity: Entity, type_path: &str) {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let Some(registration) = registry.get_with_type_path(type_path) else {
        return;
    };
    use regia_ecs::reflect::std_traits::ReflectDefault;
    let Some(default) = registration.data::<ReflectDefault>().map(|d| d.default()) else {
        tracing::warn!(target: "regia_editor", "{type_path} has no reflected default");
        return;
    };
    let short = registration
        .type_info()
        .type_path_table()
        .short_path()
        .to_string();
    drop(registry);
    match AddComponent::capture(world, entity, &short, default) {
        Ok(mut command) => {
            command.apply(world);
            world
                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                .push(Box::new(command));
        }
        Err(err) => tracing::warn!(target: "regia_editor", "add component: {err}"),
    }
}

/// The project's assets offered by inspector asset pickers (`@AssetFilter` fields): the content
/// browser's index ([`crate::content_index::ContentIndex`]), refreshed by the shell.
#[derive(Resource, Debug, Clone, Default)]
pub struct AssetCatalog {
    /// `(project-relative path, kind)`, sorted (shared with the content index; cloning is cheap).
    pub entries: std::sync::Arc<[(String, ContentKind)]>,
    /// When the catalog last took a new list from the index.
    pub scanned: Option<std::time::Instant>,
}

/// Points the [`AssetCatalog`] at the content index's asset list. The index is scanned once and
/// then re-lists only changed folders (its folder-time check runs at most once a second), so this
/// costs no walk of `assets/` per call (ADR-1329; it used to walk the tree every three seconds).
pub fn refresh_asset_catalog(world: &mut World, content: &crate::content::ContentBrowserState) {
    // What the asset fields' **Use selected** takes (ADR-7786).
    let selected = picker_links::ContentSelection(content.selected.clone());
    if world.get_resource::<picker_links::ContentSelection>() != Some(&selected) {
        world.insert_resource(selected);
    }
    let mut index = world.get_resource_or_insert_with(crate::content_index::ContentIndex::default);
    index.maybe_check(std::time::Instant::now());
    index.ensure(content.project_root.as_deref());
    let entries = index.assets();
    let current = world
        .get_resource::<AssetCatalog>()
        .is_some_and(|catalog| std::sync::Arc::ptr_eq(&catalog.entries, &entries));
    if !current {
        world.insert_resource(AssetCatalog {
            entries,
            scanned: Some(std::time::Instant::now()),
        });
    }
}

/// Draws one component of `entity` (type path `component`) and turns every edit into an undoable
/// `SetProperty`. Returns the edits made this frame.
pub fn component_ui(
    ui: &mut egui::Ui,
    world: &mut World,
    entity: Entity,
    component: &str,
    assets: &[(String, ContentKind)],
) -> Vec<Change> {
    component_ui_shared(ui, world, entity, component, assets.into())
}

/// The inspector's copies of the components it shows, kept between repaints (ADR-7326).
///
/// A repaint used to clone every shown component through reflection (a 20 000-element list
/// cloned 20 000 elements per frame). A copy is now reused while the component's change tick is
/// the one it was read at; any write to the component (an agent `editor.set_property`, an undo, a
/// system) moves the tick and the next repaint reads it again. An edit in the drawer drops the
/// copy too, so the value drawn is always the one in the world. Keyed by (entity, component type);
/// emptied when the selection changes.
#[derive(Resource, Default)]
struct InspectorValueCache {
    /// The selection the copies were made for.
    selection: Option<Entity>,
    /// The copies: the component's change tick when read, and the value.
    entries: std::collections::HashMap<(Entity, std::any::TypeId), (Tick, Box<dyn PartialReflect>)>,
    /// The field tools of each copy (its baseline, a clone of the prefab's component for a prefab
    /// member, and the modified paths), and what they were built from.
    tools: std::collections::HashMap<(Entity, std::any::TypeId), (ToolsKey, clipboard::FieldTools)>,
}

/// What a component's [`clipboard::FieldTools`] are built from: the component's change tick, the
/// prefab templates' change tick and the entity's prefab link's change tick (a Break removes it).
type ToolsKey = (Tick, Option<Tick>, Option<Tick>);

/// The [`ToolsKey`] of `entity`'s component read at `changed`.
fn field_tools_key(world: &World, entity: Entity, changed: Tick) -> ToolsKey {
    (
        changed,
        world
            .get_resource_ref::<regia_scene::prefab::PrefabLibrary>()
            .map(|library| library.last_changed()),
        world
            .get_entity(entity)
            .ok()
            .and_then(|entity| entity.get_ref::<regia_scene::prefab::PrefabLink>())
            .map(|link| link.last_changed()),
    )
}

/// Copies kept at most; past it the cache starts over (only the selection's components are
/// normally kept, so this only bounds callers drawing many entities through [`component_ui`]).
const MAX_CACHED_VALUES: usize = 256;

impl InspectorValueCache {
    /// Takes the copy of `key` if it was read at `changed`.
    fn take(
        world: &mut World,
        key: (Entity, std::any::TypeId),
        changed: Tick,
    ) -> Option<Box<dyn PartialReflect>> {
        let mut cache = world.get_resource_mut::<Self>()?;
        let (at, value) = cache.bypass_change_detection().entries.remove(&key)?;
        (at == changed).then_some(value)
    }

    /// Keeps `value` (read at `changed`) for the next repaint.
    fn keep(
        world: &mut World,
        key: (Entity, std::any::TypeId),
        changed: Tick,
        value: Box<dyn PartialReflect>,
    ) {
        let mut cache = world.get_resource_or_insert_with::<Self>(Self::default);
        let cache = cache.bypass_change_detection();
        if cache.entries.len() >= MAX_CACHED_VALUES {
            cache.entries.clear();
        }
        cache.entries.insert(key, (changed, value));
    }

    /// Takes the field tools of `key` if they were built from `tools_key`.
    fn take_tools(
        world: &mut World,
        key: (Entity, std::any::TypeId),
        tools_key: ToolsKey,
    ) -> Option<clipboard::FieldTools> {
        let mut cache = world.get_resource_mut::<Self>()?;
        let (at, tools) = cache.bypass_change_detection().tools.remove(&key)?;
        (at == tools_key).then_some(tools)
    }

    /// Keeps the field tools of `key` (built from `tools_key`) for the next repaint.
    fn keep_tools(
        world: &mut World,
        key: (Entity, std::any::TypeId),
        tools_key: ToolsKey,
        tools: clipboard::FieldTools,
    ) {
        let mut cache = world.get_resource_or_insert_with::<Self>(Self::default);
        let cache = cache.bypass_change_detection();
        if cache.tools.len() >= MAX_CACHED_VALUES {
            cache.tools.clear();
        }
        cache.tools.insert(key, (tools_key, tools));
    }
}

/// Drops the inspector's kept copies when the selection is no longer `primary`.
fn retain_cached_values(world: &mut World, primary: Option<Entity>) {
    let mut cache = world.get_resource_or_insert_with::<InspectorValueCache>(InspectorValueCache::default);
    if cache.selection != primary {
        let cache = cache.bypass_change_detection();
        cache.selection = primary;
        cache.entries.clear();
        cache.tools.clear();
    }
}

/// [`component_ui`] with the asset list shared instead of copied (the inspector panel's path).
#[tracing::instrument(target = "regia_editor", skip_all, name = "inspector_component")]
pub fn component_ui_shared(
    ui: &mut egui::Ui,
    world: &mut World,
    entity: Entity,
    component: &str,
    assets: std::sync::Arc<[(String, ContentKind)]>,
) -> Vec<Change> {
    component_ui_with(ui, world, entity, component, assets, None).0
}

/// [`component_ui_shared`] under the Inspector's row `filter` (search, Modified only), with the
/// component's pinned fields in a **Pinned** section at the top; also answers the overridden field
/// paths whose row drew an override marker with its Revert/Apply menu. An extension-drawn or Lua
/// component, and an override with no field row of its own (a list element, a vector lane, or a row
/// the filter hides), mark nothing, so the caller still shows those overrides as rows (ADR-7744).
fn component_ui_with(
    ui: &mut egui::Ui,
    world: &mut World,
    entity: Entity,
    component: &str,
    assets: std::sync::Arc<[(String, ContentKind)]>,
    filter: Option<search::RowFilter>,
) -> (Vec<Change>, std::collections::BTreeSet<String>) {
    let unmarked = std::collections::BTreeSet::new;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let Some((type_id, full, data)) = registry
        .get_with_type_path(component)
        .or_else(|| registry.get_with_short_type_path(component))
        .or_else(|| regia_core::brand::lookup_type_path(component, |p| registry.get_with_type_path(p)))
        .and_then(|r| {
            Some((
                r.type_id(),
                r.type_info().type_path(),
                r.data::<ReflectComponent>().cloned()?,
            ))
        })
    else {
        return (Vec::new(), unmarked());
    };
    let key = (entity, type_id);
    // The component's change tick: the kept copy is current while it has not moved.
    let changed = world.components().get_id(type_id).and_then(|id| {
        world
            .get_entity(entity)
            .ok()?
            .get_change_ticks_by_id(id)
            .map(|ticks| ticks.changed)
    });
    let kept = changed.and_then(|changed| InspectorValueCache::take(world, key, changed));
    let mut value = match kept {
        Some(value) => value,
        None => {
            let Some(value) = data
                .reflect(match world.get_entity(entity) {
                    Ok(entity) => entity,
                    Err(_) => return (Vec::new(), unmarked()),
                })
                .and_then(|v| reflect_ui::clone_concrete(&registry, v))
            else {
                return (Vec::new(), unmarked());
            };
            // A write later in this tick would leave the component's tick where it was read:
            // move the world's tick on so any later write is seen.
            world.increment_change_tick();
            value
        }
    };
    // An unedited copy is kept for the next repaint; an edited one is dropped (the world's value
    // is read back, which also covers an edit the commands refused).
    let keep = |world: &mut World, value: Box<dyn PartialReflect>, changes: &[Change]| {
        if let Some(changed) = changed
            && changes.is_empty()
        {
            InspectorValueCache::keep(world, key, changed, value);
        }
    };
    // An extension's customization replaces the drawer; a badge names it (specs/57 § 3.3.3, § 6).
    if let Some((owner, draw)) = crate::extension::panel::inspector_for(world, component) {
        drop(registry);
        ui.small(format!("drawn by {owner}"));
        if !draw(ui, value.as_mut()) {
            keep(world, value, &[]);
            return (Vec::new(), unmarked());
        }
        let changes = vec![Change {
            path: String::new(),
            discrete: true,
        }];
        apply_changes(world, entity, component, value.as_ref(), &changes);
        return (changes, unmarked());
    }
    // A Lua script component draws from its declaration (defaults, ranges, options), not raw
    // reflection: typed widgets need the declaring mod's schema (ADR-3356).
    if component == crate::lua_inspector::TYPE_PATH {
        drop(registry);
        let changes = crate::lua_inspector::component_ui(ui, world, value.as_mut(), &assets);
        apply_changes(world, entity, component, value.as_ref(), &changes);
        keep(world, value, &changes);
        return (changes, unmarked());
    }
    let pins = world
        .get_resource::<pinned::InspectorPins>()
        .map(|pins| pins.paths(full).to_vec())
        .unwrap_or_default();
    // Registered component actions without a `@Button` field: a row of buttons (ADR-7785).
    let type_info = registry.get(type_id).map(|r| r.type_info());
    let row_action = actions::action_row(ui, world, component, type_info);
    // The field tools compare the value with its baseline: for a prefab member that is a clone of
    // the prefab's component, so they are kept with the copy and built again only when what they
    // are built from moves (the w11 "per-frame clones in the Inspector").
    let tools_key = changed.map(|changed| field_tools_key(world, entity, changed));
    let field_tools =
        match tools_key.and_then(|tools_key| InspectorValueCache::take_tools(world, key, tools_key)) {
            Some(mut tools) => {
                tools.raw = clipboard::raw_view(world);
                tools
            }
            None => clipboard::FieldTools::for_component(world, entity, full, value.as_ref()),
        };
    let mut context = InspectorContext::new(component)
        .with_assets(assets)
        .with_thumbnails(world.remove_resource::<crate::thumb_cache::ThumbCache>())
        .with_overrides(multi::overridden_field_paths(world, entity, component))
        .with_filter(filter)
        .with_pinned(pins.clone())
        .with_linked(linked_vectors(world))
        .with_pickers(picker_links::links(world, Some(entity), component))
        .with_field_tools(field_tools);
    context.project_root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    if !pins.is_empty() {
        pinned::pinned_section(ui, &registry, &mut context, value.as_mut(), component, &pins);
    }
    InspectorUi::new(&registry, &mut context).ui_for_reflect_with_options(
        value.as_mut(),
        ui,
        egui::Id::new(("inspector", component)),
        &(),
    );
    drop(registry);
    // A pin pick is an input event (one pass only) and setting a pin is idempotent.
    for (path, on) in context.take_pin_requests() {
        if let Err(err) = pinned::set_pin(world, full, &path, on) {
            tracing::warn!(target: "regia_editor", "pin field: {err}");
        }
    }
    if let Some(thumbs) = context.take_thumbnails() {
        world.insert_resource(thumbs);
    }
    // Picker requests (Browse to, Make unique, swatches, eyedropper; ADR-7786) after the draw.
    let requests = context.pickers.take_requests();
    if !ui.ctx().will_discard() {
        picker_links::run_requests(world, entity, component, requests);
    }
    let changes = context.take_changes();
    apply_changes(world, entity, component, value.as_ref(), &changes);
    // A marker's menu picked Revert or Apply (ADR-7744); egui's discarded first pass must not act.
    let requests = context.take_override_requests();
    if !ui.ctx().will_discard() {
        for (path, request) in requests {
            let done = match request {
                reflect_ui::OverrideRequest::Revert => {
                    revert_prefab_override(world, entity, component, &path).map(|_| ())
                }
                reflect_ui::OverrideRequest::Apply => {
                    apply_prefab_override(world, entity, component, &path).map(|_| ())
                }
            };
            if let Err(err) = done {
                tracing::warn!(target: "regia_editor", "prefab override {request:?}: {err}");
            }
        }
    }
    // Field menu picks and shortcuts (copy, paste, reset; inspector::clipboard), one undo step each.
    clipboard::run_field_requests(ui.ctx(), world, entity, full, context.take_field_requests());
    // Inspector buttons run after the edits are applied, each as its own undo step (ADR-7785).
    // egui may run a panel twice in one frame: a discarded pass changes nothing, and a click is
    // taken once per frame.
    let discard = ui.ctx().will_discard();
    if !discard {
        store_link_toggles(world, context.take_link_toggles());
    }
    let frame = ui.ctx().cumulative_frame_nr();
    let mut requests = context.take_action_requests();
    requests.extend(row_action);
    if !discard && !requests.is_empty() && ran_actions_frame(world) != Some(frame) {
        world.insert_resource(ActionsRanFrame(frame));
        for action in requests {
            if let Err(e) = actions::run_component_action(world, entity, component, &action) {
                tracing::warn!(target: "regia_editor", "component action {component}.{action}: {}", e.message);
            }
        }
    }
    for path in context.take_sequence_requests() {
        // A refusal is shown in the Sequence Curves panel's authoring rows.
        if let Err(e) = crate::seq_tracks::add_to_sequence(world, entity, component, &path) {
            tracing::info!(target: "regia_editor", "add to sequence: {}", e.message);
        }
    }
    if let (Some(tools_key), Some(tools)) = (tools_key, context.take_field_tools())
        && changes.is_empty()
    {
        InspectorValueCache::keep_tools(world, key, tools_key, tools);
    }
    keep(world, value, &changes);
    (changes, context.take_marked_overrides())
}

/// The frame the inspector last ran component actions in (a click runs once per frame even when
/// egui draws the panel twice).
#[derive(Resource)]
struct ActionsRanFrame(u64);

fn ran_actions_frame(world: &World) -> Option<u64> {
    world.get_resource::<ActionsRanFrame>().map(|f| f.0)
}

/// The preference key of the linked vector fields (ADR-7785).
pub const LINKED_VECTORS_PREF: &str = "inspector.linked_vectors";

/// The link keys (`Transform.scale`) of the vector fields the user linked.
#[must_use]
pub fn linked_vectors(world: &World) -> Vec<String> {
    world
        .get_resource::<crate::editor_prefs::EditorPreferences>()
        .map(|prefs| parse_linked(&prefs.text(LINKED_VECTORS_PREF)))
        .unwrap_or_default()
}

/// The comma-separated link keys of the preference.
#[must_use]
pub fn parse_linked(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Writes link toggles into the preference (sorted, without repeats); a key that would push the
/// preference past its length limit is not stored (the toggle still holds for the session's
/// frame).
pub(crate) fn store_link_toggles(world: &mut World, toggles: Vec<(String, bool)>) {
    if toggles.is_empty() {
        return;
    }
    let mut keys: std::collections::BTreeSet<String> = linked_vectors(world).into_iter().collect();
    for (key, linked) in toggles {
        if linked {
            keys.insert(key);
        } else {
            keys.remove(&key);
        }
    }
    let text = keys.into_iter().collect::<Vec<_>>().join(",");
    let mut prefs = world.get_resource_or_insert_with(crate::editor_prefs::EditorPreferences::default);
    if let Err(e) = prefs.set(LINKED_VECTORS_PREF, &crate::editor_prefs::PrefValue::Text(text)) {
        tracing::info!(target: "regia_editor", "linked vectors not stored: {e}");
    }
}

/// True when every Euler lane's quaternion on `edited` is finite.
fn euler_lanes_stay_finite(edited: &dyn PartialReflect, changes: &[Change]) -> bool {
    for change in changes {
        let Some((parent, lane)) = change.path.rsplit_once('.') else {
            continue;
        };
        if !matches!(lane, "rx" | "ry" | "rz") {
            continue;
        }
        let Ok(field) = parent.reflect_element(edited) else {
            continue;
        };
        if field
            .try_downcast_ref::<Quat>()
            .is_some_and(|quat| !quat.is_finite())
        {
            return false;
        }
    }
    true
}

/// Turns the drawer's edits of `edited` (a clone of the component) into `SetProperty` commands:
/// captured, applied and pushed on the [`UndoStack`]. Structural edits never coalesce.
pub fn apply_changes(
    world: &mut World,
    entity: Entity,
    component: &str,
    edited: &dyn PartialReflect,
    changes: &[Change],
) {
    // Euler lanes are drawer paths rather than quaternion reflection fields. Use the same
    // axis patch as multi-selection, while keeping legacy commits for ordinary single fields.
    if multi::has_euler_lane_changes(edited, changes) {
        // An infinite or NaN Euler drag builds a non-finite quaternion (the axis-angle and raw
        // widgets already refuse that). Do not write it into the component.
        if euler_lanes_stay_finite(edited, changes)
            && let Err(err) = commit_widget_changes(world, &[entity], component, edited, changes)
        {
            tracing::warn!(target: "regia_editor", "inspector edit: {err}");
        }
        return;
    }
    for change in changes {
        let field = if change.path.is_empty() {
            Some(edited)
        } else {
            change.path.as_str().reflect_element(edited).ok()
        };
        let Some(new_value) = field.and_then(reflect_ui::clone_value) else {
            tracing::warn!(target: "regia_editor", "inspector edit: no value at {component}.{}", change.path);
            continue;
        };
        match SetProperty::capture(world, entity, component, &change.path, new_value) {
            Ok(mut command) => {
                command.coalesce = !change.discrete;
                command.apply(world);
                world
                    .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                    .push(Box::new(command));
            }
            Err(err) => tracing::warn!(target: "regia_editor", "inspector edit: {err}"),
        }
    }
}

/// Euler degrees → quaternion, XYZ order.
pub(crate) fn euler_deg_to_quat(euler: [f32; 3]) -> Quat {
    Quat::from_rotation_z(euler[2].to_radians())
        * Quat::from_rotation_y(euler[1].to_radians())
        * Quat::from_rotation_x(euler[0].to_radians())
}

/// Linear → sRGB byte (the engine's simplified EOTF inverse, `pow(x, 1/2.2)`, matching
/// `regia_render`'s encode; IEC 61966-2-1 simplified — see `docs/provenance.md`).
#[must_use]
pub fn linear_to_srgb_u8(linear: f32) -> u8 {
    let clamped = linear.clamp(0.0, 1.0);
    (clamped.powf(1.0 / 2.2) * 255.0).round() as u8
}

/// sRGB byte → linear (the matching `pow(x, 2.2)` decode).
#[must_use]
pub fn srgb8_to_linear(srgb: u8) -> f32 {
    (f32::from(srgb) / 255.0).powf(2.2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::reflect::GetPath;
    use regia_scene::prelude::{Name, Transform};

    /// A component with the field shapes the inspector widgets cover.
    #[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Material {
        roughness: f32,
        metal_mask: bool,
        label: String,
    }

    /// The inspector keeps one copy per shown component of the selected entity, and a new
    /// selection drops them (ADR-7326).
    #[test]
    fn kept_copies_follow_the_selection() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<Material>();
        app.init_resource::<UndoStack>();
        let spawn = |app: &mut regia_app::App, name: &str| {
            app.world_mut()
                .spawn((Name::new(name), Transform::default(), Material::default()))
                .id()
        };
        let rock = spawn(&mut app, "Rock");
        let tree = spawn(&mut app, "Tree");
        let ctx = egui::Context::default();
        let repaint = |app: &mut regia_app::App| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                inspector_panel(ui, app.world_mut());
            });
            output.textures_delta.clear();
        };
        let kept = |app: &regia_app::App| -> Vec<Entity> {
            let mut entities: Vec<Entity> = app
                .world()
                .get_resource::<InspectorValueCache>()
                .map(|cache| cache.entries.keys().map(|(entity, _)| *entity).collect())
                .unwrap_or_default();
            entities.sort();
            entities.dedup();
            entities
        };
        let select = |app: &mut regia_app::App, entity: Entity| {
            let mut selection = Selection::default();
            selection.select(entity, crate::selection::SelectMode::Replace);
            app.world_mut().insert_resource(selection);
        };
        select(&mut app, rock);
        repaint(&mut app);
        assert_eq!(kept(&app), [rock]);
        let calls = reflect_ui::clone_concrete_calls();
        repaint(&mut app);
        assert_eq!(
            reflect_ui::clone_concrete_calls(),
            calls,
            "unchanged: nothing cloned"
        );
        select(&mut app, tree);
        repaint(&mut app);
        assert_eq!(kept(&app), [tree], "the old selection's copies are dropped");
        app.world_mut().insert_resource(Selection::default());
        repaint(&mut app);
        assert_eq!(kept(&app), Vec::<Entity>::new());
    }

    #[test]
    fn inspector_edits_round_trip_through_reflection() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<Material>();
        let entity = app
            .world_mut()
            .spawn((
                Name::new("Rock"),
                Transform::default(),
                Material {
                    roughness: 0.5,
                    metal_mask: false,
                    label: "granite".into(),
                },
            ))
            .id();

        // Widget views of the fields.
        let registry = app.world().resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let material = app.world().get::<Material>(entity).expect("material");
        let value: &dyn Reflect = material.as_reflect();
        let roughness = value.reflect_path("roughness").expect("field");
        assert!(matches!(view_field(roughness), FieldView::Number(0.5)));
        let mask = value.reflect_path("metal_mask").expect("field");
        assert!(matches!(view_field(mask), FieldView::Bool(false)));
        let label = value.reflect_path("label").expect("field");
        assert!(matches!(view_field(label), FieldView::Str(ref s) if s == "granite"));
        let translation = app
            .world()
            .get::<Transform>(entity)
            .expect("transform")
            .as_reflect()
            .reflect_path("translation")
            .expect("vec3");
        assert!(matches!(view_field(translation), FieldView::Vec3(_)));
        drop(registry);

        // An edit pushed like the widget does, then undone, restores the value.
        let mut command =
            SetProperty::capture(app.world(), entity, "Material", "roughness", Box::new(0.9f32))
                .expect("capture");
        command.apply(app.world_mut());
        app.world_mut()
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(command));
        assert_eq!(app.world().get::<Material>(entity).expect("m").roughness, 0.9);
        crate::command::undo_world(app.world_mut());
        assert_eq!(
            app.world().get::<Material>(entity).expect("m").roughness,
            0.5,
            "undo restored the field"
        );
    }

    #[test]
    fn euler_conversions_are_inverse() {
        for euler in [
            [30.0f32, 45.0, -10.0],
            [0.0, 0.0, 0.0],
            [90.0, 0.0, 0.0],
            [0.0, -60.0, 120.0],
            [-179.0, 5.0, 179.0],
        ] {
            let quat = euler_deg_to_quat(euler);
            let back = quat_to_euler_deg(&quat);
            for (want, got) in euler.iter().zip(back.iter()) {
                assert!((want - got).abs() < 0.01, "euler {euler:?} came back as {back:?}");
            }
        }
    }

    /// Whether two rotations are the same (a quaternion and its negation are one rotation).
    fn same_turn(a: Quat, b: Quat) -> bool {
        a.abs_diff_eq(b, 1e-4) || a.abs_diff_eq(-b, 1e-4)
    }

    /// Corr editor-a finding 2: an Euler `ry` past 90 degrees reads back from a quaternion as
    /// `rx = rz = 180, ry = 180 - ry`. The commit used that reading for the one axis it patches,
    /// so typing `ry = 120` on one entity stored a 60-degree turn.
    #[test]
    fn a_single_entity_euler_lane_past_ninety_keeps_the_typed_angle() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let entity = app.world_mut().spawn(Transform::default()).id();
        let edited = Transform {
            rotation: euler_deg_to_quat([0.0, 120.0, 0.0]),
            ..Default::default()
        };
        apply_changes(
            app.world_mut(),
            entity,
            "Transform",
            &edited,
            &[Change {
                path: "rotation.ry".into(),
                discrete: false,
            }],
        );
        let stored = app.world().get::<Transform>(entity).expect("transform").rotation;
        assert!(
            same_turn(stored, euler_deg_to_quat([0.0, 120.0, 0.0])),
            "typed ry 120 must store a 120-degree turn, got {:?}",
            quat_to_euler_deg(&stored)
        );
    }

    /// The multi-selection half of the same finding: the drawn entity gets the edited quaternion
    /// and every other holder gets the typed angle on its own X and Z.
    #[test]
    fn a_multi_entity_euler_lane_past_ninety_patches_the_typed_angle() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let drawn = app.world_mut().spawn(Transform::default()).id();
        let other = app
            .world_mut()
            .spawn(Transform::from_rotation(euler_deg_to_quat([30.0, 0.0, 0.0])))
            .id();
        let edited = Transform {
            rotation: euler_deg_to_quat([0.0, 120.0, 0.0]),
            ..Default::default()
        };
        let mut lanes = std::collections::BTreeMap::new();
        lanes.insert("rotation.ry".to_string(), 120.0);
        multi::commit_drawer_changes(
            app.world_mut(),
            &[drawn, other],
            "Transform",
            &edited,
            &[Change {
                path: "rotation.ry".into(),
                discrete: false,
            }],
            Some(drawn),
            &lanes,
        )
        .expect("commit");
        let rotation = |entity| app.world().get::<Transform>(entity).expect("transform").rotation;
        assert!(
            same_turn(rotation(drawn), euler_deg_to_quat([0.0, 120.0, 0.0])),
            "drawn: {:?}",
            quat_to_euler_deg(&rotation(drawn))
        );
        assert!(
            same_turn(rotation(other), euler_deg_to_quat([30.0, 120.0, 0.0])),
            "other keeps its rx and takes ry 120: {:?}",
            quat_to_euler_deg(&rotation(other))
        );
    }

    /// Fixrev2 editor euler-k finding 1: with the drawer showing the flipped reading (an
    /// in-session `ry` drag past 90), an `rz` edit was patched onto every other holder's
    /// canonical reading and turned it 180 degrees on that axis. A at (0, 120, 0) and B at
    /// (30, 120, 0), the drawer showing (0, 120, 0); type rz = 10: B must become
    /// (30, 120, 10), not (-150, 60, 10).
    #[test]
    fn a_multi_entity_euler_patch_keeps_the_drawers_flipped_reading() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let drawn = app
            .world_mut()
            .spawn(Transform::from_rotation(euler_deg_to_quat([0.0, 120.0, 0.0])))
            .id();
        let other = app
            .world_mut()
            .spawn(Transform::from_rotation(euler_deg_to_quat([30.0, 120.0, 0.0])))
            .id();
        let edited = Transform {
            rotation: euler_deg_to_quat([0.0, 120.0, 10.0]),
            ..Default::default()
        };
        let mut lanes = std::collections::BTreeMap::new();
        lanes.insert("rotation.rz".to_string(), 10.0);
        multi::commit_drawer_changes(
            app.world_mut(),
            &[drawn, other],
            "Transform",
            &edited,
            &[Change {
                path: "rotation.rz".into(),
                discrete: false,
            }],
            Some(drawn),
            &lanes,
        )
        .expect("commit");
        let rotation = |entity| app.world().get::<Transform>(entity).expect("transform").rotation;
        assert!(
            same_turn(rotation(drawn), euler_deg_to_quat([0.0, 120.0, 10.0])),
            "drawn: {:?}",
            quat_to_euler_deg(&rotation(drawn))
        );
        assert!(
            same_turn(rotation(other), euler_deg_to_quat([30.0, 120.0, 10.0])),
            "other keeps the reading the drawer showed: {:?}",
            quat_to_euler_deg(&rotation(other))
        );
    }

    /// Corr editor-a finding 3: a drag over two entities is one undo step. Each frame used to be
    /// a `CommandGroup`, which never merges, so one undo only stepped back the last frame.
    #[test]
    fn a_multi_entity_drag_is_one_undo_step() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        let a = world.spawn(Transform::default()).id();
        let b = world
            .spawn(Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)))
            .id();
        world.insert_resource(crate::command::UndoStack::new());
        let drag = [Change {
            path: "translation.x".into(),
            discrete: false,
        }];
        for x in [1.0, 2.0, 3.0] {
            let edited = Transform::from_translation(Vec3::new(x, 0.0, 0.0));
            commit_widget_changes(world, &[a, b], "Transform", &edited, &drag).expect("frame");
        }
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 1);
        assert_eq!(world.get::<Transform>(b).expect("b").translation.x, 3.0);
        world.resource_scope(|world, mut stack: Mut<crate::command::UndoStack>| {
            assert!(stack.undo(world));
        });
        assert_eq!(world.get::<Transform>(a).expect("a").translation, Vec3::ZERO);
        assert_eq!(
            world.get::<Transform>(b).expect("b").translation,
            Vec3::new(0.0, 5.0, 0.0),
            "one undo restores the value from before the drag"
        );
        // A sealed stack (pointer released) starts a new entry.
        world.resource_mut::<crate::command::UndoStack>().seal();
        let edited = Transform::from_translation(Vec3::new(4.0, 0.0, 0.0));
        commit_widget_changes(world, &[a, b], "Transform", &edited, &drag).expect("next drag");
        world.resource_mut::<crate::command::UndoStack>().seal();
        let edited = Transform::from_translation(Vec3::new(5.0, 0.0, 0.0));
        commit_widget_changes(world, &[a, b], "Transform", &edited, &drag).expect("third drag");
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 2);
    }

    /// The Euler drawer hands the typed number to the commit with the lane change.
    #[test]
    fn record_lane_value_keeps_the_typed_number_by_full_path() {
        let mut context = InspectorContext::new("Transform");
        context.record_lane_value("ry", 120.0);
        assert_eq!(context.lane_values().get("ry"), Some(&120.0));
        assert_eq!(
            context
                .changes()
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            ["ry"]
        );
    }

    /// An infinite Euler lane must not store a non-finite quaternion on the entity.
    #[test]
    fn an_infinite_euler_lane_does_not_store_a_non_finite_rotation() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let entity = app.world_mut().spawn(Transform::default()).id();
        let finite = Transform {
            rotation: euler_deg_to_quat([90.0, 0.0, 0.0]),
            ..Default::default()
        };
        apply_changes(
            app.world_mut(),
            entity,
            "Transform",
            &finite,
            &[Change {
                path: "rotation.rx".into(),
                discrete: false,
            }],
        );
        let stored = app.world().get::<Transform>(entity).expect("transform").rotation;
        let back = quat_to_euler_deg(&stored);
        assert!(
            (back[0] - 90.0).abs() < 0.05,
            "a finite Euler lane must commit, got {back:?}"
        );
        let edited = Transform {
            rotation: euler_deg_to_quat([f32::INFINITY, 0.0, 0.0]),
            ..Default::default()
        };
        assert!(
            !edited.rotation.is_finite(),
            "the fixture quaternion is the non-finite value the drag would produce"
        );
        apply_changes(
            app.world_mut(),
            entity,
            "Transform",
            &edited,
            &[Change {
                path: "rotation.rx".into(),
                discrete: false,
            }],
        );
        let stored = app.world().get::<Transform>(entity).expect("transform").rotation;
        assert!(
            stored.is_finite(),
            "the stored rotation is non-finite: {stored:?}"
        );
        let back = quat_to_euler_deg(&stored);
        assert!(
            (back[0] - 90.0).abs() < 0.05,
            "the infinite drag replaced the committed rotation: {back:?}"
        );
    }

    #[test]
    fn colour_names_are_recognised() {
        for name in [
            "color",
            "base_color",
            "Color",
            "tint",
            "emissive",
            "emissive_factor",
        ] {
            assert!(is_color_name(name), "{name}");
        }
        for name in ["position", "speed_x", "scale", "colour_temperature_k"] {
            assert!(!is_color_name(name), "{name}");
        }
    }

    #[test]
    fn field_units_follow_engine_conventions() {
        // Radians-stored angles edit as degrees.
        for (component, field) in [
            ("regia_render::camera::Camera", "fov_y"),
            ("regia_framework::camera::CameraRig", "pitch"),
            ("regia_framework::camera::CameraRig", "yaw"),
            ("regia_render::lights::SpotLight", "outer_angle"),
        ] {
            assert_eq!(field_unit(component, field), FieldUnit::Degrees, "{field}");
        }
        // Distances are metres.
        for field in ["translation", "radius", "range", "near", "far", "ortho_size"] {
            assert_eq!(field_unit("Any", field), FieldUnit::Metres, "{field}");
        }
        // Light intensities carry their physical units (specs/13 § 6).
        assert_eq!(
            field_unit("regia_render::lights::PointLight", "intensity"),
            FieldUnit::Lumens
        );
        assert_eq!(
            field_unit("regia_render::lights::SpotLight", "intensity"),
            FieldUnit::Candela
        );
        assert_eq!(
            field_unit("regia_render::lights::DirectionalLight", "illuminance"),
            FieldUnit::Lux
        );
        assert_eq!(
            field_unit("Whatever", "intensity"),
            FieldUnit::None,
            "ambiguous stays unitless"
        );
        assert_eq!(field_unit("Whatever", "speed"), FieldUnit::MetresPerSecond);
        // Suffixes are short.
        assert_eq!(FieldUnit::Degrees.suffix(), "°");
        assert_eq!(FieldUnit::Metres.suffix(), " m");
    }

    #[test]
    fn srgb_conversion_matches_the_engine_encode() {
        // 50% linear ≈ 73% sRGB under the engine's simplified EOTF (pow 1/2.2).
        assert_eq!(linear_to_srgb_u8(0.5), 186, "0.5 linear → 0.5^(1/2.2)");
        assert_eq!(linear_to_srgb_u8(0.0), 0);
        assert_eq!(linear_to_srgb_u8(1.0), 255);
        assert_eq!(linear_to_srgb_u8(4.0), 255, "HDR clamps for the picker");
        // The pair round-trips at 8-bit precision.
        for byte in [0u8, 1, 64, 128, 189, 200, 255] {
            let back = linear_to_srgb_u8(srgb8_to_linear(byte));
            assert!((i16::from(back) - i16::from(byte)).abs() <= 1, "{byte} → {back}");
        }
    }

    /// A reflected light-like component for the widget-view tests.
    #[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
    #[reflect(Component, Default)]
    struct TestLight {
        #[allow(dead_code)]
        color: [f32; 3],
        #[allow(dead_code)]
        intensity: f32,
        #[allow(dead_code)]
        radius: f32,
    }

    #[test]
    fn reflected_colour_fields_get_the_colour_view() {
        let light = TestLight {
            color: [0.25, 0.5, 1.5],
            intensity: 800.0,
            radius: 4.0,
        };
        let boxed: Box<dyn Reflect> = Box::new(light);
        let color = boxed.reflect_path("color").expect("field");
        match view_field(color) {
            FieldView::Color { rgba, alpha } => {
                assert_eq!(rgba, [0.25, 0.5, 1.5, 1.0]);
                assert!(!alpha, "[f32; 3] has no alpha");
            }
            other => panic!("expected the color view, got {other:?}"),
        }
        let intensity = boxed.reflect_path("intensity").expect("field");
        assert!(matches!(view_field(intensity), FieldView::Number(800.0)));
        // The unit inference reads the component name from the inspector's loop.
        assert_eq!(field_unit("TestLight", "intensity"), FieldUnit::None);
        assert_eq!(field_unit("TestLight", "radius"), FieldUnit::Metres);
    }
}

#[cfg(test)]
mod picker_tests;
#[cfg(test)]
mod ui_tests;

#[cfg(test)]
mod panel_tests;

#[cfg(test)]
mod field_tools_tests;

#[cfg(test)]
mod finish_tests;
#[cfg(test)]
mod values_tests;
