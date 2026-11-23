# 96 — Guides and book completeness

Status: proposed (ADR-1704). Plan item: "complete user and agent guides" (`master-plan.md` § 3, v0.3.x (b)
deliverables; the docs/ecosystem row of § 4: "Book, agent pages, manual, templates, samples"). Crate: `xtask`
(the `book-check` merge-gate step), with test-only helpers in `regia_test_utils::book` and one test per
registering crate. No runtime crate changes.

Source: the Regia docs only (`specs/23` § 3.2 rule 8, `AGENTS.md` rule 8, `book/COVERAGE.md`). No public spec set
was used; the scan is original code (`docs/provenance.md`).

## 1. Purpose and scope

Rule 8 says every shipped feature has a guide page. `cargo xtask book-check` already enforces it for the runtime
agent protocol: every method of `regia_agent::schema::METHODS` and every `route` arm of the dispatcher needs a
`book/COVERAGE.md` row, a page that names it, an `agent.guide` topic, a contract and a test call
(`xtask/src/agent_coverage.rs`); since ADR-5866 one of its pages also names each schema parameter. Book `rust` fences compile and named `ron` fences parse through the real loaders.

Four kinds of shipped surface had no check, so a new one could land with no page:

1. **Host extension methods.** Methods a crate registers on `regia_agent::extension::AgentExtension` (the
   editor's `editor.*`, the Steam plugin's `steam.*`, `physics.cloth_stats`). They are not `route` arms, so
   `agent-coverage` skips them; `COVERAGE.md` named them in prose with no check behind it.
2. **Cargo features** of the workspace crates (§ 1 of `COVERAGE.md`): nothing compared the table with the
   manifests; 29 of the 41 features had no row.
3. **`regia` CLI subcommands** (§ 3): nothing compared the table with the clap command list; three had no row.
4. **Frozen content formats** (§ 4): nothing compared the table with the `check-formats` registry; 17 of the 45
   formats had no row, and two (`.montagevar.ron`, `.slots.ron`) had no guide page at all.

This spec adds those four checks to `book-check`, fills the gaps they find, and gives agents a JSON report.

## 2. Terms

- **Shipped agent method**: a runtime schema method, or a host extension method registered by non-test code
  under `crates/*/src` (the `MethodSchema { name: "area.name", .. }` literal of its schema row). The dynamic
  `tool.<id>.<name>` methods of external editor tools are not shipped by the engine (they come from a tool's
  manifest) and are out of scope.
- **Extension table**: § 5 of `book/COVERAGE.md`, header `| Extension method | Registered by | Agent page |`.
- **Feature table**: § 1, header `| Crate | Feature | What it does | Page |`.
- **CLI table**: § 3, header `| Command | What it does | Page |`; a row covers subcommand `x` when its first cell
  starts with `` `regia x`` followed by a space or the closing backtick.
- **Format table**: § 4, header `| Format | Extension | What it stores | Page |`.
- A table may continue after a paragraph by repeating its header; every table with the exact header counts.
  A feature row for a crate with no Cargo features, or whose tokens name none of its crate's features, is a
  non-failing note.

## 3. API shape (Rust)

```rust,ignore
// xtask/src/book_coverage.rs
/// One gap or drift found by the completeness checks.
pub struct Finding {
    pub section: Section,        // Extension | Feature | Cli | Format | Fence
    pub code: &'static str,      // see § 5
    pub subject: String,         // `editor.select`, `regia_render/test-hooks`, `regia run`
    pub message: String,
}
pub enum Section { Extension, Feature, Cli, Format, Fence }
/// Every check of this spec over the workspace at `root`.
pub fn check(root: &Path) -> anyhow::Result<Report>;
pub struct Report {
    pub extension_methods: BTreeMap<String, String>, // method -> registering crate
    pub features: BTreeMap<String, Vec<String>>,    // crate -> features
    pub cli_commands: Vec<String>,                   // top-level `regia` subcommands
    pub cli_nested: Vec<String>,                     // nested paths: `steam upload`
    pub formats: BTreeMap<String, u32>,             // frozen format -> version (format-hashes.ron)
    pub findings: Vec<Finding>,                      // sorted, deduplicated
    pub notes: Vec<String>,                          // non-failing observations
    pub fences: FenceCounts,                         // compiled / ignored Rust fences, per page
}
impl Report { pub fn to_json(&self) -> serde_json::Value; }
/// `cargo xtask book-coverage [--json]`.
pub fn run(root: &Path, json: bool) -> anyhow::Result<ExitCode>;

