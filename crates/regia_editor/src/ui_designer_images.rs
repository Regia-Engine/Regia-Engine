// SPDX-License-Identifier: MPL-2.0
//! The UI designer's pictures (ADR-3148, `docs/specs/81-ui-designer.md` § 4.6): what the inspector's
//! picture picker offers and how both previews show a picture a `.ui.ron` names by asset path or
//! icon name.
//!
//! - The picker lists the project's source pictures ([`project_pictures`]: `png`, `jpg`, `jpeg`,
//!   `tga` and `bmp` under `<project>/assets`, as manifest paths such as `assets/ui/panel.png`) and
//!   the icon names of the editor's `IconAtlas`.
//! - The previews draw a scratch copy of the document. [`substitute_pictures`] rewrites that copy
//!   so it needs no asset server: an `image` or `mask` naming an asset path is given a designer image
//!   id ([`designer_image_id`]) whose pixels are decoded from the project's source file (cached in
//!   [`PictureCache`]); a path with no readable file draws the runtime fallback; an icon draws the
//!   runtime placeholder (the designer has no icon pages) and an icon mask is left off. The saved
//!   document is never changed.
//! - [`image_report`] is the document's picture list for `editor.ui_designer_images`: every
//!   reference with whether the project has the file or the icon.

use std::collections::HashMap;
use std::path::{Component, Path};
use std::sync::Arc;

use regia_ui::authoring::{ImageDef, MaskDef, UiNodeDef};
use regia_ui::{MaskFit, UI_IMAGE_FALLBACK, UI_IMAGE_PLACEHOLDER, UiFile, UiImageRef};
use serde_json::{Value, json};

use crate::ui_designer::DesignerError;

/// File extensions the picker lists (what the image importer and the source decoder read).
pub const PICTURE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "tga", "bmp"];

/// Most pictures [`project_pictures`] lists (a guard against a huge asset tree).
pub const MAX_PICTURES: usize = 4096;

/// Deepest folder [`project_pictures`] walks into below `assets`.
pub const MAX_PICTURE_DEPTH: usize = 12;

/// Largest side of a source picture the previews decode (larger files draw the fallback).
pub const MAX_PICTURE_SIDE: u32 = 8192;

/// First renderer image id of the designer's preview pictures (in the scratch preview app only).
pub const DESIGNER_IMAGE_BASE: u64 = 0x1C2_0000_0000;

/// A decoded source picture: width, height and straight-alpha sRGB RGBA8 rows, or why it did not
/// decode.
pub type SourcePicture = Result<(u32, u32, Vec<u8>), String>;

/// `path` (as a `.ui.ron` names it) in manifest form: `/` separators, no `./`, and the `assets/`
/// prefix (`assets://ui/a.png`, `ui/a.png` and `assets/ui/a.png` all give `assets/ui/a.png`).
pub fn manifest_path(path: &str) -> String {
    let p = path.trim().replace('\\', "/");
    let p = p.strip_prefix("assets://").unwrap_or(&p);
    let p = p.strip_prefix("./").unwrap_or(p);
    if p.starts_with("assets/") {
        p.to_owned()
    } else {
        format!("assets/{p}")
    }
}

/// The manifest path the runtime looks `path` up under (`regia_ui`'s image lookup): `\` becomes
/// `/`, `assets://` becomes `assets/`, and a path without the `assets/` prefix gets it. Unlike
/// [`manifest_path`] it neither trims whitespace nor drops a `./`, because the runtime does not.
fn runtime_manifest_path(path: &str) -> String {
    let mut norm = path.replace('\\', "/");
    if let Some(rest) = norm.strip_prefix("assets://") {
        norm = format!("assets/{rest}");
    }
    if norm.starts_with("assets/") {
        norm
    } else {
        format!("assets/{norm}")
    }
}

