// SPDX-License-Identifier: MPL-2.0
//! Stand-in gem cuts from published proportions, as facet planes and as flat-shaded meshes.
//!
//! A cut is a list of facet planes (`n · x <= d` inside, unit normals, millimeters), the form a
//! part kit's `gem_cuts[].facets[]` carries (plan `interfaces.md` § 5). The mesh is the convex
//! polytope those planes bound: every plane's face is a large quad in that plane clipped by every
//! other plane (Sutherland and Hodgman 1974, polygon clipping against a half-space). Concave cuts
//! (the heart) are the union of convex pieces whose shared cut faces are dropped.
//!
//! Proportions: the round brilliant follows Tolkowsky's published ideal (table 53 %, crown angle
//! 34.5 degrees, pavilion angle 40.75 degrees). The fancy shapes put the same crown and pavilion on a
//! marquise, pear, princess or heart outline with the published length-to-width ratios (marquise
//! 2.0, pear 1.5, heart 1.0, princess 1.0). These are generic fixtures, not any maker's design.

use glam::{Vec2, Vec3};

use crate::mesh::LookMesh;

/// One facet plane: points with `normal · x <= offset` are inside the stone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FacetPlane {
    /// Outward unit normal.
    pub normal: Vec3,
    /// Plane offset along the normal, in the cut's units.
    pub offset: f32,
    /// What the facet is (for statistics and tests; the shader only sees the id).
    pub role: FacetRole,
}

/// The role of a facet plane in a cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FacetRole {
    /// The flat top.
    Table,
    /// Crown facets between the table and the girdle.
    Crown,
    /// The thin vertical band at the widest outline.
    Girdle,
    /// Facets below the girdle.
    Pavilion,
    /// An internal cut plane between convex pieces of a concave cut; never meshed.
    Cut,
}

/// The stand-in cut types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CutType {
    /// Round brilliant (57 facets plus a faceted girdle).
    RoundBrilliant,
    /// Marquise (navette), length to width 2.0.
    Marquise,
    /// Pear (teardrop), length to width 1.5.
    Pear,
    /// Princess (square modified brilliant).
    Princess,
    /// Heart (two convex halves with a cleft), length to width 1.0.
    Heart,
}

impl FacetRole {
    /// The part kit's facet `kind` (`cut` is never written to a kit).
    pub fn as_str(self) -> &'static str {
        match self {
            FacetRole::Table => "table",
            FacetRole::Crown => "crown",
            FacetRole::Girdle => "girdle",
            FacetRole::Pavilion => "pavilion",
            FacetRole::Cut => "cut",
        }
    }
}

impl CutType {
    /// Every stand-in cut type.
    pub const ALL: [CutType; 5] = [
        CutType::RoundBrilliant,
        CutType::Marquise,
        CutType::Pear,
        CutType::Princess,
        CutType::Heart,
    ];

    /// The `cut_type` string a part kit uses for it.
    pub fn as_str(self) -> &'static str {
        match self {
            CutType::RoundBrilliant => "round_brilliant",
            CutType::Marquise => "marquise",
            CutType::Pear => "pear",
            CutType::Princess => "princess",
            CutType::Heart => "heart",
        }
    }

    /// Parses a part kit's `cut_type` string (also accepts `round`).
    pub fn parse(s: &str) -> Option<CutType> {
        Some(match s {
            "round_brilliant" | "round" => CutType::RoundBrilliant,
            "marquise" => CutType::Marquise,
            "pear" => CutType::Pear,
            "princess" => CutType::Princess,
            "heart" => CutType::Heart,
            _ => return None,
        })
    }

    /// Published length to width ratio of the outline.
    pub fn length_to_width(self) -> f32 {
        match self {
            CutType::RoundBrilliant | CutType::Princess | CutType::Heart => 1.0,
            CutType::Marquise => 2.0,
            CutType::Pear => 1.5,
        }
    }
}

/// Published proportions of a brilliant-style cut, as fractions of the width and angles in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Proportions {
    /// Table width as a fraction of the girdle width (Tolkowsky: 0.53).
    pub table: f32,
    /// Crown main (bezel) angle from the girdle plane, degrees (Tolkowsky: 34.5).
    pub crown_deg: f32,
    /// Pavilion main angle from the girdle plane, degrees (Tolkowsky: 40.75).
    pub pavilion_deg: f32,
    /// Girdle thickness as a fraction of the width.
    pub girdle: f32,
}

impl Proportions {
    /// Tolkowsky's 1919 ideal round brilliant (table 53 %, crown 34.5 degrees, pavilion 40.75 degrees),
    /// with a thin-to-medium girdle of 2.4 %.
    pub const TOLKOWSKY: Proportions = Proportions {
        table: 0.53,
        crown_deg: 34.5,
        pavilion_deg: 40.75,
        girdle: 0.024,
    };
}

/// A gem cut: its facet planes in millimeters, convex pieces, and its size.
#[derive(Debug, Clone, PartialEq)]
pub struct GemCut {
    /// Cut type.
    pub cut_type: CutType,
    /// Outline size `[length, width]` in millimeters. The length runs along +y and the width along
    /// +x, the table faces +z, the girdle's center is the origin (the part kit's gem-local frame).
    pub size_mm: [f32; 2],
    /// Convex pieces, each a list of facet planes (one piece except the heart).
    pub pieces: Vec<Vec<FacetPlane>>,
    /// Total depth (table to culet) in millimeters. A stand-in carries its proportions' depth (crown
    /// height plus girdle plus pavilion depth); a cut read from a part kit carries its facet planes'
    /// extent along z, which also holds when the culet sits off the axis (a pear's sits toward the
    /// round end).
    pub depth_mm: f32,
    /// The published proportions the cut was built from (written to and read back from a part kit's
    /// `proportions`); `None` for a kit entry without a complete `proportions` block.
    pub proportions: Option<Proportions>,
}

/// A kit number for an `f32`: the shortest decimal that reads back as the same `f32`, so facet planes
/// and sizes survive a part kit's text exactly (a fixed rounding moves near-degenerate facet corners
/// and changes the mesh's sliver triangles).
fn kit_number(x: f32) -> f64 {
    // `Display` for `f32` prints the shortest digits that parse back to the same value.
    x.to_string().parse::<f64>().unwrap_or(f64::from(x))
}

