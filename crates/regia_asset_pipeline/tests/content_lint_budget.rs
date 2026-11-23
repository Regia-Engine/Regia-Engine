// SPDX-License-Identifier: MPL-2.0
//! CT-01: metadata inspection must not read or allocate the binary tail.
use std::io::{Seek, SeekFrom, Write};

use regia_asset_pipeline::gltf_material_lint::{
    MaterialLintProfile, MaterialLintSettings, lint_material_file,
};
use regia_test_utils::unique_temp_dir;

/// ADR-8250: a bare material is rewritten only by the compatibility profile.
/// These budget tests still assert the rewrite path, so they select that profile.
fn rewriting() -> MaterialLintSettings {
    MaterialLintSettings {
        profile: MaterialLintProfile::Compatibility,
        ..Default::default()
    }
}

#[test]
fn large_glb_small_json_is_checked_without_a_whole_file_cap() {
    let dir = unique_temp_dir("content-lint-budget");
    let path = dir.join("large.glb");
    let pending = dir.join("pending");
    let mut json = br#"{"asset":{"version":"2.0"},"materials":[]}"#.to_vec();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let tail = 80 * 1024 * 1024_u32;
    let length = 28 + json.len() as u32 + tail;
    let mut file = std::fs::File::create(&pending).expect("fixture");
    file.write_all(b"glTF").expect("magic");
    file.write_all(&2_u32.to_le_bytes()).expect("version");
    file.write_all(&length.to_le_bytes()).expect("length");
    file.write_all(&(json.len() as u32).to_le_bytes())
        .expect("json length");
    file.write_all(b"JSON").expect("json tag");
    file.write_all(&json).expect("json");
    file.write_all(&tail.to_le_bytes()).expect("bin length");
    file.write_all(b"BIN\0").expect("bin tag");
    file.seek(SeekFrom::Start(u64::from(length - 1)))
        .expect("sparse seek");
    file.write_all(&[0]).expect("tail");
    drop(file);
    std::fs::rename(pending, &path).expect("publish");
    let result = lint_material_file(&path, &MaterialLintSettings::default(), false);
    std::fs::remove_dir_all(dir).expect("cleanup");
    assert!(result.is_ok(), "metadata-only lint: {result:?}");
}

use regia_asset_pipeline::content_lint::{ContentBudget, ContentLimits};
use regia_asset_pipeline::open_gltf_bounded;
use serde_json::json;

fn publish(path: &std::path::Path, bytes: &[u8]) {
    let temporary = path.with_extension("pending");
    std::fs::write(&temporary, bytes).expect("fixture");
    std::fs::rename(temporary, path).expect("publish");
}

