# Actual global recovery case evidence (OPT-2179)

Run from a clean Linux checkout with the repository toolchain:

```sh
python3 scripts/qa/test_global_recovery_evidence.py
python3 scripts/qa/global_recovery_evidence.py --output /tmp/global-recovery-report.json
```

Use a fresh output path outside the checkout. The collector builds the exact
normal-build `global_registry_participant_process` integration target with
`--locked --no-default-features --no-run --message-format=json`. It accepts
exactly one Cargo test artifact for the fixture source with no enabled package
features. It hashes that executable before and after the exact named test run.
Clean HEAD/tree and bounded configuration digests must remain unchanged across
build and execution; compiler/build semantic environment overrides reject.
A successful exit without exactly one valid fixture marker is insufficient.

The fixture emits its marker only after actual SQLite receipt persistence,
owned child death and profile reacquisition, competing profile denial, stale
acquisition rejection, refenced immutable replay, payload/work conflicts,
later registry edit preservation, one retained receipt and writer shutdown
have succeeded. Internal fixture receipt observations remain private; the new
public marker contains only fixed case/provenance/outcome labels. The report
includes source/binary/configuration, framed stdout/stderr log digests, commands,
exit statuses and Linux kernel/architecture context. Paths, raw logs, owner IDs,
operation IDs, payloads, receipts, tokens and configuration contents are omitted.

The collector reuses `run_source_owner_workload.py`'s existing bounded POSIX
runner/configuration checks. Build is limited to 600 seconds/4 MiB combined
output; execution to 60 seconds/256 KiB. Failure/timeout/output overflow kills
and reaps only the newly created process group, including its fixture child.
The parent fixture also owns bounded child cleanup. Binary hashing streams
64 KiB blocks with a 256 MiB ceiling; case records are at most 2 KiB and final
reports at most 16 KiB. Input decoding rejects duplicate/unknown fields,
unsupported schemas, nonfinite JSON and malformed/trailing/ambiguous records.
Failed collection writes no success report; a complete validated staged file
is published by an exclusive same-directory hard link, without replacement.
Only the collector's own staging file is removed after success or failure.
Raw subprocess logs live only in bounded memory during collection.

`validate_report` checks a stored report's bounded schema and internal binding;
it does not authenticate external files or prove a rerun occurred. Only the
collector's current-source/build/execution checks produce a new observation.
Collector unit tests use explicitly simulated records for intake negatives;
they do not supply process recovery or physical qualification evidence.

This is one observed portable global **metadata** case. It supplies no live
owner/capacity grant, physical publication guarantee, journal/source/Harvest
recovery, power-loss evidence, native platform qualification or durability SLO.
These requirements are explicitly unexercised in every report. macOS/Windows
fixture verification remains OPT-2028/OPT-2029. OPT-1671 and its matrix/aggregate
parents OPT-1667/1668/1669/1345/1346 remain open until their full scopes are met.
