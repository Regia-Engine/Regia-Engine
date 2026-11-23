// SPDX-License-Identifier: MIT
// Portions ported from Fyrox (https://github.com/FyroxEngine/Fyrox, commit 0876f67),
// editor/src/plugins/animation/ruler.rs. Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors,
// MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! The Clip viewer: pick a clip of the preview rig, scrub or play it on the rig, edit its
//! notifies on the timeline, author named curves and property tracks in the curve editor, set
//! the root-motion mode and see the root's trajectory. Everything authored lands in the rig's
//! `.animedit.ron` sidecar (undoable edits).
//!
//! The timeline ruler is ported from Fyrox (<https://github.com/FyroxEngine/Fyrox>),
//! `editor/src/plugins/animation/ruler.rs` (`Ruler::draw`: ticks from the shared curve
//! transform's x steps with `"{x:.1}s"` labels, signal markers as triangles on the bottom
//! edge, click/drag to scrub), Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine
//! contributors, MIT licence (notice in `docs/ported-sources.toml`). Adapted to egui; markers
//! are draggable notifies.

use regia_animation::authoring::{ClipEdits, EditCurve, EditPropertyTrack};
use regia_animation::clip::{AnimationClip, Notify, RootMotionMode};
use regia_animation::curve::{Curve, CurveKey, CurveKeyKind};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_render::RenderDevice;

use super::curve_view::{CurveAction, apply_action, curve_editor};
use super::view2d::{STANDARD_GRID_SIZE, View2d};
use super::{AnimDocs, AnimEditor, PendingEdit, PreviewRig, apply_pending, edited_clip};

/// The clip's edits, created from the imported clip when absent (so its imported notifies are
/// kept when the first edit replaces the list).
pub fn clip_edits_mut<'a>(docs: &'a mut AnimDocs, imported: &AnimationClip) -> &'a mut ClipEdits {
    if docs.edits.clip(&imported.name).is_none() {
        let edits = docs.edits.clip_mut(&imported.name);
        edits.notifies = imported.notifies.clone();
    }
    docs.edits.clip_mut(&imported.name)
}

/// Total length of a polyline (the distance travelled, not the net displacement).
#[must_use]
pub fn path_length(path: &[Vec3]) -> f32 {
    path.windows(2).map(|pair| pair[0].distance(pair[1])).sum()
}

/// The root's trajectory over the clip (object space, `samples + 1` points from the origin).
#[must_use]
pub fn root_trajectory(clip: &AnimationClip, samples: usize) -> Vec<Vec3> {
    let mut out = vec![Vec3::ZERO];
    let mut position = Vec3::ZERO;
    let mut yaw = Quat::IDENTITY;
    let n = samples.max(1);
    for i in 0..n {
        let t0 = clip.duration * i as f32 / n as f32;
        let t1 = clip.duration * (i + 1) as f32 / n as f32;
        let Some((dt, dr)) = clip.root_motion_delta(t0, t1, false) else {
            break;
        };
        position += yaw * dt;
        yaw = (yaw * dr).normalize();
        out.push(position);
    }
    out
}

/// Draws the Clip panel.
#[tracing::instrument(target = "regia_editor", skip_all, name = "anim_clip_panel")]
pub fn anim_clip_panel(ui: &mut egui::Ui, world: &mut World, device: Option<&RenderDevice>) {
    let _span = tracing::info_span!("anim_clip_panel").entered();
    if !world.contains_resource::<AnimEditor>() {
        world.init_resource::<AnimEditor>();
    }
    let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
    let mut pending = Vec::new();
    let mut save: Option<Option<String>> = None;
    world.resource_scope(|_world, mut editor: Mut<AnimEditor>| {
        let editor = &mut *editor;
        let rig = editor.rig_or_demo().clone();
        save = draw(ui, editor, &rig, device, dt, &mut pending);
    });
    apply_pending(world, pending);
    if let Some(path) = save {
        super::graph_panel::perform(world, super::graph_panel::PanelAction::SaveEdits(path));
    }
    if world.resource::<AnimEditor>().ui.clip_playing {
        ui.ctx().request_repaint();
    }
}

