// SPDX-License-Identifier: MPL-2.0
//! Read-only external workspace discovery. No Cargo subprocess, dependency resolution or build.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use super::{Args, CLOSURE, Closure, Report, clippy_methods, rel_path, scan_source};

/// A root boundary for every external manifest, config and source read. Symlinks (including
/// internal ones) are rejected before traversal; non-regular files are never opened. The caller
/// must keep the workspace stable during the scan, as for other source-analysis tools.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(root: &Path) -> Result<Self> {
        let root = root.canonicalize().context("external root must exist")?;
        ensure!(root.is_dir(), "external root must be a directory");
        Ok(Self { root })
    }

    fn relative(path: &Path) -> Result<PathBuf> {
        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(name) => relative.push(name),
                Component::CurDir => {}
                _ => bail!("path must stay relative to the external root: {}", path.display()),
            }
        }
        Ok(relative)
    }

    /// Validate each existing component without following links, even dangling ones. Optional
    /// configs and member-pattern candidates may be absent; required paths must exist.
    fn path(&self, relative: &Path, optional: bool) -> Result<Option<PathBuf>> {
        let relative = Self::relative(relative)?;
        let mut path = self.root.clone();
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            path.push(component);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("inspecting {}", relative.display()));
                }
            };
            ensure!(
                !metadata.file_type().is_symlink(),
                "symlink refused: {}",
                relative.display()
            );
            if components.peek().is_some() {
                ensure!(metadata.is_dir(), "not a directory: {}", relative.display());
            } else {
                ensure!(
                    metadata.is_dir() || metadata.is_file(),
                    "not a regular file or directory: {}",
                    relative.display()
                );
            }
        }
        Ok(Some(path))
    }

    fn read(&self, relative: &Path) -> Result<String> {
        let path = self.path(relative, false)?.context("missing file")?;
        ensure!(path.is_file(), "not a regular file: {}", relative.display());
        fs::read_to_string(path).with_context(|| format!("reading {}", relative.display()))
    }

    fn manifest(&self, dir: &Path) -> Result<toml::Value> {
        let path = dir.join("Cargo.toml");
        toml::from_str(&self.read(&path)?).with_context(|| format!("parsing {}", path.display()))
    }

    fn entries(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let path = self.path(dir, false)?.context("missing directory")?;
        let mut entries = fs::read_dir(path)?
            .map(|entry| entry.map(|entry| dir.join(entry.file_name())))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        Ok(entries)
    }

    fn sources(&self, dir: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
        let path = self.path(dir, false)?.context("missing source directory")?;
        ensure!(path.is_dir(), "not a source directory: {}", dir.display());
        for entry in self.entries(dir)? {
            let path = self.path(&entry, false)?.context("missing source entry")?;
            if path.is_dir() {
                self.sources(&entry, files)?;
            } else if entry.extension().is_some_and(|extension| extension == "rs") {
                files.insert(entry);
            }
        }
        Ok(())
    }

    /// Expand manifest member globs component by component, never traversing an unmatched folder.
    fn expand(
        &self,
        base: &Path,
        components: &[&str],
        excludes: &[regex::Regex],
        out: &mut BTreeSet<PathBuf>,
    ) -> Result<()> {
        if excluded(base, excludes) {
            return Ok(());
        }
        let Some((first, rest)) = components.split_first() else {
            if self.path(&base.join("Cargo.toml"), true)?.is_some() {
                out.insert(base.to_owned());
            }
            return Ok(());
        };
        if *first == "**" {
            self.expand(base, rest, excludes, out)?;
            for entry in self.entries(base)? {
                if excluded(&entry, excludes) {
                    continue;
                }
                let path = self.path(&entry, false)?.context("missing member entry")?;
                if path.is_dir() && !ignored_directory(&entry) {
                    self.expand(&entry, components, excludes, out)?;
                }
            }
        } else if first.contains(['*', '?', '[']) {
            let matcher = glob_regex(first)?;
            for entry in self.entries(base)? {
                if excluded(&entry, excludes) {
                    continue;
                }
                let name = entry.file_name().context("member name")?.to_string_lossy();
                if matcher.is_match(&name) {
                    let path = self.path(&entry, false)?.context("missing member entry")?;
                    if path.is_dir() {
                        self.expand(&entry, rest, excludes, out)?;
                    }
                }
            }
        } else {
            self.expand(&base.join(first), rest, excludes, out)?;
        }
        Ok(())
    }
}

