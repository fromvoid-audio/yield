//! Explicit bounded dynamic inventory funded before consuming Current confirmation.
//! Native allocations/extents, allocator bookkeeping and the original SQLite cache are separate.

const PATH: usize = 8192;
const IDENTITY: usize = 256;
// These inline headers are already charged in SourceVolumeBinding. Count only allocations.
const OWNER_DYNAMIC: usize = 2 * PATH
    + 3 * IDENTITY
    + std::mem::size_of::<(usize, usize, std::cell::Cell<bool>)>()
    + std::mem::size_of::<(usize, usize, std::sync::atomic::AtomicBool)>();
// OwnedReserveAllocation clones its original root and retains namespace + physical observations.
// The root's optional profile catalog lease is retained in the already charged original request.
const RESERVE_DYNAMIC: usize =
    2 * PATH + 7 * IDENTITY + crate::sample_sources::db::BoundDatabaseRoot::LEGACY_ARC_FILE_BYTES;
// Per volume: installed owner; possible failed context alias; installed/reopened reserve;
// original namespace-failure observation; registry/marker backing before an owner exists.
const VOLUME_DYNAMIC: usize = 2 * OWNER_DYNAMIC + 2 * RESERVE_DYNAMIC + PATH + 4 * IDENTITY;
// One producer executes at a time. Its canonical account home/library/support/app and derived
// registry/lock paths are all bounded before retention; passwd lookup uses a fixed 16KiB Vec.
// Marker read/write buffers are <=1025 bytes, with Vec/String growth to <=2048 bytes each.
// Reserve binding/profile/context identity copies and one bounded native diagnostic coexist.
// One terminal-check owner context alias overlaps the installed owner + inner Store alias.
const PRODUCER_SCRATCH: usize = OWNER_DYNAMIC
    + 6 * PATH
    + 16 * 1024
    + std::mem::size_of::<Vec<u8>>()
    + 2 * 2048
    + 12 * IDENTITY
    + PATH;

pub(super) const NATIVE_BACKING_BYTES: usize = 2 * VOLUME_DYNAMIC + PRODUCER_SCRATCH;

use super::*;

// Every retained Current permit includes this strictly positive mandatory debit. The fixed
// teardown carrier therefore follows the original pool capacity, not a new source-count limit.
pub(super) const MIN_RETAINED_CURRENT_BYTES: usize =
    NATIVE_BACKING_BYTES + REGISTRY_NODE_BYTES + FIXED_INPUT_HEADERS;
pub(super) const MAX_CURRENT_CREDITS: usize = ORDINARY_INPUT_BYTES / MIN_RETAINED_CURRENT_BYTES;
const _: () = assert!(MIN_RETAINED_CURRENT_BYTES > 0 && MAX_CURRENT_CREDITS > 0);
const CREDIT_TEARDOWN_SCRATCH: usize =
    std::mem::size_of::<[Option<CurrentInput>; MAX_CURRENT_CREDITS]>();

// Rust 1.93 std/hashbrown: power-of-two bucket storage, >=7/8 load, one control byte per
// bucket plus a <=16-byte control group. Eight buckets per lifetime entry covers initial
// small tables and overlapping old/new growth tables. Both live and retired tables are
// retained until actor teardown, even after all their values have moved out.
const fn hash_entry<K, V>() -> usize {
    8 * (std::mem::size_of::<(K, V)>() + 1) + 32
}
// Rust 1.93 alloc/src/collections/btree/node.rs: B=6, <=11 key/value slots,
// <=12 child pointers, parent pointer and two u16 indices. Counting these indices as
// full pointer-sized words plus a padding word bounds the aligned node prefix.
// For height, use the weaker binary-tree minimum (rather than six-way branching):
// <=8192 keys implies <=14 occupied levels; one split per level plus two root nodes.
const fn split_nodes(mut keys: usize) -> usize {
    let mut levels = 0;
    while keys != 0 {
        levels += 1;
        keys >>= 1;
    }
    levels + 2
}
const MAX_VOLUME_KEYS: usize =
    2 * crate::sample_sources::db::source_registry_handoff::MAX_SOURCE_REGISTRY_CARRIERS;
