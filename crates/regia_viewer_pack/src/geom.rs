// SPDX-License-Identifier: MPL-2.0
//! Generic geometry: the stand-in parts (post, ball, prong head, bezel cup, ring, chain link, end
//! base) and gem cuts built from published proportions, plus the convex solid of a facet list.
//!
//! Units are mm. Every part sits with its front toward +Z and its gem seat (if any) at the origin,
//! so the recipe places a gem at `at.origin_mm` on top of it. Parts hidden behind the piece (posts,
//! pins) take few segments: they are seen only at the edge of the tilt cone.

use glam::{Vec2, Vec3};

use crate::mesh::MeshData;

/// One facet of a convex gem: the half-space `normal . x <= offset_mm` (unit normal, mm).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Facet {
    /// Outward unit normal.
    pub normal: [f32; 3],
    /// Plane offset from the origin, mm.
    pub offset_mm: f32,
    /// What the facet is (`table`, `crown`, `girdle`, `pavilion`); optional in a kit.
    #[serde(default)]
    pub kind: String,
}

/// Published proportions of a faceted stone, as fractions of the width and angles in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Proportions {
    /// Table width over the stone's width.
    pub table: f32,
    /// Crown angle to the girdle plane.
    pub crown_deg: f32,
    /// Pavilion angle to the girdle plane.
    pub pavilion_deg: f32,
    /// Girdle thickness over the stone's width.
    pub girdle: f32,
}

/// Tolkowsky's round brilliant (1919): table 53 %, crown 34.5 deg, pavilion 40.75 deg; a thin
/// girdle of 1.5 % (the common modern range is 1 to 3 %).
pub const TOLKOWSKY: Proportions = Proportions {
    table: 0.53,
    crown_deg: 34.5,
    pavilion_deg: 40.75,
    girdle: 0.015,
};

/// Fancy shapes (marquise, pear, oval, princess) take the round's angles with a wider table, as
/// published grading guides describe; good enough for a stand-in.
pub const FANCY: Proportions = Proportions {
    table: 0.58,
    crown_deg: 34.0,
    pavilion_deg: 41.0,
    girdle: 0.02,
};

/// A gem outline: a convex polygon in the girdle plane, counter-clockwise.
pub fn outline(cut_type: &str, length_mm: f32, width_mm: f32) -> Option<Vec<Vec2>> {
    let (l, w) = (length_mm.max(0.01), width_mm.max(0.01));
    let n = 32;
    let pts: Vec<Vec2> = match cut_type {
        "round_brilliant" | "round" => (0..n)
            .map(|i| {
                let a = i as f32 / n as f32 * std::f32::consts::TAU;
                Vec2::new(a.cos(), a.sin()) * (w * 0.5)
            })
            .collect(),
        "oval" => (0..n)
            .map(|i| {
                let a = i as f32 / n as f32 * std::f32::consts::TAU;
                Vec2::new(a.cos() * l * 0.5, a.sin() * w * 0.5)
            })
            .collect(),
        "marquise" => {
            // Two circular arcs meeting at the tips: the arc through (+-l/2, 0) and (0, +-w/2).
            let (a, b) = (l * 0.5, w * 0.5);
            let r = (a * a + b * b) / (2.0 * b);
            let half = (a / r).asin();
            let mut v = Vec::new();
            for side in [1.0f32, -1.0] {
                for i in 0..n / 2 {
                    let t = -half + 2.0 * half * i as f32 / (n / 2) as f32;
                    let t = if side > 0.0 { t } else { -t };
                    v.push(Vec2::new(r * t.sin(), side * (r * t.cos() - (r - b))));
                }
            }
            // Built top arc left to right, then bottom right to left: clockwise; flip it.
            v.reverse();
            v
        }
        "pear" => {
            // A half circle of radius w/2 at one end, tangent lines to a point at the other.
            // When the stone is wider than it is long the tip sits inside the bulb and that
            // construction is undefined (acos of a value above 1). An ellipse of the requested
            // size stays finite; length stays on +x in this helper.
            let rr = w * 0.5;
            let c = Vec2::new(-(l * 0.5 - rr), 0.0);
            let tip = Vec2::new(l * 0.5, 0.0);
            let d = (tip - c).length();
            if !d.is_finite() || d < rr {
                (0..n)
                    .map(|i| {
                        let a = i as f32 / n as f32 * std::f32::consts::TAU;
                        Vec2::new(a.cos() * l * 0.5, a.sin() * w * 0.5)
                    })
                    .collect()
            } else {
                let tangent = (rr / d).acos();
                let mut v = vec![tip];
                for i in 0..=n {
                    let a = tangent + (std::f32::consts::TAU - 2.0 * tangent) * i as f32 / n as f32;
                    v.push(c + Vec2::new(a.cos(), a.sin()) * rr);
                }
                v
            }
        }
        "princess" | "square" => {
            let (a, b) = (l * 0.5, w * 0.5);
            vec![
                Vec2::new(-a, -b),
                Vec2::new(a, -b),
                Vec2::new(a, b),
                Vec2::new(-a, b),
            ]
        }
        _ => return None,
    };
    Some(pts)
}

