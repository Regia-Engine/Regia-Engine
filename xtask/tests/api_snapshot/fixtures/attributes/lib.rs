//! Conditional documentation and deprecation are not unconditional policy waivers.

#[doc(hidden)]
pub fn hidden() {}
#[cfg_attr(feature = "extra", doc(hidden))]
pub fn sometimes_hidden() {}
#[cfg_attr(
    all(feature = "extra", not(test)),
    deprecated(since = "0.9.0", note = "use fresh")
)]
pub fn sometimes_deprecated() {}
#[deprecated(since = "0.9.0", note = "use fresh")]
pub fn old() {}
pub fn fresh() {}

#[cfg_attr(feature = "extra", doc(hidden))]
pub mod conditional_module {
    pub fn visible_without_extra() {}
}
