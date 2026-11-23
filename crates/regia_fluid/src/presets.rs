// SPDX-License-Identifier: MPL-2.0
//! Named starting scenes: `pour`, `dam_break`, `splash` and `stream`. Each is laid out in fractions
//! of its domain box, so a caller's bounds scale it.

use glam::Vec3;

use crate::sim::{Emitter, EmitterKind, FluidError, FluidParams, FluidSim, Shape};
use crate::solid::Collider;

/// The preset names.
pub const PRESETS: &[&str] = &["pour", "dam_break", "splash", "stream"];

/// A preset's default domain box and cell size.
#[must_use]
pub fn default_bounds(name: &str) -> Option<(Vec3, Vec3, f32)> {
    Some(match name {
        "pour" => (Vec3::ZERO, Vec3::new(1.0, 1.0, 0.5), 0.05),
        "dam_break" => (Vec3::ZERO, Vec3::new(1.6, 0.6, 0.1), 0.025),
        "splash" => (Vec3::ZERO, Vec3::new(1.0, 1.0, 1.0), 0.05),
        "stream" => (Vec3::ZERO, Vec3::new(2.0, 0.6, 0.4), 0.05),
        _ => return None,
    })
}

/// Builds preset `name`. `params` gives the bounds (`bounds_min`, `bounds_max`, `cell_size`) and the
/// rest of the settings; [`default_bounds`] has each preset's own.
pub fn build(name: &str, params: FluidParams) -> Result<FluidSim, FluidError> {
    let lo = params.bounds_min;
    let size = params.bounds_max - params.bounds_min;
    let at = |f: Vec3| lo + f * size;
    let half = |f: Vec3| f * size * 0.5;
    let fill = |center: Vec3, extent: Vec3| Emitter {
        shape: Shape::Box {
            center: at(center),
            half_extents: half(extent),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    };
    match name {
        // A source high in a tank pours a column down into a shallow pool.
        "pour" => {
            let mut sim = FluidSim::new(params)?;
            sim.add_emitter(fill(Vec3::new(0.5, 0.075, 0.5), Vec3::new(1.0, 0.15, 1.0)))?;
            sim.add_emitter(Emitter {
                shape: Shape::Sphere {
                    center: at(Vec3::new(0.3, 0.85, 0.5)),
                    radius: 0.1 * size.y,
                },
                velocity: Vec3::new(0.0, -1.0, 0.0),
                kind: EmitterKind::Source,
                done: false,
            })?;
            Ok(sim)
        }
        // A block of liquid against the left wall, released at once.
        "dam_break" => {
            let mut sim = FluidSim::new(params)?;
            sim.add_emitter(fill(
                Vec3::new(0.125, 1.0 / 3.0, 0.5),
                Vec3::new(0.25, 2.0 / 3.0, 1.0),
            ))?;
            Ok(sim)
        }
        // A ball of liquid dropped into a pool.
        "splash" => {
            let mut sim = FluidSim::new(params)?;
            sim.add_emitter(fill(Vec3::new(0.5, 0.15, 0.5), Vec3::new(1.0, 0.3, 1.0)))?;
            sim.add_emitter(Emitter {
                shape: Shape::Sphere {
                    center: at(Vec3::new(0.5, 0.75, 0.5)),
                    radius: 0.15 * size.min_element(),
                },
                velocity: Vec3::new(0.0, -2.0, 0.0),
                kind: EmitterKind::Fill,
                done: false,
            })?;
            Ok(sim)
        }
        // A source at one end runs down a terrain ramp into a sink at the other.
        "stream" => {
            let columns = 9;
            let rows = 2;
            let spacing = size.x / (columns - 1) as f32;
            let mut heights = Vec::with_capacity(columns * rows);
            for _ in 0..rows {
                for c in 0..columns {
                    heights.push(lo.y + size.y * (0.25 - 0.2 * c as f32 / (columns - 1) as f32));
                }
            }
            let ramp = Collider::Heightfield {
                origin: [lo.x, lo.z],
                spacing,
                columns,
                rows,
                heights,
            };
            let mut sim = FluidSim::with_colliders(params, vec![ramp])?;
            sim.add_emitter(Emitter {
                shape: Shape::Box {
                    center: at(Vec3::new(0.06, 0.45, 0.5)),
                    half_extents: half(Vec3::new(0.08, 0.2, 0.6)),
                },
                velocity: Vec3::new(1.0, 0.0, 0.0),
                kind: EmitterKind::Source,
                done: false,
            })?;
            sim.add_sink(Shape::Box {
                center: at(Vec3::new(0.97, 0.5, 0.5)),
                half_extents: half(Vec3::new(0.06, 1.0, 1.0)),
            })?;
            Ok(sim)
        }
        other => Err(FluidError::BadParam(format!(
            "unknown preset {other:?} (one of {})",
            PRESETS.join(", ")
        ))),
    }
}
