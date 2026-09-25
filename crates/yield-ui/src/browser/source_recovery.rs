//! Bounded, background-only recovery for a source root whose catalog authority went stale.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::Context;
use yield_library::sample_sources::{SourceId, reconciliation::RootIdentity};
use yield_session::{
    CatalogError, CatalogEvent, CatalogOperation, CatalogStatus, VerifiedSourceRoot,
    resolve_source_root,
};

use super::curation::CurationLifecycleKey;
use super::{Browser, catalog};

const MISSING_RETRY: Duration = Duration::from_secs(30);
const PATH_CHANGED_RETRY: Duration = Duration::from_secs(30);
const OPERATION_RETRY: Duration = Duration::from_secs(5);

/// The result of comparing one background root probe with the durable source context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeDecision {
    Missing,
    PathChanged,
    SameIdentity,
    Rebind,
}

/// The next safe action after a root probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryAction {
    Retry,
    AuditSameGeneration,
    WaitForCuration,
    RegisterVerified,
}

#[derive(Clone, Debug)]
enum RecoveryPhase {
    Probing,
    WaitingRetry { due: Instant },
    WaitingCurationDrain { proof: VerifiedSourceRoot },
    WaitingRegistration { identity: RootIdentity },
    WaitingAudit { identity: RootIdentity },
}

#[derive(Clone, Debug)]
struct RecoveryIntent {
    source_id: SourceId,
    lifecycle_generation: u64,
    root: PathBuf,
    prior_identity: Option<RootIdentity>,
    registered: bool,
    registration_admitted: bool,
    force_reload: bool,
    fallback_to_scan: bool,
    probe_token: u64,
    phase: RecoveryPhase,
}

/// Browser-owned source-root recovery state.
pub(super) struct SourceRecoveryState {
    intent: Option<RecoveryIntent>,
    probe_task: Option<gpui::Task<()>>,
    next_probe_token: u64,
}

impl Default for SourceRecoveryState {
    fn default() -> Self {
        Self {
            intent: None,
            probe_task: None,
            next_probe_token: 0,
        }
    }
}

/// Whether the catalog event should continue through the ordinary browser event reducer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CatalogEventDisposition {
    /// Continue with the ordinary catalog event reducer.
    Pass,
    /// Consume the event because recovery still owns the source authority transition.
    Consume,
}

/// Cancel an in-flight probe and forget recovery state when the source lifecycle changes.
pub(super) fn cancel_for_source_change(browser: &mut Browser) {
    retire_after_audit(&mut browser.source_recovery);
}

/// Retain command admission separately from the worker's later Registered event.
pub(super) fn note_registration_admitted(
    browser: &mut Browser,
    source_id: &SourceId,
    lifecycle_generation: u64,
) {
    if let Some(intent) = browser.source_recovery.intent.as_mut()
        && intent.source_id == *source_id
        && intent.lifecycle_generation == lifecycle_generation
    {
        intent.registration_admitted = true;
    }
}

/// Run one bounded recovery state-machine step from the browser poll lane.
pub(super) fn poll(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let Some(intent) = browser.source_recovery.intent.as_ref() else {
        return false;
    };
    let action = match &intent.phase {
        RecoveryPhase::WaitingRetry { due } if *due <= Instant::now() => {
            Some(RecoveryAction::Retry)
        }
        RecoveryPhase::WaitingCurationDrain { .. }
            if !has_pending_old_curation(browser, intent) =>
        {
            Some(RecoveryAction::RegisterVerified)
        }
        _ => None,
    };
    match action {
        Some(RecoveryAction::Retry) => start_probe(browser, cx),
        Some(RecoveryAction::RegisterVerified) => begin_verified_rebind(browser, cx),
        _ => false,
    }
}

/// Begin a safe manual refresh without removing the currently registered source handle.
pub(super) fn manual_refresh(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let Some(activation) = browser.catalog_activation.clone() else {
        return false;
    };
    let Some(index) = browser.active_source.filter(|index| {
        browser.source_sessions.get(*index).is_some_and(|session| {
            session.source_id.as_ref() == Some(&activation.source_id)
                && session.root == activation.root
        })
    }) else {
        return false;
    };

    let same_lifecycle = browser
        .source_recovery
        .intent
        .as_ref()
        .is_some_and(|intent| {
            intent.source_id == activation.source_id
                && intent.lifecycle_generation == activation.lifecycle_generation
                && intent.root == activation.root
        });
    if same_lifecycle {
        let restart_probe = browser
            .source_recovery
            .intent
            .as_mut()
            .is_some_and(|intent| coalesce_manual_refresh(intent, Instant::now()));
        if let Some(current) = browser.catalog_activation.as_mut() {
            current.force_reload = true;
        }
        if restart_probe {
            let _ = start_probe(browser, cx);
        }
        return true;
    }

    browser.cancel_catalog_projection();
    browser.catalog_pending_command = None;
    super::tags::cancel_for_source_change(browser);
    super::tag_chips::cancel_for_source_change(browser);
    super::tag_completion::cancel_for_source_change(browser, cx);
    super::tag_filter::cancel_for_source_change(browser);
    browser.reset_catalog_metadata();
    mark_authority_unavailable(browser, index, "Refreshing source catalog…".to_string());

    let intent = RecoveryIntent {
        source_id: activation.source_id,
        lifecycle_generation: activation.lifecycle_generation,
        root: activation.root,
        prior_identity: activation.root_identity,
        registered: activation.registered,
        registration_admitted: activation.registration_admitted,
        force_reload: true,
        fallback_to_scan: activation.fallback_to_scan,
        probe_token: 0,
        phase: RecoveryPhase::WaitingRetry {
            due: Instant::now(),
        },
    };
    browser.source_recovery.intent = Some(intent);
    start_probe(browser, cx);
    cx.notify();
    true
}

