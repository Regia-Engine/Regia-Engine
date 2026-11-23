// SPDX-License-Identifier: MPL-2.0
//! GPU instance, adapter, device and queue.

use regia_app::prelude::*;

use crate::{RenderCapabilities, RenderError};

/// The process-wide lock held while the engine changes the Vulkan loader's instance, driver
/// and device lists: instance creation with its first adapter enumeration, and device creation.
///
/// The Vulkan loader (1.4.309, Debian 13's) changes those lists under one internal lock and
/// walks them under another (ADR-2113). Its first `vkEnumeratePhysicalDevices` on a new
/// instance unloads every driver that reports no device and frees its entry before unlinking
/// it; `vkCreateDevice` links a device before its dispatch pointer is set. A thread walking the
/// lists meanwhile (`vkGetDeviceProcAddr` while creating a device, and every
/// `vkSetDebugUtilsObjectNameEXT` of a debug build) read freed memory and died with SIGSEGV in
/// `loader_get_icd_and_device`. The loader fixed this upstream after 1.4.335 (one lock for both
/// sides); until every supported distro ships that, the engine's own creations take turns.
fn loader_list_lock() -> std::sync::MutexGuard<'static, ()> {
    // A poisoned lock only means another creation panicked; creating is still safe.
    let guard = LOADER_LISTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    LOADER_LIST_LOCKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    guard
}

