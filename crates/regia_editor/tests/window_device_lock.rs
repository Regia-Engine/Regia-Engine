// SPDX-License-Identifier: MPL-2.0
//! The editor's window instance and device go through `regia_render`'s loader-list lock
//! (ADR-2113, ADR-2306). The editor opens engine devices of its own on other threads (previews,
//! bakes, thumbnails); a bare `wgpu::Instance::new` or `adapter.request_device` in the window path
//! races the Vulkan loader's lists and can die with SIGSEGV. The window path needs a display, so
//! this guards the source: the only instance and device creations in the editor binary are the
//! locked wrappers. Deterministic: text only.

/// The editor binary's source, with comments removed so a comment naming the raw calls does not
/// count.
fn editor_main_code() -> String {
    include_str!("../src/main.rs")
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_editor_window_creates_its_instance_and_device_under_the_loader_lock() {
    let code = editor_main_code();
    assert!(
        !code.contains("Instance::new("),
        "src/main.rs creates a wgpu instance outside regia_render::create_window_instance"
    );
    assert!(
        !code.contains(".request_device("),
        "src/main.rs requests a device outside regia_render::request_device_locked"
    );
    assert!(code.contains("regia_render::create_window_instance("));
    assert!(code.contains("regia_render::request_device_locked("));
}
