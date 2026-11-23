// SPDX-License-Identifier: MPL-2.0
//! The terrain shaders tonemap in-shader (they write an sRGB target directly), so they
//! carry a copy of the renderer's display transform. Both must produce the same picture:
//! a terrain pixel and a mesh pixel of the same radiance must land on the same display
//! value, or the ground and the props sitting on it disagree in brightness and colour.

const RENDER_UTILS: &str = include_str!("../../regia_render/shaders/utils.wgsl");

const MIRRORS: [(&str, &str); 4] = [
    ("terrain.wgsl", include_str!("../src/gpu/terrain.wgsl")),
    ("scatter.wgsl", include_str!("../src/gpu/scatter.wgsl")),
    ("water.wgsl", include_str!("../src/gpu/water.wgsl")),
    (
        "portal.wgsl",
        include_str!("../../../samples/lookdev/shaders/portal.wgsl"),
    ),
];

/// The source of `fn name` (signature through the closing brace at column 0), with
/// comment lines removed.
fn function(source: &str, name: &str) -> String {
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("fn {name} missing"));
    let end = source[start..]
        .find("\n}\n")
        .map(|e| start + e + 3)
        .expect("fn end");
    source[start..end]
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn constant(source: &str, name: &str) -> String {
    source
        .lines()
        .find(|line| line.starts_with(&format!("const {name}:")))
        .unwrap_or_else(|| panic!("const {name} missing"))
        .to_string()
}

#[test]
fn terrain_display_transform_mirrors_the_renderer() {
    for (file, source) in MIRRORS {
        for name in [
            "agx_sigmoid",
            "tonemap_agx_vec",
            "display_look",
            "default_display_transform",
        ] {
            assert_eq!(
                function(source, name),
                function(RENDER_UTILS, name),
                "{file}: fn {name} differs from regia_render utils.wgsl"
            );
        }
        for name in ["DEFAULT_LOOK_CONTRAST", "DEFAULT_LOOK_SATURATION"] {
            assert_eq!(
                constant(source, name),
                constant(RENDER_UTILS, name),
                "{file}: {name}"
            );
        }
        assert!(
            !source.contains("fn tonemap_agx("),
            "{file}: the per-channel linear-input AgX fit is the bug this mirror replaced"
        );
    }
}
