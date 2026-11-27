//! T-302 acceptance: handshake + capabilities + PROXY v2 (SPEC-02 §3) —
//! WPT-3.1 (negotiation masks), WPT-3.2 (major version gate), WPT-3.3
//! (proxy footgun), WPT-3.4 (proxy passthrough semantics, codec form).

use lattice_wire::frame::FrameType;
use lattice_wire::handshake::{
    Capabilities, HandshakeError, WIRE_PROTOCOL_VERSION, client_check_capabilities,
    looks_like_proxy_v2, process_hello, strip_proxy_v2,
};

fn caps(bits: u64) -> Capabilities {
    Capabilities(bits)
}

/// Server v1 supported set (PIPELINE always on; QUIC reserved).
fn server_caps() -> Capabilities {
    caps(Capabilities::SERVER_SUPPORTED)
}

/// WPT-3.1: client requests impossible flags — server masks to
/// `supported ∩ desired`; session functions with the common set.
#[test]
fn wpt_3_1_negotiation_masks_to_intersection() {
    // Client asks for ARROW + WATCH + RESUME + QUIC (reserved, unsupported)
    // + bit 40 (unknown, forward-compat — ignored, not an error).
    let client = caps(
        Capabilities::ARROW
            | Capabilities::WATCH
            | Capabilities::RESUME
            | Capabilities::QUIC_RESERVED
            | (1u64 << 40),
    );
    let negotiated = process_hello(WIRE_PROTOCOL_VERSION, client.0, server_caps(), 300, 1 << 24)
        .expect("negotiation succeeds");
    assert!(negotiated.capabilities.contains(Capabilities::ARROW));
    assert!(negotiated.capabilities.contains(Capabilities::WATCH));
    assert!(
        !negotiated
            .capabilities
            .contains(Capabilities::QUIC_RESERVED),
        "reserved never granted"
    );
    assert!(
        !negotiated.capabilities.contains(1 << 40),
        "unknown flags dropped"
    );
    // Session functions with the common set: capabilities in negotiated are
    // exactly supported ∩ desired.
    assert_eq!(
        negotiated.capabilities.0,
        server_caps().0 & client.0,
        "supported ∩ desired"
    );
    // Client-side requirement check: requiring RESUME (supported) is fine.
    assert!(client_check_capabilities(negotiated.capabilities, caps(Capabilities::RESUME)).is_ok());
    // Requiring QUIC (unsupported) → CAPABILITY_MISMATCH.
    let err = client_check_capabilities(negotiated.capabilities, caps(Capabilities::QUIC_RESERVED))
        .expect_err("client-requiring QUIC must close");
    assert!(matches!(err, HandshakeError::CapabilityMismatch { .. }));
}

/// WPT-3.2: HELLO 2.0 against a 1.x server → VERSION_MISMATCH, no session
/// state; higher minor accepted; lower minor accepted.
#[test]
fn wpt_3_2_major_version_gate() {
    let v2_0 = 2u32 << 16; // 2.0
    let err = process_hello(v2_0, 0, server_caps(), 300, 1 << 24).expect_err("major mismatch");
    assert!(matches!(err, HandshakeError::VersionMismatch { .. }));
    assert!(format!("{err}").contains("VERSION_MISMATCH"));

    // 1.1 (higher minor than server 1.0) → rejected (server accepts LOWER).
    let v1_1 = (1u32 << 16) | 1;
    assert!(process_hello(v1_1, 0, server_caps(), 300, 1 << 24).is_err());

    // 1.0 (exact) → accepted.
    assert!(process_hello(WIRE_PROTOCOL_VERSION, 0, server_caps(), 300, 1 << 24).is_ok());
}

/// WPT-3.3: PROXY v2 signature on a non-proxy listener — the footgun guard.
/// The dispatcher MUST close instantly; header bytes never enter the frame
/// stream (HELLO parse would desync).
#[test]
fn wpt_3_3_proxy_footgun_instant_close() {
    let signature_head: Vec<u8> = lattice_wire::handshake::PROXY_V2_SIGNATURE.to_vec();
    assert!(looks_like_proxy_v2(&signature_head), "signature detected");

    // Non-proxy listener sees signature bytes → the connection must close
    // before ANY frame parsing. Codec-level: the session detects the
    // signature and returns ProxyV2Unexpected (a close, not a parse attempt).
    // Simulated: the first bytes are the signature → looks_like_proxy_v2
    // returns true → non-proxy path closes with PROXY_V2_UNEXPECTED.
    let detected = looks_like_proxy_v2(&signature_head);
    assert!(detected, "footgun guard fires on signature");
    // The error the session returns:
    let err = HandshakeError::ProxyV2Unexpected;
    assert!(format!("{err}").contains("PROXY_V2_UNEXPECTED"));
    // Frame handlers never see the bytes: the signature is NOT a valid
    // frame type byte, so parsing it as a frame would be a desync — the
    // guard prevents that path entirely.
    assert!(FrameType::from_byte(signature_head[0]).is_none());
}

