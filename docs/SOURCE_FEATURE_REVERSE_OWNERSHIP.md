# Source-local feature reverse ownership and supersession maintenance

OPT-2121 adds `source_feature_reverse_owners` to the existing retained source
database. Each actual `SourceWriter::publish_feature_artifact` transaction records
the exact source ID, stable file identity, content generation and feature version
alongside immutable bytes and claimed completion. Path is deliberately excluded.
Exact duplicate publication validates the descriptor; conflicting descriptors,
capacity rejection or insertion failure roll back completion and payload changes.
An exact validated publication can adopt a descriptor-less legacy payload.

The inverse table is capped at 4096 entries by publication admission, with bounded
UTF-8 fields. It has no deletion trigger or cascading foreign key: corrupt-payload
repair from OPT-2101 still proves one exact payload deletion, and a missing payload
leaves its inverse descriptor as evidence for exact rebuild. Ambiguous or orphan
descriptors are retained; this leaf supplies no general orphan cleanup permission.

`SourceWriter::prune_superseded_feature_artifacts` is a normal-build retained-owner
command. A privately issued cursor binds enumeration to the issuing writer and
audited revision. Each call examines at most 64 ordered payload keys, materializes
one bounded descriptor/payload at a time, and has a two-second SQLite progress
deadline. Indexed identity/path and exact work-control lookups avoid full manifest
or job scans. More than 64 aliases, unknown observations or unavailable current
source state cannot authorize removal. Cancellation, owner/profile/root/database
and transaction-local audit/revision checks precede commit.

Removal requires recomputing the target key, reference and digest from the stored
inverse descriptor and validated 732-byte finite vector. A current accepted
manifest alias with the same identity/content preserves bytes, including path
renames. Every observed alias must have known live superseding content before the
old payload can be removed. Missing entries, missing/unknown hashes, offline or
retired sources, malformed controls, descriptor-less legacy payloads, corrupt bytes
and active exact claims retain evidence. Absence of a configured source or path
is never a filesystem absence or retirement proof.

One owner `BEGIN IMMEDIATE` transaction removes only the proven superseded payload
and its inverse row. Both affected rows, exactly two SQL changes and final absence
must be confirmed; unexpected trigger effects roll back. Actual deletion also uses
the existing retained hard/unknown WAL gate; it has no required/recovery exception.
Read-only retention does not require a deletion admission. Durable processing jobs,
readiness markers, user metadata and unrelated cache bytes remain. The result
reports inspected and committed removed rows, with 732 logical payload bytes per
removed row. SQLite pages, WAL and filesystem allocation may remain: this is not
physical reclaimed-byte accounting.

Actual profile/source-owned SQLite tests exercise selective supersession,
unchanged/renamed content, missing and shared alias evidence, exact legacy adoption,
conflict/capacity rollback and retry, active controls, cancellation, cursor fencing,
writer handoff, restart persistence, actual WAL pressure/retry and trigger rollback.
The existing legacy journal weak-cursor fixture now asserts the reopened retained
database and 17 persisted rows under descriptor pressure; an expired owner fd path
is not a persistence assertion. No reader production code changes. Fixture vectors and
injected retained SQL controls qualify these storage boundaries only. Existing real
catalog scanner/WAV-DSP regressions also run; neither suite qualifies physical
cleanup, native adapters, crash durability or a latency SLO.

OPT-1656/1659 and their original external/shared cache, deletion/absence and
source-retirement requirements remain open. Native OPT-2055/2061/2062/2075 retain
their platform acceptance. This leaf adds no background pruning scheduler, second
SQLite owner, path-based mutation API, physical filesystem effect or authority
constructor.
