## Status and authority

This is a target design, not an implementation change. PR #1024 is merged at
`55f6dc0d3` and is the current baseline for observed behavior. The merged code,
not this document, remains the authority for what the application does today;
this document is the contract for later, separately reviewed slices.

Directory truth, the `schema/CommittedSourceDelta`, recursive hydration, a new
backend/cursor, a durable raw-retention policy, and PR #980 are later and out
of scope here.

## Current evidence/gap

Current source maintenance already has watcher/replay evidence and downstream
`GuiMessage`, workflow, source-writer, projection, and checkpoint consumers.
That path is the compatibility surface, but the evidence reaching it is not
yet one explicit, bounded, backend-neutral contract with durable proof of its
source, root, and watcher generation.

The target gap is therefore not another filesystem walk. It is a small evidence
boundary that can say what was observed, where and when it was observed, which
generation observed it, and when observation was incomplete. Current behavior
must be described as current behavior; the scopes and transitions below are
target behavior until implemented and validated.

## Design goals

- Preserve evidence order and provenance without treating an event as directory
  truth.
- Make overflow, backend errors, missing identity, and stale generations
  explicit and conservatively recoverable.
- Keep capture bounded and non-blocking, with fairness between sources.
- Keep the model/normalizer and backend-neutral admission/lifecycle library
  slices independently testable before live/replay adapters, then normalize all
  backends into the same work scopes and retain a narrow adapter to the existing
  workflow and persistence owners.
- Make root and generation boundaries impossible to cross accidentally.
- Allow cancellation, restart, crash recovery, and later replay without
  claiming completion for work that was not committed.

## Bounded raw evidence model (backend-neutral event kind, ordered relative paths, flags/cookies/event IDs, capture time, source/root identity, watcher generation, overflow/error)

The target raw record is an observation envelope, not a database row and not a
filesystem snapshot. It contains:

- one backend-neutral event kind: `Create`, `Modify`, `Delete`, `Rename`,
  `Copy`, `RootChanged`, `Overflow`, `Error`, or `Unsupported`;
- an ordered list of relative paths, retained in backend delivery order rather
  than converted to an unordered set;
- optional backend flags, rename cookies, event IDs, and cursor information;
- capture time from the observing process;
- opaque source identity and root identity, with the root-relative path base;
- the watcher generation that admitted the observation; and
- overflow/error detail, including whether ordering, path coverage, or backend
  continuity is uncertain.

The envelope is bounded by event count, encoded path/metadata bytes, and queue
budget. Paths are validated as root-relative values before admission. An
envelope may be coalesced only when the resulting uncertainty is retained; it
must never silently become an empty notification.

### Raw observation provenance is not watcher continuity proof

Every envelope carries a `RawObservationProvenance` value describing where the
observation came from: the associated source/root identity, backend and stream
metadata when available, watcher generation, capture time, event/cursor
metadata, and the lossless root-relative path bytes. This is origin metadata,
not publication authority. A source/root label, event ID, cursor, generation,
or backend callback by itself does not prove physical root identity, current
entry type, complete coverage, or permission to publish a targeted result.

The envelope carries a separate proof state:

- `Proof::Unproven` is the default. Live notify, memory-only evidence, lost or
  evicted evidence, cancellation, rebind, stale-generation rejection, and any
  invalid or incomplete replay remain unproven.
- Only the replay adapter may attach a valid `WatcherContinuityProof`. The
  proof must contain the same source and root identity, backend stream identity,
  watcher generation, a durable prior acknowledgement that the backend can
  replay, and contiguous replay coverage from that acknowledgement through the
  current batch boundary. The pure normalizer and live adapter may carry this
  proof state but may not create it or promote `Proof::Unproven`.

The proof is necessary for watcher-derived targeted work but is not, by itself,
a source commit or projection acknowledgement. The source writer must still
validate the proof against the durable source/checkpoint boundary before a
targeted checkpoint can advance.

## Ownership and state transitions

The watcher/backend owns capture only. An admission supervisor owns source/root
identity checks, generation checks, queue budgets, cancellation, and delivery
ordering. A pure normalizer owns the conversion from raw evidence to work
scopes. The existing workflow and source writer own filesystem/database effects;
projection owns publication of accepted results; checkpointing owns the
committed boundary.

The target lifecycle is:

`Stopped -> Starting(g) -> Capturing(g) -> Queued(g) -> Normalizing(g) -> Dispatched(g) -> Applied -> Checkpointed`

