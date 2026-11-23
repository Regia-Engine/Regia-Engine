// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from dlss_wgpu (https://github.com/bevyengine/dlss_wgpu, commit 4046a407cc8d),
// src/initialization.rs, src/feature_info.rs, src/sdk.rs, src/super_resolution.rs, src/nvsdk_ngx.rs
// and build.rs: the NGX extension registration, the feature-discovery block, the NGX init call,
// the evaluate parameter block and the bindgen setup. Copyright (c) the dlss_wgpu contributors.
// Licensed MIT OR Apache-2.0 (see docs/ported-sources.toml); used here under MIT:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions: The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software. THE SOFTWARE IS PROVIDED "AS IS",
// WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS
// OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF
// CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
// OTHER DEALINGS IN THE SOFTWARE.

//! The build that links NGX (ADR-2002). Every `unsafe` block calls NGX or wgpu-hal with
//! handles taken from live wgpu objects; NGX never outlives the device it was initialised on
//! (`NgxShared` holds a clone of it and shuts NGX down before dropping it).
//!
//! Adapted from the open-source `dlss_wgpu` crate (MIT OR Apache-2.0): the instance and
//! device extension registration, the feature-info block and the evaluate parameter block
//! follow it; the error handling (no panics), the runtime directories, the format table and
//! the lifetimes are Regia Engine's.

mod sys {
    // bindgen output: C names, every NGX item (most unused), unsafe helpers with no comments
    // (ADR-2086 names this module as the one place the comment lint is off).
    #![allow(
        non_upper_case_globals,
        non_camel_case_types,
        non_snake_case,
        dead_code,
        missing_docs
    )]
    #![allow(
        unsafe_op_in_unsafe_fn,
        clippy::all,
        clippy::undocumented_unsafe_blocks,
        unnecessary_transmutes
    )]

    pub(crate) type PFN_vkGetDeviceProcAddr = ash::vk::PFN_vkGetDeviceProcAddr;
    pub(crate) type PFN_vkGetInstanceProcAddr = ash::vk::PFN_vkGetInstanceProcAddr;
    pub(crate) type VkBuffer = ash::vk::Buffer;
    pub(crate) type VkCommandBuffer = ash::vk::CommandBuffer;
    pub(crate) type VkDevice = ash::vk::Device;
    pub(crate) type VkExtensionProperties = ash::vk::ExtensionProperties;
    pub(crate) type VkFormat = ash::vk::Format;
    pub(crate) type VkImage = ash::vk::Image;
    pub(crate) type VkImageSubresourceRange = ash::vk::ImageSubresourceRange;
    pub(crate) type VkImageView = ash::vk::ImageView;
    pub(crate) type VkInstance = ash::vk::Instance;
    pub(crate) type VkPhysicalDevice = ash::vk::PhysicalDevice;

    include!(concat!(env!("OUT_DIR"), "/ngx_bindings.rs"));
}

use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::{Arc, Mutex};

use ash::vk;
use wgpu::hal::api::Vulkan;

use sys::*;

use crate::lifecycle::Lifecycle;
use crate::{
    AddedExtensions, CreateFlags, DlssError, EvalInputs, NgxConfig, NgxFailure, OptimalSettings, PerfQuality,
};

fn check(call: &'static str, result: NVSDK_NGX_Result) -> Result<(), DlssError> {
    // bindgen gives NGX's result enum the target's C enum type: `u32` on Linux, `i32` under MSVC,
    // where C enums are `int` (ADR-7695). The codes are bit patterns (0xBAD0_0000 and up), so
    // reinterpret the bits rather than convert the value.
    let code = u32::from_ne_bytes(result.to_ne_bytes());
    match NgxFailure::from_code(code) {
        None => Ok(()),
        Some(NgxFailure::OutOfDate) => Err(DlssError::NeedsNewerDriver),
        Some(failure) => Err(DlssError::Ngx { call, failure }),
    }
}

#[cfg(target_os = "windows")]
fn wide(s: &std::ffi::OsStr) -> Result<Vec<wchar_t>, DlssError> {
    use std::os::windows::ffi::OsStrExt;
    Ok(s.encode_wide().chain([0]).collect())
}

#[cfg(not(target_os = "windows"))]
fn wide(s: &std::ffi::OsStr) -> Result<Vec<wchar_t>, DlssError> {
    Ok(crate::utf8_path(s)?
        .chars()
        .chain(['\0'])
        .map(|c| c as wchar_t)
        .collect())
}

