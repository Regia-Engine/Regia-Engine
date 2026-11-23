// SPDX-License-Identifier: MPL-2.0
//! ADR-8848 (ADR-8782 follow-up): a window `seeing.screenshot` reuses its buffers. Every shot used
//! to allocate a PNG, a base64 string, the serialized reply and the WebSocket frame copy, all
//! image-sized, and glibc kept the freed blocks: on the Steam Deck each shot grew the process by
//! about 10 MB (+63 MB over twenty, `deck-pass.sh rssprobe`). Now same-size shots after the
//! first make no image-sized allocation on the app side.
//!
//! The test runs the real path: the agent WebSocket server, a fresh client connection per shot
//! (as `regia agent call` makes), the world-side dispatch of `seeing.screenshot`, and the
//! window's screenshot callback served the way a present serves it (`take_for`), with a
//! Deck-sized capture (1280 x 800). The shared counting allocator (ADR-3291) counts every
//! thread; the client thread's own requests (it reassembles the reply) are subtracted, and a
//! hook records the largest request of every other thread. CPU only, no window, no wall clock.
//! This binary holds one test so no other test's allocations land in the process counters.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use regia_agent::client::Client;
use regia_agent::server;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_platform::PrimaryWindow;
use regia_render::{CapturedImage, ScreenshotRequests};
use regia_test_utils::alloc_counter::{self, AllocEvent, AllocHook, AllocStats, CountingAlloc};
use regia_test_utils::unique_temp_dir;
use serde_json::json;

/// The Steam Deck's window.
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const FRAME_BYTES: usize = (WIDTH * HEIGHT * 4) as usize;
/// Shots before the measured ones: the first fills the pools.
const WARM_SHOTS: usize = 2;
const MEASURED_SHOTS: usize = 6;

thread_local! {
    /// Set on the client thread: its requests are the caller's, not the app's.
    static CLIENT_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// The largest request any thread but the client made since the last reset.
static LARGEST_APP_REQUEST: AtomicUsize = AtomicUsize::new(0);

struct LargestAppRequest;

impl AllocHook for LargestAppRequest {
    fn on_request(&self, event: AllocEvent) {
        let client = CLIENT_THREAD.try_with(Cell::get).unwrap_or(false);
        if !client {
            LARGEST_APP_REQUEST.fetch_max(event.size, Ordering::Relaxed);
        }
    }
}

// The shared counting allocator (ADR-3291) with a largest-request hook.
#[global_allocator]
static GLOBAL: CountingAlloc<LargestAppRequest> = CountingAlloc::with_hook(LargestAppRequest);

/// A screenshot-like RGBA8 frame: gradients with a little noise, so the PNG is a few MB.
fn frame() -> CapturedImage {
    let mut state = 0x9e37_79b9_u32;
    let mut rgba = Vec::with_capacity(FRAME_BYTES);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let noise = (state & 0x1f) as u8;
            rgba.extend_from_slice(&[
                ((x * 255) / WIDTH) as u8 ^ noise,
                ((y * 255) / HEIGHT) as u8 ^ (noise >> 1),
                ((x + y) & 0xff) as u8,
                255,
            ]);
        }
    }
    CapturedImage {
        width: WIDTH,
        height: HEIGHT,
        rgba,
    }
}

/// What one shot cost: the app side (every thread but the client) and its largest request.
#[derive(Debug, Clone, Copy)]
struct ShotCost {
    app: AllocStats,
    largest_app_request: usize,
}

