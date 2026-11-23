// SPDX-License-Identifier: MPL-2.0
//! Transient aliasing in the graph pool (ADR-2074), on the headless device.
//!
//! Two transients of one description with disjoint lifetimes run with aliasing off and on. Off
//! takes two textures, on takes one, and the two runs write the same bytes to the imported
//! outputs on every frame. A transient whose writer reads last frame's contents keeps its own
//! texture and those contents, and writes the same bytes with aliasing on and off.

use regia_test_utils::serial_gpu::{SerialGpu, serial_device};
use std::sync::{Arc, Mutex};

use regia_render::{
    GraphExecution, GraphInput, GraphOutput, GraphResource, RenderDevice, RenderGraph, RenderGraphNode,
    TextureDesc, TrackedDevice, TransientPool,
};

const SIZE: u32 = 8;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const DESC: TextureDesc = TextureDesc::new(SIZE, SIZE, FORMAT).with_usage(
    wgpu::TextureUsages::RENDER_ATTACHMENT
        .union(wgpu::TextureUsages::TEXTURE_BINDING)
        .union(wgpu::TextureUsages::COPY_SRC)
        .union(wgpu::TextureUsages::COPY_DST),
);

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
fn device() -> Option<SerialGpu> {
    match serial_device() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("no adapter: {err}");
            None
        }
    }
}

fn output(device: &RenderDevice, label: &str) -> wgpu::Texture {
    device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// Clears the view to `value` in the red channel.
fn clear(encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, value: f64) {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: None,
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: value,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                }),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    });
}

fn read_first_pixel(device: &RenderDevice, texture: &wgpu::Texture) -> [u8; 4] {
    let bytes_per_row = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(bytes_per_row * SIZE),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
    );
    device.queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped");
    [mapped[0], mapped[1], mapped[2], mapped[3]]
}

/// A node that clears its transient `write` to a value that depends on the frame.
fn writer(id: &str, reads: &[&str], write: &str, value: f64) -> RenderGraphNode {
    let name = write.to_string();
    RenderGraphNode {
        id: id.into(),
        inputs: reads
            .iter()
            .map(|name| GraphInput {
                name: (*name).to_string(),
            })
            .collect(),
        outputs: vec![GraphOutput {
            name: name.clone(),
            desc: GraphResource::Texture(DESC),
        }],
        slot: None,
        pipelines: vec![],
        record: Box::new(move |ctx| {
            let view = ctx.texture(&name).ok_or("no transient")?;
            let frame = ctx.frame_index as f64;
            clear(ctx.encoder, view, value + frame / 255.0);
            Ok(())
        }),
    }
}

/// The texture behind each transient of the run, by resource name.
type Bound = Arc<Mutex<Vec<(String, Option<wgpu::Texture>)>>>;

/// A node that copies the transient `source` into the imported `target` texture. The context
/// hands out views, so the copy goes through the view's texture.
fn copier(id: &str, source: &str, target_name: &str, target: wgpu::Texture, log: Bound) -> RenderGraphNode {
    let source_name = source.to_string();
    RenderGraphNode {
        id: id.into(),
        inputs: vec![GraphInput { name: source.into() }],
        outputs: vec![GraphOutput {
            name: target_name.into(),
            desc: GraphResource::Imported,
        }],
        slot: None,
        pipelines: vec![],
        record: Box::new(move |ctx| {
            let view = ctx.texture(&source_name).ok_or("no transient")?;
            let texture = view.texture();
            ctx.encoder.copy_texture_to_texture(
                texture.as_image_copy(),
                target.as_image_copy(),
                wgpu::Extent3d {
                    width: SIZE,
                    height: SIZE,
                    depth_or_array_layers: 1,
                },
            );
            log.lock()
                .map_err(|e| e.to_string())?
                .push((source_name.clone(), Some(texture.clone())));
            Ok(())
        }),
    }
}

struct Outcome {
    pixels: Vec<[[u8; 4]; 2]>,
    created: u64,
    aliased: u64,
    bound: Vec<(String, Option<wgpu::Texture>)>,
}

