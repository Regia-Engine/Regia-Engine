// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): the key-manifest scanner on Fluent and RON text it does not see
//! in the happy path: serializer-style layouts, odd spacing, comments, raw strings, escapes,
//! multi-byte text, unterminated input and input order. No goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_ui::loc_manifest::{GatherSource, ReferenceKind, gather, is_valid_key};

fn codes(g: &regia_ui::loc_manifest::Gathered) -> Vec<&'static str> {
    g.diagnostics.iter().map(|d| d.code).collect()
}

#[test]
fn a_closing_brace_in_column_one_belongs_to_its_message() {
    // The Fluent serializer writes select expressions this way (the `pseudo` tool's output).
    let text = "items = { $count ->\n    [one] One\n   *[other] Many\n}\nafter = x\n";
    let g = gather(&[GatherSource::ftl("en/a.ftl", "en", text)]);
    assert!(g.diagnostics.is_empty(), "{:?}", g.diagnostics);
    let keys: Vec<&str> = g.manifest.keys.keys().map(String::as_str).collect();
    assert_eq!(keys, ["after", "items"]);
    let items = &g.messages["en"][0];
    assert_eq!(
        items.block,
        "items = { $count ->\n    [one] One\n   *[other] Many\n}"
    );
    assert_eq!(items.variables.iter().collect::<Vec<_>>(), ["count"]);
    assert_eq!(g.messages["en"][1].at.line, 5);
    // A stray brace with no open message is still reported.
    let g = gather(&[GatherSource::ftl("en/b.ftl", "en", "}\nok = 1\n")]);
    assert_eq!(codes(&g), ["ftl_syntax"]);
    assert_eq!(g.manifest.keys.len(), 1);
}

#[test]
fn message_heads_accept_loose_spacing_and_reject_bad_ids() {
    let text = "a=1\nb  =  2\nC-d_9 = 3\nempty =\n9bad = 4\n  stray = 5\nx y = 6\n";
    let g = gather(&[GatherSource::ftl("a.ftl", "en", text)]);
    let keys: Vec<&str> = g.manifest.keys.keys().map(String::as_str).collect();
    assert_eq!(keys, ["C-d_9", "a", "b", "empty"]);
    let by_key = |k: &str| g.messages["en"].iter().find(|m| m.key == k).unwrap();
    assert!(by_key("a").has_value && by_key("b").has_value);
    assert!(!by_key("empty").has_value, "nothing after the equals sign");
    let bad_lines: Vec<u32> = g
        .diagnostics
        .iter()
        .filter_map(|d| d.at.as_ref().map(|a| a.line))
        .collect();
    assert_eq!(
        bad_lines,
        [5, 7],
        "9bad and `x y` are reported; the indented stray line is a continuation of nothing"
    );
}

#[test]
fn continuation_lines_blank_lines_tabs_and_attributes() {
    let text = "a =\n\n    value after a blank\n\n\nb = 1\n\tmore\nc =\n    .label = Go\n    .title = T\n";
    let g = gather(&[GatherSource::ftl("a.ftl", "en", text)]);
    assert!(g.diagnostics.is_empty(), "{:?}", g.diagnostics);
    let m = &g.messages["en"];
    assert_eq!(
        m[0].block, "a =\n\n    value after a blank",
        "inner blank lines kept, trailing ones dropped"
    );
    assert!(m[0].has_value);
    assert_eq!(m[1].block, "b = 1\n\tmore");
    assert!(!m[2].has_value, "only attributes");
    assert!(m[2].block.contains(".title"));
}

#[test]
fn terms_comments_and_the_first_line_bom_are_not_messages() {
    let text = "\u{FEFF}### resource comment\n## group\n# note\n-term = T\n    more of the term\nreal = r\n";
    let g = gather(&[GatherSource::ftl("a.ftl", "en", text)]);
    assert!(g.diagnostics.is_empty(), "{:?}", g.diagnostics);
    assert_eq!(g.manifest.keys.keys().collect::<Vec<_>>(), ["real"]);
}

