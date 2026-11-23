// SPDX-License-Identifier: MPL-2.0
//! Every decision the renderer makes from the graphics backend, as pure functions (ADR-326).
//!
//! The renderer is feature-driven almost everywhere: timestamps, indirect counts, block
//! compression, early depth, storage limits and the HDR swapchain formats are read from what the
//! adapter and the surface report (`RenderCapabilities`, `output::select_surface_format`), so the
//! same code runs on every backend. The few choices that really depend on the backend (which
//! backends an instance opens, the adapter order, the persistent pipeline cache, the D3D12 binding
//! tier, the FXC shader rules) live here, so a new backend or a DX12 change is one file and one
//! table of tests instead of a search through the renderer.
//!
//! Each function takes the backend (or the backend set) and nothing else; none reads the
//! environment, the OS or a device.

/// The backends Regia Engine opens on `os` (spelled as [`std::env::consts::OS`]): Vulkan and DX12 on
/// Windows (Vulkan is preferred, [`default_pick_rank`]), Metal on macOS, Vulkan everywhere else
/// (Linux, Android). GL is never opened (`specs/13` § 2: no GL fallback; its EGL probe also
/// enumerates every DRM device at instance creation, which hangs on a dead GPU).
#[must_use]
pub fn instance_backends_for_os(os: &str) -> wgpu::Backends {
    match os {
        "windows" => wgpu::Backends::DX12 | wgpu::Backends::VULKAN,
        "macos" => wgpu::Backends::METAL,
        _ => wgpu::Backends::VULKAN,
    }
}

/// Whether the persistent pipeline cache (`pipeline_cache`, a `wgpu::PipelineCache` saved as
/// `.fpc`) applies on `backend`. wgpu 30 implements `PipelineCache` on Vulkan only; DX12 and Metal
/// rely on the driver's own disk cache, and GL is never opened.
#[must_use]
pub fn uses_pipeline_cache(backend: wgpu::Backend) -> bool {
    backend == wgpu::Backend::Vulkan
}

/// Sort key of `backend` in the adapter list (`list_adapters`, `REGIA_GPU_INDEX`): lower comes
/// first. Vulkan adapters keep the indices they have always had; DX12 adapters follow them on
/// Windows, so `REGIA_GPU_INDEX` or `REGIA_GPU_NAME` can force DX12 (ADR-201, ADR-296).
#[must_use]
pub fn listing_rank(backend: wgpu::Backend) -> u8 {
    match backend {
        wgpu::Backend::Vulkan => 0,
        wgpu::Backend::Dx12 => 1,
        wgpu::Backend::Metal => 2,
        wgpu::Backend::Gl => 3,
        _ => 4,
    }
}

/// Preference of `backend` in the engine's own default adapter pick (no `REGIA_GPU_*`
/// variable): lower wins, `None` means the pick never takes it. Vulkan first, DX12 only when Vulkan
/// has no hardware adapter (ADR-201). Metal and GL are not candidates: on macOS the instance opens
/// Metal alone and wgpu's `request_adapter` chooses ([`needs_own_default_pick`] is `false`).
#[must_use]
pub fn default_pick_rank(backend: wgpu::Backend) -> Option<u8> {
    match backend {
        wgpu::Backend::Vulkan => Some(0),
        wgpu::Backend::Dx12 => Some(1),
        _ => None,
    }
}

/// Whether an instance opening `backends` needs the engine's own default pick instead of wgpu's
/// `request_adapter`: only when Vulkan and DX12 are both open (Windows), because wgpu's
/// high-performance pick could otherwise land on a DX12 adapter while a Vulkan one works.
#[must_use]
pub fn needs_own_default_pick(backends: wgpu::Backends) -> bool {
    backends.contains(wgpu::Backends::VULKAN | wgpu::Backends::DX12)
}

/// Whether adapters on `backend` have a D3D12 Resource Binding Tier
/// ([`crate::ResourceBindingTier`]). DX12 only.
#[must_use]
pub fn has_resource_binding_tier(backend: wgpu::Backend) -> bool {
    backend == wgpu::Backend::Dx12
}

/// Whether FXC (Shader Model 5.1) can compile this backend's shaders. On DX12 wgpu uses DXC
/// (Shader Model 6) when the `static-dxc` feature is on or `dxcompiler.dll` is found, and FXC
/// otherwise; FXC must unroll a loop that contains a gradient operation, so the shaders keep the
/// fixed-trip loops ADR-201 wrote, and `shader_hlsl_check` writes every shader at Shader Model 5.1
/// as well as 6.0. No other backend goes through FXC.
#[must_use]
pub fn fxc_may_compile(backend: wgpu::Backend) -> bool {
    backend == wgpu::Backend::Dx12
}

