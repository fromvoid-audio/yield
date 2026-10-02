# Global command caller identity and retention decision (OPT-2031)

This decision resolves the caller-policy prerequisite for OPT-1836. It covers the actual
registry, preferences and profile-vocabulary producers in the current GPUI/session graph,
plus the implemented operation-scoped registry API. It defines implementation requirements;
the original decision did not implement envelopes. Follow-up implementation status is recorded below.
The existing profile-owned `GlobalLibraryWriter` remains the sole execution owner.

## Actual route inventory and chosen identity

Library module paths below are relative to `crates/yield-library/src/`; UI and session
producer modules are in their respective `crates/yield-ui/src/` and `crates/yield-session/src/`
trees. Local request IDs correlate UI results; they are never converted to restart-stable `OperationId` values.

| Producer and handoff | Owned transaction/result | Chosen caller identity and retention |
| --- | --- | --- |
| `yield-ui` browser `source_registry.rs`: add/remove, reorder and primary-setting producers → `yield-session/src/lib.rs` `Coordinator`, `queue_save`, `poll_save` | `sample_sources/library/owner.rs` `replace_source_registry` → `sources.rs` registry + retained mappings transaction; current result `()` | Latest configured snapshot within one acquisition and one session registry series. Keep `RegistryRevision` and current desired snapshot until confirmed. Reload committed state on restart; no historical operation proposal is replayed. No file-saga OperationId for these configuration edits. |
| `yield-ui` browser `preferences.rs` and `mod.rs` preference patches/reset → session `PreferenceCoordinator`, `queue_preference_save`, `poll_preference_save` | `replace_ui_preferences_payload` → `sources.rs` metadata transaction for `ui_preferences_v1`; current result `()` | Latest bounded preference snapshot, `PreferenceRevision`, one acquisition/series. Debounced/coalesced desired state stays in the existing coordinator. Only a committed acknowledgement confirms persistence; restart loads committed payload. No file-saga OperationId. |
| `yield-ui` browser `tag_library.rs`, `tags.rs`, dictionary restore → `SourceRegistryGlobal::set_profile_tag_category` → session `vocabulary.rs` slot → `ProfileTagVocabularyClient` | `tag_vocabulary.rs` set transaction with persistent vocabulary revision; `ProfileTagVocabularySetReceipt { revision, changed }` | Current acquisition plus expected vocabulary revision and exact label/category. Preserve existing optimistic CAS semantics. A retry at the old revision can be stale; a fresh current-revision no-op is a new result, not recovery of a lost changed result. |
| `yield-ui` browser `tag_dictionary.rs` `submit_remove` → session delete slot → profile client | Delete + persistent revision transaction; `ProfileTagVocabularyDeleteReceipt { revision, removed }` | Same acquisition/CAS contract. `removed` is retained by `TagDictionaryState::last_removed` only when its actual result arrives. Current dictionary Undo is session-local; after lost response/restart the UI cannot invent that tombstone. Restart-recoverable Undo would require a separate durable command/proposal/result scope before implementation. |
| `sample_sources/library/owner.rs` `apply_source_registry_participant` (PR #196) | `registry_participant.rs`: immutable operation/work-key/payload receipt committed with the registry delta or retained `RegistryChanged` rejection | This route requires caller-stable `OperationId`, frozen global work key and immutable expected/replacement proposal. It supports exact durable replay through a fresh current acquisition. Current repository production callers do not yet submit this API; actual owner tests exercise it. Family routing/proposal recovery remains explicit consumer work. |

Schema anchors: `schema_defs.rs` defines `metadata` and `profile_tag_vocabulary`;
`sources.rs` commits configured descriptors plus `known_sources_v1`, and preferences
use `metadata.ui_preferences_v1`. `tag_vocabulary.rs` commits the row mutation with
its persistent metadata revision. `registry_participant.rs` defines and commits
`source_registry_participant_receipts`; no ordinary settings receipt table exists.

These vocabulary calls affect profile label/category metadata, not source-owned sample tag
assignments. Source-tag mutation and journal-backed assignment remain OPT-2012/2034 scope.
Configured membership removal likewise does not establish source/database retirement.
Read-only lookup/page calls keep their bounded query/revision contract and cannot admit effects.

## Common live acquisition and snapshot proposal envelope

Use a renderer-neutral `ProfileAcquisitionId` observation wrapping the existing guard's fresh
per-acquisition UUID. Clones preserve it; reopening after releasing all guards changes it.
It is an equality fence, not a monotonic counter or authority. The worker must still validate
its retained profile/root/lock capability before execution and before confirming the reply.

For ordinary snapshot calls, implement a typed envelope containing:

* captured acquisition ID, known snapshot series (`ConfiguredRegistry` or `UiPreferences`),
  session-local proposal revision and fixed payload codec/domain version;
* bounded immutable payload and its identity, captured with that revision;
* reply echo of acquisition/series/revision/payload identity and a typed committed, stale,
  conflict or unconfirmed outcome.

Capture context when the producer forms the proposal. Do not replace an old context with the
new writer's identity when a delayed action is submitted. The immutable client/queue already
prevents an old vocabulary client from targeting a reopened worker; explicit context is also
needed to reject an old action redirected through a newly obtained client. Each mutating
envelope must be checked against the live owner's acquisition before any transaction.

Snapshot series are the two named session producers, not an arbitrary growing caller map.
Keep at most the last committed revision/identity/acknowledgement per series in the owner.
A lower revision is stale. Equal revision/equal payload returns the same acquisition-local
acknowledgement; equal revision/different payload is a conflict. Greater revision commits the
current proposal, then updates the slot. On a new acquisition the slots reset and the producer
loads authoritative persisted state before constructing fresh desired proposals. An old
acquisition's proposal is rejected even when its payload happens to equal current state.
The slot is an acquisition-local ordering fence; it is not durable operation history.

Do not acknowledge a revision before SQLite commit or treat old completion as confirmation of
a newer dirty revision. The existing `Coordinator::accept_save_ack` and
`PreferenceCoordinator::accept_save_ack` already keep this distinction at the session layer.
The library-side envelope closes the remaining public-boundary gap: current raw snapshot
setters cannot distinguish an intentional new setting from an old blind resubmission.

Vocabulary set/delete get the same captured acquisition check and echoed local correlation,
while their durable expected revision remains the mutation precondition. Keep their current
CAS/no-op result types. Do not label CAS as exact OperationId replay: two proposals can share
an unchanged revision, and after a changed commit an old revision is stale. A missing reply
requires requery/refresh and an explicit unconfirmed outcome; it cannot manufacture `changed`,
`removed`, a successful Undo, or a checkpoint for another subsystem.

## Proposal durability, transaction order and ambiguous results

Ordinary snapshot/CAS requests are current-acquisition settings requests. Their proposals
remain in the existing session pending/coalescing state until result, rejection or teardown.
They have no promised crash-replay proposal log. Queue acceptance means admitted work may run,
including after its receiver is dropped. A disconnected/unconfirmable reply suspends dependent
confirmation and reloads authoritative state under a fresh valid acquisition. Reconstruct a
new desired proposal from current session state only after that reconciliation; never replay
a remembered obsolete payload automatically. Uncommitted settings can be lost on crash.
This preserves the current distinction between optimistic UI state and persisted state.

An operation-scoped participant has stronger requirements. Its caller must retain the stable
operation/work key and exact proposal before effect dispatch, either in an existing durable
intent capable of reconstructing that proposal or a specifically designed owner-bound
proposal relation. The current file-journal envelope does not contain arbitrary UI preference
or vocabulary proposals; do not force those settings into a fabricated file operation. No
current production registry-participant caller/proposal-recovery bridge is claimed here.
Such a consumer must first name its actual command, durable proposal representation and
recovery entrypoint. The registry API alone does not create admission authority.

For an admitted operation-scoped registry proposal, reuse PR #196: validate current acquisition,
look up exact operation evidence, compare work key/payload, compare expected snapshot, commit
delta + immutable result together and read back before reporting success. A lost response
replays the same committed receipt; a new acquisition changes admission fencing, not the
stable replay key or historical receipt. A conflict, malformed/duplicate retained relation,
capacity exhaustion or uncertain commit preserves evidence and supplies no completion proof.

## Negative/replay evidence matrix

| Trigger | Ordinary snapshot / vocabulary result | Operation-scoped registry result |
| --- | --- | --- |
| Stale acquisition | Old client is `Closed`; explicit old context redirected through a fresh client requires the planned common fence. | `SourceRegistryParticipantStaleOwner` before effect, even for an exact duplicate. |
| Response loss | Load committed current snapshot; vocabulary old-revision retry is `ProfileTagVocabularyStaleRevision`, and refresh is a new observation. Lost deleted-row data supplies no Undo. | Exact key/payload returns the original committed receipt. |
| Changed payload | Planned equal-revision snapshot conflict; current vocabulary competing old-revision payload is stale and cannot replace the committed row. | `SourceRegistryParticipantReplayConflict` preserves the first result. |
| Reopen | New acquisition, current persisted metadata/revision and no automatic old proposal replay. | Current acquisition can replay the historical receipt without restoring an obsolete snapshot. |
| Backpressure / ambiguous evidence | Queue/permit failure and disconnected/unconfirmable result provide no persisted acknowledgement. | Receipt capacity, malformed data or duplicate retained relation provides no completion and preserves existing evidence. |

## Bounds and ownership

Registry snapshots retain the existing 4096 descriptors / 16 MiB aggregate / 64 KiB descriptor
bounds. Preference payloads retain 128 KiB. Vocabulary labels retain 1024 bytes, exact lookup
64 labels, pages 64 entries and session results eight outstanding permits. Global queue
capacity remains 32, with one-slot replies; ordinary snapshot fence slots retain only bounded
acknowledgements/digests, not old payload history. Operation registry receipt capacity remains
8192, with no implicit eviction. Safe reclamation requires a separate journal retention proof.
Full-queue/backpressure and revision exhaustion cannot become successful acknowledgement.

The profile worker owns SQLite and executes transactions. Session producers own desired
state/coalescing and result correlation. GPUI owns projections and transient pending/Undo UI
state. None of these observations authorizes physical publication, source retirement, source
lease use, journal success or cross-database atomicity.

## Evidence and bounded implementation follow-ups

`global_replay_contract_tests.rs` exercises actual current APIs: ordinary snapshot response
loss followed by authoritative reload, vocabulary set's stale retry versus fresh no-op,
delete's lost tombstone versus current absence, and old vocabulary client rejection after
fresh acquisition. Existing `registry_participant_*` fixtures cover explicit stale acquisition,
immutable payload/work-key conflict, lost response, reopen and ambiguous retained receipts.
Session `coalesced_revision_does_not_accept_stale_save_ack`,
`stale_save_ack_keeps_newer_preference_revision_dirty` and
`reverting_before_save_ack_still_requires_latest_revision` cover local completion ordering.
These establish the original behavior and decision inputs; follow-up fixtures below cover implemented envelopes.

Under OPT-1836, implement these finite leaves before marking that parent complete:

1. **OPT-2045:** Common acquisition context and typed ordinary mutation acknowledgements in
   `sample_sources/library/owner.rs`, plus vocabulary client context capture/check. Test all four
   ordinary routes with an old context redirected through a fresh client, guard replacement
   before/after commit and wrong-context completion. Keep retained guard checks and existing owner.
2. **OPT-2046:** The two bounded snapshot revision/digest slots and fenced snapshot APIs in `owner.rs` /
   `sources.rs`; bind the actual `queue_save` / `queue_preference_save` and `SaveInFlight` /
   `PreferenceSaveInFlight` contexts in `yield-session/src/lib.rs`. Test lower/equal-conflicting
   revisions, same-context replay after response loss, coalesced new state and reopen with fresh
   acquisition. No durable settings ledger or generic caller map is required by this policy.
3. **OPT-2047:** Propagate/validate vocabulary acquisition context in session `vocabulary.rs`,
   `SourceRegistryClient` and the browser dictionary/category pending state. Test stale CAS,
   lost delete result, fresh current-state no-op and refresh without fabricated Undo. Preserve
   current session-local Undo limits; stronger durable Undo needs a new scoped requirement.

Coordinate these shared owner/session edits; rebase and rerun affected owner/session/app checks.
OPT-1836/1569 and operation-family consumers remain open until their implementation acceptance
passes. This decision and its portable owner fixtures require no native physical qualification.

## Implemented common boundary (OPT-2045)

`mutation_context.rs` now supplies the renderer-neutral `ProfileAcquisitionId`, immutable
`GlobalMutationRequest` constructors for the four ordinary routes, and correlated
`GlobalMutationCompletion`. Capture `writer.acquisition_id()` / vocabulary client's
`acquisition_id()` when forming a proposal, retain `request.context().clone()`, submit that
unchanged request via `submit_captured_mutation`, and consume `into_result_for(&context)`.
Context includes acquisition, typed route, caller-local correlation and a domain-separated
BLAKE3 identity of the exact bounded payload (including vocabulary expected revision).
A wrong context yields `GlobalMutationCompletionMismatch` without exposing an acknowledgement
or deleted-row Undo result. Correlation remains local; neither it nor the digest is authority.

The new owner command compares captured acquisition with the retained guard identity before
calling an existing metadata transaction. Stale context yields `GlobalMutationStaleAcquisition`
with the original context. Existing pre/post-command root/lock/SQLite binding checks remain
mandatory: post-commit guard failure replaces success with the existing unconfirmable error;
queued rejected requests also echo their original context. A disconnected reply supplies no
completion. The vocabulary client admits only vocabulary routes; it retains its old acquisition
and closing queue even if a fresh client is later installed.

The bounded queue, source/preference/label limits and current CAS transactions are unchanged.
The common API and session/UI producer adoption are implemented below. Legacy unframed
library compatibility methods remain without retained caller/restart identity. OPT-2046 adds
snapshot ordering/replay; the common frame by itself supplies only acquisition/context
fencing. OPT-2047 adopts vocabulary producers and retains their persistent CAS semantics.
No receipt table, source admission, file-operation authority or native physical qualification
is added. Actual-owner fixtures cover all routes through a fresh owner with old context, current
commit/CAS, mismatch checks, bounded admission, and pre-effect queued rejection / post-commit
unconfirmable replies under a replaced profile root.


## Implemented snapshot fences and session adoption (OPT-2046)

The captured registry/preferences request's correlation is now explicitly the acquisition-local
proposal revision. `SnapshotFences` retains exactly two `(revision, payload digest)` slots on the
existing owner thread. Successful SQLite commit records a slot; failed/unconfirmed transactions
supply no acknowledgement. Lower revisions return `GlobalSnapshotStaleRevision`, equal revisions
with another payload return `GlobalSnapshotPayloadConflict`, and exact duplicates return the
same typed unit acknowledgement/context without executing SQL again. A duplicate confirms its
historical commit, not that no independent command has changed current storage since then.
Slots reset with a new owner; stable operation receipt ledgers remain independent.

Actual session `queue_save` and `queue_preference_save` construct the bounded immutable proposal
from current loaded/coalesced state, capture the existing writer acquisition and retain its exact
context in `SaveInFlight`/`PreferenceSaveInFlight`. Neither service swaps its writer into another
acquisition. The completion context and typed snapshot result must match before local revision
acceptance. A valid old acknowledgement leaves newer state dirty; registry resubmits only that
newer current snapshot. Mismatched/stale-acquisition/disconnected results keep persistence
unconfirmed and suspend shared confirmation. SQLite preference failures retain the existing
route-local failure policy; they do not trigger blind replay. Shutdown/restart under a fresh
valid acquisition loads authoritative storage before forming new proposals.

Actual-owner fixtures prove ordering, conflicting payloads, response-loss replay without
reapplying later values, failed transactions, two independent bounded slots and fresh-acquisition
reopen. Session fixtures prove wrong-context and disconnected completions never clear dirty
state or emit `Persisted`, newer coalesced registry state is the only next save, and fresh
acquisition reload reconciles committed state. Four named preference integration fixtures use
the explicit test-only no-capacity startup while preserving real owner/SQLite/restart. Production
protected-reserve bootstrap is unchanged. Native retained capability checks remain separately
scoped; these metadata tests supply no physical capacity/publication qualification.

## Implemented vocabulary producer adoption (OPT-2047)

Actual session set/delete submissions use the captured mutation envelope and check
its exact completion context before projecting a typed receipt. Reads carry their
observed acquisition. The explicit `*_for_acquisition` methods preserve an originating
acquisition through a newly obtained client; the worker rejects a redirected old
proposal before its transaction. Request IDs remain local correlation, never
restart-stable OperationId values.

GPUI retains that observation in pending add-tag/category intent, profile page and
lookup state, category drag payload and transient dictionary Undo. A wrong-acquisition
reply cannot update category state, admit dependent source assignment or manufacture
Undo. Reopening requires fresh authoritative reads. Disconnected/context-mismatched/
unconfirmable SQLite or owner replies invalidate affected projections, refresh and
report an unconfirmed result; they do not blindly replay the old mutation. Only an
actually received removed-row result creates the bounded same-acquisition Undo.
A current missing-label no-op is not recovery of a lost deletion tombstone.

Owner/session tests exercise redirected old acquisitions, exact completion mismatch,
dropped changed/delete results with shutdown/reopen and fresh no-ops. UI projection
tests use real owner-issued acquisition observations but do not qualify native
interaction; the action register's OPT-2009 gate remains open. The library's existing
CAS and worker capability checks remain in force; no vocabulary receipt ledger or
new SQLite owner was added.

## OPT-1836 finite production route matrix

These are implementations through the existing global owner. The earlier OPT-2031
compatibility-owner fixtures documented policy without implementing captured envelopes. All commands retain the pre/post-command profile capability checks
in `run_owner`; `ProfileAcquisitionId` is an observation fence, not a replacement lock.
The ordinary producer contract remains the route policy above. OperationId is required by
implemented saga-participant routes, not assigned to ordinary preferences or vocabulary.

| Covered production route / consumer | Fence and response-loss/restart result | Actual focused evidence |
| --- | --- | --- |
| Configured registry snapshot: session `queue_save` / `SaveInFlight` | Captured acquisition/local revision/exact payload; lower or same-revision conflicting proposals reject, exact local duplicate acknowledges without SQL; restart reloads committed state and rejects old context. | `snapshot_slots_reject_lower_and_conflicting_revisions_replay_without_reapplying`, `snapshot_lost_response_replays_both_series_and_reopen_resets_only_local_fences`; session `snapshot_session_coalesces_new_state_after_valid_stale_ack_and_confirms_only_latest`. |
| UI preferences snapshot: session `queue_preference_save` / `PreferenceSaveInFlight` | Independent second bounded slot; failed transaction cannot poison acknowledgement; mismatched/disconnected response stays unconfirmed; restart loads current committed payload. | Same two snapshot owner fixtures, `snapshot_failed_transaction_does_not_advance_or_poison_acknowledgement_slot`, session `snapshot_session_preferences_keep_wrong_or_disconnected_completions_unconfirmed`. |
| Vocabulary Set: session `vocabulary.rs`, UI chooser/drop in `tags.rs` / `tag_library.rs` | Captured originating acquisition and persistent revision CAS; lost changed result is not reconstructed by a fresh current-state no-op. | `captured_mutations_reject_all_old_acquisition_routes_through_fresh_owner`, `captured_mutations_echo_context_and_commit_all_current_routes_with_existing_cas`, actual `vocabulary_service` session tests. |
| Vocabulary Delete/Undo: session delete and UI `tag_dictionary.rs` | Exact acquisition/CAS; mismatch/unconfirmed reply supplies no removed-row tombstone; Undo requires a received current-acquisition result and is invalidated on reacquisition. | `captured_completion_must_match_acquisition_route_correlation_and_payload`, actual `vocabulary_service` lost-delete/reopen and wrong-context tests, dictionary acquisition projection fixture. |
| Operation-scoped registry participant (OPT-1642) | Current acquisition before immutable OperationId/work-key/payload receipt lookup; same proposal returns one historical result, conflicting input rejects; rejected snapshot result also remains immutable. | `registry_participant_lost_response_replays_across_new_profile_acquisition_without_reapplying`, `registry_participant_conflict_and_stale_snapshot_have_one_retained_disposition`, `registry_participant_reversed_replies_preserve_applied_and_rejected_operation_identity`. |
| Cross-source metadata-reference participant (OPT-1643) | Current global acquisition plus genuine retained source reference observations; immutable canonical OperationId/work-key/proposal receipt; old-source/root/revision observations reject. | `reference_owner_lost_reply_self_endpoints_old_key_reuse_and_restart`, `reference_owner_ended_source_and_changed_revision_fail_before_effect`, `reference_owner_reversed_replies_preserve_immutable_operation_results_across_restart`. |
| Operation-link participant (OPT-1644/1833) | Current acquisition, composite operation/global/source/command key, immutable actual source receipt payload; reversed command/result observations preserve each historical link. | `operation_link_lost_reply_duplicate_multiple_commands_and_restart`, `operation_link_actual_source_receipt_survives_global_failure_and_readback`, `operation_link_reordered_submissions_and_restart_keep_one_result_per_key`. |

`mutation_context_owner_tests.rs` additionally proves bounded queue/route admission, queued
pre-effect rejection and post-commit profile replacement producing an unconfirmed result.
Participant owner suites also cover rollback, corrupt/ambiguous retained receipts and bounded
retention exhaustion. Session completion checks preserve newer dirty state after an older valid
acknowledgement and reject acquisition/route/correlation/payload mismatches before acceptance.

Run the matrix with `cargo test --locked -p yield-library --lib
sample_sources::library::owner::tests -- --test-threads=1`, session `snapshot_persistence` and
`vocabulary_service` filters, the dictionary acquisition projection fixture, and the application
build/source guards. These are actual owner/storage/producer tests; the original decision/compatibility
filter alone is insufficient.

`check_global_command_callers.py` is a finite lexical tripwire for unframed mutation method
calls and conventional type UFCS in non-library, non-UI workspace source. The library retains
compatibility methods for old fixtures. UI has acquisition-taking session facades and its raw
owner access is checked by `check_gpui_owner_boundary.py`. The new check does not resolve Rust
types, exotic UFCS aliases or prove runtime fences; the actual owner and consumer fixtures do.
No new global route, reply ledger or generic saga coordinator is introduced.

This matrix completes only the named command-fence acceptance when its checks pass. Actual
future Harvest/family callers must implement their own captured proposals/receipts, admitted
journal recovery, capacity, and publication rules. Tracking OPT-1357 and those consumer scopes
remain open; a metadata receipt or model fixture is not physical qualification.