/// A derived kit percentage, rounded to 6 decimals for reading (never read back into geometry).
fn kit_percent(x: f32) -> f64 {
    (f64::from(x) * 1e6).round() / 1e6
}

/// A JSON number, or null when `x` is NaN or infinite. `serde_json::json!` maps those to null as
/// well; this keeps a programmatic non-finite proportion from depending on that conversion.
fn kit_value(x: f64) -> serde_json::Value {
    serde_json::Number::from_f64(x)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}

/// A part kit's `proportions` block (`table_pct`, `crown_angle_deg`, `pavilion_angle_deg`,
/// `girdle_pct`), or `None` when any of the four is missing or does not fit in a finite `f32`
/// (`1e308` is a JSON number and casts to infinity).
fn proportions_from_kit(p: &serde_json::Value) -> Option<Proportions> {
    let field = |name: &str| p.get(name).and_then(serde_json::Value::as_f64);
    let table = (field("table_pct")? / 100.0) as f32;
    let crown_deg = field("crown_angle_deg")? as f32;
    let pavilion_deg = field("pavilion_angle_deg")? as f32;
    let girdle = (field("girdle_pct")? / 100.0) as f32;
    if table.is_finite() && crown_deg.is_finite() && pavilion_deg.is_finite() && girdle.is_finite() {
        Some(Proportions {
            table,
            crown_deg,
            pavilion_deg,
            girdle,
        })
    } else {
        None
    }
}

/// The extent along z (table to culet) of the solid the pieces bound, or `None` when they bound
/// nothing.
fn solid_depth(pieces: &[Vec<FacetPlane>]) -> Option<f32> {
    let (lo, hi) = pieces
        .iter()
        .flat_map(|piece| polytope_faces(piece))
        .flat_map(|face| face.vertices)
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v.z), hi.max(v.z))
        });
    (lo.is_finite() && hi.is_finite()).then_some((hi - lo).max(0.0))
}

/// Where a facet station sits on the outline and what it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Station {
    /// Bezel above, pavilion main below.
    Main,
    /// Star facet (crown only, near the table).
    Star,
    /// Upper and lower girdle facets.
    Halves,
}

/// A closed outline as a dense polygon with vertex normals, normalized to half-width 1.
struct Outline {
    points: Vec<Vec2>,
    normals: Vec<Vec2>,
    /// Cumulative arc length at each point (first is 0); `total` closes the loop.
    arc: Vec<f32>,
    total: f32,
    /// Arc length ranges where no station is placed (the heart's cut edge).
    skip: Option<(f32, f32)>,
}

impl Outline {
    fn from_points(points: Vec<Vec2>, skip_from_index: Option<usize>) -> Outline {
        let n = points.len();
        let mut arc = Vec::with_capacity(n);
        let mut acc = 0.0;
        for i in 0..n {
            arc.push(acc);
            acc += (points[(i + 1) % n] - points[i]).length();
        }
        // Vertex normals: the sum of the two adjacent edge normals (a bisector at corners). The
        // polygon runs counter-clockwise, so the outward edge normal of (a -> b) is (dy, -dx).
        let edge_n = |i: usize| {
            let d = points[(i + 1) % n] - points[i];
            Vec2::new(d.y, -d.x).normalize_or_zero()
        };
        let normals = (0..n)
            .map(|i| (edge_n((i + n - 1) % n) + edge_n(i)).normalize_or_zero())
            .collect();
        let skip = skip_from_index.map(|i| (arc[i], acc));
        Outline {
            points,
            normals,
            arc,
            total: acc,
            skip,
        }
    }

    /// The outline point and outward normal at arc fraction `s` in `[0, 1)`.
    fn at(&self, s: f32) -> (Vec2, Vec2) {
        let target = s.rem_euclid(1.0) * self.total;
        let n = self.points.len();
        let i = match self.arc.binary_search_by(|a| a.total_cmp(&target)) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let j = (i + 1) % n;
        let seg = (if j == 0 { self.total } else { self.arc[j] }) - self.arc[i];
        let t = if seg > 0.0 {
            (target - self.arc[i]) / seg
        } else {
            0.0
        };
        let p = self.points[i].lerp(self.points[j], t);
        let m = self.normals[i].lerp(self.normals[j], t).normalize_or_zero();
        (p, m)
    }

    fn skipped(&self, s: f32) -> bool {
        let a = s.rem_euclid(1.0) * self.total;
        self.skip.is_some_and(|(lo, hi)| a > lo - 1e-4 && a < hi + 1e-4)
    }
}

fn circle_arc(center: Vec2, r: f32, a0: f32, a1: f32, steps: usize, out: &mut Vec<Vec2>) {
    for k in 0..steps {
        let a = a0 + (a1 - a0) * k as f32 / steps as f32;
        out.push(center + r * Vec2::new(a.cos(), a.sin()));
    }
}

/// Marquise upper arc for half-length `a`, half-width `b`: a circle centered at (0, -c) through the
/// tips (+-a, 0) and the top (0, b). Returns (center y, radius, start angle).
fn marquise_arc(a: f32, b: f32) -> (f32, f32, f32) {
    let c = (a * a - b * b) / (2.0 * b);
    (-c, b + c, c.atan2(a))
}