/// Three frames of: clear `x`, copy it out, clear `y`, copy it out.
fn disjoint_run(device: &RenderDevice, aliasing: bool) -> Outcome {
    let tracked = TrackedDevice::observe(&device.device);
    let (one, two) = (output(device, "one"), output(device, "two"));
    let (one_view, two_view) = (
        one.create_view(&wgpu::TextureViewDescriptor::default()),
        two.create_view(&wgpu::TextureViewDescriptor::default()),
    );
    let bound: Bound = Arc::new(Mutex::new(Vec::new()));
    let mut graph = RenderGraph::versioned();
    graph.add_node(writer("write_x", &[], "x", 0.25));
    graph.add_node(copier("copy_x", "x", "one", one.clone(), Arc::clone(&bound)));
    graph.add_node(writer("write_y", &["one"], "y", 0.75));
    graph.add_node(copier("copy_y", "y", "two", two.clone(), Arc::clone(&bound)));
    let plan = graph.alias_plan(&["two", "one"]).expect("plan");
    assert_eq!(plan.slots.len(), 1, "{plan:#?}");
    let mut pool = TransientPool::default();
    pool.set_aliasing(aliasing);
    let mut pixels = Vec::new();
    for frame in 1..=3u64 {
        pool.begin_frame(frame);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let run = graph.execute(
            GraphExecution {
                device: &tracked,
                encoder: &mut encoder,
                pool: &mut pool,
                imports: &[("one", &one_view), ("two", &two_view)],
                frame_index: frame,
            },
            &["two", "one"],
        );
        assert!(run.failed.is_empty(), "{:?}", run.failed);
        assert_eq!(run.recorded.len(), 4);
        assert_eq!(run.transients, 2);
        device.queue.submit([encoder.finish()]);
        pixels.push([read_first_pixel(device, &one), read_first_pixel(device, &two)]);
    }
    let stats = pool.stats();
    let bound = bound.lock().expect("log").clone();
    Outcome {
        pixels,
        created: stats.created,
        aliased: stats.aliased,
        bound,
    }
}

#[test]
fn disjoint_transients_share_a_texture_and_write_the_same_bytes() {
    let Some(device) = device() else { return };
    let separate = disjoint_run(&device, false);
    let aliased = disjoint_run(&device, true);
    assert_eq!(separate.created, 2);
    assert_eq!(separate.aliased, 0);
    assert_eq!(aliased.created, 1, "the plan's one slot");
    assert_eq!(
        aliased.aliased, 3,
        "`y` reuses `x`'s texture on each of three frames"
    );
    assert_eq!(separate.pixels, aliased.pixels, "aliasing changed the outputs");
    // The transients held different values: the second write did not read the first.
    assert_ne!(aliased.pixels[0][0], aliased.pixels[0][1]);
    // The frame number reached the image, so these are this frame's writes.
    assert_ne!(aliased.pixels[0][0], aliased.pixels[1][0]);
    let (x, y) = (&aliased.bound[0], &aliased.bound[1]);
    assert_eq!((x.0.as_str(), y.0.as_str()), ("x", "y"));
    assert_eq!(x.1, y.1, "with aliasing on, both are one texture");
    let (x, y) = (&separate.bound[0], &separate.bound[1]);
    assert_ne!(x.1, y.1, "with aliasing off, two textures");
}

/// A released name no longer resolves, so a node that reads a transient without declaring it
/// fails visibly instead of reading whatever the texture holds now.
#[test]
fn a_released_transient_no_longer_resolves() {
    let Some(device) = device() else { return };
    let tracked = TrackedDevice::observe(&device.device);
    let target = output(&device, "target");
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let mut graph = RenderGraph::versioned();
    graph.add_node(writer("write_x", &[], "x", 0.5));
    graph.add_node(copier(
        "copy_x",
        "x",
        "target",
        target.clone(),
        Arc::new(Mutex::new(Vec::new())),
    ));
    graph.add_node(RenderGraphNode {
        id: "undeclared".into(),
        inputs: vec![GraphInput {
            name: "target".into(),
        }],
        outputs: vec![GraphOutput {
            name: "output".into(),
            desc: GraphResource::Imported,
        }],
        slot: None,
        pipelines: vec![],
        record: Box::new(move |ctx| {
            log.lock()
                .map_err(|e| e.to_string())?
                .push(ctx.texture("x").is_some());
            Ok(())
        }),
    });
    for (aliasing, expected) in [(false, true), (true, false)] {
        let mut pool = TransientPool::default();
        pool.set_aliasing(aliasing);
        pool.begin_frame(1);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let imports = [("target", &view), ("output", &view)];
        let run = graph.execute(
            GraphExecution {
                device: &tracked,
                encoder: &mut encoder,
                pool: &mut pool,
                imports: &imports,
                frame_index: 1,
            },
            &["output"],
        );
        assert!(run.failed.is_empty(), "{:?}", run.failed);
        device.queue.submit([encoder.finish()]);
        assert_eq!(
            seen.lock().expect("log").pop(),
            Some(expected),
            "aliasing {aliasing}"
        );
    }
}

