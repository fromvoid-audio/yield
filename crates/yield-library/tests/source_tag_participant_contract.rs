//! OPT-2034 synthetic transaction/replay decision fixture, not a production participant.
//! The Known budget and Fence below are model observations, never live owner grants.
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use uuid::Uuid;
use yield_library::operation_journal::{OperationId, ParticipantId};
use yield_library::sample_sources::reconciliation::RootRelativePath;
use yield_library::sample_sources::{SOURCE_CURATION_BATCH_LIMIT, SourceCommandId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Key {
    operation: OperationId,
    participant: ParticipantId,
    command: SourceCommandId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Mutation {
    Rename(String),
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Proposal {
    key: Key,
    binding: String,
    initial_revision: u64,
    old_label: String,
    paths: Vec<String>,
    mutation: Mutation,
}

#[derive(Clone, Copy)]
enum Budget {
    Unknown,
    KnownFixture,
}

struct Fence<'a> {
    binding: &'a str,
    current_profile: bool,
    lease: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Receipt {
    key: Key,
    revision: u64,
    selected: usize,
    changed: usize,
    historical_lease: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum Rejected {
    Invalid,
    StaleProfile,
    ReplacedBinding,
    PayloadConflict,
    StaleRevision,
    ScopeChanged,
    MissingOrLockedTag,
    TargetCollision,
    UnknownAdmission,
    InjectedFailure,
}

fn initialize(connection: &Connection) {
    connection
        .execute_batch(
            "CREATE TABLE state (revision INTEGER NOT NULL);
         INSERT INTO state VALUES (7);
         CREATE TABLE source_definitions (label TEXT PRIMARY KEY, locked INTEGER NOT NULL);
         INSERT INTO source_definitions VALUES ('old',0), ('other',0), ('empty-tag',0), ('locked',1);
         CREATE TABLE assignments (path TEXT PRIMARY KEY, label TEXT NOT NULL);
         INSERT INTO assignments VALUES ('a.wav','old'), ('b.wav','old'), ('unrelated.wav','other');
         CREATE TABLE receipts (key BLOB PRIMARY KEY, proposal BLOB NOT NULL, result BLOB NOT NULL);
         CREATE TABLE definition (label TEXT PRIMARY KEY);
         INSERT INTO definition VALUES ('old');",
        )
        .unwrap();
}

fn proposal() -> Proposal {
    Proposal {
        key: Key {
            operation: OperationId::from_uuid(Uuid::from_u128(1)),
            participant: ParticipantId::from_uuid(Uuid::from_u128(2)),
            command: SourceCommandId::from_uuid(Uuid::from_u128(3)),
        },
        binding: "source/root/database".into(),
        initial_revision: 7,
        old_label: "old".into(),
        paths: vec!["a.wav".into(), "b.wav".into()],
        mutation: Mutation::Rename("new".into()),
    }
}

fn fixture_fence(lease: u64) -> Fence<'static> {
    Fence {
        binding: "source/root/database",
        current_profile: true,
        lease,
    }
}

fn observe(connection: &Connection) -> (u64, Vec<(String, String)>, usize, bool) {
    let revision = connection
        .query_row("SELECT revision FROM state", [], |r| r.get(0))
        .unwrap();
    let rows = connection
        .prepare("SELECT path,label FROM assignments ORDER BY path")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let count = connection
        .query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0))
        .unwrap();
    let definition = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM definition WHERE label='old')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    (revision, rows, count, definition)
}

