// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/drawing.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
// MIT OR Apache-2.0, used here under the MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::Rc;

use lyon::path::PathEvent;
use lyon::tessellation::{FillOptions, LineCap, LineJoin, StrokeOptions};

use crate::draw::mesh::Mesh;
use crate::draw::primitive::Primitive;
use crate::draw::properties::{
    SetColor, SetDimensions, SetFill, SetOrientation, SetPosition, SetStroke, color, fill,
    spatial::{dimension, orientation, position},
    stroke,
};
use crate::draw::{Draw, QuickImage, write};
use crate::prelude_internal::*;

/// A **Drawing** in progress.
///
/// **Drawing** provides a way of chaining together method calls describing properties of the thing
/// that we are drawing. **Drawing** ends when the instance is **Drop**ped, at which point the
/// properties of the drawing are inserted into the **Draw** type.
///
/// When a **Drawing** begins, a slot is immediately reserved for it within the **Draw**'s
/// command list. This ensures the correct drawing order is maintained.
///
/// The type parameter `T` is a zero-cost marker: the primitive itself is stored type-erased as a
/// [`Primitive`] on this drawing, and `T` only determines which builder methods are available
/// (via bounds like `T: SetColor`) and tracks type-state transitions (e.g.
/// `PathInit -> PathFill -> Path`). The method bodies delegate to non-generic functions that
/// mutate the stored primitive in place, keeping monomorphisation in downstream crates to a
/// minimum.
///
/// The in-progress primitive lives on this drawing, not in the shared state map. Reserving the
/// command slot and finishing the drawing are the only shared-state writes. [`Drawing`] is
/// therefore not `Send`: the primitive is reference-counted on the thread that draws.
pub struct Drawing<'a, T> {
    // The `Draw` instance used to create this drawing.
    pub(crate) draw: &'a Draw,
    // The key of this drawing in the open-drawing registry (unique per thread, never reused, so
    // a drawing begun before a `reset` cannot reach the drawing that later takes its command
    // slot). The command slot itself is `OpenSlot::index`.
    pub(crate) index: usize,
    // Whether the **Drawing** should attempt to finish the drawing on drop.
    pub(crate) finish_on_drop: bool,
    /// The primitive, held until finish. `None` after [`Self::transition`] moves it.
    slot: Option<Rc<RefCell<OpenSlot>>>,
    // The node type currently being drawn.
    _ty: PhantomData<T>,
}

/// One drawing that has reserved a command slot and has not finished.
pub(crate) struct OpenSlot {
    /// The reserved command slot.
    index: usize,
    epoch: u64,
    primitive: Option<Primitive>,
    texture: Option<QuickImage>,
}

type OpenDrawings = HashMap<(usize, usize), Rc<RefCell<OpenSlot>>>;

