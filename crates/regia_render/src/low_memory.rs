// SPDX-License-Identifier: MPL-2.0
//! Defaults for integrated (shared-memory) GPUs on desktop, ADR-223 (v0.1.16).
//!
//! An integrated GPU's "VRAM" is system RAM: every byte the renderer holds is a byte the game,
//! the OS and the other apps do not have. A DO27 tester's Radeon 610M (5.7 GB of RAM, a 3.15 GB
//! device-local heap) ran out of GPU memory and lost its device with the Desktop Low texture
//! streaming budget (1 GiB, eager first use) and a scene warm that uploaded 400 MB of meshes in
//! five frames. So on an integrated GPU, unless `REGIA_INTEGRATED_LIMITS=0` ([`ENV_INTEGRATED_LIMITS`]):
//!
//! - the texture streaming budget is at most [`INTEGRATED_STREAM_BUDGET_MIB`] and never more than
//!   the device-local heap or a quarter of system RAM
//!   ([`crate::TextureStreaming::for_integrated_gpu`]); first use uploads the low mips, not
//!   everything the budget has room for (`eager` off);
//! - the new-scene warm on a scene-less window is off by default
//!   ([`crate::GameFrameWarm::on_new_scene`]; `REGIA_SCENE_WARM=1` turns it back on).
//!
//! Not on a Steam Deck (its own tuned Deck tier, 16 GB of RAM) and not on Android (the mobile
//! tiers are already sized for unified memory). Discrete, virtual and software adapters are
//! untouched. Nor on a large integrated GPU: a machine with [`INTEGRATED_LARGE_RAM_MIB`] of RAM
//! (about 16 GB) or an adapter reporting [`INTEGRATED_LARGE_HEAP_MIB`] of device-local heap
//! (Apple M-series with 16 GB or more, big APUs) keeps the presets. `REGIA_INTEGRATED_LIMITS=1`
//! applies the defaults whatever the size.

use std::sync::OnceLock;

use crate::quality::DeviceClass;

/// The texture streaming budget of an integrated GPU at most, MiB.
pub const INTEGRATED_STREAM_BUDGET_MIB: u32 = 384;

/// An integrated GPU's streaming budget is never above system RAM over this.
pub const INTEGRATED_RAM_SHARE_DIVISOR: u64 = 4;

/// An integrated GPU's streaming budget never goes below this, MiB, whatever the heap and RAM
/// say (a budget under the sets' floors only keeps every material at its floor).
pub const INTEGRATED_BUDGET_FLOOR_MIB: u32 = 64;

/// Environment switch: `0` (or `false`, `no`, `off`) turns the integrated-GPU defaults off (the
/// texture budget and the scene-warm default; the presets apply as on any other adapter); `1`
/// (or `true`, `yes`, `on`) applies them to every integrated GPU, large ones too.
pub const ENV_INTEGRATED_LIMITS: &str = "REGIA_INTEGRATED_LIMITS";

/// AMD's PCI vendor id.
const AMD: u32 = 0x1002;

/// Steam Deck GPU device ids: LCD (Van Gogh) and OLED (Sephiroth).
const DECK_DEVICES: [u32; 2] = [0x163f, 0x1435];

/// Total system RAM in MiB from which an integrated GPU counts as large (Apple M-series with 16 GB
/// or more, big APUs): it keeps the preset budgets. A "16 GB" machine reports a little less, so
/// the threshold sits under 16 GiB.
pub const INTEGRATED_LARGE_RAM_MIB: u64 = 15_000;

/// Device-local heap in MiB from which an integrated GPU counts as large, whatever the RAM says.
pub const INTEGRATED_LARGE_HEAP_MIB: u64 = 8 * 1024;

/// An environment switch as a flag: `Some(true)` for `1`/`true`/`yes`/`on`, `Some(false)` for
/// `0`/`false`/`no`/`off` (any case, trimmed), `None` when unset or anything else.
#[must_use]
pub(crate) fn parse_env_flag(value: Option<&str>) -> Option<bool> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Whether an adapter of `class` with PCI ids `vendor`:`device` gets the integrated-GPU
/// defaults, not knowing its memory sizes ([`integrated_limits_apply_sized`] is what the
/// renderer asks): an integrated GPU on desktop that is not a Steam Deck, unless
/// [`ENV_INTEGRATED_LIMITS`] is off.
#[must_use]
pub fn integrated_limits_apply(class: DeviceClass, vendor: u32, device: u32) -> bool {
    integrated_limits_apply_sized(class, vendor, device, None)
}

/// [`integrated_limits_apply`] with the adapter's device-local heap in MiB (`None`: unknown) and
/// this machine's RAM ([`system_ram_mib`]): an integrated GPU that is large
/// ([`INTEGRATED_LARGE_RAM_MIB`] of RAM or [`INTEGRATED_LARGE_HEAP_MIB`] of heap: Apple M-series
/// with 16 GB or more, big APUs) keeps the preset budgets. [`ENV_INTEGRATED_LIMITS`]: `0` /
/// `false` / `no` / `off` never applies the defaults, `1` / `true` / `yes` / `on` applies them
/// whatever the size.
#[must_use]
pub fn integrated_limits_apply_sized(
    class: DeviceClass,
    vendor: u32,
    device: u32,
    heap_mib: Option<u64>,
) -> bool {
    integrated_limits_apply_sized_with(
        class,
        vendor,
        device,
        cfg!(target_os = "android"),
        regia_core::brand::env_var("SteamDeck").is_ok_and(|v| v == "1"),
        regia_core::brand::env_var(ENV_INTEGRATED_LIMITS).ok().as_deref(),
        system_ram_mib(),
        heap_mib,
    )
}

