# Owner-mediated retained-index reveal (OPT-2036)

This decision separates a bounded inspection proposal, a real held-object
inspection, and native reveal dispatch. A proposal/retained row/root observation
supplies no filesystem or platform authority. OPT-2008 remains open for its
production owner, modal consumer and native handoff acceptance.

## Current route and exact owner mapping

| Existing interface/caller | What it supplies | Limit |
| --- | --- | --- |
| `yield-catalog/src/index_pages.rs`: `RequestIndexPage`, `CatalogIndexPage`, `IndexPageReady`; session catalog reexports | Owner-held DB read, source/lifecycle/query/index revision, bounded rows/bytes, source/query-fenced delivery | `RetainedUnknown` completeness; public rows are facts, not private reveal handles. Current pages have no distinct owner-issued entry/delivery ticket. |
| `yield-catalog/src/lib.rs`: `RegisteredSource`, `SourceWriter`, `SourcePublicationFence`, `index_queries`, `CatalogDelivery` | Retained source/database lease; admission and queued cancellation/credit boundary | Validate real custody, not caller-supplied `RootIdentity` or relative pathname. |
| `yield-ui/src/browser/context_menu.rs` -> `handoff.rs`: `reveal_context_file`, `start_file_handoff_with_paths`, `publish_file_handoff` | Supported sample selection and UI generation/context checks; asynchronous `prepare_file_paths` | Preparation checks absolute path + `is_file`; it follows pathname lookup and has no catalog/index lease or held-object proof. |
| `yield-ui/src/external_clipboard/mod.rs`: `PreparedFilePaths`, `prepare_file_paths` -> GPUI `cx.reveal_path` | Existing path-based supported-sample handoff | Not the index-only owner route. Void native dispatch does not prove selected object identity. Do not reuse it for index entries. |
| `yield-library/src/sample_sources/db/file_ops_journal/recovery_io.rs`: private `BoundDatabaseRoot`, `open_file_read_nofollow`; source lease `media_root_binding` | Descriptor-held namespace and direct-child no-follow read primitive | Current helper accepts one component. Production index inspection needs bounded nested component traversal and nonblocking/no-data special-file rejection. |
| `yield-library/src/filesystem_identity.rs`: `stable_filesystem_identity_from_open_file` | Identity of an opened object | Retain the actual handle and compare live descriptor metadata; a stored identity string alone is not a capability. |

No FileOperationOwner mutation grant is appropriate for this read-only inspection.
Keep it in the registered source/catalog ownership path; do not register new roots
from UI, rescan media, or grant write/delete/capacity authority.

## Proposal and delivery identity

`index_reveal_contract.rs` defines `IndexRevealPageIdentity`, bounded
`propose_index_reveal`, `IndexRevealProposal`, and typed `IndexRevealRejection`.
They are observations/proposals only, including their successful fixture cases.
Every source, lifecycle, root, independent index revision, query and delivery
identity must match. Cancel, malformed/escaping locator, inaccessible row or
unknown identity/size/mtime rejects the proposal. Variable source/root fields
are at most 4 KiB each, lossless relative path 1 KiB, object identity 512 bytes.
Only one entry and request identity are retained; no absolute target is exposed.
Raw bytes remain exact. Unknown facts are never replaced by zero or guessed paths.

Production pages must additionally mint opaque private entry handles containing
an owner/connection binding, source lifecycle, index revision, query token,
owner-issued page identity and exact retained row identity. A handle is created
only by the registered page producer and accepted only by that owner. Public
proposal fields cannot construct it. Issuing a later page does not silently
reinterpret an earlier row; closing/superseding the query invalidates its handles.
Root/file identities do not become live custody merely by being included in a key.

## Production inspection and native admission

1. UI submits one opaque delivered entry handle and monotonic request identity.
   Reserve bounded command/event/input bytes and a fixed outstanding inspection
   budget. Supersession, source change or modal close cancels the request; queued
   stale results release byte/event/descriptor credits. Retained consumer-held
   results must release their handles on close/drop.
2. Catalog verifies current profile, registered source lifecycle/query/handle
   issuer, retained source/root lease and independent index revision. Re-read the
   exact bounded index row; changed row/revision/connection, unknown or inaccessible
   facts reject explicitly. No directory enumeration or UI DB open is permitted.
3. Through the retained media-root descriptor, traverse a bounded normal-component
   chain without following intermediate or final symlinks. Require a regular file,
   compare live held-object identity/size/mtime with the retained row, and revalidate
   root/source/query/index fences after inspection. Reject missing, replaced,
   ambiguous, escaped or special entries; opening a FIFO must not block or read
   data. Unsupported descriptor primitives remain a typed unsupported result.