// regia_test_utils::book (dev-only; used by each registering crate's test)
pub fn workspace_root() -> PathBuf;
pub struct ExtensionRow { pub method: String, pub registered_by: String, pub page: String }
pub fn extension_rows(coverage_md: &str) -> Vec<ExtensionRow>;
pub fn extension_gaps(root: &Path, names: &[&str], krate: &str,
                      mentions: &dyn Fn(&str, &str) -> bool) -> Vec<String>;
pub fn cli_gaps(coverage_md: &str, commands: &[&str]) -> Vec<String>;
```

How the lists are found:

- **Extension methods (xtask)**: a text scan of every non-test `.rs` file under `crates/*/src` (the
  `agent-coverage` test-code rules: `tests/` files, `#![cfg(test)]` files, `#[cfg(test)]` items and the files
  they pull in are test code) for `MethodSchema {` literals outside comments and strings (the `name:` field at
  the literal's top level is the method) and for calls of same-file constructors `fn f(name: &'static str, ..) ->
  MethodSchema` whose first argument is a string literal (the editor's tools and animation methods are built that
  way). Names in the runtime schema are removed.
- **Extension methods (runtime)**: each registering crate has a test that installs its real methods on a world
  and checks `AgentExtension::method_names()` against the extension table (`regia_editor`, `regia_steam`,
  `regia_agent` for cloth). Together with the xtask drift check (a row must name a scanned method) this pins the
  scan: a method the scan misses fails one side.
- **Features**: the `[features]` keys of every `crates/*/Cargo.toml` (parsed with `toml`).
- **Formats**: the keys of `docs/formats/format-hashes.ron`; a § 4 row names one in its Format or Extension cell
  (the name, or with `-` as `_` or dropped, as a whole token).
- **CLI commands (xtask)**: the variants of `enum CliCommand` in `crates/regia_cli/src/main.rs`, kebab-cased the
  way clap names them (a `#[command(name = "..")]` wins). **(runtime)**: `crates/regia_cli/tests` runs the
  built `regia -h` and reads the `Commands:` list.

## 4. Files and formats

- `book/COVERAGE.md` § 5 (new, appended): the extension table. Markdown is not a content format and the engine
  never loads it; the table is keyed by its header cells, so a column rename is a check change in the same commit.
- The JSON report (`cargo xtask book-coverage --json`) has `"format_version": 1`. Migration note: fields are only
  added inside version 1; renaming or removing a field, or changing a finding code, bumps it to 2 and the book
  page lists both shapes for one release. No reader of it ships in the engine.
- No frozen content format changes; `cargo xtask check-formats` is unaffected.

## 5. Agent-protocol surface

The checks are host tooling, not a running app, so the surface is the xtask JSON report (the same pattern as
`cargo xtask agent-coverage` and `windows-pass --read`): `cargo xtask book-coverage --json` prints

```json
{ "format_version": 1, "ok": false,
  "counts": { "extension_methods": 133, "features": 41, "cli_commands": 22, "formats": 45 },
  "findings": [ { "section": "extension", "code": "missing_row", "subject": "editor.select",
                  "message": "..." } ],
  "notes": [ "..." ],
  "fences": { "compiled": 111, "ignored": 131, "ignored_by_page": { "user-guide/audio.md": 4 } } }
```

Finding codes (lowercase, part of the contract): `missing_row` (no row), `missing_page` (the row's page does not
exist), `page_does_not_mention` (the page does not name the extension method as a whole token; a feature row only needs a page
that exists, ADR-1704, ADR-5660), `untested`
(no test-code string literal names the extension method), `ignored_fence_grew`, `shipped_tool_unchecked`, `wrong_registrar` (the row's `Registered by` cell does
not name the registering crate), `stale_row` (a row names a method, subcommand (nested ones included) or format that is not shipped, or a crate that is not in the workspace),
`wrong_section` (an extension-table row names a runtime schema method). Exit code 1 when `findings` is not empty.
`book-check` runs the same checks and prints the findings as errors, which is how the merge gate picks them up.

## 6. Guide page

`book/agents/book-coverage.md`: what each check requires, how to add a row for a new extension method, feature or
subcommand, the finding codes, the JSON shape, and the runtime tests that back the scans. A § 3 row for
`cargo xtask book-coverage` maps to it.

## 7. Determinism

Pure functions of the tree: every list is a `BTreeMap`/sorted `Vec`, findings are sorted and deduplicated, no
timestamps, paths normalized to `/`. The same tree gives the same bytes on Linux, Windows and macOS hosts.

## 8. Tests and acceptance

