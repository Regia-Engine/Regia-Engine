// SPDX-License-Identifier: MPL-2.0
//! Picture-in-picture preview of the selected scene camera (spec addendum § 3.7, ADR-7768,
//! `book/agents/viewport-tools.md`).
//!
//! While the primary selection is a scene camera (`regia_render::Camera`) that the main view is
//! not already looking through or piloting, the main viewport's perspective pane shows an inset of
//! that camera's view in its bottom-right corner: the game's look (game view, no editor helpers),
//! the camera's own pose and vertical field of view, at a small size (the **Camera ▸ Preview** size:
//! off, small, medium or large; medium by default). It is one more pane render, through its own
//! renderer slot, at most once per egui frame, and only while such a camera is selected: with
//! nothing (or no camera) selected no request is made ([`CameraPreview::requests`] counts them).

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport::EditorCamera;
use crate::viewport_agent::Handler;

/// The renderer slot (a viewport instance id no panel uses) the preview draws through, so it never
/// shares temporal history with a pane.
pub const PREVIEW_INSTANCE: u64 = u64::MAX - 0x0050_4950;

/// The largest side of the preview picture, pixels.
pub const MAX_PREVIEW_SIDE: u32 = 640;

/// The inset's aspect (width / height) when the pane gives none.
const PREVIEW_ASPECT: f32 = 16.0 / 9.0;

/// How big the inset is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewSize {
    /// No preview.
    Off,
    /// A fifth of the pane's width.
    Small,
    /// Close to three tenths of the pane's width (the default).
    #[default]
    Medium,
    /// Close to four tenths of the pane's width.
    Large,
}

impl PreviewSize {
    /// Every size, smallest first.
    pub const ALL: [Self; 4] = [Self::Off, Self::Small, Self::Medium, Self::Large];

    /// The protocol name (`off`, `small`, `medium`, `large`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    /// The size a protocol name means (also `s`, `m`, `l`).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "small" | "s" => Some(Self::Small),
            "medium" | "m" => Some(Self::Medium),
            "large" | "l" => Some(Self::Large),
            _ => None,
        }
    }

    /// The menu label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Small => "Small",
            Self::Medium => "Medium",
            Self::Large => "Large",
        }
    }

    /// The inset's width as a fraction of the pane's (0 for off).
    #[must_use]
    pub fn fraction(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::Small => 0.2,
            Self::Medium => 0.28,
            Self::Large => 0.36,
        }
    }
}

/// The `viewport.camera_preview` preference's choices ([`PreviewSize::name`]).
pub const SIZE_CHOICES: &[&str] = &["off", "small", "medium", "large"];

/// The preference key of the preview size.
pub const SIZE_PREF: &str = "viewport.camera_preview";

/// Sets the preview size, and the `viewport.camera_preview` preference when the editor has
/// preferences (the shell writes them to the user's config).
pub fn set_size(world: &mut World, size: PreviewSize) {
    world.get_resource_or_insert_with(CameraPreview::default).size = size;
    if let Some(mut prefs) = world.get_resource_mut::<crate::editor_prefs::EditorPreferences>()
        && let Err(err) = prefs.set(
            SIZE_PREF,
            &crate::editor_prefs::PrefValue::Text(size.name().to_owned()),
        )
    {
        tracing::warn!(target: "regia_editor", "camera preview pref: {err:?}");
    }
}

/// The preview's settings and counters (view state; never saved with the level).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct CameraPreview {
    /// The inset size.
    pub size: PreviewSize,
    /// Preview render requests made so far (one per egui frame at most, only while a scene
    /// camera is selected and the size is not off).
    pub requests: u64,
    /// The egui frame of the last request (the panel may run twice in one frame).
    last_frame: Option<u64>,
}

/// The preview slot's renderer warm on the preview thread, keyed by device id (ADR-8584).
fn warm_job() -> &'static crate::preview_worker::AsyncPreview<()> {
    static JOB: std::sync::OnceLock<crate::preview_worker::AsyncPreview<()>> = std::sync::OnceLock::new();
    JOB.get_or_init(Default::default)
}

/// The scene camera the preview shows: the primary selection when it has a `regia_render::Camera`
/// and a transform and the main view is not already looking through or piloting it.
#[must_use]
pub fn previewed_camera(world: &World) -> Option<Entity> {
    let entity = world.get_resource::<crate::selection::Selection>()?.primary()?;
    world.get::<regia_render::Camera>(entity)?;
    world.get::<regia_scene::prelude::Transform>(entity)?;
    if crate::viewport_pilot::current(world).is_some_and(|session| session.entity == entity) {
        return None;
    }
    Some(entity)
}

/// The inset rectangle in a pane: the size's fraction of the pane width at `aspect`, no taller
/// than half the pane, `margin` points from the bottom-right corner. `None` when off or when the
/// pane is too small to hold it.
#[must_use]
pub fn inset_rect(pane: egui::Rect, size: PreviewSize, aspect: f32, margin: f32) -> Option<egui::Rect> {
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        PREVIEW_ASPECT
    };
    let mut width = pane.width() * size.fraction();
    let mut height = width / aspect;
    let max_height = pane.height() * 0.5;
    if height > max_height {
        height = max_height;
        width = height * aspect;
    }
    if width < 16.0 || height < 16.0 {
        return None;
    }
    let max = pane.right_bottom() - egui::vec2(margin, margin);
    Some(egui::Rect::from_min_max(max - egui::vec2(width, height), max))
}

