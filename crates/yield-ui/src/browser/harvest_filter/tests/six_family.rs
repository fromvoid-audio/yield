//! Cross-family integration: real tag/Harvest/curation reads, local metadata fixtures.
use super::*;
use crate::browser::{curation_queue, tag_filter};
use yield_core::{AudioFileCuration, BrowserFilterFamily, CurationQueueScope, PlaybackType};

#[derive(Clone, Copy, Debug)]
enum Family {
    Local(BrowserFilterFamily),
    Tags,
    Curation,
    Harvest,
}
const FAMILIES: [Family; 6] = [
    Family::Local(BrowserFilterFamily::Name),
    Family::Local(BrowserFilterFamily::PlaybackType),
    Family::Local(BrowserFilterFamily::Rating),
    Family::Tags,
    Family::Curation,
    Family::Harvest,
];
fn toggle_family(v: &mut Browser, family: Family, cx: &mut Context<Browser>) {
    match family {
        Family::Local(family) => v.toggle_filter_family(family, cx),
        Family::Tags => tag_filter::toggle(v, cx),
        Family::Curation => curation_queue::toggle(v, cx),
        Family::Harvest => toggle(v, cx),
    }
}
fn enabled(v: &Browser) -> [bool; 6] {
    [
        v.view_model
            .filter_family_enabled(BrowserFilterFamily::Name),
        v.view_model
            .filter_family_enabled(BrowserFilterFamily::PlaybackType),
        v.view_model
            .filter_family_enabled(BrowserFilterFamily::Rating),
        v.tag_filter.desired_enabled,
        v.view_model.curation_queue().enabled,
        mode(v).enabled,
    ]
}
fn settle_all(browser: &gpui::Entity<Browser>, cx: &mut gpui::VisualTestContext) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        browser.update(cx, |v, cx| {
            v.source_registry_task.take();
            poll(v, cx);
            tag_filter::poll(v, cx);
            curation_queue::poll(v, cx);
            let mut deliveries = Vec::new();
            cx.global::<SourceRegistryGlobal>()
                .drain_catalog_deliveries(&mut deliveries);
            for delivery in deliveries {
                v.apply_catalog_delivery(delivery, cx);
            }
        });
        if browser.read_with(cx, |v, _| {
            (!mode(v).filtering() || v.harvest_filter.ready.is_some())
                && !tag_filter::has_pending(v)
                && !curation_queue::has_pending(v)
        }) {
            return;
        }
        if Instant::now() >= deadline {
            let state = browser.read_with(cx, |v, _| format!(
                "Harvest ready={} pending={} error={:?}; tag mask={:?} pending={} status={:?}; curation pending={} status={:?}",
                v.harvest_filter.ready.is_some(), v.harvest_filter.pending.is_some(), v.harvest_filter.error,
                v.view_model.tag_filter().mask(), tag_filter::has_pending(v), tag_filter::status_label(v),
                curation_queue::has_pending(v), curation_queue::status_label(v)));
            panic!("six-family owner reads did not settle: {state}");
        }
        std::thread::yield_now();
    }
}
fn configure(v: &mut Browser, cx: &mut Context<Browser>) {
    // Explicit local hydrated metadata, not a durable curation or playback receipt.
    for e in &mut v.entries {
        e.curation = AudioFileCuration::from_parts(1, false, 0, None, None);
        e.playback_type = Some(PlaybackType::Loop);
    }
    v.view_model.set_entries(&v.entries);
    v.view_model.set_query(&v.entries, "anchor");
    v.view_model.toggle_rating_filter(&v.entries, 1);
    v.view_model
        .toggle_playback_type_filter(&v.entries, PlaybackType::Loop);
    v.tag_filter_input
        .update(cx, |input, cx| input.set_content("absent-tag", cx));
    curation_queue::set_scope(v, CurationQueueScope::Ratings, cx);
    curation_queue::toggle(v, cx);
    select(v, HarvestFilterChoice::New, cx);
}
fn assert_choices(v: &Browser, cx: &gpui::App) {
    assert_eq!(v.tag_filter_input.read(cx).content(), "absent-tag");
    assert_eq!(v.view_model.query(), "anchor");
    assert!(v.view_model.rating_filter().contains_level(1));
    assert!(
        v.view_model
            .playback_type_filter()
            .contains_type(PlaybackType::Loop)
    );
    assert_eq!(
        v.source_sessions[0].curation_queue_mode.scope,
        CurationQueueScope::Ratings
    );
    assert_eq!(mode(v).choice, HarvestFilterChoice::New);
}