thread_local! {
    static OPEN_DRAWINGS: RefCell<OpenDrawings> = RefCell::new(HashMap::new());
    /// The next registry key. Keys are never reused on a thread: a command index is (a `reset`
    /// or a drain recycles it), which let a stale drawing edit and commit a newer one.
    static NEXT_KEY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn state_key(draw: &Draw) -> usize {
    std::sync::Arc::as_ptr(&draw.state) as usize
}

/// Registers `primitive` as the in-progress drawing at command slot `index`. Returns the
/// drawing's registry key and its slot.
///
/// The registry is thread-local; during thread teardown (after it is destroyed) the drawing is
/// simply not registered, and finishing it is a no-op instead of a panic.
pub(crate) fn open_drawing(
    draw: &Draw,
    index: usize,
    epoch: u64,
    primitive: Primitive,
) -> (usize, Rc<RefCell<OpenSlot>>) {
    let slot = Rc::new(RefCell::new(OpenSlot {
        index,
        epoch,
        primitive: Some(primitive),
        texture: None,
    }));
    let key = NEXT_KEY
        .try_with(|next| {
            let key = next.get();
            next.set(key.wrapping_add(1));
            key
        })
        .unwrap_or(usize::MAX);
    let _ = OPEN_DRAWINGS.try_with(|map| {
        map.borrow_mut().insert((state_key(draw), key), Rc::clone(&slot));
    });
    (key, slot)
}

fn lookup_open(draw: &Draw, key: usize) -> Option<Rc<RefCell<OpenSlot>>> {
    OPEN_DRAWINGS
        .try_with(|map| map.borrow().get(&(state_key(draw), key)).cloned())
        .ok()
        .flatten()
}

fn take_open(draw: &Draw, key: usize) -> Option<Rc<RefCell<OpenSlot>>> {
    OPEN_DRAWINGS
        .try_with(|map| map.borrow_mut().remove(&(state_key(draw), key)))
        .ok()
        .flatten()
}

/// Commits every in-progress drawing on `draw`.
///
/// A drain used to find those drawings in the state map. They now live on the [`Drawing`], so
/// the drain walks this registry before it bumps the epoch. No shared-state lock when none are
/// open.
pub(crate) fn commit_open_drawings(draw: &Draw) {
    let key = state_key(draw);
    let pending: Vec<usize> = OPEN_DRAWINGS
        .try_with(|map| {
            map.borrow()
                .keys()
                .filter(|(draw_key, _)| *draw_key == key)
                .map(|(_, open_key)| *open_key)
                .collect()
        })
        .unwrap_or_default();
    // Each commits into its own reserved slot, so the order does not matter.
    for open_key in pending {
        finish_drawing(draw, open_key);
    }
}

/// Some context that may be optionally provided to primitives in the drawing implementation.
///
/// This is particularly useful for paths and meshes.
pub struct DrawingContext<'a> {
    /// The intermediary mesh for buffering yet-to-be-drawn paths and meshes.
    pub mesh: &'a mut Mesh,
    /// A re-usable buffer for collecting path events.
    pub path_event_buffer: &'a mut Vec<PathEvent>,
    /// A re-usable buffer for collecting polyline points vertex data.
    pub path_points_vertex_buffer: &'a mut Vec<(Vec2, Color, Vec2)>,
    /// A re-usable buffer for collecting text.
    pub text_buffer: &'a mut String,
}

/// Construct a new **Drawing** instance.
pub fn new<T>(draw: &Draw, index: usize, slot: Rc<RefCell<OpenSlot>>) -> Drawing<'_, T>
where
    T: Into<Primitive>,
{
    Drawing {
        draw,
        index,
        finish_on_drop: true,
        slot: Some(slot),
        _ty: PhantomData,
    }
}

impl<'a, T> Drop for Drawing<'a, T> {
    fn drop(&mut self) {
        if self.finish_on_drop {
            finish_drawing(self.draw, self.index)
        }
    }
}

impl<'a> DrawingContext<'a> {
    // Initialise the DrawingContext from the draw's IntermediaryState.
    pub(crate) fn from_intermediary_state(state: &'a mut super::IntermediaryState) -> Self {
        let super::IntermediaryState {
            ref mut intermediary_mesh,
            ref mut path_event_buffer,
            ref mut path_points_vertex_buffer,
            ref mut text_buffer,
        } = *state;
        DrawingContext {
            mesh: intermediary_mesh,
            path_event_buffer,
            path_points_vertex_buffer,
            text_buffer,
        }
    }
}

impl<'a, T> Drawing<'a, T> {
    /// Complete the drawing and insert it into the parent **Draw** instance.
    ///
    /// This will be called when the **Drawing** is **Drop**ped if it has not yet been called.
    pub fn finish(mut self) {
        self.finish_on_drop = false;
        finish_drawing(self.draw, self.index)
    }

