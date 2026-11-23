// SPDX-License-Identifier: MPL-2.0
//! End-to-end tests for `cargo xtask manual`: the real book renders into one self-contained HTML
//! file (no network requests, every in-file link resolves, every summary chapter present), the
//! render is reproducible, and the generated reference pages cover the code they come from.

use std::collections::BTreeSet;
use std::path::PathBuf;

use xtask::manual;

/// The repository's `book/` directory.
fn book_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent")
        .join("book")
}

/// Every value of `attr="…"` in `html`.
fn attr_values(html: &str, attr: &str) -> Vec<String> {
    let needle = format!("{attr}=\"");
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(&needle) {
        let after = &rest[i + needle.len()..];
        let Some(end) = after.find('"') else { break };
        out.push(after[..end].to_string());
        rest = &after[end..];
    }
    out
}

#[test]
fn the_manual_is_self_contained_and_its_links_resolve() {
    let full = manual::render_manual(&book_dir()).expect("the book renders");
    // The document without its closing script (whose string literals build result links).
    let html = &full[..full.rfind("<script>").unwrap_or(full.len())];

    // No external resources: no stylesheet links, no remote scripts, images or fonts.
    assert!(
        !html.contains("<link"),
        "the manual must not link external stylesheets"
    );
    for src in attr_values(&full, "src") {
        assert!(
            src.starts_with("data:") || !src.contains("://"),
            "external resource `{src}` would need the network"
        );
    }
    assert!(!html.contains("@import"), "no CSS imports");
    assert!(!html.contains("url(http"), "no remote CSS urls");

    // Every in-file anchor target exists.
    let ids: BTreeSet<String> = attr_values(html, "id").into_iter().collect();
    let mut dead = Vec::new();
    for href in attr_values(html, "href") {
        if let Some(target) = href.strip_prefix('#')
            && !target.is_empty()
            && !ids.contains(target)
        {
            dead.push(href);
        }
    }
    dead.sort();
    dead.dedup();
    assert!(dead.is_empty(), "in-file links without a target: {dead:?}");

    // Every summary chapter is a section, and links between pages became anchors.
    let summary = std::fs::read_to_string(book_dir().join("SUMMARY.md")).expect("SUMMARY.md");
    for path in manual::chapter_paths(&summary) {
        let id = manual::page_id(&path.to_string_lossy());
        assert!(
            html.contains(&format!("<section class=\"chapter\" id=\"{id}\"")),
            "chapter {id} missing"
        );
    }
    for href in attr_values(html, "href") {
        assert!(
            href.contains("://")
                || href.starts_with('#')
                || href.starts_with("mailto:")
                || !href.ends_with(".md"),
            "link `{href}` still points at a markdown file"
        );
    }

    // Search, theme switch and highlighted code are present.
    assert!(html.contains("id=\"search\""));
    assert!(html.contains("id=\"theme\""));
    assert!(html.contains("<span class=\"kw\">fn</span>"));

    // PiercingXX brand: embedded OFL faces, AMOLED / Paper tokens, the underlined-XX mark.
    assert!(html.contains("class=\"pxx-manual\""));
    assert!(html.contains("data-theme=\"dark\""));
    // First three wOFF2 bytes (`wOF`) encode as `d09G`. The fourth byte joins the next group.
    assert!(html.contains("data:font/woff2;base64,d09G"));
    assert!(html.contains("Space Mono"));
    assert!(html.contains("JetBrains Mono"));
    assert!(html.contains("#000000"));
    assert!(html.contains("#fdba74"));
    assert!(html.contains("#ff6767"));
    assert!(html.contains("#f3eee2"));
    assert!(html.contains("#b45309"));
    // Paper's error is the brand's light functional red (BRAND-GUIDE 3.5, ADR-8585).
    assert!(html.contains("#8f3939"));
    // Real underlined-XX mark (stacked, stroke 6), not the round-1 side-by-side reconstruction.
    assert!(html.contains("M40,26 L68,54"));
    assert!(html.contains("M40,52 L68,80"));
    assert!(html.contains("M38,88 L70,88"));
    assert!(!html.contains("M37 42"), "reconstructed side-by-side mark");
    assert!(html.contains("class=\"wordmark\">Regia Engine</span>"));
    assert!(!html.contains("quick walk"), "soft frame left in the editor tour");
    assert!(html.contains("class=\"callout"));
    assert!(html.contains("class=\"status planned\""));
    assert!(!html.contains("fonts.googleapis"));
    assert!(!html.contains("prefers-color-scheme"));
    assert!(!html.contains("#c4532a"));
    assert!(!html.contains("__SPACE400__"));
}