/// Whether the runtime finds the picture `path` names under the same manifest path the previews
/// read. `./assets/ui/a.png` or a name with a trailing space reads in the designer yet is
/// `ui_image_not_found` in the game, so the previews and the report treat it as missing.
pub(crate) fn runtime_resolvable(path: &str) -> bool {
    runtime_manifest_path(path) == manifest_path(path)
}

/// The renderer image id the previews draw the picture at `path` under: [`DESIGNER_IMAGE_BASE`] plus
/// a 32-bit FNV-1a hash of its manifest path (stable across runs).
pub fn designer_image_id(path: &str) -> u64 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in manifest_path(path).bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    DESIGNER_IMAGE_BASE | u64::from(hash)
}

/// The project's source pictures under `<root>/assets` as sorted manifest paths, at most
/// [`MAX_PICTURES`]. Hidden folders and files are skipped.
pub fn project_pictures(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let assets = root.join("assets");
    walk_pictures(&assets, "assets", 0, &mut out);
    out.sort();
    out.truncate(MAX_PICTURES);
    out
}

fn walk_pictures(dir: &Path, rel: &str, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_PICTURE_DEPTH || out.len() >= MAX_PICTURES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let child = format!("{rel}/{name}");
        if kind.is_dir() {
            walk_pictures(&entry.path(), &child, depth + 1, out);
        } else if kind.is_file() && is_picture(&name) {
            out.push(child);
            if out.len() >= MAX_PICTURES {
                return;
            }
        }
    }
}