- Unit tests (`xtask`): the `MethodSchema` scan finds a literal's top-level `name`, ignores nested `name:` fields
  (`ParamSchema`), comments, strings and `#[cfg(test)]` items, and finds same-file constructor calls; table
  parsing (escaped pipes, a table continued after a paragraph, orphan rows ignored); each finding code from a
  fixture; feature parsing and the `regia_x/feat` subject; CLI enum scan with a `name =` override and kebab-case;
  registry parsing and format-name matching; fence counts; JSON shape and `format_version`.
- Fixture test (`xtask/tests/book_coverage.rs`): a temporary workspace (`regia_test_utils::unique_temp_dir`) with
  one undocumented extension method fails with `missing_row` and `untested`; adding the row, a page that names it
  and a test passes. The workspace test requires zero findings and checks that the scan sees the constructor-built
  methods and not the test-only ones (`editor.whoami`).
- Runtime tests: `regia_editor/tests/book_extension_coverage.rs`, `regia_steam/tests/book_coverage.rs`,
  `regia_agent/tests/book_extension_coverage.rs`, `regia_cli/tests/book_cli_coverage.rs`; and
  `regia_editor/tests/extension_method_calls.rs` plus a `steam.deck` test, which call by name the 25 extension
  methods no test named before (the rendering ones up to their parameter checks, which run before any device).
- Acceptance: `cargo xtask book-check` and `cargo xtask book-coverage` pass on the tree with zero findings;
  removing any one extension row makes both fail and names the method.

## 9. Platform notes

Host tooling only: runs wherever xtask runs (Linux, Windows, macOS hosts). Windows paths are normalized. Nothing
is compiled for Android aarch64 (no runtime crate changes); the lane still runs `cargo ndk -t arm64-v8a check -p
regia` to prove that.

## 10. Performance budget

No per-frame cost: nothing runs in a game. The scan reads about 1 400 source files under `crates/*/src` (and the test-literal pass about 2 400) once per
`book-check`; it adds
well under a second to a step dominated by the snippet compile. No `tracing` spans (not per-frame code).

## 11. Out of scope, and the guides remainder (ADR-2648)

Built after the first version (ADR-2648, 2026-11-05):

- **Fence ratchet.** `book/fence-baseline.json` (`{ "format_version": 1, "ignored_by_page": { page: n } }`) holds, per
  page, how many `rust,ignore` fences without an `<!-- ignore-reason: .. -->` comment on the line before them the
  page may keep. A page above its entry, or a page with no entry (a new page starts at 0), fails `book-check` and
  `book-coverage` with `ignored_fence_grew` (section `fence`). A page below its entry is a note. Ignored fences with a
  reason are counted in `fences.ignored` but not held. The harness compiles `rust,wrap` fences (the body inside a
  `Result`-returning function) and shows hidden `# ` lines to the compiler. Counts over `book/**` (generated
  reference pages included): 116 compiled and 196 ignored before, 187 compiled and 125 ignored after (71 converted).
- **Nested subcommands.** The scan follows `#[command(subcommand)]` variants and `#[command(subcommand)]` fields of the
  argument structs (`regia test session`) to the enums they name, and requires a CLI-table row for every path (a list
  row `regia plugin list`, `info <id>`, .. documents the siblings after its first span). A row that names an unknown
  nested path under a command with subcommands is `stale_row`. The report has `cli_nested` and
  `counts.cli_nested_commands`.
- **Stale § 4 rows.** A row whose Format cell ends in a parenthesized registry name, or whose Extension cell has a
  `` `name` vN `` claim, must name a format of `format-hashes.ron`; otherwise `stale_row`. Other row text is free.
- **`tool.<id>.<name>`.** The repository ships no editor tool manifest, so there is nothing to check; the exclusion
  stays and is enforced: a `*.tool.toml` outside `tests/`, `fixtures/`, `target/` and dot directories fails with
  `shipped_tool_unchecked` until its methods have rows and pages.

Still out of scope:

- `tool.<id>.<name>` methods of tools installed by a user (dynamic, from manifests).
- An outside-tester run of the book (master plan § 3.7): it needs a person who has not seen the engine; the lead
  schedules it.
- A `page_does_not_mention` check for Cargo features (ADR-5660): at the 2026-11-10 audit 84 of the 219 feature-to-page
  links named a page that does not name the feature as a whole token (most are the `regia_dylib` mirror features mapped
  to `user-guide/hot-reload.md`). Enforcing it needs those pages or rows fixed first; v0.5.x.

## 12. Provenance

Original code; no algorithm from a public source. Row in `docs/provenance.md`.
