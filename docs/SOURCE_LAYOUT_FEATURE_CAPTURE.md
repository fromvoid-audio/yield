# Retained-source layout feature capture (OPT-2151)

`SourceWriter::capture_layout_features(expected, cancel, deadline)` captures
complete supported, non-missing audio membership through its existing retained
SQLite connection. It requires the exact current audited physical source/root/
database/lifecycle/lease/revision. Capture performs no readiness publication,
job admission, SQL repair, lease renewal, filesystem traversal or writable open.

The initial producer reads at most 257 ordered manifest rows and rejects the
257th. Paths are canonical exact UTF-8 keys of at most 1,024 bytes, file identities
at most 512 bytes, and content identities full lowercase BLAKE3 hashes. It checks
SQL types before copying bounded fields, size/timestamp facts, duplicate file
identities and a 128 KiB aggregate manifest limit. Missing or unsupported current
v1 inputs reject the complete capture; they never yield a partial source layout.
WAV path candidates remain included when their stored extension is malformed or
disagrees with the path; that disagreement rejects rather than silently omitting
an audio member. Normal unsupported formats and AppleDouble sidecars retain the
existing supported-audio selection semantics.

Each `SourceLayoutFeature` borrows its exact `source_id::relative_path` sample
identity, browser file identity, current feature target, actual immutable feature
key and validated 732-byte payload. The existing feature health/current-target/
completion validators distinguish missing payload, corrupt payload, ambiguous
provenance and missing completion; absent or unsupported current targets retain
an explicit prerequisite reason. No evidence is repaired or discarded.

`SourceLayoutCapture` has private fields and no deserialization or public
constructor. It retains only a weak writer identity and existing read fence.
`validate_read_fence` checks issuer lifetime and audited scope;
`SourceWriter::validate_layout_capture` additionally recaptures complete current
membership and feature keys before comparing the input digest. Both remain
point-in-time read observations, not admission or mutation authority.
Public capture and validation require transaction quiescence and reject an active
transaction explicitly. The future admitted store must additionally implement its
own exact checks within the publication transaction; this producer supplies no
in-transaction publication validation or write receipt.

The length-prefixed digest binds exact physical source and ordered manifest facts,
feature references/version and `SOURCE_LAYOUT_ALGORITHM_V1`, which names the
existing L2/PCA-t-SNE/seed-zero/HDBSCAN policy. Writer incarnation and lease remain
separate live checks so a new owner can reissue a capture over unchanged persisted
inputs after restart. Every owned field is conservatively charged before its
target/key/payload copies; retained input is capped at 1 MiB. Execution uses a
two-second cooperative deadline, including caller queue wait, SQLite progress
checks and row/feature checkpoints. Cancellation rejects a result; it does not
interrupt or detach an operating-system call.

```sh
cargo test --locked -p yield-library --lib layout_capture
cargo check --locked -p yield
bash scripts/agent.sh preflight
```

Fixtures use actual isolated writable profiles, retained source owners, real
temporary file identities/full hashes and production feature publication/lookup.
Negative malformed/limit cases inject data only through the held fixture owner.
Synthetic feature vectors exercise stored input custody; they do not qualify DSP,
layout computation, physical effects or power-loss durability. This producer
supplies no layout SQL generation or ANN/full-analysis readiness. OPT-2152–2157,
OPT-1709/1485, large-source/native and tracking acceptance remain open.
