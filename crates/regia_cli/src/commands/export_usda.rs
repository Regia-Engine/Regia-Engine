// SPDX-License-Identifier: MPL-2.0
//! `regia export usda`: read a glTF binary and write one USDA stage (ADR-4604).
//!
//! The stage is an exchange file. It is not a cooked Regia format, so it has no
//! `format_version` and `regia upgrade-project` does not migrate it.
//!
//! Geometry text follows the OpenUSD usda examples for `Xform` and `Mesh`
//! (`points`, `faceVertexCounts`, `faceVertexIndices`, `normals`, `primvars:st`).
//! The glTF binary is read with the workspace `gltf` crate. See `docs/provenance.md`.

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use clap::{Args, FromArgMatches};

/// A refusal from `regia export usda`. `code` is the stable token.
#[derive(Debug)]
pub struct UsdaError {
    code: &'static str,
    detail: String,
}

impl UsdaError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// Stable refusal code: `usda_glb_missing`, `usda_glb_invalid`, `usda_empty`, or `usda_write`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl std::fmt::Display for UsdaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.detail)
    }
}

impl std::error::Error for UsdaError {}

/// `regia export usda`.
#[derive(Debug, Args)]
pub struct UsdaArgs {
    /// The glTF binary (`.glb`) to read.
    #[arg(long)]
    pub glb: PathBuf,
    /// The USDA stage to write.
    #[arg(long)]
    pub out: PathBuf,
}

/// What a successful export wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportSummary {
    /// Bytes of the USDA stage.
    pub bytes: usize,
    /// glTF nodes written as `Xform` prims (the stage root `World` is not one of them).
    pub nodes: usize,
    /// Triangle primitives written as `Mesh` prims.
    pub meshes: usize,
}

/// One mesh the stage writer emitted, as the test parser read it back.
///
/// This is the writer's own check, not a general USD parser. It understands the
/// attribute text this command writes (`points`, `faceVertexCounts`,
/// `faceVertexIndices`, and, when present, `normals` and `primvars:st`).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub struct StageMesh {
    /// Positions in the order written (the glTF position accessor order).
    pub points: Vec<[f32; 3]>,
    /// One count per face. Triangles are `3`.
    pub face_vertex_counts: Vec<i32>,
    /// Face corners in the order written (the glTF index order, or `0..n` when the primitive has no indices).
    pub face_vertex_indices: Vec<i32>,
    /// Per-vertex normals when the stage wrote them.
    pub normals: Option<Vec<[f32; 3]>>,
    /// The first UV set when the stage wrote `primvars:st`.
    pub uvs: Option<Vec<[f32; 2]>>,
}

/// Adds `usda` under the existing `export` command.
///
/// `export glb` stays in `commands/export.rs`. This only appends a subcommand so
/// that file can change beside this one.
pub fn attach(command: &mut clap::Command) {
    let export = command
        .find_subcommand_mut("export")
        .expect("regia export is a command");
    let usda = clap::Command::new("usda").about(
        "Write one USDA stage from a glTF binary: an Xform per node and a Mesh per triangle primitive",
    );
    *export = std::mem::take(export).subcommand(UsdaArgs::augment_args(usda));
}

/// Runs `export usda` when `matches` selected it.
///
/// Returns `None` for every other command so the caller can keep its own parser.
pub fn try_run(matches: &clap::ArgMatches) -> Option<std::process::ExitCode> {
    let export = matches.subcommand_matches("export")?;
    let (name, sub) = export.subcommand()?;
    if name != "usda" {
        return None;
    }
    let args = match UsdaArgs::from_arg_matches(sub) {
        Ok(args) => args,
        Err(err) => err.exit(),
    };
    Some(finish(run(&args)))
}

fn finish(result: Result<(), UsdaError>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Reads `args.glb` and atomically writes `args.out`.
pub fn run(args: &UsdaArgs) -> Result<(), UsdaError> {
    let summary = export_paths(&args.glb, &args.out)?;
    println!(
        "wrote {} ({} bytes, {} nodes, {} meshes)",
        args.out.display(),
        summary.bytes,
        summary.nodes,
        summary.meshes
    );
    Ok(())
}

/// Reads a `.glb` and writes one USDA stage. On `usda_empty` the output path is left untouched.
pub fn export_paths(glb: &Path, out: &Path) -> Result<ExportSummary, UsdaError> {
    let bytes = match std::fs::read(glb) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(UsdaError::new(
                "usda_glb_missing",
                format!("cannot read {}: {err}", glb.display()),
            ));
        }
        Err(err) => {
            return Err(UsdaError::new(
                "usda_glb_invalid",
                format!("cannot read {}: {err}", glb.display()),
            ));
        }
    };
    if !is_glb(&bytes) {
        return Err(UsdaError::new(
            "usda_glb_invalid",
            format!("{} is not a glTF binary", glb.display()),
        ));
    }
    let stage = stage_from_glb(&bytes)?;
    if stage.meshes == 0 {
        return Err(UsdaError::new(
            "usda_empty",
            "the glTF binary has no triangle mesh to write",
        ));
    }
    write_atomic(out, &stage.text)?;
    Ok(ExportSummary {
        bytes: stage.text.len(),
        nodes: stage.nodes,
        meshes: stage.meshes,
    })
}

fn is_glb(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && bytes.starts_with(b"glTF")
        && u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) == 2
}

struct StageText {
    text: String,
    nodes: usize,
    meshes: usize,
}