fn excluded(path: &Path, patterns: &[regex::Regex]) -> bool {
    let relative = rel_path(Path::new(""), path);
    patterns.iter().any(|pattern| pattern.is_match(&relative))
}

fn ignored_directory(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == "target" || name.to_string_lossy().starts_with('.'))
}

/// Cargo-style member patterns: `*`, `?`, character classes and whole-component `**`.
fn glob_regex(pattern: &str) -> Result<regex::Regex> {
    let mut result = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    result.push_str("(?:.*/)?");
                } else {
                    result.push_str(".*");
                }
            }
            '*' => result.push_str("[^/]*"),
            '?' => result.push_str("[^/]"),
            '[' => {
                result.push('[');
                if chars.peek() == Some(&'!') {
                    chars.next();
                    result.push('^');
                }
                let mut closed = false;
                for next in chars.by_ref() {
                    result.push(next);
                    if next == ']' {
                        closed = true;
                        break;
                    }
                }
                ensure!(
                    closed,
                    "unclosed character class in workspace pattern `{pattern}`"
                );
            }
            _ => result.push_str(&regex::escape(&ch.to_string())),
        }
    }
    result.push('$');
    regex::Regex::new(&result).with_context(|| format!("invalid workspace pattern `{pattern}`"))
}

fn string_list(value: Option<&toml::Value>) -> Result<Vec<&str>> {
    match value {
        None => Ok(Vec::new()),
        Some(value) => value
            .as_array()
            .context("expected an array of paths")?
            .iter()
            .map(|value| value.as_str().context("expected a path string"))
            .collect(),
    }
}

fn package_name(manifest: &toml::Value) -> Result<&str> {
    manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .context("Cargo.toml has no package.name")
}

/// Path dependencies under the root are implicit workspace members. Paths outside it are never
/// resolved or read. A manifest can name registry/git dependencies without fetching anything.
fn local_dependencies(dir: &Path, manifest: &toml::Value, workspace: &toml::Value) -> Vec<PathBuf> {
    let mut tables = Vec::new();
    dependency_tables(manifest, &mut tables);
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            dependency_tables(target, &mut tables);
        }
    }
    let mut out = Vec::new();
    for table in tables {
        for (name, dependency) in table {
            let inherited = dependency.get("workspace").and_then(toml::Value::as_bool) == Some(true);
            let (base, dependency) = if inherited {
                let Some(dependency) = workspace.get("dependencies").and_then(|deps| deps.get(name)) else {
                    continue;
                };
                (Path::new(""), dependency)
            } else {
                (dir, dependency)
            };
            let Some(path) = dependency.get("path").and_then(toml::Value::as_str) else {
                continue;
            };
            let path = Path::new(path);
            if path.is_absolute() {
                continue;
            }
            let mut relative = base.to_owned();
            let mut inside = true;
            for component in path.components() {
                match component {
                    Component::Normal(name) => relative.push(name),
                    Component::CurDir => {}
                    Component::ParentDir => {
                        inside &= relative.pop();
                    }
                    _ => {
                        inside = false;
                    }
                }
            }
            if inside {
                out.push(relative);
            }
        }
    }
    out
}

fn dependency_tables<'a>(manifest: &'a toml::Value, out: &mut Vec<&'a toml::Table>) {
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = manifest.get(key).and_then(toml::Value::as_table) {
            out.push(table);
        }
    }
}

