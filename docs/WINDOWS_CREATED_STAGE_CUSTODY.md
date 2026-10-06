# Windows created-stage handoff assessment (OPT-2415)

Assessment for [OPT-2218](https://linear.app/boostnlvp/issue/OPT-2218), preserving
[OPT-1632](https://linear.app/boostnlvp/issue/OPT-1632) acceptance: an observed
readonly staged file cannot become a trusted created-stage capability. Select
one private creator on the original FileOperationOwner worker, with continuous
original-object custody from exclusive creation through publication completion.
No qualified producer exists today. Keep production publication `Unsupported`
until the implementation and qualification requirements below are satisfied.

## Actual producer and consumer boundary

`file_operation_owner.rs` has typed `CreateStaging`, `WriteStagingChunk` and
`PublishNoReplace` requests. The general worker currently executes staging
validation and refuses mutating effects because no qualified production backend
is available. `file_operation_owner/publication_binding.rs` prepares and hashes
a retained readonly observation, then keeps it in `BindingData.staged_file`.
Its [original share lease](WINDOWS_STAGING_SHARE_LEASE.md) excludes data-write,
append and DELETE file handles; it does not prove private creation or exclude
surviving mappings. A durable validation/hash/checkpoint or prepared target
therefore cannot manufacture the missing created-stage capability.

The intended producer is the same original worker's private `CreateStaging`
backend, followed by its admitted `WriteStagingChunk` commands. The intended
consumer is that worker's private publication effect entry, after the original
[journal/source lifetime gates](PUBLICATION_LIFETIME_GATE.md). The existing
`recovery_io.rs::windows_finalization::rename_no_replace` takes exact held stage
and target-parent handles and sets `ReplaceIfExists=false`; it is a primitive
foundation, not this missing producer or qualification policy. Its DELETE right
must remain private namespace authority. Passing a readonly observation to it,
reopening a staged pathname, or adding a disconnected sealed API is rejected.

## Creation and writer ownership

Before creating anything, the worker must have the original admitted operation,
current source/profile/root registrations, exact retained capacity/metadata
backing and qualified root/volume policy. The factory must create a new bounded
leaf relative to the genuinely retained staging parent, with `CREATE_NEW`
semantics and no-follow protection. Win32 API availability does not supply a
qualified capability-relative creation primitive. Existing or substituted
entries are never adopted, overwritten, truncated, deleted or treated as owned.

The factory alone constructs a non-Clone private writer. Record the original
native identity, singleton regular/nonreparse shape, parent and volume witnesses,
operation/claim/issue generation and admitted output identity from the actual
creation result. IDs and locators are comparison data, not authority constructors.
The original open must deny WRITE and DELETE sharing from its first instant;
other compatible readonly observations may remain possible. Request only the
access needed by this creator and its later private namespace consumer.

All writes run on the original bounded worker through admitted exact chunk
commands, checking offsets, lengths, immutable chunk identity, backing and live
operation/source fences. No raw handle, `File`, writable clone, inheritable
handle, writer callback or map escapes to another caller. The factory and writer
must never create writable sections/maps. No arbitrary callback receives the
writer. Genuine completion joins outstanding writer work, observes every exact
result and performs the required qualified flush before a sealing transition;
dropped replies and timeouts are not evidence that writing stopped.

These Rust ownership rules alone do not exclude another process duplicating a
native writer. In particular, a process with `PROCESS_DUP_HANDLE` may duplicate
a handle independently of file sharing. Same-account process access, native
aliases, privileged access and surviving maps require an accepted, implemented
exclusion/protection contract. No same-account exception is introduced here.
The owner-only directory ACL snapshot is not such a process protection contract.

## Reject the two-reopen handoff

Do not use this sequence as a qualified sealing transition:

1. Reopen the original writer for reading with WRITE sharing so the writer itself
   remains compatible.
2. Close the original writer.
3. Reopen the read bridge with READ sharing only.

Windows sharing is symmetric: the final READ-only sharing request conflicts with
the still-open writer's data-write access. Closing that writer first leaves a gap
while the bridge still permits WRITE sharing. A foreign writer can open, create a
writable mapping, close its handle, and leave the mapping alive before the final
lease succeeds. Matching identity and a subsequent hash do not prove exclusion.
The existing OPT-2396 lease is valid for its stated readonly observation subset;
it supplies no continuous trusted writer-to-reader transition.

## Exact-object reduced-handle candidate

The candidate to investigate is a noninheritable, explicitly reduced-right
`DuplicateHandle` of the original creator's file object, obtained before the
unique writer closes. This is not a new pathname open. Use the same process as
source/target, `bInheritHandle=false`, an explicit desired-access mask and options
zero. Never use `DUPLICATE_SAME_ACCESS` or `DUPLICATE_CLOSE_SOURCE`; the latter
closes the source even on failure. The reduced handle must retain the required
read/attribute/synchronization rights and, only in the private publication
capability, DELETE. Exclude data-write/append, EA/attribute write, owner/DACL
mutation and handle export. DELETE here does not authorize cleanup.

The SDK documents that duplicates refer to the same object and share a file
position. Driver documentation describes cleanup after the last handle to a
file object closes, and share removal for an open instance of that file object.
This supports investigating preservation of the original open's share barrier
without a new permissive bridge. It does **not** establish a qualified workflow
or prove every filesystem's share-removal/rename behavior. Native evidence must
show that the original deny-WRITE/DELETE reservation persists after the unique
writer closes while any reduced duplicate remains. Do not assume that a further
strict `ReOpenFile` can succeed: the original open instance may still account
for its initial writable access. Read the retained reduced handle directly on
the serialized worker, preserving its shared cursor deliberately.

Acquire and validate the reduced handle while retaining the unique writer and
its backing. On duplication failure, keep original custody; do not close the
writer through a failed transfer option. Check exact before/after native identity
and shape, then close the unique writer only once genuine writer completion and
all alias/provenance prerequisites hold. Revalidate the retained object's exact
length/hash and root/volume/lifetime binding before the publication gate. A new
private created-stage binding must carry this capability through queue, effect
entry, result retention and final alias retirement. Do not retrofit observation
`BindingData` with an unproved boolean or infer authority from stored bytes.

## Refusal, funding and completion

Unknown qualification, root/ancestor security, native alias/map exclusion or
creation semantics refuse **before creation**. Reserve the actual bounded writer,
native query/rename and retained capability backing before each allocation or
effect; reuse the original pools without increasing quotas. An exact implementation
must derive simultaneous Rust backing and retain it through every alias and error
path. Native/kernel resources and fixed stacks must be stated separately.

After a possible creation, failure is not clean `NoEffect`: retain the original
created object, original charge and exact uncertainty evidence. Confirmed queue
rejection returns original custody; accepted reply loss retains it. No automatic
unlink, overwrite, pathname recovery, retry issuance or reservation refund follows.
After a namespace attempt, late collision/interrupt/ambiguous status retain the
original and foreign objects and require the original final identity/content and
durable checkpoint evidence. Closing a handle, hashing, or receiving a namespace
success is not genuine journal settlement. OPT-2215/2216/2217/2218/2219/1632 remain
open for their full acceptance.

## Concrete implementation and native acceptance still required

Implement the real capability-relative factory/writer in the original worker,
private provenance binding and all original admission/gate/receipt consumers.
Resolve process/native alias protection, map exclusion, secure ancestors,
canonical root bootstrap and ACL lifetime as implementation/contract work. A
native test host alone cannot resolve these gaps. Add actual original-owner
tests for rejection before creation, capacity exhaustion, exact admitted writes,
genuine completion, cancellation and retained lost replies/aliases; synthetic
handles and model records cannot complete production integration.

Only after that wiring exists, use a genuine Windows/MSVC host, unelevated
current-account and controlled NTFS staging/target parents, exact private root
and process-protection fixtures, real capacity/reserve resources, and required
symlink privilege. Execute the original-owner tests plus a separate component
handoff harness. Record actual SDK/OS/filesystem, rights, share masks and token /
process access. Verify original creation identity, reduced-handle denied writes,
file-position behavior, existing/new writer and DELETE conflicts before/after
writer close and through every last alias, mapping/process-duplicate adversaries,
identity/content after final no-replace claim, collision/cross-volume/reparse/
hard-link refusal, cancellation and restart/uncertain checkpoint retention.
The component harness must test final-drop release independently; it cannot
replace original-owner lifecycle or namespace synchronization qualification.
GNU conditional compilation proves type/ABI support only. Track genuine native
execution in OPT-2219; it cannot waive the missing production factory or contracts.

## Source evidence

* [DuplicateHandle SDK](https://github.com/MicrosoftDocs/sdk-api/blob/docs/sdk-api-src/content/handleapi/nf-handleapi-duplicatehandle.md) (fetched blob `70fd544a7867ab5632be9db994383970d172a15c`): same object, explicit reduced access, inheritance, shared position, process access and failure semantics.
* [CreateFileW SDK](https://github.com/MicrosoftDocs/sdk-api/blob/docs/sdk-api-src/content/fileapi/nf-fileapi-createfilew.md) (fetched blob `7bc943d438edc966f7bd735509bafb26b3a383a3`): creation and symmetric read/write/delete sharing rules.
* [IRP_MJ_CLEANUP](https://github.com/MicrosoftDocs/windows-driver-docs/blob/staging/windows-driver-docs-pr/kernel/irp-mj-cleanup.md) (fetched blob `12ad19c8ee5c8fd77fb6711c7c68ff1e768be101`): last handle to a file object closes; outstanding I/O may still retain the object.
* [IoRemoveShareAccess](https://github.com/MicrosoftDocs/windows-driver-docs-ddi/blob/staging/wdk-ddi-src/content/wdm/nf-wdm-ioremoveshareaccess.md) (fetched blob `a80c8a1d66cebba9df06c669902c488beac37c04`): removal applies to an open instance of a file object.

The document blob IDs above identify fetched evidence. This assessment rejects
an unsafe handoff and identifies original-custody requirements; it does not
resolve the production ownership/transition or process/map protection designs.
The reduced-duplicate mechanism remains an unqualified candidate. Completing
OPT-2415 completes this bounded assessment only; the unresolved design and
implementation requirements remain on OPT-2218.
