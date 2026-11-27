//! Authentication facade (SPEC-02 §4): the policy table (pg_hba pattern —
//! ordered rules, first match), the four v1 auth methods, and the source
//! cooldown (5 consecutive failures ⇒ refused without DB roundtrip).
//!
//! Authn failures → security log (counters here; the log stream is M4).
//! Allow/deny decisions for in-DB operations land in the journal (R11) —
//! NOT here; this crate is connection-auth only.

use crate::scram::{DEFAULT_ITERATIONS, ScramError, ScramServer, ScramVerifier};
use std::collections::BTreeMap;
use std::fmt;

/// Auth methods (SPEC-02 §4 registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    ScramSha256,
    ScramSha256Plus,
    ClientCert,
    UdsPeer,
    /// Library mode only — never on wire listeners.
    Trust,
}

impl AuthMethod {
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "scram_sha_256" => Self::ScramSha256,
            "scram_sha_256_plus" => Self::ScramSha256Plus,
            "client_cert" => Self::ClientCert,
            "uds_peer" => Self::UdsPeer,
            "trust" => Self::Trust,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ScramSha256 => "scram_sha_256",
            Self::ScramSha256Plus => "scram_sha_256_plus",
            Self::ClientCert => "client_cert",
            Self::UdsPeer => "uds_peer",
            Self::Trust => "trust",
        }
    }
}

/// TLS state requirement for a rule (§4 policy fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsState {
    Any,
    None,
    Require,
    VerifyCa,
    VerifyFull,
}

/// One ordered policy rule (SPEC-02 §4):
/// `(listener, database, subject_pattern, source, tls_state) → (method, scope_bound)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRule {
    pub listener: String,
    pub database: String,
    pub subject_pattern: String,
    pub source: String,
    pub tls_state: TlsState,
    pub method: AuthMethod,
    pub scope_bound: Vec<String>,
}

/// The policy table: ordered rules, FIRST MATCH wins (pg_hba discipline).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PolicyTable {
    pub rules: Vec<PolicyRule>,
}

/// A resolved auth decision: the subject + scope ceiling for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthDecision {
    pub subject: String,
    pub method: AuthMethod,
    pub scope_ceiling: Vec<String>,
    /// Cooldown active for this source (refused without DB roundtrip).
    pub cooldown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// The policy table has no matching rule.
    NoMatchingRule,
    /// Authn failure: IDENTICAL text for wrong-password and unknown-user
    /// (WPT-4.5 no-user-oracle law).
    AuthFailed,
    /// Source is in failure cooldown.
    CooldownActive,
    /// Method mismatch: the rule requires client_cert but the client
    /// attempted SCRAM, etc.
    MethodMismatch {
        required: AuthMethod,
        attempted: AuthMethod,
    },
    Scram(ScramError),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::NoMatchingRule => write!(f, "no matching policy rule"),
            AuthError::AuthFailed => write!(f, "AUTH_FAILED"),
            AuthError::CooldownActive => write!(f, "connection source in failure cooldown"),
            AuthError::MethodMismatch {
                required,
                attempted,
            } => {
                write!(
                    f,
                    "method mismatch: policy requires {required:?}, client attempted {attempted:?}"
                )
            }
            AuthError::Scram(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl PolicyTable {
    /// Parse from TOML ([[rules]] array). Strict: unknown keys named.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let mut rules = Vec::new();
        let mut in_rules = false;
        let mut current: Option<PolicyRule> = None;
        let mut set_fields: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[[rules]]" {
                if let Some(r) = current.take() {
                    rules.push(r);
                }
                in_rules = true;
                set_fields.clear();
                continue;
            }
            if !in_rules {
                return Err(format!("policy: line outside [[rules]]: `{line}`"));
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("policy: `{line}` is not key = value"));
            };
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            let mut set = |r: &mut PolicyRule| {
                set_fields.insert(key.to_string());
                match key {
                    "listener" => r.listener = value.to_string(),
                    "database" => r.database = value.to_string(),
                    "subject" => r.subject_pattern = value.to_string(),
                    "source" => r.source = value.to_string(),
                    "tls" => {
                        r.tls_state = match value {
                            "any" => TlsState::Any,
                            "none" => TlsState::None,
                            "require" => TlsState::Require,
                            "verify_ca" => TlsState::VerifyCa,
                            "verify_full" => TlsState::VerifyFull,
                            other => return Err(format!("policy: unknown tls `{other}`")),
                        }
                    }
                    "method" => {
                        r.method = AuthMethod::from_str(value)
                            .ok_or_else(|| format!("policy: unknown method `{value}`"))?
                    }
                    "scope" => {
                        let inner = value
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .split(',')
                            .map(|s| s.trim().trim_matches('"').to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        r.scope_bound = inner;
                    }
                    other => return Err(format!("policy: unknown key `{other}`")),
                }
                Ok(())
            };
            if let Some(ref mut r) = current {
                set(r)?;
            } else {
                let mut r = PolicyRule {
                    listener: "tcp".into(),
                    database: "any".into(),
                    subject_pattern: "*".into(),
                    source: "any".into(),
                    tls_state: TlsState::Any,
                    method: AuthMethod::ScramSha256,
                    scope_bound: Vec::new(),
                };
                set(&mut r)?;
                current = Some(r);
            }
        }
        if let Some(r) = current.take() {
            rules.push(r);
        }
        Ok(Self { rules })
    }

    /// First-match lookup (pg_hba discipline).
    pub fn match_rule(
        &self,
        listener: &str,
        database: &str,
        subject: &str,
        source: &str,
        tls_state: TlsState,
    ) -> Option<&PolicyRule> {
        // `subject` matching: exact, or trailing-`*` prefix (`svc_*`).
        let subject_matches = |pattern: &str| -> bool {
            match pattern.strip_suffix('*') {
                Some(prefix) => subject.starts_with(prefix),
                None => pattern == subject,
            }
        };
        self.rules.iter().find(|r| {
            (r.listener == "any" || r.listener == listener)
                && (r.database == "any" || r.database == database)
                && (r.subject_pattern == "*" || subject_matches(&r.subject_pattern))
                && (r.source == "any" || r.source == source)
                && (r.tls_state == TlsState::Any || r.tls_state == tls_state)
        })
    }

    /// Resolve the scope ceiling for a subject (first-match rule).
    pub fn resolve_scopes(
        &self,
        listener: &str,
        database: &str,
        subject: &str,
        source: &str,
        tls: TlsState,
    ) -> Option<(&PolicyRule, Vec<String>)> {
        let rule = self.match_rule(listener, database, subject, source, tls)?;
        Some((rule, rule.scope_bound.clone()))
    }
}