/// Facets of a stand-in cut: a table, one crown and one pavilion facet per outline edge, and a
/// vertical girdle facet per edge. This is a simplified facet set with the right table, crown and
/// pavilion angles (not the full 57 facets of a round brilliant), enough for a cheap faceted look.
pub fn cut_facets(cut_type: &str, length_mm: f32, width_mm: f32) -> Option<Vec<Facet>> {
    let pts = outline(cut_type, length_mm, width_mm)?;
    let p = if matches!(cut_type, "round_brilliant" | "round") {
        TOLKOWSKY
    } else {
        FANCY
    };
    let w = width_mm.max(0.01);
    let g = p.girdle * w;
    let (cb, sb) = (p.crown_deg.to_radians().cos(), p.crown_deg.to_radians().sin());
    let (ca, sa) = (
        p.pavilion_deg.to_radians().cos(),
        p.pavilion_deg.to_radians().sin(),
    );
    let crown_h = (1.0 - p.table) * 0.5 * w * p.crown_deg.to_radians().tan();
    let mut f = vec![Facet {
        normal: [0.0, 0.0, 1.0],
        offset_mm: g * 0.5 + crown_h,
        kind: "table".into(),
    }];
    let n = pts.len();
    for i in 0..n {
        let (a, b) = (pts[i], pts[(i + 1) % n]);
        let e = b - a;
        if e.length() < 1e-6 {
            continue;
        }
        let out = Vec2::new(e.y, -e.x).normalize();
        let mid = (a + b) * 0.5;
        let d = out.dot(mid);
        f.push(Facet {
            normal: [out.x, out.y, 0.0],
            offset_mm: d,
            kind: "girdle".into(),
        });
        let crown = Vec3::new(out.x * sb, out.y * sb, cb);
        f.push(Facet {
            normal: crown.to_array(),
            offset_mm: crown.dot(Vec3::new(mid.x, mid.y, g * 0.5)),
            kind: "crown".into(),
        });
        let pav = Vec3::new(out.x * sa, out.y * sa, -ca);
        f.push(Facet {
            normal: pav.to_array(),
            offset_mm: pav.dot(Vec3::new(mid.x, mid.y, -g * 0.5)),
            kind: "pavilion".into(),
        });
    }
    Some(f)
}

fn plane_basis(n: Vec3) -> (Vec3, Vec3) {
    let seed = if n.z.abs() < 0.9 { Vec3::Z } else { Vec3::X };
    let u = seed.cross(n).normalize();
    (u, n.cross(u))
}

/// Clip a convex polygon by the half-space `n . x <= d` (Sutherland and Hodgman 1974).
fn clip(poly: &[Vec3], n: Vec3, d: f32) -> Vec<Vec3> {
    let mut out = Vec::with_capacity(poly.len() + 1);
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        let (da, db) = (n.dot(a) - d, n.dot(b) - d);
        if da <= 0.0 {
            out.push(a);
        }
        if (da < 0.0 && db > 0.0) || (da > 0.0 && db < 0.0) {
            out.push(a + (b - a) * (da / (da - db)));
        }
    }
    out
}

