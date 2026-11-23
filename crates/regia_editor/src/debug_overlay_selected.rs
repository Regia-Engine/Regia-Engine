// SPDX-License-Identifier: MPL-2.0
//! Selected-object gameplay inspection (UI parity P-319). The active world's primary selection
//! owns the target; this panel never keeps a separate gameplay target. View state is transient.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::perception::{Perception, draw_senses};
use regia_ai::perception_system::Perceived;
use regia_ai::smart_object::SmartObjectSlot;
use regia_ai::state_tree::StateTree;
use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_framework::abilities::{AbilityBook, AppliedEffects};
use regia_framework::visual_log::VisualShape;
use serde_json::{Value, json};

use crate::gameplay_panel::{LiveSource, entity_param, play_running};
use crate::overlays::OverlayShape;
use crate::pie::PieSession;
use crate::selection::{SelectMode, Selection};
use crate::viewport_agent::Handler;
use crate::views::ViewCamera;

/// Maximum rows from each gameplay collection in one selected-object snapshot.
pub const MAX_DETAILS: usize = 128;

/// Selected-object overlay state, shared by the panel, viewport and agent.
#[derive(Resource, Clone, Debug, Default)]
pub struct DebugOverlaySelected {
    /// Whether the selected object's debug data is drawn.
    pub on: bool,
    /// Primary selection in the snapshot's world; not an independent target.
    pub entity: Option<Entity>,
    /// World the last snapshot came from.
    pub source: LiveSource,
    /// The compact panel's rows; the world-space label uses these same rows.
    pub labels: Vec<String>,
    /// Lines and the label in world space.
    pub shapes: Vec<VisualShape>,
    /// Snapshots collected (disabled pumps do not increment it).
    pub snapshots: u64,
    /// A queued selection that no longer exists reports `entity_not_found` here.
    pub reason: Option<String>,
    pending_entity: Option<Entity>,
    last_ui_frame: Option<u64>,
}

