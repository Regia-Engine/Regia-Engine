// SPDX-License-Identifier: MPL-2.0
//! Which frames of a profile draw through the visibility buffer (ADR-2071).
//!
//! [`crate::RenderProfile::visibility_buffer`] says the path may run. [`VisibilityPath::select`]
//! says when: [`VisibilitySelect::Always`] (what the switch has always meant), or
//! [`VisibilitySelect::Dense`], where a [`DenseGate`] watches how much rigid opaque geometry the
//! frame submits and hands the frame to the path only while the scene is dense. A sparse level
//! then keeps the classic forward path, with its LOD chains, and pays nothing for clusters.
//!
//! The gate counts the whole submitted list, not what the camera sees, so turning the camera does
//! not flip the path, and frames, not time, so a fixed-step run takes the same decisions on every
//! machine. The two thresholds and the two frame counts make flapping impossible by construction:
//! entering needs [`DenseSceneSelect::enter_frames`] frames in a row at or over a threshold, and
//! leaving needs [`DenseSceneSelect::leave_frames`] frames in a row under half of both.

/// When a scene counts as dense ([`VisibilitySelect::Dense`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DenseSceneSelect {
    /// Triangles of the frame's rigid opaque meshes at which the scene is dense.
    pub enter_triangles: u64,
    /// Rigid opaque meshes in the frame at which the scene is dense.
    pub enter_instances: u32,
    /// Frames in a row over a threshold before the path takes the frame (at least 1).
    pub enter_frames: u32,
    /// Frames in a row under half of both thresholds before the path gives the frame back (at
    /// least 1).
    pub leave_frames: u32,
}

impl DenseSceneSelect {
    /// The thresholds of [`crate::RenderProfile::dense_scenes`]: a starting point, not a measured
    /// crossover. The hardware bench (`tests/visbuffer_dense_bench.rs`, `book/agents/visbuffer.md`)
    /// decides them before a preset may use them.
    pub const DEFAULT: Self = Self {
        enter_triangles: 2_000_000,
        enter_instances: 4096,
        enter_frames: 4,
        leave_frames: 120,
    };
}

impl DenseSceneSelect {
    /// The thresholds of the dense-scene default ([`VisibilityPath::DENSE_DEFAULT`], ADR-3149),
    /// from the only card rows recorded (`docs/perf-log.md`, a 5090 at 2560x1440): the sparsest
    /// scene the per-material resolve won was 1024 spheres of 3,906 triangles (3,999,744
    /// triangles, 1.085 per pixel), so a scene enters at 4,000,000 triangles, never sparser than a
    /// measured win at 1440p; the mesh count stays at [`Self::DEFAULT`]'s 4096, where every
    /// recorded row with that many meshes won. The crossover was not bracketed (the path won the
    /// sparsest row too), so the 5080 and 7900 runs with the sparse rows replace these numbers
    /// ([`DenseCrossover::of_rows`], `book/agents/visbuffer.md`).
    pub const MEASURED: Self = Self {
        enter_triangles: 4_000_000,
        ..Self::DEFAULT
    };
}

impl Default for DenseSceneSelect {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// When a profile with [`crate::RenderProfile::visibility_buffer`] on takes the path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VisibilitySelect {
    /// Every frame the path can draw (the behavior of the switch alone).
    #[default]
    Always,
    /// Only while the scene is dense: entered after [`DenseSceneSelect::enter_frames`] frames in
    /// a row over a threshold, left after [`DenseSceneSelect::leave_frames`] under half of both.
    Dense(DenseSceneSelect),
}

impl VisibilitySelect {
    /// The name `render.get_visbuffer` reports (`always`, `dense`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Dense(_) => "dense",
        }
    }
}

