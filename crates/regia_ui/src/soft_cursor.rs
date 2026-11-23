// SPDX-License-Identifier: MPL-2.0
//! Software cursor for gamepads over pointer-only UI (ADR-273 I10, ADR-1842).
//!
//! When enabled, a stick moves a cursor over the window and the right trigger acts as the primary
//! button, so menus built for a mouse work with a pad. [`step_cursor`] is the pure movement rule
//! (dead zone, a squared response curve for fine control near the center, clamping to the window);
//! [`drive_soft_cursor`] feeds the result into [`UiPointer`] so hover, press, drag and click run
//! through the normal pointer path. The cursor is drawn by the layout-overlay pass as a small
//! cross on top of the sorted draw list, and a move, an activate or a mouse takeover refreshes
//! that tail without rebuilding widget quads. A real mouse movement hides it again.
//!
//! The agent surface is `ui.soft_cursor` (`book/agents/ui-soft-cursor.md`).

use regia_app::prelude::*;
use regia_platform::{ButtonState, GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged};

use crate::color::UiColor;
use crate::draw::UiQuad;
use crate::interaction::UiPointer;
use crate::scale::UiScales;

/// Half the length of the drawn cross, in physical px.
const CROSS_HALF: f32 = 9.0;
/// Thickness of the drawn cross, in physical px.
const CROSS_THICK: f32 = 3.0;
/// Draw depth: above the debug overlay.
const CURSOR_Z: f32 = 2.0e6;

/// Which stick moves the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorStick {
    /// The left stick.
    Left,
    /// The right stick.
    #[default]
    Right,
}

/// The software cursor's settings and state.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct SoftCursor {
    /// Master switch.
    pub enabled: bool,
    /// Position in physical px.
    pub pos: [f32; 2],
    /// Full-deflection speed in physical px per second.
    pub speed: f32,
    /// Stick dead zone, `0..1`.
    pub dead_zone: f32,
    /// Which stick drives it.
    pub stick: CursorStick,
    /// Whether the cursor is shown and driving the pointer (set by stick movement, cleared by a
    /// real mouse move).
    pub active: bool,
    stick_value: [f32; 2],
    trigger_down: bool,
}

impl Default for SoftCursor {
    fn default() -> Self {
        Self {
            enabled: false,
            pos: [0.0; 2],
            speed: 900.0,
            dead_zone: 0.15,
            stick: CursorStick::Right,
            active: false,
            stick_value: [0.0; 2],
            trigger_down: false,
        }
    }
}

/// Moves `pos` for a stick deflection (`x` right, `y` up, each `-1..1`) over `dt` seconds. The
/// radial dead zone is rescaled so movement starts smoothly at the edge of the dead zone, the
/// magnitude is squared for fine control, and the result is clamped to `0..=bounds`.
pub fn step_cursor(
    pos: [f32; 2],
    stick: [f32; 2],
    dt: f32,
    speed: f32,
    dead_zone: f32,
    bounds: [f32; 2],
) -> [f32; 2] {
    let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
    let (sx, sy) = (
        finite(stick[0]).clamp(-1.0, 1.0),
        finite(stick[1]).clamp(-1.0, 1.0),
    );
    let dz = finite(dead_zone).clamp(0.0, 0.95);
    let len = (sx * sx + sy * sy).sqrt();
    let mag = len.min(1.0);
    let dt = finite(dt).max(0.0);
    let mut out = pos;
    if mag > dz {
        let scaled = (mag - dz) / (1.0 - dz);
        let response = scaled * scaled;
        // Direction from the stick; stick y is up, screen y is down.
        let (dx, dy) = (sx / len, -sy / len);
        out[0] += dx * response * finite(speed).max(0.0) * dt;
        out[1] += dy * response * finite(speed).max(0.0) * dt;
    }
    [
        finite(out[0]).clamp(0.0, finite(bounds[0]).max(0.0)),
        finite(out[1]).clamp(0.0, finite(bounds[1]).max(0.0)),
    ]
}

