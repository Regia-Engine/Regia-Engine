// SPDX-License-Identifier: MPL-2.0
//! `.texpack.ron` recipe tests (ADR-3090).

use super::*;

/// Writes a `w`×`h` PNG whose every texel is `px`.
fn png(dir: &Path, name: &str, w: u32, h: u32, px: [u8; 4]) -> PathBuf {
    let path = dir.join(name);
    let img = image::RgbaImage::from_fn(w, h, |_, _| image::Rgba(px));
    img.save(&path).expect("png");
    path
}

fn recipe(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("recipe");
    path
}

fn code(result: Result<impl std::fmt::Debug, TexPackError>) -> &'static str {
    match result {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.code,
    }
}

#[test]
fn the_example_parses_and_the_doc_copy_is_identical() {
    let recipe = TexturePackRecipe::from_ron(EXAMPLE_RECIPE).expect("example");
    assert_eq!(recipe.sources.len(), 3);
    assert_eq!(recipe.usage(), TextureCookUsage::ColorLinear);
    assert_eq!(recipe.a, PackChannel::Constant(1.0));
    let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/formats/texpack.example.ron");
    assert_eq!(std::fs::read_to_string(doc).expect("doc copy"), EXAMPLE_RECIPE);
    assert_eq!(texpack_file_version(EXAMPLE_RECIPE), Some(1));
}

#[test]
fn defaults_are_black_rgb_white_alpha_and_resize_to_largest() {
    let r = TexturePackRecipe::from_ron("(format_version: 1, size: Some((4, 4)))").expect("minimal");
    assert_eq!(r.r, PackChannel::Constant(0.0));
    assert_eq!(r.a, PackChannel::Constant(1.0));
    assert_eq!(r.size_mismatch, SizeMismatch::ResizeToLargest);
}

#[test]
fn every_structural_error_has_its_code() {
    let cases = [
        ("(format_version: 2)", codes::VERSION),
        (
            "// format_version: 9\n(format_version: 3, future: 1)",
            codes::VERSION,
        ),
        ("(format_version: 1, nonsense: 3)", codes::PARSE),
        ("(format_version: 1", codes::PARSE),
        (
            r#"(format_version: 1, sources: {"a": "a.png"}, r: Source(source: "b", channel: "r"))"#,
            codes::UNKNOWN_SOURCE,
        ),
        (
            r#"(format_version: 1, sources: {"a": "a.png"}, g: Source(source: "a", channel: "x"))"#,
            codes::BAD_CHANNEL,
        ),
        ("(format_version: 1, b: Constant(1.5))", codes::BAD_CONSTANT),
        ("(format_version: 1, b: Constant(-0.1))", codes::BAD_CONSTANT),
        (
            r#"(format_version: 1, sources: {"a": "1.png", "b": "2.png", "c": "3.png", "d": "4.png", "e": "5.png"})"#,
            codes::BAD_SOURCES,
        ),
        (
            r#"(format_version: 1, sources: {"": "a.png"})"#,
            codes::BAD_SOURCES,
        ),
        ("(format_version: 1, size: Some((0, 4)))", codes::BAD_SIZE),
        ("(format_version: 1, size: Some((4, 20000)))", codes::BAD_SIZE),
    ];
    for (text, want) in cases {
        assert_eq!(code(TexturePackRecipe::from_ron(text)), want, "{text}");
    }
}

