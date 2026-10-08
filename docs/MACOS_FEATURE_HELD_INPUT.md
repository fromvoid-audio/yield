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
Run the exact-head library held-index/feature-artifact/readiness tests, catalog
feature-job tests, analysis tests, canonical preflight and native crate checks;
record the actual OS/toolchain/filesystem, command/results and limits. A native
workflow failing before any steps supplies no acceptance evidence.
