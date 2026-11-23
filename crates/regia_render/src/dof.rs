// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_post_process/src/dof/mod.rs. Copyright (c) Bevy contributors, dual-licensed
// MIT OR Apache-2.0; used here under the MIT licence (see docs/ported-sources.toml).
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
//! Depth of field (`shaders/dof.wgsl`): a thin-lens circle of confusion per pixel, then either a
//! separable Gaussian blur or the two-pass hexagonal bokeh of Colin Barré-Brisebois, "Hexagonal
//! Bokeh Blur Revisited" (2017).
//!
//! Set [`DepthOfField`] on a view's [`PostSettings`](crate::PostSettings) (a game sets it through
//! a camera's [`PhysicalCamera`](crate::PhysicalCamera)). The focal length follows from the
//! sensor height and the view's vertical field of view, so zooming the camera changes the depth
//! of field the way a zoom lens does.

use crate::RenderError;
use crate::device::TrackedDevice;
use crate::frame_work::counted::*;
use regia_app::prelude::*;

/// The appearance of the effect.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Default, Clone)]
#[type_path = "regia_render::dof"]
pub enum DepthOfFieldMode {
    /// A closer simulation of a lens, in which circles of confusion form hexagonal "spots" of
    /// light (bokeh). Two passes with two render targets in the first.
    Bokeh,
    /// Out-of-focus areas are simply blurred. Cheaper in memory bandwidth and less like a real
    /// lens. The default.
    #[default]
    Gaussian,
}

/// Depth of field of one view, from a physical lens.
///
/// # Example
///
/// ```
/// use regia_render::{DepthOfField, DepthOfFieldMode};
///
/// let dof = DepthOfField {
///     focal_distance: 4.0,
///     aperture_f_stops: 2.8,
///     mode: DepthOfFieldMode::Bokeh,
///     ..DepthOfField::default()
/// };
/// assert!(dof.is_active());
/// // A 50 mm-equivalent lens on Super 35 at a 21° vertical field of view.
/// let f = regia_render::focal_length(dof.sensor_height, 21.0_f32.to_radians());
/// assert!((f - 0.0503).abs() < 0.001);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthOfField {
    /// The appearance of the effect.
    pub mode: DepthOfFieldMode,
    /// The distance in metres to the plane in focus.
    pub focal_distance: f32,
    /// The height of the image sensor, metres. The focal length is derived from it and the field
    /// of view. The default is 18.66 mm, the Super 35 film format.
    pub sensor_height: f32,
    /// The aperture in f-stops (f/1.0 is very shallow, f/16 nearly all in focus). With the focal
    /// length it sets how much objects out of focus blur.
    pub aperture_f_stops: f32,
    /// The largest circle of confusion diameter, pixels. Non-physical; it bounds the cost, and is
    /// itself capped at [`Self::MAX_CIRCLE_OF_CONFUSION_DIAMETER`] (the blur loops are as long as
    /// the radius: an unbounded value hung the GPU).
    pub max_circle_of_confusion_diameter: f32,
    /// Objects are never treated as farther than this, metres. The sky sits at the far plane;
    /// without a cap it blurs by the largest circle. Clamped to the far plane.
    pub max_depth: f32,
}

impl Default for DepthOfField {
    fn default() -> Self {
        Self {
            mode: DepthOfFieldMode::default(),
            focal_distance: 10.0,
            sensor_height: SUPER_35_SENSOR_HEIGHT,
            aperture_f_stops: 1.0,
            max_circle_of_confusion_diameter: 64.0,
            max_depth: f32::INFINITY,
        }
    }
}

/// Height of the Super 35 film format, metres.
pub const SUPER_35_SENSOR_HEIGHT: f32 = 0.01866;

/// The focal length, metres, of a lens that shows the vertical field of view `fov_y` (radians) on
/// a sensor `sensor_height` metres tall.
#[must_use]
pub fn focal_length(sensor_height: f32, fov_y: f32) -> f32 {
    0.5 * sensor_height / (0.5 * fov_y).tan()
}

