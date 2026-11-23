// SPDX-License-Identifier: MPL-2.0
//! Single-mesh lightmap bakes (lightmaps phase 2, ADR-1101): one instance of a scene baked into
//! its own atlas with the UV2 it already has (an authored second UV set, or a first UV set that
//! does not overlap), and the quad-and-box reference scene the baker's tests and golden use.
//!
//! The whole scene still lights and shadows the instance; only the instance receives texels.
//! The UV2 square maps inside the gutters (`padding` texels on every side), so dilation has room
//! to fill them and bilinear filtering at a chart edge never reads an unbaked texel.

use regia_asset::lightmap::{Lightmap, LightmapEntry, LightmapLayout, MAX_LIGHTMAP_SIZE, MeshUnwrap};
use regia_core::glam::{Mat4, Vec3};

use super::{LightmapBakeReport, LightmapBakeSettings, LightmapScene, bake_lightmap};
use crate::{PtCamera, PtError, PtMaterial, PtMesh, PtScene, PtSky, PtSun};

/// One instance to bake on its own ([`bake_mesh_lightmap`]).
#[derive(Clone, Copy, Debug)]
pub struct MeshLightmapRequest<'a> {
    /// The receiving instance, an index into [`LightmapScene::instances`].
    pub instance: usize,
    /// The entry's name in the lightmap.
    pub name: &'a str,
    /// One UV2 per vertex of the instance's mesh, `0..1`. Triangles should not overlap in UV2;
    /// where they do, the first triangle keeps the texel.
    pub uv: &'a [[f32; 2]],
    /// Atlas size, texels, gutters included.
    pub size: [u32; 2],
    /// Gutter texels on every side.
    pub padding: u32,
}

/// World-space surface area of a triangle list, m² (indices past the vertices count as zero).
fn surface_area(positions: &[[f32; 3]], indices: &[u32]) -> f32 {
    let at = |i: u32| Vec3::from_array(positions.get(i as usize).copied().unwrap_or([0.0; 3]));
    indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| 0.5 * (at(t[1]) - at(t[0])).cross(at(t[2]) - at(t[0])).length())
        .sum()
}

/// The one-entry atlas of `request`: the rectangle covers the whole atlas, the unwrap copies
/// every vertex of the instance (`xref` is the identity) with its UV2 inset by the gutters, and
/// the density is the interior texels over the instance's surface area. An authored unwrap counts
/// as one chart.
///
/// # Errors
///
/// [`PtError::Scene`] when the instance is missing, emissive, unlit or has no triangles, the UV2
/// count differs from its vertex count, a UV2 is outside `0..1`, or the size is outside
/// `1..=MAX_LIGHTMAP_SIZE` or leaves no interior inside the gutters.
pub fn mesh_layout(
    scene: &LightmapScene,
    request: &MeshLightmapRequest<'_>,
) -> Result<LightmapLayout, PtError> {
    let bad = |m: String| Err(PtError::Scene(format!("lightmap `{}`: {m}", request.name)));
    let Some(inst) = scene.instances().get(request.instance) else {
        return bad(format!("no instance {}", request.instance));
    };
    if !inst.receiver {
        return bad(format!(
            "instance {} is emissive, unlit or a light: it cannot hold a lightmap",
            request.instance
        ));
    }
    if inst.indices.is_empty() {
        return bad(format!("instance {} has no triangles", request.instance));
    }
    if request.uv.len() != inst.positions.len() {
        return bad(format!(
            "{} UV2s for the {} vertices of instance {}",
            request.uv.len(),
            inst.positions.len(),
            request.instance
        ));
    }
    if request
        .uv
        .iter()
        .flatten()
        .any(|v| !v.is_finite() || *v < 0.0 || *v > 1.0)
    {
        return bad("a UV2 is outside 0..1".into());
    }
    let [w, h] = request.size;
    if w == 0 || h == 0 || w > MAX_LIGHTMAP_SIZE || h > MAX_LIGHTMAP_SIZE {
        return bad(format!("atlas {w}×{h} is outside 1..={MAX_LIGHTMAP_SIZE}"));
    }
    let p = request.padding;
    let interior = |side: u32| side.checked_sub(p.saturating_mul(2)).filter(|&v| v > 0);
    let (Some(iw), Some(ih)) = (interior(w), interior(h)) else {
        return bad(format!("a {p}-texel gutter leaves no interior in {w}×{h}"));
    };
    let inset = |v: f32, side: u32, inner: u32| (p as f32 + v * inner as f32) / side as f32;
    let uv = request
        .uv
        .iter()
        .map(|q| [inset(q[0], w, iw), inset(q[1], h, ih)])
        .collect();
    let vertices = inst.positions.len() as u32;
    let area = surface_area(&inst.positions, &inst.indices);
    let texels_per_metre = if area > 0.0 {
        (iw as f32 * ih as f32 / area).sqrt()
    } else {
        0.0
    };
    Ok(LightmapLayout {
        width: w,
        height: h,
        density: texels_per_metre,
        padding: p,
        entries: vec![LightmapEntry {
            name: request.name.to_string(),
            source_vertices: vertices,
            rect: [0, 0, w, h],
            scale_offset: [1.0, 1.0, 0.0, 0.0],
            vertex_count: 0,
            index_count: 0,
            first_vertex: 0,
            first_index: 0,
            charts: 1,
            area_m2: area,
            texels_per_metre,
            key: None,
            seams: None,
            page: 0,
        }],
        unwraps: vec![MeshUnwrap {
            xref: (0..vertices).collect(),
            uv,
            indices: inst.indices.clone(),
        }],
    })
}

