// SPDX-License-Identifier: MPL-2.0
//! The clip preview's playback options and overlays (spec addendum §§ 11.2-11.3; P-207, P-211,
//! P-213, P-215, P-216): speed, loop / once / ping-pong over a trimmed preview range, onion-skin
//! ghost poses, a floor that scrolls with the clip's extracted root motion with the trajectory
//! drawn on it, and the clip picker's search and sort.
//!
//! All of it is view state on [`super::AnimUi`]: it never edits the clip asset or the
//! `.animedit.ron` sidecar and is never on the undo stack. The trimmed range only bounds what the
//! preview plays.

use regia_animation::clip::AnimationClip;
use regia_animation::skeleton::Skeleton;
use regia_core::math::{Mat4, Quat, Vec3};

use super::PreviewRig;

/// Slowest preview speed.
pub const MIN_SPEED: f32 = 0.1;
/// Fastest preview speed.
pub const MAX_SPEED: f32 = 4.0;
/// Most ghost poses on each side of the playhead.
pub const MAX_GHOSTS: u8 = 8;

/// What the preview does at the end of its range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipLoop {
    /// Stop at the end.
    Once,
    /// Wrap back to the start.
    #[default]
    Loop,
    /// Play back and forth.
    PingPong,
}

impl ClipLoop {
    /// Every mode, in the order the panel lists them.
    pub const ALL: [ClipLoop; 3] = [ClipLoop::Once, ClipLoop::Loop, ClipLoop::PingPong];

    /// The agent-protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ClipLoop::Once => "once",
            ClipLoop::Loop => "loop",
            ClipLoop::PingPong => "ping_pong",
        }
    }

    /// The panel label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ClipLoop::Once => "Once",
            ClipLoop::Loop => "Loop",
            ClipLoop::PingPong => "Ping-pong",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }
}

/// Onion skinning: ghost poses before and after the playhead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OnionSkin {
    /// Draw the ghosts.
    pub enabled: bool,
    /// Ghosts before the playhead (0..=[`MAX_GHOSTS`]).
    pub before: u8,
    /// Ghosts after the playhead (0..=[`MAX_GHOSTS`]).
    pub after: u8,
    /// Seconds between ghosts.
    pub step: f32,
    /// Opacity of the nearest ghost (0..=1); farther ghosts fade.
    pub opacity: f32,
}

impl Default for OnionSkin {
    fn default() -> Self {
        Self {
            enabled: false,
            before: 2,
            after: 2,
            step: 0.1,
            opacity: 0.5,
        }
    }
}

/// The clip preview's playback and overlay options.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipPreviewOptions {
    /// Playback speed ([`MIN_SPEED`]..=[`MAX_SPEED`]).
    pub speed: f32,
    /// End-of-range behavior.
    pub looping: ClipLoop,
    /// The trimmed preview range `[start, end]` in seconds; `None` plays the whole clip.
    pub range: Option<[f32; 2]>,
    /// Ping-pong is playing backwards.
    pub reverse: bool,
    /// Ghost poses.
    pub onion: OnionSkin,
    /// Scroll the floor with the extracted root motion and draw the trajectory on it.
    pub root_floor: bool,
}

impl Default for ClipPreviewOptions {
    fn default() -> Self {
        Self {
            speed: 1.0,
            looping: ClipLoop::Loop,
            range: None,
            reverse: false,
            onion: OnionSkin::default(),
            root_floor: true,
        }
    }
}

impl ClipPreviewOptions {
    /// The range played for a clip of `duration` seconds: the trimmed range clamped into the
    /// clip, or the whole clip.
    #[must_use]
    pub fn range_of(&self, duration: f32) -> (f32, f32) {
        let duration = duration.max(0.0);
        match self.range {
            Some([a, b]) => {
                let a = a.clamp(0.0, duration);
                let b = b.clamp(a, duration);
                (a, b)
            }
            None => (0.0, duration),
        }
    }

