// SPDX-License-Identifier: MIT OR Apache-2.0
// Ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3), examples/snake.rs,
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

//! Snake: arrow keys steer, Enter restarts. `cargo run -p regia --example snake`

use std::collections::VecDeque;

use regia::quick::prelude::*;

const SQUARES: i16 = 16;
type Point = (i16, i16);

#[derive(Reflect, Default)]
struct Snake {
    head: Point,
    body: VecDeque<Point>,
    dir: Point,
    fruit: Point,
    score: u32,
    speed: f32,
    last_update: f32,
    navigation_lock: bool,
    game_over: bool,
    #[reflect(ignore)]
    rng: Rng,
}

fn main() {
    regia::quick::app(model)
        .update(update)
        .view(view)
        .title("Snake")
        .run();
}

fn model(q: &Quick) -> Snake {
    let mut rng = Rng::new(q.frame() ^ 0x5eed);
    Snake {
        dir: (1, 0),
        fruit: (rng.range(0..SQUARES), rng.range(0..SQUARES)),
        speed: 0.3,
        last_update: q.time(),
        rng,
        ..Snake::default()
    }
}

fn update(q: &Quick, s: &mut Snake) {
    if s.game_over {
        if q.key_down("Enter") {
            *s = model(q);
        }
        return;
    }
    let (up, down, right, left) = ((0, -1), (0, 1), (1, 0), (-1, 0));
    for (key, dir, opposite) in [
        ("ArrowRight", right, left),
        ("ArrowLeft", left, right),
        ("ArrowUp", up, down),
        ("ArrowDown", down, up),
    ] {
        if q.key_down(key) && s.dir != opposite && !s.navigation_lock {
            s.dir = dir;
            s.navigation_lock = true;
        }
    }
    if q.time() - s.last_update > s.speed {
        s.last_update = q.time();
        s.body.push_front(s.head);
        s.head = (s.head.0 + s.dir.0, s.head.1 + s.dir.1);
        if s.head == s.fruit {
            s.fruit = (s.rng.range(0..SQUARES), s.rng.range(0..SQUARES));
            s.score += 100;
            s.speed *= 0.9;
        } else {
            s.body.pop_back();
        }
        let outside = s.head.0 < 0 || s.head.1 < 0 || s.head.0 >= SQUARES || s.head.1 >= SQUARES;
        if outside || s.body.contains(&s.head) {
            s.game_over = true;
        }
        s.navigation_lock = false;
    }
}

fn view(q: &Quick, s: &Snake) {
    let draw = q.draw();
    let area = q.window_rect();
    if s.game_over {
        draw.background().color(Color::WHITE);
        draw.text("Game Over. Press [enter] to play again.")
            .w(area.w())
            .font_size(30)
            .color(srgb_u8(80, 80, 80));
        return;
    }
    draw.background().color(srgb_u8(200, 200, 200));
    let game = area.w().min(area.h()) - 20.0;
    let sq = game / f32::from(SQUARES);
    let half = game / 2.0;
    draw.rect().w_h(game, game).color(Color::WHITE);
    for i in 1..SQUARES {
        let at = -half + sq * f32::from(i);
        let grey = srgb_u8(200, 200, 200);
        draw.line()
            .start(Vec2::new(-half, at))
            .end(Vec2::new(half, at))
            .weight(2.0)
            .color(grey);
        draw.line()
            .start(Vec2::new(at, -half))
            .end(Vec2::new(at, half))
            .weight(2.0)
            .color(grey);
    }
    // Grid cells count down from the top row, as in the original.
    let cell = |(x, y): Point, color: Color| {
        let centre = Vec2::new(
            -half + (f32::from(x) + 0.5) * sq,
            half - (f32::from(y) + 0.5) * sq,
        );
        draw.rect().xy(centre).w_h(sq, sq).color(color);
    };
    cell(s.head, srgb_u8(0, 117, 44));
    for &part in &s.body {
        cell(part, srgb_u8(0, 158, 47));
    }
    cell(s.fruit, srgb_u8(255, 203, 0));
    draw.text(&format!("SCORE: {}", s.score))
        .x_y(area.left() + 80.0, area.top() - 20.0)
        .font_size(20)
        .color(srgb_u8(80, 80, 80));
}
