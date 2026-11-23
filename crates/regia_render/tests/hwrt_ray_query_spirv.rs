// SPDX-License-Identifier: MPL-2.0
//! SPIR-V checks for the two ray-query shaders (ADR-3384). The SPIR-V gate's device list cannot
//! hold them (they need `EXPERIMENTAL_RAY_QUERY`), so they were validated only on a card. This
//! test runs the gate's own [`regia_render::validate_ray_query_spirv_module`] on each — naga
//! validation, pipeline-override processing, the ADR-026 layout scan with the ADR-135 allowlist
//! (exempting only the `ray_query` builtin's own laid-out object), and
//! `spirv-val --target-env vulkan1.3` — so the gate cannot drift from this test (audit
//! rescue-hwrt-lavapipe-coverage finding 4). The test itself adds only the ray-query scan on the
//! emitted words: naga must declare `OpCapability RayQueryKHR` and the `SPV_KHR_ray_query`
//! extension for each shader.

use regia_render::hwrt::{RAY_QUERY_SHADOW_WGSL, SUN_TRACE_RAY_QUERY_WGSL};

const OP_CAPABILITY: u32 = 17;
const OP_EXTENSION: u32 = 10;
const CAPABILITY_RAY_QUERY_KHR: u32 = 4472;

/// Whether the module declares `OpCapability RayQueryKHR` and the `SPV_KHR_ray_query` extension.
fn declares_ray_query(words: &[u32]) -> (bool, bool) {
    let (mut capability, mut extension) = (false, false);
    let mut at = 5; // the header
    while at < words.len() {
        let count = (words[at] >> 16) as usize;
        let op = words[at] & 0xFFFF;
        if count == 0 {
            break;
        }
        if op == OP_CAPABILITY && words.get(at + 1) == Some(&CAPABILITY_RAY_QUERY_KHR) {
            capability = true;
        }
        if op == OP_EXTENSION {
            let bytes: Vec<u8> = words[at + 1..at + count]
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect();
            if bytes.starts_with(b"SPV_KHR_ray_query") {
                extension = true;
            }
        }
        at += count;
    }
    (capability, extension)
}

fn check(name: &str, source: &str) {
    let mut module =
        naga::front::wgsl::parse_str(source).unwrap_or_else(|err| panic!("{name}: parse: {err}"));
    regia_render::specialize_handle_arguments(&mut module);
    let words =
        regia_render::validate_ray_query_spirv_module(name, &module).unwrap_or_else(|err| panic!("{err}"));
    let (capability, extension) = declares_ray_query(&words);
    assert!(capability, "{name}: no OpCapability RayQueryKHR in the module");
    assert!(extension, "{name}: no SPV_KHR_ray_query extension in the module");
}

#[test]
fn the_shadow_ray_query_pass_is_valid_spirv() {
    check("hwrt_shadow_ray_query.wgsl", RAY_QUERY_SHADOW_WGSL);
}

#[test]
fn the_sun_trace_ray_query_pass_is_valid_spirv() {
    check("sun_trace_ray_query.wgsl", SUN_TRACE_RAY_QUERY_WGSL);
}
