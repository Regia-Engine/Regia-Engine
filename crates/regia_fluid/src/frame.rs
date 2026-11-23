// SPDX-License-Identifier: MPL-2.0
//! `FluidFrame`: what a step (live or replayed from a cache) hands to drawing and tools.

use crate::sim::FluidSim;
use crate::whitewater::WhitewaterKind;

/// The type tag of a frame point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum PointKind {
    /// A liquid particle.
    Liquid = 0,
    /// Foam.
    Foam = 1,
    /// Spray.
    Spray = 2,
    /// An air bubble.
    Bubble = 3,
}

impl PointKind {
    /// From the tag number.
    #[must_use]
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Liquid),
            1 => Some(Self::Foam),
            2 => Some(Self::Spray),
            3 => Some(Self::Bubble),
            _ => None,
        }
    }

    /// Lowercase name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Liquid => "liquid",
            Self::Foam => "foam",
            Self::Spray => "spray",
            Self::Bubble => "bubble",
        }
    }
}

impl From<WhitewaterKind> for PointKind {
    fn from(kind: WhitewaterKind) -> Self {
        match kind {
            WhitewaterKind::Foam => Self::Foam,
            WhitewaterKind::Spray => Self::Spray,
            WhitewaterKind::Bubble => Self::Bubble,
        }
    }
}

/// One frame: positions with a type tag and an age (seconds; 0 for liquid), liquid first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FluidFrame {
    /// Step number.
    pub step: u64,
    /// Positions (world).
    pub positions: Vec<[f32; 3]>,
    /// Type tag per point ([`PointKind`] numbers).
    pub kinds: Vec<u8>,
    /// Age per point in seconds.
    pub ages: Vec<f32>,
}

impl FluidFrame {
    /// Points of `kind`.
    #[must_use]
    pub fn count(&self, kind: PointKind) -> usize {
        self.kinds.iter().filter(|k| **k == kind as u8).count()
    }

    /// Number of points.
    #[must_use]
    pub fn len(&self) -> usize {
        self.positions.len()
    }

    /// Whether the frame has no points.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }
}

impl FluidSim {
    /// The current frame: every liquid particle, then every whitewater particle.
    #[must_use]
    pub fn frame(&self) -> FluidFrame {
        let ww = self.whitewater().particles();
        let total = self.positions().len() + ww.len();
        let mut frame = FluidFrame {
            step: self.steps(),
            positions: Vec::with_capacity(total),
            kinds: Vec::with_capacity(total),
            ages: Vec::with_capacity(total),
        };
        for p in self.positions() {
            frame.positions.push(p.to_array());
            frame.kinds.push(PointKind::Liquid as u8);
            frame.ages.push(0.0);
        }
        for p in ww {
            frame.positions.push(p.position.to_array());
            frame.kinds.push(PointKind::from(p.kind) as u8);
            frame.ages.push(p.age);
        }
        frame
    }
}
