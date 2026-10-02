//! Native AX source mapping. macOS runtime qualification remains OPT-2054.
use super::*;

/// Native accessibility source mapping; runtime/AX qualification remains OPT-2054.
#[cfg(target_os = "macos")]
pub(in crate::browser) fn accessibility_controls(
    browser: &Browser,
) -> Option<Vec<yield_platform::accessibility::AxChild>> {
    use yield_platform::accessibility::{AxChild, AxRole};
    if !browser.unsupported_entries.open {
        return None;
    }
    let model = &browser.unsupported_entries.model;
    let epoch = model.page_epoch;
    let button = |id: &str, label: &str, enabled| AxChild {
        id: id.into(),
        role: AxRole::Button,
        label: label.into(),
        value: None,
        enabled,
    };
    let mut controls = vec![
        AxChild {
            id: MODAL_ID.into(),
            role: AxRole::Group,
            label: "Unsupported files".into(),
            value: Some(model.status.into()),
            enabled: true,
        },
        AxChild {
            id: STATUS_ID.into(),
            role: AxRole::Group,
            label: model.status.into(),
            value: None,
            enabled: true,
        },
        button(CLOSE_ID, "Close unsupported files", true),
        button(
            "unsupported-entries-restart",
            "Restart saved entry list",
            true,
        ),
        button(
            "unsupported-entries-back",
            "Back through at most eight pages",
            model.pending.is_none() && !model.history.is_empty(),
        ),
        button(
            "unsupported-entries-next",
            "Next saved entry page",
            model.pending.is_none() && model.page.as_ref().is_some_and(|p| p.next.is_some()),
        ),
    ];
    if let Some(page) = &model.page {
        controls.extend(
            page.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| AxChild {
                    id: format!("unsupported-entry-{epoch}-{index}"),
                    role: AxRole::Row,
                    label: display_path(&entry.relative_path),
                    value: Some(format!(
                        "{}{}",
                        if model.selected == Some(index) {
                            "Selected. "
                        } else {
                            ""
                        },
                        row_description(entry)
                    )),
                    enabled: eligible(entry),
                }),
        );
    }
    Some(controls)
}

/// Current row focus observation for native accessibility; never a filesystem grant.
#[cfg(target_os = "macos")]
pub(in crate::browser) fn accessibility_focus(
    browser: &Browser,
    window: &Window,
) -> Option<(String, bool)> {
    if !browser.unsupported_entries.open {
        return None;
    }
    let model = &browser.unsupported_entries.model;
    let id = model.selected.map_or_else(
        || MODAL_ID.into(),
        |index| format!("unsupported-entry-{}-{index}", model.page_epoch),
    );
    Some((
        id,
        browser.unsupported_entries.focus.is_focused(window) && window.is_window_active(),
    ))
}

/// Native events are denial/focus or bounded owner inspection, never path dispatch.
#[cfg(target_os = "macos")]
pub(in crate::browser) fn handle_accessibility(
    browser: &mut Browser,
    event: &yield_platform::accessibility::AxEvent,
    window: &mut Window,
    cx: &mut Context<Browser>,
) -> bool {
    use yield_platform::accessibility::AxAction;
    if event.id == "open-unsupported-entries" {
        if event.action == AxAction::Press {
            browser.open_unsupported_files(window, cx);
        } else if event.action == AxAction::Focus {
            window.focus(&browser.focus_handle);
        }
        return true;
    }
    if !browser.unsupported_entries.open {
        return false;
    }
    window.focus(&browser.unsupported_entries.focus);
    if event.action == AxAction::Press {
        match event.id.as_str() {
            CLOSE_ID => browser.close_unsupported_files(window, cx),
            "unsupported-entries-restart" => browser.restart_unsupported_files(cx),
            "unsupported-entries-back" | "unsupported-entries-next" => {
                if browser
                    .unsupported_entries
                    .model
                    .change_page(event.id == "unsupported-entries-next")
                {
                    retry(browser, cx);
                    cx.notify();
                }
            }
            _ => {}
        }
    }
    if let Some((epoch, index)) = event
        .id
        .strip_prefix("unsupported-entry-")
        .and_then(|tail| tail.split_once('-'))
        .and_then(|(epoch, index)| Some((epoch.parse::<u64>().ok()?, index.parse::<usize>().ok()?)))
    {
        if browser
            .unsupported_entries
            .model
            .select_row_for_page(index, epoch)
        {
            browser.unsupported_entries.scroll.scroll_to_item(index);
            if event.action == AxAction::Press {
                browser.inspect_unsupported_file(cx);
            }
            cx.notify();
        }
    }
    true
}