/// The lock behind `loader_list_lock`.
static LOADER_LISTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How many times `loader_list_lock` was taken in this process (the regression tests of the
/// public wrappers count it).
static LOADER_LIST_LOCKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The first Vulkan instance this process created, kept where the loader would otherwise unload
/// the drivers with the last instance (`crate::platform::keeps_vulkan_drivers_loaded`, ADR-5794).
/// It is held until the clean exit has destroyed the device (`release_driver_pin`), or until the
/// process ends.
///
/// The Vulkan loader loads every installed driver library for a new instance and unloads a
/// library when the last instance using it is destroyed. It also loads and unloads them for each
/// of the three queries wgpu makes before it creates an instance, so a process that opens its
/// devices one after another reloads every driver four times per device (counted with
/// `VK_LOADER_DEBUG=all`: 12 manifest scans for 3 devices). glibc gives a library loaded after
/// start-up its static thread-local storage out of a fixed surplus, and every load and unload
/// of NVIDIA's `libnvidia-tls.so` leaves 16 bytes of it used. Measured on RTX 5090s (driver
/// 615.71.09, glibc 2.41) and an RTX 5080 (615.78.08, glibc 2.44): on the 27th device of a
/// process with only that driver installed, the 19th with Mesa's drivers beside it, the loader
/// logged `cannot allocate memory in static TLS block`, dropped the driver, and the device was
/// created on another adapter or not at all (the 35th and the 51st with
/// `glibc.rtld.optional_static_tls` raised from 512 to 1024 and 2048). While one instance lives
/// its drivers stay loaded, and a later instance only takes another reference to them.
static DRIVER_PIN: std::sync::Mutex<Option<wgpu::Instance>> = std::sync::Mutex::new(None);

/// Keeps `instance` as the process's driver pin if there is none (`DRIVER_PIN`). Called under
/// the loader-list lock, after the instance's first enumeration: by then the loader has unloaded
/// the drivers without a device, so the pin holds exactly the drivers that reported one.
fn pin_vulkan_drivers(instance: &wgpu::Instance) {
    if !crate::platform::keeps_vulkan_drivers_loaded() {
        return;
    }
    // A poisoned lock only means another creation panicked; the slot is still valid.
    let mut pin = DRIVER_PIN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if pin.is_none() {
        *pin = Some(instance.clone());
    }
}

/// Drops the process's driver pin (`DRIVER_PIN`), so the Vulkan loader unloads the drivers once
/// the last instance is gone. The clean exit calls it after it has destroyed the device and no
/// object of the device's instance is left: a quitting game then ends as it did before the pin,
/// with its drivers unloaded in the device step and none left for the process's exit handlers
/// (ADR-5794). An instance created later pins again. The shared headless instances
/// (`HEADLESS_INSTANCES`, ADR-7557) are let go of here too; a later headless device creates one
/// again.
///
/// Does nothing while another thread creates an instance or a device: that creation holds the
/// drivers itself, and an exit must not wait behind a driver call.
pub(crate) fn release_driver_pin() {
    let _guard = match LOADER_LISTS.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    // The shared headless instances go too (ADR-7557), or one of them would keep the drivers
    // loaded into the exit handlers. A creation holding the cache is creating an instance: as
    // above, the exit does not wait behind it.
    let shared = match HEADLESS_INSTANCES.try_lock() {
        Ok(mut cache) => std::mem::take(&mut *cache),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => std::mem::take(&mut *poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    let pinned = DRIVER_PIN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    // Destroys the instances whose last handle these were, under the loader-list lock like the
    // creation: the loader changes its instance and driver lists here too.
    drop(shared);
    drop(pinned);
}

/// How many instance and device creations have taken the engine's loader-list lock in this
/// process so far (ADR-2113). Diagnostic only: a test can check that its creations went
/// through [`create_window_instance`] and [`request_device_locked`].
#[must_use]
pub fn loader_list_lock_count() -> u64 {
    LOADER_LIST_LOCKS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Creates a [`wgpu::Instance`] for a window the caller owns (the editor's viewport, a tool's own
/// window) the way the engine creates its own: one creation at a time under the process-wide
/// loader-list lock, with the first Vulkan adapter enumeration inside the lock (ADR-2113).
///
/// A host that creates its own instance with `wgpu::Instance::new` while an engine device is
/// being created on another thread (an asset preview, a bake, a thumbnail renderer) races the
/// Vulkan loader's instance and driver lists and can die with SIGSEGV in
/// `loader_get_icd_and_device`. Use this instead; pair it with [`request_device_locked`].
///
/// Always a new instance, even for a descriptor without a display handle: the instance the
/// engine shares between its headless devices (ADR-7557) is not handed to a host's window.
pub fn create_window_instance(descriptor: wgpu::InstanceDescriptor) -> wgpu::Instance {
    new_instance(descriptor, true)
}

/// Requests a device and queue on `adapter` under the same process-wide loader-list lock the
/// engine's own device creation holds (ADR-2113): `vkCreateDevice` links the new device into the
/// loader's lists before its dispatch pointer is set, so a concurrent creation or a debug-name
/// call elsewhere could read a half-linked entry. Blocks until wgpu answers (the native backends
/// answer at once).
///
/// # Errors
///
/// wgpu's [`wgpu::RequestDeviceError`] (an unsupported feature or limit, a lost adapter).
pub fn request_device_locked(
    adapter: &wgpu::Adapter,
    descriptor: &wgpu::DeviceDescriptor<'_>,
) -> Result<(wgpu::Device, wgpu::Queue), wgpu::RequestDeviceError> {
    let _guard = loader_list_lock();
    regia_core::tasks::block_on(adapter.request_device(descriptor))
}

/// Creates a [`wgpu::Instance`] while holding a process-wide lock.
///
/// The Vulkan loader (1.4.309) is not safe when one thread runs
/// `vkEnumerateInstanceExtensionProperties` while another `vkCreateInstance` is still loading
/// and negotiating the ICDs: on the NVIDIA driver the first thread called a null ICD entry
/// point and the process died with SIGSEGV (test binaries that open one device per test
/// thread, e.g. `local_lights`, under load). wgpu-hal makes both calls inside
/// `Instance::new`, so every Regia Engine instance is created through here, one at a time.
///
/// A Vulkan instance also enumerates its adapters once under the same lock: the loader unloads
/// drivers without a device on that first enumeration (`loader_list_lock`, ADR-2113), so no
/// later enumeration of this instance changes the loader's lists.
///
/// The first Vulkan instance of the process is then kept (`DRIVER_PIN`, ADR-5794), so the
/// drivers it loaded are not unloaded and loaded again for every later instance. The clean exit
/// lets go of it (`release_driver_pin`).
///
/// A descriptor without a display handle (every headless device, `list_adapters`,
/// `probe_adapters`) gets the instance this process already created for the same descriptor,
/// if there is one (`HEADLESS_INSTANCES`, ADR-7557): only the first such call creates one. A
/// new instance makes the Vulkan loader load every selected driver again, and its first
/// enumeration frees the entry of each driver that reports no device before it unlinks it from
/// the lists that another thread's object naming walks (ADR-2113). The driver pin keeps the
/// drivers that have a device loaded, but not those: on the Intel Arc pass
/// (`VK_LOADER_DRIVERS_SELECT=*intel*` also selects Mesa's `intel_hasvk`, which reports no device
/// on Battlemage) every instance a test opened mid-run raced the other test threads.
pub(crate) fn create_instance(descriptor: wgpu::InstanceDescriptor) -> wgpu::Instance {
    cached_instance(descriptor, true)
}

/// [`create_instance`] without the vendor upscalers' Vulkan-only creation hooks: the instance
/// lists every adapter of `descriptor`'s backends, as a build without DLSS or XeSS does.
/// [`crate::gpu_select::list_adapters`] numbers `REGIA_GPU_INDEX` from it (ADR-7820). Shared
/// like [`create_instance`]'s; in a build without a vendor upscaler it is the same instance.
pub(crate) fn create_listing_instance(descriptor: wgpu::InstanceDescriptor) -> wgpu::Instance {
    cached_instance(descriptor, false)
}

/// The shared headless instance for `descriptor` and `vendor_hooks` (ADR-7557), or a new one
/// when `descriptor` carries a display handle.
fn cached_instance(descriptor: wgpu::InstanceDescriptor, vendor_hooks: bool) -> wgpu::Instance {
    let Some(key) = HeadlessInstanceKey::of(&descriptor, vendor_hooks) else {
        return new_instance(descriptor, vendor_hooks);
    };
    // Held across the creation, so two first callers do not both create one (the second would
    // be dropped, destroying an instance beside the first's users). Lock order: this cache, then
    // `LOADER_LISTS` (inside `new_instance`); `release_driver_pin` only try-locks the cache.
    // A poisoned lock only means a creation panicked; the cached instances are still valid.
    let mut cache = HEADLESS_INSTANCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, instance)) = cache.iter().find(|(cached, _)| *cached == key) {
        return instance.clone();
    }
    let instance = new_instance(descriptor, vendor_hooks);
    cache.push((key, instance.clone()));
    instance
}

/// The instances [`create_instance`] handed out for descriptors without a display handle, one per
/// distinct descriptor, held until the clean exit lets go of them with the driver pin
/// (`release_driver_pin`) or the process ends (ADR-7557).
static HEADLESS_INSTANCES: std::sync::Mutex<Vec<(HeadlessInstanceKey, wgpu::Instance)>> =
    std::sync::Mutex::new(Vec::new());

/// What makes two headless instance descriptors the same instance: everything but the display
/// handle, which a reusable descriptor does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeadlessInstanceKey {
    /// The instance's backends.
    backends: wgpu::Backends,
    /// Its flags (validation, debug names).
    flags: wgpu::InstanceFlags,
    /// The memory budget thresholds and backend options, by their `Debug` text: wgpu gives
    /// neither type `PartialEq`, and their text names every field.
    options: String,
    /// Whether the vendor upscalers' creation hooks were asked for; always `false` in a build
    /// without DLSS or XeSS, where the hooks do nothing (ADR-7820).
    vendor_hooks: bool,
}

impl HeadlessInstanceKey {
    /// The key of `descriptor`, or `None` when it carries a display handle (a window's
    /// instance, which is never shared).
    fn of(descriptor: &wgpu::InstanceDescriptor, vendor_hooks: bool) -> Option<Self> {
        if descriptor.display.is_some() {
            return None;
        }
        Some(Self {
            backends: descriptor.backends,
            flags: descriptor.flags,
            options: format!(
                "{:?} {:?}",
                descriptor.memory_budget_thresholds, descriptor.backend_options
            ),
            vendor_hooks: vendor_hooks && cfg!(any(feature = "dlss", feature = "xess")),
        })
    }
}

/// How many instances [`create_instance`] has created in this process (not counting the ones it
/// handed out again from `HEADLESS_INSTANCES`).
static INSTANCE_CREATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many [`wgpu::Instance`]s the engine has created in this process (ADR-7557). Headless
/// devices, [`crate::list_adapters`] and [`crate::hwrt::probe_adapters`] share one instance per
/// descriptor, so after the first headless device a later one adds nothing here; each window
/// instance ([`create_window_instance`]) adds one. Diagnostic only.
#[must_use]
pub fn instance_creation_count() -> u64 {
    INSTANCE_CREATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The variable a run that expects a GPU sets (ADR-7613): with it, a device creation that finds
/// no adapter, or whose `REGIA_GPU_*` selection is refused, panics instead of returning the error.
const REQUIRE_ADAPTER_ENV: &str = "REGIA_REQUIRE_ADAPTER";

/// Whether a `REGIA_REQUIRE_ADAPTER` value asks for the guard: set, not empty and not `0`.
fn adapter_required(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// Hands back `error` (no adapter, or a refused selection), unless the run expects a GPU
/// (`REGIA_REQUIRE_ADAPTER`, ADR-7613): then it panics. A GPU test that skips on a missing
/// device (`let Ok(device) = RenderDevice::headless() else { return }`) counts as passed. A card
/// run that lost its card would then report every later test as passed, as one the build server test
/// binary did after about 18 device creations. The panic turns each such skip into a failed test.
fn require_adapter(error: RenderError) -> RenderError {
    if adapter_required(regia_core::brand::env_var_os(REQUIRE_ADAPTER_ENV).as_deref()) {
        panic!(
            "{REQUIRE_ADAPTER_ENV} is set (this run expects a GPU) and no device could be opened: \
             {error}"
        );
    }
    error
}

/// Creates a new instance under the loader-list lock (the body of [`create_instance`]), with the
/// vendor upscalers' creation hooks when `vendor_hooks` is set and the build has one.
fn new_instance(descriptor: wgpu::InstanceDescriptor, vendor_hooks: bool) -> wgpu::Instance {
    let _mem = regia_core::memory::enter_owner(&crate::heap_owners::DEVICE);
    INSTANCE_CREATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _guard = loader_list_lock();
    let vulkan = descriptor.backends.contains(wgpu::Backends::VULKAN);
    #[cfg(any(feature = "dlss", feature = "xess"))]
    let instance = if vendor_hooks {
        vendor_instance(descriptor)
    } else {
        wgpu::Instance::new(descriptor)
    };
    #[cfg(not(any(feature = "dlss", feature = "xess")))]
    let instance = {
        let _ = vendor_hooks;
        wgpu::Instance::new(descriptor)
    };
    if vulkan {
        let _span = tracing::debug_span!("first_vulkan_enumeration").entered();
        drop(regia_core::tasks::block_on(
            instance.enumerate_adapters(wgpu::Backends::VULKAN),
        ));
        pin_vulkan_drivers(&instance);
    }
    instance
}

/// The instance of a build with a vendor upscaler (DLSS, XeSS): the vendor's Vulkan-only
/// instance with its runtime's extensions when available, else (no runtime, a failed hook) the
/// instance as usual. With both bindings XeSS is tried first, then DLSS.
///
/// A vendor instance opens Vulkan only. When `REGIA_GPU_INDEX` or `REGIA_GPU_NAME` selects an
/// adapter of another backend (DX12 on Windows), the hook is skipped and the full instance is
/// kept, so the variables still force DX12; the vendor then reports its `*_needs_vulkan` reason
/// on that device (ADR-7820). `REGIA_GPU_UUID`/`REGIA_GPU_PCI` select by bus, where the Vulkan
/// adapter is listed first, so they keep the hook without the extra enumeration.
#[cfg(any(feature = "dlss", feature = "xess"))]
fn vendor_instance(descriptor: wgpu::InstanceDescriptor) -> wgpu::Instance {
    use crate::gpu_select::{GPU_INDEX_ENV, GPU_NAME_ENV, GPU_PCI_ENV, GPU_UUID_ENV, GpuRequest};
    let set = |key: &str| {
        regia_core::brand::env_var(key)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let other_backends = !descriptor.backends.difference(wgpu::Backends::VULKAN).is_empty();
    let by_bus = set(GPU_UUID_ENV).is_some() || set(GPU_PCI_ENV).is_some();
    let request = GpuRequest {
        index: set(GPU_INDEX_ENV),
        name: set(GPU_NAME_ENV),
        ..GpuRequest::default()
    };
    if other_backends && !by_bus && !request.is_empty() {
        // wgpu's descriptor is not `Clone`: list the adapters on a display-less copy of it.
        let probe = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: descriptor.backends,
            flags: descriptor.flags,
            memory_budget_thresholds: descriptor.memory_budget_thresholds,
            backend_options: descriptor.backend_options.clone(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let mut adapters = regia_core::tasks::block_on(probe.enumerate_adapters(descriptor.backends));
        adapters.sort_by_key(|a| crate::backend_policy::listing_rank(a.get_info().backend));
        let listing: Vec<_> = adapters
            .iter()
            .map(|a| {
                let info = a.get_info();
                (info.name, info.device_pci_bus_id, info.backend)
            })
            .collect();
        drop(adapters);
        drop(probe);
        if !crate::gpu_select::vendor_hook_keeps_selection(&listing, &request) {
            tracing::info!(
                target: "regia_render",
                "the GPU request selects a non-Vulkan adapter: vendor upscaler hooks skipped"
            );
            return wgpu::Instance::new(descriptor);
        }
    }
    #[cfg(feature = "xess")]
    let hooked = crate::upscaler::vendor::xess::hook_instance(&descriptor);
    #[cfg(all(feature = "dlss", not(feature = "xess")))]
    let hooked = crate::upscaler::vendor::dlss::hook_instance(&descriptor);
    #[cfg(all(feature = "dlss", feature = "xess"))]
    let hooked = hooked.or_else(|| crate::upscaler::vendor::dlss::hook_instance(&descriptor));
    hooked.unwrap_or_else(|| wgpu::Instance::new(descriptor))
}

/// Backends Regia Engine instantiates (`specs/13` § 2): Vulkan on Linux and Android
/// (Android has no GL fallback); Metal on macOS (wgpu translates WGSL to MSL; there is no Vulkan
/// loader on a stock Mac); Vulkan and DX12 on Windows, where the default adapter pick is
/// Vulkan first and DX12 only when Vulkan has no hardware adapter
/// ([`crate::gpu_select::pick_default_adapter`]).
///
/// GL is never instantiated. Besides the spec's "no GL fallback", creating the
/// GL/EGL backend enumerates every DRM device on the machine at instance
/// creation, which wedges the whole process on hardware with a dead discrete GPU
/// on the bus (observed: amdgpu `device lost from bus` made every new Vulkan
/// client hang inside `drm_sched_entity_flush` before an adapter was chosen).
pub fn instance_backends() -> wgpu::Backends {
    crate::backend_policy::instance_backends_for_os(std::env::consts::OS)
}

/// The engine's wgpu instance flags (both window and headless devices use them).
///
/// Dev builds keep wgpu's debugging and validation flags, except shader debug info
/// (`InstanceFlags::DEBUG`) on Android: it makes naga emit `OpSource` with
/// `SourceLanguage::WGSL` (operand 10 of the SPIR-V registry), which postdates the
/// registry bundled with the Android emulator's SwiftShader — its translator aborts the
/// whole emulator process on it (observed on emulator 37.1.11, M3 sim_bridge lane).
/// `WGPU_DEBUG=1` still re-enables it through `with_env`.
pub(crate) fn instance_flags() -> wgpu::InstanceFlags {
    let mut flags = wgpu::InstanceFlags::from_build_config();
    if cfg!(target_os = "android") {
        flags.remove(wgpu::InstanceFlags::DEBUG);
    }
    flags.with_env()
}

/// The GPU device and queue. Created once, on the first resume, and kept across suspends (only the
/// surfaces are dropped, `docs/specs/11-platform-and-input.md` § 2).
#[derive(Resource, Clone)]
pub struct RenderDevice {
    /// The wgpu instance.
    pub instance: wgpu::Instance,
    /// The chosen adapter.
    pub adapter: wgpu::Adapter,
    /// The logical device.
    pub device: wgpu::Device,
    /// The device's queue.
    pub queue: wgpu::Queue,
    /// Features and limits obtained from the adapter/device (`specs/13` § 2).
    pub capabilities: RenderCapabilities,
    /// Process-unique identity (see [`RenderDevice::device_id`]).
    identity: u64,
    /// How the adapter was chosen (`REGIA_GPU_INDEX` / `REGIA_GPU_NAME`).
    selection: crate::AdapterSelection,
    /// The persistent pipeline cache's install, ended when the last clone drops (so the cache
    /// releases the device before the process exits).
    _pipeline_cache: Option<std::sync::Arc<crate::pipeline_cache::InstallGuard>>,
    /// A small host-visible upload buffer held for the device's lifetime
    /// ([`upload_keepalive`]): keeps the allocator's first `MAP_WRITE` memory block alive.
    _upload_keepalive: wgpu::Buffer,
    /// The submission of the last staging flush on this device's frame-path queue handles
    /// ([`Self::tracked_queue`]): the next flush waits for it (ADR-1346).
    staging_last_flush: StagingFlushSlot,
    /// Pipelines shared by every user of this device ([`Self::shared_pipelines`]).
    shared_pipelines: std::sync::Arc<crate::device_pipelines::DevicePipelines>,
}

/// Bytes of the device-lifetime upload keep-alive buffer ([`upload_keepalive`]).
const UPLOAD_KEEPALIVE_BYTES: u64 = 64 * 1024;

/// A never-mapped `MAP_WRITE | COPY_SRC` buffer that lives as long as the device.
///
/// wgpu places `MAP_WRITE` memory (every `queue.write_buffer` staging copy, the staging belt's
/// upload) in the first host-visible block of its allocator, the 256 MiB BAR heap on NVIDIA. When
/// a scene is torn down every allocation in that block drops and the block is freed; on the
/// NVIDIA 615.78 open kernel module (RTX 3080 Ti, Linux 7.2) the block allocated next is
/// written by the CPU at about 18 MB/s instead of 3-6 GB/s, so the next scene's belt upload
/// (300-600 KiB a frame) took 18-30 ms of every frame (lane minspec-hw, standalone repro in its
/// report). Holding one small allocation keeps that block alive for the device's lifetime;
/// 64 KiB of BAR memory is the whole cost. The 580 driver (GTX 1080) was not affected.
fn upload_keepalive(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("upload keep-alive"),
        size: UPLOAD_KEEPALIVE_BYTES,
        usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    })
}

/// Source of process-unique [`RenderDevice`] identities.
static NEXT_DEVICE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The optional features the renderer uses whenever the adapter offers them (`specs/13` § 2,
/// `specs/26` § 4): GPU timestamps, filtered float32, indirect-count and first-instance draws,
/// RG11B10 render targets, block compression, forced early depth, `clear_texture` and multiview. Each is
/// requested only when `adapter_features` has it; the renderer falls back where it is missing.
///
/// [`RenderDevice::new`] starts from this set; an embedder that creates its own device (the
/// editor) must request it too, or every `fallback_*` problem fires on a capable adapter.
#[must_use]
pub fn baseline_optional_features(adapter_features: wgpu::Features) -> wgpu::Features {
    let mut features = wgpu::Features::empty();
    for feature in [
        // GPU timestamps for the per-pass frame timings (inside encoders: the renderer writes
        // them between passes), and inside render passes for the shared draw's share.
        wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS,
        wgpu::Features::TIMESTAMP_QUERY,
        wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES,
        // Filtered 32-bit float reads (the terrain budget's hardware-filtered height pyramid).
        wgpu::Features::FLOAT32_FILTERABLE,
        // wgpu 30 has no separate MULTI_DRAW_INDIRECT bit: multi-draw is available wherever
        // `DownlevelFlags::INDIRECT_EXECUTION` is; the count buffer is this feature.
        wgpu::Features::MULTI_DRAW_INDIRECT_COUNT,
        wgpu::Features::INDIRECT_FIRST_INSTANCE,
        wgpu::Features::RG11B10UFLOAT_RENDERABLE,
        // Block-compressed sampling (cooked material and terrain maps, `compressed_texture`):
        // BC on desktop GPUs, ASTC / ETC2 on mobile. Without the feature a cooked material map is
        // decoded to RGBA8 on the CPU (BCn, ASTC LDR, ETC2 / EAC; ADR-2063).
        wgpu::Features::TEXTURE_COMPRESSION_BC,
        wgpu::Features::TEXTURE_COMPRESSION_ASTC,
        wgpu::Features::TEXTURE_COMPRESSION_ETC2,
        // ASTC HDR blocks (block-compressed lightmaps, ADR-2314): phones that have it sample the
        // cooked blocks; elsewhere the lightmap's CPU decode is uploaded.
        wgpu::Features::TEXTURE_COMPRESSION_ASTC_HDR,
        // Forced early depth tests (`@early_depth_test(force)`): a fragment stage with storage
        // writes (the terrain virtual texture's feedback) keeps early-Z.
        wgpu::Features::SHADER_EARLY_DEPTH_TEST,
        // `CommandEncoder::clear_texture`: a stochastic SSR pass waking from dormancy zeroes the
        // targets it kept instead of allocating new ones mid-play (ADR-168).
        wgpu::Features::CLEAR_TEXTURE,
        // One pass over a two-layer target: the XR multiview strategy (ADR-1918). Costs nothing
        // on a device that does not use it.
        wgpu::Features::MULTIVIEW,
    ] {
        if adapter_features.contains(feature) {
            features |= feature;
        }
    }
    features
}

impl RenderDevice {
    /// Process-unique identity of this device.
    ///
    /// wgpu handle equality compares only per-instance resource ids, so two
    /// devices created by two `wgpu::Instance`s can compare equal; caches that
    /// must rebuild when the device changes (e.g.
    /// [`crate::DeviceFrameRenderer`]) key on this instead.
    #[must_use]
    pub fn device_id(&self) -> u64 {
        self.identity
    }

    /// Pipelines (and other GPU objects) shared by every user of this device for its lifetime
    /// ([`crate::device_pipelines`]): a terrain or effects context built after another one, or
    /// after a warm job built one on a worker, compiles nothing. Dropped with the last clone.
    #[must_use]
    pub fn shared_pipelines(&self) -> &std::sync::Arc<crate::device_pipelines::DevicePipelines> {
        &self.shared_pipelines
    }

    /// Blocks until the GPU has finished all submitted work.
    ///
    /// Benchmarks call this per frame so a frame's measurement includes the GPU
    /// work it submitted (otherwise the submit call returns immediately and the
    /// queue drains in the background). The wait goes through the frame-path
    /// device wrapper, so it is counted as a blocking poll.
    pub fn wait_idle(&self) -> Result<(), RenderError> {
        self.tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .map(|_| ())
            .map_err(RenderError::Poll)
    }

    /// Runs the callbacks of finished GPU work (buffer maps, such as
    /// [`crate::FrameRenderer::request_texture_readback`]) without waiting for anything.
    ///
    /// # Errors
    ///
    /// [`RenderError::Poll`] when the device is lost.
    pub fn poll_nonblocking(&self) -> Result<(), RenderError> {
        self.device
            .poll(wgpu::PollType::Poll)
            .map(|_| ())
            .map_err(RenderError::Poll)
    }

    /// A handle that counts pipeline, bind-group and blocking-poll calls.
    ///
    /// The public [`Self::device`] field stays a [`wgpu::Device`]: other crates
    /// store `device.device.clone()` as that type. Frame-path code wraps it here
    /// so a `poll(Wait)` or `create_*_pipeline` is observed without a manual note.
    #[must_use]
    pub(crate) fn tracked(&self) -> TrackedDevice {
        TrackedDevice::new(self.device.clone())
    }

    /// A handle that counts `queue.submit`.
    ///
    /// The steady-frame test asserts a warm frame went through this at least
    /// once and at most twice. [`TrackedQueue`] does not deref to
    /// [`wgpu::Queue`], so a frame-path function that holds this handle cannot
    /// call the raw `submit`. [`Self::queue`] stays a plain queue for other crates.
    ///
    /// The handle knows this device, so a staging flush on it waits for the flush before it
    /// (bounded upload staging during a level load, [`crate::staging_flush`]).
    #[must_use]
    pub(crate) fn tracked_queue(&self) -> TrackedQueue {
        TrackedQueue::observe(&self.queue).with_device(&self.device, &self.staging_last_flush)
    }

    /// Creates a device for presenting to `surface` (or a headless device when `None`). Falls back to a
    /// software adapter (lavapipe, WARP) when no hardware adapter is found.
    ///
    /// `REGIA_GPU_UUID` / `_PCI` / `_INDEX` / `_NAME` ([`crate::gpu_select`]) choose the
    /// adapter, windowed or headless; a value that selects nothing (or, windowed, an adapter that
    /// cannot present to `surface`) warns and keeps this default pick, or fails under
    /// `REGIA_GPU_STRICT`.
    pub fn new(instance: wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Result<Self, RenderError> {
        Self::new_with_profile(instance, surface, None)
    }

    /// [`Self::new`] with every device request clamped to a minimum-spec profile
    /// ([`crate::minspec::MinspecProfile`]): limits are min()'d with the profile's numbers
    /// and optional features with the profile's mask, so the frame path runs — and its
    /// fallbacks activate — exactly as it would on that tier of GPU (min-spec auditing,
    /// `specs/13` § 2). `None` asks for everything the adapter has ([`Self::new`]).
    pub fn new_with_profile(
        instance: wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        profile: Option<crate::minspec::MinspecProfile>,
    ) -> Result<Self, RenderError> {
        Self::new_with_request(
            instance,
            surface,
            profile,
            crate::hwrt::device_request(),
            wgpu::Features::empty(),
        )
    }

    /// A headless device that asks for hardware ray queries whatever
    /// [`crate::hwrt::set_device_request`] left: for tools and tests that opt in themselves. The
    /// device has them where the adapter offers them ([`crate::hwrt::last_device_report`]).
    ///
    /// # Errors
    ///
    /// No adapter, or the request cannot be satisfied (see [`Self::new`]).
    pub fn headless_with_ray_queries() -> Result<Self, RenderError> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = instance_backends();
        descriptor.flags = instance_flags();
        let instance = create_instance(descriptor);
        crate::boot::mark_named("instance");
        Self::new_with_request(
            instance,
            None,
            None,
            crate::hwrt::HwrtRequest::On,
            wgpu::Features::empty(),
        )
    }

    /// A headless device that does not ask for `withheld` even where the adapter offers it: the
    /// frame path's fallbacks for a backend without those features run on any adapter. For
    /// example, withholding `SHADER_EARLY_DEPTH_TEST` gives a lavapipe device the DX12 and Metal
    /// terrain virtual texture path (ADR-2122).
    ///
    /// # Errors
    ///
    /// No adapter, or the request cannot be satisfied (see [`Self::new`]).
    pub fn headless_withholding(withheld: wgpu::Features) -> Result<Self, RenderError> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = instance_backends();
        descriptor.flags = instance_flags();
        let instance = create_instance(descriptor);
        crate::boot::mark_named("instance");
        Self::new_with_request(instance, None, None, crate::hwrt::device_request(), withheld)
    }

    fn new_with_request(
        instance: wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        profile: Option<crate::minspec::MinspecProfile>,
        hwrt_request: crate::hwrt::HwrtRequest,
        withheld: wgpu::Features,
    ) -> Result<Self, RenderError> {
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::DEVICE);
        // `REGIA_GPU_*` applies to windowed devices too (shared machines must never land on
        // another card).
        let (chosen, selection) = crate::gpu_select::select_for_surface(&instance, surface);
        if let crate::AdapterSelection::Refused(reason) = &selection {
            return Err(require_adapter(crate::gpu_select::refused(reason)));
        }
        let chosen = chosen.or_else(|| crate::gpu_select::default_adapter(&instance, surface));
        let picked = match chosen {
            Some(adapter) => Ok(adapter),
            None => regia_core::tasks::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: surface,
                ..Default::default()
            }))
            .or_else(|_| {
                regia_core::tasks::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: true,
                    compatible_surface: surface,
                    ..Default::default()
                }))
            }),
        };
        let adapter = picked.map_err(|err| require_adapter(RenderError::NoAdapter(err)))?;
        crate::boot::mark_named("adapter");
        let info = adapter.get_info();
        tracing::info!(
            target: "regia_render",
            "GPU: {} ({:?}, {:?}, driver {} {}; selection {:?})",
            info.name,
            info.backend,
            info.device_type,
            info.driver,
            info.driver_info,
            selection
        );
        if let Some(tier) = crate::capabilities::ResourceBindingTier::for_adapter(
            info.backend,
            &adapter.limits(),
            adapter.features(),
        ) {
            tracing::info!(target: "regia_render", "DX12 resource binding: {tier}");
        }
        // Validation or shader debug info in a shipped build slows every pipeline compile;
        // tester logs show which flags the build ran with (lane v018-stalls).
        tracing::info!(target: "regia_render", "wgpu instance flags: {:?}", instance_flags());
        crate::shader_workarounds::note_adapter(&info);
        crate::forward_lite::note_adapter(&info);
        // Record the chosen adapter for crash reports (specs/21 § 5): without this, every report's
        // `gpu:` line falls back to `(not reported)`.
        regia_platform::crash::set_gpu_info(format!(
            "{} ({:?}, {:?}, driver {} {})",
            info.name, info.backend, info.device_type, info.driver, info.driver_info
        ));
        // Every crash report (panic or device loss) carries the breadcrumb trail (ADR-2307).
        regia_platform::crash::add_report_section("gpu_breadcrumbs", crate::breadcrumbs::crash_trail);
        // M5 needs storage buffers for froxel lists, skin palettes and the mesh arena.
        // Ask for the adapter's own limits so desktop Vulkan/DX12 keep compute bindings;
        // `downlevel_webgl2_defaults` zeroes `max_storage_buffers_per_shader_stage`.
        // A min-spec profile clamps them to what that tier of GPU exposes.
        let adapter_limits = adapter.limits();
        // Android refuses to start below the storage-buffer floor, with the reason logged
        // (`specs/13` § 2, ADR-7211), instead of failing later at layout creation.
        if cfg!(target_os = "android")
            && let Some(reason) = crate::storage_floor::android_storage_refusal(
                &adapter_limits,
                adapter.get_downlevel_capabilities().flags,
            )
        {
            tracing::error!(target: "regia_render", "{reason}");
            return Err(RenderError::GpuSelection(reason));
        }
        let required_limits = match profile {
            None => adapter_limits,
            Some(profile) => profile.clamp_limits(&adapter_limits),
        };
        let adapter_features = adapter.features();
        let mut features = baseline_optional_features(adapter_features);
        // Bindless level 3 (`texture_table`, ADR-863): the material texture table's features,
        // only when `REGIA_BINDLESS_LEVEL` or the render profile (ADR-2071) asks for it. Read once
        // here: the level changes the material layouts and shader defines of everything built on
        // this device.
        let bindless_level = crate::texture_table::requested_level_now();
        features |= crate::texture_table::device_features_for_level(bindless_level, adapter_features);
        // Bindless step 5 (`table_heap`, ADR-1110): partially bound arrays where the adapter has
        // them, so a table rebuild binds only the prefix the scene uses.
        features |= crate::table_heap::optional_table_features(bindless_level, adapter_features);
        // Persistent pipeline cache (`pipeline_cache`): only when a cache directory is
        // configured, so test and tool devices never carry one. A headless device beside a live
        // device leaves the cache with that one (ADR-2073); a window's device takes it over.
        // Intel's Windows Vulkan driver gets none (`driver_skips_cache`, ADR-3288).
        let cache_dir = crate::pipeline_cache::directory().filter(|_| {
            crate::backend_policy::uses_pipeline_cache(info.backend)
                && adapter_features.contains(wgpu::Features::PIPELINE_CACHE)
                && (surface.is_some() || !crate::pipeline_cache::held_by_live_device())
                && !crate::pipeline_cache::driver_skips_cache_for(&info)
        });
        if cache_dir.is_some() {
            features |= wgpu::Features::PIPELINE_CACHE;
        }
        // A min-spec profile carries only what that tier of GPU exposes.
        if let Some(profile) = profile {
            features &= profile.feature_mask();
        }
        features &= !withheld;
        // Hardware ray queries (ADR-515, ADR-2035): only when the render profile asks and the
        // adapter offers them; otherwise the request below is unchanged.
        let (hwrt_request, hwrt_probe, hwrt) =
            crate::hwrt::probe::plan_device_request(&adapter, profile.is_some(), hwrt_request);
        features |= hwrt.features;
        let device_descriptor = wgpu::DeviceDescriptor {
            label: Some("regia device"),
            required_limits,
            required_features: features,
            experimental_features: hwrt.experimental_features(),
            memory_hints: device_memory_hints(&info),
            ..Default::default()
        };
        // Device creation links the device into the loader's lists and looks up every device
        // entry point in them: one creation at a time (`loader_list_lock`, ADR-2113).
        let (device, queue) = {
            let _guard = loader_list_lock();
            // DLSS (ADR-2002): NGX's device extensions on an NVIDIA Vulkan adapter when the
            // binding is built and its runtime is found; any failure requests the device as usual.
            #[cfg(feature = "xess")]
            let hooked = crate::upscaler::vendor::xess::hook_device(&adapter, &device_descriptor);
            #[cfg(not(feature = "xess"))]
            let hooked = None;
            #[cfg(feature = "dlss")]
            let hooked =
                hooked.or_else(|| crate::upscaler::vendor::dlss::hook_device(&adapter, &device_descriptor));
            match hooked {
                Some(pair) => pair,
                None => regia_core::tasks::block_on(adapter.request_device(&device_descriptor))
                    .map_err(RenderError::RequestDevice)?,
            }
        };
        crate::hwrt::probe::record_device_report(crate::hwrt::HwrtReport::new(
            hwrt_request,
            hwrt_probe,
            hwrt,
            device.features(),
        ));
        let capabilities = RenderCapabilities::from_adapter(&adapter, &device);
        // Bindless tier (ADR-316): the device's material path, and what the adapter could add.
        let bindless =
            crate::bindless::BindlessCaps::from_adapter(device.features(), &device.limits(), info.backend);
        let adapter_bindless =
            crate::bindless::BindlessCaps::from_adapter(adapter_features, &adapter.limits(), info.backend);
        tracing::info!(
            target: "regia_render",
            "bindless tier: {} (adapter supports {}; {} storage buffers per stage, {} binding-array elements); material constants: {}",
            bindless.tier,
            adapter_bindless.tier,
            bindless.storage_buffers_per_stage,
            adapter_bindless.max_binding_array_elements,
            if crate::bindless::material_ssbo(&device) {
                "storage arena"
            } else {
                "per-material uniform buffers"
            }
        );
        match crate::texture_table::device_support_with_adapter(&device, adapter_features) {
            crate::texture_table::TableSupport::Table { capacity } => tracing::info!(
                target: "regia_render",
                "material texture table: on ({capacity} views; bindless level {bindless_level})"
            ),
            crate::texture_table::TableSupport::Fallback(reason) => tracing::info!(
                target: "regia_render",
                "material texture table: off ({reason}; bindless level {bindless_level})"
            ),
        }
        let pipeline_cache = cache_dir
            .and_then(|dir| crate::pipeline_cache::PersistentPipelineCache::open(&device, &info, &dir))
            .map(|cache| std::sync::Arc::new(crate::pipeline_cache::install(&device, cache)));
        crate::boot::mark(crate::boot::BootMark::Device);
        let render_device = Self {
            instance,
            adapter,
            queue,
            capabilities,
            identity: NEXT_DEVICE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            selection,
            _pipeline_cache: pipeline_cache,
            _upload_keepalive: upload_keepalive(&device),
            staging_last_flush: StagingFlushSlot::default(),
            shared_pipelines: std::sync::Arc::default(),
            device,
        };
        #[cfg(test)]
        keep_for_test_run(&render_device);
        Ok(render_device)
    }

    /// How this device's adapter was chosen (`REGIA_GPU_INDEX` / `REGIA_GPU_NAME`).
    #[must_use]
    pub fn adapter_selection(&self) -> &crate::AdapterSelection {
        &self.selection
    }

    /// A device assembled from parts an embedder already created (the editor
    /// shares its winit window's device with the engine's renderer).
    ///
    /// [`RenderDevice::new`] is the normal constructor; this exists so
    /// embedders that must own the surface/device themselves (egui-winit
    /// shells) don't create a second logical device.
    #[must_use]
    pub fn from_parts(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        capabilities: RenderCapabilities,
    ) -> Self {
        let render_device = Self {
            instance,
            adapter,
            queue,
            capabilities,
            identity: NEXT_DEVICE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            selection: crate::AdapterSelection::Default,
            _pipeline_cache: None,
            _upload_keepalive: upload_keepalive(&device),
            staging_last_flush: StagingFlushSlot::default(),
            shared_pipelines: std::sync::Arc::default(),
            device,
        };
        #[cfg(test)]
        keep_for_test_run(&render_device);
        render_device
    }

    /// A device without a window, for tests and offline rendering.
    pub fn headless() -> Result<Self, RenderError> {
        Self::headless_with_flags(instance_flags())
    }

    /// One [`Self::headless`] device per process, created on first use and cloned per call.
    ///
    /// For test binaries whose tests run on parallel threads. Each test opening its own
    /// instance and device livelocked the NVIDIA driver when several such processes shared
    /// one GPU (RTX 5090, driver 615.71): the test threads blocked on a priority-inheritance
    /// futex inside the driver while one driver thread spun, and no test finished (the UI
    /// compositing tests hung in 7 of 20 runs at 8 processes at once). With one device per
    /// process, no instance or device is created or destroyed while another test is using
    /// the GPU. A failed creation is not cached; the next call tries again.
    pub fn shared_headless() -> Result<Self, RenderError> {
        static SHARED: std::sync::Mutex<Option<RenderDevice>> = std::sync::Mutex::new(None);
        // A poisoned lock only means a test panicked while holding it; the slot is still valid.
        let mut slot = SHARED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(device) = slot.as_ref() {
            return Ok(device.clone());
        }
        let device = Self::headless()?;
        *slot = Some(device.clone());
        Ok(device)
    }

    /// Headless device with explicit instance flags.
    ///
    /// `check-shaders` forces [`wgpu::InstanceFlags::VALIDATION`] even in release,
    /// where the crate's default instance flags leave validation off.
    pub fn headless_with_flags(flags: wgpu::InstanceFlags) -> Result<Self, RenderError> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = instance_backends();
        descriptor.flags = flags;
        let instance = create_instance(descriptor);
        crate::boot::mark_named("instance");
        Self::new(instance, None)
    }

    /// Headless device with every request clamped to a minimum-spec profile
    /// ([`crate::minspec::MinspecProfile`]): the device a Pascal or mobile-tier GPU
    /// would have, on whatever adapter the host offers (lavapipe in CI). The renderer's
    /// fallbacks for what the tier lacks then activate, and `capabilities` records them
    /// ([`crate::RenderCapabilities::fallback_notes`]).
    ///
    /// # Errors
    ///
    /// No adapter, or the clamped request cannot be satisfied (see [`Self::new`]).
    pub fn headless_with_profile(profile: crate::minspec::MinspecProfile) -> Result<Self, RenderError> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = instance_backends();
        descriptor.flags = instance_flags();
        let instance = create_instance(descriptor);
        crate::boot::mark_named("instance");
        Self::new_with_profile(instance, None, Some(profile))
    }
}

