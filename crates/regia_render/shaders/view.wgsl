// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::view

struct FrameUniform {
    view_proj: mat4x4<f32>,
    camera_pos: vec3<f32>,
    pad_a: u32,
}

@group(0) @binding(0)
var<uniform> frame: FrameUniform;