`Overflow`, `Error`, `Unsupported`, cancellation, or a stale delivery enters an
explicit uncertain/recovery path. A restart creates a new generation; records
from an older generation cannot advance the new one. No watcher callback or
normalizer writes the filesystem or database.

## Live notify path

1. The backend callback captures one bounded raw envelope quickly, including the
   `RawObservationProvenance`, and sets `Proof::Unproven` before handing it to
   admission.
2. Admission rejects a wrong root or stale generation, or records the rejection
   as retained uncertainty; it applies the per-source/root and global budgets.
3. The pure normalizer preserves the raw evidence and widens incomplete
   evidence into `ExactEntry`, `Subtree`, or `SourceAudit` work scopes.
4. Until a later adapter PR changes the seam, live notify follows merged PR
   #1024: it routes through `WatcherAuthorityUnproven`, retains the last-good
   projection, and requests an authoritative affected-region/source audit. It
   has no targeted watcher authority and cannot advance a targeted watcher
   checkpoint.
5. Only the committed audit result may publish an authoritative replacement;
   projection and any audit-owned checkpoint advance only after the existing
   committed boundary is satisfied.

The live path does not perform a recursive walk, wait on the database, or use a
best-effort callback-time filesystem read to manufacture proof. A live callback
never becomes a continuity proof merely because its paths look complete.

## macOS FSEvents replay path

The target replay adapter feeds historical FSEvents batches into the same raw
envelope. It marks replay provenance, preserves the backend-delivered order of
relative paths at the raw boundary, and binds every batch to one source, root,
backend stream, and watcher generation. It is the only component allowed to
attach `WatcherContinuityProof`, and does so only after validating the durable
prior acknowledgement and contiguous replay interval.

An invalid, unavailable, or gapped replay cursor produces `Overflow` or `Error`
evidence with `Proof::Unproven` and a conservative recovery scope; it does not
advance a checkpoint or pretend that the interval was clean. FSEvents
coalescing is evidence of possible change, not proof that only the listed path
changed. A restart or rebind without a replayable cursor follows the same
unproven/audit path. The adapter boundary is specified here so live and replay
paths converge; implementing a new backend or cursor is a later slice.

The exact, already-existing regular file `.DS_Store` is retained as a privately
tagged native observation when FSEvents reports a certain modification. Only a
complete capture containing exclusively those tagged observations may use the
source owner's normal replay admission and durable checkpoint writer without
running path reconciliation. The tag is not proof: the owner still validates
source, root, stream, watcher generation, prior acknowledgement, revision and
capture interval, and reads the committed receipt back before retiring capture
custody. This checkpoint does not change the manifest revision or publish a
renderer delta. A genuinely empty capture retains its bounded retry behavior;
mixed source changes, create/rename/remove, non-file entries, and uncertain
delivery stay on ordinary replay or conservative audit paths.
The local native acceptance exercises an existing-file inode timestamp change;
content writes that macOS reports with `ItemCreated` remain on conservative
ordinary replay and do not qualify for the metadata-only checkpoint.

The native test source is a content-keyed fixture under the Git common
directory's `.git/local-ci/fixtures/native-metadata/`. Full and native
`catalog-replay` lanes prepare it before Cargo starts, so detached checkouts and
temporary clean-state work do not reset its filesystem history. The helper
creates the key atomically once, records root/file device and inode identities,
and verifies an existing key without repair. A missing, changed, symlinked, or
otherwise unexpected keyed tree fails the lane. The test uses a separate
disposable catalog profile and restores the fixture file's original mtime only
after watcher shutdown; the persistent source bytes and identities are checked
again afterward.

## Conservative normalization (ExactEntry/Subtree/SourceAudit, uncertainty widens, rename/copy/delete/missing-parent/symlink/unsupported-only/empty-folder rules)

Normalization is lossless and syntactic only. It validates root-relative syntax,
retains native path bytes and raw order, and produces immutable work scopes with
their source/root/generation provenance and unchanged proof state. It performs
no Unicode normalization, case folding, lossy conversion such as
`to_string_lossy`, filesystem I/O, database I/O, or current entry-type proof.
Event kind, path spelling, Finder metadata, and replay metadata do not prove
that an entry currently exists, is a leaf, is supported, or is absent. The
scopes mean:

- `ExactEntry`: inspect only the named path; this is a bounded inspection scope,
  not proof of the entry's current type or existence;
- `Subtree`: inspect the named directory and descendants; or
- `SourceAudit`: audit the complete source/root boundary.

