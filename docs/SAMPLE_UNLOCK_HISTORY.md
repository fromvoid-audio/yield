# Shared acknowledged sample unlock history

OPT-2209 implements the in-process UnlockKeepRating adapter from
[SAMPLE_METADATA_UNDO_CONTRACT.md](SAMPLE_METADATA_UNDO_CONTRACT.md). The existing
sample context action submits SetLocked(false) through the retained catalog/source
curation route. Confirmed rating 3 and lock true are required. One atomic batch of
at most 128 changed paths can retain history; larger edits retain ordinary admission
and explicitly report absent history coverage.

The existing bounded curation history lane now stores a field-specific collection
or sample-lock payload. Sample and collection records share chronological undo/redo,
128 logical records and 16 MiB including pending and presentation copies. Sample
records additionally have a 1-MiB ceiling, checked before cloning. Uncommitted
candidates never evict redo to obtain coverage. Only validated committed edits clear
redo. Pending/staged/latest-value or unresolved lineages do not supply predecessors.

Sample undo explicitly sets lock true and redo sets lock false at the current
acknowledged ExactRevision. A direction is settled only with one actual receipt,
exact root/directory/source lifecycle/request, complete unique changed-path set,
rating 3, expected lock and revision increment. Source timestamps come from each
actual new write; no historical timestamps or complete old rows are restored.
Collection partial history remains the exclusive top, including over older sample
records. Cached controls capture the original record and context revision and
revalidate them before dispatch. Existing controls show Undo unlock/Redo unlock for
sample entries and retain compound collection direction labels.

Selection, focus and viewport restoration reuse stable identity checks. Rejection,
no-op and duplicate delivery create no record. Lost or stale publication is not
rollback evidence and cannot authorize an inverse; history remains unavailable until
authority is refreshed. Ordinary admission bounds and existing collection ownership
are unchanged. No new SQL writer, journal family, session coordinator or parallel
stack is introduced.

Actual GPUI tests use the explicit catalog fixture, real profile-owned SourceWriter
receipts and independent retained SQLite snapshot reads. Consumer malformed-row
faults derive from real committed events; they cannot manufacture owner receipts.
Native normal-bootstrap and physical input verification is scoped in OPT-2233.
The tests do not qualify normal native bootstrap, physical input or history across
restart. OPT-1619/1476 and durable sample leaves OPT-2210/2211/2212 remain open.

```sh
cargo test --locked -p yield-ui sample_history --lib
cargo test --locked -p yield-ui collection_history --lib
cargo test --locked -p yield-ui --lib
bash scripts/agent.sh preflight
```
