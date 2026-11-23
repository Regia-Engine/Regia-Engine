// SPDX-License-Identifier: MPL-2.0
//! Immediate-mode triangle batching: the backend of the quick draw layer (`regia_quick`).
//!
//! An [`ImmediateBatch`] collects coloured, optionally textured triangles for one frame. Every
//! triangle is submitted under the batch's current [`ImmediateState`] (texture, blend, scissor);
//! consecutive triangles under the same state share one draw call, and a state change — or an
//! explicit [`ImmediateBatch::flush`] — closes the open call. A model-matrix stack transforms
//! positions on submission. The design follows the backend contract of macroquad's `QuadGl`
//! and Piston's `Graphics` trait (one "submit triangles with state" primitive; every shape is
//! built on it), written here for wgpu.
//!
//! [`ImmediateRenderer`] draws a batch over a window frame (the 2D quick layer goes after
//! tonemapping, through [`crate::WindowOverlay`]). An empty batch records nothing: no pass, no
//! upload, no cost.

mod gpu;

use std::ops::Range;

use regia_core::math::{Mat4, Vec3};

pub use gpu::{
    ImmediateImage, ImmediateImageFormat, ImmediateRenderer, ImmediateTarget, UploadKey,
    immediate_shader_source, uploads_vertex_buffers,
};

/// One vertex: position (transformed by the model matrix on submission), texture coordinate and
/// linear straight-alpha colour.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ImmediateVertex {
    /// Position in the batch's space (2D: logical units, `z` ignored by the overlay).
    pub position: [f32; 3],
    /// Texture coordinate (`0..1`, top-left origin).
    pub uv: [f32; 2],
    /// Linear RGBA, straight alpha.
    pub color: [f32; 4],
}

const _: () = assert!(std::mem::size_of::<ImmediateVertex>() == 36);

impl ImmediateVertex {
    /// An untextured vertex.
    #[must_use]
    pub const fn new(position: [f32; 3], color: [f32; 4]) -> Self {
        Self {
            position,
            uv: [0.0, 0.0],
            color,
        }
    }

    /// A textured vertex.
    #[must_use]
    pub const fn textured(position: [f32; 3], uv: [f32; 2], color: [f32; 4]) -> Self {
        Self { position, uv, color }
    }
}

/// What a draw call samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ImmediateTexture {
    /// No texture (a white texel): the vertex colour alone.
    #[default]
    White,
    /// A registered RGBA image ([`ImmediateRenderer::set_image`]) multiplied by the vertex colour.
    Image(u64),
    /// A registered single-channel coverage mask (glyph atlases): the vertex colour with its alpha
    /// scaled by the mask.
    Mask(u64),
}

/// How a draw call blends over what is already drawn (colours are premultiplied on the GPU).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ImmediateBlend {
    /// Ordinary alpha blending ("over").
    #[default]
    Alpha,
    /// Adds the colour (scaled by its alpha): glows, light.
    Additive,
    /// Multiplies what is underneath by the colour (alpha fades the effect out).
    Multiply,
    /// Writes the colour and alpha unblended.
    Replace,
}

/// The state every triangle is submitted under; a change starts a new draw call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ImmediateState {
    /// Sampled texture.
    pub texture: ImmediateTexture,
    /// Blend mode.
    pub blend: ImmediateBlend,
    /// Scissor rectangle in target pixels `[x, y, w, h]` (`None` = whole target).
    pub scissor: Option<[u32; 4]>,
}

/// One draw call: a state and its index range.
#[derive(Debug, Clone, PartialEq)]
pub struct ImmediateDraw {
    /// The state the triangles were submitted under.
    pub state: ImmediateState,
    /// Range into [`ImmediateBatch::indices`].
    pub indices: Range<u32>,
}

/// Counts of one batch (what the agent surface and benches report).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImmediateStats {
    /// Vertices submitted.
    pub vertices: u32,
    /// Triangles submitted.
    pub triangles: u32,
    /// Draw calls after state batching.
    pub draw_calls: u32,
}

/// A frame's worth of immediate triangles, batched by state.
#[derive(Debug, Clone, Default)]
pub struct ImmediateBatch {
    vertices: Vec<ImmediateVertex>,
    indices: Vec<u32>,
    draws: Vec<ImmediateDraw>,
    state: ImmediateState,
    /// First index of the open (not yet flushed) draw call.
    open_from: u32,
    model_stack: Vec<Mat4>,
}

