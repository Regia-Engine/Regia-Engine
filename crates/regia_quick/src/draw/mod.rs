// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/mod.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
//! A simple API for drawing 2D graphics over the frame.
//!
//! See [`Draw`] for details.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use lyon::path::PathEvent;
use regia_core::math::geom;

use self::primitive::Primitive;
pub use self::{
    background::Background,
    drawing::{Drawing, DrawingContext},
    theme::Theme,
};
use crate::prelude_internal::*;

/// Clearing the window with a colour.
pub mod background;
mod drawing;
pub(crate) mod fast;
pub mod mesh;
/// The drawable primitives and their builder methods.
pub mod primitive;
pub mod properties;
pub(crate) mod render;
/// Default colours per primitive.
pub mod theme;

/// How a drawing blends over what is already drawn.
pub use regia_render::ImmediateBlend as BlendMode;

/// A registered image that drawings can be textured with ([`crate::QuickImages`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QuickImage(pub u64);

/// A simple API for drawing 2D graphics.
///
/// [Draw] provides a simple way to compose together geometry and text with custom colours and
/// textures and draw them to the screen. A suite of methods have been provided for drawing
/// polygons, paths, meshes and text in an accessible-yet-efficient manner.
///
/// [Draw] can also be used to create new [Draw] instances that refer to the same inner draw
/// state but are slightly different from one another. E.g. `draw.rotate(radians)` produces a new
/// [Draw] instance where all drawings will be rotated by the given amount. `draw.x(x)` produces
/// a new [Draw] instance where all drawings are translated along the *x* axis by the given
/// amount.
///
/// Coordinates are logical window units with the origin at the window centre, +X right and +Y
/// up. What is drawn in a frame is shown over that frame (after tonemapping, under the runtime
/// UI) and cleared for the next one.
#[derive(Resource, Clone)]
pub struct Draw {
    /// The state of the [Draw].
    ///
    /// State is shared between this [Draw] instance and all other [Draw] instances that were
    /// produced by cloning or changing transform, scissor or blend mode.
    ///
    /// We use a lock in order to avoid requiring a `mut` handle to a `draw`. The primary
    /// purpose of a [Draw] is to be an easy-as-possible, high-level API for drawing stuff. In
    /// order to be friendlier to new users, we want to avoid them having to think about mutability
    /// and focus on creativity. Rust-lang nuances can come later.
    pub state: Arc<RwLock<State>>,
    /// Scratch buffers shared with [`State::intermediary_state`]. Property setters that only
    /// change a colour or a position never lock this.
    pub(crate) intermediary: Arc<RwLock<IntermediaryState>>,
    /// Times [`Self::write_state`] has taken the shared state write lock.
    state_writes: Arc<AtomicU64>,
    /// Times the command list was replaced by a freshly allocated vector.
    command_allocs: Arc<AtomicU64>,
    /// The current context of this [Draw] instance.
    context: DrawContext,
}

/// The current transform, blend mode and scissor of a [Draw] instance.
#[derive(Clone, Debug, PartialEq)]
pub struct DrawContext {
    /// Applied to every drawing (logical units).
    pub transform: Mat4,
    /// Blend mode of every drawing.
    pub blend: BlendMode,
    /// Clip rectangle in logical units (centre origin, +Y up); `None` = the whole window.
    pub scissor: Option<geom::Rect>,
}

impl Default for DrawContext {
    fn default() -> Self {
        Self {
            transform: Mat4::IDENTITY,
            blend: BlendMode::Alpha,
            scissor: None,
        }
    }
}

/// Commands generated by drawing that instruct how to build the frame's triangles.
#[derive(Clone, Debug)]
pub enum DrawCommand {
    /// Draw a primitive, optionally textured.
    Primitive(Primitive, Option<QuickImage>),
    /// A change in the drawing context occurred.
    Context(DrawContext),
    /// Fill the whole window with a colour (covering everything drawn before).
    BackgroundColor(Color),
}

