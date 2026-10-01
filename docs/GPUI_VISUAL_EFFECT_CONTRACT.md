# GPUI visual WGSL effect contract

This contract describes Yield's composed-window visual effects. Effects change
pixels, not audio DSP, layout, focus, or input hit testing.

## Implementation and acceptance boundary

This integration brings the staged OPT-1941–1949 visual controls, ordered Metal
passes, source review and confirmed custom activation onto current main, preserving
the newer feedback, filter and Linux storage changes. The profile preference and
approval model was already integrated independently.

Linux can check the normal application build, preference/projection behavior,
source import bounds, WGSL policy and generated shader validation. Linux cannot
compile or execute the macOS Metal/AX paths. Native compilation, renderer tests,
owned preference restart and physical display/input acceptance remain separate
Linear verification work in [OPT-1974](https://linear.app/boostnlvp/issue/OPT-1974),
under OPT-1953, and OPT-1931. Native full CI also retains
the watcher registration blocker OPT-1939/1940. A source merge does not establish
macOS release acceptance or Linux desktop support.

## Pass and texture semantics

Yield injects `post_vertex`, drawing a fullscreen triangle. Source supplies one
`@fragment fn post_fragment`; a typical input is `@location(0) uv: vec2<f32>`
and output is `@location(0) vec4<f32>`. UV is normalized: (0,0) is the upper-left
and (1,1) is the lower-right. Shaders sample the composed window image, including
text and controls. Keep output premultiplied by alpha; preserve alpha when an
effect should retain the input's opacity.

The input and intermediate textures are `BGRA8Unorm`, with four bytes per pixel.
Dimensions follow the Metal drawable in physical device pixels, not logical
layout units. The sampler uses linear minification/magnification and clamp-to-edge
addressing. No additional color-space, HDR or output color-calibration guarantee
is introduced by this pass. Hit testing continues against the unchanged UI tree.

Ordered renderer chains have at most four passes, in caller order:

| WGSL resource | Pass input |
| --- | --- |
| Group 0, binding 0: `texture_2d<f32>` | Composed UI for pass 0; previous pass output thereafter |
| Group 0, binding 1: `sampler` | Linear, clamp-to-edge sampler |
| Group 0, binding 2: optional `texture_2d<f32>` | Original composed UI, unchanged through the chain |

Binding 2 is a renderer capability for trusted chains. It is **not part of the
approved custom-source ABI v1**. Intermediate input and output never alias.
Passes before the last write separate intermediate targets; the last writes the
window drawable. The renderer retains the original UI texture for the chain.
Targets are reused at the same dimensions and recreated after size changes.
Disable and allocation failure clear the effect targets; allocation failure
renders the ordinary compositor output and logs the renderer failure.

## Shipped preset selection

The shipped presets are Grayscale, Invert, and **Gray → Invert**. The
compound preset applies exactly two ordered passes, grayscale then inversion.
The saved strength applies independently to both passes, labeled **per pass** in
visible controls, accessibility and status. At 0%, both passes preserve their
input; at 100%, the result is inverted grayscale. Alpha is retained through both.
The preset uses the trusted chain capability and does not widen custom ABI v1.
Custom selection continues to suppress all shipped passes. Disable clears the
chain through the existing clear route; custom-off restores the shipped choice.

The new serialized preset is `grayscale_then_invert`. Legacy `grayscale`/`invert`
identifiers, default-disabled state and strength remain unchanged. Older binaries
reject the newly configured enum value under existing strict deserialization;
no silent downgrade conversion is performed.

## Custom-source approval ABI v1

The custom runtime selects exactly one pass over the composed UI. It
replaces shipped effects while selected; it does not prepend or append them.
The source-policy validator accepts only the fragment entry `post_fragment`,
with optional group 0 binding 0 `texture_2d<f32>` and binding 1 non-comparison
`sampler`. Constant fragments may omit resources. Private variables are allowed;
other handle bindings, uniform/storage/workgroup/push-constant resources and
additional entry points are rejected before native preparation.

There is no binding 2, ordered user chain, external texture, file asset, time,
resolution uniform or playback/music signal in this version. Texture dimensions
can be queried with WGSL `textureDimensions`; normalized UV comes from the
injected vertex stage. No host clock or playback value is supplied. A future
resource or input-semantics expansion must version the approval contract and
require approval again, rather than widening what an existing receipt permits.

Imported source is an immutable, nonblank UTF-8 snapshot of at most 65,536 bytes.
The reader consumes at most the limit plus one byte to detect oversize input.
Review shows the exact source and its fingerprint; the source path or filename
is not execution authority. The versioned domain-separated SHA-256 receipt
hashes exact source bytes, including whitespace. Import clears approval and
disables selection; approve checks the expected fingerprint; enable is separate.
Revoke and clear disable. The local profile is trusted state, not a signature
service. Approval provides neither a GPU sandbox nor an execution-time limit.

The aggregate serialized profile preferences remain limited to 131,072 bytes.
JSON escaping may make a source that fits the raw source limit exceed that
aggregate cap. Rejected commands preserve the owner's last good state and
revision. Older binaries follow strict unknown-field rejection for configured
new fields; no transparent downgrade compatibility is promised.

## Preparation, installation and retirement

Only a settled owner snapshot can authorize custom preparation and installation:
all preference requests must be applied and persisted. `effective()` is an
optimistic projection, and an owner Snapshot may precede persistence. Neither
is sufficient authority while a request is pending or its outcome is unknown.

`Window::post_shader_compiler` returns an owned compiler retaining the exact
Metal device and unique renderer lifetime identity. `prepare_chain` can run in
background work with its scoped autorelease pool, without borrowing a Window or
holding its renderer lock. A prepared chain owns its compiled resources.
`install_prepared_post_shader_chain` performs no parsing or native compilation;
it accepts only the originating renderer/device. A different or replaced renderer
is rejected even on the same GPU and falls back to ordinary rendering.

Custom runtime work uses one process-wide preparation permit, acquired before
queueing and retained through synchronous native preparation. A view retains
its foreground task while obsolete work drains. Dropping a task prevents later
polls but cannot preempt a native call already running; canceled queued work may
retain the permit until its future is destroyed. There is no UI join, detached
borrow or forced permit release. If preparation stalls, other custom requests
receive Busy; no firm native compilation deadline is promised.

Completion uses weak entity/window handles and rechecks request generation,
current confirmed source approval and enable state, QA revision, and prepared
renderer identity. Disable, revoke, clear, import and settings reset establish
local barriers before saving, including failed or unknown saves. Temporary
owner uncertainty pauses execution without treating a pending enable as a new
explicit disable. Obsolete completion retires resources and wakes reconciliation
once; unchanged failures do not retry every frame. Enable/retry is explicit.
Custom-off restores the saved shipped selection. Error/Busy paths leave ordinary
UI readable. QA overrides invalidate pending custom results and require explicit
custom re-enable afterwards.

## Capability and performance limits

The implementation uses pinned GPUI 0.2.2's macOS Metal backend, Naga 25.0.1,
Metal 0.29 and MSL 2.3 translation with empty Naga capabilities. No Windows,
Linux or `macos-blade` post-effect implementation is qualified here. Unsupported
platform preparation APIs return Unsupported. Dependency package versions and
sources remain pinned; native backend changes require the separate macOS checks.

For N active passes, the renderer keeps one composed-UI target plus N-1
intermediates: approximately `4 * physical_width * physical_height * N` bytes
of additional texture payload. This excludes the window drawable, driver
alignment/allocation overhead, pipeline resources and the rest of the renderer.
For example, 3840 by 2160 pixels implies about 31.64 MiB for one pass and
126.56 MiB for four passes before overhead. This is an arithmetic allocation
estimate, not a measured working-set or performance benchmark.

Each pass adds fullscreen fragment work and texture traffic. GPU execution cost
is source-dependent; source-size/pass limits do not bound loop cost or ensure a
frame-rate target. There is no measured frame-time, energy, cross-display or
idle-performance guarantee. Compilation is triggered by changed selection or
explicit retry, not per frame. Shipped effects retain their existing synchronous
compile path; custom source policy/native preparation runs off the UI thread.

## Source references

Renderer definitions:
[Window API](../vendor/gpui/src/window.rs),
[platform API](../vendor/gpui/src/platform.rs),
[Metal renderer](../vendor/gpui/src/platform/mac/metal_renderer.rs), and
[WGSL translation](../vendor/gpui/src/platform/mac/post_shader.rs).
Profile and caller definitions:
[session preferences](../crates/yield-session/src/preferences.rs),
[settings controls](../crates/yield-ui/src/browser/general_settings.rs), and
[confirmation gate](../crates/yield-ui/src/browser/preferences.rs).
[Custom source policy](../crates/yield-ui/src/browser/user_shader_runtime.rs)
contains ABI-v1 validation and the preparation permit.
Use [the validation guide](TEST.md) for check selection and
[visual acceptance](GPUI_VISUAL_ACCEPTANCE.md) with OPT-1931 for hands-on evidence.
