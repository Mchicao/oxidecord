//! The control bar under the stage, and the microphone picker behind the
//! mute buttons.

use gpui::*;
use gpui_component::{
    Disableable as _, Icon, IconName, Selectable as _, Sizable as _, h_flex,
    menu::{ContextMenuExt as _, PopupMenuItem},
};

use crate::screens::home::HomeScreen;
use crate::ui::button::Button;
use crate::voice;

impl HomeScreen {
    pub(super) fn render_call_controls(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_shrink_0()
            .w_full()
            .py_3()
            .gap_2()
            .items_center()
            .justify_center()
            .child(
                self.with_device_menu(
                    "call-mic-menu",
                    "Input Device",
                    control(
                        "call-mute",
                        if self.voice_muted {
                            "icons/mic-off.svg"
                        } else {
                            "icons/mic.svg"
                        },
                        if self.voice_muted { "Unmute" } else { "Mute" },
                        self.voice_muted,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_voice_mute(cx))),
                    DeviceMenu::input(),
                    cx,
                ),
            )
            .child(
                self.with_device_menu(
                    "call-speaker-menu",
                    "Output Device",
                    control(
                        "call-deafen",
                        if self.voice_deafened {
                            "icons/headphone-off.svg"
                        } else {
                            "icons/headphones.svg"
                        },
                        if self.voice_deafened {
                            "Undeafen"
                        } else {
                            "Deafen"
                        },
                        self.voice_deafened,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_voice_deafen(cx))),
                    DeviceMenu::output(),
                    cx,
                ),
            )
            .child(
                div().id("camera-controls").child(control(
                    "call-camera",
                    if self.camera.active { "icons/video.svg" } else { "icons/video-off.svg" },
                    "Toggle Camera (right-click to choose a camera)",
                    self.camera.active,
                ).disabled(!self.can_use_camera())
                .on_click(cx.listener(|this, _, window, cx| this.toggle_camera(window, cx))))
                .context_menu({
                    let screen = cx.entity().downgrade();
                    move |menu, _, cx| {
                        let Some(entity) = screen.upgrade() else { return menu };
                        let state = entity.read(cx);
                        let mut menu = menu.label("Camera");
                        for device in &state.camera.devices {
                            let id = device.id.clone();
                            let screen = screen.clone();
                            menu = menu.item(PopupMenuItem::new(device.name.clone()).on_click(move |_, window, cx| {
                                if let Some(screen) = screen.upgrade() {
                                    screen.update(cx, |this, cx| this.select_camera(id.clone(), window, cx));
                                }
                            }));
                        }
                        let refresh = screen.clone();
                        menu = menu.separator().item(PopupMenuItem::new("Refresh cameras").on_click(move |_, _, cx| {
                            if let Some(screen) = refresh.upgrade() { screen.update(cx, |this, cx| this.refresh_cameras(cx)); }
                        }));
                        let preview = screen.clone();
                        menu.item(PopupMenuItem::new("Toggle local preview").on_click(move |_, window, cx| {
                            if let Some(screen) = preview.upgrade() { screen.update(cx, |this, cx| this.preview_camera(window, cx)); }
                        }))
                    }
                }),
            )
            .child(
                div().id("screen-share-controls").child(control(
                    "call-share",
                    "icons/screen-share.svg",
                    self.share_tooltip(),
                    self.screen_share.is_some(),
                )
                .disabled(!self.share_button_enabled())
                .on_click(cx.listener(|this, _, window, cx| this.toggle_screen_share(window, cx))))
                .context_menu({
                    let screen = cx.entity().downgrade();
                    move |menu, _, _| {
                        let mut menu = menu.label("Screen share audio");
                        let off = screen.clone();
                        menu = menu.item(PopupMenuItem::new("Share without audio").on_click(move |_, window, cx| {
                            if let Some(screen) = off.upgrade() { screen.update(cx, |this, cx| this.toggle_screen_share_with_audio(crate::platform::loopback::AudioShare::Off, window, cx)); }
                        }));
                        match crate::platform::loopback::applications() {
                            Ok(apps) => for app in apps {
                                let screen = screen.clone();
                                menu = menu.item(PopupMenuItem::new(format!("Audio: {}", app.name)).on_click(move |_, window, cx| {
                                    if let Some(screen) = screen.upgrade() { screen.update(cx, |this, cx| this.toggle_screen_share_with_audio(crate::platform::loopback::AudioShare::Process(app.process_id), window, cx)); }
                                }));
                            },
                            Err(error) => { menu = menu.label(error); }
                        }
                        let system = screen.clone();
                        menu.separator().item(PopupMenuItem::new("Share all system audio (including this call)").on_click(move |_, window, cx| {
                            if let Some(screen) = system.upgrade() { screen.update(cx, |this, cx| this.toggle_screen_share_with_audio(crate::platform::loopback::AudioShare::SystemOutput, window, cx)); }
                        }))
                    }
                }),
            )
            .child(
                Button::new("call-hangup")
                    .icon(Icon::default().path("icons/phone-off.svg"))
                    .danger()
                    .large()
                    .tooltip("Disconnect")
                    .on_click(cx.listener(|this, _, _, cx| this.leave_voice(cx))),
            )
    }

    /// Puts a device picker behind a right-click on `button`.
    ///
    /// The menu wraps the button rather than being hung off it: a `Button` with
    /// children lays itself out as a labelled button, which would stretch an icon
    /// one out of shape.
    pub(in crate::screens::home) fn with_device_menu(
        &self,
        id: &'static str,
        label: &'static str,
        button: impl IntoElement,
        devices: DeviceMenu,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let screen = cx.entity().downgrade();

        div().id(id).child(button).context_menu(move |menu, _, cx| {
            let Some(screen) = screen.upgrade() else {
                return menu;
            };
            let chosen = (devices.chosen)(screen.read(cx));

            // Enumerated as the menu opens, so a device plugged in since the
            // app started is in the list.
            let menu = menu.label(label).item(device_item(
                "System Default",
                None,
                chosen.is_none(),
                devices.set,
                &screen,
            ));

            (devices.list)().into_iter().fold(menu, |menu, device| {
                let selected = chosen.as_deref() == Some(device.id.as_str());
                menu.item(device_item(
                    device.name,
                    Some(device.id),
                    selected,
                    devices.set,
                    &screen,
                ))
            })
        })
    }
}

