# Durable journal metadata prerequisite

OPT-2237 implements the private `OperationJournalStore::prepare_database_attempt_metadata`
precondition for the attempt persistence work in OPT-2206. It reads the actual stored operation,
canonical database inventory and original aggregate database claims, then validates the durable
ordinary `JournalRecord` claims against the immutable full capacity plan. Missing, extra,
corrupt, changed-volume, changed-byte or released journal claims reject the check.

The existing initial-binding metadata policy is reused: actual journal descriptor identity and
descriptor-derived volume, compiled schema and bundled SQLite 3.50.2, no temporary schema,
MEMORY temporary storage, disabled cache spill and a 512 MiB `max_page_count` cap. The same
original plan must cover the conservative complete peak on that descriptor's volume:

```
2 * 512 MiB + (512 MiB / 512) * 8 + 2 * 65536 = 1,082,261,504 bytes
```

This counts the entire bounded database, including indexes and retained control images, again
alongside its maximum DELETE rollback copy, old-page records and header/alignment allowance.
It never subtracts existing allocation as free capacity. Future attempt insertion must respect
the same database cap; exhaustion retains old evidence and refuses new rows.

The private method returns the actual process extension-exclusion read guard. The future journal
issuer must retain it until its metadata transaction has quiesced and must revalidate namespace
identity before commit. A successful check is a logical metadata precondition; it supplies no
source/global registration, physical capacity, protected reserve, legal command, attempt nonce,
single-use transaction permit, reconciliation or refund authority.

Actual profile-owned SQLite tests exercise valid durable readback and reopen; undersized peak,
wrong descriptor-volume coverage, missing/extra/changed/released journal claims, corrupt database
binding, unexpected permanent/temporary schema and unconfirmed transaction rejection. Every
case checks retained row bytes. Positive checks run in a fresh test process so unrelated tests
that permanently load SQLite extensions cannot bypass or nondeterministically taint the real
extension-exclusion seam. Capacity/provider observations in the fixtures remain explicitly
logical and do not qualify any physical backend.

OPT-2206 remains open for the actual bounded attempt relation, immutable start/outcome images,
nonce and phase/command validation, rollback/uncertainty handling, conserved allocation and
paged restart/release audit. OPT-2168 and its consuming owners remain responsible for genuine
issuance and receipt reconciliation. This prerequisite adds no schema or production owner route.

Verification:

```
cargo test --locked -p yield-library --lib attempt_metadata_checks_run_in_an_extension_free_process -- --nocapture
cargo test --locked -p yield-library --lib
bash scripts/agent.sh preflight
```
