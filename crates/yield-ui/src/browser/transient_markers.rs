//! Source-authorized, native-frame transient markers for the loaded waveform.

use std::{sync::Arc, time::Duration};

use gpui::{
    AnyView, App, Bounds, Context, Render, Task, Window, canvas, div, fill, point, prelude::*, px,
    size,
};
use yield_core::WaveformViewport;
use yield_library::sample_sources::SourcePublicationFence;
use yield_media::WaveformFileRevision;
use yield_session::{
    CatalogCommand, CatalogEvent, FeatureAnalysisCancellation, SourceContentDigestScope,
    SourceTransientRequest, TransientCacheUse, TransientMarkers, TransientPreset,
    TransientRequestFailure,
};

use super::{Browser, catalog};

const PRESET_DEBOUNCE: Duration = Duration::from_millis(30);
const MIN_MARKER_SPACING_PX: f32 = 3.0;

enum ResultState {
    Pending,
    Unavailable,
    Ready {
        markers: Arc<TransientMarkers>,
        fence: SourcePublicationFence,
    },
}

struct ActiveRequest {
    request: SourceTransientRequest,
    revision: u64,
    waveform_generation: u64,
    file_revision: WaveformFileRevision,
    cancellation: FeatureAnalysisCancellation,
    result: ResultState,
}

impl ActiveRequest {
    fn is_current(&self, browser: &Browser) -> bool {
        if !browser.transient_markers.enabled
            || self.cancellation.is_cancelled()
            || self.waveform_generation != browser.waveform_generation
            || self.request.preset() != browser.transient_markers.sensitivity
        {
            return false;
        }
        let Some((entry, detail, source)) = catalog::audited_waveform_source(browser) else {
            return false;
        };
        self.file_revision == detail.file_revision
            && self.request.relative_path() == entry.relative_path
            && self.request.source_id() == &source.source_id
            && self.request.lifecycle_generation() == source.lifecycle_generation
            && self.revision == source.revision
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[derive(Default)]
pub(super) struct TransientMarkersState {
    enabled: bool,
    sensitivity: TransientPreset,
    next_request_id: u64,
    active: Option<ActiveRequest>,
    submission: Option<Task<()>>,
}

impl TransientMarkersState {
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn sensitivity_label(&self) -> &'static str {
        match self.sensitivity {
            TransientPreset::Low => "Low",
            TransientPreset::Normal => "Normal",
            TransientPreset::High => "High",
        }
    }

    pub(super) fn sensitivity_short_label(&self) -> &'static str {
        match self.sensitivity {
            TransientPreset::Low => "Lo",
            TransientPreset::Normal => "Mid",
            TransientPreset::High => "Hi",
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.submission = None;
        self.active = None;
    }
}

pub(super) fn toggle(browser: &mut Browser, cx: &mut Context<Browser>) {
    if browser.transient_markers.enabled {
        browser.transient_markers.enabled = false;
        browser.transient_markers.invalidate();
        browser.playback_status = String::from("Transient markers hidden");
    } else if browser.waveform.is_some() {
        browser.transient_markers.enabled = true;
        schedule(browser, cx);
    } else {
        browser.playback_status = String::from("Load a waveform to show transient markers");
    }
    cx.notify();
}

pub(super) fn cycle_sensitivity(browser: &mut Browser, cx: &mut Context<Browser>) {
    if browser.waveform.is_none() {
        return;
    }
    browser.transient_markers.sensitivity = match browser.transient_markers.sensitivity {
        TransientPreset::Low => TransientPreset::Normal,
        TransientPreset::Normal => TransientPreset::High,
        TransientPreset::High => TransientPreset::Low,
    };
    if browser.transient_markers.enabled {
        schedule(browser, cx);
    } else {
        browser.playback_status = format!(
            "Transient sensitivity: {}",
            browser.transient_markers.sensitivity_label()
        );
    }
    cx.notify();
}

struct SensitivityTooltip;

impl Render for SensitivityTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .px(px(theme.spacing.control_gap))
            .py(px(theme.spacing.control_gap))
            .bg(crate::theme::color(theme.colors.surface_raised))
            .text_size(px(theme.typography.small_size))
            .text_color(crate::theme::color(theme.colors.text_primary))
            .child("Transient sensitivity. Click to cycle Low, Normal, and High.")
    }
}

pub(super) fn sensitivity_tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| SensitivityTooltip).into()
}

