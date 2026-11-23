// SPDX-License-Identifier: MIT OR Apache-2.0
// Ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3), examples/arkanoid.rs,
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

//! Arkanoid: left/right move the platform, space launches the ball.
//! `cargo run -p regia --example arkanoid`

use regia::quick::prelude::*;

const BLOCKS_W: usize = 10;
const BLOCKS_H: usize = 10;
// The playfield is SCR_W x SCR_H units, (0, 0) top-left and y down, as in the original.
const SCR_W: f32 = 20.0;
const SCR_H: f32 = 20.0;
const PLATFORM_W: f32 = 5.0;
const PLATFORM_H: f32 = 0.2;

#[derive(Reflect)]
struct Game {
    blocks: Vec<bool>,
    ball: Vec2,
    vel: Vec2,
    platform_x: f32,
    stick: bool,
}

fn main() {
    regia::quick::app(model)
        .update(update)
        .view(view)
        .title("Arkanoid")
        .run();
}

fn model(_q: &Quick) -> Game {
    Game {
        blocks: vec![true; BLOCKS_W * BLOCKS_H],
        ball: Vec2::new(12.0, 7.0),
        vel: Vec2::new(3.5, -3.5),
        platform_x: 10.0,
        stick: true,
    }
}

fn block_rect(i: usize, j: usize) -> (Vec2, Vec2) {
    let size = Vec2::new(SCR_W / BLOCKS_W as f32, 7.0 / BLOCKS_H as f32);
    (
        Vec2::new(i as f32 * size.x + 0.05, j as f32 * size.y + 0.05),
        size,
    )
}

fn update(q: &Quick, g: &mut Game) {
    let delta = q.delta();
    if q.key_down("ArrowRight") && g.platform_x < SCR_W - PLATFORM_W / 2.0 {
        g.platform_x += 3.0 * delta;
    }
    if q.key_down("ArrowLeft") && g.platform_x > PLATFORM_W / 2.0 {
        g.platform_x -= 3.0 * delta;
    }
    if g.stick {
        g.ball = Vec2::new(g.platform_x, SCR_H - 0.5);
        g.stick = !q.key_down("Space");
    } else {
        g.ball += g.vel * delta;
    }
    if g.ball.x <= 0.0 || g.ball.x > SCR_W {
        g.vel.x *= -1.0;
    }
    let on_platform =
        g.ball.y > SCR_H - PLATFORM_H - 0.15 / 2.0 && (g.ball.x - g.platform_x).abs() <= PLATFORM_W / 2.0;
    if g.ball.y <= 0.0 || on_platform {
        g.vel.y *= -1.0;
    }
    if g.ball.y >= SCR_H {
        g.ball.y = 10.0;
        g.vel.y = -g.vel.y.abs();
        g.stick = true;
    }
    for j in 0..BLOCKS_H {
        for i in 0..BLOCKS_W {
            let (min, size) = block_rect(i, j);
            let inside = g.ball.cmpge(min).all() && g.ball.cmplt(min + size).all();
            if g.blocks[j * BLOCKS_W + i] && inside {
                g.vel.y *= -1.0;
                g.blocks[j * BLOCKS_W + i] = false;
            }
        }
    }
}

fn view(q: &Quick, g: &Game) {
    let area = q.window_rect();
    let scale = (area.w() / SCR_W).min(area.h() / SCR_H);
    // Playfield units (y down) to drawing coordinates (centred, y up).
    let to_screen = |p: Vec2| Vec2::new(p.x - SCR_W / 2.0, SCR_H / 2.0 - p.y) * scale;
    let rect = |min: Vec2, size: Vec2, color: Color| {
        q.draw()
            .rect()
            .xy(to_screen(min + size / 2.0))
            .wh(size * scale)
            .color(color);
    };
    q.draw().background().color(srgb_u8(102, 191, 255));
    for j in 0..BLOCKS_H {
        for i in 0..BLOCKS_W {
            if g.blocks[j * BLOCKS_W + i] {
                let (min, size) = block_rect(i, j);
                rect(min, size - 0.1, srgb_u8(0, 82, 172));
            }
        }
    }
    q.draw()
        .ellipse()
        .xy(to_screen(g.ball))
        .radius(0.2 * scale)
        .color(srgb_u8(230, 41, 55));
    let platform = Vec2::new(g.platform_x - PLATFORM_W / 2.0, SCR_H - PLATFORM_H);
    rect(platform, Vec2::new(PLATFORM_W, PLATFORM_H), srgb_u8(112, 31, 126));
    if g.stick {
        q.draw()
            .text("Press space to start")
            .w(area.w())
            .font_size(24)
            .color(Color::WHITE);
    }
}
