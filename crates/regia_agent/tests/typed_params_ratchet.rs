// SPDX-License-Identifier: MPL-2.0
//! ADR-2622 ratchet: no new `.get(k).and_then(Value::as_*)` parameter reads.
//!
//! That chain reads a present value of the wrong JSON type as an absent one, so `{"count": "5"}`
//! silently used the default. Handlers read optional parameters through `crate::params`
//! (`opt_str`, `opt_bool`, `opt_u64`, `opt_i64`, `opt_f64`, `opt_array`, `opt_object`), which answer
//! `invalid_params` with `data.param` and `data.expected`. This test scans the non-test code of
//! `src/**/*.rs` for the chain and fails on any site that is not in [`ALLOWED`]. The count per file
//! must match exactly, so a fixed site also has to leave the list.
//!
//! A chain followed directly by `.ok_or`/`.ok_or_else` is a required read (absent and wrong-typed
//! both refuse) and is not counted. Code after the first `#[cfg(test)]` and `*_tests.rs` files are
//! test code and are not scanned.

use std::path::{Path, PathBuf};

/// `(file under src/, number of sites, why the chain is correct there)`.
const ALLOWED: &[(&str, usize, &str)] = &[
    (
        "build_game.rs",
        9,
        "reads cargo's own `--message-format json` lines (reason, success, compiler messages and spans), not request parameters",
    ),
    (
        "runtime.rs",
        3,
        "`[x, y, z]` / `{x, y, z}` / quaternion component readers: a missing or wrong-typed component is refused through `ok_or_else(invalid_params)`",
    ),
    (
        "biome.rs",
        5,
        "reads the agent's own accumulated stats and layer records, not request parameters",
    ),
    (
        "camera_framework.rs",
        2,
        "tries the `{yaw, pitch}` object shape; a miss falls through to `bad()` which refuses",
    ),
    (
        "dispatch.rs",
        12,
        "the edit-gate flags `dry_run` / `applied` (the method handlers validate `dry_run` themselves) and the audit summary text of each authored change, which only describes the call",
    ),
    (
        "loading.rs",
        1,
        "each side is range-checked; a wrong type fails the `match` and is refused with the offscreen message",
    ),
    (
        "mcp_http.rs",
        2,
        "reads the JSON-RPC envelope (`jsonrpc`, `method`) before dispatch; a mismatch is an invalid request",
    ),
    (
        "nav.rs",
        1,
        "`{x, y, z}` object reader whose misses are refused through `ok_or_else(invalid)`",
    ),
    (
        "physics.rs",
        1,
        "`{x, y, z}` object reader whose misses are refused through `ok_or_else(invalid)`",
    ),
    (
        "post_volumes.rs",
        1,
        "range-filtered; a miss is refused by the caller's `ok_or_else`",
    ),
    (
        "render_out/time_map.rs",
        2,
        "key `frame` and `scale` are required: a wrong type is refused through `ok_or_else`",
    ),
    (
        "scene.rs",
        6,
        "three compare reads over the batch's earlier edits (`== Some(..)` is false for a wrong type), two reads of an edit's `parent` for the planned-despawn check (a wrong type names no parent there; each edit's own reader validates it when it applies) and a `{x, y, z}` reader",
    ),
    (
        "server.rs",
        1,
        "the `timeout_ms` hint is read before dispatch, where no error can be answered; a wrong type means the default",
    ),
    (
        "spatial_query.rs",
        1,
        "`band` falls through to `ok_or_else(bad_param)`",
    ),
    (
        "sprite2d.rs",
        2,
        "`world` coordinates and `x`/`y` cells: a miss is refused with the `give x and y` message",
    ),
    (
        "ui.rs",
        4,
        "reads the renderer's own answer (`finish_preview`), not request parameters",
    ),
    (
        "vfx.rs",
        1,
        "`{x, y, z}` object reader whose misses are refused through `ok_or_else(invalid)`",
    ),
    (
        "video.rs",
        1,
        "`region` fields: a miss is refused with the `{ x, y, w, h }` message",
    ),
    (
        "ai_squads.rs",
        1,
        "`roles[].name` is required: a miss or a wrong type is refused through `ok_or_else(invalid)` after the empty check",
    ),
    (
        "assets.rs",
        4,
        "reads the cook's own `font_subsets.json` report (`assets.font_subset`), not request parameters",
    ),
    (
        "ui_inspect.rs",
        1,
        "`x` and `y` are required: a miss or a wrong type is refused through `ok_or_else(bad_param)`",
    ),
    (
        "world_streaming.rs",
        1,
        "reads the `material` id of the HLOD report row this handler just built, not request parameters",
    ),
    (
        "configurator.rs",
        1,
        "echoes the checked document's own `schema` id in the answer; the document is validated by `regia_configurator::check_bytes`, not read as a parameter",
    ),
    (
        "golden_tools.rs",
        1,
        "compares the finished golden job's own result (`golden.name`), not request parameters",
    ),
    (
        "import_report.rs",
        4,
        "`path` is required (a miss, a wrong type or an empty string is refused through `ok_or_else(bad_param)`); the other three read the cook's own import report file, not request parameters",
    ),
    (
        "net_server_persistence.rs",
        1,
        "`confirm` must be exactly `true`: any other value or type is refused with `F0002`",
    ),
    (
        "plates.rs",
        2,
        "a shot camera's `name` selects the named form, and anything else must be a full `{eye, target}` pose (refused when it is not); `shots_file` falls through to the `give shot or shots_file` refusal",
    ),
    (
        "procgen_wfc.rs",
        2,
        "`tiles[i].sockets` is refused through `ok_or_else(bad)` unless it is four strings; `tiles_have_sockets` only picks the model kind and the tile reader refuses a wrong-typed `tiles`",
    ),
    (
        "ragdoll_active.rs",
        1,
        "`impulse` is required: a miss, a wrong type or a bad length is refused through `ok_or_else(invalid)`",
    ),
    (
        "reflection_captures.rs",
        1,
        "`file` is required: a miss, a wrong type or an empty string is refused through `ok_or_else(refused)`",
    ),
    (
        "render_brdf_energy.rs",
        1,
        "a missing or wrong-typed `mode` reads as no name and is refused as an unknown model (`brdf_energy_bad_mode`, pinned by its unit test)",
    ),
    (
        "viewer.rs",
        1,
        "`path` is required: a miss or a wrong type is refused with `invalid_params` in the `else` branch",
    ),
    (
        "viewer_view_pack.rs",
        1,
        "`dir` is required: a miss or a wrong type is refused with `invalid_params` in the `else` branch",
    ),
    (
        "det_projectiles.rs",
        1,
        "`geometry.spheres[].body` is required: a miss, a wrong type or an id over u32 is refused through `ok_or_else(bad)`",
    ),
    (
        "ai_reasoners.rs",
        1,
        "`team` is required: a miss, a wrong type or a value over 255 is refused with `unknown_team` in the `else` branch",
    ),
    (
        "assets_push.rs",
        1,
        "`data` is required: a miss or a wrong type is refused with `push_bad_data` in the `else` branch",
    ),
    (
        "diet_report.rs",
        1,
        "reads the `version` of a diet report file the method loaded, not a request parameter; a miss is refused as `diet_report_unreadable`",
    ),
    (
        "playtest.rs",
        1,
        "counts `key` events for the time-control stats after every event's `type` was checked (a missing or wrong-typed type is refused above)",
    ),
    (
        "unit_view.rs",
        1,
        "`units` is required: a miss or a wrong type is refused through `bad` in the `else` branch",
    ),
    (
        "geometry_probe.rs",
        1,
        "`radius`, `height`, `step_up` and `step_down` are required: a miss, a wrong type or a non-finite value is refused through `ok_or_else(bad)`",
    ),
    (
        "terrain_edit_layers.rs",
        1,
        "`channels` is required: a miss or a wrong type is refused with `invalid_params` in the `else` branch",
    ),
    (
        "terrain_scatter_colliders.rs",
        1,
        "`height` of an extent is required: a miss or a wrong type is refused through `ok_or_else(bad)`",
    ),
];

