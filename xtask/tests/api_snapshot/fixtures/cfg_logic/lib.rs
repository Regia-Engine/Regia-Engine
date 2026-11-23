//! Nested configuration fixtures: no target-specific assumptions.

#[cfg(test)]
pub fn test_direct() {}
#[cfg(all(test))]
pub fn test_singleton_all() {}
#[cfg(all(test, feature = "extra"))]
pub fn test_first() {}
#[cfg(all(feature = "extra", test))]
pub fn test_last() {}
#[cfg(all(feature = "extra", all(test)))]
pub fn test_nested_all() {}
#[cfg(any(all(test, feature = "extra"), all(feature = "other", test)))]
pub fn test_any_branches() {}
#[cfg(not(not(test)))]
pub fn test_double_not() {}
#[cfg(not(any(not(test), feature = "extra")))]
pub fn test_demorgan() {}
#[cfg(all())]
#[cfg(test)]
pub fn test_multiple_attrs() {}

#[cfg(any(test, feature = "extra"))]
pub fn conditional_any() {}
#[cfg(not(all(not(test), not(feature = "extra"))))]
pub fn conditional_demorgan() {}
#[cfg(not(test))]
pub fn production() {}
#[cfg(any())]
pub fn never() {}
#[cfg(not(all(test, feature = "extra")))]
pub fn conditional_negated_all() {}
#[cfg(not(any(test, feature = "extra")))]
pub fn conditional_negated_any() {}
#[cfg(feature = "test")]
pub fn feature_named_test() {}

pub struct Holder;
impl Holder {
    #[cfg(all(feature = "extra", test))]
    pub fn test_method() {}
    #[cfg(any(test, feature = "extra"))]
    pub fn conditional_method() {}
}
#[non_exhaustive]
pub enum Choice {
    Production,
    #[cfg(all(feature = "extra", test))]
    TestVariant,
}
pub trait Hook {
    #[cfg(all(feature = "extra", test))]
    fn test_required();
    #[cfg(all(feature = "extra", test))]
    fn test_default() {}
    #[cfg(all(feature = "extra", test))]
    const TEST_CONST: u8 = 1;
    #[cfg(all(feature = "extra", test))]
    type TestType;
}

#[cfg(all(feature = "extra", test))]
pub mod test_module {
    pub fn inside() {}
}
mod implementation {
    pub fn test_target() {}
}
#[cfg(all(feature = "extra", test))]
pub use implementation::test_target as test_export;
#[cfg(all(feature = "extra", test))]
impl Holder {
    pub fn test_impl() {}
}
#[cfg(all(feature = "extra", test))]
#[macro_export]
macro_rules! test_macro {
    () => {};
}

#[cfg(all(test, any()))]
pub fn test_unreachable_first() {}
#[cfg(all(any(), test))]
pub fn test_unreachable_last() {}

#[cfg(all(test, true))]
pub fn test_literal_true_first() {}
#[cfg(all(true, test))]
pub fn test_literal_true_last() {}
#[cfg(any(false, test))]
pub fn test_literal_false_first() {}
#[cfg(any(test, false))]
pub fn test_literal_false_last() {}
#[cfg(any(all(test, true), false))]
pub fn test_literal_nested() {}
#[cfg(all(test, false))]
pub fn test_literal_unreachable() {}
#[cfg(any(true, test))]
pub fn production_literal_true() {}
#[cfg(false)]
pub fn never_literal() {}
