// SPDX-License-Identifier: MPL-2.0
//! Mesh builders and the validation scenes of `specs/27` § 1.5.
//!
//! Cornell box geometry: the Cornell University Program of Computer Graphics' public Cornell box
//! data (vertex lists in millimetres, camera at (278, 273, −800) looking +Z, 35 mm lens on a
//! 25 mm film), scaled to metres. Reflectances and the light are RGB fits of the published
//! spectra (white 0.725/0.71/0.68, red 0.63/0.065/0.05, green 0.14/0.45/0.091, light 17/12/4),
//! the values public research scene repositories use.

use regia_core::glam::{Mat4, Vec3};

use crate::scene::{PtCamera, PtLight, PtMaterial, PtMesh, PtScene, PtSky, PtSun};

/// UV sphere of `radius` with `segments` around and `rings` from pole to pole. `inward` flips the
/// winding and normals (a closed room).
#[must_use]
pub fn uv_sphere(radius: f32, segments: u32, rings: u32, inward: bool) -> PtMesh {
    let mut m = PtMesh::default();
    for r in 0..=rings {
        let v = r as f32 / rings as f32;
        let theta = v * std::f32::consts::PI;
        for s in 0..=segments {
            let u = s as f32 / segments as f32;
            let phi = u * std::f32::consts::TAU;
            let n = [theta.sin() * phi.cos(), theta.cos(), theta.sin() * phi.sin()];
            m.positions.push([n[0] * radius, n[1] * radius, n[2] * radius]);
            m.normals.push(if inward { [-n[0], -n[1], -n[2]] } else { n });
            m.uvs.push([u, v]);
        }
    }
    let row = segments + 1;
    for r in 0..rings {
        for s in 0..segments {
            let a = r * row + s;
            let b = a + row;
            let (t0, t1) = if inward {
                ([a, b, a + 1], [a + 1, b, b + 1])
            } else {
                ([a, a + 1, b], [b, a + 1, b + 1])
            };
            m.indices.extend_from_slice(&t0);
            m.indices.extend_from_slice(&t1);
        }
    }
    m
}

