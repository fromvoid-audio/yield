# One production feature request (OPT-1708)

The sample-row context menu now offers **Analyze features**. Its existing exact
source/lifecycle/root/revision/row projection fence captures one relative WAV
path. The browser submits a bounded catalog command through the session and
shows request submission separately from actual job state. It records truthful
Queued, Running, Ready or bounded Blocked messages in the existing status/history
surface. This is one file's AnalysisFeatures state, not whole-source readiness.

The retained catalog background worker renews its actual source authority,
validates the required audited revision, calls OPT-2071's real manifest publisher
and queues one owner-derived target. OPT-1922 supplies the exact claim/held input,
bounded existing DSP and atomic immutable source payload/readiness completion.
There is no UI filesystem read, second source writer or source transaction held
during I/O/CPU. The callable request is serial on the existing background owner,
with a 120-second lease and existing 8 MiB/two-million-sample computation bounds;
this is not a broad supervisor or parallel source processing scheduler.

Ready is emitted only after the current retained owner validates actual stored
feature bytes and reissues their typed key. Existing cached completion also needs
that lookup; a marker or process-local artifact string cannot fabricate Ready.
Unclaimable retained work or missing/corrupt payload rebuild remains Blocked;
automatic corruption/eviction repair stays under OPT-1657. Unavailable native
held input returns UnsupportedPlatform, retaining retry evidence. Native adapters
OPT-2061/2062 remain actual implementation gates, never waived by a model.

Each accepted request reserves at most three bounded events. Relative path/source
input is bounded to 1024/256 bytes and invalid paths reject before queue admission.
The existing command/event capacities provide backpressure. No vectors or source
handles escape in event output; a typed immutable key is a value, not mutation
authority. Source retirement binds the same cancellation flag used by computation.
After commit, cancellation never deletes already valid artifact evidence.

The browser retains one request, cancels a superseded/dropped request, and requires
matching source, lifecycle, audited revision, request identity and path before
projecting events. Retired physical publication fences are rejected at delivery;
current browser metadata/activation is checked too. Blocked messages use bounded
plain explanations rather than raw backend diagnostics. Original playback/menu
actions remain separately available.

Production catalog tests start from a real scanner audit and exercise actual
WAV computation/source SQL through the command/event route, malformed WAV retry,
cancelled/stale requests, admission bounds, cache reuse and retained unexpired
claim after drained restart. Headless UI tests cover late result and cancellation
fences; they are not native pointer/AX presentation qualification. Native GPUI
verification is OPT-2075; macOS Ready additionally requires OPT-2061. Existing
native protected-reserve catalog regressions remain OPT-2026.

The small-source publisher rejects >256 supported files/128 KiB manifest input;
scalable portable publication is OPT-2072. Source-wide reanalysis, similarity,
embedding/layout and rich semantic progress are still separate requirements under
OPT-1718/1719/1728/1803 and the tracking parents. No general ready state or physical
qualification is inferred from this one feature request.

```sh
cargo test --locked -p yield-catalog --lib feature_request -- --test-threads=1
cargo test --locked -p yield-catalog --lib feature_job -- --test-threads=1
cargo test --locked -p yield-ui --lib -- --test-threads=1
bash scripts/gpui-development-checks.sh check
python3 scripts/internal/check/check_gpui_action_register.py --check
```

Run the other repository ownership/filesystem/source-open guards too. Record exact
heads, commands, results and limitations on the PR/issue; keep tracking parents open.