/// [`wgpu::Device`] whose pipeline, bind-group and blocking-poll calls are counted.
///
/// Inherent methods shadow [`wgpu::Device`]'s. A `&TrackedDevice` still coerces
/// to `&wgpu::Device` for every other call (`create_buffer`, `create_texture`).
/// [`RenderDevice::device`] stays a plain device so crates that store
/// `device.device.clone()` as [`wgpu::Device`] keep compiling; frame-path
/// functions call [`TrackedDevice::observe`] on the way in, which is the only
/// way those methods are reached. Crates with their own frame-path resources
/// (regia_vfx) store one so the hitch counters see their creates too.
#[derive(Clone)]
pub struct TrackedDevice {
    inner: wgpu::Device,
}

impl TrackedDevice {
    /// Wraps `device`. Cloning a device copies its id, not the GPU.
    #[must_use]
    pub fn new(device: wgpu::Device) -> Self {
        Self { inner: device }
    }

    /// Same as [`Self::new`] on a clone of `device`.
    ///
    /// Frame-path functions start with this so `create_render_pipeline`,
    /// `create_compute_pipeline`, `create_bind_group` and `poll` hit the
    /// inherent methods below. Deref coercion to [`wgpu::Device`] would skip them.
    #[must_use]
    pub fn observe(device: &wgpu::Device) -> Self {
        Self::new(device.clone())
    }