/// A planar quad (corners in order; the normal follows the right-hand rule of p0→p1→p2).
#[must_use]
pub fn quad(p: [[f32; 3]; 4]) -> PtMesh {
    let a = Vec3::from_array(p[0]);
    let n = (Vec3::from_array(p[1]) - a)
        .cross(Vec3::from_array(p[2]) - a)
        .normalize_or(Vec3::Y)
        .to_array();
    PtMesh {
        positions: p.to_vec(),
        normals: vec![n; 4],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        colors: Vec::new(),
        tangents: Vec::new(),
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}

/// Appends `other` to `m`.
pub fn merge(m: &mut PtMesh, other: &PtMesh) {
    let base = m.positions.len() as u32;
    m.positions.extend_from_slice(&other.positions);
    m.normals.extend_from_slice(&other.normals);
    m.uvs.extend_from_slice(&other.uvs);
    m.indices.extend(other.indices.iter().map(|i| i + base));
}

/// A prism: the quad `top` (counter-clockwise seen from above) extruded down to y = `bottom`,
/// sides and top only.
#[must_use]
pub fn prism(top: [[f32; 3]; 4], bottom: f32) -> PtMesh {
    let mut m = PtMesh::default();
    // Top faces +Y: wind so the normal points up.
    let t = quad([top[0], top[3], top[2], top[1]]);
    let up = t.normals[0][1] > 0.0;
    merge(&mut m, &if up { t } else { quad(top) });
    for k in 0..4 {
        let a = top[k];
        let b = top[(k + 1) % 4];
        let side = quad([[a[0], bottom, a[2]], [b[0], bottom, b[2]], b, a]);
        // Point the side normal away from the prism's centre.
        let c = top.iter().fold(Vec3::ZERO, |s, p| s + Vec3::from_array(*p)) / 4.0;
        let mid = (Vec3::from_array(a) + Vec3::from_array(b)) * 0.5;
        let out = Vec3::new(mid.x - c.x, 0.0, mid.z - c.z);
        let side = if Vec3::from_array(side.normals[0]).dot(out) >= 0.0 {
            side
        } else {
            quad([a, b, [b[0], bottom, b[2]], [a[0], bottom, a[2]]])
        };
        merge(&mut m, &side);
    }
    m
}

/// § 1.5 item 1: a sphere filling the frame inside a uniform environment of radiance 1.
/// Every pixel converges to the material's directional albedo at its view angle.
#[must_use]
pub fn furnace_sphere(material: PtMaterial, size: u32) -> PtScene {
    let mut scene = PtScene::new(PtCamera {
        eye: [0.0, 0.0, 3.0],
        target: [0.0; 3],
        fov_y: 0.25,
        width: size,
        height: size,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    });
    let mesh = scene.add_mesh(uv_sphere(1.0, 96, 48, false));
    let mat = scene.add_material(material);
    scene.add_instance(mesh, mat, Mat4::IDENTITY);
    scene.sky = PtSky::Uniform([1.0; 3]);
    scene
}

/// § 1.5 item 3: a large plane at y = 0 under a disc sun of `illuminance` lux at `elevation`
/// radians, seen from above at an angle.
#[must_use]
pub fn sun_plane(
    material: PtMaterial,
    elevation: f32,
    illuminance: f32,
    angular_radius: f32,
    size: u32,
) -> PtScene {
    let mut scene = PtScene::new(PtCamera {
        eye: [0.0, 2.0, 2.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.4,
        width: size,
        height: size,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    });
    let mesh = scene.add_mesh(quad([
        [-50.0, 0.0, -50.0],
        [-50.0, 0.0, 50.0],
        [50.0, 0.0, 50.0],
        [50.0, 0.0, -50.0],
    ]));
    let mat = scene.add_material(material);
    scene.add_instance(mesh, mat, Mat4::IDENTITY);
    scene.sun = Some(PtSun {
        toward: [elevation.cos() * 0.6, elevation.sin(), elevation.cos() * -0.8],
        color: [1.0; 3],
        illuminance,
        angular_radius,
    });
    scene
}

/// § 1.5 item 3: a large plane at y = 0 under a sphere light of `radius` centred `height`
/// above the origin, seen from straight above.
#[must_use]
pub fn sphere_light_plane(
    material: PtMaterial,
    radius: f32,
    height: f32,
    radiance: f32,
    size: u32,
) -> PtScene {
    let mut scene = PtScene::new(PtCamera {
        eye: [0.0, height * 0.5, 0.001],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.5,
        width: size,
        height: size,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    });
    let mesh = scene.add_mesh(quad([
        [-50.0, 0.0, -50.0],
        [-50.0, 0.0, 50.0],
        [50.0, 0.0, 50.0],
        [50.0, 0.0, -50.0],
    ]));
    let mat = scene.add_material(material);
    scene.add_instance(mesh, mat, Mat4::IDENTITY);
    scene.lights.push(PtLight::Sphere {
        position: [0.0, height, 0.0],
        radius,
        radiance: [radiance; 3],
    });
    scene
}

/// § 1.5 item 2: the inside of a closed diffuse sphere of `albedo` whose cap above
/// `cap_cos` (cos of the polar angle) emits `emission`. Returns the scene and the emitting
/// fraction of the triangulated area. For a sphere every point sees every area element with
/// the same form factor, so non-emitting surfaces converge to
/// `albedo · fraction · emission / (1 − albedo)`.
#[must_use]
pub fn closed_sphere(albedo: f32, emission: f32, cap_cos: f32, size: u32) -> (PtScene, f64) {
    let mut scene = PtScene::new(PtCamera {
        eye: [0.0, 0.0, 0.0],
        target: [0.0, -1.0, 0.3],
        fov_y: 1.0,
        width: size,
        height: size,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    });
    let full = uv_sphere(1.0, 128, 64, true);
    let mut cap = PtMesh::default();
    let mut rest = PtMesh::default();
    let (mut cap_area, mut total) = (0.0f64, 0.0f64);
    for t in full.indices.as_chunks::<3>().0.iter() {
        let p: Vec<Vec3> = t
            .iter()
            .map(|&i| Vec3::from_array(full.positions[i as usize]))
            .collect();
        let area = f64::from(0.5 * (p[1] - p[0]).cross(p[2] - p[0]).length());
        total += area;
        let centre_y = (p[0].y + p[1].y + p[2].y) / 3.0;
        let target = if centre_y > cap_cos {
            cap_area += area;
            &mut cap
        } else {
            &mut rest
        };
        let base = target.positions.len() as u32;
        for &i in t {
            target.positions.push(full.positions[i as usize]);
            target.normals.push(full.normals[i as usize]);
        }
        target.indices.extend_from_slice(&[base, base + 1, base + 2]);
    }
    let wall = scene.add_material(PtMaterial::lambert([albedo; 3]));
    let light = scene.add_material(PtMaterial {
        emissive: [emission; 3],
        ..PtMaterial::lambert([albedo; 3])
    });
    let cap_mesh = scene.add_mesh(cap);
    let rest_mesh = scene.add_mesh(rest);
    scene.add_instance(cap_mesh, light, Mat4::IDENTITY);
    scene.add_instance(rest_mesh, wall, Mat4::IDENTITY);
    (scene, cap_area / total)
}

/// Cornell box reflectances (RGB fits of the published spectra).
pub const CORNELL_WHITE: [f32; 3] = [0.725, 0.71, 0.68];
/// Red (left) wall.
pub const CORNELL_RED: [f32; 3] = [0.63, 0.065, 0.05];
/// Green (right) wall.
pub const CORNELL_GREEN: [f32; 3] = [0.14, 0.45, 0.091];
/// Light radiance (relative units).
pub const CORNELL_LIGHT: [f32; 3] = [17.0, 12.0, 4.0];

const MM: f32 = 0.001;

fn mm(p: [f32; 3]) -> [f32; 3] {
    [p[0] * MM, p[1] * MM, p[2] * MM]
}

/// The Cornell box camera (35 mm lens, 25 mm film → 39.3° field of view).
#[must_use]
pub fn cornell_camera(width: u32, height: u32) -> PtCamera {
    PtCamera {
        eye: mm([278.0, 273.0, -800.0]),
        target: mm([278.0, 273.0, 0.0]),
        fov_y: 2.0 * (12.5f32 / 35.0).atan(),
        width,
        height,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    }
}

/// One planar quad of a Cornell-style room: corners, reflectance, emission.
#[derive(Clone, Copy, Debug)]
pub struct RoomQuad {
    /// Corners (counter-clockwise seen from inside the room).
    pub corners: [[f32; 3]; 4],
    /// Diffuse reflectance.
    pub albedo: [f32; 3],
    /// Emitted radiance.
    pub emission: [f32; 3],
}

/// The axis-aligned Cornell room (556 × 548.8 × 559.2 mm, open at z = 0) with the standard
/// light flush in the ceiling, as quads facing inward. The ceiling is cut around the light so
/// the light is part of the surface (no recess), which the radiosity reference needs.
#[must_use]
pub fn cornell_room_quads() -> Vec<RoomQuad> {
    let (w, h, d) = (556.0, 548.8, 559.2);
    let white = CORNELL_WHITE;
    let none = [0.0; 3];
    let q = |c: [[f32; 3]; 4], albedo: [f32; 3], emission: [f32; 3]| RoomQuad {
        corners: [mm(c[0]), mm(c[1]), mm(c[2]), mm(c[3])],
        albedo,
        emission,
    };
    let mut quads = vec![
        // Floor (normal +Y).
        q(
            [[0.0, 0.0, 0.0], [0.0, 0.0, d], [w, 0.0, d], [w, 0.0, 0.0]],
            white,
            none,
        ),
        // Back wall (normal −Z).
        q([[0.0, 0.0, d], [0.0, h, d], [w, h, d], [w, 0.0, d]], white, none),
        // Red wall at x = w (normal −X).
        q(
            [[w, 0.0, 0.0], [w, 0.0, d], [w, h, d], [w, h, 0.0]],
            CORNELL_RED,
            none,
        ),
        // Green wall at x = 0 (normal +X).
        q(
            [[0.0, 0.0, 0.0], [0.0, h, 0.0], [0.0, h, d], [0.0, 0.0, d]],
            CORNELL_GREEN,
            none,
        ),
    ];
    // Ceiling (normal −Y), cut into a 3×3 grid around the light (x 213..343, z 227..332).
    let xs = [0.0, 213.0, 343.0, w];
    let zs = [0.0, 227.0, 332.0, d];
    for i in 0..3 {
        for j in 0..3 {
            let (x0, x1, z0, z1) = (xs[i], xs[i + 1], zs[j], zs[j + 1]);
            let light = i == 1 && j == 1;
            quads.push(q(
                [[x0, h, z0], [x1, h, z0], [x1, h, z1], [x0, h, z1]],
                if light { none } else { white },
                if light { CORNELL_LIGHT } else { none },
            ));
        }
    }
    quads
}

/// Adds room quads to `scene` as meshes (one per distinct material).
pub fn add_room(scene: &mut PtScene, quads: &[RoomQuad]) {
    for rq in quads {
        let material = if rq.emission.iter().any(|&e| e > 0.0) {
            PtMaterial::emitter(rq.emission)
        } else {
            PtMaterial::lambert(rq.albedo)
        };
        let mat = scene.add_material(material);
        let mesh = scene.add_mesh(quad(rq.corners));
        scene.add_instance(mesh, mat, Mat4::IDENTITY);
    }
}

/// § 1.5 item 5 scene: the Cornell room with the short and tall blocks from the public data.
#[must_use]
pub fn cornell_box(width: u32, height: u32, blocks: bool) -> PtScene {
    let mut scene = PtScene::new(cornell_camera(width, height));
    add_room(&mut scene, &cornell_room_quads());
    if blocks {
        let white = scene.add_material(PtMaterial::lambert(CORNELL_WHITE));
        let short = prism(
            [
                mm([130.0, 165.0, 65.0]),
                mm([82.0, 165.0, 225.0]),
                mm([240.0, 165.0, 272.0]),
                mm([290.0, 165.0, 114.0]),
            ],
            0.0,
        );
        let tall = prism(
            [
                mm([423.0, 330.0, 247.0]),
                mm([265.0, 330.0, 296.0]),
                mm([314.0, 330.0, 456.0]),
                mm([472.0, 330.0, 406.0]),
            ],
            0.0,
        );
        let s = scene.add_mesh(short);
        let t = scene.add_mesh(tall);
        scene.add_instance(s, white, Mat4::IDENTITY);
        scene.add_instance(t, white, Mat4::IDENTITY);
    }
    scene
}

/// An empty scene (nothing but the sky) for the atmosphere check, § 1.5 item 7.
#[must_use]
pub fn empty_sky(sky: PtSky, camera: PtCamera) -> PtScene {
    let mut scene = PtScene::new(camera);
    scene.sky = sky;
    scene
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sphere_normals_face_the_right_way() {
        for inward in [false, true] {
            let m = uv_sphere(2.0, 16, 8, inward);
            for t in m.indices.as_chunks::<3>().0.iter() {
                let p: Vec<Vec3> = t
                    .iter()
                    .map(|&i| Vec3::from_array(m.positions[i as usize]))
                    .collect();
                let face = (p[1] - p[0]).cross(p[2] - p[0]);
                if face.length() < 1e-6 {
                    continue;
                }
                let centre = (p[0] + p[1] + p[2]) / 3.0;
                assert_eq!(face.dot(centre) > 0.0, !inward);
            }
        }
    }

    #[test]
    fn prism_normals_point_out() {
        let top = [[0.0, 1.0, 0.0], [0.0, 1.0, 1.0], [1.0, 1.0, 1.0], [1.0, 1.0, 0.0]];
        let m = prism(top, 0.0);
        assert_eq!(m.triangle_count(), 10);
        let c = Vec3::new(0.5, 0.5, 0.5);
        for t in m.indices.as_chunks::<3>().0.iter() {
            let p: Vec<Vec3> = t
                .iter()
                .map(|&i| Vec3::from_array(m.positions[i as usize]))
                .collect();
            let face = (p[1] - p[0]).cross(p[2] - p[0]);
            assert!(face.dot((p[0] + p[1] + p[2]) / 3.0 - c) > 0.0);
        }
    }

    #[test]
    fn room_quads_face_inward() {
        let centre = Vec3::new(0.278, 0.274, 0.28);
        for q in cornell_room_quads() {
            let a = Vec3::from_array(q.corners[0]);
            let n = (Vec3::from_array(q.corners[1]) - a).cross(Vec3::from_array(q.corners[2]) - a);
            assert!(n.dot(centre - a) > 0.0, "{q:?}");
        }
    }

    #[test]
    fn closed_sphere_cap_fraction() {
        let (_, f) = closed_sphere(0.5, 1.0, 0.5, 8);
        // A cap above cos θ = 0.5 covers (1 − 0.5) / 2 = 25 % of a sphere.
        assert!((f - 0.25).abs() < 0.01, "{f}");
    }
}
