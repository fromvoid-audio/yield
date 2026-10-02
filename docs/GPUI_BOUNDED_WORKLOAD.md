# Linux bounded workload evidence (OPT-1736)

The [raw capture](gpui-bounded-workload-linux.json) records 56 windows at clean
source commit `0b948a4dc6fcc5b78d938a4b455be7159cf94da4`, with fixture/lockfile
hashes, Linux host/resource limits, toolchain and exact commands. Reproduce on
a clean checkout with `python3 scripts/internal/perf/gpui_bounded_workload.py
--out /tmp/yield-workload.json`. Each subprocess has a 60-second deadline and
bounded log size; incomplete or duplicate windows fail capture.

Eight owner lifetimes each process two paused windows of 33 and 31 commands.
The first fills the 32-command waiting queue plus one active command and
checks `Full` rejection. Both windows cancel every command before execution,
consume/acknowledge results, retain exact custody evidence, and retire the
owner with a synthetic custody acknowledgement. Barriers/authorizations/root
registration are test fixtures; queue, cancellation and accounting are the
actual production implementations. No filesystem effect executes.

| Measurement | Captured result |
| --- | --- |
| Cancelled commands | 512; zero effects executed |
| Retained identities per owner | 33 then 64; no growth beyond configured receipt bound |
| Peak charged input per owner | 103,168 bytes of the 4 MiB input budget |
| Terminal result handles | Zero after consumption/drop |
| Retained charge after results | Deliberately remains charged until custody; not queue leakage |
| Retired owners | All eight clear retained rows and charged input after custody |
| Largest cancel/drain window | 1,345,450 ns (descriptive timing, no performance threshold) |
| CPU waveform cache | 32 generations; 65,536 input columns at 1,120 px; 15 warm hits per generation |
| Retained cache topology | One base slot and one preview slot, four paths each; no external path references after each iteration |
| Cache iteration time | Median 14,758,958 ns; maximum 18,802,883 ns (CPU-only, unoptimized test build) |

The cache fixture repeatedly invalidates generation/projection and verifies
warm hits reuse retained geometry. Slot/path/reference counts are logical
retention measurements, not allocator bytes or process RSS. The owner input
counter charges encoded custody evidence; it is not an RSS counter either.
Acknowledgement control messages are asynchronous, so the fixture waits
within a bounded deadline for their small transient credit to return before
sampling retained charge.

Validation also passed 22 normal file-owner tests (one manual measurement
ignored), 522 UI tests (two manual measurements ignored), 12 capture/parser
regressions, source architecture/owner guards, active app check, formatting
and action-register checks. No new production regression was observed in
this bounded fixture; the corrected measurement expectations describe the
existing evidence-retention and asynchronous acknowledgement contract.

This completes the portable fixture/measurement slice. **Actual macOS GPUI
frame/input/cache behavior remains unverified in OPT-1983**, under OPT-1953.
This is not an unattended soak, physical owner/descriptor qualification,
native display/audio evidence, or final responsiveness acceptance OPT-1491.
