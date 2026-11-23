// SPDX-License-Identifier: MIT OR Apache-2.0
// Ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3), examples/input_touch.rs,
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

//! Touch input: a circle under every finger, coloured by phase; the mouse stands in on desktop.
//! `cargo run -p regia --example input_touch`

use regia::quick::prelude::*;

fn main() {
    regia::quick::sketch(view).title("InputTouch").run();
}

fn view(q: &Quick) {
    let draw = q.draw();
    let area = q.window_rect();
    draw.background().color(srgb_u8(200, 200, 200));
    for touch in q.touches() {
        let (fill, size) = match touch.phase {
            TouchPhase::Started => (srgb_u8(0, 228, 48), 80.0),
            TouchPhase::Moved => (srgb_u8(253, 249, 0), 60.0),
            TouchPhase::Ended => (srgb_u8(0, 121, 241), 80.0),
            TouchPhase::Cancelled => (Color::BLACK, 80.0),
        };
        draw.ellipse().xy(touch.position).radius(size).color(fill);
    }
    if q.touches().is_empty() && q.mouse_down(MouseButton::Left) {
        draw.ellipse().xy(q.mouse()).radius(60.0).color(Color::WHITE);
    }
    draw.text("touch the screen!")
        .x_y(area.left() + 100.0, area.top() - 20.0)
        .font_size(20)
        .color(srgb_u8(80, 80, 80));
}
