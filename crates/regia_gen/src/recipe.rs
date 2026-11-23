// SPDX-License-Identifier: MPL-2.0
//! A serializable mesh recipe: one base shape plus a list of operations. This is the form the
//! agent method `gen.mesh` takes (`book/agents/gen-mesh.md`); the Rust functions are the same
//! operations without the limits.

use serde::{Deserialize, Serialize};

use crate::{
    AnnulusOptions, Bevel, CpuMesh, CylinderOptions, ExtrudeOptions, Frame, GenError, LatheOptions, Plane,
    Profile, SweepRect, TubeOptions, annulus, clip, cylinder, extrude, intersect_convex, lathe, merge,
    rounded_box, rounded_rect, scale, sweep_rounded_rect, translate, tube, uv_box, uv_planar,
};

use crate::uv::Axis;

/// One base shape and the operations applied to it in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// The base shape.
    pub shape: Shape,
    /// Operations, applied in order.
    #[serde(default)]
    pub ops: Vec<Op>,
}

/// A base shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Shape {
    /// [`extrude`] of a profile.
    Extrude {
        /// The outer ring, `[x, y]` points.
        profile: Vec<[f32; 2]>,
        /// Holes inside it.
        #[serde(default)]
        holes: Vec<Vec<[f32; 2]>>,
        /// Length along +Z.
        depth: f32,
        /// Optional cap bevel.
        #[serde(default)]
        bevel: Option<Bevel>,
    },
    /// [`lathe`] of an `[r, y]` profile around +Y.
    Lathe {
        /// The profile, bottom to top.
        profile: Vec<[f32; 2]>,
        /// Facets around the axis.
        segments: u32,
    },
    /// A box `size` centered on the origin. With a bevel every edge is rounded: the vertical
    /// edges by a profile corner radius of twice the bevel width, the cap edges by the bevel.
    Box {
        /// Full extents `[x, y, z]`.
        size: [f32; 3],
        /// Optional bevel.
        #[serde(default)]
        bevel: Option<Bevel>,
    },
    /// [`rounded_box`]: half extents, corner radius, face segments. Centered, local +Z along world +Z.
    RoundedBox {
        /// Half extents `[x, y, z]`, metres.
        half: [f32; 3],
        /// Corner radius, metres, in `[0, min(half)]`.
        radius: f32,
        /// Quads along each face edge.
        segments: u32,
        /// UV repeats (default 1).
        #[serde(default = "one")]
        tile: f32,
    },
    /// [`cylinder`] along +Z, base on the origin.
    Cylinder {
        /// Wall radius, metres.
        radius: f32,
        /// Length along +Z, metres.
        length: f32,
        /// Facets around the axis.
        sides: u32,
        /// Odd-facet inset, in `[0, 1)`. Default 0 (smooth).
        #[serde(default)]
        knurl: f32,
        /// 45° inset at +Z. Default 0. Clamped to `min(length / 2, radius / 2)`.
        #[serde(default)]
        chamfer: f32,
        /// Close the +Z end (default true).
        #[serde(default = "yes")]
        cap: bool,
        /// Close the base at the origin (default false).
        #[serde(default)]
        bottom_cap: bool,
        /// UV repeats (default 1).
        #[serde(default = "one")]
        tile: f32,
    },
    /// [`annulus`] in the XY plane, normal +Z. `inner: 0` is a disk.
    Annulus {
        /// Inner radius, metres.
        inner: f32,
        /// Outer radius, metres.
        outer: f32,
        /// Facets around the ring.
        sides: u32,
        /// UV repeats (default 1).
        #[serde(default = "one")]
        tile: f32,
    },
    /// A [`tube`] when `radius` is set, or [`sweep_rounded_rect`] when `half` is set.
    /// Set exactly one of the two.
    Sweep {
        /// Path points, at least two.
        points: Vec<[f32; 3]>,
        /// Tube radius. Omit when `half` is set.
        #[serde(default)]
        radius: Option<f32>,
        /// Rounded-rectangle half extents `(across, out)`. Omit when `radius` is set.
        #[serde(default)]
        half: Option<[f32; 2]>,
        /// Corner radius of the rectangle (default 0).
        #[serde(default)]
        corner: f32,
        /// Slide along `out`, metres (default 0).
        #[serde(default)]
        shift: f32,
        /// Section up axis (default `[0, 1, 0]`).
        #[serde(default = "up_axis")]
        out: [f32; 3],
        /// Tube facets, or corner facets of the rectangle (default 16).
        #[serde(default = "sixteen")]
        sides: u32,
        /// Samples per path span (default 4).
        #[serde(default = "four")]
        steps: u32,
        /// UV repeats (default 1).
        #[serde(default = "one")]
        tile: f32,
    },
}

