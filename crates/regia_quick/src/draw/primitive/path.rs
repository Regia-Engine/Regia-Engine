// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/primitive/path.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
use crate::draw::mesh::Mesh;
use crate::prelude_internal::*;
use lyon::path::PathEvent;
use lyon::tessellation::{FillOptions, FillTessellator, StrokeOptions, StrokeTessellator};

use crate::draw::primitive::Primitive;
use crate::draw::properties::spatial::{orientation, position};
use crate::draw::properties::{SetColor, SetFill, SetOrientation, SetPosition, SetStroke};
use crate::draw::{self, Drawing, DrawingContext, drawing};

/// A set of path tessellation options (FillOptions or StrokeOptions).
pub trait TessellationOptions {
    /// The tessellator for which the options are built.
    type Tessellator;
    /// Convert the typed options instance to a dynamic one.
    fn into_options(self) -> Options;
}

#[derive(Clone, Debug)]
pub(crate) enum PathEventSource {
    /// Fetch events from `path_events_buffer`.
    Buffered(std::ops::Range<usize>),
    /// Generate events from the `path_points_vertex_buffer`.
    Vertex {
        range: std::ops::Range<usize>,
        close: bool,
    },
}

pub(crate) enum PathEventSourceIter<'a> {
    Events(&'a mut dyn Iterator<Item = PathEvent>),
    Vertex {
        points: &'a mut dyn Iterator<Item = (Vec2, Color, Vec2)>,
        close: bool,
    },
}

/// The beginning of the path building process, prior to choosing the tessellation mode (fill or
/// stroke).
#[derive(Clone, Debug, Default)]
pub struct PathInit;

/// A path drawing context ready to specify tessellation options.
#[derive(Clone, Debug, Default)]
pub struct PathOptions<T> {
    pub(crate) opts: T,
    pub(crate) color: Option<Color>,
    pub(crate) position: position::Properties,
    pub(crate) orientation: orientation::Properties,
}

/// Mutable access to stroke and fill tessellators.
pub struct Tessellators<'a> {
    /// The fill tessellator.
    pub fill: &'a mut FillTessellator,
    /// The stroke tessellator.
    pub stroke: &'a mut StrokeTessellator,
}

/// A filled path drawing context.
pub type PathFill = PathOptions<FillOptions>;

/// A stroked path drawing context.
pub type PathStroke = PathOptions<StrokeOptions>;

/// Properties related to drawing a **Path**.
#[derive(Clone, Debug)]
pub struct Path {
    color: Option<Color>,
    position: position::Properties,
    orientation: orientation::Properties,
    path_event_src: PathEventSource,
    options: Options,
}

/// The initial drawing context for a path.
pub type DrawingPathInit<'a> = Drawing<'a, PathInit>;

/// The drawing context for a path in the tessellation options state.
pub type DrawingPathOptions<'a, T> = Drawing<'a, PathOptions<T>>;

/// The drawing context for a stroked path, prior to path event submission.
pub type DrawingPathStroke<'a> = Drawing<'a, PathStroke>;

/// The drawing context for a filled path, prior to path event submission.
pub type DrawingPathFill<'a> = Drawing<'a, PathFill>;

/// The drawing context for a polyline whose vertices have been specified.
pub type DrawingPath<'a> = Drawing<'a, Path>;

/// Dynamically distinguish between fill and stroke tessellation options.
#[derive(Clone, Debug)]
pub enum Options {
    /// Fill tessellation.
    Fill(FillOptions),
    /// Stroke tessellation.
    Stroke(StrokeOptions),
}

impl Options {
    /// A stroke whose width is zero, negative or not finite covers no pixels. lyon's stroke
    /// tessellator turns a zero width into NaN positions, so such strokes are not tessellated.
    fn strokes_nothing(&self) -> bool {
        matches!(self, Options::Stroke(o) if !o.line_width.is_finite() || o.line_width <= 0.0)
    }
}

impl PathInit {
    /// Specify that we want to use fill tessellation for the path.
    ///
    /// The returned building context allows for specifying the fill tessellation options.
    pub fn fill(self) -> PathFill {
        let opts = FillOptions::default();
        PathFill::new(opts)
    }

