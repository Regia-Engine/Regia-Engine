// SPDX-License-Identifier: MPL-2.0
//! Cook order and runtime loading of ability assets by asset path (spec 53 § 4.1, ADR-9370).
//!
//! An ability is validated against its cost and cooldown effects, so those load first. [`load_order`]
//! sorts a set of assets so every cost and cooldown comes before the ability naming it (ties by
//! path, so the order is the same on every run); a reference to a file outside the set fails a
//! strict order with `abilities_reference_missing`, and a loop of references fails with
//! `abilities_reference_cycle` naming the paths on it. [`cook_ability_set`] is the cook of a
//! project's ability files in that order; [`load_ability_assets`] is the runtime loader: in a
//! packaged game (paks mounted) the `abilities-cooked` payload the pak holds for the path, else the
//! source text where it exists (a dev tree), else a cooked payload
//! ([`crate::table_source::read_table_packaged_first`]).

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;

use super::assets::{AbilityAssets, AssetKind, Diagnostic, Severity, has_errors, parse_asset};
use super::cooked::{CookedAbilityAsset, cooked_bytes, from_cooked_bytes};
use super::modifiers::RunStateTypes;
use crate::table_source::{TableBytes, read_table_packaged_first};
use crate::tags::TagRegistry;

/// The type path a cooked ability asset carries in its cooked-file header and pak manifest
/// entry (format `abilities-cooked`).
pub const ABILITIES_COOKED_TYPE_PATH: &str = "regia_framework::abilities::CookedAbilityAsset";

/// Diagnostics by asset path: the errors of [`load_order`] and [`cook_ability_set`].
pub type PathDiagnostics = Vec<(String, Diagnostic)>;

/// Whether the file name or asset path `name` is an ability format (`.attrs.ron`, `.effect.ron`,
/// `.ability.ron`; case-sensitive, as [`AssetKind::of`] is).
#[must_use]
pub fn is_ability_asset_name(name: &str) -> bool {
    AssetKind::of(name).is_some()
}

/// The references of `asset` that must load before it: an ability's cost and cooldown.
#[must_use]
pub fn load_order_references(asset: &CookedAbilityAsset) -> Vec<&str> {
    match asset {
        CookedAbilityAsset::Ability(a) => a
            .cost
            .iter()
            .chain(a.cooldown.iter())
            .map(String::as_str)
            .collect(),
        CookedAbilityAsset::Attributes(_) | CookedAbilityAsset::Effect(_) => Vec::new(),
    }
}

/// The paths of `assets` in load order: every cost and cooldown before the ability that names it,
/// otherwise by path. A reference outside the set is an error (`abilities_reference_missing`) when
/// `strict`, and is left to the registry otherwise (it may be loaded already).
///
/// # Errors
///
/// `(path, diagnostic)` pairs: `abilities_reference_missing` (strict only) for each asset naming a
/// file outside the set, or `abilities_reference_cycle` for every asset on one loop.
pub fn load_order(
    assets: &BTreeMap<String, CookedAbilityAsset>,
    strict: bool,
) -> Result<Vec<String>, PathDiagnostics> {
    // Edges inside the set: `deps[path]` must load before `path`.
    let mut deps: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut missing = Vec::new();
    for (path, asset) in assets {
        let mut mine = BTreeSet::new();
        for reference in load_order_references(asset) {
            if assets.contains_key(reference) {
                mine.insert(reference);
            } else if strict {
                missing.push((
                    path.clone(),
                    Diagnostic::error(
                        "abilities_reference_missing",
                        format!("`{path}` names `{reference}`, which is not an ability asset of the project"),
                    ),
                ));
            }
        }
        deps.insert(path.as_str(), mine);
    }
    if !missing.is_empty() {
        return Err(missing);
    }
    // Kahn's algorithm, smallest path first among the ready ones.
    let mut waiting: BTreeMap<&str, usize> = deps.iter().map(|(p, d)| (*p, d.len())).collect();
    let mut users: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (path, mine) in &deps {
        for dep in mine {
            users.entry(*dep).or_default().push(*path);
        }
    }
    let mut ready: BTreeSet<&str> = waiting
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(p, _)| *p)
        .collect();
    let mut order = Vec::with_capacity(assets.len());
    while let Some(path) = ready.pop_first() {
        order.push(path.to_owned());
        waiting.remove(path);
        for user in users.get(path).into_iter().flatten() {
            if let Some(n) = waiting.get_mut(user) {
                *n -= 1;
                if *n == 0 {
                    ready.insert(*user);
                }
            }
        }
    }
    if let Some((&start, _)) = waiting.iter().next() {
        let cycle = find_cycle(start, &deps, &waiting);
        let message = format!("cost and cooldown references form a loop: {}", cycle.join(" -> "));
        let mut on_cycle: Vec<&String> = cycle.iter().collect();
        on_cycle.sort();
        on_cycle.dedup();
        return Err(on_cycle
            .into_iter()
            .map(|path| {
                (
                    path.clone(),
                    Diagnostic::error("abilities_reference_cycle", message.clone()),
                )
            })
            .collect());
    }
    Ok(order)
}