/// The profile's visibility-path choices beyond the on/off switch (ADR-2071). The default changes
/// nothing: every frame the switch allows, shaded by one draw per material.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisibilityPath {
    /// Which frames take the path.
    pub select: VisibilitySelect,
    /// Shade every material in one full-screen draw through the texture table
    /// ([`SinglePassFallback`] says why a device cannot). Also asks the device for the table's
    /// features ([`crate::texture_table::set_device_level_request`]), so set it before the first
    /// window's device. Where it cannot run, the per-material resolve shades the same pixels.
    pub single_pass_resolve: bool,
}

impl VisibilityPath {
    /// The default, in a constant (the presets).
    pub const DEFAULT: Self = Self {
        select: VisibilitySelect::Always,
        single_pass_resolve: false,
    };

    /// What the dense-scene default ([`VisibilityDefault::Dense`], ADR-3149) gives a Desktop High
    /// or Ultra profile: frames take the path while the scene is dense, at
    /// [`DenseSceneSelect::MEASURED`], and shade through the per-material resolve. The single pass
    /// stays off: on the one card run recorded (`docs/perf-log.md`, a 5090 at 2560x1440) the
    /// per-material resolve beat the classic path on all four dense scenes, and the single pass
    /// lost on two of them.
    pub const DENSE_DEFAULT: Self = Self {
        select: VisibilitySelect::Dense(DenseSceneSelect::MEASURED),
        single_pass_resolve: false,
    };
}

/// The switch that routes dense scenes to the visibility buffer by default (ADR-3149):
/// [`crate::RenderSettings::visibility_default`]. Off ([`Self::Preset`]) by default, so every
/// preset keeps its row. On, a Desktop High or Ultra profile still at its preset's path row gets
/// [`VisibilityPath::DENSE_DEFAULT`] with the path switched on; a sparse level keeps drawing
/// forward, frame for frame the same, because the gate never hands it to the path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VisibilityDefault {
    /// The preset's row: the path stays as the profile has it.
    #[default]
    Preset,
    /// Dense scenes take the visibility buffer where the preset has been measured.
    Dense,
}

impl VisibilityDefault {
    /// The protocol name (`preset`, `dense`), `render.set_visbuffer { default }`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preset => "preset",
            Self::Dense => "dense",
        }
    }

    /// The switch named `name`, or `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "preset" => Some(Self::Preset),
            "dense" => Some(Self::Dense),
            _ => None,
        }
    }
}

/// Why the dense-scene default does or does not hold the profile's path
/// ([`crate::RenderSettings::visibility_default_state`]). Stable codes ([`Self::as_str`]), listed in
/// `book/agents/visbuffer.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisibilityDefaultState {
    /// The switch is on and the profile holds [`VisibilityPath::DENSE_DEFAULT`].
    Applied,
    /// The switch is off.
    SwitchOff,
    /// The profile is a preset the default has not been measured on (every one but Desktop High
    /// and Ultra), or no preset's row at all.
    PresetUnmeasured,
    /// The game set its own path (the switch, a selection, or the resolve): the default leaves it.
    ProfileChoice,
}

impl VisibilityDefaultState {
    /// The stable code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::SwitchOff => "switch_off",
            Self::PresetUnmeasured => "preset_unmeasured",
            Self::ProfileChoice => "profile_choice",
        }
    }
}

/// Why a frame that asked for the single-pass resolve shades per material. Stable codes
/// ([`Self::as_str`]), listed in `book/agents/visbuffer.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinglePassFallback {
    /// The device is off the texture table ([`crate::texture_table::TableFallback`] says why):
    /// the textures of two materials cannot be reached from one draw.
    NoTextureTable,
    /// Fewer storage buffers per fragment stage than the resolve's and the two material bindings.
    StorageLimits,
    /// An instance is a lightmap receiver: the per-material resolve has the `LIGHTMAP` variant, the
    /// single-pass module does not (ADR-2106).
    LightmapReceivers,
}

impl SinglePassFallback {
    /// The stable code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoTextureTable => "vis_no_texture_table",
            Self::StorageLimits => "vis_storage_limits",
            Self::LightmapReceivers => "vis_lightmap_receivers",
        }
    }
}

