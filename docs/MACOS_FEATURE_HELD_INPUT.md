# Darwin held feature input (OPT-2061 work in progress)

The existing SourceWriter captures the exact current feature claim, manifest
identity/size/mtime/content hash, retained Source lease and media root. Four held
input permits, 8 MiB encoded input and 2,000,000 decoded samples retain their
existing limits. Preparation performs no data read or feature computation.

The private component adapter uses descriptor-relative `openat`, one validated
normal component at a time, bounded to 32 edges and 1024 encoded path bytes.
Darwin `O_EVTONLY` holds metadata-only vnodes; `O_NOFOLLOW`, `O_CLOEXEC` and
`O_NONBLOCK` apply to every edge, and intermediate edges require `O_DIRECTORY`.
`fstat` rejects every non-directory intermediate and every non-regular leaf,
including a FIFO/device or symlink substitution, before any data read. Every
held edge remains live through computation and publication and is revalidated
against its original parent and file identity.

The compute reader opens a separate readonly no-follow/nonblocking descriptor
under the original held parent. Its type and physical identity must match the
metadata-only leaf before it can be returned for data access. The existing
reader checks size/mtime/current owner and all edges, bounded content bytes and
whole-file BLAKE3, then revalidates before Source-owned payload/readiness commit.
Changed bytes, names, root, owner, generation, expiry or cancellation retain the
existing typed retry/stale behavior. No data pathname, proc alias, native reveal,
new Source connection or decoded claim grants permission.

The actual held-index and retained Source feature-job test modules now include
macOS. Their matrix covers exact vector equivalence, nested input/relocation,
changed content, source/root/writer replacement, reclaim/restart, cancellation,
expiry and bounded/refunded permits. Additional primitive cases reject symlink,
FIFO, device and wrong type and reject a replacement leaf before returning a
data descriptor. These are filesystem/Source tests, not GUI/audio hardware proof.

Genuine macOS execution and supported SDK/filesystem acceptance remain required
before OPT-2061 is Done. Linux execution does not compile or exercise `O_EVTONLY`.
Run `scripts/internal/validation/verify_macos_feature_held_input.sh` on the exact
head. The runner records the active developer directory, full Xcode version when
available or the installed Command Line Tools package otherwise, and the
selected macOS SDK path/version and Clang. Command Line Tools are acceptable
only when the actual Darwin held-input and retained-source tests, analysis tests,
canonical preflight, and native crate check all execute successfully; recording
an SDK alone is not qualification. The held-input and catalog selections must
execute exactly three and eleven tests respectively, with no ignored or failed
tests. Every later test selection must execute at least one test with no ignored
or failed tests. The runner checks writable APFS storage and records command
results, toolchain, filesystem and limits. A missing tool, zero-match filter or
native workflow failing before its required steps supplies no acceptance
evidence.

## Local-only qualification

The hosted workflow is retained as `.yml.disabled`. Run the native script locally
with Rust 1.93.0 under the shared local-CI coordinator; do not dispatch hosted CI.
The held-file adapter is a runtime prerequisite for the existing native catalog
and UI regression checks. Those checks do not accept the broader 0.4 analysis
and similarity product workflows, or supply audio hardware acceptance.