#[gpui::test]
fn six_family_toggles_preserve_other_predicates_and_exact_owner_reads(cx: &mut TestAppContext) {
    let (fixture, service, snapshot, mut global) = owner();
    cx.set_global(crate::theme::ActiveTheme::default());
    cx.set_global(SourceRegistryGlobal::for_harvest_fixture(
        service,
        global.acquisition_id(),
    ));
    let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
    browser.update(cx, |v, cx| {
        install_owned(v, &fixture, &snapshot);
        configure(v, cx);
    });
    settle_all(&browser, cx);
    browser.read_with(cx, |v, cx| {
        assert_eq!(enabled(v), [true; 6]);
        assert!(v.view_model.rows().is_empty()); // genuine absent-tag mask excludes every row
        assert_eq!(v.view_model.tag_filter().mask().unwrap().words(), &[0]);
        assert_choices(v, cx);
    });
    for (index, family) in FAMILIES.into_iter().enumerate() {
        browser.update(cx, |v, cx| toggle_family(v, family, cx));
        settle_all(&browser, cx);
        browser.read_with(cx, |v, cx| {
            let mut expected = [true; 6];
            expected[index] = false;
            assert_eq!(enabled(v), expected, "disabled {family:?}");
            assert_choices(v, cx);
            if matches!(family, Family::Tags) {
                assert_eq!(v.view_model.rows().len(), 1);
                assert_eq!(
                    v.entries[v.view_model.rows()[0].entry_index].relative_path,
                    std::path::PathBuf::from("anchor.wav")
                );
            } else {
                assert!(v.view_model.rows().is_empty(), "{family:?} bypassed Tags");
            }
        });
        browser.update(cx, |v, cx| toggle_family(v, family, cx));
        settle_all(&browser, cx);
        browser.read_with(cx, |v, cx| {
            assert_eq!(enabled(v), [true; 6]);
            assert!(v.view_model.rows().is_empty());
            assert_choices(v, cx);
        });
    }
    // With the tag exclusion suspended, stale Harvest custody must hide the matching row.
    browser.update(cx, |v, cx| {
        tag_filter::toggle(v, cx);
        assert_eq!(v.view_model.rows().len(), 1);
        let mut c = v.catalog_metadata.context().unwrap();
        c.entry_indices = Arc::new((*c.entry_indices).clone());
        v.catalog_metadata.install_projection(c);
        invalidate_if_stale(v, cx);
        assert!(v.harvest_filter.ready.is_none());
        assert!(v.view_model.rows().is_empty());
        assert_eq!(enabled(v), [true, true, true, false, true, true]);
        assert_choices(v, cx);
    });
    settle_all(&browser, cx);
    browser.update(cx, |v, cx| {
        assert_eq!(v.view_model.rows().len(), 1);
        tag_filter::toggle(v, cx);
    });
    settle_all(&browser, cx);
    // Suspend all six; then each independently reinstates its retained predicate.
    browser.update(cx, |v, cx| {
        for family in FAMILIES {
            toggle_family(v, family, cx);
        }
        assert_eq!(v.view_model.rows().len(), 3);
        // Change current metadata while Rating/Type are suspended, without changing choices.
        for e in &mut v.entries {
            e.curation = AudioFileCuration::from_parts(3, true, 0, None, None);
            e.playback_type = Some(PlaybackType::OneShot);
        }
        v.view_model.set_entries(&v.entries);
    });
    for family in FAMILIES {
        browser.update(cx, |v, cx| {
            // Use Touched for the Harvest rejection witness; retain it through toggling.
            if matches!(family, Family::Harvest) {
                select(v, HarvestFilterChoice::Touched, cx);
            } else {
                toggle_family(v, family, cx);
            }
        });
        settle_all(&browser, cx);
        browser.read_with(cx, |v, _| {
            assert_eq!(
                v.view_model.rows().len(),
                usize::from(matches!(family, Family::Local(BrowserFilterFamily::Name))),
                "predicate {family:?}"
            );
        });
        browser.update(cx, |v, cx| {
            toggle_family(v, family, cx);
            assert_eq!(enabled(v), [false; 6]);
            assert_eq!(v.view_model.rows().len(), 3);
        });
    }
    cx.update(|_, app| app.update_global::<SourceRegistryGlobal, _>(|g, _| g.shutdown()));
    global.shutdown();
}

