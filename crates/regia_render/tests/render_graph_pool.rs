// SPDX-License-Identifier: MPL-2.0
//! The render graph's transient pool and execution (ADR-2018), on the headless device.
//!
//! - The pool hands the same textures to the same sequence of acquires on every frame, two
//!   acquires of one description in one frame get two textures, an idle texture is dropped, and
//!   a warm frame creates nothing and allocates nothing on the heap (counted here).
//! - `RenderGraph::execute` binds one pooled texture per transient name and the imports by
//!   name, records live nodes in order, skips a failing node, and a second run reuses the pool.
//!
//! Its own test binary: it installs a counting global allocator (per thread, so other tests in
//! this binary do not disturb a count).

use std::sync::{Arc, Mutex};

use regia_render::{
    GraphExecution, GraphInput, GraphOutput, GraphResource, RenderDevice, RenderGraph, RenderGraphNode,
    TextureDesc, TrackedDevice, TransientPool,
};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// This thread's allocation requests so far.
fn allocs() -> u64 {
    alloc_counter::thread_allocs()
}

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn hdr(width: u32, height: u32) -> TextureDesc {
    TextureDesc::new(width, height, wgpu::TextureFormat::Rgba16Float)
}

#[test]
fn the_same_acquires_get_the_same_textures_every_frame() {
    let device = device();
    let tracked = TrackedDevice::observe(&device.device);
    let mut pool = TransientPool::default();
    let ssao = hdr(32, 16)
        .with_layers(2)
        .with_usage(wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING);
    pool.begin_frame(1);
    let first = [
        pool.acquire(&tracked, &hdr(64, 32), "hdr").generation,
        pool.acquire(&tracked, &ssao, "ssao").generation,
        pool.acquire(&tracked, &ssao, "ssao raw").generation,
    ];
    assert_ne!(
        first[1], first[2],
        "two acquires of one desc in a frame get two textures"
    );
    assert_eq!(pool.stats().created, 3);
    assert_eq!(pool.stats().in_use, 3);
    assert_eq!(pool.stats().bytes, 64 * 32 * 8 + 2 * (32 * 16 * 8 * 2));
    for frame in 2..6 {
        pool.begin_frame(frame);
        assert_eq!(pool.stats().in_use, 0);
        let again = [
            pool.acquire(&tracked, &hdr(64, 32), "hdr").generation,
            pool.acquire(&tracked, &ssao, "ssao").generation,
            pool.acquire(&tracked, &ssao, "ssao raw").generation,
        ];
        assert_eq!(again, first, "frame {frame}");
    }
    let stats = pool.stats();
    assert_eq!((stats.created, stats.reused, stats.textures), (3, 12, 3));
    // Released mid-frame, a texture serves the next acquire of its desc.
    pool.begin_frame(6);
    let a = pool.acquire(&tracked, &hdr(64, 32), "hdr");
    assert!(pool.release(a.generation));
    assert_eq!(
        pool.acquire(&tracked, &hdr(64, 32), "hdr").generation,
        a.generation
    );
    assert!(!pool.release(0));
}

#[test]
fn an_idle_texture_is_dropped_and_its_replacement_has_a_new_generation() {
    let device = device();
    let tracked = TrackedDevice::observe(&device.device);
    let mut pool = TransientPool::new(2);
    pool.begin_frame(10);
    let old = pool.acquire(&tracked, &hdr(8, 8), "hdr").generation;
    pool.begin_frame(11);
    let kept = pool.acquire(&tracked, &hdr(16, 16), "hdr").generation;
    // Frames 13 and 14: the 8×8 texture was last used at 10, idle for more than two frames.
    assert!(!pool.begin_frame(12));
    pool.acquire(&tracked, &hdr(16, 16), "hdr");
    assert!(pool.begin_frame(13));
    assert_eq!(pool.stats().textures, 1);
    assert_eq!(pool.stats().evicted, 1);
    assert_eq!(pool.acquire(&tracked, &hdr(16, 16), "hdr").generation, kept);
    let new = pool.acquire(&tracked, &hdr(8, 8), "hdr").generation;
    assert!(new > old, "a replacement is a new texture");
    pool.clear();
    assert_eq!(pool.stats().textures, 0);
}

#[test]
fn a_warm_pool_frame_allocates_nothing() {
    let device = device();
    let tracked = TrackedDevice::observe(&device.device);
    let mut pool = TransientPool::default();
    let descs = [
        hdr(64, 32),
        hdr(64, 32).with_usage(wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST),
        TextureDesc::new(32, 16, wgpu::TextureFormat::Rgba8Unorm).with_layers(2),
    ];
    for frame in 0..2 {
        pool.begin_frame(frame);
        for desc in &descs {
            drop(pool.acquire(&tracked, desc, "warm"));
        }
    }
    let created = pool.stats().created;
    let before = allocs();
    for frame in 2..202 {
        pool.begin_frame(frame);
        for desc in &descs {
            drop(pool.acquire(&tracked, desc, "warm"));
        }
    }
    assert_eq!(allocs(), before, "a warm pool frame allocates nothing");
    assert_eq!(pool.stats().created, created, "and creates no texture");
}

fn transient(name: &str, desc: &TextureDesc) -> GraphOutput {
    GraphOutput {
        name: name.into(),
        desc: GraphResource::Texture(desc.clone()),
    }
}

#[test]
fn execute_binds_pooled_transients_and_imports_and_reuses_them_next_run() {
    let device = device();
    let tracked = TrackedDevice::observe(&device.device);
    let out_texture = device
        .device
        .create_texture(&hdr(16, 16).descriptor(Some("imported output")));
    let out_view = out_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let desc = hdr(16, 16);
    let mut graph = RenderGraph::versioned();
    let log = Arc::clone(&seen);
    graph.add_node(RenderGraphNode {
        id: "fill".into(),
        inputs: vec![],
        outputs: vec![transient("scratch", &desc)],
        slot: None,
        pipelines: vec![],
        record: Box::new(move |ctx| {
            let view = ctx.texture("scratch").ok_or_else(|| "no scratch".to_string())?;
            let _pass = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fill"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLUE),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            log.lock()
                .map_err(|e| e.to_string())?
                .push(format!("fill@{}", ctx.frame_index));
            Ok(())
        }),
    });
    graph.add_node(RenderGraphNode {
        id: "broken".into(),
        inputs: vec![GraphInput {
            name: "scratch".into(),
        }],
        outputs: vec![GraphOutput {
            name: "output".into(),
            desc: GraphResource::Imported,
        }],
        slot: None,
        pipelines: vec![],
        record: Box::new(|ctx| {
            assert!(ctx.texture("output").is_some(), "the import resolves by name");
            assert!(ctx.texture("missing").is_none());
            Err("refused on purpose".into())
        }),
    });
    let mut pool = TransientPool::default();
    for frame in [1, 2] {
        pool.begin_frame(frame);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let run = graph.execute(
            GraphExecution {
                device: &tracked,
                encoder: &mut encoder,
                pool: &mut pool,
                imports: &[("output", &out_view)],
                frame_index: frame,
            },
            &["output"],
        );
        assert_eq!(run.recorded, vec!["fill".to_string()]);
        assert_eq!(
            run.failed,
            vec![("broken".to_string(), "refused on purpose".to_string())]
        );
        assert_eq!(run.transients, 1);
        device.queue.submit([encoder.finish()]);
    }
    assert_eq!(
        pool.stats().created,
        1,
        "the second run reuses the pooled scratch"
    );
    assert_eq!(
        *seen.lock().expect("log"),
        vec!["fill@1".to_string(), "fill@2".to_string()]
    );
}
