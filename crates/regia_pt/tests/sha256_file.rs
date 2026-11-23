// SPDX-License-Identifier: MPL-2.0
//! `regia_pt::sha256_file_hex` against published and independently computed digests: the FIPS 180 one-million-`a`
//! vector, the padding edge cases (lengths around the 55/56 and 63/64 byte block boundaries, where a hasher that
//! pads wrongly differs) and files that cross the 1 MiB read size. The digests below were computed with a second
//! implementation (Python `hashlib`) from the same byte pattern. CPU only, no golden.

use regia_pt::sha256_file_hex;

/// The test pattern: `(i·7 + i/251) mod 256`.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7 + i / 251) % 256) as u8).collect()
}

fn digest_of(bytes: &[u8], dir: &std::path::Path, name: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write");
    sha256_file_hex(&path).expect("hash")
}

#[test]
fn published_and_cross_checked_digests() {
    let dir = regia_test_utils::unique_temp_dir("pt_sha256_file");
    // FIPS 180-2 appendix B.3: one million 'a'.
    assert_eq!(
        digest_of(&vec![b'a'; 1_000_000], &dir, "million_a"),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
    for (n, want) in [
        (
            0,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            1,
            "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
        ),
        (
            55,
            "576a1bf8d4478657e6dc4af9398544765c2a92cde28478b019235cfed315fc09",
        ),
        (
            56,
            "9b20501dfd1d99161c257950f3444f3e49230c351c5c8e0943ef369f85f5205d",
        ),
        (
            57,
            "a5534a9d0694341dfde5b1bb3d8addcb38bd46f44a629dc97b3f8fb2721b0720",
        ),
        (
            63,
            "30b345906b493f06f69444b6521113511c242f30e29840462950035043682f1e",
        ),
        (
            64,
            "d8bc63b4fc1156e5e7d95a418b9bf54cd3174bedbc2db40f74895349b229b3c0",
        ),
        (
            65,
            "1ee23b0fbcaecc1aff4a9e8f1645f35ab2c8e13609cd73b68df8b5e3f63ce073",
        ),
        (
            119,
            "7a6589821178918ca8d9edaba5abfc1e9b2669564f4469b66885379c1530b2c8",
        ),
        (
            120,
            "655250427d56b1b0eeb8497d21428704273458a01772d6881b65c0abac0f8a98",
        ),
        (
            128,
            "54c9eb041badfd7064645067b107661fed6113197ce2dd066ba69618abd3732f",
        ),
        (
            1000,
            "1d9d471927b6f0cbc77aed8969dfc6ef43ff35fdcdd8aa0032630c0063ff95a5",
        ),
        // Around the 1 MiB read size: exactly one read, one read plus a byte, and several reads.
        (
            1_048_576,
            "07f4465ef6fe98070beaf8d8d01454b5d11f6cd4ff86a139d92cd031b46ddfdc",
        ),
        (
            1_048_577,
            "7d129a470ea3dee543fdd2b4423cb950a5037750f9427a1b4b6488a7b7ff574c",
        ),
        (
            2_500_000,
            "e9b113bf90c4abf8d10702dbe2ac9bdffc10928331a86b1a4476b0cedafa554d",
        ),
    ] {
        assert_eq!(digest_of(&pattern(n), &dir, &format!("p{n}")), want, "{n} bytes");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_files_and_directories_are_io_errors() {
    let dir = regia_test_utils::unique_temp_dir("pt_sha256_missing");
    assert!(sha256_file_hex(&dir.join("absent.bin")).is_err());
    // A directory opens on Linux but cannot be read as a file; either way it is an error, not a hash.
    assert!(sha256_file_hex(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