    /// Specify that we want to use stroke tessellation for the path.
    ///
    /// The returned building context allows for specifying the stroke tessellation options.
    pub fn stroke(self) -> PathStroke {
        let opts = Default::default();
        PathStroke::new(opts)
    }
}

impl<T> PathOptions<T> {
    /// Initialise the `PathOptions` builder.
    pub fn new(opts: T) -> Self {
        let orientation = Default::default();
        let position = Default::default();
        let color = Default::default();
        PathOptions {
            opts,
            orientation,
            position,
            color,
        }
    }
}

impl PathFill {
    /// Maximum allowed distance to the path when building an approximation.
    ///
    /// This method is shorthand for the `fill_tolerance` method.
    pub fn tolerance(self, tolerance: f32) -> Self {
        self.fill_tolerance(tolerance)
    }

    /// Specify the rule used to determine what is inside and what is outside of the shape.
    ///
    /// Both `EvenOdd` and `NonZero` are passed through to the fill tessellator.
    ///
    /// This method is shorthand for the `fill_rule` method.
    pub fn rule(self, rule: lyon::tessellation::FillRule) -> Self {
        self.fill_rule(rule)
    }
}

impl PathStroke {
    /// Short-hand for the `stroke_weight` method.
    pub fn weight(self, weight: f32) -> Self {
        self.stroke_weight(weight)
    }

    /// Short-hand for the `stroke_tolerance` method.
    pub fn tolerance(self, tolerance: f32) -> Self {
        self.stroke_tolerance(tolerance)
    }
}

impl<T> PathOptions<T>
where
    T: TessellationOptions,
{
    /// Submit the path events to be tessellated.
    pub(crate) fn events<I>(self, ctxt: DrawingContext, events: I) -> Path
    where
        I: IntoIterator<Item = PathEvent>,
    {
        let DrawingContext {
            path_event_buffer, ..
        } = ctxt;
        let start = path_event_buffer.len();
        path_event_buffer.extend(events);
        let end = path_event_buffer.len();
        Path::new(
            self.position,
            self.orientation,
            self.color,
            PathEventSource::Buffered(start..end),
            self.opts.into_options(),
        )
    }

    /// Consumes an iterator of points and converts them to an iterator yielding path events.
    pub fn points<I>(self, ctxt: DrawingContext, points: I) -> Path
    where
        I: IntoIterator,
        I::Item: Into<Vec2>,
    {
        self.points_inner(ctxt, false, points)
    }

    /// Consumes an iterator of points and converts them to an iterator yielding path events.
    ///
    /// Closes the start and end points.
    pub fn points_closed<I>(self, ctxt: DrawingContext, points: I) -> Path
    where
        I: IntoIterator,
        I::Item: Into<Vec2>,
    {
        self.points_inner(ctxt, true, points)
    }

    /// Submit path events as a polyline of colored points.
    pub fn vertices<I, P, C, U>(self, ctxt: DrawingContext, points: I) -> Path
    where
        I: IntoIterator<Item = (P, C, U)>,
        P: Into<Vec2>,
        C: Into<Color>,
        U: Into<Vec2>,
    {
        self.points_vertex_inner(ctxt, false, points)
    }

    /// Submit path events as a polyline of colored points.
    pub fn vertices_closed<I, P, C, U>(self, ctxt: DrawingContext, points: I) -> Path
    where
        I: IntoIterator<Item = (P, C, U)>,
        P: Into<Vec2>,
        C: Into<Color>,
        U: Into<Vec2>,
    {
        self.points_vertex_inner(ctxt, true, points)
    }

    // Consumes an iterator of points and converts them to an iterator yielding events.
    fn points_inner<I>(self, ctxt: DrawingContext, close: bool, points: I) -> Path
    where
        I: IntoIterator,
        I::Item: Into<Vec2>,
    {
        let iter = points
            .into_iter()
            .map(Into::into)
            .map(|p| lyon::math::point(p.x, p.y));
        let events = lyon::path::iterator::FromPolyline::new(close, iter);
        self.events(ctxt, events)
    }

    // Consumes an iterator of points and converts them to an iterator yielding events.
    fn points_vertex_inner<I, P, C, U>(self, ctxt: DrawingContext, close: bool, points: I) -> Path
    where
        I: IntoIterator<Item = (P, C, U)>,
        P: Into<Vec2>,
        C: Into<Color>,
        U: Into<Vec2>,
    {
        let DrawingContext {
            path_points_vertex_buffer: path_points_colored_buffer,
            ..
        } = ctxt;
        let start = path_points_colored_buffer.len();
        let points = points.into_iter().map(|(p, c, u)| (p.into(), c.into(), u.into()));

        path_points_colored_buffer.extend(points);

        let end = path_points_colored_buffer.len();
        let path_event_src = PathEventSource::Vertex {
            range: start..end,
            close,
        };
        Path::new(
            self.position,
            self.orientation,
            self.color,
            path_event_src,
            self.opts.into_options(),
        )
    }
}

