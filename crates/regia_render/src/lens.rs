// SPDX-License-Identifier: MPL-2.0
//! Camera lens effects: motion blur ([`crate::motion_blur`]) and depth of field ([`crate::dof`]),
//! driven by a camera's [`PhysicalCamera`] and allowed per quality preset
//! ([`RenderProfile::motion_blur`], [`RenderProfile::depth_of_field`]).
//!
//! Both are opt-in twice over (ADR-041): a preset allows them, a camera asks for them. A frame
//! with neither, or with a camera that carries no [`PhysicalCamera`], is unchanged: the passes are
//! not recorded and the post stack reads the scene HDR as before.
//!
//! They run on the pre-exposure HDR after the temporal resolve and the translucent layers, before
//! the post stack (exposure, bloom, tonemap): motion blur first, then depth of field.

use crate::RenderError;
use crate::device::TrackedDevice;
use crate::dof::{DepthOfField, DepthOfFieldMode, DepthOfFieldPass, SUPER_35_SENSOR_HEIGHT};
use crate::frame_work::counted::*;
use crate::motion_blur::{MotionBlur, MotionBlurPass};
use crate::post::PostSettings;
use crate::profile::RenderProfile;
use regia_app::prelude::*;
use std::borrow::Cow;
use std::collections::HashMap;

/// Format of the lens targets (the HDR the post stack reads).
const LENS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The physical lens and shutter of a camera entity: shutter angle for motion blur, focus
/// distance, aperture and sensor for depth of field.
///
/// Put it next to a [`Camera`](crate::Camera). Each effect also needs the quality preset to allow
/// it ([`RenderProfile::motion_blur`], [`RenderProfile::depth_of_field`]); a preset that does not
/// leaves the frame as if the component were absent.
///
/// Exposure follows [`Self::exposure`] (ADR-1464). The default, [`CameraExposureMode::Inherit`],
/// leaves exposure to the post stack (auto exposure or the fixed EV), so racking the aperture for
/// a shallower depth of field does not brighten the frame. [`CameraExposureMode::Physical`] takes
/// the exposure from [`Self::iso`], [`Self::shutter_seconds`] and [`Self::aperture_f_stops`]
/// (`EV100 = log2(N² / t) − log2(ISO / 100)`), and the shutter time then also sets the motion
/// blur ([`Self::shutter_fraction`]). [`Self::focal_length`], when set, gives the view's field of
/// view ([`Self::vertical_fov`]).
///
/// # Example
///
/// ```
/// use regia_render::{DepthOfFieldMode, PhysicalCamera, PostSettings, RenderProfile, RenderProfilePreset};
///
/// let lens = PhysicalCamera {
///     focal_distance: 6.0,
///     aperture_f_stops: 2.0,
///     depth_of_field: Some(DepthOfFieldMode::Bokeh),
///     ..PhysicalCamera::default()
/// };
/// let mut post = PostSettings::default();
/// lens.apply(&mut post, &RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
/// assert_eq!(post.depth_of_field.map(|d| d.focal_distance), Some(6.0));
/// assert!(post.motion_blur.is_some(), "a 180° shutter by default");
///
/// let mut mobile = PostSettings::default();
/// lens.apply(&mut mobile, &RenderProfile::from_preset(RenderProfilePreset::MobileLow));
/// assert!(mobile.depth_of_field.is_none() && mobile.motion_blur.is_none());
/// ```
///
/// Saved in scene files as the reflected component `regia_render::lens::PhysicalCamera`
/// (`physical-camera` v1, ADR-2323): a component without `format_version` loads as v1, a newer
/// one is ignored by the renderer (logged once).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default, Clone)]
#[type_path = "regia_render::lens"]
pub struct PhysicalCamera {
    /// Serialized-form version ([`PHYSICAL_CAMERA_FORMAT_VERSION`] when written by this engine).
    pub format_version: u32,
    /// Fraction of the frame the shutter is open (`0.5` = 180°); `0` disables motion blur.
    pub shutter_angle: f32,
    /// Motion blur samples per direction ([`MotionBlur::samples`]); `0` disables motion blur.
    pub motion_blur_samples: u32,
    /// Depth of field kernel; `None` keeps everything in focus.
    pub depth_of_field: Option<DepthOfFieldMode>,
    /// Distance to the plane in focus, metres.
    pub focal_distance: f32,
    /// Aperture, f-stops.
    pub aperture_f_stops: f32,
    /// Sensor height, metres (Super 35 by default).
    pub sensor_height: f32,
    /// Largest circle of confusion, pixels at the output resolution.
    pub max_circle_of_confusion_diameter: f32,
    /// Depth beyond which everything blurs as if it were here, metres (the sky's stand-in).
    pub max_depth: f32,
    /// Where the view's exposure comes from ([`CameraExposureMode::Inherit`] by default).
    pub exposure: CameraExposureMode,
    /// Sensor sensitivity, ISO arithmetic speed (exposure in [`CameraExposureMode::Physical`]).
    pub iso: f32,
    /// Shutter time, seconds (exposure and motion blur in [`CameraExposureMode::Physical`]).
    pub shutter_seconds: f32,
    /// Frame rate the shutter time is measured against for motion blur in
    /// [`CameraExposureMode::Physical`]: the blur covers `shutter_seconds × shutter_reference_fps`
    /// of a frame's motion. Fixed (not the measured frame rate) so the blur is deterministic.
    pub shutter_reference_fps: f32,
    /// Focal length, metres; `Some` sets the view's vertical field of view from
    /// [`Self::sensor_height`] ([`Self::vertical_fov`]), `None` keeps the camera's.
    pub focal_length: Option<f32>,
}