    /// Forwards `poll`. A [`wgpu::PollType::Wait`] increments the blocking-poll counter and
    /// the caller's entry in [`crate::blocking_poll_sites`] (ADR-4050).
    /// [`wgpu::PollType::Poll`] does not.
    #[track_caller]
    pub fn poll(&self, poll_type: wgpu::PollType) -> Result<wgpu::PollStatus, wgpu::PollError> {
        if poll_type.is_wait() {
            crate::frame_stats::note_blocking_poll(std::panic::Location::caller());
        }
        let _span = tracing::trace_span!("device_poll").entered();
        self.inner.poll(poll_type)
    }

    /// Creates a render pipeline and counts it, with its first colour format, depth format,
    /// sample count and the calling site (ADR-7885, [`crate::CreateDetail`]).
    #[track_caller]
    pub fn create_render_pipeline(&self, desc: &wgpu::RenderPipelineDescriptor<'_>) -> wgpu::RenderPipeline {
        let format = desc
            .fragment
            .as_ref()
            .and_then(|fragment| fragment.targets.iter().flatten().next())
            .map(|target| target.format);
        crate::frame_stats::note_pipeline_shape(
            desc.label,
            format,
            desc.depth_stencil.as_ref().map(|depth| depth.format),
            desc.multisample.count,
            Some(std::panic::Location::caller()),
        );
        let _span = tracing::trace_span!("create_render_pipeline").entered();
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::PIPELINES);
        let start = std::time::Instant::now();
        let pipeline = self.inner.create_render_pipeline(desc);
        note_phase(CostPhase::RenderPipeline, start);
        note_pipeline_time(desc.label, start);
        pipeline
    }

    /// Creates a compute pipeline and counts it, with the calling site (ADR-7885).
    #[track_caller]
    pub fn create_compute_pipeline(
        &self,
        desc: &wgpu::ComputePipelineDescriptor<'_>,
    ) -> wgpu::ComputePipeline {
        crate::frame_stats::note_pipeline_shape(
            desc.label,
            None,
            None,
            0,
            Some(std::panic::Location::caller()),
        );
        let _span = tracing::trace_span!("create_compute_pipeline").entered();
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::PIPELINES);
        let start = std::time::Instant::now();
        let pipeline = self.inner.create_compute_pipeline(desc);
        note_phase(CostPhase::ComputePipeline, start);
        note_pipeline_time(desc.label, start);
        pipeline
    }

    /// Creates a bind group and counts it.
    pub fn create_bind_group(&self, desc: &wgpu::BindGroupDescriptor<'_>) -> wgpu::BindGroup {
        crate::frame_stats::note_bind_group(desc.label);
        self.inner.create_bind_group(desc)
    }

    /// Creates a buffer and counts it (and its bytes).
    pub fn create_buffer(&self, desc: &wgpu::BufferDescriptor<'_>) -> wgpu::Buffer {
        crate::frame_stats::note_buffer(desc.label, desc.size);
        self.inner.create_buffer(desc)
    }

    /// Creates a texture and counts it (and its bytes).
    pub fn create_texture(&self, desc: &wgpu::TextureDescriptor<'_>) -> wgpu::Texture {
        crate::frame_stats::note_texture(desc.label, crate::frame_stats::texture_bytes(desc));
        self.inner.create_texture(desc)
    }
}