4. Return a private inspection object retaining the actual file and namespace
   custody plus cancellation/source fences. It exposes inspection observations,
   not an absolute pathname or native dispatch function. Reopening/retargeting a
   source requires a new page/handle. Native admission must revalidate custody,
   locator/object identity and active request again; a late inspection cannot
   resurrect a closed modal.
5. A separate qualified resource-identifying native adapter consumes that custody
   at final admission. GPUI's pathname-only `reveal_path` is not such an adapter.
   A native API that cannot maintain the admitted target/required containment must
   report Unsupported/Ambiguous. Native request acceptance and verified file-viewer
   selection are distinct outcomes; never report a void dispatch as verified reveal.

No rollback, journal filesystem checkpoint, delete or physical-operation success
is inferred from inspection. Native failure preserves the list, row and typed
status. Unknown scan completeness stays unknown regardless of inspection success.

## Bounded modal consumer

The read-only modal retains at most one 128-row page and bounded path/diagnostic
presentation, one pending query/inspection and a bounded continuation stack (or
restart rather than unlimited history). Previous/next/restart do not accumulate
all rows or imply scan completeness. Empty retained table, legacy unavailable,
malformed/oversized page, stale revision and canceled query remain distinct.
Display exact Unicode; label lossless raw-path presentation and never reconstruct
an OS target from its display text. Inaccessible/identity-unknown rows are visible
with inspection/reveal unavailable. Escape/close, focus/source replacement and
query restart invalidate pending state and late results. Native handoff absence
is shown truthfully; there is no shortcut that dispatches an index path from UI.

## Concrete implementation and external acceptance

- OPT-2037: production opaque entry handles, real DB re-read and owner-held
  descriptor inspection; catalog/index modules, source DB helper and retained-root
  traversal; session type reexports. Tests cover real-profile nested/Unicode/raw
  paths, issuer/revision/lifecycle/query rejection, object/root substitution,
  symlink and FIFO/special-file rejection, descriptor/byte/queue budgets and drops.
- OPT-2038: bounded GPUI unsupported-entry list/modal and owner inspection consumer;
  browser unsupported entries/catalog/view/actions and coordinated action register.
  Tests cover navigation, close/Escape, stale/late results, source switch, visible
  unknown/errors, bounded state, selectors and owner-only command routing.
- OPT-2039, a scoped macOS child of OPT-1953, implements and qualifies the resource-identifying
  native adapter on AppKit/Finder, including object substitution, retirement,
  cancellation and selection/AX observation. A native runtime is needed for that
  acceptance, not for the preceding portable design/owner/UI implementations.

The four proposal contract tests are portable model/type evidence, not real
filesystem admission, native dispatch, physical qualification or completion of
OPT-2037/2038/2008. Do not replace the production inspection/adapter tests with
synthetic root identities or successful proposal fixtures.

## Linux production inspection (OPT-2037)

`SourceIndexPage::inspection_handles` and `CatalogIndexPage::inspection_handles`
now mint private connection/page/row and registered lifecycle/query bindings.
The corresponding display rows remain observations; editing them cannot mint or
retarget a handle. `CatalogCommand::RequestIndexInspection` takes one opaque
handle, request ID and denial-only `IndexInspectionCancellation`. Session exposes
these types and `cancel_catalog_index_query`; cancellation requires no I/O.

The registered owner re-reads the exact row at its independent revision, with
borrowed SQLite fields limited before allocation, and validates its real retained
profile/database/source/root custody. Linux inspection uses descriptor-relative
`O_PATH | O_NOFOLLOW | O_CLOEXEC` lookups and `O_DIRECTORY` for intermediate
components. Final entries must be regular files. It never opens data streams,
reads a FIFO/device, or follows symlinks. Before publication it rechecks every
parent/name edge, held identity/size/mtime, exact row/revision and custody.

Fixed bounds are 1 KiB lossless locator, 512-byte object identity, 32 components,
eight outstanding inspections globally (queued and consumer-held), at most 33
retained descriptors per inspection plus one temporary revalidation descriptor.
Shared catalog command/event/input/output credits also apply. New requests cancel
older inspections of the same source; query close/supersession, audit, replacement,
retirement and shutdown invalidate delivered handles. Cancellation denies
projection; descriptors release on queued discard or consumer drop, rather than
being closed while still retained. Results expose facts and cheap cancellation
status, never a file handle, absolute target or native dispatch function.

This implements actual Linux held-object inspection. Other platforms explicitly
return `Unsupported` until equivalent nonblocking descriptor primitives exist;
OPT-2039 owns macOS native implementation/qualification. Cheap `is_live` is not a
filesystem revalidation or native grant. OPT-2038/2008 remain open for the modal
and qualified handoff. Tests use real isolated catalog/profile/source leases for
page-to-handle-to-inspection, with separate bounded DB and descriptor-primitive
unit tests; none is physical or Finder qualification.
