// SPDX-License-Identifier: MPL-2.0
//! `render.swapchain`: what each window's swapchain is configured with and why (ADR-525, book
//! `agents/swapchain.md`, guide topic `swapchain`).
//!
//! The values come from [`WindowSurfaces::swapchains`]: the plan the renderer made from the surface
//! capabilities, [`regia_render::OutputSettings`], the window's present mode, the backend and the
//! display's HDR report. A headless app has no window surfaces and answers an empty list.

use regia_ecs::prelude::*;
use regia_render::swapchain::{DisplayHdrSummary, SwapchainInfo};
use regia_render::{HdrOutput, PresentPolicy, WindowSurfaces};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn policy_name(policy: PresentPolicy) -> &'static str {
    match policy {
        PresentPolicy::Vsync => "vsync",
        PresentPolicy::Uncapped => "uncapped",
        PresentPolicy::Vrr => "vrr",
    }
}

fn hdr_name(hdr: HdrOutput) -> &'static str {
    match hdr {
        HdrOutput::Sdr => "sdr",
        HdrOutput::Hdr10 => "hdr10",
        HdrOutput::ScRgb => "scrgb",
    }
}

fn window_present_name(mode: regia_platform::PresentMode) -> &'static str {
    match mode {
        regia_platform::PresentMode::Vsync => "vsync",
        regia_platform::PresentMode::VsyncRelaxed => "vsync_relaxed",
        regia_platform::PresentMode::Immediate => "immediate",
    }
}

fn display_json(display: &DisplayHdrSummary) -> Value {
    json!({
        "hdr_mode": display.hdr_mode,
        "peak_nits": display.peak_nits,
        "full_frame_nits": display.full_frame_nits,
        "min_nits": display.min_nits,
        "sdr_white_nits": display.sdr_white_nits,
        "bits_per_color": display.bits_per_color,
    })
}

/// One window's swapchain as JSON. wgpu enums use their wgpu names (`Bgra8UnormSrgb`,
/// `Bt2100Pq`, `AutoVsync`, `Opaque`).
fn info_json(window: Entity, info: &SwapchainInfo) -> Value {
    let plan = &info.plan;
    json!({
        "window": window.to_bits(),
        "backend": info.request.backend.to_str(),
        "size": [info.size.0, info.size.1],
        "format": format!("{:?}", plan.format),
        "color_space": format!("{:?}", plan.color_space),
        "present_mode": format!("{:?}", plan.present_mode),
        "present_mode_effective": format!("{:?}", info.effective_present_mode()),
        "alpha_mode": format!("{:?}", plan.alpha_mode),
        "frame_latency": plan.frame_latency,
        "copy_src": plan.copy_src,
        "requested": {
            "present": policy_name(info.request.present),
            "window_present": window_present_name(info.request.window_present),
            "hdr": hdr_name(info.request.hdr),
        },
        "hdr_active": plan.hdr_active,
        "hdr_fallback": plan.hdr_fallback.map(|f| f.code()),
        "offered": {
            "present_modes": info
                .offered_present_modes
                .iter()
                .map(|m| format!("{m:?}"))
                .collect::<Vec<_>>(),
            "hdr": info.offered_hdr.iter().map(|&h| hdr_name(h)).collect::<Vec<_>>(),
        },
        "display": display_json(&info.display),
    })
}

/// `render.swapchain`: every live window swapchain (format, color space, present mode, alpha,
/// frame latency), what was requested, why an HDR request ended as SDR, what the surface offers
/// and what the display reports.
pub(crate) fn swapchain(world: &mut World) -> Result<Value, RpcError> {
    let windows: Vec<Value> = world
        .get_resource::<WindowSurfaces>()
        .map(WindowSurfaces::swapchains)
        .unwrap_or_default()
        .iter()
        .map(|(window, info)| info_json(*window, info))
        .collect();
    let count = windows.len();
    Ok(json!({ "windows": windows, "count": count }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::swapchain::{HdrFallback, SwapchainPlan, SwapchainRequest};

    #[test]
    fn a_world_without_window_surfaces_answers_an_empty_list() {
        let mut world = World::new();
        let result = swapchain(&mut world).expect("never fails"); // invariant: the handler has no error path
        assert_eq!(result["count"], 0);
        assert_eq!(result["windows"], json!([]));
        world.insert_resource(WindowSurfaces::default());
        let result = swapchain(&mut world).expect("never fails"); // invariant: the handler has no error path
        assert_eq!(result["count"], 0);
    }

    #[test]
    fn a_dx12_fallback_reads_back_with_codes_and_wgpu_names() {
        let info = SwapchainInfo {
            request: SwapchainRequest {
                backend: wgpu::Backend::Dx12,
                present: PresentPolicy::Vrr,
                window_present: regia_platform::PresentMode::Vsync,
                hdr: HdrOutput::Hdr10,
                display_hdr: Some(false),
            },
            plan: SwapchainPlan {
                format: wgpu::TextureFormat::Bgra8UnormSrgb,
                color_space: wgpu::SurfaceColorSpace::Auto,
                present_mode: wgpu::PresentMode::Immediate,
                alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                frame_latency: 2,
                copy_src: true,
                hdr_active: false,
                hdr_fallback: Some(HdrFallback::DisplaySdr),
            },
            size: (1920, 1080),
            offered_present_modes: vec![
                wgpu::PresentMode::Mailbox,
                wgpu::PresentMode::Fifo,
                wgpu::PresentMode::Immediate,
            ],
            offered_hdr: vec![HdrOutput::Hdr10, HdrOutput::ScRgb],
            display: DisplayHdrSummary {
                hdr_mode: Some(false),
                peak_nits: Some(270.0),
                ..Default::default()
            },
        };
        let window = World::new().spawn_empty().id();
        let v = info_json(window, &info);
        assert_eq!(v["backend"], "dx12");
        assert_eq!(v["size"], json!([1920, 1080]));
        assert_eq!(v["format"], "Bgra8UnormSrgb");
        assert_eq!(v["color_space"], "Auto");
        assert_eq!(v["present_mode"], "Immediate");
        assert_eq!(v["present_mode_effective"], "Immediate");
        assert_eq!(v["alpha_mode"], "Opaque");
        assert_eq!(v["frame_latency"], 2);
        assert_eq!(v["requested"]["present"], "vrr");
        assert_eq!(v["requested"]["window_present"], "vsync");
        assert_eq!(v["requested"]["hdr"], "hdr10");
        assert_eq!(v["hdr_active"], false);
        assert_eq!(v["hdr_fallback"], "display_sdr");
        assert_eq!(v["offered"]["hdr"], json!(["hdr10", "scrgb"]));
        assert_eq!(
            v["offered"]["present_modes"],
            json!(["Mailbox", "Fifo", "Immediate"])
        );
        assert_eq!(v["display"]["hdr_mode"], false);
        assert_eq!(v["display"]["peak_nits"], 270.0);
        assert_eq!(v["display"]["sdr_white_nits"], Value::Null);
        assert_eq!(v["window"], window.to_bits());
    }
}
