// SPDX-License-Identifier: MPL-2.0
//! Planted host-dependent operations; these tests precede the ADR-8367 rules.
use super::*;

const PLANTS: &[(&str, &str)] = &[
    ("wall_clock", "let stamp = Instant::now();"),
    ("os_random", "let rng = RandomState::new();"),
    ("env_read", "let mode = std::env::var(\"MODE\");"),
    ("address_order", "let key = value as *const _ as usize;"),
    ("thread_spawn", "std::thread::spawn(step);"),
    ("arch_cfg", "#[cfg(target_arch = \"x86_64\")] fn step() {}"),
    ("float_sort", "values.sort_by(|a, b| a.partial_cmp(b).unwrap());"),
];

macro_rules! planted_rule {
    ($test:ident, $rule:literal) => {
        #[test]
        fn $test() {
            let source = PLANTS
                .iter()
                .find(|(rule, _)| *rule == $rule)
                .expect("plant")
                .1;
            let found = scan_source("step.rs", source, false);
            assert!(
                found.iter().any(|f| f.rule == $rule),
                "{}: {found:?}",
                $rule
            );
        }
    };
}

planted_rule!(planted_wall_clock, "wall_clock");
planted_rule!(planted_os_random, "os_random");
planted_rule!(planted_env_read, "env_read");
planted_rule!(planted_address_order, "address_order");
planted_rule!(planted_thread_spawn, "thread_spawn");
planted_rule!(planted_arch_cfg, "arch_cfg");
planted_rule!(planted_float_sort, "float_sort");

#[test]
fn host_desync_rules_find_plants_and_require_reasoned_waivers() {
    for &(rule, source) in PLANTS {
        let found = scan_source("step.rs", source, false);
        assert!(found.iter().any(|f| f.rule == rule), "{rule}: {found:?}");
        for waived in [
            format!("// det-scan: allow({rule}): reviewed boundary\n{source}"),
            format!("{source} // det-scan: allow({rule}): reviewed boundary"),
        ] {
            assert!(scan_source("step.rs", &waived, false).is_empty(), "{waived}");
        }
        let missing = format!("// det-scan: allow({rule}):\n{source}");
        let found = scan_source("step.rs", &missing, false);
        assert!(found.iter().any(|f| f.rule == rule), "{missing}");
        assert!(
            found.iter().any(|f| f.rule == "allow_without_reason"),
            "{missing}"
        );
        for inert in [
            format!("// {source}\n"),
            format!("/* {source} */"),
            format!("let help = r###\"{source}\"###;"),
            format!("let help = {source:?};"),
            format!("#[cfg(test)] mod tests {{ {source} }}"),
        ] {
            assert!(scan_source("step.rs", &inert, false).is_empty(), "{inert}");
        }
    }
}

#[test]
fn host_desync_rules_cover_spellings_and_multiline_calls() {
    for (rule, sources) in [
        (
            "wall_clock",
            &[
                "SystemTime::now()",
                "std::time::UNIX_EPOCH",
                "chrono::Utc::now()",
                "time::OffsetDateTime::now_utc()",
                "OffsetDateTime::now_local()",
                "time::OffsetDateTime::now()",
                "std :: time :: Instant ::\n now ()",
                "let clock = Instant::now;",
            ][..],
        ),
        (
            "os_random",
            &[
                "rand::thread_rng()",
                "rand::random::<u32>()",
                "getrandom::fill(&mut bytes)",
                "rand::rngs::OsRng",
                "fastrand::u32(..)",
                "thread_rng()",
                "rand::rng()",
            ][..],
        ),
        (
            "env_read",
            &[
                "env::args()",
                "std::env::var_os(key)",
                "std::env::vars()",
                "env::args_os()",
                "std :: env ::\n var(key)",
            ][..],
        ),
        (
            "address_order",
            &[
                "items.as_ptr() as usize",
                "items.as_ptr()\n as usize",
                "p as *const Item as usize",
                "ptr::addr(p)",
                "p.addr()",
                "items.sort_by_key(|x| Arc::as_ptr(x));",
                "Arc::as_ptr(a) < Arc::as_ptr(b)",
            ][..],
        ),
        (
            "thread_spawn",
            &[
                "thread::Builder::new()",
                "thread::spawn(step)",
                "tokio::spawn(step())",
                "std :: thread ::\n spawn(step)",
            ][..],
        ),
        (
            "arch_cfg",
            &[
                "#[cfg(target_feature = \"sse2\")] fn step() {}",
                "#[cfg(any(target_arch = \"x86_64\", target_arch = \"aarch64\"))] fn f() {}",
                "#[cfg(\n target_arch = \"x86_64\"\n)] fn f() {}",
                "cfg!(target_feature = \"neon\")",
                "#[cfg_attr(target_arch = \"x86_64\", inline)] fn f() {}",
                "is_x86_feature_detected!(\"avx2\")",
                "std::arch::is_aarch64_feature_detected!(\"neon\")",
            ][..],
        ),
        (
            "float_sort",
            &[
                "v.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());",
                "v.sort_by(\n |a, b| {\n a.partial_cmp(b).unwrap()\n });",
                "v.sort_by(f32::partial_cmp);",
            ][..],
        ),
    ] {
        for source in sources {
            let found = scan_source("step.rs", source, false);
            assert!(
                found.iter().any(|f| f.rule == rule),
                "{rule}: {source}: {found:?}"
            );
        }
    }
}