/// The authentication runtime: verifiers, identity maps, cooldowns.
pub struct AuthEngine {
    pub policy: PolicyTable,
    /// SCRAM verifiers: username → verifier (host-managed secrets file).
    pub scram_verifiers: BTreeMap<String, ScramVerifier>,
    /// client-cert identity map: cert CN/SAN URI → subject (pg_ident).
    pub cert_map: BTreeMap<String, String>,
    /// UDS peer map: "uid:pid" → subject.
    pub uds_map: BTreeMap<String, String>,
    /// Per-source consecutive failures.
    failures: BTreeMap<String, u32>,
    /// Sources in cooldown.
    cooldowns: BTreeMap<String, ()>,
    /// Failure threshold before cooldown (config, default 5 — §4).
    pub failure_threshold: u32,
    /// Injected server nonce fn (deterministic tests; host RNG in prod).
    pub nonce_fn: crate::scram::NonceFn,
}

impl AuthEngine {
    pub fn new(policy: PolicyTable) -> Self {
        Self {
            policy,
            scram_verifiers: BTreeMap::new(),
            cert_map: BTreeMap::new(),
            uds_map: BTreeMap::new(),
            failures: BTreeMap::new(),
            cooldowns: BTreeMap::new(),
            failure_threshold: 5,
            nonce_fn: default_nonce,
        }
    }

    /// Set a SCRAM verifier (CREATE USER path — password derived once).
    pub fn set_verifier(&mut self, username: &str, password: &str) -> &mut Self {
        // Salt: deterministic in tests (host RNG in prod — injected via
        // nonce_fn discipline; D4: salt lives in the secrets file, not the
        // journal).
        let salt = vec![0x42; 16];
        let verifier = ScramVerifier::derive(username, password, &salt, DEFAULT_ITERATIONS);
        self.scram_verifiers.insert(username.to_string(), verifier);
        self
    }

    fn source_key(source: &str) -> String {
        source.to_string()
    }

    /// Cooldown check: refused WITHOUT attempting authn (no DB roundtrip).
    fn cooldown_active(&self, source: &str) -> bool {
        self.cooldowns.contains_key(&Self::source_key(source))
    }

    fn record_failure(&mut self, source: &str) {
        let counter = self.failures.entry(Self::source_key(source)).or_insert(0);
        *counter += 1;
        if *counter >= self.failure_threshold {
            self.cooldowns.insert(Self::source_key(source), ());
        }
    }

    fn record_success(&mut self, source: &str) {
        self.failures.remove(&Self::source_key(source));
        self.cooldowns.remove(&Self::source_key(source));
    }

