// SPDX-License-Identifier: MPL-2.0
//! Tests of the game executables' Windows build script (`templates/windows_build.rs`, ADR-2645):
//! the ICO writer decoded back, the PNG reader on every colour type and on hostile input, the
//! version packing and the `Regia.toml` reading. The script itself is compiled here as
//! `crate::windows_build`; its `main` and the `winresource` call are left out of test builds.

use crate::windows_build::{
    ICON_SIZES, ICON_SOURCE_CAP, MAIN_STACK_BYTES, MAX_ICON_SIDE, ProjectInfo, Rgba, axis_weights,
    decode_png, encode_png, find_project_manifest, fit_square, ico_from_rgba, icon_path, icon_source_side,
    main_stack_link_arg, placeholder_icon, resize, version_words,
};

/// A fresh temp directory unique to this process and call (pid + counter). Not
/// `regia_test_utils::unique_temp_dir`: that crate links the whole engine, this one links nothing.
fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-{label}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// One parsed ICONDIRENTRY: the side (256 for a stored 0), planes, bit count and the image bytes.
struct IcoEntry {
    side: u32,
    planes: u16,
    bits: u16,
    image: Vec<u8>,
}

/// Reads an ICO file back (ICONDIR, then one 16-byte entry per image), checking that the images
/// tile the rest of the file in order.
fn parse_ico(bytes: &[u8]) -> Vec<IcoEntry> {
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    assert_eq!(u16_at(0), 0, "reserved");
    assert_eq!(u16_at(2), 1, "type 1 is an icon");
    let count = usize::from(u16_at(4));
    let mut expected_offset = 6 + 16 * count;
    let mut entries = Vec::new();
    for i in 0..count {
        let e = 6 + 16 * i;
        assert_eq!(bytes[e], bytes[e + 1], "square entries");
        assert_eq!(bytes[e + 2], 0, "no palette");
        assert_eq!(bytes[e + 3], 0, "reserved");
        let length = u32_at(e + 8) as usize;
        let offset = u32_at(e + 12) as usize;
        assert_eq!(offset, expected_offset, "entry {i} follows the previous image");
        expected_offset += length;
        entries.push(IcoEntry {
            side: if bytes[e] == 0 { 256 } else { u32::from(bytes[e]) },
            planes: u16_at(e + 4),
            bits: u16_at(e + 6),
            image: bytes[offset..offset + length].to_vec(),
        });
    }
    assert_eq!(expected_offset, bytes.len(), "the images end the file");
    entries
}

/// A `side` x `side` image of four solid quadrants: red, green, blue, and white at alpha 128.
fn quadrants(side: u32) -> Rgba {
    let mut pixels = Vec::new();
    for y in 0..side {
        for x in 0..side {
            pixels.extend_from_slice(&quadrant_colour(x * 2 / side, y * 2 / side));
        }
    }
    Rgba {
        width: side,
        height: side,
        pixels,
    }
}

fn quadrant_colour(qx: u32, qy: u32) -> [u8; 4] {
    match (qx, qy) {
        (0, 0) => [255, 0, 0, 255],
        (1, 0) => [0, 255, 0, 255],
        (0, _) => [0, 0, 255, 255],
        _ => [255, 255, 255, 128],
    }
}

fn pixel(image: &Rgba, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * image.width + x) * 4) as usize;
    let p = &image.pixels[at..at + 4];
    [p[0], p[1], p[2], p[3]]
}

fn close(a: [u8; 4], b: [u8; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 1)
}

/// Encodes raw samples as a PNG of the given colour type and depth.
fn png_of(
    width: u32,
    height: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    data: &[u8],
    palette: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(color);
    encoder.set_depth(depth);
    if let Some(palette) = palette {
        encoder.set_palette(palette.to_vec());
    }
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(data).expect("png data");
    writer.finish().expect("png finish");
    out
}

