# Recording participant preflight (OPT-2361)

The actual original recording pre-start Store reader checks its complete SQLite
snapshot and 64 KiB row ceiling before `codec::decode_prestart_recording`. That
scoped decoder previously built its quota-limited JSON graph before discovering
that a participant array exceeded the existing `MAX_PARTICIPANTS = 64` limit.
The exported `RecordingProposal::decode` also constructed its generic JSON graph
before participant validation. A private borrowed preflight now counts the known
arrays before either construction boundary.

## Scope and acceptance

Each participant set separately permits up to 64 descriptors and 64 checkpoints.
The schema17 recording envelope has two sets: `universal_intent.participants` and
`universal_command_args.Recording.participants`. Counts saturate at 65, but the
scanner consumes the complete input before applying a refusal. Unknown siblings
are skipped; their bytes are not validated as an accepted partial inventory.
The envelope is recognized only with integer schema17 and exactly one Recording
argument. Scanner-unsupported shapes, syntax errors and trailing data fall back
to the original bounded reader. The scanner itself explicitly refuses to scan
more than 64 KiB on that private route, leaving larger private inputs to the
unchanged bounded decoder; the actual Store already imposed this same limit.

For a complete recognized over-cap envelope, the scoped factory returns its
existing resource refusal (`RecordingPrestartResourceBound`, mapped to Full).
This conservative resource decision can precede a malformed diagnosis, including
malformed entries, duplicate outer envelope/schema/Set fields or unknown siblings. Repeated
Recording keys inside Args make that map non-singleton and retain the original
parser refusal.
Recognition is not schema acceptance. No partial inventory reaches rebuild or
admission, and original bytes, keys, sequences, charges and rollback/autocommit
state remain retained. Generic `codec::decode`, `decode_with_limits`, their quotas,
all other versions and generic ParticipantSet deserialization remain unchanged.

The public proposal route retains its existing 128 KiB wire ceiling and invalid
observation category. Inputs within the count limits continue through its exact
original strict tree/typed validators, preserving depth 24, known-field duplicates,
unknown fields, semantic validation and trailing-data rejection. Historical
proposal sequence position 4 and participant-set sequence positions 0/1 correspond
to the unchanged declared field order. Comparison tests exercise valid 64/64
sets in canonical maps, escaped noncanonical maps and nested array forms. The
count cap does not guarantee success under the factory's separate JSON quotas.

## Separate backing and lifetime accounting

The scanner owns no Value tree, participant vectors, key strings or serde Content
graph. Its identifiers and shape tags are finite enums, its State is three bools,
and the count saturates at 65. Compiled guards require one-byte identifiers/tags,
three-byte State and a Scan header of at most16 bytes; these are not total stack
or allocator/RSS bounds. Semantic visitor nesting is fixed by the closed envelope.

Pinned serde_json 1.0.151 SliceRead uses one scratch Vec for escaped identifier
bytes; IgnoredAny uses that same Vec as an iterative container-frame stack.
Ignored strings bypass owned decoding. Decoded identifiers are no longer than
the source wire, and skipped container depth is at most half its wire bytes.
UTF-8 escape reserve slack is covered by the longer source escape. On pinned
Rust 1.93.0, amortized byte-Vec backing is at most twice the wire ceiling; during
reallocation the old backing is less than one ceiling while the new backing is
at most two. Raw 64 KiB plus this 192 KiB transient fits256 KiB. The existing
factory budget's RAW_AND_SCRATCH category grows192→256 KiB, consuming its prior
64 KiB headroom exactly; compile-time category equality requires the unchanged
3,604,480-byte command debit. Pool ceilings and retained-route credits do not grow.
Scanner scratch and errors drop before the original owned decoder begins.

Public proposal scanning has a separate 128 KiB ceiling: scratch old/new overlap
can reach 384 KiB plus caller-owned raw 128 KiB. That component observation is not a
complete public-proposal or owner credit proof. The standalone allocator probe
measures large null arrays and near-ceiling escaped unknown keys separately.
Allocation requests are not allocator overhead, resident memory or peak RSS.

Long unknown keys map to an inline Other identifier. Wrong-shaped string values
and schema tags use fixed scanner diagnostics instead of copying their contents.
Other scalar/type and JSON syntax diagnostics use finite labels and numeric
positions. Scanner error backing is separate from scratch and is destroyed on
fallback; existing downstream error formatting/copying remains unchanged and is
not claimed to have a complete proposal-wide bound. CPU is linear in capped wire
per scan; accepted inputs then incur the existing decoder's separate pass.

## Evidence and remaining work

Before production changes, two actual regression tests failed: the public proposal
and genuine retained-row Store reader each entered its owned parser once for an
existing 65-entry input, versus the required zero. Actual Store cases cover all
four arrays and retain exact operation key/sequence/BLOB through rollback and
reopen. The normal-build public allocator probe reports largest/cumulative requests45/53
bytes for both dense null arrays (101,695/102,306 wire bytes). Escaped-key inputs
65,518/131,054 wire bytes report largest126,968/258,040 and cumulative190,497/387,105
bytes respectively. These exact-input requests substantiate the separate pinned
scratch assumptions; they are not resident-memory measurements. Actual retained
noncanonical and embedded-array proposal acceptance is also exercised through the
Store read/rebuild/reopen path. Existing original snapshot/resource tests remain
required. Scalar test fixtures describe stored data only.

OPT-2145/2058 startup, required-WAL admission, private terminal integration,
Finalizing/Frozen/seal storage and physical qualification remain open. OPT-2326
still requires genuine macOS verification2330. This leaf supplies no producer
startup, binding extraction, effect permit, retry, refund or genuine settlement.