/// The geometry one frame submits, as the gate counts it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SceneDensity {
    /// Triangles of the frame's rigid opaque meshes.
    pub triangles: u64,
    /// Rigid opaque meshes in the frame.
    pub instances: u32,
}

impl SceneDensity {
    /// Adds one mesh. `index_count` is its index list's length.
    pub(crate) fn add(&mut self, index_count: usize) {
        self.triangles += (index_count / 3) as u64;
        self.instances = self.instances.saturating_add(1);
    }
}

/// What the selection did, for `render.get_visbuffer`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DenseGateReport {
    /// The last main-view frame's scene is taking the path.
    pub dense: bool,
    /// The last frame's counts.
    pub density: SceneDensity,
    /// Times the gate handed frames to the path.
    pub entered: u64,
    /// Times the gate gave frames back.
    pub left: u64,
}

/// The selection's memory ([`VisibilitySelect::Dense`]): pure logic, one call per main-view frame.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DenseGate {
    report: DenseGateReport,
    /// Frames in a row pointing the other way than `report.dense`.
    streak: u32,
}

impl DenseGate {
    /// Takes one frame's counts and returns whether the frame takes the path.
    pub(crate) fn step(&mut self, select: DenseSceneSelect, density: SceneDensity) -> bool {
        self.report.density = density;
        let over = density.triangles >= select.enter_triangles || density.instances >= select.enter_instances;
        let under = density.triangles.saturating_mul(2) < select.enter_triangles
            && u64::from(density.instances) * 2 < u64::from(select.enter_instances);
        let leaning_out = if self.report.dense { under } else { over };
        self.streak = if leaning_out { self.streak + 1 } else { 0 };
        let needed = if self.report.dense {
            select.leave_frames
        } else {
            select.enter_frames
        };
        if self.streak >= needed.max(1) {
            self.report.dense = !self.report.dense;
            self.streak = 0;
            if self.report.dense {
                self.report.entered += 1;
            } else {
                self.report.left += 1;
            }
        }
        self.report.dense
    }

    /// Back to a sparse scene: a profile that no longer selects by density.
    pub(crate) fn reset(&mut self) {
        self.report.dense = false;
        self.streak = 0;
    }

    /// What the selection did.
    pub(crate) fn report(&self) -> DenseGateReport {
        self.report
    }
}

/// Pixels per side of the tiles [`DensityMetrics::clusters_per_tile`] counts in.
pub const DENSITY_TILE_PX: u32 = 16;

/// How dense one frame is, in units that carry from one resolution to another (ADR-2689): the
/// numbers a preset's [`DenseSceneSelect`] is chosen from once the hardware bench
/// (`tests/visbuffer_dense_bench.rs`) has measured where the visibility path starts to win on a
/// card. The classic path's cost grows with the triangles it submits per pixel; the visibility
/// path's with the clusters it draws per screen tile, so the crossover is a pair of these
/// numbers, not a triangle count. [`DenseSceneSelect::at_density`] turns a chosen pair back into
/// the gate's thresholds for a frame size.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DensityMetrics {
    /// Triangles of the frame's rigid opaque meshes (what the gate counts, [`SceneDensity`]) per
    /// pixel of the frame.
    pub triangles_per_pixel: f64,
    /// Rigid opaque meshes per million pixels.
    pub instances_per_megapixel: f64,
    /// Clusters the visibility path drew (both cull phases and the software raster,
    /// [`super::VisBufferCounters`]) per [`DENSITY_TILE_PX`]² tile. Zero when the frame did not draw
    /// through the path.
    pub clusters_per_tile: f64,
}

