// SPDX-License-Identifier: MPL-2.0
//! Which path the terrain's anisotropic layer taps take on an adapter (ADR-3957).
//!
//! The terrain samples its layer, triplanar and macro textures as texture ARRAYS through
//! `textureSampleGrad` with the footprint's two axes (ADR-192), and lets the sampler's
//! anisotropy (16x clamp) filter along the long one. Mesa's Intel Vulkan driver (ANV) applies
//! no anisotropy to explicit-gradient samples of array textures: a one-dispatch probe on an Arc
//! Pro B65 (Mesa 26.1.6, wgpu 30) read LOD 3 for an 8:1 footprint with the 16x sampler on a 2D
//! array and LOD 0 on a plain 2D texture (an RTX 5090 read LOD 0 for both), and
//! `terrain_aniso`'s far band kept no more detail at 16x than isotropic (lane
//! gpu-sweep-pass-v0216). On such a driver the shader assembles the anisotropic tap itself
//! (`layer_tap_footprint` in terrain.wgsl: as many trilinear taps along the long axis as the footprint's
//! ratio, up to the frame's anisotropy level).
//!
//! `REGIA_TERRAIN_ANISO_FOOTPRINT=1` forces that path on any adapter (to look at it on another
//! card), `=0` keeps the sampler path on ANV (to re-test a new driver).
//!
//! The sampler path's LOD offset (ADR-5459). With an anisotropic sampler bound, drivers remap the
//! trilinear blend between the two mips differently (the isotropic sampler is exact on all of them).
//! A one-dispatch probe (an isotropic 1.25-texel footprint, mip fraction 0.32, 16x sampler, 2D array)
//! read an effective LOD of 0.64 on an RTX 5090 (NVIDIA 615.71), 0.28 on lavapipe (exact, in 8-bit
//! steps) and 0.15 on the Steam Deck (RADV VANGOGH, Mesa 26.0); 1.1 texels (fraction 0.14) read 0.27,
//! 0.14 and 0.00. RADV narrows the blend toward the sharper mip, so its terrain kept more detail
//! than the NVIDIA look it was tuned on (far band 48.9 vs 42.2 without TAA) and under TAAU's jitter
//! changed 1.5x as much per unit of detail (`taau_motion_keeps_detail_without_shimmer` failed on the
//! Deck and on an RX 7900). On RADV the terrain scales the gradients it hands the anisotropic sampler
//! by 2^`RADV_ANISO_LOD_OFFSET`: a quarter mip puts the Deck's
//! far-band detail within 5 % of NVIDIA's and its TAAU shimmer per unit of detail 19 % under the
//! isotropic bar, at no extra taps. Every other adapter gets 0, so its output does not change.
//! `REGIA_TERRAIN_ANISO_LOD_OFFSET=<mips>` sets the offset on any adapter (0 turns it off on RADV).
//!
//! The offset is measured, not looked up (ADR-5797). A Pixel 9 Pro's Mali-G715 (ARM's own driver)
//! blends exactly as RADV does and an Apple M5 blends exactly, so the narrowed blend follows neither
//! a vendor nor a driver family. A terrain draw therefore starts from what the device's own 16x
//! sampler does to an isotropic footprint of a 2D array (`regia_render::material_aniso_calibration`'s
//! probe, once per adapter and process: [`anisotropy_lod_offset_for_device`]); the listed value
//! ([`anisotropy_lod_offset_for`]) is what a device without compute or storage buffers falls back to.
//!
//! Where the shader applies it (ADR-5943). The gradients of a projection are made once
//! (`aniso_grads` in terrain.wgsl) and then used by every tap of that projection (`layer_tap`: up to
//! three stochastic taps of each of a layer's four arrays, the triplanar sides, the macro pair).
//! `pixel_footprint` reads the offset from the frame uniform once per pixel into the private
//! `aniso_lod_offset`, and `aniso_grads` adds it to the exponent it already takes, so the gradients
//! come out 2^offset longer and the tap itself computes nothing for it. ADR-5459 first scaled them
//! in `layer_tap`. The driver's compiler inlines every tap site and derived that scale from the
//! uniform again at each one, and with the offset 0 it cost an RTX 5090 1.5 % of the terrain forward
//! pass (0.6 % even with anisotropy off, when no such tap runs). Derived from the uniform in
//! `aniso_grads` it still cost 0.4 %. `tests/layer_tap_cost.rs` holds the tap to its pre-ADR-5459
//! arithmetic and the uniform read to once per pixel.

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

