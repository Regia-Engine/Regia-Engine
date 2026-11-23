// SPDX-License-Identifier: MPL-2.0
//! The physics layer matrix editor (`docs/specs/19-editor.md` § 2 "Physics layer matrix editor",
//! `docs/specs/15-physics.md` § 3, ADR-7233): the project's collision layers, the symmetric
//! collision matrix and the query matrix of `config/physics.ron`, edited in a Project Settings
//! section and by `editor.physics_layers_get` / `editor.physics_layers_edit`
//! (`book/agents/physics-layers.md`).
//!
//! Every edit rewrites the file at once in canonical RON ([`LayerFile::to_ron`]; comments are not
//! kept, the file is created when it is missing) and is one undo step. Pairs are written in
//! canonical order: a collision pair with the lower layer index first, then sorted; query pairs
//! sorted by (query layer, seen layer). A batch is all or nothing and is validated the way the
//! runtime builds the table ([`PhysicsLayers::from_file`]), so the file the editor writes is one a
//! game loads unchanged.
//!
//! Colliders store a layer *index* (`CollisionLayer`), so a layer can be renamed freely but only
//! the last one can be removed: removing one in the middle would move every later layer's
//! colliders onto their neighbor's row.

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_physics::{LAYER_CONFIG_PATH, LayerFile, MAX_LAYERS, PhysicsLayers};
use serde_json::{Value, json};

/// The longest layer name the editor accepts.
pub const MAX_LAYER_NAME: usize = 64;

/// One edit of the layer matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatrixEdit {
    /// Append a layer (it collides with nothing until a pair is set).
    AddLayer {
        /// The new layer's name.
        name: String,
    },
    /// Remove the last layer and every pair that names it.
    RemoveLayer {
        /// The layer's name (must be the last layer).
        name: String,
    },
    /// Rename a layer; its pairs follow it.
    RenameLayer {
        /// The current name.
        from: String,
        /// The new name.
        to: String,
    },
    /// Switch a collision pair on or off (symmetric: `a, b` and `b, a` are one pair).
    SetCollides {
        /// One layer.
        a: String,
        /// The other layer.
        b: String,
        /// Whether the pair collides.
        on: bool,
    },
    /// Switch a query-matrix cell: whether queries belonging to `from` see colliders on `seen`.
    SetQuery {
        /// The query's layer (the matrix row).
        from: String,
        /// The seen layer (the matrix column).
        seen: String,
        /// Whether the cell is set.
        on: bool,
    },
}

/// A refused matrix edit or an unreadable config, with its agent error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixEditError {
    /// `bad_physics_edit`, `bad_layer_name`, `duplicate_layer`, `unknown_layer`,
    /// `layer_not_last`, `too_many_layers`, `physics_config_invalid` or `physics_config_write`.
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for MatrixEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for MatrixEditError {}

fn refuse(code: &'static str, message: impl Into<String>) -> MatrixEditError {
    MatrixEditError {
        code,
        message: message.into(),
    }
}

/// `<root>/config/physics.ron`.
#[must_use]
pub fn config_path(root: &Path) -> PathBuf {
    root.join(LAYER_CONFIG_PATH)
}

/// Loads the project's layer config: the file when it exists (validated as the runtime builds
/// it), the engine default otherwise. The flag says whether the file exists.
///
/// # Errors
///
/// `physics_config_invalid` when the file cannot be read, parsed or built.
pub fn load_matrix(root: &Path) -> Result<(LayerFile, bool), MatrixEditError> {
    let path = config_path(root);
    if !path.exists() {
        return Ok((LayerFile::default(), false));
    }
    let file = LayerFile::load(&path).map_err(|e| refuse("physics_config_invalid", e.to_string()))?;
    PhysicsLayers::from_file(&file).map_err(|e| refuse("physics_config_invalid", e.to_string()))?;
    Ok((file, true))
}

/// Whether `name` is a layer name the editor writes: 1 to [`MAX_LAYER_NAME`] characters of
/// letters, digits, `_`, `-`, `.` and inner spaces.
#[must_use]
pub fn valid_layer_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_LAYER_NAME
        && name.trim() == name
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
}