fn stage_from_glb(bytes: &[u8]) -> Result<StageText, UsdaError> {
    let (document, buffers, _images) = gltf::import_slice(bytes).map_err(|err| {
        UsdaError::new(
            "usda_glb_invalid",
            format!("the glTF binary could not be read: {err}"),
        )
    })?;
    let mut out = String::from(
        "#usda 1.0\n(\n    defaultPrim = \"World\"\n    metersPerUnit = 1.0\n    upAxis = \"Y\"\n)\n\ndef Xform \"World\"\n{\n",
    );
    let mut ctx = WriteCtx {
        buffers: &buffers,
        out: &mut out,
        nodes: 0,
        meshes: 0,
        stack: HashSet::new(),
    };
    let mut used = HashSet::new();
    for node in scene_roots(&document) {
        let name = unique_name(
            node.name().unwrap_or(""),
            &format!("Node{}", node.index()),
            &mut used,
        );
        write_node(&mut ctx, &node, &name, 1)?;
    }
    let WriteCtx { nodes, meshes, .. } = ctx;
    out.push_str("}\n");
    Ok(StageText {
        text: out,
        nodes,
        meshes,
    })
}

struct WriteCtx<'a> {
    buffers: &'a [gltf::buffer::Data],
    out: &'a mut String,
    nodes: usize,
    meshes: usize,
    stack: HashSet<usize>,
}

fn scene_roots(document: &gltf::Document) -> Vec<gltf::Node<'_>> {
    if let Some(scene) = document.default_scene().or_else(|| document.scenes().next()) {
        return scene.nodes().collect();
    }
    let mut children = HashSet::new();
    for node in document.nodes() {
        for child in node.children() {
            children.insert(child.index());
        }
    }
    document
        .nodes()
        .filter(|node| !children.contains(&node.index()))
        .collect()
}

fn write_node(
    ctx: &mut WriteCtx<'_>,
    node: &gltf::Node<'_>,
    name: &str,
    indent: usize,
) -> Result<(), UsdaError> {
    if !ctx.stack.insert(node.index()) {
        return Err(UsdaError::new(
            "usda_glb_invalid",
            "the glTF node hierarchy has a cycle",
        ));
    }
    push_line(ctx.out, indent, &format!("def Xform \"{name}\""));
    push_line(ctx.out, indent, "{");
    write_transform(ctx.out, indent + 1, node.transform())?;
    ctx.nodes += 1;

    let children: Vec<gltf::Node<'_>> = node.children().collect();
    let prims = read_prims(node, ctx.buffers)?;
    let mut used = HashSet::new();
    let mut child_names = Vec::with_capacity(children.len());
    for child in &children {
        child_names.push(unique_name(
            child.name().unwrap_or(""),
            &format!("Node{}", child.index()),
            &mut used,
        ));
    }
    let mut mesh_names = Vec::with_capacity(prims.len());
    for prim in &prims {
        mesh_names.push(unique_name(&prim.name_hint, "Mesh", &mut used));
    }
    for (prim, mesh_name) in prims.iter().zip(&mesh_names) {
        write_mesh(ctx.out, indent + 1, mesh_name, prim);
        ctx.meshes += 1;
    }
    for (child, child_name) in children.iter().zip(&child_names) {
        write_node(ctx, child, child_name, indent + 1)?;
    }
    push_line(ctx.out, indent, "}");
    ctx.stack.remove(&node.index());
    Ok(())
}

struct PrimData {
    name_hint: String,
    points: Vec<[f32; 3]>,
    counts: Vec<i32>,
    indices: Vec<i32>,
    normals: Option<Vec<[f32; 3]>>,
    uvs: Option<Vec<[f32; 2]>>,
}

fn read_prims(node: &gltf::Node<'_>, buffers: &[gltf::buffer::Data]) -> Result<Vec<PrimData>, UsdaError> {
    let Some(mesh) = node.mesh() else {
        return Ok(Vec::new());
    };
    let prims: Vec<_> = mesh.primitives().collect();
    let count = prims.len();
    let mut out = Vec::new();
    for (index, primitive) in prims.iter().enumerate() {
        if let Some(data) = read_prim(primitive, buffers, mesh.name(), index, count)? {
            out.push(data);
        }
    }
    Ok(out)
}

fn read_prim(
    primitive: &gltf::Primitive<'_>,
    buffers: &[gltf::buffer::Data],
    mesh_name: Option<&str>,
    index: usize,
    count: usize,
) -> Result<Option<PrimData>, UsdaError> {
    if primitive.mode() != gltf::mesh::Mode::Triangles {
        return Ok(None);
    }
    let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| data.0.as_slice()));
    let Some(positions) = reader.read_positions() else {
        return Ok(None);
    };
    let points: Vec<[f32; 3]> = positions.collect();
    if points.is_empty() {
        return Ok(None);
    }
    require_finite(points.iter().flatten().copied(), "a position")?;
    let indices = read_indices(&reader, points.len())?;
    if indices.is_empty() {
        return Ok(None);
    }
    if !indices.len().is_multiple_of(3) {
        return Err(UsdaError::new(
            "usda_glb_invalid",
            "a triangle primitive's index count is not a multiple of 3",
        ));
    }
    let normals = read_normals(&reader, points.len())?;
    let uvs = read_uvs(&reader, points.len())?;
    let triangles = indices.len() / 3;
    Ok(Some(PrimData {
        name_hint: prim_hint(mesh_name, index, count),
        points,
        counts: vec![3; triangles],
        indices,
        normals,
        uvs,
    }))
}