#[test]
fn host_desync_rules_leave_deterministic_alternatives_and_near_misses_clean() {
    for source in [
        "let mode = option_env!(\"MODE\");",
        "let build = env!(\"BUILD\");",
        "let x = Tick(42);",
        "let seed = RecordedSeed(7);",
        "let n = count as usize;",
        "let p = values.as_ptr();",
        "values.sort_by(|a, b| a.total_cmp(b));",
        "values.sort_unstable_by_key(|v| v.id);",
        "let x = a.partial_cmp(b);",
        "values.sort_by(|a,b| a.cmp(b)); let x = a.partial_cmp(b);",
        "my_thread::spawn(step); let x = MyInstant::now();",
        "let target_arch = ARCH;",
        "#[cfg(feature = \"portable\")] fn f() {}",
        "some_env::var(key);",
        "let chrono_value = 0; 'chrono: loop { break 'chrono; }",
    ] {
        assert!(scan_source("step.rs", source, false).is_empty(), "{source}");
    }
}

#[test]
fn host_desync_multiline_findings_use_the_start_line_for_waivers() {
    let src = "// det-scan: allow(float_sort): finite inputs checked by caller\nv.sort_by(\n |a,b| a.partial_cmp(b).unwrap()\n);\n";
    assert!(scan_source("step.rs", src, false).is_empty());
    let src = "\nv.sort_by(\n |a,b| a.partial_cmp(b).unwrap()\n);\n";
    let found = scan_source("step.rs", src, false);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].line, 2);
    assert_eq!(found[0].rule, "float_sort");
}

#[test]
fn host_desync_clippy_policy_requires_methods_and_random_state_type() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("root")
        .to_owned();
    let methods = clippy_methods(&root.join("crates/regia_detsim/clippy.toml")).expect("config");
    for method in [
        "std::time::Instant::now",
        "std::time::SystemTime::now",
        "std::thread::spawn",
        "std::env::var",
    ] {
        assert!(methods.contains(method), "missing {method}");
        assert!(clippy_paths().iter().any(|p| p == method), "unchecked {method}");
    }
    let config: toml::Value =
        toml::from_str(&std::fs::read_to_string(root.join("crates/regia_detsim/clippy.toml")).expect("read"))
            .expect("parse");
    assert!(
        config["disallowed-types"]
            .as_array()
            .expect("type policy")
            .iter()
            .any(|entry| entry["path"].as_str() == Some("std::collections::hash_map::RandomState"))
    );
}

#[test]
fn waiver_text_inside_literals_or_block_comments_cannot_hide_a_finding() {
    for &(rule, source) in PLANTS {
        for fake in [
            format!("let help = r###\"// det-scan: allow({rule}): fake\"###;"),
            format!("/* // det-scan: allow({rule}): fake */"),
            format!("let help = \"// det-scan: allow({rule}): fake\";"),
        ] {
            for text in [format!("{fake}\n{source}"), format!("{fake} {source}")] {
                let found = scan_source("step.rs", &text, false);
                assert!(found.iter().any(|f| f.rule == rule), "{text}: {found:?}");
            }
        }
    }
}

#[test]
fn existing_float_rule_cannot_be_waived_by_a_string() {
    let source = "let help = \"// det-scan: allow(std_float_fn): example\"; x.sin();";
    let found = scan_source("step.rs", source, false);
    assert!(found.iter().any(|f| f.rule == "std_float_fn"), "{found:?}");
}