fn draw(
    ui: &mut egui::Ui,
    editor: &mut AnimEditor,
    rig: &PreviewRig,
    device: Option<&RenderDevice>,
    dt: f32,
    pending: &mut Vec<PendingEdit>,
) -> Option<Option<String>> {
    let mut save = None;
    let names = rig.clip_names();
    if editor.ui.clip.as_ref().is_none_or(|c| !names.contains(c)) {
        editor.ui.clip = names.first().cloned();
    }
    let Some(clip_name) = editor.ui.clip.clone() else {
        ui.weak("The preview rig has no clips.");
        return None;
    };
    let imported = rig.clip(&clip_name).cloned()?;
    let clip = edited_clip(&imported, &editor.docs.edits);
    // Once per frame: egui can run the panel twice in one frame (a discarded pass).
    let frame_nr = ui.ctx().cumulative_frame_nr();
    let advanced_id = egui::Id::new("anim-clip-advanced-frame");
    let fresh_frame = ui.data(|d| d.get_temp::<u64>(advanced_id)) != Some(frame_nr);
    if editor.ui.clip_playing && fresh_frame {
        ui.data_mut(|d| d.insert_temp(advanced_id, frame_nr));
        // Speed, loop / once / ping-pong over the trimmed range (P-207, P-213).
        let mut playing = true;
        editor.ui.scrub = editor
            .ui
            .clip_preview
            .advance(editor.ui.scrub, dt, clip.duration, &mut playing);
        editor.ui.clip_playing = playing;
    }
    let revision = editor.revision;
    editor
        .preview
        .sample_clip(rig, &editor.docs, revision, &clip_name, editor.ui.scrub);

    ui.horizontal_wrapped(|ui| {
        ui.strong("Clip");
        let combo = egui::ComboBox::from_id_salt("anim-clip")
            .selected_text(clip_name.as_str())
            .show_ui(ui, |ui| {
                // Search and sort (P-216), then a screenful at a time (ADR-7306): a project can
                // hold thousands of clips.
                let search = ui.add(
                    egui::TextEdit::singleline(&mut editor.ui.clip_list.filter)
                        .hint_text("Search clips")
                        .desired_width(160.0),
                );
                crate::a11y_audit::name(search, egui::WidgetType::TextEdit, "Search clips");
                ui.horizontal(|ui| {
                    ui.weak("Sort");
                    for sort in super::clip_preview::ClipSort::ALL {
                        ui.selectable_value(&mut editor.ui.clip_list.sort, sort, sort.label());
                    }
                });
                let listed = editor.ui.clip_list.list(rig);
                if listed.is_empty() {
                    ui.weak("No clip matches.");
                }
                let mut picked = None;
                crate::virtual_list::rows(
                    ui,
                    "anim-clip-rows",
                    listed.len(),
                    crate::virtual_list::DROP_DOWN_HEIGHT,
                    |ui, i| {
                        let n = &listed[i];
                        let length = rig.clip(n).map_or(0.0, |c| c.duration);
                        if ui
                            .selectable_label(n == &clip_name, format!("{n}  ({length:.2} s)"))
                            .clicked()
                        {
                            picked = Some(n.clone());
                        }
                    },
                );
                if let Some(n) = picked {
                    editor.ui.clip_list.picked(&n);
                    editor.ui.clip = Some(n);
                    editor.ui.scrub = 0.0;
                    editor.ui.selected_notify = None;
                    editor.ui.curve = None;
                    editor.ui.clip_preview.range = None;
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Clip");
        let label = if editor.ui.clip_playing { "Pause" } else { "Play" };
        if ui.button(label).clicked() {
            let (start, end) = editor.ui.clip_preview.range_of(clip.duration);
            // A once-through preview that ended starts over.
            if !editor.ui.clip_playing
                && editor.ui.clip_preview.looping == super::clip_preview::ClipLoop::Once
                && editor.ui.scrub >= end
            {
                editor.ui.scrub = start;
            }
            editor.ui.clip_playing = !editor.ui.clip_playing;
        }
        ui.monospace(format!("{:.3} / {:.3} s", editor.ui.scrub, clip.duration));
        ui.separator();
        ui.label("Root motion");
        let current = clip.root_motion.as_ref().map_or(RootMotionMode::None, |r| r.mode);
        for mode in [
            RootMotionMode::None,
            RootMotionMode::XZ,
            RootMotionMode::XYZ,
            RootMotionMode::Yaw,
        ] {
            if ui
                .selectable_label(current == mode, format!("{mode:?}"))
                .clicked()
            {
                let imported = (*imported).clone();
                pending.push(PendingEdit::new("set root motion", None, move |d| {
                    clip_edits_mut(d, &imported).root_motion = Some(mode);
                    Ok(())
                }));
            }
        }
        ui.separator();
        match &editor.edits_path {
            Some(p) => ui.weak(p.display().to_string()),
            None => ui.weak("(no .animedit.ron path)"),
        };
        if editor.edits_dirty() {
            ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.unsaved, "· unsaved");
        }
        let path_id = egui::Id::new("anim-edits-path");
        let mut path: String = ui.data(|d| d.get_temp(path_id)).unwrap_or_default();
        ui.add(
            egui::TextEdit::singleline(&mut path)
                .hint_text("x.animedit.ron")
                .desired_width(120.0),
        );
        if ui.button("Save edits").clicked() {
            save = Some((!path.is_empty()).then(|| path.clone()));
        }
        ui.data_mut(|d| d.insert_temp(path_id, path));
    });
    if !editor.toast.is_empty() {
        ui.weak(&editor.toast);
    }
    playback_ui(ui, editor, &clip);

    egui::Panel::right("anim-clip-preview")
        .resizable(true)
        .default_size(280.0)
        .show(ui, |ui| {
            let skeleton = rig.edited_skeleton(&editor.docs.edits);
            let globals = editor.preview.globals(&skeleton);
            let mut orbit = editor.preview.orbit;
            let height = ui.available_height() * 0.6;
            let shown = ui.allocate_ui(egui::vec2(ui.available_width(), height), |ui| {
                if let Some(j) = super::preview::preview_widget(
                    ui,
                    "anim-clip-preview",
                    &skeleton,
                    &globals,
                    &mut orbit,
                    editor.ui.selected_joint,
                    device,
                ) {
                    editor.ui.selected_joint = Some(j);
                }
            });
            // Onion skins and the root-motion floor over the preview (P-211, P-215).
            let options = &editor.ui.clip_preview;
            let ghosts = super::clip_preview::ghost_globals(options, &clip, &skeleton, editor.ui.scrub);
            let floor = options.root_floor.then(|| {
                let (_, radius) = super::preview::rig_bounds(&globals);
                let spacing = (radius * 0.5).max(0.05);
                super::clip_preview::floor_lines(
                    &clip,
                    options.range_of(clip.duration),
                    editor.ui.scrub,
                    spacing,
                    6,
                )
            });
            super::clip_preview::paint_overlays(
                &ui.painter_at(shown.response.rect),
                shown.response.rect,
                &skeleton,
                &globals,
                orbit,
                options,
                floor.as_ref(),
                &ghosts,
            );
            editor.preview.orbit = orbit;
            ui.separator();
            root_motion_plot(ui, &clip, editor.ui.scrub);
        });

    egui::CentralPanel::default().show(ui, |ui| {
        editor.ui.timeline_view.position.y = 0.0;
        if let Some(t) = timeline(ui, editor, &clip, &imported, pending) {
            editor.ui.scrub = t.clamp(0.0, clip.duration);
        }
        ui.separator();
        notify_inspector(ui, editor, &clip, &imported, pending);
        ui.separator();
        curves_ui(ui, editor, &clip, &imported, pending);
    });
    save
}

/// The playback row: speed, loop mode, the trimmed preview range, onion skins and the
/// root-motion floor (P-207, P-211, P-213, P-215). Every control is a focusable widget.
fn playback_ui(ui: &mut egui::Ui, editor: &mut AnimEditor, clip: &AnimationClip) {
    use super::clip_preview::{ClipLoop, MAX_GHOSTS, MAX_SPEED, MIN_SPEED};
    let options = &mut editor.ui.clip_preview;
    ui.horizontal_wrapped(|ui| {
        let speed = ui.add(
            egui::Slider::new(&mut options.speed, MIN_SPEED..=MAX_SPEED)
                .logarithmic(true)
                .text("speed"),
        );
        crate::a11y_audit::name(speed, egui::WidgetType::Slider, "Preview speed");
        for mode in ClipLoop::ALL {
            if ui
                .selectable_label(options.looping == mode, mode.label())
                .clicked()
            {
                options.looping = mode;
                options.reverse = false;
            }
        }
        ui.separator();
        let (mut start, mut end) = options.range_of(clip.duration);
        ui.label("Range");
        let a = ui.add(
            egui::DragValue::new(&mut start)
                .speed(0.01)
                .range(0.0..=clip.duration)
                .suffix(" s"),
        );
        let a = crate::a11y_audit::name(a, egui::WidgetType::DragValue, "Preview range start");
        let b = ui.add(
            egui::DragValue::new(&mut end)
                .speed(0.01)
                .range(0.0..=clip.duration)
                .suffix(" s"),
        );
        let b = crate::a11y_audit::name(b, egui::WidgetType::DragValue, "Preview range end");
        if (a.changed() || b.changed()) && options.set_range(Some([start, end])).is_err() {
            editor.toast = "the preview range needs its start before its end".into();
        }
        if ui
            .add_enabled(options.range.is_some(), egui::Button::new("Whole clip"))
            .clicked()
        {
            options.range = None;
        }
        ui.separator();
        // Toggles commit once per egui frame: turning onion skins on changes the layout, egui
        // discards that pass, and the rerun sees the same click (review sprint-w12-rev-g7 #4).
        let mut onion = options.onion.enabled;
        if ui.checkbox(&mut onion, "Onion skins").changed()
            && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new("anim-clip-onion"))
        {
            options.onion.enabled = onion;
        }
        if options.onion.enabled {
            let max = u32::from(MAX_GHOSTS);
            let mut before = u32::from(options.onion.before);
            let mut after = u32::from(options.onion.after);
            let r = ui.add(egui::DragValue::new(&mut before).range(0..=max).prefix("before "));
            crate::a11y_audit::name(r, egui::WidgetType::DragValue, "Ghosts before");
            let r = ui.add(egui::DragValue::new(&mut after).range(0..=max).prefix("after "));
            crate::a11y_audit::name(r, egui::WidgetType::DragValue, "Ghosts after");
            options.onion.before = u8::try_from(before).unwrap_or(MAX_GHOSTS);
            options.onion.after = u8::try_from(after).unwrap_or(MAX_GHOSTS);
            let r = ui.add(
                egui::DragValue::new(&mut options.onion.step)
                    .speed(0.005)
                    .range(0.01..=2.0)
                    .prefix("step ")
                    .suffix(" s"),
            );
            crate::a11y_audit::name(r, egui::WidgetType::DragValue, "Ghost step");
            let r = ui.add(egui::Slider::new(&mut options.onion.opacity, 0.05..=1.0).text("opacity"));
            crate::a11y_audit::name(r, egui::WidgetType::Slider, "Ghost opacity");
        }
        ui.separator();
        let mut floor = options.root_floor;
        if ui.checkbox(&mut floor, "Root-motion floor").changed()
            && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new("anim-clip-root-floor"))
        {
            options.root_floor = floor;
        }
    });
}

