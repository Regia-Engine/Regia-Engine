// SPDX-License-Identifier: MPL-2.0
//! Painted points (spec 67 § 5.2): points an author stamps onto surfaces with the editor's Paint
//! tool, saved on a generator entity and read by `in.self` as a point set tagged `painted`.
//!
//! Positions and normals are world space (the tool writes surface hits); moving the entity does
//! not move them. `layer` is zero based (the tool's digits 1 to 8 write layers 0 to 7) and reaches
//! graphs as the integer attribute `layer`, so a graph filters layers with an ordinary attribute
//! filter. `yaw` (version 2, ADR-7215) turns a point about its normal; the Paint brush draws it at
//! random from its seed, and a version 1 point reads as yaw 0 (the rotation it always had).

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_ecs::reflect::Reflect;

use crate::data::{AttrColumn, Point, PointSet};
use regia_scene::spawn::ReflectFormatVersion;

/// `format_version` of the saved [`PaintedPoints`] component (`procgen-painted-points`). Version 2
/// (ADR-7215) adds [`PaintedPoint::yaw`]; version 1 points migrate with yaw 0.
pub const PAINTED_POINTS_FORMAT_VERSION: u32 = 2;

/// Most named layers (digits 1 to 8 in the tool).
pub const MAX_PAINT_LAYERS: usize = 8;

/// One painted point.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Default)]
#[reflect(Default)]
pub struct PaintedPoint {
    /// World position (a surface hit).
    pub position: Vec3,
    /// World unit surface normal at the hit.
    pub normal: Vec3,
    /// Zero-based layer, `< MAX_PAINT_LAYERS`.
    pub layer: u8,
    /// Turn about the normal, radians (0 = the bare normal rotation; version 2).
    pub yaw: f32,
}

/// Points painted onto surfaces for a generator (saved, `procgen-painted-points` v1).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, FormatVersion)]
pub struct PaintedPoints {
    /// The points, in paint order.
    pub points: Vec<PaintedPoint>,
    /// Display names of the layers (at most [`MAX_PAINT_LAYERS`]; may be shorter).
    pub layer_names: Vec<String>,
    /// Always [`PAINTED_POINTS_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for PaintedPoints {
    const CURRENT: u32 = PAINTED_POINTS_FORMAT_VERSION;
}

impl Default for PaintedPoints {
    fn default() -> Self {
        Self {
            points: Vec::new(),
            layer_names: Vec::new(),
            format_version: PAINTED_POINTS_FORMAT_VERSION,
        }
    }
}

impl PaintedPoints {
    /// The points as a point set: positions, rotations that turn +Y onto the normal after a turn of
    /// `yaw` about +Y, density 1, seeds from the position and paint index (spec 62 § 6.2), and the
    /// integer attribute `layer`. A zero yaw leaves the normal rotation bit for bit as version 1 had it.
    pub fn to_point_set(&self) -> PointSet {
        let mut set = PointSet::new();
        let mut layers = Vec::with_capacity(self.points.len());
        for (i, p) in self.points.iter().enumerate() {
            let normal = if p.normal.length_squared() > 1e-12 {
                p.normal.normalize()
            } else {
                Vec3::Y
            };
            let position = p.position.to_array();
            let tilt = crate::nodes::common::rotation_between(Vec3::Y, normal);
            let rotation = if p.yaw != 0.0 && p.yaw.is_finite() {
                (tilt * crate::nodes::common::axis_angle(Vec3::Y, f64::from(p.yaw))).normalize()
            } else {
                tilt
            };
            set.push(Point {
                position,
                rotation: rotation.to_array(),
                seed: crate::seeds::point_seed(position, i as u32),
                ..Point::default()
            });
            layers.push(i64::from(p.layer));
        }
        set.attrs
            .insert("layer".to_string(), AttrColumn::Int(layers.into()));
        set
    }

    /// Upgrades an older saved shape. Version 1 had no `yaw`; a missing field reads as 0, the
    /// rotation a version 1 point always had, so the step only sets the version.
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        if self.format_version > PAINTED_POINTS_FORMAT_VERSION {
            return Err(format!(
                "procgen-painted-points format_version {} is newer than this build ({PAINTED_POINTS_FORMAT_VERSION})",
                self.format_version
            ));
        }
        self.format_version = PAINTED_POINTS_FORMAT_VERSION;
        Ok(())
    }
}

/// The shape fingerprint of the saved `procgen-painted-points` component (`cargo xtask
/// check-formats`): field names from reflection.
pub fn painted_points_schema_fingerprint() -> String {
    use regia_ecs::reflect::{TypeInfo, Typed};
    fn names(info: &TypeInfo) -> String {
        match info {
            TypeInfo::Struct(s) => s.iter().map(|f| f.name()).collect::<Vec<_>>().join(","),
            _ => String::new(),
        }
    }
    format!(
        "PaintedPoints({});PaintedPoint({})",
        names(PaintedPoints::type_info()),
        names(PaintedPoint::type_info())
    )
}