/// The editor camera that sees what scene camera `entity` sees (its pose and field of view).
#[must_use]
pub fn camera_of(world: &World, entity: Entity) -> Option<EditorCamera> {
    let (eye, rotation) = crate::viewport_pilot::entity_pose(world, entity)?;
    let mut camera = crate::viewport_pilot::camera_at(&EditorCamera::default(), eye, rotation);
    if let Some(scene) = world.get::<regia_render::Camera>(entity)
        && scene.fov_y.is_finite()
    {
        camera.fov_deg = scene.fov_y.to_degrees().clamp(1.0, 179.0);
    }
    Some(camera)
}

/// Draws the inset over the main viewport's perspective pane `pane` when a scene camera is
/// selected (see the module docs). Counts a request per egui frame; renders only with a device.
pub fn draw(
    ui: &mut egui::Ui,
    world: &mut World,
    device: Option<&regia_render::RenderDevice>,
    pane: egui::Rect,
) {
    let size = world
        .get_resource::<CameraPreview>()
        .map_or_else(PreviewSize::default, |p| p.size);
    if size == PreviewSize::Off {
        return;
    }
    let Some(entity) = previewed_camera(world) else {
        return;
    };
    let margin = ui.spacing().item_spacing.x * 2.0;
    let Some(rect) = inset_rect(pane, size, PREVIEW_ASPECT, margin) else {
        return;
    };
    let _span = tracing::info_span!(target: "regia_editor", "camera_preview").entered();
    let frame_nr = ui.ctx().cumulative_frame_nr();
    let texture_id = egui::Id::new("regia_editor::camera-preview-texture");
    let mut preview = world.remove_resource::<CameraPreview>().unwrap_or_default();
    let fresh = preview.last_frame != Some(frame_nr);
    if fresh {
        preview.last_frame = Some(frame_nr);
        preview.requests += 1;
    }
    world.insert_resource(preview);
    // The preview's renderer slot is new on first use: its pipelines build on the preview thread
    // (ADR-8584) while the inset shows its placeholder, so selecting or creating a camera never
    // waits on a compile in the UI frame.
    let warm = fresh
        && device.is_some_and(|device| {
            if crate::render::instance_renderer_warm(PREVIEW_INSTANCE, 0, device) {
                return true;
            }
            let job = warm_job();
            if job.requested() != Some(device.device_id()) {
                let warm = crate::render::warm_instance_renderer_job(device, world, PREVIEW_INSTANCE, 0);
                job.request(device.device_id(), Some(ui.ctx()), warm);
            }
            false
        });
    if warm
        && let Some(device) = device
        && let Some(camera) = camera_of(world, entity)
    {
        let pixels = rect.size() * ui.ctx().pixels_per_point();
        let scale = (pixels.x.max(pixels.y) / MAX_PREVIEW_SIDE as f32).max(1.0);
        let width = ((pixels.x / scale).round() as u32).max(16);
        let height = ((pixels.y / scale).round() as u32).max(16);
        let mut layout = world
            .get_resource::<crate::views::ViewportLayout>()
            .cloned()
            .unwrap_or_default();
        layout.game_view = true;
        layout.quad = false;
        layout.possessed = false;
        let instance = crate::viewport_instance::ViewportInstance {
            id: PREVIEW_INSTANCE,
            camera: camera.clone(),
            layout,
            settings: world
                .get_resource::<crate::viewport::ViewportSettings>()
                .cloned()
                .unwrap_or_default(),
            link: None,
        };
        let request = crate::render::PaneRequest {
            pane: 0,
            kind: crate::views::ViewKind::Perspective,
            camera,
            width,
            height,
            ids: false,
            warm_frames: 0,
            debug_view: regia_render::DebugViewMode::Lit,
            reference: None,
        };
        if let Some(frame) = crate::render::render_instance_pane(device, world, &instance, &request) {
            let texture = ui.ctx().load_texture(
                "viewport-camera-preview",
                frame.image,
                egui::TextureOptions::LINEAR,
            );
            ui.ctx().data_mut(|data| data.insert_temp(texture_id, texture));
        }
    }
    let tokens = crate::theme::tokens(world).overlay;
    let texture = ui
        .ctx()
        .data(|data| data.get_temp::<egui::TextureHandle>(texture_id));
    match texture {
        Some(texture) => {
            let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0));
            ui.painter().image(texture.id(), rect, uv, egui::Color32::WHITE);
        }
        None => {
            ui.painter().rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
        }
    }
    ui.painter().rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(1.5, tokens.icon_camera),
        egui::StrokeKind::Outside,
    );
    ui.painter().text(
        rect.left_top() + egui::vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        crate::outliner::display_name(world, entity),
        egui::TextStyle::Small.resolve(ui.style()),
        tokens.icon_camera,
    );
}

