// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/world/coords.rs, commit c9513623f), ADR-8012.

//! Sim → render handedness at the bridge boundary (`docs/specs/25-sim-bridge-and-do27.md` § 3,
//! ADR-8456).
//!
//! Regia Engine renders right-handed, +Y up, forward −Z, in metres (`docs/02-architecture.md` § 3).
//! A sim whose world is **left-handed** (+X east, +Y up, +Z north, so `X × Y = −Z`) draws
//! mirrored when its coordinates are placed unchanged: sim-right shows on screen-left. The fix is
//! one reflection across the YZ plane, applied once at the boundary:
//!
//! ```text
//! render = (−sim.x, sim.y, sim.z)
//! ```
//!
//! - positions, offsets, directions and velocities: negate X ([`pos`], [`dir`]);
//! - rotations: conjugate the quaternion, `(w, x, y, z) → (w, x, −y, −z)` ([`quat`]). The result is a
//!   proper rotation (determinant +1), never a negative scale, so a placed mesh is never drawn inside
//!   out;
//! - yaw about +Y: `θ → −θ` ([`yaw`]);
//! - the reflection is its own inverse, so picking and aiming rays go back with the same function
//!   ([`inv`]).
//!
//! [`SimHandedness`] is the bridge setting
//! ([`SimBridgeSettings::handedness`](crate::sim_blend::SimBridgeSettings::handedness)) that applies
//! it: [`SimHandedness::RightHanded`] (the default) changes nothing, bit for bit;
//! [`SimHandedness::LeftHandedReflectX`] reflects every snapshot pose and placed event once in the
//! bridge, so the sim's [`ExternalSim`](crate::sim_bridge::ExternalSim) implementation must not
//! convert again.
//!
//! Placed meshes (glTF and cooked meshes are right-handed already) are not mirrored: only their
//! placement goes through the conversion. A mesh the sim **authors** in its own coordinates and that
//! is drawn with an identity transform (baked world geometry) is mirrored itself with
//! [`SimHandedness::mirror_mesh`]: positions, normals and tangents are reflected, the tangent sign
//! flips, and every triangle's winding is reversed ([`mirror_mesh_winding`]) so its front face, back-face
//! culling and lighting stay right.
//!
//! Adapted from Direct Order 27's `shell-regia/game/src/world/coords.rs` (SHA c9513623f,
//! regia-absorb-4; see `docs/ported-sources.toml`): the fixed reflection became the
//! [`SimHandedness`] setting, the ±2 m seat clamp became a parameter ([`DEFAULT_SEAT_CLAMP`]), and the
//! game's lab vertex type became Regia's [`CpuMesh`].

use regia_asset::CpuMesh;
use regia_core::math::{Quat, Vec3};
use serde::{Deserialize, Serialize};

/// The seat correction clamp of [`seat_delta`] (metres): DO27's value.
pub const DEFAULT_SEAT_CLAMP: f32 = 2.0;

/// The handedness of the sim's world frame, as the bridge sees it.
///
/// Set once on [`SimBridgeSettings`](crate::sim_blend::SimBridgeSettings); the bridge applies it to
/// every snapshot pose and placed event before anything downstream reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum SimHandedness {
    /// The sim is right-handed like Regia Engine (+Y up, forward −Z): nothing is converted.
    #[default]
    RightHanded,
    /// The sim is left-handed (+X east, +Y up, +Z north): reflect X at the boundary.
    LeftHandedReflectX,
}

impl SimHandedness {
    /// Every setting, in protocol order.
    pub const ALL: [Self; 2] = [Self::RightHanded, Self::LeftHandedReflectX];

