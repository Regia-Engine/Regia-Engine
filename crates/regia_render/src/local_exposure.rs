// SPDX-License-Identifier: MPL-2.0
//! Local exposure's bilateral grid ([`crate::LocalExposure`], `shaders/local_exposure.wgsl`).
//!
//! Two compute passes before the exposure pass: `cs_build` bins the exposed HDR image's log
//! luminance per pixel block (one workgroup per [`CELL_PX`]² block, 16 luminance bins), and
//! `cs_blur` smooths the homogeneous grid into a 3D `Rgba16Float` texture. The exposure pass
//! (`post.wgsl` mode 0) slices it with the hardware's trilinear filter. Everything is built on
//! first use and kept until the image size changes.

use crate::frame_work::counted::*;
use std::borrow::Cow;
use std::collections::HashMap;

use crate::RenderError;

/// Pixel block edge one grid cell covers.
pub(crate) const CELL_PX: u32 = 32;
/// Luminance bins (1 EV each, `local_exposure.wgsl`'s `BINS`).
pub(crate) const BINS: u32 = 16;
/// Log2 of the exposed luminance at the bottom of the first bin (`LOG_MIN`).
pub(crate) const LOG_MIN: f32 = -10.0;
/// Log2 span of the bins (`LOG_RANGE`).
pub(crate) const LOG_RANGE: f32 = 16.0;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LeParams {
    a: [f32; 4],
    wb: [f32; 4],
    dims: [u32; 4],
}

struct Grid {
    dims: [u32; 3],
    buffer: wgpu::Buffer,
    view: wgpu::TextureView,
    /// Bind group keyed by the source view it was built for.
    bind: Option<(wgpu::TextureView, wgpu::BindGroup)>,
}

/// The grid passes and their persistent resources.
#[derive(Default)]
pub(crate) struct LocalExposurePass {
    bgl: Option<wgpu::BindGroupLayout>,
    build: Option<wgpu::ComputePipeline>,
    blur: Option<wgpu::ComputePipeline>,
    params: Option<wgpu::Buffer>,
    grid: Option<Grid>,
    /// 1³ stand-in the exposure pass binds while local exposure is off.
    dummy: Option<wgpu::TextureView>,
}

impl LocalExposurePass {
    /// The grid view the exposure pass binds: this frame's grid after [`Self::record`], the
    /// 1³ stand-in otherwise.
    pub(crate) fn view(&mut self, device: &wgpu::Device, live: bool) -> wgpu::TextureView {
        if live && let Some(grid) = &self.grid {
            return grid.view.clone();
        }
        self.dummy
            .get_or_insert_with(|| {
                grid_texture(device, [1, 1, 1], "local exposure grid (off)")
                    .create_view(&wgpu::TextureViewDescriptor::default())
            })
            .clone()
    }

    /// The `local_grid` row of the post uniform for a `width` × `height` image: the UV scale
    /// from pixels to grid texture coordinates (x, y) and the luminance mapping (z = log2
    /// minimum, w = log2 span).
    pub(crate) fn grid_row(width: u32, height: u32) -> [f32; 4] {
        let gx = width.div_ceil(CELL_PX).max(1);
        let gy = height.div_ceil(CELL_PX).max(1);
        [
            1.0 / (gx * CELL_PX) as f32,
            1.0 / (gy * CELL_PX) as f32,
            LOG_MIN,
            LOG_RANGE,
        ]
    }

