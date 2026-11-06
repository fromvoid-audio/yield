//! Source-authorized, native-frame transient markers for the loaded waveform.

use std::{sync::Arc, time::Duration};

use gpui::{
    AnyView, App, Bounds, Context, Render, Task, Window, canvas, div, fill, point, prelude::*, px,
    size,
};
use yield_core::{AudioFile, WaveformViewport};
use yield_library::sample_sources::SourcePublicationFence;
use yield_media::{WaveformDetailContext, WaveformFileRevision};
use yield_session::{
    CatalogCommand, CatalogEvent, FeatureAnalysisCancellation, SourceTransientRequest,
    TransientCacheUse, TransientMarkers, TransientPreset, TransientRequestFailure,
};

use super::{Browser, catalog, catalog_metadata::CatalogMetadataContext};

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
        let Some((entry, detail, source)) = current_source(browser) else {
            return false;
        };
        self.file_revision == detail.file_revision
            && self.request.relative_path() == entry.relative_path
            && self.request.source_id() == &source.source_id
            && self.request.lifecycle_generation() == source.lifecycle_generation
            && self.request.revision() == source.revision
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

fn current_source(
    browser: &Browser,
) -> Option<(&AudioFile, &WaveformDetailContext, &CatalogMetadataContext)> {
    let selected = browser.selected?;
    let entry = browser.entries.get(selected)?;
    let detail = browser.waveform.as_ref()?.detail_context.as_ref()?;
    let source = browser.catalog_metadata.context_ref()?;
    let activation = browser.catalog_activation.as_ref()?;
    if entry.path != detail.path
        || browser.root.as_ref() != Some(&source.root)
        || source.entry_indices.get(&entry.relative_path) != Some(&selected)
        || activation.source_id != source.source_id
        || activation.lifecycle_generation != source.lifecycle_generation
        || activation.root_identity.as_ref() != Some(&source.root_identity)
    {
        return None;
    }
    Some((entry, detail, source))
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
    let Some((entry, detail, source)) = current_source(browser) else {
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
        request,
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
    use super::*;
    use gpui::{KeyDownEvent, Keystroke, TestAppContext};
    use std::{
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
        time::Instant,
    };
    use yield_library::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};

    fn fixture() -> (tempfile::TempDir, PathBuf, yield_media::WaveformPreview) {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("transients.wav");
        let frames = 16_000_u32;
        let mut bytes = Vec::with_capacity(44 + frames as usize * 4);
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
        for frame in 0..frames {
            let sample: f32 = match frame {
                2000 => 0.025,
                7000 => 0.06,
                12000 => 0.8,
                _ => 0.0,
            };
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        fs::write(&path, bytes).expect("fixture WAV");
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
        browser.update(cx, |view, _| {
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
        });
        (browser, cx)
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
            std::thread::yield_now();
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

    fn next_delivery(cx: &mut gpui::VisualTestContext) -> yield_session::CatalogDelivery {
        use super::super::source_registry::SourceRegistryGlobal;
        cx.run_until_parked();
        cx.executor().advance_clock(PRESET_DEBOUNCE);
        cx.run_until_parked();
        wait_for(|| {
            cx.update(|_, app| {
                let mut deliveries = Vec::new();
                app.global::<SourceRegistryGlobal>()
                    .drain_catalog_deliveries(&mut deliveries);
                deliveries.into_iter().next()
            })
        })
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