/// Builds NGX's feature-discovery block for `feature` and hands it to `body`; every pointer in it
/// lives until `body` returns.
fn with_feature_info<T>(
    config: &NgxConfig,
    feature: NVSDK_NGX_Feature,
    body: impl FnOnce(&NVSDK_NGX_FeatureDiscoveryInfo) -> T,
) -> Result<T, DlssError> {
    let project_id = CString::new(config.project_id.as_str()).map_err(|_| DlssError::Ngx {
        call: "project_id",
        failure: NgxFailure::InvalidParameter,
    })?;
    let engine_version = CString::new(config.engine_version.as_str()).map_err(|_| DlssError::Ngx {
        call: "engine_version",
        failure: NgxFailure::InvalidParameter,
    })?;
    let data_path = wide(config.data_dir.as_os_str())?;
    let paths: Vec<Vec<wchar_t>> = config
        .library_dirs
        .iter()
        .map(|dir| wide(dir.as_os_str()))
        .collect::<Result<_, _>>()?;
    let path_ptrs: Vec<*const wchar_t> = paths.iter().map(|p| p.as_ptr()).collect();
    let common = NVSDK_NGX_FeatureCommonInfo {
        PathListInfo: NVSDK_NGX_PathListInfo {
            Path: path_ptrs.as_ptr(),
            Length: path_ptrs.len() as u32,
        },
        InternalData: ptr::null_mut(),
        LoggingInfo: NVSDK_NGX_LoggingInfo {
            LoggingCallback: None,
            MinimumLoggingLevel: NVSDK_NGX_Logging_Level_NVSDK_NGX_LOGGING_LEVEL_OFF,
            DisableOtherLoggingSinks: false,
        },
    };
    let info = NVSDK_NGX_FeatureDiscoveryInfo {
        SDKVersion: NVSDK_NGX_Version_NVSDK_NGX_Version_API,
        FeatureID: feature,
        Identifier: NVSDK_NGX_Application_Identifier {
            IdentifierType:
                NVSDK_NGX_Application_Identifier_Type_NVSDK_NGX_Application_Identifier_Type_Project_Id,
            v: NVSDK_NGX_Application_Identifier_v {
                ProjectDesc: NVSDK_NGX_ProjectIdDescription {
                    ProjectId: project_id.as_ptr(),
                    EngineType: NVSDK_NGX_EngineType_NVSDK_NGX_ENGINE_TYPE_CUSTOM,
                    EngineVersion: engine_version.as_ptr(),
                },
            },
        },
        ApplicationDataPath: data_path.as_ptr(),
        FeatureInfo: &common,
    };
    Ok(body(&info))
}

/// The most Vulkan extensions NGX may ask for in one table. A larger count is a corrupt or
/// mismatched runtime, refused before the table is read (audit wave 5, upscalers H1).
const MAX_NGX_EXTENSIONS: u32 = 64;

/// NGX's extension table of `count` entries at `list` as a slice, or a refusal for a count past
/// [`MAX_NGX_EXTENSIONS`].
///
/// # Safety
///
/// `list` is non-null and points at `count` entries that stay valid for `'a`. NGX documents no
/// lifetime for its table, so callers keep `'a` short: [`names`] copies what it needs.
unsafe fn ngx_table<'a>(
    call: &'static str,
    list: *const VkExtensionProperties,
    count: u32,
) -> Result<&'a [VkExtensionProperties], DlssError> {
    if count > MAX_NGX_EXTENSIONS {
        return Err(DlssError::Vulkan(format!(
            "{call} listed {count} extensions; at most {MAX_NGX_EXTENSIONS} are read"
        )));
    }
    // SAFETY: the caller's contract; the count is bounded above.
    Ok(unsafe { std::slice::from_raw_parts(list, count as usize) })
}

/// The names in NGX's table, copied. wgpu keeps the names it is handed for the life of the instance
/// and device, and NGX documents no lifetime for its table, so the few bytes are leaked rather than
/// trusting one. Each name is read inside its fixed-size array; an entry with no NUL is an error
/// rather than a silent skip, so a truncated table is not mistaken for a short one (edge audit
/// 1102, plan row A06).
fn names(list: &[VkExtensionProperties]) -> Result<Vec<&'static CStr>, DlssError> {
    list.iter()
        .map(|extension| {
            extension.extension_name_as_c_str().map_err(|_| {
                DlssError::Vulkan("NGX returned an extension name with no terminator".to_owned())
            })
        })
        .map(|name| name.map(|name| &*Box::leak(CString::from(name).into_boxed_c_str())))
        .collect()
}

