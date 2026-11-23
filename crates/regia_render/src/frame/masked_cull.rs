// SPDX-License-Identifier: MPL-2.0
//! CPU masked occlusion culling of the forward draw list (ADR-10072): the frame-side owner of a
//! [`crate::masked_occlusion::MaskedOcclusionCuller`] and its per-view state.
//!
//! After `build_draw_list` has frustum-culled the frame's draws, and before it splits them into
//! the opaque, blended and shadow lists, [`FrameRenderer::masked_cull_draws`] (one call):
//!
//! 1. picks occluders among this frame's visible draws: rigid, opaque, single-material meshes
//!    with CPU vertices whose projected bounding sphere is large on screen (largest first,
//!    under a triangle budget), and rasterizes them into the culler's small software depth
//!    buffer with the unjittered view-projection;
//! 2. tests every visible opaque rigid draw's world box against it and marks the hidden ones not
//!    visible. They leave the colour, depth and velocity passes; the shadow passes keep them
//!    (an occluded mesh can still cast a visible shadow), as they keep frustum-culled ones.
//!
//! The culler is conservative (its module docs): a box is reported hidden only when every pixel
//! it could touch is covered by occluder surface nearer than the box's nearest point, so a
//! culled draw would have failed the depth test everywhere. The frame is the same with the cull
//! on or off. It runs on the thread that builds the draw list (the render thread), on the main
//! view only: a render-to-texture view and a stereo eye skip it.

use super::*;
use crate::masked_occlusion::{Backface, DepthMode, MaskedOcclusionCuller, Visibility};
use regia_core::glam::{Mat4, Vec3};

/// Width of the frame's masked depth buffer in pixels.
pub const MASKED_CULL_WIDTH: u32 = 256;
/// Height of the frame's masked depth buffer in pixels.
pub const MASKED_CULL_HEIGHT: u32 = 128;
/// Most occluder triangles rasterized per frame. Occluders are taken largest first until the
/// next one would pass this; the rest of the frame is tested against what was drawn.
pub const MASKED_OCCLUDER_TRIANGLE_BUDGET: u32 = 8192;
/// Most occluder meshes rasterized per frame.
pub const MASKED_MAX_OCCLUDERS: usize = 32;
/// Smallest occluder: its bounding sphere's projected radius as a share of the view's
/// half-height. Smaller meshes cover few whole buffer pixels and are left out.
pub const MASKED_OCCLUDER_MIN_SCREEN: f32 = 0.08;
/// Fewest draws the buffer could hide for a frame to build it: below this the occluder raster
/// costs more than the draws it could save (a tube view tests a handful).
pub const MASKED_MIN_CULLABLE: usize = 32;
/// Consecutive culling frames that hide nothing before the cull backs off.
pub const MASKED_IDLE_FRAMES: u32 = 8;
/// While backed off, one frame in this many builds the buffer again (a probe); the first frame
/// that hides something ends the back-off.
pub const MASKED_PROBE_INTERVAL: u32 = 16;

/// Why the last main-view frame did not cull with the masked depth buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskedCullSkip {
    /// The switch is off ([`FrameRenderer::set_masked_occlusion`]).
    Off,
    /// The frame drew a stereo eye.
    Stereo,
    /// No visible draw qualified as an occluder.
    NoOccluders,
    /// The view-projection matrix was not finite.
    BadMatrix,
    /// Fewer than [`MASKED_MIN_CULLABLE`] draws could be hidden.
    FewDraws,
    /// Backed off: the last [`MASKED_IDLE_FRAMES`] culling frames hid nothing; a probe frame
    /// runs every [`MASKED_PROBE_INTERVAL`] frames.
    Idle,
}

impl MaskedCullSkip {
    /// The stable lowercase name the agent protocol reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Stereo => "stereo",
            Self::NoOccluders => "no_occluders",
            Self::BadMatrix => "bad_matrix",
            Self::FewDraws => "few_draws",
            Self::Idle => "idle",
        }
    }
}

/// What the masked occlusion cull did on the last main-view frame
/// ([`FrameRenderer::masked_occlusion_report`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaskedCullReport {
    /// The renderer's switch ([`FrameRenderer::set_masked_occlusion`]).
    pub enabled: bool,
    /// The last main-view frame rasterized occluders and tested its draws.
    pub active: bool,
    /// Why it did not (`None` when it ran).
    pub skipped: Option<MaskedCullSkip>,
    /// Occluder meshes rasterized.
    pub occluders: u32,
    /// Occluder triangles submitted to the rasterizer.
    pub occluder_triangles: u32,
    /// Visible draws tested against the buffer.
    pub tested: u32,
    /// Of [`Self::tested`], the draws found hidden and dropped from the colour and depth passes.
    pub culled: u32,
    /// Wall time of the occluder rasterization, in microseconds (render thread).
    pub raster_micros: u32,
    /// Wall time of the box tests, in microseconds (render thread).
    pub test_micros: u32,
    /// Main-view frames that culled since the renderer was made.
    pub frames: u64,
}