/// The convex solid of a facet list as a flat-shaded mesh (one normal per facet, so a gem shader
/// sees sharp facets). Each facet's polygon is a large square on its plane clipped by every other
/// facet. Facets that end up empty (redundant planes) are skipped. `None` when the facets do not
/// bound a solid within `extent_mm`.
pub fn convex_mesh(facets: &[Facet], extent_mm: f32, name: &str, slot: &str) -> Option<MeshData> {
    let big = extent_mm.max(1e-3) * 4.0;
    let mut mesh = MeshData {
        name: name.into(),
        material_slot: slot.into(),
        ..MeshData::default()
    };
    for (i, f) in facets.iter().enumerate() {
        let n = Vec3::from(f.normal).normalize_or_zero();
        if n == Vec3::ZERO || !f.offset_mm.is_finite() {
            return None;
        }
        let (u, v) = plane_basis(n);
        let c = n * f.offset_mm;
        let mut poly = vec![
            c + (-u - v) * big,
            c + (u - v) * big,
            c + (u + v) * big,
            c + (-u + v) * big,
        ];
        for (j, g) in facets.iter().enumerate() {
            if i != j {
                poly = clip(&poly, Vec3::from(g.normal).normalize_or_zero(), g.offset_mm);
                if poly.len() < 3 {
                    break;
                }
            }
        }
        if poly.len() < 3 {
            continue;
        }
        if poly.iter().any(|p| p.abs().max_element() > big * 0.5) {
            return None;
        }
        let base = mesh.positions.len() as u32;
        for p in &poly {
            mesh.positions.push(p.to_array());
            mesh.normals.push(n.to_array());
        }
        // Counter-clockwise seen from outside (the square was wound around +n).
        for k in 1..poly.len() as u32 - 1 {
            mesh.indices.extend_from_slice(&[base, base + k, base + k + 1]);
        }
    }
    (!mesh.indices.is_empty()).then_some(mesh)
}

/// Builder for smooth-shaded parts.
#[derive(Default)]
struct Build {
    m: MeshData,
}

impl Build {
    fn new(name: &str, slot: &str) -> Self {
        Build {
            m: MeshData {
                name: name.into(),
                material_slot: slot.into(),
                ..MeshData::default()
            },
        }
    }

    fn vert(&mut self, p: Vec3, n: Vec3) -> u32 {
        self.m.positions.push(p.to_array());
        self.m.normals.push(n.normalize_or(Vec3::Z).to_array());
        self.m.positions.len() as u32 - 1
    }

    /// A surface of revolution about +Z from a profile of (radius, z, normal-in-rz) samples.
    fn lathe(&mut self, profile: &[(f32, f32, Vec2)], segments: u32) {
        let segs = segments.max(3);
        let base = self.m.positions.len() as u32;
        for s in 0..=segs {
            let a = s as f32 / segs as f32 * std::f32::consts::TAU;
            let (c, sn) = (a.cos(), a.sin());
            for &(r, z, nrz) in profile {
                self.vert(
                    Vec3::new(r * c, r * sn, z),
                    Vec3::new(nrz.x * c, nrz.x * sn, nrz.y),
                );
            }
        }
        let rows = profile.len() as u32;
        for s in 0..segs {
            for k in 0..rows - 1 {
                let a = base + s * rows + k;
                let b = a + rows;
                self.m.indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
            }
        }
    }

    /// A tube about the axis from `a` to `b` with radius `r`, closed with flat caps.
    fn cylinder(&mut self, a: Vec3, b: Vec3, r: f32, segments: u32) {
        let axis = (b - a).normalize_or(Vec3::Z);
        let (u, v) = plane_basis(axis);
        let segs = segments.max(3);
        let base = self.m.positions.len() as u32;
        for s in 0..=segs {
            let t = s as f32 / segs as f32 * std::f32::consts::TAU;
            let dir = u * t.cos() + v * t.sin();
            self.vert(a + dir * r, dir);
            self.vert(b + dir * r, dir);
        }
        for s in 0..segs {
            let i = base + s * 2;
            self.m
                .indices
                .extend_from_slice(&[i, i + 2, i + 1, i + 1, i + 2, i + 3]);
        }
        for (end, n) in [(a, -axis), (b, axis)] {
            let c = self.vert(end, n);
            let ring: Vec<u32> = (0..segs)
                .map(|s| {
                    let t = s as f32 / segs as f32 * std::f32::consts::TAU;
                    self.vert(end + (u * t.cos() + v * t.sin()) * r, n)
                })
                .collect();
            for s in 0..segs as usize {
                let (p, q) = (ring[s], ring[(s + 1) % segs as usize]);
                if n == axis {
                    self.m.indices.extend_from_slice(&[c, p, q]);
                } else {
                    self.m.indices.extend_from_slice(&[c, q, p]);
                }
            }
        }
    }