/// Whether `name` has one of the [`PICTURE_EXTENSIONS`].
pub fn is_picture(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| PICTURE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// The file of manifest path `path` inside `root`, or `None` for a path that leaves the project
/// (`..`, an absolute path).
pub fn picture_file(root: &Path, path: &str) -> Option<std::path::PathBuf> {
    let rel = manifest_path(path);
    let rel = Path::new(&rel);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

/// TGA and BMP have no magic the sniffer accepts. The cook path decodes those by extension.
fn decode_picture(path: &str, bytes: &[u8]) -> Result<image::DynamicImage, image::ImageError> {
    let ext = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = ext.rsplit_once('.').map(|(_, extension)| extension).unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "tga" => image::load_from_memory_with_format(bytes, image::ImageFormat::Tga),
        "bmp" => image::load_from_memory_with_format(bytes, image::ImageFormat::Bmp),
        _ => image::load_from_memory(bytes),
    }
}

/// Decodes the source picture of `path` in `root` to RGBA8.
///
/// # Errors
///
/// The path leaves the project, the file is missing, it does not decode, or a side is larger than
/// [`MAX_PICTURE_SIDE`].
pub fn load_source_picture(root: &Path, path: &str) -> SourcePicture {
    let file = picture_file(root, path).ok_or_else(|| format!("`{path}` is outside the project"))?;
    let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let decoded = decode_picture(path, &bytes).map_err(|e| format!("{}: {e}", file.display()))?;
    let (w, h) = (decoded.width(), decoded.height());
    if w == 0 || h == 0 || w > MAX_PICTURE_SIDE || h > MAX_PICTURE_SIDE {
        return Err(format!(
            "{}: {w}x{h} is not a picture size the preview draws",
            file.display()
        ));
    }
    Ok((w, h, decoded.into_rgba8().into_raw()))
}

/// Source pictures decoded for the previews, by manifest path. A picture is decoded once and kept
/// until [`PictureCache::clear`] (the panel's Refresh), so editing the document does not re-read
/// files.
#[derive(Debug, Default, Clone)]
pub struct PictureCache {
    entries: HashMap<String, Arc<SourcePicture>>,
}

impl PictureCache {
    /// The decoded picture of `path` in `root`, decoded on first use.
    pub fn get(&mut self, root: &Path, path: &str) -> Arc<SourcePicture> {
        let key = manifest_path(path);
        self.entries
            .entry(key.clone())
            .or_insert_with(|| Arc::new(load_source_picture(root, &key)))
            .clone()
    }

    /// The picture of `path` if it was decoded before (no file access).
    pub fn cached(&self, path: &str) -> Option<Arc<SourcePicture>> {
        self.entries.get(&manifest_path(path)).cloned()
    }

    /// Forgets every decoded picture (files are read again on next use).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Pictures decoded so far.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing was decoded yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A picture a preview registers with its scratch renderer: image id, width, height, RGBA8.
pub type PreviewUpload = (u64, u32, u32, Vec<u8>);

/// Rewrites a preview's scratch copy of a document so it needs no asset server (module docs).
/// With `pictures` (`root` and the cache) each asset path is decoded and returned as an upload, and
/// a path that does not decode draws the runtime fallback; without, the ids are assigned and
/// nothing is decoded (the schematic preview needs only the layout). Uploads are deduplicated.
pub fn substitute_pictures(
    file: &mut UiFile,
    pictures: Option<(&Path, &mut PictureCache)>,
) -> Vec<PreviewUpload> {
    substitute(file, pictures, false)
}

/// [`substitute_pictures`] for a preview that draws pictures but has no project to read them from
/// (the themed preview with no project open): every asset path draws the runtime fallback, as
/// `ThemeChoice::project` promises, instead of an id with no picture behind it (a blank box).
pub(crate) fn substitute_pictures_without_project(file: &mut UiFile) {
    substitute(file, None, true);
}

fn substitute(
    file: &mut UiFile,
    mut pictures: Option<(&Path, &mut PictureCache)>,
    fallback_without_project: bool,
) -> Vec<PreviewUpload> {
    let mut uploads: Vec<PreviewUpload> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut resolve = |path: &str| -> u64 {
        let id = designer_image_id(path);
        let Some((root, cache)) = pictures.as_mut() else {
            return if fallback_without_project {
                UI_IMAGE_FALLBACK
            } else {
                id
            };
        };
        if !runtime_resolvable(path) {
            return UI_IMAGE_FALLBACK;
        }
        match cache.get(root, path).as_ref() {
            Ok((w, h, rgba)) => {
                if seen.insert(id) {
                    uploads.push((id, *w, *h, rgba.clone()));
                }
                id
            }
            Err(_) => UI_IMAGE_FALLBACK,
        }
    };
    substitute_node(&mut file.root, &mut resolve);
    uploads
}

fn substitute_node(node: &mut UiNodeDef, resolve: &mut impl FnMut(&str) -> u64) {
    if let Some(image) = &mut node.image {
        if let Some(path) = image.asset.take() {
            image.icon = None;
            image.id = resolve(&path);
        } else if image.icon.take().is_some() {
            image.id = UI_IMAGE_PLACEHOLDER;
        }
    }
    if let Some(mask) = &mut node.mask {
        if let Some(path) = mask.asset.take() {
            mask.image = Some(resolve(&path));
        } else if mask.icon.is_some() {
            node.mask = None;
        }
    }
    for child in &mut node.children {
        substitute_node(child, resolve);
    }
}

/// Whether the project has the picture of one reference: `Some(true/false)` for an asset path
/// (`root` given) or an icon (`icons` given), `None` for a renderer id or when it cannot be told.
pub fn reference_found(r: &UiImageRef, root: Option<&Path>, icons: Option<&[String]>) -> Option<bool> {
    match r.kind {
        "asset" => root.map(|root| {
            runtime_resolvable(&r.value) && picture_file(root, &r.value).is_some_and(|f| f.is_file())
        }),
        "icon" => icons.map(|icons| icons.contains(&r.value)),
        _ => None,
    }
}

/// `editor.ui_designer_images`' reply for `file`: every picture reference (node path and name,
/// `image` or `mask`, `asset`, `icon` or `id`, the value, and `found`), the distinct asset paths, how
/// many are missing, and the picker lists (project pictures, icon names).
pub fn image_report(file: &UiFile, root: Option<&Path>, icons: Option<&[String]>) -> Value {
    let refs = regia_ui::ui_image_references(file);
    let mut missing = 0usize;
    let references: Vec<Value> = refs
        .iter()
        .map(|r| {
            let found = reference_found(r, root, icons);
            if found == Some(false) {
                missing += 1;
            }
            let mut v = regia_ui::ui_image_assets::image_ref_json(r);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("found".to_owned(), json!(found));
            }
            v
        })
        .collect();
    json!({
        "references": references,
        "assets": regia_ui::ui_asset_dependencies(file),
        "missing": missing,
        "pictures": root.map(project_pictures).unwrap_or_default(),
        "icons": icons.unwrap_or_default(),
    })
}

/// The picture an `image` or `mask` edit names (`editor.ui_designer_edit` ops `image` and `mask`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PictureChoice {
    /// A manifest path (`asset`).
    Asset(String),
    /// An icon name (`icon`).
    Icon(String),
    /// A renderer image id (`id`).
    Id(u64),
}

