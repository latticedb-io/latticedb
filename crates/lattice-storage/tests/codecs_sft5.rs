//! T-108 acceptance: codec registry — SFT-5.1 chain round-trip (D1: decode
//! (encode(x)) == x byte-exact, deterministic double-encode), SFT-5.2
//! override honored, SFT-5.3 ratio sanity (informational bound).

use lattice_storage::codecs::{
    BITPACK, CodecChain, ColumnData, ColumnType, DELTA_OF_DELTA, DICTIONARY, IDENTITY, NO_FRAME,
    decode_value, default_chain, encode_value, f64_bits_eq,
};

/// Deterministic pseudo-random generator (xorshift64*) — seeded, no
/// external fuzz dependency; same seed ⇒ same inputs on every machine.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn roundtrip(codec: u8, col: &ColumnData) {
    let count = col.len();
    let encoded = encode_value(codec, col).expect("encode must succeed");
    // Determinism: encoding the same input twice yields identical bytes (D1).
    let encoded_again = encode_value(codec, col).expect("encode must succeed");
    assert_eq!(
        encoded, encoded_again,
        "codec {codec:#04x}: encode must be deterministic"
    );
    let decoded = decode_value(codec, col.ty(), &encoded, count)
        .unwrap_or_else(|e| panic!("codec {codec:#04x} decode failed: {e}"));
    match (col, &decoded) {
        (ColumnData::Bool(a), ColumnData::Bool(b)) => assert_eq!(a, b, "codec {codec:#04x}"),
        (ColumnData::I64(a), ColumnData::I64(b)) => assert_eq!(a, b, "codec {codec:#04x}"),
        (ColumnData::F64(a), ColumnData::F64(b)) => {
            assert!(
                f64_bits_eq(a, b),
                "codec {codec:#04x}: bit-exact f64 (NaN-safe)"
            );
        }
        (ColumnData::Str(a), ColumnData::Str(b)) => assert_eq!(a, b, "codec {codec:#04x}"),
        (ColumnData::Bytes(a), ColumnData::Bytes(b)) => assert_eq!(a, b, "codec {codec:#04x}"),
        (ColumnData::Enum(a), ColumnData::Enum(b)) => assert_eq!(a, b, "codec {codec:#04x}"),
        (ColumnData::Dec(a), ColumnData::Dec(b)) => assert_eq!(a, b, "codec {codec:#04x}"),
        _ => panic!("type mismatch in roundtrip"),
    }
}

/// SFT-5.1 (fuzz): every codec × every compatible type, random + edge inputs.
#[test]
fn sft_5_1_chain_roundtrip_all_codecs() {
    let mut rng = Rng::new(0xD1_5EED);

    // Identity: every type, random + edge inputs.
    let i64_edge = vec![i64::MIN, 0, i64::MAX, -1, 42];
    let f64_edge = vec![
        0.0,
        -0.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        1.5e300,
        -1e-300,
    ];
    let nan_pair = vec![f64::NAN, f64::NAN, 1.0];
    let empty_i = Vec::<i64>::new();
    roundtrip(IDENTITY, &ColumnData::Bool(vec![true, false, true]));
    roundtrip(IDENTITY, &ColumnData::I64(i64_edge));
    roundtrip(IDENTITY, &ColumnData::I64(empty_i));
    roundtrip(IDENTITY, &ColumnData::F64(f64_edge));
    roundtrip(IDENTITY, &ColumnData::F64(nan_pair));
    roundtrip(
        IDENTITY,
        &ColumnData::Str(vec!["".into(), "hello".into(), "unicode: чао".into()]),
    );
    roundtrip(
        IDENTITY,
        &ColumnData::Bytes(vec![vec![], vec![0u8; 10], vec![255u8; 3]]),
    );
    roundtrip(IDENTITY, &ColumnData::Enum(vec![0, 255, 7, 0]));
    roundtrip(
        IDENTITY,
        &ColumnData::Dec(vec![i128::MIN, 0, i128::MAX, 30, -1]),
    );

    // Random inputs per codec.
    let bools: Vec<bool> = (0..500)
        .map(|i| rng.next() & 1 == 0 || i % 7 == 0)
        .collect();
    let runheavy_bools: Vec<bool> = (0..300)
        .flat_map(|_| vec![true, true, true, false, false])
        .collect();
    for col in [ColumnData::Bool(bools), ColumnData::Bool(runheavy_bools)] {
        roundtrip(BITPACK, &col);
        roundtrip(0x01, &col); // RLE
        roundtrip(IDENTITY, &col);
    }

    let enums: Vec<u8> = (0..400).map(|_| (rng.next() % 4) as u8).collect();
    let enums_runs: Vec<u8> = (0..200).flat_map(|_| vec![3u8, 3, 3, 9]).collect();
    for col in [
        ColumnData::Enum(enums),
        ColumnData::Enum(enums_runs),
        ColumnData::Enum(vec![]),
    ] {
        roundtrip(DICTIONARY, &col);
        roundtrip(0x01, &col);
        roundtrip(IDENTITY, &col);
    }

    let strs: Vec<Box<str>> = (0..300)
        .map(|i| match rng.next() % 3 {
            0 => "alpha".into(),
            1 => format!("id-{i}").into_boxed_str(),
            _ => "".into(),
        })
        .collect();
    roundtrip(DICTIONARY, &ColumnData::Str(strs));
    roundtrip(IDENTITY, &ColumnData::Str(vec!["solo".into()]));

    // Delta + delta-of-delta: monotone, constant, wrapping extremes.
    let monotone: Vec<i64> = (0..2000i64)
        .map(|i| 1_700_000_000_000_000 + i * 1_000)
        .collect();
    let constant = vec![7i64; 100];
    let extremes = vec![i64::MIN, i64::MAX, i64::MIN + 1, -1, 0, i64::MAX];
    for col in [
        ColumnData::I64(monotone),
        ColumnData::I64(constant),
        ColumnData::I64(extremes),
        ColumnData::I64(vec![]),
    ] {
        roundtrip(0x03, &col);
        roundtrip(DELTA_OF_DELTA, &col);
        roundtrip(IDENTITY, &col);
    }

    // Gorilla: smooth series, constant series, random bits, single value.
    let smooth: Vec<f64> = (0..1000).map(|i| 20.0 + (i as f64) * 0.125).collect();
    let gaussish: Vec<f64> = (0..500)
        .map(|_i| f64::from_bits((rng.next() >> 12) | 0x4000_0000_0000_0000) - 2.0)
        .collect();
    for col in [
        ColumnData::F64(smooth),
        ColumnData::F64(vec![1.5; 300]),
        ColumnData::F64(gaussish),
        ColumnData::F64(vec![f64::NEG_INFINITY]),
    ] {
        roundtrip(0x05, &col);
        roundtrip(IDENTITY, &col);
    }

    // Decimal via identity only (registered default).
    roundtrip(
        IDENTITY,
        &ColumnData::Dec(vec![0, 1, -1, 123456789012345678901234567890]),
    );
}

