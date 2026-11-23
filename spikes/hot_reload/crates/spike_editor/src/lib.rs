//! Stand-in for `regia_editor` (desktop only): the egui stack the editor links.

/// Runs one headless egui frame with the editor's widgets; returns the number of shapes.
pub fn egui_frame() -> usize {
    let ctx = egui::Context::default();
    let mut dock = egui_dock::DockState::new(vec!["Scene".to_string(), "Inspector".to_string()]);
    let snarl: egui_snarl::Snarl<u32> = egui_snarl::Snarl::new();
    let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
        ui.label("regia editor host");
        let _ = &mut dock;
        let _ = snarl.nodes().count();
    });
    out.textures_delta.clear();
    out.shapes.len()
}

/// Linked but never run: the parts that need a window, a GPU or a desktop session.
pub fn never_called() {
    let _ = rfd::FileDialog::new().set_title("open level").pick_file();
    let _ = transform_gizmo_egui::Gizmo::default();
    let _ = egui_wgpu::WgpuConfiguration::default();
    let _ = egui_winit::State::new;
}
