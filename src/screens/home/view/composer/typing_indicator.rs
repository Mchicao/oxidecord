//! The "<name> is typing…" strip above the composer while others are typing.

use std::time::Duration;

use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::*;

use crate::screens::home::HomeScreen;
use crate::screens::home::state::TypingDots;

impl Render for TypingDots {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Discord's pulsing dots: each one swells and brightens in turn, a
        // fifth of the cycle behind the one before it.
        let foreground = cx.theme().foreground;
        h_flex().gap(px(3.)).children((0..3usize).map(|i| {
            // Each dot grows inside a slot sized for its largest, since gpui
            // can't scale a div without relayout: a bare growing dot would
            // shove its neighbours and the text along every pulse.
            div()
                .flex()
                .size(px(7.))
                .items_center()
                .justify_center()
                .child(div().rounded_full().with_animation(
                    ("typing-dot", i),
                    Animation::new(Duration::from_millis(1400)).repeat(),
                    move |dot, delta| {
                        let phase = (delta - i as f32 * 0.2).rem_euclid(1.0);
                        // Up and back down over the first half of its turn,
                        // then rests dim until the cycle comes round.
                        let pulse = if phase < 0.5 {
                            ease_in_out(1.0 - (phase * 4.0 - 1.0).abs())
                        } else {
                            0.0
                        };
                        dot.size(px(5. + pulse * 2.))
                            .bg(foreground.opacity(0.35 + pulse * 0.65))
                    },
                ))
        }))
    }
}

impl HomeScreen {
    /// Floats over the bottom of the conversation rather than taking a row of
    /// its own, so the message bar doesn't grow and shrink as people start and
    /// stop typing.
    pub(super) fn render_typing_indicator(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let names = self.typing_names()?;
        let theme = cx.theme();

        let name = |name: String| {
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(name)
        };
        let mut line = h_flex()
            .gap_1()
            .text_xs()
            .text_color(theme.muted_foreground)
            .whitespace_nowrap();
        line = match names.as_slice() {
            [one] => line.child(name(one.clone())).child("is typing…"),
            [first, second] => line
                .child(name(first.clone()))
                .child("and")
                .child(name(second.clone()))
                .child("are typing…"),
            [first, second, third] => line
                .child(div().flex().child(name(first.clone())).child(","))
                .child(div().flex().child(name(second.clone())).child(","))
                .child("and")
                .child(name(third.clone()))
                .child("are typing…"),
            _ => line.child("Several people are typing…"),
        };

        Some(
            h_flex()
                .absolute()
                .bottom_full()
                .left_2()
                .right_2()
                .h(px(24.))
                .px_3()
                .items_center()
                .overflow_hidden()
                // Masks the messages it sits over.
                .bg(theme.background)
                .gap_2()
                .child(self.typing_dots.clone())
                .child(line)
                .into_any_element(),
        )
    }

    /// The names of whoever else is typing in the open conversation, oldest
    /// first, or `None` when nobody is.
    fn typing_names(&self) -> Option<Vec<String>> {
        let channel_id = self.selected_channel?;
        let typists = self.typists.get(&channel_id)?;
        let names: Vec<String> = typists
            .iter()
            .map(|typist| {
                // A DM's dispatch carries no name, so it's taken from what the
                // typist has said in the conversation.
                typist
                    .name
                    .clone()
                    .or_else(|| {
                        self.messages
                            .iter()
                            .rev()
                            .find(|message| message.author_id == typist.user_id)
                            .map(|message| {
                                self.author_name(message.author_id, &message.author_name)
                                    .to_owned()
                            })
                    })
                    .unwrap_or_else(|| "Someone".to_owned())
            })
            .collect();
        (!names.is_empty()).then_some(names)
    }
}