/// The config with its pairs in canonical order and without repeats (module docs). Pairs naming an
/// unknown layer are kept at the end in file order (validation reports them).
#[must_use]
pub fn canonical(file: &LayerFile) -> LayerFile {
    let index = |name: &str| file.layers.iter().position(|n| n == name);
    let mut collides: Vec<(usize, usize)> = Vec::new();
    let mut queries: Vec<(usize, usize)> = Vec::new();
    let mut unknown_collides = Vec::new();
    let mut unknown_queries = Vec::new();
    for (a, b) in &file.collides {
        match (index(a), index(b)) {
            (Some(i), Some(j)) => collides.push((i.min(j), i.max(j))),
            _ => unknown_collides.push((a.clone(), b.clone())),
        }
    }
    for (from, seen) in &file.queries {
        match (index(from), index(seen)) {
            (Some(i), Some(j)) => queries.push((i, j)),
            _ => unknown_queries.push((from.clone(), seen.clone())),
        }
    }
    collides.sort_unstable();
    collides.dedup();
    queries.sort_unstable();
    queries.dedup();
    let named = |(i, j): (usize, usize)| (file.layers[i].clone(), file.layers[j].clone());
    LayerFile {
        layers: file.layers.clone(),
        collides: collides.into_iter().map(named).chain(unknown_collides).collect(),
        queries: queries.into_iter().map(named).chain(unknown_queries).collect(),
    }
}

/// Applies `edits` in order to `file`; the result is canonical and builds as a runtime table.
///
/// # Errors
///
/// The first refused edit (nothing of the batch applies).
pub fn apply_edits(file: &LayerFile, edits: &[MatrixEdit]) -> Result<LayerFile, MatrixEditError> {
    let mut out = file.clone();
    let known = |out: &LayerFile, name: &str| -> Result<(), MatrixEditError> {
        if out.layers.iter().any(|n| n == name) {
            Ok(())
        } else {
            Err(refuse("unknown_layer", format!("no layer named {name:?}")))
        }
    };
    for edit in edits {
        match edit {
            MatrixEdit::AddLayer { name } => {
                if !valid_layer_name(name) {
                    return Err(refuse(
                        "bad_layer_name",
                        format!(
                            "{name:?} is not a layer name (letters, digits, _ - . and inner spaces, 1-{MAX_LAYER_NAME})"
                        ),
                    ));
                }
                if out.layers.iter().any(|n| n == name) {
                    return Err(refuse(
                        "duplicate_layer",
                        format!("a layer named {name:?} exists"),
                    ));
                }
                if out.layers.len() >= MAX_LAYERS {
                    return Err(refuse(
                        "too_many_layers",
                        format!("a project has at most {MAX_LAYERS} layers"),
                    ));
                }
                out.layers.push(name.clone());
            }
            MatrixEdit::RemoveLayer { name } => {
                known(&out, name)?;
                if out.layers.last() != Some(name) {
                    return Err(refuse(
                        "layer_not_last",
                        format!(
                            "{name:?} is not the last layer: colliders store layer indices, so only the last layer can be removed (rename this one instead)"
                        ),
                    ));
                }
                out.layers.pop();
                out.collides.retain(|(a, b)| a != name && b != name);
                out.queries.retain(|(a, b)| a != name && b != name);
            }
            MatrixEdit::RenameLayer { from, to } => {
                known(&out, from)?;
                if from == to {
                    continue;
                }
                if !valid_layer_name(to) {
                    return Err(refuse("bad_layer_name", format!("{to:?} is not a layer name")));
                }
                if out.layers.iter().any(|n| n == to) {
                    return Err(refuse("duplicate_layer", format!("a layer named {to:?} exists")));
                }
                let rename = |n: &mut String| {
                    if *n == *from {
                        n.clone_from(to);
                    }
                };
                out.layers.iter_mut().for_each(rename);
                for (a, b) in out.collides.iter_mut().chain(out.queries.iter_mut()) {
                    rename(a);
                    rename(b);
                }
            }
            MatrixEdit::SetCollides { a, b, on } => {
                known(&out, a)?;
                known(&out, b)?;
                out.collides
                    .retain(|(x, y)| !((x == a && y == b) || (x == b && y == a)));
                if *on {
                    out.collides.push((a.clone(), b.clone()));
                }
            }
            MatrixEdit::SetQuery { from, seen, on } => {
                known(&out, from)?;
                known(&out, seen)?;
                out.queries.retain(|(x, y)| !(x == from && y == seen));
                if *on {
                    out.queries.push((from.clone(), seen.clone()));
                }
            }
        }
    }
    let out = canonical(&out);
    PhysicsLayers::from_file(&out).map_err(|e| refuse("physics_config_invalid", e.to_string()))?;
    Ok(out)
}

