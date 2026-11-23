# Licensing

Regia Engine is open source under the **Mozilla Public License 2.0** (MPL-2.0). This page says in plain English what
that means for you. It is not legal advice; the licence text in `LICENSE` is what counts.

## The short version

| You | What you need to do |
|---|---|
| Write a game, tool or plugin on Regia Engine | Nothing for your own code. It stays yours, under any terms you like, and can be closed source. |
| Ship that game | Tell players where Regia Engine's source code is. `regia package` writes this for you. |
| Change one of Regia Engine's own files and ship it | Share the source of the files you changed (under the MPL), and point the notice at it. |
| Copy code or assets from a template or sample | Nothing. Template and sample code is MIT-0 and their assets are CC0: no conditions, no credit needed. |
| Quote or adapt this book | Credit it. The documentation is CC-BY-4.0. |
| Use the name "Regia Engine" | "Made with Regia Engine" is welcome; see **Trademarks** below. |

There is no fee, no royalty, no revenue report and nothing to sign or accept to use the engine.

## Your game code stays yours

The MPL works file by file. Files you write (your game's crates, scripts, shaders, tools, content) are not Regia
Engine's files, so the MPL does not apply to them, even when they are compiled into one executable with the engine.
You can keep them closed, sell them, or open them under any licence. This is also how private platform modules
work: write them as separate files and they stay private.

## Changes to Regia Engine's own files are shared

If you change a file that came from Regia Engine (anything under the engine's `crates/`, `xtask/` or its shaders)
and you ship a build that contains the change, that file stays under the MPL and you must make its source available
to the people you ship to. Usually that means publishing your copy of the engine (a public fork is fine) and setting
the link in your project:

```toml
# Regia.toml
[package]
engine_source_url = "https://example.com/your-studio/regia-engine-fork"
```

Changes you never ship (local experiments, internal builds) carry no obligation. Shared changes can be merged back
into Regia Engine, which is the point of this choice.

## What `regia package` writes

Every package (Linux, Windows, Android and dedicated server) gets these files next to the game:

- `REGIA-ENGINE-NOTICE.txt`: says the program includes Regia Engine under the MPL-2.0 and where its source code is.
  The location is `[package] engine_source_url`, or the engine's own setting when you have not changed any engine
  file, which is the public repository `https://github.com/Regia-Engine/Regia-Engine`. If your `Regia.toml` still
  sets the old placeholder `<release host>`, a shipping package (`--profile shipping`) prints a warning that names
  the key to set.
- `REGIA-ENGINE-LICENSE.txt`: the MPL-2.0 text.
- `CREDITS.txt`: your project's own `CREDITS.txt` (if it has one), then the line **Made with Regia Engine**.
- `THIRD_PARTY_NOTICES.md`: the open-source components inside the engine and their licences
  ([Bill of materials and licence audit](sbom.md)).

The notice and the licence text are always written. The credit line is not a licence condition: it is a default you
can turn off.

```sh
regia package --platform linux --no-engine-credit
```

or, for every package of the project:

```toml
# Regia.toml
[package]
engine_credit = false
```

## Templates and samples are free to use

Code in the project templates (`regia new`) and in the engine's `samples/` is under **MIT No Attribution** (MIT-0),
and their assets under **CC0 1.0**. Copy, change and ship them in anything, closed or open, with no notice and no
credit. The licence texts sit beside them (`LICENSE-MIT-0.txt`, `LICENSE-CC0-1.0.txt`). A new project's source files
start with `// SPDX-License-Identifier: MIT-0`; replace that line with your own licence if you like.

## Trademarks

The MPL does not cover the name "Regia Engine" or its logo. `TRADEMARKS.md` in the engine repository has the policy.
In short:

- "Made with Regia Engine" in credits, store pages and trailers is allowed and welcome, with or without the logo.
- Redistributing an unmodified release under its name is allowed.
- A modified engine you ship as a product (an editor, an SDK, a fork) must use a name of its own; "based on Regia
  Engine" is fine. A game built on a changed engine is not an engine product.
- Nothing may suggest that the Regia Engine project endorses you or your product.

## Contributing

Contributions to Regia Engine itself are welcome under a contributor licence agreement: you keep your copyright and
grant the licensor, Sphenisca Regia LLC, the right to relicense your contribution. See `CONTRIBUTING.md` and `CLA.md`
in the repository. The engine's source is at https://github.com/Regia-Engine/Regia-Engine.

## Where the details are

- `LICENSE`: the MPL-2.0 text. `docs/07-licensing.md`: the decision and its reasons (ADR-10225).
- `TRADEMARKS.md`, `CONTRIBUTING.md`, `CLA.md`.
- [Packaging](packaging.md) for the rest of what a package holds.
