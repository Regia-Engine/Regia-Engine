// SPDX-License-Identifier: MPL-2.0
//! Pose-space correctives (Tincal C4): morph weights authored at example joint poses and
//! interpolated between them with radial basis functions.
//!
//! An elbow that collapses the mesh at 140° is fixed by sculpting a corrective morph target at
//! that pose. [`PoseSpaceCorrective`] lists the joints it watches and a set of example poses
//! (joint rotations, euler degrees, as a delta from the bind rotation in the joint's local
//! frame) with the morph weights wanted at each. At compile time the example weights are
//! solved into radial-basis coefficients; at run time the current joint rotations are measured
//! against the examples and the interpolated weights are **added** to the morph bank. The
//! bind pose is an implicit example with all weights zero, so a rig at rest gets no
//! correction and each example pose reproduces its authored weights exactly.
//!
//! This is *authored* pose-space deformation. It does not use learned pose-feature
//! correctives.
//!
//! Provenance (`docs/provenance.md`): J. P. Lewis, M. Cordner, N. Fong, "Pose Space
//! Deformation: A Unified Approach to Shape Interpolation and Skeleton-Driven Deformation",
//! SIGGRAPH 2000 (RBF interpolation of example shapes in pose space); Gaussian kernel and the
//! dense symmetric solve (Gaussian elimination with partial pivoting and a small ridge) are
//! textbook.
//!
//! All transcendentals (`acos`, `exp`, `sin`, `cos`) go through [`regia_core::math::det`],
//! so compiled coefficients and kernel activations are bit-identical on every platform,
//! and the elimination pivot breaks exact ties by the lowest row index (audit-v040 M6).

use regia_core::math::{Quat, Vec3, det};
use serde::{Deserialize, Serialize};

use crate::pose::Pose;
use crate::skeleton::Skeleton;

fn default_radius() -> f32 {
    0.8
}

fn one() -> f32 {
    1.0
}

/// One example pose of a [`PoseSpaceCorrective`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CorrectivePose {
    /// Name for the agent and the editor (`"elbow_140"`).
    pub name: String,
    /// One euler rotation (XYZ order, degrees) per watched joint: the joint's local rotation
    /// delta from its bind rotation at this example.
    pub rotations: Vec<Vec3>,
    /// Morph target name → weight wanted at this pose.
    pub morphs: Vec<(String, f32)>,
}

/// A pose-space corrective: example poses of some joints and the morph weights at each.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PoseSpaceCorrective {
    /// Name for the agent and the editor.
    pub name: String,
    /// The watched joints, in the order the examples list their rotations.
    pub joints: Vec<String>,
    /// The example poses (at least one).
    pub poses: Vec<CorrectivePose>,
    /// Gaussian kernel width in radians of combined joint rotation distance: an example
    /// influences poses within roughly this distance (default 0.8, about 46°).
    #[serde(default = "default_radius")]
    pub radius: f32,
    /// Multiplier on the whole corrective's output (default 1).
    #[serde(default = "one")]
    pub weight: f32,
}

/// A [`PoseSpaceCorrective`] resolved against a skeleton and a morph set, with the RBF
/// coefficients solved.
#[derive(Clone, Debug)]
pub struct CompiledCorrective {
    /// Corrective name.
    pub name: String,
    joints: Vec<usize>,
    bind: Vec<Quat>,
    /// Example rotation deltas, one `Vec` per example (the implicit bind example is last).
    samples: Vec<Vec<Quat>>,
    /// Morph indices written.
    outputs: Vec<usize>,
    /// `coefficients[sample][output]`.
    coefficients: Vec<Vec<f32>>,
    radius: f32,
    weight: f32,
}

/// Combined rotation distance of two example vectors, radians.
fn distance(a: &[Quat], b: &[Quat]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(a, b)| {
            let angle = 2.0 * det::acos(a.dot(*b).abs().min(1.0));
            angle * angle
        })
        .sum::<f32>()
        .sqrt()
}

fn kernel(distance: f32, radius: f32) -> f32 {
    let t = distance / radius;
    det::exp(-t * t)
}

