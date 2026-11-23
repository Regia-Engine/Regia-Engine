// SPDX-License-Identifier: MPL-2.0
//! Contact grounding under scatter: a camera-centred R8 map of soft occlusion discs, one per
//! instance footprint, that the terrain colour pass samples (`terrain.wgsl`, `contact_ao`).
//!
//! Each footprint splats `strength · (1 − t)²` with `t` running from half the contact radius to
//! the full radius, combined multiplicatively (`1 − Π(1 − aᵢ)`, so overlapping rocks deepen the
//! crease instead of saturating). The terrain shader darkens its sky/ambient term by the map
//! fully and the sun term partly, and tints the ground toward a dustier, darker version of
//! itself — the base of every rock sits in a soft pool of occlusion like the gap a real boulder
//! leaves (ambient-occlusion approximation, see `docs/provenance.md`). The window is snapped
//! to a grid so the discs never swim as the camera moves; it is rebuilt only when the snapped
//! origin or the field's toggles change.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::field::{ContactFootprint, ScatterField};

/// Texels per side of the contact map.
pub const CONTACT_MAP_RES: u32 = 512;
/// Largest `resolution` [`ContactRaster::build`] honors; larger requests return an empty raster.
pub const MAX_CONTACT_RES: u32 = 8192;
/// World extent of the contact window, metres (0.25 m texels).
pub const CONTACT_WINDOW_M: f32 = 128.0;
/// The window origin snaps to this grid, metres.
pub const CONTACT_SNAP_M: f32 = 16.0;

/// CPU side of the contact map: the occlusion texels of one window.
#[derive(Clone, Debug, PartialEq)]
pub struct ContactRaster {
    /// World X, Z of texel (0, 0)'s corner.
    pub origin: [f32; 2],
    /// Window size, metres.
    pub size_m: f32,
    /// Texels per side.
    pub resolution: u32,
    /// Row-major occlusion, 0 = open ground, 255 = fully occluded. Row 0 at −Z.
    pub texels: Vec<u8>,
}

impl ContactRaster {
    /// Splats every footprint of `field` inside the window whose origin is `origin`.
    #[must_use]
    pub fn build(field: &ScatterField, origin: [f32; 2], size_m: f32, resolution: u32) -> Self {
        let half = size_m * 0.5;
        let centre = [origin[0] + half, origin[1] + half];
        Self::splat(origin, size_m, resolution, |push| {
            field.footprints_in(centre, half + 4.0, push)
        })
    }

    /// Splats `footprints` (a host's own scatter: entities it placed, not a [`ScatterField`])
    /// inside the window whose origin is `origin`, in slice order, exactly as [`Self::build`]
    /// splats a field's (ADR-1408). Footprints more than 4 m outside the window are skipped.
    #[must_use]
    pub fn from_footprints(
        footprints: &[ContactFootprint],
        origin: [f32; 2],
        size_m: f32,
        resolution: u32,
    ) -> Self {
        let half = size_m * 0.5;
        let centre = [origin[0] + half, origin[1] + half];
        Self::splat(origin, size_m, resolution, |push| {
            for f in footprints {
                if (f.position[0] - centre[0]).abs() <= half + 4.0
                    && (f.position[2] - centre[1]).abs() <= half + 4.0
                {
                    push(*f);
                }
            }
        })
    }