fn read_indices<'a, 's, F>(
    reader: &gltf::mesh::Reader<'a, 's, F>,
    point_count: usize,
) -> Result<Vec<i32>, UsdaError>
where
    F: Clone + Fn(gltf::Buffer<'a>) -> Option<&'s [u8]>,
{
    if let Some(indices) = reader.read_indices() {
        let mut out = Vec::new();
        for index in indices.into_u32() {
            if !usize::try_from(index).is_ok_and(|index| index < point_count) {
                return Err(UsdaError::new(
                    "usda_glb_invalid",
                    "a vertex index is outside the primitive's points",
                ));
            }
            let index = i32::try_from(index).map_err(|_| {
                UsdaError::new("usda_glb_invalid", "a vertex index does not fit in a USDA int")
            })?;
            out.push(index);
        }
        return Ok(out);
    }
    let mut out = Vec::with_capacity(point_count);
    for index in 0..point_count {
        let index = i32::try_from(index)
            .map_err(|_| UsdaError::new("usda_glb_invalid", "a vertex index does not fit in a USDA int"))?;
        out.push(index);
    }
    Ok(out)
}

fn read_normals<'a, 's, F>(
    reader: &gltf::mesh::Reader<'a, 's, F>,
    point_count: usize,
) -> Result<Option<Vec<[f32; 3]>>, UsdaError>
where
    F: Clone + Fn(gltf::Buffer<'a>) -> Option<&'s [u8]>,
{
    let Some(read) = reader.read_normals() else {
        return Ok(None);
    };
    let normals: Vec<[f32; 3]> = read.collect();
    if normals.iter().flatten().any(|value| !value.is_finite()) {
        return Err(UsdaError::new(
            "usda_glb_invalid",
            "a normal is not a finite number",
        ));
    }
    // A stream that does not match the points is not the per-vertex set. Omit it.
    if normals.len() == point_count {
        Ok(Some(normals))
    } else {
        Ok(None)
    }
}

fn read_uvs<'a, 's, F>(
    reader: &gltf::mesh::Reader<'a, 's, F>,
    point_count: usize,
) -> Result<Option<Vec<[f32; 2]>>, UsdaError>
where
    F: Clone + Fn(gltf::Buffer<'a>) -> Option<&'s [u8]>,
{
    let Some(read) = reader.read_tex_coords(0) else {
        return Ok(None);
    };
    let uvs: Vec<[f32; 2]> = read.into_f32().collect();
    if uvs.iter().flatten().any(|value| !value.is_finite()) {
        return Err(UsdaError::new("usda_glb_invalid", "a UV is not a finite number"));
    }
    if uvs.len() == point_count {
        Ok(Some(uvs))
    } else {
        Ok(None)
    }
}

fn require_finite(values: impl IntoIterator<Item = f32>, what: &str) -> Result<(), UsdaError> {
    if values.into_iter().any(|value| !value.is_finite()) {
        return Err(UsdaError::new(
            "usda_glb_invalid",
            format!("{what} is not a finite number"),
        ));
    }
    Ok(())
}

fn prim_hint(mesh_name: Option<&str>, index: usize, count: usize) -> String {
    match mesh_name {
        Some(name) if count == 1 => name.to_string(),
        Some(name) => format!("{name}_{index}"),
        None => format!("Mesh{index}"),
    }
}

fn write_mesh(out: &mut String, indent: usize, name: &str, prim: &PrimData) {
    push_line(out, indent, &format!("def Mesh \"{name}\""));
    push_line(out, indent, "{");
    let inner = indent + 1;
    write_vec3(out, inner, "point3f[] points", &prim.points, None);
    write_ints(out, inner, "int[] faceVertexCounts", &prim.counts);
    write_ints(out, inner, "int[] faceVertexIndices", &prim.indices);
    if let Some(normals) = &prim.normals {
        write_vec3(out, inner, "normal3f[] normals", normals, Some("vertex"));
    }
    if let Some(uvs) = &prim.uvs {
        write_vec2(out, inner, "texCoord2f[] primvars:st", uvs, Some("vertex"));
    }
    push_line(out, inner, "uniform token subdivisionScheme = \"none\"");
    push_line(out, inner, "uniform token orientation = \"rightHanded\"");
    let (min, max) = extent(&prim.points);
    write_vec3(out, inner, "float3[] extent", &[min, max], None);
    push_line(out, indent, "}");
}

fn write_transform(
    out: &mut String,
    indent: usize,
    transform: gltf::scene::Transform,
) -> Result<(), UsdaError> {
    match transform {
        gltf::scene::Transform::Matrix { matrix } => write_matrix(out, indent, matrix),
        gltf::scene::Transform::Decomposed {
            translation,
            rotation,
            scale,
        } => write_trs(out, indent, translation, rotation, scale),
    }
}

fn write_trs(
    out: &mut String,
    indent: usize,
    translation: [f32; 3],
    rotation: [f32; 4],
    scale: [f32; 3],
) -> Result<(), UsdaError> {
    require_finite(translation, "a translation")?;
    require_finite(rotation, "a rotation")?;
    require_finite(scale, "a scale")?;
    let mut order: Vec<&str> = Vec::new();
    // `xformOpOrder` lists ops in the opposite order from application, so the
    // last name is the most local. Scale, then orientation, then translation
    // matches glTF's T * R * S.
    if translation != [0.0, 0.0, 0.0] {
        push_indent(out, indent);
        out.push_str("double3 xformOp:translate = ");
        push_tuple3(out, &translation);
        out.push('\n');
        order.push("xformOp:translate");
    }
    if rotation != [0.0, 0.0, 0.0, 1.0] {
        push_indent(out, indent);
        out.push_str("quatf xformOp:orient = (");
        push_f32(out, rotation[3]);
        out.push_str(", ");
        push_f32(out, rotation[0]);
        out.push_str(", ");
        push_f32(out, rotation[1]);
        out.push_str(", ");
        push_f32(out, rotation[2]);
        out.push_str(")\n");
        order.push("xformOp:orient");
    }
    if scale != [1.0, 1.0, 1.0] {
        push_indent(out, indent);
        out.push_str("float3 xformOp:scale = ");
        push_tuple3(out, &scale);
        out.push('\n');
        order.push("xformOp:scale");
    }
    write_op_order(out, indent, &order);
    Ok(())
}

fn write_matrix(out: &mut String, indent: usize, matrix: [[f32; 4]; 4]) -> Result<(), UsdaError> {
    require_finite(matrix.iter().flatten().copied(), "a node matrix")?;
    if is_identity_matrix(matrix) {
        return Ok(());
    }
    // glTF stores columns (`matrix[column][row]`, translation in column 3).
    // USDA's `matrix4d` is row-vector, so each written row is that glTF column
    // and the translation sits in the last row.
    push_indent(out, indent);
    out.push_str("matrix4d xformOp:transform = ( ");
    for (row_index, row) in matrix.iter().enumerate() {
        if row_index > 0 {
            out.push_str(", ");
        }
        out.push('(');
        for (column_index, value) in row.iter().enumerate() {
            if column_index > 0 {
                out.push_str(", ");
            }
            push_f32(out, *value);
        }
        out.push(')');
    }
    out.push_str(" )\n");
    write_op_order(out, indent, &["xformOp:transform"]);
    Ok(())
}

fn is_identity_matrix(matrix: [[f32; 4]; 4]) -> bool {
    (0..4).all(|column| {
        (0..4).all(|row| {
            let expected = if column == row { 1.0 } else { 0.0 };
            matrix[column][row] == expected
        })
    })
}

fn write_op_order(out: &mut String, indent: usize, order: &[&str]) {
    if order.is_empty() {
        return;
    }
    push_indent(out, indent);
    out.push_str("uniform token[] xformOpOrder = [");
    for (index, name) in order.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push('"');
        out.push_str(name);
        out.push('"');
    }
    out.push_str("]\n");
}

fn write_ints(out: &mut String, indent: usize, decl: &str, values: &[i32]) {
    push_indent(out, indent);
    out.push_str(decl);
    out.push_str(" = [");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        push_display(out, *value);
    }
    out.push_str("]\n");
}

fn write_vec3(out: &mut String, indent: usize, decl: &str, values: &[[f32; 3]], interpolation: Option<&str>) {
    push_indent(out, indent);
    out.push_str(decl);
    out.push_str(" = [");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        push_tuple3(out, value);
    }
    close_array(out, indent, interpolation);
}