/// Standard base64 decode. Padding is required; the alphabet is the one `manual` encodes with.
fn base64_decode(data: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Result<u8, String> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("bad base64 byte {c}")),
        }
    }
    let bytes = data.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return Err(format!("bad base64 length {}", bytes.len()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && chunk[3] != b'=') {
            return Err("bad base64 padding".into());
        }
        let a = u32::from(val(chunk[0])?);
        let b = u32::from(val(chunk[1])?);
        let c = if chunk[2] == b'=' {
            0
        } else {
            u32::from(val(chunk[2])?)
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            u32::from(val(chunk[3])?)
        };
        out.push(((a << 2) | (b >> 4)) as u8);
        if pad < 2 {
            out.push(((b << 4) | (c >> 2)) as u8);
        }
        if pad < 1 {
            out.push(((c << 6) | d) as u8);
        }
    }
    Ok(out)
}

/// The `data:font/woff2;base64,…` payloads in `html`, in document order.
fn font_payloads(html: &str) -> Vec<Vec<u8>> {
    let needle = "data:font/woff2;base64,";
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(needle) {
        let after = &rest[i + needle.len()..];
        let end = after
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '/' && c != '=')
            .unwrap_or(after.len());
        out.push(base64_decode(&after[..end]).expect("font data URI is valid base64"));
        rest = &after[end..];
    }
    out
}

