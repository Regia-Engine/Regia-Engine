// SPDX-License-Identifier: MPL-2.0
//! The bird's procedural mesh (ADR-1502): 14 rigid parts posed from a [`BirdPose`].
//!
//! There are no other-engine assets and no glTF. The body and head are ellipsoids, the beak a
//! cone, the eyes two spheres each (a pale sclera and a dark pupil that sits at its rim, so a
//! wide-eyed sclera shows a ring), and the wings two tapered slabs each, hinged at the
//! shoulder and the wrist. The tail is a tapered slab whose width follows the spread, and the
//! legs are thin cylinders that tuck away in flight. Every part is its own entity with a world
//! transform written each frame, so the renderer's per-object motion vectors (previous model
//! matrix) give TAA and motion blur the flapping wings.

use std::sync::Arc;

use regia_core::math::{Quat, Vec3};
use regia_render::primitives::{Cone, ConeAnchor, Cylinder, Sphere};
use regia_render::{CpuMaterial, CpuMesh, LabVertex};
use regia_scene::Transform;

use super::sim::BirdPose;
use super::spec::BirdBody;

/// The parts, in [`BirdMesh::parts`] order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BirdPart {
    /// Body.
    Body,
    /// Head.
    Head,
    /// Beak.
    Beak,
    /// Left eye white.
    EyeLeft,
    /// Right eye white.
    EyeRight,
    /// Left pupil.
    PupilLeft,
    /// Right pupil.
    PupilRight,
    /// Left inner wing (shoulder to wrist).
    WingInnerLeft,
    /// Left outer wing (wrist to tip).
    WingOuterLeft,
    /// Right inner wing.
    WingInnerRight,
    /// Right outer wing.
    WingOuterRight,
    /// Tail.
    Tail,
    /// Left leg.
    LegLeft,
    /// Right leg.
    LegRight,
}

/// Number of parts.
pub const BIRD_PARTS: usize = 14;

impl BirdPart {
    /// Every part, in order.
    pub const ALL: [Self; BIRD_PARTS] = [
        Self::Body,
        Self::Head,
        Self::Beak,
        Self::EyeLeft,
        Self::EyeRight,
        Self::PupilLeft,
        Self::PupilRight,
        Self::WingInnerLeft,
        Self::WingOuterLeft,
        Self::WingInnerRight,
        Self::WingOuterRight,
        Self::Tail,
        Self::LegLeft,
        Self::LegRight,
    ];

    /// The part's name (`wing_outer_left`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Head => "head",
            Self::Beak => "beak",
            Self::EyeLeft => "eye_left",
            Self::EyeRight => "eye_right",
            Self::PupilLeft => "pupil_left",
            Self::PupilRight => "pupil_right",
            Self::WingInnerLeft => "wing_inner_left",
            Self::WingOuterLeft => "wing_outer_left",
            Self::WingInnerRight => "wing_inner_right",
            Self::WingOuterRight => "wing_outer_right",
            Self::Tail => "tail",
            Self::LegLeft => "leg_left",
            Self::LegRight => "leg_right",
        }
    }
}

/// Meshes and materials of one bird's parts (built once per bird).
#[derive(Clone, Debug)]
pub struct BirdMesh {
    /// One mesh per part, [`BirdPart::ALL`] order.
    pub parts: [Arc<CpuMesh>; BIRD_PARTS],
    /// One material per part.
    pub materials: [Arc<CpuMaterial>; BIRD_PARTS],
}

/// Wing slab dimensions: `(length, root chord, tip chord, tip sweep back, root thickness,
/// tip thickness)`.
type Slab = (f32, f32, f32, f32, f32, f32);

/// The wing's shoulder, the inner and outer slabs.
struct Wing {
    shoulder: Vec3,
    inner: Slab,
    outer: Slab,
}

