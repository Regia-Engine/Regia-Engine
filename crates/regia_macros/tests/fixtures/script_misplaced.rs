include!("support.rs");
use regia_macros::Script;
#[derive(Default, Script)]
struct Misplaced {
    #[script(order = 1)]
    field: u8,
}
