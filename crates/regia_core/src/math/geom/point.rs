// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_core/src/geom/point.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
//! Implementation of the **Point** types.
//!
//! Currently, **Point** is simply a type alias for **Vector**. While this makes it easier for new
//! uses to understand and switch between the two, it also conflates the two mathematical
//! concepts which are quite distinct. It is possible that in the future, we will switch to using
//! distinct types. For now, we are attempting to monitor usage and feedback to determine whether
//! or not this change is worth it.

/// A 2-dimensional point type.
pub type Point2 = glam::Vec2;

/// A 3-dimensional point type.
pub type Point3 = glam::Vec3;

/// A 4-dimensional point type.
pub type Point4 = glam::Vec4;

/// Construct a 2-dimensional point.
pub fn pt2(x: f32, y: f32) -> Point2 {
    (x, y).into()
}

/// Construct a 3-dimensional point.
pub fn pt3(x: f32, y: f32, z: f32) -> Point3 {
    (x, y, z).into()
}

/// Construct a 4-dimensional point.
pub fn pt4(x: f32, y: f32, z: f32, w: f32) -> Point4 {
    (x, y, z, w).into()
}