/// `history`'s writer reads what the last frame left (it declares its own resource as an input), so
/// it keeps its texture; the scratch before it has the same description and may not take it. The
/// frames are the same with aliasing on and off.
#[test]
fn a_writer_that_reads_last_frames_contents_keeps_them() {
    let Some(device) = device() else { return };
    let tracked = TrackedDevice::observe(&device.device);
    let run = |aliasing: bool| {
        let shown = output(&device, "shown");
        let mut graph = RenderGraph::versioned();
        graph.add_node(writer("scratch", &[], "scratch", 0.1));
        graph.add_node(copier(
            "use_scratch",
            "scratch",
            "used",
            output(&device, "used"),
            Arc::default(),
        ));
        let target = shown.clone();
        graph.add_node(RenderGraphNode {
            id: "accumulate".into(),
            inputs: vec![
                GraphInput {
                    name: "history".into(),
                },
                GraphInput { name: "used".into() },
            ],
            outputs: vec![GraphOutput {
                name: "history".into(),
                desc: GraphResource::Texture(DESC),
            }],
            slot: None,
            pipelines: vec![],
            record: Box::new(move |ctx| {
                let view = ctx.texture("history").ok_or("no history")?;
                ctx.encoder.copy_texture_to_texture(
                    view.texture().as_image_copy(),
                    target.as_image_copy(),
                    wgpu::Extent3d {
                        width: SIZE,
                        height: SIZE,
                        depth_or_array_layers: 1,
                    },
                );
                clear(ctx.encoder, view, 0.6 + ctx.frame_index as f64 / 255.0);
                Ok(())
            }),
        });
        let used = output(&device, "used import");
        let used_view = used.create_view(&wgpu::TextureViewDescriptor::default());
        let mut pool = TransientPool::default();
        pool.set_aliasing(aliasing);
        let mut pixels = Vec::new();
        for frame in 1..=3u64 {
            pool.begin_frame(frame);
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            let imports = [("used", &used_view)];
            let run = graph.execute(
                GraphExecution {
                    device: &tracked,
                    encoder: &mut encoder,
                    pool: &mut pool,
                    imports: &imports,
                    frame_index: frame,
                },
                &["history"],
            );
            assert_eq!(run.recorded.len(), 3, "{:?}", run.failed);
            device.queue.submit([encoder.finish()]);
            pixels.push(read_first_pixel(&device, &shown));
        }
        (pixels, pool.stats().created)
    };
    let (off, created_off) = run(false);
    let (on, created_on) = run(true);
    assert_eq!(off, on, "aliasing changed what the writer read");
    assert_eq!(
        (created_off, created_on),
        (2, 2),
        "`history` keeps a texture of its own"
    );
    // Frame 3 shows frame 2's write. Frame 1 shows the zero-initialized texture.
    assert_eq!(on[0][0], 0);
    assert_eq!(on[2][0], ((0.6f64 + 2.0 / 255.0) * 255.0).round() as u8);
}

/// A node that reads a transient before any node wrote it finds no texture, with aliasing on as
/// with it off, so turning aliasing on does not make a node record that did not record before.
#[test]
fn a_read_before_any_writer_does_not_resolve_on_or_off() {
    let Some(device) = device() else { return };
    let tracked = TrackedDevice::observe(&device.device);
    for aliasing in [false, true] {
        let shown = output(&device, "shown");
        let view = shown.create_view(&wgpu::TextureViewDescriptor::default());
        let mut graph = RenderGraph::versioned();
        graph.add_node(copier(
            "show_history",
            "history",
            "shown",
            shown.clone(),
            Arc::default(),
        ));
        graph.add_node(writer("save", &["shown"], "history", 0.5));
        let mut pool = TransientPool::default();
        pool.set_aliasing(aliasing);
        pool.begin_frame(1);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let imports = [("shown", &view)];
        let run = graph.execute(
            GraphExecution {
                device: &tracked,
                encoder: &mut encoder,
                pool: &mut pool,
                imports: &imports,
                frame_index: 1,
            },
            &["history"],
        );
        assert_eq!(run.failed.len(), 1, "aliasing {aliasing}: {:?}", run.recorded);
        assert_eq!(run.failed[0].0, "show_history");
        device.queue.submit([encoder.finish()]);
        // Frame 2 still has a reader of `history` before its writer. Keeping the texture does
        // not hand last frame to that earlier reader.
        pool.begin_frame(2);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let run = graph.execute(
            GraphExecution {
                device: &tracked,
                encoder: &mut encoder,
                pool: &mut pool,
                imports: &imports,
                frame_index: 2,
            },
            &["history"],
        );
        assert!(
            run.failed.iter().any(|(id, _)| id == "show_history"),
            "aliasing {aliasing}: frame 2 still misses history before its writer: {run:?}"
        );
        device.queue.submit([encoder.finish()]);
    }
}

