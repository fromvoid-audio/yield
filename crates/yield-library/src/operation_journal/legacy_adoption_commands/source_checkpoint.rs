//! Passive comparison used only with original retained source observations.
use super::*;
use crate::app_dirs::WritableProfileGuard;
use crate::durable_capacity::VolumeFacts;
use crate::sample_sources::db::LegacySourcePagerObservation;

pub(crate) struct OriginalSourceCheckpoint<'a> {
    pub(crate) main_identity: &'a str,
    pub(crate) media_identity: &'a str,
    pub(crate) directory_identity: &'a str,
    pub(crate) reservation_identity: &'a str,
    pub(crate) directory: &'a VolumeFacts,
    pub(crate) control: &'a VolumeFacts,
    pub(crate) control_bytes: u64,
}

impl LegacyAdoptionIntentArgs {
    pub(crate) fn matches_source_checkpoint(
        &self,
        source: &LegacySourcePagerObservation,
        profile: &WritableProfileGuard,
        checkpoint: OriginalSourceCheckpoint<'_>,
    ) -> bool {
        let OriginalSourceCheckpoint {
            main_identity,
            media_identity,
            directory_identity,
            reservation_identity,
            directory,
            control,
            control_bytes,
        } = checkpoint;
        let d = &self.data;
        let image = source.image();
        d.profile_owner == profile.owner_identity()
            && d.profile_root == profile.profile_root_identity()
            && d.media_root == media_identity
            && d.source_directory == directory_identity
            && d.objects[0].identity == main_identity
            && d.objects[0].device == source.volume().device
            && d.objects[0].unit == source.allocation_unit()
            && d.objects[0].observed == image.main_bytes()
            && d.revision == image.revision()
            && d.metadata_rows == image.metadata_rows() as u64
            && d.audio_rows == image.audio_rows() as u64
            && &d.image_digest == image.digest()
            && d.source_policy
                == (Policy {
                    page_size: source.page_size(),
                    page_count: source.page_count(),
                    existing_max_pages: source.existing_max_pages(),
                    selected_max_pages: source.selected_max_pages(),
                    sector: source.sector_size(),
                    observed_cache_spill: 0,
                    observed_temp_store: 2,
                })
            && d.objects[1..4].iter().all(|object| {
                object.device == directory.identity.device
                    && object.unit == directory.allocation_unit
            })
            && d.objects[4].identity == reservation_identity
            && d.objects[4].observed == control_bytes
            && control_bytes == 0
            && d.objects[4].device == control.identity.device
            && d.objects[4].unit == control.allocation_unit
    }
}