/// The vertical field of view, radians, of a lens of `focal_length` metres on a sensor
/// `sensor_height` metres tall: `2 · atan(sensor_height / (2 · focal_length))`, the inverse of
/// [`focal_length`] (ADR-1464).
#[must_use]
pub fn vertical_fov_from_focal_length(sensor_height: f32, focal_length: f32) -> f32 {
    2.0 * (sensor_height / (2.0 * focal_length)).atan()
}

impl DepthOfField {
    /// The cap on [`Self::max_circle_of_confusion_diameter`], pixels.
    pub const MAX_CIRCLE_OF_CONFUSION_DIAMETER: f32 = 128.0;

    /// `true` when the pass can change pixels: a positive focus, aperture, sensor and blur cap.
    #[must_use]
    pub fn is_active(&self) -> bool {
        let positive = |v: f32| v > 0.0 && v.is_finite();
        positive(self.focal_distance)
            && positive(self.aperture_f_stops)
            && positive(self.sensor_height)
            && self.max_circle_of_confusion_diameter >= 1.0
            && self.max_depth > 0.0
    }

    /// Circle of confusion diameter, as a fraction of the image height, of a point at `depth`
    /// metres seen through a lens of `focal_length` metres (the thin-lens formula the shader
    /// evaluates, before the pixel clamp).
    #[must_use]
    pub fn circle_of_confusion(&self, focal_length: f32, depth: f32) -> f32 {
        let scale = coc_scale(self, focal_length);
        let depth = depth.min(self.max_depth).max(f32::EPSILON);
        scale * (depth - self.focal_distance).abs()
            / (depth * (self.focal_distance - focal_length).max(f32::EPSILON))
    }

    fn uniform(&self, fov_y: f32, near: f32, far: f32) -> DepthOfFieldUniform {
        let focal_length = focal_length(self.sensor_height, fov_y);
        let max_coc = self
            .max_circle_of_confusion_diameter
            .min(Self::MAX_CIRCLE_OF_CONFUSION_DIAMETER);
        DepthOfFieldUniform {
            focal_distance: self.focal_distance,
            focal_length,
            coc_scale_factor: coc_scale(self, focal_length),
            max_circle_of_confusion_diameter: max_coc,
            max_depth: self.max_depth.min(far).min(MAX_CARRIED_DEPTH),
            near,
            near_over_far: if far.is_finite() && far > 0.0 {
                near / far
            } else {
                0.0
            },
            tile_reach: tile_reach(max_coc) as f32,
        }
    }
}

/// `focal_length² / (sensor_height × aperture_f_stops)`.
fn coc_scale(dof: &DepthOfField, focal_length: f32) -> f32 {
    focal_length * focal_length / (dof.sensor_height * dof.aperture_f_stops)
}

/// The uniform of `shaders/dof.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct DepthOfFieldUniform {
    focal_distance: f32,
    focal_length: f32,
    coc_scale_factor: f32,
    /// Capped at [`DepthOfField::MAX_CIRCLE_OF_CONFUSION_DIAMETER`].
    max_circle_of_confusion_diameter: f32,
    max_depth: f32,
    near: f32,
    /// `near / far`, 0 for an infinite far plane (the shader's view depth is
    /// `near / (1 − raw (1 − near / far))`).
    near_over_far: f32,
    /// [`tile_reach`] of the capped largest circle.
    tile_reach: f32,
}

/// Output pixels per circle-of-confusion tile edge (`TILE` in `shaders/dof.wgsl`).
const TILE: u32 = 16;

/// Largest depth, metres, the first blur pass carries in an f16 alpha (`MAX_CARRIED_DEPTH` in
/// `shaders/dof.wgsl`).
const MAX_CARRIED_DEPTH: f32 = 60000.0;

