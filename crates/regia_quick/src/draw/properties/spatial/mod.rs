// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/properties/spatial/mod.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
pub use self::dimension::SetDimensions;
pub use self::orientation::SetOrientation;
pub use self::position::SetPosition;

/// Width, height and depth.
pub mod dimension;
/// Rotation.
pub mod orientation;
pub mod position;

/// Types that may be positioned, sized and oriented within 3D space.
pub trait SetSpatial: SetDimensions + SetPosition + SetOrientation {}

impl<T> SetSpatial for T where T: SetDimensions + SetPosition + SetOrientation {}

#[derive(Copy, Clone, Debug, PartialEq)]
/// Position, size and orientation of a primitive.
pub struct Properties {
    /// Position.
    pub position: position::Properties,
    /// Size.
    pub dimensions: dimension::Properties,
    /// Orientation.
    pub orientation: orientation::Properties,
}

impl Default for Properties {
    fn default() -> Self {
        let position = Default::default();
        let dimensions = Default::default();
        let orientation = Default::default();
        Properties {
            position,
            dimensions,
            orientation,
        }
    }
}

impl SetPosition for Properties {
    fn properties(&mut self) -> &mut position::Properties {
        self.position.properties()
    }
}

impl SetDimensions for Properties {
    fn properties(&mut self) -> &mut dimension::Properties {
        self.dimensions.properties()
    }
}

impl SetOrientation for Properties {
    fn properties(&mut self) -> &mut orientation::Properties {
        self.orientation.properties()
    }
}