fn workspace(tree: &Tree) -> Result<BTreeMap<String, (PathBuf, toml::Value)>> {
    let manifest = tree.manifest(Path::new(""))?;
    let empty_workspace = toml::Value::Table(toml::Table::new());
    let ws = manifest.get("workspace").unwrap_or(&empty_workspace);
    ensure!(
        manifest.get("workspace").is_some() || manifest.get("package").is_some(),
        "external root Cargo.toml must declare [workspace] or [package]"
    );
    let excludes = string_list(ws.get("exclude"))?
        .into_iter()
        .map(glob_regex)
        .collect::<Result<Vec<_>>>()?;
    let mut pending = BTreeSet::new();
    if manifest.get("package").is_some() {
        pending.insert(PathBuf::new());
    }
    for member in string_list(ws.get("members"))? {
        let member = Tree::relative(Path::new(member))?;
        let text = rel_path(Path::new(""), &member);
        let components: Vec<_> = text.split('/').filter(|s| !s.is_empty()).collect();
        let mut matches = BTreeSet::new();
        tree.expand(Path::new(""), &components, &excludes, &mut matches)?;
        pending.extend(matches);
    }
    let mut seen = BTreeSet::new();
    let mut packages = BTreeMap::new();
    while let Some(dir) = pending.pop_first() {
        let relative = rel_path(Path::new(""), &dir);
        if !seen.insert(dir.clone()) || excludes.iter().any(|pattern| pattern.is_match(&relative)) {
            continue;
        }
        let package = tree.manifest(&dir)?;
        for dependency in local_dependencies(&dir, &package, ws) {
            pending.insert(dependency);
        }
        let name = package_name(&package)?.to_owned();
        ensure!(
            !packages.contains_key(&name),
            "duplicate workspace package `{name}`"
        );
        packages.insert(name, (dir, package));
    }
    Ok(packages)
}

fn source_files(tree: &Tree, dir: &Path, manifest: &toml::Value) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();
    let src = dir.join("src");
    if tree.path(&src, true)?.is_some() {
        tree.sources(&src, &mut files)?;
    }
    let targets = manifest.get("lib").into_iter().chain(
        manifest
            .get("bin")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten(),
    );
    for target in targets {
        if let Some(path) = target.get("path").and_then(toml::Value::as_str) {
            let path = dir.join(Tree::relative(Path::new(path))?);
            tree.path(&path, false)?;
            files.insert(path);
        }
    }
    ensure!(
        !files.is_empty(),
        "package `{}` has no Rust source files in src/ or explicit lib/bin paths",
        dir.display()
    );
    Ok(files)
}