/// The ruler + notify lane. Returns a new playhead time when scrubbed.
fn timeline(
    ui: &mut egui::Ui,
    editor: &mut AnimEditor,
    clip: &AnimationClip,
    imported: &std::sync::Arc<AnimationClip>,
    pending: &mut Vec<PendingEdit>,
) -> Option<f32> {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 64.0),
        egui::Sense::click_and_drag(),
    );
    let view = &mut editor.ui.timeline_view;
    let old_width = view.rect.width();
    view.rect = rect;
    view.scale.y = 1.0;
    // A resize (including egui's first sizing pass) keeps the visible time range.
    if old_width >= 2.0 && (old_width - rect.width()).abs() > 0.5 {
        view.scale.x *= rect.width() / old_width;
    }
    // Fit each clip once when it is first shown (F refits).
    let fit_id = response.id.with(("fitted", clip.name.as_str()));
    let fitted = ui.data(|d| d.get_temp::<bool>(fit_id)).is_some();
    if old_width < 2.0 || !fitted || ui.input(|i| i.key_pressed(egui::Key::F)) {
        view.zoom_to_fit([0.0, -1.0, clip.duration.max(0.1), 1.0], 0.04);
        view.scale.y = 1.0;
        view.position.y = 0.0;
        ui.data_mut(|d| d.insert_temp(fit_id, true));
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(30));
    // Clip extent.
    let x0 = view.to_screen(egui::vec2(0.0, 0.0)).x;
    let x1 = view.to_screen(egui::vec2(clip.duration, 0.0)).x;
    painter.rect_filled(
        egui::Rect::from_x_y_ranges(x0..=x1, rect.top()..=rect.bottom()),
        0.0,
        egui::Color32::from_rgb(36, 42, 52),
    );
    // Ruler ticks (Fyrox: ticks on the lower half, "{x:.1}s" labels).
    for x in view.x_steps(STANDARD_GRID_SIZE) {
        let sx = view.to_screen(egui::vec2(x, 0.0)).x;
        painter.line_segment(
            [
                egui::pos2(sx, rect.top() + 14.0),
                egui::pos2(sx, rect.top() + 24.0),
            ],
            egui::Stroke::new(1.0, egui::Color32::from_gray(150)),
        );
        painter.text(
            egui::pos2(sx + 2.0, rect.top()),
            egui::Align2::LEFT_TOP,
            format!("{x:.1}s"),
            egui::FontId::monospace(10.0),
            egui::Color32::from_gray(170),
        );
    }
    // Notify markers (Fyrox's signals: triangles on the bottom edge).
    let lane_y = rect.bottom() - 4.0;
    let pointer = response.hover_pos();
    let mut hovered = None;
    for (i, n) in clip.notifies.iter().enumerate() {
        let x = view.to_screen(egui::vec2(n.time, 0.0)).x;
        let size = 12.0;
        let near = pointer.is_some_and(|p| (p.x - x).abs() < size * 0.6 && p.y > lane_y - size - 14.0);
        if near {
            hovered = Some(i);
        }
        let colour = if editor.ui.selected_notify == Some(i) {
            crate::theme::ui_tokens(painter.ctx()).accents.select
        } else {
            egui::Color32::from_rgb(70, 110, 190)
        };
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(x - size * 0.5, lane_y - size),
                egui::pos2(x + size * 0.5, lane_y - size),
                egui::pos2(x, lane_y),
            ],
            colour,
            egui::Stroke::NONE,
        ));
        painter.text(
            egui::pos2(x + 7.0, lane_y - size - 2.0),
            egui::Align2::LEFT_BOTTOM,
            &n.name,
            egui::FontId::proportional(10.0),
            egui::Color32::from_gray(210),
        );
    }
    // The notify list's keyboard focus (FU-5).
    if let Some(n) = editor.ui.notify_keys.focused().and_then(|i| clip.notifies.get(i)) {
        let x = view.to_screen(egui::vec2(n.time, 0.0)).x;
        crate::canvas_keys::focus_ring(
            &painter,
            egui::Rect::from_min_max(egui::pos2(x - 6.0, lane_y - 12.0), egui::pos2(x + 6.0, lane_y)),
        );
    }
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("Clip timeline canvas, {} notifies", clip.notifies.len()),
    );
    // The trimmed preview range (P-213): shade what is cut and draw the end handle.
    let (range_start, range_end) = editor.ui.clip_preview.range_of(clip.duration);
    let tokens = crate::theme::ui_tokens(painter.ctx());
    let rs = view.to_screen(egui::vec2(range_start, 0.0)).x;
    let re = view.to_screen(egui::vec2(range_end, 0.0)).x;
    for (from, to) in [(x0, rs), (re, x1)] {
        if to > from {
            painter.rect_filled(
                egui::Rect::from_x_y_ranges(from..=to, rect.top()..=rect.bottom()),
                0.0,
                tokens.shade,
            );
        }
    }
    painter.line_segment(
        [egui::pos2(re, rect.top()), egui::pos2(re, rect.bottom())],
        egui::Stroke::new(3.0, tokens.accents.active),
    );
    let playhead = view.to_screen(egui::vec2(editor.ui.scrub, 0.0)).x;
    painter.line_segment(
        [
            egui::pos2(playhead, rect.top()),
            egui::pos2(playhead, rect.bottom()),
        ],
        egui::Stroke::new(2.0, crate::theme::ui_tokens(painter.ctx()).accents.alert),
    );
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0
            && let Some(anchor) = response.hover_pos()
        {
            view.zoom_about(anchor, egui::vec2((1.0 + scroll * 0.002).clamp(0.5, 2.0), 1.0));
        }
    }
    // The keyboard route, drawn every frame (a row that is not laid out loses the focus).
    notify_list(ui, editor, clip, imported, pending);
    let view = &mut editor.ui.timeline_view;
    let to_time = |p: egui::Pos2| view.to_value(p).x.clamp(0.0, clip.duration);
    // Dragging the end handle trims the preview range, not the clip.
    let end_drag_id = egui::Id::new("anim-range-end-drag");
    if response.drag_started()
        && hovered.is_none()
        && response
            .interact_pointer_pos()
            .is_some_and(|p| (p.x - re).abs() <= 6.0)
    {
        ui.data_mut(|d| d.insert_temp(end_drag_id, true));
    }
    if ui.data(|d| d.get_temp::<bool>(end_drag_id)).unwrap_or(false) {
        if response.drag_stopped() || !response.dragged() {
            ui.data_mut(|d| d.remove::<bool>(end_drag_id));
        }
        if let Some(p) = response.interact_pointer_pos() {
            editor.ui.clip_preview.drag_end(view.to_value(p).x, clip.duration);
        }
        return None;
    }
    let drag_id = egui::Id::new("anim-notify-drag");
    if response.drag_started() {
        ui.data_mut(|d| d.insert_temp(drag_id, hovered));
        if hovered.is_some() {
            editor.ui.selected_notify = hovered;
        }
    }
    let dragging: Option<usize> = ui.data(|d| d.get_temp::<Option<usize>>(drag_id)).flatten();
    if response.drag_stopped() {
        ui.data_mut(|d| d.remove::<Option<usize>>(drag_id));
    }
    if response.dragged()
        && let Some(p) = response.interact_pointer_pos()
    {
        let t = to_time(p);
        match dragging {
            Some(i) => {
                let imported = (**imported).clone();
                pending.push(PendingEdit::new(
                    "move notify",
                    Some(format!("notify:{}:{i}", imported.name)),
                    move |d| {
                        let edits = clip_edits_mut(d, &imported);
                        let n = edits
                            .notifies
                            .get_mut(i)
                            .ok_or_else(|| format!("no notify {i}"))?;
                        n.time = t;
                        Ok(())
                    },
                ));
                return None;
            }
            None => return Some(t),
        }
    }
    if response.double_clicked()
        && let Some(p) = response.interact_pointer_pos()
    {
        let t = to_time(p);
        let imported = (**imported).clone();
        editor.ui.selected_notify = Some(clip.notifies.len());
        pending.push(PendingEdit::new("add notify", None, move |d| {
            clip_edits_mut(d, &imported).notifies.push(Notify {
                time: t,
                name: "Notify".into(),
                payload: None,
            });
            Ok(())
        }));
        return None;
    }
    if response.clicked()
        && let Some(p) = response.interact_pointer_pos()
    {
        if hovered.is_some() {
            editor.ui.selected_notify = hovered;
            return None;
        }
        return Some(to_time(p));
    }
    None
}