/// Registers the types a saved [`PaintedPoints`] component reads (`regia upgrade-project` decodes it
/// on its own, outside a running app).
pub fn register_painted_points_types(registry: &mut regia_ecs::reflect::TypeRegistry) {
    registry.register::<PaintedPoints>();
    registry.register::<PaintedPoint>();
    registry.register::<Vec<PaintedPoint>>();
    registry.register::<Vec<String>>();
    registry.register::<Vec3>();
}

/// `procgen-painted-points` migration sources: version 1 (no `yaw`, [`PaintedPoints::migrate`]).
pub fn painted_points_migration_sources() -> Vec<u32> {
    vec![1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_become_a_set_with_a_layer_attribute() {
        let painted = PaintedPoints {
            points: vec![
                PaintedPoint {
                    position: Vec3::new(1.0, 2.0, 3.0),
                    normal: Vec3::Y,
                    layer: 2,
                    yaw: 0.0,
                },
                PaintedPoint {
                    position: Vec3::new(-1.0, 0.0, 0.5),
                    normal: Vec3::X,
                    layer: 0,
                    yaw: 0.0,
                },
            ],
            ..PaintedPoints::default()
        };
        let set = painted.to_point_set();
        assert_eq!(set.len(), 2);
        assert_eq!(set.position[0], [1.0, 2.0, 3.0]);
        assert_eq!(set.attrs.get("layer"), Some(&AttrColumn::Int(vec![2, 0].into())));
        assert_ne!(set.seed[0], set.seed[1]);
        assert!(painted_points_schema_fingerprint().contains("layer_names"));
        let mut newer = PaintedPoints {
            format_version: 9,
            ..PaintedPoints::default()
        };
        assert!(newer.migrate().is_err());
    }

    /// Pins the scalar `rotation_between` result bit for bit (x86_64 == aarch64).
    /// `audit-g-procgen2-determinism` finding 2 was a non-bug: the `Quat` on this path is
    /// `crate::rot::Rot`, whose `normalize` already was this exact scalar code, so this test
    /// passes unchanged on the pre-"fix" code too — it is a determinism pin, not a fail-before
    /// regression test. `y` is negative zero: the cross product's y lane
    /// is `0.0 * to.x - 0.0 * to.z` = `-0.0 - +0.0` = `-0.0` under IEEE sign-of-zero rules, which
    /// every architecture reproduces bit for bit.
    #[test]
    fn point_set_rotation_bits_are_the_scalar_result_on_every_architecture() {
        let normal = Vec3::new(
            f32::from_bits(0xbf4a62de),
            f32::from_bits(0x3f0c9b1a),
            f32::from_bits(0x3e8aa5e2),
        );
        let painted = PaintedPoints {
            points: vec![PaintedPoint {
                position: Vec3::new(0.0, 0.0, 0.0),
                normal,
                layer: 0,
                yaw: 0.0,
            }],
            ..PaintedPoints::default()
        };
        let set = painted.to_point_set();
        let bits: Vec<u32> = set.rotation[0].iter().map(|v| v.to_bits()).collect();
        assert_eq!(
            bits,
            vec![0x3e1d8835, 0x8000_0000, 0x3ee5f397, 0x3f614fe6],
            "rotation bits for {normal:?}"
        );
    }

    /// Version 2 (ADR-7215): a yaw turns the point about its normal; a version 1 component
    /// migrates to version 2 unchanged (its points read as yaw 0).
    #[test]
    fn yaw_turns_about_the_normal_and_version_1_migrates() {
        let painted = PaintedPoints {
            points: vec![
                PaintedPoint {
                    position: Vec3::ZERO,
                    normal: Vec3::Y,
                    layer: 0,
                    yaw: std::f32::consts::FRAC_PI_2,
                },
                PaintedPoint {
                    position: Vec3::X,
                    normal: Vec3::Y,
                    layer: 0,
                    yaw: 0.0,
                },
            ],
            ..PaintedPoints::default()
        };
        let set = painted.to_point_set();
        let turned = crate::rot::Rot(set.rotation[0]) * Vec3::Z;
        assert!((turned - Vec3::X).abs().max_element() < 1e-5, "{turned:?}");
        assert_eq!(
            set.rotation[1],
            [0.0, 0.0, 0.0, 1.0],
            "yaw 0 is the bare normal rotation"
        );
        let mut v1 = PaintedPoints {
            format_version: 1,
            ..painted
        };
        v1.migrate().expect("v1 migrates");
        assert_eq!(v1.format_version, PAINTED_POINTS_FORMAT_VERSION);
        assert_eq!(painted_points_migration_sources(), vec![1]);
    }
}
