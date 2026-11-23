// SPDX-License-Identifier: MPL-2.0
//! Spec 54 § 4.2: the `.regiatrace` header names the adapter. The telemetry plugin hands the
//! render device's adapter to the process recorder when it installs the device hooks; before
//! that fix the header's `adapter` was always empty. GPU (lavapipe counts); skips without an
//! adapter. Its own test binary: the process recorder is global state.

use regia_app::prelude::*;
use regia_telemetry::trace::{TraceConfig, adapter_label, configure, read_trace, recorder};
use regia_telemetry::{TelemetryOptions, TelemetryPlugin};
use regia_test_utils::unique_temp_dir;

#[test]
fn the_adapter_label_names_the_device_backend_and_driver() {
    let mut info = wgpu::AdapterInfo::new(wgpu::DeviceType::Cpu, wgpu::Backend::Vulkan);
    info.name = "llvmpipe".to_owned();
    assert_eq!(adapter_label(&info), "llvmpipe (Vulkan)");
    info.driver = "llvmpipe".to_owned();
    info.driver_info = "Mesa 25.2".to_owned();
    assert_eq!(adapter_label(&info), "llvmpipe (Vulkan, llvmpipe Mesa 25.2)");
}

#[test]
fn a_trace_started_after_the_device_hooks_names_the_adapter() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping: no adapter ({e})");
            return;
        }
    };
    let expected = adapter_label(&device.adapter.get_info());
    let dir = unique_temp_dir("trace_adapter");
    configure(TraceConfig {
        dir: dir.clone(),
        ..TraceConfig::default()
    });
    let mut app = App::new();
    app.add_plugins(TelemetryPlugin {
        options: TelemetryOptions {
            gpu_error_panics: false,
            summary: false,
            ..TelemetryOptions::default()
        },
    });
    app.insert_resource(device);
    app.update();
    recorder().start(Some("adapter"), None).expect("start");
    recorder().stop().expect("stop");
    let trace = read_trace(&dir.join("adapter.regiatrace")).expect("reads");
    assert!(!trace.header.adapter.is_empty(), "the header names an adapter");
    assert_eq!(trace.header.adapter, expected);
    let _ = std::fs::remove_dir_all(&dir);
}
