#![deny(missing_docs)]
#![deny(warnings)]

//! Geometry and retained painting for edit-selection fade controls.

use std::{cell::RefCell, rc::Rc};

use gpui::{
    Bounds, Hsla, Path, PathBuilder, Pixels, Point, Window, canvas, fill, point, prelude::*, px,
};
use wavecrate_core::{
    WaveformEditFade, WaveformEditFadeHandle, WaveformEditRegion, WaveformEditSettings,
    WaveformViewport,
};

const HANDLE_SIZE: f32 = 10.0;
const OUTER_GAIN_HIT_SIZE: f32 = 18.0;
const CURVE_STROKE_WIDTH: f32 = 2.0;
const INNER_REGION_ALPHA: f32 = 52.0 / 255.0;
const OUTER_REGION_ALPHA: f32 = 38.0 / 255.0;
const HANDLE_ALPHA: f32 = 205.0 / 255.0;
const HANDLE_HOVER_ALPHA: f32 = 1.0;
const CURVE_ALPHA: f32 = 225.0 / 255.0;

/// Retained geometry for one exact waveform layout and edit-fade state.
///
/// The GPUI canvas is rebuilt with each surrounding view render. Keeping its paths and handle
/// rectangles here avoids rebuilding curve geometry on playhead, hover, and status updates.
#[derive(Clone, Default)]
pub(super) struct WaveformFadeRenderCache {
    state: Rc<RefCell<WaveformFadeRenderCacheState>>,
}

#[derive(Default)]
struct WaveformFadeRenderCacheState {
    key: Option<WaveformFadeRenderCacheKey>,
    geometry: Rc<WaveformFadePaintGeometry>,
    #[cfg(test)]
    rebuild_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct WaveformFadeRenderCacheKey {
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    region: WaveformEditRegion,
    settings: WaveformEditSettings,
}

#[derive(Clone, Default)]
struct WaveformFadePaintGeometry {
    standard: Vec<HandleRect>,
    outer_gain: Vec<HandleRect>,
    curves: Vec<Path<Pixels>>,
}

impl WaveformFadeRenderCache {
    fn geometry_for(
        &self,
        bounds: Bounds<Pixels>,
        region: WaveformEditRegion,
        settings: WaveformEditSettings,
        viewport: WaveformViewport,
    ) -> Rc<WaveformFadePaintGeometry> {
        let key = WaveformFadeRenderCacheKey {
            bounds,
            viewport,
            region,
            settings,
        };
        let mut state = self.state.borrow_mut();
        if state.key != Some(key) {
            state.geometry = Rc::new(WaveformFadePaintGeometry {
                standard: standard_handle_rects(region, settings, viewport, bounds, HANDLE_SIZE),
                outer_gain: outer_gain_handle_rects(
                    region,
                    settings,
                    viewport,
                    bounds,
                    HANDLE_SIZE,
                ),
                curves: curve_paths(bounds, region, settings, viewport),
            });
            state.key = Some(key);
            #[cfg(test)]
            {
                state.rebuild_count = state.rebuild_count.saturating_add(1);
            }
        }
        Rc::clone(&state.geometry)
    }

    #[cfg(test)]
    fn rebuild_count(&self) -> usize {
        self.state.borrow().rebuild_count
    }
}

/// One fade control hit target, including the two vertical outer-gain controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WaveformEditFadeHit {
    /// A standard inner or outer endpoint handle.
    Standard(WaveformEditFadeHandle),
    /// The outer-gain handle at the leading or trailing edge.
    OuterGain {
        /// Whether this handle belongs to the leading fade-in.
        leading: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HandleRect {
    hit: WaveformEditFadeHit,
    rect: Bounds<Pixels>,
}

/// Return the highest-priority fade control under a pointer.
///
/// Outer-gain targets intentionally precede standard handles. Standard handles follow the native
/// order so a coincident endpoint remains deterministic.
pub(super) fn handle_at(
    region: WaveformEditRegion,
    settings: WaveformEditSettings,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
) -> Option<WaveformEditFadeHit> {
    outer_gain_handle_rects(region, settings, viewport, bounds, OUTER_GAIN_HIT_SIZE)
        .into_iter()
        .find(|entry| entry.rect.contains(&position))
        .map(|entry| entry.hit)
        .or_else(|| {
            standard_handle_rects(region, settings, viewport, bounds, HANDLE_SIZE)
                .into_iter()
                .find(|entry| entry.rect.contains(&position))
                .map(|entry| entry.hit)
        })
}

/// Paint the native edit-fade regions, curves, and endpoint controls over a waveform.
pub(super) fn render_overlay(
    region: WaveformEditRegion,
    settings: WaveformEditSettings,
    viewport: WaveformViewport,
    base_color: Hsla,
    hovered: Option<WaveformEditFadeHit>,
    cache: WaveformFadeRenderCache,
) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let geometry = cache.geometry_for(bounds, region, settings, viewport);
            paint_regions(bounds, region, settings, viewport, base_color, window);
            for path in &geometry.curves {
                window.paint_path(path.clone(), base_color.opacity(CURVE_ALPHA));
            }
            for entry in &geometry.standard {
                let alpha = if hovered == Some(entry.hit) {
                    HANDLE_HOVER_ALPHA
                } else {
                    HANDLE_ALPHA
                };
                window.paint_quad(fill(entry.rect, base_color.opacity(alpha)));
            }
            for entry in &geometry.outer_gain {
                let alpha = if hovered == Some(entry.hit) {
                    HANDLE_HOVER_ALPHA
                } else {
                    HANDLE_ALPHA
                };
                window.paint_quad(fill(entry.rect, base_color.opacity(alpha)));
            }
        },
    )
    .absolute()
    .top_0()
    .bottom_0()
    .left_0()
    .right_0()
}