    /// Sets the trimmed range (checked).
    ///
    /// # Errors
    ///
    /// Not finite, or `start >= end`.
    pub fn set_range(&mut self, range: Option<[f32; 2]>) -> Result<(), String> {
        if let Some([a, b]) = range
            && !(a.is_finite() && b.is_finite() && a >= 0.0 && a < b)
        {
            return Err(format!("a preview range needs 0 <= start < end, got [{a}, {b}]"));
        }
        self.range = range;
        Ok(())
    }

    /// Moves the end of the preview range (the timeline's end handle), kept a frame past the start.
    pub fn drag_end(&mut self, end: f32, duration: f32) {
        let (a, _) = self.range_of(duration);
        let end = end.clamp(a + 1.0 / 120.0, duration.max(a + 1.0 / 120.0));
        self.range = if (end - duration).abs() < 1e-4 && a == 0.0 {
            None
        } else {
            Some([a, end])
        };
    }

    /// Advances the playhead `time` by `dt` real seconds (times [`Self::speed`]) within the
    /// range of a clip of `duration`. A once-through preview clears `playing` at the end.
    pub fn advance(&mut self, time: f32, dt: f32, duration: f32, playing: &mut bool) -> f32 {
        let (a, b) = self.range_of(duration);
        let length = b - a;
        if length <= 1e-6 {
            return a;
        }
        let step = dt.max(0.0) * self.speed.clamp(MIN_SPEED, MAX_SPEED);
        let t = time.clamp(a, b);
        match self.looping {
            ClipLoop::Once => {
                let t = t + step;
                if t >= b {
                    *playing = false;
                    b
                } else {
                    t
                }
            }
            ClipLoop::Loop => {
                let t = t + step;
                if t > b { a + (t - a) % length } else { t }
            }
            ClipLoop::PingPong => {
                // A phase over one back-and-forth period: [0, L] forward, (L, 2L) backward. A step
                // longer than the range reflects as many times as it needs.
                let period = 2.0 * length;
                let phase = if self.reverse { period - (t - a) } else { t - a };
                let phase = (phase + step).rem_euclid(period);
                if phase <= length {
                    self.reverse = false;
                    a + phase
                } else {
                    self.reverse = true;
                    a + period - phase
                }
            }
        }
    }

    /// `(offset, time)` of each ghost around the playhead `time` (negative offsets before it),
    /// nearest first on each side. Ghosts wrap into the range when the preview loops and are
    /// left out past its ends otherwise.
    #[must_use]
    pub fn onion_times(&self, time: f32, duration: f32) -> Vec<(i32, f32)> {
        let onion = &self.onion;
        if !onion.enabled || onion.step <= 0.0 {
            return Vec::new();
        }
        let (a, b) = self.range_of(duration);
        let length = b - a;
        let mut out = Vec::new();
        for (count, sign) in [(onion.before, -1i32), (onion.after, 1i32)] {
            for k in 1..=i32::from(count.min(MAX_GHOSTS)) {
                let t = time + (sign * k) as f32 * onion.step;
                let t = if (a..=b).contains(&t) {
                    t
                } else if self.looping == ClipLoop::Loop && length > 1e-6 {
                    a + (t - a).rem_euclid(length)
                } else {
                    continue;
                };
                out.push((sign * k, t));
            }
        }
        out
    }
}

/// How the clip picker orders clips.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipSort {
    /// Import order.
    #[default]
    Imported,
    /// By name, case-insensitive.
    Name,
    /// Longest first.
    Length,
    /// Most recently picked first, then import order.
    Recent,
}

impl ClipSort {
    /// Every order, in the order the picker lists them.
    pub const ALL: [ClipSort; 4] = [
        ClipSort::Imported,
        ClipSort::Name,
        ClipSort::Length,
        ClipSort::Recent,
    ];

    /// The agent-protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ClipSort::Imported => "imported",
            ClipSort::Name => "name",
            ClipSort::Length => "length",
            ClipSort::Recent => "recent",
        }
    }

    /// The picker label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ClipSort::Imported => "Imported",
            ClipSort::Name => "Name",
            ClipSort::Length => "Length",
            ClipSort::Recent => "Recent",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// The clip picker's search text, sort and recently picked clips.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipListOptions {
    /// Case-insensitive substring filter on clip names.
    pub filter: String,
    /// Order.
    pub sort: ClipSort,
    /// Picked clips, most recent first (at most 32).
    pub recent: Vec<String>,
}