fn write_vec2(out: &mut String, indent: usize, decl: &str, values: &[[f32; 2]], interpolation: Option<&str>) {
    push_indent(out, indent);
    out.push_str(decl);
    out.push_str(" = [");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push('(');
        push_f32(out, value[0]);
        out.push_str(", ");
        push_f32(out, value[1]);
        out.push(')');
    }
    close_array(out, indent, interpolation);
}

fn close_array(out: &mut String, indent: usize, interpolation: Option<&str>) {
    out.push(']');
    if let Some(interpolation) = interpolation {
        out.push_str(" (\n");
        push_indent(out, indent + 1);
        out.push_str("interpolation = \"");
        out.push_str(interpolation);
        out.push_str("\"\n");
        push_indent(out, indent);
        out.push(')');
    }
    out.push('\n');
}

fn push_tuple3(out: &mut String, value: &[f32; 3]) {
    out.push('(');
    push_f32(out, value[0]);
    out.push_str(", ");
    push_f32(out, value[1]);
    out.push_str(", ");
    push_f32(out, value[2]);
    out.push(')');
}

fn push_f32(out: &mut String, value: f32) {
    push_display(out, value);
}

fn push_display(out: &mut String, value: impl std::fmt::Display) {
    let _ = std::fmt::Write::write_fmt(out, format_args!("{value}"));
}

fn extent(points: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
    let mut min = points[0];
    let mut max = points[0];
    for point in &points[1..] {
        for axis in 0..3 {
            min[axis] = min[axis].min(point[axis]);
            max[axis] = max[axis].max(point[axis]);
        }
    }
    (min, max)
}

fn push_line(out: &mut String, indent: usize, text: &str) {
    push_indent(out, indent);
    out.push_str(text);
    out.push('\n');
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("    ");
    }
}

fn unique_name(raw: &str, fallback: &str, used: &mut HashSet<String>) -> String {
    let base = sanitize(raw, fallback);
    if !used.contains(&base) {
        used.insert(base.clone());
        return base;
    }
    for suffix in 2..10_000 {
        let candidate = format!("{base}_{suffix}");
        if !used.contains(&candidate) {
            used.insert(candidate.clone());
            return candidate;
        }
    }
    let candidate = format!("{base}_{}", used.len());
    used.insert(candidate.clone());
    candidate
}