fn apply(
    connection: &mut Connection,
    proposal: &Proposal,
    fence: &Fence<'_>,
    budget: Budget,
    fail_after_delta: bool,
) -> Result<Receipt, Rejected> {
    let limit = match proposal.mutation {
        Mutation::Rename(_) => SOURCE_CURATION_BATCH_LIMIT / 2,
        Mutation::Delete => SOURCE_CURATION_BATCH_LIMIT,
    };
    if proposal.paths.len() > limit
        || proposal
            .paths
            .iter()
            .any(|p| p.len() > 1024 || RootRelativePath::try_from_path(p.into()).is_err())
        || proposal.paths.windows(2).any(|p| p[0] >= p[1])
        || proposal.old_label.is_empty()
        || proposal.old_label.len() > 1024
        || proposal.old_label.contains('\0')
        || matches!(&proposal.mutation, Mutation::Rename(label) if label.is_empty() || label.len() > 1024 || label.contains('\0') || label == &proposal.old_label)
    {
        return Err(Rejected::Invalid);
    }
    if !fence.current_profile || fence.lease == 0 {
        return Err(Rejected::StaleProfile);
    }
    if proposal.binding != fence.binding {
        return Err(Rejected::ReplacedBinding);
    }
    let key = serde_json::to_vec(&proposal.key).unwrap();
    let bytes = serde_json::to_vec(proposal).unwrap();
    if bytes.len() > 256 * 1024 {
        return Err(Rejected::Invalid);
    }
    let transaction = connection.transaction().unwrap();
    let stored = transaction
        .query_row(
            "SELECT proposal,result FROM receipts WHERE key=?1",
            [&key],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .unwrap();
    if let Some((stored, result)) = stored {
        if stored != bytes {
            return Err(Rejected::PayloadConflict);
        }
        return Ok(serde_json::from_slice(&result).unwrap());
    }
    let revision: u64 = transaction
        .query_row("SELECT revision FROM state", [], |r| r.get(0))
        .unwrap();
    if revision != proposal.initial_revision {
        return Err(Rejected::StaleRevision);
    }
    let locked = transaction
        .query_row(
            "SELECT locked FROM source_definitions WHERE label=?1",
            [&proposal.old_label],
            |r| r.get::<_, bool>(0),
        )
        .optional()
        .unwrap();
    if locked != Some(false) {
        return Err(Rejected::MissingOrLockedTag);
    }
    if let Mutation::Rename(label) = &proposal.mutation {
        let exists: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM source_definitions WHERE label=?1)",
                [label],
                |r| r.get(0),
            )
            .unwrap();
        if exists {
            return Err(Rejected::TargetCollision);
        }
    }
    let paths = transaction
        .prepare("SELECT path FROM assignments WHERE label=?1 ORDER BY path LIMIT ?2")
        .unwrap()
        .query_map(params![proposal.old_label, limit + 1], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    if paths != proposal.paths {
        return Err(Rejected::ScopeChanged);
    }
    if matches!(budget, Budget::Unknown) {
        return Err(Rejected::UnknownAdmission);
    }
    let mut changed = 0;
    for path in &proposal.paths {
        changed += match &proposal.mutation {
            Mutation::Rename(label) => transaction.execute(
                "UPDATE assignments SET label=?1 WHERE path=?2 AND label=?3",
                params![label, path, proposal.old_label],
            ),
            Mutation::Delete => transaction.execute(
                "DELETE FROM assignments WHERE path=?1 AND label=?2",
                params![path, proposal.old_label],
            ),
        }
        .unwrap();
    }
    if fail_after_delta {
        return Err(Rejected::InjectedFailure);
    }
    let receipt = Receipt {
        key: proposal.key.clone(),
        revision: revision + u64::from(changed > 0),
        selected: proposal.paths.len(),
        changed,
        historical_lease: fence.lease,
    };
    transaction
        .execute("UPDATE state SET revision=?1", [receipt.revision])
        .unwrap();
    transaction
        .execute(
            "INSERT INTO receipts VALUES (?1,?2,?3)",
            params![key, bytes, serde_json::to_vec(&receipt).unwrap()],
        )
        .unwrap();
    transaction.commit().unwrap();
    Ok(receipt)
}

