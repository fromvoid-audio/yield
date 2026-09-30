//! Independent local filter-family controls with retained choices.

use gpui::{ClickEvent, Context, div, prelude::*, px};
use yield_core::BrowserFilterFamily;
use yield_design::Theme;

use super::Browser;
use crate::theme::color;

#[cfg(target_os = "macos")]
pub(super) const FAMILIES: [BrowserFilterFamily; 3] = [
    BrowserFilterFamily::Name,
    BrowserFilterFamily::PlaybackType,
    BrowserFilterFamily::Rating,
];

pub(super) fn id(family: BrowserFilterFamily) -> &'static str {
    match family {
        BrowserFilterFamily::Name => "browser-name-filter-toggle",
        BrowserFilterFamily::PlaybackType => "browser-type-filter-toggle",
        BrowserFilterFamily::Rating => "browser-rating-filter-toggle",
    }
}

pub(super) fn label(family: BrowserFilterFamily) -> &'static str {
    match family {
        BrowserFilterFamily::Name => "NAME",
        BrowserFilterFamily::PlaybackType => "TYPE",
        BrowserFilterFamily::Rating => "RATING",
    }
}

pub(super) fn render(
    browser: &Browser,
    theme: &Theme,
    family: BrowserFilterFamily,
    cx: &mut Context<Browser>,
) -> gpui::Stateful<gpui::Div> {
    let enabled = browser.view_model.filter_family_enabled(family);
    let hover = color(theme.colors.selection_hover_fill);
    div()
        .id(id(family))
        .debug_selector(move || id(family).into())
        .when_some(browser.ax_focus(id(family)), |element, focus| {
            element.track_focus(&focus)
        })
        .flex()
        .flex_none()
        .h(px(theme.spacing.row_height))
        .items_center()
        .px(px(theme.spacing.control_gap))
        .cursor_pointer()
        .text_size(px(theme.typography.small_size))
        .text_color(color(if enabled {
            theme.colors.accent
        } else {
            theme.colors.text_muted
        }))
        .hover(move |style| style.bg(hover))
        .when(browser.active_source.is_none(), |element| {
            element.opacity(0.45)
        })
        .when_some(browser.ax_marker(id(family)), |element, marker| {
            element.child(marker)
        })
        .on_click(
            cx.listener(move |view, _: &ClickEvent, _, cx| view.toggle_filter_family(family, cx)),
        )
        .child(format!(
            "{} {}",
            if enabled { "●" } else { "○" },
            label(family)
        ))
}

impl Browser {
    pub(super) fn toggle_filter_family(
        &mut self,
        family: BrowserFilterFamily,
        cx: &mut Context<Self>,
    ) {
        if self.active_source.is_none() {
            return;
        }
        let previous_id = self.browser_focus_id();
        let previous_position = self.selected_visible_position();
        let enabled = !self.view_model.filter_family_enabled(family);
        self.invalidate_file_handoff();
        self.context_menu = None;
        self.view_model
            .set_filter_family_enabled(&self.entries, family, enabled);
        self.reset_browser_focus_state();
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.restore_explicit_projection_focus(previous_id, previous_position, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    #[gpui::test]
    fn family_actions_require_an_active_source_and_preserve_disabled_drafts(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            view.view_model.set_query(&view.entries, "retained search");
            view.view_model.toggle_rating_filter(&view.entries, 1);
            view.view_model
                .toggle_playback_type_filter(&view.entries, yield_core::PlaybackType::Loop);
            for family in [
                BrowserFilterFamily::Name,
                BrowserFilterFamily::Rating,
                BrowserFilterFamily::PlaybackType,
            ] {
                view.toggle_filter_family(family, cx);
                assert!(view.view_model.filter_family_enabled(family));
            }
            view.active_source = Some(0);
            for family in [
                BrowserFilterFamily::Name,
                BrowserFilterFamily::Rating,
                BrowserFilterFamily::PlaybackType,
            ] {
                view.toggle_filter_family(family, cx);
                assert!(!view.view_model.filter_family_enabled(family));
            }
            assert_eq!(view.view_model.query(), "retained search");
            assert!(view.view_model.rating_filter().contains_level(1));
            assert!(
                view.view_model
                    .playback_type_filter()
                    .contains_type(yield_core::PlaybackType::Loop)
            );
        });
    }
}
