include!("support.rs");
use regia_macros::NetMessage;
#[derive(Clone, NetMessage)]
#[net(direction = "to_server", version = 1, version = 2)]
struct Duplicate {
    field: u8,
}
