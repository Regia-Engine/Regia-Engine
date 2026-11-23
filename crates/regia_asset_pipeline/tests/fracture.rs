// SPDX-License-Identifier: MPL-2.0
//! Pre-fractured destruction tool: volume conservation, chunk counts, closed chunks, adjacency
//! symmetry, joint strengths, determinism and the error paths (ADR-1567).

use std::collections::HashMap;

use regia_asset_pipeline::fracture::{
    FRACTURE_FORMAT_VERSION, FractureChunk, FractureError, FractureSet, FractureSettings, FractureSites,
    MAX_CANDIDATES_PER_SITE, MAX_CHUNKS, box_mesh, fracture_mesh,
};

type Mesh = (Vec<[f32; 3]>, Vec<u32>);

/// A mesh from quads (counter-clockwise from outside), vertices shared bit-exactly.
fn from_quads(quads: &[[[f32; 3]; 4]]) -> Mesh {
    let mut ids: HashMap<[u32; 3], u32> = HashMap::new();
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut id = |p: [f32; 3]| {
        let k = p.map(f32::to_bits);
        *ids.entry(k).or_insert_with(|| {
            positions.push(p);
            (positions.len() - 1) as u32
        })
    };
    for q in quads {
        let [a, b, c, d] = [id(q[0]), id(q[1]), id(q[2]), id(q[3])];
        indices.extend_from_slice(&[a, b, c, a, c, d]);
    }
    (positions, indices)
}

/// An L-shaped prism (area 3, height 1): concave.
fn l_prism() -> Mesh {
    let l: [[f32; 2]; 6] = [
        [0.0, 0.0],
        [2.0, 0.0],
        [2.0, 1.0],
        [1.0, 1.0],
        [1.0, 2.0],
        [0.0, 2.0],
    ];
    let mut positions: Vec<[f32; 3]> = l.iter().map(|p| [p[0], p[1], 0.0]).collect();
    positions.extend(l.iter().map(|p| [p[0], p[1], 1.0]));
    let mut indices: Vec<u32> = Vec::new();
    for i in 1..5u32 {
        indices.extend_from_slice(&[0, i + 1, i]); // bottom, facing -z
        indices.extend_from_slice(&[6, i + 6, i + 7]); // top, facing +z
    }
    for i in 0..6u32 {
        let j = (i + 1) % 6;
        indices.extend_from_slice(&[i, j, j + 6, i, j + 6, i + 6]);
    }
    (positions, indices)
}

/// A 3x3x1 slab with a 1x1 hole through it: genus 1, volume 8.
fn holed_slab() -> Mesh {
    let mut quads: Vec<[[f32; 3]; 4]> = Vec::new();
    for ix in 0..3 {
        for iy in 0..3 {
            if ix == 1 && iy == 1 {
                continue;
            }
            let (x, y) = (ix as f32, iy as f32);
            quads.push([
                [x, y, 1.0],
                [x + 1.0, y, 1.0],
                [x + 1.0, y + 1.0, 1.0],
                [x, y + 1.0, 1.0],
            ]);
            quads.push([
                [x, y, 0.0],
                [x, y + 1.0, 0.0],
                [x + 1.0, y + 1.0, 0.0],
                [x + 1.0, y, 0.0],
            ]);
        }
    }
    for i in 0..3 {
        let t = i as f32;
        quads.push([
            [t, 0.0, 0.0],
            [t + 1.0, 0.0, 0.0],
            [t + 1.0, 0.0, 1.0],
            [t, 0.0, 1.0],
        ]);
        quads.push([
            [t + 1.0, 3.0, 0.0],
            [t, 3.0, 0.0],
            [t, 3.0, 1.0],
            [t + 1.0, 3.0, 1.0],
        ]);
        quads.push([
            [0.0, t + 1.0, 0.0],
            [0.0, t, 0.0],
            [0.0, t, 1.0],
            [0.0, t + 1.0, 1.0],
        ]);
        quads.push([
            [3.0, t, 0.0],
            [3.0, t + 1.0, 0.0],
            [3.0, t + 1.0, 1.0],
            [3.0, t, 1.0],
        ]);
    }
    quads.push([[1.0, 1.0, 0.0], [1.0, 1.0, 1.0], [2.0, 1.0, 1.0], [2.0, 1.0, 0.0]]);
    quads.push([[2.0, 1.0, 0.0], [2.0, 1.0, 1.0], [2.0, 2.0, 1.0], [2.0, 2.0, 0.0]]);
    quads.push([[2.0, 2.0, 0.0], [2.0, 2.0, 1.0], [1.0, 2.0, 1.0], [1.0, 2.0, 0.0]]);
    quads.push([[1.0, 2.0, 0.0], [1.0, 2.0, 1.0], [1.0, 1.0, 1.0], [1.0, 1.0, 0.0]]);
    from_quads(&quads)
}