impl DensityMetrics {
    /// The metrics of a `width × height` frame that submitted `density` and, when it drew through
    /// the path, whose cull counted `counters` (`FrameRenderer::visbuffer_counters`).
    #[must_use]
    pub fn of_frame(
        density: SceneDensity,
        counters: Option<super::VisBufferCounters>,
        width: u32,
        height: u32,
    ) -> Self {
        let pixels = (f64::from(width) * f64::from(height)).max(1.0);
        let tiles = (f64::from(width.div_ceil(DENSITY_TILE_PX))
            * f64::from(height.div_ceil(DENSITY_TILE_PX)))
        .max(1.0);
        let drawn = counters.map_or(0, |c| {
            u64::from(c.phase1_clusters) + u64::from(c.phase2_clusters) + u64::from(c.software_clusters)
        });
        Self {
            triangles_per_pixel: density.triangles as f64 / pixels,
            instances_per_megapixel: f64::from(density.instances) * 1.0e6 / pixels,
            clusters_per_tile: drawn as f64 / tiles,
        }
    }
}

impl DenseSceneSelect {
    /// The gate's thresholds for a `width × height` frame from a density chosen on the bench
    /// ([`DensityMetrics`]): a scene enters at `triangles_per_pixel` triangles per pixel or
    /// `instances_per_megapixel` meshes per million pixels, with [`Self::DEFAULT`]'s frame counts.
    /// The same density gives four times the thresholds at 4K as at 1080p.
    #[must_use]
    pub fn at_density(
        triangles_per_pixel: f64,
        instances_per_megapixel: f64,
        width: u32,
        height: u32,
    ) -> Self {
        let pixels = f64::from(width) * f64::from(height);
        let clamp_u64 = |v: f64| {
            if v.is_finite() && v > 0.0 {
                v.ceil().min(u64::MAX as f64) as u64
            } else {
                u64::MAX
            }
        };
        let instances = clamp_u64(instances_per_megapixel * pixels / 1.0e6).min(u64::from(u32::MAX)) as u32;
        Self {
            enter_triangles: clamp_u64(triangles_per_pixel * pixels).max(1),
            enter_instances: instances.max(1),
            ..Self::DEFAULT
        }
    }
}

/// One scene of the dense bench on one card (`tests/visbuffer_dense_bench.rs`): its density and
/// the GPU frame time of the classic path and of one visibility-path resolve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DenseBenchRow {
    /// The scene's density (only the triangle and mesh columns are read).
    pub density: DensityMetrics,
    /// GPU milliseconds per frame on the classic forward path.
    pub classic_ms: f64,
    /// GPU milliseconds per frame on the visibility path.
    pub path_ms: f64,
}

impl DenseBenchRow {
    /// The path is at least `margin` (a fraction) cheaper than the classic path on this row.
    #[must_use]
    pub fn path_wins(&self, margin: f64) -> bool {
        self.classic_ms.is_finite()
            && self.path_ms.is_finite()
            && self.path_ms > 0.0
            && self.path_ms <= self.classic_ms * (1.0 - margin)
    }
}

/// How much cheaper the path must be on a row before the row counts as won: 3 %, about the
/// spread of one binary between two 5090s, so a win inside the noise does not move a threshold.
pub const DENSE_CROSSOVER_MARGIN: f64 = 0.03;

/// Where the visibility path starts to win on one card, from its bench rows (ADR-3149): the
/// density a preset's [`DenseSceneSelect`] is chosen at ([`DenseSceneSelect::at_density`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DenseCrossover {
    /// Triangles per pixel of the sparsest row from which the path wins on every denser row.
    pub triangles_per_pixel: f64,
    /// Meshes per million pixels of that row.
    pub instances_per_megapixel: f64,
    /// Rows the path won, from that row up.
    pub rows_won: usize,
    /// A sparser row lost, so the crossover lies between that row and this one. `false`: the path
    /// won down to the sparsest row measured, and the real crossover may be sparser still (the
    /// threshold is then conservative: it never routes a scene the bench did not show winning).
    pub bracketed: bool,
}