/// Solve `a x = b` for several right-hand sides (columns of `b`) by Gaussian elimination with
/// partial pivoting. `None` when singular.
fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<Vec<f64>>) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    for col in 0..n {
        // Ties keep the lowest row index (deterministic pivot, audit-v040 M6).
        let mut pivot = col;
        for (i, row) in a.iter().enumerate().skip(col + 1) {
            if row[col].abs() > a[pivot][col].abs() {
                pivot = i;
            }
        }
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        let (pivot_a, pivot_b) = (a[col].clone(), b[col].clone());
        for row in col + 1..n {
            let factor = a[row][col] / pivot_a[col];
            if factor == 0.0 {
                continue;
            }
            for (x, p) in a[row].iter_mut().zip(&pivot_a).skip(col) {
                *x -= factor * p;
            }
            for (x, p) in b[row].iter_mut().zip(&pivot_b) {
                *x -= factor * p;
            }
        }
    }
    for col in (0..n).rev() {
        let (head, tail) = b.split_at_mut(col + 1);
        for (k, cell) in head[col].iter_mut().enumerate() {
            let mut value = *cell;
            for (offset, row) in tail.iter().enumerate() {
                value -= a[col][col + 1 + offset] * row[k];
            }
            *cell = value / a[col][col];
        }
    }
    Some(b)
}

