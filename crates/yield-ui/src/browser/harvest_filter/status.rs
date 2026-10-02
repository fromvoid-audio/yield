//! Bounded, actionable Harvest read failures without owner implementation vocabulary.
use yield_session::HarvestFactsFailure;
pub(super) fn failure_label(error: HarvestFactsFailure) -> &'static str {
    match error {
        HarvestFactsFailure::InvalidInput => {
            "Harvest filter exceeds the supported source or path size"
        }
        HarvestFactsFailure::IdentityUnavailable => {
            "Harvest identity is unavailable; refresh this source"
        }
        HarvestFactsFailure::OwnerUnavailable => {
            "Harvest owner is unavailable; retry when this profile is ready"
        }
        HarvestFactsFailure::Malformed => "Harvest metadata needs review before filtering",
        HarvestFactsFailure::QueryBudget => "Harvest filter timed out; select a choice to retry",
        HarvestFactsFailure::GlobalChanged => "Harvest changed; refreshing the filter",
        HarvestFactsFailure::StaleSource => "Source changed; refreshing the Harvest filter",
        HarvestFactsFailure::Canceled => "Harvest filter was canceled",
    }
}
