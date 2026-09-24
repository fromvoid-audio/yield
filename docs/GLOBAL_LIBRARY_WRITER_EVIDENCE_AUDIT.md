# Global-library writer evidence audit

Audited at `8b656c24ff8853511ee0966c817847b44c4d3c93` on 2026-09-22. This is a
historical-head evidence record for OPT-1824. It describes the code that exists at
that revision and assigns unimplemented target work to the existing issues; it
does not claim that the target global-library owner is complete.

## OPT-1826 implementation update

OPT-1826 closes the queue, owner-start, and read-only observation gaps listed below.
The original audit table and test mapping remain evidence for the audited SHA,
not a description of this later implementation.

- `GlobalLibraryWriter::start` now reserves the `library.db` participant slot in
  the acquisition-shared `WritableProfileGuard` registry before spawning. A
  second start through the same or a cloned guard fails before opening another
  connection. The worker retains the slot through database close; startup
  failure and unwind release it. The earlier one-connection test proved reuse
  within one writer instance, not exclusivity across writer instances.
- Registry commands validate at most 4096 active or retained descriptors,
  16 MiB of aggregate encoded data, 64 KiB per descriptor/path, and the journal's
  256-byte source identity limit. Validation accounts for JSON escaping before
  queue admission and SQL replacement. SQL reads use a 4097-row lookahead and
  guarded BLOB lengths before owned string allocation. Retained-history overflow
  rolls back both active rows and metadata. Existing preferences and vocabulary
  limits stay in force. Delivered observations remain caller-owned; there is no
  new durable reply ledger.
- `load_source_registry_snapshot_read_only` observes an existing `library.db`
  with SQLite read-only/no-follow flags, required-schema checks, one read
  transaction, and a cooperative two-second deadline. It does not initialize,
  migrate, checkpoint, or retain a connection or ownership capability. Missing
  and incompatible databases fail without creation or migration. The result
  can become stale immediately. Strict filesystem no-write qualification for
  live WAL/SHM remains OPT-1639.

Focused owner, registry, and read-only tests cover duplicate-start rejection,
reopen/release, queue bounds, malformed stored payloads, exact retained-history
rollback, missing/incompatible schema, live-owner observation, and deadline
rejection. This slice adds no `OperationId` participant semantics, Harvest,
metadata/index, link, cache/artifact route, or durable shutdown custody.

## Audit basis

The implementation boundary is `GlobalLibraryWriter::start` and `run_owner` in
`crates/yield-library/src/sample_sources/library/owner.rs`. The owner clones
the already acquired `WritableProfileGuard`, opens one profile-bound
`LibraryDatabase` connection on the `wavecrate-global-library-writer` thread,
and exposes typed queue methods rather than the SQLite connection. The queue is
a nonblocking `sync_channel` with capacity 32. The session startup path is
`Persistence::start` in `crates/yield-session/src/lib.rs`: profile ownership,
journal, global-library writer, and then catalog. Shutdown closes catalog before
the global writer, journal, and profile guard.

The command/reply boundary currently contains nine commands: source-registry
load/lookup/replace, UI-preferences load/replace, and profile-tag-vocabulary
lookup/page/set/delete. The owner validates the profile binding before startup,
before each receive/execute, and after each command. Initialization applies
WAL pragmas, schema, and migrations through
`LibraryDatabase::open_for_profile_guard`; this is the writable-owner startup
boundary, not a read-only snapshot contract.

Each request allocates a one-slot `sync_channel` for its reply, so the queued
reply slot is bounded. The returned `Receiver` belongs to the caller, however;
the current owner has no aggregate in-flight input/output byte budget or durable
receipt-retention budget. A follow-up must bound what the owner admits and keeps
while preserving caller-owned observation semantics rather than treating a
delivered observation as an owner-held history record.

## Current evidence and gaps