fn outline_for(cut: CutType) -> Outline {
    const STEPS: usize = 96;
    let mut pts = Vec::new();
    match cut {
        CutType::RoundBrilliant => {
            circle_arc(Vec2::ZERO, 1.0, 0.0, std::f32::consts::TAU, 4 * STEPS, &mut pts)
        }
        CutType::Marquise => {
            let (cy, r, a0) = marquise_arc(2.0, 1.0);
            let a1 = std::f32::consts::PI - a0;
            circle_arc(Vec2::new(0.0, cy), r, a0, a1, 2 * STEPS, &mut pts);
            circle_arc(
                Vec2::new(0.0, -cy),
                r,
                a0 + std::f32::consts::PI,
                a1 + std::f32::consts::PI,
                2 * STEPS,
                &mut pts,
            );
        }
        CutType::Pear => {
            // Tip toward +x: a marquise arc (half-length 2) for x >= 0 meets a round end of radius 1
            // with a horizontal tangent at x = 0. Length 3, width 2.
            let (cy, r, a0) = marquise_arc(2.0, 1.0);
            let half_pi = std::f32::consts::FRAC_PI_2;
            circle_arc(Vec2::new(0.0, cy), r, a0, half_pi, STEPS, &mut pts);
            circle_arc(Vec2::ZERO, 1.0, half_pi, 3.0 * half_pi, 2 * STEPS, &mut pts);
            circle_arc(Vec2::new(0.0, -cy), r, -half_pi, -a0, STEPS, &mut pts);
            // Center the length on the origin.
            for p in &mut pts {
                p.x -= 0.5;
            }
        }
        CutType::Princess => {
            // A square of half-width 1 with small corner chamfers so the girdle never ends in a knife edge.
            let k = 0.06;
            let corners = [
                Vec2::new(1.0, -1.0),
                Vec2::new(1.0, 1.0),
                Vec2::new(-1.0, 1.0),
                Vec2::new(-1.0, -1.0),
            ];
            for (i, c) in corners.iter().enumerate() {
                let next = corners[(i + 1) % 4];
                let prev = corners[(i + 3) % 4];
                pts.push(*c + (prev - *c).normalize() * k);
                pts.push(*c + (next - *c).normalize() * k);
                // Densify the side.
                let a = *c + (next - *c).normalize() * k;
                let b = next + (*c - next).normalize() * k;
                for s in 1..STEPS / 2 {
                    pts.push(a.lerp(b, s as f32 / (STEPS / 2) as f32));
                }
            }
        }
        CutType::Heart => return heart_half_outline(),
    }
    Outline::from_points(pts, None)
}

/// The right half (x >= 0) of a heart of width 2 pointing toward -y: the convex hull of a lobe circle
/// (center (0.5, 0.35), radius 0.5) and the tip (0, -1.15), cut at x = 0.
fn heart_half_outline() -> Outline {
    let c = Vec2::new(0.5, 0.35);
    let r = 0.5;
    let tip = Vec2::new(0.0, -1.15);
    // Tangent point from the tip to the circle on the right side.
    let d = c - tip;
    let dist = d.length();
    let base = d.y.atan2(d.x);
    let off = (r / dist).asin();
    // The right tangent line direction from the tip; the touch point is perpendicular to it.
    let dir_angle = base - off;
    let dir = Vec2::new(dir_angle.cos(), dir_angle.sin());
    let touch_normal = Vec2::new(dir.y, -dir.x);
    let touch_angle = touch_normal.y.atan2(touch_normal.x);
    let mut pts = Vec::new();
    // Counter-clockwise: tip -> tangent point -> around the lobe over the top -> cleft (0, 0.35) -> down x = 0.
    let steps = 64;
    for k in 0..steps {
        pts.push(tip.lerp(c + r * touch_normal, k as f32 / steps as f32));
    }
    circle_arc(c, r, touch_angle, std::f32::consts::PI, 3 * steps, &mut pts);
    let cleft = c + r * Vec2::new(-1.0, 0.0);
    let cut_start = pts.len();
    for k in 0..steps {
        pts.push(cleft.lerp(tip, k as f32 / steps as f32));
    }
    Outline::from_points(pts, Some(cut_start))
}

/// Facet planes of a brilliant-style cut on an outline, in half-width units (girdle center at z = 0).
fn brilliant_planes(
    outline: &Outline,
    p: Proportions,
    mains: usize,
    pavilion_tiers: &[f32],
) -> Vec<FacetPlane> {
    let g = p.girdle; // half-width units: girdle thickness fraction of the width (2) halved -> p.girdle
    let tan_c = p.crown_deg.to_radians().tan();
    let inset = (1.0 - p.table) * 1.0;
    let z_table = g + inset * tan_c;
    let crown = p.crown_deg.to_radians();
    let star = (p.crown_deg - 12.0).to_radians();
    let upper = (p.crown_deg + 7.0).to_radians();
    let pav = p.pavilion_deg.to_radians();
    let lower = (p.pavilion_deg + 1.6).to_radians();
    let mut planes = vec![FacetPlane {
        normal: Vec3::Z,
        offset: z_table,
        role: FacetRole::Table,
    }];
    let crown_plane = |q: Vec2, m: Vec2, angle: f32, z: f32| {
        let n = Vec3::new(angle.sin() * m.x, angle.sin() * m.y, angle.cos());
        FacetPlane {
            normal: n,
            offset: n.dot(Vec3::new(q.x, q.y, z)),
            role: FacetRole::Crown,
        }
    };
    let pavilion_plane = |q: Vec2, m: Vec2, angle: f32, z: f32| {
        let n = Vec3::new(angle.sin() * m.x, angle.sin() * m.y, -angle.cos());
        FacetPlane {
            normal: n,
            offset: n.dot(Vec3::new(q.x, q.y, z)),
            role: FacetRole::Pavilion,
        }
    };
    // Stations: mains at k / mains, stars between mains, halves at quarter steps.
    let n_st = mains * 4;
    for k in 0..n_st {
        let s = k as f32 / n_st as f32;
        if outline.skipped(s) {
            continue;
        }
        let station = match k % 4 {
            0 => Station::Main,
            2 => Station::Star,
            _ => Station::Halves,
        };
        let (q, m) = outline.at(s);
        match station {
            Station::Main => {
                planes.push(crown_plane(q, m, crown, g));
                planes.push(pavilion_plane(q, m, pav, -g));
                // Chevron tiers (princess): steeper planes near the girdle.
                for extra in pavilion_tiers {
                    planes.push(pavilion_plane(q, m, pav + extra.to_radians(), -g));
                }
            }
            Station::Star => {
                // Through the table edge at this station, a little inside the bezel-octagon edge.
                let star_extra = 0.04;
                let t = q - m * (inset + star_extra);
                planes.push(crown_plane(t, m, star, z_table));
            }
            Station::Halves => {
                planes.push(crown_plane(q, m, upper, g));
                planes.push(pavilion_plane(q, m, lower, -g));
            }
        }
    }
    // Faceted girdle: vertical planes on a dense set of stations.
    let n_girdle = 64;
    for k in 0..n_girdle {
        let s = (k as f32 + 0.5) / n_girdle as f32;
        if outline.skipped(s) {
            continue;
        }
        let (q, m) = outline.at(s);
        let n = Vec3::new(m.x, m.y, 0.0);
        planes.push(FacetPlane {
            normal: n,
            offset: n.dot(Vec3::new(q.x, q.y, 0.0)),
            role: FacetRole::Girdle,
        });
    }
    dedup_planes(planes)
}

