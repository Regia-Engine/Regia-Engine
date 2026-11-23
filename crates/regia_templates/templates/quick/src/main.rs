// SPDX-License-Identifier: MIT-0
//! {{name}}: a Regia Engine quick app. Arrow keys (or a touch) steer the paddle; keep the ball up.

// A shipped Windows game opens no console window (the GUI subsystem); debug builds keep the console
// for logs (ADR-2306, `regia package` checks the shipped exe).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use regia::quick::prelude::*;

#[derive(Reflect, Default)]
struct Model {
    paddle_x: f32,
    ball: Vec2,
    velocity: Vec2,
    score: u32,
    // Optional process exit for the CI smoke; kept across a missed ball.
    exit_after: Option<u64>,
}

fn main() {
    regia::quick::app(model)
        .update(update)
        .view(view)
        .title("{{name}}")
        .size(800, 600)
        .run();
}

/// `value` limited to `lo..=hi`; a window narrower than the margins (`lo > hi`) pins to the middle
/// instead of panicking like `f32::clamp`.
fn clamp_within(value: f32, lo: f32, hi: f32) -> f32 {
    if lo > hi {
        (lo + hi) * 0.5
    } else {
        value.clamp(lo, hi)
    }
}

fn model(_q: &Quick) -> Model {
    Model {
        velocity: Vec2::new(180.0, 240.0),
        exit_after: exit_after_frames(),
        ..Model::default()
    }
}

fn update(q: &Quick, m: &mut Model) {
    if m.exit_after
        .is_some_and(|limit| q.frame().saturating_add(1) >= limit)
    {
        q.quit();
    }
    let area = q.window_rect();
    let dt = q.delta();
    let mut steer = 0.0_f32;
    if q.key_down("ArrowLeft") {
        steer -= 1.0;
    }
    if q.key_down("ArrowRight") {
        steer += 1.0;
    }
    let mut step = steer * 500.0 * dt;
    if let Some(touch) = q.touches().first() {
        // Move toward the finger by at most the paddle speed, never past it.
        let reach = 500.0 * dt;
        step = (touch.position.x - m.paddle_x).clamp(-reach, reach);
    }
    m.paddle_x = clamp_within(m.paddle_x + step, area.left() + 60.0, area.right() - 60.0);

    advance_ball(m, area.left(), area.right(), area.bottom(), area.top(), dt);
}

/// The value of `--exit-after-frames N`, used by the CI smoke.
fn exit_after_frames() -> Option<u64> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--exit-after-frames" {
            return args.next()?.to_str()?.parse().ok();
        }
    }
    None
}

/// Advance the ball and resolve a downward crossing of the paddle's top before a miss.
fn advance_ball(m: &mut Model, left: f32, right: f32, bottom: f32, top: f32, dt: f32) {
    let previous = m.ball;
    m.ball += m.velocity * dt;
    if m.ball.x < left + 10.0 || m.ball.x > right - 10.0 {
        m.velocity.x = -m.velocity.x;
        m.ball.x = clamp_within(m.ball.x, left + 10.0, right - 10.0);
    }
    if m.ball.y > top - 10.0 {
        m.velocity.y = -m.velocity.y.abs();
        m.ball.y = top - 10.0;
    }
    let paddle_top = bottom + 48.0;
    if m.velocity.y < 0.0 && previous.y >= paddle_top && m.ball.y <= paddle_top {
        let fraction = (previous.y - paddle_top) / (previous.y - m.ball.y);
        let hit_x = previous.x + (m.ball.x - previous.x) * fraction;
        if (hit_x - m.paddle_x).abs() < 70.0 {
            m.ball.y = paddle_top;
            m.velocity.y = (m.velocity.y.abs() * 1.05).min(900.0);
            m.score = m.score.saturating_add(1);
            return;
        }
    }
    if m.ball.y < bottom {
        let exit_after = m.exit_after;
        *m = Model {
            velocity: Vec2::new(180.0, 240.0),
            exit_after,
            ..Model::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paddle_crossing_precedes_a_miss_and_caps_speed() {
        for (speed, dt) in [(240.0, 0.25), (2_900.0, 1.0 / 60.0)] {
            let mut m = Model {
                ball: Vec2::new(0.0, -250.0),
                velocity: Vec2::new(0.0, -speed),
                score: 3,
                ..Model::default()
            };
            advance_ball(&mut m, -400.0, 400.0, -300.0, 300.0, dt);
            assert_eq!(m.score, 4);
            assert_eq!(m.ball.y, -252.0);
            assert!(m.velocity.y > 0.0 && m.velocity.y <= 900.0);
        }
    }

    #[test]
    fn a_ball_already_below_the_paddle_is_a_miss() {
        let mut m = Model {
            ball: Vec2::new(0.0, -260.0),
            velocity: Vec2::new(0.0, -240.0),
            score: 3,
            exit_after: Some(5),
            ..Model::default()
        };
        advance_ball(&mut m, -400.0, 400.0, -300.0, 300.0, 0.2);
        assert_eq!(m.score, 0);
        assert_eq!(m.exit_after, Some(5));
    }
}

fn view(q: &Quick, m: &Model) {
    let draw = q.draw();
    let area = q.window_rect();
    draw.background().color(srgb_u8(24, 26, 38));
    draw.rect()
        .x_y(m.paddle_x, area.bottom() + 30.0)
        .w_h(120.0, 16.0)
        .color(srgb_u8(240, 200, 80));
    draw.ellipse().xy(m.ball).radius(10.0).color(Color::WHITE);
    draw.text(&format!("score {}", m.score))
        .x_y(area.left() + 80.0, area.top() - 24.0)
        .font_size(20)
        .color(Color::WHITE);
}