| Concern | Evidence at the audited head | Status and existing owner |
| --- | --- | --- |
| Owner/startup and typed queue | `owner.rs`: `GlobalLibraryWriter::start`, `start_inner`, `run_owner`, `send_command`; typed `WriterCommand`/`WriterReply`; queue capacity 32 and nonblocking `try_send`. | Implemented for the current command set. Remaining queue/init and read-boundary work belongs to **OPT-1826**. |
| Schema and writable initialization | `sample_sources/library/connection.rs`: `open_for_profile_guard` validates the retained profile binding, checks `library.db`/`-wal`/`-shm`, opens with `SQLITE_OPEN_NOFOLLOW`, applies pragmas, schema, and migrations, then revalidates the binding. | Implemented for the writable owner. There is no separate bounded global-library read-only snapshot API in this owner. Current SQLite read-only flags do not prove strict filesystem no-write behavior for WAL/SHM; that qualification remains a separate boundary in OPT-1639. |
| Source registry | `owner.rs` source-registry commands and `Persistence::queue_load`, `poll_load`, `queue_lookups`, `poll_lookups`, `queue_save` in `session/src/lib.rs`. Tests include `source_registry_and_known_sources_survive_writer_reopen` and `one_owner_connection_serves_repeated_commands_and_shutdown_drains`. | Implemented route with immutable owned snapshots and profile validation. Whole-snapshot replacement currently has session-local registry revision/request fencing, but no durable global `OperationId` participant identity. Further source identity/retirement work belongs to **OPT-1828** and participant contracts **OPT-1641**, **OPT-1642**, and **OPT-1645**. |
| Global metadata and index references | `schema_defs.rs` creates metadata, samples/features, layout, clustering, embedding, and ANN tables. UI preferences and profile tag vocabulary have typed owner commands; tag vocabulary has revision-fenced atomic mutations and bounded keyset pages. | UI preferences and tag vocabulary are routed. Analysis metadata/index references have no corresponding global-writer command in this head. Route ownership and idempotency evidence belongs to **OPT-1830**. |
| Harvest state and derivations | `sample_sources/library.rs` exposes direct Harvest wrappers (`upsert_harvest_file`, state transitions, derivation/remap queries) that open `LibraryDatabase` through the path-based library helper. `harvest.rs` defines the typed state/derivation records. | Existing tests prove state progression, fences, graph persistence, and reopen/remap behavior, but they do not prove a global-writer route. Route this participant through **OPT-1831** and its participant contract **OPT-1645**; do not infer ownership from the tests. |
| Operation links | The global-library schema and `WriterCommand` set contain no `OperationId`/participant-link table or operation-link command. Operation IDs exist in the journal/catalog layers. | No current global-writer evidence. Assign the link participant to **OPT-1833**, with ordering/idempotency boundaries in **OPT-1641** and **OPT-1644**. |
| Cache and artifact references | Schema includes `analysis_cache_features`, `analysis_cache_embeddings`, and `ann_index_meta`. Session `maintenance.rs` owns cache usage/clear requests through a separate maintenance worker; no global-writer command writes these tables, and no artifact-reference command is present. | No current global-writer route. Route global cache/artifact references through **OPT-1834**; preserve the existing maintenance worker boundary. |
| Profile epoch, operation identity, and idempotency | The writer retains and revalidates `WritableProfileGuard`/`BoundDatabaseRoot`. Session `RequestId`, registry revision, preference request identity, and tag-vocabulary revision are local or participant-specific fences. | Profile binding is fail-closed, but the command/reply/schema do not carry a durable profile epoch or global `OperationId`/idempotency key. This belongs to **OPT-1836** and participant contracts **OPT-1641**, **OPT-1642**, and **OPT-1644**. |
| Shutdown and diagnostics | `GlobalLibraryWriter::shutdown` closes admission, drops the sender, joins the owner, and returns a boolean. `GlobalLibraryWriterStatus` and `GlobalLibraryWriterUnavailable` preserve initialization/profile/database failures. Session tests cover shutdown ordering and reopen. | Current lifecycle and fail-closed diagnostics are implemented, but join results are discarded and there is no durable typed drain receipt, participant checkpoint, retry disposition, or retained recovery evidence. Assign this to **OPT-1838**. |

