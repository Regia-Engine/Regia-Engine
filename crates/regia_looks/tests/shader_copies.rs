// SPDX-License-Identifier: MPL-2.0
//! The gem path exists as text in three shaders: the `gem` and `frosted` viewer hooks (each hook is
//! its own module, so they cannot share a function) and the standalone `looks.wgsl`. These tests
//! fail when one copy changes and another does not.

const GEM_HOOK: &str = include_str!("../src/shaders/hooks/gem.wgsl");
const FROSTED_HOOK: &str = include_str!("../src/shaders/hooks/frosted.wgsl");
const STANDALONE: &str = include_str!("../src/shaders/looks.wgsl");

/// The text without `//` comments, one space for any run of whitespace.
fn normalized(source: &str) -> String {
    let code: Vec<&str> = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect();
    code.join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every top-level function in `normalized` text, as (name, whole text).
fn functions(normalized: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for (start, _) in normalized.match_indices("fn ") {
        let at_top = start == 0 || normalized.as_bytes()[start - 1] == b' ';
        let Some(open) = normalized[start..].find('{').map(|o| start + o) else {
            continue;
        };
        if !at_top || nesting_at(normalized, start) != 0 {
            continue;
        }
        let name: String = normalized[start + 3..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let mut depth = 0_i32;
        for (offset, c) in normalized[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                found.push((name, normalized[start..=open + offset].to_string()));
                break;
            }
        }
    }
    found
}

/// The brace depth of `text` at byte `end`.
fn nesting_at(text: &str, end: usize) -> i32 {
    text[..end]
        .bytes()
        .map(|b| match b {
            b'{' => 1,
            b'}' => -1,
            _ => 0,
        })
        .sum()
}

fn function<'a>(all: &'a [(String, String)], name: &str) -> &'a str {
    all.iter()
        .find(|(n, _)| n == name)
        .map_or_else(|| panic!("no function `{name}`"), |(_, text)| text.as_str())
}

/// The value of `const NAME: f32 = value;`.
fn constant<'a>(normalized: &'a str, name: &str) -> &'a str {
    let key = format!("const {name}: f32 = ");
    let start = normalized
        .find(&key)
        .unwrap_or_else(|| panic!("no constant `{name}`"))
        + key.len();
    normalized[start..]
        .split(';')
        .next()
        .unwrap_or_else(|| panic!("constant `{name}` has no end"))
}

#[test]
fn the_parser_finds_top_level_functions_only() {
    let text = normalized(
        "// fn gone() {}\nfn a(x: f32) -> f32 {\n  if x > 0.0 { return 1.0; }\n  return 0.0;\n}\nfn b() {}\n",
    );
    let all = functions(&text);
    let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(function(&all, "b"), "fn b() {}");
    assert!(function(&all, "a").ends_with("return 0.0; }"));
}

#[test]
fn the_frosted_hook_carries_the_gem_hooks_path() {
    let gem = normalized(GEM_HOOK);
    let frosted = normalized(FROSTED_HOOK);
    let gem_functions = functions(&gem);
    let frosted_functions = functions(&frosted);
    let mut compared = 0;
    for (name, text) in &gem_functions {
        // The entry is the one thing that differs: it picks the frosted look's roughness.
        if name != "shade_surface" {
            assert_eq!(text, function(&frosted_functions, name), "`{name}` differs");
            compared += 1;
        }
    }
    assert!(compared >= 6, "only {compared} functions compared");
    for name in ["GIRDLE", "PAV_TAN", "TABLE_Z", "TABLE_R", "SECTOR"] {
        let name = format!("LOOKS_{name}");
        assert_eq!(
            constant(&gem, &name),
            constant(&frosted, &name),
            "`{name}` differs"
        );
    }
}

#[test]
fn the_standalone_shader_has_the_same_virtual_pavilion() {
    let hook = normalized(GEM_HOOK);
    let standalone = normalized(STANDALONE);
    let hook_functions = functions(&hook);
    let standalone_functions = functions(&standalone);
    for name in [
        "cone_hit",
        "facet_normal",
        "pavilion_normal",
        "crown_normal",
        "gem_exit",
    ] {
        // Prefixes are the only naming difference; the stone's size sits in another parameter slot.
        let as_standalone = function(&hook_functions, &format!("looks_{name}"))
            .replace("looks_", "")
            .replace("LOOKS_", "")
            .replace("material.p1", "obj.p2");
        assert_eq!(
            as_standalone,
            function(&standalone_functions, name),
            "`{name}` differs"
        );
    }
    for name in ["GIRDLE", "PAV_TAN", "TABLE_Z", "TABLE_R", "SECTOR"] {
        assert_eq!(
            constant(&hook, &format!("LOOKS_{name}")),
            constant(&standalone, name),
            "`{name}` differs"
        );
    }
}
