// SPDX-License-Identifier: MPL-2.0
//! `ui.*`: see and drive the runtime UI (`regia_ui`) through the agent protocol. This crate must
//! not depend on `regia_ui` (ADR-202), so the host registers one function pointer,
//! [`UiAgentHost`], that runs a `ui.*` method against the world; `regia` does it when both the
//! `agent` and `ui` features are on (`UiAgentHost::new(regia_ui::agent_bridge::host_call)`).
//! The UI side is `regia_ui::agent_bridge`. Book page `agents/ui.md`.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// What a failed UI call returns: `(class, reason, message, hint)`; the shape of
/// `regia_ui::agent_bridge::HostError`.
pub type UiHostError = (&'static str, &'static str, String, String);

/// The function that runs one `ui.*` method against the world.
pub type UiHostCall = fn(&mut World, &str, &Value) -> Result<Value, UiHostError>;

/// The host's hook into the runtime UI. Without it every `ui.*` method answers `ui_not_installed`.
#[derive(Resource, Clone, Copy)]
pub struct UiAgentHost {
    call: UiHostCall,
}

impl UiAgentHost {
    /// Registers `call`, normally `regia_ui::agent_bridge::host_call`.
    #[must_use]
    pub fn new(call: UiHostCall) -> Self {
        Self { call }
    }
}

/// Maps a bridge error class onto the protocol's error codes (`not_found` F0003, `unsupported` F0013,
/// everything else F0002); `data.reason` carries the stable snake_case reason.
fn rpc_error(error: UiHostError) -> RpcError {
    let (class, reason, message, hint) = error;
    let (code, regia_code) = match class {
        "not_found" => (protocol::NOT_FOUND, "F0003"),
        "unsupported" => (protocol::UNSUPPORTED, "F0013"),
        _ => (protocol::INVALID_PARAMS, "F0002"),
    };
    RpcError {
        code,
        message,
        data: Some(json!({ "code": regia_code, "reason": reason, "hint": hint })),
    }
}

fn not_installed() -> RpcError {
    rpc_error((
        "unsupported",
        "ui_not_installed",
        "this app has no runtime UI hooked to the agent".to_owned(),
        "the app needs regia_ui::UiPlugin and a regia_agent::UiAgentHost resource (DefaultPlugins with the ui and agent features adds both)"
            .to_owned(),
    ))
}

/// Runs a `ui.*` method through the registered host.
pub(crate) fn dispatch(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
    let host = world
        .get_resource::<UiAgentHost>()
        .copied()
        .ok_or_else(not_installed)?;
    (host.call)(world, method, params).map_err(rpc_error)
}

/// `ui.world_click` (ADR-2406): a screen point (`x`/`y` or `u`/`v`, optional `camera`) becomes a
/// world ray through `camera.raycast`'s ray; a ray (screen or given) is cast against the scene
/// with `world.pick`'s query, and the nearest surface's distance goes to the UI side as
/// `occluder_distance`, which rejects a panel behind it. `{entity, uv?}` clicks a panel directly.
pub(crate) fn world_click(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let host = world
        .get_resource::<UiAgentHost>()
        .copied()
        .ok_or_else(not_installed)?;
    // Only an object is extended below (`Value` indexing would panic on an array or a number);
    // anything else reaches the UI side as is and is refused there.
    let mut params = params.clone();
    let direct = !params.is_object() || params.get("entity").is_some_and(|v| !v.is_null());
    if !direct {
        let screen = ["x", "y", "u", "v"]
            .iter()
            .any(|k| params.get(*k).is_some_and(|v| !v.is_null()));
        let ray = params.get("ray").filter(|v| v.is_object()).cloned();
        if screen && params.get("origin").is_none() && ray.is_none() {
            let mut cast = params.clone();
            cast["ray_only"] = json!(true);
            let shot = crate::camera_raycast::camera_raycast(world, &cast)?;
            params["origin"] = shot["origin"].clone();
            params["direction"] = shot["direction"].clone();
        }
        let source = params
            .get("ray")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| params.clone());
        let origin = vec3(source.get("origin"));
        let direction = vec3(source.get("direction"));
        if let (Some(origin), Some(direction)) = (origin, direction)
            && direction.length_squared() > f32::MIN_POSITIVE
            && params.get("occluder_distance").is_none_or(Value::is_null)
            && crate::params::opt_bool(&params, "occlusion")? != Some(false)
        {
            let hit = crate::pick::cast(world, regia_core::math::Ray::new(origin, direction), &json!({}))?;
            if let Some(distance) = hit["hit"]["distance"].as_f64() {
                params["occluder_distance"] = json!(distance);
            }
        }
    }
    (host.call)(world, "ui.world_click", &params).map_err(rpc_error)
}

