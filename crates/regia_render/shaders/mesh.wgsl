// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::mesh

// Column-free Mat3x4: three rows, translation in w. Matches `JointMatrix`.

struct Mat3x4 {
    row_x: vec4<f32>,
    row_y: vec4<f32>,
    row_z: vec4<f32>,
}

fn mul_mat3x4(m: Mat3x4, p: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        dot(m.row_x.xyz, p) + m.row_x.w,
        dot(m.row_y.xyz, p) + m.row_y.w,
        dot(m.row_z.xyz, p) + m.row_z.w,
    );
}

fn mul_mat3x4_dir(m: Mat3x4, d: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(m.row_x.xyz, d), dot(m.row_y.xyz, d), dot(m.row_z.xyz, d));
}

fn skin_position(pos: vec3<f32>, j0: Mat3x4, j1: Mat3x4, j2: Mat3x4, j3: Mat3x4, weights: vec4<f32>) -> vec3<f32> {
    let wsum = weights.x + weights.y + weights.z + weights.w;
    if wsum < 1e-5 {
        return pos;
    }
    return weights.x * mul_mat3x4(j0, pos)
        + weights.y * mul_mat3x4(j1, pos)
        + weights.z * mul_mat3x4(j2, pos)
        + weights.w * mul_mat3x4(j3, pos);
}

fn skin_direction(dir: vec3<f32>, j0: Mat3x4, j1: Mat3x4, j2: Mat3x4, j3: Mat3x4, weights: vec4<f32>) -> vec3<f32> {
    let wsum = weights.x + weights.y + weights.z + weights.w;
    if wsum < 1e-5 {
        return dir;
    }
    return weights.x * mul_mat3x4_dir(j0, dir)
        + weights.y * mul_mat3x4_dir(j1, dir)
        + weights.z * mul_mat3x4_dir(j2, dir)
        + weights.w * mul_mat3x4_dir(j3, dir);
}
