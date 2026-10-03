//! Bounded aggregation under one actual source attempt and retained global generation.
use super::*;
pub(crate) fn dispatch(
    request: SourceHarvestFilterRequest,
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
    let context = request.context();
    let token = lifetime::begin(
        admission,
        context.source_id(),
        context.lifecycle_generation(),
    );
    let result = match (token.as_ref(), sources.get_mut(context.source_id())) {
        (Ok(token), Some(source)) => {
            source.attempt_token = Some(Arc::clone(token));
            produce(
                source,
                &request,
                &cancellation,
                token,
                admission,
                guard,
                client,
            )
        }
        (Err(error), _) => Err(*error),
        _ => Err(HarvestFactsFailure::StaleSource),
    };
    let mut state = admission.lock().unwrap_or_else(|p| p.into_inner());
    if state.closed
        || state.shutdown
        || cancellation.is_canceled()
        || token.as_ref().is_ok_and(|t| t.load(Ordering::Acquire))
    {
        return;
    }
    let result = result.and_then(|mask| {
        if mask.is_live() {
            Ok(mask)
        } else {
            Err(HarvestFactsFailure::GlobalChanged)
        }
    });
    if result.is_err()
        && let Ok(token) = token.as_ref()
    {
        state.retained_harvest.discard_failed(
            context.source_id(),
            context.lifecycle_generation(),
            token,
        );
    }
    let fence = result.as_ref().ok().map(|m| m.source.clone());
    reservation.publish_with_fence(
        CatalogEvent::HarvestFilterCompleted {
            request,
            cancellation,
            result,
        },
        fence,
    );
}
fn produce(
    source: &RegisteredSource,
    request: &SourceHarvestFilterRequest,
    cancellation: &HarvestFactsCancellation,
    token: &Arc<AtomicBool>,
    admission: &Arc<Mutex<AdmissionState>>,
    guard: &WritableProfileGuard,
    client: Option<&HarvestFactsClient>,
) -> Result<CatalogHarvestFilter, HarvestFactsFailure> {
    let snapshot = source
        .snapshot
        .as_ref()
        .ok_or(HarvestFactsFailure::StaleSource)?;
    if snapshot.manifest.len() != request.entry_count {
        return Err(HarvestFactsFailure::StaleSource);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut words = vec![0u64; request.entry_count.div_ceil(64)];
    let mut anchor: Option<HarvestFactsBatch> = None;
    let mut start = 0;
    while start < snapshot.manifest.len() {
        if Instant::now() >= deadline {
            return Err(HarvestFactsFailure::QueryBudget);
        }
        if anchor
            .as_ref()
            .is_some_and(|a| !a.is_current_global_generation())
        {
            return Err(HarvestFactsFailure::GlobalChanged);
        }
        let mut paths = Vec::new();
        let mut bytes = 0;
        for entry in snapshot.manifest[start..]
            .iter()
            .take(HARVEST_FACTS_MAX_PATHS)
        {
            let path = entry
                .relative_path
                .to_str()
                .ok_or(HarvestFactsFailure::InvalidInput)?;
            if bytes + path.len() > HARVEST_FACTS_MAX_PATH_BYTES {
                break;
            }
            bytes += path.len();
            paths.push(path.to_owned());
        }
        if paths.is_empty() {
            return Err(HarvestFactsFailure::InvalidInput);
        }
        let c = &request.context;
        let chunk = SourceHarvestFactsRequest::new(
            c.profile.clone(),
            c.source_id.clone(),
            c.lifecycle,
            c.root.clone(),
            c.directory,
            c.revision,
            c.request_id,
            &paths,
        )?;
        let batch = read::read_until(
            source,
            &chunk,
            cancellation,
            token,
            admission,
            guard,
            client,
            deadline,
        )?;
        for (offset, entry) in batch.entries().iter().enumerate() {
            if request
                .choice
                .includes(entry.state(), entry.immediate_derivative_count() as usize)
            {
                let i = start + offset;
                words[i / 64] |= 1 << (i % 64);
            }
        }
        start += paths.len();
        if anchor.is_none() {
            anchor = Some(batch);
        }
    }
    // Empty manifests still require genuine profile/source validation, never an invented facts grant.
    read::check(
        source,
        &request.context,
        cancellation,
        token,
        admission,
        guard,
    )?;
    if client.is_none_or(|c| c.acquisition_id() != request.context.profile) {
        return Err(HarvestFactsFailure::OwnerUnavailable);
    }
    source
        .publication_fence
        .validate()
        .map_err(|_| HarvestFactsFailure::StaleSource)?;
    if Instant::now() >= deadline {
        return Err(HarvestFactsFailure::QueryBudget);
    }
    Ok(CatalogHarvestFilter {
        request: request.clone(),
        words: words.into(),
        anchor,
        source: source.publication_fence.clone(),
        attempt: Arc::clone(token),
        cancellation: cancellation.clone(),
    })
}