#[test]
fn a_missing_source_a_bad_path_and_an_undecodable_file_fail_with_their_codes() {
    let dir = regia_test_utils::unique_temp_dir("texpack-missing");
    let r = recipe(
        &dir,
        "m.texpack.ron",
        r#"(format_version: 1, sources: {"a": "gone.png"}, r: Source(source: "a", channel: "r"))"#,
    );
    assert_eq!(code(plan_pack(&r)), codes::MISSING_SOURCE);
    assert_eq!(code(pack_rgba8(&r)), codes::MISSING_SOURCE);
    // The dependency list still names the file, so creating it recooks the recipe.
    assert_eq!(recipe_inputs(&r), vec![dir.join("gone.png")]);

    let r = recipe(
        &dir,
        "abs.texpack.ron",
        r#"(format_version: 1, sources: {"a": "/etc/passwd"}, r: Source(source: "a", channel: "r"))"#,
    );
    assert_eq!(code(plan_pack(&r)), codes::BAD_PATH);

    std::fs::write(dir.join("junk.png"), b"not an image").expect("junk");
    let r = recipe(
        &dir,
        "junk.texpack.ron",
        r#"(format_version: 1, sources: {"a": "junk.png"}, r: Source(source: "a", channel: "r"))"#,
    );
    assert_eq!(code(plan_pack(&r)), codes::DECODE);
    assert_eq!(code(pack_rgba8(&r)), codes::DECODE);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cycle_through_nested_recipes_fails_and_a_chain_packs() {
    let dir = regia_test_utils::unique_temp_dir("texpack-cycle");
    png(&dir, "base.png", 4, 4, [10, 20, 30, 40]);
    let a = recipe(
        &dir,
        "a.texpack.ron",
        r#"(format_version: 1, sources: {"b": "b.texpack.ron"}, r: Source(source: "b", channel: "r"))"#,
    );
    recipe(
        &dir,
        "b.texpack.ron",
        r#"(format_version: 1, sources: {"a": "a.texpack.ron"}, r: Source(source: "a", channel: "r"))"#,
    );
    assert_eq!(code(plan_pack(&a)), codes::CYCLE);
    assert_eq!(code(pack_rgba8(&a)), codes::CYCLE);
    let own = recipe(
        &dir,
        "own.texpack.ron",
        r#"(format_version: 1, sources: {"me": "own.texpack.ron"}, r: Source(source: "me", channel: "r"))"#,
    );
    assert_eq!(code(plan_pack(&own)), codes::CYCLE);

    // A chain without a cycle: c reads base.png's G into R; d reads c's R inverted into G.
    recipe(
        &dir,
        "c.texpack.ron",
        r#"(format_version: 1, sources: {"img": "base.png"}, r: Source(source: "img", channel: "g"))"#,
    );
    let d = recipe(
        &dir,
        "d.texpack.ron",
        r#"(format_version: 1, sources: {"c": "c.texpack.ron"}, g: Source(source: "c", channel: "r", invert: true))"#,
    );
    let plan = plan_pack(&d).expect("plan");
    assert_eq!((plan.width, plan.height), (4, 4));
    assert!(plan.sources[0].recipe);
    assert_eq!(plan.inputs, vec![dir.join("c.texpack.ron"), dir.join("base.png")]);
    let packed = pack_rgba8(&d).expect("pack");
    assert!(
        packed
            .rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [0, 255 - 20, 0, 255])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_size_policy_resizes_or_fails() {
    let dir = regia_test_utils::unique_temp_dir("texpack-size");
    png(&dir, "big.png", 8, 4, [200, 0, 0, 255]);
    png(&dir, "small.png", 2, 2, [0, 100, 0, 255]);
    let text = |policy: &str, size: &str| {
        format!(
            r#"(format_version: 1, sources: {{"big": "big.png", "small": "small.png"}},
                r: Source(source: "big", channel: "r"), g: Source(source: "small", channel: "g"),
                size_mismatch: {policy}, size: {size})"#
        )
    };
    let r = recipe(&dir, "e.texpack.ron", &text("error", "None"));
    let err = plan_pack(&r).expect_err("mismatch");
    assert_eq!(err.code, codes::SIZE_MISMATCH);
    assert!(
        err.message.contains("8x4") && err.message.contains("2x2"),
        "{}",
        err.message
    );
    assert_eq!(code(pack_rgba8(&r)), codes::SIZE_MISMATCH);

    let r = recipe(&dir, "l.texpack.ron", &text("resize_to_largest", "None"));
    let plan = plan_pack(&r).expect("largest");
    assert_eq!((plan.width, plan.height), (8, 4));
    assert!(!plan.sources[0].resized && plan.sources[1].resized);
    let packed = pack_rgba8(&r).expect("pack");
    assert_eq!((packed.width, packed.height), (8, 4));
    // Constant images resize to the same constant.
    assert!(
        packed
            .rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [200, 100, 0, 255])
    );

    let r = recipe(&dir, "s.texpack.ron", &text("resize_to_smallest", "None"));
    assert_eq!(pack_rgba8(&r).map(|p| (p.width, p.height)), Ok((2, 2)));
    let r = recipe(&dir, "x.texpack.ron", &text("error", "Some((16, 16))"));
    assert_eq!(pack_rgba8(&r).map(|p| (p.width, p.height)), Ok((16, 16)));

    // Only constants and no size: nothing gives a size.
    let r = recipe(&dir, "c.texpack.ron", "(format_version: 1, r: Constant(0.5))");
    assert_eq!(code(plan_pack(&r)), codes::BAD_SIZE);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn packed_channels_hold_the_expected_values() {
    let dir = regia_test_utils::unique_temp_dir("texpack-values");
    png(&dir, "rgb.png", 4, 4, [255, 0, 0, 77]);
    let r = recipe(
        &dir,
        "v.texpack.ron",
        r#"(format_version: 1, sources: {"s": "rgb.png"},
            r: Source(source: "s", channel: "luma"),
            g: Source(source: "s", channel: "a"),
            b: Source(source: "s", channel: "r", invert: true),
            a: Constant(0.25))"#,
    );
    let packed = pack_rgba8(&r).expect("pack");
    // Rec. 709 luma of pure red: 0.2126 * 255 = 54.2.
    assert!(
        packed
            .rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [54, 77, 0, 64])
    );
    let consts = recipe(
        &dir,
        "k.texpack.ron",
        "(format_version: 1, r: Constant(0.0), g: Constant(1.0), b: Constant(0.5), a: Constant(0.2), size: Some((3, 5)))",
    );
    let packed = pack_rgba8(&consts).expect("pack");
    assert_eq!((packed.width, packed.height), (3, 5));
    assert!(packed.inputs.is_empty());
    assert!(
        packed
            .rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [0, 255, 128, 51])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resize_weights_sum_to_one_and_a_ramp_stays_monotone() {
    for (src, dst) in [(7u32, 3u32), (3, 7), (8, 2), (2, 8), (5, 5), (1, 4), (4, 1)] {
        for taps in axis_weights(src, dst) {
            let sum: f32 = taps.iter().map(|t| t.1).sum();
            assert!((sum - 1.0).abs() < 1e-4, "{src}->{dst}: {taps:?}");
            assert!(taps.iter().all(|t| t.0 < src as usize));
        }
    }
    let ramp: Vec<u8> = (0..16u8).flat_map(|i| [i * 16, 0, 0, 255]).collect();
    for dst in [4u32, 9, 32] {
        let out = resize_rgba8(16, 1, &ramp, dst, 1);
        let reds: Vec<u8> = out.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
        assert!(reds.windows(2).all(|w| w[0] <= w[1]), "{dst}: {reds:?}");
    }
}

#[test]
fn the_fingerprint_names_every_field_and_variant() {
    let f = texpack_schema_fingerprint();
    assert_eq!(
        f,
        "format_version,sources,r,g,b,a,size_mismatch,size,usage;Source,Constant;\
         resize_to_largest,resize_to_smallest,error;color,linear,normal,mask,ui"
    );
    // The hash `docs/formats/format-hashes.ron` records (`cargo xtask check-formats`).
    eprintln!("texpack schema hash {}", blake3::hash(f.as_bytes()).to_hex());
    for name in [
        "format_version",
        "sources",
        "size_mismatch",
        "Source",
        "Constant",
        "resize_to_largest",
        "error",
    ] {
        assert!(f.contains(name), "{name} missing from {f}");
    }
}
