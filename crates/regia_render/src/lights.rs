// SPDX-License-Identifier: MPL-2.0
//! Light types and clustering configuration (`specs/13` § 5–6).

use regia_app::prelude::*;

/// Physical light units for artist-facing intensities (`specs/13` § 6).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LightUnits {
    /// Directional illuminance in lux.
    #[default]
    Lux,
    /// Point/spot luminous power in lumens.
    Lumens,
    /// Spot luminous intensity in candela.
    Candela,
}

/// Directional light (sun).
///
/// As a component, `direction` is the light's travel direction; positions of
/// punctual lights come from the entity's `GlobalTransform`.
///
/// The component requires `InheritedVisibility`, so a sun under a hidden group is hidden with it
/// (scene propagation resolves the flag; the light needs no `Visibility` of its own) and lights
/// neither the frame nor a bake. `InheritedVisibility` is derived state and never saved.
///
/// # Example
///
/// ```
/// use regia_render::DirectionalLight;
///
/// let sun = DirectionalLight {
///     direction: [-0.5, -0.8, -0.3],
///     color: [1.0, 1.0, 1.0],
///     illuminance: 100_000.0, // lux
///     cast_shadows: true,
///     static_shadow: false,
/// };
/// assert!(sun.cast_shadows);
/// assert_eq!(sun.illuminance, 100_000.0);
/// ```
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default, Clone)]
#[require(regia_scene::components::InheritedVisibility)]
pub struct DirectionalLight {
    /// Direction the light travels (normalized); the shader negates it to point at the light
    /// (`l = -normalize(direction)`).
    pub direction: [f32; 3],
    /// Linear RGB colour.
    pub color: [f32; 3],
    /// Illuminance in lux.
    pub illuminance: f32,
    /// Casts cascaded shadow maps when true.
    pub cast_shadows: bool,
    /// Static shadow cache eligible.
    #[reflect(@regia_ecs::attrs::EditCondition("cast_shadows"))]
    pub static_shadow: bool,
}

/// Point light.
///
/// The component requires `InheritedVisibility` (derived, never saved): a hidden light (its own
/// `Visibility::Hidden` or a hidden ancestor) is not extracted and lights nothing (ADR-7670).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default, Clone)]
#[require(regia_scene::components::InheritedVisibility)]
pub struct PointLight {
    /// Unused as a component: extraction takes the position from the entity's
    /// `GlobalTransform`, so move the entity instead. Kept for the standalone struct.
    pub position: [f32; 3],
    /// Linear RGB colour.
    pub color: [f32; 3],
    /// Intensity (lumens).
    pub intensity: f32,
    /// Cutoff radius in world units.
    pub radius: f32,
    /// Casts shadows into the local-light atlas when true.
    pub cast_shadows: bool,
}

/// Spot light.
///
/// As a component, `direction` is ignored in favour of the entity's −Z axis
/// (the engine's forward), so aiming happens by rotating the entity.
///
/// The component requires `InheritedVisibility` (derived, never saved): a hidden light (its own
/// `Visibility::Hidden` or a hidden ancestor) is not extracted and lights nothing (ADR-7670).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default, Clone)]
#[require(regia_scene::components::InheritedVisibility)]
pub struct SpotLight {
    /// Unused as a component: extraction takes the position from the entity's
    /// `GlobalTransform`, so move the entity instead. Kept for the standalone struct.
    pub position: [f32; 3],
    /// Direction (normalized).
    pub direction: [f32; 3],
    /// Linear RGB colour.
    pub color: [f32; 3],
    /// Intensity (candela).
    pub intensity: f32,
    /// Outer cone half-angle in radians.
    pub outer_angle: f32,
    /// Inner cone half-angle in radians.
    pub inner_angle: f32,
    /// Range in world units.
    pub range: f32,
    /// Casts shadows when true.
    pub cast_shadows: bool,
}