/// The inner state of the [Draw] type.
///
/// The [Draw] type stores its [State] behind a [RwLock] - a type used for moving mutability
/// checks from compile time to runtime. We do this in order to avoid requiring a `mut` handle to a
/// `draw`.
pub struct State {
    /// The last context used to draw an image, used to detect changes and emit commands for them.
    last_draw_context: Option<DrawContext>,
    /// If `Some`, the frame was cleared with the given colour.
    background_color: Option<Color>,
    /// Primitives that are in the process of being drawn.
    ///
    /// Keys are indices into the `draw_commands` Vec.
    drawing: SlotMap<Primitive>,
    /// Textures of drawings in progress.
    textures: SlotMap<QuickImage>,
    /// The list of recorded draw commands.
    ///
    /// An element may be `None` if it is a primitive in the process of being drawn.
    pub(crate) draw_commands: Vec<Option<DrawCommand>>,
    /// State made accessible via the `DrawingContext`.
    pub(crate) intermediary_state: Arc<RwLock<IntermediaryState>>,
    /// The theme containing default values.
    pub(crate) theme: Theme,
    /// Bumped by [`State::reset`] and by draining the command list. A drawing reserved under an
    /// older epoch is dropped on finish instead of writing into a recycled slot.
    epoch: u64,
}

/// State made accessible via the `DrawingContext`.
#[derive(Clone, Debug, Default)]
pub struct IntermediaryState {
    /// Buffers of vertex data that may be re-used for paths, meshes, etc between view calls.
    pub intermediary_mesh: mesh::Mesh,
    /// A re-usable buffer for collecting path events.
    pub path_event_buffer: Vec<PathEvent>,
    /// A re-usable buffer for collecting polyline points vertex data.
    pub path_points_vertex_buffer: Vec<(Vec2, Color, Vec2)>,
    /// A buffer containing all text.
    pub text_buffer: String,
}

impl IntermediaryState {
    /// Clears every buffer, keeping the allocations.
    pub fn reset(&mut self) {
        self.intermediary_mesh.clear();
        self.path_event_buffer.clear();
        self.path_points_vertex_buffer.clear();
        self.text_buffer.clear();
    }
}

/// Hashes a command slot index as itself: the keys are small distinct integers, so SipHash
/// would only cost time on every builder call.
#[derive(Default, Clone, Copy)]
struct SlotHasher(u64);

impl std::hash::Hasher for SlotHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 << 8) | u64::from(*b);
        }
    }

    fn write_usize(&mut self, i: usize) {
        self.0 = i as u64;
    }
}

/// Map from command slot index.
type SlotMap<T> = HashMap<usize, T, std::hash::BuildHasherDefault<SlotHasher>>;

/// Write access that survives a poisoned lock (a panic inside a builder must not take the frame
/// down with it).
pub(crate) fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

/// Read access that survives a poisoned lock.
pub(crate) fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    // Resets all state within the `Draw` instance.
    fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.last_draw_context = None;
        self.background_color = None;
        self.drawing.clear();
        self.textures.clear();
        // `clear` keeps the allocation for the next frame.
        self.draw_commands.clear();
        write(&self.intermediary_state).reset();
    }

    // Drain any remaining `drawing`s and insert them as draw commands.
    fn finish_remaining_drawings(&mut self) {
        let mut drawing = std::mem::take(&mut self.drawing);
        for (index, primitive) in drawing.drain() {
            self.insert_draw_command(index, primitive);
        }
        std::mem::swap(&mut self.drawing, &mut drawing);
    }

    // Insert the draw primitive command at the given index.
    fn insert_draw_command(&mut self, index: usize, prim: Primitive) {
        let texture = self.textures.remove(&index);
        if let Some(elem) = self.draw_commands.get_mut(index) {
            *elem = Some(DrawCommand::Primitive(prim, texture));
        }
    }

    /// The background colour set this frame, if any.
    #[must_use]
    pub fn background_color(&self) -> Option<Color> {
        self.background_color
    }

    /// The theme supplying default colours.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Replaces the theme.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }
}

impl Default for Draw {
    fn default() -> Self {
        Self::new()
    }
}

