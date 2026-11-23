include!("support.rs");
use regia_macros::Replicate;
#[derive(Clone, Replicate)]
struct BadNumber {
    #[replicate(max_len = "many")]
    field: String,
}
