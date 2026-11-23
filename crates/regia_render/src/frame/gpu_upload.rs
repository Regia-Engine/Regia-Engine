// SPDX-License-Identifier: MPL-2.0
//! Texture and lookup-table uploads used by the frame renderer: IBL, material textures, mip
//! chains, probe atlases and the LTC table (moved out of `frame.rs` unchanged, ADR-1552).

use super::*;

pub(super) fn build_ibl(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    maps: &crate::ibl::IblMaps,
) -> IblGpu {
    // RGBA16F: the atmosphere bake writes physical sky radiance over `SKY_IBL_SCALE`; at
    // dusk that is a few hundredths, which RGBA8 held in ~8 levels (banded ambient).
    let (irr_tex, irr) = upload_rgba16f(
        d,
        q,
        maps.irradiance.width,
        maps.irradiance.height,
        &maps.half.as_ref().map_or_else(
            || crate::ibl::rgba8_to_rgba16f(&maps.irradiance.rgba),
            |half| half.irradiance.clone(),
        ),
        maps.lower.as_ref(),
    );
    let (pref_tex, pref) = upload_rgba_mips(d, q, &maps.prefilter, maps.half.as_ref());
    let (_, brdf) = upload_rgba(d, q, maps.brdf.width, maps.brdf.height, &maps.brdf.rgba);
    let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("ibl"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        ..Default::default()
    });
    IblGpu {
        irradiance_tex: irr_tex,
        prefilter_tex: pref_tex,
        irradiance: irr,
        prefilter: pref,
        brdf,
        sampler,
        prefilter_mips: (maps.prefilter.len().saturating_sub(1)) as f32,
        plane_feed: maps.lower.as_ref().map(|lower| lower.plane_feed),
    }
}

pub(super) fn solid_rgba(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    rgba: [u8; 4],
) -> (wgpu::Texture, wgpu::TextureView) {
    upload_rgba(d, q, 1, 1, &rgba)
}

pub(super) fn upload_rgba(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> (wgpu::Texture, wgpu::TextureView) {
    upload_rgba_format(d, q, width, height, rgba, wgpu::TextureFormat::Rgba8Unorm)
}

/// A material texture with its full mip chain (box-filtered on the CPU; colour slots in
/// linear light). One level made every textured mesh shimmer and moiré once a texel was
/// smaller than a pixel: hero assets carry 4–8 k maps seen from tens of metres.
pub(crate) fn upload_material_texture(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    width: u32,
    height: u32,
    rgba: &[u8],
    srgb: bool,
) -> (wgpu::Texture, wgpu::TextureView) {
    upload_material_texture_views(d, q, width, height, rgba, srgb, false)
}

/// The other color-space variant of `format` (`Rgba8Unorm` ↔ `Rgba8UnormSrgb`, BC7 ↔ BC7 sRGB
/// ...), as a one-entry `view_formats` list when `siblings` and the format has one (ADR-8610).
pub(crate) fn sibling_view_format(
    format: wgpu::TextureFormat,
    siblings: bool,
) -> Option<wgpu::TextureFormat> {
    let other = if format.is_srgb() {
        format.remove_srgb_suffix()
    } else {
        format.add_srgb_suffix()
    };
    (siblings && other != format).then_some(other)
}

/// [`upload_material_texture`], the texture also viewable in the other color space when
/// `siblings` ([`sibling_view_format`]; a counted texture binds either, ADR-8610). The mip chain
/// is filtered in `srgb`'s space.
pub(crate) fn upload_material_texture_views(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    width: u32,
    height: u32,
    rgba: &[u8],
    srgb: bool,
    siblings: bool,
) -> (wgpu::Texture, wgpu::TextureView) {
    let (width, height) = (width.max(1), height.max(1));
    if crate::mips::rgba8_bytes(width, height).is_none_or(|n| rgba.len() < n) {
        return upload_rgba_format(
            d,
            q,
            width,
            height,
            rgba,
            if srgb {
                wgpu::TextureFormat::Rgba8UnormSrgb
            } else {
                wgpu::TextureFormat::Rgba8Unorm
            },
        );
    }
    let levels = crate::mips::mip_chain_rgba8(width, height, rgba, srgb);
    // Colour slots (sRGB-encoded bytes) take an sRGB format so sampling decodes them once.
    let format = if srgb {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let sibling = sibling_view_format(format, siblings);
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("material texture"),
        size: extent3d(width, height),
        mip_level_count: levels.len() as u32 + 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: sibling.as_slice(),
    });
    let write = |level: u32, w: u32, h: u32, bytes: &[u8]| {
        q.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            extent3d(w, h),
        );
    };
    write(
        0,
        width,
        height,
        &rgba[..crate::mips::rgba8_bytes(width, height).unwrap_or(rgba.len())],
    );
    for (i, (w, h, bytes)) in levels.iter().enumerate() {
        write(i as u32 + 1, *w, *h, bytes);
    }
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// Uploads a ready RGBA8 mip chain (level 0 first; each level half the previous), sRGB-viewed
/// for colour slots. `None` for an empty chain or a level shorter than its size.
pub(crate) fn upload_rgba8_chain(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    levels: &[crate::LabTexture],
    srgb: bool,
) -> Option<(wgpu::Texture, wgpu::TextureView)> {
    upload_rgba8_chain_views(d, q, levels, srgb, false)
}

