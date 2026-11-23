// SPDX-License-Identifier: MPL-2.0
//! The HLOD cross-fade (spec 78 § 11.2, ADR-2022): when the repaired cut switches a region from one
//! drawable to another (a cell's full content to its proxy, a proxy to its children), both draw for
//! `hlod_fade_frames` frames with complementary screen-space dithers, so the switch dissolves instead
//! of popping. The switch frame counts as the first of those frames, so a length of N gives N - 1
//! frames with a dither (shares 1/N .. (N-1)/N) and a length of 1 is an instant switch. Opt-in: with
//! no fade length authored, switches stay instant.
//!
//! [`FadeState::step`] is the pure rule. A drawable new in the cut fades in, one that left it fades
//! out and keeps drawing until its fade ends; both start on the same frame, so their dithers (`c` for
//! the incoming share, `-c` for the outgoing one, the renderer's `MeshLodFade` convention) cover every
//! pixel once. A drawable that reverses mid-fade continues from the share it had.

use std::collections::{BTreeMap, BTreeSet};

use regia_scene::stream_grid::CellCoord;

/// Something the cut draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Drawable {
    /// A tree node's proxy.
    Proxy(u32),
    /// A leaf cell's full content.
    Full(CellCoord),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fade {
    incoming: bool,
    /// Frames into the fade.
    frame: u32,
}

/// The cross-fades in progress.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FadeState {
    prev: BTreeSet<Drawable>,
    fades: BTreeMap<Drawable, Fade>,
    started: bool,
}

impl FadeState {
    /// Advances one frame with `now` the drawables of this frame's cut and `frames` the fade
    /// length. Returns the dither of every drawable in a fade: `c` in `(0, 1)` for an incoming one
    /// (the share of pixels it keeps), `-c` for an outgoing one (it keeps `1 - c`). Outgoing
    /// drawables are not in `now` and must keep drawing while they are in the answer. The first
    /// frame, and every frame with `frames <= 1`, fades nothing. A one-frame fade has no
    /// representable weight strictly inside the signed range, so it switches instantly.
    pub fn step(&mut self, now: &BTreeSet<Drawable>, frames: u32) -> BTreeMap<Drawable, f32> {
        if frames <= 1 || !self.started {
            self.fades.clear();
            if frames == 0 && self.started && &self.prev == now {
                return BTreeMap::new();
            }
            self.prev = now.clone();
            self.started = true;
            return BTreeMap::new();
        }
        let mut reversed = BTreeSet::new();
        for d in now.difference(&self.prev) {
            let frame = match self.fades.get(d) {
                Some(f) if !f.incoming => {
                    reversed.insert(*d);
                    frames.saturating_sub(f.frame)
                }
                _ => 0,
            };
            self.fades.insert(
                *d,
                Fade {
                    incoming: true,
                    frame,
                },
            );
        }
        for d in self.prev.difference(now) {
            let frame = match self.fades.get(d) {
                Some(f) if f.incoming => {
                    reversed.insert(*d);
                    frames.saturating_sub(f.frame)
                }
                _ => 0,
            };
            self.fades.insert(
                *d,
                Fade {
                    incoming: false,
                    frame,
                },
            );
        }
        self.prev = now.clone();
        let mut out = BTreeMap::new();
        self.fades.retain(|d, fade| {
            if reversed.contains(d) {
                fade.frame = fade.frame.saturating_add(1).min(frames - 1);
            } else {
                fade.frame += 1;
            }
            if fade.frame >= frames {
                return false;
            }
            let c = fade.frame as f32 / frames as f32;
            out.insert(*d, if fade.incoming { c } else { -c });
            true
        });
        out
    }

    /// True when `frames` is zero and a previous step already recorded the cut.
    ///
    /// The caller can skip building the drawable set: an instant fade has nothing in flight, and
    /// the stored set already matches a cut that did not change.
    pub fn settled(&self, frames: u32) -> bool {
        frames == 0 && self.started && self.fades.is_empty()
    }

    /// Forgets every fade (the level closed, or the tree changed).
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[Drawable]) -> BTreeSet<Drawable> {
        items.iter().copied().collect()
    }

    const FULL: Drawable = Drawable::Full(CellCoord { x: 0, z: 0 });
    const PROXY: Drawable = Drawable::Proxy(3);

    #[test]
    fn a_switch_dissolves_with_complementary_dithers_and_ends_on_time() {
        let mut state = FadeState::default();
        assert!(
            state.step(&set(&[FULL]), 4).is_empty(),
            "the first frame fades nothing"
        );
        assert!(state.step(&set(&[FULL]), 4).is_empty(), "no switch, no fade");
        // The cell goes to its proxy: the proxy comes in, the full content goes out.
        let mut seen = Vec::new();
        for _ in 0..4 {
            let fades = state.step(&set(&[PROXY]), 4);
            seen.push(fades.clone());
            if let (Some(i), Some(o)) = (fades.get(&PROXY), fades.get(&FULL)) {
                assert_eq!(*i, -*o, "complementary shares");
            }
        }
        assert_eq!(seen[0], [(FULL, -0.25), (PROXY, 0.25)].into_iter().collect());
        assert_eq!(seen[1], [(FULL, -0.5), (PROXY, 0.5)].into_iter().collect());
        assert_eq!(seen[2], [(FULL, -0.75), (PROXY, 0.75)].into_iter().collect());
        assert!(seen[3].is_empty(), "settled after the fade length");
    }

    #[test]
    fn a_reversal_mid_fade_continues_from_the_share_it_had() {
        let mut state = FadeState::default();
        state.step(&set(&[FULL]), 4);
        state.step(&set(&[PROXY]), 4); // proxy 0.25 in
        let half = state.step(&set(&[PROXY]), 4); // proxy 0.5 in, full 0.5 kept
        assert_eq!(half[&PROXY], 0.5);
        // Back to the full content: it had half the pixels and goes on from there, one frame on.
        let back = state.step(&set(&[FULL]), 4);
        assert_eq!(back, [(FULL, 0.75), (PROXY, -0.75)].into_iter().collect());
        assert!(state.step(&set(&[FULL]), 4).is_empty(), "settled");
    }

    #[test]
    fn no_fade_length_means_instant_switches() {
        let mut state = FadeState::default();
        state.step(&set(&[FULL]), 0);
        assert!(state.step(&set(&[PROXY]), 0).is_empty());
        assert!(state.step(&set(&[FULL]), 0).is_empty());
    }
}