impl ClipListOptions {
    /// Notes a picked clip for [`ClipSort::Recent`].
    pub fn picked(&mut self, name: &str) {
        self.recent.retain(|n| n != name);
        self.recent.insert(0, name.to_string());
        self.recent.truncate(32);
    }

    /// The rig's clip names that match the filter, in the chosen order.
    #[must_use]
    pub fn list(&self, rig: &PreviewRig) -> Vec<String> {
        let needle = self.filter.trim().to_lowercase();
        let mut clips: Vec<(usize, &AnimationClip)> = rig
            .clips
            .iter()
            .enumerate()
            .map(|(i, c)| (i, &**c))
            .filter(|(_, c)| needle.is_empty() || c.name.to_lowercase().contains(&needle))
            .collect();
        match self.sort {
            ClipSort::Imported => {}
            ClipSort::Name => clips.sort_by_cached_key(|(i, c)| (c.name.to_lowercase(), *i)),
            ClipSort::Length => {
                clips.sort_by(|a, b| b.1.duration.total_cmp(&a.1.duration).then(a.0.cmp(&b.0)))
            }
            ClipSort::Recent => clips.sort_by_key(|(i, c)| {
                (
                    self.recent
                        .iter()
                        .position(|n| *n == c.name)
                        .unwrap_or(usize::MAX),
                    *i,
                )
            }),
        }
        clips.into_iter().map(|(_, c)| c.name.clone()).collect()
    }
}

/// The root's position and yaw at `time`, integrated from `start` (object space).
#[must_use]
pub fn root_at(clip: &AnimationClip, start: f32, time: f32) -> (Vec3, Quat) {
    let mut position = Vec3::ZERO;
    let mut yaw = Quat::IDENTITY;
    if time <= start {
        return (position, yaw);
    }
    // Small steps so a turning clip's path follows the turn.
    let steps = ((time - start) * 60.0).ceil().max(1.0) as usize;
    for i in 0..steps {
        let t0 = start + (time - start) * i as f32 / steps as f32;
        let t1 = start + (time - start) * (i + 1) as f32 / steps as f32;
        let Some((dt, dr)) = clip.root_motion_delta(t0, t1, false) else {
            break;
        };
        position += yaw * dt;
        yaw = (yaw * dr).normalize();
    }
    (position, yaw)
}

