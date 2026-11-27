//! Host configuration subsystem (SPEC-02 §10 — the "no hardcoded parameters"
//! owner law).
//!
//! Contract points implemented here:
//! - One host config file, TOML (SPEC-02 §12.4), six groups (§10 table).
//! - Strict validation: unknown or misspelled keys are startup errors naming
//!   the offending key **and** its section (WPT-10.5); no silent defaults.
//! - Safety clamps: documented absolute min/max per parameter; values outside
//!   a clamp are startup errors, never silently corrected (§10).
//! - Defaults come from profiles (never code literals): `dev`, `laptop`,
//!   `server`, `analytics` shipped as versioned TOML sets (§10).
//! - `config autodetect`: generates a tuned set from host CPU/RAM/disk class
//!   (pgtune pattern); same host specs ⇒ byte-identical output (WPT-10.4).
//! - Hot-reload classes: a documented safe class (keepalive, cooldown, credit
//!   caps, metrics, log levels) applies live; everything else is
//!   restart-required (WPT-10.6). See [`reload`].
//!
//! All scalar parameters of the tree are inventoried in the tests
//! (`tests/config_wpt10.rs`), which keeps the hot/restart classification and
//! the clamp table from drifting out of sync with the types.

pub mod autodetect;
pub mod profiles;
pub mod reload;

use serde::{Deserialize, Serialize};
use std::fmt;

/// TOML text of one host config file (SPEC-02 §12.4).
pub fn parse_config(text: &str) -> Result<HostConfig, ConfigError> {
    let cfg: HostConfig = toml::from_str(text).map_err(|e| annotate(text, &e))?;
    cfg.validate()?;
    Ok(cfg)
}

