//! T-309 acceptance: watches (SPEC-02 §9) — WPT-9.1 epoch ordering under
//! 500 concurrent mutations (no loss vs journal-derived set) · WPT-9.2
//! kind filter correctness · WPT-9.3 5-subscriber fairness at 1k events.

use lattice_wire::frame::FrameType;
use lattice_wire::watches::{
    EventKind, MutationEvent, WatchDispatch, WatchSource, WatchTable, watch_codes, watch_tags,
};
use std::collections::BTreeMap;

/// Fake engine source: journal-derived events (epoch-ascending, P7).
struct FakeSource {
    epoch: u64,
    events: Vec<MutationEvent>,
    /// parent chain for subtree checks.
    parents: BTreeMap<u32, u32>,
}

impl FakeSource {
    fn new() -> Self {
        Self {
            epoch: 0,
            events: Vec::new(),
            parents: BTreeMap::new(),
        }
    }

    /// Append a mutation (the engine guarantees ascending epochs).
    fn mutate(&mut self, kind: EventKind, node: u32) {
        self.epoch += 1;
        self.events.push(MutationEvent {
            epoch: self.epoch,
            kind,
            node,
            delta: vec![kind as u8, 0xAA],
        });
    }
}

impl WatchSource for FakeSource {
    fn current_epoch(&self) -> u64 {
        self.epoch
    }
    fn events_since(&self, after_epoch: u64, max: usize) -> Vec<MutationEvent> {
        self.events
            .iter()
            .filter(|e| e.epoch > after_epoch)
            .take(max)
            .cloned()
            .collect()
    }
    fn in_subtree(&self, node: u32, root: u32) -> bool {
        if node == root {
            return true;
        }
        let mut cur = node;
        for _ in 0..64 {
            match self.parents.get(&cur) {
                Some(p) if *p == root => return true,
                Some(p) => cur = *p,
                None => return false,
            }
        }
        false
    }
}

fn sub_payload(
    subtree: Option<u32>,
    kinds: Option<Vec<u8>>,
    min_epoch: Option<u64>,
) -> BTreeMap<u16, Vec<u8>> {
    let mut m = BTreeMap::new();
    if let Some(root) = subtree {
        m.insert(watch_tags::SUBTREE, root.to_le_bytes().to_vec());
    }
    if let Some(k) = kinds {
        m.insert(watch_tags::KINDS, k);
    }
    if let Some(e) = min_epoch {
        m.insert(watch_tags::MIN_EPOCH, e.to_le_bytes().to_vec());
    }
    m
}

fn drain_all(table: &mut WatchTable, watch_id: u64) -> Vec<MutationEvent> {
    let mut out = Vec::new();
    loop {
        let batch = table.drain(watch_id, 1_000).expect("drain");
        if batch.is_empty() {
            break;
        }
        out.extend(batch);
    }
    out
}

/// WPT-9.1: 500 mutations on a watched subtree → events strictly
/// epoch-ascending, no loss (compare with the journal-derived set).
#[test]
fn wpt_9_1_ordering_and_no_loss_under_concurrent_mutations() {
    let mut source = FakeSource::new();
    source.parents.insert(2, 1); // node 2 ⊆ subtree 1
    source.parents.insert(3, 2); // node 3 ⊆ subtree 1
    let mut table = WatchTable::new();
    let payload = sub_payload(Some(1), None, None);
    let ack = WatchDispatch::sub(&mut table, &source, &payload).expect("sub");
    let watch_id = u64::from_le_bytes(ack[6..14].try_into().expect("watch id"));

    // 500 mutations on the watched subtree + 500 outside (noise).
    for i in 0..500u32 {
        source.mutate(EventKind::Attr, 1 + (i % 3)); // nodes 1..3 ⊆ subtree
        source.mutate(EventKind::Attr, 50 + (i % 3)); // nodes 50..52 outside
    }
    assert_eq!(source.events.len(), 1000);

    // Poll + drain in small windows (simulates concurrent arrival).
    let mut delivered = Vec::new();
    loop {
        table.poll(&source, 50);
        let batch = table.drain(watch_id, 37).expect("drain");
        delivered.extend(batch);
        if table.pending_of(watch_id) == 0 {
            // keep polling until the source is exhausted
            let all: Vec<MutationEvent> = source
                .events
                .iter()
                .filter(|e| e.node <= 3)
                .cloned()
                .collect();
            if delivered.len() >= all.len() {
                break;
            }
        }
    }

    // No loss: delivered == the subtree-filtered journal-derived set.
    let expected: Vec<u64> = source
        .events
        .iter()
        .filter(|e| e.node >= 1 && e.node <= 3)
        .map(|e| e.epoch)
        .collect();
    let got: Vec<u64> = delivered.iter().map(|e| e.epoch).collect();
    assert_eq!(
        got.len(),
        expected.len(),
        "no loss: {got:?} vs {expected:?}"
    );
    assert_eq!(got, expected, "same events as the journal-derived set");
    // Strictly epoch-ascending.
    assert!(
        delivered.windows(2).all(|w| w[0].epoch < w[1].epoch),
        "strictly ascending"
    );
    // No dup/loss at the payload level either.
    let payload_epochs: Vec<u64> = delivered
        .iter()
        .map(WatchTable::event_payload)
        .map(|p| u64::from_le_bytes(p[6..14].try_into().expect("epoch")))
        .collect();
    assert_eq!(payload_epochs, expected);
}

