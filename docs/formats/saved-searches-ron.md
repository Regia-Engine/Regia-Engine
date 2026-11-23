# `saved_searches.ron` (universal-search saved searches, v1)

Spec: the UI parity addendum (P-021), ADR-7766. Personal searches and the palette favorites are in the user config
directory (`<user config>/regia/saved_searches.ron`); the project's shared searches are `config/saved_searches.ron`
(no favorites). Registered in `cargo xtask check-formats` and `regia upgrade-project` as `saved-searches` (ADR-7902; the
shape is guarded through `regia_core::editor_formats`, kept equal to the editor's types by `regia_editor`'s
`user_formats` tests). Fuzz target `editor.saved_searches` (`regia_editor::baseline_fuzz`); corpus fixture
`crates/regia_agent/tests/fixtures/format-corpus/saved-searches/` (loaded by `crates/regia_editor/tests/format_corpus.rs`).

```ron
(
    format_version: 1,                          // must be 1
    searches: [                                 // optional; side bar order
        (name: "Lights", query: "e: kind:light", providers: []),  // providers optional; empty = the query's or all
    ],
    favorites: ["view.toggle_grid"],            // optional; palette favorites (personal file only)
)
```

- Names are unique within a file; a repeated or invalid name is dropped on read. Saving a name that exists replaces
  its query.
- A file with a newer `format_version` is read but never written (saving answers `saved_search_read_only`);
  `regia upgrade-project` reports its version and never rewrites it.