/// Bakes one instance of `scene` into its own lightmap ([`mesh_layout`] then
/// [`bake_lightmap`]): direct light by next-event estimation, indirect light by the hemisphere
/// paths, inside-geometry texels dropped, then the filter and the gutter dilation of `settings`.
/// The result is the same for any thread count.
///
/// # Errors
///
/// The [`mesh_layout`] errors, and the [`bake_lightmap`] ones.
pub fn bake_mesh_lightmap(
    scene: &LightmapScene,
    request: &MeshLightmapRequest<'_>,
    settings: &LightmapBakeSettings,
    progress: impl Fn(usize, usize) + Sync,
) -> Result<(Lightmap, LightmapBakeReport), PtError> {
    let _span = tracing::info_span!("lightmap_bake_mesh", instance = request.instance).entered();
    let layout = mesh_layout(scene, request)?;
    bake_lightmap(scene, layout, &[request.instance], settings, progress)
}

/// The quad-and-box reference scene ([`quad_box_scene`]).
#[derive(Clone, Debug)]
pub struct QuadBoxScene {
    /// The scene: the floor is instance [`Self::floor`], the box instance [`Self::cube`].
    pub scene: PtScene,
    /// The floor quad's instance.
    pub floor: usize,
    /// The box's instance.
    pub cube: usize,
    /// The floor's UV2, one per vertex: `u = (x + 2) / 4`, `v = (z + 2) / 4`.
    pub floor_uv: Vec<[f32; 2]>,
}

impl QuadBoxScene {
    /// Half the floor's side, metres (the floor spans `-2..2` in x and z at y = 0).
    pub const FLOOR_HALF: f32 = 2.0;
    /// The floor's grey reflectance.
    pub const FLOOR_ALBEDO: f32 = 0.6;
    /// The box's lower corner, metres (it stands on the floor).
    pub const BOX_MIN: [f32; 3] = [0.0, 0.0, -0.5];
    /// The box's upper corner, metres.
    pub const BOX_MAX: [f32; 3] = [1.0, 1.0, 0.5];
    /// The box's red reflectance.
    pub const BOX_ALBEDO: [f32; 3] = [0.8, 0.1, 0.1];
    /// The uniform sky's radiance (white).
    pub const SKY: f32 = 1.0;
    /// Unit vector toward the sun: 53.13° above the horizon on the −x side, so the box's shadow
    /// falls on +x, from x = 1 to x = 1.75.
    pub const SUN_TOWARD: [f32; 3] = [-0.6, 0.8, 0.0];
    /// The sun's illuminance at normal incidence, lux (white, a delta direction).
    pub const SUN_LUX: f32 = 10.0;

