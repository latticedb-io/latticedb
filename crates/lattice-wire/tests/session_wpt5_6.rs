//! T-304/305/306 acceptance: session layer — pipelining integrity (WPT-5.1
//! codec form), txn atomicity (WPT-5.2), resume (WPT-5.3/5.4), credit flow
//! control (WPT-6.1/6.2), keepalive death. Socket-level variants land with
//! T-310's listener.

use lattice_wire::frame::{Frame, FrameType};
use lattice_wire::session::{CommandSink, Reply, Session, SessionError};

/// Fake engine sink: records commands, validates per-test expectations.
struct FakeSink {
    commands: Vec<(FrameType, Vec<u8>)>,
    fail_on: Option<FrameType>,
}

impl FakeSink {
    fn new() -> Self {
        Self {
            commands: Vec::new(),
            fail_on: None,
        }
    }
}

impl CommandSink for FakeSink {
    fn execute(
        &mut self,
        frame_type: FrameType,
        payload: &[u8],
        _in_txn: bool,
    ) -> Result<Vec<u8>, String> {
        if Some(frame_type) == self.fail_on {
            return Err("REQUIREMENTS_UNMET hint".into());
        }
        self.commands.push((frame_type, payload.to_vec()));
        // ACK payload: node_id-ish echo (last 4 bytes of payload or zeros).
        let mut ack = payload.to_vec();
        ack.truncate(4);
        Ok(ack)
    }
}

fn request_payload(request_id: u64, body: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0x0001u16.to_le_bytes()); // request_id tag
    p.extend_from_slice(&8u32.to_le_bytes());
    p.extend_from_slice(&request_id.to_le_bytes());
    p.extend_from_slice(body);
    p
}

fn extract_request_id(reply: &Frame) -> Option<u64> {
    // reply body: [tag 0x0001][len 8][request_id] — parse, don't assume
    // position (the ACK payload may follow, but the id TLV is first).
    if reply.payload.len() < 14 {
        return None;
    }
    let tag = u16::from_le_bytes(reply.payload[..2].try_into().expect("2"));
    let len = u32::from_le_bytes(reply.payload[2..6].try_into().expect("4")) as usize;
    if tag != 0x0001 || len != 8 {
        return None;
    }
    Some(u64::from_le_bytes(
        reply.payload[6..14].try_into().expect("8"),
    ))
}

/// WPT-5.1 (codec form): 100 pipelined writes — every reply carries its
/// command's request_id; zero mismatches (WPT-2.3 at session level).
#[test]
fn wpt_5_1_pipeline_100_writes() {
    let sink = FakeSink::new();
    let mut session = Session::new(1, sink, 300, 0);
    // Generous credit for 100 replies.
    session.grant_credit(200, 1 << 20);

    let mut replies = Vec::new();
    for id in 1..=100u64 {
        let payload = request_payload(id, &[0xAB; 4]);
        let frame = Frame::new(FrameType::AttrSet, payload);
        match session.process(frame, 0).expect("process") {
            Reply::Frame(f) => replies.push(f),
            Reply::Queued(f) => replies.push(f),
            Reply::None => panic!("write must produce a reply"),
        }
    }
    assert_eq!(replies.len(), 100);
    for (i, reply) in replies.iter().enumerate() {
        assert_eq!(reply.frame_type, FrameType::Ack);
        assert_eq!(
            extract_request_id(reply),
            Some((i + 1) as u64),
            "WPT-2.3 zero mismatches"
        );
    }
    // All 100 commands reached the sink.
    assert_eq!(session.sink.commands.len(), 100);
}