    /// The protocol name (`right_handed`, `left_handed_reflect_x`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RightHanded => "right_handed",
            Self::LeftHandedReflectX => "left_handed_reflect_x",
        }
    }

    /// Parses a protocol name ([`as_str`](Self::as_str)).
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|h| h.as_str() == name)
    }

    /// Whether this setting reflects (mirrors) the sim's frame.
    pub fn reflects(self) -> bool {
        matches!(self, Self::LeftHandedReflectX)
    }

    /// A sim position, offset or velocity in engine coordinates.
    #[inline]
    pub fn pos(self, v: Vec3) -> Vec3 {
        if self.reflects() { pos(v) } else { v }
    }

    /// A sim direction in engine coordinates (the same rule as [`pos`](Self::pos)).
    #[inline]
    pub fn dir(self, v: Vec3) -> Vec3 {
        self.pos(v)
    }

    /// A sim rotation in engine coordinates: a proper rotation, never a negative scale.
    #[inline]
    pub fn quat(self, q: Quat) -> Quat {
        if self.reflects() { quat(q) } else { q }
    }

    /// A sim yaw about +Y (radians) in engine coordinates.
    #[inline]
    pub fn yaw(self, radians: f32) -> f32 {
        if self.reflects() { yaw(radians) } else { radians }
    }

    /// An engine position back in sim coordinates (picking and aiming rays). Every setting is its
    /// own inverse.
    #[inline]
    pub fn inv(self, v: Vec3) -> Vec3 {
        self.pos(v)
    }

    /// A sim pose (translation, rotation) in engine coordinates.
    #[inline]
    pub fn pose(self, translation: Vec3, rotation: Quat) -> (Vec3, Quat) {
        (self.pos(translation), self.quat(rotation))
    }

    /// Mirror a mesh the sim authored in its own coordinates (drawn with an identity transform)
    /// into engine coordinates, winding included. A no-op for [`SimHandedness::RightHanded`].
    /// Placed meshes never need this: their placement is converted instead.
    pub fn mirror_mesh(self, mesh: &mut CpuMesh) {
        if self.reflects() {
            mirror_cpu_mesh_x(mesh);
        }
    }
}

/// Reflect a position, offset, direction or velocity across the mirrored X axis.
#[inline]
pub fn pos(v: Vec3) -> Vec3 {
    Vec3::new(-v.x, v.y, v.z)
}

/// Reflect a direction or velocity (same rule as [`pos`]; no normalisation).
#[inline]
pub fn dir(v: Vec3) -> Vec3 {
    pos(v)
}

/// Reflect a rotation across the mirrored X axis by conjugating the quaternion:
/// `(w, x, y, z) → (w, x, −y, −z)`. The result is a proper rotation (det +1), so a placed mesh is
/// never negatively scaled or drawn inside out.
#[inline]
pub fn quat(q: Quat) -> Quat {
    Quat::from_xyzw(q.x, -q.y, -q.z, q.w)
}

/// Reflect a yaw (rotation about +Y): `θ → −θ`.
#[inline]
pub fn yaw(radians: f32) -> f32 {
    -radians
}

/// Render → sim inverse. The reflection is its own inverse.
#[inline]
pub fn inv(v: Vec3) -> Vec3 {
    pos(v)
}

/// Sim XZ into the drawn terrain's XZ (`render.x = −sim.x`), for a heightfield stored already
/// mirrored so it lines up with [`pos`].
#[inline]
pub fn rendered_xz(sim_x: f32, sim_z: f32) -> [f32; 2] {
    [-sim_x, sim_z]
}

/// One [`rendered_xz`] per sim XZ.
pub fn rendered_points(sim_xz: &[[f32; 2]]) -> Vec<[f32; 2]> {
    sim_xz.iter().map(|p| rendered_xz(p[0], p[1])).collect()
}

/// `rendered − sim` seat correction, clamped to `±clamp` metres ([`DEFAULT_SEAT_CLAMP`] is DO27's
/// value). A non-finite rendered height is a terrain hole (no drawn ground there): leave the sim
/// height (0). A non-finite or negative `clamp` is treated as 0.
#[inline]
pub fn seat_delta(rendered_h: f32, sim_h: f32, clamp: f32) -> f32 {
    let limit = if clamp.is_finite() { clamp.max(0.0) } else { 0.0 };
    let d = rendered_h - sim_h;
    if d.is_finite() {
        d.clamp(-limit, limit)
    } else {
        0.0
    }
}

