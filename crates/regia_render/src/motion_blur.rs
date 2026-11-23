// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_post_process/src/motion_blur/{mod,pipeline}.rs. Copyright (c) Bevy contributors,
// dual-licensed MIT OR Apache-2.0; used here under the MIT licence (see docs/ported-sources.toml).
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
//! Per-object, per-pixel motion blur along the velocity target (`shaders/motion_blur.wgsl`).
//!
//! Set [`MotionBlur`] on a view's [`PostSettings`](crate::PostSettings) (a game sets it through a
//! camera's [`PhysicalCamera`](crate::PhysicalCamera)) to blur each pixel along its motion over the
//! time the shutter is open.
//!
//! Rendering takes discrete steps in time, so per-pixel motion vectors estimate each object's path
//! between frames. This kind of implementation has known artifacts:
//! - fast moving objects in front of a stationary object, or of empty space, do not have their
//!   edges blurred;
//! - translucent layers do not write velocity, so they are not blurred.
//!
//! Reconstruction filters (McGuire et al., "A Reconstruction Filter for Plausible Motion Blur",
//! I3D 2012) are more correct but more expensive; this gather is cheap and effective.
//!
//! The pass reads the velocity target the temporal resolve writes, so it runs on frames with TAA
//! (every desktop and Deck preset) and is skipped otherwise.
//!
//! At high velocity (ADR-1152) the blur is capped at [`MotionBlur::MAX_BLUR_FRACTION`] of the
//! frame height, and a long blur takes more samples (up to [`MotionBlur::MAX_SAMPLES`] per
//! direction). Below that cap the taps stay at most [`MotionBlur::SAMPLE_SPACING_PX`] apart.
//! Once the sample count sticks at the cap, consecutive taps are `height / 256` pixels apart
//! at the longest blur, which is more than 4 px when the frame is taller than 1024. A non-finite
//! motion vector reads as no motion and a non-finite colour tap is skipped.

use crate::RenderError;
use crate::device::TrackedDevice;
use crate::frame_work::counted::*;

/// Motion blur of one view.
///
/// # Example
///
/// ```
/// use regia_render::MotionBlur;
///
/// // A 180° shutter: objects blur over half the distance they move in a frame.
/// let blur = MotionBlur::default();
/// assert_eq!(blur.shutter_angle, 0.5);
/// assert!(blur.is_active());
/// assert!(!MotionBlur { samples: 0, ..blur }.is_active());
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionBlur {
    /// The strength of motion blur, the fraction of a frame the shutter is open: `0.5` is a 180°
    /// shutter (24 fps film is commonly 0.125–0.5).
    ///
    /// The blur is tied to the frame rate on purpose: above `1.0` an object's blur stretches
    /// farther than it travelled in the frame, which is non-physical (allowed for artistic
    /// reasons). To emulate 24 fps film at 60 fps, use `60 / 24 × 0.5 = 1.25` and accept the
    /// stretching.
    pub shutter_angle: f32,
    /// Samples per direction: the pass takes `2 × samples` taps per pixel, spread over the blur
    /// with a per-pixel jitter; `0` disables the blur. This is the least it takes: a blur longer
    /// than `8 × samples` pixels takes more, up to [`Self::MAX_SAMPLES`]. While the count is still
    /// under that cap the taps stay at most 4 px apart. At the cap the gap is `height / 256`
    /// pixels for a blur at [`Self::MAX_BLUR_FRACTION`] of the frame height.
    pub samples: u32,
}

impl Default for MotionBlur {
    fn default() -> Self {
        Self {
            shutter_angle: 0.5,
            samples: 1,
        }
    }
}

impl MotionBlur {
    /// Largest [`Self::samples`] the pass takes (the shader loop is `2 × samples` long).
    pub const MAX_SAMPLES: u32 = 16;

    /// Longest blur, the whole exposure vector, as a fraction of the frame height (135 px at
    /// 1080p). A faster pixel blurs this far along its motion: a teleported object, a hitch frame
    /// or a spinning prop otherwise sampled pixels across the frame, and an object faster than the
    /// frame vanished.
    pub const MAX_BLUR_FRACTION: f32 = 0.125;

    /// Widest gap, pixels, between the taps of a long blur: a blur longer than
    /// `2 × samples × SAMPLE_SPACING_PX` takes more samples, up to [`Self::MAX_SAMPLES`]
    /// (`SAMPLE_SPACING` in `shaders/motion_blur.wgsl`).
    pub const SAMPLE_SPACING_PX: f32 = 4.0;

    /// A shutter angle in degrees (180° is `0.5`).
    #[must_use]
    pub fn from_shutter_degrees(degrees: f32, samples: u32) -> Self {
        Self {
            shutter_angle: degrees / 360.0,
            samples,
        }
    }

    /// `true` when the pass changes pixels: a positive, finite shutter and at least one sample.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.samples > 0 && self.shutter_angle > 0.0 && self.shutter_angle.is_finite()
    }
}

/// The uniform of `shaders/motion_blur.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MotionBlurUniform {
    shutter_angle: f32,
    samples: u32,
    frame: u32,
    /// [`MotionBlur::MAX_BLUR_FRACTION`].
    max_blur: f32,
}

/// Pipeline, layout, sampler and uniform of the pass.
struct Gpu {
    bgl: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
}

/// The motion blur pass. Created on the first frame that needs it.
#[derive(Default)]
pub(crate) struct MotionBlurPass {
    gpu: Option<Gpu>,
    /// Groups keyed by the source, velocity and depth views.
    bg_cache: crate::bind_cache::BindCache<(wgpu::TextureView, wgpu::TextureView, wgpu::TextureView)>,
}