fn vec3(value: Option<&Value>) -> Option<regia_core::math::Vec3> {
    let a = value?.as_array().filter(|a| a.len() == 3)?;
    let x = a[0].as_f64().filter(|f| f.is_finite())? as f32;
    let y = a[1].as_f64().filter(|f| f.is_finite())? as f32;
    let z = a[2].as_f64().filter(|f| f.is_finite())? as f32;
    Some(regia_core::math::Vec3::new(x, y, z))
}

/// Runs a `ui.theme_*` method (book page `agents/ui-theme.md`). `ui.theme_set` with
/// `persist: true` writes a file, so it also needs the `assets` permission. `ui.theme_preview`'s
/// raw pixels are encoded here, through the capture path's PNG encoder, into `png_base64` and a
/// file under the scratch output dir (`crate::scratch_out_dir`: the system temp directory on
/// desktop, the app-private data directory on Android).
pub(crate) fn dispatch_theme(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
    if method == "ui.theme_set" && crate::params::opt_bool(params, "persist")? == Some(true) {
        let permissions = world
            .get_resource_or_insert_with::<crate::safety::AgentPermissions>(Default::default)
            .clone();
        if !permissions.allows(crate::schema::Permission::Assets) {
            return Err(permissions.denied_error(crate::schema::Permission::Assets));
        }
    }
    // Without the world's device the UI side opens a headless one for the preview; a host that
    // opened no GPU on purpose refuses first (ADR-5180).
    if method == "ui.theme_preview" && world.get_resource::<regia_render::RenderDevice>().is_none() {
        crate::seeing::GpuDisabled::refuse(world)?;
    }
    let mut out = dispatch(world, method, params)?;
    if method == "ui.theme_preview" {
        finish_preview(world, &mut out)?;
    }
    Ok(out)
}

/// Previews written this session, for unique file names.
static PREVIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Preview files one process keeps in the temp directory.
const PREVIEWS_KEPT: u64 = 16;

/// Replaces a preview's `rgba_base64` with `png_base64`, `mime` and the written file's `path`.
fn finish_preview(world: &World, out: &mut Value) -> Result<(), RpcError> {
    let Some(map) = out.as_object_mut() else {
        return Err(RpcError::internal("ui.theme_preview answered without an object"));
    };
    let width = map.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
    let height = map.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
    let raw = map
        .remove("rgba_base64")
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or_else(|| RpcError::internal("ui.theme_preview answered without pixels"))?;
    let rgba = crate::image::base64_decode(&raw).map_err(RpcError::internal)?;
    if rgba.len() != (width as usize) * (height as usize) * 4 {
        return Err(RpcError::internal(format!(
            "ui.theme_preview returned {} bytes for {width}x{height}",
            rgba.len()
        )));
    }
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        writer
            .write_image_data(&rgba)
            .map_err(|e| RpcError::internal(e.to_string()))?;
    }
    let dir = crate::scratch_out_dir(world, "regia-theme-preview");
    let n = PREVIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = write_preview(&dir, n, &png_bytes)
        .map_err(|e| RpcError::internal(format!("cannot write preview {n} to {}: {e}", dir.display())))?;
    // Keep the last PREVIEWS_KEPT previews of this process; older files are removed.
    if let Some(old) = n.checked_sub(PREVIEWS_KEPT) {
        let _ = std::fs::remove_file(dir.join(format!("{}-{old}.png", std::process::id())));
    }
    map.insert("mime".into(), json!("image/png"));
    map.insert("png_base64".into(), json!(crate::image::base64(&png_bytes)));
    map.insert("path".into(), json!(path.display().to_string()));
    Ok(())
}

