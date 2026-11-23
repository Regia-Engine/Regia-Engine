// SPDX-License-Identifier: MPL-2.0
//! `assets.voxel_far { asset, viewport_height?, fov_y?, threshold_px? }` (ADR-2445,
//! `book/agents/voxel-far.md`): a cooked voxel far proxy read through the running
//! [`AssetServer`]: its levels (resolution, voxel size, error, bricks, voxels), the cube faces
//! each level draws, its memory, and the distance at which each level takes over for a viewport.
//! Read-only.
//!
//! `render.voxel_far_stats {}` (ADR-2655): what the voxel far levels add to the meshes the
//! framework bridge (`regia_framework::meshlet_bridge`) put on the visibility buffer, the proxies
//! that fell back to plain meshlets, and the path's last-frame and arena counts. Read-only.

use regia_app::prelude::*;
use regia_asset::AssetServer;
use regia_render::visbuffer::{VOXEL_FAR_FORMAT_VERSION, VoxelFarLoadError, VoxelFarProxy};
use serde_json::{Value, json};

use crate::protocol::RpcError;

const METHOD: &str = "assets.voxel_far";

/// GPU words one vertex takes in the visibility buffer's geometry arena.
const VERTEX_WORDS: u64 = 16;

fn number(params: &Value, name: &str, default: f64, min: f64, max: f64) -> Result<f64, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && *x >= min && *x <= max)
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "{METHOD}: `{name}` must be a number in {min}..={max} [voxel_far_bad_param]"
                ))
            }),
    }
}

/// The report of one proxy for a viewport (`pixels_per_unit` = height / (2 tan(fov_y / 2))).
pub(crate) fn report_json(
    path: &str,
    id: &str,
    proxy: &VoxelFarProxy,
    pixels_per_unit: f32,
    threshold_px: f32,
) -> Value {
    let radius = proxy.bounds[3];
    let levels: Vec<Value> = proxy
        .levels
        .iter()
        .enumerate()
        .map(|(i, level)| {
            let surface = proxy.surface(i);
            let triangles = surface.indices.len() / 3;
            let vertices = surface.vertices.len() as u64;
            // Vertices, one cluster vertex-list entry per vertex and one word per triangle.
            let gpu_bytes = (vertices * VERTEX_WORDS + vertices + triangles as u64) * 4;
            // Coverage (ADR-3108): the mean voxel opacity and how many voxels the default opacity
            // dropout draws, so a crown cooked from masked cards can be told from a solid blob.
            let (mut opacity, mut count, mut kept) = (0.0f64, 0u64, 0u64);
            for (xyz, sample) in proxy.level_voxels(i) {
                opacity += f64::from(sample.opacity());
                count += 1;
                kept += u64::from(VoxelFarProxy::keeps(i, xyz, &sample, true));
            }
            let mean_opacity = if count == 0 { 0.0 } else { opacity / count as f64 };
            json!({
                "level": i,
                "resolution": level.resolution,
                "voxel_size": level.voxel_size,
                "error": level.error,
                "cube_error": level.voxel_size * 3f32.sqrt(),
                "box_error": proxy.level_box_error(i),
                "bricks": level.brick_count,
                "voxels": proxy.voxel_count(i),
                "surface_triangles": triangles,
                "surface_vertices": vertices,
                "gpu_bytes": gpu_bytes,
                "switch_distance_m": VoxelFarProxy::switch_distance(level.error, radius, pixels_per_unit, threshold_px),
                "mean_opacity": mean_opacity,
                "kept_voxels": kept,
                // The cook's estimate of the level's screen coverage at its switch distance as a
                // share of the alpha-tested source's (ADR-3227); null before format 4.
                "coverage_at_switch": proxy.coverage.get(i).map(|&c| f64::from(c)),
            })
        })
        .collect();
    // Per asset (ADR-3227): the first level's coverage (where the clusters hand over) and the
    // range over every level, so a canopy that thins or swells with distance shows at a glance.
    let coverage = (!proxy.coverage.is_empty()).then(|| {
        let min = proxy.coverage.iter().copied().fold(f32::INFINITY, f32::min);
        let max = proxy.coverage.iter().copied().fold(0.0f32, f32::max);
        json!({
            "first_level": f64::from(proxy.coverage[0]),
            "min": f64::from(min),
            "max": f64::from(max),
        })
    });
    json!({
        "asset": path,
        "id": id,
        "format_version": VOXEL_FAR_FORMAT_VERSION,
        "source_triangles": proxy.source_triangles,
        "origin": proxy.origin,
        "extent": proxy.extent,
        "bounds": proxy.bounds,
        "bytes": proxy.byte_size(),
        "bricks": proxy.bricks.len(),
        "voxels": proxy.voxels.len(),
        "boxes": !proxy.extents.is_empty(),
        "coverage_at_switch": coverage,
        "levels": levels,
    })
}

