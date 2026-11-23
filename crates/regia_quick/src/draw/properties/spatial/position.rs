// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/properties/spatial/position.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
//! Items related to describing positioning along each axis as
use crate::prelude_internal::*;

use crate::draw::{Draw, drawing};

/// Position properties for **Drawing** a **Primitive**.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Properties {
    /// The position.
    pub point: Vec3,
}

/// An API for setting the **position::Properties**.
pub trait SetPosition: Sized {
    /// Provide a mutable reference to the **position::Properties** for updating.
    fn properties(&mut self) -> &mut Properties;

    /// Build with the given **Absolute** **Position** along the *x* axis.
    fn x(mut self, x: f32) -> Self {
        self.properties().point.x = x;
        self
    }

    /// Build with the given **Absolute** **Position** along the *y* axis.
    fn y(mut self, y: f32) -> Self {
        self.properties().point.y = y;
        self
    }

    /// Build with the given **Absolute** **Position** along the *z* axis.
    fn z(mut self, z: f32) -> Self {
        self.properties().point.z = z;
        self
    }

    /// Set the **Position** with some two-dimensional point.
    fn xy(self, p: Vec2) -> Self {
        self.x(p.x).y(p.y)
    }

    /// Set the **Position** with some three-dimensional point.
    fn xyz(self, p: Vec3) -> Self {
        self.x(p.x).y(p.y).z(p.z)
    }

    /// Set the **Position** with *x* *y* coordinates.
    fn x_y(self, x: f32, y: f32) -> Self {
        self.xy([x, y].into())
    }

    /// Set the **Position** with *x* *y* *z* coordinates.
    fn x_y_z(self, x: f32, y: f32, z: f32) -> Self {
        self.xyz([x, y, z].into())
    }
}

impl Properties {
    /// The local transform these properties describe.
    pub fn transform(&self) -> Mat4 {
        Mat4::from_translation(self.point)
    }
}

impl SetPosition for Properties {
    fn properties(&mut self) -> &mut Properties {
        self
    }
}

impl Default for Properties {
    fn default() -> Self {
        let point = Vec3::ZERO;
        Self { point }
    }
}

// Set the position of the primitive being drawn at `index`.
//
// `None` leaves the corresponding axis unchanged.
pub(crate) fn set_position(draw: &Draw, index: usize, x: Option<f32>, y: Option<f32>, z: Option<f32>) {
    drawing::with_primitive(draw, index, |prim| match prim.position_mut() {
        Some(props) => {
            if let Some(x) = x {
                props.point.x = x;
            }
            if let Some(y) = y {
                props.point.y = y;
            }
            if let Some(z) = z {
                props.point.z = z;
            }
        }
        None => crate::warn_once!("drawing primitive does not support `position`"),
    })
}