fn paint_regions(
    bounds: Bounds<Pixels>,
    region: WaveformEditRegion,
    settings: WaveformEditSettings,
    viewport: WaveformViewport,
    base_color: Hsla,
    window: &mut Window,
) {
    let Some(audio_region) = region.clamped_to_audio() else {
        return;
    };
    let width = audio_region.width();
    if !width.is_finite() || width <= 0.0 {
        return;
    }
    if let Some(fade) = settings.fade_in().filter(|fade| fade.length() > 0.0) {
        paint_range(
            bounds,
            viewport,
            audio_region.start(),
            audio_region.start() + width * f64::from(fade.length()),
            base_color.opacity(INNER_REGION_ALPHA),
            window,
        );
    }
    if let Some(fade) = settings.fade_out().filter(|fade| fade.length() > 0.0) {
        paint_range(
            bounds,
            viewport,
            audio_region.end() - width * f64::from(fade.length()),
            audio_region.end(),
            base_color.opacity(INNER_REGION_ALPHA),
            window,
        );
    }
    let region_width = region.width();
    if !region_width.is_finite() || region_width <= 0.0 {
        return;
    }
    if let Some(fade) = settings.fade_in().filter(|fade| fade.mute() > 0.0) {
        paint_range(
            bounds,
            viewport,
            region.start() - region_width * f64::from(fade.mute()),
            region.start(),
            base_color.opacity(OUTER_REGION_ALPHA),
            window,
        );
    }
    if let Some(fade) = settings.fade_out().filter(|fade| fade.mute() > 0.0) {
        paint_range(
            bounds,
            viewport,
            region.end(),
            region.end() + region_width * f64::from(fade.mute()),
            base_color.opacity(OUTER_REGION_ALPHA),
            window,
        );
    }
}

fn paint_range(
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    start: f64,
    end: f64,
    color: Hsla,
    window: &mut Window,
) {
    let Some((visible_start, visible_end)) = viewport.visible_range_from_absolute(start, end)
    else {
        return;
    };
    let width = f32::from(bounds.size.width);
    if !width.is_finite() || width <= 0.0 {
        return;
    }
    let left = bounds.left() + px(width * visible_start as f32);
    let right = bounds.left() + px(width * visible_end as f32);
    if right <= left {
        return;
    }
    let rect = Bounds::from_corners(point(left, bounds.top()), point(right, bounds.bottom()));
    window.paint_quad(fill(rect, color));
}