fn wing(body: &BirdBody) -> Wing {
    let l = body.length_m;
    let half = (0.5 * body.wingspan_m - 0.11 * l).max(0.2 * l);
    Wing {
        shoulder: Vec3::new(0.11 * l, 0.05 * l, -0.06 * l),
        inner: (0.42 * half, 0.36 * l, 0.30 * l, 0.02 * l, 0.03 * l, 0.02 * l),
        outer: (0.58 * half, 0.30 * l, 0.09 * l, 0.10 * l, 0.02 * l, 0.008 * l),
    }
}

/// A box-topology mesh from eight corners `c[x + 2y + 4z]` (each 0 for the low side, 1 for
/// the high), flat-shaded, faces wound counter-clockwise from outside (as `Cuboid`).
fn slab_mesh(c: [Vec3; 8]) -> CpuMesh {
    const FACES: [[usize; 4]; 6] = [
        [4, 5, 7, 6],
        [2, 3, 1, 0],
        [1, 3, 7, 5],
        [4, 6, 2, 0],
        [3, 2, 6, 7],
        [5, 4, 0, 1],
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for face in FACES {
        let [a, b, d, e] = face.map(|i| c[i]);
        let n = (b - a).cross(d - a).normalize_or(Vec3::Y);
        let base = vertices.len() as u32;
        for (p, uv) in [(a, [0.0, 0.0]), (b, [1.0, 0.0]), (d, [1.0, 1.0]), (e, [0.0, 1.0])] {
            vertices.push(LabVertex::from_pos_normal_uv(p.to_array(), n.to_array(), uv));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base + 2, base + 3, base]);
    }
    CpuMesh { vertices, indices }
}

/// A tapered slab running along +X (`right`) or −X from the hinge, leading edge toward −Z.
fn wing_slab(s: Slab, right: bool) -> CpuMesh {
    let (len, c0, c1, sweep, t0, t1) = s;
    let corner = |xi: usize, yi: usize, zi: usize| {
        let (x, chord, thick, back) = if xi == 0 {
            (0.0, c0, t0, 0.0)
        } else {
            (len, c1, t1, sweep)
        };
        let z = back + if zi == 0 { -0.3 * chord } else { 0.7 * chord };
        let y = if yi == 0 { -0.5 * thick } else { 0.5 * thick };
        Vec3::new(x, y, z)
    };
    let mut c = [Vec3::ZERO; 8];
    for (i, slot) in c.iter_mut().enumerate() {
        let (xi, yi, zi) = (i & 1, (i >> 1) & 1, (i >> 2) & 1);
        *slot = if right {
            corner(xi, yi, zi)
        } else {
            // Mirror in X and swap the X sides, so the box topology (and the winding) holds.
            let p = corner(1 - xi, yi, zi);
            Vec3::new(-p.x, p.y, p.z)
        };
    }
    slab_mesh(c)
}

/// The tail: a slab from the rump back along +Z, `w0` wide at the root and `w1` at the end.
fn tail_slab(len: f32, w0: f32, w1: f32, thick: f32) -> CpuMesh {
    let mut c = [Vec3::ZERO; 8];
    for (i, slot) in c.iter_mut().enumerate() {
        let (xi, yi, zi) = (i & 1, (i >> 1) & 1, (i >> 2) & 1);
        let w = if zi == 0 { w0 } else { w1 };
        let x = if xi == 0 { -0.5 * w } else { 0.5 * w };
        let y = if yi == 0 { -0.5 * thick } else { 0.5 * thick };
        let z = if zi == 0 { 0.0 } else { len };
        *slot = Vec3::new(x, y, z);
    }
    slab_mesh(c)
}

/// A unit sphere squashed into an ellipsoid with these radii (normals corrected).
fn ellipsoid(radii: Vec3, sectors: u32, stacks: u32) -> CpuMesh {
    let mut mesh = Sphere {
        radius: 1.0,
        sectors,
        stacks,
    }
    .build();
    for v in &mut mesh.vertices {
        let p = Vec3::from_array(v.position) * radii;
        let n = (Vec3::from_array(v.normal) / radii).normalize_or(Vec3::Y);
        v.position = p.to_array();
        v.normal = n.to_array();
    }
    mesh
}