/// A regular octahedron with the given radius (convex, 8 triangles).
fn octahedron(r: f32) -> Mesh {
    let positions = vec![
        [r, 0.0, 0.0],
        [-r, 0.0, 0.0],
        [0.0, r, 0.0],
        [0.0, -r, 0.0],
        [0.0, 0.0, r],
        [0.0, 0.0, -r],
    ];
    let indices = vec![
        0, 2, 4, 2, 1, 4, 1, 3, 4, 3, 0, 4, // +z
        2, 0, 5, 1, 2, 5, 3, 1, 5, 0, 3, 5, // -z
    ];
    (positions, indices)
}

fn volume_of(positions: &[[f32; 3]], indices: &[u32]) -> f64 {
    indices
        .chunks(3)
        .map(|t| {
            let p = |i: u32| positions[i as usize].map(f64::from);
            let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
            let cr = [
                b[1] * c[2] - b[2] * c[1],
                b[2] * c[0] - b[0] * c[2],
                b[0] * c[1] - b[1] * c[0],
            ];
            (a[0] * cr[0] + a[1] * cr[1] + a[2] * cr[2]) / 6.0
        })
        .sum()
}

/// Every directed edge once and its reverse once.
fn is_closed(c: &FractureChunk) -> bool {
    let mut edges: HashMap<(u32, u32), u32> = HashMap::new();
    for t in c.indices.chunks(3) {
        for e in 0..3 {
            *edges.entry((t[e], t[(e + 1) % 3])).or_insert(0) += 1;
        }
    }
    edges
        .iter()
        .all(|(&(a, b), &n)| n == 1 && edges.contains_key(&(b, a)))
}

fn connected(set: &FractureSet) -> bool {
    let adj = set.adjacency();
    let mut seen = vec![false; adj.len()];
    let mut stack = vec![0usize];
    seen[0] = true;
    while let Some(i) = stack.pop() {
        for &n in &adj[i] {
            if !seen[n as usize] {
                seen[n as usize] = true;
                stack.push(n as usize);
            }
        }
    }
    seen.iter().all(|s| *s)
}

fn assert_conserved(set: &FractureSet, source: f64) {
    let total = set.total_chunk_volume();
    assert!(
        (total - source).abs() <= 1e-4 * source,
        "chunk volumes {total} vs source {source}"
    );
    for (i, c) in set.chunks.iter().enumerate() {
        assert!(is_closed(c), "chunk {i} is not closed");
        let v = volume_of(&c.positions, &c.indices);
        assert!(
            (v - f64::from(c.volume)).abs() <= 1e-4 * f64::from(c.volume).max(1e-6),
            "chunk {i}: mesh volume {v} vs reported {}",
            c.volume
        );
        assert!(c.volume > 0.0);
        assert!((c.interior_start as usize) <= c.triangle_count());
    }
}

#[test]
fn a_box_makes_the_asked_number_of_chunks_and_keeps_its_volume() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 0.5, 0.75])?;
    let set = fracture_mesh(&p, &i, &FractureSettings::random(8, 11))?;
    assert_eq!(set.chunks.len(), 8);
    assert_eq!(set.format_version, FRACTURE_FORMAT_VERSION);
    assert_conserved(&set, 2.0 * 1.0 * 1.5);
    assert!((f64::from(set.source_volume) - 3.0).abs() < 1e-5);
    Ok(())
}

#[test]
fn chunk_count_follows_the_request() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    for n in [1usize, 2, 5, 16, 40] {
        let set = fracture_mesh(&p, &i, &FractureSettings::random(n, 3))?;
        assert_eq!(set.chunks.len(), n, "asked for {n}");
        assert_conserved(&set, 8.0);
    }
    Ok(())
}

#[test]
fn one_chunk_is_the_whole_solid_with_no_joints() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let set = fracture_mesh(&p, &i, &FractureSettings::random(1, 1))?;
    assert_eq!(set.chunks.len(), 1);
    assert!(set.joints.is_empty());
    assert_eq!(
        set.chunks[0].interior_start as usize,
        set.chunks[0].triangle_count()
    );
    assert!((set.chunks[0].interior_area).abs() < 1e-9);
    Ok(())
}

#[test]
fn adjacency_is_symmetric_and_connected() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let set = fracture_mesh(&p, &i, &FractureSettings::random(24, 5))?;
    assert!(!set.joints.is_empty());
    for j in &set.joints {
        assert!(j.a < j.b);
        assert!(set.neighbors(j.a).contains(&j.b));
        assert!(set.neighbors(j.b).contains(&j.a));
    }
    let adj = set.adjacency();
    for (a, list) in adj.iter().enumerate() {
        for &b in list {
            assert!(
                adj[b as usize].contains(&(a as u32)),
                "{a} lists {b} but not the reverse"
            );
        }
    }
    let mut pairs: Vec<(u32, u32)> = set.joints.iter().map(|j| (j.a, j.b)).collect();
    pairs.dedup();
    assert_eq!(pairs.len(), set.joints.len(), "one joint per pair");
    assert!(connected(&set), "the contact graph of a solid block is connected");
    Ok(())
}