impl std::ops::Deref for TrackedDevice {
    type Target = wgpu::Device;

    fn deref(&self) -> &wgpu::Device {
        &self.inner
    }
}

/// [`wgpu::Queue`] whose `submit` calls are counted.
///
/// There is no [`Deref`] to [`wgpu::Queue`]. A hot-path `queue.submit` is this
/// inherent method, so a raw [`wgpu::Queue::submit`] does not type-check on a
/// [`TrackedQueue`]. Uploads go through [`Self::write_buffer`] and
/// [`Self::write_texture`]. [`RenderDevice::queue`] stays a plain queue so
/// crates outside the frame path keep compiling; frame-path functions take
/// this type instead.
///
/// The writes also feed the staging flush ([`crate::staging_flush`]): once the bytes written
/// on this thread since its last submit reach the cap, the pending writes are submitted on
/// their own, so a level's first frame never holds every texture in staging at once.
#[derive(Clone)]
pub(crate) struct TrackedQueue {
    inner: wgpu::Queue,
    /// Set by [`Self::with_device`]: a flush waits for the flush before it.
    flush_wait: Option<FlushWait>,
}

#[cfg(feature = "test-hooks")]
thread_local! {
    /// The device that every frame-path submit of this thread waits for
    /// ([`RenderDevice::wait_after_submit_on_this_thread`]).
    static WAIT_AFTER_SUBMIT: std::cell::RefCell<Option<wgpu::Device>> = const { std::cell::RefCell::new(None) };
}

#[cfg(feature = "test-hooks")]
impl RenderDevice {
    /// Test hook (cargo feature `test-hooks`): while `on`, every frame-path submit made on the
    /// calling thread waits for this device to finish it before it returns.
    ///
    /// A frame maps its readback buffers (texture feedback, the GPU timers, the exposure
    /// histogram) right after its submit. wgpu allocates a list for the first buffer mapped
    /// while that submission is still in flight, and none once the device has finished it, and
    /// its submit already looks for finished work before it returns: which of the two a frame
    /// gets depends on the machine's load, so a steady frame's heap allocations move by one.
    /// The steady-frame allocation tests turn this on for the drawing thread, so every map
    /// follows a finished submission on every machine (ADR-5718). The frame's work and its
    /// order are unchanged; only the wait moves from after the frame to after its submit.
    pub fn wait_after_submit_on_this_thread(&self, on: bool) {
        WAIT_AFTER_SUBMIT.with(|wait| *wait.borrow_mut() = on.then(|| self.device.clone()));
    }
}

/// Waits for the device this thread asked to wait for, if any (the test hook above). The
/// device's own `poll`, so the frame's blocking-poll counter does not see it.
#[cfg(feature = "test-hooks")]
fn wait_after_submit() {
    let device = WAIT_AFTER_SUBMIT
        .try_with(|wait| wait.borrow().clone())
        .ok()
        .flatten();
    if let Some(device) = device {
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
    }
}

/// The submission of a device's last staging flush, shared by every handle that waits on it.
pub(crate) type StagingFlushSlot = std::sync::Arc<std::sync::Mutex<Option<wgpu::SubmissionIndex>>>;

/// What a staging flush needs to wait for the flush before it.
#[derive(Clone)]
struct FlushWait {
    device: wgpu::Device,
    /// The submission of the device's last flush (shared by every handle of the device).
    last: StagingFlushSlot,
}

impl TrackedQueue {
    /// Wraps `queue`.
    #[must_use]
    pub(crate) fn new(queue: wgpu::Queue) -> Self {
        Self {
            inner: queue,
            flush_wait: None,
        }
    }

    /// This handle with `device`, the device of its queue, and `last`, the device's last-flush
    /// slot: a staging flush waits for the device's previous flush (from this handle or another
    /// one of the same device), which bounds the staging alive at once to about two caps.
    #[must_use]
    pub(crate) fn with_device(mut self, device: &wgpu::Device, last: &StagingFlushSlot) -> Self {
        self.flush_wait = Some(FlushWait {
            device: device.clone(),
            last: std::sync::Arc::clone(last),
        });
        self
    }

