// SPDX-License-Identifier: MPL-2.0
//! Headless capture of the whole editor window: the egui shell (docked panels, menus, the
//! viewport's rendered 3D frame and its overlays) drawn by `egui-wgpu` into an offscreen
//! texture and read back (`docs/specs/23-agent-native.md`: every feature is visible to an
//! agent; `docs/specs/19-editor.md` § 2).
//!
//! It runs the same `EditorShell::ui` the window runs, for `frames` frames (egui settles its
//! layout over a couple of frames), then reads the last one back.

use regia_ecs::prelude::World;
use regia_render::RenderDevice;

use crate::shell::EditorShell;

/// Renders `frames` editor frames at `size` physical pixels (`pixels_per_point` scale) into an
/// offscreen target and returns the last one as RGBA.
///
/// # Errors
///
/// The GPU readback failed.
#[tracing::instrument(target = "regia_editor", skip_all, name = "editor_window_capture")]
pub fn capture_editor_window(
    shell: &mut EditorShell,
    ctx: &egui::Context,
    world: &mut World,
    device: &RenderDevice,
    size: [u32; 2],
    pixels_per_point: f32,
    frames: u32,
) -> Result<egui::ColorImage, String> {
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let (width, height) = (size[0].max(64), size[1].max(64));
    let gpu = &device.device;
    let mut renderer = egui_wgpu::Renderer::new(gpu, format, Default::default());
    let texture = gpu.create_texture(&wgpu::TextureDescriptor {
        label: Some("editor window capture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let padded =
        (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = gpu.create_buffer(&wgpu::BufferDescriptor {
        label: Some("editor window readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let ppp = pixels_per_point.max(0.5);
    for frame in 0..frames.max(1) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width as f32 / ppp, height as f32 / ppp),
            )),
            time: Some(f64::from(frame) / 60.0),
            predicted_dt: 1.0 / 60.0,
            ..Default::default()
        };
        ctx.set_pixels_per_point(ppp);
        let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, world, None));
        shell.frame_effects(ctx, world);
        let jobs = ctx.tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                renderer.update_texture(gpu, &device.queue, *id, delta);
            }
        }
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [width, height],
            pixels_per_point: output.pixels_per_point,
        };
        let mut encoder = gpu.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("editor window capture"),
        });
        renderer.update_buffers(gpu, &device.queue, &mut encoder, &jobs, &screen);
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor window capture"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.02,
                                g: 0.02,
                                b: 0.025,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            renderer.render(&mut pass, &jobs, &screen);
        }
        if frame + 1 == frames.max(1) {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(height),
                    },
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
        device.queue.submit([encoder.finish()]);
        for id in &output.textures_delta.free {
            renderer.free_texture(id);
        }
        output.textures_delta.clear();
    }
    let pixels: Vec<u8> = crate::picking::read_words(gpu, &readback, width, height, padded)?
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &pixels,
    ))
}

/// `image` as a window read-back: tightly packed sRGB RGBA8 rows, top row first.
#[must_use]
pub fn captured_image(image: &egui::ColorImage) -> regia_render::CapturedImage {
    let rgba = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_srgba_unmultiplied())
        .collect();
    regia_render::CapturedImage {
        width: image.size[0] as u32,
        height: image.size[1] as u32,
        rgba,
    }
}

/// Answers the pending window screenshot requests ([`regia_render::ScreenshotRequests`], what
/// `seeing.screenshot` and the color eyedropper ask for) with one [`capture_editor_window`]
/// frame: for hosts without a present loop (tests, tools). Returns the image served, or `None`
/// when nothing was asked.
///
/// # Errors
///
/// The capture failed (each request's callback gets the same error).
pub fn serve_screenshot_requests(
    shell: &mut EditorShell,
    ctx: &egui::Context,
    world: &mut World,
    device: &RenderDevice,
    size: [u32; 2],
    pixels_per_point: f32,
    frames: u32,
) -> Result<Option<regia_render::CapturedImage>, String> {
    let window = {
        let mut windows = world.query_filtered::<regia_ecs::prelude::Entity, regia_ecs::prelude::With<regia_platform::PrimaryWindow>>();
        windows
            .iter(world)
            .next()
            .unwrap_or(regia_ecs::prelude::Entity::PLACEHOLDER)
    };
    let callbacks = match world.get_resource_mut::<regia_render::ScreenshotRequests>() {
        Some(mut requests) => requests.take_for(window),
        None => return Ok(None),
    };
    if callbacks.is_empty() {
        return Ok(None);
    }
    let captured = capture_editor_window(shell, ctx, world, device, size, pixels_per_point, frames)
        .map(|image| captured_image(&image));
    for callback in callbacks {
        callback(captured.clone());
    }
    captured.map(Some)
}

#[cfg(test)]
mod tests {
    use regia_app::prelude::*;
    use regia_render::PointLight;

    use super::*;
    use crate::inspector::color_eyedropper;

    /// P-133 acceptance: armed on a light's color, a pick in the middle of the window (the
    /// viewport's rendered frame) asks the present for the window, and the next frame writes that
    /// pixel's color, decoded to linear, as one undo step. Needs a software adapter (lavapipe).
    #[test]
    fn the_eyedropper_picks_a_viewport_pixel() {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
        };
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.init_resource::<crate::command::UndoStack>()
            .init_resource::<crate::selection::Selection>()
            .init_resource::<crate::viewport::EditorCamera>()
            .init_resource::<crate::viewport::GizmoState>()
            .init_resource::<crate::viewport::ViewportSettings>();
        let open = crate::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        crate::agent_ext::install(app.world_mut());
        app.update();
        let lamp = app
            .world_mut()
            .spawn((
                regia_scene::prelude::Name::new("Lamp"),
                regia_scene::prelude::Transform::default(),
                PointLight::default(),
            ))
            .id();
        app.update();
        let mut shell = EditorShell::new(Some(device.clone()), None);
        let ctx = egui::Context::default();
        let size = [1280u32, 800];
        let world = app.world_mut();
        let undo = world.resource::<crate::command::UndoStack>().undo_depth();

        let pixel = [size[0] / 2, size[1] / 2];
        color_eyedropper::arm(world, vec![lamp], "PointLight", "color");
        color_eyedropper::request_pick(world, pixel);
        assert!(
            world
                .resource::<color_eyedropper::ColorEyedropper>()
                .pending
                .is_some()
        );
        let served = serve_screenshot_requests(&mut shell, &ctx, world, &device, size, 1.0, 3)
            .expect("capture")
            .expect("the pick asked for the window");
        let picked = color_eyedropper::resolve(world).expect("the pick resolved");

        let bytes = color_eyedropper::pixel_of(&served, pixel).expect("in the window");
        let clear = [5u8, 5, 6];
        assert!(
            bytes[..3].iter().zip(clear).any(|(b, c)| b.abs_diff(c) > 2),
            "the middle of the window is the viewport's frame, not the clear color: {bytes:?}"
        );
        let expected = color_eyedropper::linear_of(bytes);
        let color = world.get::<PointLight>(lamp).expect("lamp").color;
        assert_eq!(color, expected, "the pixel's color, decoded to linear");
        assert_eq!(picked[..3], expected[..]);
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            undo + 1,
            "one undo step"
        );
        assert!(
            world
                .get_resource::<color_eyedropper::ColorEyedropper>()
                .is_none(),
            "disarmed"
        );
        assert!(crate::command::undo_world(world));
        assert_eq!(
            world.get::<PointLight>(lamp).expect("lamp").color,
            PointLight::default().color
        );
    }
}
