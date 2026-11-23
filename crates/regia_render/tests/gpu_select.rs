// SPDX-License-Identifier: MPL-2.0
//! `REGIA_GPU_INDEX` / `REGIA_GPU_NAME` on a real (lavapipe) device: an index that names no
//! adapter still brings a device up on the default pick and logs why.
//!
//! The variables are process-wide, so the parent test re-runs this binary's ignored child
//! test with the variable set and reads the child's log.

use std::process::Command;

/// Every GPU-selection variable, under the `REGIA_` name and the legacy `FERRITE_` name the
/// engine still honors. A child must start from none of them, or a pin exported by the gate
/// leaks into its selection.
const SELECTION_VARS: [&str; 5] = ["INDEX", "NAME", "PCI", "UUID", "STRICT"];

/// Removes every selection variable (both prefixes) from `command`, and the card runs'
/// `REGIA_REQUIRE_ADAPTER` (ADR-7613): the strict children expect the refusal as an error, which
/// that guard would turn into a panic.
fn clear_selection(command: &mut Command) -> &mut Command {
    for suffix in SELECTION_VARS {
        command
            .env_remove(format!("REGIA_GPU_{suffix}"))
            .env_remove(format!("FERRITE_GPU_{suffix}"));
    }
    command
        .env_remove("REGIA_REQUIRE_ADAPTER")
        .env_remove("FERRITE_REQUIRE_ADAPTER")
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn an_out_of_range_index_falls_back_with_a_warning() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let exe = std::env::current_exe().expect("test binary");
    let mut command = Command::new(exe);
    command.args([
        "--ignored",
        "--exact",
        "child_out_of_range_index",
        "--nocapture",
        "--test-threads=1",
    ]);
    clear_selection(&mut command);
    let out = command.env("REGIA_GPU_INDEX", "99").output().expect("run child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed:\n{stdout}\n{stderr}");
    assert!(
        stderr.contains("REGIA_GPU_INDEX=99") && stderr.contains("using the default adapter"),
        "no fallback warning in the log:\n{stderr}"
    );
}

#[test]
#[ignore = "run by an_out_of_range_index_falls_back_with_a_warning with REGIA_GPU_INDEX set"]
fn child_out_of_range_index() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if regia_core::brand::env_var_os("REGIA_GPU_INDEX").is_none() {
        return;
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .init();
    let device = regia_render::RenderDevice::headless().unwrap_or_else(|err| panic!("no device: {err}"));
    match device.adapter_selection() {
        regia_render::AdapterSelection::Fallback(reason) => assert!(reason.contains("99"), "{reason}"),
        other => panic!("expected a fallback, got {other:?}"),
    }
    // The device works.
    let _ = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM,
        mapped_at_creation: false,
    });
}

/// The listing is the doctor view: it names every adapter it could select.
#[test]
fn list_adapters_numbers_what_a_device_can_select() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let list = regia_render::list_adapters();
    for (i, adapter) in list.iter().enumerate() {
        assert_eq!(adapter.index, i);
        assert!(!adapter.name.is_empty());
        eprintln!("{adapter}");
    }
}

/// With `REGIA_GPU_STRICT` an unhonoured request is an error, never another card.
#[test]
fn strict_mode_refuses_instead_of_falling_back() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let exe = std::env::current_exe().expect("test binary");
    let mut command = Command::new(exe);
    command.args([
        "--ignored",
        "--exact",
        "child_strict",
        "--nocapture",
        "--test-threads=1",
    ]);
    clear_selection(&mut command);
    let out = command
        .env("REGIA_GPU_NAME", "no such gpu anywhere")
        .env("REGIA_GPU_STRICT", "1")
        .output()
        .expect("run child");
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A refused strict selection is written to the process's real stderr, past the test harness's
/// capture (ADR-5794): the child's test passes and prints nothing itself, and it runs without
/// `--nocapture`, so only a line written past the capture can reach the parent. A test that
/// skips on the error could otherwise lose its card in a card run and count as passed unseen.
#[test]
fn a_refused_strict_selection_is_reported_past_the_harness_capture() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let exe = std::env::current_exe().expect("test binary");
    let mut command = Command::new(exe);
    command.args(["--ignored", "--exact", "child_strict", "--test-threads=1"]);
    clear_selection(&mut command);
    let out = command
        .env("REGIA_GPU_NAME", "no such gpu anywhere")
        .env("REGIA_GPU_STRICT", "1")
        .output()
        .expect("run child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "the child's test ran and passed: {stdout}"
    );
    assert!(
        stderr
            .lines()
            .any(|line| line.starts_with("GPU selection failed: ") && line.contains("no such gpu")),
        "the refusal is not on the child's stderr: {stderr:?}"
    );
}

#[test]
#[ignore = "run by strict_mode_refuses_instead_of_falling_back"]
fn child_strict() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if regia_core::brand::env_var_os("REGIA_GPU_STRICT").is_none() {
        return;
    }
    match regia_render::RenderDevice::headless() {
        Err(regia_render::RenderError::GpuSelection(reason)) => assert!(reason.contains("no such gpu")),
        Err(other) => panic!("unexpected error {other}"),
        Ok(device) => panic!("strict mode came up on {:?}", device.adapter.get_info().name),
    }
}
