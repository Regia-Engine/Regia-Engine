// SPDX-License-Identifier: MPL-2.0
//! Picking the GPU for headless, offscreen and windowed devices (`REGIA_GPU_*`).
//!
//! A bench machine with several cards (the build machines's RTX 5090s) needs one variable that
//! selects the same card for every tool: tests, `render_lab`, `lookdev`, `vfx_lab`,
//! `terrain_lab` and the terrain's block-compression device all build their device through
//! [`crate::RenderDevice::headless`]. Without a variable, wgpu's high-performance pick stays.
//!
//! - `REGIA_GPU_UUID=<GPU-…>`: an NVIDIA GPU by UUID or unique prefix. When
//!   `/proc/driver/nvidia` exists, Linux reads `gpus/*/information` and does not execute a
//!   process. An exact full UUID still matches when another card's file is malformed, unreadable
//!   or uninitialized (`GPU-????…`); a shorter prefix fails, and the error names that card.
//!   When `/proc/driver/nvidia` is absent (WSL2, some containers), Linux uses `nvidia-smi` with
//!   a three-second timeout, the same as other platforms.
//! - `REGIA_GPU_PCI=<domain:bus:device.function>`: the adapter on that PCI bus. This pin never
//!   launches a process. On Linux the optional UUID cross-check reads only that card's
//!   information file.
//! - `REGIA_GPU_INDEX=<n>`: the n-th adapter of [`list_adapters`] (Vulkan adapters first, then
//!   DX12 on Windows).
//! - `REGIA_GPU_NAME=<substring>`: the first adapter whose name contains it (case-insensitive).
//!
//! Precedence: PCI, UUID, index, name. An explicit PCI pin never runs a UUID subprocess.
//! On Linux a readable procfs UUID is only a cross-check of the pinned card: a mismatch warns,
//! and PCI wins even in strict mode. An unavailable cross-check is ignored. The pre-rename spellings
//! of these variables follow the same rules. With `REGIA_GPU_STRICT=1` a request that selects
//! nothing is an error instead of a fallback: shared machines (the build machines) must never
//! land on another card. A value that matches nothing logs a warning and falls
//! back to the default pick ([`AdapterSelection::Fallback`]). The Vulkan loader's own filter
//! (`VK_LOADER_DRIVERS_SELECT`) still applies first.

mod uuid;

/// The adapter index variable.
pub const GPU_INDEX_ENV: &str = "REGIA_GPU_INDEX";
/// The adapter name variable.
pub const GPU_NAME_ENV: &str = "REGIA_GPU_NAME";
/// The PCI bus variable.
pub const GPU_PCI_ENV: &str = "REGIA_GPU_PCI";
/// The NVIDIA UUID variable.
pub const GPU_UUID_ENV: &str = "REGIA_GPU_UUID";
/// Set to make an unhonoured request an error ([`crate::RenderError::GpuSelection`]).
pub const GPU_STRICT_ENV: &str = "REGIA_GPU_STRICT";

/// What the variables ask for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GpuRequest {
    /// `REGIA_GPU_UUID`, resolved to a PCI bus id (or the reason it could not be).
    /// With a PCI pin this is only an optional Linux procfs cross-check of that one card.
    pub uuid_pci: Option<Result<String, String>>,
    /// `REGIA_GPU_PCI`.
    pub pci: Option<String>,
    /// `REGIA_GPU_INDEX`.
    pub index: Option<String>,
    /// `REGIA_GPU_NAME`.
    pub name: Option<String>,
}