fn standard_handle_rects(
    region: WaveformEditRegion,
    settings: WaveformEditSettings,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    size: f32,
) -> Vec<HandleRect> {
    let Some(audio_region) = region.clamped_to_audio() else {
        return Vec::new();
    };
    if !valid_size(size, bounds) {
        return Vec::new();
    }
    let size = size
        .min(f32::from(bounds.size.width))
        .min(f32::from(bounds.size.height));
    let start = audio_region.start();
    let end = audio_region.end();
    let width = audio_region.width();
    let fade_in = settings.fade_in();
    let fade_out = settings.fade_out();
    let mut handles = Vec::with_capacity(6);
    // Keep this order aligned with TimelineEditPreview::standard_order.
    push_standard(
        &mut handles,
        WaveformEditFadeHandle::InEnd,
        if let Some(fade) = fade_in {
            start + width * f64::from(fade.length())
        } else {
            start
        },
        HandleBand::Top,
        viewport,
        bounds,
        size,
    );
    push_standard(
        &mut handles,
        WaveformEditFadeHandle::OutStart,
        if let Some(fade) = fade_out {
            end - width * f64::from(fade.length())
        } else {
            end
        },
        HandleBand::Top,
        viewport,
        bounds,
        size,
    );
    if fade_in.is_some() {
        push_standard(
            &mut handles,
            WaveformEditFadeHandle::InStart,
            start,
            HandleBand::Bottom,
            viewport,
            bounds,
            size,
        );
    }
    if fade_out.is_some() {
        push_standard(
            &mut handles,
            WaveformEditFadeHandle::OutEnd,
            end,
            HandleBand::Bottom,
            viewport,
            bounds,
            size,
        );
    }
    if fade_in.is_some() {
        push_standard(
            &mut handles,
            WaveformEditFadeHandle::InOuterStart,
            region.start()
                - region.width() * f64::from(fade_in.map_or(0.0, WaveformEditFade::mute)),
            HandleBand::Center,
            viewport,
            bounds,
            size,
        );
    }
    if fade_out.is_some() {
        push_standard(
            &mut handles,
            WaveformEditFadeHandle::OutOuterEnd,
            region.end() + region.width() * f64::from(fade_out.map_or(0.0, WaveformEditFade::mute)),
            HandleBand::Center,
            viewport,
            bounds,
            size,
        );
    }
    handles
}

#[derive(Clone, Copy)]
enum HandleBand {
    Top,
    Bottom,
    Center,
}

fn push_standard(
    handles: &mut Vec<HandleRect>,
    handle: WaveformEditFadeHandle,
    absolute_ratio: f64,
    band: HandleBand,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    size: f32,
) {
    let Some(visible_ratio) = viewport.visible_ratio_from_absolute(absolute_ratio) else {
        return;
    };
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let left = f32::from(bounds.origin.x);
    let top = f32::from(bounds.origin.y);
    let x = (left + width * visible_ratio as f32 - size * 0.5).clamp(left, left + width - size);
    let y = match band {
        HandleBand::Top => top,
        HandleBand::Bottom => top + height - size,
        HandleBand::Center => top + (height - size) * 0.5,
    };
    let rect = Bounds::from_corners(point(px(x), px(y)), point(px(x + size), px(y + size)));
    handles.push(HandleRect {
        hit: WaveformEditFadeHit::Standard(handle),
        rect,
    });
}

fn outer_gain_handle_rects(
    region: WaveformEditRegion,
    settings: WaveformEditSettings,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    size: f32,
) -> Vec<HandleRect> {
    if !valid_size(size, bounds) {
        return Vec::new();
    }
    let size = size
        .min(f32::from(bounds.size.width))
        .min(f32::from(bounds.size.height));
    let width = region.width();
    if !width.is_finite() || width <= 0.0 {
        return Vec::new();
    }
    let mut handles = Vec::with_capacity(2);
    if let Some(fade) = settings.fade_in().filter(|fade| fade.mute() > 0.0) {
        if let Some(rect) = outer_gain_handle_rect(
            region.start() - width * f64::from(fade.mute()),
            fade.outer_gain(),
            WaveformEditFadeHit::OuterGain { leading: true },
            viewport,
            bounds,
            size,
        ) {
            handles.push(rect);
        }
    }
    if let Some(fade) = settings.fade_out().filter(|fade| fade.mute() > 0.0) {
        if let Some(rect) = outer_gain_handle_rect(
            region.end() + width * f64::from(fade.mute()),
            fade.outer_gain(),
            WaveformEditFadeHit::OuterGain { leading: false },
            viewport,
            bounds,
            size,
        ) {
            handles.push(rect);
        }
    }
    handles
}

fn outer_gain_handle_rect(
    absolute_ratio: f64,
    outer_gain: f32,
    hit: WaveformEditFadeHit,
    viewport: WaveformViewport,
    bounds: Bounds<Pixels>,
    size: f32,
) -> Option<HandleRect> {
    let visible_ratio = viewport.visible_ratio_from_absolute(absolute_ratio)?;
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let left = f32::from(bounds.origin.x);
    let top = f32::from(bounds.origin.y);
    let x = (left + width * visible_ratio as f32 - size * 0.5).clamp(left, left + width - size);
    let travel = (height * 0.5 - size * 0.5).max(0.0);
    let center_y = top + size * 0.5 + travel * (1.0 - outer_gain.clamp(0.0, 1.0));
    let y = (center_y - size * 0.5).clamp(top, top + height - size);
    Some(HandleRect {
        hit,
        rect: Bounds::from_corners(point(px(x), px(y)), point(px(x + size), px(y + size))),
    })
}