#[allow(clippy::too_many_arguments)] // the ported nannou signature
pub(crate) fn render_path_events<I>(
    events: I,
    color: Option<Color>,
    transform: Mat4,
    options: Options,
    theme: &draw::Theme,
    theme_prim: &draw::theme::Primitive,
    fill_tessellator: &mut lyon::tessellation::FillTessellator,
    stroke_tessellator: &mut lyon::tessellation::StrokeTessellator,
    mesh: &mut Mesh,
) where
    I: IntoIterator<Item = lyon::path::PathEvent>,
{
    if options.strokes_nothing() {
        return;
    }
    let res = match options {
        Options::Fill(options) => {
            let color = color.unwrap_or_else(|| theme.fill(theme_prim));
            let mut mesh_builder = draw::mesh::MeshBuilder::single_color(mesh, transform, color);
            fill_tessellator.tessellate(events, &options, &mut mesh_builder)
        }
        Options::Stroke(options) => {
            let color = color.unwrap_or_else(|| theme.stroke(theme_prim));
            let mut mesh_builder = draw::mesh::MeshBuilder::single_color(mesh, transform, color);
            stroke_tessellator.tessellate(events, &options, &mut mesh_builder)
        }
    };
    if let Err(err) = res {
        tracing::debug!(target: "regia_quick", "failed to tessellate path: {err:?}");
    }
}

#[allow(clippy::too_many_arguments)] // the ported nannou signature
pub(crate) fn render_path_points_themed<I>(
    points_themed: I,
    close: bool,
    color: Option<Color>,
    transform: Mat4,
    options: Options,
    theme: &draw::Theme,
    theme_prim: &draw::theme::Primitive,
    fill_tessellator: &mut lyon::tessellation::FillTessellator,
    stroke_tessellator: &mut lyon::tessellation::StrokeTessellator,
    mesh: &mut Mesh,
) where
    I: IntoIterator<Item = (Vec2, Vec2)>,
{
    if options.strokes_nothing() {
        return;
    }
    let path = match points_themed_to_lyon_path(points_themed, close) {
        None => return,
        Some(p) => p,
    };

    // Extend the mesh with the built path.
    let res = match options {
        Options::Fill(options) => {
            let color = color.unwrap_or_else(|| theme.fill(theme_prim));
            let mut mesh_builder = draw::mesh::MeshBuilder::single_color(mesh, transform, color);
            fill_tessellator.tessellate_with_ids(
                path.id_iter(),
                &path,
                Some(&path),
                &options,
                &mut mesh_builder,
            )
        }
        Options::Stroke(options) => {
            let color = color.unwrap_or_else(|| theme.stroke(theme_prim));
            let mut mesh_builder = draw::mesh::MeshBuilder::single_color(mesh, transform, color);
            stroke_tessellator.tessellate_with_ids(
                path.id_iter(),
                &path,
                Some(&path),
                &options,
                &mut mesh_builder,
            )
        }
    };
    if let Err(err) = res {
        tracing::debug!(target: "regia_quick", "failed to tessellate path: {err:?}");
    }
}