    /// The floor's world (x, z) at its UV2.
    #[must_use]
    pub fn floor_point(uv: [f32; 2]) -> [f32; 2] {
        let side = 2.0 * Self::FLOOR_HALF;
        [uv[0] * side - Self::FLOOR_HALF, uv[1] * side - Self::FLOOR_HALF]
    }
}

/// A closed box from `min` to `max`, four vertices per face with the face's normal, every face
/// counter-clockwise seen from outside (front faces out).
fn cuboid(min: Vec3, max: Vec3) -> PtMesh {
    let centre = (min + max) * 0.5;
    let half = (max - min) * 0.5;
    // (normal, u, v) with u × v = normal.
    let faces = [
        (Vec3::X, Vec3::Y, Vec3::Z),
        (-Vec3::X, Vec3::Z, Vec3::Y),
        (Vec3::Y, Vec3::Z, Vec3::X),
        (-Vec3::Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (-Vec3::Z, Vec3::Y, Vec3::X),
    ];
    let mut mesh = PtMesh::default();
    for (n, u, v) in faces {
        let (u, v, c) = (u * half, v * half, centre + n * half);
        let base = mesh.positions.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            mesh.positions.push((c + u * su + v * sv).to_array());
            mesh.normals.push(n.to_array());
        }
        mesh.indices
            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    mesh
}

/// The quad-and-box reference scene: a 4 m grey floor quad facing up, a closed 1 m red box
/// standing on it off centre, a white uniform sky and a white delta sun low enough for the box to
/// cast a 0.75 m shadow ([`QuadBoxScene`] has the numbers). Ideal Lambertian materials, so the
/// open floor has a closed-form irradiance and the box's red shows up in the floor's indirect
/// light only.
#[must_use]
pub fn quad_box_scene() -> QuadBoxScene {
    let h = QuadBoxScene::FLOOR_HALF;
    let mut scene = PtScene::new(PtCamera::pinhole([3.0, 4.0, 5.0], [0.5, 0.0, 0.0], 0.9, 64, 64));
    let floor_mesh = scene.add_mesh(PtMesh {
        positions: vec![[-h, 0.0, -h], [h, 0.0, -h], [h, 0.0, h], [-h, 0.0, h]],
        normals: vec![[0.0, 1.0, 0.0]; 4],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        // Counter-clockwise seen from above: the winding normal is +Y.
        indices: vec![0, 2, 1, 0, 3, 2],
        ..PtMesh::default()
    });
    let box_mesh = scene.add_mesh(cuboid(
        Vec3::from_array(QuadBoxScene::BOX_MIN),
        Vec3::from_array(QuadBoxScene::BOX_MAX),
    ));
    let grey = scene.add_material(PtMaterial::lambert([QuadBoxScene::FLOOR_ALBEDO; 3]));
    let red = scene.add_material(PtMaterial::lambert(QuadBoxScene::BOX_ALBEDO));
    scene.add_instance(floor_mesh, grey, Mat4::IDENTITY);
    scene.add_instance(box_mesh, red, Mat4::IDENTITY);
    scene.sky = PtSky::Uniform([QuadBoxScene::SKY; 3]);
    scene.sun = Some(PtSun {
        toward: QuadBoxScene::SUN_TOWARD,
        color: [1.0; 3],
        illuminance: QuadBoxScene::SUN_LUX,
        angular_radius: 0.0,
    });
    QuadBoxScene {
        scene,
        floor: 0,
        cube: 1,
        floor_uv: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(qb: &QuadBoxScene, size: u32, padding: u32) -> MeshLightmapRequest<'_> {
        MeshLightmapRequest {
            instance: qb.floor,
            name: "floor",
            uv: &qb.floor_uv,
            size: [size, size],
            padding,
        }
    }

    #[test]
    fn the_box_is_closed_and_faces_out() {
        let mesh = cuboid(Vec3::ZERO, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(mesh.positions.len(), 24);
        assert_eq!(mesh.indices.len(), 36);
        let centre = Vec3::new(0.5, 1.0, 1.5);
        let mut area = 0.0;
        for (k, t) in mesh.indices.as_chunks::<3>().0.iter().enumerate() {
            let p = t.map(|i| Vec3::from_array(mesh.positions[i as usize]));
            let cross = (p[1] - p[0]).cross(p[2] - p[0]);
            let n = Vec3::from_array(mesh.normals[t[0] as usize]);
            assert!(
                cross.normalize().dot(n) > 0.999,
                "triangle {k} winds against its normal"
            );
            let mid = (p[0] + p[1] + p[2]) / 3.0;
            assert!((mid - centre).dot(n) > 0.0, "triangle {k} faces in");
            area += 0.5 * cross.length();
        }
        // 2·(1·2 + 2·3 + 1·3) = 22 m².
        assert!((area - 22.0).abs() < 1e-4, "{area}");
    }

    #[test]
    fn the_layout_insets_the_uvs_by_the_gutter() {
        let qb = quad_box_scene();
        let scene = LightmapScene::from_pt_scene(&qb.scene).expect("scene");
        let layout = mesh_layout(&scene, &request(&qb, 36, 2)).expect("layout");
        assert_eq!((layout.width, layout.height, layout.padding), (36, 36, 2));
        let entry = &layout.entries[0];
        assert_eq!(entry.rect, [0, 0, 36, 36]);
        assert_eq!(entry.source_vertices, 4);
        assert!((entry.area_m2 - 16.0).abs() < 1e-4);
        // 32 interior texels over 4 m.
        assert!((entry.texels_per_metre - 8.0).abs() < 1e-4);
        let unwrap = &layout.unwraps[0];
        assert_eq!(unwrap.xref, [0, 1, 2, 3]);
        assert_eq!(unwrap.indices, qb.scene.meshes[0].indices);
        let (lo, hi) = (2.0 / 36.0, 34.0 / 36.0);
        for (got, want) in unwrap.uv.iter().zip([[lo, lo], [hi, lo], [hi, hi], [lo, hi]]) {
            assert!((got[0] - want[0]).abs() < 1e-6 && (got[1] - want[1]).abs() < 1e-6);
        }
        assert_eq!(QuadBoxScene::floor_point([0.5, 0.25]), [0.0, -1.0]);
    }

    #[test]
    fn bad_requests_are_errors_not_panics() {
        let qb = quad_box_scene();
        let scene = LightmapScene::from_pt_scene(&qb.scene).expect("scene");
        let ok = request(&qb, 16, 2);
        assert!(mesh_layout(&scene, &ok).is_ok());
        let cases: [(&str, MeshLightmapRequest<'_>); 6] = [
            ("missing instance", MeshLightmapRequest { instance: 9, ..ok }),
            (
                "short uv",
                MeshLightmapRequest {
                    uv: &qb.floor_uv[..3],
                    ..ok
                },
            ),
            ("zero size", MeshLightmapRequest { size: [0, 16], ..ok }),
            (
                "huge size",
                MeshLightmapRequest {
                    size: [16, MAX_LIGHTMAP_SIZE + 1],
                    ..ok
                },
            ),
            ("no interior", MeshLightmapRequest { padding: 8, ..ok }),
            (
                "huge padding",
                MeshLightmapRequest {
                    padding: u32::MAX,
                    ..ok
                },
            ),
        ];
        for (what, r) in cases {
            assert!(mesh_layout(&scene, &r).is_err(), "{what}");
        }
        let outside = [[0.0, 0.0], [1.5, 0.0], [1.0, 1.0], [0.0, 1.0]];
        assert!(mesh_layout(&scene, &MeshLightmapRequest { uv: &outside, ..ok }).is_err());
        let nan = [[0.0, 0.0], [f32::NAN, 0.0], [1.0, 1.0], [0.0, 1.0]];
        assert!(mesh_layout(&scene, &MeshLightmapRequest { uv: &nan, ..ok }).is_err());

        // An emitter cannot receive.
        let mut lit = qb.scene.clone();
        let glow = lit.add_material(PtMaterial::emitter([5.0; 3]));
        lit.instances[qb.cube].material = glow;
        let lit = LightmapScene::from_pt_scene(&lit).expect("scene");
        let uv = [[0.0, 0.0]; 24];
        let cube = MeshLightmapRequest {
            instance: qb.cube,
            uv: &uv,
            ..ok
        };
        let err = mesh_layout(&lit, &cube).expect_err("emitter");
        assert!(err.to_string().contains("cannot hold a lightmap"), "{err}");
    }
}