/// The undo entry of one `config/physics.ron` write: the file's text before and after. Applying
/// it writes `after`, reverting writes `before` (or removes the file when there was none).
#[derive(Debug, Clone)]
pub struct PhysicsMatrixEdit {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
    count: usize,
}

impl PhysicsMatrixEdit {
    fn write(&self, text: Option<&String>) {
        let result = match text {
            Some(text) => crate::level_io::write_atomic(&self.path, text.as_bytes()),
            None => std::fs::remove_file(&self.path).map_err(|e| e.to_string()),
        };
        if let Err(e) = result {
            tracing::warn!(target: "regia_editor", "{}: {e}", self.path.display());
        }
    }
}

impl crate::command::EditorCommand for PhysicsMatrixEdit {
    fn apply(&mut self, _world: &mut World) {
        self.write(self.after.as_ref());
    }
    fn revert(&mut self, _world: &mut World) {
        self.write(self.before.as_ref());
    }
    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }
    fn changes_level_state(&self) -> bool {
        // The level file is untouched: a project setting must not mark a saved level dirty.
        false
    }
    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        // The Undo History panel names the project file this entry rewrites.
        vec![crate::command::CommandTouch::asset("config/physics.ron", None)]
    }
    fn description(&self) -> String {
        format!("Edit physics layers ({} change(s))", self.count)
    }
}

/// Applies `edits` to the project's config and writes it; returns the new config and the undo
/// entry. An empty batch writes nothing and has no undo entry.
///
/// # Errors
///
/// A refused edit or an unreadable config (nothing is written), or `physics_config_write`.
pub fn edit_matrix(
    root: &Path,
    edits: &[MatrixEdit],
) -> Result<(LayerFile, Option<PhysicsMatrixEdit>), MatrixEditError> {
    let (current, _) = load_matrix(root)?;
    if edits.is_empty() {
        return Ok((current, None));
    }
    let next = apply_edits(&current, edits)?;
    let path = config_path(root);
    let before = std::fs::read_to_string(&path).ok();
    let text = next
        .to_ron()
        .map_err(|e| refuse("physics_config_write", e.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| refuse("physics_config_write", e.to_string()))?;
    }
    crate::level_io::write_atomic(&path, text.as_bytes()).map_err(|e| refuse("physics_config_write", e))?;
    Ok((
        next,
        Some(PhysicsMatrixEdit {
            path,
            before,
            after: Some(text),
            count: edits.len(),
        }),
    ))
}

/// Pushes a matrix write's undo entry on the editor's undo stack, when there is one.
pub fn push_matrix_undo(world: &mut World, command: PhysicsMatrixEdit) {
    if let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>() {
        stack.seal();
        stack.push(Box::new(command));
        stack.seal();
    }
}

/// The agent view of a config.
#[must_use]
pub fn matrix_json(file: &LayerFile, exists: bool) -> Value {
    let pairs =
        |pairs: &[(String, String)]| -> Vec<Value> { pairs.iter().map(|(a, b)| json!([a, b])).collect() };
    json!({
        "path": LAYER_CONFIG_PATH,
        "exists": exists,
        "max_layers": MAX_LAYERS,
        "layers": file.layers,
        "collides": pairs(&file.collides),
        "queries": pairs(&file.queries),
    })
}

/// The section's draft state (egui memory).
#[derive(Clone, Debug, Default)]
struct MatrixDraft {
    new_layer: String,
    toast: String,
}