#[test]
fn the_ico_holds_every_size_as_a_png_and_decodes_back_to_the_source() {
    let source = quadrants(256);
    let png = encode_png(&source).expect("encode");
    let decoded = decode_png(&png).expect("decode");
    assert_eq!(decoded, source, "the PNG round trip is lossless");

    let ico = ico_from_rgba(&decoded).expect("ico");
    let entries = parse_ico(&ico);
    let sides: Vec<u32> = entries.iter().map(|e| e.side).collect();
    assert_eq!(sides, ICON_SIZES, "16, 32, 48 and 256 (stored as 0)");
    for entry in &entries {
        assert_eq!((entry.planes, entry.bits), (1, 32), "{} px", entry.side);
        assert!(
            entry.image.starts_with(b"\x89PNG\r\n\x1a\n"),
            "{} px is PNG-compressed",
            entry.side
        );
        let image = decode_png(&entry.image).expect("entry decodes");
        assert_eq!((image.width, image.height), (entry.side, entry.side));
        if entry.side == 256 {
            assert_eq!(image, source, "the 256 px entry is the source, pixel for pixel");
            continue;
        }
        // The middle of each quadrant is that quadrant's colour (premultiplied resampling keeps
        // the half-transparent white white).
        for (qx, qy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let (x, y) = (
                entry.side / 4 + qx * entry.side / 2,
                entry.side / 4 + qy * entry.side / 2,
            );
            let got = pixel(&image, x, y);
            assert!(
                close(got, quadrant_colour(qx, qy)),
                "{} px at ({x}, {y}): {got:?}",
                entry.side
            );
        }
    }
}

#[test]
fn shrinking_a_solid_image_keeps_its_colour_exactly() {
    let solid = Rgba {
        width: 512,
        height: 512,
        pixels: [10u8, 200, 30, 255].repeat(512 * 512),
    };
    for size in ICON_SIZES {
        let small = resize(&solid, size);
        assert_eq!(small.pixels.len(), (size * size * 4) as usize);
        assert!(
            small
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [10, 200, 30, 255]),
            "{size} px"
        );
    }
}

#[test]
fn axis_weights_sum_to_one_whether_shrinking_or_growing() {
    for (from, to) in [
        (256, 16),
        (256, 48),
        (300, 48),
        (48, 256),
        (1, 16),
        (17, 16),
        (16, 16),
    ] {
        let weights = axis_weights(from, to);
        assert_eq!(weights.len(), to as usize);
        for (d, taps) in weights.iter().enumerate() {
            let sum: f32 = taps.iter().map(|&(_, w)| w).sum();
            assert!((sum - 1.0).abs() < 1e-4, "{from}->{to} pixel {d}: {sum}");
            assert!(
                taps.iter().all(|&(s, _)| s < from as usize),
                "{from}->{to}: tap in range"
            );
        }
    }
}

#[test]
fn a_non_square_or_small_icon_is_centred_and_scaled() {
    // 64 x 32 opaque grey: padded to 64 x 64 (16 transparent rows above and below), then grown
    // to 256 and shrunk to 16, 32 and 48.
    let png = png_of(
        64,
        32,
        png::ColorType::Grayscale,
        png::BitDepth::Eight,
        &[90u8; 64 * 32],
        None,
    );
    let image = decode_png(&png).expect("grey decodes");
    assert_eq!(pixel(&image, 3, 3), [90, 90, 90, 255]);
    let square = fit_square(&image);
    assert_eq!((square.width, square.height), (64, 64));
    assert_eq!(pixel(&square, 10, 5), [0, 0, 0, 0], "padding above");
    assert_eq!(
        pixel(&square, 10, 32),
        [90, 90, 90, 255],
        "the image in the middle"
    );
    assert_eq!(pixel(&square, 10, 60), [0, 0, 0, 0], "padding below");
    let entries = parse_ico(&ico_from_rgba(&image).expect("ico"));
    for entry in entries {
        let icon = decode_png(&entry.image).expect("entry decodes");
        let s = entry.side;
        assert_eq!(pixel(&icon, s / 2, s / 2), [90, 90, 90, 255], "{s} px middle");
        assert_eq!(pixel(&icon, s / 2, 0)[3], 0, "{s} px top row is transparent");
        assert_eq!(
            pixel(&icon, s / 2, s - 1)[3],
            0,
            "{s} px bottom row is transparent"
        );
    }
}

