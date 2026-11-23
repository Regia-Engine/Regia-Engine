// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/properties/spatial/orientation.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
// MIT OR Apache-2.0, used here under the MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
use crate::prelude_internal::*;

use crate::draw::{Draw, drawing};

/// Orientation properties for **Drawing** a **Primitive**.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Properties {
    /// The orientation described by an angle along each axis.
    Axes(Vec3),
    /// The orientation described by looking at some other point.
    LookAt(Vec3),
    /// Angle described by quarternion.
    Quat(Quat),
}

impl Properties {
    /// The local transform these properties describe.
    pub fn transform(&self) -> Mat4 {
        match *self {
            Properties::Axes(v) => Mat4::from_euler(EulerRot::XYZ, v.x, v.y, v.z),
            Properties::LookAt(p) => {
                let eye = Vec3::ZERO;
                let dir = p - eye;
                // A zero or non-finite target has no direction; a vertical one needs another up.
                // (A finite direction whose square overflows has no usable direction either.)
                let len_sq = dir.length_squared();
                if !dir.is_finite() || !len_sq.is_finite() || len_sq < 1e-12 {
                    return Mat4::IDENTITY;
                }
                let up = if dir.normalize().cross(Vec3::Y).length_squared() < 1e-8 {
                    Vec3::Z
                } else {
                    Vec3::Y
                };
                <Mat4 as regia_core::math::geom::Mat4LookTo>::look_to_rh(eye, p - eye, up)
            }
            Properties::Quat(q) => Mat4::from_quat(q),
        }
    }

    /// If the `Properties` was set to the `LookAt` variant, this method switches to the `Axes`
    /// variant.
    ///
    /// If the `Properties` is already `Axes`, nothing changes.
    pub fn switch_to_axes(&mut self) {
        match *self {
            Properties::Axes(_) => {}
            Properties::LookAt(_) => *self = Properties::Axes(Vec3::ZERO),
            // Keep the rotation so a following axis setter adjusts it rather than panicking.
            Properties::Quat(q) => {
                let (x, y, z) = q.to_euler(EulerRot::XYZ);
                *self = Properties::Axes(Vec3::new(x, y, z));
            }
        }
    }
}

/// An API for setting the **orientation::Properties**.
pub trait SetOrientation: Sized {
    /// Provide a mutable reference to the **orientation::Properties** for updating.
    fn properties(&mut self) -> &mut Properties;

    // Describing orientation via a target.

    /// Describe orientation via the vector that points to the given target.
    ///
    /// A zero or non-finite target gives no rotation; a target straight along `Y` uses `Z` as up.
    fn look_at(mut self, target: Vec3) -> Self {
        *self.properties() = Properties::LookAt(target);
        self
    }

    // Absolute orientation.

    /// Specify the orientation around the *x* axis as an absolute value in radians.
    fn x_radians(mut self, x: f32) -> Self {
        self.properties().switch_to_axes();
        expect_axes(self.properties()).x = x;
        self
    }

    /// Specify the orientation around the *y* axis as an absolute value in radians.
    fn y_radians(mut self, y: f32) -> Self {
        self.properties().switch_to_axes();
        expect_axes(self.properties()).y = y;
        self
    }

    /// Specify the orientation around the *z* axis as an absolute value in radians.
    fn z_radians(mut self, z: f32) -> Self {
        self.properties().switch_to_axes();
        expect_axes(self.properties()).z = z;
        self
    }

    /// Specify the orientation around the *x* axis as an absolute value in degrees.
    fn x_degrees(self, x: f32) -> Self {
        self.x_radians(x.to_radians())
    }

    /// Specify the orientation around the *y* axis as an absolute value in degrees.
    fn y_degrees(self, y: f32) -> Self {
        self.y_radians(y.to_radians())
    }

    /// Specify the orientation around the *z* axis as an absolute value in degrees.
    fn z_degrees(self, z: f32) -> Self {
        self.z_radians(z.to_radians())
    }

    /// Specify the orientation around the *x* axis as a number of turns around the axis.
    fn x_turns(self, x: f32) -> Self {
        self.x_radians(x * std::f32::consts::TAU)
    }

    /// Specify the orientation around the *y* axis as a number of turns around the axis.
    fn y_turns(self, y: f32) -> Self {
        self.y_radians(y * std::f32::consts::TAU)
    }

    /// Specify the orientation around the *z* axis as a number of turns around the axis.
    fn z_turns(self, z: f32) -> Self {
        self.z_radians(z * std::f32::consts::TAU)
    }