/// Draws the physics layer matrix (`config/physics.ron`) in the Project Settings panel: the
/// collision matrix (lower triangle; it is symmetric), the query matrix, add layer and remove
/// last layer. Each change writes the file at once and leaves its undo entry in `undo` for the
/// shell to push.
pub fn matrix_section(ui: &mut egui::Ui, root: &Path, undo: &mut Vec<PhysicsMatrixEdit>) {
    let id = ui.make_persistent_id("physics_matrix_draft");
    let mut draft: MatrixDraft = ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let (file, exists) = match load_matrix(root) {
        Ok(loaded) => loaded,
        Err(err) => {
            ui.colored_label(tokens.error, err.to_string());
            return;
        }
    };
    if !exists {
        ui.weak("No config/physics.ron yet: the engine default is shown; the first change writes the file.");
    }
    let mut edits: Vec<MatrixEdit> = Vec::new();
    let names = &file.layers;
    let collides = |a: &str, b: &str| {
        file.collides
            .iter()
            .any(|(x, y)| (x == a && y == b) || (x == b && y == a))
    };
    let sees = |from: &str, seen: &str| file.queries.iter().any(|(x, y)| x == from && y == seen);

    ui.strong("Collision matrix (symmetric)");
    egui::Grid::new("physics_matrix_collides")
        .striped(true)
        .show(ui, |ui| {
            ui.label("");
            for name in names {
                ui.label(name);
            }
            ui.end_row();
            for (i, a) in names.iter().enumerate() {
                ui.label(a);
                for (j, b) in names.iter().enumerate() {
                    if j > i {
                        ui.label("");
                        continue;
                    }
                    let mut on = collides(a, b);
                    if ui
                        .checkbox(&mut on, "")
                        .on_hover_text(format!("{a} collides with {b}"))
                        .changed()
                    {
                        edits.push(MatrixEdit::SetCollides {
                            a: a.clone(),
                            b: b.clone(),
                            on,
                        });
                    }
                }
                ui.end_row();
            }
        });

    ui.strong("Query matrix (row: the query's layer; column: what it sees)");
    egui::Grid::new("physics_matrix_queries")
        .striped(true)
        .show(ui, |ui| {
            ui.label("");
            for name in names {
                ui.label(name);
            }
            ui.end_row();
            for from in names {
                ui.label(from);
                for seen in names {
                    let mut on = sees(from, seen);
                    if ui
                        .checkbox(&mut on, "")
                        .on_hover_text(format!("queries on {from} see {seen}"))
                        .changed()
                    {
                        edits.push(MatrixEdit::SetQuery {
                            from: from.clone(),
                            seen: seen.clone(),
                            on,
                        });
                    }
                }
                ui.end_row();
            }
        });

    ui.horizontal(|ui| {
        ui.text_edit_singleline(&mut draft.new_layer);
        if ui.button("Add layer").clicked() {
            edits.push(MatrixEdit::AddLayer {
                name: draft.new_layer.trim().to_owned(),
            });
        }
        if let Some(last) = names.last()
            && ui
                .button(format!("Remove {last}"))
                .on_hover_text("Only the last layer can be removed: colliders store layer indices")
                .clicked()
        {
            edits.push(MatrixEdit::RemoveLayer { name: last.clone() });
        }
    });
    if !edits.is_empty() {
        match edit_matrix(root, &edits) {
            Ok((_, command)) => {
                draft.toast.clear();
                if edits.iter().any(|e| matches!(e, MatrixEdit::AddLayer { .. })) {
                    draft.new_layer.clear();
                }
                undo.extend(command);
            }
            Err(err) => draft.toast = err.to_string(),
        }
    }
    if !draft.toast.is_empty() {
        ui.colored_label(tokens.error, &draft.toast);
    }
    ui.data_mut(|d| d.insert_temp(id, draft));
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn bad(code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            bad(
                "no_project",
                "no project is open",
                "open one with editor.open_project",
            )
        })
}

fn rpc(err: MatrixEditError) -> RpcError {
    bad(
        err.code,
        err.message,
        "nothing was written; see editor.physics_layers_get",
    )
}

/// One `edits` entry of `editor.physics_layers_edit`.
fn edit_from_json(value: &Value) -> Result<MatrixEdit, RpcError> {
    let text = |key: &str| -> Result<String, RpcError> {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| bad("bad_physics_edit", format!("`{key}` must be a string"), ""))
    };
    let flag = || -> Result<bool, RpcError> {
        value
            .get("on")
            .and_then(Value::as_bool)
            .ok_or_else(|| bad("bad_physics_edit", "`on` must be a boolean", "true or false"))
    };
    match value.get("op").and_then(Value::as_str) {
        Some("add_layer") => Ok(MatrixEdit::AddLayer { name: text("name")? }),
        Some("remove_layer") => Ok(MatrixEdit::RemoveLayer { name: text("name")? }),
        Some("rename_layer") => Ok(MatrixEdit::RenameLayer {
            from: text("from")?,
            to: text("to")?,
        }),
        Some("set_collides") => Ok(MatrixEdit::SetCollides {
            a: text("a")?,
            b: text("b")?,
            on: flag()?,
        }),
        Some("set_query") => Ok(MatrixEdit::SetQuery {
            from: text("from")?,
            seen: text("seen")?,
            on: flag()?,
        }),
        _ => Err(bad(
            "bad_physics_edit",
            "`op` must be add_layer, remove_layer, rename_layer, set_collides or set_query",
            "",
        )),
    }
}

/// `editor.physics_layers_get`.
fn physics_layers_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let (file, exists) = load_matrix(&root).map_err(rpc)?;
    Ok(matrix_json(&file, exists))
}

