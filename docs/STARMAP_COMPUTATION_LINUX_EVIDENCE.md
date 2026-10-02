# Bounded Starmap computation on Linux

This implements OPT-2152's computation leaf from the
[publication contract](STARMAP_LAYOUT_PUBLICATION_CONTRACT.md). It does not
implement capture, admission, publication, restart loading, catalog dispatch,
GPUI presentation, or large-map/native qualification. Those remain separately
tracked under OPT-1709 and OPT-1485.

`yield_analysis::starmap_layout::compute_starmap_layout` accepts borrowed ordered
identities and 183-component feature vectors, with no writer or I/O capability.
It validates the complete input before copying: at most 256 unique identities,
1,282 bytes per identity, 197,120 aggregate identity bytes, and finite vectors
with a usable existing f32 L2 norm. Checked directly owned buffers must fit
1 MiB. The identity budget allows the capture's 128 KiB manifest plus its bounded
source prefix repeated for every row; counting those prefixes under the manifest
budget would reject a valid complete capture. Identity strings are allocated once
and moved through computation; the estimate conservatively allows two copies.
Engine-internal workspace is separate from that conservative bound.

The helper calls the existing L2 embedding, PCA/t-SNE (seed 0, existing 1,500
iterations), and HDBSCAN implementations unchanged. The exact version string
binds dimension, normalization and clustering policy. It requires one finite
point per input; fewer than five samples have no cluster labels. Existing
one/two-point coordinates are preserved. Three samples currently fail the
existing engine's perplexity rule and return `Unavailable`; four work without
clusters. Invalid inputs, engine errors and cancellation return typed failures
instead of fabricated coordinates or partial coverage.

The immutable result preserves input order and includes a versioned digest of
ordered identities and exact little-endian feature values. This CPU digest is
**separate from source physical/membership provenance**. It grants no publication
permission. The caller must keep the actual `SourceLayoutCapture` in custody and
revalidate its owner, membership, keys and lifetime before accepting a result.

Cancellation is checked during validation/embedding and around projection and
clustering. Existing engine calls are uninterruptible: cancellation during a
call waits for that call to return, then discards the result. No detached work or
early custody release is introduced.

## Verification

On Linux with Rust 1.93.0, the locked analysis library suite passed all 89 tests,
including seven new computation tests: real engine repeatability and ordered
identity binding, 0/1/2/3/4/multi-sample behavior, complete 256-sample computation,
malformed vectors/IDs, aggregate/row limits, and cancellation after each actual
engine stage.

```sh
cargo test --locked -p yield-analysis --lib -- --test-threads=1
bash scripts/gpui-development-checks.sh preflight
```

The compiled maximum-workload test was measured independently of compilation
using Python's `subprocess.run`, `time.monotonic` and
`resource.getrusage(resource.RUSAGE_CHILDREN)`:

- 256 distinct deterministic 183-component vectors; all points finite. Each
  identity includes a 256-byte source prefix and 407-byte path. The complete
  manifest fits 128 KiB while repeated CPU identities exceed that budget;
  this regression failed with `InputLimit` before the composition fix.
- Directly owned-buffer bound: 943,616 bytes.
- Compiled test process: 11.828 seconds wall, 11.811 seconds user CPU,
  0.012 seconds system CPU; peak RSS 10,796 KiB on this Linux run.

RSS includes the test harness and engine workspace. These observations are not
a hard peak-memory or latency guarantee, nor physical/native qualification.
Feature shape and row limits bound computation inputs; PCA/t-SNE can take
seconds and cannot be interrupted inside the existing engine. A source capture
that expires during that work must fail owner revalidation, not publish.
The initial 256-sample limit does not satisfy OPT-1730's large-map acceptance.
