# Original Windows source-feature input (OPT-2406)

`SourceWriter::prepare_feature_input` retains the original source media root and
claim owner, with the existing four-input permit acquired before inspection.
The Windows `HeldIndexFile` implementation opens at most 32 normal relative
components from their retained original parents. Each child is first opened for
attributes only through the pinned cap-primitives root-relative `NtCreateFile`
backend. The original parent and resulting disk object must have known native
identity and the exact directory/regular shape, without device or reparse
attributes. Streams and special paths refuse; no ambient path supplies a read.

The chain retains at most 33 file handles and 32 names. The existing relative
locator limit is 1024 encoded bytes; each native name is independently bounded.
Both vectors have fixed admission capacities, and the 33rd name refuses before
allocation. Native identity checks use fixed scalar/native records; new native
refusals carry inline error kinds. Native kernel and cap-primitives internal
allocations are separate from Rust retained chain backing. No new permit pool
or unbounded retained collection is added.

Before data access, the original source publication fence, owner, each named
parent-child edge and original file identity/size/mtime must still match.
`ReOpenFile` obtains readonly data access from the verified held regular leaf,
with exact native identity checked before and after. It never opens the leaf
locator for data. The existing bounded read then revalidates all facts and the
whole-file BLAKE3 target. The 8 MiB encoded and 2M decoded sample bounds remain
independent; the original feature owner publishes its immutable computed vector
and readiness atomically. Changed inputs or ownership cannot mark ready.

Existing actual SourceWriter/SourceLease feature-job tests compile on Windows,
including vector equivalence, atomic readiness, byte changes, source root
replacement, owner restart, cancellation, expiry/reclaim and permit refund.
Additional actual owner tests cover successful nested input and substituted
nested parent refusal. Component tests separately establish readonly exact-handle
reopening, regular/directory/device refusal and original bytes after substitution.
Windows symlink tests require genuine creation privilege and fail if unavailable;
they never silently skip. GNU compilation executes none of these Windows APIs.

OPT-2061/2062/1484/1708 remain open for their full native and workflow acceptance,
with macOS-first workflow qualification preserved. Genuine Windows runtime,
filesystem, MSVC/SDK and original Source startup resources are required for the
separate native verification leaf. Missing native bootstrap or qualification
implementation remains an implementation blocker, not a waived test. This
readonly input issues no reveal, namespace-effect, publication grant, retry,
reservation refund or settlement authority.
