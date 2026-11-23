// SPDX-License-Identifier: MPL-2.0
//! Material graphs: node-based materials that compile to WGSL.
//!
//! Specification: `docs/specs/22-tier2.md` T2.4 (pulled forward by `docs/specs/27-renderer-excellence.md`
//! § 4) and the `material_surface` contract of `docs/specs/13-renderer.md` § 7. Format:
//! `docs/formats/matgraph-ron.md`. Agent surface: `book/agents/material-graph.md`.
//!
//! # Pieces
//!
//! - [`mod@format`]: the versioned `.matgraph.ron` asset — parameters, textures, a node map, the
//!   surface outputs and a shading block — with the frozen-format machinery (version peek,
//!   migrator chain, schema fingerprint).
//! - [`mod@compile`]: type inference and topological emission of `fn material_surface(in:
//!   SurfaceInput) -> Surface`. Errors name the node they belong to. The output is validated
//!   by composing it into the forward, depth, shadow and velocity shaders.
//! - [`instance`]: `.matinst.ron` instances that override parameters and textures of a parent
//!   graph without changing its code.
//! - [`cache`]: compiled graphs keyed by structural hash; the renderer keys its pipeline sets
//!   by the generated code's hash, so graphs and instances that share code share pipelines.
//! - [`material`]: builds a [`regia_render::LabMaterial`] whose surface stage is the graph.
//!   Lighting stays the forward pass's single code path.
//! - [`library`]: the [`MaterialGraphLibrary`] resource the agent methods edit.
//! - [`cooked`]: the cooked form the cook writes and a shipped game loads (no compile at run
//!   time).
//! - [`runtime`]: [`MaterialGraphRef`] entities resolved through the asset server, hot reload,
//!   pipeline prewarm at level load, the material clock.
//! - [`pins`]: the editor's pin model — named input slots per node kind, rewiring, a fresh node
//!   of a kind, surface outputs by slot (`docs/specs/80-matgraph-remainder.md`).
//! - [`mod@code`]: the WGSL code node (ADR-7852) — naga validation with line and column,
//!   restrictions, `#include` resolution from `shaders/include/`, the editor's lenient compile.
//!
//! # Example
//!
//! ```
//! use regia_matgraph::{MaterialGraph, compile_unvalidated};
//!
//! let graph = MaterialGraph::from_ron(r#"(
//!     format_version: 2,
//!     params: [(name: "tint", value: Color((0.8, 0.2, 0.1)))],
//!     nodes: {
//!         "tint": Param("tint"),
//!         "vcol": VertexColor,
//!         "rgb": Swizzle(input: "vcol", channels: "rgb"),
//!         "base": Mul("tint", "rgb"),
//!     },
//!     output: (base_color: Some("base")),
//! )"#)?;
//! let compiled = compile_unvalidated(&graph)?;
//! assert!(compiled.wgsl.contains("fn material_surface"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod cache;
pub mod code;
pub mod compile;
pub mod cooked;
pub mod format;
pub mod instance;
pub mod library;
mod lower;
pub mod material;
pub mod pins;
pub mod runtime;

pub use cache::{CacheStats, ShaderCache};
pub use code::{CODE_ERROR_CODES, CodeDiagnostic, check_code, compile_lenient, compile_lenient_with};
pub use compile::{
    COMPILE_ERROR_CODES, CompileError, CompileErrors, CompiledGraph, GENERATED_CODE_INVALID_PREFIX,
    MAX_LOWERED_NODES, MAX_NOISE_OCTAVES, MAX_PARAMS, MAX_STATIC_SWITCHES, ParamSlot, TextureSlot, compile,
    compile_count, compile_unvalidated, compile_unvalidated_with, compile_with, default_params, graph_hash,
    permutation_hash, thread_compile_count,
};
pub use cooked::{
    COOKED_MATGRAPH_TYPE_PATH, COOKED_MATGRAPH_VERSION, CookedGraphError, CookedGraphTexture,
    CookedMaterialGraph,
};
pub use format::{
    Channel, CodeInput, ColorSpace, FormatError, FunctionInput, FunctionSource, GraphAlphaMode,
    MATFUNC_FORMAT_VERSION, MATGRAPH_FORMAT_VERSION, MaterialFunction, MaterialGraph, Node, NormalBlend,
    NormalSpace, ParamDecl, ParamRange, ParamValue, Shading, SurfaceOutputs, SwitchDecl, TextureDecl,
    ValueType, matfunc_migration_sources, matfunc_schema_fingerprint, matgraph_migration_sources,
    matgraph_schema_fingerprint,
};
pub use instance::{
    InstanceError, MATINST_FORMAT_VERSION, MaterialInstance, matinst_migration_sources,
    matinst_schema_fingerprint,
};
pub use library::{LibraryEntry, MaterialGraphLibrary, MaterialGraphPlugin};
pub use material::{MaterialBuildError, instance_material, lab_material};
pub use runtime::{
    GraphLoadError, GraphMaterials, MaterialGraphRef, apply_material_graphs, drive_material_clock,
    prewarm_graph_pipelines,
};