    // Consume the drawing, producing an equivalent drawing for the next type-state `U`.
    //
    // The conversion of the stored primitive itself is expected to have already been performed
    // (e.g. via `with_primitive`).
    pub(crate) fn transition<U>(mut self) -> Drawing<'a, U> {
        self.finish_on_drop = false;
        Drawing {
            draw: self.draw,
            index: self.index,
            finish_on_drop: true,
            slot: self.slot.take(),
            _ty: PhantomData,
        }
    }

    /// Texture this drawing with a registered image ([`crate::QuickImages`]): its texture
    /// coordinates sample the image, multiplied by the drawing's colour.
    pub fn texture(self, image: QuickImage) -> Self {
        if let Some(slot) = &self.slot {
            slot.borrow_mut().texture = Some(image);
        }
        self
    }
}

// SetColor implementations.

impl<'a, T> Drawing<'a, T>
where
    T: SetColor,
{
    /// Specify a color.
    ///
    /// This method supports any color type that can be converted into RGBA.
    ///
    /// Colors that have no alpha channel will be given an opaque alpha channel value `1.0`.
    pub fn color<C>(self, color: C) -> Self
    where
        C: Into<Color>,
    {
        color::set_color(self.draw, self.index, color.into());
        self
    }

    /// Specify the color via red, green and blue channels.
    pub fn srgb(self, r: f32, g: f32, b: f32) -> Self {
        self.color(crate::color::srgb(r, g, b))
    }

    /// Specify the color via red, green and blue channels as bytes
    pub fn srgb_u8(self, r: u8, g: u8, b: u8) -> Self {
        self.color(crate::color::srgb_u8(r, g, b))
    }

    /// Specify the color via red, green, blue and alpha channels.
    pub fn srgba(self, r: f32, g: f32, b: f32, a: f32) -> Self {
        self.color(crate::color::srgba(r, g, b, a))
    }

    /// Specify the color via red, green, blue and alpha channels as bytes.
    pub fn srgba_u8(self, r: u8, g: u8, b: u8, a: u8) -> Self {
        self.color(crate::color::srgba_u8(r, g, b, a))
    }

    /// Specify the color via linear red, green and blue channels.
    pub fn linear_rgb(self, r: f32, g: f32, b: f32) -> Self {
        self.color(crate::color::linear_rgb(r, g, b))
    }

    /// Specify the color via linear red, green, blue and alpha channels.
    pub fn linear_rgba(self, r: f32, g: f32, b: f32, a: f32) -> Self {
        self.color(crate::color::linear_rgba(r, g, b, a))
    }

    /// Specify the color via hue, saturation and luminance.
    ///
    /// If you're looking for HSVA or HSBA, use the `hsva` method instead.
    ///
    /// The given hue expects a value between `0.0` and `1.0` where `0.0` is 0 degress and `1.0` is
    /// 360 degrees (or 2 PI radians).
    ///
    /// See the [wikipedia entry](https://en.wikipedia.org/wiki/HSL_and_HSV) for more details on
    /// this color space.
    pub fn hsl(self, h: f32, s: f32, l: f32) -> Self {
        self.color(crate::color::hsl(h * 360.0, s, l))
    }

    /// Specify the color via hue, saturation, luminance and an alpha channel.
    ///
    /// If you're looking for HSVA or HSBA, use the `hsva` method instead.
    ///
    /// The given hue expects a value between `0.0` and `1.0` where `0.0` is 0 degress and `1.0` is
    /// 360 degrees (or 2 PI radians).
    ///
    /// See the [wikipedia entry](https://en.wikipedia.org/wiki/HSL_and_HSV) for more details on
    /// this color space.
    pub fn hsla(self, h: f32, s: f32, l: f32, a: f32) -> Self {
        self.color(crate::color::hsla(h * 360.0, s, l, a))
    }

    /// Specify the color via hue, saturation and *value* (brightness).
    ///
    /// This is sometimes also known as "hsb".
    ///
    /// The given hue expects a value between `0.0` and `1.0` where `0.0` is 0 degress and `1.0` is
    /// 360 degrees (or 2 PI radians).
    ///
    /// See the [wikipedia entry](https://en.wikipedia.org/wiki/HSL_and_HSV) for more details on
    /// this color space.
    pub fn hsv(self, h: f32, s: f32, v: f32) -> Self {
        self.color(crate::color::hsv(h * 360.0, s, v))
    }

    /// Specify the color via hue, saturation, *value* (brightness) and an alpha channel.
    ///
    /// This is sometimes also known as "hsba".
    ///
    /// The given hue expects a value between `0.0` and `1.0` where `0.0` is 0 degress and `1.0` is
    /// 360 degrees (or 2 PI radians).
    ///
    /// See the [wikipedia entry](https://en.wikipedia.org/wiki/HSL_and_HSV) for more details on
    /// this color space.
    pub fn hsva(self, h: f32, s: f32, v: f32, a: f32) -> Self {
        self.color(crate::color::hsva(h * 360.0, s, v, a))
    }

    /// Specify the color via hue (`0..1` turns), whiteness and blackness.
    pub fn hwb(self, h: f32, w: f32, b: f32) -> Self {
        self.color(crate::color::hwb(h * 360.0, w, b))
    }

    /// Specify the color via hue (`0..1` turns), whiteness, blackness and alpha.
    pub fn hwba(self, h: f32, w: f32, b: f32, a: f32) -> Self {
        self.color(crate::color::hwba(h * 360.0, w, b, a))
    }

    /// Specify the color via Oklab lightness and opponent axes.
    pub fn oklab(self, l: f32, a: f32, b: f32) -> Self {
        self.color(crate::color::oklab(l, a, b))
    }

    /// Specify the color via Oklab lightness, opponent axes and alpha.
    pub fn oklaba(self, l: f32, a: f32, b: f32, alpha: f32) -> Self {
        self.color(crate::color::oklaba(l, a, b, alpha))
    }

    /// Specify the color via Oklch lightness, chroma and hue (degrees).
    pub fn oklch(self, l: f32, c: f32, h: f32) -> Self {
        self.color(crate::color::oklch(l, c, h))
    }

    /// Specify the color via Oklch lightness, chroma, hue (degrees) and alpha.
    pub fn oklcha(self, l: f32, c: f32, h: f32, alpha: f32) -> Self {
        self.color(crate::color::oklcha(l, c, h, alpha))
    }

    /// Specify the color as gray scale
    ///
    /// The given g expects a value between `0.0` and `1.0` where `0.0` is black and `1.0` is white
    pub fn gray(self, g: f32) -> Self {
        self.color(crate::color::srgb(g, g, g))
    }
}

