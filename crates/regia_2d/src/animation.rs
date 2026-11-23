// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3),
// Copyright (c) 2019-2021 Fedor Logachev, MIT OR Apache-2.0, used under the MIT licence.
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

//! Sprite-sheet animation (`docs/specs/22-tier2.md` § T2.10): an image cut into equal tiles, one
//! animation per row, played at a frame rate.
//!
//! Ported from macroquad's `experimental/animation.rs` and adapted: the frame clock carries the
//! remainder instead of resetting to zero (so the playback rate does not drift with the render
//! frame rate), several frames are skipped when a long frame covers them, an animation can play
//! once and hold its last frame, the sheet can start at a pixel offset, and the sprite is an ECS
//! component that [`animate_sprites`] advances each frame.

use regia_app::prelude::*;

use crate::sprite::{Sprite, SpriteSource};

/// One animation of a sheet: a row of `frames` tiles played at `fps`.
#[derive(Reflect, Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[reflect(Default)]
pub struct Animation {
    /// Name, for [`AnimatedSprite::play`].
    pub name: String,
    /// Sheet row (0 = top row).
    pub row: u32,
    /// Number of frames, starting at column 0.
    pub frames: u32,
    /// Frames per second.
    pub fps: f32,
    /// Loop (true) or play once and hold the last frame (false).
    pub looping: bool,
}

impl Default for Animation {
    fn default() -> Self {
        Self {
            name: String::new(),
            row: 0,
            frames: 1,
            fps: 10.0,
            looping: true,
        }
    }
}

impl Animation {
    /// A looping animation.
    #[must_use]
    pub fn new(name: impl Into<String>, row: u32, frames: u32, fps: f32) -> Self {
        Self {
            name: name.into(),
            row,
            frames: frames.max(1),
            fps,
            looping: true,
        }
    }

    /// The same animation, played once.
    #[must_use]
    pub fn once(mut self) -> Self {
        self.looping = false;
        self
    }
}

/// The current frame of an [`AnimatedSprite`]: where it is in the sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnimationFrame {
    /// Source rectangle in sheet pixels: `[x, y, width, height]` (origin top-left).
    pub source_rect: [u32; 4],
}

/// A sprite animated from a sheet of equal tiles. Put it on an entity with a [`Sprite`]; the
/// plugin's [`animate_sprites`] advances it and writes the frame into the sprite's source.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct AnimatedSprite {
    /// Tile width in sheet pixels.
    pub tile_width: u32,
    /// Tile height in sheet pixels.
    pub tile_height: u32,
    /// Pixel offset of tile (0, 0) in the sheet (an atlas region, or a sheet with a border).
    /// Frames are always sampled from atlas page 0.
    pub sheet_offset: [u32; 2],
    /// The animations, one per row.
    pub animations: Vec<Animation>,
    /// Whether [`AnimatedSprite::update`] advances frames (ignored once the animation is finished).
    pub playing: bool,
    current: usize,
    /// Animation clock: the whole nanoseconds of the leftover before the next frame, after an
    /// integer division by the exact `f32` rate. Integer accumulation is deliberate: the frame index
    /// must depend only on the total time advanced, never on how that time was sliced into
    /// `update` calls (a 60 Hz run and four clamped 250 ms frames land on the same frame).
    ///
    /// The reflected name is `time_ns`, not `time`: components saved before the determinism
    /// fix (M-fixrev-03-k) wrote a float `time` field under this same reflected type name;
    /// the rename makes the lenient scene decoder ignore the stale float instead of failing
    /// the whole component (docs/releases/v0.1.28-fixrev-03-k.md).
    time_ns: u64,
    /// The part of the leftover below one nanosecond, in the rate's own units (see
    /// [`Self::frames_in`]). Most periods are not a whole number of nanoseconds (72 fps is
    /// 13,888,888.89 ns): a leftover kept in whole nanoseconds is rounded on every update, up or
    /// down, and either gains a frame or loses one against the total time. With this part the
    /// leftover is exact. A save without the field loads it as 0 (under 1 ns of phase).
    time_sub: u32,
    frame: u32,
    finished: bool,
}

impl Default for AnimatedSprite {
    fn default() -> Self {
        Self::new(16, 16, &[Animation::default()], true)
    }
}

/// The largest nanosecond count still below one animation step at `fps`, so a rounded phase
/// remainder can never reach a full period (`update(0.0)` must never advance a frame).
fn below_period_ns(rounded: u64, fps: f64) -> u64 {
    if rounded as f64 * fps >= 1.0e9 {
        rounded - 1
    } else {
        rounded
    }
}

impl AnimatedSprite {
    /// A sprite over a sheet of `tile_width`×`tile_height` tiles.
    #[must_use]
    pub fn new(tile_width: u32, tile_height: u32, animations: &[Animation], playing: bool) -> Self {
        Self {
            tile_width,
            tile_height,
            sheet_offset: [0, 0],
            animations: animations.to_vec(),
            playing,
            current: 0,
            time_ns: 0,
            time_sub: 0,
            frame: 0,
            finished: false,
        }
    }

