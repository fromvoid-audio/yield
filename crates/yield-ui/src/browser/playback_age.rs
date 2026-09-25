//! Refresh the in-memory playback-age projection at its next bucket boundary.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::Context;

use super::Browser;

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or_default()
}

fn delay_until(next_unix_secs: i64, now_unix_secs: i64) -> Duration {
    Duration::from_secs(
        u64::try_from(next_unix_secs.saturating_sub(now_unix_secs))
            .unwrap_or(1)
            .clamp(1, 24 * 60 * 60),
    )
}

impl Browser {
    pub(super) fn reschedule_playback_age_refresh(&mut self, cx: &mut Context<Self>) {
        self.playback_age_refresh_generation = self.playback_age_refresh_generation.wrapping_add(1);
        self.playback_age_refresh_task = None;
        if self.active_source.is_none() || !self.view_model.playback_age_filter().is_enabled() {
            return;
        }
        let now = now_unix_secs();
        let Some(next) = self.view_model.next_playback_age_change(&self.entries, now) else {
            return;
        };
        let generation = self.playback_age_refresh_generation;
        let source = self.active_source;
        let scan_generation = self.scan_generation;
        self.playback_age_refresh_task = Some(cx.spawn(async move |this, cx| {
            let mut deadline = next;
            let mut delay = delay_until(deadline, now);
            loop {
                cx.background_executor().timer(delay).await;
                let next = this
                    .update(cx, |view, cx| {
                        if view.playback_age_refresh_generation != generation
                            || view.active_source != source
                            || view.scan_generation != scan_generation
                        {
                            return None;
                        }
                        let now = now_unix_secs();
                        if now < deadline {
                            return Some((deadline, delay_until(deadline, now)));
                        }
                        let before = view.view_model.rows().to_vec();
                        let previous_id = view.browser_focus_id();
                        let previous_position = view.selected_visible_position();
                        let next = view.view_model.refresh_playback_age(&view.entries, now);
                        if view.view_model.rows() != before {
                            view.refresh_collection_filter_counts();
                            view.reconcile_selection();
                            view.restore_metadata_projection_focus(
                                previous_id,
                                previous_position,
                                cx,
                            );
                            cx.notify();
                        }
                        next.map(|next| (next, delay_until(next, now)))
                    })
                    .ok()
                    .flatten();
                let Some((next_deadline, next_delay)) = next else {
                    break;
                };
                deadline = next_deadline;
                delay = next_delay;
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::delay_until;
    use std::time::Duration;

    #[test]
    fn deadline_delay_never_busy_loops() {
        assert_eq!(delay_until(10, 10), Duration::from_secs(1));
        assert_eq!(delay_until(11, 10), Duration::from_secs(1));
        assert_eq!(delay_until(20, 10), Duration::from_secs(10));
        assert_eq!(delay_until(i64::MAX, 10), Duration::from_secs(24 * 60 * 60));
    }
}
