# One source-owned WAV feature backend (OPT-1922)

The callable backend now performs real feature computation and atomic source
payload/readiness completion. It is not the GPUI scheduler or a physical
external-artifact publication adapter. OPT-1708 and the tracking parents remain
open for actual scheduling/presentation and complete platform acceptance.

## Owner and compute phases

1. `yield_catalog::prepare_feature_job` claims one current `features-v1` file
   target through the retained SourceWriter. Preparation reads the exact
   committed manifest identity, size, mtime and content hash, then captures the
   no-follow held root/component chain. No source bytes or DSP are read while
   a source SQL transaction is held. Input admission failure persists a bounded
   retry reason through the same source owner.
2. `PreparedFeatureJob::compute` can move to a compute thread, with no borrowed
   writer/database connection. The reader revalidates owner/root/every held edge,
   opens only the matching regular file, reads bounded bytes and checks their
   whole-file BLAKE3 digest against the scanner's exact target. The WAV buffer
   decoder uses existing v1 downmix/resampling/preparation/DSP, at the existing
   16 kHz analysis rate and eight-second decode window. It opens no pathname.
3. `ComputedFeatureJob::finish` revalidates current writer instance, source
   revision and held input before calling OPT-2052's atomic payload/completion
   command. Changed content/owner/lease/target cannot publish. Expired claims
   produce RejectedStale, including expired cancellation/failure attempts;
   they are never reported as successfully ready or canceled.

There is one retained writable source connection. Source root/profile/lease
failure cannot authorize a diagnostic write through a newly acquired owner;
the old durable running claim remains recoverable by lease expiry/reclaim.
Dropped prepared/computed work releases its held-input permit and leaves the
durable claim for recovery. No GUI process-local string invents an artifact
reference or substitutes a fixture receipt for published bytes.

## Bounds and failures

Four held source inputs/results maximum across the process, 8 MiB encoded input,
2,000,000 decoded interleaved samples, at most eight channels and 192 kHz input.
The v1 output is 183 finite f32 values, 732 immutable bytes. Too-large or missing
input, decoding/computation failure, changes and admission backpressure preserve
typed bounded retry diagnostics; retries use delays 1..60 seconds and at most
three attempts. Cancellation returns the current claim to pending. A stale claim
does not alter a newer claim or erase its failure evidence.

Unsupported containers are not silently opened by the pathname decoder. This
scope is one WAV feature job; other formats, broader scheduling, reverse
ownership and cache corruption/eviction consumers remain explicit work under
OPT-1708/1656/1657. An unavailable payload lookup means rebuild-needed availability,
not proof of a usable ready artifact. The new source-owned payload has its own
typed lookup; legacy ANN/cache table strings are not automatically adopted.

## Platform implementation gates

Linux holds an O_PATH component chain and opens a matching no-follow regular
descriptor for data read, validating again before and after content capture and
before publication. Symlink replacement and parent/root relocation fail closed.
The descriptor is never converted to an ambient or proc pathname.

The existing held inspection primitive is Linux-only. **OPT-2061** requires the
actual macOS readable-object adapter and native verification; **OPT-2062** requires
the distinct Windows HANDLE adapter and verification. Both currently return
UnsupportedPlatform and preserve retry evidence. They are implementation gates,
not test waivers. Linux source/SQLite/DSP execution does not qualify those native
primitives or complete native GPUI/physical artifact acceptance. OPT-2055 covers
the source-owned SQLite owner regression separately.

## Verification

Actual retained profile/source DB tests execute prepare -> background compute ->
owner completion against a real WAV. Its stored vector equals the existing
pathname decoder's output byte-for-byte. Negative tests cover changed bytes,
symlink substitution, root/owner replacement, restart/reclaim, cancellation,
expiry and bounded/refunded input permits. The manifest is a real temporary-file
fixture, not a production scanner or macOS event qualification.

```sh
cargo test --locked -p yield-catalog --lib feature_job -- --test-threads=1
cargo test --locked -p yield-analysis --lib
cargo test --locked -p yield-library --lib feature_artifact -- --test-threads=1
cargo test --locked -p yield-library --lib readiness -- --test-threads=1
cargo test --locked -p yield-library --lib held_index -- --test-threads=1
bash scripts/gpui-development-checks.sh check
```

Run the source-open, filesystem-producer, owner-boundary and action-register
guards. External test fixtures carry explicit cfg(test) modules; their file
placement is not a blanket production exemption. Exact counts, tested/merged
SHAs and any failed command/correction belong on OPT-1922 and its PR.
