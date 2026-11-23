// SPDX-License-Identifier: MPL-2.0
//! `regia_viewer`: the lean renderer of the product viewer, for native, WebGPU and WebGL2.
//!
//! One forward pass, one baked environment, uniform buffers and textures only: no storage buffers, no
//! compute, no loops over scene data. It does not depend on the engine's renderer, so the web player
//! stays small.
//!
//! A host (the web player, the Android viewer, a test) creates a device with
//! [`gpu::request_viewer_device`], a [`ViewerRenderer`] on it, uploads meshes, creates material slots and
//! adds draws. Then each time the page or app has a chance to draw it calls
//! [`ViewerRenderer::render_if_needed`], which does nothing while nothing changed.
//!
//! ```no_run
//! # use regia_viewer::*;
//! # fn demo(device: wgpu::Device, queue: wgpu::Queue, target: wgpu::Texture) -> Result<(), ViewerError> {
//! let mut viewer = ViewerRenderer::new(device, queue, ViewerConfig::new(target.format()))?;
//! let mesh = viewer.upload_mesh(&standin::ball(3.0, 32))?;
//! let metal = viewer.create_material(&ViewerMaterial::default());
//! viewer.add_draw(mesh, metal, &[glam::Affine3A::IDENTITY])?;
//! viewer.render_if_needed(&target)?; // Draws.
//! viewer.render_if_needed(&target)?; // Nothing changed: no draw.
//! # Ok(()) }
//! ```
//!
//! [`ViewerMaterial`] and the shader hooks ([`hooks`]) are the interface a look builds on. See
//! `book/agents/viewer-renderer.md`.

pub mod bake;
pub mod camera;
pub mod env;
pub mod gpu;
pub mod hooks;
pub mod material;
pub mod mesh;
pub mod renderer;
pub mod standin;
pub mod still;

pub use camera::ViewerCamera;
pub use env::{BakedEnv, ENV_FORMAT_VERSION, EnvError};
pub use hooks::ShaderHooks;
pub use material::{
    AnodizeFilm, AnodizedParams, CabochonParams, FrostedParams, GemParams, LutHandle, MaterialBlock,
    MetalParams, OpalParams, ShaderFamily, ViewerMaterial,
};
pub use mesh::{ViewerBounds, ViewerMeshData, ViewerVertex};
pub use renderer::{
    ContactShadow, DEFAULT_KEY_LIGHTS, DFG_SIZE, DFG_TABLE, DrawHandle, KeyLight, LUT_ROWS, LUT_WIDTH,
    MaterialHandle, MeshHandle, ViewerConfig, ViewerRenderer, ViewerStats, ViewerStyle,
};
pub use still::{Still, capture_still_async};

#[cfg(not(target_arch = "wasm32"))]
pub use still::capture_still;

/// What the viewer refuses. Hosts show or log it; nothing here is fatal to the device.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ViewerError {
    /// A mesh failed validation.
    #[error("bad mesh: {0}")]
    BadMesh(String),
    /// A handle whose object was removed.
    #[error("stale {0} handle")]
    StaleHandle(&'static str),
    /// The object is still used by a draw.
    #[error("still in use: {0}")]
    InUse(&'static str),
    /// A setting cannot be used.
    #[error("bad setting: {0}")]
    Config(String),
    /// The camera cannot produce a view.
    #[error("bad camera: {0}")]
    BadCamera(&'static str),
    /// The lookup atlas is full.
    #[error("the lookup atlas is full ({0} rows)")]
    LutFull(u32),
    /// A lookup has too few entries.
    #[error("a lookup needs at least 2 entries, got {0}")]
    BadLut(usize),
    /// The target texture has another format than the pipelines.
    #[error("target format {got:?}, the pipelines were built for {expected:?}")]
    TargetFormat {
        /// The configured format.
        expected: wgpu::TextureFormat,
        /// The target's format.
        got: wgpu::TextureFormat,
    },
    /// The machine has no GPU adapter.
    #[error("no adapter: {0}")]
    NoAdapter(String),
    /// An adapter refused to create a device.
    #[error("no device: {0}")]
    Device(String),
    /// A still failed.
    #[error("still failed: {0}")]
    Still(String),
    /// An environment file was refused.
    #[error(transparent)]
    Env(#[from] EnvError),
}