// SetDimensions implementations.

impl<'a, T> Drawing<'a, T>
where
    T: SetDimensions,
{
    /// Set the absolute width for the node.
    pub fn width(self, w: f32) -> Self {
        dimension::set_dimensions(self.draw, self.index, Some(w), None, None);
        self
    }

    /// Set the absolute height for the node.
    pub fn height(self, h: f32) -> Self {
        dimension::set_dimensions(self.draw, self.index, None, Some(h), None);
        self
    }

    /// Set the absolute depth for the node.
    pub fn depth(self, d: f32) -> Self {
        dimension::set_dimensions(self.draw, self.index, None, None, Some(d));
        self
    }

    /// Short-hand for the **width** method.
    pub fn w(self, w: f32) -> Self {
        self.width(w)
    }

    /// Short-hand for the **height** method.
    pub fn h(self, h: f32) -> Self {
        self.height(h)
    }

    /// Short-hand for the **depth** method.
    pub fn d(self, d: f32) -> Self {
        self.depth(d)
    }

    /// Set the **x** and **y** dimensions for the node.
    pub fn wh(self, v: Vec2) -> Self {
        dimension::set_dimensions(self.draw, self.index, Some(v.x), Some(v.y), None);
        self
    }

    /// Set the **x**, **y** and **z** dimensions for the node.
    pub fn whd(self, v: Vec3) -> Self {
        dimension::set_dimensions(self.draw, self.index, Some(v.x), Some(v.y), Some(v.z));
        self
    }

    /// Set the width and height for the node.
    pub fn w_h(self, x: f32, y: f32) -> Self {
        dimension::set_dimensions(self.draw, self.index, Some(x), Some(y), None);
        self
    }

    /// Set the width and height for the node.
    pub fn w_h_d(self, x: f32, y: f32, z: f32) -> Self {
        dimension::set_dimensions(self.draw, self.index, Some(x), Some(y), Some(z));
        self
    }
}