/// Seat correction at each sim XZ. `rendered_h[i]` was sampled at [`rendered_points`], not at
/// `sim_xz[i]`; `sim_ground` is the sim's own ground height at a sim XZ (`None` uses the rendered
/// height, so no correction).
pub fn seat_deltas(
    sim_xz: &[[f32; 2]],
    rendered_h: &[f32],
    clamp: f32,
    mut sim_ground: impl FnMut(f32, f32) -> Option<f32>,
) -> Vec<f32> {
    sim_xz
        .iter()
        .zip(rendered_h.iter())
        .map(|(p, r)| {
            let sim = sim_ground(p[0], p[1]).unwrap_or(*r);
            seat_delta(*r, sim, clamp)
        })
        .collect()
}

/// Reverse every triangle's winding (`a, b, c → a, c, b`) in a triangle list. A trailing partial
/// triangle (fewer than three indices) is left alone.
///
/// A reflection turns counter-clockwise triangles clockwise; after [`mirror_mesh_x`] this flip makes
/// them front-facing again for back-face culling.
pub fn mirror_mesh_winding(indices: &mut [u32]) {
    for tri in indices.as_chunks_mut::<3>().0 {
        tri.swap(1, 2);
    }
}

/// Mirror mesh attributes across X in place: negate position, normal and tangent X, flip the
/// tangent sign (w, so the bitangent `w · (n × t)` is reflected too), and reverse the winding.
/// `normals` and `tangents` may be empty.
pub fn mirror_mesh_x(
    positions: &mut [[f32; 3]],
    normals: &mut [[f32; 3]],
    tangents: &mut [[f32; 4]],
    indices: &mut [u32],
) {
    for p in positions.iter_mut() {
        p[0] = -p[0];
    }
    for n in normals.iter_mut() {
        n[0] = -n[0];
    }
    for t in tangents.iter_mut() {
        t[0] = -t[0];
        t[3] = -t[3];
    }
    mirror_mesh_winding(indices);
}