    /// Selects animation `index` (out of range is ignored). The frame is kept (wrapped into the
    /// new animation's range), as in macroquad; call [`Self::set_frame`] to restart.
    pub fn set_animation(&mut self, index: usize) {
        if let Some(animation) = self.animations.get(index) {
            // Keep the clock's phase (the fraction of a step already spent), not its seconds: the
            // leftover of a slow clip would otherwise skip many frames of a fast one.
            let old_fps = self.animation().map_or(0.0, |a| a.fps);
            self.time_ns = if old_fps > 0.0 && animation.fps > 0.0 {
                let rescaled = self.time_ns as f64 * f64::from(old_fps) / f64::from(animation.fps);
                if rescaled.is_finite() {
                    below_period_ns(rescaled.round() as u64, f64::from(animation.fps))
                } else {
                    0
                }
            } else {
                0
            };
            // The carried phase is a rounded rescale: its sub-nanosecond part belongs to the old
            // rate's units and is dropped.
            self.time_sub = 0;
            self.current = index;
            self.frame %= animation.frames.max(1);
            self.finished = false;
        }
    }

    /// Selects an animation by name and restarts it from frame 0, unless it is already the current
    /// one and not finished (a paused clip resumes in place). Sets `playing`.
    /// Returns false when no animation has that name.
    pub fn play(&mut self, name: &str) -> bool {
        let Some(index) = self.animations.iter().position(|a| a.name == name) else {
            return false;
        };
        if index != self.current || self.finished {
            self.current = index;
            self.frame = 0;
            self.time_ns = 0;
            self.time_sub = 0;
            self.finished = false;
        }
        self.playing = true;
        true
    }

    /// Index of the current animation.
    #[must_use]
    pub const fn current_animation(&self) -> usize {
        self.current
    }

    /// The current animation, if the list is not empty.
    #[must_use]
    pub fn animation(&self) -> Option<&Animation> {
        self.animations.get(self.current)
    }

    /// Jumps to `frame` of the current animation (wrapped into range).
    pub fn set_frame(&mut self, frame: u32) {
        let frames = self.animation().map_or(1, |a| a.frames.max(1));
        self.frame = frame % frames;
        self.time_ns = 0;
        self.time_sub = 0;
        self.finished = false;
    }

    /// The current frame index.
    #[must_use]
    pub const fn frame_index(&self) -> u32 {
        self.frame
    }

    /// True when the last frame of the current animation is showing.
    #[must_use]
    pub fn is_last_frame(&self) -> bool {
        self.animation().is_some_and(|a| self.frame + 1 >= a.frames)
    }

    /// True when a play-once animation reached its last frame and stopped.
    #[must_use]
    pub const fn finished(&self) -> bool {
        self.finished
    }

    /// Advances the clock by `dt` seconds, stepping one frame every `1 / fps` seconds.
    pub fn update(&mut self, dt: f32) {
        let Some(animation) = self.animations.get(self.current) else {
            return;
        };
        let frames = animation.frames.max(1);
        if self.playing && !self.finished && animation.fps > 0.0 {
            // `dt` joins the clock as integer nanoseconds. The step count is then an integer
            // division by the exact f32 rate, and the leftover is kept exactly (determinism audit
            // 11-08 M1). Rebuilding the phase as `round(fract * 1e9 / fps)` was 1 ns short and
            // dropped a frame when 1 s at 7 fps was two 0.5 s steps; a leftover in whole
            // nanoseconds rounded up instead gained a frame at 72 fps and could step on
            // `update(0.0)` (fixrev-03-k L4). A huge `dt` or `fps` saturates the count.
            let added = (f64::from(dt.max(0.0)) * 1.0e9).round();
            self.time_ns = self.time_ns.saturating_add(added as u64);
            let (steps, leftover_ns, leftover_sub) =
                Self::frames_in(self.time_ns, self.time_sub, animation.fps);
            self.time_ns = leftover_ns;
            self.time_sub = leftover_sub;
            let last = u64::from(frames - 1);
            let frame = u64::from(self.frame).min(last);
            if animation.looping {
                self.frame = ((frame + steps % u64::from(frames)) % u64::from(frames)) as u32;
            } else if steps <= last - frame {
                self.frame = (frame + steps) as u32;
            } else {
                self.frame = last as u32;
                self.finished = true;
                self.time_ns = 0;
                self.time_sub = 0;
            }
        }
        self.frame %= frames;
    }

