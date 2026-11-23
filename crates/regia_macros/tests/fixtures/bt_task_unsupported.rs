include!("support.rs");
use regia_macros::BtTask;
#[derive(BtTask)]
union Unsupported {
    number: u32,
    bits: f32,
}