/// PCI vendor id of Intel as Vulkan reports it.
const INTEL_VENDOR: u32 = 0x8086;

/// PCI vendor id of AMD as Vulkan reports it.
const AMD_VENDOR: u32 = 0x1002;

/// Vendor id of ARM as Vulkan reports it (Mali).
const ARM_VENDOR: u32 = 0x13b5;

/// The environment variable that sets the sampler path's LOD offset, in mips
/// ([`anisotropy_lod_offset_for`]).
pub const ANISO_LOD_OFFSET_ENV: &str = "REGIA_TERRAIN_ANISO_LOD_OFFSET";

/// The sampler path's LOD offset on Mesa's AMD Vulkan driver (RADV), in mips (ADR-5459).
pub const RADV_ANISO_LOD_OFFSET: f32 = 0.25;

/// The largest sampler-path LOD offset a host or the environment may set, in mips.
pub const MAX_ANISO_LOD_OFFSET: f32 = 2.0;

const _: () = assert!(RADV_ANISO_LOD_OFFSET > 0.0 && RADV_ANISO_LOD_OFFSET <= MAX_ANISO_LOD_OFFSET);

/// The sampler path's listed LOD offset on ARM's Mali Vulkan driver, in mips (ADR-5797): a
/// Mali-G715 (Pixel 9 Pro, driver r54p3) blends as RADV does.
pub const MALI_ANISO_LOD_OFFSET: f32 = 0.25;

const _: () = assert!(MALI_ANISO_LOD_OFFSET > 0.0 && MALI_ANISO_LOD_OFFSET <= MAX_ANISO_LOD_OFFSET);

/// Bits of the most recently configured terrain draw's sampler-path LOD offset (`f32::NAN` bits
/// before any).
static LAST_LOD_OFFSET: AtomicU32 = AtomicU32::new(0x7fc0_0000);

/// The environment variable that forces the footprint path on (`1`) or off (`0`).
pub const ANISO_FOOTPRINT_ENV: &str = "REGIA_TERRAIN_ANISO_FOOTPRINT";

/// 0 = no terrain draw created yet, 1 = the sampler path, 2 = the footprint path.
static LAST_PATH: AtomicU8 = AtomicU8::new(0);

/// The terrain's anisotropic tap path ([`anisotropy_path`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnisotropyPath {
    /// `textureSampleGrad` through the anisotropic sampler (the hardware filters).
    Sampler,
    /// The shader spreads up to the frame's anisotropy level of trilinear taps along the footprint's long axis.
    Footprint,
}

impl AnisotropyPath {
    /// Stable lowercase name (`sampler`, `footprint`), as the agent protocol reports it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Sampler => "sampler",
            Self::Footprint => "footprint",
        }
    }
}

/// Whether an adapter takes the footprint path. `vendor` is the PCI vendor id, `vulkan` whether
/// the device runs on the Vulkan backend, `driver` and `driver_info` wgpu's `AdapterInfo` strings
/// (Mesa names itself in one of them), `env` the value of [`ANISO_FOOTPRINT_ENV`] (`1`/`true` on,
/// `0`/`false` off, anything else ignored). Only Mesa's Intel Vulkan driver is known to drop
/// the anisotropy; Intel's Windows driver and every other adapter keep the sampler path until
/// a probe shows otherwise (`terrain_aniso`'s `the_sampler_path_matches_the_driver`).
#[must_use]
pub fn anisotropy_footprint_for(
    vendor: u32,
    vulkan: bool,
    driver: &str,
    driver_info: &str,
    env: Option<&str>,
) -> bool {
    match env.map(str::trim) {
        Some("1" | "true") => return true,
        Some("0" | "false") => return false,
        _ => {}
    }
    let mesa = |s: &str| s.to_ascii_lowercase().contains("mesa");
    vendor == INTEL_VENDOR && vulkan && (mesa(driver) || mesa(driver_info))
}

