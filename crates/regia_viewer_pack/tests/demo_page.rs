// SPDX-License-Identifier: MPL-2.0
//! The static demo page stays in step with the web player's API (JW-L1): `RegiaViewer.create`,
//! `open`, `options`, `setPick`, `build`, `setView` / `resetView`, events `ready`, `changed`,
//! `error`; it reads only static files (no server URL, no stream).
#![cfg(feature = "cook")]

use std::sync::atomic::{AtomicU32, Ordering};

use regia_viewer_pack::standin::DEMO_PAGE;

#[test]
fn the_page_uses_the_player_api() {
    for name in [
        "RegiaViewer.create(",
        ".open(",
        ".options()",
        ".setPick(",
        ".build()",
        ".resetView()",
        "on(\"ready\"",
        "on(\"changed\"",
        "on(\"error\"",
        "manifest.json",
        "options.json",
    ] {
        assert!(DEMO_PAGE.contains(name), "index.html lacks {name}");
    }
}

#[test]
fn the_page_uses_only_static_files() {
    for bad in [
        "http://",
        "https://",
        "ws://",
        "wss://",
        "RTCPeerConnection",
        "WebSocket",
        "EventSource",
    ] {
        assert!(!DEMO_PAGE.contains(bad), "{bad} in the demo page");
    }
}

#[test]
fn standin_pack_writes_the_demo_page() {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "regia-viewer-demo-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let demo = regia_viewer_pack::standin::write_demo(&dir).expect("demo");
    assert_eq!(
        std::fs::read_to_string(demo.join("index.html")).ok().as_deref(),
        Some(DEMO_PAGE)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
