//! Bounded history of user-facing Browser status messages.

use std::collections::{HashMap, VecDeque};

use gpui::Context;

use super::Browser;

const STATUS_LOG_LIMIT: usize = 200;
const STATUS_ENTRY_BYTES: usize = 2_048;

/// Status writes enter immediately; rendering also captures derived visible
/// labels such as source errors. No private configuration or trace file is read.
#[derive(Default)]
pub(super) struct StatusLog {
    last_seen: HashMap<&'static str, String>,
    entries: VecDeque<String>,
}

impl StatusLog {
    /// Seed initial idle labels without treating them as activity, then retain
    /// changed messages in order. Repeated frames do not duplicate entries.
    pub(super) fn observe(&mut self, statuses: &[(&'static str, &str)]) {
        for &(source, message) in statuses {
            let safe = redact_status_message(message);
            let Some(previous) = self.last_seen.get(source) else {
                self.last_seen.insert(source, safe);
                continue;
            };
            if previous == &safe {
                continue;
            }
            self.push_safe_event(source, safe);
        }
    }

    fn push_event(&mut self, source: &'static str, message: &str) {
        self.push_safe_event(source, redact_status_message(message));
    }

    fn push_safe_event(&mut self, source: &'static str, safe: String) {
        self.last_seen.insert(source, safe.clone());
        if safe.trim().is_empty() {
            return;
        }
        let entry = format!("{source}: {safe}");
        if self.entries.back().is_some_and(|last| last == &entry) {
            return;
        }
        if self.entries.len() == STATUS_LOG_LIMIT {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub(super) fn text(&self) -> String {
        self.entries.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

impl Browser {
    pub(super) fn set_playback_status(&mut self, value: impl Into<String>) {
        let value = value.into();
        if self.playback_status != value {
            self.status_log.push_event("Playback", &value);
        }
        self.playback_status = value;
    }

    pub(super) fn set_scan_status(&mut self, value: impl Into<String>) {
        let value = value.into();
        if self.scan_status != value {
            self.status_log.push_event("Source", &value);
        }
        self.scan_status = value;
    }

    pub(super) fn set_source_registry_status(&mut self, value: impl Into<String>) {
        let value = value.into();
        if self.source_registry_status != value {
            self.status_log.push_event("Sources", &value);
        }
        self.source_registry_status = value;
    }

    pub(super) fn set_theme_status(&mut self, value: impl Into<String>) {
        let value = value.into();
        if self.theme_status != value {
            self.status_log.push_event("Theme", &value);
        }
        self.theme_status = value;
    }

    pub(super) fn set_audio_output_status(&mut self, value: impl Into<String>) {
        let value = value.into();
        if self.audio_output_status != value {
            self.status_log.push_event("Audio output", &value);
        }
        self.audio_output_status = value;
    }

    pub(super) fn set_volume_status(&mut self, value: Option<String>) {
        if self.volume_status != value {
            self.status_log
                .push_event("Volume", value.as_deref().unwrap_or(""));
        }
        self.volume_status = value;
    }

    pub(super) fn set_os_entry_status(&mut self, value: &'static str) {
        if self.os_entry_status != Some(value) {
            self.status_log.push_event("macOS", value);
        }
        self.os_entry_status = Some(value);
    }

    pub(super) fn observe_statuses(&mut self, list_status: Option<&str>) {
        let list_status = list_status.unwrap_or(&self.scan_status);
        self.status_log.observe(&[
            ("Playback", &self.playback_status),
            ("Source", list_status),
            ("Sources", &self.source_registry_status),
            ("Theme", &self.theme_status),
            ("Audio output", &self.audio_output_status),
            ("Volume", self.volume_status.as_deref().unwrap_or("")),
            ("macOS", self.os_entry_status.unwrap_or("")),
        ]);
    }

    pub(super) fn copy_status_log(&mut self, cx: &mut Context<Self>) {
        self.observe_statuses(None);
        let text = self.status_log.text();
        self.set_playback_status(copy_status_log_text(
            &text,
            crate::external_clipboard::copy_text,
        ));
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
    for prefix in ["authorization:", "bearer ", "cookie:"] {
        if let Some(start) = safe.to_ascii_lowercase().find(prefix) {
            safe.truncate(start + prefix.len());
            safe.push_str("[redacted]");
            return safe;
        }
    }
    for prefix in ["access_token=", "api_key=", "password=", "token="] {
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
    fn writes_between_frames_remain_in_order_and_render_does_not_duplicate_them() {
        let mut log = StatusLog::default();
        log.observe(&[("Playback", "Ready")]);
        log.push_event("Playback", "Starting audio…");
        log.push_event("Playback", "Playing");
        log.observe(&[("Playback", "Playing")]);
        assert_eq!(log.text(), "Playback: Starting audio…\nPlayback: Playing");
    }

    #[test]
    fn sensitive_inline_values_are_redacted_without_copying_other_data() {
        assert_eq!(
            redact_status_message("request token=secret password=hunter2 failed\nretry"),
            "request token=[redacted] password=[redacted] failed retry"
        );
        assert_eq!(
            redact_status_message("request Authorization: Bearer secret-value"),
            "request Authorization:[redacted]"
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
