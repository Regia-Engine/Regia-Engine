// SPDX-License-Identifier: MPL-2.0
//! Default positions of the editor's floating tool windows (ADR-8589; bug 15 of the 2026-11-16
//! all-host smoke run).
//!
//! egui places a window without a position at the screen's top-left corner plus a margin, and
//! windows opened in the same pass all get that one spot: the Frame Debugger, Rendering Debug,
//! Diagnostics, Profiler, Asset Jobs, Revision Control, Screenshot and About windows opened
//! stacked exactly on top of each other and over the menu bar and toolbars. Each of them now asks
//! [`default_pos`] for its first position: windows cascade down and right in the order they first
//! open, inside the work area (the dock's rectangle below the toolbars and above the status bar,
//! which the shell records every pass with [`set_work_area`]). A window keeps its slot for the
//! session, and once it is moved or resized egui's own window memory (keyed by the window id)
//! takes over: the default applies only to a window egui has no position for.

use egui::{Context, Id, Pos2, Rect, Vec2};

/// Gap between the work area's top-left corner and the first window, points.
pub const MARGIN: f32 = 24.0;
/// Offset from one window's default position to the next one's, points.
pub const CASCADE_STEP: Vec2 = Vec2::new(32.0, 32.0);
/// Offset from one cascade column to the next when a column reaches the work area's bottom.
pub const COLUMN_STEP: f32 = 96.0;
/// The part of a window that stays inside the work area at its default position, points.
pub const MIN_VISIBLE: Vec2 = Vec2::new(200.0, 120.0);

/// What the cascade remembers in the egui context: the work area and the windows in the order
/// they first asked for a position.
#[derive(Clone, Default)]
struct Placement {
    work_area: Option<Rect>,
    order: Vec<Id>,
}

fn key() -> Id {
    Id::new("regia_editor::tool_windows::placement")
}

/// Records the work area tool windows open in (the dock's rectangle, points). The shell calls it
/// every pass; a window drawn before the dock in a pass uses the previous pass's area.
pub fn set_work_area(ctx: &Context, rect: Rect) {
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Placement>(key()).work_area = Some(rect));
}

/// The work area [`set_work_area`] recorded, or the whole screen before the first record.
#[must_use]
pub fn work_area(ctx: &Context) -> Rect {
    ctx.data(|d| d.get_temp::<Placement>(key()))
        .and_then(|p| p.work_area)
        .unwrap_or_else(|| ctx.content_rect())
}

/// The cascade slot of the window titled `title`: its index in first-open order (assigned on the
/// first call, kept for the session).
#[must_use]
pub fn slot(ctx: &Context, title: &str) -> usize {
    let id = Id::new(title);
    ctx.data_mut(|d| {
        let placement = d.get_temp_mut_or_default::<Placement>(key());
        match placement.order.iter().position(|&known| known == id) {
            Some(slot) => slot,
            None => {
                placement.order.push(id);
                placement.order.len() - 1
            }
        }
    })
}

/// The default top-left corner of the window titled `title` (pass it to
/// `egui::Window::default_pos`): its [`slot`] in the cascade inside the [`work_area`].
#[must_use]
pub fn default_pos(ctx: &Context, title: &str) -> Pos2 {
    let slot = slot(ctx, title);
    cascade_pos(work_area(ctx), slot)
}

