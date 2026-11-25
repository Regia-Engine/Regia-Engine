# Printing the manual

<!-- index: Printing the manual -->

The printable copy is the single HTML file the manual generator already writes:
`docs/manual/manual.html`, next to this book in an engine checkout. There is no separate PDF
build. A browser prints that file, including to a PDF, with the print rules below.

## Open it

Open [`docs/manual/manual.html`](../../docs/manual/manual.html) in a browser from the engine
checkout (a `file://` URL is enough; the file embeds its fonts and does not request the network).
Then use the browser's print command.

The page you are reading is one chapter of the mdBook. Printing the mdBook site does not use this
layout. Print `manual.html`.

## What the print layout does

- A contents list is the first printed page. It is hidden on screen.
- Each top-level chapter starts on a new page. Nested chapters stay with the chapter that contains
  them. The first chapter does not insert a blank page in front of itself.
- Body type is 11 point. Code, tables and quotations avoid a break inside them. Headings avoid a
  break immediately after them.
- The on-screen theme switch, search box and sidebar are not printed. The paper colors are the
  ones the HTML file already uses for its light theme.

The generator that writes the file is `cargo xtask manual`. It also refreshes
[the catalog](catalog.md) and [the index](index.md). Do not edit `manual.html` by hand.
