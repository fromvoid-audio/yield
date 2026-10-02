//! Bounded retained-page and inspection state, without UI or filesystem dispatch.
use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Binding {
    pub(super) source_id: SourceId,
    pub(super) generation: u64,
    pub(super) query_id: u64,
}
#[derive(Clone)]
pub(super) struct PendingPage {
    pub(super) after: Option<CatalogIndexPageCursor>,
    pub(super) expected: Option<u64>,
    pub(super) admitted: bool,
}
pub(super) struct PendingInspection {
    pub(super) row: usize,
    pub(super) request_id: u64,
    pub(super) cancellation: IndexInspectionCancellation,
    pub(super) admitted: bool,
}
/// One source/query, one bounded page and one pending owner inspection.
#[derive(Default)]
pub(super) struct Model {
    pub(super) binding: Option<Binding>,
    pub(super) next_query: u64,
    pub(super) next_request: u64,
    pub(super) page_epoch: u64,
    pub(super) page: Option<CatalogIndexPage>,
    pub(super) pending: Option<PendingPage>,
    pub(super) current_start: Option<CatalogIndexPageCursor>,
    pub(super) history: Vec<Option<CatalogIndexPageCursor>>,
    pub(super) selected: Option<usize>,
    pub(super) inspection: Option<CatalogIndexInspection>,
    pub(super) inspection_pending: Option<PendingInspection>,
    pub(super) status: &'static str,
}
impl Model {
    pub(super) fn cancel(&mut self) -> Option<Binding> {
        self.page_epoch = self.page_epoch.wrapping_add(1).max(1);
        if let Some(pending) = self.inspection_pending.take() {
            pending.cancellation.cancel();
        }
        self.inspection = None;
        self.page = None;
        self.pending = None;
        self.history.clear();
        self.current_start = None;
        self.selected = None;
        self.binding.take()
    }
    pub(super) fn begin(&mut self, source_id: SourceId, generation: u64) {
        self.cancel();
        self.next_query = self.next_query.wrapping_add(1).max(1);
        self.binding = Some(Binding {
            source_id,
            generation,
            query_id: self.next_query,
        });
        self.pending = Some(PendingPage {
            after: None,
            expected: None,
            admitted: false,
        });
        self.status = "Loading retained entries…";
    }
    pub(super) fn matches(&self, source: &SourceId, generation: u64, query: u64) -> bool {
        self.binding.as_ref().is_some_and(|b| {
            b.source_id == *source && b.generation == generation && b.query_id == query
        })
    }
    pub(super) fn page_command(&self) -> Option<CatalogCommand> {
        let b = self.binding.as_ref()?;
        let p = self.pending.as_ref().filter(|p| !p.admitted)?;
        Some(CatalogCommand::RequestIndexPage {
            source_id: b.source_id.clone(),
            lifecycle_generation: b.generation,
            query_id: b.query_id,
            expected_index_revision: p.expected,
            after_cursor: p.after.clone(),
            page_limit: SOURCE_INDEX_PAGE_MAX_ENTRIES,
        })
    }
    pub(super) fn change_page(&mut self, next: bool) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let Some(page) = &self.page else {
            return false;
        };
        let expected = page.index_revision;
        let after = if next {
            let Some(cursor) = page.next.clone() else {
                return false;
            };
            if self.history.len() == HISTORY_LIMIT {
                self.history.remove(0);
            }
            self.history.push(self.current_start.clone());
            Some(cursor)
        } else {
            let Some(cursor) = self.history.pop() else {
                return false;
            };
            cursor
        };
        if let Some(p) = self.inspection_pending.take() {
            p.cancellation.cancel();
        }
        self.inspection = None;
        self.selected = None;
        self.page = None;
        self.pending = Some(PendingPage {
            after,
            expected: Some(expected),
            admitted: false,
        });
        self.status = "Loading retained entries…";
        true
    }
    pub(super) fn select_row_for_page(&mut self, row: usize, epoch: u64) -> bool {
        epoch == self.page_epoch && self.select_row(row)
    }
    pub(super) fn select_row(&mut self, row: usize) -> bool {
        let Some(entry) = self.page.as_ref().and_then(|page| page.entries.get(row)) else {
            return false;
        };
        let available = eligible(entry);
        if self.selected != Some(row) {
            if let Some(pending) = self.inspection_pending.take() {
                pending.cancellation.cancel();
            }
            self.inspection = None;
            self.status = if available {
                "Press Enter to check file identity. Native reveal unavailable."
            } else {
                "File identity unavailable. Inspection and reveal unavailable."
            };
        }
        self.selected = Some(row);
        true
    }
    pub(super) fn prepare_inspection(&mut self, row: usize) -> bool {
        if self.pending.is_some() || !self.select_row(row) {
            return false;
        }
        let Some(page) = &self.page else {
            return false;
        };
        if !page.entries.get(row).is_some_and(eligible)
            || page.inspection_handles.get(row).is_none()
        {
            return false;
        }
        if let Some(p) = self.inspection_pending.take() {
            p.cancellation.cancel();
        }
        self.inspection = None;
        self.selected = Some(row);
        self.next_request = self.next_request.wrapping_add(1).max(1);
        self.inspection_pending = Some(PendingInspection {
            row,
            request_id: self.next_request,
            cancellation: IndexInspectionCancellation::default(),
            admitted: false,
        });
        self.status = "Inspecting entry through source owner…";
        true
    }
    pub(super) fn inspection_command(&self) -> Option<CatalogCommand> {
        let pending = self.inspection_pending.as_ref().filter(|p| !p.admitted)?;
        let handle = self
            .page
            .as_ref()?
            .inspection_handles
            .get(pending.row)?
            .clone();
        Some(CatalogCommand::RequestIndexInspection {
            handle,
            request_id: pending.request_id,
            cancellation: pending.cancellation.clone(),
        })
    }
    pub(super) fn apply(&mut self, event: &mut CatalogEvent) -> bool {
        match event {
            CatalogEvent::IndexPageReady {
                source_id,
                lifecycle_generation,
                query_id,
                page,
                ..
            } if self.matches(source_id, *lifecycle_generation, *query_id)
                && self.pending.is_some() =>
            {
                let pending = self.pending.take().unwrap();
                if !bounded_page(page) || pending.expected.is_some_and(|r| r != page.index_revision)
                {
                    self.page = None;
                    self.status = "Retained page invalid or changed. Restart the list.";
                    return true;
                }
                self.page_epoch = self.page_epoch.wrapping_add(1).max(1);
                self.current_start = pending.after;
                self.status = if !page.schema_available {
                    "Retained index unavailable in this database. Scan completeness unknown."
                } else if page.entries.is_empty() {
                    "No retained entries on this page. Scan completeness unknown."
                } else {
                    "Retained entries only. Scan completeness unknown. Native reveal unavailable."
                };
                // Move the bounded owner delivery, preserving its opaque row handles exactly.
                self.page = Some(std::mem::replace(
                    page,
                    CatalogIndexPage {
                        schema_available: false,
                        index_revision: 0,
                        entries: Vec::new(),
                        next: None,
                        inspection_handles: Vec::new(),
                    },
                ));
                self.selected = self
                    .page
                    .as_ref()
                    .and_then(|p| (!p.entries.is_empty()).then_some(0));
                true
            }
            CatalogEvent::IndexPageFailed {
                source_id,
                lifecycle_generation,
                query_id,
                error,
            } if self.matches(source_id, *lifecycle_generation, *query_id)
                && self.pending.is_some() =>
            {
                self.pending = None;
                self.page = None;
                self.status = match error {
                    yield_session::CatalogIndexPageError::StaleRevision { .. } => {
                        "Retained index changed. Restart the list."
                    }
                    yield_session::CatalogIndexPageError::ByteLimit
                    | yield_session::CatalogIndexPageError::InputTooLarge => {
                        "Retained page exceeds its byte limit. List unavailable."
                    }
                    yield_session::CatalogIndexPageError::OwnerUnavailable => {
                        "Source owner unavailable. Restart after recovery."
                    }
                    _ => "Retained index read failed. Restart after recovery.",
                };
                true
            }
            CatalogEvent::IndexInspectionCompleted {
                source_id,
                lifecycle_generation,
                query_id,
                request_id,
                cancellation,
                result,
            } if self.matches(source_id, *lifecycle_generation, *query_id)
                && self
                    .inspection_pending
                    .as_ref()
                    .is_some_and(|p| p.request_id == *request_id) =>
            {
                let pending = self.inspection_pending.take().unwrap();
                if pending.cancellation.is_canceled() || cancellation.is_canceled() {
                    self.status = "Inspection canceled. Retry or restart.";
                    return true;
                }
                let result = std::mem::replace(
                    result,
                    Err(yield_session::SourceIndexInspectionError::Unavailable),
                );
                match result {
                    Ok(inspection)
                        if inspection.is_live()
                            && self.page.as_ref().is_some_and(|p| {
                                p.index_revision == inspection.index_revision()
                                    && p.entries.get(pending.row) == Some(inspection.entry())
                            }) =>
                    {
                        self.inspection = Some(inspection);
                        self.status = "Entry inspected by source owner. Native reveal unavailable.";
                    }
                    Ok(_) => self.status = "Inspection is stale. Restart the list.",
                    Err(yield_session::SourceIndexInspectionError::Unsupported) => {
                        self.status = "Owner inspection unavailable on this platform. Native reveal unavailable."
                    }
                    Err(yield_session::SourceIndexInspectionError::Busy) => {
                        self.status =
                            "Owner inspection limit reached. Close other inspections and retry."
                    }
                    Err(_) => {
                        self.status = "Entry changed, unavailable or inaccessible. Restart or choose another entry."
                    }
                };
                true
            }
            _ => false,
        }
    }
}
pub(super) fn bounded_page(page: &CatalogIndexPage) -> bool {
    page.entries.len() <= SOURCE_INDEX_PAGE_MAX_ENTRIES
        && page.inspection_handles.len() == page.entries.len()
        && (page.schema_available || (page.entries.is_empty() && page.next.is_none()))
        && page
            .entries
            .iter()
            .try_fold(0usize, |bytes, entry| {
                let bytes = bytes
                    .checked_add(entry.relative_path.as_os_str().as_encoded_bytes().len())?
                    .checked_add(entry.file_identity.as_ref().map_or(0, String::len))?;
                (bytes <= PAGE_BYTES).then_some(bytes)
            })
            .is_some()
}
pub(super) fn eligible(entry: &SourceIndexEntry) -> bool {
    !entry.relative_path.as_os_str().is_empty()
        && !entry
            .relative_path
            .as_os_str()
            .as_encoded_bytes()
            .contains(&0)
        && entry
            .relative_path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && entry.classification != SourceIndexClassification::Inaccessible
        && entry.file_size.is_some()
        && entry.modified_ns.is_some()
        && entry
            .file_identity
            .as_ref()
            .is_some_and(|id| !id.is_empty() && id.len() <= 512)
        && entry.relative_path.as_os_str().as_encoded_bytes().len() <= DISPLAY_BYTES
}
