//! Hot-reload classification (SPEC-02 §10; WPT-10.6).
//!
//! Safe class (applies live): keepalive interval/misses, credit windows,
//! failure cooldown, metrics interval, log level, slow-cursor trace threshold.
//! Everything else is restart-required: listed by [`plan_reload`] but never
//! applied to the live config.

use super::HostConfig;

/// Result of comparing a candidate config against the live one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadOutcome {
    /// Keys applied to the live config now (safe class).
    pub applied: Vec<String>,
    /// Keys that differ but require a restart — NOT applied.
    pub restart_required: Vec<String>,
}

/// The documented safe class. Section/key pairs only; anything not listed
/// here is restart-required.
fn is_hot(section: &str, key: &str) -> bool {
    matches!(
        (section, key),
        ("network", "credit_frames")
            | ("network", "credit_bytes")
            | ("network", "keepalive_interval_ms")
            | ("network", "keepalive_misses")
            | ("auth", "failure_cooldown_ms")
            | ("observability", "metrics_interval_ms")
            | ("observability", "log_level")
            | ("observability", "slow_cursor_trace_ms")
    )
}

/// Compare `next` against `live`; apply the safe class in place, report the
/// restart-required differences. Deterministic order = struct field order.
pub fn plan_reload(live: &mut HostConfig, next: &HostConfig) -> ReloadOutcome {
    let mut out = ReloadOutcome {
        applied: Vec::new(),
        restart_required: Vec::new(),
    };

    macro_rules! f {
        ($section:literal, $key:literal, $live:expr, $next:expr) => {
            if $live != $next {
                if is_hot($section, $key) {
                    $live = $next.clone();
                    out.applied.push(format!("{}.{}", $section, $key));
                } else {
                    out.restart_required.push(format!("{}.{}", $section, $key));
                }
            }
        };
    }

    let (ln, nn) = (&mut live.network, &next.network);
    f!(
        "network",
        "max_frame_bytes",
        ln.max_frame_bytes,
        nn.max_frame_bytes
    );
    f!(
        "network",
        "credit_frames",
        ln.credit_frames,
        nn.credit_frames
    );
    f!("network", "credit_bytes", ln.credit_bytes, nn.credit_bytes);
    f!(
        "network",
        "keepalive_interval_ms",
        ln.keepalive_interval_ms,
        nn.keepalive_interval_ms
    );
    f!(
        "network",
        "keepalive_misses",
        ln.keepalive_misses,
        nn.keepalive_misses
    );
    f!("network", "resume_ttl_s", ln.resume_ttl_s, nn.resume_ttl_s);
    f!(
        "network",
        "cursor_idle_ttl_s",
        ln.cursor_idle_ttl_s,
        nn.cursor_idle_ttl_s
    );
    f!(
        "network",
        "session_cap_per_subject",
        ln.session_cap_per_subject,
        nn.session_cap_per_subject
    );
    f!(
        "network",
        "listen_backlog",
        ln.listen_backlog,
        nn.listen_backlog
    );

    let (la, na) = (&mut live.auth, &next.auth);
    f!(
        "auth",
        "policy_table_path",
        la.policy_table_path,
        na.policy_table_path
    );
    f!("auth", "verifier_path", la.verifier_path, na.verifier_path);
    f!(
        "auth",
        "failure_cooldown_ms",
        la.failure_cooldown_ms,
        na.failure_cooldown_ms
    );
    f!(
        "auth",
        "identity_map_path",
        la.identity_map_path,
        na.identity_map_path
    );

    let (le, ne) = (&mut live.engine, &next.engine);
    f!(
        "engine",
        "worklist_batch_depth",
        le.worklist_batch_depth,
        ne.worklist_batch_depth
    );
    f!(
        "engine",
        "timer_wheel_tick_ms",
        le.timer_wheel_tick_ms,
        ne.timer_wheel_tick_ms
    );
    f!(
        "engine",
        "stall_grace_ms",
        le.stall_grace_ms,
        ne.stall_grace_ms
    );
    f!(
        "engine",
        "propagation_queue_cap",
        le.propagation_queue_cap,
        ne.propagation_queue_cap
    );

    let (ls, ns) = (&mut live.storage, &next.storage);
    f!(
        "storage",
        "group_commit_window_ms",
        ls.group_commit_window_ms,
        ns.group_commit_window_ms
    );
    f!("storage", "sync_method", ls.sync_method, ns.sync_method);
    f!(
        "storage",
        "crc_block_bytes",
        ls.crc_block_bytes,
        ns.crc_block_bytes
    );
    f!(
        "storage",
        "part_target_bytes",
        ls.part_target_bytes,
        ns.part_target_bytes
    );
    f!(
        "storage",
        "merge_max_parts",
        ls.merge_max_parts,
        ns.merge_max_parts
    );
    f!(
        "storage",
        "merge_staging_bytes",
        ls.merge_staging_bytes,
        ns.merge_staging_bytes
    );
    f!(
        "storage",
        "arena_structure_bytes",
        ls.arena_structure_bytes,
        ns.arena_structure_bytes
    );
    f!(
        "storage",
        "arena_parts_bytes",
        ls.arena_parts_bytes,
        ns.arena_parts_bytes
    );
    f!(
        "storage",
        "arena_sessions_bytes",
        ls.arena_sessions_bytes,
        ns.arena_sessions_bytes
    );

    let (lr, nr) = (&mut live.resources, &next.resources);
    f!(
        "resources",
        "worker_threads",
        lr.worker_threads,
        nr.worker_threads
    );
    f!(
        "resources",
        "sequencer_queue_depth",
        lr.sequencer_queue_depth,
        nr.sequencer_queue_depth
    );
    f!(
        "resources",
        "merge_pool_threads",
        lr.merge_pool_threads,
        nr.merge_pool_threads
    );
    f!(
        "resources.memory_budget_shares",
        "structure",
        lr.memory_budget_shares.structure,
        nr.memory_budget_shares.structure
    );
    f!(
        "resources.memory_budget_shares",
        "parts",
        lr.memory_budget_shares.parts,
        nr.memory_budget_shares.parts
    );
    f!(
        "resources.memory_budget_shares",
        "sessions",
        lr.memory_budget_shares.sessions,
        nr.memory_budget_shares.sessions
    );
    f!(
        "resources.allocator",
        "backend",
        lr.allocator.backend,
        nr.allocator.backend
    );
    f!(
        "resources.allocator",
        "purge_delay_ms",
        lr.allocator.purge_delay_ms,
        nr.allocator.purge_delay_ms
    );
    f!(
        "resources.allocator",
        "allow_thp",
        lr.allocator.allow_thp,
        nr.allocator.allow_thp
    );
    f!(
        "resources.allocator",
        "huge_pages_gib",
        lr.allocator.huge_pages_gib,
        nr.allocator.huge_pages_gib
    );
    f!(
        "resources.allocator",
        "eager_commit",
        lr.allocator.eager_commit,
        nr.allocator.eager_commit
    );

    let (lo, no) = (&mut live.observability, &next.observability);
    f!(
        "observability",
        "metrics_interval_ms",
        lo.metrics_interval_ms,
        no.metrics_interval_ms
    );
    f!("observability", "log_level", lo.log_level, no.log_level);
    f!(
        "observability",
        "slow_cursor_trace_ms",
        lo.slow_cursor_trace_ms,
        no.slow_cursor_trace_ms
    );

    out
}
