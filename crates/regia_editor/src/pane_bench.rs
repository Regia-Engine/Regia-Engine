// SPDX-License-Identifier: MPL-2.0
//! Measuring the live pane path (`docs/06` § 9, ADR-2046).
//!
//! [`measure`] renders one pane of a small proxy-cube world for a fixed number of frames on
//! either [`PanePath`] and reports the per-frame wall time. The GPU path is what the windowed
//! editor does each frame: [`render_pane_gpu`], [`PaneTextures::submit`] and
//! [`PaneTextures::apply`] (a rebind on `egui_wgpu`). The CPU path is [`render_pane`] (render plus
//! readback) and the upload of the bytes into a texture the way egui's texture update does.
//! Both end by waiting for the queue, so the numbers are frame cost and not submit cost.
//!
//! The numbers depend on the adapter: on lavapipe the "GPU" is the CPU, so the readback costs a
//! memory copy and the two paths come out close; a discrete card shows the PCIe readback the GPU
//! path removes. `cargo run -p regia_editor --example pane_bench --release` prints the table.
//! Nothing here asserts on time.

use std::time::Instant;

use regia_core::math::Vec3;
use regia_ecs::prelude::World;
use regia_render::RenderDevice;
use regia_scene::prelude::Transform;

use crate::pane_texture::{EguiNative, NativeTextures, PaneKey, PanePath, PaneTextures};
use crate::render::{PaneRequest, render_pane, render_pane_gpu};
use crate::viewport::EditorCamera;
use crate::views::ViewKind;

/// What to measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchSpec {
    /// Pane size, pixels.
    pub size: [u32; 2],
    /// Frames rendered before timing starts (pipeline creation, renderer warm-up).
    pub warmup: usize,
    /// Timed frames.
    pub frames: usize,
}

impl Default for BenchSpec {
    fn default() -> Self {
        Self {
            size: [1280, 720],
            warmup: 10,
            frames: 120,
        }
    }
}

/// The result of one [`measure`] run.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchReport {
    /// The path measured.
    pub path: PanePath,
    /// Pane size, pixels.
    pub size: [u32; 2],
    /// Timed frames that produced a picture.
    pub frames: usize,
    /// Frame times, milliseconds, in frame order.
    pub frame_ms: Vec<f64>,
}

impl BenchReport {
    fn sorted(&self) -> Vec<f64> {
        let mut sorted = self.frame_ms.clone();
        sorted.sort_by(f64::total_cmp);
        sorted
    }

    /// Median frame time, milliseconds (0 when no frame was timed).
    #[must_use]
    pub fn median_ms(&self) -> f64 {
        let sorted = self.sorted();
        sorted.get(sorted.len() / 2).copied().unwrap_or(0.0)
    }

    /// The 95th-percentile frame time, milliseconds (0 when no frame was timed).
    #[must_use]
    pub fn p95_ms(&self) -> f64 {
        let sorted = self.sorted();
        sorted
            .get((sorted.len() * 95 / 100).min(sorted.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0)
    }

    /// Pictures per second at the median frame time (0 when no frame was timed).
    #[must_use]
    pub fn median_fps(&self) -> f64 {
        let ms = self.median_ms();
        if ms > 0.0 { 1000.0 / ms } else { 0.0 }
    }

    /// Whether the median holds `fps` (the 60 fps question is `holds(60.0)`).
    #[must_use]
    pub fn holds(&self, fps: f64) -> bool {
        self.median_fps() >= fps
    }
}

/// Two proxy cubes either side of the origin: the same scene the pane tests draw.
fn world() -> World {
    let mut world = World::new();
    for x in [-1.5, 1.5] {
        world.spawn(Transform {
            translation: Vec3::new(x, 0.5, 0.0),
            ..Default::default()
        });
    }
    world
}

fn request(spec: &BenchSpec) -> PaneRequest {
    PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: EditorCamera {
            target: Vec3::new(0.0, 0.5, 0.0),
            distance: 7.0,
            yaw: 0.6,
            pitch: 0.3,
            ..EditorCamera::default()
        },
        width: spec.size[0],
        height: spec.size[1],
        ids: false,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    }
}

