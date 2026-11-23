// SPDX-License-Identifier: MPL-2.0
//! wgpu's usage-scope pool, the last timing-dependent event on a steady frame's heap allocations
//! (ADR-5718), made to happen on purpose: passes replayed at the same moment grow the pool by six
//! allocations per extra pass, once, and a pooled scope grows again after the device has made
//! more resources. `regia_test_utils::device_pools::fill_usage_scope_pool` brings the pool to its
//! full size before a steady-frame test counts its batches; here its threads are traced.
//!
//! The test also holds the two things the fill leans on in wgpu-core, which wgpu does not
//! promise: a pass takes a usage scope while `CommandEncoder::finish` replays it, and a replayed
//! `set_blend_constant` logs one fixed trace line. When a wgpu upgrade changes either, this test
//! fails and says so; the steady-frame tests then still pass on their slack, as before the fill.
//!
//! Its own test binary: it installs the counting global allocator.

use regia_render::RenderDevice;
use regia_test_utils::alloc_counter::{self, AllocTrace, CountingAlloc};
use regia_test_utils::device_pools::fill_usage_scope_pool_on;

#[global_allocator]
static A: CountingAlloc = CountingAlloc::new();

/// Requests made while wgpu-core took a usage scope from its pool (a new scope's lists, or the
/// lists of a pooled one growing).
fn pool_requests(trace: &AllocTrace) -> u64 {
    trace
        .sites()
        .iter()
        .filter(|site| {
            site.stack
                .iter()
                .any(|frame| frame.contains("new_usage_scope") || frame.contains("new_pooled"))
        })
        .map(|site| site.requests)
        .sum()
}

/// Requests that grew a map of textures used in part: a usage scope's, kept in the pool, or the
/// one each command buffer makes for itself.
fn partial_texture_map_requests(trace: &AllocTrace) -> u64 {
    trace
        .sites()
        .iter()
        .filter(|site| {
            site.stack
                .iter()
                .any(|frame| frame.contains("ComplexTextureState"))
        })
        .map(|site| site.requests)
        .sum()
}

/// Whether this build's backtraces name wgpu-core's functions, inlined ones included (the
/// workspace's line tables are enough where the gate runs). Without the names a request cannot be
/// attributed to the pool, and only the meeting of the passes is checked.
fn names_wgpu_core(trace: &AllocTrace) -> bool {
    trace.sites().iter().any(|site| {
        site.stack
            .iter()
            .any(|frame| frame.contains("encode_render_pass"))
    })
}

fn buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("a tracked buffer"),
        size: 64,
        usage: wgpu::BufferUsages::UNIFORM,
        mapped_at_creation: false,
    })
}

#[test]
fn passes_replayed_at_the_same_moment_grow_the_usage_scope_pool_once() {
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let device = &device.device;
    // A buffer, so a scope's buffer lists have a size, as in any frame (the fill makes a texture).
    let _tracked = buffer(device);
    // The fill with its threads counted, so the trace holds what they request.
    let fill = |passes: usize| {
        let (met, trace) = alloc_counter::trace_counted(|| {
            fill_usage_scope_pool_on(device, passes, alloc_counter::count_this_thread)
        });
        assert!(
            met,
            "the {passes} passes did not meet inside their replay: wgpu-core no longer logs \
             `RenderPass::set_blend_constant` at trace level while it replays a pass, the build strips trace-level \
             `log` lines, or another logger is installed. `device_pools::fill_usage_scope_pool` needs a new \
             meeting point; until then the steady-frame tests rely on their slack again"
        );
        trace
    };

    // One pass alone: the pool's first scope, which any frame makes. Three lists for the buffers
    // and three for the textures.
    let one = fill(1);
    if !names_wgpu_core(&one) {
        eprintln!(
            "the requests are not counted: this build's backtraces do not name wgpu-core's functions. The passes \
             met at every size, which is what the fill needs"
        );
        fill(4);
        fill(2);
        return;
    }
    assert_eq!(pool_requests(&one), 6, "the first scope:\n{}", one.report(16));
    // Four passes inside their replay at the same moment: three more scopes.
    let four = fill(4);
    assert_eq!(
        pool_requests(&four),
        18,
        "three more scopes:\n{}",
        four.report(16)
    );
    // The same again takes the four scopes from the pool, and fewer passes never grow it: this is
    // the state `fill_usage_scope_pool` leaves a steady-frame test in.
    let again = fill(4);
    assert_eq!(pool_requests(&again), 0, "a filled pool:\n{}", again.report(16));
    let fewer = fill(2);
    assert_eq!(pool_requests(&fewer), 0, "a filled pool:\n{}", fewer.report(16));
    // Each scope also keeps a map of the textures a pass uses in part, grown by the first pass
    // that needs more room. The fill's passes use several, so the scopes' maps grew in the first
    // fills and not in the later ones; what is left per pass is the command buffer's own map.
    eprintln!(
        "requests that grew a map of textures used in part: one pass {}, four at once {}, again {}, two {}",
        partial_texture_map_requests(&one),
        partial_texture_map_requests(&four),
        partial_texture_map_requests(&again),
        partial_texture_map_requests(&fewer)
    );
    assert!(
        partial_texture_map_requests(&four) > partial_texture_map_requests(&again),
        "the scopes' maps grow once:\n{}",
        four.report(24)
    );
    assert_eq!(
        partial_texture_map_requests(&again),
        2 * partial_texture_map_requests(&fewer),
        "per pass only the command buffer's own map is left"
    );

    // The device makes more buffers: each pooled scope's buffer lists grow the next time it is
    // taken, and only then. A scope that only a rare overlap reaches would grow in the frame of
    // that overlap; a fill after the resources exist grows all of them at once.
    let more: Vec<wgpu::Buffer> = (0..600).map(|_| buffer(device)).collect();
    let grown = fill(4);
    let grew = pool_requests(&grown);
    assert!(
        (4..=12).contains(&grew),
        "four scopes, up to three buffer lists each, grew by {grew} requests:\n{}",
        grown.report(16)
    );
    let settled = fill(4);
    assert_eq!(pool_requests(&settled), 0, "grown once:\n{}", settled.report(16));
    drop(more);
}
