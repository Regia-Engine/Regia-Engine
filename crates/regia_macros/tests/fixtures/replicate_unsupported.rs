include!("support.rs");
use regia_macros::Replicate;
#[derive(Clone, Replicate)]
struct Unsupported {
    #[replicate(spin)]
    field: u8,
}
