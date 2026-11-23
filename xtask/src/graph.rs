// SPDX-License-Identifier: MPL-2.0
//! Dependency graphs for the checks.
//!
//! Two sources, because they answer different questions:
//! - [`Graph::declared`] reads `cargo metadata` and keeps the dependencies each workspace crate
//!   *declares*. Used for direct layering rules.
//! - [`Graph::tree`] reads `cargo tree -p <crate>` for one crate, which resolves features the way a
//!   real build of that crate does, on every target platform. Used for transitive rules. (`cargo
//!   metadata`'s resolve merges every feature any workspace member enables, so it reports crates that
//!   a shipped game never compiles.)

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// One package in a graph.
#[derive(Debug, Clone)]
pub struct Package {
    /// Package name (`cargo metadata` spelling).
    pub name: String,
    /// Exact version for resolved graphs; `*` for declared external dependencies.
    pub version: String,
    /// True for members of this workspace.
    pub workspace: bool,
    /// Declared license, when the package states one.
    pub license: Option<String>,
    /// Declared repository URL, when the package states one.
    pub repository: Option<String>,
}

/// One dependency edge.
#[derive(Debug, Clone, Copy)]
pub struct Edge {
    /// Index into [`Graph::packages`] of the package this edge points to.
    pub to: usize,
    /// True when the edge is used only as a dev-dependency (tests, benches, examples).
    pub dev_only: bool,
}

/// A dependency graph.
#[derive(Debug, Default)]
pub struct Graph {
    /// All packages, in `cargo metadata` order.
    pub packages: Vec<Package>,
    /// For each package, its outgoing edges (same index order as `packages`).
    pub edges: Vec<Vec<Edge>>,
    /// Declared optional (feature-gated) non-dev edges, `(from, to)`. Filled by [`Graph::declared`] only.
    pub optional: BTreeSet<(usize, usize)>,
    /// Each workspace package's `[features]` table. Filled by [`Graph::declared`] only.
    pub features: BTreeMap<usize, BTreeMap<String, Vec<String>>>,
    /// The `<dependency>/<feature>` references each workspace package can turn on: every value of
    /// its `[features]` table plus each feature it asks of a dependency in its dependency table.
    /// Filled by [`Graph::declared`] only.
    pub feature_refs: BTreeMap<usize, Vec<String>>,
    index: HashMap<(String, String), usize>,
}

fn cargo() -> Command {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
}

fn run(cmd: &mut Command, what: &str) -> Result<String> {
    let output = cmd.output().with_context(|| format!("running {what}"))?;
    if !output.status.success() {
        bail!("{what} failed:\n{}", String::from_utf8_lossy(&output.stderr));
    }
    String::from_utf8(output.stdout).with_context(|| format!("{what} printed invalid UTF-8"))
}

impl Graph {
    /// Adds a package, or returns the existing one with the same name and version.
    pub fn add(&mut self, name: &str, version: &str, workspace: bool) -> usize {
        if let Some(&idx) = self.index.get(&(name.to_string(), version.to_string())) {
            return idx;
        }
        self.packages.push(Package {
            name: name.into(),
            version: version.into(),
            workspace,
            license: None,
            repository: None,
        });
        self.edges.push(Vec::new());
        let idx = self.packages.len() - 1;
        self.index.insert((name.into(), version.into()), idx);
        idx
    }

    /// Adds an edge unless it already exists.
    pub fn link(&mut self, from: usize, to: usize, dev_only: bool) {
        if !self.edges[from]
            .iter()
            .any(|e| e.to == to && e.dev_only == dev_only)
        {
            self.edges[from].push(Edge { to, dev_only });
        }
    }

    /// Workspace members and the dependencies they declare (all kinds, all targets, optional included).
    pub fn declared() -> Result<Self> {
        let json = run(
            cargo().args(["metadata", "--format-version", "1", "--locked", "--no-deps"]),
            "cargo metadata",
        )?;
        let meta: Metadata = serde_json::from_str(&json).context("parsing cargo metadata")?;
        let mut graph = Graph::default();
        let members: Vec<usize> = meta
            .packages
            .iter()
            .map(|p| graph.add(&p.name, &p.version, true))
            .collect();
        for (pkg, &from) in meta.packages.iter().zip(&members) {
            for dep in &pkg.dependencies {
                let to = match meta
                    .packages
                    .iter()
                    .position(|p| p.name == dep.name && dep.path.is_some())
                {
                    Some(i) => members[i],
                    None => graph.add(&dep.name, "*", false),
                };
                let dev = dep.kind.as_deref() == Some("dev");
                graph.link(from, to, dev);
                if dep.optional && !dev {
                    graph.optional.insert((from, to));
                }
                let local = dep.rename.as_deref().unwrap_or(&dep.name);
                graph
                    .feature_refs
                    .entry(from)
                    .or_default()
                    .extend(dep.features.iter().map(|f| format!("{local}/{f}")));
            }
            graph
                .feature_refs
                .entry(from)
                .or_default()
                .extend(pkg.features.values().flatten().cloned());
            graph.features.insert(from, pkg.features.clone());
        }
        Ok(graph)
    }

