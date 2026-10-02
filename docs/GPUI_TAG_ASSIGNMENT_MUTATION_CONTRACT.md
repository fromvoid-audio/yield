# Definition and assignment mutation boundary (OPT-2000)

Baseline `b9fc89bc28ebe0aec87595325b891ebf6f2c54f9`. The active APIs do not
provide a shared durable definition-plus-assignment rename/delete command.
That missing backend prerequisite is OPT-2012, before original OPT-1721/1722
can be implemented. This is a source contract blocker, not a macOS UI test.

## Actual owner/API inventory

| Owner / API | Proven scope | Missing scope |
| --- | --- | --- |
| Profile global writer: `ProfileTagVocabularyClient::delete_profile_tag_vocabulary`; `LibraryDatabase::delete_profile_tag_vocabulary` | Own SQLite transaction checks profile vocabulary revision, deletes one definition, commits new revision and returns `ProfileTagVocabularyDeleteReceipt { revision, removed }` | Does not visit source assignment databases or report affected assignment rows. No shared dictionary/assignment rename transaction |
| Session vocabulary proxy: `SourceRegistryService::delete_profile_tag_vocabulary` | Bounded background request/reply for that definition operation | Request identity is not a durable cross-source coordinator operation |
| Source writer: `SourceCurationUpdate::{AddTag, RemoveTag, AssignExistingTag, RemoveExistingTag}` and source-local curation transaction | Source revision/ownership and bounded assignment edits; exact source-local commit metadata | No assignment-wide rename/delete participant command or durable dictionary linkage |
| Catalog compound curation: `SourceCurationOperationFence::{ExactRevision, AfterOperation}` and `SourceCurationOperationOutcome` | Fenced source-lifecycle batches, committed prefix receipts and typed failed batch; latest successful predecessor lineage | Not a global OperationId/restart coordinator for all sources; partial prefix is not global success |
| GPUI `tag_dictionary.rs`, `tag_library.rs`, `curation.rs` | Definition-only undo/beforeimage, category/assignment controls and projection fencing | Cannot implement cross-database atomicity by sequencing calls in a GUI callback |

Source-local numeric tag IDs are scoped to their source database. A profile
canonical label/definition cannot supply the same numeric ID to every source.
Playback/classification locked labels must retain existing validation policy.
Display aliases and normalized spelling do not authorize destructive merge.

## Proposed shared durable boundary (not implemented)

Start with one canonical old definition and one source participant. Capture
immutable old/new canonical label/category, exact profile revision and
source/root/database/lease/revision fence; reject unsupported collisions,
locked labels, stale/missing definitions or unknown allocation peaks before
intent. The coordinator owns one durable OperationId and canonical request
fingerprint. Correlation request IDs and per-source lineage counters are
observations, not replacement identities.

Before the first assignment change, durably record the definition expectation,
participant set and bounded affected-assignment selection/page continuation.
The one-source writer applies a typed rename/delete participant transaction
that updates its definitions/assignments with its idempotency receipt and
source revision. Replaying the exact operation/participant/step fingerprint
returns that stored result without applying rows again; reused identity with
different bytes conflicts. The receipt must distinguish exact changed,
already-applied/no-op, missing and failed rows. Current metadata patch vectors
do not by themselves count every assignment affected by a dictionary command.

Finalization changes the profile definition only after committed participant
checkpoints establish the chosen assignment policy. Definition-only deletion
is a separate supported operation, never a fallback completion for this one.
Ack loss retains the coordinator/participant identity and replays its stored
receipt. Source retirement/root replacement or revision races return stale or
blocked outcomes and preserve the recoverable intent/definition expectations.

Extend to multiple sources only with bounded pages/participants, independently
committed receipts and exact durable checkpoints under OPT-1641. A committed
prefix plus failed/offline source is Partial/Blocked, with counts per source
and a remaining set; it is not rollback of already committed SQLite state or
global success. Preserve the original profile definition until every required
participant finishes, or expose the explicitly accepted staged-definition
policy. Neither policy is established by today's APIs. Restart must reconstruct
remaining work from durable participant checkpoints, not a process-local list.

Do not claim one SQLite transaction across profile and source WAL databases,
use ATTACH to invent atomicity, or issue blind compensating tag edits under a
newer source revision. Capacity plans must cover journal/profile/source/WAL
coexistence, including retained undo evidence, before ordinary admission.

## Portable evidence and acceptance limits

Existing vocabulary tests prove revision-fenced idempotency, exact beforeimage,
rollback on revision overflow/malformed rows and locked-label rejection.
Existing GPUI curation tests prove mixed tag/rating lineage and applying only
committed receipt types after partial tag-prefix failure. These are useful
negative/partial primitives, not tests of the proposed shared transaction.

OPT-2012 owns the absent backend identity/participant/receipt boundary and
restart/partial-failure fixtures; it is blocked by the shared ordering contract
OPT-1641. Original rename/delete OPT-1721/1722, cross-source failure OPT-1731
and assignment-wide undo OPT-1733 stay open. This bounded review completes
OPT-2000 with an explicit missing backend prerequisite; no vocabulary-only
mutation or native UI test is counted as assignment-wide completion.