impl DenseCrossover {
    /// The crossover of `rows`: rows sorted by triangles per pixel, the longest run of wins that
    /// ends at the densest row ([`DenseBenchRow::path_wins`] with `margin`), and its sparsest row.
    /// `None` when the path does not win the densest row (then no threshold is safe) or there are
    /// no rows. A loss above a win ends the run there, so a lone win among losses never sets it.
    #[must_use]
    pub fn of_rows(rows: &[DenseBenchRow], margin: f64) -> Option<Self> {
        let mut sorted: Vec<&DenseBenchRow> = rows
            .iter()
            .filter(|r| r.density.triangles_per_pixel.is_finite())
            .collect();
        sorted.sort_by(|a, b| {
            a.density
                .triangles_per_pixel
                .total_cmp(&b.density.triangles_per_pixel)
        });
        let won = sorted.iter().rev().take_while(|r| r.path_wins(margin)).count();
        if won == 0 {
            return None;
        }
        let first = sorted[sorted.len() - won];
        Some(Self {
            triangles_per_pixel: first.density.triangles_per_pixel,
            instances_per_megapixel: first.density.instances_per_megapixel,
            rows_won: won,
            bracketed: won < sorted.len(),
        })
    }

    /// The stricter of two cards' crossovers (the 5080 and the 7900): the denser of each column,
    /// so a scene routed to the path wins on both. Bracketed only when both are.
    #[must_use]
    pub fn stricter(self, other: Self) -> Self {
        Self {
            triangles_per_pixel: self.triangles_per_pixel.max(other.triangles_per_pixel),
            instances_per_megapixel: self.instances_per_megapixel.max(other.instances_per_megapixel),
            rows_won: self.rows_won.min(other.rows_won),
            bracketed: self.bracketed && other.bracketed,
        }
    }