/// Physical size of a point or spot light's emitter: a sphere of `radius_m` metres.
///
/// Optional. Put it next to a [`PointLight`] or [`SpotLight`] on the same entity. A light
/// without it keeps the engine's defaults: its local shadow uses a 45 cm source
/// ([`crate::shadow::LOCAL_SHADOW_SOURCE_RADIUS_M`]) and its specular highlight is a point's.
/// With it:
///
/// - the PCSS penumbra of the light's atlas shadow scales with `radius_m` (Fernando 2005);
/// - the forward specular reads the sphere's representative point and renormalizes the GGX
///   lobe for the wider light (Lagarde & de Rousiers 2014 § 4.7.5, which uses Karis 2013
///   eqs. 10, 11 and 14);
/// - the reference path tracer traces a point light as a sphere light of this radius.
///
/// A radius of 0 is a punctual highlight. On a PCSS profile the local shadow filter still
/// clamps to a 2.5-texel contact floor, so the edge is not a single compare tap. A negative
/// value counts as 0 and a non-finite one as absent.
///
/// # Example
///
/// ```
/// use regia_render::{LightSourceSize, PointLight};
///
/// // A 5 cm bulb.
/// let bulb = (PointLight::default(), LightSourceSize { radius_m: 0.05 });
/// assert_eq!(bulb.1.radius_m, 0.05);
/// ```
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct LightSourceSize {
    /// Radius of the emitting sphere, metres.
    pub radius_m: f32,
}

/// The size an absent component stands for (45 cm), so adding the component with its
/// default changes the highlight but not the shadow.
impl Default for LightSourceSize {
    fn default() -> Self {
        Self {
            radius_m: crate::shadow::LOCAL_SHADOW_SOURCE_RADIUS_M,
        }
    }
}

impl LightSourceSize {
    /// The radius the renderer uses: negative clamps to 0, non-finite is `None` (as if the
    /// component were absent).
    #[must_use]
    pub fn sanitized(self) -> Option<f32> {
        self.radius_m.is_finite().then_some(self.radius_m.max(0.0))
    }
}

/// Shades a sized light through the LTC tables (spec 13 § 6, ADR-2592): put it next to a [`LightSourceSize`] on a
/// [`PointLight`] or [`SpotLight`]. The sphere's silhouette goes through the same tables as a rect light, for
/// both the diffuse and the specular, with the physical `1 / d²` of the path tracer's sphere instead of the
/// punctual falloff's 1 m knee, so a sphere dipping below a surface's horizon lights it correctly. Without this
/// marker a sized light keeps the representative-point form (ADR-1367) bit for bit.
///
/// Materials the tables do not describe (clearcoat, sheen, anisotropy, skin, hair), lights under about 1.1° of
/// angular radius and the terrain's shared pass take the representative point, with the same falloff.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default, Clone)]
pub struct LightSourceLtc;

/// An IES photometric profile on a [`PointLight`] or [`SpotLight`] entity (spec 13 § 6, ADR-2592): the light's
/// intensity is shaped by the profile's distribution. The profile is looked up by name in the
/// [`crate::LightProfileLibrary`] (the agent's `render.set_light_profile` fills it, or the game from a cooked
/// `.ies`); an unknown name shades the light without it. A spot keeps its cone on top of the profile; the
/// profile's nadir is the spot's direction, or a point light's rotated `-Y`.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct LightProfile {
    /// Name of the profile in the library (normally the `.ies` asset path).
    pub profile: String,
    /// Take the file's candela (× `scale`) as the light's intensity. Off: a spot's `intensity` is the profile's
    /// peak, and a point light keeps its lumens spread over the profile.
    pub file_intensity: bool,
    /// Multiplier on the file's candela when `file_intensity` is set.
    pub scale: f32,
    /// Turn of the profile's horizontal 0° about the nadir, radians.
    pub rotation: f32,
}

impl Default for LightProfile {
    fn default() -> Self {
        Self {
            profile: String::new(),
            file_intensity: false,
            scale: 1.0,
            rotation: 0.0,
        }
    }
}

/// An authored rectangular area light (spec 13 § 6, ADR-2592), placed by the entity's transform: `width` along
/// its `X`, `height` along its `Y`, emitting toward its `-Z`. Shaded with the GGX LTC tables (the fitted
/// emitters' path); its shadow is a distance probe traced once against the scene (static occluders).
///
/// The component requires `InheritedVisibility` (derived, never saved): a hidden light (its own
/// `Visibility::Hidden` or a hidden ancestor) is not extracted and lights nothing (ADR-7670).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default, Clone)]
#[require(regia_scene::components::InheritedVisibility)]
pub struct RectLight {
    /// Linear RGB.
    pub color: [f32; 3],
    /// Luminous power per emitting face, lumens.
    pub intensity: f32,
    /// Width, metres.
    pub width: f32,
    /// Height, metres.
    pub height: f32,
    /// Emit from both faces.
    pub two_sided: bool,
    /// Influence range, metres; 0 takes the fitted emitters' cutoff (4 % of middle grey).
    pub range: f32,
    /// Trace the distance-probe shadow.
    pub cast_shadows: bool,
}

