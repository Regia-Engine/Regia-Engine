// SPDX-License-Identifier: MPL-2.0
//! More than one directional light, ambient volumes and the shader debug view modes of a frame
//! (`specs/13` § 6, § 10, § 11; ADR-7211).
//!
//! - **Directional lights.** A frame draws up to [`MAX_DIRECTIONAL_LIGHTS`]: the sun (the light with
//!   the cascaded shadow map, [`crate::ExtractedFrame::directional`]) and up to
//!   [`MAX_EXTRA_DIRECTIONAL`] more ([`LightsViews::extra_directional`]). The extra lights take the
//!   sun's BRDF in the forward and visibility-buffer shading and the Mobile forward, unshadowed. A
//!   further light is refused: it is counted in [`LightsViews::refused_directional`] and logged once
//!   per process.
//! - **Ambient volumes.** [`AmbientVolume`] is an artist-placed fill: an oriented box (the entity's
//!   rotation turns it, post05w4) that adds a uniform irradiance (color × intensity, lux) to the
//!   ambient term of every surface inside it, fading out over `blend_distance` past its faces. A frame takes up to [`MAX_AMBIENT_VOLUMES`];
//!   more are refused like extra lights.
//! - **Debug view modes** drawn by the forward shader ([`shading_view_code`]); the overdraw and LOD
//!   modes swap materials instead ([`crate::debug_shading`]).
//!
//! The renderer receives one [`LightsViews`] per frame ([`crate::FrameRenderer::set_lights_views`]),
//! packed into the tail of the frame uniform ([`LightsViewsUniform`]).

use std::sync::atomic::{AtomicBool, Ordering};

use bytemuck::Zeroable as _;
use regia_app::prelude::*;

use crate::DirectionalLightUniform;
use crate::debug::DebugViewMode;

/// Directional lights one frame draws (`specs/13` § 6): the sun with its cascaded shadow map and
/// [`MAX_EXTRA_DIRECTIONAL`] more.
pub const MAX_DIRECTIONAL_LIGHTS: usize = 4;

/// Directional lights a frame draws besides the sun.
pub const MAX_EXTRA_DIRECTIONAL: usize = MAX_DIRECTIONAL_LIGHTS - 1;

/// Ambient volumes one frame draws.
pub const MAX_AMBIENT_VOLUMES: usize = 4;

/// The paths that shade a frame's extra directional lights (ADR-7211, ADR-7254), as `render.lights`
/// lists them: the forward and visibility-buffer resolve, the Mobile forward, the terrain's shared
/// draw and its grass, the probe GI (DDGI) update, and in `regia_pt` the path tracer and the GPU
/// probe bake (one tracer), the CPU probe bake and both lightmap bakes, when their scene comes from
/// an extracted frame or a frame export.
pub const EXTRA_DIRECTIONAL_CONSUMERS: &[&str] = &[
    "forward",
    "visibility_buffer",
    "mobile_forward",
    "terrain",
    "grass",
    "ddgi_probes",
    "path_tracer",
    "probe_bake_gpu",
    "probe_bake_cpu",
    "lightmap_bake_cpu",
    "lightmap_bake_gpu",
    "bake_description",
];

/// The paths that still see only the sun (ADR-7254, ADR-7818 known issues): the terrain's scatter
/// impostors and water, and the two writers of bake descriptions that pick one sun from a level:
/// the level export (`level.export_bake`, `regia bake-gi --from-scene`) and the editor's bake
/// panel. A `.bake.ron` itself lists up to four suns since format v3 (`bake_description` above,
/// ADR-7818).
pub const SUN_ONLY_PATHS: &[&str] = &[
    "scatter_impostors",
    "water",
    "level_bake_export",
    "editor_bake_panel",
];

/// An artist-placed ambient fill (`specs/13` § 10): every surface inside the box takes `color ×
/// intensity` lux of extra uniform irradiance on its ambient term (diffuse, scaled by its ambient
/// occlusion). The box is centred on the entity's `GlobalTransform` translation and turned by its
/// rotation (the box's own axes, not the world's, bound it); the transform's scale multiplies
/// `half_extents` along those axes. It requires `InheritedVisibility`, so a volume under a hidden
/// group lights nothing (scene propagation resolves the flag, which is never saved).
///
/// ```
/// use regia_render::AmbientVolume;
/// let fill = AmbientVolume { intensity: 2_000.0, ..AmbientVolume::default() };
/// assert_eq!(fill.irradiance(), [2_000.0; 3]);
/// ```
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default, Clone)]
#[require(regia_scene::components::InheritedVisibility)]
pub struct AmbientVolume {
    /// Linear RGB tint of the fill.
    pub color: [f32; 3],
    /// Irradiance the fill adds, lux.
    pub intensity: f32,
    /// Half extents of the box, meters (before the transform's scale).
    pub half_extents: [f32; 3],
    /// Distance past the faces over which the fill fades to nothing, meters (0 = a hard edge).
    pub blend_distance: f32,
}

