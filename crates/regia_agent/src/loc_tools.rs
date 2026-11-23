// SPDX-License-Identifier: MPL-2.0
//! `loc.status` and `loc.check_fit`: agent access to the translation status and the headless
//! text-fit check (book `agents/localization.md`, ADR-3359).
//!
//! Both read the live [`Localization`]: `loc.status` reports per-locale coverage the way
//! [`Localization::coverage`] computes it, and `loc.check_fit` runs
//! [`regia_ui::loc_fit::check_documents`] over the `.ui.ron` documents of the app's working
//! directory. The full counts (missing keys, placeholder mismatches) need the gather scan over
//! project files, so `regia loc status` stays the CI entry point; these methods answer "how far
//! along is each language in the running app" and "does this screen fit".

use regia_ecs::prelude::*;
use regia_ui::Localization;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn not_loaded() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "loc_not_loaded",
        "this app has no localization loaded",
        "add .ftl files under assets/localization/<locale>/ and the UI plugin",
    )
}

fn localization(world: &World) -> Result<&Localization, RpcError> {
    world
        .get_resource::<Localization>()
        .filter(|l| !l.available.is_empty())
        .ok_or_else(not_loaded)
}

fn unknown_locale(tag: &str) -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "loc_unknown_locale",
        format!("no loaded locale or parent of `{tag}`"),
        "`available` in localization.status lists the loaded locales",
    )
}

/// The loaded locales as `(tag, merged ftl text)` pairs, the shape
/// [`regia_ui::loc_fit::check_documents`] takes.
fn locale_sources(l: &Localization) -> Vec<(String, String)> {
    l.available
        .iter()
        .filter_map(|tag| l.source_text(tag).map(|text| (tag.clone(), text.to_owned())))
        .collect()
}

/// `loc.status { locale? }` (read): per loaded locale (or one), how much of the source locale's
/// messages it translates: messages, translated, untranslated, placeholder count, stale count
/// (translations recorded against an older source text) and percent. The missing-key and
/// placeholder-mismatch counts need the project's gather scan; run `regia loc status` for those.
pub(crate) fn status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_types("loc.status", params)?;
    let l = localization(world)?;
    let one = crate::params::opt_str(params, "locale")?;
    let mut tags: Vec<&str> = l.available.iter().map(String::as_str).collect();
    if let Some(tag) = one {
        let Some(nearest) = l.nearest_loaded(tag) else {
            return Err(unknown_locale(tag));
        };
        tags.retain(|t| *t == nearest);
    }
    let locales: Vec<Value> = tags
        .iter()
        .filter_map(|tag| l.coverage(tag))
        .map(|c| {
            json!({
                "locale": c.locale,
                "messages": c.messages,
                "translated": c.translated,
                "untranslated": c.untranslated,
                "placeholders": c.placeholders,
                "stale": c.stale,
                "percent": c.percent,
            })
        })
        .collect();
    Ok(json!({
        "source": l.fallback(),
        "locales": locales,
    }))
}

/// `WxH` parsed to a [`regia_ui::loc_fit::FitSize`].
fn parse_size(text: &str) -> Result<regia_ui::loc_fit::FitSize, RpcError> {
    let Some((w, h)) = text.split_once(['x', 'X']) else {
        return Err(bad_size(text));
    };
    let Ok(w) = w.trim().parse::<f32>() else {
        return Err(bad_size(text));
    };
    let Ok(h) = h.trim().parse::<f32>() else {
        return Err(bad_size(text));
    };
    if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
        return Err(bad_size(text));
    }
    Ok(regia_ui::loc_fit::FitSize { width: w, height: h })
}

fn bad_size(text: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "fit_bad_size",
        format!("`{text}` is not a size; pass `WxH` such as \"1920x1080\""),
        "`size` takes a string like \"1920x1080\"",
    )
}

/// Deepest directory walk `collect_ui_documents` will follow (a bind-mount loop stops here).
const MAX_UI_WALK_DEPTH: u32 = 32;

/// Largest `.ui.ron` `loc.check_fit` will read.
const MAX_UI_DOCUMENT_BYTES: u64 = 32 * 1024 * 1024;