#[allow(clippy::too_many_arguments)] // the ported nannou signature
pub(crate) fn render_path_vertex<I>(
    points_vertex: I,
    close: bool,
    transform: Mat4,
    options: Options,
    fill_tessellator: &mut FillTessellator,
    stroke_tessellator: &mut StrokeTessellator,
    mesh: &mut Mesh,
) where
    I: IntoIterator<Item = (Vec2, Color, Vec2)>,
{
    if options.strokes_nothing() {
        return;
    }
    let path = match points_vertex_to_lyon_path(points_vertex, close) {
        None => return,
        Some(p) => p,
    };

    // Extend the mesh with the built path.
    let mut mesh_builder = draw::mesh::MeshBuilder::vertex_per_point(mesh, transform);
    let res = match options {
        Options::Fill(options) => fill_tessellator.tessellate_with_ids(
            path.id_iter(),
            &path,
            Some(&path),
            &options,
            &mut mesh_builder,
        ),
        Options::Stroke(options) => stroke_tessellator.tessellate_with_ids(
            path.id_iter(),
            &path,
            Some(&path),
            &options,
            &mut mesh_builder,
        ),
    };
    if let Err(err) = res {
        tracing::debug!(target: "regia_quick", "failed to tessellate path: {err:?}");
    }
}

#[allow(clippy::too_many_arguments)] // the ported nannou signature
pub(crate) fn render_path_source(
    path_src: PathEventSourceIter,
    color: Option<Color>,
    transform: Mat4,
    options: Options,
    theme: &draw::Theme,
    theme_prim: &draw::theme::Primitive,
    fill_tessellator: &mut FillTessellator,
    stroke_tessellator: &mut StrokeTessellator,
    mesh: &mut Mesh,
) {
    match path_src {
        PathEventSourceIter::Events(events) => render_path_events(
            events,
            color,
            transform,
            options,
            theme,
            theme_prim,
            fill_tessellator,
            stroke_tessellator,
            mesh,
        ),
        PathEventSourceIter::Vertex { points, close } => render_path_vertex(
            points,
            close,
            transform,
            options,
            fill_tessellator,
            stroke_tessellator,
            mesh,
        ),
    }
}

impl draw::render::RenderPrimitive for Path {
    fn render_primitive(self, ctxt: draw::render::RenderContext, mesh: &mut Mesh) {
        let Path {
            color,
            position,
            orientation,
            path_event_src,
            options,
            ..
        } = self;

        // Determine the transform to apply to all points.
        let global_transform = *ctxt.transform;
        let local_transform = position.transform() * orientation.transform();
        let transform = global_transform * local_transform;

        // A function for rendering the path.
        let render = |src: PathEventSourceIter,
                      theme: &draw::Theme,
                      fill_tessellator: &mut lyon::tessellation::FillTessellator,
                      stroke_tessellator: &mut lyon::tessellation::StrokeTessellator| {
            render_path_source(
                src,
                color,
                transform,
                options,
                theme,
                &draw::theme::Primitive::Path,
                fill_tessellator,
                stroke_tessellator,
                mesh,
            )
        };

        match path_event_src {
            PathEventSource::Buffered(range) => {
                let mut events = ctxt.path_event_buffer[range].iter().cloned();
                let src = PathEventSourceIter::Events(&mut events);
                render(src, ctxt.theme, ctxt.fill_tessellator, ctxt.stroke_tessellator);
            }
            PathEventSource::Vertex { range, close } => {
                let mut points_colored = ctxt.path_points_vertex_buffer[range].iter().cloned();
                let src = PathEventSourceIter::Vertex {
                    points: &mut points_colored,
                    close,
                };
                render(src, ctxt.theme, ctxt.fill_tessellator, ctxt.stroke_tessellator);
            }
        }
    }
}

/// Lyon paths built on this thread (fill and stroke each count).
///
/// The plain-shape fast paths do not increment this. The count is per thread so parallel tests
/// do not observe each other's tessellation.
#[must_use]
pub fn lyon_path_builds() -> u64 {
    LYON_PATHS.with(std::cell::Cell::get)
}

fn note_lyon_path() {
    LYON_PATHS.with(|n| n.set(n.get().saturating_add(1)));
}

