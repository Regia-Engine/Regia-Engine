// SPDX-License-Identifier: MPL-2.0
//! `assets.push`: the editor sends a changed asset into a game it launched, and the game swaps it
//! in place (P-301, ADR-7836, `book/agents/asset-push.md`).
//!
//! One of three targets per call, with the bytes as standard base64 in `data`:
//!
//! - `asset`: a cooked asset by id (or manifest path). The bytes become the id's `.fckd` file in
//!   the game's cooked cache (the path is the id's file name, never a caller's path) and the id is
//!   hot-reloaded ([`regia_asset::apply_reload_messages`]): handles stay valid, consumers get
//!   `AssetReloaded`. A pushed id is served from the cache over any mounted pak from then on.
//! - `source`: a source file the runtime reads directly (a UI document), by a project-relative path
//!   under `assets/`; the game receives `SourceAssetChanged` for it.
//! - `shader`: an engine shader by file name (`pbr_lighting.wgsl`); the text is validated and
//!   staged as a shader edit (`regia_render::shader_reload::stage_shader_edit`), nothing is written.
//!
//! The decoded size is capped at [`regia_asset::push::PUSH_MAX_BYTES`]. The method is in the
//! Assets permission class.

use regia_app::prelude::*;
use regia_asset::push::{PUSH_MAX_BYTES, PushError, PushOutcome};
use regia_asset::{AssetId, AssetServer, ReloadMessage};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// The method's error codes (lowercase ascii, listed on `book/agents/asset-push.md`).
pub mod codes {
    /// Not exactly one of `asset`, `source` and `shader` was given.
    pub const NO_TARGET: &str = "push_no_target";
    /// `data` is missing or not standard base64 (or a shader's text is not UTF-8).
    pub const BAD_DATA: &str = "push_bad_data";
    /// The decoded bytes are over the push limit (16 MiB).
    pub const TOO_LARGE: &str = "push_too_large";
    /// `source` leaves `assets/`, or `shader` is not a plain `.wgsl` file name.
    pub const BAD_PATH: &str = "push_bad_path";
    /// `asset` is neither an asset id nor a path in the game's manifest.
    pub const UNKNOWN_ASSET: &str = "push_unknown_asset";
    /// The game has no asset server (an `asset` or `source` push).
    pub const NO_SERVER: &str = "push_no_server";
    /// Writing the file failed.
    pub const IO: &str = "push_io";
    /// The shader edit did not validate; the previous shaders stay.
    pub const SHADER_INVALID: &str = "push_shader_invalid";

    /// Every code, for the book page's test.
    #[cfg(test)]
    pub const ALL: &[&str] = &[
        NO_TARGET,
        BAD_DATA,
        TOO_LARGE,
        BAD_PATH,
        UNKNOWN_ASSET,
        NO_SERVER,
        IO,
        SHADER_INVALID,
    ];
}

fn err(code: i64, regia_code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(code, regia_code, message, hint)
}

fn push_error(e: &PushError) -> RpcError {
    match e {
        PushError::TooLarge(_) => err(
            protocol::INVALID_PARAMS,
            codes::TOO_LARGE,
            e.to_string(),
            "push assets of at most 16 MiB; package the game for larger ones",
        ),
        PushError::BadPath(_) => err(
            protocol::INVALID_PARAMS,
            codes::BAD_PATH,
            e.to_string(),
            "pass a project-relative path under assets/ with no `.` or `..` parts",
        ),
        PushError::Io(_) => err(
            protocol::INTERNAL_ERROR,
            codes::IO,
            e.to_string(),
            "check the game's disk space and the permissions of its project directory",
        ),
    }
}

/// The decoded `data` parameter, capped before and after decoding.
fn data(params: &Value) -> Result<Vec<u8>, RpcError> {
    let Some(text) = params.get("data").and_then(Value::as_str) else {
        return Err(err(
            protocol::INVALID_PARAMS,
            codes::BAD_DATA,
            "`data` (the bytes as standard base64) is required",
            "encode the file with standard base64 (RFC 4648 § 4)",
        ));
    };
    // Four base64 characters per three bytes: refuse an oversized text before decoding it.
    if text.len() / 4 * 3 > PUSH_MAX_BYTES + 3 {
        return Err(push_error(&PushError::TooLarge(text.len() / 4 * 3)));
    }
    let bytes = crate::image::base64_decode(text).map_err(|e| {
        err(
            protocol::INVALID_PARAMS,
            codes::BAD_DATA,
            format!("`data` is not standard base64: {e}"),
            "encode the file with standard base64 (RFC 4648 § 4)",
        )
    })?;
    if bytes.len() > PUSH_MAX_BYTES {
        return Err(push_error(&PushError::TooLarge(bytes.len())));
    }
    Ok(bytes)
}

fn opt_target<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.as_str())),
        Some(_) => Err(RpcError::wrong_type(name, "a non-empty string")),
    }
}

fn no_server() -> RpcError {
    err(
        protocol::UNSUPPORTED,
        codes::NO_SERVER,
        "the game has no asset server",
        "add the asset plugin (DefaultPlugins has it)",
    )
}