    fn splat(
        origin: [f32; 2],
        size_m: f32,
        resolution: u32,
        visit: impl FnOnce(&mut dyn FnMut(ContactFootprint)),
    ) -> Self {
        let _span = tracing::info_span!("scatter_contact_raster").entered();
        if resolution > MAX_CONTACT_RES {
            // Past the cap the allocation could overflow `isize`; return an empty window.
            return Self {
                origin,
                size_m,
                resolution: 0,
                texels: Vec::new(),
            };
        }
        let n = resolution as usize;
        let texel = size_m / resolution as f32;
        // Footprints in emission order, as disc bounds in texels.
        let mut discs: Vec<(ContactFootprint, [f32; 3], [usize; 4])> = Vec::new();
        visit(&mut |f: ContactFootprint| {
            let r = f.radius.max(texel);
            let (cx, cz) = (
                (f.position[0] - origin[0]) / texel,
                (f.position[2] - origin[1]) / texel,
            );
            let rt = r / texel;
            let bounds = [
                (cx - rt).floor().max(0.0) as usize,
                ((cx + rt).ceil() as usize).min(n),
                (cz - rt).floor().max(0.0) as usize,
                ((cz + rt).ceil() as usize).min(n),
            ];
            if bounds[0] < bounds[1] && bounds[2] < bounds[3] {
                discs.push((f, [cx, cz, r], bounds));
            }
        });
        // Bands of rows in parallel; inside a band every texel multiplies its discs in the
        // same order as a serial pass, so the result does not depend on the split.
        const BAND_ROWS: usize = 32;
        let mut texels = vec![0u8; n * n];
        regia_core::tasks::par_chunks_mut(&mut texels, BAND_ROWS * n, |band, rows| {
            let z_start = band * BAND_ROWS;
            let z_end = z_start + rows.len() / n.max(1);
            // Accumulate transmittance so overlaps combine multiplicatively.
            let mut open = vec![1.0f32; rows.len()];
            let mut touched = false;
            for (f, [cx, cz, r], [x0, x1, z0, z1]) in &discs {
                let (zs, ze) = ((*z0).max(z_start), (*z1).min(z_end));
                if zs >= ze {
                    continue;
                }
                touched = true;
                let inner = 0.5 * r;
                for z in zs..ze {
                    for x in *x0..*x1 {
                        let dx = (x as f32 + 0.5 - cx) * texel;
                        let dz = (z as f32 + 0.5 - cz) * texel;
                        let d = (dx * dx + dz * dz).sqrt();
                        if d >= *r {
                            continue;
                        }
                        let t = ((d - inner) / (r - inner)).clamp(0.0, 1.0);
                        let a = f.strength * (1.0 - t) * (1.0 - t);
                        open[(z - z_start) * n + x] *= 1.0 - a.clamp(0.0, 1.0);
                    }
                }
            }
            if touched {
                for (out, o) in rows.iter_mut().zip(&open) {
                    *out = ((1.0 - o) * 255.0).round().clamp(0.0, 255.0) as u8;
                }
            }
        });
        Self {
            origin,
            size_m,
            resolution,
            texels,
        }
    }

    /// Occlusion (0..1) at world `(x, z)`, nearest texel; 0 outside the window.
    #[must_use]
    pub fn at(&self, x: f32, z: f32) -> f32 {
        let texel = self.size_m / self.resolution as f32;
        let (u, v) = ((x - self.origin[0]) / texel, (z - self.origin[1]) / texel);
        if u < 0.0 || v < 0.0 || u >= self.resolution as f32 || v >= self.resolution as f32 {
            return 0.0;
        }
        f32::from(self.texels[v as usize * self.resolution as usize + u as usize]) / 255.0
    }
}

/// The snapped window origin for an eye position.
#[must_use]
pub fn snapped_origin(eye: [f32; 3], size_m: f32, snap_m: f32) -> [f32; 2] {
    let half = size_m * 0.5;
    [
        ((eye[0] - half) / snap_m).round() * snap_m,
        ((eye[2] - half) / snap_m).round() * snap_m,
    ]
}

/// Uniform the terrain shader reads (`ContactParams` in `terrain.wgsl`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ContactUniform {
    /// x = window origin X (m), y = 1 / size (1/m), z = window origin Z (m), w = 1 when live.
    pub window: [f32; 4],
    /// x = ambient darkening, y = sun darkening, z = ground tint strength, w = unused.
    pub strength: [f32; 4],
}

/// GPU contact map: an `R8Unorm` texture plus its uniform, bound into the terrain draw with
/// [`crate::TerrainSharedDraw::set_contact_map`]. Call [`ContactMap::update`] once per frame
/// before the frame is rendered.
pub struct ContactMap {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    buffer: wgpu::Buffer,
    origin: Option<[f32; 2]>,
    field_state: Option<(bool, u64)>,
    size_m: f32,
    resolution: u32,
    raster: Option<ContactRaster>,
    enabled: bool,
    /// Rebuild counter, shared with the terrain draws the map is bound to: their
    /// `content_key` changes with it, so a held camera re-shades the ground.
    generation: Arc<AtomicU64>,
}