fn sanitize(raw: &str, fallback: &str) -> String {
    let source = if raw.is_empty() { fallback } else { raw };
    let mut out = String::with_capacity(source.len());
    for ch in source.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str(fallback);
    }
    if out.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        out.insert(0, '_');
    }
    out
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_atomic(path: &Path, text: &str) -> Result<(), UsdaError> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|err| {
            UsdaError::new("usda_write", format!("cannot create {}: {err}", parent.display()))
        })?;
    }
    let mut tmp_name = path
        .file_name()
        .unwrap_or(std::ffi::OsStr::new("stage.usda"))
        .to_os_string();
    let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    tmp_name.push(format!(".{}.{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let result = write_then_rename(&tmp, path, text);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_then_rename(tmp: &Path, path: &Path, text: &str) -> Result<(), UsdaError> {
    let mut file = File::create(tmp)
        .map_err(|err| UsdaError::new("usda_write", format!("cannot write {}: {err}", tmp.display())))?;
    std::io::Write::write_all(&mut file, text.as_bytes())
        .map_err(|err| UsdaError::new("usda_write", format!("cannot write {}: {err}", tmp.display())))?;
    file.sync_all()
        .map_err(|err| UsdaError::new("usda_write", format!("cannot write {}: {err}", tmp.display())))?;
    drop(file);
    std::fs::rename(tmp, path)
        .map_err(|err| UsdaError::new("usda_write", format!("cannot write {}: {err}", path.display())))?;
    Ok(())
}

/// Reads `Mesh` prims back from a stage this command wrote.
///
/// Not a general USD parser: it looks only for the attribute lines of those
/// prims and ignores everything else in the file.
#[cfg(test)]
pub fn parse_stage_meshes(text: &str) -> Result<Vec<StageMesh>, String> {
    let mut meshes = Vec::new();
    let mut rest = text;
    const MARK: &str = "def Mesh ";
    while let Some(at) = rest.find(MARK) {
        rest = &rest[at + MARK.len()..];
        let start = rest
            .find('{')
            .ok_or_else(|| "a Mesh prim has no body".to_string())?;
        let end_rel = rest[start + 1..]
            .find('}')
            .ok_or_else(|| "a Mesh prim body does not close".to_string())?;
        let body = &rest[start + 1..start + 1 + end_rel];
        meshes.push(StageMesh {
            points: group3(&parse_floats(array_inner(body, "point3f[] points")?)?)?,
            face_vertex_counts: parse_ints(array_inner(body, "int[] faceVertexCounts")?)?,
            face_vertex_indices: parse_ints(array_inner(body, "int[] faceVertexIndices")?)?,
            normals: optional_vec3(body, "normal3f[] normals")?,
            uvs: optional_vec2(body, "texCoord2f[] primvars:st")?,
        });
        rest = &rest[start + 1 + end_rel + 1..];
    }
    Ok(meshes)
}

#[cfg(test)]
fn optional_vec3(body: &str, key: &str) -> Result<Option<Vec<[f32; 3]>>, String> {
    if !body.contains(key) {
        return Ok(None);
    }
    Ok(Some(group3(&parse_floats(array_inner(body, key)?)?)?))
}

#[cfg(test)]
fn optional_vec2(body: &str, key: &str) -> Result<Option<Vec<[f32; 2]>>, String> {
    if !body.contains(key) {
        return Ok(None);
    }
    let values = parse_floats(array_inner(body, key)?)?;
    let (groups, rest) = values.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(format!("{key} has a trailing component"));
    }
    Ok(Some(groups.to_vec()))
}

#[cfg(test)]
fn array_inner<'a>(body: &'a str, key: &str) -> Result<&'a str, String> {
    let at = body.find(key).ok_or_else(|| format!("missing {key}"))?;
    let after = body[at + key.len()..].trim_start();
    let after = after
        .strip_prefix('=')
        .ok_or_else(|| format!("{key} has no value"))?
        .trim_start();
    let inner = after
        .strip_prefix('[')
        .ok_or_else(|| format!("{key} is not an array"))?;
    let end = inner
        .find(']')
        .ok_or_else(|| format!("{key} array does not close"))?;
    Ok(&inner[..end])
}

#[cfg(test)]
fn group3(values: &[f32]) -> Result<Vec<[f32; 3]>, String> {
    let (groups, rest) = values.as_chunks::<3>();
    if !rest.is_empty() {
        return Err("a float3 array has a trailing component".to_string());
    }
    Ok(groups.to_vec())
}

#[cfg(test)]
fn parse_ints(text: &str) -> Result<Vec<i32>, String> {
    let mut out = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(part.parse::<i32>().map_err(|_| format!("not an int: {part}"))?);
    }
    Ok(out)
}

