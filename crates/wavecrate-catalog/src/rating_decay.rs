//! Bounded, renderer-neutral positive-rating decay owned by the catalog worker.

use std::collections::HashMap;

use wavecrate_library::app_dirs::WritableProfileGuard;
use wavecrate_library::sample_sources::db::BrowserMetadataCursor;
use wavecrate_library::sample_sources::reconciliation::{RootIdentity, RootRelativePath};
use wavecrate_library::sample_sources::{
    BrowserFileMetadata, Rating, SourceCurationCommitReceipt, SourceCurationUpdate,
};

use super::{
    CatalogError, CatalogEvent, EventReservation, RegisteredSource, SourceId, catalog_error_from_db,
};

/// Maximum number of browser metadata rows inspected by one decay command.
pub(crate) const PAGE_SIZE: usize = 64;

/// Default number of elapsed weeks between positive unlocked-rating decay steps.
pub(crate) const DEFAULT_RATING_DECAY_WEEKS: u16 = 4;

/// Smallest accepted number of elapsed weeks between decay steps.
pub(crate) const MIN_RATING_DECAY_WEEKS: u16 = 1;

/// Largest accepted number of elapsed weeks between decay steps.
pub(crate) const MAX_RATING_DECAY_WEEKS: u16 = 52;

const SECONDS_PER_WEEK: i64 = 7 * 24 * 60 * 60;
const MAX_PAGE_PATH_BYTES: usize = 16 * 1024 * 1024;

/// Validate the bounded rating-decay request fields.
pub(crate) fn validate_request(weeks: u16) -> Result<(), super::CatalogRatingDecayRequestError> {
    if (MIN_RATING_DECAY_WEEKS..=MAX_RATING_DECAY_WEEKS).contains(&weeks) {
        Ok(())
    } else {
        Err(super::CatalogRatingDecayRequestError::InvalidWeeks {
            min: MIN_RATING_DECAY_WEEKS,
            max: MAX_RATING_DECAY_WEEKS,
            actual: weeks,
        })
    }
}

/// Reserve the maximum bounded receipt retained by one decay progress event.
pub(crate) fn command_payload_bytes(_root_identity: &RootIdentity) -> usize {
    // The actual page path bytes are checked before commit. Reserving the complete existing
    // curation-output budget keeps queue accounting independent of source path spelling.
    super::curation::MAX_CURATION_OPERATION_RECEIPT_BYTES
}

struct PageOutcome {
    root_identity: RootIdentity,
    revision: u64,
    processed_count: usize,
    updated_count: usize,
    next_cursor: Option<BrowserMetadataCursor>,
    receipt: Option<SourceCurationCommitReceipt>,
}

/// Execute one admitted rating-decay page and publish one terminal progress/failure event.
pub(crate) fn execute(
    source_id: SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    root_identity: RootIdentity,
    request_id: u64,
    rating_decay_weeks: u16,
    now_epoch_seconds: i64,
    after_cursor: Option<BrowserMetadataCursor>,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let result = execute_inner(
        &source_id,
        lifecycle_generation,
        expected_revision,
        &root_identity,
        rating_decay_weeks,
        now_epoch_seconds,
        after_cursor.as_ref(),
        profile_guard,
        sources,
    );
    match result {
        Ok(outcome) => {
            let source = sources
                .get(&source_id)
                .filter(|source| source.lifecycle_generation == lifecycle_generation);
            let token = source.and_then(|source| source.attempt_token.as_ref());
            let _ = super::publish_source_receipt(
                &reservation.admission,
                reservation,
                &source_id,
                lifecycle_generation,
                token,
                source,
                |publication| CatalogEvent::RatingDecayProgress {
                    source_id: source_id.clone(),
                    lifecycle_generation,
                    root_identity: outcome.root_identity,
                    revision: outcome.revision,
                    request_id,
                    processed_count: outcome.processed_count,
                    updated_count: outcome.updated_count,
                    next_cursor: outcome.next_cursor,
                    receipt: outcome.receipt,
                    publication,
                },
            );
        }
        Err(error) => reservation.publish(CatalogEvent::RatingDecayFailed {
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            error,
        }),
    }
}