#[gpui::test]
fn six_family_reconstruction_reloads_preferences_without_session_predicates(
    cx: &mut TestAppContext,
) {
    let (fixture, service, snapshot, mut global) = owner();
    cx.set_global(crate::theme::ActiveTheme::default());
    cx.set_global(SourceRegistryGlobal::for_harvest_fixture(
        service,
        global.acquisition_id(),
    ));
    let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
    browser.update(cx, |v, cx| {
        install_owned(v, &fixture, &snapshot);
        configure(v, cx);
    });
    settle_all(&browser, cx);
    let (recreated, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
    recreated.update(cx, |v, cx| {
        assert!(v.preferences.accept_snapshot(
            yield_session::PreferenceRevision::default(),
            yield_session::UiPreferences {
                text_scale_percent: 200,
                ..Default::default()
            }
        ));
        v.apply_preference_projection(cx);
        assert_eq!(v.general_settings.text_scale_percent, 200);
        assert_eq!(enabled(v), [true, true, true, false, false, false]);
        assert!(v.view_model.query().is_empty());
        assert!(v.tag_filter_input.read(cx).content().is_empty());
        assert!(!v.view_model.rating_filter().contains_level(1));
        assert!(
            !v.view_model
                .playback_type_filter()
                .contains_type(PlaybackType::Loop)
        );
        assert_eq!(mode(v), HarvestMode::default());
        assert!(v.active_source.is_none());
        assert!(v.view_model.rows().is_empty());
        install_owned(v, &fixture, &snapshot);
        assert_eq!(enabled(v), [true, true, true, false, false, false]);
        assert_eq!(v.view_model.rows().len(), 3);
    });
    cx.update(|_, app| app.update_global::<SourceRegistryGlobal, _>(|g, _| g.shutdown()));
    global.shutdown();
}

#[gpui::test]
fn six_family_source_retirement_cancels_harvest_custody_and_preserves_retained_choices(
    cx: &mut TestAppContext,
) {
    let (fixture, service, snapshot, mut global) = owner();
    cx.set_global(crate::theme::ActiveTheme::default());
    cx.set_global(SourceRegistryGlobal::for_harvest_fixture(
        service,
        global.acquisition_id(),
    ));
    let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
    browser.update(cx, |v, cx| {
        install_owned(v, &fixture, &snapshot);
        configure(v, cx);
    });
    settle_all(&browser, cx);
    browser.update(cx, |v, cx| {
        assert!(v.harvest_filter.ready.is_some());
        let retained_harvest = v.source_sessions[0].harvest_mode;
        let retained_curation = v.source_sessions[0].curation_queue_mode;
        v.clear_active_source_projection(cx);
        assert!(v.active_source.is_none());
        assert!(v.view_model.rows().is_empty());
        assert!(v.harvest_filter.ready.is_none());
        assert!(v.harvest_filter.pending.is_none());
        assert!(!v.view_model.harvest_filter().is_enabled());
        assert_eq!(v.source_sessions[0].harvest_mode, retained_harvest);
        assert_eq!(v.source_sessions[0].curation_queue_mode, retained_curation);
        assert_eq!(v.view_model.query(), "anchor");
        assert!(v.view_model.rating_filter().contains_level(1));
        assert!(
            v.view_model
                .playback_type_filter()
                .contains_type(PlaybackType::Loop)
        );
        // Local family labels and source-scoped labels cannot reactivate an absent source.
        for family in [
            FAMILIES[0],
            FAMILIES[1],
            FAMILIES[2],
            Family::Curation,
            Family::Harvest,
        ] {
            toggle_family(v, family, cx);
        }
        assert_eq!(v.source_sessions[0].harvest_mode, retained_harvest);
        assert_eq!(v.source_sessions[0].curation_queue_mode, retained_curation);
        assert!(v.view_model.rows().is_empty());
    });
    // A pending genuine owner request must be canceled as well as completed custody.
    browser.update(cx, |v, cx| {
        install_owned(v, &fixture, &snapshot);
        select(v, HarvestFilterChoice::Touched, cx);
        poll(v, cx);
        let cancellation = v
            .harvest_filter
            .pending
            .as_ref()
            .expect("genuine queued read")
            .cancellation
            .clone();
        assert!(!cancellation.is_canceled());
        v.clear_active_source_projection(cx);
        assert!(cancellation.is_canceled());
        assert!(v.harvest_filter.pending.is_none());
        assert!(v.harvest_filter.ready.is_none());
        assert!(!v.view_model.harvest_filter().is_enabled());
        assert_eq!(
            v.source_sessions[0].harvest_mode.choice,
            HarvestFilterChoice::Touched
        );
    });
    cx.update(|_, app| app.update_global::<SourceRegistryGlobal, _>(|g, _| g.shutdown()));
    global.shutdown();
}
