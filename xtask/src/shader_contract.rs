// SPDX-License-Identifier: MPL-2.0
//! The Rust and WGSL struct ratchet over every engine crate's shaders, and the binding-limits
//! report (ADR-3002). The checker lives in `regia_render::shader_contract`; this module gathers the
//! shader sources of the other crates, their registry rows and the allowlist.

use regia_render::{AllowEntry, ContractReport, ContractRow, LimitsReport, MatrixScope};

/// Every row: `regia_render`, `regia_terrain`, `regia_vfx` and `regia_pt`.
#[must_use]
pub fn rows() -> Vec<ContractRow> {
    let mut rows = regia_render::render_contract_rows();
    rows.extend(regia_terrain::shader_contract_rows());
    rows.extend(regia_vfx::shader_contract_rows());
    rows.extend(regia_pt::shader_contract_rows());
    rows
}

/// Every allowlist entry: `regia_render`'s, then the other crates'.
#[must_use]
pub fn allowlist() -> Vec<AllowEntry> {
    let mut allow = regia_render::render_contract_allowlist();
    allow.extend_from_slice(OTHER_CRATES_ALLOWLIST);
    allow
}

const FIRST_TABLE: &str = "no row yet: the struct was bound before this list had its first table (b73986eb3 left it \
     empty while `plain_sources` already held these modules, so the ratchet was red from its start; ADR-5802); \
     convert when a row is written";

const NEW_TO_THE_SCAN: &str = "no row yet: the module joined `plain_sources` with ADR-5802 (the product viewer and \
     jewelry looks shaders, which no check-shaders list reached); convert when a row is written";

const MERGED_IN_THE_ROLL_UP: &str = "no row yet: the module reached this list through the integ-v049 roll-up \
     (integfix-vfx-lite-determinism's gather pass, ADR-5991; ADR-5993); convert when a row is written";