/// How far Left / Right move a notify, seconds (Shift: ten times).
pub const NOTIFY_STEP: f32 = 1.0 / 30.0;

/// The pending edit of a notify move: the marker drag's, merge key included.
fn move_notify_edit(imported: &AnimationClip, i: usize, t: f32) -> PendingEdit {
    let imported = imported.clone();
    PendingEdit::new(
        "move notify",
        Some(format!("notify:{}:{i}", imported.name)),
        move |d| {
            let edits = clip_edits_mut(d, &imported);
            let n = edits
                .notifies
                .get_mut(i)
                .ok_or_else(|| format!("no notify {i}"))?;
            n.time = t;
            Ok(())
        },
    )
}

/// The notify list under the clip timeline (FU-5, the convention of `canvas_keys`): Up / Down
/// move the focus; Enter selects the notify (its fields show below); Left / Right move it one
/// step (Shift: ten; the marker drag's edit, so presses on one notify coalesce as a drag does);
/// Delete removes it (the inspector's **Delete**); Ctrl+Shift+A adds a notify one step later (the
/// double-click's add).
fn notify_list(
    ui: &mut egui::Ui,
    editor: &mut AnimEditor,
    clip: &AnimationClip,
    imported: &std::sync::Arc<AnimationClip>,
    pending: &mut Vec<PendingEdit>,
) {
    use crate::canvas_keys::{RowKey, list_rows};
    let n = clip.notifies.len();
    let name = |i: usize| {
        clip.notifies
            .get(i)
            .map_or_else(String::new, |m| format!("Notify {} at {:.3} s", m.name, m.time))
    };
    let height = (ui.spacing().interact_size.y + ui.spacing().item_spacing.y) * n.clamp(1, 4) as f32;
    let selected = editor.ui.selected_notify.filter(|i| *i < n);
    let events = list_rows(
        ui,
        "anim-clip-notifies",
        &mut editor.ui.notify_keys,
        n,
        height,
        selected,
        &name,
    );
    for event in events {
        match event {
            RowKey::Activate(i) => editor.ui.selected_notify = Some(i),
            RowKey::Delete(i) => {
                let imported = (**imported).clone();
                editor.ui.selected_notify = None;
                pending.push(PendingEdit::new("delete notify", None, move |d| {
                    let edits = clip_edits_mut(d, &imported);
                    if i < edits.notifies.len() {
                        edits.notifies.remove(i);
                        Ok(())
                    } else {
                        Err(format!("no notify {i}"))
                    }
                }));
            }
            RowKey::Palette(i) => {
                let Some(m) = clip.notifies.get(i) else { continue };
                let t = (m.time + NOTIFY_STEP).clamp(0.0, clip.duration);
                let imported = (**imported).clone();
                editor.ui.selected_notify = Some(n);
                pending.push(PendingEdit::new("add notify", None, move |d| {
                    clip_edits_mut(d, &imported).notifies.push(Notify {
                        time: t,
                        name: "Notify".into(),
                        payload: None,
                    });
                    Ok(())
                }));
            }
            RowKey::Arrow { row, dx, coarse, .. } if dx != 0 => {
                let Some(m) = clip.notifies.get(row) else { continue };
                let step = NOTIFY_STEP * if coarse { 10.0 } else { 1.0 };
                let t = (m.time + dx as f32 * step).clamp(0.0, clip.duration);
                editor.ui.selected_notify = Some(row);
                pending.push(move_notify_edit(imported, row, t));
            }
            RowKey::Arrow { .. } | RowKey::Connect(_) => {}
        }
    }
}

