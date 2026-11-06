use std::sync::atomic::{AtomicBool, Ordering};

use super::{SourceWriter, SourceWriterError, map_sql_error};
use crate::sample_sources::{
    db::transient_cache::{self, CachedTransientMarkers, DetectorConfig},
    readiness::SourceFeatureInput,
};

impl SourceWriter {
    /// Read a disposable transient payload for this exact current held input and configuration.
    /// Verify the held media bytes before returning cached markers to a consumer.
    pub fn lookup_transient_cache(
        &self,
        input: &SourceFeatureInput,
        detector_version: &str,
        preset: &str,
    ) -> Result<Option<CachedTransientMarkers>, SourceWriterError> {
        let config = DetectorConfig::new(detector_version, preset)?;
        self.validate_audio_input(input)?;
        let result = transient_cache::lookup(&self.database.connection, &input.content, config)?;
        self.validate_audio_input(input)?;
        Ok(result)
    }

    /// Atomically retain a validated frame payload for the exact current held input.
    /// The caller must have computed the markers from this input's verified bytes.
    /// A cancelled insertion leaves the prior cache intact and returns false.
    pub fn store_transient_cache(
        &self,
        input: &SourceFeatureInput,
        detector_version: &str,
        preset: &str,
        markers: &CachedTransientMarkers,
        cancel: &AtomicBool,
    ) -> Result<bool, SourceWriterError> {
        let config = DetectorConfig::new(detector_version, preset)?;
        if cancel.load(Ordering::Acquire) {
            return Ok(false);
        }
        self.database.validate_profile_owned_mutation()?;
        self.validate_audio_input(input)?;
        let transaction = self
            .database
            .connection
            .unchecked_transaction()
            .map_err(map_sql_error)?;
        transient_cache::store(&transaction, &input.content, config, markers)?;
        // The original writer serializes revision changes. While this transaction
        // owns its connection, recheck only the independent file and lease fence.
        input.validate()?;
        if cancel.load(Ordering::Acquire) {
            return Ok(false);
        }
        transaction.commit().map_err(map_sql_error)?;
        Ok(true)
    }
}