/// The **Camera ▸ Preview** size buttons (keyboard-focusable; the current size is marked).
pub fn size_menu(ui: &mut egui::Ui, world: &mut World) {
    let current = world
        .get_resource::<CameraPreview>()
        .map_or_else(PreviewSize::default, |p| p.size);
    ui.label("Selected camera preview");
    ui.horizontal(|ui| {
        for size in PreviewSize::ALL {
            let text = if size == current {
                format!("{} (current)", size.label())
            } else {
                size.label().to_owned()
            };
            if crate::agent_parity::action_button_text(ui, &crate::agent_parity::VIEW_CAMERA_PREVIEW, text)
                .clicked()
            {
                set_size(world, size);
            }
        }
    });
}

/// `editor.camera_preview`.
fn camera_preview_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(name) = params.get("size").and_then(Value::as_str) {
        let size = PreviewSize::from_name(name).ok_or_else(|| {
            RpcError::new(
                regia_agent::protocol::INVALID_PARAMS,
                "camera_preview_bad_size",
                format!("unknown preview size `{name}`"),
                "off, small, medium or large",
            )
        })?;
        set_size(world, size);
    }
    let preview = world.get_resource_or_insert_with(CameraPreview::default).clone();
    let showing = previewed_camera(world).filter(|_| preview.size != PreviewSize::Off);
    Ok(json!({
        "size": preview.size.name(),
        "camera": showing.map(Entity::to_bits),
        "requests": preview.requests,
    }))
}

/// The agent method of the camera preview (`book/agents/viewport-tools.md`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.camera_preview",
            tool: "camera_preview",
            description: "The picture-in-picture preview of the selected scene camera in the main viewport (ADR-7768): read or set its size. It renders only while a scene camera is selected.",
            params: &[ParamSchema {
                name: "size",
                kind: "string",
                required: false,
                description: "off, small, medium (default) or large.",
            }],
            result: "{ size, camera (the previewed camera id or null), requests (preview renders requested so far) }",
            mutating: false,
            permission: Permission::Read,
        },
        camera_preview_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inset_sits_in_the_bottom_right_corner_at_the_size_fraction() {
        let pane = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(1000.0, 600.0));
        let small = inset_rect(pane, PreviewSize::Small, 16.0 / 9.0, 8.0).expect("small");
        assert!((small.width() - 200.0).abs() < 1e-3);
        assert!((small.height() - 112.5).abs() < 1e-3);
        assert_eq!(small.right_bottom(), egui::pos2(1002.0, 612.0));
        let large = inset_rect(pane, PreviewSize::Large, 16.0 / 9.0, 8.0).expect("large");
        assert!(large.width() > small.width());
        assert_eq!(inset_rect(pane, PreviewSize::Off, 16.0 / 9.0, 8.0), None);
        // A wide, short pane caps the inset at half its height.
        let short = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(2000.0, 200.0));
        let capped = inset_rect(short, PreviewSize::Large, 16.0 / 9.0, 0.0).expect("capped");
        assert!((capped.height() - 100.0).abs() < 1e-3);
    }

    #[test]
    fn only_a_selected_scene_camera_is_previewed() {
        let mut world = World::new();
        world.insert_resource(EditorCamera::default());
        world.insert_resource(crate::command::UndoStack::new());
        let mut selection = crate::selection::Selection::default();
        assert_eq!(previewed_camera(&world), None, "no selection");
        let light = world
            .spawn((
                regia_scene::prelude::Transform::default(),
                regia_render::PointLight::default(),
            ))
            .id();
        selection.select(light, crate::selection::SelectMode::Replace);
        world.insert_resource(selection.clone());
        assert_eq!(previewed_camera(&world), None, "a light is not a camera");
        let camera = world
            .spawn((
                regia_scene::prelude::Transform::default(),
                regia_render::Camera::default(),
            ))
            .id();
        selection.select(camera, crate::selection::SelectMode::Replace);
        world.insert_resource(selection);
        assert_eq!(previewed_camera(&world), Some(camera));
        crate::viewport_pilot::start(&mut world, camera, crate::viewport_pilot::PilotMode::LookThrough)
            .expect("look through");
        assert_eq!(previewed_camera(&world), None, "not while looking through it");
    }

    #[test]
    fn the_preview_size_is_a_preference() {
        let mut world = World::new();
        world.insert_resource(crate::editor_prefs::EditorPreferences::default());
        set_size(&mut world, PreviewSize::Large);
        assert_eq!(
            world
                .resource::<crate::editor_prefs::EditorPreferences>()
                .text(SIZE_PREF),
            "large"
        );
        let mut prefs = crate::editor_prefs::EditorPreferences::default();
        prefs
            .set(
                SIZE_PREF,
                &crate::editor_prefs::PrefValue::Text("small".to_owned()),
            )
            .expect("a choice");
        let mut world = World::new();
        world.insert_resource(prefs);
        crate::editor_prefs::apply_prefs(None, &mut world);
        assert_eq!(world.resource::<CameraPreview>().size, PreviewSize::Small);
        for size in PreviewSize::ALL {
            assert!(SIZE_CHOICES.contains(&size.name()));
        }
    }
}
