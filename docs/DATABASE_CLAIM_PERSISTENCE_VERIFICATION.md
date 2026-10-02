# Exact database claim persistence (OPT-2166)

The journal now has a private production persistence boundary for the exact
inventory defined in [WAL_REQUIRED_COMMIT_CLAIM_CONTRACT.md](WAL_REQUIRED_COMMIT_CLAIM_CONTRACT.md).
It preserves aggregate ordinary claims and stores their per-database spending
slices in the same transaction as the initial intent. This does not implement the
all-owner preparation latch, required-commit issuance, participant consumption or
physical qualification.

## Encoding and conservation

`operation_journal/database_claims.rs` implements canonical `YWCB` version 1:
64 databases, 64 total participants, 256 slices, 256 bytes per UTF-8 identity and
64 KiB total encoding. UUIDs, closed tags, checked SQLite integer ranges, source
versus global revision domains, ownership context, allocation-unit rounding and
explicit WAL/SHM subceilings are validated. Input ordering is normalized; decoding
rejects noncanonical ordering, duplicate IDs, conflicting physical aliases,
unknown tags/versions, malformed UTF-8, truncation and trailing bytes.

Every DB/WAL ordinary claim is covered exactly by slices of the same volume and
allocation class. Giving two databases the complete aggregate ceiling fails.
Unrelated payload/journal/recovery classes cannot be relabeled. All declared
database-writing participants must be present and source fences must match the
immutable operation envelope. The command-family digest uses the existing bounded
operation codec, its version and actual normalized typed command arguments, under
`yield.database-command-family.v1`, including the requirement tag. Mutable phases,
timestamps, checkpoints and OperationId are excluded.

These are immutable admission observations. A stored direction or original
revision is not proof that an inverse, lease renewal or later retry is currently
legal; OPT-2136/2168/2169/2170/2171 must derive and validate actual owner commands,
receipts, current fences and phase-specific transitions before any effect.

## Actual store boundary

Journal SQLite schema **11** adds `operation_journal_database_binding` and
`operation_journal_database_claim_slice`; operation record codecs remain V17.
The first binding row holds one canonical inventory blob; other binding rows
carry its exact nonce/digest and indexed identity. Slice rows contain exact
operation/binding/claim/participant, direction, class and ceilings. Foreign keys,
the unique nonce/ordinal anchor,
compiled SQL/column/index validation and exact image comparison reject conflicts.
Each relation is capped at 8,192 rows; restart visits indexed operation keys in
pages of 64 and bounded per-operation images. The attempt relation/state machine
belongs to OPT-2168, not this schema increment.

All supported older journal shapes migrate in the existing guarded exclusive
transaction. Unknown shapes remain unchanged and require audit. Legacy operation
and claim bytes are preserved; absent inventories stay absent. Migration does
not backfill a nonce or grant a required-WAL exception.

The same initial store transaction writes intent, ordinary claims and the entire
inventory/decision nonce. Precommit and committed readback compare the exact
image. Confirmed rollback removes all new rows and releases only definite
provisional charge; unknown commit/reply/rollback retains the lease. Public-to-the-
crate inventory readback rejects an unresolved transaction so uncommitted rows
cannot be mistaken for durable acceptance. Duplicate intent is rejected without
rewriting the image; exact nonce/digest lookup recovers the original OperationId
when the acceptance reply was lost. Reusing a nonce for another operation rolls
back instead of creating an ambiguous lookup. Lost-delivery reconciliation reads the original operation,
claims and inventory instead of allocating a replacement.

The actual private entrypoint takes an already-admitted owner-thread lease, checks
the current profile/acquisition and validates a complete journal/DELETE rollback
peak on the retained journal descriptor. It holds the existing extension-free
scope and reuses the bundled SQLite 3.50.2 capped pager policy: 512 MiB maximum
database, cache spill disabled, MEMORY temporary storage and compiled schema.
The conservative journal peak is `RECORDING_JOURNAL_PEAK_BYTES`, covering the
whole database plus rollback records/header allowance, rather than just payload
length. This must be included in the plan before OperationId/provisional admission;
an unrelated volume's journal row is insufficient. A native physical provider,
protected reserve and genuine all-owner latch are still required by their owners.

Bound ordinary claims cannot be released until actual attempt settlement and
allocation/retirement evidence exists. Restart also rejects a prematurely released
bound claim, missing root/slice, corrupt digest, orphan or conflicting image and
retains evidence. Neither a canonical digest nor a matching UUID creates a permit.

## Verification and limits

Six codec tests cover canonical/truncated/malformed encodings, shared-volume exact
conservation, aliases, duplicate IDs, roles/generations/ranges/directions, aggregate
identity bounds and distinct global/narrow revision domains. Thirteen actual
profile-owned SQLite tests cover two databases sharing aggregate claims, atomic
rollback, uncertain commit/lost reply, exact reopen, source/command/omitted inventory,
corrupt/orphan retention, duplicate/release rejection, v10 migration, both global
relation ceilings, unresolved-transaction readback and uncovered descriptor budget.
Nonce lookup/rebinding also remains exact across reopen. Point readback checks
the immutable intent and original aggregate ledger, rejecting a corrupt claim
instead of treating the binding image alone as accepted evidence. Recording controls
and database inventories use the same initial transaction. A combined real SQLite
case covers success, confirmed rollback, lost commit reply, lost committed
readback and corrupt co-committed recording controls; inventory point readback
and restart audit reject the corrupt control evidence.

The positive store tests use an explicitly synthetic capacity provider and nonce
observations through the same private atomic transaction code. They prove real
SQLite persistence and logical accounting; they do not supply actual source/global
reservations, a sealed all-owner decision, required-write permission, physical
allocation or power-loss evidence. The existing Linux-unavailable qualified owner
route stays fail closed. OPT-2136, OPT-2168–2171, OPT-2180 and native qualification
OPT-2173/2174 remain open, as do the original WAL tracking parents.

```sh
cargo test --locked -p yield-library --lib operation_journal -- --test-threads=1
cargo test --locked -p yield-library --lib -- --test-threads=1
bash scripts/agent.sh preflight
rustfmt --check --edition 2024 crates/yield-library/src/operation_journal/store/database_claims_tests.rs crates/yield-library/src/operation_journal/store/recording_retention_tests.rs
git diff --check
```

Relevant existing migration fixtures are reconstructed as their exact older
shapes, including removal of the two new empty relations. Strict older-schema
validation and unknown-trigger rejection are preserved.