    /// Specify the orientation along each axis with the given **Vector** of radians.
    ///
    /// This has the same affect as calling `self.x_radians(v.x).y_radians(v.y).z_radians(v.z)`.
    fn radians(self, v: Vec3) -> Self {
        self.x_radians(v.x).y_radians(v.y).z_radians(v.z)
    }

    /// Specify the orientation along each axis with the given **Vector** of degrees.
    ///
    /// This has the same affect as calling `self.x_degrees(v.x).y_degrees(v.y).z_degrees(v.z)`.
    fn degrees(self, v: Vec3) -> Self {
        self.x_degrees(v.x).y_degrees(v.y).z_degrees(v.z)
    }

    /// Specify the orientation along each axis with the given **Vector** of "turns".
    ///
    /// This has the same affect as calling `self.x_turns(v.x).y_turns(v.y).z_turns(v.z)`.
    fn turns(self, v: Vec3) -> Self {
        self.x_turns(v.x).y_turns(v.y).z_turns(v.z)
    }

    /// Specify the orientation with the given euler orientation in radians.
    fn euler(self, e: Vec3) -> Self {
        self.radians(e)
    }

    /// Specify the orientation with the given **Quaternion**.
    fn quaternion(mut self, q: Quat) -> Self {
        *self.properties() = Properties::Quat(q);
        self
    }

    // Higher level methods.

    /// Specify the "pitch" of the orientation in radians.
    ///
    /// This has the same effect as calling `x_radians`.
    fn pitch(self, pitch: f32) -> Self {
        self.x_radians(pitch)
    }

    /// Specify the "yaw" of the orientation in radians.
    ///
    /// This has the same effect as calling `y_radians`.
    fn yaw(self, yaw: f32) -> Self {
        self.y_radians(yaw)
    }

    /// Specify the "roll" of the orientation in radians.
    ///
    /// This has the same effect as calling `z_radians`.
    fn roll(self, roll: f32) -> Self {
        self.z_radians(roll)
    }

    /// Assuming we're looking at a 2D plane, positive values cause a counter-clockwise rotation where the
    /// given value is specified in radians.
    ///
    /// This is equivalent to calling the `z_radians` or `roll` methods.
    fn rotate(self, radians: f32) -> Self {
        self.z_radians(radians)
    }
}

impl SetOrientation for Properties {
    fn properties(&mut self) -> &mut Properties {
        self
    }
}

impl Default for Properties {
    fn default() -> Self {
        Properties::Axes(Vec3::ZERO)
    }
}

// Expects the `Axes` variant from the given properties.
fn expect_axes(p: &mut Properties) -> &mut Vec3 {
    p.switch_to_axes();
    match p {
        Properties::Axes(axes) => axes,
        // `switch_to_axes` leaves only `Axes`.
        Properties::LookAt(_) | Properties::Quat(_) => unreachable!("switch_to_axes leaves `Axes`"),
    }
}

// An update to a primitive's orientation properties.
pub(crate) enum Update {
    LookAt(Vec3),
    XRadians(f32),
    YRadians(f32),
    ZRadians(f32),
    Radians(Vec3),
    Quat(Quat),
}

// Update the orientation of the primitive being drawn at `index`.
pub(crate) fn set_orientation(draw: &Draw, index: usize, update: Update) {
    drawing::with_primitive(draw, index, |prim| match prim.orientation_mut() {
        Some(props) => apply_update(props, update),
        None => crate::warn_once!("drawing primitive does not support `orientation`"),
    })
}

fn apply_update(props: &mut Properties, update: Update) {
    match update {
        Update::LookAt(target) => *props = Properties::LookAt(target),
        Update::XRadians(x) => *props = SetOrientation::x_radians(*props, x),
        Update::YRadians(y) => *props = SetOrientation::y_radians(*props, y),
        Update::ZRadians(z) => *props = SetOrientation::z_radians(*props, z),
        Update::Radians(v) => *props = SetOrientation::radians(*props, v),
        Update::Quat(q) => *props = Properties::Quat(q),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn look_at_degenerate_targets_stay_finite() {
        for target in [
            Vec3::ZERO,
            Vec3::Y,
            -Vec3::Y * 3.0,
            Vec3::new(f32::NAN, 0.0, 0.0),
            Vec3::new(1e20, 0.0, 0.0),
            Vec3::new(f32::INFINITY, 0.0, 0.0),
        ] {
            let m = Properties::LookAt(target).transform();
            assert!(m.to_cols_array().iter().all(|v| v.is_finite()), "{target:?}");
        }
    }
}