fn execute_inner(
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    expected_root_identity: &RootIdentity,
    rating_decay_weeks: u16,
    now_epoch_seconds: i64,
    after_cursor: Option<&BrowserMetadataCursor>,
    profile_guard: &WritableProfileGuard,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) -> Result<PageOutcome, CatalogError> {
    // Admission validates this before the command enters the queue. Keep the worker check here
    // as well because this function is the ownership boundary for every durable maintenance page.
    validate_request(rating_decay_weeks)
        .map_err(|error| CatalogError::Database(error.to_string()))?;
    let period_seconds = i64::from(rating_decay_weeks)
        .checked_mul(SECONDS_PER_WEEK)
        .ok_or_else(|| CatalogError::Database(String::from("rating-decay interval overflow")))?;

    let (processed_count, next_cursor, updates, observed_root_identity) = {
        let Some(source) = sources.get_mut(source_id) else {
            return Err(CatalogError::NotRegistered(source_id.clone()));
        };
        super::curation::ensure_generation(
            source_id,
            source.lifecycle_generation,
            lifecycle_generation,
        )?;
        validate_source_binding(
            source,
            source_id,
            lifecycle_generation,
            expected_revision,
            expected_root_identity,
            profile_guard,
        )?;

        let page = source
            .database
            .browser_metadata_page(expected_revision, after_cursor, PAGE_SIZE)
            .map_err(catalog_error_from_db)?;
        if page.revision != expected_revision {
            return Err(CatalogError::StaleRevision {
                expected: expected_revision,
                actual: page.revision,
            });
        }
        let processed_count = page.files.len();
        let next_cursor = page.next_cursor;
        let page_path_bytes = page.files.iter().fold(0usize, |total, file| {
            total.saturating_add(path_byte_len(file))
        });
        if page_path_bytes > MAX_PAGE_PATH_BYTES {
            return Err(CatalogError::Database(format!(
                "rating-decay metadata page uses {page_path_bytes} path bytes; maximum is {MAX_PAGE_PATH_BYTES}"
            )));
        }
        let updates = page
            .files
            .iter()
            .filter_map(|file| decay_update(file, now_epoch_seconds, period_seconds))
            .collect::<Result<Vec<_>, _>>()?;

        // The page query is one revision-fenced read transaction. Check the retained binding again
        // before allowing either a no-op result or a follow-up curation commit.
        validate_source_binding(
            source,
            source_id,
            lifecycle_generation,
            expected_revision,
            expected_root_identity,
            profile_guard,
        )?;
        (
            processed_count,
            next_cursor,
            updates,
            expected_root_identity.clone(),
        )
    };

    let updated_count = updates.len();
    let receipt = if updates.is_empty() {
        None
    } else {
        let receipt = super::curation::execute_inner(
            source_id,
            lifecycle_generation,
            expected_revision,
            expected_root_identity,
            &updates,
            profile_guard,
            sources,
        )?;
        if let Some(source) = sources.get_mut(source_id)
            && source.lifecycle_generation == lifecycle_generation
        {
            // Rating decay is an independent maintenance writer and cannot become the implicit
            // predecessor of a compound user-curation operation.
            source.curation_lineage = None;
        }
        Some(receipt)
    };
    let revision = receipt
        .as_ref()
        .map(SourceCurationCommitReceipt::source_revision)
        .unwrap_or(expected_revision);
    Ok(PageOutcome {
        root_identity: observed_root_identity,
        revision,
        processed_count,
        updated_count,
        next_cursor,
        receipt,
    })
}

fn validate_source_binding(
    source: &RegisteredSource,
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    expected_root_identity: &RootIdentity,
    profile_guard: &WritableProfileGuard,
) -> Result<(), CatalogError> {
    super::curation::ensure_generation(
        source_id,
        source.lifecycle_generation,
        lifecycle_generation,
    )?;
    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;

    let Some(snapshot) = source.snapshot.as_ref() else {
        return Err(CatalogError::SnapshotUnavailable);
    };
    if snapshot.source_id != *source_id {
        return Err(CatalogError::SnapshotUnavailable);
    }
    super::curation::ensure_generation(
        source_id,
        snapshot.lifecycle_generation,
        lifecycle_generation,
    )?;
    if snapshot.committed_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: snapshot.committed_revision,
        });
    }
    if snapshot.root_identity != *expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("rating-decay root identity differs from the retained snapshot"),
        });
    }
    if source
        .known_root_identity
        .as_ref()
        .is_some_and(|identity| identity != expected_root_identity)
    {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("rating-decay root identity differs from the retained source"),
        });
    }
    let root_identity = super::curation::checked_source_root_identity(source_id, &source.root)?;
    if root_identity != *expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source root identity differs from the rating-decay request"),
        });
    }
    let revision = source
        .database
        .get_revision()
        .map_err(catalog_error_from_db)?;
    if revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: revision,
        });
    }
    Ok(())
}

fn decay_update(
    file: &BrowserFileMetadata,
    now_epoch_seconds: i64,
    period_seconds: i64,
) -> Option<Result<SourceCurationUpdate, CatalogError>> {
    if file.missing || file.locked || !file.rating.is_keep() {
        return None;
    }
    let last_curated_at = file.last_curated_at?;
    let elapsed_seconds = now_epoch_seconds.checked_sub(last_curated_at)?;
    if elapsed_seconds <= 0 {
        return None;
    }
    let elapsed_periods = elapsed_seconds / period_seconds;
    let applied_periods = elapsed_periods.min(i64::from(file.rating.val()));
    if applied_periods <= 0 {
        return None;
    }
    let next_rating = Rating::new(file.rating.val() - applied_periods as i8);
    let next_last_curated_at =
        last_curated_at.saturating_add(applied_periods.saturating_mul(period_seconds));
    let path = match RootRelativePath::try_from_path(file.relative_path.clone()) {
        Ok(path) => path,
        Err(error) => {
            return Some(Err(CatalogError::Database(format!(
                "rating-decay metadata path {} is invalid: {error}",
                file.relative_path.display()
            ))));
        }
    };
    Some(Ok(SourceCurationUpdate::SetRatingAndLastCuratedAt {
        path,
        rating: next_rating,
        last_curated_at: next_last_curated_at,
    }))
}

