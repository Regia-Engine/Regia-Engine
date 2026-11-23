// SPDX-License-Identifier: MPL-2.0
// The check stub of the NGX binding (ADR-2322). `build.rs` copies this file to
// `$OUT_DIR/ngx_bindings.rs` instead of running bindgen over a DLSS SDK when
// REGIA_DLSS_CHECK_STUB=1 is set, so `cargo check` / `cargo clippy` type-check and lint `src/ngx.rs`
// (cfg `regia_dlss_linked`) on a machine with no SDK: the gate's feature matrix
// (`cargo xtask check-features`, rows `dlss-linked` and `render-dlss-linked`) does this.
//
// Written from the items `src/ngx.rs` uses and the shapes bindgen gives C declarations (enums as an
// integer alias plus `<Enum>_<Variant>` constants, opaque structs as zero-size arrays, `static inline`
// helpers as extern functions). It is not the SDK's header and makes no claim about its layout: the
// constant values are placeholders, the opaque resource is a fixed-size blob, and nothing here may be
// linked. A build that links (`cargo build`, `cargo test`) with REGIA_DLSS_CHECK_STUB set fails on the
// missing NGX symbols, by design. When `src/ngx.rs` starts using another NGX item, add it here.
//
// Included into `ngx::sys`, whose inner attributes allow the C naming and the unused items.

/// The type bindgen gives a C enum: `int` under MSVC, `unsigned int` for these enums elsewhere
/// (ADR-7695). The stub follows the target so a Windows check catches what the real bindings do.
#[cfg(target_os = "windows")]
pub type CEnum = ::std::os::raw::c_int;
/// The type bindgen gives a C enum: `int` under MSVC, `unsigned int` for these enums elsewhere.
#[cfg(not(target_os = "windows"))]
pub type CEnum = ::std::os::raw::c_uint;
/// C `wchar_t` (UTF-16 on Windows, UTF-32 elsewhere).
#[cfg(target_os = "windows")]
pub type wchar_t = u16;
/// C `wchar_t` (UTF-16 on Windows, UTF-32 elsewhere).
#[cfg(not(target_os = "windows"))]
pub type wchar_t = ::std::os::raw::c_int;

pub type NVSDK_NGX_Result = CEnum;

pub type NVSDK_NGX_Feature = CEnum;
pub const NVSDK_NGX_Feature_NVSDK_NGX_Feature_SuperSampling: NVSDK_NGX_Feature = 1;

pub type NVSDK_NGX_Version = CEnum;
pub const NVSDK_NGX_Version_NVSDK_NGX_Version_API: NVSDK_NGX_Version = 1;

pub type NVSDK_NGX_EngineType = CEnum;
pub const NVSDK_NGX_EngineType_NVSDK_NGX_ENGINE_TYPE_CUSTOM: NVSDK_NGX_EngineType = 0;

pub type NVSDK_NGX_Logging_Level = CEnum;
pub const NVSDK_NGX_Logging_Level_NVSDK_NGX_LOGGING_LEVEL_OFF: NVSDK_NGX_Logging_Level = 0;

pub type NVSDK_NGX_Application_Identifier_Type = CEnum;
pub const NVSDK_NGX_Application_Identifier_Type_NVSDK_NGX_Application_Identifier_Type_Project_Id:
    NVSDK_NGX_Application_Identifier_Type = 1;

pub type NVSDK_NGX_PerfQuality_Value = CEnum;
pub const NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxPerf: NVSDK_NGX_PerfQuality_Value = 0;
pub const NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_Balanced: NVSDK_NGX_PerfQuality_Value = 1;
pub const NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxQuality: NVSDK_NGX_PerfQuality_Value = 2;
pub const NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_UltraPerformance:
    NVSDK_NGX_PerfQuality_Value = 3;
pub const NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_DLAA: NVSDK_NGX_PerfQuality_Value = 4;

pub type NVSDK_NGX_DLSS_Feature_Flags = ::std::os::raw::c_int;
pub const NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_IsHDR: NVSDK_NGX_DLSS_Feature_Flags = 1;
pub const NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_MVLowRes: NVSDK_NGX_DLSS_Feature_Flags =
    2;