/// `id` from an id text or a manifest path (`assets/…` or relative to `assets/`).
fn resolve_id(server: &AssetServer, asset: &str) -> Result<AssetId, RpcError> {
    if let Ok(id) = asset.parse::<AssetId>() {
        return Ok(id);
    }
    let path = asset.replace('\\', "/");
    let path = path.strip_prefix("assets/").unwrap_or(&path);
    server.id_for_path(&format!("assets/{path}")).ok_or_else(|| {
        err(
            protocol::NOT_FOUND,
            codes::UNKNOWN_ASSET,
            format!("{asset:?} is not an asset id and not in the game's manifest"),
            "push by asset id (the cooked file's name without .fckd)",
        )
    })
}

fn outcome_json(kind: &str, target: &str, outcome: &PushOutcome) -> Value {
    json!({
        "kind": kind,
        "target": target,
        "bytes": outcome.bytes,
        "changed": outcome.changed,
        "path": outcome.path.display().to_string(),
    })
}

/// A plain engine shader file name: ASCII letters, digits, `_`, `-` and `.`, ending `.wgsl`.
fn shader_name_ok(name: &str) -> bool {
    name.len() <= 128
        && name.ends_with(".wgsl")
        && name.len() > ".wgsl".len()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// `assets.push { asset | source | shader, data }`.
pub(crate) fn push(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_agent", "assets_push").entered();
    let asset = opt_target(params, "asset")?;
    let source = opt_target(params, "source")?;
    let shader = opt_target(params, "shader")?;
    let given = [asset.is_some(), source.is_some(), shader.is_some()]
        .into_iter()
        .filter(|g| *g)
        .count();
    if given != 1 {
        return Err(err(
            protocol::INVALID_PARAMS,
            codes::NO_TARGET,
            "pass exactly one of `asset`, `source` and `shader`",
            "asset: a cooked asset id; source: a path under assets/; shader: an engine .wgsl name",
        ));
    }
    let bytes = data(params)?;

    if let Some(name) = shader {
        if !shader_name_ok(name) {
            return Err(push_error(&PushError::BadPath(format!(
                "shader {name:?} is not a plain .wgsl file name"
            ))));
        }
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            err(
                protocol::INVALID_PARAMS,
                codes::BAD_DATA,
                format!("shader text is not UTF-8: {e}"),
                "push the .wgsl file's bytes",
            )
        })?;
        let swap = regia_render::shader_reload::stage_shader_edit(name, text).map_err(|e| {
            err(
                protocol::INVALID_PARAMS,
                codes::SHADER_INVALID,
                format!("{name} did not validate: {e}"),
                "fix the shader; the previous shaders stay live",
            )
        })?;
        return Ok(json!({
            "kind": "shader",
            "target": name,
            "bytes": bytes.len(),
            "changed": !swap.superseded,
            "epoch": swap.epoch,
            "validated": swap.validated,
        }));
    }

    if let Some(rel) = source {
        let outcome = {
            let Some(server) = world.get_resource::<AssetServer>() else {
                return Err(no_server());
            };
            server
                .accept_source_push(rel, &bytes)
                .map_err(|e| push_error(&e))?
        };
        regia_asset::apply_reload_messages(world, vec![ReloadMessage::Source(outcome.path.clone())]);
        return Ok(outcome_json("source", rel, &outcome));
    }

    let Some(asset) = asset else {
        return Err(err(
            protocol::INVALID_PARAMS,
            codes::NO_TARGET,
            "pass exactly one of `asset`, `source` and `shader`",
            "asset: a cooked asset id",
        ));
    };
    let (id, outcome) = {
        let Some(mut server) = world.get_resource_mut::<AssetServer>() else {
            return Err(no_server());
        };
        let id = resolve_id(&server, asset)?;
        let outcome = server.accept_push(id, &bytes).map_err(|e| push_error(&e))?;
        (id, outcome)
    };
    regia_asset::apply_reload_messages(world, vec![ReloadMessage::Reload(id)]);
    let mut out = outcome_json("asset", &id.to_string(), &outcome);
    if let (Value::Object(map), Some(server)) = (&mut out, world.get_resource::<AssetServer>()) {
        map.insert("loaded".into(), json!(server.is_loaded(id)));
        map.insert("generation".into(), json!(server.generation(id)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(world: &mut World, params: Value) -> Result<Value, RpcError> {
        crate::dispatch::call_in_process(world, "assets.push", params)
            .unwrap_or_else(|| panic!("assets.push must answer synchronously"))
    }

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn world_with_server(root: &std::path::Path) -> World {
        let mut world = World::new();
        let cache = regia_asset::cooked_cache_dir(root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let server = AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("server");
        world.insert_resource(server);
        world
    }

    /// The bytes of a cooked 1x1 image with `rgba`.
    fn cooked_image(id: AssetId, rgba: [u8; 4]) -> Vec<u8> {
        let payload = regia_asset::CookedImage {
            width: 1,
            height: 1,
            rgba8: rgba.to_vec(),
        }
        .to_postcard()
        .expect("postcard");
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_asset::AssetTypeTag::image().0,
            asset_id: id,
            importer_version: regia_asset::pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        regia_asset::encode_cooked_file(&header, &payload).expect("encode")
    }

    #[test]
    fn assets_push_writes_the_cooked_file_and_hot_reloads_the_loaded_asset() {
        let tmp = regia_test_utils::unique_temp_dir("agent-assets-push");
        let mut world = world_with_server(&tmp);
        let id = AssetId::new_random();
        let cache = regia_asset::cooked_cache_dir(&tmp, "linux");
        std::fs::write(
            cache.join(regia_asset::cooked_file_name(id)),
            cooked_image(id, [1, 1, 1, 1]),
        )
        .expect("first cook");
        let handle = world.resource_mut::<AssetServer>().load_image(id).expect("load");

        let pushed = cooked_image(id, [7, 7, 7, 7]);
        let out = call(
            &mut world,
            json!({ "asset": id.to_string(), "data": crate::image::base64(&pushed) }),
        )
        .unwrap_or_else(|e| panic!("push: {e:?}"));
        assert_eq!(out["kind"], "asset");
        assert_eq!(out["changed"], true);
        assert_eq!(out["loaded"], true);
        assert_eq!(out["generation"], 1, "the push hot-reloaded the asset once");
        let server = world.resource::<AssetServer>();
        assert_eq!(
            server.images().get(&handle).map(|i| i.rgba8.clone()),
            Some(vec![7, 7, 7, 7]),
            "the live handle serves the pushed bytes"
        );
        assert!(server.is_pushed(id));

        // The same bytes again: nothing is written, the asset is still reloaded.
        let again = call(
            &mut world,
            json!({ "asset": id.to_string(), "data": crate::image::base64(&pushed) }),
        )
        .unwrap_or_else(|e| panic!("again: {e:?}"));
        assert_eq!(again["changed"], false);
        assert_eq!(again["generation"], 2);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn assets_push_source_files_stay_under_assets_and_raise_source_changed() {
        let tmp = regia_test_utils::unique_temp_dir("agent-assets-push-source");
        let mut world = world_with_server(&tmp);
        world.init_resource::<Messages<regia_asset::SourceAssetChanged>>();
        let out = call(
            &mut world,
            json!({ "source": "assets/ui/hud.ui.ron", "data": crate::image::base64(b"(root: ())") }),
        )
        .unwrap_or_else(|e| panic!("source push: {e:?}"));
        assert_eq!(out["kind"], "source");
        let written = tmp.join("assets").join("ui").join("hud.ui.ron");
        assert_eq!(std::fs::read(&written).expect("written"), b"(root: ())");
        let messages = world.resource::<Messages<regia_asset::SourceAssetChanged>>();
        let mut cursor = messages.get_cursor();
        let changed: Vec<_> = cursor.read(messages).cloned().collect();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].kind, regia_asset::SourceKind::UiLayout);

        for path in ["assets/../Regia.toml", "../outside.txt", "/etc/x", "assets"] {
            let e = call(&mut world, json!({ "source": path, "data": "eA==" })).expect_err(path);
            assert_eq!(code(&e), codes::BAD_PATH, "{path}");
        }
        assert!(!tmp.join("Regia.toml").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn assets_push_refusals_answer_their_codes() {
        let tmp = regia_test_utils::unique_temp_dir("agent-assets-push-refusals");
        let mut world = world_with_server(&tmp);
        let id = AssetId::new_random().to_string();
        let cases = [
            (json!({ "data": "eA==" }), codes::NO_TARGET),
            (
                json!({ "asset": id, "source": "assets/a", "data": "eA==" }),
                codes::NO_TARGET,
            ),
            (json!({ "asset": id, "data": "not base64!" }), codes::BAD_DATA),
            (
                json!({ "asset": "meshes/none.mesh", "data": "eA==" }),
                codes::UNKNOWN_ASSET,
            ),
            (json!({ "shader": "../x.wgsl", "data": "eA==" }), codes::BAD_PATH),
            (json!({ "shader": "x.txt", "data": "eA==" }), codes::BAD_PATH),
            (
                json!({ "shader": "x.wgsl", "data": crate::image::base64(&[0xff, 0xfe]) }),
                codes::BAD_DATA,
            ),
        ];
        for (params, want) in cases {
            let e = call(&mut world, params.clone()).expect_err("refused");
            assert_eq!(code(&e), want, "{params}");
        }
        // The contract's longest `data` (22_369_624 characters) decodes to 2 bytes over the cap.
        let huge = "A".repeat(22_369_624);
        let e = call(&mut world, json!({ "asset": id, "data": huge })).expect_err("too large");
        assert_eq!(code(&e), codes::TOO_LARGE);
        let mut bare = World::new();
        let e = call(&mut bare, json!({ "asset": id, "data": "eA==" })).expect_err("no server");
        assert_eq!(code(&e), codes::NO_SERVER);
        let page = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../book/agents/asset-push.md"),
        )
        .expect("book/agents/asset-push.md");
        for c in codes::ALL {
            assert!(page.contains(&format!("`{c}`")), "{c} is listed on the book page");
            assert!(
                c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{c}"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