#[test]
fn contact_areas_account_for_every_interior_face() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut s = FractureSettings::random(12, 9);
    s.joints.min_contact_area = 0.0;
    let set = fracture_mesh(&p, &i, &s)?;
    // Every interior face is shared by exactly two chunks, so the joints' areas sum to half of
    // the chunks' interior area.
    let interior: f64 = set.chunks.iter().map(|c| f64::from(c.interior_area)).sum();
    let joints: f64 = set.joints.iter().map(|j| f64::from(j.contact_area)).sum();
    assert!(
        (2.0 * joints - interior).abs() <= 1e-4 * interior,
        "{joints} vs {interior}"
    );
    Ok(())
}

#[test]
fn two_explicit_sites_split_a_box_through_the_middle() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut s = FractureSettings::random(2, 0);
    s.sites = FractureSites::Explicit(vec![[-0.5, 0.0, 0.0], [0.5, 0.0, 0.0]]);
    s.joints.strength_per_area = 1000.0;
    let set = fracture_mesh(&p, &i, &s)?;
    assert_eq!(set.chunks.len(), 2);
    assert_eq!(set.joints.len(), 1);
    let j = &set.joints[0];
    assert!(
        (j.contact_area - 4.0).abs() < 1e-4,
        "the cut is the 2x2 mid plane: {}",
        j.contact_area
    );
    assert!((j.break_force - 4000.0).abs() < 0.1);
    assert!(j.normal[0] > 0.999 && j.normal[1].abs() < 1e-6);
    assert!(j.anchor[0].abs() < 1e-5 && j.anchor[1].abs() < 1e-5 && j.anchor[2].abs() < 1e-5);
    for c in &set.chunks {
        assert!((c.volume - 4.0).abs() < 1e-4);
        assert!((c.interior_area - 4.0).abs() < 1e-4);
    }
    assert_conserved(&set, 8.0);
    Ok(())
}

/// Audit ext-b Medium: the zero-volume check was relative to the cube of the bounding-box
/// diagonal, so a valid long thin solid (a 1000 m rod 0.6 mm thick, volume 3.6e-4 m^3 against a
/// 1e-3 threshold) was refused as `ZeroVolume`. The flat sliver pair above is still refused.
#[test]
fn a_long_thin_rod_is_not_refused_as_zero_volume() -> Result<(), FractureError> {
    let (p, i) = box_mesh([500.0, 0.0003, 0.0003])?;
    let mut s = FractureSettings::random(2, 0);
    s.sites = FractureSites::Explicit(vec![[-250.0, 0.0, 0.0], [250.0, 0.0, 0.0]]);
    let result = fracture_mesh(&p, &i, &s);
    assert_ne!(
        result.as_ref().err(),
        Some(&FractureError::ZeroVolume),
        "a thin rod has volume"
    );
    if let Ok(set) = result {
        assert_eq!(set.chunks.len(), 2);
    }
    Ok(())
}

#[test]
fn break_force_is_clamped_to_the_joint_limits() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut s = FractureSettings::random(2, 0);
    s.sites = FractureSites::Explicit(vec![[-0.5, 0.0, 0.0], [0.5, 0.0, 0.0]]);
    s.joints.strength_per_area = 1000.0;
    s.joints.min_break_force = 5000.0;
    assert!((fracture_mesh(&p, &i, &s)?.joints[0].break_force - 5000.0).abs() < 0.01);
    s.joints.min_break_force = 0.0;
    s.joints.max_break_force = Some(2500.0);
    assert!((fracture_mesh(&p, &i, &s)?.joints[0].break_force - 2500.0).abs() < 0.01);
    Ok(())
}

#[test]
fn small_contacts_are_not_joints() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut s = FractureSettings::random(2, 0);
    s.sites = FractureSites::Explicit(vec![[-0.5, 0.0, 0.0], [0.5, 0.0, 0.0]]);
    s.joints.min_contact_area = 10.0;
    assert!(fracture_mesh(&p, &i, &s)?.joints.is_empty());
    Ok(())
}

#[test]
fn the_same_seed_gives_the_same_fracture_and_another_seed_a_different_one() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let a = fracture_mesh(&p, &i, &FractureSettings::random(10, 77))?;
    let b = fracture_mesh(&p, &i, &FractureSettings::random(10, 77))?;
    let c = fracture_mesh(&p, &i, &FractureSettings::random(10, 78))?;
    assert_eq!(a, b);
    assert_eq!(a.to_ron()?, b.to_ron()?);
    assert_ne!(a, c);
    Ok(())
}

#[test]
fn a_concave_l_prism_keeps_its_volume() -> Result<(), FractureError> {
    let (p, i) = l_prism();
    assert!((volume_of(&p, &i) - 3.0).abs() < 1e-9, "fixture volume");
    for seed in [1u64, 2, 3] {
        let set = fracture_mesh(&p, &i, &FractureSettings::random(7, seed))?;
        assert!(!set.chunks.is_empty());
        assert_conserved(&set, 3.0);
    }
    Ok(())
}

