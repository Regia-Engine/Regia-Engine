// SPDX-License-Identifier: MPL-2.0
//! What the editor has, read at run time: panels, menu items, palette commands, registered actions,
//! the allowlist's control sites and the agent methods. Nothing here names a panel or an action:
//! a panel or action another lane adds shows up in the next run without an edit (ADR-7865).
//!
//! The functions take the editor's own agent replies (`editor.layout_get`, `editor.menu_list`,
//! `editor.command`, `editor.actions`, `agent.hello`) as JSON, so the unit tests feed them fixed
//! replies and the live run feeds them the editor's.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// One panel of the dock (`editor.layout_get` → `layout.known_panels`, plus the panels of the
/// shown layout that are not built in, such as extension panels).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelItem {
    /// The stable panel id `editor.layout_apply { open }` takes.
    pub id: String,
}

/// One menu item (`editor.menu_list`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MenuItem {
    /// `Menu/Label`.
    pub path: String,
    /// The parity registry id.
    pub action: String,
    /// `method`, `command`, `os_dialog` or `confirm`.
    pub class: String,
    /// The agent method that does what the item does.
    pub method: String,
    /// The params of that call, as the menu model gives them.
    pub params: Value,
    /// Whether the item could run when the inventory was read.
    pub enabled: bool,
}

/// One palette command (`editor.command { limit: 200 }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandItem {
    /// Command id (`view.toggle_grid`).
    pub id: String,
    /// Palette category.
    pub category: String,
    /// Palette title.
    pub title: String,
}

/// One registered UI action (`editor.actions` → `actions`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionItem {
    /// Registry id.
    pub id: String,
    /// The control's label.
    pub label: String,
    /// The registry panel (`shell.file`, ...).
    pub panel: String,
    /// `method`, `command`, `view_only`, `os_dialog` or `confirm`.
    pub class: String,
    /// The agent method, when the class names one.
    pub method: Option<String>,
    /// The palette command, when the class is `command`.
    pub command: Option<String>,
}

/// One control site of the allowlist (`editor.actions` → `sites`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteItem {
    /// Source file of the control.
    pub file: String,
    /// Line of the control.
    pub line: u64,
    /// The control's label as the scan read it.
    pub label: String,
    /// The allowlist class (`gap` = no agent route yet).
    pub class: String,
    /// The agent method, when the class names one.
    pub method: Option<String>,
}

/// A problem the inventory shows before anything runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// `no_agent_route` (a GUI control without an agent path) or `route_missing` (the named agent
    /// method is not registered in the running editor).
    pub kind: String,
    /// What it is about (an action id, a menu path or `file:line`).
    pub subject: String,
    /// One line of detail.
    pub detail: String,
}

/// Everything the run will drive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inventory {
    /// Dock panels.
    pub panels: Vec<PanelItem>,
    /// Menu items.
    pub menus: Vec<MenuItem>,
    /// Palette commands.
    pub commands: Vec<CommandItem>,
    /// Registered UI actions.
    pub actions: Vec<ActionItem>,
    /// Allowlist control sites.
    pub sites: Vec<SiteItem>,
    /// Agent methods the editor answers (`agent.hello` → `methods`).
    pub methods: BTreeSet<String>,
    /// Problems found by reading alone.
    pub findings: Vec<Finding>,
}

