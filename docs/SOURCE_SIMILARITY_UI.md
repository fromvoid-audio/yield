# Bounded source similarity consumer (OPT-2158)

The active GPUI sample-name cell has a leading anchor toggle. Its pointer click
stops propagation and preserves normal row selection. Its captured full view
binding is rechecked at activation, so a cached callback cannot retarget the
same relative path in a replacement source. Aspect and close callbacks also
require their original query generation. The sample menu offers the
same action. A second toggle on the current anchor closes the result panel.
Overall, Spectrum, Timbre, Pitch proxy and Amplitude select the existing v1
scorer; changing aspect cancels the old query and captures a new generation.

The consumer uses the [actual retained catalog dispatcher](SOURCE_SIMILARITY_DISPATCH.md)
through the session facade. It inspects at most 65 cached visible rows and submits
at most 64 explicit candidates other than the anchor. It performs no full-source
pass, filesystem inspection, SQLite lookup or DSP on the GPUI thread. The panel
states that results cover the current visible candidate set; this is not
whole-library completeness, ANN, embedding or layout readiness.

The captured projection includes source/root/root identity, lifecycle, manifest
and directory revisions, the immutable entry mapping, scan/catalog generations,
query/folder/filter/sort/reveal choices, anchor/aspect/request generation and exact
candidate paths. Installation and rendering compare current cached context,
filter enablement and the bounded visible candidate prefix. A changed projection
or cancellation discards the result. Successful results require the actual
worker's source publication fence, retained in the panel and checked with its
memory-only `is_live`; SQL-bearing `SourceSimilarityResult::is_current_for` is
never called by the consumer. Replacement, closure and source activation cancel
the old token. Source-owner retirement hides already rendered rows.

Pending, bounded admission rejection, anchor/features unavailable and typed
failure remain explicit. Missing candidates show Unavailable rather than a score.
A bounded diagnostic does not need a successful source fence and confers no
qualification. A success without its fence never installs. Terminal duplicates
cannot replace an installed result. File scores remain finite existing cosines.

## Portable evidence and native limits

`cargo test --locked -p yield-ui --lib similarity_list` exercises actual GPUI
anchor/aspect clicks, CatalogService admission and worker delivery, genuine WAV
analysis/stored payload scores, unavailable candidates, queued aspect cancellation,
context changes, 64-candidate bounds, a real missing-anchor failure, missing
success fence and owner retirement. The shared cfg(test) catalog harness comes
from OPT-2113 / PR292 and retains actual source ownership and SQLite artifacts;
it does not qualify normal native session bootstrap. Negative rendered-selector
checks use a fresh window because GPUI retains historical debug-bound entries.

Run the affected/full UI suite, locked app, action/owner/filesystem guards and
canonical `scripts/agent.sh preflight` after the latest main rebase. Record exact
heads, commands, counts and merged-tree equality in OPT-2158 before Done.

[OPT-2165](https://linear.app/boostnlvp/issue/OPT-2165) owns native normal-profile
bootstrap and physical UI qualification. The normal Linux bootstrap requirement
is the same qualified per-user application-root/volume identity already recorded
in OPT-2143. [OPT-2159](https://linear.app/boostnlvp/issue/OPT-2159) retains aspect
enablement, weights and master weighting parity, with actual all-five-score
producer OPT-2164. Parent OPT-1719 and tracking OPT-1484 remain open for their
complete acceptance; these portable tests do not close those requirements.