pub(super) fn schedule(browser: &mut Browser, cx: &mut Context<Browser>) {
    if !browser.transient_markers.enabled
        || browser.waveform.is_none()
        || browser
            .transient_markers
            .active
            .as_ref()
            .is_some_and(|active| active.is_current(browser))
    {
        return;
    }
    browser.transient_markers.invalidate();
    let Some((entry, detail, source)) = catalog::audited_waveform_source(browser) else {
        browser.playback_status =
            String::from("Transient markers unavailable until this waveform has an audited Source");
        return;
    };
    let Some(id) = browser.transient_markers.next_request_id.checked_add(1) else {
        browser.playback_status = String::from("Transient request limit reached");
        return;
    };
    let Ok(request) = SourceTransientRequest::new(
        &source.source_id,
        source.lifecycle_generation,
        source.revision,
        id,
        &entry.relative_path,
        browser.transient_markers.sensitivity,
    ) else {
        browser.playback_status =
            String::from("Transient markers unavailable for this audio entry");
        return;
    };
    let file_revision = detail.file_revision.clone();
    browser.transient_markers.next_request_id = id;
    browser.transient_markers.active = Some(ActiveRequest {
        revision: request.revision(),
        request: request.with_pending_digest(),
        waveform_generation: browser.waveform_generation,
        file_revision,
        cancellation: FeatureAnalysisCancellation::new(),
        result: ResultState::Pending,
    });
    browser.playback_status = String::from("Finding transient markers…");
    // Coalesce rapid preset changes before spending a Catalog queue slot.
    browser.transient_markers.submission = Some(cx.spawn(async move |this, cx| {
        cx.background_executor().timer(PRESET_DEBOUNCE).await;
        let _ =
            this.update(cx, |browser, cx| {
                let Some(active) = browser.transient_markers.active.as_ref().filter(|active| {
                    active.request.request_id() == id && active.is_current(browser)
                }) else {
                    return;
                };
                let command = CatalogCommand::RequestTransientMarkers {
                    request: active.request.clone(),
                    cancellation: active.cancellation.clone(),
                };
                browser.transient_markers.submission = None;
                match catalog::submit_catalog_command(command, cx) {
                    Ok(()) => browser.start_source_registry_poll(cx),
                    Err(error) => {
                        if let Some(active) = browser.transient_markers.active.as_mut() {
                            active.result = ResultState::Unavailable;
                        }
                        browser.playback_status = format!("Transient markers unavailable: {error}");
                    }
                }
                cx.notify();
            });
    }));
    cx.notify();
}

pub(super) fn accept_digest_revision(
    browser: &mut Browser,
    scope: &SourceContentDigestScope,
    request_id: Option<u64>,
    revision: u64,
    cx: &mut Context<Browser>,
) {
    if let Some(active) = browser.transient_markers.active.as_mut()
        && request_id == Some(active.request.request_id())
        && active.request.matches_digest_scope(scope)
        && matches!(active.result, ResultState::Pending)
    {
        active.revision = revision;
    }
    schedule(browser, cx);
}

pub(super) fn apply_catalog_event(
    browser: &mut Browser,
    event: &mut CatalogEvent,
    fence: Option<SourcePublicationFence>,
    cx: &mut Context<Browser>,
) -> bool {
    let CatalogEvent::TransientMarkersCompleted {
        request,
        cancellation,
        result,
    } = event
    else {
        return false;
    };
    let current = browser
        .transient_markers
        .active
        .as_ref()
        .is_some_and(|active| {
            active.request == *request
                && active.is_current(browser)
                && matches!(active.result, ResultState::Pending)
        });
    if !current || cancellation.is_cancelled() {
        tracing::debug!(
            request_id = request.request_id(),
            "Discarded stale transient result"
        );
        return true;
    }
    let result = std::mem::replace(result, Err(TransientRequestFailure::Cancelled));
    let (state, status) = match result {
        Ok(outcome)
            if outcome.markers.preset() == request.preset()
                && browser.waveform.as_ref().is_some_and(|waveform| {
                    waveform.total_frames == outcome.markers.source_frames() as usize
                }) =>
        {
            if let Some(fence) = fence.filter(SourcePublicationFence::is_live) {
                let markers = outcome.markers;
                let mut status = if markers.frames().is_empty() {
                    String::from("No transients found")
                } else {
                    format!("{} transient markers", markers.frames().len())
                };
                match outcome.cache {
                    TransientCacheUse::Hit => status.push_str(" · cached"),
                    TransientCacheUse::MissStored => {}
                    TransientCacheUse::MissUnstored => status.push_str(" · cache unavailable"),
                }
                tracing::debug!(
                    request_id = request.request_id(),
                    cache = ?outcome.cache,
                    detector_version = outcome.detector_version,
                    marker_count = markers.frames().len(),
                    elapsed_ms = outcome.elapsed.as_millis() as u64,
                    "Transient analysis ready"
                );
                (
                    ResultState::Ready {
                        markers: Arc::new(markers),
                        fence,
                    },
                    status,
                )
            } else {
                unavailable(TransientRequestFailure::StaleSource)
            }
        }
        Ok(_) => unavailable(TransientRequestFailure::StaleSource),
        Err(error) => {
            tracing::debug!(
                request_id = request.request_id(),
                ?error,
                "Transient analysis unavailable"
            );
            unavailable(error)
        }
    };
    if let Some(active) = browser.transient_markers.active.as_mut() {
        active.result = state;
    }
    browser.playback_status = status;
    cx.notify();
    true
}