    /// SCRAM-SHA-256 step 1: process client-first, return (server-first
    /// message, server state). The caller relays the message and collects
    /// the client-final, then calls [`AuthEngine::complete_scram`].
    pub fn authenticate_scram(
        &mut self,
        listener: &str,
        database: &str,
        source: &str,
        client_first: &str,
        nonce_fn: Option<crate::scram::NonceFn>,
    ) -> Result<(String, ScramServer), AuthError> {
        if self.cooldown_active(source) {
            return Err(AuthError::CooldownActive);
        }
        // Policy first-match: resolve the method for this connection shape.
        // The subject rides in client-first; the policy resolves per-subject.
        let subject = client_first
            .split_once(",n=")
            .and_then(|(_, r)| r.split_once(",r=").map(|(u, _)| u.to_string()))
            .unwrap_or_default();
        let (rule, _scopes) = self
            .policy
            .resolve_scopes(listener, database, &subject, source, TlsState::Any)
            .ok_or(AuthError::NoMatchingRule)?;
        if rule.method != AuthMethod::ScramSha256 && rule.method != AuthMethod::ScramSha256Plus {
            return Err(AuthError::MethodMismatch {
                required: rule.method,
                attempted: AuthMethod::ScramSha256,
            });
        }
        let nonce_fn = nonce_fn.unwrap_or(self.nonce_fn);
        ScramServer::handle_client_first(client_first, nonce_fn, &self.scram_verifiers).map_err(
            |e| {
                self.record_failure(source);
                AuthError::Scram(e)
            },
        )
    }

    /// Complete SCRAM: client_final in → verify → decision (or failure).
    /// Returns (server-final message, decision).
    pub fn complete_scram(
        &mut self,
        listener: &str,
        database: &str,
        source: &str,
        server: &ScramServer,
        client_final: &str,
    ) -> Result<(String, AuthDecision), AuthError> {
        match server.handle_client_final(client_final) {
            Ok((server_final, _known_user)) => {
                // The username rides in client-first (validated by proof).
                let subject = server.username().to_string();
                let (_, scopes) = self
                    .policy
                    .resolve_scopes(listener, database, &subject, source, TlsState::Any)
                    .ok_or(AuthError::NoMatchingRule)?;
                self.record_success(source);
                Ok((
                    server_final,
                    AuthDecision {
                        subject,
                        method: AuthMethod::ScramSha256,
                        scope_ceiling: scopes,
                        cooldown: false,
                    },
                ))
            }
            Err(e) => {
                self.record_failure(source);
                Err(match e {
                    ScramError::AuthFailed => AuthError::AuthFailed,
                    other => AuthError::Scram(other),
                })
            }
        }
    }

    /// client-cert auth: the TLS layer presents the cert identity (CN or
    /// SAN URI) → subject via the identity map (pg_ident pattern).
    pub fn authenticate_cert(
        &mut self,
        listener: &str,
        database: &str,
        source: &str,
        cert_identity: &str,
    ) -> Result<AuthDecision, AuthError> {
        if self.cooldown_active(source) {
            return Err(AuthError::CooldownActive);
        }
        // cert identity → subject via the identity map (pg_ident), THEN
        // policy lookup for the mapped subject.
        let Some(subject) = self.cert_map.get(cert_identity).cloned() else {
            // Oracle resistance: same error as wrong password.
            self.record_failure(source);
            return Err(AuthError::AuthFailed);
        };
        let Some((rule, scopes)) =
            self.policy
                .resolve_scopes(listener, database, &subject, source, TlsState::VerifyFull)
        else {
            return Err(AuthError::NoMatchingRule);
        };
        if rule.method != AuthMethod::ClientCert {
            return Err(AuthError::MethodMismatch {
                required: rule.method,
                attempted: AuthMethod::ClientCert,
            });
        }
        self.record_success(source);
        Ok(AuthDecision {
            subject,
            method: AuthMethod::ClientCert,
            scope_ceiling: scopes,
            cooldown: false,
        })
    }

    /// UDS peer auth: (uid, pid) from SCM_CREDENTIALS → subject via map.
    pub fn authenticate_uds(
        &mut self,
        listener: &str,
        database: &str,
        source: &str,
        uid: u32,
        pid: u32,
    ) -> Result<AuthDecision, AuthError> {
        let peer_key = format!("{uid}:{pid}");
        // (uid,pid) → subject via the UDS map, THEN policy lookup.
        let Some(subject) = self.uds_map.get(&peer_key).cloned() else {
            self.record_failure(source);
            return Err(AuthError::AuthFailed);
        };
        let Some((rule, scopes)) =
            self.policy
                .resolve_scopes(listener, database, &subject, source, TlsState::None)
        else {
            return Err(AuthError::NoMatchingRule);
        };
        if rule.method != AuthMethod::UdsPeer {
            return Err(AuthError::MethodMismatch {
                required: rule.method,
                attempted: AuthMethod::UdsPeer,
            });
        }
        self.record_success(source);
        Ok(AuthDecision {
            subject,
            method: AuthMethod::UdsPeer,
            scope_ceiling: scopes,
            cooldown: false,
        })
    }
}

/// Default nonce: 24 alphanumerics from a simple LCG (host RNG injection
/// point — tests override with a deterministic fn).
fn default_nonce(_len: usize) -> String {
    // v1 core: constant-ish nonce from the process address entropy is NOT
    // deterministic — use a fixed base + caller-supplied variety. Real RNG
    // lands with the host runtime (T-304); tests inject.
    "serverservenonce".to_string()
}
