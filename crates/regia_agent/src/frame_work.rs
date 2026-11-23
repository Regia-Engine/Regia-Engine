// SPDX-License-Identifier: MPL-2.0
//! `render.frame_work`: the device-independent work of the newest window frame (passes,
//! dispatches, draws, triangles, binds, allocations, uploads) and the registered render targets'
//! bytes (`book/agents/frame-work.md`, ADR-2711).

use regia_ecs::prelude::World;
use regia_render::frame_work::{self, FRAME_WORK_FORMAT_VERSION};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `render.frame_work`: the first call turns the per-frame report on (nothing is read per frame
/// before that), so `frame` is `null` until a window frame has finished after it.
pub(crate) fn frame_work(_world: &mut World) -> Result<Value, RpcError> {
    let was_on = frame_work::window_reports_on();
    frame_work::request_window_reports();
    let targets = regia_render::resource_inspector::list_resources();
    let target_bytes: u64 = targets.iter().map(|t| t.bytes).sum();
    let (frame, seq) = match frame_work::last_window_frame_work() {
        Some((work, seq)) => (serde_json::to_value(work).unwrap_or(Value::Null), seq),
        None => (Value::Null, 0),
    };
    Ok(json!({
        "format_version": FRAME_WORK_FORMAT_VERSION,
        "reporting": true,
        "started_now": !was_on,
        "seq": seq,
        "frame": frame,
        "targets": { "count": targets.len(), "bytes": target_bytes },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_call_turns_reporting_on_and_answers_without_a_renderer() {
        let mut world = World::new();
        let first = frame_work(&mut world).expect("frame_work");
        assert_eq!(first["format_version"], FRAME_WORK_FORMAT_VERSION);
        assert_eq!(first["reporting"], true);
        assert!(first["targets"]["bytes"].is_u64());
        assert!(frame_work::window_reports_on());
        let second = frame_work(&mut world).expect("frame_work");
        assert_eq!(second["started_now"], false);
    }
}
