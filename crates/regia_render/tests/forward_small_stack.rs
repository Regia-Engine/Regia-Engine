// SPDX-License-Identifier: MPL-2.0
//! A Desktop High frame builds its pipelines and renders on a thread with a 2 MiB stack (ADR-5971).
//!
//! A game runs render code on threads with default stacks (2 MiB for a Rust thread, 1 MiB for the
//! Windows main thread), and on DX12 without DXC every shader goes through FXC on the thread that
//! creates the pipeline. FXC's optimizer recursed without end on the forward shader's cloud shadow
//! lookup (ledger row L04: `lightmap_capsule_occluders` overflowed libtest's 2 MiB test thread on
//! native DX12 while Vulkan passed), so no stack size was enough and the process died.
//!
//! A stack overflow kills the process, so the frame runs in a child: this test binary started again
//! with [`CHILD`] set, where the same test renders on a thread whose stack is [`STACK`] whatever
//! `RUST_MIN_STACK` says. The parent only reads how the child ended, so on a host where the frame
//! overflows this is a failed assertion with the child's output, not a dead test binary. It runs on
//! every backend (lavapipe in CI); on Windows DX12 with no `dxcompiler.dll` on `PATH` it is the FXC
//! check: `REGIA_GPU_INDEX=<the DX12 adapter> REGIA_GPU_STRICT=1`.

use std::process::Command;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

/// Set in the child: render instead of starting a child.
const CHILD: &str = "REGIA_FORWARD_SMALL_STACK_CHILD";
/// The render thread's stack: Rust's default for a spawned thread, and libtest's for a test.
const STACK: usize = 2 << 20;
const TEST: &str = "a_desktop_high_frame_renders_on_a_two_mebibyte_stack";
const W: u32 = 96;
const H: u32 = 64;

/// A lit floor under a sun: the full forward shader (`lab_forward.wgsl` `fs_main`), not a lite tier.
fn lab() -> LabScene {
    let n = [0.0, 1.0, 0.0];
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 2.4, 2.6],
        target: [0.0, 0.0, -0.2],
        fov_y: 0.9,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 500.0,
        double_sided: true,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.3, -1.0, -0.2], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 1.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([-4.0, 0.0, 4.0], n, [0.0, 1.0]),
                LabVertex::from_pos_normal_uv([4.0, 0.0, 4.0], n, [1.0, 1.0]),
                LabVertex::from_pos_normal_uv([4.0, 0.0, -4.0], n, [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([-4.0, 0.0, -4.0], n, [0.0, 0.0]),
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..LabMesh::unskinned()
        }],
    }
}

/// Creates the device and the renderer and renders two frames, all on the calling thread.
fn render_two_frames() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    eprintln!("forward_small_stack adapter: {:?}", device.adapter.get_info());
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let lab = lab();
    let scene = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    for frame in 0..2 {
        match renderer.render_offscreen(&device, &scene) {
            Ok(rendered) => assert_eq!(rendered.pixels.len(), (W * H * 4) as usize, "frame {frame}"),
            Err(err) => panic!("frame {frame}: {err:?}"),
        }
    }
    eprintln!("forward_small_stack: two frames on a {STACK}-byte stack");
}

#[test]
fn a_desktop_high_frame_renders_on_a_two_mebibyte_stack() {
    if std::env::var_os(CHILD).is_some() {
        let thread = std::thread::Builder::new()
            .name("small-stack-render".to_string())
            .stack_size(STACK)
            .spawn(render_two_frames);
        let thread = match thread {
            Ok(thread) => thread,
            Err(err) => panic!("no {STACK}-byte thread: {err}"),
        };
        if let Err(panic) = thread.join() {
            std::panic::resume_unwind(panic);
        }
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => panic!("this test binary's path: {err}"),
    };
    let child = Command::new(exe)
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output();
    let child = match child {
        Ok(child) => child,
        Err(err) => panic!("starting the child: {err}"),
    };
    let stderr = String::from_utf8_lossy(&child.stderr);
    // The child's adapter and result lines, so a log of this test names the backend it ran on.
    for line in stderr
        .lines()
        .filter(|line| line.starts_with("forward_small_stack"))
    {
        eprintln!("child: {line}");
    }
    let tail: Vec<&str> = stderr.lines().rev().take(12).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    assert!(
        child.status.success(),
        "the frame did not render on a {STACK}-byte stack: the child ended with {} (a stack overflow \
         is 0xc00000fd on Windows and SIGSEGV or SIGABRT elsewhere). Its last lines:\n{}",
        child.status,
        tail.join("\n")
    );
    assert!(
        stderr.contains("forward_small_stack: two frames"),
        "the child did not render (wrong test name?):\n{}",
        tail.join("\n")
    );
}