/// The automatic choice for a device; logs it. Publishing the choice to the process-global
/// report ([`record`]) is the caller's job: the pipeline-warm job's throwaway draw must not
/// overwrite a real terrain's report.
pub(crate) fn footprint_for_device(device: &regia_render::RenderDevice) -> bool {
    let info = device.adapter.get_info();
    let env = std::env::var(ANISO_FOOTPRINT_ENV).ok();
    let on = anisotropy_footprint_for(
        info.vendor,
        info.backend == wgpu::Backend::Vulkan,
        &info.driver,
        &info.driver_info,
        env.as_deref(),
    );
    if on {
        tracing::info!(
            target: "regia_terrain",
            "terrain anisotropy: footprint path on {} ({} {}; ADR-3957)",
            info.name,
            info.driver,
            info.driver_info
        );
    }
    on
}

/// The automatic sampler-path LOD offset for a device, in mips (ADR-5459): how much blurrier than the
/// footprint's own LOD the terrain asks the anisotropic sampler to filter. `vendor`, `vulkan`,
/// `driver`, `driver_info` as in [`anisotropy_footprint_for`]; `env` the value of
/// [`ANISO_LOD_OFFSET_ENV`] (a number of mips, clamped to 0..=[`MAX_ANISO_LOD_OFFSET`]; anything
/// else is ignored). Mesa's AMD Vulkan driver (RADV) gets [`RADV_ANISO_LOD_OFFSET`], ARM's Mali
/// Vulkan driver [`MALI_ANISO_LOD_OFFSET`], every other adapter 0 (measured on NVIDIA, Mesa ANV,
/// Apple and lavapipe: their anisotropic filter is at least as soft as the terrain was tuned on).
/// This is the listed value: a terrain draw uses it only where the device cannot be measured
/// ([`anisotropy_lod_offset_for_device`], ADR-5797).
#[must_use]
pub fn anisotropy_lod_offset_for(
    vendor: u32,
    vulkan: bool,
    driver: &str,
    driver_info: &str,
    env: Option<&str>,
) -> f32 {
    if let Some(v) = env
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
    {
        return v.clamp(0.0, MAX_ANISO_LOD_OFFSET);
    }
    let mesa = |s: &str| {
        let s = s.to_ascii_lowercase();
        s.contains("mesa") || s.contains("radv")
    };
    if vendor == AMD_VENDOR && vulkan && (mesa(driver) || mesa(driver_info)) {
        RADV_ANISO_LOD_OFFSET
    } else if vendor == ARM_VENDOR && vulkan {
        MALI_ANISO_LOD_OFFSET
    } else {
        0.0
    }
}

/// One adapter as [`MEASURED`] keys it: vendor, device, backend, driver, driver info.
type AdapterKey = (u32, u32, wgpu::Backend, String, String);

/// The measured sampler-path offset per adapter of this process (`None`: the probe could not run).
static MEASURED: std::sync::Mutex<Vec<(AdapterKey, Option<f32>)>> = std::sync::Mutex::new(Vec::new());

/// What the device's own 16x sampler does to an isotropic footprint of a 2D array, as a LOD offset
/// (ADR-5797): the calibration probe of `regia_render::material_aniso_calibration` with the
/// terrain's clamp, once per adapter and process. `None` when the probe cannot run there (no
/// compute or storage buffers, the web) or its isotropic control is not exact.
fn measured_lod_offset(device: &regia_render::RenderDevice, info: &wgpu::AdapterInfo) -> Option<f32> {
    let key: AdapterKey = (
        info.vendor,
        info.device,
        info.backend,
        info.driver.clone(),
        info.driver_info.clone(),
    );
    let mut cache = MEASURED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, measured)) = cache.iter().find(|(k, _)| *k == key) {
        return *measured;
    }
    let measured = regia_render::material_aniso_calibration::measured_anisotropy_lod_offset(
        &device.device,
        &device.queue,
        u16::from(regia_render::TerrainBudget::MAX_ANISOTROPY),
        regia_render::material_aniso_calibration::ProbeTexture::D2Array,
    );
    cache.push((key, measured));
    measured
}