    /// Whole frames in a clock of `time_ns` nanoseconds plus `time_sub` rate units at `fps`, and
    /// the leftover before the next frame as `(nanoseconds, rate units)`.
    ///
    /// `fps` is decoded to `significand * 2^exp2`, the exact f32 value, and the clock is counted
    /// in units of `1 / scale` ns, where `scale` is the significand (shifted left by `exp2` for
    /// a rate of 2^23 fps or more). In those units one frame is a whole number, `1e9 * 2^-exp2`
    /// (`1e9` for the huge rates), so the split is one integer division and its remainder is
    /// exact: `steps(A) + steps(leftover(A) + B) = steps(A + B)` for every `A` and `B`, and a
    /// leftover is always less than one frame. `time_sub` is that remainder's part below one
    /// nanosecond (less than `scale` and less than 2^30).
    ///
    /// The count saturates at `9e15` with an empty leftover. A non-finite or subnormal rate, or
    /// one so small that a frame does not fit the 128-bit units, contributes no steps and keeps
    /// the clock.
    fn frames_in(time_ns: u64, time_sub: u32, fps: f32) -> (u64, u64, u32) {
        const NS_PER_S: u128 = 1_000_000_000;
        const STEP_CAP: u64 = 9_000_000_000_000_000;
        // The largest `-exp2` for which `1e9 << -exp2` still fits in 128 bits (1e9 < 2^30).
        const MAX_PERIOD_SHIFT: u32 = 98;
        let bits = fps.to_bits();
        let biased = (bits >> 23) & 0xff;
        if biased == 0 || biased == 0xff || fps.is_sign_negative() {
            return (0, time_ns, time_sub);
        }
        let sig = u128::from((bits & 0x007f_ffff) | 0x0080_0000);
        // `biased` is 1..=254, so `exp2` is -149..=104.
        let (scale, frame) = if biased >= 150 {
            // `sig < 2^24` and the shift is at most 104: the product is below 2^128.
            (sig << (biased - 150), NS_PER_S)
        } else {
            let shift = 150 - biased;
            if shift > MAX_PERIOD_SHIFT {
                return (0, time_ns, time_sub);
            }
            (sig, NS_PER_S << shift)
        };
        // A part left by another rate (the clip's `fps` was edited) reads as under 1 ns.
        let sub = u128::from(time_sub).min(scale - 1);
        let Some(units) = u128::from(time_ns)
            .checked_mul(scale)
            .and_then(|units| units.checked_add(sub))
        else {
            return (STEP_CAP, 0, 0);
        };
        let steps = units / frame;
        if steps > u128::from(STEP_CAP) {
            return (STEP_CAP, 0, 0);
        }
        let rest = units % frame;
        // `rest / scale <= time_ns`, and `rest % scale` is below `scale` and below `frame`: under
        // 2^24 for an everyday rate, under 1e9 for a huge one.
        let leftover_ns = u64::try_from(rest / scale).unwrap_or(time_ns);
        let leftover_sub = u32::try_from(rest % scale).unwrap_or(0);
        (
            u64::try_from(steps).unwrap_or(STEP_CAP),
            leftover_ns,
            leftover_sub,
        )
    }

    /// The current frame's place in the sheet.
    #[must_use]
    pub fn frame(&self) -> AnimationFrame {
        let row = self.animation().map_or(0, |a| a.row);
        AnimationFrame {
            source_rect: [
                // Rows, frames and tile sizes are public and unchecked: saturate, never wrap.
                self.sheet_offset[0].saturating_add(self.tile_width.saturating_mul(self.frame)),
                self.sheet_offset[1].saturating_add(self.tile_height.saturating_mul(row)),
                self.tile_width,
                self.tile_height,
            ],
        }
    }
}

