// SPDX-License-Identifier: MPL-2.0
//! Device set-up shared by the hosts: the limits that hold everywhere the viewer runs, and the MSAA
//! choice.

use crate::ViewerError;

/// The limits a viewer device asks for: WebGL2's, with the adapter's texture size. Requesting them on
/// every backend means a storage buffer, a compute pass or an extra binding fails in native tests, not
/// only on a phone browser.
#[must_use]
pub fn viewer_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits())
}

/// Requests a device with [`viewer_limits`] and no optional features.
///
/// # Errors
/// [`ViewerError::Device`] when the adapter refuses.
pub async fn request_viewer_device(
    adapter: &wgpu::Adapter,
) -> Result<(wgpu::Device, wgpu::Queue), ViewerError> {
    request_device_with(adapter, wgpu::Features::empty()).await
}

/// [`request_viewer_device`], plus `PIPELINE_CACHE` where the adapter has it, for a host that keeps compiled
/// pipelines between runs (`ViewerConfig::pipeline_cache`).
///
/// # Errors
/// [`ViewerError::Device`] when the adapter refuses.
pub async fn request_viewer_device_with_cache(
    adapter: &wgpu::Adapter,
) -> Result<(wgpu::Device, wgpu::Queue), ViewerError> {
    request_device_with(adapter, adapter.features() & wgpu::Features::PIPELINE_CACHE).await
}

async fn request_device_with(
    adapter: &wgpu::Adapter,
    features: wgpu::Features,
) -> Result<(wgpu::Device, wgpu::Queue), ViewerError> {
    adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("regia viewer"),
            required_features: features,
            required_limits: viewer_limits(adapter),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
        })
        .await
        .map_err(|e| ViewerError::Device(e.to_string()))
}

/// 4 when the adapter can multisample this format and depth at 4x, else 1.
#[must_use]
pub fn sample_count_for(adapter: &wgpu::Adapter, format: wgpu::TextureFormat) -> u32 {
    let color = adapter.get_texture_format_features(format).flags;
    if color.sample_count_supported(4) && color.contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_RESOLVE)
    {
        4
    } else {
        1
    }
}

/// The surface format to render to from a surface's list: an sRGB one when there is one, because the
/// hardware then encodes after the MSAA resolve; otherwise the first, and the shader encodes.
#[must_use]
pub fn preferred_surface_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    formats
        .iter()
        .find(|f| f.is_srgb())
        .or_else(|| formats.first())
        .copied()
}

/// Drives a future to completion on the calling thread. Native only: wgpu's device requests and a mapped
/// buffer after a blocking poll resolve without a reactor.
#[cfg(not(target_arch = "wasm32"))]
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

/// A headless device for tests, stills and the bench, on the native backends (or `WGPU_BACKEND`): the
/// lavapipe driver on the farm. GL is left out: it is the browser's path here, and a headless GL context
/// fails where Vulkan works.
///
/// # Errors
/// [`ViewerError::NoAdapter`] when the machine has no adapter, [`ViewerError::Device`] when one refuses.
#[cfg(not(target_arch = "wasm32"))]
pub fn headless_device() -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue), ViewerError> {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::from_env()
        .unwrap_or(wgpu::Backends::VULKAN | wgpu::Backends::METAL | wgpu::Backends::DX12);
    let instance = wgpu::Instance::new(descriptor);
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: None,
        apply_limit_buckets: false,
    }))
    .map_err(|e| ViewerError::NoAdapter(e.to_string()))?;
    let (device, queue) = block_on(request_viewer_device(&adapter))?;
    Ok((adapter, device, queue))
}
