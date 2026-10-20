//! The main window's router.

use gpui::*;

use crate::discord;
use crate::screens::home::{HomeScreen, SessionEnded};
use crate::screens::login::LoginScreen;
use crate::ui::{dialogs, update_notice};

enum Route {
    Login(Entity<LoginScreen>),
    /// Holds the subscription that sends the user back to login when the
    /// session ends.
    Home {
        screen: Entity<HomeScreen>,
        _ended: Subscription,
    },
}

/// Top-level view for the main window. Owns the current screen and swaps
/// between login and home so that a successful login can take effect
/// immediately, without restarting the app.
pub struct AppScreen {
    route: Route,
    _update_notice: Subscription,
}

impl AppScreen {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let route = if discord::load_token().is_some() {
            Self::home_route(window, cx)
        } else {
            Self::login_route(window, cx)
        };

        Self {
            route,
            _update_notice: update_notice::watch(window, cx),
        }
    }

    /// Switch to the home screen once a token is available.
    pub fn show_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.route = Self::home_route(window, cx);
        cx.notify();
    }

    fn home_route(window: &mut Window, cx: &mut Context<Self>) -> Route {
        let home = cx.new(|cx| HomeScreen::new(window, cx));
        let subscription = cx.subscribe_in(
            &home,
            window,
            |this, _home, _: &SessionEnded, window, cx| {
                this.route = Self::login_route(window, cx);
                cx.notify();
            },
        );
        Route::Home {
            screen: home,
            _ended: subscription,
        }
    }

    fn login_route(window: &mut Window, cx: &mut Context<Self>) -> Route {
        let app = cx.weak_entity();
        Route::Login(cx.new(|cx| LoginScreen::new(app, window, cx)))
    }
}

impl Render for AppScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let screen = match &self.route {
            Route::Login(view) => view.clone().into_any_element(),
            Route::Home { screen: view, .. } => view.clone().into_any_element(),
        };

        // Every screen sits under the same modal backdrop, so a dialog opened
        // anywhere in the app is backed by its scrim. The root draws dialogs
        // and toasts over that, toasts on top, since one can matter while a
        // dialog is up.
        dialogs::with_dialog_layer(screen, window, cx)
    }
}
