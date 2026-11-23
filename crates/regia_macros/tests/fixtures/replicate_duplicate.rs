include!("support.rs");
use regia_macros::Replicate;
#[derive(Clone, Replicate)]
struct Duplicate {
    #[replicate(skip)]
    #[replicate(skip)]
    field: u8,
}
