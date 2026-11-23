include!("support.rs");
use regia_macros::NetMessage as MakeMessage;

// An enum message exercises the same generated codec as `replicate_enum_valid` plus the message
// constants; the marker bound checks the direction.
#[derive(Clone, MakeMessage)]
#[net(direction = "broadcast", channel = "reliable_unordered")]
enum r#notice {
    Ping,
    Score {
        value: u8,
    },
}

#[test]
fn generated_enum_message_impls_type_check() {
    fn assert_message<T: message::BroadcastMessage>() {}
    assert_message::<r#notice>();
    assert_eq!(
        <r#notice as message::NetMessage>::CHANNEL,
        message::Channel::ReliableUnordered
    );
}
