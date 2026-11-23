// SPDX-License-Identifier: MPL-2.0
//! Per-entity frame scrubbing of the development visual log (UI parity P-320).
//! The panel and agent select the same exact frame; the viewport and captures project the same
//! entries. A closed panel performs no ring scans unless an agent requests a refresh.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec2;
use regia_ecs::prelude::*;
use regia_framework::visual_log::{VisualEntry, VisualLog, VisualShape};
use serde_json::{Value, json};

use crate::debug_overlay_selected::parameter_error;
use crate::gameplay_panel::{LiveSource, play_running};
use crate::overlays::OverlayShape;
use crate::pie::PieSession;
use crate::viewport_agent::Handler;
use crate::views::ViewCamera;

/// Most entities one request can select (bounds request and overlay work).
pub const MAX_SELECTED_ENTITIES: usize = 256;

/// The Visual Log panel's view state and retained recording snapshot.
#[derive(Resource, Debug, Default)]
pub struct VisualLogPanel {
    /// Entities chosen for playback; full ids preserve generations and historical entities.
    pub entities: Vec<Entity>,
    /// Exact frame to draw. None chooses the latest frame on the first refresh.
    pub frame: Option<u64>,
    /// Entries of precisely the chosen frame and entities.
    pub entries: Vec<VisualEntry>,
    /// Entities with any retained entries, including despawned ones.
    pub available_entities: Vec<Entity>,
    /// Retained frame numbers, including empty frames.
    pub frames: Vec<u64>,
    /// Source world of the recording.
    pub source: LiveSource,
    /// Ring snapshots copied; unchanged or closed/idle views do not increment it.
    pub snapshots: u64,
    recording: Option<VisualLog>,
    refresh_requested: bool,
    visible: bool,
    last_ui_frame: Option<u64>,
}

/// Only the selected frame's entries cross into the viewport's world.
#[derive(Resource, Clone, Debug, Default)]
pub struct VisualLogOverlay {
    /// World-space entries to draw.
    pub entries: Vec<VisualEntry>,
}

impl VisualLogPanel {
    /// Copies a changed recording, then selects exactly the requested frame. The snapshot remains
    /// available after play stops; it never rewinds or modifies the game.
    pub fn refresh(&mut self, log: &VisualLog, source: LiveSource) {
        let _span = tracing::debug_span!("visual_log_panel_refresh").entered();
        if self.source != source
            || self.recording.as_ref().is_none_or(|old| {
                old.recording_id() != log.recording_id() || old.revision() != log.revision()
            })
        {
            self.recording = Some(log.clone());
            self.available_entities = log.entities();
            self.frames = log.frames();
            self.snapshots = self.snapshots.saturating_add(1);
        }
        self.source = source;
        self.frame = self.frame.or(log.frame());
        self.select_entries();
    }

    fn select_entries(&mut self) {
        self.entries = match (&self.recording, self.frame) {
            (Some(log), Some(frame)) => log.entries(frame, &self.entities),
            _ => Vec::new(),
        };
    }

    fn json(&self) -> Value {
        let entries: Vec<_> = self
            .entries
            .iter()
            .map(|entry| {
                let shape = match &entry.shape {
                    VisualShape::Line { from, to } => {
                        json!({ "kind": "line", "from": from.to_array(), "to": to.to_array() })
                    }
                    VisualShape::Text { at, text } => {
                        json!({ "kind": "text", "at": at.to_array(), "text": text })
                    }
                };
                json!({ "entity": entry.entity.to_bits(), "frame": entry.frame, "shape": shape })
            })
            .collect();
        json!({ "entities": self.entities.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
            "frame": self.frame, "entries": entries, "frames": self.frames,
            "available_entities": self.available_entities.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
            "source": self.source.name(), "snapshots": self.snapshots,
            "retained": self.frame.is_some_and(|frame| self.frames.contains(&frame)),
            "frame_count": self.recording.as_ref().map_or(0, VisualLog::frame_count),
            "entry_count": self.recording.as_ref().map_or(0, VisualLog::entry_count),
            "dropped": self.recording.as_ref().map_or(0, VisualLog::dropped),
            "queued": self.refresh_requested })
    }
}

