//! Bounded asynchronous collection capture; UI never reads SQL or filesystem ownership.
use super::*;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use yield_session::{
    CatalogCollectionHistoryGroup, CollectionHistoryCancellation, CollectionHistoryFailure,
    CollectionHistoryGroupRequest,
};

pub(super) struct Preparation {
    pub(super) key: CurationLifecycleKey,
    plan: CurationOperationPlan,
    cancellation: CollectionHistoryCancellation,
    deadline: Instant,
    playback_history: bool,
}
impl Preparation {
    pub(super) fn cancel(self) {
        self.cancellation.cancel();
    }
}
pub(super) fn is_collection_plan(plan: &CurationOperationPlan) -> bool {
    !plan.batches.is_empty()
        && plan.batches.iter().flatten().all(|u| {
            matches!(
                u,
                SourceCurationUpdate::AddCollection { .. }
                    | SourceCurationUpdate::RemoveCollection { .. }
            )
        })
}
fn target(update: &SourceCurationUpdate) -> Option<(&RootRelativePath, SampleCollection, bool)> {
    match update {
        SourceCurationUpdate::AddCollection { path, collection } => Some((path, *collection, true)),
        SourceCurationUpdate::RemoveCollection { path, collection } => {
            Some((path, *collection, false))
        }
        _ => None,
    }
}
impl Browser {
    pub(in crate::browser) fn expire_collection_preparation(&mut self) -> bool {
        if self
            .curation_state
            .collection_preparation
            .as_ref()
            .is_some_and(|p| Instant::now() >= p.deadline || p.cancellation.is_cancelled())
        {
            let p = self.curation_state.collection_preparation.take().unwrap();
            self.curation_state.set_message(
                &p.key,
                "Collection preparation expired; no edit was submitted".into(),
            );
            p.cancel();
            return true;
        }
        false
    }
    pub(super) fn begin_collection_preparation(
        &mut self,
        key: &CurationLifecycleKey,
        mut plan: CurationOperationPlan,
        playback_history: bool,
        cx: &mut Context<Self>,
    ) -> Result<bool, CatalogSubmitError> {
        if self.curation_state.collection_preparation.is_some() {
            return Ok(false);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let Some(lineage) = self.curation_state.lineages.get(key) else {
            return Ok(false);
        };
        let SourceCurationOperationFence::ExactRevision(revision) = plan.fence else {
            plan.history_unavailable = true;
            return self.submit_planned_curation(key, plan, playback_history, cx);
        };
        if !lineage.pending.is_empty() || lineage.staged_latest.is_some() {
            plan.history_unavailable = true;
            return self.submit_planned_curation(key, plan, playback_history, cx);
        }
        let Some((_, slot, _)) = plan.batches.iter().flatten().next().and_then(target) else {
            return Ok(false);
        };
        let charge = plan
            .batches
            .iter()
            .flatten()
            .try_fold(64usize * 1024, |n, u| {
                let (p, col, _) = target(u)?;
                if col != slot {
                    return None;
                }
                n.checked_add(
                    path_byte_len(p.as_path())
                        .checked_mul(8)?
                        .checked_add(1024)?,
                )
            });
        // Charge all owned capacity before copying selection/presentation or request paths.
        let presentation_charge = if plan.history_direction.is_some() {
            0
        } else {
            let scroll = self.list_scroll_handle.0.borrow();
            let top = scroll.base_handle.logical_scroll_top().0;
            self.selection
                .selected()
                .iter()
                .chain(self.selection.anchor())
                .chain(self.browser_focus_override.iter())
                .chain(self.view_model.rows().get(top).map(|r| &r.id))
                .fold(
                    2048usize.saturating_add(
                        self.selection
                            .selected()
                            .capacity()
                            .saturating_mul(std::mem::size_of::<AudioFileId>() + 32),
                    ),
                    |n, id| n.saturating_add(128 + path_byte_len(id.path())),
                )
                .saturating_add(
                    self.selected_audio_path()
                        .map_or(0, |p| 128 + path_byte_len(p)),
                )
                .saturating_mul(3)
        };
        let estimate = charge.map(|c| {
            c.saturating_add(presentation_charge)
                .saturating_add(
                    plan.overlays
                        .keys()
                        .map(|p| 3usize.saturating_mul(256 + path_byte_len(p)))
                        .sum::<usize>(),
                )
                .saturating_add(16384)
        });
        if estimate.is_none_or(|n| {
            !self
                .curation_state
                .collection_history
                .reserve_preparation(n, plan.history_direction)
        }) {
            if plan.history_direction.is_some() {
                self.curation_state.set_message(
                    key,
                    "Collection history unavailable: bounded coverage exceeded".into(),
                );
                return Ok(false);
            }
            plan.history_unavailable = true;
            return self.submit_planned_curation(key, plan, playback_history, cx);
        }
        let presentation = self.capture_collection_history_presentation(key, &plan);
        if plan.history_direction.is_none() && presentation.is_none() {
            plan.history_unavailable = true;
            return self.submit_planned_curation(key, plan, playback_history, cx);
        }
        let targets = plan
            .batches
            .iter()
            .flatten()
            .map(|u| {
                let (p, _, after) = target(u).unwrap();
                (p.clone(), after)
            })
            .collect::<Vec<_>>();
        let request = match CollectionHistoryGroupRequest::new_mixed(
            key.source_id.clone(),
            key.lifecycle_generation,
            revision,
            plan.request_id,
            slot,
            &targets,
        ) {
            Ok(r) => r,
            Err(CollectionHistoryFailure::BudgetExceeded) => {
                if plan.history_direction.is_some() {
                    self.curation_state.set_message(
                        key,
                        "Collection history unavailable: bounded preparation exceeded".into(),
                    );
                    return Ok(false);
                }
                plan.history_unavailable = true;
                return self.submit_planned_curation(key, plan, playback_history, cx);
            }
            Err(_) => {
                self.curation_state.set_message(
                    key,
                    "Collection preparation invalid; no edit submitted".into(),
                );
                return Ok(false);
            }
        };
        let cancellation = CollectionHistoryCancellation::new();
        catalog::submit_catalog_command(
            yield_session::CatalogCommand::RequestCollectionHistoryGroup {
                request,
                cancellation: cancellation.clone(),
            },
            cx,
        )?;
        plan.captured_presentation = presentation;
        self.curation_state.collection_preparation = Some(Preparation {
            key: key.clone(),
            plan,
            cancellation,
            deadline,
            playback_history,
        });
        self.curation_state
            .set_message(key, "Preparing collection edit".into());
        self.start_source_registry_poll(cx);
        cx.notify();
        Ok(true)
    }
    pub(in crate::browser) fn apply_collection_preparation_event(
        &mut self,
        event: &yield_session::CatalogEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let yield_session::CatalogEvent::CollectionHistoryGroupCompleted {
            request,
            cancellation,
            result,
        } = event
        else {
            return false;
        };
        if !self
            .curation_state
            .collection_preparation
            .as_ref()
            .is_some_and(|p| {
                p.key.source_id == *request.source_id()
                    && p.key.lifecycle_generation == request.lifecycle_generation()
                    && p.plan.request_id == request.generation()
            })
        {
            return false;
        }
        if cancellation.is_cancelled() {
            return false;
        }
        let mut pending = self.curation_state.collection_preparation.take().unwrap();
        let result = (|| -> Result<Arc<CatalogCollectionHistoryGroup>, String> {
            if Instant::now() >= pending.deadline || pending.cancellation.is_cancelled() {
                return Err("Collection preparation expired; no edit submitted".into());
            }
            let group = result.as_ref().map_err(|e| {
                format!("Collection preparation unavailable: {e:?}; no edit submitted")
            })?;
            let source = group.source_observation();
            let revision = source.revision();
            let lineage = self
                .curation_state
                .lineages
                .get(&pending.key)
                .ok_or("Collection source retired")?;
            if self.collection_history_key().as_ref() != Some(&pending.key)
                || !lineage.pending.is_empty()
                || lineage.staged_latest.is_some()
                || (lineage.failed.is_some()
                    && !pending.plan.history_direction.is_some_and(|d| {
                        self.curation_state
                            .collection_history
                            .ready(&pending.key, lineage, d)
                    }))
                || !group.within_observation_window()
                || group.generation() != pending.plan.request_id
                || revision.source_id() != &pending.key.source_id
                || revision.lifecycle_generation() != pending.key.lifecycle_generation
                || revision.revision() != lineage.revision
                || revision.root_identity() != &pending.plan.root_identity
                || source.directory_generation() != lineage.directory_generation
            {
                return Err("Collection preparation changed; no edit submitted".into());
            }
            let frozen_targets = pending
                .plan
                .batches
                .iter()
                .flatten()
                .map(|u| {
                    let (p, col, t) = target(u).unwrap();
                    (p.as_path(), (col, t))
                })
                .collect::<BTreeMap<_, _>>();
            if request.revision() != revision.revision()
                || request.collection() != source.collection()
                || request.targets().len() != frozen_targets.len()
                || request.targets().iter().any(|(p, t)| {
                    frozen_targets.get(p.as_path()) != Some(&(source.collection(), *t))
                })
            {
                return Err(
                    "Collection preparation does not match frozen targets; no edit submitted"
                        .into(),
                );
            }
            let requested_targets = request
                .targets()
                .iter()
                .map(|(p, t)| (p.as_path(), *t))
                .collect::<BTreeMap<_, _>>();
            for change in source.changes() {
                let before = lineage
                    .confirmed
                    .get(change.path().as_path())
                    .ok_or("Collection membership unavailable")?;
                let requested = requested_targets
                    .get(change.path().as_path())
                    .ok_or("Unexpected collection path")?;
                if before.has_collection(usize::from(source.collection().index()))
                    != change.before()
                    || *requested != change.after()
                {
                    return Err("Collection membership changed; no edit submitted".into());
                }
            }
            if !self
                .curation_state
                .collection_history
                .owner_matches_direction(&pending.plan, group)
            {
                return Err("Collection history owner changed; refresh catalog".into());
            }
            Ok(Arc::clone(group))
        })();
        let group = match result {
            Ok(g) => g,
            Err(message) => {
                self.curation_state.set_message(&pending.key, message);
                pending.cancel();
                cx.notify();
                return true;
            }
        };
        if group.source_observation().changes().is_empty() {
            if pending.plan.history_direction.is_some() {
                if let Some(restore) = self
                    .curation_state
                    .collection_history
                    .complete_empty_direction(&pending.plan, &group)
                {
                    self.curation_state.next_request_id = pending.plan.request_id;
                    if let Some(lineage) = self.curation_state.lineages.get_mut(&pending.key) {
                        lineage.failed = None;
                    }
                    self.restore_collection_history_presentation(restore, cx);
                    self.curation_state.set_message(
                        &pending.key,
                        "Collection endpoint confirmed; no edit needed".into(),
                    );
                }
                pending.cancel();
                cx.notify();
                return true;
            }
            self.curation_state
                .set_message(&pending.key, "Collection unchanged".into());
            pending.cancel();
            cx.notify();
            return true;
        }
        pending.plan.batches = group
            .source_observation()
            .batches()
            .map(|chunk| {
                chunk
                    .iter()
                    .map(|c| {
                        if c.after() {
                            SourceCurationUpdate::AddCollection {
                                path: c.path().clone(),
                                collection: group.source_observation().collection(),
                            }
                        } else {
                            SourceCurationUpdate::RemoveCollection {
                                path: c.path().clone(),
                                collection: group.source_observation().collection(),
                            }
                        }
                    })
                    .collect()
            })
            .collect();
        let changed = group
            .source_observation()
            .changes()
            .iter()
            .map(|c| c.path().as_path())
            .collect::<BTreeSet<_>>();
        pending
            .plan
            .overlays
            .retain(|path, _| changed.contains(path.as_path()));
        drop(changed);
        pending.plan.prepared_group = Some(group);
        // Successful submission moves genuine capture custody to the worker. Do not cancel it
        // on dropping this UI capsule; private owner handoff must happen first.
        let key = pending.key.clone();
        let cancel = pending.cancellation.clone();
        if let Err(error) =
            self.submit_planned_curation(&key, pending.plan, pending.playback_history, cx)
        {
            cancel.cancel();
            self.curation_state
                .set_message(&key, format!("Collection not saved: {error}"));
        }
        cx.notify();
        true
    }
}