impl DebugOverlaySelected {
    /// Collects only the primary selected entity. Missing components are reported as empty data;
    /// a deleted selection emits no geometry. Disabled overlays do no gameplay queries.
    pub fn snapshot(&mut self, world: &World, source: LiveSource) {
        self.source = source;
        self.entity = world.get_resource::<Selection>().and_then(Selection::primary);
        self.labels.clear();
        self.shapes.clear();
        if !self.on {
            return;
        }
        let Some(entity) = self.entity.filter(|e| world.get_entity(*e).is_ok()) else {
            self.entity = None;
            return;
        };
        let _span = tracing::debug_span!("debug_overlay_selected_snapshot").entered();
        self.snapshots = self.snapshots.saturating_add(1);
        let pose = regia_framework::nav_world::world_pose(world, entity);
        let at: Vec3 = pose.translation.into();
        if let Some(perception) = world.get::<Perception>(entity) {
            self.labels.push(format!(
                "Perception: sight {:.1} m, hearing {:.1} m",
                perception.sight_radius, perception.hearing_radius
            ));
            let mut buffer =
                regia_render::gizmos::GizmoBuffer::<regia_render::gizmos::DefaultGizmoConfigGroup>::new();
            draw_senses(&mut buffer, perception, at, pose.transform_vector3(Vec3::NEG_Z));
            for pair in buffer.list_positions.as_chunks::<2>().0 {
                self.line(pair[0], pair[1]);
            }
            for strip in buffer.strip_positions.split(|p| p.is_nan()) {
                for pair in strip.windows(2) {
                    self.line(pair[0], pair[1]);
                }
            }
        } else {
            self.labels.push("Perception: none".into());
        }
        if let Some(perceived) = world.get::<Perceived>(entity) {
            self.labels.push(format!(
                "Target: {}",
                if perceived.target == Entity::PLACEHOLDER {
                    "none".into()
                } else {
                    perceived.target.to_bits().to_string()
                }
            ));
            for stimulus in perceived.stimuli.iter().take(MAX_DETAILS) {
                self.line(at, stimulus.last_known_position);
                self.labels.push(format!(
                    "Perceived {}: {:?}",
                    stimulus.entity.to_bits(),
                    stimulus.sense
                ));
            }
        }
        let path = world
            .get::<StateTree>(entity)
            .map(|tree| tree.active_path().join(" / "))
            .unwrap_or_default();
        self.labels.push(format!(
            "State path: {}",
            if path.is_empty() { "none" } else { &path }
        ));
        let mut claims = 0;
        for slot in world
            .iter_entities()
            .filter(|slot| {
                slot.get::<SmartObjectSlot>()
                    .is_some_and(|s| s.claimed_by == Some(entity))
            })
            .take(MAX_DETAILS)
        {
            if let Some(claim) = slot.get::<SmartObjectSlot>() {
                self.labels.push(format!(
                    "Claim {}: {}{}",
                    slot.id().to_bits(),
                    claim.role,
                    if claim.in_use { " (in use)" } else { "" }
                ));
                self.line(
                    at,
                    regia_framework::nav_world::world_pose(world, slot.id())
                        .translation
                        .into(),
                );
                claims += 1;
            }
        }
        if claims == 0 {
            self.labels.push("Claims: none".into());
        }
        if let Some(nav) = world.get::<regia_nav::NavAgentState>(entity) {
            let remaining = nav.waypoints.iter().skip(nav.next_waypoint).take(MAX_DETAILS);
            let mut from = at;
            let mut count = 0;
            for to in remaining {
                self.line(from, *to);
                from = *to;
                count += 1;
            }
            self.labels
                .push(format!("Navigation: {count} remaining waypoints"));
        } else {
            self.labels.push("Navigation: no path".into());
        }
        let mut abilities = 0;
        if let Some(book) = world.get::<AbilityBook>(entity) {
            for grant in book.grants.iter().filter(|g| g.run.is_some()).take(MAX_DETAILS) {
                self.labels
                    .push(format!("Ability: {} ({:?})", grant.asset, grant.state));
                abilities += 1;
            }
        }
        if abilities == 0 {
            self.labels.push("Abilities: none active".into());
        }
        let mut effects = 0;
        if let Some(applied) = world.get::<AppliedEffects>(entity) {
            for effect in applied
                .list
                .iter()
                .filter(|effect| !effect.suspended)
                .take(MAX_DETAILS)
            {
                let duration = if effect.duration_ticks == 0 {
                    "until removed".to_owned()
                } else {
                    format!("{} ticks left", effect.remaining_ticks)
                };
                self.labels.push(format!(
                    "Effect: {} (stacks: {}, {duration})",
                    effect.asset, effect.stacks
                ));
                effects += 1;
            }
        }
        if effects == 0 {
            self.labels.push("Effects: none active".into());
        }
        if at.is_finite() {
            self.shapes.push(VisualShape::Text {
                at: at + Vec3::Y,
                text: self.labels.join("\n"),
            });
        }
    }

    fn line(&mut self, from: Vec3, to: Vec3) {
        if from.is_finite() && to.is_finite() {
            self.shapes.push(VisualShape::Line { from, to });
        }
    }

    fn json(&self) -> Value {
        json!({ "on": self.on, "entity": self.entity.map(Entity::to_bits), "source": self.source.name(),
            "labels": self.labels, "shape_count": self.shapes.len(), "snapshots": self.snapshots,
            "queued": self.pending_entity.is_some(), "reason": self.reason })
    }
}