fn valid_size(size: f32, bounds: Bounds<Pixels>) -> bool {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    size.is_finite()
        && size > 0.0
        && width.is_finite()
        && width > 0.0
        && height.is_finite()
        && height > 0.0
}

fn curve_paths(
    bounds: Bounds<Pixels>,
    region: WaveformEditRegion,
    settings: WaveformEditSettings,
    viewport: WaveformViewport,
) -> Vec<Path<Pixels>> {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
        return Vec::new();
    }
    let Some(_audio_region) = region.clamped_to_audio() else {
        return Vec::new();
    };
    let region_width = region.width();
    let steps = ((width / 6.0).ceil() as usize).clamp(2, 256);
    let mut paths = Vec::with_capacity(2);
    if let Some(fade) = settings.fade_in() {
        if let Some(path) = build_curve_path(
            bounds,
            viewport,
            region.start(),
            region.start() + region_width * f64::from(fade.length()),
            fade.curve(),
            false,
            steps,
        ) {
            paths.push(path);
        }
    }
    if let Some(fade) = settings.fade_out() {
        if let Some(path) = build_curve_path(
            bounds,
            viewport,
            region.end() - region_width * f64::from(fade.length()),
            region.end(),
            fade.curve(),
            true,
            steps,
        ) {
            paths.push(path);
        }
    }
    paths
}

fn build_curve_path(
    bounds: Bounds<Pixels>,
    viewport: WaveformViewport,
    start: f64,
    end: f64,
    curve: f32,
    trailing: bool,
    steps: usize,
) -> Option<Path<Pixels>> {
    let start = start.clamp(0.0, 1.0);
    let end = end.clamp(0.0, 1.0);
    viewport.visible_range_from_absolute(start, end)?;
    if end - start <= f64::from(f32::EPSILON) || steps < 2 {
        return None;
    }
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let left = f32::from(bounds.origin.x);
    let top = f32::from(bounds.origin.y);
    let bottom = top + height;
    let mut path = PathBuilder::stroke(px(CURVE_STROKE_WIDTH));
    let mut points = 0;
    for index in 0..steps {
        let t = index as f64 / (steps - 1) as f64;
        let ratio = start + (end - start) * t;
        let Some(visible) = viewport.visible_ratio_from_absolute(ratio) else {
            continue;
        };
        let local_t = t as f32;
        let value = fade_curve_value(local_t, curve);
        let value = if trailing { 1.0 - value } else { value };
        let x = left + width * visible as f32;
        let y = bottom - height * value;
        if points == 0 {
            path.move_to(point(px(x), px(y)));
        } else {
            path.line_to(point(px(x), px(y)));
        }
        points += 1;
    }
    (points >= 2).then(|| path.build().ok()).flatten()
}

