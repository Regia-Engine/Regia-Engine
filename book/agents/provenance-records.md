# Checking provenance records

Run `cargo xtask check-provenance` from the repository root. It reads
`docs/provenance.md`, prints the data-row count and counts by declared primary source
class, and exits with status 1 if any row is incomplete. It writes nothing and does not
open external spec sets, source repositories or cited pages. The `book-check` CLI runs it
after its book checks pass, so CI and preflight enforce it too.

A Source cell starts with one of these declarations:

| Declaration | Evidence that follows it |
|---|---|
| `[class: public-literature]` | A public paper, book, article, tutorial or published talk |
| `[class: public-documentation]` | Public user-facing documentation, including public API-reference pages |
| `[class: standard]` | A named standard, specification or its public reference page |
| `[class: permissive-code; license: MIT]` | A permissive code reference with its license and attribution |
| `[class: reader-builder]` | An authorized reader/builder behavior spec, its link and the process statement |
| `[class: regia-design]` | Regia's own design, derivation or composition of existing systems |
| `[class: licensed-vendor-sdk; approval: ADR-2002]` | A vendor SDK used under a license the owner approved in that ADR; name the license and its public URL |

Mixed rows keep all their citations. The declaration names the primary class for counting;
it does not erase the other sources. Code declarations accept the permissive SPDX license
identifiers documented by the repository's cleanroom policy, joined by `AND` or `OR`.
`Apache-2.0 WITH LLVM-exception` is the one accepted license exception (ADR-4931).
Ported source still needs the existing attribution and notice-registry checks.

A row citing a public-sources set area also needs a direct HTTP(S) public-page URL in its
Source cell. A URL in Notes does not close the gap. Repository, private-network and
source/header/shader-file links do not count as public pages. This checks URL shape and
recognized material patterns; the spec writer must still confirm that the target is a
public reference page. Vendor names inside citation URLs are references; use Regia
terminology in the surrounding prose.

A `licensed-vendor-sdk` row is accepted only for an SDK whose license the owner approved in
an ADR that the checker records (today ADR-2002, the NVIDIA RTX SDKs license). The SDK itself
is never committed. A new SDK needs its own owner decision first; the class is never a way to
relabel a proprietary header as documentation or permissive code.

Reader/builder rows must cite the narrow ADR-1441 authorization and say that the reader wrote a behavior-only spec, the builder
worked from the spec only and never saw the GPL source, and must link the retained spec.
The four approved reader specs remain outside the tree; the log links their retained
locations on the owner's machine. No GPL repository is opened by this check.

| Diagnostic code | Repair |
|---|---|
| `row_shape` | Keep all four table cells, including an empty Notes cell; escape literal pipes |
| `source_class` | Record an allowed source class; resolve an unapproved source rather than relabeling it |
| `source_missing` | Supply the citation or describe the original design; remove placeholders |
| `code_license` | Record the permissive SPDX license from the source's license evidence |
| `set_url_missing` | Copy a usable public-page URL from that area's Sources section in the spec-writer role |
| `reader_authorization` | Cite the narrow owner authorization ADR-1441; other copyleft sources need their own owner decision |
| `reader_process` | Record the authorized reader/builder separation explicitly |
| `reader_spec_link` | Link the retained behavior spec |
| `no_rows` | Restore the provenance table; an empty log cannot pass |
| `sdk_approval` | Declare `; approval: ADR-…` naming the owner ADR that approved this SDK's license; an SDK without one cannot be cited |
| `sdk_license` | Name the approved SDK license and its public license URL in the Source cell |

Diagnostics include the exact Markdown line. This is a check of recorded declarations;
it does not certify a past author's reading history or establish the license of an
unverified source. Human review remains part of the cleanroom procedure.