/// The sampler path's LOD offset on a device, in mips: [`ANISO_LOD_OFFSET_ENV`] when it holds a
/// number; else what the device's own 16x sampler measures (ADR-5797: the largest amount by which it
/// filters an isotropic footprint of a 2D array sharper than exact trilinear, 0.25 on Mesa RADV and
/// on a Mali-G715, 0 on NVIDIA, Apple and lavapipe); else, where the probe cannot run, the listed
/// value ([`anisotropy_lod_offset_for`]). [`crate::gpu::TerrainGpu::new`] starts from it.
#[must_use]
pub fn anisotropy_lod_offset_for_device(device: &regia_render::RenderDevice) -> f32 {
    let info = device.adapter.get_info();
    let env = std::env::var(ANISO_LOD_OFFSET_ENV).ok();
    let listed = anisotropy_lod_offset_for(
        info.vendor,
        info.backend == wgpu::Backend::Vulkan,
        &info.driver,
        &info.driver_info,
        env.as_deref(),
    );
    let env_set = env
        .as_deref()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .is_some_and(f32::is_finite);
    if env_set {
        return listed;
    }
    measured_lod_offset(device, &info).unwrap_or(listed)
}

/// The automatic sampler-path LOD offset for the device a terrain draw is created on
/// ([`anisotropy_lod_offset_for_device`]); logs it. Publishing it to the process-global report
/// ([`record_lod_offset`]) is the caller's job: the pipeline-warm job's throwaway draw must not
/// overwrite a real terrain's report.
pub(crate) fn lod_offset_for_device(device: &regia_render::RenderDevice) -> f32 {
    let info = device.adapter.get_info();
    let offset = anisotropy_lod_offset_for_device(device);
    if offset > 0.0 {
        tracing::info!(
            target: "regia_terrain",
            "terrain anisotropy: sampler LOD offset {offset:.2} on {} ({} {}; ADR-5459, ADR-5797)",
            info.name,
            info.driver,
            info.driver_info
        );
    }
    offset
}

/// Records the sampler-path LOD offset the most recently configured or *published* terrain
/// draw uses. A draw publishes at creation ([`super::TerrainGpu`]) unless it is the
/// pipeline-warm job's throwaway terrain, and on every
/// [`super::TerrainGpu::set_anisotropy_lod_offset`] override.
pub(crate) fn record_lod_offset(offset: f32) {
    LAST_LOD_OFFSET.store(offset.to_bits(), Ordering::Relaxed);
}

/// The sampler-path LOD offset (mips) of the most recently created or configured terrain draw in
/// this process (`None` before any), as `terrain.get` reports it (`anisotropy_lod_offset`).
#[must_use]
pub fn anisotropy_lod_offset() -> Option<f32> {
    let v = f32::from_bits(LAST_LOD_OFFSET.load(Ordering::Relaxed));
    (!v.is_nan()).then_some(v)
}

/// Records the path the most recently configured or *published* terrain draw uses. A draw
/// publishes at creation ([`super::TerrainGpu`]) unless it is the pipeline-warm job's
/// throwaway terrain, and on every [`super::TerrainGpu::set_anisotropy_footprint`] override.
pub(crate) fn record(footprint: bool) {
    LAST_PATH.store(if footprint { 2 } else { 1 }, Ordering::Relaxed);
}

/// Publishes a newly created draw's automatic anisotropy settings to the process-global
/// report. `publish` is false for the pipeline-warm job's throwaway terrain
/// (`TerrainSharedDraw::new_for_warm`): it computes the same automatic choice but must not
/// overwrite a real terrain's report — or a host's
/// [`super::TerrainGpu::set_anisotropy_footprint`] override — depending on whether the warm
/// worker ran before or after the terrain was built.
pub(crate) fn record_created_draw(footprint: bool, lod_offset: f32, publish: bool) {
    if publish {
        record(footprint);
        record_lod_offset(lod_offset);
    }
}

