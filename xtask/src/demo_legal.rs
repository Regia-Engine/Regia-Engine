// SPDX-License-Identifier: MPL-2.0
//! `demo-notices`: the legal notices of the free showcase demo (ADR-3111, ADR-4847).
//!
//! The engine-wide `THIRD_PARTY_NOTICES.md` lists everything any shipped tool can link. A demo
//! download must list what *its own binary* links, so this module renders a notices file from the
//! resolved demo packaging tree ([`crate::graph::Graph::demo_tree`]: `showcase
//! --no-default-features --features demo`), plus the ported source files that live in crates of
//! that tree, plus three hand-written sections: upscaler SDK terms, the asset credits (pending the
//! content pack) and the privacy facts the packaged build must keep.
//!
//! It also enforces the demo privacy rules (offline, no crash upload, no telemetry upload):
//! [`privacy_problems`] fails when the tree contains a crate that can send data off the machine
//! (the crash uploader, an HTTP client, a TLS stack, a WebSocket or Noise handshake, the agent
//! protocol's UDP endpoint, the net transport, the Steamworks bindings, the DLSS SDK), and
//! [`verify_package`] checks a packaged demo folder (approved EULA, notices, no uploader strings in
//! the binaries).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::graph::Graph;
use crate::notices::{PortedSource, ResolvedNotice};

/// The package whose demo build's dependency tree is the demo's.
pub const DEMO_PACKAGE: &str = "showcase";

/// Where the generated demo notices live, relative to the workspace root.
pub const NOTICES_PATH: &str = "docs/demo/THIRD_PARTY_NOTICES-demo.md";

/// The EULA draft, relative to the workspace root.
pub const EULA_PATH: &str = "docs/demo/EULA-DRAFT.md";

/// Crates that can send data off the machine. None may be in the demo's dependency tree: the
/// demo is offline (no crash upload, no telemetry upload, no accounts, no network code of any
/// kind). The list fails closed: a crate is named here when it can open a socket or speak a wire
/// protocol, even if some of its uses are local.
pub const FORBIDDEN_IN_DEMO: &[(&str, &str)] = &[
    ("regia_crash", "the crash-report uploader"),
    (
        "regia_steam",
        "the Steamworks bindings (the demo ships without the overlay or API)",
    ),
    ("steamworks", "the Steamworks SDK bindings"),
    ("steamworks-sys", "the Steamworks SDK FFI"),
    (
        "regia_dlss",
        "the DLSS SDK shim (its licence needs a separate review)",
    ),
    ("regia_agent", "the agent protocol server (binds a UDP socket)"),
    ("regia_net", "the UDP net transport"),
    ("tungstenite", "a WebSocket client"),
    ("snow", "a Noise-protocol handshake"),
    ("ureq", "an HTTP client"),
    ("reqwest", "an HTTP client"),
    ("hyper", "an HTTP stack"),
    ("rustls", "a TLS stack"),
    ("native-tls", "a TLS stack"),
    ("openssl", "a TLS stack"),
];

/// The crate names [`verify_package`] looks for in a binary, one per entry of [`FORBIDDEN_IN_DEMO`], so a crate
/// added to the forbidden list cannot forget its scan. A name counts only as a trace a linked crate leaves
/// ([`linked_crate_trace`]), not as a word: the demo binary holds the weather preset `snow` and similar text.
pub fn forbidden_strings() -> Vec<&'static str> {
    FORBIDDEN_IN_DEMO.iter().map(|(name, _)| *name).collect()
}

/// Every start offset of `needle` in `bytes`.
fn occurrences<'a>(bytes: &'a [u8], needle: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    let first = needle.first().copied();
    bytes
        .windows(needle.len().max(1))
        .enumerate()
        .filter(move |(_, w)| Some(w[0]) == first && *w == needle)
        .map(|(i, _)| i)
}

