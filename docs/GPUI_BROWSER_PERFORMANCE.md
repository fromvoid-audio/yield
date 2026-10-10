# Browser projection CPU workload

`scripts/perf.sh browser-vm --out <new-json-path>` runs an explicit, ignored
release-local workload against the production `BrowserViewModel`. It builds a
deterministic 20,000-entry catalog, warms query/sort/count paths, then records
30 query changes, sort changes and collection-count scans per run. Query checks
cover both 200 matches and all 20,000 entries. Every stage verifies visible-row
counts or all six collection counts while timing is captured.

The JSON records each run's source commit/tree, dirty-source status and hashes,
Cargo profile/toolchain/lockfile configuration, host model, dataset definition,
raw samples, and median/p95/maximum. Five repetitions are recommended when
establishing a baseline. Cargo runs hold the same `.git/local-ci/active.lock`
used by local CI, so benchmark compilation cannot race a managed check or
build.

On the calibrated Apple M5 Pro host, pass `--baseline
scripts/internal/perf/baselines/browser-view-model-apple-m5-pro.json` to enable
the scoped regression check. It compares candidate aggregate p95 values with
the worst per-run p95 from five pre-change release-local runs on that exact
host, dataset, runner and build configuration. The baseline commit must be an
ancestor of the candidate commit. Each limit adds the larger of 0.25 ms or three
times the median absolute run-to-run p95 spread, a small allowance for clock
and scheduler noise. A host, toolchain, configuration, dataset or runner
mismatch refuses a calibrated PASS. To recalibrate, run the same command
without `--baseline`, inspect five repetitions and their source/host identity,
then update the versioned baseline deliberately.

This is a CPU-only view-model workload. It excludes UI selection reconciliation,
GPUI rendering, frame scheduling, display submission, input delivery, GPU
completion, scanout and audio. It does not establish a native frame-time SLO or
attribute the previously observed native query-phase outlier. Broader native
frame thresholds, waveform editing, starmap and large-source qualification
remain future 0.6/0.7 work under P1875; this 0.1 lane covers only browser
projection, sorting and collection counts. Physical device disconnect/resume
remains deferred.
