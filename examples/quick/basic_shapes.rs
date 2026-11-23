// SPDX-License-Identifier: MIT OR Apache-2.0
// Ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3), examples/basic_shapes.rs,
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

//! Basic shapes: a line, a rectangle, a circle and text, as a sketch (one `view`, no model).
//! `cargo run -p regia --example basic_shapes`

use regia::quick::prelude::*;

fn main() {
    regia::quick::sketch(view).title("BasicShapes").run();
}

fn view(q: &Quick) {
    let draw = q.draw();
    let area = q.window_rect();
    // The original measures from the top-left corner (y down); `top_left` converts.
    let top_left = |x: f32, y: f32| Vec2::new(area.left() + x, area.top() - y);
    draw.background().color(srgb_u8(200, 200, 200));
    draw.line()
        .start(top_left(40.0, 40.0))
        .end(top_left(100.0, 200.0))
        .weight(15.0)
        .color(srgb_u8(0, 121, 241));
    draw.rect()
        .x_y(0.0, area.top() - 130.0)
        .w_h(120.0, 60.0)
        .color(srgb_u8(0, 228, 48));
    draw.ellipse()
        .xy(Vec2::new(area.right() - 30.0, area.bottom() + 30.0))
        .radius(15.0)
        .color(srgb_u8(253, 249, 0));
    draw.text("HELLO")
        .xy(top_left(60.0, 20.0))
        .font_size(30)
        .color(srgb_u8(80, 80, 80));
}
