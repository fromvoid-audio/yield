use std::sync::Arc;

use gpui::AppContext;
use gpui::{
    Bounds, Pixels, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, point, px, size,
};
use yield_core::{AudioFile, BrowserViewModel};
use yield_design::Theme;
use yield_media::WaveformPreview;

use super::Browser;
use crate::theme::{ActiveTheme, with_text_scale};

#[gpui::test]
fn compact_browser_geometry_keeps_work_surfaces_reachable(cx: &mut TestAppContext) {
    let source = tempfile::tempdir().unwrap();
    for (width, height) in [(960.0, 560.0), (1120.0, 720.0), (1440.0, 900.0)] {
        for text_scale in [100, 200] {
            let theme = with_text_scale(&Theme::default(), text_scale);
            let row_height = theme.spacing.row_height;
            let status_height = theme.spacing.status_height;
            cx.set_global(ActiveTheme(Arc::new(theme)));
            let window_size = size(px(width), px(height));
            let window = cx.update(|app| {
                app.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(0.0), px(0.0)),
                            window_size,
                        ))),
                        ..Default::default()
                    },
                    |_, app| app.new(Browser::new),
                )
                .expect("open deterministic density fixture")
            });
            let browser = window.root(cx).unwrap();
            let mut view_cx = VisualTestContext::from_window(window.into(), cx);
            browser.update(&mut view_cx, |view, cx| {
                view.general_settings.text_scale_percent = text_scale;
                view.root = Some(source.path().to_path_buf());
                view.entries = (0..48)
                    .map(|index| {
                        let relative_path = format!("texture-{index:02}-percussion.wav").into();
                        AudioFile {
                            path: source.path().join(&relative_path),
                            relative_path,
                            byte_len: 192_000,
                            duration_seconds: Some(2.0),
                            sample_rate: Some(48_000),
                            playback_type: None,
                            curation: None,
                        }
                    })
                    .collect();
                view.selected = Some(0);
                view.view_model = BrowserViewModel::new(&view.entries);
                view.waveform = Some(WaveformPreview {
                    peaks: vec![0.5; 24],
                    bands: Vec::new(),
                    duration_seconds: 2.0,
                    sample_rate: 48_000,
                    total_frames: 96_000,
                    detail_context: None,
                });
                cx.notify();
            });
            view_cx.run_until_parked();
            view_cx.refresh().expect("render density fixture");
            view_cx.update(|window, _| assert_eq!(window.bounds().size, window_size));

            let toolbar = required_bounds(&mut view_cx, "main-toolbar");
            let waveform = required_bounds(&mut view_cx, "waveform");
            let table = required_bounds(&mut view_cx, "sample-table-viewport");
            let name = required_bounds(&mut view_cx, "browser-column-Name");
            let status = required_bounds(&mut view_cx, "main-status-bar");
            let complete_rows = ((f32::from(table.size.height) - row_height) / row_height).floor();
            println!(
                "{width}×{height} at {text_scale}% | table {:.0}×{:.0} | complete rows {:.0} | Name {:.0} | waveform {:.0}×{:.0} | toolbar {:.0} | status {:.0}",
                f32::from(table.size.width),
                f32::from(table.size.height),
                complete_rows,
                f32::from(name.size.width),
                f32::from(waveform.size.width),
                f32::from(waveform.size.height),
                f32::from(toolbar.size.height),
                f32::from(status.size.height),
            );
            assert!(complete_rows >= 1.0, "{width}×{height} at {text_scale}%");
            if width == 960.0 && height == 560.0 {
                if text_scale == 100 {
                    assert!(
                        complete_rows >= 11.0,
                        "minimum window shows {complete_rows} rows"
                    );
                } else {
                    assert!(
                        waveform.size.height >= px(40.0),
                        "enlarged waveform lost its drawing area: {waveform:?}",
                    );
                }
            }
            assert!(waveform.size.height > px(0.0), "{waveform:?}");
            assert!(
                toolbar.bottom() <= waveform.top(),
                "{toolbar:?}, {waveform:?}"
            );
            assert!(waveform.bottom() <= table.top(), "{waveform:?}, {table:?}");
            assert!(table.bottom() <= status.top(), "{table:?}, {status:?}");
            assert!(status.bottom() <= px(height), "{status:?}");
            assert_eq!(status.size.height, px(status_height));
            let details = required_bounds(&mut view_cx, "status-job-details");
            assert!(
                details.left() >= status.left()
                    && details.right() <= status.right()
                    && details.top() >= status.top()
                    && details.bottom() <= status.bottom(),
                "status details {details:?} escaped {status:?} at {width}×{height}, {text_scale}%",
            );
            let mut overflowing_controls = Vec::new();
            for selector in [
                "bpm-snap-tempo-toolbar",
                "random-audition-toolbar",
                "browser-search-toolbar",
                "browser-name-filter-toggle",
                "source-root-label",
                "audio-output-settings-toolbar",
                "reveal-toolbar",
                "choose-folder-toolbar",
                "theme-settings",
                "shortcut-help-toolbar",
            ] {
                let control = required_bounds(&mut view_cx, selector);
                if control.left() < toolbar.left()
                    || control.right() > toolbar.right()
                    || control.top() < toolbar.top()
                    || control.bottom() > toolbar.bottom()
                {
                    overflowing_controls.push(selector);
                }
            }
            assert!(
                overflowing_controls.is_empty(),
                "{overflowing_controls:?} escaped toolbar {toolbar:?} at {width}×{height}, {text_scale}%",
            );
            view_cx.update(|window, _| window.remove_window());
        }
    }
}

fn required_bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing density fixture bounds for {selector}"))
}
