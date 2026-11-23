// SPDX-License-Identifier: MPL-2.0
//! Run-time gate for the hardware ray-query tests (book/agents/hwrt.md, "Testing ray queries
//! without a card").
//!
//! A ray-query test asks for [`ray_query_device`]: it gets a headless device whose own features
//! include `EXPERIMENTAL_RAY_QUERY` (lavapipe on Mesa 24.1 and later, an RTX card, an RX 7900),
//! and otherwise the helper prints `skipped: no ray queries on <adapter>` and returns `None`, so
//! the test returns early. The decision reads the returned device's own features, never
//! [`regia_render::hwrt::last_device_report`]: that global names whichever device was created most recently, and a
//! parallel sibling test's plain device can overwrite the report between this helper's device
//! creation and its read (audit rescue-hwrt-lavapipe-coverage finding 1). `REGIA_REQUIRE_RAY_QUERIES=1`
//! (the `REGIA_REQUIRE_ASSETS` precedent) turns the skip into a panic so a regressed host cannot
//! let these tests pass without covering ray queries. A missing adapter is still a failure: that
//! is not a ray-query question.

use regia_render::RenderDevice;
use regia_render::hwrt::{HwrtReason, HwrtRequest, RayTracingProbe};

/// A headless device that has hardware ray queries, or `None` after printing
/// `skipped: no ray queries on <adapter>` (with the probed reason).
///
/// # Panics
///
/// When no headless adapter exists at all: the plain lavapipe tests fail the same way. With
/// `REGIA_REQUIRE_RAY_QUERIES=1`, also when the adapter has no ray queries: a job that must
/// cover ray queries fails loudly instead of skipping.
pub fn ray_query_device() -> Option<RenderDevice> {
    let device = match RenderDevice::headless_with_ray_queries() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let name = device.adapter.get_info().name;
    if device
        .device
        .features()
        .contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY)
    {
        println!("adapter: {name}");
        return Some(device);
    }
    skip(&name, probe_reason(&device))
}

/// Why `device` ended up without ray queries, for the skip line.
fn probe_reason(device: &RenderDevice) -> HwrtReason {
    RayTracingProbe::from_adapter(&device.adapter)
        .decide(HwrtRequest::On, false)
        .reason
}

/// Prints the skip line and returns `None`, or panics when `REGIA_REQUIRE_RAY_QUERIES=1` says a
/// skip is a failure.
fn skip(name: &str, reason: HwrtReason) -> Option<RenderDevice> {
    println!("skipped: no ray queries on {name} ({})", reason.message());
    if ray_queries_required() {
        panic!(
            "REGIA_REQUIRE_RAY_QUERIES=1 but no ray queries on {name}: {}",
            reason.message()
        );
    }
    None
}

/// `REGIA_REQUIRE_RAY_QUERIES=1` (set by a job that must cover the ray-query tests): a missing
/// ray-query adapter is a failure, not a skip, so a regressed host cannot turn these tests
/// vacuous (the `REGIA_REQUIRE_ASSETS` precedent, audit-v035-flaky M1).
fn ray_queries_required() -> bool {
    std::env::var_os("REGIA_REQUIRE_RAY_QUERIES").is_some_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The only tests in this binary that read or write `REGIA_REQUIRE_RAY_QUERIES`, serialized:
    /// the unsafe env mutation is sound because no other test in this process touches the helper
    /// while a guard is held.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env(value: Option<&str>, run: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // ADR-4926: the workspace denies `unsafe_code`; this test helper is the narrow exception,
        // same pattern as `HwrtDecision::experimental_features` (ADR-515). SAFETY: within this
        // test binary the variable is read only by `ray_queries_required` (through `skip`), and
        // this binary's tests are serialized on `ENV_LOCK`, so no other thread reads the
        // environment while it is being replaced.
        #[allow(unsafe_code)]
        unsafe {
            match value {
                Some(v) => std::env::set_var("REGIA_REQUIRE_RAY_QUERIES", v),
                None => std::env::remove_var("REGIA_REQUIRE_RAY_QUERIES"),
            }
        }
        run();
        // SAFETY: as above, no concurrent reader while the variable is restored.
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("REGIA_REQUIRE_RAY_QUERIES")
        };
    }

    #[test]
    fn skip_returns_none_when_not_required() {
        with_env(None, || {
            assert!(skip("some adapter", HwrtReason::NoAdapterRayQuery).is_none());
        });
    }

    #[test]
    #[should_panic(expected = "REGIA_REQUIRE_RAY_QUERIES=1 but no ray queries on some adapter")]
    fn skip_panics_when_required() {
        with_env(Some("1"), || {
            let _ = skip("some adapter", HwrtReason::NoAdapterRayQuery);
        });
    }

    #[test]
    fn skip_returns_none_for_a_value_other_than_1() {
        with_env(Some("0"), || {
            assert!(skip("some adapter", HwrtReason::NoAdapterRayQuery).is_none());
        });
    }
}