/// A loop through the unordered assets, starting from `start` (every unordered asset waits on
/// another unordered one, so following the smallest waiting reference must come back around).
fn find_cycle(
    start: &str,
    deps: &BTreeMap<&str, BTreeSet<&str>>,
    waiting: &BTreeMap<&str, usize>,
) -> Vec<String> {
    let mut path: Vec<&str> = vec![start];
    let mut at = start;
    for _ in 0..=waiting.len() {
        let Some(next) = deps
            .get(at)
            .and_then(|d| d.iter().copied().find(|d| waiting.contains_key(d)))
        else {
            break;
        };
        if let Some(i) = path.iter().position(|p| *p == next) {
            let mut cycle: Vec<String> = path[i..].iter().map(|p| (*p).to_owned()).collect();
            cycle.push(next.to_owned());
            return cycle;
        }
        path.push(next);
        at = next;
    }
    path.iter().map(|p| (*p).to_owned()).collect()
}

/// Validation diagnostics that are the game's to check, not the cook's: the run-state types an
/// ability names are registered by the game's code (`unknown_run_state`).
fn is_game_check(d: &Diagnostic) -> bool {
    d.code == "unknown_run_state"
}

/// Cooks a project's ability files (spec 53 § 4.1, ADR-9370): `files` maps asset paths (under
/// `assets/`) to their text. Every file is parsed, the set is put in [`load_order`] (strict), and
/// each is validated in that order against the ones before it, as the game's load validates it,
/// then encoded (`abilities-cooked`). Checks on the game's run-state types are left to the game's
/// load. The same files cook to the same bytes.
///
/// # Errors
///
/// Every `(path, diagnostic)` error: parse and validation errors of each file, or the order's
/// `abilities_reference_missing` / `abilities_reference_cycle`. Warnings never fail the cook.
pub fn cook_ability_set(
    files: &BTreeMap<String, String>,
    tags: Option<&TagRegistry>,
) -> Result<BTreeMap<String, Vec<u8>>, PathDiagnostics> {
    let _span = tracing::debug_span!(target: "regia_framework", "abilities_cook_set").entered();
    let mut parsed = BTreeMap::new();
    let mut errors = Vec::new();
    for (path, text) in files {
        match parse_asset(path, text) {
            Ok(asset) => {
                parsed.insert(path.clone(), asset);
            }
            Err(d) => errors.push((path.clone(), d)),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let order = load_order(&parsed, true)?;
    let mut scratch = AbilityAssets::default();
    let mut cooked = BTreeMap::new();
    for path in order {
        let Some(asset) = parsed.remove(&path) else {
            continue;
        };
        let diags = scratch.validate_any(&asset, tags, None);
        let fatal: Vec<Diagnostic> = diags
            .into_iter()
            .filter(|d| d.severity == Severity::Error && !is_game_check(d))
            .collect();
        if !fatal.is_empty() {
            errors.extend(fatal.into_iter().map(|d| (path.clone(), d)));
            continue;
        }
        match cooked_bytes(&asset) {
            Ok(bytes) => {
                cooked.insert(path.clone(), bytes);
            }
            Err(d) => errors.push((path.clone(), d)),
        }
        match asset {
            CookedAbilityAsset::Attributes(a) => scratch.insert_attributes(&path, a),
            CookedAbilityAsset::Effect(a) => scratch.insert_effect(&path, a),
            CookedAbilityAsset::Ability(a) => scratch.insert_ability(&path, a),
        }
    }
    if errors.is_empty() {
        Ok(cooked)
    } else {
        Err(errors)
    }
}

/// What loading one asset path did.
#[derive(Debug, Clone, PartialEq)]
pub struct AbilityLoad {
    /// The asset came from its cooked payload (a pak), not source text.
    pub cooked: bool,
    /// Every diagnostic; an error means the asset was refused.
    pub diagnostics: Vec<Diagnostic>,
}

impl AbilityLoad {
    /// Whether the asset was added.
    #[must_use]
    pub fn loaded(&self) -> bool {
        !has_errors(&self.diagnostics)
    }
}

/// Reads one path: its text parsed, else its cooked payload decoded.
fn read_asset(world: &mut World, path: &str) -> Result<(CookedAbilityAsset, bool), Diagnostic> {
    match read_table_packaged_first(world, path, ABILITIES_COOKED_TYPE_PATH) {
        Ok(TableBytes::Text(text)) => parse_asset(path, &text).map(|a| (a, false)),
        Ok(TableBytes::Cooked(payload)) => {
            let asset = from_cooked_bytes(&payload)?;
            if let Some(kind) = AssetKind::of(path)
                && kind != asset.kind()
            {
                return Err(Diagnostic::error(
                    "cooked_kind_mismatch",
                    format!("`{path}` holds a cooked {:?} asset", asset.kind()),
                ));
            }
            Ok((asset, true))
        }
        Err(message) => Err(Diagnostic::error("asset_not_found", message)),
    }
}

/// Loads ability assets by asset path (under `assets/`) into the [`AbilityAssets`] resource
/// (inserted when missing): with paks mounted (a packaged game) the cooked payload the pak holds for
/// the path, else the source text where it exists (VFS, else the project's `assets/`), else a
/// cooked payload. The set loads in
/// [`load_order`] (a reference outside it may be loaded already), each validated with the world's
/// [`TagRegistry`] and [`RunStateTypes`] exactly as [`AbilityAssets::load_text`] would.
///
/// A path that cannot be read gets `asset_not_found`; a loop of references refuses the assets on
/// it with `abilities_reference_cycle` and loads the rest.
pub fn load_ability_assets(world: &mut World, paths: &[String]) -> BTreeMap<String, AbilityLoad> {
    let _span = tracing::debug_span!(target: "regia_framework", "abilities_load_paths").entered();
    let mut out = BTreeMap::new();
    let mut read = BTreeMap::new();
    let mut cooked = BTreeSet::new();
    for path in paths {
        match read_asset(world, path) {
            Ok((asset, was_cooked)) => {
                if was_cooked {
                    cooked.insert(path.clone());
                }
                read.insert(path.clone(), asset);
            }
            Err(d) => {
                out.insert(
                    path.clone(),
                    AbilityLoad {
                        cooked: false,
                        diagnostics: vec![d],
                    },
                );
            }
        }
    }
    let order = loop {
        match load_order(&read, false) {
            Ok(order) => break order,
            Err(refused) => {
                for (path, d) in refused {
                    read.remove(&path);
                    let was_cooked = cooked.contains(&path);
                    out.insert(
                        path,
                        AbilityLoad {
                            cooked: was_cooked,
                            diagnostics: vec![d],
                        },
                    );
                }
            }
        }
    };
    let tags = world.get_resource::<TagRegistry>().cloned();
    let run_types = world.get_resource::<RunStateTypes>().cloned();
    let mut assets = world.get_resource_or_insert_with(AbilityAssets::default);
    for path in order {
        let Some(asset) = read.remove(&path) else {
            continue;
        };
        let diagnostics = assets.validate_and_insert(&path, asset, tags.as_ref(), run_types.as_ref());
        for d in diagnostics.iter().filter(|d| d.severity == Severity::Error) {
            tracing::error!(target: "regia_framework", asset = %path, code = d.code, "ability asset refused: {}", d.message);
        }
        out.insert(
            path.clone(),
            AbilityLoad {
                cooked: cooked.contains(&path),
                diagnostics,
            },
        );
    }
    out
}

/// Ability asset paths (under `assets/`) [`load_registered_ability_assets`] loads, and what the last
/// load of each did.
#[derive(Resource, Debug, Clone, Default)]
pub struct AbilityAssetRegistrations {
    paths: BTreeSet<String>,
    /// The result of the last load, by path.
    pub loads: BTreeMap<String, AbilityLoad>,
}

impl AbilityAssetRegistrations {
    /// Registers `path` (an asset path such as `abilities/fire_bolt.ability.ron`).
    pub fn add(&mut self, path: impl Into<String>) {
        self.paths.insert(path.into());
    }

    /// The registered paths, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.paths.iter().map(String::as_str)
    }
}

/// `Startup` (added by [`AbilityAssetsAppExt::load_ability_assets_at_startup`]): loads every
/// registered path with [`load_ability_assets`] and keeps the results in
/// [`AbilityAssetRegistrations::loads`].
pub fn load_registered_ability_assets(world: &mut World) {
    let paths: Vec<String> = match world.get_resource::<AbilityAssetRegistrations>() {
        Some(registrations) => registrations.paths().map(str::to_owned).collect(),
        None => return,
    };
    let loads = load_ability_assets(world, &paths);
    if let Some(mut registrations) = world.get_resource_mut::<AbilityAssetRegistrations>() {
        registrations.loads = loads;
    }
}

/// Registers ability asset paths to load at startup.
pub trait AbilityAssetsAppExt {
    /// Loads `paths` (asset paths under `assets/`) at `Startup`: loose RON in a dev tree, the
    /// cooked form from the pak in a packaged game. The first call adds the startup system.
    fn load_ability_assets_at_startup<I, S>(&mut self, paths: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>;
}

impl AbilityAssetsAppExt for App {
    fn load_ability_assets_at_startup<I, S>(&mut self, paths: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let first = !self.world().contains_resource::<AbilityAssetRegistrations>();
        let mut registrations = self
            .world_mut()
            .get_resource_or_insert_with(AbilityAssetRegistrations::default);
        for path in paths {
            registrations.add(path);
        }
        if first {
            self.add_systems(Startup, load_registered_ability_assets);
        }
        self
    }
}
