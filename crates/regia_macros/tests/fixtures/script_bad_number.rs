include!("support.rs");
use regia_macros::Script;
#[derive(Default, Script)]
#[script(order = 99999999999)]
struct BadOrder;
