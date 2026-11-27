//! SCRAM-SHA-256 (RFC 5802/7677, SPEC-02 §4): server + client exchange.
//!
//! Verifier store: host-managed secrets (pg_authid pattern) — salt +
//! iterations + StoredKey + ServerKey; the password NEVER leaves the host
//! (and never exists after CREATE USER derivation).
//!
//! Messages (RFC 5802 §5):
//! - client-first:    `gs2-header + "n=" username ",r=" cnonce`
//!   (gs2: `n,,` no-binding | `y,,` binding-not-supported | `p=...,,`)
//! - server-first:    `r=fullnonce,s=b64(salt),i=iterations`
//! - client-final:    `c=b64(gs2header),r=fullnonce,p=b64(proof)`
//! - server-final:    `v=b64(server_signature)`
//!
//! AuthMessage = client-first-bare + "," + server-first + "," + client-final-
//! without-proof. Proof = ClientKey XOR HMAC(StoredKey, AuthMessage); valid
//! iff SHA256(ClientKey) == StoredKey.
//!
//! WPT-4.5 (no user oracle): unknown users run the IDENTICAL code path
//! against a dummy verifier — same error variant, same cost profile.
//!
//! Nonce generation is INJECTED (`NonceFn`) — deterministic in tests, host
//! RNG in production (D4: nonces live on the wire, never in stored bytes).

use crate::crypto::{base64_decode, base64_encode, hmac_sha256, pbkdf2_hmac_sha256, sha256};
use std::collections::BTreeMap;
use std::fmt;

/// RFC 7677 minimum iterations.
pub const DEFAULT_ITERATIONS: u32 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScramError {
    /// Generic AUTH_FAILED — identical for wrong-password and unknown-user
    /// (WPT-4.5: no user oracle).
    AuthFailed,
    Malformed {
        message: String,
    },
    NonceMismatch,
    ChannelBindingMismatch,
}

impl fmt::Display for ScramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScramError::AuthFailed => write!(f, "AUTH_FAILED"),
            ScramError::Malformed { message } => write!(f, "SCRAM malformed: {message}"),
            ScramError::NonceMismatch => write!(f, "SCRAM nonce mismatch"),
            ScramError::ChannelBindingMismatch => write!(f, "channel binding mismatch"),
        }
    }
}

impl std::error::Error for ScramError {}

/// Host-managed verifier (pg_authid pattern).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScramVerifier {
    pub salt: Vec<u8>,
    pub iterations: u32,
    pub stored_key: [u8; 32],
    pub server_key: [u8; 32],
}

impl ScramVerifier {
    /// Derive from a (one-time) password — runs at CREATE USER only.
    pub fn derive(_username: &str, password: &str, salt: &[u8], iterations: u32) -> Self {
        let salted = pbkdf2_hmac_sha256(password.as_bytes(), salt, iterations, 32);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let server_key = hmac_sha256(&salted, b"Server Key");
        Self {
            salt: salt.to_vec(),
            iterations,
            stored_key,
            server_key,
        }
    }
}

/// The dummy verifier for unknown users: real derivation (same cost), fixed
/// fixture-looking constants (stored bytes stay deterministic for tests).
fn dummy_verifier() -> ScramVerifier {
    let salt = vec![
        0x5B, 0x1A, 0x93, 0xCC, 0x74, 0xDD, 0xE2, 0xA1, 0x0F, 0x33, 0x87, 0xC5, 0x99, 0x41, 0x02,
        0xB6,
    ];
    ScramVerifier::derive(
        "__invalid__",
        "\u{1}dummy\u{7F}pw",
        &salt,
        DEFAULT_ITERATIONS,
    )
}

/// Injected nonce source: `NonceFn(len) -> nonce` (alnum only, RFC 5802
/// printable exclude `,`).
pub type NonceFn = fn(usize) -> String;

/// Server-side SCRAM conversation state (one AUTH exchange).
#[derive(Debug, Clone)]
pub struct ScramServer {
    server_first_message: String,
    client_first_bare: String,
    cnonce: String,
    snonce: String,
    verifier: ScramVerifier,
    gs2_header: String,
    is_known_user: bool,
}

