# Bounded source similarity queries (OPT-2091)

`yield_catalog::query_source_similarity` reads one explicitly supplied candidate
set through the existing retained `SourceWriter`. It reuses immutable validated
v1 feature payloads and the existing `AspectDescriptorSet::cosine_with` scorer.
It performs no artifact publication, target admission, job scheduling, SQL open,
embedding/ANN build, layout publication or whole-source traversal.

`SourceSimilarityQuery::new` requires a current owner-issued `SourceRevision`,
an exact anchor target, aspect and nonzero consumer generation. It admits at most
64 distinct candidate paths/object identities, 4096 bytes per canonical portable
path and 64 KiB of combined anchor/candidate path text. Source, file-stage,
version, content-hash and eligibility inputs are checked before payload reads.
The source owner revalidates every path/object/hash through genuine
`SourceFileObservation` values. A copied artifact key cannot substitute for that
binding. Missing, corrupt or invalid selected descriptors are unavailable; no
score or Ready state is invented. A missing anchor prevents a result.

Results carry exact query context and validated keys. Finite scores descend by
cosine, with a deterministic path tie-break; unavailable candidates follow.
They describe only the supplied candidate set. Pitch preserves the existing v1
pitch proxy semantics, rather than claiming true chroma/pitch analysis.

The future GPUI transport must call `is_current_for` on its background owner/read
worker before publishing a result. UI installation and rendering must use only
the resulting retained delivery liveness and current immutable query context;
they must never run SQL/control validation synchronously. The worker call checks source/root/database/lease/
lifecycle/revision, anchor/content/version, aspect, query generation and the
candidate set, then revalidates retained exact file observations. Retiring the
issuing writer or changing exact manifest facts invalidates stored results even
when a saved query still compares equal. This is point-in-time metadata evidence,
not filesystem effect authority, live source health or a readiness completion.
Cancellation is checked before reads, between candidates and after scoring; a
cancelled query yields no result. Existing source custody/scheduling is unchanged.

Validation uses an isolated real profile/source owner, actual SQLite claim and
feature payload publication, real WAV DSP vectors, corruption and stale injection,
and retained database/WAL byte comparisons. These portable checks do not qualify
native input adapters, signed application behavior or physical power-loss
durability. OPT-1719 retains catalog dispatch, GPUI controls/list rendering and
their native acceptance; OPT-1484 remains open. A backend result alone does not
complete those parents.

Commands:

```sh
cargo test --locked -p yield-catalog --lib similarity_query -- --test-threads=1
cargo test --locked -p yield-catalog --lib
cargo check --locked -p yield
scripts/agent.sh preflight
```
