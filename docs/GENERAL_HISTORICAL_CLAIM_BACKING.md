# General historical claim lookup backing

OPT-2423 removes the unfunded general `read_row` fallback from the original
ordinary-claim BOOL lookup. V1/V2 retain their existing projected observer. For
other families, the original validated borrowed SQLite BLOB remains borrowed
while an allocation-free scalar inventory counts the complete JSON structure,
including opaque payloads. The original strict codec and consistency classifier
then consume that same cell; no second SQL lookup or owned BLOB copy occurs.

Checked backing covers overlapping JSON graphs, map nodes, vector growth,
escaped-string scratch, complete typed argument layouts, participant/capacity
validator indices, and owned error formatting. The closed stored array families
use their actual compiled layouts, with the complete argument union included as
a conservative upper bound. This can refuse large primitive arrays before
decoding when the unchanged original pool cannot fund them. It does not change
wire limits, truncate rows, or accept a partial historical image.

The original recording prestart decoder and refusal marker remain mandatory.
Selected historical observer errors retain their original reply credit through
the caller's non-Clone error carrier; owned error storage is destroyed before
the final credit returns. This does not broaden the separate historical
consumer contract or replace namespace/control loans.

Tests compare the actual original SQLite reader with the existing Store reader
for V3–V18 fixtures, verify refusal at the general funding boundary before
decoder entry, preserve original rows, and exercise prestart and selected-error
lifetimes. A System allocator integration test observes actual Rust allocation
and destruction through the same reader on escaped/opaque payloads and invalid
envelopes. It does not measure SQLite's C allocator or process RSS, qualify a
native capacity provider, or complete the upstream adoption/settlement issues.
