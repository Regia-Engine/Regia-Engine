// SPDX-License-Identifier: MPL-2.0
//! Render attributes of fracture chunks and `.fracture.ron` v2 (ADR-1967): normals, UVs and
//! tangents on surface pieces and cut faces, seams, determinism, and the v1 migration.

use regia_asset_pipeline::fracture::{
    FRACTURE_FORMAT_VERSION, FractureChunk, FractureError, FractureSet, FractureSettings, FractureSites,
    FractureSource, RenderSettings, box_mesh, fracture_mesh, fracture_migration_sources, fracture_source,
    migrate_fracture,
};
use serde::Serialize;

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn len(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

/// A UV sphere of radius `r` with welded poles and seam, closed; analytic normals are the
/// positions over `r`.
fn sphere(r: f32) -> (Vec<[f32; 3]>, Vec<u32>) {
    let (rings, segments) = (10u32, 16u32);
    let mut positions = vec![[0.0, r, 0.0], [0.0, -r, 0.0]];
    for ring in 1..rings {
        let phi = std::f32::consts::PI * ring as f32 / rings as f32;
        for s in 0..segments {
            let theta = std::f32::consts::TAU * s as f32 / segments as f32;
            positions.push([
                r * phi.sin() * theta.cos(),
                r * phi.cos(),
                r * phi.sin() * theta.sin(),
            ]);
        }
    }
    let id = |ring: u32, s: u32| 2 + (ring - 1) * segments + s % segments;
    let mut indices = Vec::new();
    for s in 0..segments {
        indices.extend_from_slice(&[0, id(1, s + 1), id(1, s)]);
        indices.extend_from_slice(&[1, id(rings - 1, s), id(rings - 1, s + 1)]);
        for ring in 1..rings - 1 {
            let (a, b, c, d) = (id(ring, s), id(ring, s + 1), id(ring + 1, s + 1), id(ring + 1, s));
            indices.extend_from_slice(&[a, b, c, a, c, d]);
        }
    }
    (positions, indices)
}

/// What every chunk must satisfy, whatever the source was.
fn assert_render_sound(c: &FractureChunk) {
    let r = &c.render;
    assert_eq!(r.indices.len(), c.indices.len());
    assert_eq!(r.interior_start, c.interior_start);
    let n = r.positions.len();
    assert_eq!((r.normals.len(), r.tangents.len(), r.uvs.len()), (n, n, n));
    // Same triangles in the same order.
    for (t, w) in r.indices.iter().zip(&c.indices) {
        assert_eq!(r.positions[*t as usize], c.positions[*w as usize]);
    }
    for (a, b) in r.normals.iter().zip(&r.tangents) {
        assert!((len(*a) - 1.0).abs() < 1e-3, "unit normal {a:?}");
        let t = [b[0], b[1], b[2]];
        assert!((len(t) - 1.0).abs() < 1e-3, "unit tangent {b:?}");
        assert!(dot(*a, t).abs() < 1e-3, "tangent in the surface {a:?} {b:?}");
        assert!(b[3] == 1.0 || b[3] == -1.0, "handedness {b:?}");
    }
    assert!(r.uvs.iter().flatten().all(|u| u.is_finite()));
}

#[test]
fn a_box_keeps_hard_edges_and_its_cut_faces_face_outward() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 0.5, 0.75])?;
    let set = fracture_mesh(&p, &i, &FractureSettings::random(8, 11))?;
    for c in &set.chunks {
        assert_render_sound(c);
        let (tris, _) = c.render.indices.as_chunks::<3>();
        for (t, tri) in tris.iter().enumerate() {
            let [a, b, c2] = tri.map(|v| c.render.positions[v as usize]);
            let face = cross(sub(b, a), sub(c2, a));
            if len(face) < 1e-7 {
                continue; // a sliver has no direction to compare with
            }
            for v in tri {
                let n = c.render.normals[*v as usize];
                assert!(
                    dot(n, face) / len(face) > 0.999,
                    "triangle {t} ({}) is shaded flat and outward",
                    if t < c.interior_start as usize {
                        "surface"
                    } else {
                        "cut"
                    }
                );
            }
            if t < c.interior_start as usize {
                // Box faces: the normal is an axis.
                let n = c.render.normals[tri[0] as usize];
                assert!(n.iter().filter(|x| x.abs() > 0.999).count() == 1, "{n:?}");
            }
        }
    }
    Ok(())
}

