// SPDX-License-Identifier: MPL-2.0
//! The shader-hook interface: how a look plugs into the viewer's one forward pass.
//!
//! A hook is WGSL text for one [`ShaderFamily`] that defines exactly one required function:
//!
//! ```wgsl
//! fn shade_surface(s: Surface) -> vec3<f32>
//! ```
//!
//! It returns linear radiance before exposure. The viewer puts the hook between the shared prelude
//! (bindings, `Surface`, `env_radiance`, `env_diffuse`, `dfg`, `lut_sample`, `env_specular`,
//! `antialiased_roughness`, `fresnel_dielectric`, `hash11`) and the shared entry points (vertex stage,
//! exposure, tone mapping, output), then builds every family's pipelines once, when the renderer is
//! created. A hook reads its parameters from `material` ([`crate::MaterialBlock`]) and may read
//! `look_tex0` and `look_tex1` ([`crate::ViewerRenderer::set_look_texture`]).
//!
//! Rules for a hook, because WebGL2 is a target: no storage buffers, no compute, no new bindings, no
//! loops over scene data, no time. `cargo test -p regia_viewer` compiles every built-in hook to GLSL ES
//! 3.00 and fails on a violation.

use std::borrow::Cow;

use crate::material::ShaderFamily;

const PRELUDE: &str = include_str!("shaders/prelude.wgsl");
const ENTRY: &str = include_str!("shaders/entry.wgsl");
const CONTACT_SHADOW: &str = include_str!("shaders/contact_shadow.wgsl");
const FAMILY_METAL: &str = include_str!("shaders/family_metal.wgsl");
const FAMILY_GEM: &str = include_str!("shaders/family_gem.wgsl");
const FAMILY_OPAL: &str = include_str!("shaders/family_opal.wgsl");
const FAMILY_CABOCHON: &str = include_str!("shaders/family_cabochon.wgsl");
const FAMILY_FROSTED: &str = include_str!("shaders/family_frosted.wgsl");

/// The hook text of every family. Fixed when the renderer is created.
#[derive(Debug, Clone)]
pub struct ShaderHooks {
    sources: [Cow<'static, str>; ShaderFamily::ALL.len()],
}

impl Default for ShaderHooks {
    fn default() -> Self {
        Self {
            sources: [
                Cow::Borrowed(FAMILY_METAL),
                Cow::Borrowed(FAMILY_GEM),
                Cow::Borrowed(FAMILY_OPAL),
                Cow::Borrowed(FAMILY_CABOCHON),
                Cow::Borrowed(FAMILY_FROSTED),
            ],
        }
    }
}

impl ShaderHooks {
    /// Replaces one family's hook. `wgsl` defines `fn shade_surface(s: Surface) -> vec3<f32>`.
    #[must_use]
    pub fn with_hook(mut self, family: ShaderFamily, wgsl: impl Into<Cow<'static, str>>) -> Self {
        self.sources[family.index()] = wgsl.into();
        self
    }

    /// The hook text of one family.
    #[must_use]
    pub fn hook(&self, family: ShaderFamily) -> &str {
        &self.sources[family.index()]
    }

    /// The complete shader of one family: prelude, hook and entry points.
    #[must_use]
    pub fn family_source(&self, family: ShaderFamily) -> String {
        let hook = self.hook(family);
        let mut source = String::with_capacity(PRELUDE.len() + hook.len() + ENTRY.len() + 2);
        source.push_str(PRELUDE);
        source.push('\n');
        source.push_str(hook);
        source.push('\n');
        source.push_str(ENTRY);
        source
    }
}

/// The complete shader of the contact shadow.
#[must_use]
pub fn contact_shadow_source() -> String {
    let mut source = String::with_capacity(PRELUDE.len() + CONTACT_SHADOW.len() + 1);
    source.push_str(PRELUDE);
    source.push('\n');
    source.push_str(CONTACT_SHADOW);
    source
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_basis_doc_does_not_overstate_what_transpose_does() {
        // corr-web-viewer listed: the comment said `transpose(s.basis) * d` maps a direction
        // back unconditionally; that is exact only for the orthonormal bases the entry shader
        // builds (normalized matrix columns).
        assert!(
            PRELUDE.contains("so it is orthonormal and `transpose(s.basis) * d` is the"),
            "the basis comment must name the orthonormal precondition: {}",
            PRELUDE
                .lines()
                .find(|line| line.contains("transpose(s.basis)"))
                .unwrap_or("no transpose mention")
        );
    }

    /// Parses and validates `source` the way the WebGL2 path does, with the capabilities GLSL ES 3.00
    /// has, and writes each entry point as GLSL ES 3.00.
    fn check_for_webgl2(name: &str, source: &str, entries: &[(&str, naga::ShaderStage)]) {
        let module = naga::front::wgsl::parse_str(source)
            .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(source)));
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        for (_, global) in module.global_variables.iter() {
            assert!(
                !matches!(global.space, naga::AddressSpace::Storage { .. }),
                "{name}: a storage buffer cannot run on WebGL2"
            );
        }
        assert!(
            module
                .entry_points
                .iter()
                .all(|e| e.stage != naga::ShaderStage::Compute),
            "{name}: a compute entry point cannot run on WebGL2"
        );
        for (entry, stage) in entries {
            let options = naga::back::glsl::Options {
                version: naga::back::glsl::Version::Embedded {
                    version: 300,
                    is_webgl: true,
                },
                ..Default::default()
            };
            let pipeline = naga::back::glsl::PipelineOptions {
                shader_stage: *stage,
                entry_point: (*entry).to_string(),
                multiview: None,
            };
            let mut out = String::new();
            let mut writer = naga::back::glsl::Writer::new(
                &mut out,
                &module,
                &info,
                &options,
                &pipeline,
                naga::proc::BoundsCheckPolicies::default(),
            )
            .unwrap_or_else(|e| panic!("{name}/{entry}: {e:?}"));
            writer.write().unwrap_or_else(|e| panic!("{name}/{entry}: {e:?}"));
            assert!(out.contains("#version 300 es"), "{name}/{entry}: {out}");
        }
    }

    #[test]
    fn every_builtin_family_compiles_for_webgl2() {
        let hooks = ShaderHooks::default();
        for family in ShaderFamily::ALL {
            check_for_webgl2(
                family.name(),
                &hooks.family_source(family),
                &[
                    ("vs_main", naga::ShaderStage::Vertex),
                    ("fs_main", naga::ShaderStage::Fragment),
                ],
            );
        }
    }

    #[test]
    fn the_contact_shadow_compiles_for_webgl2() {
        check_for_webgl2(
            "contact_shadow",
            &contact_shadow_source(),
            &[
                ("vs_shadow", naga::ShaderStage::Vertex),
                ("fs_shadow", naga::ShaderStage::Fragment),
            ],
        );
    }

    #[test]
    fn a_hook_replaces_only_its_family() {
        let hook = "fn shade_surface(s: Surface) -> vec3<f32> { return s.normal * 0.5 + vec3<f32>(0.5); }";
        let hooks = ShaderHooks::default().with_hook(ShaderFamily::Gem, hook);
        assert!(hooks.family_source(ShaderFamily::Gem).contains("s.normal * 0.5"));
        assert!(
            !hooks
                .family_source(ShaderFamily::Metal)
                .contains("s.normal * 0.5")
        );
        check_for_webgl2(
            "override",
            &hooks.family_source(ShaderFamily::Gem),
            &[("fs_main", naga::ShaderStage::Fragment)],
        );
    }
}
