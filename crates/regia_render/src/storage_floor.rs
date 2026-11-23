// SPDX-License-Identifier: MPL-2.0
//! The Android storage-buffer floor (`specs/13` § 2, ADR-7211).
//!
//! The engine's shaders read storage buffers in the vertex and fragment stages (froxel light
//! lists, the object and material arenas, skin palettes). Android opens Vulkan only, and wgpu's
//! Vulkan backend always reports fragment storage buffers, but a device can still expose fewer
//! storage buffers per stage than the engine's layouts bind. Instead of failing later at layout
//! creation, [`android_storage_refusal`] names the shortfall and the device creation logs it and
//! refuses to start (`RenderError::GpuSelection`).
//!
//! The floor is the Mobile tier's own clamp (`MinspecProfile` mobile, `minspec.rs`): every layout
//! the Mobile profile builds fits in [`ANDROID_MIN_STORAGE_BUFFERS_PER_STAGE`] storage buffers per
//! stage, and that profile runs on lavapipe in CI.

/// Storage buffers per shader stage an Android device must expose (the WebGPU default limit and
/// the Mobile min-spec clamp).
pub const ANDROID_MIN_STORAGE_BUFFERS_PER_STAGE: u32 = 8;

/// Why an Android device cannot run the engine's storage-buffer layouts, or `None` when it can:
/// no fragment-stage storage buffers (`DownlevelFlags::FRAGMENT_STORAGE` missing), or fewer than
/// [`ANDROID_MIN_STORAGE_BUFFERS_PER_STAGE`] per stage.
#[must_use]
pub fn android_storage_refusal(limits: &wgpu::Limits, downlevel: wgpu::DownlevelFlags) -> Option<String> {
    if !downlevel.contains(wgpu::DownlevelFlags::FRAGMENT_STORAGE) {
        return Some(
            "this GPU has no storage buffers in fragment shaders; Regia Engine needs them on Android \
             (specs/13 § 2 minimum spec)"
                .to_string(),
        );
    }
    let have = limits.max_storage_buffers_per_shader_stage;
    (have < ANDROID_MIN_STORAGE_BUFFERS_PER_STAGE).then(|| {
        format!(
            "this GPU exposes {have} storage buffers per shader stage; Regia Engine needs at least \
             {ANDROID_MIN_STORAGE_BUFFERS_PER_STAGE} on Android (specs/13 § 2 minimum spec)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(storage: u32) -> wgpu::Limits {
        wgpu::Limits {
            max_storage_buffers_per_shader_stage: storage,
            ..wgpu::Limits::default()
        }
    }

    #[test]
    fn a_device_below_the_floor_is_refused_with_its_count() {
        let flags = wgpu::DownlevelFlags::all();
        let refusal = android_storage_refusal(&limits(4), flags).expect("4 is below the floor");
        assert!(refusal.contains("exposes 4 storage buffers"), "{refusal}");
        assert!(refusal.contains("at least 8"), "{refusal}");
        assert!(android_storage_refusal(&limits(7), flags).is_some());
        assert!(android_storage_refusal(&limits(0), flags).is_some());
    }

    #[test]
    fn a_device_at_or_above_the_floor_starts() {
        let flags = wgpu::DownlevelFlags::all();
        assert_eq!(android_storage_refusal(&limits(8), flags), None);
        assert_eq!(android_storage_refusal(&limits(1_048_576), flags), None);
        // The Mobile min-spec clamp is exactly the floor.
        let mobile = crate::minspec::MinspecProfile::Mobile.clamp_limits(&limits(64));
        assert_eq!(android_storage_refusal(&mobile, flags), None);
    }

    #[test]
    fn a_device_without_fragment_storage_is_refused() {
        let flags = wgpu::DownlevelFlags::all() - wgpu::DownlevelFlags::FRAGMENT_STORAGE;
        let refusal = android_storage_refusal(&limits(64), flags).expect("no fragment storage");
        assert!(refusal.contains("fragment shaders"), "{refusal}");
    }
}