impl GpuRequest {
    /// Reads the variables, honoring PCI before resolving UUID (see the module docs).
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_env_with(
            |key| regia_core::brand::env_var(key).ok(),
            uuid::resolve_uuid,
            uuid::cross_check_uuid,
        )
    }

    // No process-wide env mutation in tests. The full resolver is never called for a PCI pin;
    // the separate cross-check reads only that card's procfs file and cannot spawn a process.
    fn from_env_with(
        get: impl Fn(&str) -> Option<String>,
        resolve: impl FnOnce(&str) -> Result<String, uuid::UuidError>,
        cross_check: impl FnOnce(&str, &str) -> Result<String, uuid::UuidError>,
    ) -> Self {
        let get = |key| get(key).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let pci = get(GPU_PCI_ENV);
        let uuid_pci = get(GPU_UUID_ENV).and_then(|uuid| {
            if let Some(pci) = &pci {
                cross_check(&uuid, pci).ok().map(Ok)
            } else {
                Some(resolve(&uuid).map_err(|err| format!("{GPU_UUID_ENV}={uuid}: {err}")))
            }
        });
        Self {
            uuid_pci,
            pci,
            index: get(GPU_INDEX_ENV),
            name: get(GPU_NAME_ENV),
        }
    }

    /// `true` when no variable is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.uuid_pci.is_none() && self.pci.is_none() && self.index.is_none() && self.name.is_none()
    }
}

/// `(domain, bus, device, function)` of a PCI bus id in either the Vulkan
/// (`0000:41:00.0`) or the `nvidia-smi` (`00000000:41:00.0`) spelling.
#[must_use]
pub fn parse_pci(id: &str) -> Option<(u32, u32, u32, u32)> {
    let id = id.trim();
    let (rest, function) = id.rsplit_once('.')?;
    let mut parts = rest.rsplitn(3, ':');
    let device = parts.next()?;
    let bus = parts.next()?;
    let domain = parts.next().unwrap_or("0");
    Some((
        u32::from_str_radix(domain, 16).ok()?,
        u32::from_str_radix(bus, 16).ok()?,
        u32::from_str_radix(device, 16).ok()?,
        u32::from_str_radix(function, 16).ok()?,
    ))
}

/// How a headless device's adapter was chosen.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum AdapterSelection {
    /// No variable: wgpu's high-performance pick (or a device built from parts).
    #[default]
    Default,
    /// `REGIA_GPU_INDEX=<n>`.
    Index(usize),
    /// `REGIA_GPU_NAME` matched this adapter name.
    Name(String),
    /// `REGIA_GPU_UUID` or `REGIA_GPU_PCI` selected the adapter on this PCI bus.
    Pci(String),
    /// A variable was set but could not be honoured (the reason); the default pick was used.
    Fallback(String),
    /// As `Fallback`, with `REGIA_GPU_STRICT` set: no device is created.
    Refused(String),
}

/// One adapter the machine offers, as [`list_adapters`] numbers it.
#[derive(Clone, Debug)]
pub struct AdapterSummary {
    /// The value `REGIA_GPU_INDEX` takes to select it.
    pub index: usize,
    /// Adapter name.
    pub name: String,
    /// Backend.
    pub backend: wgpu::Backend,
    /// Discrete, integrated, CPU (lavapipe), …
    pub device_type: wgpu::DeviceType,
    /// Driver name.
    pub driver: String,
    /// Driver version string.
    pub driver_info: String,
    /// PCI bus id (`domain:bus:device.function`), empty when the driver does not report it.
    pub pci_bus: String,
    /// D3D12 Resource Binding Tier (`None` off DX12): Tier 3 is what bindless needs.
    pub resource_binding_tier: Option<crate::capabilities::ResourceBindingTier>,
}

impl std::fmt::Display for AdapterSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} ({:?}, {:?}, pci {}, driver {} {})",
            self.index,
            self.name,
            self.backend,
            self.device_type,
            if self.pci_bus.is_empty() {
                "?"
            } else {
                &self.pci_bus
            },
            self.driver,
            self.driver_info
        )?;
        if let Some(tier) = self.resource_binding_tier {
            write!(f, ", resource binding {tier}")?;
        }
        Ok(())
    }
}

/// The adapters of `instance` in selection order: every adapter of the instance's backends, the
/// Vulkan ones first in wgpu's order, then DX12 on Windows
/// ([`crate::backend_policy::listing_rank`]). Listing DX12 too is what lets `REGIA_GPU_INDEX`
/// or `REGIA_GPU_NAME` force DX12 on a machine whose Vulkan works (ADR-201, ADR-296, ADR-326).
pub(crate) fn enumerate(instance: &wgpu::Instance) -> Vec<wgpu::Adapter> {
    let mut adapters = regia_core::tasks::block_on(instance.enumerate_adapters(crate::instance_backends()));
    adapters.sort_by_key(|a| crate::backend_policy::listing_rank(a.get_info().backend));
    adapters
}