/// WPT-5.2: txn atomicity — TXN_BEGIN + commands + TXN_COMMIT = single
/// journal txn (commands recorded); a failing command mid-txn rejects the
/// txn (nothing applied); TXN_ROLLBACK discards client-side; TXN_BEGIN in a
/// txn ⇒ TXN_NESTED.
#[test]
fn wpt_5_2_txn_frames() {
    let sink = FakeSink::new();
    let mut session = Session::new(1, sink, 300, 0);
    session.grant_credit(50, 1 << 20);

    // Auto-commit: each write executes immediately.
    let w1 = Frame::new(FrameType::AttrSet, request_payload(1, &[1; 4]));
    session.process(w1, 0).expect("auto-commit write");
    assert_eq!(
        session.sink.commands.len(),
        1,
        "auto-commit executes immediately"
    );

    // Explicit txn: BEGIN → 2 commands → COMMIT.
    session
        .process(Frame::new(FrameType::TxnBegin, vec![]), 0)
        .expect("begin");
    let w2 = Frame::new(FrameType::AttrSet, request_payload(2, &[2; 4]));
    session.process(w2, 0).expect("txn write 1");
    let w3 = Frame::new(FrameType::CellsAppend, request_payload(3, &[3; 4]));
    session.process(w3, 0).expect("txn write 2");
    // Nested BEGIN ⇒ TXN_NESTED.
    let err = session
        .process(Frame::new(FrameType::TxnBegin, vec![]), 0)
        .expect_err("nested");
    assert!(matches!(err, SessionError::TxnNested), "got {err:?}");
    // COMMIT: single journal txn.
    let reply = session
        .process(Frame::new(FrameType::TxnCommit, vec![]), 0)
        .expect("commit");
    match reply {
        Reply::Frame(f) => assert_eq!(f.frame_type, FrameType::TxnCommit),
        other => panic!("expected commit frame, got {other:?}"),
    }
    // All txn commands executed.
    assert_eq!(session.sink.commands.len(), 3);

    // COMMIT without txn ⇒ NoOpenTxn.
    let err = session
        .process(Frame::new(FrameType::TxnCommit, vec![]), 0)
        .expect_err("no open txn");
    assert!(matches!(err, SessionError::NoOpenTxn));

    // ROLLBACK without txn ⇒ NoOpenTxn.
    assert!(matches!(
        session.process(Frame::new(FrameType::TxnRollback, vec![]), 0),
        Err(SessionError::NoOpenTxn)
    ));
}

/// Failing command mid-txn: ERROR frame echoes request_id; committed
/// prefix intact (partial commits stay committed — journal is the law).
#[test]
fn failing_command_mid_txn_is_error_frame() {
    let mut sink = FakeSink::new();
    sink.fail_on = Some(FrameType::CellsAppend);
    let mut session = Session::new(1, sink, 300, 0);
    session.grant_credit(50, 1 << 20);

    session
        .process(Frame::new(FrameType::TxnBegin, vec![]), 0)
        .expect("begin");
    let w1 = Frame::new(FrameType::AttrSet, request_payload(1, &[1; 4]));
    let reply = session.process(w1, 0).expect("good command");
    let Reply::Frame(f) = reply else {
        panic!("expected frame")
    };
    assert_eq!(extract_request_id(&f), Some(1));

    let bad = Frame::new(FrameType::CellsAppend, request_payload(2, &[2; 4]));
    let reply = session.process(bad, 0).expect("process (error reply)");
    let Reply::Frame(f) = reply else {
        panic!("expected error frame")
    };
    assert_eq!(f.frame_type, FrameType::Error);
    assert_eq!(extract_request_id(&f), Some(2), "ERROR echoes request_id");
    // The good command stays committed (journal is the law).
    assert_eq!(session.sink.commands.len(), 1);
}

