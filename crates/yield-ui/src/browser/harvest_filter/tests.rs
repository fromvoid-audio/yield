//! Genuine source/global/catalog to GPUI consumer fixtures; no physical qualification.
use super::super::SourceSession;
use super::*;
use gpui::{AppContext, BorrowAppContext, TestAppContext, prelude::*};
use std::time::{Duration, Instant};

struct Controls(gpui::Entity<Browser>);
impl gpui::Render for Controls {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = crate::theme::active(cx).clone();
        self.0.update(cx, |view, cx| {
            gpui::div()
                .relative()
                .size_full()
                .child(render(view, theme, cx))
        })
    }
}
fn draw(_controls: &gpui::Entity<Controls>, cx: &mut gpui::VisualTestContext) {
    // Use the real window frame lifecycle: manually painting an element repeatedly appends
    // capture listeners to an old frame and can falsely report an inside click as outside.
    cx.update(|window, app| {
        let _ = window.draw(app);
    });
}

fn settle(browser: &gpui::Entity<Browser>, cx: &mut gpui::VisualTestContext) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        browser.update(cx, |view, cx| {
            poll(view, cx);
            let mut deliveries = Vec::new();
            cx.global::<SourceRegistryGlobal>()
                .drain_catalog_deliveries(&mut deliveries);
            for delivery in deliveries {
                view.apply_catalog_delivery(delivery, cx);
            }
        });
        if browser.read_with(cx, |v, _| v.harvest_filter.ready.is_some()) {
            return;
        }
        if Instant::now() >= deadline {
            let state = browser.read_with(cx, |v, _| {
                format!(
                    "mode={:?}, context={:?}, pending={}, error={:?}, rows={}",
                    mode(v),
                    context(v).map(|c| c.revision),
                    v.harvest_filter.pending.is_some(),
                    v.harvest_filter.error,
                    v.view_model.rows().len()
                )
            });
            panic!("Harvest GPUI owner timeout: {state}");
        }
        std::thread::yield_now();
    }
}
#[gpui::test]
fn harvest_filter_actual_controls_owner_mask_all_disable_and_source_scoping(
    cx: &mut TestAppContext,
) {
    let (fixture, service, snapshot, mut global) = owner();
    cx.set_global(crate::theme::ActiveTheme::default());
    cx.set_global(SourceRegistryGlobal::for_harvest_fixture(
        service,
        global.acquisition_id(),
    ));
    let (controls, cx) = cx.add_window_view(|_, cx| Controls(cx.new(|cx| Browser::new(cx))));
    let browser = cx.update(|_, app| controls.read(app).0.clone());
    browser.update(cx, |v, _| install_owned(v, &fixture, &snapshot));
    draw(&controls, cx);
    let dropdown = cx.debug_bounds("harvest-filter-scope").unwrap();
    cx.simulate_click(dropdown.center(), gpui::Modifiers::none());
    draw(&controls, cx);
    for selector in [
        "harvest-filter-choice-0",
        "harvest-filter-choice-1",
        "harvest-filter-choice-2",
        "harvest-filter-choice-3",
        "harvest-filter-choice-4",
        "harvest-filter-choice-5",
        "harvest-filter-choice-6",
        "harvest-filter-choice-7",
        "harvest-filter-choice-8",
    ] {
        assert!(cx.debug_bounds(selector).is_some());
    }
    let choice = cx.debug_bounds("harvest-filter-choice-0").unwrap();
    cx.simulate_click(choice.center(), gpui::Modifiers::none());
    browser.read_with(cx, |v, _| {
        assert_eq!(
            mode(v),
            HarvestMode {
                enabled: true,
                choice: HarvestFilterChoice::New
            }
        )
    });
    settle(&browser, cx);
    browser.update(cx, |v, cx| toggle_menu(v, cx));
    draw(&controls, cx);
    cx.simulate_click(
        gpui::point(gpui::px(590.), gpui::px(590.)),
        gpui::Modifiers::none(),
    );
    browser.read_with(cx, |v, _| assert!(!v.harvest_filter.menu_open));
    browser.update(cx, |v, cx| {
        assert_eq!(v.view_model.rows().len(), 3);
        assert_eq!(
            mode(v),
            HarvestMode {
                enabled: true,
                choice: HarvestFilterChoice::New
            }
        );
        select(v, HarvestFilterChoice::Touched, cx);
    });
    settle(&browser, cx);
    browser.update(cx, |v, cx| {
        assert!(v.view_model.rows().is_empty());
        toggle(v, cx);
        assert_eq!(
            mode(v),
            HarvestMode {
                enabled: false,
                choice: HarvestFilterChoice::Touched
            }
        );
        assert_eq!(v.view_model.rows().len(), 3);
        select(v, HarvestFilterChoice::All, cx);
        assert_eq!(v.view_model.rows().len(), 3);
        assert!(v.harvest_filter.pending.is_none());
        let mut other = SourceSession::new(fixture.root.path().into());
        other.source_id = Some(yield_library::sample_sources::SourceId::from_string(
            "other",
        ));
        v.source_sessions.push(other);
        v.active_source = Some(1);
        cancel_for_source_change(v);
        reinstall_after_projection(v, cx);
        assert_eq!(mode(v), HarvestMode::default());
        v.active_source = Some(0);
        reinstall_after_projection(v, cx);
        assert_eq!(
            mode(v),
            HarvestMode {
                enabled: true,
                choice: HarvestFilterChoice::All
            }
        );
    });
    cx.update(|_, app| {
        app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown());
    });
    global.shutdown();
}
#[gpui::test]
fn harvest_filter_actual_mask_rejects_replaced_mapping_and_global_shutdown_before_render(
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
        select(v, HarvestFilterChoice::New, cx);
    });
    settle(&browser, cx);
    browser.update(cx, |v, cx| {
        let mut c = v.catalog_metadata.context().unwrap();
        c.entry_indices = Arc::new((*c.entry_indices).clone());
        v.catalog_metadata.install_projection(c);
        invalidate_if_stale(v, cx);
        assert!(v.harvest_filter.ready.is_none());
        assert!(v.view_model.rows().is_empty());
    });
    settle(&browser, cx);
    global.shutdown();
    browser.update(cx, |v, cx| {
        invalidate_if_stale(v, cx);
        assert!(v.harvest_filter.ready.is_none());
        assert!(v.view_model.rows().is_empty());
        select(v, HarvestFilterChoice::All, cx);
        assert_eq!(v.view_model.rows().len(), 3);
    });
    cx.update(|_, app| {
        app.update_global::<SourceRegistryGlobal, _>(|global, _| global.shutdown());
    });
}
mod owner;
use owner::{install_owned, owner};