#[test]
fn surface_pieces_read_the_source_normals() -> Result<(), FractureError> {
    let (p, i) = sphere(2.0);
    let normals: Vec<[f32; 3]> = p.iter().map(|v| v.map(|c| c / 2.0)).collect();
    let source = FractureSource {
        positions: &p,
        indices: &i,
        normals: Some(&normals),
        uvs: None,
    };
    let set = fracture_source(&source, &FractureSettings::random(6, 2))?;
    for c in &set.chunks {
        assert_render_sound(c);
        let surface = c.interior_start as usize * 3;
        for v in &c.render.indices[..surface] {
            let (pos, n) = (c.render.positions[*v as usize], c.render.normals[*v as usize]);
            // Flat facets under smooth normals: close to the radial direction, not equal to it.
            let radial = pos.map(|x| x / len(pos));
            assert!(dot(n, radial) > 0.99, "{n:?} at {pos:?}");
        }
    }
    Ok(())
}

#[test]
fn a_source_without_normals_is_smoothed_over_shallow_edges_only() -> Result<(), FractureError> {
    let (p, i) = sphere(2.0);
    let set = fracture_mesh(&p, &i, &FractureSettings::random(4, 9))?;
    let mut smooth = 0usize;
    for c in &set.chunks {
        assert_render_sound(c);
        let surface = c.interior_start as usize;
        let (tris, _) = c.render.indices.as_chunks::<3>();
        for tri in &tris[..surface] {
            let [a, b, d] = tri.map(|v| c.render.positions[v as usize]);
            let face = cross(sub(b, a), sub(d, a));
            if len(face) > 1e-6 && dot(c.render.normals[tri[0] as usize], face) / len(face) < 0.9999 {
                smooth += 1;
            }
        }
    }
    assert!(smooth > 50, "a sphere's facets blend: {smooth}");
    Ok(())
}

/// A box with 24 vertices: each face its own, so positions repeat with other attributes.
fn seamed_box() -> (Vec<[f32; 3]>, Vec<[f32; 2]>, Vec<u32>) {
    let (positions, indices) = box_mesh([1.0, 1.0, 1.0]).unwrap_or_default();
    let (mut p, mut uv, mut out) = (Vec::new(), Vec::new(), Vec::new());
    for (face, tri) in indices.as_chunks::<3>().0.iter().enumerate() {
        for v in tri {
            out.push(p.len() as u32);
            p.push(positions[*v as usize]);
            // A UV island per box side, far apart: U carries which side.
            uv.push([(face / 2) as f32 * 10.0, face as f32 * 0.0]);
        }
    }
    (p, uv, out)
}

#[test]
fn a_uv_seam_stays_a_seam_and_vertices_split_there() -> Result<(), FractureError> {
    let (p, uv, i) = seamed_box();
    let source = FractureSource {
        positions: &p,
        indices: &i,
        normals: None,
        uvs: Some(&uv),
    };
    let set = fracture_source(&source, &FractureSettings::random(5, 3))?;
    let mut islands = std::collections::BTreeSet::new();
    for c in &set.chunks {
        assert_render_sound(c);
        assert!(
            c.render.positions.len() >= c.positions.len(),
            "split, never merged"
        );
        let (tris, _) = c.render.indices.as_chunks::<3>();
        for tri in &tris[..c.interior_start as usize] {
            let us = tri.map(|v| c.render.uvs[v as usize][0]);
            assert_eq!(us[0], us[1], "a surface triangle stays on its island");
            assert_eq!(us[1], us[2]);
            assert!((us[0] / 10.0).fract() == 0.0, "islands are not blended: {us:?}");
            islands.insert(us[0] as i32);
        }
    }
    assert_eq!(islands.len(), 6, "every box side reached a chunk: {islands:?}");
    Ok(())
}