/// The floor seen from the rig, which the clip preview keeps in place: grid lines (pairs of
/// points) around the root's position at `time` and the root's path over the preview range,
/// both moved into the rig's frame at `time`. A clip without root motion gives a fixed grid and
/// no path. One integration pass over the range (1/60 s steps, at most 4096), so the cost does not
/// grow with the number of path points.
#[must_use]
pub fn floor_lines(
    clip: &AnimationClip,
    range: (f32, f32),
    time: f32,
    spacing: f32,
    half_lines: i32,
) -> (Vec<[Vec3; 2]>, Vec<Vec3>) {
    let _span = tracing::info_span!(target: "regia_editor", "anim_clip_floor").entered();
    let has_motion = clip
        .root_motion
        .as_ref()
        .is_some_and(|r| r.mode != regia_animation::clip::RootMotionMode::None);
    let (a, b) = (range.0, range.1.max(range.0));
    let time = time.clamp(a, b);
    let steps = (((b - a) * 60.0).ceil() as usize).clamp(1, 4096);
    let dt = (b - a) / steps as f32;
    let mut position = Vec3::ZERO;
    let mut yaw = Quat::IDENTITY;
    let mut here = (Vec3::ZERO, Quat::IDENTITY);
    let mut raw = Vec::with_capacity(steps + 1);
    raw.push(Vec3::ZERO);
    if has_motion && dt > 0.0 {
        for i in 0..steps {
            let t0 = a + dt * i as f32;
            let t1 = if i + 1 == steps { b } else { t0 + dt };
            // The playhead inside this step: the partial step gives the root at `time` exactly.
            if t0 <= time && time < t1 {
                here = match clip.root_motion_delta(t0, time, false) {
                    Some((d, r)) => (position + yaw * d, (yaw * r).normalize()),
                    None => (position, yaw),
                };
            }
            let Some((d, r)) = clip.root_motion_delta(t0, t1, false) else {
                break;
            };
            position += yaw * d;
            yaw = (yaw * r).normalize();
            raw.push(position);
        }
        if time >= b {
            here = (position, yaw);
        }
    }
    let (here, yaw) = here;
    let to_rig = |p: Vec3| yaw.inverse() * (p - here);
    let spacing = spacing.max(1e-3);
    let reach = spacing * half_lines as f32;
    let (cx, cz) = ((here.x / spacing).round(), (here.z / spacing).round());
    let mut grid = Vec::new();
    for i in -half_lines..=half_lines {
        let x = (cx + i as f32) * spacing;
        let z = (cz + i as f32) * spacing;
        grid.push([
            to_rig(Vec3::new(x, 0.0, here.z - reach)),
            to_rig(Vec3::new(x, 0.0, here.z + reach)),
        ]);
        grid.push([
            to_rig(Vec3::new(here.x - reach, 0.0, z)),
            to_rig(Vec3::new(here.x + reach, 0.0, z)),
        ]);
    }
    let path = if has_motion && raw.len() > 1 {
        // At most 64 drawn points, plus the last one.
        let every = raw.len().div_ceil(64);
        let mut path: Vec<Vec3> = raw
            .iter()
            .step_by(every)
            .map(|p| to_rig(Vec3::new(p.x, 0.0, p.z)))
            .collect();
        if !(raw.len() - 1).is_multiple_of(every)
            && let Some(last) = raw.last()
        {
            path.push(to_rig(Vec3::new(last.x, 0.0, last.z)));
        }
        path
    } else {
        Vec::new()
    };
    (grid, path)
}

/// Projects object-space points with the preview's orbit camera framed on `globals` into `rect`
/// (the same camera as `preview::project`, for points that are not joints).
#[must_use]
pub fn project_points(
    globals: &[Mat4],
    orbit: [f32; 3],
    rect: egui::Rect,
    points: &[Vec3],
) -> Vec<egui::Pos2> {
    let (eye, target, _) = super::preview::orbit_camera(globals, orbit);
    let view = regia_core::glam::camera::rh::view::look_at_mat4(eye, target, Vec3::Y);
    let aspect = rect.width() / rect.height().max(1.0);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(
        std::f32::consts::FRAC_PI_4,
        aspect.max(0.01),
        0.01,
        1000.0,
    );
    let vp = proj * view;
    points
        .iter()
        .map(|p| {
            let p = vp.project_point3(*p);
            egui::pos2(
                rect.center().x + p.x * rect.width() * 0.5,
                rect.center().y - p.y * rect.height() * 0.5,
            )
        })
        .collect()
}

/// The ghost poses' object-space joint matrices, `(offset, globals)` per ghost.
#[must_use]
pub fn ghost_globals(
    options: &ClipPreviewOptions,
    clip: &AnimationClip,
    skeleton: &Skeleton,
    time: f32,
) -> Vec<(i32, Vec<Mat4>)> {
    options
        .onion_times(time, clip.duration)
        .into_iter()
        .map(|(offset, t)| {
            let mut pose = skeleton.bind_pose_pose();
            clip.sample_pose(t, &mut pose);
            (offset, skeleton.globals(&pose, None))
        })
        .collect()
}