/// `editor.physics_layers_edit { edits }`.
fn physics_layers_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let edits = params
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            bad(
                "bad_physics_edit",
                "`edits` must be an array",
                "[{ op: \"set_collides\", a, b, on }]",
            )
        })?
        .iter()
        .map(edit_from_json)
        .collect::<Result<Vec<_>, _>>()?;
    let (file, command) = edit_matrix(&root, &edits).map_err(rpc)?;
    let exists = config_path(&root).exists();
    if let Some(command) = command {
        push_matrix_undo(world, command);
    }
    Ok(matrix_json(&file, exists))
}

/// The physics layer matrix agent methods.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.physics_layers_get",
                tool: "physics_layers_get",
                description: "The open project's collision layers (config/physics.ron, specs/15 § 3) as the Project Settings matrix shows them: layer names in index order, the symmetric collision matrix and the query matrix as canonical pairs. Without the file, the engine default (one self-colliding layer) with exists: false.",
                params: &[],
                result: "{ path, exists, max_layers, layers: [name], collides: [[a, b]], queries: [[from, seen]] }",
                mutating: false,
                permission: Permission::Read,
            },
            physics_layers_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.physics_layers_edit",
                tool: "physics_layers_edit",
                description: "Edit the open project's collision layers (config/physics.ron) as the Project Settings matrix does: add, rename or remove (last layer only: colliders store indices) layers and switch collision pairs (symmetric) and query cells, in order, all or nothing, validated as the runtime builds the table. The file is created when missing and rewritten in canonical RON (comments are lost). Undoable with editor.undo.",
                params: &[ParamSchema {
                    name: "edits",
                    kind: "array",
                    required: true,
                    description: "[{ op: \"add_layer\", name } | { op: \"remove_layer\", name } | { op: \"rename_layer\", from, to } | { op: \"set_collides\", a, b, on } | { op: \"set_query\", from, seen, on }]",
                }],
                result: "the editor.physics_layers_get result after the edits",
                mutating: true,
                permission: Permission::EditScene,
            },
            physics_layers_edit as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(layers: &[&str], collides: &[(&str, &str)], queries: &[(&str, &str)]) -> LayerFile {
        let own = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect()
        };
        LayerFile {
            layers: layers.iter().map(|s| (*s).to_owned()).collect(),
            collides: own(collides),
            queries: own(queries),
        }
    }

    #[test]
    fn canonical_orders_and_dedups_pairs() {
        let messy = file(
            &["world", "player", "enemy"],
            &[
                ("enemy", "world"),
                ("world", "enemy"),
                ("player", "world"),
                ("world", "world"),
            ],
            &[("enemy", "player"), ("player", "world"), ("enemy", "player")],
        );
        let tidy = canonical(&messy);
        assert_eq!(
            tidy.collides,
            file(
                &[],
                &[("world", "world"), ("world", "player"), ("world", "enemy")],
                &[]
            )
            .collides
        );
        assert_eq!(
            tidy.queries,
            file(&[], &[], &[("player", "world"), ("enemy", "player")]).queries
        );
        // The runtime table is the same before and after.
        assert_eq!(
            PhysicsLayers::from_file(&messy).ok(),
            PhysicsLayers::from_file(&tidy).ok()
        );
    }

    #[test]
    fn rename_carries_pairs_and_switching_off_removes_both_directions() {
        let start = file(&["a", "b"], &[("b", "a")], &[("a", "b")]);
        let out = apply_edits(
            &start,
            &[MatrixEdit::RenameLayer {
                from: "a".into(),
                to: "ground".into(),
            }],
        )
        .expect("rename");
        assert_eq!(out.collides, file(&[], &[("ground", "b")], &[]).collides);
        assert_eq!(out.queries, file(&[], &[], &[("ground", "b")]).queries);
        let off = apply_edits(
            &out,
            &[MatrixEdit::SetCollides {
                a: "ground".into(),
                b: "b".into(),
                on: false,
            }],
        )
        .expect("off");
        assert!(off.collides.is_empty());
    }

    #[test]
    fn layer_names_are_checked() {
        for good in ["world", "Player 2", "fx.debris", "a-b_c", "ébène"] {
            assert!(valid_layer_name(good), "{good}");
        }
        let long = "x".repeat(MAX_LAYER_NAME + 1);
        for bad in ["", " ", " lead", "trail ", "a\"b", "a\\b", "a/b", long.as_str()] {
            assert!(!valid_layer_name(bad), "{bad:?}");
        }
    }
}
