//! One bounded retained Harvest mask token per source, independent of ephemeral read attempts.
//! Tokens are cancellation/freshness only; no write, registration or capacity authority.
use super::*;

const MAX_RETAINED_SOURCES: usize = 4096;
#[derive(Default)]
pub(crate) struct RetainedMasks(HashMap<SourceId, (u64, Arc<AtomicBool>)>);
impl RetainedMasks {
    pub(crate) fn owns(&self, id: &SourceId, life: u64, token: &Arc<AtomicBool>) -> bool {
        self.0
            .get(id)
            .is_some_and(|(generation, current)| *generation == life && Arc::ptr_eq(current, token))
    }
    fn hold(&mut self, id: &SourceId, life: u64, token: &Arc<AtomicBool>) -> bool {
        if self.0.len() >= MAX_RETAINED_SOURCES && !self.0.contains_key(id) {
            return false;
        }
        self.cancel_source(id);
        self.0.insert(id.clone(), (life, Arc::clone(token)));
        true
    }
    // A completed failed read retains no mask; removing its slot does not reopen a token.
    pub(crate) fn discard_failed(&mut self, id: &SourceId, life: u64, token: &Arc<AtomicBool>) {
        if self.owns(id, life, token) {
            self.0.remove(id);
        }
    }
    pub(crate) fn cancel_source(&mut self, id: &SourceId) {
        if let Some((_, token)) = self.0.remove(id) {
            token.store(true, Ordering::Release);
        }
    }
    pub(crate) fn cancel_all(&mut self) {
        for (_, (_, token)) in self.0.drain() {
            token.store(true, Ordering::Release);
        }
    }
}

pub(crate) fn begin(
    admission: &Arc<Mutex<AdmissionState>>,
    id: &SourceId,
    life: u64,
) -> Result<Arc<AtomicBool>, HarvestFactsFailure> {
    // A new Harvest request supersedes its old mask and the ordinary current attempt.
    let token =
        begin_source_attempt(admission, id, life).ok_or(HarvestFactsFailure::StaleSource)?;
    let mut state = admission.lock().unwrap_or_else(|p| p.into_inner());
    if !publication_is_current_state(&state, id, life, Some(&token)) {
        token.store(true, Ordering::Release);
        return Err(HarvestFactsFailure::StaleSource);
    }
    if !state.retained_harvest.hold(id, life, &token) {
        token.store(true, Ordering::Release);
        return Err(HarvestFactsFailure::QueryBudget);
    }
    Ok(token)
}

pub(crate) fn invalidate_before_write(
    command: &CatalogCommand,
    admission: &Arc<Mutex<AdmissionState>>,
) {
    let id = match command {
        CatalogCommand::ApplyCuration { source_id, .. }
        | CatalogCommand::ApplyCurationOperation { source_id, .. }
        | CatalogCommand::ApplyRatingDecay { source_id, .. }
        | CatalogCommand::RequestFeatureAnalysis { source_id, .. } => source_id,
        CatalogCommand::ApplyPreparedCollectionOperation { operation } => operation
            .group()
            .source_observation()
            .revision()
            .source_id(),
        // Audits and replay enter through begin_source_attempt before their writes.
        // Lifecycle replacement/removal/shutdown invalidate at admission, before dequeue.
        _ => return,
    };
    admission
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_harvest
        .cancel_source(id);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_mask_cap_replacement_and_foreign_generation_fail_closed_without_eviction() {
        let mut masks = RetainedMasks::default();
        let tokens = (0..MAX_RETAINED_SOURCES)
            .map(|i| {
                let id = SourceId::from_string(format!("source-{i}"));
                let token = Arc::new(AtomicBool::new(false));
                assert!(masks.hold(&id, 1, &token));
                (id, token)
            })
            .collect::<Vec<_>>();
        let overflow = Arc::new(AtomicBool::new(false));
        assert!(!masks.hold(&SourceId::from_string("overflow"), 1, &overflow));
        assert!(tokens.iter().all(|(_, t)| !t.load(Ordering::Acquire)));
        assert!(!masks.owns(&tokens[0].0, 2, &tokens[0].1));
        assert!(!masks.owns(&tokens[0].0, 1, &Arc::new(AtomicBool::new(false))));
        assert!(masks.hold(&tokens[0].0, 2, &overflow));
        assert!(tokens[0].1.load(Ordering::Acquire));
        assert!(!overflow.load(Ordering::Acquire));
        masks.cancel_all();
        assert!(overflow.load(Ordering::Acquire));
        assert!(tokens.iter().all(|(_, t)| t.load(Ordering::Acquire)));
        assert!(masks.0.is_empty());
    }
}