/// Intercept stale-root events and recovery-owned snapshots before normal catalog projection.
pub(super) fn before_catalog_event(
    browser: &mut Browser,
    event: &CatalogEvent,
    cx: &mut Context<Browser>,
) -> CatalogEventDisposition {
    match event {
        CatalogEvent::Failed {
            source_id: Some(source_id),
            lifecycle_generation: Some(lifecycle_generation),
            operation,
            error,
        } if is_recovery_operation(operation) && is_root_failure(error) => {
            if begin_root_recovery(browser, source_id, *lifecycle_generation, error, cx) {
                CatalogEventDisposition::Consume
            } else {
                CatalogEventDisposition::Pass
            }
        }
        CatalogEvent::Failed {
            source_id: Some(source_id),
            lifecycle_generation: Some(lifecycle_generation),
            operation,
            error,
        } if recovery_event_matches(browser, source_id, *lifecycle_generation)
            && is_register_operation(operation) =>
        {
            registration_failure(browser, *lifecycle_generation, error);
            CatalogEventDisposition::Consume
        }
        CatalogEvent::Failed {
            source_id: Some(source_id),
            lifecycle_generation: Some(lifecycle_generation),
            operation,
            error,
        } if recovery_event_matches(browser, source_id, *lifecycle_generation)
            && is_audit_operation(operation) =>
        {
            audit_failure(browser, error);
            CatalogEventDisposition::Consume
        }
        CatalogEvent::Registered {
            source_id,
            lifecycle_generation,
        } if recovery_event_matches(browser, source_id, *lifecycle_generation) => {
            let Some(intent) = browser.source_recovery.intent.as_mut() else {
                return CatalogEventDisposition::Pass;
            };
            if let RecoveryPhase::WaitingRegistration { identity } = &intent.phase {
                let identity = identity.clone();
                intent.phase = RecoveryPhase::WaitingAudit { identity };
                CatalogEventDisposition::Pass
            } else {
                CatalogEventDisposition::Consume
            }
        }
        CatalogEvent::SnapshotReady { snapshot }
            if recovery_event_matches(
                browser,
                &snapshot.source_id,
                snapshot.lifecycle_generation,
            ) =>
        {
            CatalogEventDisposition::Consume
        }
        CatalogEvent::AuditCommitted { snapshot }
            if recovery_event_matches(
                browser,
                &snapshot.source_id,
                snapshot.lifecycle_generation,
            ) =>
        {
            before_audit_committed(browser, snapshot.root_identity.clone())
        }
        _ => CatalogEventDisposition::Pass,
    }
}

fn before_audit_committed(
    browser: &mut Browser,
    observed_identity: RootIdentity,
) -> CatalogEventDisposition {
    let Some(intent) = browser.source_recovery.intent.as_mut() else {
        return CatalogEventDisposition::Pass;
    };
    if !audit_identity_allows_completion(intent, &observed_identity) {
        let message = String::from("Source root identity changed while auditing");
        intent.phase = RecoveryPhase::WaitingRetry {
            due: Instant::now(),
        };
        set_scan_status(browser, message);
        return CatalogEventDisposition::Consume;
    }
    browser.source_recovery.probe_task.take();
    browser.source_recovery.intent = None;
    if let Some(index) = browser.active_source
        && let Some(session) = browser.source_sessions.get_mut(index)
    {
        session.last_error = None;
    }
    CatalogEventDisposition::Pass
}

