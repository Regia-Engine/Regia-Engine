include!("support.rs");
use regia_macros::NetMessage as MakeMessage;

#[expect(
    non_camel_case_types,
    reason = "this fixture exercises raw identifier derives"
)]
#[derive(Clone, MakeMessage)]
#[net(direction = "to_server", channel = "unreliable", rate = "2/5", version = 3)]
struct r#input<T: Clone + PartialEq + Send + Sync + 'static> {
    value: T,
}

#[test]
fn generated_message_and_codec_impls_type_check() {
    fn assert_message<T: message::ToServerMessage>() {}
    assert_message::<r#input<u8>>();
    assert_eq!(<r#input<u8> as message::NetMessage>::VERSION, 3);
    // The channel and rate the attributes set: a swapped mapping or transposed count/per_ticks
    // still type-checks, so only an asserted value catches it.
    assert_eq!(
        <r#input<u8> as message::NetMessage>::CHANNEL,
        message::Channel::Unreliable
    );
    assert_eq!(
        <r#input<u8> as message::NetMessage>::RATE_LIMIT,
        Some(settings::RateLimit {
            count: 2,
            per_ticks: 5
        })
    );
}
