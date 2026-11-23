// SPDX-License-Identifier: MPL-2.0
//! Channel diagnostics derived from glTF 2.0 § 3.11 and Regia's animation importer.

use std::collections::BTreeSet;

use gltf::animation::{Interpolation, Property, util::ReadOutputs};

use super::{GltfLintError, GltfLintReport, GltfLintSettings, RigChannelWarning, charge};

pub(super) fn lint(
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    settings: &GltfLintSettings,
    report: &mut GltfLintReport,
    budget: &mut crate::content_lint::ContentBudget<'_>,
) -> Result<(), GltfLintError> {
    let joints: BTreeSet<_> = document
        .skins()
        .flat_map(|s| s.joints().map(|n| n.index()))
        .collect();
    let mut channels = 0;
    let mut elements = 0;
    for animation in document.animations() {
        let mut targets = BTreeSet::new();
        for (channel_index, channel) in animation.channels().enumerate() {
            budget.checkpoint()?;
            charge(&mut channels, 1, settings.max_channels, "max_channels")?;
            let target = channel.target();
            let node = target.node();
            let property = target.property();
            let property_name = match property {
                Property::Translation => "translation",
                Property::Rotation => "rotation",
                Property::Scale => "scale",
                Property::MorphTargetWeights => "weights",
            };
            let mut codes = BTreeSet::new();
            if !targets.insert((node.index(), property_name)) {
                codes.insert("gltf_rig_duplicate_channel");
            }
            if property != Property::MorphTargetWeights && !joints.contains(&node.index()) {
                codes.insert("gltf_rig_non_joint_channel");
            }
            let morph_count = if property == Property::MorphTargetWeights {
                let counts: Vec<_> = node
                    .mesh()
                    .into_iter()
                    .flat_map(|m| {
                        m.primitives()
                            .map(|p| p.morph_targets().count())
                            .collect::<Vec<_>>()
                    })
                    .collect();
                let count = counts.first().copied().unwrap_or(0);
                if count == 0 || counts.iter().any(|&c| c != count) {
                    codes.insert("gltf_rig_morph_target_missing");
                }
                count
            } else {
                1
            };
            let sampler = channel.sampler();
            let input_count = sampler.input().count();
            let output_count = sampler.output().count();
            charge(
                &mut elements,
                input_count,
                settings.max_animation_elements,
                "max_animation_elements",
            )?;
            charge(
                &mut elements,
                output_count,
                settings.max_animation_elements,
                "max_animation_elements",
            )?;
            let factor = if sampler.interpolation() == Interpolation::CubicSpline {
                3
            } else {
                1
            };
            let expected = input_count
                .checked_mul(factor)
                .and_then(|n| n.checked_mul(morph_count));
            if expected != Some(output_count) {
                codes.insert("gltf_rig_output_count");
            }
            let reader = channel.reader(|buffer| buffers.get(buffer.index()).map(|b| b.0.as_slice()));
            let mut previous = None;
            let mut observed_inputs = 0;
            if let Some(times) = reader.read_inputs() {
                for time in times {
                    observed_inputs += 1;
                    if !time.is_finite() || time < 0.0 || previous.is_some_and(|p| time <= p) {
                        codes.insert("gltf_rig_input_times");
                    }
                    previous = Some(time);
                }
            }
            if observed_inputs == 0 || observed_inputs != input_count {
                codes.insert("gltf_rig_input_times");
            }
            let (observed_outputs, finite) = match reader.read_outputs() {
                Some(ReadOutputs::Translations(v)) | Some(ReadOutputs::Scales(v)) => finite_rows(v),
                Some(ReadOutputs::Rotations(v)) => finite_rows(v.into_f32()),
                Some(ReadOutputs::MorphTargetWeights(v)) => finite_rows(v.into_f32().map(|v| [v])),
                None => (0, false),
            };
            if observed_outputs != output_count {
                codes.insert("gltf_rig_output_count");
            }
            if !finite {
                codes.insert("gltf_rig_nonfinite_output");
            }
            for code in codes {
                report.rig_channels.push(RigChannelWarning {
                    code: code.into(),
                    animation: animation.index(),
                    channel: channel_index,
                    node: node.index(),
                });
                report.check_budget(settings)?;
            }
        }
    }
    Ok(())
}

fn finite_rows<const N: usize>(values: impl Iterator<Item = [f32; N]>) -> (usize, bool) {
    values.fold((0, true), |(count, finite), value| {
        (count + 1, finite && value.into_iter().all(f32::is_finite))
    })
}