/// Tiles the tile max is dilated by for a largest circle of `max_coc` pixels: its radius in tiles,
/// rounded up (a tap `k` tiles away can be `16 (k − 1) + 1` px from the pixel), at least 1.
fn tile_reach(max_coc: f32) -> u32 {
    ((0.5 * max_coc / TILE as f32).ceil() as u32).max(1)
}

/// Format of the tile target (one circle-of-confusion diameter per tile, pixels).
const TILE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R16Float;

/// Pipelines, layouts, sampler and uniform of the passes.
struct Gpu {
    /// The tile pass: bindings 0–4.
    tile_bgl: wgpu::BindGroupLayout,
    /// The blur passes: bindings 0–4 plus the tiles at 5.
    bgl: wgpu::BindGroupLayout,
    tile_max: wgpu::RenderPipeline,
    /// Spreads each tile's maximum over `tile_reach` tiles (blur layout, the raw tiles at 5).
    tile_dilate: wgpu::RenderPipeline,
    gaussian_horizontal: wgpu::RenderPipeline,
    gaussian_vertical: wgpu::RenderPipeline,
    bokeh_a: wgpu::RenderPipeline,
    bokeh_b: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
}

/// The tile targets of one output size: each tile's own largest circle, then that dilated by
/// `tile_reach` tiles (what the blur passes read).
struct Tiles {
    size: (u32, u32),
    _textures: [wgpu::Texture; 2],
    raw: wgpu::TextureView,
    dilated: wgpu::TextureView,
}

/// The depth of field passes. Created on the first frame that needs them.
#[derive(Default)]
pub(crate) struct DepthOfFieldPass {
    gpu: Option<Gpu>,
    tiles: Option<Tiles>,
    /// Groups keyed by colour texture A, colour texture B, the depth view and the tiles view
    /// (`None` for the tile pass's own group).
    bg_cache: crate::bind_cache::BindCache<(
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::TextureView,
        Option<wgpu::TextureView>,
    )>,
}

/// The tile grid of an output `width × height`.
fn tile_grid(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(TILE).max(1), height.div_ceil(TILE).max(1))
}