    /// A tube swept along a closed planar path (in the XY plane at height `z`).
    fn sweep_closed(&mut self, path: &[Vec2], z: f32, r: f32, segments: u32) {
        let segs = segments.max(3);
        let n = path.len();
        let base = self.m.positions.len() as u32;
        for i in 0..n {
            let (p, prev, next) = (path[i], path[(i + n - 1) % n], path[(i + 1) % n]);
            let t = (next - prev).normalize_or(Vec2::X);
            let out = Vec3::new(t.y, -t.x, 0.0);
            for s in 0..segs {
                let a = s as f32 / segs as f32 * std::f32::consts::TAU;
                let dir = out * a.cos() + Vec3::Z * a.sin();
                self.vert(Vec3::new(p.x, p.y, z) + dir * r, dir);
            }
        }
        for i in 0..n as u32 {
            let j = (i + 1) % n as u32;
            for s in 0..segs {
                let s1 = (s + 1) % segs;
                let (a, b, c, d) = (i * segs + s, j * segs + s, i * segs + s1, j * segs + s1);
                self.m.indices.extend_from_slice(&[
                    base + a,
                    base + b,
                    base + c,
                    base + c,
                    base + b,
                    base + d,
                ]);
            }
        }
    }

    fn done(self) -> MeshData {
        self.m
    }
}

/// How finely a part is tessellated: visible fronts get `Fine`, hidden backs `Cheap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detail {
    /// About 48 segments around.
    Fine,
    /// About 12 segments around (posts and pins behind the piece).
    Cheap,
}

impl Detail {
    fn segments(self) -> u32 {
        match self {
            Detail::Fine => 48,
            Detail::Cheap => 12,
        }
    }
}

/// A post or threadless pin: a cylinder of `diameter_mm` running `length_mm` back along -Z.
pub fn post(name: &str, diameter_mm: f32, length_mm: f32, detail: Detail) -> MeshData {
    let mut b = Build::new(name, "body");
    b.cylinder(
        Vec3::ZERO,
        Vec3::new(0.0, 0.0, -length_mm),
        diameter_mm * 0.5,
        detail.segments(),
    );
    b.done()
}

/// A ball (bead) of `diameter_mm`, centerd on the origin.
pub fn ball(name: &str, diameter_mm: f32, detail: Detail) -> MeshData {
    let r = diameter_mm * 0.5;
    let rings = detail.segments() / 2;
    let profile: Vec<(f32, f32, Vec2)> = (0..=rings)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * i as f32 / rings as f32;
            (r * a.cos(), r * a.sin(), Vec2::new(a.cos(), a.sin()))
        })
        .collect();
    let mut b = Build::new(name, "body");
    b.lathe(&profile, detail.segments());
    b.done()
}

/// A threadless end base: a short disc behind the setting (front face at z = 0).
pub fn end_base(name: &str, diameter_mm: f32, thickness_mm: f32, detail: Detail) -> MeshData {
    let mut b = Build::new(name, "body");
    b.cylinder(
        Vec3::new(0.0, 0.0, -thickness_mm),
        Vec3::ZERO,
        diameter_mm * 0.5,
        detail.segments(),
    );
    b.done()
}