/// Paints the floor (grid and root path) and the ghost skeletons over the clip preview in
/// `rect`. Colors come from the theme: the grid from the overlay grid tokens, the path from the
/// active accent, ghosts before the playhead from the z-axis gizmo token and after it from the
/// ok accent, faded with distance.
#[allow(clippy::too_many_arguments)] // the preview's camera, rig and overlay data, passed once
pub fn paint_overlays(
    painter: &egui::Painter,
    rect: egui::Rect,
    skeleton: &Skeleton,
    globals: &[Mat4],
    orbit: [f32; 3],
    options: &ClipPreviewOptions,
    floor: Option<&(Vec<[Vec3; 2]>, Vec<Vec3>)>,
    ghosts: &[(i32, Vec<Mat4>)],
) {
    let _span = tracing::info_span!(target: "regia_editor", "anim_clip_overlays").entered();
    let tokens = crate::theme::ui_tokens(painter.ctx());
    let ground = globals
        .iter()
        .map(|m| m.w_axis.y)
        .fold(f32::MAX, f32::min)
        .min(0.0);
    if let Some((grid, path)) = floor {
        let lift = |p: Vec3| Vec3::new(p.x, ground, p.z);
        let ends: Vec<Vec3> = grid.iter().flat_map(|[a, b]| [lift(*a), lift(*b)]).collect();
        let screen = project_points(globals, orbit, rect, &ends);
        for [a, b] in screen.as_chunks::<2>().0 {
            painter.line_segment([*a, *b], egui::Stroke::new(1.0, tokens.overlay.grid_minor));
        }
        if path.len() > 1 {
            let lifted: Vec<Vec3> = path.iter().map(|p| lift(*p)).collect();
            painter.add(egui::Shape::line(
                project_points(globals, orbit, rect, &lifted),
                egui::Stroke::new(2.0, tokens.accents.active),
            ));
        }
    }
    let nearest = f32::from(options.onion.before.max(options.onion.after).max(1));
    for (offset, ghost) in ghosts {
        let base = if *offset < 0 {
            tokens.overlay.gizmo_z
        } else {
            tokens.accents.ok
        };
        let fade = 1.0 - (offset.unsigned_abs() as f32 - 1.0) / nearest;
        let color = base.gamma_multiply((options.onion.opacity * fade).clamp(0.05, 1.0));
        let points = project_points(
            globals,
            orbit,
            rect,
            &ghost.iter().map(|m| m.w_axis.truncate()).collect::<Vec<_>>(),
        );
        for (i, parent) in skeleton.parents.iter().enumerate() {
            if let (Some(a), Some(b)) = (points.get(i), parent.and_then(|p| points.get(p as usize))) {
                painter.line_segment([*a, *b], egui::Stroke::new(2.0, color));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_loop_once_and_ping_pong_stay_in_the_range() {
        let mut o = ClipPreviewOptions::default();
        let mut playing = true;
        // Loop wraps; speed scales the step.
        o.speed = 2.0;
        let t = o.advance(0.9, 0.1, 1.0, &mut playing);
        assert!((t - 0.1).abs() < 1e-5, "{t}");
        assert!(playing);
        // The speed is clamped to the preview's limits.
        o.speed = 100.0;
        let t = o.advance(0.0, 0.1, 10.0, &mut playing);
        assert!((t - 0.4).abs() < 1e-5, "{t}");
        // Once stops at the end of the trimmed range.
        o.speed = 1.0;
        o.looping = ClipLoop::Once;
        o.set_range(Some([0.2, 0.5])).expect("range");
        let t = o.advance(0.45, 0.1, 1.0, &mut playing);
        assert_eq!(t, 0.5);
        assert!(!playing);
        // A playhead outside the range starts inside it.
        playing = true;
        let t = o.advance(0.9, 0.0, 1.0, &mut playing);
        assert_eq!(t, 0.5);
        // Ping-pong reflects at the end and comes back.
        o.looping = ClipLoop::PingPong;
        playing = true;
        let t = o.advance(0.45, 0.1, 1.0, &mut playing);
        assert!((t - 0.45).abs() < 1e-5, "{t}");
        assert!(o.reverse, "bounced off the end");
        let t = o.advance(t, 0.1, 1.0, &mut playing);
        assert!((t - 0.35).abs() < 1e-5, "{t}");
        assert!(o.reverse);
        let t = o.advance(0.25, 0.1, 1.0, &mut playing);
        assert!((t - 0.25).abs() < 1e-5, "{t}: bounced off the start");
        assert!(!o.reverse);
        assert!(playing);
        // Bad ranges are refused and change nothing.
        assert!(o.set_range(Some([0.5, 0.5])).is_err());
        assert!(o.set_range(Some([f32::NAN, 1.0])).is_err());
        assert_eq!(o.range, Some([0.2, 0.5]));
    }

    #[test]
    fn dragging_the_end_trims_the_preview_not_the_clip() {
        let mut o = ClipPreviewOptions::default();
        o.drag_end(0.6, 1.0);
        assert_eq!(o.range_of(1.0), (0.0, 0.6));
        o.drag_end(5.0, 1.0);
        assert_eq!(o.range, None, "dragging back to the end restores the whole clip");
        o.drag_end(-1.0, 1.0);
        let (a, b) = o.range_of(1.0);
        assert!(b > a, "the end stays after the start");
    }

    #[test]
    fn onion_times_wrap_when_looping_and_stop_at_ends_otherwise() {
        let mut o = ClipPreviewOptions::default();
        assert!(o.onion_times(0.5, 1.0).is_empty(), "off by default");
        o.onion = OnionSkin {
            enabled: true,
            before: 2,
            after: 1,
            step: 0.25,
            opacity: 0.5,
        };
        let times = o.onion_times(0.1, 1.0);
        assert_eq!(times.len(), 3);
        assert_eq!(times[0].0, -1);
        assert!((times[0].1 - 0.85).abs() < 1e-5, "{times:?}");
        o.looping = ClipLoop::Once;
        let times = o.onion_times(0.1, 1.0);
        assert_eq!(times, vec![(1, 0.35)]);
        o.onion.before = 200;
        assert!(o.onion_times(0.99, 100.0).len() <= usize::from(MAX_GHOSTS) + 1);
    }

    #[test]
    fn the_floor_scrolls_with_root_motion_and_the_path_runs_under_the_rig() {
        let rig = PreviewRig::demo();
        let walk = rig.clip("Walk").expect("walk");
        let range = (0.0, walk.duration);
        let (grid_start, path) = floor_lines(walk, range, 0.0, 0.5, 4);
        assert_eq!(grid_start.len(), 18);
        assert!(
            path.len() > 2 && path.len() <= 65,
            "the walk has root motion: {}",
            path.len()
        );
        assert!(path[0].length() < 1e-4, "the path starts under the rig at time 0");
        let (here, _) = root_at(walk, 0.0, walk.duration * 0.5);
        assert!(here.length() > 0.1, "the root travels: {here:?}");
        let (grid_mid, path_mid) = floor_lines(walk, range, walk.duration * 0.5, 0.5, 4);
        assert_ne!(grid_start, grid_mid, "the floor moved under the rig");
        // The path point at the playhead is under the rig.
        let under = path_mid.iter().map(|p| p.length()).fold(f32::MAX, f32::min);
        assert!(under < 0.05, "{under}");
    }

    #[test]
    fn clip_list_filters_and_sorts() {
        let rig = PreviewRig::demo();
        let mut list = ClipListOptions::default();
        let all = list.list(&rig);
        assert_eq!(all, rig.clip_names());
        list.filter = "UN".into();
        assert!(list.list(&rig).iter().all(|n| n.to_lowercase().contains("un")));
        list.filter.clear();
        list.sort = ClipSort::Name;
        let mut sorted = all.clone();
        sorted.sort_by_key(|n| n.to_lowercase());
        assert_eq!(list.list(&rig), sorted);
        list.sort = ClipSort::Length;
        let by_length = list.list(&rig);
        let lengths: Vec<f32> = by_length
            .iter()
            .map(|n| rig.clip(n).map_or(0.0, |c| c.duration))
            .collect();
        assert!(lengths.windows(2).all(|w| w[0] >= w[1]), "{lengths:?}");
        list.sort = ClipSort::Recent;
        let last = all.last().cloned().expect("clips");
        list.picked(&last);
        assert_eq!(list.list(&rig).first(), Some(&last));
        assert_eq!(ClipSort::from_name("length"), Some(ClipSort::Length));
        assert_eq!(ClipLoop::from_name("ping_pong"), Some(ClipLoop::PingPong));
    }
}