fn unavailable(error: TransientRequestFailure) -> (ResultState, String) {
    let reason = match error {
        TransientRequestFailure::InvalidInput => "invalid audio entry".into(),
        TransientRequestFailure::StaleSource => "the Source or waveform changed".into(),
        TransientRequestFailure::Input(error) => error.to_string(),
        TransientRequestFailure::Decode(reason) => reason.into(),
        TransientRequestFailure::Computation => "analysis failed".into(),
        TransientRequestFailure::Busy => "transient analysis is busy".into(),
        TransientRequestFailure::Cancelled => "analysis cancelled".into(),
    };
    (
        ResultState::Unavailable,
        format!("Transient markers unavailable: {reason}"),
    )
}

fn current_markers(browser: &Browser) -> Option<Arc<TransientMarkers>> {
    let active = browser.transient_markers.active.as_ref()?;
    let ResultState::Ready { markers, fence } = &active.result else {
        return None;
    };
    (fence.is_live() && active.is_current(browser)).then(|| Arc::clone(markers))
}

fn visible_positions(
    frames: &[u32],
    source_frames: u32,
    viewport: WaveformViewport,
    width_px: f32,
) -> impl Iterator<Item = f32> + '_ {
    let mut last_x = f32::NEG_INFINITY;
    frames.iter().filter_map(move |frame| {
        let visible =
            viewport.visible_ratio_from_absolute(f64::from(*frame) / f64::from(source_frames))?;
        let x = visible as f32 * width_px;
        if x - last_x < MIN_MARKER_SPACING_PX {
            return None;
        }
        last_x = x;
        Some(x)
    })
}

pub(super) fn render_overlay(
    browser: &Browser,
    theme: &yield_design::Theme,
) -> impl gpui::IntoElement {
    let markers = current_markers(browser);
    let viewport = browser.waveform_viewport;
    let tint = crate::theme::color(theme.colors.waveform_playhead).opacity(0.5);
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let Some(markers) = markers.as_ref() else {
                        return;
                    };
                    for x in visible_positions(
                        markers.frames(),
                        markers.source_frames(),
                        viewport,
                        f32::from(bounds.size.width),
                    ) {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.origin.x + px(x), bounds.origin.y),
                                size(px(1.0), bounds.size.height),
                            ),
                            tint,
                        ));
                    }
                },
            )
            .size_full(),
        )
}