/// SFT-5.2: an AttributeDef-pinned override (identity) is honored even when
/// the default chain (dictionary) would be smaller.
#[test]
fn sft_5_2_override_honored() {
    let col = ColumnData::Enum(vec![1, 1, 1, 2, 2, 1, 3, 3, 3, 3]);

    let default = default_chain(ColumnType::Enum);
    assert_eq!(default.value, DICTIONARY, "enum default is dictionary");
    let default_bytes = encode_value(default.value, &col).unwrap();

    let pinned = CodecChain::pure(IDENTITY);
    assert_eq!(
        pinned.frame, NO_FRAME,
        "override ships without a frame stage"
    );
    let pinned_bytes = encode_value(pinned.value, &col).unwrap();

    assert!(
        pinned_bytes.len() > default_bytes.len(),
        "fixture sanity: identity ({}) must be larger than dictionary ({}) for this column",
        pinned_bytes.len(),
        default_bytes.len()
    );

    // Both chains round-trip; the pinned chain's bytes differ from default's.
    let via_pinned =
        decode_value(pinned.value, ColumnType::Enum, &pinned_bytes, col.len()).unwrap();
    let via_default =
        decode_value(default.value, ColumnType::Enum, &default_bytes, col.len()).unwrap();
    assert_eq!(via_pinned, col);
    assert_eq!(via_default, col);
    assert_ne!(
        pinned_bytes, default_bytes,
        "the override must produce its own bytes"
    );

    // Chain header round-trip: decoders honor the recorded chain.
    let hdr = pinned.to_bytes();
    let rebuilt = CodecChain::from_bytes(&hdr).expect("2-byte chain header parses");
    assert_eq!(rebuilt, pinned);
}

/// SFT-5.3 (informational): 1M monotone timestamps via delta-of-delta stay
/// under 2 bytes/cell average.
#[test]
fn sft_5_3_ratio_sanity_monotone_timestamps() {
    const N: usize = 1_000_000;
    let mut col = Vec::with_capacity(N);
    let mut t: i64 = 1_700_000_000_000_000; // µs
    for i in 0..N {
        // Deterministic jitter: mostly steady 1 kHz with occasional gaps.
        let jitter = ((i % 997) as i64) * 3;
        col.push(t);
        t += 1_000 + jitter;
    }
    let encoded = encode_value(DELTA_OF_DELTA, &ColumnData::I64(col)).expect("encode");
    let bytes_per_cell = encoded.len() as f64 / N as f64;
    assert!(
        bytes_per_cell <= 2.0,
        "delta-of-delta on monotone timestamps: {bytes_per_cell:.2} bytes/cell exceeds the 2.0 sanity bound"
    );
}
