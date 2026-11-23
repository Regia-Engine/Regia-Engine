# Contributing to Regia Engine

Thank you for helping. This page says how a contribution gets in and what you agree to when you send one.

## Licences

| Part | Licence |
|---|---|
| Engine code (crates, editor, tools, `xtask`, shaders) | Mozilla Public License 2.0 ([`LICENSE`](LICENSE)) |
| Templates, samples and starter content | Code MIT-0, assets CC0-1.0 (the `LICENSE-*.txt` files beside them) |
| Documentation and the book | CC-BY-4.0 ([`book/LICENSE-CC-BY-4.0.txt`](book/LICENSE-CC-BY-4.0.txt)) |
| The name "Regia Engine" and the logo | [`TRADEMARKS.md`](TRADEMARKS.md) |

Your contribution is published under the licence of the part it changes.

## The contributor licence agreement (CLA)

Before we can merge your first pull request we need a signed contributor licence agreement:

- [`CLA.md`](CLA.md) holds the **individual** form (you contribute as yourself) and the **entity** form (you
  contribute for your employer or another organization; someone who can sign for it signs once and lists who may
  contribute).
- **You keep your copyright.** You grant Sphenisca Regia LLC (the licensor) a broad licence to your contribution, including the
  right to relicense it. That lets the project offer future versions under other terms (for example a supported or
  console edition) without tracking down every contributor again. The project still publishes your contribution
  under the open licence of the part it changes.
- **You sign once**, through the CLA bot on GitHub: open a pull request, and the bot comments with a link and the
  sentence to reply with. One signature covers all your later contributions.

> The CLA bot is not switched on yet: it starts once the licensor's registration is confirmed. Until then, pull
> requests from outside contributors wait; the maintainers will ask you to sign once the bot is live.

## Making a change

1. Read [`AGENTS.md`](AGENTS.md) (the working rules, including the cleanroom rule in
   [`docs/01-cleanroom.md`](docs/01-cleanroom.md)) and the spec your change touches under `docs/specs/`.
2. Never paste code from another engine's proprietary source. Code ported from a permissively licensed project keeps
   its licence header and gets a row in `docs/ported-sources.toml`.
3. Every new `.rs` or `.wgsl` file starts with its SPDX line (`// SPDX-License-Identifier: MPL-2.0`, or `MIT-0` in
   `samples/` and the templates). `cargo xtask spdx --write` adds it.
4. Format, lint and test: `cargo fmt --all`, then `cargo xtask preflight --changed`.
5. Open a pull request with what changed and why. Decisions that change a spec go in `docs/decisions.md`.

## Reporting a security problem

Do not open a public issue. Write to the maintainers privately (see the repository's security policy once it is
published) with the steps to reproduce.
