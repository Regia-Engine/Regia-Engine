// SPDX-License-Identifier: MPL-2.0
//! CPU reference implementations of post-processing effects.
//!
//! Each submodule fixes the exact arithmetic of one effect on plain `f32` buffers, with no device
//! or window, as the deterministic oracle that GPU passes and golden images are verified against.
//! Nothing here is a runtime path of the renderer: no GPU code and no shaders live here, and a
//! later lane that ports an effect to the GPU must reproduce these numbers to floating-point
//! tolerance.

pub mod glow_longtail;
pub mod lens_flare;
pub mod radial_blur;