/// Applies a pending agent selection in the active world, then mirrors the selected snapshot for
/// the edit-world panel and the play-world viewport. Called once by the editor frame loop.
pub fn pump(edit: &mut World, session: Option<&mut PieSession>) {
    if !edit.contains_resource::<DebugOverlaySelected>() {
        return;
    }
    let mut state = edit.remove_resource::<DebugOverlaySelected>().unwrap_or_default();
    if let Some(session) = session {
        let play = session.app.world_mut();
        if state.on && crate::theme::tokens(play) != crate::theme::tokens(edit) {
            play.insert_resource(crate::theme::tokens(edit).clone());
        }
        apply_selection(&mut state, play);
        state.snapshot(play, LiveSource::Play);
        play.insert_resource(state.clone());
    } else {
        apply_selection(&mut state, edit);
        state.snapshot(edit, LiveSource::Edit);
    }
    edit.insert_resource(state);
}

fn apply_selection(state: &mut DebugOverlaySelected, world: &mut World) {
    if let Some(entity) = state.pending_entity.take() {
        if world.get_entity(entity).is_ok() {
            world
                .get_resource_or_insert_with::<Selection>(Default::default)
                .select(entity, SelectMode::Replace);
            state.reason = None;
        } else {
            state.reason = Some("entity_not_found".into());
        }
    }
}

/// Applies the same view operation as the panel toggle. In play, entity selection is queued to
/// the active play world; read again after the next editor frame to observe its snapshot.
///
/// # Errors
/// `invalid_params` for malformed arguments; `entity_not_found` for a missing edit-world entity.
pub fn configure(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params, "entity").map_err(|e| parameter_error("invalid_params", e.message))?;
    let on = params
        .get("on")
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| parameter_error("invalid_params", "`on` must be a boolean"))
        })
        .transpose()?;
    let playing = play_running(world);
    if let Some(entity) = entity
        && !playing
        && world.get_entity(entity).is_err()
    {
        return Err(parameter_error(
            "entity_not_found",
            "select an entity in the active world",
        ));
    }
    let mut state = world
        .remove_resource::<DebugOverlaySelected>()
        .unwrap_or_default();
    if let Some(on) = on {
        state.on = on;
    }
    if let Some(entity) = entity {
        state.pending_entity = Some(entity);
        state.entity = Some(entity);
    }
    if !playing {
        apply_selection(&mut state, world);
        state.snapshot(world, LiveSource::Edit);
    }
    if !state.on {
        state.shapes.clear();
        state.labels.clear();
    }
    let answer = state.json();
    world.insert_resource(state);
    Ok(answer)
}

/// Draws the compact selected-object panel. The named checkbox supports Tab and Space through
/// egui and inherits `appearance.ui_scale`; repeated egui passes do not apply a toggle twice.
pub fn show(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::debug_span!("debug_overlay_selected_ui").entered();
    world.init_resource::<DebugOverlaySelected>();
    let state = world.resource::<DebugOverlaySelected>();
    let mut on = state.on;
    ui.heading("Selected Object Debug");
    let changed = ui.checkbox(&mut on, "Show selected object debug data").changed();
    let frame = ui.ctx().cumulative_frame_nr();
    if changed && world.resource::<DebugOverlaySelected>().last_ui_frame != Some(frame) {
        let _ = configure(world, &json!({ "on": on }));
        world.resource_mut::<DebugOverlaySelected>().last_ui_frame = Some(frame);
    }
    let state = world.resource::<DebugOverlaySelected>();
    ui.label(state.source.label());
    if let Some(entity) = state.entity {
        ui.label(format!("Entity {}", entity.to_bits()));
    } else {
        ui.weak("Select an object in the viewport or Outliner.");
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for label in &state.labels {
            ui.label(label);
        }
    });
    if let Some(reason) = &state.reason {
        ui.colored_label(
            crate::theme::ui_tokens(ui.ctx()).overlay.icon_light,
            reason_text(reason),
        )
        .on_hover_text(format!("Agent reason: {reason}"));
    }
}

/// The sentence the panel shows for a stored `reason` code (bug 20, ADR-8586); the agent keeps the
/// code in `reason`.
#[must_use]
pub fn reason_text(reason: &str) -> &'static str {
    match reason {
        "entity_not_found" => "The selected object no longer exists.",
        _ => "The overlay could not show the selection.",
    }
}