fn begin_root_recovery(
    browser: &mut Browser,
    source_id: &SourceId,
    lifecycle_generation: u64,
    error: &CatalogError,
    cx: &mut Context<Browser>,
) -> bool {
    let Some(activation) = browser.catalog_activation.clone() else {
        return false;
    };
    if activation.source_id != *source_id || activation.lifecycle_generation != lifecycle_generation
    {
        return false;
    }
    let Some(index) = browser.active_source.filter(|index| {
        browser.source_sessions.get(*index).is_some_and(|session| {
            session.source_id.as_ref() == Some(source_id) && session.root == activation.root
        })
    }) else {
        return false;
    };

    browser.cancel_catalog_projection();
    browser.catalog_pending_command = None;
    super::tags::cancel_for_source_change(browser);
    super::tag_chips::cancel_for_source_change(browser);
    super::tag_completion::cancel_for_source_change(browser, cx);
    super::tag_filter::cancel_for_source_change(browser);
    browser.reset_catalog_metadata();
    if let Some(current) = browser.catalog_activation.as_mut() {
        current.audit_finished = true;
        current.audit_error = Some(error.to_string());
    }
    mark_authority_error(browser, index, error.to_string());

    let now = Instant::now();
    let same_intent = browser
        .source_recovery
        .intent
        .as_ref()
        .is_some_and(|intent| {
            intent.source_id == activation.source_id
                && intent.lifecycle_generation == activation.lifecycle_generation
                && intent.root == activation.root
        });
    if let Some(intent) = browser
        .source_recovery
        .intent
        .as_mut()
        .filter(|_| same_intent)
    {
        intent.registered = browser
            .catalog_activation
            .as_ref()
            .is_some_and(|current| current.registered);
        intent.registration_admitted = activation.registration_admitted;
        if intent.prior_identity.is_none() {
            intent.prior_identity = activation.root_identity.clone();
        }
        intent.force_reload |= activation.force_reload;
        intent.fallback_to_scan |= activation.fallback_to_scan;
        if !matches!(intent.phase, RecoveryPhase::Probing) {
            let retry_due = matches!(
                intent.phase,
                RecoveryPhase::WaitingRetry { due } if due > now
            );
            if !retry_due {
                intent.phase = RecoveryPhase::WaitingRetry { due: now };
            }
        }
    } else {
        browser.source_recovery.intent = Some(RecoveryIntent {
            source_id: activation.source_id,
            lifecycle_generation: activation.lifecycle_generation,
            root: activation.root,
            prior_identity: activation.root_identity,
            registered: browser
                .catalog_activation
                .as_ref()
                .is_some_and(|current| current.registered),
            registration_admitted: activation.registration_admitted,
            force_reload: activation.force_reload,
            fallback_to_scan: activation.fallback_to_scan,
            probe_token: 0,
            phase: RecoveryPhase::WaitingRetry { due: now },
        });
    }
    set_scan_status(browser, "Source unavailable; checking root…".to_string());
    let _ = start_probe(browser, cx);
    true
}

fn registration_failure(browser: &mut Browser, lifecycle_generation: u64, error: &CatalogError) {
    browser.catalog_pending_command = None;
    browser.cancel_catalog_projection();
    if let Some(activation) = browser.catalog_activation.as_mut()
        && activation.lifecycle_generation == lifecycle_generation
    {
        activation.registered = false;
        activation.audit_finished = true;
        activation.audit_error = Some(error.to_string());
    }
    if let Some(intent) = browser.source_recovery.intent.as_mut() {
        intent.registered = false;
        intent.registration_admitted = browser
            .catalog_activation
            .as_ref()
            .is_some_and(|activation| activation.registration_admitted);
        intent.phase = RecoveryPhase::WaitingRetry {
            due: Instant::now() + OPERATION_RETRY,
        };
    }
    set_scan_status(
        browser,
        format!("Verified source registration failed; retrying: {error}"),
    );
}

fn audit_failure(browser: &mut Browser, error: &CatalogError) {
    browser.catalog_pending_command = None;
    if let Some(activation) = browser.catalog_activation.as_mut() {
        activation.audit_finished = true;
        activation.audit_error = Some(error.to_string());
    }
    if audit_failure_is_retryable(error) {
        if let Some(intent) = browser.source_recovery.intent.as_mut() {
            intent.phase = RecoveryPhase::WaitingRetry {
                due: Instant::now() + OPERATION_RETRY,
            };
        }
        set_scan_status(browser, format!("Catalog audit failed; retrying: {error}"));
    } else {
        // Identity and database-integrity uncertainty require an explicit audit or operator
        // action. Dropping the recovery intent prevents an automatic retry loop or root reopen.
        browser.source_recovery.intent = None;
        set_scan_status(browser, format!("Catalog audit paused; {error}"));
    }
}

