include!("support.rs");
use regia_macros::Script;
#[derive(Default, Script)]
#[script(order = 1)]
#[script(order = 2)]
struct Duplicate;
