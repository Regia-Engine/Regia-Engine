// SPDX-License-Identifier: MPL-2.0
// The radiance cache's incremental update (compute, after the probes' update). The CPU planner
// (`radiance_cache.rs`) lists up to a budget of cells per frame; each one reads the probes at its
// center toward the six ambient-cube directions, with the probes' own visibility and cascade
// blend. Composed as ddgi_common.wgsl + ddgi_sample.wgsl + radiance_cache_common.wgsl + this file.

@group(0) @binding(0) var<uniform> ddgi: DdgiUniform;
@group(0) @binding(1) var<storage, read> ddgi_data: array<u32>;

@group(1) @binding(0) var<uniform> rc: RcUniform;
@group(1) @binding(1) var<storage, read_write> rc_cells: array<u32>;
@group(1) @binding(2) var<storage, read> rc_queue: array<u32>;

// One face of the stored cube B from the probes' irradiance toward it (`e`), toward its opposite
// (`opposite`) and the sum of all six (`total`), packed.
fn rc_pack_face(e: vec3<f32>, opposite: vec3<f32>, total: vec3<f32>) -> u32 {
    let b = e * 3.0 + opposite - total * 0.5;
    return ddgi_pack_rgb9e5(max(b, vec3<f32>(0.0)));
}

@compute @workgroup_size(64)
fn update_cells(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= rc.counts.x) {
        return;
    }
    let slot = rc_queue[gid.x];
    let count = rc_count();
    let level = slot / count;
    let local_slot = slot % count;
    let d = vec3<i32>(rc.dims.xyz);
    let s = vec3<i32>(
        i32(local_slot % rc.dims.x),
        i32((local_slot / rc.dims.x) % rc.dims.y),
        i32(local_slot / (rc.dims.x * rc.dims.y)),
    );
    // The world cell ≡ the slot (mod dims) inside the level's grid.
    let base = rc.bases[level].xyz;
    let cell = base + (((s - base) % d) + d) % d;
    let p = vec3<f32>(cell) * rc.spacing[level];
    let word = slot * RC_CELL_WORDS;
    // Six named faces, not a function-local array: that carries naga's `ArrayStride`, which Vulkan
    // forbids in Function storage (ADR-026; the shader matrix's `radiance_cache_update.wgsl`
    // layout scan, ADR-2644). The face's own normal steers the probes' side weight and bias; no
    // viewer.
    let r0 = ddgi_irradiance(p, ddgi_cube_dir(0u), vec3<f32>(0.0));
    let r1 = ddgi_irradiance(p, ddgi_cube_dir(1u), vec3<f32>(0.0));
    let r2 = ddgi_irradiance(p, ddgi_cube_dir(2u), vec3<f32>(0.0));
    let r3 = ddgi_irradiance(p, ddgi_cube_dir(3u), vec3<f32>(0.0));
    let r4 = ddgi_irradiance(p, ddgi_cube_dir(4u), vec3<f32>(0.0));
    let r5 = ddgi_irradiance(p, ddgi_cube_dir(5u), vec3<f32>(0.0));
    let covered = r0.a > 0.0 && r1.a > 0.0 && r2.a > 0.0 && r3.a > 0.0 && r4.a > 0.0 && r5.a > 0.0;
    let e0 = max(r0.rgb, vec3<f32>(0.0));
    let e1 = max(r1.rgb, vec3<f32>(0.0));
    let e2 = max(r2.rgb, vec3<f32>(0.0));
    let e3 = max(r3.rgb, vec3<f32>(0.0));
    let e4 = max(r4.rgb, vec3<f32>(0.0));
    let e5 = max(r5.rgb, vec3<f32>(0.0));
    let total = vec3<f32>(0.0) + e0 + e1 + e2 + e3 + e4 + e5;
    // The probes hold irradiance: light already cosine-filtered around each face. A ray wants the
    // light from its own direction, so the cell stores the ambient cube B whose cosine integral gives
    // back those irradiances at the six axes. With L(w) = sum w_a^2 B(sign w_a) under a cosine lobe,
    //   E(+a) = 3/8 B(+a) - 1/8 B(-a) + 1/8 S,   S = sum of the six B = sum of the six E,
    // which inverts to B(+a) = 3 E(+a) + E(-a) - S / 2. Uniform light comes back unchanged; a negative
    // face (light too sharp for this basis) is clamped to none. Face j's opposite is j ^ 1.
    rc_cells[word] = rc_pack_face(e0, e1, total);
    rc_cells[word + 1u] = rc_pack_face(e1, e0, total);
    rc_cells[word + 2u] = rc_pack_face(e2, e3, total);
    rc_cells[word + 3u] = rc_pack_face(e3, e2, total);
    rc_cells[word + 4u] = rc_pack_face(e4, e5, total);
    rc_cells[word + 5u] = rc_pack_face(e5, e4, total);
    // Coverage 0 (probes not traced yet) or −1 (outside every cascade): the cell reads as absent.
    rc_cells[word + RC_TAG_WORD] = select(0u, rc_tag(cell), covered);
}