    /// The resolved normal + build dependency tree of one workspace crate on every shipping target (the
    /// `deny.toml` list; macOS deferred, no web target).
    /// The root is package 0.
    pub fn tree(workspace_root: &Path, package: &str) -> Result<Self> {
        Self::tree_with_features(workspace_root, package, &[])
    }

    /// The shipping-target tree of one crate with explicit optional features enabled.
    /// Defaults remain enabled, and the root is package 0.
    pub fn tree_with_features(workspace_root: &Path, package: &str, features: &[&str]) -> Result<Self> {
        let joined = features.join(",");
        let args: &[&str] = if features.is_empty() {
            &[]
        } else {
            &["--features", &joined]
        };
        Self::tree_with_args(workspace_root, package, args)
    }

    /// The shipping-target tree of one crate with explicit `cargo tree` feature arguments
    /// (`--all-features`, `--features a,b`, `--no-default-features`). The root is package 0.
    pub fn tree_with_args(workspace_root: &Path, package: &str, feature_args: &[&str]) -> Result<Self> {
        Self::tree_command(workspace_root, package, feature_args, false)
    }

    /// [`Graph::tree_with_args`], but asks cargo with `--offline` first and only goes to the network
    /// when that fails (review fixrev4 finding 2): with `--locked` the resolve is the same either way,
    /// and a host whose cache already holds every manifest makes no request at all.
    pub fn tree_offline_first(workspace_root: &Path, package: &str, feature_args: &[&str]) -> Result<Self> {
        Self::tree_command(workspace_root, package, feature_args, true).or_else(|offline| {
            Self::tree_command(workspace_root, package, feature_args, false)
                .with_context(|| format!("offline attempt: {offline:#}"))
        })
    }

    fn tree_command(
        workspace_root: &Path,
        package: &str,
        feature_args: &[&str],
        offline: bool,
    ) -> Result<Self> {
        let mut command = cargo();
        command.arg("tree").args(feature_args);
        if offline {
            command.arg("--offline");
        }
        let text = run(
            command.current_dir(workspace_root).args([
                "--locked",
                "--color",
                "never",
                "-p",
                package,
                "-e",
                "normal,build",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--target",
                "x86_64-pc-windows-msvc",
                "--target",
                "aarch64-linux-android",
                "--target",
                "x86_64-linux-android",
                "--prefix",
                "depth",
                "--format",
                "{p}|{l}|{r}",
            ]),
            "cargo tree",
        )?;
        Self::parse_tree(&text, workspace_root)
    }

    /// The resolved normal-dependency tree of `showcase --no-default-features --features demo` on
    /// every shipping target (the `deny.toml` list; macOS deferred, no web target). This is the v0.5
    /// demo packaging feature set (docs/showcase-plan.md, ADR-4847): the showcase scene with the
    /// agent protocol compiled out, so the demo is offline. Only `normal` edges: the notices list
    /// what links into the binary, not proc-macro or build-script crates.
    /// The root is package 0.
    pub fn demo_tree(workspace_root: &Path) -> Result<Self> {
        let text = run(
            cargo().current_dir(workspace_root).args([
                "tree",
                "--locked",
                "--color",
                "never",
                "-p",
                "showcase",
                "--no-default-features",
                "--features",
                "demo",
                "-e",
                "normal",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--target",
                "x86_64-pc-windows-msvc",
                "--target",
                "aarch64-linux-android",
                "--target",
                "x86_64-linux-android",
                "--prefix",
                "depth",
                "--format",
                "{p}|{l}|{r}",
            ]),
            "cargo tree (demo)",
        )?;
        Self::parse_tree(&text, workspace_root)
    }

    /// The resolved normal + build tree of `regia --no-default-features --features server` on the two
    /// server platforms (spec 76 § 13). The root is package 0.
    pub fn server_tree(workspace_root: &Path) -> Result<Self> {
        let text = run(
            cargo().current_dir(workspace_root).args([
                "tree",
                "--locked",
                "--color",
                "never",
                "-p",
                "regia",
                "--no-default-features",
                "--features",
                "server",
                "-e",
                "normal,build",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--target",
                "x86_64-pc-windows-msvc",
                "--prefix",
                "depth",
                "--format",
                "{p}|{l}|{r}",
            ]),
            "cargo tree (server)",
        )?;
        Self::parse_tree(&text, workspace_root)
    }

