//! Actual global SQLite VM budget; corruption cannot turn a bounded path read into a scan.
use super::*;
use rusqlite::Connection;
use std::time::{Duration, Instant};
const MAX_CALLBACKS: usize = 1024;
struct Progress<'a>(&'a Connection);
impl Drop for Progress<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}
pub(super) fn run<T>(
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
