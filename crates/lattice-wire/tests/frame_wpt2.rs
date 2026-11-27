//! T-301 acceptance: framing + TLV (SPEC-02 §2) — WPT-2.1 (oversized frame
//! bound), WPT-2.2 (read_exact discipline: truncation detected, committed
//! state intact), WPT-2.3 (request_id echo over pipelined commands), plus
//! TLV encode/decode determinism and duplicate-tag rejection.
//!
//! These are the codec-level forms: bound checks and parse discipline run
//! in-process over the frame/TLV encoders; the socket-level integration
//! (live connections) lands with T-304's session layer.

use lattice_wire::frame::{
    ABSOLUTE_MAX_FRAME, DEFAULT_MAX_FRAME, Frame, FrameError, FrameType, check_frame_bound,
    parse_frame,
};
use lattice_wire::tlv::{TAG_REQUEST_ID, TlvError, TlvMap};

/// WPT-2.1 (codec form): frames over the negotiated max are rejected at the
/// HEADER alone — before any body read (the stream is desynced).
#[test]
fn wpt_2_1_oversized_frame_rejected_at_header() {
    // Default max: 16 MiB. 16 MiB + 1 → rejected.
    let mut header = [0u8; 5];
    header[0] = FrameType::Ack.byte();
    header[1..5].copy_from_slice(&((DEFAULT_MAX_FRAME + 1) as u32).to_le_bytes());
    let err = check_frame_bound(&header, DEFAULT_MAX_FRAME).expect_err("over-bound rejected");
    assert!(matches!(err, FrameError::FrameTooLarge { .. }));
    assert!(format!("{err}").contains("FRAME_TOO_LARGE"));

    // Exactly at max → allowed.
    header[1..5].copy_from_slice(&(DEFAULT_MAX_FRAME as u32).to_le_bytes());
    assert!(check_frame_bound(&header, DEFAULT_MAX_FRAME).is_ok());

    // Absolute cap 256 MiB: a 100 MiB frame passes the default-max host if
    // the host negotiated higher; nothing passes 256 MiB.
    header[1..5].copy_from_slice(&(100 * 1024 * 1024u32).to_le_bytes());
    assert!(check_frame_bound(&header, DEFAULT_MAX_FRAME).is_err());
    let err = check_frame_bound(&header, ABSOLUTE_MAX_FRAME).expect("within absolute cap");
    assert_eq!(err as u64, 100 * 1024 * 1024);

    header[1..5].copy_from_slice(&((ABSOLUTE_MAX_FRAME + 1) as u32).to_le_bytes());
    assert!(check_frame_bound(&header, ABSOLUTE_MAX_FRAME).is_err());
}

/// Frame round-trip: encode → parse preserves type + payload; unknown
/// types rejected explicitly.
#[test]
fn frame_roundtrip_and_unknown_type() {
    let payload = b"hello wire".to_vec();
    let f = Frame::new(FrameType::Ack, payload.clone());
    let bytes = f.encode();
    assert_eq!(bytes[0], 0x40);
    assert_eq!(
        u32::from_le_bytes(bytes[1..5].try_into().expect("4")) as usize,
        payload.len()
    );
    let parsed = parse_frame(bytes[0], bytes[5..].to_vec()).expect("parse");
    assert_eq!(parsed.frame_type, FrameType::Ack);
    assert_eq!(parsed.payload, payload);

    // Unknown type byte.
    let err = parse_frame(0xFF, vec![]).expect_err("unknown type");
    assert!(matches!(err, FrameError::UnknownType(0xFF)));
    // Registry closed: all declared types round-trip.
    for b in [0x01u8, 0x05, 0x10, 0x20, 0x30, 0x40, 0x50] {
        let parsed = parse_frame(b, vec![]).expect("declared type");
        assert_eq!(
            FrameType::from_byte(b).expect("known").byte(),
            parsed.frame_type.byte()
        );
    }
}