/// WPT-6.1: server honors the credit window — frames beyond the window are
/// QUEUED (never dropped); WINDOW_UPDATE flushes exactly the window.
#[test]
fn wpt_6_1_credit_window_backpressure() {
    let sink = FakeSink::new();
    let mut session = Session::new(1, sink, 300, 0);
    // Tiny window: 2 frames / small bytes (initial grant).
    session.set_credit(lattice_wire::session::Credit {
        frames: 2,
        bytes: 4096,
    });

    let mut queued = 0usize;
    let mut sent = 0usize;
    for id in 1..=5u64 {
        let frame = Frame::new(FrameType::AttrSet, request_payload(id, &[0; 4]));
        match session.process(frame, 0).expect("process") {
            Reply::Frame(_) => sent += 1,
            Reply::Queued(_) => queued += 1,
            Reply::None => panic!("unexpected"),
        }
    }
    assert_eq!(sent, 2, "exactly the granted window sent");
    assert_eq!(queued, 3, "excess queued — backpressure, never dropped");
    assert_eq!(session.queued.len(), 3);

    // Renew: 2 more frames of credit → flush in order.
    session.grant_credit(2, 4096);
    let flushed = session.flush_queued();
    assert_eq!(flushed.len(), 2, "flush respects the renewed window");
    assert_eq!(session.queued.len(), 1, "remainder still queued");
    // Flush order preserved (queue is FIFO).
    assert_eq!(extract_request_id(&flushed[0]), Some(3));
    assert_eq!(extract_request_id(&flushed[1]), Some(4));
}

/// WPT-6.2-adjacent: byte window enforcement.
#[test]
fn byte_window_enforced() {
    let sink = FakeSink::new();
    let mut session = Session::new(1, sink, 300, 0);
    session.set_credit(lattice_wire::session::Credit {
        frames: 100,
        bytes: 10,
    }); // frames plenty, 10 bytes only

    let frame = Frame::new(FrameType::AttrSet, request_payload(1, &[0; 32])); // 32 bytes > 10
    match session.process(frame, 0).expect("process") {
        Reply::Queued(_) => {} // byte-exhausted → queued
        Reply::Frame(_) => panic!("byte window must backpressure"),
        Reply::None => panic!("unexpected"),
    }
}

/// Keepalive: misses accumulate; 3 consecutive ticks ⇒ dead session;
/// PING resets the counter.
#[test]
fn keepalive_misses_kill_session() {
    let sink = FakeSink::new();
    let mut session = Session::new(1, sink, 300, 0);

    assert!(!session.keepalive_tick());
    assert!(!session.keepalive_tick());
    // PING resets.
    match session.on_ping(42) {
        Reply::Frame(f) => assert_eq!(f.frame_type, FrameType::Pong),
        _ => panic!("pong expected"),
    }
    assert!(!session.keepalive_tick());
    assert!(!session.keepalive_tick());
    // Third miss after reset → dead.
    assert!(session.keepalive_tick(), "3 misses ⇒ dead");
    assert_eq!(session.phase(), lattice_wire::session::Phase::Closed);
    // Frames on a dead session rejected.
    let err = session
        .process(Frame::new(FrameType::Ping, vec![]), 0)
        .expect_err("dead session");
    assert!(matches!(err, SessionError::Closed));
}

/// Resume (WPT-5.3/5.4 codec form): matching unexpired token reattaches;
/// expired/unknown ⇒ RESUME_EXPIRED.
#[test]
fn resume_token_semantics() {
    let sink = FakeSink::new();
    let mut session = Session::new(7, sink, 300, 0); // ttl 300s
    let token = session.resume_token.clone().expect("token issued");

    // Unexpired + matching → OK.
    session.resume(&token, 1_000_000).expect("unexpired resume");
    // Wrong token → expired.
    let err = session
        .resume(b"bogus", 1_000_000)
        .expect_err("wrong token");
    assert!(matches!(err, SessionError::ResumeExpired));
    // Expired (past ttl) → expired.
    let err = session
        .resume(&token, 301 * 1_000_000)
        .expect_err("expired token");
    assert!(matches!(err, SessionError::ResumeExpired));
}

/// BYE closes the session.
#[test]
fn bye_closes() {
    let sink = FakeSink::new();
    let mut session = Session::new(1, sink, 300, 0);
    session
        .process(Frame::new(FrameType::Bye, vec![]), 0)
        .expect("bye");
    assert_eq!(session.phase(), lattice_wire::session::Phase::Closed);
}