thread_local! {
    static LYON_PATHS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A lyon path through `(point, tex_coord)` pairs (two attributes per point).
pub fn points_themed_to_lyon_path<I>(points: I, close: bool) -> Option<lyon::path::Path>
where
    I: IntoIterator<Item = (Vec2, Vec2)>,
{
    note_lyon_path();
    // Build a path with a uv attribute for each channel.
    let channels = 2;
    let mut path_builder = lyon::path::Path::builder_with_attributes(channels);

    // Begin the path.
    let mut iter = points.into_iter();
    let (first_point, first_tex_coord) = iter.next()?;
    let p = first_point.to_array().into();
    let [u, v] = first_tex_coord.to_array();
    path_builder.begin(p, &[u, v]);

    // Add the lines, keeping track of the last
    for (point, text_coord) in iter {
        let p = point.to_array().into();
        let [u, v] = text_coord.to_array();
        path_builder.line_to(p, &[u, v]);
    }

    // End the path, closing if necessary.
    path_builder.end(close);

    // Build it!
    Some(path_builder.build())
}

/// Create a lyon path for the given iterator of colored points.
pub fn points_vertex_to_lyon_path<I>(points: I, close: bool) -> Option<lyon::path::Path>
where
    I: IntoIterator<Item = (Vec2, Color, Vec2)>,
{
    note_lyon_path();
    // Build a path with a color and uv attribute for each channel.
    let channels = 6;
    let mut path_builder = lyon::path::Path::builder_with_attributes(channels);

    // Begin the path.
    let mut iter = points.into_iter();
    let (first_point, first_color, first_tex_coord) = iter.next()?;
    let p = first_point.to_array().into();
    let [r, g, b, a] = first_color.to_array();
    let [u, v] = first_tex_coord.to_array();
    path_builder.begin(p, &[r, g, b, a, u, v]);

    // Add the lines, keeping track of the last
    for (point, color, text_coord) in iter {
        let p = point.to_array().into();
        let [r, g, b, a] = color.to_array();
        let [u, v] = text_coord.to_array();
        path_builder.line_to(p, &[r, g, b, a, u, v]);
    }

    // End the path, closing if necessary.
    path_builder.end(close);

    // Build it!
    Some(path_builder.build())
}

impl Path {
    // Initialise a new `Path` with its ranges into the intermediary mesh, ready for drawing.
    fn new(
        position: position::Properties,
        orientation: orientation::Properties,
        color: Option<Color>,
        path_event_src: PathEventSource,
        options: Options,
    ) -> Self {
        Path {
            color,
            orientation,
            position,
            path_event_src,
            options,
        }
    }
}

impl<'a> DrawingPathInit<'a> {
    /// Specify that we want to use fill tessellation for the path.
    ///
    /// The returned building context allows for specifying the fill tessellation options.
    pub fn fill(self) -> DrawingPathFill<'a> {
        path_fill(self.draw, self.index);
        self.transition()
    }

    /// Specify that we want to use stroke tessellation for the path.
    ///
    /// The returned building context allows for specifying the stroke tessellation options.
    pub fn stroke(self) -> DrawingPathStroke<'a> {
        path_stroke(self.draw, self.index);
        self.transition()
    }
}

impl<'a> DrawingPathFill<'a> {
    /// Maximum allowed distance to the path when building an approximation.
    pub fn tolerance(self, tolerance: f32) -> Self {
        self.fill_tolerance(tolerance)
    }

    /// Specify the rule used to determine what is inside and what is outside of the shape.
    ///
    /// Both `EvenOdd` and `NonZero` are passed through to the fill tessellator.
    pub fn rule(self, rule: lyon::tessellation::FillRule) -> Self {
        self.fill_rule(rule)
    }
}

impl<'a> DrawingPathStroke<'a> {
    /// Short-hand for the `stroke_weight` method.
    pub fn weight(self, weight: f32) -> Self {
        self.stroke_weight(weight)
    }

    /// Short-hand for the `stroke_tolerance` method.
    pub fn tolerance(self, tolerance: f32) -> Self {
        self.stroke_tolerance(tolerance)
    }
}