/// The renderer's masked occlusion state: the switch, the culler (made on first use) and the
/// per-frame scratch lists.
#[derive(Default)]
pub(super) struct MaskedCull {
    enabled: bool,
    culler: Option<MaskedOcclusionCuller>,
    /// Object-space occluder positions of the mesh being rasterized.
    positions: Vec<Vec3>,
    /// `(projected size, draw index)` occluder candidates.
    candidates: Vec<(f32, usize)>,
    report: MaskedCullReport,
    pacer: Pacer,
}

/// When a frame builds the buffer: enough cullable draws, and not while backed off after frames
/// that hid nothing. Skipping a frame is always safe (it draws everything), so the pacing only
/// moves cost, never the image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Pacer {
    /// Consecutive culling frames that hid nothing.
    idle_streak: u32,
    /// Frames since the last probe while backed off.
    since_probe: u32,
}

impl Pacer {
    /// `None` when this frame builds the buffer, else why not.
    fn begin(&mut self, cullable: usize) -> Option<MaskedCullSkip> {
        if cullable < MASKED_MIN_CULLABLE {
            return Some(MaskedCullSkip::FewDraws);
        }
        if self.idle_streak >= MASKED_IDLE_FRAMES {
            self.since_probe += 1;
            if self.since_probe < MASKED_PROBE_INTERVAL {
                return Some(MaskedCullSkip::Idle);
            }
            self.since_probe = 0;
        }
        None
    }

    /// Records what a frame that built the buffer hid.
    fn end(&mut self, culled: usize) {
        if culled == 0 {
            self.idle_streak = self.idle_streak.saturating_add(1);
        } else {
            *self = Self::default();
        }
    }
}

/// The world-space bounding sphere of a rigid draw with a rest bound: the same sphere the
/// frustum test used (`build_draw_list`).
fn world_sphere(model: &[[f32; 4]; 4], bounds: MeshBounds, inflate: f32) -> (Vec3, f32) {
    let model = Mat4::from_cols_array_2d(model);
    let center = model.transform_point3(Vec3::from_array(bounds.center));
    let scale = (model
        .x_axis
        .length_squared()
        .max(model.y_axis.length_squared())
        .max(model.z_axis.length_squared()))
    .sqrt();
    (center, bounds.radius * scale + inflate)
}

/// A draw the masked buffer may hide: drawn in the opaque passes of this view, rigid, with a
/// rest bound the cull can trust, and nothing that shows through other geometry.
fn cullable(draw: &Draw) -> bool {
    draw.visible
        && draw.rigid
        && !draw.morphed
        && !draw.vis
        && draw.alpha_mode < ALPHA_BLEND
        && !draw.transmissive
        && draw.over == [0, 0]
        && !draw.hair
        && draw.glass.is_none()
        // A vertex-stage graph without a declared largest offset has no safe bound.
        && (!draw.graph_vertex || draw.vertex_offset_bounded)
}

/// A draw that may occlude: it writes solid depth over its whole surface in this view.
fn occluder(draw: &Draw, mesh: &FrameMesh<'_>) -> bool {
    draw.visible
        && draw.rigid
        && !draw.morphed
        && !draw.vis
        && draw.alpha_mode == crate::ALPHA_OPAQUE
        && !draw.transmissive
        && draw.over == [0, 0]
        && !draw.hair
        && draw.glass.is_none()
        && draw.graph.is_none()
        && !draw.graph_vertex
        && !draw.mask
        && draw.lod_dither == LOD_DITHER_NONE
        && !mesh.vertices.is_empty()
        && mesh.indices.len() >= 3
        && mesh.indices.len().is_multiple_of(3)
}

impl MaskedCull {
    /// The switch.
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }
}

impl FrameRenderer {
    /// Turns the CPU masked occlusion cull of the main view's draw list on or off
    /// (ADR-10072). The runtime sets it from [`crate::RenderProfile::masked_occlusion`] every
    /// frame. Off (the default for a bare renderer) the frame builds no buffer and tests
    /// nothing. On, the frame is the same: only draws the depth test would reject are skipped.
    pub fn set_masked_occlusion(&mut self, enabled: bool) {
        if enabled != self.masked_cull.enabled {
            // A fresh start: no back-off carried over from an earlier run.
            self.masked_cull.pacer = Pacer::default();
        }
        self.masked_cull.enabled = enabled;
        self.masked_cull.report.enabled = enabled;
    }

