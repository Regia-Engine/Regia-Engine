# Book coverage

Every shipped feature has a guide page (rule 8). `cargo xtask book-check` already proves it for the runtime agent
protocol: each method of the schema table needs a [coverage](../COVERAGE.md) row, a page that names it, an
`agent.guide` topic, a contract and a test call. `cargo xtask book-coverage` covers the rest of the shipped surface
(spec 96, ADR-1704):

| What | Found in | Needs |
|---|---|---|
| Host extension methods (`editor.*`, `steam.*`, `physics.cloth_stats`) | `MethodSchema { name: "..", .. }` literals and same-file constructor calls (`schema("editor.tool_list", ..)`) in non-test code under `crates/*/src`, minus the runtime schema | A row in § 5 of `COVERAGE.md` that names the registering crate, an agent page that names the method, a test-code string literal that calls it |
| Cargo features | The `[features]` keys of every `crates/*/Cargo.toml` | A row in § 1 for the crate whose Feature cell has the feature in backticks, linking a page that exists |
| `regia` subcommands | The variants of `enum CliCommand` in `crates/regia_cli/src/main.rs`, kebab-cased as clap names them, and every nested subcommand (`regia steam upload`, `regia procgen graph check`, `regia test session`): the enums reached through `#[command(subcommand)]` variants and struct fields | A row in § 3 whose command cell starts with `` `regia <name>`` (nested: `` `regia steam upload`` or a list row such as `` `regia plugin list`, `info <id>` ``) |
| Frozen content formats | The registry `docs/formats/format-hashes.ron` (`cargo xtask check-formats`) | A row in § 4 whose Format or Extension cell names the format (`render-job` is named by `.render_job.ron`; otherwise put the registry name in backticks), linking a page that exists |

It is developer tooling, not a runtime feature, so there is no protocol method: an agent runs the command and reads
the JSON. `book-check` runs the same checks on the real tree (the merge gate runs `book-check`).

## Run it

```sh
cargo xtask book-coverage           # text: one error line per finding, exit 1 when there is any
cargo xtask book-coverage --json    # the report below
cargo xtask book-check              # everything, including these checks
```

## The JSON report

```json
{
  "format_version": 1,
  "ok": false,
  "counts": { "extension_methods": 133, "features": 41, "cli_commands": 22, "cli_nested_commands": 30, "formats": 45 },
  "extension_methods": { "editor.select": "regia_editor", "steam.status": "regia_steam" },
  "findings": [
    { "section": "extension", "code": "missing_row", "subject": "editor.select",
      "message": "extension method `editor.select` (registered in `regia_editor`) has no row in the book/COVERAGE.md extension table" }
  ],
  "notes": [],
  "fences": { "compiled": 111, "ignored": 131, "ignored_by_page": { "user-guide/audio.md": 4 },
              "unjustified_by_page": { "user-guide/audio.md": 4 } }
}
```

`section` is `extension`, `feature`, `cli`, `format`, `panel` or `fence`. `subject` is the method, `crate/feature`, `regia <command>` or the
format's registry name. `notes`
lists observations that do not fail: a feature row for a crate with no Cargo features, or whose backticked tokens name none of its crate's features (the asset-pipeline modules row today).
`fences` counts the book's Rust fences: `compiled` are the `rust` fences `book-check` compiles, `ignored` the
`rust,ignore` fragments it does not, and `unjustified_by_page` the ignored ones without an `ignore-reason` comment,
which the ratchet below holds to `book/fence-baseline.json`. `format_version` stays 1 while fields are only added; a renamed or removed field or code bumps it.

## Finding codes

| Code | Meaning | Fix |
|---|---|---|
| `missing_row` | The method, feature or subcommand has no row in its table | Add the row (at the end of the table) |
| `missing_page` | The row links no page, or a page that does not exist | Link the page that documents it |
| `page_does_not_mention` | The extension method's page does not name it as a whole token (`editor.play` is not named by `editor.play_state`) | Document the method on that page, or map the row to the page that does |
| `untested` | No string literal in test code names the extension method | Add a test that calls it |
| `wrong_registrar` | The row's `Registered by` cell does not name the crate the method lives in | Name the crate in backticks |
| `stale_row` | The row names a method nothing registers, a crate that is not in the workspace (`crates/` or `samples/`), or a subcommand `regia` does not have | Remove or fix the row |
| `wrong_section` | An extension-table row names a runtime schema method | Move it to the method table (§ 2) |
| `ignored_fence_grew` | A page has more `rust,ignore` fences without a reason than its entry in `book/fence-baseline.json` (a page with no entry may have none) | Make the fence compile (below), or put `<!-- ignore-reason: why -->` on the line before it |
| `shipped_tool_unchecked` | The repository ships an editor tool manifest (`*.tool.toml` outside `tests/` and `fixtures/`), whose `tool.<id>.<name>` methods nothing checks | Add a row and a page for each of its methods, then extend the check (ADR-2648) |
| `duplicate_row` | A `Panel` variant is in both panel tables of the editor guide index | Keep one row |
| `empty_reason` | A panel in the guide's "no page of their own" table gives no reason | Say why it has no page |