/// Every `.ui.ron` under `assets/` of the working directory, sorted.
fn ui_documents() -> Vec<(String, String)> {
    collect_ui_documents(std::path::Path::new("assets"))
}

/// Every `.ui.ron` under `dir`, sorted by path.
///
/// Symlinks are skipped (`symlink_metadata`), so a link planted under `assets/` cannot pull in a
/// file outside the tree or form a cycle. A non-regular file, or one over `MAX_UI_DOCUMENT_BYTES`,
/// is skipped the same way, and the walk stops `MAX_UI_WALK_DEPTH` directories down (a bind-mount
/// loop).
fn collect_ui_documents(dir: &std::path::Path) -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>, depth: u32) {
        if depth > MAX_UI_WALK_DEPTH {
            return;
        }
        let Ok(mut entries): Result<Vec<_>, _> =
            std::fs::read_dir(dir).map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        else {
            return;
        };
        entries.sort();
        for path in entries {
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                walk(&path, out, depth.saturating_add(1));
            } else if meta.is_file()
                && meta.len() <= MAX_UI_DOCUMENT_BYTES
                && path.to_string_lossy().ends_with(".ui.ron")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.push((path.display().to_string(), text));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out, 0);
    out
}

/// `loc.check_fit { size?: string, locale?: string, document?: string }` (read): lays out every
/// text node of the app's `.ui.ron` documents (or one) with the loaded locales through the
/// headless runtime layout and reports each overflow or clip. Default sizes: 1920x1080 and
/// 1280x720.
pub(crate) fn check_fit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_types("loc.check_fit", params)?;
    let l = localization(world)?;
    let sizes = match crate::params::opt_str(params, "size")? {
        Some(text) => vec![parse_size(text)?],
        None => vec![
            // invariant: literals of the documented default sizes.
            parse_size("1920x1080").expect("literal"),
            // invariant: literals of the documented default sizes.
            parse_size("1280x720").expect("literal"),
        ],
    };
    let mut sources = ui_documents();
    if let Some(doc) = crate::params::opt_str(params, "document")? {
        // Same roots as other request paths (ADR-1816): a `..` climb or a symlink out of the
        // project, temp or output roots is `path_outside_project` and is not read.
        let path = crate::req_path::allowed_path(world, "document", doc)?;
        let bytes = crate::req_path::read_file_capped(&path, MAX_UI_DOCUMENT_BYTES).map_err(|e| {
            RpcError::new(
                NOT_FOUND,
                "fit_unknown_document",
                format!("{doc}: {e}"),
                "`document` is a .ui.ron path under the project, the temp directory or an output root",
            )
        })?;
        let text = String::from_utf8(bytes).map_err(|_| {
            RpcError::new(
                INVALID_PARAMS,
                "fit_bad_document",
                format!("{doc} is not UTF-8"),
                "fix the document or pass another `document`",
            )
        })?;
        sources.retain(|(name, _)| name == doc);
        if sources.is_empty() {
            sources.push((doc.to_owned(), text));
        }
    }
    if sources.is_empty() {
        return Err(RpcError::new(
            UNSUPPORTED,
            "fit_no_documents",
            "no .ui.ron documents under assets/ of the working directory",
            "pass `document` with a path, or run the app from the project root",
        ));
    }
    let locales = locale_sources(l);
    let reports = match regia_ui::loc_fit::check_documents(&sources, &locales, &sizes, 1.0) {
        Ok(r) => r,
        Err(regia_ui::UiError::Ron { path, .. })
        | Err(regia_ui::UiError::UnsupportedVersion { path, .. }) => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "fit_bad_document",
                format!("{path} does not parse as a .ui.ron document"),
                "fix the document or pass another `document`",
            ));
        }
        Err(e) => {
            return Err(RpcError::new(
                UNSUPPORTED,
                "fit_failed",
                format!("the fit check failed: {e}"),
                "report this as an engine bug",
            ));
        }
    };
    let findings: Vec<&regia_ui::loc_fit::FitFinding> =
        reports.iter().flat_map(|r| r.findings.iter()).collect();
    let mut all = findings;
    all.sort_by(|a, b| {
        (a.document.as_str(), a.node.as_str(), a.locale.as_str()).cmp(&(
            b.document.as_str(),
            b.node.as_str(),
            b.locale.as_str(),
        ))
    });
    Ok(json!({
        "ok": all.is_empty(),
        "findings": all.iter().map(|f| json!({
            "document": f.document,
            "node": f.node,
            "key": f.key,
            "locale": f.locale,
            "size": [f.size.width, f.size.height],
            "kind": match f.kind {
                regia_ui::loc_fit::FitKind::WidthOverflow => "width_overflow",
                regia_ui::loc_fit::FitKind::HeightOverflow => "height_overflow",
            },
            "measured": f.measured,
            "box": f.box_size,
            "excess": f.excess,
        })).collect::<Vec<_>>(),
        "nodes_checked": reports.iter().map(|r| r.nodes_checked).sum::<usize>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RpcError;

    /// The stable string code from the error's data (`{"code": "...", "hint": ...}`).
    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{err:?}"))
    }

    fn world_with_locales() -> World {
        let mut world = World::new();
        let loc = Localization::new(
            vec![
                ("en".to_owned(), "label = Settings\n".to_owned()),
                (
                    "de".to_owned(),
                    "label = EinstellungenundnochvielmehrTextdernichtinpasst\n".to_owned(),
                ),
            ],
            Some("en".to_owned()),
        )
        .expect("loc");
        world.insert_resource(loc);
        world
    }

    /// One small overflowing `.ui.ron` in a temp project; the absolute path is the `document`.
    fn temp_document() -> String {
        let root = regia_test_utils::unique_temp_dir("agent-loc-fit");
        let ui = root.join("assets/ui/menu.ui.ron");
        std::fs::create_dir_all(ui.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &ui,
            "(version: 6, root: (children: [(name: \"label\", style: (size: (200, 40)), text: (spans: [Loc(\"label\")], wrap: none))]))",
        )
        .expect("write");
        ui.display().to_string()
    }

    #[test]
    fn status_reports_every_loaded_locale_or_one() {
        let mut world = world_with_locales();
        let out = status(&mut world, &json!({})).expect("status");
        assert_eq!(out["source"], json!("en"));
        let locales = out["locales"].as_array().expect("locales");
        assert_eq!(locales.len(), 2);
        let en = locales.iter().find(|l| l["locale"] == "en").expect("en");
        assert_eq!(en["translated"], json!(1));
        assert_eq!(en["percent"], json!(100.0));
        let de = locales.iter().find(|l| l["locale"] == "de").expect("de");
        assert_eq!(de["messages"], json!(1));
        assert_eq!(de["percent"], json!(100.0));
        // One locale, by its tag or a parent of it.
        let out = status(&mut world, &json!({ "locale": "de-AT" })).expect("nearest");
        assert_eq!(out["locales"].as_array().expect("locales").len(), 1);
        let err: RpcError = status(&mut world, &json!({ "locale": "tlh" })).expect_err("unknown");
        assert_eq!(code(&err), "loc_unknown_locale");
        // No localization at all.
        let mut empty = World::new();
        let err = status(&mut empty, &json!({})).expect_err("none");
        assert_eq!(code(&err), "loc_not_loaded");
    }

    #[test]
    fn check_fit_reports_the_overflowing_text() {
        let doc = temp_document();
        let mut world = world_with_locales();
        let out = check_fit(&mut world, &json!({ "size": "800x600", "document": doc })).expect("check_fit");
        assert_eq!(out["ok"], json!(false));
        let findings = out["findings"].as_array().expect("findings");
        assert_eq!(findings.len(), 1, "{out}");
        assert_eq!(findings[0]["locale"], json!("de"));
        assert_eq!(findings[0]["node"], json!("root/label"));
        assert_eq!(findings[0]["kind"], json!("width_overflow"));
        assert_eq!(findings[0]["key"], json!("label"));
        assert!(findings[0]["excess"].as_f64().expect("excess") > 0.0);
    }

    #[test]
    fn check_fit_validates_its_params() {
        let doc = temp_document();
        let mut world = world_with_locales();
        let err: RpcError = check_fit(&mut world, &json!({ "size": "big" })).expect_err("size");
        assert_eq!(code(&err), "fit_bad_size");
        let err: RpcError = check_fit(&mut world, &json!({ "size": 42 })).expect_err("type");
        // The schema declares `size` a string; a number is refused with JSON-RPC invalid_params.
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let err: RpcError = check_fit(&mut world, &json!({ "document": "no/such.ui.ron" })).expect_err("doc");
        assert_eq!(code(&err), "fit_unknown_document");
        let _ = doc;
    }

    #[test]
    #[cfg(unix)]
    fn check_fit_refuses_a_non_regular_document() {
        let mut world = world_with_locales();
        // A device outside the allowed roots is refused before it is opened.
        check_fit(&mut world, &json!({ "document": "/dev/zero", "size": "800x600" })).expect_err("device");
        // A non-regular `document` inside an allowed root is not read either.
        let root = regia_test_utils::unique_temp_dir("agent-loc-nonreg");
        let dir = root.join("menu.ui.ron");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let err = check_fit(
            &mut world,
            &json!({ "document": dir.display().to_string(), "size": "800x600" }),
        )
        .expect_err("directory");
        assert_eq!(code(&err), "fit_unknown_document");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn ui_documents_under_stops_on_a_symlink_cycle() {
        let root = regia_test_utils::unique_temp_dir("agent-loc-cycle");
        std::os::unix::fs::symlink(&root, root.join("loop")).expect("symlink");
        let docs = collect_ui_documents(&root);
        assert!(docs.len() <= 32, "{}", docs.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `document` that climbs with `..`, or that resolves through a symlink to a file outside
    /// the allowed roots, is `path_outside_project` and is not opened.
    #[test]
    fn check_fit_refuses_a_document_outside_the_allowed_roots() {
        let mut world = world_with_locales();
        let root = regia_test_utils::unique_temp_dir("agent-loc-fit-esc");
        let secret = root.join("secret.txt");
        std::fs::write(&secret, b"not-a-ui-document").expect("write");
        let climbed = format!("{}/nosub/../secret.txt", root.display());
        let err =
            check_fit(&mut world, &json!({ "document": climbed, "size": "800x600" })).expect_err("parent");
        let data = err.data.as_ref().expect("data");
        assert_eq!(data["reason"], json!("path_outside_project"), "{err:?}");
        assert_eq!(data["path_reason"], json!("path_parent"), "{err:?}");
        assert_eq!(std::fs::read(&secret).expect("untouched"), b"not-a-ui-document");
        #[cfg(unix)]
        {
            let err = check_fit(
                &mut world,
                &json!({ "document": "/etc/passwd", "size": "800x600" }),
            )
            .expect_err("etc");
            let data = err.data.as_ref().expect("data");
            assert_eq!(data["reason"], json!("path_outside_project"), "{err:?}");
            assert_eq!(data["path_reason"], json!("path_escapes"), "{err:?}");
            let link = root.join("escape.ui.ron");
            std::os::unix::fs::symlink("/etc/passwd", &link).expect("link");
            let err = check_fit(
                &mut world,
                &json!({ "document": link.display().to_string(), "size": "800x600" }),
            )
            .expect_err("link");
            let data = err.data.as_ref().expect("data");
            assert_eq!(data["reason"], json!("path_outside_project"), "{err:?}");
            assert_eq!(data["path_reason"], json!("path_escapes"), "{err:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A symlink named `*.ui.ron` under the document tree is not followed.
    #[cfg(unix)]
    #[test]
    fn ui_document_walk_skips_symlinks() {
        let root = regia_test_utils::unique_temp_dir("agent-loc-fit-walk");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("mkdir");
        std::fs::write(assets.join("menu.ui.ron"), "real").expect("write");
        let outside = root.join("secret.ui.ron");
        std::fs::write(&outside, "SECRET").expect("secret");
        std::os::unix::fs::symlink(&outside, assets.join("linked.ui.ron")).expect("link");
        let docs = collect_ui_documents(&assets);
        assert!(
            docs.iter()
                .any(|(name, text)| name.ends_with("menu.ui.ron") && text == "real"),
            "{docs:?}"
        );
        assert!(
            docs.iter()
                .all(|(name, text)| !name.contains("linked") && text != "SECRET"),
            "{docs:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
