//! One coherent, bounded owner-backed positional mask over the authoritative manifest.
use super::*;

/// Exact reference Harvest membership choices; selecting a choice enables the filter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HarvestFilterChoice {
    /// New and Seen files.
    New,
    /// New, Seen and Touched files.
    NewAndTouched,
    /// Undone, nonignored files without derivatives.
    NeedsReview,
    /// Touched files.
    Touched,
    /// Files with immediate derivatives, regardless of state.
    HasDerivatives,
    /// Files without immediate derivatives, regardless of state.
    NoDerivatives,
    /// Done files.
    Done,
    /// Ignored files.
    Ignored,
    #[default]
    /// No exclusion; consumers bypass owner reads.
    All,
}
impl HarvestFilterChoice {
    /// Reference dropdown order.
    pub const ALL: [Self; 9] = [
        Self::New,
        Self::NewAndTouched,
        Self::NeedsReview,
        Self::Touched,
        Self::HasDerivatives,
        Self::NoDerivatives,
        Self::Done,
        Self::Ignored,
        Self::All,
    ];
    /// Accessible reference choice label.
    pub fn label(self) -> &'static str {
        match self {
            Self::New => "New",
            Self::NewAndTouched => "New and touched",
            Self::NeedsReview => "Needs review",
            Self::Touched => "Touched",
            Self::HasDerivatives => "Has derivatives",
            Self::NoDerivatives => "No derivatives",
            Self::Done => "Done",
            Self::Ignored => "Ignored",
            Self::All => "All",
        }
    }
    /// Exact legacy membership; metadata only.
    pub fn includes(self, state: HarvestState, derivatives: usize) -> bool {
        match self {
            Self::New => matches!(state, HarvestState::New | HarvestState::Seen),
            Self::NewAndTouched => matches!(
                state,
                HarvestState::New | HarvestState::Seen | HarvestState::Touched
            ),
            Self::NeedsReview => {
                !matches!(state, HarvestState::Done | HarvestState::Ignored) && derivatives == 0
            }
            Self::Touched => state == HarvestState::Touched,
            Self::HasDerivatives => derivatives > 0,
            Self::NoDerivatives => derivatives == 0,
            Self::Done => state == HarvestState::Done,
            Self::Ignored => state == HarvestState::Ignored,
            Self::All => true,
        }
    }
}
/// Context-only request. The worker derives paths and their positions from its retained manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceHarvestFilterRequest {
    context: SourceHarvestFactsRequest,
    entry_count: usize,
    choice: HarvestFilterChoice,
}
impl SourceHarvestFilterRequest {
    /// Capture validated owner and exact projection context with a bounded mask size.
    pub fn new(
        profile: ProfileAcquisitionId,
        source_id: SourceId,
        lifecycle: u64,
        root: RootIdentity,
        directory: u64,
        revision: u64,
        request_id: u64,
        entry_count: usize,
        choice: HarvestFilterChoice,
    ) -> Result<Self, HarvestFactsFailure> {
        if entry_count.div_ceil(64) > MAX_TAG_FILTER_MASK_BYTES / 8
            || choice == HarvestFilterChoice::All
        {
            return Err(HarvestFactsFailure::InvalidInput);
        }
        Ok(Self {
            context: SourceHarvestFactsRequest::context(
                profile, source_id, lifecycle, root, directory, revision, request_id,
            )?,
            entry_count,
            choice,
        })
    }
    /// Captured source/profile projection identity; contains no selected paths.
    pub fn context(&self) -> &SourceHarvestFactsRequest {
        &self.context
    }
    /// Exact authoritative manifest length.
    pub fn entry_count(&self) -> usize {
        self.entry_count
    }
    /// Requested reference membership rule.
    pub fn choice(&self) -> HarvestFilterChoice {
        self.choice
    }
    pub(crate) fn input_bytes(&self) -> usize {
        self.context.input_bytes() + std::mem::size_of::<Self>()
    }
}
/// Owner-produced mask retaining the first real coherent global generation and actual source fence.
#[derive(Clone, Debug)]
pub struct CatalogHarvestFilter {
    request: SourceHarvestFilterRequest,
    words: Arc<[u64]>,
    anchor: Option<HarvestFactsBatch>,
    source: SourcePublicationFence,
    attempt: Arc<AtomicBool>,
    cancellation: HarvestFactsCancellation,
}
impl CatalogHarvestFilter {
    /// Captured request and exact positional binding.
    pub fn request(&self) -> &SourceHarvestFilterRequest {
        &self.request
    }
    /// Immutable compact mask in authoritative manifest order.
    pub fn words(&self) -> Arc<[u64]> {
        Arc::clone(&self.words)
    }
    /// Cheap actual source, attempt, cancellation and global generation liveness.
    pub fn is_live(&self) -> bool {
        !self.cancellation.is_canceled()
            && !self.attempt.load(Ordering::Acquire)
            && self.source.is_live()
            && self
                .anchor
                .as_ref()
                .is_none_or(|a| a.is_current_global_generation())
    }
}
mod producer;
pub(crate) use producer::dispatch;

pub(crate) mod lifetime;