    /// Same as [`Self::new`] on a clone of `queue`.
    #[must_use]
    pub(crate) fn observe(queue: &wgpu::Queue) -> Self {
        Self::new(queue.clone())
    }

    /// The wrapped queue, for one-off work that is not part of a frame (no counters move).
    pub(crate) fn untracked(&self) -> &wgpu::Queue {
        &self.inner
    }

    /// Submits `command_buffers` and counts the call.
    pub(crate) fn submit<I>(&self, command_buffers: I) -> wgpu::SubmissionIndex
    where
        I: IntoIterator<Item = wgpu::CommandBuffer>,
    {
        crate::frame_stats::note_submit();
        crate::staging_flush::note_submit();
        let index = self.inner.submit(command_buffers);
        // Spec 54: the frame thread's pass markers become this submission's record, and the
        // GPU's completion of it is counted.
        if let Some(done) = crate::breadcrumbs::on_submit() {
            self.inner.on_submitted_work_done(done);
        }
        #[cfg(feature = "test-hooks")]
        wait_after_submit();
        index
    }

    /// Forwards `write_buffer` and counts its bytes. Not a submit, unless the writes since the
    /// last one reached the staging cap ([`Self::flush_staging`]).
    pub(crate) fn write_buffer(&self, buffer: &wgpu::Buffer, offset: u64, data: &[u8]) {
        crate::frame_stats::note_upload(data.len() as u64);
        self.inner.write_buffer(buffer, offset, data);
        self.after_write(data.len() as u64);
    }

    /// Forwards `write_texture` and counts its bytes. Not a submit.
    pub(crate) fn write_texture(
        &self,
        texture: wgpu::TexelCopyTextureInfo<'_>,
        data: &[u8],
        data_layout: wgpu::TexelCopyBufferLayout,
        size: wgpu::Extent3d,
    ) {
        crate::frame_stats::note_upload(data.len() as u64);
        crate::frame_stats::note_texture_upload(data.len() as u64);
        self.inner.write_texture(texture, data, data_layout, size);
        self.after_write(data.len() as u64);
    }

    /// Counts `bytes` of staging and flushes once the cap is reached.
    fn after_write(&self, bytes: u64) {
        if crate::staging_flush::note_write(bytes) {
            self.flush_staging();
        }
    }

    /// Submits the pending writes on their own (an empty submit: wgpu records them into a
    /// submission that runs before every later one, so no pass reads different bytes), then,
    /// when the handle knows its device, waits for its previous flush so that flush's staging
    /// is freed before more is written.
    fn flush_staging(&self) {
        let _span = tracing::info_span!("staging_flush").entered();
        let bytes = crate::staging_flush::take_pending();
        crate::frame_stats::note_submit();
        let index = self.inner.submit(std::iter::empty());
        let mut waited = false;
        if let Some(wait) = &self.flush_wait {
            let previous = wait
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .replace(index);
            if let Some(previous) = previous {
                let device = TrackedDevice::new(wait.device.clone());
                match device.poll(wgpu::PollType::Wait {
                    submission_index: Some(previous),
                    timeout: Some(crate::staging_flush::FLUSH_WAIT_TIMEOUT),
                }) {
                    Ok(_) => waited = true,
                    Err(err) => {
                        tracing::warn!(target: "regia_render", %err, "staging flush did not wait for the previous flush");
                    }
                }
            }
        }
        crate::staging_flush::note_flush(bytes, waited);
    }

    /// Forwards the timestamp period. Not a submit.
    #[must_use]
    pub(crate) fn get_timestamp_period(&self) -> f32 {
        self.inner.get_timestamp_period()
    }
}

/// `PreUpdate` system: publishes [`regia_platform::SoftwareRenderer`] from the adapter of the
/// current [`RenderDevice`] whenever the device is created or replaced, so crates below the
/// renderer (the audio output gate's `Auto` throttle) can tell a software rasterizer (lavapipe,
/// llvmpipe, WARP) from a GPU without depending on `regia_render`. Inert without a device.
pub(crate) fn publish_software_renderer(
    device: Option<Res<RenderDevice>>,
    current: Option<Res<regia_platform::SoftwareRenderer>>,
    mut commands: Commands,
) {
    let Some(device) = device else {
        return;
    };
    if !device.is_changed() && current.is_some() {
        return;
    }
    let info = device.adapter.get_info();
    crate::breadcrumbs::global().set_adapter(&info);
    let software = info.device_type == wgpu::DeviceType::Cpu;
    if current.map(|c| c.0) != Some(software) {
        tracing::info!(target: "regia_render", software, "render adapter class published");
        commands.insert_resource(regia_platform::SoftwareRenderer(software));
    }
}

/// A process-shared headless device for the crate's own GPU tests.
///
/// Renderer tests legitimately create several devices per process, but parallel test
/// threads each creating a fresh `wgpu::Instance` + lavapipe device at once races in the
/// software rasteriser's shared state: under a loaded gate a readback could return a
/// half-written frame (`split_screen_with_post_draws_both_halves` failed once this way).
/// Sharing one device — created lazily and cloned per call — matches the
/// `OnceLock<RenderDevice>` pattern the terrain/vfx/asset-pipeline test crates already use,
/// and removes the concurrent device-creation race while every test still drives the full
/// frame path. [`device_scoped_renderer_survives_device_changes`] is the one test that needs
/// two distinct devices and still creates them itself.
#[cfg(test)]
pub(crate) fn test_device() -> RenderDevice {
    static DEVICE: std::sync::OnceLock<RenderDevice> = std::sync::OnceLock::new();
    DEVICE
        .get_or_init(|| RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)"))
        .clone()
}

/// Whether this crate's unit tests keep the devices they open on the adapter `info` describes
/// until the process exits (`keep_for_test_run`): every hardware adapter, not a software one.
///
/// ADR-1584 kept them on NVIDIA adapters only (a live-lock in `vkDestroyDevice`). ADR-7531 extends
/// it to every hardware adapter: destroying a device while another test thread names an object
/// is one side of the Vulkan loader list race (ADR-2113; `vkDestroyDevice` frees the driver's
/// device before it unlinks it from the list `loader_get_icd_and_device` walks), and on the
/// Intel Arc pass the lib binary died in that walk (`libvulkan.so.1.4.309` +0x2d066) at a
/// different test each run. Software adapters (lavapipe) keep their old behavior: they never
/// showed the fault, and their memory is the host's.
fn unit_tests_keep(info: &wgpu::AdapterInfo) -> bool {
    info.device_type != wgpu::DeviceType::Cpu
}

/// The allocator block sizes of an engine device on `adapter`: wgpu's default
/// ([`wgpu::MemoryHints::Performance`], 128-256 MiB device blocks), except in this crate's unit
/// tests on a hardware adapter. Those keep every device they open until exit
/// (`keep_for_test_run`, ADR-1584, ADR-7531), and each kept device holds on to one empty block
/// per memory type it used, so they ask for [`wgpu::MemoryHints::MemoryUsage`] (8-64 MiB
/// blocks). With the default sizes the lib test binary peaked at 13.2 GB of an RTX 5090 (8.5 GB
/// without kept devices), with these at 7.2 GB.
///
/// Android asks for [`wgpu::MemoryHints::MemoryUsage`] on every adapter (ADR-5423). A phone's
/// GPU memory is host-visible system memory, so the allocator uses its host block sizes
/// (64 MiB, then 128 MiB, with the default) and every block it reserves counts in the process's
/// PSS. On the TEST Pixel 9 Pro the arena's 131 MB of allocations at Mobile High sometimes
/// took a third block (reserved 320 MB instead of 192 MB): PSS 717-721 MB against the 700 MB
/// budget. With 4-32 MiB host blocks the free space the allocator holds stays small.
pub(crate) fn device_memory_hints(adapter: &wgpu::AdapterInfo) -> wgpu::MemoryHints {
    memory_hints_for(unit_tests_keep(adapter), cfg!(test), cfg!(target_os = "android"))
}

/// The block-size choice behind `device_memory_hints`: `kept` is an adapter whose unit-test
/// devices are kept (`unit_tests_keep`), `unit_test` this crate's unit tests, `android` the
/// Android target.
fn memory_hints_for(kept: bool, unit_test: bool, android: bool) -> wgpu::MemoryHints {
    if android || (unit_test && kept) {
        wgpu::MemoryHints::MemoryUsage
    } else {
        wgpu::MemoryHints::default()
    }
}

/// Devices opened by this crate's unit tests on a hardware adapter, held until the process exits
/// (ADR-1584, ADR-7531). Each tuple drops its queue and device before its adapter and instance.
#[cfg(test)]
type KeptDevice = (wgpu::Queue, wgpu::Device, wgpu::Adapter, wgpu::Instance);

/// The devices [`keep_for_test_run`] holds until the process ends.
#[cfg(test)]
static KEPT: std::sync::Mutex<Vec<KeptDevice>> = std::sync::Mutex::new(Vec::new());

/// Keeps a device that a unit test opened on a hardware adapter alive until the test process
/// exits, so no test destroys a `VkDevice` while other test threads are inside the driver or the
/// Vulkan loader (ADR-1584, ADR-7531).
///
/// About 30 renderer tests open devices of their own (two paths compared on one adapter, a
/// minimum-spec profile, a device change). On the NVIDIA 615.71 driver (RTX 5090, Linux), a
/// thread in `vkDestroyDevice` while about 40 other test threads created pipelines, textures or
/// submissions on their own devices live-locked the whole lib test binary (twice in 61 full
/// runs). Only the raw wgpu handles are kept: the test's `RenderDevice` still drops its
/// pipeline cache install, upload keep-alive and shared pipelines as before. The process then
/// ends through [`register_end_of_test_process`]. Since ADR-7531 this holds on every hardware
/// adapter (the Intel Arc pass died in the loader's device-list walk while another test's device
/// was destroyed); software adapters keep their old behavior (`unit_tests_keep`).
#[cfg(test)]
fn keep_for_test_run(device: &RenderDevice) {
    if !unit_tests_keep(&device.adapter.get_info()) {
        return;
    }
    keep_device(device);
}

/// Holds clones of `device`'s handles until the process ends, on any adapter.
#[cfg(test)]
fn keep_device(device: &RenderDevice) {
    register_end_of_test_process();
    // A poisoned lock only means another test panicked while pushing; the list is still valid.
    KEPT.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((
            device.queue.clone(),
            device.device.clone(),
            device.adapter.clone(),
            device.instance.clone(),
        ));
}