impl ImmediateBatch {
    /// An empty batch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True when nothing has been submitted since the last [`Self::clear`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Drops every triangle and draw call and resets the state and matrix stack; keeps the
    /// allocations for the next frame.
    pub fn clear(&mut self) {
        self.vertices.clear();
        self.indices.clear();
        self.draws.clear();
        self.state = ImmediateState::default();
        self.open_from = 0;
        self.model_stack.clear();
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> ImmediateState {
        self.state
    }

    /// Sets the whole state, closing the open draw call when it changes.
    pub fn set_state(&mut self, state: ImmediateState) {
        if state != self.state {
            self.flush();
            self.state = state;
        }
    }

    /// Sets the sampled texture.
    pub fn set_texture(&mut self, texture: ImmediateTexture) {
        self.set_state(ImmediateState {
            texture,
            ..self.state
        });
    }

    /// Sets the blend mode.
    pub fn set_blend(&mut self, blend: ImmediateBlend) {
        self.set_state(ImmediateState { blend, ..self.state });
    }

    /// Sets (or clears) the scissor rectangle, in target pixels.
    pub fn set_scissor(&mut self, scissor: Option<[u32; 4]>) {
        self.set_state(ImmediateState {
            scissor,
            ..self.state
        });
    }

    /// Pushes `matrix` (applied after the matrices already on the stack's top, i.e. the new top
    /// is `top * matrix`).
    pub fn push_model_matrix(&mut self, matrix: Mat4) {
        let top = self.model();
        self.model_stack.push(top * matrix);
    }

    /// Pops the top model matrix (no-op on an empty stack).
    pub fn pop_model_matrix(&mut self) {
        self.model_stack.pop();
    }

    /// The model matrix submissions are transformed by.
    #[must_use]
    pub fn model(&self) -> Mat4 {
        self.model_stack.last().copied().unwrap_or(Mat4::IDENTITY)
    }

    /// Closes the open draw call so the next submission starts a new one (even under the same
    /// state). Call it before interleaving raw rendering with immediate drawing.
    pub fn flush(&mut self) {
        let end = self.indices.len() as u32;
        if end > self.open_from {
            self.draws.push(ImmediateDraw {
                state: self.state,
                indices: self.open_from..end,
            });
        }
        self.open_from = end;
    }

    /// Submits an indexed triangle list (indices relative to `vertices`) under the current
    /// state, transformed by the current model matrix.
    pub fn triangles(&mut self, vertices: &[ImmediateVertex], indices: &[u32]) {
        let base = self.vertices.len() as u32;
        self.push_vertices(vertices);
        self.indices.extend(indices.iter().map(|i| base + i));
    }

    /// Submits a non-indexed triangle list of 2D points (three per triangle) in one colour, the
    /// shape the [`crate::primitives::triangulation`] fast paths stream.
    pub fn triangle_list_2d(&mut self, points: &[[f32; 2]], color: [f32; 4]) {
        let base = self.vertices.len() as u32;
        let model = self.model_stack.last().copied();
        self.vertices.extend(points.iter().map(|p| {
            let position = match model {
                Some(m) => m.transform_point3(Vec3::new(p[0], p[1], 0.0)).to_array(),
                None => [p[0], p[1], 0.0],
            };
            ImmediateVertex::new(position, color)
        }));
        let count = (points.len() / 3 * 3) as u32;
        self.indices.extend(base..base + count);
    }

    /// Reserves `vertices` vertices and returns the index of the first: callers then push indices
    /// with [`Self::push_indices`]. For tessellators that emit vertices and triangles separately.
    pub fn push_vertices(&mut self, vertices: &[ImmediateVertex]) -> u32 {
        let base = self.vertices.len() as u32;
        match self.model_stack.last() {
            Some(m) => self.vertices.extend(vertices.iter().map(|v| ImmediateVertex {
                position: m.transform_point3(Vec3::from_array(v.position)).to_array(),
                ..*v
            })),
            None => self.vertices.extend_from_slice(vertices),
        }
        base
    }

    /// Appends absolute vertex indices (see [`Self::push_vertices`]).
    pub fn push_indices(&mut self, indices: &[u32]) {
        self.indices.extend_from_slice(indices);
    }

    /// Mutable access to the vertex and index streams for in-place builders (tessellators). New
    /// indices must refer to vertices in the returned vector; the model matrix is not applied.
    pub fn streams_mut(&mut self) -> (&mut Vec<ImmediateVertex>, &mut Vec<u32>) {
        (&mut self.vertices, &mut self.indices)
    }

    /// Every vertex.
    #[must_use]
    pub fn vertices(&self) -> &[ImmediateVertex] {
        &self.vertices
    }

    /// Every index.
    #[must_use]
    pub fn indices(&self) -> &[u32] {
        &self.indices
    }

    /// The closed draw calls (call [`Self::flush`] first to include the open one).
    #[must_use]
    pub fn draws(&self) -> &[ImmediateDraw] {
        &self.draws
    }

    /// Closes the open call and returns the draw calls.
    pub fn finish(&mut self) -> &[ImmediateDraw] {
        self.flush();
        &self.draws
    }

    /// Vertex, triangle and draw-call counts (counts the open call too).
    #[must_use]
    pub fn stats(&self) -> ImmediateStats {
        let open = u32::from(self.indices.len() as u32 > self.open_from);
        ImmediateStats {
            vertices: self.vertices.len() as u32,
            triangles: (self.indices.len() / 3) as u32,
            draw_calls: self.draws.len() as u32 + open,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];

    fn tri(batch: &mut ImmediateBatch) {
        batch.triangle_list_2d(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]], RED);
    }

