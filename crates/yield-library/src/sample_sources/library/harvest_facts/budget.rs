//! Actual global SQLite VM budget; corruption cannot turn a bounded path read into a scan.
use super::*;
use rusqlite::{Connection, limits::Limit};
use std::time::{Duration, Instant};
const MAX_CALLBACKS: usize = 1024;
struct Progress<'a>(&'a Connection);
impl Drop for Progress<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}
pub(in crate::sample_sources::library) fn run<T>(
    connection: &Connection,
    canceled: &Arc<AtomicBool>,
    read: impl FnOnce() -> Result<T, LibraryError>,
) -> Result<T, LibraryError> {
    let cancel = Arc::clone(canceled);
    let exhausted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&exhausted);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut calls = 0usize;
    connection.progress_handler(
        1000,
        Some(move || {
            calls += 1;
            if cancel.load(Ordering::Acquire) {
                return true;
            }
            if calls > MAX_CALLBACKS || Instant::now() >= deadline {
                flag.store(true, Ordering::Release);
                return true;
            }
            false
        }),
    );
    let _progress = Progress(connection);
    let result = read();
    if canceled.load(Ordering::Acquire) {
        return Err(LibraryError::HarvestFactsCanceled);
    }
    if exhausted.load(Ordering::Acquire) {
        return Err(LibraryError::HarvestFactsQueryBudget);
    }
    result
}

// SQLite can compare an unindexed foreign key before any matching-row shape predicate.
// This caps that native value/record allocation, not only Rust row materialization.
const SQLITE_HISTORY_VALUE_LIMIT: i32 = 256 * 1024;
pub(in crate::sample_sources::library) struct HistoricalValueLimit<'a> {
    connection: &'a Connection,
    previous: i32,
}
impl<'a> HistoricalValueLimit<'a> {
    pub(in crate::sample_sources::library) fn new(
        connection: &'a Connection,
    ) -> Result<Self, LibraryError> {
        let previous = connection.limit(Limit::SQLITE_LIMIT_LENGTH)?;
        connection.set_limit(
            Limit::SQLITE_LIMIT_LENGTH,
            previous.min(SQLITE_HISTORY_VALUE_LIMIT),
        )?;
        Ok(Self {
            connection,
            previous,
        })
    }
}
impl Drop for HistoricalValueLimit<'_> {
    fn drop(&mut self) {
        // Borrowed connection stays live through normal return, refusal and worker unwind.
        let _ = self
            .connection
            .set_limit(Limit::SQLITE_LIMIT_LENGTH, self.previous);
    }
}