Uncertainty widens, never narrows: `ExactEntry -> Subtree -> SourceAudit`.
Queue pressure, path existence guesses, and a convenient filesystem read cannot
reduce a scope.

- A syntactically valid create/modify path may map to `ExactEntry`; the worker
  must classify its current entry without following links and widen if that
  classification is uncertain.
- A syntactically valid delete path may map to `ExactEntry` for no-follow
  verification. An absent, directory, or otherwise uncertain result widens to
  its nearest syntactically valid parent `Subtree`; deleting or losing the root
  maps to `SourceAudit`.
- A rename covers both old and new endpoints as inspection scopes. A directory
  rename, an unknown endpoint, or incomplete rename metadata widens the
  affected parent scope; the event does not prove either endpoint's current
  type.
- A copy covers the destination and, when supplied, the source as inspection
  scopes. Unknown or directory classification widens to the relevant
  `Subtree` scopes.
- A missing-parent indication widens to the nearest syntactically valid parent;
  if no parent/root boundary is established, it becomes `SourceAudit`.
- A symlink or reparse point is classified as an entry and is never followed to
  infer descendants. Unsupported target semantics widen the scope rather than
  escaping the root.
- A batch containing only unsupported or unknown event shapes is not a no-op;
  it becomes `SourceAudit`. Supported records in a mixed batch remain, with any
  uncertainty carried alongside them.
- An empty-folder event is evidence about a directory namespace, not proof that
  it has no children and not a synthetic sample entry. It maps to the directory
  `Subtree`; its deletion follows the missing-parent/delete rules.

Invalid or root-escaping paths produce uncertainty and cannot be dispatched as
arbitrary paths.

### Physical verification and commit gate

The normalizer cannot establish physical truth. Before traversal, the worker
must reacquire a live no-follow `SourceRootCapability` and revalidate the
physical root identity and lifecycle generation immediately before using it.
The source-writer owner must also reacquire a live no-follow
`SourceRootCapability` and revalidate the same physical root/lifecycle boundary
immediately before database mutation. Entry classification uses no-follow
operations; symlinks and reparse points remain entries and are never traversed.

A capability mismatch, root replacement, lifecycle mismatch, unavailable
capability, no-follow classification failure, or any other uncertainty forbids
the traversal/mutation and retains the last-good result while widening to the
affected `Subtree` or `SourceAudit`. It cannot be interpreted as absence,
successful deletion, a clean reconciliation, or checkpoint authority.

## Proof/root/generation boundaries

Every raw envelope carries `RawObservationProvenance`; every normalized scope
retains that provenance and its separate `Proof` state, plus event sequence and
replay cursor boundaries when available. A relative path is valid only under the
root that admitted it; absolute paths, `..` escapes, and unvalidated backend
paths are not proof of any other root. `WatcherContinuityProof` is valid only
when the replay adapter has established all of its identity, generation,
durable-acknowledgement, and contiguous-coverage fields.

Event IDs, cookies, and cursors support ordering and duplicate detection but do
not override source/root or generation checks. A scope cannot combine evidence
from different roots or generations. Checkpoint advancement requires a matching
committed boundary and valid continuity proof for watcher-derived targeted work,
so stale, dropped, overflowed, or uncertain evidence leaves the source
recoverable rather than falsely complete.

## Workflow outcome mapping

| Evidence at the boundary | Workflow outcome | Publication/checkpoint rule |
| --- | --- | --- |
| `RawObservationProvenance` with `Proof::Unproven` (including live notify, cancellation, rebind, stale-generation rejection, or queue eviction) | `WatcherAuthorityUnproven`; retain last-good state and request an affected-region/source audit | No targeted watcher authority or checkpoint; only a committed authoritative audit may publish and clear the uncertainty |
| Valid replay `WatcherContinuityProof` | Normalize to `ExactEntry`/`Subtree` and admit targeted workflow work | Source-writer commit and projection handoff are still required; a matching proof may then authorize the targeted watcher checkpoint |
| Invalid, missing, non-replayable, or gapped proof | Treat as `Proof::Unproven` and use the same `WatcherAuthorityUnproven` audit path | Never infer deletion or advance a watcher checkpoint |
| Explicit source/root audit committed | Authoritative reconciliation result | May replace the last-good projection and acknowledge the retained uncertainty through the audit's committed boundary |

## Queue and budget behavior

Admission is bounded per source/root and globally by event count and encoded
bytes, with a reserved capacity for an `Overflow`/`Error` marker. Ordering is
preserved within a generation. A noisy source cannot consume all capacity or
starve other sources; callback code never waits for a filesystem walk or a
database transaction.