#[test]
fn an_empty_locale_tag_is_reported_and_its_messages_are_not_gathered() {
    let g = gather(&[GatherSource::ftl("x.ftl", "  ", "a = 1\n")]);
    assert_eq!(codes(&g), ["empty_locale"]);
    assert!(g.manifest.keys.is_empty() && g.manifest.locales.is_empty() && g.messages.is_empty());
}

const RON: &str = "(\n  a: Loc(\"menu-a\"),\n  b: \"loc:menu-b\",\n  // c: \"loc:commented\",\n  /* d: Loc(\"block\") */\n  e: r#\"loc:raw-key\"#,\n  f: \"plain\",\n  g: Loc( \"spaced\" ),\n  h: \"loc:bad key\",\n  i: Loc(\"also bad!\"),\n  j: MyLoc(\"not-a-call\"),\n)";

#[test]
fn ron_references_have_exact_positions_and_comments_hide_keys() {
    let g = gather(&[GatherSource::ui_asset("ui/menu.ui.ron", RON)]);
    let refs: Vec<(&str, u32, u32)> = g
        .manifest
        .keys
        .iter()
        .flat_map(|(k, e)| {
            e.references
                .iter()
                .map(move |r| (k.as_str(), r.at.line, r.at.column))
        })
        .collect();
    assert_eq!(
        refs,
        [
            ("menu-a", 2, 10),
            ("menu-b", 3, 6),
            ("raw-key", 6, 6),
            ("spaced", 8, 11)
        ],
        "block and line comments, plain strings and MyLoc( are not references"
    );
    assert_eq!(codes(&g), ["invalid_key", "invalid_key"]);
    let bad: Vec<(u32, u32)> = g
        .diagnostics
        .iter()
        .filter_map(|d| d.at.as_ref().map(|a| (a.line, a.column)))
        .collect();
    assert_eq!(bad, [(9, 6), (10, 10)]);
    assert!(
        g.manifest
            .keys
            .values()
            .all(|e| e.references.iter().all(|r| r.kind == ReferenceKind::Ui))
    );
    let scene = gather(&[GatherSource::scene("s.ron", "(name: \"loc:title\")")]);
    assert_eq!(
        scene.manifest.keys["title"].references[0].kind,
        ReferenceKind::Scene
    );
    // A bare string is only a key after `Loc(`; `"loc:` works anywhere.
    let bare = gather(&[GatherSource::ui_asset(
        "u.ron",
        "(a: \"title\", b: Loc(\"real\"))",
    )]);
    assert_eq!(bare.manifest.keys.keys().collect::<Vec<_>>(), ["real"]);
}

#[test]
fn ron_comments_nest_and_unterminated_input_is_reported() {
    let nested = "/* a /* b */ Loc(\"hidden\") */ Loc(\"seen\")";
    let g = gather(&[GatherSource::ui_asset("n.ron", nested)]);
    assert_eq!(g.manifest.keys.keys().collect::<Vec<_>>(), ["seen"]);
    assert!(g.diagnostics.is_empty());
    let g = gather(&[GatherSource::ui_asset("u.ron", "(a: \"open")]);
    assert_eq!(codes(&g), ["unterminated_string"]);
    let at = g.diagnostics[0].at.as_ref().unwrap();
    assert_eq!((at.line, at.column), (1, 5));
    let g = gather(&[GatherSource::ui_asset("c.ron", "(\n/* never closed Loc(\"x\")")]);
    assert_eq!(codes(&g), ["unterminated_comment"]);
    let at = g.diagnostics[0].at.as_ref().unwrap();
    assert_eq!((at.line, at.column), (2, 1));
    assert!(g.manifest.keys.is_empty(), "nothing inside an unclosed comment");
    // Raw strings with hashes and an unclosed raw string.
    let g = gather(&[GatherSource::ui_asset("r.ron", "(a: r##\"loc:ok\"#still\"##)")]);
    assert!(
        g.manifest.keys.is_empty(),
        "`ok\"#still` is not a valid key: {:?}",
        g.diagnostics
    );
    assert_eq!(codes(&g), ["invalid_key"]);
    let g = gather(&[GatherSource::ui_asset("r.ron", "(a: r#\"never")]);
    assert_eq!(codes(&g), ["unterminated_string"]);
    // The empty file and a lone quote do not panic.
    for text in ["", "\"", "r", "r#", "/", "/*", "Loc(", "Loc(\"", "\\"] {
        let _ = gather(&[GatherSource::ui_asset("e.ron", text)]);
    }
}

