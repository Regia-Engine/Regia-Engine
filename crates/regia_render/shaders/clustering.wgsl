// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::clustering

// Froxel grid from specs/13 § 5. Exponential Z slices follow Olsson, Billeter,
// Assarsson, "Clustered Deferred and Forward Shading", HPG 2012.

const GRID_X: u32 = 16u;
const GRID_Y: u32 = 9u;
const GRID_Z: u32 = 24u;

fn cluster_index(cell_x: u32, cell_y: u32, cell_z: u32) -> u32 {
    return cell_x + GRID_X * (cell_y + GRID_Y * cell_z);
}

fn cluster_count() -> u32 {
    return GRID_X * GRID_Y * GRID_Z;
}

fn slice_depth(slice: f32, near: f32, far: f32) -> f32 {
    return near * pow(far / near, slice / f32(GRID_Z));
}

fn depth_to_slice(depth: f32, near: f32, far: f32) -> u32 {
    if depth <= near {
        return 0u;
    }
    if depth >= far {
        return GRID_Z - 1u;
    }
    let t = log(depth / near) / log(far / near);
    let ez = u32(floor(t * f32(GRID_Z)));
    return min(ez, GRID_Z - 1u);
}
