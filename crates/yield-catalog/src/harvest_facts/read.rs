//! Source/root/browser validation and bounded wait on the genuine global read client.
use super::*;
use yield_library::sample_sources::library::{HarvestFactsRequest, LibraryError};
pub(super) fn read(
    source: &RegisteredSource,
    request: &SourceHarvestFactsRequest,
    cancellation: &HarvestFactsCancellation,
    token: &Arc<AtomicBool>,
    admission: &Arc<Mutex<AdmissionState>>,
    guard: &WritableProfileGuard,
    client: Option<&HarvestFactsClient>,
) -> Result<HarvestFactsBatch, HarvestFactsFailure> {
    let client = client.ok_or(HarvestFactsFailure::OwnerUnavailable)?;
    if request.profile.as_str() != guard.owner_identity()
        || client.acquisition_id() != request.profile
    {
        return Err(HarvestFactsFailure::OwnerUnavailable);
    }
    check(source, request, cancellation, token, admission, guard)?;
    let mut files = Vec::with_capacity(request.paths.len());
    for path in &request.paths {
        if canceled(cancellation, token) {
            return Err(HarvestFactsFailure::Canceled);
        }
        let observation = source
            .database
            .writer
            .observe_file_identity(path.clone())
            .map_err(|_| HarvestFactsFailure::IdentityUnavailable)?;
        if observation.source_revision().revision() != request.revision {
            return Err(HarvestFactsFailure::StaleSource);
        }
        files.push(observation);
    }
    // Internal cancellation stops timed-out work while preserving the caller's failure delivery.
    let mut pending = PendingRead {
        cancellation: HarvestFactsCancellation::new(),
        armed: true,
    };
    let query =
        HarvestFactsRequest::new(guard, files, pending.cancellation.clone()).map_err(failure)?;
    let receiver = client
        .read(query)
        .map_err(|_| HarvestFactsFailure::OwnerUnavailable)?;
    let batch = await_batch(
        receiver,
        cancellation,
        token,
        Instant::now() + Duration::from_secs(2),
    )?;
    check(source, request, cancellation, token, admission, guard)?;
    let current = source
        .database
        .writer
        .current_revision()
        .map_err(|_| HarvestFactsFailure::StaleSource)?;
    if batch.source_revision() != &current || !batch.is_current_global_generation() {
        return Err(HarvestFactsFailure::GlobalChanged);
    }
    if batch.entries().len() != request.paths.len()
        || batch
            .entries()
            .iter()
            .zip(&request.paths)
            .any(|(entry, path)| entry.path() != path)
    {
        return Err(HarvestFactsFailure::Malformed);
    }
    pending.armed = false;
    Ok(batch)
}
struct PendingRead {
    cancellation: HarvestFactsCancellation,
    armed: bool,
}
impl Drop for PendingRead {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}
fn await_batch(
    receiver: Receiver<Result<HarvestFactsBatch, LibraryError>>,
    cancellation: &HarvestFactsCancellation,
    token: &Arc<AtomicBool>,
    deadline: Instant,
) -> Result<HarvestFactsBatch, HarvestFactsFailure> {
    loop {
        if canceled(cancellation, token) {
            return Err(HarvestFactsFailure::Canceled);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(HarvestFactsFailure::QueryBudget)?;
        match receiver.recv_timeout(remaining.min(Duration::from_millis(10))) {
            Ok(result) => return result.map_err(failure),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(HarvestFactsFailure::OwnerUnavailable);
            }
        }
    }
}
fn canceled(cancellation: &HarvestFactsCancellation, token: &Arc<AtomicBool>) -> bool {
    cancellation.is_canceled() || token.load(Ordering::Acquire)
}
fn check(
    source: &RegisteredSource,
    request: &SourceHarvestFactsRequest,
    cancellation: &HarvestFactsCancellation,
    token: &Arc<AtomicBool>,
    admission: &Arc<Mutex<AdmissionState>>,
    guard: &WritableProfileGuard,
) -> Result<(), HarvestFactsFailure> {
    if canceled(cancellation, token)
        || !publication_is_current(
            admission,
            &request.source_id,
            request.lifecycle,
            Some(token),
        )
    {
        return Err(HarvestFactsFailure::Canceled);
    }
    guard
        .validate_current()
        .map_err(|_| HarvestFactsFailure::OwnerUnavailable)?;
    let snapshot = source
        .snapshot
        .as_ref()
        .ok_or(HarvestFactsFailure::StaleSource)?;
    if source.lifecycle_generation != request.lifecycle
        || snapshot.source_id != request.source_id
        || snapshot.lifecycle_generation != request.lifecycle
        || snapshot.root_identity != request.root
        || snapshot.directory_generation != request.directory
        || snapshot.committed_revision != request.revision
        || source
            .known_root_identity
            .as_ref()
            .is_some_and(|root| root != &request.root)
        || source_root_identity(&source.root).map_err(|_| HarvestFactsFailure::StaleSource)?
            != request.root
    {
        return Err(HarvestFactsFailure::StaleSource);
    }
    let revision = source
        .database
        .writer
        .current_revision()
        .map_err(|_| HarvestFactsFailure::StaleSource)?;
    if revision.source_id() != &request.source_id
        || revision.lifecycle_generation() != request.lifecycle
        || revision.root_identity() != &request.root
        || revision.revision() != request.revision
    {
        return Err(HarvestFactsFailure::StaleSource);
    }
    for path in &request.paths {
        if snapshot
            .manifest
            .binary_search_by(|entry| entry.relative_path.as_path().cmp(Path::new(path.as_str())))
            .is_err()
        {
            return Err(HarvestFactsFailure::IdentityUnavailable);
        }
    }
    Ok(())
}
fn failure(error: LibraryError) -> HarvestFactsFailure {
    match error {
        LibraryError::HarvestFactsInput => HarvestFactsFailure::InvalidInput,
        LibraryError::HarvestFactsCanceled => HarvestFactsFailure::Canceled,
        LibraryError::HarvestFactsSourceChanged => HarvestFactsFailure::StaleSource,
        LibraryError::HarvestFactsStaleOwner => HarvestFactsFailure::OwnerUnavailable,
        LibraryError::HarvestFactsMalformed => HarvestFactsFailure::Malformed,
        LibraryError::HarvestFactsIdentityUncertain => HarvestFactsFailure::IdentityUnavailable,
        LibraryError::HarvestFactsQueryBudget => HarvestFactsFailure::QueryBudget,
        _ => HarvestFactsFailure::OwnerUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn harvest_catalog_wait_timeout_cancels_internal_work_but_preserves_failure_delivery() {
        // Transport-control fixture produces no facts or owner/physical capability.
        let (sender, receiver) = mpsc::channel();
        let caller = HarvestFactsCancellation::new();
        let backend = HarvestFactsCancellation::new();
        let pending = PendingRead {
            cancellation: backend.clone(),
            armed: true,
        };
        assert_eq!(
            await_batch(
                receiver,
                &caller,
                &Arc::new(AtomicBool::new(false)),
                Instant::now()
            )
            .unwrap_err(),
            HarvestFactsFailure::QueryBudget
        );
        drop(pending);
        assert!(backend.is_canceled());
        assert!(!caller.is_canceled());
        drop(sender);
    }
}
