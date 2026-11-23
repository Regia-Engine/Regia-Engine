include!("support.rs");
use regia_macros::Replicate as MakeReplicate;

#[derive(Clone, MakeReplicate)]
struct r#state<T: Clone + PartialEq> {
    value: T,
    #[replicate(skip)]
    local: u32,
    #[replicate(angle(bits = (8)))]
    heading: f32,
}

#[test]
fn generated_codec_impl_type_checks() {
    fn assert_codec<T: codec::ReplicateCodec>() {}
    assert_codec::<r#state<u8>>();
}

#[test]
fn skipped_fields_keep_the_local_value() {
    use codec::ReplicateCodec;
    let mut received = r#state {
        value: 1u8,
        local: 0,
        heading: 0.5,
    };
    received.keep_local(&r#state {
        value: 2u8,
        local: 7,
        heading: 1.5,
    });
    assert!(received.value == 1 && received.local == 7 && received.heading == 0.5);
}
