# Source-tag intent producer and codec

`MetadataTagSourceIntentArgs::from_observation` takes the original retained
`SourceWriter`, its opaque `SourceTagAdmissionObservation`, and stable participant
and source-command IDs. It revalidates the entire observation before and after
constructing passive data. A validated reference fence from that same writer
supplies the profile acquisition identity; its exact source revision must match.
The producer issues no OperationId, capacity claim, intent row, write grant,
assignment delta, receipt, or profile-definition finalization.

Version 1 preserves the profile acquisition, source, audited root, database/root
binding, lifecycle, lease epoch, audited revision and complete directory
generation. It records the old source-local normal tag ID/identity, either
`MetadataTagRename` with a distinct canonical normal target or
`MetadataTagDelete`, and the entire sorted unique assignment set. Rename permits
at most 64 paths; delete permits at most 128. Labels and paths permit at most
1024 UTF-8 bytes. Source/physical/profile acquisition identity strings permit at
most 256 bytes, audited root bytes at most 4096; SQL-backed counters are positive
signed-64-bit values. Empty assignment sets remain valid observations.

`decode_metadata_tag_source_intent` rejects inputs over 256 KiB before parsing.
The supported format rejects unknown/duplicate fields, invalid UUIDs and bounds,
noncanonical or classification labels, self-renames, traversal, absolute/Windows
paths, unsorted/duplicate members and overflow. The path visitor stops after 128
members. Supported input must equal its canonical encoding, including field
order, whitespace, escapes and UUID spelling. Encoding uses a bounded output
writer, so escaped strings cannot grow beyond 256 KiB. A valid JSON object with
an unknown unsigned version is retained as bounded uninterpreted original bytes;
its shape is never treated as a supported command.

`MetadataTagHistoricalScope` is explicitly historical data. There is no decoding
or conversion into `SourceRevision`, a live observation or a capability. Decoding
never consults or acquires owners. All future consumers must establish current
profile/source authority, validate complete scope again and use the actual
admission machinery. Journal family/schema registration, all-owner WAL inventory,
native capacity claims, durable intent persistence/readback and private admission
proof remain OPT-2099 scope. Assignment effects and immutable V2 receipts remain
OPT-2049. Neither parent is completed by this producer/codec.

Tests exercise actual profile-bound SourceWriters in fresh subprocesses, including
SQLite's production playback-classification expression, Unicode targets, stable
roundtrips, full 64/128 assignment sets and rejection after selection, schema or
writer-handoff changes without additional SQL edits. Pure codec regressions cover
corrupt/unsupported/oversized input, ambiguity and escaped output bounds. These
are portable source/data checks; no physical volume qualification is claimed.
