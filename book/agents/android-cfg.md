# Android cfg compilation check

Run `cargo xtask android-cfg` to find crates whose `crates/*/src` files contain
`target_os = "android"`. The command checks `regia_cli` and `regia_templates` for
`aarch64-linux-android`. It prefers `cargo ndk -t arm64-v8a check -p CRATE` when
`cargo-ndk` is installed and otherwise invokes `cargo check --target aarch64-linux-android
-p CRATE`.

The command lists all crates it finds. Crates beyond `regia_cli` and `regia_templates` are
informational rows: they are not compiled by this command and do not affect its exit status.
An Android check failure in either selected package makes the command fail; the Rust compiler's
diagnostics are printed for investigation.
