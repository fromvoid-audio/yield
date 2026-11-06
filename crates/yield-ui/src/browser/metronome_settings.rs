//! Main Settings controls for the metronome's independent click mix.

use gpui::{Context, Div, FocusHandle, Window, div, prelude::*, px};
use reson::{MetronomeSound, PlaybackMetronomeMix};
use yield_design::Theme;
use yield_session::{MetronomePreferences, MetronomeSoundPreference, PreferencePatch};

use super::{Browser, general_settings::control, volume};
use crate::theme::color;

pub(super) const VOLUME: &str = "settings-metronome-volume";
pub(super) const BEATS: &str = "settings-metronome-beats";
pub(super) const OFFBEATS: &str = "settings-metronome-offbeats";
pub(super) const SOUND: &str = "settings-metronome-sound";
pub(super) const SOUNDS: [(&str, &str, MetronomeSoundPreference); 4] = [
    (
        "settings-metronome-classic",
        "Classic click",
        MetronomeSoundPreference::Classic,
    ),
    (
        "settings-metronome-woodblock",
        "Woodblock",
        MetronomeSoundPreference::Woodblock,
    ),
    (
        "settings-metronome-beep",
        "Electronic beep",
        MetronomeSoundPreference::ElectronicBeep,
    ),
    (
        "settings-metronome-rimshot",
        "Rimshot",
        MetronomeSoundPreference::Rimshot,
    ),
];

pub(super) struct State {
    pub(super) mix: MetronomePreferences,
    pub(super) volume_focus: FocusHandle,
    beats_focus: FocusHandle,
    offbeats_focus: FocusHandle,
    sound_focus: FocusHandle,
    menu: Option<usize>,
}