/// The first trace in `bytes` that the crate `name` is linked (ADR-5419), or `None`. A linked crate leaves its source
/// paths in panic locations and its module path in `tracing` targets and type names, so the scan looks for:
///
/// - `/<name>-<digit>` or `\<name>-<digit>`: a crates.io source path (`…/snow-0.9.6/src/…`);
/// - `crates/<name>/` (either separator): a workspace crate's source path;
/// - `<module>::` with no identifier character or `:` before it (`<module>` is `name` with `-` as `_`): a module path.
///
/// A bare word (`snow` the weather preset, `hyperlink`, `regia_vfx::snow::`) is not a trace. The scan still fails
/// closed on text that names a forbidden crate's module path, such as an error message suggesting one.
#[must_use]
pub fn linked_crate_trace(bytes: &[u8], name: &str) -> Option<&'static str> {
    let sep = |b: Option<&u8>| matches!(b, Some(b'/' | b'\\'));
    for i in occurrences(bytes, name.as_bytes()) {
        let before = i.checked_sub(1).and_then(|j| bytes.get(j));
        let rest = &bytes[i + name.len()..];
        if sep(before) && rest.first() == Some(&b'-') && rest.get(1).is_some_and(u8::is_ascii_digit) {
            return Some("a crates.io source path");
        }
        if sep(before) && sep(rest.first()) && i >= 7 && &bytes[i - 7..i - 1] == b"crates" {
            return Some("a workspace source path");
        }
    }
    let module = name.replace('-', "_");
    for i in occurrences(bytes, module.as_bytes()) {
        let before = i.checked_sub(1).and_then(|j| bytes.get(j));
        let ident = before.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b':');
        if !ident && bytes[i + module.len()..].starts_with(b"::") {
            return Some("a module path");
        }
    }
    None
}

/// Names from the resolved tree that are on the forbidden list, one message each.
pub fn privacy_problems(tree: &Graph) -> Vec<String> {
    let names: BTreeSet<&str> = tree.packages.iter().map(|p| p.name.as_str()).collect();
    FORBIDDEN_IN_DEMO
        .iter()
        .filter(|(name, _)| names.contains(name))
        .map(|(name, what)| format!("the demo links `{name}` ({what}); the demo must stay offline"))
        .collect()
}

/// The workspace crate a repository-relative path belongs to (`crates/<name>/...`, `samples/<name>/...`).
fn crate_of(path: &str) -> Option<&str> {
    let mut parts = path.split('/');
    match parts.next()? {
        "crates" | "samples" => parts.next(),
        _ => None,
    }
}

/// The ported sources with at least one `files_to` entry inside a crate of `tree`, trimmed to those files.
pub fn ported_in_tree(tree: &Graph, ported: &[PortedSource]) -> Vec<PortedSource> {
    let crates: BTreeSet<&str> = tree.packages.iter().map(|p| p.name.as_str()).collect();
    ported
        .iter()
        .filter_map(|s| {
            let to: Vec<String> = s
                .files_to
                .iter()
                .filter(|f| crate_of(f).is_some_and(|c| crates.contains(c)))
                .cloned()
                .collect();
            (!to.is_empty()).then(|| PortedSource {
                files_to: to,
                ..s.clone()
            })
        })
        .collect()
}

/// The bundled-asset notices with at least one file inside a crate of `tree`, trimmed to those
/// files (ADR-8888): the demo ships the runtime UI's fonts, not the editor's or the manual's.
pub fn notices_in_tree(tree: &Graph, notices: &[ResolvedNotice]) -> Vec<ResolvedNotice> {
    let crates: BTreeSet<&str> = tree.packages.iter().map(|p| p.name.as_str()).collect();
    notices
        .iter()
        .filter_map(|n| {
            let files: Vec<String> = n
                .files
                .iter()
                .filter(|f| crate_of(f).is_some_and(|c| crates.contains(c)))
                .cloned()
                .collect();
            (!files.is_empty()).then(|| ResolvedNotice { files, ..n.clone() })
        })
        .collect()
}

