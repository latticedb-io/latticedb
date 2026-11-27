//! T-001 acceptance: SPEC-02 §10 config subsystem, WPT-10.3–10.6 at the
//! config level (wire-level behavior of 10.3/10.6 lands with M3; the
//! config-level invariants — strict keys, clamps, determinism, reload
//! classes — are fully asserted here as data outcomes, per the testing law).

use latticedb::config::autodetect::{DiskClass, HostSpecs, autodetect, autodetect_validated};
use latticedb::config::profiles::{
    ANALYTICS_TOML, DEV_TOML, LAPTOP_TOML, SERVER_TOML, load_profile,
};
use latticedb::config::{
    AllocatorBackend, HostConfig, LogLevel, SyncMethod, ThpMode, parse_config, reload::plan_reload,
};

/// Inventory: legal-extreme config, every field at its clamp minimum.
/// MUST differ from [`extremes_max`] in every field (reload completeness).
fn extremes_min() -> HostConfig {
    HostConfig {
        network: latticedb::config::NetworkConfig {
            max_frame_bytes: 1024,
            credit_frames: 1,
            credit_bytes: 4096,
            keepalive_interval_ms: 100,
            keepalive_misses: 1,
            resume_ttl_s: 1,
            cursor_idle_ttl_s: 1,
            session_cap_per_subject: 1,
            listen_backlog: 1,
        },
        auth: latticedb::config::AuthConfig {
            policy_table_path: "min-policies.toml".into(),
            verifier_path: "min-secrets.toml".into(),
            failure_cooldown_ms: 0,
            identity_map_path: "min-identity.toml".into(),
        },
        engine: latticedb::config::EngineConfig {
            worklist_batch_depth: 1,
            timer_wheel_tick_ms: 1,
            stall_grace_ms: 0,
            propagation_queue_cap: 1,
        },
        storage: latticedb::config::StorageConfig {
            group_commit_window_ms: 0,
            sync_method: SyncMethod::Fdatasync,
            crc_block_bytes: 4096,
            part_target_bytes: 1_048_576,
            merge_max_parts: 2,
            merge_staging_bytes: 1_048_576,
            arena_structure_bytes: 1_048_576,
            arena_parts_bytes: 2_097_152,
            arena_sessions_bytes: 3_145_728,
        },
        resources: latticedb::config::ResourcesConfig {
            worker_threads: 1,
            sequencer_queue_depth: 16,
            merge_pool_threads: 0,
            memory_budget_shares: latticedb::config::MemoryShares {
                structure: 1,
                parts: 2,
                sessions: 3,
            },
            allocator: latticedb::config::AllocatorConfig {
                backend: AllocatorBackend::System,
                purge_delay_ms: 0,
                allow_thp: ThpMode::Off,
                huge_pages_gib: 0,
                eager_commit: false,
            },
        },
        observability: latticedb::config::ObservabilityConfig {
            metrics_interval_ms: 100,
            log_level: LogLevel::Trace,
            slow_cursor_trace_ms: 0,
        },
    }
}

/// Inventory: legal-extreme config, every field at its clamp maximum.
fn extremes_max() -> HostConfig {
    HostConfig {
        network: latticedb::config::NetworkConfig {
            max_frame_bytes: 64 * 1024 * 1024,
            credit_frames: 1_048_576,
            credit_bytes: 1024 * 1024 * 1024,
            keepalive_interval_ms: 3_600_000,
            keepalive_misses: 100,
            resume_ttl_s: 86_400,
            cursor_idle_ttl_s: 86_400,
            session_cap_per_subject: 65_535,
            listen_backlog: 32_767,
        },
        auth: latticedb::config::AuthConfig {
            policy_table_path: "max-policies.toml".into(),
            verifier_path: "max-secrets.toml".into(),
            failure_cooldown_ms: 3_600_000,
            identity_map_path: "max-identity.toml".into(),
        },
        engine: latticedb::config::EngineConfig {
            worklist_batch_depth: 1_048_576,
            timer_wheel_tick_ms: 1_000,
            stall_grace_ms: 3_600_000,
            propagation_queue_cap: 16_777_216,
        },
        storage: latticedb::config::StorageConfig {
            group_commit_window_ms: 1_000,
            sync_method: SyncMethod::OpenSync,
            crc_block_bytes: 1_048_576,
            part_target_bytes: 16 * 1024 * 1024 * 1024,
            merge_max_parts: 10_000,
            merge_staging_bytes: 256 * 1024 * 1024 * 1024,
            arena_structure_bytes: 1024 * 1024 * 1024 * 1024,
            arena_parts_bytes: 512 * 1024 * 1024 * 1024,
            arena_sessions_bytes: 128 * 1024 * 1024 * 1024,
        },
        resources: latticedb::config::ResourcesConfig {
            worker_threads: 1024,
            sequencer_queue_depth: 16_777_216,
            merge_pool_threads: 256,
            memory_budget_shares: latticedb::config::MemoryShares {
                structure: 1_000_000,
                parts: 999_999,
                sessions: 999_998,
            },
            allocator: latticedb::config::AllocatorConfig {
                backend: AllocatorBackend::Mimalloc,
                purge_delay_ms: 60_000,
                allow_thp: ThpMode::Full,
                huge_pages_gib: 1024,
                eager_commit: true,
            },
        },
        observability: latticedb::config::ObservabilityConfig {
            metrics_interval_ms: 3_600_000,
            log_level: LogLevel::Error,
            slow_cursor_trace_ms: 3_600_000,
        },
    }
}

