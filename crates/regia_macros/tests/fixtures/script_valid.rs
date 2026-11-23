include!("support.rs");
use regia_macros::Script as MakeScript;
use script::{ReflectScript, Script};

#[expect(
    non_camel_case_types,
    reason = "this fixture exercises raw identifier derives"
)]
#[derive(Default, MakeScript)]
#[script(order = -7)]
struct r#move<T: Default + Clone + 'static>
where
    T: Send + Sync,
{
    value: T,
}
// The real `Script` and `ReflectScript` require `Send + Sync + 'static`, so the impl (and the
// generated one) only type-check when `T` is `Send + Sync`; a fixture without the `Sync` bounds
// would pass here but fail against the real traits.
impl<T: Default + Clone + Send + Sync + 'static> Script for r#move<T> {}
// The derive's reflect accessor needs `PartialReflect` (the real type derives `Reflect`).
impl<T: Default + Clone + Send + Sync + 'static> script::PartialReflect for r#move<T> {}

#[test]
fn generated_script_impl_type_checks() {
    assert_eq!(<r#move<u8> as ReflectScript>::order(), -7);
    let _: Box<dyn Script> = <r#move<u8> as ReflectScript>::construct();
}