/// The demo notices: header, crates table, ported sources, bundled fonts and assets, upscalers,
/// asset credits, privacy. Deterministic: same lockfile, same text.
pub fn render(tree: &Graph, ported: &[PortedSource], assets: &[ResolvedNotice]) -> String {
    let mut crates: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
    for reached in tree.reachable(0, |_, _| false).into_keys() {
        let dep = &tree.packages[reached];
        if dep.workspace {
            continue;
        }
        crates
            .entry((dep.name.clone(), dep.version.clone()))
            .or_insert_with(|| {
                (
                    dep.license
                        .clone()
                        .unwrap_or_else(|| "UNKNOWN (see crate)".into()),
                    dep.repository.clone().unwrap_or_default(),
                )
            });
    }
    let licences: BTreeSet<&str> = crates.values().map(|(l, _)| l.as_str()).collect();
    let mut out = String::new();
    out.push_str("# Third-party notices: Regia Engine showcase demo\n\n");
    out.push_str(
        "<!-- Generated by `cargo xtask demo-notices`. Do not edit by hand; CI checks it is up to date. -->\n\n\
         This demo is a free, offline download built with Regia Engine (Mozilla Public License 2.0, `licenses/LICENSE`). It is built\n\
         with the open-source crates below, each used under the licence shown. Where a crate offers a choice of\n\
         licences (`A OR B`), Regia Engine uses it under any one of them. The list is the dependency tree of the\n\
         demo build of the `showcase` program (`--no-default-features --features demo`: the agent protocol and\n\
         every network crate are compiled out) on Windows, Linux and both Android targets, not of the editor or\n\
         the tools; only crates that link into the binary are listed.\n\n\
         Fonts: the runtime UI's bundled default fonts (`crates/regia_ui/assets/fonts`) — Inter plus the Noto\n\
         Naskh Arabic, Noto Sans SC, Noto Sans KR, Noto Sans Hebrew, Noto Sans Devanagari, Noto Sans Thai and\n\
         Noto Color Emoji subsets — are licensed under the SIL Open Font License 1.1; the licence texts are kept\n\
         next to the font files in the engine's source (see the fonts README for the full list).",
    );
    if assets.is_empty() {
        out.push_str("\n\n");
    } else {
        out.push_str(
            " Each font and other\n\
             third-party file compiled into the demo is listed with its copyright lines under \"Bundled fonts and\n\
             assets\" below, followed by the licence text.\n\n",
        );
    }
    let _ = writeln!(
        out,
        "{} crates under {} licence expressions.\n",
        crates.len(),
        licences.len()
    );
    out.push_str("| Crate | Version | Licence | Source |\n|---|---|---|---|\n");
    for ((name, version), (licence, repo)) in &crates {
        let _ = writeln!(out, "| {name} | {version} | {licence} | {repo} |");
    }
    out.push_str(&crate::notices::render_ported_section(ported));
    out.push_str(&crate::notices::render_asset_notices_section(assets));
    out.push_str(UPSCALERS);
    out.push_str(ASSETS);
    out.push_str(PRIVACY);
    out
}

const UPSCALERS: &str = "\n## Upscalers\n\n\
The demo ships one in-tree upscaler: FidelityFX Super Resolution 3 (temporal upscaling), which is Regia Engine\n\
code written from the public FSR 3 description and the shader files listed under \"Ported source code\" above\n\
(AMD FidelityFX SDK, MIT licence, copyright Advanced Micro Devices, Inc.). The MIT notice above satisfies the\n\
SDK's redistribution terms; AMD's name is not used to endorse the demo.\n\n\
Not shipped, and therefore not covered by this file:\n\n\
| Upscaler | Status in the demo | Terms to review before it ships |\n|---|---|---|\n\
| NVIDIA DLSS Super Resolution | Not linked (the `regia_dlss` crate is not in the demo's tree). | NVIDIA RTX SDKs licence: the runtime library `nvngx_dlss.dll` / `libnvidia-ngx-dlss.so` may be redistributed unmodified with its licence file, and NVIDIA's trademark attribution text must appear. Owner and counsel confirm the current text of the licence before a build includes it. |\n\
| Intel XeSS | Not linked by default. `cargo xtask demo-package --xess-sdk <SDK>` (ADR-7820) builds the Windows bundle with the binding, ships Intel's unmodified `libxess.dll` with the SDK's `LICENSE.txt` and `third-party-programs.txt` as `INTEL-XESS-LICENSE.txt` and `INTEL-XESS-THIRD-PARTY-PROGRAMS.txt`, and adds the XeSS section to that bundle's notices. | Intel's XeSS SDK licence (redistribution of the runtime DLLs with the licence file, attribution). Counsel confirms the current text before a build includes it. |\n\n\
If either is added, this table becomes a notices section with the licence text of the redistributed library.\n";