impl MotionBlurPass {
    fn ensure(&mut self, device: &TrackedDevice) -> Result<(), RenderError> {
        if self.gpu.is_none() {
            let module = crate::lens::shader_module(
                device,
                "motion blur",
                include_str!("../shaders/motion_blur.wgsl"),
                "motion_blur.wgsl",
                &[],
            )?;
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("motion blur bgl"),
                entries: &[
                    crate::lens::color_entry(0),
                    crate::lens::color_entry(1),
                    crate::lens::depth_entry(2),
                    crate::lens::sampler_entry(3),
                    crate::lens::uniform_entry(4),
                ],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("motion blur layout"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            let pipeline =
                crate::lens::fullscreen_pipeline(device, "motion blur", &layout, &module, "fs_main", 1);
            let params = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("motion blur params"),
                size: std::mem::size_of::<MotionBlurUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.gpu = Some(Gpu {
                bgl,
                pipeline,
                sampler: crate::lens::linear_sampler(device, "motion blur sampler"),
                params,
            });
        }
        Ok(())
    }

    /// Records the blur of `src` into `dest` (same size). `velocity` and `depth` are the scene's
    /// targets, at the render size (which may be below the colour's).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        device: &TrackedDevice,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        velocity: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        dest: &wgpu::TextureView,
        settings: &MotionBlur,
        frame: u64,
    ) -> Result<(), RenderError> {
        let _span = tracing::info_span!("motion_blur").entered();
        let _crumb = crate::breadcrumbs::frame_pass("motion_blur");
        self.ensure(device)?;
        let Some(gpu) = self.gpu.as_ref() else {
            return Ok(());
        };
        let uniform = MotionBlurUniform {
            shutter_angle: settings.shutter_angle,
            samples: settings.samples.min(MotionBlur::MAX_SAMPLES),
            frame: (frame % 64) as u32,
            max_blur: MotionBlur::MAX_BLUR_FRACTION,
        };
        belt.write(device, encoder, &gpu.params, 0, bytemuck::bytes_of(&uniform));
        let bind_group =
            self.bg_cache
                .get_or_insert_with((src.clone(), velocity.clone(), depth.clone()), || {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("motion blur bind group"),
                        layout: &gpu.bgl,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(src),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(velocity),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::TextureView(depth),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: gpu.params.as_entire_binding(),
                            },
                        ],
                    })
                });
        let mut pass = crate::lens::begin_fullscreen(encoder, "motion blur", &[dest]);
        pass.counted_set_pipeline(&gpu.pipeline);
        pass.counted_set_bind_group(0, &bind_group, &[]);
        pass.counted_draw(0..3, 0..1);
        Ok(())
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "motion_blur.wgsl",
        "MotionBlurParams",
        MotionBlurUniform {
            shutter_angle,
            samples,
            frame,
            max_blur
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degrees_map_to_the_shutter_fraction() {
        let blur = MotionBlur::from_shutter_degrees(180.0, 2);
        assert!((blur.shutter_angle - 0.5).abs() < 1e-6);
        assert_eq!(blur.samples, 2);
        assert!((MotionBlur::from_shutter_degrees(45.0, 1).shutter_angle - 0.125).abs() < 1e-6);
    }

    #[test]
    fn zero_samples_or_shutter_is_inactive() {
        assert!(MotionBlur::default().is_active());
        assert!(
            !MotionBlur {
                shutter_angle: 0.0,
                samples: 4
            }
            .is_active()
        );
        assert!(
            !MotionBlur {
                shutter_angle: f32::NAN,
                samples: 4
            }
            .is_active()
        );
        assert!(
            !MotionBlur {
                shutter_angle: 0.5,
                samples: 0
            }
            .is_active()
        );
    }

    #[test]
    fn uniform_is_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<MotionBlurUniform>(), 16);
    }

    #[test]
    fn the_shader_mirrors_the_sampling_constants() {
        let source = include_str!("../shaders/motion_blur.wgsl");
        assert!(
            source.contains(&format!("const MAX_SAMPLES: i32 = {};", MotionBlur::MAX_SAMPLES)),
            "MAX_SAMPLES"
        );
        assert!(
            source.contains(&format!(
                "const SAMPLE_SPACING: f32 = {:.1};",
                MotionBlur::SAMPLE_SPACING_PX
            )),
            "SAMPLE_SPACING"
        );
        // The longest blur at the cap still fits the sample budget at 1080p within the spacing.
        const {
            assert!(
                MotionBlur::MAX_BLUR_FRACTION * 1080.0 / (2.0 * MotionBlur::MAX_SAMPLES as f32)
                    <= 1.1 * MotionBlur::SAMPLE_SPACING_PX
            )
        };
    }

    /// At the sample cap, consecutive taps are `height / 256` pixels apart. That stays at
    /// 4 px only while the frame is at most 1024 tall.
    #[test]
    fn capped_tap_gap_is_height_over_256() {
        let gap =
            |height: f32| MotionBlur::MAX_BLUR_FRACTION * height / (2.0 * MotionBlur::MAX_SAMPLES as f32);
        assert!((gap(1024.0) - MotionBlur::SAMPLE_SPACING_PX).abs() < 1.0e-4);
        assert!(gap(1080.0) > MotionBlur::SAMPLE_SPACING_PX);
        assert!((gap(2160.0) - 2160.0 / 256.0).abs() < 1.0e-3);
        assert!(gap(2160.0) > MotionBlur::SAMPLE_SPACING_PX * 2.0);
    }
}
