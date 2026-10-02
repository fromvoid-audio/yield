# Atomic exact source metadata enrollment (OPT-2064)

`PreparedExactScan::enrollment_request` captures bounded canonical command custody
from the existing real scanner preparation. The admitted caller supplies its original
OperationId and frozen SourceAffect/DestinationSourceReconcile key; this API does not
invent ordinals or admit journal work. Persist `proposal_bytes()` before dispatch.
`commit_enrollment` revalidates held files, root/path bindings, generic/index revisions
and cancellation, then applies exact audio/index facts through the existing single
SourceWriteBatch hook. Filesystem preparation/hashing finishes before SQLite begins.

The retained SourceWriter checks batch ownership, original source/root/database/binding,
lifecycle/lease/revision, exact final typed rows and bounded changed-key scope. It
assigns the source/index revisions and inserts an immutable command/result receipt in
that same transaction, reads evidence back, revalidates cancellation/ownership and
commits once. Any precommit rejection rolls back metadata, revisions and receipt.
Commit or postcommit acknowledgement failure preserves the original proposal and
reports uncertainty; it must not authorize a replacement identity.

On lost reply/restart, restore the original bytes with a fresh retained SourceWriter
and call `replay_manifest_enrollment` **before** preparing files or opening a mutation.
The original historical source observations remain immutable. Existing evidence is
read before new-work capacity checks: exact input returns its original delta/revision
without changing later state; different input under the same key conflicts. An absent
receipt cannot commit across changed historical generation/lease/revision. Restore is
custody, not a receipt or authority. Requests and receipts bind weak writer liveness;
handoff/close ends them. Fresh recovery issues a new live observation of the old receipt.

V1 bounds one command to 64 unique UTF-8 exact regular-file keys, 1024 bytes/32 components
per key, 32 KiB locator bytes and 64 KiB canonical proposal. The scanner retains its
64 MiB aggregate exact audio hash budget. Unsupported raw-path keys keep the ordinary
lossless index scan route; enrollment does not coerce them. Complete audio metadata
requires a canonical full hash and object identity. Index-only entries require complete
size/mtime/object/classification/policy and do not invent a content hash.

Each source retains at most 8192 immutable receipts and 16 MiB total stored key,
proposal, result, hash and scalar overhead, with no implicit eviction. Each proposal
and result is capped at 64 KiB. SQLite type/length predicates and ambiguity counts
precede evidence allocation. Domain-separated input/result hashes, canonical codecs,
version and structured-result checks reject corrupt evidence. Exact typed row comparisons
bound metadata readback before the existing manifest-result extractor allocates fields.

The privately issued receipt proves **source metadata enrollment**. Its `validate()`
checks the issuing writer and retained physical source/profile/lease bindings; it does
not claim the historical file still has the same content or revision. Consumers must
recheck their current source/file observations and applicable owner prerequisites.
A serialized ManifestCommitResult/entry or canonical proposal cannot mint a receipt.
No publication, absence, retirement, watcher continuity, power-loss qualification or
private journal completion follows from this metadata receipt. OPT-1787/1634 and the
full OPT-1645 family/checkpoint branches remain open.

Linux verification uses actual retained SourceWriter/profile-owned SQLite and scanner
held-file preparation. Seeded backend hashes and capacity rows are explicitly metadata
test inputs, not physical qualification. Relevant commands:

- `cargo test --locked -p yield-library --lib manifest_enrollment_owner -- --test-threads=1`
- `cargo test --locked -p yield-scan --lib prepared_enrollment -- --test-threads=1`
- Existing source transaction/scanner/fence suites and locked app/preflight guards.

Native macOS/Windows filesystem/lease semantics remain in OPT-2042/2043. Unix-only
replacement tests do not establish Windows replacement behavior.
