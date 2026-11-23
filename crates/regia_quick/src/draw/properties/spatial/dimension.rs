// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/properties/spatial/dimension.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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

/// Dimension properties for **Drawing** a **Primitive**.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Properties {
    /// Width (primitive default when unset).
    pub x: Option<f32>,
    /// Height (primitive default when unset).
    pub y: Option<f32>,
    /// Depth (unused by 2D primitives).
    pub z: Option<f32>,
}

/// Primitives that support different dimensions.
pub trait SetDimensions: Sized {
    /// Provide a mutable reference to the **dimension::Properties** for updating.
    fn properties(&mut self) -> &mut Properties;

    /// Set the absolute width for the primitive.
    fn width(mut self, w: f32) -> Self {
        self.properties().x = Some(w);
        self
    }

    /// Set the absolute height for the primitive.
    fn height(mut self, h: f32) -> Self {
        self.properties().y = Some(h);
        self
    }

    /// Set the absolute depth for the primitive (unused by 2D primitives such as rect, ellipse, tri, quad and text).
    fn depth(mut self, d: f32) -> Self {
        self.properties().z = Some(d);
        self
    }

    /// Short-hand for the **width** method.
    fn w(self, w: f32) -> Self {
        self.width(w)
    }

    /// Short-hand for the **height** method.
    fn h(self, h: f32) -> Self {
        self.height(h)
    }

    /// Short-hand for the **depth** method.
    fn d(self, d: f32) -> Self {
        self.depth(d)
    }

    /// Set the **x** and **y** dimensions for the primitive.
    fn wh(self, v: Vec2) -> Self {
        self.w(v.x).h(v.y)
    }

    /// Set the **x**, **y** and **z** dimensions for the primitive.
    fn whd(self, v: Vec3) -> Self {
        self.w(v.x).h(v.y).d(v.z)
    }

    /// Set the width and height for the primitive.
    fn w_h(self, x: f32, y: f32) -> Self {
        self.wh([x, y].into())
    }

    /// Set the width and height for the primitive.
    fn w_h_d(self, x: f32, y: f32, z: f32) -> Self {
        self.whd([x, y, z].into())
    }
}

impl SetDimensions for Properties {
    fn properties(&mut self) -> &mut Properties {
        self
    }
}

// Set the dimensions of the primitive being drawn at `index`.
//
// `None` leaves the corresponding dimension unchanged.
pub(crate) fn set_dimensions(draw: &Draw, index: usize, x: Option<f32>, y: Option<f32>, z: Option<f32>) {
    drawing::with_primitive(draw, index, |prim| match prim.dimensions_mut() {
        Some(props) => {
            if x.is_some() {
                props.x = x;
            }
            if y.is_some() {
                props.y = y;
            }
            if z.is_some() {
                props.z = z;
            }
        }
        None => crate::warn_once!("drawing primitive does not support `dimensions`"),
    })
}