/// Counts per kind, printed first and kept in `result.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryCounts {
    /// Dock panels.
    pub panels: usize,
    /// Menu items.
    pub menu_items: usize,
    /// Palette commands.
    pub commands: usize,
    /// Registered UI actions.
    pub actions: usize,
    /// Allowlist control sites.
    pub sites: usize,
    /// Sites per allowlist class.
    pub sites_by_class: BTreeMap<String, usize>,
    /// Agent methods the running editor answers.
    pub methods: usize,
    /// Editor methods of the method snapshot (`docs/api-snapshots/editor-agent-methods.txt`).
    pub snapshot_methods: usize,
    /// Findings of the inventory alone.
    pub findings: usize,
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn opt_text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Panels from an `editor.layout_get` reply: every `layout.known_panels` id, then the shown
/// panels it does not list (extension panels), in reply order, without repeats.
#[must_use]
pub fn panels_from_layout(reply: &Value) -> Vec<PanelItem> {
    let mut seen = BTreeSet::new();
    let known = reply
        .pointer("/layout/known_panels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let shown = reply
        .get("panels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    known
        .chain(shown)
        .filter_map(Value::as_str)
        .filter(|id| seen.insert((*id).to_owned()))
        .map(|id| PanelItem { id: id.to_owned() })
        .collect()
}

/// Menu items from an `editor.menu_list` reply, menu by menu.
#[must_use]
pub fn menus_from_list(reply: &Value) -> Vec<MenuItem> {
    let menus = reply.get("menus").and_then(Value::as_array).into_iter().flatten();
    menus
        .flat_map(|menu| menu.get("items").and_then(Value::as_array).into_iter().flatten())
        .map(|item| MenuItem {
            path: text(item, "path"),
            action: text(item, "action"),
            class: text(item, "class"),
            method: text(item, "method"),
            params: item.get("params").cloned().unwrap_or_else(|| json!({})),
            enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        })
        .collect()
}

/// Commands from an `editor.command` listing reply.
#[must_use]
pub fn commands_from_list(reply: &Value) -> Vec<CommandItem> {
    let mut seen = BTreeSet::new();
    reply
        .get("commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| CommandItem {
            id: text(c, "id"),
            category: text(c, "category"),
            title: text(c, "title"),
        })
        .filter(|c| !c.id.is_empty() && seen.insert(c.id.clone()))
        .collect()
}

/// Actions and control sites from an `editor.actions` reply.
#[must_use]
pub fn actions_from_reply(reply: &Value) -> (Vec<ActionItem>, Vec<SiteItem>) {
    let actions = reply
        .get("actions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|a| ActionItem {
            id: text(a, "id"),
            label: text(a, "label"),
            panel: text(a, "panel"),
            class: text(a, "class"),
            method: opt_text(a, "method"),
            command: opt_text(a, "command"),
        })
        .collect();
    let sites = reply
        .get("sites")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| SiteItem {
            file: text(s, "file"),
            line: s.get("line").and_then(Value::as_u64).unwrap_or(0),
            label: text(s, "label"),
            class: text(s, "class"),
            method: opt_text(s, "method"),
        })
        .collect();
    (actions, sites)
}

/// Method names from an `agent.hello` reply (`methods` is a list of names or of `{ name }`).
#[must_use]
pub fn methods_from_hello(reply: &Value) -> BTreeSet<String> {
    reply
        .get("methods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str().map(str::to_owned).or_else(|| opt_text(m, "name")))
        .collect()
}

/// The findings the inventory shows by reading alone: every allowlist site of class `gap` (a GUI
/// control with no agent route) and every registry action or menu item whose agent method the
/// running editor does not answer. `methods` empty means "not known" and skips the second check.
#[must_use]
pub fn findings(inv: &Inventory) -> Vec<Finding> {
    let mut out = Vec::new();
    for site in inv.sites.iter().filter(|s| s.class == "gap") {
        out.push(Finding {
            kind: "no_agent_route".into(),
            subject: format!("{}:{}", site.file, site.line),
            detail: format!("control \"{}\" has a GUI action but no agent route", site.label),
        });
    }
    if inv.methods.is_empty() {
        return out;
    }
    let mut missing = BTreeSet::new();
    for action in &inv.actions {
        if let Some(method) = &action.method
            && !inv.methods.contains(method)
            && missing.insert(method.clone())
        {
            out.push(Finding {
                kind: "route_missing".into(),
                subject: action.id.clone(),
                detail: format!("its agent method `{method}` is not registered in the running editor"),
            });
        }
    }
    for item in &inv.menus {
        if !item.method.is_empty()
            && !inv.methods.contains(&item.method)
            && missing.insert(item.method.clone())
        {
            out.push(Finding {
                kind: "route_missing".into(),
                subject: item.path.clone(),
                detail: format!(
                    "its agent method `{}` is not registered in the running editor",
                    item.method
                ),
            });
        }
    }
    out
}

/// The counts printed before the run.
#[must_use]
pub fn counts(inv: &Inventory, snapshot_methods: usize) -> InventoryCounts {
    let mut sites_by_class = BTreeMap::new();
    for site in &inv.sites {
        *sites_by_class.entry(site.class.clone()).or_insert(0) += 1;
    }
    InventoryCounts {
        panels: inv.panels.len(),
        menu_items: inv.menus.len(),
        commands: inv.commands.len(),
        actions: inv.actions.len(),
        sites: inv.sites.len(),
        sites_by_class,
        methods: inv.methods.len(),
        snapshot_methods,
        findings: inv.findings.len(),
    }
}

/// One line of the editor-method snapshot: the method name and its required params.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMethod {
    /// Method name.
    pub name: String,
    /// Params without `?`.
    pub required: Vec<String>,
}