const fn tree_insert<K, V>() -> usize {
    split_nodes(MAX_VOLUME_KEYS)
        * (11 * std::mem::size_of::<(K, V)>() + (4 + 12) * std::mem::size_of::<usize>())
}
const REGISTRY_NODE_BYTES: usize = hash_entry::<crate::sample_sources::db::SourceRegistrationId, SourceAuthorityRecord>()
    + hash_entry::<crate::sample_sources::db::SourceRegistrationId, RetiredSourceAuthorityRecord>()
    + 2 * tree_insert::<VolumeIdentity, SourceVolumeBinding>()
    + 2 * tree_insert::<crate::sample_sources::db::SourceRegistrationId, ()>()
    // Temporary deduplicated volume set before installing the two entries.
    + tree_insert::<VolumeIdentity, ()>()
    // Vec initial/growing capacity is <=4 slots per lifetime credit; no permit is created.
    + 4 * std::mem::size_of::<CurrentInput>();
const FIXED_INPUT_HEADERS: usize = std::mem::size_of::<(usize, usize, CurrentInputBacking)>()
    + std::mem::size_of::<SourceAuthorityRecord>()
    + std::mem::size_of::<RetiredSourceAuthorityRecord>()
    + 2 * std::mem::size_of::<SourceVolumeBinding>()
    + 6 * std::mem::size_of::<VolumeIdentity>()
    + 512
    + std::mem::size_of::<crate::sample_sources::db::CurrentReserveUseProof<'static>>();

pub(super) fn input_bytes(
    request: &SourceAuthorityRegistrationRequest,
) -> Result<usize, OperationJournalOwnerQueueError> {
    let input_bytes = request
        .current_metadata_bytes()
        .and_then(|bytes| bytes.checked_add(MIN_RETAINED_CURRENT_BYTES))
        .and_then(|bytes| bytes.checked_add(CREDIT_TEARDOWN_SCRATCH))
        .ok_or_else(|| OperationJournalOwnerQueueError::InvalidRecord {
            reason: String::from(
                "Current source registration metadata exceeds its bounded field inventory",
            ),
        })?;
    Ok(input_bytes)
}

// No raw Arc or Weak can escape: every alias consumes Arc::into_inner. Its exact last
// winner frees the Arc allocation (including the implicit weak header) before refunding
// the moved permit. This also covers rejection before registry retention and unwind.
pub(super) struct CurrentInput(Option<Arc<CurrentInputBacking>>);
struct CurrentInputBacking {
    // Test probes observe a separate allocation, never pin the funded Arc header.
    #[cfg(all(test, unix))]
    probe: Arc<()>,
    input: InputPermit,
}
impl CurrentInput {
    pub(super) fn new(input: InputPermit) -> Self {
        Self(Some(Arc::new(CurrentInputBacking {
            #[cfg(all(test, unix))]
            probe: Arc::new(()),
            input,
        })))
    }
    #[cfg(all(test, unix))]
    pub(super) fn probe(&self) -> std::sync::Weak<()> {
        Arc::downgrade(&self.0.as_ref().expect("live Current input").probe)
    }
}
impl Clone for CurrentInput {
    fn clone(&self) -> Self {
        Self(Some(Arc::clone(
            self.0.as_ref().expect("live Current input"),
        )))
    }
}
impl std::ops::Deref for CurrentInput {
    type Target = InputPermit;
    fn deref(&self) -> &Self::Target {
        &self.0.as_ref().expect("live Current input").input
    }
}
impl Drop for CurrentInput {
    fn drop(&mut self) {
        if let Some(input) = self.0.take().and_then(Arc::into_inner) {
            drop(input);
        }
    }
}