#[test]
fn a_solid_with_a_through_hole_caps_annular_sections() -> Result<(), FractureError> {
    let (p, i) = holed_slab();
    assert!((volume_of(&p, &i) - 8.0).abs() < 1e-9, "fixture volume");
    for seed in [4u64, 5, 6] {
        let set = fracture_mesh(&p, &i, &FractureSettings::random(6, seed))?;
        assert_conserved(&set, 8.0);
    }
    // Two sites on opposite sides of the hole: the cut passes through the hole.
    let mut s = FractureSettings::random(2, 0);
    s.sites = FractureSites::Explicit(vec![[0.4, 1.5, 0.5], [2.6, 1.5, 0.5]]);
    let set = fracture_mesh(&p, &i, &s)?;
    assert_eq!(set.chunks.len(), 2);
    assert_conserved(&set, 8.0);
    // The mid plane x = 1.5 crosses the slab (3 x 1) minus the hole (1 x 1).
    assert!(
        (set.joints[0].contact_area - 2.0).abs() < 1e-4,
        "{}",
        set.joints[0].contact_area
    );
    Ok(())
}

#[test]
fn a_convex_octahedron_fractures() -> Result<(), FractureError> {
    let (p, i) = octahedron(1.0);
    let source = volume_of(&p, &i);
    assert!((source - 4.0 / 3.0).abs() < 1e-6);
    let set = fracture_mesh(&p, &i, &FractureSettings::random(9, 21))?;
    assert_conserved(&set, source);
    Ok(())
}

#[test]
fn chunks_carry_surface_triangles_first_then_caps() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let set = fracture_mesh(&p, &i, &FractureSettings::random(4, 2))?;
    for c in &set.chunks {
        assert!(c.surface_triangle_count() > 0 || c.interior_area > 0.0);
        assert!(c.interior_start as usize <= c.triangle_count());
        // Every vertex and the centroid are inside the chunk's bounds.
        for v in c.positions.iter().chain(std::iter::once(&c.centroid)) {
            for ((x, lo), hi) in v.iter().zip(&c.bounds_min).zip(&c.bounds_max) {
                assert!(*x >= lo - 1e-5 && *x <= hi + 1e-5);
            }
        }
    }
    Ok(())
}

#[test]
fn chunk_descs_carry_mass_from_volume() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let set = fracture_mesh(&p, &i, &FractureSettings::random(5, 8))?;
    let descs = set.chunk_descs(1000.0);
    assert_eq!(descs.len(), 5);
    let mass: f32 = descs.iter().map(|d| d.mass).sum();
    assert!((mass - 8000.0).abs() < 1.0, "{mass}");
    Ok(())
}

#[test]
fn ron_round_trips_and_checks_the_version() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let set = fracture_mesh(&p, &i, &FractureSettings::random(4, 6))?;
    let text = set.to_ron()?;
    assert!(text.contains("format_version: 2"));
    assert_eq!(FractureSet::from_ron(&text)?, set);
    let newer = text.replace("format_version: 2", "format_version: 3");
    assert_eq!(
        FractureSet::from_ron(&newer),
        Err(FractureError::UnsupportedVersion {
            found: 3,
            expected: 2
        })
    );
    assert!(matches!(
        FractureSet::from_ron("(nope"),
        Err(FractureError::Format(_))
    ));
    Ok(())
}

/// A finite tetrahedron chunk with `volume` (the audit's `.fracture.ron` case).
fn tetra_set_ron(volume: &str, centroid: &str) -> String {
    format!(
        "(format_version: 1, source_volume: 0.16666667, chunks: [(site: (0.25, 0.25, 0.25), \
         positions: [(0.0, 0.0, 0.0), (1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0)], \
         indices: [0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3], interior_start: 4, volume: {volume}, \
         centroid: {centroid}, bounds_min: (0.0, 0.0, 0.0), bounds_max: (1.0, 1.0, 1.0), \
         interior_area: 0.0)], joints: [])"
    )
}

#[test]
fn a_non_finite_chunk_volume_or_point_is_refused_before_it_becomes_a_mass() -> Result<(), FractureError> {
    // The finite version loads and gives a finite mass.
    let set = FractureSet::from_ron(&tetra_set_ron("0.16666667", "(0.25, 0.25, 0.25)"))?;
    assert!(set.chunk_descs(1000.0).iter().all(|d| d.mass.is_finite()));
    for (volume, centroid) in [
        ("inf", "(0.25, 0.25, 0.25)"),
        ("-inf", "(0.25, 0.25, 0.25)"),
        ("NaN", "(0.25, 0.25, 0.25)"),
        ("0.16666667", "(inf, 0.25, 0.25)"),
    ] {
        let err = FractureSet::from_ron(&tetra_set_ron(volume, centroid)).map(|_| ());
        assert!(
            matches!(&err, Err(FractureError::Format(m)) if m.contains("not finite")),
            "volume {volume}, centroid {centroid}: {err:?}"
        );
    }
    let bad_point =
        tetra_set_ron("0.16666667", "(0.25, 0.25, 0.25)").replace("(0.0, 0.0, 1.0)]", "(0.0, 0.0, inf)]");
    assert_eq!(
        FractureSet::from_ron(&bad_point)
            .map_err(|e| e.code())
            .map(|_| ()),
        Err("fracture_format")
    );
    Ok(())
}