fn start_probe(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let Some(intent) = browser.source_recovery.intent.as_mut() else {
        return false;
    };
    if browser.source_recovery.probe_task.is_some() {
        return false;
    }
    if let RecoveryPhase::WaitingRetry { due } = intent.phase
        && due > Instant::now()
    {
        return false;
    }
    browser.source_recovery.next_probe_token = browser
        .source_recovery
        .next_probe_token
        .wrapping_add(1)
        .max(1);
    let token = browser.source_recovery.next_probe_token;
    let source_id = intent.source_id.clone();
    let lifecycle_generation = intent.lifecycle_generation;
    let root = intent.root.clone();
    intent.probe_token = token;
    intent.phase = RecoveryPhase::Probing;
    let task = cx.spawn(async move |this, cx| {
        let result = cx
            .background_executor()
            .spawn(async move { resolve_source_root(&root) })
            .await;
        let _ = this.update(cx, |browser, cx| {
            if apply_probe_result(browser, source_id, lifecycle_generation, token, result, cx) {
                cx.notify();
            }
        });
    });
    browser.source_recovery.probe_task = Some(task);
    false
}

fn apply_probe_result(
    browser: &mut Browser,
    source_id: SourceId,
    lifecycle_generation: u64,
    token: u64,
    result: Result<VerifiedSourceRoot, CatalogError>,
    cx: &mut Context<Browser>,
) -> bool {
    let matches = browser
        .source_recovery
        .intent
        .as_ref()
        .is_some_and(|intent| {
            probe_context_matches(intent, &source_id, lifecycle_generation, token)
                && probe_matches_active_context(browser, intent)
        });
    if !matches {
        return false;
    }
    browser.source_recovery.probe_task = None;
    let Some(intent) = browser.source_recovery.intent.as_ref() else {
        return false;
    };
    let durable_root = intent.root.clone();
    let prior_identity = intent.prior_identity.clone();
    let registered = intent.registered;
    let decision = classify_probe_result(
        result.as_ref().map_err(|_| ()),
        &durable_root,
        prior_identity.as_ref(),
        registered,
    );
    let pending_curation = browser
        .source_recovery
        .intent
        .as_ref()
        .is_some_and(|intent| has_pending_old_curation(browser, intent));
    let action = recovery_action_for(decision, pending_curation);
    match action {
        RecoveryAction::Retry => {
            let message = result
                .err()
                .map(|error| format!("Source unavailable: {error}"))
                .unwrap_or_else(|| match decision {
                    ProbeDecision::PathChanged => {
                        String::from("Source root target changed; add the target as a new source")
                    }
                    _ => String::from("Source unavailable"),
                });
            let delay = if decision == ProbeDecision::PathChanged {
                PATH_CHANGED_RETRY
            } else {
                MISSING_RETRY
            };
            set_retry(browser, delay, message)
        }
        RecoveryAction::AuditSameGeneration => {
            let identity = result
                .ok()
                .expect("same-identity probe decision has a verified root")
                .root_identity()
                .clone();
            request_same_generation_audit(browser, identity)
        }
        RecoveryAction::WaitForCuration | RecoveryAction::RegisterVerified => {
            let Ok(proof) = result else {
                return false;
            };
            if let Some(intent) = browser.source_recovery.intent.as_mut() {
                intent.phase = RecoveryPhase::WaitingCurationDrain { proof };
            }
            let mut changed = set_scan_status(
                browser,
                "Source root changed; waiting for pending curation…".to_string(),
            );
            if !has_pending_old_curation(
                browser,
                browser
                    .source_recovery
                    .intent
                    .as_ref()
                    .expect("recovery intent retained after probe"),
            ) && action == RecoveryAction::RegisterVerified
            {
                changed |= begin_verified_rebind(browser, cx);
            }
            changed
        }
    }
}

fn request_same_generation_audit(browser: &mut Browser, identity: RootIdentity) -> bool {
    let Some(intent) = browser.source_recovery.intent.as_mut() else {
        return false;
    };
    let Some(activation) = browser.catalog_activation.as_mut() else {
        return false;
    };
    if activation.source_id != intent.source_id
        || activation.lifecycle_generation != intent.lifecycle_generation
    {
        return false;
    }
    activation.registered = true;
    activation.audit_finished = false;
    activation.audit_error = None;
    activation.root_identity = Some(identity.clone());
    activation.force_reload |= intent.force_reload;
    intent.phase = RecoveryPhase::WaitingAudit { identity };
    let audit = CatalogCommandIntent::Audit {
        source_id: intent.source_id.clone(),
        lifecycle_generation: intent.lifecycle_generation,
    };
    if browser.catalog_pending_command.as_ref() != Some(&audit) {
        browser.catalog_pending_command = Some(audit);
    }
    clear_source_error(browser);
    set_scan_status(browser, "Source restored; auditing catalog…".to_string())
}

