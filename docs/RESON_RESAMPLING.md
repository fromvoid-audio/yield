# Reson audition resampling contract

Reson owns rate conversion from source frames to the opened device rate.
Matching rates preserve sample values exactly through this stage. The existing
channel routing, gain, fade, and mixing rules still apply. This contract
covers audition playback. Export resampling has its own requirements.

## Linear baseline

The baseline uses `OutputAdapter` from
`454c3497ba8176522e5a1c72433ff5cf09ffe66e`. The fixture feeds anti-phase
stereo sine waves, skips the first 100 ms, and measures a coherent 500 ms
window. Fundamental gain and residual RMS are relative to the input tone.
Downsampling uses a second tone halfway between the two Nyquist limits,
rounded down to a 10 Hz multiple. Its output RMS measures residual alias
energy. The baseline numbers below were recorded on an Apple M5 Pro,
macOS 26.6.2 (25G83), using optimized Rust 1.93.0.

| Source → device (kHz) | Pass tone (Hz) | Gain (dB) | Residual (dB) | Downsample alias (dB) |
| --- | ---: | ---: | ---: | ---: |
| 44.1 → 48 | 18,740 | −5.507 | −10.539 | |
| 48 → 44.1 | 18,740 | −4.597 | −12.051 | −4.735 |
| 44.1 → 96 | 18,740 | −5.507 | −10.539 | |
| 96 → 44.1 | 18,740 | −1.103 | −24.635 | −3.501 |
| 48 → 96 | 20,400 | −4.198 | −8.330 | |
| 96 → 48 | 20,400 | 0.000 | −136.898 | 0.000 |

Run the baseline from the repository root. The probe uses the recorded source
even after the application adapter changes.

```sh
scratch=$(mktemp -d)
git show 454c3497ba8176522e5a1c72433ff5cf09ffe66e:crates/reson/src/source/output_adapter.rs > "$scratch/output_adapter.rs"
cp scripts/qa/reson_resample_baseline.rs "$scratch/driver.rs"
rustc +1.93.0 --edition=2024 -C opt-level=3 -C target-cpu=native "$scratch/driver.rs" -o "$scratch/probe"
"$scratch/probe"
```

## Replacement targets

- For the six directions among 44.1, 48, and 96 kHz, keep the fundamental
  within ±0.05 dB through 85% of the lower Nyquist frequency. Keep
  non-fundamental residual below −75 dB over that passband. Downsampling
  suppresses tones at and above the device Nyquist frequency to −80 dB or
  lower, measured against the input tone. Test both channel independence and
  anti-phase stereo. Use deterministic tones and exclude endpoint transients
  from spectral measurements.
- Advance source position with an exact rational phase accumulator. A finite
  input of `N` complete frames produces
  `ceil(N * device_rate / source_rate)` output frames, including zero frames
  for empty input. Start at source frame zero, preserve fractional phase across
  normal loop wraps, and finish without appending a filter tail. Reset history
  and phase for a new source or explicit seek. Check very short spans and
  partial terminal source frames.
- Account separately for source frames read ahead and audible frames rendered.
  Progress, mode changes, and live retargets must use the audible coordinate.
  Keep one-shot endpoint padding out of later sources. Check loop wraps,
  rapid replacement, and changes near a loop boundary in integration.
- Prepare coefficients and bounded scratch state off the device callback.
  Match-rate routing bypasses the filter. Common-ratio future support is
  limited to 2 ms of source audio. Target optimized stereo conversion p95
  below 30 ms per second of rendered audio on the reference Mac, measured
  separately from device I/O and decoder work. Target cold filter preparation
  below 3 ms p95 and keep the existing initial decoder wait budget. Validate
  bounded construction and callback work at other admitted rates, with
  common-ratio spectral claims limited to the measured matrix.
- Require zero allocations or deallocations while rendering and retire
  filter storage off the device callback. Rerun the complete callback and
  playback control boundaries before enabling the converter in the app.
  Signed macOS listening and device-buffer checks qualify the resulting
  runtime separately.

## Feasibility measurement

An isolated reference used a ratio-scaled, DC-normalized Kaiser-windowed sinc
with exact rational phase rows. For ratio `r = max(1, source_rate / device_rate)`,
use radius `R = ceil(80 * r)`, normalized cutoff `0.925 / r`, and Kaiser
parameter 9. For phase fraction `p`, evaluate
`sinc(cutoff * (n - p))` for `n = 1 - R ..= R`, where
`sinc(x) = sin(πx) / (πx)`. Multiply by
`(I0(9 * sqrt(max(0, 1 - ((n - p) / R)^2))) - 1) / (I0(9) - 1)`
and divide each row by its sum. The probe built the coefficients as
double-precision values, stored them as `f32`, and used one dot product per
channel. Use a bounded phase table with coefficient interpolation for rates
whose reduced denominator is too large, while keeping the timebase rational.
The filter approach follows the Kaiser and polyphase techniques described in
the [Android NDK sampling guide](https://developer.android.com/ndk/guides/audio/sampling-audio).

The probe covered 16 equally spaced passband frequencies and 16 downsampling
stopband frequencies, rounded to coherent 20 Hz bins. It measured 50 ms per
tone starting 100 ms into the stream. Across the six common directions, the
largest fundamental deviation was 0.0002 dB, the largest passband residual
was −115.56 dB, and the largest downsample stopband output was −92.86 dB.
Common-ratio kernels required 160–350 taps and 1–320 phase rows. The largest
recorded single preparation was 0.921 ms. The worst of ten optimized runs per
ratio for the isolated stereo dot-product workload was 7.193 ms per second
of output. The production streaming adapter must verify these targets with
its actual history, routing, and callback integration.
