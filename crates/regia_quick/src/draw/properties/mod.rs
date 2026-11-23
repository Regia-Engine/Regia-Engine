// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/properties/mod.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
//! Parameters which a **Drawing** instance may use to describe certain properties of a drawing.
//!
//! Each time a new method is chained onto a **Drawing** instance, it uses the given values to set
//! one or more properties for the drawing.
//!
//! Each **Drawing** instance is associated with a specific **Node** in the geometry graph and has
//! a unique **node::Index** to simplify this.

pub use self::color::SetColor;
pub use self::fill::SetFill;
pub use self::spatial::dimension::SetDimensions;
pub use self::spatial::orientation::SetOrientation;
pub use self::spatial::position::SetPosition;
pub use self::stroke::SetStroke;

/// Colour.
pub mod color;
/// Fill tessellation options.
pub mod fill;
/// Position, size and orientation.
pub mod spatial;
/// Stroke tessellation options.
pub mod stroke;
/// Texture coordinates.
pub mod tex_coords;