/// WPT-9.2: kinds=[state] subscription delivers ONLY state events;
/// attr mutations absent.
#[test]
fn wpt_9_2_kind_filter_server_side() {
    let mut source = FakeSource::new();
    let mut table = WatchTable::new();
    let payload = sub_payload(None, Some(vec![EventKind::State as u8]), None);
    let ack = WatchDispatch::sub(&mut table, &source, &payload).expect("sub");
    let watch_id = u64::from_le_bytes(ack[6..14].try_into().expect("watch id"));

    for i in 0..10u32 {
        source.mutate(EventKind::Attr, i);
        source.mutate(EventKind::State, i);
        source.mutate(EventKind::Timer, i);
    }
    table.poll(&source, 100);
    let delivered = drain_all(&mut table, watch_id);

    assert_eq!(delivered.len(), 10, "only the 10 state events");
    assert!(
        delivered.iter().all(|e| e.kind == EventKind::State),
        "attr/timer mutations absent"
    );
}

/// WPT-9.3: 5 subscribers, 1k events — each receives all 1k, order
/// preserved, no cross-starvation (drains interleave with small budgets).
#[test]
fn wpt_9_3_multi_client_fairness() {
    let mut source = FakeSource::new();
    let mut table = WatchTable::new();
    let mut ids = Vec::new();
    for _ in 0..5 {
        let ack =
            WatchDispatch::sub(&mut table, &source, &sub_payload(None, None, None)).expect("sub");
        ids.push(u64::from_le_bytes(ack[6..14].try_into().expect("watch id")));
    }

    for i in 0..1000u32 {
        source.mutate(EventKind::Action, i % 7);
    }
    assert_eq!(table.sub_count(), 5);

    // Interleaved drains with tiny budgets: one eager sub cannot starve
    // the others — drain(budget=3) per sub per round, poll between rounds.
    let mut delivered: Vec<Vec<MutationEvent>> = vec![Vec::new(); 5];
    loop {
        table.poll(&source, 200);
        let mut any = false;
        for (i, id) in ids.iter().enumerate() {
            let batch = table.drain(*id, 3).expect("drain");
            if !batch.is_empty() {
                any = true;
            }
            delivered[i].extend(batch);
        }
        if !any && table.pending_of(ids[0]) == 0 && delivered.iter().all(|d| !d.is_empty()) {
            let all_done = delivered.iter().all(|d| d.len() >= 1000);
            if all_done {
                break;
            }
        }
        if delivered.iter().all(|d| d.len() >= 1000) {
            break;
        }
    }

    let first: Vec<u64> = delivered[0].iter().map(|e| e.epoch).collect();
    for (i, d) in delivered.iter().enumerate() {
        assert_eq!(d.len(), 1000, "sub {i} received all 1k events");
        let epochs: Vec<u64> = d.iter().map(|e| e.epoch).collect();
        assert_eq!(epochs, first, "sub {i} order preserved and identical");
        assert!(
            epochs.windows(2).all(|w| w[0] < w[1]),
            "sub {i} strictly ascending"
        );
    }
}

/// min_epoch: events below the floor are skipped; delivery starts there.
#[test]
fn min_epoch_floor() {
    let mut source = FakeSource::new();
    let mut table = WatchTable::new();
    // 10 events happen BEFORE the subscription.
    for _ in 0..10 {
        source.mutate(EventKind::Attr, 1);
    }
    let payload = sub_payload(None, None, Some(source.epoch + 1)); // floor above current
    let ack = WatchDispatch::sub(&mut table, &source, &payload).expect("sub");
    let watch_id = u64::from_le_bytes(ack[6..14].try_into().expect("watch id"));

    for _ in 0..5 {
        source.mutate(EventKind::Attr, 1);
    }
    table.poll(&source, 100);
    let delivered = drain_all(&mut table, watch_id);
    assert_eq!(delivered.len(), 5, "pre-floor events skipped");
    assert!(delivered.iter().all(|e| e.epoch > 10), "floor respected");
}

/// Subtree filter: only nodes within the watched subtree are delivered.
#[test]
fn subtree_filter_uses_engine_ancestry() {
    let mut source = FakeSource::new();
    source.parents.insert(2, 1);
    source.parents.insert(7, 5); // node 7 ⊆ subtree 5, not subtree 1
    let mut table = WatchTable::new();
    let payload = sub_payload(Some(1), None, None);
    let ack = WatchDispatch::sub(&mut table, &source, &payload).expect("sub");
    let watch_id = u64::from_le_bytes(ack[6..14].try_into().expect("watch id"));

    source.mutate(EventKind::Attr, 2); // inside (child of 1)
    source.mutate(EventKind::Attr, 7); // outside (child of 5)
    source.mutate(EventKind::Attr, 1); // the root itself
    source.mutate(EventKind::Attr, 9); // unrelated
    table.poll(&source, 100);
    let delivered = drain_all(&mut table, watch_id);
    let nodes: Vec<u32> = delivered.iter().map(|e| e.node).collect();
    assert_eq!(nodes, vec![2, 1], "subtree 1 only: {nodes:?}");
}