impl Default for RectLight {
    fn default() -> Self {
        Self {
            color: [1.0; 3],
            intensity: 1000.0,
            width: 1.0,
            height: 0.5,
            two_sided: false,
            range: 0.0,
            cast_shadows: true,
        }
    }
}

/// An authored tube (linear) light (spec 13 § 6, ADR-2592): a Lambertian cylinder of `radius` along the entity's
/// `X`, `length` long. Shaded like [`RectLight`].
///
/// The component requires `InheritedVisibility` (derived, never saved): a hidden light (its own
/// `Visibility::Hidden` or a hidden ancestor) is not extracted and lights nothing (ADR-7670).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default, Clone)]
#[require(regia_scene::components::InheritedVisibility)]
pub struct TubeLight {
    /// Linear RGB.
    pub color: [f32; 3],
    /// Luminous power, lumens.
    pub intensity: f32,
    /// Length, metres.
    pub length: f32,
    /// Radius, metres.
    pub radius: f32,
    /// Influence range, metres; 0 takes the fitted emitters' cutoff.
    pub range: f32,
    /// Trace the distance-probe shadow.
    pub cast_shadows: bool,
}

impl Default for TubeLight {
    fn default() -> Self {
        Self {
            color: [1.0; 3],
            intensity: 1000.0,
            length: 1.2,
            radius: 0.015,
            range: 0.0,
            cast_shadows: true,
        }
    }
}

/// A noon sun: straight-ish down from the south-east, 100 klx, casting shadows. The
/// reflection default a level file or the editor's "add component" starts from.
impl Default for DirectionalLight {
    fn default() -> Self {
        Self {
            direction: [-0.4, -0.8, -0.45],
            color: [1.0, 0.97, 0.92],
            illuminance: 100_000.0,
            cast_shadows: true,
            static_shadow: false,
        }
    }
}

/// An 800 lm white bulb with a 10 m reach (the position comes from the entity transform).
impl Default for PointLight {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            color: [1.0; 3],
            intensity: 800.0,
            radius: 10.0,
            cast_shadows: false,
        }
    }
}

/// A 1000 cd white spot, 30° outer / 20° inner half-angles, 15 m range, aimed down the
/// entity's −Z.
impl Default for SpotLight {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            direction: [0.0, 0.0, -1.0],
            color: [1.0; 3],
            intensity: 1000.0,
            outer_angle: 30f32.to_radians(),
            inner_angle: 20f32.to_radians(),
            range: 15.0,
            cast_shadows: false,
        }
    }
}

/// Froxel grid dimensions (`specs/13` § 5): 16×9×24 with exponential Z.
/// Froxel grid width in cells.
pub const CLUSTER_GRID_X: u32 = 16;
/// Froxel grid height in cells.
pub const CLUSTER_GRID_Y: u32 = 9;
/// Froxel grid depth slices along the view axis.
pub const CLUSTER_GRID_Z: u32 = 24;

/// Clusters in the froxel grid.
pub const CLUSTER_COUNT: u32 = CLUSTER_GRID_X * CLUSTER_GRID_Y * CLUSTER_GRID_Z;

/// Smallest lights-per-cluster room the cluster index buffer is made with.
const CLUSTER_INDEX_MIN_CAPACITY: u32 = 16;

/// Lights per cluster the cluster index buffer should have room for, when it has room for
/// `current` (0: no buffer yet) and the frame caps a cluster at `max_per_cluster` lights
/// (ADR-3351). The cap is rounded up to a power of two, at least 16 and at most
/// `MAX_LIGHTS_PER_CLUSTER` (256); a buffer with room keeps its size, and a larger need at
/// least doubles it, so a run of slowly rising caps replaces the buffer a few times, not every time.
#[must_use]
pub fn cluster_index_capacity(current: u32, max_per_cluster: u32) -> u32 {
    let max = crate::profile::MAX_LIGHTS_PER_CLUSTER;
    let want = max_per_cluster
        .clamp(1, max)
        .next_power_of_two()
        .clamp(CLUSTER_INDEX_MIN_CAPACITY, max);
    if current >= want {
        current
    } else {
        want.max(current.saturating_mul(2)).min(max)
    }
}

