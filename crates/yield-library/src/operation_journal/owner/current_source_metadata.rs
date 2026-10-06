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