#[test]
fn response_loss_and_reopen_replay_original_counts_without_a_new_revision() {
    let temporary = TempDir::new().unwrap();
    let path = temporary.path().join("synthetic-contract.sqlite");
    let mut connection = Connection::open(&path).unwrap();
    initialize(&connection);
    let request = proposal();
    let first = apply(
        &mut connection,
        &request,
        &fixture_fence(9),
        Budget::KnownFixture,
        false,
    )
    .unwrap();
    assert_eq!((first.selected, first.changed, first.revision), (2, 2, 8));
    let committed = observe(&connection);
    assert!(
        committed.3,
        "source success does not delete the profile definition"
    );
    drop(connection); // response lost; reacquire a different synthetic lease on reopen
    let mut reopened = Connection::open(&path).unwrap();
    assert_eq!(
        apply(
            &mut reopened,
            &request,
            &fixture_fence(10),
            Budget::Unknown,
            false
        )
        .unwrap(),
        first
    );
    assert_eq!(observe(&reopened), committed);
    let mut conflict = request.clone();
    conflict.mutation = Mutation::Delete;
    assert_eq!(
        apply(
            &mut reopened,
            &conflict,
            &fixture_fence(10),
            Budget::KnownFixture,
            false
        ),
        Err(Rejected::PayloadConflict)
    );
    assert_eq!(observe(&reopened), committed);
    let mut stale = request;
    stale.key.command = SourceCommandId::from_uuid(Uuid::from_u128(4));
    assert_eq!(
        apply(
            &mut reopened,
            &stale,
            &fixture_fence(10),
            Budget::KnownFixture,
            false
        ),
        Err(Rejected::StaleRevision)
    );
}

#[test]
fn fence_scope_and_unknown_admission_rejections_preserve_rows_and_definition() {
    let mut connection = Connection::open_in_memory().unwrap();
    initialize(&connection);
    let before = observe(&connection);
    let request = proposal();
    assert_eq!(
        apply(
            &mut connection,
            &request,
            &fixture_fence(9),
            Budget::Unknown,
            false
        ),
        Err(Rejected::UnknownAdmission)
    );
    assert_eq!(
        apply(
            &mut connection,
            &request,
            &Fence {
                binding: "replacement",
                current_profile: true,
                lease: 10
            },
            Budget::KnownFixture,
            false
        ),
        Err(Rejected::ReplacedBinding)
    );
    assert_eq!(
        apply(
            &mut connection,
            &request,
            &Fence {
                current_profile: false,
                ..fixture_fence(9)
            },
            Budget::KnownFixture,
            false
        ),
        Err(Rejected::StaleProfile)
    );
    let mut incomplete = request;
    incomplete.paths.pop();
    assert_eq!(
        apply(
            &mut connection,
            &incomplete,
            &fixture_fence(9),
            Budget::KnownFixture,
            false
        ),
        Err(Rejected::ScopeChanged)
    );
    assert_eq!(observe(&connection), before);
}

#[test]
fn failure_between_delta_and_receipt_rolls_back_then_exact_retry_commits() {
    let mut connection = Connection::open_in_memory().unwrap();
    initialize(&connection);
    let before = observe(&connection);
    let mut request = proposal();
    request.mutation = Mutation::Delete;
    assert_eq!(
        apply(
            &mut connection,
            &request,
            &fixture_fence(9),
            Budget::KnownFixture,
            true
        ),
        Err(Rejected::InjectedFailure)
    );
    assert_eq!(observe(&connection), before);
    let result = apply(
        &mut connection,
        &request,
        &fixture_fence(9),
        Budget::KnownFixture,
        false,
    )
    .unwrap();
    assert_eq!(result.changed, 2);
    assert_eq!(
        observe(&connection).1,
        vec![("unrelated.wav".into(), "other".into())]
    );
    assert!(observe(&connection).3);
}

#[test]
fn oversized_scope_is_not_truncated_and_empty_scope_has_an_exact_noop_receipt() {
    let mut connection = Connection::open_in_memory().unwrap();
    initialize(&connection);
    let before = observe(&connection);
    let mut request = proposal();
    request.paths = (0..65).map(|n| format!("{n:03}.wav")).collect();
    assert_eq!(
        apply(
            &mut connection,
            &request,
            &fixture_fence(9),
            Budget::KnownFixture,
            false
        ),
        Err(Rejected::Invalid)
    );
    assert_eq!(observe(&connection), before);
    request.paths.clear();
    request.old_label = "empty-tag".into();
    let receipt = apply(
        &mut connection,
        &request,
        &fixture_fence(9),
        Budget::KnownFixture,
        false,
    )
    .unwrap();
    assert_eq!(
        (receipt.selected, receipt.changed, receipt.revision),
        (0, 0, 7)
    );
    assert_eq!(observe(&connection).0, 7);
    assert_eq!(
        apply(
            &mut connection,
            &request,
            &fixture_fence(9),
            Budget::Unknown,
            false
        )
        .unwrap(),
        receipt
    );
}

