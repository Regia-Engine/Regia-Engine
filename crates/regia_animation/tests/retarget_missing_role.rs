// SPDX-License-Identifier: MPL-2.0
//! Focused regression coverage for retarget missing-role reporting and clip sample limits.

use regia_animation::clip::AnimationClip;
use regia_animation::retarget::{HumanoidBone, RetargetProfile, Retargeter};
use regia_animation::retarget_check::CHECK_MAX_FRAMES;
use regia_animation::skeleton::{LocalBind, Skeleton};

fn skeleton(names: &[&str]) -> Skeleton {
    let count = names.len();
    Skeleton::from_local_bind(
        names.iter().map(|name| (*name).to_string()).collect(),
        (0..count)
            .map(|index| index.checked_sub(1).map(|parent| parent as u32))
            .collect(),
        vec![LocalBind::default(); count],
    )
    .expect("valid synthetic skeleton")
}

fn profile(roles: &[(HumanoidBone, &str)]) -> RetargetProfile {
    RetargetProfile {
        roles: roles
            .iter()
            .map(|(role, joint)| (role.name().to_string(), (*joint).to_string()))
            .collect(),
    }
}

#[test]
fn reports_required_role_missing_when_hips_are_mapped() {
    let source = skeleton(&["src_hips", "src_head"]);
    let target = skeleton(&["dst_hips"]);
    let source_profile = profile(&[(HumanoidBone::Hips, "src_hips"), (HumanoidBone::Head, "src_head")]);
    let target_profile = profile(&[(HumanoidBone::Hips, "dst_hips")]);
    let retargeter =
        Retargeter::new(&source, &source_profile, &target, &target_profile).expect("hips are a common role");

    let report = retargeter.check(None);
    let role_name = HumanoidBone::Head.name();
    assert!(report.missing_humanoid_roles.iter().any(|role| role == role_name));
    assert!(
        report
            .issues
            .iter()
            .any(|issue| { issue.code == "missing_role" && issue.detail.contains(role_name) })
    );
}

#[test]
fn does_not_report_missing_roles_when_hips_are_unmapped() {
    let source = skeleton(&["src_head"]);
    let target = skeleton(&["dst_head"]);
    let head = [(HumanoidBone::Head, "src_head")];
    let source_profile = profile(&head);
    let target_profile = profile(&[(HumanoidBone::Head, "dst_head")]);
    let retargeter = Retargeter::new(&source, &source_profile, &target, &target_profile)
        .expect("head is a common required role");

    let report = retargeter.check(None);
    assert!(report.missing_humanoid_roles.is_empty());
    assert!(!report.issues.iter().any(|issue| issue.code == "missing_role"));
}

#[test]
fn clip_check_caps_long_clip_samples_and_keeps_short_clip_below_cap() {
    let source = skeleton(&["src_hips"]);
    let target = skeleton(&["dst_hips"]);
    let source_profile = profile(&[(HumanoidBone::Hips, "src_hips")]);
    let target_profile = profile(&[(HumanoidBone::Hips, "dst_hips")]);
    let retargeter =
        Retargeter::new(&source, &source_profile, &target, &target_profile).expect("hips are a common role");

    let long_clip = AnimationClip::empty("long", 21.0);
    let long_report = retargeter.check(Some(&long_clip));
    let long_check = long_report.clip.expect("clip check returned");
    assert_eq!(long_check.frames, CHECK_MAX_FRAMES);

    let short_clip = AnimationClip::empty("short", 1.0);
    let short_report = retargeter.check(Some(&short_clip));
    let short_check = short_report.clip.expect("clip check returned");
    assert!(short_check.frames < CHECK_MAX_FRAMES);
}