/// The most facet planes [`GemCut::from_kit_entry`] reads. Building the solid is cubic in the plane
/// count; real cuts have tens to a few hundred facets, and a finer shape ships as a mesh.
pub const MAX_CUT_FACETS: usize = 512;

impl GemCut {
    /// Builds a stand-in cut of `size_mm = [length, width]` with Tolkowsky proportions.
    pub fn new(cut_type: CutType, size_mm: [f32; 2]) -> GemCut {
        GemCut::with_proportions(cut_type, size_mm, Proportions::TOLKOWSKY)
    }

    /// Builds a stand-in cut with explicit proportions. The outline keeps its published length to
    /// width ratio; `size_mm[1]` (the width) sets the scale.
    pub fn with_proportions(cut_type: CutType, size_mm: [f32; 2], p: Proportions) -> GemCut {
        let outline = outline_for(cut_type);
        let (mains, tiers): (usize, &[f32]) = match cut_type {
            CutType::RoundBrilliant | CutType::Marquise | CutType::Pear => (8, &[]),
            CutType::Princess => (8, &[9.0, 4.0]),
            CutType::Heart => (8, &[]),
        };
        let half_w = size_mm[1].max(0.01) * 0.5;
        let scale = |planes: Vec<FacetPlane>| -> Vec<FacetPlane> {
            planes
                .into_iter()
                .map(|f| FacetPlane {
                    offset: f.offset * half_w,
                    ..f
                })
                .collect()
        };
        let mut pieces = Vec::new();
        let base = brilliant_planes(&outline, p, mains, tiers);
        if cut_type == CutType::Heart {
            let mut right = base.clone();
            right.push(FacetPlane {
                normal: Vec3::NEG_X,
                offset: 0.0,
                role: FacetRole::Cut,
            });
            let left: Vec<FacetPlane> = right
                .iter()
                .map(|f| FacetPlane {
                    normal: Vec3::new(-f.normal.x, f.normal.y, f.normal.z),
                    ..*f
                })
                .collect();
            pieces.push(scale(right));
            pieces.push(scale(left));
        } else {
            // The outlines are built with the length along +x; the part kit's gem-local frame puts it
            // along +y (+x across the width): turn a quarter about +z.
            let turned = base
                .into_iter()
                .map(|f| FacetPlane {
                    normal: Vec3::new(-f.normal.y, f.normal.x, f.normal.z),
                    ..f
                })
                .collect();
            pieces.push(scale(turned));
        }
        let tan_c = p.crown_deg.to_radians().tan();
        let crown_h = p.girdle + (1.0 - p.table) * tan_c;
        let pav_h = p.girdle + p.pavilion_deg.to_radians().tan();
        let length = size_mm[1] * cut_type.length_to_width();
        GemCut {
            cut_type,
            size_mm: [length, size_mm[1]],
            pieces,
            depth_mm: (crown_h + pav_h) * half_w,
            proportions: Some(p),
        }
    }

    /// The virtual pavilion this cut shows: a square outline has its own, the rest the round cut's.
    pub fn facet_family(&self) -> crate::material::FacetFamily {
        match self.cut_type {
            CutType::Princess => crate::material::FacetFamily::Step,
            CutType::RoundBrilliant | CutType::Marquise | CutType::Pear | CutType::Heart => {
                crate::material::FacetFamily::Brilliant
            }
        }
    }

    /// The stone size the stone shaders take.
    pub fn stone_size(&self) -> crate::material::StoneSize {
        // The length runs along +y; the heart's normalized outline is its width both ways.
        let (hl, hw) = match self.cut_type {
            CutType::Heart => (self.size_mm[1] * 0.5, self.size_mm[1] * 0.5),
            _ => (self.size_mm[0] * 0.5, self.size_mm[1] * 0.5),
        };
        crate::material::StoneSize {
            depth_mm: self.depth_mm,
            half_length_mm: hl,
            half_width_mm: hw,
            turn: 0.0,
            facets: self.facet_family(),
        }
    }

    /// The cut as a part kit `gem_cuts[]` entry (`jewelry.part_kit/1`, gem-local frame): facet planes
    /// `{ kind, normal, offset_mm }`, the cut's own published proportions (`null` when it has none),
    /// symmetry. Planes and sizes are written as the shortest decimals that read back as the same
    /// `f32`, so [`GemCut::from_kit_entry`] rebuilds the same planes and mesh. `None` for a concave
    /// cut (the heart), which a kit carries as a mesh (`mesh_ref`).
    pub fn to_kit_entry(&self, cut_id: &str) -> Option<serde_json::Value> {
        let [piece] = self.pieces.as_slice() else {
            return None;
        };
        let width = self.size_mm[1];
        let facets: Vec<serde_json::Value> = piece
            .iter()
            .map(|f| {
                serde_json::json!({
                    "kind": f.role.as_str(),
                    "normal": [
                        kit_value(kit_number(f.normal.x)),
                        kit_value(kit_number(f.normal.y)),
                        kit_value(kit_number(f.normal.z)),
                    ],
                    "offset_mm": kit_value(kit_number(f.offset)),
                })
            })
            .collect();
        let length_width = serde_json::json!([
            kit_value(kit_number(self.size_mm[0])),
            kit_value(kit_number(width))
        ]);
        let (n_fold, size) = match self.cut_type {
            CutType::RoundBrilliant => (8, serde_json::json!([kit_value(kit_number(width))])),
            CutType::Princess => (4, length_width),
            CutType::Marquise => (2, length_width),
            CutType::Pear | CutType::Heart => (1, length_width),
        };
        let proportions = self.proportions.map(|p| {
            let crown_h = p.girdle + (1.0 - p.table) * p.crown_deg.to_radians().tan();
            let pav_h = p.girdle + p.pavilion_deg.to_radians().tan();
            serde_json::json!({
                "table_pct": kit_value(kit_percent(p.table * 100.0)),
                "crown_angle_deg": kit_value(kit_number(p.crown_deg)),
                "pavilion_angle_deg": kit_value(kit_number(p.pavilion_deg)),
                "girdle_pct": kit_value(kit_percent(p.girdle * 100.0)),
                "length_to_width": kit_value(kit_number(self.cut_type.length_to_width())),
                // Half-width units: 1 unit is half the width, so a percent of the width is x 50. Each
                // height includes half the girdle (crown + pavilion = total depth).
                "crown_height_pct": kit_value(kit_percent(crown_h * 50.0)),
                "pavilion_depth_pct": kit_value(kit_percent(pav_h * 50.0)),
            })
        });
        Some(serde_json::json!({
            "cut_id": cut_id,
            "cut_type": self.cut_type.as_str(),
            "size_mm": size,
            "proportions": proportions,
            "facets": facets,
            "convex": true,
            "mesh_ref": null,
            "symmetry": { "n_fold": n_fold, "mirror": true },
            "girdle_finish": "faceted",
            "max_bounces_hint": 2,
        }))
    }

