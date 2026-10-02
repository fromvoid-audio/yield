//! Exact pending-request application and cheap projection/global invalidation before rendering.
use super::*;
/// Clear an invalid cached mask before rendering, without owner calls or request submission.
pub(in crate::browser) fn invalidate_if_stale(browser: &mut Browser, cx: &Context<Browser>) {
    if browser.harvest_filter.ready.is_none() {
        return;
    }
    let current = context(browser);
    let profile = cx
        .try_global::<SourceRegistryGlobal>()
        .and_then(|global| global.vocabulary_acquisition_id().ok());
    if browser
        .harvest_filter
        .ready
        .as_ref()
        .is_some_and(|(c, mask)| {
            !mask.is_live()
                || current.as_ref().is_none_or(|now| !matches(c, now))
                || profile.as_ref() != Some(mask.request().context().profile_acquisition())
        })
    {
        browser.harvest_filter.focus = Some((
            browser.browser_focus_id(),
            browser.selected_visible_position(),
        ));
        browser.harvest_filter.ready.take();
        browser
            .view_model
            .invalidate_harvest_filter(&browser.entries);
        browser.refresh_collection_filter_counts();
        browser.reconcile_selection();
        browser.invalidate_file_handoff();
    }
}
/// Invalidate cheap retained fences before submitting a current request on the UI poll.
pub(in crate::browser) fn poll(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    if !mode(browser).filtering() {
        return false;
    }
    if cx.try_global::<SourceRegistryGlobal>().is_none() {
        if browser.harvest_filter.error.is_some() {
            return false;
        }
        browser.harvest_filter.cancel();
        browser
            .view_model
            .invalidate_harvest_filter(&browser.entries);
        browser.harvest_filter.error = Some("Harvest requires an available profile".into());
        return true;
    }
    let current = context(browser);
    let profile = cx
        .try_global::<SourceRegistryGlobal>()
        .and_then(|global| global.vocabulary_acquisition_id().ok());
    let ready_live = browser
        .harvest_filter
        .ready
        .as_ref()
        .is_some_and(|(c, mask)| {
            current.as_ref().is_some_and(|now| matches(c, now))
                && mask.is_live()
                && profile.as_ref() == Some(mask.request().context().profile_acquisition())
        });
    if ready_live && !browser.view_model.harvest_filter().is_pending() {
        return false;
    }
    let mut changed = browser
        .view_model
        .invalidate_harvest_filter(&browser.entries);
    if browser.harvest_filter.ready.take().is_some() {
        changed = true;
    }
    if browser.harvest_filter.pending.as_ref().is_some_and(|p| {
        current.as_ref().is_none_or(|c| !matches(c, &p.context))
            || profile.as_ref() != Some(p.request.context().profile_acquisition())
    }) {
        browser.harvest_filter.cancel();
        changed = true;
    }
    if browser
        .harvest_filter
        .pending
        .as_ref()
        .is_some_and(|p| p.started.elapsed() > std::time::Duration::from_secs(6))
    {
        browser.harvest_filter.cancel();
        browser.harvest_filter.error =
            Some("Harvest filter timed out; select a choice to retry".into());
        return true;
    }
    if browser.harvest_filter.pending.is_some() || browser.harvest_filter.error.is_some() {
        return changed;
    }
    match cx.global::<SourceRegistryGlobal>().catalog_status() {
        CatalogStatus::Closed | CatalogStatus::Unavailable { .. } => {
            browser.harvest_filter.error =
                Some("Harvest catalog is unavailable; retry when the profile is ready".into());
            return true;
        }
        _ => {}
    }
    if current.is_some()
        && profile.is_none()
        && cx.global::<SourceRegistryGlobal>().catalog_status() == CatalogStatus::Ready
    {
        browser.harvest_filter.error = Some("Harvest requires an available profile".into());
        return true;
    }
    let (Some(c), Some(profile)) = (current, profile) else {
        return changed;
    };
    if cx.global::<SourceRegistryGlobal>().catalog_status() != CatalogStatus::Ready {
        return changed;
    }
    browser.harvest_filter.next_request = browser
        .harvest_filter
        .next_request
        .checked_add(1)
        .unwrap_or(1);
    let request = match SourceHarvestFilterRequest::new(
        profile,
        c.source_id.clone(),
        c.lifecycle_generation,
        c.root_identity.clone(),
        c.directory_generation,
        c.revision,
        browser.harvest_filter.next_request,
        browser.entries.len(),
        mode(browser).choice,
    ) {
        Ok(r) => r,
        Err(e) => {
            browser.harvest_filter.error = Some(failure_label(e).into());
            return true;
        }
    };
    let cancellation = HarvestFactsCancellation::new();
    match cx
        .global::<SourceRegistryGlobal>()
        .submit_catalog(CatalogCommand::RequestHarvestFilter {
            request: request.clone(),
            cancellation: cancellation.clone(),
        }) {
        Ok(()) => {
            browser.harvest_filter.pending = Some(Pending {
                request,
                context: c,
                cancellation,
                started: std::time::Instant::now(),
            });
        }
        Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => {}
        Err(e) => {
            browser.harvest_filter.error = Some(format!("Harvest filter unavailable: {e}"));
            return true;
        }
    }
    changed
}
/// Apply only the exact pending request, current profile and immutable positional mapping.
pub(in crate::browser) fn apply_catalog_event(
    browser: &mut Browser,
    event: CatalogEvent,
    cx: &mut Context<Browser>,
) -> bool {
    let CatalogEvent::HarvestFilterCompleted {
        request,
        cancellation,
        result,
    } = event
    else {
        return false;
    };
    let Some(p) = browser.harvest_filter.pending.as_ref() else {
        return false;
    };
    if p.request != request
        || cancellation.is_canceled()
        || !mode(browser).filtering()
        || mode(browser).choice != request.choice()
    {
        return false;
    }
    let current = context(browser);
    if current.as_ref().is_none_or(|c| !matches(c, &p.context))
        || cx
            .global::<SourceRegistryGlobal>()
            .vocabulary_acquisition_id()
            .ok()
            .as_ref()
            != Some(request.context().profile_acquisition())
    {
        return false;
    }
    let c = p.context.clone();
    browser.harvest_filter.pending.take();
    match result {
        Ok(mask) if mask.is_live() => {
            if let Ok(words) = TagFilterMask::try_from_words(request.entry_count(), mask.words()) {
                if browser
                    .view_model
                    .apply_harvest_filter(&browser.entries, words)
                    .is_ok()
                {
                    browser.harvest_filter.ready = Some((c, mask));
                    let current_focus = browser
                        .browser_focus_id()
                        .map(|id| (Some(id), browser.selected_visible_position()));
                    if let Some(focus) =
                        current_focus.or_else(|| browser.harvest_filter.focus.take())
                    {
                        browser.reset_browser_focus_state();
                        browser.restore_projection_focus(focus.0, focus.1, cx);
                    }
                }
            }
        }
        Ok(_) => {}
        Err(
            yield_session::HarvestFactsFailure::GlobalChanged
            | yield_session::HarvestFactsFailure::Canceled
            | yield_session::HarvestFactsFailure::StaleSource,
        ) => {}
        Err(e) => browser.harvest_filter.error = Some(failure_label(e).into()),
    }
    browser.refresh_collection_filter_counts();
    browser.reconcile_selection();
    browser.invalidate_file_handoff();
    true
}
