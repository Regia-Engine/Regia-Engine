include!("support.rs");
use regia_macros::Replicate;
#[derive(Clone, Replicate)]
#[replicate(skip)]
struct Misplaced {
    field: u8,
}