impl CompiledCorrective {
    /// Resolve `corrective`: joint names against `skeleton`, morph names through
    /// `morph_index`, and solve the coefficients.
    ///
    /// # Errors
    ///
    /// Every unresolved name and shape problem (rotation count, radius, no poses).
    pub fn compile(
        corrective: &PoseSpaceCorrective,
        skeleton: &Skeleton,
        morph_index: &dyn Fn(&str) -> Option<usize>,
    ) -> Result<Self, Vec<String>> {
        let name = &corrective.name;
        let mut problems = Vec::new();
        if corrective.poses.is_empty() {
            problems.push(format!("corrective {name:?}: needs at least one pose"));
        }
        if corrective.joints.is_empty() {
            problems.push(format!("corrective {name:?}: needs at least one joint"));
        }
        if !corrective.radius.is_finite() || corrective.radius <= 0.0 {
            problems.push(format!("corrective {name:?}: radius must be positive"));
        }
        // A non-finite weight, rotation or morph value would make the solved coefficients NaN
        // and add NaN to a morph every frame (audit corr-animation).
        if !corrective.weight.is_finite() {
            problems.push(format!("corrective {name:?}: weight must be finite"));
        }
        for pose in &corrective.poses {
            if !pose.rotations.iter().all(|e| e.is_finite())
                || !pose.morphs.iter().all(|(_, w)| w.is_finite())
            {
                problems.push(format!(
                    "corrective {name:?} pose {:?}: rotations and morph weights must be finite",
                    pose.name
                ));
            }
        }
        let mut joints = Vec::new();
        for joint in &corrective.joints {
            match skeleton.joint_index(joint) {
                Some(j) => joints.push(j as usize),
                None => problems.push(format!("corrective {name:?}: no joint {joint:?}")),
            }
        }
        let mut outputs: Vec<usize> = Vec::new();
        let mut samples: Vec<Vec<Quat>> = Vec::new();
        let mut targets: Vec<Vec<(usize, f32)>> = Vec::new();
        for pose in &corrective.poses {
            if pose.rotations.len() != corrective.joints.len() {
                problems.push(format!(
                    "corrective {name:?} pose {:?}: {} rotations for {} joints",
                    pose.name,
                    pose.rotations.len(),
                    corrective.joints.len()
                ));
                continue;
            }
            samples.push(
                pose.rotations
                    .iter()
                    .map(|e| det::quat_from_euler_xyz(e.x.to_radians(), e.y.to_radians(), e.z.to_radians()))
                    .collect(),
            );
            let mut target = Vec::new();
            for (morph, weight) in &pose.morphs {
                match morph_index(morph) {
                    Some(index) => {
                        if !outputs.contains(&index) {
                            outputs.push(index);
                        }
                        target.push((index, *weight));
                    }
                    None => problems.push(format!(
                        "corrective {name:?} pose {:?}: no morph target {morph:?}",
                        pose.name
                    )),
                }
            }
            targets.push(target);
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        // The bind pose: an implicit example with every weight zero.
        samples.push(vec![Quat::IDENTITY; joints.len()]);
        targets.push(Vec::new());
        let count = samples.len();
        let mut matrix = vec![vec![0.0f64; count]; count];
        for (i, row) in matrix.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = f64::from(kernel(distance(&samples[i], &samples[j]), corrective.radius));
            }
            row[i] += 1e-6;
        }
        let mut rhs = vec![vec![0.0f64; outputs.len()]; count];
        for (i, target) in targets.iter().enumerate() {
            for &(index, weight) in target {
                if let Some(column) = outputs.iter().position(|&o| o == index) {
                    rhs[i][column] += f64::from(weight);
                }
            }
        }
        let Some(solved) = solve(matrix, rhs) else {
            return Err(vec![format!(
                "corrective {name:?}: example poses are degenerate (two coincide with different weights?)"
            )]);
        };
        Ok(CompiledCorrective {
            name: corrective.name.clone(),
            bind: joints.iter().map(|&j| skeleton.bind_pose[j].rotation).collect(),
            joints,
            samples,
            outputs,
            coefficients: solved
                .into_iter()
                .map(|row| row.into_iter().map(|v| v as f32).collect())
                .collect(),
            radius: corrective.radius,
            weight: corrective.weight,
        })
    }

    /// Number of example poses (without the implicit bind example).
    #[must_use]
    pub fn pose_count(&self) -> usize {
        self.samples.len() - 1
    }

    /// Add the corrective's morph weights for `pose` to `morphs`. Writes each example's
    /// kernel activation into `activations` (cleared first; one per example).
    pub fn apply(&self, pose: &Pose, morphs: &mut [f32], activations: &mut Vec<f32>) {
        activations.clear();
        let current: Vec<Quat> = self
            .joints
            .iter()
            .zip(&self.bind)
            .map(|(&j, bind)| bind.inverse() * pose.rotations.get(j).copied().unwrap_or(Quat::IDENTITY))
            .collect();
        let phi: Vec<f32> = self
            .samples
            .iter()
            .map(|s| kernel(distance(&current, s), self.radius))
            .collect();
        for (column, &output) in self.outputs.iter().enumerate() {
            let value: f32 = phi
                .iter()
                .zip(&self.coefficients)
                .map(|(phi, coefficients)| phi * coefficients[column])
                .sum();
            if let Some(slot) = morphs.get_mut(output) {
                *slot += value * self.weight;
            }
        }
        activations.extend_from_slice(&phi[..phi.len() - 1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::LocalBind;

    fn arm() -> Skeleton {
        Skeleton::from_local_bind(
            vec!["root".into(), "elbow".into()],
            vec![None, Some(0)],
            vec![LocalBind::default(), LocalBind::default()],
        )
        .expect("valid")
    }

    fn index(name: &str) -> Option<usize> {
        ["bulge", "crease"].iter().position(|n| *n == name)
    }

    fn elbow() -> PoseSpaceCorrective {
        PoseSpaceCorrective {
            name: "elbow".into(),
            joints: vec!["elbow".into()],
            poses: vec![
                CorrectivePose {
                    name: "bent90".into(),
                    rotations: vec![Vec3::new(90.0, 0.0, 0.0)],
                    morphs: vec![("bulge".into(), 0.5)],
                },
                CorrectivePose {
                    name: "bent140".into(),
                    rotations: vec![Vec3::new(140.0, 0.0, 0.0)],
                    morphs: vec![("bulge".into(), 1.0), ("crease".into(), 0.8)],
                },
            ],
            radius: 0.8,
            weight: 1.0,
        }
    }

    fn bent(degrees: f32) -> Pose {
        let skeleton = arm();
        let mut pose = skeleton.bind_pose_pose();
        pose.rotations[1] = Quat::from_rotation_x(degrees.to_radians());
        pose
    }

    fn evaluate(compiled: &CompiledCorrective, pose: &Pose) -> [f32; 2] {
        let mut morphs = [0.0f32; 2];
        compiled.apply(pose, &mut morphs, &mut Vec::new());
        morphs
    }

    /// Audit (corr-animation): non-finite data compiled and produced NaN morph weights.
    #[test]
    fn non_finite_data_is_rejected_at_compile() {
        let mut nan_morph = elbow();
        nan_morph.poses[0].morphs[0].1 = f32::NAN;
        let mut nan_weight = elbow();
        nan_weight.weight = f32::NAN;
        let mut inf_radius = elbow();
        inf_radius.radius = f32::INFINITY;
        let mut inf_rotation = elbow();
        inf_rotation.poses[0].rotations[0].x = f32::INFINITY;
        for bad in [nan_morph, nan_weight, inf_radius, inf_rotation] {
            assert!(CompiledCorrective::compile(&bad, &arm(), &index).is_err());
        }
    }

    #[test]
    fn examples_reproduce_their_weights_and_rest_is_zero() {
        let compiled = CompiledCorrective::compile(&elbow(), &arm(), &index).expect("compiles");
        assert_eq!(compiled.pose_count(), 2);
        let at90 = evaluate(&compiled, &bent(90.0));
        let at140 = evaluate(&compiled, &bent(140.0));
        let rest = evaluate(&compiled, &bent(0.0));
        assert!((at90[0] - 0.5).abs() < 1e-3 && at90[1].abs() < 1e-3, "{at90:?}");
        assert!(
            (at140[0] - 1.0).abs() < 1e-3 && (at140[1] - 0.8).abs() < 1e-3,
            "{at140:?}"
        );
        assert!(rest[0].abs() < 1e-3 && rest[1].abs() < 1e-3, "{rest:?}");
    }

    #[test]
    fn weights_interpolate_smoothly_between_examples() {
        let compiled = CompiledCorrective::compile(&elbow(), &arm(), &index).expect("compiles");
        let mut previous = 0.0f32;
        for degrees in (0..=140).step_by(5) {
            let value = evaluate(&compiled, &bent(degrees as f32))[0];
            assert!(value.is_finite());
            assert!(
                value >= previous - 0.02,
                "bulge dips at {degrees}: {value} < {previous}"
            );
            assert!((previous - value).abs() < 0.2, "jump at {degrees}");
            previous = value;
        }
    }

    #[test]
    fn output_adds_to_the_bank_and_scales_by_weight() {
        let mut half = elbow();
        half.weight = 0.5;
        let compiled = CompiledCorrective::compile(&half, &arm(), &index).expect("compiles");
        let mut morphs = [0.25f32, 0.0];
        let mut activations = Vec::new();
        compiled.apply(&bent(140.0), &mut morphs, &mut activations);
        assert!((morphs[0] - 0.75).abs() < 1e-3, "{morphs:?}");
        assert_eq!(activations.len(), 2);
        assert!(activations[1] > 0.99 && activations[0] < activations[1]);
    }

    #[test]
    fn a_joint_the_corrective_ignores_does_not_matter() {
        let compiled = CompiledCorrective::compile(&elbow(), &arm(), &index).expect("compiles");
        let mut pose = bent(140.0);
        pose.rotations[0] = Quat::from_rotation_y(1.0);
        assert!((evaluate(&compiled, &pose)[0] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn problems_are_all_reported() {
        let mut bad = elbow();
        bad.joints = vec!["wrist".into()];
        bad.poses[0].morphs.push(("nope".into(), 1.0));
        bad.poses[1].rotations.clear();
        bad.radius = 0.0;
        let problems = CompiledCorrective::compile(&bad, &arm(), &index).expect_err("fails");
        assert_eq!(problems.len(), 4, "{problems:?}");
    }

    #[test]
    fn pivot_ties_keep_the_lowest_row() {
        // Column 0 has an exact |value| tie between rows 0 and 1. The deterministic
        // pivot keeps the lowest row (the previous max_by kept the last maximum),
        // which selects a different elimination order; the pinned f64 bits lock the
        // lowest-row rule (audit-v040 M6).
        let a = vec![vec![0.9, 1.0, 0.5], vec![0.9, 0.7, 0.25], vec![0.5, 0.25, 1.0]];
        let b = vec![vec![1.0], vec![2.0], vec![3.0]];
        let x = solve(a, b).expect("non-singular");
        assert_eq!(x[0][0], 5.355450236966823);
        assert_eq!(x[1][0], -4.5497630331753545);
        assert_eq!(x[2][0], 1.459715639810427);
    }

    #[test]
    fn coinciding_examples_with_different_weights_are_rejected_or_blended() {
        let mut twin = elbow();
        twin.poses[1].rotations = twin.poses[0].rotations.clone();
        // The ridge keeps the solve finite; the result at the shared pose averages the two.
        let compiled = CompiledCorrective::compile(&twin, &arm(), &index).expect("ridge keeps it solvable");
        let value = evaluate(&compiled, &bent(90.0))[0];
        assert!(value.is_finite() && value > 0.4 && value < 1.1, "{value}");
    }
}