fn begin_verified_rebind(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let Some(intent) = browser.source_recovery.intent.as_ref() else {
        return false;
    };
    let RecoveryPhase::WaitingCurationDrain { proof } = &intent.phase else {
        return false;
    };
    let source_id = intent.source_id.clone();
    let old_generation = intent.lifecycle_generation;
    let root = intent.root.clone();
    let force_reload = intent.force_reload;
    let fallback_to_scan = intent.fallback_to_scan;
    let proof = proof.clone();
    let Some(index) = browser.active_source.filter(|index| {
        browser.source_sessions.get(*index).is_some_and(|session| {
            session.source_id.as_ref() == Some(&source_id) && session.root == root
        })
    }) else {
        cancel_for_source_change(browser);
        return false;
    };
    let key = CurationLifecycleKey {
        source_id: source_id.clone(),
        lifecycle_generation: old_generation,
    };
    if browser.curation_state.has_pending_for_lifecycle(&key) {
        return false;
    }
    if intent.registration_admitted
        && !browser
            .catalog_activation
            .as_ref()
            .is_some_and(|activation| activation.registration_admitted)
    {
        return false;
    }
    if proof.canonical_root() != root {
        set_retry(
            browser,
            PATH_CHANGED_RETRY,
            String::from("Source root target changed; add the target as a new source"),
        );
        return false;
    }
    if !matches!(
        catalog::catalog_status(cx),
        Some(CatalogStatus::Loading | CatalogStatus::Ready)
    ) {
        return false;
    }

    // The old handle is removed only after the pending lineage has drained. The proof is kept in
    // local state while the ordinary cleanup queues that old generation's Remove command.
    browser.deactivate_catalog_source(cx);
    let started = browser.begin_catalog_activation(index, force_reload, true, fallback_to_scan, cx);
    if !started {
        return false;
    }
    let Some(activation) = browser.catalog_activation.as_mut() else {
        return false;
    };
    let lifecycle_generation = activation.lifecycle_generation;
    activation.root_identity = Some(proof.root_identity().clone());
    activation.registered = false;
    activation.registration_admitted = false;
    activation.audit_finished = false;
    activation.audit_error = None;
    browser.catalog_pending_command = Some(CatalogCommandIntent::RegisterVerified {
        source_id: source_id.clone(),
        root: proof.clone(),
        lifecycle_generation,
    });
    browser.source_recovery.intent = Some(RecoveryIntent {
        source_id,
        lifecycle_generation,
        root,
        prior_identity: Some(proof.root_identity().clone()),
        registered: false,
        registration_admitted: false,
        force_reload,
        fallback_to_scan,
        probe_token: 0,
        phase: RecoveryPhase::WaitingRegistration {
            identity: proof.root_identity().clone(),
        },
    });
    set_scan_status(browser, "Registering verified source root…".to_string())
}

fn has_pending_old_curation(browser: &Browser, intent: &RecoveryIntent) -> bool {
    browser
        .curation_state
        .has_pending_for_lifecycle(&CurationLifecycleKey {
            source_id: intent.source_id.clone(),
            lifecycle_generation: intent.lifecycle_generation,
        })
}

fn set_retry(browser: &mut Browser, delay: Duration, message: String) -> bool {
    if let Some(intent) = browser.source_recovery.intent.as_mut() {
        intent.phase = RecoveryPhase::WaitingRetry {
            due: Instant::now() + delay,
        };
    }
    set_scan_status(browser, message)
}

fn mark_authority_unavailable(browser: &mut Browser, index: usize, message: String) {
    if let Some(activation) = browser.catalog_activation.as_mut() {
        activation.audit_finished = true;
        activation.audit_error = Some(message.clone());
    }
    if let Some(session) = browser.source_sessions.get_mut(index) {
        session.last_error = Some(message.clone());
    }
    set_scan_status(browser, message);
}

fn mark_authority_error(browser: &mut Browser, index: usize, message: String) {
    if let Some(session) = browser.source_sessions.get_mut(index)
        && session.last_error.as_deref() != Some(message.as_str())
    {
        session.last_error = Some(message.clone());
    }
    set_scan_status(browser, format!("Catalog root unavailable: {message}"));
}

fn clear_source_error(browser: &mut Browser) {
    if let Some(index) = browser.active_source
        && let Some(session) = browser.source_sessions.get_mut(index)
    {
        session.last_error = None;
    }
}

fn set_scan_status(browser: &mut Browser, message: String) -> bool {
    if !status_changed(&browser.scan_status, &message) {
        return false;
    }
    browser.set_scan_status(message);
    true
}

fn is_root_failure(error: &CatalogError) -> bool {
    matches!(
        error,
        CatalogError::StaleRootGeneration { .. }
            | CatalogError::InvalidRoot(_)
            | CatalogError::SourceUnavailable(_)
    )
}

fn audit_failure_is_retryable(error: &CatalogError) -> bool {
    !matches!(
        error,
        CatalogError::ProfileUnavailable(_)
            | CatalogError::SourceUnavailable(_)
            | CatalogError::SourceIdentityUncertain(_)
            | CatalogError::DatabaseIntegrityUncertain(_)
            | CatalogError::StaleRootGeneration { .. }
            | CatalogError::InvalidRoot(_)
    )
}

