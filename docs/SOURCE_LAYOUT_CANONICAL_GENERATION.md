# Retained-owner canonical layout generation

OPT-2189 implements private preparation and a passive strict decoder, using the
actual OPT-2151 retained `SourceWriter` capture. It does not install a generation,
admit an operation, acquire capacity, publish readiness or qualify DSP/native
behavior. OPT-2154 durable command/scanner/store/recovery, OPT-2155 admitted atomic
publication, OPT-2156 current persisted reads, OPT-2157 catalog custody and the
original renderer/physical parents remain required.

`SourceWriter::prepare_layout_generation` accepts the real `SourceLayoutCapture`
and a borrowed `SourceLayoutComputedView`. The view is untrusted data, including
when adapted from OPT-2152's immutable existing-engine result. The library does
not depend on analysis. It revalidates the complete actual capture before and
after encoding, requires transaction quiescence, exact algorithm/version, ordered
sample coverage, finite raw coordinates and the fixed cluster policy. Fewer than
five rows have no clusters; five or more require nonnegative labels.

Preparation independently recomputes the CPU fingerprint from every actual
captured payload. Its framing matches OPT-2152: BLAKE3 over
`yield.starmap.cpu-input.v1` followed by one NUL, u64LE algorithm-byte length and
algorithm, u64LE row count, then each u64LE sample-ID-byte length, exact sample ID,
and 183 raw little-endian finite f32 values. Sample IDs retain exact
`source_id::relative_path` spelling. This is distinct from the physical-source,
manifest and feature-reference capture digest. Neither fingerprint is authority.

The private non-deserializable `SourcePreparedLayoutGeneration` holds canonical
bytes, digests, a genuine reference read fence and the exact writer's weak issuer.
Its validation is a read-only point-in-time observation, with complete current
recapture for consumption. There is a two-second cooperative deadline including
queue wait; existing source capture installs bounded SQLite progress checking.
Existing control-lease reads remain bounded separate reads, not interruptible OS
calls. Cancellation/deadline/drop cannot cause a SQL effect here. In-transaction
publication checks belong to the future admitted store.

## Canonical binary v1

Reserved metadata key: `source_starmap_generation_v1`. The future SQL owner must
retain the exact canonical bytes as BLOB, check SQL type and length before copying,
and preserve unknown/corrupt prior evidence. No BLOB-to-TEXT coercion or JSON
escape expansion is part of this format. Source metadata has TEXT affinity;
SQLite permits BLOB values there. Actual installation is not implemented here.

All integers use little endian. Length-delimited fields have a u32 byte length;
UTF8 identities retain exact spelling. Floats use exact finite f32 bits, including
signed zero. There is one fixed field order and no extensible field map:

| Field | Encoding / enforced bound |
| --- | --- |
| Magic/version | Eight bytes `YSLG`, `01 00 00 00`; all other versions reject |
| Source ID | UTF8 length field, at most 256 bytes, nonblank |
| Physical root | Raw length field, at most 4096 bytes, nonempty |
| Root binding and database identity | Each UTF8 length field, at most 512 bytes, nonblank |
| Historical origin revision | u64 <=i64::MAX, matching the source SQL integer domain; no serialized incarnation, lifecycle or lease |
| Algorithm and feature version | Length fields, exact `SOURCE_LAYOUT_ALGORITHM_V1` and `features-v1` |
| Input, CPU and point digest | Three exact 32-byte values |
| Row count | u32, 1–256, checked before row-vector allocation |
| Each ordered row | Path <=1024; file identity <=512; full lowercase content hash 64; opaque feature reference <=160; u64 size; i64 mtime; two finite f32 values; cluster tag 0 or 1, followed by i32 for tag 1 |

A final 32-byte BLAKE3 checksum covers every preceding encoded byte, including
the historical origin revision. It is required even though that revision is
excluded from the reusable input/point digests. Byte corruption cannot silently
change historical provenance. This checksum is consistency checking, not signing
or authentication.

Complete ordered manifest identity bytes (path + file identity + content hash)
remain <=128KiB. Paths must have canonical source-relative spelling and a WAV
extension; duplicates, aliases and duplicate file IDs reject. Feature references
must have the exact v1 shape and target hash matching source/file/content/version.
This is archived consistency checking, never reconstruction of a private key.

The conservative encoded bound includes the maximum five-byte cluster field per
row, the final checksum and all framing before allocating encoded storage. The maximum encoded
capacity is 512KiB. The decoder checks the input cap before inspecting fields,
checks every length before borrowing its slice, and checks the row count and a
minimum remaining-row length before allocation. Strings and root borrow input;
only bounded row storage and <=1024-byte path normalization temporaries allocate.
No recursive format or unbounded field collection exists. UTF8 errors, malformed
scalars, unknown cluster tags, nonfinite coordinates, impossible cluster shape,
unordered/duplicate rows, truncated bytes and trailing/unknown/duplicate fields
fail closed. Fixed order and exact lengths/bits give one representation for each
accepted record; there is no optional whitespace or alternate number encoding.

Decode recomputes the full manifest digest with the capture's shared v1 framing.
The point digest uses length-prefixed domain `yield-source-layout-points-v1`,
algorithm, input digest, CPU digest and u64LE count, followed by each
length-prefixed path, two raw f32LE values and tagged cluster. Historical decode
cannot authenticate the CPU fingerprint against absent payloads; only the live
preparation producer compares it with actual current captured payloads. Digests
provide consistency/content identity, not signing or current ownership.

An unrelated real metadata commit invalidates the old live observation while
preserving content and point digests. Fresh preparation records its historical
origin revision. A retained owner restart over unchanged content yields identical
canonical bytes at the same semantic revision and a new issuer. The future
admitted duplicate/CAS path must preserve an already verified generation's old
origin revision and bytes, issuing a fresh current read observation rather than
rewriting it. Decoding historical bytes never reacquires an owner.

## Validation

Actual retained-owner tests reuse production feature job/claim/publication and
real profile/source SQLite ownership. Fixture coordinates test preparation and
codec acceptance; they do not qualify PCA/t-SNE/HDBSCAN computation or physical
publication. Tests cover query-only/no DB or WAL changes, complete 256-row input,
wrong CPU/version/order/coverage, finite/cluster rejection, actual metadata commit
and restart, foreign/expired/replaced issuers, cancellation/transaction quiescence,
long quoted/Unicode path roundtrip, every truncation prefix, oversize lengths,
unknown version, historical-origin corruption outside reusable digests and trailing
fields. Codec tests separately exercise
checked overflow, UTF8/escape preservation and fixed cluster policy.

Run `cargo test --locked -p yield-library layout_generation -- --test-threads=1`,
`cargo test --locked -p yield-library layout_capture -- --test-threads=1`, full
serial library tests and `bash scripts/agent.sh preflight`. Exact executed results
and merge-tree verification are recorded on OPT-2189 when completed. Native
macOS/physical/DSP qualifications and all production store/integration parents
stay open.
