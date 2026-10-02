# Source registry observations and codec (OPT-2192)

`SourceWriter::capture_registry_reference_observation` captures bounded scalar
metadata from the real retained profile/source/scan fence. It revalidates the
complete scan fence afterward. It uses the existing bounded generation inspection through a metadata-only
helper (at most two generation rows), with no directory path payload or full
integrity traversal. Completed revision/generation
is explicit `Unavailable` unless current audit/root/publication match; no pre-audit
zero is promoted to completed truth.

A supplied `SourceAuthorityRegistration` must share the original retained lease
Arc and exact owner/epoch/media root/database root/main-file identities. Phase
is sampled again after capture. Missing registration stays None. Prepared,
retired, abandoned and incompatible drain/enrollment states cannot form a
transition proposal. `RegisteredPhaseObserved` deliberately does not prove the
journal's installation/readiness or capacity: partial journal registration can
share that source phase. Actual authentication remains OPT-2193.

The capture wrapper has no Deserialize implementation. Historical subject and
phase DTOs are scalar metadata, including when decoded by a caller; none can
construct a SourceRevision, pin, lease, registration receipt or completion.
`SourceRegistryReferenceProposal::decode` is the bounded supported proposal
entrypoint. It returns canonical metadata, never a recreated owner capture.
No new SQL relation mutation, queue command or lifecycle enrollment is implemented.

Proposal v1 contains immutable operation/full GlobalLibrary work key, exact
subject, transition and expected prior row/state. Canonical JSON is at most
16 KiB including escaping; output checks before each allocation extension.
Input checks that ceiling before strict decode. Fields enforce checked SQLite
integer bounds, nonnil identities, 256-byte source IDs, 1024-byte textual
root/DB identities, 4096-byte opaque root identity and legal state directions.
Duplicate/unknown fields (including nested/tagged variants), unknown versions,
malformed/trailing input, missing registration and aggregate overflow reject.
The domain-separated BLAKE3 identity binds every semantic field; formatting
whitespace canonicalizes without changing meaning. This identity is not authority.

```sh
cargo test -p yield-library --lib source_registry_reference --locked
cargo test -p yield-library --test source_registry_reference_observation --locked
cargo test -p yield-library --lib source_writer --locked
scripts/agent.sh preflight
```

The normal-build integration cases use genuine roots/guards/SourceWriter, local
source authority preparation, manifest/audit transactions, clean reopen and
profile replacement. They do not install a journal capacity reserve. Codec
scalar fixtures explicitly create metadata only, not physical qualification.
A real retained unit case checks repeated capture leaves actual lease expiry
unchanged. Parent OPT-1828 remains open: authenticated permits, admitted relation
SQL, durable journal linkage, required source release/close, recovery and native
qualification remain OPT-2193..2198/2202 and OPT-2199/2200.