#[test]
fn bad_meshes_are_rejected_with_their_own_error() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let s = FractureSettings::random(3, 1);
    assert_eq!(fracture_mesh(&p, &[], &s), Err(FractureError::EmptyMesh));
    assert_eq!(
        fracture_mesh(&p, &i[..10], &s),
        Err(FractureError::BadIndexCount(10))
    );
    let mut oob = i.clone();
    oob[0] = 99;
    assert_eq!(
        fracture_mesh(&p, &oob, &s),
        Err(FractureError::IndexOutOfRange {
            index: 99,
            vertices: 8
        })
    );
    let mut nan = p.clone();
    nan[2][1] = f32::NAN;
    assert_eq!(fracture_mesh(&nan, &i, &s), Err(FractureError::NonFinite(2)));
    assert_eq!(fracture_mesh(&p, &i[..33], &s), Err(FractureError::NotClosed));
    let flipped: Vec<u32> = i.chunks(3).flat_map(|t| [t[0], t[2], t[1]]).collect();
    assert_eq!(fracture_mesh(&p, &flipped, &s), Err(FractureError::InsideOut));
    let mut degenerate = i.clone();
    degenerate[1] = degenerate[0];
    assert_eq!(
        fracture_mesh(&p, &degenerate, &s),
        Err(FractureError::DegenerateTriangle(0))
    );
    // A triangle listed twice is a non-manifold edge.
    let mut doubled = i.clone();
    doubled.extend_from_slice(&i[..3]);
    assert_eq!(fracture_mesh(&p, &doubled, &s), Err(FractureError::NotManifold));
    // A flat sliver pair (two coincident triangles facing opposite ways) has no volume.
    let flat_p = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    assert_eq!(
        fracture_mesh(&flat_p, &[0, 1, 2, 0, 2, 1], &s),
        Err(FractureError::ZeroVolume)
    );
    Ok(())
}

#[test]
fn bad_settings_and_sites_are_rejected() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let code = |s: &FractureSettings| fracture_mesh(&p, &i, s).map(|_| ()).map_err(|e| e.code());
    assert_eq!(
        code(&FractureSettings::random(0, 1)),
        Err("fracture_invalid_settings")
    );
    assert_eq!(
        code(&FractureSettings::random(MAX_CHUNKS + 1, 1)),
        Err("fracture_invalid_settings")
    );
    let mut s = FractureSettings::random(3, 1);
    s.candidates_per_site = 0;
    assert_eq!(code(&s), Err("fracture_invalid_settings"));
    let mut s = FractureSettings::random(3, 1);
    s.min_chunk_volume_fraction = 1.5;
    assert_eq!(code(&s), Err("fracture_invalid_settings"));
    let mut s = FractureSettings::random(3, 1);
    s.joints.strength_per_area = f32::NAN;
    assert_eq!(code(&s), Err("fracture_invalid_settings"));
    let mut s = FractureSettings::random(3, 1);
    s.joints.min_break_force = 10.0;
    s.joints.max_break_force = Some(5.0);
    assert_eq!(code(&s), Err("fracture_invalid_settings"));
    let mut s = FractureSettings::random(3, 1);
    s.sites = FractureSites::Explicit(Vec::new());
    assert_eq!(code(&s), Err("fracture_invalid_settings"));
    s.sites = FractureSites::Explicit(vec![[5.0, 0.0, 0.0]]);
    assert_eq!(code(&s), Err("fracture_site_outside"));
    s.sites = FractureSites::Explicit(vec![[0.0, 0.0, 0.0], [0.0, 0.0, 0.0]]);
    assert_eq!(code(&s), Err("fracture_duplicate_sites"));
    s.sites = FractureSites::Explicit(vec![[f32::NAN, 0.0, 0.0]]);
    assert_eq!(code(&s), Err("fracture_non_finite"));
    assert!(matches!(
        box_mesh([0.0, 1.0, 1.0]),
        Err(FractureError::InvalidSettings(_))
    ));
    Ok(())
}

#[test]
fn candidates_per_site_is_capped() -> Result<(), FractureError> {
    // `u32::MAX` candidates looped billions of inside tests (ADR-1660).
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut s = FractureSettings::random(3, 1);
    s.candidates_per_site = u32::MAX;
    assert_eq!(
        fracture_mesh(&p, &i, &s).map(|_| ()).map_err(|e| e.code()),
        Err("fracture_invalid_settings")
    );
    s.candidates_per_site = MAX_CANDIDATES_PER_SITE + 1;
    assert!(fracture_mesh(&p, &i, &s).is_err());
    s.candidates_per_site = MAX_CANDIDATES_PER_SITE;
    assert_eq!(fracture_mesh(&p, &i, &s)?.chunks.len(), 3);
    Ok(())
}

