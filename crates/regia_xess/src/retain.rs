// SPDX-License-Identifier: MPL-2.0
//! Whether an evaluate frame can reuse the retention bind group.
//!
//! The empty compute pass must still bind the group every frame so wgpu tracks the
//! views until that submission finishes. Creating the group is only required when the
//! view objects or the depth-versus-float layout changed.

/// `true` when the cached group already holds these exact view objects and layout.
#[must_use]
pub(crate) fn reuse_retention(same_depth_layout: bool, same_views: [bool; 4]) -> bool {
    same_depth_layout && same_views == [true; 4]
}

#[cfg(test)]
mod tests {
    use super::reuse_retention;

    #[test]
    fn stable_views_reuse_the_retention_group() {
        assert!(
            reuse_retention(true, [true, true, true, true]),
            "identical color, motion, depth and output views must not allocate a bind group"
        );
    }

    #[test]
    fn a_new_view_or_depth_layout_allocates() {
        assert!(!reuse_retention(false, [true, true, true, true]));
        assert!(!reuse_retention(true, [false, true, true, true]));
        assert!(!reuse_retention(true, [true, false, true, true]));
        assert!(!reuse_retention(true, [true, true, false, true]));
        assert!(!reuse_retention(true, [true, true, true, false]));
        assert!(!reuse_retention(false, [false, false, false, false]));
    }
}