fn fade_curve_value(t: f32, curve: f32) -> f32 {
    if curve <= 0.0 {
        return t.clamp(0.0, 1.0);
    }
    let t = t.clamp(0.0, 1.0);
    let t2 = t * t;
    let t3 = t2 * t;
    let smootherstep = t3 * (t * (t * 6.0 - 15.0) + 10.0);
    t * (1.0 - curve) + smootherstep * curve
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn bounds(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), size(px(width), px(height)))
    }

    fn settings(
        fade_in: Option<WaveformEditFade>,
        fade_out: Option<WaveformEditFade>,
    ) -> WaveformEditSettings {
        WaveformEditSettings::new(1.0, fade_in, fade_out).expect("finite settings")
    }

    #[test]
    fn standard_handles_follow_native_order_and_bands() {
        let region = WaveformEditRegion::new(0.2, 0.8).expect("region");
        let fade_in = WaveformEditFade::new(0.25, 0.5, 0.1, 0.4).expect("fade");
        let fade_out = WaveformEditFade::new(0.3, 0.2, 0.2, 0.7).expect("fade");
        let rects = standard_handle_rects(
            region,
            settings(Some(fade_in), Some(fade_out)),
            WaveformViewport::full(),
            bounds(200.0, 100.0),
            HANDLE_SIZE,
        );
        let handles = rects.iter().map(|entry| entry.hit).collect::<Vec<_>>();
        assert_eq!(
            handles,
            vec![
                WaveformEditFadeHit::Standard(WaveformEditFadeHandle::InEnd),
                WaveformEditFadeHit::Standard(WaveformEditFadeHandle::OutStart),
                WaveformEditFadeHit::Standard(WaveformEditFadeHandle::InStart),
                WaveformEditFadeHit::Standard(WaveformEditFadeHandle::OutEnd),
                WaveformEditFadeHit::Standard(WaveformEditFadeHandle::InOuterStart),
                WaveformEditFadeHit::Standard(WaveformEditFadeHandle::OutOuterEnd),
            ]
        );
        assert_eq!(f32::from(rects[0].rect.origin.y), 0.0);
        assert_eq!(f32::from(rects[2].rect.origin.y), 90.0);
        assert_eq!(f32::from(rects[4].rect.origin.y), 45.0);
    }

    #[test]
    fn outer_gain_hit_uses_larger_target_and_vertical_mapping() {
        let region = WaveformEditRegion::new(0.25, 0.75).expect("region");
        let fade = WaveformEditFade::new(0.2, 0.5, 0.25, 0.0).expect("fade");
        let initial = settings(Some(fade), None);
        let viewport = WaveformViewport::full();
        let bounds = bounds(200.0, 100.0);
        let hit = handle_at(region, initial, viewport, bounds, point(px(25.0), px(58.0)));
        assert_eq!(hit, Some(WaveformEditFadeHit::OuterGain { leading: true }));
        assert_eq!(
            handle_at(region, initial, viewport, bounds, point(px(25.0), px(80.0))),
            None
        );
    }

    #[test]
    fn standard_handles_clip_audio_margin_and_hide_offscreen_coordinates() {
        let region = WaveformEditRegion::new(-0.4, 0.4).expect("region");
        let fade = WaveformEditFade::new(0.2, 0.5, 0.0, 1.0).expect("fade");
        let rects = standard_handle_rects(
            region,
            settings(Some(fade), None),
            WaveformViewport::full(),
            bounds(200.0, 100.0),
            HANDLE_SIZE,
        );
        assert!(rects.iter().all(|entry| {
            let x = f32::from(entry.rect.origin.x);
            (0.0..=190.0).contains(&x)
        }));
        let hidden = standard_handle_rects(
            WaveformEditRegion::new(1.1, 1.2).expect("region"),
            settings(Some(fade), None),
            WaveformViewport::full(),
            bounds(200.0, 100.0),
            HANDLE_SIZE,
        );
        assert!(hidden.is_empty());
    }

    #[test]
    fn fade_curve_matches_native_linear_and_s_curve_values() {
        assert!((fade_curve_value(0.25, 0.0) - 0.25).abs() < 1.0e-6);
        assert!((fade_curve_value(0.25, 1.0) - 0.103515625).abs() < 1.0e-6);
        assert!((fade_curve_value(0.25, 0.5) - 0.1767578125).abs() < 1.0e-6);
    }

    #[test]
    fn curve_span_clamps_original_edit_bounds_before_painting() {
        let leading = WaveformEditFade::new(0.1, 0.5, 0.0, 1.0).unwrap();
        let fully_silent_leading = WaveformEditRegion::new(-0.2, 0.8).unwrap();
        assert!(
            curve_paths(
                bounds(200.0, 100.0),
                fully_silent_leading,
                settings(Some(leading), None),
                WaveformViewport::full(),
            )
            .is_empty()
        );

        let visible_leading = WaveformEditRegion::new(-0.2, 0.8).unwrap();
        let leading = WaveformEditFade::new(0.5, 0.5, 0.0, 1.0).unwrap();
        assert_eq!(
            curve_paths(
                bounds(200.0, 100.0),
                visible_leading,
                settings(Some(leading), None),
                WaveformViewport::full(),
            )
            .len(),
            1
        );
    }

    #[test]
    fn retained_geometry_reuses_exact_layout_and_invalidates_effect_changes() {
        let cache = WaveformFadeRenderCache::default();
        let region = WaveformEditRegion::new(0.2, 0.8).expect("region");
        let fade = WaveformEditFade::new(0.3, 0.0, 0.1, 0.5).expect("fade");
        let initial = settings(Some(fade), None);
        let bounds = bounds(200.0, 100.0);
        let viewport = WaveformViewport::full();
        let first = cache.geometry_for(bounds, region, initial, viewport);
        let second = cache.geometry_for(bounds, region, initial, viewport);
        assert!(Rc::ptr_eq(&first, &second));
        assert_eq!(cache.rebuild_count(), 1);

        let changed = settings(
            Some(WaveformEditFade::new(0.3, 1.0, 0.1, 0.5).unwrap()),
            None,
        );
        let third = cache.geometry_for(bounds, region, changed, viewport);
        assert!(!Rc::ptr_eq(&first, &third));
        assert_eq!(cache.rebuild_count(), 2);
    }
}