/// A `wait_until` / `play.expect` atom `{ "predicate": "ui.node_exists" | "ui.node_state" |
/// "ui.theme_generation", ... }`: `Ok((holds, detail))`, or `Err` when it cannot be evaluated at
/// all.
pub(crate) fn predicate(world: &mut World, name: &str, atom: &Value) -> Result<(bool, String), String> {
    // `play.expect` / `play.wait_until` are read-only: only the read predicates may reach the host
    // (the theme generation of book/agents/ui-theme.md among them, ADR-1384), never `ui.click` /
    // `ui.focus` / `ui.type` / `ui.set_value` / `ui.layers` / `ui.theme_select`.
    if !matches!(name, "ui.node_exists" | "ui.node_state" | "ui.theme_generation") {
        return Err(format!(
            "unknown ui predicate {name:?}; use ui.node_exists, ui.node_state or ui.theme_generation \
             (ui_bad_predicate)"
        ));
    }
    let host = world
        .get_resource::<UiAgentHost>()
        .copied()
        .ok_or_else(|| "this app has no runtime UI hooked to the agent (ui_not_installed)".to_owned())?;
    let out =
        (host.call)(world, name, atom).map_err(|(_, reason, message, _)| format!("{reason}: {message}"))?;
    let holds = out.get("holds").and_then(Value::as_bool).unwrap_or(false);
    let detail = out
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok((holds, detail))
}