impl Default for AmbientVolume {
    fn default() -> Self {
        Self {
            color: [1.0; 3],
            intensity: 1_000.0,
            half_extents: [5.0; 3],
            blend_distance: 1.0,
        }
    }
}

impl AmbientVolume {
    /// `color × intensity`, lux per channel (non-finite or negative values are 0).
    #[must_use]
    pub fn irradiance(&self) -> [f32; 3] {
        let k = finite_non_negative(self.intensity);
        self.color.map(|c| finite_non_negative(c) * k)
    }

    /// The volume as a frame draws it, at `center` with the transform's `scale`, unturned.
    #[must_use]
    pub fn frame_volume(&self, center: [f32; 3], scale: [f32; 3]) -> FrameAmbientVolume {
        self.frame_volume_turned(center, IDENTITY_ROTATION, scale)
    }

    /// The volume as a frame draws it, at `center`, turned by `rotation` (a unit quaternion
    /// `[x, y, z, w]`, box to world) with the transform's `scale` along the box's own axes.
    #[must_use]
    pub fn frame_volume_turned(
        &self,
        center: [f32; 3],
        rotation: [f32; 4],
        scale: [f32; 3],
    ) -> FrameAmbientVolume {
        FrameAmbientVolume {
            center,
            half_extents: [0, 1, 2].map(|i| finite_non_negative(self.half_extents[i] * scale[i].abs())),
            irradiance: self.irradiance(),
            blend_distance: finite_non_negative(self.blend_distance),
            rotation: unit_rotation(rotation),
        }
    }
}

/// The identity rotation `[x, y, z, w]` of [`FrameAmbientVolume::rotation`].
pub const IDENTITY_ROTATION: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// `q` normalized; the identity when it is not finite or has (near) zero length.
fn unit_rotation(q: [f32; 4]) -> [f32; 4] {
    let len2 = q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3];
    if !len2.is_finite() || len2 < 1e-12 {
        return IDENTITY_ROTATION;
    }
    let inv = len2.sqrt().recip();
    q.map(|c| c * inv)
}

/// `v` turned by the inverse of the unit quaternion `q` (world to box). The shaders'
/// `ambient_volume_local` computes the same: `v + w t + u × t` with `u = -q.xyz`, `t = 2 u × v`.
fn rotate_inverse(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let u = [-q[0], -q[1], -q[2]];
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let t = cross(u, v).map(|c| 2.0 * c);
    let ut = cross(u, t);
    [0, 1, 2].map(|i| v[i] + q[3] * t[i] + ut[i])
}

/// One ambient volume in world space, as the frame uniform carries it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameAmbientVolume {
    /// Box centre, world space.
    pub center: [f32; 3],
    /// Box half extents, meters.
    pub half_extents: [f32; 3],
    /// Irradiance added inside, lux per channel.
    pub irradiance: [f32; 3],
    /// Fade distance past the faces, meters.
    pub blend_distance: f32,
    /// The box's rotation, a unit quaternion `[x, y, z, w]` from the box's axes to the world's
    /// ([`IDENTITY_ROTATION`]: world aligned). `half_extents` run along the box's own axes.
    pub rotation: [f32; 4],
}

impl Default for FrameAmbientVolume {
    fn default() -> Self {
        Self {
            center: [0.0; 3],
            half_extents: [0.0; 3],
            irradiance: [0.0; 3],
            blend_distance: 0.0,
            rotation: IDENTITY_ROTATION,
        }
    }
}