#[test]
fn prefix_results_do_not_finalize_definition_without_all_receipts_and_reservations() {
    // Pure coordinator-policy decision; no production hold/checkpoint issuer exists here.
    fn can_finalize(
        required: usize,
        checkpointed: usize,
        durable_reservation: bool,
        frozen_set_current: bool,
    ) -> bool {
        required > 0 && required == checkpointed && durable_reservation && frozen_set_current
    }
    assert!(!can_finalize(2, 1, true, true));
    assert!(!can_finalize(2, 2, false, true));
    assert!(!can_finalize(2, 2, true, false));
    assert!(!can_finalize(0, 0, true, true));
    assert!(can_finalize(2, 2, true, true));
}

#[test]
fn capped_wal_policy_uses_checked_nonzero_bounds_and_rejects_unknown_facts() {
    // Decision arithmetic only, not a live allocation/capacity provider.
    fn peak(page_size: u64, maximum_pages: u64, current_pages: u64) -> Option<u64> {
        if !(512..=65536).contains(&page_size)
            || !page_size.is_power_of_two()
            || maximum_pages < current_pages
            || current_pages == 0
        {
            return None;
        }
        let database = page_size.checked_mul(maximum_pages)?;
        if database > 64 * 1024 * 1024 {
            return None;
        }
        let wal = page_size
            .checked_add(24)?
            .checked_mul(maximum_pages)?
            .checked_add(32)?;
        let wal_index = maximum_pages.checked_mul(16)?.checked_add(32768)?;
        database.checked_add(wal)?.checked_add(wal_index)
    }
    assert_eq!(peak(512, 2, 1), Some(34928));
    assert_eq!(peak(0, 2, 1), None);
    assert_eq!(peak(513, 2, 1), None);
    assert_eq!(peak(4096, u64::MAX, 1), None);
    assert_eq!(peak(4096, 1, 2), None);
    assert_eq!(peak(4096, 16385, 1), None);
}

#[test]
fn sqlite_fixture_page_cap_rejects_growth_and_preserves_prior_definition() {
    let temporary = TempDir::new().unwrap();
    let mut connection = Connection::open(temporary.path().join("capped-model.sqlite")).unwrap();
    initialize(&connection);
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA cache_spill=OFF; PRAGMA temp_store=MEMORY; PRAGMA wal_autocheckpoint=0;").unwrap();
    let current: u64 = connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    connection
        .pragma_update(None, "max_page_count", current + 1)
        .unwrap();
    let before = observe(&connection);
    let transaction = connection.transaction().unwrap();
    transaction
        .execute("UPDATE assignments SET label='new' WHERE label='old'", [])
        .unwrap();
    assert!(
        transaction
            .execute(
                "INSERT INTO receipts VALUES (?1,?2,?3)",
                params![b"key", vec![0_u8; 2 * 1024 * 1024], b"receipt"]
            )
            .is_err()
    );
    drop(transaction);
    assert_eq!(
        observe(&connection),
        before,
        "delta and definition survive a capped allocation failure"
    );
}

#[test]
fn missing_locked_and_existing_target_tags_reject_before_delta() {
    let mut connection = Connection::open_in_memory().unwrap();
    initialize(&connection);
    let before = observe(&connection);
    for label in ["missing", "locked"] {
        let mut request = proposal();
        request.old_label = label.into();
        request.paths.clear();
        assert_eq!(
            apply(
                &mut connection,
                &request,
                &fixture_fence(9),
                Budget::KnownFixture,
                false
            ),
            Err(Rejected::MissingOrLockedTag)
        );
        assert_eq!(observe(&connection), before);
    }
    let mut request = proposal();
    request.mutation = Mutation::Rename("other".into());
    assert_eq!(
        apply(
            &mut connection,
            &request,
            &fixture_fence(9),
            Budget::KnownFixture,
            false
        ),
        Err(Rejected::TargetCollision)
    );
    assert_eq!(observe(&connection), before);
}
