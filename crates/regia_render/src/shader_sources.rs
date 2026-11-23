// SPDX-License-Identifier: MPL-2.0
//! Which engine source files can put shader text into a pipeline, and one hash over all of them
//! (ADR-161 addendum, 2026-10-10). Shared by `build.rs`, which bakes the hash into
//! `REGIA_ENGINE_SHADER_SOURCES` for [`crate::pipeline_cache::engine_shader_hash`], and by the
//! tests. Plain `std` + `blake3` only: it is compiled into the build script too.
//!
//! The set: every `.wgsl` file under the [`SHADER_CRATES`]' `src/` and `shaders/` trees, plus
//! every Rust source there that carries WGSL text itself ([`WGSL_MARKERS`]: inline shaders, the
//! `shader_lib` composer, shader text generated in Rust such as the DDGI / impostor / VFX /
//! surface-graph sources). A marked Rust file is hashed whole, so a Rust-only edit to it also
//! moves the hash: an over-invalidation that costs one pipeline-cache rebuild, never a stale
//! "match".

use std::path::{Path, PathBuf};

/// Engine crates whose shaders can reach a pipeline (the editor's picking shader never does).
pub const SHADER_CRATES: &[&str] = &[
    "regia_render",
    "regia_terrain",
    "regia_vfx",
    "regia_ui",
    "regia_2d",
    "regia_pt",
    "regia_matgraph",
    "regia_xr",
    // The product viewer's own forward pass (ADR-2539): it runs on its own device, so its shaders only
    // ever over-invalidate the engine's pipeline cache.
    "regia_viewer",
    "regia_fluid",
    // The product looks (ADR-2538): hooks for that same forward pass and a standalone renderer, also on
    // their own devices.
    "regia_looks",
    "regia_xess",
    // The quick-start 2D draw list (`draw2d.wgsl`, gpu-2d-drawlist): its pipeline is built on the
    // engine's device (ADR-8598).
    "regia_quick",
];

/// Text that marks a Rust source as carrying WGSL (attributes, address spaces, vector types).
pub const WGSL_MARKERS: &[&str] = &[
    "@fragment",
    "@vertex",
    "@compute",
    "@group(",
    "var<",
    "vec2<f32>",
    "vec3<f32>",
    "vec4<f32>",
];

/// Subtrees of a shader crate that are scanned.
const TREES: &[&str] = &["src", "shaders"];

/// One hashed source file.
pub struct ShaderSource {
    /// `crate/tree/…/file`, `/`-separated (the same on every host).
    pub rel: String,
    /// Where it was read from.
    pub path: PathBuf,
    /// Its bytes with `\r` removed (a CRLF checkout hashes like an LF one).
    pub bytes: Vec<u8>,
}