/// An operation on the mesh built so far.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    /// [`clip`]: keep the part behind the plane.
    Clip {
        /// The plane.
        plane: Plane,
        /// Close the cut (default true).
        #[serde(default = "yes")]
        cap: bool,
    },
    /// [`intersect_convex`]: keep the part inside every plane.
    Intersect {
        /// The convex solid's planes.
        planes: Vec<Plane>,
    },
    /// [`uv_planar`].
    UvPlanar {
        /// Projection axis.
        axis: Axis,
        /// UV units per metre (default 1).
        #[serde(default = "one")]
        scale: f32,
        /// Added after scaling (default 0).
        #[serde(default)]
        offset: [f32; 2],
    },
    /// [`uv_box`].
    UvBox {
        /// UV units per metre (default 1).
        #[serde(default = "one")]
        scale: f32,
        /// Added after scaling (default 0).
        #[serde(default)]
        offset: [f32; 2],
    },
    /// [`translate`].
    Translate {
        /// The offset.
        by: [f32; 3],
    },
    /// [`scale`].
    Scale {
        /// Per-axis factors.
        by: [f32; 3],
    },
    /// [`merge`] with another recipe's mesh (a union of solids that do not overlap).
    Merge {
        /// The other part.
        recipe: Box<Recipe>,
    },
}

fn yes() -> bool {
    true
}

fn one() -> f32 {
    1.0
}

fn sixteen() -> u32 {
    16
}

fn four() -> u32 {
    4
}

fn up_axis() -> [f32; 3] {
    [0.0, 1.0, 0.0]
}

/// Size limits of [`build_recipe`] (an agent call is bounded by them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecipeLimits {
    /// Most points in one profile ring.
    pub ring_points: usize,
    /// Most holes in one profile.
    pub holes: usize,
    /// Most lathe or bevel segments.
    pub segments: u32,
    /// Most operations in one recipe, counting nested ones.
    pub ops: usize,
    /// Deepest `merge` nesting.
    pub depth: usize,
    /// Most vertices of the result (checked after each step).
    pub vertices: usize,
}

impl Default for RecipeLimits {
    /// The agent's limits: 1024 points per ring, 64 holes, 256 segments, 64 operations, merge
    /// nesting 4, 1 000 000 vertices.
    fn default() -> Self {
        Self {
            ring_points: 1024,
            holes: 64,
            segments: 256,
            ops: 64,
            depth: 4,
            vertices: 1_000_000,
        }
    }
}

fn too_large(what: &'static str, value: usize, limit: usize) -> Result<(), GenError> {
    if value > limit {
        Err(GenError::TooLarge { what, value, limit })
    } else {
        Ok(())
    }
}

fn base_mesh(shape: &Shape, limits: &RecipeLimits) -> Result<CpuMesh, GenError> {
    let segments = |s: u32| too_large("segments", s as usize, limits.segments as usize);
    let bevel_ok = |b: &Option<Bevel>| b.map_or(Ok(()), |b| segments(b.segments));
    match shape {
        Shape::Extrude {
            profile,
            holes,
            depth,
            bevel,
        } => {
            too_large("holes", holes.len(), limits.holes)?;
            for ring in std::iter::once(profile).chain(holes) {
                too_large("ring points", ring.len(), limits.ring_points)?;
            }
            bevel_ok(bevel)?;
            let profile = Profile {
                outer: profile.clone(),
                holes: holes.clone(),
            };
            extrude(
                &profile,
                &ExtrudeOptions {
                    depth: *depth,
                    bevel: *bevel,
                },
            )
        }
        Shape::Lathe { profile, segments: n } => {
            too_large("ring points", profile.len(), limits.ring_points)?;
            segments(*n)?;
            lathe(profile, &LatheOptions { segments: *n })
        }
        Shape::Box { size, bevel } => {
            bevel_ok(bevel)?;
            crate::finite("box size", size)?;
            let (radius, k) = bevel.map_or((0.0, 1), |b| (2.0 * b.width, b.segments));
            let outline = rounded_rect([size[0], size[1]], radius, k)?;
            let body = extrude(
                &Profile::new(outline),
                &ExtrudeOptions {
                    depth: size[2],
                    bevel: *bevel,
                },
            )?;
            Ok(translate(&body, [0.0, 0.0, -size[2] / 2.0]))
        }
        Shape::RoundedBox {
            half,
            radius,
            segments: n,
            tile,
        } => {
            segments(*n)?;
            rounded_box(&Frame::xyz(), *half, *radius, *n, *tile)
        }
        Shape::Cylinder {
            radius,
            length,
            sides,
            knurl,
            chamfer,
            cap,
            bottom_cap,
            tile,
        } => {
            segments(*sides)?;
            cylinder(
                &Frame::xyz(),
                &CylinderOptions {
                    radius: *radius,
                    length: *length,
                    sides: *sides,
                    knurl: *knurl,
                    chamfer: *chamfer,
                    cap: *cap,
                    bottom_cap: *bottom_cap,
                    tile: *tile,
                },
            )
        }
        Shape::Annulus {
            inner,
            outer,
            sides,
            tile,
        } => {
            segments(*sides)?;
            annulus(
                &Frame::xyz(),
                &AnnulusOptions {
                    inner: *inner,
                    outer: *outer,
                    sides: *sides,
                    tile: *tile,
                },
            )
        }
        Shape::Sweep {
            points,
            radius,
            half,
            corner,
            shift,
            out,
            sides,
            steps,
            tile,
        } => sweep_mesh(
            SweepIn {
                points,
                radius: *radius,
                half: *half,
                corner: *corner,
                shift: *shift,
                out: *out,
                sides: *sides,
                steps: *steps,
                tile: *tile,
            },
            limits,
        ),
    }
}