/// A bezel cup for a gem outline: a wall around the girdle, open at the front, with a floor.
/// `outline` is the gem's outline (counter-clockwise); the wall stands `lip_mm` above the girdle
/// (z = 0) and the cup reaches `depth_mm` behind it. Closed and wound outward.
pub fn bezel_cup(
    name: &str,
    outline: &[Vec2],
    wall_mm: f32,
    depth_mm: f32,
    lip_mm: f32,
    detail: Detail,
) -> MeshData {
    #[derive(Clone, Copy)]
    enum N {
        Out,
        In,
        Down,
        // Across the rim's arc: this much outward and this much up.
        Rim(f32, f32),
    }
    let floor = (depth_mm * 0.15).max(0.05);
    let inner_z = -depth_mm + floor;
    // (offset from the outline, z, normal): outer wall, bottom, inner wall, rim.
    let mut rows = vec![
        (wall_mm, lip_mm, N::Out),
        (wall_mm, -depth_mm, N::Out),
        (wall_mm, -depth_mm, N::Down),
        (0.0, -depth_mm, N::Down),
        (-0.04, inner_z, N::In),
        (-0.04, lip_mm, N::In),
    ];
    // The rim is rounded, as a polished bezel's is: a flat top has one normal, so it reflects one direction of
    // the studio and reads as paper, not metal.
    let arc_steps = match detail {
        Detail::Cheap => 2,
        Detail::Fine => 6,
    };
    let (mid, radius) = ((wall_mm - 0.04) * 0.5, (wall_mm + 0.04) * 0.5);
    for i in 0..=arc_steps {
        let a = std::f32::consts::PI * (1.0 - i as f32 / arc_steps as f32);
        let (sin, cos) = a.sin_cos();
        rows.push((mid + radius * cos, lip_mm + radius * 0.7 * sin, N::Rim(cos, sin)));
    }
    let mut b = Build::new(name, "body");
    let n = outline.len();
    let frame = |k: usize| {
        let t = (outline[(k + 1) % n] - outline[(k + n - 1) % n]).normalize_or(Vec2::X);
        (outline[k], Vec2::new(t.y, -t.x))
    };
    let base = b.m.positions.len() as u32;
    for k in 0..n {
        let (p, out) = frame(k);
        for &(by, z, nk) in &rows {
            let q = p + out * by;
            let o3 = Vec3::new(out.x, out.y, 0.0);
            let nrm = match nk {
                N::Out => o3,
                N::In => -o3,
                N::Down => -Vec3::Z,
                N::Rim(outward, up) => (o3 * outward + Vec3::Z * up).normalize_or(Vec3::Z),
            };
            b.vert(Vec3::new(q.x, q.y, z), nrm);
        }
    }
    let rc = rows.len() as u32;
    for k in 0..n as u32 {
        let j = (k + 1) % n as u32;
        // Band 3 -> 4 (the floor's edge) is where the wall ring and the floor disc meet: inside
        // the solid, so neither side draws it.
        for r in (0..rc - 1).filter(|&r| r != 3) {
            let (a, bb, c, d) = (
                base + k * rc + r,
                base + j * rc + r,
                base + k * rc + r + 1,
                base + j * rc + r + 1,
            );
            b.m.indices.extend_from_slice(&[a, c, bb, c, d, bb]);
        }
    }
    // The bottom (facing back) and the floor inside the cup (facing the front).
    for (z, nrm, offset) in [(-depth_mm, -Vec3::Z, 0.0), (inner_z, Vec3::Z, -0.04)] {
        let c = b.vert(Vec3::new(0.0, 0.0, z), nrm);
        let ring: Vec<u32> = (0..n)
            .map(|k| {
                let (p, out) = frame(k);
                let q = p + out * offset;
                b.vert(Vec3::new(q.x, q.y, z), nrm)
            })
            .collect();
        for k in 0..n {
            let (p, q) = (ring[k], ring[(k + 1) % n]);
            if nrm.z > 0.0 {
                b.m.indices.extend_from_slice(&[c, p, q]);
            } else {
                b.m.indices.extend_from_slice(&[c, q, p]);
            }
        }
    }
    b.done()
}