/// WATCH_UNSUB stops delivery; unknown id → WATCH_NOT_FOUND.
#[test]
fn unsub_stops_and_unknown_id_errors() {
    let mut source = FakeSource::new();
    let mut table = WatchTable::new();
    let ack = WatchDispatch::sub(&mut table, &source, &sub_payload(None, None, None)).expect("sub");
    let watch_id = u64::from_le_bytes(ack[6..14].try_into().expect("watch id"));

    source.mutate(EventKind::Attr, 1);
    table.poll(&source, 10);
    let mut unsub_payload = BTreeMap::new();
    unsub_payload.insert(watch_tags::WATCH_ID, watch_id.to_le_bytes().to_vec());
    WatchDispatch::dispatch(&mut table, &source, FrameType::WatchUnsub, &unsub_payload)
        .expect("unsub");
    assert_eq!(table.sub_count(), 0);

    source.mutate(EventKind::Attr, 1);
    table.poll(&source, 10);
    let err = table.drain(watch_id, 10).expect_err("unsubscribed");
    assert_eq!(err.code, watch_codes::WATCH_NOT_FOUND);

    // Unknown id at UNSUB → same code.
    unsub_payload.insert(watch_tags::WATCH_ID, 999u64.to_le_bytes().to_vec());
    let err2 = WatchDispatch::dispatch(&mut table, &source, FrameType::WatchUnsub, &unsub_payload)
        .expect_err("unknown");
    assert_eq!(err2.code, watch_codes::WATCH_NOT_FOUND);
}

/// Hint law: unknown event kind byte → BAD_WATCH_PARAMS listing known kinds.
#[test]
fn unknown_kind_hint_law() {
    let source = FakeSource::new();
    let mut table = WatchTable::new();
    let payload = sub_payload(None, Some(vec![0, 9]), None); // 9 unknown
    let err = WatchDispatch::sub(&mut table, &source, &payload).expect_err("unknown kind");
    assert_eq!(err.code, watch_codes::BAD_WATCH_PARAMS);
    assert!(err.hint.contains("known kinds"), "hint law: {}", err.hint);
}

/// ACK shape: watch_id + current_epoch TLVs (sub sees the world at sub time).
#[test]
fn sub_ack_carries_watch_id_and_epoch() {
    let mut source = FakeSource::new();
    for _ in 0..3 {
        source.mutate(EventKind::Attr, 1);
    }
    let mut table = WatchTable::new();
    let ack = WatchDispatch::sub(&mut table, &source, &sub_payload(None, None, None)).expect("sub");
    // [WATCH_ID tag 2][len 4][u64][CURRENT_EPOCH tag 2][len 4][u64]
    assert_eq!(
        u16::from_le_bytes(ack[..2].try_into().expect("tag")),
        watch_tags::WATCH_ID
    );
    let watch_id = u64::from_le_bytes(ack[6..14].try_into().expect("id"));
    assert_eq!(watch_id, 1, "ids start at 1");
    assert_eq!(
        u16::from_le_bytes(ack[14..16].try_into().expect("tag")),
        watch_tags::CURRENT_EPOCH
    );
    let current = u64::from_le_bytes(ack[20..28].try_into().expect("epoch"));
    assert_eq!(current, 3, "ACK carries the current epoch");
}

/// Event payload TLV round-trip (stable shapes per kind).
#[test]
fn event_payload_tlv_shape() {
    let ev = MutationEvent {
        epoch: 42,
        kind: EventKind::Enabled,
        node: 7,
        delta: vec![1, 2, 3],
    };
    let payload = WatchTable::event_payload(&ev);
    // epoch
    assert_eq!(
        u16::from_le_bytes(payload[..2].try_into().expect("tag")),
        watch_tags::EVENT_EPOCH
    );
    assert_eq!(
        u64::from_le_bytes(payload[6..14].try_into().expect("epoch")),
        42
    );
    // kind: tag@14, len@16, val@20
    assert_eq!(
        u16::from_le_bytes(payload[14..16].try_into().expect("tag")),
        watch_tags::EVENT_KIND
    );
    assert_eq!(payload[20], EventKind::Enabled as u8);
    // node: tag@21, len@23, val@27
    assert_eq!(
        u16::from_le_bytes(payload[21..23].try_into().expect("tag")),
        watch_tags::EVENT_NODE
    );
    assert_eq!(
        u32::from_le_bytes(payload[27..31].try_into().expect("node")),
        7
    );
    // delta: tag@31, len@33, val@37
    assert_eq!(
        u16::from_le_bytes(payload[31..33].try_into().expect("tag")),
        watch_tags::EVENT_DELTA
    );
    let dlen = u32::from_le_bytes(payload[33..37].try_into().expect("dlen")) as usize;
    assert_eq!(&payload[37..37 + dlen], &[1, 2, 3]);
}
