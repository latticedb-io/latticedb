//! Handshake + capability negotiation (SPEC-02 §3, T-302).
//!
//! Sequence: `C → HELLO {protocol_version, feature_flags, client_random}`
//! → [STARTTLS, TCP only] → `S → PARAMS {server_version, capabilities,
//! resume_token_ttl_s, max_frame}` → `C → AUTH (§4)` → `S → READY
//! {session_id, resume_token?, server_random}`.
//!
//! - Capabilities: u64 named bitfield. Server grants `supported ∩ desired`;
//!   both sides MUST NOT use unset caps. Unknown client flags ignored
//!   (forward-compat); unknown server-required cap ⇒ client closes
//!   `CAPABILITY_MISMATCH` (WPT-3.1).
//! - `protocol_version`: `major<<16|minor`. Major mismatch ⇒
//!   `VERSION_MISMATCH`, no session state created (WPT-3.2). Server accepts
//!   lower minor (additive types only).
//! - PROXY v2 (WPT-3.3/3.4): a listener with `proxy_protocol = true` MUST
//!   see the 12-byte v2 signature first (stripped before HELLO, peer
//!   address extracted for the policy table); a listener WITHOUT it closes
//!   instantly on signature bytes (`PROXY_V2_UNEXPECTED`) — the footgun
//!   guard, never desync.

use std::fmt;

pub const WIRE_PROTOCOL_MAJOR: u16 = 1;
pub const WIRE_PROTOCOL_MINOR: u16 = 0;

/// Packed `major<<16|minor`.
pub const WIRE_PROTOCOL_VERSION: u32 =
    ((WIRE_PROTOCOL_MAJOR as u32) << 16) | WIRE_PROTOCOL_MINOR as u32;

/// Capability bitfield (named registry, SPEC-02 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities(pub u64);

impl Capabilities {
    pub const ARROW: u64 = 1 << 0;
    pub const PROXY_V2: u64 = 1 << 1;
    pub const RESUME: u64 = 1 << 2;
    pub const WATCH: u64 = 1 << 3;
    pub const COMPRESSION_ZSTD: u64 = 1 << 4;
    pub const QUIC_RESERVED: u64 = 1 << 5;
    pub const PIPELINE: u64 = 1 << 6;

    /// v1 server-supported set (PIPELINE always on; QUIC reserved, never set).
    pub const SERVER_SUPPORTED: u64 = Self::ARROW
        | Self::PROXY_V2
        | Self::RESUME
        | Self::WATCH
        | Self::COMPRESSION_ZSTD
        | Self::PIPELINE;

    pub fn empty() -> Self {
        Self(0)
    }

    pub fn contains(self, cap: u64) -> bool {
        self.0 & cap == cap
    }

    pub fn set(&mut self, cap: u64) {
        self.0 |= cap;
    }

    /// WPT-3.1: server grants supported ∩ desired — unknown client flags
    /// are silently dropped (forward-compat).
    pub fn negotiate(server: Self, client_desired: Self) -> Self {
        Self(server.0 & client_desired.0)
    }

    /// A capability the client REQUIRES but the server doesn't support ⇒
    /// the client must close with CAPABILITY_MISMATCH.
    pub fn requires_unsupported(self, required: Self) -> Option<u64> {
        let missing = required.0 & !self.0;
        (missing != 0).then_some(missing)
    }
}

/// Handshake-phase errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    /// WPT-3.2: major version mismatch — no session state created.
    VersionMismatch { offered: u32, supported: u32 },
    /// Client-required capability the server can't provide.
    CapabilityMismatch { missing_bits: u64 },
    /// WPT-3.3: PROXY v2 signature on a non-proxy listener — instant close.
    ProxyV2Unexpected,
    /// PROXY v2 header required but absent (proxy-enabled listener).
    ProxyV2Required,
    /// PROXY v2 header malformed (bad version/length).
    ProxyV2Malformed { reason: String },
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandshakeError::VersionMismatch { offered, supported } => {
                let (omajor, ominor) = (offered >> 16, offered & 0xFFFF);
                let (smajor, sminor) = (supported >> 16, supported & 0xFFFF);
                write!(
                    f,
                    "VERSION_MISMATCH: client {omajor}.{ominor} vs server {smajor}.{sminor}"
                )
            }
            HandshakeError::CapabilityMismatch { missing_bits } => {
                write!(f, "CAPABILITY_MISMATCH: missing bits {missing_bits:#x}")
            }
            HandshakeError::ProxyV2Unexpected => {
                write!(
                    f,
                    "PROXY_V2_UNEXPECTED: proxy signature on non-proxy listener — closing"
                )
            }
            HandshakeError::ProxyV2Required => {
                write!(f, "proxy-enabled listener requires the PROXY v2 header")
            }
            HandshakeError::ProxyV2Malformed { reason } => {
                write!(f, "PROXY v2 header malformed: {reason}")
            }
        }
    }
}

impl std::error::Error for HandshakeError {}

/// The negotiated session parameters (server → client in PARAMS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub protocol_version: u32,
    pub capabilities: Capabilities,
    pub resume_token_ttl_s: u64,
    pub max_frame: u64,
    /// Peer address from the PROXY v2 header (policy table input, WPT-3.4).
    pub peer_addr: Option<String>,
}

/// PROXY v2 signature (12 bytes).
pub const PROXY_V2_SIGNATURE: &[u8; 12] = b"\x0D\x0A\x0D\x0A\x00\x0D\x0A\x51\x55\x49\x54\x0A";

/// WPT-3.3 footgun guard: does the stream START with the v2 signature?
pub fn looks_like_proxy_v2(first_bytes: &[u8]) -> bool {
    first_bytes.len() >= 12 && &first_bytes[..12] == PROXY_V2_SIGNATURE
}