#[test]
fn positions_count_characters_not_bytes_and_escapes_do_not_end_strings() {
    let g = gather(&[GatherSource::ui_asset(
        "m.ron",
        "(\u{f1}: \"\u{65e5}\u{672c}\", t: Loc(\"ok\"))",
    )]);
    let r = &g.manifest.keys["ok"].references[0];
    assert_eq!((r.at.line, r.at.column), (1, 18));
    let g = gather(&[GatherSource::ui_asset(
        "e.ron",
        "(a: \"loc:esc\\\"aped\", b: Loc(\"after\"))",
    )]);
    assert_eq!(
        codes(&g),
        ["invalid_key"],
        "the escaped quote stays inside the string"
    );
    assert!(g.manifest.keys.contains_key("after"));
}

#[test]
fn undefined_and_unused_keys_and_the_text_and_json_forms() {
    let g = gather(&[
        GatherSource::ftl("en/a.ftl", "en", "used = U\nunused = N\n"),
        GatherSource::ftl("fr/a.ftl", "fr", "used = Utilisé\n"),
        GatherSource::ui_asset(
            "ui/a.ui.ron",
            "(a: Loc(\"used\"), b: Loc(\"missing\"), c: Loc(\"missing\"))",
        ),
    ]);
    let undefined = g.manifest.undefined_keys();
    assert_eq!(undefined.len(), 1);
    assert_eq!((undefined[0].0, undefined[0].1.len()), ("missing", 2));
    assert_eq!(g.manifest.unused_keys(), ["unused"]);
    assert_eq!(
        g.manifest.to_text(),
        "missing  defined: -  refs: 2\nunused  defined: en  refs: 0\nused  defined: en,fr  refs: 1\n"
    );
    let json: serde_json::Value = serde_json::from_str(&g.manifest.to_json().unwrap()).unwrap();
    assert_eq!(json["locales"], serde_json::json!(["en", "fr"]));
    assert!(json["keys"]["used"]["definitions"]["fr"].is_array());
    assert_eq!(gather(&[]).manifest.to_text(), "");
}

#[test]
fn the_result_does_not_depend_on_the_order_of_the_sources() {
    let a = GatherSource::ftl("en/a.ftl", "en", "x = 1\ny = 2\n");
    let b = GatherSource::ftl("en/b.ftl", "en", "z = 3\n");
    let c = GatherSource::ui_asset("ui/c.ui.ron", "(t: Loc(\"x\"), u: Loc(\"nope\"))");
    let d = GatherSource::scene("scene/d.ron", "(n: \"loc:z\")");
    let forward = gather(&[a.clone(), b.clone(), c.clone(), d.clone()]);
    for order in [[&d, &c, &b, &a], [&b, &d, &a, &c], [&c, &a, &d, &b]] {
        let sources: Vec<GatherSource> = order.iter().map(|s| (*s).clone()).collect();
        assert_eq!(gather(&sources), forward);
    }
}

#[test]
fn key_validity_is_ascii_only() {
    for ok in ["a", "A", "menu-start", "a_b", "k9", "a-"] {
        assert!(is_valid_key(ok), "{ok}");
    }
    for bad in ["", "-a", "_a", "9a", "a b", "a.b", "é", "aé", "a\n", "a:"] {
        assert!(!is_valid_key(bad), "{bad:?}");
    }
}