#[test]
fn sparse_accessor_is_refused_before_buffer_allocation() {
    let dir = unique_temp_dir("content-sparse-budget");
    let path = dir.join("model.gltf");
    publish(&path, &serde_json::to_vec(&json!({
        "asset":{"version":"2.0"},
        "buffers":[{"byteLength":16,"uri":"data:application/octet-stream;base64,AAAAAAAAAAAAAAAAAAAAAA=="}],
        "bufferViews":[{"buffer":0,"byteLength":4},{"buffer":0,"byteOffset":4,"byteLength":12}],
        "extensionsUsed":["KHR_mesh_quantization"], "extensionsRequired":["KHR_mesh_quantization"],
        "accessors":[{"componentType":5122,"normalized":true,"type":"VEC3","count":100000,"min":[0,0,0],"max":[0,0,0],
            "sparse":{"count":1,"indices":{"bufferView":0,"componentType":5121},"values":{"bufferView":1}}}],
        "meshes":[{"primitives":[{"attributes":{"POSITION":0},"mode":0}]}]
    })).expect("json"));
    // Establish that this valid sparse/quantized fixture actually expands in
    // the legacy loader, rather than merely declaring an unused large accessor.
    let (_, legacy_buffers) = regia_asset_pipeline::open_gltf_checked(&path).expect("legacy expansion");
    assert!(legacy_buffers.iter().any(|buffer| buffer.0.len() >= 1_200_000));
    drop(legacy_buffers);
    let limits = ContentLimits {
        max_decoded_bytes: 128,
        ..Default::default()
    };
    let mut budget = ContentBudget::new(limits, &|| false);
    let error = open_gltf_bounded(&path, &mut budget).expect_err("must refuse expansion");
    assert_eq!(error.code(), "content_lint_limit");
    assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    assert!(budget.usage().decoded_bytes <= 128);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn external_buffers_share_one_aggregate_limit() {
    let dir = unique_temp_dir("content-external-budget");
    let path = dir.join("model.gltf");
    for i in 0..4 {
        publish(&dir.join(format!("{i}.bin")), &[0; 16]);
    }
    publish(
        &path,
        &serde_json::to_vec(&json!({
            "asset":{"version":"2.0"},
            "buffers": (0..4).map(|i| json!({"byteLength":16,"uri":format!("{i}.bin")})).collect::<Vec<_>>()
        }))
        .expect("json"),
    );
    let limits = ContentLimits {
        max_external_bytes: 48,
        ..Default::default()
    };
    let mut budget = ContentBudget::new(limits, &|| false);
    let error = open_gltf_bounded(&path, &mut budget).expect_err("aggregate limit");
    assert_eq!(error.code(), "content_lint_limit");
    assert_eq!(budget.usage().external_bytes, 48);
    assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn cancellation_precedes_all_io() {
    let mut budget = ContentBudget::new(ContentLimits::default(), &|| true);
    let error = open_gltf_bounded(std::path::Path::new("unopened.gltf"), &mut budget).expect_err("cancelled");
    assert_eq!(error.code(), "content_lint_cancelled");
    assert_eq!(budget.usage().encoded_bytes, 0);
    assert_eq!(budget.usage().buffer_bytes_allocated, 0);
}

#[test]
fn accessor_count_is_charged_before_building_a_document() {
    let dir = unique_temp_dir("content-accessor-count");
    let path = dir.join("model.gltf");
    publish(&path, br#"{"asset":{"version":"2.0"},"accessors":[{},{},{}]}"#);
    let limits = ContentLimits {
        max_accessors: 2,
        ..Default::default()
    };
    let mut budget = ContentBudget::new(limits, &|| false);
    let error = open_gltf_bounded(&path, &mut budget).expect_err("count limit before validation");
    assert_eq!(error.code(), "content_lint_limit");
    assert_eq!(budget.usage().accessors, 2);
    assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn changed_external_file_is_explicit_not_a_partial_success() {
    use std::cell::Cell;
    let dir = unique_temp_dir("content-changed-external");
    let path = dir.join("model.gltf");
    let external = dir.join("buffer.bin");
    publish(&external, &[0; 128 * 1024]);
    publish(
        &path,
        br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"buffer.bin","byteLength":131072}]}"#,
    );
    // First count the deterministic checkpoints; then replace the external file
    // at each checkpoint. At least the checkpoints after open must detect it.
    let calls = Cell::new(0_usize);
    let probe = || {
        calls.set(calls.get() + 1);
        false
    };
    let mut budget = ContentBudget::new(ContentLimits::default(), &probe);
    open_gltf_bounded(&path, &mut budget).expect("baseline");
    let mut detected = 0;
    for change_at in 1..=calls.get() {
        publish(&external, &[0; 128 * 1024]);
        let step = Cell::new(0_usize);
        let replace = || {
            step.set(step.get() + 1);
            if step.get() == change_at {
                publish(&external, &[1; 128 * 1024]);
            }
            false
        };
        let mut budget = ContentBudget::new(ContentLimits::default(), &replace);
        if let Err(error) = open_gltf_bounded(&path, &mut budget) {
            assert_eq!(error.code(), "content_lint_changed", "{error}");
            detected += 1;
        }
    }
    assert!(
        detected >= 3,
        "replacement must be detected across read/normalization checkpoints"
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn every_checkpoint_can_cancel_without_exceeding_limits() {
    use std::cell::Cell;
    let dir = unique_temp_dir("content-cancel-checkpoints");
    let path = dir.join("model.glb");
    publish(&path, &regia_asset_pipeline::fuzz_entry::glb_fixture());
    let calls = Cell::new(0_usize);
    let probe = || {
        calls.set(calls.get() + 1);
        false
    };
    let mut budget = ContentBudget::new(ContentLimits::default(), &probe);
    open_gltf_bounded(&path, &mut budget).expect("baseline");
    for cancel_at in 1..=calls.get() {
        let step = Cell::new(0_usize);
        let cancel = || {
            step.set(step.get() + 1);
            step.get() == cancel_at
        };
        let mut budget = ContentBudget::new(ContentLimits::default(), &cancel);
        let error = open_gltf_bounded(&path, &mut budget).expect_err("cancel at every boundary");
        assert_eq!(error.code(), "content_lint_cancelled");
        assert!(budget.usage().decoded_bytes <= budget.limits().max_decoded_bytes);
        assert!(budget.usage().work <= budget.limits().max_work);
    }
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn bounded_fuzz_arbitrary_chunks_and_uri_strings() {
    use regia_asset_pipeline::fuzz_entry::{bounded_gltf_load, glb_fixture};
    let seed = glb_fixture();
    bounded_gltf_load(&seed).expect("seed");
    let extended = regia_asset_pipeline::fuzz_entry::gltf_ext_fixture();
    bounded_gltf_load(&extended).expect("compressed/normalized/instanced seed");
    for i in 0..extended.len() {
        let mut input = extended.clone();
        input[i] ^= 255;
        let _ = bounded_gltf_load(&input);
    }
    // Deterministic chunk lengths, truncations and byte mutations. The harness
    // asserts resource counters for every successful and rejected input.
    for end in 0..seed.len() {
        let _ = bounded_gltf_load(&seed[..end]);
    }
    for i in 0..seed.len() {
        let mut input = seed.clone();
        input[i] ^= 255;
        let _ = bounded_gltf_load(&input);
    }
    // Valid base64 receives the checked loader's zero padding; it is not a
    // malformed-URI case even when byteLength includes that padding.
    bounded_gltf_load(
        br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"data:;base64,AAAA","byteLength":4}]}"#,
    )
    .expect("valid padded data URI");
    for uri in [
        "../escape.bin",
        "%2e%2e/escape.bin",
        "%ff",
        "https://host/a",
        "C:/a",
        "a\\b",
        "data:;base64,!!!!",
        "data:;base64,AAA!",
        "missing.bin",
        "a%00b",
    ] {
        let bytes =
            serde_json::to_vec(&json!({"asset":{"version":"2.0"},"buffers":[{"uri":uri,"byteLength":4}]}))
                .expect("json");
        assert!(bounded_gltf_load(&bytes).is_err(), "{uri}");
    }
}

#[test]
fn geometry_and_material_reports_expose_the_effective_limits() {
    use regia_asset_pipeline::gltf_lint::{GltfLintSettings, lint_gltf};
    let dir = unique_temp_dir("content-lint-effective");
    let path = dir.join("model.glb");
    publish(&path, &regia_asset_pipeline::fuzz_entry::glb_fixture());
    let settings = GltfLintSettings {
        input_limits: ContentLimits {
            max_json_bytes: 4096,
            ..Default::default()
        },
        ..Default::default()
    };
    let geometry = lint_gltf(&path, &settings).expect("geometry");
    assert_eq!(geometry.input_limits, settings.input_limits);
    assert!(geometry.input_usage.buffer_bytes_allocated > 0);
    let material = lint_material_file(
        &path,
        &MaterialLintSettings {
            input_limits: settings.input_limits.clone(),
            ..Default::default()
        },
        false,
    )
    .expect("material");
    assert_eq!(material.input_limits, settings.input_limits);
    assert_eq!(material.input_usage.buffer_bytes_allocated, 0);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn normalized_integer_accessor_is_admitted_before_allocation() {
    let dir = unique_temp_dir("content-normalized-budget");
    let path = dir.join("model.gltf");
    publish(&path, &serde_json::to_vec(&json!({
        "asset":{"version":"2.0"}, "extensionsUsed":["KHR_mesh_quantization"],
        "extensionsRequired":["KHR_mesh_quantization"],
        "buffers":[{"byteLength":12,"uri":"missing.bin"}],
        "bufferViews":[{"buffer":0,"byteLength":12}],
        "accessors":[{"bufferView":0,"componentType":5122,"normalized":true,"type":"VEC3","count":2,"min":[0,0,0],"max":[1,1,1]}],
        "meshes":[{"primitives":[{"attributes":{"POSITION":0},"mode":0}]}]
    })).expect("json"));
    let mut budget = ContentBudget::new(
        ContentLimits {
            max_decoded_bytes: 16,
            ..Default::default()
        },
        &|| false,
    );
    let error = open_gltf_bounded(&path, &mut budget)
        .expect_err("expanded floats exceed budget before missing file IO");
    assert_eq!(error.code(), "content_lint_limit");
    assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    assert_eq!(budget.usage().external_bytes, 0);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn encoded_bytes_and_work_are_enforced_before_buffer_allocation() {
    let dir = unique_temp_dir("content-encoded-work");
    let path = dir.join("model.glb");
    publish(&path, &regia_asset_pipeline::fuzz_entry::glb_fixture());
    for limits in [
        ContentLimits {
            max_encoded_bytes: 20,
            ..Default::default()
        },
        ContentLimits {
            max_work: 1,
            ..Default::default()
        },
        ContentLimits {
            max_json_bytes: 16,
            ..Default::default()
        },
    ] {
        let mut budget = ContentBudget::new(limits, &|| false);
        let error = open_gltf_bounded(&path, &mut budget).expect_err("limit");
        assert_eq!(error.code(), "content_lint_limit");
        assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    }
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn material_metadata_budget_does_not_read_large_binary_tail() {
    use regia_asset_pipeline::gltf_material_lint::lint_material_file_bounded;
    let dir = unique_temp_dir("content-metadata-only");
    let path = dir.join("model.glb");
    let mut json =
        br#"{"asset":{"version":"2.0"},"images":[{"uri":"nonexistent.png"}],"materials":[]}"#.to_vec();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let length = 28 + json.len() as u32 + 128 * 1024 * 1024;
    let mut bytes = b"glTF".to_vec();
    bytes.extend(2_u32.to_le_bytes());
    bytes.extend(length.to_le_bytes());
    bytes.extend((json.len() as u32).to_le_bytes());
    bytes.extend(b"JSON");
    bytes.extend(&json);
    bytes.extend((128 * 1024 * 1024_u32).to_le_bytes());
    bytes.extend(b"BIN\0");
    let pending = dir.join("pending");
    let mut file = std::fs::File::create(&pending).expect("fixture");
    file.write_all(&bytes).expect("headers");
    file.set_len(u64::from(length)).expect("sparse tail");
    drop(file);
    std::fs::rename(pending, &path).expect("publish");
    let mut budget = ContentBudget::new(
        ContentLimits {
            max_encoded_bytes: 1024,
            max_json_bytes: 256,
            max_decoded_bytes: 0,
            max_external_bytes: 0,
            ..Default::default()
        },
        &|| false,
    );
    let report = lint_material_file_bounded(&path, &MaterialLintSettings::default(), false, &mut budget)
        .expect("metadata");
    assert_eq!(report.input_usage.encoded_bytes, 28 + json.len() as u64);
    assert_eq!(report.input_usage.buffer_bytes_allocated, 0);
    assert_eq!(report.input_usage.external_bytes, 0);
    assert_eq!(report.input_usage.copied_bytes, 0);
    // Physically truncate without changing the GLB header: explicit malformed input.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("fixture");
    file.set_len(u64::from(length - 4)).expect("truncate");
    drop(file);
    let error = lint_material_file(&path, &MaterialLintSettings::default(), false).expect_err("truncated");
    assert_eq!(error.code, "gltf_material_invalid");
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn material_fix_copy_limit_and_cancellation_leave_source_untouched() {
    use regia_asset_pipeline::gltf_material_lint::lint_material_file_bounded;
    use std::cell::Cell;
    let dir = unique_temp_dir("content-fix-bounds");
    let path = dir.join("model.gltf");
    let before = br#"{"asset":{"version":"2.0"},"materials":[{}]}"#;
    publish(&path, before);
    let mut budget = ContentBudget::new(
        ContentLimits {
            max_copy_bytes: 1,
            ..Default::default()
        },
        &|| false,
    );
    let error = lint_material_file_bounded(&path, &rewriting(), true, &mut budget).expect_err("copy limit");
    assert_eq!(error.code, "content_lint_limit");
    assert_eq!(std::fs::read(&path).expect("unchanged"), before);
    assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 1);
    let calls = Cell::new(0_usize);
    let probe = || {
        calls.set(calls.get() + 1);
        false
    };
    let mut budget = ContentBudget::new(ContentLimits::default(), &probe);
    lint_material_file_bounded(&path, &rewriting(), true, &mut budget).expect("baseline fix");
    for cancel_at in 1..=calls.get() {
        publish(&path, before);
        let step = Cell::new(0_usize);
        let cancel = || {
            step.set(step.get() + 1);
            step.get() == cancel_at
        };
        let mut budget = ContentBudget::new(ContentLimits::default(), &cancel);
        let error =
            lint_material_file_bounded(&path, &rewriting(), true, &mut budget).expect_err("cancelled fix");
        assert_eq!(error.code, "content_lint_cancelled");
        assert_eq!(std::fs::read(&path).expect("unchanged"), before);
        assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 1, "no temp debris");
    }
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn normalized_loader_preserves_checked_importer_results() {
    let dir = unique_temp_dir("content-normalize-parity");
    let path = dir.join("model.glb");
    for bytes in [
        regia_asset_pipeline::fuzz_entry::glb_fixture(),
        regia_asset_pipeline::fuzz_entry::gltf_ext_fixture(),
    ] {
        publish(&path, &bytes);
        let legacy = regia_asset_pipeline::open_gltf_checked(&path).expect("legacy");
        let mut budget = ContentBudget::new(ContentLimits::default(), &|| false);
        let bounded = open_gltf_bounded(&path, &mut budget).expect("bounded");
        assert_eq!(
            serde_json::to_value(legacy.0.as_json()).expect("json"),
            serde_json::to_value(bounded.0.as_json()).expect("json")
        );
        assert_eq!(
            legacy.1.iter().map(|b| &b.0).collect::<Vec<_>>(),
            bounded.1.iter().map(|b| &b.0).collect::<Vec<_>>()
        );
        assert!(budget.usage().decoded_bytes >= bounded.1.iter().map(|b| b.0.len() as u64).sum::<u64>());
    }
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn changed_external_truncation_has_changed_code() {
    use std::cell::Cell;
    let dir = unique_temp_dir("content-truncated-external");
    let path = dir.join("model.gltf");
    let external = dir.join("buffer.bin");
    publish(
        &path,
        br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"buffer.bin","byteLength":131072}]}"#,
    );
    publish(&external, &[0; 128 * 1024]);
    let calls = Cell::new(0_usize);
    let probe = || {
        calls.set(calls.get() + 1);
        false
    };
    let mut budget = ContentBudget::new(ContentLimits::default(), &probe);
    open_gltf_bounded(&path, &mut budget).expect("baseline");
    let mut changed = 0;
    // Truncate only after the file has opened (the final read/normalize probes).
    for truncate_at in calls.get().saturating_sub(5)..=calls.get() {
        publish(&external, &[0; 128 * 1024]);
        let step = Cell::new(0_usize);
        let truncate = || {
            step.set(step.get() + 1);
            if step.get() == truncate_at {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&external)
                    .expect("fixture")
                    .set_len(0)
                    .expect("truncate");
            }
            false
        };
        let mut budget = ContentBudget::new(ContentLimits::default(), &truncate);
        let error = open_gltf_bounded(&path, &mut budget).expect_err("source changed");
        assert_eq!(
            error.code(),
            "content_lint_changed",
            "checkpoint {truncate_at}: {error}"
        );
        changed += 1;
    }
    assert!(changed > 0);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn oversized_compressed_view_is_refused_before_decoder_allocation() {
    let dir = unique_temp_dir("content-meshopt-budget");
    let path = dir.join("model.gltf");
    publish(&path, &serde_json::to_vec(&json!({
        "asset":{"version":"2.0"}, "extensionsUsed":["EXT_meshopt_compression"],
        "extensionsRequired":["EXT_meshopt_compression"],
        "buffers":[{"byteLength":4,"uri":"missing.bin"},{"byteLength":1048576,"extensions":{"EXT_meshopt_compression":{"fallback":true}}}],
        "bufferViews":[{"buffer":1,"byteLength":1048576,"extensions":{"EXT_meshopt_compression":{"buffer":0,"byteLength":4,"byteStride":4,"count":262144,"mode":"ATTRIBUTES"}}}]
    })).expect("json"));
    let mut budget = ContentBudget::new(
        ContentLimits {
            max_decoded_bytes: 1024,
            ..Default::default()
        },
        &|| false,
    );
    let error = open_gltf_bounded(&path, &mut budget).expect_err("decoded meshopt bytes");
    assert_eq!(error.code(), "content_lint_limit");
    assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn external_trailing_bytes_are_included_in_the_aggregate() {
    let dir = unique_temp_dir("content-external-trailing");
    let path = dir.join("model.gltf");
    publish(&dir.join("buffer.bin"), &[0; 4096]);
    publish(
        &path,
        br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"buffer.bin","byteLength":4}]}"#,
    );
    let mut budget = ContentBudget::new(
        ContentLimits {
            max_external_bytes: 32,
            ..Default::default()
        },
        &|| false,
    );
    let error = open_gltf_bounded(&path, &mut budget).expect_err("actual size budgeted");
    assert_eq!(error.code(), "content_lint_limit");
    assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn material_fix_preserves_large_binary_tail_with_bounded_blocks() {
    use regia_asset_pipeline::gltf_material_lint::lint_material_file_bounded;
    let dir = unique_temp_dir("content-stream-fix");
    let path = dir.join("model.glb");
    let mut json = br#"{"asset":{"version":"2.0"},"materials":[{}]}"#.to_vec();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let bin = vec![173; 192 * 1024];
    let mut bytes = b"glTF".to_vec();
    bytes.extend(2_u32.to_le_bytes());
    bytes.extend(((28 + json.len() + bin.len()) as u32).to_le_bytes());
    bytes.extend((json.len() as u32).to_le_bytes());
    bytes.extend(b"JSON");
    bytes.extend(&json);
    bytes.extend((bin.len() as u32).to_le_bytes());
    bytes.extend(b"BIN\0");
    bytes.extend(&bin);
    publish(&path, &bytes);
    let mut budget = ContentBudget::new(
        ContentLimits {
            max_encoded_bytes: 1024,
            max_json_bytes: 1024,
            max_decoded_bytes: 0,
            ..Default::default()
        },
        &|| false,
    );
    let report = lint_material_file_bounded(&path, &rewriting(), true, &mut budget).expect("stream fix");
    assert!(report.changed);
    assert_eq!(report.input_usage.buffer_bytes_allocated, 0);
    assert_eq!(report.input_usage.encoded_bytes, 28 + json.len() as u64);
    let after = std::fs::read(&path).expect("output");
    assert_eq!(&after[after.len() - bin.len()..], &bin);
    assert_eq!(report.input_usage.copied_bytes, after.len() as u64);
    assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 1);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn material_json_output_limit_is_explicit_and_atomic() {
    use regia_asset_pipeline::gltf_material_lint::lint_material_file_bounded;
    let dir = unique_temp_dir("content-output-json-limit");
    let path = dir.join("model.gltf");
    let before = br#"{"asset":{"version":"2.0"},"materials":[{}]}"#;
    publish(&path, before);
    let mut budget = ContentBudget::new(
        ContentLimits {
            max_json_bytes: before.len() as u64,
            ..Default::default()
        },
        &|| false,
    );
    let error = lint_material_file_bounded(&path, &rewriting(), true, &mut budget)
        .expect_err("pretty repaired JSON exceeds source-sized limit");
    assert_eq!(error.code, "content_lint_limit");
    assert!(error.message.contains("max_json_bytes"));
    assert_eq!(std::fs::read(&path).expect("unchanged"), before);
    assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 1);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn path_escape_and_malformed_accessors_remain_rejected() {
    let dir = unique_temp_dir("content-path-sandbox");
    let path = dir.join("model.gltf");
    for uri in [
        "../escape.bin",
        "%2e%2e/escape.bin",
        "%ff",
        "C:/escape.bin",
        "a\\b",
    ] {
        publish(
            &path,
            &serde_json::to_vec(&json!({"asset":{"version":"2.0"},"buffers":[{"uri":uri,"byteLength":4}]}))
                .expect("json"),
        );
        let mut budget = ContentBudget::new(ContentLimits::default(), &|| false);
        let error = open_gltf_bounded(&path, &mut budget).expect_err("sandbox");
        assert_eq!(error.code(), "gltf_unreadable");
        assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    }
    publish(&path, br#"{"asset":{"version":"2.0"},"accessors":[{"componentType":5126,"type":"VEC3","count":3,"bufferView":999}]}"#);
    let mut budget = ContentBudget::new(ContentLimits::default(), &|| false);
    let error = open_gltf_bounded(&path, &mut budget).expect_err("malformed accessor");
    assert_eq!(error.code(), "gltf_unreadable");
    assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn existing_outside_buffers_and_symlink_escapes_are_never_read() {
    let dir = unique_temp_dir("content-existing-escape");
    let project = dir.join("project");
    std::fs::create_dir(&project).expect("project");
    publish(&project.join(regia_core::brand::MANIFEST_FILE), b"");
    publish(&dir.join("outside.bin"), &[91; 16]);
    let path = project.join("model.gltf");
    for uri in ["../outside.bin", "%2e%2e/outside.bin"] {
        publish(
            &path,
            &serde_json::to_vec(&json!({"asset":{"version":"2.0"},"buffers":[{"uri":uri,"byteLength":16}]}))
                .expect("json"),
        );
        let mut budget = ContentBudget::new(ContentLimits::default(), &|| false);
        let error = open_gltf_bounded(&path, &mut budget).expect_err("existing outside buffer refused");
        assert_eq!(error.code(), "gltf_unreadable");
        assert_eq!(budget.usage().external_bytes, 0);
        assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.join("outside.bin"), project.join("link.bin")).expect("fixture link");
        publish(
            &path,
            br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"link.bin","byteLength":16}]}"#,
        );
        let mut budget = ContentBudget::new(ContentLimits::default(), &|| false);
        assert!(open_gltf_bounded(&path, &mut budget).is_err());
        assert_eq!(budget.usage().external_bytes, 0);
        assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    }
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn short_external_buffer_matches_the_checked_loader() {
    let dir = unique_temp_dir("content-short-external-parity");
    let path = dir.join("model.gltf");
    publish(&dir.join("buffer.bin"), &[1, 2, 3]);
    publish(
        &path,
        br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"buffer.bin","byteLength":3}]}"#,
    );
    let legacy = regia_asset_pipeline::open_gltf_checked(&path).expect("legacy");
    let mut budget = ContentBudget::new(ContentLimits::default(), &|| false);
    let bounded = open_gltf_bounded(&path, &mut budget).expect("bounded");
    assert_eq!(legacy.1[0].0, bounded.1[0].0);
    assert_eq!(budget.usage().external_bytes, 3);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn data_uri_padding_is_charged_before_allocation_and_matches_legacy() {
    let dir = unique_temp_dir("content-data-uri-padding");
    let path = dir.join("model.gltf");
    for (encoded, count) in [("AQ==", 1), ("AQI=", 2), ("AQID", 3), ("AQIDBA==", 4)] {
        publish(&path, &serde_json::to_vec(&json!({"asset":{"version":"2.0"},"buffers":[{"uri":format!("data:application/octet-stream;base64,{encoded}"),"byteLength":count}]})).expect("json"));
        let legacy = regia_asset_pipeline::open_gltf_checked(&path).expect("legacy");
        let mut budget = ContentBudget::new(ContentLimits::default(), &|| false);
        let bounded = open_gltf_bounded(&path, &mut budget).expect("bounded");
        assert_eq!(legacy.1[0].0, bounded.1[0].0);
        let mut budget = ContentBudget::new(
            ContentLimits {
                max_decoded_bytes: 3,
                ..Default::default()
            },
            &|| false,
        );
        assert_eq!(
            open_gltf_bounded(&path, &mut budget)
                .expect_err("padding is budgeted")
                .code(),
            "content_lint_limit"
        );
        assert_eq!(budget.usage().buffer_bytes_allocated, 0);
    }
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn material_suffix_must_match_the_container_without_rewriting() {
    let dir = unique_temp_dir("content-container-suffix");
    for (name, bytes) in [
        ("binary.gltf", regia_asset_pipeline::fuzz_entry::glb_fixture()),
        ("json.glb", br#"{"asset":{"version":"2.0"}}"#.to_vec()),
    ] {
        let path = dir.join(name);
        publish(&path, &bytes);
        for fix in [false, true] {
            let error =
                regia_asset_pipeline::gltf_material_lint::lint_material_file(&path, &Default::default(), fix)
                    .expect_err("suffix must match its container");
            assert_eq!(error.code, "gltf_material_invalid");
            assert_eq!(std::fs::read(&path).expect("unchanged source"), bytes);
        }
    }
    std::fs::remove_dir_all(dir).expect("cleanup");
}