fn notify_inspector(
    ui: &mut egui::Ui,
    editor: &mut AnimEditor,
    clip: &AnimationClip,
    imported: &std::sync::Arc<AnimationClip>,
    pending: &mut Vec<PendingEdit>,
) {
    ui.horizontal(|ui| {
        ui.strong("Notifies");
        ui.weak("double-click the timeline to add · drag markers to move");
    });
    let Some(i) = editor.ui.selected_notify.filter(|i| *i < clip.notifies.len()) else {
        for n in &clip.notifies {
            ui.weak(format!("{:.3}s  {}", n.time, n.name));
        }
        return;
    };
    let n = clip.notifies[i].clone();
    let mut name = n.name.clone();
    let mut time = n.time;
    let mut payload = n.payload.clone().unwrap_or_default();
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("Name");
        changed |= ui
            .add(egui::TextEdit::singleline(&mut name).desired_width(120.0))
            .changed();
        ui.label("Time");
        changed |= ui
            .add(
                egui::DragValue::new(&mut time)
                    .speed(0.005)
                    .range(0.0..=clip.duration),
            )
            .changed();
        ui.label("Payload (RON)");
        changed |= ui
            .add(egui::TextEdit::singleline(&mut payload).desired_width(140.0))
            .changed();
        if ui.button("Delete").clicked() {
            let imported = (**imported).clone();
            editor.ui.selected_notify = None;
            pending.push(PendingEdit::new("delete notify", None, move |d| {
                let edits = clip_edits_mut(d, &imported);
                if i < edits.notifies.len() {
                    edits.notifies.remove(i);
                    Ok(())
                } else {
                    Err(format!("no notify {i}"))
                }
            }));
        }
    });
    if changed {
        let imported = (**imported).clone();
        pending.push(PendingEdit::new(
            "edit notify",
            Some(format!("notify-edit:{}:{i}", imported.name)),
            move |d| {
                let edits = clip_edits_mut(d, &imported);
                let n = edits
                    .notifies
                    .get_mut(i)
                    .ok_or_else(|| format!("no notify {i}"))?;
                n.name = name;
                n.time = time;
                n.payload = (!payload.is_empty()).then_some(payload);
                Ok(())
            },
        ));
    }
}