// SetPosition methods.

impl<'a, T> Drawing<'a, T>
where
    T: SetPosition,
{
    /// Build with the given **Absolute** **Position** along the *x* axis.
    pub fn x(self, x: f32) -> Self {
        position::set_position(self.draw, self.index, Some(x), None, None);
        self
    }

    /// Build with the given **Absolute** **Position** along the *y* axis.
    pub fn y(self, y: f32) -> Self {
        position::set_position(self.draw, self.index, None, Some(y), None);
        self
    }

    /// Build with the given **Absolute** **Position** along the *z* axis.
    pub fn z(self, z: f32) -> Self {
        position::set_position(self.draw, self.index, None, None, Some(z));
        self
    }

    /// Set the **Position** with some two-dimensional point.
    pub fn xy(self, p: Vec2) -> Self {
        position::set_position(self.draw, self.index, Some(p.x), Some(p.y), None);
        self
    }

    /// Set the **Position** with some three-dimensional point.
    pub fn xyz(self, p: Vec3) -> Self {
        position::set_position(self.draw, self.index, Some(p.x), Some(p.y), Some(p.z));
        self
    }

    /// Set the **Position** with *x* *y* coordinates.
    pub fn x_y(self, x: f32, y: f32) -> Self {
        position::set_position(self.draw, self.index, Some(x), Some(y), None);
        self
    }

    /// Set the **Position** with *x* *y* *z* coordinates.
    pub fn x_y_z(self, x: f32, y: f32, z: f32) -> Self {
        position::set_position(self.draw, self.index, Some(x), Some(y), Some(z));
        self
    }
}

// SetOrientation methods.

