// SPDX-License-Identifier: MIT OR Apache-2.0
// Ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3), examples/platformer.rs and physics-platformer/src/lib.rs (move_h/move_v/solid_move); examples/platformer.rs,
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

//! A tile platformer: left/right run, space jumps; the orange platform carries you.
//! `cargo run -p regia --example platformer`
//!
//! The original loads a Tiled map and a tileset image through two helper crates. This port keeps the
//! game (8-unit tiles, a 320 x 152 playfield, pixel-stepped movement with sub-pixel remainders, a
//! moving platform that carries its rider across a pit) with the map inline and flat-colour tiles.

use regia::quick::prelude::*;

const TILE: f32 = 8.0;
const MAP: [&str; 19] = [
    "........................................",
    "........................................",
    "........................................",
    "........................................",
    "........................................",
    "........................................",
    "...............................#####....",
    "........................................",
    "......#####.............................",
    "........................................",
    "................####....................",
    "##......................................",
    "##......................................",
    "##......................................",
    "##...................................###",
    "#################................#######",
    "#################................#######",
    "#################................#######",
    "########################################",
];

/// Pixel-stepped position with a sub-pixel remainder, as in macroquad-platformer.
#[derive(Reflect, Default, Clone, Copy)]
struct Body {
    pos: Vec2,
    remainder: Vec2,
    size: Vec2,
}

#[derive(Reflect, Default)]
struct Game {
    player: Body,
    speed: Vec2,
    platform: Body,
    platform_speed: f32,
}

fn main() {
    regia::quick::app(model)
        .update(update)
        .view(view)
        .title("Platformer")
        .run();
}

fn model(_q: &Quick) -> Game {
    let body = |x, y, w, h| Body {
        pos: Vec2::new(x, y),
        size: Vec2::new(w, h),
        ..Body::default()
    };
    Game {
        player: body(50.0, 80.0, 8.0, 8.0),
        speed: Vec2::ZERO,
        platform: body(170.0, 130.0, 32.0, 8.0),
        platform_speed: 50.0,
    }
}

fn overlaps(a: Vec2, a_size: Vec2, b: Vec2, b_size: Vec2) -> bool {
    a.x < b.x + b_size.x && b.x < a.x + a_size.x && a.y < b.y + b_size.y && b.y < a.y + a_size.y
}

/// A box at `pos` hits a map tile or the platform (playfield units, y down).
fn collides(g: &Game, pos: Vec2, size: Vec2) -> bool {
    let (x0, y0) = ((pos.x / TILE).floor() as i32, (pos.y / TILE).floor() as i32);
    let (x1, y1) = (
        ((pos.x + size.x - 1.0) / TILE).floor() as i32,
        ((pos.y + size.y - 1.0) / TILE).floor() as i32,
    );
    let solid_tile = (y0..=y1).any(|y| (x0..=x1).any(|x| tile(x, y)));
    solid_tile || overlaps(pos, size, g.platform.pos, g.platform.size)
}

fn tile(x: i32, y: i32) -> bool {
    let row = usize::try_from(y).ok().and_then(|y| MAP.get(y));
    row.is_none_or(|row| usize::try_from(x).ok().and_then(|x| row.as_bytes().get(x)) != Some(&b'.'))
}

/// Moves the player one pixel at a time along `axis`; returns false when blocked.
fn move_player(g: &mut Game, axis: Vec2, amount: f32) -> bool {
    g.player.remainder += axis * amount;
    let steps = g.player.remainder.dot(axis).round();
    g.player.remainder -= axis * steps;
    for _ in 0..steps.abs() as i32 {
        let next = g.player.pos + axis * steps.signum();
        if collides(g, next, g.player.size) {
            return false;
        }
        g.player.pos = next;
    }
    true
}

fn update(q: &Quick, g: &mut Game) {
    let dt = q.delta();
    let on_ground = collides(g, g.player.pos + Vec2::Y, g.player.size);
    if !on_ground {
        g.speed.y += 500.0 * dt;
    }
    g.speed.x = if q.key_down("ArrowRight") {
        100.0
    } else if q.key_down("ArrowLeft") {
        -100.0
    } else {
        0.0
    };
    if q.key_pressed("Space") && on_ground {
        g.speed.y = -120.0;
    }
    move_player(g, Vec2::X, g.speed.x * dt);
    if !move_player(g, Vec2::Y, g.speed.y * dt) {
        g.speed.y = 0.0;
    }
    // The platform moves whole pixels; a rider standing on it moves with it.
    let p = &mut g.platform;
    p.remainder.x += g.platform_speed * dt;
    let step = p.remainder.x.round();
    p.remainder.x -= step;
    let feet = g.player.pos + Vec2::new(0.0, g.player.size.y);
    let riding = overlaps(
        feet,
        Vec2::new(g.player.size.x, 1.0),
        p.pos,
        Vec2::new(p.size.x, 1.0),
    );
    p.pos.x += step;
    if riding {
        move_player(g, Vec2::X, step);
    }
    let x = g.platform.pos.x;
    if (g.platform_speed > 1.0 && x >= 220.0) || (g.platform_speed < -1.0 && x <= 150.0) {
        g.platform_speed *= -1.0;
    }
}

fn view(q: &Quick, g: &Game) {
    let draw = q.draw();
    let area = q.window_rect();
    let scale = (area.w() / 320.0).min(area.h() / 152.0);
    // Playfield (y down, 320 x 152) to drawing coordinates (centred, y up).
    let rect = |pos: Vec2, size: Vec2, color: Color| {
        let centre = Vec2::new(pos.x + size.x / 2.0 - 160.0, 76.0 - pos.y - size.y / 2.0) * scale;
        draw.rect().xy(centre).wh(size * scale).color(color);
    };
    draw.background().color(Color::BLACK);
    for (y, row) in MAP.iter().enumerate() {
        for (x, _) in row.bytes().enumerate().filter(|(_, c)| *c == b'#') {
            let shade = if y > 0 && MAP[y - 1].as_bytes()[x] == b'.' {
                srgb_u8(90, 160, 70)
            } else {
                srgb_u8(110, 80, 60)
            };
            rect(Vec2::new(x as f32, y as f32) * TILE, Vec2::splat(TILE), shade);
        }
    }
    rect(g.platform.pos, g.platform.size, srgb_u8(230, 140, 40));
    rect(g.player.pos, g.player.size, srgb_u8(240, 220, 200));
    let eye = if g.speed.x >= 0.0 { 5.0 } else { 1.0 };
    rect(g.player.pos + Vec2::new(eye, 2.0), Vec2::splat(2.0), Color::BLACK);
}