pub const NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_MVJittered: NVSDK_NGX_DLSS_Feature_Flags =
    4;
pub const NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_DepthInverted:
    NVSDK_NGX_DLSS_Feature_Flags = 8;
pub const NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_AutoExposure:
    NVSDK_NGX_DLSS_Feature_Flags = 16;

pub type NVSDK_NGX_ToneMapperType = CEnum;
pub const NVSDK_NGX_ToneMapperType_NVSDK_NGX_TONEMAPPER_STRING: NVSDK_NGX_ToneMapperType = 0;

pub const NVSDK_NGX_Parameter_SuperSampling_Available: &[u8; 24] = b"SuperSampling.Available\0";
pub const NVSDK_NGX_Parameter_SuperSampling_NeedsUpdatedDriver: &[u8; 33] =
    b"SuperSampling.NeedsUpdatedDriver\0";

pub type NVSDK_NGX_AppLogCallback = ::std::option::Option<
    unsafe extern "C" fn(
        message: *const ::std::os::raw::c_char,
        level: NVSDK_NGX_Logging_Level,
        source: NVSDK_NGX_Feature,
    ),
>;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_PathListInfo {
    pub Path: *const *const wchar_t,
    pub Length: ::std::os::raw::c_uint,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_LoggingInfo {
    pub LoggingCallback: NVSDK_NGX_AppLogCallback,
    pub MinimumLoggingLevel: NVSDK_NGX_Logging_Level,
    pub DisableOtherLoggingSinks: bool,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_FeatureCommonInfo {
    pub PathListInfo: NVSDK_NGX_PathListInfo,
    pub InternalData: *mut ::std::os::raw::c_void,
    pub LoggingInfo: NVSDK_NGX_LoggingInfo,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_ProjectIdDescription {
    pub ProjectId: *const ::std::os::raw::c_char,
    pub EngineType: NVSDK_NGX_EngineType,
    pub EngineVersion: *const ::std::os::raw::c_char,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union NVSDK_NGX_Application_Identifier_v {
    pub ProjectDesc: NVSDK_NGX_ProjectIdDescription,
    pub ApplicationId: ::std::os::raw::c_ulonglong,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct NVSDK_NGX_Application_Identifier {
    pub IdentifierType: NVSDK_NGX_Application_Identifier_Type,
    pub v: NVSDK_NGX_Application_Identifier_v,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct NVSDK_NGX_FeatureDiscoveryInfo {
    pub SDKVersion: NVSDK_NGX_Version,
    pub FeatureID: NVSDK_NGX_Feature,
    pub Identifier: NVSDK_NGX_Application_Identifier,
    pub ApplicationDataPath: *const wchar_t,
    pub FeatureInfo: *const NVSDK_NGX_FeatureCommonInfo,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_Parameter {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_Handle {
    pub Id: ::std::os::raw::c_uint,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_Feature_Create_Params {
    pub InWidth: ::std::os::raw::c_uint,
    pub InHeight: ::std::os::raw::c_uint,
    pub InTargetWidth: ::std::os::raw::c_uint,
    pub InTargetHeight: ::std::os::raw::c_uint,
    pub InPerfQualityValue: NVSDK_NGX_PerfQuality_Value,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_DLSS_Create_Params {
    pub Feature: NVSDK_NGX_Feature_Create_Params,
    pub InFeatureCreateFlags: NVSDK_NGX_DLSS_Feature_Flags,
    pub InEnableOutputSubrects: bool,
}

/// An opaque stand-in for the Vulkan resource description (never passed to a real NGX).
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_Resource_VK {
    _opaque: [u64; 16],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_Coordinates {
    pub X: ::std::os::raw::c_uint,
    pub Y: ::std::os::raw::c_uint,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_Dimensions {
    pub Width: ::std::os::raw::c_uint,
    pub Height: ::std::os::raw::c_uint,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_VK_Feature_Eval_Params {
    pub pInColor: *mut NVSDK_NGX_Resource_VK,
    pub pInOutput: *mut NVSDK_NGX_Resource_VK,
    pub InSharpness: f32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_VK_GBuffer {
    pub pInAttrib: [*mut NVSDK_NGX_Resource_VK; 17],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NVSDK_NGX_VK_DLSS_Eval_Params {
    pub Feature: NVSDK_NGX_VK_Feature_Eval_Params,
    pub pInDepth: *mut NVSDK_NGX_Resource_VK,
    pub pInMotionVectors: *mut NVSDK_NGX_Resource_VK,
    pub InJitterOffsetX: f32,
    pub InJitterOffsetY: f32,
    pub InRenderSubrectDimensions: NVSDK_NGX_Dimensions,
    pub InReset: ::std::os::raw::c_int,
    pub InMVScaleX: f32,
    pub InMVScaleY: f32,
    pub pInTransparencyMask: *mut NVSDK_NGX_Resource_VK,
    pub pInExposureTexture: *mut NVSDK_NGX_Resource_VK,
    pub pInBiasCurrentColorMask: *mut NVSDK_NGX_Resource_VK,
    pub InColorSubrectBase: NVSDK_NGX_Coordinates,
    pub InDepthSubrectBase: NVSDK_NGX_Coordinates,
    pub InMVSubrectBase: NVSDK_NGX_Coordinates,
    pub InTranslucencySubrectBase: NVSDK_NGX_Coordinates,
    pub InBiasCurrentColorSubrectBase: NVSDK_NGX_Coordinates,
    pub InOutputSubrectBase: NVSDK_NGX_Coordinates,
    pub InPreExposure: f32,
    pub InExposureScale: f32,
    pub InIndicatorInvertXAxis: ::std::os::raw::c_int,
    pub InIndicatorInvertYAxis: ::std::os::raw::c_int,
    pub GBufferSurface: NVSDK_NGX_VK_GBuffer,
    pub InToneMapperType: NVSDK_NGX_ToneMapperType,
    pub pInMotionVectors3D: *mut NVSDK_NGX_Resource_VK,
    pub pInIsParticleMask: *mut NVSDK_NGX_Resource_VK,
    pub pInAnimatedTextureMask: *mut NVSDK_NGX_Resource_VK,
    pub pInDepthHighRes: *mut NVSDK_NGX_Resource_VK,
    pub pInPositionViewSpace: *mut NVSDK_NGX_Resource_VK,
    pub InFrameTimeDeltaInMsec: f32,
    pub pInRayTracingHitDistance: *mut NVSDK_NGX_Resource_VK,
    pub pInMotionVectorsReflections: *mut NVSDK_NGX_Resource_VK,
}

// SAFETY: declarations only, for type-checking `src/ngx.rs`; this stub is never linked (a linking
// build fails on the missing symbols, see the header), and each call site in `src/ngx.rs` states why
// its call is sound against the real SDK binding.
unsafe extern "C" {
    pub fn NVSDK_NGX_VULKAN_GetFeatureInstanceExtensionRequirements(
        FeatureDiscoveryInfo: *const NVSDK_NGX_FeatureDiscoveryInfo,
        OutExtensionCount: *mut u32,
        OutExtensionProperties: *mut *mut VkExtensionProperties,
    ) -> NVSDK_NGX_Result;

    pub fn NVSDK_NGX_VULKAN_GetFeatureDeviceExtensionRequirements(
        Instance: VkInstance,
        PhysicalDevice: VkPhysicalDevice,
        FeatureDiscoveryInfo: *const NVSDK_NGX_FeatureDiscoveryInfo,
        OutExtensionCount: *mut u32,
        OutExtensionProperties: *mut *mut VkExtensionProperties,
    ) -> NVSDK_NGX_Result;

    pub fn NVSDK_NGX_VULKAN_Init_with_ProjectID(
        InProjectId: *const ::std::os::raw::c_char,
        InEngineType: NVSDK_NGX_EngineType,
        InEngineVersion: *const ::std::os::raw::c_char,
        InApplicationDataPath: *const wchar_t,
        InInstance: VkInstance,
        InPD: VkPhysicalDevice,
        InDevice: VkDevice,
        InGIPA: PFN_vkGetInstanceProcAddr,
        InGDPA: PFN_vkGetDeviceProcAddr,
        InFeatureInfo: *const NVSDK_NGX_FeatureCommonInfo,
        InSDKVersion: NVSDK_NGX_Version,
    ) -> NVSDK_NGX_Result;

    pub fn NVSDK_NGX_VULKAN_GetCapabilityParameters(
        OutParameters: *mut *mut NVSDK_NGX_Parameter,
    ) -> NVSDK_NGX_Result;

    pub fn NVSDK_NGX_VULKAN_DestroyParameters(InParameters: *mut NVSDK_NGX_Parameter) -> NVSDK_NGX_Result;

    pub fn NVSDK_NGX_VULKAN_Shutdown1(InDevice: VkDevice) -> NVSDK_NGX_Result;

    pub fn NVSDK_NGX_VULKAN_ReleaseFeature(InHandle: *mut NVSDK_NGX_Handle) -> NVSDK_NGX_Result;

    pub fn NVSDK_NGX_Parameter_GetI(
        InParameter: *mut NVSDK_NGX_Parameter,
        InName: *const ::std::os::raw::c_char,
        OutValue: *mut ::std::os::raw::c_int,
    ) -> NVSDK_NGX_Result;

    pub fn NGX_DLSS_GET_OPTIMAL_SETTINGS(
        pInParams: *mut NVSDK_NGX_Parameter,
        InUserSelectedWidth: ::std::os::raw::c_uint,
        InUserSelectedHeight: ::std::os::raw::c_uint,
        InPerfQualityValue: NVSDK_NGX_PerfQuality_Value,
        pOutRenderOptimalWidth: *mut ::std::os::raw::c_uint,
        pOutRenderOptimalHeight: *mut ::std::os::raw::c_uint,
        pOutRenderMaxWidth: *mut ::std::os::raw::c_uint,
        pOutRenderMaxHeight: *mut ::std::os::raw::c_uint,
        pOutRenderMinWidth: *mut ::std::os::raw::c_uint,
        pOutRenderMinHeight: *mut ::std::os::raw::c_uint,
        pOutSharpness: *mut f32,
    ) -> NVSDK_NGX_Result;

    pub fn NGX_DLSS_GET_STATS(
        pInParams: *mut NVSDK_NGX_Parameter,
        pVRAMAllocatedBytes: *mut ::std::os::raw::c_ulonglong,
    ) -> NVSDK_NGX_Result;

    pub fn NVSDK_NGX_Create_ImageView_Resource_VK(
        imageView: VkImageView,
        image: VkImage,
        subresourceRange: VkImageSubresourceRange,
        format: VkFormat,
        width: ::std::os::raw::c_uint,
        height: ::std::os::raw::c_uint,
        readWrite: bool,
    ) -> NVSDK_NGX_Resource_VK;

    pub fn NGX_VULKAN_CREATE_DLSS_EXT1(
        device: VkDevice,
        InCmdList: VkCommandBuffer,
        InCreationNodeMask: ::std::os::raw::c_uint,
        InVisibilityNodeMask: ::std::os::raw::c_uint,
        ppOutHandle: *mut *mut NVSDK_NGX_Handle,
        pInParams: *mut NVSDK_NGX_Parameter,
        pInDlssCreateParams: *mut NVSDK_NGX_DLSS_Create_Params,
    ) -> NVSDK_NGX_Result;

    pub fn NGX_VULKAN_EVALUATE_DLSS_EXT(
        InCmdList: VkCommandBuffer,
        pInHandle: *mut NVSDK_NGX_Handle,
        pInParams: *mut NVSDK_NGX_Parameter,
        pInDlssEvalParams: *mut NVSDK_NGX_VK_DLSS_Eval_Params,
    ) -> NVSDK_NGX_Result;
}
