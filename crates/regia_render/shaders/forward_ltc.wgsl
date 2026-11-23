// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::forward::ltc

// Linearly transformed cosines: the clipped-polygon form factor of an emissive area light (Heitz et al. 2016, as in `ltc.rs`).
// Split out of `lab_forward.wgsl` unchanged (ADR-1552); imported there by name.

#import regia::pbr::lighting

fn safe_unit(v: vec3<f32>) -> vec3<f32> {
    return v * inverseSqrt(max(dot(v, v), 1e-20));
}

// θ · (a × b) / sin θ for unit vectors a → b (the series near θ = 0, where acos is imprecise).
fn ltc_edge(a: vec3<f32>, b: vec3<f32>) -> vec3<f32> {
    let x = clamp(dot(a, b), -1.0, 1.0);
    var k = 1.0 + (1.0 - x) / 3.0;
    if (x <= 0.9999) {
        k = acos(x) * inverseSqrt(max(1.0 - x * x, 1e-12));
    }
    return cross(a, b) * k;
}

// Form factor of the convex quad p0..p3 (relative to the shading point, z = the lobe's axis),
// clipped to z ≥ 0. Positive when the quad's front face (winding p0 → p1 → p2) faces the point.
fn ltc_polygon(p0: vec3<f32>, p1: vec3<f32>, p2: vec3<f32>, p3: vec3<f32>) -> f32 {
    var sum = vec3<f32>(0.0);
    var exit_p = vec3<f32>(0.0);
    var entry_p = vec3<f32>(0.0);
    var have_exit = false;
    var have_entry = false;
    for (var i = 0u; i < 4u; i = i + 1u) {
        var a = p0;
        var b = p1;
        if (i == 1u) {
            a = p1;
            b = p2;
        } else if (i == 2u) {
            a = p2;
            b = p3;
        } else if (i == 3u) {
            a = p3;
            b = p0;
        }
        let ua = a.z >= 0.0;
        let ub = b.z >= 0.0;
        if (ua && ub) {
            sum += ltc_edge(safe_unit(a), safe_unit(b));
        } else if (ua != ub) {
            let t = a.z / (a.z - b.z);
            let c = vec3<f32>(a.xy + (b.xy - a.xy) * t, 0.0);
            if (ua) {
                sum += ltc_edge(safe_unit(a), safe_unit(c));
                exit_p = c;
                have_exit = true;
            } else {
                sum += ltc_edge(safe_unit(c), safe_unit(b));
                entry_p = c;
                have_entry = true;
            }
        }
    }
    if (have_exit && have_entry) {
        sum += ltc_edge(safe_unit(exit_p), safe_unit(entry_p));
    }
    return -sum.z / (2.0 * regia::pbr::lighting::PI);
}

// Form factor of a sphere at `to_center` (relative to the shading point, z = the lobe's axis) through the matrix `m`
// (identity for the clamped cosine, `ltc_inverse` for the GGX lobe; `m` and `to_center` are in the tangent frame).
// The silhouette is a disk facing the point: a regular 12-gon of the disk's area, clipped to z ≥ 0 after `m`
// (Heitz et al. 2016 § 3 for polygons; the disk as a polygon is original). CPU mirror: `ltc::sphere_form_factor`.
// The edge sum loses f32 precision under about 1° of angular radius, so the caller sends those to the
// representative point (`LTC_SPHERE_MIN_TAN`).
fn ltc_sphere(m: mat3x3<f32>, to_center: vec3<f32>, radius: f32) -> f32 {
    let d2 = dot(to_center, to_center);
    let axis = to_center * inverseSqrt(max(d2, 1e-12));
    // sqrt(2π / (12 sin(2π / 12))): the circumradius of a 12-gon with the circle's area. A point inside the sphere
    // sees the widest cone the clamp allows, near the whole hemisphere.
    let tan_r = radius * inverseSqrt(max(d2 - radius * radius, 1e-4 * d2)) * 1.0233267;
    let e1 = safe_unit(cross(axis, select(vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(1.0, 0.0, 0.0), abs(axis.z) > 0.9))) * tan_r;
    let e2 = cross(axis, e1);
    let first = m * (axis + e1);
    var a = first;
    var c = 1.0;
    var s = 0.0;
    var sum = vec3<f32>(0.0);
    var exit_p = vec3<f32>(0.0);
    var entry_p = vec3<f32>(0.0);
    var have_exit = false;
    var have_entry = false;
    for (var i = 0u; i < 12u; i = i + 1u) {
        // cos 30°, sin 30°.
        let c2 = c * 0.8660254 - s * 0.5;
        s = s * 0.8660254 + c * 0.5;
        c = c2;
        var b = first;
        if (i < 11u) {
            b = m * (axis + e1 * c + e2 * s);
        }
        let ua = a.z >= 0.0;
        let ub = b.z >= 0.0;
        if (ua && ub) {
            sum += ltc_edge(safe_unit(a), safe_unit(b));
        } else if (ua != ub) {
            let t = a.z / (a.z - b.z);
            let k = vec3<f32>(a.xy + (b.xy - a.xy) * t, 0.0);
            if (ua) {
                sum += ltc_edge(safe_unit(a), safe_unit(k));
                exit_p = k;
                have_exit = true;
            } else {
                sum += ltc_edge(safe_unit(k), safe_unit(b));
                entry_p = k;
                have_entry = true;
            }
        }
        a = b;
    }
    if (have_exit && have_entry) {
        sum += ltc_edge(safe_unit(exit_p), safe_unit(entry_p));
    }
    return abs(sum.z) / (2.0 * regia::pbr::lighting::PI);
}