    /// Builds a cut from a part kit `gem_cuts[]` entry: its facet planes as given (never decimated),
    /// `size_mm` (`[d]` round, else `[length, width]`). Unknown cut types draw as a round brilliant's
    /// look on their own planes.
    pub fn from_kit_entry(entry: &serde_json::Value) -> Result<GemCut, crate::material::LookError> {
        use crate::material::LookError;
        let id = entry
            .get("cut_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let missing = |field: &str| LookError::MissingField {
            material: id.clone(),
            field: field.to_string(),
        };
        if entry.get("convex").and_then(serde_json::Value::as_bool) == Some(false) {
            return Err(LookError::NotDrawable(
                id.clone(),
                "concave cut (needs its mesh_ref)".into(),
            ));
        }
        let cut_type = entry
            .get("cut_type")
            .and_then(serde_json::Value::as_str)
            .and_then(CutType::parse)
            .unwrap_or(CutType::RoundBrilliant);
        let size: Vec<f32> = entry
            .get("size_mm")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_f64)
                    .map(|v| v as f32)
                    .collect()
            })
            .unwrap_or_default();
        let size_mm = match size.as_slice() {
            [d] => [*d, *d],
            [l, w, ..] => [*l, *w],
            _ => return Err(missing("size_mm")),
        };
        if !size_mm.iter().all(|v| v.is_finite() && *v > 0.0) {
            return Err(missing("size_mm (finite, above 0)"));
        }
        let facets = entry
            .get("facets")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| missing("facets"))?;
        if facets.len() > MAX_CUT_FACETS {
            return Err(missing(&format!(
                "facets (at most {MAX_CUT_FACETS} planes; a finer shape is a mesh)"
            )));
        }
        let mut planes = Vec::with_capacity(facets.len());
        for f in facets {
            let n: Vec<f32> = f
                .get("normal")
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(serde_json::Value::as_f64)
                        .map(|v| v as f32)
                        .collect()
                })
                .unwrap_or_default();
            let offset = f
                .get("offset_mm")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| missing("facets[].offset_mm"))?;
            let [x, y, z] = n.as_slice() else {
                return Err(missing("facets[].normal"));
            };
            let normal = Vec3::new(*x, *y, *z);
            let length = normal.length();
            if !length.is_finite() || (length - 1.0).abs() > 1e-3 {
                return Err(missing("facets[].normal (unit length)"));
            }
            if !(offset as f32).is_finite() {
                return Err(missing("facets[].offset_mm (finite)"));
            }
            let role = match f.get("kind").and_then(serde_json::Value::as_str) {
                Some("table") => FacetRole::Table,
                Some("girdle") => FacetRole::Girdle,
                Some("pavilion") => FacetRole::Pavilion,
                _ => FacetRole::Crown,
            };
            planes.push(FacetPlane {
                normal,
                offset: offset as f32,
                role,
            });
        }
        if planes.is_empty() {
            return Err(missing("facets"));
        }
        // The depth is the solid's extent along z (table to culet), not the planes' bounds on the
        // axis: a pear's culet sits toward its round end, where the pavilion reaches deepest. Planes
        // that bound no top or no bottom keep the old 60 % of the width.
        let bounded = planes.iter().any(|p| p.normal.z > 1e-3) && planes.iter().any(|p| p.normal.z < -1e-3);
        let pieces = vec![planes];
        let depth = if bounded { solid_depth(&pieces) } else { None }.unwrap_or(size_mm[1] * 0.6);
        Ok(GemCut {
            cut_type,
            size_mm,
            pieces,
            depth_mm: depth,
            proportions: entry.get("proportions").and_then(proportions_from_kit),
        })
    }

    /// Every facet plane of every piece (cut planes included).
    pub fn planes(&self) -> impl Iterator<Item = &FacetPlane> {
        self.pieces.iter().flatten()
    }

    /// The flat-shaded mesh: one id per facet, so the shader can vary sparkle per facet.
    pub fn mesh(&self) -> LookMesh {
        let _span = tracing::trace_span!("gem_cut_mesh").entered();
        let mut mesh = LookMesh::default();
        let mut facet_id = 0u32;
        for piece in &self.pieces {
            for face in polytope_faces(piece) {
                if piece[face.plane].role == FacetRole::Cut {
                    continue;
                }
                mesh.push_flat_polygon(&face.vertices, piece[face.plane].normal, facet_id);
                facet_id += 1;
            }
        }
        mesh
    }
}

/// One face of a convex polytope: the plane index and its vertices (counter-clockwise seen from outside).
#[derive(Debug, Clone)]
pub struct PolytopeFace {
    /// Index of the plane in the input list.
    pub plane: usize,
    /// Vertices, counter-clockwise around the plane normal.
    pub vertices: Vec<Vec3>,
}

