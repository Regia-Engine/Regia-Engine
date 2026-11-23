// SPDX-License-Identifier: MPL-2.0
//! The audit trail's params fingerprint allocates nothing (sprint-deep-zai-agent-protocol
//! finding 3): `regia_agent::params_hash` streams the params' JSON text into FNV-1a instead of
//! building the whole string first, so a request with a fat params object costs the world
//! thread no params-sized allocation. This binary installs the shared counting allocator (the
//! test-target opt-out of `docs/04-conventions.md` § 5, ADR-1897; ADR-3291) and measures it.

use regia_test_utils::alloc_counter;
use regia_test_utils::alloc_counter::CountingAlloc;
use serde_json::json;

// The shared counting allocator (ADR-3291): this thread's requests and bytes.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

#[test]
fn hashing_a_one_mib_params_object_allocates_nothing_params_sized() {
    let params = json!({
        "big": "x".repeat(1 << 20),
        "escaped": "\"\n\u{1F600}".repeat(1 << 14),
        "nested": [{ "a": [1, 2.5, null, true, { "b": "c" }] }],
    });
    // The control: serializing the params to a string asks for at least their size.
    let (text, control) = alloc_counter::measure(|| serde_json::to_string(&params).unwrap_or_default());
    assert!(text.len() >= 1 << 20);
    assert!(
        control.bytes >= 1 << 20,
        "the counter sees the string form's allocation: {control:?}"
    );
    let (hash, stats) = alloc_counter::measure(|| regia_agent::params_hash(&params));
    assert_ne!(hash, 0);
    assert_eq!(
        stats.allocs,
        0,
        "the params hash allocated {} bytes in {} requests for {} bytes of params",
        stats.bytes,
        stats.allocs,
        text.len()
    );
}