/// Which direction a [`HomeScreen::with_device_menu`] picker is for: what to
/// enumerate, which stored choice is ticked, and what picking an item runs.
pub(in crate::screens::home) struct DeviceMenu {
    list: fn() -> Vec<voice::AudioDevice>,
    chosen: fn(&HomeScreen) -> Option<String>,
    set: fn(&mut HomeScreen, Option<String>, &mut Context<HomeScreen>),
}

impl DeviceMenu {
    /// Microphones: capture the call sends.
    pub(in crate::screens::home) fn input() -> Self {
        Self {
            list: voice::input_devices,
            chosen: HomeScreen::chosen_input_device,
            set: HomeScreen::set_input_device,
        }
    }

    /// Speakers: play what the call receives.
    pub(in crate::screens::home) fn output() -> Self {
        Self {
            list: voice::output_devices,
            chosen: HomeScreen::chosen_output_device,
            set: HomeScreen::set_output_device,
        }
    }
}

/// One device in a picker, ticked when it's the one in use.
fn device_item(
    label: impl Into<SharedString>,
    id: Option<String>,
    selected: bool,
    set: fn(&mut HomeScreen, Option<String>, &mut Context<HomeScreen>),
    screen: &Entity<HomeScreen>,
) -> PopupMenuItem {
    let screen = screen.downgrade();
    let item = PopupMenuItem::new(label.into()).on_click(move |_, _, cx| {
        if let Some(screen) = screen.upgrade() {
            screen.update(cx, |this, cx| set(this, id.clone(), cx));
        }
    });

    if selected {
        item.icon(Icon::new(IconName::Check))
    } else {
        item
    }
}

/// One square toggle on the control bar, pushed in while its thing is on.
fn control(id: &'static str, icon: &'static str, tooltip: &'static str, active: bool) -> Button {
    Button::new(id)
        .icon(Icon::default().path(icon))
        .outline()
        .large()
        .selected(active)
        .tooltip(tooltip)
}