impl FrameAmbientVolume {
    /// The share of the fill a point receives: 1 inside the box, falling linearly to 0 at
    /// `blend_distance` past the nearest face (0 outside a hard-edged box). The point is taken
    /// into the box's own frame first, so a turned box bounds along its own axes. The shaders'
    /// `ambient_volume_irradiance` computes the same.
    #[must_use]
    pub fn weight_at(&self, p: [f32; 3]) -> f32 {
        let rel = [0, 1, 2].map(|i| p[i] - self.center[i]);
        let local = rotate_inverse(unit_rotation(self.rotation), rel);
        let outside = [0, 1, 2].map(|i| (local[i].abs() - self.half_extents[i]).max(0.0));
        let d = (outside[0] * outside[0] + outside[1] * outside[1] + outside[2] * outside[2]).sqrt();
        if d <= 0.0 {
            1.0
        } else if self.blend_distance > 0.0 {
            (1.0 - d / self.blend_distance).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// What a frame draws besides its sun and local lights: the extra directional lights, the ambient
/// volumes and the forward shader's debug view.
#[derive(Debug, Clone, Default)]
pub struct LightsViews {
    /// Directional lights besides the sun, in entity order. The frame draws the first
    /// [`MAX_EXTRA_DIRECTIONAL`]; extraction never passes more.
    pub extra_directional: Vec<DirectionalLightUniform>,
    /// Directional lights extraction refused this frame (past [`MAX_DIRECTIONAL_LIGHTS`]).
    pub refused_directional: usize,
    /// Ambient volumes, in entity order. The frame draws the first [`MAX_AMBIENT_VOLUMES`].
    pub ambient_volumes: Vec<FrameAmbientVolume>,
    /// Ambient volumes extraction refused this frame (past [`MAX_AMBIENT_VOLUMES`]).
    pub refused_ambient_volumes: usize,
    /// The debug view mode the forward shader draws ([`shading_view_code`]); any other mode,
    /// including [`DebugViewMode::Lit`], shades normally.
    pub shading_view: DebugViewMode,
}

/// Rows of the frame uniform's lights-and-views block (`LabFrame::view_mode`, `extra_dir`,
/// `ambient_box` in `lab_forward.wgsl` and `forward_mobile.wgsl`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LightsViewsUniform {
    /// `x` = shader debug view ([`shading_view_code`], 0 lit), `y` = extra directional lights,
    /// `z` = ambient volumes, `w` = 0.
    pub view_mode: [f32; 4],
    /// Per extra light: travel direction (`w` = 1), then color × lux (`w` = 0).
    pub extra_dir: [[f32; 4]; 2 * MAX_EXTRA_DIRECTIONAL],
    /// Per ambient volume: centre (`w` = blend distance), half extents (`w` = 0), irradiance
    /// (`w` = 0).
    pub ambient_box: [[f32; 4]; 3 * MAX_AMBIENT_VOLUMES],
    /// Per ambient volume: the rotation from the world into the box's frame (the inverse of
    /// [`FrameAmbientVolume::rotation`]), a unit quaternion `[x, y, z, w]`.
    pub ambient_rot: [[f32; 4]; MAX_AMBIENT_VOLUMES],
}

const _: () = assert!(size_of::<LightsViewsUniform>() == 368);

impl LightsViewsUniform {
    /// The extra directional lights these rows carry, in slot order (ADR-7254): each with its
    /// travel direction and `color × illuminance` as packed (the colour's largest channel is 1,
    /// the illuminance that channel's lux). What the path tracer's extraction reads back.
    #[must_use]
    pub fn extra_lights(&self) -> Vec<DirectionalLightUniform> {
        let count = (self.view_mode[1].max(0.0) as usize).min(MAX_EXTRA_DIRECTIONAL);
        (0..count)
            .map(|i| {
                let [x, y, z, _] = self.extra_dir[2 * i];
                let [r, g, b, _] = self.extra_dir[2 * i + 1];
                let lux = r.max(g).max(b).max(0.0);
                let color = if lux > 0.0 {
                    [r / lux, g / lux, b / lux]
                } else {
                    [1.0; 3]
                };
                DirectionalLightUniform::new([x, y, z], color, lux)
            })
            .collect()
    }
}

impl LightsViews {
    /// The uniform rows of this frame. Lights and volumes past the caps are dropped; a volume or
    /// light that adds nothing (zero irradiance) still takes its slot, so the caps do not depend
    /// on the values.
    #[must_use]
    pub fn uniform(&self) -> LightsViewsUniform {
        let mut u = LightsViewsUniform::zeroed();
        let lights = &self.extra_directional[..self.extra_directional.len().min(MAX_EXTRA_DIRECTIONAL)];
        for (i, l) in lights.iter().enumerate() {
            let d = l.direction;
            u.extra_dir[2 * i] = [d[0], d[1], d[2], 1.0];
            let k = l.illuminance;
            u.extra_dir[2 * i + 1] = [l.color[0] * k, l.color[1] * k, l.color[2] * k, 0.0];
        }
        let volumes = &self.ambient_volumes[..self.ambient_volumes.len().min(MAX_AMBIENT_VOLUMES)];
        for (i, v) in volumes.iter().enumerate() {
            let c = v.center;
            let h = v.half_extents;
            let e = v.irradiance;
            u.ambient_box[3 * i] = [c[0], c[1], c[2], v.blend_distance];
            u.ambient_box[3 * i + 1] = [h[0], h[1], h[2], 0.0];
            u.ambient_box[3 * i + 2] = [e[0], e[1], e[2], 0.0];
            let q = unit_rotation(v.rotation);
            u.ambient_rot[i] = [-q[0], -q[1], -q[2], q[3]];
        }
        u.view_mode = [
            shading_view_code(self.shading_view) as f32,
            lights.len() as f32,
            volumes.len() as f32,
            0.0,
        ];
        u
    }
}

/// The forward shader's code for a debug view mode it draws (`view_mode.x` in `lab_forward.wgsl`),
/// 0 for every other mode.
#[must_use]
pub fn shading_view_code(mode: DebugViewMode) -> u32 {
    match mode {
        DebugViewMode::Unlit => 1,
        DebugViewMode::Normals => 2,
        DebugViewMode::BaseColor => 3,
        DebugViewMode::Roughness => 4,
        DebugViewMode::Metallic => 5,
        DebugViewMode::AmbientOcclusion => 6,
        DebugViewMode::LightComplexity => 7,
        DebugViewMode::ShadowCascades => 8,
        DebugViewMode::MotionVectors => 9,
        DebugViewMode::MipLevel => 10,
        _ => 0,
    }
}

/// Which of `lights` (entity bits) a frame draws: the sun, the extra lights in entity order, and
/// how many it refuses. The sun is the lowest-entity light, as it always was (it keeps the cascaded
/// shadow map when it casts shadows); the others follow in entity order up to
/// [`MAX_EXTRA_DIRECTIONAL`].
#[must_use]
pub fn pick_directional(lights: &[u64]) -> DirectionalPick {
    let mut order: Vec<usize> = (0..lights.len()).collect();
    order.sort_by_key(|&i| lights[i]);
    let sun = order.first().copied();
    let rest = order.get(1..).unwrap_or_default();
    let keep = rest.len().min(MAX_EXTRA_DIRECTIONAL);
    DirectionalPick {
        sun,
        extra: rest[..keep].to_vec(),
        refused: rest.len() - keep,
    }
}

/// The result of [`pick_directional`]: indices into its input.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DirectionalPick {
    /// The light with the cascaded shadow map.
    pub sun: Option<usize>,
    /// The extra lights, in entity order.
    pub extra: Vec<usize>,
    /// Lights past [`MAX_DIRECTIONAL_LIGHTS`].
    pub refused: usize,
}

static WARNED_DIRECTIONAL: AtomicBool = AtomicBool::new(false);
static WARNED_VOLUMES: AtomicBool = AtomicBool::new(false);

/// Logs, once per process, that a frame refused directional lights. True the first time.
pub(crate) fn warn_refused_directional(refused: usize) -> bool {
    if refused == 0 || WARNED_DIRECTIONAL.swap(true, Ordering::Relaxed) {
        return false;
    }
    tracing::warn!(
        target: "regia_render",
        "{refused} directional light(s) refused: a frame draws at most {MAX_DIRECTIONAL_LIGHTS} \
         (the sun and {MAX_EXTRA_DIRECTIONAL} more, specs/13 § 6); reported once"
    );
    true
}

/// Logs, once per process, that a frame refused ambient volumes. True the first time.
pub(crate) fn warn_refused_volumes(refused: usize) -> bool {
    if refused == 0 || WARNED_VOLUMES.swap(true, Ordering::Relaxed) {
        return false;
    }
    tracing::warn!(
        target: "regia_render",
        "{refused} ambient volume(s) refused: a frame draws at most {MAX_AMBIENT_VOLUMES}; reported once"
    );
    true
}

fn finite_non_negative(v: f32) -> f32 {
    if v.is_finite() { v.max(0.0) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lights::DirectionalLight;
    use crate::runtime::{ExtractedFrame, extract_frame};
    use regia_scene::components::{InheritedVisibility, Visibility};
    use regia_scene::layers::LayerParked;

    fn light(lux: f32, shadows: bool) -> DirectionalLight {
        DirectionalLight {
            direction: [0.0, -1.0, 0.2],
            color: [1.0; 3],
            illuminance: lux,
            cast_shadows: shadows,
            static_shadow: false,
        }
    }

    #[test]
    fn the_sun_is_the_lowest_entity_and_three_more_follow_in_entity_order() {
        let pick = pick_directional(&[9, 3, 5, 1, 7, 2]);
        assert_eq!(pick.sun, Some(3), "entity 1");
        assert_eq!(pick.extra, vec![5, 1, 2], "entities 2, 3, 5");
        assert_eq!(pick.refused, 2, "entities 7 and 9");
        assert_eq!(pick_directional(&[]), DirectionalPick::default());
    }

    #[test]
    fn five_directional_lights_extract_four_and_refuse_the_fifth() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        let lights: Vec<Entity> = [
            (100_000.0, true),
            (1.0, false),
            (2.0, false),
            (3.0, false),
            (4.0, false),
        ]
        .into_iter()
        .map(|(lux, shadows)| app.world_mut().spawn(light(lux, shadows)).id())
        .collect();
        app.update();
        // Entity order is bit order (bevy's bits run against spawn order).
        let mut rows: Vec<(u64, f32)> = lights
            .iter()
            .map(|&e| {
                let lux = app.world().get::<DirectionalLight>(e).expect("light").illuminance;
                (e.to_bits(), lux)
            })
            .collect();
        rows.sort_by_key(|r| r.0);
        let frame = app.world().resource::<ExtractedFrame>();
        let sun = frame.directional.expect("a sun");
        assert_eq!(sun.illuminance, rows[0].1, "the lowest-entity light is the sun");
        assert_eq!(
            frame.directional_shadows,
            rows[0].1 == 100_000.0,
            "its own shadow flag"
        );
        assert_eq!(frame.lights_views.extra_directional.len(), MAX_EXTRA_DIRECTIONAL);
        assert_eq!(
            frame.lights_views.refused_directional, 1,
            "the fifth light is refused"
        );
        // The extra lights are the next three in entity order: the refused one is the highest.
        let expected: Vec<f32> = rows[1..4].iter().map(|r| r.1).collect();
        let got: Vec<f32> = frame
            .lights_views
            .extra_directional
            .iter()
            .map(|l| l.illuminance)
            .collect();
        assert_eq!(got, expected);
        // The uniform carries exactly three extra lights.
        assert_eq!(frame.lights_views.uniform().view_mode[1], 3.0);
    }

    #[test]
    fn ambient_volumes_extract_with_their_transform_and_the_fifth_is_refused() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        let mut spawned: Vec<(u64, usize)> = (0..5)
            .map(|i| {
                let entity = app.world_mut().spawn((
                    AmbientVolume {
                        intensity: 100.0 * (i + 1) as f32,
                        half_extents: [1.0, 2.0, 3.0],
                        ..AmbientVolume::default()
                    },
                    regia_scene::GlobalTransform(
                        regia_core::math::Affine3A::from_scale_rotation_translation(
                            regia_core::math::Vec3::splat(2.0),
                            regia_core::math::Quat::IDENTITY,
                            regia_core::math::Vec3::new(i as f32, 0.0, 0.0),
                        ),
                    ),
                ));
                (entity.id().to_bits(), i)
            })
            .collect();
        app.update();
        let frame = app.world().resource::<ExtractedFrame>();
        assert_eq!(frame.lights_views.ambient_volumes.len(), MAX_AMBIENT_VOLUMES);
        assert_eq!(frame.lights_views.refused_ambient_volumes, 1);
        // Entity order (bits), not spawn order: bevy stores the index xored, so bits run backwards.
        spawned.sort_by_key(|(bits, _)| *bits);
        for (volume, &(_, i)) in frame.lights_views.ambient_volumes.iter().zip(&spawned) {
            assert_eq!(
                volume.half_extents,
                [2.0, 4.0, 6.0],
                "the scale multiplies the extents"
            );
            assert_eq!(volume.irradiance, [100.0 * (i + 1) as f32; 3]);
            assert_eq!(volume.center, [i as f32, 0.0, 0.0]);
        }
    }