/// [`mirror_mesh_x`] over a [`CpuMesh`]. Morph target deltas are reflected too; UVs, colours and
/// skin weights do not change.
pub fn mirror_cpu_mesh_x(mesh: &mut CpuMesh) {
    mirror_mesh_x(
        &mut mesh.positions,
        &mut mesh.normals,
        &mut mesh.tangents,
        &mut mesh.indices,
    );
    for target in &mut mesh.morph_targets {
        let deltas = target
            .position_deltas
            .iter_mut()
            .chain(target.normal_deltas.iter_mut())
            .chain(target.tangent_deltas.iter_mut());
        for d in deltas {
            d[0] = -d[0];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Ported from DO27 `world/coords.rs` (assertions kept) ---

    #[test]
    fn pos_is_an_involution_and_flips_only_x() {
        let v = Vec3::new(3.0, -2.0, 7.0);
        let m = pos(v);
        assert_eq!(m, Vec3::new(-3.0, -2.0, 7.0));
        assert_eq!(pos(m), v);
        assert_eq!(inv(m), v);
    }

    #[test]
    fn quat_is_an_involution() {
        let q = Quat::from_xyzw(0.1, 0.2, 0.3, 0.9).normalize();
        let c = quat(q);
        // The conjugated rotation is a proper rotation of the same angle.
        assert!((c.length() - 1.0).abs() < 1.0e-5);
        assert!((quat(c) - q).length() < 1.0e-5);
    }

    #[test]
    fn yaw_sign_flips_and_round_trips() {
        assert!((yaw(0.7) - (-0.7)).abs() < 1.0e-6);
        assert!((yaw(yaw(0.7)) - 0.7).abs() < 1.0e-6);
    }

    #[test]
    fn rendered_xz_negates_only_x() {
        assert_eq!(rendered_xz(12.0, -4.0), [-12.0, -4.0]);
        assert_eq!(
            rendered_points(&[[1.0, 2.0], [-3.0, 4.0]]),
            vec![[-1.0, 2.0], [3.0, 4.0]]
        );
    }

    #[test]
    fn seat_delta_clamps_and_ignores_holes() {
        let c = DEFAULT_SEAT_CLAMP;
        assert!((seat_delta(10.0, 9.8, c) - 0.2).abs() < 1.0e-5);
        assert_eq!(seat_delta(100.0, 0.0, c), 2.0);
        assert_eq!(seat_delta(-100.0, 0.0, c), -2.0);
        assert_eq!(seat_delta(f32::NAN, 3.0, c), 0.0);
        let deltas = seat_deltas(&[[4.0, 5.0]], &[f32::NAN], c, |_, _| Some(1.0));
        assert_eq!(deltas, vec![0.0]);
    }

    // --- Regia additions (ADR-8456) ---

    #[test]
    fn seat_clamp_is_a_setting() {
        assert_eq!(seat_delta(100.0, 0.0, 0.5), 0.5);
        assert_eq!(seat_delta(-100.0, 0.0, 0.5), -0.5);
        assert_eq!(seat_delta(1.0, 0.0, f32::NAN), 0.0);
        assert_eq!(seat_delta(1.0, 0.0, -3.0), 0.0);
    }

    /// The geometric normal of triangle `t` from its winding (counter-clockwise front face).
    fn face_normal(positions: &[[f32; 3]], t: [u32; 3]) -> Vec3 {
        let p = |i: u32| Vec3::from_array(positions[i as usize]);
        (p(t[1]) - p(t[0])).cross(p(t[2]) - p(t[0]))
    }

    /// A tilted triangle whose winding agrees with its vertex normals (x component non-zero, so the
    /// reflection really moves it).
    fn tilted_triangle() -> CpuMesh {
        let positions = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 1.0], [0.0, 1.0, 0.0]];
        let n = face_normal(&positions, [0, 1, 2]).normalize();
        assert!(n.x.abs() > 0.1, "the test triangle must lean in x: {n:?}");
        CpuMesh {
            normals: vec![n.to_array(); 3],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            tangents: vec![[1.0, 0.0, 0.0, 1.0]; 3],
            positions,
            indices: vec![0, 1, 2],
            ..Default::default()
        }
    }

    fn winding_agrees_with_normals(mesh: &CpuMesh) -> bool {
        mesh.indices.as_chunks::<3>().0.iter().all(|tri| {
            let face = face_normal(&mesh.positions, *tri);
            tri.iter()
                .all(|&i| face.dot(Vec3::from_array(mesh.normals[i as usize])) > 0.0)
        })
    }

    #[test]
    fn x_reflection_flips_winding() {
        let base = tilted_triangle();
        assert!(winding_agrees_with_normals(&base));

        // Reflecting the attributes alone turns the triangle clockwise: its winding now faces away
        // from its own normals, so back-face culling would drop the visible side.
        let mut attrs_only = base.clone();
        for p in &mut attrs_only.positions {
            p[0] = -p[0];
        }
        for n in &mut attrs_only.normals {
            n[0] = -n[0];
        }
        assert!(!winding_agrees_with_normals(&attrs_only));

        // The setting mirrors the mesh with the winding flip: front-facing again.
        let mut mirrored = base.clone();
        SimHandedness::LeftHandedReflectX.mirror_mesh(&mut mirrored);
        assert!(winding_agrees_with_normals(&mirrored));
        assert_eq!(mirrored.indices, vec![0, 2, 1]);
        for (m, b) in mirrored.positions.iter().zip(&base.positions) {
            assert_eq!(*m, [-b[0], b[1], b[2]]);
        }
        // Tangent X and its sign flip, so the bitangent is reflected with the frame.
        assert_eq!(mirrored.tangents[0], [-1.0, 0.0, 0.0, -1.0]);
        // UVs are untouched.
        assert_eq!(mirrored.uvs, base.uvs);
        // The mirrored normals equal the reflected placement rule (`dir`).
        for (m, b) in mirrored.normals.iter().zip(&base.normals) {
            assert_eq!(Vec3::from_array(*m), dir(Vec3::from_array(*b)));
        }

        // Right-handed changes nothing.
        let mut same = base.clone();
        SimHandedness::RightHanded.mirror_mesh(&mut same);
        assert_eq!(same, base);

        // Mirroring twice is the identity (the reflection is an involution).
        SimHandedness::LeftHandedReflectX.mirror_mesh(&mut mirrored);
        assert_eq!(mirrored, base);
    }

    #[test]
    fn winding_flip_leaves_a_partial_triangle() {
        let mut indices = vec![0, 1, 2, 3, 4, 5, 6, 7];
        mirror_mesh_winding(&mut indices);
        assert_eq!(indices, vec![0, 2, 1, 3, 5, 4, 6, 7]);
    }

    #[test]
    fn reflection_round_trips_a_pose() {
        let h = SimHandedness::LeftHandedReflectX;
        let t = Vec3::new(12.5, -3.25, 40.0);
        let q = Quat::from_euler(regia_core::math::EulerRot::YXZ, 0.9, -0.3, 0.2);
        let (rt, rq) = h.pose(t, q);
        assert_eq!(rt, Vec3::new(-12.5, -3.25, 40.0));
        // A proper rotation: unit length, and it maps the reflected frame consistently
        // (R' = M R M with M = diag(-1, 1, 1)).
        assert!((rq.length() - 1.0).abs() < 1.0e-6);
        for axis in [Vec3::X, Vec3::Y, Vec3::Z, Vec3::new(0.3, -0.7, 0.2)] {
            let expect = pos(q * pos(axis));
            assert!((rq * axis - expect).length() < 1.0e-5, "{axis:?}");
        }
        // Back to the sim: bit-equal.
        let (bt, bq) = h.pose(rt, rq);
        assert_eq!(bt.to_array().map(f32::to_bits), t.to_array().map(f32::to_bits));
        assert_eq!(bq.to_array().map(f32::to_bits), q.to_array().map(f32::to_bits));
        assert_eq!(h.inv(rt), t);
        // A yaw-only rotation reflects to the negated yaw.
        let yawed = Quat::from_rotation_y(0.6);
        assert!((h.quat(yawed) - Quat::from_rotation_y(h.yaw(0.6))).length() < 1.0e-6);
    }

    #[test]
    fn right_handed_conversions_are_bit_identical() {
        let h = SimHandedness::RightHanded;
        let t = Vec3::new(-0.0, f32::MIN_POSITIVE, 1.0e30);
        let q = Quat::from_xyzw(-0.0, 0.6, -0.0, 0.8);
        let (rt, rq) = h.pose(t, q);
        assert_eq!(rt.to_array().map(f32::to_bits), t.to_array().map(f32::to_bits));
        assert_eq!(rq.to_array().map(f32::to_bits), q.to_array().map(f32::to_bits));
        assert_eq!(h.yaw(-0.0).to_bits(), (-0.0f32).to_bits());
        assert_eq!(
            h.dir(t).to_array().map(f32::to_bits),
            t.to_array().map(f32::to_bits)
        );
    }

    #[test]
    fn handedness_names_round_trip() {
        for h in SimHandedness::ALL {
            assert_eq!(SimHandedness::parse(h.as_str()), Some(h));
        }
        assert_eq!(SimHandedness::parse("left"), None);
        assert_eq!(SimHandedness::default(), SimHandedness::RightHanded);
    }
}
