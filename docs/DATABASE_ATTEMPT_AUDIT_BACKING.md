# Original outer database attempt audit backing

The optional-input audit uses the same original count, key-shape and UUID-page
queries, through the existing private borrowed SQLite statement bridge. Both
original row ceilings are checked before any key/page interpretation. Each whole
page completes scalar conversion before the first UUID is interpreted, preserving
later UTF-8/driver errors ahead of an earlier malformed UUID. No borrowed cell or
active cursor escapes the page collection.

The original input prepays working/statement/scalar/error storage and each
64-row page's geometric Vec capacity before collection. Start, historical context,
immutable inventory and outcome readers keep their existing original producer
loans while the audit consumes them. Native SQLite error text is separately
prepaid before Rust ownership.

The compatibility and funded routes share the same map mutation body. Before
cloning the physical database key or growing any map, the funded route reserves
full pinned internal BTree nodes for the physical set, operation count and both
nominal slice charges, plus the actual main-file string. Existing terminal
scratch loans are folded together on the same pool without allocating a carrier
chain or constructing new credit. Returned audit collections own these loans;
data drops before refunds. On failure, partial collections are freed first and
their existing loans join the original input's selected-error carrier.

The original ceilings, physical-overlap and conserved-charge checks remain.
Unknown, committed and no-commit outcome observations all retain unresolved
nominal charges. These structures supply no execution, capacity or settlement
authority. The original unfunded writer/readback audit follows its compatibility
route.

Real SQLite tests compare exact collection values for all outcome kinds,
partial-map overlap errors, whole-page UTF-8 precedence, native errors and
retained ownership after caller destruction. Pressure tests exercise refusal
before actual page growth and before the original physical-key clone. A corrupt
TEXT fixture snapshots raw SQLite bytes rather than converting invalid UTF-8 to
String. Full library/docs, canonical agent preflight, GNU Windows compilation and
the existing System historical allocator regression are required. That allocator
regression does not measure this new audit; compilation is not native positive
capacity qualification.

Other database/recording restart producers, transaction settlement, post-intent
provider growth and the full Legacy worker remain separate requirements of
OPT-2373. This prerequisite does not complete them.