    #[test]
    fn same_state_shares_one_draw_call() {
        let mut batch = ImmediateBatch::new();
        assert!(batch.is_empty());
        for _ in 0..100 {
            tri(&mut batch);
        }
        let draws = batch.finish().to_vec();
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].indices, 0..300);
        assert_eq!(
            batch.stats(),
            ImmediateStats {
                vertices: 300,
                triangles: 100,
                draw_calls: 1
            }
        );
    }

    #[test]
    fn state_changes_and_flush_split_calls() {
        let mut batch = ImmediateBatch::new();
        tri(&mut batch);
        batch.set_texture(ImmediateTexture::Mask(7));
        tri(&mut batch);
        batch.set_texture(ImmediateTexture::Mask(7)); // unchanged: no split
        tri(&mut batch);
        batch.flush();
        tri(&mut batch);
        batch.set_blend(ImmediateBlend::Additive);
        batch.set_scissor(Some([0, 0, 4, 4]));
        tri(&mut batch);
        batch.set_blend(ImmediateBlend::Alpha); // empty call in between: nothing recorded
        let draws = batch.finish().to_vec();
        let ranges: Vec<_> = draws.iter().map(|d| d.indices.clone()).collect();
        assert_eq!(ranges, vec![0..3, 3..9, 9..12, 12..15]);
        assert_eq!(draws[1].state.texture, ImmediateTexture::Mask(7));
        assert_eq!(draws[3].state.blend, ImmediateBlend::Additive);
        assert_eq!(draws[3].state.scissor, Some([0, 0, 4, 4]));
        batch.clear();
        assert!(batch.is_empty());
        assert_eq!(batch.state(), ImmediateState::default());
    }

    #[test]
    fn model_stack_transforms_submissions() {
        let mut batch = ImmediateBatch::new();
        batch.push_model_matrix(Mat4::from_translation(Vec3::new(10.0, 0.0, 0.0)));
        batch.push_model_matrix(Mat4::from_scale(Vec3::splat(2.0)));
        tri(&mut batch);
        batch.pop_model_matrix();
        batch.triangles(
            &[
                ImmediateVertex::new([0.0, 0.0, 0.0], RED),
                ImmediateVertex::new([1.0, 0.0, 0.0], RED),
                ImmediateVertex::new([0.0, 1.0, 0.0], RED),
            ],
            &[0, 1, 2],
        );
        batch.pop_model_matrix();
        batch.pop_model_matrix(); // extra pop is harmless
        tri(&mut batch);
        let v = batch.vertices();
        assert_eq!(v[1].position, [12.0, 0.0, 0.0]);
        assert_eq!(v[4].position, [11.0, 0.0, 0.0]);
        assert_eq!(v[7].position, [1.0, 0.0, 0.0]);
        assert_eq!(&batch.indices()[3..6], &[3, 4, 5]);
    }
}