impl ContactMap {
    /// Creates an empty (disabled) map of [`CONTACT_MAP_RES`]² over [`CONTACT_WINDOW_M`].
    #[must_use]
    pub fn new(device: &wgpu::Device) -> Self {
        let resolution = CONTACT_MAP_RES;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scatter contact map"),
            size: wgpu::Extent3d {
                width: resolution,
                height: resolution,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scatter contact params"),
            size: size_of::<ContactUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            texture,
            view,
            buffer,
            origin: None,
            field_state: None,
            size_m: CONTACT_WINDOW_M,
            resolution,
            raster: None,
            enabled: true,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The texture view the terrain binds.
    #[must_use]
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The uniform buffer the terrain binds.
    #[must_use]
    pub fn buffer(&self) -> &wgpu::Buffer {
        &self.buffer
    }

    /// Turns the grounding term on or off (A/B evidence); the next update rewrites the uniform.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.field_state = None;
        }
    }

    /// Times the window was re-rasterised (a held camera rebuilds once).
    #[must_use]
    pub fn rebuilds(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// The rebuild counter the terrain draw folds into its content key.
    #[must_use]
    pub fn generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.generation)
    }

    /// The last raster (tests, agent inspection).
    #[must_use]
    pub fn raster(&self) -> Option<&ContactRaster> {
        self.raster.as_ref()
    }

    /// [`Self::update`] for a host's own footprint list (ADR-1408): re-centres on `eye` and
    /// re-rasterises when the snapped window or `key` (the host's count of edits to the list)
    /// changed.
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn update_footprints(
        &mut self,
        queue: &wgpu::Queue,
        footprints: &[ContactFootprint],
        key: u64,
        eye: [f32; 3],
    ) {
        let origin = snapped_origin(eye, self.size_m, CONTACT_SNAP_M);
        let state = (self.enabled, key);
        if self.origin == Some(origin) && self.field_state.as_ref() == Some(&state) {
            return;
        }
        let raster = if self.enabled {
            ContactRaster::from_footprints(footprints, origin, self.size_m, self.resolution)
        } else {
            self.empty_raster(origin)
        };
        self.upload(queue, raster, state);
    }

    fn empty_raster(&self, origin: [f32; 2]) -> ContactRaster {
        ContactRaster {
            origin,
            size_m: self.size_m,
            resolution: self.resolution,
            texels: vec![0; (self.resolution * self.resolution) as usize],
        }
    }

    /// Re-centres on `eye` and re-rasterises when the snapped window or the field's toggles
    /// changed, writing the texture and uniform through `queue` (staged into the next submit).
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn update(&mut self, queue: &wgpu::Queue, field: &ScatterField, eye: [f32; 3]) {
        let origin = snapped_origin(eye, self.size_m, CONTACT_SNAP_M);
        let state = (field.enabled() && self.enabled, field.state_generation());
        if self.origin == Some(origin) && self.field_state.as_ref() == Some(&state) {
            return;
        }
        let raster = if state.0 {
            ContactRaster::build(field, origin, self.size_m, self.resolution)
        } else {
            self.empty_raster(origin)
        };
        self.upload(queue, raster, state);
    }

    /// Writes `raster` and the window uniform, and records the window and `state`.
    fn upload(&mut self, queue: &wgpu::Queue, raster: ContactRaster, state: (bool, u64)) {
        let origin = raster.origin;
        let live = state.0;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &raster.texels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.resolution),
                rows_per_image: Some(self.resolution),
            },
            wgpu::Extent3d {
                width: self.resolution,
                height: self.resolution,
                depth_or_array_layers: 1,
            },
        );
        let uniform = ContactUniform {
            window: [
                origin[0],
                1.0 / self.size_m,
                origin[1],
                if live { 1.0 } else { 0.0 },
            ],
            strength: [1.0, 0.45, 0.35, 0.0],
        };
        queue.write_buffer(&self.buffer, 0, bytemuck::bytes_of(&uniform));
        self.origin = Some(origin);
        self.field_state = Some(state);
        self.raster = Some(raster);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::TerrainMap;

    /// A host's own footprint list (ADR-1408) splats like a field's: darkest at the core,
    /// open ground outside the disc, overlaps multiply, footprints past the window skipped.
    #[test]
    fn footprint_list_darkens_its_discs_only() {
        let near = ContactFootprint {
            position: [10.0, 0.0, 10.0],
            radius: 2.0,
            strength: 0.6,
        };
        let far = ContactFootprint {
            position: [500.0, 0.0, 500.0],
            radius: 2.0,
            strength: 1.0,
        };
        let one = ContactRaster::from_footprints(&[near, far], [0.0, 0.0], 64.0, 256);
        assert!((one.at(10.0, 10.0) - 0.6).abs() < 0.01, "{}", one.at(10.0, 10.0));
        assert_eq!(one.at(30.0, 30.0), 0.0);
        let skipped = ContactRaster::from_footprints(&[far], [0.0, 0.0], 64.0, 256);
        assert!(skipped.texels.iter().all(|t| *t == 0));
        let two = ContactRaster::from_footprints(&[near, near], [0.0, 0.0], 64.0, 256);
        assert!((two.at(10.0, 10.0) - 0.84).abs() < 0.01, "{}", two.at(10.0, 10.0));
        assert!(
            ContactRaster::from_footprints(&[near], [0.0, 0.0], 1.0, 3_037_000_500)
                .texels
                .is_empty()
        );
    }

    #[test]
    fn oversized_resolution_returns_empty_raster() {
        let raster = ContactRaster::build(&field(), [0.0, 0.0], 1.0, 3_037_000_500);
        assert_eq!(raster.resolution, 0);
        assert!(raster.texels.is_empty());
    }
    use crate::scatter::field::{ScatterLibrary, ScatterSource};
    use crate::scatter::rock::{RockStyle, procedural_rock};
    use crate::scatter::set::{ScatterClass, ScatterMeshRef, ScatterSet};

    fn field() -> ScatterField {
        let map = TerrainMap::from_heights(33, 33, vec![0.0; 33 * 33], 256.0, 1.0, 0.0).expect("map");
        let set = ScatterSet {
            classes: vec![ScatterClass {
                meshes: vec![ScatterMeshRef {
                    mesh: "r".into(),
                    weight: 1.0,
                    min_scale: 1.0,
                    max_scale: 1.0,
                    scale_bias: 1.0,
                }],
                cell_size: 8.0,
                density: 0.5,
                contact_ao: 0.6,
                ..ScatterClass::default()
            }],
            ..ScatterSet::default()
        };
        let mut lib = ScatterLibrary::new();
        lib.insert("r".into(), procedural_rock(1, RockStyle::rounded([0.5; 3])));
        let mut field =
            ScatterField::new(&set, ScatterSource::new(std::sync::Arc::new(map)), &lib, 9).expect("field");
        field.update([0.0, 0.0, 0.0]);
        field
    }

    #[test]
    fn discs_are_dark_at_the_rock_and_open_between() {
        let f = field();
        let origin = snapped_origin([0.0, 0.0, 0.0], 64.0, 16.0);
        let raster = ContactRaster::build(&f, origin, 64.0, 256);
        let mut at_rocks = Vec::new();
        f.footprints_in([origin[0] + 32.0, origin[1] + 32.0], 30.0, |fp| {
            at_rocks.push(raster.at(fp.position[0], fp.position[2]));
        });
        assert!(at_rocks.len() > 10);
        let mean = at_rocks.iter().sum::<f32>() / at_rocks.len() as f32;
        assert!(mean > 0.4, "mean occlusion at rock bases {mean}");
        let open = raster.texels.iter().filter(|&&t| t == 0).count();
        assert!(open * 2 > raster.texels.len(), "most ground is open");
        // Soft edge: some texels are partially occluded.
        assert!(raster.texels.iter().any(|&t| t > 10 && t < 120));
    }

    #[test]
    fn origin_snaps() {
        assert_eq!(snapped_origin([3.0, 0.0, -5.0], 128.0, 16.0), [-64.0, -64.0]);
        assert_eq!(snapped_origin([9.0, 0.0, -5.0], 128.0, 16.0), [-48.0, -64.0]);
    }
}