    /// Records the build and blur passes over `src` (`width` × `height`, the HDR image the
    /// exposure pass reads), exposed by `exposure` and white-balanced by `wb` as that pass does.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        width: u32,
        height: u32,
        exposure: f32,
        wb: [f32; 3],
    ) -> Result<(), RenderError> {
        let _span = tracing::info_span!("local_exposure_grid").entered();
        let _crumb = crate::breadcrumbs::frame_pass("local_exposure_grid");
        self.ensure_pipelines(device)?;
        let dims = [
            width.div_ceil(CELL_PX).max(1),
            height.div_ceil(CELL_PX).max(1),
            BINS,
        ];
        if self.grid.as_ref().is_none_or(|g| g.dims != dims) {
            let cells = u64::from(dims[0]) * u64::from(dims[1]) * u64::from(dims[2]);
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("local exposure grid (build)"),
                size: cells * 8,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            });
            let view = grid_texture(device, dims, "local exposure grid")
                .create_view(&wgpu::TextureViewDescriptor::default());
            self.grid = Some(Grid {
                dims,
                buffer,
                view,
                bind: None,
            });
        }
        let params_buf = self
            .params
            .get_or_insert_with(|| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("local exposure params"),
                    size: size_of::<LeParams>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            })
            .clone();
        let params = LeParams {
            a: [exposure, CELL_PX as f32, width as f32, height as f32],
            wb: [wb[0], wb[1], wb[2], 0.0],
            dims: [dims[0], dims[1], dims[2], 0],
        };
        belt.write(device, encoder, &params_buf, 0, bytemuck::bytes_of(&params));
        let (Some(bgl), Some(build), Some(blur), Some(grid)) =
            (&self.bgl, &self.build, &self.blur, self.grid.as_mut())
        else {
            return Err(RenderError::Shader("local exposure pipelines missing".into()));
        };
        if grid.bind.as_ref().is_none_or(|(view, _)| view != src) {
            let bind =
                crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("local exposure bind"),
                    layout: bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(src),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: params_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: grid.buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(&grid.view),
                        },
                    ],
                });
            grid.bind = Some((src.clone(), bind));
        }
        let Some((_, bind)) = grid.bind.as_ref() else {
            return Err(RenderError::Shader("local exposure bind group missing".into()));
        };
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("local exposure grid"),
            timestamp_writes: None,
        });
        pass.counted_set_bind_group(0, bind, &[]);
        pass.counted_set_pipeline(build);
        pass.counted_dispatch(dims[0], dims[1], 1);
        pass.counted_set_pipeline(blur);
        pass.counted_dispatch(dims[0].div_ceil(4), dims[1].div_ceil(4), dims[2].div_ceil(4));
        Ok(())
    }

    fn ensure_pipelines(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        if self.build.is_some() {
            return Ok(());
        }
        let module = crate::compose_source(
            include_str!("../shaders/local_exposure.wgsl"),
            "local_exposure.wgsl",
            HashMap::new(),
        )?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("local exposure"),
            source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
        });
        let compute = wgpu::ShaderStages::COMPUTE;
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("local exposure bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: compute,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: compute,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: compute,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: compute,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba16Float,
                        view_dimension: wgpu::TextureViewDimension::D3,
                    },
                    count: None,
                },
            ],
        });
        let device = &crate::device::TrackedDevice::observe(device);
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("local exposure layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(device).as_deref(),
            })
        };
        self.build = Some(pipeline("cs_build"));
        self.blur = Some(pipeline("cs_blur"));
        self.bgl = Some(bgl);
        Ok(())
    }
}

fn grid_texture(device: &wgpu::Device, dims: [u32; 3], label: &str) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: dims[0],
            height: dims[1],
            depth_or_array_layers: dims[2],
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING,
        view_formats: &[],
    })
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "local_exposure.wgsl",
        "LeParams",
        LeParams { a, wb, dims }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shader_constants_match() {
        let source = include_str!("../shaders/local_exposure.wgsl");
        assert!(source.contains(&format!("const BINS: u32 = {BINS}u;")));
        assert!(source.contains(&format!("const LOG_MIN: f32 = {LOG_MIN:.1};")));
        assert!(source.contains(&format!("const LOG_RANGE: f32 = {LOG_RANGE:.1};")));
    }

    #[test]
    fn the_grid_row_maps_pixels_to_cells() {
        let row = LocalExposurePass::grid_row(100, 40);
        // 4 × 2 cells of 32 px.
        assert_eq!(row[0], 1.0 / 128.0);
        assert_eq!(row[1], 1.0 / 64.0);
        assert_eq!(row[2], LOG_MIN);
        assert_eq!(row[3], LOG_RANGE);
    }
}
