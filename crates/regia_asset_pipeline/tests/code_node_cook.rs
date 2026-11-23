// SPDX-License-Identifier: MPL-2.0
//! The WGSL Code node through the cook (ADR-7852): a graph whose Code node includes a
//! `shaders/include/` file cooks (the include is resolved at cook time and its text reaches the
//! cooked code); a graph whose Code node does not parse is refused with the compile's message,
//! naming the line; editing the include file changes the cooked code.

use std::path::{Path, PathBuf};

use regia_asset_pipeline::{CookOptions, cook_project};

const GOOD: &str = r##"(
    format_version: 5,
    name: "mad",
    nodes: {
        "uv": TexCoord,
        "u": Swizzle(input: "uv", channels: "x"),
        "v": Swizzle(input: "uv", channels: "y"),
        "k": Float(0.125),
        "mad": Code(
            inputs: [(name: "a", ty: Float, node: "u"), (name: "b", ty: Float, node: "v"), (name: "c", ty: Float, node: "k")],
            output: Float,
            code: "#include \"mad.wgsl\"\nreturn helper_mad(a, b, c);",
        ),
    },
    output: (roughness: Some("mad")),
)"##;

const BAD: &str = r#"(
    format_version: 5,
    name: "broken",
    nodes: {
        "k": Float(0.5),
        "bad": Code(
            inputs: [(name: "a", ty: Float, node: "k")],
            output: Float,
            code: "let t = a;\nreturn t *;",
        ),
    },
    output: (roughness: Some("bad")),
)"#;

const INCLUDE: &str = "fn helper_mad(x: f32, y: f32, z: f32) -> f32 {\n    return x * y + z;\n}\n";

fn project(tag: &str, graphs: &[(&str, &str)]) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("assets/materials")).expect("materials dir");
    std::fs::create_dir_all(root.join("shaders/include")).expect("include dir");
    std::fs::write(root.join("shaders/include/mad.wgsl"), INCLUDE).expect("include");
    for (name, text) in graphs {
        std::fs::write(root.join("assets/materials").join(name), text).expect("graph");
    }
    root
}

/// Every cooked file's bytes under the project's cache, concatenated (to look for code text).
fn cooked_bytes(root: &Path) -> Vec<u8> {
    let cache = regia_asset::cooked_cache_dir(root, regia_asset_pipeline::host_platform());
    let mut all = Vec::new();
    for entry in std::fs::read_dir(&cache).expect("cache").flatten() {
        if entry.path().is_file() {
            all.extend(std::fs::read(entry.path()).unwrap_or_default());
        }
    }
    all
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn a_code_node_with_an_include_cooks_and_a_bad_one_is_refused_at_its_line() {
    let root = project(
        "code_node_cook",
        &[("mad.matgraph.ron", GOOD), ("broken.matgraph.ron", BAD)],
    );
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook runs");
    let refused: Vec<&(String, String)> = report
        .errors
        .iter()
        .filter(|(path, _)| path.contains("broken"))
        .collect();
    assert_eq!(refused.len(), 1, "{:?}", report.errors);
    assert!(
        refused[0].1.contains("code line 2, column"),
        "the cook names the line: {:?}",
        refused[0]
    );
    assert!(
        !report
            .errors
            .iter()
            .any(|(path, _)| path.contains("mad.matgraph")),
        "{:?}",
        report.errors
    );
    let bytes = cooked_bytes(&root);
    assert!(
        contains(&bytes, "fn helper_mad"),
        "the include's text is cooked in"
    );
    assert!(contains(&bytes, "return x * y + z;"));

    // Editing the include recooks the graph with the new text.
    std::fs::write(
        root.join("shaders/include/mad.wgsl"),
        INCLUDE.replace("x * y + z", "x * y - z"),
    )
    .expect("edit include");
    let again = cook_project(CookOptions::for_host(root.clone())).expect("cook runs");
    assert!(
        !again.errors.iter().any(|(path, _)| path.contains("mad.matgraph")),
        "{:?}",
        again.errors
    );
    assert!(contains(&cooked_bytes(&root), "return x * y - z;"));
    std::fs::remove_dir_all(&root).ok();
}
