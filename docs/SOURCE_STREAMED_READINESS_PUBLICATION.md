# Streamed audited readiness publication (OPT-2072)

The production `SourceWriter::prepare_feature_readiness_target` keeps its small
256-file/128-KiB path and falls back to the retained writer's streamed publisher
when that bounded preflight reaches its row or aggregate-text limit. The initial
preflight is bounded, not a second full manifest scan. The same real catalog
request now admits larger sources without constructing a whole resident matrix.

One SQLite transaction holds the audited manifest snapshot. Keyset pages contain
at most 64 rows and 128 KiB path/identity/hash text; SQL checks storage class/byte
length before allocation. Each path is at most 1024 bytes, identity 512 bytes,
hash exactly 64 lowercase full-file BLAKE3 hex bytes. At most 193 target DTOs are
resident, including the page-local validation-only source shape; it is never
inserted as a partial source target. Resident manifest/target/validator text and
bounded containers have a conservative one-MiB representation budget, independent
of source cardinality. Existing SQLite cache/OS allocations are separate; this is
not a process RSS or physical-storage guarantee.

Each attempt admits at most 16,384 files and two seconds of SQLite work. A SQLite
VM progress deadline bounds otherwise long scans/joins; cancellation is checked
between row/page/target operations and at finalization. SQL work interrupted by
the deadline becomes AdmissionBudget; the product reports preparation's work
limit, rather than ready. OS ownership validation/commit latency cannot be killed
at a hard wall-clock deadline or release the retained writer underneath a call.
The command remains serial under that owner. This publishes no completion count
or percentage; existing preparation activity stays indeterminate and bounded.

Every page revalidates the exact original writer instance, source/root/database,
lifecycle, lease and complete authoritative audit revision. Transaction-local
bounded audit reads additionally reject revision/audit changes within the write
transaction. The existing target validator checks complete per-file stage shape
within each bounded page. Existing SQL primary keys reject duplicate identities
across pages. Final persisted targets must match manifest paths/identities/full
hashes/eligibility/versions, the streamed source-membership token and exact whole
matrix cardinality. Legitimately identical content hashes for different file
identities remain valid; content equality does not merge file ownership.

Targets, real manifest IndexedIdentity evidence, source state and the existing
digest/count-bound durable wake commit atomically. The unchanged eligible-embedding
membership contract remains zero because embedding/layout producers are still
unconnected; the unsupported layout target separately binds all actual manifest
members. WAV features remain eligible; no other stage becomes falsely complete.
Existing unchanged feature payload/marker/work survive publication. Incompatible
contracts, unavailable/pending hashes, duplicate identities, invalid paths,
incomplete audit, cancellation, owner/revision/root changes or limits roll back.

The transaction is the temporary staging boundary. Other read connections see
the preceding generation until commit, never partial pages. There is no persisted
staging table or staged receipt to adopt after a crash. SQLite transaction rollback
retains the preceding generation; restarting retries from the first bounded page.
If matrix commit succeeds but queue admission has not happened, existing idempotent
publication/enqueue retry applies. No cross-database atomicity, power-loss
qualification or resumable partial-generation authority is asserted.

Linux evidence: a 1,026-entry owned SQLite long-path fixture publishes in 17 pages
(about 170–200 ms in the initial debug runs), over 600 KiB aggregate input, while
preserving an actual owned payload across repeat publication and restart. Its
extra indexed rows/finite feature vector exercise storage contracts; they are not
physical DSP qualification. A separate production catalog test audits 301 real
WAV files with the actual scanner, claims held Linux input, computes the real DSP
vector and validates stored payload/readiness. Negative fixtures cancel after
pages, replace the actual root, corrupt a late hash/identity, change transactional
audit/revision/target data, abort SQL, exhaust elapsed budget, and omit the requested
file; the preceding generation remains visible/recoverable. Native readable input
implementations OPT-2061/2062 and physical GPUI evidence OPT-2075 remain open, as do
source-processing and GPUI tracking parents.

```sh
cargo test --locked -p yield-library --lib manifest_readiness -- --nocapture
cargo test --locked -p yield-library --lib sample_sources::readiness
cargo test --locked -p yield-catalog feature_request -- --nocapture
bash scripts/gpui-development-checks.sh check
```
