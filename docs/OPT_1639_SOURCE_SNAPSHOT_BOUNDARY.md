# OPT-1639 source migration and snapshot boundary

Status: partial implementation for [OPT-1639](https://linear.app/boostnlvp/issue/OPT-1639). The catalog validates schema compatibility on its retained owner connection after startup maintenance and before operational commands. Operational writer opens require existing compatible schema. `SourceWriter::read_manifest_snapshot_page` now provides one bounded owner-held manifest read shape. Existing `UiRead` and `BackgroundRead` connections remain unqualified as no-write snapshots.

## Current evidence

- `sample_sources/db/open.rs::initialize_source_database` checks compatibility for Operational opens and calls `apply_schema` for Full opens. Operational opens reject missing or stale schema without creating a DB file or repairing objects. The public profile-owned source-write opener and snapshot write-fence reopen use Operational mode. An explicit profile-owned maintenance opener and `open_for_catalog_with_profile_guard` use Full. Catalog startup/retarget Full reopens remain authorized maintenance exceptions.
- `sample_sources/db/open/read_only.rs` opens `SQLITE_OPEN_READ_ONLY` and skips writable schema routines. Its returned `SourceDatabase` has no retained profile guard, source lease, or database-file binding. SQLite flags reject SQL writes through that connection; they do not establish that opening a live WAL database leaves the surrounding files untouched.
- `SourceDatabase::snapshot_to_path_with_write_fence` uses SQLite backup under a source write fence and writes a destination database. It is an export/retarget primitive, not the requested bounded read-only snapshot.
- The source revision writer from OPT-1638 can provide a committed revision cut. A revision number, copied row, or old publication fence remains an observation, not owner authority.
- `SourceWriter::read_manifest_snapshot_page` uses its retained source connection and exact `SourceRevision` claim. It validates schema/owner before the read, checks revision inside one transaction, bounds one keyset page to 64 rows, 16 MiB, and two seconds, then revalidates the owner/revision. Its result carries a weak publication fence. No caller-family routing is implied by the existence of this library entrypoint.

## Chosen ownership boundary

The first qualified snapshot path uses the already-owned source connection on the source owner thread. It opens no second SQLite connection and creates no snapshot file. The owner retains the profile/source lease and database identity for the entire read. Existing path-only `UiRead` and `BackgroundRead` calls remain legacy observations until callers are separately routed or a no-sidecar-write backend is qualified. In particular, `immutable=1` is not an allowed shortcut for a live WAL database.

Only an owner startup maintenance phase may create, migrate, repair, or clean schema. That phase runs while the profile/source capability is held and before the operational queue accepts requests. Operational writer entry points now require the already-current schema; a mismatch or missing required object returns a typed maintenance-required result without DDL, retrying under another role, or changing the source database. The catalog transition preserves the same source ownership across maintenance and operational service. Catalog startup and retarget Full reopens remain explicit maintenance exceptions.

## Snapshot request and result

One request names the retained physical source, expected lifecycle generation, expected `SourceRevision`, a bounded query shape, and an optional keyset continuation. It carries neither a path-only fallback nor mutation authority. The owner validates source/root/database bindings before the read, starts one read transaction on its retained connection, checks the committed revision inside that transaction, performs bounded keyset reads, and rechecks the owner binding before returning. It does not compose pages across different revisions or generations.

The aggregate limit per request is two seconds, 100,000 decoded rows, and 16 MiB of request/result backing, with a stricter page limit where the existing query has one (metadata pages currently use 64 rows). Count actual decoded rows and checked encoded/backing bytes, including continuation state; a SQL `LIMIT` alone does not bound work before the first row. Use a cancellable SQLite progress boundary plus a monotonic elapsed-time check. A budget hit yields a continuation only at a completed keyset boundary. It never returns a partial page as `FreshAsOf`.

| Result | Required evidence | Caller action |
| --- | --- | --- |
| `FreshAsOf` | Exact source identity, lifecycle generation, revision, complete bounded page, and continuation cut from one transaction | Publish only as of that cut; revalidate for later pages |
| `Stale` or `Superseded` | Expected generation/revision differs from the transaction cut or owner binding changed | Retain last-good projection; restart at a new cut |
| `Unavailable` | Lease, database identity, owner, or source root cannot be validated | Retain last-good projection and request recovery |
| `BudgetExceeded` | Time, row, or byte bound stopped work at a safe boundary | Continue from an exact keyset cut or request audit |
| `Malformed` or `AuditRequired` | Required schema or durable data cannot be interpreted | Do not publish an optimistic replacement |

`FreshAsOf` means the returned revision was committed and matched while the transaction held a consistent read view. It does not promise that the source remains current after the response. No result or continuation can authorize a write or schema repair.

## Acceptance sequence

1. Add a non-mutating schema compatibility check and a typed maintenance-required result. Prove a current operational open leaves schema objects and `user_version` unchanged; prove stale or damaged schema fails without repair. Do not switch existing startup to this check until the owner-held maintenance-to-operation handoff is established.
2. Establish the maintenance handoff on the retained profile/source owner, then use the compatibility check for operational opens. Registration, role-dispatched writers, the public profile-owned source writer, and the snapshot write-fence reopen are wired. The profile-owned writer test proves an operational open needs prior maintenance and a second live owner is rejected. Catalog startup/retarget Full reopens remain maintenance paths; additional binding proof may be required for the snapshot read.
3. Add the bounded owner-read command and exact revision/fence result. The public source-writer manifest page entrypoint and tests now cover a 64-row page, byte limit, keyset continuation, mutation between pages, cancellation before work and inside SQLite progress, stale generation, malformed durable rows, elapsed-time progress interruption, owner loss after result, publication-fence validation, and unchanged SQL change count plus DB/WAL/SHM metadata with no concurrent writer. Fence validation now bounds waiting for the lease-control mutex; a held-mutex test proves the page returns `SnapshotBudgetExceeded`, an in-flight page test proves cancellation while waiting returns `Canceled`, and another in-flight page test proves retained-owner retirement while waiting returns `StaleFence`. Control-connection SQL now has deadline/cancellation progress handling and a busy timeout capped by the remaining budget; a unit test proves SQL interruption and restoration of the ordinary connection policy. Still prove a strict time bound through synchronous filesystem validation before claiming the complete read contract. Caller-family routing belongs to downstream tickets and is outside OPT-1639. Do not infer no-write behavior from `SQLITE_OPEN_READ_ONLY` alone.
4. Run focused source schema/migration, owner revision, read-role, and snapshot tests. A legacy read caller is not considered migrated merely because the new command exists. Caller routing remains outside OPT-1639.

If the owner-held connection cannot provide a bounded consistent read without crossing the writer transaction or lifecycle boundary, stop and replan before introducing a second connection or changing the source writer contract.

The remaining deadline proof cannot follow from a post-check alone: control validation performs synchronous profile, root, database-file, and lock-file checks. The progress handler interrupts SQL work but cannot preempt a blocked filesystem operation. Resolve that execution boundary before claiming a strict two-second wall-clock guarantee for the entire snapshot request.
