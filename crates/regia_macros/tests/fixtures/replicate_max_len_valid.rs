include!("support.rs");
use regia_macros::Replicate;

// Control fixture for `replicate_bad_number`: a *valid* `max_len` on a `String` field must
// compile, so the bad-number fixture can only pass because the number is bad (against the old
// blanket stand-in impls this control did not compile either, which made the negative test
// vacuous).
#[derive(Clone, Replicate)]
struct GoodNumber {
    #[replicate(max_len = 5)]
    field: String,
}

#[test]
fn valid_max_len_control_compiles() {
    fn assert_codec<T: codec::ReplicateCodec>() {}
    assert_codec::<GoodNumber>();
}