impl Draw {
    /// A new, empty [Draw].
    #[must_use]
    pub fn new() -> Self {
        let intermediary = Arc::new(RwLock::new(IntermediaryState::default()));
        let state = State {
            intermediary_state: intermediary.clone(),
            ..State::default()
        };
        Draw {
            state: Arc::new(RwLock::new(state)),
            intermediary,
            state_writes: Arc::new(AtomicU64::new(0)),
            command_allocs: Arc::new(AtomicU64::new(0)),
            context: DrawContext::default(),
        }
    }

    /// Takes the shared state write lock and counts the acquisition.
    ///
    /// Reserving a drawing and finishing it each take this lock once. Colour, position and the
    /// other property setters mutate the in-progress primitive and do not.
    pub(crate) fn write_state(&self) -> RwLockWriteGuard<'_, State> {
        self.state_writes.fetch_add(1, Ordering::Relaxed);
        write(&self.state)
    }

    /// How many times this draw's shared state write lock has been taken.
    ///
    /// Cloned draws (a rotated or translated view of the same draw) share the counter. Property
    /// setters do not take the lock.
    #[must_use]
    pub fn state_write_count(&self) -> u64 {
        self.state_writes.load(Ordering::Relaxed)
    }

    /// Times the command list was replaced by a freshly allocated vector.
    ///
    /// Draining into a reused buffer does not increment this. The count is shared by every clone
    /// of this draw.
    #[must_use]
    pub fn command_allocs(&self) -> u64 {
        self.command_allocs.load(Ordering::Relaxed)
    }

    /// Resets all state within the `Draw` instance.
    pub fn reset(&self) {
        self.write_state().reset();
    }

    /// The current context (transform, blend, scissor) of this instance.
    #[must_use]
    pub fn context(&self) -> &DrawContext {
        &self.context
    }

    // Context changes.

    /// Produce a new [Draw] instance transformed by the given transform matrix.
    ///
    /// The resulting [Draw] instance will be have a transform equal to the new transform applied
    /// to the existing transform.
    #[must_use]
    pub fn transform(&self, transform_matrix: Mat4) -> Self {
        let mut context = self.context.clone();
        context.transform *= transform_matrix;
        self.with_context(context)
    }

    /// Translate the position of the origin by the given translation vector.
    #[must_use]
    pub fn translate(&self, v: Vec3) -> Self {
        self.transform(Mat4::from_translation(v))
    }

    /// Translate the position of the origin by the given translation vector.
    ///
    /// This method is short for `translate`.
    #[must_use]
    pub fn xyz(&self, v: Vec3) -> Self {
        self.translate(v)
    }

    /// Translate the position of the origin by the given translation vector.
    #[must_use]
    pub fn xy(&self, v: Vec2) -> Self {
        self.xyz(v.extend(0.0))
    }

    /// Translate the position of the origin by the given amount across each axis.
    #[must_use]
    pub fn x_y_z(&self, x: f32, y: f32, z: f32) -> Self {
        self.xyz([x, y, z].into())
    }

    /// Translate the position of the origin by the given amount across each axis.
    #[must_use]
    pub fn x_y(&self, x: f32, y: f32) -> Self {
        self.xy([x, y].into())
    }

    /// Translate the position of the origin along the x axis.
    #[must_use]
    pub fn x(&self, x: f32) -> Self {
        self.x_y(x, 0.0)
    }

    /// Translate the position of the origin along the y axis.
    #[must_use]
    pub fn y(&self, y: f32) -> Self {
        self.x_y(0.0, y)
    }

    /// Translate the position of the origin along the z axis.
    #[must_use]
    pub fn z(&self, z: f32) -> Self {
        self.x_y_z(0.0, 0.0, z)
    }

    /// Produce a new [Draw] instance where the contents are scaled uniformly by the given value.
    #[must_use]
    pub fn scale(&self, s: f32) -> Self {
        self.scale_axes(Vec3::new(s, s, s))
    }

    /// Produce a new [Draw] instance where the contents are scaled by the given amount across
    /// each axis.
    #[must_use]
    pub fn scale_axes(&self, v: Vec3) -> Self {
        self.transform(Mat4::from_scale(v))
    }

    /// Produce a new [Draw] instance where the contents are scaled by the given amount along the
    /// x axis
    #[must_use]
    pub fn scale_x(&self, s: f32) -> Self {
        self.scale_axes(Vec3::new(s, 1.0, 1.0))
    }

    /// Produce a new [Draw] instance where the contents are scaled by the given amount along the
    /// y axis
    #[must_use]
    pub fn scale_y(&self, s: f32) -> Self {
        self.scale_axes(Vec3::new(1.0, s, 1.0))
    }

    /// Produce a new [Draw] instance where the contents are scaled by the given amount along the
    /// z axis
    #[must_use]
    pub fn scale_z(&self, s: f32) -> Self {
        self.scale_axes(Vec3::new(1.0, 1.0, s))
    }

    /// The given vector is interpreted as a Euler angle in radians and a transform is applied
    /// accordingly.
    #[must_use]
    pub fn euler(&self, euler: Vec3) -> Self {
        self.transform(Mat4::from_euler(EulerRot::XYZ, euler.x, euler.y, euler.z))
    }

    /// Specify the orientation with the given **Quaternion**.
    #[must_use]
    pub fn quaternion(&self, q: Quat) -> Self {
        self.transform(Mat4::from_quat(q))
    }

    /// Specify the orientation along each axis with the given **Vector** of radians.
    ///
    /// This currently has the same affect as calling `euler`.
    #[must_use]
    pub fn radians(&self, v: Vec3) -> Self {
        self.euler(v)
    }

    /// Specify the orientation around the *x* axis in radians.
    #[must_use]
    pub fn x_radians(&self, x: f32) -> Self {
        self.radians(Vec3::new(x, 0.0, 0.0))
    }

    /// Specify the orientation around the *y* axis in radians.
    #[must_use]
    pub fn y_radians(&self, y: f32) -> Self {
        self.radians(Vec3::new(0.0, y, 0.0))
    }

    /// Specify the orientation around the *z* axis in radians.
    #[must_use]
    pub fn z_radians(&self, z: f32) -> Self {
        self.radians(Vec3::new(0.0, 0.0, z))
    }

    /// Specify the orientation along each axis with the given **Vector** of degrees.
    #[must_use]
    pub fn degrees(&self, v: Vec3) -> Self {
        self.radians(Vec3::new(v.x.to_radians(), v.y.to_radians(), v.z.to_radians()))
    }

    /// Specify the orientation around the *x* axis in degrees.
    #[must_use]
    pub fn x_degrees(&self, x: f32) -> Self {
        self.x_radians(x.to_radians())
    }

    /// Specify the orientation around the *y* axis in degrees.
    #[must_use]
    pub fn y_degrees(&self, y: f32) -> Self {
        self.y_radians(y.to_radians())
    }

    /// Specify the orientation around the *z* axis in degrees.
    #[must_use]
    pub fn z_degrees(&self, z: f32) -> Self {
        self.z_radians(z.to_radians())
    }

    /// Specify the orientation along each axis with the given **Vector** of turns.
    #[must_use]
    pub fn turns(&self, v: Vec3) -> Self {
        self.radians(v * std::f32::consts::TAU)
    }

    /// Specify the orientation around the *x* axis as a number of turns around the axis.
    #[must_use]
    pub fn x_turns(&self, x: f32) -> Self {
        self.x_radians(x * std::f32::consts::TAU)
    }

    /// Specify the orientation around the *y* axis as a number of turns around the axis.
    #[must_use]
    pub fn y_turns(&self, y: f32) -> Self {
        self.y_radians(y * std::f32::consts::TAU)
    }

    /// Specify the orientation around the *z* axis as a number of turns around the axis.
    #[must_use]
    pub fn z_turns(&self, z: f32) -> Self {
        self.z_radians(z * std::f32::consts::TAU)
    }

    /// Specify the "pitch" of the orientation in radians.
    ///
    /// This has the same effect as calling `x_radians`.
    #[must_use]
    pub fn pitch(&self, pitch: f32) -> Self {
        self.x_radians(pitch)
    }

    /// Specify the "yaw" of the orientation in radians.
    ///
    /// This has the same effect as calling `y_radians`.
    #[must_use]
    pub fn yaw(&self, yaw: f32) -> Self {
        self.y_radians(yaw)
    }

    /// Specify the "roll" of the orientation in radians.
    ///
    /// This has the same effect as calling `z_radians`.
    #[must_use]
    pub fn roll(&self, roll: f32) -> Self {
        self.z_radians(roll)
    }

    /// Assuming we're looking at a 2D plane, positive values cause a counter-clockwise rotation
    /// (+Y is up) where the given value is specified in radians.
    ///
    /// This is equivalent to calling the `z_radians` or `roll` methods.
    #[must_use]
    pub fn rotate(&self, radians: f32) -> Self {
        self.z_radians(radians)
    }

    /// Produce a new [Draw] instance whose drawings blend with `mode`.
    #[must_use]
    pub fn blend(&self, mode: BlendMode) -> Self {
        let mut context = self.context.clone();
        context.blend = mode;
        self.with_context(context)
    }

    /// Produce a new [Draw] instance whose drawings are clipped to `rect` (logical units, centre
    /// origin, +Y up; not affected by the transform).
    #[must_use]
    pub fn scissor(&self, rect: geom::Rect) -> Self {
        let mut context = self.context.clone();
        context.scissor = Some(rect);
        self.with_context(context)
    }

    /// Produce a new [Draw] instance without a clip rectangle.
    #[must_use]
    pub fn no_scissor(&self) -> Self {
        let mut context = self.context.clone();
        context.scissor = None;
        self.with_context(context)
    }

    /// Produce a new [Draw] instance with the given context.
    fn with_context(&self, context: DrawContext) -> Draw {
        Draw {
            state: self.state.clone(),
            intermediary: self.intermediary.clone(),
            state_writes: self.state_writes.clone(),
            command_allocs: self.command_allocs.clone(),
            context,
        }
    }

    // Primitives.

    /// Specify a color with which the background should be cleared.
    ///
    /// The quick layer draws over the finished (tonemapped) window image, so an opaque
    /// background also covers the 3D view behind it; use a translucent color to keep it visible.
    pub fn background(&self) -> Background<'_> {
        background::new(self)
    }

    /// Add the given type to be drawn.
    pub fn a<T>(&self, primitive: T) -> Drawing<'_, T>
    where
        T: Into<Primitive>,
    {
        let primitive = primitive.into();
        let (index, epoch) = self.reserve_slot();
        let (key, slot) = drawing::open_drawing(self, index, epoch, primitive);
        drawing::new(self, key, slot)
    }

    /// Reserves a command slot and returns `(index, epoch)`. The primitive stays with the
    /// [`Drawing`] until it is finished.
    pub(crate) fn reserve_slot(&self) -> (usize, u64) {
        let mut state = self.write_state();
        if state.last_draw_context.as_ref() != Some(&self.context) {
            state
                .draw_commands
                .push(Some(DrawCommand::Context(self.context.clone())));
            state.last_draw_context = Some(self.context.clone());
        }
        let index = state.draw_commands.len();
        state.draw_commands.push(None);
        let epoch = state.epoch;
        (index, epoch)
    }

    /// Writes a finished primitive into its reserved slot when `epoch` is still current.
    pub(crate) fn commit_slot(
        &self,
        index: usize,
        epoch: u64,
        primitive: Primitive,
        texture: Option<QuickImage>,
    ) {
        let mut state = self.write_state();
        if state.epoch != epoch {
            return;
        }
        if let Some(elem) = state.draw_commands.get_mut(index)
            && elem.is_none()
        {
            *elem = Some(DrawCommand::Primitive(primitive, texture));
        }
    }

    /// Begin drawing a **Path**.
    pub fn path(&self) -> Drawing<'_, primitive::PathInit> {
        self.a(Default::default())
    }

    /// Begin drawing an **Ellipse**.
    pub fn ellipse(&self) -> Drawing<'_, primitive::Ellipse> {
        self.a(Default::default())
    }

    /// Begin drawing a **Line**.
    pub fn line(&self) -> Drawing<'_, primitive::Line> {
        self.a(Default::default())
    }

    /// Begin drawing an **Arrow**.
    pub fn arrow(&self) -> Drawing<'_, primitive::Arrow> {
        self.a(Default::default())
    }

    /// Begin drawing a **Quad**.
    pub fn quad(&self) -> Drawing<'_, primitive::Quad> {
        self.a(Default::default())
    }

    /// Begin drawing a **Rect**.
    pub fn rect(&self) -> Drawing<'_, primitive::Rect> {
        self.a(Default::default())
    }

    /// Begin drawing a **Triangle**.
    pub fn tri(&self) -> Drawing<'_, primitive::Tri> {
        self.a(Default::default())
    }

    /// Begin drawing a **Polygon**.
    pub fn polygon(&self) -> Drawing<'_, primitive::PolygonInit> {
        self.a(Default::default())
    }

    /// Begin drawing a **Mesh**.
    pub fn mesh(&self) -> Drawing<'_, primitive::mesh::Vertexless> {
        self.a(Default::default())
    }

    /// Begin drawing a **Polyline**.
    ///
    /// Note that this is simply short-hand for `draw.path().stroke()`
    pub fn polyline(&self) -> Drawing<'_, primitive::PathStroke> {
        self.path().stroke()
    }

    /// Begin drawing a **Text**.
    pub fn text(&self, s: &str) -> Drawing<'_, primitive::Text> {
        let text = {
            let mut intermediary_state = write(&self.intermediary);
            let ctxt = DrawingContext::from_intermediary_state(&mut intermediary_state);
            primitive::text::Text::new(ctxt, s)
        };
        self.a(text)
    }

    /// Finish any drawings-in-progress and produce an iterator draining the inner draw commands
    /// and yielding them by value.
    ///
    /// Drawings still open on another thread are not committed: the primitive lives in an
    /// `Rc` on the thread that draws, so a drain here cannot move it; the epoch bump makes the
    /// late finish a no-op instead of writing into a recycled slot (corr-low, ADR-5789).
    pub fn drain_commands(&self) -> impl Iterator<Item = DrawCommand> {
        drawing::commit_open_drawings(self);
        let cmds = {
            let mut state = self.write_state();
            state.finish_remaining_drawings();
            state.epoch = state.epoch.wrapping_add(1);
            // Replacing the list drops its capacity. Count that allocation.
            self.command_allocs.fetch_add(1, Ordering::Relaxed);
            let empty = Vec::with_capacity(state.draw_commands.len());
            std::mem::replace(&mut state.draw_commands, empty)
        };
        cmds.into_iter().flatten()
    }

    /// Moves finished commands into `out` (cleared first). `draw_commands` keeps its capacity,
    /// and `out` keeps its capacity when it already fits.
    pub fn drain_commands_into(&self, out: &mut Vec<DrawCommand>) {
        drawing::commit_open_drawings(self);
        let mut state = self.write_state();
        state.finish_remaining_drawings();
        state.epoch = state.epoch.wrapping_add(1);
        let cap = out.capacity();
        out.clear();
        let needed = state.draw_commands.iter().filter(|c| c.is_some()).count();
        if cap < needed {
            self.command_allocs.fetch_add(1, Ordering::Relaxed);
        }
        out.extend(state.draw_commands.drain(..).flatten());
    }

    /// Drain any remaining `drawing`s and convert them to draw commands.
    pub fn finish_remaining_drawings(&self) {
        drawing::commit_open_drawings(self);
        self.write_state().finish_remaining_drawings()
    }

    /// True when nothing has been drawn since the last reset.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let state = read(&self.state);
        // A reserved slot (`None`) is a drawing still in progress: its primitive lives on the
        // `Drawing` until it finishes, on whichever thread draws it.
        state.drawing.is_empty()
            && state.draw_commands.iter().all(|c| {
                !matches!(
                    c,
                    None | Some(DrawCommand::Primitive(..) | DrawCommand::BackgroundColor(_))
                )
            })
    }
}

impl Default for State {
    fn default() -> Self {
        State {
            last_draw_context: None,
            background_color: None,
            drawing: SlotMap::default(),
            textures: SlotMap::default(),
            draw_commands: Vec::new(),
            intermediary_state: Arc::new(RwLock::new(IntermediaryState::default())),
            theme: Theme::default(),
            epoch: 1,
        }
    }
}