/// [`upload_rgba8_chain`], the texture also viewable in the other color space when `siblings`
/// ([`sibling_view_format`], ADR-8610).
pub(crate) fn upload_rgba8_chain_views(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    levels: &[crate::LabTexture],
    srgb: bool,
    siblings: bool,
) -> Option<(wgpu::Texture, wgpu::TextureView)> {
    let top = levels.first()?;
    let (width, height) = (top.width.max(1), top.height.max(1));
    if levels.iter().enumerate().any(|(i, l)| {
        let (w, h) = ((width >> i).max(1), (height >> i).max(1));
        l.width != w || l.height != h || crate::mips::rgba8_bytes(w, h).is_none_or(|n| l.rgba.len() < n)
    }) {
        return None;
    }
    let format = if srgb {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let sibling = sibling_view_format(format, siblings);
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("material texture"),
        size: extent3d(width, height),
        mip_level_count: levels.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: sibling.as_slice(),
    });
    for (mip, level) in levels.iter().enumerate() {
        q.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: mip as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &level.rgba[..(level.width * level.height * 4) as usize],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(level.width * 4),
                rows_per_image: Some(level.height),
            },
            extent3d(level.width, level.height),
        );
    }
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Some((texture, view))
}

pub(super) fn upload_rgba_format(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    width: u32,
    height: u32,
    rgba: &[u8],
    format: wgpu::TextureFormat,
) -> (wgpu::Texture, wgpu::TextureView) {
    crate::frame_stats::note_texture(Some("frame texture"), u64::from(width.max(1) * height.max(1)) * 4);
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("frame texture"),
        size: extent3d(width.max(1), height.max(1)),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    q.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        extent3d(width.max(1), height.max(1)),
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// The emissive lights' distance-probe atlas: `R32Float`, room for every light the buffer
/// holds ([`crate::MAX_UPLOADED_LIGHTS`] tiles).
pub(super) fn create_probe_atlas(d: &wgpu::Device) -> (wgpu::Texture, wgpu::TextureView) {
    use crate::emissive_lights::{PROBE_ATLAS_COLUMNS, PROBE_RES};
    let width = PROBE_ATLAS_COLUMNS * PROBE_RES;
    let height = crate::MAX_UPLOADED_LIGHTS.div_ceil(PROBE_ATLAS_COLUMNS) * PROBE_RES;
    crate::frame_stats::note_texture(Some("emissive probes"), u64::from(width * height) * 4);
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("emissive probes"),
        size: extent3d(width, height),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// The GGX LTC table ([`crate::ltc`]) as an `Rgba32Float` texture, `LTC_SIZE²` texels.
pub(super) fn upload_ltc_table(d: &wgpu::Device, q: &crate::device::TrackedQueue) -> wgpu::TextureView {
    let size = crate::ltc::LTC_SIZE;
    let bytes = crate::ltc::ltc_texture_bytes();
    crate::frame_stats::note_texture(Some("ltc table"), bytes.len() as u64);
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("ltc table"),
        size: extent3d(size, size),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    q.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size * 16),
            rows_per_image: Some(size),
        },
        extent3d(size, size),
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

/// An `Rgba16Float` texture from 8-byte texels (see `ibl::rgb_to_rgba16f`). `lower` becomes its
/// mip 1 (`crate::ibl::IblLower`); every other read takes level 0.
pub(super) fn upload_rgba16f(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    width: u32,
    height: u32,
    bytes: &[u8],
    lower: Option<&crate::ibl::IblLower>,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("ibl irradiance"),
        size: extent3d(width.max(1), height.max(1)),
        mip_level_count: if lower.is_some() { 2 } else { 1 },
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let (padded, bytes_per_row) = crate::outdoor::pad_rows_bytes(bytes, width.max(1) * 8, height.max(1));
    q.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &padded,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(height.max(1)),
        },
        extent3d(width.max(1), height.max(1)),
    );
    if let Some(lower) = lower {
        let (padded, bytes_per_row) =
            crate::outdoor::pad_rows_bytes(&lower.half, lower.width * 8, lower.height);
        q.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 1,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &padded,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(lower.height),
            },
            extent3d(lower.width, lower.height),
        );
    }
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

pub(super) fn upload_rgba_mips(
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    mips: &[crate::ibl::IblImage],
    half: Option<&crate::ibl::IblHalf>,
) -> (wgpu::Texture, wgpu::TextureView) {
    let (width, height) = mips
        .first()
        .map(|image| (image.width, image.height))
        .unwrap_or((1, 1));
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("ibl prefilter"),
        size: extent3d(width.max(1), height.max(1)),
        mip_level_count: mips.len().max(1) as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (level, image) in mips.iter().enumerate() {
        let texels = match half.and_then(|h| h.prefilter.get(level)) {
            Some(bytes) => bytes.clone(),
            None => crate::ibl::rgba8_to_rgba16f(&image.rgba),
        };
        let (padded, bytes_per_row) =
            crate::outdoor::pad_rows_bytes(&texels, image.width.max(1) * 8, image.height.max(1));
        q.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &padded,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(image.height.max(1)),
            },
            extent3d(image.width.max(1), image.height.max(1)),
        );
    }
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}