/// A mesh whose vertices and normals are turned by `r`.
fn turned(mut mesh: CpuMesh, r: Quat) -> CpuMesh {
    for v in &mut mesh.vertices {
        v.position = (r * Vec3::from_array(v.position)).to_array();
        v.normal = (r * Vec3::from_array(v.normal)).to_array();
    }
    mesh
}

fn material(color: [f32; 3], roughness: f32) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color: [color[0], color[1], color[2], 1.0],
        roughness,
        metallic: 0.0,
        ..CpuMaterial::default()
    })
}

impl BirdMesh {
    /// Builds the parts for a bird of this size and colour.
    #[must_use]
    pub fn build(body: &BirdBody) -> Self {
        let l = body.length_m;
        let w = wing(body);
        let luminance = 0.2126 * body.color[0] + 0.7152 * body.color[1] + 0.0722 * body.color[2];
        let horn = if luminance > 0.3 {
            [0.78, 0.58, 0.14]
        } else {
            [0.05, 0.045, 0.04]
        };
        let beak = Cone {
            radius: 0.032 * l,
            height: 0.13 * l,
            resolution: 12,
            anchor: ConeAnchor::Base,
        }
        .build();
        let leg = Arc::new(
            Cylinder {
                radius: 0.012 * l,
                half_height: 0.085 * l,
                resolution: 8,
                segments: 1,
                ..Cylinder::default()
            }
            .build(),
        );
        let meshes: [Arc<CpuMesh>; BIRD_PARTS] = [
            Arc::new(ellipsoid(Vec3::new(0.14 * l, 0.13 * l, 0.32 * l), 24, 16)),
            Arc::new(ellipsoid(Vec3::new(0.10 * l, 0.10 * l, 0.11 * l), 18, 12)),
            Arc::new(turned(beak, Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2))),
            Arc::new(ellipsoid(Vec3::splat(0.028 * l), 10, 8)),
            Arc::new(ellipsoid(Vec3::splat(0.028 * l), 10, 8)),
            Arc::new(ellipsoid(Vec3::splat(0.016 * l), 8, 6)),
            Arc::new(ellipsoid(Vec3::splat(0.016 * l), 8, 6)),
            Arc::new(wing_slab(w.inner, false)),
            Arc::new(wing_slab(w.outer, false)),
            Arc::new(wing_slab(w.inner, true)),
            Arc::new(wing_slab(w.outer, true)),
            Arc::new(tail_slab(0.33 * l, 0.08 * l, 0.2 * l, 0.012 * l)),
            Arc::clone(&leg),
            leg,
        ];
        let plumage = material(body.color, 0.85);
        let wings = material(body.wing_color, 0.8);
        let horn = material(horn, 0.55);
        let sclera = material([0.72, 0.66, 0.42], 0.3);
        let pupil = material([0.008, 0.008, 0.008], 0.12);
        let materials = [
            Arc::clone(&plumage),
            Arc::clone(&plumage),
            Arc::clone(&horn),
            Arc::clone(&sclera),
            sclera,
            Arc::clone(&pupil),
            pupil,
            Arc::clone(&wings),
            Arc::clone(&wings),
            Arc::clone(&wings),
            wings,
            plumage,
            Arc::clone(&horn),
            horn,
        ];
        Self {
            parts: meshes,
            materials,
        }
    }
}