/// Bound structs of the crates outside `regia_render` with no row yet. Sorted by file, then
/// struct; the list only shrinks.
const OTHER_CRATES_ALLOWLIST: &[AllowEntry] = &[
    AllowEntry {
        file: "atrous.wgsl",
        wgsl_struct: "Params",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "draw2d.wgsl",
        wgsl_struct: "U",
        reason: "no row yet: regia_quick's private `draw2d::gpu::Uniform` (size, encode, pad; 16 bytes), which xtask \
                 cannot name (regia_quick is not an xtask dependency); added with draw2d.wgsl to `plain_sources` at \
                 integ-v06r; convert when gpu-2d-drawlist's owner writes the row",
    },
    AllowEntry {
        file: "fluid_flip.wgsl",
        wgsl_struct: "Params",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass_cull.wgsl",
        wgsl_struct: "GrassClasses",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass_cull.wgsl",
        wgsl_struct: "GrassDrawArgs",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass_cull.wgsl",
        wgsl_struct: "GrassInst",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass_cull.wgsl",
        wgsl_struct: "GrassParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass_cull.wgsl",
        wgsl_struct: "GrassPatch",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "CascadeVp",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "ContactParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "EngineCsm",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "Frame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "GrassClasses",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "GrassInst",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "GrassLod",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "GrassParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "Material",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "Node",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "ScorchUniform",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "Terrain",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "grass.wgsl",
        wgsl_struct: "VtParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor_cull.wgsl",
        wgsl_struct: "ImpInstance",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor_cull.wgsl",
        wgsl_struct: "ImpParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor_cull.wgsl",
        wgsl_struct: "ImpProto",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "CascadeVp",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "ContactParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "EngineCsm",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "Frame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "ImpInstance",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "ImpParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "ImpProto",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "Material",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "Node",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "ScorchUniform",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "Terrain",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "impostor.wgsl",
        wgsl_struct: "VtParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "lightmap_bake.wgsl",
        wgsl_struct: "Accum",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "lightmap_bake.wgsl",
        wgsl_struct: "Emitter",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "lightmap_bake.wgsl",
        wgsl_struct: "Light",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "lightmap_bake.wgsl",
        wgsl_struct: "Params",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "lightmap_bake.wgsl",
        wgsl_struct: "Task",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "lightmap_bake.wgsl",
        wgsl_struct: "Tri",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "looks.wgsl",
        wgsl_struct: "Frame",
        reason: NEW_TO_THE_SCAN,
    },
    AllowEntry {
        file: "looks.wgsl",
        wgsl_struct: "Object",
        reason: NEW_TO_THE_SCAN,
    },
    AllowEntry {
        file: "portal.wgsl",
        wgsl_struct: "Uniforms",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "pt.wgsl",
        wgsl_struct: "Pass",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "scatter.wgsl",
        wgsl_struct: "ScatterParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "sprite2d.wgsl",
        wgsl_struct: "View",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "terrain.wgsl",
        wgsl_struct: "ContactParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "terrain.wgsl",
        wgsl_struct: "EllFrame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "terrain.wgsl",
        wgsl_struct: "EllLight",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "terrain.wgsl",
        wgsl_struct: "EllShadowTile",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "terrain.wgsl",
        wgsl_struct: "VtParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "ui-overlay.wgsl",
        wgsl_struct: "BlurUniforms",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "ui-overlay.wgsl",
        wgsl_struct: "ScreenUniforms",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "ui-world.wgsl",
        wgsl_struct: "Panel",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx child emit",
        wgsl_struct: "ChildInit",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx child emit",
        wgsl_struct: "EventLink",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx child emit",
        wgsl_struct: "Frame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx child emit",
        wgsl_struct: "VfxEvent",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx decal",
        wgsl_struct: "Decal",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx decal",
        wgsl_struct: "Frame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx decal",
        wgsl_struct: "TileQuad",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx event prepare",
        wgsl_struct: "EventLink",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx event prepare",
        wgsl_struct: "Frame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx event prepare",
        wgsl_struct: "VfxEvent",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx mesh particle render",
        wgsl_struct: "Emitter",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx mesh particle render",
        wgsl_struct: "Frame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx particle render",
        wgsl_struct: "Emitter",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx particle render",
        wgsl_struct: "Frame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx radix setup",
        wgsl_struct: "SetupParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx radix setup",
        wgsl_struct: "SortConsts",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx radix sort",
        wgsl_struct: "PassParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx radix sort",
        wgsl_struct: "SortConsts",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx sim",
        wgsl_struct: "Emitter",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx sim",
        wgsl_struct: "Frame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx sim",
        wgsl_struct: "SpawnJob",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx sim",
        wgsl_struct: "VfxEvent",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx soft_half_res.wgsl",
        wgsl_struct: "Params",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx soft upsample",
        wgsl_struct: "Params",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx sort",
        wgsl_struct: "SortParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx trail",
        wgsl_struct: "Frame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "viewer contact_shadow.wgsl",
        wgsl_struct: "FrameBlock",
        reason: NEW_TO_THE_SCAN,
    },
    AllowEntry {
        file: "viewer contact_shadow.wgsl",
        wgsl_struct: "MaterialBlock",
        reason: NEW_TO_THE_SCAN,
    },
    AllowEntry {
        file: "viewer entry.wgsl",
        wgsl_struct: "FrameBlock",
        reason: NEW_TO_THE_SCAN,
    },
    AllowEntry {
        file: "viewer entry.wgsl",
        wgsl_struct: "MaterialBlock",
        reason: NEW_TO_THE_SCAN,
    },
    AllowEntry {
        file: "vt.wgsl",
        wgsl_struct: "VtReduce",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "water_body.wgsl",
        wgsl_struct: "Bodies",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "water_body.wgsl",
        wgsl_struct: "Frame",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "water_body.wgsl",
        wgsl_struct: "Obstacles",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "water_body.wgsl",
        wgsl_struct: "Ocean",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "water_body.wgsl",
        wgsl_struct: "Shore",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "water_body.wgsl",
        wgsl_struct: "Swell",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "water_body.wgsl",
        wgsl_struct: "TileParams",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "water_ocean.wgsl",
        wgsl_struct: "Params",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "xr_eye_composite.wgsl",
        wgsl_struct: "Params",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "xr_mirror.wgsl",
        wgsl_struct: "Mirror",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "xr_quad.wgsl",
        wgsl_struct: "Panel",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "xr_stereo_forward.wgsl",
        wgsl_struct: "Object",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "xr_stereo_forward.wgsl",
        wgsl_struct: "Single",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "xr_stereo_forward.wgsl",
        wgsl_struct: "Stereo",
        reason: FIRST_TABLE,
    },
    AllowEntry {
        file: "vfx gather",
        wgsl_struct: "Frame",
        reason: MERGED_IN_THE_ROLL_UP,
    },
];

/// The ratchet over all shaders.
///
/// # Errors
///
/// A module fails to compose or parse.
pub fn report() -> Result<ContractReport, String> {
    let plain = crate::dx12_shaders::plain_sources();
    let plain: Vec<(&str, &str)> = plain.iter().map(|(n, s)| (n.as_str(), s.as_str())).collect();
    let composed = crate::dx12_shaders::composed_sources();
    regia_render::ratchet_sources(true, &plain, &composed, &rows(), &allowlist())
}

/// The binding counts of every preset of `scope` against the device-class limits.
#[must_use]
pub fn limits(scope: MatrixScope) -> LimitsReport {
    regia_render::run_shader_limits(&regia_render::shader_matrix_inputs(scope))
}

#[cfg(test)]
mod tests {
    /// Every uniform and storage struct of every engine shader is checked against its Rust type or
    /// listed with a reason; the list only shrinks.
    #[test]
    fn every_bound_struct_of_the_engine_is_checked_or_allowed() {
        let report = super::report().expect("compose");
        assert!(
            report.ok(),
            "{} problems:\n{}",
            report.problems().len(),
            report.problems().join("\n")
        );
    }
}