fn curves_ui(
    ui: &mut egui::Ui,
    editor: &mut AnimEditor,
    clip: &AnimationClip,
    imported: &std::sync::Arc<AnimationClip>,
    pending: &mut Vec<PendingEdit>,
) {
    let edits = editor.docs.edits.clip(&clip.name).cloned().unwrap_or_default();
    // Every curve and property track of the clip, in palette order (the multi-curve view, P-208).
    let all: Vec<(bool, String, Curve)> = edits
        .curves
        .iter()
        .map(|c| (false, c.name.clone(), c.curve.clone()))
        .chain(
            edits
                .property_tracks
                .iter()
                .map(|t| (true, t.path.clone(), t.curve.clone())),
        )
        .collect();
    let tokens = crate::theme::ui_tokens(ui.ctx());
    ui.horizontal_wrapped(|ui| {
        ui.strong("Curves");
        for (i, (property, name, _)) in all.iter().enumerate() {
            let key = multi_curve_key(*property, name);
            let color = super::curve_view::curve_color(&tokens, i);
            let open = editor.ui.curve.as_ref() == Some(&(*property, name.clone()));
            let label = egui::RichText::new(if *property {
                format!("{name} (property)")
            } else {
                name.clone()
            })
            .color(color);
            if ui.selectable_label(open, label).clicked() {
                editor.ui.curve = Some((*property, name.clone()));
                editor.ui.selected_key = None;
            }
            let mut visible = editor.ui.curves.visible(&key);
            let r = ui.checkbox(&mut visible, "");
            let r = crate::a11y_audit::name(r, egui::WidgetType::Checkbox, &format!("Show {name}"));
            if r.on_hover_text("Show this curve").changed()
                && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new(("anim-curve-show", &key)))
            {
                editor.ui.curves.set_visible(&key, visible);
            }
            let mut locked = editor.ui.curves.is_locked(&key);
            let r = ui.checkbox(&mut locked, "lock");
            let r = crate::a11y_audit::name(r, egui::WidgetType::Checkbox, &format!("Lock {name}"));
            if r.changed()
                && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new(("anim-curve-lock", &key)))
            {
                editor.ui.curves.set_locked(&key, locked);
            }
        }
        if ui
            .add_enabled(!all.is_empty(), egui::Button::new("Frame all"))
            .clicked()
        {
            let points: Vec<Vec<(f32, f32)>> = all
                .iter()
                .filter(|(p, n, _)| editor.ui.curves.visible(&multi_curve_key(*p, n)))
                .map(|(_, _, c)| super::curve_view::key_points(c))
                .collect();
            let b = super::curve_view::frame_all(points.iter().map(Vec::as_slice));
            editor.ui.curve_view.zoom_to_fit(b, 0.1);
        }
        let new_id = egui::Id::new("anim-new-curve");
        let mut name: String = ui.data(|d| d.get_temp(new_id)).unwrap_or_default();
        ui.add(
            egui::TextEdit::singleline(&mut name)
                .hint_text("name or Comp.path")
                .desired_width(130.0),
        );
        for (property, label) in [(false, "+ curve"), (true, "+ property track")] {
            if ui.button(label).clicked() && !name.is_empty() {
                let imported = (**imported).clone();
                let n = name.clone();
                let duration = clip.duration;
                editor.ui.curve = Some((property, n.clone()));
                pending.push(PendingEdit::new(format!("add {label}"), None, move |d| {
                    let curve = Curve::from(vec![
                        CurveKey::new(0.0, 0.0, CurveKeyKind::Linear),
                        CurveKey::new(duration, 1.0, CurveKeyKind::Linear),
                    ]);
                    let edits = clip_edits_mut(d, &imported);
                    if property {
                        edits.property_tracks.retain(|t| t.path != n);
                        edits.property_tracks.push(EditPropertyTrack { path: n, curve });
                    } else {
                        edits.curves.retain(|c| c.name != n);
                        edits.curves.push(EditCurve { name: n, curve });
                    }
                    Ok(())
                }));
            }
        }
        ui.data_mut(|d| d.insert_temp(new_id, name));
    });
    let Some((property, name)) = editor.ui.curve.clone() else {
        ui.weak("Add or pick a curve to edit it.");
        return;
    };
    let curve = if property {
        edits
            .property_tracks
            .iter()
            .find(|t| t.path == name)
            .map(|t| t.curve.clone())
    } else {
        edits
            .curves
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.curve.clone())
    };
    let Some(curve) = curve else {
        return;
    };
    ui.horizontal(|ui| {
        ui.weak(format!(
            "{name}: {} keys · value at playhead {:.3} · double-click adds · right-click a key for its menu",
            curve.keys.len(),
            curve.value_at(editor.ui.scrub)
        ));
        if ui.small_button("Delete curve").clicked() {
            let imported = (**imported).clone();
            let n = name.clone();
            editor.ui.curve = None;
            pending.push(PendingEdit::new("delete curve", None, move |d| {
                let edits = clip_edits_mut(d, &imported);
                edits.curves.retain(|c| property || c.name != n);
                edits.property_tracks.retain(|t| !property || t.path != n);
                Ok(())
            }));
        }
    });
    let id = egui::Id::new(("anim-curve", property, name.clone()));
    let mut selected = editor.ui.selected_key;
    let key = multi_curve_key(property, &name);
    let action = curve_editor(
        ui,
        id,
        &curve,
        &mut editor.ui.curve_view,
        &mut selected,
        Some(editor.ui.scrub),
    );
    // The other visible curves, drawn over the grid in their palette colors.
    let view = &editor.ui.curve_view;
    let painter = ui.painter_at(view.rect);
    for (i, (p, n, c)) in all.iter().enumerate() {
        let other = multi_curve_key(*p, n);
        if other == key || !editor.ui.curves.visible(&other) {
            continue;
        }
        super::curve_view::paint_background_curve(
            &painter,
            view,
            &|t| Some(c.value_at(t)),
            &super::curve_view::key_points(c),
            super::curve_view::curve_color(&tokens, i),
            editor.ui.curves.is_locked(&other),
        );
    }
    // A locked curve scrubs but does not edit.
    let action = editor.ui.curves.filter(&key, action);
    editor.ui.selected_key = selected;
    match action {
        Some(CurveAction::Scrub(t)) => editor.ui.scrub = t.clamp(0.0, clip.duration),
        Some(action) => {
            let imported = (**imported).clone();
            let merge = match &action {
                CurveAction::Move(i, ..) | CurveAction::SetKind(i, _) => Some(format!("curve:{name}:{i}")),
                _ => None,
            };
            pending.push(PendingEdit::new("edit curve", merge, move |d| {
                let edits = clip_edits_mut(d, &imported);
                let curve = if property {
                    edits
                        .property_tracks
                        .iter_mut()
                        .find(|t| t.path == name)
                        .map(|t| &mut t.curve)
                } else {
                    edits
                        .curves
                        .iter_mut()
                        .find(|c| c.name == name)
                        .map(|c| &mut c.curve)
                }
                .ok_or_else(|| format!("no curve {name}"))?;
                apply_action(curve, &action).map(|_| ())
            }));
        }
        None => {}
    }
}