/// Where a camera's exposure comes from ([`PhysicalCamera::exposure`], ADR-1464).
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Default)]
#[reflect(Default, Clone)]
#[type_path = "regia_render::lens"]
pub enum CameraExposureMode {
    /// The post stack decides: auto exposure, or the fixed EV (`Camera::exposure_ev`). The
    /// behaviour of a camera without the mode.
    #[default]
    Inherit,
    /// A fixed EV100; auto exposure off.
    Manual {
        /// The exposure value at ISO 100.
        ev100: f32,
    },
    /// EV100 from the lens's ISO, shutter time and aperture; auto exposure off.
    Physical,
}

impl CameraExposureMode {
    /// The wire name: `inherit`, `manual` or `physical`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Manual { .. } => "manual",
            Self::Physical => "physical",
        }
    }
}

impl Default for PhysicalCamera {
    fn default() -> Self {
        Self {
            format_version: PHYSICAL_CAMERA_FORMAT_VERSION,
            shutter_angle: 0.5,
            motion_blur_samples: 2,
            depth_of_field: None,
            focal_distance: 10.0,
            aperture_f_stops: 2.8,
            sensor_height: SUPER_35_SENSOR_HEIGHT,
            max_circle_of_confusion_diameter: 48.0,
            max_depth: f32::INFINITY,
            exposure: CameraExposureMode::Inherit,
            iso: 100.0,
            shutter_seconds: 1.0 / 60.0,
            shutter_reference_fps: 30.0,
            focal_length: None,
        }
    }
}

/// The `physical-camera` format version this build reads and writes (ADR-2323).
pub const PHYSICAL_CAMERA_FORMAT_VERSION: u32 = 1;

/// A [`PhysicalCamera`] from a newer engine.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PhysicalCameraError {
    /// The component's `format_version` is newer than the engine's.
    #[error(
        "physical camera format_version {found} is newer than this engine's {PHYSICAL_CAMERA_FORMAT_VERSION}"
    )]
    NewerVersion {
        /// The version found.
        found: u32,
    },
}

impl PhysicalCamera {
    /// Brings data written by an older engine up to [`PHYSICAL_CAMERA_FORMAT_VERSION`]: `0` (a
    /// hand-written component without the field) becomes 1.
    ///
    /// # Errors
    /// [`PhysicalCameraError::NewerVersion`] for data from a newer engine.
    pub fn migrate(mut self) -> Result<Self, PhysicalCameraError> {
        match self.format_version {
            0 => {
                self.format_version = 1;
                Ok(self)
            }
            PHYSICAL_CAMERA_FORMAT_VERSION => Ok(self),
            found => Err(PhysicalCameraError::NewerVersion { found }),
        }
    }

