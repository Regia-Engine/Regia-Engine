// SPDX-License-Identifier: MPL-2.0
//! Fullscreen video above the UI (specs/50 § 2.7, ADR-2410): `layer >= 0` players go to the
//! `WindowOverlay` layer drawn after the UI, `layer < 0` ones stay in the frame-graph pass under it,
//! and the layer paints the picture and its letterbox bars over whatever the UI drew.

use regia_app::prelude::*;
use regia_media::frame_source::FullscreenVideos;
use regia_media::mux::PatternClip;
use regia_media::overlay::{OverlayVideos, VIDEO_OVERLAY_NAME, VideoOverlayDraw, VideoOverlayPlugin};
use regia_media::plugin::MediaFiles;
use regia_media::{ClockMode, MediaPlugin, VideoFramePlugin, VideoFullscreen, VideoPlayer};
use regia_render::{
    FullscreenFit, FullscreenVideo, OverlayDraw, RenderDevice, VideoFullscreenQueue, WindowOverlay,
};

struct NoDraw;

impl OverlayDraw for NoDraw {
    fn draw(
        &mut self,
        _: &RenderDevice,
        _: &mut wgpu::CommandEncoder,
        _: &wgpu::TextureView,
        _: wgpu::TextureFormat,
        _: u32,
        _: u32,
    ) {
    }
}

#[test]
fn layers_at_or_above_zero_draw_after_the_ui() {
    let mut files = MediaFiles::default();
    files.insert("m.webm", PatternClip::new(8, 8, 30, 60).to_webm());
    let mut app = App::new();
    let mut overlay = WindowOverlay::default();
    overlay.set(Box::new(NoDraw));
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .insert_resource(overlay)
        .insert_resource(FullscreenVideos::default())
        .add_plugins(MediaPlugin)
        .add_plugins(VideoFramePlugin)
        .add_plugins(VideoOverlayPlugin);
    let lockstep = |layer: i32| {
        (
            VideoPlayer {
                clock: ClockMode::Lockstep,
                ..VideoPlayer::new("m.webm")
            },
            VideoFullscreen {
                layer,
                ..VideoFullscreen::default()
            },
        )
    };
    let top = app.world_mut().spawn(lockstep(0)).id();
    let under = app.world_mut().spawn(lockstep(-1)).id();
    app.update();
    app.update();
    let names = app.world().resource::<WindowOverlay>().layer_names();
    assert_eq!(
        names,
        vec!["ui", VIDEO_OVERLAY_NAME],
        "the video layer draws after the UI"
    );
    let above: Vec<u64> = app
        .world()
        .resource::<OverlayVideos>()
        .0
        .videos()
        .iter()
        .map(|v| v.player)
        .collect();
    let below: Vec<u64> = app
        .world()
        .resource::<FullscreenVideos>()
        .0
        .videos()
        .iter()
        .map(|v| v.player)
        .collect();
    assert_eq!(above, vec![top.to_bits()]);
    assert_eq!(below, vec![under.to_bits()]);
}

/// The layer draws a letterboxed picture with black bars over the UI's pixels (lavapipe is enough).
#[test]
fn overlay_covers_the_ui_with_picture_and_bars() {
    let device = match RenderDevice::shared_headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let (w, h) = (64u32, 32u32);
    let target = regia_ui::OffscreenTarget::new(&device, w, h);
    // What the UI drew: green everywhere.
    let ui: Vec<u8> = (0..w * h).flat_map(|_| [0u8, 255, 0, 255]).collect();
    assert!(target.upload(&device, &ui));
    let queue = VideoFullscreenQueue::default();
    let red: Vec<u8> = (0..16 * 16).flat_map(|_| [255u8, 0, 0, 255]).collect();
    queue.set(vec![FullscreenVideo {
        player: 7,
        layer: 0,
        fit: FullscreenFit::Letterbox,
        aspect: 1.0,
        serial: 1,
        width: 16,
        height: 16,
        rgba: red.into(),
    }]);
    let mut draw = VideoOverlayDraw::new(queue);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("overlay test"),
        });
    draw.draw(
        &device,
        &mut encoder,
        target.view(),
        regia_ui::OffscreenTarget::FORMAT,
        w,
        h,
    );
    device.queue.submit([encoder.finish()]);
    let px = target.read(&device).expect("readback");
    let at = |x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2]]
    };
    assert_eq!(at(32, 16), [255, 0, 0], "picture in the middle");
    assert_eq!(at(4, 16), [0, 0, 0], "letterbox bar, not the UI");
    assert_eq!(at(60, 2), [0, 0, 0], "letterbox bar, not the UI");
    assert_eq!((draw.uploads(), draw.drawn()), (1, 1));
}

/// A second draw of the same picture uploads nothing and creates no bind group.
/// A letterboxed clip keeps two groups (bar and picture), not two new groups per frame.
#[test]
fn overlay_reuses_bind_groups_for_the_same_picture() {
    let device = match RenderDevice::shared_headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let (w, h) = (64u32, 32u32);
    let target = regia_ui::OffscreenTarget::new(&device, w, h);
    let queue = VideoFullscreenQueue::default();
    let red: Vec<u8> = (0..16 * 16).flat_map(|_| [255u8, 0, 0, 255]).collect();
    queue.set(vec![FullscreenVideo {
        player: 7,
        layer: 0,
        fit: FullscreenFit::Letterbox,
        aspect: 1.0,
        serial: 1,
        width: 16,
        height: 16,
        rgba: red.into(),
    }]);
    let mut draw = VideoOverlayDraw::new(queue);
    let base = regia_render::frame_path_counters().bind_groups;
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("overlay bind-group test"),
        });
    draw.draw(
        &device,
        &mut encoder,
        target.view(),
        regia_ui::OffscreenTarget::FORMAT,
        w,
        h,
    );
    device.queue.submit([encoder.finish()]);
    let after_first = regia_render::frame_path_counters().bind_groups;
    assert_eq!(draw.uploads(), 1);
    assert_eq!(
        after_first - base,
        2,
        "a letterboxed clip creates two bind groups, not a fresh pair every frame"
    );
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("overlay bind-group test 2"),
        });
    draw.draw(
        &device,
        &mut encoder,
        target.view(),
        regia_ui::OffscreenTarget::FORMAT,
        w,
        h,
    );
    device.queue.submit([encoder.finish()]);
    assert_eq!(draw.uploads(), 1, "the same serial is not uploaded again");
    assert_eq!(
        regia_render::frame_path_counters().bind_groups,
        after_first,
        "the second draw creates no bind group"
    );
}
