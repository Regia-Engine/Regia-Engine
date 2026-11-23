// SPDX-License-Identifier: MPL-2.0
//! Every optional `regia` feature must be forwarded by `regia_dylib`, so the shared engine copy
//! can be built with the same feature list as the launcher. The `default` list must equal
//! `regia`'s, and the `regia` dependency must take its defaults off (otherwise
//! `--no-default-features --features server` still builds the client stack).

use std::collections::BTreeMap;

/// Removes an end-of-line `#` comment (outside quotes).
fn strip_comment(line: &str) -> String {
    let mut in_str = false;
    let mut out = String::new();
    for c in line.chars() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => break,
            _ => {}
        }
        out.push(c);
    }
    out
}

/// Parses the `[features]` table of a manifest into name -> array items. Handles comments and
/// arrays that continue over several lines.
fn features(manifest: &str) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let mut in_features = false;
    let mut pending: Option<(String, String)> = None;
    for raw in manifest.lines() {
        let t = strip_comment(raw);
        let t = t.trim();
        if let Some((name, mut value)) = pending.take() {
            value.push_str(t);
            if t.contains(']') {
                out.insert(name, items(&value));
            } else {
                pending = Some((name, value));
            }
            continue;
        }
        if t.starts_with('[') {
            in_features = t == "[features]";
            continue;
        }
        if !in_features || t.is_empty() {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            let k = k.trim();
            if k.is_empty() || k.contains(' ') {
                continue;
            }
            if v.contains(']') {
                out.insert(k.to_owned(), items(v));
            } else {
                pending = Some((k.to_owned(), v.to_owned()));
            }
        }
    }
    out
}

/// Quoted items of an array value.
fn items(value: &str) -> Vec<String> {
    value.split('"').skip(1).step_by(2).map(str::to_owned).collect()
}

#[test]
fn forwards_every_regia_feature() {
    let regia = include_str!("../../regia/Cargo.toml");
    let dylib = include_str!("../Cargo.toml");
    let regia_features = features(regia);
    let dylib_features = features(dylib);
    assert!(
        regia_features.contains_key("physics2d"),
        "parser must find the regia features"
    );
    for name in regia_features.keys().filter(|k| *k != "default") {
        let want = format!("regia/{name}");
        assert!(
            dylib_features.get(name).is_some_and(|v| v.contains(&want)),
            "regia_dylib must forward feature `{name}` as \"{want}\""
        );
    }
}

#[test]
fn default_features_mirror_regia() {
    let regia = features(include_str!("../../regia/Cargo.toml"));
    let dylib = features(include_str!("../Cargo.toml"));
    let mut want = regia.get("default").cloned().expect("regia has a default list");
    let mut got = dylib
        .get("default")
        .cloned()
        .expect("regia_dylib must declare a `default` feature mirroring regia's");
    want.sort();
    got.sort();
    assert_eq!(got, want, "regia_dylib `default` must equal regia's");
}

#[test]
fn regia_dependency_has_default_features_off() {
    let dylib = include_str!("../Cargo.toml");
    let line = dylib
        .lines()
        .map(strip_comment)
        .find(|l| l.trim_start().starts_with("regia = "))
        .expect("regia dependency line");
    assert!(
        line.replace(' ', "").contains("default-features=false"),
        "the regia dependency must set default-features = false: {line}"
    );
}

#[test]
fn parser_handles_comments_and_multiline_arrays() {
    let m = "[features]\nserver = [] # \"regia/server\"\nclient = [\n  \"regia/client\", # c\n  \"x\",\n]\n[other]\nz = [\"q\"]\n";
    let f = features(m);
    assert!(f["server"].is_empty(), "comment text is not a value");
    assert_eq!(f["client"], ["regia/client", "x"]);
    assert!(!f.contains_key("z"));
}