/// Whether a render pass's `LoadOp::Clear` stores a large value exactly in an integer color
/// target. wgpu-hal's DX12 backend clears every color attachment with `ClearRenderTargetView`,
/// which takes `f32` values, so a `u32` clear above 2^24 reaches the driver rounded (`u32::MAX`
/// becomes 2^32, outside the format's range) and what the target holds is the driver's
/// conversion of it: the Intel N150 does not store `u32::MAX` (ADR-5807). Vulkan and Metal clear
/// integer formats with integer values. A pass that needs an exact sentinel on DX12 writes it
/// with a draw instead.
#[must_use]
pub fn clears_integer_targets_exactly(backend: wgpu::Backend) -> bool {
    backend != wgpu::Backend::Dx12
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::Backend::{Dx12, Gl, Metal, Vulkan};

    #[test]
    fn instance_backends_per_os_never_open_gl() {
        assert_eq!(
            instance_backends_for_os("windows"),
            wgpu::Backends::DX12 | wgpu::Backends::VULKAN
        );
        assert_eq!(instance_backends_for_os("macos"), wgpu::Backends::METAL);
        assert_eq!(instance_backends_for_os("linux"), wgpu::Backends::VULKAN);
        assert_eq!(instance_backends_for_os("android"), wgpu::Backends::VULKAN);
        for os in ["windows", "macos", "linux", "android", "freebsd"] {
            assert!(!instance_backends_for_os(os).contains(wgpu::Backends::GL), "{os}");
        }
        // The host's own answer is what `instance_backends` opens.
        assert_eq!(
            instance_backends_for_os(std::env::consts::OS),
            crate::instance_backends()
        );
    }

    #[test]
    fn the_pipeline_cache_is_vulkan_only() {
        assert!(uses_pipeline_cache(Vulkan));
        assert!(!uses_pipeline_cache(Dx12));
        assert!(!uses_pipeline_cache(Metal));
        assert!(!uses_pipeline_cache(Gl));
    }

    #[test]
    fn the_listing_puts_vulkan_first_then_dx12() {
        assert!(listing_rank(Vulkan) < listing_rank(Dx12));
        assert!(listing_rank(Dx12) < listing_rank(Metal));
        assert!(listing_rank(Metal) < listing_rank(Gl));
        assert!(listing_rank(Gl) < listing_rank(wgpu::Backend::Noop));
    }

    #[test]
    fn the_default_pick_prefers_vulkan_and_falls_back_to_dx12_only() {
        assert_eq!(default_pick_rank(Vulkan), Some(0));
        assert_eq!(default_pick_rank(Dx12), Some(1));
        assert_eq!(default_pick_rank(Metal), None);
        assert_eq!(default_pick_rank(Gl), None);
    }

    #[test]
    fn only_a_vulkan_plus_dx12_instance_needs_the_own_pick() {
        assert!(needs_own_default_pick(
            wgpu::Backends::VULKAN | wgpu::Backends::DX12
        ));
        assert!(!needs_own_default_pick(wgpu::Backends::VULKAN));
        assert!(!needs_own_default_pick(wgpu::Backends::DX12));
        assert!(!needs_own_default_pick(wgpu::Backends::METAL));
        assert!(!needs_own_default_pick(wgpu::Backends::GL));
        // Matches what each OS opens.
        assert!(needs_own_default_pick(instance_backends_for_os("windows")));
        assert!(!needs_own_default_pick(instance_backends_for_os("macos")));
        assert!(!needs_own_default_pick(instance_backends_for_os("linux")));
    }

    #[test]
    fn binding_tier_and_fxc_are_dx12_only() {
        assert!(has_resource_binding_tier(Dx12));
        assert!(fxc_may_compile(Dx12));
        for backend in [Vulkan, Metal, Gl] {
            assert!(!has_resource_binding_tier(backend), "{backend:?}");
            assert!(!fxc_may_compile(backend), "{backend:?}");
        }
    }

    /// ADR-5807: only DX12 clears integer targets through floats, and `u32::MAX` (the
    /// visibility buffer's empty ID) is not an `f32`, so the clear cannot hold it there.
    #[test]
    fn only_dx12_clears_integer_targets_through_floats() {
        assert!(!clears_integer_targets_exactly(Dx12));
        for backend in [Vulkan, Metal] {
            assert!(clears_integer_targets_exactly(backend), "{backend:?}");
        }
        // What wgpu-hal's DX12 clear hands the driver for the empty ID.
        assert_eq!(u32::MAX as f32 as u64, 1u64 << 32);
    }
}