    /// Whether the main view culls its draw list with the masked depth buffer
    /// ([`Self::set_masked_occlusion`]).
    #[must_use]
    pub fn masked_occlusion(&self) -> bool {
        self.masked_cull.enabled
    }

    /// What the masked occlusion cull did on the last main-view frame.
    #[must_use]
    pub fn masked_occlusion_report(&self) -> MaskedCullReport {
        self.masked_cull.report
    }

    /// Culls the frame's visible opaque rigid draws hidden behind its largest opaque meshes
    /// (module docs). Returns how many draws it marked not visible. A texture view returns 0
    /// and leaves the report alone.
    pub(super) fn masked_cull_draws(
        &mut self,
        scene: &FrameScene<'_>,
        draws: &mut [Draw],
        view_pass: bool,
        unjittered_vp: Mat4,
        fov_y: f32,
    ) -> usize {
        if view_pass {
            return 0;
        }
        let state = &mut self.masked_cull;
        let frames = state.report.frames;
        state.report = MaskedCullReport {
            enabled: state.enabled,
            frames,
            ..MaskedCullReport::default()
        };
        if !state.enabled {
            state.report.skipped = Some(MaskedCullSkip::Off);
            return 0;
        }
        if self.stereo.camera.is_some() {
            state.report.skipped = Some(MaskedCullSkip::Stereo);
            return 0;
        }
        if !unjittered_vp.is_finite() {
            state.report.skipped = Some(MaskedCullSkip::BadMatrix);
            return 0;
        }
        // Cheap first: enough draws to hide, and not backed off (`Pacer`).
        let cullable_draws = draws.iter().filter(|draw| cullable(draw)).count();
        if let Some(skip) = state.pacer.begin(cullable_draws) {
            state.report.skipped = Some(skip);
            return 0;
        }
        let _span = tracing::info_span!("masked_occlusion_cull").entered();
        let raster_started = std::time::Instant::now();
        // ---- Occluder candidates: projected size, largest first ----
        let focal = 1.0 / (fov_y * 0.5).tan().max(1e-6);
        let mut candidates = std::mem::take(&mut state.candidates);
        candidates.clear();
        for (index, draw) in draws.iter().enumerate() {
            let Some(mesh) = scene.meshes.get(draw.src) else {
                continue;
            };
            if !occluder(draw, mesh) {
                continue;
            }
            let Some(bounds) = self.meshes.get(&draw.mesh).and_then(|entry| entry.bounds) else {
                continue;
            };
            let (center, radius) = world_sphere(&draw.model, bounds, 0.0);
            let w = (unjittered_vp * center.extend(1.0)).w;
            // The eye inside (or at) the sphere: as large as an occluder gets.
            let size = if w <= radius {
                f32::INFINITY
            } else {
                radius * focal / w
            };
            if size >= MASKED_OCCLUDER_MIN_SCREEN {
                candidates.push((size, index));
            }
        }
        // Largest first; ties by draw order, so the pick is deterministic.
        candidates.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        if candidates.is_empty() {
            state.candidates = candidates;
            state.report.skipped = Some(MaskedCullSkip::NoOccluders);
            return 0;
        }
        if state.culler.is_none() {
            // The frame's projection is standard depth (0 near, 1 far; `LessEqual`). The size is
            // a constant inside the culler's limits, so this cannot fail.
            state.culler = MaskedOcclusionCuller::new_with_depth_mode(
                MASKED_CULL_WIDTH,
                MASKED_CULL_HEIGHT,
                DepthMode::Standard,
            )
            .ok();
        }
        let Some(culler) = state.culler.as_mut() else {
            state.candidates = candidates;
            state.report.skipped = Some(MaskedCullSkip::BadMatrix);
            return 0;
        };
        culler.clear();
        let mut occluders = 0u32;
        let mut triangles = 0u32;
        for &(_, index) in &candidates {
            if occluders as usize >= MASKED_MAX_OCCLUDERS {
                break;
            }
            let draw = &draws[index];
            let Some(mesh) = scene.meshes.get(draw.src) else {
                continue;
            };
            let count = u32::try_from(mesh.indices.len() / 3).unwrap_or(u32::MAX);
            if triangles.saturating_add(count) > MASKED_OCCLUDER_TRIANGLE_BUDGET {
                continue;
            }
            state.positions.clear();
            state.positions.extend(
                mesh.vertices
                    .iter()
                    .map(|vertex| Vec3::from_array(vertex.position)),
            );
            let mvp = unjittered_vp * Mat4::from_cols_array_2d(&draw.model);
            // The GPU culls back faces of a single-sided material (counter-clockwise in NDC is
            // front, as the culler's `Backface::Cull` assumes); a double-sided one draws both.
            let backface = if draw.cull_off || scene.double_sided {
                Backface::None
            } else {
                Backface::Cull
            };
            if culler
                .draw_occluder(&state.positions, mesh.indices, mvp, backface)
                .is_ok()
            {
                occluders += 1;
                triangles = triangles.saturating_add(count);
            }
        }
        state.candidates = candidates;
        let raster_micros = u32::try_from(raster_started.elapsed().as_micros()).unwrap_or(u32::MAX);
        // ---- Box tests ----
        let test_started = std::time::Instant::now();
        let mut tested = 0u32;
        let mut culled = 0usize;
        if occluders > 0 {
            for draw in draws.iter_mut() {
                if !cullable(draw) {
                    continue;
                }
                let Some(bounds) = self.meshes.get(&draw.mesh).and_then(|entry| entry.bounds) else {
                    continue;
                };
                let (center, radius) = world_sphere(&draw.model, bounds, draw.inflate);
                let half = Vec3::splat(radius.max(0.0));
                let aabb = regia_core::math::Aabb::new(center - half, center + half);
                tested += 1;
                if culler.test_aabb(aabb, unjittered_vp) == Ok(Visibility::Occluded) {
                    draw.visible = false;
                    culled += 1;
                }
            }
        }
        let test_micros = u32::try_from(test_started.elapsed().as_micros()).unwrap_or(u32::MAX);
        state.pacer.end(culled);
        state.report = MaskedCullReport {
            enabled: true,
            active: occluders > 0,
            skipped: (occluders == 0).then_some(MaskedCullSkip::NoOccluders),
            occluders,
            occluder_triangles: triangles,
            tested,
            culled: u32::try_from(culled).unwrap_or(u32::MAX),
            raster_micros,
            test_micros,
            frames: frames + u64::from(occluders > 0),
        };
        culled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shader edit rebuilds the renderer (`sync_shader_epoch`): the masked switch survives it.
    #[test]
    fn a_shader_hot_swap_keeps_the_masked_switch() {
        let mut renderer = FrameRenderer::default();
        renderer.set_masked_occlusion(true);
        renderer.shader_epoch = crate::shader_reload::shader_epoch().wrapping_add(1);
        assert!(renderer.sync_shader_epoch(), "the epoch moved");
        assert!(renderer.masked_occlusion(), "the masked cull turned off");
        assert!(renderer.masked_occlusion_report().enabled);
    }

    #[test]
    fn the_pacer_skips_few_draws_and_backs_off_after_idle_frames() {
        let mut pacer = Pacer::default();
        assert_eq!(
            pacer.begin(MASKED_MIN_CULLABLE - 1),
            Some(MaskedCullSkip::FewDraws)
        );
        // Idle frames run until the streak reaches MASKED_IDLE_FRAMES.
        for _ in 0..MASKED_IDLE_FRAMES {
            assert_eq!(pacer.begin(100), None);
            pacer.end(0);
        }
        // Backed off: MASKED_PROBE_INTERVAL - 1 skipped frames, then one probe.
        for _ in 1..MASKED_PROBE_INTERVAL {
            assert_eq!(pacer.begin(100), Some(MaskedCullSkip::Idle));
        }
        assert_eq!(pacer.begin(100), None, "the probe frame runs");
        pacer.end(0);
        assert_eq!(pacer.begin(100), Some(MaskedCullSkip::Idle), "still backed off");
        // A probe that hides something ends the back-off.
        for _ in 2..MASKED_PROBE_INTERVAL {
            assert_eq!(pacer.begin(100), Some(MaskedCullSkip::Idle));
        }
        assert_eq!(pacer.begin(100), None);
        pacer.end(3);
        assert_eq!(pacer, Pacer::default());
        assert_eq!(pacer.begin(100), None);
    }

    #[test]
    fn switching_on_again_drops_the_back_off() {
        let mut renderer = FrameRenderer::default();
        renderer.set_masked_occlusion(true);
        renderer.masked_cull.pacer.idle_streak = MASKED_IDLE_FRAMES;
        renderer.set_masked_occlusion(true);
        assert_eq!(
            renderer.masked_cull.pacer.idle_streak, MASKED_IDLE_FRAMES,
            "a repeat set keeps it"
        );
        renderer.set_masked_occlusion(false);
        renderer.set_masked_occlusion(true);
        assert_eq!(renderer.masked_cull.pacer, Pacer::default());
    }

    #[test]
    fn skip_names_are_lowercase_protocol_codes() {
        for skip in [
            MaskedCullSkip::Off,
            MaskedCullSkip::Stereo,
            MaskedCullSkip::NoOccluders,
            MaskedCullSkip::BadMatrix,
            MaskedCullSkip::FewDraws,
            MaskedCullSkip::Idle,
        ] {
            let name = skip.as_str();
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{name}"
            );
        }
    }
}