## Finite OPT-1826 checklist

The following are the concrete remaining queue/read-boundary questions identified
by this audit:

1. `SourceRegistrySnapshot::new(Vec<SampleSource>)` has no source-count,
   per-descriptor, or aggregate encoded-byte limit. `replace_source_registry`
   admits that snapshot as-is, and `lookup_retained_source_for_root(PathBuf)`
   admits an unchecked path payload. Add checked request limits before queue
   admission and bounded returned observations; preserve the immutable snapshot
   ownership model.
2. A result `sync_channel(1)` bounds one queued reply slot, but does not create
   an owner-wide input/output byte or row budget. The caller owns the returned
   `Receiver` and may retain delivered observations. Bound owner-admitted and
   owner-retained data without turning caller-owned observations into a required
   durable history or inventing a shutdown receipt; that receipt boundary is
   assigned to **OPT-1838**.
3. The production queued `load_source_registry_snapshot` is the current global
   read operation. The path-only `library::load`, `open_connection`, and
   `retained_sources` helpers open through the writable-initialization path and
   reject normal builds without profile ownership. Define a dedicated bounded
   read-only observation entrypoint that does not initialize schema, migrate, or
   checkpoint. Its SQL read-only behavior must remain separate from the strict
   filesystem no-write/WAL-SHM qualification assigned to **OPT-1639**.
4. Keep the limits that already exist: UI preferences are capped at 128 KiB;
   vocabulary labels are capped at 1024 bytes, lookup input at 64 labels, and
   pages at 64 rows. New registry/read limits should be explicit and tested
   alongside these existing limits.

This is a finite queue, payload, observation, and read-boundary milestone. It
does not add a second writer, route Harvest/metadata/operation-link/cache work,
or require durable shutdown custody.

## Reusable implementation evidence

The current owner tests cover the behavior that can be reused by the follow-up
work: queue admission and full detection, immutable source snapshots, shared
profile ownership, one connection serving repeated commands, UI-preference
atomic replacement/rollback, tag-vocabulary revision fencing and bounded pages,
source-registry reopen, corrupt-database unavailability, profile-root/sidecar
replacement, and shutdown ordering. The session tests
`service_uses_isolated_profile_and_reopens_registry`,
`session_journal_reopens_after_orderly_shutdown`, and
`persistence_drop_stops_external_catalog_before_releasing_journal` exercise the
actual startup and teardown path. Harvest tests prove their direct-library
semantics and reopen behavior only; they are not writer-ownership tests.

The owner command results are typed and dropped receivers do not stop the owner,
but the current boundary does not retain a durable operation receipt. A dropped
reply therefore cannot be treated as a durable participant acknowledgement. The
same distinction applies to `GlobalLibraryWriter::shutdown`: it drains accepted
commands and joins the worker, but it does not produce a durable custody or
recovery receipt.

## Exact existing test mapping

These names are the current evidence at the audited SHA. They prove the listed
behavior only; a missing route test is recorded explicitly rather than inferred
from a direct-library fixture.

