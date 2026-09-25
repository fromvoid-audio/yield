//! Bounded history of user-facing Browser status messages.

use std::collections::{HashMap, VecDeque};

use gpui::Context;

use super::Browser;

const STATUS_LOG_LIMIT: usize = 200;
const STATUS_ENTRY_BYTES: usize = 2_048;

/// Statuses are sampled when the UI renders and immediately before a copy.
/// Only the same user-facing messages presented by Browser are included.
#[derive(Default)]
pub(super) struct StatusLog {
    last_seen: HashMap<&'static str, String>,
    entries: VecDeque<String>,
}

impl StatusLog {
    /// Seed initial idle labels without treating them as activity, then retain
    /// changed messages in order. Repeated frames do not duplicate entries.
    pub(super) fn observe(&mut self, statuses: &[(&'static str, &str)]) {
        let initial = self.last_seen.is_empty();
        for &(source, message) in statuses {
            let safe = redact_status_message(message);
            let changed = self
                .last_seen
                .get(source)
                .is_none_or(|previous| previous != &safe);
            if !changed {
                continue;
            }
            self.last_seen.insert(source, safe.clone());
            if initial || safe.trim().is_empty() {
                continue;
            }
            let entry = format!("{source}: {safe}");
            if self.entries.back().is_some_and(|last| last == &entry) {
                continue;
            }
            if self.entries.len() == STATUS_LOG_LIMIT {
                self.entries.pop_front();
            }
            self.entries.push_back(entry);
        }
    }

    pub(super) fn text(&self) -> String {
        self.entries.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

impl Browser {
    pub(super) fn observe_statuses(&mut self, list_status: Option<&str>) {
        let list_status = list_status.unwrap_or(&self.scan_status);
        self.status_log.observe(&[
            ("Playback", &self.playback_status),
            ("Source", list_status),
            ("Sources", &self.source_registry_status),
            ("Theme", &self.theme_status),
            ("Audio output", &self.audio_output_status),
            ("Volume", self.volume_status.as_deref().unwrap_or("")),
        ]);
    }

    pub(super) fn copy_status_log(&mut self, cx: &mut Context<Self>) {
        self.observe_statuses(None);
        let text = self.status_log.text();
        self.playback_status = copy_status_log_text(&text, crate::external_clipboard::copy_text);
        cx.notify();
    }
}

fn copy_status_log_text(text: &str, copy: impl FnOnce(&str) -> Result<(), String>) -> String {
    if text.is_empty() {
        return String::from("Status log is empty");
    }
    match copy(text) {
        Ok(()) => String::from("Copied status log to clipboard"),
        Err(error) => format!("Could not copy status log: {error}"),
    }
}

/// A single app-wide macOS binding. Its exact modifier check prevents the
/// shortcut from taking ordinary text editing or browser navigation keys.
pub(super) fn is_copy_shortcut(
    key: &str,
    command: bool,
    shift: bool,
    alt: bool,
    function: bool,
    held: bool,
) -> bool {
    !held && command && shift && !alt && !function && key.eq_ignore_ascii_case("l")
}

/// Keep status text on one line and remove common inline credential values.
/// The log never reads configuration, files, tracing events, or search text.
fn redact_status_message(message: &str) -> String {
    let mut safe = message
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    for prefix in [
        "access_token=",
        "api_key=",
        "password=",
        "authorization:",
        "token=",
    ] {
        let mut start = 0;
        while let Some(relative) = safe[start..].to_ascii_lowercase().find(prefix) {
            let value_start = start + relative + prefix.len();
            let value_end = safe[value_start..]
                .find(char::is_whitespace)
                .map_or(safe.len(), |end| value_start + end);
            safe.replace_range(value_start..value_end, "[redacted]");
            start = value_start + "[redacted]".len();
            if start >= safe.len() {
                break;
            }
        }
    }
    if safe.len() > STATUS_ENTRY_BYTES {
        let mut end = STATUS_ENTRY_BYTES;
        while !safe.is_char_boundary(end) {
            end -= 1;
        }
        safe.truncate(end);
        safe.push('…');
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_bounded_and_preserves_multiple_changed_messages() {
        let mut log = StatusLog::default();
        log.observe(&[("Playback", "Ready"), ("Source", "Loading")]);
        assert_eq!(log.text(), "");
        log.observe(&[("Playback", "Starting"), ("Source", "Loaded")]);
        log.observe(&[("Playback", "Starting"), ("Source", "Loaded")]);
        assert_eq!(log.text(), "Playback: Starting\nSource: Loaded");
        for index in 0..205 {
            log.observe(&[("Playback", &format!("step {index}"))]);
        }
        assert_eq!(log.entries.len(), STATUS_LOG_LIMIT);
        assert!(!log.text().contains("step 0\n"));
    }

    #[test]
    fn sensitive_inline_values_are_redacted_without_copying_other_data() {
        assert_eq!(
            redact_status_message("request token=secret password=hunter2 failed\nretry"),
            "request token=[redacted] password=[redacted] failed retry"
        );
    }

    #[test]
    fn shortcut_requires_exact_global_chord_and_not_repeat() {
        assert!(is_copy_shortcut("l", true, true, false, false, false));
        for (command, shift, alt, function, held) in [
            (false, true, false, false, false),
            (true, false, false, false, false),
            (true, true, true, false, false),
            (true, true, false, true, false),
            (true, true, false, false, true),
        ] {
            assert!(!is_copy_shortcut("l", command, shift, alt, function, held));
        }
    }

    #[test]
    fn copy_reports_empty_success_and_failure_honestly() {
        assert_eq!(
            copy_status_log_text("", |_| panic!("empty log must not touch clipboard")),
            "Status log is empty"
        );
        assert_eq!(
            copy_status_log_text("two\nentries", |text| {
                assert_eq!(text, "two\nentries");
                Ok(())
            }),
            "Copied status log to clipboard"
        );
        assert_eq!(
            copy_status_log_text("entry", |_| Err(String::from("pasteboard rejected write"))),
            "Could not copy status log: pasteboard rejected write"
        );
    }
}