/// A failed writer's reader finds no texture with aliasing on, instead of the pixels of the resource
/// that used the texture before.
#[test]
fn the_reader_of_a_failed_writer_finds_no_texture() {
    let Some(device) = device() else { return };
    let tracked = TrackedDevice::observe(&device.device);
    let one = output(&device, "one");
    let view = one.create_view(&wgpu::TextureViewDescriptor::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let mut graph = RenderGraph::versioned();
    graph.add_node(writer("write_x", &[], "x", 0.25));
    graph.add_node(copier("copy_x", "x", "one", one.clone(), Arc::default()));
    let mut failing = writer("write_y", &["one"], "y", 0.75);
    failing.record = Box::new(|_| Err("refused on purpose".into()));
    graph.add_node(failing);
    graph.add_node(RenderGraphNode {
        id: "read_y".into(),
        inputs: vec![GraphInput { name: "y".into() }],
        outputs: vec![GraphOutput {
            name: "output".into(),
            desc: GraphResource::Imported,
        }],
        slot: None,
        pipelines: vec![],
        record: Box::new(move |ctx| {
            log.lock()
                .map_err(|e| e.to_string())?
                .push(ctx.texture("y").is_some());
            Ok(())
        }),
    });
    for (aliasing, expected) in [(false, true), (true, false)] {
        let mut pool = TransientPool::default();
        pool.set_aliasing(aliasing);
        pool.begin_frame(1);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let imports = [("one", &view), ("output", &view)];
        let run = graph.execute(
            GraphExecution {
                device: &tracked,
                encoder: &mut encoder,
                pool: &mut pool,
                imports: &imports,
                frame_index: 1,
            },
            &["output"],
        );
        assert_eq!(run.failed.len(), 1);
        device.queue.submit([encoder.finish()]);
        assert_eq!(
            seen.lock().expect("log").pop(),
            Some(expected),
            "aliasing {aliasing}"
        );
    }
}

/// A name the caller imports stays the caller's texture: the graph takes nothing from the pool for
/// it and never releases it, and a failing node's transient is still released for the next one.
#[test]
fn imports_are_not_pooled_and_a_failed_node_still_releases() {
    let Some(device) = device() else { return };
    let tracked = TrackedDevice::observe(&device.device);
    let (one, two) = (output(&device, "one"), output(&device, "two"));
    let imported_x = output(&device, "imported x");
    let one_view = one.create_view(&wgpu::TextureViewDescriptor::default());
    let two_view = two.create_view(&wgpu::TextureViewDescriptor::default());
    let x_view = imported_x.create_view(&wgpu::TextureViewDescriptor::default());
    let mut graph = RenderGraph::versioned();
    // `x` is declared a transient, but the caller imports the name.
    graph.add_node(writer("write_imported", &[], "x", 0.25));
    graph.add_node(copier(
        "copy_imported",
        "x",
        "one",
        one.clone(),
        Arc::new(Mutex::new(Vec::new())),
    ));
    // `y` fails to copy out: the texture still goes back for `z`.
    graph.add_node(writer("write_y", &["one"], "y", 0.5));
    graph.add_node(RenderGraphNode {
        id: "broken".into(),
        inputs: vec![GraphInput { name: "y".into() }],
        outputs: vec![GraphOutput {
            name: "after_broken".into(),
            desc: GraphResource::Imported,
        }],
        slot: None,
        pipelines: vec![],
        record: Box::new(|_| Err("refused on purpose".into())),
    });
    graph.add_node(writer("write_z", &["after_broken"], "z", 0.75));
    graph.add_node(copier(
        "copy_z",
        "z",
        "two",
        two.clone(),
        Arc::new(Mutex::new(Vec::new())),
    ));
    let mut pool = TransientPool::default();
    pool.set_aliasing(true);
    pool.begin_frame(1);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    let imports = [
        ("x", &x_view),
        ("one", &one_view),
        ("two", &two_view),
        ("after_broken", &two_view),
    ];
    let run = graph.execute(
        GraphExecution {
            device: &tracked,
            encoder: &mut encoder,
            pool: &mut pool,
            imports: &imports,
            frame_index: 1,
        },
        &["two"],
    );
    assert_eq!(run.failed.len(), 1, "{:?}", run.failed);
    let stats = pool.stats();
    assert_eq!(
        stats.created, 1,
        "`y` and `z` share a texture; the import takes none"
    );
    assert_eq!(stats.aliased, 1);
    assert_eq!(run.transients, 2);
    device.queue.submit([encoder.finish()]);
    let cleared = read_first_pixel(&device, &imported_x);
    assert_eq!(
        cleared[0],
        ((0.25f64 + 1.0 / 255.0) * 255.0).round() as u8,
        "the node wrote the import"
    );
}