/// Each part's world transform for `pose`, [`BirdPart::ALL`] order.
#[must_use]
pub fn pose_parts(body: &BirdBody, pose: &BirdPose) -> [Transform; BIRD_PARTS] {
    let l = body.length_m;
    let w = wing(body);
    let root_p = pose.position;
    let root_r = pose.rotation;
    let place = |local_p: Vec3, local_r: Quat, scale: Vec3| Transform {
        translation: root_p + root_r * local_p,
        rotation: root_r * local_r,
        scale,
    };
    let puff = 1.0 + 0.08 * pose.ruffle.clamp(0.0, 1.5);
    let squash = 1.0 - 0.12 * pose.crouch;
    let body_t = place(Vec3::ZERO, Quat::IDENTITY, Vec3::new(puff, puff * squash, puff));

    // Head: pivot at the neck, turned by yaw then pitch.
    let neck = Vec3::new(0.0, 0.1 * l, -0.24 * l);
    let head_r = Quat::from_rotation_y(pose.head_yaw) * Quat::from_rotation_x(pose.head_pitch);
    let in_head = |p: Vec3| neck + head_r * p;
    let head_t = place(in_head(Vec3::new(0.0, 0.0, -0.08 * l)), head_r, Vec3::ONE);
    let beak_t = place(in_head(Vec3::new(0.0, -0.01 * l, -0.17 * l)), head_r, Vec3::ONE);
    let eye = |side: f32| {
        let centre = Vec3::new(side * 0.075 * l, 0.03 * l, -0.12 * l);
        let out = Vec3::new(side, 0.15, -0.35).normalize();
        let s = pose.eye_scale.clamp(0.5, 2.0);
        let sclera = place(in_head(centre), head_r, Vec3::splat(s));
        // The pupil sits at the sclera's rim whatever its size.
        let pupil = place(in_head(centre + out * (0.028 * l * s * 0.72)), head_r, Vec3::ONE);
        (sclera, pupil)
    };
    let (eye_l, pupil_l) = eye(-1.0);
    let (eye_r, pupil_r) = eye(1.0);

    // Wings: closed (`wing_open` 0) they fold back along the body; open they spread, raised by
    // the shoulder angle, with the outer panel hinged at the wrist.
    let fold = (1.0 - pose.wing_open).clamp(0.0, 1.0);
    let sweep = pose.wing_sweep + fold * 85f32.to_radians();
    let wing_parts = |side: f32| {
        let shoulder = Vec3::new(side * w.shoulder.x, w.shoulder.y, w.shoulder.z);
        let inner_r = Quat::from_rotation_y(side * -sweep) * Quat::from_rotation_z(side * pose.wing_raise);
        let wrist = shoulder + inner_r * Vec3::new(side * w.inner.0, 0.0, w.inner.3);
        let outer_r = inner_r
            * Quat::from_rotation_y(side * -fold * 150f32.to_radians())
            * Quat::from_rotation_z(side * pose.wing_tip);
        (
            place(shoulder, inner_r, Vec3::ONE),
            place(wrist, outer_r, Vec3::ONE),
        )
    };
    let (inner_l, outer_l) = wing_parts(-1.0);
    let (inner_r, outer_r) = wing_parts(1.0);

    let spread = 0.55 + 0.45 * pose.tail_spread.clamp(0.0, 1.0);
    let tail = place(
        Vec3::new(0.0, 0.03 * l, 0.26 * l),
        Quat::from_rotation_x(0.08),
        Vec3::new(spread, 1.0, 1.0),
    );
    let legs = pose.legs.clamp(0.0, 1.0);
    let leg = |side: f32| {
        let y = -0.3 * l + 0.085 * l * legs + (1.0 - legs) * 0.12 * l;
        place(
            Vec3::new(side * 0.04 * l, y, 0.02 * l),
            Quat::IDENTITY,
            Vec3::new(1.0, legs.max(0.05), 1.0),
        )
    };
    [
        body_t,
        head_t,
        beak_t,
        eye_l,
        eye_r,
        pupil_l,
        pupil_r,
        inner_l,
        outer_l,
        inner_r,
        outer_r,
        tail,
        leg(-1.0),
        leg(1.0),
    ]
}
