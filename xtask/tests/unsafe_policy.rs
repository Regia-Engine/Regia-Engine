// SPDX-License-Identifier: MPL-2.0
//! The `unsafe` policy of `docs/04-conventions.md` § 5, checked on the source of every platform.
//!
//! Clippy's `undocumented_unsafe_blocks` only sees the code the host target compiles, and much of
//! the workspace's `unsafe` sits behind `cfg(windows)` and `cfg(target_os = "android")`. So this
//! test reads the files themselves. It asks three things: every `unsafe` block, `impl`, extern block
//! and attribute has a `// SAFETY:` comment; every `unsafe fn` outside an `unsafe impl` states its
//! contract; and every crate with `unsafe` in its `src` is in the table.
//!
//! The inventory the table's counts come from:
//! `cargo test -p xtask --test unsafe_policy inventory -- --nocapture`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The repository root.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .to_path_buf()
}

/// `text` with comments and the contents of string and character literals replaced by spaces.
/// Line numbers and columns stay the same, so a word found here is a word of the code.
fn blank_comments_and_literals(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = c.clone();
    let blank = |out: &mut Vec<char>, from: usize, to: usize| {
        for slot in &mut out[from..to] {
            if *slot != '\n' {
                *slot = ' ';
            }
        }
    };
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    // `r` starts a raw literal alone or after a `b` or `c` prefix, never inside a longer name.
    let raw_prefix = |c: &[char], i: usize| {
        i == 0 || !ident(c[i - 1]) || (matches!(c[i - 1], 'b' | 'c') && (i < 2 || !ident(c[i - 2])))
    };
    let mut i = 0;
    while i < c.len() {
        let next = c.get(i + 1).copied();
        match c[i] {
            '/' if next == Some('/') => {
                let mut end = i;
                while end < c.len() && c[end] != '\n' {
                    end += 1;
                }
                blank(&mut out, i, end);
                i = end;
            }
            '/' if next == Some('*') => {
                let mut depth = 1;
                let mut end = i + 2;
                while end < c.len() && depth > 0 {
                    if c[end] == '/' && c.get(end + 1) == Some(&'*') {
                        depth += 1;
                        end += 2;
                    } else if c[end] == '*' && c.get(end + 1) == Some(&'/') {
                        depth -= 1;
                        end += 2;
                    } else {
                        end += 1;
                    }
                }
                blank(&mut out, i, end);
                i = end;
            }
            'r' if raw_prefix(&c, i) && (next == Some('"') || next == Some('#')) => {
                let mut hashes = 0;
                while c.get(i + 1 + hashes) == Some(&'#') {
                    hashes += 1;
                }
                if c.get(i + 1 + hashes) != Some(&'"') {
                    // A raw identifier (`r#type`), not a string.
                    i += 1;
                    continue;
                }
                let start = i + 2 + hashes;
                let mut end = start;
                while end < c.len() {
                    if c[end] == '"' && (0..hashes).all(|h| c.get(end + 1 + h) == Some(&'#')) {
                        break;
                    }
                    end += 1;
                }
                blank(&mut out, start, end);
                i = end + 1 + hashes;
            }
            '"' => {
                let mut end = i + 1;
                while end < c.len() && c[end] != '"' {
                    end += if c[end] == '\\' { 2 } else { 1 };
                }
                blank(&mut out, i + 1, end.min(c.len()));
                i = end + 1;
            }
            '\'' => {
                if next == Some('\\') {
                    // Past the escaped character, which may itself be a quote.
                    let mut end = i + 3;
                    while end < c.len() && c[end] != '\'' {
                        end += 1;
                    }
                    blank(&mut out, i + 1, end.min(c.len()));
                    i = end + 1;
                } else if c.get(i + 2) == Some(&'\'') {
                    blank(&mut out, i + 1, i + 2);
                    i += 3;
                } else {
                    // A lifetime.
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    out.into_iter().collect()
}

/// What an `unsafe` keyword introduces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Block,
    Impl,
    /// `#[unsafe(no_mangle)]` and friends.
    Attribute,
    /// `unsafe extern "C" { .. }`.
    ExternBlock,
    /// A function item, with its contract stated above it.
    Fn,
    /// A function item inside an `unsafe impl`: the trait states the contract, the impl's comment
    /// says how it is met.
    TraitFn,
}

/// One `unsafe` keyword in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Site {
    /// 1-based.
    line: usize,
    kind: Kind,
}

/// The `unsafe` keywords of a file's code. A function pointer type (`unsafe extern "C" fn(i64)`)
/// promises nothing by itself, so it is not a site.
fn sites(blanked: &str) -> Vec<Site> {
    let c: Vec<char> = blanked.chars().collect();
    let word = |at: usize, w: &str| -> bool {
        let w: Vec<char> = w.chars().collect();
        c.len() >= at + w.len()
            && c[at..at + w.len()] == w[..]
            && !c
                .get(at + w.len())
                .is_some_and(|ch| ch.is_alphanumeric() || *ch == '_')
    };
    let skip_space = |mut at: usize| {
        while c.get(at).is_some_and(|ch| ch.is_whitespace()) {
            at += 1;
        }
        at
    };
    let mut found = Vec::new();
    let mut line = 1;
    let mut depth = 0usize;
    // Brace depth inside each open `unsafe impl`, and whether its opening brace was seen.
    let mut impl_depths: Vec<usize> = Vec::new();
    let mut pending_impl = false;
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\n' => line += 1,
            '{' => {
                if pending_impl {
                    impl_depths.push(depth);
                    pending_impl = false;
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if impl_depths.last() == Some(&depth) {
                    impl_depths.pop();
                }
            }
            'u' if word(i, "unsafe") && (i == 0 || !(c[i - 1].is_alphanumeric() || c[i - 1] == '_')) => {
                let mut at = skip_space(i + "unsafe".len());
                let kind = match c.get(at) {
                    Some('{') => Some(Kind::Block),
                    Some('(') => Some(Kind::Attribute),
                    _ if word(at, "impl") || word(at, "trait") => {
                        pending_impl = true;
                        Some(Kind::Impl)
                    }
                    _ => {
                        if word(at, "extern") {
                            at = skip_space(at + "extern".len());
                            if c.get(at) == Some(&'"') {
                                at += 1;
                                while c.get(at).is_some_and(|ch| *ch != '"') {
                                    at += 1;
                                }
                                at = skip_space(at + 1);
                            }
                        }
                        if c.get(at) == Some(&'{') {
                            Some(Kind::ExternBlock)
                        } else if word(at, "fn") {
                            at = skip_space(at + "fn".len());
                            if c.get(at) == Some(&'(') {
                                None
                            } else if impl_depths.is_empty() {
                                Some(Kind::Fn)
                            } else {
                                Some(Kind::TraitFn)
                            }
                        } else {
                            // Anything else is a future form: ask for a comment.
                            Some(Kind::Block)
                        }
                    }
                };
                if let Some(kind) = kind {
                    found.push(Site { line, kind });
                }
                i += "unsafe".len();
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    found
}

/// Whether the comment a site needs is there: on its line, or above the statement it belongs to.
/// The walk up stops at the end of the previous statement or block, so a comment on an earlier
/// statement does not count.
fn documented(original: &[&str], blanked: &[&str], site: Site) -> bool {
    let wanted =
        |line: &str| line.contains("SAFETY:") || (site.kind == Kind::Fn && line.contains("# Safety"));
    let index = site.line - 1;
    if wanted(original[index]) {
        return true;
    }
    let mut at = index;
    while at > 0 && index - at < 40 {
        at -= 1;
        if wanted(original[at]) {
            return true;
        }
        let code = blanked[at].trim_end();
        if code.ends_with(';') || code.ends_with('{') || code.ends_with('}') {
            return false;
        }
    }
    false
}

/// The sites of a file with no comment, as `line` numbers.
fn undocumented(text: &str) -> Vec<usize> {
    let blanked = blank_comments_and_literals(text);
    let original: Vec<&str> = text.lines().collect();
    let code: Vec<&str> = blanked.lines().collect();
    sites(&blanked)
        .into_iter()
        .filter(|site| site.kind != Kind::TraitFn && !documented(&original, &code, *site))
        .map(|site| site.line)
        .collect()
}

/// Every `.rs` file under `dir`.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            // User-project templates are not engine code; `target` is not source.
            if name != "target" && name != "templates" {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Each workspace package's directory, named as the policy table names it.
fn packages(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = vec![("xtask".to_owned(), root.join("xtask"))];
    for parent in ["crates", "samples"] {
        for entry in fs::read_dir(root.join(parent))
            .expect("workspace directory")
            .flatten()
        {
            if entry.path().join("Cargo.toml").is_file() {
                out.push((entry.file_name().to_string_lossy().into_owned(), entry.path()));
            }
        }
    }
    out.sort();
    out
}

/// The crate names in the table of `docs/04-conventions.md` § 5.
fn table_crates(root: &Path) -> Vec<String> {
    let conventions = fs::read_to_string(root.join("docs/04-conventions.md")).expect("docs/04");
    let section = conventions
        .split("## 5. `unsafe` policy")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("docs/04 has the unsafe policy section");
    section
        .lines()
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|row| row.split(" |").next())
        .map(|name| name.trim().trim_matches('`').to_owned())
        .collect()
}

#[test]
fn every_unsafe_site_says_why_it_is_sound() {
    let root = workspace_root();
    let mut missing = Vec::new();
    for (_, dir) in packages(&root) {
        let mut files = Vec::new();
        rust_files(&dir, &mut files);
        for file in files {
            let text = fs::read_to_string(&file).expect("source file");
            for line in undocumented(&text) {
                let shown = file.strip_prefix(&root).unwrap_or(&file).display().to_string();
                missing.push(format!("{shown}:{line}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "unsafe with no `// SAFETY:` comment (or `# Safety` contract on an `unsafe fn`), \
         docs/04-conventions.md § 5:\n{}",
        missing.join("\n")
    );
}

/// `src` is what ships. A test or example target may opt out on its own (docs/04 § 5).
#[test]
fn every_crate_with_unsafe_in_its_src_is_in_the_table() {
    let root = workspace_root();
    let listed = table_crates(&root);
    assert!(listed.iter().any(|name| name == "regia_platform"), "{listed:?}");
    let mut unlisted = Vec::new();
    for (name, dir) in packages(&root) {
        if listed.contains(&name) {
            continue;
        }
        let mut files = Vec::new();
        rust_files(&dir.join("src"), &mut files);
        for file in files {
            let text = fs::read_to_string(&file).expect("source file");
            let blanked = blank_comments_and_literals(&text);
            if !sites(&blanked).is_empty() || blanked.contains("allow(unsafe_code)") {
                unlisted.push(format!("{name}: {}", file.display()));
            }
        }
    }
    assert!(
        unlisted.is_empty(),
        "unsafe in a crate outside the table of docs/04-conventions.md § 5 (adding one needs an ADR):\n{}",
        unlisted.join("\n")
    );
}

/// The lints that make the comments and the table mean something stay on.
#[test]
fn the_lints_stay_on_for_every_package() {
    let root = workspace_root();
    let manifest = fs::read_to_string(root.join("Cargo.toml")).expect("root manifest");
    assert!(manifest.contains("\nunsafe_code = \"deny\""), "workspace lint");
    assert!(
        manifest.contains("\nundocumented_unsafe_blocks = \"warn\""),
        "workspace lint"
    );
    for (name, dir) in packages(&root) {
        let manifest = fs::read_to_string(dir.join("Cargo.toml")).expect("manifest");
        let inherits = manifest.contains("[lints]\nworkspace = true");
        let denies = manifest.contains("[lints.rust]\nunsafe_code = \"deny\"");
        assert!(
            inherits || denies,
            "{name}'s manifest turns off the unsafe_code lint"
        );
    }
}

/// `blanked` without the items under `#[cfg(test)]` or `#[cfg(all(test, ..))]`, line numbers kept.
fn without_test_items(blanked: &str) -> String {
    let mut c: Vec<char> = blanked.chars().collect();
    let text: String = c.iter().collect();
    for marker in ["#[cfg(test)]", "#[cfg(all(test"] {
        for (byte_at, _) in text.match_indices(marker) {
            let mut at = text[..byte_at].chars().count();
            let mut depth = 0usize;
            while at < c.len() {
                match c[at] {
                    ';' if depth == 0 => break,
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                if c[at] != '\n' {
                    c[at] = ' ';
                }
                at += 1;
            }
        }
    }
    c.into_iter().collect()
}

/// Counts per crate and kind; `-- --nocapture` prints the table the docs quote. Shipped code is `src`
/// without its test items; the last column is everything else.
#[test]
fn inventory() {
    #[derive(Default)]
    struct Row {
        shipped: BTreeMap<Kind, usize>,
        tests: usize,
    }
    let root = workspace_root();
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for (name, dir) in packages(&root) {
        let mut files = Vec::new();
        rust_files(&dir, &mut files);
        let src = dir.join("src");
        for file in files {
            let blanked = blank_comments_and_literals(&fs::read_to_string(&file).expect("source file"));
            let all = sites(&blanked).len();
            if all == 0 {
                continue;
            }
            let row = rows.entry(name.clone()).or_default();
            let mut shipped = 0;
            if file.starts_with(&src) {
                for site in sites(&without_test_items(&blanked)) {
                    *row.shipped.entry(site.kind).or_default() += 1;
                    shipped += 1;
                }
            }
            row.tests += all - shipped;
        }
    }
    println!(
        "| Crate | Blocks | Impls | Attributes | Extern blocks | Fns | Fns in an impl | In tests and examples |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    let (mut shipped_total, mut tests_total) = (0, 0);
    for (name, row) in &rows {
        let count = |kind| row.shipped.get(&kind).copied().unwrap_or(0);
        println!(
            "| {name} | {} | {} | {} | {} | {} | {} | {} |",
            count(Kind::Block),
            count(Kind::Impl),
            count(Kind::Attribute),
            count(Kind::ExternBlock),
            count(Kind::Fn),
            count(Kind::TraitFn),
            row.tests
        );
        shipped_total += row.shipped.values().sum::<usize>();
        tests_total += row.tests;
    }
    println!("total: {shipped_total} in shipped code, {tests_total} in tests and examples");
    assert!(shipped_total > 0, "the scan found no unsafe at all: it is broken");
}

#[test]
fn test_items_do_not_count_as_shipped() {
    let text = "unsafe fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() { unsafe { c() } }\n}\nunsafe fn d() {}\n#[cfg(all(test, unix))]\nfn e() { unsafe { c() } }\n#[cfg(test)]\nuse f;\nunsafe fn g() {}\n";
    let lines: Vec<usize> = sites(&without_test_items(text))
        .iter()
        .map(|site| site.line)
        .collect();
    assert_eq!(lines, vec![1, 6, 11]);
}

// The scanner decides whether a merge goes through, so its own rules are pinned here.

#[test]
fn a_block_needs_a_comment_on_its_statement() {
    assert_eq!(undocumented("fn a() {\n    unsafe { b() };\n}\n"), vec![2]);
    assert!(undocumented("fn a() {\n    // SAFETY: b has no contract.\n    unsafe { b() };\n}\n").is_empty());
    assert!(
        undocumented("fn a() {\n    // SAFETY: why.\n    let x =\n        unsafe { b() }.c();\n}\n")
            .is_empty()
    );
    assert!(
        undocumented("fn a() {\n    // SAFETY: why.\n    d(\n        unsafe { b() }\n    );\n}\n").is_empty()
    );
    assert!(
        undocumented("fn a() {\n    // SAFETY: why.\n    #[allow(unsafe_code)]\n    unsafe { b() };\n}\n")
            .is_empty()
    );
    assert!(undocumented("fn a() {\n    unsafe {\n        b();\n    }\n}\n").len() == 1);
}

#[test]
fn a_comment_on_an_earlier_statement_does_not_count() {
    assert_eq!(
        undocumented(
            "fn a() {\n    // SAFETY: why.\n    unsafe { b() };\n    c();\n    unsafe { d() };\n}\n"
        ),
        vec![5]
    );
    assert_eq!(
        undocumented("fn a() {\n    // SAFETY: why.\n    {\n        unsafe { b() };\n    }\n}\n"),
        vec![4]
    );
}

#[test]
fn strings_comments_and_names_are_not_code() {
    for text in [
        "// unsafe { b() }\n",
        "/* unsafe { b() } */\n",
        "const A: &str = \"unsafe { b() }\";\n",
        "const A: &str = r#\"unsafe { b() } \" still \"#;\n",
        "const A: &[u8] = b\"unsafe {\";\n",
        "#![allow(unsafe_code)]\n",
        "fn not_unsafe_at_all() {}\n",
        "let a = unsafe_libopus::b;\n",
        "fn a<'unsafe_ish>(b: &'unsafe_ish u8) -> char { '\"' }\n",
    ] {
        assert_eq!(sites(&blank_comments_and_literals(text)), vec![], "{text}");
    }
}

#[test]
fn a_function_pointer_type_is_not_a_site() {
    for text in [
        "type A = unsafe extern \"C\" fn(i64);\n",
        "type A = unsafe fn();\n",
        "library.get::<unsafe extern \"Rust\" fn() -> *const u8>(NAME);\n",
    ] {
        assert_eq!(sites(&blank_comments_and_literals(text)), vec![], "{text}");
    }
}

#[test]
fn a_function_states_its_contract_unless_a_trait_does() {
    assert_eq!(undocumented("unsafe fn a() {}\n"), vec![1]);
    assert_eq!(undocumented("unsafe extern \"C\" fn a() {}\n"), vec![1]);
    assert!(undocumented("/// # Safety\n/// The caller holds the lock.\nunsafe fn a() {}\n").is_empty());
    let text = "// SAFETY: forwards.\nunsafe impl GlobalAlloc for A {\n    unsafe fn alloc(&self) {}\n}\nunsafe fn b() {}\n";
    assert_eq!(undocumented(text), vec![5]);
}

#[test]
fn impls_extern_blocks_and_attributes_need_comments() {
    assert_eq!(undocumented("unsafe impl Send for A {}\n"), vec![1]);
    assert_eq!(undocumented("unsafe extern \"C\" {\n    fn a();\n}\n"), vec![1]);
    assert_eq!(
        undocumented("#[unsafe(no_mangle)]\npub static A: u8 = 0;\n"),
        vec![1]
    );
    assert!(undocumented("// SAFETY: why.\nunsafe impl Send for A {}\n").is_empty());
    assert!(undocumented("// SAFETY: why.\nunsafe extern \"C\" {\n    fn a();\n}\n").is_empty());
    assert!(
        undocumented("// SAFETY: why.\n#[used]\n#[unsafe(no_mangle)]\npub static A: u8 = 0;\n").is_empty()
    );
}

// ---- The site inventory of docs/06-testing-and-ci.md § 14 (ADR-2726) ----

/// The header of an inventory table. A section may hold several, one per group of crates.
const INVENTORY_HEADER: &str = "| Crate | File | Sites | What they do | Miri | Covered by |";
/// The `File` of a row that stands for a crate's test code: test items in `src`, plus `tests`,
/// `examples`, `benches` and everything else outside `src`. Its count is not pinned.
const TEST_CODE: &str = "(test code)";

/// The script that runs the Miri subset. A row's filters must be in it.
const MIRI_RECIPE: &str = "scripts/miri-unsafe-subset.sh";
/// The script that runs the ThreadSanitizer subset.
const TSAN_RECIPE: &str = "scripts/tsan-subset.sh";

/// One row of the inventory table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InventoryRow {
    package: String,
    file: String,
    /// `None` for a row that does not pin its count (`~12`): only [`TEST_CODE`] rows may do that.
    sites: Option<usize>,
    miri: String,
    covered_by: String,
}

/// The words between backticks in `cell`.
fn backticked(cell: &str) -> Vec<&str> {
    cell.split('`').skip(1).step_by(2).collect()
}

/// The rows of the inventory tables in `section`, or what is wrong with a malformed row.
fn inventory_rows(section: &str) -> Result<Vec<InventoryRow>, String> {
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in section.lines() {
        if line.trim_end() == INVENTORY_HEADER {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if !line.starts_with('|') {
            in_table = false;
            continue;
        }
        if line.starts_with("|---") {
            continue;
        }
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split(" | ")
            .map(str::trim)
            .collect();
        let [package, file, sites, _what, miri, covered_by] = cells[..] else {
            return Err(format!("not six cells: {line}"));
        };
        let unwrap_ticks = |cell: &str| cell.trim_matches('`').to_owned();
        let sites = match sites.strip_prefix('~') {
            Some(_) => None,
            None => Some(
                sites
                    .parse::<usize>()
                    .map_err(|_| format!("the site count is not a number: {line}"))?,
            ),
        };
        rows.push(InventoryRow {
            package: unwrap_ticks(package),
            file: unwrap_ticks(file),
            sites,
            miri: miri.to_owned(),
            covered_by: covered_by.to_owned(),
        });
    }
    Ok(rows)
}

/// What the scanner found: shipped sites per `(package, file relative to the package)`, and the
/// packages with any site in their test code.
#[derive(Debug, Default)]
struct ScannedSites {
    shipped: BTreeMap<(String, String), usize>,
    test_code: BTreeMap<String, usize>,
}

/// Adds one file to `scanned`. `relative` is the path inside the package, with `/` separators.
fn scan_file(scanned: &mut ScannedSites, package: &str, relative: &str, text: &str) {
    let blanked = blank_comments_and_literals(text);
    let all = sites(&blanked).len();
    if all == 0 {
        return;
    }
    let shipped = if relative.starts_with("src/") {
        sites(&without_test_items(&blanked)).len()
    } else {
        0
    };
    if shipped > 0 {
        scanned
            .shipped
            .insert((package.to_owned(), relative.to_owned()), shipped);
    }
    if all > shipped {
        *scanned.test_code.entry(package.to_owned()).or_default() += all - shipped;
    }
}

/// Every problem between the table and the scan. Shipped counts are exact, in both directions, so
/// a new site needs a row and a removed one needs its row corrected; a file may have several rows
/// (sites of one file can differ in what Miri can do), and their counts add up.
fn inventory_problems(rows: &[InventoryRow], scanned: &ScannedSites) -> Vec<String> {
    let mut problems = Vec::new();
    let mut listed: BTreeMap<(String, String), usize> = BTreeMap::new();
    for row in rows {
        if row.file == TEST_CODE {
            if !scanned.test_code.contains_key(&row.package) {
                problems.push(format!(
                    "{}: a test-code row, but its test code has no unsafe",
                    row.package
                ));
            }
            continue;
        }
        let Some(sites) = row.sites else {
            problems.push(format!(
                "{} {}: only a `{TEST_CODE}` row may leave its count unpinned",
                row.package, row.file
            ));
            continue;
        };
        *listed.entry((row.package.clone(), row.file.clone())).or_default() += sites;
    }
    for ((package, file), found) in &scanned.shipped {
        match listed.get(&(package.clone(), file.clone())) {
            None => problems.push(format!(
                "{package} {file}: {found} unsafe site(s) and no row. Add `| {package} | `{file}` | {found} | .. |`"
            )),
            Some(count) if count != found => problems.push(format!(
                "{package} {file}: the rows say {count} site(s), the source has {found}"
            )),
            Some(_) => {}
        }
    }
    for (package, file) in listed.keys() {
        if !scanned.shipped.contains_key(&(package.clone(), file.clone())) {
            problems.push(format!(
                "{package} {file}: a row, but no unsafe site in shipped code"
            ));
        }
    }
    for package in scanned.test_code.keys() {
        if !rows
            .iter()
            .any(|row| &row.package == package && row.file == TEST_CODE)
        {
            problems.push(format!(
                "{package}: unsafe in its test code and no row. Add `| {package} | `{TEST_CODE}` | ~N | .. |`"
            ));
        }
    }
    problems
}

/// What is wrong with a row's `Miri` and `Covered by` cells. `subset` is the text of the Miri recipe
/// (`scripts/miri-unsafe-subset.sh`), `has_test` says whether a function of that name exists in the
/// workspace.
fn row_cell_problems(row: &InventoryRow, subset: &str, has_test: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut problems = Vec::new();
    let shown = format!("{} {}", row.package, row.file);
    let runs_under_miri = row.miri.starts_with("yes") || row.miri.starts_with("partly");
    if runs_under_miri {
        // Anything after `: ` is prose; a filter's `::` has no space.
        let filters = backticked(row.miri.split(": ").next().unwrap_or_default());
        if filters.is_empty() {
            problems.push(format!(
                "{shown}: `{}` names no test filter of the recipe",
                row.miri
            ));
        }
        for filter in filters {
            if !subset.contains(filter) {
                problems.push(format!("{shown}: the Miri recipe does not run `{filter}`"));
            }
        }
    } else if !(row.miri.starts_with("no (") && row.miri.ends_with(')')) {
        problems.push(format!(
            "{shown}: the Miri cell is `yes`, `partly` or `no (why)`, not `{}`",
            row.miri
        ));
    }
    if row.covered_by.starts_with("none:") {
        return problems;
    }
    let tests = backticked(&row.covered_by);
    if tests.is_empty() {
        problems.push(format!(
            "{shown}: `Covered by` names a test or says `none: why`, not `{}`",
            row.covered_by
        ));
    }
    for test in tests {
        if !has_test(test) {
            problems.push(format!("{shown}: `{test}` is not a function in the workspace"));
        }
    }
    problems
}

/// § 14 of the testing doc.
fn section_14(root: &Path) -> String {
    let doc = fs::read_to_string(root.join("docs/06-testing-and-ci.md")).expect("docs/06");
    doc.split("\n## 14. ")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("docs/06 has section 14")
        .to_owned()
}

/// The ratchet: every `unsafe` site in shipped code is counted in a row of the table in
/// docs/06 § 14, every crate with `unsafe` in its test code has a row, and the tests and Miri filters the
/// rows name exist. A new site without a row fails the gate (ADR-2726).
#[test]
fn every_unsafe_site_is_in_the_inventory() {
    let root = workspace_root();
    let section = section_14(&root);
    let rows = inventory_rows(&section).unwrap_or_else(|problem| panic!("{problem}"));
    assert!(rows.iter().any(|row| row.package == "regia_core"), "{rows:?}");
    let mut scanned = ScannedSites::default();
    let mut functions = std::collections::BTreeSet::new();
    for (name, dir) in packages(&root) {
        let mut files = Vec::new();
        rust_files(&dir, &mut files);
        for file in files {
            let text = fs::read_to_string(&file).expect("source file");
            let relative = file.strip_prefix(&dir).expect("under the package");
            let relative = relative.to_string_lossy().replace('\\', "/");
            scan_file(&mut scanned, &name, &relative, &text);
            for piece in text.split("fn ").skip(1) {
                let end = piece
                    .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                    .unwrap_or(piece.len());
                functions.insert(piece[..end].to_owned());
            }
        }
    }
    let subset = fs::read_to_string(root.join(MIRI_RECIPE)).expect("the Miri recipe");
    let mut problems = inventory_problems(&rows, &scanned);
    for row in &rows {
        problems.extend(row_cell_problems(row, &subset, &|name| functions.contains(name)));
    }
    let jobs = fs::read_to_string(root.join("docs/ci/nightly-jobs.toml")).expect("the nightly registry");
    for (recipe, job) in [(MIRI_RECIPE, "miri-unsafe-subset"), (TSAN_RECIPE, "tsan-subset")] {
        if !section.contains(recipe) {
            problems.push(format!("docs/06 § 14 does not name {recipe}"));
        }
        let names_it = jobs
            .split("[[job]]")
            .any(|entry| entry.contains(&format!("id = \"{job}\"")) && entry.contains(recipe));
        if !names_it {
            problems.push(format!(
                "docs/ci/nightly-jobs.toml: job {job} does not run {recipe}"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "the unsafe inventory, docs/06-testing-and-ci.md § 14 (ADR-2726):\n{}",
        problems.join("\n")
    );
}

/// The rows the table should have, with the scanner's counts: `-- --nocapture`.
#[test]
fn inventory_by_file() {
    let root = workspace_root();
    let mut scanned = ScannedSites::default();
    for (name, dir) in packages(&root) {
        let mut files = Vec::new();
        rust_files(&dir, &mut files);
        for file in files {
            let text = fs::read_to_string(&file).expect("source file");
            let relative = file.strip_prefix(&dir).expect("under the package");
            scan_file(
                &mut scanned,
                &name,
                &relative.to_string_lossy().replace('\\', "/"),
                &text,
            );
        }
    }
    for ((package, file), sites) in &scanned.shipped {
        println!("{package} {file} {sites}");
    }
    for (package, sites) in &scanned.test_code {
        println!("{package} {TEST_CODE} ~{sites}");
    }
    assert!(
        !scanned.shipped.is_empty(),
        "the scan found no unsafe at all: it is broken"
    );
}

/// A row list and scan for the ratchet's own tests: one file with two sites.
fn ratchet_fixture() -> (Vec<InventoryRow>, ScannedSites) {
    let mut scanned = ScannedSites::default();
    let text =
        "fn a() {\n    // SAFETY: why.\n    unsafe { b() };\n    // SAFETY: why.\n    unsafe { c() };\n}\n";
    scan_file(&mut scanned, "demo", "src/a.rs", text);
    let row = InventoryRow {
        package: "demo".to_owned(),
        file: "src/a.rs".to_owned(),
        sites: Some(2),
        miri: "no (FFI)".to_owned(),
        covered_by: "`a_test`".to_owned(),
    };
    (vec![row], scanned)
}

#[test]
fn a_matching_table_passes_the_ratchet() {
    let (rows, scanned) = ratchet_fixture();
    assert_eq!(inventory_problems(&rows, &scanned), Vec::<String>::new());
}

#[test]
fn a_new_site_in_a_listed_file_fails_the_ratchet() {
    let (rows, mut scanned) = ratchet_fixture();
    let text = "fn a() {\n    unsafe { b() };\n    unsafe { c() };\n    unsafe { d() };\n}\n";
    scan_file(&mut scanned, "demo", "src/a.rs", text);
    let problems = inventory_problems(&rows, &scanned);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("the rows say 2 site(s), the source has 3"),
        "{problems:?}"
    );
}

#[test]
fn a_new_file_and_a_new_crate_fail_the_ratchet() {
    let (rows, mut scanned) = ratchet_fixture();
    scan_file(&mut scanned, "demo", "src/b.rs", "unsafe fn b() {}\n");
    scan_file(&mut scanned, "other", "src/lib.rs", "unsafe fn b() {}\n");
    let problems = inventory_problems(&rows, &scanned);
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(
        problems[0].contains("demo src/b.rs: 1 unsafe site(s) and no row"),
        "{problems:?}"
    );
    assert!(problems[1].contains("other src/lib.rs"), "{problems:?}");
}

#[test]
fn a_stale_row_fails_the_ratchet() {
    let (rows, _) = ratchet_fixture();
    let problems = inventory_problems(&rows, &ScannedSites::default());
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("no unsafe site in shipped code"),
        "{problems:?}"
    );
}

#[test]
fn several_rows_of_one_file_add_up() {
    let (mut rows, scanned) = ratchet_fixture();
    rows[0].sites = Some(1);
    rows.push(rows[0].clone());
    assert_eq!(inventory_problems(&rows, &scanned), Vec::<String>::new());
}

/// Test items and files outside `src` are test code: not pinned, but the crate needs a row.
#[test]
fn test_code_needs_a_row_but_not_a_pinned_count() {
    let mut scanned = ScannedSites::default();
    scan_file(&mut scanned, "demo", "tests/a.rs", "unsafe fn b() {}\n");
    scan_file(
        &mut scanned,
        "demo",
        "src/a.rs",
        "#[cfg(test)]\nmod tests {\n    unsafe fn b() {}\n}\n",
    );
    assert!(scanned.shipped.is_empty(), "{scanned:?}");
    assert_eq!(scanned.test_code.get("demo"), Some(&2));
    let problems = inventory_problems(&[], &scanned);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("unsafe in its test code and no row"),
        "{problems:?}"
    );
    let row = InventoryRow {
        package: "demo".to_owned(),
        file: TEST_CODE.to_owned(),
        sites: None,
        miri: "no (FFI)".to_owned(),
        covered_by: "`a_test`".to_owned(),
    };
    assert_eq!(inventory_problems(&[row], &scanned), Vec::<String>::new());
}

#[test]
fn only_test_code_may_leave_its_count_unpinned() {
    let (mut rows, scanned) = ratchet_fixture();
    rows[0].sites = None;
    let problems = inventory_problems(&rows, &scanned);
    assert!(problems[0].contains("only a `(test code)` row"), "{problems:?}");
}

#[test]
fn the_table_parser_reads_rows_and_refuses_bad_ones() {
    let table = format!(
        "text\n{INVENTORY_HEADER}\n|---|---|---|---|---|---|\n| demo | `src/a.rs` | 2 | calls `b` | no (FFI) | `a_test` |\n| demo | `{TEST_CODE}` | ~7 | allocators | yes, `a::tests` | none: forwards |\n\nmore text\n| not | a | row |\n"
    );
    let rows = inventory_rows(&table).expect("rows");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!((rows[0].file.as_str(), rows[0].sites), ("src/a.rs", Some(2)));
    assert_eq!((rows[1].file.as_str(), rows[1].sites), (TEST_CODE, None));
    assert_eq!(backticked(&rows[1].miri), ["a::tests"]);
    assert!(
        inventory_rows(&format!(
            "{INVENTORY_HEADER}\n| demo | `a` | many | b | c | d |\n"
        ))
        .is_err()
    );
    assert!(inventory_rows(&format!("{INVENTORY_HEADER}\n| demo | `a` | 1 |\n")).is_err());
}

#[test]
fn the_cells_name_real_tests_and_recipe_filters() {
    let row = |miri: &str, covered: &str| InventoryRow {
        package: "demo".to_owned(),
        file: "src/a.rs".to_owned(),
        sites: Some(1),
        miri: miri.to_owned(),
        covered_by: covered.to_owned(),
    };
    let known = |name: &str| name == "a_test";
    let subset = "cargo miri test -- a::tests";
    for good in [
        row("yes, `a::tests`", "`a_test`"),
        row("partly, `a::tests`: the rest is FFI", "`a_test`"),
        row("no (FFI)", "none: needs a card"),
    ] {
        assert_eq!(row_cell_problems(&good, subset, &known), Vec::<String>::new());
    }
    for (bad, wanted) in [
        (row("yes", "`a_test`"), "names no test filter"),
        (row("yes, `b::tests`", "`a_test`"), "does not run `b::tests`"),
        (row("no", "`a_test`"), "`yes`, `partly` or `no (why)`"),
        (row("no (FFI)", "`gone`"), "`gone` is not a function"),
        (row("no (FFI)", "nothing"), "says `none: why`"),
    ] {
        let problems = row_cell_problems(&bad, subset, &known);
        assert!(
            problems.len() == 1 && problems[0].contains(wanted),
            "{problems:?}"
        );
    }
}

/// Whether every suppression line of a ThreadSanitizer suppressions file has its reason on the comment
/// line above it; returns the entries that have none.
fn unexplained_suppressions(text: &str) -> Vec<&str> {
    let mut previous = "";
    let mut unexplained = Vec::new();
    for line in text.lines().map(str::trim) {
        if !line.is_empty() && !line.starts_with('#') && !previous.starts_with('#') {
            unexplained.push(line);
        }
        previous = line;
    }
    unexplained
}

/// A suppression hides a report for good, so each one says why (ADR-2726).
#[test]
fn every_thread_sanitizer_suppression_has_a_reason() {
    let path = workspace_root().join("docs/ci/tsan-suppressions.txt");
    let text = fs::read_to_string(path).expect("docs/ci/tsan-suppressions.txt");
    assert_eq!(unexplained_suppressions(&text), Vec::<&str>::new());
}

#[test]
fn a_suppression_without_a_comment_above_it_is_found() {
    let text = "# why\nrace:good\nrace:next_to_good\n\n# why\nrace:also_good\n\nrace:bad\n";
    assert_eq!(unexplained_suppressions(text), ["race:next_to_good", "race:bad"]);
}