#[test]
fn surface_uvs_interpolate_exactly_across_cuts() -> Result<(), FractureError> {
    // A UV that is linear in position: every cut vertex must land on the same function.
    let (p, i) = sphere(1.5);
    let f = |v: [f32; 3]| [0.25 * v[0] + 0.5, 0.5 * v[1] - 0.125 * v[2]];
    let uvs: Vec<[f32; 2]> = p.iter().map(|v| f(*v)).collect();
    let source = FractureSource {
        positions: &p,
        indices: &i,
        normals: None,
        uvs: Some(&uvs),
    };
    let set = fracture_source(&source, &FractureSettings::random(7, 4))?;
    for c in &set.chunks {
        for v in &c.render.indices[..c.interior_start as usize * 3] {
            let (pos, uv) = (c.render.positions[*v as usize], c.render.uvs[*v as usize]);
            let want = f(pos);
            assert!(
                (uv[0] - want[0]).abs() < 1e-5 && (uv[1] - want[1]).abs() < 1e-5,
                "{uv:?} against {want:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn the_two_sides_of_a_cut_share_their_pattern_and_face_apart() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut s = FractureSettings::random(2, 0);
    s.sites = FractureSites::Explicit(vec![[-0.5, 0.1, 0.0], [0.5, -0.1, 0.2]]);
    let set = fracture_mesh(&p, &i, &s)?;
    assert_eq!(set.chunks.len(), 2);
    let side = |c: &FractureChunk| {
        let (tris, _) = c.render.indices.as_chunks::<3>();
        let cap: Vec<u32> = tris[c.interior_start as usize..]
            .iter()
            .flatten()
            .copied()
            .collect();
        cap
    };
    let (a, b) = (&set.chunks[0], &set.chunks[1]);
    let mut shared = 0;
    for va in side(a) {
        for vb in side(b) {
            let (pa, pb) = (a.render.positions[va as usize], b.render.positions[vb as usize]);
            if len(sub(pa, pb)) < 1e-6 {
                shared += 1;
                let (ua, ub) = (a.render.uvs[va as usize], b.render.uvs[vb as usize]);
                assert!((ua[0] - ub[0]).abs() < 1e-5 && (ua[1] - ub[1]).abs() < 1e-5);
                let (na, nb) = (a.render.normals[va as usize], b.render.normals[vb as usize]);
                assert!(dot(na, nb) < -0.9999, "{na:?} {nb:?}");
            }
        }
    }
    assert!(shared > 0, "the two caps meet");
    Ok(())
}

#[test]
fn the_cut_face_uv_scale_is_uv_units_per_metre() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut s = FractureSettings::random(4, 8);
    s.render.interior_uv_scale = 3.0;
    let set = fracture_mesh(&p, &i, &s)?;
    let mut edges = 0;
    for c in &set.chunks {
        let (tris, _) = c.render.indices.as_chunks::<3>();
        for tri in &tris[c.interior_start as usize..] {
            for k in 0..3 {
                let (a, b) = (tri[k] as usize, tri[(k + 1) % 3] as usize);
                let world = len(sub(c.render.positions[a], c.render.positions[b]));
                let (ua, ub) = (c.render.uvs[a], c.render.uvs[b]);
                let uv = ((ua[0] - ub[0]).powi(2) + (ua[1] - ub[1]).powi(2)).sqrt();
                if world > 1e-3 {
                    assert!((uv / world - 3.0).abs() < 1e-3, "{uv} over {world}");
                    edges += 1;
                }
            }
        }
    }
    assert!(edges > 10);
    Ok(())
}

#[test]
fn bad_attributes_and_settings_are_refused() {
    let (p, i) = box_mesh([1.0, 1.0, 1.0]).unwrap_or_default();
    let s = FractureSettings::random(2, 0);
    let run = |normals: Option<&[[f32; 3]]>, uvs: Option<&[[f32; 2]]>| {
        fracture_source(
            &FractureSource {
                positions: &p,
                indices: &i,
                normals,
                uvs,
            },
            &s,
        )
        .map(|_| ())
        .map_err(|e| e.code())
    };
    let good_n = vec![[0.0, 1.0, 0.0]; p.len()];
    let mut nan_n = good_n.clone();
    nan_n[3][1] = f32::NAN;
    let mut zero_n = good_n.clone();
    zero_n[3] = [0.0; 3];
    let good_uv = vec![[0.0, 0.0]; p.len()];
    let mut inf_uv = good_uv.clone();
    inf_uv[1][0] = f32::INFINITY;
    assert_eq!(run(Some(&good_n), Some(&good_uv)), Ok(()));
    for (n, uv) in [
        (Some(&good_n[1..]), None),
        (Some(nan_n.as_slice()), None),
        (Some(zero_n.as_slice()), None),
        (None, Some(&good_uv[1..])),
        (None, Some(inf_uv.as_slice())),
    ] {
        assert_eq!(run(n, uv), Err("fracture_bad_attributes"));
    }
    let mut bad = FractureSettings::random(2, 0);
    bad.render = RenderSettings {
        interior_uv_scale: -1.0,
        ..RenderSettings::default()
    };
    assert_eq!(
        fracture_mesh(&p, &i, &bad).map(|_| ()).map_err(|e| e.code()),
        Err("fracture_invalid_settings")
    );
}

/// The v1 shape, for writing files as v1 wrote them.
#[derive(Serialize)]
struct V1Set {
    format_version: u32,
    source_volume: f32,
    chunks: Vec<V1Chunk>,
    joints: Vec<regia_asset_pipeline::fracture::FractureJoint>,
}

#[derive(Serialize)]
struct V1Chunk {
    site: [f32; 3],
    positions: Vec<[f32; 3]>,
    indices: Vec<u32>,
    interior_start: u32,
    volume: f32,
    centroid: [f32; 3],
    bounds_min: [f32; 3],
    bounds_max: [f32; 3],
    interior_area: f32,
}

fn as_v1(set: &FractureSet) -> String {
    let v1 = V1Set {
        format_version: 1,
        source_volume: set.source_volume,
        chunks: set
            .chunks
            .iter()
            .map(|c| V1Chunk {
                site: c.site,
                positions: c.positions.clone(),
                indices: c.indices.clone(),
                interior_start: c.interior_start,
                volume: c.volume,
                centroid: c.centroid,
                bounds_min: c.bounds_min,
                bounds_max: c.bounds_max,
                interior_area: c.interior_area,
            })
            .collect(),
        joints: set.joints.clone(),
    };
    ron::ser::to_string_pretty(&v1, regia_core::ron_pretty()).unwrap_or_default()
}

#[test]
fn a_v1_file_gains_render_meshes_and_keeps_everything_else() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 0.5, 0.25])?;
    let set = fracture_mesh(&p, &i, &FractureSettings::random(6, 5))?;
    let text = as_v1(&set);
    assert!(text.contains("format_version: 1") && !text.contains("render"));
    assert_eq!(fracture_migration_sources(), vec![1]);
    let up = FractureSet::from_ron(&text)?;
    assert_eq!(up.format_version, FRACTURE_FORMAT_VERSION);
    assert_eq!(up.joints, set.joints);
    assert_eq!(up.chunks.len(), set.chunks.len());
    for (a, b) in up.chunks.iter().zip(&set.chunks) {
        assert_render_sound(a);
        assert_eq!(a.positions, b.positions);
        assert_eq!(a.indices, b.indices);
        assert_eq!((a.interior_start, a.volume), (b.interior_start, b.volume));
    }
    // The text migrator gives a current file, and running it again changes nothing.
    let migrated = migrate_fracture(&text)?;
    assert!(migrated.contains("format_version: 2"));
    assert_eq!(FractureSet::from_ron(&migrated)?, up);
    assert_eq!(migrate_fracture(&migrated)?, migrated);
    Ok(())
}

