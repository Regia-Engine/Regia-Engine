# Stand-in OIDN install (check only)

`cargo xtask check-features` points `OIDN_DIR` here for its `denoise` rows (ADR-2322): the `oidn` crate's build
script accepts any `OIDN_DIR` and only emits a link path, which `cargo check` and `cargo clippy` never use, so the
rows compile `regia_pt::denoise` without Intel's Open Image Denoise library. Nothing here is a library; a build
that links with this `OIDN_DIR` fails. A real build sets `OIDN_DIR` to an OIDN install (`book/user-guide`, regia_pt).