/// A prong head: `count` prongs leaning in over a gem of `gem_diameter_mm` seated at the origin,
/// on a ring gallery below the girdle.
pub fn prong_head(name: &str, gem_diameter_mm: f32, count: u32, detail: Detail) -> MeshData {
    let r = gem_diameter_mm * 0.5;
    let wire = (gem_diameter_mm * 0.12).max(0.25);
    let mut b = Build::new(name, "body");
    let segs = (detail.segments() / 4).max(6);
    for k in 0..count.max(3) {
        let a = k as f32 / count.max(3) as f32 * std::f32::consts::TAU + std::f32::consts::FRAC_PI_4;
        let d = Vec3::new(a.cos(), a.sin(), 0.0);
        let foot = d * (r * 0.55) + Vec3::new(0.0, 0.0, -r * 0.9);
        let tip = d * (r * 0.92) + Vec3::new(0.0, 0.0, r * 0.25);
        b.cylinder(foot, tip, wire * 0.5, segs);
    }
    let ring: Vec<Vec2> = (0..detail.segments())
        .map(|i| {
            let a = i as f32 / detail.segments() as f32 * std::f32::consts::TAU;
            Vec2::new(a.cos(), a.sin()) * (r * 0.6)
        })
        .collect();
    b.sweep_closed(&ring, -r * 0.6, wire * 0.45, segs);
    b.done()
}

/// A plain ring (hoop) of inner diameter `inner_mm` and wire `wire_mm`, in the XY plane.
pub fn ring(name: &str, inner_mm: f32, wire_mm: f32, detail: Detail) -> MeshData {
    let rr = (inner_mm + wire_mm) * 0.5;
    let path: Vec<Vec2> = (0..detail.segments() * 2)
        .map(|i| {
            let a = i as f32 / (detail.segments() * 2) as f32 * std::f32::consts::TAU;
            Vec2::new(a.cos(), a.sin()) * rr
        })
        .collect();
    let mut b = Build::new(name, "body");
    b.sweep_closed(&path, 0.0, wire_mm * 0.5, detail.segments() / 2);
    b.done()
}

/// A chain link: a stadium (two half circles and two straights) of outer length `length_mm`,
/// width `width_mm` and wire `wire_mm`.
pub fn chain_link(name: &str, length_mm: f32, width_mm: f32, wire_mm: f32, detail: Detail) -> MeshData {
    let r = (width_mm - wire_mm) * 0.5;
    let half = ((length_mm - width_mm) * 0.5).max(0.0);
    let n = detail.segments() / 2;
    let mut path = Vec::new();
    for (cx, a0) in [
        (half, -std::f32::consts::FRAC_PI_2),
        (-half, std::f32::consts::FRAC_PI_2),
    ] {
        for i in 0..=n {
            let a = a0 + std::f32::consts::PI * i as f32 / n as f32;
            path.push(Vec2::new(cx + r * a.cos(), r * a.sin()));
        }
    }
    let mut b = Build::new(name, "body");
    b.sweep_closed(&path, 0.0, wire_mm * 0.5, detail.segments() / 3);
    b.done()
}