#[test]
fn a_hostile_file_is_refused_without_a_panic() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let set = fracture_mesh(&p, &i, &FractureSettings::random(3, 1))?;
    // v1 with an index past the vertex list (a huge one must not size an allocation).
    let mut bad = set.clone();
    bad.chunks[1].indices[4] = u32::MAX;
    assert!(matches!(
        FractureSet::from_ron(&as_v1(&bad)),
        Err(FractureError::Format(_))
    ));
    // v2 whose render mesh disagrees with its chunk, in each way the loader checks.
    let edits: [fn(&mut FractureChunk); 6] = [
        |c| c.render.indices.truncate(3),
        |c| {
            c.render.normals.pop();
        },
        |c| c.render.interior_start += 1,
        |c| c.render.indices[0] = u32::MAX,
        |c| c.render.uvs[0][0] = f32::NAN,
        |c| c.render = Default::default(),
    ];
    for edit in edits {
        let mut bad = set.clone();
        edit(&mut bad.chunks[0]);
        let text = bad.to_ron()?;
        assert!(
            matches!(FractureSet::from_ron(&text), Err(FractureError::Format(_))),
            "{text:.0}"
        );
    }
    Ok(())
}

#[test]
fn the_same_inputs_give_the_same_render_bytes() -> Result<(), FractureError> {
    let (p, i) = sphere(1.0);
    let s = FractureSettings::random(9, 21);
    let (a, b) = (fracture_mesh(&p, &i, &s)?, fracture_mesh(&p, &i, &s)?);
    assert_eq!(a.to_ron()?, b.to_ron()?);
    Ok(())
}

#[test]
fn every_shape_and_seed_gets_sound_render_meshes() -> Result<(), FractureError> {
    let shapes = [box_mesh([1.0, 0.25, 2.0])?, sphere(1.0)];
    for (p, i) in &shapes {
        for seed in 0..12 {
            let set = fracture_mesh(p, i, &FractureSettings::random(3 + seed as usize * 2, seed))?;
            for c in &set.chunks {
                assert_render_sound(c);
            }
        }
    }
    Ok(())
}