    fn parse_tree(text: &str, workspace_root: &Path) -> Result<Self> {
        let mut graph = Graph::default();
        let mut stack: Vec<usize> = Vec::new();
        let root = workspace_root.to_string_lossy();
        for line in text.lines() {
            let digits = line.chars().take_while(char::is_ascii_digit).count();
            if digits == 0 {
                continue; // blank lines and section headers
            }
            let depth: usize = line[..digits].parse()?;
            let mut fields = line[digits..].trim_end_matches(" (*)").splitn(3, '|');
            let spec = fields.next().unwrap_or_default();
            if spec.starts_with('[') {
                continue;
            }
            let mut words = spec.split_whitespace();
            let (Some(name), Some(version)) = (words.next(), words.next()) else {
                bail!("unexpected cargo tree line: {line}");
            };
            let version = version.trim_start_matches('v');
            let workspace = spec.contains(&format!("({root}"));
            let idx = graph.add(name, version, workspace);
            let nonempty = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);
            graph.packages[idx].license = nonempty(fields.next());
            graph.packages[idx].repository = nonempty(fields.next());
            stack.truncate(depth);
            if let Some(&parent) = stack.last() {
                graph.link(parent, idx, false);
            }
            stack.push(idx);
        }
        if graph.packages.is_empty() {
            bail!("cargo tree printed no packages");
        }
        Ok(graph)
    }

    /// Packages reachable from `root` through non-dev edges, each with the shortest path from `root`.
    /// `skip` decides edges that are not followed (allowed exceptions).
    pub fn reachable(&self, root: usize, skip: impl Fn(usize, usize) -> bool) -> BTreeMap<usize, Vec<usize>> {
        let mut parent: HashMap<usize, usize> = HashMap::new();
        let mut queue = VecDeque::from([root]);
        let mut seen = vec![false; self.packages.len()];
        seen[root] = true;
        while let Some(at) = queue.pop_front() {
            for edge in &self.edges[at] {
                if edge.dev_only || seen[edge.to] || skip(at, edge.to) {
                    continue;
                }
                seen[edge.to] = true;
                parent.insert(edge.to, at);
                queue.push_back(edge.to);
            }
        }
        let mut out = BTreeMap::new();
        for (idx, reached) in seen.iter().enumerate() {
            if !reached || idx == root {
                continue;
            }
            let mut path = vec![idx];
            let mut at = idx;
            while let Some(&p) = parent.get(&at) {
                path.push(p);
                at = p;
            }
            path.reverse();
            out.insert(idx, path);
        }
        out
    }

    /// Renders a path of package indices as `a -> b -> c`.
    pub fn path_string(&self, path: &[usize]) -> String {
        path.iter()
            .map(|&i| self.packages[i].name.as_str())
            .collect::<Vec<_>>()
            .join(" -> ")
    }
}

/// The manifests a resolve of this workspace reads besides the registry's: the root `Cargo.toml`,
/// every workspace member's and every path dependency's (`vendor/lewton`), as sorted paths relative
/// to the workspace root with `/` separators. Reads `cargo metadata --no-deps --offline`, which needs
/// no network and no downloaded crate.
pub fn manifest_paths(workspace_root: &Path) -> Result<Vec<String>> {
    let json = run(
        cargo().current_dir(workspace_root).args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--no-deps",
        ]),
        "cargo metadata --no-deps",
    )?;
    let meta: Metadata = serde_json::from_str(&json).context("parsing cargo metadata")?;
    let base = Path::new(&meta.workspace_root);
    let relative = |path: &Path| -> Result<String> {
        let rel = path
            .strip_prefix(base)
            .with_context(|| format!("{} is outside the workspace {}", path.display(), base.display()))?;
        Ok(rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"))
    };
    let mut out = vec!["Cargo.toml".to_string()];
    for pkg in &meta.packages {
        out.push(relative(Path::new(&pkg.manifest_path))?);
        for dep in &pkg.dependencies {
            if let Some(path) = &dep.path {
                out.push(relative(&Path::new(path).join("Cargo.toml"))?);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
    #[serde(default)]
    workspace_root: String,
}

#[derive(Deserialize)]
struct MetaPackage {
    name: String,
    version: String,
    dependencies: Vec<MetaDep>,
    #[serde(default)]
    manifest_path: String,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct MetaDep {
    name: String,
    kind: Option<String>,
    path: Option<String>,
    #[serde(default)]
    rename: Option<String>,
    #[serde(default)]
    optional: bool,
    #[serde(default)]
    features: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_tree_depth_output() {
        let text = "\
0regia_platform v0.1.0 (/ws/crates/regia_platform)||
1arboard v3.6.1|MIT OR Apache-2.0|https://github.com/1Password/arboard
2x11rb v0.13.2|MIT OR Apache-2.0|
3libloading v0.8.9|ISC|
1winit v0.30.13|Apache-2.0|
2x11rb v0.13.2|MIT OR Apache-2.0| (*)
";
        let g = Graph::parse_tree(text, Path::new("/ws")).expect("parses");
        assert_eq!(g.packages.len(), 5);
        assert!(g.packages[0].workspace);
        assert!(!g.packages[1].workspace);
        assert_eq!(g.packages[3].version, "0.8.9");
        assert_eq!(g.packages[1].license.as_deref(), Some("MIT OR Apache-2.0"));
        let reach = g.reachable(0, |_, _| false);
        assert_eq!(
            g.path_string(&reach[&3]),
            "regia_platform -> arboard -> x11rb -> libloading"
        );
        let x11rb = 2;
        assert_eq!(
            g.edges[4].iter().map(|e| e.to).collect::<Vec<_>>(),
            vec![x11rb],
            "winit -> x11rb kept"
        );
    }
}