/// Writes preview `n` as `dir/<pid>-<n>.png` through `dir/.<pid>-<n>.png.tmp`. The temp dir is
/// shared, so the names are predictable to another local user: the temp file is created
/// exclusively (a link planted at its name is removed, and one planted again in between makes
/// the create fail instead of being written through) and the rename replaces whatever is at the
/// final name without following it (audit-agent-sec Low).
fn write_preview(dir: &std::path::Path, n: u64, bytes: &[u8]) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}-{n}.png", std::process::id()));
    let tmp = dir.join(format!(".{}-{n}.png.tmp", std::process::id()));
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes));
    let renamed = written.and_then(|()| std::fs::rename(&tmp, &path));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed.map(|()| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// audit-agent-sec (Low): a symlink planted at the predictable temp or final name was
    /// written through (`fs::write` follows links).
    #[cfg(unix)]
    #[test]
    fn a_planted_link_at_a_preview_name_is_replaced_not_written_through() {
        let dir = regia_test_utils::unique_temp_dir("theme-preview-links");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "KEEP").expect("victim");
        let pid = std::process::id();
        std::os::unix::fs::symlink(&victim, dir.join(format!(".{pid}-7.png.tmp"))).expect("tmp link");
        std::os::unix::fs::symlink(&victim, dir.join(format!("{pid}-7.png"))).expect("final link");
        let path = write_preview(&dir, 7, b"png bytes").expect("written");
        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "KEEP");
        assert_eq!(std::fs::read(&path).expect("preview"), b"png bytes");
        assert!(
            !std::fs::symlink_metadata(&path)
                .expect("meta")
                .file_type()
                .is_symlink()
        );
        assert!(!dir.join(format!(".{pid}-7.png.tmp")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fake(_world: &mut World, method: &str, params: &Value) -> Result<Value, UiHostError> {
        match method {
            "ui.node_exists" | "ui.theme_generation" => {
                Ok(json!({ "holds": params["want"], "detail": "fake" }))
            }
            "ui.click" => Err((
                "rejected",
                "ui_node_disabled",
                "disabled".to_owned(),
                "enable it".to_owned(),
            )),
            _ => Err(("not_found", "ui_node_not_found", "none".to_owned(), String::new())),
        }
    }

    #[test]
    fn without_a_host_the_methods_say_so() {
        let mut world = World::new();
        let err = dispatch(&mut world, "ui.query", &json!({})).expect_err("no host");
        assert_eq!(err.code, protocol::UNSUPPORTED);
        assert_eq!(
            err.data.as_ref().and_then(|d| d["reason"].as_str()),
            Some("ui_not_installed")
        );
        assert!(predicate(&mut world, "ui.node_exists", &json!({})).is_err());
    }

    #[test]
    fn errors_carry_a_snake_case_reason_and_the_class_code() {
        let mut world = World::new();
        world.insert_resource(UiAgentHost::new(fake));
        let err = dispatch(&mut world, "ui.click", &json!({})).expect_err("rejected");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert_eq!(
            err.data.as_ref().and_then(|d| d["reason"].as_str()),
            Some("ui_node_disabled")
        );
        let missing = dispatch(&mut world, "ui.focus", &json!({})).expect_err("missing");
        assert_eq!(missing.code, protocol::NOT_FOUND);
    }

    fn unreachable_host(_world: &mut World, method: &str, _params: &Value) -> Result<Value, UiHostError> {
        panic!("host reached for {method}")
    }

    #[test]
    fn mutating_methods_are_not_predicates() {
        let mut world = World::new();
        world.insert_resource(UiAgentHost::new(unreachable_host));
        for name in [
            "ui.click",
            "ui.focus",
            "ui.type",
            "ui.set_value",
            "ui.layers",
            "ui.query",
            "ui.theme_select",
        ] {
            let err = predicate(&mut world, name, &json!({})).expect_err("rejected before the host");
            assert!(err.contains("ui_bad_predicate"), "{name}: {err}");
        }
    }

    #[test]
    fn the_theme_generation_is_a_read_predicate() {
        let mut world = World::new();
        world.insert_resource(UiAgentHost::new(fake));
        let (holds, detail) =
            predicate(&mut world, "ui.theme_generation", &json!({ "want": true })).expect("evaluates");
        assert!(holds);
        assert_eq!(detail, "fake");
    }

    #[test]
    fn predicates_report_holds_and_detail() {
        let mut world = World::new();
        world.insert_resource(UiAgentHost::new(fake));
        let (holds, detail) =
            predicate(&mut world, "ui.node_exists", &json!({ "want": true })).expect("evaluates");
        assert!(holds);
        assert_eq!(detail, "fake");
        let (holds, _) =
            predicate(&mut world, "ui.node_exists", &json!({ "want": false })).expect("evaluates");
        assert!(!holds);
    }

    /// A `ui.focus` request answered in `PreUpdate` reaches the UI drain in the same frame: the
    /// request pump is ordered before the (conditional) drain, so the focus is applied by the
    /// first `update`, not the next one.
    #[test]
    fn requests_reach_the_ui_drain_in_the_same_frame() {
        use regia_ui::{Focusable, UiNode, UiPlugin, agent_bridge::UiAgentBridge, node::Focused};
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(UiPlugin::code_only())
            .add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        // `build` only: `finish` would start the agent server.
        Plugin::build(&crate::AgentPlugin, &mut app);
        app.insert_resource(crate::safety::AgentPermissions {
            config: crate::safety::AgentConfig::default(),
            source: None,
        });
        app.insert_resource(UiAgentHost::new(regia_ui::agent_bridge::host_call));
        let node = app.world_mut().spawn((UiNode, Focusable::default())).id();
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        let (tx, rx) = crossbeam_channel::bounded(1);
        inbox_tx
            .send(crate::server::Inbound {
                request: protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: "ui.focus".into(),
                    params: json!({ "node": node.to_bits() }),
                },
                reply: tx,
            })
            .expect("queued");
        app.insert_resource(crate::AgentInbox(inbox_rx));
        app.update();
        assert!(rx.try_recv().is_ok(), "the request was answered in the frame");
        assert_eq!(app.world().resource::<UiAgentBridge>().pending_frames(), 0);
        assert!(
            app.world().get::<Focused>(node).is_some(),
            "drained in the same frame"
        );
    }
    fn echo(_world: &mut World, method: &str, params: &Value) -> Result<Value, UiHostError> {
        Ok(json!({ "method": method, "params": params }))
    }

    fn preview_host(_world: &mut World, _method: &str, _params: &Value) -> Result<Value, UiHostError> {
        // 1x1 opaque red RGBA: ff 00 00 ff -> "/wAA/w==".
        Ok(json!({ "width": 1, "height": 1, "rgba_base64": "/wAA/w==", "generation": 2 }))
    }

    /// audit-agent7-platform (M): the preview file must land under the scratch output dir, which
    /// on Android is the app-private data dir, not the shell-owned `/data/local/tmp` that
    /// `std::env::temp_dir()` falls back to there.
    #[test]
    fn theme_preview_png_lands_under_the_scratch_dir() {
        let mut world = World::new();
        world.insert_resource(UiAgentHost::new(preview_host));
        let out =
            dispatch_theme(&mut world, "ui.theme_preview", &json!({ "page": "buttons" })).expect("preview");
        let want = crate::scratch_out_dir(&world, "regia-theme-preview");
        let path = out["path"].as_str().expect("path");
        assert!(
            path.starts_with(&*want.to_string_lossy()),
            "preview at {path}, expected under {}",
            want.display()
        );
        assert_eq!(out["mime"], "image/png");
        assert!(out["png_base64"].as_str().is_some_and(|s| !s.is_empty()));
        let _ = std::fs::remove_file(path);
    }

    thread_local! {
        /// `ui.theme_preview` calls that reached the UI side, whose preview opens a device.
        static PREVIEWS_ASKED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn counting_preview_host(world: &mut World, method: &str, params: &Value) -> Result<Value, UiHostError> {
        if method == "ui.theme_preview" {
            PREVIEWS_ASKED.with(|asked| asked.set(asked.get() + 1));
        }
        preview_host(world, method, params)
    }

    /// ADR-5180: on a host that opened no GPU on purpose (`GpuDisabled`, the headless editor's
    /// `--no-gpu`) `ui.theme_preview` answers its code before the UI side, which would open a
    /// headless device for the preview, is called. The other theme methods need no GPU and still run.
    #[test]
    fn a_host_without_a_gpu_refuses_theme_preview_before_the_ui_side() {
        let mut world = World::new();
        world.insert_resource(UiAgentHost::new(counting_preview_host));
        world.insert_resource(crate::seeing::GpuDisabled {
            code: "editor_headless".into(),
            reason: "the headless editor has no GPU adapter (started with --no-gpu)".into(),
            hint: "start it without --no-gpu".into(),
        });
        let before = PREVIEWS_ASKED.with(std::cell::Cell::get);
        let error = dispatch_theme(&mut world, "ui.theme_preview", &json!({ "page": "buttons" }))
            .expect_err("refused");
        assert_eq!(error.code, protocol::UNSUPPORTED, "{error:?}");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("editor_headless"),
            "{error:?}"
        );
        assert_eq!(
            PREVIEWS_ASKED.with(std::cell::Cell::get),
            before,
            "the UI side was not asked"
        );
        assert!(
            world.get_resource::<regia_render::RenderDevice>().is_none(),
            "no device"
        );
        dispatch_theme(&mut world, "ui.theme_list", &json!({})).expect("needs no GPU");
    }

    #[test]
    fn world_click_fills_the_occluder_from_the_scene_pick() {
        use std::sync::Arc;

        use regia_core::glam::Affine3A;
        use regia_render::{CpuMesh, LabMaterial, MeshRenderer, box_mesh};
        use regia_scene::GlobalTransform;

        let mut world = World::new();
        world.insert_resource(UiAgentHost::new(echo));
        let (vertices, indices) = box_mesh([20.0, 0.1, 20.0]);
        world.spawn((
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(LabMaterial::default()),
            ),
            GlobalTransform(Affine3A::from_translation(regia_core::math::Vec3::new(
                0.0, 4.9, 0.0,
            ))),
        ));
        let ray = json!({ "origin": [0.0, 50.0, 0.0], "direction": [0.0, -1.0, 0.0] });
        let out = world_click(&mut world, &ray).expect("echo");
        assert_eq!(out["method"], "ui.world_click");
        let occluder = out["params"]["occluder_distance"].as_f64().expect("filled in");
        assert!((occluder - 45.0).abs() < 1e-3, "{occluder}");
        let mut off = ray.clone();
        off["occlusion"] = json!(false);
        let out = world_click(&mut world, &off).expect("echo");
        assert!(out["params"].get("occluder_distance").is_none());
        // A direct click and a non-object pass through untouched (no panic).
        let direct = world_click(&mut world, &json!({ "entity": 5 })).expect("echo");
        assert!(direct["params"].get("occluder_distance").is_none());
        assert_eq!(
            world_click(&mut world, &json!([1, 2])).expect("echo")["params"],
            json!([1, 2])
        );
    }
}