impl<'a, T> Drawing<'a, T>
where
    T: SetOrientation,
{
    /// Describe orientation via the vector that points to the given target.
    ///
    /// A zero or non-finite target gives no rotation; a target straight along `Y` uses `Z` as up.
    pub fn look_at(self, target: Vec3) -> Self {
        orientation::set_orientation(self.draw, self.index, orientation::Update::LookAt(target));
        self
    }

    /// Specify the orientation around the *x* axis as an absolute value in radians.
    pub fn x_radians(self, x: f32) -> Self {
        orientation::set_orientation(self.draw, self.index, orientation::Update::XRadians(x));
        self
    }

    /// Specify the orientation around the *y* axis as an absolute value in radians.
    pub fn y_radians(self, y: f32) -> Self {
        orientation::set_orientation(self.draw, self.index, orientation::Update::YRadians(y));
        self
    }

    /// Specify the orientation around the *z* axis as an absolute value in radians.
    pub fn z_radians(self, z: f32) -> Self {
        orientation::set_orientation(self.draw, self.index, orientation::Update::ZRadians(z));
        self
    }

    /// Specify the orientation around the *x* axis as an absolute value in degrees.
    pub fn x_degrees(self, x: f32) -> Self {
        self.x_radians(x.to_radians())
    }

    /// Specify the orientation around the *y* axis as an absolute value in degrees.
    pub fn y_degrees(self, y: f32) -> Self {
        self.y_radians(y.to_radians())
    }

    /// Specify the orientation around the *z* axis as an absolute value in degrees.
    pub fn z_degrees(self, z: f32) -> Self {
        self.z_radians(z.to_radians())
    }

    /// Specify the orientation around the *x* axis as a number of turns around the axis.
    pub fn x_turns(self, x: f32) -> Self {
        self.x_radians(x * std::f32::consts::TAU)
    }

    /// Specify the orientation around the *y* axis as a number of turns around the axis.
    pub fn y_turns(self, y: f32) -> Self {
        self.y_radians(y * std::f32::consts::TAU)
    }

    /// Specify the orientation around the *z* axis as a number of turns around the axis.
    pub fn z_turns(self, z: f32) -> Self {
        self.z_radians(z * std::f32::consts::TAU)
    }

    /// Specify the orientation along each axis with the given **Vector** of radians.
    ///
    /// This has the same affect as calling `self.x_radians(v.x).y_radians(v.y).z_radians(v.z)`.
    pub fn radians(self, v: Vec3) -> Self {
        orientation::set_orientation(self.draw, self.index, orientation::Update::Radians(v));
        self
    }

    /// Specify the orientation along each axis with the given **Vector** of degrees.
    ///
    /// This has the same affect as calling `self.x_degrees(v.x).y_degrees(v.y).z_degrees(v.z)`.
    pub fn degrees(self, v: Vec3) -> Self {
        self.radians(Vec3::new(v.x.to_radians(), v.y.to_radians(), v.z.to_radians()))
    }

    /// Specify the orientation along each axis with the given **Vector** of "turns".
    ///
    /// This has the same affect as calling `self.x_turns(v.x).y_turns(v.y).z_turns(v.z)`.
    pub fn turns(self, v: Vec3) -> Self {
        self.radians(v * std::f32::consts::TAU)
    }

    /// Specify the orientation with the given **Euler**.
    ///
    /// The euler must be specified in radians.
    pub fn euler(self, e: Vec3) -> Self {
        self.radians(e)
    }

    /// Specify the orientation with the given **Quaternion**.
    pub fn quaternion(self, q: Quat) -> Self {
        orientation::set_orientation(self.draw, self.index, orientation::Update::Quat(q));
        self
    }

    // Higher level methods.

    /// Specify the "pitch" of the orientation in radians.
    ///
    /// This has the same effect as calling `x_radians`.
    pub fn pitch(self, pitch: f32) -> Self {
        self.x_radians(pitch)
    }

    /// Specify the "yaw" of the orientation in radians.
    ///
    /// This has the same effect as calling `y_radians`.
    pub fn yaw(self, yaw: f32) -> Self {
        self.y_radians(yaw)
    }

    /// Specify the "roll" of the orientation in radians.
    ///
    /// This has the same effect as calling `z_radians`.
    pub fn roll(self, roll: f32) -> Self {
        self.z_radians(roll)
    }

    /// Assuming we're looking at a 2D plane, positive values cause a clockwise rotation where the
    /// given value is specified in radians.
    ///
    /// This is equivalent to calling the `z_radians` or `roll` methods.
    pub fn rotate(self, radians: f32) -> Self {
        self.z_radians(radians)
    }
}

// SetFill methods

impl<'a, T> Drawing<'a, T>
where
    T: SetFill,
{
    /// Specify the whole set of fill tessellation options.
    pub fn fill_opts(self, opts: FillOptions) -> Self {
        fill::set_fill(self.draw, self.index, fill::Update::Opts(opts));
        self
    }

    /// Maximum allowed distance to the path when building an approximation.
    pub fn fill_tolerance(self, tolerance: f32) -> Self {
        fill::set_fill(self.draw, self.index, fill::Update::Tolerance(tolerance));
        self
    }

    /// Specify the rule used to determine what is inside and what is outside of the shape.
    ///
    /// Both `EvenOdd` and `NonZero` are passed through to the fill tessellator.
    pub fn fill_rule(self, rule: lyon::tessellation::FillRule) -> Self {
        fill::set_fill(self.draw, self.index, fill::Update::Rule(rule));
        self
    }

    /// Whether to perform a vertical or horizontal traversal of the geometry.
    ///
    /// Default value: `Vertical`.
    pub fn fill_sweep_orientation(self, orientation: lyon::tessellation::Orientation) -> Self {
        fill::set_fill(self.draw, self.index, fill::Update::SweepOrientation(orientation));
        self
    }

    /// A fast path to avoid some expensive operations if the path is known to not have any
    /// self-intersections.
    ///
    /// Do not set this to `false` if the path may have intersecting edges else the tessellator may
    /// panic or produce incorrect results. In doubt, do not change the default value.
    ///
    /// Default value: `true`.
    pub fn handle_intersections(self, b: bool) -> Self {
        fill::set_fill(self.draw, self.index, fill::Update::HandleIntersections(b));
        self
    }
}