/// All shipped profiles parse+validate and are pairwise distinct (§10).
#[test]
fn profiles_load_and_differ() {
    for (name, text) in [
        ("dev", DEV_TOML),
        ("laptop", LAPTOP_TOML),
        ("server", SERVER_TOML),
        ("analytics", ANALYTICS_TOML),
    ] {
        let cfg = load_profile(name).unwrap_or_else(|e| panic!("profile {name} must load: {e}"));
        let reparsed =
            parse_config(text).unwrap_or_else(|e| panic!("profile {name} text must parse: {e}"));
        assert_eq!(cfg, reparsed, "profile {name} round-trips");
    }
    let set = [DEV_TOML, LAPTOP_TOML, SERVER_TOML, ANALYTICS_TOML];
    for i in 0..set.len() {
        for j in (i + 1)..set.len() {
            assert_ne!(set[i], set[j], "profiles {i} and {j} must differ");
        }
    }
}

/// WPT-10.5: typo'd key / unknown section / missing section / out-of-clamp
/// value are all startup errors naming key and section; no silent defaults.
#[test]
fn wpt_10_5_strict_validation() {
    let typo = DEV_TOML.replace("keepalive_interval_ms", "keepaliev");
    let e = parse_config(&typo).expect_err("typo'd key must fail startup");
    assert!(
        e.message.contains("keepaliev"),
        "must name the key: {}",
        e.message
    );
    assert!(
        e.message.contains("network"),
        "must name the section: {}",
        e.message
    );

    let unknown_section = format!("{DEV_TOML}\n[resourcess]\nworker_threads = 4\n");
    let e = parse_config(&unknown_section).expect_err("unknown section must fail");
    assert!(
        e.message.contains("resourcess"),
        "must name the section: {}",
        e.message
    );

    let missing = DEV_TOML
        .split("[observability]")
        .next()
        .expect("dev profile has an observability section")
        .to_string();
    let e = parse_config(&missing).expect_err("missing section must fail");
    assert!(
        e.message.contains("observability"),
        "must name the missing section: {}",
        e.message
    );

    let over_clamp = DEV_TOML.replace(
        "max_frame_bytes = 1048576",
        "max_frame_bytes = 999999999999",
    );
    let e = parse_config(&over_clamp).expect_err("out-of-clamp value must fail");
    assert!(
        e.message.contains("max_frame_bytes"),
        "must name the key: {}",
        e.message
    );
    assert!(
        e.message.contains("safety clamp"),
        "must name the clamp: {}",
        e.message
    );
}

/// WPT-10.4: same host specs ⇒ byte-identical generated sets across runs;
/// divergent hosts ⇒ different sets; generated sets pass strict validation.
#[test]
fn wpt_10_4_autodetect_determinism() {
    let small = HostSpecs {
        cpu_cores: 4,
        ram_gib: 8,
        disk_class: DiskClass::Hdd,
    };
    let a1 = autodetect(&small).expect("autodetect must succeed");
    let a2 = autodetect(&small).expect("autodetect must succeed");
    let a3 = autodetect(&small).expect("autodetect must succeed");
    assert_eq!(a1, a2, "same specs ⇒ byte-identical output");
    assert_eq!(a2, a3, "same specs ⇒ byte-identical output");

    let cfg = autodetect_validated(&small).expect("generated set must pass strict validation");
    assert_eq!(
        cfg.resources.worker_threads, 4,
        "workers derive from cpu_cores"
    );
    assert_eq!(
        cfg.storage.group_commit_window_ms, 20,
        "Hdd ⇒ conservative commit window"
    );

    let large = HostSpecs {
        cpu_cores: 64,
        ram_gib: 512,
        disk_class: DiskClass::Nvme,
    };
    let b = autodetect(&large).expect("autodetect must succeed");
    assert_ne!(a1, b, "divergent hosts ⇒ different sets");
    let cfg_b = autodetect_validated(&large).expect("generated set must pass strict validation");
    assert_eq!(cfg_b.resources.worker_threads, 64);
    assert_eq!(
        cfg_b.storage.group_commit_window_ms, 1,
        "Nvme ⇒ tight commit window"
    );
    assert!(
        cfg_b.storage.arena_parts_bytes > cfg.storage.arena_parts_bytes,
        "more RAM ⇒ larger parts arena"
    );
}