/// The picture `params` names: at most one of `asset` (a non-empty string), `icon` (a non-empty
/// string) and `id` (a whole number); `None` when none is given.
///
/// # Errors
///
/// `bad_value` for two of them, an empty name or an id that is not a whole number.
pub fn picture_choice(params: &Value) -> Result<Option<PictureChoice>, DesignerError> {
    let bad = |m: &str| DesignerError::new("bad_value", m.to_owned());
    let given: Vec<&str> = ["asset", "icon", "id"]
        .into_iter()
        .filter(|k| params.get(*k).is_some_and(|v| !v.is_null()))
        .collect();
    let name = |key: &str| -> Result<String, DesignerError> {
        params
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| bad(&format!("`{key}` is a non-empty string")))
    };
    match given.as_slice() {
        [] => Ok(None),
        ["asset"] => Ok(Some(PictureChoice::Asset(name("asset")?))),
        ["icon"] => Ok(Some(PictureChoice::Icon(name("icon")?))),
        ["id"] => params
            .get("id")
            .and_then(Value::as_u64)
            .map(|id| Some(PictureChoice::Id(id)))
            .ok_or_else(|| bad("`id` is a renderer image id (a whole number)")),
        _ => Err(bad("name the picture by one of `asset`, `icon` and `id`")),
    }
}

/// A node's `image` after an `image` edit: `remove: true` removes it; `asset`, `icon` or `id`
/// replaces the picture (tint and aspect stay); `nine_slice` `[left, top, right, bottom]` (source
/// px) sets the insets and `null` removes them; `preserve_aspect` sets that flag.
///
/// # Errors
///
/// `bad_value`: see [`picture_choice`]; no picture for a node without an image; insets that are not
/// four finite numbers of at least 0; a `preserve_aspect` that is not a boolean.
pub fn edit_image(current: Option<&ImageDef>, params: &Value) -> Result<Option<ImageDef>, DesignerError> {
    if params.get("remove").and_then(Value::as_bool) == Some(true) {
        return Ok(None);
    }
    let choice = picture_choice(params)?;
    let mut image = match (current, &choice) {
        (Some(image), _) => image.clone(),
        (None, Some(_)) => ImageDef::default(),
        (None, None) => {
            return Err(DesignerError::new(
                "bad_value",
                "the node has no image: pass `asset`, `icon` or `id`",
            ));
        }
    };
    match choice {
        Some(PictureChoice::Asset(path)) => {
            image.id = 0;
            image.icon = None;
            image.asset = Some(path);
        }
        Some(PictureChoice::Icon(name)) => {
            image.id = 0;
            image.asset = None;
            image.icon = Some(name);
        }
        Some(PictureChoice::Id(id)) => {
            image.asset = None;
            image.icon = None;
            image.id = id;
        }
        None => {}
    }
    match params.get("nine_slice") {
        None => {}
        Some(Value::Null) => image.nine_slice = None,
        Some(v) => image.nine_slice = Some(insets(v)?),
    }
    match params.get("preserve_aspect") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(b)) => image.preserve_aspect = *b,
        Some(_) => return Err(DesignerError::new("bad_value", "`preserve_aspect` is a boolean")),
    }
    // `.ui.ron` v11 `fit` (ADR-9980; `tile` is ADR-7751): null leaves it unwritten.
    match params.get("fit") {
        None => {}
        Some(Value::Null) => image.fit = None,
        Some(v) => {
            let fit = v.as_str().and_then(regia_ui::ImageFit::parse).ok_or_else(|| {
                DesignerError::new(
                    "bad_value",
                    "`fit` is stretch, contain, cover or tile (null: keep aspect)",
                )
            })?;
            image.fit = Some(fit);
        }
    }
    Ok(Some(image))
}