    /// The renderer can read this version (it is not newer than the engine's).
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.format_version <= PHYSICAL_CAMERA_FORMAT_VERSION
    }

    /// The motion blur this lens asks for, `None` when it is off.
    #[must_use]
    pub fn motion_blur(&self) -> Option<MotionBlur> {
        let blur = MotionBlur {
            shutter_angle: self.shutter_fraction(),
            samples: self.motion_blur_samples,
        };
        blur.is_active().then_some(blur)
    }

    /// The depth of field this lens asks for, `None` when it is off.
    #[must_use]
    pub fn depth_of_field(&self) -> Option<DepthOfField> {
        let dof = DepthOfField {
            mode: self.depth_of_field?,
            focal_distance: self.focal_distance,
            sensor_height: self.sensor_height,
            aperture_f_stops: self.aperture_f_stops,
            max_circle_of_confusion_diameter: self.max_circle_of_confusion_diameter,
            max_depth: self.max_depth,
        };
        dof.is_active().then_some(dof)
    }

    /// Writes this lens's effects into a view's post settings, each only where `profile` allows it.
    pub fn apply(&self, post: &mut PostSettings, profile: &RenderProfile) {
        post.motion_blur = self.motion_blur().filter(|_| profile.motion_blur);
        post.depth_of_field = self.depth_of_field().filter(|_| profile.depth_of_field);
    }

    /// EV100 of this lens's aperture, shutter time and ISO ([`crate::ev100_from_camera`]).
    #[must_use]
    pub fn ev100(&self) -> f32 {
        crate::post::ev100_from_camera(self.aperture_f_stops, self.shutter_seconds, self.iso)
    }

    /// The fraction of a frame the shutter is open, the motion blur's strength: in
    /// [`CameraExposureMode::Physical`] `shutter_seconds × shutter_reference_fps` (0 when either
    /// is not finite or not positive), otherwise [`Self::shutter_angle`].
    #[must_use]
    pub fn shutter_fraction(&self) -> f32 {
        match self.exposure {
            CameraExposureMode::Physical => {
                let fraction = self.shutter_seconds * self.shutter_reference_fps;
                if fraction.is_finite() && fraction > 0.0 {
                    fraction
                } else {
                    0.0
                }
            }
            _ => self.shutter_angle,
        }
    }

    /// The vertical field of view, radians, [`Self::focal_length`] gives on [`Self::sensor_height`];
    /// `None` without a focal length or when either is not finite and positive.
    #[must_use]
    pub fn vertical_fov(&self) -> Option<f32> {
        let focal = self.focal_length?;
        let positive = |v: f32| v.is_finite() && v > 0.0;
        (positive(focal) && positive(self.sensor_height))
            .then(|| crate::dof::vertical_fov_from_focal_length(self.sensor_height, focal))
    }

    /// Writes this lens's exposure into a view's post settings ([`Self::exposure`]):
    /// `Inherit` changes nothing; `Manual` pins the EV100 (a non-finite one is ignored);
    /// `Physical` turns auto exposure off and hands the post stack this lens's aperture,
    /// shutter and ISO (and their EV100 as the fixed EV the no-post paths read). Exposure
    /// compensation keeps applying. The renderer calls it after `Camera::exposure_ev`, so a
    /// manual or physical lens wins over the camera's pin.
    pub fn apply_exposure(&self, post: &mut PostSettings) {
        match self.exposure {
            CameraExposureMode::Inherit => {}
            CameraExposureMode::Manual { ev100 } => {
                if ev100.is_finite() {
                    post.auto_exposure = false;
                    post.use_physical_camera = false;
                    post.exposure_ev = ev100;
                }
            }
            CameraExposureMode::Physical => {
                post.auto_exposure = false;
                post.use_physical_camera = true;
                post.aperture = self.aperture_f_stops;
                post.shutter_seconds = self.shutter_seconds;
                post.iso = self.iso;
                post.exposure_ev = self.ev100();
            }
        }
    }
}

/// What the lens passes read from the frame.
pub(crate) struct LensInputs<'a> {
    /// The HDR the post stack would read.
    pub hdr: &'a wgpu::TextureView,
    /// Its size.
    pub width: u32,
    /// Its size.
    pub height: u32,
    /// The velocity target, `None` on a frame without the temporal resolve.
    pub velocity: Option<&'a wgpu::TextureView>,
    /// Single-sample scene depth (render size).
    pub depth: &'a wgpu::TextureView,
    /// Vertical field of view, radians, and the near / far planes, metres.
    pub projection: (f32, f32, f32),
    /// Frame index (noise decorrelation).
    pub frame: u64,
}