/// Waits until the queue is idle.
fn wait_idle(device: &RenderDevice) {
    // A failed poll means the device is lost; the next frame fails to render and is not counted.
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
}

/// Renders `spec.warmup + spec.frames` frames of the pane on `path` and times the last
/// `spec.frames`. A frame whose render failed is left out of the report.
#[tracing::instrument(target = "regia_editor", skip_all, name = "pane_bench")]
pub fn measure(device: &RenderDevice, path: PanePath, spec: &BenchSpec) -> BenchReport {
    let mut world = world();
    let request = request(spec);
    let textures = PaneTextures::new(path);
    let mut egui = egui_wgpu::Renderer::new(
        &device.device,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        Default::default(),
    );
    // The texture egui's own update would write the CPU path's bytes into.
    let upload = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pane bench upload"),
        size: wgpu::Extent3d {
            width: spec.size[0],
            height: spec.size[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let mut frame_ms = Vec::with_capacity(spec.frames);
    for i in 0..spec.warmup + spec.frames {
        let started = Instant::now();
        let produced = match path {
            PanePath::Gpu => match render_pane_gpu(device, &mut world, &request) {
                Some(frame) => {
                    textures.submit(
                        PaneKey {
                            instance: None,
                            pane: 0,
                        },
                        &frame.target,
                    );
                    textures.apply(&mut EguiNative {
                        renderer: &mut egui,
                        device: &device.device,
                    } as &mut dyn NativeTextures);
                    true
                }
                None => false,
            },
            PanePath::Cpu => match render_pane(device, &mut world, &request) {
                Some(frame) => {
                    let bytes: Vec<u8> = frame.image.pixels.iter().flat_map(|c| c.to_array()).collect();
                    device.queue.write_texture(
                        upload.as_image_copy(),
                        &bytes,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(spec.size[0] * 4),
                            rows_per_image: Some(spec.size[1]),
                        },
                        upload.size(),
                    );
                    true
                }
                None => false,
            },
        };
        wait_idle(device);
        if produced && i >= spec.warmup {
            frame_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    BenchReport {
        path,
        size: spec.size,
        frames: frame_ms.len(),
        frame_ms,
    }
}

/// A markdown table of reports (one row each), as `docs/progress.md` and the lane reports use.
#[must_use]
pub fn markdown(adapter: &str, reports: &[BenchReport]) -> String {
    let mut out = format!(
        "Adapter: {adapter}\n\n| path | size | frames | median ms | p95 ms | median fps | holds 60 |\n|---|---|---|---|---|---|---|\n"
    );
    for r in reports {
        out.push_str(&format!(
            "| {} | {}x{} | {} | {:.2} | {:.2} | {:.1} | {} |\n",
            r.path.name(),
            r.size[0],
            r.size[1],
            r.frames,
            r.median_ms(),
            r.p95_ms(),
            r.median_fps(),
            if r.holds(60.0) { "yes" } else { "no" },
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(ms: &[f64]) -> BenchReport {
        BenchReport {
            path: PanePath::Gpu,
            size: [8, 8],
            frames: ms.len(),
            frame_ms: ms.to_vec(),
        }
    }

    #[test]
    fn statistics_use_the_sorted_frame_times() {
        let r = report(&[30.0, 10.0, 20.0, 40.0, 50.0]);
        assert_eq!(r.median_ms(), 30.0);
        assert_eq!(r.p95_ms(), 50.0);
        assert!((r.median_fps() - 33.333).abs() < 0.01);
        assert!(!r.holds(60.0));
        assert!(report(&[10.0, 12.0, 16.0]).holds(60.0));
    }

    #[test]
    fn an_empty_report_reads_as_zero_and_never_holds() {
        let r = report(&[]);
        assert_eq!((r.median_ms(), r.p95_ms(), r.median_fps()), (0.0, 0.0, 0.0));
        assert!(!r.holds(1.0));
    }

    #[test]
    fn the_table_has_one_row_per_report() {
        let table = markdown("test", &[report(&[10.0]), report(&[20.0])]);
        assert_eq!(table.lines().filter(|l| l.starts_with("| gpu")).count(), 2);
        assert!(table.contains("holds 60"));
    }
}