| Requirement | Exact tests | Evidence limit |
| --- | --- | --- |
| Queue admission and full behavior | `queue_admission_distinguishes_initializing_and_full`; `queue_admission_is_nonblocking_and_reports_full` (`crates/yield-library/src/sample_sources/library/owner.rs`) | Covers lifecycle/full signaling, not aggregate registry payload bounds. |
| Owner startup, one connection, and drain | `one_owner_connection_serves_repeated_commands_and_shutdown_drains`; `writer_keeps_shared_profile_ownership_after_original_guard_is_dropped`; `source_registry_and_known_sources_survive_writer_reopen` (owner.rs) | Covers the current owner route and reopen; no durable participant receipt. |
| Profile initialization/failure boundaries | `corrupt_database_is_unavailable_without_repeated_open_attempts`; `profile_owned_open_rejects_nonregular_sidecar_before_sqlite`; `profile_owned_open_rejects_database_symlink_without_following_it`; `profile_root_replacement_during_initialization_does_not_redirect_database`; `profile_replacement_closes_owner_and_rejects_later_commands` (owner.rs) | Fail-closed profile/database checks; no dedicated read-only snapshot test. |
| Existing payload validation | `ui_preferences_payload_is_bounded_and_reopens_through_same_owner`; `profile_vocabulary_owner_validates_before_admission_and_drains_on_shutdown`; `profile_vocabulary_delete_drains_after_result_receiver_is_dropped` (owner.rs) | Covers 128 KiB preferences and vocabulary limits, not registry/path payload limits. |
| Registry session route | `service_uses_isolated_profile_and_reopens_registry` (`crates/yield-session/src/lib.rs`); `source_registry_and_known_sources_survive_writer_reopen` (owner.rs) | Covers load/save/reopen; no OperationId participant or aggregate byte-budget test. |
| Harvest semantics | `harvest_state_auto_transitions_only_move_forward`; `current_harvest_touch_fence_rechecks_before_state_write`; `harvest_file_batch_upsert_persists_every_identity`; `manual_harvest_reset_returns_file_to_new_queue`; `manual_harvest_state_update_preserves_known_identity_metadata`; `harvest_derivation_graph_survives_reopen_and_marks_parent_touched`; `remaps_harvest_graph_keys_after_file_moves`; `remaps_harvest_graph_prefix_after_folder_moves` (`crates/yield-library/src/sample_sources/library/tests.rs`) | Direct-library semantics/reopen/remap only; no global-writer Harvest route test exists. |
| Shutdown ordering | `session_journal_reopens_after_orderly_shutdown`; `persistence_drop_stops_external_catalog_before_releasing_journal`; `catalog_shares_session_profile_owner_and_releases_after_shutdown` (`crates/yield-session/src/lib.rs`) | Covers ordering and lifecycle release; durable shutdown diagnostics remain OPT-1838. |
| Unimplemented participant routes | No current exact test exists for global-writer metadata/index, operation links, cache/artifact references, profile epoch/OperationId idempotency, or durable global-writer shutdown evidence. | These remain assigned to OPT-1830, OPT-1833, OPT-1834, OPT-1836, and OPT-1838. |

## Follow-up boundaries

OPT-1826 is the only bounded follow-up recommended from this audit: close the
remaining owner initialization, queue/result, and read-only boundary questions
without adding a second writer or moving source ownership into this component.
The concrete data-family routes remain with their existing issues:

- **OPT-1828**: source registry identity, replay, and retirement.
- **OPT-1830**: global metadata/index references.
- **OPT-1831**: Harvest state/derivation persistence.
- **OPT-1833**: global operation links.
- **OPT-1834**: cache and artifact references.
- **OPT-1836**: profile epoch, `OperationId`, and idempotency fencing.
- **OPT-1838**: shutdown diagnostics and durable participant evidence.
- **OPT-1641**, **OPT-1642**, **OPT-1644**, and **OPT-1645**: shared participant
  ordering, identity, receipt, and checkpoint contracts.

No new dependency or implementation is introduced by this audit. Source-local
manifest truth, physical file mutation, strict filesystem no-write qualification,
Harvest ownership, operation sagas, and artifact payload ownership remain at
their existing owner boundaries.

## Audit limits

This artifact is documentation evidence at the named source revision. It adds
no runtime behavior and does not claim a full test-suite run. Line-level source
references should be read against the audited SHA above. The current code's
profile-bound writable initialization and SQLite read-only/open flags must not be
reported as proof of strict no-write behavior for a live WAL/SHM filesystem.
