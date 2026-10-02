# Recording byte verifier (OPT-2144)

`operation_journal::recording_commands::prefix` implements bounded real-byte reconstruction
from the [prefix seal decision](RECORDING_PREFIX_SEAL_CONTRACT.md). It supplies
`InitialRecordingHeader`, `RecordingSampleAccounting`, `RecordingTerminalKind`,
`RecordingPrefixInventory`, `RecordingPrefixCursor`, `RecordingPrefixVerifier`, and
`RecordingPrefixSeal`. These are observations; constructors and decoded values provide no
live terminal capability, Frozen journal state, filesystem grant or physical receipt.

The initializer folds actual hound header writes and the first flush's length rewrites into
one complete zero-frame revision. The verifier checks the supported 68-byte extensible
float32 grammar against the exact frozen proposal. Header revisions are processed before
PCM; the incremental BLAKE3 hash covers the final reconstructed header and PCM in logical
order. Every earlier PCM slot must be full, the last exact whole-frame tail must agree with
successful encoded coverage, and zero-frame output still needs all declared final patches.
No whole recording is buffered by the verifier.

Each page has one to eight chunks and an exact inventory/position cursor. Stale/substituted
cursors, gaps, reordered roles, partial frames and malformed pages fail without changing
state. The owner must supply rows from its actual immutable, unique-key Frozen inventory:
this byte verifier does not query SQLite, prove chunk-key uniqueness across separate pages,
authenticate caller observations or close input. Those checks remain OPT-2145.

Complete non-overflowing accounting, successful join and no writer failure/unconfirmed
samples are required. Checked accepted/encoded/abandoned partition and exact channel context
are preserved; Stop cannot abandon accepted samples. Cancel observations can describe a
known abandoned ring and remain unpublished. Rejected samples/overruns stay separate.
Control values are strict versioned JSON, at most 4,096 bytes; unknown/duplicate/missing
fields and conflicting seal replay fail. Decoding an otherwise valid hash is not a fresh
verification of physical or retained bytes; recovery must compare the real inventory again.

Only new producer proposals with a 68-byte header and at most 8,172 PCM slots are supported,
preserving the decision's four terminal-control slots. This library does not reserve those
rows/bytes or change existing V17 work/capacity/schema behavior. Historical proposals stay
decodable through their existing codec but unsupported by this verifier when incompatible.

Commands:

```sh
cargo test --locked -p yield-library --lib operation_journal::recording_commands::prefix
cargo test --locked -p yield-library --lib operation_journal:: -- --test-threads=1
bash scripts/internal/agent/run_agent_preflight.sh
```

The 11 focused tests use actual hound 3.5.1 buffered serialization for channels 1..8 and
zero, short, exact-slot and multi-slot recordings. The hash oracle is the complete actual
encoder byte image. Initialization tests record actual write/seek/flush behavior, including
unaligned raw PCM writes for non-power-of-two channel counts. Frozen namespace/accounting
inputs are synthetic and confer no authority. Malformed format/length/identity/order,
page bounds, stale cursors, missing coverage, failed/unknown accounting, overflow, strict
codec and conflicting replay are negative checks of the production algorithm.

OPT-2141 owns the real producer counters; OPT-2145 owns actual admitted SQLite closure,
immutable inventory and seal persistence/recovery. Production byte-channel rechunking,
retained transport/session integration, V17 genuine physical validation/publication and
coordinator retirement remain OPT-2058/2057/1339. Native input/storage qualification remains
OPT-2066/2067. This implementation does not open the existing V17 publication gate or
reclaim recording rows/charges; algorithm tests do not complete those missing implementations.