// SetStroke methods

impl<'a, T> Drawing<'a, T>
where
    T: SetStroke,
{
    /// The start line cap as specified by the SVG spec.
    pub fn start_cap(self, cap: LineCap) -> Self {
        stroke::set_stroke(self.draw, self.index, stroke::Update::StartCap(cap));
        self
    }

    /// The end line cap as specified by the SVG spec.
    pub fn end_cap(self, cap: LineCap) -> Self {
        stroke::set_stroke(self.draw, self.index, stroke::Update::EndCap(cap));
        self
    }

    /// The start and end line cap as specified by the SVG spec.
    pub fn caps(self, cap: LineCap) -> Self {
        stroke::set_stroke(self.draw, self.index, stroke::Update::Caps(cap));
        self
    }

    /// The stroke for each sub-path does not extend beyond its two endpoints. A zero length
    /// sub-path will therefore not have any stroke.
    pub fn start_cap_butt(self) -> Self {
        self.start_cap(LineCap::Butt)
    }

    /// At the end of each sub-path, the shape representing the stroke will be extended by a
    /// rectangle with the same width as the stroke width and whose length is half of the stroke
    /// width. `line()` submits no vertices when its endpoints are equal, so it does not draw
    /// that square.
    pub fn start_cap_square(self) -> Self {
        self.start_cap(LineCap::Square)
    }

    /// At each end of each sub-path, the stroke extends by a half circle whose radius is half
    /// the stroke width (the diameter equals the stroke width). `line()` submits no vertices
    /// when its endpoints are equal, for this cap and every other.
    pub fn start_cap_round(self) -> Self {
        self.start_cap(LineCap::Round)
    }

    /// The stroke for each sub-path does not extend beyond its two endpoints. A zero length
    /// sub-path will therefore not have any stroke.
    pub fn end_cap_butt(self) -> Self {
        self.end_cap(LineCap::Butt)
    }

    /// At the end of each sub-path, the shape representing the stroke will be extended by a
    /// rectangle with the same width as the stroke width and whose length is half of the stroke
    /// width. `line()` submits no vertices when its endpoints are equal, so it does not draw
    /// that square.
    pub fn end_cap_square(self) -> Self {
        self.end_cap(LineCap::Square)
    }

    /// At each end of each sub-path, the stroke extends by a half circle whose radius is half
    /// the stroke width (the diameter equals the stroke width). `line()` submits no vertices
    /// when its endpoints are equal, for this cap and every other.
    pub fn end_cap_round(self) -> Self {
        self.end_cap(LineCap::Round)
    }

    /// The stroke for each sub-path does not extend beyond its two endpoints. A zero length
    /// sub-path will therefore not have any stroke.
    pub fn caps_butt(self) -> Self {
        self.caps(LineCap::Butt)
    }

    /// At the end of each sub-path, the shape representing the stroke will be extended by a
    /// rectangle with the same width as the stroke width and whose length is half of the stroke
    /// width. `line()` submits no vertices when its endpoints are equal, so it does not draw
    /// that square.
    pub fn caps_square(self) -> Self {
        self.caps(LineCap::Square)
    }

    /// At each end of each sub-path, the stroke extends by a half circle whose radius is half
    /// the stroke width (the diameter equals the stroke width). `line()` submits no vertices
    /// when its endpoints are equal, for this cap and every other.
    pub fn caps_round(self) -> Self {
        self.caps(LineCap::Round)
    }

    /// The way in which lines are joined at the vertices, matching the SVG spec.
    ///
    /// Default value is `Miter`. A corner past the miter limit becomes a bevel; `MiterClip` is opt-in.
    pub fn join(self, join: LineJoin) -> Self {
        stroke::set_stroke(self.draw, self.index, stroke::Update::Join(join));
        self
    }

    /// A sharp corner is to be used to join path segments.
    pub fn join_miter(self) -> Self {
        self.join(LineJoin::Miter)
    }

    /// Same as a `join_miter`, but if the miter limit is exceeded, the miter is clipped at a miter
    /// length equal to the miter limit value multiplied by the stroke width.
    pub fn join_miter_clip(self) -> Self {
        self.join(LineJoin::MiterClip)
    }

    /// A round corner is to be used to join path segments.
    pub fn join_round(self) -> Self {
        self.join(LineJoin::Round)
    }

    /// A bevelled corner is to be used to join path segments. The bevel shape is a triangle that
    /// fills the area between the two stroked segments.
    pub fn join_bevel(self) -> Self {
        self.join(LineJoin::Bevel)
    }

    /// The total stroke_weight (aka width) of the line.
    pub fn stroke_weight(self, stroke_weight: f32) -> Self {
        stroke::set_stroke(self.draw, self.index, stroke::Update::Weight(stroke_weight));
        self
    }

    /// Describes the limit before miter lines will clip, as described in the SVG spec.
    ///
    /// Must be greater than or equal to `1.0`.
    pub fn miter_limit(self, limit: f32) -> Self {
        stroke::set_stroke(self.draw, self.index, stroke::Update::MiterLimit(limit));
        self
    }

    /// Maximum allowed distance to the path when building an approximation.
    pub fn stroke_tolerance(self, tolerance: f32) -> Self {
        stroke::set_stroke(self.draw, self.index, stroke::Update::Tolerance(tolerance));
        self
    }

    /// Specify the full set of stroke options for the path tessellation.
    pub fn stroke_opts(self, opts: StrokeOptions) -> Self {
        stroke::set_stroke(self.draw, self.index, stroke::Update::Opts(opts));
        self
    }
}