/// The multi-curve key of a clip curve (`curve:<name>`) or property track (`property:<path>`).
#[must_use]
pub fn multi_curve_key(property: bool, name: &str) -> String {
    if property {
        format!("property:{name}")
    } else {
        format!("curve:{name}")
    }
}

fn root_motion_plot(ui: &mut egui::Ui, clip: &AnimationClip, scrub: f32) {
    let mode = clip.root_motion.as_ref().map(|r| r.mode);
    ui.strong("Root motion");
    let Some(mode) = mode.filter(|m| *m != RootMotionMode::None) else {
        ui.weak("none (the clip plays in place)");
        return;
    };
    let path = root_trajectory(clip, 60);
    let travelled = path_length(&path);
    ui.monospace(format!(
        "{mode:?}: {travelled:.2} m travelled over {:.2} s ({:.2} m/s)",
        clip.duration,
        travelled / clip.duration.max(1e-3)
    ));
    let size = egui::vec2(ui.available_width(), ui.available_height().max(60.0));
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_gray(26));
    let extent = path
        .iter()
        .map(|p| p.x.abs().max(p.z.abs()))
        .fold(0.1f32, f32::max)
        * 1.15;
    let mut view = View2d {
        rect,
        ..View2d::default()
    };
    view.zoom_to_fit([-extent, -extent, extent, extent], 0.0);
    let s = view.scale.x.min(view.scale.y);
    view.scale = egui::vec2(s, s);
    // Top-down: x right, z up the plot.
    let to = |p: Vec3| view.to_screen(egui::vec2(p.x, p.z));
    painter.add(egui::Shape::line(
        path.iter().map(|p| to(*p)).collect(),
        egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 200, 140)),
    ));
    painter.circle_filled(to(Vec3::ZERO), 3.0, egui::Color32::WHITE);
    let at = (scrub / clip.duration.max(1e-3) * (path.len().saturating_sub(1)) as f32).round() as usize;
    if let Some(p) = path.get(at) {
        painter.circle_filled(to(*p), 4.0, crate::theme::ui_tokens(painter.ctx()).accents.alert);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_length_counts_out_and_back_travel() {
        let path = [Vec3::ZERO, Vec3::new(2.0, 0.0, 0.0), Vec3::ZERO];
        assert!((path_length(&path) - 4.0).abs() < 1e-6);
        assert_eq!(path_length(&[]), 0.0);
        assert_eq!(path_length(&[Vec3::X]), 0.0);
    }

    #[test]
    fn trajectory_and_first_edit_keep_imported_notifies() {
        let rig = PreviewRig::demo();
        let walk = rig.clip("Walk").expect("walk").clone();
        let path = root_trajectory(&walk, 10);
        assert_eq!(path.len(), 11);
        assert!((path[10].z - 1.4).abs() < 1e-3, "{:?}", path[10]);
        let mut docs = AnimDocs::default();
        clip_edits_mut(&mut docs, &walk).notifies.push(Notify {
            time: 0.5,
            name: "Extra".into(),
            payload: None,
        });
        assert_eq!(
            docs.edits.clip("Walk").map(|c| c.notifies.len()),
            Some(3),
            "2 imported + 1"
        );
    }

    /// FU-5: the notify list selects a notify (Enter), moves it one step (Right: the marker
    /// drag's edit) and deletes the other (Delete: the inspector's) with keys only; the clip edits
    /// equal the ones the mouse's drag and Delete build.
    #[test]
    fn the_notify_list_edits_by_keyboard_what_the_mouse_edits() {
        use crate::canvas_keys::test_input::{frame, key, labels_of, tab_to};
        use egui::{Key, Modifiers};
        const SIZE: [f32; 2] = [1100.0, 900.0];
        let rig = PreviewRig::demo();
        let walk = rig.clip("Walk").expect("walk").clone();
        assert_eq!(walk.notifies.len(), 2, "the demo clip's notifies");
        let world_of = || {
            let mut world = World::new();
            world.init_resource::<crate::command::UndoStack>();
            let mut editor = AnimEditor::default();
            editor.ui.clip = Some("Walk".into());
            world.insert_resource(editor);
            world
        };
        let mut mouse = world_of();
        let t = (walk.notifies[1].time + NOTIFY_STEP).clamp(0.0, walk.duration);
        apply_pending(&mut mouse, vec![move_notify_edit(&walk, 1, t)]);
        let w = walk.clone();
        apply_pending(
            &mut mouse,
            vec![PendingEdit::new("delete notify", None, move |d| {
                clip_edits_mut(d, &w).notifies.remove(0);
                Ok(())
            })],
        );

        let mut world = world_of();
        let ctx = egui::Context::default();
        let first = frame(&ctx, SIZE, vec![], |ui| anim_clip_panel(ui, &mut world, None));
        assert!(
            labels_of(&first, egui::accesskit::Role::Canvas)
                .contains(&"Clip timeline canvas, 2 notifies".to_string()),
            "{:?}",
            labels_of(&first, egui::accesskit::Role::Canvas)
        );
        let second = format!(
            "Notify {} at {:.3} s",
            walk.notifies[1].name, walk.notifies[1].time
        );
        tab_to(&ctx, SIZE, &second, false, 400, |ui| {
            anim_clip_panel(ui, &mut world, None)
        });
        frame(&ctx, SIZE, vec![key(Key::Enter, Modifiers::NONE)], |ui| {
            anim_clip_panel(ui, &mut world, None)
        });
        assert_eq!(
            world.resource::<AnimEditor>().ui.selected_notify,
            Some(1),
            "Enter selects"
        );
        frame(&ctx, SIZE, vec![key(Key::ArrowRight, Modifiers::NONE)], |ui| {
            anim_clip_panel(ui, &mut world, None)
        });
        frame(&ctx, SIZE, vec![key(Key::ArrowUp, Modifiers::NONE)], |ui| {
            anim_clip_panel(ui, &mut world, None)
        });
        frame(&ctx, SIZE, vec![], |ui| anim_clip_panel(ui, &mut world, None));
        frame(&ctx, SIZE, vec![key(Key::Delete, Modifiers::NONE)], |ui| {
            anim_clip_panel(ui, &mut world, None)
        });
        assert_eq!(
            format!("{:?}", world.resource::<AnimEditor>().docs.edits),
            format!("{:?}", mouse.resource::<AnimEditor>().docs.edits)
        );
    }

    fn panel_world() -> World {
        let mut world = World::new();
        world.init_resource::<AnimEditor>();
        world.init_resource::<crate::command::UndoStack>();
        let mut editor = world.resource_mut::<AnimEditor>();
        let editor = &mut *editor;
        let walk = editor.rig_or_demo().clip("Walk").expect("walk").clone();
        editor.ui.clip = Some("Walk".into());
        let edits = clip_edits_mut(&mut editor.docs, &walk);
        for (name, v) in [("Lean", 1.0), ("Bob", -1.0)] {
            edits.curves.push(EditCurve {
                name: name.into(),
                curve: Curve::from(vec![
                    CurveKey::new(0.0, 0.0, CurveKeyKind::Linear),
                    CurveKey::new(1.0, v, CurveKeyKind::Linear),
                ]),
            });
        }
        editor.ui.curve = Some((false, "Lean".into()));
        world
    }

    /// P-207, P-208, P-211, P-213, P-216 keyboard path: every new clip-panel control is reached by
    /// Tab and works from the keyboard (speed slider, preview range end, onion skins, a curve's
    /// visibility and lock, the clip search).
    #[test]
    fn the_new_clip_panel_controls_work_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to, text};
        let ctx = egui::Context::default();
        let size = [1400.0, 900.0];
        let mut world = panel_world();
        let none = egui::Modifiers::NONE;
        frame(&ctx, size, vec![], |ui| anim_clip_panel(ui, &mut world, None));
        let update = frame(&ctx, size, vec![], |ui| anim_clip_panel(ui, &mut world, None));
        let names: Vec<String> = crate::a11y_audit::widget_names(&update)
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        for want in [
            "Preview speed",
            "Preview range end",
            "Show Bob",
            "Lock Lean",
            "Clip",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} not in {names:?}");
        }
        let speed = world.resource::<AnimEditor>().ui.clip_preview.speed;
        tab_to(&ctx, size, "Preview speed", false, 80, |ui| {
            anim_clip_panel(ui, &mut world, None);
        });
        frame(&ctx, size, vec![key(egui::Key::ArrowRight, none)], |ui| {
            anim_clip_panel(ui, &mut world, None);
        });
        assert!(world.resource::<AnimEditor>().ui.clip_preview.speed > speed);
        tab_to(&ctx, size, "Preview range end", false, 80, |ui| {
            anim_clip_panel(ui, &mut world, None);
        });
        frame(&ctx, size, vec![key(egui::Key::ArrowDown, none)], |ui| {
            anim_clip_panel(ui, &mut world, None);
        });
        assert!(
            world.resource::<AnimEditor>().ui.clip_preview.range.is_some(),
            "the end moved: the preview range is trimmed"
        );
        for name in ["Onion skins", "Show Bob", "Lock Lean"] {
            tab_to(&ctx, size, name, false, 80, |ui| {
                anim_clip_panel(ui, &mut world, None)
            });
            frame(&ctx, size, vec![key(egui::Key::Space, none)], |ui| {
                anim_clip_panel(ui, &mut world, None);
            });
            frame(&ctx, size, vec![], |ui| anim_clip_panel(ui, &mut world, None));
        }
        {
            let ui = &world.resource::<AnimEditor>().ui;
            assert!(ui.clip_preview.onion.enabled);
            assert!(!ui.curves.visible("curve:Bob"));
            assert!(ui.curves.is_locked("curve:Lean"));
        }
        // The clip picker: open it from the keyboard, Tab into the search, type.
        tab_to(&ctx, size, "Clip", true, 80, |ui| {
            anim_clip_panel(ui, &mut world, None)
        });
        frame(&ctx, size, vec![key(egui::Key::Enter, none)], |ui| {
            anim_clip_panel(ui, &mut world, None);
        });
        frame(&ctx, size, vec![], |ui| anim_clip_panel(ui, &mut world, None));
        tab_to(&ctx, size, "Search clips", false, 10, |ui| {
            anim_clip_panel(ui, &mut world, None);
        });
        frame(&ctx, size, vec![text("ru")], |ui| {
            anim_clip_panel(ui, &mut world, None)
        });
        assert_eq!(world.resource::<AnimEditor>().ui.clip_list.filter, "ru");
    }
}