impl<'a, T> DrawingPathOptions<'a, T>
where
    T: TessellationOptions,
{
    /// Submit the path events to be tessellated.
    pub fn events<I>(self, events: I) -> DrawingPath<'a>
    where
        I: IntoIterator<Item = lyon::path::PathEvent>,
    {
        let mut events = events.into_iter();
        path_events(self.draw, self.index, &mut events);
        self.transition()
    }

    /// Submit the path events as a polyline of points.
    pub fn points<I>(self, points: I) -> DrawingPath<'a>
    where
        I: IntoIterator,
        I::Item: Into<Vec2>,
    {
        let mut points = points.into_iter().map(Into::into);
        path_points(self.draw, self.index, false, &mut points);
        self.transition()
    }

    /// Submit the path events as a polyline of points.
    ///
    /// An event will be generated that closes the start and end points.
    pub fn points_closed<I>(self, points: I) -> DrawingPath<'a>
    where
        I: IntoIterator,
        I::Item: Into<Vec2>,
    {
        let mut points = points.into_iter().map(Into::into);
        path_points(self.draw, self.index, true, &mut points);
        self.transition()
    }

    /// Describe the shape with a sequence of coloured points.
    pub fn points_colored<I, P, C>(self, points: I) -> DrawingPath<'a>
    where
        I: IntoIterator<Item = (P, C)>,
        P: Into<Vec2>,
        C: Into<Color>,
    {
        let mut points = points.into_iter().map(|(p, c)| (p.into(), c.into(), Vec2::ZERO));
        path_points_vertex(self.draw, self.index, false, &mut points);
        self.transition()
    }

    /// Describe the closed shape with a sequence of coloured points.
    pub fn points_colored_closed<I, P, C>(self, points: I) -> DrawingPath<'a>
    where
        I: IntoIterator<Item = (P, C)>,
        P: Into<Vec2>,
        C: Into<Color>,
    {
        let mut points = points.into_iter().map(|(p, c)| (p.into(), c.into(), Vec2::ZERO));
        path_points_vertex(self.draw, self.index, true, &mut points);
        self.transition()
    }

    /// Submit path events as a polyline of vertex points.
    pub fn points_vertex<I, P, C, U>(self, points: I) -> DrawingPath<'a>
    where
        I: IntoIterator<Item = (P, C, U)>,
        P: Into<Vec2>,
        C: Into<Color>,
        U: Into<Vec2>,
    {
        let mut points = points.into_iter().map(|(p, c, u)| (p.into(), c.into(), u.into()));
        path_points_vertex(self.draw, self.index, false, &mut points);
        self.transition()
    }

    /// Submit path events as a polyline of vertex points.
    ///
    /// The path with automatically close from the end point to the start point.
    pub fn points_vertex_closed<I, P, C, U>(self, points: I) -> DrawingPath<'a>
    where
        I: IntoIterator<Item = (P, C, U)>,
        P: Into<Vec2>,
        C: Into<Color>,
        U: Into<Vec2>,
    {
        let mut points = points.into_iter().map(|(p, c, u)| (p.into(), c.into(), u.into()));
        path_points_vertex(self.draw, self.index, true, &mut points);
        self.transition()
    }
}

// Transition the primitive being drawn at `index` from `PathInit` to `PathFill`.
fn path_fill(draw: &draw::Draw, index: usize) {
    drawing::with_primitive(draw, index, |prim| match prim {
        Primitive::PathInit(_) => *prim = Primitive::PathFill(PathInit.fill()),
        _ => crate::warn_once!("expected a `PathInit` primitive"),
    })
}

// Transition the primitive being drawn at `index` from `PathInit` to `PathStroke`.
fn path_stroke(draw: &draw::Draw, index: usize) {
    drawing::with_primitive(draw, index, |prim| match prim {
        Primitive::PathInit(_) => *prim = Primitive::PathStroke(PathInit.stroke()),
        _ => crate::warn_once!("expected a `PathInit` primitive"),
    })
}

// Submit the path's events, transitioning the primitive from `PathFill` or `PathStroke` to
// `Path`.
fn path_events(draw: &draw::Draw, index: usize, events: &mut dyn Iterator<Item = PathEvent>) {
    // Pull the user's iterator before taking the `Draw` lock: it may call back into `Draw`.
    let mut events_buf = events.collect::<Vec<_>>().into_iter();
    drawing::with_primitive_ctxt(draw, index, |prim, ctxt| match prim {
        Primitive::PathFill(opts) => Primitive::Path(opts.events(ctxt, &mut events_buf)),
        Primitive::PathStroke(opts) => Primitive::Path(opts.events(ctxt, &mut events_buf)),
        other => {
            crate::warn_once!("expected a `PathFill` or `PathStroke` primitive");
            other
        }
    })
}