#[test]
fn a_floor_that_drops_every_chunk_is_an_error_not_an_empty_set() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut s = FractureSettings::random(2, 1);
    s.sites = FractureSites::Explicit(vec![[-0.5, 0.0, 0.0], [0.5, 0.0, 0.0]]);
    s.min_chunk_volume_fraction = 0.99;
    let err = fracture_mesh(&p, &i, &s)
        .map(|_| ())
        .expect_err("both halves dropped");
    assert_eq!(err.code(), "fracture_invalid_settings");
    assert!(err.to_string().contains("min_chunk_volume_fraction"), "{err}");
    s.min_chunk_volume_fraction = 0.4;
    assert_eq!(fracture_mesh(&p, &i, &s)?.chunks.len(), 2);
    Ok(())
}

#[test]
fn a_mesh_with_a_doubled_edge_and_a_hole_names_the_same_error_every_time() -> Result<(), FractureError> {
    // The edge check walked a hash map, so the error could flip between runs (ADR-1660). Every
    // `HashMap` gets its own random keys, so repeating the call in one process shows it.
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut bad = i[..33].to_vec();
    bad.extend_from_slice(&i[..3]);
    let s = FractureSettings::random(3, 1);
    let first = fracture_mesh(&p, &bad, &s).map(|_| ()).expect_err("bad mesh");
    assert!(matches!(
        first,
        FractureError::NotManifold | FractureError::NotClosed
    ));
    for _ in 0..64 {
        assert_eq!(fracture_mesh(&p, &bad, &s).map(|_| ()), Err(first.clone()));
    }
    Ok(())
}

#[test]
fn error_codes_are_lowercase_ascii_digits_and_underscores() {
    let all = [
        FractureError::EmptyMesh,
        FractureError::BadIndexCount(1),
        FractureError::IndexOutOfRange {
            index: 0,
            vertices: 0,
        },
        FractureError::NonFinite(0),
        FractureError::DegenerateTriangle(0),
        FractureError::NotManifold,
        FractureError::NotClosed,
        FractureError::InsideOut,
        FractureError::ZeroVolume,
        FractureError::InvalidSettings(String::new()),
        FractureError::SiteSampling(0),
        FractureError::SiteOutside(0),
        FractureError::DuplicateSites(0, 1),
        FractureError::CapFailed(0),
        FractureError::Format(String::new()),
        FractureError::BadAttributes(String::new()),
        FractureError::Tangents(0, String::new()),
        FractureError::UnsupportedVersion {
            found: 0,
            expected: 1,
        },
    ];
    let mut seen = std::collections::HashSet::new();
    for e in &all {
        let c = e.code();
        assert!(c.starts_with("fracture_"));
        assert!(
            c.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{c}"
        );
        assert!(seen.insert(c), "duplicate code {c}");
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn a_site_pressed_against_the_surface_leaves_no_sliver_chunk() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut s = FractureSettings::random(2, 0);
    // The second site sits 0.01 from the +x face: its cell is a 2x2x0.055 slab (2.75 % of the
    // volume). With a 5 % floor the slab is dropped, and the first chunk stays whole.
    s.sites = FractureSites::Explicit(vec![[0.9, 0.0, 0.0], [0.99, 0.0, 0.0]]);
    s.min_chunk_volume_fraction = 0.05;
    let set = fracture_mesh(&p, &i, &s)?;
    assert_eq!(set.chunks.len(), 1);
    assert!(set.joints.is_empty());
    Ok(())
}

#[test]
fn many_seeds_and_shapes_stay_watertight_and_conserve_volume() -> Result<(), FractureError> {
    let shapes: [(Mesh, f64); 3] = [
        (box_mesh([1.0, 0.6, 0.4])?, 2.0 * 1.2 * 0.8),
        (l_prism(), 3.0),
        (holed_slab(), 8.0),
    ];
    for (mesh, volume) in &shapes {
        for seed in 0..12u64 {
            let count = 2 + (seed as usize * 5) % 23;
            let set = fracture_mesh(&mesh.0, &mesh.1, &FractureSettings::random(count, seed))?;
            assert!(!set.chunks.is_empty());
            assert_conserved(&set, *volume);
        }
    }
    Ok(())
}

#[test]
fn a_few_hundred_chunks_still_conserve_volume() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let set = fracture_mesh(&p, &i, &FractureSettings::random(200, 42))?;
    assert_eq!(set.chunks.len(), 200);
    assert_conserved(&set, 8.0);
    assert!(connected(&set));
    Ok(())
}

#[test]
fn a_translated_and_scaled_mesh_fractures_the_same_way() -> Result<(), FractureError> {
    // The same fracture seed on a mesh moved far from the origin still conserves volume (the
    // clip tolerance scales with the mesh size).
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let moved: Vec<[f32; 3]> = p
        .iter()
        .map(|v| [v[0] * 50.0 + 1000.0, v[1] * 50.0 - 300.0, v[2] * 50.0])
        .collect();
    let set = fracture_mesh(&moved, &i, &FractureSettings::random(10, 5))?;
    assert_conserved(&set, 8.0 * 125_000.0);
    Ok(())
}