const ASSETS: &str = "\n## Asset credits\n\n\
**Pending the content pack.** The showcase content pack (trees, rocks and kit pieces) is not delivered yet. Until\n\
it is, the demo ships only the scene Regia Engine generates itself (terrain, splat layers, sky, scatter from\n\
engine-made meshes), which has no third-party assets. When the pack lands, every asset gets a row (name, author,\n\
source URL, licence, the file it covers) in the scene's asset manifest, and this section is generated from that\n\
manifest. Until the packaging step enforces the manifest, the asset-licence check is a manual release-checklist\n\
item (`docs/showcase-plan.md`).\n";

const PRIVACY: &str = "\n## What the demo does not do\n\n\
The demo runs offline. It has no accounts, no analytics and no network upload of any kind: the crash-report\n\
uploader, every HTTP or TLS client, the agent protocol and the net transport are not part of the program\n\
(`cargo xtask demo-notices --check` fails if one enters its dependency tree). A crash writes a report file on\n\
your computer only; nothing is sent. The frame-time summaries the demo records stay in its own log folder.\n";

/// The section headings the approved (non-draft) EULA must keep. Section 9 carries the governing
/// law and contact details, so losing it is a problem too.
const EULA_SECTIONS: &[&str] = &[
    "## 1. What this is",
    "## 2. Your rights",
    "## 3. Videos, streaming and screenshots",
    "## 4. Privacy",
    "## 5. Third-party software",
    "## 6. No warranty",
    "## 7. Liability",
    "## 8. Termination",
    "## 9. General",
];

/// Problems with the EULA draft text: missing sections the owner decisions need.
pub fn eula_problems(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if !text.contains("DRAFT") {
        problems.push("the EULA must say DRAFT until the owner approves it".into());
    }
    for heading in EULA_SECTIONS {
        if !text.contains(heading) {
            problems.push(format!("the EULA lacks the heading `{heading}`"));
        }
    }
    if !text.contains("## Open points for the owner") {
        problems.push("the EULA lacks the heading `## Open points for the owner`".into());
    }
    problems
}

/// Problems with the EULA text *inside a package*: it must be the approved text, not the
/// repository draft. Rejects a draft (the word `DRAFT`) and unresolved owner brackets (`[...]`),
/// and requires every numbered section, including `## 9. General` (governing law, contact).
/// The repository draft itself fails this check on purpose: only the owner's approved text ships.
pub fn packaged_eula_problems(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if text.contains("DRAFT") {
        problems.push(
            "the packaged EULA is still the draft (it says DRAFT); ship the owner-approved text".into(),
        );
    }
    if text.contains('[') {
        problems.push(
            "the packaged EULA has unresolved `[...]` owner brackets; ship the owner-approved text".into(),
        );
    }
    for heading in EULA_SECTIONS {
        if !text.contains(heading) {
            problems.push(format!("the packaged EULA lacks the heading `{heading}`"));
        }
    }
    problems
}

/// Every file under `dir` whose file name is one of `names`, in sorted path order, with
/// unreadable directories and symlinked directories reported instead of skipped. Directory
/// symlinks are not followed (a zip-extracted package should not carry any; fail closed).
fn find_all(dir: &Path, names: &[&str], problems: &mut Vec<String>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(err) => {
                problems.push(format!("cannot read directory {}: {err}", d.display()));
                continue;
            }
        };
        for e in rd.flatten() {
            let p = e.path();
            let ft = match e.file_type() {
                Ok(ft) => ft,
                Err(err) => {
                    problems.push(format!("cannot stat {}: {err}", p.display()));
                    continue;
                }
            };
            if ft.is_symlink() {
                problems.push(format!(
                    "{} is a symlink; packages must not carry symlinks",
                    p.display()
                ));
                continue;
            }
            if ft.is_dir() {
                stack.push(p);
            } else if p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| names.is_empty() || names.contains(&n))
            {
                found.push(p);
            }
        }
    }
    found.sort();
    found
}