/// Refreshes only requested/visible views, then supplies playback to the play viewport. The editor
/// loop calls this once after simulation; repeated egui passes cannot duplicate recording.
pub fn pump(edit: &mut World, session: Option<&mut PieSession>) {
    let Some(mut panel) = edit.remove_resource::<VisualLogPanel>() else {
        return;
    };
    if !panel.refresh_requested && !panel.visible {
        edit.insert_resource(panel);
        return;
    }
    if let Some(session) = session {
        let play = session.app.world_mut();
        if crate::theme::tokens(play) != crate::theme::tokens(edit) {
            play.insert_resource(crate::theme::tokens(edit).clone());
        }
        if let Some(log) = play.get_resource::<VisualLog>() {
            panel.refresh(log, LiveSource::Play);
        }
        play.insert_resource(VisualLogOverlay {
            entries: panel.entries.clone(),
        });
    } else if let Some(log) = edit.get_resource::<VisualLog>() {
        panel.refresh(log, LiveSource::Edit);
    }
    edit.insert_resource(VisualLogOverlay {
        entries: panel.entries.clone(),
    });
    panel.refresh_requested = false;
    panel.visible = false;
    edit.insert_resource(panel);
}

/// Selects entities and an exact recorded frame, or reads the current view with empty params.
/// Historical ids are accepted. A missing/evicted frame draws zero entries, never a nearby frame.
/// During play a refresh is queued to the next editor frame; call again to read its result.
///
/// # Errors
/// `invalid_params` for a malformed entity array, more than 256 entities, or a non-u64 frame.
pub fn configure(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities = params
        .get("entities")
        .map(|value| {
            let values = value
                .as_array()
                .filter(|values| values.len() <= MAX_SELECTED_ENTITIES)
                .ok_or_else(|| {
                    parameter_error(
                        "invalid_params",
                        "`entities` must be an array of at most 256 entity ids",
                    )
                })?;
            let mut entities = values
                .iter()
                .map(|v| {
                    v.as_u64().and_then(Entity::try_from_bits).ok_or_else(|| {
                        parameter_error("invalid_params", "`entities` must contain entity ids")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            entities.sort_unstable();
            entities.dedup();
            Ok::<_, RpcError>(entities)
        })
        .transpose()?;
    let frame = params
        .get("frame")
        .map(|v| {
            v.as_u64()
                .ok_or_else(|| parameter_error("invalid_params", "`frame` must be an unsigned integer"))
        })
        .transpose()?;
    let mut panel = world.remove_resource::<VisualLogPanel>().unwrap_or_default();
    if let Some(entities) = entities {
        panel.entities = entities;
    }
    if let Some(frame) = frame {
        panel.frame = Some(frame);
    }
    if play_running(world) {
        panel.refresh_requested = true;
    } else if let Some(log) = world.get_resource::<VisualLog>() {
        panel.refresh(log, LiveSource::Edit);
    }
    panel.select_entries();
    world.insert_resource(VisualLogOverlay {
        entries: panel.entries.clone(),
    });
    let answer = panel.json();
    world.insert_resource(panel);
    Ok(answer)
}

/// Draws named, keyboard-focusable entity selectors and a frame slider (Tab, Space and arrows).
/// egui's point layout inherits the editor appearance scale. All colors come from theme tokens.
pub fn show(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::debug_span!("visual_log_panel_ui").entered();
    world.init_resource::<VisualLogPanel>();
    world.resource_mut::<VisualLogPanel>().visible = true;
    let panel = world.resource::<VisualLogPanel>();
    let mut entities = panel.entities.clone();
    let mut frame = panel.frame.unwrap_or(0);
    let mut changed = false;
    ui.heading("Visual Log");
    ui.label(format!("Recording: {} world", panel.source.name()));
    if let (Some(first), Some(last)) = (panel.frames.first(), panel.frames.last()) {
        changed |= ui
            .add(egui::Slider::new(&mut frame, *first..=*last).text("Recorded frame"))
            .changed();
    } else {
        ui.weak("No recorded frames. Start play with visual logging enabled.");
    }
    egui::ScrollArea::vertical()
        .id_salt("visual-log-entities")
        .max_height(180.0)
        .show(ui, |ui| {
            for entity in &panel.available_entities {
                let row_id = ui.make_persistent_id(("visual-log-entity", entity.to_bits()));
                ui.scope_builder(egui::UiBuilder::new().id(row_id), |ui| {
                    let selected = entities.contains(entity);
                    if ui
                        .selectable_label(selected, format!("Entity {}", entity.to_bits()))
                        .clicked()
                    {
                        if selected {
                            entities.retain(|e| e != entity);
                        } else if entities.len() < MAX_SELECTED_ENTITIES {
                            entities.push(*entity);
                        }
                        changed = true;
                    }
                });
            }
        });
    let ui_frame = ui.ctx().cumulative_frame_nr();
    if changed && world.resource::<VisualLogPanel>().last_ui_frame != Some(ui_frame) {
        let _ = configure(
            world,
            &json!({ "entities": entities.iter().map(|e| e.to_bits()).collect::<Vec<_>>(), "frame": frame }),
        );
        world.resource_mut::<VisualLogPanel>().last_ui_frame = Some(ui_frame);
    }
    let panel = world.resource::<VisualLogPanel>();
    ui.label(format!(
        "{} entries at frame {}",
        panel.entries.len(),
        panel.frame.unwrap_or(0)
    ));
    if panel.frame.is_some_and(|frame| !panel.frames.contains(&frame)) {
        ui.weak("This frame is not retained; no shapes are drawn.");
    }
    egui::ScrollArea::vertical()
        .id_salt("visual-log-messages")
        .show(ui, |ui| {
            for entry in &panel.entries {
                match &entry.shape {
                    VisualShape::Text { text, .. } => {
                        ui.label(format!("{}: {text}", entry.entity.to_bits()));
                    }
                    VisualShape::Line { from, to } => {
                        ui.label(format!("{}: line {from:?} to {to:?}", entry.entity.to_bits()));
                    }
                }
            }
        });
}

/// Clips and projects world-space primitives for both live drawing and captured pixels.
#[must_use]
pub fn project_shapes(
    shapes: &[VisualShape],
    view: &ViewCamera,
    size: Vec2,
    color: egui::Color32,
) -> Vec<OverlayShape> {
    let _span = tracing::debug_span!("visual_log_project").entered();
    let mut out = Vec::with_capacity(shapes.len());
    for shape in shapes {
        match shape {
            VisualShape::Line { from, to } => {
                if !from.is_finite() || !to.is_finite() {
                    continue;
                }
                let near = view.near * if view.ortho { 0.5 } else { 2.5 };
                if let Some((from, to)) =
                    crate::viewport::clip_segment_to_near(view.eye, view.forward(), near, *from, *to)
                    && let (Some(a), Some(b)) = (view.project(from, size), view.project(to, size))
                    && a.is_finite()
                    && b.is_finite()
                {
                    out.push(OverlayShape::Line {
                        a,
                        b,
                        color,
                        thickness: 0,
                    });
                }
            }
            VisualShape::Text { at, text } => {
                if at.is_finite()
                    && (view.eye - *at).dot(view.forward()) < 0.0
                    && let Some(at) = view.project(*at, size)
                    && at.is_finite()
                {
                    out.push(OverlayShape::Text {
                        at,
                        text: text.clone(),
                        color,
                    });
                }
            }
        }
    }
    out
}

/// Playback shapes for the shared viewport/capture overlay builder.
#[must_use]
pub fn overlays(world: &World, view: &ViewCamera, size: Vec2) -> Vec<OverlayShape> {
    let Some(overlay) = world.get_resource::<VisualLogOverlay>() else {
        return Vec::new();
    };
    if overlay.entries.is_empty() {
        return Vec::new();
    }
    let shapes: Vec<_> = overlay.entries.iter().map(|e| e.shape.clone()).collect();
    project_shapes(
        &shapes,
        view,
        size,
        crate::theme::tokens(world).overlay.icon_camera,
    )
}

/// Agent schema and dispatcher handler for visual-log scrubbing.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.visual_log",
            tool: "visual_log",
            description: "Select historical entities and an exact frame of the bounded visual log. Empty params read/refresh state; play refreshes on the next editor frame.",
            params: &[
                ParamSchema {
                    name: "entities",
                    kind: "array",
                    required: false,
                    description: "Full entity ids to draw; empty draws nothing, maximum 256.",
                },
                ParamSchema {
                    name: "frame",
                    kind: "integer",
                    required: false,
                    description: "Exact recorded frame; an absent or evicted frame draws nothing.",
                },
            ],
            result: "{entities, frame, entries, frames, available_entities, source, snapshots, retained, frame_count, entry_count, dropped, queued}",
            mutating: true,
            permission: Permission::Read,
        },
        configure as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::ViewKind;
    use regia_app::prelude::*;
    use regia_core::math::Vec3;
    use regia_framework::visual_log::VisualLogPlugin;

    #[derive(Resource)]
    struct TestEntity(Entity);

    fn logging_system(entity: Res<TestEntity>, mut log: ResMut<VisualLog>) {
        if log.frame().is_some_and(|frame| (10..=20).contains(&frame)) {
            regia_framework::debug_log!(entity.0, line, log, Vec3::ZERO, Vec3::X);
        }
    }

    #[test]
    fn visual_log_panel_scrubbing_fifteen_draws_exactly_the_recorded_line() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, VisualLogPlugin));
        let entity = app.world_mut().spawn_empty().id();
        app.insert_resource(TestEntity(entity));
        app.add_systems(Update, logging_system);
        for _ in 0..25 {
            app.update();
        }
        let value = configure(
            app.world_mut(),
            &json!({ "entities": [entity.to_bits()], "frame": 15 }),
        )
        .expect("scrub");
        assert_eq!(value["entries"].as_array().expect("entries").len(), 1);
        let view = ViewCamera::of(ViewKind::Front, &crate::viewport::EditorCamera::default());
        let size = Vec2::new(640.0, 480.0);
        let color = crate::theme::tokens(app.world()).overlay.icon_camera;
        let expected = OverlayShape::Line {
            a: view.project(Vec3::ZERO, size).expect("visible"),
            b: view.project(Vec3::X, size).expect("visible"),
            color,
            thickness: 0,
        };
        assert_eq!(
            overlays(app.world(), &view, size),
            std::slice::from_ref(&expected)
        );
        let show = crate::views::ShowFlags {
            grid: false,
            icons: false,
            bounds: false,
            audio: false,
            ..Default::default()
        };
        assert_eq!(
            crate::overlays::build_overlays(app.world(), &view, ViewKind::Front, size, show),
            [expected]
        );
        configure(app.world_mut(), &json!({ "frame": 21 })).expect("empty frame");
        assert!(overlays(app.world(), &view, size).is_empty());
        configure(app.world_mut(), &json!({ "frame": 15, "entities": [] })).expect("empty selection");
        assert!(overlays(app.world(), &view, size).is_empty());
    }

    #[test]
    fn visual_log_panel_preserves_history_after_despawn_and_is_idle_when_closed() {
        let mut world = World::new();
        let entity = world.spawn_empty().id();
        let mut log = VisualLog::new(2, 1);
        log.begin_frame(15);
        regia_framework::debug_log!(entity, text, log, Vec3::ZERO, "arrived");
        world.insert_resource(log);
        world.despawn(entity);
        configure(
            &mut world,
            &json!({ "entities": [entity.to_bits()], "frame": 15 }),
        )
        .expect("history");
        let snapshots = world.resource::<VisualLogPanel>().snapshots;
        for _ in 0..20 {
            pump(&mut world, None);
        }
        assert_eq!(world.resource::<VisualLogPanel>().snapshots, snapshots);
        assert_eq!(world.resource::<VisualLogPanel>().entries.len(), 1);
        for frame in 16..=17 {
            world.resource_mut::<VisualLog>().begin_frame(frame);
        }
        let value = configure(&mut world, &json!({})).expect("refresh");
        assert_eq!(value["frame_count"], 2);
        assert_eq!(value["retained"], false);
        assert_eq!(value["entries"], json!([]));
        assert!(configure(&mut world, &json!({ "entities": ["bad"], "frame": 17 })).is_err());
        assert_eq!(world.resource::<VisualLogPanel>().frame, Some(15));
    }

    #[test]
    fn visual_log_panel_keyboard_selects_entities_and_scrubs_at_two_scales() {
        use crate::canvas_keys::test_input::{focused_name, frame, key, tab_to};
        for scale in [1.0, 1.75] {
            let ctx = egui::Context::default();
            let mut world = World::new();
            let mut prefs = crate::editor_prefs::EditorPreferences::default();
            prefs
                .set(
                    "appearance.ui_scale",
                    &crate::editor_prefs::PrefValue::Float(scale),
                )
                .expect("scale preference");
            world.insert_resource(prefs);
            crate::editor_prefs::apply_prefs(Some(&ctx), &mut world);
            let mut ids = [world.spawn_empty().id(), world.spawn_empty().id()];
            ids.sort();
            let [newcomer, entity] = ids;
            let mut log = VisualLog::default();
            for number in 10..=20 {
                log.begin_frame(number);
                regia_framework::debug_log!(entity, line, log, Vec3::ZERO, Vec3::X);
            }
            world.insert_resource(log);
            configure(&mut world, &json!({ "frame": 15 })).expect("snapshot");
            let size = [900.0, 900.0];
            let audit = |update: &egui::accesskit::TreeUpdate| {
                assert!(crate::a11y_audit::unnamed_widgets(update).is_empty());
            };
            audit(&frame(&ctx, size, vec![], |ui| show(ui, &mut world)));
            // Follow named Tab stops independently: Right on an entity row is focus navigation,
            // so it must not be inserted between focusing that row and activating it with Space.
            audit(&tab_to(&ctx, size, "Recorded frame", false, 10, |ui| {
                show(ui, &mut world)
            }));
            audit(&frame(
                &ctx,
                size,
                vec![key(egui::Key::ArrowRight, egui::Modifiers::NONE)],
                |ui| show(ui, &mut world),
            ));
            frame(&ctx, size, vec![], |ui| show(ui, &mut world));
            assert_eq!(
                world.resource::<VisualLogPanel>().frame,
                Some(16),
                "timeline by keyboard at {scale}"
            );
            audit(&tab_to(
                &ctx,
                size,
                &format!("Entity {}", entity.to_bits()),
                false,
                10,
                |ui| show(ui, &mut world),
            ));
            // A newly recorded entity sorts before the focused row. Stable entity widget ids
            // must keep Space bound to the same entity while the live list grows.
            {
                let mut log = world.resource_mut::<VisualLog>();
                log.begin_frame(21);
                regia_framework::debug_log!(newcomer, line, log, Vec3::ZERO, Vec3::Y);
            }
            configure(&mut world, &json!({})).expect("new entity snapshot");
            let update = frame(&ctx, size, vec![], |ui| show(ui, &mut world));
            audit(&update);
            assert_eq!(
                focused_name(&update),
                Some(format!("Entity {}", entity.to_bits()))
            );
            audit(&frame(
                &ctx,
                size,
                vec![key(egui::Key::Space, egui::Modifiers::NONE)],
                |ui| show(ui, &mut world),
            ));
            frame(&ctx, size, vec![], |ui| show(ui, &mut world));
            let panel = world.resource::<VisualLogPanel>();
            assert_eq!(panel.entities, [entity], "entity by keyboard at {scale}");
            assert_eq!(panel.entries.len(), 1);
            assert_eq!(panel.entries[0].frame, 16);
        }
    }

    #[test]
    fn visual_log_panel_possessed_capture_diffs_against_a_control_without_goldens() {
        let mut world = World::new();
        let entity = world.spawn_empty().id();
        let mut log = VisualLog::default();
        log.begin_frame(15);
        regia_framework::debug_log!(entity, line, log, Vec3::ZERO, Vec3::X);
        world.insert_resource(log);
        let view = ViewCamera::of(ViewKind::Front, &crate::viewport::EditorCamera::default());
        let mut frame = crate::render::PaneFrame {
            image: egui::ColorImage::filled([64, 64], crate::theme::tokens(&world).overlay.grid_minor),
            view,
            ids: None,
            info: crate::render::PaneInfo {
                possessed: true,
                ..Default::default()
            },
        };
        let control = frame.image.clone();
        crate::overlays::paint_capture_overlays(&mut frame, &world, ViewKind::Front);
        assert_eq!(frame.image.pixels, control.pixels, "off by default");
        configure(
            &mut world,
            &json!({ "entities": [entity.to_bits()], "frame": 15 }),
        )
        .expect("scrub");
        crate::overlays::paint_capture_overlays(&mut frame, &world, ViewKind::Front);
        let changed = frame
            .image
            .pixels
            .iter()
            .zip(&control.pixels)
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            changed > 0,
            "explicit debug lines remain visible in the game camera"
        );
        let color = crate::theme::tokens(&world).overlay.icon_camera;
        assert!(
            frame
                .image
                .pixels
                .iter()
                .zip(&control.pixels)
                .all(|(a, b)| a == b || *a == color)
        );
    }

    #[test]
    fn visual_log_panel_drops_projection_overflow() {
        let world = World::new();
        let view = ViewCamera::of(ViewKind::Front, &crate::viewport::EditorCamera::default());
        let huge = Vec3::new(f32::MAX, 0.0, 0.0);
        let size = Vec2::new(640.0, 480.0);
        assert!(!view.project(huge, size).expect("in front").is_finite());
        let shapes = [
            VisualShape::Line {
                from: Vec3::ZERO,
                to: huge,
            },
            VisualShape::Text {
                at: huge,
                text: "outside".into(),
            },
        ];
        assert!(
            project_shapes(
                &shapes,
                &view,
                size,
                crate::theme::tokens(&world).overlay.icon_camera
            )
            .is_empty()
        );
    }

    #[test]
    fn visual_log_panel_new_recording_with_same_revision_refreshes_its_cache() {
        let mut world = World::new();
        let entity = world.spawn_empty().id();
        let mut first = VisualLog::default();
        first.begin_frame(15);
        regia_framework::debug_log!(entity, line, first, Vec3::ZERO, Vec3::X);
        let mut panel = VisualLogPanel {
            entities: vec![entity],
            frame: Some(15),
            ..Default::default()
        };
        panel.refresh(&first, LiveSource::Play);
        let mut second = VisualLog::default();
        second.begin_frame(15);
        regia_framework::debug_log!(entity, line, second, Vec3::ZERO, Vec3::Y);
        assert_eq!(first.revision(), second.revision());
        panel.refresh(&second, LiveSource::Play);
        assert_eq!(panel.entries, second.entries(15, &[entity]));
        assert_eq!(panel.snapshots, 2);
    }
}
