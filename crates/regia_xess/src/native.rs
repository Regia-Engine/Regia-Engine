// SPDX-License-Identifier: MPL-2.0
//! External SDK FFI and wgpu Vulkan ownership boundary (ADR-4656).
#![allow(unsafe_code)]
use crate::{CreateDesc, Error, EvalInputs, OptimalSettings, Quality, Version};
use ash::{vk, vk::Handle};
use std::{
    ffi::{CStr, CString},
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, Mutex, Weak},
};
use wgpu::hal::api::Vulkan;

// Generated vendor declarations use vendor spelling and may contain unused APIs/types.
// These exceptions apply only to OUT_DIR, never to handwritten adapter code (ADR-4656).
#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    missing_docs,
    clippy::all,
    clippy::undocumented_unsafe_blocks,
    unsafe_op_in_unsafe_fn
)]
mod ffi {
    use ash::vk::{
        CommandBuffer as VkCommandBuffer, Device as VkDevice, DeviceMemory as VkDeviceMemory,
        Format as VkFormat, Image as VkImage, ImageSubresourceRange as VkImageSubresourceRange,
        ImageView as VkImageView, Instance as VkInstance, PhysicalDevice as VkPhysicalDevice,
        PipelineCache as VkPipelineCache,
    };
    include!(concat!(env!("OUT_DIR"), "/xess.rs"));
}

fn check(call: &'static str, result: i32) -> Result<(), Error> {
    if result >= 0 {
        if result > 0 {
            tracing::warn!(call, result, "XeSS SDK warning");
        }
        Ok(())
    } else {
        Err(Error::Sdk { call, result })
    }
}
fn backend(error: impl std::fmt::Display) -> Error {
    Error::Backend(error.to_string())
}

struct Library {
    worker: crate::worker::Worker<ffi::Api>,
    path: PathBuf,
    version: Version,
}
#[derive(Clone)]
pub(crate) struct Runtime(Arc<Library>);
static LIBRARY: Mutex<Weak<Library>> = Mutex::new(Weak::new());
impl Library {
    fn call<R: Send + 'static>(
        &self,
        work: impl FnOnce(&ffi::Api) -> Result<R, Error> + Send + 'static,
    ) -> Result<R, Error> {
        self.worker.call(work)
    }
}

// This marker is owned by the HAL device's drop callback, not by a global device clone.
// A context can only attach to devices this module created with the complete feature chain.
struct Prepared {
    library: Arc<Library>,
    /// Whether the device enabled `VK_EXT_zero_initialize_device_memory` (ADR-7820).
    zero_init: bool,
}
static PREPARED: Mutex<Vec<(u64, Weak<Prepared>)>> = Mutex::new(Vec::new());

struct RawOwner {
    device: ash::Device,
    _prepared: Arc<Prepared>,
}
impl Drop for RawOwner {
    fn drop(&mut self) {
        // SAFETY: owned raw device, called once after HAL cleanup or failed adoption.
        unsafe {
            self.device.destroy_device(None);
        }
    }
}

fn extension_names(count: u32, names: *const *const std::ffi::c_char) -> Result<Vec<&'static CStr>, Error> {
    if count > 1024 || (count != 0 && names.is_null()) {
        return Err(Error::InvalidInput);
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: XeSS owns the returned array with the reported count; caller holds the DLL.
    let pointers = unsafe { std::slice::from_raw_parts(names, count as usize) };
    pointers
        .iter()
        .map(|name| {
            if name.is_null() {
                return Err(Error::InvalidInput);
            }
            // SAFETY: XeSS returns nul-terminated Vulkan extension names.
            let name = unsafe { CStr::from_ptr(*name) };
            // HAL requires static extension names. Intern once per distinct extension, not per device.
            static INTERNED: Mutex<Vec<&'static CStr>> = Mutex::new(Vec::new());
            let mut interned = INTERNED.lock().map_err(backend)?;
            if let Some(existing) = interned.iter().find(|existing| **existing == name) {
                return Ok(*existing);
            }
            let name: &'static CStr = Box::leak(CString::from(name).into_boxed_c_str());
            interned.push(name);
            Ok(name)
        })
        .collect()
}
fn owned(names: &[&CStr]) -> Vec<String> {
    names.iter().map(|s| s.to_string_lossy().into_owned()).collect()
}

