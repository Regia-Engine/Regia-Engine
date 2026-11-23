// SPDX-License-Identifier: MPL-2.0
//! Kill volumes (`.vfx.ron` v7, [`KillVolumeDef`]): one rule for the GPU update kernel
//! (`gpu/shaders.rs`, `in_kill_volume`), the CPU reference ([`crate::probe`]) and the CPU ribbon
//! simulation ([`crate::ribbon`]).
//!
//! The volume is resolved per instance to world space ([`resolve`]): a box becomes its centre and
//! three axis rows `axis / half_extent`, so a point is inside when every `|dot(p - c, row)| <= 1`;
//! a sphere becomes its centre and radius, inside when `|p - c|² <= r²`. A point on the boundary
//! is inside. The packed form is what the GPU emitter record carries ([`crate::EmitterGpu`]).

use regia_core::math::Vec3;

use crate::format::{KillShape, KillSide, KillVolumeDef};

/// Mode code: no volume.
pub const KILL_NONE: u32 = 0;
/// Mode code: a box that kills inside.
pub const KILL_BOX_INSIDE: u32 = 1;
/// Mode code: a box that kills outside.
pub const KILL_BOX_OUTSIDE: u32 = 2;
/// Mode code: a sphere that kills inside.
pub const KILL_SPHERE_INSIDE: u32 = 3;
/// Mode code: a sphere that kills outside.
pub const KILL_SPHERE_OUTSIDE: u32 = 4;

/// A kill volume resolved to world space, in the GPU record's layout.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ResolvedKillVolume {
    /// World centre (xyz) and the mode code (w, `KILL_*` as `f32`).
    pub volume: [f32; 4],
    /// Box: world axis rows divided by their half extent (xyz). Sphere: the radius in `[0][0]`.
    pub axes: [[f32; 4]; 3],
}

impl ResolvedKillVolume {
    /// The mode code (`KILL_*`).
    #[must_use]
    pub fn mode(&self) -> u32 {
        self.volume[3] as u32
    }

    /// Whether a particle at `p` dies: the GPU kernel's `in_kill_volume`, operation for operation.
    #[must_use]
    pub fn kills(&self, p: Vec3) -> bool {
        let mode = self.mode();
        if mode == KILL_NONE {
            return false;
        }
        let d = p - Vec3::new(self.volume[0], self.volume[1], self.volume[2]);
        let inside = if mode <= KILL_BOX_OUTSIDE {
            let row = |r: [f32; 4]| d.dot(Vec3::new(r[0], r[1], r[2])).abs();
            row(self.axes[0]).max(row(self.axes[1]).max(row(self.axes[2]))) <= 1.0
        } else {
            let r = self.axes[0][0];
            d.dot(d) <= r * r
        };
        if mode == KILL_BOX_OUTSIDE || mode == KILL_SPHERE_OUTSIDE {
            !inside
        } else {
            inside
        }
    }
}

/// Resolves `volume` for an instance at `origin` with axes `(right, up, forward)` and spatial
/// `scale`. `None` is [`KILL_NONE`].
#[must_use]
pub fn resolve(
    volume: Option<&KillVolumeDef>,
    origin: Vec3,
    axes: (Vec3, Vec3, Vec3),
    scale: f32,
) -> ResolvedKillVolume {
    let Some(volume) = volume else {
        return ResolvedKillVolume::default();
    };
    let (right, up, forward) = axes;
    let c = volume.center;
    let centre = origin + (right * c.0 + up * c.1 + forward * c.2) * scale;
    let outside = volume.kill == KillSide::Outside;
    let (mode, rows) = match volume.shape {
        KillShape::Box { half_extents: h } => {
            let row = |axis: Vec3, half: f32| {
                let v = axis / (half * scale).max(1e-6);
                [v.x, v.y, v.z, 0.0]
            };
            (
                if outside {
                    KILL_BOX_OUTSIDE
                } else {
                    KILL_BOX_INSIDE
                },
                [row(right, h.0), row(up, h.1), row(forward, h.2)],
            )
        }
        KillShape::Sphere { radius } => (
            if outside {
                KILL_SPHERE_OUTSIDE
            } else {
                KILL_SPHERE_INSIDE
            },
            [[radius * scale, 0.0, 0.0, 0.0], [0.0; 4], [0.0; 4]],
        ),
    };
    ResolvedKillVolume {
        volume: [centre.x, centre.y, centre.z, mode as f32],
        axes: rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axes() -> (Vec3, Vec3, Vec3) {
        (Vec3::X, Vec3::Y, Vec3::Z)
    }

    #[test]
    fn box_and_sphere_sides() {
        let boxed = KillVolumeDef {
            shape: KillShape::Box {
                half_extents: (1.0, 2.0, 3.0),
            },
            center: (0.0, 1.0, 0.0),
            kill: KillSide::Inside,
        };
        let k = resolve(Some(&boxed), Vec3::new(10.0, 0.0, 0.0), axes(), 1.0);
        assert!(k.kills(Vec3::new(10.0, 1.0, 0.0)));
        assert!(k.kills(Vec3::new(11.0, 3.0, 3.0)), "the faces are inside");
        assert!(!k.kills(Vec3::new(11.1, 1.0, 0.0)));
        let outside = KillVolumeDef {
            kill: KillSide::Outside,
            ..boxed.clone()
        };
        let k = resolve(Some(&outside), Vec3::new(10.0, 0.0, 0.0), axes(), 1.0);
        assert!(!k.kills(Vec3::new(10.0, 1.0, 0.0)));
        assert!(k.kills(Vec3::new(11.1, 1.0, 0.0)));
        let sphere = KillVolumeDef {
            shape: KillShape::Sphere { radius: 2.0 },
            center: (0.0, 0.0, 0.0),
            kill: KillSide::Inside,
        };
        let k = resolve(Some(&sphere), Vec3::ZERO, axes(), 2.0);
        assert!(k.kills(Vec3::new(0.0, 3.9, 0.0)), "scaled radius 4");
        assert!(!k.kills(Vec3::new(0.0, 4.1, 0.0)));
        assert!(!resolve(None, Vec3::ZERO, axes(), 1.0).kills(Vec3::ZERO));
    }

    #[test]
    fn a_box_turns_with_the_instance() {
        let boxed = KillVolumeDef {
            shape: KillShape::Box {
                half_extents: (0.5, 0.5, 4.0),
            },
            center: (0.0, 0.0, 4.0),
            kill: KillSide::Inside,
        };
        // Forward along world +X: the long axis runs along X.
        let k = resolve(Some(&boxed), Vec3::ZERO, (Vec3::NEG_Z, Vec3::Y, Vec3::X), 1.0);
        assert!(k.kills(Vec3::new(7.5, 0.0, 0.0)));
        assert!(!k.kills(Vec3::new(0.0, 0.0, 4.0)));
    }
}