/// `assets.voxel_far`: the cooked voxel far proxy `asset` (the sub-asset's project-relative path,
/// `<gltf>#voxels/<node>/primitive_<i>`, or its id).
pub fn voxel_far(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !matches!(params.get("asset"), Some(Value::String(asset)) if !asset.is_empty()) {
        return Err(RpcError::invalid_params(format!(
            "{METHOD}: `asset` (the voxel sub-asset's path, <gltf>#voxels/<node>/primitive_<i>, or its id) is required [voxel_far_bad_param]"
        )));
    }
    let height = number(params, "viewport_height", 1440.0, 1.0, 16384.0)?;
    let fov_y = number(params, "fov_y", 1.0, 0.01, 3.1)?;
    let threshold_px = number(params, "threshold_px", 1.0, 0.01, 1024.0)?;
    let Some(mut server) = world.get_resource_mut::<AssetServer>() else {
        return Err(crate::assets::no_asset_server());
    };
    let (path, id) = crate::assets::resolve_asset(&server, params)?;
    let proxy = VoxelFarProxy::load(&mut server, id).map_err(|e| load_error(&path, &e))?;
    let pixels_per_unit = (height / (2.0 * (fov_y * 0.5).tan())) as f32;
    Ok(report_json(
        &path,
        &id.to_string(),
        &proxy,
        pixels_per_unit,
        threshold_px as f32,
    ))
}

/// `render.voxel_far_stats`: answers in any world (zeros and nulls for what is absent).
pub fn stats(world: &mut World) -> Result<Value, RpcError> {
    use regia_framework::meshlet_bridge::MeshletBridgeState;
    let state = world.get_resource::<MeshletBridgeState>();
    let s = state.map(MeshletBridgeState::voxel_far_stats).unwrap_or_default();
    let fallbacks: Vec<Value> = state
        .map(|state| {
            state
                .voxel_far_fallbacks()
                .iter()
                .map(|e| json!({ "asset": e.id.to_string(), "code": e.code, "message": e.message }))
                .collect()
        })
        .unwrap_or_default();
    let renderer = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map(|r| &r.0);
    let last_frame = renderer.map(|r| {
        let v = r.visbuffer_stats();
        json!({
            "frames": v.frames,
            "instances": v.instances,
            "candidate_clusters": v.candidate_clusters,
            "resident_meshes": v.resident_meshes,
            "geometry_bytes": v.geometry_bytes,
        })
    });
    let arena = renderer.map(|r| {
        let a = r.visbuffer_arena_stats();
        json!({ "meshes": a.meshes, "pending": a.pending, "clusters": a.clusters, "used_bytes": a.used_bytes })
    });
    Ok(json!({
        "bridge": state.is_some(),
        "entities": s.entities,
        "meshes": s.meshes,
        "meshes_without_levels": s.meshes_without_levels,
        "levels": s.levels,
        "clusters": s.clusters,
        "triangles": s.triangles,
        "vertices": s.vertices,
        "gpu_bytes": s.gpu_bytes,
        "fallbacks": fallbacks,
        "last_frame": last_frame,
        "arena": arena,
    }))
}