/// [`integrated_limits_apply`] with the platform and environment given (testable), memory sizes
/// unknown.
#[must_use]
pub fn integrated_limits_apply_with(
    class: DeviceClass,
    vendor: u32,
    device: u32,
    android: bool,
    steam_deck_env: bool,
    switch: Option<&str>,
) -> bool {
    integrated_limits_apply_sized_with(class, vendor, device, android, steam_deck_env, switch, None, None)
}

/// [`integrated_limits_apply_sized`] with the platform, environment and sizes given (testable).
#[must_use]
#[allow(clippy::too_many_arguments)] // every input of the decision, so tests can set each
pub fn integrated_limits_apply_sized_with(
    class: DeviceClass,
    vendor: u32,
    device: u32,
    android: bool,
    steam_deck_env: bool,
    switch: Option<&str>,
    ram_mib: Option<u64>,
    heap_mib: Option<u64>,
) -> bool {
    let deck = steam_deck_env || (vendor == AMD && DECK_DEVICES.contains(&device));
    if class != DeviceClass::Integrated || android || deck {
        return false;
    }
    match parse_env_flag(switch) {
        Some(forced) => forced,
        None => {
            let large = ram_mib.is_some_and(|ram| ram >= INTEGRATED_LARGE_RAM_MIB)
                || heap_mib.is_some_and(|heap| heap >= INTEGRATED_LARGE_HEAP_MIB);
            !large
        }
    }
}

/// [`integrated_limits_apply_sized`] for a live adapter.
#[must_use]
pub fn integrated_limits_apply_to(info: &wgpu::AdapterInfo, heap_mib: Option<u64>) -> bool {
    integrated_limits_apply_sized(
        DeviceClass::from_wgpu(info.device_type),
        info.vendor,
        info.device,
        heap_mib,
    )
}

/// Physical RAM of this machine in MiB (`None` when the OS does not say), read once.
#[must_use]
pub fn system_ram_mib() -> Option<u64> {
    static RAM: OnceLock<Option<u64>> = OnceLock::new();
    *RAM.get_or_init(|| {
        let bytes = regia_platform::OsInfo::detect().total_ram_bytes;
        (bytes > 0).then_some(bytes >> 20)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_desktop_integrated_gpus_that_are_not_a_deck() {
        let apply =
            |class, vendor, device| integrated_limits_apply_with(class, vendor, device, false, false, None);
        // The DO27 tester's Radeon 610M.
        assert!(apply(DeviceClass::Integrated, AMD, 0x1506));
        assert!(apply(DeviceClass::Integrated, 0x8086, 0x46a6), "Intel Iris Xe");
        assert!(
            !apply(DeviceClass::Discrete, AMD, 0x744c),
            "discrete cards are untouched"
        );
        assert!(!apply(DeviceClass::Cpu, 0x10005, 0), "lavapipe is untouched");
        assert!(!apply(DeviceClass::Virtual, 0, 0));
        assert!(!apply(DeviceClass::Integrated, AMD, 0x163f), "Steam Deck LCD");
        assert!(!apply(DeviceClass::Integrated, AMD, 0x1435), "Steam Deck OLED");
        assert!(
            !integrated_limits_apply_with(DeviceClass::Integrated, AMD, 0x1506, false, true, None),
            "SteamDeck=1"
        );
        assert!(
            !integrated_limits_apply_with(DeviceClass::Integrated, 0x13b5, 0, true, false, None),
            "Android keeps its mobile tiers"
        );
        assert!(
            !integrated_limits_apply_with(DeviceClass::Integrated, AMD, 0x1506, false, false, Some("0")),
            "REGIA_INTEGRATED_LIMITS=0"
        );
        assert!(integrated_limits_apply_with(
            DeviceClass::Integrated,
            AMD,
            0x1506,
            false,
            false,
            Some("1")
        ));
    }

    #[test]
    fn large_integrated_gpus_keep_the_presets_unless_forced() {
        let apply = |ram, heap, switch| {
            integrated_limits_apply_sized_with(
                DeviceClass::Integrated,
                0x106b,
                0,
                false,
                false,
                switch,
                ram,
                heap,
            )
        };
        // The 610M tester: 5.7 GB of RAM, a 3.1 GB heap.
        assert!(apply(Some(5877), Some(3153), None));
        assert!(apply(Some(8192), None, None), "an 8 GB Apple M1");
        assert!(apply(None, None, None), "unknown sizes: the defaults apply");
        assert!(!apply(Some(16 * 1024), None, None), "16 GB of RAM");
        assert!(
            !apply(Some(15_800), Some(5000), None),
            "a 16 GB machine reports a little less"
        );
        assert!(!apply(Some(12_000), Some(9000), None), "a big heap");
        assert!(apply(Some(32 * 1024), None, Some("1")), "forced on");
        assert!(apply(Some(32 * 1024), None, Some("on")));
        assert!(!apply(Some(5877), None, Some("off")), "opted out");
        assert!(!apply(Some(5877), None, Some("false")));
    }

    #[test]
    fn env_flags_accept_the_usual_spellings() {
        for on in ["1", "true", "YES", " On "] {
            assert_eq!(parse_env_flag(Some(on)), Some(true), "{on}");
        }
        for off in ["0", "false", "No", "OFF"] {
            assert_eq!(parse_env_flag(Some(off)), Some(false), "{off}");
        }
        assert_eq!(parse_env_flag(Some("maybe")), None);
        assert_eq!(parse_env_flag(Some("")), None);
        assert_eq!(parse_env_flag(None), None);
    }
}