/// `PreUpdate`, after the platform pointer: moves the cursor and presses the primary button.
pub fn drive_soft_cursor(
    mut cursor: ResMut<SoftCursor>,
    mut pointer: ResMut<UiPointer>,
    mut axes: MessageReader<GamepadAxisChanged>,
    mut buttons: MessageReader<GamepadButtonChanged>,
    mut mouse: MessageReader<regia_platform::CursorMoved>,
    scales: Res<UiScales>,
    real: Res<regia_app::RealTime>,
) {
    let _span = tracing::trace_span!("ui_soft_cursor").entered();
    if !cursor.enabled {
        axes.clear();
        buttons.clear();
        mouse.clear();
        if cursor.active {
            cursor.active = false;
        }
        return;
    }
    let (xa, ya) = match cursor.stick {
        CursorStick::Left => (GamepadAxis::LeftStickX, GamepadAxis::LeftStickY),
        CursorStick::Right => (GamepadAxis::RightStickX, GamepadAxis::RightStickY),
    };
    for e in axes.read() {
        if e.axis == xa {
            cursor.stick_value[0] = e.value;
        } else if e.axis == ya {
            cursor.stick_value[1] = e.value;
        }
    }
    let mut trigger_changed = false;
    for e in buttons.read() {
        if e.button == GamepadButton::RightTrigger {
            let down = e.state == ButtonState::Pressed;
            if down != cursor.trigger_down {
                cursor.trigger_down = down;
                trigger_changed = true;
            }
        }
    }
    if mouse.read().count() > 0 && cursor.active {
        cursor.active = false;
        if cursor.trigger_down {
            cursor.trigger_down = false;
            pointer.primary_down = false;
        }
        return;
    }
    let moving = cursor.stick_value[0].abs().max(cursor.stick_value[1].abs()) > cursor.dead_zone;
    if !cursor.active && !moving && !trigger_changed {
        return;
    }
    if !cursor.active {
        cursor.active = true;
        // Start where the real pointer was, so the cursor does not jump.
        if let Some(p) = pointer.cursor {
            cursor.pos = p;
        } else {
            cursor.pos = [scales.window_size[0] * 0.5, scales.window_size[1] * 0.5];
        }
    }
    let dt = real.delta().as_secs_f32();
    cursor.pos = step_cursor(
        cursor.pos,
        cursor.stick_value,
        dt,
        cursor.speed,
        cursor.dead_zone,
        scales.window_size,
    );
    pointer.cursor = Some(cursor.pos);
    if trigger_changed {
        pointer.primary_down = cursor.trigger_down;
    }
}

