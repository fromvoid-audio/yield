# Audited source readiness admission (OPT-2071)

`SourceWriter::prepare_feature_readiness_target` derives a complete readiness
matrix from the retained, fully audited supported-audio manifest and returns
one current file's `features-v1` target. Production callers can now prepare a
real job without constructing a readiness matrix in a fixture or GUI process.
This callable command is a prerequisite of the catalog/session/GPUI adapter
OPT-1708; that production adapter is now implemented.

The initial contract `yield-audited-feature-matrix-v1` admits at most 256 files,
128 KiB combined path/identity/hash input, 1024 bytes per path, 512 per identity
and a lowercase full-file BLAKE3 hex digest. SQL checks byte lengths and types
before materializing strings, and reads at most 257 rows. Missing/pending hashes,
duplicate identities, invalid paths, incomplete or stale audit metadata, changed
owner/root/revision and cancellation fail before target mutation. This bounded
small-source route remains supported. Its row/aggregate-text budget rejection now
falls back to [bounded streamed publication](SOURCE_STREAMED_READINESS_PUBLICATION.md)
(OPT-2072), rather than rejecting every larger valid manifest.

The source generation is the current authoritative manifest revision. Readiness
publication revisions increment under the sole writer. Existing incompatible
contracts fail closed, requiring an explicit migration decision. The full matrix
and membership digest go through the existing complete-membership validator.
Targets, IndexedIdentity evidence and the durable readiness wake share one
transaction, including rollback and cancellation checkpoints. IndexedIdentity
means the committed manifest contains the exact identity/content generation;
it does not qualify physical publication or assert that bytes are unchanged.
The feature backend separately verifies held input metadata and full-file bytes.

Only index evidence is completed by this command. WAV feature targets are
eligible; unconnected embedding/layout stages and unsupported feature formats
remain explicitly unsupported. This feature-only contract supplies no embedding
or layout success. Their production adapters and contract upgrade remain open.
An unchanged feature completion/payload survives a repeated publication;
changed content gets a distinct exact target and cannot adopt old completion.

Queue the selected target separately with the retained writer's bounded
`enqueue_readiness_jobs`, then use the OPT-1922 prepare/compute/finish backend.
This ordering is recoverable: a crash after matrix publication can repeat it
and enqueue missing work. It does not report a queued or ready result before
those commands succeed. Publication does not itself start a scheduler or GUI.

Verification includes real profile/source SQLite tests, negative manifest/input
budgets and transaction rollback. The catalog WAV regressions now call this
production publisher instead of manually constructing readiness tables, then
compute the actual existing DSP vector and publish actual payload/readiness.
Source-store tests using finite synthetic payload vectors prove SQL preservation
only; they are not DSP or physical qualification. Native readable input adapters
remain OPT-2061/2062, and native source owner regressions remain OPT-2055.

```sh
cargo test --locked -p yield-library --lib manifest_readiness -- --test-threads=1
cargo test --locked -p yield-library --lib readiness -- --test-threads=1
cargo test --locked -p yield-library --lib feature_artifact -- --test-threads=1
cargo test --locked -p yield-catalog --lib feature_job -- --test-threads=1
bash scripts/gpui-development-checks.sh check
```

Run owner-boundary, source competing-writer, filesystem-producer and action-register
guards too. Exact commands/counts, head/merge equality and limitations belong on
the issue and PR. Tracking parents and the production GPUI adapter remain open.
