// SPDX-License-Identifier: MPL-2.0
//! `audio.cues`: cue points stored in a WAV file (ADR-4632, `book/agents/audio.md`).
//!
//! This is not a sound cue. A sound cue picks a variation when it plays. This method reads the
//! RIFF `cue ` chunk and the `LIST` / `adtl` / `labl` labels. It does not play audio, it does
//! not decode PCM, and it does not need the audio plugin.

use regia_app::prelude::World;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// `audio.cues`: reads cue points from a project WAV file.
///
/// `path` is project-relative (no `..`, no symlink that leaves the project). The result is
/// `{ count, cues: [{ order, id, sample_offset, name }] }`. `name` is a string or null.
/// A file with no `cue ` chunk returns `count: 0`.
///
/// # Errors
///
/// `F0002` when `path` is missing, not a string, or escapes the project.
/// `F0003` when the file or project root cannot be resolved.
/// `wav_cue_unreadable` when the path is not a regular file or the read fails.
/// `wav_cue_not_riff`, `wav_cue_not_wave` and `wav_cue_truncated` come from the WAV reader.
pub fn cues(world: &World, params: &Value) -> Result<Value, RpcError> {
    let path = crate::params::opt_str(params, "path")?
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            RpcError::invalid_params("`path` is required and must be a project-relative WAV path")
        })?;
    let file = crate::audio::project_file(world, path)?;
    let bytes =
        crate::req_path::read_file_capped(&file, crate::req_path::MAX_READ_BYTES).map_err(|error| {
            RpcError::new(
                protocol::INVALID_PARAMS,
                "wav_cue_unreadable",
                format!("{path}: {error}"),
                "the path must be a regular file inside the project, at most 256 MiB",
            )
        })?;
    let found = regia_audio::wav_cues::read_wav_cues(&bytes).map_err(|error| {
        RpcError::new(
            protocol::INVALID_PARAMS,
            error.code(),
            error.to_string(),
            "the file must be a RIFF WAVE; a cue chunk must contain every point it counts",
        )
    })?;
    let rows = found
        .iter()
        .map(|cue| {
            json!({
                "order": cue.order,
                "id": cue.id,
                "sample_offset": cue.sample_offset,
                "name": cue.name,
            })
        })
        .collect::<Vec<_>>();
    let count = rows.len();
    Ok(json!({ "count": count, "cues": rows }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use regia_app::prelude::World;

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(id);
        push_u32(&mut out, u32::try_from(body.len()).expect("chunk size"));
        out.extend_from_slice(body);
        if !body.len().is_multiple_of(2) {
            out.push(0);
        }
        out
    }

    fn wave(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for part in parts {
            body.extend_from_slice(part);
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        let size = u32::try_from(4 + body.len()).expect("riff size");
        push_u32(&mut out, size);
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&body);
        out
    }

    fn cue_point(id: u32, position: u32, sample_offset: u32) -> [u8; 24] {
        let mut point = [0u8; 24];
        point[0..4].copy_from_slice(&id.to_le_bytes());
        point[4..8].copy_from_slice(&position.to_le_bytes());
        point[8..12].copy_from_slice(b"data");
        point[20..24].copy_from_slice(&sample_offset.to_le_bytes());
        point
    }

    fn cue_chunk(points: &[[u8; 24]]) -> Vec<u8> {
        let mut body = Vec::new();
        push_u32(&mut body, u32::try_from(points.len()).expect("count"));
        for point in points {
            body.extend_from_slice(point);
        }
        chunk(b"cue ", &body)
    }

    fn labl(id: u32, text: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        push_u32(&mut body, id);
        body.extend_from_slice(text);
        body.push(0);
        chunk(b"labl", &body)
    }

    fn labeled_wav() -> Vec<u8> {
        let mut adtl_body = b"adtl".to_vec();
        adtl_body.extend_from_slice(&labl(10, b"intro"));
        adtl_body.extend_from_slice(&labl(11, b"loop"));
        wave(&[
            cue_chunk(&[cue_point(10, 7, 100), cue_point(11, 8, 200)]),
            chunk(b"LIST", &adtl_body),
        ])
    }

    fn silent_wav() -> Vec<u8> {
        wave(&[chunk(b"data", &[0, 0, 0, 0])])
    }

    fn truncated_wav() -> Vec<u8> {
        let mut body = Vec::new();
        push_u32(&mut body, 2);
        body.extend_from_slice(&cue_point(1, 0, 10));
        wave(&[chunk(b"cue ", &body)])
    }

    fn world_with(root: &std::path::Path) -> World {
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        let mut world = World::new();
        world.insert_resource(server);
        world
    }

    fn write_wav(root: &std::path::Path, name: &str, bytes: &[u8]) {
        regia_test_utils::write_atomic(&root.join(name), bytes).expect("write wav");
    }

    fn code_of(error: &RpcError) -> &str {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
            .expect("regia code")
    }

    #[test]
    fn audio_cues_is_a_read_only_schema_method() {
        let method = crate::schema::method("audio.cues").expect("schema");
        assert_eq!(method.tool, "audio_cues");
        assert!(!method.mutating);
    }

    #[test]
    fn two_points_and_two_labels_round_trip_from_a_temp_file() {
        let root = regia_test_utils::unique_temp_dir("wav-cues");
        write_wav(&root, "marks.wav", &labeled_wav());
        let world = world_with(&root);
        let value = cues(&world, &json!({ "path": "marks.wav" })).expect("cues");
        assert_eq!(value["count"], 2);
        let rows = value["cues"].as_array().expect("rows");
        assert_eq!(rows[0]["order"], 0);
        assert_eq!(rows[0]["id"], 10);
        assert_eq!(rows[0]["sample_offset"], 100);
        assert_eq!(rows[0]["name"], "intro");
        assert_eq!(rows[1]["order"], 1);
        assert_eq!(rows[1]["id"], 11);
        assert_eq!(rows[1]["sample_offset"], 200);
        assert_eq!(rows[1]["name"], "loop");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_wav_with_no_cue_chunk_returns_an_empty_list() {
        let root = regia_test_utils::unique_temp_dir("wav-cues");
        write_wav(&root, "plain.wav", &silent_wav());
        let world = world_with(&root);
        let value = cues(&world, &json!({ "path": "plain.wav" })).expect("empty");
        assert_eq!(value["count"], 0);
        assert_eq!(value["cues"].as_array().expect("rows").len(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_truncated_cue_chunk_returns_wav_cue_truncated() {
        let root = regia_test_utils::unique_temp_dir("wav-cues");
        write_wav(&root, "cut.wav", &truncated_wav());
        let world = world_with(&root);
        let error = cues(&world, &json!({ "path": "cut.wav" })).expect_err("truncated");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert_eq!(code_of(&error), "wav_cue_truncated");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_file_that_is_not_riff_returns_wav_cue_not_riff() {
        let root = regia_test_utils::unique_temp_dir("wav-cues");
        write_wav(&root, "notes.bin", b"not a riff file");
        let world = world_with(&root);
        let error = cues(&world, &json!({ "path": "notes.bin" })).expect_err("not riff");
        assert_eq!(code_of(&error), "wav_cue_not_riff");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_file_is_not_found_and_a_directory_is_unreadable() {
        let root = regia_test_utils::unique_temp_dir("wav-cues");
        std::fs::create_dir(root.join("folder")).expect("dir");
        let world = world_with(&root);
        let missing = cues(&world, &json!({ "path": "missing.wav" })).expect_err("missing");
        assert_eq!(code_of(&missing), "F0003");
        let directory = cues(&world, &json!({ "path": "folder" })).expect_err("dir");
        assert_eq!(code_of(&directory), "wav_cue_unreadable");
        let absent = cues(&world, &json!({})).expect_err("no path");
        assert_eq!(code_of(&absent), "F0002");
        let _ = std::fs::remove_dir_all(root);
    }
}
