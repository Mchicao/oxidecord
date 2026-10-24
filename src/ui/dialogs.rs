//! Modal dialogs shared across screens, plus the backdrop drawn behind them.
//!
//! Dialogs are opened through gpui-component's [`Root`](gpui_kit::component::Root),
//! which draws them over the whole window; [`with_dialog_layer`] pushes the
//! content behind them back, so a modal reads as being in front of the app
//! rather than floating somewhere inside it.

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, WindowExt as _, button::ButtonVariant,
    dialog::DialogButtonProps, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::ui::depth;

const BACKDROP_CONTENT_OPACITY: f32 = 0.55;

/// How dark the scrim over the faded content is.
const BACKDROP_SCRIM_ALPHA: f32 = 0.3;

/// Longest a dialog's list of problems gets before the tail is summarised, so a
/// bad multi-select can't grow the dialog past the bottom of the window.
const MAX_LISTED_PROBLEMS: usize = 6;

/// Renders `content` as the backdrop of the modal layer: while a dialog is open
/// the content is faded and covered by a scrim, under the dialog the root draws
/// on top. With no dialog open this is just `content`, with nothing painted over
/// it.
pub fn with_dialog_layer(
    content: AnyElement,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let dialog_open = window.has_active_dialog(cx);

    div()
        .relative()
        .size_full()
        .child(
            div()
                .size_full()
                .when(dialog_open, |this| this.opacity(BACKDROP_CONTENT_OPACITY))
                .child(content),
        )
        .when(dialog_open, |this| {
            // Purely visual: the dialog's own overlay sits above this and
            // occludes the content, so the scrim doesn't need to block input.
            this.child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(black().opacity(BACKDROP_SCRIM_ALPHA)),
            )
        })
}

/// Shows a modal error dialog: a titled danger icon over one line per problem,
/// dismissed with a button or the escape key.
pub fn show_error(
    title: impl Into<SharedString>,
    problems: impl IntoIterator<Item = impl Into<SharedString>>,
    window: &mut Window,
    cx: &mut App,
) {
    let title: SharedString = title.into();
    let problems: Vec<SharedString> = problems.into_iter().map(Into::into).collect();
    let hidden = problems.len().saturating_sub(MAX_LISTED_PROBLEMS);
    let listed: Vec<SharedString> = problems.into_iter().take(MAX_LISTED_PROBLEMS).collect();

    window.open_alert_dialog(cx, move |dialog, _window, cx| {
        let theme = cx.theme();
        // One problem reads as a sentence; several read as a list, so only the
        // list gets bullets.
        let bulleted = listed.len() > 1;

        dialog
            .border_color(depth::ring(cx))
            .width(px(440.))
            .button_props(
                DialogButtonProps::default()
                    .ok_text("Dismiss")
                    .ok_variant(ButtonVariant::Danger),
            )
            .title(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .size_8()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(theme.danger.opacity(0.12))
                            .child(
                                Icon::new(IconName::TriangleAlert)
                                    .size_4()
                                    .text_color(theme.danger),
                            ),
                    )
                    .child(title.clone()),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .children(listed.iter().map(|problem| {
                        h_flex()
                            .gap_2()
                            .items_start()
                            .when(bulleted, |this| {
                                this.child(
                                    div()
                                        .flex_shrink_0()
                                        // Centres the dot on the first line of text.
                                        .mt(px(7.))
                                        .size(px(4.))
                                        .rounded_full()
                                        .bg(theme.muted_foreground),
                                )
                            })
                            .child(div().flex_1().min_w_0().child(problem.clone()))
                    }))
                    .when(hidden > 0, |this| {
                        this.child(
                            div()
                                .italic()
                                .child(format!("…and {hidden} more"))
                                .text_color(theme.muted_foreground.opacity(0.8)),
                        )
                    }),
            )
    });
}

/// Asks before following a masked link. Its text can claim to lead anywhere,
/// so the dialog shows where it really goes, as Discord does.
pub fn confirm_open_link(url: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    let url: SharedString = url.into();
    window.open_alert_dialog(cx, move |dialog, _window, cx| {
        let theme = cx.theme();
        let target = url.clone();
        dialog
            .confirm()
            .border_color(depth::ring(cx))
            .width(px(440.))
            .button_props(
                DialogButtonProps::default()
                    .ok_text("Visit Site")
                    .cancel_text("Go Back"),
            )
            .title("Leaving Oxidecord")
            .child(
                v_flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child("This link is taking you to the following website"),
                    )
                    .child(
                        div()
                            .p_2()
                            .rounded(px(4.))
                            .bg(theme.muted)
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .child(url.clone()),
                    ),
            )
            .on_ok(move |_, _, cx| {
                cx.open_url(&target);
                true
            })
    });
}