/// Ranking of an adapter's device type for the default pick: higher is better (discrete GPU,
/// then integrated, virtual, other; software last).
fn device_type_rank(device_type: wgpu::DeviceType) -> u8 {
    match device_type {
        wgpu::DeviceType::DiscreteGpu => 4,
        wgpu::DeviceType::IntegratedGpu => 3,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 1,
        wgpu::DeviceType::Cpu => 0,
    }
}

/// The index of the best hardware adapter among `(backend, device type)` entries: the best device
/// class first (discrete, then integrated), and on equal class Vulkan before DX12 (ADR-2275). So DX12
/// is taken when Vulkan has no hardware adapter, or only a weaker one than a DX12 GPU (a laptop whose
/// NVIDIA GPU has no Vulkan ICD while the Intel iGPU does; Windows fallback backend; a software Vulkan adapter such as lavapipe does
/// not count, so a real DX12 GPU still beats it). `None` when there is no hardware adapter.
/// The backend order is [`crate::backend_policy::default_pick_rank`].
#[must_use]
pub fn pick_default_adapter(entries: &[(wgpu::Backend, wgpu::DeviceType)]) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, (_, t))| *t != wgpu::DeviceType::Cpu)
        .filter_map(|(i, (b, t))| crate::backend_policy::default_pick_rank(*b).map(|rank| (rank, *t, i)))
        .min_by_key(|(rank, t, i)| (std::cmp::Reverse(device_type_rank(*t)), *rank, *i))
        .map(|(_, _, i)| i)
}

/// The default hardware adapter when no `REGIA_GPU_*` variable chose one: Vulkan first, DX12
/// only when Vulkan has no hardware adapter that can present to `surface`
/// ([`pick_default_adapter`]). `None` leaves the choice to wgpu's `request_adapter` (software
/// fallback, or a platform with one backend).
pub(crate) fn default_adapter(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'_>>,
) -> Option<wgpu::Adapter> {
    if !crate::backend_policy::needs_own_default_pick(crate::instance_backends()) {
        return None;
    }
    let mut adapters: Vec<wgpu::Adapter> =
        regia_core::tasks::block_on(instance.enumerate_adapters(crate::instance_backends()))
            .into_iter()
            .filter(|a| surface.is_none_or(|s| a.is_surface_supported(s)))
            .collect();
    let entries: Vec<_> = adapters
        .iter()
        .map(|a| {
            let info = a.get_info();
            (info.backend, info.device_type)
        })
        .collect();
    pick_default_adapter(&entries).map(|i| adapters.swap_remove(i))
}

/// Every adapter a headless device could select, numbered for `REGIA_GPU_INDEX`. In a DLSS or
/// XeSS build the list is the full one too (the vendor hooks' Vulkan-only instance is not used
/// here), and an index of a DX12 adapter turns the hooks off (ADR-7820).
#[must_use]
pub fn list_adapters() -> Vec<AdapterSummary> {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = crate::instance_backends();
    // The engine's flags, so this reuses the headless devices' instance instead of creating one
    // of its own while a renderer may be naming objects on another thread (ADR-7557); without
    // the vendor hooks (ADR-7820), so the list numbers every adapter.
    descriptor.flags = crate::device::instance_flags();
    let instance = crate::device::create_listing_instance(descriptor);
    enumerate(&instance)
        .iter()
        .enumerate()
        .map(|(index, adapter)| {
            let info = adapter.get_info();
            AdapterSummary {
                index,
                name: info.name,
                backend: info.backend,
                device_type: info.device_type,
                driver: info.driver,
                driver_info: info.driver_info,
                pci_bus: info.device_pci_bus_id,
                resource_binding_tier: crate::capabilities::ResourceBindingTier::for_adapter(
                    info.backend,
                    &adapter.limits(),
                    adapter.features(),
                ),
            }
        })
        .collect()
}