#[test]
fn every_png_colour_type_decodes_to_rgba() {
    // 16-bit grey-alpha: big-endian samples, stripped to their high byte.
    let grey_alpha = png_of(
        1,
        1,
        png::ColorType::GrayscaleAlpha,
        png::BitDepth::Sixteen,
        &[0x40, 0x11, 0x80, 0x22],
        None,
    );
    assert_eq!(
        decode_png(&grey_alpha).expect("ga16").pixels,
        [0x40, 0x40, 0x40, 0x80]
    );
    let rgb = png_of(1, 1, png::ColorType::Rgb, png::BitDepth::Eight, &[1, 2, 3], None);
    assert_eq!(decode_png(&rgb).expect("rgb").pixels, [1, 2, 3, 255]);
    // A 2-bit palette image: index 1 then index 0.
    let palette = [10, 20, 30, 40, 50, 60];
    let indexed = png_of(
        2,
        1,
        png::ColorType::Indexed,
        png::BitDepth::Two,
        &[0b0100_0000],
        Some(&palette),
    );
    assert_eq!(
        decode_png(&indexed).expect("indexed").pixels,
        [40, 50, 60, 255, 10, 20, 30, 255]
    );
}

#[test]
fn hostile_or_oversized_pngs_are_refused_without_panicking() {
    assert!(decode_png(b"").is_err());
    assert!(decode_png(b"not a png at all").is_err());
    let good = encode_png(&quadrants(32)).expect("encode");
    for cut in [8, 16, 33, good.len() / 2, good.len() - 13] {
        assert!(decode_png(&good[..cut]).is_err(), "cut at {cut}");
    }
    // Flipping bytes in the image data must give an error or an image, never a panic.
    for at in (40..good.len()).step_by(7) {
        let mut bad = good.clone();
        bad[at] ^= 0x5a;
        let _ = decode_png(&bad);
    }
    let wide = png_of(
        MAX_ICON_SIDE + 1,
        1,
        png::ColorType::Grayscale,
        png::BitDepth::Eight,
        &vec![0; (MAX_ICON_SIDE + 1) as usize],
        None,
    );
    let err = decode_png(&wide).expect_err("too wide");
    assert!(err.contains("4096"), "{err}");
}

#[test]
fn a_large_icon_is_resampled_from_a_capped_source() {
    // corr-viewer-view-templates LOW 7: a 4096 icon used to build a ~270 MB premultiplied f32
    // copy in this build script. The source is capped before the size passes; the cap is the
    // observable pin (the memory itself cannot be a unit assertion).
    const {
        assert!(
            ICON_SOURCE_CAP < MAX_ICON_SIDE,
            "the working source is capped below what the decoder accepts"
        );
    }
    assert_eq!(icon_source_side(MAX_ICON_SIDE), ICON_SOURCE_CAP);
    assert_eq!(icon_source_side(256), 256, "a small icon is untouched");
    // A 2048 icon still cooks every size (and on the base ran the uncapped path).
    let big = Rgba {
        width: 2048,
        height: 2048,
        pixels: [120, 80, 40, 255].repeat(2048 * 2048),
    };
    assert_eq!(
        parse_ico(&ico_from_rgba(&big).expect("ico")).len(),
        ICON_SIZES.len()
    );
}

#[test]
fn the_placeholder_icon_is_a_rounded_square_with_a_light_triangle() {
    let icon = placeholder_icon();
    assert_eq!((icon.width, icon.height), (256, 256));
    assert_eq!(pixel(&icon, 0, 0)[3], 0, "transparent corner");
    assert_eq!(pixel(&icon, 12, 12)[3], 0, "the corner is rounded");
    let edge = pixel(&icon, 128, 20);
    assert_eq!(edge[3], 255, "inside the square");
    assert!(edge[2] > edge[0], "slate blue background: {edge:?}");
    let centre = pixel(&icon, 130, 128);
    assert!(centre[..3].iter().all(|&c| c > 220), "light triangle: {centre:?}");
    assert_eq!(
        parse_ico(&ico_from_rgba(&icon).expect("ico")).len(),
        ICON_SIZES.len()
    );
}

#[test]
fn file_versions_pack_sixteen_bits_per_part() {
    assert_eq!(version_words("1.2.3"), 0x0001_0002_0003_0000);
    assert_eq!(version_words("1.2.3.4"), 0x0001_0002_0003_0004);
    assert_eq!(version_words("0.1.0"), 0x0000_0001_0000_0000);
    assert_eq!(version_words("2.0.0-beta.3+git.abc"), 0x0002_0000_0000_0000);
    assert_eq!(
        version_words("70000.1.x"),
        0x0000_0001_0000_0000,
        "out of range and text are 0"
    );
    assert_eq!(version_words(""), 0);
    assert_eq!(
        version_words("1.2.3.4.5"),
        0x0001_0002_0003_0004,
        "a fifth part is dropped"
    );
}