#[cfg(test)]
fn parse_floats(text: &str) -> Result<Vec<f32>, String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let start_byte = bytes[index];
        if start_byte == b'+' || start_byte == b'-' || start_byte.is_ascii_digit() || start_byte == b'.' {
            let start = index;
            index += 1;
            while index < bytes.len() {
                let byte = bytes[index];
                if byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-') {
                    index += 1;
                } else {
                    break;
                }
            }
            let token = &text[start..index];
            let value = token
                .parse::<f32>()
                .map_err(|_| format!("not a float: {token}"))?;
            if !value.is_finite() {
                return Err(format!("not a finite float: {token}"));
            }
            out.push(value);
        } else {
            index += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};

    use super::*;

    struct PrimSpec {
        points: Vec<[f32; 3]>,
        indices: Option<Vec<u16>>,
        normals: Option<Vec<[f32; 3]>>,
        uvs: Option<Vec<[f32; 2]>>,
        mode: u32,
    }

    struct NodeSpec {
        name: &'static str,
        translation: Option<[f32; 3]>,
        matrix: Option<[f32; 16]>,
        prims: Vec<PrimSpec>,
        children: Vec<usize>,
    }

    fn triangle() -> PrimSpec {
        PrimSpec {
            points: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.1]],
            indices: Some(vec![0, 2, 1]),
            normals: Some(vec![[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]]),
            uvs: Some(vec![[0.0, 0.0], [1.0, 0.0], [0.1, 1.0]]),
            mode: 4,
        }
    }

    fn pack_glb(json_text: &[u8], bin: &[u8]) -> Vec<u8> {
        let mut json_bytes = json_text.to_vec();
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }
        let mut bin_bytes = bin.to_vec();
        while !bin_bytes.is_empty() && !bin_bytes.len().is_multiple_of(4) {
            bin_bytes.push(0);
        }
        let mut total = 12 + 8 + json_bytes.len();
        if !bin_bytes.is_empty() {
            total += 8 + bin_bytes.len();
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&u32::try_from(total).expect("glb fits").to_le_bytes());
        out.extend_from_slice(&u32::try_from(json_bytes.len()).expect("json fits").to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&json_bytes);
        if !bin_bytes.is_empty() {
            out.extend_from_slice(&u32::try_from(bin_bytes.len()).expect("bin fits").to_le_bytes());
            out.extend_from_slice(b"BIN\0");
            out.extend_from_slice(&bin_bytes);
        }
        out
    }

    fn scene_glb(nodes: &[NodeSpec]) -> Vec<u8> {
        let mut bin = Vec::new();
        let mut views = Vec::new();
        let mut accessors = Vec::new();
        let mut meshes = Vec::new();
        let mut mesh_of_node = Vec::new();

        for node in nodes {
            if node.prims.is_empty() {
                mesh_of_node.push(None);
                continue;
            }
            let mut prim_json = Vec::new();
            for prim in &node.prims {
                let mut attributes = serde_json::Map::new();
                let position = push_vec3(&mut bin, &mut views, &mut accessors, &prim.points, true);
                attributes.insert("POSITION".to_string(), json!(position));
                if let Some(normals) = &prim.normals {
                    let id = push_vec3(&mut bin, &mut views, &mut accessors, normals, false);
                    attributes.insert("NORMAL".to_string(), json!(id));
                }
                if let Some(uvs) = &prim.uvs {
                    let id = push_vec2(&mut bin, &mut views, &mut accessors, uvs);
                    attributes.insert("TEXCOORD_0".to_string(), json!(id));
                }
                let mut value = json!({
                    "attributes": Value::Object(attributes),
                    "mode": prim.mode,
                });
                if let Some(indices) = &prim.indices {
                    let id = push_u16(&mut bin, &mut views, &mut accessors, indices);
                    value["indices"] = json!(id);
                }
                prim_json.push(value);
            }
            mesh_of_node.push(Some(meshes.len()));
            meshes.push(json!({"name": node.name, "primitives": prim_json}));
        }

        let mut node_json = Vec::new();
        for (index, node) in nodes.iter().enumerate() {
            let mut value = json!({"name": node.name});
            if let Some(mesh) = mesh_of_node[index] {
                value["mesh"] = json!(mesh);
            }
            if let Some(translation) = node.translation {
                value["translation"] = json!(translation);
            }
            if let Some(matrix) = node.matrix {
                value["matrix"] = json!(matrix);
            }
            if !node.children.is_empty() {
                value["children"] = json!(node.children);
            }
            node_json.push(value);
        }
        let mut children = HashSet::new();
        for node in nodes {
            for child in &node.children {
                children.insert(*child);
            }
        }
        let roots: Vec<usize> = (0..nodes.len())
            .filter(|index| !children.contains(index))
            .collect();
        let mut root = json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": roots}],
            "nodes": node_json,
        });
        if !meshes.is_empty() {
            root["meshes"] = json!(meshes);
            root["accessors"] = json!(accessors);
            root["bufferViews"] = json!(views);
            root["buffers"] = json!([{"byteLength": bin.len()}]);
        }
        let json_bytes = serde_json::to_vec(&root).expect("json");
        pack_glb(&json_bytes, &bin)
    }

    fn align4(bin: &mut Vec<u8>) {
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
    }

    fn push_view(bin: &mut Vec<u8>, views: &mut Vec<Value>, payload: &[u8]) -> usize {
        align4(bin);
        let offset = bin.len();
        bin.extend_from_slice(payload);
        let id = views.len();
        views.push(json!({"buffer": 0, "byteOffset": offset, "byteLength": payload.len()}));
        id
    }

    fn push_vec3(
        bin: &mut Vec<u8>,
        views: &mut Vec<Value>,
        accessors: &mut Vec<Value>,
        points: &[[f32; 3]],
        with_bounds: bool,
    ) -> usize {
        let mut payload = Vec::new();
        for point in points {
            for component in point {
                payload.extend_from_slice(&component.to_le_bytes());
            }
        }
        let view = push_view(bin, views, &payload);
        let mut accessor = json!({
            "bufferView": view,
            "componentType": 5126,
            "count": points.len(),
            "type": "VEC3",
        });
        if with_bounds && !points.is_empty() {
            let (min, max) = extent(points);
            accessor["min"] = json!(min);
            accessor["max"] = json!(max);
        }
        let id = accessors.len();
        accessors.push(accessor);
        id
    }

    fn push_vec2(
        bin: &mut Vec<u8>,
        views: &mut Vec<Value>,
        accessors: &mut Vec<Value>,
        uvs: &[[f32; 2]],
    ) -> usize {
        let mut payload = Vec::new();
        for uv in uvs {
            payload.extend_from_slice(&uv[0].to_le_bytes());
            payload.extend_from_slice(&uv[1].to_le_bytes());
        }
        let view = push_view(bin, views, &payload);
        let id = accessors.len();
        accessors.push(json!({
            "bufferView": view,
            "componentType": 5126,
            "count": uvs.len(),
            "type": "VEC2",
        }));
        id
    }

    fn push_u16(
        bin: &mut Vec<u8>,
        views: &mut Vec<Value>,
        accessors: &mut Vec<Value>,
        indices: &[u16],
    ) -> usize {
        let mut payload = Vec::new();
        for index in indices {
            payload.extend_from_slice(&index.to_le_bytes());
        }
        let view = push_view(bin, views, &payload);
        let id = accessors.len();
        accessors.push(json!({
            "bufferView": view,
            "componentType": 5123,
            "count": indices.len(),
            "type": "SCALAR",
        }));
        id
    }

    fn write_glb(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write glb");
        path
    }

    fn temps(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let entry = entry.expect("entry");
            if entry.file_name().to_string_lossy().contains(".tmp") {
                found.push(entry.path());
            }
        }
        found
    }

    #[test]
    fn triangle_round_trip_keeps_points_counts_indices_normals_and_uvs() {
        let dir = regia_test_utils::unique_temp_dir("usda-tri");
        let glb = write_glb(
            &dir,
            "tri.glb",
            &scene_glb(&[NodeSpec {
                name: "Tri Node",
                translation: Some([1.0, 2.0, 3.0]),
                matrix: None,
                prims: vec![triangle()],
                children: Vec::new(),
            }]),
        );
        let out = dir.join("tri.usda");
        let summary = export_paths(&glb, &out).expect("export");
        assert_eq!(summary.meshes, 1);
        assert_eq!(summary.nodes, 1);
        assert!(temps(&dir).is_empty(), "the temp file is renamed away");
        let text = std::fs::read_to_string(&out).expect("read usda");
        assert!(text.contains("defaultPrim = \"World\""));
        assert!(text.contains("metersPerUnit = 1.0"));
        assert!(text.contains("upAxis = \"Y\""));
        assert!(text.contains("def Xform \"Tri_Node\""));
        assert!(text.contains("double3 xformOp:translate = (1, 2, 3)"));
        assert!(text.contains("subdivisionScheme = \"none\""));
        let meshes = parse_stage_meshes(&text).expect("parse");
        assert_eq!(meshes.len(), 1);
        assert_eq!(
            meshes[0].points,
            vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.1]]
        );
        assert_eq!(meshes[0].face_vertex_counts, vec![3]);
        assert_eq!(meshes[0].face_vertex_indices, vec![0, 2, 1]);
        assert_eq!(
            meshes[0].normals.as_deref(),
            Some([[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]].as_slice())
        );
        assert_eq!(
            meshes[0].uvs.as_deref(),
            Some([[0.0, 0.0], [1.0, 0.0], [0.1, 1.0]].as_slice())
        );
    }

    #[test]
    fn two_meshes_round_trip_and_a_mesh_without_normals_omits_them() {
        let dir = regia_test_utils::unique_temp_dir("usda-two");
        let child = PrimSpec {
            points: vec![[2.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 2.0]],
            indices: Some(vec![2, 0, 1]),
            normals: None,
            uvs: None,
            mode: 4,
        };
        let glb = write_glb(
            &dir,
            "two.glb",
            &scene_glb(&[
                NodeSpec {
                    name: "Parent",
                    translation: Some([1.0, 2.0, 3.0]),
                    matrix: None,
                    prims: vec![triangle()],
                    children: vec![1],
                },
                NodeSpec {
                    name: "Child",
                    translation: None,
                    matrix: Some([
                        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 4.0, 5.0, 6.0, 1.0,
                    ]),
                    prims: vec![child],
                    children: Vec::new(),
                },
            ]),
        );
        let out = dir.join("two.usda");
        let summary = export_paths(&glb, &out).expect("export");
        assert_eq!(summary.meshes, 2);
        assert_eq!(summary.nodes, 2);
        let text = std::fs::read_to_string(&out).expect("read usda");
        assert!(text.contains(
            "matrix4d xformOp:transform = ( (1, 0, 0, 0), (0, 1, 0, 0), (0, 0, 1, 0), (4, 5, 6, 1) )"
        ));
        let meshes = parse_stage_meshes(&text).expect("parse");
        assert_eq!(meshes.len(), 2);
        assert_eq!(meshes[0].face_vertex_indices, vec![0, 2, 1]);
        assert_eq!(meshes[0].points, triangle().points);
        assert_eq!(meshes[0].face_vertex_counts, vec![3]);
        assert!(meshes[0].normals.is_some());
        assert_eq!(
            meshes[1].points,
            vec![[2.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 2.0]]
        );
        assert_eq!(meshes[1].face_vertex_counts, vec![3]);
        assert_eq!(meshes[1].face_vertex_indices, vec![2, 0, 1]);
        assert_eq!(meshes[1].normals, None);
        assert_eq!(meshes[1].uvs, None);
    }

    #[test]
    fn two_primitives_keep_their_own_indices() {
        let dir = regia_test_utils::unique_temp_dir("usda-prims");
        let second = PrimSpec {
            points: vec![[5.0, 0.0, 0.0], [6.0, 0.0, 0.0], [5.0, 1.0, 0.0]],
            indices: Some(vec![0, 1, 2]),
            normals: None,
            uvs: None,
            mode: 4,
        };
        let glb = write_glb(
            &dir,
            "prims.glb",
            &scene_glb(&[NodeSpec {
                name: "Multi",
                translation: None,
                matrix: None,
                prims: vec![triangle(), second],
                children: Vec::new(),
            }]),
        );
        let out = dir.join("prims.usda");
        export_paths(&glb, &out).expect("export");
        let meshes = parse_stage_meshes(&std::fs::read_to_string(&out).expect("read")).expect("parse");
        assert_eq!(meshes.len(), 2);
        assert_eq!(meshes[0].face_vertex_indices, vec![0, 2, 1]);
        assert_eq!(meshes[1].points[0], [5.0, 0.0, 0.0]);
        assert_eq!(meshes[1].face_vertex_indices, vec![0, 1, 2]);
    }

    #[test]
    fn no_mesh_is_empty_and_does_not_touch_the_output() {
        let dir = regia_test_utils::unique_temp_dir("usda-empty");
        let glb = write_glb(
            &dir,
            "empty.glb",
            &scene_glb(&[NodeSpec {
                name: "Empty",
                translation: Some([1.0, 0.0, 0.0]),
                matrix: None,
                prims: Vec::new(),
                children: Vec::new(),
            }]),
        );
        let absent = dir.join("absent.usda");
        let err = export_paths(&glb, &absent).expect_err("empty");
        assert_eq!(err.code(), "usda_empty");
        assert!(!absent.exists());
        let out = dir.join("kept.usda");
        std::fs::write(&out, b"keep").expect("seed");
        let err = export_paths(&glb, &out).expect_err("empty");
        assert_eq!(err.code(), "usda_empty");
        assert_eq!(std::fs::read(&out).expect("still there"), b"keep");
        assert!(temps(&dir).is_empty());
    }

    #[test]
    fn lines_are_not_meshes_so_the_stage_is_empty() {
        let dir = regia_test_utils::unique_temp_dir("usda-lines");
        let mut line = triangle();
        line.mode = 1;
        let glb = write_glb(
            &dir,
            "lines.glb",
            &scene_glb(&[NodeSpec {
                name: "Lines",
                translation: None,
                matrix: None,
                prims: vec![line],
                children: Vec::new(),
            }]),
        );
        let err = export_paths(&glb, &dir.join("lines.usda")).expect_err("lines");
        assert_eq!(err.code(), "usda_empty");
        assert!(!dir.join("lines.usda").exists());
    }

    #[test]
    fn missing_invalid_and_write_errors_use_stable_codes() {
        let dir = regia_test_utils::unique_temp_dir("usda-err");
        let out = dir.join("out.usda");
        let missing = export_paths(&dir.join("nope.glb"), &out).expect_err("missing");
        assert_eq!(missing.code(), "usda_glb_missing");
        assert!(!out.exists());

        let bad = dir.join("bad.glb");
        std::fs::write(&bad, b"not a glb").expect("bad");
        let invalid = export_paths(&bad, &out).expect_err("invalid");
        assert_eq!(invalid.code(), "usda_glb_invalid");
        assert!(!out.exists());

        let glb = write_glb(
            &dir,
            "ok.glb",
            &scene_glb(&[NodeSpec {
                name: "Tri",
                translation: None,
                matrix: None,
                prims: vec![triangle()],
                children: Vec::new(),
            }]),
        );
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, b"file").expect("blocker");
        let write = export_paths(&glb, &blocked.join("stage.usda")).expect_err("write");
        assert_eq!(write.code(), "usda_write");
        assert!(temps(&dir).is_empty(), "a failed write removes its temp file");

        let as_dir = dir.join("out-dir");
        std::fs::create_dir(&as_dir).expect("dir");
        let write_dir = export_paths(&glb, &as_dir).expect_err("dir out");
        assert_eq!(write_dir.code(), "usda_write");
        assert!(temps(&dir).is_empty());
    }

    #[test]
    fn a_non_finite_position_is_invalid_and_writes_nothing() {
        let dir = regia_test_utils::unique_temp_dir("usda-nan");
        let mut prim = triangle();
        prim.points[0][0] = f32::NAN;
        let glb = write_glb(
            &dir,
            "nan.glb",
            &scene_glb(&[NodeSpec {
                name: "Nan",
                translation: None,
                matrix: None,
                prims: vec![prim],
                children: Vec::new(),
            }]),
        );
        let out = dir.join("nan.usda");
        let err = export_paths(&glb, &out).expect_err("nan");
        assert_eq!(err.code(), "usda_glb_invalid");
        assert!(!out.exists());
    }

    #[test]
    fn invalid_triangle_topology_leaves_the_destination_untouched() {
        for indices in [vec![0, 3, 1], vec![0, 1]] {
            let dir = regia_test_utils::unique_temp_dir("usda-topology");
            let mut prim = triangle();
            prim.indices = Some(indices);
            let glb = write_glb(
                &dir,
                "invalid.glb",
                &scene_glb(&[NodeSpec {
                    name: "Invalid",
                    translation: None,
                    matrix: None,
                    prims: vec![prim],
                    children: Vec::new(),
                }]),
            );
            let out = dir.join("kept.usda");
            std::fs::write(&out, b"keep").expect("seed");
            let err = export_paths(&glb, &out).expect_err("invalid topology");
            assert_eq!(err.code(), "usda_glb_invalid");
            assert_eq!(std::fs::read(&out).expect("kept"), b"keep");
            assert!(temps(&dir).is_empty());
        }
    }

    #[test]
    fn an_unindexed_triangle_uses_sequential_indices() {
        let dir = regia_test_utils::unique_temp_dir("usda-seq");
        let mut prim = triangle();
        prim.indices = None;
        let glb = write_glb(
            &dir,
            "seq.glb",
            &scene_glb(&[NodeSpec {
                name: "Seq",
                translation: None,
                matrix: None,
                prims: vec![prim],
                children: Vec::new(),
            }]),
        );
        let out = dir.join("seq.usda");
        export_paths(&glb, &out).expect("export");
        let meshes = parse_stage_meshes(&std::fs::read_to_string(&out).expect("read")).expect("parse");
        assert_eq!(meshes[0].face_vertex_indices, vec![0, 1, 2]);
        assert_eq!(meshes[0].points.len(), 3);
    }
}