fn insets(v: &Value) -> Result<(f32, f32, f32, f32), DesignerError> {
    let bad = || {
        DesignerError::new(
            "bad_value",
            "`nine_slice` is [left, top, right, bottom]: four finite numbers of at least 0 (source px)",
        )
    };
    let values: Vec<f32> = v
        .as_array()
        .filter(|a| a.len() == 4)
        .ok_or_else(bad)?
        .iter()
        .map(|n| n.as_f64().map(|f| f as f32))
        .collect::<Option<_>>()
        .ok_or_else(bad)?;
    if values.iter().any(|f| !f.is_finite() || *f < 0.0) {
        return Err(bad());
    }
    Ok((values[0], values[1], values[2], values[3]))
}

/// A node's `mask` after a `mask` edit: `remove: true` removes it; `asset`, `icon` or `id` replaces
/// the picture; `fit` (`stretch`, `contain`, `cover` or `tile`) sets the placement.
///
/// # Errors
///
/// `bad_value`: see [`picture_choice`]; no picture for a node without a mask; an unknown `fit`.
pub fn edit_mask(current: Option<&MaskDef>, params: &Value) -> Result<Option<MaskDef>, DesignerError> {
    if params.get("remove").and_then(Value::as_bool) == Some(true) {
        return Ok(None);
    }
    let choice = picture_choice(params)?;
    let mut mask = match (current, &choice) {
        (Some(mask), _) => mask.clone(),
        (None, Some(_)) => MaskDef {
            image: None,
            asset: None,
            icon: None,
            fit: MaskFit::default(),
        },
        (None, None) => {
            return Err(DesignerError::new(
                "bad_value",
                "the node has no mask: pass `asset`, `icon` or `id`",
            ));
        }
    };
    if let Some(choice) = choice {
        mask.image = None;
        mask.asset = None;
        mask.icon = None;
        match choice {
            PictureChoice::Asset(path) => mask.asset = Some(path),
            PictureChoice::Icon(name) => mask.icon = Some(name),
            PictureChoice::Id(id) => mask.image = Some(id),
        }
    }
    if let Some(fit) = params.get("fit").filter(|v| !v.is_null()) {
        mask.fit = serde_json::from_value::<MaskFit>(fit.clone())
            .map_err(|_| DesignerError::new("bad_value", "`fit` is stretch, contain, cover or tile"))?;
    }
    Ok(Some(mask))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::{UiMigrators, parse_ui_ron};

    const DOC: &str = r#"(version: 9, root: (children: [
        (name: "panel", image: (asset: "assets/ui/panel.png", nine_slice: (4, 4, 4, 4))),
        (name: "gear", image: (icon: "gear")),
        (name: "raw", image: (id: 7)),
        (name: "gone", image: (asset: "ui/gone.png")),
        (name: "star", mask: (asset: "assets/ui/panel.png"), children: [(name: "inner", mask: (icon: "ring"))]),
    ]))"#;

    fn project() -> std::path::PathBuf {
        let root = regia_test_utils::unique_temp_dir("designer-images");
        let ui = root.join("assets").join("ui");
        std::fs::create_dir_all(ui.join(".hidden")).expect("dirs");
        image::RgbaImage::from_pixel(8, 4, image::Rgba([10, 200, 30, 255]))
            .save(ui.join("panel.png"))
            .expect("png");
        std::fs::write(ui.join("notes.txt"), "x").expect("txt");
        std::fs::write(ui.join("broken.png"), "not a png").expect("broken");
        std::fs::write(ui.join(".hidden").join("h.png"), "x").expect("hidden");
        root
    }

    #[test]
    fn manifest_paths_and_ids_are_stable() {
        assert_eq!(manifest_path("ui/a.png"), "assets/ui/a.png");
        assert_eq!(manifest_path("assets://ui/a.png"), "assets/ui/a.png");
        assert_eq!(manifest_path("./assets/ui/a.png"), "assets/ui/a.png");
        assert_eq!(
            designer_image_id("ui/a.png"),
            designer_image_id("assets/ui/a.png")
        );
        assert_ne!(designer_image_id("ui/a.png"), designer_image_id("ui/b.png"));
        assert_eq!(designer_image_id("x") & !0xFFFF_FFFF, DESIGNER_IMAGE_BASE);
        assert!(picture_file(Path::new("/p"), "../etc/passwd").is_none());
        assert!(picture_file(Path::new("/p"), "assets/../../x.png").is_none());
    }

    /// TGA has no magic the sniffer recognizes. The picker lists `.tga`, so the preview must
    /// decode it by extension the way the cook path does.
    #[test]
    fn a_tga_source_picture_decodes() {
        let root = regia_test_utils::unique_temp_dir("designer_tga");
        let ui = root.join("assets/ui");
        std::fs::create_dir_all(&ui).expect("dirs");
        // 1×1 uncompressed truecolor TGA, BGRA pixel (10, 20, 30, 255).
        let mut tga = vec![0u8; 18];
        tga[2] = 2;
        tga[12] = 1;
        tga[14] = 1;
        tga[16] = 32;
        tga[17] = 8;
        tga.extend_from_slice(&[10, 20, 30, 255]);
        std::fs::write(ui.join("dot.tga"), &tga).expect("tga");
        let (w, h, rgba) =
            load_source_picture(&root, "assets/ui/dot.tga").expect("a tga source picture decodes");
        assert_eq!((w, h, rgba.len()), (1, 1, 4));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_picker_lists_project_pictures_sorted() {
        let root = project();
        assert_eq!(
            project_pictures(&root),
            vec![
                "assets/ui/broken.png".to_owned(),
                "assets/ui/panel.png".to_owned()
            ]
        );
        assert!(project_pictures(&root.join("nowhere")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The previews' copy names only renderer ids: the decoded picture once (two references share
    /// it), the fallback for a file that is missing, the placeholder for an icon, an icon mask
    /// dropped; without a project nothing is decoded and the same ids are assigned.
    #[test]
    fn substitution_decodes_each_picture_once_and_falls_back() {
        let root = project();
        let file = parse_ui_ron(DOC, "t", &UiMigrators::default()).expect("parse");
        let mut copy = file.clone();
        let mut cache = PictureCache::default();
        let uploads = substitute_pictures(&mut copy, Some((&root, &mut cache)));
        let panel = designer_image_id("assets/ui/panel.png");
        assert_eq!(uploads.len(), 1);
        assert_eq!((uploads[0].0, uploads[0].1, uploads[0].2), (panel, 8, 4));
        assert_eq!(uploads[0].3.len(), 8 * 4 * 4);
        let kids = &copy.root.children;
        let image = |i: usize| kids[i].image.as_ref().expect("image");
        assert_eq!(image(0).id, panel);
        assert_eq!(image(0).asset, None);
        assert_eq!(image(0).nine_slice, Some((4.0, 4.0, 4.0, 4.0)));
        assert_eq!(image(1).id, UI_IMAGE_PLACEHOLDER);
        assert_eq!(image(1).icon, None);
        assert_eq!(image(2).id, 7);
        assert_eq!(image(3).id, UI_IMAGE_FALLBACK);
        assert_eq!(kids[4].mask.as_ref().and_then(|m| m.image), Some(panel));
        assert_eq!(kids[4].children[0].mask, None);
        assert_eq!(cache.len(), 2, "panel and gone, each read once");

        let mut plain = file.clone();
        assert!(substitute_pictures(&mut plain, None).is_empty());
        assert_eq!(
            plain.root.children[3].image.as_ref().map(|i| i.id),
            Some(designer_image_id("ui/gone.png"))
        );
        // The source document is untouched.
        assert_eq!(
            file.root.children[0]
                .image
                .as_ref()
                .and_then(|i| i.asset.as_deref()),
            Some("assets/ui/panel.png")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The runtime looks a picture up by its exact spelling (no trim, no `./`): a spelling only the
    /// designer read used to show `found: true` and draw, then fail as `ui_image_not_found` in game.
    #[test]
    fn a_spelling_the_runtime_cannot_find_is_missing_in_the_designer_too() {
        let root = project();
        for (path, runtime) in [
            ("assets/ui/panel.png", true),
            ("ui/panel.png", true),
            ("assets://ui/panel.png", true),
            ("assets\\ui\\panel.png", true),
            ("./assets/ui/panel.png", false),
            ("assets/ui/panel.png ", false),
            (" ui/panel.png", false),
        ] {
            assert_eq!(runtime_resolvable(path), runtime, "{path:?}");
            let r = UiImageRef {
                node: "/0".into(),
                node_name: None,
                field: "image",
                kind: "asset",
                value: path.to_owned(),
            };
            assert_eq!(reference_found(&r, Some(&root), None), Some(runtime), "{path:?}");
            let mut file = parse_ui_ron(DOC, "t", &UiMigrators::default()).expect("parse");
            if let Some(image) = &mut file.root.children[0].image {
                image.asset = Some(path.to_owned());
            }
            let mut cache = PictureCache::default();
            substitute_pictures(&mut file, Some((&root, &mut cache)));
            let id = file.root.children[0].image.as_ref().map(|i| i.id);
            let expected = if runtime {
                designer_image_id(path)
            } else {
                UI_IMAGE_FALLBACK
            };
            assert_eq!(id, Some(expected), "{path:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `ThemeChoice::project` = `None` promises the runtime fallback; the plain substitution gave a
    /// designer id with no upload behind it, which the renderer draws as a white box.
    #[test]
    fn without_a_project_every_asset_picture_is_the_fallback() {
        let file = parse_ui_ron(DOC, "t", &UiMigrators::default()).expect("parse");
        let mut copy = file.clone();
        substitute_pictures_without_project(&mut copy);
        let kids = &copy.root.children;
        assert_eq!(kids[0].image.as_ref().map(|i| i.id), Some(UI_IMAGE_FALLBACK));
        assert_eq!(kids[3].image.as_ref().map(|i| i.id), Some(UI_IMAGE_FALLBACK));
        assert_eq!(
            kids[1].image.as_ref().map(|i| i.id),
            Some(UI_IMAGE_PLACEHOLDER),
            "icons unchanged"
        );
        assert_eq!(kids[2].image.as_ref().map(|i| i.id), Some(7), "raw ids unchanged");
        assert_eq!(
            kids[4].mask.as_ref().and_then(|m| m.image),
            Some(UI_IMAGE_FALLBACK)
        );
    }

    #[test]
    fn a_picture_that_does_not_decode_is_an_error_not_a_panic() {
        let root = project();
        assert!(load_source_picture(&root, "assets/ui/broken.png").is_err());
        assert!(load_source_picture(&root, "assets/ui/missing.png").is_err());
        assert!(load_source_picture(&root, "../outside.png").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_report_lists_references_and_what_is_missing() {
        let root = project();
        let file = parse_ui_ron(DOC, "t", &UiMigrators::default()).expect("parse");
        let icons = vec!["gear".to_owned()];
        let report = image_report(&file, Some(&root), Some(&icons));
        let refs = report["references"].as_array().expect("refs");
        let found: Vec<(String, Value)> = refs
            .iter()
            .map(|r| {
                (
                    r["value"].as_str().unwrap_or_default().to_owned(),
                    r["found"].clone(),
                )
            })
            .collect();
        assert_eq!(
            found,
            vec![
                ("assets/ui/panel.png".to_owned(), json!(true)),
                ("gear".to_owned(), json!(true)),
                ("7".to_owned(), Value::Null),
                ("ui/gone.png".to_owned(), json!(false)),
                ("assets/ui/panel.png".to_owned(), json!(true)),
                ("ring".to_owned(), json!(false)),
            ]
        );
        assert_eq!(report["missing"], json!(2));
        assert_eq!(report["assets"], json!(["assets/ui/panel.png", "ui/gone.png"]));
        assert_eq!(report["pictures"].as_array().map(Vec::len), Some(2));
        // Without a project or icons nothing is judged.
        let bare = image_report(&file, None, None);
        assert_eq!(bare["missing"], json!(0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn image_edits_switch_the_source_and_keep_the_rest() {
        let raw = ImageDef {
            tint: regia_ui::UiColor([0.5, 0.5, 0.5, 1.0]),
            ..ImageDef::raw(9)
        };
        let asset = edit_image(
            Some(&raw),
            &json!({ "asset": "assets/ui/a.png", "nine_slice": [1, 2, 3, 4] }),
        )
        .expect("asset")
        .expect("image");
        assert_eq!(
            (asset.id, asset.asset.as_deref(), asset.icon.as_deref()),
            (0, Some("assets/ui/a.png"), None)
        );
        assert_eq!(asset.tint, raw.tint, "the tint stays");
        assert_eq!(asset.nine_slice, Some((1.0, 2.0, 3.0, 4.0)));
        let icon = edit_image(Some(&asset), &json!({ "icon": "gear", "nine_slice": null }))
            .expect("icon")
            .expect("image");
        assert_eq!(
            (icon.asset.as_deref(), icon.icon.as_deref(), icon.nine_slice),
            (None, Some("gear"), None)
        );
        let back = edit_image(Some(&icon), &json!({ "id": 9 }))
            .expect("id")
            .expect("image");
        assert_eq!(back, raw);
        assert_eq!(
            edit_image(Some(&raw), &json!({ "remove": true })).expect("remove"),
            None
        );
        let fresh = edit_image(None, &json!({ "icon": "x" }))
            .expect("new")
            .expect("image");
        assert_eq!(fresh, ImageDef::icon("x"));
        for bad in [
            json!({}),
            json!({ "asset": "a.png", "icon": "x" }),
            json!({ "asset": " " }),
            json!({ "id": -1 }),
            json!({ "icon": "x", "nine_slice": [1, 2, 3] }),
            json!({ "icon": "x", "nine_slice": [1, 2, 3, -4] }),
            json!({ "icon": "x", "preserve_aspect": "yes" }),
        ] {
            let err = edit_image(None, &bad).expect_err("refused");
            assert_eq!(err.code, "bad_value", "{bad}");
        }
    }

    #[test]
    fn mask_edits_name_one_picture_and_a_fit() {
        let mask = edit_mask(None, &json!({ "icon": "star", "fit": "contain" }))
            .expect("mask")
            .expect("some");
        assert_eq!((mask.icon.as_deref(), mask.fit), (Some("star"), MaskFit::Contain));
        let asset = edit_mask(Some(&mask), &json!({ "asset": "ui/m.png" }))
            .expect("asset")
            .expect("some");
        assert_eq!(
            (asset.icon.as_deref(), asset.asset.as_deref(), asset.fit),
            (None, Some("ui/m.png"), MaskFit::Contain)
        );
        assert_eq!(
            edit_mask(Some(&asset), &json!({ "remove": true })).expect("remove"),
            None
        );
        assert_eq!(
            edit_mask(None, &json!({})).expect_err("nothing").code,
            "bad_value"
        );
        assert_eq!(
            edit_mask(Some(&asset), &json!({ "fit": "squash" }))
                .expect_err("fit")
                .code,
            "bad_value"
        );
    }
}
