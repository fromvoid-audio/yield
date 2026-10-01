# Finder evidence collector

`finder_evidence.py` assembles a bounded report from the existing
`yield.finder-continuity-evidence.v1` model records and the redacted `finder_v1.tsv`
fixture at a declared exact Git head. It does not launch Yield or alter sources.

On macOS, run the exact existing disposable model test listed by the report's
`capture_command`, with `--nocapture`, and retain its output in an ignored QA
directory. Then collect it with:

```sh
python3 scripts/qa/finder_evidence.py --head FULL_TEST_COMMIT_SHA \
  --input target/qa/finder-model.log --out target/qa/finder-report.json
```

Use a full 40-character SHA of the actual test build. The collector loads the
fixture from that Git commit; the external log/head binding remains operator
declared and requires independent verification. Unknown fields, unrestricted
identities, invalid cursor/barrier/revision order, contradictory uncertainty,
missing/duplicate cases, and oversized records fail validation. Input is capped
at 1 MiB, each record at 2 KiB, and only two existing model records are retained.
Raw log lines and fixture paths are omitted; hashes preserve input identity.

On Linux, omit `--input` to generate a report with `model_status=not_captured`.
This records the blocked host honestly and never substitutes synthetic success
for the macOS model run. Every report includes all eleven required session rows,
their issue and expected disposition/affected region, fixture cursor references,
and available aggregate model revisions. Aggregate replay proves no per-session
Finder/GPUI projection result. Empty/unsupported-only folders, overflow, restart,
and scan-overlap have explicit fixture/observation gaps rather than invented data.
No-UI-I/O requires a runtime observation and remains unverified.

The current model records explicitly state synthetic FSEvents and paused native
UI. Consequently the collector always writes `status=blocked_native_acceptance`
and exits 1, including when model validation succeeds. Exit 2 means invalid input
or failed report creation. Keep reports in ignored `target/qa/`, use fresh output
paths, and preserve OPT-1939/1940 registration blockers. Native acceptance remains
a separate unlocked-macOS Finder/GPUI session; this collector cannot clear it.

Run portable schema/redaction/failure regressions with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/qa -p test_finder_evidence.py
```
