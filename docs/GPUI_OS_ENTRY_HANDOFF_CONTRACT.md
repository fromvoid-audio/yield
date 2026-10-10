# One current-profile OS-entry handoff (OPT-1996)

This document retains the proposed **read-only file-open handoff for an already
registered source**. The current Yield 0.1 slice is deliberately smaller: GPUI's
macOS open-URLs callback is registered before profile and window startup, drops
all supplied URL strings immediately, and places one coalescing path-free signal
in a capacity-one channel. The Browser consumes it when ready and displays the
fixed status “Opening files from macOS is not supported yet. Add a folder in
Yield to browse it.” No path is parsed, retained, logged, opened, scanned,
imported, or used to modify source/profile state. The app-reopen callback only
activates the existing app/window when available; it does not create a Browser or
profile owner. External file drag-and-drop remains a separate route.

The portable UI tests exercise this path-free ingress and Browser consumer; they
do not dispatch a native macOS delegate callback. Native callback delivery,
visibility, and reopen behavior still require macOS acceptance. OPT-1753 owns
replacing this refusal with the real owner adapter. OPT-1745 still owns legacy
migration acceptance; the proposed handoff below accepts only a current-ready
profile.

## Existing and missing callers

`SourceRegistryService` owns the profile/global registry writer and catalog
client. `CanonicalSourceRoot::new` validates a scanner-supplied absolute spelling
without filesystem canonicalization; it is not root identity or a capability.
`RegistryRevision` fences registry publication; catalog revision and registered
lifecycle generation are independent. `WritableProfileGuard::owner_identity`
identifies the live profile-owner incarnation, which must not survive restart.

The existing session `submit_catalog` route accepts
`CatalogCommand::RequestWavMetadata`; `try_recv_catalog_delivery` preserves the
catalog's queued-event/delivery-credit boundary. The command carries SourceId,
registered lifecycle, retained catalog revision, request ID and source-relative
paths. `CatalogEvent::WavMetadataReady` returns those identity fields, a physical
RootIdentity, terminal requested paths and parsed metadata. A path omitted from
the metadata was attempted but may be missing, malformed, changed or non-WAV;
its omission is not a filesystem absence proof. The catalog may discard stale
requests/completions without producing an event.

GPUI native file-open and reopen callbacks are now registered for the bounded
unsupported response described above. No file-open/drop owner handoff is
established by this inventory. The profile-owner incarnation is not exposed as a
public OS-entry admission API today. OPT-1753 must attach a private
service-instance binding in the existing owner adapter, derive current-version
readiness from actual profile startup, and route completion through the existing
catalog delivery boundary. Neither field may be supplied as authority by an OS
event or renderer. The fixture's identity values are synthetic observations,
never owner capabilities.

## Event resolution and bounded admission

The native boundary captures one lossless raw path hint, at most 4096 encoded
bytes, and passes it to the existing profile/source owner. No UI scan,
canonicalize, source auto-registration, directory creation or import occurs.
The owner resolves the hint against its already registered, verified roots. An
unknown root or ambiguous nested-root match is rejected for this initial route;
never guess by longest prefix or implicitly switch profiles. That resolution is
part of OPT-1753's implementation, not proved by the fixture below.

After resolution, the owner proposes exactly one relative path with a captured
SourceId, scanner canonical root, physical RootIdentity, profile identity and
live owner incarnation, registry revision, source lifecycle and catalog revision.
The path is validated with the existing RootRelativePath validator. It remains
a read hint, not permission to reveal, mutate, import or play a file. Containment
and actual identity must still be enforced by the existing catalog owner.

Reject closed/loading/unavailable/migration-required profile startup; zero or
retired lifecycle; changed profile/owner, source ID, canonical/physical root,
registry or catalog revision. Cap encoded path at 4096 bytes, source/profile
identity at 256 bytes each and root-identity observations at 256 bytes. The
fixture fixes roots and owner identity to bounded observations; production must
also bound raw hints and diagnostics before storing them. Bound canonical-root
spelling at 4096 encoded bytes; these fixed maxima keep a single proposal below
10 KiB including scalar fields.

Only one outer handoff is pending in this initial route. Admit no new request
until it settles/cancels; return Full rather than retaining an unbounded list.
OPT-1753 must coordinate request IDs with every existing RequestWavMetadata
producer in the same client/owner: one shared allocator and waiter routing, not
an independent counter that can collide with lazy UI reads. This reservation API
is not implemented today; the fixture counter models that prerequisite only.
Use a checked monotonic request ID scoped to the live owner incarnation; overflow
is Closed, not wrap/reuse. Construct the existing RequestWavMetadata command with
one path, then call SourceRegistryService::submit_catalog. NotReady, Unavailable,
Full, Closed and invalid-request errors are explicit bounded outcomes. A rejected
submission releases the outer pending slot; it confers no effect authority.
The model fixture constructs the existing command but does not submit it.

## Completion, cancellation and revalidation

On WavMetadataReady, match the pending request ID, source/lifecycle, catalog
revision and physical root; the terminal path set must be exactly the one
requested and metadata keys must be a subset. Revalidate the entire captured
current-ready startup and captured profile/owner/registry/source fence against the current owner observation before
publishing an ephemeral completion. A successful metadata read reports
MetadataAvailable; an omitted path reports NotAvailable with no live absence
claim. WavMetadataFailed reports a bounded typed read failure only when its
request/source/lifecycle/revision match. Unrelated catalog events retain their
existing consumers; do not consume them as OS-entry failures.

Foreign source events and old request IDs cannot release a newer pending slot. A matching but stale
completion is discarded and releases that request's outer slot; a current-owner
change proactively cancels the pending handoff because the catalog can silently
discard stale reads. Timeout/cancel releases the outer slot and reports cancelled
or superseded, not success. Any already admitted catalog request retains its own
lease/delivery credits until the worker/delivery boundary releases them; the
adapter must never release those credits itself. Late delivery is discarded by
request identity. New submissions get fresh IDs. Reopening the same profile path
with a new owner incarnation cannot revive an old completion even if its numeric
registry revision resets to the same value.

## Executable evidence and remaining acceptance

`crates/yield-session/tests/current_profile_os_entry_contract.rs` is a proposed
contract fixture only. It uses actual CanonicalSourceRoot, RootRelativePath,
SourceId, RegistryRevision vocabulary and RequestWavMetadata/Ready/Failed shapes.
Tests exercise command shape, bounded admission, one pending slot, exact queued
completion, omitted/foreign paths, stale owner/source/root/lifecycle/revisions,
cancellation/retry and lossless Unix path spelling. They open no profile or
filesystem and do not verify native resolution, live owner tokens or physical
read provenance. Existing catalog metadata/lease tests supply separate current
worker regressions, not native OS callback acceptance.

OPT-1753 must implement the private owner binding, raw-hint resolution, queued
delivery integration and native file-open/Finder/reopen/AX verification. Its
owner-backed file-open behavior must replace the Yield 0.1 refusal only after
those admission and delivery boundaries are implemented and qualified.
OPT-1745 retains migration/legacy profile acceptance. Parent/native gates remain
open. This Linux decision does not create import, reveal, mutation, playback or
filesystem authority, and does not complete either original acceptance issue.