/// Which adapter the request selects among `adapters` (`(name, pci bus)` in [`list_adapters`]
/// order): `Ok(None)` when nothing is requested, `Err(reason)` when the request is malformed or
/// matches nothing. Precedence: PCI, UUID, index, name. A conflicting UUID warns; PCI wins
/// even when `REGIA_GPU_STRICT` is set.
pub fn pick_adapter(adapters: &[(String, String)], request: &GpuRequest) -> Result<Option<usize>, String> {
    let listing = || {
        adapters
            .iter()
            .enumerate()
            .map(|(i, (n, pci))| format!("{i}: {n} @ {pci}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let by_pci = |wanted: &str, what: &str| {
        let Some(key) = parse_pci(wanted) else {
            return Err(format!("{what}={wanted} is not a PCI bus id"));
        };
        adapters
            .iter()
            .position(|(_, pci)| parse_pci(pci) == Some(key))
            .map(Some)
            .ok_or_else(|| format!("{what}={wanted} matches no adapter [{}]", listing()))
    };
    if let Some(pci) = &request.pci {
        if let Some(Ok(resolved)) = &request.uuid_pci
            && parse_pci(pci).is_some()
            && parse_pci(pci) != parse_pci(resolved)
        {
            tracing::warn!(target: "regia_render", "{GPU_UUID_ENV} resolves to {resolved}, but {GPU_PCI_ENV}={pci}; the explicit PCI pin wins (including strict mode)");
        }
        return by_pci(pci, GPU_PCI_ENV);
    }
    if let Some(resolved) = &request.uuid_pci {
        return match resolved {
            Ok(pci) => by_pci(pci, GPU_UUID_ENV),
            Err(reason) => Err(reason.clone()),
        };
    }
    if let Some(raw) = &request.index {
        let Ok(i) = raw.parse::<usize>() else {
            return Err(format!("{GPU_INDEX_ENV}={raw} is not an adapter index"));
        };
        return if i < adapters.len() {
            Ok(Some(i))
        } else {
            Err(format!(
                "{GPU_INDEX_ENV}={i} but there are {} adapters [{}]",
                adapters.len(),
                listing()
            ))
        };
    }
    if let Some(wanted) = &request.name {
        let needle = wanted.to_lowercase();
        return adapters
            .iter()
            .position(|(n, _)| n.to_lowercase().contains(&needle))
            .map(Some)
            .ok_or_else(|| format!("{GPU_NAME_ENV}={wanted} matches no adapter [{}]", listing()));
    }
    Ok(None)
}

/// Whether a vendor upscaler's Vulkan-only instance may stand in for the engine's instance
/// (ADR-7820). `adapters` is `(name, pci bus, backend)` of a full instance in [`list_adapters`]
/// order. The DLSS and XeSS creation hooks open Vulkan only, so with them every DX12 adapter
/// disappears and `REGIA_GPU_INDEX`/`REGIA_GPU_NAME` could not force DX12. When the request
/// selects an adapter on another backend the hook must not run; any other request (none, PCI,
/// UUID, an index or name of a Vulkan adapter, one that matches nothing) keeps the hook. Vulkan
/// adapters come first in the listing, so a Vulkan adapter keeps its index in the hooked instance.
#[must_use]
pub fn vendor_hook_keeps_selection(
    adapters: &[(String, String, wgpu::Backend)],
    request: &GpuRequest,
) -> bool {
    let described: Vec<(String, String)> = adapters
        .iter()
        .map(|(name, pci, _)| (name.clone(), pci.clone()))
        .collect();
    match pick_adapter(&described, request) {
        Ok(Some(index)) => adapters[index].2 == wgpu::Backend::Vulkan,
        Ok(None) | Err(_) => true,
    }
}

/// The adapter the variables select on `instance`, with how it was chosen. `None` with
/// [`AdapterSelection::Default`] means no variable is set; `None` with
/// [`AdapterSelection::Fallback`] means it could not be honoured (warned here).
pub(crate) fn select(instance: &wgpu::Instance) -> (Option<wgpu::Adapter>, AdapterSelection) {
    let request = GpuRequest::from_env();
    if request.is_empty() {
        return (None, AdapterSelection::Default);
    }
    let mut adapters = enumerate(instance);
    let described: Vec<(String, String)> = adapters
        .iter()
        .map(|a| {
            let info = a.get_info();
            (info.name, info.device_pci_bus_id)
        })
        .collect();
    match pick_adapter(&described, &request) {
        Ok(Some(i)) => {
            let selection = if request.uuid_pci.is_some() || request.pci.is_some() {
                AdapterSelection::Pci(described[i].1.clone())
            } else if request.index.is_some() {
                AdapterSelection::Index(i)
            } else {
                AdapterSelection::Name(described[i].0.clone())
            };
            (Some(adapters.swap_remove(i)), selection)
        }
        Ok(None) => (None, AdapterSelection::Default),
        Err(reason) => {
            if regia_core::brand::env_var_os(GPU_STRICT_ENV).is_some_and(|v| !v.is_empty() && v != "0") {
                tracing::error!(target: "regia_render", "{reason}; {GPU_STRICT_ENV} is set, no fallback");
                return (None, AdapterSelection::Refused(reason));
            }
            tracing::warn!(target: "regia_render", "{reason}; using the default adapter");
            (None, AdapterSelection::Fallback(reason))
        }
    }
}

/// [`select`] for a windowed device: `REGIA_GPU_*` applies to windows too (shared machines must
/// never land on another card). A selected adapter that cannot present to `surface` is a fallback
/// ([`AdapterSelection::Fallback`]), a refusal under `REGIA_GPU_STRICT`.
pub(crate) fn select_for_surface(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'_>>,
) -> (Option<wgpu::Adapter>, AdapterSelection) {
    match select(instance) {
        (Some(adapter), selection) if surface.is_some_and(|s| !adapter.is_surface_supported(s)) => {
            let strict =
                regia_core::brand::env_var_os(GPU_STRICT_ENV).is_some_and(|v| !v.is_empty() && v != "0");
            (None, unpresentable(&selection, strict))
        }
        other => other,
    }
}

/// What a selected adapter that cannot present to the window becomes: a fallback to the default
/// pick, or a refusal when `strict`.
fn unpresentable(selection: &AdapterSelection, strict: bool) -> AdapterSelection {
    let reason = format!("{selection:?} cannot present to this window");
    if strict {
        AdapterSelection::Refused(reason)
    } else {
        tracing::warn!(target: "regia_render", "{reason}; using the default adapter");
        AdapterSelection::Fallback(reason)
    }
}

/// The error of a strict selection that could not be honoured (`AdapterSelection::Refused`),
/// also written to the process's real stderr as the error's own text.
///
/// A caller may swallow the error: a test that cannot open its device prints a skip line and
/// returns, and the test harness shows a passing test's output to nobody. A card run could lose
/// its card that way and still report every test as passed (ADR-5794). `REGIA_GPU_STRICT` is
/// the operator saying "this card or nothing", so the refusal is reported where no capture hides
/// it: a log of the run has the line, and the gate's GPU pass fails on it (`tools/gaterig`).
pub(crate) fn refused(reason: &str) -> crate::RenderError {
    use std::io::Write as _;
    let error = crate::RenderError::GpuSelection(reason.to_owned());
    // Best effort: a closed stderr must not turn the refusal into a panic.
    let _ = std::io::stderr()
        .lock()
        .write_all(format!("{error}\n").as_bytes());
    error
}

/// The adapter a window's device should use: the `REGIA_GPU_*` choice when one is set and can
/// present to `surface`, else the default hardware pick ([`pick_default_adapter`]); `Ok(None)`
/// leaves the choice to `request_adapter` (software fallback). The editor window and any other
/// tool that builds its own surface device call this instead of `request_adapter` alone.
///
/// # Errors
///
/// [`crate::RenderError::GpuSelection`] when a variable cannot be honoured and
/// `REGIA_GPU_STRICT` is set.
pub fn adapter_for_surface(
    instance: &wgpu::Instance,
    surface: &wgpu::Surface<'_>,
) -> Result<(Option<wgpu::Adapter>, AdapterSelection), crate::RenderError> {
    let (chosen, selection) = select_for_surface(instance, Some(surface));
    if let AdapterSelection::Refused(reason) = &selection {
        return Err(refused(reason));
    }
    Ok((
        chosen.or_else(|| default_adapter(instance, Some(surface))),
        selection,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-7820: the RTX a test machine's listing (Vulkan, then the DX12 adapters). Index 1 forces
    /// DX12, so the vendor hook must step aside; index 0, a name (Vulkan matches first), PCI and
    /// an index past the end keep it.
    #[test]
    fn a_dx12_selection_turns_the_vendor_hook_off() {
        let listing = vec![
            (
                "NVIDIA GeForce RTX 3080 Ti".to_owned(),
                "0000:01:00.0".to_owned(),
                wgpu::Backend::Vulkan,
            ),
            (
                "NVIDIA GeForce RTX 3080 Ti".to_owned(),
                String::new(),
                wgpu::Backend::Dx12,
            ),
            (
                "Microsoft Basic Render Driver".to_owned(),
                String::new(),
                wgpu::Backend::Dx12,
            ),
        ];
        let by_index = |i: &str| GpuRequest {
            index: Some(i.to_owned()),
            ..GpuRequest::default()
        };
        let by_name = |n: &str| GpuRequest {
            name: Some(n.to_owned()),
            ..GpuRequest::default()
        };
        assert!(vendor_hook_keeps_selection(&listing, &GpuRequest::default()));
        assert!(vendor_hook_keeps_selection(&listing, &by_index("0")));
        assert!(!vendor_hook_keeps_selection(&listing, &by_index("1")));
        assert!(!vendor_hook_keeps_selection(&listing, &by_index("2")));
        assert!(vendor_hook_keeps_selection(&listing, &by_index("3")));
        assert!(vendor_hook_keeps_selection(&listing, &by_index("x")));
        assert!(vendor_hook_keeps_selection(&listing, &by_name("rtx 3080")));
        assert!(!vendor_hook_keeps_selection(&listing, &by_name("basic render")));
        let pci = GpuRequest {
            pci: Some("0000:01:00.0".to_owned()),
            index: Some("1".to_owned()),
            ..GpuRequest::default()
        };
        assert!(
            vendor_hook_keeps_selection(&listing, &pci),
            "PCI takes precedence over the index"
        );
    }

    fn adapters() -> Vec<(String, String)> {
        [
            ("NVIDIA GeForce RTX 5090", "0000:01:00.0"),
            ("NVIDIA GeForce RTX 5090", "0000:21:00.0"),
            ("AMD Radeon RX 7900 XTX (RADV NAVI31)", "0000:03:00.0"),
            ("llvmpipe (LLVM 23.1.1, 256 bits)", ""),
        ]
        .into_iter()
        .map(|(n, p)| (n.to_owned(), p.to_owned()))
        .collect()
    }

    fn by(f: impl FnOnce(&mut GpuRequest)) -> GpuRequest {
        let mut request = GpuRequest::default();
        f(&mut request);
        request
    }

    #[test]
    fn vulkan_hardware_wins_and_dx12_is_the_fallback() {
        use wgpu::Backend::{Dx12, Vulkan};
        use wgpu::DeviceType::{Cpu, DiscreteGpu, IntegratedGpu};
        // Vulkan present: the best Vulkan adapter, even if DX12 lists a discrete GPU first.
        assert_eq!(
            pick_default_adapter(&[
                (Dx12, DiscreteGpu),
                (Vulkan, IntegratedGpu),
                (Vulkan, DiscreteGpu)
            ]),
            Some(2)
        );
        // Vulkan has only the integrated GPU, DX12 lists the discrete one: the discrete GPU.
        assert_eq!(
            pick_default_adapter(&[
                (Vulkan, IntegratedGpu),
                (Dx12, IntegratedGpu),
                (Dx12, DiscreteGpu)
            ]),
            Some(2)
        );
        // Same class on both backends: Vulkan.
        assert_eq!(
            pick_default_adapter(&[(Dx12, DiscreteGpu), (Vulkan, DiscreteGpu)]),
            Some(1)
        );
        // No Vulkan hardware (or only lavapipe): DX12.
        assert_eq!(
            pick_default_adapter(&[(Vulkan, Cpu), (Dx12, DiscreteGpu)]),
            Some(1)
        );
        assert_eq!(
            pick_default_adapter(&[(Dx12, IntegratedGpu), (Dx12, DiscreteGpu)]),
            Some(1)
        );
        // Ties keep enumeration order; software only leaves the choice to wgpu.
        assert_eq!(
            pick_default_adapter(&[(Vulkan, DiscreteGpu), (Vulkan, DiscreteGpu)]),
            Some(0)
        );
        assert_eq!(pick_default_adapter(&[(Vulkan, Cpu)]), None);
        assert_eq!(pick_default_adapter(&[]), None);
    }

    #[test]
    fn an_unpresentable_selection_falls_back_or_is_refused() {
        let picked = AdapterSelection::Index(1);
        assert!(
            matches!(unpresentable(&picked, false), AdapterSelection::Fallback(r) if r.contains("Index(1)"))
        );
        assert!(matches!(
            unpresentable(&picked, true),
            AdapterSelection::Refused(_)
        ));
    }

    #[test]
    fn nothing_set_keeps_the_default_pick() {
        assert_eq!(pick_adapter(&adapters(), &GpuRequest::default()), Ok(None));
    }

    #[test]
    fn the_index_selects_and_wins_over_the_name() {
        assert_eq!(
            pick_adapter(&adapters(), &by(|r| r.index = Some("1".into()))),
            Ok(Some(1))
        );
        let both = by(|r| {
            r.index = Some("3".into());
            r.name = Some("radeon".into());
        });
        assert_eq!(pick_adapter(&adapters(), &both), Ok(Some(3)));
    }

    #[test]
    fn the_name_matches_a_case_insensitive_substring_first_hit() {
        assert_eq!(
            pick_adapter(&adapters(), &by(|r| r.name = Some("radeon".into()))),
            Ok(Some(2))
        );
        assert_eq!(
            pick_adapter(&adapters(), &by(|r| r.name = Some("RTX 5090".into()))),
            Ok(Some(0))
        );
        assert_eq!(
            pick_adapter(&adapters(), &by(|r| r.name = Some("LLVMPIPE".into()))),
            Ok(Some(3))
        );
    }

    #[test]
    fn pci_and_uuid_select_by_bus_in_either_spelling_and_win() {
        let pci = by(|r| {
            r.pci = Some("00000000:21:00.0".into());
            r.index = Some("0".into());
        });
        assert_eq!(pick_adapter(&adapters(), &pci), Ok(Some(1)));
        let uuid = by(|r| r.uuid_pci = Some(Ok("00000000:03:00.0".into())));
        assert_eq!(pick_adapter(&adapters(), &uuid), Ok(Some(2)));
        let unresolved = by(|r| r.uuid_pci = Some(Err("no nvidia-smi".into())));
        assert!(pick_adapter(&adapters(), &unresolved).is_err());
        assert_eq!(parse_pci("0000:41:00.0"), parse_pci("00000000:41:00.0"));
        assert_eq!(parse_pci("41:00.1"), Some((0, 0x41, 0, 1)));
    }

    #[test]
    fn unusable_values_explain_themselves() {
        let out_of_range = pick_adapter(&adapters(), &by(|r| r.index = Some("9".into()))).expect_err("range");
        assert!(out_of_range.contains("there are 4 adapters") && out_of_range.contains("3: llvmpipe"));
        assert!(pick_adapter(&adapters(), &by(|r| r.index = Some("two".into()))).is_err());
        assert!(pick_adapter(&adapters(), &by(|r| r.name = Some("arc a770".into()))).is_err());
        assert!(pick_adapter(&adapters(), &by(|r| r.pci = Some("0000:99:00.0".into()))).is_err());
    }
}

#[cfg(test)]
#[path = "gpu_select/regression_tests.rs"]
mod regression_tests;