/// Identify a registration failure from the catalog worker's typed operation.
pub(super) fn is_register_operation(operation: &CatalogOperation) -> bool {
    matches!(operation, CatalogOperation::Register)
}

fn is_audit_operation(operation: &CatalogOperation) -> bool {
    matches!(operation, CatalogOperation::Audit)
}

fn is_recovery_operation(operation: &CatalogOperation) -> bool {
    matches!(
        operation,
        CatalogOperation::Register | CatalogOperation::Audit | CatalogOperation::ReadSnapshot
    )
}

fn recovery_event_matches(
    browser: &Browser,
    source_id: &SourceId,
    lifecycle_generation: u64,
) -> bool {
    browser
        .source_recovery
        .intent
        .as_ref()
        .is_some_and(|intent| {
            intent.source_id == *source_id
                && intent.lifecycle_generation == lifecycle_generation
                && browser
                    .catalog_activation
                    .as_ref()
                    .is_some_and(|activation| {
                        activation.source_id == *source_id
                            && activation.lifecycle_generation == lifecycle_generation
                    })
        })
}

fn probe_matches_active_context(browser: &Browser, intent: &RecoveryIntent) -> bool {
    browser
        .catalog_activation
        .as_ref()
        .is_some_and(|activation| {
            activation.source_id == intent.source_id
                && activation.lifecycle_generation == intent.lifecycle_generation
                && activation.root == intent.root
        })
        && browser.active_source.is_some_and(|index| {
            browser.source_sessions.get(index).is_some_and(|session| {
                session.source_id.as_ref() == Some(&intent.source_id) && session.root == intent.root
            })
        })
}

fn audit_identity_allows_completion(intent: &RecoveryIntent, observed: &RootIdentity) -> bool {
    let accepts_watcher_audit = matches!(
        intent.phase,
        RecoveryPhase::Probing | RecoveryPhase::WaitingRetry { .. }
    ) && intent.registered
        && intent
            .prior_identity
            .as_ref()
            .is_some_and(|identity| identity == observed);
    if accepts_watcher_audit {
        return true;
    }
    match &intent.phase {
        RecoveryPhase::WaitingAudit { identity }
        | RecoveryPhase::WaitingRegistration { identity } => identity == observed,
        RecoveryPhase::Probing
        | RecoveryPhase::WaitingRetry { .. }
        | RecoveryPhase::WaitingCurationDrain { .. } => false,
    }
}

fn probe_context_matches(
    intent: &RecoveryIntent,
    source_id: &SourceId,
    lifecycle_generation: u64,
    token: u64,
) -> bool {
    intent.source_id == *source_id
        && intent.lifecycle_generation == lifecycle_generation
        && intent.probe_token == token
}

fn classify_probe_result(
    result: Result<&VerifiedSourceRoot, ()>,
    durable_root: &Path,
    prior_identity: Option<&RootIdentity>,
    registered: bool,
) -> ProbeDecision {
    let Ok(proof) = result else {
        return ProbeDecision::Missing;
    };
    if proof.canonical_root() != durable_root {
        return ProbeDecision::PathChanged;
    }
    if registered && prior_identity.is_some_and(|identity| identity == proof.root_identity()) {
        ProbeDecision::SameIdentity
    } else {
        ProbeDecision::Rebind
    }
}

fn recovery_action_for(decision: ProbeDecision, pending_curation: bool) -> RecoveryAction {
    match decision {
        ProbeDecision::Missing | ProbeDecision::PathChanged => RecoveryAction::Retry,
        ProbeDecision::SameIdentity => RecoveryAction::AuditSameGeneration,
        ProbeDecision::Rebind if pending_curation => RecoveryAction::WaitForCuration,
        ProbeDecision::Rebind => RecoveryAction::RegisterVerified,
    }
}

fn status_changed(previous: &str, next: &str) -> bool {
    previous != next
}

/// Coalesce a manual refresh into an existing recovery intent. The probe token and active phase
/// remain correlated; a retry that was waiting becomes immediately eligible without starting a
/// second filesystem probe.
fn coalesce_manual_refresh(intent: &mut RecoveryIntent, now: Instant) -> bool {
    intent.force_reload = true;
    if let RecoveryPhase::WaitingRetry { due } = &mut intent.phase {
        *due = now;
        true
    } else {
        false
    }
}

/// Retire all recovery correlation after an authoritative watcher audit is accepted.
fn retire_after_audit(state: &mut SourceRecoveryState) {
    state.probe_task.take();
    state.intent = None;
}

use super::catalog::CatalogCommandIntent;

