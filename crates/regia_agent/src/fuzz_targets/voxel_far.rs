// SPDX-License-Identifier: MPL-2.0
//! The `voxel-far` target (ADR-2445, ADR-2655, ADR-3108, ADR-3227): a cooked voxel far proxy blob
//! (`.fvox` v4, with sub-voxel boxes, per-level sampled box errors and coverage estimates). The
//! loader parses and validates it as the runtime does (`VoxelFarProxy::from_bytes`, which also
//! reads v1 to v3), extracts every level's surface the way the visibility buffer would, and
//! re-encodes it at the current version.

use regia_render::visbuffer::{
    VOXEL_FAR_FORMAT_VERSION, VoxelBrick, VoxelFarProxy, VoxelLevel, VoxelSample, pack_voxel_extent,
};

use super::{FuzzInput, FuzzTarget};

/// A 4³ grid with a 2×2×2 core (flat, thin and full boxes) and its 2³ and 1³ mips, the level
/// errors set to the boxes' bounds, sampled box errors below them and a coverage estimate per
/// level.
fn fixture() -> Vec<u8> {
    let mut proxy = VoxelFarProxy {
        origin: [-1.0; 3],
        extent: 2.0,
        bounds: [0.0, 0.0, 0.0, 1.0],
        source_triangles: 24,
        ..VoxelFarProxy::default()
    };
    let boxes = [
        pack_voxel_extent([0, 0, 7], [15, 15, 7]),
        pack_voxel_extent([7, 0, 7], [8, 15, 8]),
        pack_voxel_extent([0; 3], [15; 3]),
        pack_voxel_extent([2, 3, 4], [9, 15, 6]),
    ];
    // Level 0: the core voxels 1..=2 on every axis (bits x + 4y + 16z).
    let mut mask = 0u64;
    for z in 1..3u32 {
        for y in 1..3u32 {
            for x in 1..3u32 {
                mask |= 1 << (x + 4 * y + 16 * z);
            }
        }
    }
    for (level, (resolution, mask)) in [(4u32, mask), (2, 0x0033_0033), (1, 1)].into_iter().enumerate() {
        let voxel_offset = proxy.voxels.len() as u32;
        let count = mask.count_ones() as usize;
        for i in 0..count {
            proxy.voxels.push(VoxelSample::new(
                [0.0, 1.0, 0.0],
                [0.2, 0.5, 0.1],
                (i as f32 + 1.0) / count as f32,
                [0.5, 0.5],
            ));
            proxy.extents.push(boxes[(i + level) % boxes.len()]);
        }
        let brick_offset = proxy.bricks.len() as u32;
        proxy.bricks.push(VoxelBrick {
            coord: 0,
            voxel_offset,
            mask_lo: mask as u32,
            mask_hi: (mask >> 32) as u32,
        });
        proxy.levels.push(VoxelLevel {
            resolution,
            voxel_size: 2.0 / resolution as f32,
            error: 0.0,
            brick_offset,
            brick_count: 1,
        });
    }
    let errors: Vec<f32> = (0..proxy.levels.len())
        .map(|l| proxy.level_extent_error(l))
        .collect();
    for (level, error) in proxy.levels.iter_mut().zip(errors) {
        level.error = error;
    }
    proxy.sampled_errors = proxy.levels.iter().map(|l| l.error * 0.5).collect();
    proxy.coverage = vec![0.97, 1.08, 1.6];
    proxy.to_bytes()
}

fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let proxy = VoxelFarProxy::from_bytes(bytes).map_err(|e| e.to_string())?;
    for level in 0..proxy.levels.len().min(4) {
        let _ = proxy.surface_with_dropout(level, true);
    }
    Ok(proxy.to_bytes())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "voxel-far",
        format: Some("voxel-far"),
        format_version: VOXEL_FAR_FORMAT_VERSION,
        round_trip: true,
        input: FuzzInput::Bytes { fixture, load },
    }]
}