/// One lens target.
struct Target {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// The lens passes and their targets, owned by the frame renderer.
#[derive(Default)]
pub(crate) struct LensPasses {
    motion_blur: MotionBlurPass,
    dof: DepthOfFieldPass,
    /// Size the targets were made for.
    size: (u32, u32),
    /// `[output, scratch 0, scratch 1]`, created on first use.
    targets: [Option<Target>; 3],
    /// Logged once: motion blur asked for on a frame without velocity.
    warned_no_velocity: bool,
}

impl LensPasses {
    /// `true` when `settings` asks for a lens effect (the frame then passes its depth and velocity).
    pub(crate) fn wanted(settings: &PostSettings) -> bool {
        settings.motion_blur.is_some_and(|m| m.is_active())
            || settings.depth_of_field.is_some_and(|d| d.is_active())
    }

    fn target(&mut self, device: &TrackedDevice, slot: usize) -> wgpu::TextureView {
        let (width, height) = self.size;
        self.targets[slot]
            .get_or_insert_with(|| {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(["lens output", "lens scratch 0", "lens scratch 1"][slot]),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: LENS_FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                Target {
                    _texture: texture,
                    view,
                }
            })
            .view
            .clone()
    }

    /// Records the lens effects `settings` asks for. Returns the view the post stack should read
    /// instead of `inputs.hdr`, or `None` when nothing ran (the frame is unchanged).
    pub(crate) fn record(
        &mut self,
        device: &TrackedDevice,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        settings: &PostSettings,
        inputs: &LensInputs<'_>,
    ) -> Result<Option<wgpu::TextureView>, RenderError> {
        let blur = settings.motion_blur.filter(MotionBlur::is_active);
        let blur = match (blur, inputs.velocity) {
            (Some(blur), Some(velocity)) => Some((blur, velocity)),
            (Some(_), None) => {
                if !self.warned_no_velocity {
                    self.warned_no_velocity = true;
                    tracing::debug!("motion blur skipped: the frame has no velocity target (TAA is off)");
                }
                None
            }
            _ => None,
        };
        let dof = settings.depth_of_field.filter(DepthOfField::is_active);
        if blur.is_none() && dof.is_none() {
            return Ok(None);
        }
        let _span = tracing::info_span!("lens").entered();
        let _crumb = crate::breadcrumbs::frame_pass("lens");
        let size = (inputs.width.max(1), inputs.height.max(1));
        if self.size != size {
            self.size = size;
            self.targets = [None, None, None];
        }
        let output = self.target(device, 0);
        let mut src = inputs.hdr.clone();
        if let Some((blur, velocity)) = blur {
            self.motion_blur.record(
                device,
                belt,
                encoder,
                &src,
                velocity,
                inputs.depth,
                &output,
                &blur,
                inputs.frame,
            )?;
            // Depth of field reads the blurred image; its second pass writes `output` again,
            // after its first pass has read it into the scratch targets.
            src = output.clone();
        }
        if let Some(dof) = dof {
            let scratch0 = self.target(device, 1);
            let scratch1 = match dof.mode {
                DepthOfFieldMode::Bokeh => self.target(device, 2),
                DepthOfFieldMode::Gaussian => scratch0.clone(),
            };
            self.dof.record(
                device,
                belt,
                encoder,
                &src,
                size,
                inputs.depth,
                [&scratch0, &scratch1],
                &output,
                &dof,
                inputs.projection,
            )?;
        }
        Ok(Some(output))
    }
}

/// A composed WGSL module.
pub(crate) fn shader_module(
    device: &TrackedDevice,
    label: &str,
    source: &str,
    file: &str,
    defs: &[&str],
) -> Result<wgpu::ShaderModule, RenderError> {
    let defs: HashMap<String, naga_oil::compose::ShaderDefValue> = defs
        .iter()
        .map(|name| ((*name).to_string(), naga_oil::compose::ShaderDefValue::Bool(true)))
        .collect();
    let module = crate::shader_lib::compose_source(source, file, defs)?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
    }))
}

/// A filterable float colour texture binding.
pub(crate) fn color_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// A single-sample depth texture binding (loaded, not sampled).
pub(crate) fn depth_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Depth,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// A filtering sampler binding.
pub(crate) fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

/// A uniform buffer binding.
pub(crate) fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// A clamped bilinear sampler.
pub(crate) fn linear_sampler(device: &TrackedDevice, label: &str) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(label),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    })
}

/// A fullscreen-triangle pipeline (`vs_main`) writing `targets` lens-format colour targets.
pub(crate) fn fullscreen_pipeline(
    device: &TrackedDevice,
    label: &str,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    fragment: &str,
    targets: usize,
) -> wgpu::RenderPipeline {
    fullscreen_pipeline_formats(
        device,
        label,
        layout,
        module,
        fragment,
        &vec![LENS_FORMAT; targets],
    )
}