/// Scan selected packages using only manifests, source and policy files beneath the root.
pub(super) fn scan(args: &Args) -> Result<Report> {
    let tree = Tree::new(args.external.as_deref().context("--external requires a root")?)?;
    ensure!(
        !args.crates.is_empty(),
        "--external requires at least one --crate"
    );
    let packages = workspace(&tree)?;
    let selected: BTreeSet<_> = args.crates.iter().collect();
    let mut sources = BTreeSet::new();
    let mut dirs = BTreeSet::new();
    for name in selected {
        let (dir, manifest) = packages
            .get(name)
            .with_context(|| format!("unknown workspace crate `{name}`"))?;
        sources.extend(source_files(&tree, dir, manifest)?);
        dirs.insert(dir.clone());
    }
    let mut report = Report {
        crates: dirs
            .iter()
            .map(|dir| {
                if dir.as_os_str().is_empty() {
                    ".".into()
                } else {
                    rel_path(Path::new(""), dir)
                }
            })
            .collect(),
        ..Report::default()
    };
    let closure = if args.with_closure {
        Some(toml::from_str::<Closure>(&tree.read(Path::new(CLOSURE))?).context("parsing external closure")?)
    } else {
        None
    };
    let math_module = args
        .math_module
        .as_deref()
        .or_else(|| closure.as_ref().map(|closure| Path::new(&closure.math_module)))
        .map(Tree::relative)
        .transpose()?;
    if let Some(math) = &math_module {
        let path = tree.path(math, false)?.context("missing math module")?;
        ensure!(path.is_file(), "--math-module must name one file");
        ensure!(
            args.math_module.is_none() || sources.contains(math),
            "math module must be a source file of a selected crate: {}",
            math.display()
        );
    }
    let pins = if let Some(closure) = &closure {
        check_policy(&tree, &packages, &dirs, closure, &mut report)?;
        closure
            .pin
            .iter()
            .filter(|pin| dirs.iter().any(|dir| Path::new(&pin.path).starts_with(dir)))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut counts = vec![0usize; pins.len()];
    for (index, pin) in pins.iter().enumerate() {
        Tree::relative(Path::new(&pin.path))?;
        if !super::RULES.contains(&pin.rule.as_str()) || pin.why.trim().is_empty() {
            report
                .problems
                .push(format!("pin {index} (`{}`): unknown rule or empty why", pin.path));
        }
    }
    for source in sources {
        let relative = rel_path(Path::new(""), &source);
        let text = tree.read(&source)?;
        report.files += 1;
        for finding in scan_source(&relative, &text, math_module.as_ref() == Some(&source)) {
            match pins.iter().position(|pin| {
                pin.path == finding.file && pin.text == finding.text && pin.rule == finding.rule
            }) {
                Some(index) => counts[index] += 1,
                None => report.findings.push(finding),
            }
        }
    }
    for (pin, count) in pins.iter().zip(counts) {
        if count != 1 {
            report.problems.push(format!(
                "pin `{}` `{}` ({}) matched {count} live line(s), expected 1",
                pin.path, pin.text, pin.rule
            ));
        }
    }
    Ok(report)
}

fn check_policy(
    tree: &Tree,
    packages: &BTreeMap<String, (PathBuf, toml::Value)>,
    selected: &BTreeSet<PathBuf>,
    closure: &Closure,
    report: &mut Report,
) -> Result<()> {
    let mut names = BTreeSet::new();
    let mut listed = BTreeSet::new();
    for krate in &closure.crates {
        let dir = Tree::relative(Path::new(&krate.path))?;
        let manifest = tree.manifest(&dir)?;
        let name = package_name(&manifest)?;
        ensure!(
            packages.get(name).is_some_and(|(path, _)| path == &dir),
            "closure crate `{}` is not a workspace member",
            krate.path
        );
        names.insert(name.to_owned());
        listed.insert(dir);
        if krate.why.trim().is_empty() {
            report
                .problems
                .push(format!("crate `{}`: why is empty", krate.path));
        }
    }
    let root_clippy = tree
        .path(Path::new("clippy.toml"), true)?
        .and_then(|path| clippy_methods(&path))
        .unwrap_or_default();
    let workspace = tree.manifest(Path::new(""))?;
    let inherited = workspace.get("workspace").and_then(|ws| ws.get("dependencies"));
    for dir in selected {
        let relative = rel_path(Path::new(""), dir);
        if !listed.contains(dir) {
            report
                .problems
                .push(format!("crate `{relative}` is not listed in {CLOSURE}"));
        }
        tree.path(&dir.join("Cargo.toml"), false)?;
        tree.path(&dir.join("clippy.toml"), true)?;
        let manifest = tree.manifest(dir)?;
        let mut tables = Vec::new();
        if let Some(table) = manifest.get("dependencies").and_then(toml::Value::as_table) {
            tables.push(table);
        }
        if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
            for target in targets.values() {
                if let Some(table) = target.get("dependencies").and_then(toml::Value::as_table) {
                    tables.push(table);
                }
            }
        }
        for table in tables {
            for (key, dependency) in table {
                let dependency = if dependency.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                    inherited
                        .and_then(|deps| deps.get(key))
                        .with_context(|| format!("missing inherited dependency `{key}`"))?
                } else {
                    dependency
                };
                let name = dependency
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(key);
                if (packages.contains_key(name) || dependency.get("path").is_some())
                    && !names.contains(name)
                    && !closure
                        .exempt
                        .iter()
                        .any(|exempt| exempt.name == name && !exempt.why.trim().is_empty())
                {
                    report.problems.push(format!("crate `{relative}` depends on the workspace crate `{name}`, which is neither a [[crate]] of the closure nor an [[exempt]] row with a why"));
                }
            }
        }
        super::check_clippy(
            &tree.root.join(dir),
            &relative,
            &root_clippy,
            &mut report.problems,
        );
    }
    Ok(())
}