fn load_error(path: &str, e: &VoxelFarLoadError) -> RpcError {
    let code = e.code();
    if matches!(e, VoxelFarLoadError::NotCooked(_)) {
        return RpcError::new(
            crate::protocol::NOT_FOUND,
            "F0016",
            format!("{METHOD}: {path} has no cooked voxel proxy: {e} [{code}]"),
            "add voxel_far: Some(()) to the glTF's .meta settings and run regia cook",
        );
    }
    RpcError::new(
        crate::protocol::INVALID_PARAMS,
        "F0013",
        format!("{METHOD}: the voxel proxy {path} does not read: {e} [{code}]"),
        "re-cook the glTF (regia cook)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::visbuffer::{VoxelBrick, VoxelLevel, VoxelSample};

    fn one_voxel() -> VoxelFarProxy {
        VoxelFarProxy {
            origin: [0.0; 3],
            extent: 2.0,
            bounds: [1.0, 1.0, 1.0, 1.7],
            source_triangles: 12,
            levels: vec![VoxelLevel {
                resolution: 1,
                voxel_size: 2.0,
                error: 2.0 * 1.732,
                brick_offset: 0,
                brick_count: 1,
            }],
            bricks: vec![VoxelBrick {
                coord: 0,
                voxel_offset: 0,
                mask_lo: 1,
                mask_hi: 0,
            }],
            voxels: vec![VoxelSample::new([0.0, 1.0, 0.0], [0.5; 3], 1.0, [0.0; 2])],
            extents: Vec::new(),
            sampled_errors: Vec::new(),
            coverage: Vec::new(),
        }
    }

    #[test]
    fn stats_answer_in_a_bare_world_and_count_the_bridge() {
        let mut world = World::new();
        let got = stats(&mut world).expect("bare world");
        assert_eq!(got["bridge"], json!(false));
        assert_eq!(got["meshes"], json!(0));
        assert!(got["last_frame"].is_null() && got["fallbacks"].as_array().is_some_and(Vec::is_empty));
        world.insert_resource(regia_framework::meshlet_bridge::MeshletBridgeState::default());
        world.insert_resource(regia_render::RuntimeRenderer::default());
        let got = stats(&mut world).expect("with bridge");
        assert_eq!(got["bridge"], json!(true));
        assert_eq!(got["arena"]["meshes"], json!(0));
        assert_eq!(got["last_frame"]["frames"], json!(0));
    }

    #[test]
    fn the_report_lists_levels_faces_memory_and_switch_distances() {
        let proxy = one_voxel();
        proxy.validate().expect("valid");
        let report = report_json(
            "assets/tree.gltf#voxels/Tree/primitive_0",
            "id",
            &proxy,
            1000.0,
            1.0,
        );
        assert_eq!(report["format_version"], json!(VOXEL_FAR_FORMAT_VERSION));
        assert_eq!(report["voxels"], json!(1));
        let level = &report["levels"][0];
        assert_eq!(level["surface_triangles"], json!(12));
        assert_eq!(level["surface_vertices"], json!(8));
        assert_eq!(level["gpu_bytes"], json!((8 * 16 + 8 + 12) * 4));
        let d = level["switch_distance_m"].as_f64().expect("number");
        assert!((d - (2.0 * 1.732 * 1000.0 + 1.7)).abs() < 0.01, "{d}");
        assert_eq!(level["kept_voxels"], json!(1), "an opaque voxel is always drawn");
        assert!((level["mean_opacity"].as_f64().expect("number") - 1.0).abs() < 0.01);
    }

    /// The coverage fields (ADR-3108): a cut-away voxel reads its opacity, and the dropout draws
    /// none of a fully transparent level.
    #[test]
    fn the_report_shows_each_levels_coverage() {
        let mut proxy = one_voxel();
        proxy.voxels[0] = VoxelSample::new([0.0, 1.0, 0.0], [0.5; 3], 0.0, [0.0; 2]);
        let report = report_json("p", "id", &proxy, 1000.0, 1.0);
        let level = &report["levels"][0];
        assert_eq!(level["kept_voxels"], json!(0));
        assert!(level["mean_opacity"].as_f64().expect("number") < 0.01);
        proxy.voxels[0] = VoxelSample::new([0.0, 1.0, 0.0], [0.5; 3], 0.5, [0.0; 2]);
        let half = report_json("p", "id", &proxy, 1000.0, 1.0);
        let opacity = half["levels"][0]["mean_opacity"].as_f64().expect("number");
        assert!((opacity - 0.5).abs() < 0.02, "{opacity}");
    }

    /// The coverage at the switch distance (ADR-3227): null per level and per asset for a proxy
    /// cooked before format 4, the cook's numbers per level and the first level, minimum and
    /// maximum per asset otherwise.
    #[test]
    fn the_report_shows_the_coverage_at_the_switch_distance_per_asset() {
        let mut proxy = one_voxel();
        let old = report_json("p", "id", &proxy, 1000.0, 1.0);
        assert_eq!(old["coverage_at_switch"], Value::Null);
        assert_eq!(old["levels"][0]["coverage_at_switch"], Value::Null);
        proxy.coverage = vec![0.875];
        let report = report_json("p", "id", &proxy, 1000.0, 1.0);
        assert_eq!(report["levels"][0]["coverage_at_switch"], json!(0.875));
        assert_eq!(
            report["coverage_at_switch"],
            json!({ "first_level": 0.875, "min": 0.875, "max": 0.875 })
        );
    }

    #[test]
    fn parameters_are_checked_and_load_errors_carry_codes() {
        let mut world = World::new();
        let err = voxel_far(&mut world, &json!({})).expect_err("asset required");
        assert!(err.message.contains("[voxel_far_bad_param]"), "{}", err.message);
        let err = voxel_far(&mut world, &json!({ "asset": "a#voxels/x", "fov_y": 9.0 })).expect_err("range");
        assert!(err.message.contains("[voxel_far_bad_param]"));
        let missing = load_error("p", &VoxelFarLoadError::NotCooked("nothing".into()));
        assert!(missing.message.contains("[voxel_far_not_cooked]"));
        let old = load_error(
            "p",
            &VoxelFarLoadError::Format(regia_render::visbuffer::VoxelFarFormatError::Version { found: 0 }),
        );
        assert!(old.message.contains("[voxel_far_old_version]"));
    }
}