/// The faces of the convex polytope `{x : n_i · x <= d_i}`. Planes that do not touch the polytope
/// (or touch it only in an edge or point) produce no face.
pub fn polytope_faces(planes: &[FacetPlane]) -> Vec<PolytopeFace> {
    let size = planes.iter().map(|p| p.offset.abs()).fold(1e-3f32, f32::max);
    let extent = size * 8.0;
    let eps = extent * 1e-6;
    // A face narrower than this (twice its area over its perimeter) is a numerical sliver where
    // nearly coincident planes meet; it is dropped.
    let min_width = size * 2e-4;
    let mut faces = Vec::new();
    for (i, plane) in planes.iter().enumerate() {
        let n = plane.normal;
        let u = n.any_orthonormal_vector();
        let v = n.cross(u);
        let c = n * plane.offset;
        // Counter-clockwise around n: u x v = n.
        let mut poly = vec![
            c - u * extent - v * extent,
            c + u * extent - v * extent,
            c + u * extent + v * extent,
            c - u * extent + v * extent,
        ];
        for (j, other) in planes.iter().enumerate() {
            if i == j || poly.is_empty() {
                continue;
            }
            poly = clip_polygon(&poly, other.normal, other.offset, eps);
        }
        dedup_ring(&mut poly, eps * 10.0);
        if poly.len() >= 3 && 2.0 * polygon_area(&poly, n) > min_width * perimeter(&poly) {
            faces.push(PolytopeFace {
                plane: i,
                vertices: poly,
            });
        }
    }
    faces
}

/// Sutherland-Hodgman: keeps the part of a convex polygon with `n · x <= d + eps`.
fn clip_polygon(poly: &[Vec3], n: Vec3, d: f32, eps: f32) -> Vec<Vec3> {
    let mut out = Vec::with_capacity(poly.len() + 1);
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        // Distances to the plane moved out by `eps`, so the inside test and the crossing point use
        // the same plane: the crossing then always lies on the segment (t in [0, 1]).
        let da = n.dot(a) - d - eps;
        let db = n.dot(b) - d - eps;
        let a_in = da <= 0.0;
        let b_in = db <= 0.0;
        if a_in {
            out.push(a);
        }
        if a_in != b_in {
            let t = (da / (da - db)).clamp(0.0, 1.0);
            out.push(a + (b - a) * t);
        }
    }
    out
}

fn dedup_ring(poly: &mut Vec<Vec3>, eps: f32) {
    let mut out: Vec<Vec3> = Vec::with_capacity(poly.len());
    for p in poly.iter() {
        if out.last().is_none_or(|q| q.distance(*p) > eps) {
            out.push(*p);
        }
    }
    while out.len() > 1 && out[0].distance(out[out.len() - 1]) <= eps {
        out.pop();
    }
    *poly = out;
}

fn perimeter(poly: &[Vec3]) -> f32 {
    (0..poly.len())
        .map(|i| poly[i].distance(poly[(i + 1) % poly.len()]))
        .sum()
}

/// Removes planes that repeat an earlier one (straight outline sides give identical stations).
fn dedup_planes(planes: Vec<FacetPlane>) -> Vec<FacetPlane> {
    let mut out: Vec<FacetPlane> = Vec::with_capacity(planes.len());
    for p in planes {
        let dup = out.iter().any(|q| {
            q.normal.distance(p.normal) < 1e-4 && (q.offset - p.offset).abs() < 1e-4 * q.offset.abs().max(1.0)
        });
        if !dup {
            out.push(p);
        }
    }
    out
}