A § 4 row also goes stale (`stale_row`) when its Format cell ends in a parenthesized registry name (`Data table
(`data-table`)`) or its Extension cell names one with a version (`` `regiatrace` v1 ``) that
`docs/formats/format-hashes.ron` no longer registers. Other text in the row is free.

## Rust fences and the ratchet

`book-check` compiles every `rust` fence as part of a throwaway crate. A fence that is a module body (items only)
compiles as written. A fragment of statements compiles with `rust,wrap`: the page shows the statements, and the check
wraps them in a function that returns `Result<(), Box<dyn Error>>`, so `?` works and the body ends with a `;`. Lines
that start with `# ` are hidden by mdBook and compiled: put the setup the reader should not see there (`# use regia::prelude::*;`,
`# let mut commands: Commands = todo!();`). Fix an example that does not match the API; do not ignore it.

A fence that cannot compile (a signature sketch, code that needs a GPU device or another game's types) stays
`rust,ignore` and needs a reason on the line before it:

```text
<!-- ignore-reason: signature sketch of the engine's API, not a body -->
```

The ratchet is `book/fence-baseline.json` (`{ "format_version": 1, "ignored_by_page": { "<page>": n } }`): the number of
ignored fences without a reason each page may keep. A page may not go above its entry; a page with no entry starts at 0.
Lowering is never required, but `book-coverage` prints a note when a page is below its entry; change the file to the
current `fences.unjustified_by_page` of `cargo xtask book-coverage --json`.

## Editor panels

The editor guide index (`book/user-guide/editor-guide/index.md`) has two tables: `| Panel variant | Tab | Page |` with one
row per `Panel` variant (`crates/regia_editor/src/shell.rs`) that links its page, and `| Panel variant | Why it has no
page of its own |` for the panels that have none yet, each with a reason. A variant in neither table is `missing_row`; a
link to a page that does not exist is `missing_page` (links are relative to the index); a row for a name that is not a
variant is `stale_row`. A new `Panel` variant therefore ships with a page, or with a reasoned exemption row.

## Adding a new extension method

1. Register it as usual (for the editor, add it to `regia_editor::agent_ext::all_methods` or the module's
   `methods()`).
2. Document it on its agent page, by its full name.
3. Append a row to § 5 of `book/COVERAGE.md`: the method in backticks, the registering crate in backticks (the
   module in parentheses), and a link to the page from step 2. The `editor.select` row there is an example.
4. Call it from a test (the string literal `"editor.my_method"` in test code).

## What backs the scans

The scans read source text, so each registering crate also has a test that checks what it really registers against
the same table: `regia_editor/tests/book_extension_coverage.rs` (installs the editor methods on a world),
`regia_steam/tests/book_coverage.rs` (the Steam plugin on the mock backend), `regia_agent/tests/book_extension_coverage.rs`
(the cloth methods) and `regia_cli/tests/book_cli_coverage.rs` (the `Commands:` list of the built `regia -h`). A method
the scan misses fails one of them; a row for a method the scan does not see fails `book-coverage` as `stale_row`.

Not covered: the `tool.<id>.<name>` methods of external editor tools. They come from a tool's manifest (see
[editor tools](editor-tools.md)); the repository ships none, and `book-coverage` reports `shipped_tool_unchecked` if one
appears (ADR-2648). § 4 rows for things that are not frozen formats are free text; only a row that names a registry
format is checked.

## Documentation links

`cargo xtask docs-links` checks relative Markdown links and image targets under `docs/`. Current misses are recorded in
[`docs/ci/docs-links.toml`](../../docs/ci/docs-links.toml); remove a row when its link is fixed.
