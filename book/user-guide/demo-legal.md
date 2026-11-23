# Demo legal notices

The free showcase demo (`docs/showcase-plan.md`) is an offline download. Its legal files are kept apart from the
engine-wide ones because the demo links far fewer crates than the editor and tools do.

## Files

| File | What it is |
|---|---|
| `docs/demo/EULA-DRAFT.md` | The demo end user agreement, a **draft the owner approves**. Brackets mark owner decisions: video policy, redistribution, telemetry, jurisdiction, contact. |
| `docs/demo/THIRD_PARTY_NOTICES-demo.md` | Generated and committed. The crates the demo build links (`showcase --no-default-features --features demo`: the agent protocol and every network crate compiled out) with licence and source, the ported source files inside those crates, upscaler terms, the asset-credit section and the privacy facts. `cargo xtask preflight` checks it is up to date. |

`LICENSE` is the engine licence (MPL-2.0, ADR-10225) and is not changed by the demo; the demo bundle ships it as
`licenses/LICENSE` and its `CREDITS.txt` names where the engine's source is ([Licensing](licensing.md)).

## Commands

```
cargo xtask demo-notices            # write docs/demo/THIRD_PARTY_NOTICES-demo.md
cargo xtask demo-notices --check    # fail if it is out of date
cargo xtask demo-notices --verify target/demo-package
```

Every run also fails when:

- the demo tree contains a crate that could send data off the machine: `regia_crash` (the crash
  uploader), `regia_steam`/`steamworks`/`steamworks-sys` (Steamworks), `regia_dlss` (the DLSS SDK
  shim), the agent protocol and net transport (`regia_agent`, `regia_net`, `tungstenite`, `snow`),
  an HTTP client (`ureq`, `reqwest`, `hyper`) or a TLS stack (`rustls`, `native-tls`, `openssl`).
  The list fails closed: anything that can open a socket is named. The demo is offline with no
  crash upload and no telemetry upload.
- the EULA draft lost a section (including `## 9. General`, the governing-law and contact section)
  or the word `DRAFT`.

`--verify DIR` is a standalone command: it writes nothing and adds the package checks to the ones
above (it does not compare the repository's notices file). It checks
every EULA and notices copy in the folder (sorted, so the result does not depend on filesystem
order), rejects an EULA that is still the draft (the word `DRAFT`) or has unresolved `[...]` owner
brackets, requires every numbered section of the approved text, compares the notices against the
freshly rendered text, and scans every executable and shared library (`exe`, `dll`, `so`, `dylib`,
case-insensitive, regardless of file mode) for a forbidden crate's name.

## Privacy facts the build keeps

- `regia_crash` uploads only with an endpoint and consent set (both off by default) and is not in the demo's tree at
  all. A crash writes a report file locally (`regia_platform::crash`); nothing sends it.
- `regia_telemetry` writes frame-time summaries to the local log folder. It has no network code.
- If the owner chooses opt-in telemetry, the EULA section 4 is rewritten and the checks above change with a code
  change first.

## Still pending

- Asset credits wait for the content pack; the notices file says so.
- The owner approves the EULA text, the video policy and the open points listed at its end.
- DLSS and XeSS are not linked; their SDK terms are listed in the notices file for review before either ships.