/// Move and scale a mesh: `p' = p * scale + offset`.
pub fn transform(mesh: &mut MeshData, scale: Vec3, offset: Vec3) {
    for p in &mut mesh.positions {
        *p = (Vec3::from(*p) * scale + offset).to_array();
    }
    if scale.x != scale.y || scale.y != scale.z {
        let inv = Vec3::ONE / scale;
        for n in &mut mesh.normals {
            *n = (Vec3::from(*n) * inv).normalize_or(Vec3::Z).to_array();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed, consistently outward-wound surface has zero vector area and positive volume
    /// (divergence theorem); an open or flipped one does not.
    fn closed(m: &MeshData) -> bool {
        let mut area = Vec3::ZERO;
        let mut volume = 0.0f32;
        let mut total = 0.0f32;
        for t in m.indices.chunks(3) {
            let [a, b, c] = [t[0], t[1], t[2]].map(|i| Vec3::from(m.positions[i as usize]));
            let cross = (b - a).cross(c - a);
            area += cross * 0.5;
            total += cross.length() * 0.5;
            volume += a.dot(b.cross(c)) / 6.0;
        }
        area.length() < 1e-4 * total.max(1e-6) && volume > 0.0
    }

    #[test]
    fn round_brilliant_has_tolkowsky_proportions() {
        let f = cut_facets("round_brilliant", 2.0, 2.0).expect("ok");
        let m = convex_mesh(&f, 2.0, "cut:round-2.0", "gem:s1").expect("ok");
        m.validate().expect("ok");
        let (lo, hi) = m.bounds();
        assert!((hi.x - lo.x - 2.0).abs() < 0.01, "width {}", hi.x - lo.x);
        // Crown height = (1 - 0.53)/2 * tan(34.5) = 16.2 % of the width; pavilion about 43 %.
        let crown = hi.z - 0.015;
        assert!((crown - 0.3233).abs() < 0.02, "crown {crown}");
        let pav = -lo.z - 0.015;
        assert!((pav - 0.86).abs() < 0.05, "pavilion {pav}");
        assert!(closed(&m), "the cut is a closed solid");
        // The table is flat and 53 % wide.
        let table: Vec<f32> = m
            .positions
            .iter()
            .zip(&m.normals)
            .filter(|(_, n)| n[2] > 0.999)
            .map(|(p, _)| Vec2::new(p[0], p[1]).length())
            .collect();
        let tr = table.iter().cloned().fold(0.0, f32::max);
        assert!((tr * 2.0 / 2.0 - 0.53).abs() < 0.03, "table {}", tr);
    }

    #[test]
    fn every_stand_in_cut_closes() {
        for (cut, l, w) in [
            ("marquise", 3.0, 1.5),
            ("pear", 4.0, 2.5),
            ("oval", 4.0, 3.0),
            ("princess", 2.0, 2.0),
        ] {
            let f = cut_facets(cut, l, w).expect("ok");
            let m = convex_mesh(&f, l, cut, "gem").expect("ok");
            m.validate().expect("ok");
            let (lo, hi) = m.bounds();
            assert!((hi.x - lo.x - l).abs() < 0.02 * l, "{cut} length {}", hi.x - lo.x);
            assert!((hi.y - lo.y - w).abs() < 0.05 * w, "{cut} width {}", hi.y - lo.y);
            assert!(closed(&m), "{cut} closes");
        }
        assert!(
            cut_facets("heart", 1.0, 1.0).is_none(),
            "concave cuts are not stand-ins here"
        );
    }

    #[test]
    fn an_open_facet_set_is_refused() {
        let f = vec![Facet {
            normal: [0.0, 0.0, 1.0],
            offset_mm: 1.0,
            kind: "table".into(),
        }];
        assert!(convex_mesh(&f, 1.0, "x", "g").is_none());
    }

    /// A pear wider than it is long has its tip inside the bulb, so the tangent
    /// construction is undefined. It must still be a finite closed solid of the
    /// requested size (length along +x in this helper), not a NaN outline.
    #[test]
    fn audit_wide_pear_is_a_closed_solid() {
        let f = cut_facets("pear", 2.0, 4.0).expect("outline");
        let m = convex_mesh(&f, 4.0, "pear-wide", "gem").expect("solid");
        m.validate().expect("ok");
        assert!(closed(&m), "wide pear closes");
        let (lo, hi) = m.bounds();
        assert!((hi.x - lo.x - 2.0).abs() < 0.08, "length {}", hi.x - lo.x);
        assert!((hi.y - lo.y - 4.0).abs() < 0.08, "width {}", hi.y - lo.y);
    }

    #[test]
    fn parts_are_valid_and_cheap_parts_are_cheap() {
        let outline = outline("marquise", 3.0, 1.5).expect("ok");
        let parts = [
            post("post", 1.2, 8.0, Detail::Cheap),
            ball("ball", 3.0, Detail::Fine),
            end_base("base", 2.5, 0.6, Detail::Fine),
            bezel_cup("bezel", &outline, 0.25, 0.9, 0.3, Detail::Fine),
            prong_head("prong", 2.25, 4, Detail::Fine),
            ring("ring", 8.0, 1.2, Detail::Fine),
            chain_link("link", 3.0, 2.0, 0.4, Detail::Fine),
        ];
        for p in &parts {
            p.validate().expect("ok");
            assert!(p.triangles() > 10, "{}", p.name);
            // Closed and wound outward, so a renderer may cull back faces.
            assert!(closed(p), "{} is closed and outward", p.name);
        }
        assert!(
            post("p", 1.2, 8.0, Detail::Cheap).triangles() * 3
                < post("p", 1.2, 8.0, Detail::Fine).triangles()
        );
    }
}