/// The top-left corner of cascade slot `slot` in `area`: slot 0 at the corner plus [`MARGIN`],
/// each next slot one [`CASCADE_STEP`] further; when a slot would leave less than [`MIN_VISIBLE`]
/// of a window inside the area, a new column starts [`COLUMN_STEP`] to the right of the last one's
/// top. Every position is clamped into the area, so a window always opens below the toolbars.
#[must_use]
pub fn cascade_pos(area: Rect, slot: usize) -> Pos2 {
    let start = area.min + Vec2::splat(MARGIN);
    let room_y = area.max.y - MIN_VISIBLE.y - start.y;
    let per_column = if room_y >= CASCADE_STEP.y {
        (room_y / CASCADE_STEP.y).floor() as usize + 1
    } else {
        1
    };
    let row = slot % per_column;
    let column = slot / per_column;
    let x = start.x + CASCADE_STEP.x * row as f32 + COLUMN_STEP * column as f32;
    let y = start.y + CASCADE_STEP.y * row as f32;
    Pos2::new(
        x.min(area.max.x - MIN_VISIBLE.x).max(area.min.x),
        y.min(area.max.y - MIN_VISIBLE.y).max(area.min.y),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_cascade_inside_the_area_and_never_repeat() {
        let area = Rect::from_min_max(Pos2::new(0.0, 60.0), Pos2::new(1280.0, 760.0));
        let mut seen: Vec<Pos2> = Vec::new();
        for slot in 0..40 {
            let pos = cascade_pos(area, slot);
            assert!(area.contains(pos), "slot {slot} at {pos:?} is outside {area:?}");
            assert!(
                pos.y >= area.min.y + MARGIN,
                "slot {slot} at {pos:?} is under the toolbar"
            );
            assert!(
                seen.iter().all(|p| p.distance(pos) >= 1.0),
                "slot {slot} at {pos:?} repeats an earlier origin"
            );
            seen.push(pos);
        }
        // A tiny area still keeps every position inside it.
        let tiny = Rect::from_min_size(Pos2::new(10.0, 40.0), Vec2::new(150.0, 90.0));
        for slot in 0..8 {
            assert!(tiny.contains(cascade_pos(tiny, slot)));
        }
    }

    /// The id of the area `egui::Window::new(title)` draws in, computed as egui 0.36 does: from the
    /// title's atoms' text (`Option<Cow<str>>`), not from the bare string, so `Id::new(title)`
    /// names no window (lane v06-editor-b-reds).
    fn window_area_id(title: &str) -> Id {
        Id::new(egui::IntoAtoms::into_atoms(title).text())
    }

    /// The eight tool windows of the all-host smoke run's bug 15, opened together in a headless
    /// editor shell: no two open at one origin, and none overlaps the menu bar and toolbars (the
    /// screen above the dock's work area).
    #[test]
    fn the_eight_tool_windows_open_apart_and_below_the_toolbars() {
        use regia_ecs::prelude::*;
        let dir = regia_test_utils::unique_temp_dir("tool-windows-cascade");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"W\"\nid = \"com.example.w\"\n",
        )
        .expect("manifest");
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = crate::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        let mut shell = crate::shell::EditorShell::new(None, Some(dir.clone()));
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0));
        let mut frame = 0_u32;
        let mut pass = |shell: &mut crate::shell::EditorShell, world: &mut World| {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                time: Some(f64::from(frame) / 60.0),
                ..Default::default()
            };
            frame += 1;
            let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, world, None));
            output.textures_delta.clear();
        };
        for _ in 0..2 {
            pass(&mut shell, app.world_mut());
        }
        const COMMANDS: [&str; 7] = [
            "view.toggle_frame_debugger",
            "view.toggle_rendering_debug",
            "view.toggle_diagnostics",
            "view.toggle_profiler",
            "window.asset_jobs",
            "view.toggle_revision_control",
            "file.screenshot",
        ];
        for id in COMMANDS {
            assert!(
                crate::command_palette::run(app.world_mut(), id).is_some(),
                "command {id} exists"
            );
        }
        crate::about::open_about(app.world_mut());
        for _ in 0..4 {
            pass(&mut shell, app.world_mut());
        }
        const TITLES: [&str; 8] = [
            "Frame Debugger",
            "Rendering Debug",
            "Diagnostics",
            "Profiler",
            "Asset Jobs",
            "Revision Control",
            "Screenshot",
            "About Regia Engine",
        ];
        let work = work_area(&ctx);
        assert!(
            work.min.y > screen.min.y,
            "the shell records the dock area below the toolbars: {work:?}"
        );
        let toolbars = Rect::from_min_max(screen.min, Pos2::new(screen.max.x, work.min.y));
        let mut rects: Vec<(&str, Rect)> = Vec::new();
        for title in TITLES {
            let rect = ctx
                .memory(|m| m.area_rect(window_area_id(title)))
                .unwrap_or_else(|| panic!("the {title} window is open"));
            assert!(
                rect.min.y >= toolbars.max.y,
                "{title} at {rect:?} overlaps the menu bar and toolbars {toolbars:?}"
            );
            for (other, seen) in &rects {
                assert!(
                    seen.min.distance(rect.min) >= 1.0,
                    "{title} and {other} share the origin {:?}",
                    rect.min
                );
            }
            rects.push((title, rect));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_title_keeps_its_slot() {
        let ctx = Context::default();
        assert_eq!(slot(&ctx, "Profiler"), 0);
        assert_eq!(slot(&ctx, "About Regia Engine"), 1);
        assert_eq!(slot(&ctx, "Profiler"), 0, "asked again: the same slot");
        let area = Rect::from_min_max(Pos2::new(0.0, 80.0), Pos2::new(1000.0, 700.0));
        set_work_area(&ctx, area);
        assert_eq!(work_area(&ctx), area);
        assert_eq!(default_pos(&ctx, "About Regia Engine"), cascade_pos(area, 1));
    }
}
