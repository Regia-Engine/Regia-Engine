include!("support.rs");
use regia_macros::NetMessage;
#[derive(Clone, NetMessage)]
#[net(direction = "to_server")]
struct Unsupported {
    #[replicate(spin)]
    field: u8,
}