#[test]
fn the_project_manifest_names_the_product_the_company_and_the_icon() {
    let text = r#"
[project]
name = "Night Train"
id = "com.example.night"
version = "1.4.2"
organization = "Example Studio"

[platforms.windows]
icon = "assets/branding/train.png"
render_profile = "DesktopHigh"
"#;
    let info = ProjectInfo::read(text, "night_train_launcher", "0.1.0").expect("reads");
    assert_eq!(info.name, "Night Train");
    assert_eq!(info.version, "1.4.2");
    assert_eq!(info.organization, "Example Studio");
    assert_eq!(info.icon.as_deref(), Some("assets/branding/train.png"));

    let bare = ProjectInfo::read("", "quick_game", "0.3.0").expect("no manifest");
    assert_eq!(
        (
            bare.name.as_str(),
            bare.version.as_str(),
            bare.organization.as_str(),
            bare.icon
        ),
        ("quick_game", "0.3.0", "", None)
    );
    let blank = ProjectInfo::read(
        "[project]\nname = \"  \"\norganization = \"\"\n[platforms.windows]\nicon = \" \"\n",
        "pkg",
        "9.9.9",
    )
    .expect("blank fields");
    assert_eq!((blank.name.as_str(), blank.icon), ("pkg", None));
    assert!(
        ProjectInfo::read("[project\nname = ", "pkg", "1.0.0").is_err(),
        "bad TOML"
    );
}

