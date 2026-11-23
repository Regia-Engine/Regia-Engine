// SPDX-License-Identifier: MPL-2.0
//! Line-of-sight hook (plan row S-16, ADR-8154).
//!
//! Sight in [`crate::fog`] is a radius test; whether terrain, walls or a tunnel roof block the line
//! between a watcher and what it looks at is a separate question that the world geometry answers.
//! [`LineOfSight`] is that seam: a host plugs its own query in (a heightfield ray march, the world
//! geometry crate's segment test, a portal graph), and code that only has open ground uses
//! [`OpenGround`], which never blocks.
//!
//! The trait is deliberately small and not tied to fog: AI perception and anything else that asks
//! "can A see B" take the same hook, so one geometry query serves every caller.
//!
//! An implementation used on a lockstep tick must itself be deterministic (the same inputs give the
//! same answer on every peer and architecture), because a fog answer that feeds gameplay would
//! otherwise desync. A presentation-only caller (the per-viewer snapshot filter) has no such need,
//! but the same implementation is usually used for both.
//!
//! ```
//! use regia_detsim::los::{LineOfSight, OpenGround};
//!
//! // Open ground never blocks.
//! assert!(OpenGround.clear([0.0, 0.0, 0.0], [500.0, 0.0, 0.0]));
//!
//! // A closure is a line-of-sight query too: here a wall on the plane x = 10.
//! let wall = |from: [f32; 3], to: [f32; 3]| (from[0] < 10.0) == (to[0] < 10.0);
//! assert!(wall.clear([0.0, 0.0, 0.0], [5.0, 0.0, 0.0]));
//! assert!(!wall.clear([0.0, 0.0, 0.0], [20.0, 0.0, 0.0]));
//! ```

/// Answers whether the straight line from one point to another is unobstructed.
///
/// Points are in the caller's world frame (metres). Implementations decide what blocks (terrain,
/// walls, closed doors) and must be pure: the same two points and the same world give the same
/// answer, whatever order the calls come in.
pub trait LineOfSight {
    /// True when nothing blocks the segment from `from` to `to`.
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool;
}

/// Line of sight over open ground: nothing ever blocks.
///
/// The default hook until a world-geometry query is plugged in; with it, fog of war is a pure
/// radius test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenGround;

impl LineOfSight for OpenGround {
    #[inline]
    fn clear(&self, _from: [f32; 3], _to: [f32; 3]) -> bool {
        true
    }
}

impl<F: Fn([f32; 3], [f32; 3]) -> bool> LineOfSight for F {
    #[inline]
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        self(from, to)
    }
}