/// `Update`: advances every [`AnimatedSprite`] by the frame time and points its [`Sprite`] at the
/// current frame.
#[tracing::instrument(skip_all)]
pub fn animate_sprites(time: Res<Time>, mut sprites: Query<(&mut AnimatedSprite, &mut Sprite)>) {
    let dt = time.delta_secs();
    for (mut animated, mut sprite) in &mut sprites {
        animated.update(dt);
        let rect = animated.frame().source_rect;
        let source = SpriteSource::Rect { rect };
        if sprite.source != source {
            sprite.source = source;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes a scene's component value the way `regia_scene` does (`ReflectCodec::from_ron_typed`).
    fn decode_scene_value(text: &str) -> Result<AnimatedSprite, String> {
        use regia_ecs::reflect::{FromReflect, TypeRegistry};
        let mut registry = TypeRegistry::default();
        registry.register::<AnimatedSprite>();
        let registration = registry
            .get(std::any::TypeId::of::<AnimatedSprite>())
            .ok_or("AnimatedSprite is not registered")?;
        let mut codec = regia_ecs::serialize::ReflectCodec::new(&registry);
        let value = codec
            .from_ron_typed(text, registration)
            .map_err(|e| e.to_string())?;
        AnimatedSprite::from_reflect(value.as_partial_reflect())
            .ok_or_else(|| "from_reflect failed".to_string())
    }

    /// v0.4 saved the private clock as `time: <f32 seconds>`. The clock became integer
    /// nanoseconds; a scene that still holds the float must load its `AnimatedSprite` (a component
    /// that fails to decode is dropped from the entity), keep the frame, and restart the phase.
    #[test]
    fn a_v04_scene_value_with_a_float_clock_still_loads() {
        let text = "(tile_width: 15, tile_height: 20, sheet_offset: (0, 0), animations: [], \
                    playing: true, current: 0, time: 0.25, frame: 2, finished: false)";
        let sprite = decode_scene_value(text).expect("a v0.4 AnimatedSprite decodes");
        assert_eq!(sprite.frame_index(), 2, "the saved frame is kept");
        assert_eq!((sprite.tile_width, sprite.tile_height), (15, 20));
        // A value saved now round-trips.
        let mut registry = regia_ecs::reflect::TypeRegistry::default();
        registry.register::<AnimatedSprite>();
        let mut playing = AnimatedSprite::new(4, 4, &[Animation::new("a", 0, 4, 7.0)], true);
        playing.update(0.2);
        let saved = regia_ecs::serialize::ReflectCodec::new(&registry)
            .to_ron_typed(&playing)
            .expect("serialize");
        assert_eq!(
            decode_scene_value(&saved).expect("round trip"),
            playing,
            "{saved}"
        );
    }

    #[test]
    fn a_huge_row_saturates_the_source_rect() {
        let sprite = AnimatedSprite::new(16, 2, &[Animation::new("idle", u32::MAX, 4, 8.0)], true);
        assert_eq!(sprite.frame().source_rect[1], u32::MAX);
    }

    fn sheet() -> AnimatedSprite {
        AnimatedSprite::new(
            15,
            20,
            &[
                Animation::new("idle", 0, 4, 8.0),
                Animation::new("jump", 2, 3, 10.0).once(),
            ],
            true,
        )
    }

    #[test]
    fn frames_step_at_the_frame_rate_and_keep_the_remainder() {
        let mut s = sheet();
        // 64 Hz updates for 0.75 s at 8 animation fps → exactly 6 steps = frame 6 % 4 = 2 (binary
        // fractions, so no rounding decides the count).
        for _ in 0..48 {
            s.update(1.0 / 64.0);
        }
        assert_eq!(s.frame_index(), 2);
        assert_eq!(s.frame().source_rect, [30, 0, 15, 20]);
    }

    #[test]
    fn three_fps_over_one_second_matches_for_both_step_sizes() {
        // 3 fps over exactly one second is three steps (frame 3 of a 4-frame loop), however the
        // second is sliced: four 250 ms frames (the wall-clock clamp) and sixty 60 Hz frames must
        // land on the same frame (determinism audit 11-08 M1: the f32 phase clock plus an
        // f32-rounded `1 / fps` period made them disagree).
        let run = |dt: f32, updates: usize| {
            let mut s = AnimatedSprite::new(8, 8, &[Animation::new("loop", 0, 4, 3.0)], true);
            for _ in 0..updates {
                s.update(dt);
            }
            s.frame_index()
        };
        assert_eq!(run(0.25, 4), 3, "four 250 ms frames");
        assert_eq!(run(1.0 / 60.0, 60), 3, "sixty 60 Hz frames");
    }

    #[test]
    fn one_second_at_seven_fps_does_not_depend_on_the_slice() {
        // 7 does not divide 1e9. One 1 s step, two 0.5 s steps and four 0.25 s steps are the same
        // duration (each sum is exact in f32) and are seven frames: index 7 % 4 = 3. Rebuilding
        // the phase with `fract * 1e9 / fps` rounded to nanoseconds drops a frame on the splits
        // (500_000_000 ns is 3.5 frames, and the stored remainder is 1 ns short of the half).
        let run = |dts: &[f32]| {
            let mut s = AnimatedSprite::new(8, 8, &[Animation::new("loop", 0, 4, 7.0)], true);
            for dt in dts {
                s.update(*dt);
            }
            s.frame_index()
        };
        let whole = run(&[1.0]);
        assert_eq!(whole, 3, "one second at 7 fps");
        assert_eq!(run(&[0.5, 0.5]), whole, "two 0.5 s steps");
        assert_eq!(run(&[0.25; 4]), whole, "four 0.25 s steps");
    }

    #[test]
    fn awkward_rates_agree_across_exact_slices_and_ignore_a_zero_step() {
        let run = |fps: f32, dts: &[f32]| {
            let mut s = AnimatedSprite::new(4, 4, &[Animation::new("a", 0, 8, fps)], true);
            for dt in dts {
                s.update(*dt);
            }
            s.frame_index()
        };
        for fps in [1.0, 3.0, 7.0, 12.5, 24.0, 30.0, 60.0, 23.976] {
            let whole = run(fps, &[1.0]);
            assert_eq!(run(fps, &[0.5, 0.5]), whole, "{fps} halves");
            assert_eq!(run(fps, &[0.25; 4]), whole, "{fps} quarters");
            let mut held = AnimatedSprite::new(4, 4, &[Animation::new("a", 0, 8, fps)], true);
            held.update(0.5);
            let frame = held.frame_index();
            held.update(0.0);
            assert_eq!(held.frame_index(), frame, "{fps} zero dt");
        }
    }

    #[test]
    fn a_zero_time_update_never_advances_a_frame() {
        // 72 fps: the exact period is 13,888,888.89 ns, which the remainder rounding pushes to
        // 13,888,889 — a full period. Before the clamp, the next `update(0.0)` (issued every
        // frame while `Time` is paused) took a 27th step although no time had elapsed, breaking
        // the slicing independence the nanosecond clock promises (review fixrev-03-k L4).
        let mut s = AnimatedSprite::new(8, 8, &[Animation::new("spin", 0, 1000, 72.0)], true);
        for dt in [0.12_f32, 0.008, 0.094, 0.056, 0.097] {
            s.update(dt);
        }
        assert_eq!(s.frame_index(), 26, "floor of the total time advanced at 72 fps");
        s.update(0.0);
        assert_eq!(s.frame_index(), 26, "no step without elapsed time");
    }

    /// The same five updates in any order are the same total time, so every order lands on the
    /// same frame: 374,999,999 ns at 72 fps is 26 frames and 2 ns short of the 27th. A leftover
    /// rounded up to whole nanoseconds on every update drifts by up to 1 ns per update, and most
    /// orders then reached the 27th frame early (integ-v048 kept the whole-nanosecond `frames_in`
    /// and dropped the fixrev-03-k clamp; neither passed the other's test).
    #[test]
    fn the_order_of_the_same_updates_never_changes_the_frame() {
        let dts = [0.12_f32, 0.008, 0.094, 0.056, 0.097];
        let run = |order: &[usize; 5]| {
            let mut s = AnimatedSprite::new(8, 8, &[Animation::new("spin", 0, 1000, 72.0)], true);
            for &i in order {
                s.update(dts[i]);
            }
            s.frame_index()
        };
        // Every permutation of the five updates (Heap's algorithm).
        let mut order = [0_usize, 1, 2, 3, 4];
        let mut counters = [0_usize; 5];
        let mut seen = 1;
        assert_eq!(run(&order), 26, "{order:?}");
        let mut i = 0;
        while i < order.len() {
            if counters[i] < i {
                if i.is_multiple_of(2) {
                    order.swap(0, i);
                } else {
                    order.swap(counters[i], i);
                }
                assert_eq!(run(&order), 26, "{order:?}");
                seen += 1;
                counters[i] += 1;
                i = 0;
            } else {
                counters[i] = 0;
                i += 1;
            }
        }
        assert_eq!(seen, 120, "all orders of five updates");
    }

    /// 27,777,777 ns at 72 fps is one frame and 13,888,888.1 ns of the next, under the
    /// 13,888,888.9 ns period. A leftover kept as 13,888,889 whole nanoseconds is already a
    /// period: the next `update(0.0)`, issued every frame while `Time` is paused, stepped with no
    /// time elapsed (review fixrev-03-k L4, brought back by the integ-v048 resolution).
    #[test]
    fn a_leftover_just_under_a_period_does_not_step_on_a_zero_update() {
        let mut s = AnimatedSprite::new(8, 8, &[Animation::new("spin", 0, 1000, 72.0)], true);
        s.update(0.005); // 5,000,000 ns
        s.update(0.022_777_777); // 22,777,777 ns
        assert_eq!(s.frame_index(), 1, "27,777,777 ns is one frame at 72 fps");
        for _ in 0..3 {
            s.update(0.0);
            assert_eq!(s.frame_index(), 1, "no step without elapsed time");
        }
    }

    /// A positive rate far below any real clip (one frame per 1e30 s) is still a rate. Its step
    /// length does not fit the clock's integer math, which must read as "no step yet", not as a
    /// division by zero (the shifted divisor of the whole-nanosecond `frames_in` wrapped to 0).
    #[test]
    fn a_tiny_positive_rate_takes_no_step_and_does_not_panic() {
        for fps in [1.0e-30_f32, 2.4e-29, 1.0e-25, 1.0e-20, f32::MIN_POSITIVE] {
            let mut s = AnimatedSprite::new(4, 4, &[Animation::new("slow", 0, 4, fps)], true);
            s.update(1.0);
            s.update(f32::MAX);
            assert_eq!(s.frame_index(), 0, "{fps:e}");
        }
    }

    #[test]
    fn below_period_clamps_a_rounded_remainder() {
        // 72 fps period: 13,888,888.89 ns rounds up; 200 fps: exactly 5,000,000 ns.
        assert_eq!(below_period_ns(13_888_889, 72.0), 13_888_888);
        assert_eq!(below_period_ns(13_888_888, 72.0), 13_888_888);
        assert_eq!(below_period_ns(5_000_000, 200.0), 4_999_999);
        assert_eq!(below_period_ns(4_999_999, 200.0), 4_999_999);
        assert_eq!(below_period_ns(0, 72.0), 0);
    }

    /// The reflected types a saved [`AnimatedSprite`] needs.
    fn sprite_registry() -> bevy_reflect::TypeRegistry {
        let mut registry = bevy_reflect::TypeRegistry::default();
        registry.register::<String>();
        registry.register::<bool>();
        registry.register::<u32>();
        registry.register::<u64>();
        registry.register::<usize>();
        registry.register::<f32>();
        registry.register::<[u32; 2]>();
        registry.register::<Animation>();
        registry.register::<Vec<Animation>>();
        registry.register::<AnimatedSprite>();
        registry
    }

    /// A save taken mid-frame keeps the exact leftover, the part below one nanosecond included:
    /// the loaded sprite and the one that never left memory step on the same later update. At
    /// 72 fps 119,999,997 ns is eight frames and 8,888,885.89 ns, and 5,000,003 ns more is
    /// 125,000,000 ns, exactly nine frames. A loaded clock without the 0.89 ns would stop at
    /// 13,888,888 ns, under the 13,888,888.89 ns period, and stay on frame eight.
    #[test]
    fn a_saved_clock_keeps_its_sub_nanosecond_part() {
        use regia_ecs::serialize::ReflectCodec;

        let registry = sprite_registry();
        let registration = registry
            .get(std::any::TypeId::of::<AnimatedSprite>())
            .expect("AnimatedSprite is registered");
        let mut codec = ReflectCodec::new(&registry);

        let mut live = AnimatedSprite::new(8, 8, &[Animation::new("spin", 0, 1000, 72.0)], true);
        live.update(0.12); // 119,999,997 ns
        assert_eq!(live.frame_index(), 8);
        assert_ne!(live.time_sub, 0, "72 fps leaves a fraction of a nanosecond");
        let text = codec.to_ron_typed(&live).expect("serialize");
        let loaded = codec.from_ron_typed(&text, registration).expect("decode");
        let mut loaded = loaded
            .downcast_ref::<AnimatedSprite>()
            .expect("the decoded value is an AnimatedSprite")
            .clone();
        assert_eq!(loaded, live);
        for sprite in [&mut live, &mut loaded] {
            sprite.update(0.005_000_003); // 5,000,003 ns
            assert_eq!(sprite.frame_index(), 9, "125,000,000 ns is nine frames at 72 fps");
        }
        assert_eq!(loaded, live);
    }

    /// A component saved before the determinism fix wrote the animation clock as a reflected
    /// float field named `time`; the field is now the nanosecond `time_ns`. The lenient scene
    /// decoder must ignore the stale float (instead of failing the whole component, as it did
    /// while the field kept the name `time` with a `u64` type), fill `time_ns` from the default
    /// and keep every other field (review fixrev-03-k finding 1).
    #[test]
    fn a_save_from_before_the_nanosecond_clock_still_loads() {
        use regia_ecs::serialize::ReflectCodec;

        let registry = sprite_registry();
        let registration = registry
            .get(std::any::TypeId::of::<AnimatedSprite>())
            .expect("AnimatedSprite is registered");
        let mut codec = ReflectCodec::new(&registry);

        // A live component mid-animation, serialized by a current build…
        let mut saved = sheet();
        saved.update(0.3);
        assert_eq!(saved.frame_index(), 2);
        let text = codec.to_ron_typed(&saved).expect("serialize");
        // …then rewritten the way an old build's save looks: every reflected field it had, the
        // clock as a float in seconds under the old name, no `time_ns` and no `time_sub`.
        assert!(text.contains("time_sub:"), "{text}");
        let old_style = text
            .lines()
            .filter(|line| !line.trim_start().starts_with("time_sub:"))
            .map(|line| {
                if line.trim_start().starts_with("time_ns:") {
                    "    time: 0.0625,".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(old_style.contains("time: 0.0625"), "{old_style}");
        assert!(!old_style.contains("time_ns"), "{old_style}");
        assert!(!old_style.contains("time_sub"), "{old_style}");

        let decoded = codec
            .from_ron_typed(&old_style, registration)
            .expect("an old save with a float `time` must still decode");
        let decoded = decoded
            .downcast_ref::<AnimatedSprite>()
            .expect("the decoded value is an AnimatedSprite");
        assert_eq!(decoded.tile_width, saved.tile_width);
        assert_eq!(decoded.tile_height, saved.tile_height);
        assert_eq!(decoded.sheet_offset, saved.sheet_offset);
        assert_eq!(decoded.animations, saved.animations);
        assert_eq!(decoded.playing, saved.playing);
        assert_eq!(decoded.current_animation(), saved.current_animation());
        assert_eq!(
            decoded.frame_index(),
            saved.frame_index(),
            "the frame is kept; the clock itself restarts from the default"
        );
        assert_eq!(decoded.finished(), saved.finished());
        // The component is functional after the load: the clock restarted from zero (the phase
        // is not recoverable from an old save, only the frame), so 100 ms at 8 fps is not yet a
        // step from the kept frame.
        let mut decoded_owned = decoded.clone();
        decoded_owned.update(0.1);
        assert_eq!(decoded_owned.frame_index(), 2);
        decoded_owned.update(0.2);
        assert_eq!(
            decoded_owned.frame_index(),
            0,
            "two more steps wrap the 4-frame loop"
        );
    }

    #[test]
    fn absurd_frame_rates_and_times_terminate() {
        let mut s = AnimatedSprite::new(
            15,
            20,
            &[
                Animation::new("inf", 0, 4, f32::INFINITY),
                Animation::new("nan", 0, 4, f32::NAN),
                Animation::new("fast", 0, 4, 1e12),
            ],
            true,
        );
        for animation in 0..3 {
            s.current = animation;
            s.update(1.0);
            s.update(f32::MAX);
            assert!(s.frame_index() < 4);
        }
    }

    #[test]
    fn a_long_frame_skips_frames() {
        let mut s = sheet();
        s.update(0.375);
        assert_eq!(s.frame_index(), 3);
    }

    #[test]
    fn play_of_the_current_animation_resumes_a_paused_clip_in_place() {
        let mut s = sheet();
        s.update(0.375);
        assert_eq!(s.frame_index(), 3);
        s.playing = false;
        assert!(s.play("idle"));
        assert!(s.playing);
        assert_eq!(s.frame_index(), 3);
    }

    #[test]
    fn play_once_holds_the_last_frame() {
        let mut s = sheet();
        assert!(s.play("jump"));
        for _ in 0..20 {
            s.update(0.1);
        }
        assert!(s.finished());
        assert!(s.is_last_frame());
        assert_eq!(s.frame().source_rect, [30, 40, 15, 20]);
        // Replaying a finished animation restarts it.
        assert!(s.play("jump"));
        assert_eq!(s.frame_index(), 0);
        assert!(!s.play("missing"));
    }

    #[test]
    fn stopped_sprites_do_not_advance_and_offsets_apply() {
        let mut s = sheet();
        s.playing = false;
        s.sheet_offset = [100, 50];
        s.update(1.0);
        assert_eq!(s.frame().source_rect, [100, 50, 15, 20]);
        s.set_animation(1);
        s.set_frame(7);
        assert_eq!(s.frame_index(), 1);
        assert_eq!(s.frame().source_rect, [115, 90, 15, 20]);
    }

    #[test]
    fn a_degenerate_frame_rate_does_not_spin() {
        for fps in [f32::INFINITY, f32::NAN, 1.0e30] {
            let mut s = AnimatedSprite::new(15, 20, &[Animation::new("a", 0, 4, fps)], true);
            s.update(0.016);
            assert!(s.frame_index() < 4);
        }
    }

    #[test]
    fn a_huge_step_count_resolves_in_constant_time() {
        let mut s = AnimatedSprite::new(15, 20, &[Animation::new("a", 0, 4, 1.0e9)], true);
        s.update(1.0);
        assert!(s.frame_index() < 4);
        let mut once = AnimatedSprite::new(15, 20, &[Animation::new("a", 0, 4, 1.0e9).once()], true);
        once.update(1.0);
        assert!(once.finished());
        assert_eq!(once.frame_index(), 3);
    }

    #[test]
    fn huge_rates_and_steps_neither_spin_nor_overflow() {
        // Looping: two huge ticks in a row used to overflow the frame counter.
        let mut s = AnimatedSprite::new(
            15,
            20,
            &[
                Animation::new("run", 0, 4, 1e30),
                Animation::new("hit", 0, 4, 1e12).once(),
            ],
            true,
        );
        s.update(1.0);
        s.update(1.0);
        assert!(s.frame_index() < 4);
        // Play-once: the clip must finish on its last frame at once instead of looping for ages.
        assert!(s.play("hit"));
        s.update(f32::MAX);
        assert!(s.finished());
        assert_eq!(s.frame_index(), 3);
        // An infinite step is saturated, not propagated.
        assert!(s.play("run"));
        s.update(f32::INFINITY);
        assert!(s.frame_index() < 4);
    }

    #[test]
    fn one_second_at_three_fps_is_three_steps_however_it_is_sliced() {
        // 1/3 is not a binary fraction: the period must be derived in f64 so one 1 s tick and
        // three 1/3 s ticks agree.
        let clip = || AnimatedSprite::new(1, 1, &[Animation::new("c", 0, 4, 3.0).once()], true);
        let mut whole = clip();
        whole.update(1.0);
        let mut sliced = clip();
        for _ in 0..3 {
            sliced.update(1.0 / 3.0);
        }
        assert_eq!(whole.frame_index(), 3);
        assert_eq!(whole.frame_index(), sliced.frame_index());
        assert_eq!(whole.finished(), sliced.finished());
    }

    #[test]
    fn set_animation_rescales_the_leftover_clock_to_the_new_period() {
        let mut s = AnimatedSprite::new(
            8,
            8,
            &[
                Animation::new("idle", 0, 20, 1.0),
                Animation::new("run", 1, 20, 12.0),
            ],
            true,
        );
        s.update(0.9); // 0.9 of a one-second step spent
        assert_eq!(s.frame_index(), 0);
        s.set_animation(1);
        s.update(0.016); // 0.9 of a step + 0.19 of a step: one frame, not eleven
        assert_eq!(s.frame_index(), 1);
    }

    #[test]
    fn closed_form_stepping_matches_single_steps() {
        let mut s = sheet();
        s.update(0.125 * 5.0); // 5 steps of 1/8 s on a 4-frame loop → frame 1
        assert_eq!(s.frame_index(), 1);
        assert!(s.play("jump"));
        s.update(0.2); // 2 steps at 10 fps: frame 2 of 3, not finished
        assert_eq!(s.frame_index(), 2);
        assert!(!s.finished());
        s.update(0.1); // one more step past the last frame finishes it
        assert!(s.finished());
    }

    #[test]
    fn set_animation_wraps_the_kept_frame_and_does_not_reset_the_clock() {
        let mut s = AnimatedSprite::new(
            8,
            8,
            &[
                Animation::new("idle", 0, 4, 8.0),
                Animation::new("hit", 1, 3, 8.0).once(),
            ],
            true,
        );
        // 0.4375 s at 8 fps is 3.5 frames: frame 3, with 0.0625 s left on the clock.
        s.update(0.4375);
        assert_eq!(s.frame_index(), 3);
        assert_eq!(s.current_animation(), 0);
        s.set_animation(9);
        assert_eq!(s.current_animation(), 0);
        assert_eq!(s.frame_index(), 3);
        s.set_animation(1);
        assert_eq!(s.current_animation(), 1);
        assert_eq!(s.animation().map(|a| a.name.as_str()), Some("hit"));
        assert!(!s.animation().expect("hit").looping);
        assert_eq!(s.frame_index(), 0);
        assert!(!s.finished());
        // The leftover 0.0625 s completes one step of the new clip.
        s.update(0.0625);
        assert_eq!(s.frame_index(), 1);
        assert!(!s.is_last_frame());
        assert!(!s.finished());
        s.update(0.125);
        assert_eq!(s.frame_index(), 2);
        assert!(s.is_last_frame());
        assert!(!s.finished());
        s.update(0.125);
        assert!(s.finished());
        assert_eq!(s.frame_index(), 2);
        assert_eq!(s.frame().source_rect, [16, 8, 8, 8]);
    }

    #[test]
    fn a_non_positive_rate_or_an_empty_sheet_does_not_advance() {
        let mut s = AnimatedSprite::new(1, 1, &[Animation::new("a", 0, 4, 8.0)], true);
        s.update(0.0625);
        assert_eq!(s.frame_index(), 0);
        s.animations[0].fps = 0.0;
        s.update(10.0);
        assert_eq!(s.frame_index(), 0);
        s.animations[0].fps = -8.0;
        s.update(10.0);
        assert_eq!(s.frame_index(), 0);
        s.animations[0].fps = 8.0;
        s.update(-1.0);
        s.update(f32::NAN);
        assert_eq!(s.frame_index(), 0);
        s.update(0.0625);
        assert_eq!(s.frame_index(), 1);

        let mut once = AnimatedSprite::new(2, 3, &[Animation::new("hold", 4, 0, 4.0).once()], true);
        assert_eq!(once.animation().expect("hold").frames, 1);
        assert!(once.is_last_frame());
        once.set_frame(5);
        assert_eq!(once.frame_index(), 0);
        once.update(0.0);
        assert!(!once.finished());
        once.update(0.25);
        assert!(once.finished());
        assert_eq!(once.frame_index(), 0);
        assert_eq!(once.frame().source_rect, [0, 12, 2, 3]);
        once.update(1.0);
        assert_eq!(once.frame_index(), 0);
        once.set_frame(3);
        assert!(!once.finished());
        assert_eq!(once.frame_index(), 0);

        let mut empty = AnimatedSprite::new(4, 6, &[], false);
        assert!(empty.animation().is_none());
        assert!(!empty.is_last_frame());
        assert!(!empty.finished());
        empty.update(3.0);
        empty.set_animation(0);
        empty.set_frame(9);
        assert_eq!(empty.current_animation(), 0);
        assert_eq!(empty.frame_index(), 0);
        assert_eq!(empty.frame().source_rect, [0, 0, 4, 6]);
        assert!(!empty.play("missing"));
        assert!(!empty.playing);
    }

    /// The frame count after any slicing is `floor(total_ns * fps / 1e9)` exactly, where
    /// `total_ns` is the sum of the per-update rounded nanoseconds (ADR-5959). A clock that keeps
    /// a rounded nanosecond leftover per update drifts by up to 1 ns per call and takes a step
    /// early or late when the total lands within a few nanoseconds of a frame edge.
    #[test]
    fn the_frame_count_is_the_floor_of_the_summed_nanoseconds_for_any_slicing() {
        // Rates whose f32 value is an exact decimal fraction num / den.
        for (fps, num, den) in [
            (72.0_f32, 72_u128, 1_u128),
            (7.0, 7, 1),
            (12.5, 25, 2),
            (3.0, 3, 1),
            (30.0, 30, 1),
            (24.0, 24, 1),
            (0.75, 3, 4),
        ] {
            let mut seed = 0x2545_f491_u64 ^ u64::from(fps.to_bits());
            let mut s = AnimatedSprite::new(1, 1, &[Animation::new("a", 0, 1 << 30, fps)], true);
            let mut total_ns = 0_u128;
            for call in 0..4000 {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                // 0 to 0.13 s in ms-ish slices, with zero-length updates mixed in.
                let dt = if call % 7 == 3 {
                    0.0
                } else {
                    ((seed >> 40) % 130_000) as f32 * 1.0e-6
                };
                s.update(dt);
                total_ns += (f64::from(dt) * 1.0e9).round() as u128;
                let expected = total_ns * num / (1_000_000_000 * den);
                assert_eq!(
                    u128::from(s.frame_index()),
                    expected % (1 << 30),
                    "{fps} fps after {call} calls, {total_ns} ns"
                );
            }
            // The adversarial last step: land a few nanoseconds below the second frame edge
            // ahead. A clock that drifted by even that much over the calls above steps early.
            let frames_now = total_ns * num / (1_000_000_000 * den);
            let edge = ((frames_now + 2) * 1_000_000_000 * den).div_ceil(num) - 1;
            let mut dt = ((edge - total_ns) as f64 / 1.0e9) as f32;
            let rounded = |dt: f32| (f64::from(dt) * 1.0e9).round() as u128;
            while total_ns + rounded(dt) > edge {
                dt = f32::from_bits(dt.to_bits() - 1);
            }
            total_ns += rounded(dt);
            assert!(
                edge - total_ns < 256,
                "{fps} fps: {} ns below the edge",
                edge - total_ns
            );
            s.update(dt);
            assert_eq!(
                u128::from(s.frame_index()),
                (frames_now + 1) % (1 << 30),
                "{fps} fps: {} ns below a frame edge is not that frame yet",
                edge + 1 - total_ns
            );
        }
    }

    /// Switching animations keeps the fraction of the current frame, not the cycle-reduced clock:
    /// at 3 fps the clock runs over a whole second (three frames), so 0.5 s is frame 1 plus half
    /// a frame, which is half of a 12 fps frame (41.7 ms) after the switch, not 1.5 frames.
    #[test]
    fn set_animation_keeps_the_phase_of_the_current_frame() {
        let mut s = AnimatedSprite::new(
            8,
            8,
            &[
                Animation::new("slow", 0, 20, 3.0),
                Animation::new("fast", 1, 20, 12.0),
            ],
            true,
        );
        s.update(0.5);
        assert_eq!(s.frame_index(), 1);
        s.set_animation(1);
        s.update(0.04);
        assert_eq!(s.frame_index(), 1, "half a frame + 40 ms is not a step yet");
        s.update(0.002);
        assert_eq!(s.frame_index(), 2, "half a frame + 42 ms is one step");
    }
}
