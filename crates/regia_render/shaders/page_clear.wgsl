// SPDX-License-Identifier: MPL-2.0
// Fullscreen triangle that writes far depth (1). Used to clear one shadow page.

struct ClearOut {
    @builtin(position) clip: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> ClearOut {
    // Corners (-1,-1), (3,-1), (-1,3). Scalars, not a function-local array:
    // naga tags that array with ArrayStride and SPIR-V 1.6 rejects it.
    let x = select(-1.0, 3.0, vertex_index == 1u);
    let y = select(-1.0, 3.0, vertex_index == 2u);
    return ClearOut(vec4<f32>(x, y, 1.0, 1.0));
}