/// Projects the snapshot through the same camera used for the viewport and capture.
#[must_use]
pub fn overlays(world: &World, view: &ViewCamera, size: Vec2) -> Vec<OverlayShape> {
    let Some(state) = world.get_resource::<DebugOverlaySelected>().filter(|s| s.on) else {
        return Vec::new();
    };
    if world.get_resource::<Selection>().and_then(Selection::primary) != state.entity {
        return Vec::new();
    }
    crate::visual_log_panel::project_shapes(
        &state.shapes,
        view,
        size,
        crate::theme::tokens(world).overlay.bounds,
    )
}

/// Stable lowercase error codes shared by the two debug-view methods.
pub(crate) fn parameter_error(code: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(
        regia_agent::protocol::INVALID_PARAMS,
        code,
        message,
        "Check the active world and parameters in book/agents/visual-log.md",
    );
    if let Some(data) = error.data.as_mut() {
        data["reason"] = json!(code);
    }
    error
}

/// Agent schema and dispatcher handler for selected-object inspection.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.debug_overlay",
            tool: "debug_overlay",
            description: "Toggle selected-object gameplay debug data; optionally select an entity in the active world. Empty params read state.",
            params: &[
                ParamSchema {
                    name: "entity",
                    kind: "integer",
                    required: false,
                    description: "Primary selection in the active world; queued during play.",
                },
                ParamSchema {
                    name: "on",
                    kind: "boolean",
                    required: false,
                    description: "Draw the selected object's debug data.",
                },
            ],
            result: "{on, entity, source, labels, shape_count, snapshots, queued, reason}",
            mutating: true,
            permission: Permission::Read,
        },
        configure as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ai::perception_system::Stimulus;
    use regia_scene::Transform;

    #[test]
    fn debug_overlay_selected_follows_primary_and_has_no_unselected_geometry() {
        let mut world = World::new();
        let first = world
            .spawn((
                Transform::default(),
                Perception {
                    sight_radius: 3.0,
                    ..Default::default()
                },
                Perceived {
                    stimuli: vec![Stimulus {
                        last_known_position: Vec3::Z,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                regia_nav::NavAgentState {
                    waypoints: vec![Vec3::X, Vec3::X * 2.0],
                    next_waypoint: 0,
                    ..Default::default()
                },
            ))
            .id();
        let second = world
            .spawn((
                Transform::from_translation(Vec3::splat(1000.0)),
                Perception::default(),
            ))
            .id();
        world.spawn((
            Transform::from_translation(Vec3::Y),
            SmartObjectSlot {
                claimed_by: Some(first),
                role: "inspect".into(),
                ..Default::default()
            },
        ));
        world.init_resource::<Selection>();
        world
            .resource_mut::<Selection>()
            .select(first, SelectMode::Replace);
        let mut state = DebugOverlaySelected {
            on: true,
            ..Default::default()
        };
        state.snapshot(&world, LiveSource::Play);
        assert_eq!(state.entity, Some(first));
        assert!(state.labels.iter().any(|s| s.contains("inspect")));
        assert!(
            state
                .labels
                .iter()
                .any(|s| s == "Navigation: 2 remaining waypoints")
        );
        assert!(state.shapes.iter().all(|s| match s {
            VisualShape::Line { from, to } => from.length() < 100.0 && to.length() < 100.0,
            VisualShape::Text { text, .. } => text == &state.labels.join("\n"),
        }));
        world
            .resource_mut::<Selection>()
            .select(second, SelectMode::Replace);
        state.snapshot(&world, LiveSource::Play);
        assert_eq!(state.entity, Some(second));
        assert!(!state.labels.iter().any(|s| s.contains("inspect")));
        world.despawn(second);
        state.snapshot(&world, LiveSource::Play);
        assert!(state.shapes.is_empty());
        assert!(state.entity.is_none());
    }

    #[test]
    fn debug_overlay_selected_disabled_is_idle_and_bad_params_are_atomic() {
        let mut world = World::new();
        let entity = world.spawn(Transform::default()).id();
        configure(&mut world, &json!({ "entity": entity.to_bits(), "on": false })).expect("configure");
        for _ in 0..20 {
            pump(&mut world, None);
        }
        assert_eq!(world.resource::<DebugOverlaySelected>().snapshots, 0);
        assert!(configure(&mut world, &json!({ "on": "yes" })).is_err());
        assert!(!world.resource::<DebugOverlaySelected>().on);
    }

    #[test]
    fn debug_overlay_selected_checkbox_is_named_and_operates_by_keyboard_at_two_scales() {
        for scale in [1.0, 1.75] {
            let ctx = egui::Context::default();
            ctx.enable_accesskit();
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
            let entity = world.spawn(Transform::default()).id();
            configure(&mut world, &json!({ "entity": entity.to_bits(), "on": false })).expect("configure");
            for key in [None, Some(egui::Key::Tab), Some(egui::Key::Space), None] {
                let raw = egui::RawInput {
                    events: key
                        .into_iter()
                        .map(|key| egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        })
                        .collect(),
                    ..Default::default()
                };
                let mut output = ctx.run_ui(raw, |ui| show(ui, &mut world));
                output.textures_delta.clear();
                let update = output.platform_output.accesskit_update.expect("accesskit");
                assert!(crate::a11y_audit::unnamed_widgets(&update).is_empty());
            }
            assert!(
                world.resource::<DebugOverlaySelected>().on,
                "Tab then Space at {scale}"
            );
        }
    }

    #[test]
    fn debug_overlay_selected_reads_live_state_ability_and_effect_rows() {
        use regia_ai::state_tree::StateTreeDef;
        use regia_ai::{BtContext, BtTaskRegistry};
        use regia_framework::abilities as ab;
        let mut world = World::new();
        let def = StateTreeDef::from_ron(r#"(format_version: 2, root: "Root", states: [(name: "Root", children: ["Walking"]), (name: "Walking")])"#).expect("tree definition");
        let mut tree = StateTree::compile(&def, &BtTaskRegistry::default()).expect("tree");
        let entity = world.spawn(Transform::default()).id();
        tree.tick(&mut BtContext {
            world: &mut world,
            entity,
            dt: 1.0 / 60.0,
        });
        world.entity_mut(entity).insert(tree);
        let mut assets = ab::AbilityAssets::default();
        assets.insert_ability("walk", ab::AbilityAsset::new("walk"));
        assets.insert_effect("boost", ab::EffectAsset::new(ab::Lifetime::UntilRemoved));
        world.insert_resource(assets);
        let grant =
            ab::grant_ability(&mut world, entity, "walk", ab::GrantOptions::default()).expect("grant");
        ab::activate(&mut world, entity, grant, ab::StartContext::default()).expect("activate");
        ab::apply_effect(&mut world, ab::ApplyEffect::new("boost", entity)).expect("effect");
        configure(&mut world, &json!({ "entity": entity.to_bits(), "on": true })).expect("select");
        let state = world.resource::<DebugOverlaySelected>();
        assert!(
            state.labels.iter().any(|s| s == "State path: Root / Walking"),
            "{:?}",
            state.labels
        );
        assert!(
            state.labels.iter().any(|s| s.starts_with("Ability: walk")),
            "{:?}",
            state.labels
        );
        assert!(
            state
                .labels
                .iter()
                .any(|s| s == "Effect: boost (stacks: 1, until removed)"),
            "{:?}",
            state.labels
        );
        world.get_mut::<ab::AppliedEffects>(entity).expect("effects").list[0].suspended = true;
        configure(&mut world, &json!({})).expect("refresh");
        assert!(
            world
                .resource::<DebugOverlaySelected>()
                .labels
                .iter()
                .any(|s| s == "Effects: none active")
        );
    }
}