/// Parse/validate error. Message is user-facing: names key and section.
#[derive(Debug)]
pub struct ConfigError {
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

fn err(msg: impl Into<String>) -> ConfigError {
    ConfigError {
        message: msg.into(),
    }
}

/// Augment a TOML error with the section it occurred in (WPT-10.5: startup
/// errors must name the offending key and section).
fn annotate(text: &str, e: &toml::de::Error) -> ConfigError {
    let base = e.message().to_string();
    let section = e.span().and_then(|span| section_at(text, span.start));
    match section {
        Some(s) => err(format!("{base} (in section [{s}])")),
        None => err(base),
    }
}

/// The last `[header]` table path at or before `offset` bytes.
fn section_at(text: &str, offset: usize) -> Option<String> {
    let upto = &text[..offset.min(text.len())];
    let line_no = upto.matches('\n').count();
    let lines: Vec<&str> = text.lines().take(line_no + 1).collect();
    for line in lines.iter().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            return Some(trimmed[1..trimmed.len() - 1].to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Typed tree. Field order = TOML output order (deterministic autodetect);
// every struct denies unknown fields.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub network: NetworkConfig,
    pub auth: AuthConfig,
    pub engine: EngineConfig,
    pub storage: StorageConfig,
    pub resources: ResourcesConfig,
    pub observability: ObservabilityConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Max frame payload bytes; server-side restart-required cap (WPT-10.6).
    pub max_frame_bytes: u64,
    /// Credit flow-control window, frames (hot-reloadable).
    pub credit_frames: u32,
    /// Credit flow-control window, bytes (hot-reloadable).
    pub credit_bytes: u64,
    /// PING/PONG idle interval (hot-reloadable).
    pub keepalive_interval_ms: u64,
    /// Missed keepalives before the session is dead (hot-reloadable).
    pub keepalive_misses: u32,
    /// Resume token lifetime.
    pub resume_ttl_s: u64,
    /// Idle cursor eviction.
    pub cursor_idle_ttl_s: u64,
    /// Concurrent session cap per subject.
    pub session_cap_per_subject: u32,
    /// Listener accept backlog.
    pub listen_backlog: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// pg_hba-style ordered policy table, TOML (SPEC-02 §4, §12.4).
    pub policy_table_path: String,
    /// SCRAM verifier store, host-managed (the pg_authid pattern).
    pub verifier_path: String,
    /// Cooldown after 5 consecutive failures for a source (hot-reloadable).
    pub failure_cooldown_ms: u64,
    /// Identity map file: cert CN/SAN URI → subject; UDS (uid,pid) → subject.
    pub identity_map_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    /// Propagation worklist batch depth (§8 fixpoint).
    pub worklist_batch_depth: u32,
    /// Timer wheel tick granularity (P8: the only clock).
    pub timer_wheel_tick_ms: u64,
    /// Stall-detector grace period.
    pub stall_grace_ms: u64,
    /// Propagation queue cap.
    pub propagation_queue_cap: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Group-commit window for journal fsync (0 = per-commit). SPEC-03 §9.8.
    pub group_commit_window_ms: u64,
    /// Journal sync method; `fdatasync` default per SPEC-03 §9.8.
    pub sync_method: SyncMethod,
    /// CRC block size for snapshot/part block tables (power of two).
    pub crc_block_bytes: u32,
    /// Target size of one immutable columnar part.
    pub part_target_bytes: u64,
    /// Size-tiered merge trigger: parts per (graph, attr).
    pub merge_max_parts: u32,
    /// Staging buffer budget before a merge is forced.
    pub merge_staging_bytes: u64,
    /// Arena budget: structural arrays.
    pub arena_structure_bytes: u64,
    /// Arena budget: columnar parts.
    pub arena_parts_bytes: u64,
    /// Arena budget: sessions.
    pub arena_sessions_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncMethod {
    Fdatasync,
    Fsync,
    OpenDatasync,
    OpenSync,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcesConfig {
    /// Kernel worker threads (subtree U-locks; disjoint from async runtime).
    pub worker_threads: u32,
    /// Sequencer queue depth (single sequencer thread — P15).
    pub sequencer_queue_depth: u32,
    /// Background merge pool size (0 = merges inline; not for production).
    pub merge_pool_threads: u32,
    /// Memory budget shares between arenas (normalized at use).
    pub memory_budget_shares: MemoryShares,
    /// Global allocator tuning (SPEC-02 §12.9; backend applied per build
    /// feature, defaults to mimalloc).
    pub allocator: AllocatorConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryShares {
    pub structure: u32,
    pub parts: u32,
    pub sessions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocatorConfig {
    /// `mimalloc` (default, feature `allocator-mimalloc`) or `system`.
    pub backend: AllocatorBackend,
    /// mimalloc purge delay, ms (0 = purge immediately).
    pub purge_delay_ms: u32,
    /// Transparent huge pages: `off` | `on` | `full` (never split THP).
    pub allow_thp: ThpMode,
    /// Reserve N 1-GiB huge pages at startup.
    pub huge_pages_gib: u32,
    /// Eager arena commit.
    pub eager_commit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllocatorBackend {
    Mimalloc,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThpMode {
    Off,
    On,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// Metrics exposition interval (hot-reloadable).
    pub metrics_interval_ms: u64,
    /// Log level (hot-reloadable).
    pub log_level: LogLevel,
    /// Slow-cursor trace threshold (hot-reloadable).
    pub slow_cursor_trace_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

// ---------------------------------------------------------------------------
// Validation: safety clamps (documented absolute bounds, SPEC-02 §10).
// Values outside a clamp are startup errors — never silently corrected.
// ---------------------------------------------------------------------------

fn clamp_u64(
    section: &str,
    key: &str,
    v: u64,
    min: u64,
    max: u64,
    unit: &str,
) -> Result<(), ConfigError> {
    if v < min || v > max {
        return Err(err(format!(
            "parameter `{section}.{key}` = {v} {unit} outside safety clamp [{min}, {max}] {unit}"
        )));
    }
    Ok(())
}

fn clamp_u32(
    section: &str,
    key: &str,
    v: u32,
    min: u32,
    max: u32,
    unit: &str,
) -> Result<(), ConfigError> {
    clamp_u64(
        section,
        key,
        u64::from(v),
        u64::from(min),
        u64::from(max),
        unit,
    )
}

fn non_empty(section: &str, key: &str, v: &str) -> Result<(), ConfigError> {
    if v.trim().is_empty() {
        return Err(err(format!(
            "parameter `{section}.{key}` must be a non-empty path"
        )));
    }
    Ok(())
}

impl HostConfig {
    /// Strict semantic validation (clamps, powers of two, non-empty paths).
    pub fn validate(&self) -> Result<(), ConfigError> {
        let n = &self.network;
        clamp_u64(
            "network",
            "max_frame_bytes",
            n.max_frame_bytes,
            1024,
            64 * 1024 * 1024,
            "bytes",
        )?;
        clamp_u32(
            "network",
            "credit_frames",
            n.credit_frames,
            1,
            1_048_576,
            "frames",
        )?;
        clamp_u64(
            "network",
            "credit_bytes",
            n.credit_bytes,
            4096,
            1024 * 1024 * 1024,
            "bytes",
        )?;
        clamp_u64(
            "network",
            "keepalive_interval_ms",
            n.keepalive_interval_ms,
            100,
            3_600_000,
            "ms",
        )?;
        clamp_u32(
            "network",
            "keepalive_misses",
            n.keepalive_misses,
            1,
            100,
            "misses",
        )?;
        clamp_u64("network", "resume_ttl_s", n.resume_ttl_s, 1, 86_400, "s")?;
        clamp_u64(
            "network",
            "cursor_idle_ttl_s",
            n.cursor_idle_ttl_s,
            1,
            86_400,
            "s",
        )?;
        clamp_u32(
            "network",
            "session_cap_per_subject",
            n.session_cap_per_subject,
            1,
            65_535,
            "sessions",
        )?;
        clamp_u32(
            "network",
            "listen_backlog",
            n.listen_backlog,
            1,
            32_767,
            "connections",
        )?;

        non_empty("auth", "policy_table_path", &self.auth.policy_table_path)?;
        non_empty("auth", "verifier_path", &self.auth.verifier_path)?;
        clamp_u64(
            "auth",
            "failure_cooldown_ms",
            self.auth.failure_cooldown_ms,
            0,
            3_600_000,
            "ms",
        )?;
        non_empty("auth", "identity_map_path", &self.auth.identity_map_path)?;

        let e = &self.engine;
        clamp_u32(
            "engine",
            "worklist_batch_depth",
            e.worklist_batch_depth,
            1,
            1_048_576,
            "items",
        )?;
        clamp_u64(
            "engine",
            "timer_wheel_tick_ms",
            e.timer_wheel_tick_ms,
            1,
            1_000,
            "ms",
        )?;
        clamp_u64(
            "engine",
            "stall_grace_ms",
            e.stall_grace_ms,
            0,
            3_600_000,
            "ms",
        )?;
        clamp_u32(
            "engine",
            "propagation_queue_cap",
            e.propagation_queue_cap,
            1,
            16_777_216,
            "items",
        )?;

        let s = &self.storage;
        clamp_u64(
            "storage",
            "group_commit_window_ms",
            s.group_commit_window_ms,
            0,
            1_000,
            "ms",
        )?;
        clamp_u32(
            "storage",
            "crc_block_bytes",
            s.crc_block_bytes,
            4096,
            1_048_576,
            "bytes",
        )?;
        if !s.crc_block_bytes.is_power_of_two() {
            return Err(err(format!(
                "parameter `storage.crc_block_bytes` = {} must be a power of two",
                s.crc_block_bytes
            )));
        }
        clamp_u64(
            "storage",
            "part_target_bytes",
            s.part_target_bytes,
            1_048_576,
            16 * 1024 * 1024 * 1024,
            "bytes",
        )?;
        clamp_u32(
            "storage",
            "merge_max_parts",
            s.merge_max_parts,
            2,
            10_000,
            "parts",
        )?;
        clamp_u64(
            "storage",
            "merge_staging_bytes",
            s.merge_staging_bytes,
            1_048_576,
            256 * 1024 * 1024 * 1024,
            "bytes",
        )?;
        clamp_u64(
            "storage",
            "arena_structure_bytes",
            s.arena_structure_bytes,
            1_048_576,
            1024 * 1024 * 1024 * 1024,
            "bytes",
        )?;
        clamp_u64(
            "storage",
            "arena_parts_bytes",
            s.arena_parts_bytes,
            1_048_576,
            1024 * 1024 * 1024 * 1024,
            "bytes",
        )?;
        clamp_u64(
            "storage",
            "arena_sessions_bytes",
            s.arena_sessions_bytes,
            1_048_576,
            1024 * 1024 * 1024 * 1024,
            "bytes",
        )?;

        let r = &self.resources;
        clamp_u32(
            "resources",
            "worker_threads",
            r.worker_threads,
            1,
            1024,
            "threads",
        )?;
        clamp_u32(
            "resources",
            "sequencer_queue_depth",
            r.sequencer_queue_depth,
            16,
            16_777_216,
            "items",
        )?;
        clamp_u32(
            "resources",
            "merge_pool_threads",
            r.merge_pool_threads,
            0,
            256,
            "threads",
        )?;
        let m = &r.memory_budget_shares;
        clamp_u32(
            "resources.memory_budget_shares",
            "structure",
            m.structure,
            1,
            1_000_000,
            "share",
        )?;
        clamp_u32(
            "resources.memory_budget_shares",
            "parts",
            m.parts,
            1,
            1_000_000,
            "share",
        )?;
        clamp_u32(
            "resources.memory_budget_shares",
            "sessions",
            m.sessions,
            1,
            1_000_000,
            "share",
        )?;
        let a = &r.allocator;
        clamp_u32(
            "resources.allocator",
            "purge_delay_ms",
            a.purge_delay_ms,
            0,
            60_000,
            "ms",
        )?;
        clamp_u32(
            "resources.allocator",
            "huge_pages_gib",
            a.huge_pages_gib,
            0,
            1024,
            "GiB",
        )?;

        let o = &self.observability;
        clamp_u64(
            "observability",
            "metrics_interval_ms",
            o.metrics_interval_ms,
            100,
            3_600_000,
            "ms",
        )?;
        clamp_u64(
            "observability",
            "slow_cursor_trace_ms",
            o.slow_cursor_trace_ms,
            0,
            3_600_000,
            "ms",
        )?;
        Ok(())
    }
}
