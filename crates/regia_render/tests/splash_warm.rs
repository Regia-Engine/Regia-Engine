// SPDX-License-Identifier: MPL-2.0
//! The boot splash's pipeline goes through the warm queue (ADR-5131): with the app's queue
//! attached, the first present compiles nothing on the presenting thread (it clears to the
//! splash background) and the boot tier builds the pipeline as its first job; once it is done
//! the splash draws its text and still compiles nothing on that thread. Without a queue the
//! splash builds its pipeline where it records, as before. Its own test binary: the pipeline
//! counters are per process, so the tests take turns.

use std::sync::Mutex;
use std::time::Duration;

use regia_render::pipeline_cache::pipelines_created;
use regia_render::{BootSplash, PipelineWarmQueue, RenderDevice, SplashPhase, WarmTier, read_rgba8};

/// One test at a time (the pipeline counters are process-wide).
static SERIAL: Mutex<()> = Mutex::new(());

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const SIZE: (u32, u32) = (96, 64);

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    }
}

fn target(device: &RenderDevice) -> wgpu::Texture {
    device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("splash test target"),
        size: wgpu::Extent3d {
            width: SIZE.0,
            height: SIZE.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Records `splash` fully shown into a fresh target and reads it back.
fn present(device: &RenderDevice, splash: &mut BootSplash) -> Vec<u8> {
    let texture = target(device);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("splash test"),
        });
    splash.record(
        device,
        &mut encoder,
        &view,
        FORMAT,
        SIZE,
        SplashPhase::Showing { opacity: 1.0 },
    );
    device.queue.submit([encoder.finish()]);
    read_rgba8(&device.device, &device.queue, &texture, SIZE.0, SIZE.1).expect("readback")
}

/// A splash on a red background with white text across the middle.
fn splash() -> BootSplash {
    let mut splash = BootSplash::new().with_text("REGIA");
    splash.background = [1.0, 0.0, 0.0, 1.0];
    splash.text_color = [1.0, 1.0, 1.0, 1.0];
    splash.text_scale = 0.12;
    splash
}

/// Pixels that are not the red background.
fn not_background(rgba: &[u8]) -> usize {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .filter(|p| !(p[0] > 200 && p[1] < 40 && p[2] < 40))
        .count()
}

#[test]
fn with_a_warm_queue_the_splash_pipeline_is_a_boot_job_and_the_first_present_compiles_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let queue = PipelineWarmQueue::new(1);
    let mut splash = splash();
    splash.set_warm_queue(Some(queue.clone()));

    // Before the queue starts (the renderer presents the splash, then starts the queue).
    let before = pipelines_created();
    let outside = queue.progress().pipelines_outside;
    let first = present(&device, &mut splash);
    let second = present(&device, &mut splash);
    assert_eq!(
        pipelines_created(),
        before,
        "no pipeline on the presenting thread"
    );
    assert_eq!(queue.progress().pipelines_outside, outside);
    assert_eq!(
        queue.progress().tiers[WarmTier::Boot as usize].jobs_total,
        1,
        "one boot job for the format, however often the splash records"
    );
    assert_eq!(
        not_background(&first),
        0,
        "the first present is the background alone"
    );
    assert_eq!(not_background(&second), 0);

    queue.start(&device);
    assert!(
        queue.wait_tier(WarmTier::Boot, Duration::from_secs(300)),
        "boot tier"
    );
    let warm = queue.progress();
    assert!(warm.last_error.is_none(), "{:?}", warm.last_error);
    assert_eq!(
        warm.tiers[WarmTier::Boot as usize].pipelines,
        1,
        "the splash pipeline"
    );
    assert_eq!(warm.pipelines_outside, outside, "{:?}", warm.outside_sites);

    let before = pipelines_created();
    let shown = present(&device, &mut splash);
    assert_eq!(pipelines_created(), before, "the warmed pipeline draws");
    assert!(
        not_background(&shown) > 0,
        "the text is drawn once the job is done"
    );
    assert_eq!(queue.progress().tiers[WarmTier::Boot as usize].jobs_total, 1);
    queue.shutdown();
}

#[test]
fn without_a_queue_the_splash_builds_its_pipeline_where_it_records() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut splash = splash();
    let before = pipelines_created();
    let shown = present(&device, &mut splash);
    assert_eq!(pipelines_created(), before + 1);
    assert!(not_background(&shown) > 0, "drawn on the first present");
    let again = pipelines_created();
    present(&device, &mut splash);
    assert_eq!(pipelines_created(), again, "built once");
}