struct SweepIn<'a> {
    points: &'a [[f32; 3]],
    radius: Option<f32>,
    half: Option<[f32; 2]>,
    corner: f32,
    shift: f32,
    out: [f32; 3],
    sides: u32,
    steps: u32,
    tile: f32,
}

fn sweep_mesh(sweep: SweepIn<'_>, limits: &RecipeLimits) -> Result<CpuMesh, GenError> {
    too_large("ring points", sweep.points.len(), limits.ring_points)?;
    too_large("segments", sweep.sides as usize, limits.segments as usize)?;
    too_large("segments", sweep.steps as usize, limits.segments as usize)?;
    match (sweep.radius, sweep.half) {
        (Some(radius), None) => tube(
            sweep.points,
            &TubeOptions {
                radius,
                sides: sweep.sides,
                steps_per_segment: sweep.steps,
                tile: sweep.tile,
            },
        ),
        (None, Some(half)) => sweep_rounded_rect(
            sweep.points,
            &SweepRect {
                half,
                corner: sweep.corner,
                shift: sweep.shift,
                out: sweep.out,
                corner_segments: sweep.sides,
                steps: sweep.steps,
                tile: sweep.tile,
            },
        ),
        _ => Err(GenError::InvalidInput(
            "sweep needs exactly one of radius (a tube) or half (a rounded rectangle)".into(),
        )),
    }
}

/// Builds `recipe` within `limits`.
pub fn build_recipe(recipe: &Recipe, limits: &RecipeLimits) -> Result<CpuMesh, GenError> {
    let _span = tracing::trace_span!("regia_gen::build_recipe").entered();
    let mut ops = 0usize;
    build(recipe, limits, 0, &mut ops)
}

fn build(recipe: &Recipe, limits: &RecipeLimits, depth: usize, ops: &mut usize) -> Result<CpuMesh, GenError> {
    too_large("merge nesting", depth, limits.depth)?;
    *ops += recipe.ops.len();
    too_large("operation count", *ops, limits.ops)?;
    let mut mesh = base_mesh(&recipe.shape, limits)?;
    too_large("vertex count", mesh.positions.len(), limits.vertices)?;
    for op in &recipe.ops {
        mesh = match op {
            Op::Clip { plane, cap } => clip(&mesh, *plane, *cap)?,
            Op::Intersect { planes } => {
                too_large("planes", planes.len(), limits.ops)?;
                intersect_convex(&mesh, planes)?
            }
            Op::UvPlanar {
                axis,
                scale: s,
                offset,
            } => uv_planar(&mesh, *axis, *s, *offset)?,
            Op::UvBox { scale: s, offset } => uv_box(&mesh, *s, *offset)?,
            Op::Translate { by } => {
                crate::finite("translation", by)?;
                translate(&mesh, *by)
            }
            Op::Scale { by } => {
                crate::finite("scale", by)?;
                if by.contains(&0.0) {
                    return Err(GenError::InvalidInput("a scale factor is zero".into()));
                }
                scale(&mesh, *by)
            }
            Op::Merge { recipe } => {
                let other = build(recipe, limits, depth + 1, ops)?;
                merge(&[&mesh, &other])
            }
        };
        too_large("vertex count", mesh.positions.len(), limits.vertices)?;
    }
    Ok(mesh)
}
