//! `latticedb config autodetect` (SPEC-02 §10): generates a tuned config set
//! from host CPU/RAM/disk class — the pgtune pattern.
//!
//! Skeleton contract (T-001): the host probe is modeled as [`HostSpecs`]; the
//! caller (the Host binary) fills it from the machine. The mapping
//! specs → config is a pure, deterministic function: same specs in ⇒
//! byte-identical TOML out (WPT-10.4). The formula constants below are the
//! documented tuning heuristic itself (the pgtune analog), not tunables.
//! Disk-class probing and cgroup-aware RAM accounting land with the
//! packaging work in M4; the Host currently probes CPU and RAM and assumes
//! the conservative `Hdd` class when unknown.

use super::{
    AllocatorBackend, AllocatorConfig, AuthConfig, EngineConfig, HostConfig, LogLevel,
    MemoryShares, NetworkConfig, ObservabilityConfig, ResourcesConfig, StorageConfig, SyncMethod,
    ThpMode, parse_config,
};
use serde::{Deserialize, Serialize};

/// Probed host inputs to the tuning formula.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSpecs {
    pub cpu_cores: u32,
    pub ram_gib: u32,
    pub disk_class: DiskClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskClass {
    Hdd,
    SataSsd,
    Nvme,
    MemoryBacked,
}

/// Generate a tuned, validated config set. Returns its TOML text.
pub fn autodetect(specs: &HostSpecs) -> Result<String, super::ConfigError> {
    let ram_bytes = u64::from(specs.ram_gib.max(1)) * 1024 * 1024 * 1024;
    let parts_share = ram_bytes * 40 / 100;
    let structure_share = ram_bytes * 10 / 100;
    let sessions_share = ram_bytes * 5 / 100;

    let (commit_window_ms, part_target_bytes) = match specs.disk_class {
        DiskClass::MemoryBacked => (0, 8 * 1024 * 1024 * 1024),
        DiskClass::Nvme => (1, 4 * 1024 * 1024 * 1024),
        DiskClass::SataSsd => (5, 2 * 1024 * 1024 * 1024),
        DiskClass::Hdd => (20, 512 * 1024 * 1024),
    };

    let cfg = HostConfig {
        network: NetworkConfig {
            max_frame_bytes: 16 * 1024 * 1024,
            credit_frames: 64,
            credit_bytes: 64 * 1024 * 1024,
            keepalive_interval_ms: 30_000,
            keepalive_misses: 3,
            resume_ttl_s: 300,
            cursor_idle_ttl_s: 60,
            session_cap_per_subject: 256,
            listen_backlog: 4096,
        },
        auth: AuthConfig {
            policy_table_path: "policies.toml".into(),
            verifier_path: "secrets.toml".into(),
            failure_cooldown_ms: 10_000,
            identity_map_path: "identity.toml".into(),
        },
        engine: EngineConfig {
            worklist_batch_depth: 4096,
            timer_wheel_tick_ms: 1,
            stall_grace_ms: 500,
            propagation_queue_cap: 65_536,
        },
        storage: StorageConfig {
            group_commit_window_ms: commit_window_ms,
            sync_method: SyncMethod::Fdatasync,
            crc_block_bytes: 65_536,
            part_target_bytes,
            merge_max_parts: 16,
            merge_staging_bytes: 32 * 1024 * 1024 * 1024,
            arena_structure_bytes: structure_share,
            arena_parts_bytes: parts_share,
            arena_sessions_bytes: sessions_share,
        },
        resources: ResourcesConfig {
            worker_threads: specs.cpu_cores.clamp(1, 1024),
            sequencer_queue_depth: specs.cpu_cores.saturating_mul(4096).clamp(16, 16_777_216),
            merge_pool_threads: (specs.cpu_cores / 8).clamp(0, 256),
            memory_budget_shares: MemoryShares {
                structure: 1,
                parts: 4,
                sessions: 1,
            },
            allocator: AllocatorConfig {
                backend: AllocatorBackend::Mimalloc,
                purge_delay_ms: 1_000,
                allow_thp: ThpMode::Off,
                huge_pages_gib: 0,
                eager_commit: false,
            },
        },
        observability: ObservabilityConfig {
            metrics_interval_ms: 1_000,
            log_level: LogLevel::Warn,
            slow_cursor_trace_ms: 500,
        },
    };

    cfg.validate()?;
    Ok(toml::to_string(&cfg).expect("serializing a validated config cannot fail"))
}

/// Validate generated text round-trips through the strict parser.
pub fn autodetect_validated(specs: &HostSpecs) -> Result<HostConfig, super::ConfigError> {
    parse_config(&autodetect(specs)?)
}
