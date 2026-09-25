# Reference appearance and animation acceptance

OPT-1490 compares the active GPUI browser against the user's Yield reference
screenshot and the retained active Radiant implementation. It does not add the
still-unimplemented starmap, destructive workflows or release packaging.

## Reference contract

The default [theme](../themes/yield.json) uses embedded Ioskeley Mono and
logical-pixel geometry. Custom themes retain their own validated tokens.

| Surface or effect | Reference and GPUI evidence |
| --- | --- |
| Dark surfaces, accent, selection | [Reference palette](../reference/radiant/src/native_app/app_chrome/palette.rs) and the default theme share accent `#E95843`, quiet selection alpha 8, selected hover alpha 16, and pale marker `#E7E5DF`. |
| Pointer hover | The reference trailing marker and [GPUI motion](../crates/yield-ui/src/browser/motion.rs) use a 2 logical-pixel pale rail at alpha 180. Hover is event-driven. |
| Keyboard and pointer focus | The leading focus rail is 6 logical pixels. [Motion tokens](../crates/yield-design/src/motion.rs) retain an 800 ms keyboard hold and 250 ms fade; pointer focus has no hold. |
| Worker feedback | [Reference status chrome](../reference/radiant/src/native_app/app_chrome/status_bar.rs) and GPUI use a 1.15 Hz pulse with alpha 80–255. |
| Source loading | The same sources use a 0.45 Hz sweep, 1.5 logical-pixel track, 28% segment with a 28-pixel minimum, track alpha 42 and segment alpha 235. |
| Overflow and reduced motion | GPUI overflow transitions settle after 260 ms. Disabling motion produces static busy feedback and suppresses decorative frame requests; completed/cancelled work no longer authorizes an ongoing cadence. |
| Waveform | [Waveform rendering](../crates/yield-ui/src/browser/waveform.rs) retains low/mid/high spectral colors, title/metadata, selection and playhead layers. Measured logical width drives bounded detail requests. |

The GPUI folder/table overflow ramp remains a surface-specific affordance; it is
not equated with the reference collection section's separate 12-pixel ramp.

## Native fixture

Validation uses a separate `Yield Visual 1490.app`, isolated profile and a
generated four-second mono WAV on the development Mac (Apple M5 Pro). No live
library or DAW session is part of this check. Local screenshots and observations
are retained under the Codex task's `artifacts/opt1490-native` directory.

Baseline scenes exposed two minimum-window defects: the sidebar's lower controls
fell below the viewport, and a fixed-width search field pushed Settings/Help off
the toolbar. Expanded-window inspection confirmed the folder tree already grows
with available height. Compact filter/action rows restore space without removing
integrated controls.

An undisturbed 20-second baseline after loading recorded resident memory stable at
115680 KiB and 0.49 seconds of process CPU time (about 2.45% of one core). This is
a bounded debug-build observation, not the full performance qualification in
OPT-1491.

## Final verification

The design suite passed all 21 tests, including canonical-theme equivalence,
palette values, bounded/periodic pulse and sweep, focus hold/fade endpoints,
rapid retargeting and disabled-motion stability.

The UI suite passed 394 tests with one existing ignored test. The new
`compact_sidebar_keeps_folder_space_and_scrolls_auxiliary_controls` regression
uses GPUI's 2x test window at 960x560: it verifies the source header, four-row
folder minimum, actual scroll movement and both action buttons fully inside the
auxiliary viewport after scrolling. Existing motion tests cover terminal work,
retained tasks without active intent, reduced motion and settled scroll fades.
The GPUI application build, formatting and diff checks passed.

Native acceptance used source commit `b2f67fd0647765dea6ec4f7a972e7d60580578a9`:

| Scene | Result |
| --- | --- |
| [Standard 1120x720](assets/opt1490/standard.jpg) | Toolbar, sidebar, table and spectral waveform retain the dark reference styling and readable source/folder typography. |
| [Minimum 960x560](assets/opt1490/minimum.jpg) | Settings/Help remain visible; search text stays inside its field; lower sidebar controls scroll fully into view. |
| [Expanded window](assets/opt1490/expanded.jpg) | Folder space grows while auxiliary controls retain their content height. No layout flicker or continuous visual movement was observed after settling. |
| [General / reduced motion](assets/opt1490/general-reduced-motion.jpg) | Settings remain usable at minimum size. Animations Off persists across restart; toggling On restores the normal mode. |
| [Audio](assets/opt1490/audio.jpg) | The panel and its device-status text fit at minimum size; Escape closes it. |
| [Shortcut help](assets/opt1490/shortcuts.jpg) | Long descriptions wrap, all rows remain reachable by scrolling, and Escape restores the browser. |

These are native captures; the capture service downscales large windows and may
show a macOS capture indicator over the traffic lights. HiDPI layout is separately
exercised at an explicit 2x scale in the regression above.

The final 20-second idle observation consumed 0.50 seconds of process CPU time
(about 2.5% of one core); resident memory decreased from 119184 to 99744 KiB.
Two fresh launches of the unchanged final binary, resizing, dialog interaction
and clean process exit passed. One earlier capture after replacing the preview
artifact showed a blank lower area. It did not recur after a clean restart;
read-only debugging found no reproducible source clipping defect, so no
speculative renderer change was made. This observation is retained rather than
presented as a proven fix. Broader long-session qualification remains OPT-1491.
