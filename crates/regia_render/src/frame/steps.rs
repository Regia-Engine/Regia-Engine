// SPDX-License-Identifier: MPL-2.0
//! The setup and bookkeeping steps of `FrameRenderer::render_into`, one module each (ADR-2300).
//!
//! `render_into` is the frame's recording order; these modules hold the steps between its pass
//! calls that only compute what the passes read (the frame's modes and matrices, the view bind
//! group, the static-screen key) or open and close the frame (the frame clock, the submit).
//! Each was moved out of `frame.rs` unchanged: same statements, same order, same side effects.

// Frame clock, device poll, timings readback and the streaming budget.
pub(super) mod begin;
// The frame's modes and matrices: extent, upscaler, exposure, GI and sky modes, the matrices.
pub(super) mod modes;
// AO and visibility targets, the probe begin and the view bind group.
pub(super) mod view_group;
// The frame's pipeline keys and the forward, lite, mask, lightmap, film, subsurface and OIT sets.
pub(super) mod pipelines;
// Owned clones of the frame's pipelines and the glass sets for the passes after the culler.
pub(super) mod forward_pipes;
// The static-screen key and what the screen-space passes run or reuse.
pub(super) mod screen_reuse;
// The depth prepass, the visibility geometry and the GPU cull.
pub(super) mod depth_passes;
// The publish copies, the timestamp resolve and the frame's submit.
pub(super) mod submit;
// The decal receiver stamps.
pub(super) mod receiver_stamps;
// The frame recording context: the opening, modes and shadow timings, and the pass recording state.
pub(super) mod recording;