/// Files under `src/` that read every parameter, required ones too, through `crate::params`, so a
/// wrong-typed value is refused naming `data.param` and `data.expected` (ADR-2622, ADR-10129). Any
/// `.get(k).and_then(Value::as_*)` chain in them fails, the `.ok_or` form included.
const STRICT: &[&str] = &["locomotion.rs"];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = read.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Source with `//` comments removed and all whitespace dropped, so a rustfmt-wrapped chain is
/// one string. Each kept byte remembers its original line.
fn squeeze(text: &str) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut lines = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let code = match line.find("//") {
            Some(i) if !line[..i].contains('"') => &line[..i],
            _ => line,
        };
        for c in code.chars().filter(|c| !c.is_whitespace()) {
            for _ in 0..c.len_utf8() {
                lines.push(n + 1);
            }
            out.push(c);
        }
    }
    (out, lines)
}

/// The part of `text` before the first `#[cfg(test)] mod`. A test-only helper function in the middle
/// of a file does not end the scan.
fn non_test_code(text: &str) -> &str {
    for (at, marker) in text.match_indices("#[cfg(test)]") {
        let rest = text[at + marker.len()..].trim_start();
        let rest = rest.strip_prefix("pub").map_or(rest, |r| {
            let r = r.trim_start();
            r.strip_prefix("(crate)").unwrap_or(r).trim_start()
        });
        if rest.starts_with("mod ") {
            return &text[..at];
        }
    }
    text
}

/// Lines of every flagged chain in `text`.
fn flagged(text: &str) -> Vec<usize> {
    scan(text, false)
}

