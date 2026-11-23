// SPDX-License-Identifier: MPL-2.0
//! Directors: the policy that picks the active rigs each frame
//! (`docs/specs/35-camera-framework.md` § 6.1).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use smallvec::SmallVec;

use super::asset::{CameraAsset, DirectorDef};
use super::nodes::{CondCtx, VarTable, eval_condition};

/// A rig a director asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RigRequest {
    /// The rig name.
    pub rig: String,
}

impl RigRequest {
    /// A request for the rig called `rig`.
    pub fn new(rig: impl Into<String>) -> Self {
        Self { rig: rig.into() }
    }
}

/// What a director (and a `Hook`) can read. Read-only over the world.
pub struct DirectorContext<'a> {
    /// The asset being driven.
    pub asset: &'a CameraAsset,
    /// The driver's variables.
    pub vars: &'a VarTable,
    /// The current primary rig, if any.
    pub primary: Option<&'a str>,
    /// Seconds the primary rig has been running.
    pub primary_elapsed: f32,
    /// Seconds since the driver started.
    pub time: f32,
    /// The driver's local player index.
    pub local_player: u8,
}

/// A Rust director registered by name (`Hook(name)`): the escape hatch for a game's state
/// machine. It must be a pure function of its context.
pub type DirectorHook = fn(&DirectorContext<'_>) -> Vec<RigRequest>;

/// The hooks registered with the camera plugin, by name.
#[derive(Resource, Default)]
pub struct CameraHooks {
    hooks: BTreeMap<String, DirectorHook>,
    any_name: bool,
}

impl CameraHooks {
    /// Registers (or replaces) a hook.
    pub fn register(&mut self, name: impl Into<String>, hook: DirectorHook) {
        self.hooks.insert(name.into(), hook);
    }

    /// Whether a hook of this name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.any_name || self.hooks.contains_key(name)
    }

    /// Accepts every hook name in validation, for a world that cannot know them: the editor's edit
    /// world validates assets without the game's code loaded. Evaluation still only finds hooks that
    /// were registered.
    pub fn accept_any_name(&mut self) {
        self.any_name = true;
    }

    /// The hook of this name.
    pub fn get(&self, name: &str) -> Option<DirectorHook> {
        self.hooks.get(name).copied()
    }

    /// Registered names in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.hooks.keys().map(String::as_str)
    }
}

/// Rig indices a director chose, primary first.
pub type Requests = SmallVec<[usize; 4]>;

/// Evaluates a director. `None` means the director has no opinion (a `Rules` director with no
/// matching rule and no default); a `Ranked` director then tries the next rank.
pub fn evaluate(def: &DirectorDef, ctx: &DirectorContext<'_>, hooks: &CameraHooks) -> Option<Requests> {
    match def {
        DirectorDef::Fixed(name) => rig_requests(ctx.asset, std::slice::from_ref(name)),
        DirectorDef::Rules { default, rules } => {
            // Conditions read the primary rig (or the first rig before one is chosen).
            let rig = ctx
                .primary
                .and_then(|name| ctx.asset.rig(name))
                .or_else(|| ctx.asset.rigs.first())?;
            let cond_ctx = CondCtx {
                elapsed: ctx.primary_elapsed,
                vars: ctx.vars,
                rig,
            };
            for rule in rules {
                if eval_condition(&rule.when, &cond_ctx) {
                    return rig_requests(ctx.asset, std::slice::from_ref(&rule.rig));
                }
            }
            if default.is_empty() {
                None
            } else {
                rig_requests(ctx.asset, std::slice::from_ref(default))
            }
        }
        DirectorDef::Ranked(list) => {
            // Highest rank first; equal ranks keep their listed order.
            let mut order: SmallVec<[usize; 4]> = (0..list.len()).collect();
            order.sort_by_key(|i| std::cmp::Reverse(list[*i].rank));
            order
                .into_iter()
                .find_map(|i| evaluate(&list[i].director, ctx, hooks))
        }
        DirectorDef::Hook(name) => {
            let hook = hooks.get(name)?;
            let requests = hook(ctx);
            let names: SmallVec<[&String; 4]> = requests.iter().map(|r| &r.rig).collect();
            let found: Requests = names
                .iter()
                .filter_map(|name| ctx.asset.rig_index(name))
                .collect();
            (!found.is_empty()).then_some(found)
        }
    }
}

