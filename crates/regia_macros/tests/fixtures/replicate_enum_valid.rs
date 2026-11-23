include!("support.rs");
use regia_macros::Replicate as MakeReplicate;

// An enum exercises the variant codec paths (tag, per-variant fields, codecs on enum fields) that
// the struct fixture does not; the generated `ReplicateCodec` impl omits the delta methods and
// relies on the trait's defaults.
#[derive(Clone, MakeReplicate)]
enum r#event {
    Idle,
    Moved {
        #[replicate(quantize(min = -10.0, max = 10.0, bits = 8))]
        x: f32,
        #[replicate(angle(bits = 12))]
        yaw: f32,
    },
    Said(
        #[replicate(max_len = 16)]
        String,
    ),
}

#[test]
fn generated_enum_codec_impl_type_checks() {
    fn assert_codec<T: codec::ReplicateCodec>() {}
    assert_codec::<r#event>();
}