impl State {
    pub(super) fn new(cx: &mut Context<Browser>) -> Self {
        Self {
            mix: MetronomePreferences::default(),
            volume_focus: cx.focus_handle().tab_stop(true),
            beats_focus: cx.focus_handle().tab_stop(true),
            offbeats_focus: cx.focus_handle().tab_stop(true),
            sound_focus: cx.focus_handle().tab_stop(true),
            menu: None,
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn focus_handles(&self) -> [(&'static str, FocusHandle); 4] {
        [
            (VOLUME, self.volume_focus.clone()),
            (BEATS, self.beats_focus.clone()),
            (OFFBEATS, self.offbeats_focus.clone()),
            (SOUND, self.sound_focus.clone()),
        ]
    }

    pub(super) fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    pub(super) fn close_menu(&mut self) {
        self.menu = None;
    }
}

fn sound_index(sound: MetronomeSoundPreference) -> usize {
    match sound {
        MetronomeSoundPreference::Classic => 0,
        MetronomeSoundPreference::Woodblock => 1,
        MetronomeSoundPreference::ElectronicBeep => 2,
        MetronomeSoundPreference::Rimshot => 3,
    }
}

pub(super) fn sound_label(sound: MetronomeSoundPreference) -> &'static str {
    SOUNDS[sound_index(sound)].1
}

pub(super) fn volume_percent(value: f32) -> u8 {
    (volume::sanitize_volume(value) * 100.0).round() as u8
}

fn reson_mix(prefs: MetronomePreferences) -> PlaybackMetronomeMix {
    let sound = match prefs.sound {
        MetronomeSoundPreference::Classic => MetronomeSound::Classic,
        MetronomeSoundPreference::Woodblock => MetronomeSound::Woodblock,
        MetronomeSoundPreference::ElectronicBeep => MetronomeSound::ElectronicBeep,
        MetronomeSoundPreference::Rimshot => MetronomeSound::Rimshot,
    };
    PlaybackMetronomeMix::new(
        volume_percent(prefs.volume),
        prefs.beats,
        prefs.offbeats,
        sound,
    )
}

impl Browser {
    pub(super) fn apply_metronome_preferences(&mut self, prefs: MetronomePreferences) {
        self.metronome_settings.mix = prefs;
        self.sync_runtime_metronome();
    }

    pub(super) fn sync_runtime_metronome(&self) {
        if let Some(runtime) = &self.playback {
            runtime
                .handle
                .set_metronome_mix(reson_mix(self.metronome_settings.mix));
        }
    }

    fn edit_metronome(&mut self, prefs: MetronomePreferences, cx: &mut Context<Self>) {
        if prefs != self.metronome_settings.mix
            && self.submit_preference_patch(PreferencePatch::SetMetronome(prefs), cx)
        {
            self.apply_metronome_preferences(prefs);
        }
        cx.notify();
    }

    pub(super) fn set_metronome_volume(&mut self, value: f32, cx: &mut Context<Self>) {
        let prefs = MetronomePreferences {
            volume: f32::from(volume_percent(value)) / 100.0,
            ..self.metronome_settings.mix
        };
        self.edit_metronome(prefs, cx);
    }

    pub(super) fn toggle_metronome_layer(&mut self, offbeats: bool, cx: &mut Context<Self>) {
        let mut prefs = self.metronome_settings.mix;
        if offbeats {
            prefs.offbeats = !prefs.offbeats;
        } else {
            prefs.beats = !prefs.beats;
        }
        self.edit_metronome(prefs, cx);
    }

    pub(super) fn toggle_metronome_sound_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        window.focus(&self.metronome_settings.sound_focus);
        self.metronome_settings.menu = if self.metronome_settings.menu_open() {
            None
        } else {
            Some(sound_index(self.metronome_settings.mix.sound))
        };
        cx.notify();
    }

    pub(super) fn select_metronome_sound(
        &mut self,
        sound: MetronomeSoundPreference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.metronome_settings.close_menu();
        window.focus(&self.metronome_settings.sound_focus);
        self.edit_metronome(
            MetronomePreferences {
                sound,
                ..self.metronome_settings.mix
            },
            cx,
        );
    }

    pub(super) fn handle_metronome_settings_key(
        &mut self,
        key: &str,
        shift: bool,
        held: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.theme_settings_open {
            return false;
        }
        if self.metronome_settings.menu_open() && matches!(key, "escape" | "tab") {
            self.metronome_settings.close_menu();
            cx.notify();
            return key == "escape";
        }
        let volume_focused = self.metronome_settings.volume_focus.is_focused(window);
        if matches!(key, "escape" | "tab") {
            return key == "escape" && volume_focused && self.cancel_volume_drag(window, cx);
        }
        if volume_focused {
            if let Some(value) =
                volume::keyboard_volume(self.metronome_settings.mix.volume, key, shift)
            {
                self.set_metronome_volume(value, cx);
            }
            return true;
        }
        let activate = !held && matches!(key, "enter" | "return" | "space");
        if self.metronome_settings.sound_focus.is_focused(window) {
            if let Some(index) = self.metronome_settings.menu {
                let offset = match key {
                    "arrowup" | "up" => SOUNDS.len() - 1,
                    "arrowdown" | "down" => 1,
                    _ if activate => {
                        self.select_metronome_sound(SOUNDS[index].2, window, cx);
                        return true;
                    }
                    _ => return true,
                };
                self.metronome_settings.menu = Some((index + offset) % SOUNDS.len());
                cx.notify();
                return true;
            }
            if activate || matches!(key, "arrowdown" | "down") && !held {
                self.toggle_metronome_sound_menu(window, cx);
            }
            return true;
        }
        let offbeats = self.metronome_settings.offbeats_focus.is_focused(window);
        let layer_focused = offbeats || self.metronome_settings.beats_focus.is_focused(window);
        if activate && layer_focused {
            self.toggle_metronome_layer(offbeats, cx);
        }
        layer_focused
    }
}

pub(super) fn render(browser: &Browser, theme: &Theme, cx: &mut Context<Browser>) -> Div {
    let state = &browser.metronome_settings;
    let editable = browser.preferences_editable();
    let toggles = [
        (
            BEATS,
            state.mix.beats,
            &state.beats_focus,
            false,
            "Beats on",
            "Beats off",
        ),
        (
            OFFBEATS,
            state.mix.offbeats,
            &state.offbeats_focus,
            true,
            "Offbeats on",
            "Offbeats off",
        ),
    ];
    div()
        .flex()
        .flex_col()
        .gap(px(theme.spacing.control_gap))
        .child("Metronome")
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(theme.spacing.control_gap))
                .child(volume::render_metronome_slider(browser, theme.clone(), cx))
                .child(format!("{}%", volume_percent(state.mix.volume)))
                .children(
                    toggles
                        .into_iter()
                        .map(|(id, enabled, focus, offbeats, on, off)| {
                            control(browser, id, if enabled { on } else { off }, editable, theme)
                                .track_focus(focus)
                                .when(editable, |button| {
                                    button.on_click(cx.listener(move |view, _, window, cx| {
                                        let focus = if offbeats {
                                            &view.metronome_settings.offbeats_focus
                                        } else {
                                            &view.metronome_settings.beats_focus
                                        };
                                        window.focus(focus);
                                        view.toggle_metronome_layer(offbeats, cx);
                                    }))
                                })
                        }),
                ),
        )
        .child(render_sound_menu(browser, editable, theme, cx))
}