/// Lines of every chain in `text`, the required `.ok_or` form included ([`STRICT`] files).
fn any_chain(text: &str) -> Vec<usize> {
    scan(text, true)
}

fn scan(text: &str, required_too: bool) -> Vec<usize> {
    let code = non_test_code(text);
    let (flat, lines) = squeeze(code);
    let mut found = Vec::new();
    for marker in [").and_then(Value::as_", ").and_then(serde_json::Value::as_"] {
        for (at, _) in flat.match_indices(marker) {
            let before = &flat[..at];
            // `.get(<no parentheses>` directly before the `)`.
            let Some(open) = before.rfind(".get(") else {
                continue;
            };
            if before[open + 5..].contains(['(', ')']) {
                continue;
            }
            // Only the string-keyed form reads a parameter; `a.get(1)` indexes an array.
            if before[open + 5..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
            {
                continue;
            }
            let kind_end = flat[at + marker.len()..]
                .find(')')
                .map_or(flat.len(), |i| at + marker.len() + i + 1);
            if !required_too && flat[kind_end..].starts_with(".ok_or") {
                continue;
            }
            found.push(lines[open]);
        }
    }
    found.sort_unstable();
    found
}

#[test]
fn optional_parameters_are_read_through_typed_helpers() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    assert!(
        files.len() > 100,
        "found only {} source files under {}",
        files.len(),
        src.display()
    );
    let mut problems = Vec::new();
    let mut seen_allowed = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(&src)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        if rel.ends_with("_tests.rs") || rel == "params.rs" || rel.starts_with("fuzz_targets/") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let lines = flagged(&text);
        let allowed = ALLOWED.iter().find(|(f, _, _)| *f == rel);
        match (lines.len(), allowed) {
            (0, None) => {}
            (n, Some((_, want, _))) if n == *want => seen_allowed.push(rel),
            (n, Some((_, want, _))) => problems.push(format!(
                "{rel}: {n} site(s) at lines {lines:?}, the allow-list says {want}"
            )),
            (n, None) => problems.push(format!(
                "{rel}: {n} `.get(k).and_then(Value::as_*)` site(s) at lines {lines:?}: read the parameter with \
                 crate::params::opt_str/opt_bool/opt_u64/opt_i64/opt_f64/opt_array/opt_object (wrong type -> invalid_params), \
                 or add the file to ALLOWED with the reason"
            )),
        }
    }
    for (file, _, _) in ALLOWED {
        if !seen_allowed.iter().any(|s| s == file) && !problems.iter().any(|p| p.starts_with(file)) {
            problems.push(format!("{file}: in ALLOWED but no site found; remove the entry"));
        }
    }
    assert!(
        problems.is_empty(),
        "typed-parameter ratchet (ADR-2622):\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_scanner_sees_wrapped_and_plain_chains() {
    let plain = "fn f(p: &Value) { let a = p.get(\"k\").and_then(Value::as_str); }";
    assert_eq!(flagged(plain), vec![1]);
    let wrapped =
        "fn f(p: &Value) {\n let a = p\n .get(\"k\")\n .and_then(Value::as_u64)\n .unwrap_or(1);\n}";
    assert_eq!(flagged(wrapped), vec![3]);
    let required = "let a = p.get(\"k\").and_then(Value::as_str).ok_or_else(|| e())?;";
    assert!(flagged(required).is_empty());
    let index = "let a = arr.get(1).and_then(Value::as_f64);";
    assert!(flagged(index).is_empty());
    let helper = "#[cfg(test)]\nfn helper() {}\nfn f(p: &Value) { p.get(\"k\").and_then(Value::as_str); }";
    assert_eq!(flagged(helper), vec![3]);
    let test_code =
        "fn f() {}\n#[cfg(test)]\nmod t { fn g(p: &Value) { p.get(\"k\").and_then(Value::as_str); } }";
    assert!(flagged(test_code).is_empty());
    let comment = "// p.get(\"k\").and_then(Value::as_str)\nfn f() {}";
    assert!(flagged(comment).is_empty());
}

#[test]
fn strict_files_read_required_parameters_through_typed_helpers() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut problems = Vec::new();
    for rel in STRICT {
        let file = src.join(rel);
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("{} does not read: {e}", file.display())); // invariant: test-only; a STRICT entry must name a real file
        let lines = any_chain(&text);
        if !lines.is_empty() {
            problems.push(format!(
                "{rel}: `.get(k).and_then(Value::as_*)` at lines {lines:?}: read it with crate::params::opt_* \
                 and refuse the miss with the method's code plus data.param and data.expected"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "strict typed-parameter files (ADR-2622, ADR-10129):\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_strict_scan_counts_required_reads() {
    let required = "let a = p.get(\"k\").and_then(Value::as_str).ok_or_else(|| e())?;";
    assert_eq!(any_chain(required), vec![1]);
    assert!(flagged(required).is_empty());
}
