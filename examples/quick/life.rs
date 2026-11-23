// SPDX-License-Identifier: MIT OR Apache-2.0
// Ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3), examples/life.rs,
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

//! Conway's Game of Life on a wrapping-free grid that fills the window.
//! `cargo run -p regia --example life`
//!
//! The original gives every pixel a cell and draws the grid as a texture; this port uses 4-unit cells
//! drawn as rectangles, which keeps the grid readable on high-density screens.

use regia::quick::prelude::*;

const CELL: f32 = 4.0;

#[derive(Reflect, Default)]
struct Life {
    w: usize,
    h: usize,
    cells: Vec<bool>,
    #[reflect(ignore)]
    buffer: Vec<bool>,
}

fn main() {
    regia::quick::app(model)
        .update(update)
        .view(view)
        .title("Life")
        .run();
}

fn model(q: &Quick) -> Life {
    let size = q.window_size() / CELL;
    let (w, h) = (size.x as usize, size.y as usize);
    let mut rng = Rng::new(42);
    // One cell in five starts alive.
    let cells = (0..w * h).map(|_| rng.range(0..5) == 0).collect();
    Life {
        w,
        h,
        cells,
        buffer: vec![false; w * h],
    }
}

fn update(_q: &Quick, life: &mut Life) {
    let (w, h) = (life.w as i32, life.h as i32);
    life.buffer.resize(life.cells.len(), false);
    for y in 0..h {
        for x in 0..w {
            let mut neighbors = 0;
            for j in -1..=1 {
                for i in -1..=1 {
                    // Out of bounds, or the cell itself.
                    if y + j < 0 || y + j >= h || x + i < 0 || x + i >= w || (i == 0 && j == 0) {
                        continue;
                    }
                    neighbors += usize::from(life.cells[((y + j) * w + x + i) as usize]);
                }
            }
            let index = (y * w + x) as usize;
            life.buffer[index] = matches!((life.cells[index], neighbors), (true, 2 | 3) | (false, 3));
        }
    }
    std::mem::swap(&mut life.cells, &mut life.buffer);
}

fn view(q: &Quick, life: &Life) {
    let draw = q.draw();
    let area = q.window_rect();
    draw.background().color(Color::WHITE);
    for (index, _) in life.cells.iter().enumerate().filter(|(_, alive)| **alive) {
        let (x, y) = ((index % life.w) as f32, (index / life.w) as f32);
        let centre = Vec2::new(area.left() + (x + 0.5) * CELL, area.top() - (y + 0.5) * CELL);
        draw.rect().xy(centre).w_h(CELL, CELL).color(Color::BLACK);
    }
}
