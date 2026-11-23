// SPDX-License-Identifier: MPL-2.0
//! Zeroing XeSS's app-owned temporary heaps on GPUs whose host-visible device-local memory does
//! not take optimal-tiled images (ADR-7820).
//!
//! ADR-4907 gives XeSS zero-initialized temporary heaps written by the CPU, which needs a
//! `DEVICE_LOCAL | HOST_VISIBLE` memory type the heap's resources may use. Without Resizable
//! BAR an NVIDIA card's only such type sits on the small BAR heap and takes buffers and linear
//! images only, so the texture heap had none and XeSS fell back to its own allocations. Two more
//! ways to zero a device-local heap: the driver zeroes it at allocation
//! (`VK_EXT_zero_initialize_device_memory`), or the GPU fills it through a transfer buffer bound
//! over the whole allocation before XeSS's first commands. Vulkan defines neither for an image
//! XeSS later creates there with an undefined layout (nor the CPU write it replaces); the
//! `optimal_images_over_filled_memory_read_the_fill` test measures what such an image reads.
#![allow(unsafe_code)]

use ash::vk;
use std::ffi::CStr;

/// `VK_EXT_zero_initialize_device_memory` (Vulkan registry, extension 621).
pub(crate) const ZERO_INIT_EXTENSION: &CStr = c"VK_EXT_zero_initialize_device_memory";
/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ZERO_INITIALIZE_DEVICE_MEMORY_FEATURES_EXT`; ash 0.38
/// predates the extension.
pub(crate) const ZERO_INIT_FEATURES_TYPE: vk::StructureType = vk::StructureType::from_raw(1_000_620_000);
/// `VK_MEMORY_ALLOCATE_ZERO_INITIALIZE_BIT_EXT`.
pub(crate) const ALLOCATE_ZERO_INITIALIZE: vk::MemoryAllocateFlags = vk::MemoryAllocateFlags::from_raw(0x8);

/// `VkPhysicalDeviceZeroInitializeDeviceMemoryFeaturesEXT`, laid out as the C structure.
#[repr(C)]
pub(crate) struct ZeroInitFeatures {
    /// Always [`ZERO_INIT_FEATURES_TYPE`].
    pub(crate) s_type: vk::StructureType,
    /// Next structure of the chain.
    pub(crate) p_next: *mut std::ffi::c_void,
    /// `zeroInitializeDeviceMemory`.
    pub(crate) zero_initialize_device_memory: vk::Bool32,
}
impl Default for ZeroInitFeatures {
    fn default() -> Self {
        Self {
            s_type: ZERO_INIT_FEATURES_TYPE,
            p_next: std::ptr::null_mut(),
            zero_initialize_device_memory: vk::FALSE,
        }
    }
}

/// Which zeroing methods `initialize` may use: every method in order, or only the one the
/// diagnostic `REGIA_XESS_HEAP_ZEROING` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Allowed {
    /// Host write, then driver zeroing, then device fill.
    Auto,
    /// Only this method.
    Only(crate::HeapZeroing),
}
impl Allowed {
    /// Parses the diagnostic variable's value; unknown values are refused (the caller warns).
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("" | "auto") => Ok(Self::Auto),
            Some("host") => Ok(Self::Only(crate::HeapZeroing::HostWrite)),
            Some("driver") => Ok(Self::Only(crate::HeapZeroing::DriverZeroed)),
            Some("fill") => Ok(Self::Only(crate::HeapZeroing::DeviceFill)),
            Some(other) => Err(format!(
                "REGIA_XESS_HEAP_ZEROING={other} is not auto, host, driver or fill"
            )),
        }
    }
    /// The process's setting; an invalid value logs a warning and allows every method.
    pub(crate) fn from_env() -> Self {
        let value = std::env::var("REGIA_XESS_HEAP_ZEROING").ok();
        Self::parse(value.as_deref()).unwrap_or_else(|error| {
            tracing::warn!(%error, "using every XeSS heap zeroing method");
            Self::Auto
        })
    }
    /// Whether `method` may be tried.
    pub(crate) fn allows(self, method: crate::HeapZeroing) -> bool {
        self == Self::Auto || self == Self::Only(method)
    }
}

/// The first memory type allowed by `bits` whose flags contain `required` and none of `excluded`.
pub(crate) fn pick_type(
    properties: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    required: vk::MemoryPropertyFlags,
    excluded: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..properties.memory_type_count.min(vk::MAX_MEMORY_TYPES as u32)).find(|&index| {
        let flags = properties.memory_types[index as usize].property_flags;
        bits & (1 << index) != 0 && flags.contains(required) && !flags.intersects(excluded)
    })
}

/// The type a CPU-zeroed heap uses: device-local, host-visible and coherent.
pub(crate) fn host_write_type(properties: &vk::PhysicalDeviceMemoryProperties, bits: u32) -> Option<u32> {
    pick_type(
        properties,
        bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL
            | vk::MemoryPropertyFlags::HOST_VISIBLE
            | vk::MemoryPropertyFlags::HOST_COHERENT,
        vk::MemoryPropertyFlags::empty(),
    )
}

/// The type a driver-zeroed or GPU-filled heap uses: device-local, preferring a type the host
/// cannot see (the card's main memory, not its BAR window).
pub(crate) fn device_type(properties: &vk::PhysicalDeviceMemoryProperties, bits: u32) -> Option<u32> {
    pick_type(
        properties,
        bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
        vk::MemoryPropertyFlags::HOST_VISIBLE,
    )
    .or_else(|| {
        pick_type(
            properties,
            bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::empty(),
        )
    })
}

/// Memory-type bits of a transfer-destination buffer of `size` bytes: the types a fill alias
/// may be bound to.
///
/// # Safety
/// `device` is a live device.
pub(crate) unsafe fn transfer_buffer_bits(device: &ash::Device, size: u64) -> Result<u32, vk::Result> {
    let info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_DST);
    // SAFETY: a short-lived probe; destroyed before return, only its requirements are read.
    unsafe {
        let probe = device.create_buffer(&info, None)?;
        let bits = device.get_buffer_memory_requirements(probe).memory_type_bits;
        device.destroy_buffer(probe, None);
        Ok(bits)
    }
}

/// A transfer-destination buffer covering `size` bytes of `memory` from offset 0.
///
/// # Safety
/// `memory` is a live allocation of `device` of at least `size` bytes whose type the buffer's
/// requirements allow ([`transfer_buffer_bits`]). Destroy the buffer before freeing the memory.
pub(crate) unsafe fn alias_buffer(
    device: &ash::Device,
    memory: vk::DeviceMemory,
    size: u64,
) -> Result<vk::Buffer, vk::Result> {
    let info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_DST);
    // SAFETY: caller guarantees the memory and its type; on a bind failure the buffer is destroyed.
    unsafe {
        let buffer = device.create_buffer(&info, None)?;
        if let Err(error) = device.bind_buffer_memory(buffer, memory, 0) {
            device.destroy_buffer(buffer, None);
            return Err(error);
        }
        Ok(buffer)
    }
}

/// Records a fill of the whole `buffer` with the 32-bit `value`, then a global barrier that
/// makes the write visible to every later command on the queue (later submissions included,
/// as a pipeline barrier's second scope runs in submission order).
///
/// # Safety
/// `command` is a command buffer of `device` in the recording state, outside a render pass;
/// `buffer` was created with `TRANSFER_DST` and stays alive until the commands complete.
pub(crate) unsafe fn record_fill(
    device: &ash::Device,
    command: vk::CommandBuffer,
    buffer: vk::Buffer,
    value: u32,
) {
    let barrier = vk::MemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);
    // SAFETY: caller guarantees the recording state and the buffer's usage and lifetime.
    unsafe {
        device.cmd_fill_buffer(command, buffer, 0, vk::WHOLE_SIZE, value);
        device.cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[barrier],
            &[],
            &[],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HeapZeroing;
    use wgpu::hal::api::Vulkan;

    /// The RTX 3080 Ti's memory types (driver 610.88, no Resizable BAR;
    /// `docs/evidence/xess/2026-11-14/windows-rtx3080ti/vulkan-memory-properties.txt`).
    fn rtx_3080_ti_without_rebar() -> vk::PhysicalDeviceMemoryProperties {
        use vk::MemoryPropertyFlags as F;
        let mut properties = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 6,
            memory_heap_count: 3,
            ..Default::default()
        };
        for (index, (heap, flags)) in [
            (1, F::empty()),
            (0, F::DEVICE_LOCAL),
            (0, F::DEVICE_LOCAL),
            (1, F::HOST_VISIBLE | F::HOST_COHERENT),
            (1, F::HOST_VISIBLE | F::HOST_COHERENT | F::HOST_CACHED),
            (2, F::DEVICE_LOCAL | F::HOST_VISIBLE | F::HOST_COHERENT),
        ]
        .into_iter()
        .enumerate()
        {
            properties.memory_types[index] = vk::MemoryType {
                property_flags: flags,
                heap_index: heap,
            };
        }
        properties
    }

    /// The red row of ADR-7718, as a table: an optimal-tiled color image may use types 0 and 1
    /// only, so no host-visible device-local type exists for the texture heap, while the device
    /// path picks the card's main memory (type 1), which transfer buffers may alias.
    #[test]
    fn the_3080_ti_texture_heap_has_no_host_write_type_but_a_device_type() {
        let properties = rtx_3080_ti_without_rebar();
        let optimal_image_bits = 0b00_0011;
        let buffer_bits = 0b11_1111;
        assert_eq!(host_write_type(&properties, optimal_image_bits), None);
        assert_eq!(device_type(&properties, optimal_image_bits), Some(1));
        assert_eq!(
            device_type(&properties, optimal_image_bits & buffer_bits),
            Some(1)
        );
        // The buffer heap still has its host-write type, the BAR window (type 5).
        assert_eq!(host_write_type(&properties, buffer_bits), Some(5));
        // A device type prefers memory the host cannot see, but takes the BAR window when it is
        // the only device-local type allowed.
        assert_eq!(device_type(&properties, buffer_bits), Some(1));
        assert_eq!(device_type(&properties, 0b10_0000), Some(5));
        assert_eq!(device_type(&properties, 0b01_1001), None);
    }

    #[test]
    fn the_diagnostic_variable_names_one_method_or_all() {
        assert_eq!(Allowed::parse(None), Ok(Allowed::Auto));
        assert_eq!(Allowed::parse(Some(" auto ")), Ok(Allowed::Auto));
        assert_eq!(Allowed::parse(Some("")), Ok(Allowed::Auto));
        for (value, method) in [
            ("host", HeapZeroing::HostWrite),
            ("driver", HeapZeroing::DriverZeroed),
            ("fill", HeapZeroing::DeviceFill),
        ] {
            let allowed = Allowed::parse(Some(value)).expect("a method");
            assert_eq!(allowed, Allowed::Only(method));
            for other in [
                HeapZeroing::HostWrite,
                HeapZeroing::DriverZeroed,
                HeapZeroing::DeviceFill,
            ] {
                assert_eq!(allowed.allows(other), other == method);
                assert!(Allowed::Auto.allows(other));
            }
        }
        assert!(Allowed::parse(Some("cpu")).is_err());
    }

    fn vulkan_device() -> (wgpu::Device, wgpu::Queue, String) {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(descriptor);
        let adapter =
            regia_core::tasks::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .unwrap_or_else(|error| {
                    panic!("no Vulkan adapter for the zeroing test (lavapipe in CI): {error}")
                });
        let info = adapter.get_info();
        let (device, queue) = regia_core::tasks::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("xess zeroing test"),
            ..Default::default()
        }))
        .unwrap_or_else(|error| panic!("no device for the zeroing test: {error}"));
        let name = format!("{} ({:?}, {})", info.name, info.device_type, info.driver_info);
        (device, queue, name)
    }

    /// Records raw commands into a fresh wgpu encoder, submits it and waits for the GPU.
    fn run(device: &wgpu::Device, queue: &wgpu::Queue, record: impl FnOnce(vk::CommandBuffer)) {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("xess zeroing test"),
        });
        // SAFETY: the encoder records only these raw commands and is submitted below.
        unsafe {
            encoder.as_hal_mut::<Vulkan, _, _>(|hal| {
                let hal = hal.expect("a Vulkan encoder");
                record(hal.raw_handle());
            });
        }
        queue.submit([encoder.finish()]);
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("wait for the zeroing test's commands");
    }

    /// What an optimal-tiled image created over `memory` at offset 0 with an undefined layout
    /// reads (its first `bytes` bytes, copied out tightly packed), as XeSS creates its images.
    fn read_image(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        raw: &ash::Device,
        properties: &vk::PhysicalDeviceMemoryProperties,
        memory: vk::DeviceMemory,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
    ) -> Vec<u8> {
        const SIDE: u32 = 64;
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: SIDE,
                height: SIDE,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage | vk::ImageUsageFlags::TRANSFER_SRC)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let texel = match format {
            vk::Format::R16G16B16A16_SFLOAT => 8,
            _ => 4,
        };
        let bytes = u64::from(SIDE * SIDE * texel);
        // SAFETY: test-owned objects, destroyed before return after the device went idle.
        unsafe {
            let image = raw.create_image(&info, None).expect("probe image");
            raw.bind_image_memory(image, memory, 0).expect("bind probe image");
            let readback_info = vk::BufferCreateInfo::default()
                .size(bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_DST);
            let readback = raw.create_buffer(&readback_info, None).expect("readback buffer");
            let requirements = raw.get_buffer_memory_requirements(readback);
            let index = pick_type(
                properties,
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                vk::MemoryPropertyFlags::empty(),
            )
            .expect("host-visible readback memory");
            let readback_memory = raw
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(index),
                    None,
                )
                .expect("readback memory");
            raw.bind_buffer_memory(readback, readback_memory, 0)
                .expect("bind readback");
            let range = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };
            run(device, queue, |command| {
                let to_copy = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(range);
                raw.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_copy],
                );
                let region = vk::BufferImageCopy::default()
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_extent(vk::Extent3D {
                        width: SIDE,
                        height: SIDE,
                        depth: 1,
                    });
                raw.cmd_copy_image_to_buffer(
                    command,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    readback,
                    &[region],
                );
                let to_host = vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::HOST_READ);
                raw.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::HOST,
                    vk::DependencyFlags::empty(),
                    &[to_host],
                    &[],
                    &[],
                );
            });
            let pointer = raw
                .map_memory(readback_memory, 0, bytes, vk::MemoryMapFlags::empty())
                .expect("map readback");
            let out = std::slice::from_raw_parts(pointer.cast::<u8>(), bytes as usize).to_vec();
            raw.unmap_memory(readback_memory);
            raw.destroy_buffer(readback, None);
            raw.free_memory(readback_memory, None);
            raw.destroy_image(image, None);
            out
        }
    }

    /// The experiment behind the device fill (ADR-7820): device-local memory is filled with a
    /// pattern through the alias buffer, then with zeros through [`record_fill`], and after each
    /// fill an optimal-tiled image created over it with an undefined layout (as XeSS creates its
    /// images) is copied out. After the zero fill every byte must read zero on every GPU. The
    /// pattern read is printed: on a CPU adapter it must read the pattern back (so the test
    /// sees the memory at all); on a GPU it shows what an image over unzeroed memory reads.
    #[test]
    fn optimal_images_over_filled_memory_read_the_fill() {
        let (device, queue, name) = vulkan_device();
        // SAFETY: the guard only reads handles and the memory properties.
        let (raw, properties, cpu) = unsafe {
            device.as_hal::<Vulkan>().map(|h| {
                let instance = h.shared_instance().raw_instance();
                let properties = instance.get_physical_device_memory_properties(h.raw_physical_device());
                let cpu = instance
                    .get_physical_device_properties(h.raw_physical_device())
                    .device_type
                    == vk::PhysicalDeviceType::CPU;
                (h.raw_device().clone(), properties, cpu)
            })
        }
        .expect("a Vulkan device");
        const PATTERN: u32 = 0xA5C3_5A3C;
        for (label, format, usage) in [
            (
                "rgba16f storage",
                vk::Format::R16G16B16A16_SFLOAT,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            ),
            (
                "rgba8 attachment",
                vk::Format::R8G8B8A8_UNORM,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            ),
            (
                "r32f storage",
                vk::Format::R32_SFLOAT,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            ),
        ] {
            // SAFETY: test-owned objects on a live device, destroyed after the device went idle.
            unsafe {
                let info = vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width: 64,
                        height: 64,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(usage | vk::ImageUsageFlags::TRANSFER_SRC)
                    .initial_layout(vk::ImageLayout::UNDEFINED);
                let probe = raw.create_image(&info, None).expect("requirements probe");
                let requirements = raw.get_image_memory_requirements(probe);
                raw.destroy_image(probe, None);
                let size = requirements.size.next_multiple_of(4);
                let bits = requirements.memory_type_bits
                    & transfer_buffer_bits(&raw, size).expect("transfer buffer bits");
                let index =
                    device_type(&properties, bits).expect("a device-local type images and buffers share");
                let memory = raw
                    .allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(size)
                            .memory_type_index(index),
                        None,
                    )
                    .expect("device-local memory");
                let alias = alias_buffer(&raw, memory, size).expect("alias buffer");
                run(&device, &queue, |command| {
                    record_fill(&raw, command, alias, PATTERN)
                });
                let unzeroed = read_image(&device, &queue, &raw, &properties, memory, format, usage);
                run(&device, &queue, |command| record_fill(&raw, command, alias, 0));
                let zeroed = read_image(&device, &queue, &raw, &properties, memory, format, usage);
                raw.destroy_buffer(alias, None);
                raw.free_memory(memory, None);
                let pattern = PATTERN.to_le_bytes();
                let reads_pattern = unzeroed
                    .iter()
                    .enumerate()
                    .filter(|(i, b)| **b == pattern[i % 4])
                    .count();
                let unzeroed_zero = unzeroed.iter().filter(|b| **b == 0).count();
                let zero = zeroed.iter().filter(|b| **b == 0).count();
                eprintln!(
                    "{name}: {label}, type {index}, {size} bytes: over the pattern {reads_pattern} of {} bytes read \
                     the pattern and {unzeroed_zero} read zero; over the zero fill {zero} of {} read zero",
                    unzeroed.len(),
                    zeroed.len()
                );
                assert_eq!(
                    zero,
                    zeroed.len(),
                    "{label}: an image over zero-filled memory read non-zero bytes"
                );
                if cpu {
                    assert_eq!(
                        reads_pattern,
                        unzeroed.len(),
                        "{label}: the CPU adapter's image did not read the memory it was bound to"
                    );
                }
            }
        }
    }
}