/// Makes this test process end through [`end_test_process`], ahead of the exit handlers
/// registered so far.
///
/// With kept devices the NVIDIA 615.71 driver's own `[vkps] Update` thread faulted during
/// process exit, in every gate's GPU pass from v0.2.4 on (`libnvidia-eglcore` +0xce10de, or an
/// instruction fetch from memory no longer mapped; kernel log, ADR-1907). Exit handlers run last
/// registered first, so each keep registers again: a driver can add handlers after it loaded
/// (its first pipeline, say), and they must not run ahead of this one. Only the first call to
/// run does anything, because it does not return.
#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
fn register_end_of_test_process() {
    let _ = crate::platform::at_exit_with_status(end_test_process);
}

/// Without glibc's `on_exit` the kept devices are destroyed at exit instead, where there is a
/// hook at all (macOS has kept devices stay alive into exit). Registered after the driver loaded,
/// so it runs before the driver's exit handlers.
#[cfg(all(test, not(all(target_os = "linux", target_env = "gnu"))))]
fn register_end_of_test_process() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let _ = crate::platform::at_exit(release_kept_devices);
    });
}

/// Process exit (`on_exit`, after the test harness returned): joins the warm workers, then ends
/// the process with the harness's status. Every test has reported by now, so nothing needs the
/// devices, and tearing the driver down is the step its thread faults in: no device is destroyed
/// and no driver exit handler runs (ADR-1907). Logs nothing and touches no thread-local, like the
/// warm queues' own exit hook.
#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
extern "C" fn end_test_process(status: std::ffi::c_int, _arg: *mut std::ffi::c_void) {
    crate::pipeline_warm::exit_hook();
    crate::platform::exit_now(status);
}

/// Process exit (`atexit`, after the test harness returned): joins the warm workers first (they
/// may hold a kept device inside the driver), then destroys the kept devices one at a time on
/// this thread. Logs nothing and touches no thread-local, like the warm queues' own exit hook.
#[cfg(all(test, not(all(target_os = "linux", target_env = "gnu"))))]
extern "C" fn release_kept_devices() {
    crate::pipeline_warm::exit_hook();
    let _ = std::panic::catch_unwind(|| {
        let kept = std::mem::take(&mut *KEPT.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        drop(kept);
    });
}

/// Tracked pipeline creations this run and their summed time, microseconds
/// ([`pipeline_create_time`]).
static CREATE_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CREATE_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Tracked pipeline creations this run and the time spent in them, summed over the creating
/// threads (shader translation plus the driver compile, cache hits included): the per-pipeline
/// cost on this machine, logged next to the pipeline cache save.
pub(crate) fn pipeline_create_time() -> (u64, std::time::Duration) {
    use std::sync::atomic::Ordering;
    (
        CREATE_COUNT.load(Ordering::Relaxed),
        std::time::Duration::from_micros(CREATE_US.load(Ordering::Relaxed)),
    )
}

/// Whether `REGIA_PIPELINE_TIMING=1` is set (read once).
pub(crate) fn pipeline_timing_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| regia_core::brand::env_var("REGIA_PIPELINE_TIMING").is_ok_and(|v| v == "1"))
}

/// Process totals behind [`pipeline_timing_summary`]: `[count, nanoseconds]` per phase.
static PHASE_TOTALS: [[std::sync::atomic::AtomicU64; 2]; 4] =
    [const { [const { std::sync::atomic::AtomicU64::new(0) }; 2] }; 4];

/// A phase of a pipeline's CPU cost that [`pipeline_timing_summary`] totals.
#[derive(Clone, Copy)]
pub(crate) enum CostPhase {
    /// WGSL composition against the engine library (`shader_lib`).
    Compose = 0,
    /// `create_render_pipeline`: wgpu's per-stage naga work (override processing and the
    /// SPIR-V writer on Vulkan) plus the driver's compile.
    RenderPipeline = 1,
    /// `create_compute_pipeline`, same parts as [`Self::RenderPipeline`].
    ComputePipeline = 2,
    /// The part of [`Self::Compose`] spent on a source, file and define set this process
    /// had already composed (what a composed-module cache would save).
    ComposeRepeat = 3,
}

/// Adds the time since `start` to `phase`'s total when `REGIA_PIPELINE_TIMING=1`.
pub(crate) fn note_phase(phase: CostPhase, start: std::time::Instant) {
    if !pipeline_timing_enabled() {
        return;
    }
    let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let [count, total] = &PHASE_TOTALS[phase as usize];
    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    total.fetch_add(ns, std::sync::atomic::Ordering::Relaxed);
}

/// Totals of the pipeline cost phases since the process started, summed over every thread:
/// `(phase, count, total_ms)` for composition, render pipelines, compute pipelines and the
/// compositions that repeated an earlier one.
/// `None` unless `REGIA_PIPELINE_TIMING=1` (the totals are only kept then).
#[must_use]
pub fn pipeline_timing_summary() -> Option<Vec<(&'static str, u64, f64)>> {
    if !pipeline_timing_enabled() {
        return None;
    }
    let names = ["compose", "render_pipeline", "compute_pipeline", "compose_repeat"];
    Some(
        names
            .iter()
            .zip(&PHASE_TOTALS)
            .map(|(name, [count, total])| {
                let ms = total.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6;
                (*name, count.load(std::sync::atomic::Ordering::Relaxed), ms)
            })
            .collect(),
    )
}