/// Fracture settings with `FractureSites::Impact` (ADR-9870).
fn impact_settings(count: usize, seed: u64, at: [f32; 3], bias: f32) -> FractureSettings {
    let mut s = FractureSettings::random(count, seed);
    s.sites = FractureSites::Impact {
        count,
        seed,
        at,
        bias,
    };
    s
}

/// Fracture settings with `FractureSites::Clusters` (ADR-9870).
fn cluster_settings(count: usize, seed: u64, clusters: usize, spread: f32) -> FractureSettings {
    let mut s = FractureSettings::random(count, seed);
    s.sites = FractureSites::Clusters {
        count,
        seed,
        clusters,
        spread,
    };
    s
}

/// Euclidean distance between two points as f64.
fn dist3(a: [f32; 3], b: [f32; 3]) -> f64 {
    let d = [
        f64::from(a[0] - b[0]),
        f64::from(a[1] - b[1]),
        f64::from(a[2] - b[2]),
    ];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// Mean distance of the kept chunks' sites to `at`.
fn mean_site_distance(set: &FractureSet, at: [f32; 3]) -> f64 {
    set.chunks.iter().map(|c| dist3(c.site, at)).sum::<f64>() / set.chunks.len() as f64
}

/// Mean over the chunks of the distance from a chunk's site to the nearest other site.
fn mean_nearest_neighbour(set: &FractureSet) -> f64 {
    set.chunks
        .iter()
        .map(|c| {
            set.chunks
                .iter()
                .filter(|o| o.site != c.site)
                .map(|o| dist3(c.site, o.site))
                .fold(f64::INFINITY, f64::min)
        })
        .sum::<f64>()
        / set.chunks.len() as f64
}

#[test]
fn impact_sites_crowd_the_hit_monotonically_and_are_byte_stable() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    // The centre of the +x face: where a hit lands.
    let hit = [1.0, 0.0, 0.0];
    let even = fracture_mesh(&p, &i, &impact_settings(48, 7, hit, 0.0))?;
    let biased = fracture_mesh(&p, &i, &impact_settings(48, 7, hit, 1.0))?;
    let strong = fracture_mesh(&p, &i, &impact_settings(48, 7, hit, 2.0))?;
    for set in [&even, &biased, &strong] {
        assert_conserved(set, 8.0);
    }
    let (de, db, ds) = (
        mean_site_distance(&even, hit),
        mean_site_distance(&biased, hit),
        mean_site_distance(&strong, hit),
    );
    assert!(de > db, "bias 1 pulls the sites to the hit: {de} vs {db}");
    assert!(db > ds, "bias 2 pulls harder: {db} vs {ds}");
    // The same settings give the same bytes; another seed does not.
    let bytes = strong.to_ron()?;
    assert_eq!(
        bytes,
        fracture_mesh(&p, &i, &impact_settings(48, 7, hit, 2.0))?.to_ron()?
    );
    assert_ne!(
        bytes,
        fracture_mesh(&p, &i, &impact_settings(48, 8, hit, 2.0))?.to_ron()?
    );
    Ok(())
}

#[test]
fn impact_sites_make_the_debris_finer_near_the_hit() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let hit = [1.0, 0.0, 0.0];
    let set = fracture_mesh(&p, &i, &impact_settings(48, 7, hit, 2.0))?;
    let mut by_dist: Vec<(f64, f32)> = set
        .chunks
        .iter()
        .map(|c| (dist3(c.site, hit), c.volume))
        .collect();
    by_dist.sort_by(|a, b| a.0.total_cmp(&b.0));
    const EDGE: usize = 12;
    let near: f64 = by_dist[..EDGE].iter().map(|(_, v)| f64::from(*v)).sum::<f64>() / EDGE as f64;
    let far: f64 = by_dist[by_dist.len() - EDGE..]
        .iter()
        .map(|(_, v)| f64::from(*v))
        .sum::<f64>()
        / EDGE as f64;
    assert!(
        near < 0.5 * far,
        "the {EDGE} chunks nearest the hit are finer than the {EDGE} farthest: {near} vs {far}"
    );
    Ok(())
}

#[test]
fn clustered_sites_stay_in_their_clumps_and_are_byte_stable() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let seeded = fracture_mesh(&p, &i, &FractureSettings::random(24, 5))?;
    let clumped = fracture_mesh(&p, &i, &cluster_settings(24, 5, 3, 0.25))?;
    assert_eq!(clumped.chunks.len(), 24, "no clump chunk is a dropped sliver");
    assert_conserved(&clumped, 8.0);
    let (dc, dn) = (mean_nearest_neighbour(&clumped), mean_nearest_neighbour(&seeded));
    assert!(
        dc < 0.5 * dn,
        "clumped sites sit nearer a neighbour than the seeded spread: {dc} vs {dn}"
    );
    let bytes = clumped.to_ron()?;
    assert_eq!(
        bytes,
        fracture_mesh(&p, &i, &cluster_settings(24, 5, 3, 0.25))?.to_ron()?
    );
    assert_ne!(
        bytes,
        fracture_mesh(&p, &i, &cluster_settings(24, 6, 3, 0.25))?.to_ron()?
    );
    Ok(())
}