impl DepthOfFieldPass {
    fn ensure(&mut self, device: &TrackedDevice) -> Result<(), RenderError> {
        if self.gpu.is_some() {
            return Ok(());
        }
        let source = include_str!("../shaders/dof.wgsl");
        let tile_module = crate::lens::shader_module(device, "dof tiles", source, "dof.wgsl", &[])?;
        let module = crate::lens::shader_module(device, "depth of field", source, "dof.wgsl", &["DOF_BLUR"])?;
        let base = [
            crate::lens::uniform_entry(0),
            crate::lens::sampler_entry(1),
            crate::lens::depth_entry(2),
            crate::lens::color_entry(3),
            crate::lens::color_entry(4),
        ];
        let tile_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("dof tiles bgl"),
            entries: &base,
        });
        let mut entries = base.to_vec();
        entries.push(crate::lens::color_entry(5));
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("depth of field bgl"),
            entries: &entries,
        });
        let layout_of = |bgl: &wgpu::BindGroupLayout, label: &str| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(bgl)],
                immediate_size: 0,
            })
        };
        let tile_layout = layout_of(&tile_bgl, "dof tiles layout");
        let layout = layout_of(&bgl, "depth of field layout");
        let pipe = |label: &str, entry: &str, targets: usize| {
            crate::lens::fullscreen_pipeline(device, label, &layout, &module, entry, targets)
        };
        let gpu = Gpu {
            tile_max: crate::lens::fullscreen_pipeline_formats(
                device,
                "dof tile max",
                &tile_layout,
                &tile_module,
                "coc_tile_max",
                &[TILE_FORMAT],
            ),
            tile_dilate: crate::lens::fullscreen_pipeline_formats(
                device,
                "dof tile dilate",
                &layout,
                &module,
                "coc_tile_dilate",
                &[TILE_FORMAT],
            ),
            gaussian_horizontal: pipe("dof gaussian horizontal", "gaussian_horizontal", 1),
            gaussian_vertical: pipe("dof gaussian vertical", "gaussian_vertical", 1),
            bokeh_a: pipe("dof bokeh pass a", "bokeh_pass_a", 2),
            bokeh_b: pipe("dof bokeh pass b", "bokeh_pass_b", 1),
            tile_bgl,
            bgl,
            sampler: crate::lens::linear_sampler(device, "depth of field sampler"),
            params: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("depth of field params"),
                size: std::mem::size_of::<DepthOfFieldUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
        };
        self.gpu = Some(gpu);
        Ok(())
    }

    /// The (raw, dilated) tile views for an output `width × height`.
    fn tiles(
        &mut self,
        device: &TrackedDevice,
        width: u32,
        height: u32,
    ) -> (wgpu::TextureView, wgpu::TextureView) {
        let size = tile_grid(width, height);
        if let Some(tiles) = self.tiles.as_ref().filter(|t| t.size == size) {
            return (tiles.raw.clone(), tiles.dilated.clone());
        }
        let make = |label: &str| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TILE_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let textures = [make("dof coc tiles"), make("dof coc tiles dilated")];
        let raw = textures[0].create_view(&wgpu::TextureViewDescriptor::default());
        let dilated = textures[1].create_view(&wgpu::TextureViewDescriptor::default());
        self.bg_cache.clear();
        self.tiles = Some(Tiles {
            size,
            _textures: textures,
            raw: raw.clone(),
            dilated: dilated.clone(),
        });
        (raw, dilated)
    }

    /// Records the depth of field of `src` (`width × height`) into `dest`. `scratch` are two
    /// targets the size of `src` (the second is used by the bokeh mode only); `depth` is the scene
    /// depth at the render size. `projection` is the view's vertical field of view, near and far.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        device: &TrackedDevice,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        (width, height): (u32, u32),
        depth: &wgpu::TextureView,
        scratch: [&wgpu::TextureView; 2],
        dest: &wgpu::TextureView,
        settings: &DepthOfField,
        projection: (f32, f32, f32),
    ) -> Result<(), RenderError> {
        let _span = tracing::info_span!("depth_of_field").entered();
        let _crumb = crate::breadcrumbs::frame_pass("depth_of_field");
        self.ensure(device)?;
        let (raw_tiles, tiles) = self.tiles(device, width, height);
        let Some(gpu) = self.gpu.as_ref() else {
            return Ok(());
        };
        let (fov_y, near, far) = projection;
        let uniform = settings.uniform(fov_y, near, far);
        belt.write(device, encoder, &gpu.params, 0, bytemuck::bytes_of(&uniform));
        let cache = &mut self.bg_cache;
        let mut group = |a: &wgpu::TextureView, b: &wgpu::TextureView, tiles: Option<&wgpu::TextureView>| {
            cache.get_or_insert_with((a.clone(), b.clone(), depth.clone(), tiles.cloned()), || {
                let mut entries = vec![
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: gpu.params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(depth),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(a),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::TextureView(b),
                    },
                ];
                if let Some(tiles) = tiles {
                    entries.push(wgpu::BindGroupEntry {
                        binding: 5,
                        resource: wgpu::BindingResource::TextureView(tiles),
                    });
                }
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("depth of field bind group"),
                    layout: if tiles.is_some() { &gpu.bgl } else { &gpu.tile_bgl },
                    entries: &entries,
                })
            })
        };
        // The largest circle of confusion in each tile, dilated over the tiles it can reach, then
        // both modes are two passes: first into the scratch target(s), then into `dest`.
        let (first, first_targets, second, second_a, second_b) = match settings.mode {
            DepthOfFieldMode::Gaussian => (
                &gpu.gaussian_horizontal,
                &scratch[..1],
                &gpu.gaussian_vertical,
                scratch[0],
                scratch[0],
            ),
            DepthOfFieldMode::Bokeh => (&gpu.bokeh_a, &scratch[..], &gpu.bokeh_b, scratch[0], scratch[1]),
        };
        let tile_group = group(src, src, None);
        let dilate_group = group(src, src, Some(&raw_tiles));
        let first_group = group(src, src, Some(&tiles));
        let second_group = group(second_a, second_b, Some(&tiles));
        {
            let mut pass = crate::lens::begin_fullscreen(encoder, "depth of field tiles", &[&raw_tiles]);
            pass.counted_set_pipeline(&gpu.tile_max);
            pass.counted_set_bind_group(0, &tile_group, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        {
            let mut pass = crate::lens::begin_fullscreen(encoder, "depth of field tile dilate", &[&tiles]);
            pass.counted_set_pipeline(&gpu.tile_dilate);
            pass.counted_set_bind_group(0, &dilate_group, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        {
            let mut pass = crate::lens::begin_fullscreen(encoder, "depth of field pass 1", first_targets);
            pass.counted_set_pipeline(first);
            pass.counted_set_bind_group(0, &first_group, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        let mut pass = crate::lens::begin_fullscreen(encoder, "depth of field pass 2", &[dest]);
        pass.counted_set_pipeline(second);
        pass.counted_set_bind_group(0, &second_group, &[]);
        pass.counted_draw(0..3, 0..1);
        Ok(())
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "dof.wgsl",
        "DepthOfFieldParams",
        DepthOfFieldUniform {
            focal_distance,
            focal_length,
            coc_scale_factor,
            max_circle_of_confusion_diameter,
            max_depth,
            near,
            near_over_far,
            tile_reach
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focal_length_matches_the_pinhole_relation() {
        // A 90° vertical field of view: f = h / 2.
        let f = focal_length(0.024, std::f32::consts::FRAC_PI_2);
        assert!((f - 0.012).abs() < 1e-6, "{f}");
    }

    #[test]
    fn the_focal_plane_is_sharp_and_blur_grows_away_from_it() {
        let dof = DepthOfField {
            focal_distance: 5.0,
            aperture_f_stops: 2.0,
            ..DepthOfField::default()
        };
        let f = focal_length(dof.sensor_height, 0.6);
        assert!(dof.circle_of_confusion(f, 5.0) < 1e-6);
        let near = dof.circle_of_confusion(f, 2.0);
        let far = dof.circle_of_confusion(f, 20.0);
        assert!(near > dof.circle_of_confusion(f, 4.0));
        assert!(far > dof.circle_of_confusion(f, 8.0));
        // A wider aperture (smaller f-number) blurs more.
        let wide = DepthOfField {
            aperture_f_stops: 1.0,
            ..dof
        };
        assert!(wide.circle_of_confusion(f, 20.0) > far);
    }

    #[test]
    fn the_sky_is_capped_by_max_depth() {
        let dof = DepthOfField {
            focal_distance: 5.0,
            max_depth: 50.0,
            ..DepthOfField::default()
        };
        let f = focal_length(dof.sensor_height, 0.6);
        assert_eq!(
            dof.circle_of_confusion(f, 1.0e6),
            dof.circle_of_confusion(f, 50.0)
        );
        let uniform = dof.uniform(0.6, 0.1, 30.0);
        assert_eq!(uniform.max_depth, 30.0, "clamped to the far plane");
    }

    #[test]
    fn inactive_settings_are_detected() {
        assert!(DepthOfField::default().is_active());
        for broken in [
            DepthOfField {
                focal_distance: 0.0,
                ..DepthOfField::default()
            },
            DepthOfField {
                aperture_f_stops: -1.0,
                ..DepthOfField::default()
            },
            DepthOfField {
                max_circle_of_confusion_diameter: 0.0,
                ..DepthOfField::default()
            },
        ] {
            assert!(!broken.is_active(), "{broken:?}");
        }
    }

    #[test]
    fn tiles_cover_the_frame() {
        assert_eq!(tile_grid(3840, 2160), (240, 135));
        assert_eq!(tile_grid(481, 300), (31, 19));
        assert_eq!(tile_grid(0, 0), (1, 1));
    }

    #[test]
    fn uniform_is_thirty_two_bytes() {
        assert_eq!(std::mem::size_of::<DepthOfFieldUniform>(), 32);
    }

    #[test]
    fn the_largest_circle_is_capped() {
        for wild in [
            1.0e9,
            f32::INFINITY,
            DepthOfField::MAX_CIRCLE_OF_CONFUSION_DIAMETER + 1.0,
        ] {
            let dof = DepthOfField {
                max_circle_of_confusion_diameter: wild,
                ..DepthOfField::default()
            };
            assert!(dof.is_active());
            let u = dof.uniform(0.6, 0.1, 100.0);
            assert_eq!(
                u.max_circle_of_confusion_diameter,
                DepthOfField::MAX_CIRCLE_OF_CONFUSION_DIAMETER
            );
            assert_eq!(u.tile_reach, 4.0, "a 64 px radius spans 4 tiles");
        }
        let nan = DepthOfField {
            max_circle_of_confusion_diameter: f32::NAN,
            ..DepthOfField::default()
        };
        assert!(!nan.is_active(), "a NaN cap never reaches the GPU");
    }

    #[test]
    fn the_tile_max_reaches_the_largest_radius() {
        // One tile covered radii up to 16 px (circles up to 32 px); the default 64 px needs two.
        assert_eq!(tile_reach(1.0), 1);
        assert_eq!(tile_reach(32.0), 1);
        assert_eq!(tile_reach(33.0), 2);
        assert_eq!(tile_reach(64.0), 2);
        assert_eq!(
            tile_reach(DepthOfField::default().max_circle_of_confusion_diameter),
            2
        );
        assert_eq!(tile_reach(128.0), 4);
        // Every tap within the radius of any pixel of a tile lies in a tile within the reach.
        let tile = TILE as i32;
        for coc in 1..=128u32 {
            let reach = tile_reach(coc as f32) as i32;
            let radius = coc.div_ceil(2) as i32;
            for p in 10 * tile..11 * tile {
                for d in 0..=radius {
                    let off = 10 - (p - d).div_euclid(tile);
                    assert!(off <= reach, "{coc} px: a tap {d} px from {p} is {off} tiles off");
                }
            }
        }
    }

    #[test]
    fn an_infinite_far_plane_is_near_over_far_zero() {
        let dof = DepthOfField::default();
        let u = dof.uniform(0.6, 0.1, f32::INFINITY);
        assert_eq!(u.near_over_far, 0.0);
        assert_eq!(u.max_depth, MAX_CARRIED_DEPTH, "finite in the f16 carry");
        assert!(u.focal_length.is_finite() && u.coc_scale_factor.is_finite());
        let finite = dof.uniform(0.6, 0.1, 100.0);
        assert!((finite.near_over_far - 0.001).abs() < 1e-9);
        // The shader's view depth, n / (1 − raw (1 − n/f)), is n f / (f − raw (f − n)).
        let (n, f, raw) = (0.1f32, 100.0f32, 0.99f32);
        let shader = n / (1.0 - raw * (1.0 - finite.near_over_far));
        let reference = n * f / (f - raw * (f - n));
        assert!(
            (shader - reference).abs() / reference < 1e-4,
            "{shader} vs {reference}"
        );
    }

    #[test]
    fn the_shader_mirrors_the_constants() {
        let source = include_str!("../shaders/dof.wgsl");
        assert!(source.contains(&format!("const TILE: i32 = {TILE};")));
        assert!(source.contains(&format!("const MAX_CARRIED_DEPTH: f32 = {MAX_CARRIED_DEPTH:.1};")));
    }
}
