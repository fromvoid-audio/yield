# Source-owned feature payload rebuild

OPT-2082 repairs one current `AnalysisFeatures / features-v1` target produced by
`yield-audited-feature-matrix-v1`. It does not complete the artifact-family
retirement/reverse-ownership work in OPT-1656/1657, source-wide analysis, embedding,
similarity layout, or platform qualification.

Both typed current-target lookup and readiness classification validate the exact
bounded immutable payload, digest and content/version-bound reference, plus its
completion marker. A marker by itself cannot make missing/corrupt bytes current.
Older generic readiness contracts keep their existing external-artifact semantics.

The retained `SourceWriter` admits repair against its current audited manifest,
source/lifecycle/revision/root/database/profile/physical-source lease fences. One
immediate transaction checks the authoritative file identity/hash/path and exact
current target, removes only a demonstrably corrupt current-key row and its exact
completion marker, and requeues the same managed work slot. Transaction-local
revision/audit validation before commit catches in-transaction changes. No raw
writer, global registry, artifact-directory scan or schema change is introduced.

Healthy payloads are retained. If their marker disappeared, an exact new job can
recompute, confirm the same immutable bytes, and atomically restore completion.
Other file/source/version/content payloads remain untouched. A live unexpired
claim defers repair; failed work retains its existing retry/permanent policy.
Unknown references, malformed claim controls, exhausted claim generations and
incompatible repair job provenance retain evidence and report an ownership block.
Ordinary newly changed content follows the existing queue replacement contract.

The production catalog request invokes this admission before claiming work. The
existing held WAV input and feature engine recompute on the worker and publish
through the same retained source owner. Bounded fault injection is available only
in fixture builds, through that owner, for real catalog restart/rebuild tests; it
accepts no arbitrary SQL or writer capability.

Linux tests exercise real WAV decoding/DSP, audited scanner admission, actual
owned SQLite repair, restart, marker recovery, changed content and preservation.
Finite vector/SQL fault fixtures separately test ownership/rollback contracts.
Neither test category qualifies macOS/Windows held-input adapters or physical
storage behavior. OPT-2061/2062 and OPT-2075 retain those native requirements;
OPT-2026 retains the existing protected-reserve native catalog failures.
