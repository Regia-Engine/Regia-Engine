# Template output path portability

This is an internal-only test oracle; it adds no agent method or runtime behavior. It checks every path returned by
`regia_templates::template_files` for the Windows/Linux portability rules needed by `regia new` output: relative
components only, no parent traversal, no case-insensitive path collisions, no trailing dot or space, and no reserved
Windows device basename. Identical basenames under different directories and extensionless dotfiles remain valid.

The test also checks each template's workspace members and local Cargo manifest paths against the embedded file map.
Engine paths containing `{{engine}}` are external to the generated project template and are skipped by that resolution
check. Local source-file paths such as a binary's `src/main.rs` are checked as files; dependency paths are checked for an
embedded `Cargo.toml`.

Run the checks with:

```sh
cargo test -p regia_templates
```

The negative fixtures exercise Unix and drive-letter absolute paths, parent components, `.` and `..` components,
case-folded collisions, trailing dots and spaces, and reserved names such as `CON.txt` and `Lpt9.data`. The duplicate
basename and dotfile fixtures are positive cases. This validator is test-only and has no protocol error codes.
