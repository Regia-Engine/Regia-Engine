// SPDX-License-Identifier: MIT OR Apache-2.0
// Ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3), examples/asteroids.rs,
// rewritten onto regia::quick. © 2019-2021 Fedor Logachev; MIT OR Apache-2.0, used here under the
// MIT licence (see docs/ported-sources.toml):
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software and
// associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute,
// sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions: The above copyright notice and this
// permission notice shall be included in all copies or substantial portions of the Software.
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT
// NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES
// OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Asteroids: up thrusts, left/right steer, space shoots, Enter restarts.
//! `cargo run -p regia --example asteroids`
//!
//! The original moves things a fixed amount per frame; here those amounts are per 1/60 s, scaled by
//! the frame time, so the game runs at the same speed at any frame rate.

use regia::quick::prelude::*;

const SHIP_HEIGHT: f32 = 25.0;
const SHIP_BASE: f32 = 22.0;

#[derive(Reflect, Default)]
struct Ship {
    pos: Vec2,
    /// Degrees clockwise from straight up.
    rot: f32,
    vel: Vec2,
}

#[derive(Reflect, Default)]
struct Bullet {
    pos: Vec2,
    vel: Vec2,
    shot_at: f32,
    collided: bool,
}

#[derive(Reflect, Default)]
struct Asteroid {
    pos: Vec2,
    vel: Vec2,
    rot: f32,
    rot_speed: f32,
    size: f32,
    sides: u8,
    collided: bool,
}

#[derive(Reflect, Default)]
struct Game {
    ship: Ship,
    bullets: Vec<Bullet>,
    asteroids: Vec<Asteroid>,
    last_shot: f32,
    gameover: bool,
    #[reflect(ignore)]
    rng: Rng,
}

fn main() {
    regia::quick::app(model)
        .update(update)
        .view(view)
        .title("Asteroids")
        .run();
}

fn model(q: &Quick) -> Game {
    let mut game = Game::default();
    let area = q.window_rect();
    let reach = area.w().min(area.h());
    for _ in 0..10 {
        let dir = Vec2::new(game.rng.range(-1.0..1.0), game.rng.range(-1.0..1.0)).normalize_or(Vec2::X);
        let asteroid = Asteroid {
            pos: dir * reach / 2.0,
            vel: Vec2::new(game.rng.range(-1.0..1.0), game.rng.range(-1.0..1.0)),
            rot_speed: game.rng.range(-2.0..2.0),
            size: reach / 10.0,
            sides: game.rng.range(3..8),
            ..Asteroid::default()
        };
        game.asteroids.push(asteroid);
    }
    game
}

/// The unit vector the ship points along (rotation is clockwise from +Y).
fn heading(rot_degrees: f32) -> Vec2 {
    let r = rot_degrees.to_radians();
    Vec2::new(r.sin(), r.cos())
}

fn wrap_around(p: Vec2, area: geom::Rect) -> Vec2 {
    let wrap = |v: f32, min: f32, max: f32| {
        if v > max {
            min
        } else if v < min {
            max
        } else {
            v
        }
    };
    Vec2::new(
        wrap(p.x, area.left(), area.right()),
        wrap(p.y, area.bottom(), area.top()),
    )
}

fn update(q: &Quick, g: &mut Game) {
    if g.gameover {
        if q.key_down("Enter") {
            *g = model(q);
        }
        return;
    }
    let (t, steps, area) = (q.time(), q.delta() * 60.0, q.window_rect());
    let forward = heading(g.ship.rot);
    let mut acc = -g.ship.vel / 100.0; // friction
    if q.key_down("ArrowUp") {
        acc = forward / 3.0;
    }
    if q.key_down("Space") && t - g.last_shot > 0.5 {
        let pos = g.ship.pos + forward * SHIP_HEIGHT / 2.0;
        g.bullets.push(Bullet {
            pos,
            vel: forward * 7.0,
            shot_at: t,
            collided: false,
        });
        g.last_shot = t;
    }
    if q.key_down("ArrowRight") {
        g.ship.rot += 5.0 * steps;
    } else if q.key_down("ArrowLeft") {
        g.ship.rot -= 5.0 * steps;
    }
    g.ship.vel = (g.ship.vel + acc * steps).clamp_length_max(5.0);
    g.ship.pos = wrap_around(g.ship.pos + g.ship.vel * steps, area);
    for bullet in &mut g.bullets {
        bullet.pos += bullet.vel * steps;
    }
    for asteroid in &mut g.asteroids {
        asteroid.pos = wrap_around(asteroid.pos + asteroid.vel * steps, area);
        asteroid.rot += asteroid.rot_speed * steps;
    }
    g.bullets.retain(|bullet| bullet.shot_at + 1.5 > t);
    let mut new_asteroids = Vec::new();
    for asteroid in &mut g.asteroids {
        if (asteroid.pos - g.ship.pos).length() < asteroid.size + SHIP_HEIGHT / 3.0 {
            g.gameover = true;
            break;
        }
        let Some(bullet) = g
            .bullets
            .iter_mut()
            .find(|b| (asteroid.pos - b.pos).length() < asteroid.size)
        else {
            continue;
        };
        asteroid.collided = true;
        bullet.collided = true;
        if asteroid.sides > 3 {
            for side in [1.0_f32, -1.0] {
                new_asteroids.push(Asteroid {
                    pos: asteroid.pos,
                    vel: bullet.vel.perp().normalize() * side * g.rng.range(1.0_f32..3.0),
                    rot: g.rng.range(0.0..360.0),
                    rot_speed: g.rng.range(-2.0..2.0),
                    size: asteroid.size * 0.8,
                    sides: asteroid.sides - 1,
                    collided: false,
                });
            }
        }
    }
    g.bullets.retain(|bullet| !bullet.collided);
    g.asteroids.retain(|asteroid| !asteroid.collided);
    g.asteroids.append(&mut new_asteroids);
    g.gameover |= g.asteroids.is_empty();
}

fn view(q: &Quick, g: &Game) {
    let draw = q.draw();
    let ink = Color::BLACK;
    draw.background().color(srgb_u8(200, 200, 200));
    if g.gameover {
        let text = if g.asteroids.is_empty() {
            "You Win!"
        } else {
            "Game Over."
        };
        draw.text(&format!("{text} Press [enter] to play again."))
            .w(q.window_rect().w())
            .font_size(30)
            .color(srgb_u8(80, 80, 80));
        return;
    }
    for bullet in &g.bullets {
        draw.ellipse().xy(bullet.pos).radius(2.0).color(ink);
    }
    for a in &g.asteroids {
        let corners = (0..a.sides).map(|i| {
            let angle = a.rot.to_radians() + std::f32::consts::TAU * f32::from(i) / f32::from(a.sides);
            a.pos + Vec2::from_angle(angle) * a.size
        });
        draw.polyline().weight(2.0).color(ink).points_closed(corners);
    }
    let (fwd, side) = (heading(g.ship.rot), heading(g.ship.rot + 90.0));
    let nose = g.ship.pos + fwd * SHIP_HEIGHT / 2.0;
    let back = g.ship.pos - fwd * SHIP_HEIGHT / 2.0;
    let hull = [nose, back - side * SHIP_BASE / 2.0, back + side * SHIP_BASE / 2.0];
    draw.polyline().weight(2.0).color(ink).points_closed(hull);
}