/// A fullscreen-triangle pipeline (`vs_main`) writing one colour target per format.
pub(crate) fn fullscreen_pipeline_formats(
    device: &TrackedDevice,
    label: &str,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    fragment: &str,
    formats: &[wgpu::TextureFormat],
) -> wgpu::RenderPipeline {
    let targets: Vec<Option<wgpu::ColorTargetState>> = formats
        .iter()
        .map(|&format| {
            Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })
        })
        .collect();
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fragment),
            targets: &targets,
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    })
}

/// A render pass over `targets`; every pixel is written, so the old contents are not loaded.
pub(crate) fn begin_fullscreen<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    label: &str,
    targets: &[&wgpu::TextureView],
) -> wgpu::RenderPass<'e> {
    let attachments: Vec<Option<wgpu::RenderPassColorAttachment<'_>>> = targets
        .iter()
        .map(|view| {
            Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })
        })
        .collect();
    encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &attachments,
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

/// The `from` versions of the `physical-camera` migration steps (`0`: no `format_version`).
#[must_use]
pub fn physical_camera_migration_sources() -> Vec<u32> {
    vec![0]
}

/// The serialized shape of [`PhysicalCamera`] for `cargo xtask check-formats` (entry
/// `physical-camera`): every reflected field (name and type) of [`PhysicalCamera`] and every
/// variant of [`CameraExposureMode`] and [`DepthOfFieldMode`], in declaration order.
#[must_use]
pub fn physical_camera_schema_fingerprint() -> String {
    let mut out = String::new();
    crate::post_volumes::struct_fields(&mut out, "PhysicalCamera", PhysicalCamera::type_info());
    crate::post_volumes::enum_variants(&mut out, "CameraExposureMode", CameraExposureMode::type_info());
    crate::post_volumes::enum_variants(&mut out, "DepthOfFieldMode", DepthOfFieldMode::type_info());
    out
}

/// Logs once per process that a lens from a newer engine was ignored.
pub(crate) fn warn_newer_lens_once(found: u32) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "regia_render",
            "a PhysicalCamera has format_version {found}, newer than this engine's \
             {PHYSICAL_CAMERA_FORMAT_VERSION}; the camera renders as if it had none (logged once)"
        );
    }
}

/// Registers the reflected lens types a scene file needs ([`PhysicalCamera`] and the enums it
/// holds); `RenderPlugin` does the same.
pub fn register_physical_camera_types(registry: &mut regia_ecs::reflect::TypeRegistry) {
    registry.register::<PhysicalCamera>();
    registry.register::<CameraExposureMode>();
    registry.register::<DepthOfFieldMode>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::RenderProfilePreset;

    #[test]
    fn a_default_lens_blurs_motion_but_keeps_focus() {
        let lens = PhysicalCamera::default();
        assert!(lens.motion_blur().is_some());
        assert!(lens.depth_of_field().is_none());
    }

    #[test]
    fn presets_gate_each_effect() {
        let lens = PhysicalCamera {
            depth_of_field: Some(DepthOfFieldMode::Gaussian),
            ..PhysicalCamera::default()
        };
        for preset in [
            RenderProfilePreset::DesktopHigh,
            RenderProfilePreset::DesktopUltra,
        ] {
            let mut post = PostSettings::default();
            lens.apply(&mut post, &RenderProfile::from_preset(preset));
            assert!(
                post.motion_blur.is_some() && post.depth_of_field.is_some(),
                "{preset:?}"
            );
        }
        for preset in [RenderProfilePreset::MobileLow, RenderProfilePreset::MobileHigh] {
            let mut post = PostSettings::default();
            lens.apply(&mut post, &RenderProfile::from_preset(preset));
            assert!(
                post.motion_blur.is_none() && post.depth_of_field.is_none(),
                "{preset:?}"
            );
        }
    }

    #[test]
    fn default_post_settings_want_no_lens_pass() {
        assert!(!LensPasses::wanted(&PostSettings::default()));
        assert!(!LensPasses::wanted(&PostSettings::golden_deterministic()));
        let post = PostSettings {
            motion_blur: Some(MotionBlur::default()),
            ..PostSettings::default()
        };
        assert!(LensPasses::wanted(&post));
    }
}