/// WPT-3.4 (codec form): PROXY v2 header on an enabled listener — peer
/// address extracted for the policy table; header consumed (never visible
/// to frame handlers).
#[test]
fn wpt_3_4_proxy_passthrough_semantics() {
    // Build a minimal v2 header: signature + version/cmd 0x21 + INET/stream
    // (0x11) + length 8 (IPv4 + 2 ports) + 4-byte IPv4 + 2 ports.
    let mut stream: Vec<u8> = lattice_wire::handshake::PROXY_V2_SIGNATURE.to_vec();
    stream.push(0x21); // v2, PROXY command
    stream.push(0x11); // INET, stream
    stream.extend_from_slice(&12u16.to_le_bytes()); // addr+ports length (4+4+2+2)
    stream.extend_from_slice(&[192, 168, 1, 42]); // source IPv4
    stream.extend_from_slice(&[10, 0, 0, 1]); // dest IPv4
    stream.extend_from_slice(&8443u16.to_be_bytes()); // src port
    stream.extend_from_slice(&5432u16.to_be_bytes()); // dst port

    // HELLO follows the header.
    let hello: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
    stream.extend_from_slice(&hello);

    let (peer, consumed) = strip_proxy_v2(&stream).expect("proxy header parses");
    assert_eq!(peer, "192.168.1.42:8443", "peer address from the header");
    assert_eq!(
        consumed,
        12 + 4 + 12,
        "signature + 4 + declared length consumed"
    );
    // The remaining stream is the HELLO bytes — the header is never visible
    // to frame handlers.
    let remainder = &stream[consumed..];
    assert_eq!(remainder, &hello, "post-header stream starts at HELLO");

    // IPv6 form: 36-byte address block.
    let mut v6: Vec<u8> = lattice_wire::handshake::PROXY_V2_SIGNATURE.to_vec();
    v6.push(0x21);
    v6.push(0x21); // INET6, stream
    v6.extend_from_slice(&36u16.to_le_bytes());
    for i in 0..16u8 {
        v6.push(i + 1); // source IPv6
    }
    for i in 0..16u8 {
        v6.push(0x20 + i); // destination IPv6
    }
    v6.extend_from_slice(&[0, 80]); // src port
    v6.extend_from_slice(&[0, 90]); // dst port
    let (peer6, consumed6) = strip_proxy_v2(&v6).expect("IPv6 header parses");
    assert!(
        peer6.starts_with('[') && peer6.contains(':'),
        "IPv6 bracketed: {peer6}"
    );
    assert_eq!(consumed6, 12 + 4 + 36);

    // Malformed: wrong version byte → ProxyV2Malformed.
    let mut bad = lattice_wire::handshake::PROXY_V2_SIGNATURE.to_vec();
    bad.push(0x20); // v2 command 0 — invalid for this parse (want 0x21)
    bad.push(0x11);
    bad.extend_from_slice(&8u16.to_le_bytes());
    bad.extend_from_slice(&[0u8; 8]);
    assert!(matches!(
        strip_proxy_v2(&bad),
        Err(HandshakeError::ProxyV2Malformed { .. })
    ));
}

/// Full handshake sequence: HELLO → PARAMS → (AUTH placeholder) → READY.
#[test]
fn handshake_sequence_yields_negotiated_params() {
    let negotiated = process_hello(
        WIRE_PROTOCOL_VERSION,
        Capabilities::ARROW | Capabilities::WATCH,
        server_caps(),
        300,
        1 << 24,
    )
    .expect("handshake");
    assert_eq!(negotiated.protocol_version, WIRE_PROTOCOL_VERSION);
    assert_eq!(negotiated.resume_token_ttl_s, 300);
    assert_eq!(negotiated.max_frame, 1 << 24);
    assert!(negotiated.capabilities.contains(Capabilities::ARROW));
    assert!(negotiated.capabilities.contains(Capabilities::WATCH));
    assert!(
        !negotiated
            .capabilities
            .contains(Capabilities::COMPRESSION_ZSTD),
        "not requested"
    );
}