/// The anisotropic tap path of the most recently created or configured terrain draw in this
/// process (`None` before any), as `terrain.get` reports it (`anisotropy_path`).
#[must_use]
pub fn anisotropy_path() -> Option<AnisotropyPath> {
    match LAST_PATH.load(Ordering::Relaxed) {
        1 => Some(AnisotropyPath::Sampler),
        2 => Some(AnisotropyPath::Footprint),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVIDIA: u32 = 0x10de;
    const AMD: u32 = 0x1002;

    #[test]
    fn mesa_intel_vulkan_takes_the_footprint_path() {
        // What wgpu reports for the Arc Pro B65 on the build server (Mesa 26.1.6).
        assert!(anisotropy_footprint_for(
            INTEL_VENDOR,
            true,
            "Intel open-source Mesa driver",
            "Mesa 26.1.6",
            None
        ));
        assert!(anisotropy_footprint_for(
            INTEL_VENDOR,
            true,
            "",
            "Mesa 25.0.1-arch1",
            None
        ));
    }

    #[test]
    fn other_drivers_keep_the_sampler_path() {
        // Intel's Windows Vulkan driver, Intel on DX12, and the other vendors (RADV is Mesa too).
        assert!(!anisotropy_footprint_for(
            INTEL_VENDOR,
            true,
            "Intel Corporation",
            "101.6130",
            None
        ));
        assert!(!anisotropy_footprint_for(
            INTEL_VENDOR,
            false,
            "Mesa",
            "Mesa 26.1.6",
            None
        ));
        assert!(!anisotropy_footprint_for(
            NVIDIA,
            true,
            "NVIDIA",
            "615.71.09",
            None
        ));
        assert!(!anisotropy_footprint_for(AMD, true, "radv", "Mesa 26.1.6", None));
    }

    #[test]
    fn the_environment_forces_either_path() {
        assert!(anisotropy_footprint_for(
            NVIDIA,
            true,
            "NVIDIA",
            "615.71.09",
            Some("1")
        ));
        assert!(anisotropy_footprint_for(
            NVIDIA,
            true,
            "NVIDIA",
            "615.71.09",
            Some(" true ")
        ));
        assert!(!anisotropy_footprint_for(
            INTEL_VENDOR,
            true,
            "Mesa",
            "Mesa 26.1.6",
            Some("0")
        ));
        assert!(!anisotropy_footprint_for(
            INTEL_VENDOR,
            true,
            "Mesa",
            "Mesa 26.1.6",
            Some("false")
        ));
        // Anything else is ignored.
        assert!(anisotropy_footprint_for(
            INTEL_VENDOR,
            true,
            "Mesa",
            "Mesa 26.1.6",
            Some("yes")
        ));
        assert!(!anisotropy_footprint_for(
            NVIDIA,
            true,
            "NVIDIA",
            "615.71.09",
            Some("")
        ));
    }

    #[test]
    fn radv_gets_the_sampler_lod_offset() {
        // What wgpu reports for the Steam Deck (RADV VANGOGH) and an RX 7900 XTX.
        let deck = anisotropy_lod_offset_for(AMD, true, "radv", "Mesa 26.0.0-devel (git-9cc9241790)", None);
        assert_eq!(deck, RADV_ANISO_LOD_OFFSET);
        let rdna3 = anisotropy_lod_offset_for(AMD, true, "radv", "Mesa 26.2.6-arch1.1", None);
        assert_eq!(rdna3, RADV_ANISO_LOD_OFFSET);
    }

    #[test]
    fn mali_gets_the_sampler_lod_offset() {
        // What wgpu reports for a Pixel 9 Pro (vendor 0x13b5, driver and name "Mali-G715").
        let pixel = anisotropy_lod_offset_for(
            ARM_VENDOR,
            true,
            "Mali-G715",
            "v1.r54p3-00eac0.03d8d836cbf5c9f29d765e58a6bdfb98",
            None,
        );
        assert_eq!(pixel, MALI_ANISO_LOD_OFFSET);
        // Vulkan only (a GLES device was not measured), and the environment still wins.
        assert_eq!(
            anisotropy_lod_offset_for(ARM_VENDOR, false, "Mali-G715", "", None),
            0.0
        );
        assert_eq!(
            anisotropy_lod_offset_for(ARM_VENDOR, true, "Mali-G715", "", Some("0")),
            0.0
        );
    }

    #[test]
    fn other_drivers_get_no_sampler_lod_offset() {
        // NVIDIA, Mesa ANV and lavapipe were measured; AMD's own drivers (AMDVLK, Windows) and
        // DX12 were not, so they keep the tuned behavior until a probe shows otherwise.
        for (vendor, vulkan, driver, info) in [
            (NVIDIA, true, "NVIDIA", "615.71.09"),
            (INTEL_VENDOR, true, "Intel open-source Mesa driver", "Mesa 26.1.6"),
            (0x10005, true, "llvmpipe", "Mesa 26.1.6-1~bpo13+1 (LLVM 19.1.7)"),
            (AMD, true, "AMD proprietary driver", "25.20.1"),
            (AMD, true, "AMD open-source driver", "2025.Q2.1 (LLPC)"),
            (AMD, false, "", "Mesa 26.2.6"),
        ] {
            assert_eq!(
                anisotropy_lod_offset_for(vendor, vulkan, driver, info, None),
                0.0,
                "{driver} {info}"
            );
        }
    }

    #[test]
    fn the_environment_sets_the_sampler_lod_offset() {
        assert_eq!(
            anisotropy_lod_offset_for(NVIDIA, true, "NVIDIA", "615.71.09", Some("0.25")),
            0.25
        );
        assert_eq!(
            anisotropy_lod_offset_for(AMD, true, "radv", "Mesa 26.0.0", Some(" 0 ")),
            0.0
        );
        // Clamped to 0..=MAX, and anything that is not a finite number is ignored.
        assert_eq!(
            anisotropy_lod_offset_for(AMD, true, "radv", "Mesa 26.0.0", Some("-1")),
            0.0
        );
        assert_eq!(
            anisotropy_lod_offset_for(NVIDIA, true, "NVIDIA", "615.71.09", Some("9")),
            MAX_ANISO_LOD_OFFSET
        );
        assert_eq!(
            anisotropy_lod_offset_for(AMD, true, "radv", "Mesa 26.0.0", Some("NaN")),
            RADV_ANISO_LOD_OFFSET
        );
        assert_eq!(
            anisotropy_lod_offset_for(AMD, true, "radv", "Mesa 26.0.0", Some("soft")),
            RADV_ANISO_LOD_OFFSET
        );
    }

    #[test]
    fn path_names_are_stable() {
        assert_eq!(AnisotropyPath::Sampler.name(), "sampler");
        assert_eq!(AnisotropyPath::Footprint.name(), "footprint");
    }

    #[test]
    fn the_warm_draws_automatic_choice_does_not_clobber_a_host_override() {
        // A real terrain publishes its automatic choice at creation; a host override then
        // moves the report. The pipeline-warm job's throwaway draw computes the same
        // automatic choice (its device is the same) but passes `publish = false`, so it must
        // leave the override in place — otherwise `terrain.get` would report the automatic
        // path whenever the warm worker happened to run after the override.
        record_created_draw(false, 0.0, true);
        assert_eq!(anisotropy_path(), Some(AnisotropyPath::Sampler));
        assert_eq!(anisotropy_lod_offset(), Some(0.0));
        record(true); // set_anisotropy_footprint(true)
        record_lod_offset(0.5);
        assert_eq!(anisotropy_path(), Some(AnisotropyPath::Footprint));
        assert_eq!(anisotropy_lod_offset(), Some(0.5));
        // The warm job's throwaway draw: same automatic choice, not published.
        record_created_draw(false, 0.0, false);
        assert_eq!(anisotropy_path(), Some(AnisotropyPath::Footprint));
        assert_eq!(anisotropy_lod_offset(), Some(0.5));
        // A real second terrain publishes its own choice again.
        record_created_draw(true, 0.25, true);
        assert_eq!(anisotropy_path(), Some(AnisotropyPath::Footprint));
        assert_eq!(anisotropy_lod_offset(), Some(0.25));
    }
}