#[test]
fn embedded_fonts_decode_to_the_woff2_files_and_strong_inverts() {
    assert_eq!(base64_decode("Zm9v").expect("foo"), b"foo");
    assert_eq!(base64_decode("Zg==").expect("f"), b"f");
    assert_eq!(base64_decode("Zm8=").expect("fo"), b"fo");

    let book = book_dir();
    let html = manual::render_manual(&book).expect("the book renders");
    let fonts = [
        "theme/fonts/SpaceMono-400-latin.woff2",
        "theme/fonts/SpaceMono-700-latin.woff2",
        "theme/fonts/JetBrainsMono-latin.woff2",
    ];
    let payloads = font_payloads(&html);
    assert_eq!(payloads.len(), 3, "the manual embeds exactly three woff2 faces");
    for (rel, payload) in fonts.iter().zip(payloads.iter()) {
        let bytes = std::fs::read(book.join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"));
        assert!(
            payload == &bytes,
            "{rel}: data URI decodes to {} bytes, file is {} bytes",
            payload.len(),
            bytes.len()
        );
        assert_eq!(&payload[..4], b"wOF2");
    }

    let strong_at = html.find("strong {").expect("strong rule");
    let strong_block = &html[strong_at
        ..html[strong_at..]
            .find('}')
            .map(|n| strong_at + n)
            .unwrap_or(html.len())];
    assert!(
        strong_block.contains("background: var(--pxx-emphasis-bg);"),
        "strong must be a Signal block: {strong_block}"
    );
    assert!(
        strong_block.contains("color: var(--pxx-emphasis-fg);"),
        "strong must be Ink text: {strong_block}"
    );

    let theme_at = html.find("html.pxx-manual #theme,").expect("theme button rule");
    let theme_end = html[theme_at..]
        .find('}')
        .map(|n| theme_at + n)
        .unwrap_or(html.len());
    let theme_block = &html[theme_at..theme_end];
    assert!(
        theme_block.contains("#menu")
            && theme_block.contains("background: var(--pxx-emphasis-bg);")
            && theme_block.contains("color: var(--pxx-emphasis-fg);")
            && !theme_block.contains("transparent"),
        "theme and menu buttons must invert (Signal block, Ink text): {theme_block}"
    );

    // Paper surfaces: the UI kit's piercingxx-paper cards, dialogs and fields (ADR-8585).
    assert!(html.contains("--pxx-ink-raised: #ece7db;"));
    assert!(html.contains("--pxx-graphite: #e6e1d6;"));
    assert!(html.contains("--pxx-slate: #e1dcd1;"));
    assert!(!html.contains("#ebe6da") && !html.contains("#e3ddd0") && !html.contains("#dad4c6"));

    let mark = std::fs::read_to_string(book.join("theme/logomark.svg")).expect("logomark");
    let head = std::fs::read_to_string(book.join("theme/head.hbs")).expect("head.hbs");
    for needle in [
        "M40,26 L68,54",
        "M40,52 L68,80",
        "M38,88 L70,88",
        "stroke-width=\"6\"",
    ] {
        assert!(mark.contains(needle), "logomark missing {needle}");
        assert!(head.contains(needle), "mdBook mark missing {needle}");
        assert!(html.contains(needle), "manual missing {needle}");
    }
}

#[test]
fn rendering_is_reproducible() {
    let a = manual::render_manual(&book_dir()).expect("first render");
    let b = manual::render_manual(&book_dir()).expect("second render");
    assert!(a == b, "two renders of the same book differ");
}

#[test]
fn generated_reference_pages_cover_the_code() {
    let pages = manual::generated_pages(&book_dir()).expect("generated pages");
    let methods = &pages
        .iter()
        .find(|(rel, _)| rel == "reference/agent-methods.md")
        .expect("agent methods page")
        .1;
    for m in regia_agent::schema::METHODS {
        assert!(
            methods.contains(&format!("| `{}` |", m.name)),
            "{} missing",
            m.name
        );
    }
    let formats = &pages
        .iter()
        .find(|(rel, _)| rel == "reference/formats.md")
        .expect("formats page")
        .1;
    for name in ["material", "scene", "ui", "vfx", "animgraph", "save", "pak"] {
        assert!(
            formats.contains(&format!("| `{name}` |")),
            "format {name} missing"
        );
    }
    let profiles = &pages
        .iter()
        .find(|(rel, _)| rel == "reference/render-profiles.md")
        .expect("profiles page")
        .1;
    assert!(profiles.contains("| `render_scale` |"));
    assert!(profiles.contains("DesktopHigh"));
}

/// sRGB channel (0–255) to linear light, as WCAG 2 defines relative luminance.
fn linear(channel: u8) -> f64 {
    let c = f64::from(channel) / 255.0;
    if c <= 0.039_28 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG 2 relative luminance of a `#rrggbb` colour.
fn luminance(hex: &str) -> f64 {
    let hex = hex.trim_start_matches('#');
    assert_eq!(hex.len(), 6, "#rrggbb colour: {hex}");
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex channel");
    0.2126 * linear(channel(0)) + 0.7152 * linear(channel(2)) + 0.0722 * linear(channel(4))
}

/// WCAG 2 contrast ratio of two `#rrggbb` colours.
fn contrast(a: &str, b: &str) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The value of `--name: value;` inside the first block that follows `selector`.
fn token_after(css: &str, selector: &str, name: &str) -> String {
    let start = css
        .find(selector)
        .unwrap_or_else(|| panic!("{selector} in the theme"));
    let block = &css[start..];
    let block = &block[..block.find('}').expect("block end")];
    let needle = format!("{name}:");
    let at = block
        .find(&needle)
        .unwrap_or_else(|| panic!("{name} in {selector}"));
    let value = &block[at + needle.len()..];
    value[..value.find(';').expect("declaration end")]
        .trim()
        .to_string()
}

#[test]
fn paper_status_colours_are_readable_on_paper() {
    // The brand palette defines only Night status colours (#fdba74 warn is ~1.5:1 on Paper);
    // Paper uses darker shades of the same hues. Swapping the Night values back in fails here.
    const PAPER: &str = "#f3eee2";
    for theme in ["brand.css", "piercing.css"] {
        let css = std::fs::read_to_string(book_dir().join("theme").join(theme)).expect("theme css");
        let paper = "html[data-theme=\"light\"]";
        for name in ["--pxx-warn", "--pxx-error"] {
            let value = token_after(&css, paper, name);
            let ratio = contrast(&value, PAPER);
            assert!(
                ratio >= 4.3,
                "{theme}: Paper {name} {value} is {ratio:.2}:1 on {PAPER}, needs >= 4.3:1"
            );
        }
        // mdBook's own light-theme variables (`html.light, html.rust { … }`).
        let border = token_after(&css, "\nhtml.light, html.rust {", "--warning-border");
        let ratio = contrast(&border, PAPER);
        assert!(
            ratio >= 4.3,
            "{theme}: Paper --warning-border {border} is {ratio:.2}:1 on {PAPER}, needs >= 4.3:1"
        );
    }
}