fn polygon_area(poly: &[Vec3], n: Vec3) -> f32 {
    let mut acc = Vec3::ZERO;
    for i in 0..poly.len() {
        acc += poly[i].cross(poly[(i + 1) % poly.len()]);
    }
    0.5 * acc.dot(n).abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_from_six_planes_has_six_square_faces() {
        let planes: Vec<FacetPlane> = [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z]
            .iter()
            .map(|n| FacetPlane {
                normal: *n,
                offset: 1.0,
                role: FacetRole::Crown,
            })
            .collect();
        let faces = polytope_faces(&planes);
        assert_eq!(faces.len(), 6);
        for f in &faces {
            assert_eq!(f.vertices.len(), 4, "{f:?}");
            assert!((polygon_area(&f.vertices, planes[f.plane].normal) - 4.0).abs() < 1e-4);
        }
    }

    #[test]
    fn redundant_plane_produces_no_face() {
        let mut planes: Vec<FacetPlane> = [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z]
            .iter()
            .map(|n| FacetPlane {
                normal: *n,
                offset: 1.0,
                role: FacetRole::Crown,
            })
            .collect();
        planes.push(FacetPlane {
            normal: Vec3::X,
            offset: 5.0,
            role: FacetRole::Crown,
        });
        assert_eq!(polytope_faces(&planes).len(), 6);
    }

    #[test]
    fn duplicate_planes_are_merged() {
        let cut = GemCut::new(CutType::Princess, [5.0, 5.0]);
        let planes = &cut.pieces[0];
        for (i, a) in planes.iter().enumerate() {
            for b in &planes[i + 1..] {
                assert!(
                    a.normal.distance(b.normal) > 1e-4 || (a.offset - b.offset).abs() > 1e-3,
                    "{a:?} {b:?}"
                );
            }
        }
    }

    #[test]
    fn round_brilliant_has_the_published_facet_count_and_depth() {
        let cut = GemCut::new(CutType::RoundBrilliant, [2.0, 2.0]);
        let faces = polytope_faces(&cut.pieces[0]);
        let count = |role| {
            faces
                .iter()
                .filter(|f| cut.pieces[0][f.plane].role == role)
                .count()
        };
        // 1 table, 8 bezel + 8 star + 16 upper girdle, 8 pavilion main + 16 lower girdle.
        assert_eq!(count(FacetRole::Table), 1);
        assert_eq!(count(FacetRole::Crown), 32, "crown facets");
        assert_eq!(count(FacetRole::Pavilion), 24, "pavilion facets");
        assert!(count(FacetRole::Girdle) >= 32, "faceted girdle");
        // Tolkowsky: total depth 59.3 % of the diameter (16.2 crown + 43.1 pavilion), plus girdle.
        let depth_frac = cut.depth_mm / 2.0;
        assert!((0.58..0.64).contains(&depth_frac), "depth {depth_frac}");
    }

    #[test]
    fn every_cut_is_closed_and_outward() {
        for cut_type in CutType::ALL {
            let cut = GemCut::new(cut_type, [3.0, 3.0]);
            let mesh = cut.mesh();
            assert!(
                mesh.triangle_count() > 40,
                "{cut_type:?} {}",
                mesh.triangle_count()
            );
            // Closed and outward: the divergence theorem gives a positive volume.
            let vol = mesh.signed_volume();
            assert!(vol > 0.0, "{cut_type:?} volume {vol}");
            // Every face normal points away from the centroid of its own piece's vertices.
            let length = cut.size_mm[0];
            let (lo, hi) = mesh.bounds();
            // The length runs along +y, the width along +x (the part kit's gem-local frame).
            assert!(
                (hi.y - lo.y - length).abs() < 0.12 * length,
                "{cut_type:?} length {} vs {length}",
                hi.y - lo.y
            );
            assert!(
                (hi.x - lo.x - 3.0).abs() < 0.12 * 3.0,
                "{cut_type:?} width {}",
                hi.x - lo.x
            );
        }
    }

    #[test]
    fn every_vertex_stays_inside_the_outline_box() {
        for cut_type in CutType::ALL {
            for width in [3.0f32, 5.0] {
                let cut = GemCut::new(cut_type, [width * cut_type.length_to_width(), width]);
                let (hx, hy) = match cut_type {
                    CutType::Heart => (width * 0.5, width * 0.5),
                    _ => (cut.size_mm[1] * 0.5, cut.size_mm[0] * 0.5),
                };
                for piece in &cut.pieces {
                    for face in polytope_faces(piece) {
                        for v in &face.vertices {
                            assert!(
                                v.x.abs() <= hx * 1.03 + 0.02
                                    && v.y.abs() <= hy * 1.2 + 0.02
                                    && v.z.abs() <= width,
                                "{cut_type:?} w{width}: plane {} {:?} vertex {v}",
                                face.plane,
                                piece[face.plane]
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn heart_has_a_cleft() {
        let cut = GemCut::new(CutType::Heart, [4.0, 4.0]);
        let mesh = cut.mesh();
        // At x = 0 the top of the outline sits below the lobes' top.
        let top_at = |x0: f32| {
            mesh.positions
                .iter()
                .filter(|p| (p[0] - x0).abs() < 0.25)
                .map(|p| p[1])
                .fold(f32::MIN, f32::max)
        };
        assert!(
            top_at(0.0) + 0.3 < top_at(1.0),
            "cleft {} lobe {}",
            top_at(0.0),
            top_at(1.0)
        );
    }

    #[test]
    fn kit_entries_round_trip_and_keep_the_mesh() {
        for cut_type in [
            CutType::RoundBrilliant,
            CutType::Marquise,
            CutType::Pear,
            CutType::Princess,
        ] {
            let cut = GemCut::new(cut_type, [3.0 * cut_type.length_to_width(), 3.0]);
            let entry = cut.to_kit_entry("standin").expect("convex");
            assert_eq!(
                entry["facets"].as_array().map(Vec::len),
                Some(cut.pieces[0].len())
            );
            let back = GemCut::from_kit_entry(&entry).expect("parses");
            assert_eq!(back.cut_type, cut_type);
            let (a, b) = (cut.mesh(), back.mesh());
            assert_eq!(a.triangle_count(), b.triangle_count(), "{cut_type:?}");
            assert!((a.signed_volume() - b.signed_volume()).abs() < 1e-3 * a.signed_volume());
            assert!(
                (back.depth_mm - cut.depth_mm).abs() < 0.02 * cut.depth_mm,
                "{} vs {}",
                back.depth_mm,
                cut.depth_mm
            );
        }
        assert!(
            GemCut::new(CutType::Heart, [3.0, 3.0])
                .to_kit_entry("h")
                .is_none()
        );
        let concave = serde_json::json!({ "cut_id": "c", "convex": false, "size_mm": [3.0], "facets": [] });
        assert_eq!(
            GemCut::from_kit_entry(&concave).err().map_or("ok", |e| e.code()),
            "material_not_drawable"
        );
        let no_facets = serde_json::json!({ "cut_id": "c", "size_mm": [3.0] });
        assert_eq!(
            GemCut::from_kit_entry(&no_facets)
                .err()
                .map_or("ok", |e| e.code()),
            "material_missing_field"
        );
    }

    /// The kit's text (not only the in-memory value) carries every plane and size bit for bit, at
    /// every stand-in width the agent accepts, so the rebuilt mesh is the same mesh.
    #[test]
    fn kit_text_round_trip_keeps_every_plane_bit() {
        for cut_type in [
            CutType::RoundBrilliant,
            CutType::Marquise,
            CutType::Pear,
            CutType::Princess,
        ] {
            for width in [0.5f32, 0.8, 1.5, 2.0, 3.0, 4.5, 6.0, 10.0, 20.0, 40.0] {
                let cut = GemCut::new(cut_type, [width * cut_type.length_to_width(), width]);
                let entry = cut.to_kit_entry("standin").expect("convex");
                let text = serde_json::to_string(&entry).expect("serializes");
                let parsed: serde_json::Value = serde_json::from_str(&text).expect("parses");
                let back = GemCut::from_kit_entry(&parsed).expect("reads");
                assert_eq!(back.size_mm, cut.size_mm, "{cut_type:?} w{width}");
                assert_eq!(back.pieces, cut.pieces, "{cut_type:?} w{width}: planes moved");
                assert_eq!(back.mesh(), cut.mesh(), "{cut_type:?} w{width}: mesh changed");
            }
        }
    }

    /// The kit carries the cut's own proportions (not always Tolkowsky's), and reads them back.
    #[test]
    fn kit_entries_carry_the_cuts_own_proportions() {
        let p = Proportions {
            table: 0.58,
            crown_deg: 33.3,
            pavilion_deg: 41.1,
            girdle: 0.031,
        };
        let cut = GemCut::with_proportions(CutType::Pear, [4.5, 3.0], p);
        let entry = cut.to_kit_entry("custom").expect("convex");
        let q = &entry["proportions"];
        let near = |v: &serde_json::Value, want: f64| v.as_f64().is_some_and(|x| (x - want).abs() < 1e-5);
        assert!(near(&q["table_pct"], 58.0), "{q}");
        assert!(near(&q["girdle_pct"], 3.1), "{q}");
        // Angles are written as the shortest decimal of the `f32`, which is the number given.
        assert_eq!(q["crown_angle_deg"].as_f64(), Some(33.3));
        assert_eq!(q["pavilion_angle_deg"].as_f64(), Some(41.1));
        assert_eq!(q["length_to_width"].as_f64(), Some(1.5));
        let back = GemCut::from_kit_entry(&entry).expect("reads");
        let r = back.proportions.expect("proportions read back");
        for (a, b) in [
            (r.table, p.table),
            (r.crown_deg, p.crown_deg),
            (r.pavilion_deg, p.pavilion_deg),
            (r.girdle, p.girdle),
        ] {
            assert!((a - b).abs() <= 1e-6 * b.abs(), "{r:?} vs {p:?}");
        }
        assert_eq!(back.pieces, cut.pieces);
        // A kit without a complete proportions block reads as `None` and writes `null`.
        let mut bare = entry.clone();
        bare["proportions"] = serde_json::json!({ "table_pct": 58.0 });
        let bare = GemCut::from_kit_entry(&bare).expect("reads");
        assert_eq!(bare.proportions, None);
        assert!(bare.to_kit_entry("bare").expect("convex")["proportions"].is_null());
    }

    /// A kit cut's depth is its solid's extent along z (table to culet), wherever the culet sits;
    /// a stand-in's published depth agrees with its own planes to within 1 % (the pear's discrete
    /// pavilion stations reach about 0.9 % below the published depth, the others under 0.01 %).
    #[test]
    fn kit_depth_is_the_solids_extent_even_off_axis() {
        for cut_type in [
            CutType::RoundBrilliant,
            CutType::Marquise,
            CutType::Pear,
            CutType::Princess,
        ] {
            let cut = GemCut::new(cut_type, [3.0 * cut_type.length_to_width(), 3.0]);
            let (lo, hi) = cut.mesh().bounds();
            let extent = hi.z - lo.z;
            assert!(
                (cut.depth_mm - extent).abs() < 0.01 * extent,
                "{cut_type:?}: published {} vs planes {extent}",
                cut.depth_mm
            );
            let back = GemCut::from_kit_entry(&cut.to_kit_entry("s").expect("convex")).expect("reads");
            assert!(
                (back.depth_mm - extent).abs() < 1e-4 * extent,
                "{cut_type:?}: kit depth {} vs extent {extent}",
                back.depth_mm
            );
        }
        // The pear's culet is off the girdle's center: the axis alone would read about 4 % short.
        let pear = GemCut::new(CutType::Pear, [4.5, 3.0]);
        let culet = pear
            .mesh()
            .positions
            .iter()
            .copied()
            .min_by(|a, b| a[2].total_cmp(&b[2]))
            .expect("vertices");
        assert!(culet[1].abs() > 0.1, "pear culet {culet:?} sits on the axis");
    }

    #[test]
    fn only_the_square_outline_takes_the_step_family() {
        use crate::material::FacetFamily;
        for c in CutType::ALL {
            let want = if c == CutType::Princess {
                FacetFamily::Step
            } else {
                FacetFamily::Brilliant
            };
            assert_eq!(GemCut::new(c, [3.0, 3.0]).stone_size().facets, want, "{c:?}");
        }
    }

    #[test]
    fn cut_type_strings_round_trip() {
        for c in CutType::ALL {
            assert_eq!(CutType::parse(c.as_str()), Some(c));
        }
        assert_eq!(CutType::parse("rose"), None);
    }

    /// Audit fix (sprint-fix-audit-jw, finding 7): a kit cut with more planes than a cut has, or with
    /// values that are not finite, is refused before any plane is built (it used to be read in full and
    /// turned into NaN planes).
    #[test]
    fn a_kit_cut_with_too_many_or_non_finite_planes_is_refused() {
        let good = GemCut::new(CutType::RoundBrilliant, [3.0, 3.0])
            .to_kit_entry("r")
            .expect("convex");
        assert!(GemCut::from_kit_entry(&good).is_ok());
        let code = |entry: &serde_json::Value| GemCut::from_kit_entry(entry).err().map_or("ok", |e| e.code());

        let mut many = good.clone();
        let first = many["facets"][0].clone();
        let facets = many["facets"].as_array_mut().expect("facets");
        while facets.len() <= MAX_CUT_FACETS {
            facets.push(first.clone());
        }
        assert_eq!(code(&many), "material_missing_field");

        let mut far = good.clone();
        far["facets"][0]["offset_mm"] = serde_json::json!(1e300);
        assert_eq!(
            code(&far),
            "material_missing_field",
            "an offset past f32 is not a plane"
        );

        for size in [
            serde_json::json!([0.0]),
            serde_json::json!([-3.0, 3.0]),
            serde_json::json!([1e300]),
        ] {
            let mut bad = good.clone();
            bad["size_mm"] = size.clone();
            assert_eq!(code(&bad), "material_missing_field", "size {size}");
        }
    }

    #[test]
    fn non_finite_proportions_do_not_panic_on_the_way_back_to_json() {
        let mut entry = GemCut::new(CutType::RoundBrilliant, [3.0, 3.0])
            .to_kit_entry("r")
            .expect("convex");
        entry["proportions"]["table_pct"] = serde_json::json!(1e308);
        let back = GemCut::from_kit_entry(&entry).expect("reads");
        assert!(
            back.proportions
                .is_none_or(|p| p.table.is_finite() && p.crown_deg.is_finite()),
            "{:?}",
            back.proportions
        );
        let again = back.to_kit_entry("r").expect("rewrite");
        assert!(
            again["proportions"].is_null()
                || again["proportions"]["table_pct"]
                    .as_f64()
                    .is_some_and(f64::is_finite)
        );

        let mut programmed = GemCut::new(CutType::RoundBrilliant, [3.0, 3.0]);
        programmed.proportions = Some(Proportions {
            table: f32::INFINITY,
            ..Proportions::TOLKOWSKY
        });
        let written = programmed.to_kit_entry("inf").expect("no panic");
        let table = &written["proportions"]["table_pct"];
        assert!(
            table.is_null() || table.as_f64().is_some_and(f64::is_finite),
            "{table}"
        );
    }
}
