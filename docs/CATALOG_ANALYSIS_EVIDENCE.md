# Bounded actual catalog analysis evidence

OPT-2207 collects observations from the public, normal-build CatalogService,
its scanner, finite WAV analysis worker, and retained SourceWriter SQLite owner.
The integration fixture does not invoke a test-only catalog factory or synthesize
worker outcomes. Cargo compiler records must show the catalog library compiled
without cfg(test) or catalog features. The library's existing test-support dev
dependency remains enabled; this is disclosed rather than a production packaging
or physical qualification claim.

From a clean Linux checkout with the repository Rust toolchain:

```sh
python3 -m unittest discover -s scripts/qa -p test_catalog_analysis_evidence.py
python3 scripts/qa/catalog_analysis_evidence.py --output /tmp/catalog-analysis-report.json
bash scripts/agent.sh preflight
```

The output must be a fresh path outside the checkout. The collector binds the
clean source commit/tree, lockfile and configuration hashes, compiler versions,
compiled binary digest, exact commands, successful exits, and bounded log digests.
It refuses dirty sources, semantic build overrides, source/binary changes,
ambiguous compiler artifacts, malformed evidence, and incomplete proof. It uses
the existing bounded process runner unchanged, with 600 seconds for compilation,
60 seconds for execution, and a 900-second total collection limit.

Two finite files and five requests exercise actual ready, validated already-ready,
invalid-WAV computation failure, stale revision, and explicit cancellation.
Each case records settled before/after worker metric counters and actual scalar
SQL observations, with a five-second wait ceiling. The valid WAV has 16,000 mono
PCM16 frames at 16 kHz; the second file contains invalid WAV bytes. Ready analysis
must retain the expected bounded payload and marker; the invalid file must retain
its real retryable failed job. Restart must reset worker counters while retaining
SQL observations. A 40-second fixture deadline and fixed file, request, event,
payload and sample limits bound the work.

Scalar evidence excludes source IDs, paths, profile UUIDs, request keys, errors,
and ownership objects. Raw process output stays private; only bounded byte counts
and digests are exported. Parser contract samples in the Python tests are explicitly
synthetic and never accepted as executed worker qualification.

Producer enqueue counts do not acknowledge UI projection. Timings are observations,
not hardware-independent SLO thresholds. The fixture runs without a journal client
and does not certify replay matrices, native GPUI, held-input behavior, physical
capacity, power-loss recovery, or production packaging. OPT-1677 and its related
tracking/native qualification issues remain open.

A checked-in report records the exact clean code commit used for collection.
Adding that report changes the enclosing commit/tree; reviewers can verify that
its recorded code commit and the final PR differ only in the evidence artifact.
The final PR receives the repository checks separately before merge.