#[cfg(test)]
mod tests {
    use super::{
        ProbeDecision, RecoveryAction, RecoveryIntent, RecoveryPhase, SourceRecoveryState,
        audit_failure_is_retryable, audit_identity_allows_completion, classify_probe_result,
        coalesce_manual_refresh, is_root_failure, probe_context_matches, recovery_action_for,
        retire_after_audit, status_changed,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;
    use yield_library::sample_sources::{SourceId, reconciliation::RootIdentity};
    use yield_session::CatalogError;
    use yield_session::resolve_source_root;

    #[test]
    fn missing_then_same_identity_reuses_generation_only_after_registration() {
        let directory = tempdir().expect("temp root");
        let proof = resolve_source_root(directory.path()).expect("verified root");
        assert_eq!(
            classify_probe_result(
                Err(()),
                proof.canonical_root(),
                Some(proof.root_identity()),
                true,
            ),
            ProbeDecision::Missing
        );
        assert_eq!(
            classify_probe_result(
                Ok(&proof),
                proof.canonical_root(),
                Some(proof.root_identity()),
                true,
            ),
            ProbeDecision::SameIdentity
        );
        assert_eq!(
            classify_probe_result(
                Ok(&proof),
                proof.canonical_root(),
                Some(proof.root_identity()),
                false,
            ),
            ProbeDecision::Rebind
        );
    }

    #[test]
    fn physical_replacement_requires_verified_rebind() {
        let directory = tempdir().expect("temp root");
        let first_path = directory.path().join("first");
        let second_path = directory.path().join("second");
        fs::create_dir(&first_path).expect("first root");
        fs::create_dir(&second_path).expect("second root");
        let first = resolve_source_root(&first_path).expect("first proof");
        let second = resolve_source_root(&second_path).expect("second proof");
        assert_ne!(first.root_identity(), second.root_identity());
        assert_eq!(
            classify_probe_result(
                Ok(&second),
                second.canonical_root(),
                Some(first.root_identity()),
                true,
            ),
            ProbeDecision::Rebind
        );
    }

    #[cfg(unix)]
    #[test]
    fn changed_canonical_target_stays_unavailable() {
        let directory = tempdir().expect("temp root");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let link = directory.path().join("link");
        fs::create_dir(&first).expect("first root");
        fs::create_dir(&second).expect("second root");
        std::os::unix::fs::symlink(&first, &link).expect("source symlink");
        let proof = resolve_source_root(&link).expect("first link proof");
        let second_proof = resolve_source_root(&second).expect("second proof");
        assert_eq!(
            classify_probe_result(
                Ok(&second_proof),
                proof.canonical_root(),
                Some(proof.root_identity()),
                true
            ),
            ProbeDecision::PathChanged
        );
    }

    #[test]
    fn stale_probe_token_or_source_context_is_not_reusable() {
        let root = PathBuf::from("/library");
        let source_id = SourceId::from_string("source");
        let intent = RecoveryIntent {
            source_id: source_id.clone(),
            lifecycle_generation: 7,
            root: root.clone(),
            prior_identity: Some(RootIdentity::from_bytes(b"first".to_vec())),
            registered: true,
            registration_admitted: true,
            force_reload: false,
            fallback_to_scan: false,
            probe_token: 4,
            phase: RecoveryPhase::Probing,
        };
        assert!(probe_context_matches(&intent, &source_id, 7, 4));
        assert!(!probe_context_matches(&intent, &source_id, 7, 3));
        assert!(!probe_context_matches(
            &intent,
            &SourceId::from_string("other"),
            7,
            4
        ));
        let first = intent.prior_identity.as_ref().expect("prior identity");
        let second = RootIdentity::from_bytes(b"second".to_vec());
        assert_ne!(first, &second);
        assert!(audit_identity_allows_completion(&intent, first));
        assert!(!audit_identity_allows_completion(&intent, &second));
        assert_eq!(
            recovery_action_for(ProbeDecision::Rebind, true),
            RecoveryAction::WaitForCuration
        );
        assert_eq!(
            recovery_action_for(ProbeDecision::Rebind, false),
            RecoveryAction::RegisterVerified
        );
    }

    #[test]
    fn matching_watcher_audit_wins_pending_probe_but_rebind_waits_for_new_identity() {
        let root = PathBuf::from("/library");
        let source_id = SourceId::from_string("source");
        let identity = RootIdentity::from_bytes(b"identity".to_vec());
        let mut intent = RecoveryIntent {
            source_id,
            lifecycle_generation: 7,
            root,
            prior_identity: Some(identity.clone()),
            registered: true,
            registration_admitted: true,
            force_reload: false,
            fallback_to_scan: false,
            probe_token: 4,
            phase: RecoveryPhase::Probing,
        };
        assert!(audit_identity_allows_completion(&intent, &identity));
        intent.phase = RecoveryPhase::WaitingAudit {
            identity: RootIdentity::from_bytes(b"replacement".to_vec()),
        };
        assert!(!audit_identity_allows_completion(&intent, &identity));
    }

    #[test]
    fn manual_refresh_coalesces_probe_and_retry_phases() {
        let source_id = SourceId::from_string("source");
        let root = PathBuf::from("/library");
        let now = Instant::now();
        let mut probing = RecoveryIntent {
            source_id: source_id.clone(),
            lifecycle_generation: 7,
            root: root.clone(),
            prior_identity: None,
            registered: true,
            registration_admitted: true,
            force_reload: false,
            fallback_to_scan: false,
            probe_token: 11,
            phase: RecoveryPhase::Probing,
        };
        assert!(!coalesce_manual_refresh(&mut probing, now));
        assert!(probing.force_reload);
        assert_eq!(probing.probe_token, 11);
        assert!(matches!(probing.phase, RecoveryPhase::Probing));

        let mut waiting_registration = RecoveryIntent {
            source_id: source_id.clone(),
            lifecycle_generation: 8,
            root: root.clone(),
            prior_identity: None,
            registered: false,
            registration_admitted: true,
            force_reload: false,
            fallback_to_scan: false,
            probe_token: 12,
            phase: RecoveryPhase::WaitingRegistration {
                identity: RootIdentity::from_bytes(b"verified".to_vec()),
            },
        };
        assert!(!coalesce_manual_refresh(&mut waiting_registration, now));
        assert!(waiting_registration.force_reload);
        assert_eq!(waiting_registration.probe_token, 12);
        assert!(matches!(
            waiting_registration.phase,
            RecoveryPhase::WaitingRegistration { .. }
        ));

        let mut retrying = RecoveryIntent {
            source_id,
            lifecycle_generation: 9,
            root,
            prior_identity: None,
            registered: false,
            registration_admitted: false,
            force_reload: false,
            fallback_to_scan: false,
            probe_token: 13,
            phase: RecoveryPhase::WaitingRetry {
                due: now + Duration::from_secs(30),
            },
        };
        assert!(coalesce_manual_refresh(&mut retrying, now));
        assert!(retrying.force_reload);
        assert_eq!(retrying.probe_token, 13);
        assert!(matches!(
            retrying.phase,
            RecoveryPhase::WaitingRetry { due } if due <= now
        ));
    }

    #[test]
    fn accepted_watcher_audit_retires_probe_correlation() {
        let source_id = SourceId::from_string("source");
        let identity = RootIdentity::from_bytes(b"identity".to_vec());
        let intent = RecoveryIntent {
            source_id: source_id.clone(),
            lifecycle_generation: 7,
            root: PathBuf::from("/library"),
            prior_identity: Some(identity.clone()),
            registered: true,
            registration_admitted: true,
            force_reload: false,
            fallback_to_scan: false,
            probe_token: 14,
            phase: RecoveryPhase::Probing,
        };
        let mut state = SourceRecoveryState {
            intent: Some(intent),
            probe_task: None,
            next_probe_token: 14,
        };
        let old_intent = state.intent.as_ref().expect("probe intent");
        assert!(audit_identity_allows_completion(old_intent, &identity));
        assert!(probe_context_matches(old_intent, &source_id, 7, 14));

        retire_after_audit(&mut state);

        assert!(state.intent.is_none());
        assert!(
            !state
                .intent
                .as_ref()
                .is_some_and(|intent| { probe_context_matches(intent, &source_id, 7, 14) })
        );
    }

    #[test]
    fn unchanged_retry_status_does_not_notify_again() {
        assert!(!status_changed("Source unavailable", "Source unavailable"));
        assert!(status_changed("Source unavailable", "Source restored"));
        assert_eq!(
            recovery_action_for(ProbeDecision::SameIdentity, false),
            RecoveryAction::AuditSameGeneration
        );
        assert_eq!(
            recovery_action_for(ProbeDecision::Missing, false),
            RecoveryAction::Retry
        );
    }

    #[test]
    fn root_recovery_and_audit_retry_policy_distinguish_uncertainty() {
        assert!(is_root_failure(&CatalogError::SourceUnavailable(
            String::from("offline",)
        )));
        assert!(is_root_failure(&CatalogError::StaleRootGeneration {
            source_id: SourceId::from_string("source"),
            detail: String::from("changed"),
        }));
        assert!(!is_root_failure(&CatalogError::SourceIdentityUncertain(
            String::from("unknown"),
        )));
        assert!(!is_root_failure(&CatalogError::DatabaseIntegrityUncertain(
            String::from("malformed"),
        )));
        assert!(!audit_failure_is_retryable(
            &CatalogError::SourceIdentityUncertain(String::from("unknown"),)
        ));
        assert!(!audit_failure_is_retryable(
            &CatalogError::DatabaseIntegrityUncertain(String::from("malformed"))
        ));
        assert!(audit_failure_is_retryable(
            &CatalogError::SourceWriterConflict(String::from("locked"),)
        ));
        assert!(audit_failure_is_retryable(&CatalogError::Busy));
        assert!(audit_failure_is_retryable(&CatalogError::Audit(
            String::from("transient scan failure",)
        )));
    }
}