/// Reads `docs/api-snapshots/editor-agent-methods.txt` lines (`name(param: kind, optional?: kind)`).
#[must_use]
pub fn parse_method_snapshot(text: &str) -> Vec<SnapshotMethod> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let open = line.find('(')?;
            let name = line[..open].trim().to_owned();
            let inner = line[open + 1..].trim_end_matches(')');
            let required = split_top_level(inner)
                .into_iter()
                .filter_map(|p| {
                    let (param, _) = p.split_once(':')?;
                    let param = param.trim();
                    (!param.ends_with('?') && !param.is_empty()).then(|| param.to_owned())
                })
                .collect();
            Some(SnapshotMethod { name, required })
        })
        .collect()
}

/// Splits on commas outside brackets (`kind` texts may hold `string|object`, never commas, but
/// stay safe).
fn split_top_level(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(text[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// The first example request of each method in the agent guide pages: every
/// `{"method": "...", "params": {...}}` that parses as JSON on one line of `book/agents/*.md`.
/// Only `editor.*` methods are kept: they are the editor's own surface.
#[must_use]
pub fn examples_from_markdown(pages: &[(String, String)]) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for (_, body) in pages {
        for line in body.lines() {
            let line = line.trim();
            if !line.starts_with('{') || !line.contains("\"method\"") {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(method) = value.get("method").and_then(Value::as_str) else {
                continue;
            };
            if !method.starts_with("editor.") {
                continue;
            }
            let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
            out.entry(method.to_owned()).or_insert(params);
        }
    }
    out
}

/// Reads every `book/agents/*.md` page (name, text); unreadable pages are skipped.
#[must_use]
pub fn read_agent_pages(workspace: &Path) -> Vec<(String, String)> {
    let dir = workspace.join("book/agents");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut pages: Vec<(String, String)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .filter_map(|p| {
            let body = std::fs::read_to_string(&p).ok()?;
            Some((p.file_name()?.to_string_lossy().into_owned(), body))
        })
        .collect();
    pages.sort();
    pages
}

/// Why a step is not run, or `None` when it is safe inside the throwaway project copy: quitting,
/// packaging and publishing, network sessions, device deploys, external programs (code editor,
/// browser, file manager), cache clearing outside the project and long bake or cook jobs. The
/// match is on the lowercased step text (id, label, menu path and method together).
#[must_use]
pub fn skip_reason(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    if has(&["file/exit", "file.exit", "quit", "agent.shutdown"]) {
        return Some("quits the editor (the run would end)");
    }
    if has(&["package", "publish", "upload", "steam", "store_"]) {
        return Some("packages or publishes a game (minutes of cargo; output outside the smoke budget)");
    }
    if has(&["deploy", "device", "adb", "launch_profile", "launch."]) {
        return Some("deploys to or launches on a device");
    }
    if has(&[
        "net.",
        "pie_net",
        "lobby",
        "connect",
        "remote",
        "multiuser",
        "presence",
        "collab",
        "vcs",
    ]) {
        return Some("opens a network session or talks to a remote");
    }
    if has(&[
        "open_code_editor",
        "code editor",
        "help_open",
        "documentation",
        "agent guide",
        "show in folder",
        "reveal",
        "open_link",
        "log_open_link",
    ]) {
        return Some("starts an external program (browser, code editor or file manager)");
    }
    if has(&["clear_caches", "clear caches"]) {
        return Some("deletes cache folders that are not inside the project copy");
    }
    if has(&["hot_reload", "compile", "build_game", "script_wizard"]) {
        return Some("builds game code with cargo (minutes; not an editor UI path)");
    }
    if has(&[
        "bake",
        "rebuild_asset_db",
        "rebuild asset database",
        "render_movie",
        "render movie",
    ]) {
        return Some("starts a long bake, cook or render job (covered by its own tests)");
    }
    None
}

/// The params to call `method` with: the menu model's own params, else the first agent guide
/// example, else `{}`. Placeholder strings (`"<file>"`) and every `path`/`out` param become files
/// under `scratch` (inside the project copy), so nothing is written elsewhere.
#[must_use]
pub fn safe_params(
    method: &str,
    given: Option<&Value>,
    examples: &BTreeMap<String, Value>,
    scratch: &str,
) -> Value {
    let base = given
        .filter(|v| v.as_object().is_some_and(|m| !m.is_empty()))
        .or_else(|| examples.get(method))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let Value::Object(map) = base else {
        return json!({});
    };
    let mut out = Map::new();
    let stem = method.replace('.', "_");
    for (key, value) in map {
        let placeholder = value
            .as_str()
            .is_some_and(|s| s.starts_with('<') && s.ends_with('>'));
        // A `path`-like key holds a file when its last segment has a suffix (`a.level.ron`);
        // `editor.menu_invoke { path: "Edit/Cut" }` and `"File/Save level as…"` are menu paths.
        let file_like = value.as_str().is_some_and(|s| {
            let last = s.rsplit('/').next().unwrap_or(s);
            !s.ends_with('…') && last.contains('.') && !last.starts_with('.')
        });
        let file_key = matches!(key.as_str(), "path" | "out" | "file" | "output") && file_like;
        let value = if (placeholder || file_key) && value.is_string() {
            // Keep the whole suffix (`.level.ron`), so loaders that read the file kind by
            // its suffix still accept the scratch file.
            let ext = value
                .as_str()
                .and_then(|s| Path::new(s.trim_matches(['<', '>'])).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .and_then(|n| n.find('.').map(|dot| n[dot..].to_owned()))
                .unwrap_or_default();
            json!(format!("{scratch}/{stem}_{key}{ext}"))
        } else {
            value
        };
        out.insert(key, value);
    }
    Value::Object(out)
}

/// `*` and `?` glob match over the whole text (`--only` / `--skip`).
#[must_use]
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Whether a step key (`kind:name`) runs under the `--only` and `--skip` globs.
#[must_use]
pub fn selected(key: &str, only: &[String], skip: &[String]) -> bool {
    (only.is_empty() || only.iter().any(|g| glob_match(g, key))) && !skip.iter().any(|g| glob_match(g, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_inventory() -> Inventory {
        let layout = json!({
            "layout": { "known_panels": ["viewport", "outliner", "content"] },
            "panels": ["viewport", "ext.counter", "outliner"],
        });
        let menus = json!({ "menus": [
            { "name": "File", "items": [
                { "path": "File/Save level", "action": "file.save_level", "class": "method", "method": "editor.save_level", "params": {}, "enabled": true },
                { "path": "File/Exit", "action": "file.exit", "class": "method", "method": "editor.menu_invoke", "params": { "path": "File/Exit" }, "enabled": true }
            ]},
            { "name": "Edit", "items": [
                { "path": "Edit/Undo", "action": "edit.undo", "class": "command", "method": "editor.command", "params": { "run": "edit.undo" }, "enabled": false }
            ]}
        ], "count": 3 });
        let commands = json!({ "commands": [
            { "id": "view.toggle_grid", "category": "View", "title": "Toggle grid" },
            { "id": "edit.undo", "category": "Edit", "title": "Undo" },
            { "id": "view.toggle_grid", "category": "View", "title": "Toggle grid" }
        ]});
        let actions = json!({
            "actions": [
                { "id": "file.save_level", "label": "Save level", "panel": "shell.file", "class": "method", "method": "editor.save_level" },
                { "id": "x.lost", "label": "Lost", "panel": "x", "class": "method", "method": "editor.not_there" }
            ],
            "sites": [
                { "panel": "p", "file": "crates/regia_editor/src/p.rs", "line": 12, "call": ".button(", "label": "Do it", "class": "gap" },
                { "panel": "p", "file": "crates/regia_editor/src/p.rs", "line": 40, "call": ".button(", "label": "Fold", "class": "view_only" }
            ],
            "counts": {}
        });
        let hello = json!({ "methods": ["editor.save_level", "editor.menu_invoke", "editor.command", { "name": "editor.layout_get" }] });
        let (actions, sites) = actions_from_reply(&actions);
        let mut inv = Inventory {
            panels: panels_from_layout(&layout),
            menus: menus_from_list(&menus),
            commands: commands_from_list(&commands),
            actions,
            sites,
            methods: methods_from_hello(&hello),
            findings: Vec::new(),
        };
        inv.findings = findings(&inv);
        inv
    }

    #[test]
    fn enumeration_reads_every_kind_from_the_replies() {
        let inv = fixture_inventory();
        let ids: Vec<&str> = inv.panels.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            ["viewport", "outliner", "content", "ext.counter"],
            "known panels first, extension panels after, no repeats"
        );
        assert_eq!(inv.menus.len(), 3);
        assert_eq!(inv.menus[1].params, json!({ "path": "File/Exit" }));
        assert!(!inv.menus[2].enabled);
        assert_eq!(inv.commands.len(), 2, "a repeated command id is listed once");
        assert_eq!(inv.actions.len(), 2);
        assert_eq!(inv.sites.len(), 2);
        assert!(
            inv.methods.contains("editor.layout_get"),
            "object-shaped method rows are read too"
        );
        let c = counts(&inv, 525);
        assert_eq!(c.panels, 4);
        assert_eq!(c.menu_items, 3);
        assert_eq!(c.sites_by_class.get("gap"), Some(&1));
        assert_eq!(c.snapshot_methods, 525);
    }

    #[test]
    fn a_gap_site_and_a_missing_route_are_findings() {
        let inv = fixture_inventory();
        let kinds: Vec<(&str, &str)> = inv
            .findings
            .iter()
            .map(|f| (f.kind.as_str(), f.subject.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("no_agent_route", "crates/regia_editor/src/p.rs:12"),
                ("route_missing", "x.lost")
            ]
        );
    }

    #[test]
    fn empty_replies_give_an_empty_inventory() {
        assert!(panels_from_layout(&json!({ "layout": null, "panels": [] })).is_empty());
        assert!(menus_from_list(&json!({})).is_empty());
        assert!(commands_from_list(&json!({ "commands": "nope" })).is_empty());
        let (a, s) = actions_from_reply(&Value::Null);
        assert!(a.is_empty() && s.is_empty());
        let inv = Inventory::default();
        assert!(
            findings(&inv).is_empty(),
            "no methods known: no route_missing guesses"
        );
    }

    #[test]
    fn snapshot_lines_give_names_and_required_params() {
        let text = "# header\neditor.menu_invoke(path: string)\neditor.history()\neditor.prefs_set(values: object, scope?: string)\n";
        let methods = parse_method_snapshot(text);
        assert_eq!(methods.len(), 3);
        assert_eq!(methods[0].required, ["path"]);
        assert!(methods[1].required.is_empty());
        assert_eq!(methods[2].required, ["values"]);
    }

    #[test]
    fn examples_keep_the_first_editor_request_per_method() {
        let pages = vec![(
            "a.md".to_owned(),
            "```json\n{\"method\": \"editor.layout_apply\", \"params\": {\"open\": \"preferences\"}}\n```\n{\"method\": \"editor.layout_apply\", \"params\": {\"open\": \"x\"}}\n{\"method\": \"scene.spawn\", \"params\": {}}\n".to_owned(),
        )];
        let ex = examples_from_markdown(&pages);
        assert_eq!(ex.len(), 1, "non-editor methods are not kept");
        assert_eq!(ex["editor.layout_apply"], json!({ "open": "preferences" }));
    }

    #[test]
    fn destructive_steps_are_skipped_with_a_reason() {
        assert!(skip_reason("menu:File/Exit editor.menu_invoke").is_some());
        assert!(skip_reason("menu:File/Package for platform… editor.package_start").is_some());
        assert!(skip_reason("action:help.docs Documentation editor.help_open").is_some());
        assert!(skip_reason("action:tools.clear Clear caches… editor.clear_caches").is_some());
        assert!(skip_reason("command:view.toggle_grid").is_none());
        assert!(skip_reason("menu:Edit/Undo editor.command").is_none());
    }

    #[test]
    fn file_params_land_in_the_scratch_folder() {
        let ex = BTreeMap::new();
        let p = safe_params(
            "asset.export_glb",
            Some(&json!({ "path": "<file>" })),
            &ex,
            "/tmp/p/smoke-out",
        );
        assert_eq!(p, json!({ "path": "/tmp/p/smoke-out/asset_export_glb_path" }));
        let p = safe_params(
            "editor.save_level",
            Some(&json!({ "path": "levels/a.level.ron", "confirm": true })),
            &ex,
            "/s",
        );
        assert_eq!(
            p,
            json!({ "path": "/s/editor_save_level_path.level.ron", "confirm": true })
        );
        let mut ex = BTreeMap::new();
        ex.insert("editor.layout_apply".to_owned(), json!({ "open": "preferences" }));
        assert_eq!(
            safe_params("editor.layout_apply", Some(&json!({})), &ex, "/s"),
            json!({ "open": "preferences" })
        );
        assert_eq!(safe_params("editor.none", None, &ex, "/s"), json!({}));
        let menu = json!({ "path": "Edit/Cut" });
        assert_eq!(
            safe_params("editor.menu_invoke", Some(&menu), &ex, "/s"),
            menu,
            "a menu path is not a file"
        );
        let dots = json!({ "path": "File/Save level as…" });
        assert_eq!(safe_params("editor.menu_invoke", Some(&dots), &ex, "/s"), dots);
    }

    #[test]
    fn globs_select_steps() {
        assert!(glob_match("panel:*", "panel:viewport"));
        assert!(glob_match("*grid*", "command:view.toggle_grid"));
        assert!(glob_match("menu:File/?xit", "menu:File/Exit"));
        assert!(!glob_match("panel:*", "menu:File/Exit"));
        assert!(glob_match("*", ""));
        assert!(selected("panel:viewport", &[], &[]));
        assert!(!selected("panel:viewport", &["menu:*".into()], &[]));
        assert!(!selected("panel:viewport", &[], &["panel:view*".into()]));
    }

    /// Lane v06-editor-undo-agent-routes (smoke bug 26): the agent guide gives the route steps of
    /// `editor.audio_mixer_set` and `editor.mesh_helper` their required params, so the smoke
    /// drives them instead of skipping ("needs [..] and the agent guide has no example"). The
    /// mesh helper's example is a built-in mesh, which no scratch path replaces.
    #[test]
    fn the_agent_guide_drives_the_audio_mixer_and_mesh_helper_routes() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the workspace above xtask");
        let examples = examples_from_markdown(&read_agent_pages(workspace));
        let mixer = safe_params("editor.audio_mixer_set", None, &examples, "/s");
        assert_eq!(mixer["bus"], "Music", "{mixer}");
        let helper = safe_params("editor.mesh_helper", None, &examples, "/s");
        assert_eq!(
            helper,
            json!({ "path": "builtin:cube", "action": "lightmap_uv" }),
            "{helper}"
        );
    }
}