/// The two bars of the drawn cross, or nothing while the cursor is inactive. Called by the
/// overlay pass of the extractor.
pub fn cursor_quads(world: &World) -> Vec<UiQuad> {
    let Some(c) = world.get_resource::<SoftCursor>() else {
        return Vec::new();
    };
    if !c.enabled || !c.active {
        return Vec::new();
    }
    let [x, y] = c.pos;
    let color = UiColor([1.0, 1.0, 1.0, 0.95]);
    let outline = UiColor([0.0, 0.0, 0.0, 0.8]);
    let mut out = Vec::new();
    for (pad, col) in [(1.5, outline), (0.0, color)] {
        let half = CROSS_HALF + pad;
        let thick = CROSS_THICK + 2.0 * pad;
        for (pos, size) in [
            ([x - half, y - thick * 0.5], [half * 2.0, thick]),
            ([x - thick * 0.5, y - half], [thick, half * 2.0]),
        ] {
            out.push(UiQuad {
                z: CURSOR_Z,
                ..UiQuad::fill(pos, size, col)
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: [f32; 2] = [1920.0, 1080.0];

    #[test]
    fn dead_zone_holds_still() {
        let p = [100.0, 100.0];
        assert_eq!(step_cursor(p, [0.1, 0.1], 1.0, 900.0, 0.15, WIN), p);
        assert_eq!(step_cursor(p, [0.0, 0.0], 1.0, 900.0, 0.15, WIN), p);
    }

    #[test]
    fn full_deflection_moves_at_full_speed_and_y_is_up() {
        let p = step_cursor([100.0, 100.0], [1.0, 0.0], 0.5, 800.0, 0.0, WIN);
        assert!((p[0] - 500.0).abs() < 1e-3 && p[1] == 100.0, "{p:?}");
        let p = step_cursor([100.0, 100.0], [0.0, 1.0], 0.5, 800.0, 0.0, WIN);
        assert_eq!(p[1], 0.0, "up clamps at the top: {p:?}");
        let p = step_cursor([100.0, 100.0], [0.0, -1.0], 0.5, 800.0, 0.0, WIN);
        assert!((p[1] - 500.0).abs() < 1e-3, "{p:?}");
    }

    #[test]
    fn response_is_squared_past_the_dead_zone() {
        let half = step_cursor([0.0, 500.0], [0.575, 0.0], 1.0, 1000.0, 0.15, WIN)[0];
        // (0.575 - 0.15) / 0.85 = 0.5, squared 0.25.
        assert!((half - 250.0).abs() < 0.5, "{half}");
    }

    #[test]
    fn clamps_to_the_window_and_ignores_garbage() {
        let p = step_cursor([1900.0, 1070.0], [1.0, -1.0], 10.0, 1000.0, 0.0, WIN);
        assert_eq!(p, [1920.0, 1080.0]);
        let p = step_cursor(
            [5.0, 5.0],
            [f32::NAN, f32::INFINITY],
            f32::NAN,
            f32::NAN,
            f32::NAN,
            WIN,
        );
        assert_eq!(p, [5.0, 5.0]);
        assert_eq!(
            step_cursor([5.0, 5.0], [1.0, 0.0], 1.0, 100.0, 0.0, [0.0, 0.0]),
            [0.0, 0.0]
        );
    }

    #[test]
    fn diagonal_is_not_faster_than_straight() {
        let straight = step_cursor([500.0, 500.0], [1.0, 0.0], 1.0, 100.0, 0.0, WIN);
        let diag = step_cursor([500.0, 500.0], [1.0, 1.0], 1.0, 100.0, 0.0, WIN);
        let d = ((diag[0] - 500.0).powi(2) + (diag[1] - 500.0).powi(2)).sqrt();
        assert!((d - (straight[0] - 500.0)).abs() < 1e-3);
    }

    #[test]
    fn drawn_only_while_active() {
        let mut world = World::new();
        assert!(cursor_quads(&world).is_empty());
        world.insert_resource(SoftCursor {
            enabled: true,
            ..SoftCursor::default()
        });
        assert!(cursor_quads(&world).is_empty(), "inactive");
        world.resource_mut::<SoftCursor>().active = true;
        assert_eq!(cursor_quads(&world).len(), 4);
    }

    fn platform_messages(app: &mut App) {
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
    }

    /// A stick move refreshes the cursor tail and leaves the widget draw list's generation alone.
    #[test]
    fn stick_move_does_not_rebuild_widget_quads() {
        use std::time::Duration;

        use regia_platform::{GamepadAxis, GamepadAxisChanged};

        use crate::prelude::*;
        use crate::render::SharedUiRenderer;

        let mut app = App::new();
        app.add_plugins(UiPlugin::code_only());
        platform_messages(&mut app);
        let world = app.world_mut();
        world.resource_mut::<UiScales>().window_size = [1280.0, 720.0];
        let root = world
            .spawn((
                UiRoot::default(),
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                    ..Default::default()
                }),
            ))
            .id();
        let panel = world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(80.0), Dimension::Points(24.0)],
                    ..Default::default()
                }),
            ))
            .id();
        world.entity_mut(root).add_child(panel);
        for _ in 0..4 {
            app.update();
        }
        {
            let world = app.world_mut();
            {
                let mut cursor = world.resource_mut::<SoftCursor>();
                cursor.enabled = true;
                cursor.dead_zone = 0.0;
                cursor.speed = 800.0;
            }
            world.resource_mut::<UiPointer>().cursor = Some([100.0, 100.0]);
            world.resource_mut::<UiScales>().window_size = [1280.0, 720.0];
            world.write_message(GamepadAxisChanged {
                device: 1,
                axis: GamepadAxis::RightStickX,
                value: 1.0,
            });
        }
        // A fresh detect sees every settled widget as changed. One scheduled frame at 500 ms
        // is the measurement: full stick, no dead zone, 800 px/s moves x by 400.
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(500),
        ));
        let snap = |app: &App| {
            let renderer = app.world().resource::<SharedUiRenderer>().lock();
            (renderer.quads_generation(), renderer.subtree_walks())
        };
        let (generation, walks) = snap(&app);
        app.update();
        let pos = app.world().resource::<SoftCursor>().pos;
        assert!(
            (pos[0] - 500.0).abs() < 1e-2 && (pos[1] - 100.0).abs() < 1e-2,
            "stick steps the cursor from the pointer: {pos:?}"
        );
        assert!(!app.world().resource::<crate::extract::UiDamage>().0);
        let (generation2, walks2) = snap(&app);
        assert_eq!(
            (generation2, walks2),
            (generation, walks),
            "cursor move must not rebuild widgets: generation {generation}->{generation2}, walks {walks}->{walks2}"
        );
        let followed = app
            .world()
            .resource::<SharedUiRenderer>()
            .lock()
            .quads()
            .iter()
            .any(|quad| quad.z > 1.0e6 && (quad.pos[0] - 500.0).abs() < 20.0);
        assert!(followed, "the cross follows the cursor without a widget rebuild");
    }
}