/// Every shader source of the [`SHADER_CRATES`] under `crates_dir` (the workspace's `crates/`),
/// sorted by [`ShaderSource::rel`]. A crate that is missing (a build outside the workspace)
/// contributes a `<crate>: missing` entry, so such a build never claims the workspace's hash.
#[must_use]
pub fn collect(crates_dir: &Path) -> Vec<ShaderSource> {
    let mut out = Vec::new();
    for name in SHADER_CRATES {
        let root = crates_dir.join(name);
        if !root.is_dir() {
            out.push(ShaderSource {
                rel: format!("{name}: missing"),
                path: root,
                bytes: Vec::new(),
            });
            continue;
        }
        for tree in TREES {
            walk(&root.join(tree), &format!("{name}/{tree}"), &mut out);
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<ShaderSource>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = format!("{rel}/{name}");
        if path.is_dir() {
            walk(&path, &child, out);
            continue;
        }
        let is_wgsl = name.ends_with(".wgsl");
        if !is_wgsl && !name.ends_with(".rs") {
            continue;
        }
        let Ok(mut bytes) = std::fs::read(&path) else {
            continue;
        };
        if !is_wgsl {
            let text = String::from_utf8_lossy(&bytes);
            if !WGSL_MARKERS.iter().any(|m| text.contains(m)) {
                continue;
            }
        }
        bytes.retain(|&b| b != b'\r');
        out.push(ShaderSource {
            rel: child,
            path,
            bytes,
        });
    }
}

/// Hash of `sources` (32 hex digits): each file's relative path, length and bytes.
#[must_use]
pub fn hash(sources: &[ShaderSource]) -> String {
    let mut h = blake3::Hasher::new();
    for s in sources {
        h.update(&(s.rel.len() as u64).to_le_bytes());
        h.update(s.rel.as_bytes());
        h.update(&(s.bytes.len() as u64).to_le_bytes());
        h.update(&s.bytes);
    }
    h.finalize().to_hex()[..32].to_string()
}

/// Directories whose new files must re-run the build script: `own_crate`'s trees (it rebuilds on
/// those edits anyway) and every directory that holds a `.wgsl` file. Individual files are
/// watched on top (`ShaderSource::path`).
#[must_use]
pub fn watch_dirs(crates_dir: &Path, own_crate: &str, sources: &[ShaderSource]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = TREES
        .iter()
        .map(|t| crates_dir.join(own_crate).join(t))
        .filter(|d| d.is_dir())
        .collect();
    for s in sources {
        if s.rel.ends_with(".wgsl")
            && let Some(parent) = s.path.parent()
            && !dirs.iter().any(|d| d == parent)
        {
            dirs.push(parent.to_path_buf());
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crates_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    #[test]
    fn the_baked_hash_is_the_trees_hash() {
        let sources = collect(&crates_dir());
        assert_eq!(
            hash(&sources),
            env!("REGIA_ENGINE_SHADER_SOURCES"),
            "build.rs baked a hash of another tree (a missed rerun-if-changed?)"
        );
    }

    #[test]
    fn every_engine_shader_source_is_covered() {
        let sources = collect(&crates_dir());
        let rels: Vec<&str> = sources.iter().map(|s| s.rel.as_str()).collect();
        assert!(!rels.iter().any(|r| r.ends_with(": missing")), "{rels:?}");
        for must in [
            // The core library ADR-161 hashed first, and the WGSL outside it that it missed.
            "regia_render/shaders/pbr_lighting.wgsl",
            "regia_render/shaders/post.wgsl",
            "regia_render/shaders/ddgi_trace.wgsl",
            "regia_render/src/tsr/tsr_resolve.wgsl",
            "regia_render/src/visbuffer/vis_raster.wgsl",
            "regia_render/src/fsr3/fsr3_accumulate.wgsl",
            "regia_terrain/src/gpu/terrain.wgsl",
            "regia_terrain/src/gpu/impostor.wgsl",
            "regia_vfx/src/gpu/radix_sort.wgsl",
            "regia_ui/shaders/ui-overlay.wgsl",
            "regia_2d/shaders/sprite2d.wgsl",
            "regia_fluid/src/fluid_flip.wgsl",
            // The composer and WGSL built in Rust.
            "regia_render/src/shader_lib.rs",
            "regia_render/src/decal_receiver.rs",
            "regia_render/src/ddgi_gpu.rs",
            "regia_render/src/surface_graph.rs",
            "regia_terrain/src/gpu/mod.rs",
            "regia_vfx/src/gpu/shaders.rs",
            "regia_matgraph/src/compile.rs",
        ] {
            assert!(rels.contains(&must), "{must} is not hashed: {rels:?}");
        }
        // Every WGSL file in the workspace's crates belongs to a hashed crate (a new shader crate
        // must be added to SHADER_CRATES), except the editor's and test fixtures.
        let mut stack = vec![crates_dir()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    if !matches!(
                        name.as_str(),
                        "target" | "tests" | "examples" | "benches" | "regia_editor"
                    ) {
                        stack.push(path);
                    }
                } else if name.ends_with(".wgsl") {
                    assert!(
                        sources.iter().any(|s| s.path == path),
                        "{} is engine WGSL but not hashed",
                        path.display()
                    );
                }
            }
        }
    }

    #[test]
    fn the_build_script_watches_every_shader_directory() {
        let crates = crates_dir();
        let sources = collect(&crates);
        let dirs = watch_dirs(&crates, "regia_render", &sources);
        for must in [
            "regia_render/src",
            "regia_render/shaders",
            "regia_terrain/src/gpu",
            "regia_vfx/src/gpu",
            "regia_ui/shaders",
            "regia_fluid/src",
        ] {
            assert!(
                dirs.contains(&crates.join(must)),
                "{must} is not watched: {dirs:?}"
            );
        }
    }

    #[test]
    fn changing_any_engine_shader_source_changes_the_hash() {
        let mut sources = collect(&crates_dir());
        let base = hash(&sources);
        for i in 0..sources.len() {
            sources[i].bytes.push(b' ');
            assert_ne!(
                hash(&sources),
                base,
                "an edit to {} kept the hash",
                sources[i].rel
            );
            sources[i].bytes.pop();
        }
        assert_eq!(hash(&sources), base);
    }

    #[test]
    fn new_files_and_inline_wgsl_count_and_plain_rust_does_not() {
        let root = std::env::temp_dir().join(format!("regia-shader-sources-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, text).expect("write");
        };
        for name in SHADER_CRATES {
            write(&format!("{name}/src/lib.rs"), "pub fn f() {}\n");
        }
        write("regia_terrain/src/gpu/a.wgsl", "fn a() {}\n");
        let h0 = hash(&collect(&root));

        write("regia_terrain/src/gpu/a.wgsl", "fn a() { }\n");
        let h1 = hash(&collect(&root));
        assert_ne!(h0, h1, "an edited WGSL file");

        write("regia_ui/shaders/b.wgsl", "fn b() {}\n");
        let h2 = hash(&collect(&root));
        assert_ne!(h1, h2, "a new WGSL file");

        write("regia_vfx/src/lib.rs", "pub fn g() {}\n");
        assert_eq!(hash(&collect(&root)), h2, "plain Rust is not shader text");

        write(
            "regia_vfx/src/lib.rs",
            "const S: &str = \"@compute @workgroup_size(1) fn main() {}\";\n",
        );
        let h3 = hash(&collect(&root));
        assert_ne!(h2, h3, "inline WGSL in Rust");

        write("regia_ui/shaders/b.wgsl", "fn b() {}\r\n");
        let crlf = collect(&root);
        write("regia_ui/shaders/b.wgsl", "fn b() {}\n");
        assert_eq!(hash(&crlf), h3, "CRLF and LF checkouts hash alike");

        std::fs::remove_dir_all(root.join("regia_pt")).expect("rm");
        let missing = collect(&root);
        assert!(missing.iter().any(|s| s.rel == "regia_pt: missing"));
        assert_ne!(
            hash(&missing),
            h3,
            "a build without a shader crate does not match"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