/// Bytes of the cluster index buffer with room for `capacity` lights per cluster (one `u32` each).
#[must_use]
pub fn cluster_index_bytes(capacity: u32) -> u64 {
    u64::from(CLUSTER_COUNT) * u64::from(capacity) * 4
}

/// CPU-side froxel index from normalized screen UV and view-space depth.
#[must_use]
pub fn cluster_index_3d(cell_x: u32, cell_y: u32, cell_z: u32) -> u32 {
    cell_x + CLUSTER_GRID_X * (cell_y + CLUSTER_GRID_Y * cell_z)
}

/// Maps positive view-space depth to an exponentially spaced Z slice.
///
/// Slice `i` covers depths from `near * (far/near)^(i/Z)` to the next boundary
/// (Olsson, Billeter, Assarsson, HPG 2012). The shader in `clustering.wgsl` uses the same split.
#[must_use]
pub fn depth_to_cluster_z(depth: f32, near: f32, far: f32) -> u32 {
    if !(depth.is_finite() && near.is_finite() && far.is_finite()) || near <= 0.0 || far <= near {
        return 0;
    }
    if depth <= near {
        return 0;
    }
    if depth >= far {
        return CLUSTER_GRID_Z - 1;
    }
    let t = (depth / near).ln() / (far / near).ln();
    let ez = (t * (CLUSTER_GRID_Z as f32)).floor() as u32;
    ez.min(CLUSTER_GRID_Z - 1)
}

/// Positive view-space depth at the near plane of cluster slice `slice`.
#[must_use]
pub fn cluster_slice_depth(slice: u32, near: f32, far: f32) -> f32 {
    let z = CLUSTER_GRID_Z as f32;
    near * (far / near).powf(slice as f32 / z)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_profile_default_is_an_empty_ies_slot() {
        let profile = LightProfile::default();
        assert!(profile.profile.is_empty());
        assert!(!profile.file_intensity);
        assert_eq!(profile.scale, 1.0);
        assert_eq!(profile.rotation, 0.0);
        let sun = DirectionalLight::default();
        assert_eq!(sun.illuminance, 100_000.0);
        assert_eq!(sun.direction, [-0.4, -0.8, -0.45]);
        assert!(sun.cast_shadows);
    }

    #[test]
    fn the_cluster_index_buffer_starts_at_the_cap_and_grows_by_doubling() {
        let max = crate::profile::MAX_LIGHTS_PER_CLUSTER;
        // First buffer: the frame's cap rounded up, never below 16 or above the maximum.
        assert_eq!(cluster_index_capacity(0, 16), 16);
        assert_eq!(cluster_index_capacity(0, 1), 16);
        assert_eq!(cluster_index_capacity(0, 64), 64);
        assert_eq!(cluster_index_capacity(0, 100), 128);
        assert_eq!(cluster_index_capacity(0, 10_000), max);
        // Room enough: kept, even for a lower cap.
        assert_eq!(cluster_index_capacity(128, 64), 128);
        assert_eq!(cluster_index_capacity(64, 64), 64);
        // More needed: at least doubled.
        assert_eq!(cluster_index_capacity(16, 17), 32);
        assert_eq!(cluster_index_capacity(16, 128), 128);
        assert_eq!(cluster_index_capacity(128, 129), max);
        // The old fixed size, and the Mobile and Desktop High sizes.
        assert_eq!(cluster_index_bytes(max), 3_538_944);
        assert_eq!(cluster_index_bytes(16), 221_184);
        assert_eq!(cluster_index_bytes(64), 884_736);
    }

    #[test]
    fn exponential_slices_are_finer_near_the_camera() {
        let near = 0.1;
        let far = 100.0;
        assert_eq!(depth_to_cluster_z(near, near, far), 0);
        assert_eq!(depth_to_cluster_z(far, near, far), CLUSTER_GRID_Z - 1);
        let mid_linear = (near + far) * 0.5;
        let mid_slice = depth_to_cluster_z(mid_linear, near, far);
        assert!(
            mid_slice > CLUSTER_GRID_Z / 2,
            "exponential grid spends more slices near the camera"
        );
        let boundary = cluster_slice_depth(1, near, far);
        assert_eq!(depth_to_cluster_z(boundary + 1e-4, near, far), 1);
    }
}