#[test]
fn same_size_window_screenshots_allocate_nothing_image_sized_after_the_first() {
    let server = server::start("secret".into()).expect("server starts");
    let url = format!("ws://{}", server.addr);
    let project = unique_temp_dir("agent-screenshot-buffer-reuse");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins(AgentPlugin);
    app.finish_plugins();
    app.insert_resource(AgentInbox(server.inbox.clone()));
    app.insert_resource(AgentPermissions {
        config: AgentConfig {
            rate_limit_per_second: u32::MAX,
            ..AgentConfig::default()
        },
        source: None,
    });
    app.insert_resource(ScreenshotRequests::default());
    let window = app.world_mut().spawn(PrimaryWindow).id();
    app.update();
    // Every shot's capture, made before any measurement: the readback's own buffer is the
    // renderer's (its reuse is `regia_render`'s screenshot_readback_reuse test).
    let total = WARM_SHOTS + MEASURED_SHOTS;
    let mut captures: Vec<CapturedImage> = (0..total).map(|_| frame()).collect();

    let done = AtomicBool::new(false);
    let (costs_tx, costs_rx) = mpsc::channel::<ShotCost>();
    std::thread::scope(|scope| {
        let client = scope.spawn(|| {
            CLIENT_THREAD.with(|c| c.set(true));
            for _ in 0..total {
                let process_before = alloc_counter::process_stats();
                let client_before = alloc_counter::thread_stats();
                LARGEST_APP_REQUEST.store(0, Ordering::Relaxed);
                let (width, height, b64_len) = {
                    // A fresh connection per shot, as `regia agent call` makes.
                    let mut client = Client::connect(&url, "secret").expect("client connects");
                    let result = client
                        .call("seeing.screenshot", json!({ "target": "window" }))
                        .expect("screenshot answers");
                    (
                        result["width"].as_u64(),
                        result["height"].as_u64(),
                        result["png_base64"].as_str().map_or(0, str::len),
                    )
                };
                let client_cost = alloc_counter::thread_stats().since(client_before);
                let process_cost = alloc_counter::process_stats().since(process_before);
                assert_eq!((width, height), (Some(u64::from(WIDTH)), Some(u64::from(HEIGHT))));
                assert!(b64_len > 0, "the reply carries the PNG");
                let app = AllocStats {
                    allocs: process_cost.allocs.saturating_sub(client_cost.allocs),
                    bytes: process_cost.bytes.saturating_sub(client_cost.bytes),
                };
                let largest_app_request = LARGEST_APP_REQUEST.load(Ordering::Relaxed);
                costs_tx
                    .send(ShotCost {
                        app,
                        largest_app_request,
                    })
                    .expect("test thread listens");
            }
            done.store(true, Ordering::Release);
        });
        // The world thread: a frame dispatches a waiting request, then the window's screenshot
        // is served as a present serves it. Frames run only when a request waits.
        while !done.load(Ordering::Acquire) && !client.is_finished() {
            if server.inbox.is_empty() {
                std::thread::yield_now();
                continue;
            }
            app.update();
            let callbacks = app
                .world_mut()
                .resource_mut::<ScreenshotRequests>()
                .take_for(window);
            for callback in callbacks {
                let capture = captures.pop().expect("one capture per shot");
                callback(Ok(capture));
            }
        }
        client.join().expect("client thread");
    });
    let _ = std::fs::remove_dir_all(&project);

    let costs: Vec<ShotCost> = costs_rx.try_iter().collect();
    assert_eq!(costs.len(), total, "every shot answered");
    eprintln!("frame {FRAME_BYTES} bytes; per-shot app-side cost: {costs:#?}");
    let first = costs[0];
    assert!(
        first.app.bytes >= FRAME_BYTES as u64 / 4,
        "the counter sees the first shot's encode and reply buffers: {first:?}"
    );
    for (i, cost) in costs.iter().enumerate().skip(WARM_SHOTS) {
        // No image-sized request: the PNG, base64 text, reply and frame copy come from the pools.
        assert!(
            cost.largest_app_request < FRAME_BYTES / 8,
            "shot {i}: an app-side request of {} bytes (frame {FRAME_BYTES}): {cost:?}",
            cost.largest_app_request
        );
        // What is left is per connection and per encode (the socket buffers, the deflate
        // state), not per image byte.
        assert!(
            cost.app.bytes < FRAME_BYTES as u64 / 4,
            "shot {i}: {} app-side bytes for a {FRAME_BYTES}-byte frame: {cost:?}",
            cost.app.bytes
        );
    }
}