/// WPT-10.3 (config level): every tunable set to a legal extreme (tiny/large)
/// parses, validates, and round-trips byte-exactly through TOML — zero value
/// divergence. Full behavioral equivalence (journal-level) is the M3
/// integration variant.
#[test]
fn wpt_10_3_legal_extremes_roundtrip() {
    for (label, cfg) in [("min", extremes_min()), ("max", extremes_max())] {
        let text = toml::to_string(&cfg).expect("serialize extremes");
        let parsed =
            parse_config(&text).unwrap_or_else(|e| panic!("{label} extremes must validate: {e}"));
        assert_eq!(
            parsed, cfg,
            "{label} extremes must round-trip with zero divergence"
        );
    }

    let over = extremes_max();
    let mut broken = over.clone();
    broken.network.credit_frames = 1_048_577; // one past the clamp
    let text = toml::to_string(&broken).expect("serialize");
    let e = parse_config(&text).expect_err("beyond-clamp value must be rejected");
    assert!(e.message.contains("credit_frames"), "{}", e.message);
}

/// WPT-10.6 (config level): reload applies the documented safe class live
/// (keepalive, cooldown, credit caps, metrics, log level, trace threshold);
/// `max_frame` and everything else is restart-required and untouched until
/// restart. Completeness: with all 40 parameters differing, every one is
/// classified exactly once (hot ∪ restart = the full tree, no drift).
#[test]
fn wpt_10_6_hot_reload_classes() {
    let mut live = extremes_min();
    let next = extremes_max();
    let before = live.clone();

    let outcome = plan_reload(&mut live, &next);

    let applied_expected: Vec<String> = [
        "network.credit_frames",
        "network.credit_bytes",
        "network.keepalive_interval_ms",
        "network.keepalive_misses",
        "auth.failure_cooldown_ms",
        "observability.metrics_interval_ms",
        "observability.log_level",
        "observability.slow_cursor_trace_ms",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        outcome.applied, applied_expected,
        "safe class applied, in field order"
    );
    assert_eq!(
        outcome.restart_required.len(),
        32,
        "all restart-required keys listed"
    );

    // Live config: hot fields now equal `next`; restart-required unchanged.
    assert_eq!(live.network.credit_frames, next.network.credit_frames);
    assert_eq!(
        live.network.keepalive_interval_ms,
        next.network.keepalive_interval_ms
    );
    assert_eq!(live.observability.log_level, next.observability.log_level);
    assert_eq!(live.auth.failure_cooldown_ms, next.auth.failure_cooldown_ms);
    assert_eq!(
        live.network.max_frame_bytes, before.network.max_frame_bytes,
        "max_frame NOT applied"
    );
    assert_eq!(
        live.resources.worker_threads, before.resources.worker_threads,
        "workers NOT applied"
    );
    assert_eq!(
        live.storage.sync_method, before.storage.sync_method,
        "sync_method NOT applied"
    );

    // Completeness / anti-drift: hot ∪ restart covers every parameter once
    // (inventory: network 9 + auth 4 + engine 4 + storage 9 + resources 11 +
    // observability 3 = 40).
    let mut all = outcome.applied.clone();
    all.extend(outcome.restart_required.iter().cloned());
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 40, "every parameter classified exactly once");
    assert!(
        outcome.restart_required.iter().all(|k| k.contains('.')),
        "restart keys are section.key paths"
    );
}

/// Reload with an identical config is a no-op on data, not on types.
#[test]
fn reload_identical_is_noop() {
    let mut live = load_profile("dev").expect("dev loads");
    let snapshot = live.clone();
    let outcome = plan_reload(&mut live, &snapshot);
    assert!(outcome.applied.is_empty() && outcome.restart_required.is_empty());
    assert_eq!(live, snapshot);
}