#[test]
fn the_manifest_and_the_icon_are_found_where_the_templates_put_them() {
    let root = unique_temp_dir("templates-windows-build");
    let launcher = root.join("launcher");
    std::fs::create_dir_all(launcher.join("src")).expect("dirs");
    assert_eq!(find_project_manifest(&launcher), None);
    std::fs::write(root.join("Regia.toml"), "[project]\n").expect("manifest");
    assert_eq!(
        find_project_manifest(&launcher),
        Some(root.join("Regia.toml")),
        "one level up"
    );
    std::fs::write(launcher.join("Regia.toml"), "[project]\n").expect("manifest");
    assert_eq!(
        find_project_manifest(&launcher),
        Some(launcher.join("Regia.toml")),
        "its own first"
    );

    assert_eq!(icon_path(&root, None), None);
    std::fs::create_dir_all(root.join("assets/branding")).expect("dirs");
    std::fs::write(root.join("assets/icon.png"), b"x").expect("icon");
    assert_eq!(icon_path(&root, None), Some(root.join("assets/icon.png")));
    std::fs::write(root.join("assets/branding/icon.png"), b"x").expect("icon");
    assert_eq!(
        icon_path(&root, None),
        Some(root.join("assets/branding/icon.png"))
    );
    assert_eq!(
        icon_path(&root, Some("art/app.png")),
        Some(root.join("art/app.png")),
        "the configured path wins, existing or not (a missing one warns at build time)"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The blocks ADR-5181 added to the script; removing them gives the 0.2.23 text back.
const ADR_5181_BLOCKS: [&str; 4] = [
    "//! It also gives the exe's main thread an 8 MiB stack, the size Linux gives it. Windows reserves\n\
     //! 1 MiB unless the exe asks for more, and a debug build of a template game overflows that while\n\
     //! it starts (seen on Windows 11, MSVC, 2026-11-10).\n\
     //!\n",
    "\n/// The main thread's stack reserve in bytes (the Linux default; Windows' own default is 1 MiB).\n\
     pub(crate) const MAIN_STACK_BYTES: u32 = 8 << 20;\n",
    "    let target_env = std::env::var(\"CARGO_CFG_TARGET_ENV\").unwrap_or_default();\n\
     \x20   if let Some(arg) = main_stack_link_arg(&target_env) {\n\
     \x20       println!(\"cargo:rustc-link-arg-bins={arg}\");\n\
     \x20   }\n",
    "/// The linker argument that sets the exe's main-thread stack reserve to [`MAIN_STACK_BYTES`]:\n\
     /// `/STACK` for the MSVC linker, `--stack` for MinGW `ld` and `lld`. Another toolchain gets none.\n\
     pub(crate) fn main_stack_link_arg(target_env: &str) -> Option<String> {\n\
     \x20   match target_env {\n\
     \x20       \"msvc\" => Some(format!(\"/STACK:{MAIN_STACK_BYTES}\")),\n\
     \x20       \"gnu\" => Some(format!(\"-Wl,--stack,{MAIN_STACK_BYTES}\")),\n\
     \x20       _ => None,\n\
     \x20   }\n\
     }\n\
     \n",
];

#[test]
fn the_exe_main_thread_gets_the_linux_stack_size_on_windows() {
    assert_eq!(MAIN_STACK_BYTES, 8 * 1024 * 1024);
    assert_eq!(main_stack_link_arg("msvc").as_deref(), Some("/STACK:8388608"));
    assert_eq!(main_stack_link_arg("gnu").as_deref(), Some("-Wl,--stack,8388608"));
    assert_eq!(main_stack_link_arg(""), None, "an unknown toolchain gets no flag");
    // Only for the exe targets of the crate, and before the resources (which may fail and return).
    let script = crate::WINDOWS_BUILD_SCRIPT;
    let link = script
        .find("cargo:rustc-link-arg-bins={arg}")
        .expect("the script emits the link argument");
    let embed = script.find("embed(&manifest_dir)").expect("embed call");
    assert!(
        link < embed,
        "the stack size does not depend on the resource compiler"
    );
}

/// The blocks the corr-low-editor icon cap added to the script; removing them (with
/// [`ADR_5181_BLOCKS`]) gives the 0.2.23 text back.
const ICON_CAP_BLOCKS: [&str; 2] = [
    "\n/// What a larger icon is pre-resampled to before the icon sizes are made: the largest size is\n\
     /// 256, so a 1024 source loses nothing, and the premultiplied f32 copy stays at 16 MB instead\n\
     /// of ~270 MB for a 4096 icon in a build script.\n\
     pub(crate) const ICON_SOURCE_CAP: u32 = 1024;\n\
     \n\
     /// The side `ico_from_rgba` resamples the fitted icon from: at most [`ICON_SOURCE_CAP`].\n\
     pub(crate) fn icon_source_side(width: u32) -> u32 {\n\
     \x20   width.min(ICON_SOURCE_CAP)\n\
     }\n",
    "    // Pre-resample a large source once so each size pass works from at most 1024 px (see\n\
     \x20   // `ICON_SOURCE_CAP`): memory stays at build-script scale.\n\
     \x20   let square = if square.width > ICON_SOURCE_CAP {\n\
     \x20       resize(&square, icon_source_side(square.width))\n\
     \x20   } else {\n\
     \x20       square\n\
     \x20   };\n",
];

/// The SPDX first line the script gained with ADR-10225 (MIT-0, template code). Removing it gives
/// the f46515d58 text back.
const SPDX_BLOCKS: [&str; 1] = ["// SPDX-License-Identifier: MIT-0\n"];

#[test]
fn an_unchanged_earlier_script_is_recognised_and_an_edited_one_is_not() {
    let current = crate::WINDOWS_BUILD_SCRIPT;
    assert!(
        current.starts_with(SPDX_BLOCKS[0]),
        "the script starts with its SPDX line"
    );
    assert!(
        crate::is_earlier_windows_build_script(&current.replacen(SPDX_BLOCKS[0], "", 1)),
        "the f46515d58 script (before the SPDX line)"
    );
    let mut earlier = current.to_owned();
    for block in SPDX_BLOCKS
        .into_iter()
        .chain(ADR_5181_BLOCKS)
        .chain(ICON_CAP_BLOCKS)
    {
        assert_eq!(
            earlier.matches(block).count(),
            1,
            "block moved or edited: {block}"
        );
        earlier = earlier.replacen(block, "", 1);
    }
    assert!(
        crate::is_earlier_windows_build_script(&earlier),
        "the 0.2.23 script"
    );
    assert!(
        crate::is_earlier_windows_build_script(&earlier.replace('\n', "\r\n")),
        "a CRLF checkout of it"
    );
    assert!(
        !crate::is_earlier_windows_build_script(current),
        "the current script is not earlier"
    );
    assert!(!crate::is_earlier_windows_build_script(&format!(
        "{earlier}// mine\n"
    )));
}