fn owned(list: &[&'static CStr]) -> AddedExtensions {
    list.iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

pub(crate) fn create_instance(
    descriptor: &wgpu::InstanceDescriptor,
    config: &NgxConfig,
) -> Result<(wgpu::Instance, AddedExtensions), DlssError> {
    let mut asked: Result<Vec<&'static CStr>, DlssError> = Ok(Vec::new());
    let hal_descriptor = wgpu::hal::InstanceDescriptor {
        name: "regia",
        flags: descriptor.flags,
        memory_budget_thresholds: descriptor.memory_budget_thresholds,
        backend_options: descriptor.backend_options.clone(),
        telemetry: None,
        display: None,
    };
    let callback: Box<wgpu::hal::vulkan::CreateInstanceCallback> = Box::new(|args| {
        asked = with_feature_info(
            config,
            NVSDK_NGX_Feature_NVSDK_NGX_Feature_SuperSampling,
            |info| {
                let mut list = ptr::null_mut();
                let mut count = 0u32;
                // SAFETY: `info` is the feature-info block `with_feature_info` keeps alive for this call, and
                // `count` and `list` are live locals; NGX fills `list` with `count` entries it owns.
                check("GetFeatureInstanceExtensionRequirements", unsafe {
                    NVSDK_NGX_VULKAN_GetFeatureInstanceExtensionRequirements(info, &mut count, &mut list)
                })?;
                if list.is_null() || count == 0 {
                    return Ok(Vec::new());
                }
                // SAFETY: NGX's table holds `count` entries (not null, checked above), and it is read
                // only until `names` has copied them.
                let wanted =
                    names(unsafe { ngx_table("GetFeatureInstanceExtensionRequirements", list, count) }?)?;
                // SAFETY: a plain Vulkan enumeration on the loader wgpu just loaded.
                let supported = unsafe { args.entry.enumerate_instance_extension_properties(None) }
                    .map_err(|e| DlssError::Vulkan(e.to_string()))?;
                let all = wanted.iter().all(|want| {
                    supported
                        .iter()
                        .any(|have| have.extension_name_as_c_str().is_ok_and(|name| name == *want))
                });
                if all {
                    for want in &wanted {
                        if !args.extensions.contains(want) {
                            args.extensions.push(*want);
                        }
                    }
                    Ok(wanted)
                } else {
                    Ok(Vec::new())
                }
            },
        )
        .and_then(|inner| inner);
    });
    // SAFETY: the callback only reads NGX's extension table and appends to wgpu's list.
    let raw = unsafe { wgpu::hal::vulkan::Instance::init_with_callback(&hal_descriptor, Some(callback)) }
        .map_err(|e| DlssError::Wgpu(e.to_string()))?;
    let added = asked?;
    // SAFETY: `raw` is a fully initialised Vulkan hal instance.
    let instance = unsafe { wgpu::Instance::from_hal::<Vulkan>(raw) };
    Ok((instance, owned(&added)))
}

pub(crate) fn request_device(
    adapter: &wgpu::Adapter,
    descriptor: &wgpu::DeviceDescriptor<'_>,
    config: &NgxConfig,
) -> Result<(wgpu::Device, wgpu::Queue, AddedExtensions), DlssError> {
    // SAFETY: the hal adapter is only used inside this function while `adapter` is alive.
    let raw_adapter = unsafe { adapter.as_hal::<Vulkan>() }.ok_or(DlssError::NotVulkan)?;
    let instance = raw_adapter.shared_instance().raw_instance().handle();
    let physical = raw_adapter.raw_physical_device();
    let wanted = with_feature_info(
        config,
        NVSDK_NGX_Feature_NVSDK_NGX_Feature_SuperSampling,
        |info| {
            let mut list = ptr::null_mut();
            let mut count = 0u32;
            // SAFETY: valid instance and physical device handles; NGX owns the returned table.
            check("GetFeatureDeviceExtensionRequirements", unsafe {
                NVSDK_NGX_VULKAN_GetFeatureDeviceExtensionRequirements(
                    instance, physical, info, &mut count, &mut list,
                )
            })?;
            if list.is_null() || count == 0 {
                return Ok(Vec::new());
            }
            // SAFETY: NGX's table holds `count` entries (not null, checked above), and it is read
            // only until `names` has copied them. A name with no NUL is an error, as on the
            // instance path.
            names(unsafe { ngx_table("GetFeatureDeviceExtensionRequirements", list, count) }?)
        },
    )??;
    let capabilities = raw_adapter.physical_device_capabilities();
    let added: Vec<&'static CStr> = if wanted.iter().all(|name| capabilities.supports_extension(name)) {
        wanted
    } else {
        Vec::new()
    };
    let to_add = added.clone();
    // SAFETY: the callback appends supported extension names only.
    let open = unsafe {
        raw_adapter.open_with_callback(
            descriptor.required_features,
            &descriptor.required_limits,
            &descriptor.memory_hints,
            Some(Box::new(move |args| {
                for name in &to_add {
                    if !args.extensions.contains(name) {
                        args.extensions.push(name);
                    }
                }
            })),
        )
    }
    .map_err(|e| DlssError::Wgpu(e.to_string()))?;
    drop(raw_adapter);
    // SAFETY: `open` was created from this adapter's hal adapter.
    let (device, queue) = unsafe { adapter.create_device_from_hal::<Vulkan>(open, descriptor) }
        .map_err(|e| DlssError::Wgpu(e.to_string()))?;
    Ok((device, queue, owned(&added)))
}

/// The names in NGX's table as owned strings (nothing leaked: for checks that run per device).
fn table_names(list: &[VkExtensionProperties]) -> Vec<String> {
    list.iter()
        .filter_map(|extension| extension.extension_name_as_c_str().ok())
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

/// Whether `device` and its instance enable every extension NGX asks for on this device's GPU,
/// asked from NGX for this device (audit 11-03, dlss M2 and L1): not the lists the last hook
/// returned, which belong to whichever device was created last, and where an empty list meant
/// both "none wanted" and "wanted but unsupported".
pub(crate) fn device_has_ngx_extensions(
    device: &wgpu::Device,
    config: &NgxConfig,
) -> Result<bool, DlssError> {
    // SAFETY: the hal device is only read (handles and extension lists) while `device` is alive.
    let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or(DlssError::NotVulkan)?;
    let shared = hal.shared_instance();
    let instance = shared.raw_instance().handle();
    let physical = hal.raw_physical_device();
    let instance_wanted = with_feature_info(
        config,
        NVSDK_NGX_Feature_NVSDK_NGX_Feature_SuperSampling,
        |info| {
            let mut list = ptr::null_mut();
            let mut count = 0u32;
            // SAFETY: `info` is the feature-info block `with_feature_info` keeps alive for this call, and
            // `count` and `list` are live locals; NGX fills `list` with `count` entries it owns.
            check("GetFeatureInstanceExtensionRequirements", unsafe {
                NVSDK_NGX_VULKAN_GetFeatureInstanceExtensionRequirements(info, &mut count, &mut list)
            })?;
            if list.is_null() || count == 0 {
                return Ok(Vec::new());
            }
            // SAFETY: NGX's table holds `count` entries (not null, checked above), read only
            // until `table_names` has copied them.
            Ok(table_names(unsafe {
                ngx_table("GetFeatureInstanceExtensionRequirements", list, count)
            }?))
        },
    )??;
    let device_wanted = with_feature_info(
        config,
        NVSDK_NGX_Feature_NVSDK_NGX_Feature_SuperSampling,
        |info| {
            let mut list = ptr::null_mut();
            let mut count = 0u32;
            // SAFETY: valid instance and physical device handles; NGX owns the returned table.
            check("GetFeatureDeviceExtensionRequirements", unsafe {
                NVSDK_NGX_VULKAN_GetFeatureDeviceExtensionRequirements(
                    instance, physical, info, &mut count, &mut list,
                )
            })?;
            if list.is_null() || count == 0 {
                return Ok(Vec::new());
            }
            // SAFETY: NGX's table holds `count` entries (not null, checked above), read only
            // until `table_names` has copied them.
            Ok(table_names(unsafe {
                ngx_table("GetFeatureDeviceExtensionRequirements", list, count)
            }?))
        },
    )??;
    Ok(has_every(hal.enabled_device_extensions(), &device_wanted)
        && has_every(shared.extensions(), &instance_wanted))
}

/// Whether `device` and its instance enable every extension of `device_wanted` and
/// `instance_wanted` (audit wave 6, upscalers M2).
pub(crate) fn device_has_extensions(
    device: &wgpu::Device,
    instance_wanted: &[String],
    device_wanted: &[String],
) -> bool {
    // SAFETY: the hal device is only read (two extension lists) while `device` is alive.
    let Some(hal) = (unsafe { device.as_hal::<Vulkan>() }) else {
        return false;
    };
    has_every(hal.enabled_device_extensions(), device_wanted)
        && has_every(hal.shared_instance().extensions(), instance_wanted)
}

/// Every name of `wanted` is in `enabled`.
fn has_every(enabled: &[&CStr], wanted: &[String]) -> bool {
    wanted
        .iter()
        .all(|name| enabled.iter().any(|have| have.to_bytes() == name.as_bytes()))
}

/// NGX's state on every device that has it: one per device, shared by every [`Ngx`] on it
/// (audit 11-03, dlss M1: the registry used to live in the renderer's seam only).
static NGX: Lifecycle<vk::Device, NgxShared> = Lifecycle::new();

/// One native NGX init or shutdown at a time in the process: a failed init's compensating
/// `Shutdown1`, and the shutdown of a device's last owner, must not run between another thread's
/// init and its capability query (audit wave 5, upscalers M2; audit 11-03, dlss M1: the
/// drop-time shutdown was outside this lock). Never held across a GPU wait (dlss M3). A poisoned
/// lock still serializes.
static NATIVE: Mutex<()> = Mutex::new(());

/// NGX's per-device state; shut down when the last owner drops.
struct NgxShared {
    params: *mut NVSDK_NGX_Parameter,
    device: wgpu::Device,
    identity: vk::Device,
    lock: Mutex<()>,
}

/// wgpu device IDs are local to an instance; live Vulkan device handles distinguish instances.
fn device_identity(device: &wgpu::Device) -> Result<vk::Device, DlssError> {
    // SAFETY: only copies the handle while the owning wgpu device is alive. The registry's
    // shared value retains that device until shutdown, so the handle cannot be recycled early.
    let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or(DlssError::NotVulkan)?;
    Ok(hal.raw_device().handle())
}

// SAFETY: NGX's parameter block is only touched under `lock` (or, in `Ngx::new` and `Drop`, by the
// sole owner); it has no thread affinity (NGX is called from whichever thread renders); the device
// is Send + Sync.
unsafe impl Send for NgxShared {}
// SAFETY: as above.
unsafe impl Sync for NgxShared {}

/// Waits until the GPU finished everything submitted so far, which is all of NGX's commands: they go
/// through wgpu's queue. wgpu's own wait, not `vkDeviceWaitIdle`, which Vulkan forbids while any other
/// thread uses a queue of the device: nothing pins a release to the submitting thread, and the renderer
/// frees whole renderers on helper threads (`regia-renderer-drop`).
fn wait_idle(device: &wgpu::Device) -> Option<vk::Device> {
    if let Err(error) = device.poll(wgpu::PollType::wait_indefinitely()) {
        // A lost device: nothing is running any more, and the release goes on.
        tracing::warn!(target: "regia_dlss", %error, "waiting for the GPU before an NGX release failed");
    }
    // SAFETY: the hal device is used only to read its handle, while `device` is alive.
    let hal = unsafe { device.as_hal::<Vulkan>() }?;
    Some(hal.raw_device().handle())
}

impl Drop for NgxShared {
    fn drop(&mut self) {
        // The GPU wait runs with no lock held: a long-draining device holds up only a reopen on
        // that device (it waits in `NGX` until `closed` below), never another device (dlss M3).
        if let Some(raw) = wait_idle(&self.device) {
            let _native = NATIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // SAFETY: `params` came from GetCapabilityParameters on this device; shut down once.
            unsafe {
                if let Err(error) = check(
                    "DestroyParameters",
                    NVSDK_NGX_VULKAN_DestroyParameters(self.params),
                ) {
                    tracing::warn!(target: "regia_dlss", %error, "NGX parameter release failed");
                }
                if let Err(error) = check("Shutdown1", NVSDK_NGX_VULKAN_Shutdown1(raw)) {
                    tracing::warn!(target: "regia_dlss", %error, "NGX shutdown failed");
                }
            }
        }
        NGX.closed(self);
    }
}

pub(crate) struct Ngx {
    shared: Arc<NgxShared>,
}

impl Ngx {
    /// The NGX instance of `device`: the live one when the device has it (whoever made it),
    /// else a new init. A reopen while the device's last instance is still shutting down waits
    /// for that shutdown; other devices never wait for this one (audit 11-03, dlss M1, M3).
    pub(crate) fn new(config: &NgxConfig, device: &wgpu::Device) -> Result<Self, DlssError> {
        let identity = device_identity(device)?;
        let shared = NGX.get_or_open(&identity, || Self::open(config, device))?;
        Ok(Self { shared })
    }

    fn open(config: &NgxConfig, device: &wgpu::Device) -> Result<Arc<NgxShared>, DlssError> {
        let _span = tracing::info_span!("ngx_init").entered();
        let identity = device_identity(device)?;
        let mut params = ptr::null_mut();
        {
            // Released before `NgxShared` exists: its drop (a failed check below) takes it.
            let _native = NATIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // SAFETY: the hal device is used only inside this block while `device` is alive.
            let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or(DlssError::NotVulkan)?;
            let shared = hal.shared_instance();
            let raw_instance = shared.raw_instance();
            let gipa = shared.entry().static_fn().get_instance_proc_addr;
            let gdpa = raw_instance.fp_v1_0().get_device_proc_addr;
            with_feature_info(
                config,
                NVSDK_NGX_Feature_NVSDK_NGX_Feature_SuperSampling,
                |info| {
                    // SAFETY: live handles of one device; `info`'s strings outlive the call.
                    check("Init_with_ProjectID", unsafe {
                        NVSDK_NGX_VULKAN_Init_with_ProjectID(
                            info.Identifier.v.ProjectDesc.ProjectId,
                            NVSDK_NGX_EngineType_NVSDK_NGX_ENGINE_TYPE_CUSTOM,
                            info.Identifier.v.ProjectDesc.EngineVersion,
                            info.ApplicationDataPath,
                            raw_instance.handle(),
                            hal.raw_physical_device(),
                            hal.raw_device().handle(),
                            gipa,
                            gdpa,
                            info.FeatureInfo,
                            NVSDK_NGX_Version_NVSDK_NGX_Version_API,
                        )
                    })
                },
            )??;
            // SAFETY: NGX is initialised; it allocates the parameter block.
            if let Err(error) = check("GetCapabilityParameters", unsafe {
                NVSDK_NGX_VULKAN_GetCapabilityParameters(&mut params)
            }) {
                // SAFETY: undo the init above on this device.
                unsafe { NVSDK_NGX_VULKAN_Shutdown1(hal.raw_device().handle()) };
                return Err(error);
            }
        }
        // From here `NgxShared::drop` releases the parameters and shuts NGX down.
        let shared = Arc::new(NgxShared {
            params,
            device: device.clone(),
            identity,
            lock: Mutex::new(()),
        });
        // An out-of-date read keeps its own error instead of reading as "not available"; other
        // failures of this optional key (older runtimes lack it) count as "no update needed".
        match get_i(&shared, NVSDK_NGX_Parameter_SuperSampling_NeedsUpdatedDriver) {
            Ok(1) | Err(DlssError::NeedsNewerDriver) => return Err(DlssError::NeedsNewerDriver),
            Ok(_) | Err(_) => {}
        }
        if get_i(&shared, NVSDK_NGX_Parameter_SuperSampling_Available)? != 1 {
            return Err(DlssError::Unavailable);
        }
        Ok(shared)
    }

    pub(crate) fn is_on(&self, device: &wgpu::Device) -> bool {
        device_identity(device).is_ok_and(|identity| identity == self.shared.identity)
    }

    pub(crate) fn optimal_settings(
        &self,
        display: [u32; 2],
        quality: PerfQuality,
    ) -> Result<OptimalSettings, DlssError> {
        let _guard = self.shared.lock.lock().map_err(|_| DlssError::Poisoned)?;
        // DLAA is ratio 1, so the display size is the whole answer. NGX is not asked, and a
        // display that contains 0 is returned as-is. Other modes reject a 0 render size.
        if quality == PerfQuality::Dlaa {
            return Ok(OptimalSettings {
                render: display,
                min: display,
                max: display,
                sharpness: 0.0,
            });
        }
        let mut render = [0u32; 2];
        let mut max = [0u32; 2];
        let mut min = [0u32; 2];
        let mut sharpness = 0.0f32;
        // SAFETY: `params` is live and guarded; the out pointers are locals.
        check("DLSS_GET_OPTIMAL_SETTINGS", unsafe {
            NGX_DLSS_GET_OPTIMAL_SETTINGS(
                self.shared.params,
                display[0],
                display[1],
                perf_value(quality),
                &mut render[0],
                &mut render[1],
                &mut max[0],
                &mut max[1],
                &mut min[0],
                &mut min[1],
                &mut sharpness,
            )
        })?;
        if render.contains(&0) {
            return Err(DlssError::Ngx {
                call: "DLSS_GET_OPTIMAL_SETTINGS",
                failure: NgxFailure::UnsupportedParameter,
            });
        }
        Ok(OptimalSettings {
            render,
            min,
            max,
            sharpness,
        })
    }

    pub(crate) fn vram_bytes(&self) -> Result<u64, DlssError> {
        let _guard = self.shared.lock.lock().map_err(|_| DlssError::Poisoned)?;
        let mut bytes = 0u64;
        // SAFETY: `params` is live and guarded.
        check("DLSS_GET_STATS", unsafe {
            NGX_DLSS_GET_STATS(self.shared.params, &mut bytes)
        })?;
        Ok(bytes)
    }
}

/// An integer from NGX's capability parameters (`None` when it has none).
fn get_i(shared: &NgxShared, key: &[u8]) -> Result<i32, DlssError> {
    let mut value = 0i32;
    // SAFETY: `params` is live and only `Ngx::new` calls this, as its sole owner; `key` is one of NGX's
    // NUL-terminated parameter names.
    let result = unsafe { NVSDK_NGX_Parameter_GetI(shared.params, key.as_ptr().cast(), &mut value) };
    check("Parameter_GetI", result)?;
    Ok(value)
}

fn perf_value(quality: PerfQuality) -> NVSDK_NGX_PerfQuality_Value {
    match quality {
        PerfQuality::Dlaa => NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_DLAA,
        PerfQuality::Quality => NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxQuality,
        PerfQuality::Balanced => NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_Balanced,
        PerfQuality::Performance => NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxPerf,
        PerfQuality::UltraPerformance => {
            NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_UltraPerformance
        }
    }
}

fn create_flags(flags: CreateFlags) -> NVSDK_NGX_DLSS_Feature_Flags {
    let mut bits = 0;
    let mut set = |on: bool, bit: NVSDK_NGX_DLSS_Feature_Flags| {
        if on {
            bits |= bit;
        }
    };
    set(
        flags.hdr,
        NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_IsHDR,
    );
    set(
        flags.depth_inverted,
        NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_DepthInverted,
    );
    set(
        flags.motion_low_res,
        NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_MVLowRes,
    );
    set(
        flags.motion_jittered,
        NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_MVJittered,
    );
    set(
        flags.auto_exposure,
        NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_AutoExposure,
    );
    bits
}

/// Vulkan formats of the textures the engine hands DLSS (no adapter is needed to name them).
fn vk_format(format: wgpu::TextureFormat) -> Result<vk::Format, DlssError> {
    use wgpu::TextureFormat as F;
    Ok(match format {
        F::Rgba16Float => vk::Format::R16G16B16A16_SFLOAT,
        F::Rgba32Float => vk::Format::R32G32B32A32_SFLOAT,
        F::Rg11b10Ufloat => vk::Format::B10G11R11_UFLOAT_PACK32,
        F::Rgb10a2Unorm => vk::Format::A2B10G10R10_UNORM_PACK32,
        F::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
        F::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
        F::Rg16Float => vk::Format::R16G16_SFLOAT,
        F::Rg32Float => vk::Format::R32G32_SFLOAT,
        F::R16Float => vk::Format::R16_SFLOAT,
        F::R32Float => vk::Format::R32_SFLOAT,
        F::R8Unorm => vk::Format::R8_UNORM,
        F::Depth32Float => vk::Format::D32_SFLOAT,
        other => return Err(DlssError::UnsupportedFormat(other)),
    })
}

fn resource(view: &wgpu::TextureView, writable: bool) -> Result<NVSDK_NGX_Resource_VK, DlssError> {
    let texture = view.texture();
    let format = texture.format();
    let raw_format = vk_format(format)?;
    // SAFETY: each hal guard is held only to read its handle (one at a time: both take wgpu's
    // snatch lock) while `view` is alive.
    let raw_view =
        unsafe { view.as_hal::<Vulkan>().map(|hal| hal.raw_handle()) }.ok_or(DlssError::NotVulkan)?;
    // SAFETY: as above.
    let raw_image =
        unsafe { texture.as_hal::<Vulkan>().map(|hal| hal.raw_handle()) }.ok_or(DlssError::NotVulkan)?;
    let aspect = if format.has_depth_aspect() {
        vk::ImageAspectFlags::DEPTH
    } else {
        vk::ImageAspectFlags::COLOR
    };
    // SAFETY: a plain struct fill (NGX's inline helper).
    Ok(unsafe {
        NVSDK_NGX_Create_ImageView_Resource_VK(
            raw_view,
            raw_image,
            vk::ImageSubresourceRange {
                aspect_mask: aspect,
                base_mip_level: 0,
                level_count: vk::REMAINING_MIP_LEVELS,
                base_array_layer: 0,
                layer_count: vk::REMAINING_ARRAY_LAYERS,
            },
            raw_format,
            texture.width(),
            texture.height(),
            writable && texture.usage().contains(wgpu::TextureUsages::STORAGE_BINDING),
        )
    })
}

/// True when any present sampled resource is the output resource.
fn aliases_output<T: PartialEq>(sampled: &[Option<&T>], output: &T) -> bool {
    sampled.iter().flatten().any(|texture| *texture == output)
}

fn sampled(view: &wgpu::TextureView) -> wgpu::TextureTransition<&wgpu::Texture> {
    wgpu::TextureTransition {
        texture: view.texture(),
        selector: None,
        state: wgpu::TextureUses::RESOURCE,
    }
}

/// A DLSS feature handle, released when its last owner drops: the [`SuperResolution`] and every
/// [`Lease`] of a command buffer that records it (audit 11-03, dlss H1).
struct Feature {
    shared: Arc<NgxShared>,
    handle: *mut NVSDK_NGX_Handle,
}

// SAFETY: the feature handle is used only with the shared lock held, or by `Drop` as its sole owner;
// the `Arc` keeps NGX alive for as long as the handle.
unsafe impl Send for Feature {}
// SAFETY: a shared `&Feature` gives no access to the handle outside the shared lock (`evaluate`
// reads it under the lock); only `Drop`, with `&mut self`, releases it, under the same lock.
unsafe impl Sync for Feature {}

impl Drop for Feature {
    fn drop(&mut self) {
        // Every buffer that recorded the feature was submitted (its lease is gone), so waiting for
        // the GPU covers it. The wait holds no lock (dlss M3); the release does.
        if wait_idle(&self.shared.device).is_none() {
            return;
        }
        let _guard = self.shared.lock.lock();
        // SAFETY: the feature was created on this device and is released once, by its last owner.
        if let Err(error) = check("ReleaseFeature", unsafe {
            NVSDK_NGX_VULKAN_ReleaseFeature(self.handle)
        }) {
            tracing::warn!(target: "regia_dlss", %error, "DLSS feature release failed");
        }
    }
}

/// Keeps a feature from being released while a command buffer that records it may still be
/// submitted.
pub(crate) struct Lease {
    _feature: Arc<Feature>,
}

pub(crate) struct SuperResolution {
    feature: Arc<Feature>,
    display: [u32; 2],
    render_max: [u32; 2],
}

impl SuperResolution {
    pub(crate) fn new(
        ngx: &Ngx,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        display: [u32; 2],
        quality: PerfQuality,
        flags: CreateFlags,
        render_max: [u32; 2],
    ) -> Result<Self, DlssError> {
        let _span = tracing::info_span!("dlss_create").entered();
        crate::check_create_sizes(display, render_max)?;
        let _guard = ngx.shared.lock.lock().map_err(|_| DlssError::Poisoned)?;
        let mut create = NVSDK_NGX_DLSS_Create_Params {
            Feature: NVSDK_NGX_Feature_Create_Params {
                InWidth: render_max[0],
                InHeight: render_max[1],
                InTargetWidth: display[0],
                InTargetHeight: display[1],
                InPerfQualityValue: perf_value(quality),
            },
            InFeatureCreateFlags: create_flags(flags),
            InEnableOutputSubrects: false,
        };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("dlss feature creation"),
        });
        let mut feature = ptr::null_mut();
        // SAFETY: the encoder records only NGX's commands; `params` is guarded.
        let result = unsafe {
            encoder.as_hal_mut::<Vulkan, _, _>(|hal| match hal {
                Some(hal) => check(
                    "CREATE_DLSS_EXT1",
                    NGX_VULKAN_CREATE_DLSS_EXT1(
                        ngx.shared.identity,
                        hal.raw_handle(),
                        1,
                        1,
                        &mut feature,
                        ngx.shared.params,
                        &mut create,
                    ),
                ),
                None => Err(DlssError::NotVulkan),
            })
        };
        result?;
        queue.submit([encoder.finish()]);
        if feature.is_null() {
            return Err(DlssError::Ngx {
                call: "CREATE_DLSS_EXT1",
                failure: NgxFailure::FeatureNotFound,
            });
        }
        Ok(Self {
            feature: Arc::new(Feature {
                shared: Arc::clone(&ngx.shared),
                handle: feature,
            }),
            display,
            render_max,
        })
    }

    pub(crate) fn evaluate(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &EvalInputs<'_>,
    ) -> Result<(wgpu::CommandBuffer, Lease), DlssError> {
        let _span = tracing::info_span!("dlss_evaluate").entered();
        crate::check_render_size(inputs.render_size, self.render_max)?;
        // An input that is also the output would merge RESOURCE with STORAGE_READ_WRITE in the
        // transition below; wgpu reports that at `finish` through a fatal error handler.
        let sampled_textures = [
            Some(inputs.color.texture()),
            Some(inputs.depth.texture()),
            Some(inputs.motion.texture()),
            inputs.exposure.map(wgpu::TextureView::texture),
            inputs.reactive.map(wgpu::TextureView::texture),
            inputs.transparency.map(wgpu::TextureView::texture),
        ];
        if aliases_output(&sampled_textures, inputs.output.texture()) {
            return Err(DlssError::Ngx {
                call: "evaluate_inputs",
                failure: NgxFailure::InvalidParameter,
            });
        }
        // NGX reads these through raw pointers during the evaluate call below. Only the output
        // may be marked writable; an input NGX should sample must not advertise storage-write
        // (edge audit 1102, plan row A06).
        let mut color = resource(inputs.color, false)?;
        let mut output = resource(inputs.output, true)?;
        let mut depth = resource(inputs.depth, false)?;
        let mut motion = resource(inputs.motion, false)?;
        let mut exposure = inputs.exposure.map(|v| resource(v, false)).transpose()?;
        let mut reactive = inputs.reactive.map(|v| resource(v, false)).transpose()?;
        let mut transparency = inputs.transparency.map(|v| resource(v, false)).transpose()?;
        let zero = NVSDK_NGX_Coordinates { X: 0, Y: 0 };
        let mut eval = NVSDK_NGX_VK_DLSS_Eval_Params {
            Feature: NVSDK_NGX_VK_Feature_Eval_Params {
                pInColor: &mut color,
                pInOutput: &mut output,
                InSharpness: 0.0,
            },
            pInDepth: &mut depth,
            pInMotionVectors: &mut motion,
            InJitterOffsetX: inputs.jitter[0],
            InJitterOffsetY: inputs.jitter[1],
            InRenderSubrectDimensions: NVSDK_NGX_Dimensions {
                Width: inputs.render_size[0],
                Height: inputs.render_size[1],
            },
            InReset: i32::from(inputs.reset),
            InMVScaleX: inputs.motion_scale[0],
            InMVScaleY: inputs.motion_scale[1],
            pInTransparencyMask: transparency.as_mut().map_or(ptr::null_mut(), ptr::from_mut),
            pInExposureTexture: exposure.as_mut().map_or(ptr::null_mut(), ptr::from_mut),
            pInBiasCurrentColorMask: reactive.as_mut().map_or(ptr::null_mut(), ptr::from_mut),
            InColorSubrectBase: zero,
            InDepthSubrectBase: zero,
            InMVSubrectBase: zero,
            InTranslucencySubrectBase: zero,
            InBiasCurrentColorSubrectBase: zero,
            InOutputSubrectBase: zero,
            InPreExposure: 1.0,
            InExposureScale: 1.0,
            InIndicatorInvertXAxis: 0,
            InIndicatorInvertYAxis: 0,
            GBufferSurface: NVSDK_NGX_VK_GBuffer {
                pInAttrib: [ptr::null_mut(); 17],
            },
            InToneMapperType: NVSDK_NGX_ToneMapperType_NVSDK_NGX_TONEMAPPER_STRING,
            pInMotionVectors3D: ptr::null_mut(),
            pInIsParticleMask: ptr::null_mut(),
            pInAnimatedTextureMask: ptr::null_mut(),
            pInDepthHighRes: ptr::null_mut(),
            pInPositionViewSpace: ptr::null_mut(),
            InFrameTimeDeltaInMsec: inputs.frame_time_ms,
            pInRayTracingHitDistance: ptr::null_mut(),
            pInMotionVectorsReflections: ptr::null_mut(),
        };
        let mut barriers = vec![
            sampled(inputs.color),
            sampled(inputs.depth),
            sampled(inputs.motion),
        ];
        barriers.extend(
            [inputs.exposure, inputs.reactive, inputs.transparency]
                .into_iter()
                .flatten()
                .map(sampled),
        );
        barriers.push(wgpu::TextureTransition {
            texture: inputs.output.texture(),
            selector: None,
            state: wgpu::TextureUses::STORAGE_READ_WRITE,
        });
        encoder.transition_resources(std::iter::empty(), barriers.into_iter());
        let shared = &self.feature.shared;
        let _guard = shared.lock.lock().map_err(|_| DlssError::Poisoned)?;
        let mut dlss = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("dlss evaluate"),
        });
        let feature = self.feature.handle;
        let params = shared.params;
        // SAFETY: this encoder records only NGX's commands. Every pointer in `eval` is to a local of
        // this function, alive until it returns. Every resource in them was transitioned on
        // `encoder`, whose tracker also keeps it alive through that submission; the caller submits the
        // returned buffer right after `encoder` (the contract of `lib.rs`'s `evaluate`). The lease
        // returned with the buffer keeps the feature from being released before that submission.
        let result = unsafe {
            dlss.as_hal_mut::<Vulkan, _, _>(|hal| match hal {
                Some(hal) => check(
                    "EVALUATE_DLSS_EXT",
                    NGX_VULKAN_EVALUATE_DLSS_EXT(hal.raw_handle(), feature, params, &mut eval),
                ),
                None => Err(DlssError::NotVulkan),
            })
        };
        result?;
        Ok((
            dlss.finish(),
            Lease {
                _feature: Arc::clone(&self.feature),
            },
        ))
    }

    pub(crate) fn display(&self) -> [u32; 2] {
        self.display
    }

    /// Whether the feature lives on `device` (the device its NGX instance was initialised on).
    pub(crate) fn is_on(&self, device: &wgpu::Device) -> bool {
        device_identity(device).is_ok_and(|identity| identity == self.feature.shared.identity)
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;

    /// An input sharing the output's texture is detected; absent optional inputs are ignored.
    #[test]
    fn a_sampled_input_aliasing_the_output_is_detected() {
        assert!(!aliases_output(&[Some(&1), Some(&2), None], &3));
        assert!(aliases_output(&[Some(&1), None, Some(&3)], &3));
        assert!(!aliases_output::<i32>(&[None, None], &3));
    }

    fn entry(name: &[u8]) -> VkExtensionProperties {
        let mut out = VkExtensionProperties::default();
        for (slot, byte) in out.extension_name.iter_mut().zip(name) {
            *slot = *byte as std::ffi::c_char;
        }
        out
    }

    /// Audit wave 5 (upscalers H1): a table count past the cap is refused before the table is
    /// read; edge audit 1102 (A06): an entry whose name fills its array with no NUL is an error,
    /// not a silent skip, and good names are still copied.
    #[test]
    fn the_extension_table_is_bounded_and_names_stay_inside_their_arrays() {
        let table: &'static [VkExtensionProperties] = Box::leak(
            vec![
                entry(b"VK_KHR_good\0"),
                entry(&[b'x'; 256]),
                entry(b"VK_EXT_also\0"),
            ]
            .into_boxed_slice(),
        );
        // SAFETY: `table` holds three entries and lives for the process.
        let three = unsafe { ngx_table("test", table.as_ptr(), 3) }.expect("three");
        // A name with no NUL is an error, not a silent skip (edge audit 1102, plan row A06).
        assert!(matches!(names(three), Err(DlssError::Vulkan(_))));
        let good: &'static [VkExtensionProperties] =
            Box::leak(vec![entry(b"VK_KHR_good\0"), entry(b"VK_EXT_also\0")].into_boxed_slice());
        let got: Vec<&str> = names(good)
            .expect("names")
            .iter()
            .map(|n| n.to_str().expect("utf8"))
            .collect();
        assert_eq!(got, ["VK_KHR_good", "VK_EXT_also"]);
        // SAFETY: the count is refused before the pointer is used.
        let huge = unsafe { ngx_table("test", table.as_ptr(), MAX_NGX_EXTENSIONS + 1) };
        assert!(matches!(huge, Err(DlssError::Vulkan(_))));
    }
}