// Submit the path's points, transitioning the primitive from `PathFill` or `PathStroke` to
// `Path`.
fn path_points(draw: &draw::Draw, index: usize, close: bool, points: &mut dyn Iterator<Item = Vec2>) {
    // Pull the user's iterator before taking the `Draw` lock: it may call back into `Draw`.
    let mut points_buf = points.collect::<Vec<_>>().into_iter();
    drawing::with_primitive_ctxt(draw, index, |prim, ctxt| match prim {
        Primitive::PathFill(opts) => Primitive::Path(opts.points_inner(ctxt, close, &mut points_buf)),
        Primitive::PathStroke(opts) => Primitive::Path(opts.points_inner(ctxt, close, &mut points_buf)),
        other => {
            crate::warn_once!("expected a `PathFill` or `PathStroke` primitive");
            other
        }
    })
}

// Submit the path's vertex points, transitioning the primitive from `PathFill` or `PathStroke`
// to `Path`.
fn path_points_vertex(
    draw: &draw::Draw,
    index: usize,
    close: bool,
    points: &mut dyn Iterator<Item = (Vec2, Color, Vec2)>,
) {
    // Pull the user's iterator before taking the `Draw` lock: it may call back into `Draw`.
    let mut points_buf = points.collect::<Vec<_>>().into_iter();
    drawing::with_primitive_ctxt(draw, index, |prim, ctxt| match prim {
        Primitive::PathFill(opts) => Primitive::Path(opts.points_vertex_inner(ctxt, close, &mut points_buf)),
        Primitive::PathStroke(opts) => {
            Primitive::Path(opts.points_vertex_inner(ctxt, close, &mut points_buf))
        }
        other => {
            crate::warn_once!("expected a `PathFill` or `PathStroke` primitive");
            other
        }
    })
}

impl SetFill for PathFill {
    fn fill_options_mut(&mut self) -> &mut FillOptions {
        &mut self.opts
    }
}

impl SetStroke for PathStroke {
    fn stroke_options_mut(&mut self) -> &mut StrokeOptions {
        &mut self.opts
    }
}

impl TessellationOptions for FillOptions {
    type Tessellator = FillTessellator;
    fn into_options(self) -> Options {
        Options::Fill(self)
    }
}

impl TessellationOptions for StrokeOptions {
    type Tessellator = StrokeTessellator;
    fn into_options(self) -> Options {
        Options::Stroke(self)
    }
}

impl<T> SetOrientation for PathOptions<T> {
    fn properties(&mut self) -> &mut orientation::Properties {
        SetOrientation::properties(&mut self.orientation)
    }
}

impl<T> SetPosition for PathOptions<T> {
    fn properties(&mut self) -> &mut position::Properties {
        SetPosition::properties(&mut self.position)
    }
}

impl<T> SetColor for PathOptions<T> {
    fn color_mut(&mut self) -> &mut Option<Color> {
        SetColor::color_mut(&mut self.color)
    }
}

impl SetOrientation for Path {
    fn properties(&mut self) -> &mut orientation::Properties {
        SetOrientation::properties(&mut self.orientation)
    }
}

impl SetPosition for Path {
    fn properties(&mut self) -> &mut position::Properties {
        SetPosition::properties(&mut self.position)
    }
}

impl SetColor for Path {
    fn color_mut(&mut self) -> &mut Option<Color> {
        SetColor::color_mut(&mut self.color)
    }
}

impl From<PathInit> for Primitive {
    fn from(prim: PathInit) -> Self {
        Primitive::PathInit(prim)
    }
}

impl From<PathStroke> for Primitive {
    fn from(prim: PathStroke) -> Self {
        Primitive::PathStroke(prim)
    }
}

impl From<PathFill> for Primitive {
    fn from(prim: PathFill) -> Self {
        Primitive::PathFill(prim)
    }
}

impl From<Path> for Primitive {
    fn from(prim: Path) -> Self {
        Primitive::Path(prim)
    }
}