When a budget is exhausted, the implementation may discard superseded raw
detail only while retaining an explicit `Proof::Unproven` uncertainty marker
with the relevant source/root, generation, time, and sequence boundaries.
Repeated pressure may coalesce markers, but it must widen the resulting scope
and remain observable. Queue eviction is not successful delivery, and a queue
drain is not an uncertainty acknowledgement. Durable retention of every raw
record is deliberately not specified here; the bounded uncertainty marker is
still retained by the owning recovery path.

## Compatibility seam to GuiMessage/workflow/source writer/projection/checkpoint

A later, separately reviewed adapter PR translates normalized scopes into the
existing `GuiMessage`/workflow entry point, carrying source/root identity,
generation, scope kind, proof state, paths, and uncertainty provenance. Existing
workflow and source-writer code remains the owner of source mutation and its
current transaction boundaries.

The raw envelope and pure normalizer retain backend order in their own lossless
representations. This document makes no claim that the current `BTreeSet`
coalescing or current `GuiMessage` path payload preserves that raw order. The
adapter PR must define any order-preservation or diagnostic handoff explicitly;
the current compatibility seam is not evidence of that property.

Projection continues to publish only what the source writer accepts. The
checkpoint continues to represent the last committed boundary, not the last
received notification. No new persistent `CommittedSourceDelta` schema is
introduced by this design; a future schema slice must preserve the same
proof-boundary rules.

## Lifecycle/cancellation/restart/crash recovery

Starting a watcher allocates a generation; stopping it cancels admission and
prevents post-stop delivery. Restarting or rebinding increments the generation
before new capture begins. In-flight records from an older generation are
rejected from delivery only after a bounded uncertainty record is retained; they
cannot mutate the new generation.

Every cancellation, rebind, stale-generation rejection, and queue eviction must
retain uncertainty for its affected source/root and sequence interval. That
uncertainty remains actionable until the source writer returns a committed
authoritative reconciliation acknowledgement whose committed boundary covers it,
including for an explicit authoritative source/root audit; audit completion alone
does not clear it. Cancellation completion, rebinding, dropping a
stale record, draining a queue, receiving a projection message, or receiving a
checkpoint request does not clear it. A restart with no replayable cursor is
`Proof::Unproven`, routes through `WatcherAuthorityUnproven`, retains the
last-good projection, and requests an authoritative audit.

After a crash, recovery resumes from the last committed checkpoint and requests
replay or a conservative `SourceAudit` for the uncovered interval. Missing
raw records are treated as missing evidence, not as proof of no change. Recovery
must be idempotent at the workflow/source-writer boundary and must not advance a
checkpoint until the corresponding work is committed and projected according to
the existing contract.

### macOS live registration capability

The legacy live backend, `notify` 6.1.1, synchronously starts its stream once
for the recursive source root and restarts it to add the nonrecursive parent.
Its worker sends the run-loop handle only after the Apple registration call
returns. Both initial registration and retry use this path, and the backend
joins its worker on retirement. An observed registration RPC stall took
17.043 seconds for the root and another 13.750 seconds for the parent.

Apple's [stream creation API](https://developer.apple.com/documentation/coreservices/1443980-fseventstreamcreate)
accepts multiple roots in one stream. The disposable QA probe linked below
confirms that both configured paths can share one successful start on this
host. Preserving the intended observation scope would require accepting source
descendants and root lifecycle evidence while excluding unrelated parent events.
Apple's [stream start API](https://developer.apple.com/documentation/coreservices/1448000-fseventstreamstart)
provides a synchronous result without a timeout or cancellation contract.
One registration call reduces duplicate work and still leaves the caller
exposed to that call's latency. Current in-process startup and retirement are
therefore unqualified for a hard deadline.

The macOS catalog uses the helper beside the application executable. Build a
local app with `scripts/build.sh` or use the exact-source bundle builder to
include the helper. The catalog owns at most 32 helper processes and retains
one slot per Source through removal, supersession, or failed readiness until
the original process is reaped. Checked readiness is tied to the Source,
physical root and generation. A missing or delayed helper leaves an
authoritative audit obligation. The two-second replay bootstrap and existing
durable checkpoint admission remain separate from live watcher hints.