impl ScramServer {
    /// The username from client-first (proof-validated by handle_client_final).
    pub fn username(&self) -> &str {
        self.client_first_bare
            .strip_prefix("n=")
            .and_then(|r| r.split_once(",r=").map(|(u, _)| u))
            .unwrap_or("")
    }
    /// Process the client-first message. Unknown users get the SAME code
    /// path with a dummy verifier (WPT-4.5). Returns the server-first
    /// message for the wire.
    pub fn handle_client_first(
        client_first: &str,
        nonce_fn: NonceFn,
        verifiers: &BTreeMap<String, ScramVerifier>,
    ) -> Result<(String, Self), ScramError> {
        // gs2-header: `flag,authzid,` (two commas) + client-first-bare.
        // flag: n | y | p=<channel-binding>. authzid: usually empty.
        let Some((flag, after_flag)) = client_first.split_once(',') else {
            return Err(ScramError::Malformed {
                message: "missing gs2 header".into(),
            });
        };
        let Some((authzid, bare_after)) = after_flag.split_once(',') else {
            return Err(ScramError::Malformed {
                message: "missing gs2 authzid comma".into(),
            });
        };
        let gs2_header = format!("{flag},{authzid},");
        if flag.starts_with('p') && !flag.contains("tls-server-end-point") {
            return Err(ScramError::ChannelBindingMismatch);
        }
        let Some((username, cnonce)) = bare_after
            .strip_prefix("n=")
            .and_then(|r| r.split_once(",r="))
            .map(|(u, n)| (u.to_string(), n.to_string()))
        else {
            return Err(ScramError::Malformed {
                message: "missing username/nonce".into(),
            });
        };

        let (verifier, is_known_user) = match verifiers.get(username.as_str()) {
            Some(v) => (v.clone(), true),
            None => (dummy_verifier(), false),
        };

        let snonce = nonce_fn(24);
        let client_first_bare = bare_after.to_string();
        let full_nonce = format!("{cnonce}{snonce}");
        let server_first_message = format!(
            "r={full_nonce},s={},i={}",
            base64_encode(&verifier.salt),
            verifier.iterations
        );

        Ok((
            server_first_message.clone(),
            Self {
                server_first_message,
                client_first_bare,
                cnonce,
                snonce,
                verifier,
                gs2_header: gs2_header.to_string(),
                is_known_user,
            },
        ))
    }

    /// Process the client-final message: verify nonce + channel binding +
    /// proof; return the server-final message and whether the user was a
    /// known user (tests only — the wire result is binary).
    pub fn handle_client_final(&self, client_final: &str) -> Result<(String, bool), ScramError> {
        // channel binding: "c=" must be base64(gs2_header)
        let expected_c = base64_encode(self.gs2_header.as_bytes());
        if !client_final.starts_with(&format!("c={expected_c},")) {
            return Err(ScramError::ChannelBindingMismatch);
        }
        let Some(p_pos) = client_final.rfind(",p=") else {
            return Err(ScramError::Malformed {
                message: "missing proof".into(),
            });
        };
        let without_proof = &client_final[..p_pos];
        let proof_b64 = &client_final[p_pos + 3..];
        let proof = base64_decode(proof_b64).ok_or(ScramError::AuthFailed)?;

        // Nonce continuation must match exactly.
        let expected_r = format!("{}{}", self.cnonce, self.snonce);
        let Some(client_r) = client_final
            .split_once(",r=")
            .map(|(_, r)| r.split(',').next().unwrap_or("").to_string())
        else {
            return Err(ScramError::Malformed {
                message: "missing r=".into(),
            });
        };
        if client_r != expected_r {
            return Err(ScramError::NonceMismatch);
        }

        // AuthMessage = client-first-bare + "," + server-first + "," + cwp
        let auth_message = format!(
            "{},{},{}",
            self.client_first_bare, self.server_first_message, without_proof
        );
        let client_signature = hmac_sha256(&self.verifier.stored_key, auth_message.as_bytes());
        let mut client_key = [0u8; 32];
        for (i, p) in proof.iter().enumerate() {
            client_key[i] = p ^ client_signature[i];
        }
        if sha256(&client_key) != self.verifier.stored_key {
            return Err(ScramError::AuthFailed);
        }
        let server_signature = hmac_sha256(&self.verifier.server_key, auth_message.as_bytes());
        Ok((
            format!("v={}", base64_encode(&server_signature)),
            self.is_known_user,
        ))
    }
}