/// PROXY v2 header parse (minimal: v2 protocol, INET stream, 16-byte IPv4/36
/// IPv6 address, no extensions). Returns the peer address string.
/// `header` = the full header AFTER the 12-byte signature.
pub fn parse_proxy_v2_header(header: &[u8]) -> Result<String, HandshakeError> {
    if header.len() < 4 {
        return Err(HandshakeError::ProxyV2Malformed {
            reason: "header truncated".into(),
        });
    }
    // byte 0: version (high nibble) + command (low nibble); v2 PROXY = 0x21.
    if header[0] != 0x21 {
        return Err(HandshakeError::ProxyV2Malformed {
            reason: format!("version/command byte {:#04x} (want 0x21)", header[0]),
        });
    }
    // byte 1: address family + protocol; INET(0x1)/INET6(0x2) stream(0x1).
    let family = header[1] >> 4;
    let addr_len = match family {
        0x1 => 4usize,  // IPv4
        0x2 => 16usize, // IPv6
        _ => {
            return Err(HandshakeError::ProxyV2Malformed {
                reason: format!("address family {family:#x} unsupported (UNIX/unsupported)"),
            });
        }
    };
    // PROXY v2: length covers src_addr + dst_addr + src_port + dst_port.
    let declared = u16::from_le_bytes(header[2..4].try_into().expect("2")) as usize;
    let expected_len = addr_len * 2 + 4; // both families: two 2-byte ports
    if declared != expected_len {
        return Err(HandshakeError::ProxyV2Malformed {
            reason: format!(
                "address length {declared} wrong for family {family:#x} (want {expected_len})"
            ),
        });
    }
    if header.len() < 4 + declared {
        return Err(HandshakeError::ProxyV2Malformed {
            reason: "address truncated".into(),
        });
    }
    // PROXY v2 INET: src_addr | dst_addr | src_port | dst_port.
    // The PEER (source) address is first; its port sits AFTER both
    // addresses (bytes 8..10 for INET, 20..22 for INET6).
    let addr = &header[4..4 + addr_len];
    // Layout: src_addr(addr_len) + dst_addr(addr_len) + ports(4) — the
    // peer port sits after BOTH addresses.
    let port_off = 4 + addr_len * 2;
    let port = u16::from_be_bytes(le_bytes(&header[port_off..port_off + 2]));
    Ok(match family {
        0x1 => format!("{}.{}.{}.{}:{}", addr[0], addr[1], addr[2], addr[3], port),
        _ => {
            // IPv6 hex groups (full form, deterministic)
            let groups: Vec<String> = (0..8)
                .map(|i| {
                    format!(
                        "{:x}",
                        u16::from_be_bytes(le_bytes(&addr[i * 2..i * 2 + 2]))
                    )
                })
                .collect();
            format!("[{}]:{}", groups.join(":"), port)
        }
    })
}

/// Strip the PROXY v2 header from the stream head (proxy-enabled listener).
/// Returns (peer_addr, bytes_consumed). WPT-3.4: the header is consumed and
/// never visible to frame handlers.
pub fn strip_proxy_v2(stream_head: &[u8]) -> Result<(String, usize), HandshakeError> {
    if stream_head.len() < 12 {
        return Err(HandshakeError::ProxyV2Required);
    }
    if !looks_like_proxy_v2(stream_head) {
        return Err(HandshakeError::ProxyV2Required);
    }
    let peer = parse_proxy_v2_header(&stream_head[12..])?;
    // total consumed = signature(12) + 4 + declared length
    let declared = u16::from_le_bytes(stream_head[14..16].try_into().expect("2")) as usize;
    Ok((peer, 12 + 4 + declared))
}

/// Byte-array from a statically verified slice (loud on mismatch).
fn le_bytes<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(src);
    out
}

/// Server-side HELLO processing: version gate + capability negotiation.
/// Returns the PARAMS payload (negotiated session parameters).
pub fn process_hello(
    protocol_version: u32,
    client_flags: u64,
    server_capabilities: Capabilities,
    resume_token_ttl_s: u64,
    max_frame: u64,
) -> Result<Negotiated, HandshakeError> {
    // WPT-3.2: major mismatch fails BEFORE any session state exists.
    if protocol_version >> 16 != WIRE_PROTOCOL_MAJOR as u32 {
        return Err(HandshakeError::VersionMismatch {
            offered: protocol_version,
            supported: WIRE_PROTOCOL_VERSION,
        });
    }
    // Minor: server accepts lower or equal minor (additive types only).
    let client_minor = protocol_version & 0xFFFF;
    if client_minor > WIRE_PROTOCOL_MINOR as u32 {
        return Err(HandshakeError::VersionMismatch {
            offered: protocol_version,
            supported: WIRE_PROTOCOL_VERSION,
        });
    }
    let capabilities = Capabilities::negotiate(server_capabilities, Capabilities(client_flags));
    Ok(Negotiated {
        protocol_version: WIRE_PROTOCOL_VERSION,
        capabilities,
        resume_token_ttl_s,
        max_frame,
        peer_addr: None,
    })
}

/// Client-side: check the PARAMS capabilities against requirements.
/// Missing required bit ⇒ CAPABILITY_MISMATCH (client closes).
pub fn client_check_capabilities(
    server_caps: Capabilities,
    client_required: Capabilities,
) -> Result<(), HandshakeError> {
    match server_caps.requires_unsupported(client_required) {
        Some(missing) => Err(HandshakeError::CapabilityMismatch {
            missing_bits: missing,
        }),
        None => Ok(()),
    }
}
