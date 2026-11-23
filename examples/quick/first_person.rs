// SPDX-License-Identifier: MIT OR Apache-2.0
// Ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3), examples/first_person.rs,
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

//! First-person camera over a grid of wire cubes: arrow keys move, the mouse looks, Tab frees the
//! cursor, Escape quits. `cargo run -p regia --example first_person`
//!
//! The original clears to light grey; here a light-grey floor plane plays that part (the 3D view
//! renders when at least one mesh is drawn, and wire cubes and the grid are lines).

use regia::quick::prelude::*;

const MOVE_SPEED: f32 = 6.0; // units per second (the original moves 0.1 per frame)
const LOOK_SPEED: f32 = 0.1;
const BOUNDS: f32 = 8.0;

#[derive(Reflect)]
struct Walker {
    position: Vec3,
    yaw: f32,
    pitch: f32,
    grabbed: bool,
    x: f32,
    switch: bool,
}

fn main() {
    regia::quick::app(model)
        .update(update)
        .view(view)
        .title("First Person")
        .size(1260, 768)
        .run();
}

fn model(q: &Quick) -> Walker {
    q.grab_cursor(true);
    Walker {
        position: Vec3::new(0.0, 1.0, 0.0),
        yaw: 1.18,
        pitch: 0.0,
        grabbed: true,
        x: 0.0,
        switch: false,
    }
}

fn front(w: &Walker) -> Vec3 {
    Vec3::new(
        w.yaw.cos() * w.pitch.cos(),
        w.pitch.sin(),
        w.yaw.sin() * w.pitch.cos(),
    )
    .normalize()
}

fn update(q: &Quick, w: &mut Walker) {
    let delta = q.delta();
    if q.key_pressed("Escape") {
        q.quit();
    }
    if q.key_pressed("Tab") {
        w.grabbed = !w.grabbed;
        q.grab_cursor(w.grabbed);
    }
    let front = front(w);
    let right = front.cross(Vec3::Y).normalize();
    let step = MOVE_SPEED * delta;
    for (key, dir) in [
        ("ArrowUp", front),
        ("ArrowDown", -front),
        ("ArrowLeft", -right),
        ("ArrowRight", right),
    ] {
        if q.key_down(key) {
            w.position += dir * step;
        }
    }
    if w.grabbed {
        let mouse = q.mouse_delta();
        w.yaw += mouse.x * delta * LOOK_SPEED;
        w.pitch = (w.pitch - mouse.y * delta * LOOK_SPEED).clamp(-1.5, 1.5);
        w.x += if w.switch { 2.4 * delta } else { -2.4 * delta };
        if w.x >= BOUNDS || w.x <= -BOUNDS {
            w.switch = !w.switch;
        }
    }
}

fn view(q: &Quick, w: &Walker) {
    q.camera_3d(w.position, w.position + front(w));
    let d3 = q.draw_3d();
    d3.plane(Vec3::new(0.0, -0.01, 0.0), Vec2::splat(30.0))
        .color(srgb_u8(200, 200, 200));
    d3.grid(Vec3::ZERO, 20, 1.0, Color::BLACK, Color::GRAY);
    d3.line(Vec3::new(w.x, 0.0, w.x), Vec3::splat(5.0), Color::YELLOW);
    d3.cube_wires(Vec3::new(0.0, 1.0, -6.0), Vec3::splat(2.0), Color::GREEN);
    d3.cube_wires(Vec3::new(0.0, 1.0, 6.0), Vec3::splat(2.0), Color::BLUE);
    d3.cube_wires(Vec3::new(2.0, 1.0, 2.0), Vec3::splat(2.0), Color::RED);
    // Back to screen space for the text.
    let draw = q.draw();
    let area = q.window_rect();
    let mouse = q.mouse_screen();
    let lines = [
        "First Person Camera".to_string(),
        format!("X: {} Y: {}", mouse.x, mouse.y),
        format!("Press <TAB> to toggle mouse grab: {}", w.grabbed),
    ];
    for (i, line) in lines.iter().enumerate() {
        draw.text(line)
            .left_justify()
            .w(area.w() - 20.0)
            .x_y(0.0, area.top() - 24.0 - 26.0 * i as f32)
            .font_size(24)
            .color(Color::BLACK);
    }
}