// Finish the drawing at the given index.
//
// Shared between the **finish** method and the **Drawing**'s **Drop** implementation. Blocks on
// the state lock: systems running in parallel may draw through the same **Draw**.
fn finish_drawing(draw: &Draw, key: usize) {
    let Some(slot) = take_open(draw, key) else {
        return;
    };
    let mut open = slot.borrow_mut();
    let Some(primitive) = open.primitive.take() else {
        return;
    };
    let texture = open.texture.take();
    let (index, epoch) = (open.index, open.epoch);
    drop(open);
    draw.commit_slot(index, epoch, primitive, texture);
}

// Mutate the primitive stored on the drawing at `index`.
//
// The function is only applied if the node has not yet been finished. It does not take the
// shared state lock.
pub(crate) fn with_primitive(draw: &Draw, index: usize, f: impl FnOnce(&mut Primitive)) {
    let Some(slot) = lookup_open(draw, index) else {
        return;
    };
    if let Some(primitive) = slot.borrow_mut().primitive.as_mut() {
        f(primitive);
    }
}

// The same as `with_primitive`, but passes ownership of the primitive to the given function
// along with mutable access to the intermediary vertex buffers. This is useful for type-state
// transitions and for primitives with an arbitrary number of vertices.
//
// The primitive is taken out of the slot before `f` runs, so `f` may draw other primitives
// (their iterators are collected by the caller before this lock-free call).
pub(crate) fn with_primitive_ctxt(
    draw: &Draw,
    index: usize,
    f: impl FnOnce(Primitive, DrawingContext) -> Primitive,
) {
    let Some(slot) = lookup_open(draw, index) else {
        return;
    };
    let prim = slot.borrow_mut().primitive.take();
    let Some(prim) = prim else {
        return;
    };
    let new = {
        let mut intermediary_state = write(&draw.intermediary);
        let ctxt = DrawingContext::from_intermediary_state(&mut intermediary_state);
        f(prim, ctxt)
    };
    slot.borrow_mut().primitive = Some(new);
}
