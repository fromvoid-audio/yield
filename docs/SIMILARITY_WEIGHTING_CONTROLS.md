# Bounded retained similarity weighting

OPT-2159 implements the reference aspect enablement, normalized weights and master
switch on the existing actual similarity result. All five optional descriptor
scores come from OPT-2164's single retained catalog query; controls never query
SQL, decode payloads, traverse a source, or manufacture scores.

The default master switch is off. Effective ranking then uses finite clamped raw
Overall, independently of the selected descriptor query. With weighting enabled,
only enabled available positive-weight scores contribute, normalized by their
combined weight. Empty, disabled or zero-weight contributions fall back to raw
Overall. Missing Overall remains unavailable; missing individual aspects remain
unavailable and do not contribute. Scores stay within [-1,1]. Weights clamp to
[0,1]; nonfinite weight input normalizes to the reference default 1.

Each aspect has enablement and minus/plus weight controls, in increments of 0.25.
The master and settings live in Browser memory across anchor/aspect requests;
this leaf adds no persisted settings schema. The existing selected-aspect query
and anchor-unavailable behavior remain unchanged. Changing controls reranks at
most 64 borrowed rows of the same immutable result in memory, placing unavailable
rows last and breaking score ties by deterministic relative path. Raw scores,
query generation, cancellation and live owner publication custody are retained.

Controls capture the exact request generation and recheck the existing full
source/root/lifecycle/revision/directory/view/candidate/publication fences before
changing settings. Context replacement refuses cached callbacks and suppresses
stale results before rendering. New settings are not owner authority. Existing
fixed input, candidate and aggregate result-credit budgets are unchanged.

Actual GPUI tests exercise real WAV/DSP/source payloads, retained catalog delivery,
rendered controls, immutable result reuse and context rejection. Pure arithmetic
regressions separately cover finite/clamped normalization, missing aspects,
weighted blending, disabled/zero fallback and nonfinite input. These tests do not
qualify normal native bootstrap or physical input: OPT-2165 retains that scope.
OPT-1719/1484 and whole-library, ANN/embedding and platform tracking remain open.