#[test]
fn impact_and_cluster_settings_answer_with_stable_reasons() -> Result<(), FractureError> {
    let (p, i) = box_mesh([1.0, 1.0, 1.0])?;
    let mut s = FractureSettings::random(24, 1);
    let mut refuse = |sites: FractureSites| {
        s.sites = sites;
        let e = fracture_mesh(&p, &i, &s).expect_err("refused");
        assert_eq!(e.code(), "fracture_invalid_settings", "{e}");
    };
    refuse(FractureSites::Impact {
        count: 0,
        seed: 1,
        at: [0.0; 3],
        bias: 1.0,
    });
    refuse(FractureSites::Impact {
        count: MAX_CHUNKS + 1,
        seed: 1,
        at: [0.0; 3],
        bias: 1.0,
    });
    refuse(FractureSites::Impact {
        count: 8,
        seed: 1,
        at: [f32::NAN, 0.0, 0.0],
        bias: 1.0,
    });
    refuse(FractureSites::Impact {
        count: 8,
        seed: 1,
        at: [0.0; 3],
        bias: 0.5,
    });
    refuse(FractureSites::Impact {
        count: 8,
        seed: 1,
        at: [0.0; 3],
        bias: 3.0,
    });
    refuse(FractureSites::Clusters {
        count: 24,
        seed: 1,
        clusters: 0,
        spread: 0.5,
    });
    refuse(FractureSites::Clusters {
        count: 24,
        seed: 1,
        clusters: 25,
        spread: 0.5,
    });
    refuse(FractureSites::Clusters {
        count: 0,
        seed: 1,
        clusters: 1,
        spread: 0.5,
    });
    refuse(FractureSites::Clusters {
        count: 24,
        seed: 1,
        clusters: 3,
        spread: 0.0,
    });
    refuse(FractureSites::Clusters {
        count: 24,
        seed: 1,
        clusters: 3,
        spread: f32::INFINITY,
    });
    // A hit impossibly far from the solid: no draw lands inside it.
    s.sites = FractureSites::Impact {
        count: 4,
        seed: 1,
        at: [5000.0, 0.0, 0.0],
        bias: 2.0,
    };
    let e = fracture_mesh(&p, &i, &s).expect_err("refused");
    assert_eq!(e.code(), "fracture_site_sampling", "{e}");
    Ok(())
}

/// FNV-1a 64 of a byte string (Fowler, Noll and Vo; the digest the review pinned with).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The same mesh, settings and seed give the same bytes on every target (ADR-9870, ADR-7550): the
/// impact and clustered samplers and the cut they feed use only IEEE-exact arithmetic. The digests
/// below were computed on x86_64 Linux, aarch64 Android (TEST Pixel 9 Pro) and x86_64 Windows
/// (GNU) and agreed before they were pinned; a change here is a change of output on every target
/// (re-pin only for an intended sampler or cut change, with the reason in the commit message).
#[test]
fn impact_and_clustered_fractures_have_the_same_digest_on_every_target() -> Result<(), FractureError> {
    let cube: Mesh = box_mesh([1.0, 1.0, 1.0])?;
    let pane: Mesh = box_mesh([1.0, 1.0, 0.005])?;
    let hit = [1.0, 0.0, 0.0];
    let corner = [1.0, 1.0, 0.005];
    let cases = [
        ("cube impact bias 0", &cube, impact_settings(48, 7, hit, 0.0)),
        ("cube impact bias 1", &cube, impact_settings(48, 7, hit, 1.0)),
        ("cube impact bias 2", &cube, impact_settings(48, 7, hit, 2.0)),
        ("cube clusters", &cube, cluster_settings(24, 5, 3, 0.25)),
        ("pane corner bias 1", &pane, impact_settings(24, 3, corner, 1.0)),
        ("pane corner bias 2", &pane, impact_settings(24, 3, corner, 2.0)),
    ];
    let mut got = Vec::new();
    for (name, (p, i), settings) in &cases {
        let ron = fracture_mesh(p, i, settings)?.to_ron()?;
        got.push(format!("{name}: {:016x} {}", fnv1a(ron.as_bytes()), ron.len()));
    }
    assert_eq!(got.join("\n"), DIGESTS, "fracture digests on this target");
    Ok(())
}

/// The pinned digests of
/// `impact_and_clustered_fractures_have_the_same_digest_on_every_target`: RON FNV-1a and length.
const DIGESTS: &str = "cube impact bias 0: 4b4b514a9505caa8 5124979
cube impact bias 1: 8b30fbd146a6ad86 5297315
cube impact bias 2: 337c8b62c9d83111 4680820
cube clusters: e7f95387008fb422 1544710
pane corner bias 1: db9ab6a5c4000667 883404
pane corner bias 2: 25a7c84602e60a82 855012";