fn render_sound_menu(
    browser: &Browser,
    editable: bool,
    theme: &Theme,
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    let state = &browser.metronome_settings;
    div()
        .id("metronome-sound-menu")
        .flex()
        .flex_col()
        .items_start()
        .gap(px(1.0))
        .on_mouse_down_out(cx.listener(|view, _: &gpui::MouseDownEvent, _, cx| {
            if view.metronome_settings.menu_open() {
                view.metronome_settings.close_menu();
                cx.notify();
            }
        }))
        .child(
            control(
                browser,
                SOUND,
                sound_label(state.mix.sound),
                editable,
                theme,
            )
            .track_focus(&state.sound_focus)
            .gap(px(theme.spacing.control_gap))
            .child(if state.menu_open() { "▴" } else { "▾" })
            .when(editable, |button| {
                button.on_click(cx.listener(|view, _, window, cx| {
                    view.toggle_metronome_sound_menu(window, cx);
                }))
            }),
        )
        .when_some(state.menu.filter(|_| editable), |element, highlighted| {
            element.children(
                SOUNDS
                    .into_iter()
                    .enumerate()
                    .map(|(index, (id, label, sound))| {
                        control(browser, id, label, true, theme)
                            .when(index == highlighted, |option| {
                                option.bg(color(theme.interaction.hover_fill))
                            })
                            .child(if sound == state.mix.sound { "✓" } else { "" })
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.select_metronome_sound(sound, window, cx);
                            }))
                    }),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::source_registry::SourceRegistryGlobal;
    use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext, point};

    fn click(cx: &mut VisualTestContext, selector: &'static str, x_fraction: f32) {
        let bounds = cx.debug_bounds(selector).expect("visible setting");
        let position = point(
            bounds.origin.x + bounds.size.width * x_fraction,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn main_settings_routes_metronome_controls_to_the_profile_owner(cx: &mut TestAppContext) {
        let profile = tempfile::tempdir().expect("isolated profile");
        cx.set_global(crate::theme::ActiveTheme::default());
        cx.set_global(SourceRegistryGlobal::for_preferences_fixture(
            profile.path().join("profiles").join("gpui-development"),
        ));
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|_, app| {
            browser.update(app, |view, cx| {
                view.theme_settings_open = true;
                cx.notify();
            });
        });
        cx.run_until_parked();

        click(cx, VOLUME, 0.25);
        cx.simulate_keystrokes("right");
        click(cx, BEATS, 0.5);
        click(cx, OFFBEATS, 0.5);
        cx.simulate_keystrokes("space");
        click(cx, SOUND, 0.5);
        for (id, _, _) in SOUNDS {
            assert!(cx.debug_bounds(id).is_some(), "{id}");
        }
        cx.simulate_keystrokes("down down enter");
        cx.run_until_parked();
        cx.update(|window, app| {
            let view = browser.read(app);
            let mix = view.preferences.effective().metronome;
            assert_eq!(mix.volume, 0.30);
            assert!(!mix.beats);
            assert!(mix.offbeats);
            assert_eq!(mix.sound, MetronomeSoundPreference::ElectronicBeep);
            assert_eq!(view.metronome_settings.mix, mix);
            assert_eq!(view.volume, 1.0);
            assert!(!view.metronome_settings.menu_open());
            assert!(view.metronome_settings.sound_focus.is_focused(window));
        });
        cx.simulate_keystrokes("enter up escape");
        cx.update(|_, app| {
            let view = browser.read(app);
            assert!(view.theme_settings_open);
            assert!(!view.metronome_settings.menu_open());
            assert_eq!(
                view.metronome_settings.mix.sound,
                MetronomeSoundPreference::ElectronicBeep
            );
            app.update_global::<SourceRegistryGlobal, _>(|service, _| service.shutdown());
        });
    }
}