/// Adds one creation to [`pipeline_create_time`]; `REGIA_PIPELINE_TIMING=1` also logs every
/// tracked pipeline creation slower than 5 ms with its label, thread and time (which compiles
/// a warm waits on).
fn note_pipeline_time(label: Option<&str>, start: std::time::Instant) {
    use std::sync::atomic::Ordering;
    let elapsed = start.elapsed();
    CREATE_COUNT.fetch_add(1, Ordering::Relaxed);
    CREATE_US.fetch_add(
        u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    if !pipeline_timing_enabled() {
        return;
    }
    let ms = elapsed.as_secs_f64() * 1e3;
    if ms > 5.0 {
        let thread = std::thread::current();
        tracing::info!(
            target: "regia_render",
            "pipeline {:?} {ms:.1} ms on {}",
            label.unwrap_or(""),
            thread.name().unwrap_or("?")
        );
        eprintln!(
            "PIPELINE_TIMING {ms:.1} ms {:?} on {}",
            label.unwrap_or(""),
            thread.name().unwrap_or("?")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-5423: Android takes the small blocks on every adapter, shipped or tested; desktop keeps
    /// wgpu's default outside this crate's unit tests on a hardware adapter, whose devices are
    /// kept until exit (ADR-1584, ADR-7531).
    #[test]
    fn android_asks_for_small_allocator_blocks_on_every_vendor() {
        let small = |h: wgpu::MemoryHints| matches!(h, wgpu::MemoryHints::MemoryUsage);
        let default = |h: wgpu::MemoryHints| matches!(h, wgpu::MemoryHints::Performance);
        for kept in [false, true] {
            assert!(small(memory_hints_for(kept, false, true)), "kept {kept}");
            assert!(small(memory_hints_for(kept, true, true)), "kept {kept}");
            assert!(default(memory_hints_for(kept, false, false)), "kept {kept}");
        }
        assert!(small(memory_hints_for(true, true, false)));
        assert!(default(memory_hints_for(false, true, false)));
    }

    /// ADR-7613: `REGIA_REQUIRE_ADAPTER` is on when set to anything but empty or `0`; unset leaves
    /// the error to the caller (the process-level panic is `tests/require_adapter.rs`).
    #[test]
    fn the_require_adapter_flag_reads_set_empty_and_zero() {
        use std::ffi::OsStr;
        assert!(!adapter_required(None));
        assert!(!adapter_required(Some(OsStr::new(""))));
        assert!(!adapter_required(Some(OsStr::new("0"))));
        assert!(adapter_required(Some(OsStr::new("1"))));
        assert!(adapter_required(Some(OsStr::new("yes"))));
    }

    /// ADR-7531: the unit tests keep their devices on every hardware adapter (NVIDIA, Intel, AMD,
    /// a mobile GPU), not only on NVIDIA as ADR-1584 did, and never on a software adapter.
    #[test]
    fn unit_tests_keep_devices_on_every_hardware_adapter() {
        const NVIDIA: u32 = 0x10DE;
        const INTEL: u32 = 0x8086;
        const AMD: u32 = 0x1002;
        const MESA_LAVAPIPE: u32 = 0x10005;
        let info = |vendor, device_type| wgpu::AdapterInfo {
            vendor,
            device_type,
            ..wgpu::AdapterInfo::new(wgpu::DeviceType::Other, wgpu::Backend::Vulkan)
        };
        for vendor in [NVIDIA, INTEL, AMD] {
            for device_type in [wgpu::DeviceType::DiscreteGpu, wgpu::DeviceType::IntegratedGpu] {
                assert!(
                    unit_tests_keep(&info(vendor, device_type)),
                    "{vendor:#x} {device_type:?}"
                );
            }
        }
        assert!(!unit_tests_keep(&info(MESA_LAVAPIPE, wgpu::DeviceType::Cpu)));
        // The shared test device went through `keep_for_test_run` when it opened: it is in the
        // kept list on a real card (the Intel Arc included) and not on lavapipe.
        let device = test_device();
        let live = device.adapter.get_info();
        let kept = KEPT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|(_, kept, _, _)| *kept == device.device);
        assert_eq!(
            kept,
            live.device_type != wgpu::DeviceType::Cpu,
            "{} ({:?}): kept {kept}",
            live.name,
            live.device_type
        );
    }

    /// The public wrappers a host's own window uses (the editor, ADR-2113 / ADR-2306) take the
    /// loader-list lock: each creation moves the process-wide count. Other tests create devices
    /// at the same time, so the check is "moved", never an exact delta.
    #[test]
    fn window_wrappers_take_the_loader_list_lock() {
        let before = loader_list_lock_count();
        // No backend: the wrapper takes the lock all the same, and no Vulkan instance is created
        // while other tests name objects (on a machine with a driver that has no device, that
        // creation frees the driver's loader entry under their walk, ADR-7557).
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::empty();
        let _instance = create_window_instance(descriptor);
        let after_instance = loader_list_lock_count();
        assert!(after_instance > before, "instance creation took the lock");
        let base = test_device();
        let info = base.adapter.get_info();
        let (device, queue) = request_device_locked(
            &base.adapter,
            &wgpu::DeviceDescriptor {
                label: Some("loader-lock wrapper test"),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: device_memory_hints(&info),
                ..Default::default()
            },
        )
        .expect("a second device on the test adapter");
        assert!(
            loader_list_lock_count() > after_instance,
            "device creation took the lock"
        );
        // A device a unit test opened on a hardware adapter stays alive until exit (ADR-1584,
        // ADR-7531).
        if unit_tests_keep(&info) {
            register_end_of_test_process();
            KEPT.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((queue, device, base.adapter.clone(), base.instance.clone()));
        }
    }

    /// How many instances `HEADLESS_INSTANCES` holds for `key`.
    fn cached_for(key: &HeadlessInstanceKey) -> usize {
        HEADLESS_INSTANCES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(cached, _)| cached == key)
            .count()
    }

    /// ADR-7557: a headless descriptor gets one instance per process, however many threads ask at
    /// once, so no instance is created (and no driver without a device is loaded and freed by the
    /// Vulkan loader) after the first. The descriptor is one no other test uses (a memory budget
    /// threshold of 97 %), so the count is exact while other tests run. It names no backend: a
    /// new Vulkan instance mid-run is the very list change this guards against, so the test makes
    /// none (the engine's own instance is checked in the next test).
    #[test]
    fn concurrent_headless_creations_share_one_instance() {
        let descriptor = || {
            let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
            descriptor.backends = wgpu::Backends::empty();
            descriptor.flags = instance_flags();
            descriptor.memory_budget_thresholds.for_resource_creation = Some(97);
            descriptor
        };
        let key = HeadlessInstanceKey::of(&descriptor(), true).expect("no display handle");
        let instances: Vec<wgpu::Instance> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| create_instance(descriptor())))
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().expect("creation thread"))
                .collect()
        });
        assert_eq!(instances.len(), 8);
        assert_eq!(cached_for(&key), 1, "eight creators, one instance");
        // A later caller gets it too.
        let _again = create_instance(descriptor());
        assert_eq!(cached_for(&key), 1);
    }

    /// ADR-7557: the engine's headless devices, `list_adapters` and `probe_adapters` all use the
    /// engine's one headless instance; a window's instance is never shared, and a descriptor that
    /// differs in anything but the display handle is another instance.
    #[test]
    fn engine_headless_paths_reuse_the_engine_instance() {
        let engine = {
            let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
            descriptor.backends = instance_backends();
            descriptor.flags = instance_flags();
            HeadlessInstanceKey::of(&descriptor, true).expect("no display handle")
        };
        let _device = test_device();
        assert_eq!(
            cached_for(&engine),
            1,
            "the test device's instance is the shared one"
        );
        let listed = crate::list_adapters();
        let probed = crate::hwrt::probe_adapters();
        assert_eq!(listed.len(), probed.len());
        assert!(!listed.is_empty(), "the test device's adapter is listed");
        assert_eq!(cached_for(&engine), 1, "listing and probing reused it");
        // A window's instance is created afresh (a host's window, ADR-2113), even for a descriptor
        // with no display handle. No backend, so no Vulkan instance is made mid-run.
        let before = instance_creation_count();
        let mut window = wgpu::InstanceDescriptor::new_without_display_handle();
        window.backends = wgpu::Backends::empty();
        window.flags = instance_flags();
        let window_key = HeadlessInstanceKey::of(&window, true).expect("no display handle");
        let _window = create_window_instance(window);
        assert!(instance_creation_count() > before, "a window instance is new");
        assert_eq!(cached_for(&window_key), 0, "and is not put in the cache");
        assert_eq!(cached_for(&engine), 1);
        // Other flags, backends or options are another key.
        let mut other = wgpu::InstanceDescriptor::new_without_display_handle();
        other.backends = instance_backends();
        other.flags = instance_flags() ^ wgpu::InstanceFlags::VALIDATION;
        assert_ne!(HeadlessInstanceKey::of(&other, true), Some(engine.clone()));
        other.flags = instance_flags();
        assert_eq!(HeadlessInstanceKey::of(&other, true), Some(engine.clone()));
        // The hook-free listing instance (ADR-7820) is the devices' one unless a vendor hook is built.
        assert_eq!(
            HeadlessInstanceKey::of(&other, false) == Some(engine.clone()),
            !cfg!(any(feature = "dlss", feature = "xess"))
        );
        other.backend_options.gl.fence_behavior = wgpu::GlFenceBehavior::AutoFinish;
        assert_ne!(HeadlessInstanceKey::of(&other, true), Some(engine));
    }

    /// Every tracked creation counts towards the per-pipeline time logged at the cache save,
    /// whether or not `REGIA_PIPELINE_TIMING` is set.
    #[test]
    fn tracked_pipeline_creations_are_timed() {
        let device = test_device();
        let module = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("timed"),
            source: wgpu::ShaderSource::Wgsl("@compute @workgroup_size(1) fn main() {}".into()),
        });
        let (count, time) = pipeline_create_time();
        let _pipeline = TrackedDevice::observe(&device.device).create_compute_pipeline(
            &wgpu::ComputePipelineDescriptor {
                label: Some("timed"),
                layout: None,
                module: &module,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                cache: None,
            },
        );
        let (after, after_time) = pipeline_create_time();
        assert!(after > count, "the creation was counted");
        assert!(after_time >= time);
    }

    #[test]
    fn software_renderer_fact_follows_the_adapter() {
        let device = test_device();
        let expected = device.adapter.get_info().device_type == wgpu::DeviceType::Cpu;
        let mut app = regia_app::App::new();
        app.add_systems(regia_app::PreUpdate, publish_software_renderer);
        app.update();
        assert!(
            app.world()
                .get_resource::<regia_platform::SoftwareRenderer>()
                .is_none(),
            "no device, no fact"
        );
        app.insert_resource(device);
        app.update();
        assert_eq!(
            app.world()
                .get_resource::<regia_platform::SoftwareRenderer>()
                .copied(),
            Some(regia_platform::SoftwareRenderer(expected)),
            "the fact matches the adapter's device type"
        );
    }

    #[test]
    fn backends_exclude_gl_everywhere() {
        let backends = instance_backends();
        assert!(
            !backends.contains(wgpu::Backends::GL),
            "no GL fallback (specs/13 § 2)"
        );
        assert!(!backends.is_empty(), "at least one backend must be requested");
        if cfg!(target_os = "windows") {
            assert!(backends.contains(wgpu::Backends::DX12));
        } else if cfg!(target_os = "macos") {
            assert_eq!(backends, wgpu::Backends::METAL, "Metal only on macOS");
        } else {
            assert_eq!(backends, wgpu::Backends::VULKAN, "Vulkan only on Linux/Android");
        }
    }

    /// Choosing an adapter records the GPU/driver for crash reports (specs/21 § 5), so a real
    /// report's `gpu:` line is never the `(not reported)` placeholder. Skips when no adapter is
    /// available (CI runs this on lavapipe).
    #[test]
    fn choosing_an_adapter_records_gpu_info_for_crash_reports() {
        let Ok(device) = RenderDevice::headless() else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        // Hold the device until the assertion so the adapter stays alive for the duration.
        let _device = device;
        let gpu = regia_platform::crash::gpu_info();
        assert!(
            !gpu.is_empty(),
            "the chosen adapter is recorded for crash reports"
        );
        assert_ne!(gpu, "(not reported)", "a placeholder is never recorded");
    }
}

#[cfg(test)]
mod baseline_feature_tests {
    use super::baseline_optional_features;

    /// Only what the adapter offers is requested, and a capable adapter gets every fallback feature.
    #[test]
    fn requests_exactly_the_offered_features() {
        assert!(baseline_optional_features(wgpu::Features::empty()).is_empty());
        let offered = wgpu::Features::TIMESTAMP_QUERY
            | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS
            | wgpu::Features::TEXTURE_COMPRESSION_BC
            | wgpu::Features::MULTI_DRAW_INDIRECT_COUNT
            | wgpu::Features::INDIRECT_FIRST_INSTANCE
            | wgpu::Features::RG11B10UFLOAT_RENDERABLE
            | wgpu::Features::FLOAT32_FILTERABLE
            | wgpu::Features::SHADER_EARLY_DEPTH_TEST;
        assert_eq!(
            baseline_optional_features(offered | wgpu::Features::PIPELINE_CACHE),
            offered
        );
    }
}

/// How a test process with kept devices ends (ADR-1907). Each test re-runs this binary, so the
/// real `exit` of the real harness is what gets checked, on any adapter and without a GPU race.
#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
mod exit_tests {
    use std::process::{Command, Output};

    use super::*;

    /// Tells [`child`] what to do; unset, it is an ordinary test that does nothing.
    const CHILD_ENV: &str = "REGIA_RENDER_EXIT_CHILD";

    /// Stands in for a driver's exit handler, registered before the end of the process like the
    /// handlers of a driver that loaded with the first device. It ends the process with SIGABRT,
    /// so a run in which any such handler runs cannot exit with a status.
    extern "C" fn driver_exit_handler() {
        std::process::abort();
    }

    #[test]
    fn child() {
        let Ok(mode) = regia_core::brand::env_var(CHILD_ENV) else {
            return;
        };
        assert!(crate::platform::at_exit(driver_exit_handler));
        // A real device stays alive into the exit, as a kept device does on a hardware adapter.
        match RenderDevice::headless() {
            Ok(device) => keep_device(&device),
            Err(_) => register_end_of_test_process(),
        }
        assert_ne!(mode, "fail", "the child was asked to fail");
    }

    fn run_child(mode: &str) -> Output {
        Command::new(std::env::current_exe().expect("test binary path"))
            .args(["--exact", "device::exit_tests::child", "--test-threads=1"])
            .env(CHILD_ENV, mode)
            .output()
            .expect("run the test binary again")
    }

    #[test]
    fn a_passing_run_exits_zero_without_the_driver_exit_handlers() {
        let run = run_child("pass");
        let log = String::from_utf8_lossy(&run.stdout);
        assert_eq!(run.status.code(), Some(0), "{:?}\n{log}", run.status);
        assert!(log.contains("1 passed"), "{log}");
    }

    #[test]
    fn a_failing_run_keeps_the_status_of_the_harness() {
        let run = run_child("fail");
        let log = String::from_utf8_lossy(&run.stdout);
        assert_eq!(run.status.code(), Some(101), "{:?}\n{log}", run.status);
        assert!(log.contains("1 failed"), "{log}");
    }
}