/// WPT-2.2 (codec form): truncated payloads are detected at parse — no
/// partial interpretation; the caller's committed state is untouched (the
/// journal LSN sequence lives in the storage layer, asserted in
/// determinism_oracle SFT-6.1).
#[test]
fn wpt_2_2_truncated_payload_detected() {
    // TLV declares 100-byte value but body carries 10 → truncated.
    let mut payload = Vec::new();
    payload.extend_from_slice(&0x0002u16.to_le_bytes());
    payload.extend_from_slice(&100u32.to_le_bytes());
    payload.extend_from_slice(&[0u8; 10]);
    let err = TlvMap::decode(&payload).expect_err("truncated TLV");
    assert!(matches!(err, TlvError::Truncated { .. }));

    // Frame body shorter than its declared length → parse-level truncation
    // is the session layer's read_exact (codec: we detect via the bound
    // check + body length mismatch at the session). Header alone stays
    // consistent: bound check passes for any len ≤ max.
    let mut header = [0u8; 5];
    header[0] = FrameType::AttrSet.byte();
    header[1..5].copy_from_slice(&50u32.to_le_bytes());
    assert!(
        check_frame_bound(&header, DEFAULT_MAX_FRAME).is_ok(),
        "bound ok; body shortage caught by read_exact"
    );
}

/// WPT-2.3 (codec form): request_id echo — 100 pipelined "commands", each
/// reply carries exactly its request's request_id; zero mismatches.
#[test]
fn wpt_2_3_request_id_echo_pipeline() {
    let mut replies = Vec::new();
    for id in 1..=100u64 {
        let mut req = TlvMap::new();
        req.request_id(id);
        req.put_str(0x0010, "cmd");
        let bytes = req.encode();

        // Server side: decode request, echo request_id into the reply.
        let decoded = TlvMap::decode_request(&bytes).expect("request decodes");
        let echoed_id = decoded.request_id_value().expect("mandatory id");
        let mut reply = TlvMap::new();
        reply.request_id(echoed_id);
        reply.put_str(0x0011, "ok");
        replies.push((id, reply.encode()));
    }
    // Client side: correlate replies to commands.
    for (id, reply_bytes) in &replies {
        let reply = TlvMap::decode(reply_bytes).expect("reply decodes");
        assert_eq!(
            reply.request_id_value(),
            Some(*id),
            "reply must echo its command's request_id"
        );
    }
}

/// TLV semantics: deterministic encode order (BTreeMap), duplicate tags
/// rejected, missing request_id rejected, u64/str accessors.
#[test]
fn tlv_semantics() {
    let mut m = TlvMap::new();
    m.put_str(0x0010, "task");
    m.put_u64(0x0002, 42);
    m.request_id(7);
    let bytes = m.encode();
    let back = TlvMap::decode(&bytes).expect("decode");
    assert_eq!(back, m, "round-trip equality");
    assert_eq!(back.get_str(0x0010), Some("task"));
    assert_eq!(back.get_u64(0x0002), Some(42));
    assert_eq!(back.request_id_value(), Some(7));

    // Duplicate tag rejected.
    let mut dup = Vec::new();
    for _ in 0..2 {
        dup.extend_from_slice(&TAG_REQUEST_ID.to_le_bytes());
        dup.extend_from_slice(&8u32.to_le_bytes());
        dup.extend_from_slice(&1u64.to_le_bytes());
    }
    assert!(matches!(
        TlvMap::decode(&dup),
        Err(TlvError::DuplicateTag {
            tag: TAG_REQUEST_ID
        })
    ));

    // Request without request_id rejected.
    let no_id = TlvMap::new().encode();
    assert!(matches!(
        TlvMap::decode_request(&no_id),
        Err(TlvError::MissingRequest)
    ));

    // u64 shape enforcement.
    let mut bad = TlvMap::new();
    bad.put(TAG_REQUEST_ID, vec![1, 2, 3]);
    assert!(
        bad.request_id_value().is_none(),
        "3-byte request_id is not u64"
    );
}
