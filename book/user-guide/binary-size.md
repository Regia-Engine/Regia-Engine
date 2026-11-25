# Binary size

<!-- index: Binary size -->
<!-- index: apk-size -->

The executable (or the Android `.so`) is the biggest single file of most games. It sets the Steam
download, the install size on a phone and how long the final link takes. `regia apk-size` splits an
Android archive by kind of entry; `cargo xtask binary-size` goes one level deeper and tells you which
crates and which generic functions make the native code large, then keeps a budget so the size does not
grow by accident. A shipped package strips the game binary and keeps symbols beside it
([Packaging](packaging.md)). `cargo xtask binary-size` measures the build before that strip.

```console
$ cargo xtask binary-size                       # blank and third_person on this machine
$ cargo xtask binary-size --template quick --json
$ cargo xtask binary-size --target aarch64-linux-android      # needs cargo-ndk and the NDK
$ cargo xtask binary-size --target x86_64-pc-windows-msvc     # needs cargo-xwin; sections only
$ cargo xtask binary-size --file path/to/game --target x86_64-unknown-linux-gnu
$ cargo xtask binary-size --check               # fail above docs/ci/size-budgets.toml
$ cargo xtask binary-size --update              # record the budgets again
```

The command generates each template with `regia new`, builds it with the shipping profile (fat LTO, one
codegen unit, `panic = "abort"`, symbol table kept) and reads the result. It builds each template once per
target (two by default), so expect a long run; it is a nightly job, not part of preflight. The generated
projects share one cargo target directory under the temp dir, removed after the run; `--target-dir <dir>`
uses (and keeps) a directory of your own, so a later run reuses its build.

## Reading the report

```text
== blank on x86_64-unknown-linux-gnu (elf) ==
file 212.40 MiB | loaded 71.03 MiB | not loaded (symbols, debug info) 141.37 MiB
text 38.10 MiB | rodata 22.00 MiB | data 4.90 MiB | bss 0.40 MiB
```

- **loaded** is what the operating system maps: text (code), rodata (strings, tables, vtables, unwind
  data), data (initialized globals, relocation targets) and bss (zeroed globals, no file bytes). This is the
  number the budget tracks. It does not change when you strip the file.
- **not loaded** is the symbol table and debug line tables. `regia package` extracts them to a
  `symbols/` folder and strips the shipped copy, so they do not reach players.
- **top crates** lists each crate's symbol bytes, biggest first. A symbol belongs to the crate in its path;
  `<A as Trait>::method` belongs to the crate of `A` (or of `Trait` when `A` is a primitive).
- **`(no symbol)`** is every loaded byte no symbol covers: string literals, unwind tables, relocations,
  padding. It is a line of its own and is never spread over the crates, so a crate's number is only what its
  named code and data weigh. When this line is large, the crate numbers understate the real cost of
  everything that carries strings or `Debug` output.
- **`(native)`** is C and C++ libraries linked in (their symbols are not Rust paths).
- **top symbols** are the 30 biggest single functions or statics (`--top N` sets the row count of this
  list and the crate list).
- **generic paths with the most instantiations** counts symbols that have the same path and different type
  parameters. Each copy is separate machine code. A function with dozens of copies is worth turning into a
  thin generic wrapper around a non-generic inner function, or into a trait object.

The Windows report has section sizes only: its symbols are in the PDB, which the tool does not read. Use the
Linux or Android numbers for the crate split, the engine code is the same. A stripped binary reports
nearly everything under `(no symbol)` and says so.

## What usually makes a binary large

- **Generic bloat**: one function instantiated per type, found in the generics list.
- **Crates that should not ship**: an editor, importer or debug-only crate reached through a default feature.
  A crate in the top list that your game never uses is a feature to switch off (`book/user-guide/cargo-features.md`).
- **Duplicated dependency versions**: two versions of one crate show up as two rows with the same prefix.
  `cargo tree -d` lists them.
- **Panic and format machinery**: every `unwrap`, `expect`, `assert!` and `{:?}` carries a message and a
  location string; they land in rodata and in `(no symbol)`.

## Budgets

`docs/ci/size-budgets.toml` (`format_version = 1`) has one `[[budget]]` per template and target with the
loaded total, the biggest crates and a `tolerance_percent` (default 3). `--check` exits 1 when the total or a
crate is above the tolerance (a crate also has 16 KiB of slack, and a crate with no row fails above 64 KiB, or above the smallest
recorded crate plus tolerance when the row holds the full 40 crates),
and prints a hint when the total is far below the budget so you can lock the gain in with `--update`.
Re-record only when the growth is intended, and say why in the commit message. Budgets are only comparable on
the same compiler version and flags.

See also [Android packaging](android.md) for `regia apk-size`, which splits an APK or AAB by entry kind.