    /// post05w4 item 2: a volume turned 45 degrees about Y applies in its own frame, not the
    /// world axes (the parent lane ignored the rotation).
    #[test]
    fn a_45_degree_ambient_volume_applies_in_its_local_frame() {
        use regia_core::math::{Affine3A, Quat, Vec3};
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        let turn = Quat::from_rotation_y(std::f32::consts::FRAC_PI_4);
        let center = Vec3::new(10.0, 0.0, 0.0);
        app.world_mut().spawn((
            AmbientVolume {
                half_extents: [4.0, 1.0, 0.5],
                blend_distance: 0.0,
                ..AmbientVolume::default()
            },
            regia_scene::GlobalTransform(Affine3A::from_scale_rotation_translation(Vec3::ONE, turn, center)),
        ));
        app.update();
        let frame = app.world().resource::<ExtractedFrame>();
        let v = frame.lights_views.ambient_volumes[0];
        // 3.5 m along the box's own long axis: inside the turned box, outside the world-axis one.
        let along = center + turn * Vec3::new(3.5, 0.0, 0.0);
        assert_eq!(
            v.weight_at(along.to_array()),
            1.0,
            "inside the turned box at {along}"
        );
        // 3.5 m along world +X: inside a world-axis box, 2.47 m off the turned box's long axis.
        assert_eq!(v.weight_at([13.5, 0.0, 0.0]), 0.0, "outside the turned box");
        // Across the thin axis in the box's frame: 0.4 m is in, 0.6 m is out.
        let thin_in = center + turn * Vec3::new(0.0, 0.0, 0.4);
        let thin_out = center + turn * Vec3::new(0.0, 0.0, 0.6);
        assert_eq!(v.weight_at(thin_in.to_array()), 1.0);
        assert_eq!(v.weight_at(thin_out.to_array()), 0.0);
    }

