//! Retained index facts through the registered database owner, without reveal authority.
use super::*;
use yield_library::sample_sources::db::{
    SOURCE_INDEX_PAGE_MAX_BYTES, SourceIndexEntry, SourceIndexPageCursor, SourceIndexPageError,
};

/// Conservative per-delivery credit for paths, decoded/raw keys, cursor and row metadata.
/// SQLite fields are borrowed and limited to 128 KiB before allocation; this includes lookahead.
pub const MAX_INDEX_PAGE_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Retained rows cannot establish a complete or current scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedIndexCompleteness {
    /// Scan completeness and live filesystem identity are unknown.
    RetainedUnknown,
}

/// Explicit input/continuation/database failure for a retained index page.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum CatalogIndexPageError {
    /// Requested rows are outside the fixed library bound.
    #[error("invalid index page row limit")]
    InvalidLimit,
    /// Cursor or source identifier exceeds bounded input credits.
    #[error("index page input exceeds its byte limit")]
    InputTooLarge,
    /// Cursor does not belong to this source, lifecycle, query or connection.
    #[error("foreign index page cursor")]
    ForeignCursor,
    /// The independent retained index changed; restart traversal.
    #[error("index revision changed: expected {expected}, observed {actual}")]
    StaleRevision {
        /// Requested index revision.
        expected: u64,
        /// Observed index revision.
        actual: u64,
    },
    /// Stored variable fields exceed the fixed page budget.
    #[error("retained index page exceeds its byte limit")]
    ByteLimit,
    /// Malformed rows or database read failure prevent a trustworthy projection.
    #[error("retained index projection failed")]
    Database,
    /// Registered profile/source lease is unavailable or stale.
    #[error("retained index owner unavailable")]
    OwnerUnavailable,
}

/// Opaque continuation bound to the registered source, lifecycle and caller query.
#[derive(Clone, Debug)]
pub struct CatalogIndexPageCursor {
    source_id: SourceId,
    lifecycle_generation: u64,
    query_id: u64,
    cursor: SourceIndexPageCursor,
    query_token: Arc<AtomicBool>,
}

impl CatalogIndexPageCursor {
    pub(super) fn input_bytes(&self) -> usize {
        self.source_id
            .as_str()
            .len()
            .saturating_add(self.cursor.input_bytes())
    }
}

/// Bounded retained index projection, with independent index revision.
#[derive(Clone, Debug)]
pub struct CatalogIndexPage {
    /// False means legacy index facts are unavailable, not empty/current.
    pub schema_available: bool,
    /// Independent retained index revision, distinct from catalog manifest revision.
    pub index_revision: u64,
    /// Lossless source-relative paths/classifications/diagnostics; no reveal permission.
    pub entries: Vec<SourceIndexEntry>,
    /// Continue this source/lifecycle/query at this database connection and revision.
    pub next: Option<CatalogIndexPageCursor>,
}

pub(super) fn validate_request(
    source_id: &SourceId,
    cursor: Option<&CatalogIndexPageCursor>,
    limit: usize,
) -> Result<(), CatalogIndexPageError> {
    if !(1..=SOURCE_INDEX_PAGE_MAX_ENTRIES).contains(&limit) {
        return Err(CatalogIndexPageError::InvalidLimit);
    }
    if source_id.as_str().len() > 4096
        || cursor.is_some_and(|cursor| cursor.input_bytes() > SOURCE_INDEX_PAGE_MAX_BYTES + 4096)
    {
        return Err(CatalogIndexPageError::InputTooLarge);
    }
    Ok(())
}

fn page_error(error: SourceIndexPageError) -> CatalogIndexPageError {
    match error {
        SourceIndexPageError::InvalidLimit => CatalogIndexPageError::InvalidLimit,
        SourceIndexPageError::ByteLimit => CatalogIndexPageError::ByteLimit,
        SourceIndexPageError::ForeignCursor => CatalogIndexPageError::ForeignCursor,
        SourceIndexPageError::StaleCursor { expected, actual } => {
            CatalogIndexPageError::StaleRevision { expected, actual }
        }
        SourceIndexPageError::Database(_) => CatalogIndexPageError::Database,
    }
}

pub(super) fn request(
    source_id: SourceId,
    lifecycle_generation: u64,
    expected_index_revision: Option<u64>,
    query_id: u64,
    after_cursor: Option<CatalogIndexPageCursor>,
    page_limit: usize,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let query_token = admission_state(admission, |state| {
        state
            .index_queries
            .get(&source_id)
            .filter(|(generation, id, token)| {
                *generation == lifecycle_generation
                    && *id == query_id
                    && !token.load(Ordering::Acquire)
            })
            .map(|(_, _, token)| token.clone())
    });
    let Some(query_token) = query_token else {
        return;
    };
    let Some(source) = sources
        .get(&source_id)
        .filter(|source| source.lifecycle_generation == lifecycle_generation)
    else {
        return;
    };
    let read = || -> Result<CatalogIndexPage, CatalogIndexPageError> {
        if profile_guard.validate_current().is_err()
            || source
                .database
                .validate_profile_guard(profile_guard)
                .is_err()
            || source.publication_fence.validate().is_err()
        {
            return Err(CatalogIndexPageError::OwnerUnavailable);
        }
        if after_cursor.as_ref().is_some_and(|cursor| {
            cursor.source_id != source_id
                || cursor.lifecycle_generation != lifecycle_generation
                || cursor.query_id != query_id
                || !Arc::ptr_eq(&cursor.query_token, &query_token)
        }) {
            return Err(CatalogIndexPageError::ForeignCursor);
        }
        let page = source
            .database
            .source_index_page(
                after_cursor.as_ref().map(|cursor| &cursor.cursor),
                page_limit,
            )
            .map_err(page_error)?;
        if let Some(expected) = expected_index_revision {
            if page.revision != expected {
                return Err(CatalogIndexPageError::StaleRevision {
                    expected,
                    actual: page.revision,
                });
            }
        }
        // Revalidate the retained lease/profile after the bounded database read.
        if profile_guard.validate_current().is_err()
            || source
                .database
                .validate_profile_guard(profile_guard)
                .is_err()
            || source.publication_fence.validate().is_err()
        {
            return Err(CatalogIndexPageError::OwnerUnavailable);
        }
        Ok(CatalogIndexPage {
            schema_available: page.schema_available,
            index_revision: page.revision,
            entries: page.entries,
            next: page.next.map(|cursor| CatalogIndexPageCursor {
                source_id: source_id.clone(),
                lifecycle_generation,
                query_id,
                cursor,
                query_token: query_token.clone(),
            }),
        })
    };
    let event = match read() {
        Ok(page) => CatalogEvent::IndexPageReady {
            source_id: source_id.clone(),
            lifecycle_generation,
            query_id,
            completeness: RetainedIndexCompleteness::RetainedUnknown,
            page,
        },
        Err(error) => CatalogEvent::IndexPageFailed {
            source_id: source_id.clone(),
            lifecycle_generation,
            query_id,
            error,
        },
    };
    // Hold admission through queue publication. Removal/supersession cannot race the enqueue.
    let state = admission
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !publication_is_current_state(&state, &source_id, lifecycle_generation, None)
        || query_token.load(Ordering::Acquire)
    {
        return;
    }
    assert!(reservation.hub.send_index_page(
        event,
        source.publication_fence.clone(),
        query_token,
        reservation.tag_read_payload_bytes
    ));
    reservation.remaining -= 1;
    reservation.tag_read_payload_bytes = 0;
}