fn rig_requests(asset: &CameraAsset, names: &[String]) -> Option<Requests> {
    let found: Requests = names.iter().filter_map(|n| asset.rig_index(n)).collect();
    (!found.is_empty()).then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::asset::{Condition, RankedDef, RuleDef};

    fn asset() -> CameraAsset {
        CameraAsset::from_ron(
            r#"(format_version: 1, name: "d",
                variables: [(name: "aim", default: Bool(false)), (name: "boost", default: Bool(false))],
                rigs: [(name: "a", root: [Fov(degrees: Literal(60.0))]),
                       (name: "b", root: [Fov(degrees: Literal(50.0))]),
                       (name: "c", root: [Fov(degrees: Literal(40.0))])],
                director: Fixed("a"))"#,
        )
        .expect("asset parses")
    }

    fn ctx<'a>(asset: &'a CameraAsset, vars: &'a VarTable) -> DirectorContext<'a> {
        DirectorContext {
            asset,
            vars,
            primary: None,
            primary_elapsed: 0.0,
            time: 0.0,
            local_player: 0,
        }
    }

    fn rules(default: &str) -> DirectorDef {
        DirectorDef::Rules {
            default: default.into(),
            rules: vec![
                RuleDef {
                    when: Condition::VarBool("aim".into(), true),
                    rig: "b".into(),
                },
                RuleDef {
                    when: Condition::VarBool("boost".into(), true),
                    rig: "c".into(),
                },
            ],
        }
    }

    #[test]
    fn fixed_always_picks_its_rig() {
        let asset = asset();
        let vars = VarTable::from_asset(&asset);
        let picked = evaluate(
            &DirectorDef::Fixed("c".into()),
            &ctx(&asset, &vars),
            &CameraHooks::default(),
        );
        assert_eq!(picked.as_deref(), Some(&[2usize][..]));
    }

    #[test]
    fn rules_first_match_wins_then_default() {
        let asset = asset();
        let mut vars = VarTable::from_asset(&asset);
        let hooks = CameraHooks::default();
        let def = rules("a");
        assert_eq!(
            evaluate(&def, &ctx(&asset, &vars), &hooks).as_deref(),
            Some(&[0usize][..])
        );
        assert!(vars.set("boost", crate::camera::asset::VarValue::Bool(true)));
        assert_eq!(
            evaluate(&def, &ctx(&asset, &vars), &hooks).as_deref(),
            Some(&[2usize][..])
        );
        assert!(vars.set("aim", crate::camera::asset::VarValue::Bool(true)));
        assert_eq!(
            evaluate(&def, &ctx(&asset, &vars), &hooks).as_deref(),
            Some(&[1usize][..]),
            "the first matching rule wins"
        );
    }

    #[test]
    fn rules_without_default_have_no_opinion() {
        let asset = asset();
        let vars = VarTable::from_asset(&asset);
        assert!(evaluate(&rules(""), &ctx(&asset, &vars), &CameraHooks::default()).is_none());
    }

    #[test]
    fn ranked_highest_some_wins() {
        let asset = asset();
        let mut vars = VarTable::from_asset(&asset);
        let hooks = CameraHooks::default();
        let def = DirectorDef::Ranked(vec![
            RankedDef {
                rank: 1,
                director: DirectorDef::Fixed("a".into()),
            },
            RankedDef {
                rank: 10,
                director: rules(""),
            },
        ]);
        assert_eq!(
            evaluate(&def, &ctx(&asset, &vars), &hooks).as_deref(),
            Some(&[0usize][..])
        );
        assert!(vars.set("aim", crate::camera::asset::VarValue::Bool(true)));
        assert_eq!(
            evaluate(&def, &ctx(&asset, &vars), &hooks).as_deref(),
            Some(&[1usize][..]),
            "the on-demand high-rank director acts only when a rule matches"
        );
    }

    #[test]
    fn hook_is_called_by_registered_name() {
        fn pick(ctx: &DirectorContext<'_>) -> Vec<RigRequest> {
            if ctx.time > 1.0 {
                vec![RigRequest::new("b"), RigRequest::new("c")]
            } else {
                vec![RigRequest::new("a")]
            }
        }
        let asset = asset();
        let vars = VarTable::from_asset(&asset);
        let mut hooks = CameraHooks::default();
        assert!(!hooks.contains("h"));
        hooks.register("h", pick);
        assert!(hooks.contains("h"));
        let def = DirectorDef::Hook("h".into());
        assert_eq!(
            evaluate(&def, &ctx(&asset, &vars), &hooks).as_deref(),
            Some(&[0usize][..])
        );
        let mut late = ctx(&asset, &vars);
        late.time = 2.0;
        assert_eq!(evaluate(&def, &late, &hooks).as_deref(), Some(&[1usize, 2][..]));
        assert!(evaluate(&DirectorDef::Hook("missing".into()), &late, &hooks).is_none());
    }
}