#[cfg(test)]
mod tests {
    use super::super::catalog_metadata::CatalogMetadataContext;
    use super::*;
    use gpui::{KeyDownEvent, Keystroke, TestAppContext};
    use std::{
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
        time::Instant,
    };
    use yield_core::AudioFile;
    use yield_library::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};

    fn fixture() -> (tempfile::TempDir, PathBuf, yield_media::WaveformPreview) {
        fixture_frames(16_000)
    }

    fn fixture_frames(frames: u32) -> (tempfile::TempDir, PathBuf, yield_media::WaveformPreview) {
        use std::io::Write;
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("transients.wav");
        let mut bytes = Vec::with_capacity(44 + 16_000 * 4);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + frames * 4).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&64_000_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&32_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(frames * 4).to_le_bytes());
        for frame in 0..frames.min(16_000) {
            let sample: f32 = match frame {
                2000 => 0.025,
                7000 => 0.06,
                12000 => 0.8,
                _ => 0.0,
            };
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&bytes).unwrap();
        file.set_len(44 + u64::from(frames) * 4).unwrap();
        drop(file);
        let preview = yield_media::load_waveform_preview(&path).expect("load preview");
        (directory, path, preview)
    }

    fn install<'a>(
        cx: &'a mut TestAppContext,
        path: &Path,
        preview: yield_media::WaveformPreview,
    ) -> (gpui::Entity<Browser>, &'a mut gpui::VisualTestContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| install_waveform(view, path, preview));
        (browser, cx)
    }

    fn install_waveform(view: &mut Browser, path: &Path, preview: yield_media::WaveformPreview) {
        view.entries = vec![AudioFile {
            path: path.to_path_buf(),
            relative_path: PathBuf::from("transients.wav"),
            byte_len: fs::metadata(path).expect("metadata").len(),
            duration_seconds: Some(1.0),
            sample_rate: Some(16_000),
            playback_type: None,
            curation: None,
        }];
        view.selected = Some(0);
        view.waveform_generation = 7;
        view.waveform = Some(preview);
    }

    #[test]
    fn projection_thins_close_pixels_and_reveals_exact_frames_on_zoom() {
        let frames = [1000, 1001, 1500, 1999];
        let overview =
            visible_positions(&frames, 2000, WaveformViewport::full(), 200.0).collect::<Vec<_>>();
        assert_eq!(overview.len(), 3);
        for (actual, expected) in overview.into_iter().zip([100.0, 150.0, 199.9]) {
            assert!((actual - expected).abs() < 0.001);
        }
        let detail = WaveformViewport {
            start: 0.499,
            end: 0.501,
        };
        let zoomed = visible_positions(&frames, 2000, detail, 200.0).collect::<Vec<_>>();
        assert_eq!(zoomed, [100.0, 150.0]);
    }

    fn wait_for<T>(mut poll: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(value) = poll() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for Catalog");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn audited_owner(
        guard: &WritableProfileGuard,
        root: &Path,
    ) -> (
        yield_catalog::CatalogService,
        yield_catalog::CatalogSnapshot,
    ) {
        let service = yield_catalog::CatalogService::start(guard).unwrap();
        let source_id = yield_library::sample_sources::SourceId::from_string("gpui-transients");
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: root.to_path_buf(),
                lifecycle_generation: 1,
            })
            .unwrap();
        wait_for(|| {
            matches!(
                service.try_recv_event(),
                Ok(CatalogEvent::Registered { .. })
            )
            .then_some(())
        });
        service
            .submit(CatalogCommand::RequestAudit {
                source_id,
                lifecycle_generation: 1,
            })
            .unwrap();
        let snapshot = wait_for(|| match service.try_recv_event() {
            Ok(CatalogEvent::AuditCommitted { snapshot }) => Some(snapshot),
            Ok(CatalogEvent::Failed { error, .. }) => panic!("audit failed: {error}"),
            _ => None,
        });
        (service, snapshot)
    }

    fn install_source(view: &mut Browser, root: &Path, snapshot: &yield_catalog::CatalogSnapshot) {
        view.root = Some(root.to_path_buf());
        view.catalog_activation = Some(catalog::CatalogActivation {
            source_id: snapshot.source_id.clone(),
            root: root.to_path_buf(),
            lifecycle_generation: snapshot.lifecycle_generation,
            registered: true,
            registration_admitted: true,
            root_identity: Some(snapshot.root_identity.clone()),
            audit_finished: true,
            audit_error: None,
            audit_retryable: false,
            force_reload: false,
            fallback_to_scan: false,
        });
        view.catalog_metadata
            .install_projection(CatalogMetadataContext {
                source_id: snapshot.source_id.clone(),
                lifecycle_generation: snapshot.lifecycle_generation,
                root: root.to_path_buf(),
                root_identity: snapshot.root_identity.clone(),
                directory_generation: snapshot.directory_generation,
                revision: snapshot.committed_revision,
                entry_indices: Arc::new(HashMap::from([(PathBuf::from("transients.wav"), 0)])),
            });
    }

    fn install_digest_projection(
        view: &mut Browser,
        root: &Path,
        snapshot: &yield_catalog::CatalogSnapshot,
    ) {
        install_source(view, root, snapshot);
        let mut session = super::super::SourceSession::new(root.to_path_buf());
        session.source_id = Some(snapshot.source_id.clone());
        session.catalog_lifecycle_generation = Some(snapshot.lifecycle_generation);
        session.catalog_facts = Arc::new(
            snapshot
                .manifest
                .iter()
                .map(|entry| {
                    (
                        entry.relative_path.clone(),
                        catalog::CatalogFileFacts {
                            relative_path: entry.relative_path.clone(),
                            file_size: entry.file_size,
                            modified_ns: entry.modified_ns,
                            file_identity: entry.file_identity.clone(),
                            content_hash: entry.content_hash.clone(),
                        },
                    )
                })
                .collect(),
        );
        view.source_sessions = vec![session];
        view.active_source = Some(0);
        view.curation_state.install_projection(
            super::super::curation::CurationLifecycleKey {
                source_id: snapshot.source_id.clone(),
                lifecycle_generation: snapshot.lifecycle_generation,
            },
            snapshot.root_identity.clone(),
            snapshot.directory_generation,
            snapshot.committed_revision,
            &view.entries,
        );
    }

    fn next_delivery(cx: &mut gpui::VisualTestContext) -> yield_session::CatalogDelivery {
        let mut deliveries = next_deliveries(cx);
        assert_eq!(deliveries.len(), 1);
        deliveries.pop().unwrap()
    }

    fn next_deliveries(cx: &mut gpui::VisualTestContext) -> Vec<yield_session::CatalogDelivery> {
        use super::super::source_registry::SourceRegistryGlobal;
        cx.run_until_parked();
        cx.executor().advance_clock(PRESET_DEBOUNCE);
        cx.run_until_parked();
        wait_for(|| {
            cx.update(|_, app| {
                let mut deliveries = Vec::new();
                app.global::<SourceRegistryGlobal>()
                    .drain_catalog_deliveries(&mut deliveries);
                (!deliveries.is_empty()).then_some(deliveries)
            })
        })
    }

    fn next_tempo_delivery(cx: &mut TestAppContext) -> yield_session::CatalogDelivery {
        use super::super::source_registry::SourceRegistryGlobal;
        wait_for(|| {
            cx.update(|app| {
                let mut deliveries = Vec::new();
                app.global::<SourceRegistryGlobal>()
                    .drain_catalog_deliveries(&mut deliveries);
                assert!(deliveries.len() <= 1);
                deliveries.pop()
            })
        })
    }

    #[gpui::test]
    fn saved_playmark_tempo_restores_only_the_current_unedited_waveform(cx: &mut TestAppContext) {
        use super::super::{
            playmark_tempo, source_registry::SourceRegistryGlobal, waveform::WaveformPlayRegion,
        };
        use gpui::AppContext;
        use std::sync::atomic::AtomicBool;
        use yield_session::{
            PlaymarkTempoOperation, SourcePlaymarkTempo, SourcePlaymarkTempoRequest,
        };

        let (_directory, path, preview) = fixture();
        let root = path.parent().unwrap();
        let config = tempfile::tempdir().unwrap();
        let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let _profile = PersistenceProfileGuard::named("gpui-playmark-restore");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let (service, snapshot) = audited_owner(&guard, root);
        let saved = SourcePlaymarkTempo::new(2_000, 8_000, 16_000, 16_000, 6).unwrap();
        service
            .submit(CatalogCommand::PlaymarkTempo {
                request: SourcePlaymarkTempoRequest::new(
                    &snapshot.source_id,
                    snapshot.lifecycle_generation,
                    snapshot.committed_revision,
                    1,
                    Path::new("transients.wav"),
                    PlaymarkTempoOperation::Replace {
                        previous: None,
                        next: Some(saved),
                    },
                )
                .unwrap(),
                cancellation: Arc::new(AtomicBool::new(false)),
            })
            .unwrap();
        wait_for(|| match service.try_recv_event() {
            Ok(CatalogEvent::PlaymarkTempoCompleted { result, .. }) => {
                assert_eq!(result, Ok(Some(saved)));
                Some(())
            }
            _ => None,
        });
        cx.set_global(SourceRegistryGlobal::for_catalog_fixture(service));
        cx.set_global(crate::theme::ActiveTheme::default());
        let browser = cx.new(Browser::new);
        browser.update(cx, |view, cx| {
            install_waveform(view, &path, preview);
            install_source(view, root, &snapshot);
            assert!(playmark_tempo::load_for_waveform(view, cx));
            assert!(!playmark_tempo::load_for_waveform(view, cx));
        });
        let (older, fence) = next_tempo_delivery(cx).into_parts_if_live().unwrap();
        browser.update(cx, |view, cx| {
            view.set_waveform_play_region(WaveformPlayRegion::new(0.1, 0.2));
            view.set_waveform_play_region(None);
            assert!(playmark_tempo::apply_catalog_event(
                view,
                &older,
                fence.as_ref(),
                cx
            ));
            assert!(view.waveform_play_region.is_none());
            assert!(!playmark_tempo::load_for_waveform(view, cx));
            view.invalidate_waveform_generation();
            assert!(playmark_tempo::load_for_waveform(view, cx));
            assert!(playmark_tempo::apply_catalog_event(
                view,
                &older,
                fence.as_ref(),
                cx
            ));
            assert!(view.waveform_play_region.is_none());
        });
        let current = next_tempo_delivery(cx);
        browser.update(cx, |view, cx| {
            assert!(view.apply_catalog_delivery(current, cx));
            assert_eq!(
                view.waveform_play_region.unwrap().frame_bounds(16_000),
                Some((2_000, 8_000))
            );
            assert_eq!(view.beat_count_input.read(cx).count(), 6);
            assert!(view.playing_path.is_none());
            assert!(view.playback_init_intent.is_none());
        });
        cx.update(|app| {
            app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown())
        });
    }

    #[gpui::test]
    fn tempo_save_waits_for_the_receipt_and_preserves_newer_local_edits(cx: &mut TestAppContext) {
        use super::super::{
            playmark_tempo, source_registry::SourceRegistryGlobal, waveform::WaveformPlayRegion,
        };
        use gpui::AppContext;
        use yield_session::SourcePlaymarkTempo;
        let (_directory, path, preview) = fixture();
        let root = path.parent().unwrap();
        let config = tempfile::tempdir().unwrap();
        let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let _profile = PersistenceProfileGuard::named("gpui-playmark-save");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let (service, snapshot) = audited_owner(&guard, root);
        cx.set_global(SourceRegistryGlobal::for_catalog_fixture(service));
        cx.set_global(crate::theme::ActiveTheme::default());
        let browser = cx.new(Browser::new);
        let baseline = WaveformPlayRegion::new(0.125, 0.25);
        let first = SourcePlaymarkTempo::new(2_000, 8_000, 16_000, 16_000, 6).unwrap();
        let second = SourcePlaymarkTempo::new(2_000, 12_000, 16_000, 16_000, 6).unwrap();
        browser.update(cx, |view, cx| {
            install_waveform(view, &path, preview);
            install_source(view, root, &snapshot);
            view.set_waveform_play_region(baseline);
            assert!(playmark_tempo::save_authored_tempo(view, first, cx));
            assert_eq!(view.waveform_play_region, baseline);
            assert_eq!(view.playback_status, "Saving playmark tempo…");
        });
        let prior = next_tempo_delivery(cx);
        browser.update(cx, |view, cx| {
            assert!(view.apply_catalog_delivery(prior, cx));
            assert_eq!(view.waveform_play_region, baseline);
        });
        let first_receipt = next_tempo_delivery(cx);
        browser.update(cx, |view, cx| {
            assert!(view.apply_catalog_delivery(first_receipt, cx));
            assert_eq!(
                view.waveform_play_region.unwrap().frame_bounds(16_000),
                Some((2_000, 8_000))
            );
            assert_eq!(view.beat_count_input.read(cx).count(), 6);
            assert!(playmark_tempo::save_authored_tempo(view, second, cx));
        });
        let second_receipt = next_tempo_delivery(cx);
        browser.update(cx, |view, cx| {
            view.set_waveform_play_region(baseline);
            assert!(view.apply_catalog_delivery(second_receipt, cx));
            assert_eq!(view.waveform_play_region, baseline);
            view.invalidate_waveform_generation();
            assert!(playmark_tempo::load_for_waveform(view, cx));
        });
        let reopened = next_tempo_delivery(cx);
        browser.update(cx, |view, cx| {
            assert!(view.apply_catalog_delivery(reopened, cx));
            assert_eq!(
                view.waveform_play_region.unwrap().frame_bounds(16_000),
                Some((2_000, 12_000))
            );
            assert!(view.playback_init_intent.is_none());
        });
        cx.update(|app| {
            app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown())
        });
    }

    #[gpui::test]
    fn playmark_tempo_history_waits_for_the_source_and_restores_beats(cx: &mut TestAppContext) {
        use super::super::{
            playmark_tempo, source_registry::SourceRegistryGlobal, waveform::WaveformPlayRegion,
        };
        use gpui::AppContext;
        use yield_session::{CatalogEvent, SourcePlaymarkTempo};

        let (_directory, path, preview) = fixture();
        let root = path.parent().unwrap();
        let config = tempfile::tempdir().unwrap();
        let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let _profile = PersistenceProfileGuard::named("gpui-playmark-history");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let (service, snapshot) = audited_owner(&guard, root);
        cx.set_global(SourceRegistryGlobal::for_catalog_fixture(service));
        cx.set_global(crate::theme::ActiveTheme::default());
        let browser = cx.new(Browser::new);
        let baseline = WaveformPlayRegion::new(0.125, 0.25);
        let authored = WaveformPlayRegion::new(0.125, 0.5);
        let tempo = SourcePlaymarkTempo::new(2_000, 8_000, 16_000, 16_000, 6).unwrap();
        browser.update(cx, |view, cx| {
            install_waveform(view, &path, preview);
            install_source(view, root, &snapshot);
            view.set_waveform_play_region(baseline);
            assert!(playmark_tempo::save_authored_tempo(view, tempo, cx));
        });
        for _ in 0..2 {
            let delivery = next_tempo_delivery(cx);
            browser.update(cx, |view, cx| {
                assert!(view.apply_catalog_delivery(delivery, cx))
            });
        }
        for (redo, before, after, expected, beats) in [
            (false, authored, baseline, None, 4),
            (true, baseline, authored, Some(tempo), 6),
        ] {
            browser.update(cx, |view, cx| {
                assert_eq!(view.waveform_play_region, before);
                assert!(playmark_tempo::move_history(view, redo, cx));
                assert_eq!(view.waveform_play_region, before);
            });
            let (event, fence) = next_tempo_delivery(cx).into_parts_if_live().unwrap();
            assert!(matches!(
                event,
                CatalogEvent::PlaymarkTempoCompleted { result, .. } if result == Ok(expected)
            ));
            browser.update(cx, |view, cx| {
                assert!(playmark_tempo::apply_catalog_event(
                    view,
                    &event,
                    fence.as_ref(),
                    cx
                ));
                assert_eq!(view.waveform_play_region, after);
                assert_eq!(view.beat_count_input.read(cx).count(), beats);
            });
        }
        browser.update(cx, |view, cx| {
            view.invalidate_waveform_generation();
            assert!(!playmark_tempo::move_history(view, false, cx));
        });
        cx.update(|app| {
            app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown())
        });
    }

    #[gpui::test]
    fn timing_editor_saves_bpm_and_keeps_duration_edits_local(cx: &mut TestAppContext) {
        use super::super::{
            playmark_tempo, source_registry::SourceRegistryGlobal, waveform::WaveformPlayRegion,
        };
        use yield_session::SourcePlaymarkTempo;

        let (_directory, path, preview) = fixture();
        let root = path.parent().unwrap();
        let config = tempfile::tempdir().unwrap();
        let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let _profile = PersistenceProfileGuard::named("gpui-manual-tempo");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let (service, snapshot) = audited_owner(&guard, root);
        cx.set_global(SourceRegistryGlobal::for_catalog_fixture(service));
        let (browser, cx) = install(cx, &path, preview);
        let baseline = WaveformPlayRegion::new(0.125, 0.25);
        browser.update(cx, |view, _| {
            install_source(view, root, &snapshot);
            view.set_waveform_play_region(baseline);
        });
        let enter = KeyDownEvent {
            keystroke: Keystroke::parse("enter").unwrap(),
            is_held: false,
        };
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(view.begin_playmark_timing_edit(window, cx));
                view.playmark_timing_input
                    .update(cx, |input, cx| input.set_content("640 BpM", cx));
                view.handle_key_down(&enter, window, cx);
                assert!(view.waveform_focus_handle.is_focused(window));
                assert_eq!(view.waveform_play_region, baseline);
            });
        });
        let prior = next_tempo_delivery(&mut cx.cx);
        browser.update(cx, |view, cx| {
            assert!(view.apply_catalog_delivery(prior, cx))
        });
        let (event, fence) = next_tempo_delivery(&mut cx.cx)
            .into_parts_if_live()
            .unwrap();
        let expected = SourcePlaymarkTempo::new(2_000, 8_000, 16_000, 16_000, 4);
        assert!(matches!(
            event,
            CatalogEvent::PlaymarkTempoCompleted { result, .. } if result == Ok(expected)
        ));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(playmark_tempo::apply_catalog_event(
                    view,
                    &event,
                    fence.as_ref(),
                    cx
                ));
                assert_eq!(
                    view.waveform_play_region,
                    WaveformPlayRegion::new(0.125, 0.5)
                );
                assert!(view.begin_playmark_timing_edit(window, cx));
                view.playmark_timing_input
                    .update(cx, |input, cx| input.set_content("250ms", cx));
                view.handle_key_down(&enter, window, cx);
                assert_eq!(
                    view.waveform_play_region,
                    WaveformPlayRegion::new(0.125, 0.375)
                );
                assert!(view.playback_init_intent.is_none());
                view.invalidate_waveform_generation();
                assert!(playmark_tempo::load_for_waveform(view, cx));
            });
        });
        let reopened = next_tempo_delivery(&mut cx.cx);
        browser.update(cx, |view, cx| {
            assert!(view.apply_catalog_delivery(reopened, cx));
            assert_eq!(
                view.waveform_play_region,
                WaveformPlayRegion::new(0.125, 0.5)
            );
        });
        cx.update(|_, app| {
            app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown());
        });
    }

    #[gpui::test]
    fn quick_audited_recording_advances_the_source_without_reloading_the_waveform(
        cx: &mut TestAppContext,
    ) {
        use super::super::source_registry::SourceRegistryGlobal;
        use yield_library::sample_sources::readiness::FEATURE_INPUT_MAX_BYTES;
        let frames = (FEATURE_INPUT_MAX_BYTES / 4) as u32 + 256;
        let (_directory, path, preview) = fixture_frames(frames);
        let root = path.parent().unwrap();
        let config = tempfile::tempdir().unwrap();
        let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let _profile = PersistenceProfileGuard::named("gpui-long-transients");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let (service, snapshot) = audited_owner(&guard, root);
        assert!(snapshot.manifest[0].content_hash.is_none());
        cx.set_global(SourceRegistryGlobal::for_catalog_fixture(service));
        let (browser, cx) = install(cx, &path, preview);
        browser.update(cx, |view, cx| {
            install_digest_projection(view, root, &snapshot);
            toggle(view, cx);
        });
        for _ in 0..2 {
            let deliveries = next_deliveries(cx);
            let ready = browser.update(cx, |view, cx| {
                for delivery in deliveries {
                    view.apply_catalog_delivery(delivery, cx);
                }
                assert_eq!(view.waveform_generation, 7);
                assert_eq!(view.selected, Some(0));
                current_markers(view).is_some()
            });
            if ready {
                break;
            }
        }
        browser.update(cx, |view, _| {
            assert_eq!(
                view.catalog_metadata.context_ref().unwrap().revision,
                snapshot.committed_revision + 1
            );
            assert_eq!(view.transient_markers.next_request_id, 1);
            let markers = current_markers(view).unwrap();
            assert_eq!(markers.source_frames(), frames);
            assert!(markers.frames().contains(&12_000));
            assert!(view.curation_state.authority_matches(
                &super::super::curation::CurationLifecycleKey {
                    source_id: snapshot.source_id.clone(),
                    lifecycle_generation: snapshot.lifecycle_generation,
                },
                &snapshot.root_identity,
                snapshot.directory_generation,
                snapshot.committed_revision + 1,
            ));
            let facts = view.source_sessions[0]
                .catalog_facts
                .get(Path::new("transients.wav"))
                .unwrap();
            assert!(facts.content_hash.is_some());
        });
        cx.update(|_, app| {
            app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown())
        });
    }

    #[gpui::test]
    fn toolbar_projects_exact_owner_frames_and_rejects_the_old_preset(cx: &mut TestAppContext) {
        use super::super::source_registry::SourceRegistryGlobal;
        let (_directory, path, preview) = fixture();
        let root = path.parent().unwrap();
        let config = tempfile::tempdir().unwrap();
        let _base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let _profile = PersistenceProfileGuard::named("gpui-transients");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let (service, snapshot) = audited_owner(&guard, root);
        cx.set_global(SourceRegistryGlobal::for_catalog_fixture(service));
        let (browser, cx) = install(cx, &path, preview);
        browser.update(cx, |view, cx| {
            install_source(view, root, &snapshot);
            // This fixture holds raw marker deliveries while changing presets.
            view.playmark_tempo.preserve_local_edit();
            toggle(view, cx);
        });
        let old = next_delivery(cx);
        let token = browser.read_with(cx, |view, _| {
            view.transient_markers
                .active
                .as_ref()
                .unwrap()
                .cancellation
                .clone()
        });
        cx.refresh().expect("loaded waveform layout");
        let toolbar = cx.debug_bounds("main-toolbar").expect("toolbar");
        let button = cx
            .debug_bounds("transient-sensitivity-toolbar")
            .expect("sensitivity button");
        assert!(button.left() >= toolbar.left() && button.right() <= toolbar.right());
        assert!(button.top() >= toolbar.top() && button.bottom() <= toolbar.bottom());
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        assert!(token.is_cancelled());
        browser.update(cx, |view, cx| {
            assert!(!view.apply_catalog_delivery(old, cx));
            assert!(current_markers(view).is_none());
        });

        let completion = next_delivery(cx);
        browser.update(cx, |view, cx| {
            assert!(view.apply_catalog_delivery(completion, cx));
            let markers = current_markers(view).unwrap();
            assert_eq!(markers.preset(), TransientPreset::High);
            assert_eq!(markers.frames(), [2000, 7000, 12000]);
            assert_eq!(view.playback_status, "3 transient markers");
            toggle(view, cx);
            toggle(view, cx);
        });
        let cached = next_delivery(cx);
        browser.update(cx, |view, cx| {
            assert!(view.apply_catalog_delivery(cached, cx));
            assert_eq!(view.playback_status, "3 transient markers · cached");
            view.catalog_metadata
                .advance_projection_revision(snapshot.committed_revision + 1);
            assert!(current_markers(view).is_none());
            toggle(view, cx);
            assert_eq!(view.playback_status, "Transient markers hidden");
        });
        cx.update(|_, app| {
            app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown())
        });
    }

    #[gpui::test]
    fn i_shortcut_only_toggles_with_waveform_focus_and_ignores_repeat(cx: &mut TestAppContext) {
        let (_directory, path, preview) = fixture();
        let (browser, cx) = install(cx, &path, preview);
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let press = |view: &mut Browser,
                             held,
                             window: &mut gpui::Window,
                             cx: &mut Context<Browser>| {
                    view.handle_key_down(
                        &KeyDownEvent {
                            keystroke: Keystroke::parse("i").expect("I key"),
                            is_held: held,
                        },
                        window,
                        cx,
                    );
                };
                window.focus(&view.search_input.read(cx).focus_handle());
                press(view, false, window, cx);
                assert!(!view.transient_markers.enabled());
                window.focus(&view.waveform_focus_handle);
                press(view, false, window, cx);
                assert!(view.transient_markers.enabled());
                press(view, true, window, cx);
                assert!(view.transient_markers.enabled());
            });
        });
    }
}