    /// The gate's thresholds for a `width × height` frame at this crossover.
    #[must_use]
    pub fn select_at(self, width: u32, height: u32) -> DenseSceneSelect {
        DenseSceneSelect::at_density(
            self.triangles_per_pixel,
            self.instances_per_megapixel,
            width,
            height,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELECT: DenseSceneSelect = DenseSceneSelect {
        enter_triangles: 1000,
        enter_instances: 100,
        enter_frames: 3,
        leave_frames: 5,
    };

    fn density(triangles: u64, instances: u32) -> SceneDensity {
        SceneDensity { triangles, instances }
    }

    #[test]
    fn a_sparse_scene_never_takes_the_path() {
        let mut gate = DenseGate::default();
        for _ in 0..1000 {
            assert!(!gate.step(SELECT, density(999, 99)));
        }
        assert_eq!(gate.report().entered, 0);
    }

    #[test]
    fn entering_needs_consecutive_frames_over_either_threshold() {
        for over in [density(1000, 0), density(0, 100)] {
            let mut gate = DenseGate::default();
            assert!(!gate.step(SELECT, over));
            assert!(!gate.step(SELECT, over));
            assert!(gate.step(SELECT, over), "{over:?}: the third frame in a row");
            assert_eq!(gate.report().entered, 1);
        }
        // A sparse frame in between starts the count again.
        let mut gate = DenseGate::default();
        assert!(!gate.step(SELECT, density(1000, 0)));
        assert!(!gate.step(SELECT, density(1000, 0)));
        assert!(!gate.step(SELECT, density(0, 0)));
        assert!(!gate.step(SELECT, density(1000, 0)));
        assert!(!gate.step(SELECT, density(1000, 0)));
        assert!(gate.step(SELECT, density(1000, 0)));
    }

    #[test]
    fn leaving_needs_consecutive_frames_under_half_of_both() {
        let mut gate = DenseGate::default();
        for _ in 0..3 {
            gate.step(SELECT, density(1000, 0));
        }
        assert!(gate.report().dense);
        // Between half and the threshold, it stays forever (the band that stops flapping).
        for _ in 0..1000 {
            assert!(gate.step(SELECT, density(500, 49)));
            assert!(gate.step(SELECT, density(499, 50)));
        }
        for _ in 0..4 {
            assert!(gate.step(SELECT, density(499, 49)));
        }
        // One frame back over half starts the leave count again.
        assert!(gate.step(SELECT, density(500, 0)));
        for _ in 0..4 {
            assert!(gate.step(SELECT, density(0, 0)));
        }
        assert!(!gate.step(SELECT, density(0, 0)), "the fifth frame in a row");
        assert_eq!(gate.report().left, 1);
    }

    #[test]
    fn a_scene_at_the_threshold_never_flaps() {
        // Counts that wobble around the threshold: one entry, no leave.
        let mut gate = DenseGate::default();
        let mut flips = 0;
        let mut last = false;
        for frame in 0..10_000u64 {
            let now = gate.step(SELECT, density(990 + frame % 21, 0));
            flips += u32::from(now != last);
            last = now;
        }
        assert!(flips <= 1, "{flips} flips");
    }

    #[test]
    fn a_threshold_of_one_can_still_leave() {
        let select = DenseSceneSelect {
            enter_triangles: 1,
            enter_instances: 1,
            enter_frames: 1,
            leave_frames: 1,
        };
        let mut gate = DenseGate::default();
        assert!(gate.step(select, density(1, 1)));
        assert!(
            !gate.step(select, density(0, 0)),
            "an empty frame is under half of 1"
        );
    }

    #[test]
    fn zero_frame_counts_mean_one_frame() {
        let select = DenseSceneSelect {
            enter_frames: 0,
            leave_frames: 0,
            ..SELECT
        };
        let mut gate = DenseGate::default();
        assert!(gate.step(select, density(1000, 0)));
        assert!(!gate.step(select, density(0, 0)));
    }

    #[test]
    fn density_counts_triangles_and_instances() {
        let mut d = SceneDensity::default();
        d.add(36);
        d.add(3);
        assert_eq!(d, density(13, 2));
    }

    #[test]
    fn the_default_is_the_old_switch() {
        assert_eq!(VisibilityPath::default(), VisibilityPath::DEFAULT);
        assert_eq!(
            VisibilityPath::default(),
            VisibilityPath {
                select: VisibilitySelect::Always,
                single_pass_resolve: false,
            }
        );
    }

    #[test]
    fn codes_are_protocol_shaped() {
        for code in [
            SinglePassFallback::NoTextureTable.as_str(),
            SinglePassFallback::StorageLimits.as_str(),
            VisibilitySelect::Always.as_str(),
            VisibilitySelect::Dense(SELECT).as_str(),
        ] {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn density_metrics_carry_across_resolutions() {
        let counters = super::super::VisBufferCounters {
            phase1_clusters: 3000,
            phase2_clusters: 600,
            software_clusters: 240,
            ..Default::default()
        };
        let m = DensityMetrics::of_frame(density(4_147_200, 2_073), Some(counters), 1920, 1080);
        assert!((m.triangles_per_pixel - 2.0).abs() < 1e-9, "{m:?}");
        assert!((m.instances_per_megapixel - 999.71).abs() < 0.01, "{m:?}");
        // 120 × 68 tiles of 16 px (1080 rounds up to 68 rows).
        assert!(
            (m.clusters_per_tile - 3840.0 / (120.0 * 68.0)).abs() < 1e-9,
            "{m:?}"
        );
        // Not on the path: no clusters drawn.
        assert_eq!(
            DensityMetrics::of_frame(density(10, 1), None, 8, 8).clusters_per_tile,
            0.0
        );
        // A chosen density back into thresholds: four times the pixels, four times the counts.
        let hd = DenseSceneSelect::at_density(2.0, 1000.0, 1920, 1080);
        let uhd = DenseSceneSelect::at_density(2.0, 1000.0, 3840, 2160);
        assert_eq!(hd.enter_triangles, 4_147_200);
        assert_eq!(hd.enter_instances, 2_074);
        assert_eq!(uhd.enter_triangles, 4 * hd.enter_triangles);
        assert_eq!((hd.enter_frames, hd.leave_frames), (4, 120));
        // A scene exactly at the chosen density enters the gate.
        let mut gate = DenseGate::default();
        let at = density(hd.enter_triangles, 0);
        let entered = (0..hd.enter_frames).map(|_| gate.step(hd, at)).last();
        assert_eq!(entered, Some(true));
        // Off (zero or not finite) means never by that count.
        assert_eq!(
            DenseSceneSelect::at_density(0.0, f64::NAN, 64, 64).enter_triangles,
            u64::MAX
        );
    }

    fn row(triangles_per_pixel: f64, classic_ms: f64, path_ms: f64) -> DenseBenchRow {
        DenseBenchRow {
            density: DensityMetrics {
                triangles_per_pixel,
                instances_per_megapixel: triangles_per_pixel * 100.0,
                clusters_per_tile: 0.0,
            },
            classic_ms,
            path_ms,
        }
    }

    #[test]
    fn the_crossover_is_the_sparsest_row_of_the_winning_run() {
        // Unsorted on purpose: the order of the bench's scene list must not matter.
        let rows = [
            row(4.0, 12.0, 9.0),
            row(0.25, 2.0, 3.0),
            row(1.0, 6.0, 5.0),
            row(0.5, 4.0, 4.1),
            row(16.0, 22.0, 11.0),
        ];
        let c = DenseCrossover::of_rows(&rows, DENSE_CROSSOVER_MARGIN).expect("the densest row wins");
        assert_eq!(c.triangles_per_pixel, 1.0);
        assert_eq!(c.instances_per_megapixel, 100.0);
        assert_eq!(c.rows_won, 3);
        assert!(c.bracketed, "0.5 lost below it");
    }

    #[test]
    fn a_lone_win_among_losses_sets_nothing_and_a_lost_top_row_none() {
        // The sparse row wins, the denser ones lose: no run ends at the densest row.
        let rows = [row(0.5, 4.0, 3.0), row(1.0, 6.0, 7.0), row(2.0, 8.0, 9.0)];
        assert_eq!(DenseCrossover::of_rows(&rows, DENSE_CROSSOVER_MARGIN), None);
        // A loss in the middle cuts the run there.
        let rows = [row(0.5, 4.0, 3.0), row(1.0, 6.0, 7.0), row(2.0, 8.0, 5.0)];
        let c = DenseCrossover::of_rows(&rows, DENSE_CROSSOVER_MARGIN).expect("top row wins");
        assert_eq!((c.triangles_per_pixel, c.rows_won), (2.0, 1));
        assert_eq!(DenseCrossover::of_rows(&[], DENSE_CROSSOVER_MARGIN), None);
    }

    #[test]
    fn a_win_inside_the_margin_is_not_a_win() {
        assert!(!row(1.0, 10.0, 9.8).path_wins(DENSE_CROSSOVER_MARGIN));
        assert!(row(1.0, 10.0, 9.7).path_wins(DENSE_CROSSOVER_MARGIN));
        assert!(!row(1.0, f64::NAN, 1.0).path_wins(0.0));
        assert!(!row(1.0, 10.0, 0.0).path_wins(0.0), "a row with no GPU stamp");
    }

    #[test]
    fn every_row_won_is_not_bracketed() {
        let rows = [row(1.0, 6.0, 5.0), row(2.0, 9.0, 6.0)];
        let c = DenseCrossover::of_rows(&rows, DENSE_CROSSOVER_MARGIN).expect("wins");
        assert!(!c.bracketed);
        assert_eq!(c.rows_won, 2);
    }

    #[test]
    fn the_stricter_of_two_cards_takes_the_denser_columns() {
        let a = DenseCrossover {
            triangles_per_pixel: 1.0,
            instances_per_megapixel: 900.0,
            rows_won: 3,
            bracketed: true,
        };
        let b = DenseCrossover {
            triangles_per_pixel: 0.5,
            instances_per_megapixel: 1200.0,
            rows_won: 4,
            bracketed: false,
        };
        let s = a.stricter(b);
        assert_eq!((s.triangles_per_pixel, s.instances_per_megapixel), (1.0, 1200.0));
        assert_eq!(s.rows_won, 3);
        assert!(!s.bracketed);
        let select = s.select_at(1920, 1080);
        assert_eq!(select.enter_triangles, 2_073_600);
        assert_eq!(select.enter_instances, 2_489);
    }

    /// The recorded 5090 rows (`docs/perf-log.md`, 2560x1440) through the crossover: the
    /// per-material resolve wins every row, so the crossover is the sparsest one and not bracketed;
    /// the single pass loses the 4096x8192 row, so its run is the densest row alone. The densities
    /// are the bench's scene sizes (sphere triangle counts from `uv_sphere`).
    #[test]
    fn the_recorded_5090_rows_favor_the_per_material_resolve() {
        let pixels = 2560.0 * 1440.0;
        let tris = |segments: u32, instances: u32| {
            let rings = (segments / 2).max(3);
            let (_, indices) = crate::uv_sphere(0.45, segments, rings);
            (indices.len() / 3) as f64 * f64::from(instances) / pixels
        };
        let seg = |triangles: u32| ((f64::from(triangles)).sqrt() * 1.4).max(4.0) as u32;
        let density = |instances: u32, triangles: u32| DensityMetrics {
            triangles_per_pixel: tris(seg(triangles), instances),
            instances_per_megapixel: f64::from(instances) * 1.0e6 / pixels,
            clusters_per_tile: 0.0,
        };
        // (instances, triangles, classic, per material, single pass)
        let recorded = [
            (1024, 2048, 7.19, 6.84, 8.12),
            (4096, 2048, 12.64, 9.40, 10.06),
            (4096, 8192, 8.48, 6.50, 10.88),
            (16384, 2048, 22.77, 11.53, 11.59),
        ];
        let rows = |pick: fn(&(u32, u32, f64, f64, f64)) -> f64| {
            recorded
                .iter()
                .map(|r| DenseBenchRow {
                    density: density(r.0, r.1),
                    classic_ms: r.2,
                    path_ms: pick(r),
                })
                .collect::<Vec<_>>()
        };
        let per_material = DenseCrossover::of_rows(&rows(|r| r.3), DENSE_CROSSOVER_MARGIN).expect("wins");
        assert_eq!(per_material.rows_won, 4);
        assert!(!per_material.bracketed);
        // The sparsest recorded row is 1024 spheres of about 3,800 triangles at 1440p.
        assert!(
            (0.5..2.0).contains(&per_material.triangles_per_pixel),
            "{per_material:?}"
        );
        // The default's thresholds are not sparser than that row at 1440p: the default never
        // routes a 1440p scene the bench did not show winning.
        let at_1440 = per_material.select_at(2560, 1440);
        assert_eq!(at_1440.enter_triangles, 3_999_744, "{at_1440:?}");
        assert!(DenseSceneSelect::MEASURED.enter_triangles >= at_1440.enter_triangles);
        const { assert!(DenseSceneSelect::MEASURED.enter_instances >= 4096) };
        let single = DenseCrossover::of_rows(&rows(|r| r.4), DENSE_CROSSOVER_MARGIN);
        assert!(
            single.is_none_or(|c| c.rows_won < 4),
            "the single pass lost a dense row: {single:?}"
        );
    }

    #[test]
    fn default_switch_names_round_trip() {
        for d in [VisibilityDefault::Preset, VisibilityDefault::Dense] {
            assert_eq!(VisibilityDefault::from_name(d.as_str()), Some(d));
        }
        assert_eq!(VisibilityDefault::from_name("always"), None);
        assert_eq!(VisibilityDefault::default(), VisibilityDefault::Preset);
        for code in [
            VisibilityDefaultState::Applied,
            VisibilityDefaultState::SwitchOff,
            VisibilityDefaultState::PresetUnmeasured,
            VisibilityDefaultState::ProfileChoice,
        ]
        .map(VisibilityDefaultState::as_str)
        {
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
        const { assert!(!VisibilityPath::DENSE_DEFAULT.single_pass_resolve) };
        assert_eq!(
            VisibilityPath::DENSE_DEFAULT.select,
            VisibilitySelect::Dense(DenseSceneSelect::MEASURED)
        );
    }
}