    #[test]
    fn the_weight_is_one_inside_and_fades_over_the_blend_distance() {
        let v = FrameAmbientVolume {
            center: [0.0; 3],
            half_extents: [1.0; 3],
            irradiance: [1.0; 3],
            blend_distance: 2.0,
            ..FrameAmbientVolume::default()
        };
        assert_eq!(v.weight_at([0.5, -0.9, 0.0]), 1.0);
        assert!((v.weight_at([2.0, 0.0, 0.0]) - 0.5).abs() < 1e-6);
        assert_eq!(v.weight_at([3.5, 0.0, 0.0]), 0.0);
        let hard = FrameAmbientVolume {
            blend_distance: 0.0,
            ..v
        };
        assert_eq!(hard.weight_at([1.01, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn a_turned_volume_fades_along_its_own_axes_and_the_uniform_carries_the_inverse_turn() {
        let half = std::f32::consts::FRAC_PI_8;
        // 45 degrees about +Z, unnormalized on purpose (twice the unit quaternion).
        let v = FrameAmbientVolume {
            center: [1.0, 2.0, 3.0],
            half_extents: [2.0, 0.5, 1.0],
            irradiance: [1.0; 3],
            blend_distance: 1.0,
            rotation: [0.0, 0.0, 2.0 * half.sin(), 2.0 * half.cos()],
        };
        let s = std::f32::consts::FRAC_1_SQRT_2;
        // Box +Y (the thin axis) points along world (-s, s, 0): 0.5 m past the face is half way.
        let p = [1.0 - s, 2.0 + s, 3.0];
        assert!((v.weight_at(p) - 0.5).abs() < 1e-5, "{}", v.weight_at(p));
        // Box +X (the long axis) points along world (s, s, 0): 1.9 m out is still inside.
        assert_eq!(v.weight_at([1.0 + 1.9 * s, 2.0 + 1.9 * s, 3.0]), 1.0);
        let u = LightsViews {
            ambient_volumes: vec![v, FrameAmbientVolume::default()],
            ..LightsViews::default()
        }
        .uniform();
        let expected = [0.0, 0.0, -half.sin(), half.cos()];
        for (got, want) in u.ambient_rot[0].iter().zip(expected) {
            assert!((got - want).abs() < 1e-6, "{:?}", u.ambient_rot[0]);
        }
        assert_eq!(u.ambient_rot[1], IDENTITY_ROTATION, "an unturned volume");
        assert_eq!(u.ambient_rot[2], [0.0; 4], "unused slots stay zero");
        // A zero or non-finite rotation reads as the identity.
        let bad = FrameAmbientVolume {
            rotation: [0.0; 4],
            ..v
        };
        assert_eq!(bad.weight_at([1.0 + 1.9, 2.0, 3.0]), 1.0);
    }

    #[test]
    fn the_uniform_caps_lights_and_volumes_and_names_the_view() {
        let views = LightsViews {
            extra_directional: vec![DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 0.5, 0.25], 4.0); 5],
            ambient_volumes: vec![FrameAmbientVolume::default(); 6],
            shading_view: DebugViewMode::Normals,
            ..LightsViews::default()
        };
        let u = views.uniform();
        assert_eq!(u.view_mode, [2.0, 3.0, 4.0, 0.0]);
        assert_eq!(u.extra_dir[0], [0.0, -1.0, 0.0, 1.0]);
        assert_eq!(u.extra_dir[1], [4.0, 2.0, 1.0, 0.0]);
        assert_eq!(LightsViews::default().uniform(), LightsViewsUniform::default());
        // The rows read back as the lights they pack (colour × lux), the cap applied.
        let back = u.extra_lights();
        assert_eq!(back.len(), MAX_EXTRA_DIRECTIONAL);
        assert_eq!(back[0].direction, [0.0, -1.0, 0.0]);
        assert_eq!(back[0].color.map(|c| c * back[0].illuminance), [4.0, 2.0, 1.0]);
        assert!(LightsViewsUniform::default().extra_lights().is_empty());
    }

    #[test]
    fn every_reserved_mode_has_a_shader_code_or_a_material_swap() {
        let mut codes = std::collections::HashSet::new();
        for mode in [
            DebugViewMode::Unlit,
            DebugViewMode::Normals,
            DebugViewMode::BaseColor,
            DebugViewMode::Roughness,
            DebugViewMode::Metallic,
            DebugViewMode::AmbientOcclusion,
            DebugViewMode::LightComplexity,
            DebugViewMode::ShadowCascades,
            DebugViewMode::MotionVectors,
            DebugViewMode::MipLevel,
        ] {
            let code = shading_view_code(mode);
            assert!(code > 0, "{mode:?}");
            assert!(codes.insert(code), "{mode:?} shares a code");
        }
        for mode in [DebugViewMode::Overdraw, DebugViewMode::LodColor] {
            assert_eq!(shading_view_code(mode), 0);
            assert!(crate::debug_shading::handles(mode));
        }
        assert_eq!(shading_view_code(DebugViewMode::Lit), 0);
    }

    #[test]
    fn refusals_are_reported_once() {
        // The two flags are process-wide; whichever call reaches them first logs, so after one
        // refusal no further call logs.
        warn_refused_directional(1);
        assert!(
            !warn_refused_directional(1),
            "a second refusal is not logged again"
        );
        assert!(!warn_refused_directional(0));
        let _ = warn_refused_volumes(2);
        assert!(!warn_refused_volumes(2));
    }

    /// A volume of `intensity` at `x`, with the resolved visibility a real scene would carry.
    fn volume_at(intensity: f32, x: f32, inherited: bool) -> impl Bundle {
        (
            AmbientVolume {
                intensity,
                half_extents: [1.0, 2.0, 3.0],
                ..AmbientVolume::default()
            },
            regia_scene::GlobalTransform(regia_core::math::Affine3A::from_translation(
                regia_core::math::Vec3::new(x, 0.0, 0.0),
            )),
            InheritedVisibility(inherited),
        )
    }

    #[test]
    fn hidden_ambient_volumes_are_not_extracted_and_do_not_count_as_refused() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        // A visible volume is extracted...
        app.world_mut().spawn(volume_at(100.0, 1.0, true));
        // ...the same volume hidden by its own Visibility (its resolved
        // `InheritedVisibility(false)`) is not...
        app.world_mut().spawn(volume_at(200.0, 2.0, false));
        // ...one under a hidden parent (Visibility::Hidden above it; propagation writes
        // `InheritedVisibility(false)` on the child) is not...
        let hidden_parent = app
            .world_mut()
            .spawn((Visibility::Hidden, InheritedVisibility(false)))
            .id();
        app.world_mut()
            .spawn(volume_at(300.0, 3.0, false))
            .insert(ChildOf(hidden_parent));
        // ...and a parked one still is not.
        app.world_mut().spawn((
            volume_at(400.0, 4.0, true),
            LayerParked {
                restore: Visibility::Visible,
                inherited: false,
            },
        ));
        app.update();
        let frame = app.world().resource::<ExtractedFrame>();
        assert_eq!(
            frame.lights_views.ambient_volumes.len(),
            1,
            "only the visible volume lights the frame"
        );
        assert_eq!(
            frame.lights_views.refused_ambient_volumes, 0,
            "hidden and parked volumes are not extracted, so they are not kept-over-budget refusals"
        );
        assert_eq!(frame.lights_views.ambient_volumes[0].irradiance, [100.0; 3]);
        assert_eq!(frame.lights_views.ambient_volumes[0].center, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn a_hidden_directional_light_is_not_extracted_and_does_not_count_as_refused() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        // The brighter light is hidden: the visible one must shade the frame alone.
        app.world_mut()
            .spawn((light(5_000.0, false), InheritedVisibility(true)));
        app.world_mut()
            .spawn((light(50_000.0, true), InheritedVisibility(false)));
        app.update();
        let frame = app.world().resource::<ExtractedFrame>();
        let sun = frame.directional.expect("the visible light shades the frame");
        assert_eq!(
            sun.illuminance, 5_000.0,
            "the hidden brighter light is not extracted"
        );
        assert_eq!(
            frame.lights_views.refused_directional, 0,
            "a skipped light is not a kept-over-budget refusal"
        );
        assert!(frame.lights_views.extra_directional.is_empty());
    }

    /// Spawns a parent group with `parent_visibility` and, under it, a child that carries only
    /// `child` and a `Transform` (no `Visibility`, no `InheritedVisibility`: the shape the editor's
    /// default sun and a hand-spawned volume have), then runs the scene's real propagation and the
    /// frame extraction.
    fn extract_under_parent(parent_visibility: Visibility, child: impl Bundle) -> (App, Entity) {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        let parent = app
            .world_mut()
            .spawn((regia_scene::Transform::default(), parent_visibility))
            .id();
        app.world_mut()
            .spawn((regia_scene::Transform::default(), child))
            .insert(ChildOf(parent));
        regia_scene::propagate::propagate_transforms(app.world_mut());
        app.update();
        (app, parent)
    }

    #[test]
    fn a_directional_light_under_a_hidden_parent_lights_nothing_through_real_propagation() {
        let (app, _) = extract_under_parent(Visibility::Hidden, light(50_000.0, true));
        let frame = app.world().resource::<ExtractedFrame>();
        assert!(
            frame.directional.is_none(),
            "a sun under a hidden group lights nothing"
        );
        assert!(frame.lights_views.extra_directional.is_empty());
        assert_eq!(frame.lights_views.refused_directional, 0);

        // The control: the same child under a visible group still lights the frame.
        let (app, _) = extract_under_parent(Visibility::Visible, light(50_000.0, true));
        let frame = app.world().resource::<ExtractedFrame>();
        let sun = frame.directional.expect("a sun under a visible group lights");
        assert_eq!(sun.illuminance, 50_000.0);
    }

    #[test]
    fn an_ambient_volume_under_a_hidden_parent_lights_nothing_through_real_propagation() {
        let fill = AmbientVolume {
            intensity: 300.0,
            ..AmbientVolume::default()
        };
        let (app, _) = extract_under_parent(Visibility::Hidden, fill.clone());
        let frame = app.world().resource::<ExtractedFrame>();
        assert!(
            frame.lights_views.ambient_volumes.is_empty(),
            "a volume under a hidden group lights nothing"
        );
        assert_eq!(frame.lights_views.refused_ambient_volumes, 0);

        let (app, _) = extract_under_parent(Visibility::Visible, fill);
        let frame = app.world().resource::<ExtractedFrame>();
        assert_eq!(frame.lights_views.ambient_volumes.len(), 1);
        assert_eq!(frame.lights_views.ambient_volumes[0].irradiance, [300.0; 3]);
    }

    /// The editor's default sun is `(Name, DirectionalLight)`: no `Transform`. Parented under a
    /// hidden group it goes dark; unparented again it leaves propagation's tracked set (no
    /// `Transform`, `ChildOf` or `Visibility`), and must light again rather than keep the hidden
    /// group's resolved flag.
    #[test]
    fn a_transformless_sun_taken_out_of_a_hidden_group_lights_again() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        let group = app
            .world_mut()
            .spawn((regia_scene::Transform::default(), Visibility::Hidden))
            .id();
        let sun = app.world_mut().spawn(light(30_000.0, true)).id();
        app.world_mut().entity_mut(sun).insert(ChildOf(group));
        regia_scene::propagate::propagate_transforms(app.world_mut());
        app.update();
        assert!(
            app.world().resource::<ExtractedFrame>().directional.is_none(),
            "dark under the hidden group"
        );
        app.world_mut().entity_mut(sun).remove::<ChildOf>();
        regia_scene::propagate::propagate_transforms(app.world_mut());
        app.update();
        let lit = app
            .world()
            .resource::<ExtractedFrame>()
            .directional
            .expect("out of the hidden group, the sun lights again");
        assert_eq!(lit.illuminance, 30_000.0);
    }

    #[test]
    fn showing_the_parent_again_restores_the_light() {
        let (mut app, parent) = extract_under_parent(Visibility::Hidden, light(20_000.0, false));
        assert!(app.world().resource::<ExtractedFrame>().directional.is_none());
        app.world_mut().entity_mut(parent).insert(Visibility::Inherited);
        regia_scene::propagate::propagate_transforms(app.world_mut());
        app.update();
        let sun = app
            .world()
            .resource::<ExtractedFrame>()
            .directional
            .expect("the shown group's sun lights again");
        assert_eq!(sun.illuminance, 20_000.0);
    }
}
