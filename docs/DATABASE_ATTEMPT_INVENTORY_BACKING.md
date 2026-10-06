# Original attempt inventory byte construction (OPT-2414)

The original start decoder validates a complete JSON image, compares its canonical
encoding, then checks the binary inventory's existing 64-KiB limit. Its byte vector
previously grew for the entire untrusted sequence before that final check.

The private byte visitor validates every original u8 scalar while retaining at most
65,537 bytes. The extra byte preserves an invalid oversized shape, which can never
pass the original inventory check or return a decoded start. Valid inventories keep
all original bytes. Explicit capped reserve growth keeps vector capacity at or below
65,537 bytes; old/new allocation overlap during the final reserve is at most
65,536 + 65,537 bytes. This component bound excludes allocator metadata, the raw
image, serializer output, command JSON, SQLite workspace and complete audit maps.

For an overflowing inventory, canonical comparison borrows its original array
instead of owning the discarded suffix. A second scalar-only pass counts the full
normalized decimal encoding and applies the original capped serializer error before
canonical equality. Exact prefix/suffix and canonical decimal-array spelling then
preserve the original whole-image comparison. Later malformed fields, duplicate
fields or non-u8 scalars still fail the complete original typed parse first.

No schema, image ceiling, inventory version or accepted valid inventory changes.
The tests exercise byte/capacity boundaries, complete late scalar validation,
canonical/error ordering and an actual retained SQLite hostile image through reopen.
The test counters measure inventory byte entries and vector capacity, not whole
attempt allocation, process memory, physical qualification or admission authority.

Original command-graph and complete aggregate audit backing remain prerequisites.
Recording prestart attempt/full-progress gates remain intact. Storage observations
issue no permits, retries, reservation refunds, genuine settlement or Start authority.
OPT-2168, OPT-2145, OPT-2326 and native qualification remain open.