impl Runtime {
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        if !path.is_absolute() || !path.is_file() {
            return Err(Error::RuntimeNotFound);
        }
        let path = path.canonicalize().map_err(backend)?;
        let mut registry = LIBRARY.lock().map_err(backend)?;
        if let Some(library) = registry.upgrade() {
            if library.path != path {
                return Err(Error::Backend("another XeSS DLL is already loaded".into()));
            }
            return Ok(Self(library));
        }
        let load_path = path.clone();
        let (worker, version) = crate::worker::Worker::spawn(move || {
            // SAFETY: owner-approved external DLL at an absolute path; exact generated signatures.
            let api = unsafe { ffi::Api::new(load_path) }.map_err(backend)?;
            let mut v = ffi::xess_version_t::default();
            // SAFETY: writable output, library loaded on this dedicated SDK thread.
            check("xessGetVersion", unsafe { (api.xessGetVersion)(&mut v) })?;
            let version = Version {
                major: v.major,
                minor: v.minor,
                patch: v.patch,
            };
            if !version.supported() {
                return Err(Error::UnsupportedVersion);
            }
            Ok((api, version))
        })?;
        let library = Arc::new(Library {
            worker,
            path,
            version,
        });
        *registry = Arc::downgrade(&library);
        Ok(Self(library))
    }

    pub(crate) fn version(&self) -> Version {
        self.0.version
    }
    fn instance_requirements(&self) -> Result<(Vec<&'static CStr>, u32), Error> {
        self.0.call(|api| {
            let (mut count, mut names, mut version) = (0, ptr::null(), 0);
            // SAFETY: writable out parameters; copy names before another SDK call.
            check("xessVKGetRequiredInstanceExtensions", unsafe {
                (api.xessVKGetRequiredInstanceExtensions)(&mut count, &mut names, &mut version)
            })?;
            Ok((extension_names(count, names)?, version))
        })
    }

    pub(crate) fn create_instance(
        &self,
        desc: &wgpu::InstanceDescriptor,
    ) -> Result<(wgpu::Instance, Vec<String>), Error> {
        let (names, version) = self.instance_requirements()?;
        // wgpu-hal 30 requests Vulkan 1.3. Do not claim a newer version on its behalf.
        if version > vk::API_VERSION_1_3 {
            return Err(Error::UnsupportedVersion);
        }
        let descriptor = wgpu::hal::InstanceDescriptor {
            name: "regia-xess",
            flags: desc.flags,
            memory_budget_thresholds: desc.memory_budget_thresholds,
            backend_options: desc.backend_options.clone(),
            telemetry: None,
            display: None,
        };
        let mut supported = false;
        let callback: Box<wgpu::hal::vulkan::CreateInstanceCallback> = Box::new(|args| {
            // SAFETY: ordinary instance extension enumeration through wgpu's loaded entry.
            if let Ok(extensions) = unsafe { args.entry.enumerate_instance_extension_properties(None) } {
                supported = names.iter().all(|n| {
                    extensions
                        .iter()
                        .any(|e| e.extension_name_as_c_str().is_ok_and(|s| s == *n))
                });
                if supported {
                    for name in &names {
                        if !args.extensions.contains(name) {
                            args.extensions.push(name);
                        }
                    }
                }
            }
        });
        // SAFETY: callback only adds supported extensions and never removes wgpu requirements.
        let hal = unsafe { wgpu::hal::vulkan::Instance::init_with_callback(&descriptor, Some(callback)) }
            .map_err(backend)?;
        if !supported {
            return Err(Error::DeviceNotPrepared);
        }
        // SAFETY: fully initialized HAL instance, ownership moves to wgpu.
        Ok((unsafe { wgpu::Instance::from_hal::<Vulkan>(hal) }, owned(&names)))
    }
    pub(crate) fn request_device(
        &self,
        adapter: &wgpu::Adapter,
        desc: &wgpu::DeviceDescriptor<'_>,
    ) -> Result<(wgpu::Device, wgpu::Queue, Vec<String>), Error> {
        // SAFETY: guard is held until the raw device is adopted; adapter stays alive.
        let hal = unsafe { adapter.as_hal::<Vulkan>() }.ok_or(Error::NotVulkan)?;
        let instance = hal.shared_instance();
        let (required, version) = self.instance_requirements()?;
        if version > vk::API_VERSION_1_3 || required.iter().any(|name| !instance.extensions().contains(name))
        {
            return Err(Error::DeviceNotPrepared);
        }
        let physical = hal.raw_physical_device();
        let raw_instance = instance.raw_instance();
        let instance_handle = raw_instance.handle();
        let required = self.0.call(move |api| {
            let (mut count, mut names) = (0, ptr::null());
            // SAFETY: caller retains the live instance/adapter until this synchronous call returns.
            check("xessVKGetRequiredDeviceExtensions", unsafe {
                (api.xessVKGetRequiredDeviceExtensions)(instance_handle, physical, &mut count, &mut names)
            })?;
            extension_names(count, names)
        })?;
        if required
            .iter()
            .any(|name| !hal.physical_device_capabilities().supports_extension(name))
        {
            return Err(Error::DeviceNotPrepared);
        }
        let mut extensions = hal.required_device_extensions(desc.required_features);
        for name in &required {
            if !extensions.contains(name) {
                extensions.push(name);
            }
        }
        // Driver-zeroed temporary heaps (ADR-7820): enable the extension where the GPU offers it.
        let mut zero_init_features = crate::zeroing::ZeroInitFeatures::default();
        let zero_init = hal
            .physical_device_capabilities()
            .supports_extension(crate::zeroing::ZERO_INIT_EXTENSION)
            && {
                let mut query = crate::zeroing::ZeroInitFeatures::default();
                let mut features2 = vk::PhysicalDeviceFeatures2 {
                    p_next: ptr::from_mut(&mut query).cast(),
                    ..Default::default()
                };
                // SAFETY: a Vulkan 1.1+ instance (wgpu-hal requires it) and a valid chain of one
                // node laid out as the registry's structure.
                unsafe { raw_instance.get_physical_device_features2(physical, &mut features2) };
                query.zero_initialize_device_memory == vk::TRUE
            };
        if zero_init && !extensions.contains(&crate::zeroing::ZERO_INIT_EXTENSION) {
            extensions.push(crate::zeroing::ZERO_INIT_EXTENSION);
        }
        let mut features = hal.physical_device_features(&extensions, desc.required_features);
        // Use HAL's graphics/compute queue family convention (wgpu-hal 30 uses family 0).
        let families = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(0)
            .queue_priorities(&[1.0])];
        let pointers: Vec<_> = extensions.iter().map(|n| n.as_ptr()).collect();
        let mut info = features.add_to_device_create(
            vk::DeviceCreateInfo::default()
                .queue_create_infos(&families)
                .enabled_extension_names(&pointers),
        );
        // XeSS requires Features2 instead of the const pEnabledFeatures field. Preserve every
        // HAL feature node, then let Intel patch that chain to avoid duplicate structure types.
        let mut core = vk::PhysicalDeviceFeatures2::default().features(features_core(&info)?);
        core.p_next = info.p_next.cast_mut();
        if zero_init {
            zero_init_features.zero_initialize_device_memory = vk::TRUE;
            zero_init_features.p_next = core.p_next;
            core.p_next = ptr::from_mut(&mut zero_init_features).cast();
        }
        info.p_enabled_features = ptr::null();
        let address = ptr::from_mut(&mut core) as usize;
        let info_address = ptr::from_ref(&info) as usize;
        let create_instance = raw_instance.clone();
        let raw = self.0.call(move |api| {
            let mut chain = address as *mut std::ffi::c_void;
            // SAFETY: caller blocks until this closure returns; its complete writable feature
            // chain remains in place and untouched until the result arrives. XeSS owns additions.
            check("xessVKGetRequiredDeviceFeatures", unsafe {
                (api.xessVKGetRequiredDeviceFeatures)(instance_handle, physical, &mut chain)
            })?;
            // SAFETY: the caller retains info and its referenced arrays for this synchronous call.
            // Create the device before another SDK call can replace SDK-owned chain nodes.
            let mut info = unsafe { *(info_address as *const vk::DeviceCreateInfo<'_>) };
            info.p_next = chain;
            // SAFETY: complete HAL + XeSS requirements and live instance/physical device.
            unsafe { create_instance.create_device(physical, &info, None) }.map_err(backend)
        })?;
        let handle = raw.handle().as_raw();
        tracing::debug!(zero_init, "XeSS device created");
        let prepared = Arc::new(Prepared {
            library: Arc::clone(&self.0),
            zero_init,
        });
        let owner = RawOwner {
            device: raw.clone(),
            _prepared: Arc::clone(&prepared),
        };
        {
            let mut registry = PREPARED.lock().map_err(backend)?;
            registry.retain(|(_, entry)| entry.strong_count() != 0);
            registry.push((handle, Arc::downgrade(&prepared)));
        }
        // SAFETY: raw was created from this adapter with the complete HAL requirements, family 0.
        // The drop callback retains the DLL/marker until HAL has released all device resources.
        let open = unsafe {
            hal.device_from_raw(
                raw,
                Some(Box::new(move || drop(owner))),
                &extensions,
                desc.required_features,
                &desc.required_limits,
                &desc.memory_hints,
                0,
                0,
            )
        }
        .map_err(backend)?;
        drop(hal);
        // SAFETY: open was created on this exact adapter and matches desc.
        let (device, queue) =
            unsafe { adapter.create_device_from_hal::<Vulkan>(open, desc) }.map_err(backend)?;
        Ok((device, queue, owned(&required)))
    }
    pub(crate) fn context(&self, device: &wgpu::Device) -> Result<Context, Error> {
        // SAFETY: guarded reads only, no Vulkan work while holding the snatch lock.
        let (instance, physical, raw) = unsafe {
            device.as_hal::<Vulkan>().map(|hal| {
                (
                    hal.shared_instance().raw_instance().handle(),
                    hal.raw_physical_device(),
                    hal.raw_device().handle(),
                )
            })
        }
        .ok_or(Error::NotVulkan)?;
        let prepared = PREPARED
            .lock()
            .map_err(backend)?
            .iter()
            .find(|(handle, _)| *handle == raw.as_raw())
            .and_then(|(_, p)| p.upgrade())
            .ok_or(Error::DeviceNotPrepared)?;
        let context = prepared.library.call(move |api| {
            let mut context = ptr::null_mut();
            // SAFETY: caller retains the prepared device and instance for this synchronous call.
            check("xessVKCreateContext", unsafe {
                (api.xessVKCreateContext)(instance, physical, raw, &mut context)
            })?;
            if context.is_null() {
                return Err(Error::InvalidInput);
            }
            Ok(context as usize)
        })?;
        Ok(Context {
            shared: Arc::new(Shared {
                prepared,
                device: device.clone(),
                handle: context,
                temp_heaps: Mutex::new(Vec::new()),
                fill_aliases: Mutex::new(Vec::new()),
            }),
            desc: None,
            settings: None,
            retention_layouts: [None, None],
            retained: None,
            motion: None,
            zeroing: None,
            pending_fills: Vec::new(),
            pacer: crate::pacing::Pacer::new(crate::pacing::MAX_AHEAD),
        })
    }
}
fn features_core(info: &vk::DeviceCreateInfo<'_>) -> Result<vk::PhysicalDeviceFeatures, Error> {
    if info.p_enabled_features.is_null() {
        return Err(Error::InvalidInput);
    }
    // SAFETY: pointer was just populated by HAL from its live PhysicalDeviceFeatures.
    Ok(unsafe { *info.p_enabled_features })
}
struct Shared {
    prepared: Arc<Prepared>,
    device: wgpu::Device,
    handle: usize,
    /// App-owned zeroed temporary heaps (raw `VkDeviceMemory`), freed after the context.
    temp_heaps: Mutex<Vec<u64>>,
    /// Transfer buffers bound over device-filled heaps (raw `VkBuffer`), destroyed before them.
    fill_aliases: Mutex<Vec<u64>>,
}
impl Drop for Shared {
    fn drop(&mut self) {
        // Leases keep Shared alive until recorded buffers were submitted/discarded. Wait without
        // the SDK lock so GPU completion cannot deadlock another call.
        if let Err(error) = self.device.poll(wgpu::PollType::wait_indefinitely()) {
            tracing::warn!(%error, "XeSS device wait failed during teardown");
        }
        let handle = self.handle;
        if let Err(error) = self.prepared.library.call(move |api| {
            // SAFETY: last context owner; GPU completion awaited; destruction runs on SDK thread.
            check("xessDestroyContext", unsafe {
                (api.xessDestroyContext)(handle as _)
            })
        }) {
            tracing::warn!(%error, "XeSS context destruction failed");
        }
        let heaps = std::mem::take(
            self.temp_heaps
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let aliases = std::mem::take(
            self.fill_aliases
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if heaps.is_empty() && aliases.is_empty() {
            return;
        }
        // SAFETY: guard only reads the dispatch table.
        let Some(raw) = (unsafe { self.device.as_hal::<Vulkan>().map(|h| h.raw_device().clone()) }) else {
            return;
        };
        for alias in aliases {
            // SAFETY: the fill that used this buffer completed (device idle); its memory is freed below.
            unsafe { raw.destroy_buffer(vk::Buffer::from_raw(alias), None) };
        }
        for heap in heaps {
            // SAFETY: the context that used this memory is destroyed and the device is idle.
            unsafe { raw.free_memory(vk::DeviceMemory::from_raw(heap), None) };
        }
    }
}

/// One app-owned temporary heap and how it is zeroed.
struct Heap {
    memory: vk::DeviceMemory,
    /// A transfer buffer over the whole heap that the first evaluate fills with zeros, or null.
    fill: vk::Buffer,
    how: crate::HeapZeroing,
}

/// Allocates `size` bytes of zeroed device-local memory usable for buffers or (with `image`)
/// optimal-tiling images, for XeSS's optional temporary heaps (ADR-4907). Host-visible memory is
/// zeroed by the CPU; where no such type takes the heap's resources (an NVIDIA card without
/// Resizable BAR has none for images), the driver zeroes it (`zero_init`) or the first evaluate
/// fills it on the GPU (ADR-7820). `allowed` limits the methods tried.
fn zeroed_heap(
    device: &wgpu::Device,
    size: u64,
    image: bool,
    zero_init: bool,
    allowed: crate::zeroing::Allowed,
) -> Result<Heap, Error> {
    use crate::HeapZeroing as How;
    if size == 0 {
        return Ok(Heap {
            memory: vk::DeviceMemory::null(),
            fill: vk::Buffer::null(),
            how: How::HostWrite,
        });
    }
    // SAFETY: the guard only reads handles and the physical device's memory properties.
    let (raw, properties) = unsafe {
        device.as_hal::<Vulkan>().map(|h| {
            let instance = h.shared_instance().raw_instance();
            (
                h.raw_device().clone(),
                instance.get_physical_device_memory_properties(h.raw_physical_device()),
            )
        })
    }
    .ok_or(Error::NotVulkan)?;
    // SAFETY: a short-lived probe object, destroyed before return; only its requirements are read.
    let bits = unsafe {
        if image {
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R16G16B16A16_SFLOAT)
                .extent(vk::Extent3D {
                    width: 64,
                    height: 64,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            let probe = raw.create_image(&info, None).map_err(backend)?;
            let bits = raw.get_image_memory_requirements(probe).memory_type_bits;
            raw.destroy_image(probe, None);
            bits
        } else {
            let info = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::UNIFORM_BUFFER);
            let probe = raw.create_buffer(&info, None).map_err(backend)?;
            let bits = raw.get_buffer_memory_requirements(probe).memory_type_bits;
            raw.destroy_buffer(probe, None);
            bits
        }
    };
    let allocate = |index: u32, flags: vk::MemoryAllocateFlags| {
        let mut flags_info = vk::MemoryAllocateFlagsInfo::default().flags(flags);
        let mut info = vk::MemoryAllocateInfo::default()
            .allocation_size(size)
            .memory_type_index(index);
        if !flags.is_empty() {
            info = info.push_next(&mut flags_info);
        }
        // SAFETY: a valid type index of this device; the flag needs the feature `zero_init` enabled.
        unsafe { raw.allocate_memory(&info, None) }
    };
    let mut refusals = Vec::new();
    if allowed.allows(How::HostWrite) {
        match crate::zeroing::host_write_type(&properties, bits) {
            Some(index) => match allocate(index, vk::MemoryAllocateFlags::empty()) {
                Ok(memory) => {
                    let length = usize::try_from(size).map_err(backend)?;
                    // SAFETY: fresh allocation of a host-visible type; mapped, zeroed over its whole
                    // size and unmapped before any GPU use. Coherent memory needs no flush.
                    match unsafe { raw.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) } {
                        Ok(pointer) => {
                            // SAFETY: the mapping covers `length` bytes.
                            unsafe {
                                ptr::write_bytes(pointer.cast::<u8>(), 0, length);
                                raw.unmap_memory(memory);
                            }
                            return Ok(Heap {
                                memory,
                                fill: vk::Buffer::null(),
                                how: How::HostWrite,
                            });
                        }
                        Err(error) => {
                            // SAFETY: never used by the GPU.
                            unsafe { raw.free_memory(memory, None) };
                            refusals.push(format!("host write: map {error}"));
                        }
                    }
                }
                Err(error) => refusals.push(format!("host write: {error}")),
            },
            None => refusals.push("host write: no host-visible device-local type".into()),
        }
    }
    if allowed.allows(How::DriverZeroed) {
        if !zero_init {
            refusals.push("driver: VK_EXT_zero_initialize_device_memory not enabled".into());
        } else if let Some(index) = crate::zeroing::device_type(&properties, bits) {
            match allocate(index, crate::zeroing::ALLOCATE_ZERO_INITIALIZE) {
                Ok(memory) => {
                    return Ok(Heap {
                        memory,
                        fill: vk::Buffer::null(),
                        how: How::DriverZeroed,
                    });
                }
                Err(error) => refusals.push(format!("driver: {error}")),
            }
        } else {
            refusals.push("driver: no device-local type".into());
        }
    }
    if allowed.allows(How::DeviceFill) {
        // SAFETY: a live device; probe only.
        let alias_bits = unsafe { crate::zeroing::transfer_buffer_bits(&raw, size) }.map_err(backend)?;
        match crate::zeroing::device_type(&properties, bits & alias_bits) {
            Some(index) => match allocate(index, vk::MemoryAllocateFlags::empty()) {
                // SAFETY: the memory is `size` bytes of a type the transfer buffer allows.
                Ok(memory) => match unsafe { crate::zeroing::alias_buffer(&raw, memory, size) } {
                    Ok(fill) => {
                        return Ok(Heap {
                            memory,
                            fill,
                            how: How::DeviceFill,
                        });
                    }
                    Err(error) => {
                        // SAFETY: never used by the GPU.
                        unsafe { raw.free_memory(memory, None) };
                        refusals.push(format!("fill: {error}"));
                    }
                },
                Err(error) => refusals.push(format!("fill: {error}")),
            },
            None => refusals.push("fill: no device-local type buffers and the heap share".into()),
        }
    }
    Err(Error::Backend(format!(
        "no zeroed XeSS {} heap ({})",
        if image { "texture" } else { "buffer" },
        refusals.join("; ")
    )))
}
pub(crate) struct Context {
    shared: Arc<Shared>,
    desc: Option<CreateDesc>,
    settings: Option<OptimalSettings>,
    retention_layouts: [Option<wgpu::BindGroupLayout>; 2],
    /// Last frame's lifetime anchor. Reused only when `reuse_retention` accepts the
    /// same view objects and depth layout.
    retained: Option<Retained>,
    motion: Option<crate::motion::Motion>,
    /// How initialization zeroed `[buffer heap, texture heap]`; `None` on SDK allocations.
    zeroing: Option<[crate::HeapZeroing; 2]>,
    /// Device-filled heaps' transfer buffers; the next evaluate records their zero fill first.
    pending_fills: Vec<vk::Buffer>,
    /// Keeps this context's recording at most `pacing::MAX_AHEAD` executes ahead (ADR-7695).
    pacer: crate::pacing::Pacer,
}
struct Retained {
    views: [wgpu::TextureView; 4],
    depth: bool,
    group: wgpu::BindGroup,
}
pub(crate) struct Lease {
    _shared: Arc<Shared>,
}
impl Context {
    pub(crate) fn optimal_settings(
        &self,
        display: [u32; 2],
        quality: Quality,
    ) -> Result<OptimalSettings, Error> {
        let handle = self.shared.handle;
        let result = self.shared.prepared.library.call(move |api| {
            let output = ffi::xess_2d_t {
                x: display[0],
                y: display[1],
            };
            let (mut render, mut min, mut max) = (
                ffi::xess_2d_t::default(),
                ffi::xess_2d_t::default(),
                ffi::xess_2d_t::default(),
            );
            // SAFETY: caller retains context, outputs are writable and preset is valid.
            check("xessGetOptimalInputResolution", unsafe {
                (api.xessGetOptimalInputResolution)(
                    handle as _,
                    &output,
                    quality.sdk_value() as _,
                    &mut render,
                    &mut min,
                    &mut max,
                )
            })?;
            Ok(OptimalSettings {
                render: [render.x, render.y],
                min: [min.x, min.y],
                max: [max.x, max.y],
            })
        })?;
        if (0..2).any(|i| {
            result.min[i] == 0
                || result.min[i] > result.render[i]
                || result.render[i] > result.max[i]
                || result.max[i] > display[i]
        }) {
            return Err(Error::InvalidInput);
        }
        Ok(result)
    }
    /// Whether initialization gave XeSS app-owned zero-initialized temporary heaps.
    pub(crate) fn zeroed_temp_heaps(&self) -> bool {
        self.zeroing.is_some()
    }
    pub(crate) fn temp_heap_zeroing(&self) -> Option<[crate::HeapZeroing; 2]> {
        self.zeroing
    }
    /// Queries XeSS's temporary heap sizes and allocates both zeroed; the context frees them.
    fn zeroed_temp_heaps_for(&self, display: [u32; 2]) -> Result<[Heap; 2], Error> {
        let handle = self.shared.handle;
        let (buffer_size, texture_size) = self.shared.prepared.library.call(move |api| {
            let output = ffi::xess_2d_t {
                x: display[0],
                y: display[1],
            };
            let mut properties = ffi::xess_properties_t::default();
            // SAFETY: caller retains context; outputs are writable.
            check("xessGetProperties", unsafe {
                (api.xessGetProperties)(handle as _, &output, &mut properties)
            })?;
            Ok((properties.tempBufferHeapSize, properties.tempTextureHeapSize))
        })?;
        let allowed = crate::zeroing::Allowed::from_env();
        let zero_init = self.shared.prepared.zero_init;
        let mut heaps = self.shared.temp_heaps.lock().map_err(backend)?;
        let mut aliases = self.shared.fill_aliases.lock().map_err(backend)?;
        let mut keep = |heap: &Heap| {
            if heap.memory != vk::DeviceMemory::null() {
                heaps.push(heap.memory.as_raw());
            }
            if heap.fill != vk::Buffer::null() {
                aliases.push(heap.fill.as_raw());
            }
        };
        let buffer = zeroed_heap(&self.shared.device, buffer_size, false, zero_init, allowed)?;
        keep(&buffer);
        let texture = zeroed_heap(&self.shared.device, texture_size, true, zero_init, allowed)?;
        keep(&texture);
        tracing::debug!(
            buffer_size,
            texture_size,
            buffer = buffer.how.code(),
            texture = texture.how.code(),
            "XeSS zeroed temporary heaps"
        );
        Ok([buffer, texture])
    }
    pub(crate) fn initialize(&mut self, desc: CreateDesc) -> Result<(), Error> {
        let _span = tracing::info_span!("xess_initialize").entered();
        if self.desc.is_some() {
            return Err(Error::InvalidInput);
        }
        let settings = self.optimal_settings(desc.display, desc.quality)?;
        let mut flags: ffi::xess_init_flags_t = 0;
        if desc.depth_inverted {
            flags |= ffi::XESS_INIT_FLAG_INVERTED_DEPTH;
        }
        if !desc.hdr {
            flags |= ffi::XESS_INIT_FLAG_LDR_INPUT_COLOR;
        }
        if desc.motion_jittered {
            flags |= ffi::XESS_INIT_FLAG_JITTERED_MV;
        }
        let display = desc.display;
        let quality = desc.quality.sdk_value();
        let params = move |[buffer, texture]: [vk::DeviceMemory; 2]| ffi::xess_vk_init_params_t {
            outputResolution: ffi::xess_2d_t {
                x: display[0],
                y: display[1],
            },
            qualitySetting: quality as _,
            initFlags: flags as u32,
            tempBufferHeap: buffer,
            tempTextureHeap: texture,
            ..Default::default()
        };
        let handle = self.shared.handle;
        // XeSS reads parts of its temporary storage on a reset frame before writing them, so SDK
        // allocations (reused, uninitialized memory) made identical inputs give different
        // outputs. App-owned zero-initialized temporary heaps make the output repeatable
        // (ADR-4907). Without a suitable memory type, or if the SDK refuses them, fall back to
        // the SDK's own allocations and say so.
        let zeroed = match self.zeroed_temp_heaps_for(desc.display) {
            Ok(heaps) => Some(heaps),
            Err(error) => {
                tracing::warn!(%error, "XeSS zeroed temporary heaps unavailable; SDK allocates its own");
                None
            }
        };
        let heaps = zeroed.as_ref().map(|[b, t]| [b.memory, t.memory]);
        let internal = [vk::DeviceMemory::null(); 2];
        let first = self.shared.prepared.library.call(move |api| {
            let params = params(heaps.unwrap_or(internal));
            // SAFETY: caller retains context, params owned by the SDK-thread job; any external
            // heaps stay allocated until the context is destroyed.
            check("xessVKInit", unsafe { (api.xessVKInit)(handle as _, &params) })
        });
        match (first, zeroed) {
            (Ok(()), zeroed) => {
                self.zeroing = zeroed.as_ref().map(|[b, t]| [b.how, t.how]);
                self.pending_fills = zeroed
                    .iter()
                    .flatten()
                    .map(|heap| heap.fill)
                    .filter(|fill| *fill != vk::Buffer::null())
                    .collect();
            }
            (Err(error), Some(_)) => {
                tracing::warn!(%error, "XeSS refused zeroed temporary heaps; retrying with SDK allocations");
                self.shared.prepared.library.call(move |api| {
                    let params = params(internal);
                    // SAFETY: as above; the failed initialization left no pending GPU work.
                    check("xessVKInit", unsafe { (api.xessVKInit)(handle as _, &params) })
                })?;
                self.zeroing = None;
            }
            (Err(error), None) => return Err(error),
        }
        self.desc = Some(desc);
        self.settings = Some(settings);
        Ok(())
    }
    pub(crate) fn evaluate(
        &mut self,
        device: &wgpu::Device,
        before: &mut wgpu::CommandEncoder,
        inputs: &EvalInputs<'_>,
    ) -> Result<(wgpu::CommandBuffer, Lease), Error> {
        let _span = tracing::info_span!("xess_evaluate").entered();
        let desc = self.desc.ok_or(Error::InvalidInput)?;
        let settings = self.settings.ok_or(Error::InvalidInput)?;
        if *device != self.shared.device
            || !inputs.exposure.is_finite()
            || inputs.exposure <= 0.0
            || inputs.jitter.iter().any(|v| !v.is_finite() || v.abs() > 0.5)
            || inputs.motion_scale.iter().any(|v| !v.is_finite())
            || (0..2)
                .any(|i| inputs.render_size[i] < settings.min[i] || inputs.render_size[i] > settings.max[i])
        {
            return Err(Error::InvalidInput);
        }
        for (view, size, formats, usage) in [
            (
                inputs.color,
                inputs.render_size,
                &[wgpu::TextureFormat::Rgba16Float][..],
                wgpu::TextureUsages::TEXTURE_BINDING,
            ),
            (
                inputs.motion,
                inputs.render_size,
                &[wgpu::TextureFormat::Rg16Float, wgpu::TextureFormat::Rgba16Float][..],
                wgpu::TextureUsages::TEXTURE_BINDING,
            ),
            (
                inputs.depth,
                inputs.render_size,
                &[wgpu::TextureFormat::Depth32Float, wgpu::TextureFormat::R32Float][..],
                wgpu::TextureUsages::TEXTURE_BINDING,
            ),
            (
                inputs.output,
                desc.display,
                &[wgpu::TextureFormat::Rgba16Float][..],
                wgpu::TextureUsages::STORAGE_BINDING,
            ),
        ] {
            let t = view.texture();
            if t.dimension() != wgpu::TextureDimension::D2
                || t.width() != size[0]
                || t.height() != size[1]
                || t.depth_or_array_layers() != 1
                || t.sample_count() != 1
                || t.mip_level_count() != 1
                || !formats.contains(&t.format())
                || !t.usage().contains(usage)
            {
                return Err(Error::InvalidInput);
            }
        }
        if [inputs.color, inputs.depth, inputs.motion]
            .iter()
            .any(|v| v.texture() == inputs.output.texture())
        {
            return Err(Error::InvalidInput);
        }
        // XeSS corrupts queued frames when too many executes are outstanding (ADR-7695).
        self.pacer.pace(device, before);
        let converted_motion =
            (inputs.motion.texture().format() == wgpu::TextureFormat::Rgba16Float).then(|| {
                self.motion
                    .get_or_insert_with(|| crate::motion::Motion::new(device))
                    .record(device, before, inputs.motion)
            });
        let motion = converted_motion.as_ref().unwrap_or(inputs.motion);
        let params = ffi::xess_vk_execute_params_t {
            colorTexture: resource(inputs.color)?,
            velocityTexture: resource(motion)?,
            depthTexture: resource(inputs.depth)?,
            outputTexture: resource(inputs.output)?,
            jitterOffsetX: inputs.jitter[0],
            jitterOffsetY: inputs.jitter[1],
            exposureScale: inputs.exposure,
            resetHistory: u32::from(inputs.reset),
            inputWidth: inputs.render_size[0],
            inputHeight: inputs.render_size[1],
            ..Default::default()
        };
        let barriers = [inputs.color, inputs.depth, motion].map(|v| wgpu::TextureTransition {
            texture: v.texture(),
            selector: None,
            state: wgpu::TextureUses::RESOURCE,
        });
        before.transition_resources(
            std::iter::empty(),
            barriers.into_iter().chain([wgpu::TextureTransition {
                texture: inputs.output.texture(),
                selector: None,
                state: wgpu::TextureUses::STORAGE_READ_WRITE,
            }]),
        );
        let handle = self.shared.handle;
        let motion_scale = inputs.motion_scale;
        // Raw Vulkan recording does not register views with wgpu. An otherwise empty compute
        // pass in the preceding buffer records this bind group in wgpu's lifetime tracker,
        // including all views and destroyed-resource checks, even without a dispatch.
        // Both buffers must be submitted in one batch: wgpu retains their trackers until the
        // complete submission finishes. The raw encoder cannot contain any wgpu API commands.
        let depth = inputs.depth.texture().format().has_depth_aspect();
        let views = [inputs.color, motion, inputs.depth, inputs.output];
        let (same_depth_layout, same_views) = match &self.retained {
            Some(cached) => (
                cached.depth == depth,
                std::array::from_fn(|index| cached.views[index] == *views[index]),
            ),
            None => (false, [false; 4]),
        };
        // The layout borrow ends inside this block, before `self.retained` is stored.
        if !crate::retain::reuse_retention(same_depth_layout, same_views) {
            let owned = views.map(wgpu::TextureView::clone);
            let entries = std::array::from_fn::<_, 4, _>(|index| wgpu::BindGroupEntry {
                binding: index as u32,
                resource: wgpu::BindingResource::TextureView(&owned[index]),
            });
            let group = {
                let layout = self.retention_layouts[usize::from(depth)]
                    .get_or_insert_with(|| retention_layout(device, depth));
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("xess retained views"),
                    layout,
                    entries: &entries,
                })
            };
            self.retained = Some(Retained {
                views: owned,
                depth,
                group,
            });
        }
        let Some(retained) = &self.retained else {
            return Err(Error::Backend("XeSS retention group missing".into()));
        };
        {
            let mut pass = before.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("xess retain resources"),
                timestamp_writes: None,
            });
            pass.set_bind_group(0, &retained.group, &[]);
        }
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("xess evaluate"),
        });
        // SAFETY: read the dispatch table under a short HAL guard, before locking the encoder.
        let raw_device =
            unsafe { device.as_hal::<Vulkan>().map(|h| h.raw_device().clone()) }.ok_or(Error::NotVulkan)?;
        // Device-filled heaps are zeroed at the start of the first execute's buffer (ADR-7820).
        let fills = std::mem::take(&mut self.pending_fills);
        // SAFETY: this encoder records only XeSS commands and the heap fills (their buffers live
        // until the context is destroyed after a device wait). Input handles are live; the preceding
        // encoder retains/transitions resources and must be submitted first. Lease retains context.
        let result = unsafe {
            encoder.as_hal_mut::<Vulkan, _, _>(|hal| match hal {
                Some(hal) => {
                    for fill in &fills {
                        crate::zeroing::record_fill(&raw_device, hal.raw_handle(), *fill, 0);
                    }
                    let is_depth = inputs.depth.texture().format().has_depth_aspect();
                    let depth_image = params.depthTexture.image;
                    let depth_range = params.depthTexture.subresourceRange;
                    let transition = |old_layout, new_layout| {
                        let barrier = vk::ImageMemoryBarrier::default()
                            .src_access_mask(vk::AccessFlags::SHADER_READ)
                            .dst_access_mask(vk::AccessFlags::SHADER_READ)
                            .old_layout(old_layout)
                            .new_layout(new_layout)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .image(depth_image)
                            .subresource_range(depth_range);
                        raw_device.cmd_pipeline_barrier(
                            hal.raw_handle(),
                            vk::PipelineStageFlags::ALL_COMMANDS,
                            vk::PipelineStageFlags::ALL_COMMANDS,
                            vk::DependencyFlags::empty(),
                            &[],
                            &[],
                            &[barrier],
                        );
                    };
                    if is_depth {
                        transition(
                            vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        );
                    }
                    let command = hal.raw_handle();
                    let execute = params;
                    let result = self.shared.prepared.library.call(move |api| {
                        // SAFETY: caller keeps resources, encoder and context live while waiting;
                        // the SDK thread has exclusive use of the raw command buffer during this call.
                        check(
                            "xessSetVelocityScale",
                            (api.xessSetVelocityScale)(handle as _, motion_scale[0], motion_scale[1]),
                        )?;
                        check(
                            "xessVKExecute",
                            (api.xessVKExecute)(handle as _, command, &execute),
                        )
                    });
                    // Restore HAL's tracked layout even when SDK recording fails.
                    if is_depth {
                        transition(
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
                        );
                    }
                    result
                }
                None => Err(Error::NotVulkan),
            })
        };
        if let Err(error) = result {
            // This buffer is never submitted: the next evaluate records the fills again.
            self.pending_fills = fills;
            return Err(error);
        }
        Ok((
            encoder.finish(),
            Lease {
                _shared: Arc::clone(&self.shared),
            },
        ))
    }
}
fn retention_layout(device: &wgpu::Device, depth: bool) -> wgpu::BindGroupLayout {
    let entries = std::array::from_fn::<_, 4, _>(|index| wgpu::BindGroupLayoutEntry {
        binding: index as u32,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: if index == 3 {
            wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format: wgpu::TextureFormat::Rgba16Float,
                view_dimension: wgpu::TextureViewDimension::D2,
            }
        } else {
            wgpu::BindingType::Texture {
                sample_type: if index == 2 && depth {
                    wgpu::TextureSampleType::Depth
                } else {
                    wgpu::TextureSampleType::Float { filterable: false }
                },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            }
        },
        count: None,
    });
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("xess retained views"),
        entries: &entries,
    })
}
fn resource(view: &wgpu::TextureView) -> Result<ffi::xess_vk_image_view_info, Error> {
    let texture = view.texture();
    let format = match texture.format() {
        wgpu::TextureFormat::Rgba16Float => vk::Format::R16G16B16A16_SFLOAT,
        wgpu::TextureFormat::Rg16Float => vk::Format::R16G16_SFLOAT,
        wgpu::TextureFormat::R32Float => vk::Format::R32_SFLOAT,
        wgpu::TextureFormat::Depth32Float => vk::Format::D32_SFLOAT,
        _ => return Err(Error::InvalidInput),
    };
    // SAFETY: read handles under separate guards (same snatch lock), view and texture live.
    let image_view = unsafe { view.as_hal::<Vulkan>().map(|h| h.raw_handle()) }.ok_or(Error::NotVulkan)?;
    // SAFETY: as above.
    let image = unsafe { texture.as_hal::<Vulkan>().map(|h| h.raw_handle()) }.ok_or(Error::NotVulkan)?;
    Ok(ffi::xess_vk_image_view_info {
        imageView: image_view,
        image,
        subresourceRange: vk::ImageSubresourceRange {
            aspect_mask: if texture.format().has_depth_aspect() {
                vk::ImageAspectFlags::DEPTH
            } else {
                vk::ImageAspectFlags::COLOR
            },
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        },
        format,
        width: texture.width(),
        height: texture.height(),
    })
}

pub(crate) fn device_prepared(device: &wgpu::Device) -> bool {
    // SAFETY: read a live handle under the HAL guard; no Vulkan operation is issued.
    let Some(raw) = (unsafe {
        device
            .as_hal::<Vulkan>()
            .map(|h| h.raw_device().handle().as_raw())
    }) else {
        return false;
    };
    PREPARED.lock().is_ok_and(|entries| {
        entries
            .iter()
            .any(|(handle, entry)| *handle == raw && entry.strong_count() != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_match_generated_sdk_constants() {
        for (quality, value) in [
            (Quality::Native, ffi::XESS_QUALITY_SETTING_AA),
            (Quality::Quality, ffi::XESS_QUALITY_SETTING_QUALITY),
            (Quality::Balanced, ffi::XESS_QUALITY_SETTING_BALANCED),
            (Quality::Performance, ffi::XESS_QUALITY_SETTING_PERFORMANCE),
            (
                Quality::UltraPerformance,
                ffi::XESS_QUALITY_SETTING_ULTRA_PERFORMANCE,
            ),
        ] {
            assert_eq!(quality.sdk_value(), value as u32);
        }
    }
}
