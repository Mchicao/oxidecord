use gpui::*;
use gpui_component::{
    Icon,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
};

use crate::platform::prefs;
use crate::screens::home::HomeScreen;

pub fn build_peer_audio_menu(
    menu: PopupMenu,
    user_id: u64,
    user_name: &str,
    is_self: bool,
    screen: WeakEntity<HomeScreen>,
    cx: &App,
) -> PopupMenu {
    if is_self {
        return menu;
    }

    let screen_opt = screen.upgrade();
    let (current_setting, captured_account) = screen_opt
        .as_ref()
        .map(|s| {
            let s_ref = s.read(cx);
            let setting = s_ref
                .voice_engine
                .as_ref()
                .map(|e| e.audio_settings().peer_setting(user_id))
                .unwrap_or_default();
            let account = s_ref.self_user_id.map(|id| id.get());
            (setting, account)
        })
        .unwrap_or_default();

    let is_muted = current_setting.muted;
    let volume_pct = current_setting.volume_percent();

    let mute_label = if is_muted {
        format!("Unmute {user_name}")
    } else {
        format!("Mute {user_name}")
    };
    let mute_icon = if is_muted {
        "icons/mic.svg"
    } else {
        "icons/mic-off.svg"
    };

    let screen_mute = screen.clone();
    let captured_account_mute = captured_account;

    let mut menu = menu
        .item(
            PopupMenuItem::new(mute_label)
                .icon(Icon::default().path(mute_icon))
                .on_click(move |_, _, cx| {
                    if let Some(screen) = screen_mute.upgrade() {
                        screen.update(cx, |this, cx| {
                            let current_account = this.self_user_id.map(|id| id.get());
                            if current_account != captured_account_mute {
                                return;
                            }
                            let new_muted = !is_muted;
                            if let Some(engine) = &this.voice_engine {
                                engine.audio_settings().set_peer_muted(user_id, new_muted);
                            }
                            if let Some(account_id) = current_account {
                                prefs::update(|prefs| {
                                    let account_map =
                                        prefs.peer_audio.accounts.entry(account_id).or_default();
                                    let entry = account_map.entry(user_id).or_default();
                                    entry.muted = new_muted;
                                });
                            }
                            cx.notify();
                        });
                    }
                }),
        )
        .separator()
        .label(format!("User Volume: {volume_pct}%"));

    for target_pct in [0u32, 50, 100, 150, 200] {
        let is_current = volume_pct == target_pct;
        let screen_vol = screen.clone();
        let captured_account_vol = captured_account;
        menu = menu.item(
            PopupMenuItem::new(format!("{target_pct}%"))
                .checked(is_current)
                .on_click(move |_, _, cx| {
                    if let Some(screen) = screen_vol.upgrade() {
                        screen.update(cx, |this, cx| {
                            let current_account = this.self_user_id.map(|id| id.get());
                            if current_account != captured_account_vol {
                                return;
                            }
                            let gain = target_pct as f32 / 100.0;
                            if let Some(engine) = &this.voice_engine {
                                let _ = engine.audio_settings().set_peer_volume(user_id, gain);
                            }
                            if let Some(account_id) = current_account {
                                prefs::update(|prefs| {
                                    let account_map =
                                        prefs.peer_audio.accounts.entry(account_id).or_default();
                                    let entry = account_map.entry(user_id).or_default();
                                    entry.volume = gain;
                                });
                            }
                            cx.notify();
                        });
                    }
                }),
        );
    }

    if volume_pct != 100 {
        let screen_reset = screen.clone();
        let captured_account_reset = captured_account;
        menu = menu.separator().item(
            PopupMenuItem::new("Reset Volume to 100%")
                .icon(Icon::default().path("icons/undo-2.svg"))
                .on_click(move |_, _, cx| {
                    if let Some(screen) = screen_reset.upgrade() {
                        screen.update(cx, |this, cx| {
                            let current_account = this.self_user_id.map(|id| id.get());
                            if current_account != captured_account_reset {
                                return;
                            }
                            if let Some(engine) = &this.voice_engine {
                                let _ = engine.audio_settings().set_peer_volume(user_id, 1.0);
                            }
                            if let Some(account_id) = current_account {
                                prefs::update(|prefs| {
                                    let account_map =
                                        prefs.peer_audio.accounts.entry(account_id).or_default();
                                    let entry = account_map.entry(user_id).or_default();
                                    entry.volume = 1.0;
                                });
                            }
                            cx.notify();
                        });
                    }
                }),
        );
    }

    menu
}

pub fn attach_peer_audio_context_menu(
    element: Stateful<Div>,
    user_id: u64,
    user_name: String,
    is_self: bool,
    cx: &Context<HomeScreen>,
) -> impl IntoElement + use<> {
    let screen = cx.entity().downgrade();
    element.context_menu(move |popup, _, cx| {
        build_peer_audio_menu(popup, user_id, &user_name, is_self, screen.clone(), cx)
    })
}
