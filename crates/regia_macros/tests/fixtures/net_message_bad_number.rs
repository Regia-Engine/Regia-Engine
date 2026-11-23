include!("support.rs");
use regia_macros::NetMessage;
#[derive(Clone, NetMessage)]
#[net(direction = "to_server", version = 0)]
struct BadVersion {
    field: u8,
}
