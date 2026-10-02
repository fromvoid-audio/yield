use super::*;

#[test]
fn unavailable_after_measurement_and_sql_failure_preserve_evidence() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let dir = tempfile::tempdir().unwrap();
    let writer = database(&dir.path().join("db.sqlite"));
    let seen_checkpoint = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = seen_checkpoint.clone();
    writer.authorizer(Some(move |context: AuthContext<'_>| match context.action {
        AuthAction::Pragma { pragma_name, .. } if pragma_name == "wal_checkpoint" => {
            seen.store(true, std::sync::atomic::Ordering::SeqCst);
            Authorization::Allow
        }
        AuthAction::Pragma { pragma_name, .. }
            if pragma_name == "journal_mode" && seen.load(std::sync::atomic::Ordering::SeqCst) =>
        {
            Authorization::Deny
        }
        _ => Authorization::Allow,
    }));
    let Observation::Attempt {
        before,
        after,
        frames,
        ..
    } = CheckpointPolicy::default().observe(&writer, Instant::now(), 1)
    else {
        panic!("expected attempt")
    };
    assert!(before > 0);
    assert!(frames.is_ok());
    assert!(matches!(
        after,
        Err(WalAdmissionError::ObservationUnavailable { .. })
    ));
    writer.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Pragma {
            pragma_name: "wal_checkpoint",
            ..
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }));
    let policy = CheckpointPolicy::default();
    let now = Instant::now();
    let Observation::Attempt {
        before,
        after,
        frames,
        ..
    } = policy.observe(&writer, now, 1)
    else {
        panic!("expected SQL failure")
    };
    assert_eq!(after.unwrap(), before);
    assert!(frames.is_err());
    assert!(matches!(
        policy.observe(&writer, now, 1),
        Observation::Throttled
    ));
    writer.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
}

fn database(path: &std::path::Path) -> Connection {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
        CREATE TABLE payload(value BLOB); INSERT INTO payload VALUES (zeroblob(1024));",
        )
        .unwrap();
    connection
}

#[test]
fn reader_pinned_frames_remain_incomplete_then_catch_up_without_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let writer = database(&path);
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT * FROM payload;")
        .unwrap();
    writer
        .execute("INSERT INTO payload VALUES (zeroblob(65536))", [])
        .unwrap();
    let policy = CheckpointPolicy::default();
    let now = Instant::now();
    let Observation::Attempt {
        before,
        after,
        frames,
        ..
    } = policy.observe(&writer, now, 1)
    else {
        panic!("expected attempt")
    };
    let frames = frames.unwrap();
    assert!(
        !frames.busy,
        "PASSIVE can report busy=0 despite a pinned reader"
    );
    assert!(frames.log > frames.checkpointed);
    assert_eq!(
        after.unwrap(),
        before,
        "PASSIVE does not promise WAL allocation reclamation"
    );
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM payload", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(matches!(
        policy.observe(&writer, now + MIN_INTERVAL - Duration::from_nanos(1), 1),
        Observation::Throttled
    ));
    reader.execute_batch("ROLLBACK").unwrap();
    let Observation::Attempt { frames, .. } = policy.observe(&writer, now + MIN_INTERVAL, 1) else {
        panic!("expected catch-up")
    };
    let frames = frames.unwrap();
    assert_eq!(frames.log, frames.checkpointed);
    assert!(!frames.busy);
    // A new owner has an independent bounded throttle, even at the same pathname.
    assert!(matches!(
        CheckpointPolicy::default().observe(&writer, now, 1),
        Observation::Attempt { .. }
    ));
}

#[test]
fn active_transaction_and_unsupported_mode_never_consume_checkpoint_window() {
    let policy = CheckpointPolicy::default();
    let memory = Connection::open_in_memory().unwrap();
    assert!(matches!(
        policy.observe(&memory, Instant::now(), 1),
        Observation::Unavailable(_)
    ));
    assert!(policy.last_attempt.get().is_none());
    let dir = tempfile::tempdir().unwrap();
    let writer = database(&dir.path().join("db.sqlite"));
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(matches!(
        policy.observe(&writer, Instant::now(), 1),
        Observation::ActiveTransaction
    ));
    assert!(policy.last_attempt.get().is_none());
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(matches!(
        policy.observe(&writer, Instant::now(), u64::MAX),
        Observation::BelowThreshold
    ));
    assert!(policy.last_attempt.get().is_none());
}

#[cfg(unix)]
#[test]
fn checkpoint_measures_retained_wal_after_sidecar_path_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let writer = database(&dir.path().join("db.sqlite"));
    let wal = dir.path().join("db.sqlite-wal");
    let held = dir.path().join("held-wal");
    let bytes = retained_wal_bytes(&writer).unwrap();
    std::fs::rename(&wal, &held).unwrap();
    std::fs::write(&wal, []).unwrap();
    let observed = CheckpointPolicy::default().observe(&writer, Instant::now(), 1);
    std::fs::remove_file(&wal).unwrap();
    std::fs::rename(&held, &wal).unwrap();
    let Observation::Attempt {
        before,
        after,
        frames,
        ..
    } = observed
    else {
        panic!("retained WAL must trigger maintenance")
    };
    assert_eq!(before, bytes);
    assert_eq!(after.unwrap(), bytes);
    assert!(frames.is_ok());
}

#[test]
fn external_writer_reservation_preserves_passive_outcome_and_throttle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let writer = database(&path);
    let external = Connection::open(&path).unwrap();
    external.execute_batch("BEGIN IMMEDIATE").unwrap();
    let policy = CheckpointPolicy::default();
    let now = Instant::now();
    let Observation::Attempt {
        before,
        after,
        frames,
        ..
    } = policy.observe(&writer, now, 1)
    else {
        panic!("expected passive attempt")
    };
    assert_eq!(after.unwrap(), before);
    // SQLite may copy committed frames while another writer holds a reservation. Preserve
    // its actual tuple rather than inventing an external-reader identity or busy diagnosis.
    assert!(frames.is_ok());
    assert!(matches!(
        policy.observe(&writer, now, 1),
        Observation::Throttled
    ));
    external.execute_batch("ROLLBACK").unwrap();
}
