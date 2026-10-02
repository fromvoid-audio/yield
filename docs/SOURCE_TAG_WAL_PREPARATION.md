# Owned source tag preparation (OPT-2222)

`SourceWriter::prepare_tag_wal_reservation` consumes the actual source writer and
its original `SourceTagAdmissionObservation`. It returns an opaque, non-Clone
`SourceTagWalPreparation` that owns the same connection, an empty immediate
transaction, the entered finite metadata policy, and its extension-policy read
guard. The guard remains on the owning thread. Ordinary `SourceWalPreparation`
keeps its existing behavior and thread traits.

The effective cooperative deadline is the minimum of the requested deadline and
both original capture deadlines. Retry, queue delay and repeated validation
cannot refresh it. The caller must abort or drop an expired preparation; expiry
alone never proves rollback or releases ownership.

Before entry and again while the transaction is held, validation checks the
original issuer, complete source revision/lifecycle, strict retained profile,
media root, database-file and live lease bindings, the original measured
DB/WAL/SHM facts, known schema/runtime, normal-tag ID and canonical identity,
complete active directory publication/generation, and every sorted assignment
(up to 64 for rename or 128 for delete). Held validation reads directly through
that connection without a nested BEGIN. Owner validation reuses OPT-2208 and
never renews the lease or falls back to command admission.

The borrowing `SourceTagMetadataPolicy` and this owning composition share a
private policy state that stores every original setting before installing any
restriction. Its extension guard remains retained even after partial entry.
The owning path retains the original busy timeout while the reservation is
held. Cleanup confirms rollback before restoring timeout, maximum page count,
temp store, cache spill, and autocheckpoint settings. Only confirmed restoration
returns the exact writer to normal dispatch. Failed rollback retains custody for
explicit retry; failed policy restoration is sticky and poisons the retained
owner until actual close/reopen. Drop attempts cleanup and closes an uncertain
connection before releasing its source lease and extension guard.

This is production policy/preparation composition, not journal admission.
It allocates no source/control metadata, revision, receipt, OperationId, intent,
claim or effect. The public surface offers observations, expiry, revalidation
and abort only, with no connection or mutation grant. OPT-2099 still requires
actual common/native claims, pre-intent latch, persistence/readback and private
admission proof; OPT-2049 requires actual assignment and immutable V2 receipts;
OPT-2012 requires dictionary-wide finalization. These parents remain open.
OPT-2204 and OPT-2191 own their separate layout/actor consumers. Physical reserve,
CoW and power-loss qualification remain separate platform scopes.

## Linux verification

Real profile-bound source databases, source leases and SQLite connections cover
exact-handle restoration and subsequent genuine commands; external writer
exclusion; validation with BEGIN denied; full-size rename/delete scopes with the
last assignment changed; stale issuer/revision, schema and directory publication;
original deadline expiry; near/expired leases without renewal; hard WAL pressure
and an actual per-file size failure; media-root, main-file and profile
substitution; real SQLite rollback/PRAGMA failures, partial entry, sticky poison,
and safe drop/reopen. Fault injection changes only test-owned connection/file
objects and does not qualify any physical backend.

```sh
cargo test -p yield-library --lib tag_owned_preparation -- --nocapture
cargo test -p yield-library --lib tag_metadata -- --nocapture
cargo test -p yield-library --lib wal_reservation -- --nocapture
cargo test -p yield-library --lib --locked -- --test-threads=1
bash scripts/agent.sh preflight
python3 scripts/internal/check/check_gpui_action_register.py --check
```

Source, lease-control rows, revision and receipt observations are compared across
empty preparation. Deliberate fixture corruption is installed before those
comparisons, or rolled back as an explicit validation fault within the held test
transaction. No production mutation acceptance or unavailable macOS/Windows
physical qualification is inferred from these portable tests.