/// True when `p` is a file an uploader would be linked into: an executable or shared library.
/// Extensions are matched case-insensitively (a zip-extracted `.EXE` or `.DLL` loses mode bits and
/// keeps no exec bit, and `llvm-objdump`-style scans on Unix must still see Windows libraries);
/// extensionless files count only with the executable bit, which on Windows means `.exe` only.
fn is_binary(p: &Path) -> bool {
    match p.extension().and_then(|x| x.to_str()) {
        Some(ext) => matches!(ext.to_ascii_lowercase().as_str(), "exe" | "dll" | "so" | "dylib"),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                p.metadata().is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
    }
}

/// Checks a packaged demo folder: an approved EULA and the notices present (every copy of each,
/// in sorted order), the notices equal to `expected_notices`, and no executable or shared library
/// carrying a forbidden crate's string. Returns one message per problem.
pub fn verify_package(dir: &Path, expected_notices: &str) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let eulas = find_all(dir, &["EULA.txt", "EULA.md", "EULA-DRAFT.md"], &mut problems);
    if eulas.is_empty() {
        problems.push("the package has no EULA file".into());
    }
    for p in &eulas {
        let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
        if text.trim().is_empty() {
            problems.push(format!("{} is empty", p.display()));
        } else {
            for problem in packaged_eula_problems(&text) {
                problems.push(format!("{}: {problem}", p.display()));
            }
        }
    }
    let notices_files = find_all(
        dir,
        &["THIRD_PARTY_NOTICES.md", "THIRD_PARTY_NOTICES-demo.md"],
        &mut problems,
    );
    if notices_files.is_empty() {
        problems.push("the package has no THIRD_PARTY_NOTICES file".into());
    }
    for p in &notices_files {
        let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
        if text != expected_notices {
            problems.push(format!("{} is not the current demo notices", p.display()));
        }
    }
    let binaries = find_all(dir, &[], &mut problems);
    for p in &binaries {
        if !is_binary(p) {
            continue;
        }
        let bytes = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
        for needle in forbidden_strings() {
            if let Some(trace) = linked_crate_trace(&bytes, needle) {
                problems.push(format!(
                    "{} contains `{needle}` ({trace}): an uploader is linked",
                    p.display()
                ));
            }
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(extra: &[&str]) -> Graph {
        let mut g = Graph::default();
        g.add("showcase", "0.1.0", true);
        let r = g.add("regia_render", "0.1.0", true);
        g.link(0, r, false);
        let glam = g.add("glam", "0.33.12", false);
        g.packages[glam].license = Some("MIT OR Apache-2.0".into());
        g.link(r, glam, false);
        for n in extra {
            let i = g.add(n, "1.0.0", n.starts_with("regia_"));
            g.link(0, i, false);
        }
        g
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(dir.join(name), bytes).expect("test fixture write");
    }

    /// A packaged-EULA text that passes [`packaged_eula_problems`].
    fn approved_eula() -> String {
        let mut text = String::from("# EULA\n\n");
        for section in EULA_SECTIONS {
            let _ = writeln!(text, "{section}\n\nTerms.");
        }
        text
    }

    #[test]
    fn a_clean_tree_has_no_privacy_problems_and_every_forbidden_crate_is_one() {
        assert!(privacy_problems(&tree(&[])).is_empty());
        for (name, _) in FORBIDDEN_IN_DEMO {
            let p = privacy_problems(&tree(&[name]));
            assert_eq!(p.len(), 1, "{name}");
            assert!(p[0].contains(name));
        }
    }

    #[test]
    fn every_forbidden_crate_has_a_scan_string() {
        let strings = forbidden_strings();
        assert_eq!(strings.len(), FORBIDDEN_IN_DEMO.len());
        for (name, _) in FORBIDDEN_IN_DEMO {
            assert!(strings.contains(name), "{name}");
        }
    }

    #[test]
    fn notices_list_the_demo_tree_and_the_pending_sections() {
        let text = render(&tree(&[]), &[], &[]);
        assert!(text.contains("| glam | 0.33.12 | MIT OR Apache-2.0 |"));
        assert!(text.contains("--no-default-features --features demo"));
        assert!(
            !text.contains("| regia_render"),
            "workspace crates are not third-party"
        );
        assert!(text.contains("## Upscalers") && text.contains("NVIDIA DLSS") && text.contains("Intel XeSS"));
        assert!(text.contains("**Pending the content pack.**"));
        assert!(
            !text.contains("demo-package` already refuses"),
            "no unbuilt tooling is claimed"
        );
        assert!(text.contains("## What the demo does not do"));
        assert!(text.contains("Noto Sans Hebrew") && text.contains("Noto Sans Devanagari"));
        assert!(text.contains("Noto Sans Thai"));
        assert!(text.contains("Windows, Linux and both Android targets"));
        assert!(
            !text.contains("all three desktop"),
            "two desktop targets ship (macOS is testing-only)"
        );
        assert_eq!(text, render(&tree(&[]), &[], &[]), "deterministic");
    }

    #[test]
    fn ported_sources_are_trimmed_to_crates_in_the_tree() {
        let s = PortedSource {
            project: "P".into(),
            url: "https://example.org/p".into(),
            licence: "MIT".into(),
            copyright: "c".into(),
            commit: "abc".into(),
            files_from: vec!["a".into()],
            files_to: vec![
                "crates/regia_render/a.rs".into(),
                "crates/regia_editor/b.rs".into(),
            ],
        };
        let kept = ported_in_tree(&tree(&[]), std::slice::from_ref(&s));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].files_to, vec!["crates/regia_render/a.rs".to_string()]);
        let editor_only = PortedSource {
            files_to: vec!["crates/regia_editor/b.rs".into()],
            ..s
        };
        assert!(ported_in_tree(&tree(&[]), &[editor_only]).is_empty());
    }

    /// ADR-8888: the demo's notices carry the bundled fonts its crates compile in (the runtime
    /// UI's), with copyright lines and the OFL text, and not the editor's.
    #[test]
    fn demo_notices_list_only_the_fonts_its_crates_compile_in() {
        let notice = |name: &str, file: &str| ResolvedNotice {
            name: name.into(),
            licence: "OFL-1.1".into(),
            copyright: vec![format!("Copyright 2026 The {name} Project Authors")],
            files: vec![file.into()],
            licence_file: Some(format!("{file}.txt")),
            ported: None,
        };
        let all = [
            notice("Inter", "crates/regia_ui/assets/fonts/Inter-Regular.ttf"),
            notice("Geist", "crates/regia_editor/assets/fonts/Geist-wght.ttf"),
            notice("Manual Mono", "book/theme/fonts/Mono.woff2"),
        ];
        let g = tree(&["regia_ui"]);
        let kept = notices_in_tree(&g, &all);
        assert_eq!(kept, vec![all[0].clone()]);
        let text = render(&g, &[], &kept);
        assert!(text.contains("### Inter\n") && text.contains("Copyright 2026 The Inter Project Authors"));
        assert!(text.contains("under \"Bundled fonts and\nassets\" below"));
        assert!(!text.contains("Geist"));
        assert_eq!(text.matches("SIL OPEN FONT LICENSE Version 1.1").count(), 1);
    }

    #[test]
    fn the_repository_eula_draft_has_every_section() {
        let text = include_str!("../../docs/demo/EULA-DRAFT.md");
        assert_eq!(eula_problems(text), Vec::<String>::new());
        assert_eq!(eula_problems("# nothing").len(), 11);
    }

    #[test]
    fn a_packaged_eula_must_be_the_approved_text() {
        assert!(packaged_eula_problems(&approved_eula()).is_empty());
        let draft = include_str!("../../docs/demo/EULA-DRAFT.md");
        let p = packaged_eula_problems(draft);
        assert!(p.iter().any(|m| m.contains("DRAFT")), "{p:?}");
        assert!(p.iter().any(|m| m.contains('[')), "{p:?}");
        // A non-empty stub with no sections is not an EULA either.
        assert!(!packaged_eula_problems("terms").is_empty());
        // Dropping section 9 (governing law, contact) is a problem.
        let no_nine = approved_eula().replace("## 9. General", "## 9. Misc");
        assert!(
            packaged_eula_problems(&no_nine)
                .iter()
                .any(|m| m.contains("9. General"))
        );
    }

    #[test]
    fn verify_package_finds_missing_files_and_linked_uploaders() -> Result<()> {
        let dir = regia_test_utils::unique_temp_dir("demo_legal");
        let notices = "notices\n";
        let missing = verify_package(&dir, notices)?;
        assert_eq!(missing.len(), 2, "{missing:?}");
        write(&dir, "EULA.txt", approved_eula().as_bytes());
        write(&dir, "THIRD_PARTY_NOTICES.md", notices.as_bytes());
        assert_eq!(verify_package(&dir, notices)?.len(), 0);
        // A draft EULA must not pass as the shipped one.
        write(&dir, "EULA.txt", include_bytes!("../../docs/demo/EULA-DRAFT.md"));
        assert!(!verify_package(&dir, notices)?.is_empty());
        write(&dir, "EULA.txt", approved_eula().as_bytes());
        // Shared libraries are scanned without any execute bit, case-insensitively. The upper-case
        // one has its own name: on a case-insensitive file system (APFS, NTFS) `Game.DLL` would
        // replace `game.dll` and leave three files (ADR-5860).
        for lib in ["game.dll", "libgame.so", "Engine.DLL", "game.dylib"] {
            write(&dir, lib, b"MZ....crates/regia_crash/src/upload.rs....");
        }
        let p = verify_package(&dir, notices)?;
        assert_eq!(p.len(), 4, "{p:?}");
        assert!(p.iter().all(|m| m.contains("regia_crash")), "{p:?}");
        for lib in ["game.dll", "libgame.so", "Engine.DLL", "game.dylib"] {
            std::fs::remove_file(dir.join(lib))?;
        }
        // A network crate string in a library is caught too.
        write(
            &dir,
            "net.dll",
            b"PE....registry\\src\\index\\tungstenite-0.26.2\\src\\handshake....",
        );
        let p = verify_package(&dir, notices)?;
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("tungstenite"));
        std::fs::remove_file(dir.join("net.dll"))?;
        // A stale nested copy of the notices is found even after the current one, in sorted order.
        std::fs::create_dir_all(dir.join("nested"))?;
        write(&dir, "nested/THIRD_PARTY_NOTICES.md", "old".as_bytes());
        let p = verify_package(&dir, notices)?;
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("nested"), "{p:?}");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verify_package_scans_extensionless_executables() -> Result<()> {
        let dir = regia_test_utils::unique_temp_dir("demo_legal");
        write(&dir, "EULA.txt", approved_eula().as_bytes());
        write(&dir, "THIRD_PARTY_NOTICES.md", "notices\n".as_bytes());
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("showcase");
        write(
            &dir,
            "showcase",
            b"\x7fELF....snow::handshakestate::HandshakeState....",
        );
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
        let p = verify_package(&dir, "notices\n")?;
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("snow"));
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// ADR-5419: a forbidden crate's name as a word (the weather preset `snow`, `hyperlink`, a module of another
    /// crate) is not a trace; its source paths and module paths are, in every form a build leaves.
    #[test]
    fn only_traces_of_a_linked_crate_count_not_words() {
        let clean: &[u8] = b"\0snow\0blizzard\0set_hyperlink\0regia_vfx::snow::Flakes\0weather/snow/a.png\0\
            the steamworks feature is off\0crates/regia_vfx/src/snow.rs\0";
        for (name, _) in FORBIDDEN_IN_DEMO {
            assert_eq!(linked_crate_trace(clean, name), None, "{name}");
        }
        for (name, _) in FORBIDDEN_IN_DEMO {
            let module = name.replace('-', "_");
            for (text, trace) in [
                (
                    format!("/home/u/.cargo/registry/src/index.crates.io-1/{name}-1.2.3/src/lib.rs"),
                    "a crates.io source path",
                ),
                (
                    format!("C:\\cargo\\registry\\src\\{name}-0.1.0\\src\\lib.rs"),
                    "a crates.io source path",
                ),
                (
                    format!("/src/crates/{name}/src/lib.rs"),
                    "a workspace source path",
                ),
                (format!("crates\\{name}\\src\\lib.rs"), "a workspace source path"),
                (format!("<{module}::Server as Plugin>"), "a module path"),
            ] {
                assert_eq!(
                    linked_crate_trace(text.as_bytes(), name),
                    Some(trace),
                    "{name}: {text}"
                );
            }
        }
    }
}
