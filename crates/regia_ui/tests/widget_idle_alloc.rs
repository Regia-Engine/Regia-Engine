// SPDX-License-Identifier: MPL-2.0
//! Review rev-kimi-20 finding 4: an idle dropdown and tab bar compare the shown label with the
//! widget's string in place, so an idle `ui_widgets` pass allocates nothing that grows with the
//! label length. Counted with a counting global allocator (bytes) around direct, single-threaded
//! `ui_widgets` calls (no timers). One test per binary, so no other test's allocations are
//! counted.

// The counting global allocator is the only unsafe code (ADR-3147; the pattern of
// `ui_component_alloc.rs`).
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_app::prelude::*;
use regia_ui::prelude::*;

struct Counting;

static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.update();
    app
}

const IDLE_CALLS: u64 = 5;

/// Bytes allocated by `IDLE_CALLS` idle `ui_widgets` passes over one dropdown and one three-tab
/// bar whose labels are `len` characters long.
fn idle_bytes(len: usize) -> u64 {
    let mut app = app();
    let label = |c: char| std::iter::repeat_n(c, len).collect::<String>();
    let world = app.world_mut();
    world.spawn(Dropdown {
        options: vec![label('a'), label('b'), label('c')],
        selected: Some(1),
        open: false,
    });
    world.spawn(TabBar {
        tabs: vec![label('x'), label('y'), label('z')],
        active: 0,
    });
    // Build the children and labels, then let every one-off cache warm.
    for _ in 0..3 {
        regia_ui::widgets::ui_widgets(world);
    }
    let before = BYTES.load(Ordering::Relaxed);
    for _ in 0..IDLE_CALLS {
        regia_ui::widgets::ui_widgets(world);
    }
    BYTES.load(Ordering::Relaxed) - before
}

#[test]
fn idle_dropdown_and_tab_sync_allocate_nothing_per_label_byte() {
    const LONG: usize = 4096;
    // Warm process-wide one-offs (type registrations, query state) before measuring.
    idle_bytes(8);
    let short = idle_bytes(8);
    let long = idle_bytes(LONG);
    // One label clone in one of the idle passes is already LONG bytes.
    assert!(
        long < short + LONG as u64,
        "idle passes allocated {long} bytes with {LONG}-char labels and {short} bytes with \
         8-char labels: a label is cloned on an idle frame"
    );
}