fn path_byte_len(file: &BrowserFileMetadata) -> usize {
    file.relative_path.as_os_str().as_encoded_bytes().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecrate_library::sample_sources::SourcePlaybackType;

    fn metadata(rating: Rating, locked: bool, last_curated_at: Option<i64>) -> BrowserFileMetadata {
        BrowserFileMetadata {
            relative_path: "one.wav".into(),
            file_size: 11,
            modified_ns: 22,
            missing: false,
            rating,
            locked,
            collections: Vec::new(),
            last_played_at: Some(7),
            last_curated_at,
            playback_type: Some(SourcePlaybackType::Loop),
        }
    }

    fn update(file: &BrowserFileMetadata, now: i64, weeks: u16) -> Option<SourceCurationUpdate> {
        decay_update(file, now, i64::from(weeks) * SECONDS_PER_WEEK)
            .transpose()
            .unwrap()
    }

    #[test]
    fn decay_observes_boundary_and_multiple_periods() {
        let four_weeks = 4 * SECONDS_PER_WEEK;
        assert!(
            update(
                &metadata(Rating::KEEP_3, false, Some(100)),
                100 + four_weeks - 1,
                4
            )
            .is_none()
        );
        let one = update(
            &metadata(Rating::KEEP_3, false, Some(100)),
            100 + four_weeks,
            4,
        )
        .expect("one period");
        assert!(
            matches!(one, SourceCurationUpdate::SetRatingAndLastCuratedAt { rating, last_curated_at, .. } if rating == Rating::new(2) && last_curated_at == 100 + four_weeks)
        );
        let three = update(
            &metadata(Rating::KEEP_3, false, Some(100)),
            100 + 9 * SECONDS_PER_WEEK,
            4,
        )
        .expect("two periods capped by rating");
        assert!(matches!(
            three,
            SourceCurationUpdate::SetRatingAndLastCuratedAt {
                rating,
                last_curated_at,
                ..
            } if rating == Rating::KEEP_1 && last_curated_at == 100 + 8 * SECONDS_PER_WEEK
        ));
    }

    #[test]
    fn decay_skips_locked_nonpositive_missing_and_missing_or_future_timestamps() {
        let now = 100 + 4 * SECONDS_PER_WEEK;
        for file in [
            metadata(Rating::KEEP_3, true, Some(100)),
            metadata(Rating::NEUTRAL, false, Some(100)),
            metadata(Rating::TRASH_3, false, Some(100)),
            BrowserFileMetadata {
                missing: true,
                ..metadata(Rating::KEEP_3, false, Some(100))
            },
            metadata(Rating::KEEP_3, false, None),
            metadata(Rating::KEEP_3, false, Some(now + 1)),
        ] {
            assert!(update(&file, now, 4).is_none());
        }
    }

    #[test]
    fn decay_preserves_unrelated_metadata_in_update_input() {
        let file = metadata(Rating::KEEP_3, false, Some(100));
        let update = update(&file, 100 + 4 * SECONDS_PER_WEEK, 4).expect("eligible");
        let SourceCurationUpdate::SetRatingAndLastCuratedAt {
            path,
            rating,
            last_curated_at,
        } = update
        else {
            panic!("rating decay must use the atomic rating/timestamp update");
        };
        assert_eq!(path.as_path(), file.relative_path);
        assert_eq!(rating, Rating::new(2));
        assert_eq!(last_curated_at, 100 + 4 * SECONDS_PER_WEEK);
        assert_eq!(file.last_played_at, Some(7));
        assert_eq!(file.collections, Vec::new());
        assert_eq!(file.playback_type, Some(SourcePlaybackType::Loop));
    }

    #[test]
    fn decay_interval_is_bounded_to_one_through_fifty_two_weeks() {
        assert!(validate_request(MIN_RATING_DECAY_WEEKS).is_ok());
        assert!(validate_request(MAX_RATING_DECAY_WEEKS).is_ok());
        assert!(matches!(
            validate_request(0),
            Err(super::super::CatalogRatingDecayRequestError::InvalidWeeks {
                min: 1,
                max: 52,
                actual: 0
            })
        ));
        assert!(matches!(
            validate_request(53),
            Err(super::super::CatalogRatingDecayRequestError::InvalidWeeks {
                min: 1,
                max: 52,
                actual: 53
            })
        ));
    }
}
