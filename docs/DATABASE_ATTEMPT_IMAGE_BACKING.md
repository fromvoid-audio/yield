# Retained database attempt image backing

The optional-input attempt audit reads the same original start and outcome rows
through the private borrowed SQLite statement bridge. It keeps the original SQL
projections and scalar conversion order, then calls the unchanged canonical
start/outcome decoders. The compatibility writer and unfunded point reads retain
their existing behavior. These values remain observations, never live execution
permissions or settlement evidence.

Before decoding, the original input prepays statement/scalar/header working
storage and JSON syntax scratch. An allocation-free syntax inventory counts the
actual command Value graph, including full pinned BTree nodes (or the wider
IndexMap layout), geometric array/string growth and unknown-field diagnostics.
The root inventory array is a bounded byte Vec, so its elements are excluded
from Value accounting. A fixed stack buffer inspects that same borrowed byte
stream and counts the original embedded binary inventory decoder and source
fence validation. It validates every original u8 element even after overflow.
If this permissive counter fails, the typed parse or canonical shape must fail
before the subsequent binary decoder; counters never accept a start.

Canonical output backing includes the existing 4096-byte initial buffer,
geometric growth, finite float normalization and both start output buffers that
can coexist during decode. Original byte-array canonical overflow handling and
validation precedence stay in the original decoder. The outcome reader also
prepays the original start digest's repeated binary decoding and canonical
output. The retained start's unused transient syntax scratch remains available
for its embedded inventory parser while the owned start graph stays separately
charged.

Each retained value owns its original producer loans. Its data drops before the
loans refund. Selected failures transfer working, syntax, producer and native
error loans into the same original input error carrier. Folding existing scratch
loans changes no pool size or authority and allocates no carrier chain.

Real SQLite tests compare all outcome kinds, scalar conversion and canonical
errors, oversized byte arrays, malformed binary inventories, escaped field names,
float spelling, native SQL errors, caller destruction and refusal before typed
byte ownership. The full library, documentation, agent preflight, GNU Windows
compilation and existing System allocator historical regression are required.
The allocator regression covers the historical reader path; it does not measure
this new attempt reader or qualify native capacity behavior.

Outer audit UUID pages, physical/charge maps and other restart producers remain
separate work. This prerequisite does not complete the Legacy admission worker
or waive native capacity qualification.
