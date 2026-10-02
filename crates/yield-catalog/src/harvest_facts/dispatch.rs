//! Actual catalog worker publication; no source/global SQL runs under the admission mutex.
use super::*;
pub(crate) fn dispatch(
    request: SourceHarvestFactsRequest,
    cancellation: HarvestFactsCancellation,
    guard: &WritableProfileGuard,
    client: Option<&HarvestFactsClient>,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    if cancellation.is_canceled() {
        return;
    }
    let token = begin_source_attempt(admission, &request.source_id, request.lifecycle);
    let mut fence = None;
    let result = if let Some(token) = &token {
        if let Some(source) = sources.get_mut(&request.source_id) {
            source.attempt_token = Some(Arc::clone(token));
            read::read(
                source,
                &request,
                &cancellation,
                token,
                admission,
                guard,
                client,
            )
            .and_then(|batch| {
                // Slow retained ownership validation stays outside admission and UI locks.
                source
                    .publication_fence
                    .validate()
                    .map_err(|_| HarvestFactsFailure::StaleSource)?;
                fence = Some(source.publication_fence.clone());
                Ok(CatalogHarvestFacts {
                    request: request.clone(),
                    batch,
                    source: source.publication_fence.clone(),
                    attempt: Arc::clone(token),
                    cancellation: cancellation.clone(),
                })
            })
        } else {
            Err(HarvestFactsFailure::StaleSource)
        }
    } else {
        Err(HarvestFactsFailure::StaleSource)
    };
    let state = admission.lock().unwrap_or_else(|p| p.into_inner());
    if state.closed
        || state.shutdown
        || cancellation.is_canceled()
        || token
            .as_ref()
            .is_some_and(|token| token.load(Ordering::Acquire))
    {
        return;
    }
    let result = if result.as_ref().is_ok_and(|facts| !facts.is_live()) {
        fence = None;
        Err(HarvestFactsFailure::GlobalChanged)
    } else {
        result
    };
    reservation.publish_with_fence(
        CatalogEvent::HarvestFactsCompleted {
            request,
            cancellation,
            result,
        },
        fence,
    );
}
