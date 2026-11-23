// SPDX-License-Identifier: MPL-2.0
//! Cheap product looks for the light viewer (track JW, ADR-2538).
//!
//! Everything here is built for a phone browser: WebGL2 limits on every backend (uniform buffers
//! and textures only), a fixed number of texture samples per fragment, no ray tracing and no
//! bounce loops. Material values are **data** from a material library
//! (`jewelry.material_library/1`); the built-in library holds generic stand-ins only.
//!
//! - [`cuts`]: stand-in gem cuts from published proportions (round brilliant, marquise, pear,
//!   princess, heart) as facet planes and flat-shaded meshes.
//! - [`mesh`]: the vertex format and generic parts (ring band, ball, cabochon dome, bezel cup).
//! - [`material`]: library entries mapped to look materials ([`LookMaterial`]) and their uniform data.
//! - [`anodize`] and [`bake`]: the offline anodize baker (one film on a metal, cos theta to RGB);
//!   never called per frame.
//! - [`env`](mod@env): the fixed studio (environment cube map with roughness mips, three key lights).
//! - `gpu` (feature `gpu`): the wgpu renderer, one pipeline per look kind, and headless stills.
//! - `viewer` (feature `viewer`): the looks as `regia_viewer` shader hooks and material blocks.
//! - [`agent`]: the JSON surface the viewer's agent methods call (`looks.*`).
//!
//! ```
//! use regia_looks::{Library, LookKind};
//! let lib = Library::builtin().expect("built-in library");
//! let gold = lib.look("gold-yellow-18k", Some("finish-polish")).expect("gold");
//! assert_eq!(gold.kind(), LookKind::Metal);
//! ```

pub mod agent;
pub mod anodize;
pub mod bake;
pub mod board;
pub mod cuts;
pub mod env;
#[cfg(feature = "gpu")]
pub mod gpu;
pub mod material;
pub mod mesh;
#[cfg(feature = "viewer")]
pub mod viewer;

pub use cuts::{CutType, FacetPlane, FacetRole, GemCut, Proportions};
pub use env::{KeyLight, Studio};
pub use material::{
    FacetFamily, GemLook, Library, LookError, LookKind, LookMaterial, MaterialParams, MetalLook, OpalLook,
    StoneSize,
};
pub use mesh::{LookMesh, LookVertex};

/// The WGSL source of the look shaders (also in `gpu::LOOKS_WGSL`).
pub const LOOKS_SHADER: &str = concat!(
    include_str!("shaders/shared/step_cut.wgsl"),
    "\n",
    include_str!("shaders/shared/opal_play.wgsl"),
    "\n",
    include_str!("shaders/shared/metal_grain.wgsl"),
    "\n",
    include_str!("shaders/looks.wgsl"),
);
