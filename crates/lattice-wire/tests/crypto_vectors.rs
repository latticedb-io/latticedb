//! Crypto primitive verification against published vectors.

use lattice_wire::crypto::{base64_decode, base64_encode, hmac_sha256, pbkdf2_hmac_sha256, sha256};

#[test]
fn sha256_known_vectors() {
    // FIPS 180-4 / NIST test vectors.
    let e = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert_eq!(lattice_wire::crypto::hex(&sha256(b"")), e);
    let e2 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(lattice_wire::crypto::hex(&sha256(b"abc")), e2);
    let e3 = "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1";
    assert_eq!(
        lattice_wire::crypto::hex(&sha256(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        e3
    );
}

#[test]
fn hmac_sha256_rfc4231_vectors() {
    // RFC 4231 test case 1: key = 0x0b × 20, data = "Hi There"
    let key = [0x0bu8; 20];
    let sig = hmac_sha256(&key, b"Hi There");
    assert_eq!(
        lattice_wire::crypto::hex(&sig),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    // RFC 4231 test case 2: key = "Jefe", data = "what do ya want for nothing?"
    let sig = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    assert_eq!(
        lattice_wire::crypto::hex(&sig),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn sha256_multi_block() {
    // > 64 bytes: exercises the multi-block path.
    let e = "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1";
    assert_eq!(lattice_wire::crypto::hex(&sha256(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu")), e);
}

#[test]
fn pbkdf2_known_vectors() {
    // PBKDF2-HMAC-SHA256 canonical vectors (P="password", S="salt"):
    // the c=2 vector is ae4d0c95... — commonly miscited as c=4096
    // (c=4096 is c5e478d5..., verified against hashlib.pbkdf2_hmac).
    let cases = [
        (
            1u32,
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b",
        ),
        (
            2u32,
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43",
        ),
        (
            4096u32,
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a",
        ),
    ];
    for (iterations, expected) in cases {
        let dk = pbkdf2_hmac_sha256(b"password", b"salt", iterations, 32);
        assert_eq!(lattice_wire::crypto::hex(&dk), expected, "c={iterations}");
    }
}

#[test]
fn base64_roundtrip() {
    for data in [
        b"".as_slice(),
        b"f",
        b"fo",
        b"foo",
        b"foob",
        b"fooba",
        b"foobar",
    ] {
        let enc = base64_encode(data);
        let dec = base64_decode(&enc).expect("valid base64");
        assert_eq!(dec, data, "round-trip {data:?}");
    }
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    assert!(base64_decode("!!!!").is_none(), "invalid chars");
    assert!(base64_decode("Zm9").is_none(), "not multiple of 4");
}