The [disposable native registration probe](../scripts/qa/README.md#macos-live-watcher-start-probe)
measures successful registration and teardown. Its timings give no guarantee
about daemon stalls or event delivery.

## PR sequence with bounded library slices

1. Pure model/normalizer library PR: define the bounded raw envelope,
   `RawObservationProvenance`, separate `Proof`/`WatcherContinuityProof` types,
   and a pure conservative normalizer with deterministic tests. This PR has no
   admission supervisor, watcher callback, live/replay adapter, `GuiMessage`,
   `BTreeSet` coalescing, source-writer/database mutation, filesystem I/O, or
   persistent schema.
2. Separate backend-neutral library-only admission/lifecycle PR: add the
   bounded admission supervisor, lifecycle generation, queue budgets, fairness,
   cancellation, and acknowledgement boundaries. Use synthetic envelopes and
   acknowledgements for deterministic tests covering root mismatch,
   stale/duplicate delivery, budgets/fairness, cancellation/rebind/restart,
   eviction/overflow/error, and retained uncertainty. The tests must prove that
   retained uncertainty clears only after a committed authoritative
   reconciliation acknowledgement whose committed boundary covers it; audit
   completion alone cannot clear it. This is a required gate before any
   live/replay adapter or downstream workflow, projection, checkpoint,
   crash-recovery, or native integration.
3. Live/replay adapter PR: connect the validated library boundary to live
   notify and the replay adapter. Live notify must emit `Proof::Unproven` and
   preserve the merged #1024 `WatcherAuthorityUnproven` audit/last-good
   behavior; only the replay adapter may attach a validated continuity proof.
   The adapter PR must define the compatibility handoff without claiming that
   current `BTreeSet`/`GuiMessage` preserves raw order.
4. Downstream workflow/projection/checkpoint/crash/native integration: integrate
   committed acknowledgements with the existing workflow, source-writer,
   projection, and checkpoint boundaries, then validate crash recovery and
   native macOS behavior.
5. Handle directory truth, `schema/CommittedSourceDelta`, recursive hydration,
   a new backend/cursor, durable raw retention, and PR #980 only as separately
   scoped later work.

## Validation matrix

| Area | Target validation | Required evidence |
| --- | --- | --- |
| Contract | Check provenance/proof separation, field names, scope meanings, workflow mapping, and compatibility seam | Review of this design and `git diff --check` |
| Model/normalizer library (first PR) | Unit-test bounded raw envelopes, lossless/syntactic normalization, no I/O/type proof, proof non-escalation, and widening | Deterministic scope/provenance/proof assertions with no admission/lifecycle, adapter, or downstream dependency |
| Admission/lifecycle library (second PR; required gate before adapters) | With synthetic envelopes/acknowledgements, deterministically test root mismatch, stale/duplicate delivery, per-source/global budgets and fairness, cancellation/rebind/restart, eviction/overflow/error, and retained uncertainty | No live/replay adapter proceeds until this gate passes; no cross-root/generation delivery or false checkpoint; every uncertainty survives until a committed authoritative reconciliation acknowledgement whose committed boundary covers it; explicit authoritative audit completion alone is insufficient |
| Live notify | In the third adapter PR, drive representative macOS live notifications through the existing seam | `Proof::Unproven`, `WatcherAuthorityUnproven`, last-good retention, no targeted authority/checkpoint |
| Replay | In the third adapter PR, replay ordered/coalesced FSEvents batches, including an unavailable or gapped cursor | Only valid contiguous replay attaches proof; gaps use audit and last-good behavior |
| Commit/recovery | In the fourth downstream integration PR, interrupt before and after writer/projection/checkpoint boundaries, then restart | Idempotent recovery from the last committed boundary |
| Native acceptance | In the fourth downstream integration PR, use the normal writable macOS development profile with real sources | User-visible refresh/reconciliation behavior and no stale-generation effects |

### Redacted continuity evidence from the existing Finder fixture

The macOS replay integration test
`replay::macos::tests::disposable_profile_reports_finder_replay_continuity_and_audit`
is the machine-readable evidence harness for the redacted `finder_v1.tsv`
fixture. It replays supported fixture rows through a disposable source/profile
database, commits the contiguous 101–106 interval, closes and reopens SQLite,
and checks that a duplicate old proof cannot change the projection or durable
cursor. It then holds a valid contiguous 107–108 proof, retains the unsupported
fixture uncertainty, and checks that this otherwise-valid candidate cannot
publish or advance the barrier. The two emitted JSON lines are prefixed with
`YIELD_FINDER_CONTINUITY_EVIDENCE=` for capture. The payload contains only
stable fixture references, redacted stream identity, bounded sequence/revision
facts, and explicit blocked-native status; it never contains a source path or a
Finder filename. The profile and root are temporary and isolated. The fixture
uses synthetic callback records, so these records are not evidence of native
FSEvents delivery or Finder UI acceptance.

Run and capture the evidence from the repository root:

```sh
set -o pipefail
cargo test --locked -p yield-source-watcher \
  disposable_profile_reports_finder_replay_continuity_and_audit \
  -- --test-threads=1 --nocapture 2>&1 \
  | tee /tmp/yield-finder-continuity.log
```

Extract and validate the emitted JSON using the system Python standard library
(no Rust dependency or additional reporting framework). Keep the raw Cargo log
separate because it includes local compiler paths; the `.jsonl` file contains
only the two redacted records:

```sh
python3 - /tmp/yield-finder-continuity.log /tmp/yield-finder-continuity.jsonl <<'PY'
import json
import sys

marker = "YIELD_FINDER_CONTINUITY_EVIDENCE="
reports = [
    json.loads(line.split(marker, 1)[1])
    for line in open(sys.argv[1], encoding="utf-8")
    if marker in line
]
assert len(reports) == 2, len(reports)
required = {
    "schema", "case", "source_identity_ref", "root_identity_ref",
    "stream_identity", "audit_marker_stream_identity", "lifecycle_generation",
    "watcher_generation", "cursor", "barrier", "covered_region",
    "source_revision", "uncertainty", "audit_disposition", "outcome",
    "restart", "native_fsevents", "native_ui",
}
for report in reports:
    assert set(report) == required
    assert report["schema"] == "yield.finder-continuity-evidence.v1"
    assert len(json.dumps(report, separators=(",", ":"))) <= 2048
    assert report["source_identity_ref"] == "fixture-source-01"
    assert report["root_identity_ref"] == "fixture-root-01"
    assert report["restart"] == "actual_profile_sqlite_reopen_before_case"
    assert report["native_fsevents"] == "blocked_not_exercised_synthetic_fixture_only"
    assert report["native_ui"] == "blocked_all_UI_controls_paused"
    encoded = json.dumps(report)
    assert "/Users/" not in encoded and "/tmp/" not in encoded
    assert "kick.wav" not in encoded and "snare.wav" not in encoded

valid, uncertain = reports
assert valid["cursor"] == {
    "prior_ack": 100, "started_after": 100, "first_sequence": 101,
    "last_sequence": 106, "durable_ack_after": 106,
}
assert valid["barrier"] == {"before": 100, "after": 106, "advanced": True}
assert valid["covered_region"] == {"after": 100, "through": 106, "contiguous": True}
assert valid["source_revision"]["after"] == valid["source_revision"]["before"] + 1
assert valid["audit_disposition"] == "not_required"
assert uncertain["cursor"] == {
    "prior_ack": 106, "started_after": 106, "first_sequence": 107,
    "last_sequence": 108, "durable_ack_after": 106,
}
assert uncertain["barrier"] == {"before": 106, "after": 106, "advanced": False}
assert uncertain["covered_region"] == {"after": 106, "through": 108, "contiguous": True}
assert uncertain["source_revision"]["after"] == uncertain["source_revision"]["before"]
assert uncertain["uncertainty"] == ["unknown_event"]
assert uncertain["audit_disposition"] == "retained_source_audit"
with open(sys.argv[2], "w", encoding="utf-8") as artifact:
    for report in reports:
        artifact.write(json.dumps(report, separators=(",", ":")) + "\n")
print("validated 2 bounded, redacted Finder continuity evidence records")
PY
```

This output is test evidence only. Record the tested commit separately in review
metadata; do not place branch names, commit IDs, or machine paths in the bounded
payload. Native FSEvents capture and manual Finder/UI acceptance remain separate
gates.

## Explicit non-goals and risks

Non-goals are directory truth, recursive hydration, a new persistent
`CommittedSourceDelta` schema, a new backend or cursor, durable raw-event
retention, changes to existing source-writer ownership, current entry-type proof
at normalization time, and PR #980. This edit also does not change current
runtime behavior.

The principal risks are conservative widening causing extra audit work, backend
coalescing or cursor gaps producing frequent uncertainty, ambiguous rename/copy
semantics, symlink and root replacement edge cases, queue pressure during source
bursts, lossless path representation, and compatibility mistakes at the existing
message/checkpoint seam. Each risk is bounded by explicit provenance/proof
separation, generation rejection, retained uncertainty, reserved overflow/error
capacity, last-good audit fallback, and validation before a later slice is
merged.
