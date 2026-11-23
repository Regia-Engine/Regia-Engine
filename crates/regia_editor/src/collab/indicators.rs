// SPDX-License-Identifier: MPL-2.0
//! Presence indicators compose existing theme tokens, GUID identity and silhouette masks.

use regia_core::math::{Vec2, Vec3};

use super::*;
use crate::overlays::OverlayShape;

fn color(world: &World, slot: u8) -> egui::Color32 {
    let tokens = &crate::theme::tokens(world).overlay;
    [
        tokens.icon_camera,
        tokens.icon_audio,
        tokens.icon_decal,
        tokens.icon_light,
        tokens.gizmo_x,
        tokens.gizmo_y,
        tokens.gizmo_z,
        tokens.reverb_zone,
    ][usize::from(slot).min(MAX_PARTICIPANTS - 1)]
}

/// Adds named color dots for remote users selecting this entity. No local selection is changed.
pub fn outliner_indicator(ui: &mut egui::Ui, world: &World, entity: Entity) {
    for participant in selected_by(world, entity) {
        let label = format!("Selected by {} ({})", participant.name, participant.id);
        let response = ui.label(egui::RichText::new("●").color(color(world, participant.color)));
        let _ = crate::a11y_audit::name(response, egui::WidgetType::Label, &label);
    }
}

fn camera_shapes(
    world: &World,
    frame: &crate::render::PaneFrame,
    participants: &[Participant],
) -> Vec<OverlayShape> {
    let mut shapes = vec![];
    let size = Vec2::new(frame.image.width() as f32, frame.image.height() as f32);
    for participant in participants {
        let camera = participant.presence.camera.camera(1.0);
        let eye = camera.eye();
        let forward = camera.forward();
        let right = camera.right();
        let up = right.cross(forward).normalize_or_zero();
        // A fixed one-metre camera marker, independent of the remote orbit distance.
        let center = eye + forward;
        let corners = [
            center - right * 0.5 - up * 0.3,
            center + right * 0.5 - up * 0.3,
            center + right * 0.5 + up * 0.3,
            center - right * 0.5 + up * 0.3,
        ];
        let color = color(world, participant.color);
        let mut line = |a: Vec3, b: Vec3| {
            if let Some((a, b)) = crate::viewport::clip_segment_to_near(
                frame.view.eye,
                frame.view.forward(),
                frame.view.near * 0.5,
                a,
                b,
            ) && let (Some(a), Some(b)) = (frame.view.project(a, size), frame.view.project(b, size))
            {
                shapes.push(OverlayShape::Line {
                    a,
                    b,
                    color,
                    thickness: 0,
                });
            }
        };
        for (i, corner) in corners.iter().enumerate() {
            line(eye, *corner);
            line(*corner, corners[(i + 1) % 4]);
        }
        line(center + up * 0.3, center + up * 0.65);
    }
    shapes
}

/// Paints remote selection silhouettes and camera gizmos before the local selection outline.
/// Called by the shared live/capture outline hook, so game view and Outline visibility apply.
pub fn paint(frame: &mut crate::render::PaneFrame, world: &World) {
    let Some(session) = world.get_resource::<Session>() else {
        return;
    };
    let participants = session.remote_participants();
    if participants.is_empty() {
        return;
    }
    let _span = tracing::info_span!(target: "regia_editor", "collab_indicators").entered();
    if let Some(ids) = &frame.ids {
        let owners: std::collections::HashMap<_, Vec<_>> = ids
            .entities
            .iter()
            .filter_map(|entity| {
                let guid = world.get::<EntityGuid>(*entity)?;
                let colors: Vec<_> = participants
                    .iter()
                    .filter(|p| p.presence.selection.contains(&guid.0))
                    .map(|p| color(world, p.color))
                    .collect();
                (!colors.is_empty()).then_some((*entity, colors))
            })
            .collect();
        // One union mask, regardless of participant count. The outer pixel survives the
        // existing local one-pixel outline; shared selections alternate their owners' colors.
        let mask = if owners.is_empty() {
            Vec::new()
        } else {
            ids.outline_mask(&|entity| owners.contains_key(&entity), 2)
        };
        for (index, (pixel, outline)) in frame.image.pixels.iter_mut().zip(mask).enumerate() {
            if !outline {
                continue;
            }
            let x = (index % ids.width as usize) as i32;
            let y = (index / ids.width as usize) as i32;
            let closest = (-2..=2)
                .flat_map(|dy| (-2..=2).map(move |dx| (dx, dy)))
                .filter_map(|(dx, dy)| {
                    let (entity, _) = ids.at((x + dx) as u32, (y + dy) as u32)?;
                    owners.get(&entity).map(|colors| (dx * dx + dy * dy, colors))
                })
                .min_by_key(|(distance, _)| *distance);
            if let Some((_, colors)) = closest {
                *pixel = colors[(x as usize + y as usize) % colors.len()];
            }
        }
    }
    let shapes = camera_shapes(world, frame, &participants);
    crate::overlays::paint_shapes(&mut frame.image, &shapes);
}

pub(super) fn participant_color(world: &World, participant: &Participant) -> egui::Color32 {
    color(world, participant.color)
}
