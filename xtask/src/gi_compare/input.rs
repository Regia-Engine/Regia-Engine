// SPDX-License-Identifier: MPL-2.0
//! Checks at the comparison boundary; the pure metric functions retain their signatures.

use anyhow::{Result, anyhow};
use regia_pt::PtImage;

use super::{gpu::Capture, metrics};

pub(super) struct Prepared {
    pub ref_ind: Vec<[f32; 3]>,
    pub baked_ind: Vec<[f32; 3]>,
    pub ddgi_ind: Vec<[f32; 3]>,
}

pub(super) fn prepare(
    size: (u32, u32),
    baked: &[Capture],
    ddgi: &Capture,
    direct: &Capture,
    reference: &PtImage,
    emitter: Option<&PtImage>,
) -> Result<Prepared> {
    let shape = Shape::new(size)?;
    for (i, capture) in baked.iter().enumerate() {
        shape.plane(&format!("baked[{i}].rgb"), capture.rgb.len())?;
    }
    shape.plane("ddgi.rgb", ddgi.rgb.len())?;
    shape.plane("direct.rgb", direct.rgb.len())?;
    shape.reference("reference", reference)?;
    if let Some(image) = emitter {
        shape.dimensions("emitter", (image.width, image.height))?;
        // Only this AOV is consumed from the optional trace; its other planes are not saved.
        shape.plane("emitter.direct", image.direct.len())?;
    }
    let main = baked
        .first()
        .ok_or_else(|| anyhow!("[render_failed] no baked frame"))?;
    Ok(Prepared {
        ref_ind: metrics::reference_indirect(
            &reference.indirect_diffuse,
            &reference.indirect_specular,
            emitter.map(|img| (reference.direct.as_slice(), img.direct.as_slice())),
        ),
        baked_ind: metrics::sub_clamped(&main.rgb, &direct.rgb),
        ddgi_ind: metrics::sub_clamped(&ddgi.rgb, &direct.rgb),
    })
}

// Dimensions remain available here, before Capture deliberately drops the HDR size fields.
pub(super) fn check_capture(expected: (u32, u32), actual: (u32, u32), rgb: &metrics::Pixels) -> Result<()> {
    if actual != expected {
        // Keep the pre-existing capture diagnostic for hosts that inspect this error.
        return Err(anyhow!(
            "[render_failed] HDR capture is {}x{}, the frame {}x{}",
            actual.0,
            actual.1,
            expected.0,
            expected.1
        ));
    }
    Shape::new(expected)?.plane("HDR capture.rgb", rgb.len())
}

pub(super) fn check_score(
    size: (u32, u32),
    reference: &metrics::Pixels,
    test: &metrics::Pixels,
    depth: &[f32],
) -> Result<()> {
    let shape = Shape::new(size)?;
    shape.plane("score.reference", reference.len())?;
    shape.plane("score.test", test.len())?;
    shape.plane("score.depth", depth.len())
}

pub(super) fn checked_leak(
    size: (u32, u32),
    baked: &metrics::Pixels,
    reference: &metrics::Pixels,
    full: &metrics::Pixels,
    depth: &[f32],
    mask: &[bool],
) -> Result<Option<metrics::LeakStats>> {
    let shape = Shape::new(size)?;
    shape.plane("leak.baked_indirect", baked.len())?;
    shape.plane("leak.reference_indirect", reference.len())?;
    shape.plane("leak.reference_full", full.len())?;
    shape.plane("leak.depth", depth.len())?;
    shape.plane("leak.mask", mask.len())?;
    Ok(metrics::leak(baked, reference, full, depth, mask))
}

struct Shape {
    size: (u32, u32),
    len: usize,
}

impl Shape {
    fn new(size: (u32, u32)) -> Result<Self> {
        let (w, h) = size;
        let len = usize::try_from(w)
            .ok()
            .and_then(|w| usize::try_from(h).ok().and_then(|h| w.checked_mul(h)));
        let Some(len) = len.filter(|n| *n > 0) else {
            return Err(anyhow!(
                "[image_shape] requested image is {w}x{h}; dimensions must be nonzero and their product fit usize"
            ));
        };
        Ok(Self { size, len })
    }

    fn dimensions(&self, source: &str, actual: (u32, u32)) -> Result<()> {
        if actual != self.size {
            return Err(anyhow!(
                "[image_shape] {source} is {}x{}; expected {}x{}",
                actual.0,
                actual.1,
                self.size.0,
                self.size.1
            ));
        }
        Ok(())
    }

    fn plane(&self, name: &str, actual: usize) -> Result<()> {
        if actual != self.len {
            return Err(anyhow!(
                "[image_shape] {name} has {actual} elements; expected {} for {}x{}",
                self.len,
                self.size.0,
                self.size.1
            ));
        }
        Ok(())
    }

    fn reference(&self, source: &str, image: &PtImage) -> Result<()> {
        self.dimensions(source, (image.width, image.height))?;
        for (name, plane) in [
            ("beauty", &image.beauty),
            ("albedo", &image.albedo),
            ("normal", &image.normal),
            ("direct", &image.direct),
            ("indirect_diffuse", &image.indirect_diffuse),
            ("indirect_specular", &image.indirect_specular),
            ("emissive", &image.emissive),
            ("volumetric", &image.volumetric),
            ("sky", &image.sky),
            ("variance", &image.variance),
        ] {
            self.plane(&format!("{source}.{name}"), plane.len())?;
        }
        self.plane(&format!("{source}.depth"), image.depth.len())?;
        self.plane(&format!("{source}.shadow"), image.shadow.len())?;
        // PtImage explicitly uses an empty sample map for a uniform trace.
        if !image.spp_map.is_empty() {
            self.plane(&format!("{source}.spp_map"), image.spp_map.len())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
